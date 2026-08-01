use std::sync::{Arc, Mutex};

use anyhow::Result;
use pi_agent::{AbortController, ThinkingLevel, ToolCallContext};
use pi_ai::{
    AssistantMessage, ContentBlock, Context, Model, StopReason,
    new_assistant_message_event_stream,
};
use pi_coding::{
    Application, ResourceDiscovery, ResourceManager, ResourceManagerOptions, Session,
    SessionOptions, ToolSelection,
};
use serde_json::json;

#[test]
fn explicit_project_system_prompt_is_trust_gated_before_read() -> Result<()> {
    let cwd = tempfile::tempdir()?;
    let agent_dir = tempfile::tempdir()?;
    let project_dir = cwd.path().join(".pi");
    std::fs::create_dir_all(&project_dir)?;
    let prompt = project_dir.join("system.txt");
    std::fs::write(&prompt, "trusted prompt")?;

    let mut denied = ResourceManagerOptions::new(cwd.path());
    denied.agent_dir = agent_dir.path().to_path_buf();
    denied.headless = true;
    denied.system_prompt = Some(String::new());
    denied.system_prompt_path = Some(prompt.clone());
    let error = ResourceManager::new(denied)
        .err()
        .expect("untrusted project prompt must fail");
    assert!(error.to_string().contains("requires project trust"));

    let mut allowed = ResourceManagerOptions::new(cwd.path());
    allowed.agent_dir = agent_dir.path().to_path_buf();
    allowed.headless = true;
    allowed.project_trust_override = Some(true);
    allowed.system_prompt = Some(String::new());
    allowed.system_prompt_path = Some(prompt);
    let manager = ResourceManager::new(allowed)?;
    assert_eq!(manager.snapshot().system_prompt.as_deref(), Some("trusted prompt"));
    Ok(())
}

#[test]
fn append_system_prompt_preserves_literal_and_file_order() -> Result<()> {
    let cwd = tempfile::tempdir()?;
    let agent_dir = tempfile::tempdir()?;
    let prompt = cwd.path().join("append.txt");
    std::fs::write(&prompt, "from file")?;

    let mut options = ResourceManagerOptions::new(cwd.path());
    options.agent_dir = agent_dir.path().to_path_buf();
    options.headless = true;
    options.project_trust_override = Some(true);
    options.append_system_prompt = vec!["literal".to_owned(), String::new()];
    options.append_system_prompt_paths = vec![None, Some(prompt)];
    let manager = ResourceManager::new(options)?;
    assert_eq!(manager.snapshot().append_system_prompt, ["literal", "from file"]);
    Ok(())
}

#[tokio::test]
async fn global_coordinate_skill_expands_and_executes_once_in_attached_session() -> Result<()> {
    let cwd = tempfile::tempdir()?;
    let home = tempfile::tempdir()?;
    let agent_dir = home.path().join(".pi").join("agent");
    let skill_dir = agent_dir.join("skills").join("coordinate");
    std::fs::create_dir_all(&skill_dir)?;
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: coordinate\ndescription: Coordinate work across agents\n---\ncoordinate skill body\n",
    )?;
    let disabled_dir = agent_dir.join("skills").join("disabled-review");
    std::fs::create_dir_all(&disabled_dir)?;
    std::fs::write(
        disabled_dir.join("SKILL.md"),
        "---\nname: disabled-review\ndescription: Must not be invoked\ndisable-model-invocation: true\n---\ndisabled body\n",
    )?;

    let mut options = ResourceManagerOptions::new(cwd.path());
    options.agent_dir = agent_dir;
    options.headless = true;
    options.project_trust_override = Some(true);
    let manager = ResourceManager::new(options)?;
    assert_eq!(
        manager
            .snapshot()
            .skills
            .iter()
            .filter(|skill| skill.name == "coordinate")
            .count(),
        1,
    );

    let contexts = Arc::new(Mutex::new(Vec::<Context>::new()));
    let captured = contexts.clone();
    let stream_fn: pi_agent::StreamFn = Arc::new(move |model, context, _options| {
        let captured = captured.clone();
        Box::pin(async move {
            captured.lock().expect("captured contexts").push(context);
            let stream = new_assistant_message_event_stream();
            let mut message = AssistantMessage::pending(&model);
            message.content.push(ContentBlock::text("done"));
            message.stop_reason = StopReason::Stop;
            stream.end(Some(message)).await;
            stream
        })
    });

    let session = Session::new_with_additional_tools_filtered_and_discovery(
        SessionOptions {
            model: Model::default(),
            cwd: cwd.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: "test".to_owned(),
            compaction: None,
            stream_options: Default::default(),
            tools: None,
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: Some(stream_fn),
            auth_resolver: None,
        },
        Vec::new(),
        ToolSelection::default(),
        ResourceDiscovery::Disabled,
    )?;
    session.attach_resources(manager).await?;

    let prompt = session.system_prompt().await;
    assert!(prompt.contains("<name>coordinate</name>"), "{prompt}");
    assert!(
        prompt.contains("<location>skill://coordinate</location>"),
        "{prompt}"
    );
    assert!(!prompt.contains("disabled-review"), "{prompt}");

    let read = session
        .get_tool_definition("read")
        .expect("interactive session read tool");
    let (_controller, abort) = AbortController::new();
    let result = (read.execute)(ToolCallContext {
        tool_call_id: "read-installed-coordinate-skill".to_owned(),
        arguments: json!({ "path": "skill://coordinate" }),
        on_update: Arc::new(|_| {}),
        abort,
        model: None,
    })
    .await?;
    assert!(result.content.iter().any(
        |block| matches!(block, ContentBlock::Text { text, .. } if text.contains("coordinate skill body"))
    ));

    let application = Application::new(session).await;
    let commands = application.commands_catalog();
    assert_eq!(commands.iter().filter(|command| command.name == "skill:coordinate").count(), 1);
    assert!(!commands.iter().any(|command| command.name == "skill:disabled-review"));
    let expanded = application
        .expand_resource_command("skill:coordinate", "coordinate this change")?
        .expect("installed coordinate skill command");
    assert_eq!(expanded.matches("coordinate skill body").count(), 1, "{expanded}");
    assert!(expanded.contains("location=\"skill://coordinate\""), "{expanded}");
    assert!(expanded.ends_with("coordinate this change"), "{expanded}");
    application.prompt(expanded.clone(), Vec::new(), None).await?;
    application.wait_for_idle().await;
    let contexts = contexts.lock().expect("captured contexts");
    let model_context = serde_json::to_string(&contexts[0])?;
    assert_eq!(model_context.matches("coordinate skill body").count(), 1, "{model_context}");
    drop(contexts);
    let disabled = application
        .expand_resource_command("skill:disabled-review", "")
        .expect_err("disabled skill must not be invokable");
    assert!(disabled.to_string().contains("disabled"), "{disabled:#}");
    let unknown = application
        .expand_resource_command("skill:not-installed", "")
        .expect_err("unknown skill must not be invokable");
    assert!(unknown.to_string().contains("unknown"), "{unknown:#}");
    application.cleanup().await;
    Ok(())
}
