use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Read as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use pi_agent::{ThinkingLevel, ToolCapability};
use pi_ai::Model;
use serde::{Deserialize, Serialize};

use crate::resources::CONFIG_DIR_NAME;
use crate::settings::{AgentRuntimeSettings, Settings};

const MAX_AGENT_NAME_LENGTH: usize = 64;
const MAX_AGENT_DESCRIPTION_LENGTH: usize = 1024;
const MAX_AGENT_DEFINITION_BYTES: u64 = 256 * 1024;
const MAX_AGENT_CATALOG_BYTES: u64 = 2 * 1024 * 1024;
const BUNDLED_TASK_PROMPT: &str = include_str!("task_agent.md");
const BUNDLED_RESEARCHER_PROMPT: &str = include_str!("researcher_agent.md");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentDefinitionSource {
    Project,
    User,
    Bundled,
}

/// Whether a discovered definition is an ordinary agent or a durable persona.
///
/// Personas live under `<scope>/personas/<name>/persona.md` and carry durable
/// memory/sessions alongside the prompt; ordinary agents are stateless
/// `.md` files under `<scope>/agents`. The kind is assigned by discovery
/// (location-based), never parsed from frontmatter, so a persona file cannot
/// masquerade as an agent or vice versa.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentDefinitionKind {
    /// A stateless agent definition (`<scope>/agents/<name>.md`).
    #[default]
    Agent,
    /// A durable persona (`<scope>/personas/<name>/persona.md`).
    Persona,
}

/// Soft budget knobs that let a child subagent yield control back to its
/// parent instead of running to completion.
///
/// Every knob is optional and defaults to unlimited, so a default
/// [`JobSoftBudget`] preserves the historical run-to-completion behavior. A
/// persona may declare its own `softBudget` frontmatter block; when present it
/// replaces the global [`crate::orchestration::OrchestrationConfig::soft_budget`]
/// for that persona's child jobs. This type is declared here (alongside
/// [`AgentDefinition`]) so the catalog and runtime share one definition.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobSoftBudget {
    /// Maximum assistant requests (LLM turns) a child job may make before it
    /// yields. Reaching the cap settles the job with the soft-limit marker.
    pub max_requests: Option<usize>,
    /// Maximum cumulative tokens (sum of per-turn `Usage::total_tokens`) a
    /// child job may consume before it yields.
    pub max_tokens: Option<u64>,
    /// Yield-driving: return control to the parent after this many requests,
    /// regardless of any remaining budget, so the supervisor can steer or
    /// continue the child. The job settles with the soft-limit marker.
    pub yield_after: Option<usize>,
}

/// Per-capability ceiling for a role's child-session tool set.
///
/// Each boolean enables tools of the matching [`ToolCapability`]; a capability
/// omitted from the ceiling is disallowed. The ceiling filters the child's
/// coding tools by their declared capability at spawn, so a role that sets
/// `read: true, write: false, exec: false` gets a strictly read-only tool set.
/// Orchestration plumbing (`todo`/`process`/`task`/`hub`/`goal`) is kept so a
/// read-only role can still delegate and be supervised.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CapabilityCeiling {
    pub read: bool,
    pub write: bool,
    pub exec: bool,
}

impl CapabilityCeiling {
    #[must_use]
    pub fn is_unrestricted(self) -> bool {
        self.read && self.write && self.exec
    }

    #[must_use]
    pub fn allowed_capabilities(self) -> Vec<ToolCapability> {
        let mut allowed = Vec::with_capacity(3);
        if self.read {
            allowed.push(ToolCapability::Read);
        }
        if self.write {
            allowed.push(ToolCapability::Write);
        }
        if self.exec {
            allowed.push(ToolCapability::Exec);
        }
        allowed
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentDefinition {
    pub name: String,
    pub description: String,
    pub system_prompt: String,
    pub tools: Option<Vec<String>>,
    pub autoload_skills: Vec<String>,
    pub model: Option<Vec<String>>,
    pub thinking_level: Option<ThinkingLevel>,
    /// Maximum assistant turns (LLM requests) a child spawned with this role may
    /// take. The child stops cleanly after the cap with a clear reason.
    pub max_turns: Option<usize>,
    /// Maximum cumulative tool calls a child spawned with this role may make.
    /// The child stops cleanly after the cap with a clear reason.
    pub max_tool_calls: Option<usize>,
    /// Wall-clock bound for the whole child run, in seconds. The child is
    /// aborted when the deadline is exceeded.
    pub timeout_secs: Option<u64>,
    /// Tools the child must never receive, matched against [`crate::TOOL_NAMES`].
    pub disallowed_tools: Vec<String>,
    pub capability_ceiling: Option<CapabilityCeiling>,
    pub source: AgentDefinitionSource,
    pub path: Option<PathBuf>,
    pub trusted: bool,
    /// Whether this definition is an ordinary agent or a durable persona.
    /// Assigned by discovery (location-based); the parser defaults to `Agent`.
    pub kind: AgentDefinitionKind,
    /// Free-form personality preamble for a persona (frontmatter `personality`).
    /// `None` for ordinary agents that do not declare it.
    pub personality: Option<String>,
    /// Per-persona soft budget that replaces the global
    /// [`crate::orchestration::OrchestrationConfig::soft_budget`] when
    /// configured. `None` falls back to the global budget.
    pub soft_budget: Option<JobSoftBudget>,
}

impl AgentDefinition {
    /// Whether this definition is a durable persona (`kind == Persona`).
    #[must_use]
    pub fn is_persona(&self) -> bool {
        self.kind == AgentDefinitionKind::Persona
    }

    /// The persona root directory (`<scope>/personas/<name>`) holding
    /// `persona.md`, `memory/entries.jsonl`, and `sessions/<agent-id>.jsonl`,
    /// for a persona; `None` for ordinary agents or personas without a path.
    #[must_use]
    pub fn persona_root(&self) -> Option<PathBuf> {
        self.is_persona()
            .then(|| self.path.as_ref().and_then(|path| path.parent()).map(Path::to_path_buf))
            .flatten()
    }
}

impl Default for AgentDefinition {
    fn default() -> Self {
        Self {
            name: String::new(),
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
            source: AgentDefinitionSource::Bundled,
            path: None,
            trusted: true,
            kind: AgentDefinitionKind::Agent,
            personality: None,
            soft_budget: None,
        }
    }
}


#[derive(Clone, Debug)]
pub struct AgentDiscoveryOptions {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    pub project_trusted: bool,
}

impl AgentDiscoveryOptions {
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>, agent_dir: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            agent_dir: agent_dir.into(),
            project_trusted: false,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentCatalog {
    agents: Vec<AgentDefinition>,
}

impl AgentCatalog {
    pub fn discover(options: &AgentDiscoveryOptions) -> Result<Self> {
        let cwd = absolute_existing_or_lexical(&options.cwd)?;
        let agent_dir = absolute_existing_or_lexical(&options.agent_dir)?;
        let mut agents = Vec::new();
        let mut seen = BTreeSet::new();
        let mut total_bytes = 0_u64;

        if options.project_trusted {
            append_persona_directory(
                &cwd.join(CONFIG_DIR_NAME).join("personas"),
                AgentDefinitionSource::Project,
                options.project_trusted,
                &mut seen,
                &mut agents,
                &mut total_bytes,
            )?;
            append_directory(
                &cwd.join(CONFIG_DIR_NAME).join("agents"),
                AgentDefinitionSource::Project,
                options.project_trusted,
                &mut seen,
                &mut agents,
                &mut total_bytes,
            )?;
        }
        append_persona_directory(
            &agent_dir.join("personas"),
            AgentDefinitionSource::User,
            true,
            &mut seen,
            &mut agents,
            &mut total_bytes,
        )?;
        append_directory(
            &agent_dir.join("agents"),
            AgentDefinitionSource::User,
            true,
            &mut seen,
            &mut agents,
            &mut total_bytes,
        )?;
        for agent in bundled_agents() {
            if seen.insert(agent.name.clone()) {
                agents.push(agent);
            }
        }
        Ok(Self { agents })
    }

    #[must_use]
    pub fn from_agents(agents: Vec<AgentDefinition>) -> Self {
        let mut unique = Vec::with_capacity(agents.len());
        let mut seen = BTreeSet::new();
        for agent in agents {
            if seen.insert(agent.name.clone()) {
                unique.push(agent);
            }
        }
        Self { agents: unique }
    }

    #[must_use]
    pub fn agents(&self) -> &[AgentDefinition] {
        &self.agents
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&AgentDefinition> {
        self.agents.iter().find(|agent| agent.name == name)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentModelSource {
    /// Explicit `settings.agents.<name>.model` override.
    SettingsOverride,
    /// First matching entry from the agent definition `model` list.
    DefinitionFallback,
    /// Parent session model when no override/list match exists.
    Parent,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedAgentModel {
    pub model: Model,
    pub source: AgentModelSource,
    /// Pattern that matched when source is SettingsOverride or DefinitionFallback.
    pub matched_pattern: Option<String>,
    /// Thinking level parsed from a model suffix such as `:high` or `:max`.
    pub thinking_level: Option<ThinkingLevel>,
}

pub fn agent_disabled_error(name: &str) -> anyhow::Error {
    anyhow!(
        "agent `{name}` is disabled in settings; enable it with /agents or settings.agents.{name}.enabled"
    )
}

/// Effective child-tool allow-list after applying the canonical settings override.
#[must_use]
pub fn effective_agent_tool_names<'a>(
    definition: &'a AgentDefinition,
    settings: Option<&'a AgentRuntimeSettings>,
) -> Option<&'a [String]> {
    settings
        .and_then(AgentRuntimeSettings::tools_override)
        .or(definition.tools.as_deref())
}

/// Orchestration plumbing appended to every child's tool set by the child
/// factory, independent of agent-definition declarations. A declaration of
/// any of these names is valid (and redundant): the factory supplies them
/// regardless, so the validator treats them as known child tools and the
/// base-tool builder skips them (it cannot construct them via
/// `create_tool`, which only knows the main-session tool set). `yield` is
/// the child-only explicit-delivery tool — it is auto-appended by the child
/// factory to every orchestration child and is intentionally absent from
/// [`crate::TOOL_NAMES`] (the main session never exposes it). Unknown
/// non-plumbing declarations are reported once for a warning, never fatal.
pub const CHILD_PLUMBING_TOOLS: &[&str] = &["todo", "process", "task", "hub", "goal", "yield"];

/// Whether `name` is orchestration plumbing auto-provided to children. Such
/// tools are valid in agent definitions (a declaration is redundant) but are
/// never built via the main-session tool factories.
#[must_use]
pub fn is_child_plumbing_tool(name: &str) -> bool {
    CHILD_PLUMBING_TOOLS.contains(&name)
}

/// Whether `name` is a tool a child agent may actually receive: either
/// orchestration plumbing (auto-appended by the child factory) or one of the
/// main-session built-ins the factory can construct. Any other declared name
/// is unknown: it is reported once for a deduped warning and silently dropped
/// (OMP-aligned — the declaration never becomes an injected tool and never
/// makes the agent unavailable).
#[must_use]
pub fn is_known_child_tool(name: &str) -> bool {
    is_child_plumbing_tool(name) || crate::TOOL_NAMES.contains(&name)
}

/// Names an agent declares (via the definition or the
/// `settings.agents.<name>.tools` override) that can never be injected into a
/// child session. Unknown names are REPORTED for a single deduped warning —
/// they are never fatal: the definition stays loadable/spawnable and the
/// unknown tool is simply not injected (OMP-compatible silent ignore).
#[must_use]
pub fn unsupported_agent_tools(
    definition: &AgentDefinition,
    settings: Option<&AgentRuntimeSettings>,
) -> Vec<String> {
    effective_agent_tool_names(definition, settings)
        .into_iter()
        .flatten()
        .filter(|tool| !is_known_child_tool(tool))
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Human-readable, deduped warning for agent-declared tools that will be
/// silently ignored. One message per (agent, tool) pair keeps repeated spawns
/// from re-warning while still surfacing every unknown name.
pub fn unknown_tools_warning(name: &str, unknown: &[String]) -> String {
    format!(
        "agent `{name}` declares unknown tools: {}; ignoring them (OMP-compatible — the declared tool is not injected); supported child tools: {}",
        unknown.join(", "),
        crate::TOOL_NAMES.join(", "),
    )
}

pub fn agent_model_error(name: &str, error: &anyhow::Error) -> anyhow::Error {
    anyhow!(
        "agent `{name}` is unavailable because its model configuration is invalid: {error}; choose a valid model with /agents or settings.agents.{name}.model"
    )
}

/// Whether `name` may be spawned given effective settings. Missing entries default to enabled.
#[must_use]
pub fn is_agent_enabled(name: &str, settings: &Settings) -> bool {
    settings.is_agent_enabled(name)
}

#[must_use]
pub fn enabled_agent_definitions<'a>(
    agents: &'a [AgentDefinition],
    agent_settings: &std::collections::BTreeMap<String, AgentRuntimeSettings>,
) -> Vec<&'a AgentDefinition> {
    let available = available_models();
    let parent_model = Model::default();
    agents
        .iter()
        .filter(|agent| {
            let settings = agent_settings.get(&agent.name);
            settings.map_or(true, AgentRuntimeSettings::is_enabled)
                && agent_compatibility_error(agent, settings, &parent_model, &available).is_none()
        })
        .collect()
}

/// The first reason an agent definition cannot be used, if any. Unknown
/// declared tools never make an agent unavailable: they are silently ignored
/// (with a deduped warning via [`unknown_tools_warning`]), so this check only
/// covers model resolution.
#[must_use]
pub fn agent_compatibility_error(
    definition: &AgentDefinition,
    settings: Option<&AgentRuntimeSettings>,
    parent_model: &Model,
    available: &[Model],
) -> Option<anyhow::Error> {
    resolve_agent_model(definition, settings, parent_model, available)
        .err()
        .map(|error| agent_model_error(&definition.name, &error))
}


/// Resolve the model a child session should use.
///
/// Precedence:
/// 1. settings override (`settings.agents.<name>.model`)
/// 2. first matching entry in `definition.model`
/// 3. parent session model
///
/// An explicit settings override is an operator request, not a fallback hint, so
/// an invalid override is rejected instead of silently using the definition or
/// parent model.
pub fn resolve_agent_model(
    definition: &AgentDefinition,
    agent_settings: Option<&AgentRuntimeSettings>,
    parent_model: &Model,
    available: &[Model],
) -> Result<ResolvedAgentModel> {
    if let Some(pattern) = agent_settings.and_then(AgentRuntimeSettings::model_override) {
        let (model_pattern, thinking_level) = split_agent_model_thinking_suffix(pattern);
        let model = match_model_pattern(model_pattern, available, parent_model).map_err(|error| {
            anyhow!(
                "settings.agents.{}.model override {}: {error}",
                definition.name,
                pattern,
            )
        })?;
        return Ok(ResolvedAgentModel {
            model,
            source: AgentModelSource::SettingsOverride,
            matched_pattern: Some(pattern.to_owned()),
            thinking_level,
        });
    }

    if let Some(patterns) = definition.model.as_ref() {
        for pattern in patterns {
            let pattern = pattern.trim();
            if pattern.is_empty() {
                continue;
            }
            let (model_pattern, thinking_level) = split_agent_model_thinking_suffix(pattern);
            match match_model_pattern(model_pattern, available, parent_model) {
                Ok(model) => {
                    return Ok(ResolvedAgentModel {
                        model,
                        source: AgentModelSource::DefinitionFallback,
                        matched_pattern: Some(pattern.to_owned()),
                        thinking_level,
                    });
                }
                Err(error) if error.to_string().contains("ambiguous") => {
                    return Err(anyhow!(
                        "agent {:?} model list pattern {}: {error}",
                        definition.name,
                        pattern
                    ));
                }
                Err(_) => continue,
            }
        }
    }

    Ok(ResolvedAgentModel {
        model: parent_model.clone(),
        source: AgentModelSource::Parent,
        matched_pattern: None,
        thinking_level: None,
    })
}

pub fn resolve_agent_model_from_settings(
    definition: &AgentDefinition,
    settings: &Settings,
    parent_model: &Model,
    available: &[Model],
) -> Result<ResolvedAgentModel> {
    resolve_agent_model(
        definition,
        settings.agent_settings(&definition.name),
        parent_model,
        available,
    )
}

pub(crate) fn available_models() -> Vec<Model> {
    let mut providers = pi_ai::get_providers();
    providers.sort();
    let mut available = Vec::new();
    for provider in providers {
        let mut models = pi_ai::get_models(&provider);
        models.sort_by(|left, right| left.id.cmp(&right.id));
        available.extend(models);
    }
    available
}

fn split_agent_model_thinking_suffix(pattern: &str) -> (&str, Option<ThinkingLevel>) {
    let trimmed = pattern.trim();
    let Some((model, suffix)) = trimmed.rsplit_once(':') else {
        return (trimmed, None);
    };
    let thinking_level = match suffix.to_ascii_lowercase().as_str() {
        "off" => ThinkingLevel::Off,
        "minimal" => ThinkingLevel::Minimal,
        "low" => ThinkingLevel::Low,
        "medium" => ThinkingLevel::Medium,
        "high" => ThinkingLevel::High,
        "xhigh" => ThinkingLevel::Xhigh,
        "max" => ThinkingLevel::Max,
        _ => return (trimmed, None),
    };
    (model, Some(thinking_level))
}

fn match_model_pattern(pattern: &str, available: &[Model], parent_model: &Model) -> Result<Model> {
    if let Some(model) = find_matching_model(pattern, available)? {
        return Ok(model);
    }
    // Keep parent when the pattern literally names the parent even if the catalog
    // is empty or filtered (tests and offline child factories).
    if model_matches_pattern(parent_model, pattern) {
        return Ok(parent_model.clone());
    }
    // When a catalog was supplied, do not invent models for unmatched patterns —
    // settings overrides must fail fast instead of silently materialising junk.
    if !available.is_empty() {
        bail!(
            "did not match an available model; use /models or --list-models to inspect valid models"
        );
    }
    // Empty catalog (offline/tests): materialise provider/id via global resolver.
    crate::resolve_model(pattern).map_err(|error| anyhow!("{error}"))
}

/// Unique catalog match for `pattern`, or an actionable ambiguity error.
fn find_matching_model(pattern: &str, available: &[Model]) -> Result<Option<Model>> {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return Ok(None);
    }
    // Prefer exact provider/id uniqueness first.
    let exact = available
        .iter()
        .filter(|model| model_id(model).eq_ignore_ascii_case(pattern))
        .cloned()
        .collect::<Vec<_>>();
    match exact.as_slice() {
        [single] => return Ok(Some(single.clone())),
        [] => {}
        many => {
            let ids = many.iter().map(model_id).collect::<Vec<_>>().join(", ");
            bail!(
                "is ambiguous (exact id matches: {ids}); use a unique provider/id"
            );
        }
    }
    let matches = available
        .iter()
        .filter(|model| model_matches_pattern(model, pattern))
        .cloned()
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => Ok(None),
        [single] => Ok(Some(single.clone())),
        many => {
            let ids = many.iter().map(model_id).collect::<Vec<_>>().join(", ");
            bail!(
                "is ambiguous (matches: {ids}); use provider/id to select exactly one model"
            );
        }
    }
}

#[must_use]
pub fn model_id(model: &Model) -> String {
    format!("{}/{}", model.provider, model.id)
}

#[must_use]
pub fn model_matches_pattern(model: &Model, pattern: &str) -> bool {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return false;
    }
    let full = model_id(model);
    if full.eq_ignore_ascii_case(pattern) || model.id.eq_ignore_ascii_case(pattern) {
        return true;
    }
    if let Some((provider, id)) = pattern.split_once('/') {
        return model.provider.eq_ignore_ascii_case(provider.trim())
            && model.id.eq_ignore_ascii_case(id.trim());
    }
    // Provider-only / bare patterns may match multiple models; callers must treat multi-match as error.
    model.provider.eq_ignore_ascii_case(pattern)
}

fn bundled_agents() -> Vec<AgentDefinition> {
    vec![
        AgentDefinition {
            name: "task".to_owned(),
            description: "General-purpose subagent with full capabilities for delegated multi-step tasks".to_owned(),
            system_prompt: BUNDLED_TASK_PROMPT.trim().to_owned(),
            tools: None,
            autoload_skills: Vec::new(),
            model: None,
            thinking_level: None,
            max_turns: None,
            max_tool_calls: None,
            timeout_secs: None,
            disallowed_tools: Vec::new(),
            capability_ceiling: None,
            source: AgentDefinitionSource::Bundled,
            path: None,
            trusted: true,
            kind: AgentDefinitionKind::Agent,
            personality: None,
            soft_budget: None,
        },
        AgentDefinition {
            name: "researcher".to_owned(),
            description: "Research and study assigned topics, then report findings".to_owned(),
            system_prompt: BUNDLED_RESEARCHER_PROMPT.trim().to_owned(),
            tools: None,
            autoload_skills: Vec::new(),
            model: None,
            thinking_level: None,
            max_turns: None,
            max_tool_calls: None,
            timeout_secs: None,
            disallowed_tools: Vec::new(),
            capability_ceiling: None,
            source: AgentDefinitionSource::Bundled,
            path: None,
            trusted: true,
            kind: AgentDefinitionKind::Agent,
            personality: None,
            soft_budget: None,
        },
    ]
}

/// Names reserved for bundled agents. A persona may not take a bundled name —
/// it would shadow a built-in role — so persona discovery rejects them.
const BUNDLED_AGENT_NAMES: &[&str] = &["task", "researcher"];

fn append_directory(
    directory: &Path,
    source: AgentDefinitionSource,
    trusted: bool,
    seen: &mut BTreeSet<String>,
    output: &mut Vec<AgentDefinition>,
    total_bytes: &mut u64,
) -> Result<()> {
    let mut paths = match fs::read_dir(directory) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let file_type = entry.file_type().ok()?;
                let path = entry.path();
                ((file_type.is_file() || file_type.is_symlink())
                    && path.extension().is_some_and(|extension| extension == "md"))
                .then_some(path)
            })
            .collect::<Vec<_>>(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading agent definitions from {}", directory.display()));
        }
    };
    paths.sort();
    for path in paths {
        let size = fs::metadata(&path)
            .with_context(|| format!("reading agent definition metadata {}", path.display()))?
            .len();
        if size > MAX_AGENT_DEFINITION_BYTES {
            bail!(
                "agent definition {} exceeds maximum size of {} bytes",
                path.display(),
                MAX_AGENT_DEFINITION_BYTES
            );
        }
        *total_bytes = total_bytes.saturating_add(size);
        if *total_bytes > MAX_AGENT_CATALOG_BYTES {
            bail!("agent definition catalog exceeds maximum size of {MAX_AGENT_CATALOG_BYTES} bytes");
        }
        let mut content = String::with_capacity(usize::try_from(size).unwrap_or(0));
        File::open(&path)
            .with_context(|| format!("opening agent definition {}", path.display()))?
            .take(MAX_AGENT_DEFINITION_BYTES + 1)
            .read_to_string(&mut content)
            .with_context(|| format!("reading agent definition {}", path.display()))?;
        if content.len() as u64 > MAX_AGENT_DEFINITION_BYTES {
            bail!("agent definition {} grew beyond maximum size while reading", path.display());
        }
        let agent = parse_agent_definition(&path, &content, source, trusted)?;
        if seen.insert(agent.name.clone()) {
            output.push(agent);
        }
    }
    Ok(())
}

/// Discover durable persona definitions under a `personas` directory.
///
/// Each immediate child directory contributes only its `persona.md`; state
/// files and subdirectories (`memory/`, `sessions/`) are ignored. A persona
/// whose `name` frontmatter differs from its containing directory, or that
/// collides with a bundled agent name, is rejected. Size ceilings reuse the
/// agent limits. The shared `seen` set enforces discovery precedence: the
/// first source to claim a name wins, so callers must invoke this in
/// precedence order (project personas before user personas, before agents).
fn append_persona_directory(
    directory: &Path,
    source: AgentDefinitionSource,
    trusted: bool,
    seen: &mut BTreeSet<String>,
    output: &mut Vec<AgentDefinition>,
    total_bytes: &mut u64,
) -> Result<()> {
    let mut children = match fs::read_dir(directory) {
        Ok(entries) => entries
            .collect::<std::io::Result<Vec<_>>>()
            .map_err(|error| anyhow!("reading persona definitions from {}: {error}", directory.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading persona definitions from {}", directory.display()));
        }
    };
    children.sort_by_key(std::fs::DirEntry::file_name);
    for entry in children {
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("reading persona directory entry {}", entry.path().display())
                });
            }
        };
        // Only real immediate child directories can host a persona. Symlinked
        // roots are rejected so persona-local memory and transcript writes
        // cannot escape the discovered persona scope.
        if file_type.is_symlink() {
            bail!("persona directory must not be a symlink: {}", entry.path().display());
        }
        if !file_type.is_dir() {
            continue;
        }
        let dir_name = entry.file_name().to_string_lossy().into_owned();
        let persona_md = entry.path().join("persona.md");
        if !persona_md.is_file() {
            // A child directory without a persona.md is not a persona; skip it
            // rather than failing, so partial/auxiliary directories are tolerated.
            continue;
        }
        let size = fs::metadata(&persona_md)
            .with_context(|| format!("reading persona definition metadata {}", persona_md.display()))?
            .len();
        if size > MAX_AGENT_DEFINITION_BYTES {
            bail!(
                "persona definition {} exceeds maximum size of {} bytes",
                persona_md.display(),
                MAX_AGENT_DEFINITION_BYTES
            );
        }
        *total_bytes = total_bytes.saturating_add(size);
        if *total_bytes > MAX_AGENT_CATALOG_BYTES {
            bail!("agent definition catalog exceeds maximum size of {MAX_AGENT_CATALOG_BYTES} bytes");
        }
        let mut content = String::with_capacity(usize::try_from(size).unwrap_or(0));
        File::open(&persona_md)
            .with_context(|| format!("opening persona definition {}", persona_md.display()))?
            .take(MAX_AGENT_DEFINITION_BYTES + 1)
            .read_to_string(&mut content)
            .with_context(|| format!("reading persona definition {}", persona_md.display()))?;
        if content.len() as u64 > MAX_AGENT_DEFINITION_BYTES {
            bail!("persona definition {} grew beyond maximum size while reading", persona_md.display());
        }
        let persona = parse_persona_definition(&persona_md, &content, source, trusted)?;
        if persona.name != dir_name {
            bail!(
                "persona name {:?} must match its directory name {:?} (at {})",
                persona.name,
                dir_name,
                persona_md.display()
            );
        }
        if BUNDLED_AGENT_NAMES.contains(&persona.name.as_str()) {
            bail!(
                "persona name {:?} conflicts with a bundled agent name (at {})",
                persona.name,
                persona_md.display()
            );
        }

        if seen.insert(persona.name.clone()) {
            output.push(persona);
        }
    }
    Ok(())
}

pub fn parse_agent_definition(
    path: &Path,
    content: &str,
    source: AgentDefinitionSource,
    trusted: bool,
) -> Result<AgentDefinition> {
    parse_definition(path, content, source, trusted, AgentDefinitionKind::Agent)
}

pub fn parse_persona_definition(
    path: &Path,
    content: &str,
    source: AgentDefinitionSource,
    trusted: bool,
) -> Result<AgentDefinition> {
    parse_definition(path, content, source, trusted, AgentDefinitionKind::Persona)
}

fn parse_definition(
    path: &Path,
    content: &str,
    source: AgentDefinitionSource,
    trusted: bool,
    kind: AgentDefinitionKind,
) -> Result<AgentDefinition> {
    let (fields, body) = parse_frontmatter(content)
        .with_context(|| format!("parsing agent definition {}", path.display()))?;
    let name = required_scalar(&fields, "name")?;
    validate_name(&name)?;
    let description = required_scalar(&fields, "description")?;
    if description.chars().count() > MAX_AGENT_DESCRIPTION_LENGTH {
        bail!("agent description exceeds {MAX_AGENT_DESCRIPTION_LENGTH} characters");
    }
    if body.trim().is_empty() {
        bail!("agent system prompt must not be empty");
    }
    let tools = fields.get("tools").map(|value| parse_list(value)).transpose()?;
    let autoload_skills = fields
        .get("autoloadSkills")
        .or_else(|| fields.get("autoload-skills"))
        .map(|value| parse_list(value))
        .transpose()?
        .unwrap_or_default();
    let model = fields.get("model").map(|value| parse_list(value)).transpose()?;
    let thinking_level = fields
        .get("thinkingLevel")
        .or_else(|| fields.get("thinking-level"))
        .map(|value| parse_thinking_level(value))
        .transpose()?;
    let max_turns = fields
        .get("maxTurns")
        .or_else(|| fields.get("max-turns"))
        .map(|value| parse_positive_contract("maxTurns", value))
        .transpose()?;
    let max_tool_calls = fields
        .get("maxToolCalls")
        .or_else(|| fields.get("max-tool-calls"))
        .map(|value| parse_positive_contract("maxToolCalls", value))
        .transpose()?;
    let timeout_secs = fields
        .get("timeoutSecs")
        .or_else(|| fields.get("timeout-secs"))
        .map(|value| {
            let seconds = parse_positive_contract("timeoutSecs", value)?;
            u64::try_from(seconds).map_err(|_| anyhow!("agent frontmatter timeoutSecs is too large"))
        })
        .transpose()?;
    let disallowed_tools = fields
        .get("disallowedTools")
        .or_else(|| fields.get("disallowed-tools"))
        .map(|value| parse_list(value))
        .transpose()?
        .unwrap_or_default();
    let capability_ceiling = fields
        .get("capabilityCeiling")
        .or_else(|| fields.get("capability-ceiling"))
        .map(|value| parse_capability_ceiling(value))
        .transpose()?;
    let personality = (kind == AgentDefinitionKind::Persona)
        .then(|| {
            fields
                .get("personality")
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        })
        .flatten();
    let soft_budget = if kind == AgentDefinitionKind::Persona {
        parse_soft_budget(&fields)?
    } else {
        None
    };
    Ok(AgentDefinition {
        name,
        description,
        system_prompt: body.trim().to_owned(),
        tools,
        autoload_skills,
        model,
        thinking_level,
        max_turns,
        max_tool_calls,
        timeout_secs,
        disallowed_tools,
        capability_ceiling,
        source,
        path: Some(path.to_path_buf()),
        trusted,
        kind,
        personality,
        soft_budget,
    })
}

fn parse_frontmatter(content: &str) -> Result<(BTreeMap<String, String>, String)> {
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    let Some(rest) = normalized.strip_prefix("---\n") else {
        bail!("agent definition is missing frontmatter");
    };
    let Some(end) = rest.find("\n---\n") else {
        bail!("agent frontmatter is missing a closing delimiter");
    };
    let header = &rest[..end];
    let body = rest[end + 5..].to_owned();
    let mut fields = BTreeMap::new();
    let mut current_list: Option<String> = None;
    // Parent block key for an indented nested map (e.g. `softBudget:`). Child
    // scalars under it are stored as `parent.child` so flat lookups keep
    // working; lists fall back to `current_list` as before.
    let mut current_block: Option<String> = None;
    for (index, raw_line) in header.lines().enumerate() {
        let line = raw_line.trim_end();
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let trimmed = line.trim_start();
        let indented = line.len() > trimmed.len();
        if let Some(item) = trimmed.strip_prefix("- ") {
            let key = current_list.as_ref().ok_or_else(|| {
                anyhow!("frontmatter line {} has a list item without a field", index + 1)
            })?;
            let value = unquote(item.trim())?;
            let entry = fields.entry(key.clone()).or_insert_with(String::new);
            if !entry.is_empty() {
                entry.push(',');
            }
            entry.push_str(&value);
            // A block that yields list items is a list, not a nested map.
            current_block = None;
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            bail!("invalid frontmatter line {}: expected key: value", index + 1);
        };
        let key = key.trim();
        if key.is_empty() {
            bail!("invalid frontmatter line {}: key is empty", index + 1);
        }
        let value = value.trim();
        if indented {
            // Nested child of the current block. An indented key without an
            // active block is a structural error (actionable, like a stray
            // list item).
            let parent = current_block.as_ref().ok_or_else(|| {
                anyhow!("frontmatter line {} is indented without a parent block", index + 1)
            })?;
            let qualified = format!("{parent}.{key}");
            if value.is_empty() {
                current_list = Some(qualified.clone());
                current_block = Some(qualified.clone());
                fields.entry(qualified).or_default();
            } else {
                current_list = None;
                fields.insert(qualified, unquote(value)?);
            }
            continue;
        }
        if value.is_empty() {
            current_list = Some(key.to_owned());
            current_block = Some(key.to_owned());
            fields.entry(key.to_owned()).or_default();
        } else {
            current_list = None;
            current_block = None;
            fields.insert(key.to_owned(), unquote(value)?);
        }
    }
    Ok((fields, body))
}

fn required_scalar(fields: &BTreeMap<String, String>, name: &str) -> Result<String> {
    fields
        .get(name)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("agent frontmatter requires {name}"))
}

fn validate_name(name: &str) -> Result<()> {
    if name.chars().count() > MAX_AGENT_NAME_LENGTH {
        bail!("agent name exceeds {MAX_AGENT_NAME_LENGTH} characters");
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        bail!("agent name must contain only ASCII letters, digits, '_' or '-'");
    }
    Ok(())
}

fn parse_list(value: &str) -> Result<Vec<String>> {
    let trimmed = value.trim();
    let inner = if trimmed.starts_with('[') {
        trimmed
            .strip_prefix('[')
            .and_then(|value| value.strip_suffix(']'))
            .ok_or_else(|| anyhow!("unterminated frontmatter list"))?
    } else {
        trimmed
    };
    let mut values = Vec::new();
    for item in inner.split(',') {
        let item = unquote(item.trim())?;
        if !item.is_empty() {
            values.push(item);
        }
    }
    Ok(values)
}

fn parse_positive_contract(field: &str, value: &str) -> Result<usize> {
    let parsed = value
        .trim()
        .parse::<usize>()
        .map_err(|_| anyhow!("agent frontmatter {field} must be a positive integer"))?;
    if parsed == 0 {
        bail!("agent frontmatter {field} must be a positive integer (got 0; omit the field for no limit)");
    }
    Ok(parsed)
}

/// Like [`parse_positive_contract`] but for `u64`-typed knobs (e.g. token
/// budgets that may exceed `usize` on 32-bit hosts). Same actionable errors.
fn parse_positive_u64_contract(field: &str, value: &str) -> Result<u64> {
    let parsed = value
        .trim()
        .parse::<u64>()
        .map_err(|_| anyhow!("agent frontmatter {field} must be a positive integer"))?;
    if parsed == 0 {
        bail!("agent frontmatter {field} must be a positive integer (got 0; omit the field for no limit)");
    }
    Ok(parsed)
}

/// Resolve a nested positive-integer child (`parent.child` or `parent.kebab`)
/// from the flattened frontmatter map. Returns `None` when neither alias is
/// present; errors reuse [`parse_positive_contract`] so the message names the
/// exact key that was supplied.
fn nested_positive_usize(
    fields: &BTreeMap<String, String>,
    parent: &str,
    camel: &str,
    kebab: &str,
) -> Result<Option<usize>> {
    let camel_key = format!("{parent}.{camel}");
    let kebab_key = format!("{parent}.{kebab}");
    let (field, value) = match (fields.get(&camel_key), fields.get(&kebab_key)) {
        (Some(value), _) => (camel_key, value),
        (None, Some(value)) => (kebab_key, value),
        (None, None) => return Ok(None),
    };
    parse_positive_contract(&field, value).map(Some)
}

/// `u64` variant of [`nested_positive_usize`] for token-budget children.
fn nested_positive_u64(
    fields: &BTreeMap<String, String>,
    parent: &str,
    camel: &str,
    kebab: &str,
) -> Result<Option<u64>> {
    let camel_key = format!("{parent}.{camel}");
    let kebab_key = format!("{parent}.{kebab}");
    let (field, value) = match (fields.get(&camel_key), fields.get(&kebab_key)) {
        (Some(value), _) => (camel_key, value),
        (None, Some(value)) => (kebab_key, value),
        (None, None) => return Ok(None),
    };
    parse_positive_u64_contract(&field, value).map(Some)
}

/// Parse a persona's nested `softBudget` frontmatter block into a
/// [`JobSoftBudget`]. Accepts camelCase (`softBudget`) and kebab-case
/// (`soft-budget`) parents with `maxRequests`/`max-requests`,
/// `maxTokens`/`max-tokens`, `yieldAfter`/`yield-after` children, each a
/// positive integer. An absent block yields `None` (fall back to the global
/// budget); a present-but-empty block yields an unlimited budget; a non-empty
/// scalar value is rejected as a structural error.
fn parse_soft_budget(fields: &BTreeMap<String, String>) -> Result<Option<JobSoftBudget>> {
    const PARENTS: &[&str] = &["softBudget", "soft-budget"];
    let parent = PARENTS
        .iter()
        .find(|candidate| fields.contains_key(**candidate))
        .copied();
    let Some(parent) = parent else {
        return Ok(None);
    };
    let marker = fields.get(parent).map(String::as_str).unwrap_or_default();
    if !marker.is_empty() {
        bail!(
            "agent frontmatter {parent} must be a nested map of positive integers (e.g. `{parent}:\\n  maxRequests: 5`)"
        );
    }
    let max_requests = nested_positive_usize(fields, parent, "maxRequests", "max-requests")?;
    let max_tokens = nested_positive_u64(fields, parent, "maxTokens", "max-tokens")?;
    let yield_after = nested_positive_usize(fields, parent, "yieldAfter", "yield-after")?;
    Ok(Some(JobSoftBudget {
        max_requests,
        max_tokens,
        yield_after,
    }))
}

/// Parse a capability ceiling from a list of allowed capabilities
/// (`read`, `write`, `exec` — the same lowercase names [`ToolCapability`]
/// serializes with). Capabilities absent from the list are disallowed.
fn parse_capability_ceiling(value: &str) -> Result<CapabilityCeiling> {
    let mut ceiling = CapabilityCeiling::default();
    for item in parse_list(value)? {
        match item.trim().to_ascii_lowercase().as_str() {
            "read" => ceiling.read = true,
            "write" => ceiling.write = true,
            "exec" => ceiling.exec = true,
            other => {
                bail!(
                    "unsupported agent capability ceiling item {other:?}; use read, write, and/or exec"
                )
            }
        }
    }
    Ok(ceiling)
}

fn parse_thinking_level(value: &str) -> Result<ThinkingLevel> {
    match value.trim().to_ascii_lowercase().as_str() {
        "off" => Ok(ThinkingLevel::Off),
        "minimal" => Ok(ThinkingLevel::Minimal),
        "low" => Ok(ThinkingLevel::Low),
        "medium" => Ok(ThinkingLevel::Medium),
        "high" => Ok(ThinkingLevel::High),
        "xhigh" => Ok(ThinkingLevel::Xhigh),
        "max" => Ok(ThinkingLevel::Max),
        other => bail!("unsupported agent thinking level {other:?}"),
    }
}

fn unquote(value: &str) -> Result<String> {
    if value.is_empty() {
        return Ok(String::new());
    }
    if let Some(inner) = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
    {
        return Ok(inner
            .replace("\\n", "\n")
            .replace("\\t", "\t")
            .replace("\\\"", "\"")
            .replace("\\\\", "\\"));
    }
    if let Some(inner) = value
        .strip_prefix('\'')
        .and_then(|value| value.strip_suffix('\''))
    {
        return Ok(inner.replace("''", "'"));
    }
    if value.starts_with(['"', '\'']) || value.ends_with(['"', '\'']) {
        bail!("unterminated quoted frontmatter value");
    }
    Ok(value
        .split_once(" #")
        .map_or(value, |(value, _)| value)
        .trim_end()
        .to_owned())
}

fn absolute_existing_or_lexical(path: &Path) -> Result<PathBuf> {
    if let Ok(canonical) = path.canonicalize() {
        return Ok(canonical);
    }
    if path.is_absolute() {
        return Ok(lexical_normalize(path));
    }
    Ok(lexical_normalize(&std::env::current_dir()?.join(path)))
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

#[cfg(test)]
mod size_tests {
    use super::*;

    fn valid_agent_bytes(size: usize) -> Vec<u8> {
        let prefix = b"---\nname: bounded\ndescription: bounded\n---\n";
        assert!(size >= prefix.len());
        let mut bytes = prefix.to_vec();
        bytes.resize(size, b'x');
        bytes
    }

    #[test]
    fn agent_definition_accepts_exact_file_limit() {
        let root = tempfile::tempdir().expect("root");
        let agents = root.path().join("agents");
        fs::create_dir_all(&agents).expect("agents");
        fs::write(agents.join("bounded.md"), valid_agent_bytes(MAX_AGENT_DEFINITION_BYTES as usize)).expect("agent");
        let catalog = AgentCatalog::discover(&AgentDiscoveryOptions::new(root.path(), root.path())).expect("boundary accepted");
        assert!(catalog.get("bounded").is_some());
    }

    #[test]
    fn agent_definition_rejects_one_byte_over_file_limit() {
        let root = tempfile::tempdir().expect("root");
        let agents = root.path().join("agents");
        fs::create_dir_all(&agents).expect("agents");
        fs::write(agents.join("oversized.md"), valid_agent_bytes(MAX_AGENT_DEFINITION_BYTES as usize + 1)).expect("agent");
        let error = AgentCatalog::discover(&AgentDiscoveryOptions::new(root.path(), root.path())).expect_err("oversized agent rejected").to_string();
        assert!(error.contains("exceeds maximum size"));
    }

    #[test]
    fn bundled_researcher_is_discovered_and_user_definitions_win() {
        // P0-C: a normal install must offer the `researcher` role (the literal
        // `你让researcher…` prompt depends on it), so `discover` bundles it.
        let root = tempfile::tempdir().expect("root");
        let catalog = AgentCatalog::discover(&AgentDiscoveryOptions::new(root.path(), root.path())).expect("discover");
        let researcher = catalog.get("researcher").expect("bundled researcher must be discovered");
        assert_eq!(researcher.name, "researcher");
        assert_eq!(researcher.source, AgentDefinitionSource::Bundled);
        assert!(researcher.trusted);
        assert!(catalog.get("task").is_some(), "bundled task must remain");

        // A user definition with the same name wins over the bundle.
        let agents = root.path().join("agents");
        fs::create_dir_all(&agents).expect("agents");
        fs::write(
            agents.join("researcher.md"),
            "---\nname: researcher\ndescription: user researcher\n---\nUSER_RESEARCHER_PROMPT",
        )
        .expect("user agent");
        let catalog = AgentCatalog::discover(&AgentDiscoveryOptions::new(root.path(), root.path())).expect("discover");
        let researcher = catalog.get("researcher").expect("researcher present");
        assert_eq!(researcher.source, AgentDefinitionSource::User);
        assert!(researcher.system_prompt.contains("USER_RESEARCHER_PROMPT"));
    }

    #[test]
    fn agent_catalog_rejects_aggregate_over_limit() {
        let root = tempfile::tempdir().expect("root");
        let agents = root.path().join("agents");
        fs::create_dir_all(&agents).expect("agents");
        let count = MAX_AGENT_CATALOG_BYTES / MAX_AGENT_DEFINITION_BYTES + 1;
        for index in 0..count {
            let content = format!("---\nname: agent{index}\ndescription: bounded\n---\n");
            let mut bytes = content.into_bytes();
            bytes.resize(MAX_AGENT_DEFINITION_BYTES as usize, b'x');
            fs::write(agents.join(format!("{index:02}.md")), bytes).expect("agent");
        }
        let error = AgentCatalog::discover(&AgentDiscoveryOptions::new(root.path(), root.path())).expect_err("aggregate rejected").to_string();
        assert!(error.contains("catalog exceeds maximum size"));
    }
}


#[cfg(test)]
mod model_resolution_tests {
    use super::*;
    use crate::settings::AgentRuntimeSettings;

    fn model(provider: &str, id: &str) -> Model {
        Model {
            provider: provider.to_owned(),
            id: id.to_owned(),
            name: id.to_owned(),
            ..Model::default()
        }
    }

    fn def(name: &str, models: Option<Vec<&str>>) -> AgentDefinition {
        AgentDefinition {
            name: name.to_owned(),
            description: "d".to_owned(),
            system_prompt: "p".to_owned(),
            tools: Some(vec!["read".to_owned()]),
            autoload_skills: vec!["rust".to_owned()],
            model: models.map(|values| values.into_iter().map(str::to_owned).collect()),
            thinking_level: Some(ThinkingLevel::Low),
            max_turns: None,
            max_tool_calls: None,
            timeout_secs: None,
            disallowed_tools: Vec::new(),
            capability_ceiling: None,
            source: AgentDefinitionSource::Bundled,
            path: None,
            trusted: true,
            kind: AgentDefinitionKind::Agent,
            personality: None,
            soft_budget: None,
        }
    }

    #[test]
    fn settings_override_parses_max_thinking_suffix() {
        let parent = model("openai", "gpt-4.1");
        let available = vec![model("cliproxy", "gpt-5.6-sol")];
        let definition = def("coordinate", None);
        let settings = AgentRuntimeSettings {
            enabled: Some(true),
            model: Some("cliproxy/gpt-5.6-sol:max".to_owned()),
            tools: None,
        };
        let resolved = resolve_agent_model(&definition, Some(&settings), &parent, &available)
            .expect("valid model with max suffix must resolve");
        assert_eq!(resolved.model.provider, "cliproxy");
        assert_eq!(resolved.model.id, "gpt-5.6-sol");
        assert_eq!(resolved.thinking_level, Some(ThinkingLevel::Max));
        assert_eq!(
            resolved.matched_pattern.as_deref(),
            Some("cliproxy/gpt-5.6-sol:max"),
        );
    }

    #[test]
    fn settings_override_beats_definition_and_parent() {
        let parent = model("openai", "gpt-4.1");
        let available = vec![
            model("anthropic", "claude-sonnet-4-5"),
            model("openai", "gpt-4.1"),
            model("google", "gemini-2.5-pro"),
        ];
        let definition = def("reviewer", Some(vec!["google/gemini-2.5-pro"]));
        let settings = AgentRuntimeSettings {
            enabled: Some(true),
            model: Some("anthropic/claude-sonnet-4-5".to_owned()),
                tools: None,
            };
        let resolved = resolve_agent_model(&definition, Some(&settings), &parent, &available)
            .expect("settings override must resolve");
        assert_eq!(resolved.source, AgentModelSource::SettingsOverride);
        assert_eq!(resolved.model.provider, "anthropic");
        assert_eq!(resolved.model.id, "claude-sonnet-4-5");
        assert_eq!(resolved.matched_pattern.as_deref(), Some("anthropic/claude-sonnet-4-5"));
    }

    #[test]
    fn definition_fallback_list_selects_first_available() {
        let parent = model("openai", "gpt-4.1");
        let available = vec![
            model("openai", "gpt-4.1"),
            model("anthropic", "claude-sonnet-4-5"),
        ];
        let definition = def(
            "reviewer",
            Some(vec!["missing/model", "anthropic/claude-sonnet-4-5", "openai/gpt-4.1"]),
        );
        let resolved = resolve_agent_model(&definition, None, &parent, &available)
            .expect("definition fallback must resolve");
        assert_eq!(resolved.source, AgentModelSource::DefinitionFallback);
        assert_eq!(resolved.model.provider, "anthropic");
        assert_eq!(resolved.matched_pattern.as_deref(), Some("anthropic/claude-sonnet-4-5"));
    }

    #[test]
    fn parent_model_used_when_no_override_or_match() {
        let parent = model("openai", "gpt-4.1");
        let available = vec![model("anthropic", "claude-sonnet-4-5")];
        let definition = def("task", Some(vec!["missing/one", "missing/two"]));
        let resolved = resolve_agent_model(&definition, None, &parent, &available)
            .expect("parent fallback must resolve");
        assert_eq!(resolved.source, AgentModelSource::Parent);
        assert_eq!(resolved.model.id, "gpt-4.1");
        assert!(resolved.matched_pattern.is_none());
    }

    #[test]
    fn disabled_error_is_actionable() {
        let error = agent_disabled_error("reviewer").to_string();
        assert!(error.contains("disabled"));
        assert!(error.contains("/agents"));
        assert!(error.contains("reviewer"));
    }

    #[test]
    fn is_agent_enabled_defaults_true() {
        let mut settings = Settings::default();
        assert!(is_agent_enabled("task", &settings));
        settings.set_agent_settings(
            "task",
            AgentRuntimeSettings {
                enabled: Some(false),
                model: None,
                tools: None,
            },
        );
        assert!(!is_agent_enabled("task", &settings));
    }

    #[test]
    fn ambiguous_provider_pattern_is_rejected() {
        let parent = model("openai", "gpt-4.1");
        let available = vec![
            model("openai", "gpt-4.1"),
            model("openai", "gpt-4.1-mini"),
            model("anthropic", "claude-sonnet-4-5"),
        ];
        let definition = def("reviewer", None);
        let settings = AgentRuntimeSettings {
            enabled: Some(true),
            model: Some("openai".to_owned()),
                tools: None,
            };
        let error = resolve_agent_model(&definition, Some(&settings), &parent, &available)
            .expect_err("provider-only multi-match must fail");
        let message = error.to_string();
        assert!(message.contains("ambiguous"), "{message}");
        assert!(message.contains("openai/gpt-4.1"), "{message}");
    }

    #[test]
    fn ambiguous_bare_id_pattern_is_rejected() {
        let parent = model("other", "x");
        let available = vec![
            model("openai", "shared-id"),
            model("anthropic", "shared-id"),
        ];
        let definition = def("reviewer", Some(vec!["shared-id"]));
        let error = resolve_agent_model(&definition, None, &parent, &available)
            .expect_err("bare id multi-match must fail");
        let message = error.to_string();
        assert!(message.contains("ambiguous"), "{message}");
        assert!(message.contains("provider/id"), "{message}");
    }

    #[test]
    fn exact_provider_id_still_unique_among_siblings() {
        let parent = model("openai", "gpt-4.1");
        let available = vec![
            model("openai", "gpt-4.1"),
            model("openai", "gpt-4.1-mini"),
        ];
        let definition = def("reviewer", None);
        let settings = AgentRuntimeSettings {
            enabled: Some(true),
            model: Some("openai/gpt-4.1-mini".to_owned()),
                tools: None,
            };
        let resolved = resolve_agent_model(&definition, Some(&settings), &parent, &available)
            .expect("exact provider/id must resolve");
        assert_eq!(resolved.model.id, "gpt-4.1-mini");
    }

    #[test]
    fn enabled_agent_definitions_filters_disabled() {
        let agents = vec![def("task", None), def("reviewer", None)];
        let mut settings = std::collections::BTreeMap::new();
        settings.insert(
            "reviewer".to_owned(),
            AgentRuntimeSettings {
                enabled: Some(false),
                model: None,
                tools: None,
            },
        );
        let enabled = enabled_agent_definitions(&agents, &settings);
        assert_eq!(enabled.len(), 1);
        assert_eq!(enabled[0].name, "task");
    }

    #[test]
    fn settings_override_unknown_pattern_is_rejected() {
        let parent = model("openai", "gpt-4.1");
        let available = vec![model("anthropic", "claude-sonnet-4-5")];
        let definition = def("reviewer", Some(vec!["anthropic/claude-sonnet-4-5"]));
        let settings = AgentRuntimeSettings {
            enabled: Some(true),
            model: Some("totally-missing/model".to_owned()),
                tools: None,
            };
        let error = resolve_agent_model(&definition, Some(&settings), &parent, &available)
            .expect_err("invalid settings override must not fall through");
        let message = error.to_string();
        assert!(message.contains("settings.agents.reviewer.model"), "{message}");
        assert!(message.contains("totally-missing/model"), "{message}");
    }
}

#[cfg(test)]
mod contract_tests {
    use super::*;

    fn parse(content: &str) -> Result<AgentDefinition> {
        parse_agent_definition(Path::new("role.md"), content, AgentDefinitionSource::User, true)
    }

    #[test]
    fn definition_without_contract_fields_decodes_to_unlimited_defaults() {
        let definition = parse("---\nname: task\ndescription: plain\ntools: [read]\n---\nprompt")
            .expect("definition without contract fields");
        assert_eq!(definition.max_turns, None);
        assert_eq!(definition.max_tool_calls, None);
        assert_eq!(definition.timeout_secs, None);
        assert!(definition.disallowed_tools.is_empty());
        assert_eq!(definition.capability_ceiling, None);
    }

    #[test]
    fn definition_with_contract_fields_decodes_values() {
        let definition = parse(
            "---\n\
             name: bounded\n\
             description: bounded role\n\
             maxTurns: 3\n\
             maxToolCalls: 25\n\
             timeoutSecs: 300\n\
             disallowedTools: [bash, browser]\n\
             capabilityCeiling: [read, write]\n\
             ---\n\
             prompt",
        )
        .expect("definition with contract fields");
        assert_eq!(definition.max_turns, Some(3));
        assert_eq!(definition.max_tool_calls, Some(25));
        assert_eq!(definition.timeout_secs, Some(300));
        assert_eq!(definition.disallowed_tools, vec!["bash", "browser"]);
        assert_eq!(
            definition.capability_ceiling,
            Some(CapabilityCeiling {
                read: true,
                write: true,
                exec: false,
            })
        );
    }

    #[test]
    fn kebab_case_contract_field_names_are_accepted() {
        let definition = parse(
            "---\n\
             name: kebab\n\
             description: kebab role\n\
             max-turns: 2\n\
             max-tool-calls: 4\n\
             timeout-secs: 60\n\
             disallowed-tools:\n\
             - mcp\n\
             capability-ceiling: read\n\
             ---\n\
             prompt",
        )
        .expect("kebab-case contract fields");
        assert_eq!(definition.max_turns, Some(2));
        assert_eq!(definition.max_tool_calls, Some(4));
        assert_eq!(definition.timeout_secs, Some(60));
        assert_eq!(definition.disallowed_tools, vec!["mcp"]);
        assert_eq!(
            definition.capability_ceiling,
            Some(CapabilityCeiling {
                read: true,
                write: false,
                exec: false,
            })
        );
    }

    #[test]
    fn zero_contract_values_are_rejected() {
        for (field, content) in [
            ("maxTurns", "maxTurns: 0\n"),
            ("maxToolCalls", "maxToolCalls: 0\n"),
            ("timeoutSecs", "timeoutSecs: 0\n"),
        ] {
            let definition = parse(&format!(
                "---\nname: bounded\ndescription: d\n{content}---\nprompt"
            ))
            .expect_err(&format!("{field} zero must be rejected"));
            let message = definition.to_string();
            assert!(message.contains(field), "{message}");
            assert!(message.contains("positive"), "{message}");
        }
    }

    #[test]
    fn non_numeric_contract_values_are_rejected() {
        let definition = parse("---\nname: bounded\ndescription: d\nmaxTurns: many\n---\nprompt")
            .expect_err("non-numeric maxTurns must be rejected");
        assert!(definition.to_string().contains("maxTurns"), "{definition:#}");
    }

    #[test]
    fn unsupported_capability_ceiling_item_is_rejected() {
        let definition = parse(
            "---\nname: bounded\ndescription: d\ncapabilityCeiling: [read, delete]\n---\nprompt",
        )
        .expect_err("unknown ceiling item must be rejected");
        assert!(
            definition.to_string().contains("delete"),
            "{definition:#}"
        );
    }

    #[test]
    fn ceiling_helpers_expose_allowed_capabilities_and_unrestricted() {
        assert!(CapabilityCeiling::default().is_unrestricted() == false);
        let all = CapabilityCeiling {
            read: true,
            write: true,
            exec: true,
        };
        assert!(all.is_unrestricted());
        assert_eq!(
            all.allowed_capabilities(),
            vec![ToolCapability::Read, ToolCapability::Write, ToolCapability::Exec]
        );
        let read_only = CapabilityCeiling {
            read: true,
            write: false,
            exec: false,
        };
        assert!(!read_only.is_unrestricted());
        assert_eq!(read_only.allowed_capabilities(), vec![ToolCapability::Read]);
    }
}

#[cfg(test)]
mod child_tool_validation_tests {
    use super::*;

    fn def_with_tools(name: &str, tools: &[&str]) -> AgentDefinition {
        AgentDefinition {
            name: name.to_owned(),
            description: "d".to_owned(),
            system_prompt: "p".to_owned(),
            tools: Some(tools.iter().map(|t| (*t).to_owned()).collect()),
            autoload_skills: Vec::new(),
            model: None,
            thinking_level: None,
            max_turns: None,
            max_tool_calls: None,
            timeout_secs: None,
            disallowed_tools: Vec::new(),
            capability_ceiling: None,
            source: AgentDefinitionSource::Bundled,
            path: None,
            trusted: true,
            kind: AgentDefinitionKind::Agent,
            personality: None,
            soft_budget: None,
        }
    }

    #[test]
    fn is_child_plumbing_tool_recognizes_yield_and_orchestration_plumbing() {
        for name in ["todo", "process", "task", "hub", "goal", "yield"] {
            assert!(
                is_child_plumbing_tool(name),
                "{name:?} must be child plumbing"
            );
        }
        assert!(!is_child_plumbing_tool("read"), "read is a real tool, not plumbing");
        assert!(!is_child_plumbing_tool("yieldx"), "yieldx must not match yield");
        assert_eq!(CHILD_PLUMBING_TOOLS.len(), 6);
        // yield must NOT leak into the main-session built-in set: the validator
        // accepts it only because it is auto-provided child plumbing.
        assert!(
            !crate::TOOL_NAMES.contains(&"yield"),
            "yield must remain absent from the main-session tool set"
        );
    }

    #[test]
    fn qwen_style_definition_declaring_yield_is_not_rejected_as_unsupported() {
        // Regression for the reported bug: an agent definition (the `qwen` role)
        // declares `yield` among its tools. The validator must treat `yield` as
        // a known child tool — it is auto-appended to every orchestration child
        // by the factory regardless of declarations — so the declaration is
        // valid (and redundant), never reported as unknown.
        let definition = def_with_tools("qwen", &["read", "bash", "yield"]);
        let unsupported = unsupported_agent_tools(&definition, None);
        assert!(
            unsupported.is_empty(),
            "declaring yield must not be unsupported: {unsupported:?}"
        );
    }

    #[test]
    fn definition_declaring_only_plumbing_tools_has_no_unsupported_tools() {
        let definition = def_with_tools("plumbing", &["todo", "process", "task", "hub", "goal", "yield"]);
        let unsupported = unsupported_agent_tools(&definition, None);
        assert!(
            unsupported.is_empty(),
            "all orchestration plumbing must be accepted: {unsupported:?}"
        );
    }

    #[test]
    fn unknown_tool_alongside_yield_is_reported_but_never_fatal() {
        // A definition declaring BOTH yield and a genuinely unknown tool must
        // report the unknown one — but only for the unknown tool; yield must
        // not be listed as unsupported. The report is advisory: the definition
        // stays compatible (silently ignored, OMP-aligned).
        let definition = def_with_tools("mixed", &["read", "yield", "not_a_real_tool"]);
        let unsupported = unsupported_agent_tools(&definition, None);
        assert_eq!(
            unsupported,
            vec!["not_a_real_tool".to_owned()],
            "only the truly unknown tool is unsupported: {unsupported:?}"
        );
        let error = agent_compatibility_error(
            &definition,
            None,
            &Model::default(),
            &available_models(),
        );
        assert!(
            error.is_none(),
            "unknown tools must not make the definition incompatible: {error:?}"
        );
    }

    #[test]
    fn settings_tools_override_declaring_yield_is_accepted() {
        // A settings.agents.<name>.tools override listing yield is equally
        // valid and must not be rejected (effective tools come from the
        // settings override when present).
        let definition = def_with_tools("qwen", &["read", "yield"]);
        let settings = AgentRuntimeSettings {
            enabled: Some(true),
            model: None,
            tools: Some(vec!["bash".to_owned(), "yield".to_owned()]),
        };
        let unsupported = unsupported_agent_tools(&definition, Some(&settings));
        assert!(
            unsupported.is_empty(),
            "settings override declaring yield must be accepted: {unsupported:?}"
        );
    }

    #[test]
    fn qa_style_definition_with_ghost_tools_stays_available() {
        // The qa role declares `yield_output` (a ghost tool that exists in
        // neither OMP nor rpi) plus genuinely unknown names next to valid
        // tools. OMP silently ignores unknown declarations; rpi must mirror
        // that: every unknown name is reported (for the deduped warning) but
        // the definition remains compatible and spawnable.
        let definition = def_with_tools(
            "qa",
            &[
                "read", "grep", "glob", "bash", "edit", "write", "yield",
                "yield_output", "computer", "imaginary_tool",
            ],
        );
        let unsupported = unsupported_agent_tools(&definition, None);
        assert_eq!(
            unsupported,
            vec!["computer", "imaginary_tool", "yield_output"],
            "only the ghost/unknown names are unsupported: {unsupported:?}"
        );
        let error = agent_compatibility_error(
            &definition,
            None,
            &Model::default(),
            &available_models(),
        );
        assert!(
            error.is_none(),
            "a qa-style definition with ghost tools must stay available: {error:?}"
        );
    }

    #[test]
    fn is_known_child_tool_accepts_plumbing_and_builtins_only() {
        for name in CHILD_PLUMBING_TOOLS {
            assert!(is_known_child_tool(name), "{name:?} is plumbing");
        }
        for name in crate::TOOL_NAMES {
            assert!(is_known_child_tool(name), "{name:?} is a built-in");
        }
        for name in ["yield_output", "computer", "imaginary_tool"] {
            assert!(!is_known_child_tool(name), "{name:?} must be unknown");
        }
    }

    #[test]
    fn unknown_tools_warning_names_the_agent_and_unknown_tool() {
        let definition = def_with_tools("broken", &["not_a_real_tool"]);
        let unsupported = unsupported_agent_tools(&definition, None);
        let warning = unknown_tools_warning(&definition.name, &unsupported);
        assert!(warning.contains("broken"), "{warning}");
        assert!(warning.contains("not_a_real_tool"), "{warning}");
        assert!(warning.contains("ignoring"), "{warning}");
        assert!(warning.contains("not injected"), "{warning}");
        // The listed supported set must not advertise yield as a main tool:
        // yield is child plumbing, surfaced via the plumbing list, not
        // TOOL_NAMES. (The message currently prints TOOL_NAMES; this guards
        // against yield silently joining the main set.)
        assert!(!warning.contains("yield") || unsupported.iter().any(|t| t == "yield"));
    }

    #[test]
    fn frontmatter_parse_preserves_unknown_tool_names() {
        // qa-style definitions load at parse time regardless of tool names:
        // validation is purely runtime-side, and unknown names are reported
        // but never fatal.
        let definition = parse_agent_definition(
            Path::new("qa.md"),
            "---\nname: qa\ndescription: qa\ntools: [read, yield_output]\n---\nprompt",
            AgentDefinitionSource::User,
            true,
        )
        .expect("qa-style frontmatter must parse");
        assert_eq!(
            definition.tools.as_deref(),
            Some(&["read".to_owned(), "yield_output".to_owned()][..]),
            "both declared names survive parsing"
        );
    }
}


#[cfg(test)]
mod persona_tests {
    use super::*;

    fn parse(content: &str) -> Result<AgentDefinition> {
        parse_persona_definition(Path::new("persona.md"), content, AgentDefinitionSource::User, true)
    }

    fn write_persona_md(dir: &Path, body: &str) {
        fs::create_dir_all(dir).expect("persona dir");
        fs::write(dir.join("persona.md"), body).expect("persona.md");
    }

    #[test]
    fn ordinary_agent_defaults_to_agent_kind_without_persona_fields() {
        let definition = parse_agent_definition(
            Path::new("role.md"),
            "---\nname: task\ndescription: plain\n---\nprompt",
            AgentDefinitionSource::User,
            true,
        )
        .expect("parse");
        assert_eq!(definition.kind, AgentDefinitionKind::Agent);
        assert!(!definition.is_persona());
        assert!(definition.persona_root().is_none());
        assert!(definition.personality.is_none());
        assert!(definition.soft_budget.is_none());
    }

    #[test]
    fn ordinary_agent_ignores_persona_only_frontmatter() {
        let definition = parse_agent_definition(
            Path::new("role.md"),
            "---\nname: task\ndescription: plain\npersonality: hidden\nsoftBudget:\n  maxRequests: 1\n---\nprompt",
            AgentDefinitionSource::User,
            true,
        )
        .expect("ordinary agent parses");
        assert_eq!(definition.kind, AgentDefinitionKind::Agent);
        assert!(definition.personality.is_none());
        assert!(definition.soft_budget.is_none());
    }

    #[test]
    fn personality_scalar_is_parsed() {
        let definition = parse(
            "---\nname: p\ndescription: d\npersonality: calm and concise\n---\nprompt",
        )
        .expect("parse");
        assert_eq!(definition.personality.as_deref(), Some("calm and concise"));
    }

    #[test]
    fn quoted_personality_is_unquoted() {
        let definition = parse(
            "---\nname: p\ndescription: d\npersonality: \"warm, direct\"\n---\nprompt",
        )
        .expect("parse");
        assert_eq!(definition.personality.as_deref(), Some("warm, direct"));
    }

    #[test]
    fn blank_personality_is_none() {
        let definition = parse("---\nname: p\ndescription: d\npersonality:   \n---\nprompt")
            .expect("parse");
        assert!(definition.personality.is_none());
    }

    #[test]
    fn soft_budget_camel_case_fields_decode() {
        let definition = parse(
            "---\nname: p\ndescription: d\nsoftBudget:\n  maxRequests: 5\n  maxTokens: 90000\n  yieldAfter: 3\n---\nprompt",
        )
        .expect("parse");
        let budget = definition.soft_budget.expect("soft budget present");
        assert_eq!(budget.max_requests, Some(5));
        assert_eq!(budget.max_tokens, Some(90_000));
        assert_eq!(budget.yield_after, Some(3));
    }

    #[test]
    fn soft_budget_kebab_case_fields_and_parent_decode() {
        let definition = parse(
            "---\nname: p\ndescription: d\nsoft-budget:\n  max-requests: 7\n  max-tokens: 1234\n  yield-after: 2\n---\nprompt",
        )
        .expect("parse");
        let budget = definition.soft_budget.expect("soft budget present");
        assert_eq!(budget.max_requests, Some(7));
        assert_eq!(budget.max_tokens, Some(1234));
        assert_eq!(budget.yield_after, Some(2));
    }

    #[test]
    fn soft_budget_partial_block_leaves_unspecified_knobs_unlimited() {
        let definition = parse(
            "---\nname: p\ndescription: d\nsoftBudget:\n  maxRequests: 5\n---\nprompt",
        )
        .expect("parse");
        let budget = definition.soft_budget.expect("soft budget present");
        assert_eq!(budget.max_requests, Some(5));
        assert!(budget.max_tokens.is_none());
        assert!(budget.yield_after.is_none());
    }

    #[test]
    fn soft_budget_zero_values_are_rejected() {
        for (field, line) in [
            ("maxRequests", "maxRequests: 0"),
            ("maxTokens", "maxTokens: 0"),
            ("yieldAfter", "yieldAfter: 0"),
        ] {
            let error = parse(&format!(
                "---\nname: p\ndescription: d\nsoftBudget:\n  {line}\n---\nprompt"
            ))
            .expect_err("zero soft budget field must be rejected");
            let message = error.to_string();
            assert!(message.contains(field), "{message}");
            assert!(message.contains("positive"), "{message}");
        }
    }

    #[test]
    fn soft_budget_non_numeric_nested_field_is_rejected() {
        let error = parse("---\nname: p\ndescription: d\nsoftBudget:\n  maxRequests: many\n---\nprompt")
            .expect_err("non-numeric soft budget field must be rejected");
        let message = error.to_string();
        assert!(message.contains("maxRequests"), "{message}");
        assert!(message.contains("positive integer"), "{message}");
    }

    #[test]
    fn soft_budget_scalar_value_is_rejected_as_non_map() {
        let error = parse("---\nname: p\ndescription: d\nsoftBudget: 5\n---\nprompt")
            .expect_err("scalar softBudget must be rejected");
        assert!(error.to_string().contains("nested map"), "{error}");
    }

    #[test]
    fn soft_budget_absent_block_is_none() {
        let definition = parse("---\nname: p\ndescription: d\n---\nprompt").expect("parse");
        assert!(definition.soft_budget.is_none());
    }

    #[test]
    fn indented_frontmatter_without_parent_block_is_rejected() {
        let error = parse("---\nname: p\ndescription: d\n  orphan: 5\n---\nprompt")
            .expect_err("orphan indented line must be rejected");
        assert!(
            format!("{error:#}").contains("indented without a parent block"),
            "{error:#}"
        );
    }

    #[test]
    fn user_persona_is_discovered_as_persona_with_valid_root() {
        let root = tempfile::tempdir().expect("root");
        let persona_dir = root.path().join("personas").join("mentor");
        write_persona_md(&persona_dir, "---\nname: mentor\ndescription: d\n---\nmentor prompt");
        let catalog =
            AgentCatalog::discover(&AgentDiscoveryOptions::new(root.path(), root.path()))
                .expect("discover");
        let mentor = catalog.get("mentor").expect("persona discovered");
        assert_eq!(mentor.kind, AgentDefinitionKind::Persona);
        assert!(mentor.is_persona());
        assert_eq!(mentor.source, AgentDefinitionSource::User);
        assert_eq!(mentor.persona_root().as_deref(), Some(persona_dir.as_path()));
    }

    #[test]
    fn project_persona_excluded_when_untrusted() {
        let root = tempfile::tempdir().expect("root");
        let persona_dir = root.path().join(".pi").join("personas").join("mentor");
        write_persona_md(&persona_dir, "---\nname: mentor\ndescription: d\n---\nmentor prompt");
        let options = AgentDiscoveryOptions {
            cwd: root.path().to_path_buf(),
            agent_dir: root.path().to_path_buf(),
            project_trusted: false,
        };
        let catalog = AgentCatalog::discover(&options).expect("discover");
        assert!(
            catalog.get("mentor").is_none(),
            "untrusted project persona must be excluded"
        );
    }

    #[cfg(unix)]
    #[test]
    fn user_persona_symlink_root_is_rejected() {
        let root = tempfile::tempdir().expect("root");
        let outside = root.path().join("outside");
        write_persona_md(
            &outside,
            "---\nname: mentor\ndescription: d\n---\nmentor prompt",
        );
        let personas = root.path().join("personas");
        fs::create_dir_all(&personas).expect("personas root");
        std::os::unix::fs::symlink(&outside, personas.join("mentor"))
            .expect("persona symlink");

        let error = AgentCatalog::discover(&AgentDiscoveryOptions::new(
            root.path(),
            root.path(),
        ))
        .expect_err("symlinked persona root must fail");
        assert!(error.to_string().contains("must not be a symlink"), "{error:#}");
    }

    #[test]
    fn project_persona_discovered_when_trusted() {
        let root = tempfile::tempdir().expect("root");
        let persona_dir = root.path().join(".pi").join("personas").join("mentor");
        write_persona_md(&persona_dir, "---\nname: mentor\ndescription: d\n---\nmentor prompt");
        let mut options = AgentDiscoveryOptions::new(root.path(), root.path());
        options.project_trusted = true;
        let catalog = AgentCatalog::discover(&options).expect("discover");
        let mentor = catalog.get("mentor").expect("project persona discovered");
        assert_eq!(mentor.kind, AgentDefinitionKind::Persona);
        assert_eq!(mentor.source, AgentDefinitionSource::Project);
        assert_eq!(mentor.persona_root().as_deref(), Some(persona_dir.as_path()));
    }

    #[test]
    fn project_persona_wins_over_user_persona_with_same_name() {
        let root = tempfile::tempdir().expect("root");
        let user_persona = root.path().join("personas").join("shared");
        write_persona_md(&user_persona, "---\nname: shared\ndescription: user\n---\nuser prompt");
        let project_persona = root.path().join(".pi").join("personas").join("shared");
        write_persona_md(&project_persona, "---\nname: shared\ndescription: project\n---\nproject prompt");
        let mut options = AgentDiscoveryOptions::new(root.path(), root.path());
        options.project_trusted = true;
        let catalog = AgentCatalog::discover(&options).expect("discover");
        let shared = catalog.get("shared").expect("shared discovered once");
        assert_eq!(shared.source, AgentDefinitionSource::Project);
        assert_eq!(shared.kind, AgentDefinitionKind::Persona);
        assert!(shared.system_prompt.contains("project prompt"));
        assert_eq!(
            catalog.agents().iter().filter(|agent| agent.name == "shared").count(),
            1
        );
    }

    #[test]
    fn project_agent_wins_over_user_persona_with_same_name() {
        let root = tempfile::tempdir().expect("root");
        let user_persona = root.path().join("personas").join("dup");
        write_persona_md(&user_persona, "---\nname: dup\ndescription: persona\n---\npersona prompt");
        let project_agents = root.path().join(".pi").join("agents");
        fs::create_dir_all(&project_agents).expect("project agents");
        fs::write(
            project_agents.join("dup.md"),
            "---\nname: dup\ndescription: agent\n---\nagent prompt",
        )
        .expect("project agent");
        let mut options = AgentDiscoveryOptions::new(root.path(), root.path());
        options.project_trusted = true;
        let catalog = AgentCatalog::discover(&options).expect("discover");
        let dup = catalog.get("dup").expect("dup discovered once");
        assert_eq!(dup.source, AgentDefinitionSource::Project);
        // Precedence: project agents come before user personas, so the agent
        // (kind Agent) shadows the user persona of the same name.
        assert_eq!(dup.kind, AgentDefinitionKind::Agent);
        assert!(!dup.is_persona());
    }

    #[test]
    fn persona_rejects_bundled_agent_name() {
        let root = tempfile::tempdir().expect("root");
        let persona_dir = root.path().join("personas").join("task");
        write_persona_md(&persona_dir, "---\nname: task\ndescription: d\n---\nprompt");
        let error = AgentCatalog::discover(&AgentDiscoveryOptions::new(root.path(), root.path()))
            .expect_err("bundled persona name must be rejected");
        assert!(error.to_string().contains("bundled"), "{error}");
    }

    #[test]
    fn persona_name_must_match_its_directory() {
        let root = tempfile::tempdir().expect("root");
        let persona_dir = root.path().join("personas").join("mentor");
        write_persona_md(&persona_dir, "---\nname: guide\ndescription: d\n---\nprompt");
        let error = AgentCatalog::discover(&AgentDiscoveryOptions::new(root.path(), root.path()))
            .expect_err("name/directory mismatch must be rejected");
        assert!(error.to_string().contains("must match"), "{error}");
    }

    #[test]
    fn persona_state_files_and_subdirectories_are_ignored() {
        let root = tempfile::tempdir().expect("root");
        let persona_dir = root.path().join("personas").join("mentor");
        write_persona_md(&persona_dir, "---\nname: mentor\ndescription: d\n---\nprompt");
        fs::create_dir_all(persona_dir.join("memory")).expect("memory");
        fs::write(persona_dir.join("memory").join("entries.jsonl"), "{}").expect("entries");
        fs::create_dir_all(persona_dir.join("sessions")).expect("sessions");
        fs::write(persona_dir.join("sessions").join("abc.jsonl"), "{}").expect("session");
        fs::write(persona_dir.join("notes.txt"), "ignore me").expect("notes");
        // A stray .md at the personas root is not a persona directory.
        fs::write(root.path().join("personas").join("stray.md"), "ignore").expect("stray");
        let catalog = AgentCatalog::discover(&AgentDiscoveryOptions::new(root.path(), root.path()))
            .expect("discover");
        let mentor = catalog.get("mentor").expect("persona still discovered");
        assert_eq!(mentor.kind, AgentDefinitionKind::Persona);
        assert!(catalog.get("stray").is_none());
        assert!(catalog.get("memory").is_none());
        assert!(catalog.get("sessions").is_none());
    }

    #[test]
    fn child_directory_without_persona_md_is_skipped() {
        let root = tempfile::tempdir().expect("root");
        fs::create_dir_all(root.path().join("personas").join("empty")).expect("dir");
        let catalog = AgentCatalog::discover(&AgentDiscoveryOptions::new(root.path(), root.path()))
            .expect("discover");
        assert!(catalog.get("empty").is_none());
    }

    #[test]
    fn persona_personality_and_soft_budget_are_parsed_through_discovery() {
        let root = tempfile::tempdir().expect("root");
        let persona_dir = root.path().join("personas").join("mentor");
        write_persona_md(
            &persona_dir,
            "---\nname: mentor\ndescription: d\npersonality: calm\nsoftBudget:\n  maxRequests: 4\n  yieldAfter: 2\n---\nprompt",
        );
        let catalog = AgentCatalog::discover(&AgentDiscoveryOptions::new(root.path(), root.path()))
            .expect("discover");
        let mentor = catalog.get("mentor").expect("persona discovered");
        assert_eq!(mentor.personality.as_deref(), Some("calm"));
        let budget = mentor.soft_budget.expect("soft budget parsed");
        assert_eq!(budget.max_requests, Some(4));
        assert_eq!(budget.yield_after, Some(2));
        assert!(budget.max_tokens.is_none());
    }
}