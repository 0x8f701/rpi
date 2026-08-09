use pi_coding::{ResourceDiagnosticLevel, ResourceManager, ResourceManagerOptions};

#[test]
fn resource_snapshot_warns_once_for_unknown_tools_but_advertises_the_agent() {
    let cwd = tempfile::tempdir().expect("cwd");
    let agent_dir = tempfile::tempdir().expect("agent dir");
    let agents = agent_dir.path().join("agents");
    std::fs::create_dir_all(&agents).expect("agents dir");
    std::fs::write(
        agents.join("architect.md"),
        "---\nname: architect\ndescription: architecture agent\ntools: [read, unsupported_child_tool, yield_output]\n---\nDesign the system.",
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
    let resources = ResourceManager::new(options).expect("unknown tools must not fail startup");
    let snapshot = resources.snapshot();

    assert!(snapshot.agents.iter().any(|agent| agent.name == "architect"));
    // Exactly ONE warning for the architect listing BOTH unknown names
    // (unsupported_child_tool + the ghost yield_output); read is known.
    let warnings = snapshot
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("architect"))
        .collect::<Vec<_>>();
    assert_eq!(warnings.len(), 1, "one deduped warning per agent: {warnings:#?}");
    let warning = warnings[0];
    assert_eq!(warning.level, ResourceDiagnosticLevel::Warning);
    assert!(warning.message.contains("unsupported_child_tool"), "{}", warning.message);
    assert!(warning.message.contains("yield_output"), "{}", warning.message);
    assert!(warning.message.contains("ignoring"), "{}", warning.message);
    assert_eq!(warning.path.as_deref(), Some(agents.join("architect.md").as_path()));
    let model_warning = snapshot
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.message.contains("broken-model"))
        .expect("invalid model warning");
    assert_eq!(model_warning.level, ResourceDiagnosticLevel::Warning);
    assert!(model_warning.message.contains("model configuration is invalid"));
    assert!(model_warning.message.contains("missing-provider/missing-model"));

    // OMP alignment: unknown declared tools never make an agent unavailable —
    // the architect IS advertised; the broken-model agent stays excluded.
    let advertised = pi_coding::enabled_agent_definitions(&snapshot.agents, &snapshot.settings.agents);
    assert!(advertised.iter().any(|agent| agent.name == "architect"));
    assert!(advertised.iter().all(|agent| agent.name != "broken-model"));
    assert!(advertised.iter().any(|agent| agent.name == "task"));
}
