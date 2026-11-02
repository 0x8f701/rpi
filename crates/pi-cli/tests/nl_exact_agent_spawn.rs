//! Main-interpreted named delegation through an ordinary Task spawn.
//!
//! Contract: the exact user prompt reaches Main unchanged; Main emits one
//! ordinary `task` call, which always allocates a fresh child runtime and job.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures_util::FutureExt;
use parking_lot::Mutex;
use pi_agent::ThinkingLevel;
use pi_ai::{AssistantMessage, ContentBlock, Context, Message, Model, StopReason, ToolCall};
use pi_coding::{
    AgentCatalog, AgentDefinition, AgentDefinitionSource, Application, ApplicationEvent, AutoMode,
    ChildSessionFactory, JobStatus, OrchestrationConfig, OrchestrationEvent, OrchestrationRuntime,
    SelectorSettings, Session, SessionOptions, start_session_in,
};

fn model() -> Model {
    Model {
        id: "nl-spawn-model".to_owned(),
        name: "NL Spawn model".to_owned(),
        api: "nl-spawn-api".to_owned(),
        provider: "nl-spawn-provider".to_owned(),
        // Valid session HTTP settings: the sandbox authority resolves (and
        // stages) the model's provider HTTP authority during bootstrap.
        base_url: "http://localhost:8787".to_owned(),
        ..Model::default()
    }
}

/// The generated streams are supplied by the custom `stream_fn` below, but the
/// sandbox authority still requires a registered provider for the model's
/// `api` when staging the session's HTTP requirements.
fn register_nl_provider() -> pi_ai::providers::FauxProviderRegistration {
    let model = model();
    pi_ai::providers::register_faux_provider(pi_ai::providers::FauxProviderOptions {
        api: model.api.clone(),
        provider: model.provider.clone(),
        models: vec![model],
        chunk_size: 4,
    })
}

/// Hermetic session store root kept alive for the process (the `Session` holds
/// the path, not the `TempDir` guard).
fn session_store_root() -> std::path::PathBuf {
    let dir = tempfile::tempdir().expect("session store tempdir");
    let path = dir.path().to_path_buf();
    std::mem::forget(dir);
    path
}

/// Bind `session` to a freshly-created hermetic session store and attach a
/// recorder, so the sandbox authority can open the store during bootstrap and
/// orchestration can read the parent recording when binding a runtime.
fn recorded_session(session: Session) -> Session {
    let store = session_store_root();
    session.set_session_dir(store.clone());
    let recorder = start_session_in(
        &store,
        session.model().as_ref(),
        Some("off"),
        Some(&store),
        None,
        None,
    )
    .expect("start session recorder");
    session.record(recorder).expect("attach session recorder");
    session
}

fn agent(name: &str, description: &str, prompt: &str) -> AgentDefinition {
    AgentDefinition {
        name: name.to_owned(),
        description: description.to_owned(),
        system_prompt: prompt.to_owned(),
        tools: Some(Vec::new()),
        autoload_skills: Vec::new(),
        model: None,
        thinking_level: Some(ThinkingLevel::Off),
        max_turns: None,
        max_tool_calls: None,
        timeout_secs: None,
        disallowed_tools: Vec::new(),
        capability_ceiling: None,
        source: AgentDefinitionSource::User,
        path: None,
        trusted: true,
        kind: pi_coding::AgentDefinitionKind::Agent,
        personality: None,
        soft_budget: None,
        knowledge_roots: Vec::new(),
        user_proxy: false,
    }
}

fn selector_settings() -> SelectorSettings {
    SelectorSettings {
        min_score: 0,
        autoload_threshold: 1,
        auto_select_threshold: 1,
        confidence_margin: 0,
        auto_mode: AutoMode::Auto,
        ..SelectorSettings::default()
    }
}

fn parent_session(contexts: Arc<Mutex<Vec<Context>>>, tool_call_id: &'static str) -> Session {
    let calls = Arc::new(AtomicUsize::new(0));
    let session = Session::new(SessionOptions {
        model: model(),
        cwd: std::env::current_dir().expect("cwd"),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: "nl-spawn".to_owned(),
        compaction: None,
        stream_options: Default::default(),
        tools: Some(Vec::new()),
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: Some(Arc::new(move |model, context, _| {
            let contexts = contexts.clone();
            let call = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                contexts.lock().push(context);
                let events = pi_ai::new_assistant_message_event_stream();
                let writer = events.clone();
                tokio::spawn(async move {
                    let mut message = AssistantMessage::pending(&model);
                    if call == 0 {
                        message.content.push(ContentBlock::ToolCall(ToolCall {
                            id: tool_call_id.to_owned(),
                            name: "task".to_owned(),
                            arguments: serde_json::json!({
                                "name": "glm",
                                "agent": "glm",
                                "task": "审核下呢, 仔细检查可行性"
                            }),
                            thought_signature: None,
                        }));
                        message.stop_reason = StopReason::ToolUse;
                    } else {
                        message
                            .content
                            .push(ContentBlock::text("delegation started"));
                        message.stop_reason = StopReason::Stop;
                    }
                    writer.end(Some(message)).await;
                });
                events
            }
            .boxed()
        })),
        auth_resolver: None,
        auth_manager: None,
    })
    .expect("parent session");
    recorded_session(session)
}

#[tokio::test]
async fn conversational_glm_review_reaches_main_and_spawns_fresh_task_while_glm_runs() {
    const REQUEST: &str = "你也让glm审核下呢, 仔细检查可行性";
    const TASK_CALL_ID: &str = "task-glm-review";
    let root = tempfile::tempdir().expect("tempdir");
    let child_contexts = Arc::new(Mutex::new(Vec::new()));
    let parent_contexts = Arc::new(Mutex::new(Vec::new()));
    let run_index = Arc::new(AtomicUsize::new(0));
    let release_first = Arc::new(tokio::sync::Semaphore::new(0));
    let child_factory: ChildSessionFactory = Arc::new({
        let child_contexts = child_contexts.clone();
        let run_index = run_index.clone();
        let release_first = release_first.clone();
        move |request| {
            let child_contexts = child_contexts.clone();
            let index = run_index.fetch_add(1, Ordering::SeqCst);
            let release_first = release_first.clone();
            Box::pin(async move {
                let stream: pi_agent::StreamFn = Arc::new(move |model, context, _| {
                    let child_contexts = child_contexts.clone();
                    let release_first = release_first.clone();
                    async move {
                        child_contexts.lock().push(context);
                        let events = pi_ai::new_assistant_message_event_stream();
                        let writer = events.clone();
                        tokio::spawn(async move {
                            if index == 0 {
                                let permit = release_first
                                    .acquire()
                                    .await
                                    .expect("first-child release semaphore remains open");
                                permit.forget();
                            }
                            let mut message = AssistantMessage::pending(&model);
                            message.content.push(ContentBlock::text(if index == 0 {
                                "first result"
                            } else {
                                "fresh result"
                            }));
                            message.stop_reason = StopReason::Stop;
                            writer.end(Some(message)).await;
                        });
                        events
                    }
                    .boxed()
                });
                Ok(recorded_session(
                    Session::new(SessionOptions {
                        model: request.model,
                        cwd: std::env::current_dir().expect("cwd"),
                        system_prompt: request.system_prompt,
                        thinking_level: request.thinking_level.unwrap_or(ThinkingLevel::Off),
                        api_key: "nl-spawn-child".to_owned(),
                        compaction: None,
                        stream_options: Default::default(),
                        tools: Some(request.orchestration_tools),
                        before_tool_call: None,
                        after_tool_call: None,
                        stream_fn: Some(stream),
                        auth_resolver: None,
                        auth_manager: None,
                    })
                    .expect("child session"),
                ))
            })
        }
    });
    let mut config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![
            agent("glm", "Review feasibility carefully", "GLM_PROMPT"),
            agent("writer", "Write assigned content", "WRITER_PROMPT"),
        ]),
        root.path(),
    )
    .with_selector_settings(selector_settings());
    config.default_agent = "writer".to_owned();
    config.parent_model = model();
    let runtime = OrchestrationRuntime::new(config, child_factory).expect("runtime");

    let _provider = register_nl_provider();
    let application = Application::new_with_orchestration(
        parent_session(parent_contexts.clone(), TASK_CALL_ID),
        runtime.clone(),
    )
    .await;
    let mut events = application.subscribe();
    let first = runtime
        .spawn_tasks(
            "Main",
            0,
            vec![pi_coding::TaskItem {
                index: 0,
                id: "glm".to_owned(),
                agent: "glm".to_owned(),
                assignment: "review the existing implementation".to_owned(),
                ..Default::default()
            }],
        )
        .expect("first glm spawn");
    let first_job_id = first[0].job_id.clone();
    let first_agent_id = first[0].agent_id.clone();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if runtime
                .jobs(None)
                .iter()
                .any(|job| job.id == first_job_id && job.status == JobStatus::Running)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("first glm job reaches running state");

    let accepted = application
        .prompt(REQUEST.to_owned(), Vec::new(), None)
        .await
        .expect("Main accepts exact named delegation")
        .expect("ordinary prompt must admit a Main turn");
    application.wait_for_idle().await;
    assert_eq!(
        application
            .turn_lifecycle_snapshot()
            .last_terminal
            .as_ref()
            .map(|terminal| terminal.turn_id.as_str()),
        Some(accepted.turn_id.as_str()),
    );
    let parent_contexts = parent_contexts.lock();
    assert_eq!(
        parent_contexts.len(),
        2,
        "Task tool use plus terminal Main turn"
    );
    assert!(parent_contexts[0].messages.iter().any(|message| {
        matches!(message, Message::User(user) if user.content.iter().any(|block| {
            matches!(block, ContentBlock::Text { text, .. } if text == REQUEST)
        }))
    }));
    for context in parent_contexts.iter() {
        assert_eq!(
            context
                .messages
                .iter()
                .filter(|message| matches!(message, Message::User(user) if user.content.iter().any(|block| {
                    matches!(block, ContentBlock::Text { text, .. } if text == REQUEST)
                })))
                .count(),
            1,
            "the same admitted request remains in each turn context",
        );
    }
    drop(parent_contexts);

    let jobs = runtime.jobs(None);
    assert_eq!(jobs.len(), 2, "one existing and one fresh glm job");
    let first_job = jobs
        .iter()
        .find(|job| job.id == first_job_id)
        .expect("first job");
    let second_job = jobs
        .iter()
        .find(|job| job.id != first_job_id)
        .expect("fresh job");
    assert_eq!(first_job.agent_id, first_agent_id);
    assert_eq!(second_job.agent, "glm");
    assert_eq!(second_job.agent_id, "glm_2");
    assert_ne!(second_job.id, first_job.id);
    assert_ne!(second_job.agent_id, first_job.agent_id);
    assert_eq!(
        second_job.description.as_deref(),
        Some("审核下呢, 仔细检查可行性")
    );
    let second_job_id = second_job.id.clone();
    let second_agent_id = second_job.agent_id.clone();

    let task_results = application
        .session()
        .history()
        .into_iter()
        .filter_map(|message| match message {
            Message::ToolResult(result)
                if result.tool_call_id == TASK_CALL_ID && result.tool_name == "task" =>
            {
                Some(result)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        task_results.len(),
        1,
        "exactly one ordinary Task tool result"
    );
    let spawns: Vec<pi_coding::TaskSpawn> = serde_json::from_value(
        task_results[0]
            .details
            .clone()
            .expect("Task result details"),
    )
    .expect("Task spawn details");
    assert_eq!(spawns.len(), 1, "no preliminary child");
    assert_eq!(spawns[0].job_id, second_job_id);
    assert_eq!(spawns[0].agent_id, second_agent_id);
    let task_result_text = task_results[0]
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        task_result_text.contains(&second_job_id),
        "fresh spawn job id: {task_result_text}"
    );
    assert!(
        !task_result_text.contains(&first_job_id),
        "old job must not attach to the new Task result: {task_result_text}"
    );
    assert!(
        !task_result_text.contains("first result"),
        "old child output must not attach to the Task spawn receipt"
    );
    assert!(
        !task_result_text.contains("fresh result"),
        "child output arrives by authoritative job update, not the spawn receipt"
    );

    let fresh_settled = runtime
        .wait_jobs(
            std::slice::from_ref(&second_job_id),
            Some(Duration::from_secs(5)),
            None,
        )
        .await
        .expect("fresh child settles independently");
    let fresh_job = fresh_settled
        .into_iter()
        .find(|job| job.id == second_job_id)
        .expect("fresh settled job identity");
    assert_eq!(fresh_job.status, JobStatus::Completed);
    let fresh_output = &fresh_job
        .result
        .as_ref()
        .expect("fresh child result")
        .output;
    assert!(
        fresh_output.starts_with("fresh result"),
        "fresh job output: {fresh_output}"
    );
    assert!(
        !fresh_output.contains("first result"),
        "old child output crossed into the fresh job"
    );
    assert!(
        !fresh_output.contains("orchestration shut down"),
        "fresh child must settle normally"
    );

    let captured = child_contexts.lock();
    assert_eq!(captured.len(), 2, "existing and fresh child only");
    assert!(captured.iter().any(|context| {
        context.system_prompt.starts_with("GLM_PROMPT")
            && context.system_prompt.contains(
                "<delegated_assignment>\n审核下呢, 仔细检查可行性\n</delegated_assignment>",
            )
    }));
    drop(captured);
    assert_eq!(
        runtime
            .jobs(None)
            .into_iter()
            .find(|job| job.id == first_job_id)
            .expect("first job remains registered")
            .status,
        JobStatus::Running,
    );
    assert!(
        runtime
            .inbox(runtime.main_agent_id(), true)
            .expect("Main inbox")
            .is_empty()
    );

    release_first.add_permits(1);
    let first_settled = runtime
        .wait_jobs(
            std::slice::from_ref(&first_job_id),
            Some(Duration::from_secs(5)),
            None,
        )
        .await
        .expect("first job settles after stored release permit");
    let first_job = first_settled
        .into_iter()
        .find(|job| job.id == first_job_id)
        .expect("first settled job identity");
    assert_eq!(first_job.status, JobStatus::Completed);
    let first_output = &first_job
        .result
        .as_ref()
        .expect("first child result")
        .output;
    assert!(
        first_output.starts_with("first result"),
        "first job output: {first_output}"
    );
    assert!(
        !first_output.contains("fresh result"),
        "fresh child output crossed into the old job"
    );
    assert!(
        !first_output.contains("orchestration shut down"),
        "old child must settle normally"
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut saw_fresh_result = false;
    let mut saw_first_result = false;
    while !saw_fresh_result || !saw_first_result {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("both identity-bound job results must be published")
            .expect("application event channel remains open");
        match event {
            ApplicationEvent::RunFailed { message } => {
                panic!("named Task path emitted RunFailed: {message}");
            }
            ApplicationEvent::TodoChildRouted { receipt } => {
                panic!(
                    "non-CodeTask review was diverted to Todo child {}",
                    receipt.child_session_id
                );
            }
            ApplicationEvent::TurnTerminal {
                status, message, ..
            } => {
                assert_ne!(
                    status,
                    pi_coding::TurnTerminalStatus::Failed,
                    "parent turn failed: {message:?}"
                );
                assert!(
                    !message
                        .as_deref()
                        .is_some_and(|text| text.contains("orchestration shut down")),
                    "parent terminal shutdown: {message:?}"
                );
            }
            ApplicationEvent::Orchestration(OrchestrationEvent::JobUpdated { job, .. }) => {
                if let Some(result) = job.result.as_ref() {
                    assert!(
                        !result.output.contains("orchestration shut down"),
                        "job {} shutdown output",
                        job.id
                    );
                    assert!(
                        !result
                            .error
                            .as_deref()
                            .is_some_and(|error| error.contains("orchestration shut down")),
                        "job {} shutdown error",
                        job.id
                    );
                    if job.id == second_job_id {
                        assert!(
                            !result.output.contains("first result"),
                            "old output attached to fresh job event"
                        );
                        saw_fresh_result |= result.output.starts_with("fresh result");
                    } else if job.id == first_job_id {
                        assert!(
                            !result.output.contains("fresh result"),
                            "fresh output attached to old job event"
                        );
                        saw_first_result |= result.output.starts_with("first result");
                    }
                }
            }
            _ => {}
        }
    }
    while let Ok(event) = events.try_recv() {
        match event {
            ApplicationEvent::RunFailed { message } => {
                panic!("named Task path emitted late RunFailed: {message}");
            }
            ApplicationEvent::TurnTerminal { message, .. } => {
                assert!(
                    !message
                        .as_deref()
                        .is_some_and(|text| text.contains("orchestration shut down")),
                    "late parent terminal shutdown: {message:?}"
                );
            }
            _ => {}
        }
    }
    assert!(
        saw_fresh_result,
        "fresh output must publish only on the fresh job id"
    );
    assert!(
        saw_first_result,
        "old output must publish only on the old job id"
    );
    assert!(
        runtime
            .inbox(runtime.main_agent_id(), true)
            .expect("Main inbox after settlement")
            .is_empty()
    );
    application.cleanup().await;
}
