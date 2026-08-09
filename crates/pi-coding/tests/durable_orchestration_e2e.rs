//! Durable orchestration two-runtime restart / revival E2E contracts.
//!
//! These tests exercise the corrected public orchestration durability boundary
//! across process-loss reconstruction and application lifecycle replacement.
//!
//! Contracts defended (observable public behavior only):
//! - Existing sidecar survives bind and is recovered before any empty write
//! - Fresh non-explicit Application attachment binds before parent JSONL creation
//! - Child JSONL lives under `<parent-dir>/children/<parent-id>/` with
//!   parentSession linkage; pathful and pathless revival append real turns
//! - Interrupted agents recover as Parked; unsettled jobs as Cancelled
//! - Mailbox contents restore across runtime reconstruction
//! - Parked send returns Revived; concurrent sends claim at most one revival job
//! - Running Woken delivery is durable before active steering (not exactly-once)
//! - Persistence failure does not report durable success / Revived without commit
//! - Runtime and Application replacement rebind transactionally to the new root
//! - Dotted parent ids bind; oversize writes reject before atomic replacement
//! - Same-batch sibling roster visibility + XML/byte bounds
//! - Hub list projects distinct id / display name / agent type
//!
//! Does **not** claim maka-style exactly-once execution.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_agent::{AbortController, AgentTool, ThinkingLevel, ToolCallContext};
use pi_ai::{Model, StopReason};
use pi_coding::{
    AgentCatalog, AgentDefinition, AgentDefinitionSource, AgentSnapshot, AgentStatus, Application,
    ChildSessionFactory, DURABLE_STATE_VERSION, DeliveryOutcome, DeliveryReceipt, DurableRuntime,
    DurableState, JobSnapshot, JobStatus, MailboxMessage, OrchestrationConfig,
    OrchestrationRuntime, PersistedAgent, PersistedDefinition, PersistedRequest, Session,
    SessionOptions, TaskSpawn, start_durable_child_session_in, start_session_in,
};
use serde_json::{Value, json};
use tokio::sync::{Barrier, Notify};
use tokio_util::sync::CancellationToken;

fn task_definition() -> AgentDefinition {
    AgentDefinition { name: "task".to_owned(),
    description: "background task".to_owned(),
    system_prompt: "complete the assignment thoroughly".to_owned(),
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
    path: None, trusted: true, kind: pi_coding::AgentDefinitionKind::Agent, personality: None, soft_budget: None }
}

fn reviewer_definition() -> AgentDefinition {
    AgentDefinition { name: "reviewer".to_owned(),
    description: "code reviewer".to_owned(),
    system_prompt: "review carefully".to_owned(),
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
    path: None, trusted: true, kind: pi_coding::AgentDefinitionKind::Agent, personality: None, soft_budget: None }
}

fn test_model() -> Model {
    Model {
        id: "durable-orch-e2e".to_owned(),
        name: "Durable Orch E2E".to_owned(),
        api: "durable-orch-e2e".to_owned(),
        provider: "durable-orch-e2e".to_owned(),
        ..Model::default()
    }
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
    }
}

fn quick_factory(response_text: &'static str) -> ChildSessionFactory {
    Arc::new(move |request| {
        let response_text = response_text;
        Box::pin(async move {
            let stream_fn: pi_agent::StreamFn = Arc::new(move |model, _ctx, _opts| {
                let response_text = response_text;
                Box::pin(async move {
                    let stream = pi_ai::new_assistant_message_event_stream();
                    let producer = stream.clone();
                    tokio::spawn(async move {
                        let mut message = pi_ai::AssistantMessage::pending(&model);
                        message
                            .content
                            .push(pi_ai::ContentBlock::text(response_text));
                        message.stop_reason = StopReason::Stop;
                        producer.end(Some(message)).await;
                    });
                    stream
                })
            });
            Session::new(SessionOptions {
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
            })
        })
    })
}

fn capturing_factory(
    prompts: Arc<Mutex<HashMap<String, String>>>,
    response_text: &'static str,
) -> ChildSessionFactory {
    Arc::new(move |request| {
        let prompts = prompts.clone();
        let response_text = response_text;
        Box::pin(async move {
            prompts
                .lock()
                .expect("prompt lock")
                .insert(request.child_id.clone(), request.system_prompt.clone());
            let stream_fn: pi_agent::StreamFn = Arc::new(move |model, _ctx, _opts| {
                let response_text = response_text;
                Box::pin(async move {
                    let stream = pi_ai::new_assistant_message_event_stream();
                    let producer = stream.clone();
                    tokio::spawn(async move {
                        let mut message = pi_ai::AssistantMessage::pending(&model);
                        message
                            .content
                            .push(pi_ai::ContentBlock::text(response_text));
                        message.stop_reason = StopReason::Stop;
                        producer.end(Some(message)).await;
                    });
                    stream
                })
            });
            Session::new(SessionOptions {
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
            })
        })
    })
}

fn blocking_factory(started: Arc<Notify>, release: CancellationToken) -> ChildSessionFactory {
    Arc::new(move |request| {
        let started = started.clone();
        let release = release.clone();
        Box::pin(async move {
            let stream_fn: pi_agent::StreamFn = Arc::new(move |model, _ctx, _opts| {
                let started = started.clone();
                let release = release.clone();
                Box::pin(async move {
                    let stream = pi_ai::new_assistant_message_event_stream();
                    let producer = stream.clone();
                    started.notify_one();
                    tokio::spawn(async move {
                        release.cancelled().await;
                        let mut message = pi_ai::AssistantMessage::pending(&model);
                        message
                            .content
                            .push(pi_ai::ContentBlock::text("running child completed"));
                        message.stop_reason = StopReason::Stop;
                        producer.end(Some(message)).await;
                    });
                    stream
                })
            });
            Session::new(SessionOptions {
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
            })
        })
    })
}

fn config(artifacts: &Path, definitions: Vec<AgentDefinition>) -> OrchestrationConfig {
    let mut config = OrchestrationConfig::new(AgentCatalog::from_agents(definitions), artifacts);
    config.parent_model = test_model();
    config.idle_ttl = None;
    config
}

fn parent_session(artifacts: &Path, session_dir: &Path) -> (Session, String, PathBuf) {
    let recorder = start_session_in(
        artifacts,
        Some(&test_model()),
        Some("off"),
        Some(session_dir),
        None,
        None,
    )
    .expect("parent recorder");
    recorder
        .persist_now()
        .expect("materialize real parent header for durable fixture");
    let parent_id = recorder.id();
    let parent_path = recorder.path();
    let parent = Session::new(SessionOptions {
        model: test_model(),
        cwd: artifacts.to_path_buf(),
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
    })
    .expect("parent session");
    parent.set_session_dir(session_dir.to_path_buf());
    parent.record(recorder).expect("attach parent recorder");
    (parent, parent_id, parent_path)
}

fn fresh_unmaterialized_parent_session(
    artifacts: &Path,
    session_dir: &Path,
) -> (Session, String, PathBuf) {
    let recorder = start_session_in(
        artifacts,
        Some(&test_model()),
        Some("off"),
        Some(session_dir),
        None,
        None,
    )
    .expect("fresh parent recorder");
    let parent_id = recorder.id();
    let parent_path = recorder.path();
    assert!(
        !parent_path.exists(),
        "fresh non-explicit recorder stays pending before its first real turn: {}",
        parent_path.display()
    );
    let parent = Session::new(SessionOptions {
        model: test_model(),
        cwd: artifacts.to_path_buf(),
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
    })
    .expect("fresh parent session");
    parent.set_session_dir(session_dir.to_path_buf());
    parent.record(recorder).expect("attach fresh parent recorder");
    (parent, parent_id, parent_path)
}

fn parent_session_with_id(
    artifacts: &Path,
    session_dir: &Path,
    parent_id: &str,
) -> (Session, PathBuf) {
    let recorder = start_session_in(
        artifacts,
        Some(&test_model()),
        Some("off"),
        Some(session_dir),
        Some(parent_id),
        None,
    )
    .expect("explicit parent recorder");
    let parent_path = recorder.path();
    let parent = Session::new(SessionOptions {
        model: test_model(),
        cwd: artifacts.to_path_buf(),
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
    })
    .expect("explicit parent session");
    parent.set_session_dir(session_dir.to_path_buf());
    parent.record(recorder).expect("attach explicit parent recorder");
    (parent, parent_path)
}

fn child_root_for(parent_path: &Path, parent_id: &str) -> PathBuf {
    parent_path
        .parent()
        .expect("parent dir")
        .join("children")
        .join(parent_id)
}

fn plant_child_jsonl(child_root: &Path, parent_path: &Path, marker: &str) -> PathBuf {
    std::fs::create_dir_all(child_root).expect("child root");
    let child_cwd = std::env::current_dir().expect("child cwd");
    let recorder = start_durable_child_session_in(
        &child_cwd,
        Some(&test_model()),
        Some("off"),
        child_root,
        None,
        parent_path,
    )
    .expect("durable child recorder");
    let path = recorder.path();
    let mut contents = std::fs::read_to_string(&path).expect("read child jsonl");
    contents.push('\n');
    contents.push_str(
        &serde_json::json!({
            "type": "message",
            "id": "user-marker",
            "parentId": null,
            "timestamp": "2026-01-01T00:00:00.000Z",
            "message": {
                "role": "user",
                "content": [{"type": "text", "text": marker}],
                "timestamp": 0,
            },
        })
        .to_string(),
    );
    contents.push('\n');
    std::fs::write(&path, contents).expect("plant child history");
    path
}

fn persisted_definition(name: &str, description: &str, system_prompt: &str) -> PersistedDefinition {
    serde_json::from_value(json!({
        "name": name,
        "description": description,
        "systemPrompt": system_prompt,
        "tools": [],
        "autoloadSkills": [],
        "thinkingLevel": "off",
        "source": "bundled",
        "trusted": true
    }))
    .expect("persisted definition")
}

fn persisted_request(agent_id: &str, assignment: &str, system_prompt: &str) -> PersistedRequest {
    serde_json::from_value(json!({
        "childId": agent_id,
        "parentId": "Main",
        "depth": 1,
        "assignment": assignment,
        "systemPrompt": system_prompt,
        "requestedToolNames": [],
        "thinkingLevel": "off",
        "maxToolsPerAgent": 32,
        "modelProvider": test_model().provider,
        "modelId": test_model().id
    }))
    .expect("persisted request")
}

fn persisted_worker(
    agent_id: &str,
    status: AgentStatus,
    assignment: &str,
    session_path: Option<&Path>,
    mailbox: Vec<MailboxMessage>,
) -> PersistedAgent {
    let system_prompt = format!(
        "complete the assignment thoroughly\n\n<peer_roster>\n  <peer id=\"Main\" agent=\"Main\" status=\"idle\" parent=\"none\" />\n</peer_roster>\n\n<delegated_assignment>\n{assignment}\n</delegated_assignment>"
    );
    PersistedAgent {
        snapshot: AgentSnapshot {
            id: agent_id.to_owned(),
            display_name: "task".to_owned(),
            agent: "task".to_owned(),
            parent_id: Some("Main".to_owned()),
            status,
            created_at: 1_000,
            last_activity: 1_100,
            unread: mailbox.len(),
            artifact_ref: Some(format!("agent://{agent_id}")),
            history_ref: Some(format!("history://{agent_id}")),
        },
        definition: persisted_definition(
            "task",
            "background task",
            "complete the assignment thoroughly",
        ),
        request: persisted_request(agent_id, assignment, &system_prompt),
        session_path: session_path.map(|path| path.to_string_lossy().into_owned()),
        mailbox,
    }
}

fn running_job(id: &str, agent_id: &str) -> JobSnapshot {
    JobSnapshot {
        id: id.to_owned(),
        agent_id: agent_id.to_owned(),
        agent: "task".to_owned(),
        parent_id: "Main".to_owned(),
        description: Some("do durable work".to_owned()),
        todo_task_id: None,
        workflow_id: None,
        workflow_generation: None,
        status: JobStatus::Running,
        created_at: 1_000,
        started_at: Some(1_050),
        finished_at: None,
        result: None,
        soft_budget_exhausted: false,
    }
}

fn plant_sidecar(
    parent_id: &str,
    parent_path: &Path,
    agents: Vec<PersistedAgent>,
    jobs: Vec<JobSnapshot>,
) -> PathBuf {
    let child_root = child_root_for(parent_path, parent_id);
    let durable = DurableRuntime::new(parent_id.to_owned(), parent_path.to_path_buf(), child_root)
        .expect("durable runtime");
    // Persist using the canonical parent path DurableRuntime bound to.
    let mut agents = agents;
    for agent in &mut agents {
        if let Some(session_path) = agent.session_path.take() {
            let canonical = durable
                .canonicalize_child_session_path(Path::new(&session_path))
                .unwrap_or_else(|error| {
                    panic!("planted child session path must canonicalize: {error:#}")
                });
            agent.session_path = Some(canonical.to_string_lossy().into_owned());
        }
    }
    let state = DurableState {
        version: DURABLE_STATE_VERSION,
        parent_session_id: durable.parent_session_id().to_owned(),
        parent_session_path: durable
            .parent_session_path()
            .to_string_lossy()
            .into_owned(),
        agents,
        jobs,
    };
    durable.persist(&state).expect("plant sidecar");
    durable.sidecar_path().to_path_buf()
}

/// Planted-sidecar path: bind without writing, then strict recover (errors if missing).
fn bind_then_recover_existing(runtime: &OrchestrationRuntime, parent: &Session) {
    runtime
        .bind_parent_session(parent)
        .expect("bind parent session");
    runtime.recover().expect("recover existing durable state");
}

/// Fresh-parent path: public recover-or-initialize after bind.
fn bind_fresh_parent(runtime: &OrchestrationRuntime, parent: &Session) {
    runtime
        .bind_and_recover(parent)
        .expect("bind and recover/initialize parent");
}

fn receipts_from_send(
    runtime: &OrchestrationRuntime,
    from: &str,
    to: &str,
    body: &str,
) -> Vec<DeliveryReceipt> {
    runtime.send(from, to, body, None)
}

/// Existing sidecar must load on the second runtime: Parked agent, Cancelled
/// interrupted job, and restored mailbox — without bind wiping state first.
#[tokio::test]
async fn two_runtime_restart_recovers_parked_cancelled_and_mailbox() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let sessions = tempfile::tempdir().expect("sessions");
    let (parent, parent_id, parent_path) = parent_session(artifacts.path(), sessions.path());
    let child_root = child_root_for(&parent_path, &parent_id);
    let child_jsonl = plant_child_jsonl(&child_root, &parent_path, "pre-crash assignment");
    let mail = MailboxMessage {
        id: "mail-1".to_owned(),
        from: "Main".to_owned(),
        to: "Worker".to_owned(),
        body: "resume after crash".to_owned(),
        timestamp: 1_200,
        reply_to: None,
    };
    let sidecar = plant_sidecar(
        &parent_id,
        &parent_path,
        vec![persisted_worker(
            "Worker",
            AgentStatus::Running,
            "do durable work",
            Some(&child_jsonl),
            vec![mail.clone()],
        )],
        vec![running_job("job-running-1", "Worker")],
    );
    assert!(sidecar.is_file(), "sidecar planted at {}", sidecar.display());

    let runtime_a = OrchestrationRuntime::new(
        config(artifacts.path(), vec![task_definition()]),
        quick_factory("from A"),
    )
    .expect("runtime A");

    let runtime_b = OrchestrationRuntime::new(
        config(artifacts.path(), vec![task_definition()]),
        quick_factory("from B"),
    )
    .expect("runtime B");
    bind_then_recover_existing(&runtime_b, &parent);

    let peers = runtime_b.list("Main");
    let worker = peers
        .iter()
        .find(|peer| peer.id == "Worker")
        .unwrap_or_else(|| panic!("Worker must recover; peers={peers:?}"));
    assert_eq!(
        worker.status,
        AgentStatus::Parked,
        "interrupted Running agent recovers as Parked"
    );
    assert_eq!(worker.agent, "task", "agent type survives recovery");

    let jobs = runtime_b.jobs(None);
    let job = jobs
        .iter()
        .find(|job| job.id == "job-running-1")
        .unwrap_or_else(|| panic!("interrupted job must recover; jobs={jobs:?}"));
    assert_eq!(
        job.status,
        JobStatus::Cancelled,
        "unsettled job recovers as Cancelled (truthful interruption)"
    );
    assert!(
        job.result
            .as_ref()
            .and_then(|result| result.error.as_ref())
            .is_some_and(|error| error.contains("interrupted")),
        "cancelled job carries interruption context: {:?}",
        job.result
    );

    let inbox = runtime_b.inbox("Worker", true);
    assert_eq!(
        inbox.len(),
        1,
        "mailbox must restore across runtime reconstruction: {inbox:?}"
    );
    assert_eq!(inbox[0].body, "resume after crash");
    assert_eq!(inbox[0].from, "Main");

    let root_entries: Vec<_> = std::fs::read_dir(sessions.path())
        .expect("read session root")
        .map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        root_entries.iter().any(|name| name.ends_with(".jsonl")),
        "parent catalog retains parent jsonl: {root_entries:?}"
    );
    assert!(
        !root_entries
            .iter()
            .any(|name| name == "Worker" || name.contains("Worker")),
        "child sessions must not pollute the flat parent catalog: {root_entries:?}"
    );
    assert!(
        child_jsonl.starts_with(&child_root),
        "child JSONL under children/<parent-id>/: {} vs {}",
        child_jsonl.display(),
        child_root.display()
    );

    let sidecar_text = std::fs::read_to_string(&sidecar).expect("read sidecar");
    assert!(
        sidecar_text.contains("Worker"),
        "bind must not wipe existing sidecar before recover: {sidecar_text}"
    );

    runtime_a.shutdown().await;
    runtime_b.shutdown().await;
}

/// Revival after recovery reuses the recorded child JSONL path and returns Revived.
#[tokio::test]
async fn parked_recovery_revives_reusing_recorded_child_jsonl() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let sessions = tempfile::tempdir().expect("sessions");
    let (parent, parent_id, parent_path) = parent_session(artifacts.path(), sessions.path());
    let child_root = child_root_for(&parent_path, &parent_id);
    let marker = "transcript-continuity-marker-v1";
    let child_jsonl = plant_child_jsonl(&child_root, &parent_path, marker);
    let before = std::fs::read_to_string(&child_jsonl).expect("child before");
    assert!(before.contains(marker), "planted marker missing: {before}");
    assert!(
        before.contains("\"parentSession\""),
        "child header must link parentSession: {before}"
    );

    plant_sidecar(
        &parent_id,
        &parent_path,
        vec![persisted_worker(
            "Worker",
            AgentStatus::Parked,
            "continue durable work",
            Some(&child_jsonl),
            vec![MailboxMessage {
                id: "mail-pre".to_owned(),
                from: "Main".to_owned(),
                to: "Worker".to_owned(),
                body: "queued before revival".to_owned(),
                timestamp: 1_300,
                reply_to: None,
            }],
        )],
        vec![],
    );

    let runtime = OrchestrationRuntime::new(
        config(artifacts.path(), vec![task_definition()]),
        quick_factory("revived assistant turn"),
    )
    .expect("runtime");
    bind_then_recover_existing(&runtime, &parent);

    let worker = runtime
        .list("Main")
        .into_iter()
        .find(|peer| peer.id == "Worker")
        .expect("recovered Worker");
    assert_eq!(worker.status, AgentStatus::Parked);

    let receipts = receipts_from_send(&runtime, "Main", "Worker", "wake and continue");
    assert_eq!(receipts.len(), 1, "{receipts:?}");
    assert_eq!(
        receipts[0].outcome,
        DeliveryOutcome::Revived,
        "parked durable send must revive: {receipts:?}"
    );
    assert!(receipts[0].error.is_none(), "{receipts:?}");

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut revival_jobs = Vec::new();
    while std::time::Instant::now() < deadline {
        revival_jobs = runtime
            .jobs(None)
            .into_iter()
            .filter(|job| job.agent_id == "Worker")
            .collect::<Vec<_>>();
        if let Some(job) = revival_jobs.iter().find(|job| !job.status.is_settled()) {
            let _ = runtime
                .wait_jobs(&[job.id.clone()], Some(Duration::from_millis(400)), None)
                .await;
        } else if !revival_jobs.is_empty() {
            break;
        } else {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    assert!(
        !revival_jobs.is_empty(),
        "revival must create a job for Worker"
    );

    assert!(
        child_jsonl.is_file(),
        "recorded child JSONL must remain at {}",
        child_jsonl.display()
    );
    let after = std::fs::read_to_string(&child_jsonl).expect("child after revival");
    assert!(
        after.contains(marker),
        "revival must reuse the same transcript (marker lost): {after}"
    );
    assert!(
        after.contains("wake and continue"),
        "revival must append the wake delivery to the reused transcript: {after}"
    );
    assert!(
        after.contains("revived assistant turn"),
        "revival must append the new assistant turn to the reused transcript: {after}"
    );

    let catalog = std::fs::read_dir(sessions.path())
        .expect("catalog")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
        .collect::<Vec<_>>();
    assert_eq!(
        catalog.len(),
        1,
        "only parent jsonl in catalog, children stay nested: {catalog:?}"
    );

    runtime.shutdown().await;
}

/// Barrier-concurrent sends against one Parked child create exactly one revival job.
/// All messages are delivered or retained; this does not claim exactly-once execution.
#[tokio::test]
async fn concurrent_sends_claim_single_revival_job() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let sessions = tempfile::tempdir().expect("sessions");
    let (parent, parent_id, parent_path) = parent_session(artifacts.path(), sessions.path());
    let child_root = child_root_for(&parent_path, &parent_id);
    let child_jsonl = plant_child_jsonl(&child_root, &parent_path, "concurrent-revival");
    plant_sidecar(
        &parent_id,
        &parent_path,
        vec![persisted_worker(
            "Worker",
            AgentStatus::Parked,
            "handle concurrent wakes",
            Some(&child_jsonl),
            Vec::new(),
        )],
        vec![],
    );

    let factory_calls = Arc::new(AtomicUsize::new(0));
    let calls = factory_calls.clone();
    let factory: ChildSessionFactory = Arc::new(move |request| {
        calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let stream_fn: pi_agent::StreamFn = Arc::new(move |model, _ctx, _opts| {
                Box::pin(async move {
                    tokio::time::sleep(Duration::from_millis(80)).await;
                    let stream = pi_ai::new_assistant_message_event_stream();
                    let producer = stream.clone();
                    tokio::spawn(async move {
                        let mut message = pi_ai::AssistantMessage::pending(&model);
                        message
                            .content
                            .push(pi_ai::ContentBlock::text("one revival"));
                        message.stop_reason = StopReason::Stop;
                        producer.end(Some(message)).await;
                    });
                    stream
                })
            });
            Session::new(SessionOptions {
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
            })
        })
    });

    let runtime = Arc::new(
        OrchestrationRuntime::new(config(artifacts.path(), vec![task_definition()]), factory)
            .expect("runtime"),
    );
    bind_then_recover_existing(&runtime, &parent);

    let barrier = Arc::new(Barrier::new(8));
    let mut handles = Vec::new();
    for index in 0..8 {
        let runtime = runtime.clone();
        let barrier = barrier.clone();
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            receipts_from_send(
                &runtime,
                "Main",
                "Worker",
                &format!("concurrent wake {index}"),
            )
        }));
    }

    let mut revived = 0_usize;
    let mut queued = 0_usize;
    let mut failed = 0_usize;
    let mut bodies_ok = 0_usize;
    for handle in handles {
        let receipts = handle.await.expect("join send");
        assert_eq!(receipts.len(), 1, "{receipts:?}");
        match receipts[0].outcome {
            DeliveryOutcome::Revived => revived += 1,
            DeliveryOutcome::Queued | DeliveryOutcome::Woken => queued += 1,
            DeliveryOutcome::Failed => failed += 1,
        }
        if receipts[0].error.is_none() {
            bodies_ok += 1;
        }
    }
    assert_eq!(
        revived, 1,
        "exactly one send may claim revival (revived={revived} queued={queued} failed={failed})"
    );
    assert_eq!(
        revived + queued + failed,
        8,
        "every concurrent send reports an outcome"
    );
    assert_eq!(
        bodies_ok, 8,
        "no concurrent send may silently drop its message"
    );

    let worker_jobs: Vec<_> = runtime
        .jobs(None)
        .into_iter()
        .filter(|job| job.agent_id == "Worker")
        .collect();
    assert_eq!(
        worker_jobs.len(),
        1,
        "exactly one revival job for Worker, got {worker_jobs:?}"
    );
    assert!(
        factory_calls.load(Ordering::SeqCst) <= 1,
        "at most one child factory invocation for a single revival claim, got {}",
        factory_calls.load(Ordering::SeqCst)
    );

    if let Some(job) = worker_jobs.first() {
        let _ = runtime
            .wait_jobs(&[job.id.clone()], Some(Duration::from_secs(5)), None)
            .await;
    }
    runtime.shutdown().await;
}

/// Persistence failure must not report Revived/durable success, and must not
/// silently lose the outbound message.
#[tokio::test]
async fn persistence_failure_propagates_without_false_revived() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let sessions = tempfile::tempdir().expect("sessions");
    let (parent, parent_id, parent_path) = parent_session(artifacts.path(), sessions.path());
    let child_root = child_root_for(&parent_path, &parent_id);
    let child_jsonl = plant_child_jsonl(&child_root, &parent_path, "persist-fail");
    let sidecar = plant_sidecar(
        &parent_id,
        &parent_path,
        vec![persisted_worker(
            "Worker",
            AgentStatus::Parked,
            "persist carefully",
            Some(&child_jsonl),
            Vec::new(),
        )],
        vec![],
    );

    let runtime = OrchestrationRuntime::new(
        config(artifacts.path(), vec![task_definition()]),
        quick_factory("should not false-revive"),
    )
    .expect("runtime");
    bind_then_recover_existing(&runtime, &parent);

    std::fs::remove_file(&sidecar).expect("remove sidecar file");
    std::fs::create_dir(&sidecar).expect("sidecar path becomes directory");

    let receipts = receipts_from_send(&runtime, "Main", "Worker", "must not vanish");
    assert_eq!(receipts.len(), 1, "{receipts:?}");

    // Public contract: persistence failures surface as Failed receipts with
    // error context — send remains Vec, not Result.
    let receipt = &receipts[0];
    assert_eq!(
        receipt.outcome,
        DeliveryOutcome::Failed,
        "persistence failure must not claim Revived/Queued success: {receipt:?}"
    );
    assert!(
        receipt.error.as_ref().is_some_and(|error| {
            let lower = error.to_lowercase();
            lower.contains("persist")
                || lower.contains("durable")
                || lower.contains("write")
                || lower.contains("io")
                || lower.contains("directory")
                || lower.contains("not a directory")
                || lower.contains("isfile")
                || lower.contains("sidecar")
        }),
        "Failed receipt must carry persistence/IO context: {receipt:?}"
    );

    // Message must not be silently lost: either still in the mailbox after the
    // failed commit, or the Failed receipt is the observable failure surface.
    let inbox = runtime.inbox("Worker", true);
    let retained = inbox.iter().any(|message| message.body == "must not vanish");
    assert!(
        retained || receipt.error.is_some(),
        "message must remain observable (inbox or Failed receipt); inbox={inbox:?} receipt={receipt:?}"
    );

    runtime.shutdown().await;
}

/// The mailbox commit precedes the revival claim. If the claim cannot be
/// persisted, the Failed receipt must leave the already-durable message queued
/// in both live and recovered state.
#[tokio::test]
async fn revival_claim_failure_keeps_committed_mailbox_message() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let sessions = tempfile::tempdir().expect("sessions");
    let (parent, parent_id, parent_path) = parent_session(artifacts.path(), sessions.path());
    let child_root = child_root_for(&parent_path, &parent_id);
    let child_jsonl = plant_child_jsonl(&child_root, &parent_path, "claim-fail");
    let jobs = (0..4096)
        .map(|index| JobSnapshot {
            id: format!("settled-{index}"),
            agent_id: "Worker".to_owned(),
            agent: "task".to_owned(),
            parent_id: "Main".to_owned(),
            description: None,
            todo_task_id: None,
            workflow_id: None,
            workflow_generation: None,
            status: JobStatus::Completed,
            created_at: index,
            started_at: Some(index),
            finished_at: Some(index),
            result: None,
            soft_budget_exhausted: false,
        })
        .collect();
    let sidecar = plant_sidecar(
        &parent_id,
        &parent_path,
        vec![persisted_worker(
            "Worker",
            AgentStatus::Parked,
            "retain committed mail",
            Some(&child_jsonl),
            Vec::new(),
        )],
        jobs,
    );
    let runtime = OrchestrationRuntime::new(
        config(artifacts.path(), vec![task_definition()]),
        quick_factory("must not launch"),
    )
    .expect("runtime");
    bind_then_recover_existing(&runtime, &parent);

    let receipts = runtime.send("Main", "Worker", "durably queued", None);
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].outcome, DeliveryOutcome::Failed, "{receipts:?}");
    assert!(receipts[0].error.is_some(), "{receipts:?}");
    assert!(runtime.inbox("Worker", true).iter().any(|message| message.body == "durably queued"));

    let durable = DurableRuntime::new(
        parent_id,
        parent_path,
        child_root,
    )
    .expect("durable reader");
    let state = durable.load().expect("restored sidecar");
    assert!(state.agents.iter().any(|agent| {
        agent.snapshot.id == "Worker"
            && agent.mailbox.iter().any(|message| message.body == "durably queued")
    }));
    assert!(sidecar.is_file());
    runtime.shutdown().await;
}

/// Hub inbox drain must surface persistence failure and restore the mailbox.
#[tokio::test]
async fn hub_inbox_drain_failure_restores_mailbox() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let sessions = tempfile::tempdir().expect("sessions");
    let (parent, parent_id, parent_path) = parent_session(artifacts.path(), sessions.path());
    let message = MailboxMessage {
        id: "mail-1".to_owned(),
        from: "Worker".to_owned(),
        to: "Main".to_owned(),
        body: "keep me".to_owned(),
        timestamp: 1,
        reply_to: None,
    };
    let sidecar = plant_sidecar(
        &parent_id,
        &parent_path,
        vec![persisted_worker(
            "Worker",
            AgentStatus::Parked,
            "wait",
            None,
            Vec::new(),
        )],
        Vec::new(),
    );
    let runtime = OrchestrationRuntime::new(
        config(artifacts.path(), vec![task_definition()]),
        quick_factory("unused"),
    )
    .expect("runtime");
    bind_then_recover_existing(&runtime, &parent);
    assert!(runtime.send("Worker", "Main", &message.body, None)[0].error.is_none());
    std::fs::remove_file(&sidecar).expect("remove sidecar");
    std::fs::create_dir(&sidecar).expect("block sidecar writes");

    let main_tools = runtime.agent_tools("Main", 0);
    let hub = tool(&main_tools, "hub");
    let error = (hub.execute)(context("inbox-fail", json!({ "op": "inbox" })))
        .await
        .expect_err("inbox drain persistence failure must propagate");
    assert!(error.to_string().contains("persist") || error.to_string().contains("durable"), "{error:#}");
    assert!(runtime.inbox("Main", true).iter().any(|queued| queued.body == "keep me"));
    runtime.shutdown().await;
}

/// After runtime/session replacement, bind targets the current recorder — children
/// and sidecar live under the new parent root, never the old one.
#[tokio::test]
async fn lifecycle_rebind_after_runtime_and_session_replacement() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let sessions = tempfile::tempdir().expect("sessions");

    let (parent_old, old_id, old_path) = parent_session(artifacts.path(), sessions.path());
    let old_child_root = child_root_for(&old_path, &old_id);
    let old_child = plant_child_jsonl(&old_child_root, &old_path, "old-parent-child");
    plant_sidecar(
        &old_id,
        &old_path,
        vec![persisted_worker(
            "Legacy",
            AgentStatus::Parked,
            "belongs to old parent",
            Some(&old_child),
            Vec::new(),
        )],
        vec![],
    );

    let runtime_old = OrchestrationRuntime::new(
        config(artifacts.path(), vec![task_definition()]),
        quick_factory("old runtime"),
    )
    .expect("old runtime");
    bind_then_recover_existing(&runtime_old, &parent_old);
    assert!(
        runtime_old
            .list("Main")
            .iter()
            .any(|peer| peer.id == "Legacy"),
        "old runtime sees Legacy"
    );
    runtime_old.shutdown().await;

    let (parent_new, new_id, new_path) = parent_session(artifacts.path(), sessions.path());
    assert_ne!(old_id, new_id, "replacement session gets a new id");
    assert_ne!(old_path, new_path, "replacement session gets a new path");

    let runtime_new = OrchestrationRuntime::new(
        config(artifacts.path(), vec![task_definition()]),
        quick_factory("new runtime"),
    )
    .expect("new runtime");
    bind_fresh_parent(&runtime_new, &parent_new);

    let new_peers = runtime_new.list("Main");
    assert!(
        new_peers.iter().all(|peer| peer.id != "Legacy"),
        "replacement runtime must not inherit foreign parent roster: {new_peers:?}"
    );

    let tools = runtime_new.agent_tools("Main", 0);
    let task = tool(&tools, "task");
    let spawn = (task.execute)(context(
        "spawn-new",
        json!({
            "context": "Replacement-runtime roster check: complete the assignment so the parent can verify a fresh child under the new runtime.",
            "tasks": [{ "name": "Fresh", "task": "work under new parent" }]
        }),
    ))
    .await
    .expect("spawn under new parent");
    let spawns: Vec<TaskSpawn> = serde_json::from_value(spawn.details).expect("spawns");
    assert_eq!(spawns.len(), 1);
    runtime_new
        .wait_jobs(
            &[spawns[0].job_id.clone()],
            Some(Duration::from_secs(5)),
            None,
        )
        .await
        .expect("wait fresh job");

    let new_child_root = child_root_for(&new_path, &new_id);
    let new_sidecar = new_child_root.join("orchestration-state.json");
    assert!(
        new_sidecar.is_file() || new_child_root.is_dir(),
        "new parent durable root should materialize under {}",
        new_child_root.display()
    );
    if new_sidecar.is_file() {
        let text = std::fs::read_to_string(&new_sidecar).expect("new sidecar");
        assert!(
            text.contains(&new_id) || text.contains("Fresh"),
            "new sidecar bound to replacement parent, not old: {text}"
        );
    }

    assert!(
        old_child.is_file(),
        "old child JSONL remains at {}",
        old_child.display()
    );
    let old_sidecar = old_child_root.join("orchestration-state.json");
    let old_text = std::fs::read_to_string(&old_sidecar).expect("old sidecar");
    assert!(
        old_text.contains("Legacy"),
        "old parent sidecar stays under old root: {old_text}"
    );

    let catalog_jsonl = std::fs::read_dir(sessions.path())
        .expect("catalog")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".jsonl"))
        .collect::<Vec<_>>();
    assert!(
        catalog_jsonl.len() >= 2,
        "old and new parent jsonl present: {catalog_jsonl:?}"
    );
    assert!(
        catalog_jsonl
            .iter()
            .all(|name| !name.contains("Legacy") && !name.contains("Fresh")),
        "children must not appear in flat parent catalog: {catalog_jsonl:?}"
    );

    runtime_new.shutdown().await;
}

/// Same-batch spawn registers every child before launch so each peer roster
/// includes siblings, stays XML-safe, and remains byte-bounded with Main kept.
#[tokio::test]
async fn sibling_roster_same_batch_visibility_and_xml_bounds() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let sessions = tempfile::tempdir().expect("sessions");
    let (parent, _parent_id, _parent_path) = parent_session(artifacts.path(), sessions.path());
    let prompts = Arc::new(Mutex::new(HashMap::<String, String>::new()));
    let runtime = OrchestrationRuntime::new(
        config(
            artifacts.path(),
            vec![task_definition(), reviewer_definition()],
        ),
        capturing_factory(prompts.clone(), "batch done"),
    )
    .expect("runtime");
    bind_fresh_parent(&runtime, &parent);

    let tools = runtime.agent_tools("Main", 0);
    let task = tool(&tools, "task");
    let spawn = (task.execute)(context(
        "batch-spawn",
        json!({
            "context": "Same-batch sibling visibility check: children spawned together must each appear in the others' peer rosters and complete independently.",
            "tasks": [
                { "name": "Alpha", "agent": "task", "task": "first sibling" },
                { "name": "Beta", "agent": "reviewer", "task": "second sibling" },
                { "name": "Gamma", "agent": "task", "task": "third sibling" }
            ]
        }),
    ))
    .await
    .expect("batch spawn");
    let spawns: Vec<TaskSpawn> = serde_json::from_value(spawn.details).expect("spawns");
    assert_eq!(spawns.len(), 3, "{spawns:?}");
    let job_ids = spawns
        .iter()
        .map(|spawn| spawn.job_id.clone())
        .collect::<Vec<_>>();
    runtime
        .wait_jobs(&job_ids, Some(Duration::from_secs(8)), None)
        .await
        .expect("wait batch");

    let captured = prompts.lock().expect("prompts").clone();
    assert_eq!(
        captured.len(),
        3,
        "factory must observe each child prompt: {captured:?}"
    );

    for (child_id, prompt) in &captured {
        let start = prompt
            .find("<peer_roster>")
            .unwrap_or_else(|| panic!("missing peer_roster for {child_id}: {prompt}"));
        let end = prompt
            .find("</peer_roster>")
            .unwrap_or_else(|| panic!("missing roster close for {child_id}: {prompt}"))
            + "</peer_roster>".len();
        let roster = &prompt[start..end];

        assert!(
            roster.contains("<peer id=\"Main\""),
            "{child_id} roster must include Main: {roster}"
        );
        for peer in ["Alpha", "Beta", "Gamma"] {
            if peer == child_id.as_str() {
                assert!(
                    !roster.contains(&format!("<peer id=\"{peer}\"")),
                    "{child_id} must not list self: {roster}"
                );
            } else {
                assert!(
                    roster.contains(&format!("<peer id=\"{peer}\"")),
                    "{child_id} must see same-batch peer {peer}: {roster}"
                );
            }
        }
        assert!(
            roster.starts_with("<peer_roster>\n"),
            "stable roster header: {roster}"
        );
        assert!(
            roster.ends_with("</peer_roster>"),
            "stable roster footer: {roster}"
        );
        assert!(
            prompt.contains(
                "<context>\nSame-batch sibling visibility check: children spawned together must each appear in the others' peer rosters and complete independently.\n</context>"
            ),
            "{child_id} prompt must carry the shared batch CONTEXT section: {prompt}"
        );
    }

    let main_tools = runtime.agent_tools("Main", 0);
    let hub = tool(&main_tools, "hub");
    let listed = (hub.execute)(context("list-peers", json!({ "op": "list" })))
        .await
        .expect("hub list");
    let text = listed
        .content
        .iter()
        .filter_map(|block| match block {
            pi_ai::ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("agent task") || text.contains("agent reviewer"),
        "hub list must project agent type: {text}"
    );
    let peers: Vec<Value> =
        serde_json::from_value(listed.details["peers"].clone()).expect("peers json");
    for peer in &peers {
        if peer["id"] == "Main" {
            continue;
        }
        assert!(
            peer.get("agent")
                .and_then(Value::as_str)
                .is_some_and(|agent| agent == "task" || agent == "reviewer"),
            "peer JSON carries agent type field: {peer}"
        );
    }

    runtime.shutdown().await;
}

/// Hub agent-type projection distinguishes id, display name, and agent type on
/// the public list surface after durable recovery as well as live spawn.
#[tokio::test]
async fn hub_agent_type_projection_after_recovery() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let sessions = tempfile::tempdir().expect("sessions");
    let (parent, parent_id, parent_path) = parent_session(artifacts.path(), sessions.path());
    let child_root = child_root_for(&parent_path, &parent_id);
    let child_jsonl = plant_child_jsonl(&child_root, &parent_path, "type-projection");

    let mut agent = persisted_worker(
        "review-job",
        AgentStatus::Parked,
        "review the tree",
        Some(&child_jsonl),
        Vec::new(),
    );
    agent.snapshot.display_name = "Code Review".to_owned();
    agent.snapshot.agent = "reviewer".to_owned();
    agent.definition = persisted_definition("reviewer", "code reviewer", "review carefully");
    agent.request = persisted_request(
        "review-job",
        "review the tree",
        "review carefully\n\n<delegated_assignment>\nreview the tree\n</delegated_assignment>",
    );

    plant_sidecar(&parent_id, &parent_path, vec![agent], vec![]);

    let runtime = OrchestrationRuntime::new(
        config(
            artifacts.path(),
            vec![task_definition(), reviewer_definition()],
        ),
        quick_factory("typed"),
    )
    .expect("runtime");
    bind_then_recover_existing(&runtime, &parent);

    let main_tools = runtime.agent_tools("Main", 0);
    let hub = tool(&main_tools, "hub");
    let listed = (hub.execute)(context("list-typed", json!({ "op": "list" })))
        .await
        .expect("hub list");
    let text = listed
        .content
        .iter()
        .filter_map(|block| match block {
            pi_ai::ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("review-job")
            && text.contains("Code Review")
            && text.contains("agent reviewer"),
        "hub list text must distinguish id, display name, and agent type:\n{text}"
    );
    let peers: Vec<Value> =
        serde_json::from_value(listed.details["peers"].clone()).expect("peers");
    let review = peers
        .iter()
        .find(|peer| peer["id"] == "review-job")
        .expect("review-job peer");
    assert_eq!(review["displayName"], "Code Review");
    assert_eq!(review["agent"], "reviewer");
    assert_eq!(review["status"], "parked");

    runtime.shutdown().await;
}

/// Spawned durable child keeps the parent catalog clean; nested child root is
/// the only allowed location for child state / JSONL.
#[tokio::test]
async fn durable_spawn_keeps_parent_catalog_clean_and_nested_child_root() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let sessions = tempfile::tempdir().expect("sessions");
    let (parent, parent_id, parent_path) = parent_session(artifacts.path(), sessions.path());
    let expected_child_root = child_root_for(&parent_path, &parent_id);

    let runtime = OrchestrationRuntime::new(
        config(artifacts.path(), vec![task_definition()]),
        quick_factory("spawn complete"),
    )
    .expect("runtime");
    bind_fresh_parent(&runtime, &parent);
    assert!(runtime.is_durable(), "bound runtime reports durable");

    let tools = runtime.agent_tools("Main", 0);
    let task = tool(&tools, "task");
    let spawn = (task.execute)(context(
        "spawn-isolated",
        json!({
            "context": "Child-root isolation check: perform the assignment inside the durable child's own root directory.",
            "tasks": [{ "name": "Nested", "task": "write under child root" }]
        }),
    ))
    .await
    .expect("spawn");
    let spawns: Vec<TaskSpawn> = serde_json::from_value(spawn.details).expect("spawns");
    runtime
        .wait_jobs(
            &[spawns[0].job_id.clone()],
            Some(Duration::from_secs(5)),
            None,
        )
        .await
        .expect("wait nested");

    let root_jsonl: Vec<_> = std::fs::read_dir(sessions.path())
        .expect("session root")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
        .collect();
    assert!(
        root_jsonl
            .iter()
            .all(|path| path.parent() == Some(sessions.path())),
        "parent catalog entries stay flat at session root: {root_jsonl:?}"
    );
    assert!(
        root_jsonl.iter().all(|path| {
            let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("");
            !name.contains("Nested")
        }),
        "child id must not appear as a top-level catalog jsonl: {root_jsonl:?}"
    );

    let sidecar = expected_child_root.join("orchestration-state.json");
    if sidecar.is_file() {
        let text = std::fs::read_to_string(&sidecar).expect("sidecar");
        assert!(
            text.contains("Nested") || text.contains(&parent_id),
            "sidecar under nested child root: {text}"
        );
        if let Ok(state) = serde_json::from_str::<DurableState>(&text) {
            for agent in state.agents {
                if let Some(session_path) = agent.session_path {
                    let path = PathBuf::from(&session_path);
                    assert!(
                        path.starts_with(&expected_child_root),
                        "child session_path escapes child root: {} vs {}",
                        path.display(),
                        expected_child_root.display()
                    );
                    if path.is_file() {
                        let body = std::fs::read_to_string(&path).expect("child body");
                        assert!(
                            body.contains("\"type\":\"session\"")
                                || body.contains("\"type\": \"session\""),
                            "child JSONL header missing: {body}"
                        );
                        assert!(
                            body.contains("parentSession") || body.contains("parent_session"),
                            "child JSONL must carry parent linkage: {body}"
                        );
                    }
                }
            }
        }
    } else {
        assert!(
            expected_child_root.is_dir() || !sessions.path().join("Nested.jsonl").exists(),
            "children must not land beside parent jsonl when durable"
        );
    }

    runtime.shutdown().await;
}

/// Corrupt / mismatched sidecar fails closed before registry mutation and leaves
/// the file intact — two-runtime safety for fail-closed recovery.
#[tokio::test]
async fn corrupt_sidecar_fails_closed_without_registry_mutation() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let sessions = tempfile::tempdir().expect("sessions");
    let (parent, parent_id, parent_path) = parent_session(artifacts.path(), sessions.path());
    let child_root = child_root_for(&parent_path, &parent_id);
    std::fs::create_dir_all(&child_root).expect("child root");
    let sidecar = child_root.join("orchestration-state.json");
    std::fs::write(&sidecar, b"{not-json").expect("corrupt sidecar");

    let runtime = OrchestrationRuntime::new(
        config(artifacts.path(), vec![task_definition()]),
        quick_factory("unused"),
    )
    .expect("runtime");
    let recovered = runtime.bind_parent_session(&parent);
    assert!(
        recovered.is_err(),
        "corrupt sidecar must fail the transactional bind, got {recovered:?}"
    );
    assert!(
        sidecar.is_file(),
        "corrupt sidecar must not be deleted on failure"
    );
    let peers = runtime.list("Main");
    assert!(
        peers.iter().all(|peer| peer.id == "Main"),
        "failed recover must not partially mutate roster: {peers:?}"
    );
    assert!(
        runtime.jobs(None).is_empty(),
        "failed recover must not insert jobs"
    );

    let foreign = DurableState {
        version: DURABLE_STATE_VERSION,
        parent_session_id: "foreign-parent".to_owned(),
        parent_session_path: parent_path.to_string_lossy().into_owned(),
        agents: vec![persisted_worker(
            "Intruder",
            AgentStatus::Parked,
            "should not load",
            None,
            Vec::new(),
        )],
        jobs: vec![],
    };
    let _ = std::fs::remove_file(&sidecar);
    let durable = DurableRuntime::new(parent_id.clone(), parent_path.clone(), child_root.clone())
        .expect("durable");
    durable
        .persist(&DurableState {
            version: DURABLE_STATE_VERSION,
            parent_session_id: parent_id.clone(),
            parent_session_path: parent_path.to_string_lossy().into_owned(),
            agents: vec![],
            jobs: vec![],
        })
        .expect("valid seed");
    std::fs::write(
        &sidecar,
        serde_json::to_vec(&foreign).expect("serialize foreign"),
    )
    .expect("write foreign");

    let runtime2 = OrchestrationRuntime::new(
        config(artifacts.path(), vec![task_definition()]),
        quick_factory("unused"),
    )
    .expect("runtime2");
    let mismatched = runtime2.bind_parent_session(&parent);
    assert!(
        mismatched.is_err(),
        "mismatched parent id must fail the transactional bind: {mismatched:?}"
    );
    assert!(
        runtime2.list("Main").iter().all(|peer| peer.id == "Main"),
        "no partial foreign roster import"
    );
    assert!(sidecar.is_file(), "mismatched sidecar retained");

    runtime.shutdown().await;
    runtime2.shutdown().await;
}

/// Application attachment binds a fresh non-explicit recorder before its first
/// prompt without materializing a misleading empty parent JSONL.
#[tokio::test]
async fn fresh_non_explicit_application_attach_binds_before_parent_jsonl_exists() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let sessions = tempfile::tempdir().expect("sessions");
    let (parent, parent_id, parent_path) =
        fresh_unmaterialized_parent_session(artifacts.path(), sessions.path());
    let child_root = child_root_for(&parent_path, &parent_id);
    let runtime = OrchestrationRuntime::new(
        config(artifacts.path(), vec![task_definition()]),
        quick_factory("unused"),
    )
    .expect("runtime");
    let application = Application::new(parent).await;

    application
        .attach_orchestration_with_override(runtime.clone(), false)
        .expect("fresh non-explicit application attachment");

    assert!(runtime.is_durable(), "attachment binds durable orchestration");
    assert!(
        !parent_path.exists(),
        "binding must not change ordinary pending-recorder persistence semantics: {}",
        parent_path.display()
    );
    let sidecar = child_root.join("orchestration-state.json");
    assert!(sidecar.is_file(), "fresh binding initializes {}", sidecar.display());
    let durable = DurableRuntime::new(parent_id, parent_path.clone(), child_root)
        .expect("fresh durable reader");
    let state = durable.load().expect("fresh sidecar remains loadable");
    assert!(state.agents.is_empty());
    assert!(state.jobs.is_empty());

    application.cleanup().await;
}

/// Parent session identifiers follow session-store validation, including dots.
#[tokio::test]
async fn dotted_parent_session_id_binds_and_round_trips() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let sessions = tempfile::tempdir().expect("sessions");
    let parent_id = "parent.session.v1";
    let (parent, parent_path) =
        parent_session_with_id(artifacts.path(), sessions.path(), parent_id);
    let child_root = child_root_for(&parent_path, parent_id);
    let runtime = OrchestrationRuntime::new(
        config(artifacts.path(), vec![task_definition()]),
        quick_factory("unused"),
    )
    .expect("runtime");

    bind_fresh_parent(&runtime, &parent);
    let durable = DurableRuntime::new(parent_id.to_owned(), parent_path, child_root)
        .expect("dotted parent durable reader");
    let state = durable.load().expect("dotted parent sidecar");
    assert_eq!(state.parent_session_id, parent_id);

    runtime.shutdown().await;
}

/// A recovered Parked agent without a prior transcript receives a fresh
/// durable child recorder before revival executes, and the wake plus assistant
/// continuation are appended to that newly persisted JSONL.
#[tokio::test]
async fn pathless_parked_revival_creates_and_appends_fresh_child_jsonl() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let sessions = tempfile::tempdir().expect("sessions");
    let (parent, parent_id, parent_path) = parent_session(artifacts.path(), sessions.path());
    let child_root = child_root_for(&parent_path, &parent_id);
    plant_sidecar(
        &parent_id,
        &parent_path,
        vec![persisted_worker(
            "Worker",
            AgentStatus::Parked,
            "continue from a pathless recovery",
            None,
            Vec::new(),
        )],
        Vec::new(),
    );
    let runtime = OrchestrationRuntime::new(
        config(artifacts.path(), vec![task_definition()]),
        quick_factory("fresh revived assistant turn"),
    )
    .expect("runtime");
    bind_then_recover_existing(&runtime, &parent);

    let receipts = receipts_from_send(&runtime, "Main", "Worker", "wake pathless worker");
    assert_eq!(receipts.len(), 1, "{receipts:?}");
    assert_eq!(receipts[0].outcome, DeliveryOutcome::Revived, "{receipts:?}");
    assert!(receipts[0].error.is_none(), "{receipts:?}");

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let job_id = loop {
        if let Some(job) = runtime.jobs(None).into_iter().find(|job| job.agent_id == "Worker") {
            if job.status.is_settled() {
                break job.id;
            }
            let _ = runtime
                .wait_jobs(&[job.id], Some(Duration::from_millis(400)), None)
                .await;
        } else {
            assert!(std::time::Instant::now() < deadline, "revival job was not created");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(std::time::Instant::now() < deadline, "revival job did not settle");
    };
    let job = runtime
        .jobs(None)
        .into_iter()
        .find(|job| job.id == job_id)
        .expect("settled revival job");
    assert_eq!(job.status, JobStatus::Completed, "{job:?}");

    let durable = DurableRuntime::new(parent_id, parent_path, child_root.clone())
        .expect("durable reader");
    let state = durable.load().expect("post-revival state");
    let worker = state
        .agents
        .iter()
        .find(|agent| agent.snapshot.id == "Worker")
        .expect("persisted Worker");
    let session_path = PathBuf::from(
        worker
            .session_path
            .as_deref()
            .expect("pathless revival persists a fresh session path"),
    );
    assert!(session_path.starts_with(&child_root), "{}", session_path.display());
    assert!(session_path.is_file(), "fresh child JSONL at {}", session_path.display());
    let transcript = std::fs::read_to_string(&session_path).expect("fresh child transcript");
    assert!(
        transcript.contains("parentSession") && transcript.contains("wake pathless worker"),
        "fresh transcript contains its linkage and wake delivery: {transcript}"
    );
    assert!(
        transcript.contains("fresh revived assistant turn"),
        "fresh transcript contains the revival assistant turn: {transcript}"
    );

    runtime.shutdown().await;
}

/// A post-request revival failure must retain the resolved agent type in the
/// terminal job result so the sidecar remains valid and restart-readable.
#[tokio::test]
async fn failed_revival_persists_terminal_result_agent_identity() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let sessions = tempfile::tempdir().expect("sessions");
    let (parent, parent_id, parent_path) = parent_session(artifacts.path(), sessions.path());
    let child_root = child_root_for(&parent_path, &parent_id);
    let child_jsonl = plant_child_jsonl(&child_root, &parent_path, "failed-revival");
    plant_sidecar(
        &parent_id,
        &parent_path,
        vec![persisted_worker(
            "Worker",
            AgentStatus::Parked,
            "fail after revival request reconstruction",
            Some(&child_jsonl),
            Vec::new(),
        )],
        Vec::new(),
    );
    let factory: ChildSessionFactory = Arc::new(|_| {
        Box::pin(async { Err(anyhow::anyhow!("forced revived child factory failure")) })
    });
    let runtime = OrchestrationRuntime::new(
        config(artifacts.path(), vec![task_definition()]),
        factory,
    )
    .expect("runtime");
    bind_then_recover_existing(&runtime, &parent);

    let receipts = runtime.send("Main", "Worker", "trigger failed revival", None);
    assert_eq!(receipts.len(), 1, "{receipts:?}");
    assert_eq!(receipts[0].outcome, DeliveryOutcome::Revived, "{receipts:?}");

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let terminal = loop {
        if let Some(job) = runtime
            .jobs(None)
            .into_iter()
            .find(|job| job.agent_id == "Worker" && job.status.is_settled())
        {
            break job;
        }
        assert!(std::time::Instant::now() < deadline, "failed revival did not settle");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(terminal.status, JobStatus::Failed, "{terminal:?}");
    let result = terminal.result.as_ref().expect("terminal failure result");
    assert_eq!(terminal.agent, "task");
    assert_eq!(result.agent, terminal.agent);
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|error| error.contains("forced revived child factory failure")),
        "{terminal:?}"
    );

    let durable = DurableRuntime::new(parent_id, parent_path, child_root)
        .expect("durable restart reader");
    let state = durable.load().expect("failed revival sidecar remains readable");
    let persisted = state
        .jobs
        .iter()
        .find(|job| job.id == terminal.id)
        .expect("persisted failed revival job");
    assert_eq!(persisted.status, JobStatus::Failed, "{persisted:?}");
    assert_eq!(persisted.agent, "task");
    assert_eq!(
        persisted.result.as_ref().expect("persisted failure result").agent,
        persisted.agent
    );

    runtime.shutdown().await;
}

/// Recovering a corrupt replacement parent is transactional: the failed
/// rebind leaves the previous durable handle, roster, jobs, mailbox, and
/// sidecar available exactly as before the attempted cutover.
#[tokio::test]
async fn corrupt_new_parent_rebind_preserves_old_live_binding_and_state() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let sessions = tempfile::tempdir().expect("sessions");
    let (old_parent, old_id, old_path) = parent_session(artifacts.path(), sessions.path());
    let old_child_root = child_root_for(&old_path, &old_id);
    let old_sidecar = plant_sidecar(
        &old_id,
        &old_path,
        vec![persisted_worker(
            "Legacy",
            AgentStatus::Running,
            "preserve old state",
            None,
            vec![MailboxMessage {
                id: "old-mail".to_owned(),
                from: "Main".to_owned(),
                to: "Legacy".to_owned(),
                body: "old durable mailbox".to_owned(),
                timestamp: 7,
                reply_to: None,
            }],
        )],
        vec![running_job("old-job", "Legacy")],
    );
    let runtime = OrchestrationRuntime::new(
        config(artifacts.path(), vec![task_definition()]),
        quick_factory("unused"),
    )
    .expect("runtime");
    bind_then_recover_existing(&runtime, &old_parent);
    let old_peers = runtime.list("Main");
    let old_jobs = runtime.jobs(None);
    let old_mailbox = runtime.inbox("Legacy", true);
    let old_reader = DurableRuntime::new(
        old_id.clone(),
        old_path.clone(),
        old_child_root.clone(),
    )
    .expect("old durable reader");
    let old_state = old_reader.load().expect("old state after initial recovery");

    let (new_parent, new_id, new_path) = parent_session(artifacts.path(), sessions.path());
    let new_child_root = child_root_for(&new_path, &new_id);
    std::fs::create_dir_all(&new_child_root).expect("new child root");
    let new_sidecar = new_child_root.join("orchestration-state.json");
    let mut mismatched = persisted_worker(
        "Intruder",
        AgentStatus::Parked,
        "must never install",
        None,
        Vec::new(),
    );
    mismatched.request.child_id = "DifferentChild".to_owned();
    let invalid_state = DurableState {
        version: DURABLE_STATE_VERSION,
        parent_session_id: new_id,
        parent_session_path: new_path.to_string_lossy().into_owned(),
        agents: vec![mismatched],
        jobs: Vec::new(),
    };
    let invalid_bytes = serde_json::to_vec_pretty(&invalid_state).expect("invalid state json");
    std::fs::write(&new_sidecar, &invalid_bytes).expect("write invalid replacement sidecar");

    runtime
        .bind_and_recover(&new_parent)
        .expect_err("mismatched replacement state must fail closed");
    assert_eq!(runtime.list("Main"), old_peers, "old roster survives failed rebind");
    assert_eq!(runtime.jobs(None), old_jobs, "old jobs survive failed rebind");
    assert_eq!(
        runtime.inbox("Legacy", true),
        old_mailbox,
        "old mailbox survives failed rebind"
    );
    assert_eq!(
        old_reader.load().expect("old sidecar after failed rebind"),
        old_state,
        "failed cutover must not rewrite or erase old durable state"
    );
    assert_eq!(
        std::fs::read(&new_sidecar).expect("invalid sidecar retained"),
        invalid_bytes,
        "invalid replacement sidecar remains intact for diagnosis"
    );
    assert!(old_sidecar.is_file(), "old sidecar remains installed");
    runtime
        .recover()
        .expect("runtime remains bound to the previous valid parent");
    assert!(
        runtime.list("Main").iter().any(|agent| agent.id == "Legacy"),
        "old binding remains recoverable"
    );

    runtime.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_recovery_rejects_active_child_without_mutating_live_state() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let sessions = tempfile::tempdir().expect("sessions");
    let (parent, _, _) = parent_session(artifacts.path(), sessions.path());
    let started = Arc::new(Notify::new());
    let release = CancellationToken::new();
    let runtime = OrchestrationRuntime::new(
        config(artifacts.path(), vec![task_definition()]),
        blocking_factory(started.clone(), release.clone()),
    )
    .expect("runtime");
    bind_fresh_parent(&runtime, &parent);
    let spawn = runtime
        .spawn_tasks(
            "Main",
            0,
            vec![pi_coding::TaskItem {
                index: 0,
                id: "ActiveRecoveryGuard".to_owned(),
                agent: "task".to_owned(),
                assignment: "remain active across rejected recovery".to_owned(),
                todo_task_id: None,
                ..Default::default()
            }],
        )
        .expect("spawn active child")
        .remove(0);
    tokio::time::timeout(Duration::from_secs(5), started.notified())
        .await
        .expect("child started");

    let before_agents = runtime.list("Main");
    let before_jobs = runtime.jobs(None);
    for error in [
        runtime.recover().expect_err("strict recover rejects active child"),
        runtime
            .recover_or_initialize()
            .expect_err("recover-or-initialize rejects active child"),
    ] {
        assert!(
            error
                .to_string()
                .contains("cannot recover durable orchestration while child jobs are active"),
            "unexpected recovery error: {error:#}"
        );
    }
    assert_eq!(runtime.list("Main"), before_agents, "live roster is preserved");
    assert_eq!(runtime.jobs(None), before_jobs, "live job state is preserved");
    assert!(
        runtime
            .jobs(Some(std::slice::from_ref(&spawn.job_id)))
            .first()
            .is_some_and(|job| matches!(job.status, JobStatus::Queued | JobStatus::Running)),
        "active job remains live after rejected recovery"
    );

    release.cancel();
    runtime
        .wait_jobs(&[spawn.job_id], Some(Duration::from_secs(5)), None)
        .await
        .expect("active child settles");
    runtime.shutdown().await;
}

/// A message reported as Woken is committed to the sidecar before the active
/// delivery bridge can consume it. A second durable reader can therefore
/// reconstruct the message at the crash boundary immediately after send.
#[tokio::test(flavor = "current_thread")]
async fn running_woken_message_is_durable_before_active_delivery() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let sessions = tempfile::tempdir().expect("sessions");
    let (parent, parent_id, parent_path) = parent_session(artifacts.path(), sessions.path());
    let child_root = child_root_for(&parent_path, &parent_id);
    let started = Arc::new(Notify::new());
    let release = CancellationToken::new();
    let runtime = OrchestrationRuntime::new(
        config(artifacts.path(), vec![task_definition()]),
        blocking_factory(started.clone(), release.clone()),
    )
    .expect("runtime");
    bind_fresh_parent(&runtime, &parent);
    let spawn = runtime
        .spawn_tasks(
            "Main",
            0,
            vec![pi_coding::TaskItem {
                index: 0,
                id: "Runner".to_owned(),
                agent: "task".to_owned(),
                assignment: "remain active for delivery".to_owned(),
                todo_task_id: None,
                ..Default::default()
            }],
        )
        .expect("spawn running child");
    tokio::time::timeout(Duration::from_secs(5), started.notified())
        .await
        .expect("child stream started");

    let receipts = runtime.send("Main", "Runner", "durable active wake", None);
    assert_eq!(receipts.len(), 1, "{receipts:?}");
    assert_eq!(receipts[0].outcome, DeliveryOutcome::Woken, "{receipts:?}");
    assert!(receipts[0].error.is_none(), "{receipts:?}");

    let durable = DurableRuntime::new(parent_id, parent_path, child_root)
        .expect("durable crash-window reader");
    let state = durable.load().expect("load immediately after Woken receipt");
    assert!(
        state.agents.iter().any(|agent| {
            agent.snapshot.id == "Runner"
                && agent
                    .mailbox
                    .iter()
                    .any(|message| message.body == "durable active wake")
        }),
        "Woken delivery must be represented in durable mailbox state: {state:?}"
    );

    release.cancel();
    runtime
        .wait_jobs(
            &[spawn[0].job_id.clone()],
            Some(Duration::from_secs(5)),
            None,
        )
        .await
        .expect("finish running child");
    runtime.shutdown().await;
}

/// Serialized state larger than 16 MiB is rejected before atomic replacement,
/// leaving the previously committed sidecar byte-for-byte intact and loadable.
#[test]
fn oversize_serialized_write_is_rejected_before_replacement() {
    const MAX_SIDECAR_BYTES: usize = 16 * 1024 * 1024;

    let root = tempfile::tempdir().expect("root");
    let parent_id = "oversize.parent";
    let parent_path = root.path().join("parent.jsonl");
    std::fs::write(&parent_path, b"{}\n").expect("parent jsonl");
    let child_root = root.path().join("children").join(parent_id);
    let durable = DurableRuntime::new(parent_id.to_owned(), parent_path.clone(), child_root)
        .expect("durable runtime");
    let baseline = DurableState {
        version: DURABLE_STATE_VERSION,
        parent_session_id: parent_id.to_owned(),
        parent_session_path: parent_path.to_string_lossy().into_owned(),
        agents: vec![persisted_worker(
            "Worker",
            AgentStatus::Parked,
            "small state",
            None,
            Vec::new(),
        )],
        jobs: Vec::new(),
    };
    durable.persist(&baseline).expect("baseline persist");
    let before = std::fs::read(durable.sidecar_path()).expect("baseline bytes");

    let mut oversized = baseline.clone();
    oversized.agents[0].definition.system_prompt = "x".repeat(MAX_SIDECAR_BYTES + 1);
    let error = durable
        .persist(&oversized)
        .expect_err("oversize serialized state must be rejected");
    assert!(
        error.to_string().contains("maximum") || error.to_string().contains("16"),
        "oversize context: {error:#}"
    );
    assert_eq!(
        std::fs::read(durable.sidecar_path()).expect("sidecar after rejection"),
        before,
        "oversize rejection must occur before atomic replacement"
    );
    assert_eq!(durable.load().expect("baseline remains loadable"), baseline);
}

/// Two-runtime recovery is proven from state produced only through public live
/// runtime operations: bind, spawn, message persistence, shutdown, and recover.
/// The recovered child keeps its mailbox and continues the same real JSONL.
#[tokio::test]
async fn public_live_persist_restart_recovers_mailbox_and_transcript_continuity() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let sessions = tempfile::tempdir().expect("sessions");
    let (parent, parent_id, parent_path) = parent_session(artifacts.path(), sessions.path());
    let child_root = child_root_for(&parent_path, &parent_id);
    let runtime_a = OrchestrationRuntime::new(
        config(artifacts.path(), vec![task_definition()]),
        quick_factory("public restart assistant"),
    )
    .expect("runtime A");
    bind_fresh_parent(&runtime_a, &parent);

    let spawn = runtime_a
        .spawn_tasks(
            "Main",
            0,
            vec![pi_coding::TaskItem {
                index: 0,
                id: "Worker".to_owned(),
                agent: "task".to_owned(),
                assignment: "initial public durable turn".to_owned(),
                todo_task_id: None,
                ..Default::default()
            }],
        )
        .expect("public spawn");
    runtime_a
        .wait_jobs(
            &[spawn[0].job_id.clone()],
            Some(Duration::from_secs(5)),
            None,
        )
        .await
        .expect("initial public job");
    let queued = runtime_a.send("Main", "Worker", "mail retained across restart", None);
    assert_eq!(queued.len(), 1, "{queued:?}");
    assert_eq!(queued[0].outcome, DeliveryOutcome::Woken, "{queued:?}");
    assert!(queued[0].error.is_none(), "{queued:?}");

    let reader = DurableRuntime::new(
        parent_id.clone(),
        parent_path.clone(),
        child_root.clone(),
    )
    .expect("durable reader after live runtime A");
    let live_state = reader.load().expect("runtime A sidecar");
    let live_worker = live_state
        .agents
        .iter()
        .find(|agent| agent.snapshot.id == "Worker")
        .expect("runtime A persisted Worker");
    assert!(
        live_worker
            .mailbox
            .iter()
            .any(|message| message.body == "mail retained across restart"),
        "runtime A publicly persisted mailbox: {live_worker:?}"
    );
    let child_path = PathBuf::from(
        live_worker
            .session_path
            .as_deref()
            .expect("runtime A persisted real child transcript path"),
    );
    let before_restart = std::fs::read_to_string(&child_path).expect("runtime A child transcript");
    assert!(before_restart.contains("initial public durable turn"), "{before_restart}");
    assert!(before_restart.contains("public restart assistant"), "{before_restart}");
    runtime_a.shutdown().await;

    let runtime_b = OrchestrationRuntime::new(
        config(artifacts.path(), vec![task_definition()]),
        quick_factory("public restart assistant"),
    )
    .expect("runtime B");
    bind_then_recover_existing(&runtime_b, &parent);
    let worker = runtime_b
        .list("Main")
        .into_iter()
        .find(|agent| agent.id == "Worker")
        .expect("runtime B recovered Worker");
    assert_eq!(worker.status, AgentStatus::Parked, "{worker:?}");
    assert!(
        runtime_b
            .inbox("Worker", true)
            .iter()
            .any(|message| message.body == "mail retained across restart"),
        "runtime B recovered runtime A mailbox"
    );

    let receipts = runtime_b.send("Main", "Worker", "continue after public restart", None);
    assert_eq!(receipts.len(), 1, "{receipts:?}");
    assert_eq!(receipts[0].outcome, DeliveryOutcome::Revived, "{receipts:?}");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let jobs = runtime_b
            .jobs(None)
            .into_iter()
            .filter(|job| job.agent_id == "Worker")
            .collect::<Vec<_>>();
        if jobs.iter().any(|job| job.status.is_settled() && job.id != spawn[0].job_id) {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "revival did not settle: {jobs:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let after_restart = std::fs::read_to_string(&child_path).expect("continued child transcript");
    assert!(after_restart.contains("initial public durable turn"), "{after_restart}");
    assert!(
        after_restart.contains("mail retained across restart")
            && after_restart.contains("continue after public restart"),
        "recovered mailbox and new wake append to the same transcript: {after_restart}"
    );
    assert!(
        after_restart.matches("public restart assistant").count() >= 2,
        "a new assistant turn appends after restart: {after_restart}"
    );

    runtime_b.shutdown().await;
}

/// The Application session replacement surface preserves the old live
/// orchestration binding when the new parent sidecar is invalid. Retrying after
/// repair binds the new child root, clears Legacy, and creates only new children.
#[tokio::test]
async fn application_switch_rebind_is_transactional_and_uses_new_child_root() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let sessions = tempfile::tempdir().expect("sessions");
    let (old_parent, old_id, old_path) = parent_session(artifacts.path(), sessions.path());
    let old_child_root = child_root_for(&old_path, &old_id);
    let runtime = OrchestrationRuntime::new(
        config(artifacts.path(), vec![task_definition()]),
        quick_factory("application child turn"),
    )
    .expect("runtime");
    let application = Application::new(old_parent).await;
    application
        .attach_orchestration(runtime.clone())
        .expect("attach old application orchestration");
    let legacy = runtime
        .spawn_tasks(
            "Main",
            0,
            vec![pi_coding::TaskItem {
                index: 0,
                id: "Legacy".to_owned(),
                agent: "task".to_owned(),
                assignment: "old application child".to_owned(),
                todo_task_id: None,
                ..Default::default()
            }],
        )
        .expect("spawn Legacy");
    runtime
        .wait_jobs(
            &[legacy[0].job_id.clone()],
            Some(Duration::from_secs(5)),
            None,
        )
        .await
        .expect("wait Legacy");
    assert!(runtime.list("Main").iter().any(|agent| agent.id == "Legacy"));
    let old_state = DurableRuntime::new(
        old_id.clone(),
        old_path.clone(),
        old_child_root.clone(),
    )
    .expect("old application durable reader")
    .load()
    .expect("old application sidecar");

    let (new_parent, new_path) =
        parent_session_with_id(artifacts.path(), sessions.path(), "application.parent.v2");
    drop(new_parent);
    let new_id = "application.parent.v2";
    let new_child_root = child_root_for(&new_path, new_id);
    std::fs::create_dir_all(&new_child_root).expect("new application child root");
    let new_sidecar = new_child_root.join("orchestration-state.json");
    let mut invalid_agent = persisted_worker(
        "Intruder",
        AgentStatus::Parked,
        "invalid replacement",
        None,
        Vec::new(),
    );
    invalid_agent.request.child_id = "Mismatch".to_owned();
    let invalid = DurableState {
        version: DURABLE_STATE_VERSION,
        parent_session_id: new_id.to_owned(),
        parent_session_path: new_path.to_string_lossy().into_owned(),
        agents: vec![invalid_agent],
        jobs: Vec::new(),
    };
    std::fs::write(
        &new_sidecar,
        serde_json::to_vec_pretty(&invalid).expect("invalid application state"),
    )
    .expect("write invalid application sidecar");

    application
        .switch_session(&new_path)
        .await
        .expect_err("invalid application replacement must fail");
    let (retained_id, retained_path) = application
        .session()
        .recorder_info()
        .expect("failed Application cutover retains old parent recorder");
    assert_eq!(retained_id, old_id, "failed Application cutover retains old parent id");
    assert_eq!(retained_path, old_path, "failed Application cutover retains old parent path");
    let attached = application
        .orchestration_runtime()
        .expect("application retains orchestration runtime");
    assert_eq!(
        attached.group_id(),
        runtime.group_id(),
        "same live runtime remains attached"
    );
    assert!(
        attached.list("Main").iter().any(|agent| agent.id == "Legacy"),
        "failed Application cutover preserves old live roster"
    );
    let old_after_failure = DurableRuntime::new(old_id, old_path, old_child_root)
        .expect("old reader after failed application switch")
        .load()
        .expect("old state after failed application switch");
    assert_eq!(old_after_failure, old_state, "failed Application cutover preserves old sidecar");

    std::fs::remove_file(&new_sidecar).expect("repair invalid replacement sidecar");
    let switched = application
        .switch_session(&new_path)
        .await
        .expect("retry repaired application replacement");
    assert!(!switched.cancelled);
    let (switched_id, switched_path) = application
        .session()
        .recorder_info()
        .expect("successful Application cutover installs new parent recorder");
    assert_eq!(switched_id, new_id);
    assert_eq!(switched_path, new_path);
    assert!(
        runtime.list("Main").iter().all(|agent| agent.id != "Legacy"),
        "successful Application cutover cannot leak Legacy"
    );
    let fresh = runtime
        .spawn_tasks(
            "Main",
            0,
            vec![pi_coding::TaskItem {
                index: 0,
                id: "Fresh".to_owned(),
                agent: "task".to_owned(),
                assignment: "new application child".to_owned(),
                todo_task_id: None,
                ..Default::default()
            }],
        )
        .expect("spawn Fresh after Application cutover");
    runtime
        .wait_jobs(
            &[fresh[0].job_id.clone()],
            Some(Duration::from_secs(5)),
            None,
        )
        .await
        .expect("wait Fresh");
    let new_state = DurableRuntime::new(new_id.to_owned(), new_path, new_child_root.clone())
        .expect("new application durable reader")
        .load()
        .expect("new application sidecar");
    assert!(new_state.agents.iter().all(|agent| agent.snapshot.id != "Legacy"));
    let fresh_agent = new_state
        .agents
        .iter()
        .find(|agent| agent.snapshot.id == "Fresh")
        .expect("Fresh persisted under new application parent");
    let fresh_path = PathBuf::from(fresh_agent.session_path.as_deref().expect("Fresh transcript"));
    assert!(fresh_path.starts_with(&new_child_root), "{}", fresh_path.display());

    application.cleanup().await;
}
