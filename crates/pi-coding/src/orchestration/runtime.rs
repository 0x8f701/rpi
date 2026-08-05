use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs::{self, File};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, atomic::{AtomicBool, AtomicU64, Ordering}};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use parking_lot::Mutex;
use pi_agent::{AgentTool, BoxFuture, ThinkingLevel};
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, Semaphore, broadcast};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use unicode_normalization::UnicodeNormalization;

use crate::{Session, Skill};

use super::{
    AgentCatalog, AgentDefinition, JobClock, JobManager, JobRetention, JobSnapshot, JobStatus,
    PreparedJobRecords, TaskSpawn,
};

pub const DEFAULT_MAILBOX_CAPACITY: usize = 100;
pub const DEFAULT_MAX_CONCURRENCY: usize = 4;
pub const DEFAULT_MAX_RECURSION_DEPTH: usize = 2;
pub const DEFAULT_MAX_TOOLS_PER_AGENT: usize = 16;
pub const DEFAULT_IDLE_TTL_SECS: u64 = 300;
pub const DEFAULT_MAX_RETAINED_JOBS: usize = 256;
pub const DEFAULT_RETAINED_JOB_TTL_SECS: u64 = 24 * 60 * 60;
/// Stable custom-message type for orchestration mailbox deliveries.
pub const ORCHESTRATION_MESSAGE_TYPE: &str = "orchestration_message";
const MAX_AUTOLOAD_SKILL_BYTES: u64 = 256 * 1024;
const MAX_AUTOLOAD_PROMPT_BYTES: usize = 1024 * 1024;
const MAX_SIBLING_ROSTER_ENTRIES: usize = 64;
const MAX_SIBLING_ROSTER_BYTES: usize = MAX_AUTOLOAD_PROMPT_BYTES / 64;
const MAX_ROSTER_AGENT_CHARS: usize = 80;
const CHILD_ABORT_GRACE: Duration = Duration::from_secs(2);
pub type AgentSelectorFn = Arc<
    dyn Fn(&str, &[AgentDefinition]) -> Option<String> + Send + Sync,
>;
pub type ParentModelProvider = Arc<dyn Fn() -> pi_ai::Model + Send + Sync>;
pub type ChildSessionFactory =
    Arc<dyn Fn(ChildSessionRequest) -> BoxFuture<Result<Session>> + Send + Sync>;
#[derive(Clone)]
pub struct ChildSession {
    session: Session,
}

impl ChildSession {
    #[must_use]
    pub fn new(session: Session) -> Self {
        Self { session }
    }

    async fn steer(&self, message: &MailboxMessage) {
        self.session
            .steer(pi_ai::Message::Custom(pi_ai::CustomMessage {
                custom_type: ORCHESTRATION_MESSAGE_TYPE.to_owned(),
                content: format_orchestration_message(message).into(),
                display: true,
                details: Some(serde_json::json!({
                    "id": message.id,
                    "from": message.from,
                    "to": message.to,
                    "body": message.body,
                    "replyTo": message.reply_to,
                })),
                timestamp: i64::try_from(message.timestamp).unwrap_or(i64::MAX),
            }))
            .await;
    }

    async fn run(&self, assignment: &str) -> Result<crate::RunResult> {
        self.session.run(assignment, Vec::new()).await
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
    selector_settings: Option<crate::SelectorSettings>,
    pub agent_settings: BTreeMap<String, crate::AgentRuntimeSettings>,
    pub parent_model: pi_ai::Model,
    parent_model_provider: Option<ParentModelProvider>,
    pub idle_ttl: Option<Duration>,
    pub max_retained_jobs: usize,
    pub retained_job_ttl: Duration,
    job_clock: JobClock,
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
            .field("idle_ttl", &self.idle_ttl)
            .field("max_retained_jobs", &self.max_retained_jobs)
            .field("retained_job_ttl", &self.retained_job_ttl)
            .field("agent_settings", &self.agent_settings)
            .field("parent_model_provider", &self.parent_model_provider.as_ref().map(|_| "configured"))
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
                let cwd = snapshot.cwd.to_string_lossy();
                let base_tools = match request.requested_tool_names.as_deref() {
                    Some(names) => names
                        .iter()
                        .filter(|name| !matches!(name.as_str(), "todo" | "process" | "task" | "hub" | "goal"))
                        .map(|name| crate::create_tool_with_context_and_resolver(name, &cwd, None, None, uri_resolver.clone()))
                        .collect::<Result<Vec<_>>>()?,
                    None => crate::create_coding_tools_with_context_and_resolver(&cwd, None, None, None, uri_resolver.clone()),
                };
                if base_tools.len() > request.max_tools_per_agent {
                    bail!(
                        "child agent {:?} resolves to more than {} tools",
                        request.definition.name,
                        request.max_tools_per_agent
                    );
                }
                let mut tools = base_tools;
                tools.extend(request.orchestration_tools);
                let api_key = if request.model.provider == snapshot.model.provider {
                    snapshot.api_key
                } else if let Some(resolver) = &snapshot.auth_resolver {
                    resolver(request.model.clone())
                        .await
                        .with_context(|| format!("resolving auth for child model {}/{}", request.model.provider, request.model.id))?
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
                Session::new_with_additional_tools_filtered_discovery_and_uri(
                    crate::SessionOptions {
                        model: request.model,
                        cwd: snapshot.cwd,
                        system_prompt: request.system_prompt,
                        thinking_level: request.thinking_level.unwrap_or(snapshot.thinking_level),
                        api_key,
                        compaction: None,
                        stream_options,
                        tools: Some(tools),
                        before_tool_call: None,
                        after_tool_call: None,
                        stream_fn: Some(snapshot.stream_fn),
                        auth_resolver: snapshot.auth_resolver,
                    },
                    Vec::new(),
                    crate::ToolSelection::default(),
                    crate::ResourceDiscovery::Disabled,
                    uri_resolver.clone(),
                )
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
            selector_settings: None,
            agent_settings: BTreeMap::new(),
            parent_model: pi_ai::Model::default(),
            parent_model_provider: None,
            idle_ttl: Some(Duration::from_secs(DEFAULT_IDLE_TTL_SECS)),
            max_retained_jobs: DEFAULT_MAX_RETAINED_JOBS,
            retained_job_ttl: Duration::from_secs(DEFAULT_RETAINED_JOB_TTL_SECS),
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
        let default = self
            .catalog
            .get(&self.default_agent)
            .ok_or_else(|| anyhow!("default orchestration agent {:?} was not discovered", self.default_agent))?;
        if !default.trusted {
            bail!("default orchestration agent {:?} is not trusted", self.default_agent);
        }
        if let Some(agent) = self.catalog.agents().iter().find(|agent| !agent.trusted) {
            bail!("orchestration catalog contains untrusted agent {:?}", agent.name);
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
    pub fn with_selector_settings(
        mut self,
        settings: crate::SelectorSettings,
    ) -> Self {
        self.selector_settings = Some(settings.clone());
        self.default_agent_selector = Some(Arc::new(move |request, agents| {
            match crate::selector::exact_agent_mention(request, agents) {
                crate::selector::ExactAgentMention::Unique(name) => Some(name),
                crate::selector::ExactAgentMention::Ambiguous(_) => None,
                crate::selector::ExactAgentMention::None => {
                    let ranked = crate::rank_agents(request, agents, &settings);
                    crate::select_default_agent(&ranked, &settings)
                }
            }
        }));
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
            && self.agent_settings == other.agent_settings
            && parent_models_equal
            && self.idle_ttl == other.idle_ttl
            && self.max_retained_jobs == other.max_retained_jobs
            && self.retained_job_ttl == other.retained_job_ttl
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
    pub thinking_level: Option<ThinkingLevel>,
    pub model: pi_ai::Model,
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
            .field("system_prompt", &self.system_prompt)
            .field("requested_tool_names", &self.requested_tool_names)
            .field("max_tools_per_agent", &self.max_tools_per_agent)
            .field("thinking_level", &self.thinking_level)
            .field("model", &format!("{}/{}", self.model.provider, self.model.id))
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
    /// Live projection of a message successfully delivered to Main.
    /// Does not drain the mailbox; presentation-only for parent TUI/human UI.
    MessageDelivered {
        group_id: String,
        message: MailboxMessage,
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
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeliveryReceipt {
    pub to: String,
    pub outcome: DeliveryOutcome,
    /// Original `to` when it differed from the canonical agent id (e.g. job UUID).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskItem {
    pub index: usize,
    pub id: String,
    pub agent: String,
    pub assignment: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub todo_task_id: Option<String>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub artifact_ref: String,
    pub history_ref: String,
    pub artifact_uri: String,
}

pub struct OrchestrationRuntime {
    inner: Arc<RuntimeInner>,
    owner: bool,
}

impl Clone for OrchestrationRuntime {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            owner: false,
        }
    }
}

struct RuntimeInner {
    group_id: String,
    config: OrchestrationConfig,
    factory: ChildSessionFactory,
    semaphore: Arc<Semaphore>,
    shutdown: CancellationToken,
    active: Mutex<HashMap<String, CancellationToken>>,
    active_changed: Notify,
    park_timers: Mutex<HashMap<String, JoinHandle<()>>>,
    jobs: JobManager,
    workflow_scope: Mutex<Option<WorkflowRuntimeScope>>,
    global_concurrency: Mutex<Option<OrchestrationConcurrencyGate>>,
    events: broadcast::Sender<OrchestrationEvent>,
    durable: Mutex<Option<super::persistence::DurableRuntime>>,
    durable_bound: AtomicBool,
    durable_mutation: Mutex<()>,
    rebind_reserved: AtomicBool,
}

#[derive(Default)]
struct GlobalRegistry {
    groups: Mutex<HashMap<String, HashMap<String, Arc<AgentEntry>>>>,
}

/// A registered `hub wait` interest tied to an agent's mailbox lifetime.
///
/// While at least one `MessageWaiter` is registered for an `AgentEntry`,
/// `deliver_committed` skips active steering for matching messages so the
/// waiting task can durably drain them via `wait_message` instead of racing
/// the active delivery bridge. `from == None` is a wildcard claim. The guard
/// is RAII: dropping it removes the registration on every return path
/// (return/cancel/timeout/drop), so a stale claim can never strand a message.
struct MessageWaiter {
    token: u64,
    from: Option<String>,
}

struct MessageWaiterGuard {
    entry: Arc<AgentEntry>,
    token: u64,
}

impl Drop for MessageWaiterGuard {
    fn drop(&mut self) {
        let mut waiters = self.entry.waiters.lock();
        if let Some(index) = waiters.iter().position(|waiter| waiter.token == self.token) {
            waiters.remove(index);
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
    agents: Vec<super::persistence::PersistedAgent>,
    jobs: PreparedJobRecords,
}

pub struct PreparedDurableBinding {
    durable: super::persistence::DurableRuntime,
    recovered: Option<PreparedRecoveredState>,
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

static REGISTRY: LazyLock<GlobalRegistry> = LazyLock::new(GlobalRegistry::default);

impl OrchestrationRuntime {
    pub fn new(config: OrchestrationConfig, factory: ChildSessionFactory) -> Result<Self> {
        config.validate()?;
        let artifact_dir = absolute_lexical(&config.artifact_dir)?;
        fs::create_dir_all(&artifact_dir).with_context(|| {
            format!("creating orchestration artifact directory {}", artifact_dir.display())
        })?;
        let mut config = config;
        config.artifact_dir = artifact_dir;
        let (events, _) = broadcast::channel(256);
        let group_id = Uuid::now_v7().to_string();
        REGISTRY.register(
            &group_id,
            AgentSnapshot {
                id: config.main_agent_id.clone(),
                display_name: config.main_agent_id.clone(),
                agent: config.main_agent_id.clone(),
                parent_id: None,
                status: AgentStatus::Idle,
                created_at: now_millis(),
                last_activity: now_millis(),
                unread: 0,
                artifact_ref: None,
                history_ref: None,
            },
            config.mailbox_capacity,
        )?;
        let jobs = JobManager::with_retention(
            JobRetention {
                max_settled: config.max_retained_jobs,
                ttl: config.retained_job_ttl,
            },
            config.job_clock.clone(),
        );
        Ok(Self {
            inner: Arc::new(RuntimeInner {
                group_id,
                semaphore: Arc::new(Semaphore::new(config.max_concurrency)),
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
                durable: Mutex::new(None),
                durable_bound: AtomicBool::new(false),
                durable_mutation: Mutex::new(()),
                rebind_reserved: AtomicBool::new(false),
            }),
            owner: true,
        })
    }

    #[must_use]
    pub fn group_id(&self) -> &str {
        &self.inner.group_id
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
                group_id: self.inner.group_id.clone(),
                job: presentation_job_snapshot(job),
            })
            .collect::<Vec<_>>();
        events.extend(self.list(self.main_agent_id()).into_iter().map(|agent| {
            OrchestrationEvent::AgentUpdated {
                group_id: self.inner.group_id.clone(),
                agent: presentation_agent_snapshot(agent),
            }
        }));
        events
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
                    && crate::agent_compatibility_error(
                        agent,
                        settings,
                        &parent_model,
                        &available,
                    )
                    .is_none()
            })
            .collect()
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

    fn prune_retained_jobs(&self) {
        let _durable_mutation = self.inner.durable_mutation.lock();
        if self.inner.rebind_reserved.load(Ordering::Acquire) {
            return;
        }
        for candidate in self.inner.jobs.prune_candidates() {
            let artifact_path = self
                .inner
                .config
                .artifact_dir
                .join(format!("{}-{}.md", candidate.agent_id, candidate.job_id));
            let history_path = self
                .inner
                .config
                .artifact_dir
                .join(format!("{}-{}.history.json", candidate.agent_id, candidate.job_id));
            if remove_retained_file(&artifact_path) && remove_retained_file(&history_path) {
                REGISTRY.remove_agent_if_paths(
                    &self.inner.group_id,
                    &candidate.agent_id,
                    &artifact_path,
                    &history_path,
                );
                self.inner.jobs.remove_settled(&candidate.job_id);
            }
        }
    }

    fn cleanup_retained_jobs(&self) {
        let _durable_mutation = self.inner.durable_mutation.lock();
        if self.inner.rebind_reserved.load(Ordering::Acquire) {
            return;
        }
        for candidate in self.inner.jobs.settled_candidates() {
            let artifact_path = self
                .inner
                .config
                .artifact_dir
                .join(format!("{}-{}.md", candidate.agent_id, candidate.job_id));
            let history_path = self
                .inner
                .config
                .artifact_dir
                .join(format!("{}-{}.history.json", candidate.agent_id, candidate.job_id));
            if remove_retained_file(&artifact_path) && remove_retained_file(&history_path) {
                REGISTRY.remove_agent_if_paths(
                    &self.inner.group_id,
                    &candidate.agent_id,
                    &artifact_path,
                    &history_path,
                );
                self.inner.jobs.remove_settled(&candidate.job_id);
            }
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
    ) -> Result<PreparedDurableBinding> {
        self.prepare_parent_identity_binding(session_id, session_path)
    }

    pub(crate) fn prepare_parent_binding(&self, parent: &Session) -> Result<PreparedDurableBinding> {
        let (session_id, session_path) = parent
            .recorder_info()
            .ok_or_else(|| anyhow!("parent session recording is unavailable"))?;
        self.prepare_parent_identity(session_id, session_path)
    }

    fn prepare_parent_identity_binding(
        &self,
        session_id: String,
        session_path: PathBuf,
    ) -> Result<PreparedDurableBinding> {
        let _spawn_guard = self.inner.jobs.lock_spawns();
        let durable_mutation = self.inner.durable_mutation.lock();
        self.inner
            .rebind_reserved
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| anyhow!("durable orchestration rebind is already in progress"))?;
        let result = (|| {
            if !self.inner.active.lock().is_empty()
                || self.inner.jobs.snapshots(None).iter().any(|job| !job.status.is_settled())
            {
                bail!("cannot rebind durable orchestration while child jobs are active");
            }
            let child_root = session_path
                .parent()
                .ok_or_else(|| anyhow!("parent session path has no parent"))?
                .join("children")
                .join(&session_id);
            let durable = super::persistence::DurableRuntime::new(
                session_id,
                session_path,
                child_root,
            )?;
            let same_parent = self
                .inner
                .durable
                .lock()
                .as_ref()
                .is_some_and(|current| {
                    current.parent_session_id() == durable.parent_session_id()
                        && current.parent_session_path() == durable.parent_session_path()
                });
            let recovered = if same_parent {
                None
            } else {
                durable
                    .load_optional()?
                    .map(|state| self.prepare_recovered_state(&durable, state))
                    .transpose()?
            };
            let initialize = !same_parent && recovered.is_none();
            Ok(PreparedDurableBinding {
                durable,
                recovered,
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
            prepared.durable.persist(&super::persistence::build_state(
                prepared.durable.parent_session_id(),
                prepared.durable.parent_session_path(),
                Vec::new(),
                Vec::new(),
            ))?;
            prepared.recovered = Some(PreparedRecoveredState {
                group_entries: self.prepared_group_entries(HashMap::new()),
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
        self.initialize_prepared_binding(prepared)
    }

    pub(crate) fn commit_prepared_parent(&self, prepared: PreparedDurableBinding) {
        self.commit_parent_binding(prepared);
    }

    /// Install a fully prepared binding. This path is intentionally infallible:
    /// after an Application mutates its live Session, only in-memory swaps and
    /// notifications remain.
    pub(crate) fn commit_parent_binding(&self, mut prepared: PreparedDurableBinding) {
        let _durable_mutation = self.inner.durable_mutation.lock();
        if !prepared.same_parent {
            let recovered = prepared
                .install_recovered
                .then(|| prepared.recovered.take())
                .flatten();
            *self.inner.durable.lock() = Some(prepared.durable.clone());
            self.inner.durable_bound.store(true, Ordering::Release);
            if let Some(recovered) = recovered {
                self.install_prepared_state(recovered);
            }
        } else if prepared.install_recovered {
            if let Some(recovered) = prepared.recovered.take() {
                self.install_prepared_state(recovered);
            }
            *self.inner.durable.lock() = Some(prepared.durable.clone());
            self.inner.durable_bound.store(true, Ordering::Release);
        }
        prepared.release_reservation();
    }

    /// Bind to the parent recorder and either recover an existing sidecar or
    /// initialize fresh state when no sidecar exists. Existing state is always
    /// loaded and prebuilt before the first write or live-state swap.
    pub fn bind_and_recover(&self, parent: &Session) -> Result<()> {
        let already_bound = self.inner.durable_bound.load(Ordering::Acquire);
        let mut prepared = self.prepare_parent_binding(parent)?;
        if prepared.same_parent && already_bound {
            self.commit_parent_binding(prepared);
            return Ok(());
        }
        if prepared.same_parent {
            match prepared.durable.load_optional()? {
                Some(state) => {
                    prepared.recovered = Some(self.prepare_recovered_state(
                        &prepared.durable,
                        state,
                    )?);
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
    /// is replaced by a non-equivalent reload: the previous runtime's jobs are
    /// cancelled by its own shutdown, so the replacement must start clean
    /// rather than recovering the prior sidecar (which would resurrect
    /// cancelled jobs as live state under the new configuration).
    pub(crate) fn bind_and_reset(&self, parent: &Session) -> Result<()> {
        let mut prepared = self.prepare_parent_binding(parent)?;
        prepared.recovered = None;
        prepared.initialize = true;
        self.initialize_prepared_binding(&mut prepared)?;
        self.commit_parent_binding(prepared);
        Ok(())
    }

    /// Persist the current live snapshot to the durable sidecar. Used after a
    /// replaced runtime's shutdown re-writes the shared sidecar with stale
    /// state, so the replacement (now the sole durable owner) re-syncs it to
    /// its current state. Errors propagate: a clean sidecar is part of the
    /// reload contract.
    pub(crate) fn persist_durable_state(&self) -> Result<()> {
        self.persist_state()
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
        let durable = self
            .inner
            .durable
            .lock()
            .clone()
            .ok_or_else(|| anyhow!("durable orchestration binding is unavailable"))?;
        durable.persist_with(|| {
            Ok(super::persistence::build_state(
                durable.parent_session_id(),
                durable.parent_session_path(),
                self.collect_persisted_agents(),
                self.inner.jobs.snapshots(None),
            ))
        })
    }

 /// Collect all non-main agents with their persisted definition, request,
 /// session path, and mailbox for durable state.
 fn collect_persisted_agents(&self) -> Vec<super::persistence::PersistedAgent> {
 let group_id = self.inner.group_id.clone();
 let mut agents = Vec::new();
 let groups = REGISTRY.groups.lock();
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
 source: super::persistence::PersistedDefinitionSource::Bundled,
 path: None,
 trusted: true,
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
 }),
 session_path: durable_info
 .as_ref()
 .and_then(|info| info.session_path.as_ref().map(|p| p.to_string_lossy().into_owned())),
 mailbox,
 });
 }
 }
 drop(groups);
 agents
 }

    /// Attempt to revive a parked durable child. The caller holds
    /// `durable_mutation`, making the Parked→Queued claim and persistence one
    /// serialized transaction against sends and rebind commit.
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
        let entry = match REGISTRY.get(&self.inner.group_id, target) {
            Some(entry) => entry,
            None => return Ok(None),
        };
        let snapshot = entry.snapshot.lock().clone();
        if snapshot.status != AgentStatus::Parked {
            return Ok(None);
        }
        let info = match entry.durable_info.lock().clone() {
            Some(info) => info,
            None => return Ok(None),
        };
        let session_path = info
            .session_path
            .as_ref()
            .map(|path| durable.canonicalize_child_session_path(path))
            .transpose()?;
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
        let orchestration_tools = self.agent_tools(target, info.request.depth);
        let request = match super::persistence::reconstruct_request(
            &super::persistence::PersistedAgent {
                snapshot: snapshot.clone(),
                definition: info.definition.clone(),
                request: info.request.clone(),
                session_path: session_path
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned()),
                mailbox: Vec::new(),
            },
            resolved.model,
            &definition,
            orchestration_tools,
            self.inner.config.max_tools_per_agent,
        ) {
            Some(request) => request,
            None => return Ok(None),
        };
        if !REGISTRY.compare_status(
            &self.inner.group_id,
            target,
            AgentStatus::Parked,
            AgentStatus::Queued,
        )? {
            return Ok(None);
        }

        let job_id = Uuid::now_v7().to_string();
        let cancel = CancellationToken::new();
        let created_at = now_millis();
        let job_snapshot = JobSnapshot {
            id: job_id.clone(),
            agent_id: target.to_owned(),
            agent: definition.name.clone(),
            parent_id: info.request.parent_id.clone(),
            description: Some(one_line(&info.request.assignment)),
            todo_task_id: None,
            workflow_id: None,
            workflow_generation: None,
            status: JobStatus::Queued,
            created_at,
            started_at: None,
            finished_at: None,
            result: None,
        };
        if let Err(error) = self.inner.jobs.insert(job_snapshot.clone(), cancel.clone()) {
            let _ = REGISTRY.compare_status(
                &self.inner.group_id,
                target,
                AgentStatus::Queued,
                AgentStatus::Parked,
            );
            return Err(error);
        }
        self.inner.active.lock().insert(target.to_owned(), cancel.clone());
        if let Err(error) = self.persist_state() {
            self.inner.jobs.remove(&job_id);
            self.inner.active.lock().remove(target);
            let _ = REGISTRY.compare_status(
                &self.inner.group_id,
                target,
                AgentStatus::Queued,
                AgentStatus::Parked,
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

        let runtime = self.clone();
        let job_id_owned = job_id.clone();
        let target_owned = target.to_owned();
        let assignment = info.request.assignment.clone();
        tokio::spawn(async move {
            let result = runtime
                .run_revival(
                    target_owned,
                    request,
                    session_path,
                    assignment,
                    cancel,
                    &job_id_owned,
                )
                .await;
            {
                let _durable_mutation = runtime.inner.durable_mutation.lock();
                if !runtime.inner.rebind_reserved.load(Ordering::Acquire)
                    && let Some(job) = runtime.inner.jobs.finish(&job_id_owned, result, now_millis())
                {
                    let job = match runtime.persist_state() {
                        Ok(()) => job,
                        Err(error) => runtime
                            .inner
                            .jobs
                            .append_result_error(&job_id_owned, &error.to_string())
                            .unwrap_or(job),
                    };
                    runtime.publish_job(job);
                }
            }
            runtime.prune_retained_jobs();
        });
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
            group_id: self.inner.group_id.clone(),
        };
        let durable = match self.inner.durable.lock().clone() {
            Some(durable) => durable,
            None => return self.failed_result(
                &TaskItem { index: 0, id: agent_id.clone(), agent: request.definition.name.clone(), assignment: assignment.clone(), todo_task_id: None },
                job_id,
                AgentStatus::Idle,
                "durable orchestration binding is unavailable",
            ),
        };
        let artifact_ref = format!("agent://{agent_id}");
        let history_ref = format!("history://{agent_id}");
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
                    &TaskItem { index: 0, id: agent_id.clone(), agent: request.definition.name.clone(), assignment: assignment.clone(), todo_task_id: None },
                    job_id,
                    AgentStatus::Aborted,
                    "orchestration semaphore closed",
                ),
            },
            () = cancel.cancelled() => return self.failed_result(
                &TaskItem { index: 0, id: agent_id.clone(), agent: request.definition.name.clone(), assignment: assignment.clone(), todo_task_id: None },
                job_id,
                AgentStatus::Aborted,
                "task cancelled before start",
            ),
        };
        let durable_mutation = self.inner.durable_mutation.lock();
        if self.inner.rebind_reserved.load(Ordering::Acquire) {
            drop(durable_mutation);
            return self.failed_result(
                &TaskItem { index: 0, id: agent_id.clone(), agent: request.definition.name.clone(), assignment: assignment.clone(), todo_task_id: None },
                job_id,
                AgentStatus::Idle,
                "durable orchestration rebind is in progress",
            );
        }
        REGISTRY
            .set_status(&self.inner.group_id, &agent_id, AgentStatus::Running)
            .ok();
        let running_agent = self.agent_snapshot(&agent_id);
        let running_job = self.inner.jobs.mark_running(job_id, now_millis());
        if let Err(error) = self.persist_state() {
            let error = format!("persisting revived child running state: {error:#}");
            drop(durable_mutation);
            return self.failed_result(
                &TaskItem { index: 0, id: agent_id.clone(), agent: request.definition.name.clone(), assignment: assignment.clone(), todo_task_id: None },
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
        let child = match (self.inner.factory)(request).await {
            Ok(session) => {
                let recording = match session_path.as_deref() {
                    Some(path) => session.resume_durable_child_recording(path).await,
                    None => session.start_durable_child_recording(
                        durable.child_root(),
                        durable.parent_session_path(),
                    ),
                };
                if let Err(error) = recording {
                    return self.failed_result(
                        &TaskItem { index: 0, id: agent_id.clone(), agent: agent_type.clone(), assignment: assignment.clone(), todo_task_id: None },
                        job_id,
                        AgentStatus::Idle,
                        &format!("opening durable child transcript: {error:#}"),
                    );
                }
                if session_path.is_none() {
                    let Some((_, new_path)) = session.recorder_info() else {
                        return self.failed_result(
                            &TaskItem { index: 0, id: agent_id.clone(), agent: agent_type.clone(), assignment: assignment.clone(), todo_task_id: None },
                            job_id,
                            AgentStatus::Idle,
                            "durable child recorder is unavailable",
                        );
                    };
                    let new_path = match durable.canonicalize_child_session_path(&new_path) {
                        Ok(path) => path,
                        Err(error) => return self.failed_result(
                            &TaskItem { index: 0, id: agent_id.clone(), agent: agent_type.clone(), assignment: assignment.clone(), todo_task_id: None },
                            job_id,
                            AgentStatus::Idle,
                            &error.to_string(),
                        ),
                    };
                    let durable_mutation = self.inner.durable_mutation.lock();
                    if self.inner.rebind_reserved.load(Ordering::Acquire) {
                        drop(durable_mutation);
                        return self.failed_result(
                            &TaskItem { index: 0, id: agent_id.clone(), agent: agent_type.clone(), assignment: assignment.clone(), todo_task_id: None },
                            job_id,
                            AgentStatus::Idle,
                            "durable orchestration rebind is in progress",
                        );
                    }
                    if let Some(entry) = REGISTRY.get(&self.inner.group_id, &agent_id)
                        && let Some(info) = entry.durable_info.lock().as_mut()
                    {
                        info.session_path = Some(new_path);
                    }
                    if let Err(error) = self.persist_state() {
                        let error = format!("persisting revived child transcript path: {error:#}");
                        drop(durable_mutation);
                        return self.failed_result(
                            &TaskItem { index: 0, id: agent_id.clone(), agent: agent_type.clone(), assignment: assignment.clone(), todo_task_id: None },
                            job_id,
                            AgentStatus::Idle,
                            &error,
                        );
                    }
                    drop(durable_mutation);
                }
                ChildSession::new(session)
            }
            Err(error) => {
                return self.failed_result(
                    &TaskItem { index: 0, id: agent_id.clone(), agent: agent_type.clone(), assignment: assignment.clone(), todo_task_id: None },
                    job_id,
                    AgentStatus::Idle,
                    &error.to_string(),
                );
            }
        };

        // Register active delivery and drain the durable mailbox atomically
        // against concurrent durable sends.
        let (delivery_tx, mut delivery_rx) = tokio::sync::mpsc::unbounded_channel();
        let pre_run = {
            let durable_mutation = self.inner.durable_mutation.lock();
            if self.inner.rebind_reserved.load(Ordering::Acquire) {
                drop(durable_mutation);
                return self.failed_result(
                    &TaskItem { index: 0, id: agent_id.clone(), agent: agent_type.clone(), assignment: assignment.clone(), todo_task_id: None },
                    job_id,
                    AgentStatus::Idle,
                    "durable orchestration rebind is in progress",
                );
            }
            match REGISTRY.register_active_delivery(
                &self.inner.group_id,
                &agent_id,
                delivery_tx,
                cancel.clone(),
            ) {
                Ok(messages) => messages,
                Err(error) => {
                    drop(durable_mutation);
                    return self.failed_result(
                        &TaskItem { index: 0, id: agent_id.clone(), agent: agent_type.clone(), assignment: assignment.clone(), todo_task_id: None },
                        job_id,
                        AgentStatus::Idle,
                        &error.to_string(),
                    );
                }
            }
        };

        // Steer pre-run messages, acknowledging each only after delivery.
        for message in &pre_run {
            child.steer(message).await;
            self.acknowledge_delivery(&agent_id, &message.id);
        }

        // Run the revival as a continuation turn.
        let mut run = Box::pin(child.run(&assignment));

        let outcome = loop {
            tokio::select! {
                result = &mut run => break result.map_err(|error| error.to_string()),
                message = delivery_rx.recv() => {
                    match message {
                        Some(message) => {
                            child.steer(&message).await;
                            self.acknowledge_delivery(&agent_id, &message.id);
                        }
                        None => break Err("active child delivery bridge closed".to_owned()),
                    }
                }
                () = cancel.cancelled() => {
                    {
                        let _durable_mutation = self.inner.durable_mutation.lock();
                        REGISTRY.unregister_active_delivery(&self.inner.group_id, &agent_id);
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
            }
        };

        {
            let _durable_mutation = self.inner.durable_mutation.lock();
            REGISTRY.unregister_active_delivery(&self.inner.group_id, &agent_id);
        }
        drop(local_permit);

        let status = if cancel.is_cancelled() {
            AgentStatus::Aborted
        } else {
            AgentStatus::Idle
        };
        let (output, error, usage) = match outcome {
            Ok(result) => (result.text, result.error_message, result.usage),
            Err(error) => (
                child.last_assistant_text(),
                Some(error),
                pi_ai::Usage::default(),
            ),
        };
        let artifact_body = if output.is_empty() {
            error.as_deref().unwrap_or("(no output)").to_owned()
        } else {
            output.clone()
        };
        let artifact_error = write_new_artifact(&artifact_path, artifact_body.as_bytes())
            .with_context(|| format!("writing subagent artifact {}", artifact_path.display()))
            .err();
        let artifact_written = artifact_error.is_none();
        let history_error = serde_json::to_vec_pretty(&child.history())
            .context("serializing subagent history")
            .and_then(|bytes| {
                write_new_artifact(&history_path, &bytes)
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
        TaskResult {
            index: 0,
            id: agent_id,
            agent: agent_type,
            status,
            output,
            error: final_error,
            usage,
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
        let _durable_mutation = self.inner.durable_mutation.lock();
        self.ensure_recovery_idle()?;
        let durable = self
            .inner
            .durable
            .lock()
            .clone()
            .ok_or_else(|| anyhow!("orchestration runtime is not bound to a durable parent session"))?;
        let state = durable.load()?;
        self.install_recovered_state_locked(&durable, state)
    }

    /// Recover existing state before any write, or initialize a new sidecar
    /// when the bound parent has no durable orchestration state yet.
    pub fn recover_or_initialize(&self) -> Result<()> {
        let _spawn_guard = self.inner.jobs.lock_spawns();
        if self.inner.rebind_reserved.load(Ordering::Acquire) {
            bail!("durable orchestration rebind is in progress");
        }
        let _durable_mutation = self.inner.durable_mutation.lock();
        self.ensure_recovery_idle()?;
        let durable = self
            .inner
            .durable
            .lock()
            .clone()
            .ok_or_else(|| anyhow!("orchestration runtime is not bound to a durable parent session"))?;
        match durable.load_optional()? {
            Some(state) => self.install_recovered_state_locked(&durable, state),
            None => self.persist_state(),
        }
    }
    fn prepared_group_entries(
        &self,
        mut children: HashMap<String, Arc<AgentEntry>>,
    ) -> HashMap<String, Arc<AgentEntry>> {
        if let Some(main) = REGISTRY.get(&self.inner.group_id, self.main_agent_id()) {
            children.insert(self.main_agent_id().to_owned(), main);
        }
        children
    }


    fn prepare_recovered_state(
        &self,
        durable: &super::persistence::DurableRuntime,
        state: super::persistence::DurableState,
    ) -> Result<PreparedRecoveredState> {
        let now = now_millis();
        let mut entries = HashMap::with_capacity(state.agents.len());
        let mut agents = Vec::with_capacity(state.agents.len());
        for agent in state.agents {
            let session_path = agent
                .session_path
                .as_deref()
                .map(|path| durable.canonicalize_child_session_path(Path::new(path)))
                .transpose()?;
            let snapshot = AgentSnapshot {
                status: super::persistence::recovery_status(agent.snapshot.status),
                last_activity: now,
                ..agent.snapshot
            };
            let definition = agent.definition;
            let request = agent.request;
            let mailbox = agent.mailbox;
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
        let job_snapshots = state
            .jobs
            .into_iter()
            .map(|job| super::persistence::recovery_job(job, now))
            .collect::<Vec<_>>();
        let jobs = self.inner.jobs.prepare_replacement(job_snapshots);
        Ok(PreparedRecoveredState {
            group_entries: self.prepared_group_entries(entries),
            agents,
            jobs,
        })
    }
    fn install_prepared_state(&self, state: PreparedRecoveredState) {
        REGISTRY.install_prepared_group(&self.inner.group_id, state.group_entries);
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
        let recovered = self.prepare_recovered_state(durable, state)?;
        durable.persist(&super::persistence::build_state(
            durable.parent_session_id(),
            durable.parent_session_path(),
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
        if self.inner.jobs.snapshots(None).iter().any(|job| !job.status.is_settled()) {
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
        if self.inner.jobs.snapshots(None).iter().any(|job| !job.status.is_settled()) {
            bail!("cannot change global concurrency gate while orchestration jobs are active");
        }
        let mut current = self.inner.global_concurrency.lock();
        if current.as_ref().is_some_and(|configured| configured != &gate) {
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

    fn publish_job(&self, job: JobSnapshot) {
        let _ = self.inner.events.send(OrchestrationEvent::JobUpdated {
            group_id: self.inner.group_id.clone(),
            job: presentation_job_snapshot(job),
        });
    }

    fn publish_agent(&self, agent: AgentSnapshot) {
        let _ = self.inner.events.send(OrchestrationEvent::AgentUpdated {
            group_id: self.inner.group_id.clone(),
            agent: presentation_agent_snapshot(agent),
        });
    }

    fn publish_message_delivered(&self, message: MailboxMessage) {
        let _ = self.inner.events.send(OrchestrationEvent::MessageDelivered {
            group_id: self.inner.group_id.clone(),
            message,
        });
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
        REGISTRY
            .get(&self.inner.group_id, id)
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

    #[must_use]
    pub fn list(&self, caller_id: &str) -> Vec<AgentSnapshot> {
        REGISTRY.list(&self.inner.group_id, caller_id)
    }

    pub fn send(
        &self,
        from: &str,
        to: &str,
        body: &str,
        reply_to: Option<String>,
    ) -> Vec<DeliveryReceipt> {
        if self.inner.rebind_reserved.load(Ordering::Acquire) {
            return vec![DeliveryReceipt {
                to: to.to_owned(),
                outcome: DeliveryOutcome::Failed,
                requested: None,
                error: Some("durable orchestration rebind is in progress".to_owned()),
            }];
        }
        let (targets, requested_alias) = if to == "all" {
            (
                REGISTRY
                    .list(&self.inner.group_id, from)
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
                Err(error) if error.to_string().starts_with("ambiguous orchestration job or agent id") => {
                    return vec![DeliveryReceipt {
                        to: to.to_owned(),
                        outcome: DeliveryOutcome::Failed,
                        requested: None,
                        error: Some(error.to_string()),
                    }];
                }
                Err(_) => (vec![to.to_owned()], None),
            }
        };
        let main_id = self.main_agent_id().to_owned();
        let group_id = self.inner.group_id.clone();
        let mut receipts = Vec::with_capacity(targets.len());
        for target in targets {
            let message = MailboxMessage {
                id: Uuid::now_v7().to_string(),
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
                    error: Some("durable orchestration rebind is in progress".to_owned()),
                });
                continue;
            }
            match REGISTRY.enqueue(&group_id, &target, message.clone()) {
                Ok(outcome) => match self
                    .persist_state()
                    .with_context(|| format!("persisting message to orchestration agent {target:?}"))
                {
                    Err(error) => {
                        REGISTRY.remove_message(&group_id, &target, &message.id);
                        receipts.push(DeliveryReceipt {
                            to: target,
                            outcome: DeliveryOutcome::Failed,
                            requested: requested_alias.clone(),
                            error: Some(error.to_string()),
                        });
                    }
                    Ok(()) => {
                        if matches!(outcome, DeliveryOutcome::Woken) {
                            REGISTRY.deliver_committed(&group_id, &target, &message.id);
                        }
                        match if matches!(outcome, DeliveryOutcome::Queued) {
                            self.maybe_revive(&target)
                        } else {
                            Ok(None)
                        } {
                            Ok(revival) => {
                                let outcome = revival.unwrap_or(outcome);
                                if target == main_id {
                                    self.publish_message_delivered(message);
                                }
                                receipts.push(DeliveryReceipt {
                                    to: target,
                                    outcome,
                                    requested: requested_alias.clone(),
                                    error: None,
                                });
                            }
                            Err(error) => {
                                // The mailbox enqueue was durably committed before the
                                // revival claim. Keep the queued message in memory/disk.
                                receipts.push(DeliveryReceipt {
                                    to: target,
                                    outcome: DeliveryOutcome::Failed,
                                    requested: requested_alias.clone(),
                                    error: Some(error.to_string()),
                                });
                            }
                        }
                    }
                },
                Err(error) => receipts.push(DeliveryReceipt {
                    to: target,
                    outcome: DeliveryOutcome::Failed,
                    requested: requested_alias.clone(),
                    error: Some(error.to_string()),
                }),
            }
        }
        receipts
    }

    pub fn inbox_result(&self, agent_id: &str, peek: bool) -> Result<Vec<MailboxMessage>> {
        if peek {
            if self.inner.rebind_reserved.load(Ordering::Acquire) {
                bail!("durable orchestration rebind is in progress");
            }
            return Ok(REGISTRY.inbox(&self.inner.group_id, agent_id, true));
        }
        let _durable_mutation = self.inner.durable_mutation.lock();
        if self.inner.rebind_reserved.load(Ordering::Acquire) {
            bail!("durable orchestration rebind is in progress");
        }
        let messages = REGISTRY.inbox(&self.inner.group_id, agent_id, false);
        if messages.is_empty() {
            return Ok(messages);
        }
        if let Err(error) = self.persist_state().context("persisting orchestration inbox drain") {
            REGISTRY.restore_mailbox(&self.inner.group_id, agent_id, messages);
            return Err(error);
        }
        Ok(messages)
    }
    fn acknowledge_delivery(&self, agent_id: &str, message_id: &str) {
        let _durable_mutation = self.inner.durable_mutation.lock();
        let Some(message) = REGISTRY.remove_message_value(
            &self.inner.group_id,
            agent_id,
            message_id,
        ) else {
            return;
        };
        if self.persist_state().is_err() {
            REGISTRY.restore_mailbox(&self.inner.group_id, agent_id, vec![message]);
        }
    }

    #[must_use]
    pub fn inbox(&self, agent_id: &str, peek: bool) -> Vec<MailboxMessage> {
        self.inbox_result(agent_id, peek).unwrap_or_default()
    }

    pub async fn wait_message(
        &self,
        agent_id: &str,
        from: Option<&str>,
        timeout: Option<Duration>,
        abort: Option<pi_agent::AbortSignal>,
    ) -> Result<Option<MailboxMessage>> {
        if self.inner.rebind_reserved.load(Ordering::Acquire) {
            bail!("durable orchestration rebind is in progress");
        }
        let from = from.map(|identifier| {
            self.inner
                .jobs
                .resolve_agent_id(identifier)
                .unwrap_or_else(|_| identifier.to_owned())
        });
        let from = from.as_deref();
        let entry = REGISTRY
            .get(&self.inner.group_id, agent_id)
            .ok_or_else(|| anyhow!("unknown orchestration agent {agent_id:?}"))?;
        // Register an explicit waiter before the loop checks the mailbox so a
        // concurrent `deliver_committed` defers matching (and wildcard) sends
        // to this waiter instead of the active steering bridge. The guard is
        // RAII: it unregisters on every return path — return, timeout, abort,
        // shutdown, or task drop — so no stale claim can strand a message.
        let _waiter_guard = REGISTRY
            .register_message_waiter(&self.inner.group_id, agent_id, from.map(str::to_owned))
            .ok_or_else(|| anyhow!("unknown orchestration agent {agent_id:?}"))?;
        let wait = async {
            loop {
                let notified = entry.message_ready.notified();
                let message = {
                    let _durable_mutation = self.inner.durable_mutation.lock();
                    if self.inner.rebind_reserved.load(Ordering::Acquire) {
                        return Err(anyhow!("durable orchestration rebind is in progress"));
                    }
                    match take_matching_message(&entry, from) {
                        Some(message) => {
                            if let Err(error) = self
                                .persist_state()
                                .context("persisting waited orchestration message")
                            {
                                REGISTRY.restore_mailbox(
                                    &self.inner.group_id,
                                    agent_id,
                                    vec![message],
                                );
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
                notified.await;
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
        REGISTRY.set_status(&self.inner.group_id, id, AgentStatus::Parked)?;
        if let Err(error) = self.persist_state().context("persisting parked child") {
            let _ = REGISTRY.compare_status(
                &self.inner.group_id,
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
        let previous_artifact = REGISTRY
            .get(&self.inner.group_id, id)
            .and_then(|entry| entry.artifact_path.lock().clone());
        let previous_history = REGISTRY
            .get(&self.inner.group_id, id)
            .and_then(|entry| entry.history_path.lock().clone());
        REGISTRY.finish(
            &self.inner.group_id,
            id,
            status,
            artifact_path,
            history_path,
        )?;
        if let Err(error) = self.persist_state().context("persisting finished child") {
            REGISTRY.restore_finish(
                &self.inner.group_id,
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
        let Some(entry) = REGISTRY.get(&self.inner.group_id, id) else {
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
                    if let Some(agent) = REGISTRY.park_if_idle(
                        &runtime.inner.group_id,
                        &id_owned,
                        &runtime.inner.config.main_agent_id,
                    ) {
                        runtime.publish_agent(agent);
                        if runtime.persist_state().is_err() {
                            let _ = REGISTRY.compare_status(
                                &runtime.inner.group_id,
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
        if let Some(entry) = REGISTRY.get(&self.inner.group_id, id)
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
        let _spawn_guard = self.inner.jobs.lock_spawns();
        let _durable_mutation = self.inner.durable_mutation.lock();
        if self.inner.rebind_reserved.load(Ordering::Acquire) {
            bail!("durable orchestration rebind is in progress");
        }
        let mut reserved = std::collections::BTreeSet::new();
        let mut prepared = Vec::with_capacity(items.len());
        let available_models = crate::available_models();
        for mut item in items {
            item.id = self.allocate_unique_agent_id(&item.id, &mut reserved)?;
            let definition = self
                .inner
                .config
                .catalog
                .get(&item.agent)
                .cloned()
                .ok_or_else(|| anyhow!("unknown agent definition {:?}", item.agent))?;
            if !definition.trusted {
                bail!("agent definition {:?} is not trusted", item.agent);
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
                return Err(crate::agent_unsupported_tools_error(
                    &item.agent,
                    &unsupported,
                ));
            }
            if crate::effective_agent_tool_names(&definition, agent_settings).is_some_and(|tools| {
                tools
                    .iter()
                    .filter(|name| {
                        !matches!(name.as_str(), "todo" | "process" | "task" | "hub" | "goal")
                    })
                    .count()
                    > self.inner.config.max_tools_per_agent
            }) {
                bail!(
                    "agent definition {:?} requests more than {} tools",
                    item.agent,
                    self.inner.config.max_tools_per_agent
                );
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
                description: Some(one_line(&item.assignment)),
                todo_task_id: item.todo_task_id.clone(),
                workflow_id: workflow_scope
                    .as_ref()
                    .map(|scope| scope.workflow_id.clone()),
                workflow_generation: workflow_scope
                    .as_ref()
                    .map(|scope| scope.generation),
                status: JobStatus::Queued,
                created_at,
                started_at: None,
                finished_at: None,
                result: None,
            };
            launches.push((item, definition, resolved_model, cancel, agent_snapshot, job_snapshot));
        }

        REGISTRY.register_batch(
            &self.inner.group_id,
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
            .map(|(item, definition, resolved_model, cancel, agent_snapshot, job_snapshot)| {
                let peer_roster = self.sibling_roster(&item.id);
                let request = ChildSessionRequest {
                    child_id: item.id.clone(),
                    parent_id: parent_id.to_owned(),
                    depth: parent_depth + 1,
                    system_prompt: self.child_system_prompt(
                        &definition,
                        &item.assignment,
                        &peer_roster,
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
                    definition,
                    assignment: item.assignment.clone(),
                };
                Ok((item, request, cancel, agent_snapshot, job_snapshot))
            })
            .collect::<Result<Vec<_>>>()
        {
            Ok(launches) => launches,
            Err(error) => {
                REGISTRY.unregister_batch(&self.inner.group_id, &registered_ids);
                return Err(error);
            }
        };
        let mut spawns = Vec::with_capacity(launches.len());
        for (item, _, cancel, agent_snapshot, job_snapshot) in &launches {
            let job_snapshot = match self.inner.jobs.insert(job_snapshot.clone(), cancel.clone()) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    REGISTRY.unregister_batch(&self.inner.group_id, &registered_ids);
                    return Err(error);
                }
            };
            self.publish_agent(agent_snapshot.clone());
            self.publish_job(job_snapshot.clone());
            self.inner.active.lock().insert(item.id.clone(), cancel.clone());
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
                if let Some(entry) = REGISTRY.get(&self.inner.group_id, &item.id) {
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
            REGISTRY.unregister_batch(&self.inner.group_id, &registered_ids);
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
                        && let Some(job) = runtime
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
            group_id: self.inner.group_id.clone(),
        };
        let artifact_ref = format!("agent://{}", item.id);
        let history_ref = format!("history://{}", item.id);
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
        REGISTRY
            .set_status(&self.inner.group_id, &item.id, AgentStatus::Running)
            .ok();
        let running_agent = self.agent_snapshot(&item.id);
        let running_job = self.inner.jobs.mark_running(job_id, now_millis());
        if let Err(error) = self.persist_state() {
            let error = format!("persisting child running state: {error:#}");
            drop(durable_mutation);
            return self.failed_result(
                &item,
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
        let child_session = tokio::select! {
            result = (self.inner.factory)(request) => match result {
                Ok(session) => session,
                Err(error) => return self.failed_result(&item, job_id, AgentStatus::Idle, &error.to_string()),
            },
            () = cancel.cancelled() => return self.failed_result(&item, job_id, AgentStatus::Aborted, "task cancelled during child session creation"),
        };
        if self.inner.durable_bound.load(Ordering::Acquire) {
            let durable = match self.inner.durable.lock().clone() {
                Some(durable) => durable,
                None => return self.failed_result(&item, job_id, AgentStatus::Idle, "durable orchestration binding is unavailable"),
            };
            if let Err(error) = child_session.start_durable_child_recording(
                durable.child_root(),
                durable.parent_session_path(),
            ) {
                return self.failed_result(
                    &item,
                    job_id,
                    AgentStatus::Idle,
                    &format!("starting durable child recording: {error:#}"),
                );
            }
            let Some((_, session_path)) = child_session.recorder_info() else {
                return self.failed_result(&item, job_id, AgentStatus::Idle, "durable child recorder is unavailable");
            };
            let session_path = match durable.canonicalize_child_session_path(&session_path) {
                Ok(path) => path,
                Err(error) => return self.failed_result(&item, job_id, AgentStatus::Idle, &error.to_string()),
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
            if let Some(entry) = REGISTRY.get(&self.inner.group_id, &item.id)
                && let Some(info) = entry.durable_info.lock().as_mut()
            {
                info.session_path = Some(session_path);
            }
            if let Err(error) = self.persist_state() {
                let error = format!("persisting durable child transcript path: {error:#}");
                drop(durable_mutation);
                return self.failed_result(
                    &item,
                    job_id,
                    AgentStatus::Idle,
                    &error,
                );
            }
            drop(durable_mutation);
        }
        let child = ChildSession::new(child_session);
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
            match REGISTRY.register_active_delivery(
                &self.inner.group_id,
                &item.id,
                delivery_tx,
                cancel.clone(),
            ) {
                Ok(messages) => messages,
                Err(error) => {
                    drop(durable_mutation);
                    return self.failed_result(&item, job_id, AgentStatus::Idle, &error.to_string());
                }
            }
        };
        for message in &pre_run {
            child.steer(message).await;
            self.acknowledge_delivery(&item.id, &message.id);
        }
        let mut run = Box::pin(child.run(&item.assignment));
        let outcome = loop {
            tokio::select! {
                result = &mut run => break result.map_err(|error| error.to_string()),
                message = delivery_rx.recv() => {
                    match message {
                        Some(message) => {
                            child.steer(&message).await;
                            self.acknowledge_delivery(&item.id, &message.id);
                        }
                        None => break Err("active child delivery bridge closed".to_owned()),
                    }
                }
                () = cancel.cancelled() => {
                    {
                        let _durable_mutation = self.inner.durable_mutation.lock();
                        REGISTRY.unregister_active_delivery(&self.inner.group_id, &item.id);
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
            }
        };
        {
            let _durable_mutation = self.inner.durable_mutation.lock();
            REGISTRY.unregister_active_delivery(&self.inner.group_id, &item.id);
        }
        drop(global_permit);
        drop(local_permit);
        let status = if cancel.is_cancelled() {
            AgentStatus::Aborted
        } else {
            AgentStatus::Idle
        };
        let (output, error, usage) = match outcome {
            Ok(result) => (result.text, result.error_message, result.usage),
            Err(error) => (
                child.last_assistant_text(),
                Some(error),
                pi_ai::Usage::default(),
            ),
        };
        let artifact_body = if output.is_empty() {
            error.as_deref().unwrap_or("(no output)").to_owned()
        } else {
            output.clone()
        };
        let artifact_error = write_new_artifact(&artifact_path, artifact_body.as_bytes())
            .with_context(|| format!("writing subagent artifact {}", artifact_path.display()))
            .err();
        let artifact_written = artifact_error.is_none();
        let history_error = serde_json::to_vec_pretty(&child.history())
            .context("serializing subagent history")
            .and_then(|bytes| {
                write_new_artifact(&history_path, &bytes)
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
        TaskResult {
            index: item.index,
            id: item.id,
            agent: item.agent,
            status,
            output,
            error: final_error,
            usage,
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
        let history_ref = format!("history://{}", item.id);
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
        let artifact_error = write_new_artifact(&artifact_path, error.as_bytes()).err();
        let artifact_written = artifact_error.is_none();
        let history_error = write_new_artifact(&history_path, b"[]").err();
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
            artifact_ref,
            history_ref,
            artifact_uri,
        }
    }

    fn child_system_prompt(
        &self,
        definition: &AgentDefinition,
        assignment: &str,
        peer_roster: &str,
    ) -> Result<String> {
        let mut prompt = definition.system_prompt.clone();
        let mut autoload_bytes = 0_usize;
        let visible_skills = self
            .inner
            .config
            .skills
            .iter()
            .filter(|skill| !skill.hidden && !skill.disable_model_invocation)
            .collect::<Vec<_>>();
        if !visible_skills.is_empty() {
            prompt.push_str("\n\n<available_skills>\n");
            for skill in visible_skills {
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
        let mut seen_autoload = std::collections::BTreeSet::new();
        for name in definition.autoload_skills.iter().chain(&plan.autoload_skills) {
            if seen_autoload.insert(name.as_str()) {
                autoload_names.push(name);
            }
        }
        for name in autoload_names {
            let skill = self
                .inner
                .config
                .skills
                .iter()
                .find(|skill| skill.name == *name)
                .ok_or_else(|| anyhow!("autoload skill {name:?} is unavailable"))?;
            let size = fs::metadata(&skill.file_path)
                .with_context(|| format!("reading autoload skill metadata {}", skill.file_path.display()))?
                .len();
            if size > MAX_AUTOLOAD_SKILL_BYTES {
                bail!(
                    "autoload skill {} exceeds maximum size of {} bytes",
                    skill.file_path.display(),
                    MAX_AUTOLOAD_SKILL_BYTES
                );
            }
            let size = usize::try_from(size).unwrap_or(usize::MAX);
            autoload_bytes = autoload_bytes.saturating_add(size);
            if autoload_bytes > MAX_AUTOLOAD_PROMPT_BYTES {
                bail!("autoload skills exceed aggregate prompt budget of {MAX_AUTOLOAD_PROMPT_BYTES} bytes");
            }
            let mut content = String::with_capacity(size);
            File::open(&skill.file_path)
                .with_context(|| format!("opening autoload skill {}", skill.file_path.display()))?
                .take(MAX_AUTOLOAD_SKILL_BYTES + 1)
                .read_to_string(&mut content)
                .with_context(|| format!("reading autoload skill {}", skill.file_path.display()))?;
            if content.len() as u64 > MAX_AUTOLOAD_SKILL_BYTES {
                bail!("autoload skill {} grew beyond maximum size while reading", skill.file_path.display());
            }
            prompt.push_str(&format!(
                "\n\n<autoloaded_skill name=\"{}\" location=\"skill://{}\">\n{}\n</autoloaded_skill>",
                escape_xml(&skill.name),
                escape_xml(&skill.name),
                content
            ));
        }
        prompt.push_str("\n\n");
        prompt.push_str(peer_roster);
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
        let mut peers = REGISTRY.live_roster(&self.inner.group_id, child_id, main_id);
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

    /// Resolve the agent for a task assignment.
    ///
    /// Precedence: explicit `task.agent` override, then unique exact trusted
    /// agent-name mention (including disabled/untrusted rejection), then ranked
    /// metadata selection / default. Ambiguous exact mentions return an error.
    pub fn resolve_task_agent(&self, assignment: &str, explicit: Option<&str>) -> Result<String> {
        if let Some(explicit) = explicit.filter(|name| !name.trim().is_empty()) {
            self.ensure_agent_enabled(explicit)?;
            return Ok(explicit.to_owned());
        }
        match self.exact_agent_mention_in_catalog(assignment) {
            crate::selector::ExactAgentMention::Unique(name) => {
                self.ensure_agent_enabled(&name)?;
                if !self
                    .enabled_agents()
                    .iter()
                    .any(|agent| agent.name == name)
                {
                    bail!(
                        "exact agent mention {:?} is not available for spawning",
                        name
                    );
                }
                Ok(name)
            }
            crate::selector::ExactAgentMention::Ambiguous(_) => Err(anyhow!(
                "{}",
                self.exact_agent_ambiguity(assignment).unwrap_or_else(|| {
                    "exact agent mention is ambiguous".to_owned()
                })
            )),
            crate::selector::ExactAgentMention::None => {
                Ok(self.select_ranked_or_default_agent(assignment))
            }
        }
    }

    /// Spawn a child when the request carries a delegation verb and a unique
    /// exact trusted agent name. Generic skill/semantic text returns `Ok(None)`
    /// so the caller can keep selection recommendations without spawning.
    pub fn spawn_from_natural_language(
        &self,
        parent_id: &str,
        parent_depth: usize,
        request: &str,
    ) -> Result<Option<Vec<TaskSpawn>>> {
        if !request_has_delegation_verb(request) {
            return Ok(None);
        }
        match self.exact_agent_mention_in_catalog(request) {
            crate::selector::ExactAgentMention::None => Ok(None),
            crate::selector::ExactAgentMention::Ambiguous(_) => {
                bail!(
                    "{}",
                    self.exact_agent_ambiguity(request).unwrap_or_else(|| {
                        "exact agent mention is ambiguous".to_owned()
                    })
                );
            }
            crate::selector::ExactAgentMention::Unique(name) => {
                self.ensure_agent_enabled(&name)?;
                if !self
                    .enabled_agents()
                    .iter()
                    .any(|agent| agent.name == name)
                {
                    bail!(
                        "exact agent mention {:?} is not available for spawning",
                        name
                    );
                }
                let spawns = self.spawn_tasks(
                    parent_id,
                    parent_depth,
                    vec![TaskItem {
                        index: 0,
                        id: name.clone(),
                        agent: name,
                        assignment: request.to_owned(),
                        todo_task_id: None,
                    }],
                )?;
                Ok(Some(spawns))
            }
        }
    }

    fn select_ranked_or_default_agent(&self, assignment: &str) -> String {
        let enabled_owned = self
            .enabled_agents()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
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
                trusted: skill.trusted,
            })
            .collect::<Vec<_>>();
        crate::selector::exact_skill_mention(assignment, &skills)
    }

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

    pub(crate) fn exact_agent_ambiguity(&self, assignment: &str) -> Option<String> {
        self.exact_agent_mention_in_catalog(assignment)
            .ambiguity_message()
    }

    pub fn read_uri_resolver(&self) -> crate::InternalUriResolverFn {
        let group_id = self.inner.group_id.clone();
        let artifact_dir = self.inner.config.artifact_dir.clone();
        Arc::new(move |uri| resolve_read_uri_in(&group_id, &artifact_dir, uri))
    }


    pub fn resolve_read_uri(&self, uri: &str) -> Result<PathBuf> {
        resolve_read_uri_in(&self.inner.group_id, &self.inner.config.artifact_dir, uri)
    }

    pub fn resolve_agent_reference(&self, id: &str) -> Result<PathBuf> {
        resolve_registered_artifact(
            &self.inner.group_id,
            &self.inner.config.artifact_dir,
            id,
            false,
        )
    }

    pub fn resolve_history_reference(&self, id: &str) -> Result<PathBuf> {
        resolve_registered_artifact(
            &self.inner.group_id,
            &self.inner.config.artifact_dir,
            id,
            true,
        )
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

    pub fn cancel_jobs_result(&self, ids: &[String]) -> Result<Vec<String>> {
        let _durable_mutation = self.inner.durable_mutation.lock();
        if self.inner.rebind_reserved.load(Ordering::Acquire) {
            bail!("durable orchestration rebind is in progress");
        }
        if self.inner.durable_bound.load(Ordering::Acquire) {
            let durable = self
                .inner
                .durable
                .lock()
                .clone()
                .ok_or_else(|| anyhow!("durable orchestration binding is unavailable"))?;
            let mut prepared = self.inner.jobs.prepare_cancellation(ids);
            let jobs = prepared.take_snapshots();
            durable.persist(&super::persistence::build_state(
                durable.parent_session_id(),
                durable.parent_session_path(),
                self.collect_persisted_agents(),
                jobs,
            ))?;
            return Ok(self.inner.jobs.commit_cancellation(prepared));
        }
        Ok(self.inner.jobs.cancel(ids))
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

    pub async fn shutdown(&self) {
        self.inner.shutdown.cancel();
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
        loop {
            let notified = self.inner.active_changed.notified();
            if self.inner.active.lock().is_empty() {
                break;
            }
            notified.await;
        }
        // Shutdown has no synchronous command caller after all children drain;
        // preserve the last successfully persisted sidecar if this final sync fails.
        let _shutdown_persist_error = self.persist_state().err();
        self.cleanup_retained_jobs();
        REGISTRY.remove_group(&self.inner.group_id);
    }
}

struct ActiveChildGuard {
    inner: Arc<RuntimeInner>,
    id: String,
    group_id: String,
}

impl Drop for ActiveChildGuard {
    fn drop(&mut self) {
        let _durable_mutation = self.inner.durable_mutation.lock();
        REGISTRY.unregister_active_delivery(&self.group_id, &self.id);
        self.inner.active.lock().remove(&self.id);
        self.inner.active_changed.notify_waiters();
    }
}

impl Drop for OrchestrationRuntime {
    fn drop(&mut self) {
        if !self.owner {
            return;
        }
        self.inner.shutdown.cancel();
        for token in self.inner.active.lock().values() {
            token.cancel();
        }
        for handle in self.inner.park_timers.lock().drain().map(|(_, handle)| handle) {
            handle.abort();
        }
        self.cleanup_retained_jobs();
        REGISTRY.remove_group(&self.inner.group_id);
    }
}

impl Drop for RuntimeInner {
    fn drop(&mut self) {
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
            let history_path = self
                .config
                .artifact_dir
                .join(format!("{}-{}.history.json", candidate.agent_id, candidate.job_id));
            let _ = remove_retained_file(&artifact_path);
            let _ = remove_retained_file(&history_path);
        }
        REGISTRY.remove_group(&self.group_id);
    }
}

impl GlobalRegistry {
    fn register(&self, group: &str, snapshot: AgentSnapshot, mailbox_capacity: usize) -> Result<()> {
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
                bail!("orchestration agent id {:?} appears more than once in batch", snapshot.id);
            }
        }

        let mut groups = self.groups.lock();
        if let Some(entries) = groups.get(group)
            && let Some(snapshot) = snapshots.iter().find(|snapshot| entries.contains_key(&snapshot.id))
        {
            bail!("orchestration agent id {:?} is already registered", snapshot.id);
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

    fn install_prepared_group(
        &self,
        group: &str,
        prepared: HashMap<String, Arc<AgentEntry>>,
    ) {
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
    fn enqueue(&self, group: &str, target: &str, message: MailboxMessage) -> Result<DeliveryOutcome> {
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
                AgentStatus::Queued | AgentStatus::Parked => {
                    let mut mailbox = entry.mailbox.lock();
                    if mailbox.len() >= entry.mailbox_capacity {
                        bail!("orchestration mailbox for {target:?} is full");
                    }
                    mailbox.push_back(message);
                    DeliveryOutcome::Queued
                }
                AgentStatus::Idle => {
                    let mut mailbox = entry.mailbox.lock();
                    if mailbox.len() >= entry.mailbox_capacity {
                        bail!("orchestration mailbox for {target:?} is full");
                    }
                    mailbox.push_back(message);
                    DeliveryOutcome::Woken
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

    fn deliver_committed(&self, group: &str, target: &str, message_id: &str) -> bool {
        let Some(entry) = self.get(group, target) else {
            return false;
        };
        let message = entry
            .mailbox
            .lock()
            .iter()
            .find(|message| message.id == message_id)
            .cloned();
        let Some(message) = message else {
            return false;
        };
        // An explicit `hub wait` registered for this message takes precedence
        // over the active steering bridge: skip delivery so `wait_message` can
        // drain the mailbox item itself. A wildcard waiter (`from == None`)
        // claims every message; otherwise the sender must match exactly.
        let claimed = entry
            .waiters
            .lock()
            .iter()
            .any(|waiter| waiter.from.is_none() || waiter.from.as_deref() == Some(&message.from));
        if claimed {
            return false;
        }
        entry
            .active_delivery
            .lock()
            .as_ref()
            .is_some_and(|sender| sender.send(message).is_ok())
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
    fn register_message_waiter(
        &self,
        group: &str,
        id: &str,
        from: Option<String>,
    ) -> Option<MessageWaiterGuard> {
        let entry = self.get(group, id)?;
        let token = entry.waiter_seq.fetch_add(1, Ordering::Relaxed) + 1;
        entry.waiters.lock().push(MessageWaiter { token, from });
        Some(MessageWaiterGuard { entry, token })
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
        let index = mailbox.iter().position(|message| message.id == message_id)?;
        mailbox.remove(index)
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
            snapshot.history_ref = Some(format!("history://{id}"));
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

fn take_matching_message(entry: &AgentEntry, from: Option<&str>) -> Option<MailboxMessage> {
    let mut mailbox = entry.mailbox.lock();
    let index = mailbox
        .iter()
        .position(|message| from.is_none_or(|from| message.from == from))?;
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
pub fn orchestration_message_view(message: &pi_ai::CustomMessage) -> Option<OrchestrationMessageView> {
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

fn extract_orchestration_body_from_content(content: &pi_ai::CustomMessageContent) -> Option<String> {
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
fn remove_retained_file(path: &Path) -> bool {
    match fs::remove_file(path) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
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

fn resolve_read_uri_in(group_id: &str, artifact_dir: &Path, uri: &str) -> Result<PathBuf> {
    let (id, history) = if let Some(id) = uri.strip_prefix("agent://") {
        (id, false)
    } else if let Some(id) = uri.strip_prefix("history://") {
        (id, true)
    } else if let Some(id) = uri.strip_prefix("artifact://") {
        (id, false)
    } else {
        bail!("unsupported orchestration URI {uri:?}");
    };
    resolve_registered_artifact(group_id, artifact_dir, id, history)
}

fn resolve_registered_artifact(
    group_id: &str,
    artifact_dir: &Path,
    id: &str,
    history: bool,
) -> Result<PathBuf> {
    validate_agent_id(id)?;
    let entry = REGISTRY
        .get(group_id, id)
        .ok_or_else(|| anyhow!("unknown orchestration agent {id:?}"))?;
    let path = if history {
        entry.history_path.lock().clone()
    } else {
        entry.artifact_path.lock().clone()
    }
    .ok_or_else(|| anyhow!("orchestration artifact for agent {id:?} is not available yet"))?;
    ensure_existing_artifact(artifact_dir, &path)
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

fn write_new_artifact(path: &Path, content: &[u8]) -> Result<()> {
    use std::io::Write as _;

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("creating artifact {}", path.display()))?;
    file.write_all(content)
        .with_context(|| format!("writing artifact {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("syncing artifact {}", path.display()))
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

fn ensure_existing_artifact(root: &Path, path: &Path) -> Result<PathBuf> {
    let canonical_root = root
        .canonicalize()
        .with_context(|| format!("resolving artifact directory {}", root.display()))?;
    let canonical = path
        .canonicalize()
        .with_context(|| format!("resolving artifact {}", path.display()))?;
    if !canonical.starts_with(canonical_root) {
        bail!("artifact path escapes orchestration artifact directory");
    }
    Ok(canonical)
}

fn presentation_job_snapshot(mut job: JobSnapshot) -> JobSnapshot {
    job.description = job
        .description
        .as_deref()
        .map(|description| presentation_text(description, 160));
    if let Some(result) = &mut job.result {
        result.output = presentation_text(&result.output, 600);
        result.error = result.error.as_deref().map(|error| presentation_text(error, 300));
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
    format!(
        "  <peer id=\"{id}\" agent=\"{agent}\" status=\"{status}\" parent=\"{parent}\" />\n"
    )
}

/// True when the request uses an explicit natural-language delegation verb.
/// Used to gate parent prompt-time auto-spawn; the task tool does not require it.
#[must_use]
fn request_has_delegation_verb(request: &str) -> bool {
    const VERBS: &[&str] = &[
        "have", "ask", "tell", "get", "let", "make", "please", "delegate", "assign", "spawn",
        "run", "send", "kick", "dispatch",
    ];
    let tokens = request
        .nfkc()
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .split(|ch: char| !ch.is_alphanumeric() && ch != '-' && ch != '_')
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    tokens
        .iter()
        .any(|token| VERBS.iter().any(|verb| token == verb))
}

fn one_line(value: &str) -> String {
    let collapsed = value.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(80).collect()
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

impl OrchestrationRuntime {
    pub(crate) fn generated_agent_id(&self, index: usize) -> String {
        let suffix = Uuid::now_v7().simple().to_string();
        format!("Agent{}-{}", index + 1, &suffix[..8])
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
                    suffix = suffix
                        .checked_add(1)
                        .ok_or_else(|| anyhow!("exhausted unique agent id suffixes for {base:?}"))?;
                    continue;
                }
                candidate
            };
            reserved.insert(candidate.clone());
            return Ok(candidate);
        }
    }

    fn agent_id_is_taken(
        &self,
        id: &str,
        reserved: &std::collections::BTreeSet<String>,
    ) -> bool {
        reserved.contains(id)
            || REGISTRY.get(&self.inner.group_id, id).is_some()
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

#[cfg(test)]
mod prompt_size_tests {
    use super::*;
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
        let config = OrchestrationConfig::new(
            AgentCatalog::from_agents(vec![definition]),
            root,
        )
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
        REGISTRY
            .register(
                &runtime.inner.group_id,
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
    fn child_prompt_includes_stable_live_peer_roster_and_excludes_self() {
        let root = tempfile::tempdir().expect("root");
        let runtime = prompt_runtime(root.path());
        register_peer(&runtime, "Zulu", "reviewer<&\"", Some("Main"), AgentStatus::Running);
        register_peer(
            &runtime,
            "peer_token_abcdefgh",
            "security",
            Some("Main"),
            AgentStatus::Running,
        );
        register_peer(&runtime, "Alpha", "researcher", Some("Parent"), AgentStatus::Queued);
        register_peer(&runtime, "Pending", "task", Some("Main"), AgentStatus::Running);
        register_peer(&runtime, "Finished", "writer", Some("Main"), AgentStatus::Idle);
        register_peer(&runtime, "Retained", "reviewer", Some("Main"), AgentStatus::Parked);
        register_peer(&runtime, "Aborted", "task", Some("Main"), AgentStatus::Aborted);

        let definition = runtime.catalog().get("task").expect("agent");
        let roster = runtime.sibling_roster("Pending");
        let prompt = runtime
            .child_system_prompt(definition, "inspect assignment", &roster)
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
        let roster_end = prompt.find("</peer_roster>").expect("roster end")
            + "</peer_roster>".len();
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
        assert_eq!(roster.matches("  <peer ").count(), MAX_SIBLING_ROSTER_ENTRIES);
        assert!(roster.contains("<peer id=\"Main\""));
        assert!(roster.contains("  <truncated />"));
        let ids = roster
            .lines()
            .filter_map(|line| line.strip_prefix("  <peer id=\"")?.split_once('"').map(|(id, _)| id))
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
            .child_system_prompt(definition, "work", &roster)
            .expect("prompt");
        let roster_start = prompt.find("<peer_roster>").expect("roster start");
        let roster_end = prompt.find("</peer_roster>").expect("roster end")
            + "</peer_roster>".len();
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
        let error = REGISTRY
            .register_batch(
                &runtime.inner.group_id,
                vec![snapshot("WouldInsert", "Main"), snapshot("InvalidParent", "Main\n")],
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
        let error = REGISTRY
            .register(
                &runtime.inner.group_id,
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
            "---\nname: task\ndescription: task\n---\nprompt",
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn batch_children_each_receive_all_registered_siblings() {
        let root = tempfile::tempdir().expect("root");
        let captured = Arc::new(Mutex::new(BTreeMap::<String, String>::new()));
        let factory_capture = captured.clone();
        let definition = super::super::definitions::parse_agent_definition(
            Path::new("task.md"),
            "---\nname: task\ndescription: task\n---\nprompt",
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
                    },
                    TaskItem {
                        index: 1,
                        id: "Second".to_owned(),
                        agent: "task".to_owned(),
                        assignment: "second".to_owned(),
                        todo_task_id: None,
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
        assert!(first.contains(
            "<peer id=\"Second\" agent=\"task\" status=\"queued\" parent=\"Main\""
        ));
        assert!(!first.contains("<peer id=\"First\""));
        assert!(first.contains("<peer id=\"Main\""));
        assert!(second.contains(
            "<peer id=\"First\" agent=\"task\" status=\"queued\" parent=\"Main\""
        ));
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
        }
    }

    fn insert_retained_job(
        runtime: &OrchestrationRuntime,
        id: &str,
        job_id: &str,
        timestamp: u64,
    ) {
        REGISTRY
            .register(
                &runtime.inner.group_id,
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
                },
                CancellationToken::new(),
            )
            .expect("insert");
        let result = retained_result(id, &runtime.inner.config.artifact_dir, job_id);
        runtime
            .finish_agent(
                id,
                AgentStatus::Idle,
                Some(runtime.inner.config.artifact_dir.join(format!("{id}-{job_id}.md"))),
                Some(runtime.inner.config.artifact_dir.join(format!("{id}-{job_id}.history.json"))),
            )
            .expect("finish agent");
        runtime.inner.jobs.finish(job_id, result, timestamp);
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
        REGISTRY
            .finish(
                &runtime.inner.group_id,
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
            },
            2,
        );
        let jobs = runtime.jobs(None);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, "job-new");
        assert!(!old_artifact.exists());
        assert_eq!(
            runtime.resolve_read_uri("artifact://Shared").expect("new alias"),
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
    async fn owner_shutdown_removes_retained_physical_files_and_aliases() {
        let root = tempfile::tempdir().expect("root");
        let now = Arc::new(AtomicU64::new(1));
        let runtime = retention_runtime(root.path(), now, 10, Duration::from_secs(60));
        insert_retained_job(&runtime, "Owned", "job-owned", 1);
        let resolver = runtime.read_uri_resolver();
        let artifact = resolver("artifact://Owned").expect("artifact");
        runtime.shutdown().await;
        assert!(!artifact.exists());
        assert!(resolver("artifact://Owned").is_err());
    }


    fn runtime_with_skill(path: PathBuf) -> OrchestrationRuntime {
        let definition = super::super::definitions::parse_agent_definition(
            Path::new("bounded.md"),
            "---\nname: bounded\ndescription: bounded\nautoloadSkills: [large]\n---\nprompt",
            super::super::AgentDefinitionSource::Bundled,
            true,
        )
        .expect("definition");
        let mut config = OrchestrationConfig::new(
            AgentCatalog::from_agents(vec![definition]),
            path.parent().expect("skill parent"),
        );
        config.default_agent = "bounded".to_owned();
        config.skills.push(OrchestrationSkill {
            name: "large".to_owned(),
            description: "large".to_owned(),
            file_path: path.clone(),
            base_dir: path.parent().expect("skill parent").to_path_buf(),
            globs: Vec::new(),
            always_apply: false,
            hidden: false,
            disable_model_invocation: false,
            source: crate::SkillSource::User,
            trusted: true,
        });
        OrchestrationRuntime::new(config, Arc::new(|_| Box::pin(async { unreachable!() })))
            .expect("runtime")
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
            .child_system_prompt(definition, "work", &roster)
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
            .child_system_prompt(definition, "work", &roster)
            .expect_err("oversized rejected")
            .to_string();
        assert!(error.contains("exceeds maximum size"));
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
        legacy.as_object_mut().expect("snapshot object").remove("agent");
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
        };
        let legacy_wire = serde_json::to_value(&legacy).expect("serialize legacy job");
        assert!(legacy_wire.get("workflowId").is_none());
        assert!(legacy_wire.get("workflowGeneration").is_none());

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
}
