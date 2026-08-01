use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Read as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use pi_agent::ThinkingLevel;

use crate::resources::CONFIG_DIR_NAME;

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

#[derive(Clone, Debug, Default)]
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
