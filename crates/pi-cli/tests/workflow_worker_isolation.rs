//! Workflow workers never leak into ordinary agent-definition discovery.
//!
//! Contract: spawning orchestration workers creates runtime child sessions
//! and durable [`pi_coding::PersistedDefinition`]s inside the orchestration
//! sidecar, but never adds rows to disk agent-definition discovery.

use std::sync::Arc;

use futures_util::FutureExt;
use parking_lot::Mutex;
use pi_agent::ThinkingLevel;
use pi_ai::{AssistantMessage, AssistantMessageEvent, Context, Model, StopReason};
use pi_coding::{
    AgentCatalog, AgentDefinition, AgentDefinitionSource, Application, ChildSessionFactory,
    OrchestrationConfig, OrchestrationRuntime, Session, SessionOptions, start_session_in,
};

fn model() -> Model {
    Model {
        id: "worker-iso-model".to_owned(),
        name: "Worker Isolation model".to_owned(),
        api: "worker-iso-api".to_owned(),
        provider: "worker-iso-provider".to_owned(),
        base_url: "http://example.invalid".to_owned(),
        ..Model::default()
    }
}

fn register_provider() -> pi_ai::providers::FauxProviderRegistration {
    let model = model();
    pi_ai::providers::register_faux_provider(pi_ai::providers::FauxProviderOptions {
        api: model.api.clone(),
        provider: model.provider.clone(),
        models: vec![model],
        chunk_size: 4,
    })
}

fn session_store_root() -> std::path::PathBuf {
    let dir = tempfile::tempdir().expect("session store tempdir");
    let path = dir.path().to_path_buf();
    std::mem::forget(dir);
    path
}

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

fn definition_agent(name: &str) -> AgentDefinition {
    AgentDefinition {
        name: name.to_owned(),
        description: format!("{name} on-disk definition"),
        system_prompt: format!("{name} PROMPT"),
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

/// A spawned worker whose name does NOT exist as a disk definition — exactly
/// the case where a leak into definition discovery would be visible.
const WORKER_NAME: &str = "ephemeral-worker";

fn parent_session() -> Session {
    let stream_fn: pi_agent::StreamFn = Arc::new(|model: Model, _context, _| {
        let model = model.clone();
        async move {
            let events = pi_ai::new_assistant_message_event_stream();
            let writer = events.clone();
            tokio::spawn(async move {
                // The parent streams one task-tool-free completion; the spawn
                // below goes through the runtime API directly so this test
                // stays independent of NL selection.
                let mut message = AssistantMessage::pending(&model);
                message.stop_reason = StopReason::Stop;
                writer
                    .push(AssistantMessageEvent::Done {
                        reason: StopReason::Stop,
                        message: message.clone(),
                    })
                    .await;
                writer.end(Some(message)).await;
            });
            events
        }
        .boxed()
    });
    let session = Session::new(SessionOptions {
        model: model(),
        cwd: std::env::current_dir().expect("cwd"),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: "worker-iso".to_owned(),
        compaction: None,
        stream_options: Default::default(),
        tools: Some(Vec::new()),
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: Some(stream_fn),
        auth_resolver: None,
        auth_manager: None,
    })
    .expect("parent session");
    recorded_session(session)
}

fn child_factory(contexts: Arc<Mutex<Vec<Context>>>) -> ChildSessionFactory {
    Arc::new(move |request| {
        let contexts = contexts.clone();
        Box::pin(async move {
            let stream: pi_agent::StreamFn = Arc::new(move |model: Model, context, _| {
                let contexts = contexts.clone();
                let model = model.clone();
                async move {
                    contexts.lock().push(context);
                    let events = pi_ai::new_assistant_message_event_stream();
                    let writer = events.clone();
                    tokio::spawn(async move {
                        let mut message = AssistantMessage::pending(&model);
                        message.stop_reason = StopReason::Stop;
                        writer
                            .push(AssistantMessageEvent::Done {
                                reason: StopReason::Stop,
                                message: message.clone(),
                            })
                            .await;
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
                    api_key: "worker-iso-child".to_owned(),
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
    })
}

#[tokio::test]
async fn spawned_workflow_workers_stay_out_of_definition_discovery() {
    let root = tempfile::tempdir().expect("tempdir");
    let definitions = vec![definition_agent("researcher"), definition_agent("writer")];
    let mut config =
        OrchestrationConfig::new(AgentCatalog::from_agents(definitions.clone()), root.path());
    config.default_agent = "researcher".to_owned();
    config.parent_model = model();
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let runtime =
        OrchestrationRuntime::new(config, child_factory(contexts.clone())).expect("runtime");

    let _provider = register_provider();
    let application = Application::new_with_orchestration(parent_session(), runtime.clone()).await;

    // Spawn a worker whose name exists nowhere in definition discovery.
    let tools = runtime.agent_tools("Main", 0);
    let task_tool = tools
        .iter()
        .find(|tool| tool.name == "task")
        .expect("task tool registered")
        .clone();
    let (_, abort) = pi_agent::AbortController::new();
    let spawn_result = (task_tool.execute)(pi_agent::ToolCallContext {
        tool_call_id: "spawn-worker".to_owned(),
        arguments: serde_json::json!({
            "context": "isolation probe",
            "tasks": [{ "name": WORKER_NAME, "task": "probe" }]
        }),
        on_update: Arc::new(|_| {}),
        abort,
        model: None,
        approval_token: None,
    })
    .await
    .expect("task spawn");
    let spawns: Vec<pi_coding::TaskSpawn> =
        serde_json::from_value(spawn_result.details).expect("spawns");
    assert_eq!(spawns.len(), 1, "one worker spawned");
    assert!(!spawns[0].job_id.is_empty());

    // The worker is a real runtime actor: hub sees its job.
    let jobs = runtime.jobs(None);
    assert!(
        jobs.iter()
            .any(|job| job.agent_id == WORKER_NAME || job.agent == WORKER_NAME),
        "worker must exist in the orchestration registry: {jobs:?}"
    );

    // Definition discovery must not expose the ephemeral worker. The parent
    // Application owns its independently discovered user/project catalog.
    let snapshot = application
        .resource_snapshot()
        .expect("resource snapshot attached");
    let discovered: Vec<&str> = snapshot
        .agents
        .iter()
        .map(|agent| agent.name.as_str())
        .collect();
    assert!(
        !discovered.contains(&WORKER_NAME),
        "workflow worker leaked into discovery"
    );

    // No worker definition file was written under the scope either.
    let agents_dir = root.path().join("agents");
    if agents_dir.exists() {
        let entries: Vec<String> = std::fs::read_dir(&agents_dir)
            .expect("read agents dir")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            !entries.iter().any(|name| name.contains(WORKER_NAME)),
            "worker definition file written to disk: {entries:?}"
        );
    }

    application.cleanup().await;
}
