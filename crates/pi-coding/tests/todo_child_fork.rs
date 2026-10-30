//! Integration tests for the Todo child fork system.
//!
//! Contracts defended:
//! - Two Todo children from the same parent have independent transcripts and receipts
//! - Labels are parent-scoped: same label from different parents does not cross-contaminate
//! - A child survives a parent Application restart and remains addressable via manifest
//! - Stopping one child does not affect a sibling
//! - Fork-time transcript visibility: child A sees pre-t2 messages only, child B sees both
//! - Parent continuation after forking does not pollute the child's transcript
//! - Rewinding the parent does not change a child's fork-base ancestry
//! - Concurrent forks from a parent with ongoing recording capture distinct revisions
//! - Sending to a non-existent child session auto-creates it and returns a receipt

use std::collections::{HashSet, VecDeque};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use pi_agent::{StreamFn, ThinkingLevel};
use pi_ai::providers::{
    FauxProviderOptions, FauxProviderRegistration, FauxResponse, register_faux_provider,
};
use pi_ai::{
    AssistantMessage, ContentBlock, Context, Message, Model, SimpleStreamOptions, StopReason,
    ToolCall, new_assistant_message_event_stream,
};
use pi_coding::{
    AgentCatalog, AgentDefinition, AgentDefinitionKind, AgentDefinitionSource, Application,
    ApplicationRuntimeCandidate, ApplicationRuntimeFactory, ApplicationRuntimeFuture,
    OrchestrationConfig, OrchestrationRuntime, PreparedSessionResume, RequestAuth, RequestAuthKind,
    ResourceDiscovery, ResourceManager, ResourceManagerOptions, RetryFallbackChains, RetrySettings,
    Session, SessionOptions, SessionRecorder, TodoChildAddTaskRequest, TodoChildCreateRequest,
    TodoChildReceiptStatus, TodoChildService, TodoChildStatus, TodoChildTask, TodoChildTaskStatus,
    ToolSelection, session_store,
};
use tokio_util::sync::CancellationToken;

const DANGER_SETTINGS: &str =
    r#"{"sandbox":{"mode":"danger-full-access"},"orchestration":{"process":true}}"#;
const AUTO_ROUTE_SETTINGS: &str = r#"{
    "sandbox":{"mode":"danger-full-access"},
    "orchestration":{"process":true,"tasks":true,"todo":true},
    "selector":{"autoMode":"auto"}
}"#;

/// Per-process isolated native session root so recorder writes never touch
/// the real `~/.pi/agent/sessions` tree.
fn test_sessions_root() -> PathBuf {
    static ROOT: std::sync::LazyLock<tempfile::TempDir> =
        std::sync::LazyLock::new(|| tempfile::tempdir().expect("test sessions root"));
    ROOT.path().to_path_buf()
}

/// Registers a unique faux provider and returns the registered model plus
/// registration. Resolution failure after registration hard-fails.
fn faux_model() -> (Model, FauxProviderRegistration) {
    let suffix = uuid::Uuid::now_v7().to_string();
    let model = Model {
        id: format!("todo-fork-{suffix}"),
        name: "Todo Fork Test".into(),
        api: format!("todo-fork-api-{suffix}"),
        provider: format!("todo-fork-provider-{suffix}"),
        ..Model::default()
    };
    let registration = register_faux_provider(FauxProviderOptions {
        api: model.api.clone(),
        provider: model.provider.clone(),
        models: vec![model.clone()],
        chunk_size: 32,
    });
    (
        require_registered_model(&model.provider, &model.id),
        registration,
    )
}

fn require_registered_model(provider: &str, id: &str) -> Model {
    pi_ai::get_model(provider, id).unwrap_or_else(|| {
        panic!("registered faux provider unavailable: provider={provider} model={id}")
    })
}

/// Creates a recorded parent session with resources attached.
async fn recorded_parent(
    artifacts: &Path,
    session_id: &str,
    model: &Model,
) -> (Session, SessionRecorder) {
    let session_dir = test_sessions_root();
    let recorder = session_store::start_session_in(
        artifacts,
        Some(model),
        None,
        Some(&session_dir),
        Some(session_id),
        None,
    )
    .expect("parent recorder");
    let session = Session::new_with_additional_tools_filtered_and_discovery(
        SessionOptions {
            model: model.clone(),
            cwd: artifacts.to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: "faux".into(),
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
    session.set_session_dir(session_dir);
    session.record(recorder.clone()).expect("record parent");
    attach_resources(&session, artifacts).await;
    (session, recorder)
}

async fn attach_resources(session: &Session, root: &Path) {
    let agent_dir = test_sessions_root()
        .join("agents")
        .join(uuid::Uuid::now_v7().to_string());
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::write(agent_dir.join("settings.json"), DANGER_SETTINGS).expect("danger settings");
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

async fn attach_auto_route_resources(session: &Session, root: &Path) {
    let agent_dir = test_sessions_root()
        .join("agents")
        .join(uuid::Uuid::now_v7().to_string());
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::write(agent_dir.join("settings.json"), AUTO_ROUTE_SETTINGS)
        .expect("auto route settings");
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
        .expect("attach auto route resources");
}

/// Holds tempdirs alive for the lifetime of child sessions created by the factory.
struct FactoryOwners {
    _agent_dirs: Vec<tempfile::TempDir>,
    _session_dirs: Vec<tempfile::TempDir>,
}

/// Runtime factory for Todo children: builds a complete session with
/// danger-full-access resources, loads history from the prepared resume,
/// and records the recorder. Tempdirs are retained for the child's lifetime.
#[derive(Clone)]
struct TodoChildTestFactory {
    owners: Arc<parking_lot::Mutex<FactoryOwners>>,
    inherit_context_files: bool,
    runtime_stream: Option<StreamFn>,
}

impl TodoChildTestFactory {
    fn new() -> Self {
        Self::configured(false, None)
    }

    fn with_context_files(inherit_context_files: bool) -> Self {
        Self::configured(inherit_context_files, None)
    }

    fn with_runtime_stream(stream_fn: StreamFn) -> Self {
        Self::configured(false, Some(stream_fn))
    }

    fn configured(inherit_context_files: bool, runtime_stream: Option<StreamFn>) -> Self {
        Self {
            owners: Arc::new(parking_lot::Mutex::new(FactoryOwners {
                _agent_dirs: Vec::new(),
                _session_dirs: Vec::new(),
            })),
            inherit_context_files,
            runtime_stream,
        }
    }
}

impl ApplicationRuntimeFactory for TodoChildTestFactory {
    fn build_runtime_candidate(
        &self,
        cwd: PathBuf,
        mut options: SessionOptions,
        resume: Option<PreparedSessionResume>,
    ) -> ApplicationRuntimeFuture {
        let owners = self.owners.clone();
        let inherit_context_files = self.inherit_context_files;
        let runtime_stream = self.runtime_stream.clone();
        Box::pin(async move {
            options.cwd.clone_from(&cwd);
            if let Some(stream_fn) = runtime_stream.clone() {
                options.stream_fn = Some(stream_fn);
            }
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
            resource_options.disable_context_files = !inherit_context_files;
            let resources = ResourceManager::new(resource_options)?;
            let session = if runtime_stream.is_some() {
                Session::new_with_todo_and_additional_tools_filtered_and_discovery(
                    options,
                    Vec::new(),
                    ToolSelection {
                        enable_process: true,
                        ..ToolSelection::default()
                    },
                    ResourceDiscovery::Disabled,
                )?
            } else {
                Session::new_with_additional_tools_filtered_and_discovery(
                    options,
                    Vec::new(),
                    ToolSelection::default(),
                    ResourceDiscovery::Disabled,
                )?
            };
            session.set_session_dir(session_dir.path().to_path_buf());
            session.attach_resources(resources).await?;
            if let Some(resume) = resume {
                let context = resume.build_context();
                let recorder = resume.into_recorder()?;
                session.load_history(context.messages).await?;
                session.record(recorder)?;
            }
            let orchestration = runtime_stream
                .is_some()
                .then(|| {
                    child_orchestration_runtime(
                        &session,
                        cwd.join(".rpi").join("todo-child-runtime-test-artifacts"),
                    )
                })
                .transpose()?;
            owners.lock()._agent_dirs.push(agent_dir);
            owners.lock()._session_dirs.push(session_dir);
            let candidate = ApplicationRuntimeCandidate::new(session);
            Ok(match orchestration {
                Some(runtime) => candidate.with_orchestration(runtime),
                None => candidate,
            })
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

fn tool_call_response(id: &str, name: &str, arguments: serde_json::Value) -> FauxResponse {
    FauxResponse {
        content: vec![ContentBlock::ToolCall(ToolCall {
            id: id.to_owned(),
            name: name.to_owned(),
            arguments,
            thought_signature: None,
        })],
        stop_reason: StopReason::ToolUse,
        error_message: None,
    }
}

fn runtime_isolation_stream(
    main_responses: Arc<parking_lot::Mutex<VecDeque<FauxResponse>>>,
    subagent_responses: Arc<parking_lot::Mutex<VecDeque<FauxResponse>>>,
    subagent_release: CancellationToken,
    main_calls: Arc<parking_lot::Mutex<Vec<CapturedChildRequest>>>,
    subagent_calls: Arc<parking_lot::Mutex<Vec<CapturedChildRequest>>>,
) -> StreamFn {
    Arc::new(move |model, context, options| {
        let is_subagent = context
            .system_prompt
            .contains("complete the assigned child-owned Todo task");
        let responses = if is_subagent {
            subagent_responses.clone()
        } else {
            main_responses.clone()
        };
        let calls = if is_subagent {
            subagent_calls.clone()
        } else {
            main_calls.clone()
        };
        let release = subagent_release.clone();
        Box::pin(async move {
            if is_subagent {
                release.cancelled().await;
            }
            calls.lock().push(CapturedChildRequest {
                model: model.clone(),
                context,
                options,
            });
            let response = responses.lock().pop_front().unwrap_or_else(|| {
                FauxResponse::text("runtime isolation response queue exhausted")
            });
            let stream = new_assistant_message_event_stream();
            let producer = stream.clone();
            tokio::spawn(async move {
                let mut message = AssistantMessage::pending(&model);
                message.content = response.content;
                message.stop_reason = response.stop_reason;
                message.error_message = response.error_message;
                producer.end(Some(message)).await;
            });
            stream
        })
    })
}

fn receipt_side_effect_stream(
    marker_path: PathBuf,
    entered: Arc<tokio::sync::Semaphore>,
    release: CancellationToken,
    invocation_count: Arc<AtomicUsize>,
) -> StreamFn {
    Arc::new(move |model, _context, _options| {
        let marker_path = marker_path.clone();
        let entered = entered.clone();
        let release = release.clone();
        let invocation = invocation_count.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let response = if invocation == 0 {
                entered.add_permits(1);
                release.cancelled().await;
                tool_call_response(
                    "receipt-side-effect",
                    "process",
                    serde_json::json!({
                        "op": "start",
                        "argv": [
                            "/bin/sh",
                            "-c",
                            "printf 'executed\\n' >> \"$1\"",
                            "receipt-side-effect",
                            marker_path.to_string_lossy()
                        ],
                        "pty": false,
                        "label": "todo-receipt-exactly-once"
                    }),
                )
            } else {
                FauxResponse::text("receipt side effect completed")
            };
            let stream = new_assistant_message_event_stream();
            let producer = stream.clone();
            tokio::spawn(async move {
                let mut message = AssistantMessage::pending(&model);
                message.content = response.content;
                message.stop_reason = response.stop_reason;
                message.error_message = response.error_message;
                producer.end(Some(message)).await;
            });
            stream
        })
    })
}

fn mutate_running_receipt_accepted_at(path: &Path, receipt_id: &str) -> i64 {
    let bytes = std::fs::read(path).expect("read Todo child journal");
    let mut lines = bytes
        .split_inclusive(|byte| *byte == b'\n')
        .map(Vec::from)
        .collect::<Vec<_>>();
    let mut matched = None;
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.strip_suffix(b"\n").unwrap_or(line);
        let entry = serde_json::from_slice::<serde_json::Value>(trimmed)
            .expect("decode Todo child journal entry");
        if entry.get("customType").and_then(serde_json::Value::as_str)
            != Some("pi.todo_child.receipt")
        {
            continue;
        }
        let receipt = &entry["data"]["receipt"];
        if receipt["id"].as_str() == Some(receipt_id)
            && receipt["status"].as_str() == Some("running")
        {
            assert!(
                matched.replace(index).is_none(),
                "receipt has duplicate Running records"
            );
        }
    }
    let index = matched.expect("durable Running receipt");
    let line = &mut lines[index];
    let accepted_at =
        serde_json::from_slice::<serde_json::Value>(line.strip_suffix(b"\n").unwrap_or(line))
            .expect("decode Running receipt")["data"]["receipt"]["acceptedAt"]
            .as_i64()
            .expect("Running receipt acceptedAt");
    let original = accepted_at.to_string();
    let replacement_digit = if original.ends_with('9') { '8' } else { '9' };
    let mutated = format!("{}{replacement_digit}", &original[..original.len() - 1]);
    let field = format!("\"acceptedAt\":{original}");
    let field_offset = line
        .windows(field.len())
        .position(|window| window == field.as_bytes())
        .expect("acceptedAt field bytes in Running receipt");
    let offset = field_offset + "\"acceptedAt\":".len();
    line[offset..offset + original.len()].copy_from_slice(mutated.as_bytes());
    let rewritten = lines.concat();
    assert_eq!(
        rewritten.len(),
        bytes.len(),
        "journal mutation must preserve byte length"
    );
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open Todo child journal in place");
    file.set_len(0)
        .expect("truncate Todo child journal in place");
    file.seek(SeekFrom::Start(0))
        .expect("seek Todo child journal");
    file.write_all(&rewritten)
        .expect("rewrite Todo child journal in place");
    file.sync_all().expect("sync Todo child journal mutation");
    mutated.parse().expect("mutated acceptedAt")
}

fn marker_execution_count(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|text| text.lines().count())
        .unwrap_or_else(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                0
            } else {
                panic!("read side-effect marker: {error}")
            }
        })
}

/// Builds an Application from a recorded parent session and attaches the factory.
async fn application_with_factory(session: Session) -> (Application, Arc<TodoChildService>) {
    let application = Application::new(session).await;
    application.with_hermetic_repository_executor(
        pi_coding::process::HermeticRepositoryExecutor::default_executor(),
    );
    let factory: Arc<dyn ApplicationRuntimeFactory> = Arc::new(TodoChildTestFactory::new());
    let service = TodoChildService::new();
    service
        .configure_factory(factory.clone())
        .expect("configure factory");
    application
        .attach_runtime_factory(factory)
        .expect("attach factory");
    application
        .attach_todo_child_client(service.client())
        .expect("attach todo child client");
    (application, service)
}

/// Sets a large pool of faux responses so child turns have deterministic output.
fn set_child_responses(registration: &FauxProviderRegistration, count: usize) {
    let responses: Vec<FauxResponse> = (0..count)
        .map(|i| FauxResponse::text(format!("child response {i}")))
        .collect();
    registration.set_responses(responses);
}

// ---------------------------------------------------------------------------
// Service-backed fixtures: shared TodoChildService across parent Applications
// and a controlled stream for deterministic task execution gating.
// ---------------------------------------------------------------------------

/// A controlled stream fixture that gates child model turns behind explicit
/// cancellation tokens. Tests arm a gate before triggering a task, wait for
/// the task to reach Running, then release the gate to let the turn complete.
/// Gates are consumed FIFO, so arming before each `add_task` matches turns to
/// gates in execution order. When no gate is armed the turn completes
/// immediately — useful for tasks that should run to completion unimpeded.
struct ControlledStream {
    gates: Arc<parking_lot::Mutex<VecDeque<CancellationToken>>>,
}

impl ControlledStream {
    fn new() -> Self {
        Self {
            gates: Arc::new(parking_lot::Mutex::new(VecDeque::new())),
        }
    }

    /// Arms a new gate; the next child model turn will block until this token
    /// is cancelled.
    fn arm(&self) -> CancellationToken {
        let token = CancellationToken::new();
        self.gates.lock().push_back(token.clone());
        token
    }

    /// Returns a `StreamFn` that pops gates FIFO. When no gate is armed the
    /// turn completes immediately with a text response.
    fn stream_fn(&self) -> StreamFn {
        let gates = self.gates.clone();
        Arc::new(move |model, _ctx, opts| {
            let gates = gates.clone();
            let abort = opts.stream.abort_signal;
            Box::pin(async move {
                let stream = new_assistant_message_event_stream();
                let producer = stream.clone();
                tokio::spawn(async move {
                    let gate = gates.lock().pop_front();
                    if let Some(gate) = gate {
                        tokio::select! {
                            () = gate.cancelled() => {}
                            () = async {
                                match abort {
                                    Some(token) => token.cancelled().await,
                                    None => std::future::pending::<()>().await,
                                }
                            } => {
                                let mut message = AssistantMessage::pending(&model);
                                message.stop_reason = StopReason::Aborted;
                                message.error_message = Some("request aborted".to_owned());
                                producer.end(Some(message)).await;
                                return;
                            }
                        }
                    }
                    let mut message = AssistantMessage::pending(&model);
                    message.content.push(ContentBlock::text("done"));
                    message.stop_reason = StopReason::Stop;
                    producer.end(Some(message)).await;
                });
                stream
            })
        })
    }
}

/// Creates a recorded parent session with a custom `stream_fn` (for
/// controlled task gating) and danger-full-access resources attached.
async fn recorded_parent_with_stream(
    artifacts: &Path,
    session_id: &str,
    model: &Model,
    stream_fn: Option<StreamFn>,
) -> (Session, SessionRecorder) {
    let session_dir = test_sessions_root();
    let recorder = session_store::start_session_in(
        artifacts,
        Some(model),
        None,
        Some(&session_dir),
        Some(session_id),
        None,
    )
    .expect("parent recorder");
    let session = Session::new_with_additional_tools_filtered_and_discovery(
        SessionOptions {
            model: model.clone(),
            cwd: artifacts.to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: "faux".into(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn,
            auth_resolver: None,
            auth_manager: None,
        },
        Vec::new(),
        Default::default(),
        ResourceDiscovery::Disabled,
    )
    .expect("parent session");
    session.set_session_dir(session_dir);
    session.record(recorder.clone()).expect("record parent");
    attach_resources(&session, artifacts).await;
    (session, recorder)
}

/// Creates a shared `TodoChildService` configured with a `TodoChildTestFactory`.
/// The same factory `Arc` is reused across all parent Applications that attach
/// this service's client.
fn shared_service() -> (Arc<TodoChildService>, Arc<TodoChildTestFactory>) {
    let factory = Arc::new(TodoChildTestFactory::new());
    let service = TodoChildService::new();
    service
        .configure_factory(factory.clone())
        .expect("configure factory");
    (service, factory)
}

/// Builds an `Application` from a recorded parent session and attaches the
/// service's `TodoChildClient` — the frozen wiring path that replaces the
/// old `attach_runtime_factory` call.
async fn application_with_service(
    session: Session,
    service: &Arc<TodoChildService>,
) -> Application {
    let application = Application::new(session).await;
    application.with_hermetic_repository_executor(
        pi_coding::process::HermeticRepositoryExecutor::default_executor(),
    );
    application
        .attach_todo_child_client(service.client())
        .expect("attach todo child client");
    application
}

#[derive(Clone)]
struct CapturedChildRequest {
    model: Model,
    context: Context,
    options: SimpleStreamOptions,
}

/// Captures the exact boundary passed to the child model. Each invocation
/// consumes an optional scripted faux response; absent entries succeed with
/// deterministic text.
fn capturing_stream_with_responses(
    requests: Arc<parking_lot::Mutex<Vec<CapturedChildRequest>>>,
    responses: Arc<parking_lot::Mutex<VecDeque<FauxResponse>>>,
) -> StreamFn {
    Arc::new(move |model, context, options| {
        requests.lock().push(CapturedChildRequest {
            model: model.clone(),
            context: context.clone(),
            options: options.clone(),
        });
        let response = responses
            .lock()
            .pop_front()
            .unwrap_or_else(|| FauxResponse::text("captured"));
        Box::pin(async move {
            let stream = new_assistant_message_event_stream();
            let producer = stream.clone();
            tokio::spawn(async move {
                let mut message = AssistantMessage::pending(&model);
                message.content = response.content;
                message.stop_reason = response.stop_reason;
                message.error_message = response.error_message;
                producer.end(Some(message)).await;
            });
            stream
        })
    })
}

fn capturing_stream(requests: Arc<parking_lot::Mutex<Vec<CapturedChildRequest>>>) -> StreamFn {
    capturing_stream_with_responses(requests, Arc::new(parking_lot::Mutex::new(VecDeque::new())))
}

fn messages_text(messages: &[Message]) -> String {
    messages
        .iter()
        .flat_map(|message| match message {
            Message::User(message) => message.content.iter(),
            Message::Assistant(message) => message.content.iter(),
            Message::ToolResult(message) => message.content.iter(),
            Message::BashExecution(_)
            | Message::Custom(_)
            | Message::BranchSummary(_)
            | Message::CompactionSummary(_) => {
                panic!("provider context contains an unprojected session message")
            }
        })
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn request_context_text(context: &Context) -> String {
    messages_text(&context.messages)
}

fn captured_request_with_marker<'a>(
    requests: &'a [CapturedChildRequest],
    marker: &str,
) -> &'a CapturedChildRequest {
    requests
        .iter()
        .find(|request| request_context_text(&request.context).contains(marker))
        .unwrap_or_else(|| panic!("missing captured request for {marker}"))
}

async fn wait_captured_requests(
    requests: &parking_lot::Mutex<Vec<CapturedChildRequest>>,
    expected: usize,
) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while requests.lock().len() < expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("expected {expected} captured child model requests"));
}

/// Waits for a task to reach the expected `TodoChildTaskStatus`, returning the
/// task snapshot. Polls with `yield_now` so it works on the current-thread
/// runtime without busy-spinning.
async fn wait_task_status(
    application: &Application,
    child_id: &str,
    task_id: &str,
    expected: TodoChildTaskStatus,
    timeout_secs: u64,
) -> TodoChildTask {
    tokio::time::timeout(Duration::from_secs(timeout_secs), async {
        loop {
            let snap = application
                .todo_child_status(None, Some(child_id))
                .await
                .expect("status");
            if let Some(task) = snap.tasks.iter().find(|t| t.id == task_id) {
                if task.status == expected {
                    return task.clone();
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("task {task_id} did not reach {expected:?} within {timeout_secs}s"))
}

/// Waits for the child status to reach the expected `TodoChildStatus`.
async fn wait_child_status(
    application: &Application,
    child_id: &str,
    expected: TodoChildStatus,
    timeout_secs: u64,
) {
    tokio::time::timeout(Duration::from_secs(timeout_secs), async {
        loop {
            let snap = application
                .todo_child_status(None, Some(child_id))
                .await
                .expect("status");
            if snap.status == expected {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!("child {child_id} did not reach {expected:?} within {timeout_secs}s")
    })
}

/// Waits for the child to become idle (not running).
async fn wait_child_idle(application: &Application, child_id: &str, timeout_secs: u64) {
    tokio::time::timeout(Duration::from_secs(timeout_secs), async {
        loop {
            let snap = application
                .todo_child_status(None, Some(child_id))
                .await
                .expect("status");
            if !snap.is_running {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("child {child_id} did not become idle within {timeout_secs}s"));
}

#[tokio::test]
async fn natural_language_code_task_routes_to_one_durable_selected_child() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (model, registration) = faux_model();
    set_child_responses(&registration, 1);
    let marker_path = artifacts.path().join("natural-language-child-edit.log");
    let child_entered = Arc::new(tokio::sync::Semaphore::new(0));
    let child_release = CancellationToken::new();
    let child_invocations = Arc::new(AtomicUsize::new(0));
    let factory = Arc::new(TodoChildTestFactory::with_runtime_stream(
        receipt_side_effect_stream(
            marker_path.clone(),
            child_entered.clone(),
            child_release.clone(),
            child_invocations.clone(),
        ),
    ));
    let service = TodoChildService::new();
    service
        .configure_factory(factory)
        .expect("configure routing factory");
    let (session, recorder) =
        recorded_parent(artifacts.path(), "parent-natural-language-routing", &model).await;
    attach_auto_route_resources(&session, artifacts.path()).await;
    let application = application_with_service(session, &service).await;
    let parent_history_before = application.session().history();
    let parent_todo_before = application.todo_state();
    let parent_model_before = application.session().model();

    let first = application
        .prompt(
            "implement a parser in src/lib.rs".to_owned(),
            Vec::new(),
            None,
        )
        .await
        .expect("route first code task");
    assert!(
        first.is_none(),
        "parent turn must not start for a routed code task"
    );
    let child = application
        .todo_child_list(None)
        .await
        .expect("list routed child")
        .into_iter()
        .next()
        .expect("one routed child");
    let first_receipt = application
        .todo_child_status(None, Some(&child.child_session_id))
        .await
        .expect("first routed status")
        .receipts
        .into_iter()
        .next()
        .expect("first routed receipt");
    assert_eq!(first_receipt.child_session_id, child.child_session_id);
    assert_eq!(application.todo_state(), parent_todo_before);
    assert_eq!(application.session().model(), parent_model_before);
    assert!(application.active_turn_id().is_none());
    assert!(!application.is_streaming());
    assert!(
        application
            .orchestration_runtime()
            .is_none_or(|runtime| runtime.jobs(None).is_empty()),
        "routing must not create parent task jobs",
    );

    let entered_permit = tokio::time::timeout(Duration::from_secs(5), child_entered.acquire())
        .await
        .expect("routed child did not start")
        .expect("child-entry semaphore remains open");
    entered_permit.forget();
    wait_child_status(
        &application,
        &child.child_session_id,
        TodoChildStatus::Running,
        5,
    )
    .await;
    let running = application
        .todo_child_status(None, Some(&child.child_session_id))
        .await
        .expect("running routed status");
    assert_eq!(running.receipts.len(), 1);
    assert_eq!(running.receipts[0].id, first_receipt.id);
    assert_eq!(running.receipts[0].status, TodoChildReceiptStatus::Running);
    let parent_follow_up = application
        .prompt("what is rust?".to_owned(), Vec::new(), None)
        .await
        .expect("accept parent follow-up while child runs")
        .expect("question must run on parent");
    application.wait_for_idle().await;
    assert_eq!(
        application
            .turn_lifecycle_snapshot()
            .last_terminal
            .as_ref()
            .map(|terminal| terminal.turn_id.as_str()),
        Some(parent_follow_up.turn_id.as_str()),
    );
    assert_eq!(application.todo_state(), parent_todo_before);
    assert_eq!(application.session().model(), parent_model_before);

    let still_running = application
        .todo_child_status(None, Some(&child.child_session_id))
        .await
        .expect("child remains running after parent settles");
    assert!(still_running.is_running);
    assert_eq!(still_running.receipts.len(), 1);
    assert_eq!(still_running.receipts[0].id, first_receipt.id);
    assert_eq!(
        still_running.receipts[0].status,
        TodoChildReceiptStatus::Running
    );
    assert!(application.active_turn_id().is_none());
    assert!(!application.is_streaming());
    child_release.cancel();
    wait_child_idle(&application, &child.child_session_id, 5).await;
    assert_eq!(marker_execution_count(&marker_path), 1);
    let settled = application
        .todo_child_status(None, Some(&child.child_session_id))
        .await
        .expect("settled routed status");
    assert!(!settled.is_running);
    assert_eq!(
        settled.receipts.len(),
        1,
        "one routed message has one identity-bound receipt"
    );
    assert_eq!(settled.receipts[0].id, first_receipt.id);
    assert_eq!(
        settled.receipts[0].status,
        TodoChildReceiptStatus::Completed
    );
    assert_eq!(
        settled.receipts[0].summary.as_deref(),
        Some("receipt side effect completed")
    );
    let image = ContentBlock::Image {
        data: "aW1hZ2U=".to_owned(),
        mime_type: "image/png".to_owned(),
    };
    let second = application
        .prompt(
            "fix parser tests in tests/parser.rs".to_owned(),
            vec![image.clone()],
            None,
        )
        .await
        .expect("route second code task");
    assert!(second.is_none());
    let list = application
        .todo_child_list(None)
        .await
        .expect("list reused child");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].child_session_id, child.child_session_id);
    let receipts = application
        .todo_child_status(None, Some(&child.child_session_id))
        .await
        .expect("reused child status")
        .receipts;
    assert_eq!(receipts.len(), 2);
    assert_eq!(receipts[0].id, first_receipt.id);
    assert_ne!(receipts[0].id, receipts[1].id);
    wait_child_idle(&application, &child.child_session_id, 5).await;
    assert_eq!(child_invocations.load(Ordering::SeqCst), 3);

    let child_path = recorder
        .path()
        .parent()
        .expect("parent session directory")
        .join("children")
        .join(recorder.id())
        .join(format!("{}.jsonl", child.child_session_id));
    let child_tree = session_store::load_session_tree(&child_path).expect("load routed child tree");
    let first_receipt_statuses = child_tree
        .branch(None)
        .iter()
        .filter(|entry| entry.custom_type.as_deref() == Some("pi.todo_child.receipt"))
        .filter_map(|entry| entry.data.as_ref())
        .filter(|payload| payload["receipt"]["id"].as_str() == Some(first_receipt.id.as_str()))
        .filter_map(|payload| payload["receipt"]["status"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        first_receipt_statuses,
        vec!["queued", "running", "completed"],
        "the routed receipt must have one durable transition per lifecycle state",
    );
    let child_text = messages_text(&child_tree.build_context(None).messages);
    assert!(child_text.contains("implement a parser in src/lib.rs"));
    assert!(child_text.contains("receipt side effect completed"));
    assert!(
        !child_text.contains("what is rust?"),
        "post-fork Main turns must not enter the child transcript"
    );
    let image_json = serde_json::to_value(&image).expect("image json");
    assert!(child_tree.branch(None).iter().any(|entry| {
        entry.custom_type.as_deref() == Some("pi.todo_child.receipt")
            && entry.data.as_ref().is_some_and(|payload| {
                payload["message"]["images"]
                    .as_array()
                    .is_some_and(|images| images.contains(&image_json))
            })
    }));
    assert!(
        child_path.is_file(),
        "routed child transcript must be durable"
    );
    let parent_text = messages_text(
        &recorder
            .tree()
            .expect("parent tree")
            .build_context(None)
            .messages,
    );
    assert!(!parent_text.contains("implement a parser in src/lib.rs"));
    assert!(!parent_text.contains("fix parser tests in tests/parser.rs"));
    assert!(parent_text.contains("what is rust?"));
    assert!(!parent_text.contains("receipt side effect completed"));
    assert!(application.orchestration_runtime().is_none_or(|runtime| {
        !runtime
            .jobs(None)
            .iter()
            .any(|job| job.todo_task_id.is_some())
    }));
    let child_detail = application
        .todo_child_detail(&recorder.id(), &child.child_session_id)
        .await
        .expect("routed child detail");
    assert_eq!(
        child_detail.workspace.cwd,
        artifacts.path().to_string_lossy()
    );
    assert!(application.session().history().len() > parent_history_before.len());

    service.shutdown().await.expect("shutdown routing service");
    application.cleanup().await;
    registration.unregister();
}
/// A natural-language routed child stays owned by the process service when
/// the originating parent disconnects. A different displayed parent cannot
/// see it, and returning to the original durable parent can observe the live
/// child and route later work to the same identity.
#[tokio::test]
async fn natural_language_route_survives_parent_disconnect_and_a_b_a_switch() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (model, registration) = faux_model();
    let controlled = ControlledStream::new();
    let service = TodoChildService::new();
    service
        .configure_factory(Arc::new(TodoChildTestFactory::with_runtime_stream(
            controlled.stream_fn(),
        )))
        .expect("configure routing factory");
    let (session_a, recorder_a) =
        recorded_parent(artifacts.path(), "parent-natural-language-switch-a", &model).await;
    attach_auto_route_resources(&session_a, artifacts.path()).await;
    let app_a = application_with_service(session_a, &service).await;

    let gate = controlled.arm();
    assert!(
        app_a
            .prompt(
                "implement the active child switch fixture in src/lib.rs".to_owned(),
                Vec::new(),
                None,
            )
            .await
            .expect("route code task")
            .is_none()
    );
    let child = app_a
        .todo_child_list(None)
        .await
        .expect("list routed child")
        .into_iter()
        .next()
        .expect("routed child");
    wait_child_status(&app_a, &child.child_session_id, TodoChildStatus::Running, 5).await;

    app_a.cleanup().await;
    drop(app_a);

    let (session_b, _) =
        recorded_parent(artifacts.path(), "parent-natural-language-switch-b", &model).await;
    attach_auto_route_resources(&session_b, artifacts.path()).await;
    let app_b = application_with_service(session_b, &service).await;
    assert!(
        app_b
            .todo_child_list(None)
            .await
            .expect("list parent B")
            .is_empty(),
        "displayed parent B must not inherit parent A's routed child",
    );
    app_b.cleanup().await;
    drop(app_b);

    let resumed_a = Session::new_with_additional_tools_filtered_and_discovery(
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
            stream_fn: None,
            auth_resolver: None,
            auth_manager: None,
        },
        Vec::new(),
        Default::default(),
        ResourceDiscovery::Disabled,
    )
    .expect("resume parent A");
    resumed_a.set_session_dir(test_sessions_root());
    resumed_a
        .record(recorder_a)
        .expect("record resumed parent A");
    attach_auto_route_resources(&resumed_a, artifacts.path()).await;
    let restored_a = application_with_service(resumed_a, &service).await;
    let running = restored_a
        .todo_child_status(None, Some(&child.child_session_id))
        .await
        .expect("status after A-B-A switch");
    assert_eq!(running.status, TodoChildStatus::Running);
    assert_eq!(running.receipts.len(), 1);

    gate.cancel();
    wait_child_idle(&restored_a, &child.child_session_id, 5).await;
    assert!(
        restored_a
            .prompt(
                "fix the active child switch fixture in tests/switch.rs".to_owned(),
                Vec::new(),
                None,
            )
            .await
            .expect("route after A-B-A switch")
            .is_none()
    );
    wait_child_idle(&restored_a, &child.child_session_id, 5).await;
    let children = restored_a
        .todo_child_list(None)
        .await
        .expect("list restored A");
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].child_session_id, child.child_session_id);
    let continued = restored_a
        .todo_child_status(None, Some(&child.child_session_id))
        .await
        .expect("continued routed child");
    assert_eq!(continued.receipts.len(), 2);

    service.shutdown().await.expect("shutdown routing service");
    restored_a.cleanup().await;
    registration.unregister();
}
/// Two Todo children from the same parent with different idempotency keys have
/// independent child session IDs and receipts. Sending to each returns a
/// receipt routed to the correct child.
#[tokio::test]
async fn same_parent_two_todo_children_parallel() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (model, registration) = faux_model();
    set_child_responses(&registration, 8);
    let (session, _recorder) = recorded_parent(artifacts.path(), "parent-parallel", &model).await;
    let (application, _service) = application_with_factory(session).await;

    let child_a = application
        .todo_child_create(TodoChildCreateRequest {
            label: Some("alpha".into()),
            idempotency_key: Some("key-alpha".into()),
        })
        .await
        .expect("create child A");
    let child_b = application
        .todo_child_create(TodoChildCreateRequest {
            label: Some("beta".into()),
            idempotency_key: Some("key-beta".into()),
        })
        .await
        .expect("create child B");

    assert_ne!(
        child_a.identity.child_session_id, child_b.identity.child_session_id,
        "two children from the same parent must have distinct session IDs"
    );
    assert_eq!(
        child_a.identity.parent_session_id, child_b.identity.parent_session_id,
        "both children share the same parent session ID"
    );

    let receipt_a = application
        .todo_child_send(
            None,
            Some(&child_a.identity.child_session_id),
            "do alpha".into(),
        )
        .await
        .expect("send to A");
    let receipt_b = application
        .todo_child_send(
            None,
            Some(&child_b.identity.child_session_id),
            "do beta".into(),
        )
        .await
        .expect("send to B");

    assert_eq!(
        receipt_a.child_session_id,
        child_a.identity.child_session_id
    );
    assert_eq!(
        receipt_b.child_session_id,
        child_b.identity.child_session_id
    );
    assert_ne!(receipt_a.id, receipt_b.id, "receipts must be distinct");
    assert_ne!(receipt_a.message_id, receipt_b.message_id);

    _service.shutdown().await.expect("shutdown");
    application.cleanup().await;
    registration.unregister();
}

/// Labels are parent-scoped: two different parent sessions can each create a
/// child with the same label without cross-contaminating the manifest entries.
#[tokio::test]
async fn parent_scoped_labels_isolated() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (model, registration) = faux_model();
    set_child_responses(&registration, 4);

    let (session_a, _recorder_a) =
        recorded_parent(artifacts.path(), "parent-a-labels", &model).await;
    let (session_b, _recorder_b) =
        recorded_parent(artifacts.path(), "parent-b-labels", &model).await;

    let (app_a, _service_a) = application_with_factory(session_a).await;
    let (app_b, _service_b) = application_with_factory(session_b).await;

    let child_a = app_a
        .todo_child_create(TodoChildCreateRequest {
            label: Some("shared-label".into()),
            idempotency_key: Some("key-a-shared".into()),
        })
        .await
        .expect("create child A");
    let child_b = app_b
        .todo_child_create(TodoChildCreateRequest {
            label: Some("shared-label".into()),
            idempotency_key: Some("key-b-shared".into()),
        })
        .await
        .expect("create child B");

    let list_a = app_a.todo_child_list(None).await.expect("list A");
    let list_b = app_b.todo_child_list(None).await.expect("list B");

    assert_eq!(list_a.len(), 1, "parent A manifest has exactly one child");
    assert_eq!(list_b.len(), 1, "parent B manifest has exactly one child");
    assert_eq!(
        list_a[0].child_session_id,
        child_a.identity.child_session_id
    );
    assert_eq!(
        list_b[0].child_session_id,
        child_b.identity.child_session_id
    );
    assert_ne!(
        list_a[0].child_session_id, list_b[0].child_session_id,
        "same label from different parents must yield distinct children"
    );
    assert_eq!(list_a[0].label.as_deref(), Some("shared-label"));
    assert_eq!(list_b[0].label.as_deref(), Some("shared-label"));

    _service_a.shutdown().await.expect("shutdown A");
    _service_b.shutdown().await.expect("shutdown B");
    app_a.cleanup().await;
    app_b.cleanup().await;
    registration.unregister();
}

/// After dropping the Application and rebuilding from the same parent recorder,
/// the child is still addressable via the manifest and status can be queried.
#[tokio::test]
async fn restart_switching_separate_addressing() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (model, registration) = faux_model();
    set_child_responses(&registration, 4);

    let (session, recorder) = recorded_parent(artifacts.path(), "parent-restart", &model).await;
    let session_id = recorder.id();

    // First Application: create a child.
    let (app_a, _service_a) = application_with_factory(session).await;
    recorder
        .record_message(&Message::user_text("concrete restart ancestry leaf", 1000))
        .expect("record concrete ancestry leaf");
    let fork = recorder
        .capture_fork_point()
        .expect("capture concrete parent fork");
    let expected_parent_entry_id = fork
        .parent_active_leaf_id
        .clone()
        .expect("recorded parent fork must have a non-null active leaf");
    let expected_parent_revision = fork.parent_revision;
    assert!(
        expected_parent_revision > 0,
        "recorded parent fork must advance revision"
    );
    drop(fork);
    let child = app_a
        .todo_child_create(TodoChildCreateRequest {
            label: Some("restart-child".into()),
            idempotency_key: Some("key-restart".into()),
        })
        .await
        .expect("create child");
    let child_id = child.identity.child_session_id.clone();
    let entry_before_restart = app_a
        .todo_child_list(None)
        .await
        .expect("list before restart")
        .into_iter()
        .find(|entry| entry.child_session_id == child_id)
        .expect("child entry before restart");
    assert_eq!(
        entry_before_restart.parent_entry_id.as_deref(),
        Some(expected_parent_entry_id.as_str()),
    );
    assert_eq!(
        entry_before_restart.parent_revision,
        expected_parent_revision
    );
    _service_a.shutdown().await.expect("shutdown A");
    app_a.cleanup().await;
    drop(app_a);

    // Second Application: create a fresh session that shares the same recorder
    // (and thus the same manifest storage lease) so the child is discoverable.
    let session_b = Session::new_with_additional_tools_filtered_and_discovery(
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
            stream_fn: None,
            auth_resolver: None,
            auth_manager: None,
        },
        Vec::new(),
        Default::default(),
        ResourceDiscovery::Disabled,
    )
    .expect("session B");
    session_b.set_session_dir(test_sessions_root());
    session_b.record(recorder).expect("record B");
    attach_resources(&session_b, artifacts.path()).await;
    let (app_b, _service_b) = application_with_factory(session_b).await;

    let list = app_b
        .todo_child_list(None)
        .await
        .expect("list after restart");
    assert!(
        list.iter().any(|e| e.child_session_id == child_id),
        "child must still be in the manifest after restart: {list:?}"
    );
    let recovered_entry = list
        .iter()
        .find(|entry| entry.child_session_id == child_id)
        .expect("recovered child manifest entry");
    assert_eq!(
        recovered_entry.parent_entry_id.as_deref(),
        Some(expected_parent_entry_id.as_str()),
        "restart must preserve the concrete fork leaf",
    );
    assert_eq!(
        recovered_entry.parent_revision, expected_parent_revision,
        "restart must preserve the concrete fork revision",
    );

    let status = app_b
        .todo_child_status(None, Some(&child_id))
        .await
        .expect("status after restart");
    assert_eq!(status.child_session_id, child_id);
    assert_eq!(status.parent_session_id, session_id);

    _service_b.shutdown().await.expect("shutdown B");
    app_b.cleanup().await;
    registration.unregister();
}

/// Stopping one child does not affect a sibling: the sibling remains
/// addressable and its status is independent of the stopped child.
#[tokio::test]
async fn stop_one_preserves_sibling() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (model, registration) = faux_model();
    set_child_responses(&registration, 8);

    let (session, _recorder) =
        recorded_parent(artifacts.path(), "parent-stop-sibling", &model).await;
    let (application, _service) = application_with_factory(session).await;

    let child_a = application
        .todo_child_create(TodoChildCreateRequest {
            label: Some("stop-me".into()),
            idempotency_key: Some("key-stop".into()),
        })
        .await
        .expect("create child A");
    let child_b = application
        .todo_child_create(TodoChildCreateRequest {
            label: Some("keep-me".into()),
            idempotency_key: Some("key-keep".into()),
        })
        .await
        .expect("create child B");

    let stopped = application
        .todo_child_stop(None, Some(&child_a.identity.child_session_id))
        .await
        .expect("stop A");
    assert_eq!(stopped.status, TodoChildStatus::Stopped);

    // Sibling B must still be addressable and not Stopped.
    let status_b = application
        .todo_child_status(None, Some(&child_b.identity.child_session_id))
        .await
        .expect("status B");
    assert_ne!(
        status_b.status,
        TodoChildStatus::Stopped,
        "stopping child A must not stop sibling B"
    );

    // Sending to B must still work.
    let receipt_b = application
        .todo_child_send(
            None,
            Some(&child_b.identity.child_session_id),
            "still alive".into(),
        )
        .await
        .expect("send to B after stop A");
    assert_eq!(
        receipt_b.child_session_id,
        child_b.identity.child_session_id
    );

    // Sending to the stopped child must fail.
    let send_stopped = application
        .todo_child_send(
            None,
            Some(&child_a.identity.child_session_id),
            "dead".into(),
        )
        .await;
    assert!(
        send_stopped.is_err(),
        "sending to a stopped child must fail"
    );

    _service.shutdown().await.expect("shutdown");
    application.cleanup().await;
    registration.unregister();
}

/// Fork-time transcript visibility: child A forked at t1 sees only t1
/// messages; child B forked at t2 sees both t1 and t2 messages. The
/// transcript_entries count differs because B inherited more parent entries.
#[tokio::test]
async fn fork_time_transcript_visibility() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (model, registration) = faux_model();
    set_child_responses(&registration, 4);

    let (session, recorder) = recorded_parent(artifacts.path(), "parent-fork-time", &model).await;
    let (application, _service) = application_with_factory(session).await;

    // t1: record two messages on the parent.
    recorder
        .record_message(&Message::user_text("parent msg t1-a", 1000))
        .expect("record t1-a");
    recorder
        .record_message(&Message::user_text("parent msg t1-b", 1001))
        .expect("record t1-b");

    // Fork child A at t1.
    let child_a = application
        .todo_child_create(TodoChildCreateRequest {
            label: Some("fork-t1".into()),
            idempotency_key: Some("key-t1".into()),
        })
        .await
        .expect("create child A");
    let entries_a = child_a.snapshot.transcript_entries;

    // t2: record two more messages on the parent.
    recorder
        .record_message(&Message::user_text("parent msg t2-a", 2000))
        .expect("record t2-a");
    recorder
        .record_message(&Message::user_text("parent msg t2-b", 2001))
        .expect("record t2-b");

    // Fork child B at t2.
    let child_b = application
        .todo_child_create(TodoChildCreateRequest {
            label: Some("fork-t2".into()),
            idempotency_key: Some("key-t2".into()),
        })
        .await
        .expect("create child B");
    let entries_b = child_b.snapshot.transcript_entries;

    assert!(
        entries_b > entries_a,
        "child B (forked at t2) must have more transcript entries than child A (forked at t1): \
         A={entries_a}, B={entries_b}"
    );

    // Child A's transcript must not change after t2 messages are recorded.
    let status_a = application
        .todo_child_status(None, Some(&child_a.identity.child_session_id))
        .await
        .expect("status A");
    assert_eq!(
        status_a.transcript_entries, entries_a,
        "child A transcript must be frozen at its fork point"
    );

    _service.shutdown().await.expect("shutdown");
    application.cleanup().await;
    registration.unregister();
}

/// Exact model-boundary fork visibility and bidirectional isolation for two
/// siblings. Child A is forked after t1, child B after t2, and both continue
/// after later parent writes without seeing them or each other's turns. The
/// parent branch likewise never receives child task or assistant content.
#[tokio::test]
async fn exact_two_child_markers_preserve_fork_and_reverse_isolation() {
    const T1: &str = "exact parent t1 marker c7a6b1";
    const T2: &str = "exact parent t2 marker 08d96e";
    const AFTER_B: &str = "parent marker after both forks f032e7";
    const PARENT_CONTINUE: &str = "parent continuation after child turns db72aa";
    const A_FIRST: &str = "child A first task 7cc102";
    const B_FIRST: &str = "child B first task ef2509";
    const A_CONTINUE: &str = "child A continuation task d7cb21";
    const B_CONTINUE: &str = "child B continuation task 185d90";
    const A_ASSISTANT: &str = "child A private assistant marker 19f466";
    const B_ASSISTANT: &str = "child B private assistant marker b931d8";

    let artifacts = tempfile::tempdir().expect("artifacts");
    let (model, registration) = faux_model();
    let requests = Arc::new(parking_lot::Mutex::new(Vec::<CapturedChildRequest>::new()));
    let responses = Arc::new(parking_lot::Mutex::new(VecDeque::from([
        FauxResponse::text(A_ASSISTANT),
        FauxResponse::text(B_ASSISTANT),
        FauxResponse::text("child A continued privately"),
        FauxResponse::text("child B continued privately"),
    ])));
    let (session, recorder) = recorded_parent_with_stream(
        artifacts.path(),
        "parent-exact-two-child-markers",
        &model,
        Some(capturing_stream_with_responses(requests.clone(), responses)),
    )
    .await;
    let (service, _factory) = shared_service();
    let application = application_with_service(session, &service).await;

    recorder
        .record_message(&Message::user_text(T1, 1000))
        .expect("record t1 marker");
    let child_a = application
        .todo_child_create(TodoChildCreateRequest {
            label: Some("exact-t1-child".into()),
            idempotency_key: Some("exact-t1-child".into()),
        })
        .await
        .expect("fork child A at t1");
    recorder
        .record_message(&Message::user_text(T2, 2000))
        .expect("record t2 marker");
    let child_b = application
        .todo_child_create(TodoChildCreateRequest {
            label: Some("exact-t2-child".into()),
            idempotency_key: Some("exact-t2-child".into()),
        })
        .await
        .expect("fork child B at t2");
    recorder
        .record_message(&Message::user_text(AFTER_B, 3000))
        .expect("record marker after both forks");

    let a_first = application
        .todo_child_add_task(
            None,
            &child_a.identity.child_session_id,
            TodoChildAddTaskRequest {
                content: A_FIRST.into(),
                ..Default::default()
            },
        )
        .await
        .expect("run child A first turn");
    wait_task_status(
        &application,
        &child_a.identity.child_session_id,
        &a_first.task.id,
        TodoChildTaskStatus::Completed,
        5,
    )
    .await;
    let b_first = application
        .todo_child_add_task(
            None,
            &child_b.identity.child_session_id,
            TodoChildAddTaskRequest {
                content: B_FIRST.into(),
                ..Default::default()
            },
        )
        .await
        .expect("run child B first turn");
    wait_task_status(
        &application,
        &child_b.identity.child_session_id,
        &b_first.task.id,
        TodoChildTaskStatus::Completed,
        5,
    )
    .await;

    recorder
        .record_message(&Message::user_text(PARENT_CONTINUE, 4000))
        .expect("record later parent continuation");
    let a_continue = application
        .todo_child_add_task(
            None,
            &child_a.identity.child_session_id,
            TodoChildAddTaskRequest {
                content: A_CONTINUE.into(),
                ..Default::default()
            },
        )
        .await
        .expect("continue child A");
    wait_task_status(
        &application,
        &child_a.identity.child_session_id,
        &a_continue.task.id,
        TodoChildTaskStatus::Completed,
        5,
    )
    .await;
    let b_continue = application
        .todo_child_add_task(
            None,
            &child_b.identity.child_session_id,
            TodoChildAddTaskRequest {
                content: B_CONTINUE.into(),
                ..Default::default()
            },
        )
        .await
        .expect("continue child B");
    wait_task_status(
        &application,
        &child_b.identity.child_session_id,
        &b_continue.task.id,
        TodoChildTaskStatus::Completed,
        5,
    )
    .await;
    wait_captured_requests(&requests, 4).await;

    {
        let captured = requests.lock();
        let a_first_request = captured_request_with_marker(&captured, A_FIRST);
        let b_first_request = captured_request_with_marker(&captured, B_FIRST);
        let a_continue_request = captured_request_with_marker(&captured, A_CONTINUE);
        let b_continue_request = captured_request_with_marker(&captured, B_CONTINUE);

        for request in [a_first_request, a_continue_request] {
            let text = request_context_text(&request.context);
            assert!(text.contains(T1), "child A lost t1: {text}");
            assert!(!text.contains(T2), "child A crossed its t1 fork: {text}");
            assert!(
                !text.contains(AFTER_B),
                "child A saw post-fork parent marker: {text}"
            );
            assert!(
                !text.contains(PARENT_CONTINUE),
                "child A saw parent continuation: {text}"
            );
            assert!(!text.contains(B_FIRST), "child A saw sibling task: {text}");
            assert!(
                !text.contains(B_CONTINUE),
                "child A saw sibling continuation: {text}"
            );
            assert!(
                !text.contains(B_ASSISTANT),
                "child A saw sibling assistant: {text}"
            );
        }
        for request in [b_first_request, b_continue_request] {
            let text = request_context_text(&request.context);
            assert!(text.contains(T1), "child B lost t1: {text}");
            assert!(text.contains(T2), "child B lost t2: {text}");
            assert!(
                !text.contains(AFTER_B),
                "child B saw post-fork parent marker: {text}"
            );
            assert!(
                !text.contains(PARENT_CONTINUE),
                "child B saw parent continuation: {text}"
            );
            assert!(!text.contains(A_FIRST), "child B saw sibling task: {text}");
            assert!(
                !text.contains(A_CONTINUE),
                "child B saw sibling continuation: {text}"
            );
            assert!(
                !text.contains(A_ASSISTANT),
                "child B saw sibling assistant: {text}"
            );
        }
        let a_continue_text = request_context_text(&a_continue_request.context);
        assert!(a_continue_text.contains(A_FIRST));
        assert!(a_continue_text.contains(A_ASSISTANT));
        let b_continue_text = request_context_text(&b_continue_request.context);
        assert!(b_continue_text.contains(B_FIRST));
        assert!(b_continue_text.contains(B_ASSISTANT));
    }

    let parent_context = recorder.tree().expect("parent tree").build_context(None);
    let parent_text = messages_text(&parent_context.messages);
    for marker in [T1, T2, AFTER_B, PARENT_CONTINUE] {
        assert!(
            parent_text.contains(marker),
            "parent lost its own marker {marker}: {parent_text}"
        );
    }
    for marker in [
        A_FIRST,
        B_FIRST,
        A_CONTINUE,
        B_CONTINUE,
        A_ASSISTANT,
        B_ASSISTANT,
    ] {
        assert!(
            !parent_text.contains(marker),
            "child-only marker leaked into parent branch: {marker}: {parent_text}"
        );
    }

    service.shutdown().await.expect("shutdown");
    application.cleanup().await;
    registration.unregister();
}

/// After creating a child, recording more messages on the parent does not
/// pollute the child's transcript. The child's transcript_entries stays
/// unchanged.
#[tokio::test]
async fn parent_continuation_does_not_pollute_child() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (model, registration) = faux_model();
    set_child_responses(&registration, 4);

    let (session, recorder) =
        recorded_parent(artifacts.path(), "parent-continuation", &model).await;
    let (application, _service) = application_with_factory(session).await;

    recorder
        .record_message(&Message::user_text("before fork", 1000))
        .expect("record before");

    let child = application
        .todo_child_create(TodoChildCreateRequest {
            label: Some("continuation-child".into()),
            idempotency_key: Some("key-cont".into()),
        })
        .await
        .expect("create child");
    let entries_before = child.snapshot.transcript_entries;
    assert!(entries_before > 0, "child must inherit parent entries");

    // Record more parent messages.
    for i in 0..5 {
        recorder
            .record_message(&Message::user_text(format!("after fork {i}"), 2000 + i))
            .expect("record after");
    }

    let status = application
        .todo_child_status(None, Some(&child.identity.child_session_id))
        .await
        .expect("status");
    assert_eq!(
        status.transcript_entries, entries_before,
        "child transcript must not change after parent continuation"
    );

    _service.shutdown().await.expect("shutdown");
    application.cleanup().await;
    registration.unregister();
}

/// Rewinding the parent session does not change a child's fork-base ancestry:
/// the manifest entry's parent_entry_id and parent_revision remain the values
/// captured at fork time.
#[tokio::test]
async fn rewind_preserves_child_ancestry() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (model, registration) = faux_model();
    set_child_responses(&registration, 4);

    let (session, recorder) = recorded_parent(artifacts.path(), "parent-rewind", &model).await;
    let (application, _service) = application_with_factory(session).await;

    // Record several messages so the rewind has something to truncate.
    for i in 0..6 {
        recorder
            .record_message(&Message::user_text(format!("msg {i}"), 1000 + i as i64))
            .expect("record");
    }

    let child = application
        .todo_child_create(TodoChildCreateRequest {
            label: Some("rewind-child".into()),
            idempotency_key: Some("key-rewind".into()),
        })
        .await
        .expect("create child");

    // Capture the fork-base ancestry from the manifest entry.
    let list_before = application
        .todo_child_list(None)
        .await
        .expect("list before");
    let entry_before = list_before
        .iter()
        .find(|e| e.child_session_id == child.identity.child_session_id)
        .expect("child in manifest");
    let parent_entry_id_before = entry_before.parent_entry_id.clone();
    let parent_revision_before = entry_before.parent_revision;

    // Rewind the parent: drop the last 3 entries.
    let tree = recorder.tree().expect("tree");
    let keep = tree.entries.len().saturating_sub(3);
    if keep >= 1 && keep < tree.entries.len() {
        recorder.rewind_to(keep).expect("rewind");
    }

    // The child's manifest entry must still show the original fork base.
    let list_after = application.todo_child_list(None).await.expect("list after");
    let entry_after = list_after
        .iter()
        .find(|e| e.child_session_id == child.identity.child_session_id)
        .expect("child still in manifest");
    assert_eq!(
        entry_after.parent_entry_id, parent_entry_id_before,
        "rewind must not change the child's fork-base parent_entry_id"
    );
    assert_eq!(
        entry_after.parent_revision, parent_revision_before,
        "rewind must not change the child's fork-base parent_revision"
    );

    _service.shutdown().await.expect("shutdown");
    application.cleanup().await;
    registration.unregister();
}

/// Two forks from a parent with ongoing message recording capture distinct
/// parent revisions: each `capture_fork_point` observes a different snapshot
/// of the parent's revision counter. The first fork is spawned as a
/// background task and allowed to capture its snapshot; a message is then
/// recorded on the parent (incrementing the revision) before the second fork
/// captures its own snapshot, proving each fork is a point-in-time snapshot.
#[tokio::test]
async fn concurrent_forks_capture_distinct_revisions() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (model, registration) = faux_model();
    set_child_responses(&registration, 8);

    let (session, recorder) = recorded_parent(artifacts.path(), "parent-concurrent", &model).await;
    let (application, _service) = application_with_factory(session).await;

    // Record initial messages so the revision is well above zero.
    for i in 0..3 {
        recorder
            .record_message(&Message::user_text(format!("pre {i}"), 1000 + i as i64))
            .expect("record pre");
    }

    // Spawn the first fork as a background task. On the current-thread
    // runtime it starts running when the main task yields.
    let app_clone = application.clone();
    let create_a = tokio::spawn(async move {
        app_clone
            .todo_child_create(TodoChildCreateRequest {
                label: Some("concurrent-a".into()),
                idempotency_key: Some("key-conc-a".into()),
            })
            .await
            .expect("create A")
    });

    // Yield repeatedly so the spawned fork captures its fork point (which
    // happens synchronously before the first real await in `ensure_child`).
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }

    // Record a message on the parent: this increments the revision counter
    // after child A has captured its fork point but before child B captures.
    recorder
        .record_message(&Message::user_text("between forks", 9999))
        .expect("record between");

    // Create child B, which captures the new revision.
    let child_b = application
        .todo_child_create(TodoChildCreateRequest {
            label: Some("concurrent-b".into()),
            idempotency_key: Some("key-conc-b".into()),
        })
        .await
        .expect("create B");

    let child_a = create_a.await.expect("create A task panicked");

    let list = application.todo_child_list(None).await.expect("list");
    let entry_a = list
        .iter()
        .find(|e| e.child_session_id == child_a.identity.child_session_id)
        .expect("entry A");
    let entry_b = list
        .iter()
        .find(|e| e.child_session_id == child_b.identity.child_session_id)
        .expect("entry B");

    assert_ne!(
        entry_a.parent_revision, entry_b.parent_revision,
        "concurrent forks from a parent with ongoing recording must capture distinct revisions: \
         A={}, B={}",
        entry_a.parent_revision, entry_b.parent_revision
    );

    _service.shutdown().await.expect("shutdown");
    application.cleanup().await;
    registration.unregister();
}

/// Sending to a child_session_id that does not exist yet auto-creates the
/// child and returns a receipt. The child is then visible via status.
#[tokio::test]
async fn initialization_receipt_and_lifecycle_races() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (model, registration) = faux_model();
    set_child_responses(&registration, 4);

    let (session, _recorder) =
        recorded_parent(artifacts.path(), "parent-auto-create", &model).await;
    let (application, _service) = application_with_factory(session).await;

    // Generate a UUID for the auto-created child.
    let auto_child_id = uuid::Uuid::now_v7().to_string();

    // Send to a child that does not exist yet — should auto-create it.
    let receipt = application
        .todo_child_send(None, Some(&auto_child_id), "auto-create and run".into())
        .await
        .expect("auto-create send");

    assert_eq!(receipt.child_session_id, auto_child_id);
    assert_eq!(receipt.status, TodoChildReceiptStatus::Queued);
    assert!(!receipt.id.is_empty());

    // The child must now be visible via status.
    let status = application
        .todo_child_status(None, Some(&auto_child_id))
        .await
        .expect("status");
    assert_eq!(status.child_session_id, auto_child_id);
    assert!(
        !matches!(status.status, TodoChildStatus::Stopped),
        "auto-created child must not be Stopped"
    );

    // The child must appear in the manifest list.
    let list = application.todo_child_list(None).await.expect("list");
    assert!(
        list.iter().any(|e| e.child_session_id == auto_child_id),
        "auto-created child must appear in manifest"
    );

    _service.shutdown().await.expect("shutdown");
    application.cleanup().await;
    registration.unregister();
}

#[tokio::test]
async fn concurrent_first_sends_share_one_durable_child() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (model, registration) = faux_model();
    let controlled = ControlledStream::new();
    let (session, recorder) = recorded_parent_with_stream(
        artifacts.path(),
        "parent-concurrent-first-send",
        &model,
        Some(controlled.stream_fn()),
    )
    .await;
    let (service, factory) = shared_service();
    let application = application_with_service(session, &service).await;
    let child_session_id = uuid::Uuid::now_v7().to_string();
    let gate = controlled.arm();
    let first = tokio::spawn({
        let application = application.clone();
        let child_session_id = child_session_id.clone();
        async move {
            application
                .todo_child_send(None, Some(&child_session_id), "first".into())
                .await
        }
    });
    let second = tokio::spawn({
        let application = application.clone();
        let child_session_id = child_session_id.clone();
        async move {
            application
                .todo_child_send(None, Some(&child_session_id), "second".into())
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if application
                .todo_child_list(None)
                .await
                .expect("list during concurrent initialization")
                .iter()
                .any(|entry| entry.child_session_id == child_session_id)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("concurrent first sends did not create a durable manifest entry");
    let pause = tokio::spawn({
        let application = application.clone();
        let child_session_id = child_session_id.clone();
        async move {
            application
                .todo_child_pause(None, Some(&child_session_id))
                .await
        }
    });
    let first = first.await.expect("first task").expect("first send");
    let second = second.await.expect("second task").expect("second send");
    gate.cancel();
    let paused = pause.await.expect("pause task").expect("pause child");

    assert_eq!(first.child_session_id, child_session_id);
    assert_eq!(second.child_session_id, child_session_id);
    assert_ne!(first.id, second.id, "receipt ids must be distinct");
    assert_ne!(
        first.message_id, second.message_id,
        "message ids must be distinct"
    );
    assert_eq!(paused.status, TodoChildStatus::Paused);
    assert_eq!(
        paused.receipts.len(),
        2,
        "both first-send receipts must be durable"
    );
    assert_eq!(
        paused
            .receipts
            .iter()
            .map(|receipt| receipt.id.as_str())
            .collect::<HashSet<_>>(),
        HashSet::from([first.id.as_str(), second.id.as_str()]),
    );
    assert_eq!(
        paused
            .receipts
            .iter()
            .map(|receipt| receipt.message_id.as_str())
            .collect::<HashSet<_>>(),
        HashSet::from([first.message_id.as_str(), second.message_id.as_str()]),
    );
    let durable_receipts = paused.receipts.clone();
    let manifest = application.todo_child_list(None).await.expect("list");
    assert_eq!(
        manifest
            .iter()
            .filter(|entry| entry.child_session_id == child_session_id)
            .count(),
        1,
        "concurrent first sends must register exactly one child"
    );
    assert_eq!(
        manifest
            .iter()
            .find(|entry| entry.child_session_id == child_session_id)
            .expect("concurrent child manifest entry")
            .status,
        TodoChildStatus::Paused,
    );

    drop(application);
    drop(service);
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    let recovered_parent = Session::new_with_additional_tools_filtered_and_discovery(
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
            stream_fn: None,
            auth_resolver: None,
            auth_manager: None,
        },
        Vec::new(),
        Default::default(),
        ResourceDiscovery::Disabled,
    )
    .expect("recovered concurrent-send parent");
    recovered_parent.set_session_dir(test_sessions_root());
    recovered_parent
        .record(recorder)
        .expect("reuse concurrent-send recorder");
    attach_resources(&recovered_parent, artifacts.path()).await;
    let recovered_service = TodoChildService::new();
    recovered_service
        .configure_factory(factory)
        .expect("configure recovered service");
    let recovered = application_with_service(recovered_parent, &recovered_service).await;
    let recovered_manifest = recovered
        .todo_child_list(None)
        .await
        .expect("recovered list");
    assert_eq!(
        recovered_manifest
            .iter()
            .filter(|entry| entry.child_session_id == child_session_id)
            .count(),
        1,
        "restart must retain one linearized manifest entry",
    );
    let recovered_status = recovered
        .todo_child_status(None, Some(&child_session_id))
        .await
        .expect("recover concurrent first-send child");
    assert_eq!(recovered_status.status, TodoChildStatus::Paused);
    assert_eq!(
        recovered_status.receipts, durable_receipts,
        "restart must preserve both receipt/message identities and lifecycle outcomes",
    );

    recovered_service
        .shutdown()
        .await
        .expect("shutdown recovered service");
    recovered.cleanup().await;
    registration.unregister();
}

#[tokio::test]
async fn archived_child_stays_absent_after_restart() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (model, registration) = faux_model();
    let (session, recorder) = recorded_parent(artifacts.path(), "parent-archive", &model).await;
    let (application, _service) = application_with_factory(session).await;
    let child = application
        .todo_child_create(TodoChildCreateRequest {
            label: Some("archive".into()),
            idempotency_key: Some("archive-key".into()),
        })
        .await
        .expect("create child");
    application
        .todo_child_archive(None, &child.identity.child_session_id)
        .await
        .expect("archive child");
    assert!(
        application
            .todo_child_list(None)
            .await
            .expect("list after archive")
            .is_empty()
    );
    _service.shutdown().await.expect("shutdown");
    application.cleanup().await;

    let session = Session::new_with_additional_tools_filtered_and_discovery(
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
            stream_fn: None,
            auth_resolver: None,
            auth_manager: None,
        },
        Vec::new(),
        Default::default(),
        ResourceDiscovery::Disabled,
    )
    .expect("restart session");
    session.set_session_dir(test_sessions_root());
    session.record(recorder).expect("reuse parent recorder");
    attach_resources(&session, artifacts.path()).await;
    let (restarted, _restart_service) = application_with_factory(session).await;
    assert!(
        restarted
            .todo_child_list(None)
            .await
            .expect("list after restart")
            .is_empty()
    );
    assert!(
        restarted
            .todo_child_status(None, Some(&child.identity.child_session_id))
            .await
            .is_err(),
        "logical archive must prevent restart recovery"
    );

    _restart_service.shutdown().await.expect("shutdown restart");
    restarted.cleanup().await;
    registration.unregister();
}


/// Removal prunes one child, preserves siblings/selection, idempotent when absent.
#[tokio::test]
async fn removed_child_is_isolated_selection_aware_and_idempotent() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (model, registration) = faux_model();
    set_child_responses(&registration, 8);
    let (session, _recorder) =
        recorded_parent(artifacts.path(), "parent-remove-sibling", &model).await;
    let (application, _service) = application_with_factory(session).await;

    let child_a = application
        .todo_child_create(TodoChildCreateRequest {
            label: Some("remove-me".into()),
            idempotency_key: Some("key-remove".into()),
        })
        .await
        .expect("create child A");
    let child_b = application
        .todo_child_create(TodoChildCreateRequest {
            label: Some("keep-me".into()),
            idempotency_key: Some("key-keep".into()),
        })
        .await
        .expect("create child B");
    let child_c = application
        .todo_child_create(TodoChildCreateRequest {
            label: Some("fallback-me".into()),
            idempotency_key: Some("key-fallback".into()),
        })
        .await
        .expect("create child C");
    application
        .todo_child_select(None, &child_a.identity.child_session_id)
        .await
        .expect("select A");
    let receipt_a = application
        .todo_child_send(
            None,
            Some(&child_a.identity.child_session_id),
            "work for A".into(),
        )
        .await
        .expect("send to A");
    assert_eq!(receipt_a.child_session_id, child_a.identity.child_session_id);

    let manifest_path = child_artifact_root(&child_a.identity.parent_session_id)
        .join("todo-manifest.json");
    application
        .todo_child_remove(None, &child_b.identity.child_session_id)
        .await
        .expect("remove non-selected child B");
    let listed = application.todo_child_list(None).await.expect("list after remove");
    assert_eq!(
        listed
            .iter()
            .map(|entry| entry.child_session_id.as_str())
            .collect::<Vec<_>>(),
        vec![
            child_a.identity.child_session_id.as_str(),
            child_c.identity.child_session_id.as_str(),
        ],
        "removal must prune exactly the removed child's manifest entry"
    );
    let status_a = application
        .todo_child_status(None, Some(&child_a.identity.child_session_id))
        .await
        .expect("sibling status after remove");
    assert_eq!(status_a.receipts.len(), 1, "sibling receipts must be untouched");
    let routed = application
        .todo_child_send(None, None, "route by selection".into())
        .await
        .expect("send with no explicit id after non-selected removal");
    assert_eq!(
        routed.child_session_id,
        child_a.identity.child_session_id,
        "removing a non-selected child must preserve the selection"
    );

    let manifest_before = std::fs::read(&manifest_path).expect("read manifest");
    application
        .todo_child_remove(None, "00000000-0000-4000-8000-00000000000f")
        .await
        .expect("removing an absent child is an idempotent success");
    assert_eq!(
        std::fs::read(&manifest_path).expect("read manifest after absent removal"),
        manifest_before,
        "absent removal must not rewrite the durable manifest"
    );

    application
        .todo_child_remove(None, &child_a.identity.child_session_id)
        .await
        .expect("repeat removal is an idempotent success");

    let fallback = application
        .todo_child_send(None, None, "route after selected removal".into())
        .await
        .expect("send with no explicit id after selected removal");
    assert_eq!(
        fallback.child_session_id,
        child_c.identity.child_session_id,
        "removing the selected child must fall back to the sole active sibling"
    );
    application
        .todo_child_select(None, &child_a.identity.child_session_id)
        .await
        .expect_err("selecting a removed child must fail");

    application
        .todo_child_remove(None, &child_c.identity.child_session_id)
        .await
        .expect("remove the last remaining child");
    assert!(
        application
            .todo_child_list(None)
            .await
            .expect("list after removing all")
            .is_empty()
    );
    assert!(
        application.todo_child_status(None, None).await.is_err(),
        "no selected child and no sole active child must fail bare status"
    );
    for child in [
        &child_a.identity,
        &child_b.identity,
        &child_c.identity,
    ] {
        assert_no_child_artifacts(
            &child.parent_session_id,
            &child.child_session_id,
        );
    }

    _service.shutdown().await.expect("shutdown");
    application.cleanup().await;
    registration.unregister();
}

/// Removing a running child cancels and awaits its teardown before pruning.
#[tokio::test]
async fn removing_running_child_cancels_awaits_and_prunes_durable_state() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (model, registration) = faux_model();
    let controlled = ControlledStream::new();
    let (session, recorder) = recorded_parent_with_stream(
        artifacts.path(),
        "parent-remove-running",
        &model,
        Some(controlled.stream_fn()),
    )
    .await;
    let (application, _service) = application_with_factory(session).await;
    let child = application
        .todo_child_create(TodoChildCreateRequest {
            label: Some("running-remove".into()),
            idempotency_key: Some("key-running-remove".into()),
        })
        .await
        .expect("create child");
    let gate = controlled.arm();
    let child_session_id = child.identity.child_session_id.clone();
    let sent = tokio::spawn({
        let application = application.clone();
        async move {
            application
                .todo_child_send(None, Some(&child_session_id), "long work".into())
                .await
        }
    });
    wait_child_status(&application, &child.identity.child_session_id, TodoChildStatus::Running, 5).await;
    let receipt = sent
        .await
        .expect("send task")
        .expect("queued send returns its receipt while the turn runs");
    assert_eq!(receipt.child_session_id, child.identity.child_session_id);
    let running_snapshot = application
        .todo_child_status(None, Some(&child.identity.child_session_id))
        .await
        .expect("running status");
    assert_eq!(running_snapshot.status, TodoChildStatus::Running);
    assert_eq!(
        running_snapshot
            .receipts
            .iter()
            .filter(|receipt| receipt.status == TodoChildReceiptStatus::Running)
            .map(|receipt| receipt.id.as_str())
            .collect::<Vec<_>>(),
        vec![receipt.id.as_str()],
        "exactly the in-flight send receipt must be Running before removal"
    );

    application
        .todo_child_remove(None, &child.identity.child_session_id)
        .await
        .expect("remove a running child cancels and awaits its teardown");
    gate.cancel();
    assert!(
        application
            .todo_child_list(None)
            .await
            .expect("list after running removal")
            .is_empty()
    );
    assert!(
        application
            .todo_child_status(None, Some(&child.identity.child_session_id))
            .await
            .is_err(),
        "removed child must not be recoverable in the same process"
    );
    assert_no_child_artifacts(
        &child.identity.parent_session_id,
        &child.identity.child_session_id,
    );
    _service.shutdown().await.expect("shutdown");
    application.cleanup().await;

    let session = Session::new_with_additional_tools_filtered_and_discovery(
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
            stream_fn: None,
            auth_resolver: None,
            auth_manager: None,
        },
        Vec::new(),
        Default::default(),
        ResourceDiscovery::Disabled,
    )
    .expect("restart session");
    session.set_session_dir(test_sessions_root());
    session.record(recorder).expect("reuse parent recorder");
    attach_resources(&session, artifacts.path()).await;
    let (restarted, _restart_service) = application_with_factory(session).await;
    assert!(
        restarted
            .todo_child_list(None)
            .await
            .expect("list after restart")
            .is_empty()
    );
    assert!(
        restarted
            .todo_child_status(None, Some(&child.identity.child_session_id))
            .await
            .is_err(),
        "removal must prevent restart recovery"
    );

    _restart_service.shutdown().await.expect("shutdown restart");
    restarted.cleanup().await;
    registration.unregister();
}

/// Faulted manifest persistence leaves manifest entry, payload, and selection
/// intact; a disarmed retry removes the child. Needs `--features test-support`.
#[cfg(feature = "test-support")]
#[tokio::test]
async fn manifest_persist_fault_preserves_payload_selection_and_manifest() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (model, registration) = faux_model();
    set_child_responses(&registration, 4);
    let (session, _recorder) =
        recorded_parent(artifacts.path(), "parent-remove-fault", &model).await;
    let (application, service) = application_with_factory(session).await;
    let child = application
        .todo_child_create(TodoChildCreateRequest {
            label: Some("faulted-remove".into()),
            idempotency_key: Some("key-faulted-remove".into()),
        })
        .await
        .expect("create child");
    application
        .todo_child_send(
            None,
            Some(&child.identity.child_session_id),
            "durable work".into(),
        )
        .await
        .expect("send to create durable receipts");
    let child_root = child_artifact_root(&child.identity.parent_session_id);
    let transcript_path = child_root.join(format!(
        "{}.jsonl",
        child.identity.child_session_id
    ));
    assert!(transcript_path.try_exists().expect("stat transcript"));
    let orchestration_path = child_root
        .join("children")
        .join(&child.identity.child_session_id);
    std::fs::create_dir_all(&orchestration_path).expect("seed orchestration state");
    std::fs::write(
        orchestration_path.join("orchestration-state.json"),
        "{\"seed\":true}\n",
    )
    .expect("seed orchestration state");

    service.inject_prune_manifest_remove_fault_for_test();
    let faulted = application
        .todo_child_remove(None, &child.identity.child_session_id)
        .await;
    assert!(faulted.is_err(), "injected manifest persist fault must fail the removal");
    let listed = application
        .todo_child_list(None)
        .await
        .expect("list after faulted removal");
    assert_eq!(
        listed
            .iter()
            .map(|entry| entry.child_session_id.as_str())
            .collect::<Vec<_>>(),
        vec![child.identity.child_session_id.as_str()],
        "manifest entry must survive the faulted removal"
    );
    assert!(
        transcript_path.try_exists().expect("stat transcript"),
        "faulted manifest persistence must not delete the transcript"
    );
    assert!(
        orchestration_path.try_exists().expect("stat orchestration state"),
        "faulted manifest persistence must not delete orchestration state"
    );
    let status_after_fault = application
        .todo_child_status(None, Some(&child.identity.child_session_id))
        .await
        .expect("child must stay addressable after faulted removal");
    assert_eq!(
        status_after_fault.child_session_id,
        child.identity.child_session_id
    );
    let selected_after_fault = application
        .todo_child_status(None, None)
        .await
        .expect("selection must survive the faulted removal");
    assert_eq!(
        selected_after_fault.child_session_id,
        child.identity.child_session_id,
        "removal failure must not mutate the selection"
    );

    application
        .todo_child_remove(None, &child.identity.child_session_id)
        .await
        .expect("disarmed retry removes the child");
    assert!(
        application
            .todo_child_list(None)
            .await
            .expect("list after retry")
            .is_empty()
    );
    assert_no_child_artifacts(
        &child.identity.parent_session_id,
        &child.identity.child_session_id,
    );

    service.shutdown().await.expect("shutdown");
    application.cleanup().await;
    registration.unregister();
}

/// Faulted finalize commits the removal (manifest gone, tombstones staged); retry cleans.
#[cfg(feature = "test-support")]
#[tokio::test]
async fn payload_finalize_fault_commits_removal_and_leaves_retry_cleanable_tombstones() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (model, registration) = faux_model();
    set_child_responses(&registration, 4);
    let (session, _recorder) =
        recorded_parent(artifacts.path(), "parent-finalize-fault", &model).await;
    let (application, service) = application_with_factory(session).await;
    let child = application
        .todo_child_create(TodoChildCreateRequest {
            label: Some("finalize-fault".into()),
            idempotency_key: Some("key-finalize-fault".into()),
        })
        .await
        .expect("create child");
    application
        .todo_child_send(
            None,
            Some(&child.identity.child_session_id),
            "durable work".into(),
        )
        .await
        .expect("send to create durable receipts");
    let child_root = child_artifact_root(&child.identity.parent_session_id);
    let transcript_path = child_root.join(format!(
        "{}.jsonl",
        child.identity.child_session_id
    ));
    let orchestration_path = child_root
        .join("children")
        .join(&child.identity.child_session_id);
    std::fs::create_dir_all(&orchestration_path).expect("seed orchestration state");
    std::fs::write(
        orchestration_path.join("orchestration-state.json"),
        "{\"seed\":true}\n",
    )
    .expect("seed orchestration state");
    let staging_dir = child_root.join(".prune-staging");

    service.inject_prune_payload_finalize_fault_for_test();
    let faulted = application
        .todo_child_remove(None, &child.identity.child_session_id)
        .await;
    let error_text = format!("{:#}", faulted.expect_err("finalize fault must fail the removal"));
    assert!(
        error_text.contains("injected Todo child payload finalize fault"),
        "{error_text}"
    );
    assert!(
        !transcript_path.try_exists().expect("stat transcript"),
        "post-commit finalize failure must not restore the transcript"
    );
    assert!(
        !orchestration_path.try_exists().expect("stat orchestration state"),
        "post-commit finalize failure must not restore the orchestration state"
    );
    assert!(
        staging_dir
            .join(format!("{}.jsonl", child.identity.child_session_id))
            .try_exists()
            .expect("stat staged transcript"),
        "staged transcript must remain as a retry-cleanable tombstone"
    );
    assert!(
        staging_dir
            .join(&child.identity.child_session_id)
            .try_exists()
            .expect("stat staged orchestration state"),
        "staged orchestration state must remain as a retry-cleanable tombstone"
    );
    assert!(
        application
            .todo_child_list(None)
            .await
            .expect("list after committed faulted finalize")
            .is_empty(),
        "manifest entry must stay absent once removal committed"
    );
    assert!(
        application
            .todo_child_status(None, Some(&child.identity.child_session_id))
            .await
            .is_err(),
        "committed removal must leave the child unaddressable"
    );
    assert_eq!(
        service.init_slot_count_for_test(),
        0,
        "committed faulted removal releases the child's init slot"
    );

    application
        .todo_child_remove(None, &child.identity.child_session_id)
        .await
        .expect("disarmed retry finalizes the staged tombstones");
    assert!(
        application
            .todo_child_list(None)
            .await
            .expect("list after retry")
            .is_empty()
    );

    assert_no_child_artifacts(
        &child.identity.parent_session_id,
        &child.identity.child_session_id,
    );
    assert_eq!(
        service.init_slot_count_for_test(),
        0,
        "successful removal must release the child's init slot"
    );

    service.shutdown().await.expect("shutdown");
    application.cleanup().await;
    registration.unregister();
}

/// Repeated finalize failure keeps the explicit recoverable state; clean retry prunes.
#[cfg(feature = "test-support")]
#[tokio::test]
async fn double_prune_failure_leaves_recoverable_state_and_retry_prunes() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (model, registration) = faux_model();
    set_child_responses(&registration, 4);
    let (session, _recorder) =
        recorded_parent(artifacts.path(), "parent-double-fault", &model).await;
    let (application, service) = application_with_factory(session).await;
    let child = application
        .todo_child_create(TodoChildCreateRequest {
            label: Some("double-fault".into()),
            idempotency_key: Some("key-double-fault".into()),
        })
        .await
        .expect("create child");
    application
        .todo_child_send(
            None,
            Some(&child.identity.child_session_id),
            "durable work".into(),
        )
        .await
        .expect("send to create durable receipts");
    let child_root = child_artifact_root(&child.identity.parent_session_id);
    let transcript_path = child_root.join(format!(
        "{}.jsonl",
        child.identity.child_session_id
    ));
    let orchestration_path = child_root
        .join("children")
        .join(&child.identity.child_session_id);
    std::fs::create_dir_all(&orchestration_path).expect("seed orchestration state");
    std::fs::write(
        orchestration_path.join("orchestration-state.json"),
        "{\"seed\":true}\n",
    )
    .expect("seed orchestration state");

    service.inject_prune_payload_finalize_fault_for_test();
    service.inject_prune_payload_finalize_fault_for_test();
    let first = application
        .todo_child_remove(None, &child.identity.child_session_id)
        .await;
    let first_error = format!(
        "{:#}",
        first.expect_err("first finalize fault must fail the removal")
    );
    assert!(
        first_error.contains("injected Todo child payload finalize fault"),
        "{first_error}"
    );
    let staging_dir = child_root.join(".prune-staging");
    assert!(
        !transcript_path.try_exists().expect("stat transcript")
            && !orchestration_path.try_exists().expect("stat orchestration state"),
        "committed removal must not resurrect the payload"
    );
    assert!(
        staging_dir
            .join(format!("{}.jsonl", child.identity.child_session_id))
            .try_exists()
            .expect("stat staged transcript"),
        "first failure leaves the staged transcript tombstone"
    );
    let second = application
        .todo_child_remove(None, &child.identity.child_session_id)
        .await;
    let second_error = format!(
        "{:#}",
        second.expect_err("second finalize fault must fail too")
    );
    assert!(
        second_error.contains("injected Todo child payload finalize fault"),
        "{second_error}"
    );
    assert!(
        staging_dir
            .join(format!("{}.jsonl", child.identity.child_session_id))
            .try_exists()
            .expect("stat staged transcript"),
        "double failure keeps the staged tombstones recoverable"
    );
    assert!(
        application
            .todo_child_list(None)
            .await
            .expect("list after double fault")
            .is_empty(),
        "double failure keeps the manifest entry absent — the explicit recoverable state"
    );
    let manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(child_root.join("todo-manifest.json")).expect("read manifest"),
    )
    .expect("decode manifest");
    assert!(
        manifest["entries"].as_array().expect("entries").is_empty(),
        "double failure leaves the manifest entry absent — the explicit recoverable state: {manifest}"
    );

    application
        .todo_child_remove(None, &child.identity.child_session_id)
        .await
        .expect("disarmed retry prunes the recoverable state");
    assert!(
        application
            .todo_child_list(None)
            .await
            .expect("list after retry")
            .is_empty()
    );
    assert_no_child_artifacts(
        &child.identity.parent_session_id,
        &child.identity.child_session_id,
    );
    assert_eq!(service.init_slot_count_for_test(), 0);

    service.shutdown().await.expect("shutdown");
    application.cleanup().await;
    registration.unregister();
}

/// Absent removals leave zero init slots, zero disk changes, and an untouched manifest.
#[cfg(feature = "test-support")]
#[tokio::test]
async fn absent_remove_has_no_slot_or_disk_side_effects() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (model, registration) = faux_model();
    let (session, recorder) =
        recorded_parent(artifacts.path(), "parent-absent-remove", &model).await;
    let (application, service) = application_with_factory(session).await;
    let absent_id = idempotent_child_id(&recorder.id(), "never-created");

    application
        .todo_child_remove(None, &absent_id)
        .await
        .expect("absent removal is an idempotent success");
    assert!(!child_artifact_root(&recorder.id()).try_exists().expect("stat child root"));
    assert_eq!(
        service.init_slot_count_for_test(),
        0,
        "absent removal must not register an init slot"
    );

    let child = application
        .todo_child_create(TodoChildCreateRequest {
            label: Some("sibling".into()),
            idempotency_key: Some("key-absent-sibling".into()),
        })
        .await
        .expect("create sibling");
    assert_eq!(
        service.init_slot_count_for_test(),
        0,
        "settled creation must release its init slot"
    );
    let manifest_path = child_artifact_root(&recorder.id()).join("todo-manifest.json");
    let manifest_before = std::fs::read(&manifest_path).expect("read manifest");

    application
        .todo_child_remove(None, &idempotent_child_id(&recorder.id(), "also-absent"))
        .await
        .expect("absent removal with a present sibling");
    assert_eq!(
        std::fs::read(&manifest_path).expect("read manifest after absent removal"),
        manifest_before,
        "absent removal must not rewrite the durable manifest"
    );
    assert!(
        !child_artifact_root(&recorder.id())
            .join(".prune-staging")
            .try_exists()
            .expect("stat staging dir"),
        "absent removal must not create the prune staging directory"
    );
    assert_eq!(
        application
            .todo_child_list(None)
            .await
            .expect("list")
            .iter()
            .map(|entry| entry.child_session_id.as_str())
            .collect::<Vec<_>>(),
        vec![child.identity.child_session_id.as_str()],
    );
    assert_eq!(service.init_slot_count_for_test(), 0);

    service.shutdown().await.expect("shutdown");
    application.cleanup().await;
    registration.unregister();
}

/// Staged tombstones from a crashed attempt are recognized and pruned by the next remove.
#[cfg(feature = "test-support")]
#[tokio::test]
async fn remove_prunes_staged_tombstones_left_by_a_crashed_attempt() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (model, registration) = faux_model();
    let (session, recorder) =
        recorded_parent(artifacts.path(), "parent-tombstone", &model).await;
    let (application, service) = application_with_factory(session).await;
    let child = application
        .todo_child_create(TodoChildCreateRequest {
            label: Some("tombstone".into()),
            idempotency_key: Some("key-tombstone".into()),
        })
        .await
        .expect("create child");
    let child_id = child.identity.child_session_id.clone();
    let parent_id = child.identity.parent_session_id.clone();
    let child_root = child_artifact_root(&parent_id);
    let staging_dir = child_root.join(".prune-staging");

    std::fs::create_dir_all(staging_dir.join(&child_id)).expect("seed staged tombstone");
    std::fs::write(
        staging_dir.join(&child_id).join("orchestration-state.json"),
        "{\"seed\":true}\n",
    )
    .expect("seed staged orchestration state");
    std::fs::remove_file(child_root.join(format!("{child_id}.jsonl")))
        .expect("simulate finalized transcript");
    let manifest_path = child_root.join("todo-manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).expect("read manifest"))
            .expect("decode manifest");
    manifest["entries"] = serde_json::Value::Array(
        manifest["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .filter(|entry| entry["childSessionId"] != serde_json::json!(child_id))
            .cloned()
            .collect(),
    );
    std::fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest).expect("encode manifest"))
        .expect("rewrite manifest without the entry");

    application
        .todo_child_remove(None, &child_id)
        .await
        .expect("remove finalizes the crashed attempt's staged tombstones");
    assert!(
        application
            .todo_child_list(None)
            .await
            .expect("list after tombstone prune")
            .is_empty()
    );
    assert_no_child_artifacts(&parent_id, &child_id);
    assert_eq!(service.init_slot_count_for_test(), 0);

    service.shutdown().await.expect("shutdown");
    application.cleanup().await;
    registration.unregister();
}

/// Remove waits for a parked pre-publication create, then prunes it.
#[cfg(feature = "test-support")]
#[tokio::test]
async fn remove_racing_in_progress_create_waits_then_prunes() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (model, registration) = faux_model();
    let (session, recorder) =
        recorded_parent(artifacts.path(), "parent-remove-create-race", &model).await;
    let child_id = idempotent_child_id(&recorder.id(), "remove-create-race");
    let service = TodoChildService::new();
    service
        .configure_factory(Arc::new(InjectionRuntimeFactory::new()))
        .expect("configure working factory");
    let gate = service.arm_creation_before_publication_gate_for_test();
    let application = application_with_service(session, &service).await;

    let creator = tokio::spawn({
        let application = application.clone();
        async move {
            application
                .todo_child_create(TodoChildCreateRequest {
                    label: Some("race".into()),
                    idempotency_key: Some("remove-create-race".into()),
                })
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(5), gate.wait_entered())
        .await
        .expect("creator reached the pre-publication gate");

    let remover = tokio::spawn({
        let application = application.clone();
        let child_id = child_id.clone();
        async move { application.todo_child_remove(None, &child_id).await }
    });
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    gate.release();

    let created = tokio::time::timeout(Duration::from_secs(5), creator)
        .await
        .expect("creator settles after gate release")
        .expect("creator task")
        .expect("create succeeds");
    assert_eq!(created.identity.child_session_id, child_id);
    tokio::time::timeout(Duration::from_secs(5), remover)
        .await
        .expect("remove settles after the creator publishes")
        .expect("remove task")
        .expect("remove of the raced create prunes it");
    assert_eq!(
        service.creation_ticket_count_for_test(),
        0,
        "settled creation must leave no ticket behind"
    );
    assert!(
        application
            .todo_child_list(None)
            .await
            .expect("list after raced removal")
            .is_empty()
    );
    assert!(
        application.todo_child_status(None, Some(&child_id)).await.is_err(),
        "raced create must be pruned, not resurrected"
    );
    assert_no_child_artifacts(&recorder.id(), &child_id);

    service.shutdown().await.expect("shutdown");
    application.cleanup().await;
    registration.unregister();
}

// ---------------------------------------------------------------------------
// Service-backed integration tests: task DAG, revision CAS, lifecycle
// rejection, and crash-restart recovery — all against the frozen API.
// ---------------------------------------------------------------------------

/// Parent `Application::cleanup()` does not stop an active Todo child. A
/// separate Application sharing the same service can still status, send, and
/// add tasks to the child after the original parent is gone.
#[tokio::test]
async fn parent_cleanup_does_not_stop_active_todo() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (model, registration) = faux_model();

    let controlled = ControlledStream::new();
    let (session, recorder) = recorded_parent_with_stream(
        artifacts.path(),
        "parent-cleanup-survival",
        &model,
        Some(controlled.stream_fn()),
    )
    .await;
    let (service, _factory) = shared_service();
    let application = application_with_service(session, &service).await;

    let child = application
        .todo_child_create(TodoChildCreateRequest::default())
        .await
        .expect("create child");
    let child_id = child.identity.child_session_id.clone();

    // Arm a gate and add a task so the child is actively Running.
    let gate = controlled.arm();
    let add = application
        .todo_child_add_task(
            None,
            &child_id,
            TodoChildAddTaskRequest {
                content: "task A".into(),
                ..Default::default()
            },
        )
        .await
        .expect("add task A");
    wait_task_status(
        &application,
        &child_id,
        &add.task.id,
        TodoChildTaskStatus::Running,
        5,
    )
    .await;

    // Parent cleanup must NOT stop the Todo child (owned by the service).
    application.cleanup().await;
    drop(application);

    // A fresh parent Application sharing the same recorder and service can
    // still observe and interact with the active child.
    let session_b = Session::new_with_additional_tools_filtered_and_discovery(
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
            stream_fn: None,
            auth_resolver: None,
            auth_manager: None,
        },
        Vec::new(),
        Default::default(),
        ResourceDiscovery::Disabled,
    )
    .expect("restart session");
    session_b.set_session_dir(test_sessions_root());
    session_b.record(recorder).expect("record B");
    attach_resources(&session_b, artifacts.path()).await;
    let app_b = application_with_service(session_b, &service).await;

    let status = app_b
        .todo_child_status(None, Some(&child_id))
        .await
        .expect("status after parent cleanup");
    assert_ne!(
        status.status,
        TodoChildStatus::Stopped,
        "parent cleanup must not stop the active Todo child"
    );

    // Send and add_task must still work from the new application.
    let receipt = app_b
        .todo_child_send(None, Some(&child_id), "still alive".into())
        .await
        .expect("send after parent cleanup");
    assert_eq!(receipt.child_session_id, child_id);

    let add_b = app_b
        .todo_child_add_task(
            None,
            &child_id,
            TodoChildAddTaskRequest {
                content: "task B".into(),
                ..Default::default()
            },
        )
        .await
        .expect("add task after parent cleanup");
    assert!(!add_b.task.id.is_empty());

    // Release the gate so the child can settle before shutdown.
    gate.cancel();
    wait_child_idle(&app_b, &child_id, 5).await;

    service.shutdown().await.expect("shutdown service");
    app_b.cleanup().await;
    registration.unregister();
}

/// Adding a task while another task is Running appends durably and the
/// dependency-ready scheduler continues: the dependent task starts after
/// the running task completes.
#[tokio::test]
async fn add_task_while_running_schedules_dependent() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (model, registration) = faux_model();

    let controlled = ControlledStream::new();
    let (session, _recorder) = recorded_parent_with_stream(
        artifacts.path(),
        "parent-add-while-running",
        &model,
        Some(controlled.stream_fn()),
    )
    .await;
    let (service, _factory) = shared_service();
    let application = application_with_service(session, &service).await;

    let child = application
        .todo_child_create(TodoChildCreateRequest::default())
        .await
        .expect("create child");
    let child_id = child.identity.child_session_id.clone();

    // Add task A (no deps) — it starts executing immediately and blocks.
    let gate_a = controlled.arm();
    let add_a = application
        .todo_child_add_task(
            None,
            &child_id,
            TodoChildAddTaskRequest {
                content: "task A".into(),
                ..Default::default()
            },
        )
        .await
        .expect("add task A");
    let task_a_id = add_a.task.id.clone();
    wait_task_status(
        &application,
        &child_id,
        &task_a_id,
        TodoChildTaskStatus::Running,
        5,
    )
    .await;

    // While A is Running, add task B depending on A.
    let gate_b = controlled.arm();
    let add_b = application
        .todo_child_add_task(
            None,
            &child_id,
            TodoChildAddTaskRequest {
                content: "task B".into(),
                depends_on: vec![task_a_id.clone()],
                ..Default::default()
            },
        )
        .await
        .expect("add task B");
    assert_eq!(
        add_b.task.status,
        TodoChildTaskStatus::Pending,
        "dependent task must be Pending while its dependency is Running"
    );
    assert_eq!(add_b.task.depends_on, vec![task_a_id.clone()]);

    // Release A — A completes, then B should start (dependency-ready scheduling).
    gate_a.cancel();
    wait_task_status(
        &application,
        &child_id,
        &task_a_id,
        TodoChildTaskStatus::Completed,
        5,
    )
    .await;

    // B should become Running (its dependency A is now Completed).
    wait_task_status(
        &application,
        &child_id,
        &add_b.task.id,
        TodoChildTaskStatus::Running,
        5,
    )
    .await;

    // Release B — B completes.
    gate_b.cancel();
    wait_task_status(
        &application,
        &child_id,
        &add_b.task.id,
        TodoChildTaskStatus::Completed,
        5,
    )
    .await;

    service.shutdown().await.expect("shutdown");
    application.cleanup().await;
    registration.unregister();
}

/// Adding a task after all tasks are completed/idle reactivates the Todo
/// child: the child transitions from Idle back to Running.
#[tokio::test]
async fn add_task_after_idle_reactivates() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (model, registration) = faux_model();

    let controlled = ControlledStream::new();
    let (session, _recorder) = recorded_parent_with_stream(
        artifacts.path(),
        "parent-reactivate-after-idle",
        &model,
        Some(controlled.stream_fn()),
    )
    .await;
    let (service, _factory) = shared_service();
    let application = application_with_service(session, &service).await;

    let child = application
        .todo_child_create(TodoChildCreateRequest::default())
        .await
        .expect("create child");
    let child_id = child.identity.child_session_id.clone();

    // Add task A with no gate — it completes immediately.
    let add_a = application
        .todo_child_add_task(
            None,
            &child_id,
            TodoChildAddTaskRequest {
                content: "task A".into(),
                ..Default::default()
            },
        )
        .await
        .expect("add task A");
    wait_task_status(
        &application,
        &child_id,
        &add_a.task.id,
        TodoChildTaskStatus::Completed,
        5,
    )
    .await;

    // Child must be Idle after A completes.
    wait_child_status(&application, &child_id, TodoChildStatus::Idle, 5).await;

    // Arm a gate for B so we can observe the Running state before it completes.
    let gate_b = controlled.arm();
    let add_b = application
        .todo_child_add_task(
            None,
            &child_id,
            TodoChildAddTaskRequest {
                content: "task B".into(),
                ..Default::default()
            },
        )
        .await
        .expect("add task B");

    // Child must be Running (reactivated from Idle).
    wait_child_status(&application, &child_id, TodoChildStatus::Running, 5).await;
    wait_task_status(
        &application,
        &child_id,
        &add_b.task.id,
        TodoChildTaskStatus::Running,
        5,
    )
    .await;

    // Release B — B completes.
    gate_b.cancel();
    wait_task_status(
        &application,
        &child_id,
        &add_b.task.id,
        TodoChildTaskStatus::Completed,
        5,
    )
    .await;

    service.shutdown().await.expect("shutdown");
    application.cleanup().await;
    registration.unregister();
}

/// Concurrent `add_task` calls with the same `expected_revision`: exactly
/// one commits (optimistic CAS), the other is rejected with a stale revision
/// error. No lost update — the DAG has exactly the seed plus the winner.
#[tokio::test]
async fn concurrent_add_same_revision_one_commits() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (model, registration) = faux_model();

    let controlled = ControlledStream::new();
    let (session, _recorder) = recorded_parent_with_stream(
        artifacts.path(),
        "parent-concurrent-add",
        &model,
        Some(controlled.stream_fn()),
    )
    .await;
    let (service, _factory) = shared_service();
    let application = application_with_service(session, &service).await;

    let child = application
        .todo_child_create(TodoChildCreateRequest::default())
        .await
        .expect("create child");
    let child_id = child.identity.child_session_id.clone();

    // Add a seed task and block its execution so the revision is stable.
    let gate = controlled.arm();
    let add_a = application
        .todo_child_add_task(
            None,
            &child_id,
            TodoChildAddTaskRequest {
                content: "seed".into(),
                ..Default::default()
            },
        )
        .await
        .expect("add seed");
    wait_task_status(
        &application,
        &child_id,
        &add_a.task.id,
        TodoChildTaskStatus::Running,
        5,
    )
    .await;

    // Read the stable revision (seed is blocked, no further events fire).
    let snapshot = application
        .todo_child_status(None, Some(&child_id))
        .await
        .expect("status");
    let base_revision = snapshot.revision;

    // Two concurrent adds with the same expected_revision.
    let app1 = application.clone();
    let child1 = child_id.clone();
    let task1 = tokio::spawn(async move {
        app1.todo_child_add_task(
            None,
            &child1,
            TodoChildAddTaskRequest {
                content: "concurrent-1".into(),
                expected_revision: Some(base_revision),
                ..Default::default()
            },
        )
        .await
    });
    let app2 = application.clone();
    let child2 = child_id.clone();
    let task2 = tokio::spawn(async move {
        app2.todo_child_add_task(
            None,
            &child2,
            TodoChildAddTaskRequest {
                content: "concurrent-2".into(),
                expected_revision: Some(base_revision),
                ..Default::default()
            },
        )
        .await
    });

    let result1 = task1.await.expect("task1 panicked");
    let result2 = task2.await.expect("task2 panicked");

    // Exactly one must succeed, the other must fail (stale revision).
    let (winner, loser_content) = match (&result1, &result2) {
        (Ok(r), Err(_)) => (r.clone(), "concurrent-2"),
        (Err(_), Ok(r)) => (r.clone(), "concurrent-1"),
        (Ok(_), Ok(_)) => {
            panic!("both concurrent adds succeeded — expected CAS rejection")
        }
        (Err(_), Err(_)) => {
            panic!("both concurrent adds failed — at least one must commit")
        }
    };

    assert!(
        winner.revision > base_revision,
        "winner must advance revision beyond base"
    );
    assert!(!winner.task.id.is_empty());

    // Snapshot must have exactly 2 tasks: the seed plus the winner.
    let status = application
        .todo_child_status(None, Some(&child_id))
        .await
        .expect("status");
    assert_eq!(
        status.tasks.len(),
        2,
        "DAG must have exactly 2 tasks (seed + winner), got {}: {:?}",
        status.tasks.len(),
        status.tasks
    );
    assert!(
        status.tasks.iter().any(|t| t.id == add_a.task.id),
        "seed task must be present"
    );
    assert!(
        status.tasks.iter().any(|t| t.id == winner.task.id),
        "winner task must be present"
    );
    assert!(
        !status.tasks.iter().any(|t| t.content == loser_content),
        "loser task must not be in the DAG"
    );

    // Release the seed gate so it can settle before cleanup.
    gate.cancel();
    wait_child_idle(&application, &child_id, 5).await;

    service.shutdown().await.expect("shutdown");
    application.cleanup().await;
    registration.unregister();
}

/// Stale `expected_revision` rejects `add_task` without changing the DAG.
/// Stopped and archived Todo children also reject `add_task`.
#[tokio::test]
async fn stale_revision_and_stopped_archived_reject_add_task() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (model, registration) = faux_model();

    let controlled = ControlledStream::new();
    let (session, _recorder) = recorded_parent_with_stream(
        artifacts.path(),
        "parent-stale-revision",
        &model,
        Some(controlled.stream_fn()),
    )
    .await;
    let (service, _factory) = shared_service();
    let application = application_with_service(session, &service).await;

    let child = application
        .todo_child_create(TodoChildCreateRequest::default())
        .await
        .expect("create child");
    let child_id = child.identity.child_session_id.clone();

    // Block execution so the revision remains stable after the Started event.
    let gate_a = controlled.arm();
    let add_a = application
        .todo_child_add_task(
            None,
            &child_id,
            TodoChildAddTaskRequest {
                content: "task A".into(),
                ..Default::default()
            },
        )
        .await
        .expect("add A");
    wait_task_status(
        &application,
        &child_id,
        &add_a.task.id,
        TodoChildTaskStatus::Running,
        5,
    )
    .await;

    // Read the stable revision after A started (Started event persisted).
    let snapshot = application
        .todo_child_status(None, Some(&child_id))
        .await
        .expect("status");
    let stable_rev = snapshot.revision;

    let gate_b = controlled.arm();
    let add_b = application
        .todo_child_add_task(
            None,
            &child_id,
            TodoChildAddTaskRequest {
                content: "task B".into(),
                expected_revision: Some(stable_rev),
                ..Default::default()
            },
        )
        .await
        .expect("add B with correct revision");
    assert!(add_b.revision > stable_rev);

    // Stale expected_revision must reject without changing the DAG.
    let stale = application
        .todo_child_add_task(
            None,
            &child_id,
            TodoChildAddTaskRequest {
                content: "task C (stale)".into(),
                expected_revision: Some(stable_rev),
                ..Default::default()
            },
        )
        .await;
    assert!(
        stale.is_err(),
        "stale expected_revision must reject add_task"
    );

    let status = application
        .todo_child_status(None, Some(&child_id))
        .await
        .expect("status after stale reject");
    assert_eq!(
        status.tasks.len(),
        2,
        "DAG must be unchanged after stale reject"
    );
    assert!(
        status.tasks.iter().all(|t| t.content != "task C (stale)"),
        "stale task must not be in the DAG"
    );

    gate_a.cancel();
    gate_b.cancel();
    wait_task_status(
        &application,
        &child_id,
        &add_a.task.id,
        TodoChildTaskStatus::Completed,
        5,
    )
    .await;
    wait_task_status(
        &application,
        &child_id,
        &add_b.task.id,
        TodoChildTaskStatus::Completed,
        5,
    )
    .await;

    // Stop the child — add_task must reject.
    application
        .todo_child_stop(None, Some(&child_id))
        .await
        .expect("stop child");
    let stopped_add = application
        .todo_child_add_task(
            None,
            &child_id,
            TodoChildAddTaskRequest {
                content: "after stop".into(),
                ..Default::default()
            },
        )
        .await;
    assert!(stopped_add.is_err(), "stopped child must reject add_task");

    // Archive the child — add_task must reject (child no longer exists).
    application
        .todo_child_archive(None, &child_id)
        .await
        .expect("archive child");
    let archived_add = application
        .todo_child_add_task(
            None,
            &child_id,
            TodoChildAddTaskRequest {
                content: "after archive".into(),
                ..Default::default()
            },
        )
        .await;
    assert!(archived_add.is_err(), "archived child must reject add_task");

    service.shutdown().await.expect("shutdown");
    application.cleanup().await;
    registration.unregister();
}

/// Restart recovery preserves task ids and dependencies, durably records the
/// recovery transition, and maps a Running task to Interrupted without
/// automatically executing it again.
#[tokio::test]
async fn restart_recovery_preserves_tasks_maps_running_to_interrupted() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (model, registration) = faux_model();

    let controlled = ControlledStream::new();
    let (session, recorder) = recorded_parent_with_stream(
        artifacts.path(),
        "parent-crash-recovery",
        &model,
        Some(controlled.stream_fn()),
    )
    .await;
    let (service, _factory) = shared_service();
    let application = application_with_service(session, &service).await;

    let child = application
        .todo_child_create(TodoChildCreateRequest::default())
        .await
        .expect("create child");
    let child_id = child.identity.child_session_id.clone();

    // Add task A (no deps) and block it so it stays Running (Started persisted).
    let _gate_a = controlled.arm();
    let add_a = application
        .todo_child_add_task(
            None,
            &child_id,
            TodoChildAddTaskRequest {
                content: "task A".into(),
                ..Default::default()
            },
        )
        .await
        .expect("add task A");
    let task_a_id = add_a.task.id.clone();
    wait_task_status(
        &application,
        &child_id,
        &task_a_id,
        TodoChildTaskStatus::Running,
        5,
    )
    .await;

    // Add task B depending on A — B stays Pending while A is Running.
    let add_b = application
        .todo_child_add_task(
            None,
            &child_id,
            TodoChildAddTaskRequest {
                content: "task B".into(),
                depends_on: vec![task_a_id.clone()],
                ..Default::default()
            },
        )
        .await
        .expect("add task B");
    let task_b_id = add_b.task.id.clone();

    // Capture the revision before the crash.
    let pre_crash = application
        .todo_child_status(None, Some(&child_id))
        .await
        .expect("status before crash");
    let pre_crash_revision = pre_crash.revision;

    // Simulate a crash: drop without cleanup or shutdown. The transcript has
    // A's Started event but no terminal event, and B's Added event.
    drop(application);
    drop(service);
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }

    // Create a new service and fresh parent sharing the same recorder.
    let (new_service, _new_factory) = shared_service();
    let session_b = Session::new_with_additional_tools_filtered_and_discovery(
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
            stream_fn: None,
            auth_resolver: None,
            auth_manager: None,
        },
        Vec::new(),
        Default::default(),
        ResourceDiscovery::Disabled,
    )
    .expect("restart session");
    session_b.set_session_dir(test_sessions_root());
    session_b.record(recorder).expect("record B");
    attach_resources(&session_b, artifacts.path()).await;
    let app_b = application_with_service(session_b, &new_service).await;

    // Recover the child from the transcript.
    let recovered = app_b
        .todo_child_status(None, Some(&child_id))
        .await
        .expect("recovered status");

    // Task ids and dependencies are preserved, while recovery durably appends
    // exactly one Interrupted transition.
    assert_eq!(
        recovered.revision,
        pre_crash_revision + 1,
        "recovery must append one Interrupted task event"
    );
    assert_eq!(recovered.tasks.len(), 2, "both tasks must be recovered");

    let task_a = recovered
        .tasks
        .iter()
        .find(|t| t.id == task_a_id)
        .expect("task A recovered");
    assert_eq!(
        task_a.status,
        TodoChildTaskStatus::Interrupted,
        "durable Running task must be mapped to Interrupted on recovery"
    );

    let task_b = recovered
        .tasks
        .iter()
        .find(|t| t.id == task_b_id)
        .expect("task B recovered");
    assert_eq!(
        task_b.status,
        TodoChildTaskStatus::Pending,
        "non-started task must remain Pending"
    );
    assert_eq!(
        task_b.depends_on,
        vec![task_a_id.clone()],
        "dependencies must be preserved across restart"
    );

    // The recovered child must not auto-execute the Interrupted task.
    assert!(
        !recovered.is_running,
        "recovered child must not be running (no auto-execution)"
    );
    // Double-check after a brief wait that it stays idle.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let recovered2 = app_b
        .todo_child_status(None, Some(&child_id))
        .await
        .expect("status2");
    assert_eq!(
        recovered2.revision, recovered.revision,
        "subsequent reads must not append another recovery event"
    );
    assert!(
        !recovered2.is_running,
        "recovered child must remain idle (no auto-execution after delay)"
    );
    let task_a2 = recovered2
        .tasks
        .iter()
        .find(|t| t.id == task_a_id)
        .expect("task A still present");
    assert_eq!(
        task_a2.status,
        TodoChildTaskStatus::Interrupted,
        "A must remain Interrupted"
    );

    new_service.shutdown().await.expect("shutdown");
    app_b.cleanup().await;
    registration.unregister();
}

/// Captures the exact child model-request boundary across initial, later, and
/// recovered turns. Every request must retain the forked parent transcript,
/// exclude parent continuation recorded after the fork, and preserve the
/// product Todo-child prompt plus project AGENTS/persona context. The child
/// must also retain the fork-time registered model and retry fallback chain,
/// while request auth is re-resolved through the recovered parent resolver.
#[tokio::test]
async fn child_model_requests_preserve_fork_context_prompt_model_auth_and_fallback_after_restart() {
    const PRE_FORK: &str = "exact pre-fork parent marker 8c3241";
    const POST_FORK: &str = "exact post-fork parent marker f6750e";
    const AGENTS_RULE: &str = "AGENTS project rule 718d2a";
    const PERSONA_RULE: &str = "Persona rule 37a6bf: calm and concise.";
    const FIRST_TASK: &str = "capture first child request";
    const LATER_TASK: &str = "capture later child request";
    const RESTART_TASK: &str = "capture recovered child request";
    const FALLBACK_TASK: &str = "exercise inherited fallback";
    const PRIMARY_KEY: &str = "todo-primary-request-key";
    const RESTART_STATIC_KEY: &str = "wrong-static-restart-parent-key";
    const ROTATED_PRIMARY_KEY: &str = "rotated-recovered-primary-key";
    const FALLBACK_KEY: &str = "todo-fallback-request-key";
    const PARENT_ONLY_PROMPT: &str = "parent-only prompt must not replace Todo child prompt";
    const RESTART_PARENT_ONLY_PROMPT: &str = "restart parent-only prompt";

    let artifacts = tempfile::tempdir().expect("artifacts");
    std::fs::write(
        artifacts.path().join("AGENTS.md"),
        format!("{AGENTS_RULE}\n{PERSONA_RULE}\n"),
    )
    .expect("write AGENTS context");

    let (primary, registration) = faux_model();
    let fallback = Model {
        id: format!("{}-fallback", primary.id),
        name: "Todo Fork Fallback".into(),
        api: format!("{}-fallback", primary.api),
        provider: format!("{}-fallback", primary.provider),
        ..Model::default()
    };
    let fallback_registration = register_faux_provider(FauxProviderOptions {
        api: fallback.api.clone(),
        provider: fallback.provider.clone(),
        models: vec![fallback.clone()],
        chunk_size: 32,
    });
    let fallback = require_registered_model(&fallback.provider, &fallback.id);

    let requests = Arc::new(parking_lot::Mutex::new(Vec::<CapturedChildRequest>::new()));
    let captured_responses = Arc::new(parking_lot::Mutex::new(VecDeque::<FauxResponse>::new()));
    let auth_models = Arc::new(parking_lot::Mutex::new(Vec::<Model>::new()));
    let observed_auth = auth_models.clone();
    let primary_provider = primary.provider.clone();
    let fallback_provider = fallback.provider.clone();
    let auth_resolver: pi_coding::SessionAuthResolver = Arc::new(move |model: Model| {
        observed_auth.lock().push(model.clone());
        let primary_provider = primary_provider.clone();
        let fallback_provider = fallback_provider.clone();
        Box::pin(async move {
            let api_key = if model.provider == fallback_provider {
                FALLBACK_KEY
            } else if model.provider == primary_provider {
                PRIMARY_KEY
            } else {
                anyhow::bail!("unexpected auth model {}/{}", model.provider, model.id)
            };
            Ok(RequestAuth {
                api_key: api_key.to_owned(),
                kind: RequestAuthKind::ApiKeyOrHeaders,
                ..RequestAuth::default()
            })
        })
    });

    let session_dir = test_sessions_root();
    let recorder = session_store::start_session_in(
        artifacts.path(),
        Some(&primary),
        None,
        Some(&session_dir),
        Some("parent-request-inheritance"),
        None,
    )
    .expect("parent recorder");
    let session = Session::new_with_additional_tools_filtered_and_discovery(
        SessionOptions {
            model: primary.clone(),
            cwd: artifacts.path().to_path_buf(),
            system_prompt: PARENT_ONLY_PROMPT.into(),
            thinking_level: ThinkingLevel::Off,
            api_key: PRIMARY_KEY.into(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: Some(capturing_stream_with_responses(
                requests.clone(),
                captured_responses.clone(),
            )),
            auth_resolver: Some(auth_resolver.clone()),
            auth_manager: None,
        },
        Vec::new(),
        Default::default(),
        ResourceDiscovery::Disabled,
    )
    .expect("parent session");
    session.set_session_dir(session_dir);
    session.record(recorder.clone()).expect("record parent");
    attach_resources(&session, artifacts.path()).await;
    recorder
        .record_message(&Message::user_text(PRE_FORK, 10))
        .expect("record pre-fork message");
    let mut fallback_chains = RetryFallbackChains::default();
    fallback_chains.insert(
        "default".into(),
        vec![format!("{}/{}", fallback.provider, fallback.id)],
    );
    let fork_retry_settings = RetrySettings {
        enabled: true,
        max_retries: 0,
        base_delay_ms: 1,
        model_fallback: true,
        fallback_chains,
    };
    session.set_retry_settings(fork_retry_settings.clone());

    let factory: Arc<dyn ApplicationRuntimeFactory> =
        Arc::new(TodoChildTestFactory::with_context_files(true));
    let service = TodoChildService::new();
    service
        .configure_factory(factory.clone())
        .expect("configure service factory");
    let application = application_with_service(session, &service).await;
    application
        .session()
        .set_retry_settings(fork_retry_settings);
    let child = application
        .todo_child_create(TodoChildCreateRequest::default())
        .await
        .expect("create child");
    let child_id = child.identity.child_session_id.clone();
    let child_path = recorder
        .path()
        .parent()
        .expect("parent session directory")
        .join("children")
        .join(recorder.id())
        .join(format!("{child_id}.jsonl"));
    let child_tree = session_store::load_session_tree(&child_path).expect("load child derivation");
    let derivation = child_tree
        .branch(None)
        .into_iter()
        .find(|entry| entry.custom_type.as_deref() == Some("pi.todo_child.derivation"))
        .and_then(|entry| entry.data.as_ref())
        .expect("durable Todo child derivation");
    let durable_runtime = derivation["runtime"].clone();
    let durable_retry = durable_runtime
        .get("retrySettings")
        .or_else(|| durable_runtime.get("retry_settings"))
        .expect("durable retry settings");
    let durable_chains = durable_retry
        .get("fallbackChains")
        .or_else(|| durable_retry.get("fallback_chains"))
        .expect("durable fallback chains");
    assert_eq!(
        durable_chains["default"][0],
        format!("{}/{}", fallback.provider, fallback.id),
        "fork-time fallback chain must be durable"
    );
    assert_eq!(
        durable_retry
            .get("modelFallback")
            .or_else(|| durable_retry.get("model_fallback")),
        Some(&serde_json::Value::Bool(true))
    );
    assert_eq!(durable_runtime["modelProvider"], primary.provider);
    assert_eq!(durable_runtime["modelId"], primary.id);

    // Fork is fixed before this parent continuation. It must never reach the
    // child's first, later, or recovered request contexts.
    recorder
        .record_message(&Message::user_text(POST_FORK, 20))
        .expect("record post-fork message");

    let first = application
        .todo_child_add_task(
            None,
            &child_id,
            TodoChildAddTaskRequest {
                content: FIRST_TASK.into(),
                ..Default::default()
            },
        )
        .await
        .expect("add first task");
    wait_task_status(
        &application,
        &child_id,
        &first.task.id,
        TodoChildTaskStatus::Completed,
        5,
    )
    .await;

    let later = application
        .todo_child_add_task(
            None,
            &child_id,
            TodoChildAddTaskRequest {
                content: LATER_TASK.into(),
                ..Default::default()
            },
        )
        .await
        .expect("add later task");
    wait_task_status(
        &application,
        &child_id,
        &later.task.id,
        TodoChildTaskStatus::Completed,
        5,
    )
    .await;
    wait_captured_requests(&requests, 2).await;

    {
        let captured = requests.lock();
        assert_eq!(captured.len(), 2, "first + later child requests");
        for (index, request) in captured.iter().enumerate() {
            let text = request_context_text(&request.context);
            assert!(
                text.contains(PRE_FORK),
                "request {index} lost pre-fork context: {text}"
            );
            assert!(
                !text.contains(POST_FORK),
                "request {index} leaked post-fork context: {text}"
            );
            assert!(
                request.context.system_prompt.contains("Todo child"),
                "request {index} lost the product Todo-child prompt: {}",
                request.context.system_prompt,
            );
            assert!(
                !request.context.system_prompt.contains(PARENT_ONLY_PROMPT),
                "request {index} reused the parent prompt: {}",
                request.context.system_prompt,
            );
            assert!(
                request.context.system_prompt.contains(AGENTS_RULE),
                "request {index} lost AGENTS context: {}",
                request.context.system_prompt,
            );
            assert!(
                request.context.system_prompt.contains(PERSONA_RULE),
                "request {index} lost persona context: {}",
                request.context.system_prompt,
            );
            assert_eq!(request.model.provider, primary.provider);
            assert_eq!(request.model.id, primary.id);
            assert_eq!(request.options.stream.api_key.as_deref(), Some(PRIMARY_KEY));
        }
        assert!(request_context_text(&captured[0].context).contains(FIRST_TASK));
        assert!(request_context_text(&captured[1].context).contains(LATER_TASK));
    }

    service.shutdown().await.expect("shutdown old service");
    application.cleanup().await;
    drop(application);

    // Restart through a new service/factory and the same durable parent
    // recorder; the recovered child request must defend the same invariants.
    let auth_events_before_restart = auth_models.lock().len();
    let restart_observed_auth = auth_models.clone();
    let restart_primary_provider = primary.provider.clone();
    let restart_fallback_provider = fallback.provider.clone();
    let restart_auth_resolver: pi_coding::SessionAuthResolver = Arc::new(move |model: Model| {
        restart_observed_auth.lock().push(model.clone());
        let primary_provider = restart_primary_provider.clone();
        let fallback_provider = restart_fallback_provider.clone();
        Box::pin(async move {
            let api_key = if model.provider == fallback_provider {
                FALLBACK_KEY
            } else if model.provider == primary_provider {
                ROTATED_PRIMARY_KEY
            } else {
                anyhow::bail!(
                    "unexpected restart auth model {}/{}",
                    model.provider,
                    model.id
                )
            };
            Ok(RequestAuth {
                api_key: api_key.to_owned(),
                kind: RequestAuthKind::ApiKeyOrHeaders,
                ..RequestAuth::default()
            })
        })
    });
    let session_b = Session::new_with_additional_tools_filtered_and_discovery(
        SessionOptions {
            model: primary.clone(),
            cwd: artifacts.path().to_path_buf(),
            system_prompt: RESTART_PARENT_ONLY_PROMPT.into(),
            thinking_level: ThinkingLevel::Off,
            api_key: RESTART_STATIC_KEY.into(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: Some(capturing_stream_with_responses(
                requests.clone(),
                captured_responses.clone(),
            )),
            auth_resolver: Some(restart_auth_resolver),
            auth_manager: None,
        },
        Vec::new(),
        Default::default(),
        ResourceDiscovery::Disabled,
    )
    .expect("restart parent session");
    session_b.set_session_dir(test_sessions_root());
    session_b.record(recorder).expect("reuse parent recorder");
    attach_resources(&session_b, artifacts.path()).await;
    let mut changed_parent_chains = RetryFallbackChains::default();
    changed_parent_chains.insert(
        "default".into(),
        vec![format!("{}/{}", primary.provider, primary.id)],
    );
    session_b.set_retry_settings(RetrySettings {
        enabled: true,
        max_retries: 0,
        base_delay_ms: 999,
        model_fallback: false,
        fallback_chains: changed_parent_chains,
    });
    assert_eq!(
        session_b.retry_settings().fallback_chains["default"],
        vec![format!("{}/{}", primary.provider, primary.id)],
        "restart parent fixture must differ from the fork-time child fallback chain"
    );

    let new_service = TodoChildService::new();
    new_service
        .configure_factory(factory)
        .expect("configure restart factory");
    let restarted = application_with_service(session_b, &new_service).await;
    let recovered = restarted
        .todo_child_add_task(
            None,
            &child_id,
            TodoChildAddTaskRequest {
                content: RESTART_TASK.into(),
                ..Default::default()
            },
        )
        .await
        .expect("add recovered task");
    wait_task_status(
        &restarted,
        &child_id,
        &recovered.task.id,
        TodoChildTaskStatus::Completed,
        5,
    )
    .await;
    wait_captured_requests(&requests, 3).await;

    let restart_request = requests.lock()[2].clone();
    let restart_text = request_context_text(&restart_request.context);
    assert!(
        restart_text.contains(PRE_FORK),
        "restart lost pre-fork context: {restart_text}"
    );
    assert!(
        !restart_text.contains(POST_FORK),
        "restart leaked post-fork context: {restart_text}"
    );
    assert!(
        restart_text.contains(RESTART_TASK),
        "restart task missing: {restart_text}"
    );
    assert!(restart_request.context.system_prompt.contains("Todo child"));
    assert!(
        !restart_request
            .context
            .system_prompt
            .contains(RESTART_PARENT_ONLY_PROMPT)
    );
    assert!(restart_request.context.system_prompt.contains(AGENTS_RULE));
    assert!(restart_request.context.system_prompt.contains(PERSONA_RULE));
    assert_eq!(restart_request.model.provider, primary.provider);
    assert_eq!(restart_request.model.id, primary.id);
    assert_eq!(
        restart_request.options.stream.api_key.as_deref(),
        Some(ROTATED_PRIMARY_KEY),
        "recovered primary request must re-resolve auth rather than reuse the restart parent key",
    );
    assert_ne!(
        restart_request.options.stream.api_key.as_deref(),
        Some(RESTART_STATIC_KEY),
    );
    assert!(
        auth_models
            .lock()
            .iter()
            .skip(auth_events_before_restart)
            .any(|model| model.provider == primary.provider && model.id == primary.id),
        "recovered primary model must be observed by the restart auth resolver",
    );

    // The child retains its captured StreamFn across model switches, so both
    // the primary failure and fallback success cross the same request-capture
    // boundary. The second request must still carry the fallback model/auth.
    captured_responses.lock().extend([
        FauxResponse::error("503 primary unavailable"),
        FauxResponse::text("fallback-completed"),
    ]);
    let fallback_task = restarted
        .todo_child_add_task(
            None,
            &child_id,
            TodoChildAddTaskRequest {
                content: FALLBACK_TASK.into(),
                ..Default::default()
            },
        )
        .await
        .expect("add fallback task");
    wait_task_status(
        &restarted,
        &child_id,
        &fallback_task.task.id,
        TodoChildTaskStatus::Completed,
        5,
    )
    .await;
    wait_captured_requests(&requests, 5).await;
    let captured_requests = requests.lock();
    let primary_fallback_request = &captured_requests[3];
    assert_eq!(
        primary_fallback_request.model.provider, primary.provider,
        "fallback turn must begin on inherited primary provider",
    );
    assert_eq!(primary_fallback_request.model.id, primary.id);
    assert_eq!(
        primary_fallback_request.options.stream.api_key.as_deref(),
        Some(ROTATED_PRIMARY_KEY),
        "fallback turn primary attempt must retain recovered primary auth",
    );
    let primary_fallback_text = request_context_text(&primary_fallback_request.context);
    assert!(
        primary_fallback_text.contains(PRE_FORK),
        "fallback primary request lost pre-fork context: {primary_fallback_text}"
    );
    assert!(
        !primary_fallback_text.contains(POST_FORK),
        "fallback primary request leaked post-fork context: {primary_fallback_text}"
    );
    assert!(
        primary_fallback_text.contains(FALLBACK_TASK),
        "fallback task missing from primary request: {primary_fallback_text}"
    );

    let fallback_request = &captured_requests[4];
    assert_eq!(fallback_request.model.provider, fallback.provider);
    assert_eq!(fallback_request.model.id, fallback.id);
    assert_eq!(
        fallback_request.options.stream.api_key.as_deref(),
        Some(FALLBACK_KEY),
    );
    let fallback_text = request_context_text(&fallback_request.context);
    assert!(
        fallback_text.contains(PRE_FORK),
        "fallback lost pre-fork context: {fallback_text}"
    );
    assert!(
        !fallback_text.contains(POST_FORK),
        "fallback leaked post-fork context: {fallback_text}"
    );
    assert!(
        fallback_text.contains(FALLBACK_TASK),
        "fallback task missing: {fallback_text}"
    );

    new_service.shutdown().await.expect("shutdown new service");
    restarted.cleanup().await;
    registration.unregister();
    fallback_registration.unregister();
}

/// Service shutdown is the host-owned runtime boundary: it closes admission,
/// interrupts running work durably, preserves queued messages, and awaits all
/// child runtimes without permanently stopping their durable sessions. A fresh
/// service recovers the children and may continue with send/add-task.
#[tokio::test]
async fn service_shutdown_preserves_restartable_children() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (model, registration) = faux_model();

    let controlled = ControlledStream::new();
    let (session, recorder) = recorded_parent_with_stream(
        artifacts.path(),
        "parent-service-shutdown",
        &model,
        Some(controlled.stream_fn()),
    )
    .await;
    let (service, factory) = shared_service();
    let application = application_with_service(session, &service).await;

    let paused_child = application
        .todo_child_create(TodoChildCreateRequest {
            label: Some("shutdown-paused".into()),
            idempotency_key: Some("shutdown-paused".into()),
        })
        .await
        .expect("create paused child");
    let paused_id = paused_child.identity.child_session_id.clone();
    application
        .todo_child_pause(None, Some(&paused_id))
        .await
        .expect("pause child");
    let queued = application
        .todo_child_send(None, Some(&paused_id), "queued before shutdown".into())
        .await
        .expect("queue before shutdown");
    assert_eq!(queued.status, TodoChildReceiptStatus::Queued);

    let running_child = application
        .todo_child_create(TodoChildCreateRequest {
            label: Some("shutdown-running".into()),
            idempotency_key: Some("shutdown-running".into()),
        })
        .await
        .expect("create running child");
    let running_id = running_child.identity.child_session_id.clone();
    let gate = controlled.arm();
    let task = application
        .todo_child_add_task(
            None,
            &running_id,
            TodoChildAddTaskRequest {
                content: "active during service shutdown".into(),
                ..Default::default()
            },
        )
        .await
        .expect("add running task");
    wait_task_status(
        &application,
        &running_id,
        &task.task.id,
        TodoChildTaskStatus::Running,
        5,
    )
    .await;

    let shutdown_service = service.clone();
    let shutdown = tokio::spawn(async move { shutdown_service.shutdown().await });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match application.todo_child_list(None).await {
                Err(error)
                    if error
                        .to_string()
                        .contains("Todo child service is shut down") =>
                {
                    break;
                }
                _ => tokio::task::yield_now().await,
            }
        }
    })
    .await
    .expect("shutdown did not close admission");
    gate.cancel();
    shutdown
        .await
        .expect("shutdown task panicked")
        .expect("shutdown service");
    service
        .shutdown()
        .await
        .expect("second shutdown is idempotent");

    let create_error = application
        .todo_child_create(TodoChildCreateRequest::default())
        .await
        .expect_err("create after shutdown must fail");
    assert!(
        create_error
            .to_string()
            .contains("Todo child service is shut down")
    );
    let send_error = application
        .todo_child_send(None, Some(&paused_id), "late send".into())
        .await
        .expect_err("send after shutdown must fail");
    assert!(
        send_error
            .to_string()
            .contains("Todo child service is shut down")
    );
    let status_error = application
        .todo_child_status(None, Some(&paused_id))
        .await
        .expect_err("status after shutdown must fail");
    assert!(
        status_error
            .to_string()
            .contains("Todo child service is shut down")
    );
    let list_error = application
        .todo_child_list(None)
        .await
        .expect_err("list after shutdown must fail");
    assert!(
        list_error
            .to_string()
            .contains("Todo child service is shut down")
    );
    let add_error = application
        .todo_child_add_task(
            None,
            &paused_id,
            TodoChildAddTaskRequest {
                content: "late task".into(),
                ..Default::default()
            },
        )
        .await
        .expect_err("add_task after shutdown must fail");
    assert!(
        add_error
            .to_string()
            .contains("Todo child service is shut down")
    );

    application.cleanup().await;
    drop(application);

    let recovered_parent = Session::new_with_additional_tools_filtered_and_discovery(
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
            stream_fn: None,
            auth_resolver: None,
            auth_manager: None,
        },
        Vec::new(),
        Default::default(),
        ResourceDiscovery::Disabled,
    )
    .expect("recovered parent session");
    recovered_parent.set_session_dir(test_sessions_root());
    recovered_parent
        .record(recorder)
        .expect("reuse parent recorder");
    attach_resources(&recovered_parent, artifacts.path()).await;
    let recovered_service = TodoChildService::new();
    recovered_service
        .configure_factory(factory)
        .expect("configure recovered service");
    let recovered = application_with_service(recovered_parent, &recovered_service).await;

    let catalog = recovered
        .todo_child_list(None)
        .await
        .expect("list recovered children");
    assert_eq!(
        catalog.len(),
        2,
        "shutdown must retain both durable catalog entries"
    );
    let paused_entry = catalog
        .iter()
        .find(|entry| entry.child_session_id == paused_id)
        .expect("paused child remains in catalog");
    assert_eq!(paused_entry.status, TodoChildStatus::Paused);
    let running_entry = catalog
        .iter()
        .find(|entry| entry.child_session_id == running_id)
        .expect("running child remains in catalog");
    assert_ne!(running_entry.status, TodoChildStatus::Stopped);

    let paused = recovered
        .todo_child_status(None, Some(&paused_id))
        .await
        .expect("recover paused child");
    assert_eq!(paused.status, TodoChildStatus::Paused);
    let queued_receipt = paused
        .receipts
        .iter()
        .find(|receipt| receipt.id == queued.id)
        .expect("queued receipt recovered");
    assert_eq!(queued_receipt.status, TodoChildReceiptStatus::Queued);

    recovered
        .todo_child_resume(None, Some(&paused_id))
        .await
        .expect("resume paused child");
    let continued_send = recovered
        .todo_child_send(None, Some(&paused_id), "continue after host restart".into())
        .await
        .expect("send after host restart");
    assert_eq!(continued_send.child_session_id, paused_id);

    let running = recovered
        .todo_child_status(None, Some(&running_id))
        .await
        .expect("recover running child");
    let interrupted = running
        .tasks
        .iter()
        .find(|candidate| candidate.id == task.task.id)
        .expect("running task recovered");
    assert_eq!(interrupted.status, TodoChildTaskStatus::Interrupted);
    let continued_task = recovered
        .todo_child_add_task(
            None,
            &running_id,
            TodoChildAddTaskRequest {
                content: "continue task after host restart".into(),
                ..Default::default()
            },
        )
        .await
        .expect("add task after host restart");
    assert!(
        continued_task.revision > running.revision,
        "add_task after host restart must durably advance the recovered DAG"
    );

    recovered_service
        .shutdown()
        .await
        .expect("shutdown recovered service");
    recovered.cleanup().await;
    registration.unregister();
}

/// Pausing disables execution but not bounded admission. Exactly 64 messages
/// may remain queued; overflow and input validation errors must not mutate the
/// receipt set or remove the child from its catalog.
#[tokio::test]
async fn paused_queue_at_capacity_rejects_send_without_dropping_queued() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (model, registration) = faux_model();

    let (session, _recorder) =
        recorded_parent(artifacts.path(), "parent-queue-capacity", &model).await;
    let (application, service) = application_with_factory(session).await;
    let child = application
        .todo_child_create(TodoChildCreateRequest {
            label: Some("bounded-queue".into()),
            idempotency_key: Some("bounded-queue".into()),
        })
        .await
        .expect("create child");
    let child_id = child.identity.child_session_id.clone();
    application
        .todo_child_pause(None, Some(&child_id))
        .await
        .expect("pause child");

    let mut accepted_ids = Vec::new();
    let mut accepted_message_ids = HashSet::new();
    for index in 0..64 {
        let receipt = application
            .todo_child_send(None, Some(&child_id), format!("queued message {index}"))
            .await
            .expect("queue within capacity");
        assert_eq!(receipt.status, TodoChildReceiptStatus::Queued);
        assert!(accepted_message_ids.insert(receipt.message_id.clone()));
        accepted_ids.push(receipt.id);
    }
    assert_eq!(accepted_ids.iter().collect::<HashSet<_>>().len(), 64);

    let full_error = application
        .todo_child_send(None, Some(&child_id), "queue overflow".into())
        .await
        .expect_err("65th queued message must fail");
    assert!(
        full_error
            .to_string()
            .contains("todo child message queue is full")
    );
    let blank_error = application
        .todo_child_send(None, Some(&child_id), "   ".into())
        .await
        .expect_err("blank message must fail");
    assert!(
        blank_error
            .to_string()
            .contains("todo child message must not be blank")
    );
    let oversized_error = application
        .todo_child_send(None, Some(&child_id), "x".repeat(64 * 1024 + 1))
        .await
        .expect_err("oversized message must fail");
    assert!(
        oversized_error
            .to_string()
            .contains("todo child message exceeds")
    );

    let status = application
        .todo_child_status(None, Some(&child_id))
        .await
        .expect("status after rejected sends");
    assert_eq!(status.status, TodoChildStatus::Paused);
    assert!(!status.is_running);
    assert_eq!(status.receipts.len(), 64);
    assert!(
        status
            .receipts
            .iter()
            .all(|receipt| receipt.status == TodoChildReceiptStatus::Queued)
    );
    assert_eq!(
        status
            .receipts
            .iter()
            .map(|receipt| receipt.id.as_str())
            .collect::<Vec<_>>(),
        accepted_ids.iter().map(String::as_str).collect::<Vec<_>>(),
        "rejected sends must not mutate or reorder accepted receipts",
    );
    assert_eq!(
        status
            .receipts
            .iter()
            .map(|receipt| receipt.message_id.as_str())
            .collect::<HashSet<_>>(),
        accepted_message_ids
            .iter()
            .map(String::as_str)
            .collect::<HashSet<_>>(),
    );
    let catalog = application
        .todo_child_list(None)
        .await
        .expect("list bounded child");
    let entry = catalog
        .iter()
        .find(|entry| entry.child_session_id == child_id)
        .expect("bounded child remains in catalog");
    assert_eq!(entry.status, TodoChildStatus::Paused);

    service.shutdown().await.expect("shutdown");
    application.cleanup().await;
    registration.unregister();
}

/// Crash recovery retains the paused queue and immutable fork ancestry without
/// replay. Explicit resume drains the recovered queue exactly once, preserving
/// receipt identities rather than appending duplicates.
#[tokio::test]
async fn restart_recovers_paused_queued_receipts_and_catalog_ancestry() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (model, registration) = faux_model();
    set_child_responses(&registration, 8);

    let (session, recorder) =
        recorded_parent(artifacts.path(), "parent-paused-queue-recovery", &model).await;
    let (service, factory) = shared_service();
    let application = application_with_service(session, &service).await;
    let child = application
        .todo_child_create(TodoChildCreateRequest {
            label: Some("paused-recovery".into()),
            idempotency_key: Some("paused-recovery".into()),
        })
        .await
        .expect("create child");
    let child_id = child.identity.child_session_id.clone();
    application
        .todo_child_pause(None, Some(&child_id))
        .await
        .expect("pause child");

    let original_entry = application
        .todo_child_list(None)
        .await
        .expect("list before crash")
        .into_iter()
        .find(|entry| entry.child_session_id == child_id)
        .expect("child entry before crash");
    let mut original_receipts = Vec::new();
    for index in 0..3 {
        original_receipts.push(
            application
                .todo_child_send(None, Some(&child_id), format!("recover queued {index}"))
                .await
                .expect("queue before crash"),
        );
    }

    // Simulate process loss: neither the parent Application nor the service
    // gets a graceful cleanup boundary, so the durable receipts remain Queued.
    drop(application);
    drop(service);
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }

    let recovered_parent = Session::new_with_additional_tools_filtered_and_discovery(
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
            stream_fn: None,
            auth_resolver: None,
            auth_manager: None,
        },
        Vec::new(),
        Default::default(),
        ResourceDiscovery::Disabled,
    )
    .expect("recovered parent session");
    recovered_parent.set_session_dir(test_sessions_root());
    recovered_parent
        .record(recorder)
        .expect("reuse parent recorder");
    attach_resources(&recovered_parent, artifacts.path()).await;
    let recovered_service = TodoChildService::new();
    recovered_service
        .configure_factory(factory)
        .expect("configure recovered service");
    let recovered = application_with_service(recovered_parent, &recovered_service).await;

    let recovered_catalog = recovered
        .todo_child_list(None)
        .await
        .expect("list after crash");
    let recovered_entry = recovered_catalog
        .iter()
        .find(|entry| entry.child_session_id == child_id)
        .expect("recovered catalog entry");
    assert_eq!(recovered_entry.status, TodoChildStatus::Paused);
    assert_eq!(
        recovered_entry.parent_entry_id,
        original_entry.parent_entry_id
    );
    assert_eq!(
        recovered_entry.parent_revision,
        original_entry.parent_revision
    );

    let queued = recovered
        .todo_child_status(None, Some(&child_id))
        .await
        .expect("recover paused child");
    assert_eq!(queued.status, TodoChildStatus::Paused);
    assert!(!queued.is_running);
    assert_eq!(queued.receipts.len(), original_receipts.len());
    assert_eq!(
        queued
            .receipts
            .iter()
            .map(|receipt| (&receipt.id, &receipt.message_id, receipt.status))
            .collect::<Vec<_>>(),
        original_receipts
            .iter()
            .map(|receipt| (
                &receipt.id,
                &receipt.message_id,
                TodoChildReceiptStatus::Queued
            ))
            .collect::<Vec<_>>(),
    );

    tokio::time::sleep(Duration::from_millis(100)).await;
    let still_queued = recovered
        .todo_child_status(None, Some(&child_id))
        .await
        .expect("status before explicit resume");
    assert_eq!(
        still_queued
            .receipts
            .iter()
            .map(|receipt| receipt.status)
            .collect::<Vec<_>>(),
        vec![TodoChildReceiptStatus::Queued; original_receipts.len()],
        "paused recovered messages must not replay before resume",
    );

    recovered
        .todo_child_resume(None, Some(&child_id))
        .await
        .expect("resume recovered child");
    let completed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let status = recovered
                .todo_child_status(None, Some(&child_id))
                .await
                .expect("status while draining recovered queue");
            if status.receipts.len() == original_receipts.len()
                && status
                    .receipts
                    .iter()
                    .all(|receipt| receipt.status == TodoChildReceiptStatus::Completed)
            {
                break status;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("recovered queue did not drain");
    assert_eq!(completed.status, TodoChildStatus::Idle);
    assert_eq!(
        completed
            .receipts
            .iter()
            .map(|receipt| receipt.id.as_str())
            .collect::<Vec<_>>(),
        original_receipts
            .iter()
            .map(|receipt| receipt.id.as_str())
            .collect::<Vec<_>>(),
        "resume must settle existing receipts without duplicating them",
    );
    let final_catalog = recovered
        .todo_child_list(None)
        .await
        .expect("final catalog");
    let final_entry = final_catalog
        .iter()
        .find(|entry| entry.child_session_id == child_id)
        .expect("final catalog entry");
    assert_eq!(final_entry.parent_entry_id, original_entry.parent_entry_id);
    assert_eq!(final_entry.parent_revision, original_entry.parent_revision);

    recovered_service
        .shutdown()
        .await
        .expect("shutdown recovered service");
    recovered.cleanup().await;
    registration.unregister();
}

#[tokio::test]
async fn natural_language_routed_child_shutdown_propagates_settlement_failure() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let marker_path = artifacts.path().join("routed-shutdown-failure.log");
    let (model, registration) = faux_model();
    let entered = Arc::new(tokio::sync::Semaphore::new(0));
    let release = CancellationToken::new();
    let invocation_count = Arc::new(AtomicUsize::new(0));
    let service = TodoChildService::new();
    service
        .configure_factory(Arc::new(TodoChildTestFactory::with_runtime_stream(
            receipt_side_effect_stream(
                marker_path.clone(),
                entered.clone(),
                release.clone(),
                invocation_count.clone(),
            ),
        )))
        .expect("configure routing factory");
    let (session, recorder) =
        recorded_parent(artifacts.path(), "parent-routed-shutdown-failure", &model).await;
    attach_auto_route_resources(&session, artifacts.path()).await;
    let application = application_with_service(session, &service).await;

    assert!(
        application
            .prompt(
                "implement the routed shutdown fixture in src/lib.rs".to_owned(),
                Vec::new(),
                None,
            )
            .await
            .expect("route code task")
            .is_none()
    );
    let entered_permit = tokio::time::timeout(Duration::from_secs(5), entered.acquire())
        .await
        .expect("routed child did not enter its turn")
        .expect("routed child entry semaphore remains open");
    entered_permit.forget();
    let child = application
        .todo_child_list(None)
        .await
        .expect("list routed child")
        .into_iter()
        .next()
        .expect("routed child");
    let receipt = application
        .todo_child_status(None, Some(&child.child_session_id))
        .await
        .expect("routed child status")
        .receipts
        .into_iter()
        .next()
        .expect("routed receipt");
    let child_journal = test_sessions_root()
        .join("children")
        .join(recorder.id())
        .join(format!("{}.jsonl", child.child_session_id));
    mutate_running_receipt_accepted_at(&child_journal, &receipt.id);
    release.cancel();

    let shutdown_error = service
        .shutdown()
        .await
        .expect_err("routed child settlement failure must propagate through shutdown");
    let message = shutdown_error.to_string();
    assert!(message.contains("Todo child shutdown failed"), "{message}");
    assert!(
        message.contains("Todo child receipt immutable fields changed"),
        "{message}"
    );
    assert_eq!(marker_execution_count(&marker_path), 1);
    assert_eq!(invocation_count.load(Ordering::SeqCst), 2);

    application.cleanup().await;
    registration.unregister();
}

#[tokio::test]
async fn terminal_receipt_append_failure_is_not_replayed_after_restart() {
    const RECOVERY_SUMMARY: &str =
        "execution outcome is unknown after restart; message was not replayed";
    let artifacts = tempfile::tempdir().expect("artifacts");
    let marker_path = artifacts.path().join("receipt-side-effect.log");
    let (model, registration) = faux_model();
    let entered = Arc::new(tokio::sync::Semaphore::new(0));
    let release = CancellationToken::new();
    let invocation_count = Arc::new(AtomicUsize::new(0));
    let stream_fn = receipt_side_effect_stream(
        marker_path.clone(),
        entered.clone(),
        release.clone(),
        invocation_count.clone(),
    );

    let (session, recorder) =
        recorded_parent(artifacts.path(), "parent-receipt-exactly-once", &model).await;
    let parent_id = recorder.id();
    let service = TodoChildService::new();
    service
        .configure_factory(Arc::new(TodoChildTestFactory::with_runtime_stream(
            stream_fn.clone(),
        )))
        .expect("configure side-effect child factory");
    let application = application_with_service(session, &service).await;
    let child = application
        .todo_child_create(TodoChildCreateRequest {
            label: Some("receipt-exactly-once".to_owned()),
            idempotency_key: Some("receipt-exactly-once".to_owned()),
        })
        .await
        .expect("create receipt child");
    let child_id = child.identity.child_session_id.clone();

    let receipt = application
        .todo_child_send(
            None,
            Some(&child_id),
            "execute the durable side effect once".to_owned(),
        )
        .await
        .expect("accept receipt message");
    let entered_permit = tokio::time::timeout(Duration::from_secs(5), entered.acquire())
        .await
        .expect("child model turn did not reach the injection gate")
        .expect("child-entry semaphore remains open");
    entered_permit.forget();
    let child_journal = test_sessions_root()
        .join("children")
        .join(&parent_id)
        .join(format!("{child_id}.jsonl"));
    let mutated_accepted_at = mutate_running_receipt_accepted_at(&child_journal, &receipt.id);
    assert_ne!(mutated_accepted_at, receipt.accepted_at);
    release.cancel();

    let failed = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let status = application
                .todo_child_status(None, Some(&child_id))
                .await
                .expect("status after terminal append injection");
            if status.status == TodoChildStatus::Failed && marker_execution_count(&marker_path) == 1
            {
                break status;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("terminal receipt append did not fail after the side effect");
    assert_eq!(failed.receipts.len(), 1);
    assert_eq!(failed.receipts[0].id, receipt.id);
    assert_eq!(failed.receipts[0].status, TodoChildReceiptStatus::Running);
    assert!(
        failed
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("Todo child receipt immutable fields changed")),
        "terminal append failure must expose immutable-field validation: {:?}",
        failed.last_error,
    );
    assert_eq!(marker_execution_count(&marker_path), 1);
    assert_eq!(invocation_count.load(Ordering::SeqCst), 2);

    let shutdown_error = service
        .shutdown()
        .await
        .expect_err("injected immutable-field mismatch must block graceful settlement");
    assert!(
        shutdown_error
            .to_string()
            .contains("Todo child receipt immutable fields changed")
    );
    application.cleanup().await;
    drop(service);

    let recovered_parent = Session::new_with_additional_tools_filtered_and_discovery(
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
            stream_fn: None,
            auth_resolver: None,
            auth_manager: None,
        },
        Vec::new(),
        Default::default(),
        ResourceDiscovery::Disabled,
    )
    .expect("recovered parent session");
    recovered_parent.set_session_dir(test_sessions_root());
    recovered_parent
        .record(recorder)
        .expect("reuse parent recorder");
    attach_resources(&recovered_parent, artifacts.path()).await;
    let recovered_service = TodoChildService::new();
    recovered_service
        .configure_factory(Arc::new(TodoChildTestFactory::with_runtime_stream(
            stream_fn,
        )))
        .expect("configure recovered child factory");
    let recovered_application =
        application_with_service(recovered_parent, &recovered_service).await;
    let recovered = recovered_application
        .todo_child_status(None, Some(&child_id))
        .await
        .expect("recover failed receipt");

    assert_eq!(recovered.status, TodoChildStatus::Failed);
    assert!(!recovered.is_running);
    assert_eq!(
        recovered.receipts.len(),
        1,
        "restart must not duplicate the receipt"
    );
    let recovered_receipt = &recovered.receipts[0];
    assert_eq!(recovered_receipt.id, receipt.id);
    assert_eq!(recovered_receipt.message_id, receipt.message_id);
    assert_eq!(recovered_receipt.accepted_at, mutated_accepted_at);
    assert_eq!(recovered_receipt.status, TodoChildReceiptStatus::Failed);
    assert_eq!(recovered_receipt.summary.as_deref(), Some(RECOVERY_SUMMARY));
    recovered_service
        .shutdown()
        .await
        .expect("shutdown recovered service");
    assert_eq!(marker_execution_count(&marker_path), 1);
    assert_eq!(
        invocation_count.load(Ordering::SeqCst),
        2,
        "restart must not replay the model turn"
    );
    recovered_application.cleanup().await;
    registration.unregister();
}

/// A Todo child is a complete production Application, not a projection onto
/// its parent. Its canonical Todo DAG, orchestration jobs/agents/IRC,
/// supervised process, transcript, and fork-time model evidence remain owned
/// by that child while the parent and a sibling stay byte-for-byte unchanged.
#[tokio::test]
async fn complete_child_runtime_publication_is_child_owned_and_bidirectionally_isolated() {
    const PARENT_BEFORE: &str = "runtime-isolation parent before fork 80f2d9";
    const PARENT_AFTER: &str = "runtime-isolation parent after fork f29a8c";
    const CHILD_TASK: &str = "runtime-isolation child task c28c41";
    const CANONICAL_TODO: &str = "runtime-isolation canonical child DAG node 36f23a";
    const SUBAGENT_OUTPUT: &str = "runtime-isolation private subagent output 785cde";
    const IRC_BODY: &str = "runtime-isolation private IRC 63fb4e";
    const PROCESS_LABEL: &str = "todo-child-runtime-isolation";

    let artifacts = tempfile::tempdir().expect("artifacts");
    let (model, registration) = faux_model();
    let main_responses = Arc::new(parking_lot::Mutex::new(VecDeque::from([
        tool_call_response(
            "child-todo-init",
            "todo",
            serde_json::json!({
                "op": "init",
                "list": [{ "phase": "Child runtime", "items": [CANONICAL_TODO] }]
            }),
        ),
        tool_call_response(
            "child-process-start",
            "process",
            serde_json::json!({
                "op": "start",
                "argv": ["/bin/cat"],
                "label": PROCESS_LABEL
            }),
        ),
        FauxResponse::text("child runtime orchestration published"),
    ])));
    let subagent_responses = Arc::new(parking_lot::Mutex::new(VecDeque::from([
        tool_call_response(
            "subagent-send-main",
            "hub",
            serde_json::json!({ "op": "send", "to": "Main", "message": IRC_BODY }),
        ),
        tool_call_response(
            "subagent-yield",
            "yield",
            serde_json::json!({ "text": SUBAGENT_OUTPUT }),
        ),
    ])));
    let subagent_release = CancellationToken::new();
    let main_calls = Arc::new(parking_lot::Mutex::new(Vec::<CapturedChildRequest>::new()));
    let subagent_calls = Arc::new(parking_lot::Mutex::new(Vec::<CapturedChildRequest>::new()));
    let stream_fn = runtime_isolation_stream(
        main_responses,
        subagent_responses,
        subagent_release.clone(),
        main_calls.clone(),
        subagent_calls.clone(),
    );

    let (session, recorder) = recorded_parent(
        artifacts.path(),
        "parent-complete-child-runtime-isolation",
        &model,
    )
    .await;
    let parent_id = recorder.id();
    recorder
        .record_message(&Message::user_text(PARENT_BEFORE, 1_000))
        .expect("record parent fork marker");
    let service = TodoChildService::new();
    service
        .configure_factory(Arc::new(TodoChildTestFactory::with_runtime_stream(
            stream_fn,
        )))
        .expect("configure runtime-isolated child factory");
    let application = application_with_service(session, &service).await;
    let child = application
        .todo_child_create(TodoChildCreateRequest {
            label: Some("runtime-proof-child".to_owned()),
            idempotency_key: Some("runtime-proof-child".to_owned()),
        })
        .await
        .expect("create runtime proof child");
    let child_manifest_entry = application
        .todo_child_list(None)
        .await
        .expect("child manifest after create")
        .into_iter()
        .find(|entry| entry.child_session_id == child.identity.child_session_id)
        .expect("runtime proof child manifest entry");
    let sibling = application
        .todo_child_create(TodoChildCreateRequest {
            label: Some("runtime-proof-sibling".to_owned()),
            idempotency_key: Some("runtime-proof-sibling".to_owned()),
        })
        .await
        .expect("create untouched sibling");
    recorder
        .record_message(&Message::user_text(PARENT_AFTER, 2_000))
        .expect("record post-fork parent marker");

    let parent_todo_before = application.todo_state();
    let parent_processes_before = application.process_list();
    let parent_model_before = application.session().model();
    let parent_retry_before = application.session().retry_settings();
    let parent_tree_before = recorder.tree().expect("parent tree before child work");
    let sibling_detail_before = application
        .todo_child_detail(&parent_id, &sibling.identity.child_session_id)
        .await
        .expect("sibling detail before child work");

    let added = application
        .todo_child_add_task(
            None,
            &child.identity.child_session_id,
            TodoChildAddTaskRequest {
                content: CHILD_TASK.to_owned(),
                ..Default::default()
            },
        )
        .await
        .expect("add child runtime task");
    wait_task_status(
        &application,
        &child.identity.child_session_id,
        &added.task.id,
        TodoChildTaskStatus::Completed,
        10,
    )
    .await;

    let (running_detail, job_id, canonical_task_id) =
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let detail = application
                    .todo_child_detail(&parent_id, &child.identity.child_session_id)
                    .await
                    .expect("child detail while awaiting DAG-owned job");
                if let Some((job_id, canonical_task_id)) = detail
                    .jobs
                    .iter()
                    .find(|job| job.task_id.is_some() && !job.status.is_settled())
                    .map(|job| {
                        (
                            job.job_id.clone(),
                            job.task_id.clone().expect("canonical task id"),
                        )
                    })
                {
                    break (detail, job_id, canonical_task_id);
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("DAG executor did not publish a running child-owned job");
    assert!(running_detail.transcript.iter().any(|entry| {
        entry.actor.as_deref() == Some("todo")
            && entry.text.contains(CANONICAL_TODO)
            && entry.text.contains(&canonical_task_id)
    }));
    subagent_release.cancel();

    let detail = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let detail = application
                .todo_child_detail(&parent_id, &child.identity.child_session_id)
                .await
                .expect("child detail while awaiting settled DAG job");
            if detail.jobs.iter().any(|job| {
                job.job_id == job_id
                    && job.status.is_settled()
                    && job.summary.as_deref() == Some(SUBAGENT_OUTPUT)
                    && job.task_id.as_deref() == Some(canonical_task_id.as_str())
            }) {
                break detail;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("child-owned DAG job did not settle");

    let child_path = recorder
        .path()
        .parent()
        .expect("parent session directory")
        .join("children")
        .join(&parent_id)
        .join(format!("{}.jsonl", child.identity.child_session_id));
    let child_tree =
        session_store::load_session_tree(&child_path).expect("load child runtime transcript");
    let child_context = child_tree.build_context(None);
    assert_eq!(
        child_context.provider.as_deref(),
        Some(model.provider.as_str())
    );
    assert_eq!(child_context.model_id.as_deref(), Some(model.id.as_str()));
    let child_text = child_context
        .messages
        .iter()
        .flat_map(|message| match message {
            Message::User(message) => Some(message.content.iter()),
            Message::Assistant(message) => Some(message.content.iter()),
            Message::ToolResult(message) => Some(message.content.iter()),
            Message::BashExecution(_)
            | Message::Custom(_)
            | Message::BranchSummary(_)
            | Message::CompactionSummary(_) => None,
        })
        .flatten()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(child_text.contains(PARENT_BEFORE));
    assert!(!child_text.contains(PARENT_AFTER));
    assert_eq!(detail.parent.session_id, parent_id);
    assert_eq!(detail.parent.entry_id, child_manifest_entry.parent_entry_id);
    assert_eq!(detail.parent.revision, child_manifest_entry.parent_revision);
    assert_eq!(
        detail.supervisor.as_ref().map(|agent| agent.id.as_str()),
        Some("Main")
    );
    assert!(detail.agents.iter().any(|agent| {
        agent.id.starts_with("Todo") && agent.task_id.as_deref() == Some(canonical_task_id.as_str())
    }));
    assert!(detail.tasks.iter().any(|task| {
        task.id == added.task.id
            && task.content == CHILD_TASK
            && task.status == TodoChildTaskStatus::Completed
    }));
    assert!(detail.irc.iter().any(|message| message.text == IRC_BODY));
    assert!(
        detail
            .transcript
            .iter()
            .any(|entry| entry.role == "tool_result" && entry.actor.as_deref() == Some("todo"))
    );
    assert!(
        detail
            .transcript
            .iter()
            .any(|entry| entry.role == "tool_result" && entry.actor.as_deref() == Some("process"))
    );
    assert_eq!(
        subagent_calls.lock().len(),
        2,
        "hub then yield subagent turns"
    );
    assert_eq!(
        subagent_calls.lock()[0].model,
        model,
        "subagent must run the fork-time child model",
    );

    let process_info: pi_coding::ProcessInfo = detail
        .transcript
        .iter()
        .find(|entry| entry.role == "tool_result" && entry.actor.as_deref() == Some("process"))
        .and_then(|entry| serde_json::from_str(&entry.text).ok())
        .expect("projected child process tool result must contain ProcessInfo JSON");
    assert_eq!(process_info.label.as_deref(), Some(PROCESS_LABEL));
    assert!(
        !process_info.state.is_terminal(),
        "process must be live before child cleanup"
    );
    let stopped_child = service
        .stop(
            &application.session(),
            Some(&parent_id),
            Some(&child.identity.child_session_id),
        )
        .await
        .expect("stop child and clean up its owned runtime");
    assert_eq!(stopped_child.status, TodoChildStatus::Stopped);
    assert!(!stopped_child.is_running);

    assert_eq!(application.todo_state(), parent_todo_before);
    assert_eq!(application.process_list(), parent_processes_before);
    assert_eq!(application.session().model(), parent_model_before);
    assert_eq!(application.session().retry_settings(), parent_retry_before);
    let parent_tree_after = recorder.tree().expect("parent tree after child work");
    assert_eq!(
        parent_tree_after.active_leaf_id,
        parent_tree_before.active_leaf_id
    );
    assert_eq!(
        serde_json::to_value(&parent_tree_after.entries).expect("serialize parent entries after"),
        serde_json::to_value(&parent_tree_before.entries).expect("serialize parent entries before"),
    );
    let sibling_detail_after = application
        .todo_child_detail(&parent_id, &sibling.identity.child_session_id)
        .await
        .expect("sibling detail after child work");
    assert_eq!(sibling_detail_after, sibling_detail_before);

    let parent_text = messages_text(&parent_tree_after.build_context(None).messages);
    assert!(parent_text.contains(PARENT_BEFORE));
    assert!(parent_text.contains(PARENT_AFTER));
    for private in [
        CHILD_TASK,
        CANONICAL_TODO,
        SUBAGENT_OUTPUT,
        IRC_BODY,
        PROCESS_LABEL,
    ] {
        assert!(
            !parent_text.contains(private),
            "child evidence leaked into parent: {private}"
        );
    }
    let sibling_text = sibling_detail_after
        .transcript
        .iter()
        .map(|entry| entry.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    for private in [
        CHILD_TASK,
        CANONICAL_TODO,
        SUBAGENT_OUTPUT,
        IRC_BODY,
        PROCESS_LABEL,
    ] {
        assert!(
            !sibling_text.contains(private),
            "child evidence leaked into sibling: {private}"
        );
    }
    assert_eq!(
        main_calls.lock().len(),
        3,
        "todo, process, terminal main turns"
    );

    service.shutdown().await.expect("shutdown");
    application.cleanup().await;
    registration.unregister();
}
// ---------------------------------------------------------------------------
// Transactional creation: factory-failure rollback, cancel race, idempotent
// cancel of a failed provisional identity.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum InjectionMode {
    /// Fail the complete-runtime build immediately (after the durable fork).
    Fail,
    /// Park the complete-runtime build on a gate until the test cancels.
    Gate,
    /// Park the complete-runtime build FOREVER, ignoring cancellation — a
    /// builder wedged outside any cancellation point (the factory future is
    /// only ever abandoned by the creation's own select dropping it).
    Never,
    /// Build a real child runtime via the working inner factory.
    Build,
}

/// Wraps a working Todo child factory with an injectable failure/gate at the
/// complete-runtime build step — after `ensure_child` has durably forked the
/// child transcript and registered the manifest entry.
#[derive(Clone)]
struct InjectionRuntimeFactory {
    inner: TodoChildTestFactory,
    mode: Arc<parking_lot::Mutex<InjectionMode>>,
    gate: CancellationToken,
    gate_entries: Arc<AtomicUsize>,
}

impl InjectionRuntimeFactory {
    fn new() -> Self {
        Self {
            inner: TodoChildTestFactory::new(),
            mode: Arc::new(parking_lot::Mutex::new(InjectionMode::Build)),
            gate: CancellationToken::new(),
            gate_entries: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn set_mode(&self, mode: InjectionMode) {
        *self.mode.lock() = mode;
    }
}

impl ApplicationRuntimeFactory for InjectionRuntimeFactory {
    fn build_runtime_candidate(
        &self,
        cwd: PathBuf,
        options: SessionOptions,
        resume: Option<PreparedSessionResume>,
    ) -> ApplicationRuntimeFuture {
        let inner = self.inner.clone();
        let mode = self.mode.clone();
        let gate = self.gate.clone();
        let gate_entries = self.gate_entries.clone();
        Box::pin(async move {
            let mode = *mode.lock();
            match mode {
                InjectionMode::Fail => Err(anyhow::anyhow!("injected complete-runtime failure")),
                InjectionMode::Gate => {
                    gate_entries.fetch_add(1, Ordering::SeqCst);
                    gate.cancelled().await;
                    Err(anyhow::anyhow!("injected gated failure"))
                }
                InjectionMode::Never => {
                    gate_entries.fetch_add(1, Ordering::SeqCst);
                    std::future::pending::<()>().await;
                    unreachable!("pending never resolves")
                }
                InjectionMode::Build => {
                    inner.build_runtime_candidate(cwd, options, resume).await
                }
            }
        })
    }
}

/// Mirrors `child_id_for_request` for idempotency-keyed creates so tests can
/// assert on-disk rollback without depending on list ordering.
fn idempotent_child_id(parent_session_id: &str, key: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest([parent_session_id.as_bytes(), b"\0", key.as_bytes()].concat());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(bytes).to_string()
}

fn collect_relative_paths(root: &Path) -> anyhow::Result<Vec<PathBuf>> {
    fn walk(directory: &Path, out: &mut Vec<PathBuf>) -> anyhow::Result<()> {
        use anyhow::Context as _;
        let entries = std::fs::read_dir(directory)
            .with_context(|| format!("reading directory {}", directory.display()))?;
        for entry in entries {
            let entry = entry
                .with_context(|| format!("reading entry under {}", directory.display()))?;
            let path = directory.join(entry.file_name());
            if entry
                .file_type()
                .with_context(|| format!("reading type of {}", path.display()))?
                .is_dir()
            {
                walk(&path, out)?;
            } else {
                out.push(path);
            }
        }
        Ok(())
    }
    let mut paths = Vec::new();
    walk(root, &mut paths)?;
    Ok(paths)
}

/// The real on-disk Todo child artifact root for a parent: the
/// `children/<parent session id>` directory under the session store root,
/// where transcripts and `todo-manifest.json` are durably written.
fn child_artifact_root(parent_session_id: &str) -> PathBuf {
    test_sessions_root().join("children").join(parent_session_id)
}

fn assert_no_child_artifacts(parent_session_id: &str, child_session_id: &str) {
    use anyhow::Context as _;
    let root = child_artifact_root(parent_session_id);
    // try_exists, never exists(): a stat/permission failure must fail the
    // test instead of silently reporting "absent".
    if !root
        .try_exists()
        .with_context(|| format!("stat child artifact root {}", root.display()))
        .expect("stat child artifact root")
    {
        // Explicitly checked absence: with no child directory at all under
        // the session store, nothing could have survived the rollback.
        return;
    }
    for path in
        collect_relative_paths(&root).expect("walk the real child artifact root")
    {
        let rendered = path.to_string_lossy().into_owned();
        assert!(
            !rendered.contains(child_session_id),
            "provisional Todo child artifact survived rollback: {rendered}"
        );
        if rendered.ends_with("todo-manifest.json") {
            let bytes = std::fs::read(&path).expect("read manifest");
            let manifest =
                serde_json::from_slice::<serde_json::Value>(&bytes).expect("decode manifest");
            let entries = manifest["entries"].as_array().expect("manifest entries");
            assert!(
                entries
                    .iter()
                    .all(|entry| entry["childSessionId"] != serde_json::json!(child_session_id)),
                "provisional Todo child manifest entry survived rollback"
            );
        }
    }
}

/// A factory failure at the complete-runtime build step — after the durable
/// fork exists — must leave zero listed/selected/partial state, must surface
/// the safe category plus internal cause without exposing paths, must make
/// cancel an idempotent success instead of a stale-selection error, and must
/// leave retry `/todo new` working. Status/select keep true "no child"
/// behavior.
#[tokio::test]
async fn failed_complete_build_rolls_back_provisional_child_and_retry_succeeds() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (model, registration) = faux_model();
    let (session, recorder) = recorded_parent(artifacts.path(), "parent-rollback-retry", &model).await;
    let child_id = idempotent_child_id(&recorder.id(), "rollback-retry");
    let service = TodoChildService::new();
    let factory = Arc::new(InjectionRuntimeFactory::new());
    factory.set_mode(InjectionMode::Fail);
    service
        .configure_factory(factory.clone())
        .expect("configure injected factory");
    let request = TodoChildCreateRequest {
        label: Some("rollback retry".to_owned()),
        idempotency_key: Some("rollback-retry".to_owned()),
    };

    let error = service
        .create_child(&session, request.clone())
        .await
        .expect_err("complete-runtime build failure must fail the create");
    let rendered = format!("{error:#}");
    assert!(
        rendered.contains("building complete Todo child application runtime"),
        "create error must carry the safe build category: {rendered}"
    );
    assert!(
        rendered.contains("injected complete-runtime failure"),
        "create error must carry the internal cause: {rendered}"
    );

    assert!(service.list(&session, None).await.expect("list").is_empty());
    assert!(service
        .snapshot_selected(&session)
        .expect("selected snapshot")
        .is_none());
    assert_no_child_artifacts(&recorder.id(), &child_id);

    // Cancel after a synchronously failed create is an idempotent success,
    // both bare and for the failed provisional identity.
    assert!(service
        .cancel(&session, None, None)
        .await
        .expect("bare cancel after failure")
        .is_none());
    assert!(service
        .cancel(&session, None, Some(&child_id))
        .await
        .expect("explicit cancel after failure")
        .is_none());
    // Status keeps true "no child" behavior for the rolled-back identity.
    let status_error = service
        .status(&session, None, Some(&child_id))
        .await
        .expect_err("status must still reject an absent child");
    assert!(
        format!("{status_error:#}").contains("does not exist"),
        "{status_error:#}"
    );

    // Retry the same create key succeeds on the clean slate.
    factory.set_mode(InjectionMode::Build);
    let result = service
        .create_child(&session, request)
        .await
        .expect("retry create after failure");
    assert_eq!(result.identity.child_session_id, child_id);
    let entries = service.list(&session, None).await.expect("list after retry");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].child_session_id, child_id);
    let snapshot = service
        .status(&session, None, Some(&child_id))
        .await
        .expect("status after retry");
    assert_eq!(snapshot.status, TodoChildStatus::Idle);

    service.shutdown().await.expect("shutdown");
    registration.unregister();
}

/// A failed rollback (provisional discard) must not poison the creation
/// ticket: the failing create surfaces the original build failure plus the
/// rollback failure as combined context, the exact creating ticket is removed
/// despite the rollback failure, and a retry never reuses the settled
/// (possibly-cancelled) ticket — it fails for the real retained-artifact
/// cause, and after repair (working factory) the retry succeeds on the
/// retained durable fork.
/// Compiled only with `test-support`: the discard-fault seam it injects does
/// not exist in production builds. Run with
/// `cargo test -p pi-coding --features test-support`.
#[cfg(feature = "test-support")]
#[tokio::test]
async fn discard_failure_clears_creation_ticket_and_retry_is_not_poisoned() {
    use anyhow::Context as _;
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (model, registration) = faux_model();
    let (session, recorder) =
        recorded_parent(artifacts.path(), "parent-discard-fault", &model).await;
    let child_id = idempotent_child_id(&recorder.id(), "discard-fault");
    let service = TodoChildService::new();
    let factory = Arc::new(InjectionRuntimeFactory::new());
    factory.set_mode(InjectionMode::Fail);
    service
        .configure_factory(factory.clone())
        .expect("configure injected factory");
    service.inject_provisional_discard_fault_for_test();
    let request = TodoChildCreateRequest {
        label: Some("discard fault".to_owned()),
        idempotency_key: Some("discard-fault".to_owned()),
    };

    // First create: the build fails and the rollback fails too. The error
    // carries the original safe build category plus the combined rollback
    // context, without leaking paths.
    let error = service
        .create_child(&session, request.clone())
        .await
        .expect_err("create must fail with build plus rollback failure");
    let rendered = format!("{error:#}");
    assert!(
        rendered.contains("building complete Todo child application runtime"),
        "error must carry the safe build category: {rendered}"
    );
    assert!(
        rendered.contains("injected complete-runtime failure"),
        "error must carry the original internal cause: {rendered}"
    );
    assert!(
        rendered.contains("discarding the provisional Todo child state after failure also failed"),
        "error must carry the rollback failure context: {rendered}"
    );
    assert!(
        rendered.contains("injected provisional discard fault"),
        "error must carry the rollback internal cause: {rendered}"
    );

    // The exact creating ticket is removed despite the failed rollback.
    assert_eq!(
        service.creation_ticket_count_for_test(),
        0,
        "failed rollback must not retain the creation ticket"
    );

    // The real retained artifact category: the manifest entry and transcript
    // survive because the rollback faulted, so a retry rebuilds from them.
    let entries = service
        .list(&session, None)
        .await
        .expect("list after failed rollback");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].child_session_id, child_id);
    let transcript = child_artifact_root(&recorder.id()).join(format!("{child_id}.jsonl"));
    // try_exists, never is_file(): a stat/permission failure must fail the
    // test instead of silently reporting "absent".
    assert!(
        transcript
            .try_exists()
            .with_context(|| format!("stat retained transcript {}", transcript.display()))
            .expect("stat retained transcript"),
        "retained provisional transcript must survive the failed rollback"
    );

    // Second create: must not be immediately cancelled by a stale settled
    // ticket. It fails for the real retained build category (the factory is
    // still failing), never with a cancellation.
    let retry_error = service
        .create_child(&session, request.clone())
        .await
        .expect_err("retry must still fail while the factory keeps failing");
    let retry_rendered = format!("{retry_error:#}");
    assert!(
        retry_rendered.contains("injected complete-runtime failure"),
        "retry must fail with the real build failure: {retry_rendered}"
    );
    assert!(
        !retry_rendered.contains("cancelled"),
        "retry must not reuse a cancelled creation ticket: {retry_rendered}"
    );
    assert_eq!(
        service.creation_ticket_count_for_test(),
        0,
        "second failed attempt must also clear its ticket"
    );

    // Repair: with a working factory the retry succeeds on the retained
    // durable fork.
    factory.set_mode(InjectionMode::Build);
    let result = service
        .create_child(&session, request)
        .await
        .expect("retry after repair succeeds");
    assert_eq!(result.identity.child_session_id, child_id);
    let snapshot = service
        .status(&session, None, Some(&child_id))
        .await
        .expect("status after repair retry");
    assert_eq!(snapshot.status, TodoChildStatus::Idle);

    service.shutdown().await.expect("shutdown");
    registration.unregister();
}

/// Cancelling while the complete-runtime build is gated aborts the build
/// future: the create settles cancelled, every provisional artifact is rolled
/// back, nothing is listed/selected/runnable, and retry succeeds.
#[tokio::test]
async fn cancel_racing_mid_build_aborts_create_and_rolls_back_provisional_child() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (model, registration) = faux_model();
    let (session, recorder) = recorded_parent(artifacts.path(), "parent-cancel-race", &model).await;
    let child_id = idempotent_child_id(&recorder.id(), "cancel-race");
    let service = TodoChildService::new();
    let factory = Arc::new(InjectionRuntimeFactory::new());
    factory.set_mode(InjectionMode::Gate);
    service
        .configure_factory(factory.clone())
        .expect("configure gated factory");
    let request = TodoChildCreateRequest {
        label: Some("cancel race".to_owned()),
        idempotency_key: Some("cancel-race".to_owned()),
    };

    let application = application_with_service(session, &service).await;

    let creator = tokio::spawn({
        let application = application.clone();
        let request = request.clone();
        async move { application.todo_child_create(request).await }
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while factory.gate_entries.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("gated factory was entered");

    // The in-progress creation is the only addressable lifecycle for the
    // parent: bare cancel targets it and waits for the creator to settle.
    let cancelled = tokio::time::timeout(
        Duration::from_secs(5),
        application.todo_child_cancel(None, None),
    )
    .await
    .expect("cancel must not hang on the in-progress creation")
    .expect("cancel succeeds");
    assert!(cancelled.is_none());

    let error = creator
        .await
        .expect("creator task")
        .expect_err("create must settle cancelled");
    assert_eq!(format!("{error:#}"), "Todo child creation was cancelled");

    assert!(application
        .todo_child_list(None)
        .await
        .expect("list")
        .is_empty());
    let status_error = application
        .todo_child_status(None, None)
        .await
        .expect_err("no selected child remains after rollback");
    assert!(
        format!("{status_error:#}").contains("no Todo child is selected"),
        "{status_error:#}"
    );
    assert_no_child_artifacts(&recorder.id(), &child_id);

    factory.set_mode(InjectionMode::Build);
    let result = application
        .todo_child_create(request)
        .await
        .expect("retry create after cancel");
    assert_eq!(result.identity.child_session_id, child_id);

    service.shutdown().await.expect("shutdown");
    application.cleanup().await;
    registration.unregister();
}

/// Cancelling a published child keeps its existing snapshot semantics: the
/// child stops, remains listed, and a second cancel is still a successful
/// no-op on the stopped child rather than an error.
#[tokio::test]
async fn cancel_of_published_child_keeps_snapshot_semantics() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (model, registration) = faux_model();
    let (session, _recorder) =
        recorded_parent(artifacts.path(), "parent-cancel-published", &model).await;
    let service = TodoChildService::new();
    service
        .configure_factory(Arc::new(InjectionRuntimeFactory::new()))
        .expect("configure working factory");
    let result = service
        .create_child(
            &session,
            TodoChildCreateRequest {
                label: Some("cancel published".to_owned()),
                idempotency_key: Some("cancel-published".to_owned()),
            },
        )
        .await
        .expect("create child");

    let snapshot = service
        .cancel(&session, None, None)
        .await
        .expect("cancel published child")
        .expect("published child cancels to a snapshot");
    assert_eq!(snapshot.child_session_id, result.identity.child_session_id);
    assert_eq!(snapshot.status, TodoChildStatus::Stopped);
    assert_eq!(
        service
            .list(&session, None)
            .await
            .expect("list after cancel")
            .len(),
        1
    );

    service.shutdown().await.expect("shutdown");
    registration.unregister();
}

/// Cancelling bare in the exact previous lost-wakeup window — after the
/// complete runtime built, before the candidate is published into the
/// service's children map — must wait for the creator and then cancel the
/// published supervisor: `Some(stopped snapshot)`, never `None` while the
/// creator can still publish, and no zombie task, receipt, manifest status,
/// or runtime survives. Retry creation succeeds against the settled child.
#[tokio::test]
async fn bare_cancel_in_post_build_pre_publication_window_cancels_published_supervisor() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (model, registration) = faux_model();
    let (session, recorder) =
        recorded_parent(artifacts.path(), "parent-cancel-prepublish", &model).await;
    let child_id = idempotent_child_id(&recorder.id(), "cancel-prepublish");
    let service = TodoChildService::new();
    service
        .configure_factory(Arc::new(InjectionRuntimeFactory::new()))
        .expect("configure working factory");
    let gate = service.arm_creation_before_publication_gate_for_test();
    let request = TodoChildCreateRequest {
        label: Some("cancel prepublish".to_owned()),
        idempotency_key: Some("cancel-prepublish".to_owned()),
    };
    let application = application_with_service(session, &service).await;

    let creator = tokio::spawn({
        let application = application.clone();
        let request = request.clone();
        async move { application.todo_child_create(request).await }
    });
    tokio::time::timeout(Duration::from_secs(5), gate.wait_entered())
        .await
        .expect("creator reached the pre-publication gate");

    let cancelled = tokio::spawn({
        let application = application.clone();
        async move { application.todo_child_cancel(None, None).await }
    });
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    gate.release();

    let created = tokio::time::timeout(Duration::from_secs(5), creator)
        .await
        .expect("creator settles after gate release")
        .expect("creator task")
        .expect("create succeeds");
    assert_eq!(created.identity.child_session_id, child_id);
    let snapshot = tokio::time::timeout(Duration::from_secs(5), cancelled)
        .await
        .expect("cancel settles after publication")
        .expect("cancel task")
        .expect("cancel ok")
        .expect("published child cancels to a snapshot");
    assert_eq!(snapshot.child_session_id, child_id);
    assert_eq!(snapshot.status, TodoChildStatus::Stopped);
    assert!(!snapshot.is_running);
    assert!(
        snapshot.receipts.is_empty(),
        "no running or queued receipt may survive the cancel race"
    );

    let entries = application.todo_child_list(None).await.expect("list");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].child_session_id, child_id);
    assert_eq!(entries[0].status, TodoChildStatus::Stopped);
    let status = application
        .todo_child_status(None, Some(&child_id))
        .await
        .expect("status after cancel");
    assert_eq!(status.status, TodoChildStatus::Stopped);

    let retry = application
        .todo_child_create(request)
        .await
        .expect("retry create after cancel");
    assert_eq!(retry.identity.child_session_id, child_id);
    assert_eq!(
        application
            .todo_child_status(None, Some(&child_id))
            .await
            .expect("status after retry")
            .status,
        TodoChildStatus::Stopped,
        "retry must address the cancelled child, not resurrect execution"
    );

    service.shutdown().await.expect("shutdown leaves no zombie runtime");
    application.cleanup().await;
    registration.unregister();
}

/// Cancelling with an explicit child id in the post-publication/pre-spawn
/// window — the supervisor is already in the children map but its execution
/// loops have not spawned — must wait for the creator to finish spawning and
/// then tear the supervisor down: `Some(stopped snapshot)` with zero running
/// tasks or receipts and no resurrected execution.
#[tokio::test]
async fn explicit_cancel_in_post_publication_pre_spawn_window_cancels_published_supervisor() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (model, registration) = faux_model();
    let (session, recorder) =
        recorded_parent(artifacts.path(), "parent-cancel-prespon", &model).await;
    let child_id = idempotent_child_id(&recorder.id(), "cancel-prespon");
    let service = TodoChildService::new();
    service
        .configure_factory(Arc::new(InjectionRuntimeFactory::new()))
        .expect("configure working factory");
    let gate = service.arm_creation_before_spawn_gate_for_test();
    let request = TodoChildCreateRequest {
        label: Some("cancel prespawn".to_owned()),
        idempotency_key: Some("cancel-prespon".to_owned()),
    };
    let application = application_with_service(session, &service).await;

    let creator = tokio::spawn({
        let application = application.clone();
        let request = request.clone();
        async move { application.todo_child_create(request).await }
    });
    tokio::time::timeout(Duration::from_secs(5), gate.wait_entered())
        .await
        .expect("creator reached the pre-spawn gate");

    let cancelled = tokio::spawn({
        let application = application.clone();
        let child_id = child_id.clone();
        async move { application.todo_child_cancel(None, Some(&child_id)).await }
    });
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    gate.release();

    let created = tokio::time::timeout(Duration::from_secs(5), creator)
        .await
        .expect("creator settles after gate release")
        .expect("creator task")
        .expect("create succeeds");
    assert_eq!(created.identity.child_session_id, child_id);

    let snapshot = tokio::time::timeout(Duration::from_secs(5), cancelled)
        .await
        .expect("cancel settles")
        .expect("cancel task")
        .expect("cancel ok")
        .expect("stopped child cancels to a snapshot");
    assert_eq!(snapshot.child_session_id, child_id);
    assert_eq!(snapshot.status, TodoChildStatus::Stopped);
    assert!(!snapshot.is_running);
    assert!(snapshot.receipts.is_empty());

    let entries = application.todo_child_list(None).await.expect("list");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].child_session_id, child_id);
    assert_eq!(entries[0].status, TodoChildStatus::Stopped);
    // A second explicit cancel on the torn-down supervisor stays an
    // idempotent success, never an error.
    let again = application
        .todo_child_cancel(None, Some(&child_id))
        .await
        .expect("idempotent second cancel")
        .expect("stopped child still cancels to a snapshot");
    assert_eq!(again.status, TodoChildStatus::Stopped);

    service.shutdown().await.expect("shutdown leaves no zombie runtime");
    application.cleanup().await;
    registration.unregister();
}

/// Cancelling a child that is registered in the manifest but has no published
/// runtime rebuilds it through the factory; real rebuild failures (factory,
/// I/O, derivation) must propagate — only an authoritative, cleanly-loaded
/// "not in manifest" maps to the idempotent `None`.
#[tokio::test]
async fn cancel_propagates_rebuild_failure_instead_of_swallowing_it() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (model, registration) = faux_model();
    let (session, recorder) =
        recorded_parent(artifacts.path(), "parent-cancel-rebuild-fail", &model).await;
    let child_id = idempotent_child_id(&recorder.id(), "cancel-rebuild-fail");
    let request = TodoChildCreateRequest {
        label: Some("cancel rebuild fail".to_owned()),
        idempotency_key: Some("cancel-rebuild-fail".to_owned()),
    };

    // Publish the child once so its manifest entry exists with no live
    // runtime, as after a host restart.
    let service = TodoChildService::new();
    service
        .configure_factory(Arc::new(InjectionRuntimeFactory::new()))
        .expect("configure working factory");
    service
        .create_child(&session, request.clone())
        .await
        .expect("initial create");
    service.shutdown().await.expect("host shutdown keeps the child restartable");
    drop(service);

    // A failing rebuild factory must surface through cancel instead of the
    // old swallow-everything `Ok(None)`.
    let failing = TodoChildService::new();
    let factory = Arc::new(InjectionRuntimeFactory::new());
    factory.set_mode(InjectionMode::Fail);
    failing
        .configure_factory(factory.clone())
        .expect("configure failing factory");
    let error = failing
        .cancel(&session, None, Some(&child_id))
        .await
        .expect_err("cancel must propagate the rebuild failure");
    let rendered = format!("{error:#}");
    assert!(
        rendered.contains("injected complete-runtime failure"),
        "cancel error must carry the internal cause: {rendered}"
    );

    // The failed rebuild must not roll back the pre-existing manifest entry.
    let entries = failing.list(&session, None).await.expect("list");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].child_session_id, child_id);

    // With the factory working again, cancel rebuilds and stops the child.
    factory.set_mode(InjectionMode::Build);
    let snapshot = failing
        .cancel(&session, None, Some(&child_id))
        .await
        .expect("cancel succeeds after rebuild")
        .expect("manifested child cancels to a snapshot");
    assert_eq!(snapshot.child_session_id, child_id);
    assert_eq!(snapshot.status, TodoChildStatus::Stopped);
    assert!(snapshot.receipts.is_empty());
    let entries = failing.list(&session, None).await.expect("list after cancel");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].status, TodoChildStatus::Stopped);

    failing.shutdown().await.expect("shutdown leaves no zombie runtime");
    registration.unregister();
}

/// A terminal shutdown must never hang on a never-returning factory build:
/// it cancels every in-flight creation ticket BEFORE the admission
/// exclusion, settles the cancelled creation bounded (the wedged factory
/// future is abandoned by the creation's own select), removes the ticket,
/// rolls the provisional artifacts back, and publishes `Ok(())`.
/// Compiled only with `test-support`: the creation-ticket seam it asserts on
/// does not exist in production builds. Run with
/// `cargo test -p pi-coding --features test-support`.
#[cfg(feature = "test-support")]
#[tokio::test]
async fn shutdown_cancels_never_returning_creation_bounded() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let (model, registration) = faux_model();
    let (session, recorder) =
        recorded_parent(artifacts.path(), "parent-never-build", &model).await;
    let child_id = idempotent_child_id(&recorder.id(), "never-build");
    let service = TodoChildService::new();
    let factory = Arc::new(InjectionRuntimeFactory::new());
    factory.set_mode(InjectionMode::Never);
    service
        .configure_factory(factory.clone())
        .expect("configure never factory");
    let request = TodoChildCreateRequest {
        label: Some("never build".to_owned()),
        idempotency_key: Some("never-build".to_owned()),
    };

    let creator = tokio::spawn({
        let service = service.clone();
        let session = session.clone();
        async move { service.create_child(&session, request).await }
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while factory.gate_entries.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the wedged build was entered");
    assert_eq!(
        service.creation_ticket_count_for_test(),
        1,
        "the in-flight creation holds its ticket"
    );

    let shutdown = tokio::spawn({
        let service = service.clone();
        async move { service.shutdown().await }
    });
    tokio::time::timeout(Duration::from_secs(15), shutdown)
        .await
        .expect("shutdown is bounded despite the never-returning build")
        .expect("shutdown task")
        .expect("shutdown succeeds");

    // The wedged creator settled with the exact cancellation, and the
    // ticket is gone: nothing is published, nothing provisioned.
    let error = tokio::time::timeout(Duration::from_secs(15), creator)
        .await
        .expect("wedged creator settles bounded")
        .expect("creator task")
        .expect_err("never-returning build settles cancelled");
    assert_eq!(format!("{error:#}"), "Todo child creation was cancelled");
    assert_eq!(
        service.creation_ticket_count_for_test(),
        0,
        "cancelled creation ticket is removed"
    );
    assert_no_child_artifacts(&recorder.id(), &child_id);
    registration.unregister();
}
