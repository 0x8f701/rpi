use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use pi_coding::{
    AgentCatalog, ApplicationRuntimeCandidate, ApplicationRuntimeFactory, ApplicationRuntimeFuture,
    ExtensionMode, ExtensionPermissionSet, ExtensionRuntime, ExtensionRuntimeOptions,
    ExtensionUiHost, GoalToolBinding, InternalUriResolver, OrchestrationConfig,
    OrchestrationRuntime, OrchestrationSkill, ResourceManager, ResourceManagerOptions, Session,
    SessionOptions, TodoChildToolBinding, ToolSelection, WorkspaceRoots,
};

use crate::args::Cli;
use crate::extension_ui::{ExtensionUiAdapter, NonInteractiveExtensionUiHost};
use crate::models_config;
use crate::session_run::SessionStorage;
use crate::output::parse_thinking_level;

/// Default `orchestration.maxToolsPerAgent` for a workflow child when the
/// setting is unset. Covers the dynamic factory tool set (19 default coding
/// tools + 3 hindsight memory tools = 22 max supported) so a supervisor's
/// default worker spawn never trips the cap. Inherited `tools: None` child
/// sets are exempt from the cap at spawn; this raised default still covers
/// explicit factory-sized lists. Explicit caps override this and stay
/// fail-closed. Keep in sync with `DEFAULT_CHILD_CODING_TOOLS` +
/// `memory_tools_for` (hindsight backend).
const WORKFLOW_DEFAULT_CHILD_TOOL_CAP: usize = 22;

#[derive(Clone)]
pub(super) struct RunSessionBlueprint {
    add_dirs: Vec<PathBuf>,
    resource_options: ResourceManagerOptions,
    system_prompt_input: Option<String>,
    append_system_prompt_inputs: Vec<String>,
    tool_policy: MainToolPolicy,
    extension_mode: ExtensionMode,
    extension_ui: Option<ExtensionUiAdapter>,
    approval_mode_override: Option<pi_agent::ApprovalMode>,
    extension_permissions: ExtensionPermissionSet,
    explicit_api_key: Option<String>,
    session_storage: Option<SessionStorage>,
    /// ACP approval bridge (agent mode). When set, sessions built from this
    /// blueprint route host tool approval through ACP `session/request_permission`
    /// reverse requests instead of the extension-UI confirmation adapter.
    acp_approval: Option<crate::modes::acp::AcpApprovalFactory>,
}

#[derive(Clone)]
struct MainToolPolicy {
    allow: Option<Vec<String>>,
    deny: Vec<String>,
    disable_all: bool,
    disable_builtins: bool,
    orchestration: bool,
}

pub(super) struct RunSessionCandidate {
    pub(super) session: Session,
    pub(super) extension_runtime: ExtensionRuntime,
    pub(super) extension_permissions: ExtensionPermissionSet,
    pub(super) orchestration: Option<OrchestrationRuntime>,
    pub(super) goal_tool: Option<GoalToolBinding>,
    pub(super) todo_child_tool: Option<TodoChildToolBinding>,
}
impl RunSessionCandidate {
    pub(super) async fn restore_resume(
        self,
        context: pi_coding::BranchContext,
        recorder: pi_coding::SessionRecorder,
        session_name: Option<String>,
        resolved_model: Option<pi_ai::Model>,
        thinking_level: pi_agent::ThinkingLevel,
    ) -> Result<Self> {
        let Self {
            session,
            extension_runtime,
            extension_permissions,
            orchestration,
            goal_tool,
            todo_child_tool,
        } = self;

        if let Some(model) = resolved_model.or_else(|| {
            context
                .provider
                .as_deref()
                .zip(context.model_id.as_deref())
                .and_then(|(provider, model_id)| pi_ai::get_model(provider, model_id))
        }) && session.model().as_ref() != Some(&model)
        {
            session.set_model_with_resolved_auth(model).await?;
        }
        session.set_thinking_level(thinking_level);
        session.resume_recording(recorder, context.messages).await?;
        if let Some(name) = session_name.as_deref() {
            session.set_session_name(name)?;
        }
        Ok(Self {
            session,
            extension_runtime,
            extension_permissions,
            orchestration,
            goal_tool,
            todo_child_tool,
        })
    }
}

impl RunSessionBlueprint {
    pub(super) fn from_cli(
        cli: &Cli,
        resource_options: ResourceManagerOptions,
        extension_ui: Option<ExtensionUiAdapter>,
    ) -> Self {
        Self {
            add_dirs: cli.add_dirs.clone(),
            resource_options,
            system_prompt_input: cli.system.clone(),
            append_system_prompt_inputs: cli.append_system_prompt.clone(),
            tool_policy: MainToolPolicy {
                allow: cli.tools.clone(),
                deny: cli.exclude_tools.clone(),
                disable_all: cli.no_tools,
                disable_builtins: cli.no_builtin_tools,
                orchestration: true,
            },
            extension_mode: crate::session_run::extension_mode(cli),
            extension_ui,
            approval_mode_override: cli.approval_mode.map(Into::into),
            extension_permissions: ExtensionPermissionSet::allow_all(),
            explicit_api_key: cli.api_key.clone(),
            session_storage: None,
            acp_approval: None,
        }
    }

    pub(super) fn extension_ui(&self) -> Option<ExtensionUiAdapter> {
        self.extension_ui.clone()
    }

    /// Override the extension UI mode (e.g. `ExtensionMode::Json` for the
    /// headless ACP agent mode, where the CLI-mode inference would otherwise
    /// pick the interactive TUI).
    pub(super) fn set_extension_mode(&mut self, mode: ExtensionMode) {
        self.extension_mode = mode;
    }

    /// Route host tool approval through the ACP reverse-request bridge for
    /// every session built from this blueprint (agent mode).
    pub(super) fn set_acp_approval(&mut self, factory: crate::modes::acp::AcpApprovalFactory) {
        self.acp_approval = Some(factory);
    }

    fn approval_mode(&self, settings: &pi_coding::Settings) -> pi_agent::ApprovalMode {
        self.approval_mode_override
            .unwrap_or_else(|| settings.approval_mode())
    }

    pub(super) fn set_session_storage(&mut self, storage: SessionStorage) {
        self.session_storage = Some(storage);
    }

    pub(super) async fn build(
        &self,
        cwd: &Path,
        options: SessionOptions,
    ) -> Result<RunSessionCandidate> {
        let workspace = WorkspaceRoots::new(cwd, &self.add_dirs)?;
        self.build_with_workspace(workspace, options).await
    }

    pub(super) async fn build_with_workspace(
        &self,
        workspace: WorkspaceRoots,
        options: SessionOptions,
    ) -> Result<RunSessionCandidate> {
        let cwd = workspace.cwd().to_path_buf();
        let resources = ResourceManager::new_for_workspace(
            self.resource_options_for_startup(&cwd)?,
            &workspace,
        )
        .context("loading settings and resources")?;
        let settings = resources.snapshot().settings.clone();
        let permissions = self.extension_permissions.clone();
        let runtime = self.extension_runtime();
        resources
            .extension_specs(&permissions)
            .context("validating configured extensions")?;

        let build_result = self
            .build_session(
                cwd,
                workspace,
                resources.clone(),
                settings.clone(),
                runtime.clone(),
                options,
                false,
            )
            .await;
        let (session, orchestration, goal_tool, todo_child_tool) = match build_result {
            Ok(candidate) => candidate,
            Err(error) => {
                runtime.shutdown().await;
                return Err(error);
            }
        };
        Ok(RunSessionCandidate {
            session,
            extension_runtime: runtime,
            extension_permissions: permissions,
            orchestration,
            goal_tool,
            todo_child_tool,
        })
    }
    pub(super) async fn build_without_orchestration(
        &self,
        cwd: &Path,
        options: SessionOptions,
    ) -> Result<RunSessionCandidate> {
        let mut blueprint = self.clone();
        blueprint.tool_policy.orchestration = false;
        blueprint.build(cwd, options).await
    }

    async fn build_workflow(
        &self,
        cwd: &Path,
        options: SessionOptions,
    ) -> Result<RunSessionCandidate> {
        let workspace = WorkspaceRoots::new(cwd, &self.add_dirs)?;
        let cwd = workspace.cwd().to_path_buf();
        let resources = ResourceManager::new_for_workspace(
            self.resource_options_for_startup(&cwd)?,
            &workspace,
        )
        .context("loading settings and resources")?;
        let settings = resources.snapshot().settings.clone();
        let permissions = self.extension_permissions.clone();
        let runtime = self.extension_runtime();
        resources
            .extension_specs(&permissions)
            .context("validating configured extensions")?;
        let built = self
            .build_session(
                cwd,
                workspace,
                resources,
                settings,
                runtime.clone(),
                options,
                true,
            )
            .await;
        let (session, orchestration, goal_tool, todo_child_tool) = match built {
            Ok(candidate) => candidate,
            Err(error) => {
                runtime.shutdown().await;
                return Err(error);
            }
        };
        Ok(RunSessionCandidate {
            session,
            extension_runtime: runtime,
            extension_permissions: permissions,
            orchestration,
            goal_tool,
            todo_child_tool,
        })
    }

    pub(super) fn resource_options_for_startup(
        &self,
        cwd: &Path,
    ) -> Result<ResourceManagerOptions> {
        let mut options = self.resource_options.clone();
        options.cwd = cwd.to_path_buf();
        options.system_prompt = None;
        options.system_prompt_path = None;
        options.system_prompt_source = None;
        options.append_system_prompt.clear();
        options.append_system_prompt_paths.clear();
        options.append_system_prompt_sources.clear();
        if let Some(input) = self.system_prompt_input.as_deref() {
            let (prompt, path) =
                crate::session_run::resolve_prompt_input(input, cwd, "system prompt")?;
            options.system_prompt = Some(prompt);
            options.system_prompt_source = path;
        }
        for input in &self.append_system_prompt_inputs {
            let (prompt, path) =
                crate::session_run::resolve_prompt_input(input, cwd, "append system prompt")?;
            options.append_system_prompt.push(prompt);
            options.append_system_prompt_paths.push(None);
            options.append_system_prompt_sources.push(path);
        }
        Ok(options)
    }

    fn extension_runtime(&self) -> ExtensionRuntime {
        let ui_host: Option<Arc<dyn ExtensionUiHost>> = self
            .extension_ui
            .as_ref()
            .map(|adapter| Arc::new(adapter.clone()) as Arc<dyn ExtensionUiHost>)
            .or_else(|| {
                Some(Arc::new(NonInteractiveExtensionUiHost::default()) as Arc<dyn ExtensionUiHost>)
            });
        ExtensionRuntime::process(
            ui_host,
            ExtensionRuntimeOptions {
                mode: self.extension_mode,
                ..ExtensionRuntimeOptions::default()
            },
        )
    }

    async fn build_session(
        &self,
        cwd: PathBuf,
        workspace: WorkspaceRoots,
        resources: ResourceManager,
        settings: pi_coding::Settings,
        runtime: ExtensionRuntime,
        mut options: SessionOptions,
        force_workflow_runtime: bool,
    ) -> Result<(
        Session,
        Option<OrchestrationRuntime>,
        Option<GoalToolBinding>,
        Option<TodoChildToolBinding>,
    )> {
        let session_dir = self
            .session_storage
            .as_ref()
            .map(|storage| storage.session_dir_for(&cwd))
            .transpose()?;
        options.cwd.clone_from(&cwd);
        options.system_prompt = resources
            .snapshot()
            .system_prompt
            .clone()
            .unwrap_or_default();
        let auth_manager = options
            .auth_manager
            .clone()
            .ok_or_else(|| anyhow!("session authentication owner is unavailable"))?;
        options.auth_resolver = Some(models_config::session_auth_resolver(
            self.explicit_api_key.clone(),
            auth_manager,
        ));
        settings.apply_session_options(&mut options)?;
        let approval_mode = self.approval_mode(&settings);
        // Validate that the generation's settings manager remains available;
        // session construction must surface a typed settings error rather than
        // silently dropping path policy.
        resources
            .settings_manager()
            .context("loading permission rules")?;
        // Read the published generation on every tool call so successful reloads
        // take effect without a session restart. A failed reload never publishes.
        let permission_rules: crate::approval::PermissionRulesSource = {
            let resources = resources.clone();
            Arc::new(move || resources.snapshot().settings.permission_rules.clone())
        };
        // Keep a clone for orchestration children: they inherit the same live
        // source so their lsp rename preflight obeys current permissionRules.
        let child_permission_rules = permission_rules.clone();
        let approval = match &self.acp_approval {
            Some(factory) => crate::modes::acp::acp_approval_before_tool_call(
                approval_mode,
                factory.clone(),
                None,
                cwd.clone(),
                permission_rules,
            ),
            None => crate::approval::host_approval_before_tool_call(
                approval_mode,
                self.extension_mode,
                self.extension_ui.clone(),
                None,
                cwd.clone(),
                permission_rules,
            ),
        };
        let mut gates = self.tool_gates(&settings);
        // Workflow planner/runtime children need the internal Todo DAG tool
        // even when interactive `orchestration.todo` is false. Interactive
        // Main stays on `tool_gates` (setting + CLI request).
        if force_workflow_runtime {
            // An explicit `--no-tools` or `--exclude-tools todo` request
            // would strip the internal Todo tool the workflow planner
            // depends on. Fail construction naming the exact flag instead
            // of silently overriding the explicit deny (the allow-list
            // omission case below is still overridden: Todo is internal
            // plumbing, not a user tool).
            if self.tool_policy.disable_all {
                return Err(anyhow!(
                    "workflow runtime construction requires the internal Todo tool, but --no-tools disables all tools"
                ));
            }
            if self.tool_policy.deny.iter().any(|name| name == "todo") {
                return Err(anyhow!(
                    "workflow runtime construction requires the internal Todo tool, but --exclude-tools excludes todo"
                ));
            }
            gates.orchestration = true;
            gates.todo = true;
        }
        let (orchestration, uri_resolver) = if gates.orchestration {
            let snapshot = resources.snapshot();
            let config =
                orchestration_config(&snapshot, &settings, &options.model, force_workflow_runtime);
            let (uri_resolver, resolver_binding) = InternalUriResolver::deferred();
            let factory = OrchestrationRuntime::child_factory_from_snapshot_and_uri(
                pi_coding::ChildSessionOptionsSnapshot {
                    model: options.model.clone(),
                    workspace: workspace.clone(),
                    thinking_level: options.thinking_level,
                    api_key: options.api_key.clone(),
                    stream_options: options.stream_options.clone(),
                    stream_fn: options
                        .stream_fn
                        .clone()
                        .unwrap_or_else(|| pi_agent::AgentOptions::default().stream_fn),
                    auth_resolver: options.auth_resolver.clone(),
                    auth_manager: options.auth_manager.clone(),
                    retry_settings: settings.retry_settings(),
                    retry_role: None,
                    generation_capture: resources.child_generation_capture(),
                    memory: {
                        let resources = resources.clone();
                        let resolver: pi_coding::MemoryConfigFn =
                            Arc::new(move || Some(resources.snapshot().settings.memory_config()));
                        Some(resolver)
                    },
                    // The same live source the host approval hook consults, so
                    // child lsp rename preflight obeys current permissionRules.
                    permission_rules: Some(child_permission_rules.clone()),
                    context_files_resolver: {
                        let resources = resources.clone();
                        let resolver: pi_coding::ContextFilesFn =
                            Arc::new(move || resources.snapshot().context_files.clone());
                        Some(resolver)
                    },
                },
                Some(uri_resolver.clone()),
            );
            let orchestration = OrchestrationRuntime::new(config, factory)?;
            resolver_binding.bind(orchestration.read_uri_resolver())?;
            (Some(orchestration), Some(uri_resolver))
        } else {
            (None, None)
        };

        // Extension, orchestration (`task`/`hub`), Todo-child, and goal tools
        // are application-generation-owned: they join the construction-time
        // generation channel and the `Application` re-provisions them every
        // generation — never the persistent base, or merges hit duplicates.
        let mut generation_tools = runtime.agent_tools();
        if let Some(orchestration) = &orchestration {
            generation_tools.extend(orchestration.agent_tools("Main", 0));
        }
        let todo_child_tool = (!self.tool_policy.disable_all).then(TodoChildToolBinding::default);
        if let Some(binding) = &todo_child_tool {
            generation_tools.extend(binding.tools());
        }
        let goal_tool = (!self.tool_policy.disable_all).then(GoalToolBinding::default);
        if let Some(binding) = &goal_tool {
            generation_tools.push(binding.tool());
        }
        let mut allow = self.tool_policy.allow.clone();
        // Workflow planner Todo is internal plumbing: keep it on the child
        // even when the parent CLI `--tools` allow-list omitted `todo`.
        // Explicit `--no-tools` / `--exclude-tools todo` already failed
        // construction at the guard above; only the allow-list omission
        // is overridden here.
        if force_workflow_runtime
            && gates.todo
            && let Some(names) = allow.as_mut()
            && !names.iter().any(|name| name == "todo")
        {
            names.push("todo".to_owned());
        }
        let selection = ToolSelection {
            allow,
            deny: self.tool_policy.deny.clone(),
            disable_all: self.tool_policy.disable_all,
            disable_builtins: self.tool_policy.disable_builtins,
            enable_process: gates.process,
            enable_glob: gates.glob,
        };
        // Persistent caller tools stay empty; see the ownership note above.
        let session = if gates.todo {
            Session::new_with_persistent_and_generation_tools_filtered_discovery_workspace_and_uri(
                options,
                Vec::new(),
                generation_tools,
                selection,
                pi_coding::ResourceDiscovery::Disabled,
                workspace,
                uri_resolver,
                true,
                force_workflow_runtime,
            )
        } else {
            Session::new_with_persistent_and_generation_tools_filtered_discovery_workspace_and_uri(
                options,
                Vec::new(),
                generation_tools,
                selection,
                pi_coding::ResourceDiscovery::Disabled,
                workspace,
                uri_resolver,
                false,
                false,
            )
        };
        let session = match session {
            Ok(session) => session,
            Err(error) => {
                if let Some(orchestration) = orchestration {
                    orchestration.shutdown().await;
                }
                return Err(error).context("building session");
            }
        };
        session.set_approve_tool_call(Some(approval));
        // Offline contract: `--offline` / process `RPI_OFFLINE` (resolved by
        // `session_run::offline`) must fail network-touching session
        // operations (e.g. the gist share path) closed on every
        // blueprint-built session — fresh and resumed alike.
        session.set_offline(crate::session_run::offline());
        if let Some(session_dir) = session_dir {
            session.set_session_dir(session_dir);
        }
        session.set_steering_mode(settings.steering_mode()).await;
        session.set_follow_up_mode(settings.follow_up_mode()).await;
        session.set_retry_settings(settings.retry_settings());
        if let Err(error) = session.attach_resources(resources).await {
            if let Some(orchestration) = orchestration {
                orchestration.shutdown().await;
            }
            return Err(error).context("attaching settings and resources");
        }
        Ok((
            session,
            orchestration,
            gates.goal.then_some(goal_tool).flatten(),
            todo_child_tool,
        ))
    }

    /// Interactive Main tool gates. `todo` follows `settings.todo_tool_enabled()`
    /// (default on; `orchestration.todo: false` opts out) unless the CLI
    /// allow-list names `todo`. Workflow children do not use this result
    /// unmodified: `build_session(force_workflow_runtime)` force-enables
    /// orchestration and the internal Todo tool.
    fn tool_gates(&self, settings: &pi_coding::Settings) -> MainToolGates {
        let tools_allowed = !self.tool_policy.disable_all;
        let requested = |name: &str| {
            self.tool_policy
                .allow
                .as_ref()
                .is_some_and(|tools| tools.iter().any(|tool| tool == name))
        };
        MainToolGates {
            orchestration: tools_allowed
                && self.tool_policy.orchestration
                && (settings.orchestration_enabled() || requested("task") || requested("hub")),

            process: tools_allowed && (settings.process_tool_enabled() || requested("process")),
            glob: tools_allowed && (settings.glob_tool_enabled() || requested("glob")),
            todo: tools_allowed && (settings.todo_tool_enabled() || requested("todo")),
            goal: tools_allowed
                && self
                    .tool_policy
                    .allow
                    .as_ref()
                    .is_none_or(|tools| tools.iter().any(|tool| tool == "goal"))
                && !self.tool_policy.deny.iter().any(|tool| tool == "goal"),
        }
    }
}

impl ApplicationRuntimeFactory for RunSessionBlueprint {
    fn build_runtime_candidate(
        &self,
        cwd: PathBuf,
        options: SessionOptions,
        resume: Option<pi_coding::PreparedSessionResume>,
    ) -> ApplicationRuntimeFuture {
        let blueprint = self.clone();
        Box::pin(async move {
            let candidate = blueprint.build(&cwd, options).await?;
            let RunSessionCandidate {
                session,
                extension_runtime,
                extension_permissions,
                orchestration,
                goal_tool,
                todo_child_tool,
            } = candidate;
            if let Some(resume) = resume {
                let context = resume.build_context();
                let resume_has_thinking_entry = resume.tree().has_thinking_entry();
                let recorder = resume.into_recorder()?;

                if let Some(provider) = context.provider.as_deref()
                    && let Some(model_id) = context.model_id.as_deref()
                    && let Some(model) = pi_ai::get_model(provider, model_id)
                {
                    session.set_model_with_resolved_auth(model).await?;
                }
                if resume_has_thinking_entry {
                    session.set_thinking_level(parse_thinking_level(&context.thinking_level));
                }
                session.resume_recording(recorder, context.messages).await?;
            } else {
                session.start_new_recording()?;
            }
            let mut runtime = ApplicationRuntimeCandidate::new(session)
                .with_extensions(extension_runtime, extension_permissions);
            if let Some(orchestration) = orchestration {
                runtime = runtime.with_orchestration(orchestration);
            }
            if let Some(goal_tool) = goal_tool {
                runtime = runtime.with_goal_tool(goal_tool);
            }
            if let Some(todo_child_tool) = todo_child_tool {
                runtime = runtime.with_todo_child_tool(todo_child_tool);
            }
            Ok(runtime)
        })
    }

    fn build_trusted_workflow_candidate(
        &self,
        cwd: pi_coding::workflow_worktree::TrustedWorkflowCwd,
        options: SessionOptions,
    ) -> ApplicationRuntimeFuture {
        let mut blueprint = self.clone();
        blueprint.resource_options.project_trust_override = Some(true);
        let path = cwd.path().to_path_buf();
        Box::pin(async move {
            let candidate = blueprint.build_workflow(&path, options).await?;
            let RunSessionCandidate {
                session,
                extension_runtime,
                extension_permissions,
                orchestration,
                goal_tool,
                todo_child_tool,
            } = candidate;
            // Every trusted workflow candidate is a supervisor/planner/worktree
            // runtime: provenance keeps it out of user session catalogs.
            session
                .start_new_recording_with_kind(pi_coding::SessionKind::WorkflowSupervisor)?;
            let mut runtime = ApplicationRuntimeCandidate::new(session)
                .with_extensions(extension_runtime, extension_permissions);
            if let Some(orchestration) = orchestration {
                runtime = runtime.with_orchestration(orchestration);
            }
            if let Some(goal_tool) = goal_tool {
                runtime = runtime.with_goal_tool(goal_tool);
            }
            if let Some(todo_child_tool) = todo_child_tool {
                runtime = runtime.with_todo_child_tool(todo_child_tool);
            }
            Ok(runtime)
        })
    }
}
#[cfg(test)]
impl RunSessionBlueprint {
    pub(super) fn test_tool_gates(
        &self,
        settings: &pi_coding::Settings,
    ) -> (bool, bool, bool, bool, bool) {
        let gates = self.tool_gates(settings);
        (
            gates.orchestration,
            gates.process,
            gates.glob,
            gates.todo,
            gates.goal,
        )
    }
    pub(super) fn test_approval_mode(
        &self,
        settings: &pi_coding::Settings,
    ) -> pi_agent::ApprovalMode {
        self.approval_mode(settings)
    }
}

#[cfg(test)]
pub(crate) fn test_orchestration_config(
    snapshot: &pi_coding::ResourceSnapshot,
    settings: &pi_coding::Settings,
    parent_model: &pi_ai::Model,
) -> OrchestrationConfig {
    orchestration_config(snapshot, settings, parent_model, false)
}

#[cfg(test)]
pub(crate) fn test_workflow_orchestration_config(
    snapshot: &pi_coding::ResourceSnapshot,
    settings: &pi_coding::Settings,
    parent_model: &pi_ai::Model,
) -> OrchestrationConfig {
    orchestration_config(snapshot, settings, parent_model, true)
}

struct MainToolGates {
    orchestration: bool,
    process: bool,
    glob: bool,
    todo: bool,
    goal: bool,
}

fn orchestration_config(
    snapshot: &pi_coding::ResourceSnapshot,
    settings: &pi_coding::Settings,
    parent_model: &pi_ai::Model,
    is_workflow: bool,
) -> OrchestrationConfig {
    let artifact_dir = if is_workflow {
        pi_coding::agent_dir_path().join("workflow-artifacts")
    } else {
        snapshot.cwd.join(".rpi").join("artifacts")
    };
    let mut config = OrchestrationConfig::new(
        AgentCatalog::from_agents(snapshot.agents.clone()),
        artifact_dir,
    );
    config.skills = snapshot
        .skills
        .iter()
        .map(OrchestrationSkill::from)
        .collect();
    let mut workflow_tool_cap_explicit = false;
    if let Some(orchestration) = &settings.orchestration {
        if let Some(value) = orchestration.max_concurrency {
            config.max_concurrency = value;
        }
        if let Some(value) = orchestration.max_recursion_depth {
            config.max_recursion_depth = value;
        }
        if let Some(value) = orchestration.mailbox_capacity {
            config.mailbox_capacity = value;
        }
        if let Some(value) = orchestration.max_tools_per_agent {
            // Explicit caps stay fail-closed: a child resolving more tools
            // than the configured cap errors at spawn with an actionable
            // `child_tool_cap_error`.
            config.max_tools_per_agent = value;
            workflow_tool_cap_explicit = true;
        }
        if let Some(value) = orchestration.soft_budget.as_ref() {
            config.soft_budget = pi_coding::JobSoftBudget {
                max_requests: value.max_requests,
                max_tokens: value.max_tokens,
                yield_after: value.yield_after,
            };
        }
        if let Some(value) = orchestration.preferred_agent.as_ref() {
            // Missing/disabled selections are ignored at spawn time; the runtime
            // falls back to ranked/default agent selection when the name is not
            // an enabled catalog entry.
            config.preferred_agent = Some(value.clone());
        }
    }
    // Unset workflow child capacity must cover the actual dynamic factory tool
    // set (19 default coding tools plus the largest memory backend = 3 hindsight
    // tools, max supported 22) so the supervisor's default worker spawn never
    // trips the cap. Applies whether or not `settings.orchestration` exists, so
    // a fully-default workflow child (no orchestration settings at all) still
    // raises the cap. Only the workflow child path (build_workflow) gets the
    // raised default; the parent orchestration keeps the legacy cap so the
    // separate async cap failure for non-workflow children is unchanged.
    if is_workflow && !workflow_tool_cap_explicit {
        config.max_tools_per_agent = WORKFLOW_DEFAULT_CHILD_TOOL_CAP;
    }
    config = config.with_selector_settings(settings.selector.clone().unwrap_or_default());
    config.agent_settings = settings.agents.clone();
    config.parent_model = parent_model.clone();
    config
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;
    use pi_agent::{AbortController, AgentToolResult, ThinkingLevel, ToolCallContext};
    use pi_ai::{
        ContentBlock, Model, SimpleStreamOptions, StopReason, ToolCall,
        providers::{FauxProviderRegistration, FauxResponse, register_faux_provider},
    };
    use serde_json::json;
    use std::fs;

    fn faux_model(label: &str) -> (Model, FauxProviderRegistration) {
        let suffix = uuid::Uuid::now_v7().simple();
        let mut model = Model::default();
        model.id = label.to_owned();
        model.name = label.to_owned();
        model.api = format!("{label}-{suffix}-api");
        model.provider = format!("{label}-{suffix}-provider");
        model.base_url = "http://localhost:0".to_owned();
        let registration = register_faux_provider(pi_ai::providers::FauxProviderOptions {
            api: model.api.clone(),
            provider: model.provider.clone(),
            models: vec![model.clone()],
            chunk_size: 12,
        });
        (model, registration)
    }

    fn isolated_resource_options(cwd: &Path, agent_dir: &Path) -> ResourceManagerOptions {
        let mut options = ResourceManagerOptions::new(cwd);
        options.agent_dir = agent_dir.to_path_buf();
        options.project_trust_override = Some(false);
        options.disable_extensions = true;
        options.disable_skills = true;
        options.disable_prompt_templates = true;
        options.disable_themes = true;
        options.disable_context_files = true;
        options
    }

    fn write_unconfined_sandbox(agent_dir: &Path) {
        fs::write(
            agent_dir.join("settings.json"),
            r#"{"sandbox":{"mode":"danger-full-access"}}"#,
        )
        .expect("unconfined fixture sandbox settings");
    }
    fn options(cwd: &Path, model: &Model, agent_dir: &Path) -> SessionOptions {
        SessionOptions {
            model: model.clone(),
            cwd: cwd.to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::High,
            api_key: "launch-secret".to_owned(),
            compaction: Some(pi_coding::DEFAULT_COMPACTION_SETTINGS),
            stream_options: SimpleStreamOptions::default(),
            tools: None,
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
            auth_manager: Some(Arc::new(
                pi_coding::AuthManager::new(agent_dir.join("auth.json")).expect("auth manager"),
            )),
        }
    }

    struct OwnedCandidate {
        session: Session,
        orchestration: Option<OrchestrationRuntime>,
        _session_dir: tempfile::TempDir,
        application: pi_coding::Application,
    }

    async fn try_own_candidate(candidate: RunSessionCandidate) -> Result<OwnedCandidate> {
        let RunSessionCandidate {
            session,
            extension_runtime,
            extension_permissions,
            orchestration,
            goal_tool,
            todo_child_tool,
        } = candidate;
        let session_root = tempfile::tempdir().expect("candidate session root");
        let session_dir = session_root.path().join("sessions");
        fs::create_dir_all(&session_dir).expect("candidate session dir");
        session.set_session_dir(session_dir);
        session.start_new_recording().expect("candidate recording");
        let inspected_session = session.clone();
        let inspected_orchestration = orchestration.clone();
        let mut runtime = ApplicationRuntimeCandidate::new(session)
            .with_extensions(extension_runtime, extension_permissions);
        if let Some(orchestration) = orchestration {
            runtime = runtime.with_orchestration(orchestration);
        }
        if let Some(goal_tool) = goal_tool {
            runtime = runtime.with_goal_tool(goal_tool);
        }
        if let Some(todo_child_tool) = todo_child_tool {
            runtime = runtime.with_todo_child_tool(todo_child_tool);
        }
        let application = pi_coding::Application::from_runtime_candidate(runtime).await?;
        Ok(OwnedCandidate {
            session: inspected_session,
            orchestration: inspected_orchestration,
            _session_dir: session_root,
            application,
        })
    }

    async fn own_candidate(candidate: RunSessionCandidate) -> OwnedCandidate {
        try_own_candidate(candidate)
            .await
            .expect("candidate application")
    }

    fn write_project(root: &Path, label: &str, max_tokens: i64) {
        let project = root.join(".rpi");
        fs::create_dir_all(project.join("agents")).expect("agents directory");
        fs::create_dir_all(project.join("skills")).expect("skills directory");
        fs::write(
            project.join("settings.json"),
            format!(
                r#"{{"maxTokens":{max_tokens},"orchestration":{{"tasks":true,"process":true,"glob":true}}}}"#,
            ),
        )
        .expect("settings");
        fs::write(root.join("AGENTS.md"), format!("{label} project context")).expect("context");
        fs::write(
            project.join("agents").join(format!("{label}.md")),
            format!(
                "---\nname: {label}\ndescription: {label} project agent\n---\n{label} agent prompt\n"
            ),
        )
        .expect("agent");
        fs::write(
            project.join("skills").join(format!("{label}.md")),
            format!(
                "---\nname: {label}-skill\ndescription: {label} project skill\n---\n# {label} skill\n"
            ),
        )
        .expect("skill");
    }

    async fn read_tool(tool: &pi_agent::AgentTool, path: &str) -> anyhow::Result<String> {
        let (controller, abort) = AbortController::new();
        let result = (tool.execute)(ToolCallContext {
            tool_call_id: "blueprint-read".to_owned(),
            arguments: json!({ "path": path }),
            on_update: Arc::new(|_: AgentToolResult| {}),
            abort,
            model: None,
            approval_token: None,
        })
        .await?;
        drop(controller);
        Ok(result
            .content
            .into_iter()
            .find_map(|block| match block {
                pi_ai::ContentBlock::Text { text, .. } => Some(text),
                _ => None,
            })
            .unwrap_or_default())
    }

    #[test]
    fn approval_mode_precedence_is_cli_then_global_setting_then_yolo() {
        let cwd = tempfile::tempdir().expect("cwd");
        let agent = tempfile::tempdir().expect("agent dir");
        let base = |args: &[&str]| {
            let cli = Cli::try_parse_from(args).expect("cli");
            RunSessionBlueprint::from_cli(
                &cli,
                isolated_resource_options(cwd.path(), agent.path()),
                None,
            )
        };

        let mut settings = pi_coding::Settings::default();
        let default_blueprint = base(&["rpi"]);
        assert_eq!(
            default_blueprint.test_approval_mode(&settings),
            pi_agent::ApprovalMode::Yolo
        );

        settings.approval_mode = Some(pi_agent::ApprovalMode::Ask);
        assert_eq!(
            default_blueprint.test_approval_mode(&settings),
            pi_agent::ApprovalMode::Ask
        );

        let cli_blueprint = base(&["rpi", "--approval-mode", "write"]);
        assert_eq!(
            cli_blueprint.test_approval_mode(&settings),
            pi_agent::ApprovalMode::Write
        );
    }

    #[tokio::test]
    async fn default_tree_runtime_factory_cross_cwd_selects_candidate_tree() {
        let agent = tempfile::tempdir().expect("agent root");
        write_unconfined_sandbox(agent.path());
        let project_a = tempfile::tempdir().expect("project A");
        let project_b = tempfile::tempdir().expect("project B");
        let native_root = tempfile::tempdir().expect("native root");
        let cli =
            Cli::try_parse_from(["rpi", "--no-tools", "--api-key", "launch-secret"]).expect("cli");
        let storage = SessionStorage::DefaultTree {
            native_root: native_root.path().to_path_buf(),
        };
        let mut blueprint = RunSessionBlueprint::from_cli(
            &cli,
            isolated_resource_options(project_a.path(), agent.path()),
            None,
        );
        blueprint.set_session_storage(storage.clone());
        let (model, _faux) = faux_model("runtime-root-policy");

        let candidate = blueprint
            .build_runtime_candidate(
                project_b.path().to_path_buf(),
                options(project_b.path(), &model, agent.path()),
                None,
            )
            .await
            .expect("cross-cwd candidate");
        let application = pi_coding::Application::from_runtime_candidate(candidate)
            .await
            .expect("cross-cwd application");

        assert_eq!(
            application.session().session_dir(),
            storage
                .session_dir_for(project_b.path())
                .expect("project B tree")
        );
        assert_ne!(
            application.session().session_dir(),
            storage
                .session_dir_for(project_a.path())
                .expect("project A tree")
        );
        application.cleanup().await;
    }

    #[tokio::test]
    async fn explicit_root_runtime_factory_cross_cwd_remains_fixed() {
        let agent = tempfile::tempdir().expect("agent root");
        write_unconfined_sandbox(agent.path());
        let project_a = tempfile::tempdir().expect("project A");
        let project_b = tempfile::tempdir().expect("project B");
        let explicit_root = tempfile::tempdir().expect("explicit root");
        let cli =
            Cli::try_parse_from(["rpi", "--no-tools", "--api-key", "launch-secret"]).expect("cli");
        let mut blueprint = RunSessionBlueprint::from_cli(
            &cli,
            isolated_resource_options(project_a.path(), agent.path()),
            None,
        );
        blueprint.set_session_storage(SessionStorage::ExplicitRoot(
            explicit_root.path().to_path_buf(),
        ));
        let (model, _faux) = faux_model("runtime-explicit-root");

        let candidate = blueprint
            .build_runtime_candidate(
                project_b.path().to_path_buf(),
                options(project_b.path(), &model, agent.path()),
                None,
            )
            .await
            .expect("cross-cwd candidate");
        let application = pi_coding::Application::from_runtime_candidate(candidate)
            .await
            .expect("cross-cwd application");

        assert_eq!(application.session().session_dir(), explicit_root.path());
        application.cleanup().await;
    }

    #[tokio::test]
    async fn resumed_runtime_factory_keeps_original_recorder_path() {
        let agent = tempfile::tempdir().expect("agent root");
        write_unconfined_sandbox(agent.path());
        let project_a = tempfile::tempdir().expect("project A");
        let project_b = tempfile::tempdir().expect("project B");
        let source_root = tempfile::tempdir().expect("source root");
        let native_root = tempfile::tempdir().expect("native root");
        let (model, _faux) = faux_model("runtime-resume-recorder");
        let recorder = pi_coding::start_session_in(
            project_b.path(),
            Some(&model),
            Some("high"),
            Some(source_root.path()),
            None,
            None,
        )
        .expect("source recorder");
        recorder.persist_now().expect("persist source recorder");
        let source_path = recorder.path();
        drop(recorder);
        let prepared = pi_coding::PreparedSessionResume::prepare_path(&source_path)
            .expect("prepare source resume");
        let storage = SessionStorage::DefaultTree {
            native_root: native_root.path().to_path_buf(),
        };
        let cli =
            Cli::try_parse_from(["rpi", "--no-tools", "--api-key", "launch-secret"]).expect("cli");
        let mut blueprint = RunSessionBlueprint::from_cli(
            &cli,
            isolated_resource_options(project_a.path(), agent.path()),
            None,
        );
        blueprint.set_session_storage(storage.clone());

        let candidate = blueprint
            .build_runtime_candidate(
                project_b.path().to_path_buf(),
                options(project_b.path(), &model, agent.path()),
                Some(prepared),
            )
            .await
            .expect("resumed candidate");
        let application = pi_coding::Application::from_runtime_candidate(candidate)
            .await
            .expect("resumed application");
        let (_, recorder_path) = application
            .session()
            .recorder_info()
            .expect("resumed recorder");

        assert_eq!(
            application.session().session_dir(),
            storage
                .session_dir_for(project_b.path())
                .expect("project B tree")
        );
        assert_eq!(recorder_path, source_path);
        application.cleanup().await;
    }

    #[tokio::test]
    async fn resumed_runtime_factory_restores_journal_thinking_level() {
        let agent = tempfile::tempdir().expect("agent root");
        write_unconfined_sandbox(agent.path());
        let project_a = tempfile::tempdir().expect("project A");
        let project_b = tempfile::tempdir().expect("project B");
        let source_root = tempfile::tempdir().expect("source root");
        let native_root = tempfile::tempdir().expect("native root");
        let suffix = uuid::Uuid::now_v7().simple();
        let mut model = Model::default();
        model.id = "runtime-resume-thinking".to_owned();
        model.name = "runtime-resume-thinking".to_owned();
        model.api = format!("runtime-resume-thinking-{suffix}-api");
        model.provider = format!("runtime-resume-thinking-{suffix}-provider");
        model.base_url = "http://localhost:0".to_owned();
        model.reasoning = true;
        let _faux = register_faux_provider(pi_ai::providers::FauxProviderOptions {
            api: model.api.clone(),
            provider: model.provider.clone(),
            models: vec![model.clone()],
            chunk_size: 12,
        });
        let recorder = pi_coding::start_session_in(
            project_b.path(),
            Some(&model),
            Some("high"),
            Some(source_root.path()),
            None,
            None,
        )
        .expect("source recorder");
        recorder.persist_now().expect("persist source recorder");
        let source_path = recorder.path();
        drop(recorder);
        let prepared = pi_coding::PreparedSessionResume::prepare_path(&source_path)
            .expect("prepare source resume");
        assert!(prepared.tree().has_thinking_entry());
        assert_eq!(prepared.build_context().thinking_level, "high");
        let storage = SessionStorage::DefaultTree {
            native_root: native_root.path().to_path_buf(),
        };
        let cli =
            Cli::try_parse_from(["rpi", "--no-tools", "--api-key", "launch-secret"]).expect("cli");
        let mut blueprint = RunSessionBlueprint::from_cli(
            &cli,
            isolated_resource_options(project_a.path(), agent.path()),
            None,
        );
        blueprint.set_session_storage(storage);
        let mut session_options = options(project_b.path(), &model, agent.path());
        session_options.thinking_level = ThinkingLevel::Medium;

        let candidate = blueprint
            .build_runtime_candidate(
                project_b.path().to_path_buf(),
                session_options,
                Some(prepared),
            )
            .await
            .expect("resumed candidate");
        let application = pi_coding::Application::from_runtime_candidate(candidate)
            .await
            .expect("resumed application");
        assert_eq!(
            application.session().thinking_level(),
            ThinkingLevel::High,
            "IsolatedApplication factory resume must restore journal thinking, not the new-session default"
        );
        application.cleanup().await;
    }


    #[tokio::test]
    async fn blueprint_rebuilds_isolated_cwd_resources_tools_and_orchestration() {
        let agent = tempfile::tempdir().expect("agent root");
        write_unconfined_sandbox(agent.path());
        let root_a = tempfile::tempdir().expect("root A");
        let root_b = tempfile::tempdir().expect("root B");
        let add_dir = tempfile::tempdir().expect("additional root");
        write_project(root_a.path(), "alpha", 111);
        write_project(root_b.path(), "beta", 222);

        let cli = Cli::try_parse_from([
            "rpi",
            "--approve",
            "--add-dir",
            add_dir.path().to_str().expect("utf8 add dir"),
            "--tools",
            "read,process,glob,task,hub",
            "--api-key",
            "launch-secret",
        ])
        .expect("cli");
        let mut resource_options = ResourceManagerOptions::new(root_a.path());
        resource_options.agent_dir = agent.path().to_path_buf();
        resource_options.headless = true;
        resource_options.disable_extensions = true;
        resource_options.disable_prompt_templates = true;
        resource_options.disable_themes = true;
        resource_options.project_trust_override = Some(true);
        let blueprint = RunSessionBlueprint::from_cli(&cli, resource_options, None);
        let cloned = blueprint.clone();
        let (mut model, _faux) = faux_model("launch-model");
        model.reasoning = true;

        let candidate_a = blueprint
            .build(root_a.path(), options(root_a.path(), &model, agent.path()))
            .await
            .expect("candidate A");
        let candidate_b = cloned
            .build(root_b.path(), options(root_b.path(), &model, agent.path()))
            .await
            .expect("candidate B");
        let owner_a = candidate_a.session.process_owner_id();
        let owner_b = candidate_b.session.process_owner_id();
        let candidate_a = own_candidate(candidate_a).await;
        let candidate_b = own_candidate(candidate_b).await;

        assert_eq!(
            candidate_a.session.cwd(),
            root_a.path().canonicalize().expect("root A")
        );
        assert_eq!(
            candidate_b.session.cwd(),
            root_b.path().canonicalize().expect("root B")
        );
        assert_eq!(
            candidate_b.session.workspace_roots().additional_roots(),
            [add_dir.path().canonicalize().expect("add dir")]
        );
        assert_eq!(candidate_b.session.model().as_ref(), Some(&model));
        assert_eq!(candidate_b.session.thinking_level(), ThinkingLevel::High);
        assert_eq!(candidate_b.session.current_api_key(), "launch-secret");
        assert_eq!(
            candidate_a.session.stream_options().stream.max_tokens,
            Some(111)
        );
        assert_eq!(
            candidate_b.session.stream_options().stream.max_tokens,
            Some(222)
        );
        assert_ne!(owner_a, owner_b);

        let snapshot_a = candidate_a
            .session
            .resource_manager()
            .expect("resources A")
            .snapshot();
        let snapshot_b = candidate_b
            .session
            .resource_manager()
            .expect("resources B")
            .snapshot();
        assert_eq!(
            snapshot_a.cwd,
            root_a.path().canonicalize().expect("root A")
        );
        assert_eq!(
            snapshot_b.cwd,
            root_b.path().canonicalize().expect("root B")
        );
        assert!(
            snapshot_a
                .context_files
                .iter()
                .all(|file| !file.content.contains("beta"))
        );
        assert!(
            snapshot_b
                .context_files
                .iter()
                .all(|file| !file.content.contains("alpha"))
        );
        assert!(snapshot_a.agents.iter().any(|agent| agent.name == "alpha"));
        assert!(!snapshot_a.agents.iter().any(|agent| agent.name == "beta"));
        assert!(snapshot_b.agents.iter().any(|agent| agent.name == "beta"));
        assert!(!snapshot_b.agents.iter().any(|agent| agent.name == "alpha"));
        assert!(
            snapshot_a
                .skills
                .iter()
                .any(|skill| skill.name == "alpha-skill")
        );
        assert!(
            !snapshot_a
                .skills
                .iter()
                .any(|skill| skill.name == "beta-skill")
        );
        assert!(
            snapshot_b
                .skills
                .iter()
                .any(|skill| skill.name == "beta-skill")
        );
        assert!(
            !snapshot_b
                .skills
                .iter()
                .any(|skill| skill.name == "alpha-skill")
        );

        assert_eq!(
            candidate_a.session.get_active_tool_names(),
            ["read", "glob", "task", "hub", "process"]
        );
        assert_eq!(
            candidate_b.session.get_active_tool_names(),
            ["read", "glob", "task", "hub", "process"]
        );
        let read_a = candidate_a
            .session
            .get_tool_definition("read")
            .expect("read A");
        let read_b = candidate_b
            .session
            .get_tool_definition("read")
            .expect("read B");
        assert!(
            read_tool(&read_a, "AGENTS.md")
                .await
                .expect("read A")
                .contains("alpha")
        );
        assert!(
            read_tool(&read_b, "AGENTS.md")
                .await
                .expect("read B")
                .contains("beta")
        );

        let orchestration_a = candidate_a.orchestration.as_ref().expect("orchestration A");
        let orchestration_b = candidate_b.orchestration.as_ref().expect("orchestration B");
        assert!(orchestration_a.catalog().get("alpha").is_some());
        assert!(orchestration_a.catalog().get("beta").is_none());
        assert!(orchestration_b.catalog().get("beta").is_some());
        assert!(orchestration_b.catalog().get("alpha").is_none());
        assert!(
            orchestration_a
                .skills()
                .iter()
                .any(|skill| skill.name == "alpha-skill")
        );
        assert!(
            orchestration_b
                .skills()
                .iter()
                .any(|skill| skill.name == "beta-skill")
        );
        assert_eq!(
            orchestration_a
                .resolve_read_uri("artifact://missing")
                .expect_err("missing A artifact")
                .to_string()
                .contains(root_b.path().to_string_lossy().as_ref()),
            false,
        );
        assert_eq!(
            orchestration_b
                .resolve_read_uri("artifact://missing")
                .expect_err("missing B artifact")
                .to_string()
                .contains(root_a.path().to_string_lossy().as_ref()),
            false,
        );

        candidate_a.application.cleanup().await;
        candidate_b.application.cleanup().await;
    }

    #[test]
    fn workflow_orchestration_artifacts_stay_outside_worktree() {
        let root = tempfile::tempdir().expect("root");
        let agent = tempfile::tempdir().expect("agent root");
        let cwd = root.path().join("workflow-worktrees").join("workflow-id");
        fs::create_dir_all(&cwd).expect("workflow worktree");
        let resources =
            ResourceManager::new(isolated_resource_options(&cwd, agent.path())).expect("resources");
        let snapshot = resources.snapshot();
        let config = orchestration_config(
            &snapshot,
            &pi_coding::Settings::default(),
            &pi_ai::Model::default(),
            true,
        );
        assert!(!config.artifact_dir.starts_with(&snapshot.cwd));
        assert!(config.artifact_dir.ends_with("workflow-artifacts"));
    }

    /// Write an executable `pre_trust_decision` hook that appends the stdin
    /// payload to `capture` and answers with `response`.
    fn write_trust_hook(dir: &Path, capture: &Path, response: &str) -> PathBuf {
        let hook = dir.join("hook.sh");
        let capture_text = capture.to_string_lossy().into_owned();
        fs::write(
            &hook,
            format!(
                "#!/bin/sh\nread -r payload\nprintf '%s\\n' \"$payload\" >> \"{capture_text}\"\necho '{response}'\n"
            ),
        )
        .expect("hook script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&hook).expect("hook meta").permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&hook, permissions).expect("hook mode");
        }
        hook
    }

    /// A project-local package QuickJS extension whose top level throws:
    /// executing it at startup fails the extension load, so a startup that
    /// succeeds proves the extension was never executed. Registered as a
    /// project package so discovery happens only after project trust; an
    /// explicit `--extension` path would fail closed before the host hook
    /// can deny trust.
    fn write_throwing_project_extension(root: &Path) {
        let package = root.join(".rpi").join("packages").join("boom-ext");
        let ext = package.join("extensions").join("boom");
        fs::create_dir_all(&ext).expect("extension dir");
        fs::write(
            ext.join("entry.mjs"),
            "throw new Error('boom at top level');\n",
        )
        .expect("extension entry");
        fs::write(
            ext.join("pi-extension.json"),
            serde_json::to_vec_pretty(&json!({
                "schemaVersion": 1,
                "id": "boom-ext",
                "runtime": "quickjs",
                "entry": "entry.mjs",
                "capabilities": [],
                "uiCapabilities": []
            }))
            .expect("manifest json"),
        )
        .expect("extension manifest");
        fs::write(
            package.join("package.json"),
            serde_json::to_vec_pretty(&json!({
                "pi": {
                    "schemaVersion": 1,
                    "extensions": ["extensions/boom/pi-extension.json"]
                }
            }))
            .expect("package json"),
        )
        .expect("package manifest");
        fs::write(
            root.join(".rpi").join("settings.json"),
            serde_json::to_vec_pretty(&json!({
                "packages": ["packages/boom-ext"]
            }))
            .expect("project package settings"),
        )
        .expect("project settings");
    }

    #[tokio::test]
    async fn blocking_pre_trust_hook_prevents_project_extension_startup_execution() {
        let agent = tempfile::tempdir().expect("agent root");
        let cwd = tempfile::tempdir().expect("cwd");
        write_unconfined_sandbox(agent.path());
        let hook_dir = tempfile::tempdir().expect("hook dir");
        let cwd_path = cwd.path().canonicalize().expect("canonical cwd");
        let capture = hook_dir.path().join("payloads.jsonl");
        let (mut model, _faux) = faux_model("launch-model");
        model.reasoning = true;

        write_throwing_project_extension(&cwd_path);
        // Trust-gated project resources (a non-empty `.rpi` directory): their
        // presence makes the trust store consulted, so the hook observation
        // fires.
        fs::create_dir_all(cwd_path.join(".rpi")).expect(".rpi dir");
        fs::write(cwd_path.join(".rpi").join("marker"), "gated").expect(".rpi marker");
        // The store decision is Trusted so the project package extension
        // enters the resource snapshot; only the host hook can stop it.
        pi_coding::TrustStore::new(agent.path())
            .set(&cwd_path, pi_coding::TrustDecision::Trusted)
            .expect("trusted store");
        let hook = write_trust_hook(
            hook_dir.path(),
            &capture,
            r#"{"decision":"block","reason":"denied by policy"}"#,
        );
        fs::write(
            agent.path().join("settings.json"),
            serde_json::to_vec_pretty(&json!({
                "sandbox": { "mode": "danger-full-access" },
                "hooks": [{
                    "event": "pre_trust_decision",
                    "command": [hook.to_string_lossy()],
                    "timeoutMs": 2_000,
                    "failClosed": true,
                }],
            }))
            .expect("settings json"),
        )
        .expect("agent settings");

        let cli = Cli::try_parse_from(["rpi", "--api-key", "launch-secret"]).expect("cli");
        let mut resource_options = ResourceManagerOptions::new(cwd.path());
        resource_options.disable_skills = true;
        resource_options.disable_prompt_templates = true;
        resource_options.disable_themes = true;
        resource_options.disable_context_files = true;
        resource_options.agent_dir = agent.path().to_path_buf();
        resource_options.headless = true;
        resource_options.disable_extensions = false;
        let blueprint = RunSessionBlueprint::from_cli(&cli, resource_options, None);

        // The host hook fires BEFORE the extension runtime loads and blocks:
        // startup succeeds and the throwing project extension is never
        // executed (had it run, the top-level throw would fail the load).
        let candidate = own_candidate(
            blueprint
                .build(cwd.path(), options(&cwd_path, &model, agent.path()))
                .await
                .expect("startup with a blocking host hook must succeed"),
        )
        .await;
        let payload = fs::read_to_string(&capture).expect("captured payload");
        let captured: serde_json::Value =
            serde_json::from_str(payload.lines().last().expect("one payload")).expect("json");
        assert_eq!(captured["event"], "pre_trust_decision");
        assert_eq!(captured["path"], cwd_path.to_string_lossy().as_ref());
        assert_eq!(captured["decision"], "trusted");
        assert_eq!(captured["isNew"], false);
        assert_eq!(
            payload.lines().count(),
            1,
            "the host hook must fire exactly once at startup (pre-load)"
        );
        assert!(
            candidate
                .application
                .extension_runtime()
                .expect("extension runtime")
                .agent_tools()
                .is_empty(),
            "the blocked project extension must not be loaded"
        );
        assert_eq!(
            candidate
                .session
                .resource_manager()
                .expect("resources")
                .snapshot()
                .trust
                .decision,
            pi_coding::TrustDecision::Untrusted,
            "the snapshot must record the host-composed denial"
        );
        candidate.application.cleanup().await;

        // Without the blocking hook the same project extension runs and its
        // top-level throw fails the load: startup errors with the extension
        // id in the failure, proving the extension was executed.
        let agent2 = tempfile::tempdir().expect("agent root 2");
        write_unconfined_sandbox(agent2.path());
        pi_coding::TrustStore::new(agent2.path())
            .set(&cwd_path, pi_coding::TrustDecision::Trusted)
            .expect("trusted store 2");
        let mut resource_options = ResourceManagerOptions::new(cwd.path());
        resource_options.agent_dir = agent2.path().to_path_buf();
        resource_options.headless = true;
        resource_options.disable_extensions = false;
        resource_options.disable_skills = true;
        resource_options.disable_prompt_templates = true;
        resource_options.disable_themes = true;
        let blueprint = RunSessionBlueprint::from_cli(&cli, resource_options, None);
        let candidate = blueprint
            .build(cwd.path(), options(&cwd_path, &model, agent2.path()))
            .await
            .expect("unblocked candidate preparation");
        let error = match try_own_candidate(candidate).await {
            Ok(_) => panic!("the throwing project extension must fail during publication"),
            Err(error) => error,
        };
        assert!(
            format!("{error:#}").contains("boom-ext"),
            "extension startup failure must name the extension: {error:#}"
        );
    }

    #[test]
    fn orchestration_config_carries_preference_and_soft_budget_from_settings() {
        let cwd = tempfile::tempdir().expect("cwd");
        let agent = tempfile::tempdir().expect("agent");
        write_project(cwd.path(), "alpha", 128);
        let mut resource_options = isolated_resource_options(cwd.path(), agent.path());
        resource_options.project_trust_override = Some(true);
        resource_options.disable_skills = false;
        let resources = ResourceManager::new(resource_options).expect("resources");
        let snapshot = resources.snapshot();
        let mut settings = pi_coding::Settings::default();
        settings.orchestration = Some(pi_coding::OrchestrationSettings {
            preferred_agent: Some("alpha".to_owned()),
            soft_budget: Some(pi_coding::SoftBudgetSettings {
                max_requests: Some(3),
                max_tokens: Some(400),
                yield_after: Some(2),
            }),
            ..pi_coding::OrchestrationSettings::default()
        });
        let model = Model {
            id: "pref".into(),
            name: "pref".into(),
            api: "pref".into(),
            provider: "test".into(),
            ..Model::default()
        };
        let config = test_orchestration_config(&snapshot, &settings, &model);
        assert_eq!(config.preferred_agent.as_deref(), Some("alpha"));
        assert_eq!(config.soft_budget.max_requests, Some(3));
        assert_eq!(config.soft_budget.max_tokens, Some(400));
        assert_eq!(config.soft_budget.yield_after, Some(2));

        let default_config =
            test_orchestration_config(&snapshot, &pi_coding::Settings::default(), &model);
        assert!(default_config.preferred_agent.is_none());
        assert_eq!(
            default_config.soft_budget,
            pi_coding::JobSoftBudget::default()
        );
    }

    /// Build a workflow child `Application` through the production
    /// `RunSessionBlueprint::build_trusted_workflow_candidate` trait path
    /// verbatim (clone blueprint, force `project_trust_override`, call
    /// `build_workflow`, `session.start_new_recording()`, wrap the
    /// `ApplicationRuntimeCandidate`) and then `Application::from_runtime_candidate`
    /// — the exact two steps `ApplicationWorkflowRuntimeFactory::build_child`
    /// runs. The resulting workflow child owns a real orchestration runtime
    /// whose `max_tools_per_agent` reflects `orchestration_config(is_workflow)`,
    /// so a worker spawned through it exercises the production cap path.
    async fn build_trusted_workflow_application(
        blueprint: &RunSessionBlueprint,
        cwd: &Path,
        model: &Model,
        agent: &Path,
    ) -> Result<pi_coding::Application> {
        let mut blueprint = blueprint.clone();
        blueprint.resource_options.project_trust_override = Some(true);
        let candidate = blueprint
            .build_workflow(cwd, options(cwd, model, agent))
            .await?;
        let RunSessionCandidate {
            session,
            extension_runtime,
            extension_permissions,
            orchestration,
            goal_tool,
            todo_child_tool,
        } = candidate;
        session.start_new_recording()?;
        let mut runtime = ApplicationRuntimeCandidate::new(session)
            .with_extensions(extension_runtime, extension_permissions);
        if let Some(orchestration) = orchestration {
            runtime = runtime.with_orchestration(orchestration);
        }
        if let Some(goal_tool) = goal_tool {
            runtime = runtime.with_goal_tool(goal_tool);
        }
        if let Some(todo_child_tool) = todo_child_tool {
            runtime = runtime.with_todo_child_tool(todo_child_tool);
        }
        pi_coding::Application::from_runtime_candidate(runtime).await
    }

    /// The workflow child orchestration cap defaults to the actual dynamic
    /// factory tool set (22) only when `orchestration.maxToolsPerAgent` is unset,
    /// so a default `task` worker (19 coding tools + memory) never trips the cap.
    /// Inherited `tools: None` sets are also exempt at spawn. An explicit cap
    /// overrides the default and stays fail-closed. The parent (non-workflow)
    /// orchestration keeps the legacy 16 default for explicit lists.
    #[test]
    fn workflow_child_default_tool_cap_covers_dynamic_factory_set_when_unset() {
        let cwd = tempfile::tempdir().expect("cwd");
        let agent = tempfile::tempdir().expect("agent");
        write_project(cwd.path(), "cap", 128);
        let mut resource_options = isolated_resource_options(cwd.path(), agent.path());
        resource_options.project_trust_override = Some(true);
        let snapshot = pi_coding::ResourceManager::new(resource_options)
            .expect("resources")
            .snapshot();
        let model = Model::default();

        // Unset workflow child cap → raised default (22).
        let workflow_config =
            test_workflow_orchestration_config(&snapshot, &pi_coding::Settings::default(), &model);
        assert_eq!(
            workflow_config.max_tools_per_agent, 22,
            "unset workflow child cap must cover the dynamic factory set (19 + 3 hindsight = 22)"
        );

        // Explicit workflow child cap is preserved (fail-closed at that value).
        let mut settings = pi_coding::Settings::default();
        settings.orchestration = Some(pi_coding::OrchestrationSettings {
            max_tools_per_agent: Some(5),
            ..pi_coding::OrchestrationSettings::default()
        });
        let explicit = test_workflow_orchestration_config(&snapshot, &settings, &model);
        assert_eq!(explicit.max_tools_per_agent, 5);

        // Parent (non-workflow) orchestration keeps the legacy 16 default.
        let parent_config =
            test_orchestration_config(&snapshot, &pi_coding::Settings::default(), &model);
        assert_eq!(parent_config.max_tools_per_agent, 16);
    }

    /// Trusted blueprint workflow child with an UNSET cap spawns a default
    /// `task` worker (19 coding tools + memory) through its production
    /// orchestration, settles the job, and never surfaces a tool-cap error —
    /// the raised default (22) covers the dynamic factory set.
    #[tokio::test]
    async fn trusted_blueprint_workflow_child_settles_without_cap_error_on_unset_capacity() {
        let agent = tempfile::tempdir().expect("agent root");
        write_unconfined_sandbox(agent.path());
        let root = tempfile::tempdir().expect("project root");
        write_project(root.path(), "pizig", 128);
        let cli = Cli::try_parse_from([
            "rpi",
            "--approve",
            "--tools",
            "read,task,hub",
            "--api-key",
            "launch-secret",
        ])
        .expect("cli");
        let mut resource_options = isolated_resource_options(root.path(), agent.path());
        resource_options.project_trust_override = Some(true);
        let blueprint = RunSessionBlueprint::from_cli(&cli, resource_options, None);
        let (model, registration) = faux_model("pizig-workflow");
        // The worker calls `yield` once and settles; the supervisor is not
        // driven here (we spawn the worker directly through the workflow child's
        // orchestration), so exactly one model response is consumed.
        registration.set_responses(vec![FauxResponse {
            content: vec![pi_ai::ContentBlock::ToolCall(pi_ai::ToolCall {
                id: "yield-call".to_owned(),
                name: "yield".to_owned(),
                arguments: json!({ "text": "settled deliverable" }),
                thought_signature: None,
            })],
            stop_reason: pi_ai::StopReason::ToolUse,
            error_message: None,
        }]);
        let application =
            build_trusted_workflow_application(&blueprint, root.path(), &model, agent.path())
                .await
                .expect("workflow child construction through the production blueprint path");
        let orchestration = application
            .orchestration_runtime()
            .expect("workflow child orchestration");
        assert_eq!(
            orchestration.main_agent_id(),
            "Main",
            "the workflow child supervisor is the orchestration main agent"
        );
        let spawn = orchestration
            .spawn_tasks(
                orchestration.main_agent_id(),
                0,
                vec![pi_coding::TaskItem {
                    index: 0,
                    id: "settle-worker".to_owned(),
                    agent: "task".to_owned(),
                    assignment: "deliver the settled result".to_owned(),
                    ..Default::default()
                }],
            )
            .expect("unset cap must let the default task worker spawn")
            .remove(0);
        let settled = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            orchestration.wait_jobs(
                &[spawn.job_id.clone()],
                Some(std::time::Duration::from_secs(30)),
                None,
            ),
        )
        .await
        .expect("workflow child job must settle")
        .expect("wait_jobs")
        .into_iter()
        .find(|job| job.id == spawn.job_id)
        .expect("settled job");
        assert_eq!(
            settled.status,
            pi_coding::JobStatus::Completed,
            "the default task worker must settle Completed (no cap error): {:?}",
            settled.result
        );
        let result = settled.result.expect("settled result");
        assert_eq!(result.output, "settled deliverable");
        assert!(
            result
                .error
                .as_deref()
                .map_or(true, |error| !error.contains("cap")),
            "no tool-cap error must surface: {:?}",
            result.error
        );
        application.cleanup().await;
    }

    /// An explicit low workflow child cap stays fail-closed for an agent that
    /// *declares* a tools list larger than the cap. Bundled `task` (`tools: None`)
    /// is inherit-exempt and is covered by the sibling test below.
    #[tokio::test]
    async fn explicit_low_workflow_tool_cap_fails_closed_at_spawn() {
        let agent = tempfile::tempdir().expect("agent root");
        write_unconfined_sandbox(agent.path());
        let root = tempfile::tempdir().expect("project root");
        write_project(root.path(), "pizig", 128);
        fs::write(
            root.path().join(".rpi").join("agents").join("wide.md"),
            format!(
                "---\nname: wide\ndescription: explicit wide agent\ntools: [{}]\n---\nwide prompt\n",
                pi_coding::DEFAULT_CHILD_CODING_TOOLS.join(", ")
            ),
        )
        .expect("explicit wide agent");
        // An explicit cap of 5 is below the 19-name declared list.
        fs::write(
            root.path().join(".rpi").join("settings.json"),
            serde_json::to_vec_pretty(&json!({
                "maxTokens": 128,
                "orchestration": {
                    "tasks": true,
                    "process": true,
                    "glob": true,
                    "maxToolsPerAgent": 5
                }
            }))
            .expect("settings json"),
        )
        .expect("explicit low cap settings");
        let cli = Cli::try_parse_from([
            "rpi",
            "--approve",
            "--tools",
            "read,task,hub",
            "--api-key",
            "launch-secret",
        ])
        .expect("cli");
        let mut resource_options = isolated_resource_options(root.path(), agent.path());
        resource_options.project_trust_override = Some(true);
        let blueprint = RunSessionBlueprint::from_cli(&cli, resource_options, None);
        let (model, _registration) = faux_model("pizig-workflow-low-cap");
        let application =
            build_trusted_workflow_application(&blueprint, root.path(), &model, agent.path())
                .await
                .expect("workflow child construction");
        let orchestration = application
            .orchestration_runtime()
            .expect("workflow child orchestration");
        assert_eq!(orchestration.main_agent_id(), "Main");
        let error = orchestration
            .spawn_tasks(
                orchestration.main_agent_id(),
                0,
                vec![pi_coding::TaskItem {
                    index: 0,
                    id: "capped-worker".to_owned(),
                    agent: "wide".to_owned(),
                    assignment: "exceed the explicit cap".to_owned(),
                    ..Default::default()
                }],
            )
            .expect_err("explicit low cap must fail closed at spawn");
        let message = format!("{error:#}");
        assert!(
            message.contains("cap 5"),
            "the actionable cap error must name the cap: {message}"
        );
        assert!(
            message.contains("wide"),
            "the cap error must name the agent: {message}"
        );
        application.cleanup().await;
    }

    /// Bundled `task` declares no `tools` field, so it inherits the full default
    /// child coding set and is exempt from an explicit low cap.
    #[tokio::test]
    async fn explicit_low_workflow_cap_still_spawns_inherited_bundled_task() {
        let agent = tempfile::tempdir().expect("agent root");
        write_unconfined_sandbox(agent.path());
        let root = tempfile::tempdir().expect("project root");
        write_project(root.path(), "pizig", 128);
        fs::write(
            root.path().join(".rpi").join("settings.json"),
            serde_json::to_vec_pretty(&json!({
                "maxTokens": 128,
                "orchestration": {
                    "tasks": true,
                    "process": true,
                    "glob": true,
                    "maxToolsPerAgent": 5
                }
            }))
            .expect("settings json"),
        )
        .expect("explicit low cap settings");
        let cli = Cli::try_parse_from([
            "rpi",
            "--approve",
            "--api-key",
            "launch-secret",
        ])
        .expect("cli");
        let mut resource_options = isolated_resource_options(root.path(), agent.path());
        resource_options.project_trust_override = Some(true);
        let blueprint = RunSessionBlueprint::from_cli(&cli, resource_options, None);
        let (model, _registration) = faux_model("pizig-workflow-inherit-cap");
        let application =
            build_trusted_workflow_application(&blueprint, root.path(), &model, agent.path())
                .await
                .expect("workflow child construction");
        let orchestration = application
            .orchestration_runtime()
            .expect("workflow child orchestration");
        let spawn = orchestration
            .spawn_tasks(
                orchestration.main_agent_id(),
                0,
                vec![pi_coding::TaskItem {
                    index: 0,
                    id: "inherit-worker".to_owned(),
                    agent: "task".to_owned(),
                    assignment: "inherited full set must spawn".to_owned(),
                    ..Default::default()
                }],
            )
            .expect("inherited tools:None must be exempt from the explicit cap");
        assert_eq!(spawn.len(), 1);
        application.cleanup().await;
    }

    #[test]
    fn interactive_main_todo_gate_follows_orchestration_todo_setting() {
        let cwd = tempfile::tempdir().expect("cwd");
        let agent = tempfile::tempdir().expect("agent");
        let cli = Cli::try_parse_from(["rpi", "--approve", "--api-key", "launch-secret"])
            .expect("cli");
        let blueprint = RunSessionBlueprint::from_cli(
            &cli,
            isolated_resource_options(cwd.path(), agent.path()),
            None,
        );
        let mut settings = pi_coding::Settings::default();
        settings.orchestration = Some(pi_coding::OrchestrationSettings {
            todo: Some(false),
            ..pi_coding::OrchestrationSettings::default()
        });
        let gates = blueprint.test_tool_gates(&settings);
        assert!(
            !gates.3,
            "interactive Main todo must stay off when orchestration.todo is false"
        );

        let requested = Cli::try_parse_from([
            "rpi",
            "--approve",
            "--tools",
            "todo",
            "--api-key",
            "launch-secret",
        ])
        .expect("cli");
        let requested_blueprint = RunSessionBlueprint::from_cli(
            &requested,
            isolated_resource_options(cwd.path(), agent.path()),
            None,
        );
        let requested_gates = requested_blueprint.test_tool_gates(&settings);
        assert!(
            requested_gates.3,
            "CLI --tools todo must still enable interactive Main todo"
        );
    }

    /// Construction-time contract: workflow `build_session(force_workflow_runtime)`
    /// injects the internal Todo tool even when `orchestration.todo` is false
    /// and the parent `--tools` allow-list omitted `todo`. Interactive Main
    /// with the same settings stays without the tool.
    #[tokio::test]
    async fn workflow_planner_injects_todo_when_orchestration_todo_is_false() {
        let agent = tempfile::tempdir().expect("agent root");
        write_unconfined_sandbox(agent.path());
        let root = tempfile::tempdir().expect("project root");
        write_project(root.path(), "plan", 128);
        fs::write(
            root.path().join(".rpi").join("settings.json"),
            serde_json::to_vec_pretty(&json!({
                "maxTokens": 128,
                "orchestration": {
                    "tasks": true,
                    "process": true,
                    "glob": true,
                    "todo": false
                }
            }))
            .expect("settings json"),
        )
        .expect("todo:false settings");
        let cli = Cli::try_parse_from([
            "rpi",
            "--approve",
            "--tools",
            "read,task,hub",
            "--api-key",
            "launch-secret",
        ])
        .expect("cli");
        let mut resource_options = isolated_resource_options(root.path(), agent.path());
        resource_options.project_trust_override = Some(true);
        let blueprint = RunSessionBlueprint::from_cli(&cli, resource_options, None);
        let (model, _registration) = faux_model("plan-todo-false");

        let workflow = blueprint
            .build_workflow(root.path(), options(root.path(), &model, agent.path()))
            .await
            .expect("workflow candidate");
        assert!(
            workflow
                .session
                .get_active_tool_names()
                .iter()
                .any(|name| name == "todo"),
            "workflow planner must keep internal Todo when orchestration.todo is false: {:?}",
            workflow.session.get_active_tool_names()
        );
        if let Some(orchestration) = workflow.orchestration {
            let _ = orchestration.shutdown().await;
        }
        workflow.extension_runtime.shutdown().await;

        let main = blueprint
            .build(root.path(), options(root.path(), &model, agent.path()))
            .await
            .expect("interactive main candidate");
        assert!(
            !main
                .session
                .get_active_tool_names()
                .iter()
                .any(|name| name == "todo"),
            "interactive Main must stay setting-controlled: {:?}",
            main.session.get_active_tool_names()
        );
        if let Some(orchestration) = main.orchestration {
            let _ = orchestration.shutdown().await;
        }
        main.extension_runtime.shutdown().await;

        let application = build_trusted_workflow_application(
            &blueprint, root.path(), &model, agent.path(),
        )
        .await
        .expect("attached workflow application");
        let active_tools = application.get_active_tool_names();
        let reload = application.reload().await;
        let reloaded_tools = application.get_active_tool_names();
        application.cleanup().await;
        assert!(
            active_tools.iter().any(|name| name == "todo"),
            "attached workflow planner must retain callable Todo under interactive todo:false: {active_tools:?}"
        );
        reload.expect("reload attached workflow application");
        assert!(
            reloaded_tools.iter().any(|name| name == "todo"),
            "reloaded workflow planner must retain callable Todo under interactive todo:false: {reloaded_tools:?}"
        );
    }

    #[tokio::test]
    async fn interactive_todo_reload_drops_unpinned_todo_when_setting_disabled() {
        let agent = tempfile::tempdir().expect("agent root");
        write_unconfined_sandbox(agent.path());
        let root = tempfile::tempdir().expect("project root");
        write_project(root.path(), "plan", 128);
        let cli = Cli::try_parse_from(["rpi", "--approve", "--api-key", "launch-secret"])
            .expect("cli");
        let mut resource_options = isolated_resource_options(root.path(), agent.path());
        resource_options.project_trust_override = Some(true);
        let blueprint = RunSessionBlueprint::from_cli(&cli, resource_options, None);
        let (model, _registration) = faux_model("interactive-todo-reload");

        let candidate = blueprint
            .build(root.path(), options(root.path(), &model, agent.path()))
            .await
            .expect("interactive main");
        let RunSessionCandidate {
            session,
            extension_runtime,
            extension_permissions,
            orchestration,
            goal_tool,
            todo_child_tool,
        } = candidate;
        session.start_new_recording().expect("recording");
        let mut runtime = ApplicationRuntimeCandidate::new(session)
            .with_extensions(extension_runtime, extension_permissions);
        if let Some(orchestration) = orchestration {
            runtime = runtime.with_orchestration(orchestration);
        }
        if let Some(goal_tool) = goal_tool {
            runtime = runtime.with_goal_tool(goal_tool);
        }
        if let Some(todo_child_tool) = todo_child_tool {
            runtime = runtime.with_todo_child_tool(todo_child_tool);
        }
        let application = pi_coding::Application::from_runtime_candidate(runtime)
            .await
            .expect("attached interactive application");
        let active_tools = application.get_active_tool_names();
        assert!(
            active_tools.iter().any(|name| name == "todo"),
            "interactive Main starts with setting-controlled Todo: {active_tools:?}"
        );
        fs::write(
            root.path().join(".rpi").join("settings.json"),
            serde_json::to_vec_pretty(&json!({
                "maxTokens": 128,
                "orchestration": {
                    "tasks": true,
                    "process": true,
                    "glob": true,
                    "todo": false
                }
            }))
            .expect("settings json"),
        )
        .expect("todo:false settings");
        let reload = application.reload().await;
        let reloaded_tools = application.get_active_tool_names();
        application.cleanup().await;
        reload.expect("reload interactive application");
        assert!(
            !reloaded_tools.iter().any(|name| name == "todo"),
            "interactive reload must drop unpinned Todo when orchestration.todo is false: {reloaded_tools:?}"
        );
    }

    /// Construction-time contract: an explicit `--no-tools` request would strip
    /// the internal Todo tool the workflow planner depends on, so workflow
    /// `build_session(force_workflow_runtime)` must fail construction naming
    /// `--no-tools` instead of silently overriding the explicit disable.
    #[tokio::test]
    async fn workflow_force_runtime_rejects_no_tools_naming_the_flag() {
        let agent = tempfile::tempdir().expect("agent root");
        write_unconfined_sandbox(agent.path());
        let root = tempfile::tempdir().expect("project root");
        write_project(root.path(), "plan", 128);
        let cli = Cli::try_parse_from([
            "rpi",
            "--approve",
            "--no-tools",
            "--api-key",
            "launch-secret",
        ])
        .expect("cli");
        let mut resource_options = isolated_resource_options(root.path(), agent.path());
        resource_options.project_trust_override = Some(true);
        let blueprint = RunSessionBlueprint::from_cli(&cli, resource_options, None);
        let (model, _registration) = faux_model("plan-no-tools");

        let error = match blueprint
            .build_workflow(root.path(), options(root.path(), &model, agent.path()))
            .await
        {
            Ok(_) => panic!("workflow construction must reject --no-tools"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("--no-tools"),
            "error must name --no-tools: {error:#}"
        );
    }

    /// Construction-time contract: an explicit `--exclude-tools todo` request
    /// would strip the internal Todo tool the workflow planner depends on, so
    /// workflow `build_session(force_workflow_runtime)` must fail construction
    /// naming `--exclude-tools` instead of silently overriding the explicit
    /// deny.
    #[tokio::test]
    async fn workflow_force_runtime_rejects_exclude_tools_todo_naming_the_flag() {
        let agent = tempfile::tempdir().expect("agent root");
        write_unconfined_sandbox(agent.path());
        let root = tempfile::tempdir().expect("project root");
        write_project(root.path(), "plan", 128);
        let cli = Cli::try_parse_from([
            "rpi",
            "--approve",
            "--exclude-tools",
            "todo",
            "--api-key",
            "launch-secret",
        ])
        .expect("cli");
        let mut resource_options = isolated_resource_options(root.path(), agent.path());
        resource_options.project_trust_override = Some(true);
        let blueprint = RunSessionBlueprint::from_cli(&cli, resource_options, None);
        let (model, _registration) = faux_model("plan-exclude-todo");

        let error = match blueprint
            .build_workflow(root.path(), options(root.path(), &model, agent.path()))
            .await
        {
            Ok(_) => panic!("workflow construction must reject --exclude-tools todo"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("--exclude-tools"),
            "error must name --exclude-tools: {error:#}"
        );
    }

    /// Construction-time contract: an unrelated `--exclude-tools` deny (not
    /// `todo`) must not block workflow construction, and the workflow planner
    /// still carries the internal Todo tool.
    #[tokio::test]
    async fn workflow_force_runtime_unrelated_deny_succeeds_and_keeps_todo() {
        let agent = tempfile::tempdir().expect("agent root");
        write_unconfined_sandbox(agent.path());
        let root = tempfile::tempdir().expect("project root");
        write_project(root.path(), "plan", 128);
        let cli = Cli::try_parse_from([
            "rpi",
            "--approve",
            "--exclude-tools",
            "browser",
            "--api-key",
            "launch-secret",
        ])
        .expect("cli");
        let mut resource_options = isolated_resource_options(root.path(), agent.path());
        resource_options.project_trust_override = Some(true);
        let blueprint = RunSessionBlueprint::from_cli(&cli, resource_options, None);
        let (model, _registration) = faux_model("plan-deny-browser");

        let workflow = blueprint
            .build_workflow(root.path(), options(root.path(), &model, agent.path()))
            .await
            .expect("unrelated deny must not block workflow construction");
        let active = workflow.session.get_active_tool_names();
        assert!(
            active.iter().any(|name| name == "todo"),
            "workflow planner must keep internal Todo under an unrelated deny: {active:?}"
        );
        assert!(
            !active.iter().any(|name| name == "browser"),
            "unrelated deny must still apply: {active:?}"
        );
        if let Some(orchestration) = workflow.orchestration {
            let _ = orchestration.shutdown().await;
        }
        workflow.extension_runtime.shutdown().await;
    }

    /// Construction-time contract: without `force_workflow_runtime`, an
    /// explicit `--exclude-tools todo` deny keeps interactive Main without the
    /// Todo tool (unchanged non-workflow behavior — no guard, no override).
    #[tokio::test]
    async fn non_workflow_exclude_tools_todo_deny_behavior_is_unchanged() {
        let agent = tempfile::tempdir().expect("agent root");
        write_unconfined_sandbox(agent.path());
        let root = tempfile::tempdir().expect("project root");
        write_project(root.path(), "plan", 128);
        let cli = Cli::try_parse_from([
            "rpi",
            "--approve",
            "--exclude-tools",
            "todo",
            "--api-key",
            "launch-secret",
        ])
        .expect("cli");
        let mut resource_options = isolated_resource_options(root.path(), agent.path());
        resource_options.project_trust_override = Some(true);
        let blueprint = RunSessionBlueprint::from_cli(&cli, resource_options, None);
        let (model, _registration) = faux_model("main-exclude-todo");

        let main = blueprint
            .build(root.path(), options(root.path(), &model, agent.path()))
            .await
            .expect("non-workflow build must not be blocked by the deny");
        assert!(
            !main
                .session
                .get_active_tool_names()
                .iter()
                .any(|name| name == "todo"),
            "interactive Main must honor --exclude-tools todo: {:?}",
            main.session.get_active_tool_names()
        );
        if let Some(orchestration) = main.orchestration {
            let _ = orchestration.shutdown().await;
        }
        main.extension_runtime.shutdown().await;
    }

    /// Run git with a hermetic environment (no inherited global config), so the
    /// fixture is deterministic and never touches the user's real git config.
    fn fixture_git(cwd: &Path, args: &[&str]) -> String {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let output = std::process::Command::new("git")
            .env_clear()
            .env("PATH", path)
            .env("HOME", cwd)
            .env("USERPROFILE", cwd)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", cwd.join("absent-global-git-config"))
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_AUTHOR_NAME", "Pi Test")
            .env("GIT_AUTHOR_EMAIL", "pi@example.test")
            .env("GIT_COMMITTER_NAME", "Pi Test")
            .env("GIT_COMMITTER_EMAIL", "pi@example.test")
            .env("LC_ALL", "C")
            .args([
                "-c",
                "color.ui=false",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("fixture git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("utf8")
            .trim()
            .to_owned()
    }

    /// Production-shape regression for `ApplicationWorkflowRuntimeFactory::build_child`:
    /// the real factory builds the workflow child in a git worktree cwd that lives
    /// OUTSIDE the source repository and INSIDE the agent resource root (the
    /// production placement from `workflow_storage_roots`:
    /// `agent_dir/workflow-worktrees/<namespace>/workflow-worktrees/<id>`). The
    /// prior `build_trusted_workflow_application` regression used
    /// `cwd == resource_options` source root, so it never exercised the distinct
    /// worktree cwd. This commits a source repo, creates a real git worktree under
    /// the agent dir, and builds the workflow child there through the production
    /// `build_trusted_workflow_candidate` path (build_workflow + Application
    /// construction), asserting construction succeeds in that distinct cwd.
    #[tokio::test]
    async fn build_workflow_child_in_distinct_worktree_cwd_under_agent_dir_constructs() {
        let agent = tempfile::tempdir().expect("agent root");
        write_unconfined_sandbox(agent.path());
        // Source repo (committed) — the `resource_options` source root.
        let source = tempfile::tempdir().expect("source root");
        write_project(source.path(), "ship", 128);
        fs::write(source.path().join("README.md"), "base\n").expect("base");
        fixture_git(source.path(), &["init"]);
        fixture_git(source.path(), &["config", "user.name", "Pi Test"]);
        fixture_git(source.path(), &["config", "user.email", "pi@example.test"]);
        fixture_git(source.path(), &["add", "README.md"]);
        fixture_git(source.path(), &["commit", "-m", "source seed"]);
        // Production worktree placement under the agent dir (mirrors
        // `workflow_storage_roots` → `agent_dir/workflow-worktrees/<ns>/...`).
        let worktree = agent
            .path()
            .join("workflow-worktrees")
            .join("ns-1")
            .join("workflow-worktrees")
            .join("ship-flow");
        fixture_git(
            source.path(),
            &[
                "worktree",
                "add",
                "--",
                worktree.to_str().expect("utf8"),
                "HEAD",
            ],
        );
        let cli = Cli::try_parse_from([
            "rpi",
            "--approve",
            "--tools",
            "read,task,hub",
            "--api-key",
            "launch-secret",
        ])
        .expect("cli");
        let mut resource_options = isolated_resource_options(source.path(), agent.path());
        resource_options.project_trust_override = Some(true);
        let blueprint = RunSessionBlueprint::from_cli(&cli, resource_options, None);
        let (model, _faux) = faux_model("ship-worktree");
        // Build the workflow child in the DISTINCT worktree cwd (outside the
        // source root, inside the agent dir) — the exact shape
        // `build_trusted_workflow_candidate` runs in production.
        let application = build_trusted_workflow_application(
            &blueprint,
            &worktree,
            &model,
            agent.path(),
        )
        .await
        .expect("workflow child must construct in a distinct git worktree cwd under the agent dir");
        assert_eq!(
            application
                .orchestration_runtime()
                .expect("workflow child orchestration")
                .main_agent_id(),
            "Main"
        );
        application.cleanup().await;
    }

    /// Full production-path regression for the immediate-create blocker: build a
    /// parent `Application` via the production blueprint path (production
    /// `governed_process_context()`, NO hermetic repository executor — exactly
    /// what the real `rpi` uses), attach the blueprint as the runtime factory,
    /// `setup_workflows` over a committed source repo (which builds the worktree
    /// manager under the production governed repository-helper spawn path), then
    /// drive the PUBLIC workflow API the real `rpi /workflow create` uses:
    /// `workflow_create` → `workflow_list` → `workflow_get` → `workflow_cancel` →
    /// `workflow_remove`. The faux supervisor is fed a planning `todo init`
    /// response so it commits a plan and reaches Running (non-terminal) before
    /// cancel — mirroring the in-crate
    /// `real_runtime_resume_cancel_and_remove_lifecycle_is_bounded` lifecycle.
    /// This is the only path that exercises the full `WorkflowManager::create`
    /// reservation + factory `build_child` + worktree creation under the
    /// production governed repository-helper context (the immediate-create
    /// blocker failed here with `InvalidRetainedWorkingDirectory` before the
    /// DangerFullAccess repository-helper spawn guard).
    #[tokio::test]
    async fn production_workflow_create_lifecycle_under_production_governed_context() {
        let agent = tempfile::tempdir().expect("agent root");
        write_unconfined_sandbox(agent.path());
        let source = tempfile::tempdir().expect("source root");
        write_project(source.path(), "ship", 128);
        fs::write(source.path().join("README.md"), "base\n").expect("base");
        fixture_git(source.path(), &["init"]);
        fixture_git(source.path(), &["config", "user.name", "Pi Test"]);
        fixture_git(source.path(), &["config", "user.email", "pi@example.test"]);
        fixture_git(source.path(), &["add", "README.md"]);
        fixture_git(source.path(), &["commit", "-m", "source seed"]);
        let state = tempfile::tempdir().expect("workflow state");
        let managed = tempfile::tempdir().expect("managed worktree root");
        let cli = Cli::try_parse_from([
            "rpi",
            "--approve",
            "--tools",
            "read,task,hub",
            "--api-key",
            "launch-secret",
        ])
        .expect("cli");
        let resource_options = isolated_resource_options(source.path(), agent.path());
        let blueprint = RunSessionBlueprint::from_cli(&cli, resource_options, None);
        let (model, registration) = faux_model("ship-factory");
        // The supervisor's planning turn commits a `todo init` plan (Running),
        // then "planned" ends the turn — the workflow stays non-terminal until
        // cancel (mirroring the in-crate lifecycle fixture).
        registration.set_responses(vec![
            FauxResponse {
                content: vec![ContentBlock::ToolCall(ToolCall {
                    id: "todo-init".to_owned(),
                    name: "todo".to_owned(),
                    arguments: json!({"op":"init","items":["ship the widget"]}),
                    thought_signature: None,
                })],
                stop_reason: StopReason::ToolUse,
                error_message: None,
            },
            FauxResponse::text("planned"),
        ]);
        // Parent Application through the production blueprint path. own_candidate
        // does NOT install a hermetic repository executor, so the workflow
        // factory's worktree creation runs under the production governed
        // repository-helper context — the exact E2E shape.
        let candidate = blueprint
            .build(source.path(), options(source.path(), &model, agent.path()))
            .await
            .expect("parent candidate");
        let application = own_candidate(candidate).await;
        application
            .application
            .attach_runtime_factory(Arc::new(blueprint.clone()))
            .expect("attach runtime factory");
        let _factory = application
            .application
            .setup_workflows(source.path(), state.path(), managed.path())
            .await
            .expect("setup workflows");
        // create: the public `rpi /workflow create` path — WorkflowManager
        // reservation + factory build_child + worktree creation under the
        // production governed repository-helper spawn context.
        let created = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            application.application.workflow_create(pi_coding::WorkflowCreateRequest {
                name: "ship-flow".to_owned(),
                objective: "ship the widget".to_owned(),
            }),
        )
        .await
        .expect("workflow create timed out")
        .expect("production workflow_create must succeed under the production governed repository-helper spawn path (immediate-create blocker regression)");
        let branch = created
            .branch
            .as_deref()
            .expect("workflow_create must record a branch");
        assert!(
            branch.starts_with(pi_coding::workflow_worktree::WORKFLOW_BRANCH_PREFIX),
            "workflow_create must record a workflow branch, got {branch:?}"
        );
        let worktree_label = created
            .worktree_label
            .as_deref()
            .expect("workflow_create must record a worktree label");
        let worktree_path = managed
            .path()
            .join(pi_coding::workflow_worktree::WORKTREE_ROOT_DIR_NAME)
            .join(worktree_label);
        assert!(
            worktree_path.exists(),
            "production workflow_create must materialize the worktree at {worktree_path:?}"
        );
        // list: the manager catalog contains exactly the created workflow
        // (immediate, observable from the returned snapshot — no Waiting).
        assert_eq!(application.application.workflow_list().len(), 1);
        // get (show): the snapshot is retrievable by id with the recorded branch.
        let got = application
            .application
            .workflow_get(&created.workflow_id)
            .expect("workflow_get must return the created workflow");
        assert_eq!(got.workflow_id, created.workflow_id);
        assert_eq!(got.branch.as_deref(), Some(branch));
        // cancel: terminates the workflow at its current cancellable status. The
        // supervisor start() runs in a background task; `run_planning_phase`
        // interrupts a Cancel that arrives mid-planning, so cancel does not
        // require the workflow to reach Running first. On Err/timeout the
        // current status + failure are surfaced as a diagnostic so the exact
        // faux response or lifecycle call can be chosen (no fixed sleep).
        let cancelled = match tokio::time::timeout(
            std::time::Duration::from_secs(10),
            application
                .application
                .workflow_cancel(&created.workflow_id, created.generation),
        )
        .await
        {
            Ok(Ok(snapshot)) => snapshot,
            Ok(Err(error)) => {
                let current = application
                    .application
                    .workflow_get(&created.workflow_id)
                    .expect("workflow_get for cancel diagnostic");
                panic!(
                    "workflow_cancel failed: {error:#}; current status={:?} failure={}",
                    current.status,
                    current
                        .failure
                        .as_ref()
                        .map_or("<none>", |f| f.message.as_str()),
                );
            }
            Err(_elapsed) => {
                let current = application
                    .application
                    .workflow_get(&created.workflow_id)
                    .expect("workflow_get for cancel timeout diagnostic");
                panic!(
                    "workflow_cancel timed out; current status={:?} failure={}",
                    current.status,
                    current
                        .failure
                        .as_ref()
                        .map_or("<none>", |f| f.message.as_str()),
                );
            }
        };
        assert_eq!(cancelled.status, pi_coding::WorkflowStatus::Cancelled);
        assert!(
            worktree_path.exists(),
            "cancel must preserve the owned worktree until explicit remove"
        );
        // remove: destroys the worktree and clears the catalog.
        let removed = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            application
                .application
                .workflow_remove(&created.workflow_id, created.generation),
        )
        .await
        .expect("workflow remove timed out")
        .expect("workflow_remove must destroy the workflow");
        assert_eq!(removed.workflow_id, created.workflow_id);
        assert!(application.application.workflow_list().is_empty());
        assert!(!worktree_path.exists(), "remove must destroy the worktree");
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            application.application.cleanup(),
        )
        .await
        .expect("workflow application cleanup timed out");
    }

    /// Production shape for the authorized-subdirectory contract: the active
    /// session cwd is a committed subdirectory of the repository, the managed
    /// worktree root lives under the agent dir (the production
    /// `workflow_storage_roots` shape), and the public `workflow_create` the
    /// Web command drives must succeed end to end.
    #[tokio::test]
    async fn production_workflow_create_succeeds_for_authorized_subdirectory_source() {
        let agent = tempfile::tempdir().expect("agent root");
        write_unconfined_sandbox(agent.path());
        let source = tempfile::tempdir().expect("source root");
        write_project(source.path(), "ship", 128);
        fs::write(source.path().join("README.md"), "base\n").expect("base");
        fs::create_dir_all(source.path().join("crates/demo")).expect("subdir");
        fs::write(
            source.path().join("crates/demo/lib.rs"),
            "pub fn demo() {}\n",
        )
        .expect("subdir source");
        fixture_git(source.path(), &["init"]);
        fixture_git(source.path(), &["config", "user.name", "Pi Test"]);
        fixture_git(source.path(), &["config", "user.email", "pi@example.test"]);
        fixture_git(source.path(), &["add", "-A"]);
        fixture_git(source.path(), &["commit", "-m", "source seed"]);
        let subdir = source.path().join("crates/demo");
        let state = tempfile::tempdir().expect("workflow state");
        let managed = agent.path().join("workflow-worktrees").join("subdir-ns");
        let cli = Cli::try_parse_from([
            "rpi",
            "--approve",
            "--tools",
            "read,task,hub",
            "--api-key",
            "launch-secret",
        ])
        .expect("cli");
        let resource_options = isolated_resource_options(&subdir, agent.path());
        let blueprint = RunSessionBlueprint::from_cli(&cli, resource_options, None);
        let (model, registration) = faux_model("subdir-factory");
        registration.set_responses(vec![
            FauxResponse {
                content: vec![ContentBlock::ToolCall(ToolCall {
                    id: "todo-init".to_owned(),
                    name: "todo".to_owned(),
                    arguments: json!({"op":"init","items":["port the widget crate"]}),
                    thought_signature: None,
                })],
                stop_reason: StopReason::ToolUse,
                error_message: None,
            },
            FauxResponse::text("planned"),
        ]);
        let candidate = blueprint
            .build(&subdir, options(&subdir, &model, agent.path()))
            .await
            .expect("parent candidate");
        let application = own_candidate(candidate).await;
        application
            .application
            .attach_runtime_factory(Arc::new(blueprint.clone()))
            .expect("attach runtime factory");
        application
            .application
            .setup_workflows(source.path(), state.path(), &managed)
            .await
            .expect("setup workflows");
        let created = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            application.application.workflow_create(pi_coding::WorkflowCreateRequest {
                name: "pi-zig-subdir".to_owned(),
                objective: "port the widget crate".to_owned(),
            }),
        )
        .await
        .expect("workflow create timed out")
        .expect("workflow_create must succeed for an authorized subdirectory source cwd");
        assert!(
            created.branch.is_some(),
            "subdirectory create must record a workflow branch"
        );
        assert_eq!(
            application.application.workflow_list().len(),
            1,
            "the created workflow must be the only catalog entry"
        );
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            application.application.cleanup(),
        )
        .await
        .expect("workflow application cleanup timed out");
    }

    /// Fail-closed production shape: when the trusted workflow child build is
    /// impossible (forced here by `--exclude-tools todo`, which the workflow
    /// runtime refuses), `workflow_create` must fail with the bounded
    /// construction stage/category — no absolute paths, no objective text, no
    /// raw guard text — and must leave the manager catalog empty, publish no
    /// durable record, and roll the reserved worktree back.
    #[tokio::test]
    async fn production_workflow_create_failure_is_bounded_and_leaves_catalog_empty() {
        let agent = tempfile::tempdir().expect("agent root");
        write_unconfined_sandbox(agent.path());
        let source = tempfile::tempdir().expect("source root");
        write_project(source.path(), "ship", 128);
        fs::write(source.path().join("README.md"), "base\n").expect("base");
        fixture_git(source.path(), &["init"]);
        fixture_git(source.path(), &["config", "user.name", "Pi Test"]);
        fixture_git(source.path(), &["config", "user.email", "pi@example.test"]);
        fixture_git(source.path(), &["add", "README.md"]);
        fixture_git(source.path(), &["commit", "-m", "source seed"]);
        let state = tempfile::tempdir().expect("workflow state");
        let managed = tempfile::tempdir().expect("managed worktree root");
        let cli = Cli::try_parse_from([
            "rpi",
            "--approve",
            "--exclude-tools",
            "todo",
            "--tools",
            "read,task,hub",
            "--api-key",
            "launch-secret",
        ])
        .expect("cli");
        let resource_options = isolated_resource_options(source.path(), agent.path());
        let blueprint = RunSessionBlueprint::from_cli(&cli, resource_options, None);
        let (model, registration) = faux_model("fail-closed-factory");
        registration.set_responses(vec![FauxResponse::text("planned")]);
        let candidate = blueprint
            .build(source.path(), options(source.path(), &model, agent.path()))
            .await
            .expect("parent candidate must build without the workflow runtime force");
        let application = own_candidate(candidate).await;
        application
            .application
            .attach_runtime_factory(Arc::new(blueprint.clone()))
            .expect("attach runtime factory");
        application
            .application
            .setup_workflows(source.path(), state.path(), managed.path())
            .await
            .expect("setup workflows");
        let objective = "实现zig版本的pi-coding-agent";
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            application.application.workflow_create(pi_coding::WorkflowCreateRequest {
                name: "pi-zig".to_owned(),
                objective: objective.to_owned(),
            }),
        )
        .await
        .expect("workflow create timed out")
        .expect_err("a forced-invalid child build must fail workflow_create");
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains(
                "workflow runtime creation failed: workflow child runtime construction failed at child-build (todo)",
            ),
            "bounded construction cause must chain through the manager: {rendered}"
        );
        assert!(
            !rendered.contains('/'),
            "no absolute path may leak: {rendered}"
        );
        assert!(
            !rendered.contains(objective),
            "the objective must not leak: {rendered}"
        );
        assert!(
            !rendered.contains("exclude"),
            "raw guard text must not leak: {rendered}"
        );
        assert!(
            application.application.workflow_list().is_empty(),
            "a failed create must leave the manager catalog empty"
        );
        let worktree_root = managed
            .path()
            .join(pi_coding::workflow_worktree::WORKTREE_ROOT_DIR_NAME);
        let worktree_entries = std::fs::read_dir(&worktree_root)
            .map(|entries| entries.count())
            .unwrap_or(0);
        assert_eq!(
            worktree_entries, 0,
            "a failed create must roll the reserved worktree back"
        );
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            application.application.cleanup(),
        )
        .await
        .expect("workflow application cleanup timed out");
    }
}
