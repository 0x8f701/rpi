use anyhow::Result;
use pi_agent::{AgentTool, ThinkingLevel};
use pi_ai::providers::{FauxProviderOptions, register_faux_provider};
use pi_ai::{Model, Schema};
use pi_coding::{
    Application, ApplicationRuntimeCandidate, ResourceDiscovery, ResourceManager,
    ResourceManagerOptions, Session, SessionOptions, ToolSelection,
};

fn model() -> Model {
    Model {
        id: "tool-selection".to_owned(),
        name: "Tool Selection".to_owned(),
        api: "faux".to_owned(),
        provider: "faux".to_owned(),
        ..Model::default()
    }
}

fn options(cwd: &std::path::Path) -> SessionOptions {
    SessionOptions {
        model: model(),
        cwd: cwd.to_path_buf(),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: String::new(),
        compaction: None,
        stream_options: Default::default(),
        tools: None,
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: None,
        auth_resolver: None,
        auth_manager: None,
    }
}

fn default_catalog() -> [&'static str; 22] {
    [
        "read",
        "inspect_image",
        "bash",
        "browser",
        "edit",
        "write",
        "ast_edit",
        "ast_grep",
        "ls",
        "find",
        "grep",
        "glob",
        "generate_image",
        "web_search",
        "github",
        "lsp",
        "debug",
        "eval",
        "notebook",
        "memory",
        "ask",
        "mcp",
    ]
}

fn custom_tool() -> AgentTool {
    AgentTool::new("custom", "custom", Schema::default(), |_| async {
        Ok(pi_agent::AgentToolResult::text("ok"))
    })
}

#[test]
fn default_tool_set_remains_the_coding_tools() -> Result<()> {
    let cwd = tempfile::tempdir()?;
    let session = Session::new_with_additional_tools_filtered_and_discovery(
        options(cwd.path()),
        Vec::new(),
        ToolSelection::default(),
        ResourceDiscovery::Disabled,
    )?;
    assert_eq!(session.get_active_tool_names(), default_catalog());
    Ok(())
}

#[test]
fn allowlist_then_denylist_filters_builtins_and_custom_tools() -> Result<()> {
    let cwd = tempfile::tempdir()?;
    let session = Session::new_with_additional_tools_filtered_and_discovery(
        options(cwd.path()),
        vec![custom_tool()],
        ToolSelection {
            allow: Some(vec![
                "read".to_owned(),
                "write".to_owned(),
                "custom".to_owned(),
            ]),
            deny: vec!["write".to_owned()],
            ..ToolSelection::default()
        },
        ResourceDiscovery::Disabled,
    )?;
    assert_eq!(session.get_active_tool_names(), ["read", "custom"]);
    Ok(())
}

#[test]
fn no_builtin_tools_preserves_nonbuiltins() -> Result<()> {
    let cwd = tempfile::tempdir()?;
    let session = Session::new_with_additional_tools_filtered_and_discovery(
        options(cwd.path()),
        vec![custom_tool()],
        ToolSelection {
            disable_builtins: true,
            ..ToolSelection::default()
        },
        ResourceDiscovery::Disabled,
    )?;
    assert_eq!(session.get_active_tool_names(), ["custom"]);
    Ok(())
}

#[test]
fn unknown_allow_or_deny_names_fail_with_available_names() -> Result<()> {
    let cwd = tempfile::tempdir()?;
    for selection in [
        ToolSelection {
            allow: Some(vec!["missing".to_owned()]),
            ..ToolSelection::default()
        },
        ToolSelection {
            deny: vec!["missing".to_owned()],
            ..ToolSelection::default()
        },
    ] {
        let error = Session::new_with_additional_tools_filtered_and_discovery(
            options(cwd.path()),
            vec![custom_tool()],
            selection,
            ResourceDiscovery::Disabled,
        )
        .err()
        .expect("unknown tool must fail");
        let message = error.to_string();
        assert!(message.contains("missing"), "{message}");
        assert!(message.contains("available tools"), "{message}");
        assert!(message.contains("read"), "{message}");
        assert!(message.contains("custom"), "{message}");
    }
    Ok(())
}

#[test]
fn process_and_todo_remain_explicit_capabilities() -> Result<()> {
    let cwd = tempfile::tempdir()?;
    let session = Session::new_with_todo_and_additional_tools_filtered_and_discovery(
        options(cwd.path()),
        Vec::new(),
        ToolSelection {
            enable_process: true,
            ..ToolSelection::default()
        },
        ResourceDiscovery::Disabled,
    )?;
    let mut names = default_catalog().to_vec();
    let ask = names.iter().position(|name| *name == "ask").expect("ask");
    names.insert(ask, "todo");
    names.push("process");
    assert_eq!(session.get_active_tool_names(), names);
    Ok(())
}

#[test]
fn glob_stays_single_in_the_default_generation_catalog() -> Result<()> {
    let cwd = tempfile::tempdir()?;
    let default_session = Session::new_with_additional_tools_filtered_and_discovery(
        options(cwd.path()),
        Vec::new(),
        ToolSelection::default(),
        ResourceDiscovery::Disabled,
    )?;
    let default_names = default_session.get_active_tool_names();
    assert_eq!(default_names, default_catalog());
    assert_eq!(default_names.iter().filter(|name| **name == "glob").count(), 1);

    // enable_glob must not append a second glob next to the generation tool.
    let with_glob = Session::new_with_additional_tools_filtered_and_discovery(
        options(cwd.path()),
        Vec::new(),
        ToolSelection {
            enable_glob: true,
            ..ToolSelection::default()
        },
        ResourceDiscovery::Disabled,
    )?;
    assert_eq!(with_glob.get_active_tool_names(), default_catalog());

    // Allow-list naming glob also injects it into the available set.
    let allow_glob = Session::new_with_additional_tools_filtered_and_discovery(
        options(cwd.path()),
        Vec::new(),
        ToolSelection {
            allow: Some(vec![
                "read".to_owned(),
                "bash".to_owned(),
                "glob".to_owned(),
            ]),
            ..ToolSelection::default()
        },
        ResourceDiscovery::Disabled,
    )?;
    assert_eq!(allow_glob.get_active_tool_names(), ["read", "bash", "glob"]);
    // Parent can invoke the native tool by name.
    assert_eq!(
        allow_glob.get_tool_definition("glob").map(|t| t.name),
        Some("glob".to_owned())
    );
    Ok(())
}

#[tokio::test]
async fn parent_session_can_execute_native_glob_tool() -> Result<()> {
    let cwd = tempfile::tempdir()?;
    std::fs::write(cwd.path().join("alpha.rs"), b"fn a() {}")?;
    std::fs::write(cwd.path().join("beta.ts"), b"const b = 1;")?;
    let suffix = uuid::Uuid::now_v7().to_string();
    let model = Model {
        id: "glob-parent".to_owned(),
        name: "Glob Parent".to_owned(),
        api: format!("tool-selection-glob-{suffix}"),
        provider: format!("tool-selection-glob-provider-{suffix}"),
        ..Model::default()
    };
    let _registration = register_faux_provider(FauxProviderOptions {
        api: model.api.clone(),
        provider: model.provider.clone(),
        models: vec![model.clone()],
        chunk_size: 1,
    });
    let agent_dir = tempfile::tempdir()?;
    let session_dir = tempfile::tempdir()?;
    let mut resource_options = ResourceManagerOptions::new(cwd.path());
    resource_options.agent_dir = agent_dir.path().to_path_buf();
    resource_options.disable_extensions = true;
    resource_options.disable_skills = true;
    resource_options.disable_prompt_templates = true;
    resource_options.disable_themes = true;
    resource_options.disable_context_files = true;
    let resources = ResourceManager::new(resource_options)?;
    let mut session_options = options(cwd.path());
    session_options.model = model;
    let session = Session::new_with_additional_tools_filtered_and_discovery(
        session_options,
        Vec::new(),
        ToolSelection::default(),
        ResourceDiscovery::Disabled,
    )?;
    session.set_session_dir(session_dir.path().to_path_buf());
    session.attach_resources(resources).await?;
    let application =
        Application::from_runtime_candidate(ApplicationRuntimeCandidate::new(session)).await?;
    let tool = application
        .session()
        .get_tool_definition("glob")
        .expect("glob is on the default generation catalog");
    assert_eq!(tool.name, "glob");
    let (controller, abort) = pi_agent::AbortController::new();
    std::mem::forget(controller);
    let result = (tool.execute)(pi_agent::ToolCallContext {
        tool_call_id: "parent-glob".to_owned(),
        arguments: serde_json::json!({ "pattern": "*.rs" }),
        on_update: std::sync::Arc::new(|_r| {}),
        abort,
        model: None,
        approval_token: None,
    })
    .await?;
    let text = result
        .content
        .iter()
        .filter_map(|block| match block {
            pi_ai::ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    assert!(
        text.contains("alpha.rs"),
        "parent glob call missed match: {text}"
    );
    assert!(
        !text.contains("beta.ts"),
        "parent glob should not match ts: {text}"
    );
    Ok(())
}
