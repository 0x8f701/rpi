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
