use std::fs;

use pi_agent::ThinkingLevel;
use pi_ai::Model;
use pi_ai::providers::{FauxProviderOptions, register_faux_provider};
use pi_coding::{
    Application, ResourceManager, ResourceManagerOptions, Session, SessionOptions, SettingsScope,
    SettingSource,
};

#[tokio::test]
async fn real_enable_model_thinking_and_legacy_migration_persist() {
    let root = tempfile::tempdir().expect("root");
    let agent_dir = root.path().join("agent-home");
    fs::create_dir_all(agent_dir.join("agents")).expect("agents");
    fs::write(
        agent_dir.join("settings.json"),
        r#"{
          "orchestration": {"tasks": true, "process": true, "todo": true, "maxConcurrency": 1},
          "subagents": {
            "agentOverrides": {
              "reviewer": {
                "enabled": true,
                "model": "openai/gpt-4.1",
                "tools": ["read"]
              }
            },
            "keepMe": {"x": 1}
          }
        }"#,
    )
    .expect("settings");
    fs::write(
        agent_dir.join("agents").join("reviewer.md"),
        "---\nname: reviewer\ndescription: review code\n---\nYou review.\n",
    )
    .expect("agent def");

    let suffix = uuid::Uuid::now_v7().to_string();
    let api = format!("agent-cfg-api-{suffix}");
    let provider = "openai".to_owned();
    let reasoning = Model {
        id: "reasoner".into(),
        name: "Reasoner".into(),
        api: api.clone(),
        provider: provider.clone(),
        reasoning: true,
        ..Model::default()
    };
    let non_reasoning = Model {
        id: "qwen".into(),
        name: "Qwen".into(),
        api: api.clone(),
        provider: provider.clone(),
        reasoning: false,
        ..Model::default()
    };
    let registration = register_faux_provider(FauxProviderOptions {
        api,
        provider: provider.clone(),
        models: vec![reasoning.clone(), non_reasoning.clone()],
        chunk_size: 1,
    });

    let mut options = ResourceManagerOptions::new(root.path());
    options.agent_dir = agent_dir.clone();
    options.disable_extensions = true;
    options.disable_skills = true;
    options.disable_prompt_templates = true;
    options.disable_themes = true;
    options.disable_context_files = true;
    let resources = ResourceManager::new(options).expect("resources");

    let settings = resources.settings_manager().settings();
    let migrated = settings.agent_settings("reviewer").expect("migrated");
    assert_eq!(migrated.enabled, Some(true));
    assert_eq!(migrated.model.as_deref(), Some("openai/gpt-4.1"));
    assert_eq!(
        migrated.tools.as_deref(),
        Some(["read".to_owned()].as_slice())
    );

    let session = Session::new(SessionOptions {
        model: reasoning.clone(),
        cwd: root.path().to_path_buf(),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
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
    session
        .attach_resources(resources.clone())
        .await
        .expect("attach");
    let application = Application::new(session).await;
    application.reload().await.expect("reload");

    resources
        .settings_manager()
        .update_global(|settings| {
            settings.agents.insert(
                "reviewer".into(),
                pi_coding::AgentRuntimeSettings {
                    enabled: Some(false),
                    model: Some("openai/reasoner".into()),
                    tools: Some(vec!["read".into()]),
                },
            );
        })
        .expect("disable+model");
    application.reload().await.expect("reload after save");

    let runtime = application.orchestration_runtime().expect("orch");
    assert!(runtime.ensure_agent_enabled("reviewer").is_err());
    let task = runtime
        .agent_tools("Main", 0)
        .into_iter()
        .find(|tool| tool.name == "task")
        .expect("task");
    assert!(
        !task.description.contains("reviewer —"),
        "{}",
        task.description
    );

    let high = application.set_thinking_level(ThinkingLevel::High);
    assert!(!high.clamped, "{}", high.message);
    assert_eq!(high.effective, ThinkingLevel::High);

    let clamp = application
        .set_model_with_resolved_auth(non_reasoning)
        .await
        .expect("switch model");
    assert!(clamp.clamped, "{}", clamp.message);
    assert_eq!(clamp.effective, ThinkingLevel::Off);
    assert!(clamp.message.contains("unsupported"), "{}", clamp.message);
    assert_eq!(application.session().thinking_level(), ThinkingLevel::Off);

    let raw = fs::read_to_string(agent_dir.join("settings.json")).expect("raw");
    assert!(!raw.contains("agentOverrides"), "{raw}");
    assert!(raw.contains("\"agents\""), "{raw}");
    assert!(raw.contains("keepMe"), "{raw}");
    assert!(
        raw.contains("\"enabled\": false") || raw.contains("\"enabled\":false"),
        "{raw}"
    );

    let draft = application
        .settings_draft(SettingsScope::Global)
        .expect("draft");
    let view = draft.get("defaultThinkingLevel").expect("thinking view");
    assert_eq!(view.source, SettingSource::Runtime);
    assert_eq!(view.effective_value, serde_json::json!("off"));

    application.cleanup().await;
    registration.unregister();
}
