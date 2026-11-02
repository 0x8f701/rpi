//! Real production-path acceptance for natural-language Todo mutations: a
//! real `Application` turn executes the model's typed tool call through the
//! production tool registry with no client NL parser and no slash
//! translation, asserting the exact persisted result and failed-mutation
//! invariance.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use pi_agent::{AgentEvent, StreamFn, ThinkingLevel};
use pi_ai::providers::{FauxProviderOptions, FauxProviderRegistration, register_faux_provider};
use pi_ai::{
    AssistantMessage, ContentBlock, Context, Message, Model, SimpleStreamOptions, StopReason,
    ToolCall, new_assistant_message_event_stream,
};
use pi_coding::{
    AgentCatalog, AgentDefinition, AgentDefinitionKind, AgentDefinitionSource, Application,
    ApplicationEvent, ApplicationRuntimeCandidate, ApplicationRuntimeFactory,
    ApplicationRuntimeFuture, OrchestrationConfig, OrchestrationRuntime, PreparedSessionResume,
    ResourceDiscovery, ResourceManager, ResourceManagerOptions, Session, SessionOptions,
    SessionRecorder, TodoChildCreateRequest, TodoChildService, TodoChildStatus, TodoState,
    TodoStatus, ToolSelection, session_store,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const DANGER_SETTINGS: &str =
    r#"{"sandbox":{"mode":"danger-full-access"},"orchestration":{"process":true,"tasks":true,"todo":true}}"#;
const TODO_ERROR_MARKER: &str = "__piTodoError";

const NL_ADD_TASK: &str = "add a task to the todo list: send the welcome email";
const NL_REMOVE_TASK: &str = "remove the send the welcome email task from the todo list";
const NL_CLEAR_TODO: &str = "clear the whole todo list";
const NL_FAILED_REMOVE: &str = "delete the todo task that was never created";
const NL_CANCEL_RUN: &str = "cancel the todo child run";
const NL_DELETE_TODO: &str = "delete the todo child completely";

fn test_sessions_root() -> PathBuf {
    static ROOT: std::sync::LazyLock<tempfile::TempDir> =
        std::sync::LazyLock::new(|| tempfile::tempdir().expect("test sessions root"));
    ROOT.path().to_path_buf()
}

fn faux_model(tag: &str) -> (Model, FauxProviderRegistration) {
    let suffix = uuid::Uuid::now_v7().to_string();
    let model = Model {
        id: format!("nl-todo-{tag}-{suffix}"),
        name: "NL Todo Proof".into(),
        api: format!("nl-todo-api-{tag}-{suffix}"),
        provider: format!("nl-todo-provider-{tag}-{suffix}"),
        ..Model::default()
    };
    let registration = register_faux_provider(FauxProviderOptions {
        api: model.api.clone(),
        provider: model.provider.clone(),
        models: vec![model.clone()],
        chunk_size: 32,
    });
    let registered = pi_ai::get_model(&model.provider, &model.id)
        .unwrap_or_else(|| panic!("registered faux provider unavailable: {tag}"));
    (registered, registration)
}

struct CapturedRequest {
    model: Model,
    context: Context,
}

fn tool_call_message(name: &str, arguments: Value) -> AssistantMessage {
    let mut message = AssistantMessage::pending(&Model::default());
    message.content = vec![ContentBlock::ToolCall(ToolCall {
        id: format!("call-{name}"),
        name: name.to_owned(),
        arguments,
        thought_signature: None,
    })];
    message.stop_reason = StopReason::ToolUse;
    message
}

fn text_message(text: &str) -> AssistantMessage {
    let mut message = AssistantMessage::pending(&Model::default());
    message.content = vec![ContentBlock::text(text)];
    message.stop_reason = StopReason::Stop;
    message
}

fn scripted_stream(
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
    queue: Arc<Mutex<VecDeque<AssistantMessage>>>,
) -> StreamFn {
    Arc::new(move |model: Model, context: Context, _options: SimpleStreamOptions| {
        requests.lock().push(CapturedRequest {
            model: model.clone(),
            context: context.clone(),
        });
        let message = queue
            .lock()
            .pop_front()
            .unwrap_or_else(|| text_message("no scripted response for an unexpected model call"));
        let stream = new_assistant_message_event_stream();
        let producer = stream.clone();
        tokio::spawn(async move {
            producer.end(Some(message)).await;
        });
        Box::pin(async move { stream })
    })
}

async fn attach_resources(session: &Session, root: &Path) {
    let agent_dir = test_sessions_root()
        .join("agents")
        .join(uuid::Uuid::now_v7().to_string());
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::write(agent_dir.join("settings.json"), DANGER_SETTINGS).expect("settings");
    let mut options = ResourceManagerOptions::new(root);
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

struct NlTodoFixture {
    application: Application,
    recorder: SessionRecorder,
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
    queue: Arc<Mutex<VecDeque<AssistantMessage>>>,
    tool_ends: Arc<Mutex<Vec<(String, Value, bool)>>>,
    _artifacts: tempfile::TempDir,
    _registration: FauxProviderRegistration,
}

async fn nl_todo_fixture(tag: &str, session_id: &str, with_child_tools: bool) -> NlTodoFixture {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (model, registration) = faux_model(tag);
    let session_dir = test_sessions_root();
    let recorder = session_store::start_session_in(
        artifacts.path(),
        Some(&model),
        None,
        Some(&session_dir),
        Some(session_id),
        None,
    )
    .expect("parent recorder");
    let requests: Arc<Mutex<Vec<CapturedRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let queue: Arc<Mutex<VecDeque<AssistantMessage>>> = Arc::new(Mutex::new(VecDeque::new()));
    let session = Session::new_with_todo_and_additional_tools_filtered_and_discovery(
        SessionOptions {
            model: model.clone(),
            cwd: artifacts.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: "faux".into(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: Some(scripted_stream(requests.clone(), queue.clone())),
            auth_resolver: None,
            auth_manager: None,
        },
        Vec::new(),
        ToolSelection::default(),
        ResourceDiscovery::Disabled,
    )
    .expect("parent session");
    session.set_session_dir(session_dir);
    session.record(recorder.clone()).expect("record parent");
    attach_resources(&session, artifacts.path()).await;
    let mut candidate = ApplicationRuntimeCandidate::new(session);
    if with_child_tools {
        candidate = candidate.with_todo_child_tool(pi_coding::TodoChildToolBinding::default());
    }
    let application = Application::from_runtime_candidate(candidate)
        .await
        .expect("publish application");
    let tool_ends: Arc<Mutex<Vec<(String, Value, bool)>>> = Arc::new(Mutex::new(Vec::new()));
    let collector = tool_ends.clone();
    let mut subscription = application.subscribe();
    tokio::spawn(async move {
        loop {
            match subscription.recv().await {
                Ok(
                    ApplicationEvent::Agent(AgentEvent::ToolExecutionEnd {
                        tool_name,
                        result,
                        is_error,
                        ..
                    })
                    | ApplicationEvent::AgentPublished {
                        event:
                            AgentEvent::ToolExecutionEnd {
                                tool_name,
                                result,
                                is_error,
                                ..
                            },
                        ..
                    },
                ) => {
                    let value = serde_json::to_value(&result).expect("tool result json");
                    collector.lock().push((tool_name, value, is_error));
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            }
        }
    });
    NlTodoFixture {
        application,
        recorder,
        requests,
        queue,
        tool_ends,
        _artifacts: artifacts,
        _registration: registration,
    }
}

fn push_turn(fixture: &NlTodoFixture, message: AssistantMessage) {
    fixture.queue.lock().push_back(message);
}

async fn run_prompt(fixture: &NlTodoFixture, message: &str) {
    fixture
        .application
        .prompt(message.to_owned(), Vec::new(), None)
        .await
        .expect("prompt accepted")
        .expect("natural-language management prompt must run a parent turn");
    fixture.application.wait_for_idle().await;
}

fn tool_ends(fixture: &NlTodoFixture, name: &str) -> Vec<(Value, bool)> {
    fixture
        .tool_ends
        .lock()
        .iter()
        .filter(|(tool_name, _, _)| tool_name == name)
        .map(|(_, value, is_error)| (value.clone(), *is_error))
        .collect()
}

fn todo_snapshots(recorder: &SessionRecorder) -> Vec<TodoState> {
    session_store::load_session_tree(recorder.path())
        .expect("load session tree")
        .branch(None)
        .iter()
        .filter(|entry| entry.entry_type == "todo_snapshot")
        .filter_map(|entry| entry.todo_state.clone())
        .collect()
}


fn phase_of<'a>(state: &'a TodoState, name: &str) -> &'a pi_coding::TodoPhase {
    state
        .phases
        .iter()
        .find(|phase| phase.name == name)
        .unwrap_or_else(|| panic!("phase {name} missing: {state:?}"))
}



fn persisted_user_texts(recorder: &SessionRecorder) -> Vec<String> {
    session_store::load_session_tree(recorder.path())
        .expect("load session tree")
        .branch(None)
        .iter()
        .filter(|entry| entry.entry_type == "message")
        .filter_map(|entry| entry.message.as_ref())
        .filter_map(|message| match message {
            Message::User(message) => {
                let text = message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                (!text.is_empty()).then_some(text)
            }
            _ => None,
        })
        .collect()
}

fn captured_user_texts(fixture: &NlTodoFixture) -> Vec<String> {
    fixture
        .requests
        .lock()
        .iter()
        .flat_map(|request| request.context.messages.iter())
        .filter_map(|message| match message {
            Message::User(message) => {
                let text = message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                (!text.is_empty()).then_some(text)
            }
            _ => None,
        })
        .collect()
}

fn assert_no_client_translation(fixture: &NlTodoFixture, prompts: &[&str]) {
    let captured = captured_user_texts(fixture);
    let persisted = persisted_user_texts(&fixture.recorder);
    for prompt in prompts {
        assert!(
            !prompt.starts_with('/'),
            "acceptance prompts must be plain natural language, never slash commands"
        );
        assert!(
            captured.iter().any(|text| text == prompt),
            "model boundary must receive the verbatim natural-language prompt: {prompt}"
        );
        assert!(
            persisted.iter().any(|text| text == prompt),
            "persisted user message must be the verbatim natural-language prompt: {prompt}; persisted={persisted:?}"
        );
    }
    let tool_names: Vec<String> = fixture
        .requests
        .lock()
        .iter()
        .flat_map(|request| request.context.tools.iter().map(|tool| tool.name.clone()))
        .collect();
    assert!(
        tool_names.iter().any(|name| name == "todo"),
        "production tool registry must expose the typed todo tool at the model boundary"
    );
}

#[tokio::test]
async fn nl_add_then_remove_task_persists_exact_result() {
    let fixture = nl_todo_fixture("dag", "nl-add-remove", false).await;
    push_turn(
        &fixture,
        tool_call_message(
            "todo",
            json!({"op": "init", "list": [{"phase": "Build", "items": ["compile the parser"]}]}),
        ),
    );
    push_turn(&fixture, text_message("task list ready"));
    run_prompt(&fixture, "set up the task list for the release").await;

    let seeded = fixture.application.todo_state();
    let build = phase_of(&seeded, "Build");
    assert_eq!(build.tasks.len(), 1);
    assert_eq!(build.tasks[0].content, "compile the parser");
    assert_eq!(build.tasks[0].status, TodoStatus::InProgress);
    assert!(build.tasks[0].ready);
    assert_eq!(todo_snapshots(&fixture.recorder).len(), 1);

    push_turn(
        &fixture,
        tool_call_message(
            "todo",
            json!({"op": "append", "phase": "Delivery", "items": ["send the welcome email"]}),
        ),
    );
    push_turn(&fixture, text_message("task added"));
    run_prompt(&fixture, NL_ADD_TASK).await;

    let state = fixture.application.todo_state();
    let delivery = phase_of(&state, "Delivery");
    assert_eq!(delivery.tasks.len(), 1);
    assert_eq!(delivery.tasks[0].content, "send the welcome email");
    assert_eq!(delivery.tasks[0].status, TodoStatus::Pending);
    assert_eq!(phase_of(&state, "Build").tasks, build.tasks);

    let ends = tool_ends(&fixture, "todo");
    assert_eq!(ends.len(), 2, "exactly two typed todo tool calls must execute");
    assert!(!ends[1].1, "the add-task mutation must not be an error");
    assert_eq!(ends[1].0["details"]["op"], "append");
    assert_eq!(
        ends[1].0["details"]["phases"][1]["tasks"][0]["content"],
        "send the welcome email"
    );

    let snapshots = todo_snapshots(&fixture.recorder);
    assert_eq!(snapshots.len(), 2, "each successful mutation persists exactly one snapshot");
    assert_eq!(snapshots[0], seeded);
    assert_eq!(snapshots[1], state, "durable snapshot must equal the live state");

    push_turn(
        &fixture,
        tool_call_message(
            "todo",
            json!({"op": "rm", "task": "send the welcome email", "cascade": false}),
        ),
    );
    push_turn(&fixture, text_message("task removed"));
    run_prompt(&fixture, NL_REMOVE_TASK).await;

    let state = fixture.application.todo_state();
    assert_eq!(phase_of(&state, "Build").tasks, build.tasks);
    assert!(phase_of(&state, "Delivery").tasks.is_empty());
    assert_eq!(tool_ends(&fixture, "todo").len(), 3);
    let snapshots = todo_snapshots(&fixture.recorder);
    assert_eq!(snapshots.len(), 3);
    assert_eq!(snapshots[2], state);

    assert_no_client_translation(
        &fixture,
        &["set up the task list for the release", NL_ADD_TASK, NL_REMOVE_TASK],
    );
    fixture.application.cleanup().await;
}

#[tokio::test]
async fn nl_clear_todo_and_failed_mutation_leave_exact_persisted_state() {
    let fixture = nl_todo_fixture("clear", "nl-clear-failed", false).await;
    push_turn(
        &fixture,
        tool_call_message(
            "todo",
            json!({"op": "init", "list": [
                {"phase": "Build", "items": ["compile the parser"]},
                {"phase": "Wrap", "items": ["tag the release"]}
            ]}),
        ),
    );
    push_turn(&fixture, text_message("task list ready"));
    run_prompt(&fixture, "set up the release task list").await;
    let seeded = fixture.application.todo_state();

    push_turn(&fixture, tool_call_message("todo", json!({"op": "rm"})));
    push_turn(&fixture, text_message("list cleared"));
    run_prompt(&fixture, NL_CLEAR_TODO).await;

    let state = fixture.application.todo_state();
    assert_eq!(phase_of(&state, "Build").tasks, Vec::new());
    assert_eq!(phase_of(&state, "Wrap").tasks, Vec::new());
    let snapshots_after_clear = todo_snapshots(&fixture.recorder);
    assert_eq!(snapshots_after_clear.len(), 2);
    assert_eq!(snapshots_after_clear[1], state);

    let before_failed = fixture.application.todo_state();
    let journal_before = std::fs::read(fixture.recorder.path()).expect("read pre-turn journal");

    push_turn(
        &fixture,
        tool_call_message(
            "todo",
            json!({"op": "rm", "task": "task that was never created", "cascade": false}),
        ),
    );
    push_turn(&fixture, text_message("nothing to delete"));
    run_prompt(&fixture, NL_FAILED_REMOVE).await;

    let ends = tool_ends(&fixture, "todo");
    assert_eq!(
        ends.len(),
        3,
        "init, clear, and the failed rm are each a typed todo call: {ends:?}"
    );
    assert!(ends[2].1, "the failed rm must surface as an error tool result");
    assert_eq!(ends[2].0["details"]["op"], "rm");
    assert!(
        ends[2].0["details"].get(TODO_ERROR_MARKER).is_none()
            || ends[2].0["details"][TODO_ERROR_MARKER].is_null(),
        "published todo errors keep is_error and drop the internal marker: {}",
        ends[2].0
    );
    assert_eq!(
        ends[2].0["details"]["phases"],
        serde_json::to_value(before_failed.phases.clone()).expect("serialize phases"),
        "the error result must project the unchanged state"
    );

    assert_eq!(
        fixture.application.todo_state(),
        before_failed,
        "a failed mutation must leave the live state unchanged"
    );
    let journal_after = std::fs::read(fixture.recorder.path()).expect("read post-turn journal");
    assert!(
        journal_after.starts_with(&journal_before),
        "the failed turn must not rewrite any existing journal bytes"
    );
    let appended = &journal_after[journal_before.len()..];
    assert!(
        !appended
            .windows("todo_snapshot".len())
            .any(|window| window == b"todo_snapshot"),
        "the failed turn must not append any durable snapshot record; appended journal bytes: {}",
        String::from_utf8_lossy(appended)
    );

    assert_no_client_translation(
        &fixture,
        &["set up the release task list", NL_CLEAR_TODO, NL_FAILED_REMOVE],
    );
    fixture.application.cleanup().await;
}

#[tokio::test]
async fn nl_cancel_run_cancels_child_via_typed_tool_and_is_idempotent() {
    let fixture = nl_todo_fixture("cancel", "nl-cancel-run", true).await;
    let service = TodoChildService::new();
    let gates: Arc<Mutex<VecDeque<CancellationToken>>> = Arc::new(Mutex::new(VecDeque::new()));
    service
        .configure_factory(Arc::new(GatedChildFactory::new(gates.clone())))
        .expect("configure child factory");
    fixture
        .application
        .attach_todo_child_client(service.client())
        .expect("attach todo child client");
    fixture
        .application
        .reload()
        .await
        .expect("stage todo child tools into the model-facing registry");

    let child = fixture
        .application
        .todo_child_create(TodoChildCreateRequest {
            label: Some("nl-cancel".into()),
            idempotency_key: Some("nl-cancel-run".into()),
        })
        .await
        .expect("create child")
        .identity
        .child_session_id;

    let gate = CancellationToken::new();
    gates.lock().push_back(gate.clone());
    fixture
        .application
        .todo_child_send(None, Some(&child), "run long work".into())
        .await
        .expect("send long work");
    wait_child_status(&fixture.application, &child, TodoChildStatus::Running, 10).await;

    fixture
        .application
        .reload()
        .await
        .expect("stage todo child tools into the model-facing registry");

    push_turn(&fixture, tool_call_message("todo_child_cancel", json!({})));
    push_turn(&fixture, text_message("run cancelled"));
    run_prompt(&fixture, NL_CANCEL_RUN).await;

    let ends = tool_ends(&fixture, "todo_child_cancel");
    assert_eq!(ends.len(), 1, "exactly one typed cancel call must execute");
    assert!(
        !ends[0].1,
        "cancel of an active run must succeed: {}",
        ends[0].0
    );
    assert_eq!(ends[0].0["details"]["childSessionId"], Value::String(child.clone()));
    assert_eq!(ends[0].0["details"]["status"], Value::String("stopped".to_owned()));
    assert_eq!(ends[0].0["details"]["isRunning"], Value::Bool(false));
    wait_child_idle(&fixture.application, &child, 10).await;
    let settled = fixture
        .application
        .todo_child_status(None, Some(&child))
        .await
        .expect("settled status");
    assert_eq!(settled.status, TodoChildStatus::Stopped);
    assert!(!settled.is_running);

    push_turn(&fixture, tool_call_message("todo_child_cancel", json!({})));
    push_turn(&fixture, text_message("already cancelled"));
    run_prompt(&fixture, "cancel the run again").await;
    let ends = tool_ends(&fixture, "todo_child_cancel");
    assert_eq!(ends.len(), 2);
    assert!(!ends[1].1, "second cancel must be an idempotent success");
    assert_eq!(ends[1].0["details"]["status"], Value::String("stopped".to_owned()));

    assert_no_client_translation(&fixture, &[NL_CANCEL_RUN, "cancel the run again"]);
    service.shutdown().await.expect("shutdown service");
    fixture.application.cleanup().await;
}

#[tokio::test]
async fn nl_delete_todo_removes_child_and_durable_artifacts() {
    let fixture = nl_todo_fixture("delete", "nl-delete-todo", true).await;
    let service = TodoChildService::new();
    let gates: Arc<Mutex<VecDeque<CancellationToken>>> = Arc::new(Mutex::new(VecDeque::new()));
    service
        .configure_factory(Arc::new(GatedChildFactory::new(gates)))
        .expect("configure child factory");
    fixture
        .application
        .attach_todo_child_client(service.client())
        .expect("attach todo child client");
    fixture
        .application
        .reload()
        .await
        .expect("stage todo child tools into the model-facing registry");

    let child = uuid::Uuid::now_v7().to_string();
    let child_root = fixture
        .recorder
        .path()
        .parent()
        .expect("parent session directory")
        .join("children")
        .join(fixture.recorder.id());
    let manifest_file = child_root.join("todo-manifest.json");
    let child_file = child_root.join(format!("{child}.jsonl"));
    let sidecar_file = child_root
        .join("children")
        .join(&child)
        .join("orchestration-state.json");
    fixture
        .application
        .todo_child_send(None, Some(&child), "do durable work".into())
        .await
        .expect("production send-as-create must ensure the child");
    wait_child_idle(&fixture.application, &child, 10).await;
    assert!(manifest_file.is_file(), "the exact durable manifest path must exist before delete");
    assert!(child_file.is_file(), "the exact durable transcript path must exist before delete");
    assert!(
        sidecar_file.is_file(),
        "the exact durable orchestration sidecar path must exist before delete"
    );
    let before_delete = fixture
        .application
        .todo_child_status(None, Some(&child))
        .await
        .expect("status before delete");
    assert!(
        !before_delete.receipts.is_empty(),
        "the sent work must leave a durable receipt before delete"
    );
    let child_journal_bytes_before = std::fs::read(&child_file).expect("read child journal bytes");
    let receipt_marker = format!("\"{}\"", before_delete.receipts[0].id);
    assert!(
        String::from_utf8_lossy(&child_journal_bytes_before).contains(&receipt_marker),
        "the raw child journal must carry the receipt id before delete"
    );
    let child_tree = session_store::load_session_tree(&child_file).expect("load child journal tree");
    let mut durable_receipts: Vec<(String, String, String)> = Vec::new();
    for entry in child_tree.branch(None) {
        if entry.custom_type.as_deref() != Some("pi.todo_child.receipt") {
            continue;
        }
        let Some(payload) = entry.data.as_ref() else {
            continue;
        };
        let Some(receipt) = payload.get("receipt") else {
            continue;
        };
        let Some(id) = receipt["id"].as_str() else {
            continue;
        };
        let Some(receipt_child) = receipt["childSessionId"].as_str() else {
            continue;
        };
        let Some(status) = receipt["status"].as_str() else {
            continue;
        };
        let row = (id.to_owned(), receipt_child.to_owned(), status.to_owned());
        if let Some(existing) = durable_receipts.iter_mut().find(|(seen, _, _)| seen == id) {
            *existing = row;
        } else {
            durable_receipts.push(row);
        }
    }
    let mut durable_ids: Vec<&str> = durable_receipts.iter().map(|(id, _, _)| id.as_str()).collect();
    durable_ids.sort_unstable();
    durable_ids.dedup();
    let mut status_ids: Vec<&str> = before_delete.receipts.iter().map(|receipt| receipt.id.as_str()).collect();
    status_ids.sort_unstable();
    assert_eq!(durable_ids, status_ids, "parsed durable receipt records must match the status receipts exactly");
    for (id, receipt_child, status) in &durable_receipts {
        assert!(
            before_delete.receipts.iter().any(|receipt| {
                &receipt.id == id
                    && &receipt.child_session_id == receipt_child
                    && json!(status) == serde_json::to_value(receipt.status).expect("status json")
            }),
            "durable receipt {id} must carry the exact status {status}"
        );
    }
    let manifest_before: Value =
        serde_json::from_str(&std::fs::read_to_string(&manifest_file).expect("read manifest"))
            .expect("parse manifest before delete");
    assert!(
        manifest_before["entries"]
            .as_array()
            .is_some_and(|entries| {
                entries
                    .iter()
                    .any(|entry| entry["childSessionId"].as_str() == Some(child.as_str()))
            }),
        "the manifest must durably carry exactly the created child before delete"
    );
    assert_eq!(
        fixture
            .application
            .todo_child_list(None)
            .await
            .expect("list before delete")
            .len(),
        1
    );

    push_turn(&fixture, tool_call_message("todo_child_list", json!({})));
    push_turn(
        &fixture,
        tool_call_message("todo_child_remove", json!({"childSessionId": child.clone()})),
    );
    push_turn(&fixture, text_message("todo deleted"));
    run_prompt(&fixture, NL_DELETE_TODO).await;

    let ends = tool_ends(&fixture, "todo_child_remove");
    assert_eq!(ends.len(), 1, "exactly one typed remove call must execute");
    assert!(!ends[0].1, "delete must succeed: {}", ends[0].0);
    assert!(
        !child_file.exists(),
        "delete must prune the durable child transcript"
    );
    assert!(
        !sidecar_file.exists(),
        "delete must prune the exact durable orchestration sidecar path"
    );
    assert!(
        manifest_file.is_file(),
        "the exact durable manifest path must survive remove (only the child entry is pruned)"
    );
    let manifest: Value =
        serde_json::from_str(&std::fs::read_to_string(&manifest_file).expect("read manifest"))
            .expect("parse manifest after delete");
    let entry_for_child = manifest["entries"]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .any(|entry| entry["childSessionId"].as_str() == Some(child.as_str()))
        })
        .unwrap_or(false);
    assert!(
        !entry_for_child,
        "delete must prune the child's manifest entry"
    );
    assert!(
        fixture
            .application
            .todo_child_list(None)
            .await
            .expect("list after delete")
            .is_empty(),
        "delete must drop the manifest entry"
    );
    assert!(
        fixture
            .application
            .todo_child_status(None, Some(&child))
            .await
            .is_err(),
        "a removed child must no longer be addressable"
    );

    push_turn(
        &fixture,
        tool_call_message("todo_child_remove", json!({"childSessionId": child.clone()})),
    );
    push_turn(&fixture, text_message("already gone"));
    run_prompt(&fixture, "delete the todo again").await;
    let ends = tool_ends(&fixture, "todo_child_remove");
    assert_eq!(ends.len(), 2);
    assert!(!ends[1].1, "removing an absent child must be an idempotent no-op");

    assert_no_client_translation(&fixture, &[NL_DELETE_TODO, "delete the todo again"]);
    service.shutdown().await.expect("shutdown service");
    fixture.application.cleanup().await;
}

struct GatedChildFactory {
    gates: Arc<Mutex<VecDeque<CancellationToken>>>,
    owners: Arc<Mutex<Vec<tempfile::TempDir>>>,
}

impl GatedChildFactory {
    fn new(gates: Arc<Mutex<VecDeque<CancellationToken>>>) -> Self {
        Self {
            gates,
            owners: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn child_stream(&self) -> StreamFn {
        let gates = self.gates.clone();
        Arc::new(move |model: Model, _context: Context, options: SimpleStreamOptions| {
            let gate = gates.lock().pop_front();
            let stream = new_assistant_message_event_stream();
            let producer = stream.clone();
            tokio::spawn(async move {
                if let Some(gate) = gate {
                    let abort = options.stream.abort_signal;
                    tokio::select! {
                        () = gate.cancelled() => {}
                        () = async {
                            match abort {
                                Some(token) => token.cancelled().await,
                                None => std::future::pending::<()>().await,
                            }
                        } => {}
                    }
                    let mut message = AssistantMessage::pending(&model);
                    message.stop_reason = StopReason::Aborted;
                    message.error_message = Some("child turn cancelled".to_owned());
                    producer.end(Some(message)).await;
                } else {
                    let mut message = AssistantMessage::pending(&model);
                    message.content = vec![ContentBlock::text("child done")];
                    message.stop_reason = StopReason::Stop;
                    producer.end(Some(message)).await;
                }
            });
            Box::pin(async move { stream })
        })
    }
}

fn child_runtime_agent() -> AgentDefinition {
    AgentDefinition {
        name: "task".to_owned(),
        description: "execute child-owned Todo work".to_owned(),
        system_prompt: "complete the assigned child-owned Todo task".to_owned(),
        tools: Some(vec!["process".to_owned()]),
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
        kind: AgentDefinitionKind::Agent,
        personality: None,
        soft_budget: None,
        knowledge_roots: Vec::new(),
        user_proxy: false,
    }
}

fn child_orchestration_runtime(
    session: &Session,
    artifact_dir: PathBuf,
) -> anyhow::Result<OrchestrationRuntime> {
    let factory = OrchestrationRuntime::child_factory_from_session(session);
    let mut config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![child_runtime_agent()]),
        artifact_dir,
    );
    config.idle_ttl = None;
    config.parent_model = session.model().expect("child runtime model");
    OrchestrationRuntime::new(config, factory)
}

impl ApplicationRuntimeFactory for GatedChildFactory {
    fn build_runtime_candidate(
        &self,
        cwd: PathBuf,
        mut options: SessionOptions,
        resume: Option<PreparedSessionResume>,
    ) -> ApplicationRuntimeFuture {
        let owners = self.owners.clone();
        let child_stream = self.child_stream();
        Box::pin(async move {
            options.cwd.clone_from(&cwd);
            options.stream_fn = Some(child_stream);
            let agent_dir = tempfile::tempdir()?;
            std::fs::write(agent_dir.path().join("settings.json"), DANGER_SETTINGS)?;
            let session_dir = tempfile::tempdir()?;
            let mut resource_options = ResourceManagerOptions::new(&cwd);
            resource_options.agent_dir = agent_dir.path().to_path_buf();
            resource_options.project_trust_override = Some(true);
            resource_options.disable_extensions = true;
            resource_options.disable_skills = true;
            resource_options.disable_prompt_templates = true;
            resource_options.disable_themes = true;
            resource_options.disable_context_files = true;
            let resources = ResourceManager::new(resource_options)?;
            let session = Session::new_with_todo_and_additional_tools_filtered_and_discovery(
                options,
                Vec::new(),
                ToolSelection {
                    enable_process: true,
                    ..ToolSelection::default()
                },
                ResourceDiscovery::Disabled,
            )?;
            session.set_session_dir(session_dir.path().to_path_buf());
            session.attach_resources(resources).await?;
            if let Some(resume) = resume {
                let context = resume.build_context();
                let recorder = resume.into_recorder()?;
                session.load_history(context.messages).await?;
                session.record(recorder)?;
            }
            let orchestration = child_orchestration_runtime(
                &session,
                cwd.join(".rpi").join("todo-child-runtime-test-artifacts"),
            )?;
            owners.lock().push(agent_dir);
            owners.lock().push(session_dir);
            Ok(ApplicationRuntimeCandidate::new(session).with_orchestration(orchestration))
        })
    }
}

async fn wait_child_status(
    application: &Application,
    child_id: &str,
    expected: TodoChildStatus,
    timeout_secs: u64,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        let snapshot = application
            .todo_child_status(None, Some(child_id))
            .await
            .expect("child status");
        if snapshot.status == expected {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "child never reached {expected:?}"
        );
        tokio::task::yield_now().await;
    }
}

async fn wait_child_idle(application: &Application, child_id: &str, timeout_secs: u64) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        let snapshot = application
            .todo_child_status(None, Some(child_id))
            .await
            .expect("child status");
        if !snapshot.is_running {
            return;
        }
        assert!(tokio::time::Instant::now() < deadline, "child never went idle");
        tokio::task::yield_now().await;
    }
}