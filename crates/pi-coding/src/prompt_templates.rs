//! File-backed prompt templates and deterministic, non-recursive expansion.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use regex::{Captures, Regex};
use serde::{Deserialize, Serialize};

use crate::resources::CONFIG_DIR_NAME;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResourceScope {
    Global,
    Project,
    Explicit,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptTemplate {
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argument_hint: Option<String>,
    pub content: String,
    pub file_path: PathBuf,
    pub scope: ResourceScope,
}

#[derive(Clone, Debug)]
pub struct LoadPromptTemplatesOptions {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    pub explicit_paths: Vec<PathBuf>,
    pub include_defaults: bool,
    pub include_project: bool,
}

impl LoadPromptTemplatesOptions {
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>, agent_dir: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            agent_dir: agent_dir.into(),
            explicit_paths: Vec::new(),
            include_defaults: true,
            include_project: false,
        }
    }
}

pub fn load_prompt_templates(options: &LoadPromptTemplatesOptions) -> Result<Vec<PromptTemplate>> {
    let cwd = absolute_path(&options.cwd)?;
    let agent_dir = absolute_path(&options.agent_dir)?;
    let project_pi = cwd.join(CONFIG_DIR_NAME);
    let mut templates = Vec::new();
    if options.include_defaults {
        load_templates_from_dir(
            &agent_dir.join("prompts"),
            ResourceScope::Global,
            &mut templates,
        )?;
        if options.include_project {
            load_templates_from_dir(
                &project_pi.join("prompts"),
                ResourceScope::Project,
                &mut templates,
            )?;
        }
    }
    for raw_path in &options.explicit_paths {
        let path = if raw_path.is_absolute() {
            raw_path.clone()
        } else {
            cwd.join(raw_path)
        };
        let path = lexical_normalize(&path);
        if is_under(&path, &project_pi) && !options.include_project {
            continue;
        }
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                bail!("prompt template path does not exist: {}", path.display());
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reading prompt template path {}", path.display()));
            }
        };
        let scope = if is_under(&path, &project_pi) {
            ResourceScope::Project
        } else if is_under(&path, &agent_dir) {
            ResourceScope::Global
        } else {
            ResourceScope::Explicit
        };
        if metadata.is_dir() {
            load_templates_from_dir(&path, scope, &mut templates)?;
        } else if metadata.is_file() && path.extension().is_some_and(|ext| ext == "md") {
            templates.push(load_template_from_file(&path, scope)?);
        } else {
            bail!("prompt template path is not a markdown file or directory: {}", path.display());
        }
    }
    Ok(dedupe_templates(templates))
}

fn load_templates_from_dir(
    directory: &Path,
    scope: ResourceScope,
    templates: &mut Vec<PromptTemplate>,
) -> Result<()> {
    let mut entries = match fs::read_dir(directory) {
        Ok(entries) => entries
            .collect::<std::io::Result<Vec<_>>>()
            .with_context(|| format!("reading prompt template directory {}", directory.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading prompt template directory {}", directory.display()));
        }
    };
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let metadata = fs::metadata(&path)
            .with_context(|| format!("reading prompt template metadata {}", path.display()))?;
        if metadata.is_file() && path.extension().is_some_and(|extension| extension == "md") {
            templates.push(load_template_from_file(&path, scope)?);
        }
    }
    Ok(())
}

fn load_template_from_file(path: &Path, scope: ResourceScope) -> Result<PromptTemplate> {
    let content = crate::read_resource_text(path, "prompt template")?;
    let (frontmatter, body) = parse_frontmatter(&content)
        .with_context(|| format!("parsing prompt template frontmatter {}", path.display()))?;
    let name = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(str::to_owned)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| anyhow::anyhow!("prompt template has no valid name: {}", path.display()))?;
    let mut description = frontmatter.get("description").cloned().unwrap_or_default();
    if description.is_empty()
        && let Some(line) = body.lines().find(|line| !line.trim().is_empty())
    {
        description = line.chars().take(60).collect();
        if line.chars().count() > 60 {
            description.push_str("...");
        }
    }
    let argument_hint = frontmatter
        .get("argument-hint")
        .cloned()
        .filter(|hint| !hint.is_empty());
    Ok(PromptTemplate {
        name,
        description,
        argument_hint,
        content: body,
        file_path: path.to_path_buf(),
        scope,
    })
}

fn dedupe_templates(templates: Vec<PromptTemplate>) -> Vec<PromptTemplate> {
    // Later-loaded templates shadow earlier ones with the same name, matching
    // upstream behavior. The shadowing (later) template keeps its own position
    // and the earlier duplicate is dropped, so only one command is emitted per
    // name. Load order is global prompts, then project prompts, then explicit
    // paths, so project/explicit templates override global ones of the same name.
    let mut last_index = BTreeMap::<String, usize>::new();
    for (index, template) in templates.iter().enumerate() {
        last_index.insert(template.name.clone(), index);
    }
    templates
        .into_iter()
        .enumerate()
        .filter_map(|(index, template)| {
            if last_index.get(&template.name) == Some(&index) {
                Some(template)
            } else {
                None
            }
        })
        .collect()
}

/// Parse command arguments with the original prompt-template quoting rules.
#[must_use]
pub fn parse_command_args(input: &str) -> Vec<String> {
    let mut arguments = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    for character in input.chars() {
        if let Some(expected) = quote {
            if character == expected {
                quote = None;
            } else {
                current.push(character);
            }
        } else if character == '\'' || character == '"' {
            quote = Some(character);
        } else if character.is_whitespace() {
            if !current.is_empty() {
                arguments.push(std::mem::take(&mut current));
            }
        } else {
            current.push(character);
        }
    }
    if !current.is_empty() {
        arguments.push(current);
    }
    arguments
}

/// Substitute placeholders in one regex pass. Inserted arguments/defaults are
/// never scanned again, so values containing `$1` or `$@` remain literal.
#[must_use]
pub fn substitute_args(content: &str, arguments: &[String]) -> String {
    let pattern = Regex::new(
        r"\$\{(\d+|ARGUMENTS|@):-([^}]*)\}|\$\{@:(\d+)(?::(\d+))?\}|\$(ARGUMENTS|@|\d+)",
    )
    .expect("static prompt placeholder regex");
    let all = arguments.join(" ");
    pattern
        .replace_all(content, |captures: &Captures<'_>| {
            if let Some(target) = captures.get(1) {
                let value = if target.as_str() == "@" || target.as_str() == "ARGUMENTS" {
                    all.as_str()
                } else {
                    target
                        .as_str()
                        .parse::<usize>()
                        .ok()
                        .and_then(|index| index.checked_sub(1))
                        .and_then(|index| arguments.get(index))
                        .map_or("", String::as_str)
                };
                return if value.is_empty() {
                    captures.get(2).map_or("", |value| value.as_str()).to_owned()
                } else {
                    value.to_owned()
                };
            }
            if let Some(start) = captures.get(3) {
                let start = start
                    .as_str()
                    .parse::<usize>()
                    .unwrap_or(1)
                    .saturating_sub(1);
                let length = captures
                    .get(4)
                    .and_then(|length| length.as_str().parse::<usize>().ok());
                let end = length
                    .map_or(arguments.len(), |length| start.saturating_add(length))
                    .min(arguments.len());
                return arguments
                    .get(start.min(arguments.len())..end)
                    .unwrap_or_default()
                    .join(" ");
            }
            let simple = captures.get(5).map_or("", |value| value.as_str());
            if simple == "@" || simple == "ARGUMENTS" {
                return all.clone();
            }
            simple
                .parse::<usize>()
                .ok()
                .and_then(|index| index.checked_sub(1))
                .and_then(|index| arguments.get(index))
                .cloned()
                .unwrap_or_default()
        })
        .into_owned()
}

#[must_use]
pub fn expand_prompt_template(text: &str, templates: &[PromptTemplate]) -> String {
    let Some(command) = text.strip_prefix('/') else {
        return text.to_owned();
    };
    let split = command.find(char::is_whitespace);
    let (name, arguments) = match split {
        Some(index) => (&command[..index], command[index..].trim_start()),
        None => (command, ""),
    };
    if name.is_empty() {
        return text.to_owned();
    }
    let Some(template) = templates.iter().find(|template| template.name == name) else {
        return text.to_owned();
    };
    substitute_args(&template.content, &parse_command_args(arguments))
}

fn parse_frontmatter(content: &str) -> Result<(BTreeMap<String, String>, String)> {
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    if !normalized.starts_with("---") {
        return Ok((BTreeMap::new(), normalized));
    }
    if !normalized.starts_with("---\n") {
        bail!("opening frontmatter delimiter must occupy its own line");
    }
    let Some(end) = normalized[4..].find("\n---") else {
        bail!("frontmatter is missing a closing --- delimiter");
    };
    let header_end = 4 + end;
    let header = &normalized[4..header_end];
    let after_delimiter = header_end + 4;
    if normalized
        .as_bytes()
        .get(after_delimiter)
        .is_some_and(|byte| *byte != b'\n')
    {
        bail!("closing frontmatter delimiter must occupy its own line");
    }
    let body = normalized[after_delimiter..].trim().to_owned();
    let mut values = BTreeMap::new();
    for (index, line) in header.lines().enumerate() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            bail!("invalid frontmatter line {}: expected key: value", index + 1);
        };
        if key.trim().is_empty() {
            bail!("invalid frontmatter line {}: key is empty", index + 1);
        }
        let value = unquote(value.trim())?;
        values.insert(key.trim().to_owned(), value);
    }
    Ok((values, body))
}

fn unquote(value: &str) -> Result<String> {
    if value.is_empty() {
        return Ok(String::new());
    }
    if let Some(inner) = value.strip_prefix('"').and_then(|value| value.strip_suffix('"')) {
        return Ok(inner
            .replace("\\n", "\n")
            .replace("\\t", "\t")
            .replace("\\\"", "\"")
            .replace("\\\\", "\\"));
    }
    if let Some(inner) = value.strip_prefix('\'').and_then(|value| value.strip_suffix('\'')) {
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

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(lexical_normalize(path))
    } else {
        Ok(lexical_normalize(
            &std::env::current_dir().context("getting current directory")?.join(path),
        ))
    }
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

fn is_under(target: &Path, root: &Path) -> bool {
    target == root || target.starts_with(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn template(name: &str, content: &str, scope: ResourceScope, file_path: &str) -> PromptTemplate {
        PromptTemplate {
            name: name.to_owned(),
            description: String::new(),
            argument_hint: None,
            content: content.to_owned(),
            file_path: PathBuf::from(file_path),
            scope,
        }
    }

    #[test]
    fn no_duplicates_preserves_order_and_content() {
        let templates = vec![
            template("alpha", "a", ResourceScope::Global, "/g/alpha.md"),
            template("beta", "b", ResourceScope::Project, "/p/beta.md"),
            template("gamma", "c", ResourceScope::Explicit, "/e/gamma.md"),
        ];
        let deduped = dedupe_templates(templates);
        assert_eq!(deduped.len(), 3);
        assert_eq!(deduped[0].name, "alpha");
        assert_eq!(deduped[1].name, "beta");
        assert_eq!(deduped[2].name, "gamma");
        assert_eq!(deduped[0].content, "a");
        assert_eq!(deduped[1].content, "b");
        assert_eq!(deduped[2].content, "c");
    }

    #[test]
    fn later_duplicate_shadows_earlier_at_later_position() {
        let templates = vec![
            template("shared", "global-content", ResourceScope::Global, "/g/shared.md"),
            template("other", "other-content", ResourceScope::Global, "/g/other.md"),
            template("shared", "explicit-content", ResourceScope::Explicit, "/e/shared.md"),
        ];
        let deduped = dedupe_templates(templates);
        assert_eq!(deduped.len(), 2);
        // The later-loaded "shared" wins and occupies the later position; the
        // earlier global "shared" is dropped, so only one command is emitted.
        assert_eq!(deduped[0].name, "other");
        assert_eq!(deduped[1].name, "shared");
        assert_eq!(deduped[1].content, "explicit-content");
        assert_eq!(deduped[1].scope, ResourceScope::Explicit);
        assert_eq!(deduped[1].file_path, PathBuf::from("/e/shared.md"));
    }

    #[test]
    fn same_scope_duplicate_uses_the_later_template() {
        let templates = vec![
            template("review", "first", ResourceScope::Explicit, "/a/review.md"),
            template("review", "second", ResourceScope::Explicit, "/b/review.md"),
        ];
        let deduped = dedupe_templates(templates);
        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].content, "second");
        assert_eq!(deduped[0].file_path, PathBuf::from("/b/review.md"));
    }

    #[test]
    fn project_template_shadows_global_of_same_name() {
        let templates = vec![
            template("deploy", "global deploy", ResourceScope::Global, "/g/deploy.md"),
            template("deploy", "project deploy", ResourceScope::Project, "/p/deploy.md"),
        ];
        let deduped = dedupe_templates(templates);
        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].name, "deploy");
        assert_eq!(deduped[0].content, "project deploy");
        assert_eq!(deduped[0].scope, ResourceScope::Project);
        assert_eq!(deduped[0].file_path, PathBuf::from("/p/deploy.md"));
    }
}