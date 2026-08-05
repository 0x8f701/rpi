use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use pi_coding::{
    AgentCatalog, ApplicationRuntimeCandidate, ApplicationRuntimeFactory, ApplicationRuntimeFuture,
    ExtensionMode, ExtensionPermissionSet, ExtensionRuntime, ExtensionRuntimeOptions,
    ExtensionUiHost, GoalToolBinding, InternalUriResolverFn, OrchestrationConfig,
    OrchestrationRuntime, OrchestrationSkill, ResourceManager, ResourceManagerOptions, Session,
    SessionOptions, ToolSelection, WorkspaceRoots,
};

use crate::args::Cli;
use crate::extension_ui::{ExtensionUiAdapter, NonInteractiveExtensionUiHost};
use crate::models_config;

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
    session_dir: Option<PathBuf>,
}

#[derive(Clone)]
struct MainToolPolicy {
    allow: Option<Vec<String>>,
    deny: Vec<String>,
    disable_all: bool,
    disable_builtins: bool,
}

pub(super) struct RunSessionCandidate {
    pub(super) session: Session,
    pub(super) extension_runtime: ExtensionRuntime,
    pub(super) extension_permissions: ExtensionPermissionSet,
    pub(super) orchestration: Option<OrchestrationRuntime>,
    pub(super) goal_tool: Option<GoalToolBinding>,
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
            },
            extension_mode: crate::session_run::extension_mode(cli),
            extension_ui,
            approval_mode_override: cli.approval_mode.map(Into::into),
            extension_permissions: ExtensionPermissionSet::allow_all(),
            explicit_api_key: cli.api_key.clone(),
            session_dir: None,
        }
    }

    pub(super) fn extension_ui(&self) -> Option<ExtensionUiAdapter> {
        self.extension_ui.clone()
    }

    fn approval_mode(&self, settings: &pi_coding::Settings) -> pi_agent::ApprovalMode {
        self.approval_mode_override
            .unwrap_or_else(|| settings.approval_mode())
    }

    pub(super) fn set_session_dir(&mut self, session_dir: PathBuf) {
        self.session_dir = Some(session_dir);
    }

    pub(super) async fn build(
        &self,
        cwd: &Path,
        options: SessionOptions,
    ) -> Result<RunSessionCandidate> {
        let workspace = WorkspaceRoots::new(cwd, &self.add_dirs)?;
        let cwd = workspace.cwd().to_path_buf();
        let resources = ResourceManager::new(self.resource_options_for_startup(&cwd)?)
            .context("loading settings and resources")?;
        let settings = resources.snapshot().settings.clone();
        let permissions = self.extension_permissions.clone();
        let runtime = self.extension_runtime();
        let specs = resources
            .extension_specs(&permissions)
            .context("validating configured extensions")?;
        let report = runtime.load(specs).await;
        if !report.failures.is_empty() {
            runtime.shutdown().await;
            return Err(crate::session_run::extension_startup_error(&report));
        }

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
        let (session, orchestration, goal_tool) = match build_result {
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
        })
    }

    async fn build_workflow(
        &self,
        cwd: &Path,
        options: SessionOptions,
    ) -> Result<RunSessionCandidate> {
        let workspace = WorkspaceRoots::new(cwd, &self.add_dirs)?;
        let cwd = workspace.cwd().to_path_buf();
        let resources = ResourceManager::new(self.resource_options_for_startup(&cwd)?)
            .context("loading settings and resources")?;
        let settings = resources.snapshot().settings.clone();
        let permissions = self.extension_permissions.clone();
        let runtime = self.extension_runtime();
        let specs = resources.extension_specs(&permissions)
            .context("validating configured extensions")?;
        let report = runtime.load(specs).await;
        if !report.failures.is_empty() {
            runtime.shutdown().await;
            return Err(crate::session_run::extension_startup_error(&report));
        }
        let built = self.build_session(
            cwd,
            workspace,
            resources,
            settings,
            runtime.clone(),
            options,
            true,
        ).await;
        let (session, orchestration, goal_tool) = match built {
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
        })
    }


    pub(super) fn resource_options_for_startup(&self, cwd: &Path) -> Result<ResourceManagerOptions> {
        let mut options = self.resource_options.clone();
        options.cwd = cwd.to_path_buf();
        options.system_prompt = None;
        options.system_prompt_path = None;
        options.append_system_prompt.clear();
        options.append_system_prompt_paths.clear();
        if let Some(input) = self.system_prompt_input.as_deref() {
            let (prompt, path) = crate::session_run::resolve_prompt_input(
                input,
                cwd,
                "system prompt",
            )?;
            options.system_prompt = Some(prompt);
            options.system_prompt_path = path;
        }
        for input in &self.append_system_prompt_inputs {
            let (prompt, path) = crate::session_run::resolve_prompt_input(
                input,
                cwd,
                "append system prompt",
            )?;
            options.append_system_prompt.push(prompt);
            options.append_system_prompt_paths.push(path);
        }
        Ok(options)
    }

    fn extension_runtime(&self) -> ExtensionRuntime {
        let ui_host: Option<Arc<dyn ExtensionUiHost>> = self
            .extension_ui
            .as_ref()
            .map(|adapter| Arc::new(adapter.clone()) as Arc<dyn ExtensionUiHost>)
            .or_else(|| {
                Some(Arc::new(NonInteractiveExtensionUiHost::default())
                    as Arc<dyn ExtensionUiHost>)
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
    ) -> Result<(Session, Option<OrchestrationRuntime>, Option<GoalToolBinding>)> {
        options.cwd.clone_from(&cwd);
        options.system_prompt = resources.snapshot().system_prompt.clone().unwrap_or_default();
        options.auth_resolver = Some(models_config::session_auth_resolver(self.explicit_api_key.clone()));
        settings.apply_session_options(&mut options)?;
        let approval_mode = self.approval_mode(&settings);
        options.before_tool_call = Some(crate::approval::host_approval_before_tool_call(
            approval_mode,
            self.extension_mode,
            self.extension_ui.clone(),
            options.before_tool_call.take(),
        ));
        let mut gates = self.tool_gates(&settings);
        if force_workflow_runtime {
            gates.orchestration = true;
            gates.todo = true;
        }
        let (orchestration, uri_resolver) = if gates.orchestration {
            let snapshot = resources.snapshot();
            let config = orchestration_config(&snapshot, &settings, &options.model, force_workflow_runtime);
            let resolver_slot = Arc::new(parking_lot::Mutex::new(None::<InternalUriResolverFn>));
            let child_resolver_slot = resolver_slot.clone();
            let uri_resolver: InternalUriResolverFn = Arc::new(move |uri| {
                child_resolver_slot
                    .lock()
                    .as_ref()
                    .ok_or_else(|| anyhow!("orchestration URI resolver is not initialized"))?(uri)
            });
            let factory = OrchestrationRuntime::child_factory_from_snapshot_and_uri(
                pi_coding::ChildSessionOptionsSnapshot {
                    model: options.model.clone(),
                    cwd: cwd.clone(),
                    thinking_level: options.thinking_level,
                    api_key: options.api_key.clone(),
                    stream_options: options.stream_options.clone(),
                    stream_fn: options
                        .stream_fn
                        .clone()
                        .unwrap_or_else(|| pi_agent::AgentOptions::default().stream_fn),
                    auth_resolver: options.auth_resolver.clone(),
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
        let goal_tool = (!self.tool_policy.disable_all).then(GoalToolBinding::default);
        if let Some(binding) = &goal_tool {
            additional_tools.push(binding.tool());
        }
        let selection = ToolSelection {
            allow: self.tool_policy.allow.clone(),
            deny: self.tool_policy.deny.clone(),
            disable_all: self.tool_policy.disable_all,
            disable_builtins: self.tool_policy.disable_builtins,
            enable_process: gates.process,
            enable_glob: gates.glob,
        };
        let session = if gates.todo {
            Session::new_with_todo_additional_tools_filtered_discovery_workspace_and_uri(
                options,
                additional_tools,
                selection,
                pi_coding::ResourceDiscovery::Disabled,
                workspace,
                uri_resolver,
            )
        } else {
            Session::new_with_additional_tools_filtered_discovery_workspace_and_uri(
                options,
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
                if let Some(orchestration) = orchestration {
                    orchestration.shutdown().await;
                }
                return Err(error).context("building session");
            }
        };
        if let Some(session_dir) = &self.session_dir {
            session.set_session_dir(session_dir.clone());
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
        Ok((session, orchestration, gates.goal.then_some(goal_tool).flatten()))
    }

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
            } = candidate;
            if let Some(resume) = resume {
                let context = resume.build_context();
                let recorder = resume.into_recorder()?;
                session.load_history(context.messages).await?;
                if let Some(provider) = context.provider.as_deref()
                    && let Some(model_id) = context.model_id.as_deref()
                    && let Some(model) = pi_ai::get_model(provider, model_id)
                {
                    session.set_model_with_resolved_auth(model).await?;
                }
                session.record(recorder)?;
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
        (gates.orchestration, gates.process, gates.glob, gates.todo, gates.goal)
    }
    pub(super) fn test_approval_mode(
        &self,
        settings: &pi_coding::Settings,
    ) -> pi_agent::ApprovalMode {
        self.approval_mode(settings)
    }
}

#[cfg(test)]
pub(super) fn test_orchestration_config(
    snapshot: &pi_coding::ResourceSnapshot,
    settings: &pi_coding::Settings,
    parent_model: &pi_ai::Model,
) -> OrchestrationConfig {
    orchestration_config(snapshot, settings, parent_model, false)
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
        snapshot.cwd.join(".pi").join("artifacts")
    };
    let mut config = OrchestrationConfig::new(
        AgentCatalog::from_agents(snapshot.agents.clone()),
        artifact_dir,
    );
    config.skills = snapshot.skills.iter().map(OrchestrationSkill::from).collect();
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
            config.max_tools_per_agent = value;
        }
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
    use pi_ai::{Model, SimpleStreamOptions};
    use serde_json::json;
    use std::fs;

    fn options(cwd: &Path, model: &Model) -> SessionOptions {
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
        }
    }

    fn write_project(root: &Path, label: &str, max_tokens: i64) {
        let project = root.join(".pi");
        fs::create_dir_all(project.join("agents")).expect("agents directory");
        fs::create_dir_all(project.join("skills")).expect("skills directory");
        fs::write(
            project.join("settings.json"),
            format!(
                r#"{{"maxTokens":{max_tokens},"orchestration":{{"tasks":true,"process":true,"glob":true}}}}"#,
            ),
        )
        .expect("settings");
        fs::write(root.join("AGENTS.md"), format!("{label} project context"))
            .expect("context");
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
        let cwd = std::env::current_dir().expect("cwd");
        let base = |args: &[&str]| {
            let cli = Cli::try_parse_from(args).expect("cli");
            RunSessionBlueprint::from_cli(&cli, ResourceManagerOptions::new(&cwd), None)
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
    async fn blueprint_rebuilds_isolated_cwd_resources_tools_and_orchestration() {
        let agent = tempfile::tempdir().expect("agent root");
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
        resource_options.project_trust_override = Some(true);
        let blueprint = RunSessionBlueprint::from_cli(&cli, resource_options, None);
        let cloned = blueprint.clone();
        let model = Model {
            reasoning: true,
            provider: "launch-provider".to_owned(),
            id: "launch-model".to_owned(),
            ..Model::default()
        };

        let candidate_a = blueprint
            .build(root_a.path(), options(root_a.path(), &model))
            .await
            .expect("candidate A");
        let candidate_b = cloned
            .build(root_b.path(), options(root_b.path(), &model))
            .await
            .expect("candidate B");

        assert_eq!(candidate_a.session.cwd(), root_a.path().canonicalize().expect("root A"));
        assert_eq!(candidate_b.session.cwd(), root_b.path().canonicalize().expect("root B"));
        assert_eq!(
            candidate_b.session.workspace_roots().additional_roots(),
            [add_dir.path().canonicalize().expect("add dir")]
        );
        assert_eq!(candidate_b.session.model().as_ref(), Some(&model));
        assert_eq!(candidate_b.session.thinking_level(), ThinkingLevel::High);
        assert_eq!(candidate_b.session.current_api_key(), "launch-secret");
        assert_eq!(candidate_a.session.stream_options().stream.max_tokens, Some(111));
        assert_eq!(candidate_b.session.stream_options().stream.max_tokens, Some(222));
        assert_ne!(
            candidate_a.session.process_owner_id(),
            candidate_b.session.process_owner_id(),
        );

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
        assert_eq!(snapshot_a.cwd, root_a.path().canonicalize().expect("root A"));
        assert_eq!(snapshot_b.cwd, root_b.path().canonicalize().expect("root B"));
        assert!(snapshot_a.context_files.iter().all(|file| !file.content.contains("beta")));
        assert!(snapshot_b.context_files.iter().all(|file| !file.content.contains("alpha")));
        assert!(snapshot_a.agents.iter().any(|agent| agent.name == "alpha"));
        assert!(!snapshot_a.agents.iter().any(|agent| agent.name == "beta"));
        assert!(snapshot_b.agents.iter().any(|agent| agent.name == "beta"));
        assert!(!snapshot_b.agents.iter().any(|agent| agent.name == "alpha"));
        assert!(snapshot_a.skills.iter().any(|skill| skill.name == "alpha-skill"));
        assert!(!snapshot_a.skills.iter().any(|skill| skill.name == "beta-skill"));
        assert!(snapshot_b.skills.iter().any(|skill| skill.name == "beta-skill"));
        assert!(!snapshot_b.skills.iter().any(|skill| skill.name == "alpha-skill"));

        assert_eq!(
            candidate_a.session.get_active_tool_names(),
            ["read", "task", "hub", "process", "glob"]
        );
        assert_eq!(
            candidate_b.session.get_active_tool_names(),
            ["read", "task", "hub", "process", "glob"]
        );
        let read_a = candidate_a.session.get_tool_definition("read").expect("read A");
        let read_b = candidate_b.session.get_tool_definition("read").expect("read B");
        assert!(read_tool(&read_a, "AGENTS.md").await.expect("read A").contains("alpha"));
        assert!(read_tool(&read_b, "AGENTS.md").await.expect("read B").contains("beta"));

        let orchestration_a = candidate_a.orchestration.as_ref().expect("orchestration A");
        let orchestration_b = candidate_b.orchestration.as_ref().expect("orchestration B");
        assert!(orchestration_a.catalog().get("alpha").is_some());
        assert!(orchestration_a.catalog().get("beta").is_none());
        assert!(orchestration_b.catalog().get("beta").is_some());
        assert!(orchestration_b.catalog().get("alpha").is_none());
        assert!(orchestration_a.skills().iter().any(|skill| skill.name == "alpha-skill"));
        assert!(orchestration_b.skills().iter().any(|skill| skill.name == "beta-skill"));
        assert_eq!(
            orchestration_a.read_uri_resolver()("artifact://missing")
                .expect_err("missing A artifact")
                .to_string()
                .contains(root_b.path().to_string_lossy().as_ref()),
            false,
        );
        assert_eq!(
            orchestration_b.read_uri_resolver()("artifact://missing")
                .expect_err("missing B artifact")
                .to_string()
                .contains(root_a.path().to_string_lossy().as_ref()),
            false,
        );

        if let Some(runtime) = candidate_a.orchestration {
            runtime.shutdown().await;
        }

        candidate_a.extension_runtime.shutdown().await;
        if let Some(runtime) = candidate_b.orchestration {
            runtime.shutdown().await;
        }
        candidate_b.extension_runtime.shutdown().await;
    }

    #[test]
    fn workflow_orchestration_artifacts_stay_outside_worktree() {
        let root = tempfile::tempdir().expect("root");
        let cwd = root.path().join("workflow-worktrees").join("workflow-id");
        let snapshot = pi_coding::ResourceSnapshot {
            generation: 1,
            cwd: cwd.clone(),
            trust: pi_coding::TrustResolution {
                decision: pi_coding::TrustDecision::Trusted,
                matched_path: None,
                project_path: cwd,
            },
            settings: pi_coding::Settings::default(),
            context_files: Vec::new(),
            skills: Vec::new(),
            agents: Vec::new(),
            prompts: Vec::new(),
            themes: Vec::new(),
            package_extensions: Vec::new(),
            theme_dirs: Vec::new(),
            keybinding_files: Vec::new(),
            system_prompt: None,
            append_system_prompt: Vec::new(),
            diagnostics: Vec::new(),
        };
        let config = orchestration_config(
            &snapshot,
            &pi_coding::Settings::default(),
            &pi_ai::Model::default(),
            true,
        );
        assert!(!config.artifact_dir.starts_with(&snapshot.cwd));
        assert!(config.artifact_dir.ends_with("workflow-artifacts"));
    }
}
