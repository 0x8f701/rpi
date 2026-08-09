//! Persistent persona end-to-end tests through the orchestration runtime's
//! public surface.
//!
//! Contracts defended (deterministic, faux model, isolated temp dirs):
//! - **Discovery**: `ResourceManager` discovers a durable persona from
//!   `<agent_dir>/personas/<name>/persona.md` with `kind == Persona` and a
//!   real `persona_root`.
//! - **Preferred selection**: `set_preferred_agent` wins for unnamed `task`
//!   tool spawns when the named role is enabled (over ranked/default).
//! - **Fresh run**: spawning a bound persona settles the job and archives the
//!   child transcript to `<persona-root>/sessions/<agentId>.jsonl`.
//! - **Local memory retention**: the persona `memory` tool (wired by the
//!   production child factory) writes to `<persona-root>/memory/entries.jsonl`
//!   and that file persists across a second run.
//! - **Transcript archive continuity**: a second persona run loads the first
//!   run's archived transcript as history before the child runs (observable in
//!   the stream context).
//! - **History fallback after job retention**: once the settled job is pruned,
//!   `history://<agentId>` falls back to the persona archive.
//! - **Ordinary agent regression**: an ordinary agent (kind `Agent`) still
//!   spawns and settles alongside a persona in the same runtime.
//!
//! The production child factory (`child_factory_from_snapshot`) is used so the
//! persona coding tools — including the persona-rooted `memory` tool — are
//! injected exactly as in live runs; no tool construction is mocked.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;
use parking_lot::Mutex;
use pi_agent::{AbortController, StreamFn, ThinkingLevel, ToolCallContext};
use pi_ai::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Context, Model, SimpleStreamOptions,
    StopReason, ToolCall, new_assistant_message_event_stream,
};
use pi_coding::{
    AgentCatalog, AgentDefinition, AgentDefinitionKind, AgentDefinitionSource, JobStatus,
    OrchestrationConfig, OrchestrationRuntime, ResourceManager, ResourceManagerOptions, Session,
    SessionOptions, TaskItem, TaskSpawn, session_store,
};
use serde_json::{Value, json};

fn test_model() -> Model {
    let suffix = uuid::Uuid::now_v7().simple().to_string();
    Model {
        id: format!("persona-e2e-{suffix}"),
        name: "Persona E2E".to_owned(),
        api: format!("persona-e2e-api-{suffix}"),
        provider: format!("persona-e2e-provider-{suffix}"),
        ..Model::default()
    }
}

/// Writes `<persona_root>/persona.md` so `persona_root()` resolves and the
/// durable layout exists on disk.
fn write_persona_md(persona_root: &std::path::Path, name: &str) {
    std::fs::create_dir_all(persona_root).expect("persona root dir");
    std::fs::write(
        persona_root.join("persona.md"),
        format!("---\nname: {name}\ndescription: durable {name}\n---\n{name} prompt"),
    )
    .expect("persona.md");
}

/// A durable persona definition whose `persona_root()` is `persona_root`
/// (because `path` is `<persona_root>/persona.md`) and which declares only the
/// `memory` tool so the production factory injects the persona-rooted memory
/// tool and nothing else from the coding set.
fn persona_definition(persona_root: &std::path::Path, name: &str) -> AgentDefinition {
    AgentDefinition {
        name: name.to_owned(),
        description: format!("durable {name}"),
        system_prompt: format!("{name} prompt"),
        tools: Some(vec!["memory".to_owned()]),
        autoload_skills: Vec::new(),
        model: None,
        thinking_level: Some(ThinkingLevel::Off),
        max_turns: None,
        max_tool_calls: None,
        timeout_secs: None,
        disallowed_tools: Vec::new(),
        capability_ceiling: None,
        source: AgentDefinitionSource::User,
        path: Some(persona_root.join("persona.md")),
        trusted: true,
        kind: AgentDefinitionKind::Persona,
        personality: None,
        soft_budget: None,
    }
}

/// An ordinary (non-persona) agent that shares the runtime catalog with the
/// persona for regression coverage.
fn ordinary_definition(name: &str) -> AgentDefinition {
    AgentDefinition {
        name: name.to_owned(),
        description: format!("ordinary {name}"),
        system_prompt: format!("{name} prompt"),
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
        kind: AgentDefinitionKind::Agent,
        personality: None,
        soft_budget: None,
    }
}

fn tool_call_message(id: &str, name: &str, arguments: Value) -> AssistantMessage {
    let mut message = AssistantMessage::pending(&Model::default());
    message.stop_reason = StopReason::ToolUse;
    message.content = vec![ContentBlock::ToolCall(ToolCall {
        id: id.to_owned(),
        name: name.to_owned(),
        arguments,
        thought_signature: None,
    })];
    message
}

fn text_message(text: &str) -> AssistantMessage {
    let mut message = AssistantMessage::pending(&Model::default());
    message.stop_reason = StopReason::Stop;
    message.content = vec![ContentBlock::text(text)];
    message
}

/// A scripted stream that records the inbound `context.messages` for each call
/// (so a second run's continuity load is observable) then emits the next
/// preloaded assistant message. Calls beyond the preloaded list emit a plain
/// "done" message.
fn capturing_scripted(
    messages: Vec<AssistantMessage>,
    captured: Arc<Mutex<Vec<Vec<pi_ai::Message>>>>,
) -> StreamFn {
    let queue = Arc::new(Mutex::new(VecDeque::from(messages)));
    Arc::new(move |model: Model, context: Context, _options: SimpleStreamOptions| {
        let queue = queue.clone();
        let captured = captured.clone();
        async move {
            captured.lock().push(context.messages.clone());
            let message = queue.lock().pop_front().unwrap_or_else(|| {
                let mut fallback = AssistantMessage::pending(&model);
                fallback.content = vec![ContentBlock::text("done")];
                fallback.stop_reason = StopReason::Stop;
                fallback
            });
            let stream = new_assistant_message_event_stream();
            let producer = stream.clone();
            let model = model.clone();
            tokio::spawn(async move {
                producer
                    .push(AssistantMessageEvent::Start {
                        partial: AssistantMessage::pending(&model),
                    })
                    .await;
                let terminal = if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
                    AssistantMessageEvent::Error {
                        reason: message.stop_reason,
                        error: message.clone(),
                    }
                } else {
                    AssistantMessageEvent::Done {
                        reason: message.stop_reason,
                        message: message.clone(),
                    }
                };
                producer.push(terminal).await;
                producer.end(Some(message)).await;
            });
            stream
        }
        .boxed()
    })
}

struct PersonaRuntime {
    runtime: OrchestrationRuntime,
    persona_root: std::path::PathBuf,
    persona_name: String,
    captured: Arc<Mutex<Vec<Vec<pi_ai::Message>>>>,
}

/// Builds a durably-bound runtime whose production child factory injects the
/// persona-rooted `memory` tool, with a capturing scripted stream. The parent
/// session is recorded so `bind_and_recover` establishes the durable child
/// root. `max_retained_jobs` controls job retention for the history-fallback
/// test.
fn persona_runtime(
    root: &std::path::Path,
    persona_name: &str,
    messages: Vec<AssistantMessage>,
    max_retained_jobs: usize,
) -> PersonaRuntime {
    let persona_root = root.join("personas").join(persona_name);
    write_persona_md(&persona_root, persona_name);
    let definition = persona_definition(&persona_root, persona_name);
    let model = test_model();
    let captured: Arc<Mutex<Vec<Vec<pi_ai::Message>>>> = Arc::new(Mutex::new(Vec::new()));

    let parent = Session::new(SessionOptions {
        model: model.clone(),
        cwd: root.to_path_buf(),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: "persona-e2e".to_owned(),
        compaction: None,
        stream_options: SimpleStreamOptions::default(),
        tools: Some(Vec::new()),
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: Some(capturing_scripted(messages, captured.clone())),
        auth_resolver: None,
    })
    .expect("parent session");

    let session_dir = root.join("parent-sessions");
    let recorder = session_store::start_session_in(
        root,
        Some(&model),
        None,
        Some(&session_dir),
        None,
        None,
    )
    .expect("parent recorder");
    parent.record(recorder).expect("record parent");

    let mut config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![definition.clone(), ordinary_definition("worker")]),
        root.join("artifacts"),
    );
    config.default_agent = "worker".to_owned();
    config.parent_model = model;
    config.idle_ttl = None;
    config.max_retained_jobs = max_retained_jobs;
    config.retained_job_ttl = Duration::from_secs(24 * 60 * 60);

    let snapshot = parent.child_session_options_snapshot();
    let runtime =
        OrchestrationRuntime::new(config, OrchestrationRuntime::child_factory_from_snapshot(snapshot))
            .expect("runtime");
    runtime.bind_and_recover(&parent).expect("bind durable");
    assert!(runtime.is_durable(), "persona runtime must be durable-bound");
    PersonaRuntime {
        runtime,
        persona_root,
        persona_name: persona_name.to_owned(),
        captured,
    }
}

fn tool_context(id: &str, arguments: Value) -> ToolCallContext {
    let (_, abort) = AbortController::new();
    ToolCallContext {
        tool_call_id: id.to_owned(),
        arguments,
        on_update: Arc::new(|_| {}),
        abort,
        model: None,
    }
}

fn spawn_persona(
    runtime: &OrchestrationRuntime,
    agent: &str,
    id: &str,
    assignment: &str,
) -> TaskSpawn {
    runtime
        .spawn_tasks(
            "Main",
            0,
            vec![TaskItem {
                index: 0,
                id: id.to_owned(),
                agent: agent.to_owned(),
                assignment: assignment.to_owned(),
                todo_task_id: None,
                ..Default::default()
            }],
        )
        .expect("spawn")
        .remove(0)
}

async fn wait_settled(runtime: &OrchestrationRuntime, job_id: &str) -> pi_coding::JobSnapshot {
    let snapshots = runtime
        .wait_jobs(&[job_id.to_owned()], Some(Duration::from_secs(10)), None)
        .await
        .expect("wait");
    snapshots
        .into_iter()
        .find(|snapshot| snapshot.id == job_id)
        .expect("settled snapshot")
}

/// Discovery: `ResourceManager` loads a user persona from
/// `<agent_dir>/personas/<name>/persona.md` as `kind == Persona` with a
/// `persona_root` pointing at the on-disk directory.
#[test]
fn resource_manager_discovers_durable_persona() {
    let root = tempfile::tempdir().expect("root");
    let agent_dir = root.path().join("agent");
    let personas_dir = agent_dir.join("personas");
    let mentor_dir = personas_dir.join("mentor");
    std::fs::create_dir_all(&mentor_dir).expect("persona dir");
    std::fs::write(
        mentor_dir.join("persona.md"),
        "---\nname: mentor\ndescription: durable mentor\n---\nmentor prompt",
    )
    .expect("persona.md");

    let cwd = root.path().join("project");
    std::fs::create_dir_all(&cwd).expect("cwd");
    let mut options = ResourceManagerOptions::new(&cwd);
    options.agent_dir = agent_dir;
    options.project_trust_override = Some(false);
    let resources = ResourceManager::new(options).expect("resources");
    let snapshot = resources.snapshot();
    let mentor = snapshot
        .agents
        .iter()
        .find(|agent| agent.name == "mentor")
        .expect("mentor persona discovered");
    assert_eq!(mentor.kind, AgentDefinitionKind::Persona);
    assert!(mentor.is_persona());
    assert_eq!(mentor.source, AgentDefinitionSource::User);
    assert_eq!(
        mentor.persona_root().as_deref(),
        Some(mentor_dir.as_path()),
        "persona_root must point at the on-disk persona directory"
    );
}

/// Preferred selection: `set_preferred_agent` makes an unnamed `task` tool
/// spawn dispatch the preferred persona even when a different default agent is
/// configured.
#[tokio::test]
async fn preferred_agent_wins_unnamed_task_spawn() {
    let root = tempfile::tempdir().expect("root");
    // The preferred persona never actually runs a turn here — the task tool
    // only resolves the agent and returns spawn ids — so a single terminal
    // message keeps the stream non-empty for any incidental child start.
    let env = persona_runtime(
        root.path(),
        "mentor",
        vec![text_message("done")],
        256,
    );
    let runtime = &env.runtime;
    runtime.set_preferred_agent(Some("mentor"));
    assert_eq!(runtime.preferred_agent().as_deref(), Some("mentor"));

    let tools = runtime.agent_tools("Main", 0);
    let task = tools
        .iter()
        .find(|tool| tool.name == "task")
        .expect("task tool");
    let result = (task.execute)(tool_context(
        "preferred-spawn",
        json!({ "task": "study the persistent persona lifecycle" }),
    ))
    .await
    .expect("task tool spawn");
    let spawns: Vec<TaskSpawn> =
        serde_json::from_value(result.details).expect("spawns decoded");
    let spawn = spawns.first().expect("one spawn");
    assert_eq!(
        spawn.agent, "mentor",
        "unnamed task spawn must use the preferred persona, not the default worker"
    );
    runtime.shutdown().await;
}

/// Fresh run + local memory retention: a bound persona run settles, archives
/// its transcript under the persona root, and writes a persona-rooted memory
/// entry that survives the run.
#[tokio::test]
async fn persona_fresh_run_archives_transcript_and_persists_memory() {
    let root = tempfile::tempdir().expect("root");
    let env = persona_runtime(
        root.path(),
        "mentor",
        vec![
            tool_call_message(
                "learn-1",
                "memory",
                json!({ "op": "learn", "content": "durable-persona-note-AAAA" }),
            ),
            text_message("learned"),
        ],
        256,
    );
    let spawn = spawn_persona(&env.runtime, "mentor", "Mentor", "first-run-marker-AAAA");
    let snapshot = wait_settled(&env.runtime, &spawn.job_id).await;
    assert_eq!(snapshot.status, JobStatus::Completed, "job should settle completed");
    let result = snapshot.result.expect("result");
    assert!(result.error.is_none(), "no error: {:?}", result.error);
    assert!(!result.output.is_empty(), "run must produce output");

    let entries = env.persona_root.join("memory").join("entries.jsonl");
    let entries_text = std::fs::read_to_string(&entries).expect("persona memory entries");
    assert!(
        entries_text.contains("durable-persona-note-AAAA"),
        "persona memory must be rooted at <persona-root>/memory/entries.jsonl: {entries_text}"
    );

    let archive = env.persona_root.join("sessions").join("Mentor.jsonl");
    assert!(archive.exists(), "transcript archive must be written: {}", archive.display());
    let archive_text = std::fs::read_to_string(&archive).expect("archive");
    assert!(
        archive_text.contains("first-run-marker-AAAA"),
        "archived transcript must carry the run assignment: {archive_text}"
    );
    env.runtime.shutdown().await;
}

/// Transcript archive continuity across a second run: the second persona run
/// (allocated `Mentor_2` because `Mentor` is archived) loads the first run's
/// archived transcript as history — observable in the stream context — and the
/// persona memory entry persists unchanged.
#[tokio::test]
async fn persona_continuity_loaded_on_second_run() {
    let root = tempfile::tempdir().expect("root");
    let env = persona_runtime(
        root.path(),
        "mentor",
        vec![
            // Run 1: learn a durable note, then settle.
            tool_call_message(
                "learn-1",
                "memory",
                json!({ "op": "learn", "content": "durable-persona-note-AAAA" }),
            ),
            text_message("learned"),
            // Run 2: recall (reads the same persona-rooted store), then settle.
            tool_call_message(
                "recall-2",
                "memory",
                json!({ "op": "recall", "query": "durable-persona-note" }),
            ),
            text_message("recalled"),
        ],
        256,
    );

    let first = spawn_persona(&env.runtime, "mentor", "Mentor", "first-run-marker-AAAA");
    wait_settled(&env.runtime, &first.job_id).await;
    let first_captured = env.captured.lock().len();

    // The second run is allocated Mentor_2 because the Mentor archive exists.
    let second = spawn_persona(&env.runtime, "mentor", "Mentor", "second-run-marker-BBBB");
    assert_eq!(
        second.agent_id, "Mentor_2",
        "second persona run must be allocated a fresh id past the archived one"
    );
    wait_settled(&env.runtime, &second.job_id).await;

    // Run 2's stream calls happen after run 1's; their context must include run
    // 1's archived transcript (loaded as continuity before the child runs).
    let second_contexts: Vec<Vec<pi_ai::Message>> =
        env.captured.lock().iter().skip(first_captured).cloned().collect();
    assert!(
        !second_contexts.is_empty(),
        "run 2 must make at least one stream call"
    );
    let serialized = serde_json::to_string(&second_contexts).expect("serialize contexts");
    assert!(
        serialized.contains("first-run-marker-AAAA"),
        "run 2 must load run 1's archived transcript as continuity: {serialized}"
    );
    assert!(
        !serialized.contains("second-run-marker-BBBB") || serialized.contains("first-run-marker-AAAA"),
        "continuity marker present"
    );

    // Two archives now exist; memory entry persists unchanged across both runs.
    let sessions = env.persona_root.join("sessions");
    assert!(sessions.join("Mentor.jsonl").exists(), "first archive present");
    assert!(sessions.join("Mentor_2.jsonl").exists(), "second archive present");
    let entries = std::fs::read_to_string(env.persona_root.join("memory").join("entries.jsonl"))
        .expect("entries");
    assert!(
        entries.matches("durable-persona-note-AAAA").count() == 1,
        "recall must not duplicate the learned entry: {entries}"
    );
    env.runtime.shutdown().await;
}

/// History fallback after job retention: once the settled persona job is
/// evicted over the retention limit (the live REGISTRY entry and artifact
/// history are removed), `history://<agentId>` falls back to the persona
/// archive, which survives retention.
#[tokio::test]
async fn persona_history_falls_back_to_archive_after_retention() {
    let root = tempfile::tempdir().expect("root");
    let env = persona_runtime(
        root.path(),
        "mentor",
        vec![
            // Run 1 (persona): learn a durable note, then settle.
            tool_call_message(
                "learn-1",
                "memory",
                json!({ "op": "learn", "content": "durable-persona-note-AAAA" }),
            ),
            text_message("learned"),
            // Evictor run (ordinary worker): settles on a single text turn.
            text_message("worker-done"),
        ],
        // Retain only one settled job: a second settled job evicts the oldest
        // (the persona run) on the next jobs() prune, dropping its REGISTRY
        // entry and artifact history file while leaving the persona archive.
        1,
    );
    let persona_spawn = spawn_persona(&env.runtime, "mentor", "Mentor", "first-run-marker-AAAA");
    wait_settled(&env.runtime, &persona_spawn.job_id).await;

    let archive = env.persona_root.join("sessions").join("Mentor.jsonl");
    assert!(archive.exists(), "persona archive must exist before retention");

    // Before eviction the live REGISTRY entry resolves to the artifact history.
    let before = env
        .runtime
        .resolve_read_uri("history://Mentor")
        .expect("history resolves before retention");
    assert!(before.exists(), "artifact history exists before eviction");

    // Evict the persona job: a second settled job pushes it over the
    // one-job retention limit, so jobs() prunes the oldest (Mentor).
    let evictor = spawn_persona(&env.runtime, "worker", "Evictor", "evict the persona job");
    wait_settled(&env.runtime, &evictor.job_id).await;
    env.runtime.jobs(None);

    let resolved = env
        .runtime
        .resolve_read_uri("history://Mentor")
        .expect("history resolves after retention");
    assert_eq!(
        resolved,
        std::fs::canonicalize(&archive).expect("canonical archive"),
        "history:// must fall back to the persona archive after the live entry is pruned"
    );
    assert!(resolved.exists(), "fallback archive must still exist");
    env.runtime.shutdown().await;
}

/// Ordinary agent regression: an ordinary agent (kind `Agent`) spawns and
/// settles in the same runtime as a persona, and is not promoted to a persona.
#[tokio::test]
async fn ordinary_agent_spawns_and_settles_alongside_persona() {
    let root = tempfile::tempdir().expect("root");
    let env = persona_runtime(
        root.path(),
        "mentor",
        // The ordinary worker settles on the first scripted message.
        vec![text_message("worker-done")],
        256,
    );
    // The worker is an ordinary agent, not a persona.
    let worker = env
        .runtime
        .enabled_agents()
        .into_iter()
        .find(|agent| agent.name == "worker")
        .expect("worker in catalog");
    assert!(!worker.is_persona(), "worker must not be a persona");
    assert_eq!(worker.kind, AgentDefinitionKind::Agent);

    let spawn = spawn_persona(&env.runtime, "worker", "Worker", "ordinary-agent-marker-CCCC");
    assert_eq!(spawn.agent, "worker");
    let snapshot = wait_settled(&env.runtime, &spawn.job_id).await;
    assert_eq!(snapshot.status, JobStatus::Completed);
    let result = snapshot.result.expect("result");
    assert!(result.error.is_none(), "ordinary agent must not error: {:?}", result.error);
    assert!(
        result.output.contains("worker-done"),
        "ordinary agent output must reflect the settled turn: {}",
        result.output
    );
    env.runtime.shutdown().await;
}

/// A persona spawn on a non-durable runtime is rejected: personas require a
/// durable parent session binding.
#[tokio::test]
async fn persona_spawn_requires_durable_binding() {
    let root = tempfile::tempdir().expect("root");
    let persona_root = root.path().join("personas").join("mentor");
    write_persona_md(&persona_root, "mentor");
    let definition = persona_definition(&persona_root, "mentor");
    let model = test_model();
    let parent = Session::new(SessionOptions {
        model: model.clone(),
        cwd: root.path().to_path_buf(),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: "persona-e2e".to_owned(),
        compaction: None,
        stream_options: SimpleStreamOptions::default(),
        tools: Some(Vec::new()),
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: Some(capturing_scripted(vec![text_message("done")], Arc::new(Mutex::new(Vec::new())))),
        auth_resolver: None,
    })
    .expect("parent session");
    let mut config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![definition]),
        root.path().join("artifacts"),
    );
    config.default_agent = "mentor".to_owned();
    config.parent_model = model;
    config.idle_ttl = None;
    let snapshot = parent.child_session_options_snapshot();
    let runtime =
        OrchestrationRuntime::new(config, OrchestrationRuntime::child_factory_from_snapshot(snapshot))
            .expect("runtime");
    assert!(!runtime.is_durable(), "runtime is not bound yet");
    let error = runtime
        .spawn_tasks(
            "Main",
            0,
            vec![TaskItem {
                index: 0,
                id: "Mentor".to_owned(),
                agent: "mentor".to_owned(),
                assignment: "must be rejected without durable binding".to_owned(),
                todo_task_id: None,
                ..Default::default()
            }],
        )
        .expect_err("non-durable persona spawn must fail");
    assert!(
        error.to_string().contains("durable parent session"),
        "expected durable-binding requirement, got: {error}"
    );
    runtime.shutdown().await;
}