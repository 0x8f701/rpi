use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::Mutex;
use pi_agent::{AbortController, ThinkingLevel};
use pi_ai::providers::{FauxProviderOptions, FauxResponse, register_faux_provider};
use pi_ai::{Model, SimpleStreamOptions};
use pi_coding::{
    AgentCatalog, AgentDefinition, AgentDefinitionSource, AgentDiscoveryOptions, AgentStatus,
    ChildSessionFactory, OrchestrationConfig, OrchestrationRuntime, Session, SessionOptions,
    TaskItem,
};

fn definition(name: &str) -> AgentDefinition {
    AgentDefinition {
        name: name.to_owned(),
        description: format!("{name} description"),
        system_prompt: format!("{name} prompt"),
        tools: Some(Vec::new()),
        autoload_skills: Vec::new(),
        model: None,
        thinking_level: Some(ThinkingLevel::Off),
        source: AgentDefinitionSource::Bundled,
        path: None,
        trusted: true,
    }
}

fn faux_factory(
    registrations: Arc<Mutex<HashMap<String, pi_ai::providers::FauxProviderRegistration>>>,
) -> ChildSessionFactory {
    Arc::new(move |request| {
        let registrations = registrations.clone();
        Box::pin(async move {
            let registration = registrations
                .lock()
                .get(&request.child_id)
                .cloned()
                .expect("registration for child");
            let child_model = registration.model(None).expect("registered child model");
            Session::new(SessionOptions {
                model: child_model,
                cwd: std::env::current_dir().expect("cwd"),
                system_prompt: request.system_prompt,
                thinking_level: request.thinking_level.unwrap_or(ThinkingLevel::Off),
                api_key: "faux".to_owned(),
                compaction: None,
                stream_options: SimpleStreamOptions::default(),
                tools: Some(request.orchestration_tools),
                before_tool_call: None,
                after_tool_call: None,
                stream_fn: None,
                auth_resolver: None,
            })
        })
    })
}

fn runtime_with_responses(
    artifact_dir: &std::path::Path,
    responses: Vec<(&str, FauxResponse)>,
    max_concurrency: usize,
    mailbox_capacity: usize,
    max_recursion_depth: usize,
) -> (
    OrchestrationRuntime,
    Vec<pi_ai::providers::FauxProviderRegistration>,
) {
    let suffix = uuid::Uuid::now_v7().to_string();
    let model = Model {
        id: format!("orchestration-model-{suffix}"),
        name: "Orchestration Test".to_owned(),
        api: format!("orchestration-api-{suffix}"),
        provider: format!("orchestration-provider-{suffix}"),
        ..Model::default()
    };
    let mut registrations = Vec::new();
    let mut by_id = HashMap::new();
    for (index, (child_id, response)) in responses.into_iter().enumerate() {
        let mut child_model = model.clone();
        child_model.api = format!("{}-{index}", model.api);
        child_model.provider = format!("{}-{index}", model.provider);
        let registration = register_faux_provider(FauxProviderOptions {
            api: child_model.api.clone(),
            provider: child_model.provider.clone(),
            models: vec![child_model],
            chunk_size: 1,
        });
        registration.set_responses(vec![response]);
        by_id.insert(child_id.to_owned(), registration.clone());
        registrations.push(registration);
    }
    let factory = faux_factory(Arc::new(Mutex::new(by_id)));
    let mut config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![definition("task")]),
        artifact_dir,
    );
    config.max_concurrency = max_concurrency;
    config.mailbox_capacity = mailbox_capacity;
    config.max_recursion_depth = max_recursion_depth;
    let runtime = OrchestrationRuntime::new(config, factory).expect("runtime");
    (runtime, registrations)
}

#[test]
fn discovery_is_first_wins_and_project_trust_gated() {
    let root = tempfile::tempdir().expect("root");
    let cwd = root.path().join("project");
    let agent_dir = root.path().join("agent");
    std::fs::create_dir_all(cwd.join(".pi/agents")).expect("project agents");
    std::fs::create_dir_all(agent_dir.join("agents")).expect("user agents");
    std::fs::write(
        cwd.join(".pi/agents/task.md"),
        "---\nname: task\ndescription: project\nautoloadSkills: [rust]\n---\nproject prompt",
    )
    .expect("project definition");
    std::fs::write(
        agent_dir.join("agents/task.md"),
        "---\nname: task\ndescription: user\n---\nuser prompt",
    )
    .expect("user definition");
    std::fs::write(
        agent_dir.join("agents/review.md"),
        "---\nname: review\ndescription: review\ntools:\n  - read\n  - grep\n---\nreview prompt",
    )
    .expect("review definition");

    let mut options = AgentDiscoveryOptions::new(&cwd, &agent_dir);
    let untrusted = AgentCatalog::discover(&options).expect("untrusted catalog");
    assert_eq!(untrusted.get("task").expect("task").description, "user");
    assert!(untrusted.get("review").is_some());
    assert!(untrusted.agents().iter().all(|agent| agent.trusted));

    options.project_trusted = true;
    let trusted = AgentCatalog::discover(&options).expect("trusted catalog");
    let task = trusted.get("task").expect("project task");
    assert_eq!(task.description, "project");
    assert_eq!(task.autoload_skills, vec!["rust"]);
    assert_eq!(task.source, AgentDefinitionSource::Project);
}

#[tokio::test]
async fn selector_settings_choose_the_deterministic_matching_agent() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let mut review = definition("reviewer");
    review.description = "Reviews patches for correctness and security".to_owned();
    let config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![definition("task"), review]),
        artifacts.path(),
    )
    .with_selector_settings(pi_coding::SelectorSettings {
        auto_select_threshold: 1,
        confidence_margin: 0,
        min_score: 0,
        ..pi_coding::SelectorSettings::default()
    });
    let runtime = OrchestrationRuntime::new(config, Arc::new(|_| {
        Box::pin(async { panic!("selector test must not create a child") })
    }))
    .expect("runtime");
    assert_eq!(runtime.select_agent("review this security patch", None), "reviewer");
    assert_eq!(runtime.select_agent("review this security patch", Some("task")), "task");
    runtime.shutdown().await;
}

#[tokio::test]
async fn parent_factory_disables_ambient_discovery_and_forbidden_child_tools() {
    let root = tempfile::tempdir().expect("root");
    std::fs::create_dir_all(root.path().join(".pi/skills/local")).expect("skill dir");
    std::fs::write(
        root.path().join(".pi/skills/local/SKILL.md"),
        "---\nname: local\ndescription: local skill\n---\nlocal",
    )
    .expect("skill");
    let parent = Session::new(SessionOptions {
        model: Model::default(),
        cwd: root.path().to_path_buf(),
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
    })
    .expect("parent");
    let mut child_definition = definition("child");
    child_definition.tools = Some(vec!["read".to_owned(), "todo".to_owned(), "process".to_owned(), "task".to_owned(), "hub".to_owned()]);
    let child = OrchestrationRuntime::child_factory_from_session(&parent)(pi_coding::ChildSessionRequest {
        child_id: "Child".to_owned(),
        parent_id: "Main".to_owned(),
        max_tools_per_agent: 16,
        depth: 1,
        definition: child_definition,
        assignment: "inspect".to_owned(),
        system_prompt: "child".to_owned(),
        requested_tool_names: Some(vec!["read".to_owned(), "todo".to_owned(), "process".to_owned(), "task".to_owned(), "hub".to_owned()]),
        orchestration_tools: Vec::new(),
        thinking_level: None,
    })
    .await
    .expect("child");
    assert_eq!(child.get_active_tool_names(), vec!["read"]);
    assert!(child.select_for_request("local skill").await.skills.is_empty());
}

#[test]
fn orchestration_settings_are_explicitly_off_by_default() {
    let runtime = pi_coding::Settings::default().runtime_settings().expect("runtime settings");
    assert!(!runtime.orchestration_enabled);
    assert!(!runtime.process_tool_enabled);
    assert!(!runtime.todo_tool_enabled);
}

#[tokio::test]
async fn batch_results_are_correlated_and_artifacts_resolve() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (runtime, registrations) = runtime_with_responses(
        artifacts.path(),
        vec![
            ("First", FauxResponse::text("first")),
            ("Second", FauxResponse::text("second")),
        ],
        2,
        100,
        2,
    );
    let (_, abort) = AbortController::new();
    let results = runtime
        .run_tasks(
            "Main",
            0,
            vec![
                TaskItem {
                    index: 1,
                    id: "Second".to_owned(),
                    agent: "task".to_owned(),
                    assignment: "second assignment".to_owned(),
                },
                TaskItem {
                    index: 0,
                    id: "First".to_owned(),
                    agent: "task".to_owned(),
                    assignment: "first assignment".to_owned(),
                },
            ],
            abort,
        )
        .await
        .expect("batch");
    assert_eq!(
        results
            .iter()
            .map(|result| result.index)
            .collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert_eq!(results[0].output, "first");
    assert_eq!(results[1].output, "second");
    for result in &results {
        assert_eq!(result.status, AgentStatus::Idle);
        assert!(
            runtime
                .resolve_agent_reference(&result.id)
                .expect("agent ref")
                .is_file()
        );
        assert!(
            runtime
                .resolve_history_reference(&result.id)
                .expect("history ref")
                .is_file()
        );
        assert_eq!(
            runtime.resolve_read_uri(&result.artifact_ref).expect("artifact URI"),
            runtime.resolve_agent_reference(&result.id).expect("agent ref")
        );
        assert_eq!(
            runtime.resolve_read_uri(&result.history_ref).expect("history URI"),
            runtime.resolve_history_reference(&result.id).expect("history ref")
        );
        assert_eq!(result.artifact_uri, format!("artifact://{}", result.id));
        assert_eq!(
            runtime.resolve_read_uri(&result.artifact_uri).expect("artifact alias"),
            runtime.resolve_agent_reference(&result.id).expect("agent ref")
        );
    }
    let read = pi_coding::read_tool_with_resolver(
        artifacts.path().to_str().expect("artifact cwd"),
        None,
        Some(runtime.read_uri_resolver()),
    );
    let (_, read_abort) = AbortController::new();
    let read_result = (read.execute)(pi_agent::ToolCallContext {
        tool_call_id: "read-artifact".to_owned(),
        arguments: serde_json::json!({ "path": results[0].artifact_ref }),
        on_update: Arc::new(|_| {}),
        abort: read_abort,
        model: None,
    })
    .await
    .expect("read artifact URI");
    assert!(matches!(
        read_result.content.first(),
        Some(pi_ai::ContentBlock::Text { text, .. }) if text.contains("first")
    ));
    assert_eq!(
        runtime.send("First", "Second", "peer result", None)[0].outcome,
        pi_coding::DeliveryOutcome::Queued
    );
    let sibling_inbox = runtime.inbox("Second", false);
    assert_eq!(sibling_inbox.len(), 1);
    assert_eq!(sibling_inbox[0].from, "First");
    assert_eq!(sibling_inbox[0].body, "peer result");
    runtime.shutdown().await;
    for registration in registrations {
        registration.unregister();
    }
}

#[tokio::test]
async fn batch_never_exceeds_configured_concurrency() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let model = Model {
        id: "concurrency-model".to_owned(),
        name: "Concurrency Model".to_owned(),
        api: "concurrency-api".to_owned(),
        provider: "concurrency-provider".to_owned(),
        ..Model::default()
    };
    let current = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(tokio::sync::Notify::new());
    let release = tokio_util::sync::CancellationToken::new();
    let observed_peak = peak.clone();
    let observed_started = started.clone();
    let release_all = release.clone();
    let factory: ChildSessionFactory = Arc::new(move |request| {
        let model = model.clone();
        let current = current.clone();
        let peak = peak.clone();
        let started = started.clone();
        let release = release.clone();
        Box::pin(async move {
            let stream_fn: pi_agent::StreamFn = Arc::new(move |model, _context, _options| {
                let current = current.clone();
                let peak = peak.clone();
                let started = started.clone();
                let release = release.clone();
                Box::pin(async move {
                    let stream = pi_ai::new_assistant_message_event_stream();
                    let producer = stream.clone();
                    tokio::spawn(async move {
                        let active = current.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(active, Ordering::SeqCst);
                        started.notify_waiters();
                        release.cancelled().await;
                        current.fetch_sub(1, Ordering::SeqCst);
                        let mut message = pi_ai::AssistantMessage::pending(&model);
                        message.content.push(pi_ai::ContentBlock::text("done"));
                        message.stop_reason = pi_ai::StopReason::Stop;
                        producer.end(Some(message)).await;
                    });
                    stream
                })
            });
            Session::new(SessionOptions {
                model,
                cwd: std::env::current_dir().expect("cwd"),
                system_prompt: request.system_prompt,
                thinking_level: ThinkingLevel::Off,
                api_key: String::new(),
                compaction: None,
                stream_options: Default::default(),
                tools: Some(request.orchestration_tools),
                before_tool_call: None,
                after_tool_call: None,
                stream_fn: Some(stream_fn),
                auth_resolver: None,
            })
        })
    });
    let mut config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![definition("task")]),
        artifacts.path(),
    );
    config.max_concurrency = 2;
    let runtime = OrchestrationRuntime::new(config, factory).expect("runtime");
    let (_, abort) = AbortController::new();
    let runner = runtime.clone();
    let run = tokio::spawn(async move {
        runner
            .run_tasks(
                "Main",
                0,
                (0..3)
                    .map(|index| TaskItem {
                        index,
                        id: format!("Concurrent{index}"),
                        agent: "task".to_owned(),
                        assignment: format!("work {index}"),
                    })
                    .collect(),
                abort,
            )
            .await
            .expect("batch")
    });
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let notified = observed_started.notified();
            if observed_peak.load(Ordering::SeqCst) == 2 {
                break;
            }
            notified.await;
        }
    })
    .await
    .expect("two children started");
    assert_eq!(observed_peak.load(Ordering::SeqCst), 2);
    release_all.cancel();
    assert_eq!(run.await.expect("run join").len(), 3);
    assert_eq!(observed_peak.load(Ordering::SeqCst), 2);
    runtime.shutdown().await;
}

#[tokio::test]
async fn mailbox_cap_wait_and_peer_roster_are_enforced() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (runtime, registrations) = runtime_with_responses(
        artifacts.path(),
        vec![("Worker", FauxResponse::text("done"))],
        1,
        2,
        2,
    );
    let (_, abort) = AbortController::new();
    runtime
        .run_tasks(
            "Main",
            0,
            vec![TaskItem {
                index: 0,
                id: "Worker".to_owned(),
                agent: "task".to_owned(),
                assignment: "finish".to_owned(),
            }],
            abort,
        )
        .await
        .expect("child");

    assert_eq!(runtime.send("Main", "Worker", "one", None)[0].outcome, pi_coding::DeliveryOutcome::Queued);
    assert_eq!(runtime.send("Main", "Worker", "two", None)[0].outcome, pi_coding::DeliveryOutcome::Queued);
    assert_eq!(runtime.send("Main", "Worker", "three", None)[0].outcome, pi_coding::DeliveryOutcome::Failed);
    let inbox = runtime.inbox("Worker", true);
    assert_eq!(inbox.len(), 2);
    assert_eq!(inbox[0].body, "one");
    assert_eq!(inbox[1].body, "two");

    let wait_runtime = runtime.clone();
    let waiter = tokio::spawn(async move {
        wait_runtime
            .wait_message(
                "Main",
                Some("Worker"),
                Some(std::time::Duration::from_secs(1)),
                None,
            )
            .await
            .expect("wait")
            .expect("message")
    });
    tokio::task::yield_now().await;
    assert_eq!(runtime.send("Worker", "Main", "hello main", None)[0].outcome, pi_coding::DeliveryOutcome::Queued);
    assert_eq!(waiter.await.expect("wait join").body, "hello main");
    let peers = runtime.list("Worker");
    assert!(peers.iter().any(|peer| peer.id == "Main"));

    runtime.shutdown().await;
    for registration in registrations {
        registration.unregister();
    }
}

#[tokio::test]
async fn recursion_cutoff_removes_task_and_shutdown_cleans_children() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (runtime, registrations) = runtime_with_responses(
        artifacts.path(),
        vec![("Unused", FauxResponse::text("unused"))],
        1,
        100,
        1,
    );
    let root_tools = runtime.agent_tools("Main", 0);
    assert_eq!(
        root_tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        vec!["task", "hub"]
    );
    let child_tools = runtime.agent_tools("Child", 1);
    assert_eq!(
        child_tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        vec!["hub"]
    );
    runtime.shutdown().await;
    assert_eq!(runtime.active_child_count(), 0);
    for registration in registrations {
        registration.unregister();
    }
}

#[tokio::test]
async fn cancellation_aborts_a_running_child() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let suffix = uuid::Uuid::now_v7().to_string();
    let model = Model {
        id: format!("cancel-model-{suffix}"),
        name: "Cancel Model".to_owned(),
        api: format!("cancel-api-{suffix}"),
        provider: format!("cancel-provider-{suffix}"),
        ..Model::default()
    };
    let started = Arc::new(tokio::sync::Notify::new());
    let observed = Arc::new(AtomicUsize::new(0));
    let stream_started = started.clone();
    let stream_observed = observed.clone();
    let stream_fn: pi_agent::StreamFn = Arc::new(move |model, _context, options| {
        let started = stream_started.clone();
        let observed = stream_observed.clone();
        Box::pin(async move {
            let stream = pi_ai::new_assistant_message_event_stream();
            let producer = stream.clone();
            tokio::spawn(async move {
                started.notify_waiters();
                if let Some(abort) = options.stream.abort_signal {
                    abort.cancelled().await;
                    observed.fetch_add(1, Ordering::SeqCst);
                }
                let mut message = pi_ai::AssistantMessage::pending(&model);
                message.stop_reason = pi_ai::StopReason::Aborted;
                producer.end(Some(message)).await;
            });
            stream
        })
    });
    let factory: ChildSessionFactory = Arc::new(move |request| {
        let model = model.clone();
        let stream_fn = stream_fn.clone();
        Box::pin(async move {
            Session::new(SessionOptions {
                model,
                cwd: std::env::current_dir().expect("cwd"),
                system_prompt: request.system_prompt,
                thinking_level: ThinkingLevel::Off,
                api_key: String::new(),
                compaction: None,
                stream_options: Default::default(),
                tools: Some(request.orchestration_tools),
                before_tool_call: None,
                after_tool_call: None,
                stream_fn: Some(stream_fn),
                auth_resolver: None,
            })
        })
    });
    let runtime = OrchestrationRuntime::new(
        OrchestrationConfig::new(
            AgentCatalog::from_agents(vec![definition("task")]),
            artifacts.path(),
        ),
        factory,
    )
    .expect("runtime");
    let (_, abort) = AbortController::new();
    let run_runtime = runtime.clone();
    let run = tokio::spawn(async move {
        run_runtime
            .run_tasks(
                "Main",
                0,
                vec![TaskItem {
                    index: 0,
                    id: "CancelMe".to_owned(),
                    agent: "task".to_owned(),
                    assignment: "wait".to_owned(),
                }],
                abort,
            )
            .await
            .expect("cancel batch")
    });
    started.notified().await;
    assert_eq!(runtime.cancel(&["CancelMe".to_owned()]), vec!["CancelMe"]);
    let results = run.await.expect("run join");
    assert_eq!(results[0].status, AgentStatus::Aborted);
    assert_eq!(observed.load(Ordering::SeqCst), 1);
    runtime.shutdown().await;
    assert_eq!(runtime.active_child_count(), 0);
}

#[tokio::test]
async fn dropping_runtime_owner_cancels_children() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let model = Model {
        id: "drop-model".to_owned(),
        name: "Drop Model".to_owned(),
        api: "drop-api".to_owned(),
        provider: "drop-provider".to_owned(),
        ..Model::default()
    };
    let started = Arc::new(tokio::sync::Notify::new());
    let aborted = Arc::new(tokio::sync::Notify::new());
    let stream_started = started.clone();
    let stream_aborted = aborted.clone();
    let stream_fn: pi_agent::StreamFn = Arc::new(move |model, _context, options| {
        let started = stream_started.clone();
        let aborted = stream_aborted.clone();
        Box::pin(async move {
            let stream = pi_ai::new_assistant_message_event_stream();
            let producer = stream.clone();
            tokio::spawn(async move {
                started.notify_waiters();
                options
                    .stream
                    .abort_signal
                    .expect("abort signal")
                    .cancelled()
                    .await;
                aborted.notify_waiters();
                let mut message = pi_ai::AssistantMessage::pending(&model);
                message.stop_reason = pi_ai::StopReason::Aborted;
                producer.end(Some(message)).await;
            });
            stream
        })
    });
    let factory: ChildSessionFactory = Arc::new(move |request| {
        let model = model.clone();
        let stream_fn = stream_fn.clone();
        Box::pin(async move {
            Session::new(SessionOptions {
                model,
                cwd: std::env::current_dir().expect("cwd"),
                system_prompt: request.system_prompt,
                thinking_level: ThinkingLevel::Off,
                api_key: String::new(),
                compaction: None,
                stream_options: Default::default(),
                tools: Some(request.orchestration_tools),
                before_tool_call: None,
                after_tool_call: None,
                stream_fn: Some(stream_fn),
                auth_resolver: None,
            })
        })
    });
    let runtime = OrchestrationRuntime::new(
        OrchestrationConfig::new(
            AgentCatalog::from_agents(vec![definition("task")]),
            artifacts.path(),
        ),
        factory,
    )
    .expect("runtime");
    let (_, abort) = AbortController::new();
    let runner = runtime.clone();
    let task = tokio::spawn(async move {
        runner
            .run_tasks(
                "Main",
                0,
                vec![TaskItem {
                    index: 0,
                    id: "DropMe".to_owned(),
                    agent: "task".to_owned(),
                    assignment: "wait".to_owned(),
                }],
                abort,
            )
            .await
    });
    started.notified().await;
    drop(runtime);
    tokio::time::timeout(std::time::Duration::from_secs(1), aborted.notified())
        .await
        .expect("child observed owner drop");
    let results = task.await.expect("task join").expect("task results");
    assert_eq!(results[0].status, AgentStatus::Aborted);
}
