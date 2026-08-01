use anyhow::Result;
use pi_agent::{AgentTool, ThinkingLevel};
use pi_ai::{Model, Schema};
use pi_coding::{ResourceDiscovery, Session, SessionOptions, ToolSelection};

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
    }
}

fn custom_tool() -> AgentTool {
    AgentTool::new("custom", "custom", Schema::default(), |_| async {
        Ok(pi_agent::AgentToolResult::text("ok"))
    })
}

#[test]
fn default_tool_set_remains_the_four_coding_tools() -> Result<()> {
    let cwd = tempfile::tempdir()?;
    let session = Session::new_with_additional_tools_filtered_and_discovery(
        options(cwd.path()),
        Vec::new(),
        ToolSelection::default(),
        ResourceDiscovery::Disabled,
    )?;
    assert_eq!(session.get_active_tool_names(), ["read", "bash", "edit", "write"]);
    Ok(())
}

#[test]
fn allowlist_then_denylist_filters_builtins_and_custom_tools() -> Result<()> {
    let cwd = tempfile::tempdir()?;
    let session = Session::new_with_additional_tools_filtered_and_discovery(
        options(cwd.path()),
        vec![custom_tool()],
        ToolSelection {
            allow: Some(vec!["read".to_owned(), "write".to_owned(), "custom".to_owned()]),
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
    assert_eq!(
        session.get_active_tool_names(),
        ["read", "bash", "edit", "write", "todo", "process"]
    );
    Ok(())
}

#[test]
fn glob_is_opt_in_not_in_default_main_catalog() -> Result<()> {
    let cwd = tempfile::tempdir()?;
    // Default baseline: strict coding four — no glob.
    let default_session = Session::new_with_additional_tools_filtered_and_discovery(
        options(cwd.path()),
        Vec::new(),
        ToolSelection::default(),
        ResourceDiscovery::Disabled,
    )?;
    let default_names = default_session.get_active_tool_names();
    assert_eq!(default_names, ["read", "bash", "edit", "write"]);
    assert!(!default_names.iter().any(|n| n == "glob"));

    // Explicit enable_glob adds native glob without other expansions.
    let with_glob = Session::new_with_additional_tools_filtered_and_discovery(
        options(cwd.path()),
        Vec::new(),
        ToolSelection {
            enable_glob: true,
            ..ToolSelection::default()
        },
        ResourceDiscovery::Disabled,
    )?;
    assert_eq!(
        with_glob.get_active_tool_names(),
        ["read", "bash", "edit", "write", "glob"]
    );

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
    assert_eq!(
        allow_glob.get_active_tool_names(),
        ["read", "bash", "glob"]
    );
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
    let session = Session::new_with_additional_tools_filtered_and_discovery(
        options(cwd.path()),
        Vec::new(),
        ToolSelection {
            enable_glob: true,
            ..ToolSelection::default()
        },
        ResourceDiscovery::Disabled,
    )?;
    let tool = session
        .get_tool_definition("glob")
        .expect("glob must be on main catalog when enable_glob");
    assert_eq!(tool.name, "glob");
    let (controller, abort) = pi_agent::AbortController::new();
    std::mem::forget(controller);
    let result = (tool.execute)(pi_agent::ToolCallContext {
        tool_call_id: "parent-glob".to_owned(),
        arguments: serde_json::json!({ "pattern": "*.rs" }),
        on_update: std::sync::Arc::new(|_r| {}),
        abort,
        model: None,
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
    assert!(text.contains("alpha.rs"), "parent glob call missed match: {text}");
    assert!(!text.contains("beta.ts"), "parent glob should not match ts: {text}");
    Ok(())
}
