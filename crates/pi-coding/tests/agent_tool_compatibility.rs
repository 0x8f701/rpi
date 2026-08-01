use pi_coding::{ResourceDiagnosticLevel, ResourceManager, ResourceManagerOptions};

#[test]
fn resource_snapshot_warns_for_incompatible_agent_without_advertising_it() {
    let cwd = tempfile::tempdir().expect("cwd");
    let agent_dir = tempfile::tempdir().expect("agent dir");
    let agents = agent_dir.path().join("agents");
    std::fs::create_dir_all(&agents).expect("agents dir");
    std::fs::write(
        agents.join("architect.md"),
        "---\nname: architect\ndescription: architecture agent\ntools: [read, lsp]\n---\nDesign the system.",
    )
    .expect("architect definition");
    std::fs::write(
        agents.join("broken-model.md"),
        "---\nname: broken-model\ndescription: broken model agent\n---\nUse a missing model.",
    )
    .expect("broken model definition");
    std::fs::write(
        agent_dir.path().join("settings.json"),
        r#"{"agents":{"broken-model":{"enabled":true,"model":"missing-provider/missing-model"}}}"#,
    )
    .expect("agent settings");

    let mut options = ResourceManagerOptions::new(cwd.path());
    options.agent_dir = agent_dir.path().to_path_buf();
    options.disable_extensions = true;
    options.disable_skills = true;
    options.disable_prompt_templates = true;
    options.disable_themes = true;
    options.disable_context_files = true;
    let resources = ResourceManager::new(options).expect("incompatible agent must not fail startup");
    let snapshot = resources.snapshot();

    assert!(snapshot.agents.iter().any(|agent| agent.name == "architect"));
    let warning = snapshot
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.message.contains("architect"))
        .expect("architect incompatibility warning");
    assert_eq!(warning.level, ResourceDiagnosticLevel::Warning);
    assert!(warning.message.contains("unsupported child tools: lsp"));
    assert_eq!(warning.path.as_deref(), Some(agents.join("architect.md").as_path()));
    let model_warning = snapshot
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.message.contains("broken-model"))
        .expect("invalid model warning");
    assert_eq!(model_warning.level, ResourceDiagnosticLevel::Warning);
    assert!(model_warning.message.contains("model configuration is invalid"));
    assert!(model_warning.message.contains("missing-provider/missing-model"));

    let advertised = pi_coding::enabled_agent_definitions(&snapshot.agents, &snapshot.settings.agents);
    assert!(advertised.iter().all(|agent| agent.name != "architect"));
    assert!(advertised.iter().all(|agent| agent.name != "broken-model"));
    assert!(advertised.iter().any(|agent| agent.name == "task"));
}
