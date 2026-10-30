//! Durable child session tests: crash-durable child JSONL, atomic
//! orchestration state, restart recovery, and hub-triggered revival.
//!
//! Contracts defended:
//! - Durable recorder persists its header before a child turn can run
//! - Child JSONL parent linkage and canonical root isolation
//! - Strict state round-trip, corruption, wrong-parent, traversal, and symlink rejection
//! - Non-durable runtimes preserve existing spawn behavior
//! - Parked durable children revive by continuing the recorded transcript
//! - Interrupted jobs and agent statuses recover truthfully

use std::sync::Arc;
use std::time::Duration;

use pi_agent::{AbortController, AgentTool, ThinkingLevel, ToolCallContext};
use pi_ai::{Model, StopReason};
use pi_coding::{
    AgentCatalog, AgentDefinition, AgentDefinitionSource, AgentStatus, Application,
    ApplicationRuntimeCandidate, ChildSessionFactory, DeliveryOutcome, DurableState, JobStatus,
    OrchestrationConfig, OrchestrationRuntime, ResourceDiscovery, ResourceManager,
    ResourceManagerOptions, Session, SessionOptions, TaskSpawn,
    session_store::{self, start_durable_child_session_in},
};
use serde_json::{Value, json};

const DANGER_SETTINGS: &str = r#"{"sandbox":{"mode":"danger-full-access"}}"#;

fn definition() -> AgentDefinition {
    AgentDefinition {
        name: "task".to_owned(),
        description: "background task".to_owned(),
        system_prompt: "complete the assignment".to_owned(),
        tools: Some(Vec::new()),
        autoload_skills: Vec::new(),
        model: None,
        thinking_level: Some(ThinkingLevel::Off),
        max_turns: None,
        max_tool_calls: None,
        timeout_secs: None,
        disallowed_tools: Vec::new(),
        capability_ceiling: None,
        source: AgentDefinitionSource::Bundled,
        path: None,
        trusted: true,
        kind: pi_coding::AgentDefinitionKind::Agent,
        personality: None,
        soft_budget: None,
        knowledge_roots: Vec::new(),
        user_proxy: false,
    }
}

fn test_model() -> Model {
    Model {
        id: "durable-test".to_owned(),
        name: "Durable Test".to_owned(),
        api: pi_ai::API_FAUX.to_owned(),
        provider: "faux".to_owned(),
        ..Model::default()
    }
}

async fn attach_test_resources(session: &Session, root: &std::path::Path) {
    let agent_dir = root.join("agent-home");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::write(agent_dir.join("settings.json"), DANGER_SETTINGS).expect("danger settings");
    let mut options = ResourceManagerOptions::new(session.cwd());
    options.agent_dir = agent_dir;
    options.disable_extensions = true;
    options.disable_skills = true;
    options.disable_prompt_templates = true;
    options.disable_themes = true;
    options.disable_context_files = true;
    session
        .attach_resources(ResourceManager::new(options).expect("resources"))
        .await
        .expect("attach resources");
}

async fn recorded_parent(
    artifacts: &std::path::Path,
    session_dir: &std::path::Path,
    session_id: &str,
) -> Session {
    let project = artifacts.join("project");
    std::fs::create_dir_all(&project).expect("project cwd");
    let recorder = session_store::start_session_in(
        artifacts,
        Some(&test_model()),
        None,
        Some(session_dir),
        Some(session_id),
        None,
    )
    .expect("parent recorder");
    let parent = Session::new_with_additional_tools_filtered_and_discovery(
        SessionOptions {
            model: test_model(),
            cwd: project,
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
            auth_manager: None,
        },
        Vec::new(),
        Default::default(),
        ResourceDiscovery::Disabled,
    )
    .expect("parent session");
    parent.set_session_dir(session_dir.to_path_buf());
    parent.record(recorder).expect("record parent");
    attach_test_resources(&parent, artifacts).await;
    parent
}

async fn resumed_parent(parent: &Session, resource_root: &std::path::Path) -> Session {
    let (_, parent_path) = parent.recorder_info().expect("parent recording");
    let resumed = Session::new_with_additional_tools_filtered_and_discovery(
        SessionOptions {
            model: test_model(),
            cwd: parent.cwd().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
            auth_manager: None,
        },
        Vec::new(),
        Default::default(),
        ResourceDiscovery::Disabled,
    )
    .expect("resumed parent session");
    resumed.set_session_dir(parent.session_dir());
    resumed
        .record(pi_coding::resume_session(parent_path).expect("resume parent recording"))
        .expect("record resumed parent");
    attach_test_resources(&resumed, resource_root).await;
    resumed
}

fn unused_runtime(artifacts: &std::path::Path) -> OrchestrationRuntime {
    let mut config =
        OrchestrationConfig::new(AgentCatalog::from_agents(vec![definition()]), artifacts);
    config.parent_model = test_model();
    config.idle_ttl = None;
    OrchestrationRuntime::new(
        config,
        Arc::new(|_request| Box::pin(async { anyhow::bail!("no child session expected") })),
    )
    .expect("runtime")
}

fn child_root(parent_path: &std::path::Path, parent_id: &str) -> std::path::PathBuf {
    parent_path
        .parent()
        .expect("parent directory")
        .join("children")
        .join(parent_id)
}

fn sidecar_path(parent_path: &std::path::Path, parent_id: &str) -> std::path::PathBuf {
    child_root(parent_path, parent_id).join("orchestration-state.json")
}

fn persisted_agent_with_session_path(path: &std::path::Path) -> pi_coding::PersistedAgent {
    serde_json::from_value(json!({
        "snapshot": {
            "id": "Worker",
            "displayName": "Worker",
            "agent": "task",
            "parentId": "Main",
            "status": "parked",
            "createdAt": 1,
            "lastActivity": 1,
            "unread": 0,
            "artifactRef": "agent://Worker",
            "historyRef": "history://Worker"
        },
        "definition": {
            "name": "task",
            "description": "background task",
            "systemPrompt": "complete the assignment",
            "tools": [],
            "autoloadSkills": [],
            "thinkingLevel": "off",
            "source": "bundled",
            "trusted": true
        },
        "request": {
            "childId": "Worker",
            "parentId": "Main",
            "depth": 1,
            "assignment": "do work",
            "systemPrompt": "complete the assignment",
            "requestedToolNames": [],
            "thinkingLevel": "off",
            "maxToolsPerAgent": 32,
            "modelProvider": test_model().provider,
            "modelId": test_model().id
        },
        "sessionPath": path.to_string_lossy(),
        "mailbox": []
    }))
    .expect("persisted agent")
}

fn tool<'a>(tools: &'a [AgentTool], name: &str) -> &'a AgentTool {
    tools
        .iter()
        .find(|tool| tool.name == name)
        .unwrap_or_else(|| panic!("missing {name} tool"))
}

fn context(id: &str, arguments: Value) -> ToolCallContext {
    let (_, abort) = AbortController::new();
    ToolCallContext {
        tool_call_id: id.to_owned(),
        arguments,
        on_update: Arc::new(|_| {}),
        abort,
        model: None,
        approval_token: None,
    }
}

/// Durable recorder persists header before any assistant message.
#[test]
fn durable_recorder_persists_header_before_assistant() {
    let dir = tempfile::tempdir().expect("temp dir");
    let parent_path = dir.path().join("parent.jsonl");
    std::fs::write(&parent_path, b"{}").expect("parent exists");
    let recorder = start_durable_child_session_in(
        dir.path(),
        Some(&test_model()),
        Some("off"),
        dir.path(),
        None,
        &parent_path,
    )
    .expect("start durable child");
    let path = recorder.path();
    // The header must be on disk before any assistant message.
    assert!(
        path.exists(),
        "durable child session file must exist immediately"
    );
    let contents = std::fs::read_to_string(&path).expect("read child session");
    assert!(
        contents.contains("\"type\":\"session\""),
        "header must be persisted: {contents}"
    );
    assert!(
        !contents.contains("\"type\":\"message\""),
        "no messages yet: {contents}"
    );
}

/// Child JSONL parent linkage: parentSession field points to canonical parent path.
#[test]
fn child_jsonl_has_parent_linkage() {
    let dir = tempfile::tempdir().expect("temp dir");
    let parent_path = dir.path().join("parent.jsonl");
    std::fs::write(&parent_path, b"{}").expect("parent exists");
    let recorder = start_durable_child_session_in(
        dir.path(),
        Some(&test_model()),
        None,
        dir.path(),
        None,
        &parent_path,
    )
    .expect("start child");
    let header = recorder.header();
    assert_eq!(
        header.parent_session.as_deref(),
        Some(parent_path.to_string_lossy().as_ref()),
        "parentSession must be canonical parent path"
    );
}

/// Child JSONL root isolation: the session file must be inside the child dir.
#[test]
fn child_jsonl_root_isolation() {
    let dir = tempfile::tempdir().expect("temp dir");
    let parent_path = dir.path().join("parent.jsonl");
    std::fs::write(&parent_path, b"{}").expect("parent exists");
    let child_root = dir.path().join("children").join("parent-id");
    let recorder =
        start_durable_child_session_in(dir.path(), None, None, &child_root, None, &parent_path)
            .expect("start child");
    let path = recorder.path();
    assert!(
        path.starts_with(&child_root),
        "child session path must be inside child root: {} vs {}",
        path.display(),
        child_root.display()
    );
}

/// State round-trip through the production orchestration owner retains the
/// exact durable policy captured from the live application generation.
#[tokio::test]
async fn durable_state_round_trip() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let sessions = tempfile::tempdir().expect("sessions");
    let parent = recorded_parent(artifacts.path(), sessions.path(), "parent-id").await;
    let (_, parent_path) = parent.recorder_info().expect("parent recording");
    let runtime = unused_runtime(artifacts.path());
    let application = Application::new_with_orchestration(parent, runtime.clone()).await;

    let state: DurableState = serde_json::from_slice(
        &std::fs::read(sidecar_path(&parent_path, "parent-id")).expect("read sidecar"),
    )
    .expect("load durable state");
    assert_eq!(state.version, pi_coding::DURABLE_STATE_VERSION);
    assert_eq!(state.parent_session_id, "parent-id");
    assert_eq!(state.parent_session_path, parent_path.to_string_lossy());
    assert_eq!(
        state.durable_policy,
        application
            .durable_policy_state()
            .expect("live durable policy")
    );
    let encoded = serde_json::to_vec(&state).expect("serialize durable state");
    assert_eq!(
        serde_json::from_slice::<DurableState>(&encoded).expect("deserialize durable state"),
        state
    );
    assert!(
        runtime.is_durable(),
        "production owner retains parent authority"
    );
    application.cleanup().await;
}

/// Wrong parent rejection: recovery validates the sidecar against the retained
/// parent recorder authority before constructing application state.
#[tokio::test]
async fn durable_state_wrong_parent_rejected() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let sessions = tempfile::tempdir().expect("sessions");
    let parent = recorded_parent(artifacts.path(), sessions.path(), "parent-id").await;
    let (_, parent_path) = parent.recorder_info().expect("parent recording");
    let application =
        Application::new_with_orchestration(parent.clone(), unused_runtime(artifacts.path())).await;
    application.cleanup().await;

    let sidecar = sidecar_path(&parent_path, "parent-id");
    let mut state: DurableState =
        serde_json::from_slice(&std::fs::read(&sidecar).expect("read sidecar"))
            .expect("durable state");
    let different = sessions.path().join("different.jsonl");
    std::fs::write(&different, b"{}\n").expect("different parent");
    state.parent_session_path = different.to_string_lossy().into_owned();
    std::fs::write(
        &sidecar,
        serde_json::to_vec(&state).expect("serialize state"),
    )
    .expect("write mismatched state");

    let restarted_parent = resumed_parent(&parent, artifacts.path()).await;
    let result = Application::from_runtime_candidate(
        ApplicationRuntimeCandidate::new(restarted_parent)
            .with_orchestration(unused_runtime(artifacts.path())),
    )
    .await;
    let error = match result {
        Ok(_) => panic!("wrong parent path accepted"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("mismatch"), "{error:#}");
}

/// Corrupt sidecar fails without being deleted.
#[tokio::test]
async fn corrupt_sidecar_fails_closed() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let sessions = tempfile::tempdir().expect("sessions");
    let parent = recorded_parent(artifacts.path(), sessions.path(), "parent-id").await;
    let (_, parent_path) = parent.recorder_info().expect("parent recording");
    let application =
        Application::new_with_orchestration(parent.clone(), unused_runtime(artifacts.path())).await;
    application.cleanup().await;
    let sidecar = sidecar_path(&parent_path, "parent-id");
    std::fs::write(&sidecar, b"{corrupt").expect("corrupt");

    let restarted_parent = resumed_parent(&parent, artifacts.path()).await;
    let result = Application::from_runtime_candidate(
        ApplicationRuntimeCandidate::new(restarted_parent)
            .with_orchestration(unused_runtime(artifacts.path())),
    )
    .await;
    let error = match result {
        Ok(_) => panic!("corrupt sidecar accepted"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("parsing"), "{error:#}");
    assert!(sidecar.exists(), "sidecar not deleted");
}

/// Path traversal in session_path is rejected through the production recovery
/// owner, which validates it against the retained child-root capability.
#[tokio::test]
async fn path_traversal_rejected() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let sessions = tempfile::tempdir().expect("sessions");
    let parent = recorded_parent(artifacts.path(), sessions.path(), "parent-id").await;
    let (_, parent_path) = parent.recorder_info().expect("parent recording");
    let application =
        Application::new_with_orchestration(parent.clone(), unused_runtime(artifacts.path())).await;
    application.cleanup().await;

    let outside = tempfile::tempdir().expect("outside");
    let escaped = outside.path().join("escaped.jsonl");
    std::fs::write(&escaped, b"{}").expect("write outside");
    let sidecar = sidecar_path(&parent_path, "parent-id");
    let mut state: DurableState =
        serde_json::from_slice(&std::fs::read(&sidecar).expect("read sidecar"))
            .expect("durable state");
    state
        .agents
        .push(persisted_agent_with_session_path(&escaped));
    std::fs::write(
        &sidecar,
        serde_json::to_vec(&state).expect("serialize state"),
    )
    .expect("write traversal state");

    let restarted_parent = resumed_parent(&parent, artifacts.path()).await;
    let result = Application::from_runtime_candidate(
        ApplicationRuntimeCandidate::new(restarted_parent)
            .with_orchestration(unused_runtime(artifacts.path())),
    )
    .await;
    let error = match result {
        Ok(_) => panic!("outside child path accepted"),
        Err(error) => error,
    };
    assert!(format!("{error:#}").contains("escapes"), "{error:#}");
}

#[cfg(unix)]
#[tokio::test]
async fn symlinked_children_root_is_rejected() {
    use std::os::unix::fs::symlink;

    let artifacts = tempfile::tempdir().expect("artifacts");
    let sessions = tempfile::tempdir().expect("sessions");
    let outside = tempfile::tempdir().expect("outside");
    symlink(outside.path(), sessions.path().join("children")).expect("children symlink");
    let parent = recorded_parent(artifacts.path(), sessions.path(), "parent-id").await;
    let result = Application::from_runtime_candidate(
        ApplicationRuntimeCandidate::new(parent)
            .with_orchestration(unused_runtime(artifacts.path())),
    )
    .await;
    let error = match result {
        Ok(_) => panic!("symlink escape accepted"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("filesystem")
            || error.to_string().contains("symlink")
            || error
                .to_string()
                .contains("opening durable child-session capability"),
        "{error:#}"
    );
}

/// Normal non-bound runtime behavior is unchanged: spawning works, no persistence.
#[tokio::test]
async fn non_bound_runtime_works_without_persistence() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let factory: ChildSessionFactory = Arc::new(|request| {
        Box::pin(async move {
            let stream_fn: pi_agent::StreamFn = Arc::new(move |model, _ctx, _opts| {
                Box::pin(async move {
                    let stream = pi_ai::new_assistant_message_event_stream();
                    let producer = stream.clone();
                    tokio::spawn(async move {
                        let mut message = pi_ai::AssistantMessage::pending(&model);
                        message.content.push(pi_ai::ContentBlock::text("done"));
                        message.stop_reason = StopReason::Stop;
                        producer.end(Some(message)).await;
                    });
                    stream
                })
            });
            Session::new_with_additional_tools_filtered_and_discovery(
                SessionOptions {
                    model: request.model,
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
                    auth_manager: None,
                },
                Vec::new(),
                Default::default(),
                ResourceDiscovery::Disabled,
            )
        })
    });
    let mut config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![definition()]),
        artifacts.path(),
    );
    config.parent_model = test_model();
    config.idle_ttl = None;
    let runtime = OrchestrationRuntime::new(config, factory).expect("runtime");
    assert!(
        !runtime.is_durable(),
        "non-bound runtime must not be durable"
    );
    let tools = runtime.agent_tools("Main", 0);
    let task = tool(&tools, "task");
    let result = (task.execute)(context(
        "spawn",
        json!({
            "context": "Non-durable runtime check: complete the assignment, then stay available until the parent settles the test.",
            "tasks": [{ "name": "Child", "task": "do work" }]
        }),
    ))
    .await
    .expect("spawn");
    let spawns: Vec<TaskSpawn> = serde_json::from_value(result.details).expect("spawns");
    assert_eq!(spawns.len(), 1);
    runtime
        .wait_jobs(
            &[spawns[0].job_id.clone()],
            Some(Duration::from_secs(5)),
            None,
        )
        .await
        .expect("wait");
    runtime.shutdown().await;
}

/// Parked send returns Revived when durable runtime is bound.
#[tokio::test]
async fn parked_send_returns_revived() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let parent_session_dir = tempfile::tempdir().expect("parent sessions");

    let parent = recorded_parent(
        artifacts.path(),
        parent_session_dir.path(),
        "parked-revival-parent",
    )
    .await;

    let factory: ChildSessionFactory = Arc::new(|request| {
        Box::pin(async move {
            let stream_fn: pi_agent::StreamFn = Arc::new(move |model, _ctx, _opts| {
                Box::pin(async move {
                    let stream = pi_ai::new_assistant_message_event_stream();
                    let producer = stream.clone();
                    tokio::spawn(async move {
                        let mut message = pi_ai::AssistantMessage::pending(&model);
                        message
                            .content
                            .push(pi_ai::ContentBlock::text("revived result"));
                        message.stop_reason = StopReason::Stop;
                        producer.end(Some(message)).await;
                    });
                    stream
                })
            });
            Session::new_with_additional_tools_filtered_and_discovery(
                SessionOptions {
                    model: request.model,
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
                    auth_manager: None,
                },
                Vec::new(),
                Default::default(),
                ResourceDiscovery::Disabled,
            )
        })
    });
    let mut config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![definition()]),
        artifacts.path(),
    );
    config.parent_model = test_model();
    config.idle_ttl = None;
    let runtime = OrchestrationRuntime::new(config, factory).expect("runtime");
    let application = Application::new_with_orchestration(parent, runtime.clone()).await;

    // Spawn a child.
    let tools = runtime.agent_tools("Main", 0);
    let task = tool(&tools, "task");
    let spawn_result = (task.execute)(context(
        "spawn-child",
        json!({
            "context": "Durable child session check: complete the assignment; the parent then parks and wakes the child to verify recovery.",
            "tasks": [{ "name": "Worker", "task": "do work" }]
        }),
    ))
    .await
    .expect("spawn");
    let spawns: Vec<TaskSpawn> = serde_json::from_value(spawn_result.details).expect("spawns");
    let job_id = spawns[0].job_id.clone();
    runtime
        .wait_jobs(&[job_id], Some(Duration::from_secs(5)), None)
        .await
        .expect("wait for child");

    // The child should be Idle, then park after TTL (but idle_ttl=None so stays Idle).
    // Manually park it.
    runtime.park("Worker").expect("park");

    // Send a message to the parked agent.
    let receipts = runtime.send("Main", "Worker", "wake up", None);
    assert_eq!(receipts.len(), 1);
    assert_eq!(
        receipts[0].outcome,
        DeliveryOutcome::Revived,
        "parked durable child must return Revived"
    );
    assert!(receipts[0].error.is_none(), "{:?}", receipts[0].error);

    // Wait for the revived job to complete.
    let jobs = runtime.jobs(None);
    let revived_job = jobs
        .iter()
        .find(|j| j.agent_id == "Worker" && j.status == JobStatus::Running)
        .or_else(|| {
            jobs.iter()
                .find(|j| j.agent_id == "Worker" && !j.status.is_settled())
        })
        .expect("revived job exists");
    runtime
        .wait_jobs(
            &[revived_job.id.clone()],
            Some(Duration::from_secs(5)),
            None,
        )
        .await
        .expect("wait for revived job");

    application.cleanup().await;
}

/// An Idle durable child revives immediately on a committed message: the
/// idle→park TTL is not a forced delivery boundary, the receipt is a truthful
/// Revived (not a fake Woken), and the mailbox message reaches the revived
/// run's model context as the typed orchestration custom message.
#[tokio::test]
async fn idle_send_revives_durable_child_with_message_in_context() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let parent_session_dir = tempfile::tempdir().expect("parent sessions");

    let parent = recorded_parent(
        artifacts.path(),
        parent_session_dir.path(),
        "idle-revival-parent",
    )
    .await;

    let contexts = Arc::new(std::sync::Mutex::new(Vec::<pi_ai::Context>::new()));
    let stream_contexts = contexts.clone();
    let factory: ChildSessionFactory = Arc::new(move |request| {
        let stream_contexts = stream_contexts.clone();
        Box::pin(async move {
            let stream_fn: pi_agent::StreamFn = Arc::new(move |model, context, _opts| {
                let stream_contexts = stream_contexts.clone();
                Box::pin(async move {
                    stream_contexts.lock().expect("context lock").push(context);
                    let stream = pi_ai::new_assistant_message_event_stream();
                    let producer = stream.clone();
                    tokio::spawn(async move {
                        let mut message = pi_ai::AssistantMessage::pending(&model);
                        message.content.push(pi_ai::ContentBlock::text("done"));
                        message.stop_reason = StopReason::Stop;
                        producer.end(Some(message)).await;
                    });
                    stream
                })
            });
            Session::new_with_additional_tools_filtered_and_discovery(
                SessionOptions {
                    model: request.model,
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
                    auth_manager: None,
                },
                Vec::new(),
                Default::default(),
                ResourceDiscovery::Disabled,
            )
        })
    });
    let mut config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![definition()]),
        artifacts.path(),
    );
    config.parent_model = test_model();
    config.idle_ttl = None;
    let runtime = OrchestrationRuntime::new(config, factory).expect("runtime");
    let application = Application::new_with_orchestration(parent, runtime.clone()).await;

    // Spawn a child; it settles Idle (idle_ttl is None, so it never parks on
    // its own).
    let tools = runtime.agent_tools("Main", 0);
    let task = tool(&tools, "task");
    let spawn_result = (task.execute)(context(
        "spawn-worker",
        json!({
            "context": "Idle revival exercise: complete the assignment, then remain available.",
            "tasks": [{ "name": "Worker", "task": "do work" }]
        }),
    ))
    .await
    .expect("spawn");
    let spawns: Vec<TaskSpawn> = serde_json::from_value(spawn_result.details).expect("spawns");
    runtime
        .wait_jobs(
            &[spawns[0].job_id.clone()],
            Some(Duration::from_secs(5)),
            None,
        )
        .await
        .expect("wait for child");
    let idle = runtime
        .list("Main")
        .into_iter()
        .find(|peer| peer.id == "Worker")
        .expect("worker listed");
    assert_eq!(idle.status, AgentStatus::Idle, "child settles Idle");

    // The first message to the settled child revives it immediately and
    // truthfully instead of lying with a Woken receipt.
    let receipts = runtime.send("Main", "Worker", "wake up now", None);
    assert_eq!(receipts.len(), 1);
    assert_eq!(
        receipts[0].outcome,
        DeliveryOutcome::Revived,
        "idle durable child must revive on a committed message"
    );
    assert!(receipts[0].error.is_none(), "{:?}", receipts[0].error);

    // The revived job settles, drains the mailbox, and the message reaches
    // the revived run's model context exactly once.
    let jobs = runtime.jobs(None);
    let revived_job = jobs
        .iter()
        .find(|j| j.agent_id == "Worker" && j.status == JobStatus::Running)
        .or_else(|| {
            jobs.iter()
                .find(|j| j.agent_id == "Worker" && !j.status.is_settled())
        })
        .expect("revived job exists");
    runtime
        .wait_jobs(
            &[revived_job.id.clone()],
            Some(Duration::from_secs(5)),
            None,
        )
        .await
        .expect("wait for revived job");
    assert!(
        runtime.inbox("Worker", true).expect("inbox").is_empty(),
        "mailbox must be drained by the revived run"
    );
    let contains_message = |context: &pi_ai::Context| {
        context.messages.iter().any(|message| {
            let text = match message {
                pi_ai::Message::Custom(custom) => custom
                    .content
                    .to_blocks()
                    .iter()
                    .filter_map(|block| match block {
                        pi_ai::ContentBlock::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>(),
                pi_ai::Message::User(user) => user
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        pi_ai::ContentBlock::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>(),
                _ => String::new(),
            };
            text.contains("wake up now")
        })
    };
    let captured = contexts.lock().expect("contexts lock");
    assert_eq!(captured.len(), 2, "initial run + revived run");
    assert!(
        !contains_message(&captured[0]),
        "the initial run must not see the later message"
    );
    assert!(
        captured.iter().skip(1).any(contains_message),
        "the mailbox message must reach the revived run's model context"
    );

    application.cleanup().await;
}

/// Unsettled jobs keep Running on recovery for revival.
#[test]
fn interrupted_job_preserved_on_recovery() {
    let job = pi_coding::JobSnapshot {
        id: "job-1".to_owned(),
        agent_id: "Alpha".to_owned(),
        agent: "task".to_owned(),
        parent_id: "Main".to_owned(),
        description: None,
        todo_task_id: None,
        workflow_id: None,
        workflow_generation: None,
        status: JobStatus::Running,
        created_at: 1,
        started_at: Some(2),
        finished_at: None,
        result: None,
        soft_budget_exhausted: false,
    };
    let recovered = pi_coding::recovery_job(job, 100);
    assert_eq!(
        recovered.status,
        JobStatus::Running,
        "unsettled jobs keep status for revival"
    );
    assert_eq!(
        recovered.finished_at, None,
        "unsettled jobs are not finished"
    );
    assert!(
        recovered.result.is_none(),
        "unsettled jobs get no synthetic error"
    );
}

/// Recovery status queues active agents for revival and keeps settled.
#[test]
fn recovery_status_truthful() {
    use pi_coding::AgentStatus;
    assert_eq!(
        pi_coding::recovery_status(AgentStatus::Running),
        AgentStatus::Queued
    );
    assert_eq!(
        pi_coding::recovery_status(AgentStatus::Queued),
        AgentStatus::Queued
    );
    assert_eq!(
        pi_coding::recovery_status(AgentStatus::Idle),
        AgentStatus::Parked
    );
    assert_eq!(
        pi_coding::recovery_status(AgentStatus::Aborted),
        AgentStatus::Aborted
    );
    assert_eq!(
        pi_coding::recovery_status(AgentStatus::Parked),
        AgentStatus::Parked
    );
}

/// A waiter aborted after the atomic handoff restores the message to the
/// mailbox, and the committed message survives a restart: the handoff removal
/// is only persisted when the wait actually returns the message, so the
/// durable sidecar never records a consumption that did not happen.
#[tokio::test]
async fn aborted_waiter_handoff_restores_and_survives_restart() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let parent_session_dir = tempfile::tempdir().expect("parent sessions");

    let parent = recorded_parent(
        artifacts.path(),
        parent_session_dir.path(),
        "handoff-restart-parent",
    )
    .await;

    let factory: ChildSessionFactory = Arc::new(|request| {
        Box::pin(async {
            let stream_fn: pi_agent::StreamFn = Arc::new(|model, _ctx, _opts| {
                Box::pin(async move {
                    let stream = pi_ai::new_assistant_message_event_stream();
                    let producer = stream.clone();
                    tokio::spawn(async move {
                        let mut message = pi_ai::AssistantMessage::pending(&model);
                        message.content.push(pi_ai::ContentBlock::text("done"));
                        message.stop_reason = StopReason::Stop;
                        producer.end(Some(message)).await;
                    });
                    stream
                })
            });
            Session::new_with_additional_tools_filtered_and_discovery(
                SessionOptions {
                    model: test_model(),
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
                    auth_manager: None,
                },
                Vec::new(),
                Default::default(),
                ResourceDiscovery::Disabled,
            )
        })
    });
    let mut config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![definition()]),
        artifacts.path(),
    );
    config.parent_model = test_model();
    config.idle_ttl = None;
    let runtime = OrchestrationRuntime::new(config.clone(), factory.clone()).expect("runtime");
    let group_id = runtime.group_id().to_owned();
    let application = Application::new_with_orchestration(parent.clone(), runtime.clone()).await;

    // Spawn a child so "Worker" is a registered durable agent, then settle it.
    let tools = runtime.agent_tools("Main", 0);
    let task = tool(&tools, "task");
    let spawn_result = (task.execute)(context(
        "spawn-worker",
        json!({
            "context": "Durable handoff exercise: complete the assignment, then remain available.",
            "tasks": [{ "name": "Worker", "task": "do work" }]
        }),
    ))
    .await
    .expect("spawn");
    let spawns: Vec<TaskSpawn> = serde_json::from_value(spawn_result.details).expect("spawns");
    runtime
        .wait_jobs(
            &[spawns[0].job_id.clone()],
            Some(Duration::from_secs(5)),
            None,
        )
        .await
        .expect("wait for child");

    // Worker waits for Main; the send is atomically handed off, then the wait
    // task is dropped before it can return the message.
    let waiter = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .wait_message("Worker", Some("Main"), Some(Duration::from_secs(30)), None)
                .await
        }
    });
    tokio::task::yield_now().await;
    let receipts = runtime.send("Main", "Worker", "durable handoff", None);
    assert_eq!(
        receipts[0].outcome,
        DeliveryOutcome::Processed,
        "atomic handoff"
    );
    waiter.abort();
    assert!(waiter.await.is_err(), "aborted waiter join");
    assert_eq!(
        runtime.inbox("Worker", true).expect("inbox").len(),
        1,
        "the aborted waiter must restore the handed-off message"
    );

    // Restart from the same parent: the committed message must still be in
    // the recovered mailbox (the removal was never persisted).
    application.cleanup().await;
    let restarted = OrchestrationRuntime::new(
        config,
        Arc::new(|_request| {
            Box::pin(async { anyhow::bail!("no child session expected after restart") })
        }),
    )
    .expect("restarted runtime");
    let restarted_parent = resumed_parent(&parent, artifacts.path()).await;
    let restarted_application =
        Application::new_with_orchestration(restarted_parent, restarted.clone()).await;
    assert_eq!(
        restarted.group_id(),
        group_id,
        "restart keeps durable group identity"
    );
    let inbox = restarted.inbox("Worker", true).expect("inbox");
    assert_eq!(
        inbox.len(),
        1,
        "the handed-off message must survive the restart"
    );
    assert_eq!(inbox[0].body, "durable handoff");
    restarted_application.cleanup().await;
}
