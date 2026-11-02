use super::*;
use pi_agent::ThinkingLevel;
use pi_ai::{
    ContentBlock, Model, StopReason, ToolCall,
    providers::{
        FauxProviderOptions, FauxProviderRegistration, FauxResponse, register_faux_provider,
    },
};
use pi_coding::{
    AgentCatalog, AgentDefinition, AgentDefinitionKind, AgentDefinitionSource, Application,
    ApplicationRuntimeCandidate, ApplicationRuntimeFactory, ApplicationRuntimeFuture,
    OrchestrationConfig, OrchestrationRuntime, Session, SessionOptions, TodoState, TodoStorage,
    WorkflowFailure, WorkspaceRoots,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[test]
fn primary_parse_contract_covers_all_subcommands() {
    assert_eq!(
        parse_interactive_workflow_command(None).unwrap(),
        InteractiveWorkflowCommand::OpenPage
    );
    assert_eq!(
        parse_interactive_workflow_command(Some("")).unwrap(),
        InteractiveWorkflowCommand::OpenPage
    );
    assert_eq!(
        parse_interactive_workflow_command(Some("   ")).unwrap(),
        InteractiveWorkflowCommand::OpenPage
    );
    assert_eq!(
        parse_interactive_workflow_command(Some("list")).unwrap(),
        InteractiveWorkflowCommand::List
    );
    assert_eq!(
        parse_interactive_workflow_command(Some("show")).unwrap(),
        InteractiveWorkflowCommand::Show { selector: None }
    );
    assert_eq!(
        parse_interactive_workflow_command(Some("show wf-1")).unwrap(),
        InteractiveWorkflowCommand::Show {
            selector: Some(WorkflowSelector("wf-1".into())),
        }
    );
    assert_eq!(
        parse_interactive_workflow_command(Some("show release")).unwrap(),
        InteractiveWorkflowCommand::Show {
            selector: Some(WorkflowSelector("release".into())),
        }
    );
    assert_eq!(
        parse_interactive_workflow_command(Some(
            r#"create "ship it" "land the multi workflow foundation""#
        ))
        .unwrap(),
        InteractiveWorkflowCommand::Create {
            name: "ship it".into(),
            objective: "land the multi workflow foundation".into(),
        }
    );
    assert_eq!(
        parse_interactive_workflow_command(Some("create release land the feature")).unwrap(),
        InteractiveWorkflowCommand::Create {
            name: "release".into(),
            objective: "land the feature".into(),
        }
    );

    assert_eq!(
        parse_interactive_workflow_command(Some("create 调研并且撰写zig版本的pi-coding-agent"))
            .unwrap(),
        InteractiveWorkflowCommand::Create {
            name: "调研并且撰写zig版本的pi-coding-agent".into(),
            objective: "调研并且撰写zig版本的pi-coding-agent".into(),
        }
    );
    assert_eq!(
        parse_interactive_workflow_command(Some(
            "create zig-agent 调研并且撰写zig版本的pi-coding-agent"
        ))
        .unwrap(),
        InteractiveWorkflowCommand::Create {
            name: "zig-agent".into(),
            objective: "调研并且撰写zig版本的pi-coding-agent".into(),
        }
    );
    assert_eq!(
        parse_interactive_workflow_command(Some(
            "create moonbit-agent 制作moonbit版本的pi-coding-agent"
        ))
        .unwrap(),
        InteractiveWorkflowCommand::Create {
            name: "moonbit-agent".into(),
            objective: "制作moonbit版本的pi-coding-agent".into(),
        }
    );
    assert_eq!(
        parse_interactive_workflow_command(Some("create 制作moonbit版本的pi-coding-agent"))
            .unwrap(),
        InteractiveWorkflowCommand::Create {
            name: "制作moonbit版本的pi-coding-agent".into(),
            objective: "制作moonbit版本的pi-coding-agent".into(),
        }
    );
    assert_eq!(
        parse_interactive_workflow_command(Some("pause")).unwrap(),
        InteractiveWorkflowCommand::Pause { selector: None }
    );
    assert_eq!(
        parse_interactive_workflow_command(Some("pause wf-9")).unwrap(),
        InteractiveWorkflowCommand::Pause {
            selector: Some(WorkflowSelector("wf-9".into())),
        }
    );
    assert_eq!(
        parse_interactive_workflow_command(Some("resume release")).unwrap(),
        InteractiveWorkflowCommand::Resume {
            selector: Some(WorkflowSelector("release".into())),
        }
    );
    assert_eq!(
        parse_interactive_workflow_command(Some("cancel")).unwrap(),
        InteractiveWorkflowCommand::Cancel { selector: None }
    );
    assert_eq!(
        parse_interactive_workflow_command(Some("integrate wf-2")).unwrap(),
        InteractiveWorkflowCommand::Integrate {
            selector: Some(WorkflowSelector("wf-2".into())),
        }
    );
    assert_eq!(
        parse_interactive_workflow_command(Some("remove old")).unwrap(),
        InteractiveWorkflowCommand::Remove {
            selector: Some(WorkflowSelector("old".into())),
        }
    );
}

#[test]
fn rejects_unknown_and_malformed_subcommands_with_usage() {
    let err = parse_interactive_workflow_command(Some("workfloww")).unwrap_err();
    assert!(
        err.to_string().contains("unknown workflow subcommand"),
        "{err:#}"
    );
    assert!(err.to_string().contains(WORKFLOW_USAGE), "{err:#}");

    let err = parse_interactive_workflow_command(Some("list extra")).unwrap_err();
    assert!(err.to_string().contains("unexpected argument"), "{err:#}");

    let err = parse_interactive_workflow_command(Some("show a b")).unwrap_err();
    assert!(err.to_string().contains("unexpected argument"), "{err:#}");

    assert_eq!(
        parse_interactive_workflow_command(Some("create only-name")).unwrap(),
        InteractiveWorkflowCommand::Create {
            name: "only-name".into(),
            objective: "only-name".into(),
        }
    );

    assert!(
        parse_interactive_workflow_command(Some("create"))
            .unwrap_err()
            .to_string()
            .contains("create requires <objective> or <name> <objective>")
    );

    assert_eq!(
        parse_interactive_workflow_command(Some("create \"\" objective")).unwrap(),
        InteractiveWorkflowCommand::Create {
            name: "objective".into(),
            objective: "objective".into(),
        }
    );
}

#[test]
fn quoted_args_preserve_spaces_via_shared_parser() {
    let command =
        parse_interactive_workflow_command(Some("create 'alpha beta' \"objective with spaces\""))
            .unwrap();
    assert_eq!(
        command,
        InteractiveWorkflowCommand::Create {
            name: "alpha beta".into(),
            objective: "objective with spaces".into(),
        }
    );
}

#[test]
fn status_wire_strings_are_exact_and_complete() {
    let expected = [
        "queued",
        "planning",
        "running",
        "paused",
        "integrating",
        "completed",
        "failed",
        "cancelled",
        "conflicted",
    ];
    for value in expected {
        let status = parse_workflow_status(value).unwrap();
        assert_eq!(status.as_str(), value);
    }
    assert!(parse_workflow_status("done").is_err());
    assert!(WorkflowStatus::Running.is_active());
    assert!(!WorkflowStatus::Completed.is_active());
    assert!(WorkflowStatus::Completed.is_terminal());
    assert!(!WorkflowStatus::Failed.is_active());
    assert!(!WorkflowStatus::Cancelled.is_active());
    assert!(!WorkflowStatus::Conflicted.is_active());
}

#[test]
fn human_formatting_is_english_only() {
    let items = vec![
        summary("wf-1", "release", "ship it", WorkflowStatus::Running),
        summary("wf-2", "docs", "write docs", WorkflowStatus::Completed),
    ];
    assert_eq!(
        format_workflows_header(1, 2),
        "Workflows · 1 active · 2 total"
    );
    assert_eq!(count_active_workflows(&items), 1);
    let list = format_workflow_list(&items);
    assert!(list.starts_with("Workflows · 1 active · 2 total\n"));
    assert!(list.contains("running · release · wf-1 · ship it"));
    assert!(list.contains("completed · docs · wf-2 · write docs"));
    assert_eq!(format_workflow_list(&[]), "No workflows.");

    let detail = format_workflow_detail(&WorkflowSummary {
        worktree_path: Some("workspaces/wf-1".into()),
        branch: Some("rpi/workflow/release".into()),
        supervisor_agent_id: Some("sup-1".into()),
        failure: Some("merge conflict".into()),
        integration: Some("conflicted · a.rs".into()),
        ..summary("wf-1", "release", "ship it", WorkflowStatus::Conflicted)
    });
    assert!(detail.contains("Name: release"));
    assert!(detail.contains("Status: conflicted"));
    assert!(detail.contains("Worktree: wf-1"));
    assert!(detail.contains("Branch: rpi/workflow/release"));
    assert!(detail.contains("Supervisor: sup-1"));
    assert!(detail.contains("Failure: merge conflict"));
    assert!(detail.contains("Integration: conflicted · a.rs"));
    for label in [
        "Name:",
        "Id:",
        "Status:",
        "Objective:",
        "Worktree:",
        "Branch:",
        "Supervisor:",
        "Failure:",
        "Integration:",
    ] {
        assert!(detail.contains(label), "missing English label {label}");
    }
}

#[test]
fn snapshot_projection_is_lossless_for_cli_fields() {
    let snapshot = WorkflowSnapshot {
        workflow_id: WorkflowId::new("wf-9"),
        name: "release".into(),
        objective: "ship".into(),
        status: WorkflowStatus::Running,
        created_at_ms: 1,
        updated_at_ms: 2,
        generation: 4,
        todo: TodoState {
            phases: Vec::new(),
            storage: TodoStorage::Session,
        },
        durable_policy: crate::test_support::test_durable_policy_state(),
        worktree_label: Some("workspaces/wf-9".into()),
        branch: Some("rpi/workflow/wf-9".into()),
        supervisor_agent_id: Some("sup".into()),
        supervisor_job_id: Some("job".into()),
        failure: Some(WorkflowFailure {
            message: "boom".into(),
        }),

        integration: WorkflowIntegration::Conflicted {
            conflicts: vec!["a.rs".into()],
        },
        planning_prompts_consumed: 0,


    };
    let summary = WorkflowSummary::from(&snapshot);
    assert_eq!(summary.id.as_str(), "wf-9");
    assert_eq!(summary.name, "release");
    assert_eq!(summary.objective, "ship");
    assert_eq!(summary.status, WorkflowStatus::Running);
    assert_eq!(summary.generation, 4);
    assert_eq!(summary.worktree_path.as_deref(), Some("workspaces/wf-9"));
    assert_eq!(summary.branch.as_deref(), Some("rpi/workflow/wf-9"));
    assert_eq!(summary.supervisor_agent_id.as_deref(), Some("sup"));
    assert_eq!(summary.failure.as_deref(), Some("boom"));
    assert_eq!(summary.integration.as_deref(), Some("conflicted · a.rs"));
}

struct UniqueFauxRegistration {
    registration: FauxProviderRegistration,
    model_key: (String, String),
}

impl UniqueFauxRegistration {
    fn register(responses: Vec<FauxResponse>) -> (Self, Model) {
        let suffix = uuid::Uuid::now_v7();
        let mut model = Model::default();
        model.id = format!("workflow-cli-{suffix}");
        model.name = "Workflow CLI Test".into();
        model.api = format!("workflow-cli-api-{suffix}");
        model.provider = format!("workflow-cli-provider-{suffix}");
        model.base_url = "http://localhost:0".into();
        let registration = register_faux_provider(FauxProviderOptions {
            api: model.api.clone(),
            provider: model.provider.clone(),
            models: vec![model.clone()],
            chunk_size: 32,
        });
        registration.set_responses(responses);
        let model_key = (model.provider.clone(), model.id.clone());
        (
            Self {
                registration,
                model_key,
            },
            model,
        )
    }
}

impl Drop for UniqueFauxRegistration {
    fn drop(&mut self) {
        self.registration.unregister();
        pi_ai::replace_registered_models(&[self.model_key.clone()], Vec::new());
    }
}

struct ApplicationCleanup {
    application: Option<Application>,
}

impl ApplicationCleanup {
    fn new(application: Application) -> Self {
        Self {
            application: Some(application),
        }
    }

    fn get(&self) -> &Application {
        self.application.as_ref().expect("application")
    }

    async fn cleanup(&mut self) {
        if let Some(application) = self.application.take() {
            application.cleanup().await;
        }
    }
}

impl Drop for ApplicationCleanup {
    fn drop(&mut self) {
        let Some(application) = self.application.take() else {
            return;
        };
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                application.cleanup().await;
            });
        } else {
            std::thread::spawn(move || {
                if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    runtime.block_on(application.cleanup());
                }
            });
        }
    }
}

fn planning(objective: &str) -> FauxResponse {
    FauxResponse {
        content: vec![ContentBlock::ToolCall(ToolCall {
            id: "todo-init".into(),
            name: "todo".into(),
            arguments: serde_json::json!({"op": "init", "items": [objective]}),
            thought_signature: None,
        })],
        stop_reason: StopReason::ToolUse,
        error_message: None,
    }
}

fn todo_is_committed(snapshot: &pi_coding::WorkflowSnapshot) -> bool {
    snapshot
        .todo
        .phases
        .iter()
        .any(|phase| !phase.tasks.is_empty())
}

async fn wait_for_committed_todo(
    port: &ManagerWorkflowPort,
    id: &str,
) -> pi_coding::WorkflowSnapshot {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let snapshot = port.manager().get(&WorkflowId::new(id)).expect("workflow");
            if snapshot.status == WorkflowStatus::Running && todo_is_committed(&snapshot) {
                return snapshot;
            }
            if snapshot.status.is_terminal() {
                panic!(
                    "workflow settled terminally before committing a Todo: {:?}",
                    snapshot.status
                );
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("committed nonempty Todo timeout")
}

#[tokio::test]
async fn adapter_executes_through_real_manager_lifecycle() {
    let root = tempfile::tempdir().expect("tempdir");
    let source = root.path().join("source");
    std::fs::create_dir(&source).expect("source");
    let agent_dir = root.path().join("agent");
    std::fs::create_dir(&agent_dir).expect("agent dir");
    std::fs::write(
        agent_dir.join("settings.json"),
        r#"{
            "sandbox": {"mode": "danger-full-access"},
            "orchestration": {"isolation": "none"}
        }"#,
    )
    .expect("sandbox settings");
    let session_dir = root.path().join("sessions");
    std::fs::create_dir(&session_dir).expect("session dir");
    let objective = "land multi workflow";
    let (_faux, model) =
        UniqueFauxRegistration::register(vec![planning(objective), FauxResponse::text("planned")]);
    let workspace = WorkspaceRoots::new(&source, Vec::<PathBuf>::new()).expect("workspace");
    let session = Session::new_with_todo_additional_tools_filtered_discovery_workspace_and_uri(
        SessionOptions {
            model,
            cwd: source.clone(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: "faux".into(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
            auth_manager: None,
        },
        Vec::new(),
        Default::default(),
        pi_coding::ResourceDiscovery::Disabled,
        workspace,
        None,
        true,
    )
    .expect("session");
    session.set_session_dir(session_dir);
    let mut resource_options = pi_coding::ResourceManagerOptions::new(&source);
    resource_options.agent_dir = agent_dir.clone();
    resource_options.project_trust_override = Some(true);
    resource_options.disable_extensions = true;
    resource_options.disable_skills = true;
    resource_options.disable_prompt_templates = true;
    resource_options.disable_themes = true;
    resource_options.disable_context_files = true;
    let resources = pi_coding::ResourceManager::new(resource_options).expect("resources");
    session
        .attach_resources(resources.clone())
        .await
        .expect("attach resources");
    let snapshot = session.child_session_options_snapshot();
    let mut application = ApplicationCleanup::new(
        Application::from_runtime_candidate(ApplicationRuntimeCandidate::new(session))
            .await
            .expect("publish application"),
    );
    application
        .get()
        .attach_runtime_factory(Arc::new(TestApplicationFactory {
            snapshot,
            agent_dir: agent_dir.clone(),
        }))
        .expect("runtime factory");
    application
        .get()
        .setup_workflows(
            &source,
            root.path().join("state"),
            root.path().join("managed"),
        )
        .await
        .expect("workflow setup");
    let port = application_workflow_port(application.get()).expect("workflow port");

    let effect = execute_interactive_workflow_command(
        &port,
        parse_interactive_workflow_command(None).unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(effect, WorkflowCommandEffect::OpenPage);

    let effect = execute_interactive_workflow_command(
        &port,
        parse_interactive_workflow_command(Some(r#"create "ship it" "land multi workflow""#))
            .unwrap(),
    )
    .await
    .unwrap();
    let WorkflowCommandEffect::Message(created) = effect else {
        panic!("expected message");
    };
    assert!(created.contains("Name: ship it"));
    assert!(created.contains("Status: queued"));
    assert!(created.contains("Objective: land multi workflow"));
    assert!(created.contains("Branch: rpi/none/"));

    let effect = execute_interactive_workflow_command(
        &port,
        parse_interactive_workflow_command(Some("list")).unwrap(),
    )
    .await
    .unwrap();
    let WorkflowCommandEffect::Message(list) = effect else {
        panic!("expected message");
    };
    assert!(list.contains("Workflows · 1 active · 1 total"));
    assert!(list.contains("ship it"));

    let id = port.list().await.unwrap()[0].id.as_str().to_owned();
    let effect = execute_interactive_workflow_command(
        &port,
        parse_interactive_workflow_command(Some(&format!("show {id}"))).unwrap(),
    )
    .await
    .unwrap();
    assert!(matches!(effect, WorkflowCommandEffect::Message(message) if message.contains(&id)));

    let committed = wait_for_committed_todo(&port, &id).await;
    assert_eq!(committed.status, WorkflowStatus::Running);
    assert_eq!(
        committed.todo.phases[0].tasks[0].content,
        "land multi workflow"
    );

    let effect = execute_interactive_workflow_command(
        &port,
        parse_interactive_workflow_command(Some(&format!("pause {id}"))).unwrap(),
    )
    .await
    .unwrap();
    assert!(
        matches!(effect, WorkflowCommandEffect::Message(message) if message.contains("paused"))
    );

    let effect = execute_interactive_workflow_command(
        &port,
        parse_interactive_workflow_command(Some(&format!("resume {id}"))).unwrap(),
    )
    .await
    .unwrap();
    let WorkflowCommandEffect::Message(resumed) = effect else {
        panic!("expected message");
    };
    assert!(
        resumed.contains("running"),
        "committed nonempty Todo must resume as running, got {resumed}"
    );
    assert!(
        !resumed.contains("planning"),
        "planning is unacceptable once a Todo exists, got {resumed}"
    );
    let resumed = port
        .manager()
        .get(&WorkflowId::new(&id))
        .expect("resumed workflow");
    assert_eq!(resumed.status, WorkflowStatus::Running);
    assert!(
        todo_is_committed(&resumed),
        "resume must keep the committed nonempty Todo"
    );

    let effect = execute_interactive_workflow_command(
        &port,
        parse_interactive_workflow_command(Some(&format!("cancel {id}"))).unwrap(),
    )
    .await
    .unwrap();
    assert!(
        matches!(effect, WorkflowCommandEffect::Message(message) if message.contains("cancelled"))
    );

    let effect = execute_interactive_workflow_command(
        &port,
        parse_interactive_workflow_command(Some(&format!("remove {id}"))).unwrap(),
    )
    .await
    .unwrap();
    let WorkflowCommandEffect::Message(message) = effect else {
        panic!("expected message");
    };
    assert!(message.starts_with("removed "));
    assert!(port.list().await.unwrap().is_empty());
    application.cleanup().await;
}

fn summary(id: &str, name: &str, objective: &str, status: WorkflowStatus) -> WorkflowSummary {
    WorkflowSummary {
        id: WorkflowId::new(id),
        name: name.into(),
        objective: objective.into(),
        status,
        worktree_path: None,
        branch: None,
        supervisor_agent_id: None,
        generation: 1,
        failure: None,
        integration: None,
    }
}

#[derive(Clone)]
struct TestApplicationFactory {
    snapshot: pi_coding::ChildSessionOptionsSnapshot,
    agent_dir: PathBuf,
}

impl ApplicationRuntimeFactory for TestApplicationFactory {
    fn build_runtime_candidate(
        &self,
        _: PathBuf,
        _: SessionOptions,
        _: Option<pi_coding::PreparedSessionResume>,
    ) -> ApplicationRuntimeFuture {
        Box::pin(async { anyhow::bail!("unused") })
    }

    fn build_trusted_workflow_candidate(
        &self,
        cwd: pi_coding::workflow_worktree::TrustedWorkflowCwd,
        mut options: SessionOptions,
    ) -> ApplicationRuntimeFuture {
        let snapshot = self.snapshot.clone();
        let agent_dir = self.agent_dir.clone();
        Box::pin(async move {
            options.cwd = cwd.path().to_path_buf();
            options.tools = Some(Vec::new());
            let workspace = WorkspaceRoots::new(cwd.path(), Vec::<PathBuf>::new())?;
            let session =
                Session::new_with_todo_additional_tools_filtered_discovery_workspace_and_uri(
                    options,
                    Vec::new(),
                    Default::default(),
                    pi_coding::ResourceDiscovery::Disabled,
                    workspace,
                    None,
                    true,
                )?;
            std::fs::create_dir_all(&agent_dir)?;
            std::fs::write(
                agent_dir.join("settings.json"),
                r#"{"sandbox":{"mode":"danger-full-access"}}"#,
            )?;
            let mut resource_options = pi_coding::ResourceManagerOptions::new(cwd.path());
            resource_options.agent_dir = agent_dir;
            resource_options.project_trust_override = Some(true);
            resource_options.disable_extensions = true;
            resource_options.disable_skills = true;
            resource_options.disable_prompt_templates = true;
            resource_options.disable_themes = true;
            resource_options.disable_context_files = true;
            let resources = pi_coding::ResourceManager::new(resource_options)?;
            let session_dir = cwd.path().join(".pi-workflow-test-sessions");
            std::fs::create_dir_all(&session_dir)?;
            session.set_session_dir(session_dir);
            session.attach_resources(resources).await?;
            session.start_new_recording()?;
            let definition = AgentDefinition {
                name: "task".into(),
                description: "workflow worker".into(),
                system_prompt: "complete workflow".into(),
                tools: Some(Vec::new()),
                autoload_skills: Vec::new(),
                model: None,
                thinking_level: Some(ThinkingLevel::Off),
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
                knowledge_roots: Vec::new(),
                user_proxy: false,
            };
            let artifacts = std::env::temp_dir().join(format!(
                "pi-cli-workflow-artifacts-{}",
                uuid::Uuid::now_v7()
            ));
            let mut config =
                OrchestrationConfig::new(AgentCatalog::from_agents(vec![definition]), artifacts);
            config.parent_model = snapshot.model.clone();
            config.idle_ttl = None;
            let orchestration = OrchestrationRuntime::new(
                config,
                OrchestrationRuntime::child_factory_from_snapshot(snapshot),
            )?;
            Ok(ApplicationRuntimeCandidate::new(session).with_orchestration(orchestration))
        })
    }
}
