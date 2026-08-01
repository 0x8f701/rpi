use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs::{self, File};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use parking_lot::Mutex;
use pi_agent::{AgentTool, BoxFuture, ThinkingLevel};
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, Semaphore, broadcast};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{Session, Skill};

use super::{
    AgentCatalog, AgentDefinition, JobClock, JobManager, JobRetention, JobSnapshot, JobStatus,
    TaskSpawn,
};

pub const DEFAULT_MAILBOX_CAPACITY: usize = 100;
pub const DEFAULT_MAX_CONCURRENCY: usize = 4;
pub const DEFAULT_MAX_RECURSION_DEPTH: usize = 2;
pub const DEFAULT_MAX_TOOLS_PER_AGENT: usize = 16;
pub const DEFAULT_IDLE_TTL_SECS: u64 = 300;
pub const DEFAULT_MAX_RETAINED_JOBS: usize = 256;
pub const DEFAULT_RETAINED_JOB_TTL_SECS: u64 = 24 * 60 * 60;
const MAX_AUTOLOAD_SKILL_BYTES: u64 = 256 * 1024;
const MAX_AUTOLOAD_PROMPT_BYTES: usize = 1024 * 1024;
const CHILD_ABORT_GRACE: Duration = Duration::from_secs(2);
pub type AgentSelectorFn = Arc<
    dyn Fn(&str, &[AgentDefinition]) -> Option<String> + Send + Sync,
>;
pub type ParentModelProvider = Arc<dyn Fn() -> pi_ai::Model + Send + Sync>;
pub type ChildSessionFactory =
    Arc<dyn Fn(ChildSessionRequest) -> BoxFuture<Result<Session>> + Send + Sync>;
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
            let ranked = crate::rank_agents(request, agents, &settings);
            crate::select_default_agent(&ranked, &settings)
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
}

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
    events: broadcast::Sender<OrchestrationEvent>,
}

#[derive(Default)]
struct GlobalRegistry {
    groups: Mutex<HashMap<String, HashMap<String, Arc<AgentEntry>>>>,
}

struct AgentEntry {
    snapshot: Mutex<AgentSnapshot>,
    mailbox: Mutex<VecDeque<MailboxMessage>>,
    mailbox_capacity: usize,
    message_ready: Notify,
    idle_park_token: Mutex<Option<CancellationToken>>,
    artifact_path: Mutex<Option<PathBuf>>,
    history_path: Mutex<Option<PathBuf>>,
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
                events,
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
        let targets = if to == "all" {
            REGISTRY
                .list(&self.inner.group_id, from)
                .into_iter()
                .filter(|entry| entry.status != AgentStatus::Aborted)
                .map(|entry| entry.id)
                .collect::<Vec<_>>()
        } else {
            vec![to.to_owned()]
        };
        targets
            .into_iter()
            .map(|target| {
                let message = MailboxMessage {
                    id: Uuid::now_v7().to_string(),
                    from: from.to_owned(),
                    to: target.clone(),
                    body: body.to_owned(),
                    timestamp: now_millis(),
                    reply_to: reply_to.clone(),
                };
                match REGISTRY.enqueue(&self.inner.group_id, &target, message) {
                    Ok(outcome) => DeliveryReceipt {
                        to: target,
                        outcome,
                        error: None,
                    },
                    Err(error) => DeliveryReceipt {
                        to: target,
                        outcome: DeliveryOutcome::Failed,
                        error: Some(error.to_string()),
                    },
                }
            })
            .collect()
    }

    #[must_use]
    pub fn inbox(&self, agent_id: &str, peek: bool) -> Vec<MailboxMessage> {
        REGISTRY.inbox(&self.inner.group_id, agent_id, peek)
    }

    pub async fn wait_message(
        &self,
        agent_id: &str,
        from: Option<&str>,
        timeout: Option<Duration>,
        abort: Option<pi_agent::AbortSignal>,
    ) -> Result<Option<MailboxMessage>> {
        let entry = REGISTRY
            .get(&self.inner.group_id, agent_id)
            .ok_or_else(|| anyhow!("unknown orchestration agent {agent_id:?}"))?;
        let wait = async {
            loop {
                let notified = entry.message_ready.notified();
                if let Some(message) = take_matching_message(&entry, from) {
                    return Some(message);
                }
                notified.await;
            }
        };
        match (timeout, abort) {
            (Some(timeout), Some(abort)) => tokio::select! {
                message = tokio::time::timeout(timeout, wait) => Ok(message.ok().flatten()),
                () = abort.cancelled() => Err(anyhow!("message wait aborted")),
                () = self.inner.shutdown.cancelled() => Err(anyhow!("orchestration shut down")),
            },
            (Some(timeout), None) => tokio::select! {
                message = tokio::time::timeout(timeout, wait) => Ok(message.ok().flatten()),
                () = self.inner.shutdown.cancelled() => Err(anyhow!("orchestration shut down")),
            },
            (None, Some(abort)) => tokio::select! {
                message = wait => Ok(message),
                () = abort.cancelled() => Err(anyhow!("message wait aborted")),
                () = self.inner.shutdown.cancelled() => Err(anyhow!("orchestration shut down")),
            },
            (None, None) => tokio::select! {
                message = wait => Ok(message),
                () = self.inner.shutdown.cancelled() => Err(anyhow!("orchestration shut down")),
            },
        }
    }

    pub fn cancel(&self, ids: &[String]) -> Vec<String> {
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
        if id == self.inner.config.main_agent_id {
            bail!("orchestration main agent cannot be parked");
        }
        self.cancel_park_timer(id);
        REGISTRY.set_status(&self.inner.group_id, id, AgentStatus::Parked)?;
        if let Some(agent) = self.agent_snapshot(id) {
            self.publish_agent(agent);
        }
        Ok(())
    }

    /// Finalize an agent's lifecycle state and arm idle-to-park transition.
    ///
    /// Mirrors `GlobalRegistry::finish` but additionally schedules an idle TTL
    /// park timer for completed (Idle) agents. Aborted or Running terminals
    /// cancel any pending park timer so a revived-then-failed agent does not
    /// park later.
    pub fn finish_agent(
        &self,
        id: &str,
        status: AgentStatus,
        artifact_path: Option<PathBuf>,
        history_path: Option<PathBuf>,
    ) -> Result<()> {
        REGISTRY.finish(
            &self.inner.group_id,
            id,
            status,
            artifact_path,
            history_path,
        )?;
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
        if let Some(prev) = timers.remove(id) {
            prev.abort();
        }
        let handle = tokio::spawn(async move {
            tokio::select! {
                () = shutdown.cancelled() => {}
                () = token.cancelled() => {}
                () = tokio::time::sleep(ttl) => {
                    if let Some(agent) = REGISTRY.park_if_idle(&runtime.inner.group_id, &id_owned, &runtime.inner.config.main_agent_id) {
                        runtime.publish_agent(agent);
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
        if let Some(entry) = REGISTRY.get(&self.inner.group_id, id) {
            if let Some(token) = entry.idle_park_token.lock().take() {
                token.cancel();
            }
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
        let mut seen = std::collections::BTreeSet::new();
        let mut prepared = Vec::with_capacity(items.len());
        let available_models = crate::available_models();
        for item in items {
            validate_agent_id(&item.id)?;
            if !seen.insert(item.id.clone()) {
                bail!("duplicate child agent id {:?}", item.id);
            }
            if REGISTRY.get(&self.inner.group_id, &item.id).is_some() {
                bail!("child agent id {:?} is already registered", item.id);
            }
            if self.inner.jobs.contains_identifier(&item.id) {
                bail!("child agent id {:?} conflicts with an existing job identifier", item.id);
            }
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
            if let Err(error) = crate::resolve_agent_model(
                &definition,
                agent_settings,
                &parent_model,
                &available_models,
            ) {
                return Err(crate::agent_model_error(&item.agent, &error));
            }
            prepared.push((item, definition));
        }

        let mut spawns = Vec::with_capacity(prepared.len());
        for (item, definition) in prepared {
            let created_at = now_millis();
            let job_id = loop {
                let candidate = Uuid::now_v7().to_string();
                if !self.inner.jobs.contains_identifier(&candidate)
                    && !seen.contains(&candidate)
                {
                    break candidate;
                }
            };
            let cancel = CancellationToken::new();
            let agent_snapshot = AgentSnapshot {
                id: item.id.clone(),
                display_name: format!("{}: {}", definition.name, one_line(&item.assignment)),
                parent_id: Some(parent_id.to_owned()),
                status: AgentStatus::Queued,
                created_at,
                last_activity: created_at,
                unread: 0,
                artifact_ref: None,
                history_ref: None,
            };
            REGISTRY.register(
                &self.inner.group_id,
                agent_snapshot.clone(),
                self.inner.config.mailbox_capacity,
            )?;
            self.publish_agent(agent_snapshot);
            let job_snapshot = self.inner.jobs.insert(
                JobSnapshot {
                    id: job_id.clone(),
                    agent_id: item.id.clone(),
                    agent: item.agent.clone(),
                    parent_id: parent_id.to_owned(),
                    description: Some(one_line(&item.assignment)),
                    status: JobStatus::Queued,
                    created_at,
                    started_at: None,
                    finished_at: None,
                    result: None,
                },
                cancel.clone(),
            )?;
            self.publish_job(job_snapshot);
            self.inner.active.lock().insert(item.id.clone(), cancel.clone());
            spawns.push(TaskSpawn {
                index: item.index,
                job_id: job_id.clone(),
                agent_id: item.id.clone(),
                agent: item.agent.clone(),
                status: JobStatus::Queued,
            });
            let runtime = self.clone();
            let parent_id = parent_id.to_owned();
            let spawned_job_id = job_id;
            tokio::spawn(async move {
                let result = runtime
                    .run_one(
                        parent_id,
                        parent_depth + 1,
                        item,
                        definition,
                        cancel,
                        &spawned_job_id,
                    )
                    .await;
                if let Some(job) = runtime
                    .inner
                    .jobs
                    .finish(&spawned_job_id, result, now_millis())
                {
                    runtime.publish_job(job);
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
        parent_id: String,
        depth: usize,
        item: TaskItem,
        definition: AgentDefinition,
        cancel: CancellationToken,
        job_id: &str,
    ) -> TaskResult {
        let _active_guard = ActiveChildGuard {
            inner: self.inner.clone(),
            id: item.id.clone(),
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
        let permit = tokio::select! {
            permit = self.inner.semaphore.clone().acquire_owned() => match permit {
                Ok(permit) => permit,
                Err(_) => return self.failed_result(&item, job_id, AgentStatus::Aborted, "orchestration semaphore closed"),
            },
            () = cancel.cancelled() => return self.failed_result(&item, job_id, AgentStatus::Aborted, "task cancelled before start"),
        };
        let _ = REGISTRY.set_status(&self.inner.group_id, &item.id, AgentStatus::Running);
        if let Some(agent) = self.agent_snapshot(&item.id) {
            self.publish_agent(agent);
        }
        if let Some(job) = self.inner.jobs.mark_running(job_id, now_millis()) {
            self.publish_job(job);
        }
        let system_prompt = match self.child_system_prompt(&definition, &item.assignment) {
            Ok(prompt) => prompt,
            Err(error) => return self.failed_result(&item, job_id, AgentStatus::Idle, &error.to_string()),
        };
        let orchestration_tools = self.agent_tools(&item.id, depth);
        let resolved_model = {
            let available = crate::available_models();
            let parent_model = self.inner.config.current_parent_model();
            match crate::resolve_agent_model(
                &definition,
                self.inner.config.agent_settings.get(&definition.name),
                &parent_model,
                &available,
            ) {
                Ok(resolved) => resolved,
                Err(error) => {
                    return self.failed_result(&item, job_id, AgentStatus::Idle, &error.to_string());
                }
            }
        };
        let request = ChildSessionRequest {
            child_id: item.id.clone(),
            parent_id,
            depth,
            definition: definition.clone(),
            assignment: item.assignment.clone(),
            system_prompt,
            requested_tool_names: crate::effective_agent_tool_names(
                &definition,
                self.inner.config.agent_settings.get(&definition.name),
            )
            .map(<[String]>::to_vec),
            orchestration_tools,
            thinking_level: resolved_model.thinking_level.or(definition.thinking_level),
            max_tools_per_agent: self.inner.config.max_tools_per_agent,
            model: resolved_model.model,
        };
        let session = tokio::select! {
            result = (self.inner.factory)(request) => match result {
                Ok(session) => session,
                Err(error) => return self.failed_result(&item, job_id, AgentStatus::Idle, &error.to_string()),
            },
            () = cancel.cancelled() => return self.failed_result(&item, job_id, AgentStatus::Aborted, "task cancelled during child session creation"),
        };
        let mut run = Box::pin(session.run(&item.assignment, Vec::new()));
        let outcome = tokio::select! {
            result = &mut run => result.map_err(|error| error.to_string()),
            () = cancel.cancelled() => {
                let drain = async {
                    session.abort().await;
                    let _ = (&mut run).await;
                };
                if tokio::time::timeout(CHILD_ABORT_GRACE, drain).await.is_err() {
                    Err(format!(
                        "task cancellation timed out after {}s",
                        CHILD_ABORT_GRACE.as_secs()
                    ))
                } else {
                    Err("task cancelled".to_owned())
                }
            }
        };
        drop(permit);
        let status = if cancel.is_cancelled() {
            AgentStatus::Aborted
        } else {
            AgentStatus::Idle
        };
        let (output, error, usage) = match outcome {
            Ok(result) => (result.text, result.error_message, result.usage),
            Err(error) => (
                session.last_assistant_text(),
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
        let history_error = serde_json::to_vec_pretty(&session.history())
            .context("serializing subagent history")
            .and_then(|bytes| {
                write_new_artifact(&history_path, &bytes)
                    .with_context(|| format!("writing subagent history {}", history_path.display()))
            })
            .err();
        let history_written = history_error.is_none();
        let final_error = match artifact_error.or(history_error) {
            Some(write_error) => Some(match error.as_deref() {
                Some(error) => format!("{error}; {write_error}"),
                None => write_error.to_string(),
            }),
            None => error,
        };
        let _ = self.finish_agent(
            &item.id,
            status,
            artifact_written.then_some(artifact_path),
            history_written.then_some(history_path),
        );
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
        let final_error = match artifact_error.or(history_error) {
            Some(write_error) => format!("{error}; {write_error}"),
            None => error.to_owned(),
        };
        let _ = self.finish_agent(
            &item.id,
            status,
            artifact_written.then_some(artifact_path),
            history_written.then_some(history_path),
        );
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

    fn child_system_prompt(&self, definition: &AgentDefinition, assignment: &str) -> Result<String> {
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
            agents: &[],
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
        prompt.push_str(&format!(
            "\n\n<delegated_assignment>\n{}\n</delegated_assignment>",
            assignment
        ));
        Ok(prompt)
    }

    #[must_use]
    pub fn select_agent(&self, assignment: &str, explicit: Option<&str>) -> String {
        if let Some(explicit) = explicit.filter(|name| !name.trim().is_empty()) {
            return explicit.to_owned();
        }
        let enabled_owned = self
            .enabled_agents()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        if let Some(selector) = &self.inner.config.default_agent_selector
            && let Some(selected) = selector(assignment, &enabled_owned)
        {
            return selected;
        }
        if enabled_owned
            .iter()
            .any(|agent| agent.name == self.inner.config.default_agent)
        {
            return self.inner.config.default_agent.clone();
        }
        enabled_owned
            .first()
            .map(|agent| agent.name.clone())
            .unwrap_or_else(|| self.inner.config.default_agent.clone())
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

    pub fn cancel_jobs(&self, ids: &[String]) -> Vec<String> {
        self.prune_retained_jobs();
        self.inner.jobs.cancel(ids)
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
        self.cleanup_retained_jobs();
        REGISTRY.remove_group(&self.inner.group_id);
    }
}

struct ActiveChildGuard {
    inner: Arc<RuntimeInner>,
    id: String,
}

impl Drop for ActiveChildGuard {
    fn drop(&mut self) {
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
        let mut groups = self.groups.lock();
        let entries = groups.entry(group.to_owned()).or_default();
        if entries.contains_key(&snapshot.id) {
            bail!("orchestration agent id {:?} is already registered", snapshot.id);
        }
        entries.insert(
            snapshot.id.clone(),
            Arc::new(AgentEntry {
                snapshot: Mutex::new(snapshot),
                mailbox: Mutex::new(VecDeque::new()),
                mailbox_capacity,
                message_ready: Notify::new(),
                idle_park_token: Mutex::new(None),
                artifact_path: Mutex::new(None),
                history_path: Mutex::new(None),
            }),
        );
        Ok(())
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
    fn enqueue(&self, group: &str, target: &str, message: MailboxMessage) -> Result<DeliveryOutcome> {
        let entry = self
            .get(group, target)
            .ok_or_else(|| anyhow!("unknown orchestration agent {target:?}"))?;
        {
            let snapshot = entry.snapshot.lock();
            if snapshot.status == AgentStatus::Aborted {
                bail!("orchestration agent {target:?} is aborted");
            }
        }
        let mut mailbox = entry.mailbox.lock();
        if mailbox.len() >= entry.mailbox_capacity {
            bail!("orchestration mailbox for {target:?} is full");
        }
        mailbox.push_back(message);
        drop(mailbox);
        let outcome = {
            let mut snapshot = entry.snapshot.lock();
            snapshot.last_activity = now_millis();
            match snapshot.status {
                AgentStatus::Queued | AgentStatus::Running | AgentStatus::Parked => {
                    DeliveryOutcome::Queued
                }
                AgentStatus::Idle => DeliveryOutcome::Woken,
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

    fn set_status(&self, group: &str, id: &str, status: AgentStatus) -> Result<()> {
        let entry = self
            .get(group, id)
            .ok_or_else(|| anyhow!("unknown orchestration agent {id:?}"))?;
        let mut snapshot = entry.snapshot.lock();
        snapshot.status = status;
        snapshot.last_activity = now_millis();
        Ok(())
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
        runtime.child_system_prompt(definition, "work").expect("boundary accepted");
    }

    #[test]
    fn autoload_skill_rejects_one_byte_over_file_limit() {
        let root = tempfile::tempdir().expect("root");
        let path = root.path().join("SKILL.md");
        fs::write(&path, vec![b'x'; MAX_AUTOLOAD_SKILL_BYTES as usize + 1]).expect("skill");
        let runtime = runtime_with_skill(path);
        let definition = runtime.catalog().get("bounded").expect("agent");
        let error = runtime.child_system_prompt(definition, "work").expect_err("oversized rejected").to_string();
        assert!(error.contains("exceeds maximum size"));
    }
}
