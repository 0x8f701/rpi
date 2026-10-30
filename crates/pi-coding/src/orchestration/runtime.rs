use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use parking_lot::{Mutex, RwLock};
use pi_agent::{
    AgentTool, BoxFuture, ShouldStopAfterTurnContext, ShouldStopAfterTurnFn, ThinkingLevel,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{Notify, Semaphore, broadcast};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

use crate::fs::{
    CapabilityAccess, CapabilityEntry, CapabilityFile, CapabilityFs, CapabilityRootLease,
    CapabilityRootPurpose, EntryKind, FileMode, FsError, Replacement, RootCreation,
    is_link_blocked,
};
use crate::{Session, Skill};

use super::{
    AgentCatalog, AgentDefinition, CapabilityCeiling, JobClock, JobManager, JobRetention,
    JobSnapshot, JobSoftBudget, JobStatus, PreparedJobRecords, TaskSpawn,
};

pub const DEFAULT_MAILBOX_CAPACITY: usize = 100;
pub const DEFAULT_MAX_CONCURRENCY: usize = 4;
pub const DEFAULT_MAX_RECURSION_DEPTH: usize = 2;
/// Cap applied to explicitly declared agent tool lists. Agents whose
/// effective `tools` are `None` inherit the full parent/default child set
/// and are exempt from this cap.
pub const DEFAULT_MAX_TOOLS_PER_AGENT: usize = 16;
pub const DEFAULT_IDLE_TTL_SECS: u64 = 300;
pub const DEFAULT_MAX_RETAINED_JOBS: usize = 256;
pub const DEFAULT_RETAINED_JOB_TTL_SECS: u64 = 24 * 60 * 60;
/// Bound on the delivered-message log the workflow page's Recent IRC reads.
pub(crate) const DELIVERED_MESSAGE_LOG_CAP: usize = 200;
/// Ingress/persistence bound on a mailbox message body.
pub const MAX_MESSAGE_BODY_BYTES: usize = 64 * 1024;
/// Stable custom-message type for orchestration mailbox deliveries.
pub const ORCHESTRATION_MESSAGE_TYPE: &str = "orchestration_message";
/// Default number of transcript lines rendered by `hub read_history`.
pub const DEFAULT_HISTORY_LINES: usize = 50;
/// Hard maximum for `hub read_history` lines (clamped, never exceeded).
pub const MAX_HISTORY_LINES: usize = 200;
/// Hard byte cap for a rendered `hub read_history` transcript.
pub const MAX_HISTORY_BYTES: usize = 320 * 1024;
const MAX_AUTOLOAD_SKILL_BYTES: u64 = 256 * 1024;
const MAX_AUTOLOAD_PROMPT_BYTES: usize = 1024 * 1024;
const MAX_PERSONALITY_BYTES: usize = 64 * 1024;
const PERSONA_CONTINUITY_MAX_MESSAGES: usize = 200;
const PERSONA_CONTINUITY_MAX_BYTES: usize = 32 * 1024;
const MAX_SIBLING_ROSTER_ENTRIES: usize = 64;
const MAX_SIBLING_ROSTER_BYTES: usize = MAX_AUTOLOAD_PROMPT_BYTES / 64;
const MAX_ROSTER_AGENT_CHARS: usize = 80;
const CHILD_ABORT_GRACE: Duration = Duration::from_secs(2);
const MAX_HISTORY_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;
pub type AgentSelectorFn = Arc<dyn Fn(&str, &[AgentDefinition]) -> Option<String> + Send + Sync>;
pub type ParentModelProvider = Arc<dyn Fn() -> pi_ai::Model + Send + Sync>;
pub type ChildSessionFactory =
    Arc<dyn Fn(ChildSessionRequest) -> BoxFuture<Result<Session>> + Send + Sync>;

pub const CODE_REVIEW_PARENT_AGENT_ID: &str = "Review";
/// Short text preview of a queued pending message, for surfaced loss reports.
fn queued_message_preview(message: &pi_ai::Message) -> String {
    match message {
        pi_ai::Message::User(user) => user
            .content
            .iter()
            .filter_map(|block| match block {
                pi_ai::ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" "),
        pi_ai::Message::Custom(custom) => match &custom.content {
            pi_ai::CustomMessageContent::Text(text) => text.clone(),
            pi_ai::CustomMessageContent::Blocks(_) => "(blocks)".to_owned(),
        },
        _ => format!("{:?}", message.role()),
    }
    .chars()
    .take(120)
    .collect()
}

#[derive(Clone)]
pub struct ChildSession {
    session: Session,
}

impl ChildSession {
    #[must_use]
    pub fn new(session: Session) -> Self {
        Self { session }
    }

    async fn steer(&self, message: &MailboxMessage) -> Result<()> {
        self.session.steer(mailbox_message_as_custom(message)).await
    }

    /// Drain steering messages that were steered into the session's queue but
    /// never processed by the run. The active delivery bridge acknowledges a
    /// message the moment it is steered; if the run completes before the agent
    /// loop drains the queue (the final-turn race), the message would
    /// otherwise be lost with the dropped session. The caller restores the
    /// mailbox messages to the mailbox so the next run/revival delivers them
    /// exactly once. Anything that cannot be mapped back to a mailbox record
    /// (foreign custom types, undecodable payloads, user-text steering) is
    /// returned as a description so the caller can surface it — a teardown
    /// never silently discards acknowledged steering.
    async fn drain_unprocessed_orchestration_steering(&self) -> (Vec<MailboxMessage>, Vec<String>) {
        let (steering, _) = self.session.drain_queued_messages().await;
        let mut mailbox = Vec::new();
        let mut unrestorable = Vec::new();
        for message in steering {
            match message {
                pi_ai::Message::Custom(custom)
                    if custom.custom_type == ORCHESTRATION_MESSAGE_TYPE =>
                {
                    match mailbox_message_from_custom(&custom) {
                        Some(message) => mailbox.push(message),
                        None => unrestorable.push(format!(
                            "orchestration message {} lost at child teardown (undecodable details)",
                            custom.details.as_ref().and_then(|details| details.get("id")).and_then(serde_json::Value::as_str).unwrap_or("?")
                        )),
                    }
                }
                other => unrestorable.push(format!(
                    "steering message lost at child teardown: {}",
                    queued_message_preview(&other)
                )),
            }
        }
        (mailbox, unrestorable)
    }

    async fn run(&self, assignment: &str) -> Result<crate::RunResult> {
        self.session.run(assignment, Vec::new()).await
    }

    async fn continue_run(&self) -> Result<crate::RunResult> {
        self.session.continue_run().await
    }

    fn can_continue_without_input(&self) -> bool {
        self.session
            .history()
            .last()
            .is_some_and(|message| !matches!(message, pi_ai::Message::Assistant(_)))
    }

    /// Names of the tools this child actually possesses: the session's active
    /// tool set — role-filtered base tools plus orchestration plumbing and the
    /// child-only `yield` tool — as built by the child factory at spawn. This
    /// is the structural reference for last-failure classification: a tool
    /// call whose name is absent from this set was denied by the child's own
    /// restricted tool set (e.g. a read-only child calling `write`), never
    /// executed, and is not an execution failure.
    fn available_tool_names(&self) -> BTreeSet<String> {
        self.session.get_active_tool_names().into_iter().collect()
    }

    async fn abort(&self) {
        self.session.abort().await;
    }

    fn last_assistant_text(&self) -> String {
        self.session.last_assistant_text()
    }

    fn history(&self) -> Vec<pi_ai::Message> {
        self.session.history()
    }

    fn set_should_stop_after_turn(&self, hook: Option<ShouldStopAfterTurnFn>) {
        self.session.set_should_stop_after_turn(hook);
    }
}

impl From<Session> for ChildSession {
    fn from(session: Session) -> Self {
        Self::new(session)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestrationSkill {
    pub name: String,
    pub description: String,
    pub file_path: PathBuf,
    pub base_dir: PathBuf,
    pub globs: Vec<String>,
    pub always_apply: bool,
    pub hidden: bool,
    pub disable_model_invocation: bool,
    pub source: crate::SkillSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_project_boundary: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_root: Option<PathBuf>,
    pub trusted: bool,
}

impl From<&Skill> for OrchestrationSkill {
    fn from(skill: &Skill) -> Self {
        Self {
            name: skill.name.clone(),
            description: skill.description.clone(),
            file_path: PathBuf::from(&skill.file_path),
            base_dir: PathBuf::from(&skill.base_dir),
            globs: skill.globs.clone(),
            always_apply: skill.always_apply,
            hidden: skill.hidden,
            disable_model_invocation: skill.disable_model_invocation,
            source: skill.source,
            managed_project_boundary: skill.managed_project_boundary.clone(),
            managed_root: skill.managed_root.clone(),
            trusted: skill.trusted,
        }
    }
}

#[derive(Clone)]
pub struct OrchestrationConfig {
    pub catalog: AgentCatalog,
    pub skills: Vec<OrchestrationSkill>,
    pub artifact_dir: PathBuf,
    pub max_concurrency: usize,
    pub max_recursion_depth: usize,
    pub mailbox_capacity: usize,
    pub max_tools_per_agent: usize,
    pub main_agent_id: String,
    pub default_agent: String,
    pub default_agent_selector: Option<AgentSelectorFn>,
    /// Persisted runtime preference used for unnamed child spawns. Invalid or
    /// disabled values remain advisory and fall back through normal selection.
    pub preferred_agent: Option<String>,
    selector_settings: Option<crate::SelectorSettings>,
    pub agent_settings: BTreeMap<String, crate::AgentRuntimeSettings>,
    pub parent_model: pi_ai::Model,
    parent_model_provider: Option<ParentModelProvider>,
    pub idle_ttl: Option<Duration>,
    pub max_retained_jobs: usize,
    pub retained_job_ttl: Duration,
    /// Soft budget for child jobs. Defaults to unlimited (all knobs `None`),
    /// preserving run-to-completion behavior; opt-in knobs make a child yield
    /// with a partial result and the `soft_budget_exhausted` marker instead
    /// of running to completion.
    pub soft_budget: JobSoftBudget,
    job_clock: JobClock,
}

struct OrchestrationStorage {
    artifacts: CapabilityRootLease,
    persona_roots: BTreeMap<String, CapabilityRootLease>,
    skill_files: BTreeMap<String, Arc<CapabilityFile>>,
}

#[derive(Clone)]
enum RetainedHistoryFile {
    Artifact(Arc<CapabilityFile>),
    Session(Arc<CapabilityFile>),
    Persona(Arc<CapabilityFile>),
}

impl RetainedHistoryFile {
    fn display_path(&self) -> PathBuf {
        match self {
            Self::Artifact(file) | Self::Session(file) | Self::Persona(file) => file.display_path(),
        }
    }

    fn read_bounded(&self, max_bytes: u64) -> Result<Vec<u8>> {
        match self {
            Self::Artifact(file) | Self::Session(file) | Self::Persona(file) => {
                file.read_bounded(max_bytes).map_err(Into::into)
            }
        }
    }

    fn session_file(&self) -> Result<std::fs::File> {
        match self {
            Self::Session(file) | Self::Persona(file) => Ok(file.try_clone_std()?),
            Self::Artifact(_) => bail!("history snapshot is not a session transcript"),
        }
    }

    fn into_file(self) -> Arc<CapabilityFile> {
        match self {
            Self::Artifact(file) | Self::Session(file) | Self::Persona(file) => file,
        }
    }
}

impl std::fmt::Debug for OrchestrationConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OrchestrationConfig")
            .field("catalog", &self.catalog)
            .field("skills", &self.skills)
            .field("artifact_dir", &self.artifact_dir)
            .field("max_concurrency", &self.max_concurrency)
            .field("max_recursion_depth", &self.max_recursion_depth)
            .field("mailbox_capacity", &self.mailbox_capacity)
            .field("max_tools_per_agent", &self.max_tools_per_agent)
            .field("main_agent_id", &self.main_agent_id)
            .field("default_agent", &self.default_agent)
            .field(
                "default_agent_selector",
                &self.default_agent_selector.as_ref().map(|_| "configured"),
            )
            .field("preferred_agent", &self.preferred_agent)
            .field("idle_ttl", &self.idle_ttl)
            .field("max_retained_jobs", &self.max_retained_jobs)
            .field("retained_job_ttl", &self.retained_job_ttl)
            .field("soft_budget", &self.soft_budget)
            .field("agent_settings", &self.agent_settings)
            .field(
                "parent_model_provider",
                &self.parent_model_provider.as_ref().map(|_| "configured"),
            )
            .finish()
    }
}

impl OrchestrationRuntime {
    pub fn child_factory_from_session(parent: &Session) -> ChildSessionFactory {
        Self::child_factory_from_session_and_uri(parent, None)
    }

    pub fn child_factory_from_session_and_uri(
        parent: &Session,
        uri_resolver: Option<crate::InternalUriResolverFn>,
    ) -> ChildSessionFactory {
        let parent = parent.clone();
        Arc::new(move |request| {
            Self::child_factory_from_snapshot_and_uri(
                parent.child_session_options_snapshot(),
                uri_resolver.clone(),
            )(request)
        })
    }

    pub fn child_factory_from_snapshot(
        snapshot: crate::ChildSessionOptionsSnapshot,
    ) -> ChildSessionFactory {
        Self::child_factory_from_snapshot_and_uri(snapshot, None)
    }

    pub fn child_factory_from_snapshot_and_uri(
        snapshot: crate::ChildSessionOptionsSnapshot,
        uri_resolver: Option<crate::InternalUriResolverFn>,
    ) -> ChildSessionFactory {
        Arc::new(move |request| {
            let snapshot = snapshot.clone();
            let uri_resolver = uri_resolver.clone();
            Box::pin(async move {
                let mut stream_options = snapshot.stream_options;
                stream_options.stream.session_id = None;
                let cwd = snapshot.workspace.cwd().to_string_lossy();
                let generation_capture = snapshot.generation_capture.capture_fn();
                let memory = snapshot.memory.clone();
                let persona_root = request.definition.persona_root();
                if request.definition.is_persona() {
                    let missing = request
                        .definition
                        .missing_knowledge_roots(&snapshot.workspace);
                    if !missing.is_empty() {
                        return Err(crate::knowledge_root_membership_error(
                            &request.definition.name,
                            &missing,
                        ));
                    }
                }
                let requested_memory =
                    request
                        .requested_tool_names
                        .as_deref()
                        .is_some_and(|names| {
                            names.iter().any(|name| {
                                matches!(name.as_str(), "memory" | "recall" | "retain" | "reflect")
                            })
                        });
                let shared_skills = request.shared_skills.clone();
                let skill_provider: crate::SkillSnapshotFn =
                    Arc::new(move || (*shared_skills).clone());
                let requested_learn_skill = request.definition.is_persona()
                    && request
                        .requested_tool_names
                        .as_deref()
                        .is_some_and(|names| names.iter().any(|name| name == "learn_skill"));
                let mut base_tools = match request.requested_tool_names.as_deref() {
                    Some(names) => {
                        let requested = names
                            .iter()
                            .filter(|name| {
                                !crate::is_child_plumbing_tool(name)
                                    && name.as_str() != "learn_skill"
                                    && crate::is_known_child_tool_for_definition(name, &request.definition)
                                    && !matches!(name.as_str(), "memory" | "recall" | "retain" | "reflect")
                            })
                            .cloned()
                            .collect::<std::collections::BTreeSet<_>>();
                        let mut tools = crate::tools::create_coding_tools_for_workspace_with_generation_and_persona_skills(
                            snapshot.workspace.clone(),
                            None,
                            Some(skill_provider.clone()),
                            None,
                            uri_resolver.clone(),
                            Some(Arc::new(|| Some(crate::MemoryConfig {
                                backend: crate::MemoryBackend::Off,
                                ..crate::MemoryConfig::default()
                            }))),
                            None,
                            generation_capture.clone(),
                            Some(request.persona_skills.clone()),
                        )?
                        .into_iter()
                        .filter(|tool| requested.contains(&tool.name))
                        .collect::<Vec<_>>();
                        if requested_memory {
                            let config = memory.as_ref().and_then(|resolver| resolver());
                            tools.extend(match persona_root.as_deref() {
                                Some(root) => crate::memory::memory_tools_for_persona(
                                    &cwd,
                                    root,
                                    None,
                                    config,
                                ),
                                None => crate::memory::memory_tools_for_with_generation(
                                    &cwd,
                                    None,
                                    config,
                                    Some(generation_capture.clone()),
                                ),
                            });
                        }
                        tools
                    }
                    None => crate::tools::create_coding_tools_for_workspace_with_generation_and_persona_skills(
                        snapshot.workspace.clone(),
                        None,
                        Some(skill_provider),
                        None,
                        uri_resolver.clone(),
                        memory.clone(),
                        None,
                        generation_capture.clone(),
                        Some(request.persona_skills.clone()),
                    )?,
                };
                if requested_learn_skill {
                    let store = request.persona_skill_store.as_ref().ok_or_else(|| {
                        anyhow!(
                            "persona {:?} has no retained skill authority",
                            request.definition.name
                        )
                    })?;
                    base_tools.push(store.learn_skill_tool());
                }
                let disallowed = request
                    .definition
                    .disallowed_tools
                    .iter()
                    .map(String::as_str)
                    .collect::<std::collections::BTreeSet<_>>();
                let allowed_capabilities = request
                    .definition
                    .capability_ceiling
                    .map(CapabilityCeiling::allowed_capabilities);
                let mut tools = base_tools
                    .into_iter()
                    .filter(|tool| {
                        !disallowed.contains(tool.name.as_str())
                            && allowed_capabilities
                                .as_ref()
                                .is_none_or(|allowed| allowed.contains(&tool.capability))
                    })
                    .collect::<Vec<_>>();
                // `tools: None` inherits the parent's full factory tool set,
                // which may exceed the cap; only explicitly declared lists
                // are capped.
                if request.requested_tool_names.is_some()
                    && tools.len() > request.max_tools_per_agent
                {
                    let names = tools
                        .iter()
                        .map(|tool| tool.name.clone())
                        .collect::<Vec<_>>();
                    return Err(crate::child_tool_cap_error(
                        &request.definition.name,
                        request.max_tools_per_agent,
                        &names,
                    ));
                }
                tools.extend(request.orchestration_tools.into_iter().filter(|tool| {
                    tool.name != "yield" && !disallowed.contains(tool.name.as_str())
                }));
                tools.push(super::tools::yield_tool(request.yield_state.clone()));
                let api_key = if request.model.provider == snapshot.model.provider {
                    snapshot.api_key
                } else if let Some(resolver) = &snapshot.auth_resolver {
                    resolver(request.model.clone())
                        .await
                        .with_context(|| {
                            format!(
                                "resolving auth for child model {}/{}",
                                request.model.provider, request.model.id
                            )
                        })?
                        .api_key
                } else {
                    bail!(
                        "child model {}/{} uses provider {:?}, but the parent session provider is {:?} and no auth resolver is configured",
                        request.model.provider,
                        request.model.id,
                        request.model.provider,
                        snapshot.model.provider,
                    );
                };
                let context_files = snapshot
                    .context_files_resolver
                    .as_ref()
                    .map_or_else(Vec::new, |resolver| resolver());
                let context_section = crate::system_prompt::build_context_section(&context_files);
                let primary_model = request.model.clone();
                let child = Session::new_with_inherited_generation(
                    crate::SessionOptions {
                        model: primary_model.clone(),
                        cwd: snapshot.workspace.cwd().to_path_buf(),
                        system_prompt: format!("{}{context_section}", request.system_prompt),
                        thinking_level: request.thinking_level.unwrap_or(snapshot.thinking_level),
                        api_key,
                        compaction: None,
                        stream_options,
                        tools: Some(tools),
                        before_tool_call: None,
                        after_tool_call: None,
                        stream_fn: Some(snapshot.stream_fn),
                        auth_resolver: snapshot.auth_resolver,
                        auth_manager: snapshot.auth_manager,
                    },
                    Vec::new(),
                    crate::ToolSelection::default(),
                    crate::ResourceDiscovery::Disabled,
                    snapshot.workspace.clone(),
                    uri_resolver.clone(),
                    generation_capture,
                    snapshot.generation_capture.authority_required(),
                )?;
                // Nested child always uses its own definition name as the retry
                // role — never the parent snapshot's retry_role (which is the
                // parent's role, e.g. researcher, and would mis-key the child's
                // fallback chain when the child is a different role like reviewer).
                let retry_role = request.definition.name.clone();
                let mut retry_settings = snapshot.retry_settings;
                // Build the child fallback chain from the pre-resolved ordered
                // sequence carried on the request, never reparsing the
                // definition's source patterns. The entries after the active
                // model form the ordered fallback list; configured chains for
                // this role are appended (deduped) so operator-configured
                // fallbacks still apply after the resolved sequence.
                let definition_chain: Vec<String> = request
                    .resolved_model_sequence
                    .iter()
                    .skip(request.resolved_model_selected_index.saturating_add(1))
                    .map(|candidate| match candidate.thinking_level {
                        None => candidate.selector.clone(),
                        Some(level) => format!(
                            "{}:{}",
                            candidate.selector,
                            match level {
                                ThinkingLevel::Off => "off",
                                ThinkingLevel::Minimal => "minimal",
                                ThinkingLevel::Low => "low",
                                ThinkingLevel::Medium => "medium",
                                ThinkingLevel::High => "high",
                                ThinkingLevel::Xhigh => "xhigh",
                                ThinkingLevel::Max => "max",
                            }
                        ),
                    })
                    .collect();
                if !definition_chain.is_empty() {
                    let configured = retry_settings
                        .fallback_chains
                        .remove(&retry_role)
                        .unwrap_or_default();
                    let mut merged = definition_chain;
                    for selector in configured {
                        if !merged.contains(&selector) {
                            merged.push(selector);
                        }
                    }
                    retry_settings
                        .fallback_chains
                        .insert(retry_role.clone(), merged);
                }
                child.set_retry_settings(retry_settings);
                child.set_retry_role(Some(retry_role));
                Ok(child)
            })
        })
    }
}

impl OrchestrationConfig {
    #[must_use]
    pub fn new(catalog: AgentCatalog, artifact_dir: impl Into<PathBuf>) -> Self {
        Self {
            catalog,
            skills: Vec::new(),
            artifact_dir: artifact_dir.into(),
            max_tools_per_agent: DEFAULT_MAX_TOOLS_PER_AGENT,
            max_concurrency: DEFAULT_MAX_CONCURRENCY,
            max_recursion_depth: DEFAULT_MAX_RECURSION_DEPTH,
            mailbox_capacity: DEFAULT_MAILBOX_CAPACITY,
            main_agent_id: "Main".to_owned(),
            default_agent: "task".to_owned(),
            default_agent_selector: None,
            preferred_agent: None,
            selector_settings: None,
            agent_settings: BTreeMap::new(),
            parent_model: pi_ai::Model::default(),
            parent_model_provider: None,
            idle_ttl: Some(Duration::from_secs(DEFAULT_IDLE_TTL_SECS)),
            max_retained_jobs: DEFAULT_MAX_RETAINED_JOBS,
            retained_job_ttl: Duration::from_secs(DEFAULT_RETAINED_JOB_TTL_SECS),
            soft_budget: JobSoftBudget::default(),
            job_clock: Arc::new(now_millis),
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.catalog.is_empty() {
            bail!("orchestration requires at least one agent definition");
        }
        if self.max_concurrency == 0 {
            bail!("orchestration max_concurrency must be greater than zero");
        }
        if self.mailbox_capacity == 0 {
            bail!("orchestration mailbox_capacity must be greater than zero");
        }
        if self.max_tools_per_agent == 0 || self.max_tools_per_agent > 64 {
            bail!("orchestration max_tools_per_agent must be between 1 and 64");
        }
        if let Some(ttl) = self.idle_ttl
            && ttl.is_zero()
        {
            bail!("orchestration idle_ttl must be greater than zero when set");
        }
        JobRetention {
            max_settled: self.max_retained_jobs,
            ttl: self.retained_job_ttl,
        }
        .validate()?;
        validate_agent_id(&self.main_agent_id)?;
        let default = self.catalog.get(&self.default_agent).ok_or_else(|| {
            anyhow!(
                "default orchestration agent {:?} was not discovered",
                self.default_agent
            )
        })?;
        if !default.trusted {
            bail!(
                "default orchestration agent {:?} is not trusted",
                self.default_agent
            );
        }
        if let Some(agent) = self.catalog.agents().iter().find(|agent| !agent.trusted) {
            bail!(
                "orchestration catalog contains untrusted agent {:?}",
                agent.name
            );
        }
        let mut skill_names = std::collections::BTreeMap::<&str, &Path>::new();
        for skill in &self.skills {
            if !skill.trusted {
                bail!("orchestration skill {:?} is not trusted", skill.name);
            }
            if let Some(first_path) = skill_names.insert(&skill.name, &skill.file_path) {
                bail!(
                    "orchestration skill name {:?} is ambiguous: {} and {}; rename one skill so every discovered skill has a unique name",
                    skill.name,
                    first_path.display(),
                    skill.file_path.display(),
                );
            }
        }
        for agent in self.catalog.agents() {
            for skill in &agent.autoload_skills {
                if !skill_names.contains_key(skill.as_str()) {
                    bail!(
                        "agent {:?} autoloads undiscovered skill {:?}",
                        agent.name,
                        skill
                    );
                }
            }
        }
        Ok(())
    }
    #[must_use]
    pub fn with_selector_settings(mut self, settings: crate::SelectorSettings) -> Self {
        self.selector_settings = Some(settings.clone());
        self.default_agent_selector = Some(Arc::new(
            move |request, agents| match crate::selector::delegation_agent_mention(request, agents)
            {
                crate::selector::ExactAgentMention::Unique(name) => Some(name),
                crate::selector::ExactAgentMention::Ambiguous(_) => None,
                crate::selector::ExactAgentMention::None => {
                    let ranked = crate::rank_agents(request, agents, &settings);
                    crate::select_default_agent(&ranked, &settings)
                }
            },
        ));
        self
    }

    #[must_use]
    pub fn with_agent_settings(
        mut self,
        agent_settings: BTreeMap<String, crate::AgentRuntimeSettings>,
    ) -> Self {
        self.agent_settings = agent_settings;
        self
    }

    #[must_use]
    pub fn with_parent_model(mut self, model: pi_ai::Model) -> Self {
        self.parent_model = model;
        self
    }

    #[must_use]
    pub fn with_parent_model_provider(mut self, provider: ParentModelProvider) -> Self {
        self.parent_model_provider = Some(provider);
        self
    }

    #[must_use]
    pub fn with_job_retention(mut self, max_retained_jobs: usize, ttl: Duration) -> Self {
        self.max_retained_jobs = max_retained_jobs;
        self.retained_job_ttl = ttl;
        self
    }

    #[cfg(test)]
    fn with_job_clock(mut self, clock: JobClock) -> Self {
        self.job_clock = clock;
        self
    }

    fn current_parent_model(&self) -> pi_ai::Model {
        self.parent_model_provider
            .as_ref()
            .map_or_else(|| self.parent_model.clone(), |provider| provider())
    }

    fn runtime_equivalent(&self, other: &Self) -> bool {
        let selectors_equal = self.selector_settings == other.selector_settings
            && match (&self.default_agent_selector, &other.default_agent_selector) {
                (None, None) => true,
                (Some(left), Some(right)) if self.selector_settings.is_some() => true,
                (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                _ => false,
            };
        let parent_models_equal = match (
            self.parent_model_provider.is_some(),
            other.parent_model_provider.is_some(),
        ) {
            (true, true) => true,
            (false, false) => self.parent_model == other.parent_model,
            _ => false,
        };
        self.catalog == other.catalog
            && self.skills == other.skills
            && self.artifact_dir == other.artifact_dir
            && self.max_concurrency == other.max_concurrency
            && self.max_recursion_depth == other.max_recursion_depth
            && self.mailbox_capacity == other.mailbox_capacity
            && self.max_tools_per_agent == other.max_tools_per_agent
            && self.main_agent_id == other.main_agent_id
            && self.default_agent == other.default_agent
            && selectors_equal
            && self.preferred_agent == other.preferred_agent
            && self.agent_settings == other.agent_settings
            && parent_models_equal
            && self.idle_ttl == other.idle_ttl
            && self.max_retained_jobs == other.max_retained_jobs
            && self.retained_job_ttl == other.retained_job_ttl
            && self.soft_budget == other.soft_budget
    }
}

#[derive(Clone)]
pub struct ChildSessionRequest {
    pub child_id: String,
    pub parent_id: String,
    pub max_tools_per_agent: usize,
    pub depth: usize,
    pub definition: AgentDefinition,
    pub assignment: String,
    pub system_prompt: String,
    pub requested_tool_names: Option<Vec<String>>,
    pub orchestration_tools: Vec<AgentTool>,
    /// Retained persona authority used only to construct the explicitly
    /// declared `learn_skill` tool. Never serialized.
    pub persona_skill_store: Option<crate::PersonaSkillStore>,
    /// Immutable persona-local skill snapshot shared by this child's prompt
    /// and `skill://` read tool for the lifetime of the run.
    pub persona_skills: Arc<Vec<crate::PersonaSkill>>,
    /// Shared skill catalog snapshot used by this child's `skill://` reads.
    /// Persona-local names shadow these entries without mutating the catalog.
    pub shared_skills: Arc<Vec<Skill>>,
    pub thinking_level: Option<ThinkingLevel>,
    pub model: pi_ai::Model,
    /// Per-item JSON Schema contract for the delivered `yield` payload (OMP
    /// `outputSchema`). The run loop validates the settled payload against it.
    pub output_schema: Option<Value>,
    /// Validation mode for `output_schema`: `"permissive"` or `"strict"`.
    pub schema_mode: Option<String>,
    /// Delivery state for the child-only `yield` tool. The child factory wires
    /// the tool to this state and the run loop reads the payload when the run
    /// settles. Fresh per spawn/revival; it is not persisted (a job that
    /// already delivered via `yield` never needs revival, and a revived run
    /// gets a clean state for its remaining turns).
    pub yield_state: Arc<YieldState>,
    /// Pre-resolved ordered fallback sequence for this child's model. The
    /// child factory builds the retry fallback chain directly from this
    /// sequence (the entries after the active model) instead of reparsing the
    /// definition's source patterns against a different catalog. Empty when no
    /// definition model list resolved or the active model came from a settings
    /// override / parent fallback with a single-entry sequence.
    pub resolved_model_sequence: Vec<crate::ResolvedAgentModelCandidate>,
    /// 0-based position of [`ChildSessionRequest::model`] within
    /// [`ChildSessionRequest::resolved_model_sequence`]. The fallback chain is
    /// the entries after this index.
    pub resolved_model_selected_index: usize,
}

impl std::fmt::Debug for ChildSessionRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ChildSessionRequest")
            .field("child_id", &self.child_id)
            .field("parent_id", &self.parent_id)
            .field("depth", &self.depth)
            .field("definition", &self.definition)
            .field("assignment", &self.assignment)
            .field("requested_tool_names", &self.requested_tool_names)
            .field("max_tools_per_agent", &self.max_tools_per_agent)
            .field("persona_skills", &self.persona_skills.len())
            .field("shared_skills", &self.shared_skills.len())
            .field("thinking_level", &self.thinking_level)
            .field(
                "model",
                &format!("{}/{}", self.model.provider, self.model.id),
            )
            .field("output_schema", &self.output_schema)
            .field("schema_mode", &self.schema_mode)
            .field("yield_state", &self.yield_state.was_called())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
    Queued,
    Running,
    Idle,
    Parked,
    Aborted,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSnapshot {
    pub id: String,
    pub display_name: String,
    #[serde(default)]
    pub agent: String,
    pub parent_id: Option<String>,
    pub status: AgentStatus,
    pub created_at: u64,
    pub last_activity: u64,
    pub unread: usize,
    pub artifact_ref: Option<String>,
    pub history_ref: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OrchestrationEvent {
    JobUpdated {
        group_id: String,
        job: JobSnapshot,
    },
    AgentUpdated {
        group_id: String,
        agent: AgentSnapshot,
    },
    /// Live projection of a successfully delivered message.
    /// Unclaimed root-agent messages remain durable until the Application consumes them.
    MessageDelivered {
        group_id: String,
        message: MailboxMessage,
        /// True when an active `hub wait` claimed the message at delivery
        /// time: the waiter returns the body as its own tool result, so the
        /// application skips steering it into the parent session to avoid
        /// duplicating it in the model context.
        #[serde(default)]
        waiter_claimed: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MailboxMessage {
    pub id: String,
    pub from: String,
    pub to: String,
    pub body: String,
    pub timestamp: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeliveryOutcome {
    Queued,
    Woken,
    Revived,
    Processed,
    Failed,
}

/// What `deliver_committed` actually did with a committed mailbox message, so
/// `send` can report a truthful receipt instead of a fake `Woken`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommittedDelivery {
    /// Handed to an active steering bridge; the run processes it.
    Delivered,
    /// Claimed by an active `hub wait`, which drains the mailbox itself.
    WaiterClaimed,
    /// No active bridge and no waiter; the message stays in the mailbox.
    Retained,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeliveryReceipt {
    pub to: String,
    pub outcome: DeliveryOutcome,
    /// Original `to` when it differed from the canonical agent id (e.g. job UUID).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested: Option<String>,
    /// Mailbox id of the outbound message, used as the correlation id for
    /// `hub ask` replies (`replyTo`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskItem {
    pub index: usize,
    pub id: String,
    pub agent: String,
    pub assignment: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub todo_task_id: Option<String>,
    /// Shared background context rendered into the child's system prompt as a
    /// `CONTEXT` section. Optional for one child and required for a batch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// Per-item JSON Schema contract for the child's delivered `yield`
    /// payload. When present, the run loop parses the payload as JSON and
    /// validates it against this schema, reporting the outcome as
    /// [`TaskResult::structured_output`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    /// Validation mode for [`TaskItem::output_schema`]: `"permissive"`
    /// (default, reports the outcome only) or `"strict"` (surfaces a
    /// validation failure as a job error).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_mode: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowRuntimeScope {
    pub workflow_id: String,
    pub generation: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowJobSnapshot {
    pub workflow_id: String,
    pub generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub todo_task_id: Option<String>,
    pub job: JobSnapshot,
}

#[derive(Clone, Debug)]
pub struct OrchestrationConcurrencyGate {
    semaphore: Arc<Semaphore>,
    limit: usize,
}

impl OrchestrationConcurrencyGate {
    pub fn new(limit: usize) -> Result<Self> {
        if limit == 0 {
            bail!("workflow orchestration global concurrency must be greater than zero");
        }
        Ok(Self {
            semaphore: Arc::new(Semaphore::new(limit)),
            limit,
        })
    }

    #[must_use]
    pub const fn limit(&self) -> usize {
        self.limit
    }
}

impl PartialEq for OrchestrationConcurrencyGate {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.semaphore, &other.semaphore)
    }
}

impl Eq for OrchestrationConcurrencyGate {}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskResult {
    pub index: usize,
    pub id: String,
    pub agent: String,
    pub status: AgentStatus,
    pub output: String,
    #[serde(default)]
    pub usage: pi_ai::Usage,
    /// True when the job settled early because a configured soft budget was
    /// reached (max requests, max tokens, or a yield-after threshold). The job
    /// is not failed: `output` holds the partial result and the parent decides
    /// whether to continue the child.
    #[serde(default)]
    pub soft_budget_exhausted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Outcome of validating the delivered `yield` payload against the
    /// invocation's per-item `outputSchema` contract (absent when the item
    /// carried no contract or the run itself failed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_output: Option<StructuredOutput>,
    pub artifact_ref: String,
    pub history_ref: String,
    pub artifact_uri: String,
}

/// Report of validating a child's delivered `yield` payload against the
/// invocation's `outputSchema` (OMP `outputSchema`/`schemaMode` parity).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StructuredOutput {
    /// Where the effective schema came from — always `"task"` (the invocation
    /// parameter); rpi has no agent-frontmatter or parent-session schema layer
    /// yet, so this is the only source today.
    pub schema_source: String,
    /// Effective validation mode: `"permissive"` or `"strict"`.
    pub schema_mode: String,
    /// Whether the payload parsed as JSON and validated against the schema.
    pub valid: bool,
    /// The parsed payload when it was valid JSON (present even when the schema
    /// rejected it, so the parent can inspect what the child delivered).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    /// Validation/parse failure description; absent when `valid` is true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub struct OrchestrationRuntime {
    inner: Arc<RuntimeInner>,
    owner: bool,
}

/// Exclusive lifecycle claim for a persona destructive operation.
///
/// While held, new spawns and revivals for the persona fail closed. Creation
/// is serialized with the existing spawn lock and durable mutation lock so the
/// active check cannot race a queued spawn or a child settle transaction.
pub struct PersonaLifecycleGuard {
    inner: Arc<RuntimeInner>,
    persona: String,
    release_on_drop: bool,
}

impl std::fmt::Debug for PersonaLifecycleGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PersonaLifecycleGuard")
            .field("persona", &self.persona)
            .field("release_on_drop", &self.release_on_drop)
            .finish_non_exhaustive()
    }
}

impl PersonaLifecycleGuard {
    /// Keep the persona blocked for the lifetime of this runtime. Used after a
    /// destructive write succeeds but catalog reload fails, preventing stale
    /// catalog state from spawning the deleted persona.
    pub fn retain(mut self) {
        self.release_on_drop = false;
    }
}

impl Drop for PersonaLifecycleGuard {
    fn drop(&mut self) {
        if self.release_on_drop {
            self.inner
                .persona_lifecycle_blocks
                .lock()
                .remove(&self.persona);
        }
    }
}

impl Clone for OrchestrationRuntime {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            owner: false,
        }
    }
}

/// Boundary inside [`OrchestrationRuntime::spawn_tasks`] where a
/// test-armed parking hook can hold a spawn, making the interleaving with
/// `commit_orchestration_candidate`'s spawn gate deterministic.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SpawnTestParkPoint {
    /// Parks before the spawn lock is taken: the reload commit runs to
    /// completion first, and the parked spawn must be rejected by the
    /// retiring flag without creating an authoritative child.
    BeforeSpawnLock,
    /// Parks while the spawn lock is held: the commit blocks on the same
    /// gate, then must observe the registered job and defer the replacement.
    AfterSpawnLock,
}

struct RuntimeInner {
    group_id: OnceLock<String>,
    config: OrchestrationConfig,
    factory: ChildSessionFactory,
    storage: OrchestrationStorage,
    semaphore: Arc<Semaphore>,
    shutdown: CancellationToken,
    active: Mutex<HashMap<String, CancellationToken>>,
    active_changed: Notify,
    park_timers: Mutex<HashMap<String, JoinHandle<()>>>,
    jobs: JobManager,
    workflow_scope: Mutex<Option<WorkflowRuntimeScope>>,
    global_concurrency: Mutex<Option<OrchestrationConcurrencyGate>>,
    events: broadcast::Sender<OrchestrationEvent>,
    /// Bounded, newest-last log of every durably delivered group message
    /// (subagent ⇄ subagent included), independent of mailbox consumption:
    /// the workflow page's Recent IRC reads this stream, so messages stay
    /// visible after the recipient consumes them.
    delivered_messages: Mutex<VecDeque<MailboxMessage>>,
    /// Per-runtime agent registry. Isolated from every other runtime so two
    /// sessions may reuse the same group/agent ids independently.
    registry: GlobalRegistry,
    durable: Mutex<Option<super::persistence::DurableRuntime>>,
    durable_bound: AtomicBool,
    /// Workspace roots of the currently bound parent session. Persona
    /// knowledgeRoots are validated against this snapshot before a task spawn
    /// is accepted, rather than failing later inside the child factory.
    parent_workspace: RwLock<Option<crate::WorkspaceRoots>>,
    durable_mutation: Mutex<()>,
    durable_frozen: AtomicBool,
    #[cfg(test)]
    restart_checkpoint_count: AtomicUsize,
    #[cfg(test)]
    restart_checkpoint_error: Mutex<Option<String>>,
    generation_capture: Mutex<Option<crate::application::generation::GenerationCaptureFn>>,
    /// Personas currently claimed by a destructive lifecycle operation.
    persona_lifecycle_blocks: Mutex<std::collections::BTreeSet<String>>,
    rebind_reserved: AtomicBool,
    /// Set by the atomic retirement protocol in
    /// `Application::commit_orchestration_candidate` while the spawn gate is
    /// held: any stale-handle `spawn_tasks` after this point is rejected
    /// before creating an authoritative child, so a reload-driven replacement
    /// can never orphan a job that registered after the active-job check.
    retiring: AtomicBool,

    /// Test-only parking hook used by the deterministic reload-commit race
    /// tests: parks a spawn at the exact check/registration boundary of
    /// [`OrchestrationRuntime::spawn_tasks`] so the interleaving against
    /// `commit_orchestration_candidate`'s spawn gate is deterministic.
    #[cfg(test)]
    spawn_test_park: Mutex<
        Option<(
            SpawnTestParkPoint,
            std::sync::Arc<std::sync::atomic::AtomicBool>,
        )>,
    >,
    /// Role selected with `/role <name> --select`; consulted as the default
    /// agent for child task spawns that do not name an agent explicitly.
    preferred_agent: RwLock<Option<String>>,
    /// Dedup set of (agent, tool) pairs for silently-ignored unknown declared
    /// tools. Each pair produces exactly one warning message for the lifetime
    /// of the runtime (repeated spawns do not re-warn); surfaced via
    /// [`OrchestrationRuntime::unknown_tool_warnings`].
    unknown_tool_warnings: Mutex<std::collections::BTreeSet<(String, String)>>,
}
/// Per-job soft-budget counters shared between the agent-loop stop hook and
/// `run_one`/`run_revival`.
///
/// The hook increments the counters after each completed assistant turn and,
/// when a configured limit is reached, records `triggered` so the run loop can
/// mark the settled job with `soft_budget_exhausted`.
#[derive(Clone, Default)]
struct JobSoftBudgetState {
    requests: Arc<AtomicUsize>,
    tokens: Arc<AtomicU64>,
    triggered: Arc<AtomicBool>,
}

/// Per-job counters for role contract limits (max turns / max tool calls)
/// shared between the turn stop hook and the run loop.
///
/// The hook counts completed assistant turns and the tool calls inside their
/// messages; once a configured limit is reached the child stops cleanly after
/// that turn and the run loop surfaces the triggered limit as a clear reason.
#[derive(Clone, Default)]
struct JobContractState {
    turns: Arc<AtomicUsize>,
    tool_calls: Arc<AtomicUsize>,
    max_turns_triggered: Arc<AtomicBool>,
    max_tool_calls_triggered: Arc<AtomicBool>,
}

/// Per-child delivery state shared between the child-only `yield` tool (inside
/// the child session's tool set) and the run loop (after `child.run` settles).
///
/// The tool records the delivered payload exactly once; `was_called` doubles
/// as the stop signal — the composed turn stop hook ends the run right after
/// the yielding turn so the model never produces trailing text after the
/// delivery. The run loop then projects the payload as the job's final output
/// (OMP's explicit-delivery protocol: the payload, not the trailing assistant
/// text, is what the parent receives).
#[derive(Clone, Default)]
pub struct YieldState {
    called: Arc<AtomicBool>,
    payload: Arc<Mutex<Option<String>>>,
}

impl YieldState {
    /// Record a yield delivery. Only the first call wins; later calls (a
    /// misbehaving model calling `yield` twice in one message) are ignored.
    /// Returns `true` when this call recorded the payload.
    pub fn deliver(&self, text: String) -> bool {
        let mut payload = self.payload.lock();
        if payload.is_none() {
            *payload = Some(text);
            self.called.store(true, Ordering::Release);
            true
        } else {
            false
        }
    }

    /// The delivered payload, if the child called `yield` at least once.
    pub fn payload(&self) -> Option<String> {
        self.payload.lock().clone()
    }

    /// Whether the child called `yield` at least once.
    pub fn was_called(&self) -> bool {
        self.called.load(Ordering::Acquire)
    }
}

/// Appended to a naturally-completed child's output when it never called the
/// `yield` tool. Back-compat: children written before the explicit-delivery
/// protocol still settle with their natural final text; the warning makes the
/// missed delivery observable to the parent.
pub const MISSING_YIELD_WARNING: &str = "SYSTEM WARNING: Subagent exited without calling yield";

/// Appends [`MISSING_YIELD_WARNING`] to a natural child output (an empty
/// output becomes the warning itself).
fn append_missing_yield_warning(output: &mut String) {
    if output.is_empty() {
        *output = MISSING_YIELD_WARNING.to_owned();
    } else {
        output.push_str("\n\n");
        output.push_str(MISSING_YIELD_WARNING);
    }
}

#[derive(Default)]
struct GlobalRegistry {
    groups: Mutex<HashMap<String, HashMap<String, Arc<AgentEntry>>>>,
}

/// A registered `hub wait` interest tied to an agent's mailbox lifetime.
///
/// While at least one `MessageWaiter` is registered for an `AgentEntry`, a
/// matching send is atomically handed to the waiter: the message leaves the
/// mailbox and arrives on `delivery`, so the claim is the actual consumption
/// — a stale pre-check can never strand a message under a claim that never
/// drains it. `from == None` is a wildcard claim. The guard is RAII: dropping
/// it removes the registration on every return path (return/cancel/timeout/
/// drop) and restores any handed-off message that this wait ended without
/// returning, so the message is never lost.
struct MessageWaiter {
    token: u64,
    from: Option<String>,
    reply_to: Option<String>,
    delivery: tokio::sync::mpsc::UnboundedSender<MailboxMessage>,
}

pub(crate) struct MessageWaiterGuard {
    entry: Arc<AgentEntry>,
    token: u64,
    delivery_rx: tokio::sync::mpsc::UnboundedReceiver<MailboxMessage>,
    /// Called when this wait ends without returning an atomically handed-off
    /// message (timeout/abort/drop): the message is back in the mailbox, and
    /// Main-target deliveries are republished (flag false) so the application
    /// still steers them into the main session instead of leaving the reply
    /// stranded under the earlier claimed-flag.
    republish: Option<Arc<dyn Fn(MailboxMessage) + Send + Sync>>,
}

impl Drop for MessageWaiterGuard {
    fn drop(&mut self) {
        {
            let mut waiters = self.entry.waiters.lock();
            if let Some(index) = waiters.iter().position(|waiter| waiter.token == self.token) {
                waiters.remove(index);
            }
        }
        // A message handed off by the atomic one-shot claim but never returned
        // by this wait goes back to the mailbox (it was durably committed
        // before the handoff, so it must not be lost). The restore is forced:
        // it reverses the handoff removal, so the bound never rejects it, and
        // duplicates of the same id are replaced, never doubled.
        self.restore_pending();
    }
}
impl MessageWaiterGuard {
    fn restore_pending(&mut self) {
        if let Ok(message) = self.delivery_rx.try_recv() {
            {
                let mut mailbox = self.entry.mailbox.lock();
                mailbox.retain(|queued| queued.id != message.id);
                mailbox.push_front(message.clone());
            }
            self.entry.message_ready.notify_waiters();
            if let Some(republish) = &self.republish {
                republish(message);
            }
        }
    }
}

struct AgentEntry {
    snapshot: Mutex<AgentSnapshot>,
    mailbox: Mutex<VecDeque<MailboxMessage>>,
    mailbox_capacity: usize,
    message_ready: Notify,
    active_delivery: Mutex<Option<tokio::sync::mpsc::UnboundedSender<MailboxMessage>>>,
    /// Explicit `hub wait` interests. A matching or wildcard entry makes
    /// `deliver_committed` defer to the waiter so the active steering bridge
    /// cannot consume (and acknowledge away) a message the waiter will drain.
    waiters: Mutex<Vec<MessageWaiter>>,
    waiter_seq: AtomicU64,
    cancellation: Mutex<Option<CancellationToken>>,
    idle_park_token: Mutex<Option<CancellationToken>>,
    artifact_path: Mutex<Option<PathBuf>>,
    history_path: Mutex<Option<PathBuf>>,
    durable_info: Mutex<Option<DurableAgentInfo>>,
}
/// Durable material for a spawned agent: enough to reconstruct a
/// `ChildSessionRequest` and resume the child JSONL on revival.
#[derive(Clone, Debug)]
struct DurableAgentInfo {
    definition: super::persistence::PersistedDefinition,
    request: super::persistence::PersistedRequest,
    session_path: Option<PathBuf>,
}

struct PreparedRecoveredState {
    group_entries: HashMap<String, Arc<AgentEntry>>,
    main_mailbox: Vec<MailboxMessage>,
    agents: Vec<super::persistence::PersistedAgent>,
    jobs: PreparedJobRecords,
    delivered_messages: Vec<MailboxMessage>,
}
pub struct PreparedDurableBinding {
    durable: super::persistence::DurableRuntime,
    recovered: Option<PreparedRecoveredState>,
    generation: Option<Arc<crate::application::generation::ApplicationGeneration>>,
    parent_workspace: Option<crate::WorkspaceRoots>,
    initialize: bool,
    install_recovered: bool,
    same_parent: bool,
    inner: Arc<RuntimeInner>,
    committed: bool,
}

impl PreparedDurableBinding {
    fn release_reservation(&mut self) {
        if !self.committed {
            self.committed = true;
            self.inner.rebind_reserved.store(false, Ordering::Release);
        }
    }
}

impl Drop for PreparedDurableBinding {
    fn drop(&mut self) {
        self.release_reservation();
    }
}

impl OrchestrationRuntime {
    pub fn new(config: OrchestrationConfig, factory: ChildSessionFactory) -> Result<Self> {
        config.validate()?;
        let artifact_dir = absolute_lexical(&config.artifact_dir)?;
        let filesystem = CapabilityFs::new().context("initializing orchestration filesystem")?;
        let artifacts = filesystem
            .open_root(
                CapabilityRootPurpose::OrchestrationArtifacts,
                pi_sandbox::AbsolutePath::parse(artifact_dir.clone())
                    .map_err(|_| anyhow!("invalid orchestration artifact directory"))?,
                CapabilityAccess::PrivateReadWrite,
                RootCreation::CreateDirectory,
            )
            .context("opening orchestration artifact capability")?;
        let mut persona_roots = BTreeMap::new();
        for definition in config.catalog.agents() {
            let Some(root) = definition.persona_root() else {
                continue;
            };
            let lease = filesystem
                .open_root(
                    CapabilityRootPurpose::PersonaStore,
                    pi_sandbox::AbsolutePath::parse(root)
                        .map_err(|_| anyhow!("invalid persona root for {:?}", definition.name))?,
                    CapabilityAccess::PrivateReadWrite,
                    RootCreation::Existing,
                )
                .with_context(|| format!("opening persona capability for {:?}", definition.name))?;
            persona_roots.insert(definition.name.clone(), lease);
        }
        let mut skill_files = BTreeMap::new();
        for skill in &config.skills {
            // User and package-global links are resolved only inside their
            // retained discovery root. Project, package-project, and explicit
            // skills remain strict no-follow.
            let file = if skill.source.allows_managed_links() {
                let root = skill.managed_root.as_deref().ok_or_else(|| {
                    anyhow!(
                        "managed skill {:?} has no source-root authorization",
                        skill.name
                    )
                })?;
                let relative = skill.file_path.strip_prefix(root).map_err(|_| {
                    anyhow!("managed skill {:?} is outside its source root", skill.name)
                })?;
                let lease = filesystem
                    .open_root(
                        CapabilityRootPurpose::ProjectResources,
                        pi_sandbox::AbsolutePath::parse(root.to_path_buf())
                            .map_err(|_| anyhow!("invalid managed skill source root"))?,
                        CapabilityAccess::ReadOnly,
                        RootCreation::Existing,
                    )
                    .with_context(|| format!("opening managed skill root for {:?}", skill.name))?;
                let managed = lease
                    .open_managed_file_within_root(relative)
                    .with_context(|| format!("opening orchestration skill {:?}", skill.name))?;
                if skill
                    .managed_project_boundary
                    .as_deref()
                    .is_some_and(|boundary| managed.resolved_path().starts_with(boundary))
                {
                    bail!("managed skill target crosses the current project boundary");
                }
                managed.into_file()
            } else {
                let absolute = pi_sandbox::AbsolutePath::parse(skill.file_path.clone())
                    .map_err(|_| anyhow!("invalid orchestration skill path"))?;
                match filesystem.open_exact_file(CapabilityRootPurpose::ProjectResources, absolute)
                {
                    Ok(file) => file,
                    Err(error) if is_link_blocked(&error) => {
                        bail!(
                            "symbolic links are not allowed for {} skill `{}`",
                            crate::selector::skill_source_name(skill.source),
                            skill.name,
                        );
                    }
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("opening orchestration skill {:?}", skill.name)
                        });
                    }
                }
            };
            skill_files.insert(skill.name.clone(), Arc::new(file));
        }
        let storage = OrchestrationStorage {
            artifacts,
            persona_roots,
            skill_files,
        };
        let mut config = config;
        config.artifact_dir = artifact_dir;
        let preferred_agent = config.preferred_agent.clone();
        let (events, _) = broadcast::channel(256);
        let registry = GlobalRegistry::default();
        let jobs = JobManager::with_retention(
            JobRetention {
                max_settled: config.max_retained_jobs,
                ttl: config.retained_job_ttl,
            },
            config.job_clock.clone(),
        );
        Ok(Self {
            inner: Arc::new(RuntimeInner {
                group_id: OnceLock::new(),
                semaphore: Arc::new(Semaphore::new(config.max_concurrency)),
                storage,
                config,
                factory,
                shutdown: CancellationToken::new(),
                active: Mutex::new(HashMap::new()),
                active_changed: Notify::new(),
                park_timers: Mutex::new(HashMap::new()),
                jobs,
                workflow_scope: Mutex::new(None),
                global_concurrency: Mutex::new(None),
                events,
                delivered_messages: Mutex::new(VecDeque::new()),
                registry,
                durable: Mutex::new(None),
                durable_bound: AtomicBool::new(false),
                parent_workspace: RwLock::new(None),
                durable_mutation: Mutex::new(()),
                durable_frozen: AtomicBool::new(false),
                #[cfg(test)]
                restart_checkpoint_count: AtomicUsize::new(0),
                #[cfg(test)]
                restart_checkpoint_error: Mutex::new(None),
                generation_capture: Mutex::new(None),
                persona_lifecycle_blocks: Mutex::new(std::collections::BTreeSet::new()),
                rebind_reserved: AtomicBool::new(false),
                retiring: AtomicBool::new(false),
                #[cfg(test)]
                spawn_test_park: Mutex::new(None),
                preferred_agent: RwLock::new(preferred_agent),
                unknown_tool_warnings: Mutex::new(std::collections::BTreeSet::new()),
            }),
            owner: true,
        })
    }

    #[must_use]
    pub fn group_id(&self) -> &str {
        if let Some(group_id) = self.inner.group_id.get() {
            return group_id;
        }
        let generated = Uuid::now_v7().to_string();
        self.initialize_group_id(&generated)
            .unwrap_or_else(|existing| existing)
    }

    fn initialize_group_id<'a>(&'a self, group_id: &str) -> std::result::Result<&'a str, &'a str> {
        let configured_main_agent_id = self.inner.config.main_agent_id.clone();
        let mailbox_capacity = self.inner.config.mailbox_capacity;
        let established = self.inner.group_id.get_or_init(|| {
            self.inner.registry.register_main(
                group_id,
                AgentSnapshot {
                    id: configured_main_agent_id.clone(),
                    display_name: configured_main_agent_id.clone(),
                    agent: configured_main_agent_id,
                    parent_id: None,
                    status: AgentStatus::Idle,
                    created_at: now_millis(),
                    last_activity: now_millis(),
                    unread: 0,
                    artifact_ref: None,
                    history_ref: None,
                },
                mailbox_capacity,
            );
            group_id.to_owned()
        });
        if established == group_id {
            Ok(established)
        } else {
            Err(established)
        }
    }

    fn adopt_durable_group_id(&self, group_id: &str) -> Result<()> {
        self.initialize_group_id(group_id)
            .map(|_| ())
            .map_err(|established| anyhow!(
                "durable orchestration group id mismatch: runtime {established:?}, sidecar {group_id:?}"
            ))
    }

    /// Select the role used for child task spawns that do not name an agent
    /// explicitly. `None` clears the preference (back to ranked/default
    /// selection). The selected role must still be enabled and compatible.
    pub fn set_preferred_agent(&self, name: Option<&str>) {
        *self.inner.preferred_agent.write() = name.map(str::to_owned);
    }

    /// The currently preferred role for unnamed task spawns, if any.
    #[must_use]
    pub fn preferred_agent(&self) -> Option<String> {
        self.inner.preferred_agent.read().clone()
    }

    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<OrchestrationEvent> {
        self.inner.events.subscribe()
    }

    #[must_use]
    pub(crate) fn presentation_events(&self) -> Vec<OrchestrationEvent> {
        let mut events = self
            .jobs(None)
            .into_iter()
            .map(|job| OrchestrationEvent::JobUpdated {
                group_id: self.group_id().to_owned(),
                job: presentation_job_snapshot(job),
            })
            .collect::<Vec<_>>();
        events.extend(self.list(self.main_agent_id()).into_iter().map(|agent| {
            OrchestrationEvent::AgentUpdated {
                group_id: self.group_id().to_owned(),
                agent: presentation_agent_snapshot(agent),
            }
        }));
        events
    }

    pub(crate) fn stage_generation(
        &self,
        provenance: std::sync::Arc<crate::application::generation::GenerationProvenance>,
    ) -> anyhow::Result<std::sync::Arc<crate::application::generation::OrchestrationGeneration>>
    {
        crate::application::generation::OrchestrationGeneration::enabled(
            provenance,
            self.clone(),
            crate::application::generation::OrchestrationSnapshot::default().into(),
            self.inner.storage.artifacts.clone(),
        )
    }

    #[must_use]
    pub fn catalog(&self) -> &AgentCatalog {
        &self.inner.config.catalog
    }

    /// Agent definitions that are enabled and compatible with this runtime.
    #[must_use]
    pub fn enabled_agents(&self) -> Vec<&AgentDefinition> {
        let available = crate::available_models();
        let parent_model = self.inner.config.current_parent_model();
        self.inner
            .config
            .catalog
            .agents()
            .iter()
            .filter(|agent| {
                let settings = self.inner.config.agent_settings.get(&agent.name);
                settings.map_or(true, crate::AgentRuntimeSettings::is_enabled)
                    && crate::agent_compatibility_error(agent, settings, &parent_model, &available)
                        .is_none()
            })
            .collect()
    }

    /// Enabled persona marked `userProxy: true`, if exactly one is configured.
    #[must_use]
    pub fn user_proxy_agent(&self) -> Option<&AgentDefinition> {
        let mut found = None;
        for agent in self.enabled_agents() {
            if agent.is_persona() && agent.user_proxy {
                if found.is_some() {
                    return None;
                }
                found = Some(agent);
            }
        }
        found
    }

    /// Whether `to` names the human user / Main and should be rewritten to
    /// the configured user-proxy persona.
    #[must_use]
    pub fn is_user_ask_target(&self, to: &str) -> bool {
        let trimmed = to.trim();
        trimmed.eq_ignore_ascii_case("user")
            || trimmed.eq_ignore_ascii_case("me")
            || trimmed == self.inner.config.main_agent_id
    }

    /// Incompatibility diagnostics for configured agents excluded from advertisement.
    #[must_use]
    pub fn incompatible_agent_diagnostics(&self) -> Vec<String> {
        let available = crate::available_models();
        let parent_model = self.inner.config.current_parent_model();
        self.inner
            .config
            .catalog
            .agents()
            .iter()
            .filter_map(|agent| {
                crate::agent_compatibility_error(
                    agent,
                    self.inner.config.agent_settings.get(&agent.name),
                    &parent_model,
                    &available,
                )
                .map(|error| error.to_string())
            })
            .collect()
    }

    /// Deduped warnings for agent-declared tools that were silently ignored
    /// (unknown names never injected into children). Exactly one message per
    /// (agent, tool) pair for the lifetime of this runtime, sorted by
    /// (agent, tool) for determinism. Unknown tools never make an agent
    /// unavailable, so these are advisory only.
    #[must_use]
    pub fn unknown_tool_warnings(&self) -> Vec<String> {
        self.inner
            .unknown_tool_warnings
            .lock()
            .iter()
            .map(|(agent, tool)| crate::unknown_tools_warning(agent, std::slice::from_ref(tool)))
            .collect()
    }

    /// Record one deduped warning per (agent, tool) pair so repeated spawns of
    /// the same agent never re-warn.
    fn record_unknown_tool_warnings(&self, agent: &str, unknown: &[String]) {
        let mut recorded = self.inner.unknown_tool_warnings.lock();
        for tool in unknown {
            recorded.insert((agent.to_owned(), tool.clone()));
        }
    }

    fn live_agents_name_parent(&self, agent_id: &str) -> bool {
        if agent_id == self.inner.config.main_agent_id {
            return true;
        }
        let group_id = self.group_id();
        let groups = self.inner.registry.groups.lock();
        let Some(entries) = groups.get(group_id) else {
            return false;
        };
        entries.iter().any(|(id, entry)| {
            if id == agent_id {
                return false;
            }
            if entry.snapshot.lock().parent_id.as_deref() == Some(agent_id) {
                return true;
            }
            entry
                .durable_info
                .lock()
                .as_ref()
                .is_some_and(|info| info.request.parent_id == agent_id)
        })
    }

    fn retire_retained_job(&self, agent_id: &str, job_id: &str) {
        let artifact_path = self
            .inner
            .config
            .artifact_dir
            .join(format!("{agent_id}-{job_id}.md"));
        let history_path = self
            .inner
            .config
            .artifact_dir
            .join(format!("{agent_id}-{job_id}.history.json"));
        if !(remove_retained_file(&self.inner.storage.artifacts, &artifact_path)
            && remove_retained_file(&self.inner.storage.artifacts, &history_path))
        {
            return;
        }
        if !self.live_agents_name_parent(agent_id) {
            self.inner.registry.remove_agent_if_paths(
                self.group_id(),
                agent_id,
                &artifact_path,
                &history_path,
            );
        }
        self.inner.jobs.remove_settled(job_id);
    }

    fn prune_retained_jobs(&self) {
        let _durable_mutation = self.inner.durable_mutation.lock();
        if self.inner.rebind_reserved.load(Ordering::Acquire) {
            return;
        }
        for candidate in self.inner.jobs.prune_candidates() {
            self.retire_retained_job(&candidate.agent_id, &candidate.job_id);
        }
    }

    fn cleanup_retained_jobs(&self) {
        let _durable_mutation = self.inner.durable_mutation.lock();
        if self.inner.rebind_reserved.load(Ordering::Acquire) {
            return;
        }
        for candidate in self.inner.jobs.settled_candidates() {
            self.retire_retained_job(&candidate.agent_id, &candidate.job_id);
        }
    }

    /// Fail when `name` is explicitly disabled in settings.
    pub fn ensure_agent_enabled(&self, name: &str) -> Result<()> {
        if self
            .inner
            .config
            .agent_settings
            .get(name)
            .is_some_and(|settings| settings.enabled == Some(false))
        {
            return Err(crate::agent_disabled_error(name));
        }
        Ok(())
    }

    #[must_use]
    pub fn skills(&self) -> &[OrchestrationSkill] {
        &self.inner.config.skills
    }

    #[must_use]
    pub fn main_agent_id(&self) -> &str {
        &self.inner.config.main_agent_id
    }

    #[must_use]
    pub fn artifact_dir(&self) -> &Path {
        &self.inner.config.artifact_dir
    }

    #[must_use]
    pub fn max_recursion_depth(&self) -> usize {
        self.inner.config.max_recursion_depth
    }
    #[must_use]
    pub fn max_concurrency(&self) -> usize {
        self.inner.config.max_concurrency
    }

    /// Bind this runtime to the parent recorder without creating or rewriting
    /// durable state. Existing state is validated and prebuilt before commit,
    /// but recovery remains explicit through [`Self::recover`].
    pub fn bind_parent_session(&self, parent: &Session) -> Result<()> {
        let mut prepared = self.prepare_parent_binding(parent)?;
        if !prepared.same_parent {
            prepared.install_recovered = prepared.recovered.is_some();
        }
        self.commit_parent_binding(prepared);
        Ok(())
    }

    /// Reserve a durable rebind and fully validate/prebuild its replacement.
    ///
    /// The reservation spans preparation through commit/drop. Runtime mutation
    /// entry points fail closed while it is held, so the prepared registry and
    /// job snapshots cannot become stale before the infallible swap.
    pub(crate) fn prepare_parent_identity(
        &self,
        session_id: String,
        session_path: PathBuf,
        parent_directory: crate::fs::CapabilityRootLease,
        parent_relative: PathBuf,
    ) -> Result<PreparedDurableBinding> {
        self.prepare_parent_identity_binding(
            session_id,
            session_path,
            parent_directory,
            parent_relative,
            true,
        )
    }

    pub(crate) fn prepare_parent_binding(
        &self,
        parent: &Session,
    ) -> Result<PreparedDurableBinding> {
        let (session_id, session_path) = parent
            .recorder_info()
            .ok_or_else(|| anyhow!("parent session recording is unavailable"))?;
        let (parent_directory, parent_relative) = parent
            .recorder_file_authority()
            .ok_or_else(|| anyhow!("parent session recording authority is unavailable"))?;
        self.prepare_parent_identity(session_id, session_path, parent_directory, parent_relative)
            .map(|mut prepared| {
                prepared.parent_workspace = Some(parent.workspace_roots().clone());
                prepared
            })
    }
    fn prepare_parent_reset_binding(&self, parent: &Session) -> Result<PreparedDurableBinding> {
        let (session_id, session_path) = parent
            .recorder_info()
            .ok_or_else(|| anyhow!("parent session recording is unavailable"))?;
        let (parent_directory, parent_relative) = parent
            .recorder_file_authority()
            .ok_or_else(|| anyhow!("parent session recording authority is unavailable"))?;
        self.prepare_parent_identity_binding(
            session_id,
            session_path,
            parent_directory,
            parent_relative,
            false,
        )
        .map(|mut prepared| {
            prepared.parent_workspace = Some(parent.workspace_roots().clone());
            prepared
        })
    }

    pub(crate) fn durable_policy_for_parent(
        &self,
        parent: &Session,
    ) -> Result<Option<crate::DurablePolicyState>> {
        let (session_id, session_path) = parent
            .recorder_info()
            .ok_or_else(|| anyhow!("parent session recording is unavailable"))?;
        let (parent_directory, parent_relative) = parent
            .recorder_file_authority()
            .ok_or_else(|| anyhow!("parent session recording authority is unavailable"))?;
        let durable = super::persistence::DurableRuntime::new_with_main_agent_id(
            session_id,
            session_path,
            parent_directory,
            parent_relative,
            self.main_agent_id().to_owned(),
        )?;
        match durable.load_optional()? {
            Some(state) => {
                self.adopt_durable_group_id(&state.group_id)?;
                Ok(Some(state.durable_policy))
            }
            None => Ok(None),
        }
    }

    fn capture_generation(
        &self,
    ) -> Result<Arc<crate::application::generation::ApplicationGeneration>> {
        self.inner
            .generation_capture
            .lock()
            .clone()
            .ok_or_else(|| anyhow!("durable orchestration generation is unavailable"))?()
    }

    fn durable_policy_state(&self) -> Result<crate::DurablePolicyState> {
        let generation = self.capture_generation()?;
        crate::DurablePolicyState::capture(&generation).map_err(anyhow::Error::from)
    }
    pub(crate) fn bind_generation_capture(
        &self,
        generation_capture: crate::application::generation::GenerationCaptureFn,
    ) -> Result<()> {
        generation_capture()?;
        *self.inner.generation_capture.lock() = Some(generation_capture);
        Ok(())
    }
    pub(crate) fn install_generation_capture(
        &self,
        generation_capture: crate::application::generation::GenerationCaptureFn,
    ) {
        *self.inner.generation_capture.lock() = Some(generation_capture);
    }

    fn bind_parent_generation_capture(&self, parent: &Session) -> Result<()> {
        if self.inner.generation_capture.lock().is_none() {
            self.bind_generation_capture(parent.generation_capture_fn())?;
        }
        Ok(())
    }

    fn prepare_parent_identity_binding(
        &self,
        session_id: String,
        session_path: PathBuf,
        parent_directory: crate::fs::CapabilityRootLease,
        parent_relative: PathBuf,
        should_recover: bool,
    ) -> Result<PreparedDurableBinding> {
        let _spawn_guard = self.inner.jobs.lock_spawns();
        let durable_mutation = self.inner.durable_mutation.lock();
        self.inner
            .rebind_reserved
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| anyhow!("durable orchestration rebind is already in progress"))?;
        let result = (|| {
            let durable = super::persistence::DurableRuntime::new_with_main_agent_id(
                session_id,
                session_path,
                parent_directory,
                parent_relative,
                self.main_agent_id().to_owned(),
            )?;
            let same_parent = self.inner.durable.lock().as_ref().is_some_and(|current| {
                current.parent_session_id() == durable.parent_session_id()
                    && current.parent_session_path() == durable.parent_session_path()
            });
            if !same_parent
                && (!self.inner.active.lock().is_empty()
                    || self
                        .inner
                        .jobs
                        .snapshots(None)
                        .iter()
                        .any(|job| !job.status.is_settled()))
            {
                bail!("cannot rebind durable orchestration while child jobs are active");
            }
            let generation = self.capture_generation()?;
            let recovered = if !should_recover || same_parent {
                None
            } else {
                durable
                    .load_optional()?
                    .map(|state| -> Result<PreparedRecoveredState> {
                        let recovered =
                            self.prepare_recovered_state(&durable, &generation, state)?;
                        durable.persist(&super::persistence::build_state(
                            self.group_id(),
                            durable.parent_session_id(),
                            durable.parent_session_path(),
                            crate::DurablePolicyState::capture(&generation)?,
                            recovered.main_mailbox.clone(),
                            recovered.delivered_messages.clone(),
                            recovered.agents.clone(),
                            recovered.jobs.snapshots().to_vec(),
                        ))?;
                        Ok(recovered)
                    })
                    .transpose()?
            };
            let initialize = !should_recover || (!same_parent && recovered.is_none());
            Ok(PreparedDurableBinding {
                durable,
                recovered,
                generation: Some(generation),
                parent_workspace: None,
                initialize,
                install_recovered: false,
                same_parent,
                inner: self.inner.clone(),
                committed: false,
            })
        })();
        drop(durable_mutation);
        if result.is_err() {
            self.inner.rebind_reserved.store(false, Ordering::Release);
        }
        result
    }

    /// Materialize fresh durable state before any live session cutover.
    pub(crate) fn initialize_prepared_binding(
        &self,
        prepared: &mut PreparedDurableBinding,
    ) -> Result<()> {
        if prepared.initialize {
            let generation = prepared.generation.as_ref().ok_or_else(|| {
                anyhow!("durable orchestration recovery generation is unavailable")
            })?;
            prepared.durable.persist(&super::persistence::build_state(
                self.group_id(),
                prepared.durable.parent_session_id(),
                prepared.durable.parent_session_path(),
                crate::DurablePolicyState::capture(generation)?,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
            ))?;
            prepared.recovered = Some(PreparedRecoveredState {
                group_entries: self.prepared_group_entries(HashMap::new()),
                main_mailbox: Vec::new(),
                delivered_messages: Vec::new(),
                agents: Vec::new(),
                jobs: self.inner.jobs.prepare_replacement(Vec::new()),
            });
            prepared.initialize = false;
            prepared.install_recovered = true;
        }
        Ok(())
    }

    pub(crate) fn initialize_prepared_parent(
        &self,
        prepared: &mut PreparedDurableBinding,
    ) -> Result<()> {
        if prepared.same_parent && self.inner.durable_bound.load(Ordering::Acquire) {
            return Ok(());
        }
        if prepared.same_parent && prepared.recovered.is_none() && !prepared.initialize {
            match prepared.durable.load_optional()? {
                Some(state) => {
                    let generation = prepared.generation.as_ref().ok_or_else(|| {
                        anyhow!("durable orchestration recovery generation is unavailable")
                    })?;
                    let recovered =
                        self.prepare_recovered_state(&prepared.durable, generation, state)?;
                    prepared.durable.persist(&super::persistence::build_state(
                        self.group_id(),
                        prepared.durable.parent_session_id(),
                        prepared.durable.parent_session_path(),
                        crate::DurablePolicyState::capture(generation)?,
                        recovered.main_mailbox.clone(),
                        recovered.delivered_messages.clone(),
                        recovered.agents.clone(),
                        recovered.jobs.snapshots().to_vec(),
                    ))?;
                    prepared.recovered = Some(recovered);
                }
                None => prepared.initialize = true,
            }
        }
        self.initialize_prepared_binding(prepared)?;
        prepared.install_recovered = true;
        Ok(())
    }

    pub(crate) fn commit_prepared_parent(&self, prepared: PreparedDurableBinding) {
        if self.install_prepared_parent(prepared) {
            self.revive_prepared_parent();
        }
    }

    /// Install a fully prepared parent binding without starting recovered
    /// children. The caller may inspect the installed job registry and bind
    /// application-level ownership before making revival observable.
    #[must_use]
    pub(crate) fn install_prepared_parent(
        &self,
        mut prepared: PreparedDurableBinding,
    ) -> bool {
        let _durable_mutation = self.inner.durable_mutation.lock();
        let installed_recovered = if !prepared.same_parent {
            let recovered = prepared
                .install_recovered
                .then(|| prepared.recovered.take())
                .flatten();
            *self.inner.durable.lock() = Some(prepared.durable.clone());
            self.inner.durable_bound.store(true, Ordering::Release);
            if let Some(workspace) = prepared.parent_workspace.take() {
                *self.inner.parent_workspace.write() = Some(workspace);
            }
            if let Some(recovered) = recovered {
                self.install_prepared_state(recovered);
                true
            } else {
                false
            }
        } else if prepared.install_recovered {
            let installed = if let Some(recovered) = prepared.recovered.take() {
                self.install_prepared_state(recovered);
                true
            } else {
                false
            };
            *self.inner.durable.lock() = Some(prepared.durable.clone());
            self.inner.durable_bound.store(true, Ordering::Release);
            if let Some(workspace) = prepared.parent_workspace.take() {
                *self.inner.parent_workspace.write() = Some(workspace);
            }
            installed
        } else {
            false
        };
        prepared.release_reservation();
        installed_recovered
    }

    pub(crate) fn revive_prepared_parent(&self) {
        self.revive_recovered_jobs();
    }

    /// Install a fully prepared binding and immediately revive recovered
    /// children. Callers that need to bind ownership first use the split
    /// `install_prepared_parent`/`revive_prepared_parent` sequence instead.
    pub(crate) fn commit_parent_binding(&self, prepared: PreparedDurableBinding) {
        self.commit_prepared_parent(prepared);
    }

    /// Bind to the parent recorder and either recover an existing sidecar or
    /// initialize fresh state when no sidecar exists. The resumed application
    /// generation must be bound before any recovered registry is constructed.
    pub fn bind_and_recover(&self, parent: &Session) -> Result<()> {
        let already_bound = self.inner.durable_bound.load(Ordering::Acquire);
        self.bind_parent_generation_capture(parent)?;
        let mut prepared = self.prepare_parent_binding(parent)?;
        if prepared.same_parent && already_bound {
            self.commit_parent_binding(prepared);
            return Ok(());
        }
        if prepared.same_parent {
            match prepared.durable.load_optional()? {
                Some(state) => {
                    let generation = prepared.generation.as_ref().ok_or_else(|| {
                        anyhow!("durable orchestration recovery generation is unavailable")
                    })?;
                    let recovered =
                        self.prepare_recovered_state(&prepared.durable, generation, state)?;
                    prepared.durable.persist(&super::persistence::build_state(
                        self.group_id(),
                        prepared.durable.parent_session_id(),
                        prepared.durable.parent_session_path(),
                        crate::DurablePolicyState::capture(generation)?,
                        recovered.main_mailbox.clone(),
                        recovered.delivered_messages.clone(),
                        recovered.agents.clone(),
                        recovered.jobs.snapshots().to_vec(),
                    ))?;
                    prepared.recovered = Some(recovered);
                }
                None => prepared.initialize = true,
            }
        }
        self.initialize_prepared_binding(&mut prepared)?;
        prepared.install_recovered = true;
        self.commit_parent_binding(prepared);
        Ok(())
    }

    /// Bind to the parent recorder and initialize fresh durable state,
    /// discarding any existing sidecar. Used when a live orchestration runtime
    /// is replaced by a non-equivalent reload after the previous runtime has
    /// been explicitly shut down, so cancelled jobs cannot be recovered into
    /// the replacement configuration.
    pub(crate) fn bind_and_reset(&self, parent: &Session) -> Result<()> {
        self.bind_parent_generation_capture(parent)?;
        let mut prepared = self.prepare_parent_reset_binding(parent)?;
        prepared.recovered = None;
        prepared.initialize = true;
        self.initialize_prepared_binding(&mut prepared)?;
        self.commit_parent_binding(prepared);
        Ok(())
    }

    /// Persist the current live snapshot to the durable sidecar. Errors
    /// propagate because a loadable sidecar is part of the reload contract.
    pub(crate) fn persist_durable_state(&self) -> Result<()> {
        self.persist_state()
    }

    /// Persist the live sidecar and freeze it so process teardown cannot
    /// rewrite recovered Running/Queued identities. Does not cancel waiters
    /// and does not mark the runtime shut down.
    pub fn checkpoint_for_restart(&self) -> Result<()> {
        let _spawn_guard = self.inner.jobs.lock_spawns();
        let _durable_mutation = self.inner.durable_mutation.lock();
        if self.inner.rebind_reserved.load(Ordering::Acquire) {
            bail!("durable orchestration rebind is in progress");
        }
        #[cfg(test)]
        if let Some(message) = self.inner.restart_checkpoint_error.lock().take() {
            bail!(message);
        }
        self.persist_state()?;
        self.inner.durable_frozen.store(true, Ordering::Release);
        #[cfg(test)]
        self.inner
            .restart_checkpoint_count
            .fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Reopens a runtime after the process-level restart checkpoint failed.
    /// The latest live state is persisted before ordinary mutation resumes.
    pub(crate) fn resume_after_failed_restart_checkpoint(&self) -> Result<()> {
        let _spawn_guard = self.inner.jobs.lock_spawns();
        let _durable_mutation = self.inner.durable_mutation.lock();
        if !self.inner.durable_frozen.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        self.persist_state()
            .context("persisting orchestration after failed restart checkpoint")
    }

    #[cfg(test)]
    pub(crate) fn fail_next_restart_checkpoint_for_test(&self, message: &str) {
        *self.inner.restart_checkpoint_error.lock() = Some(message.to_owned());
    }

    #[cfg(test)]
    pub(crate) fn is_restart_checkpointed_for_test(&self) -> bool {
        self.inner.durable_frozen.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn durable_state_for_test(&self) -> Result<super::persistence::DurableState> {
        self.inner
            .durable
            .lock()
            .clone()
            .ok_or_else(|| anyhow!("durable orchestration binding is unavailable"))?
            .load()
    }

    #[cfg(test)]
    pub(crate) fn restart_checkpoint_count_for_test(&self) -> usize {
        self.inner.restart_checkpoint_count.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn is_durable(&self) -> bool {
        self.inner.durable_bound.load(Ordering::Acquire)
    }

    /// Persist a live snapshot. Snapshot capture and atomic replacement run
    /// under the durable runtime's single ordering lock.
    fn persist_state(&self) -> Result<()> {
        if !self.inner.durable_bound.load(Ordering::Acquire) {
            return Ok(());
        }
        if self.inner.durable_frozen.load(Ordering::Acquire) {
            return Ok(());
        }
        let durable = self
            .inner
            .durable
            .lock()
            .clone()
            .ok_or_else(|| anyhow!("durable orchestration binding is unavailable"))?;
        durable.persist_with(|| {
            Ok(super::persistence::build_state(
                self.group_id(),
                durable.parent_session_id(),
                durable.parent_session_path(),
                self.durable_policy_state()?,
                self.collect_main_mailbox(),
                self.delivered_messages(),
                self.collect_persisted_agents(),
                self.inner.jobs.snapshots(None),
            ))
        })
    }

    fn collect_main_mailbox(&self) -> Vec<MailboxMessage> {
        self.inner
            .registry
            .get(self.group_id(), self.main_agent_id())
            .map(|entry| entry.mailbox.lock().iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Collect all non-main agents with their persisted definition, request,
    /// session path, and mailbox for durable state.
    fn collect_persisted_agents(&self) -> Vec<super::persistence::PersistedAgent> {
        let group_id = self.group_id().to_owned();
        let mut agents = Vec::new();
        let groups = self.inner.registry.groups.lock();
        if let Some(entries) = groups.get(&group_id) {
            for (id, entry) in entries {
                if *id == self.inner.config.main_agent_id {
                    continue;
                }
                let snapshot = entry.snapshot.lock().clone();
                let mailbox = entry.mailbox.lock().iter().cloned().collect::<Vec<_>>();
                // The definition and request are only available for agents that
                // were spawned through spawn_tasks; they are stored on the entry
                // via the durable extension. For agents recovered from a prior
                // process, the persisted definition/request come from the sidecar.
                let durable_info = entry.durable_info.lock();
                agents.push(super::persistence::PersistedAgent {
                    snapshot,
                    definition: durable_info
                        .as_ref()
                        .map(|info| info.definition.clone())
                        .unwrap_or_else(|| super::persistence::PersistedDefinition {
                            name: id.clone(),
                            description: String::new(),
                            system_prompt: String::new(),
                            tools: None,
                            autoload_skills: Vec::new(),
                            model: None,
                            thinking_level: None,
                            max_turns: None,
                            max_tool_calls: None,
                            timeout_secs: None,
                            disallowed_tools: Vec::new(),
                            capability_ceiling: None,
                            source: super::persistence::PersistedDefinitionSource::Bundled,
                            path: None,
                            trusted: true,
                            kind: super::AgentDefinitionKind::Agent,
                            personality: None,
                            soft_budget: None,
                            knowledge_roots: Vec::new(),
                            user_proxy: false,
                        }),
                    request: durable_info
                        .as_ref()
                        .map(|info| info.request.clone())
                        .unwrap_or_else(|| super::persistence::PersistedRequest {
                            child_id: id.clone(),
                            parent_id: self.inner.config.main_agent_id.clone(),
                            depth: 1,
                            assignment: String::new(),
                            system_prompt: String::new(),
                            requested_tool_names: None,
                            thinking_level: None,
                            max_tools_per_agent: self.inner.config.max_tools_per_agent,
                            model_provider: None,
                            model_id: None,
                            output_schema: None,
                            schema_mode: None,
                        }),
                    session_path: durable_info.as_ref().and_then(|info| {
                        info.session_path
                            .as_ref()
                            .map(|p| p.to_string_lossy().into_owned())
                    }),
                    mailbox,
                });
            }
        }
        drop(groups);
        agents
    }

    fn rebuild_revival_request(
        &self,
        target: &str,
        snapshot: &AgentSnapshot,
        info: &DurableAgentInfo,
        session_path: Option<&PathBuf>,
    ) -> Result<Option<ChildSessionRequest>> {
        let definition = match self.inner.config.catalog.get(&info.definition.name) {
            Some(definition) => definition.clone(),
            None => return Ok(None),
        };
        if !definition.trusted
            || self
                .inner
                .config
                .agent_settings
                .get(&definition.name)
                .is_some_and(|settings| settings.enabled == Some(false))
        {
            return Ok(None);
        }
        let available = crate::available_models();
        let parent_model = self.inner.config.current_parent_model();
        let resolved = match crate::resolve_agent_model(
            &definition,
            self.inner.config.agent_settings.get(&definition.name),
            &parent_model,
            &available,
        ) {
            Ok(resolved) => resolved,
            Err(_) => return Ok(None),
        };
        let (persona_skill_store, persona_skills) = self.persona_skill_context(&definition)?;
        let shared_skills = self.shared_skill_snapshot();
        let peer_roster = self.sibling_roster(target);
        let system_prompt = self.child_system_prompt_with_persona_skills(
            &definition,
            &info.request.assignment,
            None,
            info.request.output_schema.as_ref(),
            info.request.schema_mode.as_deref(),
            &peer_roster,
            &persona_skills,
        )?;
        let requested_tool_names = crate::effective_agent_tool_names(
            &definition,
            self.inner.config.agent_settings.get(&definition.name),
        )
        .map(<[String]>::to_vec);
        let orchestration_tools = self.agent_tools(target, info.request.depth);
        Ok(super::persistence::reconstruct_request(
            &super::persistence::PersistedAgent {
                snapshot: snapshot.clone(),
                definition: info.definition.clone(),
                request: info.request.clone(),
                session_path: session_path.map(|path| path.to_string_lossy().into_owned()),
                mailbox: Vec::new(),
            },
            resolved.model.clone(),
            resolved.sequence,
            resolved.selected_index,
            &definition,
            system_prompt,
            requested_tool_names,
            orchestration_tools,
            self.inner.config.max_tools_per_agent,
            persona_skill_store,
            persona_skills,
            shared_skills,
        )
        .into())
    }

    fn spawn_revival_runner(
        &self,
        agent_id: String,
        request: ChildSessionRequest,
        session_path: Option<PathBuf>,
        assignment: String,
        cancel: CancellationToken,
        job_id: String,
    ) {
        let runtime = self.clone();
        tokio::spawn(async move {
            let result = runtime
                .run_revival(agent_id, request, session_path, assignment, cancel, &job_id)
                .await;
            {
                let _durable_mutation = runtime.inner.durable_mutation.lock();
                if !runtime.inner.rebind_reserved.load(Ordering::Acquire)
                    && let Some(job) = runtime.inner.jobs.finish(&job_id, result, now_millis())
                {
                    let job = match runtime.persist_state() {
                        Ok(()) => job,
                        Err(error) => runtime
                            .inner
                            .jobs
                            .append_result_error(&job_id, &error.to_string())
                            .unwrap_or(job),
                    };
                    runtime.publish_job(job);
                }
            }
            runtime.prune_retained_jobs();
        });
    }

    fn settle_unrevivable_job(&self, job: &JobSnapshot, error: &str) {
        let item = TaskItem {
            index: 0,
            id: job.agent_id.clone(),
            agent: job.agent.clone(),
            assignment: job.description.clone().unwrap_or_default(),
            todo_task_id: job.todo_task_id.clone(),
            ..Default::default()
        };
        let result = self.failed_result(&item, &job.id, AgentStatus::Parked, error);
        if let Some(job) = self.inner.jobs.finish(&job.id, result, now_millis()) {
            let job = match self.persist_state() {
                Ok(()) => job,
                Err(persist_error) => self
                    .inner
                    .jobs
                    .append_result_error(&job.id, &persist_error.to_string())
                    .unwrap_or(job),
            };
            self.publish_job(job);
        }
    }

    /// Continue recovered Running/Queued jobs on the same job identity.
    /// Must run only after the durable rebind reservation is released.
    fn revive_recovered_jobs(&self) {
        if !self.inner.durable_bound.load(Ordering::Acquire) {
            return;
        }
        if self.inner.rebind_reserved.load(Ordering::Acquire) {
            return;
        }
        let Some(durable) = self.inner.durable.lock().clone() else {
            return;
        };
        for job in self.inner.jobs.snapshots(None) {
            if job.status.is_settled() {
                continue;
            }
            if self.inner.active.lock().contains_key(&job.agent_id) {
                continue;
            }
            let Some(entry) = self.inner.registry.get(self.group_id(), &job.agent_id) else {
                self.settle_unrevivable_job(
                    &job,
                    "durable child identity is unavailable for revival",
                );
                continue;
            };
            let snapshot = entry.snapshot.lock().clone();
            let Some(info) = entry.durable_info.lock().clone() else {
                self.settle_unrevivable_job(
                    &job,
                    "durable child identity is unavailable for revival",
                );
                continue;
            };
            let session_path = match info
                .session_path
                .as_ref()
                .map(|path| durable.child_file_authority(path).map(|_| path.clone()))
                .transpose()
            {
                Ok(path) => path,
                Err(error) => {
                    self.settle_unrevivable_job(
                        &job,
                        &format!("durable child transcript is unavailable for revival: {error:#}"),
                    );
                    continue;
                }
            };
            let request = match self.rebuild_revival_request(
                &job.agent_id,
                &snapshot,
                &info,
                session_path.as_ref(),
            ) {
                Ok(Some(request)) => request,
                Ok(None) => {
                    self.settle_unrevivable_job(
                        &job,
                        "durable child definition is no longer trusted or reconstructable for revival",
                    );
                    continue;
                }
                Err(error) => {
                    self.settle_unrevivable_job(
                        &job,
                        &format!("reconstructing durable child revival request: {error:#}"),
                    );
                    continue;
                }
            };
            if request.definition.is_persona()
                && self
                    .inner
                    .persona_lifecycle_blocks
                    .lock()
                    .contains(&request.definition.name)
            {
                self.settle_unrevivable_job(
                    &job,
                    &format!(
                        "persona {:?} has a destructive operation in progress",
                        request.definition.name
                    ),
                );
                continue;
            }
            let Some(cancel) = self.inner.jobs.cancellation_token(&job.id) else {
                self.settle_unrevivable_job(
                    &job,
                    "durable child cancellation token is unavailable for revival",
                );
                continue;
            };
            self.inner
                .active
                .lock()
                .insert(job.agent_id.clone(), cancel.clone());
            self.spawn_revival_runner(
                job.agent_id.clone(),
                request,
                session_path,
                info.request.assignment.clone(),
                cancel,
                job.id,
            );
        }
    }

    /// Attempt to revive a parked or idle durable child. The caller holds
    /// `durable_mutation`, making the {Parked,Idle}→Queued claim and
    /// persistence one serialized transaction against sends and rebind
    /// commit. Idle children are revived immediately on a committed message
    /// (the idle→park TTL is no longer a forced delivery boundary); Main is
    /// never claimed because its registry entry carries no `durable_info`.
    fn maybe_revive(&self, target: &str) -> Result<Option<DeliveryOutcome>> {
        if !self.inner.durable_bound.load(Ordering::Acquire) {
            return Ok(None);
        }
        if self.inner.rebind_reserved.load(Ordering::Acquire) {
            bail!("durable orchestration rebind is in progress");
        }
        let durable = self
            .inner
            .durable
            .lock()
            .clone()
            .ok_or_else(|| anyhow!("durable orchestration binding is unavailable"))?;
        let entry = match self.inner.registry.get(self.group_id(), target) {
            Some(entry) => entry,
            None => return Ok(None),
        };
        let snapshot = entry.snapshot.lock().clone();
        if !matches!(snapshot.status, AgentStatus::Parked | AgentStatus::Idle) {
            return Ok(None);
        }
        let previous_status = snapshot.status;
        let info = match entry.durable_info.lock().clone() {
            Some(info) => info,
            None => return Ok(None),
        };
        let session_path = info
            .session_path
            .as_ref()
            .map(|path| durable.child_file_authority(path).map(|_| path.clone()))
            .transpose()?;
        let request =
            match self.rebuild_revival_request(target, &snapshot, &info, session_path.as_ref())? {
                Some(request) => request,
                None => return Ok(None),
            };
        if request.definition.is_persona()
            && self
                .inner
                .persona_lifecycle_blocks
                .lock()
                .contains(&request.definition.name)
        {
            bail!(
                "persona {:?} has a destructive operation in progress",
                request.definition.name
            );
        }
        if !self.inner.registry.compare_status(
            self.group_id(),
            target,
            previous_status,
            AgentStatus::Queued,
        )? {
            return Ok(None);
        }
        // The claim owns the lifecycle now: disarm the idle→park timer so it
        // cannot race the revival start.
        self.cancel_park_timer(target);

        let job_id = Uuid::now_v7().to_string();
        let cancel = CancellationToken::new();
        let created_at = now_millis();
        let job_snapshot = JobSnapshot {
            id: job_id.clone(),
            agent_id: target.to_owned(),
            agent: request.definition.name.clone(),
            parent_id: info.request.parent_id.clone(),
            description: Some(presentation_text(&info.request.assignment, TASK_DESCRIPTION_MAX_CHARS)),
            todo_task_id: None,
            workflow_id: None,
            workflow_generation: None,
            status: JobStatus::Queued,
            created_at,
            started_at: None,
            finished_at: None,
            result: None,
            soft_budget_exhausted: false,
        };
        if let Err(error) = self.inner.jobs.insert(job_snapshot.clone(), cancel.clone()) {
            let _ = self.inner.registry.compare_status(
                self.group_id(),
                target,
                AgentStatus::Queued,
                previous_status,
            );
            return Err(error);
        }
        self.inner
            .active
            .lock()
            .insert(target.to_owned(), cancel.clone());
        if let Err(error) = self.persist_state() {
            self.inner.jobs.remove(&job_id);
            self.inner.active.lock().remove(target);
            let _ = self.inner.registry.compare_status(
                self.group_id(),
                target,
                AgentStatus::Queued,
                previous_status,
            );
            if let Err(restore_error) = self.persist_state() {
                return Err(anyhow!(
                    "persisting durable child revival claim failed: {error:#}; restoring durable queued mailbox failed: {restore_error:#}"
                ));
            }
            return Err(error).context("persisting durable child revival claim");
        }
        if let Some(agent) = self.agent_snapshot(target) {
            self.publish_agent(agent);
        }
        self.publish_job(job_snapshot);

        self.spawn_revival_runner(
            target.to_owned(),
            request,
            session_path,
            info.request.assignment.clone(),
            cancel,
            job_id,
        );
        Ok(Some(DeliveryOutcome::Revived))
    }

    /// Run a revived child session by reopening the exact persisted JSONL,
    /// draining its durable mailbox, and continuing with a new turn.
    async fn run_revival(
        &self,
        agent_id: String,
        request: ChildSessionRequest,
        session_path: Option<PathBuf>,
        assignment: String,
        cancel: CancellationToken,
        job_id: &str,
    ) -> TaskResult {
        let _active_guard = ActiveChildGuard {
            inner: self.inner.clone(),
            id: agent_id.clone(),
            group_id: self.group_id().to_owned(),
        };
        let definition = request.definition.clone();
        let durable = { self.inner.durable.lock().clone() };
        let durable = match durable {
            Some(durable) => durable,
            None => {
                return self.failed_result(
                    &TaskItem {
                        index: 0,
                        id: agent_id.clone(),
                        agent: request.definition.name.clone(),
                        assignment: assignment.clone(),
                        todo_task_id: None,
                        ..Default::default()
                    },
                    job_id,
                    AgentStatus::Idle,
                    "durable orchestration binding is unavailable",
                );
            }
        };
        let artifact_ref = format!("agent://{agent_id}");
        let history_ref = history_reference(&definition, &agent_id);
        let artifact_uri = format!("artifact://{agent_id}");
        let artifact_path = self
            .inner
            .config
            .artifact_dir
            .join(format!("{agent_id}-{job_id}.md"));
        let history_path = self
            .inner
            .config
            .artifact_dir
            .join(format!("{agent_id}-{job_id}.history.json"));

        let local_permit = tokio::select! {
            permit = self.inner.semaphore.clone().acquire_owned() => match permit {
                Ok(permit) => permit,
                Err(_) => return self.failed_result(
                    &TaskItem { index: 0, id: agent_id.clone(), agent: request.definition.name.clone(), assignment: assignment.clone(), todo_task_id: None, ..Default::default() },
                    job_id,
                    AgentStatus::Aborted,
                    "orchestration semaphore closed",
                ),
            },
            () = cancel.cancelled() => return self.failed_result(
                &TaskItem { index: 0, id: agent_id.clone(), agent: request.definition.name.clone(), assignment: assignment.clone(), todo_task_id: None, ..Default::default() },
                job_id,
                AgentStatus::Aborted,
                "task cancelled before start",
            ),
        };
        let durable_mutation = self.inner.durable_mutation.lock();
        if self.inner.rebind_reserved.load(Ordering::Acquire) {
            drop(durable_mutation);
            return self.failed_result(
                &TaskItem {
                    index: 0,
                    id: agent_id.clone(),
                    agent: request.definition.name.clone(),
                    assignment: assignment.clone(),
                    todo_task_id: None,
                    ..Default::default()
                },
                job_id,
                AgentStatus::Idle,
                "durable orchestration rebind is in progress",
            );
        }
        self.inner
            .registry
            .set_status(self.group_id(), &agent_id, AgentStatus::Running)
            .ok();
        let running_agent = self.agent_snapshot(&agent_id);
        let running_job = self.inner.jobs.mark_running(job_id, now_millis());
        if let Err(error) = self.persist_state() {
            let error = format!("persisting revived child running state: {error:#}");
            drop(durable_mutation);
            return self.failed_result(
                &TaskItem {
                    index: 0,
                    id: agent_id.clone(),
                    agent: request.definition.name.clone(),
                    assignment: assignment.clone(),
                    todo_task_id: None,
                    ..Default::default()
                },
                job_id,
                AgentStatus::Idle,
                &error,
            );
        }
        drop(durable_mutation);
        if let Some(agent) = running_agent {
            self.publish_agent(agent);
        }
        if let Some(job) = running_job {
            self.publish_job(job);
        }
        let agent_type = request.definition.name.clone();
        // The factory takes `request` by value; the run loop still needs the
        // delivery state (and the agent name below) after the child session is
        // built, so both are captured before the move.
        let yield_state = request.yield_state.clone();
        let output_schema = request.output_schema.clone();
        let schema_mode = request.schema_mode.clone();
        let continuation_recipient = request.child_id.clone();
        let persona_root = definition.persona_root();
        let mut canonical_session_path = session_path.clone();
        let child = match (self.inner.factory)(request).await {
            Ok(session) => {
                let generation_capture = {
                    let capture = self.inner.generation_capture.lock().clone();
                    match capture {
                        Some(capture) => capture,
                        None => {
                            return self.failed_result(
                                &TaskItem {
                                    index: 0,
                                    id: agent_id.clone(),
                                    agent: agent_type.clone(),
                                    assignment: assignment.clone(),
                                    todo_task_id: None,
                                    ..Default::default()
                                },
                                job_id,
                                AgentStatus::Idle,
                                "durable orchestration generation is unavailable",
                            );
                        }
                    }
                };
                if session.resource_manager().is_some() {
                    return self.failed_result(
                        &TaskItem {
                            index: 0,
                            id: agent_id.clone(),
                            agent: agent_type.clone(),
                            assignment: assignment.clone(),
                            todo_task_id: None,
                            ..Default::default()
                        },
                        job_id,
                        AgentStatus::Idle,
                        "child session factory returned a session with foreign resource authority",
                    );
                }
                session.bind_generation_capture(generation_capture);
                let recording = match session_path.as_deref() {
                    Some(path) => {
                        if let Err(error) = persist_interrupted_tool_results(path, &durable) {
                            return self.failed_result(
                                &TaskItem {
                                    index: 0,
                                    id: agent_id.clone(),
                                    agent: agent_type.clone(),
                                    assignment: assignment.clone(),
                                    todo_task_id: None,
                                    ..Default::default()
                                },
                                job_id,
                                AgentStatus::Idle,
                                &format!("reconciling interrupted child tool calls: {error:#}"),
                            );
                        }
                        session.resume_durable_child_recording(path).await
                    }
                    None => match self.workflow_scope() {
                        Some(_) => session.start_durable_child_recording_with_kind(
                            crate::SessionKind::WorkflowWorker,
                            durable.child_root(),
                            durable.parent_session_path(),
                        ),
                        None => session.start_durable_child_recording(
                            durable.child_root(),
                            durable.parent_session_path(),
                        ),
                    },
                };
                if let Err(error) = recording {
                    return self.failed_result(
                        &TaskItem {
                            index: 0,
                            id: agent_id.clone(),
                            agent: agent_type.clone(),
                            assignment: assignment.clone(),
                            todo_task_id: None,
                            ..Default::default()
                        },
                        job_id,
                        AgentStatus::Idle,
                        &format!("opening durable child transcript: {error:#}"),
                    );
                }
                if session_path.is_none() {
                    let Some((_, new_path)) = session.recorder_info() else {
                        return self.failed_result(
                            &TaskItem {
                                index: 0,
                                id: agent_id.clone(),
                                agent: agent_type.clone(),
                                assignment: assignment.clone(),
                                todo_task_id: None,
                                ..Default::default()
                            },
                            job_id,
                            AgentStatus::Idle,
                            "durable child recorder is unavailable",
                        );
                    };
                    let new_path = match durable.child_file_authority(&new_path) {
                        Ok(_) => new_path,
                        Err(error) => {
                            return self.failed_result(
                                &TaskItem {
                                    index: 0,
                                    id: agent_id.clone(),
                                    agent: agent_type.clone(),
                                    assignment: assignment.clone(),
                                    todo_task_id: None,
                                    ..Default::default()
                                },
                                job_id,
                                AgentStatus::Idle,
                                &error.to_string(),
                            );
                        }
                    };
                    canonical_session_path = Some(new_path.clone());
                    let durable_mutation = self.inner.durable_mutation.lock();
                    if self.inner.rebind_reserved.load(Ordering::Acquire) {
                        drop(durable_mutation);
                        return self.failed_result(
                            &TaskItem {
                                index: 0,
                                id: agent_id.clone(),
                                agent: agent_type.clone(),
                                assignment: assignment.clone(),
                                todo_task_id: None,
                                ..Default::default()
                            },
                            job_id,
                            AgentStatus::Idle,
                            "durable orchestration rebind is in progress",
                        );
                    }
                    if let Some(entry) = self.inner.registry.get(self.group_id(), &agent_id)
                        && let Some(info) = entry.durable_info.lock().as_mut()
                    {
                        info.session_path = Some(new_path);
                    }
                    if let Err(error) = self.persist_state() {
                        let error = format!("persisting revived child transcript path: {error:#}");
                        drop(durable_mutation);
                        return self.failed_result(
                            &TaskItem {
                                index: 0,
                                id: agent_id.clone(),
                                agent: agent_type.clone(),
                                assignment: assignment.clone(),
                                todo_task_id: None,
                                ..Default::default()
                            },
                            job_id,
                            AgentStatus::Idle,
                            &error,
                        );
                    }
                    drop(durable_mutation);
                }
                if definition.is_persona() {
                    let continuity = match self.load_persona_continuity(&definition.name) {
                        Ok(messages) => messages,
                        Err(error) => {
                            return self.failed_result(
                                &TaskItem {
                                    index: 0,
                                    id: agent_id.clone(),
                                    agent: agent_type.clone(),
                                    assignment: assignment.clone(),
                                    todo_task_id: None,
                                    ..Default::default()
                                },
                                job_id,
                                AgentStatus::Idle,
                                &format!("loading revived persona continuity: {error:#}"),
                            );
                        }
                    };
                    let merged = merge_persona_continuity(continuity, session.history());
                    if let Err(error) = session.load_history(merged).await {
                        return self.failed_result(
                            &TaskItem {
                                index: 0,
                                id: agent_id.clone(),
                                agent: agent_type.clone(),
                                assignment: assignment.clone(),
                                todo_task_id: None,
                                ..Default::default()
                            },
                            job_id,
                            AgentStatus::Idle,
                            &format!("installing revived persona continuity: {error:#}"),
                        );
                    }
                }
                ChildSession::new(session)
            }
            Err(error) => {
                return self.failed_result(
                    &TaskItem {
                        index: 0,
                        id: agent_id.clone(),
                        agent: agent_type.clone(),
                        assignment: assignment.clone(),
                        todo_task_id: None,
                        ..Default::default()
                    },
                    job_id,
                    AgentStatus::Idle,
                    &error.to_string(),
                );
            }
        };
        let soft_budget_state = JobSoftBudgetState::default();
        let contract_state = JobContractState::default();
        let stop_hook = self.compose_turn_stop_hooks(
            &definition,
            &soft_budget_state,
            &contract_state,
            &yield_state,
        );
        child.set_should_stop_after_turn(stop_hook);

        // Register active delivery and drain the durable mailbox atomically
        // against concurrent durable sends.
        let (delivery_tx, mut delivery_rx) = tokio::sync::mpsc::unbounded_channel();
        let pre_run = {
            let durable_mutation = self.inner.durable_mutation.lock();
            if self.inner.rebind_reserved.load(Ordering::Acquire) {
                drop(durable_mutation);
                return self.failed_result(
                    &TaskItem {
                        index: 0,
                        id: agent_id.clone(),
                        agent: agent_type.clone(),
                        assignment: assignment.clone(),
                        todo_task_id: None,
                        ..Default::default()
                    },
                    job_id,
                    AgentStatus::Idle,
                    "durable orchestration rebind is in progress",
                );
            }
            match self.inner.registry.register_active_delivery(
                self.group_id(),
                &agent_id,
                delivery_tx,
                cancel.clone(),
            ) {
                Ok(messages) => messages,
                Err(error) => {
                    drop(durable_mutation);
                    return self.failed_result(
                        &TaskItem {
                            index: 0,
                            id: agent_id.clone(),
                            agent: agent_type.clone(),
                            assignment: assignment.clone(),
                            todo_task_id: None,
                            ..Default::default()
                        },
                        job_id,
                        AgentStatus::Idle,
                        &error.to_string(),
                    );
                }
            }
        };

        // Steer pre-run messages, acknowledging each only after delivery.
        for message in &pre_run {
            if let Err(error) = child.steer(message).await {
                return self.failed_result(
                    &TaskItem {
                        index: 0,
                        id: agent_id.clone(),
                        agent: agent_type.clone(),
                        assignment: assignment.clone(),
                        todo_task_id: None,
                        ..Default::default()
                    },
                    job_id,
                    AgentStatus::Idle,
                    &format!("failed to deliver queued message {}: {error:#}", message.id),
                );
            }
            if let Err(error) = self.acknowledge_delivery(&agent_id, &message.id) {
                return self.failed_result(
                    &TaskItem {
                        index: 0,
                        id: agent_id.clone(),
                        agent: agent_type.clone(),
                        assignment: assignment.clone(),
                        todo_task_id: None,
                        ..Default::default()
                    },
                    job_id,
                    AgentStatus::Idle,
                    &format!("acknowledging queued message {}: {error:#}", message.id),
                );
            }
        }

        let mut run = Box::pin(async {
            if session_path.is_some() && (child.can_continue_without_input() || !pre_run.is_empty())
            {
                child.continue_run().await
            } else if session_path.is_some() {
                child
                    .steer(&MailboxMessage {
                        id: Uuid::now_v7().to_string(),
                        from: self.inner.config.main_agent_id.clone(),
                        to: continuation_recipient,
                        body: "Continue the interrupted assignment from the recorded transcript."
                            .to_owned(),
                        timestamp: now_millis(),
                        reply_to: None,
                    })
                    .await?;
                child.continue_run().await
            } else {
                child.run(&assignment).await
            }
        });
        let run_deadline = definition
            .timeout_secs
            .map(|seconds| tokio::time::Instant::now() + Duration::from_secs(seconds));

        let outcome = loop {
            tokio::select! {
                result = &mut run => break result.map_err(|error| error.to_string()),
                message = delivery_rx.recv() => {
                    match message {
                        Some(message) => {
                            if let Err(error) = child.steer(&message).await {
                                break Err(format!("failed to deliver message {}: {error:#}", message.id));
                            }
                            if let Err(error) = self.acknowledge_delivery(&agent_id, &message.id) {
                                break Err(format!("acknowledging delivered message {}: {error:#}", message.id));
                            }
                        }
                        None => break Err("active child delivery bridge closed".to_owned()),
                    }
                }
                () = cancel.cancelled() => {
                    {
                        let _durable_mutation = self.inner.durable_mutation.lock();
                        self.inner.registry.unregister_active_delivery(self.group_id(), &agent_id);
                    }
                    let drain = async {
                        child.abort().await;
                        let _ = (&mut run).await;
                    };
                    break if tokio::time::timeout(CHILD_ABORT_GRACE, drain).await.is_err() {
                        Err(format!(
                            "task cancellation timed out after {}s",
                            CHILD_ABORT_GRACE.as_secs()
                        ))
                    } else {
                        Err("task cancelled".to_owned())
                    };
                }
                () = async {
                    tokio::time::sleep_until(run_deadline.expect("deadline guard")).await;
                }, if run_deadline.is_some() => {
                    {
                        let _durable_mutation = self.inner.durable_mutation.lock();
                        self.inner.registry.unregister_active_delivery(self.group_id(), &agent_id);
                    }
                    let drain = async {
                        child.abort().await;
                        let _ = (&mut run).await;
                    };
                    let timeout_secs = definition.timeout_secs.unwrap_or_default();
                    break if tokio::time::timeout(CHILD_ABORT_GRACE, drain).await.is_err() {
                        Err(format!(
                            "role `{}` exceeded its timeout contract of {timeout_secs}s and did not settle after abort",
                            definition.name,
                        ))
                    } else {
                        Err(format!(
                            "role `{}` exceeded its timeout contract of {timeout_secs}s",
                            definition.name,
                        ))
                    };
                }
            }
        };

        {
            let _durable_mutation = self.inner.durable_mutation.lock();
            self.inner
                .registry
                .unregister_active_delivery(self.group_id(), &agent_id);
        }
        // A message steered in the final-turn window may still sit in the
        // session's steering queue (the bridge acknowledged it at steer time,
        // before the run settled). Return it to the mailbox so the next run
        // or revival delivers it exactly once. A failed persist of the
        // restore must surface on the run result, never be swallowed.
        let mut restore_errors = Vec::new();
        let (mailbox_messages, unrestorable) = child.drain_unprocessed_orchestration_steering().await;
        for message in mailbox_messages {
            if let Err(error) = self.restore_unprocessed_delivery(&agent_id, message.clone()) {
                restore_errors.push(format!(
                    "restoring unprocessed delivery {}: {error:#}",
                    message.id
                ));
            }
        }
        restore_errors.extend(unrestorable);
        drop(local_permit);

        let status = if cancel.is_cancelled() {
            AgentStatus::Aborted
        } else {
            AgentStatus::Idle
        };
        let soft_budget_exhausted = soft_budget_state.triggered.load(Ordering::Acquire);
        let execution_ceiling_reached = soft_budget_exhausted
            || contract_state.max_turns_triggered.load(Ordering::Acquire)
            || contract_state
                .max_tool_calls_triggered
                .load(Ordering::Acquire);
        let (output, error, usage) =
            self.settle_child_outcome(outcome, &child, &yield_state, execution_ceiling_reached);
        let structured_output = super::tools::validate_delivered_output(
            &output,
            output_schema.as_ref(),
            schema_mode.as_deref(),
            error.as_deref(),
        );
        let artifact_body = if output.is_empty() {
            error.as_deref().unwrap_or("(no output)").to_owned()
        } else {
            output.clone()
        };
        let artifact_error = write_revival_artifact(
            &self.inner.storage.artifacts,
            &artifact_path,
            artifact_body.as_bytes(),
        )
        .with_context(|| format!("writing subagent artifact {}", artifact_path.display()))
        .err();
        let artifact_written = artifact_error.is_none();
        let history_error = serde_json::to_vec_pretty(&child.history())
            .context("serializing subagent history")
            .and_then(|bytes| {
                write_revival_artifact(&self.inner.storage.artifacts, &history_path, &bytes)
                    .with_context(|| format!("writing subagent history {}", history_path.display()))
            })
            .err();
        let history_written = history_error.is_none();
        let mut final_error = match artifact_error.or(history_error) {
            Some(write_error) => Some(match error.as_deref() {
                Some(error) => format!("{error}; {write_error}"),
                None => write_error.to_string(),
            }),
            None => error,
        };
        for restore_error in restore_errors {
            final_error = Some(match final_error {
                Some(error) => format!("{error}; {restore_error}"),
                None => restore_error,
            });
        }
        if definition.is_persona()
            && let Some(source) = canonical_session_path.as_deref()
            && let Err(archive_error) =
                self.archive_persona_session(&definition.name, &agent_id, &durable, source, true)
        {
            let message = format!("archiving revived persona transcript: {archive_error:#}");
            final_error = Some(match final_error {
                Some(error) => format!("{error}; {message}"),
                None => message,
            });
        }
        if let Err(persist_error) = self.finish_agent(
            &agent_id,
            status,
            artifact_written.then_some(artifact_path),
            history_written.then_some(history_path),
        ) {
            let message = persist_error.to_string();
            final_error = Some(match final_error {
                Some(error) => format!("{error}; {message}"),
                None => message,
            });
        }
        if let Some(reason) = self.contract_stop_reason(&definition, &contract_state) {
            final_error = Some(match final_error {
                Some(error) => format!("{reason}; {error}"),
                None => reason,
            });
        }
        // Strict schema mode surfaces a delivered payload that fails its
        // outputSchema contract as a job error (the child still settled).
        if schema_mode.as_deref() == Some("strict")
            && let Some(validation) = structured_output.as_ref()
            && !validation.valid
            && let Some(validation_error) = validation.error.as_deref()
        {
            final_error = Some(match final_error {
                Some(error) => format!("{validation_error}; {error}"),
                None => validation_error.to_owned(),
            });
        }
        TaskResult {
            index: 0,
            id: agent_id,
            agent: agent_type,
            status,
            output,
            error: final_error,
            usage,
            soft_budget_exhausted,
            structured_output,
            artifact_ref,
            history_ref,
            artifact_uri,
        }
    }

    /// Recover an existing sidecar. Missing state remains an error so callers
    /// that require an existing parent can fail closed.
    pub fn recover(&self) -> Result<()> {
        let _spawn_guard = self.inner.jobs.lock_spawns();
        if self.inner.rebind_reserved.load(Ordering::Acquire) {
            bail!("durable orchestration rebind is in progress");
        }
        {
            let _durable_mutation = self.inner.durable_mutation.lock();
            self.ensure_recovery_idle()?;
            let durable = self.inner.durable.lock().clone().ok_or_else(|| {
                anyhow!("orchestration runtime is not bound to a durable parent session")
            })?;
            let state = durable.load()?;
            self.install_recovered_state_locked(&durable, state)?;
        }
        self.revive_recovered_jobs();
        Ok(())
    }

    /// Recover existing state before any write, or initialize a new sidecar
    /// when the bound parent has no durable orchestration state yet.
    pub fn recover_or_initialize(&self) -> Result<()> {
        let _spawn_guard = self.inner.jobs.lock_spawns();
        if self.inner.rebind_reserved.load(Ordering::Acquire) {
            bail!("durable orchestration rebind is in progress");
        }
        {
            let _durable_mutation = self.inner.durable_mutation.lock();
            self.ensure_recovery_idle()?;
            let durable = self.inner.durable.lock().clone().ok_or_else(|| {
                anyhow!("orchestration runtime is not bound to a durable parent session")
            })?;
            match durable.load_optional()? {
                Some(state) => self.install_recovered_state_locked(&durable, state)?,
                None => self.persist_state()?,
            }
        }
        self.revive_recovered_jobs();
        Ok(())
    }
    fn prepared_group_entries(
        &self,
        mut children: HashMap<String, Arc<AgentEntry>>,
    ) -> HashMap<String, Arc<AgentEntry>> {
        if let Some(main) = self
            .inner
            .registry
            .get(self.group_id(), self.main_agent_id())
        {
            children.insert(self.main_agent_id().to_owned(), main);
        }
        children
    }

    fn prepare_recovered_state(
        &self,
        durable: &super::persistence::DurableRuntime,
        generation: &Arc<crate::application::generation::ApplicationGeneration>,
        state: super::persistence::DurableState,
    ) -> Result<PreparedRecoveredState> {
        self.adopt_durable_group_id(&state.group_id)?;
        let captured = crate::DurablePolicyState::capture(generation)?;
        if captured != state.durable_policy
            && !crate::sandbox::durable_policy_same_authority(&state.durable_policy, &captured)
            && !crate::sandbox::durable_policy_is_protected_subset_upgrade(
                &state.durable_policy,
                &captured,
            )
        {
            return Err(anyhow::Error::new(
                crate::DurableResumeError::PolicyMismatch,
            ));
        }
        let now = now_millis();
        let mut entries = HashMap::with_capacity(state.agents.len());
        let main_mailbox = state.main_mailbox;
        let delivered_messages = state.delivered_messages;
        let mut agents = Vec::with_capacity(state.agents.len());
        for agent in state.agents {
            let session_path = agent
                .session_path
                .as_deref()
                .map(|path| {
                    durable
                        .child_file_authority(Path::new(path))
                        .map(|_| PathBuf::from(path))
                })
                .transpose()?;
            let definition = agent.definition;
            let request = agent.request;
            let mailbox = agent.mailbox;
            let was_unsettled = matches!(
                agent.snapshot.status,
                AgentStatus::Queued | AgentStatus::Running
            );
            let mut snapshot = AgentSnapshot {
                status: super::persistence::recovery_status(agent.snapshot.status),
                last_activity: now,
                ..agent.snapshot
            };
            if was_unsettled || snapshot.history_ref.is_some() {
                snapshot.history_ref = Some(persisted_history_reference(&definition, &snapshot.id));
            }
            agents.push(super::persistence::PersistedAgent {
                snapshot: snapshot.clone(),
                definition: definition.clone(),
                request: request.clone(),
                session_path: session_path
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned()),
                mailbox: mailbox.clone(),
            });
            entries.insert(
                snapshot.id.clone(),
                Arc::new(AgentEntry {
                    snapshot: Mutex::new(snapshot),
                    mailbox: Mutex::new(mailbox.into()),
                    mailbox_capacity: self.inner.config.mailbox_capacity,
                    message_ready: Notify::new(),
                    active_delivery: Mutex::new(None),
                    waiters: Mutex::new(Vec::new()),
                    waiter_seq: AtomicU64::new(0),
                    cancellation: Mutex::new(None),
                    idle_park_token: Mutex::new(None),
                    artifact_path: Mutex::new(None),
                    history_path: Mutex::new(None),
                    durable_info: Mutex::new(Some(DurableAgentInfo {
                        definition,
                        request,
                        session_path,
                    })),
                }),
            );
        }
        let persona_names = agents
            .iter()
            .filter(|agent| agent.definition.kind == super::AgentDefinitionKind::Persona)
            .map(|agent| (agent.snapshot.id.clone(), agent.definition.name.clone()))
            .collect::<HashMap<_, _>>();
        let job_snapshots = state
            .jobs
            .into_iter()
            .map(|job| {
                let persona = persona_names.get(&job.agent_id).map(String::as_str);
                super::persistence::recovery_job_with_persona(job, now, persona)
            })
            .collect::<Vec<_>>();
        let jobs = self.inner.jobs.prepare_replacement(job_snapshots);
        Ok(PreparedRecoveredState {
            main_mailbox,
            delivered_messages,
            group_entries: self.prepared_group_entries(entries),
            agents,
            jobs,
        })
    }
    fn install_prepared_state(&self, mut state: PreparedRecoveredState) {
        if let Some(main) = state.group_entries.get(self.main_agent_id()) {
            *main.mailbox.lock() = std::mem::take(&mut state.main_mailbox).into();
        }
        *self.inner.delivered_messages.lock() = state.delivered_messages.into();
        self.inner
            .registry
            .install_prepared_group(self.group_id(), state.group_entries);
        self.inner.jobs.install_replacement(state.jobs);
    }
    fn ensure_recovery_idle(&self) -> Result<()> {
        if !self.inner.active.lock().is_empty()
            || self
                .inner
                .jobs
                .snapshots(None)
                .iter()
                .any(|job| !job.status.is_settled())
        {
            bail!("cannot recover durable orchestration while child jobs are active");
        }
        Ok(())
    }

    fn install_recovered_state_locked(
        &self,
        durable: &super::persistence::DurableRuntime,
        state: super::persistence::DurableState,
    ) -> Result<()> {
        self.ensure_recovery_idle()?;
        let generation = self.capture_generation()?;
        let durable_policy = crate::DurablePolicyState::capture(&generation)?;
        let recovered = self.prepare_recovered_state(durable, &generation, state)?;
        durable.persist(&super::persistence::build_state(
            self.group_id(),
            durable.parent_session_id(),
            durable.parent_session_path(),
            durable_policy,
            recovered.main_mailbox.clone(),
            recovered.delivered_messages.clone(),
            recovered.agents.clone(),
            recovered.jobs.snapshots().to_vec(),
        ))?;
        self.install_prepared_state(recovered);
        Ok(())
    }

    pub fn set_workflow_scope(&self, scope: WorkflowRuntimeScope) -> Result<()> {
        if scope.workflow_id.trim().is_empty() {
            bail!("workflow id must not be empty");
        }
        let _spawn_guard = self.inner.jobs.lock_spawns();
        let _durable_mutation = self.inner.durable_mutation.lock();
        if self.inner.rebind_reserved.load(Ordering::Acquire) {
            bail!("durable orchestration rebind is in progress");
        }
        if self
            .inner
            .jobs
            .snapshots(None)
            .iter()
            .any(|job| !job.status.is_settled())
        {
            bail!("cannot change workflow scope while orchestration jobs are active");
        }
        *self.inner.workflow_scope.lock() = Some(scope);
        Ok(())
    }

    #[must_use]
    pub fn workflow_scope(&self) -> Option<WorkflowRuntimeScope> {
        self.inner.workflow_scope.lock().clone()
    }

    pub fn set_global_concurrency_gate(&self, gate: OrchestrationConcurrencyGate) -> Result<()> {
        let _spawn_guard = self.inner.jobs.lock_spawns();
        let _durable_mutation = self.inner.durable_mutation.lock();
        if self.inner.rebind_reserved.load(Ordering::Acquire) {
            bail!("durable orchestration rebind is in progress");
        }
        if self
            .inner
            .jobs
            .snapshots(None)
            .iter()
            .any(|job| !job.status.is_settled())
        {
            bail!("cannot change global concurrency gate while orchestration jobs are active");
        }
        let mut current = self.inner.global_concurrency.lock();
        if current
            .as_ref()
            .is_some_and(|configured| configured != &gate)
        {
            bail!("orchestration runtime already uses a different global concurrency gate");
        }
        *current = Some(gate);
        Ok(())
    }

    #[must_use]
    pub fn global_concurrency_gate(&self) -> Option<OrchestrationConcurrencyGate> {
        self.inner.global_concurrency.lock().clone()
    }

    #[must_use]
    pub fn workflow_jobs(&self, workflow_id: &str, generation: u64) -> Vec<WorkflowJobSnapshot> {
        self.jobs(None)
            .into_iter()
            .filter(|job| {
                job.workflow_id.as_deref() == Some(workflow_id)
                    && job.workflow_generation == Some(generation)
            })
            .map(|job| WorkflowJobSnapshot {
                workflow_id: workflow_id.to_owned(),
                generation,
                todo_task_id: job.todo_task_id.clone(),
                job,
            })
            .collect()
    }

    #[must_use]
    pub fn active_child_count(&self) -> usize {
        self.inner.active.lock().len()
    }

    pub async fn wait_until_idle(&self) {
        loop {
            let notified = self.inner.active_changed.notified();
            if self.inner.active.lock().is_empty() {
                break;
            }
            notified.await;
        }
    }

    fn publish_job(&self, job: JobSnapshot) {
        let _ = self.inner.events.send(OrchestrationEvent::JobUpdated {
            group_id: self.group_id().to_owned(),
            job: presentation_job_snapshot(job),
        });
    }

    fn publish_agent(&self, agent: AgentSnapshot) {
        let _ = self.inner.events.send(OrchestrationEvent::AgentUpdated {
            group_id: self.group_id().to_owned(),
            agent: presentation_agent_snapshot(agent),
        });
    }

    fn publish_message_delivered(&self, message: MailboxMessage, waiter_claimed: bool) {
        let _ = self
            .inner
            .events
            .send(OrchestrationEvent::MessageDelivered {
                group_id: self.group_id().to_owned(),
                message,
                waiter_claimed,
            });
    }

    /// Append one durably delivered group message to the bounded delivered
    /// log (newest-last). The log is the workflow page's Recent IRC source:
    /// it survives mailbox consumption, so subagent ⇄ subagent messages stay
    /// visible after the recipient reads them.
    fn record_delivered_message(&self, message: MailboxMessage) {
        let mut log = self.inner.delivered_messages.lock();
        log.retain(|delivered| delivered.id != message.id);
        log.push_back(message);
        if log.len() > DELIVERED_MESSAGE_LOG_CAP {
            let excess = log.len() - DELIVERED_MESSAGE_LOG_CAP;
            log.drain(..excess);
        }
    }
    fn remove_delivered_message(&self, message_id: &str) {
        self.inner
            .delivered_messages
            .lock()
            .retain(|message| message.id != message_id);
    }

    /// Bounded, newest-last log of every durably delivered group message,
    /// independent of mailbox consumption.
    #[must_use]
    pub fn delivered_messages(&self) -> Vec<MailboxMessage> {
        self.inner
            .delivered_messages
            .lock()
            .iter()
            .cloned()
            .collect()
    }

    /// Human-facing label for an agent id; falls back to the stable id.
    #[must_use]
    pub fn resolve_agent_display_name(&self, agent_id: &str) -> String {
        self.agent_snapshot(agent_id)
            .map(|agent| {
                let name = agent.display_name.trim();
                if name.is_empty() {
                    agent.id
                } else {
                    agent.display_name
                }
            })
            .unwrap_or_else(|| agent_id.to_owned())
    }

    fn agent_snapshot(&self, id: &str) -> Option<AgentSnapshot> {
        self.inner
            .registry
            .get(self.group_id(), id)
            .map(|entry| entry.snapshot.lock().clone())
    }
    #[must_use]
    pub(crate) fn runtime_equivalent(&self, other: &Self) -> bool {
        self.inner.config.runtime_equivalent(&other.inner.config)
    }

    #[must_use]
    pub(crate) fn shares_runtime(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    pub(crate) fn fresh_for_parent(&self, parent: &Session) -> Result<Self> {
        let fresh = Self::new(self.inner.config.clone(), self.inner.factory.clone())?;
        *fresh.inner.parent_workspace.write() = Some(parent.workspace_roots().clone());
        Ok(fresh)
    }

    #[must_use]
    pub fn list(&self, caller_id: &str) -> Vec<AgentSnapshot> {
        self.inner.registry.list(self.group_id(), caller_id)
    }

    pub fn send(
        &self,
        from: &str,
        to: &str,
        body: &str,
        reply_to: Option<String>,
    ) -> Vec<DeliveryReceipt> {
        self.send_message_with_id(from, to, body, reply_to, None)
    }
    pub fn send_workflow_message(
        &self,
        workflow_id: &str,
        generation: u64,
        recipient: &str,
        body: &str,
        reply_to: Option<String>,
    ) -> Result<Vec<DeliveryReceipt>> {
        let scope = self
            .workflow_scope()
            .ok_or_else(|| anyhow!("orchestration runtime is not workflow-scoped"))?;
        if scope.workflow_id != workflow_id || scope.generation != generation {
            bail!("workflow runtime identity mismatch");
        }
        if recipient.trim().eq_ignore_ascii_case("all") {
            bail!("workflow hub send does not allow broadcast recipients");
        }
        let receipts =
            self.send_message_with_id(self.main_agent_id(), recipient, body, reply_to, None);
        Ok(receipts)
    }

    fn send_message_with_id(
        &self,
        from: &str,
        to: &str,
        body: &str,
        reply_to: Option<String>,
        message_id: Option<String>,
    ) -> Vec<DeliveryReceipt> {
        let invalid = if body.trim().is_empty() {
            Some("orchestration message body must not be empty".to_owned())
        } else if body.len() > MAX_MESSAGE_BODY_BYTES {
            Some(format!(
                "orchestration message body exceeds maximum of {MAX_MESSAGE_BODY_BYTES} bytes"
            ))
        } else if let Err(error) = validate_agent_id(from) {
            Some(format!("invalid orchestration sender {from:?}: {error:#}"))
        } else if self.agent_snapshot(from).is_none() {
            Some(format!("unknown orchestration sender {from:?}"))
        } else {
            None
        };
        if let Some(error) = invalid {
            return vec![DeliveryReceipt {
                to: to.to_owned(),
                outcome: DeliveryOutcome::Failed,
                requested: None,
                message_id: None,
                error: Some(error),
            }];
        }
        if self.inner.rebind_reserved.load(Ordering::Acquire) {
            return vec![DeliveryReceipt {
                to: to.to_owned(),
                outcome: DeliveryOutcome::Failed,
                requested: None,
                message_id: None,
                error: Some("durable orchestration rebind is in progress".to_owned()),
            }];
        }
        let (targets, requested_alias) = if to == "all" {
            (
                self.inner
                    .registry
                    .list(self.group_id(), from)
                    .into_iter()
                    .filter(|entry| entry.status != AgentStatus::Aborted)
                    .map(|entry| entry.id)
                    .collect::<Vec<_>>(),
                None,
            )
        } else {
            match self.inner.jobs.resolve_agent_id(to) {
                Ok(agent_id) => {
                    let requested = (agent_id != to).then(|| to.to_owned());
                    (vec![agent_id], requested)
                }
                Err(error)
                    if error
                        .to_string()
                        .starts_with("ambiguous orchestration job or agent id") =>
                {
                    return vec![DeliveryReceipt {
                        to: to.to_owned(),
                        outcome: DeliveryOutcome::Failed,
                        requested: None,
                        message_id: None,
                        error: Some(error.to_string()),
                    }];
                }
                Err(_) => (vec![to.to_owned()], None),
            }
        };
        let main_id = self.main_agent_id().to_owned();
        let group_id = self.group_id().to_owned();
        let mut receipts = Vec::with_capacity(targets.len());
        for target in targets {
            let message = MailboxMessage {
                id: message_id
                    .clone()
                    .unwrap_or_else(|| Uuid::now_v7().to_string()),
                from: from.to_owned(),
                to: target.clone(),
                body: body.to_owned(),
                timestamp: now_millis(),
                reply_to: reply_to.clone(),
            };
            if self.inner.rebind_reserved.load(Ordering::Acquire) {
                receipts.push(DeliveryReceipt {
                    to: target,
                    outcome: DeliveryOutcome::Failed,
                    requested: requested_alias.clone(),
                    message_id: Some(message.id.clone()),
                    error: Some("durable orchestration rebind is in progress".to_owned()),
                });
                continue;
            }
            let _durable_mutation = self.inner.durable_mutation.lock();
            if self.inner.rebind_reserved.load(Ordering::Acquire) {
                receipts.push(DeliveryReceipt {
                    to: target,
                    outcome: DeliveryOutcome::Failed,
                    requested: requested_alias.clone(),
                    message_id: Some(message.id.clone()),
                    error: Some("durable orchestration rebind is in progress".to_owned()),
                });
                continue;
            }
            match self
                .inner
                .registry
                .enqueue(&group_id, &target, message.clone())
            {
                Ok(outcome) => {
                    self.record_delivered_message(message.clone());
                    match self.persist_state().with_context(|| {
                        format!("persisting message to orchestration agent {target:?}")
                    }) {
                        Err(error) => {
                            self.inner
                                .registry
                                .remove_message(&group_id, &target, &message.id);
                            self.remove_delivered_message(&message.id);
                            receipts.push(DeliveryReceipt {
                                to: target,
                                outcome: DeliveryOutcome::Failed,
                                requested: requested_alias.clone(),
                                message_id: Some(message.id.clone()),
                                error: Some(format!("{error:#}")),
                            });
                        }
                        Ok(()) => {
                            let mut outcome = outcome;
                            let mut waiter_claimed = false;
                            if matches!(outcome, DeliveryOutcome::Woken) {
                                match self.inner.registry.deliver_committed(
                                    &group_id,
                                    &target,
                                    &message.id,
                                ) {
                                    CommittedDelivery::Delivered => {}
                                    CommittedDelivery::WaiterClaimed => {
                                        waiter_claimed = true;
                                        outcome = DeliveryOutcome::Processed;
                                    }
                                    CommittedDelivery::Retained => {
                                        outcome = DeliveryOutcome::Queued;
                                    }
                                }
                            } else if self
                                .inner
                                .registry
                                .hand_to_waiter(&group_id, &target, &message)
                            {
                                waiter_claimed = true;
                                outcome = DeliveryOutcome::Processed;
                            }
                            match if matches!(outcome, DeliveryOutcome::Queued) {
                                self.maybe_revive(&target)
                            } else {
                                Ok(None)
                            } {
                                Ok(revival) => {
                                    let outcome = revival.unwrap_or(outcome);
                                    let message_id = message.id.clone();
                                    if target == main_id {
                                        self.publish_message_delivered(message, waiter_claimed);
                                    }
                                    receipts.push(DeliveryReceipt {
                                        to: target,
                                        outcome,
                                        requested: requested_alias.clone(),
                                        message_id: Some(message_id),
                                        error: None,
                                    });
                                }
                                Err(error) => {
                                    receipts.push(DeliveryReceipt {
                                        to: target,
                                        outcome: DeliveryOutcome::Queued,
                                        requested: requested_alias.clone(),
                                        message_id: Some(message.id.clone()),
                                        error: Some(error.to_string()),
                                    });
                                }
                            }
                        }
                    }
                }
                Err(error) => receipts.push(DeliveryReceipt {
                    to: target,
                    outcome: DeliveryOutcome::Failed,
                    requested: requested_alias.clone(),
                    message_id: Some(message.id.clone()),
                    error: Some(error.to_string()),
                }),
            }
        }
        receipts
    }

    pub fn inbox_result(&self, agent_id: &str, peek: bool) -> Result<Vec<MailboxMessage>> {
        if self.agent_snapshot(agent_id).is_none() {
            bail!("unknown orchestration agent {agent_id:?}");
        }
        if peek {
            if self.inner.rebind_reserved.load(Ordering::Acquire) {
                bail!("durable orchestration rebind is in progress");
            }
            return Ok(self.inner.registry.inbox(self.group_id(), agent_id, true));
        }
        let _durable_mutation = self.inner.durable_mutation.lock();
        if self.inner.rebind_reserved.load(Ordering::Acquire) {
            bail!("durable orchestration rebind is in progress");
        }
        let messages = self.inner.registry.inbox(self.group_id(), agent_id, false);
        if messages.is_empty() {
            return Ok(messages);
        }
        if let Err(error) = self
            .persist_state()
            .context("persisting orchestration inbox drain")
        {
            self.inner
                .registry
                .restore_mailbox(self.group_id(), agent_id, messages);
            return Err(error);
        }
        Ok(messages)
    }
    fn acknowledge_delivery(&self, agent_id: &str, message_id: &str) -> Result<()> {
        let _durable_mutation = self.inner.durable_mutation.lock();
        let Some(message) =
            self.inner
                .registry
                .remove_message_value(self.group_id(), agent_id, message_id)
        else {
            return Ok(());
        };
        if let Err(error) = self.persist_state().with_context(|| {
            format!("persisting delivered message {message_id:?} for agent {agent_id:?}")
        }) {
            self.inner
                .registry
                .restore_mailbox_message(self.group_id(), agent_id, message);
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn acknowledge_main_delivery(&self, message_id: &str) -> Result<()> {
        let _durable_mutation = self.inner.durable_mutation.lock();
        let main_id = self.main_agent_id();
        let Some(message) =
            self.inner
                .registry
                .remove_message_value(self.group_id(), main_id, message_id)
        else {
            return Ok(());
        };
        if let Err(error) = self
            .persist_state()
            .context("persisting delivered Main message")
        {
            self.inner
                .registry
                .restore_mailbox_message(self.group_id(), main_id, message);
            return Err(error);
        }
        Ok(())
    }

    /// Return a steered-but-unprocessed mailbox message to the recipient's
    /// mailbox, reversing the bridge's acknowledgement. Called when a run
    /// settles while the agent loop still holds the message in its steering
    /// queue (final-turn race), so the next run/revival delivers it exactly
    /// once instead of losing it with the dropped session. The in-memory
    /// restore always stands; a persistence failure is returned so the caller
    /// can surface it (the message was durably committed before the bridge
    /// acknowledged it, so a crash must not lose it silently).
    fn restore_unprocessed_delivery(&self, agent_id: &str, message: MailboxMessage) -> Result<()> {
        let _durable_mutation = self.inner.durable_mutation.lock();
        if self.inner.rebind_reserved.load(Ordering::Acquire) {
            bail!(
                "durable orchestration rebind is in progress; restored delivery persisted by the rebind"
            );
        }
        // Forced restore (dedup + front, never cap-rejected): the message was
        // durably committed and acknowledged away, and the in-flight capacity
        // it occupied is being returned.
        self.inner
            .registry
            .restore_mailbox_message(self.group_id(), agent_id, message);
        self.persist_state()
            .context("persisting restored unprocessed delivery")
    }

    /// Restore a waited/handed-off message whose consumption could not be
    /// persisted, using the forced id-dedup restore (the message was durably
    /// committed before the handoff). Main-target messages are republished
    /// fresh non-claimed event the restored reply would never reach the main
    /// session's model context.
    fn restore_waited_message(&self, agent_id: &str, message: MailboxMessage) {
        self.inner
            .registry
            .restore_mailbox_message(self.group_id(), agent_id, message.clone());
        if message.to == self.main_agent_id() {
            self.publish_message_delivered(message, false);
        }
    }

    pub fn inbox(&self, agent_id: &str, peek: bool) -> Result<Vec<MailboxMessage>> {
        self.inbox_result(agent_id, peek)
    }

    pub(crate) fn canonical_agent_id(&self, identifier: &str) -> Result<String> {
        if self.agent_snapshot(identifier).is_some() {
            return Ok(identifier.to_owned());
        }
        self.inner.jobs.resolve_agent_id(identifier)
    }

    fn register_waiter(
        &self,
        agent_id: &str,
        from: Option<&str>,
        reply_to: Option<&str>,
    ) -> Result<MessageWaiterGuard> {
        if self.inner.rebind_reserved.load(Ordering::Acquire) {
            bail!("durable orchestration rebind is in progress");
        }
        let from = from
            .map(|identifier| self.canonical_agent_id(identifier))
            .transpose()?;
        let republish: Option<Arc<dyn Fn(MailboxMessage) + Send + Sync>> = Some(Arc::new({
            let runtime = self.clone();
            move |message: MailboxMessage| {
                if message.to == runtime.main_agent_id() {
                    runtime.publish_message_delivered(message, false);
                }
            }
        }));
        self.inner
            .registry
            .register_message_waiter(
                self.group_id(),
                agent_id,
                from,
                reply_to.map(str::to_owned),
                republish,
            )
            .ok_or_else(|| anyhow!("unknown orchestration agent {agent_id:?}"))
    }

    pub(crate) fn send_with_reply_waiter(
        &self,
        from: &str,
        to: &str,
        body: &str,
        reply_to: Option<String>,
    ) -> Result<(Vec<DeliveryReceipt>, MessageWaiterGuard, String)> {
        if to == "all" {
            bail!("await is invalid with to=all");
        }
        let canonical_to = self.canonical_agent_id(to)?;
        let message_id = Uuid::now_v7().to_string();
        let waiter = self.register_waiter(from, Some(&canonical_to), Some(&message_id))?;
        let receipts =
            self.send_message_with_id(from, to, body, reply_to, Some(message_id.clone()));
        Ok((receipts, waiter, message_id))
    }

    pub(crate) async fn wait_registered_reply(
        &self,
        agent_id: &str,
        from: &str,
        message_id: &str,
        mut waiter: MessageWaiterGuard,
        timeout: Option<Duration>,
        abort: Option<pi_agent::AbortSignal>,
    ) -> Result<Option<MailboxMessage>> {
        self.wait_with_guard(
            agent_id,
            Some(from),
            Some(message_id),
            &mut waiter,
            timeout,
            abort,
        )
        .await
    }

    pub async fn wait_message(
        &self,
        agent_id: &str,
        from: Option<&str>,
        timeout: Option<Duration>,
        abort: Option<pi_agent::AbortSignal>,
    ) -> Result<Option<MailboxMessage>> {
        self.wait_correlated(agent_id, from, None, timeout, abort)
            .await
    }

    /// Wait for a reply whose `replyTo` matches `message_id`.
    pub async fn wait_reply(
        &self,
        agent_id: &str,
        from: Option<&str>,
        message_id: &str,
        timeout: Option<Duration>,
        abort: Option<pi_agent::AbortSignal>,
    ) -> Result<Option<MailboxMessage>> {
        self.wait_correlated(agent_id, from, Some(message_id), timeout, abort)
            .await
    }

    async fn wait_correlated(
        &self,
        agent_id: &str,
        from: Option<&str>,
        reply_to: Option<&str>,
        timeout: Option<Duration>,
        abort: Option<pi_agent::AbortSignal>,
    ) -> Result<Option<MailboxMessage>> {
        if self.inner.shutdown.is_cancelled() {
            bail!("orchestration shut down");
        }
        let from = from
            .map(|identifier| self.canonical_agent_id(identifier))
            .transpose()?;
        let mut waiter = self.register_waiter(agent_id, from.as_deref(), reply_to)?;
        self.wait_with_guard(
            agent_id,
            from.as_deref(),
            reply_to,
            &mut waiter,
            timeout,
            abort,
        )
        .await
    }

    async fn wait_with_guard(
        &self,
        agent_id: &str,
        from: Option<&str>,
        reply_to: Option<&str>,
        waiter_guard: &mut MessageWaiterGuard,
        timeout: Option<Duration>,
        abort: Option<pi_agent::AbortSignal>,
    ) -> Result<Option<MailboxMessage>> {
        let entry = self
            .inner
            .registry
            .get(self.group_id(), agent_id)
            .ok_or_else(|| anyhow!("unknown orchestration agent {agent_id:?}"))?;
        let wait = async {
            loop {
                // Pre-existing matching mail (sent before this waiter
                // registered, or restored by an earlier timed-out wait) is
                // drained from the mailbox first.
                let message = {
                    let _durable_mutation = self.inner.durable_mutation.lock();
                    if self.inner.rebind_reserved.load(Ordering::Acquire) {
                        return Err(anyhow!("durable orchestration rebind is in progress"));
                    }
                    match take_matching_message(&entry, from, reply_to) {
                        Some(message) => {
                            if let Err(error) = self
                                .persist_state()
                                .context("persisting waited orchestration message")
                            {
                                self.restore_waited_message(agent_id, message);
                                return Err(error);
                            }
                            Some(message)
                        }
                        None => None,
                    }
                };
                if message.is_some() {
                    return Ok(message);
                }
                // Otherwise wait for an atomic handoff (the sender removed the
                // message from the mailbox and delivered it to this waiter's
                // channel) or a mailbox wake that warrants re-checking.
                tokio::select! {
                    message = waiter_guard.delivery_rx.recv() => {
                        match message {
                            Some(message) => {
                                if let Err(error) = self
                                    .persist_state()
                                    .context("persisting handed-off orchestration message")
                                {
                                    self.restore_waited_message(agent_id, message);
                                    return Err(error);
                                }
                                return Ok(Some(message));
                            }
                            None => {
                                let notified = entry.message_ready.notified();
                                notified.await;
                            }
                        }
                    }
                    notified = entry.message_ready.notified() => {}
                }
            }
        };
        match (timeout, abort) {
            (Some(timeout), Some(abort)) => tokio::select! {
                message = tokio::time::timeout(timeout, wait) => message.unwrap_or(Ok(None)),
                () = abort.cancelled() => Err(anyhow!("message wait aborted")),
                () = self.inner.shutdown.cancelled() => Err(anyhow!("orchestration shut down")),
            },
            (Some(timeout), None) => tokio::select! {
                message = tokio::time::timeout(timeout, wait) => message.unwrap_or(Ok(None)),
                () = self.inner.shutdown.cancelled() => Err(anyhow!("orchestration shut down")),
            },
            (None, Some(abort)) => tokio::select! {
                message = wait => message,
                () = abort.cancelled() => Err(anyhow!("message wait aborted")),
                () = self.inner.shutdown.cancelled() => Err(anyhow!("orchestration shut down")),
            },
            (None, None) => tokio::select! {
                message = wait => message,
                () = self.inner.shutdown.cancelled() => Err(anyhow!("orchestration shut down")),
            },
        }
    }

    pub fn cancel(&self, ids: &[String]) -> Vec<String> {
        let _durable_mutation = self.inner.durable_mutation.lock();
        if self.inner.rebind_reserved.load(Ordering::Acquire) {
            return Vec::new();
        }
        let active = self.inner.active.lock();
        let mut cancelled = Vec::new();
        for id in ids {
            if let Some(token) = active.get(id) {
                token.cancel();
                cancelled.push(id.clone());
            }
        }
        cancelled
    }

    pub fn park(&self, id: &str) -> Result<()> {
        let _durable_mutation = self.inner.durable_mutation.lock();
        if self.inner.rebind_reserved.load(Ordering::Acquire) {
            bail!("durable orchestration rebind is in progress");
        }
        if id == self.inner.config.main_agent_id {
            bail!("orchestration main agent cannot be parked");
        }
        self.cancel_park_timer(id);
        let previous = self
            .agent_snapshot(id)
            .ok_or_else(|| anyhow!("unknown orchestration agent {id:?}"))?
            .status;
        self.inner
            .registry
            .set_status(self.group_id(), id, AgentStatus::Parked)?;
        if let Err(error) = self.persist_state().context("persisting parked child") {
            let _ = self.inner.registry.compare_status(
                self.group_id(),
                id,
                AgentStatus::Parked,
                previous,
            );
            return Err(error);
        }
        if let Some(agent) = self.agent_snapshot(id) {
            self.publish_agent(agent);
        }
        Ok(())
    }

    /// Finalize an agent's lifecycle state and arm idle-to-park transition.
    pub fn finish_agent(
        &self,
        id: &str,
        status: AgentStatus,
        artifact_path: Option<PathBuf>,
        history_path: Option<PathBuf>,
    ) -> Result<()> {
        let _durable_mutation = self.inner.durable_mutation.lock();
        if self.inner.rebind_reserved.load(Ordering::Acquire) {
            bail!("durable orchestration rebind is in progress");
        }
        let previous = self
            .agent_snapshot(id)
            .ok_or_else(|| anyhow!("unknown orchestration agent {id:?}"))?;
        let previous_artifact = self
            .inner
            .registry
            .get(self.group_id(), id)
            .and_then(|entry| entry.artifact_path.lock().clone());
        let previous_history = self
            .inner
            .registry
            .get(self.group_id(), id)
            .and_then(|entry| entry.history_path.lock().clone());
        self.inner
            .registry
            .finish(self.group_id(), id, status, artifact_path, history_path)?;
        if let Err(error) = self.persist_state().context("persisting finished child") {
            self.inner.registry.restore_finish(
                self.group_id(),
                id,
                previous,
                previous_artifact,
                previous_history,
            );
            return Err(error);
        }
        if let Some(agent) = self.agent_snapshot(id) {
            self.publish_agent(agent);
        }
        match status {
            AgentStatus::Idle => {
                if let Some(ttl) = self.inner.config.idle_ttl {
                    self.schedule_idle_park(id, ttl);
                }
            }
            AgentStatus::Aborted | AgentStatus::Running | AgentStatus::Queued => {
                self.cancel_park_timer(id);
            }
            AgentStatus::Parked => {}
        }
        Ok(())
    }

    fn schedule_idle_park(&self, id: &str, ttl: Duration) {
        if id == self.inner.config.main_agent_id {
            return;
        }
        let Some(entry) = self.inner.registry.get(self.group_id(), id) else {
            return;
        };
        let token = CancellationToken::new();
        *entry.idle_park_token.lock() = Some(token.clone());
        let shutdown = self.inner.shutdown.clone();
        let runtime = self.clone();
        let id_owned = id.to_owned();
        let mut timers = self.inner.park_timers.lock();
        if let Some(previous) = timers.remove(id) {
            previous.abort();
        }
        let handle = tokio::spawn(async move {
            tokio::select! {
                () = shutdown.cancelled() => {}
                () = token.cancelled() => {}
                () = tokio::time::sleep(ttl) => {
                    let _durable_mutation = runtime.inner.durable_mutation.lock();
                    if runtime.inner.rebind_reserved.load(Ordering::Acquire) {
                        return;
                    }
                    if let Some(agent) = runtime.inner.registry.park_if_idle(
                        runtime.group_id(),
                        &id_owned,
                        &runtime.inner.config.main_agent_id,
                    ) {
                        runtime.publish_agent(agent);
                        if runtime.persist_state().is_err() {
                            let _ = runtime.inner.registry.compare_status(
                                runtime.group_id(),
                                &id_owned,
                                AgentStatus::Parked,
                                AgentStatus::Idle,
                            );
                            if let Some(agent) = runtime.agent_snapshot(&id_owned) {
                                runtime.publish_agent(agent);
                            }
                        }
                    }
                }
            }
        });
        timers.insert(id.to_owned(), handle);
    }

    fn cancel_park_timer(&self, id: &str) {
        let handle = self.inner.park_timers.lock().remove(id);
        if let Some(handle) = handle {
            handle.abort();
        }
        if let Some(entry) = self.inner.registry.get(self.group_id(), id)
            && let Some(token) = entry.idle_park_token.lock().take()
        {
            token.cancel();
        }
    }

    pub fn spawn_tasks(
        &self,
        parent_id: &str,
        parent_depth: usize,
        items: Vec<TaskItem>,
    ) -> Result<Vec<TaskSpawn>> {
        if parent_depth >= self.inner.config.max_recursion_depth {
            bail!("subagent recursion depth limit reached");
        }
        if items.is_empty() {
            bail!("task batch must contain at least one item");
        }
        #[cfg(test)]
        if let Some((SpawnTestParkPoint::BeforeSpawnLock, release)) =
            self.inner.spawn_test_park.lock().clone()
        {
            while !release.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
        }
        let _spawn_guard = self.inner.jobs.lock_spawns();
        #[cfg(test)]
        if let Some((SpawnTestParkPoint::AfterSpawnLock, release)) =
            self.inner.spawn_test_park.lock().clone()
        {
            while !release.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
        }
        let _durable_mutation = self.inner.durable_mutation.lock();
        if self.inner.retiring.load(Ordering::Acquire) {
            bail!(
                "orchestration runtime is being replaced; retry the task after the runtime switch completes"
            );
        }
        if self.inner.rebind_reserved.load(Ordering::Acquire) {
            bail!("durable orchestration rebind is in progress");
        }
        let mut reserved = std::collections::BTreeSet::new();
        let mut prepared = Vec::with_capacity(items.len());
        let available_models = crate::available_models();
        for mut item in items {
            let definition = self
                .inner
                .config
                .catalog
                .get(&item.agent)
                .cloned()
                .ok_or_else(|| anyhow!("unknown agent definition {:?}", item.agent))?;
            if definition.is_persona()
                && self
                    .inner
                    .persona_lifecycle_blocks
                    .lock()
                    .contains(&definition.name)
            {
                bail!(
                    "persona {:?} has a destructive operation in progress",
                    definition.name
                );
            }
            item.id =
                self.allocate_unique_agent_id_for_definition(&item.id, &definition, &mut reserved)?;
            if !definition.trusted {
                bail!("agent definition {:?} is not trusted", item.agent);
            }
            if definition.is_persona() && !self.inner.durable_bound.load(Ordering::Acquire) {
                bail!(
                    "persona {:?} requires orchestration to be bound to a durable parent session",
                    definition.name
                );
            }
            if definition.is_persona() && definition.persona_root().is_none() {
                bail!("persona {:?} has no durable persona root", definition.name);
            }
            if self
                .inner
                .config
                .agent_settings
                .get(&item.agent)
                .is_some_and(|settings| settings.enabled == Some(false))
            {
                return Err(crate::agent_disabled_error(&item.agent));
            }
            let agent_settings = self.inner.config.agent_settings.get(&item.agent);
            let unsupported = crate::unsupported_agent_tools(&definition, agent_settings);
            if !unsupported.is_empty() {
                self.record_unknown_tool_warnings(&item.agent, &unsupported);
            }
            let resolved_tools = crate::resolved_child_coding_tools(&definition, agent_settings);
            // `tools: None` inherits the full parent/default child tool set,
            // which may exceed the cap; only explicitly declared lists are
            // capped.
            let inherited_full_set =
                crate::effective_agent_tool_names(&definition, agent_settings).is_none();
            if !inherited_full_set
                && resolved_tools.len() > self.inner.config.max_tools_per_agent
            {
                return Err(crate::child_tool_cap_error(
                    &item.agent,
                    self.inner.config.max_tools_per_agent,
                    &resolved_tools,
                ));
            }
            let parent_model = self.inner.config.current_parent_model();
            let resolved_model = crate::resolve_agent_model(
                &definition,
                agent_settings,
                &parent_model,
                &available_models,
            )
            .map_err(|error| crate::agent_model_error(&item.agent, &error))?;
            prepared.push((item, definition, resolved_model));
        }

        let workflow_scope = self.inner.workflow_scope.lock().clone();
        let mut launches = Vec::with_capacity(prepared.len());
        for (item, definition, resolved_model) in prepared {
            let created_at = now_millis();
            let job_id = loop {
                let candidate = Uuid::now_v7().to_string();
                if !self.inner.jobs.contains_identifier(&candidate)
                    && !reserved.contains(&candidate)
                {
                    reserved.insert(candidate.clone());
                    break candidate;
                }
            };
            let cancel = CancellationToken::new();
            let agent_snapshot = AgentSnapshot {
                id: item.id.clone(),
                display_name: definition.name.clone(),
                agent: definition.name.clone(),
                parent_id: Some(parent_id.to_owned()),
                status: AgentStatus::Queued,
                created_at,
                last_activity: created_at,
                unread: 0,
                artifact_ref: None,
                history_ref: None,
            };
            let job_snapshot = JobSnapshot {
                id: job_id,
                agent_id: item.id.clone(),
                agent: item.agent.clone(),
                parent_id: parent_id.to_owned(),
                description: Some(presentation_text(&item.assignment, TASK_DESCRIPTION_MAX_CHARS)),
                todo_task_id: item.todo_task_id.clone(),
                workflow_id: workflow_scope
                    .as_ref()
                    .map(|scope| scope.workflow_id.clone()),
                workflow_generation: workflow_scope.as_ref().map(|scope| scope.generation),
                status: JobStatus::Queued,
                created_at,
                started_at: None,
                finished_at: None,
                result: None,
                soft_budget_exhausted: false,
            };
            launches.push((
                item,
                definition,
                resolved_model,
                cancel,
                agent_snapshot,
                job_snapshot,
            ));
        }

        self.inner.registry.register_batch(
            self.group_id(),
            launches
                .iter()
                .map(|(_, _, _, _, agent, _)| agent.clone())
                .collect(),
            self.inner.config.mailbox_capacity,
        )?;

        let registered_ids = launches
            .iter()
            .map(|(item, _, _, _, _, _)| item.id.clone())
            .collect::<Vec<_>>();
        let launches = match launches
            .into_iter()
            .map(
                |(item, definition, resolved_model, cancel, agent_snapshot, job_snapshot)| {
                    let (persona_skill_store, persona_skills) =
                        self.persona_skill_context(&definition)?;
                    let shared_skills = self.shared_skill_snapshot();
                    let peer_roster = self.sibling_roster(&item.id);
                    let request = ChildSessionRequest {
                        child_id: item.id.clone(),
                        parent_id: parent_id.to_owned(),
                        depth: parent_depth + 1,
                        system_prompt: self.child_system_prompt_with_persona_skills(
                            &definition,
                            &item.assignment,
                            item.context.as_deref(),
                            item.output_schema.as_ref(),
                            item.schema_mode.as_deref(),
                            &peer_roster,
                            &persona_skills,
                        )?,
                        requested_tool_names: crate::effective_agent_tool_names(
                            &definition,
                            self.inner.config.agent_settings.get(&definition.name),
                        )
                        .map(<[String]>::to_vec),
                        orchestration_tools: self.agent_tools(&item.id, parent_depth + 1),
                        thinking_level: resolved_model.thinking_level.or(definition.thinking_level),
                        max_tools_per_agent: self.inner.config.max_tools_per_agent,
                        model: resolved_model.model,
                        persona_skill_store,
                        persona_skills,
                        shared_skills,
                        definition,
                        assignment: item.assignment.clone(),
                        output_schema: item.output_schema.clone(),
                        schema_mode: item.schema_mode.clone(),
                        yield_state: Arc::new(YieldState::default()),
                        resolved_model_sequence: resolved_model.sequence.clone(),
                        resolved_model_selected_index: resolved_model.selected_index,
                    };
                    Ok((item, request, cancel, agent_snapshot, job_snapshot))
                },
            )
            .collect::<Result<Vec<_>>>()
        {
            Ok(launches) => launches,
            Err(error) => {
                self.inner
                    .registry
                    .unregister_batch(self.group_id(), &registered_ids);
                return Err(error);
            }
        };
        let mut spawns = Vec::with_capacity(launches.len());
        for (item, _, cancel, agent_snapshot, job_snapshot) in &launches {
            let job_snapshot = match self.inner.jobs.insert(job_snapshot.clone(), cancel.clone()) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    self.inner
                        .registry
                        .unregister_batch(self.group_id(), &registered_ids);
                    return Err(error);
                }
            };
            self.publish_agent(agent_snapshot.clone());
            self.publish_job(job_snapshot.clone());
            self.inner
                .active
                .lock()
                .insert(item.id.clone(), cancel.clone());
            spawns.push(TaskSpawn {
                index: item.index,
                job_id: job_snapshot.id,
                agent_id: item.id.clone(),
                agent: item.agent.clone(),
                status: JobStatus::Queued,
            });
        }

        if self.inner.durable_bound.load(Ordering::Acquire) {
            for (item, request, _, _, _) in &launches {
                if let Some(entry) = self.inner.registry.get(self.group_id(), &item.id) {
                    *entry.durable_info.lock() = Some(DurableAgentInfo {
                        definition: super::persistence::persist_definition(&request.definition),
                        request: super::persistence::persist_request(request),
                        session_path: None,
                    });
                }
            }
        }
        if let Err(error) = self.persist_state() {
            for (_, _, cancel, _, job) in &launches {
                cancel.cancel();
                self.inner.jobs.remove(&job.id);
            }
            for id in &registered_ids {
                self.inner.active.lock().remove(id);
            }
            self.inner
                .registry
                .unregister_batch(self.group_id(), &registered_ids);
            return Err(error).context("persisting durable child spawn");
        }

        for (item, request, cancel, _, job_snapshot) in launches {
            let runtime = self.clone();
            tokio::spawn(async move {
                let result = runtime
                    .run_one(item, request, cancel, &job_snapshot.id)
                    .await;
                {
                    let _durable_mutation = runtime.inner.durable_mutation.lock();
                    if !runtime.inner.rebind_reserved.load(Ordering::Acquire)
                        && let Some(job) =
                            runtime
                                .inner
                                .jobs
                                .finish(&job_snapshot.id, result, now_millis())
                    {
                        let job = match runtime.persist_state() {
                            Ok(()) => job,
                            Err(error) => runtime
                                .inner
                                .jobs
                                .append_result_error(&job_snapshot.id, &error.to_string())
                                .unwrap_or(job),
                        };
                        runtime.publish_job(job);
                    }
                }
                runtime.prune_retained_jobs();
            });
        }
        spawns.sort_by_key(|spawn| spawn.index);
        Ok(spawns)
    }

    pub async fn run_tasks(
        &self,
        parent_id: &str,
        parent_depth: usize,
        items: Vec<TaskItem>,
        abort: pi_agent::AbortSignal,
    ) -> Result<Vec<TaskResult>> {
        let spawns = self.spawn_tasks(parent_id, parent_depth, items)?;
        let ids = spawns
            .iter()
            .map(|spawn| spawn.job_id.clone())
            .collect::<Vec<_>>();
        self.inner
            .jobs
            .wait_all_results(&ids, abort, self.inner.shutdown.clone())
            .await
    }

    /// Build the per-turn stop hook implementing this runtime's soft budget for
    /// a child job, or `None` when no budget knob is configured (unlimited,
    /// historical run-to-completion behavior).
    ///
    /// The hook fires after every completed assistant turn: it counts requests
    /// and accumulates per-turn `Usage::total_tokens`, and returns `true` once
    /// a configured limit is reached. The agent loop then ends the run cleanly
    /// after that turn — the partial output and accumulated usage are preserved
    /// — and `state.triggered` records that the settled job must carry the
    /// `soft_budget_exhausted` marker (the job is never failed by a budget).
    fn soft_budget_stop_hook(
        &self,
        definition: &AgentDefinition,
        state: &JobSoftBudgetState,
    ) -> Option<ShouldStopAfterTurnFn> {
        let budget = if definition.is_persona() {
            definition
                .soft_budget
                .unwrap_or(self.inner.config.soft_budget)
        } else {
            self.inner.config.soft_budget
        };
        if budget.max_requests.is_none()
            && budget.max_tokens.is_none()
            && budget.yield_after.is_none()
        {
            return None;
        }
        let requests = state.requests.clone();
        let tokens = state.tokens.clone();
        let triggered = state.triggered.clone();
        Some(Arc::new(move |turn: &ShouldStopAfterTurnContext| {
            let request_count = requests.fetch_add(1, Ordering::Relaxed) + 1;
            let turn_tokens =
                u64::try_from(turn.message.usage.total_tokens.max(0)).unwrap_or(u64::MAX);
            let token_count = tokens.fetch_add(turn_tokens, Ordering::Relaxed) + turn_tokens;
            let stop = budget
                .yield_after
                .is_some_and(|limit| request_count >= limit)
                || budget
                    .max_requests
                    .is_some_and(|limit| request_count >= limit)
                || budget.max_tokens.is_some_and(|limit| token_count >= limit);
            if stop {
                triggered.store(true, Ordering::Release);
            }
            stop
        }))
    }

    /// Turn stop hook implementing the definition's `max_turns` and
    /// `max_tool_calls` contracts, or `None` when neither is configured.
    ///
    /// Like [`Self::soft_budget_stop_hook`] the hook fires after each completed
    /// assistant turn and ends the run cleanly once a limit is reached; the run
    /// loop then surfaces the triggered limit as a clear per-job reason.
    fn definition_contract_stop_hook(
        &self,
        definition: &AgentDefinition,
        state: &JobContractState,
    ) -> Option<ShouldStopAfterTurnFn> {
        if definition.max_turns.is_none() && definition.max_tool_calls.is_none() {
            return None;
        }
        let max_turns = definition.max_turns;
        let max_tool_calls = definition.max_tool_calls;
        let turns = state.turns.clone();
        let tool_calls = state.tool_calls.clone();
        let max_turns_triggered = state.max_turns_triggered.clone();
        let max_tool_calls_triggered = state.max_tool_calls_triggered.clone();
        Some(Arc::new(move |context: &ShouldStopAfterTurnContext| {
            let mut stop = false;
            let turn_count = turns.fetch_add(1, Ordering::Relaxed) + 1;
            if max_turns.is_some_and(|limit| turn_count >= limit) {
                max_turns_triggered.store(true, Ordering::Release);
                stop = true;
            }
            let turn_tool_calls = context
                .message
                .content
                .iter()
                .filter(|block| matches!(block, pi_ai::ContentBlock::ToolCall(_)))
                .count();
            let call_count =
                tool_calls.fetch_add(turn_tool_calls, Ordering::Relaxed) + turn_tool_calls;
            if max_tool_calls.is_some_and(|limit| call_count >= limit) {
                max_tool_calls_triggered.store(true, Ordering::Release);
                stop = true;
            }
            stop
        }))
    }

    /// Combine the soft-budget, role-contract, and yield-delivery stop hooks so
    /// the child stops when any of them fires. The yield hook is always
    /// present: once the child calls `yield` the run must end after that turn
    /// so the delivered payload becomes the final output and the model never
    /// produces trailing text after the delivery.
    fn compose_turn_stop_hooks(
        &self,
        definition: &AgentDefinition,
        soft_budget: &JobSoftBudgetState,
        contract: &JobContractState,
        yield_state: &YieldState,
    ) -> Option<ShouldStopAfterTurnFn> {
        let yield_called = yield_state.clone();
        let yield_hook =
            Arc::new(move |_turn: &ShouldStopAfterTurnContext| yield_called.was_called());
        match (
            self.soft_budget_stop_hook(definition, soft_budget),
            self.definition_contract_stop_hook(definition, contract),
        ) {
            (Some(left), Some(right)) => Some(Arc::new(move |context| {
                left(context) || right(context) || yield_hook(context)
            })),
            (Some(hook), None) | (None, Some(hook)) => Some(Arc::new(move |context| {
                hook(context) || yield_hook(context)
            })),
            (None, None) => Some(yield_hook),
        }
    }

    /// Clear reason when a role contract limit stopped the child early, or
    /// `None` when no contract limit fired.
    fn contract_stop_reason(
        &self,
        definition: &AgentDefinition,
        state: &JobContractState,
    ) -> Option<String> {
        if state.max_turns_triggered.load(Ordering::Acquire) {
            return Some(format!(
                "role `{}` exceeded its maxTurns contract ({} turns)",
                definition.name,
                definition.max_turns.unwrap_or_default(),
            ));
        }
        if state.max_tool_calls_triggered.load(Ordering::Acquire) {
            return Some(format!(
                "role `{}` exceeded its maxToolCalls contract ({} tool calls)",
                definition.name,
                definition.max_tool_calls.unwrap_or_default(),
            ));
        }
        None
    }

    /// Projects a settled child run into the job's `(output, error, usage)`
    /// triple.
    ///
    /// Yield protocol (OMP parity): when the child called `yield`, the
    /// delivered payload REPLACES the trailing assistant text as the final
    /// output — the transcript keeps the child's concise yield-marker message,
    /// but the payload is what the parent receives. When the child ended
    /// naturally WITHOUT calling yield, the natural final text is kept and
    /// [`MISSING_YIELD_WARNING`] is appended (back-compat: children written
    /// before the explicit-delivery protocol still settle with their text).
    /// Soft-budget and contract-limited stops are untouched (no warning): the
    /// host cut the run short, so the partial text stands as-is. Error paths
    /// keep the error text and never project a payload.
    fn settle_child_outcome(
        &self,
        outcome: Result<crate::RunResult, String>,
        child: &ChildSession,
        yield_state: &YieldState,
        execution_ceiling_reached: bool,
    ) -> (String, Option<String>, pi_ai::Usage) {
        let (mut output, error, usage) = match outcome {
            Ok(result) => {
                // An unrecovered final tool failure is authoritative: when
                // the last actual ToolResult/BashExecution failed and no
                // later tool result succeeded, the child's final prose must
                // never mask it into a Completed job (the workflow would
                // otherwise mark the task Done and integrate work whose last
                // step failed). Denied/unavailable attempts caused by the
                // child's own restricted tool set (an error ToolResult for a
                // tool the child does not possess, e.g. a read-only child
                // calling `write`) remain visible in the transcript as
                // `is_error` results but never fail an otherwise completed
                // read-only run. A provider-reported error still wins when
                // present; the synthesized summary is bounded and carries no
                // tool output.
                let tool_failure =
                    last_failed_tool_kind(&result.messages, &child.available_tool_names())
                        .map(|kind| format!("last tool execution failed: {kind}"));
                (
                    result.text,
                    result.error_message.or(tool_failure),
                    result.usage,
                )
            }
            Err(error) => (
                child.last_assistant_text(),
                Some(error),
                pi_ai::Usage::default(),
            ),
        };
        if error.is_none() {
            if let Some(payload) = yield_state.payload() {
                output = payload;
            } else if !execution_ceiling_reached {
                append_missing_yield_warning(&mut output);
            }
        }
        (output, error, usage)
    }

    async fn run_one(
        &self,
        item: TaskItem,
        request: ChildSessionRequest,
        cancel: CancellationToken,
        job_id: &str,
    ) -> TaskResult {
        let _active_guard = ActiveChildGuard {
            inner: self.inner.clone(),
            id: item.id.clone(),
            group_id: self.group_id().to_owned(),
        };
        let definition = request.definition.clone();
        let artifact_ref = format!("agent://{}", item.id);
        let history_ref = history_reference(&definition, &item.id);
        let artifact_uri = format!("artifact://{}", item.id);
        let artifact_path = self
            .inner
            .config
            .artifact_dir
            .join(format!("{}-{job_id}.md", item.id));
        let history_path = self
            .inner
            .config
            .artifact_dir
            .join(format!("{}-{job_id}.history.json", item.id));
        let local_permit = tokio::select! {
            permit = self.inner.semaphore.clone().acquire_owned() => match permit {
                Ok(permit) => permit,
                Err(_) => return self.failed_result(&item, job_id, AgentStatus::Aborted, "orchestration semaphore closed"),
            },
            () = cancel.cancelled() => return self.failed_result(&item, job_id, AgentStatus::Aborted, "task cancelled before start"),
        };
        let global_gate = self.inner.global_concurrency.lock().clone();
        let global_permit = if let Some(gate) = global_gate {
            Some(tokio::select! {
                permit = gate.semaphore.acquire_owned() => match permit {
                    Ok(permit) => permit,
                    Err(_) => return self.failed_result(&item, job_id, AgentStatus::Aborted, "workflow orchestration global semaphore closed"),
                },
                () = cancel.cancelled() => return self.failed_result(&item, job_id, AgentStatus::Aborted, "task cancelled before start"),
            })
        } else {
            None
        };
        let durable_mutation = self.inner.durable_mutation.lock();
        if self.inner.rebind_reserved.load(Ordering::Acquire) {
            drop(durable_mutation);
            return self.failed_result(
                &item,
                job_id,
                AgentStatus::Idle,
                "durable orchestration rebind is in progress",
            );
        }
        self.inner
            .registry
            .set_status(self.group_id(), &item.id, AgentStatus::Running)
            .ok();
        let running_agent = self.agent_snapshot(&item.id);
        let running_job = self.inner.jobs.mark_running(job_id, now_millis());
        if let Err(error) = self.persist_state() {
            let error = format!("persisting child running state: {error:#}");
            drop(durable_mutation);
            return self.failed_result(&item, job_id, AgentStatus::Idle, &error);
        }
        drop(durable_mutation);
        if let Some(agent) = running_agent {
            self.publish_agent(agent);
        }
        if let Some(job) = running_job {
            self.publish_job(job);
        }
        // The factory takes `request` by value; the run loop still needs the
        // delivery state and output contract after the child session is built.
        let yield_state = request.yield_state.clone();
        let output_schema = request.output_schema.clone();
        let schema_mode = request.schema_mode.clone();
        let persona_root = request.definition.persona_root();
        let child_session = tokio::select! {
            result = (self.inner.factory)(request) => match result {
                Ok(session) => session,
                Err(error) => return self.failed_result(&item, job_id, AgentStatus::Idle, &error.to_string()),
            },
            () = cancel.cancelled() => return self.failed_result(&item, job_id, AgentStatus::Aborted, "task cancelled during child session creation"),
        };
        let generation_capture = { self.inner.generation_capture.lock().clone() };
        if let Some(generation_capture) = generation_capture {
            if child_session.resource_manager().is_some() {
                return self.failed_result(
                    &item,
                    job_id,
                    AgentStatus::Idle,
                    "child session factory returned a session with foreign resource authority",
                );
            }
            child_session.bind_generation_capture(generation_capture);
        }
        if definition.is_persona() {
            let continuity = match self.load_persona_continuity(&definition.name) {
                Ok(messages) => messages,
                Err(error) => {
                    return self.failed_result(
                        &item,
                        job_id,
                        AgentStatus::Idle,
                        &format!("loading persona continuity: {error:#}"),
                    );
                }
            };
            if let Err(error) = child_session.load_history(continuity).await {
                return self.failed_result(
                    &item,
                    job_id,
                    AgentStatus::Idle,
                    &format!("installing persona continuity: {error:#}"),
                );
            }
        }
        let durable = if self.inner.durable_bound.load(Ordering::Acquire) {
            let durable = { self.inner.durable.lock().clone() };
            match durable {
                Some(durable) => Some(durable),
                None => {
                    return self.failed_result(
                        &item,
                        job_id,
                        AgentStatus::Idle,
                        "durable orchestration binding is unavailable",
                    );
                }
            }
        } else {
            None
        };
        let mut canonical_session_path = None;
        if let Some(durable) = durable.as_ref() {
            let recording = if self.workflow_scope().is_some() {
                child_session.start_durable_child_recording_with_kind(
                    crate::SessionKind::WorkflowWorker,
                    durable.child_root(),
                    durable.parent_session_path(),
                )
            } else {
                child_session.start_durable_child_recording(
                    durable.child_root(),
                    durable.parent_session_path(),
                )
            };
            if let Err(error) = recording {
                return self.failed_result(
                    &item,
                    job_id,
                    AgentStatus::Idle,
                    &format!("starting durable child recording: {error:#}"),
                );
            }
            let Some((_, session_path)) = child_session.recorder_info() else {
                return self.failed_result(
                    &item,
                    job_id,
                    AgentStatus::Idle,
                    "durable child recorder is unavailable",
                );
            };
            let session_path = match durable.child_file_authority(&session_path) {
                Ok(_) => session_path,
                Err(error) => {
                    return self.failed_result(
                        &item,
                        job_id,
                        AgentStatus::Idle,
                        &error.to_string(),
                    );
                }
            };
            canonical_session_path = Some(session_path.clone());
            let durable_mutation = self.inner.durable_mutation.lock();
            if self.inner.rebind_reserved.load(Ordering::Acquire) {
                drop(durable_mutation);
                return self.failed_result(
                    &item,
                    job_id,
                    AgentStatus::Idle,
                    "durable orchestration rebind is in progress",
                );
            }
            if let Some(entry) = self.inner.registry.get(self.group_id(), &item.id)
                && let Some(info) = entry.durable_info.lock().as_mut()
            {
                info.session_path = Some(session_path);
            }
            if let Err(error) = self.persist_state() {
                let error = format!("persisting durable child transcript path: {error:#}");
                drop(durable_mutation);
                return self.failed_result(&item, job_id, AgentStatus::Idle, &error);
            }
            drop(durable_mutation);
        }
        let child = ChildSession::new(child_session);
        let soft_budget_state = JobSoftBudgetState::default();
        let contract_state = JobContractState::default();
        let stop_hook = self.compose_turn_stop_hooks(
            &definition,
            &soft_budget_state,
            &contract_state,
            &yield_state,
        );
        child.set_should_stop_after_turn(stop_hook);
        let (delivery_tx, mut delivery_rx) = tokio::sync::mpsc::unbounded_channel();
        let pre_run = {
            let durable_mutation = self.inner.durable_mutation.lock();
            if self.inner.rebind_reserved.load(Ordering::Acquire) {
                drop(durable_mutation);
                return self.failed_result(
                    &item,
                    job_id,
                    AgentStatus::Idle,
                    "durable orchestration rebind is in progress",
                );
            }
            match self.inner.registry.register_active_delivery(
                self.group_id(),
                &item.id,
                delivery_tx,
                cancel.clone(),
            ) {
                Ok(messages) => messages,
                Err(error) => {
                    drop(durable_mutation);
                    return self.failed_result(
                        &item,
                        job_id,
                        AgentStatus::Idle,
                        &error.to_string(),
                    );
                }
            }
        };
        for message in &pre_run {
            if let Err(error) = child.steer(message).await {
                return self.failed_result(
                    &item,
                    job_id,
                    AgentStatus::Idle,
                    &format!("failed to deliver queued message {}: {error:#}", message.id),
                );
            }
            if let Err(error) = self.acknowledge_delivery(&item.id, &message.id) {
                return self.failed_result(
                    &item,
                    job_id,
                    AgentStatus::Idle,
                    &format!("acknowledging queued message {}: {error:#}", message.id),
                );
            }
        }
        let mut run = Box::pin(child.run(&item.assignment));
        let run_deadline = definition
            .timeout_secs
            .map(|seconds| tokio::time::Instant::now() + Duration::from_secs(seconds));
        let outcome = loop {
            tokio::select! {
                result = &mut run => break result.map_err(|error| error.to_string()),
                message = delivery_rx.recv() => {
                    match message {
                        Some(message) => {
                            if let Err(error) = child.steer(&message).await {
                                break Err(format!("failed to deliver message {}: {error:#}", message.id));
                            }
                            if let Err(error) = self.acknowledge_delivery(&item.id, &message.id) {
                                break Err(format!("acknowledging delivered message {}: {error:#}", message.id));
                            }
                        }
                        None => break Err("active child delivery bridge closed".to_owned()),
                    }
                }
                () = cancel.cancelled() => {
                    {
                        let _durable_mutation = self.inner.durable_mutation.lock();
                        self.inner.registry.unregister_active_delivery(self.group_id(), &item.id);
                    }
                    let drain = async {
                        child.abort().await;
                        let _ = (&mut run).await;
                    };
                    break if tokio::time::timeout(CHILD_ABORT_GRACE, drain).await.is_err() {
                        Err(format!(
                            "task cancellation timed out after {}s",
                            CHILD_ABORT_GRACE.as_secs()
                        ))
                    } else {
                        Err("task cancelled".to_owned())
                    };
                }
                () = async {
                    tokio::time::sleep_until(run_deadline.expect("deadline guard")).await;
                }, if run_deadline.is_some() => {
                    {
                        let _durable_mutation = self.inner.durable_mutation.lock();
                        self.inner.registry.unregister_active_delivery(self.group_id(), &item.id);
                    }
                    let drain = async {
                        child.abort().await;
                        let _ = (&mut run).await;
                    };
                    let timeout_secs = definition.timeout_secs.unwrap_or_default();
                    break if tokio::time::timeout(CHILD_ABORT_GRACE, drain).await.is_err() {
                        Err(format!(
                            "role `{}` exceeded its timeout contract of {timeout_secs}s and did not settle after abort",
                            definition.name,
                        ))
                    } else {
                        Err(format!(
                            "role `{}` exceeded its timeout contract of {timeout_secs}s",
                            definition.name,
                        ))
                    };
                }
            }
        };
        {
            let _durable_mutation = self.inner.durable_mutation.lock();
            self.inner
                .registry
                .unregister_active_delivery(self.group_id(), &item.id);
        }
        // A message steered in the final-turn window may still sit in the
        // session's steering queue (the bridge acknowledged it at steer time,
        // before the run settled). Return it to the mailbox so the next run
        // or revival delivers it exactly once. A failed persist of the
        // restore must surface on the run result, never be swallowed.
        let mut restore_errors = Vec::new();
        let (mailbox_messages, unrestorable) = child.drain_unprocessed_orchestration_steering().await;
        for message in mailbox_messages {
            if let Err(error) = self.restore_unprocessed_delivery(&item.id, message.clone()) {
                restore_errors.push(format!(
                    "restoring unprocessed delivery {}: {error:#}",
                    message.id
                ));
            }
        }
        restore_errors.extend(unrestorable);
        drop(global_permit);
        drop(local_permit);
        let status = if cancel.is_cancelled() {
            AgentStatus::Aborted
        } else {
            AgentStatus::Idle
        };
        let soft_budget_exhausted = soft_budget_state.triggered.load(Ordering::Acquire);
        let execution_ceiling_reached = soft_budget_exhausted
            || contract_state.max_turns_triggered.load(Ordering::Acquire)
            || contract_state
                .max_tool_calls_triggered
                .load(Ordering::Acquire);
        let (output, error, usage) =
            self.settle_child_outcome(outcome, &child, &yield_state, execution_ceiling_reached);
        let structured_output = super::tools::validate_delivered_output(
            &output,
            output_schema.as_ref(),
            schema_mode.as_deref(),
            error.as_deref(),
        );
        let artifact_body = if output.is_empty() {
            error.as_deref().unwrap_or("(no output)").to_owned()
        } else {
            output.clone()
        };
        let artifact_error = write_new_artifact(
            &self.inner.storage.artifacts,
            &artifact_path,
            artifact_body.as_bytes(),
        )
        .with_context(|| format!("writing subagent artifact {}", artifact_path.display()))
        .err();
        let artifact_written = artifact_error.is_none();
        let history_error = serde_json::to_vec_pretty(&child.history())
            .context("serializing subagent history")
            .and_then(|bytes| {
                write_new_artifact(&self.inner.storage.artifacts, &history_path, &bytes)
                    .with_context(|| format!("writing subagent history {}", history_path.display()))
            })
            .err();
        let history_written = history_error.is_none();
        let mut final_error = match artifact_error.or(history_error) {
            Some(write_error) => Some(match error.as_deref() {
                Some(error) => format!("{error}; {write_error}"),
                None => write_error.to_string(),
            }),
            None => error,
        };
        for restore_error in restore_errors {
            final_error = Some(match final_error {
                Some(error) => format!("{error}; {restore_error}"),
                None => restore_error,
            });
        }
        if definition.is_persona()
            && let (Some(durable), Some(source)) =
                (durable.as_ref(), canonical_session_path.as_deref())
            && let Err(archive_error) =
                self.archive_persona_session(&definition.name, &item.id, durable, source, false)
        {
            let message = format!("archiving persona transcript: {archive_error:#}");
            final_error = Some(match final_error {
                Some(error) => format!("{error}; {message}"),
                None => message,
            });
        }
        if let Err(persist_error) = self.finish_agent(
            &item.id,
            status,
            artifact_written.then_some(artifact_path),
            history_written.then_some(history_path),
        ) {
            let message = persist_error.to_string();
            final_error = Some(match final_error {
                Some(error) => format!("{error}; {message}"),
                None => message,
            });
        }
        if let Some(reason) = self.contract_stop_reason(&definition, &contract_state) {
            final_error = Some(match final_error {
                Some(error) => format!("{reason}; {error}"),
                None => reason,
            });
        }
        // Strict schema mode surfaces a delivered payload that fails its
        // outputSchema contract as a job error (the child still settled).
        if schema_mode.as_deref() == Some("strict")
            && let Some(validation) = structured_output.as_ref()
            && !validation.valid
            && let Some(validation_error) = validation.error.as_deref()
        {
            final_error = Some(match final_error {
                Some(error) => format!("{validation_error}; {error}"),
                None => validation_error.to_owned(),
            });
        }
        TaskResult {
            index: item.index,
            id: item.id,
            agent: item.agent,
            status,
            output,
            error: final_error,
            usage,
            soft_budget_exhausted,
            structured_output,
            artifact_ref,
            history_ref,
            artifact_uri,
        }
    }

    fn failed_result(
        &self,
        item: &TaskItem,
        job_id: &str,
        status: AgentStatus,
        error: &str,
    ) -> TaskResult {
        let artifact_ref = format!("agent://{}", item.id);
        let history_ref = self.inner.config.catalog.get(&item.agent).map_or_else(
            || format!("history://{}", item.id),
            |definition| {
                if definition.is_persona() {
                    format!("history://persona/{}/{}", definition.name, item.id)
                } else {
                    format!("history://{}", item.id)
                }
            },
        );
        let artifact_uri = format!("artifact://{}", item.id);
        let artifact_path = self
            .inner
            .config
            .artifact_dir
            .join(format!("{}-{job_id}.md", item.id));
        let history_path = self
            .inner
            .config
            .artifact_dir
            .join(format!("{}-{job_id}.history.json", item.id));
        let artifact_error = write_new_artifact(
            &self.inner.storage.artifacts,
            &artifact_path,
            error.as_bytes(),
        )
        .err();
        let artifact_written = artifact_error.is_none();
        let history_error =
            write_new_artifact(&self.inner.storage.artifacts, &history_path, b"[]").err();
        let history_written = history_error.is_none();
        let mut final_error = match artifact_error.or(history_error) {
            Some(write_error) => format!("{error}; {write_error}"),
            None => error.to_owned(),
        };
        if let Err(persist_error) = self.finish_agent(
            &item.id,
            status,
            artifact_written.then_some(artifact_path),
            history_written.then_some(history_path),
        ) {
            final_error = format!("{final_error}; {persist_error}");
        }
        TaskResult {
            index: item.index,
            id: item.id.clone(),
            agent: item.agent.clone(),
            status,
            output: String::new(),
            error: Some(final_error),
            usage: pi_ai::Usage::default(),
            soft_budget_exhausted: false,
            structured_output: None,
            artifact_ref,
            history_ref,
            artifact_uri,
        }
    }

    fn shared_skill_snapshot(&self) -> Arc<Vec<Skill>> {
        Arc::new(
            self.inner
                .config
                .skills
                .iter()
                .map(|skill| Skill {
                    name: skill.name.clone(),
                    description: skill.description.clone(),
                    file_path: skill.file_path.to_string_lossy().into_owned(),
                    base_dir: skill.base_dir.to_string_lossy().into_owned(),
                    globs: skill.globs.clone(),
                    always_apply: skill.always_apply,
                    hidden: skill.hidden,
                    disable_model_invocation: skill.disable_model_invocation,
                    source: skill.source,
                    managed_project_boundary: skill.managed_project_boundary.clone(),
                    managed_root: skill.managed_root.clone(),
                    trusted: skill.trusted,
                })
                .collect(),
        )
    }

    fn persona_skill_context(
        &self,
        definition: &AgentDefinition,
    ) -> Result<(
        Option<crate::PersonaSkillStore>,
        Arc<Vec<crate::PersonaSkill>>,
    )> {
        if !definition.is_persona() {
            return Ok((None, Arc::new(Vec::new())));
        }
        let lease = self
            .inner
            .storage
            .persona_roots
            .get(&definition.name)
            .ok_or_else(|| {
                anyhow!(
                    "persona {:?} has no retained root authority",
                    definition.name
                )
            })?
            .clone();
        let store = crate::PersonaSkillStore::new(lease)?;
        let skills = Arc::new(store.load_persona_skills()?);
        Ok((Some(store), skills))
    }

    fn child_system_prompt(
        &self,
        definition: &AgentDefinition,
        assignment: &str,
        context: Option<&str>,
        output_schema: Option<&Value>,
        schema_mode: Option<&str>,
        peer_roster: &str,
    ) -> Result<String> {
        self.child_system_prompt_with_persona_skills(
            definition,
            assignment,
            context,
            output_schema,
            schema_mode,
            peer_roster,
            &[],
        )
    }

    fn child_system_prompt_with_persona_skills(
        &self,
        definition: &AgentDefinition,
        assignment: &str,
        context: Option<&str>,
        output_schema: Option<&Value>,
        schema_mode: Option<&str>,
        peer_roster: &str,
        persona_skills: &[crate::PersonaSkill],
    ) -> Result<String> {
        let mut prompt = definition.system_prompt.clone();
        if let Some(personality) = definition
            .is_persona()
            .then_some(definition.personality.as_deref())
            .flatten()
            .filter(|personality| !personality.trim().is_empty())
        {
            if personality.len() > MAX_PERSONALITY_BYTES {
                bail!("persona personality exceeds maximum size of {MAX_PERSONALITY_BYTES} bytes");
            }
            prompt.push_str("\n\n<personality>\n");
            prompt.push_str(&escape_xml(personality));
            prompt.push_str("\n</personality>");
        }
        if definition.is_persona() && !definition.knowledge_roots.is_empty() {
            prompt.push_str("\n\n<knowledge_roots>\n");
            for root in &definition.knowledge_roots {
                prompt.push_str("  <root>");
                prompt.push_str(&escape_xml(root));
                prompt.push_str("</root>\n");
            }
            prompt.push_str("Absolute knowledge roots must already be parent workspace roots (`--add-dir`). Placeholders such as `&lt;memory-repo&gt;` are labels only.\n</knowledge_roots>");
        }
        let persona_names = persona_skills
            .iter()
            .map(|skill| skill.name.as_str())
            .collect::<BTreeSet<_>>();
        let visible_shared = self
            .inner
            .config
            .skills
            .iter()
            .filter(|skill| {
                !persona_names.contains(skill.name.as_str())
                    && !skill.hidden
                    && !skill.disable_model_invocation
            })
            .collect::<Vec<_>>();
        if !visible_shared.is_empty() || !persona_skills.is_empty() {
            prompt.push_str("\n\n<available_skills>\n");
            for skill in visible_shared {
                prompt.push_str(&format!(
                    "  <skill><name>{}</name><description>{}</description><location>skill://{}</location></skill>\n",
                    escape_xml(&skill.name),
                    escape_xml(&skill.description),
                    escape_xml(&skill.name),
                ));
            }
            for skill in persona_skills {
                prompt.push_str(&format!(
                    "  <skill><name>{}</name><description>{}</description><location>skill://{}</location></skill>\n",
                    escape_xml(&skill.name),
                    escape_xml(&skill.description),
                    escape_xml(&skill.name),
                ));
            }
            prompt.push_str("</available_skills>");
        }

        let selector_skills = self
            .inner
            .config
            .skills
            .iter()
            .filter(|skill| !persona_names.contains(skill.name.as_str()))
            .map(|skill| Skill {
                name: skill.name.clone(),
                description: skill.description.clone(),
                file_path: skill.file_path.to_string_lossy().into_owned(),
                base_dir: skill.base_dir.to_string_lossy().into_owned(),
                globs: skill.globs.clone(),
                always_apply: skill.always_apply,
                hidden: skill.hidden,
                disable_model_invocation: skill.disable_model_invocation,
                source: skill.source,
                managed_project_boundary: skill.managed_project_boundary.clone(),
                managed_root: skill.managed_root.clone(),
                trusted: skill.trusted,
            })
            .collect::<Vec<_>>();
        let selector_settings = self
            .inner
            .config
            .selector_settings
            .clone()
            .unwrap_or_default();
        let plan = crate::select_deterministic(crate::SelectionInput {
            request: assignment,
            skills: &selector_skills,
            agents: std::slice::from_ref(definition),
            settings: &selector_settings,
        });
        let mut autoload_names = Vec::new();
        let mut seen_autoload = BTreeSet::new();
        for name in definition
            .autoload_skills
            .iter()
            .chain(&plan.autoload_skills)
            .map(String::as_str)
            .chain(persona_skills.iter().map(|skill| skill.name.as_str()))
        {
            if seen_autoload.insert(name) {
                autoload_names.push(name);
            }
        }
        let mut autoload_bytes = 0_usize;
        for name in autoload_names {
            let (skill_name, content) =
                if let Some(skill) = persona_skills.iter().find(|skill| skill.name == name) {
                    (skill.name.as_str(), skill.content.clone())
                } else {
                    let skill = self
                        .inner
                        .config
                        .skills
                        .iter()
                        .find(|skill| skill.name == name)
                        .ok_or_else(|| anyhow!("autoload skill {name:?} is unavailable"))?;
                    let file =
                        self.inner
                            .storage
                            .skill_files
                            .get(&skill.name)
                            .ok_or_else(|| {
                                anyhow!("autoload skill {:?} has no retained file", skill.name)
                            })?;
                    let bytes = file
                        .read_bounded(MAX_AUTOLOAD_SKILL_BYTES)
                        .with_context(|| format!("reading autoload skill {:?}", skill.name))?;
                    let content = String::from_utf8(bytes)
                        .with_context(|| format!("decoding autoload skill {:?}", skill.name))?;
                    (skill.name.as_str(), content)
                };
            autoload_bytes = autoload_bytes.saturating_add(content.len());
            if autoload_bytes > MAX_AUTOLOAD_PROMPT_BYTES {
                bail!(
                    "autoload skills exceed aggregate prompt budget of {MAX_AUTOLOAD_PROMPT_BYTES} bytes"
                );
            }
            prompt.push_str(&format!(
                "\n\n<autoloaded_skill name=\"{}\" location=\"skill://{}\">\n{}\n</autoloaded_skill>",
                escape_xml(skill_name),
                escape_xml(skill_name),
                content
            ));
        }
        prompt.push_str("\n\n");
        prompt.push_str(peer_roster);
        if let Some(context) = context.filter(|text| !text.trim().is_empty()) {
            prompt.push_str(&format!("\n\n<context>\n{context}\n</context>"));
        }
        prompt.push_str(
            "\n\n<delivery_protocol>\nWhen the assigned work is fully complete, call the `yield` tool exactly once with your final deliverable as its `text` argument: that payload becomes your delivered output and ends your session. Do not call `yield` mid-work, and do not do any work after calling it.\n</delivery_protocol>",
        );
        if let Some(output_schema) = output_schema {
            let schema_text = serde_json::to_string_pretty(output_schema)
                .unwrap_or_else(|_| output_schema.to_string());
            prompt.push_str(&format!(
                "\n\n<output_contract>\nYour final `yield` payload must be a single JSON value that validates against this JSON Schema:\n{schema_text}\nValidation mode: {}\n</output_contract>",
                schema_mode.unwrap_or("permissive")
            ));
        }
        prompt.push_str(&format!(
            "\n\n<delegated_assignment>\n{}\n</delegated_assignment>",
            assignment
        ));
        Ok(prompt)
    }

    fn sibling_roster(&self, child_id: &str) -> String {
        const HEADER: &str = "<peer_roster>\nThis is a spawn-time snapshot. `hub list` refreshes state; `hub send` addresses exact ids.\n";
        const TRUNCATED: &str = "  <truncated />\n";
        const FOOTER: &str = "</peer_roster>";

        let main_id = self.main_agent_id();
        let mut peers = self
            .inner
            .registry
            .live_roster(self.group_id(), child_id, main_id);
        let live_count = peers.len();
        if peers.len() > MAX_SIBLING_ROSTER_ENTRIES {
            let main = peers.iter().find(|peer| peer.id == main_id).cloned();
            peers.retain(|peer| peer.id != main_id);
            let main_count = if main.is_some() { 1 } else { 0 };
            peers.truncate(MAX_SIBLING_ROSTER_ENTRIES.saturating_sub(main_count));
            peers.extend(main);
            peers.sort_by(|left, right| left.id.cmp(&right.id));
        }

        let main_line = peers
            .iter()
            .find(|peer| peer.id == main_id)
            .map(render_roster_peer);
        let mut roster = String::with_capacity(MAX_SIBLING_ROSTER_BYTES.min(HEADER.len() + 1024));
        roster.push_str(HEADER);
        let mut main_rendered = false;
        let mut truncated = live_count > peers.len();
        for peer in &peers {
            let line = render_roster_peer(peer);
            let reserve_main = if !main_rendered && peer.id != main_id {
                main_line.as_ref().map_or(0, String::len)
            } else {
                0
            };
            let truncation_bytes = TRUNCATED.len();
            let required = roster
                .len()
                .saturating_add(line.len())
                .saturating_add(reserve_main)
                .saturating_add(truncation_bytes)
                .saturating_add(FOOTER.len());
            if required > MAX_SIBLING_ROSTER_BYTES {
                truncated = true;
                continue;
            }
            roster.push_str(&line);
            main_rendered |= peer.id == main_id;
        }
        if truncated {
            roster.push_str(TRUNCATED);
        }
        roster.push_str(FOOTER);
        debug_assert!(roster.len() <= MAX_SIBLING_ROSTER_BYTES);
        roster
    }

    #[must_use]
    pub fn select_agent(&self, assignment: &str, explicit: Option<&str>) -> String {
        match self.resolve_task_agent(assignment, explicit) {
            Ok(name) => name,
            Err(_) => {
                if let Some(explicit) = explicit.filter(|name| !name.trim().is_empty()) {
                    return explicit.to_owned();
                }
                self.select_ranked_or_default_agent(assignment)
            }
        }
    }

    fn validate_resolved_persona(&self, name: &str) -> Result<()> {
        let Some(definition) = self.inner.config.catalog.get(name) else {
            return Ok(());
        };
        if !definition.is_persona() {
            return Ok(());
        }
        if !self.inner.durable_bound.load(Ordering::Acquire) {
            bail!(
                "persona `{}` requires orchestration to be bound to a durable parent session; enable durable binding before spawning this persona",
                definition.name
            );
        }
        if definition.persona_root().is_none() {
            bail!("persona `{}` has no durable persona root", definition.name);
        }
        let workspace = self
            .inner
            .parent_workspace
            .read()
            .clone()
            .ok_or_else(|| anyhow!(
                "persona `{}` requires the durable parent workspace to be available; rebind the parent session before spawning this persona",
                definition.name
            ))?;
        let missing = definition.missing_knowledge_roots(&workspace);
        if !missing.is_empty() {
            return Err(crate::knowledge_root_membership_error(
                &definition.name,
                &missing,
            ));
        }
        Ok(())
    }

    /// Resolve the agent for a task assignment.
    ///
    /// Precedence: explicit `task.agent` override (validated against the
    /// catalog: missing, disabled, or incompatible definitions fail
    /// actionably instead of falling back), then a unique exact trusted
    /// agent-name delegation — a raw token-stream mention of the agent name
    /// in the assignment via [`crate::selector::exact_agent_mention`], the
    /// single-target contract for typed task content — then ranked
    /// metadata selection / default. Ambiguous delegations return an
    /// error. Typed task content is model-generated and routed raw;
    /// interactive user prompts keep the gated delegation grammar
    /// ([`crate::selector::delegation_agent_mention`]) on the
    /// `pi.agent.mention` / auto-spawn surfaces.
    pub fn resolve_task_agent(&self, assignment: &str, explicit: Option<&str>) -> Result<String> {
        if let Some(explicit) = explicit.filter(|name| !name.trim().is_empty()) {
            if self.inner.config.catalog.get(explicit).is_none() {
                bail!(
                    "explicit agent {:?} is not defined in the agent catalog; \
                     define it as a user agent (~/.rpi/agents) or a project agent in the \
                     current worktree, or remove the explicit agent choice",
                    explicit
                );
            }
            self.ensure_agent_enabled(explicit)?;
            if !self
                .enabled_agents()
                .iter()
                .any(|agent| agent.name == explicit)
            {
                bail!(
                    "explicit agent {:?} is not available for spawning",
                    explicit
                );
            }
            self.validate_resolved_persona(explicit)?;
            return Ok(explicit.to_owned());
        }
        match self.exact_agent_mention_in_catalog(assignment) {
            crate::selector::ExactAgentMention::Unique(name) => {
                self.ensure_agent_enabled(&name)?;
                if !self.enabled_agents().iter().any(|agent| agent.name == name) {
                    bail!(
                        "exact agent mention {:?} is not available for spawning",
                        name
                    );
                }
                self.validate_resolved_persona(&name)?;
                Ok(name)
            }
            crate::selector::ExactAgentMention::Ambiguous(_) => Err(anyhow!(
                "{}",
                self.exact_agent_ambiguity(assignment)
                    .unwrap_or_else(|| { "exact agent mention is ambiguous".to_owned() })
            )),
            crate::selector::ExactAgentMention::None => {
                Ok(self.select_ranked_or_default_agent(assignment))
            }
        }
    }

    /// Catalog diagnostics: fail actionably when `request` (a workflow
    /// objective) explicitly delegates to agent names that are absent from
    /// this catalog or disabled in it, instead of silently degrading to the
    /// default agent. Skill names are exempt (a skill invocation is not an
    /// agent delegation). The grammar is the selector's own delegation
    /// grammar, shared through
    /// [`crate::selector::delegation_agent_mention`]: delegation markers, the
    /// name-initial imperative, CJK constructions, the bare whole-name
    /// address, and the name-leading comma/colon vocative all surface a
    /// catalogued name. The message names the missing/disabled definition
    /// and where workflow agents are discovered.
    pub fn validate_delegation_agents(&self, request: &str) -> Result<()> {
        let catalog_agents = self.inner.config.catalog.agents().clone();
        let skill_names = self
            .skills()
            .iter()
            .map(|skill| skill.name.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        // Known identities are EXACT configured agent and skill names only.
        // Description words are deliberately excluded: they are common
        // language (`content`, `topics`, `code`) and accepting them as
        // identities would recreate the W3 false-positive class.
        let known_identities = catalog_agents
            .iter()
            .map(|agent| normalized_lower(&agent.name))
            .chain(skill_names.iter().map(|name| normalized_lower(name)))
            .collect::<BTreeSet<_>>();
        let mut absent = std::collections::BTreeSet::new();
        let mut disabled = std::collections::BTreeSet::new();
        for candidate in
            delegation_candidates_with_identities(request, &catalog_agents, Some(&known_identities))
        {
            if skill_names
                .iter()
                .any(|name| name.eq_ignore_ascii_case(&candidate))
            {
                continue;
            }
            if let Some(agent) = catalog_agents
                .iter()
                .find(|agent| agent.name.eq_ignore_ascii_case(&candidate))
            {
                if self
                    .inner
                    .config
                    .agent_settings
                    .get(&agent.name)
                    .is_some_and(|settings| settings.enabled == Some(false))
                {
                    disabled.insert(agent.name.clone());
                }
                continue;
            }
            absent.insert(candidate);
        }
        if absent.is_empty() && disabled.is_empty() {
            return Ok(());
        }
        let mut message = String::new();
        if !absent.is_empty() {
            let names = absent
                .iter()
                .map(|name| format!("{name:?}"))
                .collect::<Vec<_>>()
                .join(", ");
            message.push_str(&format!(
                "workflow objective delegates to agent(s) {names} that are not defined in the \
                 workflow agent catalog; define them as user agents under ~/.rpi/agents or as \
                 project agents inside the workflow worktree, or reword the objective"
            ));
        }
        if !disabled.is_empty() {
            if !message.is_empty() {
                message.push_str("; ");
            }
            let names = disabled
                .iter()
                .map(|name| format!("{name:?}"))
                .collect::<Vec<_>>()
                .join(", ");
            message.push_str(&format!(
                "workflow objective delegates to disabled agent(s) {names}; enable them in the \
                 workflow agent settings or reword the objective"
            ));
        }
        bail!("{message}");
    }

    fn select_ranked_or_default_agent(&self, assignment: &str) -> String {
        let enabled_owned = self
            .enabled_agents()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        // An explicit `/role <name> --select` preference wins over ranked
        // selection; it only applies when the role is enabled and compatible.
        if let Some(preferred) = self.inner.preferred_agent.read().as_ref()
            && enabled_owned.iter().any(|agent| agent.name == *preferred)
        {
            return preferred.clone();
        }
        // Cross-kind: unique/ambiguous exact skill mention keeps the configured
        // default rather than promoting an overlapping agent via ranking.
        if !matches!(
            self.exact_skill_mention_in_catalog(assignment),
            crate::selector::ExactSkillMention::None
        ) {
            return self.configured_default_agent(&enabled_owned);
        }
        if let Some(selector) = &self.inner.config.default_agent_selector
            && let Some(selected) = selector(assignment, &enabled_owned)
        {
            return selected;
        }
        self.configured_default_agent(&enabled_owned)
    }

    fn configured_default_agent(&self, enabled: &[AgentDefinition]) -> String {
        if enabled
            .iter()
            .any(|agent| agent.name == self.inner.config.default_agent)
        {
            return self.inner.config.default_agent.clone();
        }
        enabled
            .first()
            .map(|agent| agent.name.clone())
            .unwrap_or_else(|| self.inner.config.default_agent.clone())
    }

    fn exact_skill_mention_in_catalog(
        &self,
        assignment: &str,
    ) -> crate::selector::ExactSkillMention {
        let skills = self
            .inner
            .config
            .skills
            .iter()
            .map(|skill| Skill {
                name: skill.name.clone(),
                description: skill.description.clone(),
                file_path: skill.file_path.to_string_lossy().into_owned(),
                base_dir: skill.base_dir.to_string_lossy().into_owned(),
                globs: skill.globs.clone(),
                always_apply: skill.always_apply,
                hidden: skill.hidden,
                disable_model_invocation: skill.disable_model_invocation,
                source: skill.source,
                managed_project_boundary: skill.managed_project_boundary.clone(),
                managed_root: skill.managed_root.clone(),
                trusted: skill.trusted,
            })
            .collect::<Vec<_>>();
        crate::selector::exact_skill_mention(assignment, &skills)
    }

    /// Raw token-stream exact mention of trusted agent names in `request`
    /// — the single-target contract for typed task assignment. Interactive
    /// user-prompt surfaces gate through
    /// [`crate::selector::delegation_agent_mention`] instead.
    fn exact_agent_mention_in_catalog(
        &self,
        assignment: &str,
    ) -> crate::selector::ExactAgentMention {
        let trusted = self
            .inner
            .config
            .catalog
            .agents()
            .iter()
            .filter(|agent| agent.trusted)
            .cloned()
            .collect::<Vec<_>>();
        crate::selector::exact_agent_mention(assignment, &trusted)
    }

    /// Ordered exact trusted agent names mentioned in `request` (raw-text
    /// scan with CJK-conjunction support), for multi-target delegation.
    fn exact_agent_mentions_in_catalog(&self, request: &str) -> Vec<String> {
        let trusted = self
            .inner
            .config
            .catalog
            .agents()
            .iter()
            .filter(|agent| agent.trusted)
            .cloned()
            .collect::<Vec<_>>();
        crate::selector::exact_agent_mentions(request, &trusted)
    }

    pub(crate) fn exact_agent_ambiguity(&self, assignment: &str) -> Option<String> {
        self.exact_agent_mention_in_catalog(assignment)
            .ambiguity_message()
    }

    pub fn read_uri_resolver(&self) -> crate::InternalUriResolver {
        let runtime = self.clone();
        crate::InternalUriResolver::new(move |uri| runtime.resolve_read_uri_file(uri))
    }

    pub fn resolve_read_uri(&self, uri: &str) -> Result<PathBuf> {
        Ok(self.resolve_read_uri_file(uri)?.display_path())
    }

    fn resolve_read_uri_file(&self, uri: &str) -> Result<Arc<CapabilityFile>> {
        if let Some(reference) = uri.strip_prefix("history://") {
            return Ok(self.resolve_history_file(reference)?.into_file());
        }
        let id = uri
            .strip_prefix("agent://")
            .or_else(|| uri.strip_prefix("artifact://"))
            .ok_or_else(|| anyhow!("unsupported orchestration URI {uri:?}"))?;
        self.resolve_registered_artifact_file(id, false)
    }

    fn open_artifact_path(&self, path: &Path) -> Result<Arc<CapabilityFile>> {
        let relative = path
            .strip_prefix(self.inner.storage.artifacts.display_root())
            .map_err(|_| anyhow!("artifact path escapes orchestration artifact capability"))?;
        self.inner
            .storage
            .artifacts
            .open_file(relative)
            .map(Arc::new)
            .context("opening orchestration artifact through retained authority")
    }

    fn resolve_registered_artifact_file(
        &self,
        id: &str,
        history: bool,
    ) -> Result<Arc<CapabilityFile>> {
        validate_agent_id(id)?;
        let entry = self
            .inner
            .registry
            .get(self.group_id(), id)
            .ok_or_else(|| anyhow!("unknown orchestration agent {id:?}"))?;
        let path = if history {
            entry.history_path.lock().clone()
        } else {
            entry.artifact_path.lock().clone()
        }
        .ok_or_else(|| anyhow!("orchestration artifact for agent {id:?} is not available yet"))?;
        self.open_artifact_path(&path)
    }

    pub fn resolve_agent_reference(&self, id: &str) -> Result<PathBuf> {
        Ok(self
            .resolve_registered_artifact_file(id, false)?
            .display_path())
    }

    pub fn resolve_history_reference(&self, reference: &str) -> Result<PathBuf> {
        Ok(self.resolve_history_file(reference)?.display_path())
    }

    fn resolve_history_file(&self, reference: &str) -> Result<RetainedHistoryFile> {
        if let Some(qualified) = reference.strip_prefix("persona/") {
            let (persona, agent_id) = qualified.split_once('/').ok_or_else(|| {
                anyhow!("persona history URI must be history://persona/<persona>/<agent-id>")
            })?;
            validate_agent_id(persona)?;
            validate_agent_id(agent_id)?;
            return self.resolve_qualified_persona_archive(persona, agent_id);
        }
        validate_agent_id(reference)?;
        let Some(entry) = self.inner.registry.get(self.group_id(), reference) else {
            return self.resolve_persona_archive(reference);
        };
        if let Some(info) = entry.durable_info.lock().clone()
            && let Some(session_path) = info.session_path
        {
            let durable = self.inner.durable.lock().clone().ok_or_else(|| {
                anyhow!("orchestration history for agent {reference:?} is not available yet")
            })?;
            return durable
                .child_file(&session_path)
                .map(|file| RetainedHistoryFile::Session(Arc::new(file)))
                .with_context(|| format!("resolving history for agent {reference:?}"));
        }
        if let Some(path) = entry.history_path.lock().clone() {
            return self
                .open_artifact_path(&path)
                .map(RetainedHistoryFile::Artifact);
        }
        if reference == self.main_agent_id() {
            let durable = self.inner.durable.lock().clone().ok_or_else(|| {
                anyhow!("orchestration history for agent {reference:?} is not available yet")
            })?;
            return durable
                .parent_file()
                .map(|file| RetainedHistoryFile::Session(Arc::new(file)))
                .with_context(|| format!("resolving history for agent {reference:?}"));
        }
        self.resolve_persona_archive(reference)
    }

    pub fn read_child_history(&self, agent_id: &str, lines: usize) -> Result<String> {
        let lines = lines.clamp(1, MAX_HISTORY_LINES);
        let file = self.resolve_history_file(agent_id)?;
        let rendered = render_history_file(&file, lines)
            .with_context(|| format!("rendering history for agent {agent_id:?}"))?;
        let redacted = crate::redact::redact_secrets(&rendered);
        Ok(cap_history_bytes(redacted))
    }

    fn resolve_persona_archive(&self, agent_id: &str) -> Result<RetainedHistoryFile> {
        let relative = Path::new("sessions").join(format!("{agent_id}.jsonl"));
        let mut matches = Vec::new();
        for (persona, root) in &self.inner.storage.persona_roots {
            match root.open_file(&relative) {
                Ok(file) => matches.push((persona.clone(), file)),
                Err(error) if fs_missing(&error) => {}
                Err(error) => return Err(error).context("reading persona history"),
            }
        }
        match matches.len() {
            0 => bail!("unknown orchestration agent {agent_id:?}"),
            1 => Ok(RetainedHistoryFile::Persona(Arc::new(
                matches.pop().expect("one persona history match").1,
            ))),
            _ => bail!(
                "persona history id {agent_id:?} is ambiguous; use history://persona/<persona>/{agent_id}"
            ),
        }
    }

    fn persona_sessions(
        &self,
        persona: &str,
        creation: RootCreation,
    ) -> Result<CapabilityRootLease> {
        let root = self
            .inner
            .storage
            .persona_roots
            .get(persona)
            .ok_or_else(|| anyhow!("unknown persona {persona:?}"))?;
        root.lease_directory(
            Path::new("sessions"),
            CapabilityAccess::PrivateReadWrite,
            creation,
        )
        .context("opening persona sessions capability")
    }

    fn load_persona_continuity(&self, persona: &str) -> Result<Vec<pi_ai::Message>> {
        let sessions = match self.persona_sessions(persona, RootCreation::Existing) {
            Ok(sessions) => sessions,
            Err(error) if error.downcast_ref::<FsError>().is_some_and(fs_missing) => {
                return Ok(Vec::new());
            }
            Err(error) => return Err(error),
        };
        let mut entries = sessions.read_dir(Path::new(""))?;
        let mut archives = Vec::new();
        while let Some(entry) = entries.next()? {
            let name = entry.name().to_os_string();
            if Path::new(&name)
                .extension()
                .and_then(|extension| extension.to_str())
                != Some("jsonl")
            {
                continue;
            }
            let CapabilityEntry::File(file) = entry.into_entry() else {
                continue;
            };
            let modified = file.metadata()?.modified_ns;
            archives.push((modified, name, file));
        }
        archives.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| right.1.cmp(&left.1)));
        let mut newest_to_oldest = Vec::new();
        let mut bytes = 0usize;
        for (_, _, file) in archives {
            let display = file.display_path();
            let tree = crate::session_store::load_session_tree_from_file(
                file.try_clone_std()
                    .context("cloning persona archive handle")?,
                &display,
            )
            .context("loading persona continuity archive")?;
            for message in tree.build_context(None).messages.into_iter().rev() {
                if newest_to_oldest.len() >= PERSONA_CONTINUITY_MAX_MESSAGES {
                    break;
                }
                let message_bytes = serde_json::to_vec(&message)
                    .context("measuring persona continuity message")?
                    .len();
                if bytes.saturating_add(message_bytes) > PERSONA_CONTINUITY_MAX_BYTES {
                    newest_to_oldest.reverse();
                    return Ok(newest_to_oldest);
                }
                bytes += message_bytes;
                newest_to_oldest.push(message);
            }
            if newest_to_oldest.len() >= PERSONA_CONTINUITY_MAX_MESSAGES {
                break;
            }
        }
        newest_to_oldest.reverse();
        Ok(newest_to_oldest)
    }

    fn archive_persona_session(
        &self,
        persona: &str,
        agent_id: &str,
        durable: &super::persistence::DurableRuntime,
        source: &Path,
        replace_existing: bool,
    ) -> Result<PathBuf> {
        validate_agent_id(agent_id)?;
        let source = durable.child_file(source)?;
        let bytes = source
            .read_bounded(MAX_HISTORY_FILE_BYTES)
            .context("reading retained child transcript")?;
        let sessions = self.persona_sessions(persona, RootCreation::CreateDirectory)?;
        let relative = PathBuf::from(format!("{agent_id}.jsonl"));
        if replace_existing {
            let expected = match sessions.metadata(&relative) {
                Ok(metadata) if metadata.identity.kind() == EntryKind::RegularFile => {
                    Some(metadata.identity)
                }
                Err(error) if fs_missing(&error) => None,
                Ok(_) => bail!("existing persona archive is not a regular file"),
                Err(error) => return Err(error).context("reading persona archive identity"),
            };
            sessions.replace_file(Replacement {
                relative: relative.clone(),
                expected,
                bytes,
                mode: FileMode::Private,
            })?;
        } else {
            let mut destination = sessions
                .create_new(&relative, FileMode::Private)
                .context("creating persona archive")?;
            destination.write_all(&bytes)?;
            destination.finish()?;
        }
        Ok(sessions.display_root().join(relative))
    }

    fn resolve_qualified_persona_archive(
        &self,
        persona: &str,
        agent_id: &str,
    ) -> Result<RetainedHistoryFile> {
        let root = self
            .inner
            .storage
            .persona_roots
            .get(persona)
            .ok_or_else(|| anyhow!("unknown persona {persona:?}"))?;
        root.open_file(&Path::new("sessions").join(format!("{agent_id}.jsonl")))
            .map(|file| RetainedHistoryFile::Persona(Arc::new(file)))
            .with_context(|| format!("resolving persona history {persona:?}/{agent_id:?}"))
    }
    /// Claim a persona for reset/removal after proving it has no queued or
    /// running jobs and no active registry entry.
    ///
    /// The returned guard must remain alive through the destructive operation
    /// (and any catalog reload). New spawns and revivals for this persona are
    /// rejected until the guard is dropped.
    pub fn begin_persona_destructive_operation(
        &self,
        persona: &str,
    ) -> Result<PersonaLifecycleGuard> {
        let _spawn_guard = self.inner.jobs.lock_spawns();
        let _durable_mutation = self.inner.durable_mutation.lock();
        let definition = self
            .inner
            .config
            .catalog
            .get(persona)
            .filter(|definition| definition.is_persona())
            .ok_or_else(|| anyhow!("unknown persona {persona:?}"))?;
        let mut blocked = self.inner.persona_lifecycle_blocks.lock();
        if blocked.contains(&definition.name) {
            bail!("persona {persona:?} already has a destructive operation in progress");
        }
        let active_ids = self
            .inner
            .active
            .lock()
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        let registry_active = self
            .inner
            .registry
            .list(self.group_id(), self.main_agent_id())
            .into_iter()
            .any(|snapshot| {
                snapshot.agent == definition.name
                    && (active_ids.contains(&snapshot.id)
                        || matches!(snapshot.status, AgentStatus::Queued | AgentStatus::Running))
            });
        let job_active = self
            .inner
            .jobs
            .snapshots(None)
            .iter()
            .any(|job| job.agent == definition.name && !job.status.is_settled());
        if registry_active || job_active {
            bail!("persona {persona:?} is in use by an active orchestration job");
        }
        blocked.insert(definition.name.clone());
        Ok(PersonaLifecycleGuard {
            inner: self.inner.clone(),
            persona: definition.name.clone(),
            release_on_drop: true,
        })
    }

    #[must_use]
    pub fn jobs(&self, ids: Option<&[String]>) -> Vec<JobSnapshot> {
        self.prune_retained_jobs();
        self.inner.jobs.snapshots(ids)
    }

    pub async fn wait_jobs(
        &self,
        ids: &[String],
        timeout: Option<Duration>,
        abort: Option<pi_agent::AbortSignal>,
    ) -> Result<Vec<JobSnapshot>> {
        self.prune_retained_jobs();
        self.inner
            .jobs
            .wait(ids, timeout, abort, self.inner.shutdown.clone())
            .await
    }

    /// Entry to the atomic retirement protocol shared with
    /// `Application::commit_orchestration_candidate`: hold this spawn gate
    /// while checking unsettled jobs and marking the runtime retiring, so no
    /// spawn can register between the active-job check and the replacement.
    pub(crate) fn lock_spawns_for_retirement(&self) -> parking_lot::MutexGuard<'_, ()> {
        self.inner.jobs.lock_spawns()
    }

    /// Marks the runtime retiring under the held spawn gate: every
    /// stale-handle `spawn_tasks` from here on is rejected before creating an
    /// authoritative child. Terminal shutdown and checkpoint paths are
    /// unaffected — they keep their own authority.
    pub(crate) fn begin_retirement(&self) {
        self.inner.retiring.store(true, Ordering::Release);
    }

    /// Test-only: arms a parking hook at `point` inside [`spawn_tasks`]. The
    /// parked spawn blocks its thread on `release` (event flag, no sleeps) so
    /// the reload-commit race tests can interleave deterministically at the
    /// exact check/registration boundary.
    #[cfg(test)]
    pub(crate) fn arm_spawn_test_park(
        &self,
        point: SpawnTestParkPoint,
        release: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        *self.inner.spawn_test_park.lock() = Some((point, release));
    }

    pub fn cancel_jobs_result(&self, ids: &[String]) -> Result<Vec<String>> {
        let (cancelled, snapshots) =
            {
                let _durable_mutation = self.inner.durable_mutation.lock();
                if self.inner.rebind_reserved.load(Ordering::Acquire) {
                    bail!("durable orchestration rebind is in progress");
                }
                if self.inner.durable_frozen.load(Ordering::Acquire) {
                    bail!("restart-checkpointed orchestration cannot cancel jobs");
                }
                let mut prepared = self.inner.jobs.prepare_cancellation(ids, "task cancelled");
                if self.inner.durable_bound.load(Ordering::Acquire) {
                    let durable =
                        self.inner.durable.lock().clone().ok_or_else(|| {
                            anyhow!("durable orchestration binding is unavailable")
                        })?;
                    let jobs = prepared.take_snapshots();
                    durable.persist(&super::persistence::build_state(
                        self.group_id(),
                        durable.parent_session_id(),
                        durable.parent_session_path(),
                        self.durable_policy_state()?,
                        self.collect_main_mailbox(),
                        self.delivered_messages(),
                        self.collect_persisted_agents(),
                        jobs,
                    ))?;
                } else {
                    prepared.take_snapshots();
                }
                let cancelled = self.inner.jobs.commit_cancellation(prepared);
                let snapshots = self.inner.jobs.snapshots(Some(&cancelled));
                (cancelled, snapshots)
            };
        for job in snapshots {
            self.publish_job(job);
        }
        Ok(cancelled)
    }
    pub fn cancel_jobs(&self, ids: &[String]) -> Vec<String> {
        self.cancel_jobs_result(ids).unwrap_or_default()
    }

    pub fn cancel_active(&self) {
        let tokens = self
            .inner
            .active
            .lock()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for token in tokens {
            token.cancel();
        }
    }

    #[must_use]
    pub fn is_shutdown(&self) -> bool {
        self.inner.shutdown.is_cancelled()
    }

    pub async fn shutdown(&self) -> Result<()> {
        if self.is_shutdown() {
            return Ok(());
        }
        if self.inner.durable_frozen.load(Ordering::Acquire) {
            bail!("restart-checkpointed orchestration cannot enter terminal shutdown");
        }
        let cancelled_jobs =
            {
                let _spawn_guard = self.inner.jobs.lock_spawns();
                let _durable_mutation = self.inner.durable_mutation.lock();
                if self.inner.durable_frozen.load(Ordering::Acquire) {
                    bail!("restart-checkpointed orchestration cannot enter terminal shutdown");
                }
                if self.inner.rebind_reserved.load(Ordering::Acquire) {
                    bail!("durable orchestration rebind is in progress");
                }
                let ids = self
                    .inner
                    .jobs
                    .snapshots(None)
                    .into_iter()
                    .filter(|job| !job.status.is_settled())
                    .map(|job| job.id)
                    .collect::<Vec<_>>();
                let mut prepared = self
                    .inner
                    .jobs
                    .prepare_cancellation(&ids, "orchestration shut down");
                let cancelled_ids = prepared.cancelled_ids().to_vec();
                let jobs = prepared.take_snapshots();
                let cancelled_jobs = jobs
                    .iter()
                    .filter(|job| cancelled_ids.contains(&job.id))
                    .cloned()
                    .collect::<Vec<_>>();
                if self.inner.durable_bound.load(Ordering::Acquire) {
                    let durable =
                        self.inner.durable.lock().clone().ok_or_else(|| {
                            anyhow!("durable orchestration binding is unavailable")
                        })?;
                    let cancelled_agents = cancelled_jobs
                        .iter()
                        .map(|job| job.agent_id.as_str())
                        .collect::<std::collections::HashSet<_>>();
                    let mut agents = self.collect_persisted_agents();
                    for agent in &mut agents {
                        if cancelled_agents.contains(agent.snapshot.id.as_str()) {
                            agent.snapshot.status = AgentStatus::Aborted;
                            agent.snapshot.last_activity = now_millis();
                        }
                    }

                    durable.persist(&super::persistence::build_state(
                        self.group_id(),
                        durable.parent_session_id(),
                        durable.parent_session_path(),
                        self.durable_policy_state()?,
                        self.collect_main_mailbox(),
                        self.delivered_messages(),
                        agents,
                        jobs,
                    ))?;
                }
                self.inner.jobs.commit_cancellation(prepared);
                for job in &cancelled_jobs {
                    let _ = self.inner.registry.set_status(
                        self.group_id(),
                        &job.agent_id,
                        AgentStatus::Aborted,
                    );
                }
                self.begin_retirement();
                cancelled_jobs
            };
        for job in cancelled_jobs {
            if let Some(job) = self
                .inner
                .jobs
                .snapshots(Some(std::slice::from_ref(&job.id)))
                .into_iter()
                .next()
            {
                self.publish_job(job);
            }
        }
        let park_handles = self
            .inner
            .park_timers
            .lock()
            .drain()
            .map(|(_, handle)| handle)
            .collect::<Vec<_>>();
        for handle in park_handles {
            handle.abort();
        }
        self.wait_until_idle().await;
        {
            let _durable_mutation = self.inner.durable_mutation.lock();
            self.persist_state()
                .context("persisting terminal orchestration shutdown")?;
        }
        log_shutdown_cancel_site(
            "explicit_shutdown",
            self.inner.group_id.get().map(String::as_str),
        );
        self.inner.shutdown.cancel();
        self.cleanup_retained_jobs();
        self.inner.registry.remove_group(self.group_id());
        self.remove_review_artifact_tree()?;
        Ok(())
    }

    fn remove_review_artifact_tree(&self) -> Result<()> {
        if self.main_agent_id() != CODE_REVIEW_PARENT_AGENT_ID {
            return Ok(());
        }
        let artifact_dir = &self.inner.config.artifact_dir;
        match std::fs::remove_dir_all(artifact_dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).context("removing review orchestration artifact directory");
            }
        }
        let Some(review_dir) = artifact_dir.parent() else {
            return Ok(());
        };
        match std::fs::remove_dir(review_dir) {
            Ok(()) => Ok(()),
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    || error.kind() == std::io::ErrorKind::DirectoryNotEmpty =>
            {
                Ok(())
            }
            Err(error) => Err(error).context("removing review orchestration directory"),
        }
    }

    pub fn establish_group_id(&self, group_id: &str) -> Result<()> {
        if group_id.is_empty() {
            bail!("orchestration group id cannot be empty");
        }
        self.initialize_group_id(group_id)
            .map(|_| ())
            .map_err(|established| {
                anyhow!("orchestration group id mismatch: runtime {established:?}, requested {group_id:?}")
            })
    }

    pub fn create_unbound_review_runtime(
        &self,
        review_id: &str,
        parent: &crate::Session,
    ) -> Result<Self> {
        let mut config = self.inner.config.clone();
        config.main_agent_id = CODE_REVIEW_PARENT_AGENT_ID.to_owned();
        config.artifact_dir = parent
            .cwd()
            .join(crate::resources::CONFIG_DIR_NAME)
            .join("review")
            .join(review_id)
            .join("artifacts");
        config.idle_ttl = None;
        config.parent_model_provider = None;
        let snapshot = parent.child_session_options_snapshot();
        config.parent_model = snapshot.model.clone();
        let factory = Self::child_factory_from_snapshot(snapshot);
        let runtime = Self::new(config, factory)?;
        runtime.establish_group_id(review_id)?;
        Ok(runtime)
    }
}
fn persist_interrupted_tool_results(
    path: &Path,
    durable: &super::persistence::DurableRuntime,
) -> Result<()> {
    let file = durable.child_file(path)?;
    let display = file.display_path();
    let tree = crate::session_store::load_session_tree_from_file(
        file.try_clone_std()
            .context("cloning child transcript handle")?,
        &display,
    )
    .context("loading child transcript for interrupted tool reconciliation")?;
    let messages = tree.build_context(None).messages;
    let mut last_tool_assistant = None;
    for (index, message) in messages.iter().enumerate() {
        if let pi_ai::Message::Assistant(assistant) = message
            && assistant
                .content
                .iter()
                .any(|block| matches!(block, pi_ai::ContentBlock::ToolCall(_)))
        {
            last_tool_assistant = Some(index);
        }
    }
    let Some(index) = last_tool_assistant else {
        return Ok(());
    };
    if messages[index + 1..]
        .iter()
        .any(|message| matches!(message, pi_ai::Message::Assistant(_)))
    {
        return Ok(());
    }
    let mut found = std::collections::HashSet::new();
    for message in &messages[index + 1..] {
        if let pi_ai::Message::ToolResult(result) = message {
            found.insert(result.tool_call_id.clone());
        }
    }
    let missing: Vec<(String, String)> = match &messages[index] {
        pi_ai::Message::Assistant(assistant) => assistant
            .content
            .iter()
            .filter_map(|block| match block {
                pi_ai::ContentBlock::ToolCall(call) if !found.contains(&call.id) => {
                    Some((call.id.clone(), call.name.clone()))
                }
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    if missing.is_empty() {
        return Ok(());
    }
    let timestamp = now_millis() as i64;
    let mut extra = Vec::new();
    let mut parent_id = tree.leaf_id.clone();
    for (tool_call_id, tool_name) in missing {
        let entry_id = format!("interrupted-{tool_call_id}");
        extra.push(serde_json::json!({
            "type": "message",
            "id": entry_id,
            "parentId": parent_id,
            "timestamp": "1970-01-01T00:00:00.000Z",
            "message": pi_ai::Message::ToolResult(pi_ai::ToolResultMessage {
                tool_call_id: tool_call_id.clone(),
                tool_name,
                content: vec![pi_ai::ContentBlock::text(
                    "tool interrupted by process restart; the original call was not replayed",
                )],
                usage: None,
                details: None,
                added_tool_names: Vec::new(),
                is_error: true,
                timestamp,
            }),
        }));
        parent_id = Some(entry_id);
    }
    let (root, relative) = durable.child_file_authority(path)?;
    let mut bytes = root
        .open_file(&relative)
        .context("opening child transcript for interrupted tool append")?
        .read_bounded(MAX_HISTORY_FILE_BYTES)
        .context("reading child transcript for interrupted tool append")?;
    if !bytes.is_empty() && bytes.last() != Some(&b'\n') {
        bytes.push(b'\n');
    }
    for entry in extra {
        bytes.extend_from_slice(entry.to_string().as_bytes());
        bytes.push(b'\n');
    }
    let expected = match root.metadata(&relative) {
        Ok(metadata) if metadata.identity.kind() == EntryKind::RegularFile => {
            Some(metadata.identity)
        }
        Ok(_) => bail!("child transcript is not a regular file"),
        Err(error) => return Err(error).context("reading child transcript identity"),
    };
    root.replace_file(Replacement {
        relative,
        expected,
        bytes,
        mode: FileMode::Private,
    })
    .context("appending interrupted tool results to child transcript")?;
    Ok(())
}

/// Structural scan of a settled child run's transcript for the FINAL tool
/// outcome. Returns a bounded kind label — the failing tool's name, or
/// `"bash"` — when the last actual `ToolResult`/`BashExecution` failed and no
/// later tool result succeeded, or `None` when the run ended on a successful
/// tool result.
///
/// Failures are structural, never content-derived: `ToolResult.is_error`, or
/// a `BashExecution` with a non-zero exit code or `cancelled`. Assistant and
/// user text, custom messages and summaries are ignored — prose after a
/// failed tool can never mask the failure — and a subsequent successful tool
/// result clears an earlier failure (a child that recovered is not failed).
///
/// Denied/unavailable attempts are structural too, and are NOT failures: an
/// error `ToolResult` for a tool name outside `available_tools` (the child's
/// own role-filtered tool set — e.g. a read-only child calling `write`) stays
/// visible in the transcript as an `is_error` result but neither fails the
/// run nor clears an earlier real failure. Only error results for tools the
/// child actually possesses — real execution errors and strict schema errors
/// — are authoritative failures. `BashExecution` outcomes are unaffected: a
/// child that runs bash necessarily possesses it, so non-zero/cancelled bash
/// always fails.
///
/// The returned label is bounded and contains no tool output, so it is safe
/// to surface on the job wire.
fn last_failed_tool_kind(
    messages: &[pi_ai::Message],
    available_tools: &BTreeSet<String>,
) -> Option<String> {
    let mut failed: Option<String> = None;
    for message in messages {
        match message {
            pi_ai::Message::ToolResult(result) => {
                if !result.is_error {
                    failed = None;
                } else if available_tools.contains(&result.tool_name) {
                    failed = Some(result.tool_name.clone());
                }
            }
            pi_ai::Message::BashExecution(bash) => {
                failed = if bash.cancelled || bash.exit_code.is_some_and(|code| code != 0) {
                    Some("bash".to_owned())
                } else {
                    None
                };
            }
            pi_ai::Message::User(_)
            | pi_ai::Message::Assistant(_)
            | pi_ai::Message::Custom(_)
            | pi_ai::Message::BranchSummary(_)
            | pi_ai::Message::CompactionSummary(_) => {}
        }
    }
    failed
}

/// Debug-only forensics for the three shutdown-token cancellation sites
/// (explicit [`OrchestrationRuntime::shutdown`], owner drop, final inner
/// drop). Enabled exclusively by `RPI_DEBUG_SHUTDOWN_TOKEN`; identity is
/// truncated and the captured backtrace pinpoints the production caller that
/// cancelled the token. Never part of the error contract.
fn log_shutdown_cancel_site(site: &'static str, group_id: Option<&str>) {
    if std::env::var_os("RPI_DEBUG_SHUTDOWN_TOKEN").is_none() {
        return;
    }
    let group = group_id.map(|id| id.get(..11.min(id.len())).unwrap_or(id));
    let backtrace = std::backtrace::Backtrace::force_capture();
    eprintln!(
        "orchestration: debug shutdown-token cancelled site={site} group={group:?} backtrace={backtrace:#}"
    );
}

struct ActiveChildGuard {
    inner: Arc<RuntimeInner>,
    id: String,
    group_id: String,
}

impl Drop for ActiveChildGuard {
    fn drop(&mut self) {
        let _durable_mutation = self.inner.durable_mutation.lock();
        self.inner
            .registry
            .unregister_active_delivery(&self.group_id, &self.id);
        self.inner.active.lock().remove(&self.id);
        self.inner.active_changed.notify_waiters();
    }
}

impl Drop for OrchestrationRuntime {
    fn drop(&mut self) {
        if !self.owner || self.inner.durable_frozen.load(Ordering::Acquire) {
            return;
        }
        log_shutdown_cancel_site("owner_drop", self.inner.group_id.get().map(String::as_str));
        self.inner.shutdown.cancel();
        for token in self.inner.active.lock().values() {
            token.cancel();
        }
        for handle in self
            .inner
            .park_timers
            .lock()
            .drain()
            .map(|(_, handle)| handle)
        {
            handle.abort();
        }
        self.cleanup_retained_jobs();
        self.inner.registry.remove_group(self.group_id());
    }
}

impl Drop for RuntimeInner {
    fn drop(&mut self) {
        if *self.durable_frozen.get_mut() {
            return;
        }
        log_shutdown_cancel_site("inner_drop", self.group_id.get().map(String::as_str));
        self.shutdown.cancel();
        for token in self.active.get_mut().values() {
            token.cancel();
        }
        for handle in self.park_timers.get_mut().drain().map(|(_, handle)| handle) {
            handle.abort();
        }
        for candidate in self.jobs.settled_candidates() {
            let artifact_path = self
                .config
                .artifact_dir
                .join(format!("{}-{}.md", candidate.agent_id, candidate.job_id));
            let history_path = self.config.artifact_dir.join(format!(
                "{}-{}.history.json",
                candidate.agent_id, candidate.job_id
            ));
            let _ = remove_retained_file(&self.storage.artifacts, &artifact_path);
            let _ = remove_retained_file(&self.storage.artifacts, &history_path);
        }
        if let Some(group_id) = self.group_id.get() {
            self.registry.remove_group(group_id);
        }
    }
}

impl GlobalRegistry {
    fn register_main(&self, group: &str, snapshot: AgentSnapshot, mailbox_capacity: usize) {
        self.register(group, snapshot, mailbox_capacity)
            .expect("generated orchestration main identity is valid");
    }

    fn register(
        &self,
        group: &str,
        snapshot: AgentSnapshot,
        mailbox_capacity: usize,
    ) -> Result<()> {
        self.register_batch(group, vec![snapshot], mailbox_capacity)
    }

    fn register_batch(
        &self,
        group: &str,
        snapshots: Vec<AgentSnapshot>,
        mailbox_capacity: usize,
    ) -> Result<()> {
        let mut batch_ids = std::collections::BTreeSet::new();
        for snapshot in &snapshots {
            validate_agent_id(&snapshot.id)?;
            if let Some(parent_id) = snapshot.parent_id.as_deref() {
                validate_agent_id(parent_id)
                    .with_context(|| format!("invalid parent agent id {parent_id:?}"))?;
            }
            if !batch_ids.insert(snapshot.id.as_str()) {
                bail!(
                    "orchestration agent id {:?} appears more than once in batch",
                    snapshot.id
                );
            }
        }

        let mut groups = self.groups.lock();
        if let Some(entries) = groups.get(group)
            && let Some(snapshot) = snapshots
                .iter()
                .find(|snapshot| entries.contains_key(&snapshot.id))
        {
            bail!(
                "orchestration agent id {:?} is already registered",
                snapshot.id
            );
        }
        let entries = groups.entry(group.to_owned()).or_default();
        for snapshot in snapshots {
            entries.insert(
                snapshot.id.clone(),
                Arc::new(AgentEntry {
                    snapshot: Mutex::new(snapshot),
                    mailbox: Mutex::new(VecDeque::new()),
                    mailbox_capacity,
                    message_ready: Notify::new(),
                    active_delivery: Mutex::new(None),
                    waiters: Mutex::new(Vec::new()),
                    waiter_seq: AtomicU64::new(0),
                    cancellation: Mutex::new(None),
                    idle_park_token: Mutex::new(None),
                    artifact_path: Mutex::new(None),
                    history_path: Mutex::new(None),
                    durable_info: Mutex::new(None),
                }),
            );
        }
        Ok(())
    }
    fn retain_only(&self, group: &str, id: &str) {
        let mut groups = self.groups.lock();
        let Some(entries) = groups.get_mut(group) else {
            return;
        };
        entries.retain(|entry_id, _| entry_id == id);
    }

    fn install_prepared_group(&self, group: &str, prepared: HashMap<String, Arc<AgentEntry>>) {
        let mut groups = self.groups.lock();
        if let Some(entries) = groups.get_mut(group) {
            *entries = prepared;
        } else {
            groups.insert(group.to_owned(), prepared);
        }
    }

    fn unregister_batch(&self, group: &str, ids: &[String]) {
        let mut groups = self.groups.lock();
        let Some(entries) = groups.get_mut(group) else {
            return;
        };
        for id in ids {
            entries.remove(id);
        }
        if entries.is_empty() {
            groups.remove(group);
        }
    }

    fn get(&self, group: &str, id: &str) -> Option<Arc<AgentEntry>> {
        self.groups
            .lock()
            .get(group)
            .and_then(|entries| entries.get(id))
            .cloned()
    }

    fn list(&self, group: &str, caller_id: &str) -> Vec<AgentSnapshot> {
        let groups = self.groups.lock();
        let Some(entries) = groups.get(group) else {
            return Vec::new();
        };
        let mut snapshots = entries
            .values()
            .filter_map(|entry| {
                let mut snapshot = entry.snapshot.lock().clone();
                (snapshot.id != caller_id).then(|| {
                    snapshot.unread = entry.mailbox.lock().len();
                    snapshot
                })
            })
            .collect::<Vec<_>>();
        snapshots.sort_by(|left, right| left.id.cmp(&right.id));
        snapshots
    }

    fn live_roster(&self, group: &str, child_id: &str, main_id: &str) -> Vec<AgentSnapshot> {
        let groups = self.groups.lock();
        let Some(entries) = groups.get(group) else {
            return Vec::new();
        };
        let mut snapshots = Vec::with_capacity(entries.len().min(MAX_SIBLING_ROSTER_ENTRIES + 1));
        for entry in entries.values() {
            let snapshot = entry.snapshot.lock();
            if snapshot.id != child_id
                && (snapshot.id == main_id
                    || matches!(snapshot.status, AgentStatus::Queued | AgentStatus::Running))
            {
                snapshots.push(snapshot.clone());
            }
        }
        snapshots.sort_by(|left, right| left.id.cmp(&right.id));
        snapshots
    }
    fn enqueue(
        &self,
        group: &str,
        target: &str,
        message: MailboxMessage,
    ) -> Result<DeliveryOutcome> {
        let entry = self
            .get(group, target)
            .ok_or_else(|| anyhow!("unknown orchestration agent {target:?}"))?;
        let outcome = {
            if entry
                .cancellation
                .lock()
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled)
            {
                bail!("orchestration agent {target:?} is cancelling");
            }
            let mut snapshot = entry.snapshot.lock();
            if snapshot.status == AgentStatus::Aborted {
                bail!("orchestration agent {target:?} is aborted");
            }
            snapshot.last_activity = now_millis();
            match snapshot.status {
                AgentStatus::Running => {
                    let mut mailbox = entry.mailbox.lock();
                    if mailbox.len() >= entry.mailbox_capacity {
                        bail!("orchestration mailbox for {target:?} is full");
                    }
                    mailbox.push_back(message);
                    DeliveryOutcome::Woken
                }
                AgentStatus::Queued | AgentStatus::Parked | AgentStatus::Idle => {
                    let mut mailbox = entry.mailbox.lock();
                    if mailbox.len() >= entry.mailbox_capacity {
                        bail!("orchestration mailbox for {target:?} is full");
                    }
                    mailbox.push_back(message);
                    DeliveryOutcome::Queued
                }
                AgentStatus::Aborted => DeliveryOutcome::Failed,
            }
        };
        if outcome == DeliveryOutcome::Woken {
            if let Some(token) = entry.idle_park_token.lock().take() {
                token.cancel();
            }
        }
        entry.message_ready.notify_waiters();
        Ok(outcome)
    }

    /// Atomically claim a committed mailbox message for the first matching
    /// active `hub wait`: the message leaves the mailbox and is handed to the
    /// waiter's delivery channel, so the claim is the actual consumption — a
    /// pre-check could never guarantee the waiter drains it. The waiter is
    /// one-shot: it is removed from the registry in the same critical section
    /// as the handoff, so a burst of sends hands exactly one message to the
    /// waiter and later sends stay in the bounded mailbox. Returns true only
    /// when a waiter actually received the message.
    fn hand_to_waiter(&self, group: &str, target: &str, message: &MailboxMessage) -> bool {
        let Some(entry) = self.get(group, target) else {
            return false;
        };
        self.hand_to_waiter_entry(&entry, message)
    }

    fn hand_to_waiter_entry(&self, entry: &AgentEntry, message: &MailboxMessage) -> bool {
        // Lock order: waiters → mailbox (matches the waiter-guard Drop).
        let mut waiters = entry.waiters.lock();
        let Some(index) = waiters
            .iter()
            .position(|waiter| waiter_matches(waiter, message))
        else {
            return false;
        };
        let mut mailbox = entry.mailbox.lock();
        let Some(mail_index) = mailbox.iter().position(|queued| queued.id == message.id) else {
            return false;
        };
        let message = mailbox.remove(mail_index).expect("position checked");
        drop(mailbox);
        // One-shot claim: remove the waiter so a send burst cannot flood its
        // unbounded channel (later sends remain mailbox-bounded) and no
        // second handoff can race the guard's restore.
        let waiter = waiters.remove(index);
        drop(waiters);
        if waiter.delivery.send(message.clone()).is_err() {
            // The waiter's task ended between the lookup and the send: put the
            // message back so it is never lost.
            let mut mailbox = entry.mailbox.lock();
            mailbox.retain(|queued| queued.id != message.id);
            mailbox.push_front(message);
            entry.message_ready.notify_waiters();
            return false;
        }
        true
    }

    /// Deliver a committed mailbox message to an active recipient, reporting
    /// exactly what happened so `send` can return a truthful receipt.
    fn deliver_committed(&self, group: &str, target: &str, message_id: &str) -> CommittedDelivery {
        let Some(entry) = self.get(group, target) else {
            return CommittedDelivery::Retained;
        };
        let message = entry
            .mailbox
            .lock()
            .iter()
            .find(|message| message.id == message_id)
            .cloned();
        let Some(message) = message else {
            return CommittedDelivery::Retained;
        };
        // An explicit `hub wait` takes precedence over the active steering
        // bridge: atomically hand the message to the waiter (removing it from
        // the mailbox) so `wait_message` returns it as its own result.
        if self.hand_to_waiter_entry(&entry, &message) {
            return CommittedDelivery::WaiterClaimed;
        }
        match entry.active_delivery.lock().as_ref() {
            Some(sender) if sender.send(message).is_ok() => CommittedDelivery::Delivered,
            // No active bridge (e.g. the run settled between the enqueue and
            // this call): the message stays in the mailbox and the next
            // run/revival delivers it.
            _ => CommittedDelivery::Retained,
        }
    }

    fn register_active_delivery(
        &self,
        group: &str,
        id: &str,
        sender: tokio::sync::mpsc::UnboundedSender<MailboxMessage>,
        cancellation: CancellationToken,
    ) -> Result<Vec<MailboxMessage>> {
        let entry = self
            .get(group, id)
            .ok_or_else(|| anyhow!("unknown orchestration agent {id:?}"))?;
        let mut active = entry.active_delivery.lock();
        if active.is_some() {
            bail!("orchestration agent {id:?} already has an active delivery bridge");
        }
        let messages = entry.mailbox.lock().iter().cloned().collect();
        *active = Some(sender);
        *entry.cancellation.lock() = Some(cancellation);
        Ok(messages)
    }

    fn unregister_active_delivery(&self, group: &str, id: &str) {
        if let Some(entry) = self.get(group, id) {
            entry.active_delivery.lock().take();
            entry.cancellation.lock().take();
        }
    }

    /// Register an explicit `hub wait` interest tied to `id`'s mailbox.
    ///
    /// The returned guard unregisters on drop, so a waiter can never strand a
    /// message after its `wait_message` returns, times out, is cancelled, or
    /// the owning task is dropped. `from == None` registers a wildcard claim
    /// (matches every sender); otherwise only that exact sender is claimed.
    /// Matching sends are atomically handed to the waiter's delivery channel
    /// (removed from the mailbox). `republish` fires only when this wait ends
    /// without returning an already-handed-off message (the message is
    /// restored to the mailbox), letting the caller re-surface Main-target
    /// deliveries so the application still steers them into the main session.
    fn register_message_waiter(
        &self,
        group: &str,
        id: &str,
        from: Option<String>,
        reply_to: Option<String>,
        republish: Option<Arc<dyn Fn(MailboxMessage) + Send + Sync>>,
    ) -> Option<MessageWaiterGuard> {
        let entry = self.get(group, id)?;
        let token = entry.waiter_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let (delivery, delivery_rx) = tokio::sync::mpsc::unbounded_channel();
        entry.waiters.lock().push(MessageWaiter {
            token,
            from,
            reply_to,
            delivery,
        });
        Some(MessageWaiterGuard {
            entry,
            token,
            delivery_rx,
            republish,
        })
    }

    fn park_if_idle(&self, group: &str, id: &str, main_id: &str) -> Option<AgentSnapshot> {
        if id == main_id {
            return None;
        }
        let entry = self.get(group, id)?;
        let mut snapshot = entry.snapshot.lock();
        if snapshot.status != AgentStatus::Idle {
            return None;
        }
        snapshot.status = AgentStatus::Parked;
        snapshot.last_activity = now_millis();
        Some(snapshot.clone())
    }

    fn inbox(&self, group: &str, id: &str, peek: bool) -> Vec<MailboxMessage> {
        let Some(entry) = self.get(group, id) else {
            return Vec::new();
        };
        let mut mailbox = entry.mailbox.lock();
        if peek {
            mailbox.iter().cloned().collect()
        } else {
            mailbox.drain(..).collect()
        }
    }

    fn remove_message(&self, group: &str, id: &str, message_id: &str) -> bool {
        let Some(entry) = self.get(group, id) else {
            return false;
        };
        let mut mailbox = entry.mailbox.lock();
        let Some(index) = mailbox.iter().position(|message| message.id == message_id) else {
            return false;
        };
        mailbox.remove(index);
        true
    }
    fn remove_message_value(
        &self,
        group: &str,
        id: &str,
        message_id: &str,
    ) -> Option<MailboxMessage> {
        let entry = self.get(group, id)?;
        let mut mailbox = entry.mailbox.lock();
        let index = mailbox
            .iter()
            .position(|message| message.id == message_id)?;
        mailbox.remove(index)
    }

    /// Re-insert a previously committed mailbox message. The restore reverses
    /// an earlier removal (bridge ack or waiter handoff), so it is never
    /// rejected by the mailbox bound — the slot the in-flight message occupied
    /// is being returned, even if other sends refilled it meanwhile. A
    /// duplicate of the same message id is replaced (never doubled), and the
    /// restored message returns to the front of the queue so the next
    /// run/revival/waiter sees it first.
    fn restore_mailbox_message(&self, group: &str, id: &str, message: MailboxMessage) -> bool {
        let Some(entry) = self.get(group, id) else {
            return false;
        };
        let mut mailbox = entry.mailbox.lock();
        mailbox.retain(|queued| queued.id != message.id);
        mailbox.push_front(message);
        entry.message_ready.notify_waiters();
        true
    }

    fn set_status(&self, group: &str, id: &str, status: AgentStatus) -> Result<()> {
        let entry = self
            .get(group, id)
            .ok_or_else(|| anyhow!("unknown orchestration agent {id:?}"))?;
        let mut snapshot = entry.snapshot.lock();
        snapshot.status = status;
        snapshot.last_activity = now_millis();
        Ok(())
    }

    fn restore_mailbox(&self, group: &str, id: &str, messages: Vec<MailboxMessage>) {
        let Some(entry) = self.get(group, id) else {
            return;
        };
        let mut mailbox = entry.mailbox.lock();
        for message in messages.into_iter().rev() {
            mailbox.push_front(message);
        }
        entry.message_ready.notify_waiters();
    }

    fn compare_status(
        &self,
        group: &str,
        id: &str,
        expected: AgentStatus,
        next: AgentStatus,
    ) -> Result<bool> {
        let entry = self
            .get(group, id)
            .ok_or_else(|| anyhow!("unknown orchestration agent {id:?}"))?;
        let mut snapshot = entry.snapshot.lock();
        if snapshot.status != expected {
            return Ok(false);
        }
        snapshot.status = next;
        snapshot.last_activity = now_millis();
        Ok(true)
    }

    fn finish(
        &self,
        group: &str,
        id: &str,
        status: AgentStatus,
        artifact_path: Option<PathBuf>,
        history_path: Option<PathBuf>,
    ) -> Result<()> {
        let entry = self
            .get(group, id)
            .ok_or_else(|| anyhow!("unknown orchestration agent {id:?}"))?;
        let mut snapshot = entry.snapshot.lock();
        snapshot.status = status;
        snapshot.last_activity = now_millis();
        if let Some(artifact_path) = artifact_path {
            snapshot.artifact_ref = Some(format!("agent://{id}"));
            *entry.artifact_path.lock() = Some(artifact_path);
        }
        if let Some(history_path) = history_path {
            snapshot.history_ref = Some(entry.durable_info.lock().as_ref().map_or_else(
                || format!("history://{id}"),
                |info| persisted_history_reference(&info.definition, id),
            ));
            *entry.history_path.lock() = Some(history_path);
        }
        Ok(())
    }

    fn restore_finish(
        &self,
        group: &str,
        id: &str,
        snapshot: AgentSnapshot,
        artifact_path: Option<PathBuf>,
        history_path: Option<PathBuf>,
    ) {
        let Some(entry) = self.get(group, id) else {
            return;
        };
        *entry.snapshot.lock() = snapshot;
        *entry.artifact_path.lock() = artifact_path;
        *entry.history_path.lock() = history_path;
    }

    fn remove_agent_if_paths(
        &self,
        group: &str,
        id: &str,
        artifact_path: &Path,
        history_path: &Path,
    ) -> bool {
        let mut groups = self.groups.lock();
        let Some(entries) = groups.get_mut(group) else {
            return false;
        };
        let Some(entry) = entries.get(id).cloned() else {
            return false;
        };
        let current_artifact = entry.artifact_path.lock().clone();
        let current_history = entry.history_path.lock().clone();
        let has_match = current_artifact.as_deref() == Some(artifact_path)
            || current_history.as_deref() == Some(history_path);
        let only_matches_candidate = current_artifact
            .as_deref()
            .is_none_or(|path| path == artifact_path)
            && current_history
                .as_deref()
                .is_none_or(|path| path == history_path);
        if !has_match || !only_matches_candidate {
            return false;
        }
        entries.remove(id);
        true
    }

    fn remove_group(&self, group: &str) {
        self.groups.lock().remove(group);
    }
}

fn waiter_matches(waiter: &MessageWaiter, message: &MailboxMessage) -> bool {
    if waiter
        .from
        .as_ref()
        .is_some_and(|from| from != &message.from)
    {
        return false;
    }
    if waiter
        .reply_to
        .as_ref()
        .is_some_and(|id| message.reply_to.as_deref() != Some(id.as_str()))
    {
        return false;
    }
    true
}

fn take_matching_message(
    entry: &AgentEntry,
    from: Option<&str>,
    reply_to: Option<&str>,
) -> Option<MailboxMessage> {
    let mut mailbox = entry.mailbox.lock();
    let index = mailbox.iter().position(|message| {
        from.is_none_or(|from| message.from == from)
            && reply_to.is_none_or(|id| message.reply_to.as_deref() == Some(id))
    })?;
    mailbox.remove(index)
}

fn format_orchestration_message(message: &MailboxMessage) -> String {
    let reply = message
        .reply_to
        .as_deref()
        .map_or(String::new(), |id| format!("\nReplying to message: {id}"));
    format!(
        "<orchestration-message id=\"{}\" from=\"{}\">\n{}{}\n</orchestration-message>",
        escape_xml(&message.id),
        escape_xml(&message.from),
        message.body,
        reply,
    )
}

/// Build the typed custom message used to deliver a mailbox message into a
/// session's steering queue (children via the active-delivery bridge, Main
/// via the application event loop). The typed `details` carry the full
/// mailbox record so the body can be reconstructed if the run settles before
/// processing it.
pub(crate) fn mailbox_message_as_custom(message: &MailboxMessage) -> pi_ai::Message {
    pi_ai::Message::Custom(pi_ai::CustomMessage {
        custom_type: ORCHESTRATION_MESSAGE_TYPE.to_owned(),
        content: format_orchestration_message(message).into(),
        display: true,
        details: Some(serde_json::json!({
            "id": message.id,
            "from": message.from,
            "to": message.to,
            "body": message.body,
            "replyTo": message.reply_to,
            "timestamp": message.timestamp,
        })),
        timestamp: i64::try_from(message.timestamp).unwrap_or(i64::MAX),
    })
}

/// Reverse of [`mailbox_message_as_custom`]: reconstruct the mailbox record
/// from a steered custom message (used when a run settles before processing
/// it, so the message returns to the mailbox instead of being lost).
pub(crate) fn mailbox_message_from_custom(custom: &pi_ai::CustomMessage) -> Option<MailboxMessage> {
    let details = custom.details.as_ref()?;
    Some(MailboxMessage {
        id: details.get("id")?.as_str()?.to_owned(),
        from: details.get("from")?.as_str()?.to_owned(),
        to: details.get("to")?.as_str()?.to_owned(),
        body: details.get("body")?.as_str()?.to_owned(),
        timestamp: details
            .get("timestamp")
            .and_then(|value| value.as_u64())
            .unwrap_or_else(now_millis),
        reply_to: details
            .get("replyTo")
            .and_then(|value| value.as_str())
            .map(str::to_owned),
    })
}

/// Typed view over an orchestration IRC custom message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrchestrationMessageView {
    pub id: String,
    pub from: String,
    pub to: String,
    pub body: String,
    pub reply_to: Option<String>,
}

/// Extract a typed orchestration message view without leaking the raw XML wrapper.
#[must_use]
pub fn orchestration_message_view(
    message: &pi_ai::CustomMessage,
) -> Option<OrchestrationMessageView> {
    if message.custom_type != ORCHESTRATION_MESSAGE_TYPE {
        return None;
    }
    let details = message.details.as_ref()?;
    let id = details.get("id")?.as_str()?.to_owned();
    let from = details.get("from")?.as_str()?.to_owned();
    let to = details.get("to")?.as_str()?.to_owned();
    let reply_to = details
        .get("replyTo")
        .and_then(|value| value.as_str())
        .map(str::to_owned);
    let body = details
        .get("body")
        .and_then(|value| value.as_str())
        .map(str::to_owned)
        .or_else(|| extract_orchestration_body_from_content(&message.content))?;
    Some(OrchestrationMessageView {
        id,
        from,
        to,
        body,
        reply_to,
    })
}

fn extract_orchestration_body_from_content(
    content: &pi_ai::CustomMessageContent,
) -> Option<String> {
    let text = match content {
        pi_ai::CustomMessageContent::Text(text) => text.clone(),
        pi_ai::CustomMessageContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                pi_ai::ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    };
    extract_orchestration_body_from_wrapper(&text)
}

fn extract_orchestration_body_from_wrapper(text: &str) -> Option<String> {
    let trimmed = text.trim();
    let open = "<orchestration-message";
    let close = "</orchestration-message>";
    let start = trimmed.find(open)?;
    let after_open = &trimmed[start + open.len()..];
    let body_start = after_open.find('>')? + 1;
    let inner = &after_open[body_start..];
    let end = inner.rfind(close)?;
    let mut body = inner[..end].trim().to_owned();
    if let Some(index) = body.rfind("\nReplying to message:") {
        body.truncate(index);
        body = body.trim_end().to_owned();
    }
    Some(body)
}
fn fs_missing(error: &FsError) -> bool {
    matches!(
        error,
        FsError::System {
            kind: std::io::ErrorKind::NotFound,
            ..
        }
    )
}

fn remove_retained_file(root: &CapabilityRootLease, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root.display_root()) else {
        return false;
    };
    let identity = match root.metadata(relative) {
        Ok(metadata) if metadata.identity.kind() == EntryKind::RegularFile => metadata.identity,
        Err(error) if fs_missing(&error) => return true,
        _ => return false,
    };
    match root.remove_file_expected(relative, identity) {
        Ok(()) => true,
        Err(error) if fs_missing(&error) => true,
        Err(_) => false,
    }
}

pub(super) fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn validate_agent_id(id: &str) -> Result<()> {
    if id.is_empty() {
        bail!("agent id cannot be empty");
    }
    if id.len() > 80 {
        bail!("agent id must be at most 80 bytes");
    }
    if !id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        bail!("agent id must contain only ASCII letters, digits, '_' or '-'");
    }
    Ok(())
}

fn merge_persona_continuity(
    continuity: Vec<pi_ai::Message>,
    resumed: Vec<pi_ai::Message>,
) -> Vec<pi_ai::Message> {
    let mut merged = continuity;
    for message in resumed {
        if !merged.contains(&message) {
            merged.push(message);
        }
    }
    merged
}

fn history_reference(definition: &AgentDefinition, agent_id: &str) -> String {
    if definition.is_persona() {
        format!("history://persona/{}/{agent_id}", definition.name)
    } else {
        format!("history://{agent_id}")
    }
}

fn persisted_history_reference(
    definition: &super::persistence::PersistedDefinition,
    agent_id: &str,
) -> String {
    if definition.kind == super::AgentDefinitionKind::Persona {
        format!("history://persona/{}/{agent_id}", definition.name)
    } else {
        format!("history://{agent_id}")
    }
}

#[cfg(test)]
mod persona_runtime_tests {
    use super::*;
    use std::fs;

    fn persona_definition(root: &Path) -> AgentDefinition {
        let persona_root = root.join("personas").join("mentor");
        fs::create_dir_all(&persona_root).expect("persona root");
        let path = persona_root.join("persona.md");
        super::super::definitions::parse_persona_definition(
            &path,
            "---\nname: mentor\ndescription: mentor\n---\nprompt",
            super::super::AgentDefinitionSource::User,
            true,
        )
        .expect("persona definition")
    }
    #[test]
    fn resolve_task_agent_rejects_unbound_persona_before_spawn() {
        let root = tempfile::tempdir().expect("root");
        let (runtime, _) = runtime(root.path());
        let error = runtime
            .resolve_task_agent("study the design", Some("mentor"))
            .expect_err("unbound persona must fail during resolution")
            .to_string();
        assert!(error.contains("persona `mentor`"), "{error}");
        assert!(error.contains("durable binding"), "{error}");
    }

    #[test]
    fn resolve_task_agent_rejects_missing_cj_knowledge_root_before_spawn() {
        let root = tempfile::tempdir().expect("root");
        let workspace = tempfile::tempdir().expect("workspace");
        let missing = tempfile::tempdir().expect("missing knowledge root");
        let mut definition = persona_definition(root.path());
        definition.name = "cj".to_owned();
        definition.knowledge_roots = vec![missing.path().to_string_lossy().into_owned()];
        let mut config = OrchestrationConfig::new(
            AgentCatalog::from_agents(vec![definition]),
            root.path().join("artifacts"),
        );
        config.default_agent = "cj".to_owned();
        let runtime =
            OrchestrationRuntime::new(config, Arc::new(|_| Box::pin(async { unreachable!() })))
                .expect("runtime");
        runtime.inner.durable_bound.store(true, Ordering::Release);
        *runtime.inner.parent_workspace.write() = Some(
            crate::WorkspaceRoots::new(workspace.path(), Vec::<PathBuf>::new())
                .expect("parent workspace"),
        );

        let error = runtime
            .resolve_task_agent("study the design", Some("cj"))
            .expect_err("missing knowledge root must fail during resolution")
            .to_string();
        assert!(error.contains("persona `cj`"), "{error}");
        assert!(
            error.contains(missing.path().to_string_lossy().as_ref()),
            "{error}"
        );
        assert!(error.contains("--add-dir"), "{error}");
    }

    #[test]
    fn preferred_agent_precedence_unchanged_after_persona_prevalidation() {
        let root = tempfile::tempdir().expect("root");
        let workspace = tempfile::tempdir().expect("workspace");
        let missing = tempfile::tempdir().expect("missing knowledge root");
        let mut persona = persona_definition(root.path());
        persona.knowledge_roots = vec![missing.path().to_string_lossy().into_owned()];
        let mut ordinary = persona.clone();
        ordinary.name = "worker".to_owned();
        ordinary.kind = super::super::AgentDefinitionKind::Agent;
        ordinary.path = None;
        ordinary.knowledge_roots.clear();
        let mut config = OrchestrationConfig::new(
            AgentCatalog::from_agents(vec![persona, ordinary]),
            root.path().join("artifacts"),
        );
        config.default_agent = "mentor".to_owned();
        let runtime =
            OrchestrationRuntime::new(config, Arc::new(|_| Box::pin(async { unreachable!() })))
                .expect("runtime");
        runtime.inner.durable_bound.store(true, Ordering::Release);
        *runtime.inner.parent_workspace.write() = Some(
            crate::WorkspaceRoots::new(workspace.path(), Vec::<PathBuf>::new())
                .expect("parent workspace"),
        );
        runtime.set_preferred_agent(Some("worker"));

        let error = runtime
            .resolve_task_agent("study the design", Some("mentor"))
            .expect_err("invalid persona must be rejected before preferred fallback")
            .to_string();
        assert!(error.contains("--add-dir"), "{error}");
        assert_eq!(runtime.preferred_agent().as_deref(), Some("worker"));
        assert_eq!(
            runtime
                .resolve_task_agent("study the design", None)
                .expect("ordinary preferred agent"),
            "worker"
        );
    }

    #[tokio::test]
    async fn prepare_parent_reset_binding_carries_parent_workspace() {
        let root = tempfile::tempdir().expect("root");
        let workspace = tempfile::tempdir().expect("workspace");
        let parent = isolated_session(workspace.path(), Vec::new(), None).await;
        parent
            .session
            .start_new_recording()
            .expect("record reset parent");
        let (runtime, _) = runtime(root.path());
        runtime
            .install_generation_capture(parent.session.generation_capture_fn());

        let prepared = runtime
            .prepare_parent_reset_binding(&parent.session)
            .expect("prepare reset binding");

        let binding = prepared
            .parent_workspace
            .as_ref()
            .expect("reset binding must carry parent workspace");
        let parent_roots = parent.session.workspace_roots();
        assert_eq!(
            binding.cwd(),
            parent_roots.cwd(),
            "reset binding must carry the parent cwd"
        );
        assert_eq!(
            binding.roots(),
            parent_roots.roots(),
            "reset binding must carry the same ordered workspace snapshot as ordinary parent binding"
        );
        assert_eq!(
            binding.additional_roots(),
            parent_roots.additional_roots(),
            "reset binding must carry the parent additional roots"
        );
        assert!(
            std::sync::Arc::ptr_eq(
                &binding.capabilities().expect("binding capability authority"),
                &parent_roots
                    .capabilities()
                    .expect("parent capability authority"),
            ),
            "reset binding must retain the parent capability authority"
        );
        for (index, root) in parent_roots.roots().iter().enumerate() {
            assert_eq!(
                binding.authority(index).map(|lease| lease.identity()),
                parent_roots.authority(index).map(|lease| lease.identity()),
                "reset binding authority identity must match the parent snapshot for {root:?}"
            );
        }
    }

    fn runtime(root: &Path) -> (OrchestrationRuntime, AgentDefinition) {
        let definition = persona_definition(root);
        let mut config = OrchestrationConfig::new(
            AgentCatalog::from_agents(vec![definition.clone()]),
            root.join("artifacts"),
        );
        config.default_agent = definition.name.clone();
        let runtime =
            OrchestrationRuntime::new(config, Arc::new(|_| Box::pin(async { unreachable!() })))
                .expect("runtime");
        (runtime, definition)
    }

    #[test]
    fn rebuild_revival_request_propagates_prompt_construction_failure() {
        let root = tempfile::tempdir().expect("root");
        let mut definition = persona_definition(root.path());
        definition.personality = Some("x".repeat(MAX_PERSONALITY_BYTES + 1));
        let mut config = OrchestrationConfig::new(
            AgentCatalog::from_agents(vec![definition.clone()]),
            root.path().join("artifacts"),
        );
        config.default_agent = definition.name.clone();
        let runtime =
            OrchestrationRuntime::new(config, Arc::new(|_| Box::pin(async { unreachable!() })))
                .expect("runtime");
        let snapshot = AgentSnapshot {
            id: "MentorRun".to_owned(),
            display_name: definition.name.clone(),
            agent: definition.name.clone(),
            parent_id: Some("Main".to_owned()),
            status: AgentStatus::Parked,
            created_at: 1,
            last_activity: 1,
            unread: 0,
            artifact_ref: None,
            history_ref: None,
        };
        let info = DurableAgentInfo {
            definition: super::super::persistence::persist_definition(&definition),
            request: super::super::persistence::PersistedRequest {
                child_id: "MentorRun".to_owned(),
                parent_id: "Main".to_owned(),
                depth: 1,
                assignment: "work".to_owned(),
                system_prompt: "prompt".to_owned(),
                requested_tool_names: None,
                thinking_level: None,
                max_tools_per_agent: 16,
                model_provider: None,
                model_id: None,
                output_schema: None,
                schema_mode: None,
            },
            session_path: None,
        };

        let error = runtime
            .rebuild_revival_request("MentorRun", &snapshot, &info, None)
            .expect_err("prompt construction failure must propagate as Err, not Ok(None)")
            .to_string();
        assert!(
            error.contains("personality exceeds maximum size"),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn persona_archive_rejects_symlinked_sessions_directory() {
        let root = tempfile::tempdir().expect("root");
        let (runtime, definition) = runtime(root.path());
        let persona_root = definition.persona_root().expect("persona root");
        let outside = root.path().join("outside");
        fs::create_dir_all(&outside).expect("outside");
        std::os::unix::fs::symlink(&outside, persona_root.join("sessions"))
            .expect("sessions symlink");

        let error = runtime
            .persona_sessions("mentor", RootCreation::CreateDirectory)
            .expect_err("symlinked sessions directory must fail");
        assert!(
            error.chain().any(|source| {
                source
                    .downcast_ref::<crate::fs::FsError>()
                    .is_some_and(|error| {
                        matches!(
                            error,
                            crate::fs::FsError::UnsupportedEntryType
                                | crate::fs::FsError::System {
                                    raw_os_error: Some(40) | Some(62),
                                    ..
                                }
                        )
                    })
            }),
            "{error:#}"
        );
        assert!(
            outside
                .read_dir()
                .expect("outside directory")
                .next()
                .is_none()
        );
    }

    #[test]
    fn persona_history_fallback_survives_ordinary_retention() {
        let root = tempfile::tempdir().expect("root");
        let (runtime, definition) = runtime(root.path());
        let persona_root = definition.persona_root().expect("persona root");
        let sessions = persona_root.join("sessions");
        fs::create_dir_all(&sessions).expect("sessions");
        let archive = sessions.join("MentorRun.jsonl");
        fs::write(&archive, "{}\n").expect("archive");

        assert_eq!(
            runtime
                .resolve_read_uri("history://MentorRun")
                .expect("persona history"),
            archive,
        );
        runtime.prune_retained_jobs();
        assert!(
            archive.exists(),
            "ordinary retention must preserve persona archives"
        );
    }

    #[test]
    fn cross_persona_history_requires_qualified_uri() {
        let root = tempfile::tempdir().expect("root");
        let mentor = persona_definition(root.path());
        let reviewer_root = root.path().join("personas").join("reviewer");
        fs::create_dir_all(&reviewer_root).expect("reviewer root");
        let reviewer = super::super::definitions::parse_persona_definition(
            &reviewer_root.join("persona.md"),
            "---\nname: reviewer\ndescription: reviewer\n---\nprompt",
            super::super::AgentDefinitionSource::User,
            true,
        )
        .expect("reviewer");
        let mut config = OrchestrationConfig::new(
            AgentCatalog::from_agents(vec![mentor.clone(), reviewer.clone()]),
            root.path().join("artifacts"),
        );
        config.default_agent = mentor.name.clone();
        let runtime =
            OrchestrationRuntime::new(config, Arc::new(|_| Box::pin(async { unreachable!() })))
                .expect("runtime");
        for definition in [&mentor, &reviewer] {
            let persona_root = definition.persona_root().expect("persona root");
            fs::create_dir_all(persona_root.join("sessions")).expect("sessions");
            fs::write(persona_root.join("sessions").join("Shared.jsonl"), "{}\n").expect("archive");
        }
        let error = runtime
            .resolve_read_uri("history://Shared")
            .expect_err("bare collision must be ambiguous");
        assert!(error.to_string().contains("ambiguous"), "{error:#}");
        assert_eq!(
            runtime
                .resolve_read_uri("history://persona/mentor/Shared")
                .expect("qualified mentor"),
            fs::canonicalize(
                mentor
                    .persona_root()
                    .expect("mentor root")
                    .join("sessions/Shared.jsonl")
            )
            .expect("canonical mentor archive")
        );
    }
    #[tokio::test]
    async fn finish_agent_keeps_persona_history_qualified_and_agent_history_bare() {
        let root = tempfile::tempdir().expect("root");
        let persona = persona_definition(root.path());
        let ordinary = super::super::definitions::parse_agent_definition(
            Path::new("task.md"),
            "---\nname: task\ndescription: task\n---\nprompt",
            super::super::AgentDefinitionSource::Bundled,
            true,
        )
        .expect("ordinary definition");
        let config = OrchestrationConfig::new(
            AgentCatalog::from_agents(vec![persona.clone(), ordinary.clone()]),
            root.path().join("artifacts"),
        );
        let runtime =
            OrchestrationRuntime::new(config, Arc::new(|_| Box::pin(async { unreachable!() })))
                .expect("runtime");
        for (id, definition) in [("MentorRun", persona), ("TaskRun", ordinary)] {
            runtime
                .inner
                .registry
                .register(
                    runtime.group_id(),
                    AgentSnapshot {
                        id: id.to_owned(),
                        display_name: definition.name.clone(),
                        agent: definition.name.clone(),
                        parent_id: Some("Main".to_owned()),
                        status: AgentStatus::Running,
                        created_at: 1,
                        last_activity: 1,
                        unread: 0,
                        artifact_ref: None,
                        history_ref: None,
                    },
                    runtime.inner.config.mailbox_capacity,
                )
                .expect("register child");
            let entry = runtime
                .inner
                .registry
                .get(runtime.group_id(), id)
                .expect("registered entry");
            *entry.durable_info.lock() = Some(DurableAgentInfo {
                definition: super::super::persistence::persist_definition(&definition),
                request: super::super::persistence::PersistedRequest {
                    child_id: id.to_owned(),
                    parent_id: "Main".to_owned(),
                    depth: 1,
                    assignment: "work".to_owned(),
                    system_prompt: "prompt".to_owned(),
                    requested_tool_names: None,
                    thinking_level: None,
                    max_tools_per_agent: 16,
                    model_provider: None,
                    model_id: None,
                    output_schema: None,
                    schema_mode: None,
                },
                session_path: None,
            });
            runtime
                .finish_agent(
                    id,
                    AgentStatus::Idle,
                    None,
                    Some(root.path().join(format!("{id}.history.json"))),
                )
                .expect("finish child");
        }

        assert_eq!(
            runtime
                .agent_snapshot("MentorRun")
                .expect("persona snapshot")
                .history_ref
                .as_deref(),
            Some("history://persona/mentor/MentorRun")
        );
        assert_eq!(
            runtime
                .agent_snapshot("TaskRun")
                .expect("agent snapshot")
                .history_ref
                .as_deref(),
            Some("history://TaskRun")
        );
    }

    #[test]
    fn persona_archive_names_are_not_reused() {
        let root = tempfile::tempdir().expect("root");
        let (runtime, definition) = runtime(root.path());
        let persona_root = definition.persona_root().expect("persona root");
        fs::create_dir_all(persona_root.join("sessions")).expect("sessions");
        fs::write(persona_root.join("sessions").join("Mentor.jsonl"), "{}\n").expect("archive");
        let mut reserved = std::collections::BTreeSet::new();

        let id = runtime
            .allocate_unique_agent_id_for_definition("Mentor", &definition, &mut reserved)
            .expect("unique id");
        assert_eq!(id, "Mentor_2");
    }

    #[test]
    fn persona_lifecycle_guard_blocks_spawn_and_preserves_other_personas() {
        let root = tempfile::tempdir().expect("root");
        let mentor = persona_definition(root.path());
        let reviewer_root = root.path().join("personas").join("reviewer");
        fs::create_dir_all(&reviewer_root).expect("reviewer root");
        let reviewer = super::super::definitions::parse_persona_definition(
            &reviewer_root.join("persona.md"),
            "---\nname: reviewer\ndescription: reviewer\n---\nprompt",
            super::super::AgentDefinitionSource::User,
            true,
        )
        .expect("reviewer");
        let task = super::super::definitions::parse_agent_definition(
            Path::new("task.md"),
            "---\nname: task\ndescription: task\n---\nprompt",
            super::super::AgentDefinitionSource::Bundled,
            true,
        )
        .expect("task");
        let config = OrchestrationConfig::new(
            AgentCatalog::from_agents(vec![mentor, reviewer, task]),
            root.path().join("artifacts"),
        );
        let runtime =
            OrchestrationRuntime::new(config, Arc::new(|_| Box::pin(async { unreachable!() })))
                .expect("runtime");
        let guard = runtime
            .begin_persona_destructive_operation("mentor")
            .expect("idle mentor claim");

        let blocked = runtime
            .spawn_tasks(
                "Main",
                0,
                vec![TaskItem {
                    index: 0,
                    id: "MentorRun".to_owned(),
                    agent: "mentor".to_owned(),
                    assignment: "work".to_owned(),
                    ..TaskItem::default()
                }],
            )
            .expect_err("claimed persona spawn must fail");
        assert!(
            blocked.to_string().contains("destructive operation"),
            "{blocked:#}"
        );
        assert!(
            runtime
                .inner
                .persona_lifecycle_blocks
                .lock()
                .contains("mentor")
        );
        assert!(
            !runtime
                .inner
                .persona_lifecycle_blocks
                .lock()
                .contains("reviewer"),
            "unrelated persona must remain available"
        );

        drop(guard);
        assert!(runtime.inner.persona_lifecycle_blocks.lock().is_empty());

        let retained = runtime
            .begin_persona_destructive_operation("mentor")
            .expect("idle mentor claim after release");
        retained.retain();
        let second = runtime
            .begin_persona_destructive_operation("mentor")
            .expect_err("retained lifecycle block must fail closed");
        assert!(
            second.to_string().contains("destructive operation"),
            "{second:#}"
        );
    }

    #[test]
    fn persona_lifecycle_guard_rejects_active_registry_entry_without_job() {
        let root = tempfile::tempdir().expect("root");
        let (runtime, _) = runtime(root.path());
        runtime
            .inner
            .registry
            .register(
                runtime.group_id(),
                AgentSnapshot {
                    id: "MentorActive".to_owned(),
                    display_name: "mentor".to_owned(),
                    agent: "mentor".to_owned(),
                    parent_id: Some("Main".to_owned()),
                    status: AgentStatus::Running,
                    created_at: 1,
                    last_activity: 1,
                    unread: 0,
                    artifact_ref: None,
                    history_ref: None,
                },
                runtime.inner.config.mailbox_capacity,
            )
            .expect("register active persona");

        let error = runtime
            .begin_persona_destructive_operation("mentor")
            .expect_err("active registry entry must reject lifecycle claim");
        assert!(
            error.to_string().contains("active orchestration job"),
            "{error:#}"
        );
    }
    #[test]
    fn persona_continuity_accumulates_across_archives() {
        let root = tempfile::tempdir().expect("root");
        let (runtime, definition) = runtime(root.path());
        let persona_root = definition.persona_root().expect("persona root");
        let sessions = persona_root.join("sessions");
        fs::create_dir_all(&sessions).expect("sessions");

        let first = root.path().join("first.jsonl");
        let first_recorder = crate::session_store::start_session_in(
            root.path(),
            None,
            None,
            Some(root.path()),
            Some("first"),
            None,
        )
        .expect("first session");
        first_recorder
            .record_message(&pi_ai::Message::user_text("first run", 0))
            .expect("first record");
        first_recorder.close().expect("close first session");
        fs::copy(&first, sessions.join("run-1.jsonl")).expect("first archive");

        let second = root.path().join("second.jsonl");
        let second_recorder = crate::session_store::start_session_in(
            root.path(),
            None,
            None,
            Some(root.path()),
            Some("second"),
            None,
        )
        .expect("second session");
        second_recorder
            .record_message(&pi_ai::Message::user_text("second run", 1))
            .expect("second record");
        second_recorder.close().expect("close second session");
        fs::copy(&second, sessions.join("run-2.jsonl")).expect("second archive");

        let continuity = runtime
            .load_persona_continuity("mentor")
            .expect("continuity");
        let rendered = serde_json::to_string(&continuity).expect("continuity json");
        assert!(rendered.contains("first run"), "{rendered}");
        assert!(rendered.contains("second run"), "{rendered}");
    }

    #[test]
    fn revival_continuity_merge_keeps_archives_and_deduplicates_resume() {
        let archived = pi_ai::Message::user_text("archived before crash", 1);
        let resumed = pi_ai::Message::user_text("persisted partial", 2);
        let merged = merge_persona_continuity(
            vec![archived.clone(), resumed.clone()],
            vec![resumed.clone()],
        );
        assert_eq!(merged, vec![archived, resumed]);
    }

    #[test]
    fn definition_soft_budget_replaces_global_budget() {
        let root = tempfile::tempdir().expect("root");
        let (runtime, mut definition) = runtime(root.path());
        definition.soft_budget = Some(JobSoftBudget {
            max_requests: Some(1),
            max_tokens: None,
            yield_after: None,
        });
        let state = JobSoftBudgetState::default();
        let hook = runtime
            .soft_budget_stop_hook(&definition, &state)
            .expect("persona budget hook");
        let mut assistant = pi_ai::AssistantMessage::pending(&pi_ai::Model::default());
        assistant.usage.total_tokens = 0;
        assert!(hook(&pi_agent::ShouldStopAfterTurnContext {
            message: assistant,
            tool_results: Vec::new(),
            context: pi_agent::AgentContext {
                system_prompt: String::new(),
                messages: Vec::new(),
                tools: Vec::new(),
            },
            new_messages: Vec::new(),
        }));
    }

    #[test]
    fn ordinary_agent_ignores_persona_runtime_fields_defense_in_depth() {
        let root = tempfile::tempdir().expect("root");
        let (runtime, mut definition) = runtime(root.path());
        definition.kind = super::super::AgentDefinitionKind::Agent;
        definition.personality = Some("must not inject".to_owned());
        definition.soft_budget = Some(JobSoftBudget {
            max_requests: Some(1),
            max_tokens: None,
            yield_after: None,
        });
        let prompt = runtime
            .child_system_prompt(&definition, "work", None, None, None, "<peer_roster />")
            .expect("prompt");
        assert!(!prompt.contains("must not inject"), "{prompt}");
        let state = JobSoftBudgetState::default();
        assert!(
            runtime.soft_budget_stop_hook(&definition, &state).is_none(),
            "ordinary agent must use the unlimited global budget"
        );
    }
    #[test]
    fn persona_skills_shadow_shared_prompt_without_mutating_global_catalog() {
        let root = tempfile::tempdir().expect("root");
        let (mut runtime, definition) = runtime(root.path());
        let shared_path = root.path().join("shared-skill.md");
        fs::write(
            &shared_path,
            "---\nname: overlap\ndescription: shared description\n---\nshared body",
        )
        .expect("shared skill");
        let shared = OrchestrationSkill {
            name: "overlap".to_owned(),
            description: "shared description".to_owned(),
            file_path: shared_path.clone(),
            base_dir: root.path().to_path_buf(),
            globs: Vec::new(),
            always_apply: false,
            hidden: false,
            disable_model_invocation: false,
            source: crate::SkillSource::User,
            managed_project_boundary: None,
            managed_root: Some(root.path().to_path_buf()),
            trusted: true,
        };
        Arc::get_mut(&mut runtime.inner)
            .expect("unique runtime")
            .config
            .skills = vec![shared];
        let persona = crate::PersonaSkill {
            name: "overlap".to_owned(),
            description: "persona description".to_owned(),
            content: "---\nname: overlap\ndescription: persona description\n---\npersona body"
                .to_owned(),
        };
        let prompt = runtime
            .child_system_prompt_with_persona_skills(
                &definition,
                "work",
                None,
                None,
                None,
                "<peer_roster />",
                std::slice::from_ref(&persona),
            )
            .expect("persona prompt");
        assert!(prompt.contains("persona description"), "{prompt}");
        assert!(prompt.contains("persona body"), "{prompt}");
        assert!(!prompt.contains("shared description"), "{prompt}");
        let mut ordinary = definition.clone();
        ordinary.kind = super::super::AgentDefinitionKind::Agent;
        let ordinary_prompt = runtime
            .child_system_prompt(&ordinary, "work", None, None, None, "<peer_roster />")
            .expect("ordinary prompt");
        assert!(
            ordinary_prompt.contains("shared description"),
            "{ordinary_prompt}"
        );
        assert!(
            !ordinary_prompt.contains("persona description"),
            "{ordinary_prompt}"
        );
        assert_eq!(runtime.skills().len(), 1);
        assert_eq!(runtime.skills()[0].description, "shared description");
    }

    #[test]
    fn persona_skill_snapshot_changes_only_on_next_prompt() {
        let root = tempfile::tempdir().expect("root");
        let (runtime, definition) = runtime(root.path());
        let (store, initial) = runtime
            .persona_skill_context(&definition)
            .expect("initial persona skill context");
        let initial_prompt = runtime
            .child_system_prompt_with_persona_skills(
                &definition,
                "work",
                None,
                None,
                None,
                "<peer_roster />",
                &initial,
            )
            .expect("initial prompt");
        store
            .expect("persona store")
            .learn("fresh", "fresh description", "fresh instructions")
            .expect("learn skill");
        assert!(!initial_prompt.contains("fresh instructions"));
        assert!(initial.is_empty(), "current snapshot must remain immutable");

        let (_, next) = runtime
            .persona_skill_context(&definition)
            .expect("next persona skill context");
        let next_prompt = runtime
            .child_system_prompt_with_persona_skills(
                &definition,
                "work",
                None,
                None,
                None,
                "<peer_roster />",
                &next,
            )
            .expect("next prompt");
        assert!(next_prompt.contains("fresh instructions"), "{next_prompt}");
        assert_eq!(
            runtime.skills().len(),
            0,
            "global skill catalog stays unchanged"
        );
    }

    #[test]
    fn persona_skill_snapshot_load_rejects_symlink_traversal() {
        #[cfg(unix)]
        {
            let root = tempfile::tempdir().expect("root");
            let outside = tempfile::tempdir().expect("outside");
            let (runtime, definition) = runtime(root.path());
            std::os::unix::fs::symlink(
                outside.path(),
                definition
                    .persona_root()
                    .expect("persona root")
                    .join("skills"),
            )
            .expect("skills symlink");
            let error = runtime
                .persona_skill_context(&definition)
                .expect_err("symlinked skills must fail closed");
            assert!(!error.to_string().is_empty());
        }
    }
}

fn write_new_artifact(root: &CapabilityRootLease, path: &Path, content: &[u8]) -> Result<()> {
    if content.len() as u64 > MAX_ARTIFACT_BYTES {
        bail!("orchestration artifact exceeds maximum size");
    }
    let relative = path
        .strip_prefix(root.display_root())
        .map_err(|_| anyhow!("artifact path escapes orchestration artifact capability"))?;
    let mut file = root
        .create_new(relative, FileMode::Private)
        .context("creating orchestration artifact")?;
    file.write_all(content)
        .context("writing orchestration artifact")?;
    file.finish().context("syncing orchestration artifact")?;
    Ok(())
}

fn write_revival_artifact(root: &CapabilityRootLease, path: &Path, content: &[u8]) -> Result<()> {
    if content.len() as u64 > MAX_ARTIFACT_BYTES {
        bail!("orchestration artifact exceeds maximum size");
    }
    let relative = path
        .strip_prefix(root.display_root())
        .map_err(|_| anyhow!("artifact path escapes orchestration artifact capability"))?
        .to_path_buf();
    let expected = match root.metadata(&relative) {
        Ok(metadata) if metadata.identity.kind() == EntryKind::RegularFile => {
            Some(metadata.identity)
        }
        Err(error) if fs_missing(&error) => None,
        Ok(_) => bail!("orchestration artifact is not a regular file"),
        Err(error) => return Err(error).context("reading orchestration artifact identity"),
    };
    root.replace_file(Replacement {
        relative,
        expected,
        bytes: content.to_vec(),
        mode: FileMode::Private,
    })
    .context("replacing orchestration artifact")?;
    Ok(())
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

fn presentation_job_snapshot(mut job: JobSnapshot) -> JobSnapshot {
    job.description = job
        .description
        .as_deref()
        .map(|description| presentation_text(description, TASK_DESCRIPTION_MAX_CHARS));
    if let Some(result) = &mut job.result {
        result.output = presentation_text(&result.output, 600);
        result.error = result
            .error
            .as_deref()
            .map(|error| presentation_text(error, 300));
        result.artifact_ref = presentation_text(&result.artifact_ref, 240);
        result.history_ref = presentation_text(&result.history_ref, 240);
        result.artifact_uri = presentation_text(&result.artifact_uri, 240);
    }
    job
}

fn presentation_agent_snapshot(mut agent: AgentSnapshot) -> AgentSnapshot {
    agent.display_name = presentation_text(&agent.display_name, 160);
    agent.artifact_ref = agent
        .artifact_ref
        .as_deref()
        .map(|reference| presentation_text(reference, 240));
    agent.history_ref = agent
        .history_ref
        .as_deref()
        .map(|reference| presentation_text(reference, 240));
    agent
}

fn presentation_text(text: &str, max_chars: usize) -> String {
    let value = crate::redact_value(&serde_json::Value::String(text.to_owned()));
    let redacted = value.as_str().unwrap_or_default();
    if redacted.chars().count() <= max_chars {
        return redacted.to_owned();
    }
    if max_chars <= 1 {
        return redacted.chars().take(max_chars).collect();
    }
    let mut output = redacted.chars().take(max_chars - 1).collect::<String>();
    output.push('…');
    output
}

fn render_roster_peer(peer: &AgentSnapshot) -> String {
    let status = match peer.status {
        AgentStatus::Queued => "queued",
        AgentStatus::Running => "running",
        AgentStatus::Idle => "idle",
        AgentStatus::Parked => "parked",
        AgentStatus::Aborted => "aborted",
    };
    let id = escape_xml(&peer.id);
    let agent = escape_xml(&presentation_text(&peer.agent, MAX_ROSTER_AGENT_CHARS));
    let parent = peer
        .parent_id
        .as_deref()
        .map(escape_xml)
        .unwrap_or_else(|| "none".to_owned());
    format!("  <peer id=\"{id}\" agent=\"{agent}\" status=\"{status}\" parent=\"{parent}\" />\n")
}
/// These are unambiguous delegation grammars — the causative
/// `have`/`ask`/`tell X do …` and the explicit `delegate`/`assign`/`spawn`/
/// `dispatch` — so their object is a delegation candidate whether or not it
/// is catalogued: delegating to a missing agent is still caught and reported
/// actionably.
const ENGLISH_DELEGATION_VERBS: &[&str] = &[
    "have", "ask", "tell", "delegate", "assign", "spawn", "dispatch",
];

/// Verbs that double as ordinary instruction verbs (`run python3`,
/// `make sure`, `let me know`, `send the report`) or common nouns (`the
/// run`, `the start`, `the launch`). Their object only counts as a
/// delegation candidate when it names an actual configured agent or skill
/// identity, so natural language never fabricates missing agents; the
/// noun-prone ones additionally require an imperative frame, matching the
/// selector grammar.
const ENGLISH_WEAK_DELEGATION_VERBS: &[&str] = &[
    "get", "launch", "let", "make", "please", "run", "send", "kick", "start", "use",
];

const CJK_DELEGATION_TOKENS: &[&str] = &["让", "请", "叫", "派", "安排", "委托", "交给"];

fn agent_name_span(request: &str, agent_name: &str) -> Option<(usize, usize)> {
    let name_chars = agent_name.chars().collect::<Vec<_>>();
    if name_chars.is_empty() {
        return None;
    }
    let chars = request.char_indices().collect::<Vec<_>>();
    if chars.len() < name_chars.len() {
        return None;
    }
    for start in 0..=chars.len() - name_chars.len() {
        let matched = (0..name_chars.len()).all(|offset| {
            let request_char = chars[start + offset].1;
            let name_char = name_chars[offset];
            if request_char.is_ascii() && name_char.is_ascii() {
                request_char.eq_ignore_ascii_case(&name_char)
            } else {
                request_char == name_char
            }
        });
        if !matched {
            continue;
        }
        let before_ok = start == 0 || !chars[start - 1].1.is_ascii_alphanumeric();
        let after_index = start + name_chars.len();
        let after_ok = after_index >= chars.len() || !chars[after_index].1.is_ascii_alphanumeric();
        if before_ok && after_ok {
            let (byte_start, _) = chars[start];
            let (byte_end, last_char) = chars[after_index - 1];
            return Some((byte_start, byte_end + last_char.len_utf8()));
        }
    }
    None
}

fn cjk_delegation_construction(request: &str, agent_name: &str) -> bool {
    let normalized = normalized_lower(request);
    let (span_start, span_end) = match agent_name_span(&normalized, &normalized_lower(agent_name)) {
        Some(span) => span,
        None => return false,
    };
    let before = &normalized[..span_start];
    let token_hit = CJK_DELEGATION_TOKENS.iter().any(|token| {
        before.ends_with(token)
            || before
                .strip_suffix(' ')
                .is_some_and(|prefix| prefix.ends_with(token) && !prefix.ends_with("  "))
    });
    if !token_hit {
        return false;
    }
    let clause = before
        .rsplit(|character: char| {
            matches!(
                character,
                '。' | '；' | '，' | '、' | '！' | '？' | '…' | '.' | ';' | ',' | '!' | '?'
            )
        })
        .next()
        .unwrap_or(before);
    // Mirror the selector's clause-aware CJK gate: the 不要/不用/不必/勿/莫
    // negation family, the paired 是否 interrogative, and the potential
    // 能不能 form void the delegation, while a lone `否` (否则) does not.
    if ["不要", "不用", "不必", "勿", "莫", "是否", "能不能"]
        .iter()
        .any(|marker| clause.contains(marker))
        || clause.char_indices().any(|(index, character)| {
            character == '别'
                && !matches!(
                    clause[..index].chars().next_back(),
                    Some('特' | '分' | '个')
                )
        })
    {
        return false;
    }
    let runs = ascii_identifier_runs(&normalized);
    let Some((index, _)) = runs
        .iter()
        .enumerate()
        .find(|(_, (_, start))| *start == span_start)
    else {
        return false;
    };
    let mut chain_end = span_end;
    for (next_run, next_start) in runs.iter().skip(index + 1) {
        let gap = &normalized[chain_end..*next_start];
        let joined = !gap.is_empty()
            && gap
                .chars()
                .all(|character| is_cjk_agent_conjunction(character) || character.is_whitespace())
            && gap.chars().any(is_cjk_agent_conjunction)
            && looks_like_agent_name(next_run);
        if !joined {
            break;
        }
        chain_end = next_start + next_run.len();
    }
    normalized[chain_end..]
        .chars()
        .filter(|character| {
            !character.is_whitespace()
                && !character.is_ascii_punctuation()
                && !matches!(
                    character,
                    '。' | '？'
                        | '！'
                        | '，'
                        | '、'
                        | '；'
                        | '：'
                        | '…'
                        | '～'
                        | '「'
                        | '」'
                        | '『'
                        | '』'
                        | '（'
                        | '）'
                        | '《'
                        | '》'
                )
        })
        .take(2)
        .count()
        >= 2
}

fn normalized_lower(value: &str) -> String {
    value.nfkc().flat_map(char::to_lowercase).collect()
}

fn looks_like_agent_name(token: &str) -> bool {
    let mut chars = token.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_alphabetic())
        && token.len() >= 2
        && token.len() <= 80
        && token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn is_english_function_word(token: &str) -> bool {
    matches!(
        token,
        "the"
            | "a"
            | "an"
            | "this"
            | "that"
            | "these"
            | "those"
            | "some"
            | "any"
            | "my"
            | "your"
            | "our"
            | "their"
            | "its"
            | "his"
            | "her"
            | "me"
            | "us"
            | "them"
            | "him"
            | "we"
            | "you"
            | "i"
            | "it"
            | "they"
            | "he"
            | "she"
    )
}

fn follows_without_action(token: &str) -> bool {
    matches!(
        token,
        "the"
            | "a"
            | "an"
            | "this"
            | "that"
            | "these"
            | "those"
            | "my"
            | "your"
            | "our"
            | "its"
            | "his"
            | "her"
            | "him"
            | "them"
            | "us"
            | "me"
            | "it"
            | "for"
            | "of"
            | "in"
            | "on"
            | "at"
            | "with"
            | "by"
            | "from"
            | "and"
            | "or"
            | "but"
            | "is"
            | "are"
            | "was"
            | "were"
            | "be"
            | "being"
            | "been"
            | "as"
            | "than"
            | "then"
            | "there"
            | "here"
            | "so"
            | "just"
            | "only"
            | "also"
            | "too"
            | "very"
            | "really"
            | "how"
            | "why"
            | "what"
            | "when"
            | "where"
            | "who"
            | "whom"
            | "which"
            | "whose"
    )
}

fn ascii_identifier_runs(request: &str) -> Vec<(String, usize)> {
    let mut runs = Vec::new();
    let mut current = String::new();
    let mut start = 0;
    for (index, character) in request.char_indices() {
        if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
            if current.is_empty() {
                start = index;
            }
            current.push(character);
        } else if !current.is_empty() {
            runs.push((std::mem::take(&mut current), start));
        }
    }
    if !current.is_empty() {
        runs.push((current, start));
    }
    runs
}

#[cfg(test)]
fn delegation_candidates(request: &str) -> Vec<String> {
    delegation_candidates_with_identities(request, &[], None)
}

fn delegation_candidates_with_identities(
    request: &str,
    trusted_agents: &[AgentDefinition],
    known_identities: Option<&BTreeSet<String>>,
) -> Vec<String> {
    let mut candidates = BTreeSet::new();
    let normalized = normalized_lower(request);
    let tokens = delegation_scan_tokens(&normalized);
    for (index, (verb, _)) in tokens.iter().enumerate() {
        let strong = ENGLISH_DELEGATION_VERBS.contains(&verb.as_str());
        let weak = ENGLISH_WEAK_DELEGATION_VERBS.contains(&verb.as_str());
        if !strong && !weak {
            continue;
        }
        // Grammar gate mirroring the selector: interrogatives,
        // clause-local negations, declarative subject clauses, and
        // mid-clause noun-prone verbs never delegate.
        if diagnostics_verb_clause_vetoed(&normalized, &tokens, index) {
            continue;
        }

        let mut cursor = index + 1;
        if strong
            && matches!(verb.as_str(), "delegate" | "assign" | "dispatch")
            && tokens.get(cursor).is_some_and(|(token, _)| token == "to")
        {
            cursor += 1;
        }
        let Some((first, first_start)) = tokens.get(cursor) else {
            continue;
        };
        if !looks_like_agent_name(first)
            || is_english_function_word(first)
            || is_english_agent_conjunction(first)
        {
            continue;
        }

        let mut members = vec![first.clone()];
        let mut previous_end = first_start + first.len();
        cursor += 1;
        let mut list_started = false;
        while let Some((token, token_start)) = tokens.get(cursor) {
            let separated =
                gap_has_english_list_separator(&normalized, previous_end, *token_start);
            let conjunction = is_english_agent_conjunction(token);
            if !separated && !conjunction {
                break;
            }
            list_started = true;
            if conjunction {
                cursor += 1;
            }
            let Some((member, member_start)) = tokens.get(cursor) else {
                break;
            };
            if !looks_like_agent_name(member)
                || is_english_function_word(member)
                || is_english_agent_conjunction(member)
            {
                break;
            }
            members.push(member.clone());
            previous_end = member_start + member.len();
            cursor += 1;
        }

        // Weak instruction verbs only establish delegation when at least one
        // complete-list member exactly names a configured agent or skill.
        // Once anchored, retain every member so absent co-delegates cannot be
        // hidden by their position in a conjunction/comma list.
        if weak
            && !members.iter().any(|member| {
                known_identities.is_some_and(|identities| identities.contains(member))
            })
        {
            continue;
        }
        if members.len() == 1
            && !list_started
            && tokens
                .get(cursor)
                .is_some_and(|(after, _)| follows_without_action(after))
        {
            continue;
        }
        candidates.extend(members);
    }

    // Name-initial imperative (`coordinator please fix the build`,
    // `reviewer audit this`): the request opens with a name-like token
    // followed by a selector delegation verb, an optional polite `please`
    // in between. Mirrors the selector's `name_initial_imperative`;
    // identity-anchored like the weak verbs so ordinary sentences never
    // fabricate missing agents, and skipped in question form.
    if let Some((first, _)) = tokens.first()
        && looks_like_agent_name(first)
        && !is_english_function_word(first)
        && !crate::selector::is_question_form(&normalized)
    {
        let verb_index = if tokens.get(1).is_some_and(|(token, _)| token == "please") {
            2
        } else {
            1
        };
        if tokens
            .get(verb_index)
            .is_some_and(|(token, _)| crate::selector::DELEGATION_VERBS.contains(&token.as_str()))
            && known_identities.is_some_and(|identities| identities.contains(first))
        {
            candidates.insert(first.clone());
        }
    }

    let runs = ascii_identifier_runs(request);
    let mut chained = false;
    for (index, (run, start)) in runs.iter().enumerate() {
        if !looks_like_agent_name(run) {
            chained = false;
            continue;
        }
        let direct = cjk_delegation_construction(request, run);
        let joined = chained && {
            let (previous, previous_start) = &runs[index - 1];
            let gap = &request[previous_start + previous.len()..*start];
            !gap.is_empty()
                && gap.chars().any(is_cjk_agent_conjunction)
                && gap.chars().all(|character| {
                    is_cjk_agent_conjunction(character) || character.is_whitespace()
                })
        };
        if direct || joined {
            candidates.insert(run.clone());
            chained = true;
        } else {
            chained = false;
        }
    }

    // Selector-parity direct addresses, taken verbatim from the shared
    // selector delegated-name API (`crate::selector::delegation_agent_mention`)
    // so the diagnostics can never drift from the grammar that gates
    // user-prompt delegation: a bare whole-name request (`coordinator`) and a
    // name-leading comma/colon vocative (`coordinator: route next`,
    // `coordinator, route next`) are explicit delegations there, so a
    // disabled or enabled catalogued name addressed that way must surface in
    // the missing/disabled diagnostics too. The API only ever returns names
    // of the passed trusted catalogued agents, so uncatalogued text can never
    // be fabricated into a missing-agent report here.
    match crate::selector::delegation_agent_mention(request, trusted_agents) {
        crate::selector::ExactAgentMention::None => {}
        crate::selector::ExactAgentMention::Unique(name) => {
            candidates.insert(name);
        }
        crate::selector::ExactAgentMention::Ambiguous(names) => candidates.extend(names),
    }
    candidates.into_iter().collect()
}

/// Clause-initial test for `tokens[index]`, mirroring the selector's
/// `english_clause_starts`: first token, or a clause punctuation gap
/// directly before it.
#[must_use]
fn diagnostics_clause_initial(
    normalized: &str,
    tokens: &[(String, usize)],
    index: usize,
) -> bool {
    if index == 0 {
        return true;
    }
    let (previous, previous_start) = &tokens[index - 1];
    normalized[previous_start + previous.len()..tokens[index].1]
        .contains([',', '.', ';', '!', '?'])
}

/// Clause-local grammar veto for one English verb occurrence in the
/// workflow missing-agent scan, sharing the selector's constants
/// (`DELEGATION_MARKER_NEGATIONS`, `ENGLISH_CLAUSE_AUXILIARIES`,
/// `ENGLISH_CLAUSE_PRONOUNS`, `ENGLISH_DECLARATIVE_SUBJECTS`,
/// `DELEGATION_NOUN_PRONE_MARKERS`) so both parsers cannot drift apart:
/// subject-inverted or contracted interrogatives (`did you have glm run`,
/// `can't have glm run`) and clause-local negations (`don't have glm run`)
/// void the verb; a declarative subject + auxiliary clause (`we have used
/// glm`, `I did have glm run it`) describes rather than delegates; question
/// form keeps only clause-initial verbs; and the noun-prone verbs (`run`,
/// `get`, `launch`, `start`) delegate only from an imperative frame
/// (`run glm on the failure`, never `review the run glm produced`).
#[must_use]
fn diagnostics_verb_clause_vetoed(
    normalized: &str,
    tokens: &[(String, usize)],
    verb_index: usize,
) -> bool {
    let clause_initial = diagnostics_clause_initial(normalized, tokens, verb_index);
    if crate::selector::is_question_form(normalized) && !clause_initial {
        return true;
    }
    if crate::selector::DELEGATION_NOUN_PRONE_MARKERS.contains(&tokens[verb_index].0.as_str())
        && !clause_initial
    {
        return true;
    }
    let mut scanned = 0;
    let mut index = verb_index;
    while index > 0 && scanned < 8 {
        let (previous, previous_start) = &tokens[index - 1];
        if normalized[previous_start + previous.len()..tokens[index].1]
            .contains([',', '.', ';', '!', '?'])
        {
            return false;
        }
        index -= 1;
        scanned += 1;
        if crate::selector::DELEGATION_MARKER_NEGATIONS.contains(&tokens[index].0.as_str()) {
            return true;
        }
        // Declarative subject + auxiliary (the auxiliary may be the verb
        // itself, as in `we have used glm` / `I have glm fix it`).
        if crate::selector::ENGLISH_DECLARATIVE_SUBJECTS.contains(&tokens[index].0.as_str())
            && index + 1 < tokens.len()
            && index + 1 <= verb_index
            && crate::selector::ENGLISH_CLAUSE_AUXILIARIES
                .contains(&tokens[index + 1].0.as_str())
        {
            return true;
        }
        if crate::selector::ENGLISH_CLAUSE_AUXILIARIES.contains(&tokens[index].0.as_str()) {
            // `can't` / `shouldn't` contraction remnant after the auxiliary.
            if normalized[tokens[index].1..].starts_with('\'') {
                return true;
            }
            // Subject inversion: `did you have glm run it`.
            if index + 1 < tokens.len()
                && crate::selector::ENGLISH_CLAUSE_PRONOUNS
                    .contains(&tokens[index + 1].0.as_str())
            {
                return true;
            }
            // Declarative subject directly before the auxiliary.
            if index > 0
                && crate::selector::ENGLISH_DECLARATIVE_SUBJECTS
                    .contains(&tokens[index - 1].0.as_str())
            {
                return true;
            }
        }
    }
    false
}

fn is_cjk_agent_conjunction(character: char) -> bool {
    matches!(character, '和' | '跟' | '与' | '、' | '，' | ',')
}

fn delegation_scan_tokens(normalized: &str) -> Vec<(String, usize)> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut start = 0;
    for (index, character) in normalized.char_indices() {
        if character.is_alphanumeric() || character == '-' || character == '_' {
            if current.is_empty() {
                start = index;
            }
            current.push(character);
        } else if !current.is_empty() {
            tokens.push((std::mem::take(&mut current), start));
        }
    }
    if !current.is_empty() {
        tokens.push((current, start));
    }
    tokens
}

fn is_english_agent_conjunction(token: &str) -> bool {
    matches!(token, "and" | "or")
}

fn gap_has_english_list_separator(
    normalized: &str,
    previous_end: usize,
    next_start: usize,
) -> bool {
    normalized[previous_end..next_start]
        .chars()
        .any(|character| matches!(character, ',' | '，' | '&'))
}

/// Stored/wire bound for a task description (redacted, newlines preserved).
const TASK_DESCRIPTION_MAX_CHARS: usize = 1024;

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Per-line preview cap for `hub read_history` transcript labels, mirroring
/// the tree-panel label style (prefix + first-line preview) with a generous
/// allowance so a coach reads real context on the subagent detail page.
const HISTORY_LABEL_MAX_CHARS: usize = 1200;

/// Leading indent of every continuation row. No label row starts with
/// whitespace, so a consumer attributes an indented row to the label above it
/// (the web details modal splits on exactly this indent).
const HISTORY_CONTINUATION_INDENT: &str = "  ";

/// Closes a preview cut by any budget, so a reader never mistakes a truncated
/// preview for the whole body. It is an ADDITIONAL row: the `*_PREVIEW_LINES`
/// budgets count content rows, so a cut preview contributes at most
/// `HISTORY_*_PREVIEW_LINES + 1` rows.
const HISTORY_PREVIEW_TRUNCATED_MARKER: &str = "… [preview truncated]";

/// Prose rows previewed after an assistant label row.
const HISTORY_ASSISTANT_PREVIEW_LINES: usize = 40;

/// Tool-result output preview budgets: rows and bytes per tool result.
const HISTORY_TOOL_OUTPUT_PREVIEW_LINES: usize = 60;
const HISTORY_TOOL_OUTPUT_PREVIEW_BYTES: usize = 5120;

/// Compact labels for one transcript message, mirroring the tree-panel style:
/// a type prefix plus a first-line preview. Assistant messages additionally
/// project one readable line per tool call — the concrete action
/// (target/selector/command/operation) — so a rendered history never reduces a
/// child's work to generic `[tool: <name>]` tags, and follow their prose with
/// up to [`HISTORY_ASSISTANT_PREVIEW_LINES`] indented continuation rows. A
/// tool result keeps its reliably-known `· ok`/`· error` outcome on the row
/// itself and follows it with a bounded indented output preview instead of raw
/// unbounded output. Both previews are secret-scrubbed over the WHOLE body
/// first (one implementation, [`crate::redact::redact_secrets`]), redacting the
/// body's logical text concatenation before any row is cut: a cut applied
/// first could strip a pattern's closing delimiter or its minimum token length
/// and leak a credential prefix.
/// EVERY branch scrubs its body before its own cut; the whole-payload pass
/// in [`OrchestrationRuntime::read_child_history`] stays on top of that.
fn history_message_label(message: &pi_ai::Message) -> String {
    let mut lines = Vec::new();
    match message {
        pi_ai::Message::User(user) => {
            let text = crate::redact::redact_secrets(&content_list_text(&user.content));
            lines.push(format!("user: {}", first_line(&text)));
            lines.extend(history_continuation_rows(
                text.split(['\r', '\n']).skip(1),
                HISTORY_ASSISTANT_PREVIEW_LINES,
                None,
            ));
        }
        pi_ai::Message::Assistant(assistant) => {
            let text = crate::redact::redact_secrets(&assistant.text());
            if !text.trim().is_empty() {
                lines.push(format!("assistant: {}", first_line(&text)));
                lines.extend(history_continuation_rows(
                    text.split(['\r', '\n']).skip(1),
                    HISTORY_ASSISTANT_PREVIEW_LINES,
                    None,
                ));
            }
            // One readable line per call, in content order. `text()` above
            // drops ToolCall blocks, so walk `content` directly for the
            // concrete actions the child took.
            for block in &assistant.content {
                if let pi_ai::ContentBlock::ToolCall(tool) = block {
                    lines.push(format!("assistant · {}", tool_call_summary(tool)));
                }
            }
            if lines.is_empty() {
                lines.push("assistant".to_owned());
            }
        }
        pi_ai::Message::ToolResult(result) => {
            // The outcome stays on the row (`is_error` is authoritative, so
            // nothing is fabricated); the output follows as a bounded,
            // indented preview so a coach can read what the child got back.
            let outcome = if result.is_error {
                " · error"
            } else {
                " · ok"
            };
            lines.push(format!("[tool: {}]{outcome}", result.tool_name));
            // Secret-scrub the whole output before any row is cut. The blocks
            // are redacted as their LOGICAL CONCATENATION, with no separator
            // inserted — the semantic the rest of the codebase uses for a
            // message's text (`tool_presentation::content_blocks_text`,
            // `AssistantMessage::text`) — so a private-key delimiter split
            // across adjacent blocks still matches. Non-text blocks are
            // skipped.
            let output = crate::redact::redact_secrets(
                &result
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        pi_ai::ContentBlock::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>(),
            );
            lines.extend(history_continuation_rows(
                output.split(['\r', '\n']),
                HISTORY_TOOL_OUTPUT_PREVIEW_LINES,
                Some(HISTORY_TOOL_OUTPUT_PREVIEW_BYTES),
            ));
        }
        pi_ai::Message::BashExecution(bash) => {
            let command = crate::redact::redact_secrets(&bash.command);
            lines.push(format!("[bash] {}", first_line(&command)));
        }
        pi_ai::Message::Custom(custom) => {
            let text = crate::redact::redact_secrets(&custom_content_text(&custom.content));
            lines.push(format!("custom: {}", first_line(&text)));
            lines.extend(history_continuation_rows(
                text.split(['\r', '\n']).skip(1),
                HISTORY_ASSISTANT_PREVIEW_LINES,
                None,
            ));
        }
        pi_ai::Message::BranchSummary(summary) => {
            let text = crate::redact::redact_secrets(&summary.summary);
            lines.push(format!("[branch summary] {}", first_line(&text)));
        }
        pi_ai::Message::CompactionSummary(summary) => {
            let text = crate::redact::redact_secrets(&summary.summary);
            lines.push(format!("[compaction] {}", first_line(&text)));
        }
    }
    lines
        .into_iter()
        .map(|line| truncate_label(line.trim_end(), HISTORY_LABEL_MAX_CHARS))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Indented continuation rows for the body of a row whose own label is already
/// emitted and whose secrets are already scrubbed: at most `max_rows` content
/// rows, each path-redacted exactly like a label
/// ([`redact_absolute_path_tokens`] — no second redaction implementation) and
/// capped at [`HISTORY_LABEL_MAX_CHARS`] including the indent, admitted only
/// while the SERIALIZED block they form — every row plus its newline
/// separator, with the marker row and its separator reserved — still fits
/// `max_bytes` when that bound is given. Blank rows are dropped because
/// consumers drop them too; whenever a budget cut content the marker row
/// closes the preview, so text is never cut silently and never mid-row.
fn history_continuation_rows<'a>(
    body: impl Iterator<Item = &'a str>,
    max_rows: usize,
    max_bytes: Option<usize>,
) -> Vec<String> {
    let marker = format!("{HISTORY_CONTINUATION_INDENT}{HISTORY_PREVIEW_TRUNCATED_MARKER}");
    let width = HISTORY_LABEL_MAX_CHARS - HISTORY_CONTINUATION_INDENT.len();
    let mut rows: Vec<String> = Vec::new();
    let mut bytes = 0usize;
    let mut cut = false;
    for line in body {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if rows.len() == max_rows {
            cut = true;
            break;
        }
        let row = format!(
            "{HISTORY_CONTINUATION_INDENT}{}",
            truncate_label(&redact_absolute_path_tokens(line), width)
        );
        // Charge this row's separator plus the reserved marker row and its
        // separator: the budget bounds the block a caller actually joins.
        let charged = 1 + row.len();
        if max_bytes.is_some_and(|max| bytes + charged + 1 + marker.len() > max) {
            cut = true;
            break;
        }
        bytes += charged;
        rows.push(row);
    }
    if cut {
        rows.push(marker);
    }
    rows
}

/// Compact human-facing summary of one assistant tool call for history
/// labels. Reuses the budgeted `compact_tool_arguments_with_budget`; file tools surface their repo-relative
/// target/selector, `bash` its first concrete command, planning/messaging
/// tools their operation + target, and unknown tools a safe compact digest
/// (never raw JSON). Absolute machine paths never leak: a bare absolute
/// path/selector shrinks to its final component, and absolute path tokens
/// embedded anywhere in a command or argument digest are redacted to `[PATH]`
/// (http/https URLs are preserved as targets).
fn tool_call_summary(tool: &pi_ai::ToolCall) -> String {
    let compact = crate::tool_presentation::compact_tool_arguments_with_budget(
        &tool.arguments,
        HISTORY_LABEL_MAX_CHARS,
    );
    let digest = compact.split_whitespace().collect::<Vec<_>>().join(" ");
    let digest = if is_absolute_path(&digest) && !is_http_url(&digest) {
        digest
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(&digest)
            .to_owned()
    } else {
        digest
    };
    let digest = redact_absolute_path_tokens(&digest);
    if digest.is_empty() {
        return tool.name.clone();
    }
    format!("{} {}", tool.name, digest)
}

/// True when `value` is an absolute machine path: Unix (`/…`), home-relative
/// (`~…`), or a Windows drive path (`X:\…` / `X:/…`).
fn is_absolute_path(value: &str) -> bool {
    value.starts_with('/')
        || value.starts_with('~')
        || (value.len() >= 3
            && value.as_bytes()[0].is_ascii_alphabetic()
            && value.as_bytes()[1] == b':')
}

/// True when `value` is an http/https URL, which is a target, not a machine
/// path, and is preserved verbatim.
fn is_http_url(value: &str) -> bool {
    value.starts_with("http://") || value.starts_with("https://")
}

fn redact_absolute_path_tokens(value: &str) -> String {
    value
        .split_whitespace()
        .map(|token| {
            if is_absolute_path(token) && !is_http_url(token) {
                "[PATH]"
            } else {
                token
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Caps a rendered transcript at [`MAX_HISTORY_BYTES`]. The cut lands on a line
/// boundary whenever the budget contains one — a partial row would render as a
/// broken label in the panel — and falls back to a char boundary only for a
/// single line longer than the whole budget; an explicit marker always closes
/// the result.
fn cap_history_bytes(mut rendered: String) -> String {
    if rendered.len() <= MAX_HISTORY_BYTES {
        return rendered;
    }
    let marker = format!("\n[history truncated at {} KiB]", MAX_HISTORY_BYTES / 1024);
    let budget = MAX_HISTORY_BYTES.saturating_sub(marker.len());
    let mut end = budget;
    while !rendered.is_char_boundary(end) {
        end -= 1;
    }
    if let Some(newline) = rendered[..end].rfind('\n') {
        end = newline;
    }
    rendered.truncate(end);
    rendered.push_str(&marker);
    rendered
}

fn render_history_file(file: &RetainedHistoryFile, lines: usize) -> Result<String> {
    let path = file.display_path();
    let data = file
        .read_bounded(MAX_HISTORY_FILE_BYTES)
        .with_context(|| format!("reading history {}", path.display()))?;
    if data
        .iter()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace())
        == Some(b'[')
    {
        let messages: Vec<pi_ai::Message> = serde_json::from_slice(&data)
            .with_context(|| format!("parsing history snapshot {}", path.display()))?;
        let rendered = messages
            .iter()
            .flat_map(|message| {
                history_message_label(message)
                    .lines()
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let start = rendered.len().saturating_sub(lines);
        return Ok(rendered[start..].join("\n"));
    }
    let tree = crate::session_store::load_session_tree_from_file(file.session_file()?, &path)
        .with_context(|| format!("parsing session transcript {}", path.display()))?;
    let rendered = tree
        .entries
        .iter()
        .filter(|entry| entry.entry_type != "label" && entry.entry_type != "checkpoint")
        .flat_map(|entry| {
            history_entry_label(entry)
                .lines()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let start = rendered.len().saturating_sub(lines);
    Ok(rendered[start..].join("\n"))
}

/// Compact single-line label for one session record. Message records delegate
/// to [`history_message_label`]; non-message records (model/thinking changes,
/// session info, compaction, custom state) render as short bracketed tags.
fn history_entry_label(entry: &crate::session_store::SessionEntry) -> String {
    if let Some(message) = entry.message.as_ref() {
        return history_message_label(message);
    }
    let (prefix, body) = match entry.entry_type.as_str() {
        "custom_message" => (
            "custom: ".to_owned(),
            entry
                .content
                .as_ref()
                .map(custom_content_text)
                .unwrap_or_default(),
        ),
        "model_change" => (
            format!(
                "[model: {}/{}]",
                entry.provider.as_deref().unwrap_or_default(),
                entry.model_id.as_deref().unwrap_or_default()
            ),
            String::new(),
        ),
        "thinking_level_change" => (
            format!(
                "[thinking: {}]",
                entry.thinking_level.as_deref().unwrap_or_default()
            ),
            String::new(),
        ),
        "session_info" => (
            format!("[title: {}]", entry.name.as_deref().unwrap_or_default()),
            String::new(),
        ),
        "compaction" => (
            "[compaction] ".to_owned(),
            entry.summary.clone().unwrap_or_default(),
        ),
        "branch_summary" => (
            "[branch summary] ".to_owned(),
            entry.summary.clone().unwrap_or_default(),
        ),
        other => (format!("[{other}]"), String::new()),
    };
    let composed = if body.is_empty() {
        prefix
    } else {
        format!("{prefix}{body}")
    };
    let redacted = crate::redact::redact_secrets(&composed);
    truncate_label(first_line(&redacted).trim_end(), HISTORY_LABEL_MAX_CHARS)
}

/// Joins the text blocks of a content list with single spaces (mirrors the
/// tree panel's `text` helper).
fn content_list_text(content: &[pi_ai::ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            pi_ai::ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Extracts the text of a [`pi_ai::CustomMessageContent`] (a bare string or a
/// block list) for label previews.
fn custom_content_text(content: &pi_ai::CustomMessageContent) -> String {
    match content {
        pi_ai::CustomMessageContent::Text(text) => text.clone(),
        pi_ai::CustomMessageContent::Blocks(blocks) => content_list_text(blocks),
    }
}

/// Returns the first line (before any CR/LF) of `value`.
fn first_line(value: &str) -> &str {
    match value.find(['\r', '\n']) {
        Some(index) => &value[..index],
        None => value,
    }
}

/// Truncates `value` to at most `max_chars` characters, appending an ellipsis
/// when content was cut (mirrors the tree panel's `truncate_label`).
fn truncate_label(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    let mut out: String = value.chars().take(max_chars - 1).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod history_label_tests {
    use super::*;
    use pi_ai::{AssistantMessage, ContentBlock, ToolCall};
    use std::fs;

    fn tool_call(name: &str, arguments: serde_json::Value) -> ContentBlock {
        ContentBlock::ToolCall(ToolCall {
            id: format!("call-{name}"),
            name: name.to_owned(),
            arguments,
            thought_signature: None,
        })
    }

    fn assistant(content: Vec<ContentBlock>) -> pi_ai::Message {
        let mut message = AssistantMessage::pending(&pi_ai::Model::default());
        message.content = content;
        pi_ai::Message::Assistant(message)
    }

    #[test]
    fn assistant_tool_calls_render_concrete_actions_not_generic_tags() {
        let message = assistant(vec![
            ContentBlock::text("checking the seed"),
            tool_call("read", serde_json::json!({ "path": "seed.txt" })),
            tool_call(
                "bash",
                serde_json::json!({ "command": "cargo build --release" }),
            ),
            tool_call(
                "hub",
                serde_json::json!({ "op": "send", "to": "worker", "message": "go" }),
            ),
            tool_call("yield", serde_json::json!({ "text": "deliverable done" })),
        ]);
        let label = history_message_label(&message);
        // Assistant text is preserved alongside the projected calls.
        assert!(label.contains("assistant: checking the seed"), "{label}");
        assert!(label.contains("assistant · read seed.txt"), "{label}");
        assert!(
            label.contains("assistant · bash cargo build --release"),
            "{label}"
        );
        // Planning/messaging tools surface operation + target key fields.
        assert!(
            label.contains("assistant · hub message=go, op=send, to=worker"),
            "{label}"
        );
        // Unknown tools get a safe compact digest, never raw JSON.
        assert!(
            label.contains("assistant · yield text=deliverable done"),
            "{label}"
        );
        // One readable line per call (text line + 4 call lines).
        assert_eq!(label.lines().count(), 5, "{label}");
        assert!(!label.contains("\"path\""), "no raw JSON dump: {label}");
    }

    #[test]
    fn tool_call_summary_redacts_secrets_in_workspace_paths_and_commands() {
        let secret = ["s", "k-", "abcdefghijklmnop1234"].concat();
        // Repo-allowed `<workspace>/...` placeholder target: the secret-shaped
        // filename is redacted while the safe placeholder stays readable.
        let read = tool_call_summary(&ToolCall {
            id: "call-1".to_owned(),
            name: "read".to_owned(),
            arguments: serde_json::json!({ "path": format!("<workspace>/secrets/{secret}.txt") }),
            thought_signature: None,
        });
        assert_eq!(read, "read <workspace>/secrets/[REDACTED].txt", "{read}");
        assert!(!read.contains(&secret), "secret must be redacted: {read}");
        assert!(read.starts_with("read "), "{read}");
        assert!(!read.contains('\n'), "summary must stay one line: {read}");

        let bash = tool_call_summary(&ToolCall {
            id: "call-2".to_owned(),
            name: "bash".to_owned(),
            arguments: serde_json::json!({ "command": format!("echo {secret}\nsecond line") }),
            thought_signature: None,
        });
        assert_eq!(bash, "bash echo [REDACTED] second line", "{bash}");
        assert!(!bash.contains(&secret), "secret must be redacted: {bash}");
        assert!(
            !bash.contains('\n'),
            "command must collapse to one line: {bash}"
        );
    }

    #[test]
    fn tool_call_summary_keeps_workspace_paths_safe_inside_commands() {
        // A `<workspace>/...` placeholder embedded in a command is a repo-relative
        // target, so it stays readable in the safe display; only credential-shaped
        // content inside it is redacted.
        let bash = tool_call_summary(&ToolCall {
            id: "call-1".to_owned(),
            name: "bash".to_owned(),
            arguments: serde_json::json!({ "command": "cat <workspace>/secrets/secret.txt" }),
            thought_signature: None,
        });
        assert_eq!(bash, "bash cat <workspace>/secrets/secret.txt", "{bash}");

        // Secret-shaped filenames inside a command never leak.
        let secret = ["s", "k-", "abcdefghijklmnop1234"].concat();
        let leaky = tool_call_summary(&ToolCall {
            id: "call-2".to_owned(),
            name: "bash".to_owned(),
            arguments: serde_json::json!({ "command": format!("cat <workspace>/secrets/{secret}.txt") }),
            thought_signature: None,
        });
        assert_eq!(
            leaky, "bash cat <workspace>/secrets/[REDACTED].txt",
            "{leaky}"
        );
        assert!(!leaky.contains(&secret), "secret must be redacted: {leaky}");

        // http/https URLs are targets, not machine paths: preserved verbatim.
        let curl = tool_call_summary(&ToolCall {
            id: "call-3".to_owned(),
            name: "bash".to_owned(),
            arguments: serde_json::json!({ "command": "curl https://example.com/api" }),
            thought_signature: None,
        });
        assert_eq!(curl, "bash curl https://example.com/api", "{curl}");
    }

    fn tool_result(tool_name: &str, output: &str, is_error: bool) -> pi_ai::Message {
        pi_ai::Message::ToolResult(pi_ai::ToolResultMessage {
            tool_call_id: format!("call-{tool_name}"),
            tool_name: tool_name.to_owned(),
            content: vec![ContentBlock::text(output)],
            usage: None,
            details: None,
            added_tool_names: Vec::new(),
            is_error,
            timestamp: 0,
        })
    }

    fn rows(label: &str) -> Vec<&str> {
        label.lines().collect()
    }

    #[test]
    fn assistant_reply_previews_the_first_lines_after_the_label_row() {
        let short = assistant(vec![ContentBlock::text("first line\nsecond line\nthird line")]);
        let label = history_message_label(&short);
        assert_eq!(
            rows(&label),
            vec!["assistant: first line", "  second line", "  third line"],
            "{label}"
        );

        let long = (1..=(HISTORY_ASSISTANT_PREVIEW_LINES * 2))
            .map(|index| format!("answer {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let label = history_message_label(&assistant(vec![ContentBlock::text(long)]));
        let lines = rows(&label);
        assert_eq!(
            lines.len(),
            1 + HISTORY_ASSISTANT_PREVIEW_LINES + 1,
            "label + preview + marker: {label}"
        );
        assert_eq!(lines[0], "assistant: answer 1");
        for (offset, line) in lines[1..=HISTORY_ASSISTANT_PREVIEW_LINES].iter().enumerate() {
            assert_eq!(*line, format!("  answer {}", offset + 2));
        }
        assert_eq!(
            lines[HISTORY_ASSISTANT_PREVIEW_LINES + 1],
            format!("{HISTORY_CONTINUATION_INDENT}{HISTORY_PREVIEW_TRUNCATED_MARKER}"),
            "{label}"
        );
        assert!(
            !label.contains(&format!("answer {}", HISTORY_ASSISTANT_PREVIEW_LINES + 5)),
            "cut content absent: {label}"
        );

        // Blank prose lines are dropped, not previewed as empty rows.
        let spaced = assistant(vec![ContentBlock::text("first\n\n   \nsecond")]);
        assert_eq!(
            rows(&history_message_label(&spaced)),
            vec!["assistant: first", "  second"]
        );
    }

    #[test]
    fn tool_result_row_keeps_the_outcome_with_a_bounded_output_preview() {
        // Row budget binds: short output lines preview the row budget and say so.
        let many = (1..=(HISTORY_TOOL_OUTPUT_PREVIEW_LINES + 20))
            .map(|index| format!("out {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let label = history_message_label(&tool_result("bash", &many, false));
        let lines = rows(&label);
        assert_eq!(lines[0], "[tool: bash] · ok", "{label}");
        assert_eq!(
            lines.len(),
            1 + HISTORY_TOOL_OUTPUT_PREVIEW_LINES + 1,
            "row + preview + marker: {label}"
        );
        assert_eq!(lines[1], "  out 1");
        assert_eq!(
            lines[HISTORY_TOOL_OUTPUT_PREVIEW_LINES],
            format!("  out {HISTORY_TOOL_OUTPUT_PREVIEW_LINES}")
        );
        assert_eq!(
            lines[HISTORY_TOOL_OUTPUT_PREVIEW_LINES + 1],
            format!("{HISTORY_CONTINUATION_INDENT}{HISTORY_PREVIEW_TRUNCATED_MARKER}"),
            "{label}"
        );
        assert!(
            !label.contains(&format!("out {}", HISTORY_TOOL_OUTPUT_PREVIEW_LINES + 10)),
            "cut content absent: {label}"
        );
        // Byte budget binds first for long rows, and the cut lands between
        // whole output lines — never a silently truncated one.
        let wide = (1..=(HISTORY_TOOL_OUTPUT_PREVIEW_LINES + 20))
            .map(|index| format!("{index:02}{}", "y".repeat(400)))
            .collect::<Vec<_>>()
            .join("\n");
        let label = history_message_label(&tool_result("bash", &wide, false));
        let lines = rows(&label);
        assert_eq!(lines[0], "[tool: bash] · ok", "{label}");
        let preview = &lines[1..lines.len() - 1];
        assert!(
            preview.len() < HISTORY_TOOL_OUTPUT_PREVIEW_LINES,
            "byte budget must bind before the row budget: {label}"
        );
        for (index, row) in preview.iter().enumerate() {
            assert_eq!(
                *row,
                format!("  {:02}{}", index + 1, "y".repeat(400)),
                "row {index} unchanged within the byte budget: {row:?}"
            );
        }
        assert_eq!(
            lines[lines.len() - 1],
            format!("{HISTORY_CONTINUATION_INDENT}{HISTORY_PREVIEW_TRUNCATED_MARKER}"),
            "{label}"
        );
        assert!(
            !label.contains(&format!("{}y", HISTORY_TOOL_OUTPUT_PREVIEW_LINES)),
            "rows past the byte budget are absent: {label}"
        );
        // A row longer than the label width is still cut at the width, marked.
        let wider = history_message_label(&tool_result(
            "bash",
            &"y".repeat(HISTORY_LABEL_MAX_CHARS + 40),
            false,
        ));
        assert!(rows(&wider)[1].ends_with('…'), "{wider}");
        let serialized: usize = lines[1..].iter().map(|row| row.len() + 1).sum();
        assert!(
            serialized <= HISTORY_TOOL_OUTPUT_PREVIEW_BYTES,
            "the joined continuation block stays within the byte budget: {serialized}"
        );

        // A failing result keeps `error` on the row and still previews output.
        let failed = history_message_label(&tool_result("read", "boom\nmore detail", true));
        assert_eq!(
            rows(&failed),
            vec!["[tool: read] · error", "  boom", "  more detail"],
            "{failed}"
        );

        // Output without lines (or without text) renders the bare row.
        let empty = history_message_label(&tool_result("read", "", false));
        assert_eq!(empty, "[tool: read] · ok", "{empty}");
    }

    #[test]
    fn tool_output_preview_never_leaks_a_private_key_block() {
        // The block is far longer than the preview, so a redaction pass applied
        // after the preview cuts would never see the closing delimiter.
        let fill = "Zm9vYmFy".repeat(10);
        let key = format!(
            "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----",
            std::iter::repeat_n(fill.as_str(), 40)
                .collect::<Vec<_>>()
                .join("\n")
        );
        let label = history_message_label(&tool_result("bash", &key, false));
        assert!(!label.contains("-----"), "no delimiter may survive: {label}");
        assert!(!label.contains(&fill), "no key material: {label}");
        assert_eq!(rows(&label), vec!["[tool: bash] · ok", "  [REDACTED]"], "{label}");

        // Adjacent text blocks are one logical text: a delimiter split across
        // them (either one, including mid-word) must still match, which rules
        // out inserting any separator between blocks before redaction.
        let body = std::iter::repeat_n(fill.as_str(), 40)
            .collect::<Vec<_>>()
            .join("\n");
        let split_key = |blocks: Vec<String>| {
            let mut message = tool_result("bash", "unused", false);
            if let pi_ai::Message::ToolResult(result) = &mut message {
                result.content = blocks
                    .iter()
                    .map(|block| ContentBlock::text(block.as_str()))
                    .collect();
            }
            let label = history_message_label(&message);
            assert_eq!(
                rows(&label),
                vec!["[tool: bash] · ok", "  [REDACTED]"],
                "{label}"
            );
            assert!(!label.contains("-----"), "delimiter fragment: {label}");
            assert!(!label.contains("PRIVATE"), "delimiter fragment: {label}");
            assert!(!label.contains(&fill), "key material: {label}");
        };
        split_key(vec![
            format!("-----BEGIN PRIVATE KEY-----\n{body}"),
            format!("{body}\n-----END PRIVATE KEY-----"),
        ]);
        split_key(vec![
            "-----BEGIN PRI".to_owned(),
            format!("VATE KEY-----\n{body}\n-----END PRIVATE KEY-----"),
        ]);
        split_key(vec![
            format!("-----BEGIN PRIVATE KEY-----\n{body}\n-----END PRIVATE"),
            " KEY-----".to_owned(),
        ]);
    }

    #[test]
    fn tool_output_preview_never_leaks_a_credential_at_the_row_cut() {
        let secret = ["s", "k-", "abcdefghijklmnop1234"].concat();
        // The credential ends before the preview width, so it must arrive
        // scrubbed rather than as a visible prefix.
        let line = format!("{} {secret} tail", "a".repeat(100));
        let label = history_message_label(&tool_result("bash", &line, false));
        assert!(!label.contains("sk-"), "credential prefix leaked: {label}");
        assert!(!label.contains(&secret), "{label}");
        assert!(label.contains("[REDACTED]"), "{label}");

        // A credential just inside the width cut, with content after it: the
        // row is cut either way, but only a pass that runs before the cut
        // leaves the scrubbed form (the shared patterns need the delimiter
        // that separates the token from the padding).
        let straddling = format!(
            "{} {secret} {}",
            "b".repeat(HISTORY_LABEL_MAX_CHARS - 19),
            "z".repeat(30)
        );
        let label = history_message_label(&tool_result("bash", &straddling, false));
        assert!(!label.contains("sk-"), "credential prefix leaked: {label}");
        assert!(!label.contains(&secret), "{label}");
        assert!(
            label.contains("[REDACTED] zzzz…"),
            "cut row keeps the scrubbed form: {label}"
        );

        // Assistant prose is scrubbed ahead of its own label/row cuts too.
        let prose = assistant(vec![ContentBlock::text(format!(
            "{} {secret} tail",
            "c".repeat(HISTORY_LABEL_MAX_CHARS - 19)
        ))]);
        let label = history_message_label(&prose);
        assert!(!label.contains("sk-"), "credential prefix leaked: {label}");
        assert!(!label.contains(&secret), "{label}");
    }

    #[test]
    fn continuation_rows_always_follow_a_label_row() {
        let long = (1..=(HISTORY_ASSISTANT_PREVIEW_LINES * 2))
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let cases = [
            (
                pi_ai::Message::user_text(format!("ask\n{long}"), 0),
                HISTORY_ASSISTANT_PREVIEW_LINES + 1,
                "a user turn previews its prose under the label",
            ),
            (
                assistant(vec![ContentBlock::text(long.as_str())]),
                HISTORY_ASSISTANT_PREVIEW_LINES + 1,
                "assistant prose previews its tail",
            ),
            (
                pi_ai::Message::Custom(pi_ai::CustomMessage {
                    custom_type: "note".to_owned(),
                    content: pi_ai::CustomMessageContent::from(format!("note\n{long}")),
                    display: true,
                    details: None,
                    timestamp: 0,
                }),
                HISTORY_ASSISTANT_PREVIEW_LINES + 1,
                "a custom message previews its prose under the label",
            ),
            (
                tool_result("bash", long.as_str(), false),
                HISTORY_TOOL_OUTPUT_PREVIEW_LINES + 1,
                "tool output previews under its row",
            ),
        ];
        for (message, continuations, case) in cases {
            let label = history_message_label(&message);
            let lines = rows(&label);
            assert_eq!(
                lines.len(),
                continuations + 1,
                "one label row plus its continuations ({case}): {label:?}"
            );
            assert!(
                !lines[0].starts_with(HISTORY_CONTINUATION_INDENT),
                "the first row of a message block is its label ({case}): {label:?}"
            );
            for line in &lines[1..] {
                assert!(
                    line.starts_with(HISTORY_CONTINUATION_INDENT),
                    "a continuation without a preceding label ({case}): {label:?}"
                );
                assert!(
                    !line.starts_with(&format!("{HISTORY_CONTINUATION_INDENT} ")),
                    "continuations carry exactly one indent ({case}): {line:?}"
                );
            }
        }
    }

    #[test]
    fn continuation_rows_redact_absolute_path_tokens() {
        let secret_path = "/home/coach/.ssh/id_rsa";
        let label = history_message_label(&tool_result(
            "bash",
            &format!("{secret_path}\nsee {secret_path} for the key\nplain line"),
            false,
        ));
        assert!(!label.contains(secret_path), "path must be redacted: {label}");
        assert_eq!(
            rows(&label),
            vec!["[tool: bash] · ok", "  [PATH]", "  see [PATH] for the key", "  plain line"],
            "{label}"
        );

        // Prose continuations go through the same single redaction path.
        let prose = assistant(vec![ContentBlock::text(format!(
            "checked\nran {secret_path} twice"
        ))]);
        let label = history_message_label(&prose);
        assert!(!label.contains(secret_path), "{label}");
        assert!(label.contains("  ran [PATH] twice"), "{label}");
    }

    #[test]
    fn history_byte_cap_cuts_on_a_line_boundary_with_a_marker() {
        let marker = format!("\n[history truncated at {} KiB]", MAX_HISTORY_BYTES / 1024);
        let line = format!("{}\n", "x".repeat(100));
        let rendered = line.repeat(MAX_HISTORY_BYTES / line.len() + 10);
        assert!(rendered.len() > MAX_HISTORY_BYTES, "fixture must exceed the cap");
        let capped = cap_history_bytes(rendered.clone());
        assert!(capped.len() <= MAX_HISTORY_BYTES, "{}", capped.len());
        assert!(capped.ends_with(&marker), "explicit marker required");
        let body = capped.strip_suffix(&marker).expect("marker suffix");
        assert!(rendered.starts_with(body), "only new rows are dropped");
        assert!(
            rendered[body.len()..].starts_with('\n'),
            "the cut lands on a line boundary, never mid-row"
        );

        // Under the cap the payload is returned unchanged, no marker.
        let short = "user: ask\nassistant: reply".to_owned();
        assert_eq!(cap_history_bytes(short.clone()), short);

        // A single line longer than the whole budget has no line boundary to
        // land on: the cut backs off to a char boundary and still says so.
        let unbroken = "文".repeat(MAX_HISTORY_BYTES);
        let capped = cap_history_bytes(unbroken);
        assert!(capped.len() <= MAX_HISTORY_BYTES, "{}", capped.len());
        assert!(capped.ends_with(&marker), "explicit marker required");
    }

    #[test]
    fn history_label_lines_stay_under_the_per_line_cap() {
        let long = "x".repeat(400);
        let message = assistant(vec![
            tool_call("read", serde_json::json!({ "path": long.clone() })),
            tool_call("bash", serde_json::json!({ "command": long.clone() })),
        ]);
        let label = history_message_label(&message);
        assert!(
            label
                .lines()
                .all(|line| line.chars().count() <= HISTORY_LABEL_MAX_CHARS),
            "{label}"
        );

        // Continuation rows respect the same per-line cap, indent included, so
        // the renderer never receives an over-wide row from either producer.
        let wide = (1..=8).map(|_| long.clone()).collect::<Vec<_>>().join("\n");
        for message in [
            assistant(vec![ContentBlock::text(wide.as_str())]),
            tool_result("bash", wide.as_str(), false),
        ] {
            let label = history_message_label(&message);
            assert!(
                label.lines().count() > 1,
                "fixture must preview rows: {label}"
            );
            assert!(
                label
                    .lines()
                    .all(|line| line.chars().count() <= HISTORY_LABEL_MAX_CHARS),
                "{label}"
            );
        }
    }

    #[test]
    fn rendered_history_redacts_secrets_inside_continuations() {
        let secret = ["s", "k-", "abcdefghijklmnop1234"].concat();
        let root = tempfile::tempdir().expect("root");
        let path = root.path().join("history.json");
        let messages = vec![tool_result(
            "bash",
            &format!("wrote {secret} to disk\nsecond output row"),
            false,
        )];
        fs::write(&path, serde_json::to_vec(&messages).expect("snapshot")).expect("write history");
        let capabilities = CapabilityFs::new().expect("capability filesystem");
        let file = capabilities
            .open_exact_file(
                CapabilityRootPurpose::OrchestrationArtifacts,
                pi_sandbox::AbsolutePath::parse(path).expect("absolute history path"),
            )
            .expect("history capability");
        let retained = RetainedHistoryFile::Artifact(Arc::new(file));
        let rendered = render_history_file(&retained, HISTORY_TOOL_OUTPUT_PREVIEW_LINES + 1)
            .expect("render");
        assert!(
            rendered.contains("  second output row"),
            "continuations reach the wire: {rendered}"
        );

        // `read_child_history` applies exactly this redaction to the rendered
        // text, so a secret inside a continuation row is scrubbed on the wire.
        let wire = cap_history_bytes(crate::redact::redact_secrets(&rendered));
        assert!(
            !wire.contains(&secret),
            "secret must never reach the wire: {wire}"
        );
        assert!(wire.contains("[REDACTED]"), "{wire}");
    }

    #[test]
    fn render_history_tails_flat_rows_not_messages() {
        let root = tempfile::tempdir().expect("root");
        let path = root.path().join("history.json");
        // One assistant message projects four flat rows (text + three calls);
        // a message-level tail would hand back all four for `lines: 2`.
        let messages = vec![
            pi_ai::Message::user_text("ask", 0),
            assistant(vec![
                ContentBlock::text("line one"),
                tool_call("read", serde_json::json!({ "path": "a.txt" })),
                tool_call("bash", serde_json::json!({ "command": "cargo check" })),
                tool_call("read", serde_json::json!({ "path": "b.txt" })),
            ]),
        ];
        fs::write(&path, serde_json::to_vec(&messages).expect("snapshot")).expect("write history");
        let capabilities = CapabilityFs::new().expect("capability filesystem");
        let file = capabilities
            .open_exact_file(
                CapabilityRootPurpose::OrchestrationArtifacts,
                pi_sandbox::AbsolutePath::parse(path).expect("absolute history path"),
            )
            .expect("history capability");
        let retained = RetainedHistoryFile::Artifact(Arc::new(file));
        let rendered = render_history_file(&retained, 2).expect("render");
        assert_eq!(
            rendered.lines().count(),
            2,
            "lines bound must count flat rows: {rendered}"
        );
        assert!(
            rendered.contains("cargo check"),
            "last two flat rows kept: {rendered}"
        );
        assert!(
            rendered.contains("read b.txt"),
            "last two flat rows kept: {rendered}"
        );
        assert!(
            !rendered.contains("a.txt"),
            "older rows dropped: {rendered}"
        );
        assert!(
            !rendered.contains("line one"),
            "older rows dropped: {rendered}"
        );
    }

    #[test]
    fn long_assistant_prose_is_preserved_with_its_ending() {
        // Beyond the old 120-char label cut a paragraph reaches the details
        // page whole — ending included — and a reply with more lines than
        // the old four-row preview keeps every line without a cut marker.
        let paragraph = "The child retried the flaky integration suite twice, isolated the race to the mailbox drain path, and landed a fix that keeps delivery exactly-once while shrinking settled bookkeeping.";
        assert!(paragraph.chars().count() > 120);
        let label = history_message_label(&assistant(vec![ContentBlock::text(paragraph)]));
        assert!(label.contains(paragraph), "paragraph was cut: {label}");
        assert!(!label.contains('…'), "{label}");
        let reply = (1..=12)
            .map(|index| format!("reasoning step {index:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        let label = history_message_label(&assistant(vec![ContentBlock::text(reply)]));
        let lines = rows(&label);
        assert_eq!(lines.len(), 12, "{label}");
        assert_eq!(lines[11], "  reasoning step 12", "{label}");
        assert!(!label.contains(HISTORY_PREVIEW_TRUNCATED_MARKER), "{label}");
    }

    #[test]
    fn long_tool_targets_are_preserved_in_history_and_stay_bounded() {
        // The concrete action is the point of a tool row: targets longer than
        // the old compact-60 title cut stay readable in history (the public
        // title contract itself is unchanged); anything beyond the raised
        // budget is cut safely at a marked boundary.
        let relative_path = "crates/pi-coding/src/orchestration/fixtures/nested/deep/module/history_rendering_fixture.rs";
        let command = "cargo test -p pi-coding orchestration::runtime::history_label -- --nocapture --test-threads=1";
        let label = history_message_label(&assistant(vec![
            tool_call("read", serde_json::json!({ "path": relative_path })),
            tool_call(
                "grep",
                serde_json::json!({
                    "pattern": "exactly-once delivery with mailbox drain race and settled bookkeeping regression"
                }),
            ),
            tool_call("bash", serde_json::json!({ "command": command })),
        ]));
        assert!(label.contains(relative_path), "{label}");
        assert!(label.contains("settled bookkeeping regression"), "{label}");
        assert!(label.contains(command), "{label}");
        assert!(!label.contains("..."), "no silent cut: {label}");
        let oversized = format!("generated/{}", "module/".repeat(400));
        let label = history_message_label(&assistant(vec![tool_call(
            "read",
            serde_json::json!({ "path": oversized }),
        )]));
        assert!(label.ends_with('…'), "marked cut: {label}");
        assert!(
            label.lines().all(|line| line.chars().count() <= HISTORY_LABEL_MAX_CHARS),
            "{label}"
        );
    }

    #[test]
    fn every_branch_scrubs_secrets_before_the_label_cut() {
        // User, custom, bash-command and tool-output rows must all run the
        // secret pass before their own label cut: a credential straddling the
        // cut arrives scrubbed, never as a visible prefix.
        let secret = ["s", "k-", "abcdefghijklmnop1234"].concat();
        let straddling = |prefix: &str| {
            format!(
                "{prefix} {} {secret} {}",
                "x".repeat(HISTORY_LABEL_MAX_CHARS - 30),
                "y".repeat(40)
            )
        };
        let cases = [
            ("user", pi_ai::Message::user_text(straddling("ask"), 0)),
            (
                "custom",
                pi_ai::Message::Custom(pi_ai::CustomMessage {
                    custom_type: "orchestration_message".to_owned(),
                    content: pi_ai::CustomMessageContent::from(straddling("note")),
                    display: true,
                    details: None,
                    timestamp: 0,
                }),
            ),
            (
                "bash",
                pi_ai::Message::BashExecution(pi_ai::BashExecutionMessage {
                    command: straddling("echo"),
                    output: String::new(),
                    exit_code: None,
                    cancelled: false,
                    truncated: false,
                    full_output_path: None,
                    timestamp: 0,
                    exclude_from_context: None,
                }),
            ),
            ("tool", tool_result("bash", &straddling("out"), false)),
        ];
        for (branch, message) in cases {
            let label = history_message_label(&message);
            assert!(
                !label.contains("sk-") && !label.contains(&secret),
                "{branch} leaked a credential across the cut: {label}"
            );
            assert!(
                label.contains("[REDACTED]"),
                "{branch} must scrub before its label cut: {label}"
            );
        }
    }

    #[test]
    fn raised_history_ceiling_keeps_a_wide_transcript_whole() {
        // The byte cap stays a retained backstop (covered by the cap test
        // above); at the raised ceiling a wide multibyte transcript far past
        // the old cap reaches the wire whole, ending included.
        let root = tempfile::tempdir().expect("root");
        let path = root.path().join("history.json");
        let wide = "文".repeat(4000);
        let messages = (1..=40)
            .map(|index| tool_result("bash", &format!("report {index:02} {wide}"), false))
            .collect::<Vec<_>>();
        fs::write(&path, serde_json::to_vec(&messages).expect("snapshot")).expect("write");
        let capabilities = CapabilityFs::new().expect("capability filesystem");
        let file = capabilities
            .open_exact_file(
                CapabilityRootPurpose::OrchestrationArtifacts,
                pi_sandbox::AbsolutePath::parse(path).expect("absolute path"),
            )
            .expect("history capability");
        let rendered = render_history_file(
            &RetainedHistoryFile::Artifact(Arc::new(file)),
            MAX_HISTORY_LINES,
        )
        .expect("render");
        let wire = cap_history_bytes(crate::redact::redact_secrets(&rendered));
        assert!(rendered.len() > 64 * 1024, "{}", rendered.len());
        assert!(
            wire.len() <= MAX_HISTORY_BYTES && !wire.contains("[history truncated"),
            "{}",
            wire.len()
        );
        assert!(wire.contains("report 40"), "ending reaches the wire");
    }

    #[test]
    fn jsonl_non_message_records_scrub_secrets_before_the_label_cut() {
        // Real child-transcript JSONL: non-message records (custom state,
        // compaction) are redacted before their own label cut.
        let secret = ["s", "k-", "abcdefghijklmnop1234"].concat();
        let long = "x".repeat(HISTORY_LABEL_MAX_CHARS - 30);
        let root = tempfile::tempdir().expect("root");
        let path = root.path().join("child.jsonl");
        let header = "{\"type\":\"session\",\"version\":3,\"id\":\"detail-history-parity\",\"timestamp\":\"2026-01-01T00:00:00.000Z\",\"cwd\":\"/w\"}";
        let custom = format!("{{\"type\":\"custom_message\",\"id\":\"c1\",\"parentId\":null,\"timestamp\":\"2026-01-01T00:00:01.000Z\",\"customType\":\"note\",\"content\":\"note {long} {secret} tail\",\"display\":true,\"details\":{{}}}}");
        let compaction = format!("{{\"type\":\"compaction\",\"id\":\"c2\",\"parentId\":\"c1\",\"timestamp\":\"2026-01-01T00:00:02.000Z\",\"summary\":\"compacted {long} {secret} tail\"}}");
        fs::write(&path, format!("{header}\n{custom}\n{compaction}\n")).expect("write jsonl");
        let capabilities = CapabilityFs::new().expect("capability filesystem");
        let file = capabilities
            .open_exact_file(
                CapabilityRootPurpose::SessionStore,
                pi_sandbox::AbsolutePath::parse(path).expect("absolute path"),
            )
            .expect("history capability");
        let rendered = render_history_file(
            &RetainedHistoryFile::Session(Arc::new(file)),
            MAX_HISTORY_LINES,
        )
        .expect("render");
        assert!(
            !rendered.contains(&secret) && !rendered.contains("sk-"),
            "non-message record leaked a credential: {rendered}"
        );
        assert!(rendered.contains("custom: "), "{rendered}");
        assert!(rendered.contains("[compaction] "), "{rendered}");
        assert!(rendered.contains("[REDACTED]"), "{rendered}");
    }
}

impl OrchestrationRuntime {
    pub(crate) fn generated_agent_id(&self, index: usize) -> String {
        let suffix = Uuid::now_v7().simple().to_string();
        format!("Agent{}-{}", index + 1, &suffix[..8])
    }

    fn allocate_unique_agent_id_for_definition(
        &self,
        requested: &str,
        definition: &AgentDefinition,
        reserved: &mut std::collections::BTreeSet<String>,
    ) -> Result<String> {
        if !definition.is_persona() {
            return self.allocate_unique_agent_id(requested, reserved);
        }
        let persona = &definition.name;
        validate_agent_id(requested)?;
        let mut suffix = 1u32;
        loop {
            let candidate = if suffix == 1 {
                requested.to_owned()
            } else {
                format!("{requested}_{suffix}")
            };
            validate_agent_id(&candidate)?;
            let relative = Path::new("sessions").join(format!("{candidate}.jsonl"));
            let archive_exists = match self.inner.storage.persona_roots[persona].metadata(&relative)
            {
                Ok(metadata) if metadata.identity.kind() == EntryKind::RegularFile => true,
                Ok(_) => bail!("persona archive for {candidate:?} is not a regular file"),
                Err(error) if fs_missing(&error) => false,
                Err(error) => return Err(error).context("reading persona archive identity"),
            };
            if !archive_exists && !self.agent_id_is_taken(&candidate, reserved) {
                reserved.insert(candidate.clone());
                return Ok(candidate);
            }
            suffix = suffix
                .checked_add(1)
                .ok_or_else(|| anyhow!("exhausted unique agent id suffixes for {requested:?}"))?;
        }
    }

    /// Prefer `requested` as the agent id; on collision with registry entries,
    /// live/retained job identifiers, or ids already claimed in this batch,
    /// allocate `{base}_2`, `{base}_3`, … under the caller-held spawn lock.
    fn allocate_unique_agent_id(
        &self,
        requested: &str,
        reserved: &mut std::collections::BTreeSet<String>,
    ) -> Result<String> {
        validate_agent_id(requested)?;
        let base = requested;
        let mut suffix = 2u32;
        loop {
            let candidate = if suffix == 2 && !self.agent_id_is_taken(base, reserved) {
                base.to_owned()
            } else {
                let candidate = format!("{base}_{suffix}");
                validate_agent_id(&candidate)?;
                if self.agent_id_is_taken(&candidate, reserved) {
                    suffix = suffix.checked_add(1).ok_or_else(|| {
                        anyhow!("exhausted unique agent id suffixes for {base:?}")
                    })?;
                    continue;
                }
                candidate
            };
            reserved.insert(candidate.clone());
            return Ok(candidate);
        }
    }

    fn agent_id_is_taken(&self, id: &str, reserved: &std::collections::BTreeSet<String>) -> bool {
        reserved.contains(id)
            || self.inner.registry.get(self.group_id(), id).is_some()
            || self.inner.jobs.contains_identifier(id)
    }

    pub(crate) fn task_spawns_text(spawns: &[TaskSpawn]) -> String {
        spawns
            .iter()
            .map(|spawn| {
                format!(
                    "[{}] {} ({}) — queued as job {}",
                    spawn.index, spawn.agent_id, spawn.agent, spawn.job_id
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub(crate) fn task_results_text(results: &[TaskResult]) -> String {
        let mut lines = Vec::new();
        for result in results {
            let status = match result.status {
                AgentStatus::Queued => "queued",
                AgentStatus::Running => "running",
                AgentStatus::Idle => "completed",
                AgentStatus::Parked => "parked",
                AgentStatus::Aborted => "aborted",
            };
            lines.push(format!(
                "[{}] {} ({}) — {}\n{}\nArtifacts: {}, {}, {}",
                result.index,
                result.id,
                result.agent,
                status,
                if result.output.is_empty() {
                    result.error.as_deref().unwrap_or("(no output)")
                } else {
                    &result.output
                },
                result.artifact_ref,
                result.history_ref,
                result.artifact_uri,
            ));
        }
        lines.join("\n\n")
    }
}

/// Test helper: register an agent in this runtime's group and (optionally) arm
/// its settled history path via [`OrchestrationRuntime::finish_agent`]. Used by
/// `hub read_history` tests in `tools.rs` that exercise the history snapshot
/// path without a full durable binding.
#[cfg(test)]
pub(crate) fn register_test_agent(
    runtime: &OrchestrationRuntime,
    id: &str,
    history_path: Option<PathBuf>,
) {
    runtime
        .inner
        .registry
        .register(
            runtime.group_id(),
            AgentSnapshot {
                id: id.to_owned(),
                display_name: id.to_owned(),
                agent: "task".to_owned(),
                parent_id: Some("Main".to_owned()),
                status: AgentStatus::Idle,
                created_at: 1,
                last_activity: 1,
                unread: 0,
                artifact_ref: None,
                history_ref: None,
            },
            runtime.inner.config.mailbox_capacity,
        )
        .expect("register test agent");
    if let Some(path) = history_path {
        runtime
            .finish_agent(id, AgentStatus::Idle, None, Some(path))
            .expect("finish test agent");
    }
}
#[cfg(test)]
struct IsolatedSessionFixture {
    _agent: tempfile::TempDir,
    _sessions: tempfile::TempDir,
    _application: crate::Application,
    _faux: pi_ai::providers::FauxProviderRegistration,
    session: Session,
}

#[cfg(test)]
async fn isolated_session(
    root: &Path,
    tools: Vec<AgentTool>,
    stream_fn: Option<pi_agent::StreamFn>,
) -> IsolatedSessionFixture {
    let agent = tempfile::tempdir().expect("agent dir");
    std::fs::write(
        agent.path().join("settings.json"),
        r#"{"sandbox":{"mode":"danger-full-access"}}"#,
    )
    .expect("isolated sandbox settings");
    let sessions = tempfile::tempdir().expect("session dir");
    let mut resource_options = crate::ResourceManagerOptions::new(root);
    resource_options.agent_dir = agent.path().to_path_buf();
    resource_options.project_trust_override = Some(true);
    resource_options.disable_extensions = true;
    resource_options.disable_skills = true;
    resource_options.disable_prompt_templates = true;
    resource_options.disable_themes = true;
    resource_options.disable_context_files = true;
    let resources = crate::ResourceManager::new(resource_options).expect("resources");
    let suffix = uuid::Uuid::now_v7().simple().to_string();
    let model = pi_ai::Model {
        id: format!("orchestration-fixture-{suffix}"),
        name: "Orchestration Fixture".to_owned(),
        api: format!("orchestration-fixture-api-{suffix}"),
        provider: format!("orchestration-fixture-provider-{suffix}"),
        base_url: "http://localhost:0".to_owned(),
        ..pi_ai::Model::default()
    };
    let faux = pi_ai::providers::register_faux_provider(pi_ai::providers::FauxProviderOptions {
        api: model.api.clone(),
        provider: model.provider.clone(),
        models: vec![model.clone()],
        chunk_size: 12,
    });
    let workspace = crate::WorkspaceRoots::new(root, Vec::<PathBuf>::new()).expect("workspace");
    let session = Session::new_with_additional_tools_filtered_discovery_workspace_and_uri(
        crate::SessionOptions {
            model,
            cwd: root.to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(tools),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn,
            auth_resolver: None,
            auth_manager: None,
        },
        Vec::new(),
        crate::ToolSelection::default(),
        crate::ResourceDiscovery::Disabled,
        workspace,
        None,
    )
    .expect("session");
    session.set_session_dir(sessions.path().to_path_buf());
    session
        .attach_resources(resources)
        .await
        .expect("attach resources");
    let application = crate::Application::new(session.clone()).await;
    IsolatedSessionFixture {
        _agent: agent,
        _sessions: sessions,
        _application: application,
        _faux: faux,
        session,
    }
}

#[cfg(test)]
mod prompt_size_tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn retention_runtime(
        root: &Path,
        now: Arc<AtomicU64>,
        max_retained: usize,
        ttl: Duration,
    ) -> OrchestrationRuntime {
        let definition = super::super::definitions::parse_agent_definition(
            Path::new("task.md"),
            "---\nname: task\ndescription: task\n---\nprompt",
            super::super::AgentDefinitionSource::Bundled,
            true,
        )
        .expect("definition");
        let clock_now = now.clone();
        let config = OrchestrationConfig::new(AgentCatalog::from_agents(vec![definition]), root)
            .with_job_retention(max_retained, ttl)
            .with_job_clock(Arc::new(move || clock_now.load(Ordering::SeqCst)));
        OrchestrationRuntime::new(config, Arc::new(|_| Box::pin(async { unreachable!() })))
            .expect("runtime")
    }

    fn register_peer(
        runtime: &OrchestrationRuntime,
        id: &str,
        agent: &str,
        parent_id: Option<&str>,
        status: AgentStatus,
    ) {
        runtime
            .inner
            .registry
            .register(
                runtime.group_id(),
                AgentSnapshot {
                    id: id.to_owned(),
                    display_name: "untrusted <display>".to_owned(),
                    agent: agent.to_owned(),
                    parent_id: parent_id.map(str::to_owned),
                    status,
                    created_at: 1,
                    last_activity: 1,
                    unread: 0,
                    artifact_ref: None,
                    history_ref: None,
                },
                runtime.inner.config.mailbox_capacity,
            )
            .expect("register peer");
    }

    fn prompt_runtime(root: &Path) -> OrchestrationRuntime {
        retention_runtime(
            root,
            Arc::new(AtomicU64::new(1)),
            DEFAULT_MAX_RETAINED_JOBS,
            Duration::from_secs(DEFAULT_RETAINED_JOB_TTL_SECS),
        )
    }
    #[test]
    fn delivered_message_log_survives_mailbox_consumption() {
        // Every durably delivered group message (subagent ⇄ subagent
        // included) lands in the bounded delivered log that the workflow
        // page's Recent IRC reads; draining the recipient's mailbox must not
        // erase it, so consumed messages stay visible.
        let root = tempfile::tempdir().expect("root");
        let runtime = prompt_runtime(root.path());
        // "Main" is pre-registered by the runtime itself.
        register_peer(
            &runtime,
            "WorkerA",
            "task",
            Some("Main"),
            AgentStatus::Parked,
        );
        register_peer(
            &runtime,
            "WorkerB",
            "task",
            Some("Main"),
            AgentStatus::Parked,
        );

        runtime.send("WorkerA", "WorkerB", "results ready", None);
        runtime.send("WorkerB", "WorkerA", "acknowledged", None);
        runtime.send("WorkerA", "Main", "progress", None);

        let log = runtime.delivered_messages();
        assert_eq!(log.len(), 3, "every delivered message must be logged");
        assert_eq!(log[0].from, "WorkerA");
        assert_eq!(log[0].to, "WorkerB");
        assert_eq!(log[0].body, "results ready");
        assert_eq!(log[1].from, "WorkerB");
        assert_eq!(log[1].to, "WorkerA");
        assert_eq!(log[2].from, "WorkerA");
        assert_eq!(log[2].to, "Main");

        // The recipient consumes its mailbox; the log is unchanged.
        assert_eq!(runtime.inbox("WorkerB", false).expect("drain").len(), 1);
        assert_eq!(runtime.inbox("WorkerB", true).expect("peek").len(), 0);
        assert_eq!(
            runtime.delivered_messages().len(),
            3,
            "consumption must not erase the log"
        );
        assert!(
            runtime
                .delivered_messages()
                .iter()
                .any(|message| message.body == "results ready")
        );
    }

    #[test]
    fn delivered_message_log_deduplicates_and_retains_newest_bound() {
        let root = tempfile::tempdir().expect("root");
        let runtime = prompt_runtime(root.path());
        let duplicate_id = "message-duplicate";
        runtime.record_delivered_message(MailboxMessage {
            id: duplicate_id.to_owned(),
            from: "Main".to_owned(),
            to: "Main".to_owned(),
            body: "old".to_owned(),
            timestamp: 1,
            reply_to: None,
        });
        for index in 0..DELIVERED_MESSAGE_LOG_CAP {
            runtime.record_delivered_message(MailboxMessage {
                id: format!("message-{index}"),
                from: "Main".to_owned(),
                to: "Main".to_owned(),
                body: format!("body-{index}"),
                timestamp: index as u64 + 2,
                reply_to: None,
            });
        }
        runtime.record_delivered_message(MailboxMessage {
            id: duplicate_id.to_owned(),
            from: "Main".to_owned(),
            to: "Main".to_owned(),
            body: "new".to_owned(),
            timestamp: 500,
            reply_to: None,
        });

        let log = runtime.delivered_messages();
        assert_eq!(log.len(), DELIVERED_MESSAGE_LOG_CAP);
        assert_eq!(log.last().expect("newest message").id, duplicate_id);
        assert_eq!(log.last().expect("newest message").body, "new");
        assert_eq!(
            log.iter()
                .filter(|message| message.id == duplicate_id)
                .count(),
            1,
            "duplicate message ids are replaced, never retained twice"
        );
        assert!(
            !log.iter().any(|message| message.id == "message-0"),
            "the oldest message is evicted when the bounded log overflows"
        );
    }

    #[tokio::test]
    async fn broadcast_reports_processed_queued_and_failed_per_recipient() {
        let root = tempfile::tempdir().expect("root");
        let runtime = prompt_runtime(root.path());
        register_peer(&runtime, "Waiter", "task", Some("Main"), AgentStatus::Idle);
        register_peer(&runtime, "Queued", "task", Some("Main"), AgentStatus::Idle);
        register_peer(&runtime, "Full", "task", Some("Main"), AgentStatus::Idle);
        register_peer(
            &runtime,
            "Aborted",
            "task",
            Some("Main"),
            AgentStatus::Aborted,
        );
        for index in 0..runtime.inner.config.mailbox_capacity {
            let receipt = runtime.send("Main", "Full", &format!("fill-{index}"), None);
            assert_eq!(receipt[0].outcome, DeliveryOutcome::Queued);
        }
        let waiter_runtime = runtime.clone();
        let waiter = tokio::spawn(async move {
            waiter_runtime
                .wait_message("Waiter", Some("Main"), Some(Duration::from_secs(1)), None)
                .await
        });
        tokio::task::yield_now().await;

        let receipts = runtime.send("Main", "all", "broadcast", None);
        let outcomes = receipts
            .iter()
            .map(|receipt| (receipt.to.as_str(), receipt.outcome))
            .collect::<HashMap<_, _>>();
        assert_eq!(outcomes.get("Waiter"), Some(&DeliveryOutcome::Processed));
        assert_eq!(outcomes.get("Queued"), Some(&DeliveryOutcome::Queued));
        assert_eq!(outcomes.get("Full"), Some(&DeliveryOutcome::Failed));
        assert!(
            !outcomes.contains_key("Aborted"),
            "broadcast excludes aborted recipients rather than reporting a false delivery"
        );
        let message = waiter
            .await
            .expect("waiter join")
            .expect("wait result")
            .expect("broadcast handoff");
        assert_eq!(message.body, "broadcast");
        assert_eq!(
            runtime.inbox("Queued", true).expect("queued inbox").len(),
            1
        );
    }

    #[test]
    fn send_rejects_blank_oversized_invalid_and_unknown_senders() {
        let root = tempfile::tempdir().expect("root");
        let runtime = prompt_runtime(root.path());
        register_peer(&runtime, "Worker", "task", Some("Main"), AgentStatus::Idle);

        let blank = runtime.send("Main", "Worker", "   ", None);
        assert_eq!(blank[0].outcome, DeliveryOutcome::Failed);
        assert!(
            blank[0]
                .error
                .as_deref()
                .is_some_and(|error| error.contains("must not be empty"))
        );

        let oversized_body = "x".repeat(MAX_MESSAGE_BODY_BYTES + 1);
        let oversized = runtime.send("Main", "Worker", &oversized_body, None);
        assert_eq!(oversized[0].outcome, DeliveryOutcome::Failed);
        assert!(
            oversized[0]
                .error
                .as_deref()
                .is_some_and(|error| error.contains("exceeds maximum"))
        );

        let invalid = runtime.send("bad sender", "Worker", "hello", None);
        assert_eq!(invalid[0].outcome, DeliveryOutcome::Failed);
        assert!(
            invalid[0]
                .error
                .as_deref()
                .is_some_and(|error| error.contains("invalid orchestration sender"))
        );

        let unknown = runtime.send("Missing", "Worker", "hello", None);
        assert_eq!(unknown[0].outcome, DeliveryOutcome::Failed);
        assert!(
            unknown[0]
                .error
                .as_deref()
                .is_some_and(|error| error.contains("unknown orchestration sender"))
        );
        assert!(
            runtime
                .inbox("Worker", true)
                .expect("worker inbox")
                .is_empty()
        );
    }

    #[test]
    fn inbox_rejects_unknown_agents_and_rebind_window() {
        let root = tempfile::tempdir().expect("root");
        let runtime = prompt_runtime(root.path());
        let unknown = runtime
            .inbox("Missing", true)
            .expect_err("unknown inbox must propagate an error");
        assert!(
            unknown.to_string().contains("unknown orchestration agent"),
            "{unknown:#}"
        );

        runtime.inner.rebind_reserved.store(true, Ordering::Release);
        let peek = runtime
            .inbox("Main", true)
            .expect_err("peek must fail closed during rebind");
        assert!(
            peek.to_string().contains("rebind is in progress"),
            "{peek:#}"
        );
        let drain = runtime
            .inbox("Main", false)
            .expect_err("drain must fail closed during rebind");
        assert!(
            drain.to_string().contains("rebind is in progress"),
            "{drain:#}"
        );
        runtime
            .inner
            .rebind_reserved
            .store(false, Ordering::Release);
    }

    #[tokio::test]
    async fn wait_reply_claims_only_the_matching_correlation_id() {
        let root = tempfile::tempdir().expect("root");
        let runtime = prompt_runtime(root.path());
        register_peer(&runtime, "Worker", "task", Some("Main"), AgentStatus::Idle);
        let ask = runtime.send("Main", "Worker", "what is the path?", None);
        let message_id = ask[0].message_id.clone().expect("ask correlation id");
        runtime.send("Worker", "Main", "unrelated chatter", None);
        runtime.send(
            "Worker",
            "Main",
            "the path is src/lib.rs",
            Some(message_id.clone()),
        );
        let reply = runtime
            .wait_reply(
                "Main",
                Some("Worker"),
                &message_id,
                Some(Duration::from_secs(1)),
                None,
            )
            .await
            .expect("wait")
            .expect("correlated reply");
        assert_eq!(reply.body, "the path is src/lib.rs");
        assert_eq!(reply.reply_to.as_deref(), Some(message_id.as_str()));
        let leftover = runtime.inbox("Main", true).expect("peek");
        assert_eq!(
            leftover.len(),
            1,
            "unrelated mail stays in the mailbox: {leftover:?}"
        );
        assert_eq!(leftover[0].body, "unrelated chatter");
    }

    #[test]
    fn mailbox_restore_is_forced_and_dedupes_across_full_mailbox() {
        // A run-end restore reverses an earlier removal (bridge ack or waiter
        // handoff): it must never be rejected by the mailbox bound — the
        // in-flight capacity the message occupied is being returned — and a
        // duplicate of the same id is replaced, never doubled.
        let root = tempfile::tempdir().expect("root");
        let runtime = prompt_runtime(root.path());
        register_peer(&runtime, "Cap", "task", Some("Main"), AgentStatus::Idle);
        let capacity = runtime.inner.config.mailbox_capacity;
        for index in 0..capacity {
            runtime.send("Main", "Cap", &format!("fill-{index}"), None);
        }
        assert_eq!(
            runtime.inbox("Cap", true).expect("peek").len(),
            capacity,
            "mailbox at capacity"
        );
        assert_eq!(
            runtime.send("Main", "Cap", "overflow", None)[0].outcome,
            DeliveryOutcome::Failed,
            "ordinary sends still respect the bound"
        );
        let restored = MailboxMessage {
            id: "restored-1".to_owned(),
            from: "Main".to_owned(),
            to: "Cap".to_owned(),
            body: "in flight".to_owned(),
            timestamp: now_millis(),
            reply_to: None,
        };
        assert!(
            runtime.inner.registry.restore_mailbox_message(
                runtime.group_id(),
                "Cap",
                restored.clone()
            ),
            "restore must succeed even at capacity"
        );
        let inbox = runtime.inbox("Cap", true).expect("peek");
        assert_eq!(
            inbox.len(),
            capacity + 1,
            "restore returns the in-flight slot"
        );
        assert_eq!(
            inbox[0].id, "restored-1",
            "restored message returns to the front"
        );
        // Restoring the same committed message again replaces it, never doubles.
        assert!(runtime.inner.registry.restore_mailbox_message(
            runtime.group_id(),
            "Cap",
            restored.clone()
        ));
        let inbox = runtime.inbox("Cap", true).expect("peek");
        assert_eq!(inbox.len(), capacity + 1, "id-dedup restore");
        assert_eq!(
            inbox
                .iter()
                .filter(|message| message.id == "restored-1")
                .count(),
            1,
            "exactly one copy of the restored message"
        );
    }

    #[tokio::test]
    async fn restore_persistence_failure_surfaces_while_memory_restore_stands() {
        // A run-end restore of a durably committed message must never swallow
        // a persistence failure: the error surfaces (the run records it) while
        // the in-memory restore keeps the message deliverable.
        let root = tempfile::tempdir().expect("root");
        let parent_dir = tempfile::tempdir().expect("parent sessions");
        let recorder = crate::session_store::start_session_in(
            root.path(),
            Some(&pi_ai::Model::default()),
            None,
            Some(parent_dir.path()),
            None,
            None,
        )
        .expect("recorder");
        let parent_fixture = isolated_session(root.path(), Vec::new(), None).await;
        let parent = &parent_fixture.session;
        parent.record(recorder).expect("record");
        let runtime = prompt_runtime(root.path());
        runtime.bind_and_recover(parent).expect("bind durable");
        assert!(runtime.is_durable(), "runtime must be durable-bound");
        // Main is pre-registered and excluded from the durable state. The raw
        // peer fixture must keep its role equal to its id so its synthesized
        // durable definition remains validation-consistent.
        register_peer(
            &runtime,
            "Worker",
            "Worker",
            Some("Main"),
            AgentStatus::Idle,
        );
        let receipt = runtime.send("Worker", "Main", "committed", None);
        assert!(
            receipt[0].error.is_none(),
            "setup send must persist: {receipt:?}"
        );
        assert_eq!(runtime.inbox("Main", true).expect("peek").len(), 1);
        // Simulate the bridge acknowledgement (durable removal), then break
        // the sidecar so the restore persist must fail.
        let message = runtime.inbox("Main", false).expect("drain").remove(0);
        let sidecar = runtime
            .inner
            .durable
            .lock()
            .as_ref()
            .expect("durable binding")
            .sidecar_path()
            .to_path_buf();
        std::fs::remove_file(&sidecar).expect("remove sidecar");
        std::fs::create_dir(&sidecar).expect("sidecar path becomes directory");
        let error = runtime
            .restore_unprocessed_delivery("Main", message.clone())
            .expect_err("persistence failure must surface, never be swallowed");
        assert!(
            error.to_string().to_lowercase().contains("persist")
                || error.to_string().to_lowercase().contains("restor"),
            "{error:#}"
        );
        let inbox = runtime.inbox("Main", true).expect("peek");
        assert_eq!(inbox.len(), 1, "the in-memory restore must stand");
        assert_eq!(inbox[0].id, message.id);
    }

    #[tokio::test]
    async fn acknowledge_persistence_failure_restores_child_mailbox_once() {
        let root = tempfile::tempdir().expect("root");
        let parent_dir = tempfile::tempdir().expect("parent sessions");
        let recorder = crate::session_store::start_session_in(
            root.path(),
            Some(&pi_ai::Model::default()),
            None,
            Some(parent_dir.path()),
            None,
            None,
        )
        .expect("recorder");
        let parent_fixture = isolated_session(root.path(), Vec::new(), None).await;
        let parent = &parent_fixture.session;
        parent.record(recorder).expect("record");
        let runtime = prompt_runtime(root.path());
        runtime.bind_and_recover(parent).expect("bind durable");
        register_peer(
            &runtime,
            "Worker",
            "Worker",
            Some("Main"),
            AgentStatus::Idle,
        );
        let receipt = runtime.send("Main", "Worker", "ack-fail", None);
        assert!(
            receipt[0].error.is_none(),
            "setup send must persist: {receipt:?}"
        );
        let message_id = receipt[0].message_id.as_deref().expect("message id");
        let sidecar = runtime
            .inner
            .durable
            .lock()
            .as_ref()
            .expect("durable binding")
            .sidecar_path();
        std::fs::remove_file(&sidecar).expect("remove sidecar");
        std::fs::create_dir(&sidecar).expect("block sidecar writes");

        let error = runtime
            .acknowledge_delivery("Worker", message_id)
            .expect_err("ack persistence failure must propagate");
        assert!(error.to_string().contains("persist"), "{error:#}");
        let inbox = runtime
            .inbox("Worker", true)
            .expect("peek restored mailbox");
        assert_eq!(inbox.len(), 1, "ack failure restores exactly one copy");
        assert_eq!(inbox[0].id, message_id);
    }

    #[tokio::test]
    async fn handed_off_wait_persist_failure_restores_and_republishes() {
        // When the waiter's consumption persist fails after an atomic
        // handoff, the message is force-restored to the mailbox (exactly one
        // copy) and Main-target deliveries are republished non-claimed so the
        // application still steers them (the original claimed event skipped
        // steering, so without the republish the reply would be stranded).
        let root = tempfile::tempdir().expect("root");
        let parent_dir = tempfile::tempdir().expect("parent sessions");
        let recorder = crate::session_store::start_session_in(
            root.path(),
            Some(&pi_ai::Model::default()),
            None,
            Some(parent_dir.path()),
            None,
            None,
        )
        .expect("recorder");
        let parent_fixture = isolated_session(root.path(), Vec::new(), None).await;
        let parent = &parent_fixture.session;
        parent.record(recorder).expect("record");
        let runtime = prompt_runtime(root.path());
        runtime.bind_and_recover(parent).expect("bind durable");
        let mut events = runtime.subscribe();
        register_peer(
            &runtime,
            "Worker",
            "Worker",
            Some("Main"),
            AgentStatus::Idle,
        );
        let mut waiter = runtime
            .register_waiter("Main", Some("Worker"), None)
            .expect("register waiter before send");
        // Registration is synchronous, so the send can atomically hand the
        // committed message to this exact waiter without a scheduling race.
        let receipt = runtime.send("Worker", "Main", "handoff-persist-fail", None);
        assert_eq!(receipt[0].outcome, DeliveryOutcome::Processed);
        // Break the sidecar so the waiter's consumption persist must fail.
        let sidecar = runtime
            .inner
            .durable
            .lock()
            .as_ref()
            .expect("durable binding")
            .sidecar_path()
            .to_path_buf();
        std::fs::remove_file(&sidecar).expect("remove sidecar");
        std::fs::create_dir(&sidecar).expect("sidecar path becomes directory");
        let wait_result = runtime
            .wait_with_guard(
                "Main",
                Some("Worker"),
                None,
                &mut waiter,
                Some(Duration::from_secs(3)),
                None,
            )
            .await;
        assert!(
            wait_result.is_err(),
            "consumption persist failure must surface: {wait_result:?}"
        );
        assert_eq!(
            runtime.inbox("Main", true).expect("peek").len(),
            1,
            "exactly one restored copy of the handed-off message"
        );
        // The original claimed event AND the republished non-claimed event
        // both arrive (the latter drives the application steer).
        let mut saw_claimed = false;
        let mut saw_republished = false;
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                match events.recv().await {
                    Ok(OrchestrationEvent::MessageDelivered {
                        message,
                        waiter_claimed,
                        ..
                    }) if message.body == "handoff-persist-fail" => {
                        if waiter_claimed {
                            saw_claimed = true;
                        } else {
                            saw_republished = true;
                        }
                        if saw_claimed && saw_republished {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        })
        .await
        .expect("claimed and republished events");
        assert!(saw_claimed, "original claimed event must publish");
        assert!(
            saw_republished,
            "republished non-claimed event must publish"
        );
    }

    #[test]
    fn child_prompt_includes_stable_live_peer_roster_and_excludes_self() {
        let root = tempfile::tempdir().expect("root");
        let runtime = prompt_runtime(root.path());
        register_peer(
            &runtime,
            "Zulu",
            "reviewer<&\"",
            Some("Main"),
            AgentStatus::Running,
        );
        register_peer(
            &runtime,
            "peer_token_abcdefgh",
            "security",
            Some("Main"),
            AgentStatus::Running,
        );
        register_peer(
            &runtime,
            "Alpha",
            "researcher",
            Some("Parent"),
            AgentStatus::Queued,
        );
        register_peer(
            &runtime,
            "Pending",
            "task",
            Some("Main"),
            AgentStatus::Running,
        );
        register_peer(
            &runtime,
            "Finished",
            "writer",
            Some("Main"),
            AgentStatus::Idle,
        );
        register_peer(
            &runtime,
            "Retained",
            "reviewer",
            Some("Main"),
            AgentStatus::Parked,
        );
        register_peer(
            &runtime,
            "Aborted",
            "task",
            Some("Main"),
            AgentStatus::Aborted,
        );

        let definition = runtime.catalog().get("task").expect("agent");
        let roster = runtime.sibling_roster("Pending");
        let prompt = runtime
            .child_system_prompt(definition, "inspect assignment", None, None, None, &roster)
            .expect("prompt");
        let expected = concat!(
            "<peer_roster>\n",
            "This is a spawn-time snapshot. `hub list` refreshes state; `hub send` addresses exact ids.\n",
            "  <peer id=\"Alpha\" agent=\"researcher\" status=\"queued\" parent=\"Parent\" />\n",
            "  <peer id=\"Main\" agent=\"Main\" status=\"idle\" parent=\"none\" />\n",
            "  <peer id=\"Zulu\" agent=\"reviewer&lt;&amp;&quot;\" status=\"running\" parent=\"Main\" />\n",
            "  <peer id=\"peer_token_abcdefgh\" agent=\"security\" status=\"running\" parent=\"Main\" />\n",
            "</peer_roster>",
        );
        let roster_start = prompt.find("<peer_roster>").expect("roster start");
        let roster_end =
            prompt.find("</peer_roster>").expect("roster end") + "</peer_roster>".len();
        assert_eq!(&prompt[roster_start..roster_end], expected);
        assert!(!prompt.contains("untrusted <display>"));
        assert!(!prompt[roster_start..roster_end].contains("Pending"));
        assert!(!prompt[roster_start..roster_end].contains("Finished"));
        assert!(!prompt[roster_start..roster_end].contains("Retained"));
        assert!(!prompt[roster_start..roster_end].contains("Aborted"));
        assert!(roster_start > prompt.find("prompt").expect("definition prompt"));
        assert!(roster_end < prompt.find("<delegated_assignment>").expect("assignment"));
        assert!(prompt.contains("inspect assignment"));
    }

    #[test]
    fn sibling_roster_is_count_and_byte_bounded_while_preserving_main() {
        let root = tempfile::tempdir().expect("root");
        let runtime = prompt_runtime(root.path());
        for index in (0..MAX_SIBLING_ROSTER_ENTRIES + 10).rev() {
            register_peer(
                &runtime,
                &format!("Peer{index:03}"),
                &"a".repeat(MAX_ROSTER_AGENT_CHARS),
                Some("Main"),
                AgentStatus::Running,
            );
        }

        let roster = runtime.sibling_roster("Pending");
        assert!(roster.len() <= MAX_SIBLING_ROSTER_BYTES);
        assert_eq!(
            roster.matches("  <peer ").count(),
            MAX_SIBLING_ROSTER_ENTRIES
        );
        assert!(roster.contains("<peer id=\"Main\""));
        assert!(roster.contains("  <truncated />"));
        let ids = roster
            .lines()
            .filter_map(|line| {
                line.strip_prefix("  <peer id=\"")?
                    .split_once('"')
                    .map(|(id, _)| id)
            })
            .collect::<Vec<_>>();
        assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn sibling_roster_enforces_byte_budget_after_escaping() {
        let root = tempfile::tempdir().expect("root");
        let runtime = prompt_runtime(root.path());
        for index in 0..MAX_SIBLING_ROSTER_ENTRIES - 1 {
            register_peer(
                &runtime,
                &format!("Peer{index:03}"),
                &"<".repeat(MAX_ROSTER_AGENT_CHARS),
                Some("Main"),
                AgentStatus::Running,
            );
        }

        let definition = runtime.catalog().get("task").expect("agent");
        let roster = runtime.sibling_roster("Pending");
        let prompt = runtime
            .child_system_prompt(definition, "work", None, None, None, &roster)
            .expect("prompt");
        let roster_start = prompt.find("<peer_roster>").expect("roster start");
        let roster_end =
            prompt.find("</peer_roster>").expect("roster end") + "</peer_roster>".len();
        let roster = &prompt[roster_start..roster_end];
        assert!(roster.len() <= MAX_SIBLING_ROSTER_BYTES);
        assert!(roster.matches("  <peer ").count() < MAX_SIBLING_ROSTER_ENTRIES);
        assert!(roster.contains("<peer id=\"Main\""));
        assert!(roster.contains("  <truncated />"));
        assert!(roster.contains("agent=\"&lt;&lt;&lt;"));
    }

    #[test]
    fn batch_registration_failure_does_not_insert_any_agents() {
        let root = tempfile::tempdir().expect("root");
        let runtime = prompt_runtime(root.path());
        let snapshot = |id: &str, parent_id: &str| AgentSnapshot {
            id: id.to_owned(),
            display_name: id.to_owned(),
            agent: "task".to_owned(),
            parent_id: Some(parent_id.to_owned()),
            status: AgentStatus::Queued,
            created_at: 1,
            last_activity: 1,
            unread: 0,
            artifact_ref: None,
            history_ref: None,
        };
        let error = runtime
            .inner
            .registry
            .register_batch(
                runtime.group_id(),
                vec![
                    snapshot("WouldInsert", "Main"),
                    snapshot("InvalidParent", "Main\n"),
                ],
                runtime.inner.config.mailbox_capacity,
            )
            .expect_err("invalid batch rejected atomically");
        assert!(error.to_string().contains("invalid parent agent id"));
        assert!(runtime.agent_snapshot("WouldInsert").is_none());
        assert!(runtime.agent_snapshot("InvalidParent").is_none());
    }

    #[test]
    fn registration_rejects_control_characters_in_parent_id() {
        let root = tempfile::tempdir().expect("root");
        let runtime = prompt_runtime(root.path());
        let error = runtime
            .inner
            .registry
            .register(
                runtime.group_id(),
                AgentSnapshot {
                    id: "ValidChild".to_owned(),
                    display_name: "ignored".to_owned(),
                    agent: "task".to_owned(),
                    parent_id: Some("Main\u{0001}".to_owned()),
                    status: AgentStatus::Running,
                    created_at: 1,
                    last_activity: 1,
                    unread: 0,
                    artifact_ref: None,
                    history_ref: None,
                },
                runtime.inner.config.mailbox_capacity,
            )
            .expect_err("control-character parent rejected");
        assert!(error.to_string().contains("invalid parent agent id"));
        assert!(runtime.agent_snapshot("ValidChild").is_none());
    }

    #[tokio::test]
    async fn spawn_passes_registered_child_id_to_roster_builder() {
        let root = tempfile::tempdir().expect("root");
        let captured = Arc::new(Mutex::new(None::<String>));
        let factory_capture = captured.clone();
        let definition = super::super::definitions::parse_agent_definition(
            Path::new("task.md"),
            "---\nname: task\ndescription: task\ntools: []\n---\nprompt",
            super::super::AgentDefinitionSource::Bundled,
            true,
        )
        .expect("definition");
        let runtime = OrchestrationRuntime::new(
            OrchestrationConfig::new(AgentCatalog::from_agents(vec![definition]), root.path()),
            Arc::new(move |request| {
                *factory_capture.lock() = Some(request.system_prompt);
                Box::pin(async { Err(anyhow!("stop after prompt capture")) })
            }),
        )
        .expect("runtime");
        register_peer(
            &runtime,
            "Sibling",
            "reviewer",
            Some("Main"),
            AgentStatus::Running,
        );

        let spawn = runtime
            .spawn_tasks(
                "Main",
                0,
                vec![TaskItem {
                    index: 0,
                    id: "Spawned".to_owned(),
                    agent: "task".to_owned(),
                    assignment: "work".to_owned(),
                    todo_task_id: None,
                    ..Default::default()
                }],
            )
            .expect("spawn")
            .remove(0);
        runtime
            .wait_jobs(&[spawn.job_id], Some(Duration::from_secs(1)), None)
            .await
            .expect("settled");

        let prompt = captured.lock().clone().expect("captured prompt");
        let roster_start = prompt.find("<peer_roster>").expect("roster start");
        let roster_end = prompt.find("</peer_roster>").expect("roster end");
        assert!(!prompt[roster_start..roster_end].contains("Spawned"));
        assert!(prompt[roster_start..roster_end].contains("<peer id=\"Main\""));
        assert!(prompt[roster_start..roster_end].contains(
            "<peer id=\"Sibling\" agent=\"reviewer\" status=\"running\" parent=\"Main\""
        ));
    }

    #[tokio::test]
    async fn spawned_task_description_survives_the_detail_budget() {
        // The public job snapshot is where a coach reads what a spawn was
        // asked to do: bounded redacted 1024 with newlines preserved (not the
        // old collapsed 80-char cut), and the presentation wire keeps the
        // stored text instead of re-cutting it.
        let root = tempfile::tempdir().expect("root");
        let definition = super::super::definitions::parse_agent_definition(
            Path::new("task.md"),
            "---\nname: task\ndescription: task\ntools: []\n---\nprompt",
            super::super::AgentDefinitionSource::Bundled,
            true,
        )
        .expect("definition");
        let runtime = OrchestrationRuntime::new(
            OrchestrationConfig::new(AgentCatalog::from_agents(vec![definition]), root.path()),
            Arc::new(|_| Box::pin(async { Err(anyhow!("stop after spawn")) })),
        )
        .expect("runtime");
        let secret = ["s", "k-", "abcdefghijklmnop1234"].concat();
        let assignment = format!(
            "Investigate the failing subagent detail history rendering\nthe leaked {secret} token must not survive\n{}report the final verdict in the job result",
            "filler ".repeat(20)
        );
        let spawn = runtime
            .spawn_tasks(
                "Main",
                0,
                vec![TaskItem {
                    index: 0,
                    id: "DetailProbe".to_owned(),
                    agent: "task".to_owned(),
                    assignment: assignment.clone(),
                    todo_task_id: None,
                    ..Default::default()
                }],
            )
            .expect("spawn")
            .remove(0);
        let job = runtime
            .jobs(Some(std::slice::from_ref(&spawn.job_id)))
            .into_iter()
            .next()
            .expect("job snapshot");
        let description = job.description.as_deref().expect("task description");
        assert!(
            description
                .starts_with("Investigate the failing subagent detail history rendering\n"),
            "{description}"
        );
        assert!(
            description.ends_with("report the final verdict in the job result"),
            "{description}"
        );
        assert!(
            !description.contains(&secret) && description.contains("[REDACTED]"),
            "{description}"
        );
        assert!(
            description.chars().count() <= 1024,
            "{}",
            description.chars().count()
        );
        let presented = presentation_job_snapshot(job.clone());
        assert_eq!(
            presented.description.as_deref(),
            Some(description),
            "wire keeps the stored description"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn batch_children_each_receive_all_registered_siblings() {
        let root = tempfile::tempdir().expect("root");
        let captured = Arc::new(Mutex::new(BTreeMap::<String, String>::new()));
        let factory_capture = captured.clone();
        let definition = super::super::definitions::parse_agent_definition(
            Path::new("task.md"),
            "---\nname: task\ndescription: task\ntools: []\n---\nprompt",
            super::super::AgentDefinitionSource::Bundled,
            true,
        )
        .expect("definition");
        let runtime = OrchestrationRuntime::new(
            OrchestrationConfig::new(AgentCatalog::from_agents(vec![definition]), root.path()),
            Arc::new(move |request| {
                factory_capture
                    .lock()
                    .insert(request.child_id, request.system_prompt);
                Box::pin(async { Err(anyhow!("stop after prompt capture")) })
            }),
        )
        .expect("runtime");

        let spawns = runtime
            .spawn_tasks(
                "Main",
                0,
                vec![
                    TaskItem {
                        index: 0,
                        id: "First".to_owned(),
                        agent: "task".to_owned(),
                        assignment: "first".to_owned(),
                        todo_task_id: None,
                        ..Default::default()
                    },
                    TaskItem {
                        index: 1,
                        id: "Second".to_owned(),
                        agent: "task".to_owned(),
                        assignment: "second".to_owned(),
                        todo_task_id: None,
                        ..Default::default()
                    },
                ],
            )
            .expect("spawn batch");
        let job_ids = spawns
            .iter()
            .map(|spawn| spawn.job_id.clone())
            .collect::<Vec<_>>();
        runtime
            .wait_jobs(&job_ids, Some(Duration::from_secs(1)), None)
            .await
            .expect("settled");

        let prompts = captured.lock();
        let first = prompts.get("First").expect("first prompt");
        let second = prompts.get("Second").expect("second prompt");
        assert!(
            first.contains("<peer id=\"Second\" agent=\"task\" status=\"queued\" parent=\"Main\"")
        );
        assert!(!first.contains("<peer id=\"First\""));
        assert!(first.contains("<peer id=\"Main\""));
        assert!(
            second.contains("<peer id=\"First\" agent=\"task\" status=\"queued\" parent=\"Main\"")
        );
        assert!(!second.contains("<peer id=\"Second\""));
        assert!(second.contains("<peer id=\"Main\""));
    }

    fn retained_result(id: &str, root: &Path, job_id: &str) -> TaskResult {
        let artifact = root.join(format!("{id}-{job_id}.md"));
        let history = root.join(format!("{id}-{job_id}.history.json"));
        fs::write(&artifact, id).expect("artifact");
        fs::write(&history, "[]").expect("history");
        TaskResult {
            index: 0,
            id: id.to_owned(),
            agent: "task".to_owned(),
            status: AgentStatus::Idle,
            output: id.to_owned(),
            error: None,
            artifact_ref: format!("agent://{id}"),
            history_ref: format!("history://{id}"),
            artifact_uri: format!("artifact://{id}"),
            usage: pi_ai::Usage::default(),
            soft_budget_exhausted: false,
            structured_output: None,
        }
    }

    fn insert_retained_job(runtime: &OrchestrationRuntime, id: &str, job_id: &str, timestamp: u64) {
        runtime
            .inner
            .registry
            .register(
                runtime.group_id(),
                AgentSnapshot {
                    id: id.to_owned(),
                    display_name: id.to_owned(),
                    agent: "task".to_owned(),
                    parent_id: Some("Main".to_owned()),
                    status: AgentStatus::Idle,
                    created_at: timestamp,
                    last_activity: timestamp,
                    unread: 0,
                    artifact_ref: None,
                    history_ref: None,
                },
                runtime.inner.config.mailbox_capacity,
            )
            .expect("register");
        runtime
            .inner
            .jobs
            .insert(
                JobSnapshot {
                    id: job_id.to_owned(),
                    agent_id: id.to_owned(),
                    agent: "task".to_owned(),
                    parent_id: "Main".to_owned(),
                    description: None,
                    todo_task_id: None,
                    workflow_id: None,
                    workflow_generation: None,
                    status: JobStatus::Queued,
                    created_at: timestamp,
                    started_at: None,
                    finished_at: None,
                    result: None,
                    soft_budget_exhausted: false,
                },
                CancellationToken::new(),
            )
            .expect("insert");
        let result = retained_result(id, &runtime.inner.config.artifact_dir, job_id);
        runtime
            .finish_agent(
                id,
                AgentStatus::Idle,
                Some(
                    runtime
                        .inner
                        .config
                        .artifact_dir
                        .join(format!("{id}-{job_id}.md")),
                ),
                Some(
                    runtime
                        .inner
                        .config
                        .artifact_dir
                        .join(format!("{id}-{job_id}.history.json")),
                ),
            )
            .expect("finish agent");
        runtime.inner.jobs.finish(job_id, result, timestamp);
    }

    fn attach_request_parent(runtime: &OrchestrationRuntime, child_id: &str, parent_id: &str) {
        let definition = super::super::definitions::parse_agent_definition(
            Path::new("task.md"),
            "---\nname: task\ndescription: task\n---\nprompt",
            super::super::AgentDefinitionSource::Bundled,
            true,
        )
        .expect("definition");
        let entry = runtime
            .inner
            .registry
            .get(runtime.group_id(), child_id)
            .expect("child");
        *entry.durable_info.lock() = Some(DurableAgentInfo {
            definition: super::super::persistence::persist_definition(&definition),
            request: super::super::persistence::PersistedRequest {
                child_id: child_id.to_owned(),
                parent_id: parent_id.to_owned(),
                depth: 1,
                assignment: String::new(),
                system_prompt: String::new(),
                requested_tool_names: None,
                thinking_level: None,
                max_tools_per_agent: 16,
                model_provider: None,
                model_id: None,
                output_schema: None,
                schema_mode: None,
            },
            session_path: None,
        });
    }

    fn assert_persisted_graph_valid(runtime: &OrchestrationRuntime) {
        let agents = runtime.collect_persisted_agents();
        let ids = agents
            .iter()
            .map(|agent| agent.snapshot.id.as_str())
            .collect::<std::collections::HashSet<_>>();
        let main = runtime.main_agent_id();
        for agent in &agents {
            if let Some(parent) = agent.snapshot.parent_id.as_deref() {
                assert!(
                    parent == main || ids.contains(parent),
                    "snapshot parent {parent:?} missing for {}",
                    agent.snapshot.id
                );
            }
            let request_parent = agent.request.parent_id.as_str();
            assert!(
                request_parent == main || ids.contains(request_parent),
                "request parent {request_parent:?} missing for {}",
                agent.snapshot.id
            );
        }
    }

    fn registry_has(runtime: &OrchestrationRuntime, id: &str) -> bool {
        runtime.inner.registry.get(runtime.group_id(), id).is_some()
    }

    #[tokio::test]
    async fn pruning_old_job_preserves_newer_same_agent_alias() {
        let root = tempfile::tempdir().expect("root");
        let now = Arc::new(AtomicU64::new(3));
        let runtime = retention_runtime(root.path(), now, 1, Duration::from_secs(60));
        insert_retained_job(&runtime, "Shared", "job-old", 1);
        let old_artifact = root.path().join("Shared-job-old.md");
        let new_artifact = root.path().join("Shared-job-new.md");
        let new_history = root.path().join("Shared-job-new.history.json");
        fs::write(&new_artifact, "new").expect("new artifact");
        fs::write(&new_history, "[]").expect("new history");
        runtime
            .inner
            .registry
            .finish(
                runtime.group_id(),
                "Shared",
                AgentStatus::Idle,
                Some(new_artifact.clone()),
                Some(new_history),
            )
            .expect("publish newer alias");
        runtime
            .inner
            .jobs
            .insert(
                JobSnapshot {
                    id: "job-new".to_owned(),
                    agent_id: "Shared".to_owned(),
                    agent: "task".to_owned(),
                    parent_id: "Main".to_owned(),
                    description: None,
                    todo_task_id: None,
                    workflow_id: None,
                    workflow_generation: None,
                    status: JobStatus::Queued,
                    created_at: 2,
                    started_at: None,
                    finished_at: None,
                    result: None,
                    soft_budget_exhausted: false,
                },
                CancellationToken::new(),
            )
            .expect("insert newer job");
        runtime.inner.jobs.finish(
            "job-new",
            TaskResult {
                index: 0,
                id: "Shared".to_owned(),
                agent: "task".to_owned(),
                status: AgentStatus::Idle,
                output: "new".to_owned(),
                error: None,
                artifact_ref: "agent://Shared".to_owned(),
                history_ref: "history://Shared".to_owned(),
                artifact_uri: "artifact://Shared".to_owned(),
                usage: pi_ai::Usage::default(),
                soft_budget_exhausted: false,
                structured_output: None,
            },
            2,
        );
        let jobs = runtime.jobs(None);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, "job-new");
        assert!(!old_artifact.exists());
        assert_eq!(
            runtime
                .resolve_read_uri("artifact://Shared")
                .expect("new alias"),
            new_artifact.canonicalize().expect("canonical new artifact"),
        );
        assert_eq!(fs::read_to_string(new_artifact).expect("new body"), "new");
    }

    #[tokio::test]
    async fn retained_job_cap_prunes_oldest_and_stales_alias() {
        let root = tempfile::tempdir().expect("root");
        let now = Arc::new(AtomicU64::new(3));
        let runtime = retention_runtime(root.path(), now, 1, Duration::from_secs(60));
        insert_retained_job(&runtime, "Old", "job-old", 1);
        insert_retained_job(&runtime, "New", "job-new", 2);
        let jobs = runtime.jobs(None);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].agent_id, "New");
        assert!(runtime.resolve_read_uri("artifact://Old").is_err());
        assert!(!root.path().join("Old-job-old.md").exists());
        assert!(runtime.resolve_read_uri("artifact://New").is_ok());
    }

    #[tokio::test]
    async fn retained_job_ttl_prunes_expired_but_never_running_or_queued() {
        let root = tempfile::tempdir().expect("root");
        let now = Arc::new(AtomicU64::new(1));
        let runtime = retention_runtime(root.path(), now.clone(), 10, Duration::from_millis(10));
        insert_retained_job(&runtime, "Expired", "job-expired", 1);
        runtime
            .inner
            .jobs
            .insert(
                JobSnapshot {
                    id: "job-queued".to_owned(),
                    agent_id: "Queued".to_owned(),
                    agent: "task".to_owned(),
                    parent_id: "Main".to_owned(),
                    description: None,
                    todo_task_id: None,
                    workflow_id: None,
                    workflow_generation: None,
                    status: JobStatus::Queued,
                    created_at: 1,
                    started_at: None,
                    finished_at: None,
                    result: None,
                    soft_budget_exhausted: false,
                },
                CancellationToken::new(),
            )
            .expect("queued");
        runtime
            .inner
            .jobs
            .insert(
                JobSnapshot {
                    id: "job-running".to_owned(),
                    agent_id: "Running".to_owned(),
                    agent: "task".to_owned(),
                    parent_id: "Main".to_owned(),
                    description: None,
                    todo_task_id: None,
                    workflow_id: None,
                    workflow_generation: None,
                    status: JobStatus::Queued,
                    created_at: 1,
                    started_at: None,
                    finished_at: None,
                    result: None,
                    soft_budget_exhausted: false,
                },
                CancellationToken::new(),
            )
            .expect("running");
        runtime.inner.jobs.mark_running("job-running", 1);
        now.store(12, Ordering::SeqCst);
        let jobs = runtime.jobs(None);
        assert_eq!(jobs.len(), 2);
        assert!(jobs.iter().any(|job| job.status == JobStatus::Queued));
        assert!(jobs.iter().any(|job| job.status == JobStatus::Running));
        assert!(runtime.resolve_read_uri("artifact://Expired").is_err());
    }

    #[tokio::test]
    async fn prune_keeps_parent_named_by_jobless_parked_children() {
        let root = tempfile::tempdir().expect("root");
        let now = Arc::new(AtomicU64::new(1));
        let runtime = retention_runtime(root.path(), now.clone(), 10, Duration::from_millis(10));
        insert_retained_job(&runtime, "Parent", "job-parent", 1);
        register_peer(
            &runtime,
            "audit-vm",
            "task",
            Some("Parent"),
            AgentStatus::Parked,
        );
        register_peer(
            &runtime,
            "audit-web",
            "task",
            Some("Parent"),
            AgentStatus::Parked,
        );
        now.store(12, Ordering::SeqCst);
        let jobs = runtime.jobs(None);
        assert!(!jobs.iter().any(|job| job.id == "job-parent"));
        assert!(registry_has(&runtime, "Parent"));
        assert!(registry_has(&runtime, "audit-vm"));
        assert!(registry_has(&runtime, "audit-web"));
        assert_persisted_graph_valid(&runtime);
    }

    #[tokio::test]
    async fn prune_keeps_parent_named_only_by_request_parent_id() {
        let root = tempfile::tempdir().expect("root");
        let now = Arc::new(AtomicU64::new(1));
        let runtime = retention_runtime(root.path(), now.clone(), 10, Duration::from_millis(10));
        insert_retained_job(&runtime, "Parent", "job-parent", 1);
        register_peer(
            &runtime,
            "audit-prover",
            "task",
            Some("Main"),
            AgentStatus::Parked,
        );
        attach_request_parent(&runtime, "audit-prover", "Parent");
        now.store(12, Ordering::SeqCst);
        let jobs = runtime.jobs(None);
        assert!(!jobs.iter().any(|job| job.id == "job-parent"));
        assert!(registry_has(&runtime, "Parent"));
        assert!(registry_has(&runtime, "audit-prover"));
        assert_persisted_graph_valid(&runtime);
    }

    #[tokio::test]
    async fn prune_keeps_mid_parent_while_grandchild_remains() {
        let root = tempfile::tempdir().expect("root");
        let now = Arc::new(AtomicU64::new(1));
        let runtime = retention_runtime(root.path(), now.clone(), 10, Duration::from_millis(10));
        insert_retained_job(&runtime, "Mid", "job-mid", 1);
        register_peer(&runtime, "Child", "task", Some("Mid"), AgentStatus::Parked);
        register_peer(
            &runtime,
            "Grandchild",
            "task",
            Some("Child"),
            AgentStatus::Parked,
        );
        now.store(12, Ordering::SeqCst);
        let jobs = runtime.jobs(None);
        assert!(!jobs.iter().any(|job| job.id == "job-mid"));
        assert!(registry_has(&runtime, "Mid"));
        assert!(registry_has(&runtime, "Child"));
        assert!(registry_has(&runtime, "Grandchild"));
        assert_persisted_graph_valid(&runtime);
    }

    #[tokio::test]
    async fn prune_one_settled_job_keeps_multi_job_agent() {
        let root = tempfile::tempdir().expect("root");
        let now = Arc::new(AtomicU64::new(20));
        let runtime = retention_runtime(root.path(), now, 10, Duration::from_millis(10));
        insert_retained_job(&runtime, "Shared", "job-old", 1);
        runtime
            .inner
            .jobs
            .insert(
                JobSnapshot {
                    id: "job-new".to_owned(),
                    agent_id: "Shared".to_owned(),
                    agent: "task".to_owned(),
                    parent_id: "Main".to_owned(),
                    description: None,
                    todo_task_id: None,
                    workflow_id: None,
                    workflow_generation: None,
                    status: JobStatus::Queued,
                    created_at: 15,
                    started_at: None,
                    finished_at: None,
                    result: None,
                    soft_budget_exhausted: false,
                },
                CancellationToken::new(),
            )
            .expect("insert newer job");
        let new_artifact = root.path().join("Shared-job-new.md");
        let new_history = root.path().join("Shared-job-new.history.json");
        fs::write(&new_artifact, "new").expect("new artifact");
        fs::write(&new_history, "[]").expect("new history");
        runtime
            .finish_agent(
                "Shared",
                AgentStatus::Idle,
                Some(new_artifact),
                Some(new_history),
            )
            .expect("finish newer alias");
        runtime.inner.jobs.finish(
            "job-new",
            retained_result("Shared", &runtime.inner.config.artifact_dir, "job-new"),
            15,
        );
        let jobs = runtime.jobs(None);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, "job-new");
        assert!(registry_has(&runtime, "Shared"));
    }

    #[tokio::test]
    async fn prune_drops_expired_leaf_without_children() {
        let root = tempfile::tempdir().expect("root");
        let now = Arc::new(AtomicU64::new(1));
        let runtime = retention_runtime(root.path(), now.clone(), 10, Duration::from_millis(10));
        insert_retained_job(&runtime, "Leaf", "job-leaf", 1);
        now.store(12, Ordering::SeqCst);
        let jobs = runtime.jobs(None);
        assert!(jobs.is_empty());
        assert!(!registry_has(&runtime, "Leaf"));
        assert_persisted_graph_valid(&runtime);
    }

    #[tokio::test]
    async fn cleanup_keeps_parent_named_by_parked_children() {
        let root = tempfile::tempdir().expect("root");
        let runtime = retention_runtime(
            root.path(),
            Arc::new(AtomicU64::new(1)),
            10,
            Duration::from_secs(60),
        );
        insert_retained_job(&runtime, "Parent", "job-parent", 1);
        register_peer(
            &runtime,
            "audit-deployer",
            "task",
            Some("Parent"),
            AgentStatus::Parked,
        );
        runtime.cleanup_retained_jobs();
        assert!(runtime.jobs(None).is_empty());
        assert!(registry_has(&runtime, "Parent"));
        assert!(registry_has(&runtime, "audit-deployer"));
        assert_persisted_graph_valid(&runtime);
    }

    #[tokio::test]
    async fn job_cancellation_publishes_terminal_job_update() {
        let root = tempfile::tempdir().expect("root");
        let runtime = prompt_runtime(root.path());
        runtime
            .inner
            .jobs
            .insert(
                JobSnapshot {
                    id: "job-running".to_owned(),
                    agent_id: "Worker".to_owned(),
                    agent: "task".to_owned(),
                    parent_id: "Main".to_owned(),
                    description: None,
                    todo_task_id: None,
                    workflow_id: None,
                    workflow_generation: None,
                    status: JobStatus::Running,
                    created_at: 1,
                    started_at: Some(1),
                    finished_at: None,
                    result: None,
                    soft_budget_exhausted: false,
                },
                CancellationToken::new(),
            )
            .expect("insert running job");
        let mut events = runtime.subscribe();

        let cancelled = runtime
            .cancel_jobs_result(&["job-running".to_owned()])
            .expect("cancel job");

        assert_eq!(cancelled, ["job-running"]);
        let published = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Ok(OrchestrationEvent::JobUpdated { job, .. }) = events.recv().await
                    && job.id == "job-running"
                {
                    break job;
                }
            }
        })
        .await
        .expect("terminal job update");
        assert_eq!(
            published
                .result
                .as_ref()
                .and_then(|result| result.error.as_deref()),
            Some("task cancelled"),
        );
        assert_eq!(runtime.jobs(Some(&cancelled))[0], published);
    }

    #[tokio::test]
    async fn durable_job_cancellation_publishes_terminal_job_update() {
        let root = tempfile::tempdir().expect("root");
        let parent_dir = tempfile::tempdir().expect("parent sessions");
        let recorder = crate::session_store::start_session_in(
            root.path(),
            Some(&pi_ai::Model::default()),
            None,
            Some(parent_dir.path()),
            None,
            None,
        )
        .expect("recorder");
        let parent_fixture = isolated_session(root.path(), Vec::new(), None).await;
        let parent = &parent_fixture.session;
        parent.record(recorder).expect("record");
        let runtime = prompt_runtime(root.path());
        runtime.bind_and_recover(parent).expect("bind durable");
        register_peer(
            &runtime,
            "Worker",
            "Worker",
            Some("Main"),
            AgentStatus::Running,
        );
        runtime
            .inner
            .jobs
            .insert(
                JobSnapshot {
                    id: "job-running".to_owned(),
                    agent_id: "Worker".to_owned(),
                    agent: "Worker".to_owned(),
                    parent_id: "Main".to_owned(),
                    description: None,
                    todo_task_id: None,
                    workflow_id: None,
                    workflow_generation: None,
                    status: JobStatus::Running,
                    created_at: 1,
                    started_at: Some(1),
                    finished_at: None,
                    result: None,
                    soft_budget_exhausted: false,
                },
                CancellationToken::new(),
            )
            .expect("insert running job");
        runtime.persist_state().expect("persist running job");
        let mut events = runtime.subscribe();

        let cancelled = runtime
            .cancel_jobs_result(&["job-running".to_owned()])
            .expect("cancel durable job");
        assert_eq!(cancelled, ["job-running"]);
        let published = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Ok(OrchestrationEvent::JobUpdated { job, .. }) = events.recv().await
                    && job.id == "job-running"
                {
                    break job;
                }
            }
        })
        .await
        .expect("terminal job update");
        assert_eq!(
            published
                .result
                .as_ref()
                .and_then(|result| result.error.as_deref()),
            Some("task cancelled"),
        );
        let durable = runtime
            .inner
            .durable
            .lock()
            .as_ref()
            .expect("durable binding")
            .load()
            .expect("load durable state");
        let persisted = durable
            .jobs
            .iter()
            .find(|job| job.id == "job-running")
            .expect("persisted cancelled job");
        assert_eq!(persisted.status, JobStatus::Cancelled);
    }

    #[tokio::test]
    async fn restart_checkpoint_rejects_job_cancellation_without_rewriting_sidecar() {
        let root = tempfile::tempdir().expect("root");
        let parent_dir = tempfile::tempdir().expect("parent sessions");
        let recorder = crate::session_store::start_session_in(
            root.path(),
            Some(&pi_ai::Model::default()),
            None,
            Some(parent_dir.path()),
            None,
            None,
        )
        .expect("recorder");
        let parent_fixture = isolated_session(root.path(), Vec::new(), None).await;
        let parent = &parent_fixture.session;
        parent.record(recorder).expect("record");
        let runtime = prompt_runtime(root.path());
        runtime.bind_and_recover(parent).expect("bind durable");
        register_peer(
            &runtime,
            "Running",
            "Running",
            Some("Main"),
            AgentStatus::Running,
        );
        runtime
            .inner
            .jobs
            .insert(
                JobSnapshot {
                    id: "job-running".to_owned(),
                    agent_id: "Running".to_owned(),
                    agent: "Running".to_owned(),
                    parent_id: "Main".to_owned(),
                    description: None,
                    todo_task_id: None,
                    workflow_id: None,
                    workflow_generation: None,
                    status: JobStatus::Running,
                    created_at: 1,
                    started_at: Some(1),
                    finished_at: None,
                    result: None,
                    soft_budget_exhausted: false,
                },
                CancellationToken::new(),
            )
            .expect("insert running job");
        runtime.persist_state().expect("persist running job");
        runtime
            .checkpoint_for_restart()
            .expect("checkpoint restart");
        let durable = runtime
            .inner
            .durable
            .lock()
            .as_ref()
            .expect("durable binding")
            .clone();
        let before = durable.load().expect("load checkpointed sidecar");

        let error = runtime
            .cancel_jobs_result(&["job-running".to_owned()])
            .expect_err("restart-frozen cancellation must fail");
        assert!(
            error.to_string().contains("restart-checkpointed"),
            "{error:#}"
        );
        let after = durable.load().expect("reload checkpointed sidecar");
        assert_eq!(
            after, before,
            "failed cancellation must not mutate the sidecar"
        );
        let job = after
            .jobs
            .iter()
            .find(|job| job.id == "job-running")
            .expect("running job remains durable");
        assert_eq!(job.status, JobStatus::Running);
        assert!(job.finished_at.is_none());
        assert!(job.result.is_none());
    }

    #[tokio::test]
    async fn restart_checkpoint_rejects_terminal_shutdown_without_rewriting_sidecar() {
        let root = tempfile::tempdir().expect("root");
        let parent_dir = tempfile::tempdir().expect("parent sessions");
        let recorder = crate::session_store::start_session_in(
            root.path(),
            Some(&pi_ai::Model::default()),
            None,
            Some(parent_dir.path()),
            None,
            None,
        )
        .expect("recorder");
        let parent_fixture = isolated_session(root.path(), Vec::new(), None).await;
        let parent = &parent_fixture.session;
        parent.record(recorder).expect("record");
        let runtime = prompt_runtime(root.path());
        runtime.bind_and_recover(parent).expect("bind durable");
        register_peer(
            &runtime,
            "Running",
            "Running",
            Some("Main"),
            AgentStatus::Running,
        );
        runtime
            .inner
            .jobs
            .insert(
                JobSnapshot {
                    id: "job-running".to_owned(),
                    agent_id: "Running".to_owned(),
                    agent: "Running".to_owned(),
                    parent_id: "Main".to_owned(),
                    description: None,
                    todo_task_id: None,
                    workflow_id: None,
                    workflow_generation: None,
                    status: JobStatus::Running,
                    created_at: 1,
                    started_at: Some(1),
                    finished_at: None,
                    result: None,
                    soft_budget_exhausted: false,
                },
                CancellationToken::new(),
            )
            .expect("insert running job");
        runtime.persist_state().expect("persist running job");
        runtime
            .checkpoint_for_restart()
            .expect("checkpoint restart");
        let durable = runtime
            .inner
            .durable
            .lock()
            .as_ref()
            .expect("durable binding")
            .clone();
        let before = durable.load().expect("load checkpointed sidecar");

        let error = runtime
            .shutdown()
            .await
            .expect_err("restart-frozen terminal shutdown must fail");
        assert!(
            error.to_string().contains("restart-checkpointed"),
            "{error:#}"
        );
        assert_eq!(durable.load().expect("reload checkpointed sidecar"), before);
    }

    #[tokio::test]
    async fn owner_shutdown_removes_retained_physical_files_and_aliases() {
        let root = tempfile::tempdir().expect("root");
        let now = Arc::new(AtomicU64::new(1));
        let runtime = retention_runtime(root.path(), now, 10, Duration::from_secs(60));
        insert_retained_job(&runtime, "Owned", "job-owned", 1);
        let resolver = runtime.read_uri_resolver();
        let artifact = resolver.resolve_file("artifact://Owned").expect("artifact");
        runtime.shutdown().await;
        assert!(!artifact.display_path().exists());
        assert!(resolver.resolve_file("artifact://Owned").is_err());
    }

    fn runtime_with_skill(path: PathBuf) -> OrchestrationRuntime {
        try_runtime_with_skill_source(path, crate::SkillSource::Explicit).expect("runtime")
    }

    fn try_runtime_with_skill_source(
        path: PathBuf,
        source: crate::SkillSource,
    ) -> anyhow::Result<OrchestrationRuntime> {
        try_runtime_with_skill_source_at(path.clone(), source, path.parent().expect("skill parent"))
    }

    fn try_runtime_with_skill_source_at(
        path: PathBuf,
        source: crate::SkillSource,
        artifact_dir: &Path,
    ) -> anyhow::Result<OrchestrationRuntime> {
        let definition = super::super::definitions::parse_agent_definition(
            Path::new("bounded.md"),
            "---\nname: bounded\ndescription: bounded\nautoloadSkills: [large]\n---\nprompt",
            super::super::AgentDefinitionSource::Bundled,
            true,
        )
        .expect("definition");
        let mut config =
            OrchestrationConfig::new(AgentCatalog::from_agents(vec![definition]), artifact_dir);
        config.default_agent = "bounded".to_owned();
        // Managed User/PackageGlobal skills require a real project-boundary
        // authorization. Supply a sibling directory outside the skill files
        // only when the source allows managed links; strict sources stay
        // None so the fail-closed no-follow path is exercised unchanged.
        let managed_project_boundary = if source.allows_managed_links() {
            let boundary = artifact_dir.join("boundary");
            fs::create_dir_all(&boundary).expect("boundary directory");
            Some(boundary)
        } else {
            None
        };
        config.skills.push(OrchestrationSkill {
            name: "large".to_owned(),
            description: "large".to_owned(),
            file_path: path.clone(),
            base_dir: path.parent().expect("skill parent").to_path_buf(),
            globs: Vec::new(),
            always_apply: false,
            hidden: false,
            disable_model_invocation: false,
            source,
            managed_project_boundary,
            managed_root: source
                .allows_managed_links()
                .then(|| path.parent().expect("skill parent").to_path_buf()),
            trusted: true,
        });
        OrchestrationRuntime::new(config, Arc::new(|_| Box::pin(async { unreachable!() })))
    }
    fn try_runtime_with_discovered_skills(
        skills_root: &Path,
        source: crate::SkillSource,
        artifact_dir: &Path,
    ) -> anyhow::Result<OrchestrationRuntime> {
        let capabilities = crate::fs::CapabilityFs::new()?;
        let project_boundary = artifact_dir.join("project-boundary");
        fs::create_dir_all(&project_boundary)?;
        let (skills, diagnostics) =
            crate::resources::load_skills_from_dir_with_capabilities_and_boundary(
                &capabilities,
                &skills_root.to_string_lossy(),
                source,
                true,
                true,
                Some(&project_boundary),
            );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let skill_name = skills.first().expect("discovered skill").name.clone();
        let definition = super::super::definitions::parse_agent_definition(
            Path::new("bounded.md"),
            &format!(
                "---\nname: bounded\ndescription: bounded\nautoloadSkills: [{skill_name}]\n---\nprompt"
            ),
            super::super::AgentDefinitionSource::Bundled,
            true,
        )
        .expect("definition");
        let mut config =
            OrchestrationConfig::new(AgentCatalog::from_agents(vec![definition]), artifact_dir);
        config.default_agent = "bounded".to_owned();
        config.skills = skills.iter().map(OrchestrationSkill::from).collect();
        OrchestrationRuntime::new(config, Arc::new(|_| Box::pin(async { unreachable!() })))
    }

    #[test]
    fn autoload_skill_accepts_exact_file_limit() {
        let root = tempfile::tempdir().expect("root");
        let path = root.path().join("SKILL.md");
        fs::write(&path, vec![b'x'; MAX_AUTOLOAD_SKILL_BYTES as usize]).expect("skill");
        let runtime = runtime_with_skill(path);
        let definition = runtime.catalog().get("bounded").expect("agent");
        let roster = runtime.sibling_roster("Pending");
        runtime
            .child_system_prompt(definition, "work", None, None, None, &roster)
            .expect("boundary accepted");
    }

    #[test]
    fn autoload_skill_rejects_one_byte_over_file_limit() {
        let root = tempfile::tempdir().expect("root");
        let path = root.path().join("SKILL.md");
        fs::write(&path, vec![b'x'; MAX_AUTOLOAD_SKILL_BYTES as usize + 1]).expect("skill");
        let runtime = runtime_with_skill(path);
        let definition = runtime.catalog().get("bounded").expect("agent");
        let roster = runtime.sibling_roster("Pending");
        let error = runtime
            .child_system_prompt(definition, "work", None, None, None, &roster)
            .expect_err("oversized rejected");
        assert!(
            error.chain().any(|source| {
                source
                    .downcast_ref::<crate::fs::FsError>()
                    .is_some_and(|error| {
                        matches!(
                            error,
                            crate::fs::FsError::ReadLimitExceeded { limit }
                                if *limit == MAX_AUTOLOAD_SKILL_BYTES
                        )
                    })
            }),
            "{error:#}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn managed_global_skill_with_symlinked_directory_loads() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("root");
        let skills = root.path().join("skills");
        fs::create_dir_all(&skills).expect("skills dir");
        // The visible `core` directory is a link to another directory retained
        // inside the managed user skill root.
        let real = skills.join(".managed-core");
        fs::create_dir_all(&real).expect("real core dir");
        fs::write(
            real.join("SKILL.md"),
            "---\nname: core\ndescription: managed core skill\n---\nbody\n",
        )
        .expect("skill body");
        symlink(&real, skills.join("core")).expect("core dir symlink");

        // User-sourced (global) skill must load through the symlinked dir
        // instead of failing with FilesystemLoop. The orchestration artifact
        // dir is a real (non-symlinked) path, as in production.
        let artifacts = root.path().join("artifacts");
        fs::create_dir_all(&artifacts).expect("artifacts dir");
        let runtime =
            try_runtime_with_discovered_skills(&skills, crate::SkillSource::User, &artifacts)
                .expect("managed global skill must load through symlinked directory");
        assert_eq!(runtime.skills().len(), 1);
        let definition = runtime.catalog().get("bounded").expect("agent");
        let prompt = runtime
            .child_system_prompt(
                definition,
                "work",
                None,
                None,
                None,
                &runtime.sibling_roster("Pending"),
            )
            .expect("autoload managed directory skill");
        assert!(
            prompt.contains("managed core skill") && prompt.contains("body"),
            "{prompt}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn managed_global_skill_with_symlinked_skill_md_loads() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("root");
        let skills = root.path().join("skills");
        fs::create_dir_all(&skills).expect("skills dir");
        fs::create_dir(skills.join("lib")).expect("lib dir");
        let real = skills.join(".managed-lib");
        fs::create_dir_all(&real).expect("real lib dir");
        fs::write(
            real.join("SKILL.md"),
            "---\nname: lib\ndescription: managed lib skill\n---\nbody\n",
        )
        .expect("skill body");
        // The SKILL.md entry itself is a link within the managed root.
        symlink(real.join("SKILL.md"), skills.join("lib").join("SKILL.md"))
            .expect("skill md symlink");

        let runtime =
            try_runtime_with_discovered_skills(&skills, crate::SkillSource::User, root.path())
                .expect("managed global skill must load through symlinked SKILL.md");
        assert_eq!(runtime.skills().len(), 1);
        let definition = runtime.catalog().get("bounded").expect("agent");
        let prompt = runtime
            .child_system_prompt(
                definition,
                "work",
                None,
                None,
                None,
                &runtime.sibling_roster("Pending"),
            )
            .expect("autoload managed SKILL.md link");
        assert!(
            prompt.contains("managed lib skill") && prompt.contains("body"),
            "{prompt}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn strict_skill_with_external_directory_symlink_fails_closed() {
        use std::os::unix::fs::symlink;

        for source in [
            crate::SkillSource::Project,
            crate::SkillSource::PackageProject,
            crate::SkillSource::Explicit,
        ] {
            let root = tempfile::tempdir().expect("root");
            let project = root.path().join("project");
            let skills = project.join(".pi").join("skills");
            fs::create_dir_all(&skills).expect("skills dir");
            let outside = root.path().join("outside");
            fs::create_dir_all(&outside).expect("outside dir");
            fs::write(
                outside.join("SKILL.md"),
                "---\nname: evil\ndescription: malicious\n---\nbody\n",
            )
            .expect("outside skill");
            // Malicious project: a skill subdirectory symlinks outside the project.
            symlink(&outside, skills.join("evil")).expect("evil dir symlink");
            // Artifact dir stays inside the real project root, never through the
            // malicious symlink.
            let artifacts = project.join("artifacts");
            fs::create_dir_all(&artifacts).expect("artifacts dir");

            let error = match try_runtime_with_skill_source_at(
                skills.join("evil").join("SKILL.md"),
                source,
                &artifacts,
            ) {
                Ok(_) => unreachable!(
                    "strict skill with external directory symlink must fail closed for {source:?}"
                ),
                Err(error) => error,
            };
            let message = format!("{error:#}");
            assert!(
                message.contains("symbolic links are not allowed"),
                "missing diagnostic for {source:?}: {message}"
            );
            assert!(
                message.contains("skill `large`"),
                "missing skill name for {source:?}: {message}"
            );
            // The bounded diagnostic must never leak the absolute host path.
            assert!(
                !message.contains(outside.to_str().unwrap()),
                "absolute outside path leaked for {source:?}: {message}"
            );
            assert!(
                !message.contains(skills.join("evil").to_str().unwrap()),
                "absolute symlink path leaked for {source:?}: {message}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn strict_skill_with_external_skill_md_symlink_fails_closed() {
        use std::os::unix::fs::symlink;

        for source in [
            crate::SkillSource::Project,
            crate::SkillSource::PackageProject,
            crate::SkillSource::Explicit,
        ] {
            let root = tempfile::tempdir().expect("root");
            let project = root.path().join("project");
            let skills = project.join(".pi").join("skills");
            fs::create_dir_all(&skills).expect("skills dir");
            fs::create_dir(skills.join("evil")).expect("evil dir");
            let outside = root.path().join("outside.md");
            fs::write(
                &outside,
                "---\nname: evil\ndescription: malicious\n---\nbody\n",
            )
            .expect("outside skill");
            // Malicious project: SKILL.md itself symlinks outside the project.
            symlink(&outside, skills.join("evil").join("SKILL.md")).expect("skill md symlink");

            let error = match try_runtime_with_skill_source(
                skills.join("evil").join("SKILL.md"),
                source,
            ) {
                Ok(_) => unreachable!(
                    "strict skill with external SKILL.md symlink must fail closed for {source:?}"
                ),
                Err(error) => error,
            };
            let message = format!("{error:#}");
            assert!(
                message.contains("symbolic links are not allowed"),
                "missing diagnostic for {source:?}: {message}"
            );
            assert!(
                message.contains("skill `large`"),
                "missing skill name for {source:?}: {message}"
            );
            assert!(
                !message.contains(outside.to_str().unwrap()),
                "absolute outside path leaked for {source:?}: {message}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn managed_discovered_skill_inside_project_never_reaches_child_prompt() {
        use std::os::unix::fs::symlink;

        for trusted in [false, true] {
            for source in [crate::SkillSource::User, crate::SkillSource::PackageGlobal] {
                let root = tempfile::tempdir().expect("root");
                let project = root.path().join("project");
                let skills = root.path().join("global-skills");
                let project_skill = project.join("linked");
                fs::create_dir_all(&project_skill).expect("project skill");
                fs::create_dir_all(&skills).expect("global skills");
                fs::write(
                    project_skill.join("SKILL.md"),
                    "---\nname: linked\ndescription: project body\n---\nsecret\n",
                )
                .expect("skill body");
                symlink(&project_skill, skills.join("linked")).expect("skill link");
                let capabilities = crate::fs::CapabilityFs::new().expect("capabilities");
                let (discovered, diagnostics) =
                    crate::resources::load_skills_from_dir_with_capabilities_and_boundary(
                        &capabilities,
                        &skills.to_string_lossy(),
                        source,
                        trusted,
                        true,
                        Some(&project),
                    );
                assert!(discovered.is_empty(), "{discovered:?}");
                assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
                assert!(diagnostics[0].message.contains("project boundary"));
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn managed_package_global_skill_with_symlinked_directory_loads() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("root");
        let skills = root.path().join("skills");
        fs::create_dir_all(&skills).expect("skills dir");
        let real = skills.join(".managed-core");
        fs::create_dir_all(&real).expect("real core dir");
        fs::write(
            real.join("SKILL.md"),
            "---\nname: core\ndescription: managed core skill\n---\nbody\n",
        )
        .expect("skill body");
        symlink(&real, skills.join("core")).expect("core dir symlink");

        let artifacts = root.path().join("artifacts");
        fs::create_dir_all(&artifacts).expect("artifacts dir");
        let runtime = try_runtime_with_discovered_skills(
            &skills,
            crate::SkillSource::PackageGlobal,
            &artifacts,
        )
        .expect("managed package-global skill must load through symlinked directory");
        assert_eq!(runtime.skills().len(), 1);
        let definition = runtime.catalog().get("bounded").expect("agent");
        let prompt = runtime
            .child_system_prompt(
                definition,
                "work",
                None,
                None,
                None,
                &runtime.sibling_roster("Pending"),
            )
            .expect("autoload managed directory skill");
        assert!(
            prompt.contains("managed core skill") && prompt.contains("body"),
            "{prompt}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn managed_package_global_skill_with_symlinked_skill_md_loads() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("root");
        let skills = root.path().join("skills");
        fs::create_dir_all(&skills).expect("skills dir");
        fs::create_dir(skills.join("lib")).expect("lib dir");
        let real = skills.join(".managed-lib");
        fs::create_dir_all(&real).expect("real lib dir");
        fs::write(
            real.join("SKILL.md"),
            "---\nname: lib\ndescription: managed lib skill\n---\nbody\n",
        )
        .expect("skill body");
        // The SKILL.md entry itself is a link within the managed root.
        symlink(real.join("SKILL.md"), skills.join("lib").join("SKILL.md"))
            .expect("skill md symlink");

        let runtime = try_runtime_with_discovered_skills(
            &skills,
            crate::SkillSource::PackageGlobal,
            root.path(),
        )
        .expect("managed package-global skill must load through symlinked SKILL.md");
        assert_eq!(runtime.skills().len(), 1);
        let definition = runtime.catalog().get("bounded").expect("agent");
        let prompt = runtime
            .child_system_prompt(
                definition,
                "work",
                None,
                None,
                None,
                &runtime.sibling_roster("Pending"),
            )
            .expect("autoload managed SKILL.md link");
        assert!(
            prompt.contains("managed lib skill") && prompt.contains("body"),
            "{prompt}"
        );
    }

    #[test]
    fn agent_snapshot_serializes_agent_without_renaming_existing_fields() {
        let snapshot = AgentSnapshot {
            id: "worker-1".to_owned(),
            display_name: "Worker One".to_owned(),
            agent: "reviewer".to_owned(),
            parent_id: Some("Main".to_owned()),
            status: AgentStatus::Running,
            created_at: 1,
            last_activity: 2,
            unread: 3,
            artifact_ref: Some("agent://worker-1".to_owned()),
            history_ref: Some("history://worker-1".to_owned()),
        };

        let wire = serde_json::to_value(&snapshot).expect("serialize agent snapshot");
        assert_eq!(wire["id"], "worker-1");
        assert_eq!(wire["displayName"], "Worker One");
        assert_eq!(wire["agent"], "reviewer");
        assert_eq!(wire["parentId"], "Main");
        assert_eq!(wire["status"], "running");
        assert_eq!(wire["createdAt"], 1);
        assert_eq!(wire["lastActivity"], 2);
        assert_eq!(wire["unread"], 3);
        assert_eq!(wire["artifactRef"], "agent://worker-1");
        assert_eq!(wire["historyRef"], "history://worker-1");

        let mut legacy = wire;
        legacy
            .as_object_mut()
            .expect("snapshot object")
            .remove("agent");
        let decoded: AgentSnapshot =
            serde_json::from_value(legacy).expect("deserialize legacy agent snapshot");
        assert_eq!(decoded.agent, "");
    }

    #[test]
    fn workflow_scope_is_optional_and_serializes_compatibly() {
        let legacy = JobSnapshot {
            id: "job".to_owned(),
            agent_id: "Worker".to_owned(),
            agent: "task".to_owned(),
            parent_id: "Main".to_owned(),
            description: None,
            todo_task_id: Some("todo-1".to_owned()),
            workflow_id: None,
            workflow_generation: None,
            status: JobStatus::Queued,
            created_at: 1,
            started_at: None,
            finished_at: None,
            result: None,
            soft_budget_exhausted: false,
        };
        let legacy_wire = serde_json::to_value(&legacy).expect("serialize legacy job");
        assert!(legacy_wire.get("workflowId").is_none());
        assert!(legacy_wire.get("workflowGeneration").is_none());
        assert!(legacy_wire.get("softBudgetExhausted").is_none());

        let scoped = JobSnapshot {
            workflow_id: Some("workflow-a".to_owned()),
            workflow_generation: Some(7),
            ..legacy
        };
        let scoped_wire = serde_json::to_value(&scoped).expect("serialize scoped job");
        assert_eq!(scoped_wire["workflowId"], "workflow-a");
        assert_eq!(scoped_wire["workflowGeneration"], 7);
    }

    #[test]
    fn workflow_scope_and_global_gate_are_fail_closed_after_jobs_start() {
        let root = tempfile::tempdir().expect("root");
        let now = Arc::new(AtomicU64::new(1));
        let runtime = retention_runtime(root.path(), now, 8, Duration::from_secs(60));
        runtime
            .set_workflow_scope(WorkflowRuntimeScope {
                workflow_id: "workflow-a".to_owned(),
                generation: 1,
            })
            .expect("initial workflow scope");
        let gate = OrchestrationConcurrencyGate::new(2).expect("gate");
        runtime
            .set_global_concurrency_gate(gate.clone())
            .expect("initial gate");
        runtime
            .set_global_concurrency_gate(gate)
            .expect("same shared gate is idempotent");

        runtime
            .inner
            .jobs
            .insert(
                JobSnapshot {
                    id: "job".to_owned(),
                    agent_id: "Worker".to_owned(),
                    agent: "task".to_owned(),
                    parent_id: "Main".to_owned(),
                    description: None,
                    todo_task_id: Some("todo-1".to_owned()),
                    workflow_id: Some("workflow-a".to_owned()),
                    workflow_generation: Some(1),
                    status: JobStatus::Queued,
                    created_at: 1,
                    started_at: None,
                    finished_at: None,
                    result: None,
                    soft_budget_exhausted: false,
                },
                CancellationToken::new(),
            )
            .expect("active job");
        let scope_error = runtime
            .set_workflow_scope(WorkflowRuntimeScope {
                workflow_id: "workflow-b".to_owned(),
                generation: 2,
            })
            .expect_err("active job blocks scope replacement");
        assert!(scope_error.to_string().contains("jobs are active"));
        let gate_error = runtime
            .set_global_concurrency_gate(
                OrchestrationConcurrencyGate::new(1).expect("replacement gate"),
            )
            .expect_err("active job blocks gate replacement");
        assert!(gate_error.to_string().contains("jobs are active"));
    }

    fn budget_runtime(
        root: &Path,
        budget: JobSoftBudget,
    ) -> (OrchestrationRuntime, AgentDefinition) {
        let definition = super::super::definitions::parse_agent_definition(
            Path::new("task.md"),
            "---\nname: task\ndescription: task\n---\nprompt",
            super::super::AgentDefinitionSource::Bundled,
            true,
        )
        .expect("definition");
        let definition_for_hook = definition.clone();
        let mut config =
            OrchestrationConfig::new(AgentCatalog::from_agents(vec![definition]), root);
        config.soft_budget = budget;
        let runtime =
            OrchestrationRuntime::new(config, Arc::new(|_| Box::pin(async { unreachable!() })))
                .expect("runtime");
        (runtime, definition_for_hook)
    }

    fn budget_turn(tokens: i64) -> pi_agent::ShouldStopAfterTurnContext {
        let mut message = pi_ai::AssistantMessage::pending(&pi_ai::Model::default());
        message.usage.total_tokens = tokens;
        pi_agent::ShouldStopAfterTurnContext {
            message,
            tool_results: Vec::new(),
            context: pi_agent::AgentContext {
                system_prompt: String::new(),
                messages: Vec::new(),
                tools: Vec::new(),
            },
            new_messages: Vec::new(),
        }
    }

    #[test]
    fn soft_budget_config_builds_stop_hook_that_triggers_on_limits() {
        let root = tempfile::tempdir().expect("root");
        let (runtime, definition) = budget_runtime(
            root.path(),
            JobSoftBudget {
                max_requests: Some(2),
                max_tokens: None,
                yield_after: None,
            },
        );
        let state = JobSoftBudgetState::default();
        let hook = runtime
            .soft_budget_stop_hook(&definition, &state)
            .expect("configured budget must build a stop hook");
        assert!(!hook(&budget_turn(0)), "first request stays under the cap");
        assert!(hook(&budget_turn(0)), "second request reaches the cap");
        assert!(
            state.triggered.load(Ordering::Relaxed),
            "budget marks the job"
        );

        // Token cap: 100 tokens per turn against a 150-token budget.
        let (token_runtime, token_definition) = budget_runtime(
            root.path(),
            JobSoftBudget {
                max_requests: None,
                max_tokens: Some(150),
                yield_after: None,
            },
        );
        let token_state = JobSoftBudgetState::default();
        let token_hook = token_runtime
            .soft_budget_stop_hook(&token_definition, &token_state)
            .expect("token budget must build a stop hook");
        assert!(
            !token_hook(&budget_turn(100)),
            "100 tokens stay under the cap"
        );
        assert!(
            token_hook(&budget_turn(100)),
            "200 cumulative tokens exceed the cap"
        );
        assert!(token_state.triggered.load(Ordering::Relaxed));
    }

    #[test]
    fn unlimited_default_soft_budget_builds_no_stop_hook() {
        let root = tempfile::tempdir().expect("root");
        // OrchestrationConfig::new defaults soft_budget to unlimited, which is
        // what a Settings without orchestration.softBudget resolves to.
        let (runtime, definition) = budget_runtime(root.path(), JobSoftBudget::default());
        assert!(
            runtime
                .soft_budget_stop_hook(&definition, &JobSoftBudgetState::default())
                .is_none(),
            "unlimited budget must preserve run-to-completion behavior"
        );
    }

    #[tokio::test]
    async fn wait_jobs_resolves_only_when_every_requested_child_is_terminal() {
        // P0 (parent Task Done while a child is still Running): a parent wait
        // spanning several children must never resolve while any requested
        // job is still non-terminal — a partially settled batch is not a
        // settled batch.
        let root = tempfile::tempdir().expect("root");
        let runtime = prompt_runtime(root.path());
        for (id, agent_id) in [("job-alpha", "Alpha"), ("job-beta", "Beta")] {
            runtime
                .inner
                .jobs
                .insert(
                    JobSnapshot {
                        id: id.to_owned(),
                        agent_id: agent_id.to_owned(),
                        agent: "task".to_owned(),
                        parent_id: "Main".to_owned(),
                        description: None,
                        todo_task_id: None,
                        workflow_id: None,
                        workflow_generation: None,
                        status: JobStatus::Running,
                        created_at: 1,
                        started_at: Some(1),
                        finished_at: None,
                        result: None,
                        soft_budget_exhausted: false,
                    },
                    CancellationToken::new(),
                )
                .expect("insert running job");
        }
        let completed = TaskResult {
            index: 0,
            id: "Alpha".to_owned(),
            agent: "task".to_owned(),
            status: AgentStatus::Idle,
            output: "alpha done".to_owned(),
            usage: pi_ai::Usage::default(),
            soft_budget_exhausted: false,
            error: None,
            structured_output: None,
            artifact_ref: "agent://Alpha".to_owned(),
            history_ref: "history://Alpha".to_owned(),
            artifact_uri: "artifact://Alpha".to_owned(),
        };
        runtime
            .inner
            .jobs
            .finish("job-alpha", completed, 2)
            .expect("alpha settles");

        let ids = vec!["job-alpha".to_owned(), "job-beta".to_owned()];
        let mut wait = tokio::spawn({
            let runtime = runtime.clone();
            async move { runtime.wait_jobs(&ids, None, None).await }
        });
        let early = tokio::time::timeout(Duration::from_millis(150), &mut wait).await;
        assert!(
            early.is_err(),
            "wait_jobs must not resolve while any requested child is non-terminal: {early:?}"
        );

        let beta = TaskResult {
            index: 0,
            id: "Beta".to_owned(),
            agent: "task".to_owned(),
            status: AgentStatus::Idle,
            output: "beta done".to_owned(),
            usage: pi_ai::Usage::default(),
            soft_budget_exhausted: false,
            error: None,
            structured_output: None,
            artifact_ref: "agent://Beta".to_owned(),
            history_ref: "history://Beta".to_owned(),
            artifact_uri: "artifact://Beta".to_owned(),
        };
        runtime
            .inner
            .jobs
            .finish("job-beta", beta, 3)
            .expect("beta settles");
        let settled = tokio::time::timeout(Duration::from_secs(3), wait)
            .await
            .expect("wait resolves once every requested child is terminal")
            .expect("wait join")
            .expect("wait result");
        assert_eq!(settled.len(), 2);
        assert!(
            settled.iter().all(|job| job.status.is_settled()),
            "{settled:#?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn explicit_shutdown_waiting_child_terminates_exactly_once_and_never_revives() {
        // P0 (Waiting child + explicit runtime-wide shutdown): a child blocked
        // in a hub wait is authoritative Waiting/Running work. Explicit
        // shutdown must transition it to terminal EXACTLY ONCE with the
        // `orchestration shut down` reason, publish exactly one terminal job
        // update, never hang, and never revive it after reopening the same
        // parent.
        let root = tempfile::tempdir().expect("root");
        let parent_dir = tempfile::tempdir().expect("parent sessions");
        let recorder = crate::session_store::start_session_in(
            root.path(),
            Some(&pi_ai::Model::default()),
            None,
            Some(parent_dir.path()),
            None,
            None,
        )
        .expect("recorder");
        recorder.persist_now().expect("materialize parent header");
        let parent_fixture = isolated_session(root.path(), Vec::new(), None).await;
        let parent = parent_fixture.session.clone();
        parent.record(recorder).expect("record parent");
        let (parent_id, parent_path) = parent.recorder_info().expect("parent recorder info");
        let sidecar = parent_path
            .parent()
            .expect("parent directory")
            .join("children")
            .join(&parent_id)
            .join("orchestration-state.json");

        let definition = super::super::definitions::parse_agent_definition(
            Path::new("task.md"),
            "---\nname: task\ndescription: task\ntools: [read]\n---\nprompt",
            super::super::AgentDefinitionSource::Bundled,
            true,
        )
        .expect("definition");
        let child_cwd = root.path().to_path_buf();
        let runtime = OrchestrationRuntime::new(
            OrchestrationConfig::new(
                AgentCatalog::from_agents(vec![definition.clone()]),
                root.path(),
            ),
            Arc::new(move |request: ChildSessionRequest| {
                let child_cwd = child_cwd.clone();
                Box::pin(async move {
                    let stream_fn: pi_agent::StreamFn = Arc::new(
                        |_model: pi_ai::Model,
                         _context: pi_ai::Context,
                         _options: pi_ai::SimpleStreamOptions| {
                            Box::pin(async move {
                                let stream = pi_ai::new_assistant_message_event_stream();
                                let producer = stream.clone();
                                tokio::spawn(async move {
                                    let mut message =
                                        pi_ai::AssistantMessage::pending(&pi_ai::Model::default());
                                    message.content.push(pi_ai::ContentBlock::ToolCall(
                                        pi_ai::ToolCall {
                                            id: "call-wait".to_owned(),
                                            name: "hub".to_owned(),
                                            arguments: serde_json::json!({ "op": "wait" }),
                                            thought_signature: None,
                                        },
                                    ));
                                    message.stop_reason = pi_ai::StopReason::ToolUse;
                                    producer.end(Some(message)).await;
                                });
                                stream
                            })
                        },
                    );
                    Session::new_with_additional_tools_filtered_and_discovery(
                        crate::SessionOptions {
                            model: request.model,
                            cwd: child_cwd,
                            system_prompt: request.system_prompt,
                            thinking_level: ThinkingLevel::Off,
                            api_key: String::new(),
                            compaction: None,
                            stream_options: Default::default(),
                            tools: Some(request.orchestration_tools),
                            before_tool_call: None,
                            after_tool_call: None,
                            stream_fn: Some(stream_fn),
                            auth_resolver: None,
                            auth_manager: None,
                        },
                        Vec::new(),
                        Default::default(),
                        crate::ResourceDiscovery::Disabled,
                    )
                })
            }),
        )
        .expect("runtime");
        runtime.bind_and_recover(&parent).expect("bind durable");
        let mut events = runtime.subscribe();

        let spawn = runtime
            .spawn_tasks(
                "Main",
                0,
                vec![TaskItem {
                    index: 0,
                    id: "WaitingChild".to_owned(),
                    agent: "task".to_owned(),
                    assignment: "wait for a hub message that never arrives".to_owned(),
                    ..Default::default()
                }],
            )
            .expect("spawn waiting child")
            .remove(0);
        // Event-driven gate: the job is Running and the child's own hub wait
        // has registered its waiter (authoritative Waiting state).
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let running = runtime
                    .jobs(Some(std::slice::from_ref(&spawn.job_id)))
                    .first()
                    .is_some_and(|job| job.status == JobStatus::Running);
                let waiting = runtime
                    .inner
                    .registry
                    .get(runtime.group_id(), "WaitingChild")
                    .is_some_and(|entry| !entry.waiters.lock().is_empty());
                if running && waiting {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("child entered hub wait while its job is Running");

        tokio::time::timeout(Duration::from_secs(10), runtime.shutdown())
            .await
            .expect("explicit shutdown must not hang on a Waiting child")
            .expect("terminal shutdown");
        assert!(
            runtime.is_shutdown(),
            "explicit shutdown must cancel waiters"
        );
        assert_eq!(runtime.active_child_count(), 0);

        let terminal = std::iter::from_fn(|| events.try_recv().ok())
            .filter_map(|event| match event {
                OrchestrationEvent::JobUpdated { job, .. } if job.id == spawn.job_id => Some(job),
                _ => None,
            })
            .filter(|job| job.status.is_settled())
            .collect::<Vec<_>>();
        assert_eq!(
            terminal.len(),
            1,
            "exactly one terminal job update may be published: {terminal:#?}"
        );
        assert_eq!(terminal[0].status, JobStatus::Cancelled);
        assert_eq!(
            terminal[0]
                .result
                .as_ref()
                .and_then(|result| result.error.as_deref()),
            Some("orchestration shut down")
        );

        let durable = runtime.durable_state_for_test().expect("durable state");
        let persisted = durable
            .jobs
            .iter()
            .find(|job| job.id == spawn.job_id)
            .expect("terminal job persisted");
        assert_eq!(persisted.status, JobStatus::Cancelled);
        assert_eq!(
            persisted
                .result
                .as_ref()
                .and_then(|result| result.error.as_deref()),
            Some("orchestration shut down")
        );

        // Reopen the SAME parent: the sidecar job is terminal, so the resumed
        // runtime must revive nothing.
        let reopened = crate::resume_session(&parent_path).expect("reopen parent recording");
        let resumed_fixture = isolated_session(root.path(), Vec::new(), None).await;
        resumed_fixture
            .session
            .set_session_dir(parent.session_dir());
        resumed_fixture
            .session
            .record(reopened)
            .expect("attach reopened recorder");
        let resumed_definition = super::super::definitions::parse_agent_definition(
            Path::new("task.md"),
            "---\nname: task\ndescription: task\ntools: [read]\n---\nprompt",
            super::super::AgentDefinitionSource::Bundled,
            true,
        )
        .expect("definition");
        let resumed_runtime = OrchestrationRuntime::new(
            OrchestrationConfig::new(
                AgentCatalog::from_agents(vec![resumed_definition]),
                root.path(),
            ),
            Arc::new(|_request| {
                Box::pin(async { Err(anyhow!("revival must never run after explicit shutdown")) })
            }),
        )
        .expect("resumed runtime");
        resumed_runtime
            .bind_and_recover(&resumed_fixture.session)
            .expect("recover reopened parent");
        let recovered = resumed_runtime.jobs(Some(std::slice::from_ref(&spawn.job_id)));
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].status, JobStatus::Cancelled);
        assert_eq!(recovered[0].id, spawn.job_id);
        assert_eq!(resumed_runtime.active_child_count(), 0);
    }
}

#[cfg(test)]
mod delegation_intent_tests {
    use super::*;

    #[test]
    fn agent_name_span_respects_embedded_ascii_word_boundaries() {
        // CJK-embedded and standalone mentions are found…
        assert_eq!(
            agent_name_span("你让researcher仔细调研", "researcher"),
            Some(("你让".len(), "你让researcher".len()))
        );
        assert_eq!(
            agent_name_span("Have researcher study this", "researcher"),
            Some(("Have ".len(), "Have researcher".len()))
        );
        // …but `researcher` never matches inside `researchers`, and casing is
        // matched case-insensitively.
        assert!(agent_name_span("researchers study this", "researcher").is_none());
        assert!(agent_name_span("你让Researcher去调查", "researcher").is_some());
    }

    #[test]
    fn delegation_candidates_are_precise() {
        // English: the noun directly after a delegation verb.
        assert_eq!(
            delegation_candidates("Have researcher study this"),
            vec!["researcher".to_owned()]
        );
        // The verb itself and clause objects are never candidates.
        assert!(delegation_candidates("please review the security patch").is_empty());
        assert!(delegation_candidates("ask me to review").is_empty());
        // CJK: only names abutted by a delegation token with an action clause.
        assert_eq!(
            delegation_candidates("你让researcher仔细调研pi-coding-agent"),
            vec!["researcher".to_owned()]
        );
        assert!(delegation_candidates("请使用research技能").is_empty());
        assert!(delegation_candidates("researcher 是做什么的？").is_empty());
    }

    #[test]
    fn delegation_candidates_chain_cjk_conjunction_joined_names() {
        // Every name in a CJK conjunction chain is a candidate, so validation
        // reports ALL named agents, never just the first.
        assert_eq!(
            delegation_candidates("你让missing1和missing2一起调研"),
            vec!["missing1".to_owned(), "missing2".to_owned()]
        );
        assert_eq!(
            delegation_candidates("你让missing1、missing2、missing3一起调研"),
            vec![
                "missing1".to_owned(),
                "missing2".to_owned(),
                "missing3".to_owned()
            ]
        );
        assert_eq!(
            delegation_candidates("你让missing1，missing2一起调研"),
            vec!["missing1".to_owned(), "missing2".to_owned()]
        );
        // An action clause between names breaks the chain.
        assert_eq!(
            delegation_candidates("你让missing1仔细调研missing2"),
            vec!["missing1".to_owned()]
        );
        // A bare space is not a conjunction (conservative, mirroring the
        // single-space CJK token rule): nothing chains.
        assert_eq!(
            delegation_candidates("你让missing1 missing2一起调研"),
            vec!["missing1".to_owned()]
        );
    }

    #[test]
    fn delegation_candidates_chain_english_list_joined_names() {
        // Every name in an English list after a delegation verb is a
        // candidate, so validation reports ALL named agents.
        assert_eq!(
            delegation_candidates("Have missing1 and missing2 study this"),
            vec!["missing1".to_owned(), "missing2".to_owned()]
        );
        assert_eq!(
            delegation_candidates("Have missing1, missing2 and missing3 study this"),
            vec![
                "missing1".to_owned(),
                "missing2".to_owned(),
                "missing3".to_owned()
            ]
        );
        // `or` chains like `and`.
        assert_eq!(
            delegation_candidates("Have missing1 or missing2 study this"),
            vec!["missing1".to_owned(), "missing2".to_owned()]
        );
        // The chain stops at the action clause: a plain verb after the name
        // is not a delegated agent.
        assert_eq!(
            delegation_candidates("Have writer check this"),
            vec!["writer".to_owned()]
        );
        // Function words end the list ("the team" is not an agent list).
        assert_eq!(
            delegation_candidates("Have missing1 and the team study this"),
            vec!["missing1".to_owned()]
        );
        // The first-noun clause-object rejection still applies.
        assert!(delegation_candidates("please review the security patch").is_empty());
        assert!(delegation_candidates("ask me to review").is_empty());
    }

    #[test]
    fn delegation_candidates_gate_ordinary_instruction_verbs_on_known_identities() {
        // Unambiguous delegation verbs keep their object regardless of the
        // catalog: delegating to a missing agent must still be caught.
        assert_eq!(
            delegation_candidates("Have ghost-agent study this"),
            vec!["ghost-agent".to_owned()]
        );
        assert_eq!(
            delegation_candidates("delegate to ghost-agent, ghost-two and ghost-three"),
            vec![
                "ghost-agent".to_owned(),
                "ghost-three".to_owned(),
                "ghost-two".to_owned()
            ]
        );
        // Ordinary instruction verbs produce nothing without a known
        // identity, so natural language never fabricates agents.
        assert!(delegation_candidates("run python3 --version").is_empty());
        assert!(delegation_candidates("make sure the build passes").is_empty());
        assert!(delegation_candidates("please review the security patch").is_empty());
        assert!(delegation_candidates("let me know when it lands").is_empty());
        assert!(delegation_candidates("get the latest results").is_empty());

        let known = BTreeSet::from([
            "researcher".to_owned(),
            "research".to_owned(),
            "writer".to_owned(),
        ]);
        assert_eq!(
            delegation_candidates_with_identities("run researcher study this", &[], Some(&known)),
            vec!["researcher".to_owned()]
        );
        // `make sure` remains ordinary even when a later word happens to be a
        // configured identity: weak verbs only inspect their direct object or
        // a syntactically joined object list.
        assert!(
            delegation_candidates_with_identities(
                "make sure research skills are loaded",
                &[],
                Some(&known),
            )
            .is_empty()
        );
        // A configured identity anywhere in a weak-verb list anchors the
        // entire list, including every missing target before and after it.
        assert_eq!(
            delegation_candidates_with_identities(
                "run ghost-one, writer and ghost-two now",
                &[],
                Some(&known),
            ),
            vec![
                "ghost-one".to_owned(),
                "ghost-two".to_owned(),
                "writer".to_owned()
            ]
        );
        assert!(
            delegation_candidates_with_identities("run ghost-agent now", &[], Some(&known)).is_empty()
        );
    }

    #[test]
    fn configured_identity_matching_is_exact_and_normalized() {
        let known = BTreeSet::from(["review-bot".to_owned(), "research".to_owned()]);
        assert_eq!(
            delegation_candidates_with_identities("RUN REVIEW-BOT now", &[], Some(&known)),
            vec!["review-bot".to_owned()]
        );
        assert_eq!(
            delegation_candidates_with_identities("run research now", &[], Some(&known)),
            vec!["research".to_owned()]
        );
        assert!(
            delegation_candidates_with_identities("run review now", &[], Some(&known)).is_empty()
        );
    }

    #[test]
    fn delegation_candidates_never_fire_on_w3_report_phrases() {
        assert!(delegation_candidates("run python3 --version").is_empty());
        assert!(delegation_candidates("make sure the workflow passes").is_empty());
        assert!(delegation_candidates("make sure integration is clean").is_empty());
        assert!(delegation_candidates("ensure the build is green").is_empty());
        assert!(delegation_candidates("check that tests pass").is_empty());
    }

    fn routing_definition(name: &str) -> AgentDefinition {
        AgentDefinition {
            name: name.to_owned(),
            description: format!("{name} routing definition"),
            system_prompt: "p".to_owned(),
            tools: None,
            autoload_skills: Vec::new(),
            model: None,
            thinking_level: None,
            max_turns: None,
            max_tool_calls: None,
            timeout_secs: None,
            disallowed_tools: Vec::new(),
            capability_ceiling: None,
            source: crate::AgentDefinitionSource::Bundled,
            path: None,
            trusted: true,
            kind: crate::orchestration::AgentDefinitionKind::Agent,
            personality: None,
            soft_budget: None,
            knowledge_roots: Vec::new(),
            user_proxy: false,
        }
    }

    /// Typed task assignment routes on the RAW exact-name mention: a
    /// descriptive reference inside typed task content still selects the
    /// named agent, while the interactive user-prompt surfaces keep the
    /// gated delegation grammar and stay Main.
    #[test]
    fn resolve_task_agent_routes_typed_content_on_raw_exact_mention() {
        let dir = tempfile::tempdir().expect("artifacts");
        let runtime = OrchestrationRuntime::new(
            OrchestrationConfig::new(
                AgentCatalog::from_agents(vec![
                    routing_definition("task"),
                    routing_definition("coordinator"),
                ]),
                dir.path(),
            ),
            Arc::new(|_| Box::pin(async { unreachable!() })),
        )
        .expect("runtime");
        assert_eq!(
            runtime
                .resolve_task_agent("summarize the coordinator findings for the report", None)
                .expect("raw exact mention routes"),
            "coordinator"
        );
        assert_eq!(
            runtime
                .resolve_task_agent("coordinator please fix the build", None)
                .expect("imperative mention routes"),
            "coordinator"
        );
        // Interactive prompts keep the gated grammar: the descriptive
        // reference is metadata only there.
        let trusted = runtime.inner.config.catalog.agents().clone();
        assert!(matches!(
            crate::selector::delegation_agent_mention(
                "summarize the coordinator findings for the report",
                &trusted,
            ),
            crate::selector::ExactAgentMention::None
        ));
        assert!(matches!(
            crate::selector::delegation_agent_mention(
                "coordinator please fix the build",
                &trusted,
            ),
            crate::selector::ExactAgentMention::Unique(name) if name == "coordinator"
        ));
    }

    #[test]
    fn resolve_task_agent_reports_ambiguity_for_colliding_exact_mentions() {
        let dir = tempfile::tempdir().expect("artifacts");
        let runtime = OrchestrationRuntime::new(
            OrchestrationConfig::new(
                AgentCatalog::from_agents(vec![
                    routing_definition("task"),
                    routing_definition("Research-Agent"),
                    routing_definition("research-agent"),
                ]),
                dir.path(),
            ),
            Arc::new(|_| Box::pin(async { unreachable!() })),
        )
        .expect("runtime");
        let error = runtime
            .resolve_task_agent("study with research-agent", None)
            .expect_err("normalized name collisions must stay ambiguous");
        assert!(
            error.to_string().contains("ambiguous"),
            "{error}"
        );
    }

    /// Contract: the workflow missing-agent diagnostics parser and the
    /// selector's delegation grammar agree exactly on catalogued names —
    /// every prompt the selector treats as delegation surfaces as a
    /// workflow candidate, every prompt the selector keeps Main yields
    /// none. Direct addresses are taken from the selector's own
    /// [`crate::selector::delegation_agent_mention`] (the bare whole-name
    /// request and the name-leading comma/colon vocative included), and the
    /// remaining marker/imperative/CJK forms share the selector's clause
    /// constants (`DELEGATION_MARKER_NEGATIONS`, `ENGLISH_CLAUSE_AUXILIARIES`,
    /// `ENGLISH_CLAUSE_PRONOUNS`, `ENGLISH_DECLARATIVE_SUBJECTS`,
    /// `DELEGATION_NOUN_PRONE_MARKERS`) so no second, weaker parser can
    /// regress independently.
    #[test]
    fn workflow_diagnostics_candidates_match_selector_delegation_grammar() {
        let known = BTreeSet::from(["coordinator".to_owned()]);
        let trusted = vec![routing_definition("coordinator")];
        let delegations = [
            "Have coordinator audit the parser",
            "please coordinator fix the build",
            "coordinator",
            "coordinator: route next",
            "coordinator, route next",
            "coordinator please fix the build",
            "run coordinator on the failing test",
            "get coordinator to review the patch",
            "launch coordinator to fix the build",
            "start coordinator before the batch",
            "delegate to coordinator for the triage",
            "use coordinator (after warmup)",
            "你让coordinator处理这个",
            "否则让coordinator接手",
        ];
        for request in delegations {
            let diagnostics = delegation_candidates_with_identities(request, &trusted, Some(&known));
            assert!(
                diagnostics.contains(&"coordinator".to_owned()),
                "workflow diagnostics must treat {request:?} as delegation: {diagnostics:?}"
            );
            assert!(
                matches!(
                    crate::selector::delegation_agent_mention(request, &trusted),
                    crate::selector::ExactAgentMention::Unique(name) if name == "coordinator"
                ),
                "selector must delegate: {request:?}"
            );
        }
        let references = [
            "I did use coordinator for this",
            "She does use coordinator for this",
            "We have used coordinator for this",
            "review the run coordinator produced",
            "check the get coordinator output",
            "did you have coordinator run it",
            "don't have coordinator run it",
            "can you use coordinator for this",
            "I do not want you to use coordinator",
            "是否让coordinator处理这个",
            "能不能让coordinator处理这个",
            "不要让coordinator处理这个",
        ];
        for request in references {
            let diagnostics = delegation_candidates_with_identities(request, &trusted, Some(&known));
            assert!(
                !diagnostics.contains(&"coordinator".to_owned()),
                "workflow diagnostics must not treat {request:?} as delegation: {diagnostics:?}"
            );
            assert!(
                matches!(
                    crate::selector::delegation_agent_mention(request, &trusted),
                    crate::selector::ExactAgentMention::None
                ),
                "selector must keep Main: {request:?}"
            );
        }
    }

    /// The workflow missing-agent diagnostics share the selector grammar:
    /// a politely delegated disabled agent fails actionably, and so do the
    /// selector's direct-address forms (bare whole-name, name-leading
    /// comma/colon vocative), while the same name referenced by a
    /// descriptive subject form stays silent.
    #[test]
    fn validate_delegation_agents_matches_selector_grammar() {
        let dir = tempfile::tempdir().expect("artifacts");
        let mut config = OrchestrationConfig::new(
            AgentCatalog::from_agents(vec![
                routing_definition("task"),
                routing_definition("coordinator"),
            ]),
            dir.path(),
        );
        config.agent_settings.insert(
            "coordinator".to_owned(),
            crate::AgentRuntimeSettings {
                enabled: Some(false),
                model: None,
                tools: None,
            },
        );
        let runtime =
            OrchestrationRuntime::new(config, Arc::new(|_| Box::pin(async { unreachable!() })))
                .expect("runtime");
        let error = runtime
            .validate_delegation_agents("please coordinator fix the build")
            .expect_err("disabled polite delegate must fail");
        assert!(error.to_string().contains("disabled"), "{error}");
        for request in ["coordinator", "coordinator: route next", "coordinator, route next"] {
            let error = runtime
                .validate_delegation_agents(request)
                .expect_err("disabled direct-address delegate must fail");
            assert!(error.to_string().contains("disabled"), "{error}");
        }
        runtime
            .validate_delegation_agents("the coordinator: route next")
            .expect("non-leading vocative reference must not report a disabled agent");
        runtime
            .validate_delegation_agents("I did use coordinator for this")
            .expect("descriptive reference must not report a disabled agent");
    }
}

#[cfg(test)]
mod yield_tool_tests {
    use super::*;
    use crate::orchestration::tools::yield_tool;
    use pi_ai::{
        AssistantMessage, AssistantMessageEvent, ContentBlock, Model, SimpleStreamOptions,
        StopReason, ToolCall, new_assistant_message_event_stream,
    };
    use serde_json::json;
    use std::fs;

    /// Scripted provider: pops one assistant message per stream invocation so
    /// the turn count is deterministic; an exhausted script falls back to a
    /// plain "done" message, so any turn streamed after the scripted ones is
    /// observable in the transcript.
    fn scripted(messages: Vec<AssistantMessage>) -> pi_agent::StreamFn {
        let messages = Arc::new(parking_lot::Mutex::new(VecDeque::from(messages)));
        Arc::new(
            move |model: Model, _context: pi_ai::Context, _options: SimpleStreamOptions| {
                let messages = messages.clone();
                Box::pin(async move {
                    let message = messages.lock().pop_front().unwrap_or_else(|| {
                        let mut fallback = AssistantMessage::pending(&model);
                        fallback.content = vec![ContentBlock::text("done")];
                        fallback.stop_reason = StopReason::Stop;
                        fallback
                    });
                    let stream = new_assistant_message_event_stream();
                    let producer = stream.clone();
                    let model = model.clone();
                    tokio::spawn(async move {
                        producer
                            .push(AssistantMessageEvent::Start {
                                partial: AssistantMessage::pending(&model),
                            })
                            .await;
                        let terminal = if matches!(
                            message.stop_reason,
                            StopReason::Error | StopReason::Aborted
                        ) {
                            AssistantMessageEvent::Error {
                                reason: message.stop_reason,
                                error: message.clone(),
                            }
                        } else {
                            AssistantMessageEvent::Done {
                                reason: message.stop_reason,
                                message: message.clone(),
                            }
                        };
                        producer.push(terminal).await;
                        producer.end(Some(message)).await;
                    });
                    stream
                })
            },
        )
    }

    fn task_definition() -> AgentDefinition {
        super::super::definitions::parse_agent_definition(
            Path::new("task.md"),
            "---\nname: task\ndescription: task\ntools: []\n---\nprompt",
            super::super::AgentDefinitionSource::Bundled,
            true,
        )
        .expect("definition")
    }

    /// Like [`task_definition`] but the agent explicitly declares `tools:
    /// [read, yield]` — the qwen-style case where `yield` appears in the
    /// definition's tool list. The validator must accept it and the child
    /// factory must skip it during base-tool creation (so `create_tool` is
    /// never asked to build `yield`) and append exactly one `yield` tool.
    fn yield_declaring_definition() -> AgentDefinition {
        super::super::definitions::parse_agent_definition(
            Path::new("qwen.md"),
            "---\nname: qwen\ndescription: qwen\ntools: [read, yield]\n---\nprompt",
            super::super::AgentDefinitionSource::Bundled,
            true,
        )
        .expect("qwen definition")
    }

    /// Runtime whose catalog carries the yield-declaring definition, so spawns
    /// exercise the declared-`yield` path through the real child factory.
    async fn yield_declaring_runtime(
        root: &Path,
        messages: Vec<AssistantMessage>,
    ) -> (IsolatedSessionFixture, OrchestrationRuntime) {
        let fixture = isolated_session(root, Vec::new(), Some(scripted(messages))).await;
        let snapshot = fixture.session.child_session_options_snapshot();
        let catalog =
            AgentCatalog::from_agents(vec![task_definition(), yield_declaring_definition()]);
        let config = OrchestrationConfig::new(catalog, root.join("artifacts"))
            .with_parent_model(fixture.session.model().expect("parent model"));
        let runtime = OrchestrationRuntime::new(
            config,
            OrchestrationRuntime::child_factory_from_snapshot(snapshot),
        )
        .expect("runtime");
        (fixture, runtime)
    }

    /// Runtime whose child factory builds real sessions from a parent session
    /// carrying the scripted provider — the exact production path, so the
    /// child's tool set includes the orchestration plumbing plus the appended
    /// `yield` tool.
    async fn yield_runtime(
        root: &Path,
        messages: Vec<AssistantMessage>,
        soft_budget: JobSoftBudget,
    ) -> (IsolatedSessionFixture, OrchestrationRuntime) {
        let fixture = isolated_session(root, Vec::new(), Some(scripted(messages))).await;
        let snapshot = fixture.session.child_session_options_snapshot();
        let mut config = OrchestrationConfig::new(
            AgentCatalog::from_agents(vec![task_definition()]),
            root.join("artifacts"),
        )
        .with_parent_model(fixture.session.model().expect("parent model"));
        config.soft_budget = soft_budget;
        let runtime = OrchestrationRuntime::new(
            config,
            OrchestrationRuntime::child_factory_from_snapshot(snapshot),
        )
        .expect("runtime");
        (fixture, runtime)
    }

    async fn spawn_and_settle(
        runtime: &OrchestrationRuntime,
        id: &str,
        assignment: &str,
    ) -> JobSnapshot {
        let spawn = runtime
            .spawn_tasks(
                "Main",
                0,
                vec![TaskItem {
                    index: 0,
                    id: id.to_owned(),
                    agent: "task".to_owned(),
                    assignment: assignment.to_owned(),
                    todo_task_id: None,
                    ..Default::default()
                }],
            )
            .expect("spawn")
            .remove(0);
        let settled = runtime
            .wait_jobs(&[spawn.job_id.clone()], Some(Duration::from_secs(10)), None)
            .await
            .expect("jobs settle");
        settled
            .into_iter()
            .find(|job| job.id == spawn.job_id)
            .expect("settled job")
    }

    /// Assistant message that calls the `yield` tool with `text`, preceded by
    /// a concise yield-marker text block (the trailing text the payload must
    /// replace in the delivered output).
    fn yield_call_message(text: &str) -> AssistantMessage {
        let mut message = AssistantMessage::pending(&Model::default());
        message.content = vec![
            ContentBlock::text("Delivering my final result."),
            ContentBlock::ToolCall(ToolCall {
                id: "call-yield".to_owned(),
                name: "yield".to_owned(),
                arguments: serde_json::json!({ "text": text }),
                thought_signature: None,
            }),
        ];
        message.stop_reason = StopReason::ToolUse;
        message
    }

    fn text_message(text: &str) -> AssistantMessage {
        let mut message = AssistantMessage::pending(&Model::default());
        message.content = vec![ContentBlock::text(text)];
        message.stop_reason = StopReason::Stop;
        message
    }

    fn read_history(root: &Path, id: &str, job_id: &str) -> Vec<pi_ai::Message> {
        let path = root
            .join("artifacts")
            .join(format!("{id}-{job_id}.history.json"));
        let bytes = fs::read(&path).expect("history file");
        serde_json::from_slice(&bytes).expect("parse history")
    }

    #[tokio::test]
    async fn yield_call_settles_job_with_delivered_payload_and_stops_after_the_turn() {
        let root = tempfile::tempdir().expect("root");
        let (_fixture, runtime) = yield_runtime(
            root.path(),
            vec![yield_call_message("deliverable")],
            JobSoftBudget::default(),
        )
        .await;
        let job = spawn_and_settle(&runtime, "YieldChild", "produce a deliverable").await;

        assert_eq!(
            job.status,
            JobStatus::Completed,
            "a yield call settles the job as Completed"
        );
        let result = job.result.expect("settled result");
        assert_eq!(
            result.output, "deliverable",
            "the yield payload becomes the job's final output"
        );
        assert!(
            !result.output.contains("Delivering my final result."),
            "the trailing assistant text is replaced, not kept: {:?}",
            result.output
        );
        assert!(result.error.is_none());
        assert!(!result.soft_budget_exhausted);

        // The run must end right after the yielding turn: the transcript holds
        // exactly one assistant message (the yield-marker turn) and never a
        // follow-up turn after the delivery.
        let history = read_history(root.path(), "YieldChild", &job.id);
        let assistant_count = history
            .iter()
            .filter(|message| matches!(message, pi_ai::Message::Assistant(_)))
            .count();
        assert_eq!(
            assistant_count, 1,
            "the run stops after the yield turn: {history:?}"
        );
    }

    #[tokio::test]
    async fn child_that_never_calls_yield_keeps_natural_text_with_missing_yield_warning() {
        let root = tempfile::tempdir().expect("root");
        let (_fixture, runtime) = yield_runtime(
            root.path(),
            vec![text_message("natural final text")],
            JobSoftBudget::default(),
        )
        .await;
        let job = spawn_and_settle(&runtime, "NoYieldChild", "work without yield").await;

        assert_eq!(job.status, JobStatus::Completed);
        let result = job.result.expect("settled result");
        assert_eq!(
            result.output,
            format!("natural final text\n\n{MISSING_YIELD_WARNING}"),
            "natural completion keeps the final text and appends the warning"
        );
        assert!(result.error.is_none());
        assert!(!result.soft_budget_exhausted);
    }

    #[tokio::test]
    async fn soft_budget_and_yield_compose_without_panic() {
        let root = tempfile::tempdir().expect("root");
        let (_fixture, runtime) = yield_runtime(
            root.path(),
            vec![yield_call_message("budgeted deliverable")],
            JobSoftBudget {
                max_requests: Some(1),
                ..JobSoftBudget::default()
            },
        )
        .await;
        let job = spawn_and_settle(&runtime, "BudgetedYield", "budgeted work").await;

        assert_eq!(
            job.status,
            JobStatus::Completed,
            "a soft budget must not fail a job that yields"
        );
        let result = job.result.expect("settled result");
        assert_eq!(
            result.output, "budgeted deliverable",
            "the yield payload wins over the budget's partial-text projection"
        );
        assert!(
            result.soft_budget_exhausted,
            "the soft-budget marker must survive the yield delivery"
        );
    }

    #[test]
    fn yield_is_not_registered_in_main_session_tool_sets() {
        let cwd = tempfile::tempdir().expect("cwd");
        let cwd = cwd.path().to_str().expect("utf-8 cwd");
        let error = crate::create_tool("yield", cwd)
            .expect_err("main-session create_tool must reject yield");
        assert!(
            error.to_string().contains("Unknown tool"),
            "rejection must be the standard unknown-tool error: {error}"
        );
        assert!(
            !crate::TOOL_NAMES.contains(&"yield"),
            "yield must not be a built-in"
        );
        assert!(
            !crate::create_all_tools(cwd)
                .iter()
                .any(|tool| tool.name == "yield"),
            "create_all_tools must not expose yield"
        );
        assert!(
            !crate::create_coding_tools(cwd)
                .iter()
                .any(|tool| tool.name == "yield"),
            "the default coding set must not expose yield"
        );
    }

    #[tokio::test]
    async fn yield_payload_is_raw_in_storage_and_redacted_at_presentation() {
        let root = tempfile::tempdir().expect("root");
        let secret = ["gh", "p_", "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij"].concat();
        let payload = format!("deploy using {secret} and token=abc123");
        let (_fixture, runtime) = yield_runtime(
            root.path(),
            vec![yield_call_message(&payload)],
            JobSoftBudget::default(),
        )
        .await;
        let job = spawn_and_settle(&runtime, "RedactedYield", "work with secrets").await;
        let result = job.result.as_ref().expect("settled result");

        // Storage keeps the raw payload (same contract as every tool text:
        // raw in storage, redacted at display/transport boundaries).
        let artifact_path = root
            .path()
            .join("artifacts")
            .join(format!("RedactedYield-{}.md", job.id));
        let artifact = fs::read_to_string(&artifact_path).expect("artifact file");
        assert!(
            artifact.contains(secret.as_str()),
            "storage must keep the raw payload: {artifact}"
        );
        let history = fs::read_to_string(
            root.path()
                .join("artifacts")
                .join(format!("RedactedYield-{}.history.json", job.id)),
        )
        .expect("history file");
        assert!(
            history.contains(secret.as_str()),
            "the raw transcript keeps the payload"
        );

        // Presentation redacts the payload at the same boundary as any other
        // tool text: the presented job output and the presented tool-call
        // arguments never carry the secret.
        let presented = presentation_job_snapshot(job);
        let presented_output = presented.result.expect("presented result").output;
        assert!(
            !presented_output.contains(secret.as_str()),
            "presentation must redact the payload: {presented_output}"
        );
        assert!(presented_output.contains("[REDACTED]"), "redaction marker");
        let redacted_arguments = crate::redact_value(&serde_json::json!({ "text": payload }));
        assert!(
            !redacted_arguments.to_string().contains(secret.as_str()),
            "presented tool-call arguments must be redacted like every other tool's"
        );
    }

    #[tokio::test]
    async fn yield_tool_records_payload_once_and_ignores_repeats() {
        let state = Arc::new(YieldState::default());
        let tool = yield_tool(state.clone());
        assert_eq!(tool.name, "yield");
        assert!(
            tool.description.contains("final deliverable"),
            "the description carries the delivery protocol: {}",
            tool.description
        );
        assert!(tool.description.contains("Do not call it mid-work"));

        let (_, abort) = pi_agent::AbortController::new();
        let context = pi_agent::ToolCallContext {
            tool_call_id: "call-yield".to_owned(),
            arguments: serde_json::json!({ "text": "direct deliverable" }),
            on_update: Arc::new(|_| {}),
            abort,
            model: None,
            approval_token: None,
        };
        let result = (tool.execute)(context).await.expect("yield executes");
        assert_eq!(state.payload().as_deref(), Some("direct deliverable"));
        assert!(state.was_called());
        // The acknowledgment never echoes the payload back into the transcript.
        let result_text = result
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert!(
            !result_text.contains("direct deliverable"),
            "the tool result must not echo the payload: {result_text}"
        );

        // A second call (a model calling yield twice in one message) is
        // ignored: the first payload wins.
        let (_, second_abort) = pi_agent::AbortController::new();
        let second = (tool.execute)(pi_agent::ToolCallContext {
            tool_call_id: "call-yield-2".to_owned(),
            arguments: serde_json::json!({ "text": "second payload" }),
            on_update: Arc::new(|_| {}),
            abort: second_abort,
            model: None,
            approval_token: None,
        })
        .await
        .expect("second yield executes");
        let second_text = second
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert!(second_text.contains("already called"), "{second_text}");
        assert_eq!(
            state.payload().as_deref(),
            Some("direct deliverable"),
            "the first payload wins"
        );
    }

    #[tokio::test]
    async fn definition_declaring_yield_spawns_with_exactly_one_working_yield_tool() {
        // Idempotency + qwen-case regression: an agent definition that declares
        // `yield` among its tools must (a) pass the validator (no "unsupported
        // child tools" rejection), (b) spawn without the child factory asking
        // `create_tool` to build `yield` (which would fail with "Unknown tool
        // name"), and (c) end up with exactly one working `yield` tool — the
        // factory appends it regardless of the declaration. A child that calls
        // `yield` settles with the delivered payload, proving the single
        // appended tool is live and the declared copy was not double-registered.
        let root = tempfile::tempdir().expect("root");
        let (_fixture, runtime) =
            yield_declaring_runtime(root.path(), vec![yield_call_message("declared-yield")]).await;
        let job = spawn_and_settle(&runtime, "QwenChild", "deliver via declared yield").await;

        assert_eq!(
            job.status,
            JobStatus::Completed,
            "the declared-yield child must spawn and complete, not be rejected"
        );
        let result = job.result.expect("settled result");
        assert_eq!(
            result.output, "declared-yield",
            "the single appended yield tool must deliver the payload"
        );
        assert!(
            result.error.is_none(),
            "no spawn/tool errors: {:?}",
            result.error
        );
        // Exactly one assistant turn: the factory did not register two yield
        // tools that could confuse the model into extra turns.
        let history = read_history(root.path(), "QwenChild", &job.id);
        let assistant_count = history
            .iter()
            .filter(|message| matches!(message, pi_ai::Message::Assistant(_)))
            .count();
        assert_eq!(
            assistant_count, 1,
            "one yield turn, no trailing work: {history:?}"
        );
    }

    async fn spawn_and_settle_with_contract(
        runtime: &OrchestrationRuntime,
        id: &str,
        assignment: &str,
        output_schema: Option<Value>,
        schema_mode: Option<&str>,
    ) -> JobSnapshot {
        let spawn = runtime
            .spawn_tasks(
                "Main",
                0,
                vec![TaskItem {
                    index: 0,
                    id: id.to_owned(),
                    agent: "task".to_owned(),
                    assignment: assignment.to_owned(),
                    todo_task_id: None,
                    output_schema,
                    schema_mode: schema_mode.map(str::to_owned),
                    ..TaskItem::default()
                }],
            )
            .expect("spawn")
            .remove(0);
        let settled = runtime
            .wait_jobs(&[spawn.job_id.clone()], Some(Duration::from_secs(10)), None)
            .await
            .expect("jobs settle");
        settled
            .into_iter()
            .find(|job| job.id == spawn.job_id)
            .expect("settled job")
    }

    #[tokio::test]
    async fn output_schema_validates_conforming_delivered_payload_per_child() {
        let root = tempfile::tempdir().expect("root");
        let (_fixture, runtime) = yield_runtime(
            root.path(),
            vec![yield_call_message(r#"{"ok": true}"#)],
            JobSoftBudget::default(),
        )
        .await;
        let schema = json!({
            "type": "object",
            "properties": {"ok": {"type": "boolean"}},
            "required": ["ok"],
        });
        let job = spawn_and_settle_with_contract(
            &runtime,
            "SchemaChild",
            "deliver a JSON report",
            Some(schema),
            Some("strict"),
        )
        .await;

        assert_eq!(job.status, JobStatus::Completed);
        let result = job.result.expect("settled result");
        assert_eq!(result.output, r#"{"ok": true}"#);
        assert!(
            result.error.is_none(),
            "conforming payload must not fail: {:?}",
            result.error
        );
        let validation = result.structured_output.expect("structured output");
        assert!(validation.valid, "conforming payload must validate");
        assert_eq!(validation.schema_source, "task");
        assert_eq!(validation.schema_mode, "strict");
        assert_eq!(validation.data, Some(json!({"ok": true})));
        assert_eq!(validation.error, None);
    }

    #[tokio::test]
    async fn output_schema_reports_non_conforming_payload_without_failing_permissive_job() {
        let root = tempfile::tempdir().expect("root");
        let (_fixture, runtime) = yield_runtime(
            root.path(),
            vec![yield_call_message(r#"{"ok": "nope"}"#)],
            JobSoftBudget::default(),
        )
        .await;
        let schema = json!({
            "type": "object",
            "properties": {"ok": {"type": "boolean"}},
            "required": ["ok"],
        });
        let job = spawn_and_settle_with_contract(
            &runtime,
            "PermissiveChild",
            "deliver a JSON report",
            Some(schema),
            None,
        )
        .await;

        assert_eq!(
            job.status,
            JobStatus::Completed,
            "permissive mode keeps the job completed"
        );
        let result = job.result.expect("settled result");
        assert!(
            result.error.is_none(),
            "permissive mode must not fail the job: {:?}",
            result.error
        );
        let validation = result.structured_output.expect("structured output");
        assert!(!validation.valid, "non-conforming payload must be flagged");
        assert_eq!(validation.schema_mode, "permissive");
        assert_eq!(validation.data, Some(json!({"ok": "nope"})));
        assert!(
            validation
                .error
                .as_deref()
                .is_some_and(|error| error.contains("outputSchema")),
            "validation error: {:?}",
            validation.error
        );
    }

    #[tokio::test]
    async fn strict_output_schema_failure_surfaces_as_job_error() {
        let root = tempfile::tempdir().expect("root");
        let (_fixture, runtime) = yield_runtime(
            root.path(),
            vec![yield_call_message("not json at all")],
            JobSoftBudget::default(),
        )
        .await;
        let schema = json!({"type": "object"});
        let job = spawn_and_settle_with_contract(
            &runtime,
            "StrictChild",
            "deliver a JSON report",
            Some(schema),
            Some("strict"),
        )
        .await;

        assert_eq!(
            job.status,
            JobStatus::Failed,
            "strict mode surfaces the validation failure as a job failure"
        );
        let result = job.result.expect("settled result");
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("not valid JSON")),
            "strict mode surfaces the validation failure as a job error: {:?}",
            result.error
        );
        let validation = result.structured_output.expect("structured output");
        assert!(!validation.valid);
        assert_eq!(validation.schema_mode, "strict");
        assert_eq!(validation.data, None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn batch_context_is_rendered_into_every_child_prompt_with_own_task() {
        let root = tempfile::tempdir().expect("root");
        let captured = Arc::new(Mutex::new(BTreeMap::<String, String>::new()));
        let factory_capture = captured.clone();
        let definition = task_definition();
        let runtime = OrchestrationRuntime::new(
            OrchestrationConfig::new(AgentCatalog::from_agents(vec![definition]), root.path()),
            Arc::new(move |request| {
                factory_capture
                    .lock()
                    .insert(request.child_id, request.system_prompt);
                Box::pin(async { Err(anyhow!("stop after prompt capture")) })
            }),
        )
        .expect("runtime");

        let spawns = runtime
            .spawn_tasks(
                "Main",
                0,
                vec![
                    TaskItem {
                        index: 0,
                        id: "First".to_owned(),
                        agent: "task".to_owned(),
                        assignment: "first task briefing".to_owned(),
                        todo_task_id: None,
                        context: Some("shared background context".to_owned()),
                        output_schema: Some(json!({"type": "object"})),
                        schema_mode: Some("strict".to_owned()),
                    },
                    TaskItem {
                        index: 1,
                        id: "Second".to_owned(),
                        agent: "task".to_owned(),
                        assignment: "second task briefing".to_owned(),
                        todo_task_id: None,
                        context: Some("shared background context".to_owned()),
                        ..TaskItem::default()
                    },
                ],
            )
            .expect("spawn batch");
        let job_ids = spawns
            .iter()
            .map(|spawn| spawn.job_id.clone())
            .collect::<Vec<_>>();
        runtime
            .wait_jobs(&job_ids, Some(Duration::from_secs(1)), None)
            .await
            .expect("settled");

        let prompts = captured.lock();
        let first = prompts.get("First").expect("first prompt");
        let second = prompts.get("Second").expect("second prompt");
        for prompt in [first, second] {
            assert!(
                prompt.contains("<context>\nshared background context\n</context>"),
                "every child prompt carries the shared CONTEXT section: {prompt}"
            );
        }
        assert!(
            first.contains("<delegated_assignment>\nfirst task briefing\n</delegated_assignment>"),
            "first child's own task: {first}"
        );
        assert!(
            second
                .contains("<delegated_assignment>\nsecond task briefing\n</delegated_assignment>"),
            "second child's own task: {second}"
        );
        // The context is not duplicated into the assignment.
        assert!(!first.contains("shared background context\n\nfirst task briefing"));
        // The output contract is rendered only for the item that carries it.
        assert!(
            first.contains("<output_contract>"),
            "first child sees its contract: {first}"
        );
        assert!(first.contains("Validation mode: strict"));
        assert!(
            !second.contains("<output_contract>"),
            "no contract for the second child: {second}"
        );
    }
}

#[cfg(test)]
mod unknown_tool_declaration_tests {
    use super::*;
    use pi_ai::{
        AssistantMessage, AssistantMessageEvent, ContentBlock, Model, SimpleStreamOptions,
        StopReason, new_assistant_message_event_stream,
    };

    /// Stream function that records the exact tool set injected into the
    /// child session (the `Context.tools` the provider sees) and then
    /// completes with a plain "done" message so the run settles cleanly.
    fn recording_stream(captured: Arc<Mutex<Vec<String>>>) -> pi_agent::StreamFn {
        Arc::new(
            move |model: Model, context: pi_ai::Context, _options: SimpleStreamOptions| {
                *captured.lock() = context.tools.iter().map(|tool| tool.name.clone()).collect();
                Box::pin(async move {
                    let mut done = AssistantMessage::pending(&model);
                    done.content = vec![ContentBlock::text("done")];
                    done.stop_reason = StopReason::Stop;
                    let stream = new_assistant_message_event_stream();
                    let producer = stream.clone();
                    let model = model.clone();
                    tokio::spawn(async move {
                        producer
                            .push(AssistantMessageEvent::Start {
                                partial: AssistantMessage::pending(&model),
                            })
                            .await;
                        producer
                            .push(AssistantMessageEvent::Done {
                                reason: StopReason::Stop,
                                message: done.clone(),
                            })
                            .await;
                        producer.end(Some(done)).await;
                    });
                    stream
                })
            },
        )
    }

    fn qa_definition(tools: &[&str]) -> AgentDefinition {
        super::super::definitions::parse_agent_definition(
            Path::new("qa.md"),
            &format!(
                "---\nname: qa\ndescription: qa\ntools: [{}]\n---\nprompt",
                tools.join(", ")
            ),
            super::super::AgentDefinitionSource::Bundled,
            true,
        )
        .expect("qa definition")
    }

    fn task_definition() -> AgentDefinition {
        super::super::definitions::parse_agent_definition(
            Path::new("task.md"),
            "---\nname: task\ndescription: task\ntools: [read]\n---\nprompt",
            super::super::AgentDefinitionSource::Bundled,
            true,
        )
        .expect("task definition")
    }

    /// Runtime whose catalog carries the qa-style definition (declaring the
    /// ghost `yield_output` plus valid tools) and a plain task definition.
    /// The child factory is the real production path; the recording stream fn
    /// captures each child's injected tool set.
    async fn qa_runtime(
        root: &Path,
        qa_tools: &[&str],
        captured: Arc<Mutex<Vec<String>>>,
        max_tools_per_agent: usize,
    ) -> (IsolatedSessionFixture, OrchestrationRuntime) {
        let fixture = isolated_session(root, Vec::new(), Some(recording_stream(captured))).await;
        let snapshot = fixture.session.child_session_options_snapshot();
        let mut config = OrchestrationConfig::new(
            AgentCatalog::from_agents(vec![task_definition(), qa_definition(qa_tools)]),
            root.join("artifacts"),
        )
        .with_parent_model(fixture.session.model().expect("parent model"));
        config.max_tools_per_agent = max_tools_per_agent;
        let runtime = OrchestrationRuntime::new(
            config,
            OrchestrationRuntime::child_factory_from_snapshot(snapshot),
        )
        .expect("runtime");
        (fixture, runtime)
    }

    fn spawn_item(runtime: &OrchestrationRuntime, id: &str, agent: &str) -> TaskSpawn {
        runtime
            .spawn_tasks(
                "Main",
                0,
                vec![TaskItem {
                    index: 0,
                    id: id.to_owned(),
                    agent: agent.to_owned(),
                    assignment: "work".to_owned(),
                    todo_task_id: None,
                    ..TaskItem::default()
                }],
            )
            .expect("spawn")
            .remove(0)
    }

    async fn settle(runtime: &OrchestrationRuntime, spawns: &[TaskSpawn]) {
        let ids = spawns
            .iter()
            .map(|spawn| spawn.job_id.clone())
            .collect::<Vec<_>>();
        runtime
            .wait_jobs(&ids, Some(Duration::from_secs(10)), None)
            .await
            .expect("jobs settle");
    }

    #[tokio::test]
    async fn child_factory_skips_unknown_declared_tools() {
        // A qa-style definition declaring `yield_output` + a ghost next to
        // valid tools: the child receives the valid tools + orchestration
        // plumbing + yield, and NEVER the unknown names (which would otherwise
        // hard-fail `create_tool` with "Unknown tool name").
        let root = tempfile::tempdir().expect("root");
        let captured = Arc::new(Mutex::new(Vec::<String>::new()));
        let (_fixture, runtime) = qa_runtime(
            root.path(),
            &["read", "yield_output", "ghost"],
            captured.clone(),
            16,
        )
        .await;
        let spawn = spawn_item(&runtime, "QaChild", "qa");
        settle(&runtime, &[spawn]).await;

        let injected = captured.lock().clone();
        assert!(
            injected.iter().any(|name| name == "read"),
            "valid declared tools are injected: {injected:?}"
        );
        assert!(
            injected.iter().any(|name| name == "yield"),
            "the child-only yield tool is appended: {injected:?}"
        );
        assert!(
            injected.iter().any(|name| name == "task" || name == "hub"),
            "orchestration plumbing is injected: {injected:?}"
        );
        assert!(
            !injected
                .iter()
                .any(|name| name == "yield_output" || name == "ghost"),
            "unknown declared tools are silently dropped, never injected: {injected:?}"
        );
    }

    #[tokio::test]
    async fn spawn_succeeds_with_deduped_warning() {
        // The qa definition loads, spawns, and completes despite declaring the
        // ghost yield_output; the warning fires exactly once per (agent, tool)
        // even across repeated spawns.
        let root = tempfile::tempdir().expect("root");
        let captured = Arc::new(Mutex::new(Vec::<String>::new()));
        let (_fixture, runtime) = qa_runtime(
            root.path(),
            &["read", "grep", "bash", "write", "yield_output"],
            captured.clone(),
            16,
        )
        .await;
        let first = spawn_item(&runtime, "QaChildA", "qa");
        let second = spawn_item(&runtime, "QaChildB", "qa");
        settle(&runtime, &[first, second]).await;

        assert!(
            captured.lock().iter().all(|name| name != "yield_output"),
            "the ghost tool is never injected: {:?}",
            captured.lock()
        );
        let warnings = runtime.unknown_tool_warnings();
        assert_eq!(warnings.len(), 1, "one deduped warning: {warnings:?}");
        assert!(warnings[0].contains("qa"), "{}", warnings[0]);
        assert!(warnings[0].contains("yield_output"), "{}", warnings[0]);
        assert!(warnings[0].contains("ignoring"), "{}", warnings[0]);
    }

    #[tokio::test]
    async fn batch_with_one_unknown_tool_item_still_spawns_every_item() {
        // A batch where ONE item's agent declares an unknown tool must not
        // abort the batch: every item spawns.
        let root = tempfile::tempdir().expect("root");
        let captured = Arc::new(Mutex::new(Vec::<String>::new()));
        let (_fixture, runtime) =
            qa_runtime(root.path(), &["read", "yield_output"], captured.clone(), 16).await;
        let batch = runtime
            .spawn_tasks(
                "Main",
                0,
                vec![
                    TaskItem {
                        index: 0,
                        id: "TaskChild".to_owned(),
                        agent: "task".to_owned(),
                        assignment: "plain work".to_owned(),
                        todo_task_id: None,
                        ..TaskItem::default()
                    },
                    TaskItem {
                        index: 1,
                        id: "QaChild".to_owned(),
                        agent: "qa".to_owned(),
                        assignment: "ghost work".to_owned(),
                        todo_task_id: None,
                        ..TaskItem::default()
                    },
                ],
            )
            .expect("the whole batch must spawn despite the unknown tool");
        assert_eq!(batch.len(), 2, "both items spawn: {batch:?}");
        settle(&runtime, &batch).await;
        assert_eq!(
            runtime.unknown_tool_warnings().len(),
            1,
            "the qa item warns once, the task item never: {:?}",
            runtime.unknown_tool_warnings()
        );
    }

    #[tokio::test]
    async fn max_tools_per_agent_counts_only_injected_names() {
        // max_tools_per_agent = 1; the qa definition declares 4 names but only
        // `read` is actually injected (the unknowns are dropped), so the
        // pre-check counts 1 — the spawn must NOT be rejected for
        // over-counting ghosts.
        let root = tempfile::tempdir().expect("root");
        let captured = Arc::new(Mutex::new(Vec::<String>::new()));
        let (_fixture, runtime) = qa_runtime(
            root.path(),
            &["read", "yield_output", "ghost_a", "ghost_b"],
            captured.clone(),
            1,
        )
        .await;
        let spawn = spawn_item(&runtime, "QaChild", "qa");
        settle(&runtime, &[spawn]).await;

        let injected = captured.lock().clone();
        assert_eq!(
            injected.iter().filter(|name| *name == "read").count(),
            1,
            "exactly one declared tool is injected: {injected:?}"
        );
        assert!(
            !injected
                .iter()
                .any(|name| name == "yield_output" || name == "ghost_a" || name == "ghost_b"),
            "unknowns never count toward or occupy the tool budget: {injected:?}"
        );
    }

    #[tokio::test]
    async fn bundled_task_spawn_inherits_full_toolset_beyond_cap() {
        // The bundled task agent declares no `tools`, inheriting the full
        // parent/default child tool set (19 coding tools). That inherited
        // set is exempt from cap 16: the spawn succeeds and the child
        // receives the inherited set plus orchestration plumbing.
        let root = tempfile::tempdir().expect("root");
        let captured = Arc::new(Mutex::new(Vec::<String>::new()));
        let fixture =
            isolated_session(root.path(), Vec::new(), Some(recording_stream(captured.clone())))
                .await;
        let snapshot = fixture.session.child_session_options_snapshot();
        let default_task = super::super::definitions::parse_agent_definition(
            Path::new("task.md"),
            "---\nname: task\ndescription: task\n---\nprompt",
            super::super::AgentDefinitionSource::Bundled,
            true,
        )
        .expect("default task");
        assert!(crate::effective_agent_tool_names(&default_task, None).is_none());
        assert_eq!(
            crate::resolved_child_coding_tools(&default_task, None).len(),
            19,
            "bundled task resolves the full default child coding set"
        );
        let mut config = OrchestrationConfig::new(
            AgentCatalog::from_agents(vec![default_task]),
            root.path().join("artifacts"),
        )
        .with_parent_model(fixture.session.model().expect("parent model"));
        config.max_tools_per_agent = 16;
        let runtime = OrchestrationRuntime::new(
            config,
            OrchestrationRuntime::child_factory_from_snapshot(snapshot),
        )
        .expect("runtime");
        let spawn = spawn_item(&runtime, "Todo1", "task");
        settle(&runtime, &[spawn]).await;
        let injected = captured.lock().clone();
        assert!(
            injected.len() > 16,
            "inherited set beyond the cap must be injected: {injected:?}"
        );
        for name in ["read", "bash", "edit", "grep"] {
            assert!(
                injected.iter().any(|tool| tool == name),
                "{name} must be inherited into the child: {injected:?}"
            );
        }
    }

    #[tokio::test]
    async fn explicit_tool_list_over_cap_still_fails() {
        // An explicit 17-name tool list stays capped at 16: the spawn fails
        // with the actionable cap error and never queues a job.
        let root = tempfile::tempdir().expect("root");
        let captured = Arc::new(Mutex::new(Vec::<String>::new()));
        let fixture =
            isolated_session(root.path(), Vec::new(), Some(recording_stream(captured))).await;
        let snapshot = fixture.session.child_session_options_snapshot();
        let declared: Vec<String> = super::super::definitions::DEFAULT_CHILD_CODING_TOOLS
            .iter()
            .take(17)
            .map(|name| (*name).to_owned())
            .collect();
        let wide = super::super::definitions::parse_agent_definition(
            Path::new("wide.md"),
            &format!(
                "---\nname: wide\ndescription: wide\ntools: [{}]\n---\nprompt",
                declared.join(", ")
            ),
            super::super::AgentDefinitionSource::Bundled,
            true,
        )
        .expect("wide definition");
        assert!(crate::effective_agent_tool_names(&wide, None).is_some());
        assert_eq!(
            crate::resolved_child_coding_tools(&wide, None),
            declared,
            "all 17 declared names are known and resolve unchanged"
        );
        let default_task = super::super::definitions::parse_agent_definition(
            Path::new("task.md"),
            "---\nname: task\ndescription: task\n---\nprompt",
            super::super::AgentDefinitionSource::Bundled,
            true,
        )
        .expect("default task");
        let mut config = OrchestrationConfig::new(
            AgentCatalog::from_agents(vec![default_task, wide]),
            root.path().join("artifacts"),
        )
        .with_parent_model(fixture.session.model().expect("parent model"));
        config.max_tools_per_agent = 16;
        let runtime = OrchestrationRuntime::new(
            config,
            OrchestrationRuntime::child_factory_from_snapshot(snapshot),
        )
        .expect("runtime");
        let error = runtime
            .spawn_tasks(
                "Main",
                0,
                vec![TaskItem {
                    index: 0,
                    id: "Wide1".to_owned(),
                    agent: "wide".to_owned(),
                    assignment: "work".to_owned(),
                    todo_task_id: None,
                    ..TaskItem::default()
                }],
            )
            .expect_err("explicit 17-tool list must stay capped at 16");
        let message = error.to_string();
        assert!(message.contains("cap 16"), "{message}");
        assert!(message.contains("resolves to 17 tools"), "{message}");
        assert!(
            message.contains("orchestration.maxToolsPerAgent"),
            "{message}"
        );
        assert!(
            runtime.jobs(None).is_empty(),
            "cap failure must not queue a job"
        );
    }

    #[tokio::test]
    async fn definition_declaring_only_unknowns_loads_with_zero_injected_tools() {
        // A definition declaring ONLY unknown names stays compatible and
        // spawns; no declared tool is injected (plumbing + yield remain).
        let root = tempfile::tempdir().expect("root");
        let captured = Arc::new(Mutex::new(Vec::<String>::new()));
        let (_fixture, runtime) = qa_runtime(
            root.path(),
            &["yield_output", "computer"],
            captured.clone(),
            16,
        )
        .await;
        let spawn = spawn_item(&runtime, "GhostChild", "qa");
        settle(&runtime, &[spawn]).await;

        let injected = captured.lock().clone();
        assert!(
            !injected
                .iter()
                .any(|name| name == "yield_output" || name == "computer"),
            "no unknown tool is injected: {injected:?}"
        );
        assert!(
            injected.iter().any(|name| name == "yield"),
            "plumbing + the yield tool are still appended: {injected:?}"
        );
        assert!(
            runtime.unknown_tool_warnings().len() == 2,
            "one warning per unknown tool: {:?}",
            runtime.unknown_tool_warnings()
        );
    }

    #[tokio::test]
    async fn settings_tools_override_over_cap_fails_closed_at_spawn() {
        // A settings.agents.<name>.tools override is an explicit list: it
        // must stay fail-closed against max_tools_per_agent even when the
        // bundled definition itself declares no tools. The inherited
        // `tools: None` exemption applies only to genuinely omitted lists.
        let root = tempfile::tempdir().expect("root");
        let captured = Arc::new(Mutex::new(Vec::<String>::new()));
        let fixture =
            isolated_session(root.path(), Vec::new(), Some(recording_stream(captured))).await;
        let snapshot = fixture.session.child_session_options_snapshot();
        let declared: Vec<String> = super::super::definitions::DEFAULT_CHILD_CODING_TOOLS
            .iter()
            .take(17)
            .map(|name| (*name).to_owned())
            .collect();
        let default_task = super::super::definitions::parse_agent_definition(
            Path::new("task.md"),
            "---\nname: task\ndescription: task\n---\nprompt",
            super::super::AgentDefinitionSource::Bundled,
            true,
        )
        .expect("default task");
        assert!(
            crate::effective_agent_tool_names(&default_task, None).is_none(),
            "the bundled task must start inherit-None"
        );
        let mut settings = BTreeMap::new();
        settings.insert(
            "task".to_owned(),
            crate::AgentRuntimeSettings {
                enabled: Some(true),
                model: None,
                tools: Some(declared.clone()),
            },
        );
        let mut config = OrchestrationConfig::new(
            AgentCatalog::from_agents(vec![default_task]),
            root.path().join("artifacts"),
        )
        .with_parent_model(fixture.session.model().expect("parent model"))
        .with_agent_settings(settings);
        config.max_tools_per_agent = 16;
        let runtime = OrchestrationRuntime::new(
            config,
            OrchestrationRuntime::child_factory_from_snapshot(snapshot),
        )
        .expect("runtime");
        let error = runtime
            .spawn_tasks(
                "Main",
                0,
                vec![TaskItem {
                    index: 0,
                    id: "OverrideWide".to_owned(),
                    agent: "task".to_owned(),
                    assignment: "work".to_owned(),
                    todo_task_id: None,
                    ..TaskItem::default()
                }],
            )
            .expect_err("explicit settings override must stay capped at 16");
        let message = error.to_string();
        assert!(message.contains("cap 16"), "{message}");
        assert!(message.contains("resolves to 17 tools"), "{message}");
        assert!(
            message.contains("orchestration.maxToolsPerAgent"),
            "{message}"
        );
        assert!(
            runtime.jobs(None).is_empty(),
            "cap failure must not queue a job"
        );
    }
}

#[cfg(test)]
mod memory_child_factory_tests {
    use super::*;
    use pi_ai::{
        AssistantMessage, AssistantMessageEvent, ContentBlock, Model, SimpleStreamOptions,
        StopReason, new_assistant_message_event_stream,
    };

    fn recording_stream(captured: Arc<Mutex<Vec<String>>>) -> pi_agent::StreamFn {
        Arc::new(
            move |model: Model, context: pi_ai::Context, _options: SimpleStreamOptions| {
                *captured.lock() = context.tools.iter().map(|tool| tool.name.clone()).collect();
                Box::pin(async move {
                    let mut done = AssistantMessage::pending(&model);
                    done.content = vec![ContentBlock::text("done")];
                    done.stop_reason = StopReason::Stop;
                    let stream = new_assistant_message_event_stream();
                    let producer = stream.clone();
                    let model = model.clone();
                    tokio::spawn(async move {
                        producer
                            .push(AssistantMessageEvent::Start {
                                partial: AssistantMessage::pending(&model),
                            })
                            .await;
                        producer
                            .push(AssistantMessageEvent::Done {
                                reason: StopReason::Stop,
                                message: done.clone(),
                            })
                            .await;
                        producer.end(Some(done)).await;
                    });
                    stream
                })
            },
        )
    }

    fn definition(root: &Path, persona: bool, tools: Option<&str>) -> AgentDefinition {
        let path = if persona {
            root.join("reviewer").join("persona.md")
        } else {
            root.join("task.md")
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("definition parent");
        }
        let tools = tools
            .map(|tools| format!("tools: [{tools}]\n"))
            .unwrap_or_default();
        let mut definition = super::super::definitions::parse_agent_definition(
            &path,
            &format!("---\nname: test\ndescription: test\n{tools}---\nprompt"),
            super::super::AgentDefinitionSource::User,
            true,
        )
        .expect("definition");
        if persona {
            definition.kind = super::super::definitions::AgentDefinitionKind::Persona;
        }
        definition
    }

    async fn parent_snapshot(
        root: &Path,
        captured: Arc<Mutex<Vec<String>>>,
        config: crate::MemoryConfig,
    ) -> (IsolatedSessionFixture, crate::ChildSessionOptionsSnapshot) {
        let fixture = isolated_session(root, Vec::new(), Some(recording_stream(captured))).await;
        let mut snapshot = fixture.session.child_session_options_snapshot();
        snapshot.memory = Some(Arc::new(move || Some(config.clone())));
        (fixture, snapshot)
    }
    fn request(
        definition: AgentDefinition,
        requested: Option<Vec<String>>,
        model: Model,
    ) -> ChildSessionRequest {
        ChildSessionRequest {
            child_id: "Child".to_owned(),
            parent_id: "Main".to_owned(),
            max_tools_per_agent: 32,
            depth: 1,
            definition,
            assignment: "work".to_owned(),
            system_prompt: String::new(),
            requested_tool_names: requested,
            orchestration_tools: Vec::new(),
            thinking_level: None,
            persona_skill_store: None,
            persona_skills: Arc::new(Vec::new()),
            resolved_model_sequence: Vec::new(),
            resolved_model_selected_index: 0,
            shared_skills: Arc::new(Vec::new()),
            model,
            output_schema: None,
            schema_mode: None,
            yield_state: Arc::new(YieldState::default()),
        }
    }

    #[tokio::test]
    async fn ordinary_and_persona_children_inherit_hindsight_for_default_and_explicit_memory_requests()
     {
        for (persona, explicit) in [(false, false), (false, true), (true, false), (true, true)] {
            let root = tempfile::tempdir().expect("root");
            let captured = Arc::new(Mutex::new(Vec::new()));
            let config = crate::MemoryConfig {
                backend: crate::MemoryBackend::Hindsight,
                hindsight_api_url: Some("https://memory.example.test/nondefault".to_owned()),
                hindsight_bank_id: "nondefault-bank".to_owned(),
                hindsight_scoping: crate::HindsightScoping::PerProject,
                ..Default::default()
            };
            let (_fixture, snapshot) = parent_snapshot(root.path(), captured.clone(), config).await;
            let model = snapshot.model.clone();
            let factory = OrchestrationRuntime::child_factory_from_snapshot(snapshot);
            let child = factory(request(
                definition(root.path(), persona, explicit.then_some("memory")),
                explicit.then(|| vec!["memory".to_owned()]),
                model,
            ))
            .await
            .expect("child");
            child.run("work", Vec::new()).await.expect("run child");
            let tools = captured.lock().clone();
            assert!(!tools.iter().any(|name| name == "memory"), "{tools:?}");
            for name in ["recall", "retain", "reflect"] {
                assert!(tools.iter().any(|tool| tool == name), "{tools:?}");
            }
        }
    }

    #[tokio::test]
    async fn child_factory_applies_off_and_local_to_ordinary_and_persona_scopes() {
        for persona in [false, true] {
            for backend in [crate::MemoryBackend::Off, crate::MemoryBackend::Local] {
                let root = tempfile::tempdir().expect("root");
                let captured = Arc::new(Mutex::new(Vec::new()));
                let (_fixture, snapshot) = parent_snapshot(
                    root.path(),
                    captured.clone(),
                    crate::MemoryConfig {
                        backend,
                        ..Default::default()
                    },
                )
                .await;
                let model = snapshot.model.clone();
                let factory = OrchestrationRuntime::child_factory_from_snapshot(snapshot);
                let child = factory(request(definition(root.path(), persona, None), None, model))
                    .await
                    .expect("child");
                child.run("work", Vec::new()).await.expect("run child");
                let tools = captured.lock().clone();
                if backend == crate::MemoryBackend::Off {
                    assert!(
                        !tools.iter().any(|name| matches!(
                            name.as_str(),
                            "memory" | "recall" | "retain" | "reflect"
                        )),
                        "{tools:?}"
                    );
                } else {
                    assert!(tools.iter().any(|name| name == "memory"), "{tools:?}");
                    assert!(
                        !tools
                            .iter()
                            .any(|name| matches!(name.as_str(), "recall" | "retain" | "reflect")),
                        "{tools:?}"
                    );
                }
            }
        }
    }

    fn named_tool(name: &str, capability: pi_agent::ToolCapability) -> AgentTool {
        AgentTool::new(
            name,
            name,
            pi_ai::Schema::object(std::collections::HashMap::new(), Vec::new()),
            |_| async { Ok(pi_agent::AgentToolResult::default()) },
        )
        .with_capability(capability)
    }

    #[tokio::test]
    async fn learn_skill_injects_only_for_explicit_writable_persona_declaration() {
        for (persona, ceiling, disallowed, expected) in [
            (true, None, Vec::<String>::new(), true),
            (
                true,
                Some(CapabilityCeiling {
                    read: true,
                    write: false,
                    exec: true,
                }),
                Vec::new(),
                false,
            ),
            (true, None, vec!["learn_skill".to_owned()], false),
            (false, None, Vec::new(), false),
        ] {
            let root = tempfile::tempdir().expect("root");
            let captured = Arc::new(Mutex::new(Vec::new()));
            let (_fixture, snapshot) = parent_snapshot(
                root.path(),
                captured.clone(),
                crate::MemoryConfig::default(),
            )
            .await;
            let model = snapshot.model.clone();
            let mut definition = definition(root.path(), persona, Some("learn_skill"));
            definition.capability_ceiling = ceiling;
            definition.disallowed_tools = disallowed;
            let store = if persona {
                let filesystem = CapabilityFs::new().expect("filesystem");
                let lease = filesystem
                    .open_root(
                        CapabilityRootPurpose::PersonaStore,
                        pi_sandbox::AbsolutePath::parse(
                            definition.persona_root().expect("persona root"),
                        )
                        .expect("absolute persona root"),
                        CapabilityAccess::PrivateReadWrite,
                        RootCreation::Existing,
                    )
                    .expect("persona lease");
                Some(crate::PersonaSkillStore::new(lease).expect("persona skill store"))
            } else {
                None
            };
            let mut request = request(definition, Some(vec!["learn_skill".to_owned()]), model);
            request.persona_skill_store = store;
            let child = OrchestrationRuntime::child_factory_from_snapshot(snapshot)(request)
                .await
                .expect("child");
            child.run("work", Vec::new()).await.expect("run child");
            assert_eq!(
                captured.lock().iter().any(|name| name == "learn_skill"),
                expected
            );
        }
    }

    fn skill_read_context(path: &str) -> pi_agent::ToolCallContext {
        let (_controller, abort) = pi_agent::AbortController::new();
        std::mem::forget(_controller);
        pi_agent::ToolCallContext {
            tool_call_id: "read-skill".to_owned(),
            arguments: serde_json::json!({ "path": path }),
            on_update: Arc::new(|_result: pi_agent::AgentToolResult| {}),
            abort,
            model: None,
            approval_token: None,
        }
    }

    fn skill_read_text(result: &pi_agent::AgentToolResult) -> String {
        match result.content.first() {
            Some(ContentBlock::Text { text, .. }) => text.clone(),
            _ => String::new(),
        }
    }

    fn trusted_shared_skill(root: &Path, name: &str, body: &str) -> crate::Skill {
        let base = root.join(name);
        std::fs::create_dir_all(&base).expect("shared skill directory");
        std::fs::write(base.join("SKILL.md"), body).expect("shared skill file");
        crate::Skill {
            name: name.to_owned(),
            description: format!("{name} shared"),
            file_path: base.join("SKILL.md").to_string_lossy().into_owned(),
            base_dir: base.to_string_lossy().into_owned(),
            globs: Vec::new(),
            always_apply: false,
            hidden: false,
            disable_model_invocation: false,
            source: crate::SkillSource::User,
            managed_project_boundary: None,
            managed_root: None,
            trusted: true,
        }
    }

    #[tokio::test]
    async fn persona_child_skill_uri_overlays_shared_catalog() {
        let root = tempfile::tempdir().expect("root");
        let overlap = trusted_shared_skill(root.path(), "overlap", "shared body");
        let unique_shared = trusted_shared_skill(root.path(), "shared-asset", "shared unique");
        std::fs::write(root.path().join("shared-asset/asset.txt"), "shared asset body")
            .expect("shared asset");
        let shared = Arc::new(vec![overlap, unique_shared]);
        let persona_skills = Arc::new(vec![
            crate::PersonaSkill {
                name: "overlap".to_owned(),
                description: "persona description".to_owned(),
                content: "persona body".to_owned(),
            },
            crate::PersonaSkill {
                name: "persona-only".to_owned(),
                description: "persona only".to_owned(),
                content: "persona only body".to_owned(),
            },
        ]);
        let captured = Arc::new(Mutex::new(Vec::new()));
        let (_fixture, snapshot) = parent_snapshot(
            root.path(),
            captured,
            crate::MemoryConfig::default(),
        )
        .await;
        let model = snapshot.model.clone();
        let factory = OrchestrationRuntime::child_factory_from_snapshot(snapshot);

        let mut persona_request = request(
            definition(root.path(), true, Some("read")),
            Some(vec!["read".to_owned()]),
            model.clone(),
        );
        persona_request.shared_skills = shared.clone();
        persona_request.persona_skills = persona_skills.clone();
        let persona_child = factory(persona_request).await.expect("persona child");
        let persona_read = persona_child
            .active_tools()
            .into_iter()
            .find(|tool| tool.name == "read")
            .expect("persona read tool");
        let overlap_text = skill_read_text(
            &(persona_read.execute)(skill_read_context("skill://overlap"))
                .await
                .expect("persona overlap"),
        );
        assert!(overlap_text.contains("persona body"), "{overlap_text}");
        assert!(!overlap_text.contains("shared body"), "{overlap_text}");
        let only = skill_read_text(
            &(persona_read.execute)(skill_read_context("skill://persona-only"))
                .await
                .expect("persona only"),
        );
        assert!(only.contains("persona only body"), "{only}");
        let traversal = (persona_read.execute)(skill_read_context("skill://overlap/../escape"))
            .await
            .expect_err("persona traversal")
            .to_string();
        assert!(
            traversal.contains("persona skills expose only their main SKILL.md"),
            "{traversal}"
        );
        let asset = skill_read_text(
            &(persona_read.execute)(skill_read_context("skill://shared-asset/asset.txt"))
                .await
                .expect("shared asset fallthrough"),
        );
        assert!(asset.contains("shared asset body"), "{asset}");

        let mut ordinary_request = request(
            definition(root.path(), false, Some("read")),
            Some(vec!["read".to_owned()]),
            model,
        );
        ordinary_request.shared_skills = shared;
        let ordinary_child = factory(ordinary_request).await.expect("ordinary child");
        let ordinary_read = ordinary_child
            .active_tools()
            .into_iter()
            .find(|tool| tool.name == "read")
            .expect("ordinary read tool");
        let shared_text = skill_read_text(
            &(ordinary_read.execute)(skill_read_context("skill://overlap"))
                .await
                .expect("ordinary overlap"),
        );
        assert!(shared_text.contains("shared body"), "{shared_text}");
        assert!(!shared_text.contains("persona body"), "{shared_text}");
        let missing = (ordinary_read.execute)(skill_read_context("skill://persona-only"))
            .await
            .expect_err("ordinary persona-only")
            .to_string();
        assert!(missing.contains("unknown or untrusted skill"), "{missing}");
    }

    #[tokio::test]
    async fn disallowed_plumbing_removes_five_tools_and_keeps_single_yield() {
        let root = tempfile::tempdir().expect("root");
        let captured = Arc::new(Mutex::new(Vec::new()));
        let (_fixture, snapshot) = parent_snapshot(
            root.path(),
            captured.clone(),
            crate::MemoryConfig::default(),
        )
        .await;
        let model = snapshot.model.clone();
        let mut definition = definition(root.path(), true, Some("read"));
        definition.disallowed_tools = ["todo", "process", "task", "hub", "goal", "yield"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let mut request = request(definition, Some(vec!["read".to_owned()]), model);
        request.orchestration_tools = vec![
            named_tool("todo", pi_agent::ToolCapability::Write),
            named_tool("process", pi_agent::ToolCapability::Exec),
            named_tool("task", pi_agent::ToolCapability::Exec),
            named_tool("hub", pi_agent::ToolCapability::Read),
            named_tool("goal", pi_agent::ToolCapability::Write),
            named_tool("yield", pi_agent::ToolCapability::Write),
        ];
        let child = OrchestrationRuntime::child_factory_from_snapshot(snapshot)(request)
            .await
            .expect("child");
        child.run("work", Vec::new()).await.expect("run child");
        let tools = captured.lock().clone();
        for removed in ["todo", "process", "task", "hub", "goal"] {
            assert!(!tools.iter().any(|name| name == removed), "{tools:?}");
        }
        assert_eq!(
            tools.iter().filter(|name| name.as_str() == "yield").count(),
            1
        );
    }
}

#[cfg(test)]
mod settle_tool_failure_tests {
    //! The last-tool-failure contract for settled child runs: when the final
    //! actual `ToolResult`/`BashExecution` failed and no later tool result
    //! succeeded, `settle_child_outcome` must surface a bounded error so the
    //! job settles `Failed` (never Completed/Done/Integrated), while a
    //! recovery (a later successful tool result) settles cleanly. Assistant
    //! prose is ignored — the check is structural, never content-derived.
    //!
    //! Denied/unavailable attempts are structural too, and are not failures:
    //! an error `ToolResult` for a tool outside the child's own role-filtered
    //! tool set (a read-only child calling `write`) stays visible as an
    //! `is_error` result but settles the job Completed — only error results
    //! for tools the child actually possesses, plus non-zero/cancelled bash,
    //! are authoritative failures.

    use super::*;
    use crate::AgentDefinitionSource;

    fn tool_result(tool_name: &str, is_error: bool, timestamp: i64) -> pi_ai::Message {
        pi_ai::Message::ToolResult(pi_ai::ToolResultMessage {
            tool_call_id: format!("call-{timestamp}"),
            tool_name: tool_name.to_owned(),
            content: vec![pi_ai::ContentBlock::text(if is_error {
                "boom"
            } else {
                "ok"
            })],
            usage: None,
            details: None,
            added_tool_names: Vec::new(),
            is_error,
            timestamp,
        })
    }

    fn bash(
        command: &str,
        exit_code: Option<i32>,
        cancelled: bool,
        timestamp: i64,
    ) -> pi_ai::Message {
        pi_ai::Message::BashExecution(pi_ai::BashExecutionMessage {
            command: command.to_owned(),
            output: String::new(),
            exit_code,
            cancelled,
            truncated: false,
            full_output_path: None,
            timestamp,
            exclude_from_context: None,
        })
    }

    fn prose(text: &str) -> pi_ai::Message {
        pi_ai::Message::Assistant(pi_ai::AssistantMessage {
            content: vec![pi_ai::ContentBlock::text(text.to_owned())],
            ..pi_ai::AssistantMessage::pending(&pi_ai::Model::default())
        })
    }

    fn tool_names(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    fn task_definition() -> AgentDefinition {
        super::super::definitions::parse_agent_definition(
            Path::new("task.md"),
            "---\nname: task\ndescription: task\ntools: [read]\n---\nprompt",
            AgentDefinitionSource::Bundled,
            true,
        )
        .expect("definition")
    }

    async fn session_with_tools(root: &Path, names: &[&str]) -> IsolatedSessionFixture {
        let tools = names
            .iter()
            .map(|name| {
                AgentTool::new(*name, *name, pi_ai::Schema::default(), |_| async {
                    Ok(pi_agent::AgentToolResult::text("ok"))
                })
            })
            .collect::<Vec<_>>();
        isolated_session(root, tools, None).await
    }

    async fn session(root: &Path) -> IsolatedSessionFixture {
        session_with_tools(root, &[]).await
    }

    fn settled_run(
        text: &str,
        messages: Vec<pi_ai::Message>,
        error_message: Option<String>,
    ) -> crate::RunResult {
        crate::RunResult {
            text: text.to_owned(),
            messages,
            tool_calls: Vec::new(),
            usage: pi_ai::Usage::default(),
            stop_reason: pi_ai::StopReason::Stop,
            error_message,
        }
    }

    #[test]
    fn failed_tool_result_is_last_failure_despite_trailing_prose() {
        // fail tool → final prose still error: assistant text after the
        // failure never clears the authoritative structural failure.
        let messages = vec![
            prose("starting work"),
            tool_result("read", false, 1),
            tool_result("edit", true, 2),
            prose("wrapping up after the edit failed"),
        ];
        assert_eq!(
            last_failed_tool_kind(&messages, &tool_names(&["read", "edit"])).as_deref(),
            Some("edit")
        );
    }

    #[test]
    fn failed_tool_then_successful_tool_recovers() {
        // fail → success recovery: a later successful tool result clears the
        // earlier failure, so the run is allowed to complete normally.
        let messages = vec![
            tool_result("edit", true, 1),
            prose("retrying"),
            tool_result("edit", false, 2),
            prose("fixed"),
        ];
        assert_eq!(
            last_failed_tool_kind(&messages, &tool_names(&["edit"])),
            None
        );
    }

    #[test]
    fn successful_or_tool_free_transcripts_are_not_failures() {
        assert_eq!(last_failed_tool_kind(&[], &tool_names(&[])), None);
        assert_eq!(
            last_failed_tool_kind(&[tool_result("read", false, 1)], &tool_names(&["read"])),
            None
        );
        assert_eq!(
            last_failed_tool_kind(
                &[prose("no tools called"), prose("still no tools")],
                &tool_names(&[])
            ),
            None
        );
    }

    #[test]
    fn bash_nonzero_cancelled_and_zero_outcomes() {
        // Bash behavior is independent of the tool set: a child that runs
        // bash necessarily possesses it, so non-zero/cancelled bash always
        // fails regardless of `available_tools`.
        assert_eq!(
            last_failed_tool_kind(&[bash("false", Some(1), false, 1)], &tool_names(&[])).as_deref(),
            Some("bash")
        );
        assert_eq!(
            last_failed_tool_kind(&[bash("sleep", None, true, 1)], &tool_names(&[])).as_deref(),
            Some("bash")
        );
        assert_eq!(
            last_failed_tool_kind(&[bash("true", Some(0), false, 1)], &tool_names(&[])),
            None
        );
        // A zero exit clears an earlier non-zero failure (recovery).
        assert_eq!(
            last_failed_tool_kind(
                &[
                    bash("false", Some(1), false, 1),
                    bash("true", Some(0), false, 2)
                ],
                &tool_names(&[])
            ),
            None
        );
        // No exit code reported and not cancelled is not a failure signal.
        assert_eq!(
            last_failed_tool_kind(&[bash("?", None, false, 1)], &tool_names(&[])),
            None
        );
    }

    #[test]
    fn denied_tool_outside_child_tool_set_is_visible_but_not_fatal() {
        // A read-only child (tool set {"read"}) deliberately calls `write`;
        // the denial is recorded as an is_error ToolResult but is not a
        // failure — the run completes despite the trailing prose.
        let messages = vec![
            tool_result("read", false, 1),
            tool_result("write", true, 2),
            prose("scout finished without write"),
        ];
        assert_eq!(
            last_failed_tool_kind(&messages, &tool_names(&["read"])),
            None
        );
    }

    #[test]
    fn denied_tool_does_not_clear_an_earlier_real_failure() {
        // A denial is neither a success nor a failure: it must not mask an
        // unrecovered real failure that precedes it.
        let messages = vec![
            tool_result("edit", true, 1),
            tool_result("write", true, 2),
            prose("wrapping up"),
        ];
        assert_eq!(
            last_failed_tool_kind(&messages, &tool_names(&["edit"])).as_deref(),
            Some("edit")
        );
    }

    #[test]
    fn real_failure_of_possessed_tool_still_fails() {
        // A genuine execution failure of a tool the child actually possesses
        // stays authoritative even when a later tool is denied.
        let messages = vec![
            tool_result("read", false, 1),
            tool_result("write", true, 2),
            tool_result("read", true, 3),
            prose("done"),
        ];
        assert_eq!(
            last_failed_tool_kind(&messages, &tool_names(&["read", "write"])).as_deref(),
            Some("read")
        );
        // Same transcript against a write-less role: the write attempt is a
        // denial, but the possessed read failure still fails the run.
        assert_eq!(
            last_failed_tool_kind(&messages, &tool_names(&["read"])).as_deref(),
            Some("read")
        );
        // A real write failure (the child possesses write) is authoritative
        // even after a successful read — only a later success would clear it.
        assert_eq!(
            last_failed_tool_kind(
                &[tool_result("read", false, 1), tool_result("write", true, 2)],
                &tool_names(&["read", "write"])
            )
            .as_deref(),
            Some("write")
        );
    }

    #[tokio::test]
    async fn settle_failed_last_tool_keeps_prose_and_surfaces_bounded_error() {
        let root = tempfile::tempdir().expect("root");
        let parent = session(root.path()).await;
        let snapshot = parent.session.child_session_options_snapshot();
        let child_fixture = session_with_tools(root.path(), &["read", "edit"]).await;
        let child = ChildSession::new(child_fixture.session.clone());
        let config = OrchestrationConfig::new(
            AgentCatalog::from_agents(vec![task_definition()]),
            root.path().join("artifacts"),
        );
        let runtime = OrchestrationRuntime::new(
            config,
            OrchestrationRuntime::child_factory_from_snapshot(snapshot),
        )
        .expect("runtime");
        let result = settled_run(
            "final prose after the failed tool",
            vec![
                tool_result("read", false, 1),
                tool_result("edit", true, 2),
                prose("wrapping up"),
            ],
            None,
        );
        let (output, error, _) =
            runtime.settle_child_outcome(Ok(result), &child, &YieldState::default(), false);
        // The final prose stands as output, but the authoritative tool
        // failure still errors the job — prose never masks it.
        assert_eq!(output, "final prose after the failed tool");
        assert_eq!(error.as_deref(), Some("last tool execution failed: edit"));
    }

    #[tokio::test]
    async fn settle_denied_write_on_read_only_child_completes() {
        // A read-only child whose only tool is `read` deliberately calls
        // `write`: the denial stays visible in the transcript as an is_error
        // result, but the run completes without error (the campaign contract:
        // the write denial is recorded while the scout finishes read-only).
        let root = tempfile::tempdir().expect("root");
        let parent = session(root.path()).await;
        let snapshot = parent.session.child_session_options_snapshot();
        let child_fixture = session_with_tools(root.path(), &["read"]).await;
        let child = ChildSession::new(child_fixture.session.clone());
        let config = OrchestrationConfig::new(
            AgentCatalog::from_agents(vec![task_definition()]),
            root.path().join("artifacts"),
        );
        let runtime = OrchestrationRuntime::new(
            config,
            OrchestrationRuntime::child_factory_from_snapshot(snapshot),
        )
        .expect("runtime");
        let result = settled_run(
            "scout finished without write",
            vec![
                tool_result("read", false, 1),
                tool_result("write", true, 2),
                prose("scout finished without write"),
            ],
            None,
        );
        let (output, error, _) =
            runtime.settle_child_outcome(Ok(result), &child, &YieldState::default(), false);
        assert_eq!(
            output,
            format!("scout finished without write\n\n{MISSING_YIELD_WARNING}")
        );
        assert!(
            error.is_none(),
            "denied write on a read-only child must not error the job: {error:?}"
        );
    }

    #[tokio::test]
    async fn settle_recovered_tool_failure_completes_without_error() {
        let root = tempfile::tempdir().expect("root");
        let parent = session(root.path()).await;
        let snapshot = parent.session.child_session_options_snapshot();
        let child_fixture = session_with_tools(root.path(), &["edit"]).await;
        let child = ChildSession::new(child_fixture.session.clone());
        let config = OrchestrationConfig::new(
            AgentCatalog::from_agents(vec![task_definition()]),
            root.path().join("artifacts"),
        );
        let runtime = OrchestrationRuntime::new(
            config,
            OrchestrationRuntime::child_factory_from_snapshot(snapshot),
        )
        .expect("runtime");
        let result = settled_run(
            "recovered and done",
            vec![
                tool_result("edit", true, 1),
                tool_result("edit", false, 2),
                prose("done"),
            ],
            None,
        );
        let (output, error, _) =
            runtime.settle_child_outcome(Ok(result), &child, &YieldState::default(), false);
        assert_eq!(
            output,
            format!("recovered and done\n\n{MISSING_YIELD_WARNING}")
        );
        assert!(
            error.is_none(),
            "recovered tool failure must not error: {error:?}"
        );
    }

    #[tokio::test]
    async fn settle_provider_error_keeps_priority_over_tool_failure() {
        let root = tempfile::tempdir().expect("root");
        let parent = session(root.path()).await;
        let snapshot = parent.session.child_session_options_snapshot();
        let child_fixture = session_with_tools(root.path(), &["read"]).await;
        let child = ChildSession::new(child_fixture.session.clone());
        let config = OrchestrationConfig::new(
            AgentCatalog::from_agents(vec![task_definition()]),
            root.path().join("artifacts"),
        );
        let runtime = OrchestrationRuntime::new(
            config,
            OrchestrationRuntime::child_factory_from_snapshot(snapshot),
        )
        .expect("runtime");
        let result = settled_run(
            "partial prose",
            vec![tool_result("read", true, 1)],
            Some("provider: upstream request failed".to_owned()),
        );
        let (_, error, _) =
            runtime.settle_child_outcome(Ok(result), &child, &YieldState::default(), false);
        // The provider's own error wins; the tool failure summary must not
        // replace or be appended to it.
        assert_eq!(error.as_deref(), Some("provider: upstream request failed"));
    }
}
