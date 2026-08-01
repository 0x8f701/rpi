//! Session setup, resume, and print-mode execution.
//!
//! This wires the parsed CLI flags into the `pi_coding::Session` facade,
//! mirroring the Go `main()`: resume-path resolution, model restoration from
//! a resumed branch, API-key gating, reasoning-level priority, session
//! recording, and print-mode streaming.

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, anyhow, bail};

use crate::args::Cli;
use crate::commands::{resolve_model_spec, resolve_resume_for_startup};
use crate::extension_ui::{ExtensionUiAdapter, NonInteractiveExtensionUiHost};
use crate::models_config::ModelRequestAuth;
use crate::output::{parse_thinking_level, thinking_level_str, warn_line};
use pi_coding::{
    AgentCatalog, Application, BranchContext, DEFAULT_COMPACTION_SETTINGS, ExtensionMode,
    ExtensionPermissionSet, ExtensionRuntime, ExtensionRuntimeOptions, ExtensionUiHost,
    OrchestrationConfig, OrchestrationRuntime, OrchestrationSkill, ResourceManager,
    ResourceManagerOptions, Session, SessionOptions, ToolSelection,
};

static OFFLINE: AtomicBool = AtomicBool::new(false);

pub fn set_offline(offline: bool) {
    OFFLINE.store(offline, Ordering::Release);
}

#[must_use]
pub fn offline() -> bool {
    OFFLINE.load(Ordering::Acquire)
        || std::env::var("PI_OFFLINE").is_ok_and(|value| {
            matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes")
        })
}

/// The fully-resolved live application and its mode-specific extension UI adapter.
pub struct RunSession {
    pub application: Application,
    pub extension_ui: Option<ExtensionUiAdapter>,
    pub model: pi_ai::Model,
    pub scoped_models: Option<Vec<pi_ai::Model>>,
}

fn extension_mode(cli: &Cli) -> ExtensionMode {
    match cli.mode {
        Some(crate::args::Mode::Json) => ExtensionMode::Json,
        Some(crate::args::Mode::Rpc) => ExtensionMode::Rpc,
        Some(crate::args::Mode::Text) => ExtensionMode::Print,
        None if cli.is_print_mode() => ExtensionMode::Print,
        None => ExtensionMode::Tui,
    }
}

async fn authenticated_model(
    model: pi_ai::Model,
    explicit_key: Option<&str>,
) -> Result<Option<(pi_ai::Model, ModelRequestAuth)>> {
    if explicit_key.is_none() && !crate::models_config::has_configured_auth(&model) {
        return Ok(None);
    }
    let auth = crate::models_config::resolve_model_request_auth_async(&model, explicit_key, None).await?;
    Ok(crate::models_config::model_is_available_for_request_auth(&model, &auth)
        .then_some((model, auth)))
}

async fn first_authenticated_model(
    explicit_key: Option<&str>,
) -> Result<Option<(pi_ai::Model, ModelRequestAuth)>> {
    let mut providers = pi_ai::get_providers();
    providers.sort();
    providers.retain(|provider| provider != "faux");
    let mut candidates = Vec::new();
    for provider in &providers {
        if let Some(id) = pi_coding::default_model_per_provider(provider)
            && let Some(model) = pi_ai::get_model(provider, id)
        {
            candidates.push(model);
        }
    }
    let mut seen = candidates
        .iter()
        .map(|model| (model.provider.clone(), model.id.clone()))
        .collect::<HashSet<_>>();
    for provider in providers {
        let mut models = pi_ai::get_models(&provider);
        models.sort_by(|left, right| left.id.cmp(&right.id));
        for model in models {
            if seen.insert((model.provider.clone(), model.id.clone())) {
                candidates.push(model);
            }
        }
    }
    for model in candidates {
        if let Some(selected) = authenticated_model(model, explicit_key).await? {
            return Ok(Some(selected));
        }
    }
    Ok(None)
}


fn resolve_initial_thinking_level(
    cli_level: Option<&str>,
    model_level: &str,
    resume: Option<&BranchContext>,
    resume_has_thinking_entry: bool,
    settings_level: Option<&str>,
) -> pi_agent::ThinkingLevel {
    let level = cli_level
        .filter(|value| !value.is_empty())
        .or_else(|| (!model_level.is_empty()).then_some(model_level))
        .or_else(|| {
            resume
                .filter(|_| resume_has_thinking_entry)
                .map(|context| context.thinking_level.as_str())
        })
        .or_else(|| settings_level.filter(|value| !value.is_empty()))
        .unwrap_or_default();
    parse_thinking_level(level)
}

pub(crate) async fn resolve_model_scope(patterns: &[String]) -> Result<Vec<pi_ai::Model>> {
    use globset::GlobBuilder;

    let mut available = pi_ai::get_providers()
        .into_iter()
        .flat_map(|provider| pi_ai::get_models(&provider))
        .collect::<Vec<_>>();
    available.sort_by(|left, right| {
        (&left.provider, &left.id).cmp(&(&right.provider, &right.id))
    });
    available.dedup_by(|left, right| {
        left.provider == right.provider && left.id == right.id
    });

    available = crate::models_config::filter_models_for_resolved_auth_async(available, None).await;
    let mut scoped = Vec::new();
    let mut seen = HashSet::new();
    for pattern in patterns {
        let trimmed = pattern.trim();
        let base_pattern = trimmed.rsplit_once(':').map_or(trimmed, |(base, suffix)| {
            matches!(suffix, "off" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max")
                .then_some(base)
                .unwrap_or(trimmed)
        });
        let matches = if base_pattern.contains(['*', '?', '[']) {
            let matcher = GlobBuilder::new(base_pattern)
                .case_insensitive(true)
                .build()
                .with_context(|| format!("invalid model pattern {pattern:?}"))?
                .compile_matcher();
            available
                .iter()
                .filter(|model| {
                    matcher.is_match(format!("{}/{}", model.provider, model.id))
                        || matcher.is_match(&model.id)
                })
                .cloned()
                .collect::<Vec<_>>()
        } else {
            available
                .iter()
                .filter(|model| {
                    format!("{}/{}", model.provider, model.id).eq_ignore_ascii_case(base_pattern)
                        || model.id.eq_ignore_ascii_case(base_pattern)
                })
                .cloned()
                .collect::<Vec<_>>()
        };
        if matches.is_empty() {
            warn_line(&format!("Warning: No models match pattern {pattern:?}"));
        }
        for model in matches {
            if seen.insert((model.provider.clone(), model.id.clone())) {
                scoped.push(model);
            }
        }
    }
    Ok(scoped)
}

fn resolve_prompt_input(
    input: &str,
    cwd: &Path,
    description: &str,
) -> Result<(String, Option<PathBuf>)> {
    let candidate = Path::new(input);
    let candidate = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        cwd.join(candidate)
    };
    if !candidate.exists() {
        return Ok((input.to_owned(), None));
    }
    let root = cwd
        .canonicalize()
        .with_context(|| format!("resolving working directory {}", cwd.display()))?;
    let canonical = candidate
        .canonicalize()
        .with_context(|| format!("resolving {description} file {}", candidate.display()))?;
    if !Path::new(input).is_absolute() && !canonical.starts_with(&root) {
        bail!("unsafe {description} path {input:?}: symlink escapes the working directory");
    }
    if !canonical.is_file() {
        bail!("{description} path is not a file: {}", canonical.display());
    }
    Ok((String::new(), Some(canonical)))
}

fn resolve_session_argument(
    argument: &str,
    cwd: &Path,
    session_dir: Option<&Path>,
) -> Result<PathBuf> {
    let path_like = argument.contains('/') || argument.contains('\\') || argument.ends_with(".jsonl");
    if path_like {
        let root = session_dir
            .map(Path::to_path_buf)
            .unwrap_or_else(|| pi_coding::default_session_dir(cwd));
        let path = Path::new(argument);
        let path = if path.is_absolute() { path.to_path_buf() } else { cwd.join(path) };
        return pi_coding::validated_saved_session_path(&root, &path)
            .with_context(|| format!("validating explicit session path {argument:?}"));
    }
    let sessions = pi_coding::list_sessions_in(cwd, session_dir);
    if let Some(exact) = sessions.iter().find(|session| session.id == argument) {
        return Ok(exact.path.clone());
    }
    let matches = sessions
        .iter()
        .filter(|session| session.id.starts_with(argument))
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => bail!("No session found matching {argument:?}"),
        [session] => Ok(session.path.clone()),
        _ => bail!(
            "Session id prefix {argument:?} is ambiguous; matches: {}",
            matches.iter().map(|session| session.id.as_str()).collect::<Vec<_>>().join(", ")
        ),
    }
}

fn requested_tool(name: &str, cli: &Cli) -> bool {
    cli.tools.as_ref().is_some_and(|tools| tools.iter().any(|tool| tool == name))
}

struct MainToolGates {
    orchestration: bool,
    process: bool,
    glob: bool,
    todo: bool,
    goal: bool,
}

fn main_tool_gates(settings: &pi_coding::Settings, cli: &Cli) -> MainToolGates {
    let tools_allowed = !cli.no_tools;
    MainToolGates {
        orchestration: tools_allowed
            && (settings.orchestration_enabled()
                || requested_tool("task", cli)
                || requested_tool("hub", cli)),
        process: tools_allowed
            && (settings.process_tool_enabled() || requested_tool("process", cli)),
        glob: tools_allowed
            && (settings.glob_tool_enabled() || requested_tool("glob", cli)),
        todo: tools_allowed
            && (settings.todo_tool_enabled() || requested_tool("todo", cli)),
        goal: tools_allowed && requested_tool("goal", cli),
    }
}

fn orchestration_config(
    snapshot: &pi_coding::ResourceSnapshot,
    settings: &pi_coding::Settings,
    parent_model: &pi_ai::Model,
) -> OrchestrationConfig {
    let mut config = OrchestrationConfig::new(
        AgentCatalog::from_agents(snapshot.agents.clone()),
        snapshot.cwd.join(".pi").join("artifacts"),
    );
    config.skills = snapshot.skills.iter().map(OrchestrationSkill::from).collect();
    if let Some(orchestration) = &settings.orchestration {
        if let Some(value) = orchestration.max_concurrency { config.max_concurrency = value; }
        if let Some(value) = orchestration.max_recursion_depth { config.max_recursion_depth = value; }
        if let Some(value) = orchestration.mailbox_capacity { config.mailbox_capacity = value; }
        if let Some(value) = orchestration.max_tools_per_agent { config.max_tools_per_agent = value; }
    }
    config = config.with_selector_settings(settings.selector.clone().unwrap_or_default());
    config.agent_settings = settings.agents.clone();
    config.parent_model = parent_model.clone();
    config
}


/// Build the live [`Session`] from the parsed CLI flags, applying resume,
/// model restoration, and recording exactly as the Go CLI does.
pub async fn build_session(cli: &Cli) -> Result<RunSession> {
    crate::models_config::load_custom_models()?;
    if let Err(error) = crate::models_config::load_radius_catalog(!offline()).await
        && cli.verbose
    {
        warn_line(&format!("Warning: Could not load Radius catalog: {error:#}"));
    }
    let llama = pi_coding::LlamaManager::default();
    if llama.effective_settings()?.is_some() && !offline() {
        if let Ok(refreshed) = llama.refresh_catalog().await
            && let Some(warning) = refreshed.warning
        {
            warn_line(&format!(
                "Warning: llama.cpp router unavailable; using cached catalog: {warning}"
            ));
        }
    } else if pi_ai::get_models(pi_ai::LLAMA_PROVIDER_ID).is_empty() {
        llama.load_cached_catalog()?;
    }
    for provider in pi_ai::get_providers() {
        crate::models_config::clear_runtime_api_key(&provider);
    }
    let mut cwd: PathBuf = match &cli.cwd {
        Some(cwd) => cwd.clone(),
        None => std::env::current_dir().context("getting current directory")?,
    };
    cwd = cwd
        .canonicalize()
        .with_context(|| format!("resolving working directory {}", cwd.display()))?;
    let mut workspace = pi_coding::WorkspaceRoots::new(&cwd, &cli.add_dirs)?;
    let session_dir = cli.session_dir.as_deref();
    if let Some(id) = cli.session_id.as_deref() {
        pi_coding::validate_session_id(id)?;
    }
    let resume_path: Option<PathBuf> = if let Some(input) = cli.resume.as_deref() {
        Some(resolve_resume_for_startup(input, Some(&cwd))?.path)
    } else if let Some(argument) = cli.session.as_deref() {
        Some(resolve_session_argument(argument, &cwd, session_dir)?)
    } else if let Some(argument) = cli.fork.as_deref() {
        Some(resolve_session_argument(argument, &cwd, session_dir)?)
    } else if cli.continue_latest {
        // Deliberately native-only: `--continue` must never surprise-import a
        // foreign session merely because it is newer.
        pi_coding::list_sessions_in(&cwd, session_dir)
            .into_iter()
            .next()
            .map(|session| session.path)
    } else if let Some(id) = cli.session_id.as_deref() {
        pi_coding::list_sessions_in(&cwd, session_dir)
            .into_iter()
            .find(|session| session.id == id)
            .map(|session| session.path)
    } else {
        None
    };

    let mut resume_ctx: Option<BranchContext> = None;
    let mut resume_has_thinking_entry = false;
    if let Some(path) = &resume_path {
        let tree = pi_coding::load_session_tree(path)
            .with_context(|| format!("loading session {}", path.display()))?;
        if cli.cwd.is_none() && !tree.header.cwd.as_os_str().is_empty() && tree.header.cwd.exists() {
            // Restore the session's recorded cwd only when it still exists;
            // an imported/foreign session may record a cwd that is absent on
            // this machine — fall back to the current cwd rather than fail.
            cwd = tree.header.cwd.canonicalize().with_context(|| {
                format!("resolving resumed working directory {}", tree.header.cwd.display())
            })?;
        }
        workspace = pi_coding::WorkspaceRoots::new(&cwd, &cli.add_dirs)?;
        resume_has_thinking_entry = tree.has_thinking_entry();
        resume_ctx = Some(tree.build_context(None));
    }

    let mut resource_options = ResourceManagerOptions::new(&cwd);
    let stdin_tty = std::io::IsTerminal::is_terminal(&std::io::stdin());
    let stdout_tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
    resource_options.headless = matches!(cli.mode, Some(crate::args::Mode::Json | crate::args::Mode::Rpc))
        || cli.is_print_mode()
        || !stdin_tty
        || !stdout_tty;
    resource_options.project_trust_override = if cli.approve {
        Some(true)
    } else if cli.no_approve {
        Some(false)
    } else {
        None
    };
    resource_options.explicit_extension_paths.clone_from(&cli.extensions);
    resource_options.explicit_skill_paths.clone_from(&cli.skills);
    resource_options.explicit_prompt_paths.clone_from(&cli.prompt_templates);
    resource_options.explicit_theme_paths.clone_from(&cli.themes);
    resource_options.disable_extensions = cli.no_extensions;
    resource_options.disable_skills = cli.no_skills;
    resource_options.disable_prompt_templates = cli.no_prompt_templates;
    resource_options.disable_themes = cli.no_themes;
    resource_options.disable_context_files = cli.no_context_files;
    if let Some(input) = cli.system.as_deref() {
        let (prompt, path) = resolve_prompt_input(input, &cwd, "system prompt")?;
        resource_options.system_prompt = Some(prompt);
        resource_options.system_prompt_path = path;
    }
    for input in &cli.append_system_prompt {
        let (prompt, path) = resolve_prompt_input(input, &cwd, "append system prompt")?;
        resource_options.append_system_prompt.push(prompt);
        resource_options.append_system_prompt_paths.push(path);
    }
    let resources = ResourceManager::new(resource_options)
        .context("loading settings and resources")?;
    let settings = resources.snapshot().settings.clone();
    if cli.verbose {
        for diagnostic in resources.diagnostics() {
            let path = diagnostic.path.as_ref().map_or(String::new(), |path| {
                format!(" ({})", path.display())
            });
            eprintln!("{:?}: {}{}", diagnostic.level, diagnostic.message, path);
        }
    }
    let model_patterns = cli.models.as_deref().or_else(|| settings.scoped_model_patterns());
    let scoped_models = match model_patterns {
        Some(patterns) => Some(resolve_model_scope(patterns).await?),
        None => None,
    };

    // 3. Resolve the initial model: explicit CLI, authenticated resumed model,
    //    authenticated settings default, then the first authenticated model.
    let explicit_model = cli.model.as_deref().filter(|spec| !spec.is_empty());
    let explicit_key = cli.api_key.as_deref();
    let mut parsed_think = String::new();
    let mut selected: Option<(pi_ai::Model, ModelRequestAuth)> = None;
    let explicit_model_spec = explicit_model.map(|spec| {
        let Some(provider) = cli.provider.as_deref() else {
            return spec.to_owned();
        };
        let prefix = format!("{provider}/");
        if spec.get(..prefix.len()).is_some_and(|value| value.eq_ignore_ascii_case(&prefix)) {
            spec.to_owned()
        } else {
            format!("{provider}/{spec}")
        }
    });
    if let Some(spec) = explicit_model_spec.as_deref() {
        let (model, level) = resolve_model_spec(spec)?;
        parsed_think = level;
        selected = authenticated_model(model, explicit_key).await?;
        if selected.is_none() {
            bail!("Model {spec:?} is not available for the resolved credential");
        }
    } else if let Some(ctx) = &resume_ctx
        && let (Some(provider), Some(id)) = (ctx.provider.as_ref(), ctx.model_id.as_ref())
    {
        match pi_ai::get_model(provider, id) {
            Some(model) => match authenticated_model(model, explicit_key).await? {
                Some(resolved) => selected = Some(resolved),
                None => warn_line(&format!(
                    "Warning: Could not restore model {provider}/{id} (no auth configured)"
                )),
            },
            None => warn_line(&format!("Warning: Could not restore model {provider}/{id}")),
        }
    }
    if selected.is_none()
        && let (Some(provider), Some(id)) = (
            settings
                .default_provider
                .as_deref()
                .filter(|value| !value.is_empty()),
            settings
                .default_model
                .as_deref()
                .filter(|value| !value.is_empty()),
        )
        && let Some(model) = pi_ai::get_model(provider, id)
    {
        selected = authenticated_model(model, explicit_key).await?;
    }
    if selected.is_none() {
        selected = first_authenticated_model(explicit_key).await?;
    }
    let (model, auth) = selected.ok_or_else(|| {
        anyhow!(
            "No authenticated models available. Configure auth.json, models.json, a provider API-key environment variable, or pass --model with --api-key."
        )
    })?;
    if let Some(key) = explicit_key.filter(|key| !key.trim().is_empty()) {
        crate::models_config::set_runtime_api_key(&model.provider, key);
    }
    let api_key = auth.api_key;
    let auth_resolver = crate::models_config::session_auth_resolver(cli.api_key.clone());

    // 4. Thinking priority: --think, model suffix, resumed recorded thinking,
    //    settings default, then the existing medium default.
    let thinking_level = resolve_initial_thinking_level(
        cli.think.as_deref(),
        &parsed_think,
        resume_ctx.as_ref(),
        resume_has_thinking_entry,
        settings.default_thinking_level.map(thinking_level_str),
    );

    // 6. Extensions must be loaded before Session creation so their tools join
    // the default coding tool set exposed to the agent.
    let mode = extension_mode(cli);
    let extension_ui =
        matches!(mode, ExtensionMode::Tui | ExtensionMode::Rpc).then(ExtensionUiAdapter::new);
    let ui_host: Option<Arc<dyn ExtensionUiHost>> = extension_ui
        .as_ref()
        .map(|adapter| Arc::new(adapter.clone()) as Arc<dyn ExtensionUiHost>)
        .or_else(|| {
            Some(Arc::new(NonInteractiveExtensionUiHost::default()) as Arc<dyn ExtensionUiHost>)
        });
    let runtime = ExtensionRuntime::process(
        ui_host,
        ExtensionRuntimeOptions {
            mode,
            ..ExtensionRuntimeOptions::default()
        },
    );
    let permissions = ExtensionPermissionSet::allow_all();
    let specs = resources
        .extension_specs(&permissions)
        .context("validating configured extensions")?;
    let report = runtime.load(specs).await;
    if !report.failures.is_empty() {
        runtime.shutdown().await;
        return Err(extension_startup_error(&report));
    }

    let snapshot = resources.snapshot();
    let system_prompt = snapshot.system_prompt.clone().unwrap_or_default();
    let mut opts = SessionOptions {
        model: model.clone(),
        cwd: cwd.clone(),
        system_prompt,
        thinking_level,
        api_key: api_key.clone(),
        compaction: Some(DEFAULT_COMPACTION_SETTINGS),
        stream_options: Default::default(),
        tools: None,
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: None,
        auth_resolver: Some(auth_resolver.clone()),
    };
    settings.apply_session_options(&mut opts)?;
    let gates = main_tool_gates(&settings, cli);
    let (orchestration, uri_resolver) = if gates.orchestration {
        let config = orchestration_config(&snapshot, &settings, &model);
        let resolver_slot = Arc::new(parking_lot::Mutex::new(
            None::<pi_coding::InternalUriResolverFn>,
        ));
        let child_resolver_slot = resolver_slot.clone();
        let uri_resolver: pi_coding::InternalUriResolverFn = Arc::new(move |uri| {
            child_resolver_slot
                .lock()
                .as_ref()
                .ok_or_else(|| anyhow!("orchestration URI resolver is not initialized"))?(uri)
        });
        let factory = OrchestrationRuntime::child_factory_from_snapshot_and_uri(
            pi_coding::ChildSessionOptionsSnapshot {
                model: model.clone(),
                cwd: cwd.clone(),
                thinking_level,
                api_key: api_key.clone(),
                stream_options: opts.stream_options.clone(),
                stream_fn: pi_agent::AgentOptions::default().stream_fn,
                auth_resolver: Some(auth_resolver),
            },
            Some(uri_resolver.clone()),
        );
        let orchestration = OrchestrationRuntime::new(config, factory)?;
        *resolver_slot.lock() = Some(orchestration.read_uri_resolver());
        (Some(orchestration), Some(uri_resolver))
    } else {
        (None, None)
    };
    let mut additional_tools = runtime.agent_tools();
    if let Some(orchestration) = &orchestration {
        additional_tools.extend(orchestration.agent_tools("Main", 0));
    }
    let goal_tool = gates.goal.then(pi_coding::GoalToolBinding::default);
    if let Some(binding) = &goal_tool {
        additional_tools.push(binding.tool());
    }
    let selection = ToolSelection {
        allow: cli.tools.clone(),
        deny: cli.exclude_tools.clone(),
        disable_all: cli.no_tools,
        disable_builtins: cli.no_builtin_tools,
        enable_process: gates.process,
        enable_glob: gates.glob,
    };
    let session = if gates.todo {
        Session::new_with_todo_additional_tools_filtered_discovery_workspace_and_uri(
            opts,
            additional_tools,
            selection,
            pi_coding::ResourceDiscovery::Disabled,
            workspace.clone(),
            uri_resolver.clone(),
        )
    } else {
        Session::new_with_additional_tools_filtered_discovery_workspace_and_uri(
            opts,
            additional_tools,
            selection,
            pi_coding::ResourceDiscovery::Disabled,
            workspace,
            uri_resolver,
        )
    };
    let session = match session {
        Ok(session) => session,
        Err(error) => {
            if let Some(orchestration) = orchestration { orchestration.shutdown().await; }
            runtime.shutdown().await;
            return Err(error).context("building session");
        }
    };
    session.set_steering_mode(settings.steering_mode()).await;
    session.set_follow_up_mode(settings.follow_up_mode()).await;
    session.set_retry_settings(settings.retry_settings());
    if let Err(error) = session.attach_resources(resources).await {
        if let Some(orchestration) = orchestration { orchestration.shutdown().await; }
        runtime.shutdown().await;
        return Err(error).context("attaching settings and resources");
    }

    let setup_result: Result<()> = async {
        if let Some(path) = &resume_path {
            let ctx = resume_ctx.expect("resume context");
            let messages_len = ctx.messages.len();
            session.load_history(ctx.messages).await?;
            eprintln!(
                "\x1b[2mresumed {} messages from {}\x1b[0m",
                messages_len,
                path.display()
            );
            if !cli.no_session {
                let recorder = if cli.fork.is_some() {
                    pi_coding::fork_session_in(
                        path,
                        &cwd,
                        session_dir,
                        cli.session_id.as_deref(),
                    )?
                } else {
                    pi_coding::resume_session(path)?
                };
                if !resume_has_thinking_entry {
                    recorder.record_thinking_level(thinking_level_str(thinking_level))?;
                }
                session.record(recorder)?;
            }
        } else if !cli.no_session {
            let recorder = pi_coding::start_session_in(
                &cwd,
                Some(&model),
                Some(thinking_level_str(thinking_level)),
                session_dir,
                cli.session_id.as_deref(),
                None,
            )?;
            session.record(recorder)?;
        }
        if let Some(name) = cli.name.as_deref() {
            session.set_session_name(name)?;
        }
        Ok(())
    }
    .await;
    if let Err(error) = setup_result {
        session.abort().await;
        session.wait_for_idle().await;
        if let Some(orchestration) = orchestration {
            orchestration.shutdown().await;
        }
        runtime.shutdown().await;
        return Err(error);
    }

    let application = Application::new_with_extensions(session, runtime, permissions).await;
    if resume_path.is_some() {
        application.prepare_resumed_goal(cli.fork.is_some())?;
    }
    if let Some(binding) = goal_tool {
        application.attach_goal_tool(binding)?;
    }
    if let Some(orchestration) = orchestration {
        if let Err(error) = application.attach_orchestration(orchestration) {
            application.cleanup().await;
            return Err(error);
        }
    }
    Ok(RunSession {
        application,
        extension_ui,
        model,
        scoped_models,
    })
}

/// Run a single prompt in print mode through the shared [`Application`]
/// lifecycle and human event renderer. Returns the final assistant text.
pub async fn run_print_to<W>(
    application: &Application,
    prompt: &str,
    writer: &mut W,
    ansi: bool,
) -> Result<String>
where
    W: std::io::Write,
{
    if let Some(argument) = prompt.trim().strip_prefix("/goal") {
        let command = crate::goal_commands::parse_interactive_goal_command(
            (!argument.trim().is_empty()).then_some(argument.trim()),
        )?;
        let output = crate::goal_commands::execute_interactive_goal_command(application, command)?;
        writer.write_all(output.as_bytes())?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        return Ok(output);
    }
    if prompt.trim().is_empty() {
        bail!("print mode requires a prompt");
    }
    crate::human_event_renderer::run_human_turn_to(application, prompt, writer, ansi).await
}

/// Run a single prompt in print mode, streaming to stdout.
pub async fn run_print(application: &Application, prompt: &str) -> Result<String> {
    use std::io::IsTerminal as _;

    let mut stdout = std::io::stdout();
    let ansi = stdout.is_terminal();
    run_print_to(application, prompt, &mut stdout, ansi).await
}

/// `pi --print` / non-interactive text entrypoint.
pub async fn print_mode(cli: &Cli) -> Result<()> {
    let prompts = cli
        .prompt
        .iter()
        .filter(|prompt| !prompt.is_empty())
        .collect::<Vec<_>>();
    if prompts.is_empty() {
        bail!("print mode requires a prompt");
    }
    let RunSession { application, .. } = build_session(cli).await?;
    let result = async {
        for prompt in prompts {
            run_print(&application, prompt).await?;
        }
        Ok(())
    }
    .await;
    application.cleanup().await;
    result
}

fn extension_startup_error(report: &pi_coding::ExtensionLoadReport) -> anyhow::Error {
    anyhow!(
        "extension startup rejected {} extension(s): {}",
        report.failures.len(),
        report
            .failures
            .iter()
            .map(|failure| format!(
                "{} ({}): {}",
                failure.extension_id,
                failure.path.display(),
                failure.message
            ))
            .collect::<Vec<_>>()
            .join("; ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use parking_lot::Mutex;
    use pi_agent::ThinkingLevel;
    use pi_ai::{Model, SimpleStreamOptions};
    use pi_coding::{
        AgentDefinition, AgentDefinitionSource, ChildSessionFactory, ResourceSnapshot, Settings,
        TaskItem, TrustDecision, TrustResolution,
    };

    #[test]
    fn settings_and_cli_selection_drive_identical_initial_tool_gates() {
        let mut settings = Settings::default();
        settings.orchestration = Some(pi_coding::OrchestrationSettings {
            tasks: Some(true),
            process: Some(true),
            todo: Some(true),
            glob: Some(true),
            ..pi_coding::OrchestrationSettings::default()
        });
        let cli = <Cli as clap::Parser>::try_parse_from(["pi"]).expect("default cli");
        let gates = main_tool_gates(&settings, &cli);
        assert!(gates.orchestration);
        assert!(gates.process);
        assert!(gates.todo);
        assert!(gates.glob);
        assert!(!gates.goal);

        let selected = <Cli as clap::Parser>::try_parse_from([
            "pi",
            "--tools",
            "task,hub,process,todo,glob,goal",
        ])
        .expect("selected tools");
        let gates = main_tool_gates(&Settings::default(), &selected);
        assert!(gates.orchestration);
        assert!(gates.process);
        assert!(gates.todo);
        assert!(gates.glob);
        assert!(gates.goal);

        let disabled = <Cli as clap::Parser>::try_parse_from(["pi", "--no-tools"])
            .expect("disabled tools");
        let gates = main_tool_gates(&settings, &disabled);
        assert!(!gates.orchestration);
        assert!(!gates.process);
        assert!(!gates.todo);
        assert!(!gates.glob);
        assert!(!gates.goal);
    }

    #[test]
    fn settings_thinking_level_is_used_after_cli_model_and_resume_priorities() {
        assert_eq!(
            resolve_initial_thinking_level(None, "", None, false, Some("high")),
            pi_agent::ThinkingLevel::High
        );
        assert_eq!(
            resolve_initial_thinking_level(Some("low"), "", None, false, Some("high")),
            pi_agent::ThinkingLevel::Low
        );
    }

    #[tokio::test]
    async fn initial_cli_orchestration_inherits_selected_parent_model() {
        let artifacts = tempfile::tempdir().expect("artifacts root");
        let parent_model = Model {
            id: "selected-parent".to_owned(),
            name: "Selected Parent".to_owned(),
            api: "test-api".to_owned(),
            provider: "test-provider".to_owned(),
            ..Model::default()
        };
        let bundled = AgentDefinition {
            name: "task".to_owned(),
            description: "bundled task agent".to_owned(),
            system_prompt: "do the task".to_owned(),
            tools: Some(Vec::new()),
            autoload_skills: Vec::new(),
            // No definition model list — must fall through to parent.
            model: None,
            thinking_level: Some(ThinkingLevel::Off),
            source: AgentDefinitionSource::Bundled,
            path: None,
            trusted: true,
        };
        let snapshot = ResourceSnapshot {
            generation: 1,
            cwd: artifacts.path().to_path_buf(),
            trust: TrustResolution {
                decision: TrustDecision::Trusted,
                matched_path: None,
                project_path: artifacts.path().to_path_buf(),
            },
            settings: Settings::default(),
            context_files: Vec::new(),
            skills: Vec::new(),
            agents: vec![bundled],
            prompts: Vec::new(),
            themes: Vec::new(),
            package_extensions: Vec::new(),
            theme_dirs: Vec::new(),
            keybinding_files: Vec::new(),
            system_prompt: None,
            append_system_prompt: Vec::new(),
            diagnostics: Vec::new(),
        };
        // Empty settings.agents → no per-agent model override.
        let settings = Settings::default();
        let config = orchestration_config(&snapshot, &settings, &parent_model);
        assert_eq!(config.parent_model.provider, parent_model.provider);
        assert_eq!(config.parent_model.id, parent_model.id);

        let seen = Arc::new(Mutex::new(None::<Model>));
        let seen_factory = seen.clone();
        let factory: ChildSessionFactory = Arc::new(move |request| {
            let seen_factory = seen_factory.clone();
            Box::pin(async move {
                *seen_factory.lock() = Some(request.model.clone());
                // Debug must never surface secret material from the request.
                let debug = format!("{request:?}");
                assert!(
                    !debug.contains("sk-") && !debug.to_ascii_lowercase().contains("api_key"),
                    "ChildSessionRequest debug leaked secrets: {debug}"
                );
                Session::new(SessionOptions {
                    model: request.model,
                    cwd: std::env::current_dir().expect("cwd"),
                    system_prompt: request.system_prompt,
                    thinking_level: request.thinking_level.unwrap_or(ThinkingLevel::Off),
                    api_key: String::new(),
                    compaction: None,
                    stream_options: SimpleStreamOptions::default(),
                    tools: Some(Vec::new()),
                    before_tool_call: None,
                    after_tool_call: None,
                    stream_fn: None,
                    auth_resolver: None,
                })
            })
        });

        let runtime = OrchestrationRuntime::new(config, factory).expect("runtime");
        let _ = runtime
            .spawn_tasks(
                "Main",
                0,
                vec![TaskItem {
                    index: 0,
                    id: "Child".to_owned(),
                    agent: "task".to_owned(),
                    assignment: "inherit parent model".to_owned(),
                }],
            )
            .expect("spawn");
        for _ in 0..50 {
            if seen.lock().is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let captured = seen
            .lock()
            .clone()
            .expect("factory observed ChildSessionRequest.model");
        assert_eq!(captured.provider, parent_model.provider);
        assert_eq!(captured.id, parent_model.id);
        runtime.shutdown().await;
    }
}
