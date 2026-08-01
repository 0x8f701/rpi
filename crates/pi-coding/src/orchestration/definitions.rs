use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Read as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use pi_agent::ThinkingLevel;
use pi_ai::Model;

use crate::resources::CONFIG_DIR_NAME;
use crate::settings::{AgentRuntimeSettings, Settings};

const MAX_AGENT_NAME_LENGTH: usize = 64;
const MAX_AGENT_DESCRIPTION_LENGTH: usize = 1024;
const MAX_AGENT_DEFINITION_BYTES: u64 = 256 * 1024;
const MAX_AGENT_CATALOG_BYTES: u64 = 2 * 1024 * 1024;
const BUNDLED_TASK_PROMPT: &str = include_str!("task_agent.md");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentDefinitionSource {
    Project,
    User,
    Bundled,
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
    pub source: AgentDefinitionSource,
    pub path: Option<PathBuf>,
    pub trusted: bool,
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
            append_directory(
                &cwd.join(CONFIG_DIR_NAME).join("agents"),
                AgentDefinitionSource::Project,
                options.project_trusted,
                &mut seen,
                &mut agents,
                &mut total_bytes,
            )?;
        }
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

/// Why a child session model was chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentModelSource {
    /// Explicit `settings.agents.<name>.model` override.
    SettingsOverride,
    /// First matching entry from the agent definition `model` list.
    DefinitionFallback,
    /// Parent session model when no override/list match exists.
    Parent,
}

/// Resolved child-session model plus the precedence tier that produced it.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedAgentModel {
    pub model: Model,
    pub source: AgentModelSource,
    /// Pattern that matched when source is SettingsOverride or DefinitionFallback.
    pub matched_pattern: Option<String>,
    /// Thinking level parsed from a model suffix such as `:high` or `:max`.
    pub thinking_level: Option<ThinkingLevel>,
}

/// Actionable error when a disabled agent is requested for spawn.
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

/// Unsupported tools requested by an agent's effective child-tool allow-list.
#[must_use]
pub fn unsupported_agent_tools(
    definition: &AgentDefinition,
    settings: Option<&AgentRuntimeSettings>,
) -> Vec<String> {
    effective_agent_tool_names(definition, settings)
        .into_iter()
        .flatten()
        .filter(|tool| {
            !matches!(tool.as_str(), "todo" | "process" | "task" | "hub" | "goal")
                && !crate::TOOL_NAMES.contains(&tool.as_str())
        })
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Actionable error when an agent requests child tools this runtime cannot provide.
pub fn agent_unsupported_tools_error(name: &str, unsupported: &[String]) -> anyhow::Error {
    anyhow!(
        "agent `{name}` is unavailable because it requests unsupported child tools: {}; remove those tools from the agent definition or settings.agents.{name}.tools; supported child tools: {}",
        unsupported.join(", "),
        crate::TOOL_NAMES.join(", "),
    )
}

/// Actionable error when an agent has an invalid model configuration.
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

/// Filter agent definitions to those enabled and compatible with this runtime.
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

/// Whether an agent is compatible with the runtime's supported tools and model catalog.
#[must_use]
pub fn agent_compatibility_error(
    definition: &AgentDefinition,
    settings: Option<&AgentRuntimeSettings>,
    parent_model: &Model,
    available: &[Model],
) -> Option<anyhow::Error> {
    let unsupported = unsupported_agent_tools(definition, settings);
    if !unsupported.is_empty() {
        return Some(agent_unsupported_tools_error(&definition.name, &unsupported));
    }
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

/// Convenience wrapper that pulls the agent entry from effective [`Settings`].
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
        "xhigh" | "max" => ThinkingLevel::Xhigh,
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
    vec![AgentDefinition {
        name: "task".to_owned(),
        description: "General-purpose subagent with full capabilities for delegated multi-step tasks".to_owned(),
        system_prompt: BUNDLED_TASK_PROMPT.trim().to_owned(),
        tools: None,
        autoload_skills: Vec::new(),
        model: None,
        thinking_level: None,
        source: AgentDefinitionSource::Bundled,
        path: None,
        trusted: true,
    }]
}

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

pub fn parse_agent_definition(
    path: &Path,
    content: &str,
    source: AgentDefinitionSource,
    trusted: bool,
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
    Ok(AgentDefinition {
        name,
        description,
        system_prompt: body.trim().to_owned(),
        tools,
        autoload_skills,
        model,
        thinking_level,
        source,
        path: Some(path.to_path_buf()),
        trusted,
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
    for (index, raw_line) in header.lines().enumerate() {
        let line = raw_line.trim_end();
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let trimmed = line.trim_start();
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
        if value.is_empty() {
            current_list = Some(key.to_owned());
            fields.entry(key.to_owned()).or_default();
        } else {
            current_list = None;
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

fn parse_thinking_level(value: &str) -> Result<ThinkingLevel> {
    match value.trim().to_ascii_lowercase().as_str() {
        "off" => Ok(ThinkingLevel::Off),
        "minimal" => Ok(ThinkingLevel::Minimal),
        "low" => Ok(ThinkingLevel::Low),
        "medium" => Ok(ThinkingLevel::Medium),
        "high" => Ok(ThinkingLevel::High),
        "xhigh" | "max" => Ok(ThinkingLevel::Xhigh),
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
            source: AgentDefinitionSource::Bundled,
            path: None,
            trusted: true,
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
        assert_eq!(resolved.thinking_level, Some(ThinkingLevel::Xhigh));
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
