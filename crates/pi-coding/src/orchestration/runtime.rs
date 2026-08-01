use std::collections::{HashMap, VecDeque};
use std::fs::{self, File};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use parking_lot::Mutex;
use pi_agent::{AgentTool, BoxFuture, ThinkingLevel};
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, Semaphore};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{Session, Skill};

use super::{AgentCatalog, AgentDefinition};

pub const DEFAULT_MAILBOX_CAPACITY: usize = 100;
pub const DEFAULT_MAX_CONCURRENCY: usize = 4;
pub const DEFAULT_MAX_RECURSION_DEPTH: usize = 2;
pub const DEFAULT_MAX_TOOLS_PER_AGENT: usize = 16;
const MAX_AUTOLOAD_SKILL_BYTES: u64 = 256 * 1024;
const MAX_AUTOLOAD_PROMPT_BYTES: usize = 1024 * 1024;

pub type AgentSelectorFn = Arc<
    dyn Fn(&str, &[AgentDefinition]) -> Option<String> + Send + Sync,
>;
pub type ChildSessionFactory =
    Arc<dyn Fn(ChildSessionRequest) -> BoxFuture<Result<Session>> + Send + Sync>;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestrationSkill {
    pub name: String,
    pub description: String,
    pub file_path: PathBuf,
    pub base_dir: PathBuf,
    pub trusted: bool,
}

impl From<&Skill> for OrchestrationSkill {
    fn from(skill: &Skill) -> Self {
        Self {
            name: skill.name.clone(),
            description: skill.description.clone(),
            file_path: PathBuf::from(&skill.file_path),
            base_dir: PathBuf::from(&skill.base_dir),
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
            .finish()
    }
}

impl OrchestrationRuntime {
    pub fn child_factory_from_session(parent: &Session) -> ChildSessionFactory {
        Self::child_factory_from_snapshot(parent.child_session_options_snapshot())
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
                        .filter(|name| !matches!(name.as_str(), "todo" | "process" | "task" | "hub"))
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
                Session::new_with_additional_tools_filtered_discovery_and_uri(
                    crate::SessionOptions {
                        model: snapshot.model,
                        cwd: snapshot.cwd,
                        system_prompt: request.system_prompt,
                        thinking_level: request.thinking_level.unwrap_or(snapshot.thinking_level),
                        api_key: snapshot.api_key,
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
        if let Some(skill) = self.skills.iter().find(|skill| !skill.trusted) {
            bail!("orchestration skill {:?} is not trusted", skill.name);
        }
        let skill_names = self
            .skills
            .iter()
            .map(|skill| skill.name.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        for agent in self.catalog.agents() {
            for skill in &agent.autoload_skills {
                if !skill_names.contains(skill.as_str()) {
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
        self.default_agent_selector = Some(Arc::new(move |request, agents| {
            let ranked = crate::rank_agents(request, agents, &settings);
            crate::select_default_agent(&ranked, &settings)
        }));
        self
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
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeliveryOutcome {
    Queued,
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
        Ok(Self {
            inner: Arc::new(RuntimeInner {
                group_id,
                semaphore: Arc::new(Semaphore::new(config.max_concurrency)),
                config,
                factory,
                shutdown: CancellationToken::new(),
                active: Mutex::new(HashMap::new()),
                active_changed: Notify::new(),
            }),
            owner: true,
        })
    }

    #[must_use]
    pub fn group_id(&self) -> &str {
        &self.inner.group_id
    }

    #[must_use]
    pub fn catalog(&self) -> &AgentCatalog {
        &self.inner.config.catalog
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
                    Ok(()) => DeliveryReceipt {
                        to: target,
                        outcome: DeliveryOutcome::Queued,
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
        REGISTRY.set_status(&self.inner.group_id, id, AgentStatus::Parked)
    }

    pub async fn run_tasks(
        &self,
        parent_id: &str,
        parent_depth: usize,
        items: Vec<TaskItem>,
        abort: pi_agent::AbortSignal,
    ) -> Result<Vec<TaskResult>> {
        if parent_depth >= self.inner.config.max_recursion_depth {
            bail!("subagent recursion depth limit reached");
        }
        if items.is_empty() {
            bail!("task batch must contain at least one item");
        }
        let mut seen = std::collections::BTreeSet::new();
        for item in &items {
            validate_agent_id(&item.id)?;
            if !seen.insert(item.id.clone()) {
                bail!("duplicate child agent id {:?}", item.id);
            }
            if REGISTRY.get(&self.inner.group_id, &item.id).is_some() {
                bail!("child agent id {:?} is already registered", item.id);
            }
            let definition = self
                .inner
                .config
                .catalog
                .get(&item.agent)
                .ok_or_else(|| anyhow!("unknown agent definition {:?}", item.agent))?;
            if !definition.trusted {
                bail!("agent definition {:?} is not trusted", item.agent);
            }
            if definition.tools.as_ref().is_some_and(|tools| {
                tools
                    .iter()
                    .filter(|name| {
                        !matches!(name.as_str(), "todo" | "process" | "task" | "hub")
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
        }

        let batch_cancel = CancellationToken::new();
        let abort_token = abort.cancellation_token();
        let shutdown = self.inner.shutdown.clone();
        let cancellation = batch_cancel.clone();
        let cancellation_watcher = tokio::spawn(async move {
            tokio::select! {
                () = abort_token.cancelled() => cancellation.cancel(),
                () = shutdown.cancelled() => cancellation.cancel(),
            }
        });

        let mut tasks = tokio::task::JoinSet::new();
        for item in items {
            let definition = self
                .inner
                .config
                .catalog
                .get(&item.agent)
                .cloned()
                .expect("agent definition was validated");
            REGISTRY.register(
                &self.inner.group_id,
                AgentSnapshot {
                    id: item.id.clone(),
                    display_name: format!("{}: {}", definition.name, one_line(&item.assignment)),
                    parent_id: Some(parent_id.to_owned()),
                    status: AgentStatus::Running,
                    created_at: now_millis(),
                    last_activity: now_millis(),
                    unread: 0,
                    artifact_ref: None,
                    history_ref: None,
                },
                self.inner.config.mailbox_capacity,
            )?;
            let runtime = self.clone();
            let parent_id = parent_id.to_owned();
            let cancel = batch_cancel.child_token();
            self.inner.active.lock().insert(item.id.clone(), cancel.clone());
            tasks.spawn(async move {
                runtime
                    .run_one(parent_id, parent_depth + 1, item, definition, cancel)
                    .await
            });
        }

        let mut results = Vec::new();
        while let Some(joined) = tasks.join_next().await {
            match joined {
                Ok(result) => results.push(result),
                Err(error) => {
                    batch_cancel.cancel();
                    cancellation_watcher.abort();
                    return Err(anyhow!("subagent task failed to join: {error}"));
                }
            }
        }
        cancellation_watcher.abort();
        results.sort_by_key(|result| result.index);
        Ok(results)
    }

    async fn run_one(
        &self,
        parent_id: String,
        depth: usize,
        item: TaskItem,
        definition: AgentDefinition,
        cancel: CancellationToken,
    ) -> TaskResult {
        let _active_guard = ActiveChildGuard {
            inner: self.inner.clone(),
            id: item.id.clone(),
        };
        let artifact_ref = format!("agent://{}", item.id);
        let history_ref = format!("history://{}", item.id);
        let artifact_uri = format!("artifact://{}", item.id);
        let permit = tokio::select! {
            permit = self.inner.semaphore.clone().acquire_owned() => match permit {
                Ok(permit) => permit,
                Err(_) => return self.failed_result(&item, AgentStatus::Aborted, "orchestration semaphore closed"),
            },
            () = cancel.cancelled() => return self.failed_result(&item, AgentStatus::Aborted, "task cancelled before start"),
        };
        let system_prompt = match self.child_system_prompt(&definition, &item.assignment) {
            Ok(prompt) => prompt,
            Err(error) => return self.failed_result(&item, AgentStatus::Idle, &error.to_string()),
        };
        let orchestration_tools = self.agent_tools(&item.id, depth);
        let request = ChildSessionRequest {
            child_id: item.id.clone(),
            parent_id,
            depth,
            definition: definition.clone(),
            assignment: item.assignment.clone(),
            system_prompt,
            requested_tool_names: definition.tools.clone(),
            orchestration_tools,
            thinking_level: definition.thinking_level,
            max_tools_per_agent: self.inner.config.max_tools_per_agent,
        };
        let session = match (self.inner.factory)(request).await {
            Ok(session) => session,
            Err(error) => return self.failed_result(&item, AgentStatus::Idle, &error.to_string()),
        };
        let mut run = Box::pin(session.run(&item.assignment, Vec::new()));
        let outcome = tokio::select! {
            result = &mut run => result.map_err(|error| error.to_string()),
            () = cancel.cancelled() => {
                session.abort().await;
                let _ = run.await;
                Err("task cancelled".to_owned())
            }
        };
        drop(permit);
        let status = if cancel.is_cancelled() {
            AgentStatus::Aborted
        } else {
            AgentStatus::Idle
        };
        let (output, error) = match outcome {
            Ok(result) => (result.text, result.error_message),
            Err(error) => (session.last_assistant_text(), Some(error)),
        };
        let artifact_path = self.inner.config.artifact_dir.join(format!("{}.md", item.id));
        let history_path = self
            .inner
            .config
            .artifact_dir
            .join(format!("{}.history.json", item.id));
        let artifact_body = if output.is_empty() {
            error.as_deref().unwrap_or("(no output)").to_owned()
        } else {
            output.clone()
        };
        let artifact_write = write_new_artifact(&artifact_path, artifact_body.as_bytes())
            .with_context(|| format!("writing subagent artifact {}", artifact_path.display()));
        let history_write = serde_json::to_vec_pretty(&session.history())
            .context("serializing subagent history")
            .and_then(|bytes| {
                write_new_artifact(&history_path, &bytes)
                    .with_context(|| format!("writing subagent history {}", history_path.display()))
            });
        let final_error = match artifact_write.err().or_else(|| history_write.err()) {
            Some(write_error) => Some(match error.as_deref() {
                Some(error) => format!("{error}; {write_error}"),
                None => write_error.to_string(),
            }),
            None => error,
        };
        let _ = REGISTRY.finish(
            &self.inner.group_id,
            &item.id,
            status,
            artifact_ref.clone(),
            history_ref.clone(),
        );
        TaskResult {
            index: item.index,
            id: item.id,
            agent: item.agent,
            status,
            output,
            error: final_error,
            artifact_ref,
            history_ref,
            artifact_uri,
        }
    }

    fn failed_result(&self, item: &TaskItem, status: AgentStatus, error: &str) -> TaskResult {
        let artifact_ref = format!("agent://{}", item.id);
        let history_ref = format!("history://{}", item.id);
        let artifact_uri = format!("artifact://{}", item.id);
        let _ = write_new_artifact(
            &self.inner.config.artifact_dir.join(format!("{}.md", item.id)),
            error.as_bytes(),
        );
        let _ = write_new_artifact(
            &self
                .inner
                .config
                .artifact_dir
                .join(format!("{}.history.json", item.id)),
            b"[]",
        );
        let _ = REGISTRY.finish(
            &self.inner.group_id,
            &item.id,
            status,
            artifact_ref.clone(),
            history_ref.clone(),
        );
        TaskResult {
            index: item.index,
            id: item.id.clone(),
            agent: item.agent.clone(),
            status,
            output: String::new(),
            error: Some(error.to_owned()),
            artifact_ref,
            history_ref,
            artifact_uri,
        }
    }

    fn child_system_prompt(&self, definition: &AgentDefinition, assignment: &str) -> Result<String> {
        let mut prompt = definition.system_prompt.clone();
        let mut autoload_bytes = 0_usize;
        if !self.inner.config.skills.is_empty() {
            prompt.push_str("\n\n<available_skills>\n");
            for skill in &self.inner.config.skills {
                prompt.push_str(&format!(
                    "  <skill><name>{}</name><description>{}</description><location>{}</location></skill>\n",
                    escape_xml(&skill.name),
                    escape_xml(&skill.description),
                    escape_xml(&skill.file_path.to_string_lossy()),
                ));
            }
            prompt.push_str("</available_skills>");
        }
        for name in &definition.autoload_skills {
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
                "\n\n<autoloaded_skill name=\"{}\" location=\"{}\">\n{}\n</autoloaded_skill>",
                escape_xml(&skill.name),
                escape_xml(&skill.file_path.to_string_lossy()),
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
        if let Some(selector) = &self.inner.config.default_agent_selector
            && let Some(selected) = selector(assignment, self.inner.config.catalog.agents())
        {
            return selected;
        }
        self.inner.config.default_agent.clone()
    }

    pub fn read_uri_resolver(&self) -> crate::InternalUriResolverFn {
        let artifact_dir = self.inner.config.artifact_dir.clone();
        Arc::new(move |uri| resolve_read_uri_in(&artifact_dir, uri))
    }

    pub fn read_uri_resolver_for_artifact_dir(
        artifact_dir: impl AsRef<Path>,
    ) -> Result<crate::InternalUriResolverFn> {
        let artifact_dir = absolute_lexical(artifact_dir.as_ref())?;
        Ok(Arc::new(move |uri| resolve_read_uri_in(&artifact_dir, uri)))
    }

    pub fn resolve_read_uri(&self, uri: &str) -> Result<PathBuf> {
        resolve_read_uri_in(&self.inner.config.artifact_dir, uri)
    }

    pub fn resolve_agent_reference(&self, id: &str) -> Result<PathBuf> {
        validate_agent_id(id)?;
        let path = self.inner.config.artifact_dir.join(format!("{id}.md"));
        ensure_existing_artifact(&self.inner.config.artifact_dir, &path)
    }

    pub fn resolve_history_reference(&self, id: &str) -> Result<PathBuf> {
        validate_agent_id(id)?;
        let path = self
            .inner
            .config
            .artifact_dir
            .join(format!("{id}.history.json"));
        ensure_existing_artifact(&self.inner.config.artifact_dir, &path)
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
        loop {
            let notified = self.inner.active_changed.notified();
            if self.inner.active.lock().is_empty() {
                break;
            }
            notified.await;
        }
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
        REGISTRY.remove_group(&self.inner.group_id);
    }
}

impl Drop for RuntimeInner {
    fn drop(&mut self) {
        self.shutdown.cancel();
        for token in self.active.get_mut().values() {
            token.cancel();
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

    fn enqueue(&self, group: &str, target: &str, message: MailboxMessage) -> Result<()> {
        let entry = self
            .get(group, target)
            .ok_or_else(|| anyhow!("unknown orchestration agent {target:?}"))?;
        if entry.snapshot.lock().status == AgentStatus::Aborted {
            bail!("orchestration agent {target:?} is aborted");
        }
        let mut mailbox = entry.mailbox.lock();
        if mailbox.len() >= entry.mailbox_capacity {
            bail!("orchestration mailbox for {target:?} is full");
        }
        mailbox.push_back(message);
        drop(mailbox);
        entry.message_ready.notify_waiters();
        Ok(())
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
        artifact_ref: String,
        history_ref: String,
    ) -> Result<()> {
        let entry = self
            .get(group, id)
            .ok_or_else(|| anyhow!("unknown orchestration agent {id:?}"))?;
        let mut snapshot = entry.snapshot.lock();
        snapshot.status = status;
        snapshot.last_activity = now_millis();
        snapshot.artifact_ref = Some(artifact_ref);
        snapshot.history_ref = Some(history_ref);
        Ok(())
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

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn resolve_read_uri_in(artifact_dir: &Path, uri: &str) -> Result<PathBuf> {
    let (id, history) = if let Some(id) = uri.strip_prefix("agent://") {
        (id, false)
    } else if let Some(id) = uri.strip_prefix("history://") {
        (id, true)
    } else if let Some(id) = uri.strip_prefix("artifact://") {
        (id, false)
    } else {
        bail!("unsupported orchestration URI {uri:?}");
    };
    validate_agent_id(id)?;
    let suffix = if history { ".history.json" } else { ".md" };
    ensure_existing_artifact(artifact_dir, &artifact_dir.join(format!("{id}{suffix}")))
}

fn validate_agent_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 80 {
        bail!("agent id must contain 1 to 80 characters");
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

    pub(crate) fn task_results_text(results: &[TaskResult]) -> String {
        let mut lines = Vec::new();
        for result in results {
            let status = match result.status {
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
