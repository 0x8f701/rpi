//! Orchestration subagent coverage for the 8-area checklist gaps not already
//! defended by `orchestration_jobs.rs`, `durable_orchestration_e2e.rs`,
//! `role_contracts.rs`, `sandbox_smoke.rs`, and `orchestration.rs`.
//!
//! Every test here fails on a plausible bug:
//! - Cancelling a Queued (concurrency-gated) job must never start it and must
//!   settle it as Cancelled (a bug that only cancels running jobs leaves the
//!   queued job to start after the permit frees).
//! - `spawn_tasks` at the recursion depth limit must bail with the depth
//!   message and the `task` tool must be absent at the limit (an off-by-one
//!   lets a grandchild spawn).
//! - The `OrchestrationEvent::JobUpdated` stream must publish a Running event
//!   carrying `started_at` (so a job card can compute elapsed) and exactly one
//!   terminal event with `finished_at >= started_at`; `AgentUpdated` must
//!   expose live `last_activity` (a bug that omits `started_at` hides progress).
//! - A settled job must survive a late cancel: `cancel` claims nothing, the
//!   terminal snapshot/result stay byte-stable, and no second terminal event is
//!   published (a double-settle overwrite would turn Completed into Cancelled).
//! - A cross-directory session switch cancels orchestration children via the
//!   old runtime's `shutdown()`; their jobs settle Cancelled (dropping the
//!   shutdown call lets the child keep running).
//! - REAL SMOKE: the `task` tool on a real `Application` with a faux provider
//!   spawns a child, the parent messages it through `hub`, the child reads the
//!   message and completes, and the transcript is durably recorded under
//!   `children/<parent-id>/`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use pi_agent::{
    AbortController, AgentTool, AgentToolResult, BeforeToolCallContext, BeforeToolCallFn,
    BeforeToolCallResult, BoxFuture, ThinkingLevel, ToolCallContext,
};
use pi_ai::providers::{
    FauxProviderOptions, FauxProviderRegistration, FauxResponse, register_faux_provider,
};
use pi_ai::{ContentBlock, Model, Schema, StopReason, ToolCall};
use pi_coding::{
    AgentCatalog, AgentDefinition, AgentDefinitionSource, AgentStatus, Application,
    ApplicationRuntimeCandidate, ApplicationRuntimeFactory, ApplicationRuntimeFuture,
    ChildSessionFactory, JobSnapshot, JobStatus, OrchestrationConfig, OrchestrationEvent,
    OrchestrationRuntime, PreparedSessionResume, ResourceManager, ResourceManagerOptions,
    ResourceDiscovery, Session, SessionOptions, TaskItem, TaskSpawn, ToolSelection,
    WorkspaceRoots, YieldState, start_session_in,
};
use parking_lot::Mutex as PLMutex;
use serde_json::{Value, json};
use tokio::sync::Notify;
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

fn test_model() -> Model {
    Model {
        id: "gap-coverage-model".to_owned(),
        name: "Gap Coverage Model".to_owned(),
        api: "gap-coverage-api".to_owned(),
        provider: "gap-coverage-provider".to_owned(),
        ..Model::default()
    }
}

fn tool<'a>(tools: &'a [AgentTool], name: &str) -> &'a AgentTool {
    tools
        .iter()
        .find(|candidate| candidate.name == name)
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

fn hub_tool_call(id: &str, arguments: Value) -> FauxResponse {
    FauxResponse {
        content: vec![ContentBlock::ToolCall(ToolCall {
            id: id.to_owned(),
            name: "hub".to_owned(),
            arguments,
            thought_signature: None,
        })],
        stop_reason: StopReason::ToolUse,
        error_message: None,
    }
}
/// Assistant turn that calls the child-only `yield` tool with `text` as the
/// delivered payload. A child that settles through this call projects the
/// payload as its final output (no `MISSING_YIELD_WARNING` is appended); this
/// is the explicit-delivery protocol the smoke test exercises end to end.
fn yield_tool_call(id: &str, text: &str) -> FauxResponse {
    FauxResponse {
        content: vec![ContentBlock::ToolCall(ToolCall {
            id: id.to_owned(),
            name: "yield".to_owned(),
            arguments: json!({ "text": text }),
            thought_signature: None,
        })],
        stop_reason: StopReason::ToolUse,
        error_message: None,
    }
}

/// The child-only `yield` tool wired to a spawn's [`YieldState`]. The
/// production builder is `pub(crate)`, so this mirrors it from the public API:
/// `YieldState::deliver` records the payload, the composed turn stop hook
/// (installed by the runtime on every child) fires on `was_called`, and the
/// run loop projects the payload as the job output — the real terminal-yield
/// settlement path, with no `MISSING_YIELD_WARNING` appended.
fn child_yield_tool(state: Arc<YieldState>) -> AgentTool {
    AgentTool::new(
        "yield",
        "End your work by delivering the final result. Call this exactly once, when the assigned work is complete: pass the full final deliverable as `text` — that payload becomes your delivered output and your session terminates.",
        Schema::object_ordered(vec![("text".to_owned(), Schema::string(), true)]),
        move |context| {
            let state = state.clone();
            async move {
                let text = context
                    .arguments
                    .get("text")
                    .and_then(|value| value.as_str())
                    .unwrap_or("")
                    .to_owned();
                state.deliver(text);
                Ok(AgentToolResult::text(
                    "Delivered. Your task is complete — this session ends now.",
                ))
            }
        },
    )
}

/// Blocking child factory: the stream holds until `release` cancels (or the
/// abort signal fires), then emits "retained result" or an Aborted stop. The
/// `started` notify and `current`/`peak` counters expose lifecycle so a test
/// can assert a queued job never started.
struct ControlledRuntime {
    runtime: OrchestrationRuntime,
    current: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    started: Arc<Notify>,
    release: CancellationToken,
    aborted: Arc<AtomicUsize>,
}

fn controlled_runtime(
    artifact_dir: &Path,
    max_concurrency: usize,
    max_recursion_depth: usize,
) -> ControlledRuntime {
    let current = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let release = CancellationToken::new();
    let aborted = Arc::new(AtomicUsize::new(0));
    let factory_current = current.clone();
    let factory_peak = peak.clone();
    let factory_started = started.clone();
    let factory_release = release.clone();
    let factory_aborted = aborted.clone();
    let factory: ChildSessionFactory = Arc::new(move |request| {
        let current = factory_current.clone();
        let peak = factory_peak.clone();
        let started = factory_started.clone();
        let release = factory_release.clone();
        let aborted = factory_aborted.clone();
        Box::pin(async move {
            let stream_fn: pi_agent::StreamFn = Arc::new(move |model, _context, options| {
                let current = current.clone();
                let peak = peak.clone();
                let started = started.clone();
                let release = release.clone();
                let aborted = aborted.clone();
                Box::pin(async move {
                    let stream = pi_ai::new_assistant_message_event_stream();
                    let producer = stream.clone();
                    tokio::spawn(async move {
                        let active = current.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(active, Ordering::SeqCst);
                        started.notify_waiters();
                        let abort = options
                            .stream
                            .abort_signal
                            .expect("child stream abort signal");
                        let was_aborted = tokio::select! {
                            () = release.cancelled() => false,
                            () = abort.cancelled() => true,
                        };
                        current.fetch_sub(1, Ordering::SeqCst);
                        let mut message = pi_ai::AssistantMessage::pending(&model);
                        if was_aborted {
                            aborted.fetch_add(1, Ordering::SeqCst);
                            message.stop_reason = StopReason::Aborted;
                        } else {
                            message.content.push(pi_ai::ContentBlock::text("retained result"));
                            message.stop_reason = StopReason::Stop;
                        }
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
    let mut config =
        OrchestrationConfig::new(AgentCatalog::from_agents(vec![task_definition()]), artifact_dir);
    config.max_concurrency = max_concurrency;
    config.max_recursion_depth = max_recursion_depth;
    config.idle_ttl = None;
    config.parent_model = test_model();
    ControlledRuntime {
        runtime: OrchestrationRuntime::new(config, factory).expect("runtime"),
        current,
        peak,
        started,
        release,
        aborted,
    }
}

async fn wait_for_running(count: &AtomicUsize, notify: &Notify, description: &str) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let notified = notify.notified();
            if count.load(Ordering::SeqCst) >= 1 {
                break;
            }
            notified.await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {description}"));
}
async fn wait_for_job_settled(
    runtime: &OrchestrationRuntime,
    job_id: &str,
    timeout: Duration,
) -> JobSnapshot {
    let job_id = job_id.to_owned();
    tokio::time::timeout(timeout, async {
        loop {
            if let Some(job) = runtime
                .jobs(Some(std::slice::from_ref(&job_id)))
                .into_iter()
                .next()
            {
                if job.status.is_settled() {
                    return job;
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for job {job_id} to settle"))
}

fn task_item(index: usize, id: &str, assignment: &str) -> TaskItem {
    TaskItem {
        index,
        id: id.to_owned(),
        agent: "task".to_owned(),
        assignment: assignment.to_owned(),
        todo_task_id: None,
        ..Default::default()
    }
}

/// Cancelling a Queued job (gated behind the concurrency permit) must never
/// start it and must settle it as Cancelled. A plausible bug cancels only
/// running jobs, so the queued job starts the moment the permit frees and
/// produces a Completed result instead of Cancelled.
#[tokio::test]
async fn cancel_queued_job_never_starts_and_settles_cancelled() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let controlled = controlled_runtime(artifacts.path(), 1, 2);
    let spawns = controlled
        .runtime
        .spawn_tasks(
            "Main",
            0,
            vec![
                task_item(0, "First", "runs first"),
                task_item(1, "Second", "queued behind first"),
            ],
        )
        .expect("spawn batch");

    // First acquires the single permit; Second waits Queued.
    wait_for_running(&controlled.current, &controlled.started, "first child running").await;
    let jobs = controlled.runtime.jobs(None);
    let first = jobs
        .iter()
        .find(|job| job.status == JobStatus::Running)
        .expect("first is running");
    let queued_id = jobs
        .iter()
        .find(|job| job.status == JobStatus::Queued)
        .expect("second is queued")
        .id
        .clone();
    let first_id = first.id.clone();
    assert_eq!(controlled.peak.load(Ordering::SeqCst), 1);

    // Cancel the queued job before the permit frees. `cancel_jobs` accepts a
    // job id (the user-facing cancel path); `cancel` is agent-id keyed, so a
    // bug that only cancels running/active children would leave this queued.
    let cancelled = controlled
        .runtime
        .cancel_jobs(std::slice::from_ref(&queued_id));
    assert_eq!(
        cancelled,
        vec![queued_id.clone()],
        "cancel_jobs must claim the queued job id"
    );

    let queued_terminal = wait_for_job_settled(&controlled.runtime, &queued_id, Duration::from_secs(2))
        .await;
    assert_eq!(
        queued_terminal.status,
        JobStatus::Cancelled,
        "queued job must settle Cancelled without ever running"
    );
    assert_eq!(
        controlled.peak.load(Ordering::SeqCst),
        1,
        "the cancelled queued job must never have started"
    );

    // Releasing the running child must not resurrect the cancelled job: it
    // stays Cancelled and First completes normally.
    controlled.release.cancel();
    let first_terminal = wait_for_job_settled(&controlled.runtime, &first_id, Duration::from_secs(2))
        .await;
    assert_eq!(first_terminal.status, JobStatus::Completed);
    let still_cancelled = controlled
        .runtime
        .jobs(Some(std::slice::from_ref(&queued_id)))
        .into_iter()
        .next()
        .expect("cancelled job retained");
    assert_eq!(still_cancelled.status, JobStatus::Cancelled);

    controlled.runtime.shutdown().await;
}

/// `spawn_tasks` must reject a spawn at the recursion depth limit with a clear
/// message, and the `task` tool must be absent from a depth-limit child's tool
/// set. An off-by-one (e.g. `>` instead of `>=`) lets a grandchild spawn.
#[tokio::test]
async fn spawn_tasks_at_recursion_depth_limit_rejected() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let controlled = controlled_runtime(artifacts.path(), 1, 1);

    // A child at depth 1 (== max_recursion_depth) cannot spawn grandchildren.
    let error = controlled
        .runtime
        .spawn_tasks("Child", 1, vec![task_item(0, "Grandchild", "should be refused")])
        .expect_err("depth limit must reject the spawn");
    assert!(
        error.to_string().contains("recursion depth limit"),
        "expected a recursion depth message, got: {error}"
    );

    // The task tool is removed at the limit; hub remains for IRC.
    let child_tools = controlled.runtime.agent_tools("Child", 1);
    assert!(
        child_tools.iter().all(|candidate| candidate.name != "task"),
        "task tool must be absent at the recursion depth limit"
    );
    assert!(
        child_tools.iter().any(|candidate| candidate.name == "hub"),
        "hub tool must remain at the recursion depth limit"
    );

    // A parent at depth 0 still gets the task tool.
    let root_tools = controlled.runtime.agent_tools("Main", 0);
    assert!(root_tools.iter().any(|candidate| candidate.name == "task"));

    controlled.runtime.shutdown().await;
}

/// The JobUpdated event stream must expose progress data a job card needs:
/// a Queued event (no started_at), a Running event with started_at set, exactly
/// one terminal event, finished_at >= started_at, and an AgentUpdated with a
/// live last_activity. A bug that omits started_at makes elapsed uncomputable.
#[tokio::test]
async fn job_event_stream_publishes_progress_timestamps_for_card() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let controlled = controlled_runtime(artifacts.path(), 1, 2);
    let mut events = controlled.runtime.subscribe();

    let spawn = controlled
        .runtime
        .spawn_tasks("Main", 0, vec![task_item(0, "CardChild", "card progress work")])
        .expect("spawn")
        .remove(0);
    let job_id = spawn.job_id.clone();
    let agent_id = spawn.agent_id.clone();

    wait_for_running(&controlled.current, &controlled.started, "card child running").await;

    // While running, the roster must project the child as Running with an
    // advanced last_activity (the activity feed the job card renders).
    let agent = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(agent) = controlled
                .runtime
                .list("Main")
                .into_iter()
                .find(|agent| agent.id == agent_id)
            {
                if agent.status == AgentStatus::Running {
                    return agent;
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("child projected Running");
    assert!(
        agent.last_activity >= agent.created_at,
        "running agent must expose an advanced last_activity: {agent:?}"
    );

    controlled.release.cancel();

    // Drain the event stream: a Queued event (no started_at), a Running event
    // (started_at set), exactly one terminal event with finished_at >= started_at.
    let mut queued_started_at: Option<Option<u64>> = None;
    let mut running_started_at: Option<u64> = None;
    let mut terminal_events = 0usize;
    let mut terminal_snapshot: Option<JobSnapshot> = None;
    let mut saw_agent_updated = false;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match events.recv().await {
                Ok(OrchestrationEvent::JobUpdated { job, .. }) if job.id == job_id => {
                    match job.status {
                        JobStatus::Queued => queued_started_at = Some(job.started_at),
                        JobStatus::Running => running_started_at = job.started_at,
                        _ if job.status.is_settled() => {
                            terminal_events += 1;
                            terminal_snapshot = Some(job);
                            // Drain a little longer to catch a duplicate
                            // terminal event (double-settle publish).
                            let mut guard =
                                tokio::time::interval(Duration::from_millis(60));
                            let mut extra_terminal = 0usize;
                            for _ in 0..3 {
                                guard.tick().await;
                                while let Ok(OrchestrationEvent::JobUpdated {
                                    job,
                                    ..
                                }) = events.try_recv()
                                {
                                    if job.id == job_id && job.status.is_settled() {
                                        extra_terminal += 1;
                                    }
                                }
                            }
                            assert_eq!(
                                extra_terminal, 0,
                                "a second terminal JobUpdated was published for {job_id}"
                            );
                            return;
                        }
                        _ => {}
                    }
                }
                Ok(OrchestrationEvent::AgentUpdated { agent, .. })
                    if agent.id == agent_id =>
                {
                    saw_agent_updated = true;
                }
                Ok(_) => {}
                Err(_) => return,
            }
        }
    })
    .await
    .expect("progress event stream drained");

    assert_eq!(
        queued_started_at,
        Some(None),
        "the Queued JobUpdated must advertise no start time"
    );
    let started = running_started_at.expect("a Running JobUpdated with started_at");
    let terminal = terminal_snapshot.expect("a terminal JobUpdated");
    assert_eq!(terminal.status, JobStatus::Completed);
    let finished = terminal
        .finished_at
        .expect("terminal JobUpdated must carry finished_at");
    assert!(
        finished >= started,
        "finished_at ({finished}) must be >= started_at ({started})"
    );
    assert_eq!(
        terminal_events, 1,
        "exactly one terminal JobUpdated event for the job"
    );
    assert!(
        saw_agent_updated,
        "AgentUpdated must be published for the child"
    );

    controlled.runtime.shutdown().await;
}

/// A settled job must survive a late cancel: cancel claims nothing, the
/// terminal snapshot and result stay byte-stable, and no second terminal event
/// is published. A double-settle bug overwrites Completed with Cancelled.
#[tokio::test]
async fn settled_job_survives_late_cancel_without_double_settle() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let controlled = controlled_runtime(artifacts.path(), 1, 2);
    let mut events = controlled.runtime.subscribe();

    let spawn = controlled
        .runtime
        .spawn_tasks("Main", 0, vec![task_item(0, "OnceOnly", "settle exactly once")])
        .expect("spawn")
        .remove(0);
    let job_id = spawn.job_id.clone();

    // Complete the child immediately by releasing the gate before it runs.
    controlled.release.cancel();
    let settled = wait_for_job_settled(&controlled.runtime, &job_id, Duration::from_secs(3)).await;
    assert_eq!(settled.status, JobStatus::Completed);
    let original_output = settled
        .result
        .as_ref()
        .expect("completed result")
        .output
        .clone();

    // Drain the event stream until the first terminal event arrives, then keep
    // watching briefly to detect any duplicate terminal JobUpdated.
    let mut terminal_events = 0usize;
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match events.recv().await {
                Ok(OrchestrationEvent::JobUpdated { job, .. }) if job.id == job_id => {
                    if job.status.is_settled() {
                        terminal_events += 1;
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    })
    .await
    .expect("first terminal event");

    // A late cancel against the settled job must claim nothing and must not
    // mutate the terminal snapshot or re-publish a terminal event.
    let claimed = controlled.runtime.cancel(&[job_id.clone()]);
    assert!(
        claimed.is_empty(),
        "cancel of a settled job must claim nothing: {claimed:?}"
    );

    let after = controlled
        .runtime
        .jobs(Some(std::slice::from_ref(&job_id)))
        .into_iter()
        .next()
        .expect("retained snapshot");
    assert_eq!(after.status, JobStatus::Completed);
    assert_eq!(
        after.result.as_ref().expect("retained result").output,
        original_output,
        "the terminal result must be byte-stable after a late cancel"
    );

    // Watch the channel a little longer: no duplicate terminal event.
    let mut extra_terminal = 0usize;
    let deadline = std::time::Instant::now() + Duration::from_millis(150);
    while std::time::Instant::now() < deadline {
        match events.try_recv() {
            Ok(OrchestrationEvent::JobUpdated { job, .. }) if job.id == job_id => {
                if job.status.is_settled() {
                    extra_terminal += 1;
                }
            }
            Ok(_) => {}
            Err(_) => tokio::task::yield_now().await,
        }
    }
    assert_eq!(extra_terminal, 0, "a late cancel re-published a terminal event");
    assert_eq!(terminal_events, 1, "exactly one terminal event overall");

    controlled.runtime.shutdown().await;
}

/// A cross-directory session switch cancels orchestration children via the old
/// runtime's `shutdown()`: their jobs settle Cancelled. Dropping the shutdown
/// call lets the child keep running and the job stays Running forever.
#[derive(Clone, Default)]
struct NoOpRuntimeFactory;

impl ApplicationRuntimeFactory for NoOpRuntimeFactory {
    fn build_runtime_candidate(
        &self,
        cwd: PathBuf,
        mut options: SessionOptions,
        resume: Option<PreparedSessionResume>,
    ) -> ApplicationRuntimeFuture {
        Box::pin(async move {
            options.cwd.clone_from(&cwd);
            let workspace = WorkspaceRoots::new(&cwd, Vec::<PathBuf>::new())?;
            let mut resource_options = ResourceManagerOptions::new(&cwd);
            resource_options.project_trust_override = Some(true);
            let resources = ResourceManager::new(resource_options)?;
            options.system_prompt = resources
                .snapshot()
                .system_prompt
                .clone()
                .unwrap_or_default();
            let session = Session::new_with_additional_tools_filtered_discovery_workspace_and_uri(
                options,
                Vec::new(),
                ToolSelection {
                    enable_glob: true,
                    ..ToolSelection::default()
                },
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

fn recorded_parent(cwd: &Path, session_dir: &Path) -> Session {
    let recorder =
        start_session_in(cwd, Some(&test_model()), Some("off"), Some(session_dir), None, None)
            .expect("parent recorder");
    recorder
        .persist_now()
        .expect("materialize parent header");
    let parent = Session::new(SessionOptions {
        model: test_model(),
        cwd: cwd.to_path_buf(),
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
    parent
}

/// Write a minimal resumable session JSONL into `dir` and return its path.
fn write_resume_session(dir: &Path, id: &str, message: &str) -> PathBuf {
    std::fs::create_dir_all(dir).expect("resume dir");
    let path = dir.join(format!("{id}.jsonl"));
    let header = serde_json::json!({
        "type": "session",
        "id": id,
        "model": test_model(),
        "cwd": dir,
        "systemPrompt": "",
        "thinkingLevel": "off",
        "createdAt": 1,
        "version": 2,
    });
    let user_msg = serde_json::json!({
        "type": "message",
        "role": "user",
        "content": [{"type": "text", "text": message}],
        "index": 1,
    });
    let mut bytes = serde_json::to_string(&header).expect("header");
    bytes.push('\n');
    bytes.push_str(&serde_json::to_string(&user_msg).expect("user message"));
    bytes.push('\n');
    std::fs::write(&path, bytes).expect("write resume session");
    path
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cross_cwd_session_switch_cancels_orchestration_children() {
    let source_cwd = tempfile::tempdir().expect("source cwd");
    let target_cwd = tempfile::tempdir().expect("target cwd");
    let sessions = tempfile::tempdir().expect("sessions");
    let artifacts = source_cwd.path().join(".pi").join("artifacts");
    std::fs::create_dir_all(&artifacts).expect("artifacts dir");

    let controlled = controlled_runtime(&artifacts, 1, 2);
    let runtime = controlled.runtime.clone();

    let parent = recorded_parent(source_cwd.path(), sessions.path());
    let application = Application::new(parent).await;
    application
        .attach_orchestration(runtime.clone())
        .expect("attach orchestration to the application");
    application
        .attach_runtime_factory(Arc::new(NoOpRuntimeFactory))
        .expect("factory");

    let mut events = runtime.subscribe();
    let spawn = runtime
        .spawn_tasks("Main", 0, vec![task_item(0, "SwitchVictim", "blocked child")])
        .expect("spawn")
        .remove(0);
    let job_id = spawn.job_id.clone();
    wait_for_running(
        &controlled.current,
        &controlled.started,
        "switch victim running",
    )
    .await;

    // Switch to a session in a *different* working directory: the cross-cwd
    // path shuts the old orchestration runtime down, cancelling its children.
    // `shutdown()` drains and then prunes retained jobs, so observe the outcome
    // by polling both the event stream (a Cancelled terminal JobUpdated during
    // the drain) and the job snapshot (pruned to None once shutdown completes).
    let target_file = write_resume_session(target_cwd.path(), "switch-target", "target message");
    let switch = tokio::time::timeout(
        Duration::from_secs(10),
        application.switch_session(&target_file),
    )
    .await
    .expect("switch session timeout")
    .expect("switch session");
    assert!(!switch.cancelled, "cross-cwd switch must complete");
    assert!(
        !application.orchestration_runtime().is_some(),
        "the replacement runtime has no orchestration; the old one was taken for shutdown"
    );

    // Exactly one child started (the switch victim); the cancelled child must
    // not produce its completion output. The child stream ignores abort (as
    // `cancellation_settles_when_child_stream_ignores_abort` exercises), so the
    // observable contract is the terminal JobUpdated, not the stream internals.
    assert_eq!(
        controlled.peak.load(Ordering::SeqCst),
        1,
        "exactly one child started before the switch"
    );

    // The switch's shutdown cancels the child. `shutdown()` drains and then
    // prunes retained jobs, so observe the outcome by polling both the event
    // stream (a Cancelled terminal JobUpdated published during the drain) and
    // the job snapshot (pruned to None once shutdown completes). A dropped
    // shutdown call leaves the child Running in both views and the test times
    // out — the real bug this test defends against.
    let outcome = tokio::time::timeout(Duration::from_secs(8), async {
        let job_id = job_id.clone();
        loop {
            while let Ok(event) = events.try_recv() {
                if let OrchestrationEvent::JobUpdated { job, .. } = event
                    && job.id == job_id
                    && job.status.is_settled()
                {
                    return job.status;
                }
            }
            match runtime
                .jobs(Some(std::slice::from_ref(&job_id)))
                .into_iter()
                .next()
            {
                // Pruned: shutdown ran, drained, and removed the job.
                None => return JobStatus::Cancelled,
                Some(job) if job.status.is_settled() => return job.status,
                Some(_) => {}
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the switched-out child must be cancelled or pruned by the switch shutdown");
    assert_eq!(
        outcome,
        JobStatus::Cancelled,
        "cross-cwd switch must cancel the running child"
    );

    application.cleanup().await;
}

/// REAL SMOKE: the `task` tool on a real `Application` with a faux provider
/// spawns a child, the parent messages it through `hub`, the child reads the
/// message via `hub inbox` and completes, and the transcript is durably
/// recorded under `children/<parent-id>/`. Verifies the whole subagent path.
#[tokio::test]
async fn real_application_task_tool_end_to_end_smoke() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let sessions = tempfile::tempdir().expect("sessions");
    let (parent, parent_id, parent_path) = real_parent(artifacts.path(), sessions.path());
    let child_root = parent_path
        .parent()
        .expect("parent dir")
        .join("children")
        .join(&parent_id);

    // Real faux provider for the child: list the roster, drain the inbox, then
    // emit the completion text. The child resolves the provider through the
    // normal session machinery (stream_fn is None), exercising the real path.
    let suffix = uuid::Uuid::now_v7().to_string();
    let api = format!("smoke-api-{suffix}");
    let provider = format!("smoke-provider-{suffix}");
    let child_model = Model {
        id: format!("smoke-model-{suffix}"),
        name: "Smoke Child".to_owned(),
        api: api.clone(),
        provider: provider.clone(),
        ..Model::default()
    };
    let registration = register_faux_provider(FauxProviderOptions {
        api,
        provider,
        models: vec![child_model.clone()],
        chunk_size: 1,
    });
    registration.set_responses(vec![
        hub_tool_call("list-1", json!({ "op": "list" })),
        hub_tool_call("wait-1", json!({ "op": "wait", "timeoutMs": 5000 })),
        yield_tool_call("yield-1", "smoke complete"),
    ]);

    // The child factory installs a before_tool_call hook that notifies the test
    // the instant the child enters its `hub wait` call. `hub wait` registers a
    // MessageWaiter, so the parent's send is queued (not steered) and the wait
    // deterministically returns the message — the body lands in the transcript.
    let wait_started = Arc::new(Notify::new());
    let factory_wait_started = wait_started.clone();
    let factory: ChildSessionFactory = Arc::new(move |request| {
        let wait_started = factory_wait_started.clone();
        let child_model = child_model.clone();
        Box::pin(async move {
            let before: BeforeToolCallFn = {
                let wait_started = wait_started.clone();
                Arc::new(
                    move |context: BeforeToolCallContext| {
                        let wait_started = wait_started.clone();
                        Box::pin(async move {
                            if context.tool_call.name == "hub"
                                && context
                                    .arguments
                                    .get("op")
                                    .and_then(|value| value.as_str())
                                    == Some("wait")
                            {
                                wait_started.notify_one();
                            }
                            Ok(BeforeToolCallResult::default())
                        })
                            as BoxFuture<anyhow::Result<BeforeToolCallResult>>
                    },
                )
            };
            // The child settles through the real terminal-yield protocol: the
            // factory appends the child-only `yield` tool (wired to the
            // spawn's YieldState) alongside the orchestration plumbing, so a
            // `yield` call delivers the payload and ends the run instead of
            // naturally completing and attracting MISSING_YIELD_WARNING.
            let mut tools = request.orchestration_tools;
            tools.push(child_yield_tool(request.yield_state.clone()));
            Session::new(SessionOptions {
                model: child_model,
                cwd: std::env::current_dir().expect("cwd"),
                system_prompt: request.system_prompt,
                thinking_level: ThinkingLevel::Off,
                api_key: "faux".to_owned(),
                compaction: None,
                stream_options: Default::default(),
                tools: Some(tools),
                before_tool_call: Some(before),
                after_tool_call: None,
                stream_fn: None,
                auth_resolver: None,
            })
        })
    });

    let mut config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![task_definition()]),
        artifacts.path(),
    );
    config.idle_ttl = None;
    config.parent_model = test_model();
    let runtime = OrchestrationRuntime::new(config, factory).expect("runtime");
    let application = Application::new(parent).await;
    application
        .attach_orchestration(runtime.clone())
        .expect("attach orchestration");

    // Drive the real `task` tool. The Application is attached (real parent
    // session, durable binding, event forwarding) and the tool instance is the
    // same one the Application injects into the parent tool surface; driving it
    // exercises the full subagent path through the real machinery.
    let tools = runtime.agent_tools("Main", 0);
    let task = tool(&tools, "task");
    let spawn_result = tokio::time::timeout(
        Duration::from_secs(5),
        (task.execute)(context(
            "smoke-spawn",
            json!({ "name": "SmokeChild", "task": "prove the subagent path end to end" }),
        )),
    )
    .await
    .expect("task tool must return while the child runs")
    .expect("task spawn ok");
    let spawns: Vec<TaskSpawn> =
        serde_json::from_value(spawn_result.details).expect("spawn details");
    let spawn = spawns.into_iter().next().expect("one spawn");
    assert_eq!(spawn.status, JobStatus::Queued);
    assert!(!spawn.job_id.is_empty());

    // Parent -> child message once the child has registered its hub wait.
    wait_started.notified().await;
    const PARENT_BODY: &str = "hello-from-parent";
    let receipts = runtime.send("Main", &spawn.agent_id, PARENT_BODY, None);
    assert!(
        receipts.iter().all(|receipt| receipt.error.is_none()),
        "parent send must not error: {receipts:?}"
    );

    // The child receives the message (hub wait returns it) and completes with the final text.
    let job = wait_for_job_settled(&runtime, &spawn.job_id, Duration::from_secs(15)).await;
    assert_eq!(job.status, JobStatus::Completed, "smoke job: {job:?}");
    let result = job.result.as_ref().expect("smoke result");
    assert_eq!(result.agent, "task");
    assert_eq!(result.output, "smoke complete");
    assert!(result.error.is_none(), "smoke result error: {:?}", result.error);

    // The child transcript is durably recorded under children/<parent-id>/. The
    // recorder names the file with a generated session id (not the agent id),
    // so locate it by scanning the child root for the one .jsonl transcript.
    let child_jsonl = std::fs::read_dir(&child_root)
        .expect("child root dir exists")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .find(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
        .expect("one durable child transcript jsonl under child root");
    let transcript = std::fs::read_to_string(&child_jsonl)
        .expect("durable child transcript jsonl");
    assert!(
        transcript.contains("smoke complete"),
        "the final assistant turn must be recorded in the child transcript"
    );
    assert!(
        transcript.contains(PARENT_BODY),
        "the parent hub message must be recorded in the child transcript (returned via hub wait)"
    );

    application.cleanup().await;
    registration.unregister();
}

fn real_parent(artifacts: &Path, session_dir: &Path) -> (Session, String, PathBuf) {
    let recorder =
        start_session_in(artifacts, Some(&test_model()), Some("off"), Some(session_dir), None, None)
            .expect("parent recorder");
    recorder
        .persist_now()
        .expect("materialize real parent header");
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