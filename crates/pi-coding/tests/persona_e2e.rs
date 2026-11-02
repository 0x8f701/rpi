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
//! - **Learned skills**: `learn_skill` persists under the persona root and is
//!   autoloaded beginning with the next spawn; it stays inside that persona
//!   and shadows a same-named shared skill only there.
//! - **Memory across rebuild**: a persona-rooted `memory` entry survives a
//!   new `Application` on the same disk state.
//! - **Workspace inheritance**: a parent `WorkspaceRoots` additional root
//!   reaches the built child's `Session::workspace_roots()`.
//!
//! The production child factory (`child_factory_from_snapshot`) is used so the
//! persona coding tools — including the persona-rooted `memory` tool — are
//! injected exactly as in live runs; no tool construction is mocked. Child
//! tool names and prompts are captured from the built `Session` after the
//! inner factory returns `Ok`.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;
use parking_lot::Mutex;
use pi_agent::{AbortController, StreamFn, ThinkingLevel, ToolCallContext};
use pi_ai::providers::{FauxProviderOptions, FauxProviderRegistration, register_faux_provider};
use pi_ai::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Context, Model, SimpleStreamOptions,
    StopReason, ToolCall, new_assistant_message_event_stream,
};
use pi_coding::{
    AgentCatalog, AgentDefinition, AgentDefinitionKind, AgentDefinitionSource, Application,
    ApplicationRuntimeCandidate, ChildSessionFactory, JobStatus, OrchestrationConfig,
    OrchestrationRuntime, OrchestrationSkill, ResourceDiscovery, ResourceManager,
    ResourceManagerOptions, Session, SessionOptions, SkillSource, TaskItem, TaskSpawn,
    WorkspaceRoots, session_store,
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
        knowledge_roots: Vec::new(),
        user_proxy: false,
    }
}

fn persona_definition_with_tools(
    persona_root: &std::path::Path,
    name: &str,
    tools: &[&str],
) -> AgentDefinition {
    let mut definition = persona_definition(persona_root, name);
    definition.tools = Some(tools.iter().map(|tool| (*tool).to_owned()).collect());
    definition
}

fn seed_persona_skill(persona_root: &std::path::Path, name: &str, description: &str, body: &str) {
    let skill = persona_root.join("skills").join(name);
    std::fs::create_dir_all(&skill).expect("persona skill dir");
    std::fs::write(
        skill.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}\n"),
    )
    .expect("persona SKILL.md");
}

fn shared_skill(
    root: &std::path::Path,
    name: &str,
    description: &str,
    body: &str,
) -> OrchestrationSkill {
    let skill_dir = root.join("shared-skills").join(name);
    std::fs::create_dir_all(&skill_dir).expect("shared skill dir");
    let file_path = skill_dir.join("SKILL.md");
    std::fs::write(
        &file_path,
        format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}\n"),
    )
    .expect("shared SKILL.md");
    OrchestrationSkill {
        name: name.to_owned(),
        description: description.to_owned(),
        file_path,
        base_dir: skill_dir,
        globs: Vec::new(),
        always_apply: false,
        hidden: false,
        disable_model_invocation: false,
        source: SkillSource::Project,
        managed_project_boundary: None,
        managed_root: None,
        trusted: true,
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
        knowledge_roots: Vec::new(),
        user_proxy: false,
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
    Arc::new(
        move |model: Model, context: Context, _options: SimpleStreamOptions| {
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
                    let terminal =
                        if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
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
        },
    )
}

struct CapturedChild {
    agent: String,
    tool_names: Vec<String>,
    system_prompt: String,
    session: Session,
}

#[derive(Default)]
struct PersonaRuntimeConfig {
    extra_catalog: Vec<AgentDefinition>,
    extra_skills: Vec<OrchestrationSkill>,
    workspace: Option<WorkspaceRoots>,
    persona_tools: Option<Vec<String>>,
}

struct PersonaRuntime {
    runtime: OrchestrationRuntime,
    application: Application,
    model_registration: FauxProviderRegistration,
    persona_root: std::path::PathBuf,
    persona_name: String,
    captured: Arc<Mutex<Vec<Vec<pi_ai::Message>>>>,
    children: tokio::sync::mpsc::UnboundedReceiver<CapturedChild>,
}

/// Builds a durably-bound application whose production child factory injects
/// the persona-rooted `memory` tool, with a capturing scripted stream. The
/// parent session is recorded before publication so the application installs
/// one authoritative generation and binds the durable child root against it.
/// `max_retained_jobs` controls job retention for the history-fallback test.
async fn persona_runtime(
    root: &std::path::Path,
    persona_name: &str,
    messages: Vec<AssistantMessage>,
    max_retained_jobs: usize,
) -> PersonaRuntime {
    persona_runtime_configured(
        root,
        persona_name,
        messages,
        max_retained_jobs,
        PersonaRuntimeConfig::default(),
    )
    .await
}

async fn persona_runtime_configured(
    root: &std::path::Path,
    persona_name: &str,
    messages: Vec<AssistantMessage>,
    max_retained_jobs: usize,
    config_ext: PersonaRuntimeConfig,
) -> PersonaRuntime {
    let persona_root = root.join("personas").join(persona_name);
    write_persona_md(&persona_root, persona_name);
    let definition = match config_ext.persona_tools.as_deref() {
        Some(tools) => persona_definition_with_tools(
            &persona_root,
            persona_name,
            &tools.iter().map(String::as_str).collect::<Vec<_>>(),
        ),
        None => persona_definition(&persona_root, persona_name),
    };
    let model = test_model();
    let model_registration = register_faux_provider(FauxProviderOptions {
        api: model.api.clone(),
        provider: model.provider.clone(),
        models: vec![model.clone()],
        chunk_size: 1,
    });
    let captured: Arc<Mutex<Vec<Vec<pi_ai::Message>>>> = Arc::new(Mutex::new(Vec::new()));

    let cwd = config_ext
        .workspace
        .as_ref()
        .map(|workspace| workspace.cwd().to_path_buf())
        .unwrap_or_else(|| {
            let cwd = root.join("project");
            std::fs::create_dir_all(&cwd).expect("project cwd");
            cwd
        });
    let options = SessionOptions {
        model: model.clone(),
        cwd: cwd.clone(),
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
        auth_manager: None,
    };
    let parent = if let Some(workspace) = config_ext.workspace.clone() {
        Session::new_with_additional_tools_filtered_discovery_workspace_and_uri(
            options,
            Vec::new(),
            Default::default(),
            ResourceDiscovery::Disabled,
            workspace,
            None,
        )
    } else {
        Session::new_with_additional_tools_filtered_and_discovery(
            options,
            Vec::new(),
            Default::default(),
            ResourceDiscovery::Disabled,
        )
    }
    .expect("parent session");

    let session_dir = root.join("parent-sessions");
    let recorder =
        session_store::start_session_in(root, Some(&model), None, Some(&session_dir), None, None)
            .expect("parent recorder");
    parent.record(recorder).expect("record parent");

    let mut agents = vec![definition.clone(), ordinary_definition("worker")];
    agents.extend(config_ext.extra_catalog);
    let mut config = OrchestrationConfig::new(
        AgentCatalog::from_agents(agents),
        root.join("artifacts"),
    );
    config.skills = config_ext.extra_skills;
    config.default_agent = "worker".to_owned();
    config.parent_model = model;
    config.idle_ttl = None;
    config.max_retained_jobs = max_retained_jobs;
    config.retained_job_ttl = Duration::from_secs(24 * 60 * 60);

    let agent_dir = root.join("agent-home");
    std::fs::create_dir_all(&agent_dir).expect("agent home");
    std::fs::write(
        agent_dir.join("settings.json"),
        r#"{"sandbox":{"mode":"danger-full-access"},"orchestration":{"isolation":"none"}}"#,
    )
    .expect("persona settings");
    let mut resource_options = ResourceManagerOptions::new(&cwd);
    resource_options.agent_dir = agent_dir;
    resource_options.disable_extensions = true;
    resource_options.disable_skills = true;
    resource_options.disable_prompt_templates = true;
    resource_options.disable_themes = true;
    resource_options.disable_context_files = true;
    let resources = ResourceManager::new(resource_options).expect("persona resources");
    parent
        .attach_resources(resources)
        .await
        .expect("attach persona resources");
    let snapshot = parent.child_session_options_snapshot();
    let inner = OrchestrationRuntime::child_factory_from_snapshot(snapshot);
    let (sender, children) = tokio::sync::mpsc::unbounded_channel();
    let factory: ChildSessionFactory = std::sync::Arc::new(move |request| {
        let inner = inner.clone();
        let sender = sender.clone();
        Box::pin(async move {
            let agent = request.definition.name.clone();
            let child = inner(request).await?;
            let captured = CapturedChild {
                agent,
                tool_names: child.get_active_tool_names(),
                system_prompt: child.system_prompt().await,
                session: child.clone(),
            };
            let _ = sender.send(captured);
            Ok(child)
        })
    });
    let runtime = OrchestrationRuntime::new(config, factory).expect("runtime");
    let application = Application::from_runtime_candidate(
        ApplicationRuntimeCandidate::new(parent).with_orchestration(runtime.clone()),
    )
    .await
    .expect("publish persona application");
    assert!(
        runtime.is_durable(),
        "persona runtime must be durable-bound"
    );
    PersonaRuntime {
        runtime,
        model_registration,
        application,
        persona_root,
        persona_name: persona_name.to_owned(),
        captured,
        children,
    }
}

async fn captured_child(
    children: &mut tokio::sync::mpsc::UnboundedReceiver<CapturedChild>,
    agent: &str,
) -> CapturedChild {
    loop {
        let child = tokio::time::timeout(std::time::Duration::from_secs(30), children.recv())
            .await
            .expect("the spawned child factory must build a session")
            .expect("captured child channel");
        if child.agent == agent {
            return child;
        }
    }
}

fn tool_text(result: &pi_agent::AgentToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            pi_ai::ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn tool_context(id: &str, arguments: Value) -> ToolCallContext {
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
    let env = persona_runtime(root.path(), "mentor", vec![text_message("done")], 256).await;
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
    let spawns: Vec<TaskSpawn> = serde_json::from_value(result.details).expect("spawns decoded");
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
    )
    .await;
    let spawn = spawn_persona(&env.runtime, "mentor", "Mentor", "first-run-marker-AAAA");
    let snapshot = wait_settled(&env.runtime, &spawn.job_id).await;
    assert_eq!(
        snapshot.status,
        JobStatus::Completed,
        "job should settle completed"
    );
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
    assert!(
        archive.exists(),
        "transcript archive must be written: {}",
        archive.display()
    );
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
            tool_call_message(
                "learn-1",
                "memory",
                json!({ "op": "learn", "content": "durable-persona-note-AAAA" }),
            ),
            text_message("learned"),
            tool_call_message(
                "recall-2",
                "memory",
                json!({ "op": "recall", "query": "durable-persona-note" }),
            ),
            text_message("recalled"),
        ],
        256,
    )
    .await;

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
    let second_contexts: Vec<Vec<pi_ai::Message>> = env
        .captured
        .lock()
        .iter()
        .skip(first_captured)
        .cloned()
        .collect();
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
        !serialized.contains("second-run-marker-BBBB")
            || serialized.contains("first-run-marker-AAAA"),
        "continuity marker present"
    );

    // Two archives now exist; memory entry persists unchanged across both runs.
    let sessions = env.persona_root.join("sessions");
    assert!(
        sessions.join("Mentor.jsonl").exists(),
        "first archive present"
    );
    assert!(
        sessions.join("Mentor_2.jsonl").exists(),
        "second archive present"
    );
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
            tool_call_message(
                "learn-1",
                "memory",
                json!({ "op": "learn", "content": "durable-persona-note-AAAA" }),
            ),
            text_message("learned"),
            text_message("worker-done"),
        ],
        1,
    )
    .await;
    let persona_spawn = spawn_persona(&env.runtime, "mentor", "Mentor", "first-run-marker-AAAA");
    wait_settled(&env.runtime, &persona_spawn.job_id).await;

    let archive = env.persona_root.join("sessions").join("Mentor.jsonl");
    assert!(
        archive.exists(),
        "persona archive must exist before retention"
    );

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
    )
    .await;
    // The worker is an ordinary agent, not a persona.
    let worker = env
        .runtime
        .enabled_agents()
        .into_iter()
        .find(|agent| agent.name == "worker")
        .expect("worker in catalog");
    assert!(!worker.is_persona(), "worker must not be a persona");
    assert_eq!(worker.kind, AgentDefinitionKind::Agent);

    let spawn = spawn_persona(
        &env.runtime,
        "worker",
        "Worker",
        "ordinary-agent-marker-CCCC",
    );
    assert_eq!(spawn.agent, "worker");
    let snapshot = wait_settled(&env.runtime, &spawn.job_id).await;
    assert_eq!(snapshot.status, JobStatus::Completed);
    let result = snapshot.result.expect("result");
    assert!(
        result.error.is_none(),
        "ordinary agent must not error: {:?}",
        result.error
    );
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
        stream_fn: Some(capturing_scripted(
            vec![text_message("done")],
            Arc::new(Mutex::new(Vec::new())),
        )),
        auth_resolver: None,
        auth_manager: None,
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
    let runtime = OrchestrationRuntime::new(
        config,
        OrchestrationRuntime::child_factory_from_snapshot(snapshot),
    )
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


#[tokio::test]
async fn persona_learn_skill_persists_and_loads_on_next_spawn() {
    let root = tempfile::tempdir().expect("root");
    let mut env = persona_runtime_configured(
        root.path(),
        "mentor",
        vec![text_message("first-spawn-done"), text_message("second-spawn-done")],
        256,
        PersonaRuntimeConfig {
            persona_tools: Some(vec!["memory".to_owned(), "learn_skill".to_owned()]),
            ..PersonaRuntimeConfig::default()
        },
    )
    .await;

    let first = spawn_persona(&env.runtime, "mentor", "Mentor", "draft the first report");
    let first_child = captured_child(&mut env.children, "mentor").await;
    assert!(
        first_child.tool_names.iter().any(|name| name == "learn_skill"),
        "learn_skill must reach the first child: {:?}",
        first_child.tool_names
    );
    assert!(
        !first_child.system_prompt.contains("release-audit"),
        "the current prompt stays immutable after learn_skill: {}",
        first_child.system_prompt
    );
    let learn = first_child
        .session
        .active_tools()
        .into_iter()
        .find(|tool| tool.name == "learn_skill")
        .expect("the child holds learn_skill");
    let learned = (learn.execute)(tool_context(
        "learn-skill-1",
        json!({
            "name": "release-audit",
            "description": "persona-local release audit",
            "instructions": "persona-only-skill-body-YYYY"
        }),
    ))
    .await
    .expect("learn_skill execute");
    assert!(
        tool_text(&learned).contains("Learned persona skill `release-audit`"),
        "learn_skill must persist through the real tool: {}",
        tool_text(&learned)
    );
    let snapshot = wait_settled(&env.runtime, &first.job_id).await;
    assert_eq!(snapshot.status, JobStatus::Completed);
    let result = snapshot.result.expect("result");
    assert!(result.error.is_none(), "no error: {:?}", result.error);
    let skill_md = env
        .persona_root
        .join("skills")
        .join("release-audit")
        .join("SKILL.md");
    let skill_text = std::fs::read_to_string(&skill_md).expect("learned SKILL.md");
    assert!(
        skill_text.contains("persona-only-skill-body-YYYY"),
        "learned skill bytes must land under the persona root: {skill_text}"
    );

    let second = spawn_persona(&env.runtime, "mentor", "Mentor", "draft the follow-up report");
    let second_child = captured_child(&mut env.children, "mentor").await;
    wait_settled(&env.runtime, &second.job_id).await;
    assert!(
        second_child.system_prompt.contains("skill://release-audit"),
        "the next spawn must autoload the learned skill: {}",
        second_child.system_prompt
    );
    assert!(
        second_child
            .system_prompt
            .contains("persona-only-skill-body-YYYY"),
        "the next spawn must include the learned skill body: {}",
        second_child.system_prompt
    );
    env.runtime.shutdown().await;
    env.application.cleanup().await;
}

#[tokio::test]
async fn persona_learn_skill_stays_inside_its_own_persona() {
    let root = tempfile::tempdir().expect("root");
    let archivist_root = root.path().join("personas").join("archivist");
    write_persona_md(&archivist_root, "archivist");
    let mut env = persona_runtime_configured(
        root.path(),
        "mentor",
        vec![
            tool_call_message(
                "learn-skill-1",
                "learn_skill",
                json!({
                    "name": "release-audit",
                    "description": "persona-local release audit",
                    "instructions": "persona-only-skill-body-YYYY"
                }),
            ),
            text_message("learned-skill"),
            text_message("archivist-done"),
            text_message("worker-done"),
        ],
        256,
        PersonaRuntimeConfig {
            persona_tools: Some(vec!["memory".to_owned(), "learn_skill".to_owned()]),
            extra_catalog: vec![persona_definition(&archivist_root, "archivist")],
            ..PersonaRuntimeConfig::default()
        },
    )
    .await;

    let learned = spawn_persona(&env.runtime, "mentor", "Mentor", "draft the first report");
    captured_child(&mut env.children, "mentor").await;
    wait_settled(&env.runtime, &learned.job_id).await;

    let skill_md = env
        .persona_root
        .join("skills")
        .join("release-audit")
        .join("SKILL.md");
    assert!(skill_md.is_file(), "mentor learned the skill");
    assert!(
        !archivist_root.join("skills").exists(),
        "the sibling persona must not receive the learned skill directory"
    );

    let other = spawn_persona(
        &env.runtime,
        "archivist",
        "Archivist",
        "review the archive labels",
    );
    let other_child = captured_child(&mut env.children, "archivist").await;
    wait_settled(&env.runtime, &other.job_id).await;
    assert!(
        !other_child.system_prompt.contains("release-audit"),
        "a sibling persona must not see another persona's learned skill: {}",
        other_child.system_prompt
    );
    assert!(
        !other_child
            .tool_names
            .iter()
            .any(|name| name == "learn_skill"),
        "learn_skill stays on the persona that declared it: {:?}",
        other_child.tool_names
    );

    let ordinary = spawn_persona(
        &env.runtime,
        "worker",
        "Worker",
        "review the ordinary notes",
    );
    let ordinary_child = captured_child(&mut env.children, "worker").await;
    wait_settled(&env.runtime, &ordinary.job_id).await;
    assert!(
        !ordinary_child.system_prompt.contains("release-audit"),
        "an ordinary child must not see a persona-local skill: {}",
        ordinary_child.system_prompt
    );
    let snapshot = env
        .application
        .resource_snapshot()
        .expect("resource snapshot");
    assert!(
        snapshot
            .skills
            .iter()
            .all(|skill| skill.name != "release-audit"),
        "learn_skill must not pollute the shared skill catalog: {:?}",
        snapshot.skills.iter().map(|skill| &skill.name).collect::<Vec<_>>()
    );
    env.runtime.shutdown().await;
    env.application.cleanup().await;
}

#[tokio::test]
async fn persona_memory_recall_survives_application_rebuild() {
    let root = tempfile::tempdir().expect("root");
    let env = persona_runtime(
        root.path(),
        "mentor",
        vec![
            tool_call_message(
                "learn-1",
                "memory",
                json!({ "op": "learn", "content": "durable-rebuild-note-ZZZZ" }),
            ),
            text_message("learned"),
        ],
        256,
    )
    .await;
    let first = spawn_persona(&env.runtime, "mentor", "Mentor", "store the rebuild note");
    wait_settled(&env.runtime, &first.job_id).await;
    let entries = env.persona_root.join("memory").join("entries.jsonl");
    let entries_text = std::fs::read_to_string(&entries).expect("persona memory entries");
    assert!(
        entries_text.contains("durable-rebuild-note-ZZZZ"),
        "first application must persist the persona memory entry: {entries_text}"
    );
    env.runtime.shutdown().await;
    env.application.cleanup().await;

    let mut rebuilt = persona_runtime(
        root.path(),
        "mentor",
        vec![
            tool_call_message(
                "list-1",
                "memory",
                json!({ "op": "list" }),
            ),
            text_message("listed"),
        ],
        256,
    )
    .await;
    let second = spawn_persona(
        &rebuilt.runtime,
        "mentor",
        "Mentor",
        "recall the rebuild note",
    );
    let child = captured_child(&mut rebuilt.children, "mentor").await;
    wait_settled(&rebuilt.runtime, &second.job_id).await;
    let memory = child
        .session
        .active_tools()
        .into_iter()
        .find(|tool| tool.name == "memory")
        .expect("rebuilt child holds the persona memory tool");
    let listed = (memory.execute)(tool_context("rebuild-list", json!({ "op": "list" })))
        .await
        .expect("persona memory list after rebuild");
    assert!(
        tool_text(&listed).contains("durable-rebuild-note-ZZZZ"),
        "persona memory must survive application rebuild: {}",
        tool_text(&listed)
    );
    rebuilt.runtime.shutdown().await;
    rebuilt.application.cleanup().await;
}

#[tokio::test]
async fn persona_skill_shadows_shared_skill_only_inside_persona() {
    let root = tempfile::tempdir().expect("root");
    let persona_root = root.path().join("personas").join("mentor");
    write_persona_md(&persona_root, "mentor");
    seed_persona_skill(
        &persona_root,
        "release-audit",
        "persona-local release audit",
        "persona-only-skill-body-YYYY",
    );
    let shared = shared_skill(
        root.path(),
        "release-audit",
        "shared release audit",
        "shared-skill-body-XXXX",
    );
    let mut env = persona_runtime_configured(
        root.path(),
        "mentor",
        vec![text_message("persona-done"), text_message("worker-done")],
        256,
        PersonaRuntimeConfig {
            extra_skills: vec![shared],
            ..PersonaRuntimeConfig::default()
        },
    )
    .await;

    let persona_spawn = spawn_persona(&env.runtime, "mentor", "Mentor", "draft the first report");
    let persona_child = captured_child(&mut env.children, "mentor").await;
    wait_settled(&env.runtime, &persona_spawn.job_id).await;
    assert!(
        persona_child
            .system_prompt
            .contains("persona-only-skill-body-YYYY"),
        "the persona child must autoload its own skill body: {}",
        persona_child.system_prompt
    );
    assert!(
        !persona_child
            .system_prompt
            .contains("shared-skill-body-XXXX"),
        "the persona-local skill must shadow the shared skill: {}",
        persona_child.system_prompt
    );

    let ordinary = spawn_persona(
        &env.runtime,
        "worker",
        "Worker",
        "review the ordinary notes",
    );
    let ordinary_child = captured_child(&mut env.children, "worker").await;
    wait_settled(&env.runtime, &ordinary.job_id).await;
    assert!(
        ordinary_child.system_prompt.contains("skill://release-audit"),
        "an ordinary child still sees the shared skill catalog entry: {}",
        ordinary_child.system_prompt
    );
    assert!(
        ordinary_child
            .system_prompt
            .contains("shared release audit"),
        "an ordinary child still sees the shared skill description: {}",
        ordinary_child.system_prompt
    );
    assert!(
        !ordinary_child
            .system_prompt
            .contains("shared-skill-body-XXXX"),
        "shared skill bodies autoload only when selected; catalog listing is the ordinary view: {}",
        ordinary_child.system_prompt
    );
    assert!(
        !ordinary_child
            .system_prompt
            .contains("persona-only-skill-body-YYYY"),
        "an ordinary child must not see the persona-local skill body: {}",
        ordinary_child.system_prompt
    );
    env.runtime.shutdown().await;
    env.application.cleanup().await;
}

#[tokio::test]
async fn persona_child_inherits_parent_additional_workspace_roots() {
    let root = tempfile::tempdir().expect("root");
    let project = root.path().join("project");
    let extra = root.path().join("extra-root");
    std::fs::create_dir_all(&project).expect("project cwd");
    std::fs::create_dir_all(&extra).expect("additional root");
    let workspace = WorkspaceRoots::new(&project, [extra.clone()]).expect("workspace");
    assert_eq!(workspace.cwd(), project.as_path());
    assert_eq!(
        workspace.additional_roots(),
        std::slice::from_ref(&extra.canonicalize().unwrap_or(extra.clone())),
        "fixture must carry exactly one additional root"
    );
    let expected_cwd = workspace.cwd().to_path_buf();
    let expected_additional = workspace.additional_roots().to_vec();
    assert_eq!(expected_additional.len(), 1);

    let mut env = persona_runtime_configured(
        root.path(),
        "mentor",
        vec![text_message("done")],
        256,
        PersonaRuntimeConfig {
            workspace: Some(workspace),
            ..PersonaRuntimeConfig::default()
        },
    )
    .await;

    let parent = env.application.session();
    let snapshot = parent.child_session_options_snapshot();
    assert_eq!(snapshot.workspace.cwd(), expected_cwd.as_path());
    assert_eq!(
        snapshot.workspace.additional_roots(),
        expected_additional.as_slice(),
        "the parent snapshot must preserve the additional root"
    );

    let spawn = spawn_persona(&env.runtime, "mentor", "Mentor", "inspect the extra root");
    let child = captured_child(&mut env.children, "mentor").await;
    wait_settled(&env.runtime, &spawn.job_id).await;
    assert_eq!(child.session.workspace_roots().cwd(), expected_cwd.as_path());
    assert_eq!(
        child.session.workspace_roots().additional_roots(),
        expected_additional.as_slice(),
        "the built child must inherit the parent's additional workspace roots"
    );
    env.runtime.shutdown().await;
    env.application.cleanup().await;
}
