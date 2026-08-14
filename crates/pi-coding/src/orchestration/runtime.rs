use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering}};
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
use uuid::Uuid;
use unicode_normalization::UnicodeNormalization;

use crate::{Session, Skill};

use super::{
    AgentCatalog, AgentDefinition, CapabilityCeiling, JobClock, JobManager, JobRetention,
    JobSnapshot, JobSoftBudget, JobStatus, PreparedJobRecords, TaskSpawn,
};

pub const DEFAULT_MAILBOX_CAPACITY: usize = 100;
pub const DEFAULT_MAX_CONCURRENCY: usize = 4;
pub const DEFAULT_MAX_RECURSION_DEPTH: usize = 2;
pub const DEFAULT_MAX_TOOLS_PER_AGENT: usize = 16;
pub const DEFAULT_IDLE_TTL_SECS: u64 = 300;
pub const DEFAULT_MAX_RETAINED_JOBS: usize = 256;
pub const DEFAULT_RETAINED_JOB_TTL_SECS: u64 = 24 * 60 * 60;
/// Bound on the delivered-message log the workflow page's Recent IRC reads.
const DELIVERED_MESSAGE_LOG_CAP: usize = 200;
/// Stable custom-message type for orchestration mailbox deliveries.
pub const ORCHESTRATION_MESSAGE_TYPE: &str = "orchestration_message";
/// Default number of transcript lines rendered by `hub read_history`.
pub const DEFAULT_HISTORY_LINES: usize = 50;
/// Hard maximum for `hub read_history` lines (clamped, never exceeded).
pub const MAX_HISTORY_LINES: usize = 200;
/// Hard byte cap for a rendered `hub read_history` transcript.
pub const MAX_HISTORY_BYTES: usize = 32 * 1024;
const MAX_AUTOLOAD_SKILL_BYTES: u64 = 256 * 1024;
const MAX_AUTOLOAD_PROMPT_BYTES: usize = 1024 * 1024;
const MAX_PERSONALITY_BYTES: usize = 64 * 1024;
const PERSONA_CONTINUITY_MAX_MESSAGES: usize = 200;
const PERSONA_CONTINUITY_MAX_BYTES: usize = 32 * 1024;
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

    async fn steer(&self, message: &MailboxMessage) -> Result<()> {
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
            .await
    }

    async fn run(&self, assignment: &str) -> Result<crate::RunResult> {
        self.session.run(assignment, Vec::new()).await
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
                let sandbox = snapshot.sandbox.clone();
                let memory = snapshot.memory.clone();
                // Declared tools become base tools, EXCEPT orchestration plumbing
                // (todo/process/task/hub/goal/yield): those are auto-provided by
                // the factory below (orchestration_tools + the yield append), so
                // they must never reach `create_tool`, which only knows the
                // main-session tool set. Skipping a declared `yield` here is what
                // keeps registration idempotent: declared-or-not, exactly one
                // `yield` tool is appended below. Unknown declared names are
                // likewise filtered before `create_tool` (which would otherwise
                // error): they are silently ignored (OMP-compatible) and never
                // injected into the child.
                let persona_root = request.definition.persona_root();
                let requested_memory = request.requested_tool_names.as_deref().is_some_and(|names| {
                    names.iter().any(|name| matches!(name.as_str(), "memory" | "recall" | "retain" | "reflect"))
                });
                let base_tools = match request.requested_tool_names.as_deref() {
                    Some(names) => {
                        let mut tools = names
                            .iter()
                            .filter(|name| {
                                !crate::is_child_plumbing_tool(name)
                                    && crate::is_known_child_tool(name)
                                    && !matches!(name.as_str(), "memory" | "recall" | "retain" | "reflect")
                            })
                            .map(|name| match persona_root.as_deref() {
                                Some(root) => crate::tools::create_tool_with_context_and_resolver_and_rules_for_persona(
                                    name, &cwd, root, None, None, sandbox.clone(), uri_resolver.clone(), snapshot.permission_rules.clone(),
                                ),
                                None => crate::create_tool_with_context_and_resolver_and_rules(
                                    name, &cwd, None, None, sandbox.clone(), uri_resolver.clone(), snapshot.permission_rules.clone(),
                                ),
                            })
                            .collect::<Result<Vec<_>>>()?;
                        if requested_memory {
                            let config = memory.as_ref().and_then(|resolver| resolver());
                            tools.extend(match persona_root.as_deref() {
                                Some(root) => crate::memory::memory_tools_for_persona(&cwd, root, None, config),
                                None => crate::memory::memory_tools_for(&cwd, None, config),
                            });
                        }
                        tools
                    }
                    None => match persona_root.as_deref() {
                        Some(root) => crate::tools::create_coding_tools_with_context_and_resolver_for_persona(
                            &cwd, root, None, None, None, sandbox, uri_resolver.clone(), memory.clone(), None,
                        ),
                        None => crate::create_coding_tools_with_context_and_resolver(
                            &cwd, None, None, None, sandbox, uri_resolver.clone(), memory.clone(), None,
                        ),
                    },
                };
                if base_tools.len() > request.max_tools_per_agent {
                    bail!(
                        "child agent {:?} resolves to more than {} tools",
                        request.definition.name,
                        request.max_tools_per_agent
                    );
                }
                // Apply the role contract filters to the child's tool set:
                // drop disallowed tools by name and any tool whose declared
                // capability sits above the role's ceiling. Orchestration
                // plumbing (todo/process/task/hub/goal) is kept so a restricted
                // role can still delegate and be supervised.
                let disallowed = request
                    .definition
                    .disallowed_tools
                    .iter()
                    .map(String::as_str)
                    .collect::<std::collections::BTreeSet<_>>();
                let ceiling = request.definition.capability_ceiling;
                let allowed_capabilities = ceiling.map(CapabilityCeiling::allowed_capabilities);
                let mut tools = base_tools
                    .into_iter()
                    .filter(|tool| {
                        !disallowed.contains(tool.name.as_str())
                            && allowed_capabilities
                                .as_ref()
                                .is_none_or(|allowed| allowed.contains(&tool.capability))
                    })
                    .collect::<Vec<_>>();
                tools.extend(request.orchestration_tools);
                // The `yield` tool joins every orchestration child's tool set
                // (OMP's explicit-delivery protocol): it is orchestration
                // plumbing like task/hub/goal, so role filters never remove
                // it. It is wired to the per-run delivery state the run loop
                // reads when the child settles.
                tools.push(super::tools::yield_tool(request.yield_state.clone()));
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
    /// `CONTEXT` section (OMP batch parity). `None` for single spawns.
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
            self.inner.persona_lifecycle_blocks.lock().remove(&self.persona);
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
    /// Bounded, newest-last log of every durably delivered group message
    /// (subagent ⇄ subagent included), independent of mailbox consumption:
    /// the workflow page's Recent IRC reads this stream, so messages stay
    /// visible after the recipient consumes them.
    delivered_messages: Mutex<VecDeque<MailboxMessage>>,
    durable: Mutex<Option<super::persistence::DurableRuntime>>,
    durable_bound: AtomicBool,
    durable_mutation: Mutex<()>,
    /// Personas currently claimed by a destructive lifecycle operation.
    persona_lifecycle_blocks: Mutex<std::collections::BTreeSet<String>>,
    rebind_reserved: AtomicBool,
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
        let preferred_agent = config.preferred_agent.clone();
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
                delivered_messages: Mutex::new(VecDeque::new()),
                durable: Mutex::new(None),
                durable_bound: AtomicBool::new(false),
                durable_mutation: Mutex::new(()),
                persona_lifecycle_blocks: Mutex::new(std::collections::BTreeSet::new()),
                rebind_reserved: AtomicBool::new(false),
                preferred_agent: RwLock::new(preferred_agent),
                unknown_tool_warnings: Mutex::new(std::collections::BTreeSet::new()),
            }),
            owner: true,
        })
    }

    #[must_use]
    pub fn group_id(&self) -> &str {
        &self.inner.group_id
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
            .map(|(agent, tool)| {
                crate::unknown_tools_warning(agent, std::slice::from_ref(tool))
            })
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
        let peer_roster = self.sibling_roster(target);
        let system_prompt = match self.child_system_prompt(
            &definition,
            &info.request.assignment,
            None,
            info.request.output_schema.as_ref(),
            info.request.schema_mode.as_deref(),
            &peer_roster,
        ) {
            Ok(prompt) => prompt,
            Err(_) => return Ok(None),
        };
        let requested_tool_names = crate::effective_agent_tool_names(
            &definition,
            self.inner.config.agent_settings.get(&definition.name),
        )
        .map(<[String]>::to_vec);
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
            system_prompt,
            requested_tool_names,
            orchestration_tools,
            self.inner.config.max_tools_per_agent,
        ) {
            Some(request) => request,
            None => return Ok(None),
        };
        if definition.is_persona()
            && self
                .inner
                .persona_lifecycle_blocks
                .lock()
                .contains(&definition.name)
        {
            bail!("persona {:?} has a destructive operation in progress", definition.name);
        }
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
            soft_budget_exhausted: false,
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
        let definition = request.definition.clone();
        let durable = match self.inner.durable.lock().clone() {
            Some(durable) => durable,
            None => return self.failed_result(
                &TaskItem { index: 0, id: agent_id.clone(), agent: request.definition.name.clone(), assignment: assignment.clone(), todo_task_id: None, ..Default::default() },
                job_id,
                AgentStatus::Idle,
                "durable orchestration binding is unavailable",
            ),
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
                &TaskItem { index: 0, id: agent_id.clone(), agent: request.definition.name.clone(), assignment: assignment.clone(), todo_task_id: None, ..Default::default() },
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
                &TaskItem { index: 0, id: agent_id.clone(), agent: request.definition.name.clone(), assignment: assignment.clone(), todo_task_id: None, ..Default::default() },
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
        let persona_root = definition.persona_root();
        let mut canonical_session_path = session_path.clone();
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
                        &TaskItem { index: 0, id: agent_id.clone(), agent: agent_type.clone(), assignment: assignment.clone(), todo_task_id: None, ..Default::default() },
                        job_id,
                        AgentStatus::Idle,
                        &format!("opening durable child transcript: {error:#}"),
                    );
                }
                if session_path.is_none() {
                    let Some((_, new_path)) = session.recorder_info() else {
                        return self.failed_result(
                            &TaskItem { index: 0, id: agent_id.clone(), agent: agent_type.clone(), assignment: assignment.clone(), todo_task_id: None, ..Default::default() },
                            job_id,
                            AgentStatus::Idle,
                            "durable child recorder is unavailable",
                        );
                    };
                    let new_path = match durable.canonicalize_child_session_path(&new_path) {
                        Ok(path) => path,
                        Err(error) => return self.failed_result(
                            &TaskItem { index: 0, id: agent_id.clone(), agent: agent_type.clone(), assignment: assignment.clone(), todo_task_id: None, ..Default::default() },
                            job_id,
                            AgentStatus::Idle,
                            &error.to_string(),
                        ),
                    };
                    canonical_session_path = Some(new_path.clone());
                    let durable_mutation = self.inner.durable_mutation.lock();
                    if self.inner.rebind_reserved.load(Ordering::Acquire) {
                        drop(durable_mutation);
                        return self.failed_result(
                            &TaskItem { index: 0, id: agent_id.clone(), agent: agent_type.clone(), assignment: assignment.clone(), todo_task_id: None, ..Default::default() },
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
                            &TaskItem { index: 0, id: agent_id.clone(), agent: agent_type.clone(), assignment: assignment.clone(), todo_task_id: None, ..Default::default() },
                            job_id,
                            AgentStatus::Idle,
                            &error,
                        );
                    }
                    drop(durable_mutation);
                }
                if let Some(root) = persona_root.as_deref() {
                    let continuity = match load_persona_continuity(root) {
                        Ok(messages) => messages,
                        Err(error) => return self.failed_result(
                            &TaskItem { index: 0, id: agent_id.clone(), agent: agent_type.clone(), assignment: assignment.clone(), todo_task_id: None, ..Default::default() },
                            job_id,
                            AgentStatus::Idle,
                            &format!("loading revived persona continuity: {error:#}"),
                        ),
                    };
                    let merged = merge_persona_continuity(continuity, session.history());
                    if let Err(error) = session.load_history(merged).await {
                        return self.failed_result(
                            &TaskItem { index: 0, id: agent_id.clone(), agent: agent_type.clone(), assignment: assignment.clone(), todo_task_id: None, ..Default::default() },
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
                    &TaskItem { index: 0, id: agent_id.clone(), agent: agent_type.clone(), assignment: assignment.clone(), todo_task_id: None, ..Default::default() },
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
                    &TaskItem { index: 0, id: agent_id.clone(), agent: agent_type.clone(), assignment: assignment.clone(), todo_task_id: None, ..Default::default() },
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
                        &TaskItem { index: 0, id: agent_id.clone(), agent: agent_type.clone(), assignment: assignment.clone(), todo_task_id: None, ..Default::default() },
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
                    &TaskItem { index: 0, id: agent_id.clone(), agent: agent_type.clone(), assignment: assignment.clone(), todo_task_id: None, ..Default::default() },
                    job_id,
                    AgentStatus::Idle,
                    &format!("failed to deliver queued message {}: {error:#}", message.id),
                );
            }
            self.acknowledge_delivery(&agent_id, &message.id);
        }

        // Run the revival as a continuation turn.
        let mut run = Box::pin(child.run(&assignment));
        let run_deadline = definition.timeout_secs.map(|seconds| {
            tokio::time::Instant::now() + Duration::from_secs(seconds)
        });

        let outcome = loop {
            tokio::select! {
                result = &mut run => break result.map_err(|error| error.to_string()),
                message = delivery_rx.recv() => {
                    match message {
                        Some(message) => {
                            if let Err(error) = child.steer(&message).await {
                                break Err(format!("failed to deliver message {}: {error:#}", message.id));
                            }
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
                () = async {
                    tokio::time::sleep_until(run_deadline.expect("deadline guard")).await;
                }, if run_deadline.is_some() => {
                    {
                        let _durable_mutation = self.inner.durable_mutation.lock();
                        REGISTRY.unregister_active_delivery(&self.inner.group_id, &agent_id);
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
            REGISTRY.unregister_active_delivery(&self.inner.group_id, &agent_id);
        }
        drop(local_permit);

        let status = if cancel.is_cancelled() {
            AgentStatus::Aborted
        } else {
            AgentStatus::Idle
        };
        let soft_budget_exhausted = soft_budget_state.triggered.load(Ordering::Acquire);
        let execution_ceiling_reached = soft_budget_exhausted
            || contract_state.max_turns_triggered.load(Ordering::Acquire)
            || contract_state.max_tool_calls_triggered.load(Ordering::Acquire);
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
        if let (Some(root), Some(source)) = (
            persona_root.as_deref(),
            canonical_session_path.as_deref(),
        ) && let Err(archive_error) = archive_persona_session(root, &agent_id, source, true)
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
                snapshot.history_ref = Some(persisted_history_reference(
                    &definition,
                    &snapshot.id,
                ));
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

    /// Append one durably delivered group message to the bounded delivered
    /// log (newest-last). The log is the workflow page's Recent IRC source:
    /// it survives mailbox consumption, so subagent ⇄ subagent messages stay
    /// visible after the recipient reads them.
    fn record_delivered_message(&self, message: MailboxMessage) {
        let mut log = self.inner.delivered_messages.lock();
        log.push_back(message);
        if log.len() > DELIVERED_MESSAGE_LOG_CAP {
            let excess = log.len() - DELIVERED_MESSAGE_LOG_CAP;
            log.drain(..excess);
        }
    }

    /// Bounded, newest-last log of every durably delivered group message,
    /// independent of mailbox consumption.
    #[must_use]
    pub fn delivered_messages(&self) -> Vec<MailboxMessage> {
        self.inner.delivered_messages.lock().iter().cloned().collect()
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
                                // Every durably delivered group message lands
                                // in the bounded delivered-message log (the
                                // workflow page's Recent IRC source), whether
                                // or not the recipient has consumed it yet.
                                self.record_delivered_message(message.clone());
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
                bail!("persona {:?} has a destructive operation in progress", definition.name);
            }
            item.id = self.allocate_unique_agent_id_for_definition(
                &item.id,
                &definition,
                &mut reserved,
            )?;
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
            // Unknown declared tools are silently ignored (OMP-compatible):
            // record a deduped warning and continue — they never abort the
            // batch and never make the agent unusable.
            let unsupported = crate::unsupported_agent_tools(&definition, agent_settings);
            if !unsupported.is_empty() {
                self.record_unknown_tool_warnings(&item.agent, &unsupported);
            }
            // The pre-check counts only names that will actually be injected
            // (known, non-plumbing): unknown declarations and plumbing never
            // consume max_tools_per_agent budget, matching the child-side
            // enforcement in the factory.
            if crate::effective_agent_tool_names(&definition, agent_settings).is_some_and(|tools| {
                tools
                    .iter()
                    .filter(|name| {
                        !crate::is_child_plumbing_tool(name)
                            && crate::is_known_child_tool(name)
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
                soft_budget_exhausted: false,
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
                        item.context.as_deref(),
                        item.output_schema.as_ref(),
                        item.schema_mode.as_deref(),
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
                    output_schema: item.output_schema.clone(),
                    schema_mode: item.schema_mode.clone(),
                    yield_state: Arc::new(YieldState::default()),
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
            definition.soft_budget.unwrap_or(self.inner.config.soft_budget)
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
            let turn_tokens = u64::try_from(turn.message.usage.total_tokens.max(0))
                .unwrap_or(u64::MAX);
            let token_count = tokens.fetch_add(turn_tokens, Ordering::Relaxed) + turn_tokens;
            let stop = budget.yield_after.is_some_and(|limit| request_count >= limit)
                || budget.max_requests.is_some_and(|limit| request_count >= limit)
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
            let call_count = tool_calls.fetch_add(turn_tool_calls, Ordering::Relaxed) + turn_tool_calls;
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
        let yield_hook = Arc::new(move |_turn: &ShouldStopAfterTurnContext| {
            yield_called.was_called()
        });
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
                let tool_failure = last_failed_tool_kind(
                    &result.messages,
                    &child.available_tool_names(),
                )
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
            group_id: self.inner.group_id.clone(),
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
        if let Some(root) = persona_root.as_deref() {
            let continuity = match load_persona_continuity(root) {
                Ok(messages) => messages,
                Err(error) => return self.failed_result(
                    &item,
                    job_id,
                    AgentStatus::Idle,
                    &format!("loading persona continuity: {error:#}"),
                ),
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
            match self.inner.durable.lock().clone() {
                Some(durable) => Some(durable),
                None => return self.failed_result(&item, job_id, AgentStatus::Idle, "durable orchestration binding is unavailable"),
            }
        } else {
            None
        };
        let mut canonical_session_path = None;
        if let Some(durable) = durable.as_ref() {
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
            if let Some(entry) = REGISTRY.get(&self.inner.group_id, &item.id)
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
            if let Err(error) = child.steer(message).await {
                return self.failed_result(
                    &item,
                    job_id,
                    AgentStatus::Idle,
                    &format!("failed to deliver queued message {}: {error:#}", message.id),
                );
            }
            self.acknowledge_delivery(&item.id, &message.id);
        }
        let mut run = Box::pin(child.run(&item.assignment));
        let run_deadline = definition.timeout_secs.map(|seconds| {
            tokio::time::Instant::now() + Duration::from_secs(seconds)
        });
        let outcome = loop {
            tokio::select! {
                result = &mut run => break result.map_err(|error| error.to_string()),
                message = delivery_rx.recv() => {
                    match message {
                        Some(message) => {
                            if let Err(error) = child.steer(&message).await {
                                break Err(format!("failed to deliver message {}: {error:#}", message.id));
                            }
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
                () = async {
                    tokio::time::sleep_until(run_deadline.expect("deadline guard")).await;
                }, if run_deadline.is_some() => {
                    {
                        let _durable_mutation = self.inner.durable_mutation.lock();
                        REGISTRY.unregister_active_delivery(&self.inner.group_id, &item.id);
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
            REGISTRY.unregister_active_delivery(&self.inner.group_id, &item.id);
        }
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
            || contract_state.max_tool_calls_triggered.load(Ordering::Acquire);
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
        if let (Some(root), Some(source)) = (persona_root.as_deref(), canonical_session_path.as_deref())
            && let Err(archive_error) = archive_persona_session(root, &item.id, source, false)
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
        let history_ref = self
            .inner
            .config
            .catalog
            .get(&item.agent)
            .map_or_else(
                || format!("history://{}", item.id),
                |definition| history_reference(definition, &item.id),
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
            soft_budget_exhausted: false,
            structured_output: None,
            artifact_ref,
            history_ref,
            artifact_uri,
        }
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
        let mut prompt = definition.system_prompt.clone();
        if let Some(personality) = definition
            .is_persona()
            .then_some(definition.personality.as_deref())
            .flatten()
            .filter(|personality| !personality.trim().is_empty())
        {
            if personality.len() > MAX_PERSONALITY_BYTES {
                bail!(
                    "persona personality exceeds maximum size of {MAX_PERSONALITY_BYTES} bytes"
                );
            }
            prompt.push_str("\n\n<personality>\n");
            prompt.push_str(&escape_xml(personality));
            prompt.push_str("\n</personality>");
        }
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
        if let Some(context) = context.filter(|text| !text.trim().is_empty()) {
            // OMP batch parity: the shared `context` is rendered verbatim into
            // every spawned child's system prompt as a CONTEXT section, so
            // each child of a batch sees the shared background alongside its
            // own delegated assignment.
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
    /// Precedence: explicit `task.agent` override (validated against the
    /// catalog: missing, disabled, or incompatible definitions fail actionably
    /// instead of falling back), then unique exact trusted agent-name mention
    /// (including disabled/untrusted rejection), then ranked metadata
    /// selection / default. Ambiguous exact mentions return an error.
    pub fn resolve_task_agent(&self, assignment: &str, explicit: Option<&str>) -> Result<String> {
        if let Some(explicit) = explicit.filter(|name| !name.trim().is_empty()) {
            if self.inner.config.catalog.get(explicit).is_none() {
                bail!(
                    "explicit agent {:?} is not defined in the agent catalog; \
                     define it as a user agent (~/.pi/agents) or a project agent in the \
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

    /// Spawn children when the request carries a delegation construction
    /// naming one or more exact trusted agents. Delegation intent is
    /// Unicode-aware: a conservative CJK construction where a Chinese
    /// delegation token abuts the FIRST mentioned name and an action clause
    /// follows the whole conjunction chain (`你让glm和grok一起调研这个仓库`),
    /// or an English delegation verb whose raw-text list includes the
    /// mentions (`Have glm and grok study this`; object mentions like
    /// `Have glm review grok's output` are not delegated). Every explicitly
    /// delegated name is spawned in one batch, in mention order, each with
    /// the full request as assignment.
    /// Informational mentions (`researcher 是做什么的？`,
    /// `glm和grok哪个好？`) and generic skill/semantic text return
    /// `Ok(None)` so the caller can keep selection recommendations without
    /// spawning. Distinct names that normalize identically (e.g.
    /// `Research-Agent` vs `research-agent`) stay ambiguous and fail
    /// actionably instead of fanning out.
    pub fn spawn_from_natural_language(
        &self,
        parent_id: &str,
        parent_depth: usize,
        request: &str,
    ) -> Result<Option<Vec<TaskSpawn>>> {
        let mentions = self.exact_agent_mentions_in_catalog(request);
        if mentions.is_empty() {
            return Ok(None);
        }
        let delegated_runs = self.delegated_runs_in(request, &mentions);
        // Normalized-name collisions (e.g. `Research-Agent` vs
        // `research-agent`) stay ambiguous ONLY when the colliding phrase
        // participates in the explicit delegation clause — an informational
        // mention (`Tell me whether Research Agent is available`) or a
        // non-delegating verb list (`Have me compare Research Agent
        // descriptions`) never errors and never spawns.
        if let Some(colliding) = crate::selector::exact_agent_mention_collisions(&mentions)
            && colliding
                .iter()
                .any(|name| delegated_phrase_participates(&delegated_runs, name))
        {
            let names = colliding
                .iter()
                .map(|name| format!("{name:?}"))
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "exact agent mention is ambiguous between {names}; pass the intended name in \
                 task.agent or rename agents so their normalized names are unique"
            );
        }
        let delegated_mentions = mentions
            .iter()
            .filter(|name| {
                let lowered = normalized_lower(name);
                delegated_runs.iter().any(|member| member == &lowered)
            })
            .cloned()
            .collect::<Vec<_>>();
        if delegated_mentions.is_empty() {
            return Ok(None);
        }
        // Delegation intent gates the fan-out: only mentions inside an
        // explicit delegation clause spawn. CJK: every mention carrying a
        // delegation construction delegates its own conjunction chain
        // (`你让glm和grok调研` -> both; `让glm调研；grok是做什么` -> glm only;
        // `glm是做什么？让grok调研` -> grok only; `不要让glm调研` -> nothing).
        // English: the verb's raw-text list — `Have glm and grok study this`
        // spawns both, `Have glm review grok's output` spawns only glm (grok
        // is the review's object), `Tell me whether glm or grok is better`
        // spawns nothing, and a negated verb (`Do not have glm study this`)
        // spawns nothing.
        let delegated_mentions = self.delegated_mentions_in(request);
        if delegated_mentions.is_empty() {
            return Ok(None);
        }
        // Validate every named agent up front so a single absent/disabled
        // name fails actionably before any job is created.
        for name in &delegated_mentions {
            self.ensure_agent_enabled(name)?;
            if !self
                .enabled_agents()
                .iter()
                .any(|agent| agent.name == *name)
            {
                bail!(
                    "exact agent mention {:?} is not available for spawning",
                    name
                );
            }
        }
        let items = delegated_mentions
            .iter()
            .enumerate()
            .map(|(index, name)| TaskItem {
                index,
                id: name.clone(),
                agent: name.clone(),
                assignment: request.to_owned(),
                todo_task_id: None,
                ..Default::default()
            })
            .collect::<Vec<_>>();
        let spawns = self.spawn_tasks(parent_id, parent_depth, items)?;
        Ok(Some(spawns))
    }

    /// P0-C catalog diagnostics: fail actionably when `request` (a workflow
    /// objective) explicitly delegates to agent names that are absent from
    /// this catalog or disabled in it, instead of silently degrading to the
    /// default agent. Skill names are exempt (a skill invocation is not an
    /// agent delegation). The message names the missing/disabled definition
    /// and where workflow agents are discovered.
    pub fn validate_delegation_agents(&self, request: &str) -> Result<()> {
        let catalog_agents = self.inner.config.catalog.agents().clone();
        let skill_names = self
            .skills()
            .iter()
            .map(|skill| skill.name.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        let mut absent = std::collections::BTreeSet::new();
        let mut disabled = std::collections::BTreeSet::new();
        for candidate in delegation_candidates(request) {
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
                 workflow agent catalog; define them as user agents under ~/.pi/agents or as \
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

    /// Exact trusted agent mentions that form an explicit delegation clause
    /// in `request` — the same scan [`spawn_from_natural_language`] fans out
    /// — without spawning anything. Callers use this to gate the spawn
    /// through the task authorization boundary before any job is created.
    #[must_use]
    pub(crate) fn delegated_mentions_in(&self, request: &str) -> Vec<String> {
        let mentions = self.exact_agent_mentions_in_catalog(request);
        let runs = self.delegated_runs_in(request, &mentions);
        mentions
            .iter()
            .filter(|name| {
                let lowered = normalized_lower(name);
                runs.iter().any(|member| member == &lowered)
            })
            .cloned()
            .collect()
    }

    /// Raw lowercase runs that form explicit delegation clauses in `request`
    /// (English verb lists plus per-mention CJK conjunction chains).
    fn delegated_runs_in(&self, request: &str, mentions: &[String]) -> Vec<String> {
        let mut delegated = Vec::new();
        if request_has_delegation_verb(request) {
            delegated.extend(english_delegation_list(request));
        }
        for mention in mentions {
            if let Some(chain) = cjk_delegation_chain(request, mention) {
                delegated.extend(chain);
            }
        }
        delegated
    }

    /// True when `request` carries an explicit delegation clause that is
    /// NEGATED (`别让glm调研`, `Do not have glm study this`): the user said
    /// NOT to delegate the named agents, so nothing may spawn and nothing
    /// may be orchestrated through an alternate owner (the auto-todo DAG).
    /// Shares the exact parser with the positive scan — never an ad hoc app
    /// string check.
    #[must_use]
    pub(crate) fn has_explicit_negated_delegation(&self, request: &str) -> bool {
        let mentions = self.exact_agent_mentions_in_catalog(request);
        if mentions.is_empty() {
            return false;
        }
        if request_has_delegation_verb(request)
            && mentions.iter().any(|name| {
                delegated_phrase_participates(&english_delegation_list_negated(request), name)
            })
        {
            return true;
        }
        mentions
            .iter()
            .any(|mention| cjk_delegation_chain_negated(request, mention).is_some())
    }

    pub(crate) fn exact_agent_ambiguity(&self, assignment: &str) -> Option<String> {
        self.exact_agent_mention_in_catalog(assignment)
            .ambiguity_message()
    }

    pub fn read_uri_resolver(&self) -> crate::InternalUriResolverFn {
        let runtime = self.clone();
        Arc::new(move |uri| {
            if let Some(reference) = uri.strip_prefix("history://") {
                return runtime.resolve_history_reference(reference);
            }
            resolve_read_uri_in(
                &runtime.inner.group_id,
                &runtime.inner.config.artifact_dir,
                uri,
            )
        })
    }

    pub fn resolve_read_uri(&self, uri: &str) -> Result<PathBuf> {
        if let Some(reference) = uri.strip_prefix("history://") {
            return self.resolve_history_reference(reference);
        }
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

    pub fn resolve_history_reference(&self, reference: &str) -> Result<PathBuf> {
        if let Some(qualified) = reference.strip_prefix("persona/") {
            let (persona, agent_id) = qualified
                .split_once('/')
                .ok_or_else(|| anyhow!("persona history URI must be history://persona/<persona>/<agent-id>"))?;
            validate_agent_id(persona)?;
            validate_agent_id(agent_id)?;
            return self.resolve_qualified_persona_archive(persona, agent_id);
        }
        validate_agent_id(reference)?;
        let Some(entry) = REGISTRY.get(&self.inner.group_id, reference) else {
            return self.resolve_persona_archive(reference);
        };
        match entry.history_path.lock().clone() {
            Some(path) => ensure_existing_artifact(&self.inner.config.artifact_dir, &path),
            None => self.resolve_persona_archive(reference),
        }
    }

    pub fn read_child_history(&self, agent_id: &str, lines: usize) -> Result<String> {
        let lines = lines.clamp(1, MAX_HISTORY_LINES);
        let path = if agent_id.starts_with("persona/") {
            self.resolve_history_reference(agent_id)?
        } else {
            validate_agent_id(agent_id)?;
            self.resolve_history_source(agent_id)?
        };
        let rendered = render_history_file(&path, lines)
            .with_context(|| format!("rendering history for agent {agent_id:?}"))?;
        let redacted = crate::redact::redact_secrets(&rendered);
        if redacted.len() <= MAX_HISTORY_BYTES {
            return Ok(redacted);
        }
        let marker = format!("\n[history truncated at {} KiB]", MAX_HISTORY_BYTES / 1024);
        let budget = MAX_HISTORY_BYTES.saturating_sub(marker.len());
        let mut end = budget;
        while !redacted.is_char_boundary(end) {
            end -= 1;
        }
        let mut capped = redacted[..end].to_owned();
        capped.push_str(&marker);
        Ok(capped)
    }

    fn resolve_persona_archive(&self, agent_id: &str) -> Result<PathBuf> {
        let mut matches = Vec::new();
        for definition in self.inner.config.catalog.agents() {
            if !definition.trusted || !definition.is_persona() {
                continue;
            }
            let Some(root) = definition.persona_root() else {
                continue;
            };
            let candidate = persona_archive_path(&root, agent_id)?;
            match fs::symlink_metadata(&candidate) {
                Ok(_) => matches.push((definition.name.clone(), root, candidate)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("reading persona history metadata"),
            }
        }
        match matches.as_slice() {
            [] => bail!("unknown orchestration agent {agent_id:?}"),
            [(persona, root, candidate)] => validate_persona_archive_candidate(
                &root.join("sessions"),
                candidate,
            )
            .with_context(|| format!("resolving persona history {persona:?}/{agent_id:?}")),
            _ => bail!(
                "persona history id {agent_id:?} is ambiguous; use history://persona/<persona>/{agent_id}"
            ),
        }
    }

    fn resolve_qualified_persona_archive(&self, persona: &str, agent_id: &str) -> Result<PathBuf> {
        let definition = self
            .inner
            .config
            .catalog
            .get(persona)
            .filter(|definition| definition.trusted && definition.is_persona())
            .ok_or_else(|| anyhow!("unknown persona {persona:?}"))?;
        let root = definition
            .persona_root()
            .ok_or_else(|| anyhow!("persona {persona:?} has no durable root"))?;
        let candidate = persona_archive_path(&root, agent_id)?;
        validate_persona_archive_candidate(&root.join("sessions"), &candidate)
            .with_context(|| format!("resolving persona history {persona:?}/{agent_id:?}"))
    }

    /// Resolve the canonical transcript file for a registered agent: the
    /// durable child session JSONL (the live transcript) when the agent has
    /// one, the settle-time `.history.json` snapshot otherwise, and the bound
    /// parent session JSONL for Main (whose registry history path starts
    /// unset). Every path is runtime-owned and canonical — never constructed
    /// from the caller-supplied `agent_id` (which only selects a registry entry).
    fn resolve_history_source(&self, agent_id: &str) -> Result<PathBuf> {
        let Some(entry) = REGISTRY.get(&self.inner.group_id, agent_id) else {
            return self.resolve_persona_archive(agent_id);
        };
        if let Some(info) = entry.durable_info.lock().clone()
            && let Some(session_path) = info.session_path
        {
            let durable = self
                .inner
                .durable
                .lock()
                .clone()
                .ok_or_else(|| anyhow!("orchestration history for agent {agent_id:?} is not available yet"))?;
            return durable
                .canonicalize_child_session_path(&session_path)
                .with_context(|| format!("resolving history for agent {agent_id:?}"));
        }
        if let Some(path) = entry.history_path.lock().clone() {
            return ensure_existing_artifact(&self.inner.config.artifact_dir, &path);
        }
        if agent_id == self.main_agent_id() {
            let durable = self
                .inner
                .durable
                .lock()
                .clone()
                .ok_or_else(|| anyhow!("orchestration history for agent {agent_id:?} is not available yet"))?;
            let path = durable.parent_session_path().to_path_buf();
            let metadata = fs::metadata(&path)
                .with_context(|| format!("reading parent session history {}", path.display()))?;
            if !metadata.is_file() {
                bail!("orchestration history for agent {agent_id:?} is not available yet");
            }
            return Ok(path);
        }
        bail!("orchestration history for agent {agent_id:?} is not available yet")
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
        let active_ids = self.inner.active.lock().keys().cloned().collect::<std::collections::BTreeSet<_>>();
        let registry_active = REGISTRY
            .list(&self.inner.group_id, self.main_agent_id())
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
            snapshot.history_ref = Some(
                entry
                    .durable_info
                    .lock()
                    .as_ref()
                    .map_or_else(
                        || format!("history://{id}"),
                        |info| persisted_history_reference(&info.definition, id),
                    ),
            );
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

fn persona_archive_path(persona_root: &Path, agent_id: &str) -> Result<PathBuf> {
    validate_agent_id(agent_id)?;
    Ok(persona_root
        .join("sessions")
        .join(format!("{agent_id}.jsonl")))
}

fn validate_persona_archive_candidate(
    sessions_root: &Path,
    candidate: &Path,
) -> Result<PathBuf> {
    // User-visible error contexts stay path-free: persona failures surface on
    // the job wire (job.result.error / agent history), so the filesystem
    // layout must never leak.
    let metadata = fs::symlink_metadata(candidate)
        .with_context(|| "reading persona archive metadata")?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        bail!("persona archive is not a regular non-symlink file");
    }
    let canonical_root = fs::canonicalize(sessions_root)
        .with_context(|| "resolving persona sessions root")?;
    let canonical = fs::canonicalize(candidate)
        .with_context(|| "resolving persona archive")?;
    if canonical.parent() != Some(canonical_root.as_path())
        || canonical.extension().and_then(|extension| extension.to_str()) != Some("jsonl")
    {
        bail!("persona archive escapes its sessions root");
    }
    Ok(canonical)
}
fn validate_persona_sessions_root(persona_root: &Path) -> Result<Option<PathBuf>> {
    let root_metadata = fs::symlink_metadata(persona_root)
        .with_context(|| "reading persona root")?;
    if root_metadata.file_type().is_symlink() || !root_metadata.file_type().is_dir() {
        bail!("persona root is not a regular non-symlink directory");
    }
    let canonical_root = fs::canonicalize(persona_root)
        .with_context(|| "resolving persona root")?;
    let sessions = persona_root.join("sessions");
    let metadata = match fs::symlink_metadata(&sessions) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| "reading persona sessions");
        }
    };
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        bail!("persona sessions path is not a regular non-symlink directory");
    }
    let canonical_sessions = fs::canonicalize(&sessions)
        .with_context(|| "resolving persona sessions")?;
    if canonical_sessions.parent() != Some(canonical_root.as_path()) {
        bail!("persona sessions directory escapes its persona root");
    }
    Ok(Some(canonical_sessions))
}

fn persona_archives_newest_first(persona_root: &Path) -> Result<Vec<PathBuf>> {
    let Some(sessions) = validate_persona_sessions_root(persona_root)? else {
        return Ok(Vec::new());
    };
    let mut archives = Vec::new();
    for entry in fs::read_dir(&sessions).with_context(|| "reading persona sessions")? {
        let entry = entry.with_context(|| "reading persona sessions")?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
            continue;
        }
        let path = validate_persona_archive_candidate(&sessions, &path)?;
        let modified = fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .unwrap_or(UNIX_EPOCH);
        archives.push((modified, path));
    }
    archives.sort_by(|(left_time, left_path), (right_time, right_path)| {
        right_time
            .cmp(left_time)
            .then_with(|| right_path.cmp(left_path))
    });
    Ok(archives.into_iter().map(|(_, path)| path).collect())
}

fn load_persona_continuity(persona_root: &Path) -> Result<Vec<pi_ai::Message>> {
    let mut newest_to_oldest = Vec::new();
    let mut bytes = 0usize;
    for path in persona_archives_newest_first(persona_root)? {
        let messages = match crate::session_store::load_session_messages(&path) {
            Ok(messages) => messages,
            Err(error) => {
                // The session-store error chain embeds the absolute archive
                // path, which would surface on the job wire (job.result.error
                // / agent history). Re-map to a bounded, path-free failure —
                // the operation stays actionable without leaking the
                // filesystem layout.
                let mut message = format!("{error:#}");
                message = message.replace(&path.to_string_lossy().as_ref(), "");
                anyhow::bail!(
                    "loading persona continuity archive: {}",
                    message.trim().trim_end_matches(':')
                );
            }
        };
        for message in messages.into_iter().rev() {
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

fn archive_persona_session(
    persona_root: &Path,
    agent_id: &str,
    source: &Path,
    replace_existing: bool,
) -> Result<PathBuf> {
    // User-visible contexts stay path-free (persona job failures surface on
    // the job wire); the server-side diagnostics keep the operation label.
    let source_metadata = fs::symlink_metadata(source)
        .with_context(|| "reading canonical child transcript")?;
    if source_metadata.file_type().is_symlink() || !source_metadata.file_type().is_file() {
        bail!("canonical child transcript is not a regular non-symlink file");
    }
    let destination = persona_archive_path(persona_root, agent_id)?;
    let parent = match validate_persona_sessions_root(persona_root)? {
        Some(parent) => parent,
        None => {
            let parent = destination
                .parent()
                .ok_or_else(|| anyhow!("persona archive has no parent"))?;
            fs::create_dir(parent)
                .with_context(|| "creating persona sessions directory")?;
            validate_persona_sessions_root(persona_root)?
                .ok_or_else(|| anyhow!("persona sessions directory was not created"))?
        }
    };
    let destination = parent.join(format!("{agent_id}.jsonl"));
    let temporary = parent.join(format!(".{agent_id}.{}.tmp", Uuid::new_v4().simple()));
    let result = (|| -> Result<()> {
        let mut input = File::open(source)
            .with_context(|| "opening canonical child transcript")?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .with_context(|| "creating persona archive temporary")?;
        std::io::copy(&mut input, &mut output)
            .with_context(|| "copying persona archive")?;
        output.flush().context("flushing persona archive")?;
        output.sync_all().context("syncing persona archive")?;
        drop(output);
        if replace_existing {
            match fs::symlink_metadata(&destination) {
                Ok(metadata)
                    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() =>
                {
                    bail!("existing persona archive is not a regular non-symlink file");
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| "reading persona archive");
                }
            }
            match fs::rename(&temporary, &destination) {
                Ok(()) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::PermissionDenied
                    ) =>
                {
                    let destination_metadata = fs::symlink_metadata(&destination)
                        .with_context(|| "reading persona archive")?;
                    if destination_metadata.file_type().is_symlink()
                        || !destination_metadata.file_type().is_file()
                    {
                        bail!("existing persona archive is not a regular non-symlink file");
                    }
                    fs::remove_file(&destination)
                        .with_context(|| "replacing persona archive")?;
                    fs::rename(&temporary, &destination)
                        .with_context(|| "installing persona archive")?;
                }
                Err(error) => {
                    return Err(error).with_context(|| "installing persona archive");
                }
            }
        } else {
            fs::hard_link(&temporary, &destination)
                .with_context(|| "installing persona archive")?;
            fs::remove_file(&temporary)
                .with_context(|| "removing persona archive temporary")?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;
    validate_persona_archive_candidate(&parent, &destination)
}

#[cfg(test)]
mod persona_runtime_tests {
    use super::*;

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

    fn runtime(root: &Path) -> (OrchestrationRuntime, AgentDefinition) {
        let definition = persona_definition(root);
        let mut config = OrchestrationConfig::new(
            AgentCatalog::from_agents(vec![definition.clone()]),
            root.join("artifacts"),
        );
        config.default_agent = definition.name.clone();
        let runtime = OrchestrationRuntime::new(
            config,
            Arc::new(|_| Box::pin(async { unreachable!() })),
        )
        .expect("runtime");
        (runtime, definition)
    }

    #[cfg(unix)]
    #[test]
    fn persona_archive_rejects_symlinked_sessions_directory() {
        let root = tempfile::tempdir().expect("root");
        let persona_root = root.path().join("persona");
        let outside = root.path().join("outside");
        fs::create_dir_all(&persona_root).expect("persona root");
        fs::create_dir_all(&outside).expect("outside");
        std::os::unix::fs::symlink(&outside, persona_root.join("sessions"))
            .expect("sessions symlink");
        let source = root.path().join("source.jsonl");
        fs::write(&source, "{}\n").expect("source");

        let error = archive_persona_session(&persona_root, "run-1", &source, false)
            .expect_err("symlinked sessions directory must fail");
        assert!(error.to_string().contains("non-symlink directory"), "{error:#}");
        assert!(!outside.join("run-1.jsonl").exists());
    }

    #[test]
    fn persona_history_fallback_survives_ordinary_retention() {
        let root = tempfile::tempdir().expect("root");
        let (runtime, definition) = runtime(root.path());
        let persona_root = definition.persona_root().expect("persona root");
        let source = root.path().join("source.jsonl");
        fs::write(&source, "{}\n").expect("source");
        let archive = archive_persona_session(&persona_root, "MentorRun", &source, false)
            .expect("archive");

        assert_eq!(
            runtime
                .resolve_read_uri("history://MentorRun")
                .expect("persona history"),
            archive,
        );
        runtime.prune_retained_jobs();
        assert!(archive.exists(), "ordinary retention must preserve persona archives");
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
        let runtime = OrchestrationRuntime::new(
            config,
            Arc::new(|_| Box::pin(async { unreachable!() })),
        )
        .expect("runtime");
        for definition in [&mentor, &reviewer] {
            let persona_root = definition.persona_root().expect("persona root");
            fs::create_dir_all(persona_root.join("sessions")).expect("sessions");
            fs::write(persona_root.join("sessions").join("Shared.jsonl"), "{}\n")
                .expect("archive");
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
        let runtime = OrchestrationRuntime::new(
            config,
            Arc::new(|_| Box::pin(async { unreachable!() })),
        )
        .expect("runtime");
        for (id, definition) in [("MentorRun", persona), ("TaskRun", ordinary)] {
            REGISTRY
                .register(
                    &runtime.inner.group_id,
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
            let entry = REGISTRY
                .get(&runtime.inner.group_id, id)
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
            runtime.agent_snapshot("MentorRun").expect("persona snapshot").history_ref.as_deref(),
            Some("history://persona/mentor/MentorRun")
        );
        assert_eq!(
            runtime.agent_snapshot("TaskRun").expect("agent snapshot").history_ref.as_deref(),
            Some("history://TaskRun")
        );
    }

    #[test]
    fn persona_archive_names_are_not_reused() {
        let root = tempfile::tempdir().expect("root");
        let (runtime, definition) = runtime(root.path());
        let persona_root = definition.persona_root().expect("persona root");
        fs::create_dir_all(persona_root.join("sessions")).expect("sessions");
        fs::write(persona_root.join("sessions").join("Mentor.jsonl"), "{}\n")
            .expect("archive");
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
        let runtime = OrchestrationRuntime::new(
            config,
            Arc::new(|_| Box::pin(async { unreachable!() })),
        )
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
        assert!(blocked.to_string().contains("destructive operation"), "{blocked:#}");
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
        assert!(second.to_string().contains("destructive operation"), "{second:#}");
    }

    #[test]
    fn persona_lifecycle_guard_rejects_active_registry_entry_without_job() {
        let root = tempfile::tempdir().expect("root");
        let (runtime, _) = runtime(root.path());
        REGISTRY
            .register(
                &runtime.inner.group_id,
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
        assert!(error.to_string().contains("active orchestration job"), "{error:#}");
    }
    #[test]
    fn persona_continuity_accumulates_across_archives() {
        let root = tempfile::tempdir().expect("root");
        let persona_root = root.path().join("persona");
        fs::create_dir_all(&persona_root).expect("persona root");

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
        archive_persona_session(&persona_root, "run-1", &first, false).expect("first archive");

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
        archive_persona_session(&persona_root, "run-2", &second, false).expect("second archive");

        let continuity = load_persona_continuity(&persona_root).expect("continuity");
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

/// English natural-language delegation verbs recognized as tokens anywhere in
/// the request. `request_has_delegation_verb` and the diagnostics candidate
/// scan share this list.
const ENGLISH_DELEGATION_VERBS: &[&str] = &[
    "have", "ask", "tell", "get", "let", "make", "please", "delegate", "assign", "spawn", "run",
    "send", "kick", "dispatch",
];

/// Chinese delegation constructions recognized by [`cjk_delegation_construction`].
/// The single-character tokens (`让`/`请`/`叫`/`派`) must directly abut the
/// agent name or follow it with exactly one ASCII space (`让 mentor 审查…`);
/// the two-character tokens (`安排`/`委托`/`交给`) end immediately before it.
const CJK_DELEGATION_TOKENS: &[&str] = &["让", "请", "叫", "派", "安排", "委托", "交给"];

/// True when the request uses an explicit natural-language delegation
/// construction: a recognized English delegation verb token anywhere in the
/// request, or a conservative CJK construction where a Chinese delegation
/// token directly abuts the agent name (or precedes it with exactly one ASCII
/// space) and a non-trivial action clause follows it. Informational mentions
/// ("researcher 是做什么的？", "我在文档里看到researcher") return false even
/// when they name the agent exactly.
#[cfg(test)]
#[must_use]
fn delegation_intent(request: &str, agent_name: &str) -> bool {
    request_has_delegation_verb(request) || cjk_delegation_construction(request, agent_name)
}

/// True when the request uses an explicit English natural-language delegation
/// verb token (whitespace/`-`/`_` separated). Used to gate parent prompt-time
/// auto-spawn; the task tool does not require it.
#[must_use]
fn request_has_delegation_verb(request: &str) -> bool {
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
        .any(|token| ENGLISH_DELEGATION_VERBS.iter().any(|verb| token == verb))
}

/// Byte span of the first occurrence of `agent_name` in `request` with
/// non-ASCII-alphanumeric boundaries on both sides (mirrors the selector's
/// `contains_embedded_ascii_word` semantics, so `researcher` never matches
/// inside `researchers` and CJK-embedded names are found). Case-insensitive
/// for ASCII; agent names are ASCII by construction.
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

/// Conservative CJK delegation construction: a Chinese delegation token
/// (`让`/`请`/`叫`/`派`/`安排`/`委托`/`交给`) directly abuts the agent name
/// (`你让researcher…`) or precedes it with exactly one ASCII space
/// (`让 mentor 审查…`), and a non-trivial action clause follows the WHOLE
/// conjunction chain (`你让glm和grok调研…` — `你让glm和grok` with no action is
/// not a delegation). Negated delegations (`不要让glm调研`, `别让grok调研`)
/// are not delegations. This is deliberately stricter than the English token
/// path: `请 review the security patch` is not a delegation to an agent named
/// `review`, and `请使用research技能` names a skill, not an agent. Returns
/// the lowercase raw names of the delegation clause (the matched name plus
/// every name joined to it by CJK conjunctions), or `None`. NFKC+lowercase
/// parity with the selector scan: `让ｇｌｍ调研` matches `glm` exactly like
/// `让glm调研`.
#[must_use]
fn cjk_delegation_chain(request: &str, agent_name: &str) -> Option<Vec<String>> {
    cjk_delegation_matching(request, agent_name, false)
}

/// Negated projection of [`cjk_delegation_matching`]: the conjunction chain
/// of an explicitly NEGATED CJK delegation (`别让glm调研`), used by the
/// negated-intent probe. The action-clause gate does not apply — the
/// negation itself is the signal.
#[must_use]
fn cjk_delegation_chain_negated(request: &str, agent_name: &str) -> Option<Vec<String>> {
    cjk_delegation_matching(request, agent_name, true)
}

/// Shared CJK delegation parser. The positive projection (false) yields the
/// chain when a delegation token abuts the name and an action clause follows
/// the WHOLE conjunction chain, without negation; the negated projection
/// (true) yields the chain when the construction is explicitly negated. Both
/// run on the NFKC-lowercased request so fullwidth forms match identically.
#[must_use]
fn cjk_delegation_matching(
    request: &str,
    agent_name: &str,
    expected_negated: bool,
) -> Option<Vec<String>> {
    let normalized = normalized_lower(request);
    let (span_start, span_end) = agent_name_span(&normalized, &normalized_lower(agent_name))?;
    let before = &normalized[..span_start];
    // The token must directly abut the name (`你让researcher…`) or precede it
    // with exactly one ASCII space (`让 mentor 审查这次修改`); two or more
    // spaces stay non-delegations (conservative).
    if !cjk_delegation_token_hit(before) {
        return None;
    }
    // Negation in the trailing clause before the name (`不要让glm调研`,
    // `别让grok调研`).
    let negated = cjk_clause_is_negated(trailing_cjk_clause(before));
    if negated != expected_negated {
        return None;
    }
    // Walk the CJK conjunction chain so the action clause is required AFTER
    // the LAST joined name: `你让glm和grok` names the pair but delegates no
    // work, while `你让glm和grok调研` delegates both.
    let runs = ascii_identifier_runs(&normalized);
    let (index, (head_run, _)) = runs
        .iter()
        .enumerate()
        .find(|(_, (_, start))| *start == span_start)?;
    let mut chain = vec![head_run.to_ascii_lowercase()];
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
        chain.push(next_run.to_ascii_lowercase());
        chain_end = next_start + next_run.len();
    }
    if expected_negated {
        return Some(chain);
    }
    let after = &normalized[chain_end..];
    // If the clause opens with a determiner/preposition (English), the name is
    // the clause's object ("请review the security patch"), not a delegated
    // agent — reject it like the English candidate scan does.
    let clause_open = after
        .trim_start_matches(|c: char| {
            c.is_whitespace()
                || c.is_ascii_punctuation()
                || matches!(
                    c,
                    '。' | '？' | '！' | '，' | '、' | '；' | '：' | '…' | '～' | '「' | '」'
                        | '『' | '』' | '（' | '）' | '《' | '》'
                )
        })
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect::<String>();
    if !clause_open.is_empty() && follows_without_action(&clause_open.to_ascii_lowercase()) {
        return None;
    }
    // An action clause must follow: at least two characters that are neither
    // whitespace nor punctuation (e.g. 仔细调研 in the literal prompt). A bare
    // name, trailing punctuation, or a possessive marker (的) does not count.
    let mut clause_chars = 0;
    for character in after.chars() {
        if character == '的' {
            break;
        }
        if character.is_whitespace()
            || character.is_ascii_punctuation()
            || matches!(
                character,
                '。' | '？' | '！' | '，' | '、' | '；' | '：' | '…' | '～' | '「' | '」'
                    | '『' | '』' | '（' | '）' | '《' | '》'
            )
        {
            continue;
        }
        clause_chars += 1;
        if clause_chars >= 2 {
            return Some(chain);
        }
    }
    None
}

/// The CJK delegation token directly abuts the name (`你让researcher…`) or
/// precedes it with exactly one ASCII space (`让 mentor 审查这次修改`).
#[must_use]
fn cjk_delegation_token_hit(before: &str) -> bool {
    CJK_DELEGATION_TOKENS.iter().any(|token| {
        before.ends_with(token)
            || before
                .strip_suffix(' ')
                .is_some_and(|prefix| prefix.ends_with(token) && !prefix.ends_with("  "))
    })
}

/// The trailing clause of the text before a name, used for the negation
/// check. Splits on CJK AND ASCII sentence punctuation — the parser runs on
/// the NFKC-normalized request, where fullwidth `；` becomes ASCII `;`.
#[must_use]
fn trailing_cjk_clause(before: &str) -> &str {
    before
        .rsplit(|c: char| {
            matches!(
                c,
                '。' | '；' | '，' | '、' | '！' | '？' | '…' | '.' | ';' | ',' | '!' | '?'
            )
        })
        .next()
        .unwrap_or(before)
}

/// True when a conservative CJK delegation construction targets `agent_name`
/// (see [`cjk_delegation_chain`]).
#[must_use]
fn cjk_delegation_construction(request: &str, agent_name: &str) -> bool {
    cjk_delegation_chain(request, agent_name).is_some()
}

/// CJK negation markers that make a delegation construction an instruction
/// NOT to delegate (`不要让glm调研`, `别让grok调研`).
const CJK_DELEGATION_NEGATIONS: &[&str] = &["不要", "不用", "不必", "勿", "莫"];

/// True when the trailing clause before a CJK delegation token negates the
/// delegation. The multi-character markers match anywhere. A trailing `别`
/// is also a negation unless it belongs to a known non-negating compound:
/// `你别让…` and `请别让…` are denied, while `特别让…`, `分别让…`, and
/// `个别让…` retain their ordinary meanings.
#[must_use]
fn cjk_clause_is_negated(clause: &str) -> bool {
    if CJK_DELEGATION_NEGATIONS
        .iter()
        .any(|marker| clause.contains(marker))
    {
        return true;
    }
    clause.char_indices().any(|(index, character)| {
        if character != '别' {
            return false;
        }
        !matches!(
            clause[..index].chars().next_back(),
            Some('特' | '分' | '个')
        )
    })
}

/// NFKC-lowercased form used for cross-space name comparison (the same
/// normalization the selector applies to requests).
#[must_use]
fn normalized_lower(value: &str) -> String {
    value.nfkc().flat_map(char::to_lowercase).collect()
}

/// True when the colliding name's normalized phrase participates in an
/// explicit delegation clause: the phrase as a single run (hyphenated form,
/// e.g. `research-agent`) or any of its split runs (`research` for
/// `Research Agent`) appears in the delegated run set.
#[must_use]
fn delegated_phrase_participates(delegated_runs: &[String], name: &str) -> bool {
    let lowered = normalized_lower(name);
    if delegated_runs.iter().any(|member| member == &lowered) {
        return true;
    }
    lowered
        .split(|c: char| !c.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .any(|token| delegated_runs.iter().any(|member| member == token))
}

/// ASCII identifier-like tokens that could name an agent: `[A-Za-z][A-Za-z0-9_-]*`
/// with 2..=80 characters (the orchestration agent-id charset).
fn looks_like_agent_name(token: &str) -> bool {
    let mut chars = token.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_alphabetic())
        && token.len() >= 2
        && token.len() <= 80
        && token.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
        })
}

/// English function words that cannot be agent names. Used to reject
/// `have the researcher …` / `ask me …` style false candidates.
fn is_english_function_word(token: &str) -> bool {
    matches!(
        token,
        "the" | "a" | "an" | "this" | "that" | "these" | "those" | "some" | "any" | "my"
            | "your" | "our" | "their" | "its" | "his" | "her" | "me" | "us" | "them"
            | "him" | "we" | "you" | "i" | "it" | "they" | "he" | "she"
    )
}

/// Tokens whose presence directly after a candidate indicates the candidate is
/// the grammatical object of the clause rather than a delegated agent
/// (`please review THE patch`, `ask writer TO write` is deliberately allowed,
/// so `to` is not listed).
fn follows_without_action(token: &str) -> bool {
    matches!(
        token,
        "the" | "a" | "an" | "this" | "that" | "these" | "those" | "my" | "your" | "our"
            | "its" | "his" | "her" | "him" | "them" | "us" | "me" | "it" | "for" | "of"
            | "in" | "on" | "at" | "with" | "by" | "from" | "and" | "or" | "but" | "is"
            | "are" | "was" | "were" | "be" | "being" | "been" | "as" | "than" | "then"
            | "there" | "here" | "so" | "just" | "only" | "also" | "too" | "very" | "really"
            | "how" | "why" | "what" | "when" | "where" | "who" | "whom" | "which" | "whose"
    )
}

/// ASCII identifier runs of `request` (the orchestration agent-id charset).
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

/// Deterministic candidate agent names referenced with explicit delegation
/// intent in `request`: CJK constructions (a Chinese delegation token abuts
/// the name and an action clause follows) and the noun directly after an
/// English delegation verb. Names joined to a candidate by a conjunction
/// (`你让missing1和missing2一起调研`, `Have missing1 and missing2 study this`)
/// are candidates too, so validation reports EVERY named agent, never just
/// the first. Function words and the verbs themselves are never candidates,
/// so `Have researcher study this` yields `researcher` while `please review
/// the security patch` and `请使用research技能` yield nothing.
fn delegation_candidates(request: &str) -> Vec<String> {
    let mut candidates = std::collections::BTreeSet::new();
    // English: the token right after a delegation verb, plus names chained
    // to it by list conjunctions (`and`/`or`) or separators (`,`/`，`/`&`):
    // `Have ghost-one and ghost-two study this` names BOTH agents, so
    // validation reports every named agent, not just the first. The chain
    // stops at the first token that is neither a conjunction nor a candidate
    // name — the action clause — so `Have writer check this` chains nothing
    // after `writer`.
    let normalized = request
        .nfkc()
        .flat_map(char::to_lowercase)
        .collect::<String>();
    let tokens = delegation_scan_tokens(&normalized);
    for (index, (token, _)) in tokens.iter().enumerate() {
        if !ENGLISH_DELEGATION_VERBS.contains(&token.as_str()) {
            continue;
        }
        let Some((next, next_start)) = tokens.get(index + 1) else {
            continue;
        };
        if !looks_like_agent_name(next)
            || is_english_function_word(next)
            || is_english_agent_conjunction(next)
        {
            continue;
        }
        // The token after the candidate must not be a determiner / pronoun /
        // preposition that turns the candidate into the clause's object
        // ("please review the patch", "ask me to …"). A list continuation
        // (conjunction or separator) after the name keeps it a candidate.
        let noun_end = next_start + next.len();
        let after = tokens.get(index + 2);
        let list_continues = after.is_some_and(|(after_token, after_start)| {
            is_english_agent_conjunction(after_token)
                || gap_has_english_list_separator(&normalized, noun_end, *after_start)
        });
        let action_follows = list_continues
            || after.is_none_or(|(after_token, _)| !follows_without_action(after_token));
        if !action_follows {
            continue;
        }
        candidates.insert(next.clone());
        // Chain the rest of the list: a name following a conjunction or a
        // separator is also delegated; the first token that is neither ends
        // the list (the action clause).
        let mut cursor = index + 2;
        let mut previous_end = noun_end;
        loop {
            let (candidate, candidate_start) = match tokens.get(cursor) {
                Some(tuple) => tuple,
                None => break,
            };
            let separated =
                gap_has_english_list_separator(&normalized, previous_end, *candidate_start);
            let conjunction = is_english_agent_conjunction(candidate);
            if !separated && !conjunction {
                break; // the action clause follows the list
            }
            if conjunction {
                cursor += 1; // `and`/`or` joins the next name
            }
            let Some((chained, chained_start)) = tokens.get(cursor) else {
                break;
            };
            if !looks_like_agent_name(chained)
                || is_english_function_word(chained)
                || is_english_agent_conjunction(chained)
            {
                break;
            }
            candidates.insert(chained.clone());
            previous_end = chained_start + chained.len();
            cursor += 1;
        }
    }
    // CJK: names abutted by a Chinese delegation token with an action clause,
    // plus names joined to them by CJK conjunctions (`和`/`跟`/`与`/`、`/`，`
    // or commas): `你让missing1和missing2一起调研` names BOTH agents, so
    // validation must report every named agent, not just the first. A
    // conjunction-free gap (an action clause, or a bare space) breaks the
    // chain, mirroring the conservative single-space CJK token rule.
    let runs = ascii_identifier_runs(request);
    let mut chained = false;
    for (index, (run, start)) in runs.iter().enumerate() {
        if !looks_like_agent_name(run) {
            chained = false;
            continue;
        }
        let direct = cjk_delegation_construction(request, run);
        let joined = chained
            && {
                let (previous, previous_start) = &runs[index - 1];
                let gap = &request[previous_start + previous.len()..*start];
                !gap.is_empty()
                    && gap
                        .chars()
                        .any(|character| is_cjk_agent_conjunction(character))
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
    candidates.into_iter().collect()
}

/// CJK conjunctions that join two delegated agent names in one clause
/// (`你让glm和grok一起调研`): the next name follows through one of these
/// without an intervening action clause.
fn is_cjk_agent_conjunction(character: char) -> bool {
    matches!(character, '和' | '跟' | '与' | '、' | '，' | ',')
}

/// NFKC-lowercased token scan for the English delegation branch: runs of
/// alphanumerics, `-` and `_` with byte offsets into `normalized` (used to
/// inspect raw separators such as commas between tokens).
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

/// English list conjunctions that join delegated agent names
/// (`Have glm and grok study this`).
fn is_english_agent_conjunction(token: &str) -> bool {
    matches!(token, "and" | "or")
}

/// True when the raw gap between two tokens contains an English list
/// separator (comma or ampersand), so `Have glm, grok study this` chains
/// even though tokenization drops the comma.
fn gap_has_english_list_separator(
    normalized: &str,
    previous_end: usize,
    next_start: usize,
) -> bool {
    normalized[previous_end..next_start]
        .chars()
        .any(|character| matches!(character, ',' | '，' | '&'))
}

/// Raw-text list of names delegated by the English delegation verbs in
/// `request` (lowercase runs, unioned across every verb occurrence): the
/// head of each verb's list is the first non-function-word run after the
/// verb, and every later run whose gap to the previous member contains a
/// list separator (`,`/`，`/`&`) or that follows a `and`/`or` run is also a
/// member. The walk stops at the first non-member run — the action clause.
/// `Have glm review grok's output` yields only `glm` (grok is the review's
/// object), and `Tell me whether glm or grok is better` yields nothing
/// (`whether` is not a function word, so it becomes the head and is never a
/// catalog mention).
fn english_delegation_list(request: &str) -> Vec<String> {
    english_delegation_list_by(request, false)
}

/// Negated projection of [`english_delegation_list_by`]: the lists of the
/// NEGATED verbs (`Do not have glm study this` yields `glm`), used by the
/// negated-intent probe.
#[must_use]
fn english_delegation_list_negated(request: &str) -> Vec<String> {
    english_delegation_list_by(request, true)
}

/// Shared English delegation list parser: the verb-list runs under
/// `expected_negated` — with `false` the non-negated verb lists (the names
/// actually delegated), with `true` the lists of negated verbs (the names
/// the user forbade). NFKC+lowercase parity with the selector scan:
/// `Have ｇｌｍ study this` == `Have glm study this`. All offsets are in the
/// normalized space.
fn english_delegation_list_by(request: &str, expected_negated: bool) -> Vec<String> {
    let normalized = normalized_lower(request);
    let runs = ascii_identifier_runs(&normalized);
    let mut members = std::collections::BTreeSet::new();
    for (verb_index, (run, verb_start)) in runs.iter().enumerate() {
        if !ENGLISH_DELEGATION_VERBS
            .iter()
            .any(|verb| run.eq_ignore_ascii_case(verb))
        {
            continue;
        }
        // A negated verb (`Do not have glm study this`, `Don't ask glm to
        // review this`) is an instruction NOT to delegate.
        let negated = english_verb_is_negated(&normalized, *verb_start);
        if negated != expected_negated {
            continue;
        }
        // Head: the first run after the verb that is not a function word.
        let mut cursor = verb_index + 1;
        let mut previous_end = 0usize;
        let mut head = None;
        while let Some((candidate, start)) = runs.get(cursor) {
            if is_english_function_word(candidate) {
                cursor += 1;
                continue;
            }
            head = Some(candidate.to_ascii_lowercase());
            previous_end = start + candidate.len();
            cursor += 1;
            break;
        }
        let Some(head) = head else {
            continue;
        };
        members.insert(head);
        // Chain: a `and`/`or` run or a separator-joined run continues the
        // list; the first token that is neither ends it (the action clause).
        loop {
            let Some((candidate, start)) = runs.get(cursor) else {
                break;
            };
            let conjunction = candidate.eq_ignore_ascii_case("and")
                || candidate.eq_ignore_ascii_case("or");
            let separated = !conjunction
                && gap_has_english_list_separator(&normalized, previous_end, *start);
            if !conjunction && !separated {
                break;
            }
            if conjunction {
                cursor += 1;
                let Some((chained, chained_start)) = runs.get(cursor) else {
                    break;
                };
                if is_english_function_word(chained) || !looks_like_agent_name(chained) {
                    break;
                }
                members.insert(chained.to_ascii_lowercase());
                previous_end = chained_start + chained.len();
                cursor += 1;
                continue;
            }
            if is_english_function_word(candidate) || !looks_like_agent_name(candidate) {
                break;
            }
            members.insert(candidate.to_ascii_lowercase());
            previous_end = start + candidate.len();
            cursor += 1;
        }
    }
    members.into_iter().collect()
}

/// True when the delegation verb starting at `verb_start` is negated in its
/// clause (`Do not have glm study this`, `Don't ask glm to review this`): an
/// instruction NOT to delegate. Checks the trailing clause before the verb
/// for negation tokens (`not`/`never`/`dont`/`doesnt`/`didnt`/`cannot`/
/// `cant`/`wont`) or a contracted `n't`.
fn english_verb_is_negated(request: &str, verb_start: usize) -> bool {
    let before = &request[..verb_start];
    let clause = before
        .rsplit(|c: char| {
            matches!(
                c,
                '.' | ';' | '!' | '?' | ',' | '。' | '；' | '！' | '？' | '、' | '，'
            )
        })
        .next()
        .unwrap_or(before)
        .to_ascii_lowercase();
    let negated_token = clause
        .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
        .filter(|token| !token.is_empty())
        .any(|token| {
            matches!(
                token,
                "not" | "never" | "dont" | "doesnt" | "didnt" | "cannot" | "cant" | "wont"
            )
        });
    negated_token || clause.contains("n't")
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

/// Per-line preview cap for `hub read_history` transcript labels, mirroring the
/// tree-panel label style (prefix + first-line preview) with a slightly larger
/// allowance than the TUI's 60-char row cap so a coach can read useful context.
const HISTORY_LABEL_MAX_CHARS: usize = 120;

/// Renders a session transcript file into compact single-line labels, keeping
/// the last `lines` FLAT rows. Labels are flattened into rows before the tail
/// so a message that projects several rows (an assistant turn with multiple
/// tool calls) cannot smuggle more than `lines` rows past the bound — the
/// `lines` contract counts rendered lines, not messages. The file is either a
/// durable child JSONL (session records) or the settle-time `.history.json`
/// snapshot (a JSON array of [`pi_ai::Message`]); the format is sniffed from
/// the first non-whitespace byte.
fn render_history_file(path: &Path, lines: usize) -> Result<String> {
    let data = fs::read(path).with_context(|| format!("reading history {}", path.display()))?;
    if data.iter().copied().find(|byte| !byte.is_ascii_whitespace()) == Some(b'[') {
        let messages: Vec<pi_ai::Message> = serde_json::from_slice(&data)
            .with_context(|| format!("parsing history snapshot {}", path.display()))?;
        let rendered = messages
            .iter()
            .flat_map(|message| history_message_label(message).lines().map(str::to_owned).collect::<Vec<_>>())
            .collect::<Vec<_>>();
        let start = rendered.len().saturating_sub(lines);
        return Ok(rendered[start..].join("\n"));
    }
    let tree = crate::session_store::load_session_tree(path)
        .with_context(|| format!("parsing session transcript {}", path.display()))?;
    let rendered = tree
        .entries
        .iter()
        .filter(|entry| entry.entry_type != "label" && entry.entry_type != "checkpoint")
        .flat_map(|entry| history_entry_label(entry).lines().map(str::to_owned).collect::<Vec<_>>())
        .collect::<Vec<_>>();
    let start = rendered.len().saturating_sub(lines);
    Ok(rendered[start..].join("\n"))
}

/// Compact single-line labels for one transcript message, mirroring the
/// tree-panel style: a type prefix plus a first-line preview. Assistant
/// messages additionally project one readable line per tool call — the
/// concrete action (target/selector/command/operation) — so a rendered
/// history never reduces a child's work to generic `[tool: <name>]` tags;
/// tool results stay tag-only with a reliably-known `· ok`/`· error` outcome
/// so their output never floods the rendering.
fn history_message_label(message: &pi_ai::Message) -> String {
    let mut lines = Vec::new();
    match message {
        pi_ai::Message::User(user) => {
            lines.push(format!("user: {}", first_line(&content_list_text(&user.content))));
        }
        pi_ai::Message::Assistant(assistant) => {
            let text = assistant.text();
            if !text.trim().is_empty() {
                lines.push(format!("assistant: {}", first_line(&text)));
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
            // Output is never previewed; only the tool and its outcome. The
            // `is_error` flag is authoritative, so nothing is fabricated.
            let outcome = if result.is_error { " · error" } else { " · ok" };
            lines.push(format!("[tool: {}]{outcome}", result.tool_name));
        }
        pi_ai::Message::BashExecution(bash) => {
            lines.push(format!("[bash] {}", first_line(&bash.command)));
        }
        pi_ai::Message::Custom(custom) => {
            lines.push(format!("custom: {}", first_line(&custom_content_text(&custom.content))));
        }
        pi_ai::Message::BranchSummary(summary) => {
            lines.push(format!("[branch summary] {}", first_line(&summary.summary)));
        }
        pi_ai::Message::CompactionSummary(summary) => {
            lines.push(format!("[compaction] {}", first_line(&summary.summary)));
        }
    }
    lines
        .into_iter()
        .map(|line| truncate_label(line.trim_end(), HISTORY_LABEL_MAX_CHARS))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Compact human-facing summary of one assistant tool call for history
/// labels. Reuses [`crate::tool_presentation::compact_tool_arguments`] so the
/// redaction rules stay a single source of truth — no second secret-filtering
/// implementation here: file tools surface their repo-relative
/// target/selector, `bash` its first concrete command, planning/messaging
/// tools their operation + target, and unknown tools a safe compact digest
/// (never raw JSON). Absolute machine paths never leak: a bare absolute
/// path/selector shrinks to its final component, and absolute path tokens
/// embedded anywhere in a command or argument digest are redacted to `[PATH]`
/// (http/https URLs are preserved as targets).
fn tool_call_summary(tool: &pi_ai::ToolCall) -> String {
    let compact = crate::tool_presentation::compact_tool_arguments(&tool.arguments);
    let digest = compact.split_whitespace().collect::<Vec<_>>().join(" ");
    let digest = if is_absolute_path(&digest) && !is_http_url(&digest) {
        digest.rsplit(['/', '\\']).next().unwrap_or(&digest).to_owned()
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

/// Redact absolute machine-path tokens (Unix, home-relative, Windows drive)
/// from a tool-call digest, keeping any `key=` prefix readable and preserving
/// http/https URLs. Used ONLY for path redaction; secret shapes stay with the
/// shared redactor ([`crate::tool_presentation::compact_tool_arguments`]).
fn redact_absolute_path_tokens(value: &str) -> String {
    value
        .split_whitespace()
        .map(|token| {
            if is_http_url(token) {
                return token.to_owned();
            }
            // A key=value digest carries the path after the last '='.
            let candidate = token.rsplit('=').next().unwrap_or(token);
            if is_absolute_path(candidate) {
                if candidate.len() == token.len() {
                    "[PATH]".to_owned()
                } else {
                    format!("{}[PATH]", &token[..token.len() - candidate.len()])
                }
            } else {
                token.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
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
            entry.content.as_ref().map(custom_content_text).unwrap_or_default(),
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
            format!("[thinking: {}]", entry.thinking_level.as_deref().unwrap_or_default()),
            String::new(),
        ),
        "session_info" => (
            format!("[title: {}]", entry.name.as_deref().unwrap_or_default()),
            String::new(),
        ),
        "compaction" => ("[compaction] ".to_owned(), entry.summary.clone().unwrap_or_default()),
        "branch_summary" => {
            ("[branch summary] ".to_owned(), entry.summary.clone().unwrap_or_default())
        }
        other => (format!("[{other}]"), String::new()),
    };
    let preview = first_line(&body);
    let composed = if preview.is_empty() { prefix } else { format!("{prefix}{preview}") };
    truncate_label(composed.trim_end(), HISTORY_LABEL_MAX_CHARS)
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
            tool_call("bash", serde_json::json!({ "command": "cargo build --release" })),
            tool_call("hub", serde_json::json!({ "op": "send", "to": "worker", "message": "go" })),
            tool_call("yield", serde_json::json!({ "text": "deliverable done" })),
        ]);
        let label = history_message_label(&message);
        // Assistant text is preserved alongside the projected calls.
        assert!(label.contains("assistant: checking the seed"), "{label}");
        assert!(label.contains("assistant · read seed.txt"), "{label}");
        assert!(label.contains("assistant · bash cargo build --release"), "{label}");
        // Planning/messaging tools surface operation + target key fields.
        assert!(label.contains("assistant · hub message=go, op=send, to=worker"), "{label}");
        // Unknown tools get a safe compact digest, never raw JSON.
        assert!(label.contains("assistant · yield text=deliverable done"), "{label}");
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
        assert!(!bash.contains('\n'), "command must collapse to one line: {bash}");
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
        assert_eq!(leaky, "bash cat <workspace>/secrets/[REDACTED].txt", "{leaky}");
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

    #[test]
    fn tool_result_labels_carry_ok_error_outcome_without_output() {
        let ok = history_message_label(&pi_ai::Message::ToolResult(pi_ai::ToolResultMessage {
            tool_call_id: "call-1".to_owned(),
            tool_name: "read".to_owned(),
            content: vec![pi_ai::ContentBlock::text("raw output that must not appear")],
            usage: None,
            details: None,
            added_tool_names: Vec::new(),
            is_error: false,
            timestamp: 0,
        }));
        assert_eq!(ok, "[tool: read] · ok", "{ok}");

        let failed = history_message_label(&pi_ai::Message::ToolResult(pi_ai::ToolResultMessage {
            tool_call_id: "call-2".to_owned(),
            tool_name: "bash".to_owned(),
            content: vec![pi_ai::ContentBlock::text("boom")],
            usage: None,
            details: None,
            added_tool_names: Vec::new(),
            is_error: true,
            timestamp: 0,
        }));
        assert_eq!(failed, "[tool: bash] · error", "{failed}");
    }

    #[test]
    fn history_label_lines_stay_under_the_per_line_cap() {
        let long = "x".repeat(400);
        let message = assistant(vec![
            tool_call("read", serde_json::json!({ "path": long.clone() })),
            tool_call("bash", serde_json::json!({ "command": long })),
        ]);
        let label = history_message_label(&message);
        assert!(label.lines().all(|line| line.chars().count() <= HISTORY_LABEL_MAX_CHARS), "{label}");
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
        let rendered = render_history_file(&path, 2).expect("render");
        assert_eq!(rendered.lines().count(), 2, "lines bound must count flat rows: {rendered}");
        assert!(rendered.contains("cargo check"), "last two flat rows kept: {rendered}");
        assert!(rendered.contains("read b.txt"), "last two flat rows kept: {rendered}");
        assert!(!rendered.contains("a.txt"), "older rows dropped: {rendered}");
        assert!(!rendered.contains("line one"), "older rows dropped: {rendered}");
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
        let Some(persona_root) = definition.persona_root() else {
            return self.allocate_unique_agent_id(requested, reserved);
        };
        validate_agent_id(requested)?;
        let mut suffix = 1u32;
        loop {
            let candidate = if suffix == 1 {
                requested.to_owned()
            } else {
                format!("{requested}_{suffix}")
            };
            validate_agent_id(&candidate)?;
            let archive_exists = match fs::symlink_metadata(persona_archive_path(
                &persona_root,
                &candidate,
            )?) {
                Ok(_) => true,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("reading persona archive metadata for {candidate:?}")
                    });
                }
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
    REGISTRY
        .register(
            &runtime.inner.group_id,
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
    fn delivered_message_log_survives_mailbox_consumption() {
        // Every durably delivered group message (subagent ⇄ subagent
        // included) lands in the bounded delivered log that the workflow
        // page's Recent IRC reads; draining the recipient's mailbox must not
        // erase it, so consumed messages stay visible.
        let root = tempfile::tempdir().expect("root");
        let runtime = prompt_runtime(root.path());
        // "Main" is pre-registered by the runtime itself.
        register_peer(&runtime, "WorkerA", "task", Some("Main"), AgentStatus::Parked);
        register_peer(&runtime, "WorkerB", "task", Some("Main"), AgentStatus::Parked);

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
        assert_eq!(runtime.inbox("WorkerB", false).len(), 1);
        assert_eq!(runtime.inbox("WorkerB", true).len(), 0);
        assert_eq!(runtime.delivered_messages().len(), 3, "consumption must not erase the log");
        assert!(runtime.delivered_messages().iter().any(|message| message.body == "results ready"));
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
            .child_system_prompt(definition, "work", None, None, None, &roster)
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
            soft_budget_exhausted: false,
            structured_output: None,
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
        let mut config = OrchestrationConfig::new(
            AgentCatalog::from_agents(vec![definition]),
            root,
        );
        config.soft_budget = budget;
        let runtime = OrchestrationRuntime::new(
            config,
            Arc::new(|_| Box::pin(async { unreachable!() })),
        )
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
        assert!(state.triggered.load(Ordering::Relaxed), "budget marks the job");

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
        assert!(!token_hook(&budget_turn(100)), "100 tokens stay under the cap");
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
}

#[cfg(test)]
mod delegation_intent_tests {
    use super::*;

    #[test]
    fn english_delegation_verbs_are_recognized() {
        for prompt in [
            "Have researcher study this",
            "please ask researcher to investigate",
            "let researcher handle the patch",
            "dispatch researcher now",
        ] {
            assert!(request_has_delegation_verb(prompt), "{prompt}");
            assert!(delegation_intent(prompt, "researcher"), "{prompt}");
        }
    }

    #[test]
    fn literal_cjk_prompt_has_delegation_intent() {
        assert!(delegation_intent(
            "你让researcher仔细调研pi-coding-agent",
            "researcher"
        ));
    }

    #[test]
    fn cjk_constructions_are_conservative() {
        for (prompt, agent) in [
            ("请你让researcher去调查这个项目", "researcher"),
            ("请researcher写一份调研报告", "researcher"),
            ("把这项调研交给researcher完成", "researcher"),
            ("安排researcher仔细调研这个仓库", "researcher"),
            ("委托researcher调研这个仓库", "researcher"),
            ("叫researcher去研究这个bug", "researcher"),
            ("派researcher去处理这个任务", "researcher"),
            // Spaced single-char-token form: exactly one ASCII space between
            // the CJK delegation token and the agent name is a delegation.
            ("让 mentor 审查这次修改", "mentor"),
            ("请 researcher 去调查", "researcher"),
            ("叫 writer 检查这段代码", "writer"),
            // The action clause must follow the WHOLE conjunction chain:
            // `你让glm和grok调研` delegates both names.
            ("你让glm和grok调研这个仓库", "glm"),
            // `别` inside a word (`特别让…` = "especially let …") is not a
            // negation marker.
            ("特别让researcher仔细调研", "researcher"),
        ] {
            assert!(
                cjk_delegation_construction(prompt, agent),
                "{prompt:?} must be a CJK delegation construction"
            );
        }
        // Negatives: informational mentions, questions, possessive markers,
        // whitespace-separated English after a CJK token, double spaces
        // after the token (only a single space is accepted), a conjunction
        // chain WITHOUT a following action clause (`你让glm和grok` names the
        // pair but delegates no work), and negations (`不要让…`, `别让…`).
        for (prompt, agent) in [
            ("researcher 是做什么的？", "researcher"),
            ("我在文档里看到researcher", "researcher"),
            ("让researcher的调研完成", "researcher"),
            ("请review the security patch", "review"),
            ("请使用research技能", "research"),
            ("让  mentor 审查这次修改", "mentor"),
            ("请  researcher 去调查", "researcher"),
            ("你让glm和grok", "glm"),
            ("不要让glm调研这个仓库", "glm"),
            ("别让missing1调研", "missing1"),
            ("你别让missing1调研", "missing1"),
            ("请别让missing1调研", "missing1"),
        ] {
            assert!(
                !cjk_delegation_construction(prompt, agent),
                "{prompt:?} must NOT be a CJK delegation construction"
            );
        }
    }

    #[test]
    fn space_separated_cjk_delegation_routes_persona_through_catalog_selector() {
        // A Persona-kind definition (durable root under personas/mentor) is
        // routed exactly like any agent: `让 mentor 审查这次修改` with NO
        // explicit agent resolves to mentor through the existing catalog/
        // selector — the delegated agent is the persona itself, never a
        // frontend heuristic.
        let root = tempfile::tempdir().expect("root");
        let persona_root = root.path().join("personas").join("mentor");
        fs::create_dir_all(&persona_root).expect("persona root");
        let definition = super::super::definitions::parse_persona_definition(
            &persona_root.join("persona.md"),
            "---\nname: mentor\ndescription: mentor\n---\nprompt",
            super::super::AgentDefinitionSource::User,
            true,
        )
        .expect("persona definition");
        assert!(definition.is_persona(), "fixture must be kind Persona");
        let mut config = OrchestrationConfig::new(
            AgentCatalog::from_agents(vec![definition.clone()]),
            root.path().join("artifacts"),
        );
        config.default_agent = "mentor".to_owned();
        config.parent_model = pi_ai::Model::default();
        let runtime = OrchestrationRuntime::new(
            config,
            Arc::new(|_| Box::pin(async { unreachable!() })),
        )
        .expect("runtime");

        let assignment = "让 mentor 审查这次修改";
        // The spaced CJK construction is delegation intent for the persona.
        assert!(delegation_intent(assignment, "mentor"), "{assignment}");
        // The exact-mention scan finds the persona name in the prompt.
        assert!(matches!(
            runtime.exact_agent_mention_in_catalog(assignment),
            crate::selector::ExactAgentMention::Unique(name) if name == "mentor"
        ));
        // The catalog/selector routes the NL task to the persona WITHOUT an
        // explicit agent, and the persona is spawn-selectable.
        assert_eq!(
            runtime.resolve_task_agent(assignment, None).expect("resolve"),
            "mentor"
        );
        assert!(
            runtime
                .enabled_agents()
                .iter()
                .any(|agent| agent.name == "mentor" && agent.is_persona()),
            "the persona must be enabled for spawning"
        );
        // The informational-mention guard still applies to the persona: a
        // question about it is not a delegation.
        assert!(!delegation_intent("mentor 是做什么的？", "mentor"));
    }

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
}

#[cfg(test)]
mod yield_tool_tests {
    use super::*;
    use crate::orchestration::tools::yield_tool;
    use crate::SessionOptions;
    use pi_ai::{
        AssistantMessage, AssistantMessageEvent, ContentBlock, Model, SimpleStreamOptions,
        StopReason, ToolCall, new_assistant_message_event_stream,
    };
    use serde_json::json;

    /// Scripted provider: pops one assistant message per stream invocation so
    /// the turn count is deterministic; an exhausted script falls back to a
    /// plain "done" message, so any turn streamed after the scripted ones is
    /// observable in the transcript.
    fn scripted(messages: Vec<AssistantMessage>) -> pi_agent::StreamFn {
        let messages = Arc::new(parking_lot::Mutex::new(VecDeque::from(messages)));
        Arc::new(move |model: Model, _context: pi_ai::Context, _options: SimpleStreamOptions| {
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
        })
    }

    fn task_definition() -> AgentDefinition {
        super::super::definitions::parse_agent_definition(
            Path::new("task.md"),
            "---\nname: task\ndescription: task\n---\nprompt",
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
    fn yield_declaring_runtime(
        root: &Path,
        messages: Vec<AssistantMessage>,
    ) -> OrchestrationRuntime {
        let session = Session::new(SessionOptions {
            model: Model::default(),
            cwd: root.to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: Some(scripted(messages)),
            auth_resolver: None,
        })
        .expect("parent session");
        let snapshot = session.child_session_options_snapshot();
        let catalog = AgentCatalog::from_agents(vec![task_definition(), yield_declaring_definition()]);
        let config = OrchestrationConfig::new(catalog, root.join("artifacts"));
        OrchestrationRuntime::new(
            config,
            OrchestrationRuntime::child_factory_from_snapshot(snapshot),
        )
        .expect("runtime")
    }

    /// Runtime whose child factory builds real sessions from a parent session
    /// carrying the scripted provider — the exact production path, so the
    /// child's tool set includes the orchestration plumbing plus the appended
    /// `yield` tool.
    fn yield_runtime(
        root: &Path,
        messages: Vec<AssistantMessage>,
        soft_budget: JobSoftBudget,
    ) -> OrchestrationRuntime {
        let session = Session::new(SessionOptions {
            model: Model::default(),
            cwd: root.to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: Some(scripted(messages)),
            auth_resolver: None,
        })
        .expect("parent session");
        let snapshot = session.child_session_options_snapshot();
        let mut config = OrchestrationConfig::new(
            AgentCatalog::from_agents(vec![task_definition()]),
            root.join("artifacts"),
        );
        config.soft_budget = soft_budget;
        OrchestrationRuntime::new(
            config,
            OrchestrationRuntime::child_factory_from_snapshot(snapshot),
        )
        .expect("runtime")
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
        let runtime = yield_runtime(
            root.path(),
            vec![yield_call_message("deliverable")],
            JobSoftBudget::default(),
        );
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
        let runtime = yield_runtime(
            root.path(),
            vec![text_message("natural final text")],
            JobSoftBudget::default(),
        );
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
        let runtime = yield_runtime(
            root.path(),
            vec![yield_call_message("budgeted deliverable")],
            JobSoftBudget {
                max_requests: Some(1),
                ..JobSoftBudget::default()
            },
        );
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
        assert!(!crate::TOOL_NAMES.contains(&"yield"), "yield must not be a built-in");
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
        let runtime = yield_runtime(
            root.path(),
            vec![yield_call_message(&payload)],
            JobSoftBudget::default(),
        );
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
        assert!(history.contains(secret.as_str()), "the raw transcript keeps the payload");

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
        let redacted_arguments =
            crate::redact_value(&serde_json::json!({ "text": payload }));
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
        let runtime =
            yield_declaring_runtime(root.path(), vec![yield_call_message("declared-yield")]);
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
        assert!(result.error.is_none(), "no spawn/tool errors: {:?}", result.error);
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
        let runtime = yield_runtime(
            root.path(),
            vec![yield_call_message(r#"{"ok": true}"#)],
            JobSoftBudget::default(),
        );
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
        assert!(result.error.is_none(), "conforming payload must not fail: {:?}", result.error);
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
        let runtime = yield_runtime(
            root.path(),
            vec![yield_call_message(r#"{"ok": "nope"}"#)],
            JobSoftBudget::default(),
        );
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

        assert_eq!(job.status, JobStatus::Completed, "permissive mode keeps the job completed");
        let result = job.result.expect("settled result");
        assert!(result.error.is_none(), "permissive mode must not fail the job: {:?}", result.error);
        let validation = result.structured_output.expect("structured output");
        assert!(!validation.valid, "non-conforming payload must be flagged");
        assert_eq!(validation.schema_mode, "permissive");
        assert_eq!(validation.data, Some(json!({"ok": "nope"})));
        assert!(
            validation.error.as_deref().is_some_and(|error| error.contains("outputSchema")),
            "validation error: {:?}",
            validation.error
        );
    }

    #[tokio::test]
    async fn strict_output_schema_failure_surfaces_as_job_error() {
        let root = tempfile::tempdir().expect("root");
        let runtime = yield_runtime(
            root.path(),
            vec![yield_call_message("not json at all")],
            JobSoftBudget::default(),
        );
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
            result.error.as_deref().is_some_and(|error| error.contains("not valid JSON")),
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
            second.contains("<delegated_assignment>\nsecond task briefing\n</delegated_assignment>"),
            "second child's own task: {second}"
        );
        // The context is not duplicated into the assignment.
        assert!(!first.contains("shared background context\n\nfirst task briefing"));
        // The output contract is rendered only for the item that carries it.
        assert!(first.contains("<output_contract>"), "first child sees its contract: {first}");
        assert!(first.contains("Validation mode: strict"));
        assert!(!second.contains("<output_contract>"), "no contract for the second child: {second}");
    }
}

#[cfg(test)]
mod unknown_tool_declaration_tests {
    use super::*;
    use crate::SessionOptions;
    use pi_ai::{
        AssistantMessage, AssistantMessageEvent, ContentBlock, Model, SimpleStreamOptions,
        StopReason, new_assistant_message_event_stream,
    };

    /// Stream function that records the exact tool set injected into the
    /// child session (the `Context.tools` the provider sees) and then
    /// completes with a plain "done" message so the run settles cleanly.
    fn recording_stream(captured: Arc<Mutex<Vec<String>>>) -> pi_agent::StreamFn {
        Arc::new(move |model: Model, context: pi_ai::Context, _options: SimpleStreamOptions| {
            *captured.lock() = context
                .tools
                .iter()
                .map(|tool| tool.name.clone())
                .collect();
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
        })
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
            "---\nname: task\ndescription: task\n---\nprompt",
            super::super::AgentDefinitionSource::Bundled,
            true,
        )
        .expect("task definition")
    }

    /// Runtime whose catalog carries the qa-style definition (declaring the
    /// ghost `yield_output` plus valid tools) and a plain task definition.
    /// The child factory is the real production path; the recording stream fn
    /// captures each child's injected tool set.
    fn qa_runtime(
        root: &Path,
        qa_tools: &[&str],
        captured: Arc<Mutex<Vec<String>>>,
        max_tools_per_agent: usize,
    ) -> OrchestrationRuntime {
        let session = Session::new(SessionOptions {
            model: Model::default(),
            cwd: root.to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: Some(recording_stream(captured)),
            auth_resolver: None,
        })
        .expect("parent session");
        let snapshot = session.child_session_options_snapshot();
        let mut config = OrchestrationConfig::new(
            AgentCatalog::from_agents(vec![task_definition(), qa_definition(qa_tools)]),
            root.join("artifacts"),
        );
        config.max_tools_per_agent = max_tools_per_agent;
        OrchestrationRuntime::new(
            config,
            OrchestrationRuntime::child_factory_from_snapshot(snapshot),
        )
        .expect("runtime")
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
        let ids = spawns.iter().map(|spawn| spawn.job_id.clone()).collect::<Vec<_>>();
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
        let runtime = qa_runtime(
            root.path(),
            &["read", "yield_output", "ghost"],
            captured.clone(),
            16,
        );
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
            !injected.iter().any(|name| name == "yield_output" || name == "ghost"),
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
        let runtime = qa_runtime(
            root.path(),
            &["read", "grep", "bash", "write", "yield_output"],
            captured.clone(),
            16,
        );
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
        let runtime = qa_runtime(
            root.path(),
            &["read", "yield_output"],
            captured.clone(),
            16,
        );
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
        let runtime = qa_runtime(
            root.path(),
            &["read", "yield_output", "ghost_a", "ghost_b"],
            captured.clone(),
            1,
        );
        let spawn = spawn_item(&runtime, "QaChild", "qa");
        settle(&runtime, &[spawn]).await;

        let injected = captured.lock().clone();
        assert_eq!(
            injected.iter().filter(|name| *name == "read").count(),
            1,
            "exactly one declared tool is injected: {injected:?}"
        );
        assert!(
            !injected.iter().any(|name| name == "yield_output" || name == "ghost_a" || name == "ghost_b"),
            "unknowns never count toward or occupy the tool budget: {injected:?}"
        );
    }

    #[tokio::test]
    async fn definition_declaring_only_unknowns_loads_with_zero_injected_tools() {
        // A definition declaring ONLY unknown names stays compatible and
        // spawns; no declared tool is injected (plumbing + yield remain).
        let root = tempfile::tempdir().expect("root");
        let captured = Arc::new(Mutex::new(Vec::<String>::new()));
        let runtime = qa_runtime(root.path(), &["yield_output", "computer"], captured.clone(), 16);
        let spawn = spawn_item(&runtime, "GhostChild", "qa");
        settle(&runtime, &[spawn]).await;

        let injected = captured.lock().clone();
        assert!(
            !injected.iter().any(|name| name == "yield_output" || name == "computer"),
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
}

#[cfg(test)]
mod memory_child_factory_tests {
    use super::*;
    use crate::SessionOptions;
    use pi_ai::{AssistantMessage, AssistantMessageEvent, ContentBlock, Model, SimpleStreamOptions, StopReason, new_assistant_message_event_stream};

    fn recording_stream(captured: Arc<Mutex<Vec<String>>>) -> pi_agent::StreamFn {
        Arc::new(move |model: Model, context: pi_ai::Context, _options: SimpleStreamOptions| {
            *captured.lock() = context.tools.iter().map(|tool| tool.name.clone()).collect();
            Box::pin(async move {
                let mut done = AssistantMessage::pending(&model);
                done.content = vec![ContentBlock::text("done")];
                done.stop_reason = StopReason::Stop;
                let stream = new_assistant_message_event_stream();
                let producer = stream.clone();
                let model = model.clone();
                tokio::spawn(async move {
                    producer.push(AssistantMessageEvent::Start { partial: AssistantMessage::pending(&model) }).await;
                    producer.push(AssistantMessageEvent::Done { reason: StopReason::Stop, message: done.clone() }).await;
                    producer.end(Some(done)).await;
                });
                stream
            })
        })
    }

    fn definition(root: &Path, persona: bool, tools: Option<&str>) -> AgentDefinition {
        let path = if persona { root.join("reviewer").join("persona.md") } else { root.join("task.md") };
        if let Some(parent) = path.parent() { std::fs::create_dir_all(parent).expect("definition parent"); }
        let tools = tools.map(|tools| format!("tools: [{tools}]\n")).unwrap_or_default();
        let mut definition = super::super::definitions::parse_agent_definition(
            &path,
            &format!("---\nname: test\ndescription: test\n{tools}---\nprompt"),
            super::super::AgentDefinitionSource::User,
            true,
        ).expect("definition");
        if persona { definition.kind = super::super::definitions::AgentDefinitionKind::Persona; }
        definition
    }

    fn parent_snapshot(root: &Path, captured: Arc<Mutex<Vec<String>>>, config: crate::MemoryConfig) -> crate::ChildSessionOptionsSnapshot {
        let session = Session::new(SessionOptions {
            model: Model::default(), cwd: root.to_path_buf(), system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off, api_key: String::new(), compaction: None,
            stream_options: Default::default(), tools: Some(Vec::new()), before_tool_call: None,
            after_tool_call: None, stream_fn: Some(recording_stream(captured)), auth_resolver: None,
        }).expect("parent session");
        let mut snapshot = session.child_session_options_snapshot();
        snapshot.memory = Some(Arc::new(move || Some(config.clone())));
        snapshot
    }

    fn request(definition: AgentDefinition, requested: Option<Vec<String>>) -> ChildSessionRequest {
        ChildSessionRequest {
            child_id: "Child".to_owned(), parent_id: "Main".to_owned(), max_tools_per_agent: 32,
            depth: 1, definition, assignment: "work".to_owned(), system_prompt: String::new(),
            requested_tool_names: requested, orchestration_tools: Vec::new(), thinking_level: None,
            model: Model::default(), output_schema: None, schema_mode: None,
            yield_state: Arc::new(YieldState::default()),
        }
    }

    #[tokio::test]
    async fn ordinary_and_persona_children_inherit_hindsight_for_default_and_explicit_memory_requests() {
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
            let factory = OrchestrationRuntime::child_factory_from_snapshot(parent_snapshot(root.path(), captured.clone(), config));
            let child = factory(request(
                definition(root.path(), persona, explicit.then_some("memory")),
                explicit.then(|| vec!["memory".to_owned()]),
            )).await.expect("child");
            child.run("work", Vec::new()).await.expect("run child");
            let tools = captured.lock().clone();
            assert!(!tools.iter().any(|name| name == "memory"), "{tools:?}");
            for name in ["recall", "retain", "reflect"] { assert!(tools.iter().any(|tool| tool == name), "{tools:?}"); }
        }
    }

    #[tokio::test]
    async fn child_factory_applies_off_and_local_to_ordinary_and_persona_scopes() {
        for persona in [false, true] {
            for backend in [crate::MemoryBackend::Off, crate::MemoryBackend::Local] {
                let root = tempfile::tempdir().expect("root");
                let captured = Arc::new(Mutex::new(Vec::new()));
                let factory = OrchestrationRuntime::child_factory_from_snapshot(parent_snapshot(root.path(), captured.clone(), crate::MemoryConfig { backend, ..Default::default() }));
                let child = factory(request(definition(root.path(), persona, None), None)).await.expect("child");
                child.run("work", Vec::new()).await.expect("run child");
                let tools = captured.lock().clone();
                if backend == crate::MemoryBackend::Off {
                    assert!(!tools.iter().any(|name| matches!(name.as_str(), "memory" | "recall" | "retain" | "reflect")), "{tools:?}");
                } else {
                    assert!(tools.iter().any(|name| name == "memory"), "{tools:?}");
                    assert!(!tools.iter().any(|name| matches!(name.as_str(), "recall" | "retain" | "reflect")), "{tools:?}");
                }
            }
        }
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
    use crate::{AgentDefinitionSource, SessionOptions};

    fn tool_result(tool_name: &str, is_error: bool, timestamp: i64) -> pi_ai::Message {
        pi_ai::Message::ToolResult(pi_ai::ToolResultMessage {
            tool_call_id: format!("call-{timestamp}"),
            tool_name: tool_name.to_owned(),
            content: vec![pi_ai::ContentBlock::text(if is_error { "boom" } else { "ok" })],
            usage: None,
            details: None,
            added_tool_names: Vec::new(),
            is_error,
            timestamp,
        })
    }

    fn bash(command: &str, exit_code: Option<i32>, cancelled: bool, timestamp: i64) -> pi_ai::Message {
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
            "---\nname: task\ndescription: task\n---\nprompt",
            AgentDefinitionSource::Bundled,
            true,
        )
        .expect("definition")
    }

    fn session_with_tools(root: &Path, names: &[&str]) -> Session {
        let tools = names
            .iter()
            .map(|name| {
                AgentTool::new(*name, *name, pi_ai::Schema::default(), |_| async {
                    Ok(pi_agent::AgentToolResult::text("ok"))
                })
            })
            .collect::<Vec<_>>();
        Session::new(SessionOptions {
            model: pi_ai::Model::default(),
            cwd: root.to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(tools),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
        })
        .expect("session")
    }

    fn session(root: &Path) -> Session {
        session_with_tools(root, &[])
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
        assert_eq!(last_failed_tool_kind(&messages, &tool_names(&["edit"])), None);
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
                &[bash("false", Some(1), false, 1), bash("true", Some(0), false, 2)],
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

    #[test]
    fn settle_failed_last_tool_keeps_prose_and_surfaces_bounded_error() {
        let root = tempfile::tempdir().expect("root");
        let parent = session(root.path());
        let snapshot = parent.child_session_options_snapshot();
        let child = ChildSession::new(session_with_tools(root.path(), &["read", "edit"]));
        let config = OrchestrationConfig::new(
            AgentCatalog::from_agents(vec![task_definition()]),
            root.path().join("artifacts"),
        );
        let runtime =
            OrchestrationRuntime::new(config, OrchestrationRuntime::child_factory_from_snapshot(snapshot))
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
        let (output, error, _) = runtime.settle_child_outcome(
            Ok(result),
            &child,
            &YieldState::default(),
            false,
        );
        // The final prose stands as output, but the authoritative tool
        // failure still errors the job — prose never masks it.
        assert_eq!(output, "final prose after the failed tool");
        assert_eq!(error.as_deref(), Some("last tool execution failed: edit"));
    }

    #[test]
    fn settle_denied_write_on_read_only_child_completes() {
        // A read-only child whose only tool is `read` deliberately calls
        // `write`: the denial stays visible in the transcript as an is_error
        // result, but the run completes without error (the campaign contract:
        // the write denial is recorded while the scout finishes read-only).
        let root = tempfile::tempdir().expect("root");
        let parent = session(root.path());
        let snapshot = parent.child_session_options_snapshot();
        let child = ChildSession::new(session_with_tools(root.path(), &["read"]));
        let config = OrchestrationConfig::new(
            AgentCatalog::from_agents(vec![task_definition()]),
            root.path().join("artifacts"),
        );
        let runtime =
            OrchestrationRuntime::new(config, OrchestrationRuntime::child_factory_from_snapshot(snapshot))
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
        let (output, error, _) = runtime.settle_child_outcome(
            Ok(result),
            &child,
            &YieldState::default(),
            false,
        );
        assert_eq!(
            output,
            format!("scout finished without write\n\n{MISSING_YIELD_WARNING}")
        );
        assert!(
            error.is_none(),
            "denied write on a read-only child must not error the job: {error:?}"
        );
    }

    #[test]
    fn settle_recovered_tool_failure_completes_without_error() {
        let root = tempfile::tempdir().expect("root");
        let parent = session(root.path());
        let snapshot = parent.child_session_options_snapshot();
        let child = ChildSession::new(session_with_tools(root.path(), &["edit"]));
        let config = OrchestrationConfig::new(
            AgentCatalog::from_agents(vec![task_definition()]),
            root.path().join("artifacts"),
        );
        let runtime =
            OrchestrationRuntime::new(config, OrchestrationRuntime::child_factory_from_snapshot(snapshot))
                .expect("runtime");
        let result = settled_run(
            "recovered and done",
            vec![tool_result("edit", true, 1), tool_result("edit", false, 2), prose("done")],
            None,
        );
        let (output, error, _) = runtime.settle_child_outcome(
            Ok(result),
            &child,
            &YieldState::default(),
            false,
        );
        assert_eq!(output, format!("recovered and done\n\n{MISSING_YIELD_WARNING}"));
        assert!(error.is_none(), "recovered tool failure must not error: {error:?}");
    }

    #[test]
    fn settle_provider_error_keeps_priority_over_tool_failure() {
        let root = tempfile::tempdir().expect("root");
        let parent = session(root.path());
        let snapshot = parent.child_session_options_snapshot();
        let child = ChildSession::new(session_with_tools(root.path(), &["read"]));
        let config = OrchestrationConfig::new(
            AgentCatalog::from_agents(vec![task_definition()]),
            root.path().join("artifacts"),
        );
        let runtime =
            OrchestrationRuntime::new(config, OrchestrationRuntime::child_factory_from_snapshot(snapshot))
                .expect("runtime");
        let result = settled_run(
            "partial prose",
            vec![tool_result("read", true, 1)],
            Some("provider: upstream request failed".to_owned()),
        );
        let (_, error, _) = runtime.settle_child_outcome(
            Ok(result),
            &child,
            &YieldState::default(),
            false,
        );
        // The provider's own error wins; the tool failure summary must not
        // replace or be appended to it.
        assert_eq!(error.as_deref(), Some("provider: upstream request failed"));
    }
}
