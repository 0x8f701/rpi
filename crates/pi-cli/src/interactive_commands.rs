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

/// Parse `/run` arguments into `(command_name, remainder)`.
pub fn parse_run_invocation(arguments: &str) -> anyhow::Result<(&str, &str)> {
    let trimmed = arguments.trim();
    if trimmed.is_empty() {
        anyhow::bail!("usage: /run <command> [args]");
    }
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or_default().trim();
    if name.is_empty() {
        anyhow::bail!("usage: /run <command> [args]");
    }
    if name.starts_with('/') {
        anyhow::bail!("extension command names must not include the leading slash");
    }
    let rest = parts.next().unwrap_or("").trim_start();
    Ok((name, rest))
}

/// Parse `/chain` / `/run-chain` into ordered `(command, args)` steps.
///
/// Steps are separated by `|`. Each step must name a real installed extension
/// command at dispatch time; this parser only validates non-empty shape.
pub fn parse_chain_invocation(arguments: &str) -> anyhow::Result<Vec<(String, String)>> {
    let trimmed = arguments.trim();
    if trimmed.is_empty() {
        anyhow::bail!("usage: /chain <command> [args] [| <command> [args] ...]");
    }
    let mut steps = Vec::new();
    for raw_step in trimmed.split('|') {
        let step = raw_step.trim();
        if step.is_empty() {
            anyhow::bail!("empty step in /chain; separate commands with |");
        }
        let (name, rest) = parse_run_invocation(step)
            .map_err(|_| anyhow::anyhow!("invalid /chain step {step:?}"))?;
        steps.push((name.to_owned(), rest.to_owned()));
    }
    if steps.is_empty() {
        anyhow::bail!("usage: /chain <command> [args] [| <command> [args] ...]");
    }
    Ok(steps)
}

/// Invoke one trusted installed extension command by registered name.
pub async fn invoke_extension_command(
    application: &pi_coding::Application,
    name: &str,
    arguments: String,
) -> anyhow::Result<serde_json::Value> {
    let runtime = application
        .extension_runtime()
        .ok_or_else(|| anyhow::anyhow!("extension runtime is not loaded"))?;
    if !runtime.commands().iter().any(|command| command.name == name) {
        anyhow::bail!(
            "unknown or untrusted extension command {name:?}; only commands registered by installed trusted extensions can run"
        );
    }
    runtime.invoke_command(name, arguments, None, None).await
}

/// Invoke a sequence of trusted installed extension commands.
pub async fn invoke_extension_chain(
    application: &pi_coding::Application,
    steps: &[(String, String)],
) -> anyhow::Result<Vec<(String, serde_json::Value)>> {
    let mut outputs = Vec::with_capacity(steps.len());
    for (name, arguments) in steps {
        let value = invoke_extension_command(application, name, arguments.clone()).await?;
        outputs.push((name.clone(), value));
    }
    Ok(outputs)
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
        description: "Inspect and edit schema-driven settings",
        argument_hint: Some("[list|search|set|reset|validate|apply|cancel] ..."),
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
        name: "branch",
        description: "Create a new branch from a previous message",
        argument_hint: None,
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
        name: "loop-update",
        description: "Update a recurring loop by ID",
        argument_hint: Some("<id> [interval] [prompt]"),
    },
    BuiltinCommand {
        name: "loop-delete",
        description: "Delete a recurring loop by ID without aborting its active turn",
        argument_hint: Some("<id>"),
    },
    BuiltinCommand {
        name: "loop-cancel",
        description: "Cancel a recurring loop by ID",
        argument_hint: Some("<id>"),
    },
    BuiltinCommand {
        name: "goal",
        description: "Create, inspect, pause, resume, complete, or drop the session goal",
        argument_hint: Some("[show|create [--tokens N] <objective>|pause|resume|complete|drop]"),
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
        description: "Resume a native or foreign session",
        argument_hint: Some("[path|id|prefix]"),
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
        name: "agents",
        description: "Manage agent definitions and model overrides",
        argument_hint: None,
    },
    BuiltinCommand {
        name: "reload",
        description: "Reload extensions and project resources",
        argument_hint: None,
    },
    BuiltinCommand {
        name: "run",
        description: "Run a trusted installed extension command",
        argument_hint: Some("<command> [args]"),
    },
    BuiltinCommand {
        name: "chain",
        description: "Run trusted installed extension commands in sequence",
        argument_hint: Some("<command> [args] [| <command> [args] ...]"),
    },
    BuiltinCommand {
        name: "run-chain",
        description: "Alias for /chain over trusted installed extension commands",
        argument_hint: Some("<command> [args] [| <command> [args] ...]"),
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

#[derive(Clone, Debug, PartialEq)]
pub enum InteractiveSettingsCommand {
    Inspect { scope: pi_coding::SettingsScope },
    Search { scope: pi_coding::SettingsScope, query: String },
    Set { scope: pi_coding::SettingsScope, key: String, value: serde_json::Value },
    Reset { scope: pi_coding::SettingsScope, key: String },
    Validate { scope: pi_coding::SettingsScope, key: Option<String>, value: Option<serde_json::Value> },
    Apply { scope: pi_coding::SettingsScope, key: String, value: serde_json::Value },
    Cancel { scope: pi_coding::SettingsScope },
}

pub fn parse_interactive_settings_command(
    name: &str,
    argument: Option<&str>,
) -> anyhow::Result<Option<InteractiveSettingsCommand>> {
    if name != "settings" {
        return Ok(None);
    }
    let arguments = pi_coding::parse_command_args(argument.unwrap_or_default());
    let mut parts = arguments.into_iter();
    let action = parts.next().unwrap_or_else(|| "list".to_owned());
    let mut rest = parts.collect::<Vec<_>>();
    let scope = if rest.first().is_some_and(|value| value == "--project") {
        rest.remove(0);
        pi_coding::SettingsScope::Project
    } else if rest.first().is_some_and(|value| value == "--global") {
        rest.remove(0);
        pi_coding::SettingsScope::Global
    } else {
        pi_coding::SettingsScope::Global
    };
    let command = match action.as_str() {
        "list" | "inspect" | "open" => {
            if !rest.is_empty() {
                anyhow::bail!("usage: /settings list [--global|--project]");
            }
            InteractiveSettingsCommand::Inspect { scope }
        }
        "search" => {
            if rest.is_empty() {
                anyhow::bail!("usage: /settings search [--global|--project] <query>");
            }
            InteractiveSettingsCommand::Search { scope, query: rest.join(" ") }
        }
        "set" => {
            let key = rest.first().cloned().ok_or_else(|| anyhow::anyhow!(
                "usage: /settings set [--global|--project] <key> <json-value>"
            ))?;
            if rest.len() != 2 {
                anyhow::bail!("usage: /settings set [--global|--project] <key> <json-value>");
            }
            let value = serde_json::from_str(&rest[1])
                .map_err(|error| anyhow::anyhow!("invalid JSON setting value: {error}"))?;
            InteractiveSettingsCommand::Set { scope, key, value }
        }
        "reset" => {
            if rest.len() != 1 {
                anyhow::bail!("usage: /settings reset [--global|--project] <key>");
            }
            InteractiveSettingsCommand::Reset { scope, key: rest.remove(0) }
        }
        "validate" => {
            let (key, value) = match rest.as_slice() {
                [] => (None, None),
                [key, value] => (
                    Some(key.clone()),
                    Some(serde_json::from_str(value).map_err(|error| {
                        anyhow::anyhow!("invalid JSON setting value: {error}")
                    })?),
                ),
                _ => anyhow::bail!("usage: /settings validate [--global|--project] [<key> <json-value>]"),
            };
            InteractiveSettingsCommand::Validate { scope, key, value }
        }
        "apply" => {
            if rest.len() != 2 {
                anyhow::bail!("usage: /settings apply [--global|--project] <key> <json-value>");
            }
            let key = rest.remove(0);
            let value = serde_json::from_str(&rest[0])
                .map_err(|error| anyhow::anyhow!("invalid JSON setting value: {error}"))?;
            InteractiveSettingsCommand::Apply { scope, key, value }
        }
        "cancel" => {
            if !rest.is_empty() {
                anyhow::bail!("usage: /settings cancel [--global|--project]");
            }
            InteractiveSettingsCommand::Cancel { scope }
        }
        _ => anyhow::bail!("unknown /settings action {action:?}"),
    };
    Ok(Some(command))
}

pub async fn execute_interactive_settings_command(
    application: &pi_coding::Application,
    command: InteractiveSettingsCommand,
) -> anyhow::Result<String> {
    let scope = match &command {
        InteractiveSettingsCommand::Inspect { scope }
        | InteractiveSettingsCommand::Search { scope, .. }
        | InteractiveSettingsCommand::Set { scope, .. }
        | InteractiveSettingsCommand::Reset { scope, .. }
        | InteractiveSettingsCommand::Validate { scope, .. }
        | InteractiveSettingsCommand::Apply { scope, .. }
        | InteractiveSettingsCommand::Cancel { scope } => *scope,
    };
    let mut panel = crate::settings_panel::SettingsPanel::from_application(application, scope)?;
    match command {
        InteractiveSettingsCommand::Inspect { .. } => {
            Ok(serde_json::to_string_pretty(&panel.snapshot()?)?)
        }
        InteractiveSettingsCommand::Search { query, .. } => {
            panel.set_search(query);
            Ok(serde_json::to_string_pretty(&panel.snapshot()?)?)
        }
        InteractiveSettingsCommand::Set { key, value, .. } => {
            panel.set_value(&key, value)?;
            panel.validate()?;
            let outcome = panel.apply(application).await?;
            Ok(serde_json::to_string_pretty(&outcome)?)
        }
        InteractiveSettingsCommand::Reset { key, .. } => {
            panel.reset(&key)?;
            panel.validate()?;
            let outcome = panel.apply(application).await?;
            Ok(serde_json::to_string_pretty(&outcome)?)
        }
        InteractiveSettingsCommand::Validate { key, value, .. } => {
            if let (Some(key), Some(value)) = (key, value) {
                panel.set_value(&key, value)?;
            }
            panel.validate()?;
            Ok("settings are valid".to_owned())
        }
        InteractiveSettingsCommand::Apply { key, value, .. } => {
            panel.set_value(&key, value)?;
            panel.validate()?;
            let outcome = panel.apply(application).await?;
            Ok(serde_json::to_string_pretty(&outcome)?)
        }
        InteractiveSettingsCommand::Cancel { .. } => {
            panel.cancel()?;
            Ok("settings draft cancelled".to_owned())
        }
    }
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
            usage(builtin("loop-update").expect("loop update command")),
            "/loop-update <id> [interval] [prompt]"
        );
        assert_eq!(
            usage(builtin("loop-delete").expect("loop delete command")),
            "/loop-delete <id>"
        );
        assert_eq!(
            usage(builtin("loop-cancel").expect("loop cancel command")),
            "/loop-cancel <id>"
        );
        assert_eq!(usage(builtin("todo").expect("todo command")), "/todo [markdown]");
        assert_eq!(usage(builtin("process").expect("process command")), "/process <start|describe|logs|send|resize|signal|stop|wait> ...");
        assert_eq!(usage(builtin("settings").expect("settings command")), "/settings [list|search|set|reset|validate|apply|cancel] ...");
        assert_eq!(
            usage(builtin("resume").expect("resume command")),
            "/resume [path|id|prefix]"
        );
        assert!(builtin("resume-codex").is_none());
    }

    #[test]
    fn parses_typed_settings_actions() {
        assert!(matches!(
            parse_interactive_settings_command("settings", Some("search --project retry")).unwrap(),
            Some(InteractiveSettingsCommand::Search { scope: pi_coding::SettingsScope::Project, query }) if query == "retry"
        ));
        assert!(matches!(
            parse_interactive_settings_command("settings", Some("set compaction.enabled false")).unwrap(),
            Some(InteractiveSettingsCommand::Set { scope: pi_coding::SettingsScope::Global, key, value }) if key == "compaction.enabled" && value == serde_json::json!(false)
        ));
        assert!(matches!(
            parse_interactive_settings_command("settings", Some("apply --global compaction.enabled false")).unwrap(),
            Some(InteractiveSettingsCommand::Apply { scope: pi_coding::SettingsScope::Global, key, value }) if key == "compaction.enabled" && value == serde_json::json!(false)
        ));
        assert!(parse_interactive_settings_command("settings", Some("apply compaction.enabled")).is_err());
        assert!(parse_interactive_settings_command("settings", Some("set theme not-json")).is_err());
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

    #[test]
    fn run_and_chain_parsers_require_named_extension_commands() {
        assert!(parse_run_invocation("").is_err());
        assert!(parse_run_invocation("/leading").is_err());
        let (name, args) = parse_run_invocation("hello  world").unwrap();
        assert_eq!(name, "hello");
        assert_eq!(args, "world");

        assert!(parse_chain_invocation("").is_err());
        assert!(parse_chain_invocation("a | | b").is_err());
        let steps = parse_chain_invocation("one | two arg | three").unwrap();
        assert_eq!(
            steps,
            vec![
                ("one".to_owned(), String::new()),
                ("two".to_owned(), "arg".to_owned()),
                ("three".to_owned(), String::new()),
            ]
        );
        assert!(builtin("run").is_some());
        assert!(builtin("chain").is_some());
        assert!(builtin("run-chain").is_some());
    }
}
