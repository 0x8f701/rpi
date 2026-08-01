use std::time::Duration;

use pi_agent::{AgentEvent, QueueMode};
use pi_ai::providers::{
    FauxProviderOptions, FauxProviderRegistration, FauxResponse, register_faux_provider,
};
use pi_ai::{ContentBlock, Model, StopReason};
use pi_coding::{Application, ApplicationEvent, Session, SessionOptions, StreamingBehavior};
use pi_coding::{OrchestrationSettings, ResourceManager, ResourceManagerOptions};

fn session_with_responses(responses: Vec<FauxResponse>) -> (Session, FauxProviderRegistration) {
    let suffix = uuid::Uuid::now_v7().to_string();
    let api = format!("application-test-api-{suffix}");
    let provider = format!("application-test-provider-{suffix}");
    let model = Model {
        id: "application-test-model".to_owned(),
        name: "Application Test Model".to_owned(),
        api: api.clone(),
        provider: provider.clone(),
        ..Model::default()
    };
    let registration = register_faux_provider(FauxProviderOptions {
        api,
        provider,
        models: vec![model.clone()],
        chunk_size: 1,
    });
    registration.set_responses(responses);
    let session = Session::new(SessionOptions {
        model,
        cwd: std::env::current_dir().expect("current directory"),
        system_prompt: String::new(),
        thinking_level: pi_agent::ThinkingLevel::Off,
        api_key: String::new(),
        compaction: None,
        stream_options: Default::default(),
        tools: Some(Vec::new()),
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: None,
        auth_resolver: None,
    })
    .expect("build session");
    (session, registration)
}

#[tokio::test]
async fn publishes_agent_events_and_settled_signal() {
    let (session, registration) = session_with_responses(vec![FauxResponse::text("hello")]);
    let application = Application::new(session).await;
    let mut events = application.subscribe();

    application
        .prompt("say hello".to_owned(), Vec::new(), None)
        .await
        .expect("accept prompt");

    let mut saw_text = false;
    let mut saw_settled = false;
    while !saw_settled {
        let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("event timeout")
            .expect("event channel");
        match event {
            ApplicationEvent::Agent(AgentEvent::MessageUpdate {
                assistant_message_event: pi_ai::AssistantMessageEvent::TextDelta { delta, .. },
                ..
            }) if !delta.is_empty() => saw_text = true,
            ApplicationEvent::AgentSettled => saw_settled = true,
            _ => {}
        }
    }

    application.wait_for_idle().await;
    assert!(saw_text);
    assert_eq!(application.state().await.message_count, 2);
    registration.unregister();
}

#[tokio::test]
async fn rejects_another_prompt_without_streaming_behavior() {
    let long_text = "streaming response ".repeat(200);
    let (session, registration) = session_with_responses(vec![FauxResponse {
        content: vec![ContentBlock::text(long_text)],
        stop_reason: StopReason::Stop,
        error_message: None,
    }]);
    let application = Application::new(session).await;

    application
        .prompt("first".to_owned(), Vec::new(), None)
        .await
        .expect("accept first prompt");
    let error = application
        .prompt("second".to_owned(), Vec::new(), None)
        .await
        .expect_err("reject concurrent prompt");
    assert!(error.to_string().contains("choose steer or followUp"));

    application.abort().await;
    application.wait_for_idle().await;
    registration.unregister();
}

#[tokio::test]
async fn accepts_steering_during_an_active_run() {
    let long_text = "streaming response ".repeat(200);
    let (session, registration) = session_with_responses(vec![
        FauxResponse {
            content: vec![ContentBlock::text(long_text)],
            stop_reason: StopReason::Stop,
            error_message: None,
        },
        FauxResponse::text("steered"),
    ]);
    let application = Application::new(session).await;

    application
        .prompt("first".to_owned(), Vec::new(), None)
        .await
        .expect("accept first prompt");
    application
        .prompt(
            "change direction".to_owned(),
            Vec::new(),
            Some(StreamingBehavior::Steer),
        )
        .await
        .expect("accept steering prompt");

    application.wait_for_idle().await;
    assert!(application.state().await.message_count >= 2);
    registration.unregister();
}

#[tokio::test]
async fn state_tracks_modes_pending_counts_and_new_session_reset() {
    let (session, registration) = session_with_responses(Vec::new());
    let application = Application::new(session).await;
    application.set_steering_mode(QueueMode::All).await;
    application.set_follow_up_mode(QueueMode::All).await;
    application.set_auto_compaction_enabled(true);
    application
        .set_session_name("  State\nTest  ")
        .expect("set name");
    application.steer("steer".to_owned(), Vec::new()).await;
    application.follow_up("follow".to_owned(), Vec::new()).await;

    let queued = application.state().await;
    assert!(!queued.is_streaming);
    assert_eq!(queued.steering_mode, QueueMode::All);
    assert_eq!(queued.follow_up_mode, QueueMode::All);
    assert_eq!(queued.session_name.as_deref(), Some("State Test"));
    assert!(queued.auto_compaction_enabled);
    assert_eq!(queued.pending_message_count, 2);

    application.new_session().await.expect("new session");
    let reset = application.state().await;
    assert!(!reset.is_streaming);
    assert!(!reset.is_compacting);
    assert_eq!(reset.message_count, 0);
    assert_eq!(reset.pending_message_count, 0);
    assert_eq!(reset.session_name, None);
    assert_eq!(reset.steering_mode, QueueMode::All);
    assert_eq!(reset.follow_up_mode, QueueMode::All);
    assert!(reset.session_id.is_some());
    assert!(reset.session_file.is_some());
    registration.unregister();
}

#[tokio::test]
async fn navigation_and_fork_events_serialize_as_typed_application_events() {
    let (session, registration) = session_with_responses(vec![FauxResponse::text("answer")]);
    session.start_new_recording().expect("start recorder");
    session
        .run("question", Vec::new())
        .await
        .expect("record turn");
    let application = Application::new(session).await;
    let user_id = application
        .session_entries(None)
        .expect("entries")
        .entries
        .into_iter()
        .find(|entry| matches!(entry.message, Some(pi_ai::Message::User(_))))
        .expect("user entry")
        .id;

    let mut events = application.subscribe();
    let result = application
        .navigate_tree(&user_id, pi_coding::NavigateTreeOptions::default())
        .await
        .expect("navigate");
    assert_eq!(result.editor_text.as_deref(), Some("question"));
    let before = events.recv().await.expect("before tree");
    let after = events.recv().await.expect("after tree");
    assert_eq!(
        serde_json::to_value(before).expect("serialize")["type"],
        "session_before_tree"
    );
    assert_eq!(
        serde_json::to_value(after).expect("serialize")["type"],
        "session_tree"
    );

    let previous = application
        .state()
        .await
        .session_file
        .expect("session file");
    let prompt = application
        .fork_session(&user_id)
        .await
        .expect("fork session");
    assert_eq!(prompt, "question");
    assert_ne!(
        application.state().await.session_file.as_deref(),
        Some(previous.as_str())
    );
    let fork_file = application.state().await.session_file.expect("fork session file");
    let fork_tree = pi_coding::load_session_tree(&fork_file).expect("load persisted fork");
    assert_eq!(fork_tree.header.parent_session.as_deref(), Some(previous.as_str()));
    let before = events.recv().await.expect("before fork");
    let after = events.recv().await.expect("after fork");
    assert_eq!(
        serde_json::to_value(before).expect("serialize")["type"],
        "session_before_fork"
    );
    let serialized = serde_json::to_value(after).expect("serialize");
    assert_eq!(serialized["type"], "session_forked");
    assert_eq!(serialized["editorText"], "question");
    registration.unregister();
}

#[tokio::test]
async fn queue_snapshot_and_drain_preserve_steering_then_follow_up_order() {
    let (session, registration) = session_with_responses(Vec::new());
    let application = Application::new(session).await;
    let mut events = application.subscribe();
    application.steer("steer one".to_owned(), Vec::new()).await;
    application.steer("steer two".to_owned(), Vec::new()).await;
    application.follow_up("follow one".to_owned(), Vec::new()).await;
    let mut queue_updates = Vec::new();
    while queue_updates.len() < 3 {
        match tokio::time::timeout(Duration::from_secs(2), events.recv()).await.expect("queue event timeout").expect("queue event") {
            ApplicationEvent::Session(pi_coding::SessionEvent::QueueUpdate { steering, follow_up }) => {
                queue_updates.push((steering.len(), follow_up.len()));
            }
            _ => {}
        }
    }
    assert_eq!(queue_updates, [(1, 0), (2, 0), (2, 1)]);
    let (steering, follow_up) = application.queued_messages().await;
    assert_eq!(steering.len(), 2);
    assert_eq!(follow_up.len(), 1);
    let (steering, follow_up) = application.drain_queued_messages().await;
    let text = |message: &pi_ai::Message| match message {
        pi_ai::Message::User(message) => message.content.iter().filter_map(|block| if let ContentBlock::Text { text, .. } = block { Some(text.as_str()) } else { None }).collect::<String>(),
        other => panic!("unexpected queued message: {other:?}"),
    };
    assert_eq!(steering.iter().map(text).collect::<Vec<_>>(), ["steer one", "steer two"]);
    assert_eq!(follow_up.iter().map(text).collect::<Vec<_>>(), ["follow one"]);
    assert_eq!(application.state().await.pending_message_count, 0);
    loop {
        match tokio::time::timeout(Duration::from_secs(2), events.recv()).await.expect("drain event timeout").expect("drain event") {
            ApplicationEvent::Session(pi_coding::SessionEvent::QueueUpdate { steering, follow_up }) => {
                assert!(steering.is_empty());
                assert!(follow_up.is_empty());
                break;
            }
            _ => {}
        }
    }
    registration.unregister();
}

#[tokio::test]
async fn foreground_bash_updates_and_end_forward_through_application_events() {
    let (session, registration) = session_with_responses(Vec::new());
    let application = Application::new(session).await;
    let mut events = application.subscribe();
    let result = application.execute_bash("printf app".to_owned(), false).await.expect("application bash");
    assert_eq!(result.output, "app");
    let mut saw_update = false;
    let mut saw_end = false;
    while !saw_end {
        let event = tokio::time::timeout(Duration::from_secs(2), events.recv()).await.expect("event timeout").expect("event channel");
        match event {
            ApplicationEvent::Session(pi_coding::SessionEvent::BashExecutionUpdate { delta, .. }) if delta == "app" => saw_update = true,
            ApplicationEvent::Session(pi_coding::SessionEvent::BashExecutionEnd { message }) => {
                assert_eq!(message.command, "printf app");
                assert_eq!(message.output, "app");
                let serialized = serde_json::to_value(ApplicationEvent::Session(
                    pi_coding::SessionEvent::BashExecutionEnd { message: message.clone() },
                ))
                .expect("serialize bash end");
                assert_eq!(serialized["type"], "bash_execution_end");
                assert_eq!(serialized["message"]["exitCode"], 0);
                assert!(serialized["message"].get("excludeFromContext").is_none());
                saw_end = true;
            }
            _ => {}
        }
    }
    assert!(saw_update);
    registration.unregister();
}

#[tokio::test]
async fn reload_reconfigures_orchestration_and_all_tools_atomically() {
    let root = tempfile::tempdir().expect("root");
    let agent_dir = root.path().join("agent-home");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::write(agent_dir.join("settings.json"), r#"{"orchestration":{"tasks":true,"process":true,"todo":true,"maxConcurrency":1}}"#).expect("settings");

    let mut options = ResourceManagerOptions::new(root.path());
    options.agent_dir = agent_dir;
    options.disable_extensions = true;
    options.disable_skills = true;
    options.disable_prompt_templates = true;
    options.disable_themes = true;
    options.disable_context_files = true;
    let resources = ResourceManager::new(options).expect("resources");
    let (session, registration) = session_with_responses(Vec::new());
    session.attach_resources(resources.clone()).await.expect("attach resources");
    let application = Application::new(session).await;

    application.reload().await.expect("enable orchestration");
    let enabled = application
        .get_all_tools()
        .into_iter()
        .map(|tool| tool.name)
        .collect::<Vec<_>>();
    for name in ["task", "hub", "process", "todo"] {
        assert!(enabled.iter().any(|tool| tool == name), "missing {name}");
    }
    application
        .set_active_tools_by_name(&["task".to_owned(), "hub".to_owned()])
        .await
        .expect("activate orchestration tools");

    resources
        .settings_manager()
        .update_global(|settings| {
            settings.orchestration = Some(OrchestrationSettings {
                tasks: Some(false),
                process: Some(false),
                todo: Some(false),
                ..OrchestrationSettings::default()
            });
        })
        .expect("disable settings");
    application.reload().await.expect("disable orchestration");
    let disabled = application
        .get_all_tools()
        .into_iter()
        .map(|tool| tool.name)
        .collect::<Vec<_>>();
    for name in ["task", "hub", "process", "todo"] {
        assert!(!disabled.iter().any(|tool| tool == name), "stale {name}");
    }
    assert!(application.orchestration_runtime().is_none());
    assert!(application
        .set_active_tools_by_name(&["task".to_owned()])
        .await
        .is_err());

    application.cleanup().await;
    registration.unregister();
}
