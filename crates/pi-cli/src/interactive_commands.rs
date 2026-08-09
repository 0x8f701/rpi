//! Shared interactive slash-command metadata and matching helpers.

use std::path::{Path, PathBuf};

use anyhow::Context as _;

/// One executable built-in slash command exposed by interactive modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuiltinCommand {
    pub name: &'static str,
    pub description: &'static str,
    pub argument_hint: Option<&'static str>,
    /// When true, bare `/{name}` is rejected with usage before dispatch.
    /// Must match real parsers — optional-arg commands stay false.
    pub requires_arguments: bool,
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

/// Build the full collision-free executable catalog for dispatch and source resolution.
///
/// Help and slash completion use [`visible_catalog`] / [`is_primary_command`] instead.
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
        for command in application
            .commands_catalog()
            .into_iter()
            .filter(|command| command.source == "skill")
        {
            let name = command.name;
            if builtin_names.contains(name.as_str()) || !dynamic_names.insert(name.clone()) {
                diagnostics.push(format!("Skill command '/{name}' conflicts with another command and was excluded from autocomplete"));
                continue;
            }
            commands.push(InteractiveCommand {
                name,
                description: command.description.unwrap_or_else(|| "Installed skill".to_owned()),
                source: CommandSource::Skill,
            });
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

/// Built-ins shown in slash completion and `/help`. Hidden commands stay manually executable.
pub const PRIMARY_COMMAND_NAMES: &[&str] = &[
    "settings",
    "model",
    "branch",
    "resume",
    "fork",
    "export",
    "dump",
    "handoff",
    "agents",
    "role",
    "persona",
    "compact",
    "rewind",
    "checkpoint",
    "ps",
    "loop",
    "goal",
    "workflow",
    "code-review",
    "btw",
    "queue",
    "live",
];

/// True when `name` is part of the visible primary slash surface.
#[must_use]
pub fn is_primary_command(name: &str) -> bool {
    PRIMARY_COMMAND_NAMES.contains(&name)
}

/// Visible primary built-ins for slash completion and help listings.
#[must_use]
pub fn visible_catalog() -> Vec<InteractiveCommand> {
    PRIMARY_COMMAND_NAMES
        .iter()
        .filter_map(|name| {
            builtin(name).map(|command| InteractiveCommand {
                name: command.name.to_owned(),
                description: command.description.to_owned(),
                source: CommandSource::Builtin,
            })
        })
        .collect()
}

/// Expand a prompt-template or skill command into the user prompt it executes.
pub fn expand_resource_command(
    application: &pi_coding::Application,
    name: &str,
    arguments: &str,
) -> anyhow::Result<Option<String>> {
    application.expand_resource_command(name, arguments)
}

/// Renders `/skill <name>`: the named loaded skill's frontmatter summary
/// (name, description, plus `globs`/`alwaysApply` when set).
///
/// Returns `None` when no resource snapshot is available or no loaded skill
/// matches `name`; callers surface the unknown-skill error.
#[must_use]
pub fn skill_frontmatter_summary(
    application: &pi_coding::Application,
    name: &str,
) -> Option<String> {
    let snapshot = application.resource_snapshot()?;
    let skill = snapshot.skills.iter().find(|skill| skill.name == name)?;
    let mut lines = vec![
        format!("name: {}", skill.name),
        format!("description: {}", skill.description),
    ];
    if !skill.globs.is_empty() {
        lines.push(format!("globs: {}", skill.globs.join(", ")));
    }
    if skill.always_apply {
        lines.push("alwaysApply: true".to_owned());
    }
    Some(lines.join("\n"))
}

/// Interactive `/role` actions.
///
/// Roles are the loaded agent definitions (project `<cwd>/.pi/agents/`, user
/// `<agent_dir>/agents/`, and the bundled `task`); there is no separate
/// `roles/` directory convention, so the command surface maps 1:1 onto
/// definitions. Personas are also roles and appear here; use `/persona` for
/// the persona-only surface and destructive archive operations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InteractiveRoleCommand {
    /// Bare `/role` — list every loaded role.
    List,
    /// `/role <name>` — show a role's details.
    Show { name: String },
    /// `/role <name> --select` (or `/role --select <name>`) — prefer this role
    /// for the next `task` spawn that does not name an agent explicitly.
    Select { name: String },
    /// `/role --clear` — stop preferring a role.
    Clear,
    /// `/role --current` — show the currently preferred role.
    Current,
}

/// Interactive `/persona` actions. Operates only on persistent persona
/// definitions (`definition.is_persona()`), never ordinary agents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InteractivePersonaCommand {
    /// Bare `/persona` — list loaded personas.
    List,
    /// `/persona <name>` — show persona details.
    Show { name: String },
    /// `/persona <name> --select` or `/persona --select <name>`.
    Select { name: String },
    /// `/persona --clear`.
    Clear,
    /// `/persona --current`.
    Current,
    /// `/persona run <name> <assignment>`.
    Run { name: String, assignment: String },
    /// `/persona reset <name> --yes` — clear memory and sessions, keep persona.md.
    Reset { name: String },
    /// `/persona remove <name> --yes` — delete persona.md, keep state.
    Remove { name: String },
    /// `/persona remove <name> --purge --yes` — delete the persona root.
    Purge { name: String },
    /// `/persona new <name>` — open an editor seeded with a template, then
    /// validate and atomically write `persona.md` under the user persona scope.
    New { name: String },
    /// `/persona edit <name>` — open the existing `persona.md` in an editor,
    /// then validate and atomically write it back (live reload on commit).
    Edit { name: String },
}

/// Parse `/role` arguments into an [`InteractiveRoleCommand`].
pub fn parse_interactive_role_command(argument: Option<&str>) -> anyhow::Result<InteractiveRoleCommand> {
    let Some(argument) = argument else {
        return Ok(InteractiveRoleCommand::List);
    };
    let argument = argument.trim();
    if argument.is_empty() {
        return Ok(InteractiveRoleCommand::List);
    }
    match argument {
        "--clear" => return Ok(InteractiveRoleCommand::Clear),
        "--current" => return Ok(InteractiveRoleCommand::Current),
        _ => {}
    }
    if let Some(name) = argument.strip_prefix("--select ") {
        let name = name.trim();
        if name.is_empty() {
            anyhow::bail!("usage: /role <name> --select | /role --select <name>");
        }
        return Ok(InteractiveRoleCommand::Select { name: name.to_owned() });
    }
    if let Some(name) = argument.strip_suffix(" --select") {
        let name = name.trim();
        if name.is_empty() {
            anyhow::bail!("usage: /role <name> --select | /role --select <name>");
        }
        return Ok(InteractiveRoleCommand::Select { name: name.to_owned() });
    }
    if argument.starts_with("--") {
        anyhow::bail!("unknown /role flag {argument:?}; use /role [<name>] [--select|--clear|--current]");
    }
    Ok(InteractiveRoleCommand::Show { name: argument.to_owned() })
}

/// Parse `/persona` arguments into an [`InteractivePersonaCommand`].
pub fn parse_interactive_persona_command(
    argument: Option<&str>,
) -> anyhow::Result<InteractivePersonaCommand> {
    let Some(argument) = argument else {
        return Ok(InteractivePersonaCommand::List);
    };
    let argument = argument.trim();
    if argument.is_empty() {
        return Ok(InteractivePersonaCommand::List);
    }
    match argument {
        "--clear" => return Ok(InteractivePersonaCommand::Clear),
        "--current" => return Ok(InteractivePersonaCommand::Current),
        _ => {}
    }
    if let Some(name) = argument.strip_prefix("--select ") {
        let name = name.trim();
        if name.is_empty() {
            anyhow::bail!("usage: /persona <name> --select | /persona --select <name>");
        }
        return Ok(InteractivePersonaCommand::Select {
            name: name.to_owned(),
        });
    }
    if let Some(name) = argument.strip_suffix(" --select") {
        let name = name.trim();
        if name.is_empty() {
            anyhow::bail!("usage: /persona <name> --select | /persona --select <name>");
        }
        return Ok(InteractivePersonaCommand::Select {
            name: name.to_owned(),
        });
    }
    if let Some(rest) = argument.strip_prefix("run ") {
        let rest = rest.trim();
        let mut parts = rest.splitn(2, char::is_whitespace);
        let name = parts.next().unwrap_or_default().trim();
        let assignment = parts.next().unwrap_or("").trim();
        if name.is_empty() || assignment.is_empty() {
            anyhow::bail!("usage: /persona run <name> <assignment>");
        }
        return Ok(InteractivePersonaCommand::Run {
            name: name.to_owned(),
            assignment: assignment.to_owned(),
        });
    }
    if argument == "new" || argument.starts_with("new ") {
        return parse_persona_named_subcommand(argument, "new")
            .map(|name| InteractivePersonaCommand::New { name });
    }
    if argument == "edit" || argument.starts_with("edit ") {
        return parse_persona_named_subcommand(argument, "edit")
            .map(|name| InteractivePersonaCommand::Edit { name });
    }
    if let Some(rest) = argument.strip_prefix("reset ") {
        return parse_persona_destructive(rest, PersonaDestructiveKind::Reset);
    }
    if let Some(rest) = argument.strip_prefix("remove ") {
        let rest = rest.trim();
        let purge = rest.split_whitespace().any(|token| token == "--purge");
        let kind = if purge {
            PersonaDestructiveKind::Purge
        } else {
            PersonaDestructiveKind::Remove
        };
        return parse_persona_destructive(rest, kind);
    }
    if argument.starts_with("--")
        || argument == "run"
        || argument == "reset"
        || argument == "remove"
        || argument == "new"
        || argument == "edit"
    {
        anyhow::bail!(
            "unknown /persona usage; use /persona [<name>] [--select|--clear|--current] | new <name> | edit <name> | run <name> <assignment> | reset <name> --yes | remove <name> [--purge] --yes"
        );
    }
    Ok(InteractivePersonaCommand::Show {
        name: argument.to_owned(),
    })
}

#[derive(Clone, Copy)]
enum PersonaDestructiveKind {
    Reset,
    Remove,
    Purge,
}

fn parse_persona_destructive(
    rest: &str,
    kind: PersonaDestructiveKind,
) -> anyhow::Result<InteractivePersonaCommand> {
    let tokens = rest.split_whitespace().collect::<Vec<_>>();
    let allowed_flags: &[&str] = match kind {
        PersonaDestructiveKind::Reset | PersonaDestructiveKind::Remove => &["--yes"],
        PersonaDestructiveKind::Purge => &["--yes", "--purge"],
    };
    for flag in tokens.iter().copied().filter(|token| token.starts_with("--")) {
        if !allowed_flags.contains(&flag) {
            anyhow::bail!("unknown flag {flag:?}; {}", persona_destructive_usage(kind));
        }
    }
    let names = tokens
        .iter()
        .copied()
        .filter(|token| !token.starts_with("--"))
        .collect::<Vec<_>>();
    if names.len() != 1 {
        anyhow::bail!(persona_destructive_usage(kind));
    }
    if tokens.iter().filter(|token| **token == "--yes").count() != 1 {
        anyhow::bail!(
            "{} requires exactly one --yes; refusing destructive operation without confirmation",
            persona_destructive_label(kind)
        );
    }
    if matches!(kind, PersonaDestructiveKind::Purge)
        && tokens.iter().filter(|token| **token == "--purge").count() != 1
    {
        anyhow::bail!(persona_destructive_usage(kind));
    }
    let name = names[0].to_owned();
    Ok(match kind {
        PersonaDestructiveKind::Reset => InteractivePersonaCommand::Reset { name },
        PersonaDestructiveKind::Remove => InteractivePersonaCommand::Remove { name },
        PersonaDestructiveKind::Purge => InteractivePersonaCommand::Purge { name },
    })
}

fn persona_destructive_usage(kind: PersonaDestructiveKind) -> &'static str {
    match kind {
        PersonaDestructiveKind::Reset => "usage: /persona reset <name> --yes",
        PersonaDestructiveKind::Remove => "usage: /persona remove <name> --yes",
        PersonaDestructiveKind::Purge => "usage: /persona remove <name> --purge --yes",
    }
}

fn persona_destructive_label(kind: PersonaDestructiveKind) -> &'static str {
    match kind {
        PersonaDestructiveKind::Reset => "/persona reset",
        PersonaDestructiveKind::Remove => "/persona remove",
        PersonaDestructiveKind::Purge => "/persona remove --purge",
    }
}

/// Parse `/persona new <name>` or `/persona edit <name>`: exactly one name
/// token that must be path-safe before it is ever joined into a persona root
/// path. Trailing tokens are rejected so flags can never become part of a name.
fn parse_persona_named_subcommand(argument: &str, keyword: &str) -> anyhow::Result<String> {
    let rest = argument
        .strip_prefix(keyword)
        .ok_or_else(|| anyhow::anyhow!("not a /persona {keyword} subcommand"))?
        .trim();
    let mut tokens = rest.split_whitespace();
    let name = tokens
        .next()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| anyhow::anyhow!("usage: /persona {keyword} <name>"))?;
    if tokens.next().is_some() {
        anyhow::bail!("unexpected argument; usage: /persona {keyword} <name>");
    }
    persona_name_path_safe(name)?;
    Ok(name.to_owned())
}

/// Path-safety guard for a persona name used to build a persona root path.
/// Mirrors the discovery charset (ASCII `[A-Za-z0-9_-]`, 1..=64) so a name that
/// passes here also passes `parse_persona_definition`; the authoritative name
/// validation runs on the committed content. Never join a name into a path
/// before this guard has accepted it.
fn persona_name_path_safe(name: &str) -> anyhow::Result<()> {
    if name.is_empty() || name.chars().count() > 64 {
        anyhow::bail!("persona name must be 1..=64 characters");
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        anyhow::bail!("persona name must contain only ASCII letters, digits, '_' or '-'");
    }
    Ok(())
}

/// Execute one `/role` action and render its text result.
pub fn execute_interactive_role_command(
    application: &pi_coding::Application,
    command: InteractiveRoleCommand,
) -> anyhow::Result<String> {
    let snapshot = application
        .resource_snapshot()
        .ok_or_else(|| anyhow::anyhow!("role catalog is unavailable"))?;
    let definitions = &snapshot.agents;
    let preferred = current_preferred_agent(application);
    match command {
        InteractiveRoleCommand::List => Ok(format_role_list(definitions, preferred.as_deref())),
        InteractiveRoleCommand::Show { name } => {
            let definition = definitions
                .iter()
                .find(|definition| definition.name == name)
                .ok_or_else(|| anyhow::anyhow!("unknown role {name:?}; /role lists available roles"))?;
            Ok(format_role_details(definition))
        }
        InteractiveRoleCommand::Select { name } => {
            let definition = definitions
                .iter()
                .find(|definition| definition.name == name)
                .ok_or_else(|| anyhow::anyhow!("unknown role {name:?}; /role lists available roles"))?;
            validate_selectable_definition(definition, &snapshot, "role")?;
            apply_preferred_agent(application, Some(&name))?;
            Ok(format!(
                "Role {name:?} selected for the next task spawn without an explicit agent"
            ))
        }
        InteractiveRoleCommand::Clear => {
            apply_preferred_agent(application, None)?;
            Ok("Role preference cleared".to_owned())
        }
        InteractiveRoleCommand::Current => Ok(match preferred {
            Some(name) => format!("Selected role: {name}"),
            None => "No role selected (ranked/default agent selection)".to_owned(),
        }),
    }
}

/// Execute one `/persona` action and render its text result.
pub async fn execute_interactive_persona_command(
    application: &pi_coding::Application,
    command: InteractivePersonaCommand,
) -> anyhow::Result<String> {
    let snapshot = application
        .resource_snapshot()
        .ok_or_else(|| anyhow::anyhow!("persona catalog is unavailable"))?;
    let personas = snapshot
        .agents
        .iter()
        .filter(|definition| definition.is_persona())
        .cloned()
        .collect::<Vec<_>>();
    let preferred = current_preferred_agent(application);
    match command {
        InteractivePersonaCommand::List => Ok(format_persona_list(&personas, preferred.as_deref())),
        InteractivePersonaCommand::Show { name } => {
            let definition = find_persona(&personas, &name)?;
            Ok(format_persona_details(definition, preferred.as_deref()))
        }
        InteractivePersonaCommand::Select { name } => {
            let definition = find_persona(&personas, &name)?;
            validate_selectable_definition(definition, &snapshot, "persona")?;
            apply_preferred_agent(application, Some(&name))?;
            Ok(format!(
                "Persona {name:?} selected for the next task spawn without an explicit agent"
            ))
        }
        InteractivePersonaCommand::Clear => {
            apply_preferred_agent(application, None)?;
            Ok("Persona preference cleared".to_owned())
        }
        InteractivePersonaCommand::Current => Ok(match preferred {
            Some(name) if personas.iter().any(|definition| definition.name == name) => {
                format!("Selected persona: {name}")
            }
            Some(name) => format!(
                "Selected role: {name} (not a persona; /persona lists personas only)"
            ),
            None => "No persona selected (ranked/default agent selection)".to_owned(),
        }),
        InteractivePersonaCommand::Run { name, assignment } => {
            let definition = find_persona(&personas, &name)?;
            validate_selectable_definition(definition, &snapshot, "persona")?;
            let runtime = application.orchestration_runtime().ok_or_else(|| {
                anyhow::anyhow!("orchestration is not available in this session")
            })?;
            let spawns = runtime.spawn_tasks(
                "Main",
                0,
                vec![pi_coding::TaskItem {
                    index: 0,
                    id: name.clone(),
                    agent: name.clone(),
                    assignment,
                    todo_task_id: None,
                    ..Default::default()
                }],
            )?;
            let spawn = spawns
                .into_iter()
                .next()
                .ok_or_else(|| anyhow::anyhow!("persona run produced no spawn"))?;
            Ok(format!(
                "Persona {name} started\njob: {}\nagent: {}",
                spawn.job_id, spawn.agent_id
            ))
        }
        InteractivePersonaCommand::Reset { name } => {
            let definition = find_persona(&personas, &name)?;
            let root = resolve_persona_root(application, definition)?;
            let _lifecycle_guard = application
                .orchestration_runtime()
                .map(|runtime| runtime.begin_persona_destructive_operation(&definition.name))
                .transpose()?;
            reset_persona_state(&root)?;
            Ok(format!(
                "Persona {name:?} reset: memory and sessions cleared; persona.md kept"
            ))
        }
        InteractivePersonaCommand::Remove { name } => {
            let definition = find_persona(&personas, &name)?;
            let root = resolve_persona_root(application, definition)?;
            let lifecycle_guard = application
                .orchestration_runtime()
                .map(|runtime| runtime.begin_persona_destructive_operation(&definition.name))
                .transpose()?;
            remove_persona_definition(&root)?;
            // Reload so the deleted definition disappears from subsequent
            // catalog reads. Failure retains the lifecycle block fail-closed.
            if let Err(error) = application.reload().await {
                if let Some(guard) = lifecycle_guard {
                    guard.retain();
                }
                anyhow::bail!(
                    "persona {name:?} was removed, but resource reload failed; the persona remains blocked in this session until reload or restart: {error:#}"
                );
            }
            Ok(format!(
                "Persona {name:?} removed: persona.md deleted; memory and sessions kept under the persona root"
            ))
        }
        InteractivePersonaCommand::Purge { name } => {
            let definition = find_persona(&personas, &name)?;
            let root = resolve_persona_root(application, definition)?;
            let lifecycle_guard = application
                .orchestration_runtime()
                .map(|runtime| runtime.begin_persona_destructive_operation(&definition.name))
                .transpose()?;
            purge_persona_root(&root)?;
            if let Err(error) = application.reload().await {
                if let Some(guard) = lifecycle_guard {
                    guard.retain();
                }
                anyhow::bail!(
                    "persona {name:?} was purged, but resource reload failed; the persona remains blocked in this session until reload or restart: {error:#}"
                );
            }
            Ok(format!(
                "Persona {name:?} purged: persona root deleted"
            ))
        }
        InteractivePersonaCommand::New { name } => {
            anyhow::bail!(
                "/persona new {name} opens an interactive editor; run it from the TUI or REPL, not the in-process command path"
            )
        }
        InteractivePersonaCommand::Edit { name } => {
            anyhow::bail!(
                "/persona edit {name} opens an interactive editor; run it from the TUI or REPL, not the in-process command path"
            )
        }
    }
}

fn find_persona<'a>(
    personas: &'a [pi_coding::AgentDefinition],
    name: &str,
) -> anyhow::Result<&'a pi_coding::AgentDefinition> {
    personas
        .iter()
        .find(|definition| definition.name == name)
        .ok_or_else(|| anyhow::anyhow!("unknown persona {name:?}; /persona lists available personas"))
}

fn current_preferred_agent(application: &pi_coding::Application) -> Option<String> {
    if let Some(name) = application
        .orchestration_runtime()
        .and_then(|runtime| runtime.preferred_agent())
    {
        return Some(name);
    }
    application
        .settings_manager()
        .ok()
        .and_then(|manager| {
            manager
                .settings()
                .orchestration
                .as_ref()
                .and_then(|orchestration| orchestration.preferred_agent.clone())
        })
}

fn validate_selectable_definition(
    definition: &pi_coding::AgentDefinition,
    snapshot: &pi_coding::ResourceSnapshot,
    kind: &str,
) -> anyhow::Result<()> {
    if !definition.trusted {
        anyhow::bail!("{kind} {:?} is not trusted and cannot be selected", definition.name);
    }
    if !snapshot.settings.is_agent_enabled(&definition.name) {
        anyhow::bail!(
            "{kind} {:?} is disabled in settings; enable it with /agents or settings.agents.{}.enabled",
            definition.name,
            definition.name
        );
    }
    Ok(())
}

fn apply_preferred_agent(
    application: &pi_coding::Application,
    name: Option<&str>,
) -> anyhow::Result<()> {
    if let Some(runtime) = application.orchestration_runtime() {
        runtime.set_preferred_agent(name);
    } else if name.is_some() {
        anyhow::bail!("orchestration is not available in this session");
    }
    let manager = application.settings_manager().with_context(|| {
        "settings manager is unavailable; cannot persist preferred agent selection"
    })?;
    manager
        .update_global(|settings| {
            let orchestration = settings
                .orchestration
                .get_or_insert_with(pi_coding::OrchestrationSettings::default);
            orchestration.preferred_agent = name.map(str::to_owned);
        })
        .context("persisting preferred agent selection")?;
    Ok(())
}

fn resolve_persona_root(
    application: &pi_coding::Application,
    definition: &pi_coding::AgentDefinition,
) -> anyhow::Result<PathBuf> {
    let root = definition.persona_root().ok_or_else(|| {
        anyhow::anyhow!(
            "persona {:?} has no persona root; only discovered personas can be modified",
            definition.name
        )
    })?;
    let resources = application
        .resource_snapshot()
        .ok_or_else(|| anyhow::anyhow!("persona catalog is unavailable"))?;
    if matches!(definition.source, pi_coding::AgentDefinitionSource::Project)
        && !resources.trust.allows_project_resources(true)
    {
        anyhow::bail!(
            "project persona {:?} is not available while project resources are untrusted",
            definition.name
        );
    }
    let manager = application
        .settings_manager()
        .context("settings manager is unavailable")?;
    let agent_dir = manager
        .paths()
        .global
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| anyhow::anyhow!("cannot resolve agent directory from settings paths"))?;
    let scopes = [
        agent_dir.join("personas"),
        resources.cwd.join(".pi").join("personas"),
    ];
    ensure_persona_root_contained(&root, &scopes)
}

/// Ensure `root` is a real directory strictly inside one of the allowed persona
/// scope roots, rejecting symlink escapes and path aliases. Returns the
/// canonical persona root on success.
fn ensure_persona_root_contained(root: &Path, scopes: &[PathBuf]) -> anyhow::Result<PathBuf> {
    let meta = std::fs::symlink_metadata(root)
        .with_context(|| format!("reading persona root {}", root.display()))?;
    if meta.file_type().is_symlink() {
        anyhow::bail!("persona root must not be a symlink");
    }
    if !meta.is_dir() {
        anyhow::bail!("persona root is not a directory");
    }
    // Reject symlink parents (scope path itself must not be a link).
    if let Some(parent) = root.parent() {
        let parent_meta = std::fs::symlink_metadata(parent)
            .with_context(|| format!("reading persona parent {}", parent.display()))?;
        if parent_meta.file_type().is_symlink() {
            anyhow::bail!("persona scope path must not contain symlinks");
        }
    }
    let canonical_root = std::fs::canonicalize(root)
        .with_context(|| format!("canonicalizing persona root {}", root.display()))?;
    let mut allowed = false;
    for scope in scopes {
        // Materialize the scope directory when absent so canonicalize can
        // establish the containment boundary without following links.
        if !scope.exists() {
            continue;
        }
        let scope_meta = std::fs::symlink_metadata(scope)
            .with_context(|| format!("reading persona scope {}", scope.display()))?;
        if scope_meta.file_type().is_symlink() {
            anyhow::bail!("persona scope path must not contain symlinks");
        }
        let Ok(canonical_scope) = std::fs::canonicalize(scope) else {
            continue;
        };
        if canonical_root.starts_with(&canonical_scope)
            && canonical_root != canonical_scope
            && canonical_root
                .strip_prefix(&canonical_scope)
                .is_ok_and(|rest| {
                    rest.components().count() == 1
                        && rest
                            .components()
                            .all(|component| matches!(component, std::path::Component::Normal(_)))
                })
        {
            allowed = true;
            break;
        }
    }
    if !allowed {
        anyhow::bail!("persona root escapes the allowed persona directories");
    }
    Ok(canonical_root)
}

fn reset_persona_state(root: &Path) -> anyhow::Result<()> {
    let memory = root.join("memory");
    let sessions = root.join("sessions");
    if memory.exists() {
        std::fs::remove_dir_all(&memory)
            .with_context(|| format!("removing persona memory {}", memory.display()))?;
    }
    if sessions.exists() {
        std::fs::remove_dir_all(&sessions)
            .with_context(|| format!("removing persona sessions {}", sessions.display()))?;
    }
    Ok(())
}

fn remove_persona_definition(root: &Path) -> anyhow::Result<()> {
    let persona_md = root.join("persona.md");
    if persona_md.exists() {
        let meta = std::fs::symlink_metadata(&persona_md)
            .with_context(|| format!("reading {}", persona_md.display()))?;
        if meta.file_type().is_symlink() {
            anyhow::bail!("refusing to delete symlinked persona.md");
        }
        std::fs::remove_file(&persona_md)
            .with_context(|| format!("removing {}", persona_md.display()))?;
    }
    Ok(())
}

fn purge_persona_root(root: &Path) -> anyhow::Result<()> {
    std::fs::remove_dir_all(root)
        .with_context(|| format!("purging persona root {}", root.display()))?;
    Ok(())
}

/// Which lifecycle write a `/persona new`/`/persona edit` is performing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PersonaEditKind {
    /// `/persona new <name>` — create a fresh `persona.md` in the user scope.
    New,
    /// `/persona edit <name>` — overwrite an existing discovered `persona.md`.
    Edit,
}

/// Commit persona definition content from the editor with full validation and
/// an atomic write, then live-reload the catalog. This is the deterministic,
/// editor-free core exercised by tests; the TUI/REPL editor flow feeds it the
/// edited content. Selection is preserved (the preferred agent is untouched).
pub async fn commit_persona_definition(
    application: &pi_coding::Application,
    name: &str,
    content: &str,
    kind: PersonaEditKind,
) -> anyhow::Result<String> {
    persona_name_path_safe(name)?;
    let snapshot = application
        .resource_snapshot()
 .ok_or_else(|| anyhow::anyhow!("persona catalog is unavailable"))?;
    let (root, source, trusted) = match kind {
        PersonaEditKind::New => {
            if let Some(existing) = snapshot.agents.iter().find(|d| d.name == name) {
                let existing_kind = if existing.is_persona() { "persona" } else { "agent" };
                anyhow::bail!(
                    "name {name:?} is already in use by a {existing_kind}; use /persona edit {name} or pick another name"
                );
            }
            validate_persona_content(content, name, pi_coding::AgentDefinitionSource::User, true)?;
            let (root, source, trusted) =
                resolve_new_persona_root(application, &snapshot, name)?;
            let persona_md = root.join("persona.md");
            if persona_md.exists() {
                anyhow::bail!("persona {name:?} already exists; use /persona edit {name}");
            }
            atomic_write_persona(&root, &persona_md, content, PersonaEditKind::New)?;
            (root, source, trusted)
        }
        PersonaEditKind::Edit => {
            let (root, definition) = resolve_existing_persona(application, name)?;
            validate_persona_content(content, name, definition.source, definition.trusted)?;
            let persona_md = root.join("persona.md");
            let meta = std::fs::symlink_metadata(&persona_md)
                .with_context(|| format!("reading {}", persona_md.display()))?;
            if meta.file_type().is_symlink() {
                anyhow::bail!("refusing to overwrite a symlinked persona.md");
            }
            if !meta.is_file() {
                anyhow::bail!("persona.md is not a regular file");
            }
            atomic_write_persona(&root, &persona_md, content, PersonaEditKind::Edit)?;
            (root, definition.source, definition.trusted)
        }
    };

    if let Err(error) = application.reload().await {
        return Ok(format!(
            "Persona {name:?} {} but resource reload failed: {error:#}",
            persona_kind_verb(kind)
        ));
    }
    Ok(format!(
        "Persona {name:?} {} ({})",
        persona_kind_verb(kind),
        persona_source_label(source)
    ))
}

/// Build the editor seed content for `/persona new` (a template) or
/// `/persona edit` (the current `persona.md` contents). Performs no write.
pub fn persona_editor_seed(
    application: &pi_coding::Application,
    name: &str,
    kind: PersonaEditKind,
) -> anyhow::Result<String> {
    persona_name_path_safe(name)?;
    match kind {
        PersonaEditKind::New => Ok(format!(
            "---\nname: {name}\ndescription: {name} persona\n---\n\
             Describe {name}'s behavior, personality, and contract here.\n\
             Optional frontmatter: personality, softBudget, maxTurns, timeoutSecs, tools.\n"
        )),
        PersonaEditKind::Edit => {
            let (root, _definition) = resolve_existing_persona(application, name)?;
            let persona_md = root.join("persona.md");
            let meta = std::fs::symlink_metadata(&persona_md)
                .with_context(|| format!("reading {}", persona_md.display()))?;
            if meta.file_type().is_symlink() {
                anyhow::bail!("refusing to read a symlinked persona.md");
            }
            if !meta.is_file() {
                anyhow::bail!("persona.md is not a regular file");
            }
            std::fs::read_to_string(&persona_md)
                .with_context(|| format!("reading {}", persona_md.display()))
        }
    }
}

/// Resolve the external editor command string (settings `externalEditor`,
/// then `VISUAL`, then `EDITOR`, then a platform default).
pub fn persona_editor_command(application: &pi_coding::Application) -> String {
    let configured = application
        .session()
        .resource_manager()
        .and_then(|resources| {
            resources
                .settings_manager()
                .settings()
                .extra
                .get("externalEditor")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        });
    configured
        .filter(|value| !value.trim().is_empty())
        .or_else(|| std::env::var("VISUAL").ok().filter(|v| !v.trim().is_empty()))
        .or_else(|| std::env::var("EDITOR").ok().filter(|v| !v.trim().is_empty()))
        .unwrap_or_else(|| {
            if cfg!(windows) {
                "notepad".to_owned()
            } else {
                "nano".to_owned()
            }
        })
}

/// Spawn the resolved editor command on `path` and await its exit. Used by the
/// REPL directly and by the TUI inside `terminal.suspend`.
pub async fn spawn_editor_on(command: &str, path: &Path) -> anyhow::Result<()> {
    let mut parts = command.split_whitespace();
    let program = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("external editor command is empty"))?;
    let status = tokio::process::Command::new(program)
        .args(parts)
        .arg(path)
        .status()
        .await
        .with_context(|| format!("launching editor {command:?}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow::anyhow!("external editor exited with {status}"))
    }
}

/// Resolve an existing discovered persona to its canonical root and definition.
fn resolve_existing_persona(
    application: &pi_coding::Application,
    name: &str,
) -> anyhow::Result<(PathBuf, pi_coding::AgentDefinition)> {
    let snapshot = application
        .resource_snapshot()
        .ok_or_else(|| anyhow::anyhow!("persona catalog is unavailable"))?;
    let definition = snapshot
        .agents
        .iter()
        .find(|definition| definition.is_persona() && definition.name == name)
        .ok_or_else(|| anyhow::anyhow!("unknown persona {name:?}; /persona lists available personas"))?
        .clone();
    if matches!(definition.source, pi_coding::AgentDefinitionSource::Project)
        && !snapshot.trust.allows_project_resources(true)
    {
        anyhow::bail!(
            "project persona {name:?} is not available while project resources are untrusted"
        );
    }
    let root = resolve_persona_root(application, &definition)?;
    Ok((root, definition))
}

/// Resolve the user-scope persona root for a new persona, creating the scope
/// and root directories. The user scope is pre-checked for symlinks before any
/// child path is created, and the final root is containment-checked.
fn resolve_new_persona_root(
    application: &pi_coding::Application,
    snapshot: &pi_coding::ResourceSnapshot,
    name: &str,
) -> anyhow::Result<(PathBuf, pi_coding::AgentDefinitionSource, bool)> {
    let manager = application
        .settings_manager()
        .context("settings manager is unavailable")?;
    let agent_dir = manager
        .paths()
        .global
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| anyhow::anyhow!("cannot resolve agent directory from settings paths"))?;
    let user_scope = agent_dir.join("personas");
    let project_scope = snapshot.cwd.join(".pi").join("personas");
    match std::fs::symlink_metadata(&user_scope) {
        Ok(meta) if meta.file_type().is_symlink() => {
            anyhow::bail!("persona scope path must not contain symlinks");
        }
        Ok(meta) if !meta.is_dir() => {
            anyhow::bail!("persona user scope is not a directory: {}", user_scope.display());
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(&user_scope)
                .with_context(|| format!("creating persona user scope {}", user_scope.display()))?;
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading persona user scope {}", user_scope.display()));
        }
    }
    let root = user_scope.join(name);
    std::fs::create_dir_all(&root)
        .with_context(|| format!("creating persona root {}", root.display()))?;
    let scopes = [user_scope, project_scope];
    let canonical = ensure_persona_root_contained(&root, &scopes)?;
    Ok((canonical, pi_coding::AgentDefinitionSource::User, true))
}

/// Validate persona definition content and enforce name/directory agreement
/// without touching the filesystem (the path is error context only).
fn validate_persona_content(
    content: &str,
    name: &str,
    source: pi_coding::AgentDefinitionSource,
    trusted: bool,
) -> anyhow::Result<()> {
    let probe = PathBuf::from(format!("personas/{name}/persona.md"));
    let parsed = pi_coding::parse_persona_definition(&probe, content, source, trusted)
        .context("persona definition is invalid")?;
    if parsed.name != name {
        anyhow::bail!(
            "persona name {:?} must match the target name {:?}; rename via /persona remove + /persona new",
            parsed.name,
            name
        );
    }
    Ok(())
}

fn atomic_write_persona(
    root: &Path,
    persona_md: &Path,
    content: &str,
    kind: PersonaEditKind,
) -> anyhow::Result<()> {
    let tmp = write_persona_temp(root, content)?;
    let install = match kind {
        // No-clobber install: hard_link fails if persona.md already exists
        // (any type, including a symlink), so a concurrent creator cannot be
        // silently overwritten. create_new already refused to follow a symlink
        // at the temp path.
        PersonaEditKind::New => std::fs::hard_link(&tmp, persona_md),
        // Target was validated as a regular file (not a symlink) by the caller
        // before this point, so atomic-replace via rename is safe.
        PersonaEditKind::Edit => std::fs::rename(&tmp, persona_md),
    };
    if let Err(error) = install {
        let _ = std::fs::remove_file(&tmp);
        return Err(error).with_context(|| format!("installing {}", persona_md.display()));
    }
    // New: persona.md is now a hard link to the temp; drop the temp name. For
    // Edit, rename already consumed the temp path.
    if matches!(kind, PersonaEditKind::New) {
        let _ = std::fs::remove_file(&tmp);
    }
    // Best-effort parent directory sync so the install is durable.
    if let Ok(dir) = std::fs::File::open(root) {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Create a unique, never-before-existing temp file inside the validated
/// persona root and write the content with fsync. Uses `create_new` so a
/// pre-placed symlink at a guessed name cannot be followed.
fn write_persona_temp(root: &Path, content: &str) -> anyhow::Result<PathBuf> {
    use std::io::Write;
    let tmp = root.join(format!(".persona.md.tmp.{}", uuid::Uuid::new_v4()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .with_context(|| format!("creating persona temp file {}", tmp.display()))?;
    if let Err(error) = file.write_all(content.as_bytes()) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error)
            .with_context(|| format!("writing persona temp file {}", tmp.display()));
    }
    if let Err(error) = file.flush() {
        let _ = std::fs::remove_file(&tmp);
        return Err(error).context("flushing persona temp file");
    }
    let _ = file.sync_all();
    Ok(tmp)
}

fn persona_kind_verb(kind: PersonaEditKind) -> &'static str {
    match kind {
        PersonaEditKind::New => "created",
        PersonaEditKind::Edit => "edited",
    }
}

fn persona_source_label(source: pi_coding::AgentDefinitionSource) -> &'static str {
    match source {
        pi_coding::AgentDefinitionSource::Project => "project scope",
        pi_coding::AgentDefinitionSource::User => "user scope",
        pi_coding::AgentDefinitionSource::Bundled => "bundled",
    }
}

/// Render the `/role` listing: one line per loaded role, marking the currently
/// preferred role with `*`.
fn format_role_list(definitions: &[pi_coding::AgentDefinition], preferred: Option<&str>) -> String {
    if definitions.is_empty() {
        return "(no roles loaded)".to_owned();
    }
    let mut lines = definitions
        .iter()
        .map(|definition| {
            let marker = if preferred == Some(definition.name.as_str()) {
                "* "
            } else {
                "  "
            };
            let kind = if definition.is_persona() {
                "persona"
            } else {
                "agent"
            };
            format!(
                "{marker}{} [{kind}/{:?}] — {}",
                definition.name, definition.source, definition.description
            )
        })
        .collect::<Vec<_>>();
    lines.push(String::new());
    lines.push(
        "Use /role <name> for details and /role <name> --select to choose the default role for task spawns."
            .to_owned(),
    );
    lines.join("\n")
}

/// Render one role's contract details.
fn format_role_details(definition: &pi_coding::AgentDefinition) -> String {
    format_definition_details("role", definition, None)
}

fn format_persona_list(
    definitions: &[pi_coding::AgentDefinition],
    preferred: Option<&str>,
) -> String {
    if definitions.is_empty() {
        return "(no personas loaded)".to_owned();
    }
    let mut lines = definitions
        .iter()
        .map(|definition| {
            let marker = if preferred == Some(definition.name.as_str()) {
                "* "
            } else {
                "  "
            };
            format!(
                "{marker}{} [{:?}] — {}",
                definition.name, definition.source, definition.description
            )
        })
        .collect::<Vec<_>>();
    lines.push(String::new());
    lines.push(
        "Use /persona <name> for details, /persona <name> --select to prefer it, and /persona run <name> <assignment> to spawn."
            .to_owned(),
    );
    lines.join("\n")
}

fn format_persona_details(
    definition: &pi_coding::AgentDefinition,
    preferred: Option<&str>,
) -> String {
    format_definition_details("persona", definition, preferred)
}

fn format_definition_details(
    kind: &str,
    definition: &pi_coding::AgentDefinition,
    preferred: Option<&str>,
) -> String {
    let mut lines = vec![
        format!("{kind}: {}", definition.name),
        format!("description: {}", definition.description),
    ];
    if preferred == Some(definition.name.as_str()) {
        lines.push("selected: true".to_owned());
    }
    if definition.is_persona() {
        lines.push(format!(
            "personality: {}",
            if definition.personality.as_ref().is_some_and(|value| !value.is_empty()) {
                "present"
            } else {
                "absent"
            }
        ));
        lines.push(format!(
            "scope: {}",
            match definition.source {
                pi_coding::AgentDefinitionSource::Project => "project",
                pi_coding::AgentDefinitionSource::User => "user",
                pi_coding::AgentDefinitionSource::Bundled => "bundled",
            }
        ));
    }
    if let Some(model) = &definition.model {
        lines.push(format!("model: {}", model.join(", ")));
    }
    if let Some(thinking_level) = definition.thinking_level {
        lines.push(format!("thinkingLevel: {thinking_level:?}"));
    }
    match &definition.tools {
        Some(tools) if tools.is_empty() => lines.push("tools: (none)".to_owned()),
        Some(tools) => lines.push(format!("tools: {}", tools.join(", "))),
        None => lines.push("tools: (all)".to_owned()),
    }
    if !definition.disallowed_tools.is_empty() {
        lines.push(format!(
            "disallowedTools: {}",
            definition.disallowed_tools.join(", ")
        ));
    }
    if !definition.autoload_skills.is_empty() {
        lines.push(format!(
            "autoloadSkills: {}",
            definition.autoload_skills.join(", ")
        ));
    }
    if let Some(max_turns) = definition.max_turns {
        lines.push(format!("maxTurns: {max_turns}"));
    }
    if let Some(max_tool_calls) = definition.max_tool_calls {
        lines.push(format!("maxToolCalls: {max_tool_calls}"));
    }
    if let Some(timeout_secs) = definition.timeout_secs {
        lines.push(format!("timeoutSecs: {timeout_secs}"));
    }
    if let Some(ceiling) = definition.capability_ceiling {
        lines.push(format!(
            "capabilityCeiling: read={} write={} exec={}",
            ceiling.read, ceiling.write, ceiling.exec,
        ));
    }
    if let Some(budget) = definition.soft_budget {
        lines.push(format!(
            "softBudget: maxRequests={} maxTokens={} yieldAfter={}",
            option_display(budget.max_requests),
            option_display(budget.max_tokens),
            option_display(budget.yield_after),
        ));
    }
    lines.push(format!("source: {:?}", definition.source));
    lines.push(format!("trusted: {}", definition.trusted));
    lines.push(String::new());
    lines.push(format!(
        "system prompt: {}",
        definition.system_prompt.lines().next().unwrap_or_default()
    ));
    lines.join("\n")
}

fn option_display<T: std::fmt::Display>(value: Option<T>) -> String {
    match value {
        Some(value) => value.to_string(),
        None => "unlimited".to_owned(),
    }
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

/// How `/handoff` should render.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandoffInvocation {
    /// Deterministic envelope only — no provider call.
    Envelope,
    /// Envelope plus one bounded summarizer prose paragraph.
    Prose,
}

/// Parse `/handoff` arguments. Accepts exactly `--prose` (or empty); any other
/// flag is rejected so typos surface as typed errors instead of silently
/// producing an envelope-only handoff.
pub fn parse_handoff_invocation(arguments: &str) -> anyhow::Result<HandoffInvocation> {
    match arguments.trim() {
        "" => Ok(HandoffInvocation::Envelope),
        "--prose" => Ok(HandoffInvocation::Prose),
        other => anyhow::bail!("usage: /handoff [--prose] (unknown argument {other:?})"),
    }
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


/// Commands implemented by both full-screen and line-oriented interactive modes.
pub const BUILTIN_COMMANDS: &[BuiltinCommand] = &[
    BuiltinCommand {
        name: "help",
        description: "Show available commands",
        argument_hint: None,
        requires_arguments: false,
    },
    BuiltinCommand {
        name: "settings",
        description: "Inspect and edit schema-driven settings",
        argument_hint: Some("[list|search|set|reset|validate|apply|cancel] ..."),
        requires_arguments: false,
    },
    BuiltinCommand {
        name: "model",
        description: "Select or switch model",
        argument_hint: Some("[provider/model]"),
        requires_arguments: false,
    },
    BuiltinCommand {
        name: "scoped-models",
        description: "Enable or disable models for cycling",
        argument_hint: None,
        requires_arguments: false,
    },
    BuiltinCommand {
        name: "models",
        description: "List available models",
        argument_hint: Some("[filter]"),
        requires_arguments: false,
    },
    BuiltinCommand {
        name: "export",
        description: "Export session to HTML or JSONL",
        argument_hint: Some("[path]"),
        requires_arguments: false,
    },
    BuiltinCommand {
        name: "dump",
        description: "Export the current session to a file (HTML default; --jsonl for JSONL)",
        argument_hint: Some("[--jsonl] [path]"),
        requires_arguments: false,
    },
    BuiltinCommand {
        name: "import",
        description: "Import and resume a JSONL session",
        argument_hint: Some("<path.jsonl>"),
        requires_arguments: true,
    },
    BuiltinCommand {
        name: "share",
        description: "Share session as a secret GitHub gist, or --encrypt it to an AES-256-GCM protected .jsonl.enc file",
        argument_hint: Some("[--encrypt [passphrase]]"),
        requires_arguments: false,
    },
    BuiltinCommand {
        name: "copy",
        description: "Copy the last assistant message",
        argument_hint: None,
        requires_arguments: false,
    },
    BuiltinCommand {
        name: "name",
        description: "Set or show the session name",
        argument_hint: Some("[name]"),
        requires_arguments: false,
    },
    BuiltinCommand {
        name: "session",
        description: "Show current session information",
        argument_hint: None,
        requires_arguments: false,
    },
    BuiltinCommand {
        name: "sessions",
        description: "List saved sessions",
        argument_hint: None,
        requires_arguments: false,
    },
    BuiltinCommand {
        name: "handoff",
        description: "Generate a handoff summary of this session (copied to clipboard when available)",
        argument_hint: Some("[--prose]"),
        requires_arguments: false,
    },
    BuiltinCommand {
        name: "changelog",
        description: "Show version history",
        argument_hint: None,
        requires_arguments: false,
    },
    BuiltinCommand {
        name: "hotkeys",
        description: "Show keyboard shortcuts",
        argument_hint: None,
        requires_arguments: false,
    },
    BuiltinCommand {
        name: "theme",
        description: "Show or switch the active theme",
        argument_hint: Some("[name|next|prev]"),
        requires_arguments: false,
    },
    BuiltinCommand {
        name: "branch",
        description: "Create a new branch from a previous message",
        argument_hint: None,
        requires_arguments: false,
    },
    BuiltinCommand {
        name: "fork",
        description: "Fork from a previous user message",
        argument_hint: None,
        requires_arguments: false,
    },
    BuiltinCommand {
        name: "clone",
        description: "Clone the current active branch",
        argument_hint: None,
        requires_arguments: false,
    },
    BuiltinCommand {
        name: "tree",
        description: "Navigate the current session tree",
        argument_hint: None,
        requires_arguments: false,
    },
    BuiltinCommand {
        name: "rewind",
        description: "Roll the session back to before an entry, archiving the dropped tail; bare /rewind lists entry indices and checkpoints to pick from",
        argument_hint: Some("[<entry-index|checkpoint-name>]"),
        requires_arguments: false,
    },
    BuiltinCommand {
        name: "checkpoint",
        description: "Mark the current position as a named rewind target",
        argument_hint: Some("<name>"),
        requires_arguments: true,
    },
    BuiltinCommand {
        name: "loop",
        description: "Run a prompt on a recurring interval",
        argument_hint: Some("[list|cancel <id>|delete <id>|update <id> [interval] [prompt]|create <interval> <prompt>|<interval> <prompt>]"),
        requires_arguments: true,
    },
    BuiltinCommand {
        name: "loops",
        description: "Alias for /loop list",
        argument_hint: None,
        requires_arguments: false,
    },
    BuiltinCommand {
        name: "loop-update",
        description: "Alias for /loop update <id> [interval] [prompt]",
        argument_hint: Some("<id> [interval] [prompt]"),
        requires_arguments: true,
    },
    BuiltinCommand {
        name: "loop-delete",
        description: "Alias for /loop delete <id>",
        argument_hint: Some("<id>"),
        requires_arguments: true,
    },
    BuiltinCommand {
        name: "loop-cancel",
        description: "Alias for /loop cancel <id>",
        argument_hint: Some("<id>"),
        requires_arguments: true,
    },
    BuiltinCommand {
        name: "goal",
        description: "Create, inspect, pin, pause, resume, complete, or drop the session goal",
        argument_hint: Some("[show|inspect|create [--tokens N] <objective>|pin <text>|pins|unpin <index>|pause|resume|complete|drop]"),
        requires_arguments: false,
    },
    BuiltinCommand {
        name: "workflow",
        description: "Create and manage concurrent isolated workflows",
        argument_hint: Some(
            "[list|show [id|name]|create <objective>|create <name> <objective>|pause|resume|cancel|integrate|remove]",
        ),
        requires_arguments: false,
    },
    BuiltinCommand {
        name: "code-review",
        description: "Browse a working-tree or two-revision Git diff in a fullscreen panel",
        argument_hint: Some("[<from> <to>]"),
        requires_arguments: false,
    },
    BuiltinCommand {
        name: "btw",
        description: "Open the multi-tab side chat (parallel sessions)",
        argument_hint: Some("[prompt | new <name> | list | close [<name>]]"),
        requires_arguments: false,
    },
    BuiltinCommand {
        name: "overlay",
        description: "Open an extension-rendered overlay panel by registered id",
        argument_hint: Some("<id>"),
        requires_arguments: true,
    },

    BuiltinCommand {
        name: "todo",
        description: "Show or edit the task list",
        argument_hint: Some("[list|markdown]"),
        requires_arguments: false,
    },
    BuiltinCommand {
        name: "trust",
        description: "Save a project trust decision",
        argument_hint: None,
        requires_arguments: false,
    },
    BuiltinCommand {
        name: "login",
        description: "Configure provider authentication",
        argument_hint: Some("[provider]"),
        requires_arguments: false,
    },
    BuiltinCommand {
        name: "logout",
        description: "Remove provider authentication",
        argument_hint: Some("[provider]"),
        requires_arguments: false,
    },
    BuiltinCommand {
        name: "llama",
        description: "Manage the llama.cpp router",
        argument_hint: Some("[status|configure|refresh|load|unload]"),
        requires_arguments: false,
    },
    BuiltinCommand {
        name: "new",
        description: "Start a new session",
        argument_hint: None,
        requires_arguments: false,
    },
    BuiltinCommand {
        name: "fresh",
        description: "Start a fresh session (alias for /new)",
        argument_hint: None,
        requires_arguments: false,
    },
    BuiltinCommand {
        name: "queue",
        description: "Show pending steering/follow-up prompts (cancel clears them)",
        argument_hint: Some("[cancel]"),
        requires_arguments: false,
    },
    BuiltinCommand {
        name: "compact",
        description: "Manually compact session context",
        argument_hint: Some("[--snap] [instructions]"),
        requires_arguments: false,
    },
    BuiltinCommand {
        name: "snapcompact",
        description: "Archive dense history deterministically without an LLM call (alias of /compact --snap)",
        argument_hint: None,
        requires_arguments: false,
    },
    BuiltinCommand {
        name: "resume",
        description: "Resume a native or foreign session",
        argument_hint: Some("[path|id|prefix]"),
        requires_arguments: false,
    },
    BuiltinCommand {
        name: "ps",
        description: "List supervised processes",
        argument_hint: None,
        requires_arguments: false,
    },
    BuiltinCommand {
        name: "process",
        description: "Control a supervised process",
        argument_hint: Some("<start|describe|logs|send|resize|signal|stop|wait> ..."),
        requires_arguments: true,
    },
    BuiltinCommand {
        name: "agents",
        description: "Manage agent definitions and model overrides",
        argument_hint: None,
        requires_arguments: false,
    },
    BuiltinCommand {
        name: "role",
        description: "List roles, show role details, or select the default role for task spawns",
        argument_hint: Some("[<name>] [--select|--clear|--current]"),
        requires_arguments: false,
    },
    BuiltinCommand {
        name: "persona",
        description: "List, show, select, create, edit, run, reset, or remove persistent personas",
        argument_hint: Some("[<name>] [--select|--clear|--current] | new <name> | edit <name> | run <name> <assignment> | reset <name> --yes | remove <name> [--purge] --yes"),
        requires_arguments: false,
    },
    BuiltinCommand {
        name: "reload",
        description: "Reload extensions and project resources",
        argument_hint: None,
        requires_arguments: false,
    },
    BuiltinCommand {
        name: "run",
        description: "Run a trusted installed extension command",
        argument_hint: Some("<command> [args]"),
        requires_arguments: true,
    },
    BuiltinCommand {
        name: "chain",
        description: "Run trusted installed extension commands in sequence",
        argument_hint: Some("<command> [args] [| <command> [args] ...]"),
        requires_arguments: true,
    },
    BuiltinCommand {
        name: "run-chain",
        description: "Alias for /chain over trusted installed extension commands",
        argument_hint: Some("<command> [args]"),
        requires_arguments: true,
    },
    BuiltinCommand {
        name: "skill",
        description: "Show a loaded skill's frontmatter summary",
        argument_hint: Some("<name>"),
        requires_arguments: true,
    },
    BuiltinCommand {
        name: "quit",
        description: "Quit rpi",
        argument_hint: None,
        requires_arguments: false,
    },

    BuiltinCommand {
        name: "live",
        description: "Toggle hold-to-talk voice mode (press Ctrl+Space to talk; transcript lands in the composer for review before Enter)",
        argument_hint: None,
        requires_arguments: false,
    },
    BuiltinCommand {
        name: "collab",
        description: "Start a live collaboration room and print the full-control and view-only join links, or show/stop the running room",
        argument_hint: Some("[status|stop]"),
        requires_arguments: false,
    },
    BuiltinCommand {
        name: "join",
        description: "Join a live collaboration room as a guest from a full-control (#c=...) or view-only (#v=...) link",
        argument_hint: Some("<link>"),
        requires_arguments: true,
    },
    BuiltinCommand {
        name: "leave",
        description: "Leave the joined collaboration room",
        argument_hint: None,
        requires_arguments: false,
    },
];

#[must_use]
pub fn builtin(name: &str) -> Option<&'static BuiltinCommand> {
    BUILTIN_COMMANDS.iter().find(|command| command.name == name)
}

/// Parses `/compact` arguments. `--snap` (alone or as a leading flag) selects
/// the deterministic snapcompact archive that never calls the provider; any
/// trailing text after `--snap` is ignored (the deterministic path has no use
/// for summarization instructions). Anything else is treated as custom
/// summarization instructions for the LLM path (legacy behavior).
#[must_use]
pub fn parse_compact_arguments(arg: &str) -> (bool, Option<&str>) {
    let trimmed = arg.trim();
    if let Some(rest) = trimmed.strip_prefix("--snap") {
        if rest.is_empty() || rest.starts_with(char::is_whitespace) {
            return (true, None);
        }
    }
    (false, (!trimmed.is_empty()).then_some(trimmed))
}

#[must_use]
pub fn usage(command: &BuiltinCommand) -> String {
    command.argument_hint.map_or_else(
        || format!("/{}", command.name),
        |hint| format!("/{} {hint}", command.name),
    )
}

/// True when bare `/{name}` must be rejected with usage before dispatch.
#[must_use]
pub fn requires_arguments(command: &BuiltinCommand) -> bool {
    command.requires_arguments
}

/// Parsed `/collab` invocation: bare `/collab` starts a room (or reprints the
/// active room's links); `status` and `stop` inspect or tear down the room.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CollabInvocation {
    Start,
    Status,
    Stop,
}

/// Parse `/collab` arguments: `[status|stop]`. Anything else is rejected so
/// typos surface as typed errors instead of silently starting a new room.
pub fn parse_collab_invocation(arguments: &str) -> anyhow::Result<CollabInvocation> {
    match arguments.trim() {
        "" => Ok(CollabInvocation::Start),
        "status" => Ok(CollabInvocation::Status),
        "stop" => Ok(CollabInvocation::Stop),
        other => anyhow::bail!("usage: /collab [status|stop] (unknown argument {other:?})"),
    }
}

/// Parse and validate a `/join <link>` argument through the core link parser.
/// Errors are secret-free and path-free by construction (the core parser
/// never echoes key material or filesystem paths).
pub fn parse_join_invocation(arguments: &str) -> anyhow::Result<pi_coding::collab::CollabLink> {
    let link = arguments.trim();
    if link.is_empty() {
        anyhow::bail!("usage: /join <link>");
    }
    pi_coding::collab::parse_link(link)
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

// ---------------------------------------------------------------------------
// /fresh, /dump, and /share --encrypt — shared parse + execution so the
// full-screen TUI and the line-oriented repl stay thin and testable.
// ---------------------------------------------------------------------------

/// Format selected by `/dump` (`--jsonl` flag or a `.jsonl` output path).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DumpFormat {
    Html,
    Jsonl,
}

/// Parsed `/dump` invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DumpRequest {
    pub format: DumpFormat,
    pub output: Option<PathBuf>,
}

/// Parse `/dump` arguments: `[--jsonl] [path]`.
///
/// HTML is the default; `--jsonl` (or a `.jsonl` output path, matching
/// `/export`) selects JSONL. A path may contain spaces — only a leading
/// `--jsonl` token is treated as a flag.
#[must_use]
pub fn parse_dump_invocation(arguments: &str) -> DumpRequest {
    let trimmed = arguments.trim();
    let (jsonl, rest) = if trimmed == "--jsonl"
        || trimmed.starts_with("--jsonl ")
        || trimmed.starts_with("--jsonl\t")
    {
        (true, trimmed["--jsonl".len()..].trim_start())
    } else {
        (false, trimmed)
    };
    let output = (!rest.is_empty()).then(|| PathBuf::from(rest));
    let jsonl_by_extension = output.as_ref().is_some_and(|path| {
        path.extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
    });
    let format = if jsonl || jsonl_by_extension {
        DumpFormat::Jsonl
    } else {
        DumpFormat::Html
    };
    DumpRequest { format, output }
}

/// Export the current session through the existing export path.
///
/// HTML uses the session-file-derived default (like `/export`). JSONL with no
/// explicit path writes `<session-stem>.jsonl` in the session working
/// directory, because the session-dir default would collide with the source
/// `.jsonl` file.
pub async fn execute_dump(
    application: &pi_coding::Application,
    request: DumpRequest,
) -> anyhow::Result<PathBuf> {
    match request.format {
        DumpFormat::Html => application.export_html(request.output.as_deref()),
        DumpFormat::Jsonl => {
            let output = match request.output {
                Some(path) => path,
                None => {
                    let state = application.state().await;
                    let stem = state
                        .session_file
                        .as_deref()
                        .map(Path::new)
                        .and_then(Path::file_stem)
                        .map(|stem| stem.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "session".to_owned());
                    application.session().cwd().join(format!("{stem}.jsonl"))
                }
            };
            application.export_jsonl(Some(&output))
        }
    }
}

/// Start a fresh session, archiving the current one on disk. Returns `true`
/// when the new session was created and `false` when the user cancelled.
pub async fn execute_fresh(application: &pi_coding::Application) -> anyhow::Result<bool> {
    let outcome = application.new_session().await?;
    Ok(!outcome.cancelled)
}

/// A parsed `/rewind` invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RewindInvocation {
    /// Bare `/rewind`: list entry indices + first-line previews to pick from.
    List,
    /// `/rewind <N>`: keep records `0..N`, dropping record `N` and beyond.
    Index(usize),
    /// `/rewind <name>`: roll back to the position a checkpoint marks.
    Checkpoint(String),
}

/// Parse `/rewind` arguments. A bare or whitespace-only argument lists
/// rewind targets; a plain number is an entry index; anything else is a
/// checkpoint name.
#[must_use]
pub fn parse_rewind_invocation(arguments: Option<&str>) -> RewindInvocation {
    let Some(arguments) = arguments else {
        return RewindInvocation::List;
    };
    let trimmed = arguments.trim();
    if trimmed.is_empty() {
        return RewindInvocation::List;
    }
    if let Ok(index) = trimmed.parse::<usize>() {
        return RewindInvocation::Index(index);
    }
    RewindInvocation::Checkpoint(trimmed.to_owned())
}

/// Execute a parsed `/rewind` invocation and render its result text.
pub async fn execute_rewind(
    application: &pi_coding::Application,
    invocation: RewindInvocation,
) -> anyhow::Result<String> {
    match invocation {
        RewindInvocation::List => format_rewind_list(application),
        RewindInvocation::Index(index) => {
            let outcome = application
                .rewind(pi_coding::RewindTarget::Index(index))
                .await?;
            Ok(format_rewind_outcome(&outcome))
        }
        RewindInvocation::Checkpoint(name) => {
            let outcome = application
                .rewind(pi_coding::RewindTarget::Checkpoint(name.clone()))
                .await?;
            Ok(format_rewind_outcome(&outcome))
        }
    }
}

/// Execute `/checkpoint <name>`: mark the current position as a named rewind
/// target.
pub fn execute_checkpoint(
    application: &pi_coding::Application,
    name: &str,
) -> anyhow::Result<String> {
    let id = application.set_checkpoint(name)?;
    Ok(format!("checkpoint {name:?} marked at entry {id}"))
}

/// Render the bare `/rewind` picker: the last 20 records with their index and
/// a first-line preview, checkpoints annotated with the entry they target.
fn format_rewind_list(application: &pi_coding::Application) -> anyhow::Result<String> {
    const PICKER_LIMIT: usize = 20;
    let previews = application.rewind_preview(PICKER_LIMIT)?;
    if previews.is_empty() {
        return Ok("(no session records yet — nothing to rewind)".to_owned());
    }
    let mut lines = vec![
        "/rewind <entry-index|checkpoint-name> rolls back to before that record (the dropped tail is archived to a .rewind-*.jsonl sidecar):".to_owned(),
    ];
    for preview in previews {
        if let Some(name) = preview.checkpoint_name.as_deref() {
            let target = preview
                .checkpoint_target_id
                .as_deref()
                .unwrap_or("?");
            lines.push(format!(
                "{:>3}  [checkpoint {name} -> {target}]",
                preview.index
            ));
        } else {
            let preview_text = preview
                .preview
                .as_deref()
                .map(|text| format!("  {text}"))
                .unwrap_or_default();
            lines.push(format!(
                "{:>3}  [{:<18}]{}",
                preview.index, preview.entry_type, preview_text
            ));
        }
    }
    Ok(lines.join("\n"))
}

/// Render a successful rewind's outcome: what was kept, what was dropped, and
/// where the archived tail lives.
fn format_rewind_outcome(outcome: &pi_coding::RewindOutcome) -> String {
    let last_kept = outcome.retained_entries.saturating_sub(1);
    let mut message = format!(
        "rewound to entry {last_kept} (kept {}, dropped {} record(s)); archived tail to {}",
        outcome.retained_entries,
        outcome.dropped_entries,
        outcome.archive_path.display()
    );
    if let Some(checkpoint) = outcome.checkpoint.as_deref() {
        message = format!("rewound to checkpoint {checkpoint:?}: {message}");
    }
    message
}

/// Parsed `/share` invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShareRequest {
    pub encrypt: bool,
    pub passphrase: Option<String>,
}

/// Parse `/share` arguments: `[--encrypt [passphrase]]`.
///
/// `--encrypt` without a passphrase leaves [`ShareRequest::passphrase`]
/// `None` so the caller can prompt (hidden input) before encrypting.
pub fn parse_share_invocation(arguments: &str) -> anyhow::Result<ShareRequest> {
    let trimmed = arguments.trim();
    if trimmed == "--encrypt"
        || trimmed.starts_with("--encrypt ")
        || trimmed.starts_with("--encrypt\t")
    {
        let passphrase = trimmed["--encrypt".len()..].trim();
        Ok(ShareRequest {
            encrypt: true,
            passphrase: (!passphrase.is_empty()).then(|| passphrase.to_owned()),
        })
    } else if trimmed.is_empty() {
        Ok(ShareRequest {
            encrypt: false,
            passphrase: None,
        })
    } else {
        anyhow::bail!("usage: /share [--encrypt [passphrase]]");
    }
}

/// Prompt for a passphrase without echoing it to the terminal.
///
/// Works both inside the full-screen TUI (call from
/// [`crate::tui::TerminalGuard::suspend`]) and in line-oriented mode. The
/// passphrase is returned once and never logged.
pub fn prompt_passphrase(message: &str) -> anyhow::Result<String> {
    use std::io::Write as _;
    eprint!("{message}: ");
    std::io::stderr()
        .flush()
        .context("flushing passphrase prompt")?;
    let passphrase = crate::auth_commands::read_secret_line()?;
    eprintln!();
    Ok(passphrase)
}

/// Encrypt the current session to `<name>.jsonl.enc` and return the display
/// message (share note + ciphertext path + optional gist URL).
pub async fn execute_encrypted_share(
    application: &pi_coding::Application,
    passphrase: &str,
) -> anyhow::Result<String> {
    let result = application.share_session_encrypted(passphrase, None).await?;
    let mut message = format!(
        "Wrote encrypted session share to {}\n{}",
        result.ciphertext_path.display(),
        result.note
    );
    if let Some(url) = result.gist_url {
        message.push_str(&format!("\nGist: {url}"));
    }
    Ok(message)
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
            "/loop [list|cancel <id>|delete <id>|update <id> [interval] [prompt]|create <interval> <prompt>|<interval> <prompt>]"
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
        assert_eq!(usage(builtin("todo").expect("todo command")), "/todo [list|markdown]");
        assert_eq!(
            usage(builtin("code-review").expect("code review command")),
            "/code-review [<from> <to>]"
        );
        assert_eq!(usage(builtin("process").expect("process command")), "/process <start|describe|logs|send|resize|signal|stop|wait> ...");
        assert_eq!(usage(builtin("settings").expect("settings command")), "/settings [list|search|set|reset|validate|apply|cancel] ...");
        assert_eq!(
            usage(builtin("resume").expect("resume command")),
            "/resume [path|id|prefix]"
        );
        assert_eq!(usage(builtin("fresh").expect("fresh command")), "/fresh");
        assert_eq!(
            usage(builtin("queue").expect("queue command")),
            "/queue [cancel]"
        );
        assert_eq!(
            usage(builtin("compact").expect("compact command")),
            "/compact [--snap] [instructions]"
        );
        assert_eq!(
            usage(builtin("snapcompact").expect("snapcompact command")),
            "/snapcompact"
        );
        assert_eq!(
            usage(builtin("dump").expect("dump command")),
            "/dump [--jsonl] [path]"
        );
        assert_eq!(
            usage(builtin("handoff").expect("handoff command")),
            "/handoff [--prose]"
        );
        assert_eq!(
            usage(builtin("share").expect("share command")),
            "/share [--encrypt [passphrase]]"
        );
        assert!(!requires_arguments(builtin("fresh").expect("fresh command")));
        assert!(!requires_arguments(builtin("dump").expect("dump command")));
        assert!(!requires_arguments(builtin("share").expect("share command")));
        assert!(builtin("resume-codex").is_none());
    }

    #[test]
    fn parses_compact_arguments() {
        assert_eq!(parse_compact_arguments(""), (false, None));
        assert_eq!(parse_compact_arguments("   "), (false, None));
        assert_eq!(parse_compact_arguments("keep my recent work"), (false, Some("keep my recent work")));
        assert_eq!(parse_compact_arguments("--snap"), (true, None));
        assert_eq!(parse_compact_arguments("  --snap  "), (true, None));
        // Trailing text after --snap is ignored by the deterministic path.
        assert_eq!(parse_compact_arguments("--snap preserve everything"), (true, None));
        // A non-flag prefix stays instructions.
        assert_eq!(parse_compact_arguments("--snapshot the plan"), (false, Some("--snapshot the plan")));
    }

    #[test]
    fn required_arguments_match_real_parsers_not_hint_heuristics() {
        let required = BUILTIN_COMMANDS
            .iter()
            .filter(|command| requires_arguments(command))
            .map(|command| command.name)
            .collect::<Vec<_>>();
        assert_eq!(
            required,
            vec![
                "import",
                "checkpoint",
                "loop",
                "loop-update",
                "loop-delete",
                "loop-cancel",
                "overlay",
                "process",
                "run",
                "chain",
                "run-chain",
                "skill",
                "join",
            ]
        );
        assert!(
            !requires_arguments(builtin("goal").expect("goal command")),
            "bare /goal must reach Show semantics"
        );
        assert!(
            !requires_arguments(builtin("rewind").expect("rewind command")),
            "bare /rewind must list rewind targets"
        );
        assert!(requires_arguments(builtin("import").expect("import command")));
        assert!(
            requires_arguments(builtin("checkpoint").expect("checkpoint command")),
            "bare /checkpoint must be rejected with usage"
        );
        assert!(
            requires_arguments(builtin("skill").expect("skill command")),
            "bare /skill must be rejected with usage"
        );
    }

    #[test]
    fn rewind_invocation_parses_list_index_and_checkpoint() {
        assert_eq!(parse_rewind_invocation(None), RewindInvocation::List);
        assert_eq!(parse_rewind_invocation(Some("")), RewindInvocation::List);
        assert_eq!(parse_rewind_invocation(Some("   ")), RewindInvocation::List);
        assert_eq!(
            parse_rewind_invocation(Some("7")),
            RewindInvocation::Index(7)
        );
        assert_eq!(
            parse_rewind_invocation(Some(" 12 ")),
            RewindInvocation::Index(12)
        );
        assert_eq!(
            parse_rewind_invocation(Some("mid")),
            RewindInvocation::Checkpoint("mid".to_owned())
        );
        assert_eq!(
            parse_rewind_invocation(Some("keep this")),
            RewindInvocation::Checkpoint("keep this".to_owned())
        );
        // A checkpoint named like a number is unreachable by design: numbers
        // always resolve to entry indices (checkpoint names reject digits).
        assert_eq!(
            parse_rewind_invocation(Some("0")),
            RewindInvocation::Index(0)
        );
    }

    #[test]
    fn handoff_parses_prose_flag_and_rejects_unknown_arguments() {
        assert_eq!(
            parse_handoff_invocation("").expect("bare handoff"),
            HandoffInvocation::Envelope
        );
        assert_eq!(
            parse_handoff_invocation("   ").expect("whitespace handoff"),
            HandoffInvocation::Envelope
        );
        assert_eq!(
            parse_handoff_invocation("--prose").expect("prose flag"),
            HandoffInvocation::Prose
        );
        assert_eq!(
            parse_handoff_invocation(" --prose ").expect("trimmed prose flag"),
            HandoffInvocation::Prose
        );
        for bogus in ["--bogus", "-prose", "--PROSE", "--prose extra", "prose"] {
            let error = parse_handoff_invocation(bogus)
                .expect_err(&format!("{bogus:?} must be rejected"));
            assert!(
                error.to_string().contains("/handoff [--prose]"),
                "{bogus:?} must surface usage: {error}"
            );
        }
    }

    #[test]
    fn primary_command_surface_is_explicit_and_stable() {
        assert_eq!(
            PRIMARY_COMMAND_NAMES,
            &[
                "settings",
                "model",
                "branch",
                "resume",
                "fork",
                "export",
                "dump",
                "handoff",
                "agents",
                "role",
                "persona",
                "compact",
                "rewind",
                "checkpoint",
                "ps",
                "loop",
                "goal",
                "workflow",
                "code-review",
                "btw",
                "queue",
                "live",
            ]
        );
        assert_eq!(PRIMARY_COMMAND_NAMES.len(), 22);
        let visible = visible_catalog()
            .into_iter()
            .map(|command| command.name)
            .collect::<Vec<_>>();
        assert_eq!(visible, PRIMARY_COMMAND_NAMES);
        assert!(is_primary_command("goal"));
        assert!(is_primary_command("workflow"));
        assert!(is_primary_command("rewind"));
        assert!(is_primary_command("checkpoint"));
        assert!(is_primary_command("live"));
        assert!(is_primary_command("persona"));
        assert!(!is_primary_command("help"));
        assert!(!is_primary_command("import"));
        assert!(!is_primary_command("workfloww"));
        assert!(!is_primary_command("skill:release"));
        assert!(builtin("import").is_some(), "hidden commands remain executable");
        assert_eq!(
            usage(builtin("workflow").expect("workflow command")),
            "/workflow [list|show [id|name]|create <objective>|create <name> <objective>|pause|resume|cancel|integrate|remove]"
        );
        assert!(
            !requires_arguments(builtin("workflow").expect("workflow command")),
            "bare /workflow must open the workflows page"
        );
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

    #[tokio::test]
    async fn skill_frontmatter_summary_shows_loaded_skill_fields_and_rejects_unknown() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let skill_dir = cwd.path().join(".pi/skills/research");
        std::fs::create_dir_all(&skill_dir).expect("skill directory");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: research\ndescription: Deep-dive codebase researcher.\nglobs: [\"**/*.rs\", \"Cargo.toml\"]\nalwaysApply: true\n---\n# Research\n\nBody.",
        )
        .expect("skill file");
        let model = pi_ai::Model {
            id: "skill-test".into(),
            name: "Skill Test".into(),
            api: "skill-test".into(),
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

        let summary =
            skill_frontmatter_summary(&application, "research").expect("loaded skill summary");
        assert!(summary.contains("name: research"), "{summary}");
        assert!(
            summary.contains("description: Deep-dive codebase researcher."),
            "{summary}"
        );
        assert!(summary.contains("globs: **/*.rs, Cargo.toml"), "{summary}");
        assert!(summary.contains("alwaysApply: true"), "{summary}");

        assert!(
            skill_frontmatter_summary(&application, "missing").is_none(),
            "unknown skill must not resolve"
        );
        assert!(
            skill_frontmatter_summary(&application, "research ").is_none(),
            "skill names must match exactly"
        );
    }

    #[tokio::test]
    async fn skill_frontmatter_summary_omits_unset_globs_and_always_apply() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let skill_dir = cwd.path().join(".pi/skills/bare");
        std::fs::create_dir_all(&skill_dir).expect("skill directory");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: bare\ndescription: No patterns.\n---\nBody.",
        )
        .expect("skill file");
        let model = pi_ai::Model {
            id: "skill-bare".into(),
            name: "Skill Bare".into(),
            api: "skill-bare".into(),
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

        let summary = skill_frontmatter_summary(&application, "bare").expect("bare skill summary");
        assert!(summary.contains("name: bare"), "{summary}");
        assert!(summary.contains("description: No patterns."), "{summary}");
        assert!(!summary.contains("globs:"), "globs must be omitted when unset: {summary}");
        assert!(!summary.contains("alwaysApply:"), "alwaysApply must be omitted when unset: {summary}");
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

    #[test]
    fn dump_invocation_parses_flags_paths_and_extensions() {
        assert_eq!(
            parse_dump_invocation(""),
            DumpRequest { format: DumpFormat::Html, output: None }
        );
        assert_eq!(
            parse_dump_invocation("--jsonl"),
            DumpRequest { format: DumpFormat::Jsonl, output: None }
        );
        assert_eq!(
            parse_dump_invocation("--jsonl out.jsonl"),
            DumpRequest {
                format: DumpFormat::Jsonl,
                output: Some(PathBuf::from("out.jsonl"))
            }
        );
        assert_eq!(
            parse_dump_invocation("export.html"),
            DumpRequest {
                format: DumpFormat::Html,
                output: Some(PathBuf::from("export.html"))
            }
        );
        // A `.jsonl` output selects JSONL even without the flag (matches /export).
        assert_eq!(
            parse_dump_invocation("out.JSONL"),
            DumpRequest {
                format: DumpFormat::Jsonl,
                output: Some(PathBuf::from("out.JSONL"))
            }
        );
        // Paths with spaces survive as a single output token.
        assert_eq!(
            parse_dump_invocation("--jsonl my dump file.jsonl"),
            DumpRequest {
                format: DumpFormat::Jsonl,
                output: Some(PathBuf::from("my dump file.jsonl"))
            }
        );
        // "--jsonlx" is a path, not the flag.
        assert_eq!(
            parse_dump_invocation("--jsonlx"),
            DumpRequest {
                format: DumpFormat::Html,
                output: Some(PathBuf::from("--jsonlx"))
            }
        );
    }

    #[test]
    fn share_invocation_parses_encrypt_and_passphrase() {
        assert_eq!(
            parse_share_invocation("").unwrap(),
            ShareRequest { encrypt: false, passphrase: None }
        );
        assert_eq!(
            parse_share_invocation("--encrypt").unwrap(),
            ShareRequest { encrypt: true, passphrase: None }
        );
        assert_eq!(
            parse_share_invocation("--encrypt hunter2").unwrap(),
            ShareRequest { encrypt: true, passphrase: Some("hunter2".to_owned()) }
        );
        assert_eq!(
            parse_share_invocation("--encrypt  a b c ").unwrap(),
            ShareRequest {
                encrypt: true,
                passphrase: Some("a b c".to_owned())
            }
        );
        assert!(parse_share_invocation("--public").is_err());
        assert!(parse_share_invocation("--encryptx").is_err());
    }

    #[test]
    fn collab_invocation_parses_start_status_stop() {
        assert_eq!(parse_collab_invocation("").unwrap(), CollabInvocation::Start);
        assert_eq!(parse_collab_invocation("  ").unwrap(), CollabInvocation::Start);
        assert_eq!(parse_collab_invocation("status").unwrap(), CollabInvocation::Status);
        assert_eq!(parse_collab_invocation(" stop ").unwrap(), CollabInvocation::Stop);
        // Typos are rejected, never silently treated as a start.
        assert!(parse_collab_invocation("statuss").is_err());
        assert!(parse_collab_invocation("stpo").is_err());
        assert!(parse_collab_invocation("--list").is_err());
    }

    #[test]
    fn join_invocation_parses_links_and_rejects_junk() {
        // A syntactically valid room link parses through the core parser.
        let (room, keys) = (
            pi_coding::collab::new_room_id().expect("room id"),
            pi_coding::collab::generate_room_keys().expect("keys"),
        );
        let control = pi_coding::collab::CollabSecret {
            role: pi_coding::collab::CollabRole::Control,
            key: keys.control,
        };
        let link = pi_coding::collab::format_link("http://127.0.0.1:4321", &room, &control);
        let parsed = parse_join_invocation(&link).expect("valid control link");
        assert_eq!(parsed.room_id, room);
        assert_eq!(parsed.secret.role, pi_coding::collab::CollabRole::Control);
        assert_eq!(parsed.secret.key, keys.control);

        // Empty argument and non-links are rejected with usage-style errors.
        assert!(parse_join_invocation("").is_err());
        assert!(parse_join_invocation("   ").is_err());
        assert!(parse_join_invocation("not a link").is_err());
        // A link without its key fragment is rejected.
        assert!(parse_join_invocation(&link[..link.find('#').expect("fragment")]).is_err());
        // Errors never echo the secret: a malformed fragment reports the
        // problem, not the key bytes.
        let broken = format!("{link}#c=not-base64!!");
        let error = parse_join_invocation(&broken).expect_err("broken fragment");
        assert!(!error.to_string().contains("not-base64"), "errors must be secret-free");
    }

    async fn recorded_application(cwd: &std::path::Path, sessions: &std::path::Path, id: &str) -> pi_coding::Application {
        let recorder = pi_coding::start_session_in(
            cwd,
            None,
            Some("off"),
            Some(sessions),
            Some(id),
            None,
        )
        .expect("start recorder");
        recorder
            .record_message(&pi_ai::Message::user_text("fresh dump fixture", 0))
            .expect("record message");
        recorder.persist_now().expect("persist session");
        let session = pi_coding::Session::new(pi_coding::SessionOptions {
            model: pi_ai::Model::default(),
            cwd: cwd.to_path_buf(),
            system_prompt: String::new(),
            thinking_level: pi_agent::ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
        })
        .expect("session");
        session.set_session_dir(sessions.to_path_buf());
        session.record(recorder).expect("attach recorder");
        pi_coding::Application::new(session).await
    }

    #[tokio::test]
    async fn execute_fresh_archives_current_session_and_starts_new_recorder() {
        let cwd = tempfile::tempdir().expect("cwd");
        let sessions = tempfile::tempdir().expect("sessions");
        let application = recorded_application(cwd.path(), sessions.path(), "fresh-source").await;
        let before = application.state().await;
        assert_eq!(before.session_id.as_deref(), Some("fresh-source"));
        assert!(before.session_file.is_some());

        assert!(
            execute_fresh(&application).await.expect("fresh must complete"),
            "fresh must not be cancelled in this flow"
        );
        let after = application.state().await;
        assert_ne!(after.session_id, before.session_id, "/fresh must switch to a new recorder");
        let old_file = sessions.path().join("fresh-source.jsonl");
        assert!(
            old_file.exists(),
            "the archived session file must remain on disk"
        );
        assert_ne!(
            after.session_file.as_deref(),
            before.session_file.as_deref(),
            "/fresh must record into a new session file"
        );
        application.cleanup().await;
    }

    #[tokio::test]
    async fn execute_dump_writes_nonempty_html_and_jsonl() {
        let cwd = tempfile::tempdir().expect("cwd");
        let sessions = tempfile::tempdir().expect("sessions");
        let application = recorded_application(cwd.path(), sessions.path(), "dump-source").await;

        // HTML default derives from the session file path.
        let html_path = execute_dump(
            &application,
            DumpRequest { format: DumpFormat::Html, output: None },
        )
        .await
        .expect("html dump");
        let html = std::fs::read_to_string(&html_path).expect("read html");
        assert!(!html.is_empty());
        assert!(html.contains("<html"));
        assert!(html.contains("fresh dump fixture"));

        // JSONL default lands in the session cwd under the session stem.
        let jsonl_path = execute_dump(
            &application,
            DumpRequest { format: DumpFormat::Jsonl, output: None },
        )
        .await
        .expect("jsonl dump");
        assert_eq!(jsonl_path, cwd.path().join("dump-source.jsonl"));
        let jsonl = std::fs::read_to_string(&jsonl_path).expect("read jsonl");
        assert!(!jsonl.is_empty());
        assert!(jsonl.contains("fresh dump fixture"));
        application.cleanup().await;
    }

    #[test]
    fn role_parser_handles_list_show_select_clear_and_current() {
        assert_eq!(
            parse_interactive_role_command(None).expect("bare /role"),
            InteractiveRoleCommand::List
        );
        assert_eq!(
            parse_interactive_role_command(Some("")).expect("empty argument"),
            InteractiveRoleCommand::List
        );
        assert_eq!(
            parse_interactive_role_command(Some("reviewer")).expect("show"),
            InteractiveRoleCommand::Show {
                name: "reviewer".to_owned()
            }
        );
        assert_eq!(
            parse_interactive_role_command(Some("reviewer --select")).expect("select suffix"),
            InteractiveRoleCommand::Select {
                name: "reviewer".to_owned()
            }
        );
        assert_eq!(
            parse_interactive_role_command(Some("--select reviewer")).expect("select prefix"),
            InteractiveRoleCommand::Select {
                name: "reviewer".to_owned()
            }
        );
        assert_eq!(
            parse_interactive_role_command(Some("--clear")).expect("clear"),
            InteractiveRoleCommand::Clear
        );
        assert_eq!(
            parse_interactive_role_command(Some("--current")).expect("current"),
            InteractiveRoleCommand::Current
        );
        assert!(parse_interactive_role_command(Some("--bogus")).is_err());
        assert!(parse_interactive_role_command(Some("--select")).is_err());
    }

    /// Session + resource fixture with one user role definition and an
    /// orchestration runtime built from the same catalog.
    async fn role_application(
        cwd: &std::path::Path,
        agent_dir: &std::path::Path,
    ) -> pi_coding::Application {
        let agents_dir = agent_dir.join("agents");
        std::fs::create_dir_all(&agents_dir).expect("agents directory");
        std::fs::write(
            agents_dir.join("reviewer.md"),
            "---\n\
             name: reviewer\n\
             description: Deep code reviewer\n\
             disallowedTools: [bash]\n\
             capabilityCeiling: [read, write]\n\
             ---\n\
             Review every change carefully.",
        )
        .expect("role definition");
        let model = pi_ai::Model {
            id: "role-test".into(),
            name: "Role Test".into(),
            api: "role-test".into(),
            provider: "test".into(),
            ..pi_ai::Model::default()
        };
        let session = pi_coding::Session::new(pi_coding::SessionOptions {
            model,
            cwd: cwd.to_path_buf(),
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
        let mut resource_options = pi_coding::ResourceManagerOptions::new(cwd);
        resource_options.agent_dir = agent_dir.to_path_buf();
        let resources = pi_coding::ResourceManager::new(resource_options).expect("resources");
        session
            .attach_resources(resources)
            .await
            .expect("attach resources");
        let snapshot = session
            .resource_manager()
            .expect("resource manager")
            .snapshot();
        let artifacts = tempfile::tempdir().expect("artifacts");
        let mut config = pi_coding::OrchestrationConfig::new(
            pi_coding::AgentCatalog::from_agents(snapshot.agents.clone()),
            artifacts.path(),
        );
        config.idle_ttl = None;
        let factory: pi_coding::ChildSessionFactory = std::sync::Arc::new(|_| {
            Box::pin(async { Err(anyhow::anyhow!("role test factory is not exercised")) })
        });
        let runtime =
            pi_coding::OrchestrationRuntime::new(config, factory).expect("orchestration runtime");
        pi_coding::Application::new_with_orchestration(session, runtime).await
    }

    #[tokio::test]
    async fn role_command_lists_loaded_definitions() {
        let cwd = tempfile::tempdir().expect("cwd");
        let agent_dir = tempfile::tempdir().expect("agent dir");
        let application = role_application(cwd.path(), agent_dir.path()).await;

        let output = execute_interactive_role_command(
            &application,
            parse_interactive_role_command(None).expect("bare /role"),
        )
        .expect("role list");
        assert!(output.contains("reviewer"), "{output}");
        assert!(output.contains("Deep code reviewer"), "{output}");
        assert!(output.contains("task"), "bundled task role must be listed: {output}");

        application.cleanup().await;
    }

    #[tokio::test]
    async fn role_command_shows_details_and_rejects_unknown() {
        let cwd = tempfile::tempdir().expect("cwd");
        let agent_dir = tempfile::tempdir().expect("agent dir");
        let application = role_application(cwd.path(), agent_dir.path()).await;

        let output = execute_interactive_role_command(
            &application,
            parse_interactive_role_command(Some("reviewer")).expect("show reviewer"),
        )
        .expect("role details");
        assert!(output.contains("role: reviewer"), "{output}");
        assert!(output.contains("Deep code reviewer"), "{output}");
        assert!(output.contains("disallowedTools: bash"), "{output}");
        assert!(
            output.contains("capabilityCeiling: read=true write=true exec=false"),
            "{output}"
        );
        assert!(output.contains("system prompt: Review every change carefully."), "{output}");

        let error = execute_interactive_role_command(
            &application,
            parse_interactive_role_command(Some("missing")).expect("show missing"),
        )
        .expect_err("unknown role must be rejected");
        assert!(error.to_string().contains("unknown role"), "{error:#}");
        assert!(error.to_string().contains("missing"), "{error:#}");

        application.cleanup().await;
    }

    #[tokio::test]
    async fn role_command_select_clear_and_current_drive_preferred_agent() {
        let cwd = tempfile::tempdir().expect("cwd");
        let agent_dir = tempfile::tempdir().expect("agent dir");
        let application = role_application(cwd.path(), agent_dir.path()).await;
        let runtime = application
            .orchestration_runtime()
            .expect("orchestration runtime");

        let current = execute_interactive_role_command(
            &application,
            parse_interactive_role_command(Some("--current")).expect("current"),
        )
        .expect("current role");
        assert!(current.contains("No role selected"), "{current}");

        let selected = execute_interactive_role_command(
            &application,
            parse_interactive_role_command(Some("reviewer --select")).expect("select"),
        )
        .expect("select role");
        assert!(selected.contains("reviewer"), "{selected}");
        assert_eq!(runtime.preferred_agent().as_deref(), Some("reviewer"));
        assert_eq!(
            application
                .settings_manager()
                .expect("settings")
                .settings()
                .orchestration
                .as_ref()
                .and_then(|orchestration| orchestration.preferred_agent.as_deref()),
            Some("reviewer"),
            "select must persist preferredAgent in global settings"
        );

        let current = execute_interactive_role_command(
            &application,
            parse_interactive_role_command(Some("--current")).expect("current"),
        )
        .expect("current after select");
        assert!(current.contains("reviewer"), "{current}");

        execute_interactive_role_command(
            &application,
            parse_interactive_role_command(Some("--clear")).expect("clear"),
        )
        .expect("clear role");
        assert!(runtime.preferred_agent().is_none());
        assert!(
            application
                .settings_manager()
                .expect("settings")
                .settings()
                .orchestration
                .as_ref()
                .and_then(|orchestration| orchestration.preferred_agent.as_ref())
                .is_none(),
            "clear must drop preferredAgent from global settings"
        );

        application.cleanup().await;
    }

    #[tokio::test]
    async fn role_command_rejects_unknown_or_disabled_selection() {
        let cwd = tempfile::tempdir().expect("cwd");
        let agent_dir = tempfile::tempdir().expect("agent dir");
        let application = role_application(cwd.path(), agent_dir.path()).await;

        let error = execute_interactive_role_command(
            &application,
            parse_interactive_role_command(Some("missing --select")).expect("select missing"),
        )
        .expect_err("unknown role selection must be rejected");
        assert!(error.to_string().contains("unknown role"), "{error:#}");

        application.cleanup().await;
    }

    #[test]
    fn persona_parser_handles_list_show_select_current_clear_run_and_destructive() {
        assert_eq!(
            parse_interactive_persona_command(None).expect("bare /persona"),
            InteractivePersonaCommand::List
        );
        assert_eq!(
            parse_interactive_persona_command(Some("mentor")).expect("show"),
            InteractivePersonaCommand::Show {
                name: "mentor".to_owned()
            }
        );
        assert_eq!(
            parse_interactive_persona_command(Some("mentor --select")).expect("select suffix"),
            InteractivePersonaCommand::Select {
                name: "mentor".to_owned()
            }
        );
        assert_eq!(
            parse_interactive_persona_command(Some("--select mentor")).expect("select prefix"),
            InteractivePersonaCommand::Select {
                name: "mentor".to_owned()
            }
        );
        assert_eq!(
            parse_interactive_persona_command(Some("--current")).expect("current"),
            InteractivePersonaCommand::Current
        );
        assert_eq!(
            parse_interactive_persona_command(Some("--clear")).expect("clear"),
            InteractivePersonaCommand::Clear
        );
        assert_eq!(
            parse_interactive_persona_command(Some("run mentor ship the docs")).expect("run"),
            InteractivePersonaCommand::Run {
                name: "mentor".to_owned(),
                assignment: "ship the docs".to_owned()
            }
        );
        assert_eq!(
            parse_interactive_persona_command(Some("reset mentor --yes")).expect("reset"),
            InteractivePersonaCommand::Reset {
                name: "mentor".to_owned()
            }
        );
        assert_eq!(
            parse_interactive_persona_command(Some("remove mentor --yes")).expect("remove"),
            InteractivePersonaCommand::Remove {
                name: "mentor".to_owned()
            }
        );
        assert_eq!(
            parse_interactive_persona_command(Some("remove mentor --purge --yes")).expect("purge"),
            InteractivePersonaCommand::Purge {
                name: "mentor".to_owned()
            }
        );
        // Destructive ops without --yes are refused.
        let reset_err = parse_interactive_persona_command(Some("reset mentor"))
            .expect_err("reset without --yes");
        assert!(
            reset_err.to_string().contains("--yes"),
            "{reset_err:#}"
        );
        let remove_err = parse_interactive_persona_command(Some("remove mentor"))
            .expect_err("remove without --yes");
        assert!(
            remove_err.to_string().contains("--yes"),
            "{remove_err:#}"
        );
        let purge_err = parse_interactive_persona_command(Some("remove mentor --purge"))
            .expect_err("purge without --yes");
        assert!(purge_err.to_string().contains("--yes"), "{purge_err:#}");
        assert!(parse_interactive_persona_command(Some("run mentor")).is_err());
        assert!(parse_interactive_persona_command(Some("--bogus")).is_err());
        for invalid in [
            "reset mentor extra --yes",
            "reset mentor --purge --yes",
            "reset mentor --yes --yes",
            "remove mentor extra --yes",
            "remove mentor --purge --yes --yes",
            "remove mentor --purge --purge --yes",
        ] {
            assert!(
                parse_interactive_persona_command(Some(invalid)).is_err(),
                "junk destructive grammar must fail: {invalid}"
            );
        }
    }

    fn write_user_persona(agent_dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
        let root = agent_dir.join("personas").join(name);
        std::fs::create_dir_all(root.join("memory")).expect("persona memory");
        std::fs::create_dir_all(root.join("sessions")).expect("persona sessions");
        std::fs::write(
            root.join("persona.md"),
            format!(
                "---\n\
                 name: {name}\n\
                 description: {name} persona\n\
                 personality: steady mentor\n\
                 softBudget:\n\
                   maxRequests: 4\n\
                 ---\n\
                 {body}"
            ),
        )
        .expect("persona.md");
        std::fs::write(root.join("memory").join("entries.jsonl"), "{\"k\":1}\n")
            .expect("memory entry");
        std::fs::write(root.join("sessions").join("run-1.jsonl"), "{}\n").expect("session archive");
        root
    }

    async fn persona_application(
        cwd: &std::path::Path,
        agent_dir: &std::path::Path,
    ) -> pi_coding::Application {
        // Ordinary agent still present so /role lists both and /persona filters.
        let agents_dir = agent_dir.join("agents");
        std::fs::create_dir_all(&agents_dir).expect("agents directory");
        std::fs::write(
            agents_dir.join("reviewer.md"),
            "---\nname: reviewer\ndescription: Ordinary agent\n---\nReview code.",
        )
        .expect("agent definition");
        write_user_persona(agent_dir, "mentor", "Guide the user patiently.");

        let model = pi_ai::Model {
            id: "persona-test".into(),
            name: "Persona Test".into(),
            api: "persona-test".into(),
            provider: "test".into(),
            ..pi_ai::Model::default()
        };
        let session = pi_coding::Session::new(pi_coding::SessionOptions {
            model,
            cwd: cwd.to_path_buf(),
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
        let session_dir = cwd.join("parent-sessions");
        std::fs::create_dir_all(&session_dir).expect("parent session directory");
        let recorder = pi_coding::start_session_in(
            cwd,
            None,
            None,
            Some(&session_dir),
            Some("persona-parent"),
            None,
        )
        .expect("parent recorder");
        session.record(recorder).expect("attach parent recorder");
        let mut resource_options = pi_coding::ResourceManagerOptions::new(cwd);
        resource_options.agent_dir = agent_dir.to_path_buf();
        let resources = pi_coding::ResourceManager::new(resource_options).expect("resources");
        session
            .attach_resources(resources)
            .await
            .expect("attach resources");
        let snapshot = session
            .resource_manager()
            .expect("resource manager")
            .snapshot();
        let artifacts = cwd.join("artifacts");
        std::fs::create_dir_all(&artifacts).expect("artifacts");
        let mut config = pi_coding::OrchestrationConfig::new(
            pi_coding::AgentCatalog::from_agents(snapshot.agents.clone()),
            &artifacts,
        );
        config.idle_ttl = None;
        let factory: pi_coding::ChildSessionFactory = std::sync::Arc::new(|_| {
            Box::pin(std::future::pending())
        });
        let runtime =
            pi_coding::OrchestrationRuntime::new(config, factory).expect("orchestration runtime");
        runtime
            .bind_and_recover(&session)
            .expect("bind persona runtime");
        pi_coding::Application::new_with_orchestration(session, runtime).await
    }

    #[tokio::test]
    async fn persona_list_excludes_ordinary_agents_and_role_includes_both() {
        let cwd = tempfile::tempdir().expect("cwd");
        let agent_dir = tempfile::tempdir().expect("agent dir");
        let application = persona_application(cwd.path(), agent_dir.path()).await;

        let persona_list = execute_interactive_persona_command(
            &application,
            parse_interactive_persona_command(None).expect("bare /persona"),
        )
        .await
        .expect("persona list");
        assert!(
            persona_list.contains("mentor"),
            "persona list must include personas: {persona_list}"
        );
        assert!(
            !persona_list.contains("reviewer"),
            "persona list must exclude ordinary agents: {persona_list}"
        );
        assert!(
            !persona_list.contains("task"),
            "persona list must exclude bundled agents: {persona_list}"
        );

        let role_list = execute_interactive_role_command(
            &application,
            parse_interactive_role_command(None).expect("bare /role"),
        )
        .expect("role list");
        assert!(role_list.contains("mentor"), "role list includes personas: {role_list}");
        assert!(
            role_list.contains("reviewer"),
            "role list includes ordinary agents: {role_list}"
        );
        assert!(role_list.contains("task"), "role list includes bundled: {role_list}");

        application.cleanup().await;
    }

    #[tokio::test]
    async fn persona_select_persists_preferred_agent_into_settings_and_config() {
        let cwd = tempfile::tempdir().expect("cwd");
        let agent_dir = tempfile::tempdir().expect("agent dir");
        let application = persona_application(cwd.path(), agent_dir.path()).await;

        execute_interactive_persona_command(
            &application,
            parse_interactive_persona_command(Some("mentor --select")).expect("select"),
        )
        .await
        .expect("select persona");

        let settings = application
            .settings_manager()
            .expect("settings")
            .settings();
        assert_eq!(
            settings
                .orchestration
                .as_ref()
                .and_then(|orchestration| orchestration.preferred_agent.as_deref()),
            Some("mentor")
        );

        // A newly built orchestration config carries the persisted preference.
        let snapshot = application.resource_snapshot().expect("snapshot");
        let model = pi_ai::Model {
            id: "cfg".into(),
            name: "cfg".into(),
            api: "cfg".into(),
            provider: "test".into(),
            ..pi_ai::Model::default()
        };
        let config = crate::session_run_blueprint::test_orchestration_config(
            &snapshot,
            &settings,
            &model,
        );
        assert_eq!(config.preferred_agent.as_deref(), Some("mentor"));

        application.cleanup().await;
    }

    #[tokio::test]
    async fn persona_show_reports_details_without_absolute_paths() {
        let cwd = tempfile::tempdir().expect("cwd");
        let agent_dir = tempfile::tempdir().expect("agent dir");
        let application = persona_application(cwd.path(), agent_dir.path()).await;

        let output = execute_interactive_persona_command(
            &application,
            parse_interactive_persona_command(Some("mentor")).expect("show"),
        )
        .await
        .expect("persona details");
        assert!(output.contains("persona: mentor"), "{output}");
        assert!(output.contains("personality: present"), "{output}");
        assert!(output.contains("scope: user"), "{output}");
        assert!(output.contains("softBudget:"), "{output}");
        assert!(
            !output.contains(agent_dir.path().to_string_lossy().as_ref()),
            "details must not leak absolute agent_dir paths: {output}"
        );
        assert!(
            !output.contains(cwd.path().to_string_lossy().as_ref()),
            "details must not leak absolute cwd paths: {output}"
        );

        application.cleanup().await;
    }

    #[tokio::test]
    async fn persona_run_spawns_through_orchestration_runtime() {
        let cwd = tempfile::tempdir().expect("cwd");
        let agent_dir = tempfile::tempdir().expect("agent dir");
        let application = persona_application(cwd.path(), agent_dir.path()).await;

        let output = execute_interactive_persona_command(
            &application,
            parse_interactive_persona_command(Some("run mentor draft the plan"))
                .expect("run parse"),
        )
        .await
        .expect("persona run");
        assert!(output.contains("Persona mentor started"), "{output}");
        assert!(output.contains("job:"), "{output}");
        assert!(output.contains("agent:"), "{output}");

        application.cleanup().await;
    }

    #[test]
    fn persona_root_containment_rejects_symlink_escape() {
        let base = tempfile::tempdir().expect("base");
        let scope = base.path().join("personas");
        std::fs::create_dir_all(&scope).expect("scope");
        let outside = base.path().join("outside");
        std::fs::create_dir_all(&outside).expect("outside");
        let link = scope.join("escaped");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &link).expect("symlink");
        #[cfg(not(unix))]
        {
            // Non-unix: containment still rejects non-child paths via canonicalize.
            let _ = link;
            return;
        }
        let err = ensure_persona_root_contained(&link, &[scope]).expect_err("symlink escape");
        assert!(
            err.to_string().contains("symlink") || err.to_string().contains("escapes"),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn persona_reset_removes_state_keeps_definition() {
        let cwd = tempfile::tempdir().expect("cwd");
        let agent_dir = tempfile::tempdir().expect("agent dir");
        let root = write_user_persona(agent_dir.path(), "mentor", "Guide the user patiently.");
        let application = persona_application(cwd.path(), agent_dir.path()).await;

        execute_interactive_persona_command(
            &application,
            parse_interactive_persona_command(Some("reset mentor --yes")).expect("reset"),
        )
        .await
        .expect("reset persona");

        assert!(root.join("persona.md").is_file(), "persona.md must remain");
        assert!(!root.join("memory").exists(), "memory must be cleared");
        assert!(!root.join("sessions").exists(), "sessions must be cleared");

        application.cleanup().await;
    }

    #[tokio::test]
    async fn persona_remove_deletes_definition_keeps_state() {
        let cwd = tempfile::tempdir().expect("cwd");
        let agent_dir = tempfile::tempdir().expect("agent dir");
        let root = write_user_persona(agent_dir.path(), "mentor", "Guide the user patiently.");
        let application = persona_application(cwd.path(), agent_dir.path()).await;

        execute_interactive_persona_command(
            &application,
            parse_interactive_persona_command(Some("remove mentor --yes")).expect("remove"),
        )
        .await
        .expect("remove persona");

        assert!(!root.join("persona.md").exists(), "persona.md must be deleted");
        assert!(root.join("memory").exists(), "memory must remain");
        assert!(root.join("sessions").exists(), "sessions must remain");
        assert!(root.is_dir(), "persona root must remain");

        application.cleanup().await;
    }

    #[tokio::test]
    async fn persona_purge_deletes_root() {
        let cwd = tempfile::tempdir().expect("cwd");
        let agent_dir = tempfile::tempdir().expect("agent dir");
        let root = write_user_persona(agent_dir.path(), "mentor", "Guide the user patiently.");
        let application = persona_application(cwd.path(), agent_dir.path()).await;

        execute_interactive_persona_command(
            &application,
            parse_interactive_persona_command(Some("remove mentor --purge --yes")).expect("purge"),
        )
        .await
        .expect("purge persona");

        assert!(!root.exists(), "persona root must be deleted");

        application.cleanup().await;
    }

    #[tokio::test]
    async fn persona_destructive_commands_reject_active_run() {
        for command in [
            "reset mentor --yes",
            "remove mentor --yes",
            "remove mentor --purge --yes",
        ] {
            let cwd = tempfile::tempdir().expect("cwd");
            let agent_dir = tempfile::tempdir().expect("agent dir");
            let root = write_user_persona(agent_dir.path(), "mentor", "Guide the user patiently.");
            let application = persona_application(cwd.path(), agent_dir.path()).await;
            let runtime = application
                .orchestration_runtime()
                .expect("orchestration runtime");
            runtime
                .spawn_tasks(
                    "Main",
                    0,
                    vec![pi_coding::TaskItem {
                        index: 0,
                        id: "MentorActive".to_owned(),
                        agent: "mentor".to_owned(),
                        assignment: "stay active".to_owned(),
                        ..Default::default()
                    }],
                )
                .expect("spawn active persona");

            let error = execute_interactive_persona_command(
                &application,
                parse_interactive_persona_command(Some(command)).expect("destructive command"),
            )
            .await
            .expect_err("active persona must reject destructive command");
            assert!(error.to_string().contains("active orchestration job"), "{error:#}");
            assert!(root.join("persona.md").is_file(), "definition must remain");
            assert!(root.join("memory").is_dir(), "memory must remain");
            assert!(root.join("sessions").is_dir(), "sessions must remain");

            application.cleanup().await;
        }
    }
    #[test]
    fn persona_parser_handles_new_and_edit() {
        assert_eq!(
            parse_interactive_persona_command(Some("new mentor")).expect("new"),
            InteractivePersonaCommand::New {
                name: "mentor".to_owned()
            }
        );
        assert_eq!(
            parse_interactive_persona_command(Some("edit mentor")).expect("edit"),
            InteractivePersonaCommand::Edit {
                name: "mentor".to_owned()
            }
        );
        // Bare subcommands require a name.
        let new_err = parse_interactive_persona_command(Some("new")).expect_err("bare new");
        assert!(new_err.to_string().contains("usage"), "{new_err}");
        let edit_err = parse_interactive_persona_command(Some("edit")).expect_err("bare edit");
        assert!(edit_err.to_string().contains("usage"), "{edit_err}");
        // Trailing tokens are rejected so flags can never become a name.
        let trailing = parse_interactive_persona_command(Some("new mentor --foo"))
            .expect_err("trailing token");
        assert!(trailing.to_string().contains("unexpected argument"), "{trailing}");
        // Path-unsafe names are rejected before any path is constructed.
        let traversal = parse_interactive_persona_command(Some("new ../escape"))
            .expect_err("traversal name");
        assert!(
            traversal.to_string().contains("ASCII") || traversal.to_string().contains("letters"),
            "{traversal}"
        );
        let slash = parse_interactive_persona_command(Some("new a/b")).expect_err("slash name");
        assert!(
            slash.to_string().contains("ASCII") || slash.to_string().contains("letters"),
            "{slash}"
        );
    }

    fn valid_persona_content(name: &str) -> String {
        format!(
            "---\nname: {name}\ndescription: {name} persona\npersonality: eager\n---\nDo the {name} work.\n"
        )
    }

    #[tokio::test]
    async fn persona_new_creates_and_reloads_into_catalog() {
        let cwd = tempfile::tempdir().expect("cwd");
        let agent_dir = tempfile::tempdir().expect("agent dir");
        let application = persona_application(cwd.path(), agent_dir.path()).await;

        let root = agent_dir.path().join("personas").join("scout");
        assert!(!root.join("persona.md").exists(), "scout must not pre-exist");

        let message = commit_persona_definition(
            &application,
            "scout",
            &valid_persona_content("scout"),
            PersonaEditKind::New,
        )
        .await
        .expect("new persona commit");
        assert!(message.contains("created"), "{message}");
        assert!(message.contains("user scope"), "{message}");

        // Atomic write produced a real, regular persona.md (no temp leftover).
        assert!(root.join("persona.md").is_file(), "persona.md written");
        assert!(
            std::fs::symlink_metadata(root.join("persona.md"))
                .expect("meta")
                .is_file(),
            "persona.md is regular"
        );
        assert_eq!(
            std::fs::read_dir(&root)
                .expect("dir")
                .filter_map(Result::ok)
                .count(),
            1,
            "no temp file left behind"
        );

        // Live reload: /persona now lists scout.
        let list = execute_interactive_persona_command(
            &application,
            parse_interactive_persona_command(None).expect("list"),
        )
        .await
        .expect("persona list");
        assert!(list.contains("scout"), "scout discovered after reload: {list}");

        application.cleanup().await;
    }

    #[tokio::test]
    async fn persona_new_rejects_duplicate_agent_and_persona_names() {
        let cwd = tempfile::tempdir().expect("cwd");
        let agent_dir = tempfile::tempdir().expect("agent dir");
        let application = persona_application(cwd.path(), agent_dir.path()).await;

        let mentor_err = commit_persona_definition(
            &application,
            "mentor",
            &valid_persona_content("mentor"),
            PersonaEditKind::New,
        )
        .await
        .expect_err("duplicate persona");
        assert!(mentor_err.to_string().contains("already in use"), "{mentor_err}");

        let reviewer_err = commit_persona_definition(
            &application,
            "reviewer",
            &valid_persona_content("reviewer"),
            PersonaEditKind::New,
        )
        .await
        .expect_err("duplicate agent");
        assert!(reviewer_err.to_string().contains("already in use"), "{reviewer_err}");

        application.cleanup().await;
    }

    #[tokio::test]
    async fn persona_new_rejects_name_directory_mismatch_and_traversal() {
        let cwd = tempfile::tempdir().expect("cwd");
        let agent_dir = tempfile::tempdir().expect("agent dir");
        let application = persona_application(cwd.path(), agent_dir.path()).await;

        let mismatch = commit_persona_definition(
            &application,
            "scout",
            &valid_persona_content("wrongname"),
            PersonaEditKind::New,
        )
        .await
        .expect_err("name mismatch");
        assert!(mismatch.to_string().contains("must match"), "{mismatch}");

        let traversal =
            commit_persona_definition(&application, "../x", &valid_persona_content("x"), PersonaEditKind::New)
                .await
                .expect_err("traversal name");
        assert!(
            traversal.to_string().contains("ASCII") || traversal.to_string().contains("letters"),
            "{traversal}"
        );

        application.cleanup().await;
    }

    #[tokio::test]
    async fn persona_new_rejects_symlinked_user_scope() {
        let cwd = tempfile::tempdir().expect("cwd");
        let agent_dir = tempfile::tempdir().expect("agent dir");
        let application = persona_application(cwd.path(), agent_dir.path()).await;
        let real = agent_dir.path().join("personas");
        let moved = agent_dir.path().join("personas_real");
        std::fs::rename(&real, &moved).expect("move personas");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&moved, &real).expect("symlink personas");
            let err = commit_persona_definition(
                &application,
                "scout",
                &valid_persona_content("scout"),
                PersonaEditKind::New,
            )
            .await
            .expect_err("symlinked scope");
            assert!(err.to_string().contains("symlink"), "{err}");
        }
        #[cfg(not(unix))]
        let _ = moved;
        application.cleanup().await;
    }

    #[tokio::test]
    async fn persona_edit_rewrites_and_preserves_selection() {
        let cwd = tempfile::tempdir().expect("cwd");
        let agent_dir = tempfile::tempdir().expect("agent dir");
        let root = write_user_persona(agent_dir.path(), "mentor", "Guide the user patiently.");
        let application = persona_application(cwd.path(), agent_dir.path()).await;

        execute_interactive_persona_command(
            &application,
            parse_interactive_persona_command(Some("mentor --select")).expect("select"),
        )
        .await
        .expect("select mentor");

        let edited = "---\nname: mentor\ndescription: sharper mentor\npersonality: sharper\nsoftBudget:\n  maxRequests: 8\n---\nGuide sharply.\n";
        let message = commit_persona_definition(&application, "mentor", edited, PersonaEditKind::Edit)
            .await
            .expect("edit persona");
        assert!(message.contains("edited"), "{message}");

        let on_disk = std::fs::read_to_string(root.join("persona.md")).expect("read persona.md");
        assert!(on_disk.contains("sharper"), "disk reflects edit: {on_disk}");

        let current = execute_interactive_persona_command(
            &application,
            parse_interactive_persona_command(Some("--current")).expect("current"),
        )
        .await
        .expect("current");
        assert!(current.contains("Selected persona: mentor"), "{current}");

        let details = execute_interactive_persona_command(
            &application,
            parse_interactive_persona_command(Some("mentor")).expect("show"),
        )
        .await
        .expect("persona details");
        assert!(details.contains("personality: present"), "{details}");
        assert!(details.contains("softBudget:"), "{details}");

        application.cleanup().await;
    }

    #[tokio::test]
    async fn persona_edit_rejects_missing_and_mismatch_and_nonregular() {
        let cwd = tempfile::tempdir().expect("cwd");
        let agent_dir = tempfile::tempdir().expect("agent dir");
        let root = write_user_persona(agent_dir.path(), "mentor", "Guide the user patiently.");
        let application = persona_application(cwd.path(), agent_dir.path()).await;

        let missing = commit_persona_definition(&application, "ghost", &valid_persona_content("ghost"), PersonaEditKind::Edit)
            .await
            .expect_err("missing persona");
        assert!(missing.to_string().contains("unknown persona"), "{missing}");

        let mismatch = commit_persona_definition(&application, "mentor", &valid_persona_content("renamed"), PersonaEditKind::Edit)
            .await
            .expect_err("edit name mismatch");
        assert!(mismatch.to_string().contains("must match"), "{mismatch}");

        // Non-regular persona.md (a directory in its place).
        std::fs::remove_file(root.join("persona.md")).expect("remove persona.md");
        std::fs::create_dir(root.join("persona.md")).expect("dir in its place");
        let nonregular = commit_persona_definition(&application, "mentor", &valid_persona_content("mentor"), PersonaEditKind::Edit)
            .await
            .expect_err("non-regular persona.md");
        assert!(nonregular.to_string().contains("not a regular file"), "{nonregular}");

        application.cleanup().await;
    }

    #[tokio::test]
    async fn persona_edit_rejects_symlinked_persona_md() {
        let cwd = tempfile::tempdir().expect("cwd");
        let agent_dir = tempfile::tempdir().expect("agent dir");
        let root = write_user_persona(agent_dir.path(), "mentor", "Guide the user patiently.");
        let application = persona_application(cwd.path(), agent_dir.path()).await;
        #[cfg(unix)]
        {
            let target = agent_dir.path().join("elsewhere.md");
            std::fs::write(&target, "x").expect("target");
            std::fs::remove_file(root.join("persona.md")).expect("remove persona.md");
            std::os::unix::fs::symlink(&target, root.join("persona.md")).expect("symlink persona.md");
            let err = commit_persona_definition(&application, "mentor", &valid_persona_content("mentor"), PersonaEditKind::Edit)
                .await
                .expect_err("symlinked persona.md");
            assert!(err.to_string().contains("symlink"), "{err}");
        }
        application.cleanup().await;
    }

    #[tokio::test]
    async fn persona_editor_seed_returns_template_for_new_and_content_for_edit() {
        let cwd = tempfile::tempdir().expect("cwd");
        let agent_dir = tempfile::tempdir().expect("agent dir");
        let _root = write_user_persona(agent_dir.path(), "mentor", "Guide the user patiently.");
        let application = persona_application(cwd.path(), agent_dir.path()).await;

        let seed = persona_editor_seed(&application, "scout", PersonaEditKind::New).expect("new seed");
        assert!(seed.contains("name: scout"), "{seed}");
        assert!(seed.contains("Describe"), "{seed}");

        let edit_seed = persona_editor_seed(&application, "mentor", PersonaEditKind::Edit).expect("edit seed");
        assert!(edit_seed.contains("name: mentor"), "{edit_seed}");
        assert!(edit_seed.contains("Guide the user patiently"), "{edit_seed}");

        let missing = persona_editor_seed(&application, "ghost", PersonaEditKind::Edit).expect_err("missing seed");
        assert!(missing.to_string().contains("unknown persona"), "{missing}");

        application.cleanup().await;
    }
}
