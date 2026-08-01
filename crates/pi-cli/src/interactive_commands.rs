//! Shared interactive slash-command metadata and matching helpers.

/// One executable built-in slash command exposed by interactive modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuiltinCommand {
    pub name: &'static str,
    pub description: &'static str,
    pub argument_hint: Option<&'static str>,
}

/// Source that supplies an executable interactive command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandSource {
    Builtin,
    Prompt,
    Skill,
    Extension,
}

/// One executable command exposed by an interactive adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InteractiveCommand {
    pub name: String,
    pub description: String,
    pub source: CommandSource,
}

/// Build the collision-free executable catalog shared by help, completion, and dispatch.
#[must_use]
pub fn executable_catalog(application: &pi_coding::Application) -> (Vec<InteractiveCommand>, Vec<String>) {
    let mut commands = BUILTIN_COMMANDS
        .iter()
        .map(|command| InteractiveCommand {
            name: command.name.to_owned(),
            description: command.description.to_owned(),
            source: CommandSource::Builtin,
        })
        .collect::<Vec<_>>();
    let builtin_names = BUILTIN_COMMANDS
        .iter()
        .map(|command| command.name)
        .collect::<std::collections::HashSet<_>>();
    let mut dynamic_names = std::collections::HashSet::new();
    let mut diagnostics = Vec::new();
    if let Some(resources) = application.resource_snapshot() {
        for prompt in &resources.prompts {
            if builtin_names.contains(prompt.name.as_str()) || !dynamic_names.insert(prompt.name.clone()) {
                diagnostics.push(format!("Prompt command '/{}' conflicts with another command and was excluded from autocomplete", prompt.name));
                continue;
            }
            commands.push(InteractiveCommand {
                name: prompt.name.clone(),
                description: prompt.description.clone(),
                source: CommandSource::Prompt,
            });
        }
        if resources.settings.enable_skill_commands.unwrap_or(true) {
            for skill in &resources.skills {
                let name = format!("skill:{}", skill.name);
                if builtin_names.contains(name.as_str()) || !dynamic_names.insert(name.clone()) {
                    diagnostics.push(format!("Skill command '/{name}' conflicts with another command and was excluded from autocomplete"));
                    continue;
                }
                commands.push(InteractiveCommand {
                    name,
                    description: skill.description.clone(),
                    source: CommandSource::Skill,
                });
            }
        }
    }
    if let Some(runtime) = application.extension_runtime() {
        for command in runtime.commands() {
            if builtin_names.contains(command.name.as_str()) || !dynamic_names.insert(command.name.clone()) {
                diagnostics.push(format!("Extension command '/{}' conflicts with another command and was excluded from autocomplete", command.name));
                continue;
            }
            commands.push(InteractiveCommand {
                name: command.name,
                description: command.description.unwrap_or_else(|| "Extension command".to_owned()),
                source: CommandSource::Extension,
            });
        }
    }
    commands.sort_by(|left, right| {
        let left_builtin = left.source == CommandSource::Builtin;
        let right_builtin = right.source == CommandSource::Builtin;
        right_builtin.cmp(&left_builtin).then_with(|| left.name.cmp(&right.name))
    });
    (commands, diagnostics)
}

/// Expand a prompt-template or skill command into the user prompt it executes.
pub fn expand_resource_command(
    application: &pi_coding::Application,
    name: &str,
    arguments: &str,
) -> anyhow::Result<Option<String>> {
    let Some(resources) = application.resource_snapshot() else {
        return Ok(None);
    };
    if let Some(template) = resources.prompts.iter().find(|template| template.name == name) {
        return Ok(Some(pi_coding::substitute_args(
            &template.content,
            &pi_coding::parse_command_args(arguments),
        )));
    }
    let Some(skill_name) = name.strip_prefix("skill:") else {
        return Ok(None);
    };
    if !resources.settings.enable_skill_commands.unwrap_or(true) {
        return Ok(None);
    }
    let Some(skill) = resources.skills.iter().find(|skill| skill.name == skill_name) else {
        return Ok(None);
    };
    let content = std::fs::read_to_string(&skill.file_path)?;
    let body = strip_frontmatter(&content).trim();
    let block = format!(
        "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {}.\n\n{}\n</skill>",
        skill.name, skill.file_path, skill.base_dir, body
    );
    Ok(Some(if arguments.is_empty() { block } else { format!("{block}\n\n{arguments}") }))
}

fn strip_frontmatter(content: &str) -> &str {
    let Some(rest) = content.strip_prefix("---\n") else {
        return content;
    };
    rest.find("\n---\n").map_or(content, |end| &rest[end + 5..])
}

/// Commands implemented by both full-screen and line-oriented interactive modes.
pub const BUILTIN_COMMANDS: &[BuiltinCommand] = &[
    BuiltinCommand {
        name: "help",
        description: "Show available commands",
        argument_hint: None,
    },
    BuiltinCommand {
        name: "settings",
        description: "Open settings menu",
        argument_hint: None,
    },
    BuiltinCommand {
        name: "model",
        description: "Select or switch model",
        argument_hint: Some("[provider/model]"),
    },
    BuiltinCommand {
        name: "scoped-models",
        description: "Enable or disable models for cycling",
        argument_hint: None,
    },
    BuiltinCommand {
        name: "models",
        description: "List available models",
        argument_hint: Some("[filter]"),
    },
    BuiltinCommand {
        name: "export",
        description: "Export session to HTML or JSONL",
        argument_hint: Some("[path]"),
    },
    BuiltinCommand {
        name: "import",
        description: "Import and resume a JSONL session",
        argument_hint: Some("<path.jsonl>"),
    },
    BuiltinCommand {
        name: "share",
        description: "Share session as a private GitHub gist",
        argument_hint: None,
    },
    BuiltinCommand {
        name: "copy",
        description: "Copy the last assistant message",
        argument_hint: None,
    },
    BuiltinCommand {
        name: "name",
        description: "Set or show the session name",
        argument_hint: Some("[name]"),
    },
    BuiltinCommand {
        name: "session",
        description: "Show current session information",
        argument_hint: None,
    },
    BuiltinCommand {
        name: "sessions",
        description: "List saved sessions",
        argument_hint: None,
    },
    BuiltinCommand {
        name: "changelog",
        description: "Show version history",
        argument_hint: None,
    },
    BuiltinCommand {
        name: "hotkeys",
        description: "Show keyboard shortcuts",
        argument_hint: None,
    },
    BuiltinCommand {
        name: "theme",
        description: "Show or switch the active theme",
        argument_hint: Some("[name|next|prev]"),
    },
    BuiltinCommand {
        name: "fork",
        description: "Fork from a previous user message",
        argument_hint: None,
    },
    BuiltinCommand {
        name: "clone",
        description: "Clone the current active branch",
        argument_hint: None,
    },
    BuiltinCommand {
        name: "tree",
        description: "Navigate the current session tree",
        argument_hint: None,
    },
    BuiltinCommand {
        name: "loop",
        description: "Run a prompt on a recurring interval",
        argument_hint: Some("[interval] <prompt>"),
    },
    BuiltinCommand {
        name: "loops",
        description: "List active recurring loops",
        argument_hint: None,
    },
    BuiltinCommand {
        name: "loop-cancel",
        description: "Cancel a recurring loop by ID",
        argument_hint: Some("<id>"),
    },
    BuiltinCommand {
        name: "todo",
        description: "Show or edit the task list",
        argument_hint: Some("[markdown]"),
    },
    BuiltinCommand {
        name: "trust",
        description: "Save a project trust decision",
        argument_hint: None,
    },
    BuiltinCommand {
        name: "login",
        description: "Configure provider authentication",
        argument_hint: Some("[provider]"),
    },
    BuiltinCommand {
        name: "logout",
        description: "Remove provider authentication",
        argument_hint: Some("[provider]"),
    },
    BuiltinCommand {
        name: "llama",
        description: "Manage the llama.cpp router",
        argument_hint: Some("[status|configure|refresh|load|unload]"),
    },
    BuiltinCommand {
        name: "new",
        description: "Start a new session",
        argument_hint: None,
    },
    BuiltinCommand {
        name: "compact",
        description: "Manually compact session context",
        argument_hint: Some("[instructions]"),
    },
    BuiltinCommand {
        name: "resume",
        description: "Resume a different session",
        argument_hint: Some("[path]"),
    },
    BuiltinCommand {
        name: "resume-codex",
        description: "Import and resume a Codex session",
        argument_hint: Some("<path|id>"),
    },
    BuiltinCommand {
        name: "ps",
        description: "List supervised processes",
        argument_hint: None,
    },
    BuiltinCommand {
        name: "process",
        description: "Control a supervised process",
        argument_hint: Some("<start|describe|logs|send|resize|signal|stop|wait> ..."),
    },
    BuiltinCommand {
        name: "reload",
        description: "Reload extensions and project resources",
        argument_hint: None,
    },
    BuiltinCommand {
        name: "quit",
        description: "Quit pi",
        argument_hint: None,
    },
];

#[must_use]
pub fn builtin(name: &str) -> Option<&'static BuiltinCommand> {
    BUILTIN_COMMANDS.iter().find(|command| command.name == name)
}

#[must_use]
pub fn usage(command: &BuiltinCommand) -> String {
    command.argument_hint.map_or_else(
        || format!("/{}", command.name),
        |hint| format!("/{} {hint}", command.name),
    )
}

#[must_use]
pub fn closest_builtin(name: &str) -> Option<&'static str> {
    BUILTIN_COMMANDS
        .iter()
        .map(|command| (edit_distance(command.name, name), command.name))
        .filter(|(distance, _)| *distance <= 3)
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, name)| name)
}

fn edit_distance(left: &str, right: &str) -> usize {
    let mut previous = (0..=right.chars().count()).collect::<Vec<_>>();
    let mut current = vec![0; previous.len()];
    for (left_index, left_character) in left.chars().enumerate() {
        current[0] = left_index + 1;
        for (right_index, right_character) in right.chars().enumerate() {
            current[right_index + 1] = (previous[right_index + 1] + 1)
                .min(current[right_index] + 1)
                .min(previous[right_index] + usize::from(left_character != right_character));
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.chars().count()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_registry_has_unique_names_and_usage() {
        let mut names = BUILTIN_COMMANDS
            .iter()
            .map(|command| command.name)
            .collect::<Vec<_>>();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), BUILTIN_COMMANDS.len());
        assert_eq!(
            usage(builtin("import").expect("import command")),
            "/import <path.jsonl>"
        );
        assert_eq!(
            usage(builtin("loop").expect("loop command")),
            "/loop [interval] <prompt>"
        );
        assert_eq!(
            usage(builtin("loop-cancel").expect("loop cancel command")),
            "/loop-cancel <id>"
        );
        assert_eq!(usage(builtin("todo").expect("todo command")), "/todo [markdown]");
        assert_eq!(usage(builtin("process").expect("process command")), "/process <start|describe|logs|send|resize|signal|stop|wait> ...");
    }

    #[tokio::test]
    async fn executable_catalog_excludes_dynamic_builtin_collisions() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let prompt_dir = cwd.path().join(".pi/prompts");
        std::fs::create_dir_all(&prompt_dir).expect("prompt directory");
        std::fs::write(
            prompt_dir.join("help.md"),
            "---\ndescription: Conflicting help\n---\nreplacement",
        )
        .expect("prompt template");
        let model = pi_ai::Model {
            id: "catalog-test".into(),
            name: "Catalog Test".into(),
            api: "catalog-test".into(),
            provider: "test".into(),
            ..pi_ai::Model::default()
        };
        let session = pi_coding::Session::new(pi_coding::SessionOptions {
            model,
            cwd: cwd.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: pi_agent::ThinkingLevel::Off,
            api_key: "test".into(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
        })
        .expect("session");
        let mut resource_options = pi_coding::ResourceManagerOptions::new(cwd.path());
        resource_options.project_trust_override = Some(true);
        let resources = pi_coding::ResourceManager::new(resource_options).expect("resources");
        session.attach_resources(resources).await.expect("attach resources");
        let application = pi_coding::Application::new(session).await;
        let (commands, diagnostics) = executable_catalog(&application);
        assert_eq!(commands.iter().filter(|command| command.name == "help").count(), 1);
        assert_eq!(
            commands.iter().find(|command| command.name == "help").map(|command| command.source),
            Some(CommandSource::Builtin)
        );
        assert!(diagnostics.iter().any(|diagnostic| diagnostic.contains("Prompt command '/help' conflicts")));
    }

    #[test]
    fn closest_builtin_is_contextual_but_not_noisy() {
        assert_eq!(closest_builtin("relaod"), Some("reload"));
        assert_eq!(closest_builtin("not-remotely-a-command"), None);
    }
}
