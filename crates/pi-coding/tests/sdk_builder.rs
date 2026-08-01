use std::sync::Arc;

use anyhow::Result;
use pi_agent::{AgentTool, ThinkingLevel};
use pi_ai::{Message, Model, Schema};
use pi_coding::{
    AgentSessionBuilder, ExtensionHost, ExtensionInstanceId, ExtensionLaunch, ExtensionLoadFailure,
    ExtensionLoadReport, ExtensionPermissionSet, ExtensionRuntime, ExtensionRuntimeOptions,
    ExtensionTransport, PreparedExtensionRuntime, RequestAuth, ResourceDiscovery, SessionEntry,
    SessionRecorder, SessionTreeNode, Settings, create_agent_session, start_session_in,
};

fn model() -> Model {
    Model {
        id: "sdk-model".to_owned(),
        name: "SDK model".to_owned(),
        api: "faux".to_owned(),
        provider: "sdk-provider".to_owned(),
        reasoning: true,
        ..Model::default()
    }
}

fn injected_tool() -> AgentTool {
    AgentTool::new("injected", "injected", Schema::default(), |_| async {
        Ok(pi_agent::AgentToolResult::text("ok"))
    })
}

fn recorder(
    cwd: &tempfile::TempDir,
    model: &Model,
    thinking_level: &str,
) -> Result<SessionRecorder> {
    Ok(start_session_in(
        cwd.path(),
        Some(model),
        Some(thinking_level),
        Some(&cwd.path().join("sessions")),
        None,
        None,
    )?)
}
fn find_entry<'a>(
    nodes: &'a [SessionTreeNode],
    predicate: &impl Fn(&SessionEntry) -> bool,
) -> Option<&'a SessionEntry> {
    nodes.iter().find_map(|node| {
        predicate(&node.entry)
            .then_some(&node.entry)
            .or_else(|| find_entry(&node.children, predicate))
    })
}



#[tokio::test]
async fn builder_defaults_to_no_resource_discovery_and_injects_settings_tools_and_recorder() -> Result<()> {
    let cwd = tempfile::tempdir()?;
    std::fs::write(cwd.path().join("AGENTS.md"), "project-only-marker")?;
    let skill_dir = cwd.path().join(".pi/skills/project-skill");
    std::fs::create_dir_all(&skill_dir)?;
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: project-skill\ndescription: project-only-skill-marker\n---\n",
    )?;
    let recorder = recorder(&cwd, &model(), "off")?;
    let recorder_id = recorder.id();
    let settings = Settings {
        temperature: Some(0.25),
        ..Settings::default()
    };

    let built = create_agent_session(
        AgentSessionBuilder::new(model(), cwd.path())
            .thinking_level(ThinkingLevel::Off)
            .api_key("injected-key")
            .settings(settings)
            .tools(vec![])
            .additional_tools(vec![injected_tool()])
            .recorder(recorder),
    )
    .await?;

    assert_eq!(built.session.get_active_tool_names(), ["injected"]);
    assert_eq!(built.session.recorder_info().map(|value| value.0), Some(recorder_id));
    assert_eq!(built.application.session().cwd(), cwd.path());
    let state = built.application.state().await;
    assert_eq!(state.thinking_level, ThinkingLevel::Off);
    assert_eq!(built.session.stream_options().stream.temperature, Some(0.25));
    let prompt = built.session.system_prompt().await;
    assert!(!prompt.contains("project-only-marker"));
    assert!(!prompt.contains("project-only-skill-marker"));
    let selection = built.session.select_for_request("project-only-skill-marker").await;
    assert!(selection.skills.is_empty());
    assert!(selection.autoload_skills.is_empty());
    Ok(())
}

#[tokio::test]
async fn explicit_trusted_project_discovery_loads_project_context() -> Result<()> {
    let cwd = tempfile::tempdir()?;
    std::fs::write(cwd.path().join("AGENTS.md"), "trusted-project-marker")?;

    let built = AgentSessionBuilder::new(model(), cwd.path())
        .tools(vec![])
        .resource_discovery(ResourceDiscovery::TrustedProject)
        .build()
        .await?;

    assert!(built
        .session
        .system_prompt()
        .await
        .contains("trusted-project-marker"));
    Ok(())
}

#[derive(Default)]
struct NeverLaunchHost;

impl ExtensionHost for NeverLaunchHost {
    fn launch(
        &self,
        _launch: ExtensionLaunch,
    ) -> pi_coding::ExtensionFuture<'_, Result<Arc<dyn ExtensionTransport>>> {
        Box::pin(async { anyhow::bail!("extensions should not launch in this test") })
    }
}

#[tokio::test]
async fn prepared_extension_runtime_is_attached_and_reports_its_load_outcome() -> Result<()> {
    let cwd = tempfile::tempdir()?;
    let runtime = ExtensionRuntime::new(
        Arc::new(NeverLaunchHost),
        None,
        ExtensionRuntimeOptions::default(),
    );
    let report = ExtensionLoadReport {
        generation: 7,
        loaded: vec![ExtensionInstanceId {
            extension_id: "loaded-extension".to_owned(),
            generation: 7,
        }],
        failures: vec![ExtensionLoadFailure {
            extension_id: "failed-extension".to_owned(),
            path: cwd.path().join("failed-extension.ts"),
            message: "load failed".to_owned(),
        }],
    };

    let built = AgentSessionBuilder::new(model(), cwd.path())
        .tools(vec![])
        .extensions(
            PreparedExtensionRuntime::new(
                runtime.clone(),
                ExtensionPermissionSet::deny_all(),
            )
            .with_load_report(report.clone()),
        )
        .build()
        .await?;

    assert_eq!(
        built.application.extension_runtime().map(|runtime| runtime.generation()),
        Some(runtime.generation())
    );
    assert_eq!(built.extensions_result, Some(report));
    assert_eq!(built.application.session().cwd(), built.session.cwd());
    Ok(())
}

#[tokio::test]
async fn resume_recorder_restores_active_branch_history_and_lineage() -> Result<()> {
    let cwd = tempfile::tempdir()?;
    let recorder = recorder(&cwd, &model(), "low")?;
    let first_id = recorder.record_message(&Message::user_text("first branch", 1))?;
    recorder.record_message(&Message::user_text("discarded branch", 2))?;
    recorder.branch(&first_id)?;
    let resumed_id = recorder.id();
    let resumed_path = recorder.path();

    let built = AgentSessionBuilder::new(model(), cwd.path())
        .tools(vec![])
        .resume_recorder(recorder)
        .build()
        .await?;

    assert_eq!(built.session.history(), [Message::user_text("first branch", 1)]);
    assert_eq!(built.session.thinking_level(), ThinkingLevel::Low);
    assert_eq!(
        built.session.stream_options().stream.session_id.as_deref(),
        built.session.recorder_info().as_ref().map(|(id, _)| id.as_str())
    );
    assert_eq!(built.session.recorder_info(), Some((resumed_id, resumed_path)));
    assert_eq!(
        built.session.session_tree()?.active_leaf_id.as_deref(),
        Some(first_id.as_str())
    );
    let continued_id = built.session.append_custom_entry("resume-check", None)?;
    let continued_tree = built.session.session_tree()?;
    assert_eq!(
        find_entry(&continued_tree.tree, &|entry| entry.id == continued_id)
            .and_then(|entry| entry.parent_id.as_deref()),
        Some(first_id.as_str())
    );
    Ok(())
}

#[tokio::test]
async fn resume_reports_saved_model_fallback_without_discovery() -> Result<()> {
    let cwd = tempfile::tempdir()?;
    std::fs::write(cwd.path().join("AGENTS.md"), "resume-project-marker")?;
    let mut saved_model = model();
    saved_model.provider = "missing-provider".to_owned();
    saved_model.id = "missing-model".to_owned();
    let recorder = recorder(&cwd, &saved_model, "off")?;
    recorder.record_message(&Message::user_text("resume history", 1))?;

    let built = AgentSessionBuilder::new(model(), cwd.path())
        .tools(vec![])
        .resume_recorder(recorder)
        .build()
        .await?;

    assert_eq!(
        built.model_fallback_message.as_deref(),
        Some("Could not restore model missing-provider/missing-model. Using sdk-provider/sdk-model")
    );
    assert_eq!(built.session.model(), Some(model()));
    assert_eq!(built.session.history(), [Message::user_text("resume history", 1)]);
    assert!(!built.session.system_prompt().await.contains("resume-project-marker"));
    Ok(())
}

#[tokio::test]
async fn resume_restores_catalog_model_when_auth_resolves() -> Result<()> {
    let cwd = tempfile::tempdir()?;
    let saved_model = pi_ai::get_model("anthropic", "claude-sonnet-4-5")
        .expect("embedded model catalog");
    let recorder = recorder(&cwd, &saved_model, "medium")?;
    recorder.record_message(&Message::user_text("restore model", 1))?;
    let resolver = Arc::new(|_model: Model| {
        Box::pin(async {
            Ok(RequestAuth {
                api_key: "restored-key".to_owned(),
                ..RequestAuth::default()
            })
        }) as pi_agent::BoxFuture<Result<RequestAuth>>
    });

    let built = AgentSessionBuilder::new(model(), cwd.path())
        .tools(vec![])
        .auth_resolver(resolver)
        .resume_recorder(recorder)
        .build()
        .await?;

    assert_eq!(built.session.model(), Some(saved_model));
    assert_eq!(built.session.current_api_key(), "restored-key");
    assert_eq!(built.model_fallback_message, None);
    assert_eq!(built.session.thinking_level(), ThinkingLevel::Medium);
    Ok(())
}

#[tokio::test]
async fn model_override_suppresses_saved_model_restore_notification() -> Result<()> {
    let cwd = tempfile::tempdir()?;
    let mut saved_model = model();
    saved_model.provider = "missing-provider".to_owned();
    saved_model.id = "missing-model".to_owned();
    let recorder = recorder(&cwd, &saved_model, "off")?;
    recorder.record_message(&Message::user_text("explicit model", 1))?;
    let explicit_model = model();

    let built = AgentSessionBuilder::new(model(), cwd.path())
        .model_override(explicit_model.clone())
        .tools(vec![])
        .resume_recorder(recorder)
        .build()
        .await?;

    assert_eq!(built.session.model(), Some(explicit_model));
    assert_eq!(built.model_fallback_message, None);
    Ok(())
}

#[tokio::test]
async fn settings_supply_thinking_level_when_resume_has_no_recorded_level() -> Result<()> {
    let cwd = tempfile::tempdir()?;
    let recorder = start_session_in(
        cwd.path(),
        Some(&model()),
        None,
        Some(&cwd.path().join("sessions")),
        None,
        None,
    )?;
    recorder.record_message(&Message::user_text("legacy session", 1))?;
    let settings = Settings {
        default_thinking_level: Some(ThinkingLevel::High),
        ..Settings::default()
    };

    let built = AgentSessionBuilder::new(model(), cwd.path())
        .settings(settings)
        .tools(vec![])
        .resume_recorder(recorder)
        .build()
        .await?;

    assert_eq!(built.session.thinking_level(), ThinkingLevel::High);
    let tree = built.session.session_tree()?;
    let context = find_entry(&tree.tree, &|entry| {
        entry.entry_type == "thinking_level_change"
    });
    assert_eq!(
        context.and_then(|entry| entry.thinking_level.as_deref()),
        Some("high")
    );
    Ok(())
}
