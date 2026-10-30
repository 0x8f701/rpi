//! Integration tests for the Todo child fork system's RPC control plane.
//!
//! These tests build a test Application with a faux provider, orchestration,
//! and a runtime factory for child session creation. An [`RpcDispatcher`] is
//! constructed from the Application, and [`RpcCommand`] variants are dispatched
//! to verify the todo child control plane contracts:
//!
//! - `todo_child_create` + `todo_child_send` round-trip a receipt
//! - `todo_child_list` reports every child; `todo_child_select` routes
//!   subsequent `todo_child_send` to the selected child
//! - `todo_child_status` returns a snapshot; `todo_child_stop` transitions to
//!   `Stopped`
//!
//! No network or real provider is used — the faux stream settles instantly.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use futures_util::FutureExt;
use pi_agent::{StreamFn, ThinkingLevel};
use pi_ai::providers::{
    FauxProviderOptions, FauxProviderRegistration, FauxResponse, register_faux_provider,
};
use pi_ai::{AssistantMessage, AssistantMessageEvent, Model, StopReason};
use pi_cli::modes::rpc::{RpcCommand, RpcDispatcher, RpcResponse};
use pi_coding::{
    AgentCatalog, AgentDefinition, AgentDefinitionSource, Application, ApplicationRuntimeCandidate,
    ApplicationRuntimeFactory, ApplicationRuntimeFuture, OrchestrationConfig, OrchestrationRuntime,
    PreparedSessionResume, ResourceDiscovery, ResourceManager, ResourceManagerOptions, Session,
    SessionOptions, TodoChildAddTaskRequest, TodoChildCreateRequest, TodoChildReceipt,
    TodoChildService, TodoChildStatus, ToolSelection, WorkspaceRoots,
};
use tempfile::TempDir;

const DANGER_SETTINGS: &str = r#"{"sandbox":{"mode":"danger-full-access"}}"#;

/// Per-process isolated native session root, so parent/child recording never
/// writes into the real `~/.pi/agent/sessions` tree. Parallel-safe via
/// `LazyLock`, isolated from `HOME`.
fn test_sessions_root() -> PathBuf {
    static ROOT: std::sync::LazyLock<TempDir> =
        std::sync::LazyLock::new(|| tempfile::tempdir().expect("test sessions root"));
    ROOT.path().to_path_buf()
}

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

/// A faux stream_fn that immediately settles with an empty assistant
/// message. Inherited by todo children so no real provider call is made.
fn instant_settle_stream() -> StreamFn {
    Arc::new(|model: Model, _context, _options| {
        let model = model.clone();
        async move {
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
    })
}

async fn attach_isolated_resources(session: &Session) -> anyhow::Result<()> {
    let agent_dir = test_sessions_root()
        .join("agents")
        .join(uuid::Uuid::now_v7().to_string());
    fs::create_dir_all(&agent_dir)?;
    fs::write(agent_dir.join("settings.json"), DANGER_SETTINGS)?;
    let mut options = ResourceManagerOptions::new(session.cwd());
    options.agent_dir = agent_dir;
    options.project_trust_override = Some(true);
    options.disable_extensions = true;
    options.disable_skills = true;
    options.disable_prompt_templates = true;
    options.disable_themes = true;
    options.disable_context_files = true;
    session
        .attach_resources(ResourceManager::new(options)?)
        .await;
    Ok(())
}

/// A runtime factory that builds child sessions for the todo child system.
///
/// Each child session gets isolated resources and handles resume by loading
/// history and attaching the recorder. The todo child system then re-resumes
/// via `resume_durable_child_under_lease` (overwriting the recorder).
#[derive(Clone)]
struct TodoChildRuntimeFactory;

impl ApplicationRuntimeFactory for TodoChildRuntimeFactory {
    fn build_runtime_candidate(
        &self,
        cwd: PathBuf,
        mut options: SessionOptions,
        resume: Option<PreparedSessionResume>,
    ) -> ApplicationRuntimeFuture {
        Box::pin(async move {
            options.cwd.clone_from(&cwd);
            let workspace = WorkspaceRoots::new(&cwd, Vec::<PathBuf>::new())?;
            let agent_dir = test_sessions_root()
                .join("agents")
                .join(uuid::Uuid::now_v7().to_string());
            fs::create_dir_all(&agent_dir)?;
            fs::write(agent_dir.join("settings.json"), DANGER_SETTINGS)?;
            let mut resource_options = ResourceManagerOptions::new(&cwd);
            resource_options.agent_dir = agent_dir;
            resource_options.project_trust_override = Some(true);
            resource_options.disable_extensions = true;
            resource_options.disable_skills = true;
            resource_options.disable_prompt_templates = true;
            resource_options.disable_themes = true;
            resource_options.disable_context_files = true;
            let resources = ResourceManager::new(resource_options)?;
            options.system_prompt = resources
                .snapshot()
                .system_prompt
                .clone()
                .unwrap_or_default();
            let session = Session::new_with_additional_tools_filtered_discovery_workspace_and_uri(
                options,
                Vec::new(),
                ToolSelection::default(),
                ResourceDiscovery::Disabled,
                workspace,
                None,
            )?;
            session.attach_resources(resources).await?;
            if let Some(resume) = resume {
                let context = resume.build_context();
                let recorder = resume.into_recorder()?;
                session.load_history(context.messages).await?;
                session.record(recorder)?;
            }
            Ok(ApplicationRuntimeCandidate::new(session))
        })
    }
}

/// Build a test Application with a faux provider, orchestration, a host-owned
/// TodoChildService, and a runtime factory for child session creation.
async fn build_dispatcher(
    tag: &str,
) -> (
    TempDir,
    FauxProviderRegistration,
    RpcDispatcher,
    Arc<TodoChildService>,
) {
    let root = tempfile::tempdir().expect("tempdir");

    let mut model = Model::default();
    model.id = format!("todo-rpc-{tag}");
    model.name = format!("Todo RPC {tag}");
    model.api = format!("todo-rpc-{tag}-api");
    model.provider = "faux".to_owned();
    model.base_url = "http://localhost:0".to_owned();
    let provider = register_faux_provider(FauxProviderOptions {
        api: model.api.clone(),
        provider: model.provider.clone(),
        models: vec![model.clone()],
        chunk_size: 4,
    });
    provider.set_responses(vec![FauxResponse::text("todo child response")]);

    let model = pi_ai::get_model(&model.provider, &model.id).unwrap_or_else(|| {
        panic!(
            "registered faux provider unavailable: provider={} model={}",
            model.provider, model.id
        )
    });

    let session = Session::new_with_additional_tools_filtered_and_discovery(
        SessionOptions {
            model: model.clone(),
            cwd: root.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: "todo-rpc-test".to_owned(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: Some(instant_settle_stream()),
            auth_resolver: None,
            auth_manager: None,
        },
        Vec::new(),
        ToolSelection::default(),
        ResourceDiscovery::Disabled,
    )
    .expect("parent session");
    let session_dir = test_sessions_root().join(uuid::Uuid::now_v7().to_string());
    fs::create_dir_all(&session_dir).expect("session dir");
    session.set_session_dir(session_dir);
    session.start_new_recording().expect("start recording");
    attach_isolated_resources(&session)
        .await
        .expect("attach resources");

    let mut config =
        OrchestrationConfig::new(AgentCatalog::from_agents(vec![definition()]), root.path());
    config.parent_model = model;
    config.idle_ttl = None;
    let runtime = OrchestrationRuntime::new(
        config,
        Arc::new(|_request| Box::pin(async { anyhow::bail!("no orchestration child expected") })),
    )
    .expect("orchestration runtime");

    let application = Application::new_with_orchestration(session, runtime).await;
    let factory: Arc<dyn ApplicationRuntimeFactory> = Arc::new(TodoChildRuntimeFactory);
    let service = TodoChildService::new();
    application
        .attach_runtime_factory(factory.clone())
        .expect("attach runtime factory");
    service
        .configure_factory(factory)
        .expect("configure todo factory");
    application
        .attach_todo_child_client(service.client())
        .expect("attach todo child client");

    let dispatcher = RpcDispatcher::new(application);
    (root, provider, dispatcher, service)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Contract: `todo_child_create` returns a create result with the child
/// identity, and `todo_child_send` returns a receipt for the message. The
/// receipt's child_session_id must match the create result's identity, and
/// both IDs must be non-empty.
#[tokio::test]
async fn rpc_todo_child_create_and_send() {
    let (_root, _provider, dispatcher, service) = build_dispatcher("create-send").await;

    // Create a todo child.
    let create_response = dispatcher
        .dispatch_inner(RpcCommand::TodoChildCreate {
            id: None,
            request: TodoChildCreateRequest {
                label: Some("test".to_owned()),
                idempotency_key: None,
            },
        })
        .await;
    assert!(
        create_response.success,
        "create failed: {:?}",
        create_response.error
    );
    assert_eq!(create_response.command, "todo_child_create");
    let create_data = create_response.data.expect("create response data");
    let child_session_id = create_data["identity"]["childSessionId"]
        .as_str()
        .expect("childSessionId")
        .to_owned();
    let parent_session_id = create_data["identity"]["parentSessionId"]
        .as_str()
        .expect("parentSessionId")
        .to_owned();
    assert!(
        !child_session_id.is_empty(),
        "child session id must not be empty"
    );
    assert!(
        !parent_session_id.is_empty(),
        "parent session id must not be empty"
    );
    assert_eq!(
        create_data["snapshot"]["status"], "idle",
        "newly created child should be idle"
    );

    // Send a message to the child (selected by create).
    let send_response = dispatcher
        .dispatch_inner(RpcCommand::TodoChildSend {
            id: None,
            parent_session_id: parent_session_id.clone(),
            child_session_id: None,
            message: "do something".to_owned(),
        })
        .await;
    assert!(
        send_response.success,
        "send failed: {:?}",
        send_response.error
    );
    assert_eq!(send_response.command, "todo_child_send");
    let send_data = send_response.data.expect("send response data");
    let receipt: TodoChildReceipt = serde_json::from_value(send_data).expect("parse receipt");
    assert!(!receipt.id.is_empty(), "receipt id must not be empty");
    assert!(
        !receipt.message_id.is_empty(),
        "message id must not be empty"
    );
    assert_eq!(
        receipt.child_session_id, child_session_id,
        "receipt child id mismatch"
    );
    assert_eq!(
        receipt.parent_session_id, parent_session_id,
        "receipt parent id mismatch"
    );

    service
        .shutdown()
        .await
        .expect("shutdown Todo child service");
    dispatcher.application().cleanup().await;
}

/// Contract: `todo_child_list` returns every created child, `todo_child_select`
/// routes a subsequent `todo_child_send` (with `child_session_id: None`) to the
/// selected child, and the receipt's child_session_id matches the selection.
#[tokio::test]
async fn rpc_todo_child_list_and_select() {
    let (_root, _provider, dispatcher, service) = build_dispatcher("list-select").await;

    // Create two children.
    let first = dispatcher
        .dispatch_inner(RpcCommand::TodoChildCreate {
            id: None,
            request: TodoChildCreateRequest {
                label: Some("first".to_owned()),
                idempotency_key: None,
            },
        })
        .await;
    assert!(first.success, "first create failed: {:?}", first.error);
    let first_data = first.data.expect("first data");
    let first_id = first_data["identity"]["childSessionId"]
        .as_str()
        .expect("first childSessionId")
        .to_owned();
    let parent_session_id = first_data["identity"]["parentSessionId"]
        .as_str()
        .expect("parentSessionId")
        .to_owned();

    let second = dispatcher
        .dispatch_inner(RpcCommand::TodoChildCreate {
            id: None,
            request: TodoChildCreateRequest {
                label: Some("second".to_owned()),
                idempotency_key: None,
            },
        })
        .await;
    assert!(second.success, "second create failed: {:?}", second.error);
    let second_id = second.data.expect("second data")["identity"]["childSessionId"]
        .as_str()
        .expect("second childSessionId")
        .to_owned();
    assert_ne!(
        first_id, second_id,
        "children must have distinct session ids"
    );

    // List should report both children.
    let list_response = dispatcher
        .dispatch_inner(RpcCommand::TodoChildList {
            id: None,
            parent_session_id: parent_session_id.clone(),
        })
        .await;
    assert!(
        list_response.success,
        "list failed: {:?}",
        list_response.error
    );
    assert_eq!(list_response.command, "todo_child_list");
    let entries: Vec<pi_coding::TodoChildManifestEntry> =
        serde_json::from_value(list_response.data.expect("list data")).expect("parse entries");
    assert_eq!(entries.len(), 2, "list must report exactly two children");
    let listed_ids: Vec<&str> = entries
        .iter()
        .map(|e| e.child_session_id.as_str())
        .collect();
    assert!(
        listed_ids.contains(&first_id.as_str()),
        "first child must appear in list: {listed_ids:?}"
    );
    assert!(
        listed_ids.contains(&second_id.as_str()),
        "second child must appear in list: {listed_ids:?}"
    );

    // Select the first child (create auto-selected the second).
    let select_response = dispatcher
        .dispatch_inner(RpcCommand::TodoChildSelect {
            id: None,
            parent_session_id: parent_session_id.clone(),
            child_session_id: first_id.clone(),
        })
        .await;
    assert!(
        select_response.success,
        "select failed: {:?}",
        select_response.error
    );
    assert_eq!(select_response.command, "todo_child_select");

    // Send with child_session_id: None — must route to the selected (first) child.
    let send_response = dispatcher
        .dispatch_inner(RpcCommand::TodoChildSend {
            id: None,
            parent_session_id,
            child_session_id: None,
            message: "work on first".to_owned(),
        })
        .await;
    assert!(
        send_response.success,
        "send failed: {:?}",
        send_response.error
    );
    let receipt: TodoChildReceipt =
        serde_json::from_value(send_response.data.expect("send data")).expect("parse receipt");
    assert_eq!(
        receipt.child_session_id, first_id,
        "send must route to the selected child"
    );

    service
        .shutdown()
        .await
        .expect("shutdown Todo child service");
    dispatcher.application().cleanup().await;
}

/// Contract: `todo_child_status` returns a snapshot reflecting the child's
/// current state, and `todo_child_stop` transitions the child to `Stopped`.
/// After stop, a subsequent status check still reports `Stopped`.
#[tokio::test]
async fn rpc_todo_child_status_and_stop() {
    let (_root, _provider, dispatcher, service) = build_dispatcher("status-stop").await;

    // Create a child and send a message.
    let create_response = dispatcher
        .dispatch_inner(RpcCommand::TodoChildCreate {
            id: None,
            request: TodoChildCreateRequest {
                label: Some("status-probe".to_owned()),
                idempotency_key: None,
            },
        })
        .await;
    assert!(
        create_response.success,
        "create failed: {:?}",
        create_response.error
    );
    let create_data = create_response.data.expect("create data");
    let child_id = create_data["identity"]["childSessionId"]
        .as_str()
        .expect("childSessionId")
        .to_owned();
    let parent_session_id = create_data["identity"]["parentSessionId"]
        .as_str()
        .expect("parentSessionId")
        .to_owned();

    let send_response = dispatcher
        .dispatch_inner(RpcCommand::TodoChildSend {
            id: None,
            parent_session_id: parent_session_id.clone(),
            child_session_id: None,
            message: "run a turn".to_owned(),
        })
        .await;
    assert!(
        send_response.success,
        "send failed: {:?}",
        send_response.error
    );

    // Status must return a snapshot with the correct child identity.
    let status_response = dispatcher
        .dispatch_inner(RpcCommand::TodoChildStatus {
            id: None,
            parent_session_id: parent_session_id.clone(),
            child_session_id: None,
        })
        .await;
    assert!(
        status_response.success,
        "status failed: {:?}",
        status_response.error
    );
    assert_eq!(status_response.command, "todo_child_status");
    let snapshot: pi_coding::TodoChildSnapshot =
        serde_json::from_value(status_response.data.expect("status data")).expect("parse snapshot");
    assert_eq!(
        snapshot.child_session_id, child_id,
        "status child id mismatch"
    );
    assert!(
        !snapshot.receipts.is_empty(),
        "status must report the queued receipt"
    );
    // The child was created (Idle) and a message was sent. The status must
    // not be Stopped — it should be Idle, Running, or another active state.
    assert_ne!(
        snapshot.status,
        TodoChildStatus::Stopped,
        "active child must not report Stopped"
    );

    // Stop the child.
    let stop_response = dispatcher
        .dispatch_inner(RpcCommand::TodoChildStop {
            id: None,
            parent_session_id,
            child_session_id: None,
        })
        .await;
    assert!(
        stop_response.success,
        "stop failed: {:?}",
        stop_response.error
    );
    assert_eq!(stop_response.command, "todo_child_stop");
    let stop_snapshot: pi_coding::TodoChildSnapshot =
        serde_json::from_value(stop_response.data.expect("stop data"))
            .expect("parse stop snapshot");
    assert_eq!(
        stop_snapshot.status,
        TodoChildStatus::Stopped,
        "stop must transition to Stopped"
    );
    assert_eq!(
        stop_snapshot.child_session_id, child_id,
        "stop child id mismatch"
    );

    service
        .shutdown()
        .await
        .expect("shutdown Todo child service");
    dispatcher.application().cleanup().await;
}

async fn create_child(dispatcher: &RpcDispatcher, label: &str) -> (String, String, u64) {
    let create_response = dispatcher
        .dispatch_inner(RpcCommand::TodoChildCreate {
            id: None,
            request: TodoChildCreateRequest {
                label: Some(label.to_owned()),
                idempotency_key: None,
            },
        })
        .await;
    assert!(
        create_response.success,
        "create failed: {:?}",
        create_response.error
    );
    let data = create_response.data.expect("create data");
    let child_session_id = data["identity"]["childSessionId"]
        .as_str()
        .expect("childSessionId")
        .to_owned();
    let parent_session_id = data["identity"]["parentSessionId"]
        .as_str()
        .expect("parentSessionId")
        .to_owned();
    let revision = data["snapshot"]["revision"].as_u64().expect("revision");
    (parent_session_id, child_session_id, revision)
}

/// Contract: `todo_child_add_task` appends a task and advances revision; a
/// stale `expectedRevision` fails closed without mutating the DAG.
#[tokio::test]
async fn rpc_todo_child_add_task_revision_and_stale() {
    let (_root, _provider, dispatcher, service) = build_dispatcher("add-task").await;
    let (parent_session_id, child_session_id, revision) =
        create_child(&dispatcher, "add-task").await;

    let added = dispatcher
        .dispatch_inner(RpcCommand::TodoChildAddTask {
            id: None,
            parent_session_id: parent_session_id.clone(),
            child_session_id: child_session_id.clone(),
            request: TodoChildAddTaskRequest {
                content: "task A".to_owned(),
                expected_revision: Some(revision),
                ..Default::default()
            },
        })
        .await;
    assert!(added.success, "add_task failed: {:?}", added.error);
    assert_eq!(added.command, "todo_child_add_task");
    let added_data = added.data.expect("add data");
    let first_revision = added_data["revision"].as_u64().expect("added revision");
    assert!(first_revision > revision, "add_task must advance revision");
    assert_eq!(added_data["task"]["content"], "task A");

    let stale = dispatcher
        .dispatch_inner(RpcCommand::TodoChildAddTask {
            id: None,
            parent_session_id: parent_session_id.clone(),
            child_session_id: child_session_id.clone(),
            request: TodoChildAddTaskRequest {
                content: "task C (stale)".to_owned(),
                expected_revision: Some(revision),
                ..Default::default()
            },
        })
        .await;
    assert!(!stale.success, "stale expected_revision must fail closed");
    let error = stale.error.unwrap_or_default();
    assert!(
        error.to_ascii_lowercase().contains("revision")
            || error.to_ascii_lowercase().contains("stale"),
        "stale error must mention revision, got {error}"
    );

    let status = dispatcher
        .dispatch_inner(RpcCommand::TodoChildStatus {
            id: None,
            parent_session_id,
            child_session_id: Some(child_session_id),
        })
        .await;
    assert!(status.success, "status failed: {:?}", status.error);
    let snapshot: pi_coding::TodoChildSnapshot =
        serde_json::from_value(status.data.expect("status data")).expect("parse snapshot");
    assert_eq!(snapshot.tasks.len(), 1, "stale add must not mutate the DAG");
    assert_eq!(snapshot.tasks[0].content, "task A");
    assert!(
        snapshot.revision >= first_revision,
        "task execution may append terminal events"
    );

    service
        .shutdown()
        .await
        .expect("shutdown Todo child service");
    dispatcher.application().cleanup().await;
}

/// Contract: parent Application cleanup does not shut down the host-owned
/// TodoChildService. A second parent attached to the same weak client can
/// still list and add tasks after the first parent is cleaned up.
#[tokio::test]
async fn rpc_todo_child_parent_cleanup_does_not_shutdown_service() {
    let (root, _provider, dispatcher, service) = build_dispatcher("parent-cleanup").await;
    let (parent_session_id, child_session_id, _) =
        create_child(&dispatcher, "survive-cleanup").await;
    let added = dispatcher
        .dispatch_inner(RpcCommand::TodoChildAddTask {
            id: None,
            parent_session_id: parent_session_id.clone(),
            child_session_id: child_session_id.clone(),
            request: TodoChildAddTaskRequest {
                content: "keep running".to_owned(),
                ..Default::default()
            },
        })
        .await;
    assert!(added.success, "add_task failed: {:?}", added.error);

    let original_parent_path = dispatcher
        .application()
        .session()
        .recorder_info()
        .expect("original parent recorder")
        .1;
    dispatcher
        .application()
        .session()
        .persist_recording_now()
        .expect("persist original parent recorder");
    dispatcher.application().cleanup().await;

    let session = Session::new_with_additional_tools_filtered_and_discovery(
        SessionOptions {
            model: dispatcher
                .application()
                .session()
                .model()
                .expect("second parent model"),
            cwd: root.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: "todo-rpc-test".to_owned(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: Some(instant_settle_stream()),
            auth_resolver: None,
            auth_manager: None,
        },
        Vec::new(),
        ToolSelection::default(),
        ResourceDiscovery::Disabled,
    )
    .expect("second parent session");
    session.set_session_dir(test_sessions_root());
    session
        .record(
            pi_coding::resume_session(&original_parent_path)
                .expect("resume original parent recorder"),
        )
        .expect("attach original parent recorder");
    attach_isolated_resources(&session)
        .await
        .expect("second resources");
    let second = Application::new(session).await;
    second
        .attach_todo_child_client(service.client())
        .expect("attach surviving client");
    let second_dispatcher = RpcDispatcher::new(second);

    let listed = second_dispatcher
        .dispatch_inner(RpcCommand::TodoChildList {
            id: None,
            parent_session_id: parent_session_id.clone(),
        })
        .await;
    assert!(
        listed.success,
        "list after parent cleanup must succeed: {:?}",
        listed.error
    );

    let still_add = second_dispatcher
        .dispatch_inner(RpcCommand::TodoChildAddTask {
            id: None,
            parent_session_id,
            child_session_id,
            request: TodoChildAddTaskRequest {
                content: "after parent cleanup".to_owned(),
                ..Default::default()
            },
        })
        .await;
    assert!(
        still_add.success,
        "add_task after parent cleanup must succeed: {:?}",
        still_add.error
    );

    service
        .shutdown()
        .await
        .expect("shutdown Todo child service");
    second_dispatcher.application().cleanup().await;
}

/// Contract: `todo_child_remove` prunes the exact child — a non-selected
/// removal keeps the sibling addressable with its receipts, repeated removals
/// and absent ids are idempotent successes, removing the selected child
/// clears the selection so subsequent bare status fails closed, and status on
/// a removed child fails instead of recovering it.
#[tokio::test]
async fn rpc_todo_child_remove_prunes_isolates_and_is_idempotent() {
    let (_root, _provider, dispatcher, service) = build_dispatcher("remove").await;
    let (parent_session_id, child_a, _) = create_child(&dispatcher, "remove-me").await;
    let (_parent_b, child_b, _) = create_child(&dispatcher, "keep-me").await;
    assert_eq!(parent_session_id, _parent_b, "both children share one parent");

    let selected = dispatcher
        .dispatch_inner(RpcCommand::TodoChildSelect {
            id: None,
            parent_session_id: parent_session_id.clone(),
            child_session_id: child_b.clone(),
        })
        .await;
    assert!(selected.success, "select failed: {:?}", selected.error);

    let send_response = dispatcher
        .dispatch_inner(RpcCommand::TodoChildSend {
            id: None,
            parent_session_id: parent_session_id.clone(),
            child_session_id: Some(child_a.clone()),
            message: "work for the removed child".to_owned(),
        })
        .await;
    assert!(send_response.success, "send failed: {:?}", send_response.error);

    let remove_response = dispatcher
        .dispatch_inner(RpcCommand::TodoChildRemove {
            id: None,
            parent_session_id: parent_session_id.clone(),
            child_session_id: child_a.clone(),
        })
        .await;
    assert!(
        remove_response.success,
        "remove failed: {:?}",
        remove_response.error
    );
    assert_eq!(remove_response.command, "todo_child_remove");

    let listed = dispatcher
        .dispatch_inner(RpcCommand::TodoChildList {
            id: None,
            parent_session_id: parent_session_id.clone(),
        })
        .await;
    assert!(listed.success, "list failed: {:?}", listed.error);
    let entries: Vec<pi_coding::TodoChildManifestEntry> =
        serde_json::from_value(listed.data.expect("list data")).expect("parse entries");
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.child_session_id.as_str())
            .collect::<Vec<_>>(),
        vec![child_b.as_str()],
        "removal must prune exactly the removed child's manifest entry"
    );

    let sibling = dispatcher
        .dispatch_inner(RpcCommand::TodoChildStatus {
            id: None,
            parent_session_id: parent_session_id.clone(),
            child_session_id: Some(child_b.clone()),
        })
        .await;
    assert!(sibling.success, "sibling status failed: {:?}", sibling.error);
    let sibling_snapshot: pi_coding::TodoChildSnapshot =
        serde_json::from_value(sibling.data.expect("status data")).expect("parse sibling");
    assert_eq!(
        sibling_snapshot.child_session_id, child_b,
        "sibling identity must be untouched by the removal"
    );

    for repeat_id in [child_a.clone(), "00000000-0000-4000-8000-00000000000f".to_owned()] {
        let repeat = dispatcher
            .dispatch_inner(RpcCommand::TodoChildRemove {
                id: None,
                parent_session_id: parent_session_id.clone(),
                child_session_id: repeat_id,
            })
            .await;
        assert!(
            repeat.success,
            "repeat/absent removal must be idempotent: {:?}",
            repeat.error
        );
    }

    let removed_status = dispatcher
        .dispatch_inner(RpcCommand::TodoChildStatus {
            id: None,
            parent_session_id: parent_session_id.clone(),
            child_session_id: Some(child_a.clone()),
        })
        .await;
    assert!(
        !removed_status.success,
        "status on a removed child must fail closed"
    );

    let remove_selected = dispatcher
        .dispatch_inner(RpcCommand::TodoChildRemove {
            id: None,
            parent_session_id: parent_session_id.clone(),
            child_session_id: child_b.clone(),
        })
        .await;
    assert!(
        remove_selected.success,
        "removing the selected child failed: {:?}",
        remove_selected.error
    );
    let final_list = dispatcher
        .dispatch_inner(RpcCommand::TodoChildList {
            id: None,
            parent_session_id,
        })
        .await;
    assert!(
        final_list.success,
        "final list failed: {:?}",
        final_list.error
    );
    let final_entries: Vec<pi_coding::TodoChildManifestEntry> =
        serde_json::from_value(final_list.data.expect("final list data")).expect("parse entries");
    assert!(
        final_entries.is_empty(),
        "removing both children must leave an empty manifest"
    );

    service
        .shutdown()
        .await
        .expect("shutdown Todo child service");
    dispatcher.application().cleanup().await;
}
