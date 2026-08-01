use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{Arc, atomic::{AtomicUsize, Ordering}},
    time::Duration,
};

use pi_agent::{AbortController, AgentEvent, QueueMode, ToolCallContext};
use pi_ai::providers::{
    FauxProviderOptions, FauxProviderRegistration, FauxResponse, register_faux_provider,
};
use pi_ai::{ContentBlock, Model, StopReason};
use pi_coding::{
    AgentRuntimeSettings, Application, ApplicationEvent, ExtensionCapability, ExtensionMode,
    ExtensionOrigin, ExtensionPermissionSet, ExtensionRuntime, ExtensionRuntimeOptions,
    ExtensionSpec, ExtensionSpecRuntime, JobStatus, OrchestrationSettings, ResourceManager,
    ResourceManagerOptions, Session, SessionOptions, StreamingBehavior, TaskItem,
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

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
fn session_with_recorded_contexts(
    responses: Vec<FauxResponse>,
    contexts: Arc<parking_lot::Mutex<Vec<pi_ai::Context>>>,
) -> (Session, FauxProviderRegistration) {
    let suffix = uuid::Uuid::now_v7().to_string();
    let api = format!("application-context-test-api-{suffix}");
    let provider = format!("application-context-test-provider-{suffix}");
    let model = Model {
        id: "application-context-test-model".to_owned(),
        name: "Application Context Test Model".to_owned(),
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
    let stream_fn: pi_agent::StreamFn = Arc::new(move |model, context, options| {
        let contexts = contexts.clone();
        Box::pin(async move {
            contexts.lock().push(context.clone());
            pi_ai::stream_simple(model, context, options).await
        })
    });
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
        stream_fn: Some(stream_fn),
        auth_resolver: None,
    })
    .expect("build context-recording session");
    (session, registration)
}

fn bun_executable() -> Option<PathBuf> {
    if let Some(configured) = std::env::var_os("PI_BUN_EXECUTABLE") {
        let configured = PathBuf::from(configured);
        if configured.is_file() {
            return Some(configured);
        }
    }
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|directory| directory.join(if cfg!(windows) { "bun.exe" } else { "bun" }))
            .find(|candidate| candidate.is_file())
    })
}

async fn semantic_runtime_with_source(bun: &Path, source: Option<&str>) -> anyhow::Result<(ExtensionRuntime, ExtensionPermissionSet, Option<tempfile::TempDir>)> {
    let temporary;
    let fixture = if let Some(source) = source {
        let directory = tempfile::tempdir()?;
        let entry = directory.path().join("extension.ts");
        std::fs::write(&entry, source)?;
        temporary = Some(directory);
        entry
    } else {
        temporary = None;
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/extensions/hook-transforms.ts")
    };
    let permissions = ExtensionPermissionSet {
        capabilities: BTreeSet::from([ExtensionCapability::EventHooks]),
        ui_capabilities: BTreeSet::new(),
    };
    let mut spec = ExtensionSpec::new_runtime(
        "hook-transforms",
        ExtensionSpecRuntime::Bun { entry: fixture.clone() },
        fixture.parent().expect("fixture parent"),
        ExtensionOrigin::Project,
        true,
        permissions.clone(),
    );
    spec.environment.insert(
        "PI_BUN_EXECUTABLE".to_owned(),
        bun.to_string_lossy().into_owned(),
    );
    let runtime = ExtensionRuntime::process(
        None,
        ExtensionRuntimeOptions {
            mode: ExtensionMode::Tui,
            hook_timeout: Duration::from_secs(10),
            ..ExtensionRuntimeOptions::default()
        },
    );
    let report = runtime.load(vec![spec]).await;
    anyhow::ensure!(report.failures.is_empty(), "{:?}", report.failures);
    Ok((runtime, permissions, temporary))
}

async fn semantic_runtime(bun: &Path) -> anyhow::Result<(ExtensionRuntime, ExtensionPermissionSet)> {
    let (runtime, permissions, _temporary) = semantic_runtime_with_source(bun, None).await?;
    Ok((runtime, permissions))
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
async fn reload_reconfigures_parent_orchestration_tools_and_todo_atomically() {
    let root = tempfile::tempdir().expect("root");
    let agent_dir = root.path().join("agent-home");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::write(
        agent_dir.join("settings.json"),
        r#"{"orchestration":{"tasks":false,"process":false,"todo":false,"glob":false,"maxConcurrency":1}}"#,
    )
    .expect("settings");

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

    let tool_names = |application: &Application| {
        application
            .get_all_tools()
            .into_iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>()
    };
    for name in ["task", "hub", "process", "todo", "glob"] {
        assert!(!tool_names(&application).iter().any(|tool| tool == name), "unexpected {name}");
        assert!(!application.get_active_tool_names().iter().any(|tool| tool == name), "active {name}");
    }

    resources
        .settings_manager()
        .update_global(|settings| {
            settings.orchestration = Some(OrchestrationSettings {
                tasks: Some(true),
                process: Some(true),
                todo: Some(true),
                glob: Some(true),
                ..OrchestrationSettings::default()
            });
        })
        .expect("enable settings");
    application.reload().await.expect("enable orchestration");

    let enabled = tool_names(&application);
    let active = application.get_active_tool_names();
    for name in ["task", "hub", "process", "todo", "glob"] {
        assert!(enabled.iter().any(|tool| tool == name), "missing {name} from all_tools");
        assert!(active.iter().any(|tool| tool == name), "missing active {name}");
    }
    assert!(application.orchestration_runtime().is_some());

    let todo = application
        .get_all_tools()
        .into_iter()
        .find(|tool| tool.name == "todo")
        .expect("parent todo tool");
    let (_, abort) = AbortController::new();
    let result = (todo.execute)(ToolCallContext {
        tool_call_id: "parent-todo-init".to_owned(),
        arguments: serde_json::json!({
            "op": "init",
            "list": [{ "phase": "Build", "items": ["wire parent todo"] }]
        }),
        on_update: Arc::new(|_| {}),
        abort,
        model: None,
    })
    .await
    .expect("execute parent todo");
    assert_eq!(result.details["op"], "init");
    let state = application.todo_state();
    assert_eq!(state.phases.len(), 1);
    assert_eq!(state.phases[0].name, "Build");
    assert_eq!(state.phases[0].tasks[0].content, "wire parent todo");

    resources
        .settings_manager()
        .update_global(|settings| {
            settings.orchestration = Some(OrchestrationSettings {
                tasks: Some(false),
                process: Some(false),
                todo: Some(false),
                glob: Some(false),
                ..OrchestrationSettings::default()
            });
        })
        .expect("disable settings");
    application.reload().await.expect("disable orchestration");
    let disabled = tool_names(&application);
    let active = application.get_active_tool_names();
    for name in ["task", "hub", "process", "todo", "glob"] {
        assert!(!disabled.iter().any(|tool| tool == name), "stale {name} in all_tools");
        assert!(!active.iter().any(|tool| tool == name), "stale active {name}");
    }
    assert!(application.orchestration_runtime().is_none());
    assert!(application
        .set_active_tools_by_name(&["todo".to_owned()])
        .await
        .is_err());

    application.cleanup().await;
    registration.unregister();
}

#[tokio::test]
async fn reload_applies_agent_enablement_and_model_live() {
    let root = tempfile::tempdir().expect("root");
    let agent_dir = root.path().join("agent-home");
    std::fs::create_dir_all(agent_dir.join("agents")).expect("agents dir");
    std::fs::write(
        agent_dir.join("settings.json"),
        r#"{"orchestration":{"tasks":true,"process":true,"todo":true,"maxConcurrency":1}}"#,
    )
    .expect("settings");
    std::fs::write(
        agent_dir.join("agents").join("reviewer.md"),
        "---\nname: reviewer\ndescription: review code\n---\nYou review code.\n",
    )
    .expect("reviewer agent");

    let mut options = ResourceManagerOptions::new(root.path());
    options.agent_dir = agent_dir;
    options.disable_extensions = true;
    options.disable_skills = true;
    options.disable_prompt_templates = true;
    options.disable_themes = true;
    options.disable_context_files = true;
    let resources = ResourceManager::new(options).expect("resources");
    let (session, registration) = session_with_responses(Vec::new());
    session
        .attach_resources(resources.clone())
        .await
        .expect("attach resources");
    let application = Application::new(session).await;
    application.reload().await.expect("enable orchestration");

    let runtime = application
        .orchestration_runtime()
        .expect("orchestration runtime");
    let task_tool = runtime
        .agent_tools("Main", 0)
        .into_iter()
        .find(|tool| tool.name == "task")
        .expect("task tool");
    assert!(
        task_tool.description.contains("reviewer —"),
        "enabled reviewer must be advertised: {}",
        task_tool.description
    );
    assert_eq!(
        runtime.select_agent("reviewer", None),
        "reviewer",
        "default selector settings must route matching user text without explicit configuration",
    );

    resources
        .settings_manager()
        .update_global(|settings| {
            settings.agents.insert(
                "reviewer".to_owned(),
                AgentRuntimeSettings {
                    enabled: Some(false),
                    model: Some("openai/gpt-4.1".to_owned()),
                tools: None,
            },
            );
        })
        .expect("disable reviewer");
    application
        .reload()
        .await
        .expect("reload after agent settings save");

    let runtime = application
        .orchestration_runtime()
        .expect("orchestration runtime after reload");
    let task_tool = runtime
        .agent_tools("Main", 0)
        .into_iter()
        .find(|tool| tool.name == "task")
        .expect("task tool");
    assert!(
        !task_tool.description.contains("reviewer —"),
        "disabled reviewer must leave task catalog: {}",
        task_tool.description
    );
    assert!(
        runtime.ensure_agent_enabled("reviewer").is_err(),
        "disabled agent must fail ensure_agent_enabled"
    );

    // Selector catalog must also omit disabled agents.
    let plan = application.session().select_for_request("review code").await;
    assert!(
        plan.agents.iter().all(|hit| hit.name != "reviewer"),
        "disabled agent must not be selectable: {plan:?}"
    );

    application.cleanup().await;
    registration.unregister();
}

fn controlled_reload_session(
    cwd: &Path,
) -> (
    Session,
    FauxProviderRegistration,
    Arc<AtomicUsize>,
    Arc<Notify>,
    CancellationToken,
) {
    let suffix = uuid::Uuid::now_v7().to_string();
    let api = format!("reload-retention-api-{suffix}");
    let provider = format!("reload-retention-provider-{suffix}");
    let model = Model {
        id: "reload-retention-model".to_owned(),
        name: "Reload Retention Model".to_owned(),
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
    let started_count = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let release = CancellationToken::new();
    let stream_started_count = started_count.clone();
    let stream_started = started.clone();
    let stream_release = release.clone();
    let stream_fn: pi_agent::StreamFn = Arc::new(move |model, _context, options| {
        let started_count = stream_started_count.clone();
        let started = stream_started.clone();
        let release = stream_release.clone();
        Box::pin(async move {
            let stream = pi_ai::new_assistant_message_event_stream();
            let producer = stream.clone();
            tokio::spawn(async move {
                started_count.fetch_add(1, Ordering::SeqCst);
                started.notify_waiters();
                let aborted = match options.stream.abort_signal {
                    Some(abort) => tokio::select! {
                        () = release.cancelled() => false,
                        () = abort.cancelled() => true,
                    },
                    None => {
                        release.cancelled().await;
                        false
                    }
                };
                let mut message = pi_ai::AssistantMessage::pending(&model);
                if aborted {
                    message.stop_reason = StopReason::Aborted;
                } else {
                    message.content.push(ContentBlock::text("reload retained result"));
                    message.stop_reason = StopReason::Stop;
                }
                producer.end(Some(message)).await;
            });
            stream
        })
    });
    let session = Session::new(SessionOptions {
        model,
        cwd: cwd.to_path_buf(),
        system_prompt: String::new(),
        thinking_level: pi_agent::ThinkingLevel::Off,
        api_key: String::new(),
        compaction: None,
        stream_options: Default::default(),
        tools: Some(Vec::new()),
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: Some(stream_fn),
        auth_resolver: None,
    })
    .expect("controlled reload session");
    (session, registration, started_count, started, release)
}

async fn wait_for_reload_child(count: &AtomicUsize, notify: &Notify) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let notified = notify.notified();
            if count.load(Ordering::SeqCst) > 0 {
                break;
            }
            notified.await;
        }
    })
    .await
    .expect("child started");
}

fn orchestration_resources(root: &Path, agent_dir: PathBuf) -> ResourceManager {
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::write(
        agent_dir.join("settings.json"),
        r#"{"orchestration":{"tasks":true,"maxConcurrency":1}}"#,
    )
    .expect("settings");
    let mut options = ResourceManagerOptions::new(root);
    options.agent_dir = agent_dir;
    options.disable_extensions = true;
    options.disable_skills = true;
    options.disable_prompt_templates = true;
    options.disable_themes = true;
    options.disable_context_files = true;
    ResourceManager::new(options).expect("resources")
}

#[tokio::test]
async fn equivalent_reload_retains_running_and_retained_jobs() {
    let root = tempfile::tempdir().expect("root");
    let resources = orchestration_resources(root.path(), root.path().join("agent-home"));
    let (session, registration, started_count, started, release) =
        controlled_reload_session(root.path());
    session
        .attach_resources(resources)
        .await
        .expect("attach resources");
    let application = Application::new(session).await;
    application.reload().await.expect("enable orchestration");
    let before = application.orchestration_runtime().expect("runtime");
    let group_id = before.group_id().to_owned();
    let spawn = before
        .spawn_tasks(
            "Main",
            0,
            vec![TaskItem {
                index: 0,
                id: "ReloadSurvivor".to_owned(),
                agent: "task".to_owned(),
                assignment: "remain active across reload".to_owned(),
            }],
        )
        .expect("spawn")
        .remove(0);
    wait_for_reload_child(&started_count, &started).await;

    application.reload().await.expect("equivalent reload");
    let after_running = application.orchestration_runtime().expect("retained runtime");
    assert_eq!(after_running.group_id(), group_id);
    assert_eq!(after_running.active_child_count(), 1);
    assert_eq!(
        after_running.jobs(Some(std::slice::from_ref(&spawn.job_id)))[0].status,
        JobStatus::Running,
    );

    release.cancel();
    let settled = after_running
        .wait_jobs(
            std::slice::from_ref(&spawn.job_id),
            Some(Duration::from_secs(2)),
            None,
        )
        .await
        .expect("settled job");
    assert_eq!(settled[0].status, JobStatus::Completed);
    application.reload().await.expect("equivalent retained reload");
    let after_retained = application
        .orchestration_runtime()
        .expect("runtime after retained reload");
    assert_eq!(after_retained.group_id(), group_id);
    assert_eq!(
        after_retained.jobs(Some(std::slice::from_ref(&spawn.job_id)))[0].status,
        JobStatus::Completed,
    );

    application.cleanup().await;
    registration.unregister();
}

#[tokio::test]
async fn non_equivalent_reload_cleans_running_and_retained_job_state() {
    let root = tempfile::tempdir().expect("root");
    let resources = orchestration_resources(root.path(), root.path().join("agent-home"));
    let (session, registration, started_count, started, _release) =
        controlled_reload_session(root.path());
    session
        .attach_resources(resources.clone())
        .await
        .expect("attach resources");
    let application = Application::new(session).await;
    application.reload().await.expect("enable orchestration");
    let previous = application.orchestration_runtime().expect("runtime");
    let previous_group = previous.group_id().to_owned();
    let spawn = previous
        .spawn_tasks(
            "Main",
            0,
            vec![TaskItem {
                index: 0,
                id: "ReloadCleanup".to_owned(),
                agent: "task".to_owned(),
                assignment: "cancel on non-equivalent reload".to_owned(),
            }],
        )
        .expect("spawn")
        .remove(0);
    wait_for_reload_child(&started_count, &started).await;
    resources
        .settings_manager()
        .update_global(|settings| {
            settings.orchestration = Some(OrchestrationSettings {
                tasks: Some(true),
                max_concurrency: Some(2),
                ..OrchestrationSettings::default()
            });
        })
        .expect("change orchestration settings");

    application.reload().await.expect("non-equivalent reload");
    let replacement = application.orchestration_runtime().expect("replacement runtime");
    assert_ne!(replacement.group_id(), previous_group);
    assert!(replacement.jobs(None).is_empty());
    assert_eq!(previous.active_child_count(), 0);
    assert!(previous
        .jobs(Some(std::slice::from_ref(&spawn.job_id)))
        .is_empty());
    assert!(previous.resolve_read_uri("artifact://ReloadCleanup").is_err());
    assert!(previous.list("Main").is_empty());

    application.cleanup().await;
    registration.unregister();
}

#[tokio::test]
async fn extension_session_reducers_cancel_without_mutation_and_transform_tree() -> anyhow::Result<()> {
    let Some(bun) = bun_executable() else { return Ok(()); };
    let (session, registration) = session_with_responses(vec![FauxResponse::text("answer")]);
    session.start_new_recording()?;
    session.run("question", Vec::new()).await?;
    let user_id = session
        .session_entries(None)?
        .entries
        .into_iter()
        .find(|entry| matches!(entry.message, Some(pi_ai::Message::User(_))))
        .expect("recorded user entry")
        .id;
    let (runtime, permissions) = semantic_runtime(&bun).await?;
    let application = Application::new_with_extensions(session, runtime.clone(), permissions).await;
    let before = application.state().await;
    let history = application.messages();

    assert!(application.new_session().await.expect_err("cancel new").to_string().contains("cancelled"));
    assert!(application.switch_session(Path::new("missing.jsonl")).await.expect_err("cancel resume").to_string().contains("cancelled"));
    assert!(application.fork_session(&user_id).await.expect_err("cancel fork").to_string().contains("cancelled"));
    let unchanged = application.state().await;
    assert_eq!(unchanged.session_id, before.session_id);
    assert_eq!(unchanged.session_file, before.session_file);
    assert_eq!(application.messages(), history);

    let navigation = application
        .navigate_tree(&user_id, pi_coding::NavigateTreeOptions::default())
        .await?;
    assert!(navigation.changed);
    let summary_id = navigation.summary_entry_id.expect("extension summary entry");
    let entries = application.session_entries(None)?.entries;
    assert!(entries.iter().any(|entry| entry.id == summary_id && entry.summary.as_deref() == Some("fixture summary")));
    assert!(entries.iter().any(|entry| entry.target_id.as_deref() == Some(summary_id.as_str()) && entry.label.as_deref() == Some("fixture-label")));

    application.cleanup().await;
    runtime.shutdown().await;
    registration.unregister();
    Ok(())
}

#[tokio::test]
async fn extension_fork_skip_conversation_restore_creates_empty_live_context() -> anyhow::Result<()> {
    let Some(bun) = bun_executable() else { return Ok(()); };
    let (session, registration) = session_with_responses(vec![FauxResponse::text("answer")]);
    session.start_new_recording()?;
    session.run("question", Vec::new()).await?;
    let user_id = session
        .session_entries(None)?
        .entries
        .into_iter()
        .find(|entry| matches!(entry.message, Some(pi_ai::Message::User(_))))
        .expect("recorded user entry")
        .id;
    let source = r#"export default function (pi) {
        pi.on("session_before_fork", () => ({ skipConversationRestore: true }));
    }"#;
    let (runtime, permissions, _temporary) = semantic_runtime_with_source(&bun, Some(source)).await?;
    let application = Application::new_with_extensions(session, runtime.clone(), permissions).await;

    assert_eq!(application.fork_session(&user_id).await?, "question");
    assert!(application.messages().is_empty());
    assert!(application.session_entries(None)?.entries.iter().all(|entry| entry.message.is_none()));

    application.cleanup().await;
    runtime.shutdown().await;
    registration.unregister();
    Ok(())
}


#[tokio::test]
async fn extension_context_snapshot_strips_model_headers() -> anyhow::Result<()> {
    let Some(bun) = bun_executable() else {
        return Ok(());
    };
    let secret = "Bearer extension-context-header-secret";
    let (session, registration) = session_with_responses(Vec::new());
    let mut model = session.model().expect("session model");
    model.headers = Some(std::collections::HashMap::from([
        ("Authorization".to_owned(), secret.to_owned()),
        ("X-Probe-Secret".to_owned(), "probe-extension-secret".to_owned()),
    ]));
    let expected_id = model.id.clone();
    let expected_provider = model.provider.clone();
    let expected_name = model.name.clone();
    session.set_model(model, String::new());

    let source = r#"
export default function (pi: any) {
  pi.registerCommand("probe-model", {
    handler: (_args: string, ctx: any) => ctx.model ?? null,
  });
}
"#;
    let directory = tempfile::tempdir()?;
    let entry = directory.path().join("extension.ts");
    std::fs::write(&entry, source)?;
    let permissions = ExtensionPermissionSet {
        capabilities: BTreeSet::from([ExtensionCapability::Commands]),
        ui_capabilities: BTreeSet::new(),
    };
    let mut spec = ExtensionSpec::new_runtime(
        "context-model-probe",
        ExtensionSpecRuntime::Bun { entry: entry.clone() },
        directory.path(),
        ExtensionOrigin::Project,
        true,
        permissions.clone(),
    );
    spec.environment.insert(
        "PI_BUN_EXECUTABLE".to_owned(),
        bun.to_string_lossy().into_owned(),
    );
    let runtime = ExtensionRuntime::process(
        None,
        ExtensionRuntimeOptions {
            mode: ExtensionMode::Tui,
            invocation_timeout: Duration::from_secs(10),
            ..ExtensionRuntimeOptions::default()
        },
    );
    let report = runtime.load(vec![spec]).await;
    anyhow::ensure!(report.failures.is_empty(), "{:?}", report.failures);
    let application = Application::new_with_extensions(session, runtime.clone(), permissions).await;

    let model_value = runtime
        .invoke_command("probe-model", String::new(), None, None)
        .await?;
    let encoded = serde_json::to_string(&model_value)?;
    assert!(
        !encoded.contains(secret) && !encoded.contains("probe-extension-secret"),
        "extension context model leaked credential headers: {encoded}"
    );
    assert_eq!(model_value["id"], expected_id);
    assert_eq!(model_value["provider"], expected_provider);
    assert_eq!(model_value["name"], expected_name);
    assert!(
        model_value.get("headers").is_none()
            || model_value.get("headers") == Some(&serde_json::Value::Null),
        "headers must be absent/null after host sanitization: {encoded}"
    );

    application.cleanup().await;
    runtime.shutdown().await;
    registration.unregister();
    Ok(())
}

#[tokio::test]
async fn loop_turns_project_events_use_session_path_and_suspend_on_switch() {
    let session_dir = tempfile::tempdir().expect("session dir");
    let model_contexts = Arc::new(parking_lot::Mutex::new(Vec::<pi_ai::Context>::new()));
    let (session, registration) = session_with_recorded_contexts(
        vec![FauxResponse::text("loop answer")],
        model_contexts.clone(),
    );
    let first_recorder = pi_coding::start_session_in(
        std::env::current_dir().expect("current directory"),
        session.model().as_ref(),
        Some("off"),
        Some(session_dir.path()),
        Some("loop-first"),
        None,
    )
    .expect("first recorder");
    let first_path = first_recorder.path();
    session.record(first_recorder).expect("attach first recorder");
    let second_recorder = pi_coding::start_session_in(
        std::env::current_dir().expect("current directory"),
        session.model().as_ref(),
        Some("off"),
        Some(session_dir.path()),
        Some("loop-second"),
        None,
    )
    .expect("second recorder");
    let second_path = second_recorder.path();
    second_recorder.persist_now().expect("persist second session");
    second_recorder.close().expect("close second session");

    let application = Application::new(session).await;
    let mut events = application.subscribe();
    let task = application
        .loop_create(pi_coding::LoopCreateRequest::immediate("3s", "run through session"))
        .await
        .expect("create loop");

    let mut saw_fired = false;
    let mut saw_finished = false;
    while !saw_finished {
        let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("loop event timeout")
            .expect("application event channel");
        match event {
            ApplicationEvent::Loop(pi_coding::LoopEvent::Fired { task_id, .. })
                if task_id == task.id => saw_fired = true,
            ApplicationEvent::Loop(pi_coding::LoopEvent::Finished { task_id, .. })
                if task_id == task.id => saw_finished = true,
            _ => {}
        }
    }
    assert!(saw_fired);
    application.wait_for_idle().await;
    let messages = application.messages();
    let scheduled = messages
        .iter()
        .find_map(|message| match message {
            pi_ai::Message::Custom(custom) if custom.custom_type == "loop_scheduled_turn" => Some(custom),
            _ => None,
        })
        .expect("loop turn must append a typed scheduled message");
    assert!(!scheduled.display);
    assert_eq!(scheduled.details.as_ref().and_then(|details| details["taskId"].as_str()), Some(task.id.as_str()));
    assert_eq!(scheduled.details.as_ref().and_then(|details| details["prompt"].as_str()), Some("run through session"));
    assert_eq!(scheduled.details.as_ref().and_then(|details| details["schedule"].as_str()), Some("every 3 seconds"));
    assert!(!messages.iter().any(|message| matches!(message, pi_ai::Message::User(_))), "scheduled wrapper must not be a public user message");
    let context = model_contexts.lock();
    let model_prompt = context
        .first()
        .and_then(|context| context.messages.iter().find_map(|message| match message {
            pi_ai::Message::User(user) => Some(user.content.iter().filter_map(|block| match block { ContentBlock::Text { text, .. } => Some(text.as_str()), _ => None }).collect::<String>()),
            _ => None,
        }))
        .expect("model receives loop wrapper as user content");
    assert!(model_prompt.contains("scheduled task execution"));
    assert!(model_prompt.contains("task "));
    assert!(model_prompt.contains("every 3 seconds"));
    assert!(model_prompt.ends_with("run through session"));
    let assistant = messages
        .iter()
        .find_map(|message| match message {
            pi_ai::Message::Assistant(assistant) => Some(assistant),
            _ => None,
        })
        .expect("loop turn must append an assistant message");
    assert_eq!(assistant.text(), "loop answer");

    let durable = application
        .loop_create(pi_coding::LoopCreateRequest {
            interval: "1h".to_owned(),
            prompt: "restore only in first session".to_owned(),
            fire_immediately: false,
            durable: true,
        })
        .await
        .expect("create durable loop");
    application
        .switch_session(&second_path)
        .await
        .expect("switch session");
    assert!(application.loop_list().await.expect("second session loops").is_empty());
    let mut removed_for_switch = false;
    while let Ok(event) = events.try_recv() {
        if matches!(
            event,
            ApplicationEvent::Loop(pi_coding::LoopEvent::Removed {
                task_id,
                reason: pi_coding::LoopRemovalReason::SessionChanged,
            }) if task_id == durable.id
        ) {
            removed_for_switch = true;
        }
    }
    assert!(removed_for_switch, "session switch must project loop suspension");

    application
        .switch_session(&first_path)
        .await
        .expect("restore first session");
    let restored = application.loop_list().await.expect("restored loops");
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].id, durable.id);
    assert_eq!(restored[0].prompt, "restore only in first session");

    application.cleanup().await;
    registration.unregister();
}

#[tokio::test]
async fn enabling_orchestration_on_reload_forwards_job_events() {
    let root = tempfile::tempdir().expect("root");
    let agent_dir = root.path().join("agent-home");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::write(
        agent_dir.join("settings.json"),
        r#"{"orchestration":{"tasks":false,"maxConcurrency":1}}"#,
    )
    .expect("settings");
    let mut options = ResourceManagerOptions::new(root.path());
    options.agent_dir = agent_dir;
    options.disable_extensions = true;
    options.disable_skills = true;
    options.disable_prompt_templates = true;
    options.disable_themes = true;
    options.disable_context_files = true;
    let resources = ResourceManager::new(options).expect("resources");
    let (session, registration) = session_with_responses(vec![FauxResponse::text("done")]);
    session.attach_resources(resources.clone()).await.expect("attach resources");
    let application = Application::new(session).await;
    let mut events = application.subscribe();

    resources
        .settings_manager()
        .update_global(|settings| {
            settings.orchestration = Some(OrchestrationSettings {
                tasks: Some(true),
                max_concurrency: Some(1),
                ..OrchestrationSettings::default()
            });
        })
        .expect("enable orchestration");
    application.reload().await.expect("reload enabling orchestration");
    let runtime = application.orchestration_runtime().expect("runtime after reload");
    let spawn = runtime
        .spawn_tasks(
            runtime.main_agent_id(),
            0,
            vec![TaskItem {
                index: 0,
                id: "ForwardedChild".to_owned(),
                agent: "task".to_owned(),
                assignment: "prove reload event forwarding".to_owned(),
            }],
        )
        .expect("spawn child")
        .remove(0);

    let forwarded = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let ApplicationEvent::Orchestration(
                pi_coding::OrchestrationEvent::JobUpdated { group_id, job },
            ) = events.recv().await.expect("application event channel")
                && job.id == spawn.job_id
            {
                break (group_id, job);
            }
        }
    })
    .await
    .expect("forwarded orchestration event timeout");
    assert_eq!(forwarded.0, runtime.group_id());
    assert_eq!(forwarded.1.description.as_deref(), Some("prove reload event forwarding"));

    application.cleanup().await;
    registration.unregister();
}

#[tokio::test]
async fn compact_persists_checkpoint_and_keeps_application_transcript_usable() {
    let contexts = Arc::new(parking_lot::Mutex::new(Vec::<pi_ai::Context>::new()));
    let (session, registration) = session_with_recorded_contexts(
        vec![FauxResponse::text("application checkpoint"), FauxResponse::text("after compact")],
        contexts.clone(),
    );
    let session_dir = tempfile::tempdir().expect("session dir");
    let recorder = pi_coding::start_session_in(
        std::env::current_dir().expect("current directory"),
        session.model().as_ref(),
        Some("off"),
        Some(session_dir.path()),
        Some("application-compact"),
        None,
    )
    .expect("recorder");
    let mut assistant = pi_ai::AssistantMessage::pending(&session.model().expect("model"));
    assistant.content = vec![ContentBlock::text("old answer")];
    assistant.stop_reason = StopReason::Stop;
    assistant.timestamp = 2;
    let history = vec![
        pi_ai::Message::user_text("old request ".repeat(20), 1),
        pi_ai::Message::Assistant(assistant),
        pi_ai::Message::user_text("recent request", 3),
    ];
    for message in &history {
        recorder.record_message(message).expect("record history message");
    }
    session.record(recorder).expect("attach recorder");
    session.load_history(history).await.expect("load history");
    session.enable_compaction(pi_coding::CompactionSettings {
        enabled: true,
        reserve_tokens: 20,
        keep_recent_tokens: 4,
    });
    let application = Application::new(session).await;

    let result = application.compact(Some("preserve decisions")).await.expect("compact");
    assert!(result.summary.ends_with("application checkpoint"), "{}", result.summary);
    assert!(application
        .session_entries(None)
        .expect("session entries")
        .entries
        .iter()
        .any(|entry| entry.entry_type == "compaction" && entry.summary.as_deref() == Some(result.summary.as_str())));
    assert!(matches!(application.messages().first(), Some(pi_ai::Message::CompactionSummary(_))));

    application.prompt("continue from checkpoint".into(), Vec::new(), None).await.expect("prompt");
    application.wait_for_idle().await;
    assert_eq!(application.last_assistant_text().as_deref(), Some("after compact"));
    let captured = contexts.lock();
    assert_eq!(captured.len(), 2, "compact must summarize once and the next prompt must run once");
    let first_prompt = captured[1].messages.first().expect("checkpoint context");
    let pi_ai::Message::User(user) = first_prompt else { panic!("checkpoint must project to user context: {first_prompt:?}") };
    assert!(user.content.iter().any(|block| matches!(block, ContentBlock::Text { text, .. } if text.contains("application checkpoint"))));

    application.cleanup().await;
    registration.unregister();
}
