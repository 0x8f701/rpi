//! Orchestration subagent coverage for the 8-area checklist gaps not already
//! defended by `orchestration_jobs.rs`, `durable_orchestration_e2e.rs`,
//! `role_contracts.rs`, and `orchestration.rs`.
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
//! - Same- and cross-directory session switches retain resident orchestration
//!   by exact `(session id, canonical recorder path)`, preserving running
//!   children, mailbox delivery, waits, history, and application event routing.
//! - REAL SMOKE: the `task` tool on a real `Application` with a faux provider
//!   spawns a child, the parent messages it through `hub`, the child reads the
//!   message and completes, and the transcript is durably recorded under
//!   `children/<parent-id>/`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use parking_lot::Mutex as PLMutex;
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
    ChildSessionFactory, ExtensionMode, ExtensionPermissionSet, ExtensionRuntime,
    ExtensionRuntimeOptions, JobSnapshot, JobStatus, OrchestrationConfig, OrchestrationEvent,
    OrchestrationRuntime, PreparedSessionResume, ResourceDiscovery, ResourceManager,
    ResourceManagerOptions, Session, SessionOptions, TaskItem, TaskSpawn, ToolSelection,
    WorkspaceRoots, YieldState, start_session_in,
};
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
const HERMETIC_DANGER_SETTINGS: &str = r#"{"sandbox":{"mode":"danger-full-access"}}"#;

fn hermetic_resources(cwd: &Path) -> ResourceManager {
    let agent_dir = cwd.parent().expect("workspace parent").join(format!(
        "{}-agent-home",
        cwd.file_name().expect("workspace name").to_string_lossy()
    ));
    std::fs::create_dir_all(&agent_dir).expect("hermetic agent dir");
    std::fs::write(agent_dir.join("settings.json"), HERMETIC_DANGER_SETTINGS)
        .expect("hermetic settings");
    let mut options = ResourceManagerOptions::new(cwd);
    options.agent_dir = agent_dir;
    options.disable_extensions = true;
    options.disable_skills = true;
    options.disable_prompt_templates = true;
    options.disable_themes = true;
    options.disable_context_files = true;
    options.project_trust_override = Some(true);
    ResourceManager::new(options).expect("hermetic resources")
}

fn task_definition() -> AgentDefinition {
    AgentDefinition {
        name: "task".to_owned(),
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
        id: "gap-coverage-model".to_owned(),
        name: "Gap Coverage Model".to_owned(),
        api: "gap-coverage-api".to_owned(),
        provider: "gap-coverage-provider".to_owned(),
        base_url: "http://localhost:0".to_owned(),
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
        approval_token: None,
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
                            message
                                .content
                                .push(pi_ai::ContentBlock::text("retained result"));
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
                auth_manager: None,
            })
        })
    });
    let mut config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![task_definition()]),
        artifact_dir,
    );
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
    wait_for_running(
        &controlled.current,
        &controlled.started,
        "first child running",
    )
    .await;
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

    let queued_terminal =
        wait_for_job_settled(&controlled.runtime, &queued_id, Duration::from_secs(2)).await;
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
    let first_terminal =
        wait_for_job_settled(&controlled.runtime, &first_id, Duration::from_secs(2)).await;
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
        .spawn_tasks(
            "Child",
            1,
            vec![task_item(0, "Grandchild", "should be refused")],
        )
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
        .spawn_tasks(
            "Main",
            0,
            vec![task_item(0, "CardChild", "card progress work")],
        )
        .expect("spawn")
        .remove(0);
    let job_id = spawn.job_id.clone();
    let agent_id = spawn.agent_id.clone();

    wait_for_running(
        &controlled.current,
        &controlled.started,
        "card child running",
    )
    .await;

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
                            let mut guard = tokio::time::interval(Duration::from_millis(60));
                            let mut extra_terminal = 0usize;
                            for _ in 0..3 {
                                guard.tick().await;
                                while let Ok(OrchestrationEvent::JobUpdated { job, .. }) =
                                    events.try_recv()
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
                Ok(OrchestrationEvent::AgentUpdated { agent, .. }) if agent.id == agent_id => {
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
        .spawn_tasks(
            "Main",
            0,
            vec![task_item(0, "OnceOnly", "settle exactly once")],
        )
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
    assert_eq!(
        extra_terminal, 0,
        "a late cancel re-published a terminal event"
    );
    assert_eq!(terminal_events, 1, "exactly one terminal event overall");

    controlled.runtime.shutdown().await;
}

/// A cross-directory view switch must detach, not shut down, the source
/// session's resident orchestration runtime. Switching back by the exact
/// recorder identity reattaches the same group, child, job, mailbox, and wait.
#[derive(Clone)]
struct ResidentRuntimeFactory {
    target_artifacts: PathBuf,
    extension_namespaces: Option<Arc<PLMutex<HashMap<PathBuf, Vec<String>>>>>,
}

impl ApplicationRuntimeFactory for ResidentRuntimeFactory {
    fn build_runtime_candidate(
        &self,
        cwd: PathBuf,
        mut options: SessionOptions,
        resume: Option<PreparedSessionResume>,
    ) -> ApplicationRuntimeFuture {
        let target_artifacts = self.target_artifacts.clone();
        let extension_namespaces = self.extension_namespaces.clone();
        Box::pin(async move {
            options.cwd.clone_from(&cwd);
            let workspace = WorkspaceRoots::new(&cwd, Vec::<PathBuf>::new())?;
            let resources = hermetic_resources(&cwd);
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
            let factory = OrchestrationRuntime::child_factory_from_session(&session);
            let mut config = OrchestrationConfig::new(
                AgentCatalog::from_agents(vec![task_definition()]),
                target_artifacts,
            );
            config.idle_ttl = None;
            config.parent_model = test_model();
            let orchestration = OrchestrationRuntime::new(config, factory)?;
            let mut candidate =
                ApplicationRuntimeCandidate::new(session).with_orchestration(orchestration);
            if let Some(extension_namespaces) = extension_namespaces {
                let extension = ExtensionRuntime::process(
                    None,
                    ExtensionRuntimeOptions {
                        mode: ExtensionMode::Tui,
                        ..ExtensionRuntimeOptions::default()
                    },
                );
                extension_namespaces
                    .lock()
                    .entry(cwd)
                    .or_default()
                    .push(extension.provider_namespace().to_owned());
                candidate =
                    candidate.with_extensions(extension, ExtensionPermissionSet::deny_all());
            }
            Ok(candidate)
        })
    }
}

fn recorded_parent(cwd: &Path, session_dir: &Path) -> Session {
    let recorder = start_session_in(
        cwd,
        Some(&test_model()),
        Some("off"),
        Some(session_dir),
        None,
        None,
    )
    .expect("parent recorder");
    recorder.persist_now().expect("materialize parent header");
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
        auth_manager: None,
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
async fn queued_orchestration_message(application: &Application, needle: &str) -> bool {
    application
        .queued_messages()
        .await
        .0
        .iter()
        .any(|message| match message {
            pi_ai::Message::Custom(custom) => custom.content.to_blocks().iter().any(
                |block| matches!(block, ContentBlock::Text { text, .. } if text.contains(needle)),
            ),
            _ => false,
        })
}

async fn wait_for_application_orchestration_group(
    events: &mut tokio::sync::broadcast::Receiver<pi_coding::ApplicationEvent>,
    expected_group: &str,
    forbidden_group: Option<&str>,
) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match events.recv().await {
                Ok(pi_coding::ApplicationEvent::Orchestration(event)) => {
                    let group = match event {
                        OrchestrationEvent::JobUpdated { group_id, .. }
                        | OrchestrationEvent::AgentUpdated { group_id, .. }
                        | OrchestrationEvent::MessageDelivered { group_id, .. } => group_id,
                    };
                    assert_ne!(
                        forbidden_group,
                        Some(group.as_str()),
                        "resident source group routed through the active target application",
                    );
                    if group == expected_group {
                        break;
                    }
                }
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    panic!("application event channel closed")
                }
            }
        }
    })
    .await
    .expect("expected orchestration group was not routed through the application");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_cwd_switch_round_trip_is_transactional_and_routes_only_target_events() {
    let cwd = tempfile::tempdir().expect("cwd");
    let sessions = tempfile::tempdir().expect("sessions");
    let artifacts = cwd.path().join(".pi").join("artifacts");
    std::fs::create_dir_all(&artifacts).expect("artifacts dir");
    let model = test_model();
    let _registration = register_faux_provider(FauxProviderOptions {
        api: model.api.clone(),
        provider: model.provider.clone(),
        models: vec![model],
        chunk_size: 1,
    });

    let ControlledRuntime {
        runtime: runtime_owner,
        current,
        started,
        release,
        ..
    } = controlled_runtime(&artifacts, 1, 2);
    let runtime_a = runtime_owner.clone();
    let parent = recorded_parent(cwd.path(), sessions.path());
    parent
        .attach_resources(hermetic_resources(cwd.path()))
        .await
        .expect("source resources");
    let source_file = parent.recorder_info().expect("source identity").1;
    let target_file = write_resume_session(cwd.path(), "same-cwd-target", "target message");
    let application = Application::from_runtime_candidate(
        ApplicationRuntimeCandidate::new(parent).with_orchestration(runtime_owner),
    )
    .await
    .expect("application");
    let mut application_events = application.subscribe();

    let spawn = runtime_a
        .spawn_tasks(
            "Main",
            0,
            vec![task_item(0, "SameCwdSurvivor", "blocked child")],
        )
        .expect("spawn")
        .remove(0);
    wait_for_running(&current, &started, "same-cwd child").await;
    let wait = tokio::spawn({
        let runtime = runtime_a.clone();
        async move {
            runtime
                .wait_message(
                    "Main",
                    Some("SameCwdSurvivor"),
                    Some(Duration::from_secs(10)),
                    None,
                )
                .await
        }
    });
    tokio::task::yield_now().await;
    let ask = runtime_a
        .send("Main", "SameCwdSurvivor", "same-cwd ask", None)
        .remove(0);
    let ask_id = ask.message_id.expect("ask id");

    application
        .switch_session(&target_file)
        .await
        .expect("switch to B");
    let runtime_b = application.orchestration_runtime().expect("target runtime");
    assert_ne!(
        runtime_b.group_id(),
        runtime_a.group_id(),
        "A must never be rebound for B"
    );
    assert_eq!(runtime_a.active_child_count(), 1);
    assert!(!runtime_a.is_shutdown());

    while application_events.try_recv().is_ok() {}
    let target_delivery = runtime_b
        .send("Main", "Main", "target-only delivery", None)
        .remove(0);
    assert!(target_delivery.error.is_none(), "{target_delivery:?}");
    wait_for_application_orchestration_group(
        &mut application_events,
        runtime_b.group_id(),
        Some(runtime_a.group_id()),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if queued_orchestration_message(&application, "target-only delivery").await {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("target mailbox replay/event steering");

    let reply = runtime_a
        .send(
            "SameCwdSurvivor",
            "Main",
            "source waited reply",
            Some(ask_id.clone()),
        )
        .remove(0);
    assert!(reply.error.is_none(), "{reply:?}");
    let waited = wait
        .await
        .expect("wait join")
        .expect("wait result")
        .expect("reply");
    assert_eq!(waited.body, "source waited reply");

    let source_delivery = runtime_a
        .send("SameCwdSurvivor", "Main", "source resident reply", None)
        .remove(0);
    assert!(source_delivery.error.is_none(), "{source_delivery:?}");
    let source_main_after = runtime_a.inbox("Main", true).expect("source Main inbox");
    assert!(
        source_main_after
            .iter()
            .any(|message| message.body == "source resident reply"),
        "resident source reply must remain in the detached source mailbox",
    );
    let target_main_after = runtime_b.inbox("Main", true).expect("target Main inbox");
    assert!(
        target_main_after
            .iter()
            .all(|message| message.body != "source resident reply"),
        "resident source reply leaked into the target mailbox: {target_main_after:?}",
    );
    let target_barrier = runtime_b
        .send("Main", "Main", "target event barrier", None)
        .remove(0);
    assert!(target_barrier.error.is_none(), "{target_barrier:?}");
    wait_for_application_orchestration_group(
        &mut application_events,
        runtime_b.group_id(),
        Some(runtime_a.group_id()),
    )
    .await;
    assert!(
        !queued_orchestration_message(&application, "source resident reply").await,
        "resident A forwarder leaked into B",
    );
    while let Ok(event) = application_events.try_recv() {
        if let pi_coding::ApplicationEvent::Orchestration(event) = event {
            let group = match event {
                OrchestrationEvent::JobUpdated { group_id, .. }
                | OrchestrationEvent::AgentUpdated { group_id, .. }
                | OrchestrationEvent::MessageDelivered { group_id, .. } => group_id,
            };
            assert_ne!(
                group,
                runtime_a.group_id(),
                "resident source event routed through B while detached",
            );
        }
    }

    application
        .switch_session(&source_file)
        .await
        .expect("switch back to A");
    let rebound = application.orchestration_runtime().expect("rebound A");
    assert_eq!(rebound.group_id(), runtime_a.group_id());
    assert_eq!(
        rebound.jobs(Some(std::slice::from_ref(&spawn.job_id)))[0].status,
        JobStatus::Running
    );
    assert_eq!(
        rebound
            .delivered_messages()
            .iter()
            .filter(|message| message.id == waited.id)
            .count(),
        1
    );
    while application_events.try_recv().is_ok() {}
    let fresh_a_event = rebound
        .send("SameCwdSurvivor", "Main", "source active event", None)
        .remove(0);
    assert!(fresh_a_event.error.is_none(), "{fresh_a_event:?}");
    wait_for_application_orchestration_group(
        &mut application_events,
        runtime_a.group_id(),
        Some(runtime_b.group_id()),
    )
    .await;

    release.cancel();
    let settled = wait_for_job_settled(&rebound, &spawn.job_id, Duration::from_secs(10)).await;
    assert_eq!(settled.status, JobStatus::Completed);
    application.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_session_id_at_distinct_paths_never_aliases_and_post_take_abort_rolls_back() {
    let source_cwd = tempfile::tempdir().expect("source cwd");
    let target_cwd = tempfile::tempdir().expect("target cwd");
    let sessions = tempfile::tempdir().expect("sessions");
    let artifacts = source_cwd.path().join(".pi").join("artifacts");
    std::fs::create_dir_all(&artifacts).expect("artifacts dir");
    let model = test_model();
    let _registration = register_faux_provider(FauxProviderOptions {
        api: model.api.clone(),
        provider: model.provider.clone(),
        models: vec![model],
        chunk_size: 1,
    });
    let ControlledRuntime {
        runtime: runtime_owner,
        ..
    } = controlled_runtime(&artifacts, 1, 2);
    let runtime_a = runtime_owner.clone();
    let parent = recorded_parent(source_cwd.path(), sessions.path());
    parent
        .attach_resources(hermetic_resources(source_cwd.path()))
        .await
        .expect("source resources");
    let (source_id, source_file) = parent.recorder_info().expect("source identity");
    let target_file = write_resume_session(target_cwd.path(), &source_id, "same id, other path");
    let application = Application::from_runtime_candidate(
        ApplicationRuntimeCandidate::new(parent).with_orchestration(runtime_owner),
    )
    .await
    .expect("application");
    application
        .attach_runtime_factory(Arc::new(ResidentRuntimeFactory {
            target_artifacts: target_cwd.path().join(".pi").join("artifacts"),
            extension_namespaces: None,
        }))
        .expect("factory");

    application
        .switch_session(&target_file)
        .await
        .expect("switch target");
    let runtime_b = application.orchestration_runtime().expect("target runtime");
    assert_ne!(
        runtime_b.group_id(),
        runtime_a.group_id(),
        "session id alone must not select A"
    );

    let gate = application.arm_cutover_after_resident_take_gate_for_test();
    let switch_back = tokio::spawn({
        let application = application.clone();
        let source_file = source_file.clone();
        async move { application.switch_session(&source_file).await }
    });
    gate.wait_entered().await;
    switch_back.abort();
    let _ = switch_back.await;
    gate.release();
    assert_eq!(
        application
            .orchestration_runtime()
            .expect("B remains active")
            .group_id(),
        runtime_b.group_id()
    );

    application
        .switch_session(&source_file)
        .await
        .expect("retry switch back");
    assert_eq!(
        application
            .orchestration_runtime()
            .expect("A restored")
            .group_id(),
        runtime_a.group_id()
    );
    application.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cleanup_waits_for_post_take_claim_and_shuts_every_orchestration_owner() {
    let source_cwd = tempfile::tempdir().expect("source cwd");
    let target_cwd = tempfile::tempdir().expect("target cwd");
    let sessions = tempfile::tempdir().expect("sessions");
    let artifacts = source_cwd.path().join(".pi").join("artifacts");
    std::fs::create_dir_all(&artifacts).expect("artifacts dir");
    let model = test_model();
    let _registration = register_faux_provider(FauxProviderOptions {
        api: model.api.clone(),
        provider: model.provider.clone(),
        models: vec![model],
        chunk_size: 1,
    });
    let ControlledRuntime {
        runtime: runtime_owner,
        ..
    } = controlled_runtime(&artifacts, 1, 2);
    let runtime_a = runtime_owner.clone();
    let parent = recorded_parent(source_cwd.path(), sessions.path());
    parent
        .attach_resources(hermetic_resources(source_cwd.path()))
        .await
        .expect("source resources");
    let source_file = parent.recorder_info().expect("source identity").1;
    let target_file = write_resume_session(target_cwd.path(), "cleanup-target", "target");
    let application = Application::from_runtime_candidate(
        ApplicationRuntimeCandidate::new(parent).with_orchestration(runtime_owner),
    )
    .await
    .expect("application");
    application
        .attach_runtime_factory(Arc::new(ResidentRuntimeFactory {
            target_artifacts: target_cwd.path().join(".pi").join("artifacts"),
            extension_namespaces: None,
        }))
        .expect("factory");
    application
        .switch_session(&target_file)
        .await
        .expect("switch target");
    let runtime_b = application.orchestration_runtime().expect("target runtime");

    let gate = application.arm_cutover_after_resident_take_gate_for_test();
    let switch_back = tokio::spawn({
        let application = application.clone();
        async move { application.switch_session(&source_file).await }
    });
    gate.wait_entered().await;
    let cleanup = application.cleanup();
    tokio::pin!(cleanup);
    let cleanup_poll = std::future::poll_fn(|context| {
        std::task::Poll::Ready(std::future::Future::poll(cleanup.as_mut(), context))
    })
    .await;
    assert!(
        matches!(cleanup_poll, std::task::Poll::Pending),
        "cleanup returned while a resident owner was claimed",
    );
    gate.release();
    switch_back
        .await
        .expect("switch join")
        .expect("switch back");
    cleanup.await;
    assert!(
        runtime_a.is_shutdown(),
        "claimed source owner survived cleanup"
    );
    assert!(
        runtime_b.is_shutdown(),
        "resident target owner survived cleanup"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cross_cwd_session_switch_preserves_resident_orchestration_lifecycle() {
    let source_cwd = tempfile::tempdir().expect("source cwd");
    let target_cwd = tempfile::tempdir().expect("target cwd");
    let sessions = tempfile::tempdir().expect("sessions");
    let artifacts = source_cwd.path().join(".pi").join("artifacts");
    std::fs::create_dir_all(&artifacts).expect("artifacts dir");
    let model = test_model();
    let _registration = register_faux_provider(FauxProviderOptions {
        api: model.api.clone(),
        provider: model.provider.clone(),
        models: vec![model],
        chunk_size: 1,
    });
    let extension_namespaces = Arc::new(PLMutex::new(HashMap::<PathBuf, Vec<String>>::new()));

    let ControlledRuntime {
        runtime: runtime_owner,
        current,
        started,
        release,
        ..
    } = controlled_runtime(&artifacts, 1, 2);
    let runtime = runtime_owner.clone();
    let parent = recorded_parent(source_cwd.path(), sessions.path());
    parent
        .attach_resources(hermetic_resources(source_cwd.path()))
        .await
        .expect("attach source resources");
    let source_file = parent.recorder_info().expect("source identity").1;
    let application = Application::from_runtime_candidate(
        ApplicationRuntimeCandidate::new(parent).with_orchestration(runtime_owner),
    )
    .await
    .expect("publish resident application");
    application
        .attach_runtime_factory(Arc::new(ResidentRuntimeFactory {
            target_artifacts: target_cwd.path().join(".pi").join("artifacts"),
            extension_namespaces: Some(extension_namespaces.clone()),
        }))
        .expect("factory");

    let spawn = runtime
        .spawn_tasks(
            "Main",
            0,
            vec![task_item(0, "SwitchSurvivor", "blocked child")],
        )
        .expect("spawn")
        .remove(0);
    let original_group_id = runtime.group_id().to_owned();
    let original_job = runtime
        .jobs(Some(std::slice::from_ref(&spawn.job_id)))
        .remove(0);
    wait_for_running(&current, &started, "switch survivor running").await;

    let pending_wait = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .wait_message(
                    "Main",
                    Some("SwitchSurvivor"),
                    Some(Duration::from_secs(10)),
                    None,
                )
                .await
        }
    });
    tokio::task::yield_now().await;
    let ask_receipt = runtime
        .send("Main", "SwitchSurvivor", "resident ask", None)
        .remove(0);
    let ask_message_id = ask_receipt.message_id.clone().expect("ask message id");
    assert!(ask_receipt.error.is_none(), "ask receipt: {ask_receipt:?}");

    let target_file = write_resume_session(target_cwd.path(), "switch-target", "target message");
    let switch = tokio::time::timeout(
        Duration::from_secs(10),
        application.switch_session(&target_file),
    )
    .await
    .expect("switch session timeout")
    .expect("switch session");
    assert!(!switch.cancelled, "cross-cwd switch must complete");
    let target_runtime = application.orchestration_runtime().expect("target runtime");
    assert_ne!(target_runtime.group_id(), original_group_id);
    let first_target_extension = application
        .extension_runtime()
        .expect("first target extension");
    assert_eq!(
        extension_namespaces
            .lock()
            .get(target_cwd.path())
            .expect("first target namespace")
            .as_slice(),
        &[first_target_extension.provider_namespace().to_owned()],
    );
    assert_eq!(
        runtime.active_child_count(),
        1,
        "source child must remain resident"
    );
    assert_eq!(
        runtime.jobs(Some(std::slice::from_ref(&spawn.job_id)))[0].status,
        JobStatus::Running,
    );
    assert!(
        !runtime.is_shutdown(),
        "view switch must not shut the runtime down"
    );

    let reply_receipt = runtime
        .send(
            "SwitchSurvivor",
            "Main",
            "resident reply",
            Some(ask_message_id.clone()),
        )
        .remove(0);
    assert!(
        reply_receipt.error.is_none(),
        "reply receipt: {reply_receipt:?}"
    );
    let waited = pending_wait
        .await
        .expect("pending wait join")
        .expect("pending wait result")
        .expect("pending wait message");
    assert_eq!(waited.body, "resident reply");
    assert_eq!(waited.reply_to.as_deref(), Some(ask_message_id.as_str()));

    let switched_back = tokio::time::timeout(
        Duration::from_secs(10),
        application.switch_session(&source_file),
    )
    .await
    .expect("switch back timeout")
    .expect("switch back");
    assert!(!switched_back.cancelled);
    let rebound = application
        .orchestration_runtime()
        .expect("resident rebound");
    assert_eq!(rebound.group_id(), original_group_id.as_str());
    let rebound_job = rebound
        .jobs(Some(std::slice::from_ref(&spawn.job_id)))
        .remove(0);
    assert_eq!(rebound_job.id, original_job.id);
    assert_eq!(rebound_job.agent_id, original_job.agent_id);
    assert_eq!(rebound_job.created_at, original_job.created_at);
    assert_eq!(rebound_job.status, JobStatus::Running);
    let matching_replies = rebound
        .delivered_messages()
        .into_iter()
        .filter(|message| {
            message.from == "SwitchSurvivor"
                && message.reply_to.as_deref() == Some(ask_message_id.as_str())
        })
        .count();
    assert_eq!(
        matching_replies, 1,
        "reply receipt/history must remain exactly once"
    );

    release.cancel();
    let settled = wait_for_job_settled(&rebound, &spawn.job_id, Duration::from_secs(10)).await;
    assert!(
        settled.status.is_settled(),
        "released child must settle without leaking"
    );
    application
        .switch_session(&target_file)
        .await
        .expect("reattach target with fresh extension generation");
    let target_namespaces = extension_namespaces
        .lock()
        .get(target_cwd.path())
        .cloned()
        .expect("target extension namespaces");
    assert_eq!(
        target_namespaces.len(),
        2,
        "target runtime must be rebuilt before resident reattach"
    );
    let active_extension = application
        .extension_runtime()
        .expect("reattached target extension");
    assert_eq!(active_extension.provider_namespace(), target_namespaces[1]);
    assert_ne!(active_extension.provider_namespace(), target_namespaces[0]);
    application.cleanup().await;
    assert!(
        runtime.is_shutdown(),
        "true application teardown shuts the resident runtime down"
    );
    assert_eq!(
        runtime.active_child_count(),
        0,
        "teardown must await child exit"
    );
    let wait_error = runtime
        .wait_message("Main", None, None, None)
        .await
        .expect_err("post-teardown wait must fail");
    assert!(
        wait_error.to_string().contains("orchestration shut down"),
        "{wait_error:#}"
    );
}

/// A sibling ask/reply must survive a cross-directory session switch and
/// reconnect, reach ONLY its exact addressed authority (the sibling, never
/// Main or an unrelated sibling), and never leak its body/identity into the
/// unrelated target Main session's transcript. The source runtime's Main
/// forwarder is detached on a cross-cwd switch, so a resident reply to Main
/// stays queued in the source mailbox (not steered into the target) and is
/// replayed into the source session on reconnect. A plausible bug routes the
/// resident runtime's Main delivery to whichever session is currently active
/// (leaking the sibling exchange into the target transcript) or delivers a
/// sibling reply to every agent (breaking authority isolation).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sibling_ask_reply_survives_cross_cwd_switch_and_isolates_exact_authority() {
    let source_cwd = tempfile::tempdir().expect("source cwd");
    let target_cwd = tempfile::tempdir().expect("target cwd");
    let sessions = tempfile::tempdir().expect("sessions");
    let artifacts = source_cwd.path().join(".pi").join("artifacts");
    std::fs::create_dir_all(&artifacts).expect("artifacts dir");
    let model = test_model();
    let _registration = register_faux_provider(FauxProviderOptions {
        api: model.api.clone(),
        provider: model.provider.clone(),
        models: vec![model],
        chunk_size: 1,
    });

    let ControlledRuntime {
        runtime: runtime_owner,
        current,
        started,
        release,
        ..
    } = controlled_runtime(&artifacts, 3, 2);
    let runtime_a = runtime_owner.clone();
    let parent = recorded_parent(source_cwd.path(), sessions.path());
    parent
        .attach_resources(hermetic_resources(source_cwd.path()))
        .await
        .expect("attach source resources");
    let source_file = parent.recorder_info().expect("source identity").1;
    let application = Application::from_runtime_candidate(
        ApplicationRuntimeCandidate::new(parent).with_orchestration(runtime_owner),
    )
    .await
    .expect("publish resident application");
    application
        .attach_runtime_factory(Arc::new(ResidentRuntimeFactory {
            target_artifacts: target_cwd.path().join(".pi").join("artifacts"),
            extension_namespaces: None,
        }))
        .expect("factory");
    let mut application_events = application.subscribe();

    // Three resident children: the asker, the replier, and an unrelated
    // bystander. The reply must reach ONLY the asker, never Main or the
    // bystander.
    let spawns = runtime_a
        .spawn_tasks(
            "Main",
            0,
            vec![
                task_item(0, "Asker", "blocked asker"),
                task_item(1, "Replier", "blocked replier"),
                task_item(2, "Bystander", "blocked bystander"),
            ],
        )
        .expect("spawn siblings")
        .into_iter()
        .collect::<Vec<_>>();
    assert_eq!(spawns.len(), 3, "all three siblings must spawn");
    // Wait for all three children to register and start running so every agent
    // is addressable before the exchange begins.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let notified = started.notified();
            if current.load(Ordering::SeqCst) >= 3 {
                break;
            }
            notified.await;
        }
    })
    .await
    .expect("all three siblings running");

    // A pending wait on the asker for a reply from the replier. The wait
    // registers on the resident runtime and survives the session switch.
    let pending_reply = tokio::spawn({
        let runtime = runtime_a.clone();
        async move {
            runtime
                .wait_message(
                    "Asker",
                    Some("Replier"),
                    Some(Duration::from_secs(30)),
                    None,
                )
                .await
        }
    });
    tokio::task::yield_now().await;
    let ask_receipt = runtime_a
        .send("Asker", "Replier", "sibling-ask", None)
        .remove(0);
    assert!(ask_receipt.error.is_none(), "ask receipt: {ask_receipt:?}");
    let ask_id = ask_receipt.message_id.expect("ask message id");

    // Cross-directory switch to an unrelated Main session (fresh runtime B).
    let target_file = write_resume_session(target_cwd.path(), "sibling-target", "target message");
    while application_events.try_recv().is_ok() {}
    let switch = tokio::time::timeout(
        Duration::from_secs(10),
        application.switch_session(&target_file),
    )
    .await
    .expect("switch session timeout")
    .expect("switch session");
    assert!(!switch.cancelled, "cross-cwd switch must complete");
    let runtime_b = application.orchestration_runtime().expect("target runtime");
    assert_ne!(
        runtime_b.group_id(),
        runtime_a.group_id(),
        "B must be a fresh group"
    );
    assert_eq!(
        runtime_a.active_child_count(),
        3,
        "all resident siblings survive the switch"
    );
    assert!(
        !runtime_a.is_shutdown(),
        "view switch must not shut the source runtime down"
    );

    // A target-local delivery establishes a positive barrier: B's forwarder is
    // live and routes its own group through the application, and no resident
    // A-group event may route through while the source is detached.
    let b_marker = runtime_b.send("Main", "Main", "b-marker", None).remove(0);
    assert!(b_marker.error.is_none(), "b-marker receipt: {b_marker:?}");
    wait_for_application_orchestration_group(
        &mut application_events,
        runtime_b.group_id(),
        Some(runtime_a.group_id()),
    )
    .await;

    // The sibling reply, sent while the source is detached, is claimed by the
    // pending wait — it reaches ONLY the asker, surviving the switch.
    let reply_receipt = runtime_a
        .send("Replier", "Asker", "sibling-reply", Some(ask_id.clone()))
        .remove(0);
    assert!(
        reply_receipt.error.is_none(),
        "reply receipt: {reply_receipt:?}"
    );
    let waited = pending_reply
        .await
        .expect("pending wait join")
        .expect("pending wait result")
        .expect("pending wait message");
    assert_eq!(waited.body, "sibling-reply");
    assert_eq!(waited.from, "Replier");
    assert_eq!(waited.to, "Asker");
    assert_eq!(waited.reply_to.as_deref(), Some(ask_id.as_str()));

    // Authority isolation: the reply and ask reached ONLY their exact
    // recipients. Main's mailbox and the bystander's mailbox never hold the
    // reply; the asker's mailbox holds neither (it consumed the reply via the
    // wait, and never received its own ask); the durable delivery log records
    // the ask exactly once for the replier even if the running child consumes it.
    let delivered_asks = runtime_a
        .delivered_messages()
        .into_iter()
        .filter(|message| {
            message.from == "Asker"
                && message.to == "Replier"
                && message.body == "sibling-ask"
                && message.reply_to.is_none()
        })
        .count();
    assert_eq!(
        delivered_asks, 1,
        "the ask must be durably delivered exactly once to the replier",
    );
    let main_inbox = runtime_a.inbox("Main", true).expect("source Main inbox");
    assert!(
        main_inbox
            .iter()
            .all(|message| message.body != "sibling-reply"),
        "sibling reply leaked into Main mailbox: {main_inbox:?}",
    );
    assert!(
        main_inbox
            .iter()
            .all(|message| message.body != "sibling-ask"),
        "sibling ask leaked into Main mailbox: {main_inbox:?}",
    );
    let bystander_inbox = runtime_a.inbox("Bystander", true).expect("bystander inbox");
    assert!(
        bystander_inbox
            .iter()
            .all(|message| message.body != "sibling-reply"),
        "sibling reply reached the unrelated bystander: {bystander_inbox:?}",
    );
    assert!(
        bystander_inbox
            .iter()
            .all(|message| message.body != "sibling-ask"),
        "sibling ask reached the unrelated bystander: {bystander_inbox:?}",
    );
    let asker_inbox = runtime_a.inbox("Asker", true).expect("asker inbox");
    assert!(
        asker_inbox
            .iter()
            .all(|message| message.body != "sibling-reply" && message.body != "sibling-ask"),
        "asker mailbox holds a sibling exchange it already consumed: {asker_inbox:?}",
    );

    // No leak into the unrelated target Main session. The target runtime is a
    // distinct group with its own Main; the resident exchange never reaches it.
    let target_main_inbox = runtime_b.inbox("Main", true).expect("target Main inbox");
    assert!(
        target_main_inbox
            .iter()
            .all(|message| message.body != "sibling-reply" && message.body != "sibling-ask"),
        "resident sibling exchange leaked into the target Main mailbox: {target_main_inbox:?}",
    );

    // A resident reply to Main (unsolicited, no waiter) must NOT be steered into
    // the active target session while the source is detached. The source
    // forwarder is detached on a cross-cwd switch, so the body stays queued in
    // the source Main mailbox — a bug that routes resident Main deliveries to
    // the currently-active session leaks the body into the target transcript.
    let main_canary = runtime_a
        .send("Replier", "Main", "main-canary", None)
        .remove(0);
    assert!(
        main_canary.error.is_none(),
        "canary receipt: {main_canary:?}"
    );
    // Deterministic state barrier (no fixed sleep): the canary is retained in
    // the source Main mailbox and absent from the target Main mailbox.
    let source_main_after = runtime_a
        .inbox("Main", true)
        .expect("source Main inbox after canary");
    assert!(
        source_main_after
            .iter()
            .any(|message| message.body == "main-canary"),
        "unsolicited Main reply must be retained in the source mailbox while detached",
    );
    let target_main_after = runtime_b
        .inbox("Main", true)
        .expect("target Main after canary");
    assert!(
        target_main_after
            .iter()
            .all(|message| message.body != "main-canary"),
        "resident Main reply leaked into the target Main mailbox: {target_main_after:?}",
    );
    // A second target-local delivery is the positive barrier that B's forwarder
    // has processed events through the canary's publish time; the resident
    // A-group event must never route through, and the canary must never be
    // steered into the target session.
    let b_flush = runtime_b.send("Main", "Main", "b-flush", None).remove(0);
    assert!(b_flush.error.is_none(), "b-flush receipt: {b_flush:?}");
    wait_for_application_orchestration_group(
        &mut application_events,
        runtime_b.group_id(),
        Some(runtime_a.group_id()),
    )
    .await;
    assert!(
        !queued_orchestration_message(&application, "main-canary").await,
        "resident Main reply leaked into the active target session steering queue",
    );
    while let Ok(event) = application_events.try_recv() {
        if let pi_coding::ApplicationEvent::Orchestration(event) = event {
            let group = match event {
                OrchestrationEvent::JobUpdated { group_id, .. }
                | OrchestrationEvent::AgentUpdated { group_id, .. }
                | OrchestrationEvent::MessageDelivered { group_id, .. } => group_id,
            };
            assert_ne!(
                group,
                runtime_a.group_id(),
                "resident source event routed through the target application while detached",
            );
        }
    }

    // Reconnect: switching back to the source by its exact recorder identity
    // reattaches the same resident group, the sibling reply is durably recorded
    // exactly once, and the retained Main canary is replayed into the source
    // session (the detached forwarder never steered it away).
    let switched_back = tokio::time::timeout(
        Duration::from_secs(10),
        application.switch_session(&source_file),
    )
    .await
    .expect("switch back timeout")
    .expect("switch back");
    assert!(!switched_back.cancelled);
    let rebound = application
        .orchestration_runtime()
        .expect("resident rebound");
    assert_eq!(rebound.group_id(), runtime_a.group_id());
    let recorded_replies = rebound
        .delivered_messages()
        .into_iter()
        .filter(|message| {
            message.from == "Replier"
                && message.to == "Asker"
                && message.body == "sibling-reply"
                && message.reply_to.as_deref() == Some(ask_id.as_str())
        })
        .count();
    assert_eq!(
        recorded_replies, 1,
        "sibling reply must be durably recorded exactly once across reconnect",
    );
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if queued_orchestration_message(&application, "main-canary").await {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("reconnect must replay the retained resident Main reply into the source session");

    // Existing pending-wait behavior must remain after reconnect: a fresh
    // correlated wait still resolves end to end on the reattached runtime.
    let fresh_reply_wait = tokio::spawn({
        let runtime = runtime_a.clone();
        async move {
            runtime
                .wait_message(
                    "Asker",
                    Some("Replier"),
                    Some(Duration::from_secs(30)),
                    None,
                )
                .await
        }
    });
    tokio::task::yield_now().await;
    let second_ask = runtime_a
        .send("Asker", "Replier", "second-ask", None)
        .remove(0);
    assert!(second_ask.error.is_none(), "second ask: {second_ask:?}");
    let second_ask_id = second_ask.message_id.expect("second ask id");
    let second_reply = runtime_a
        .send(
            "Replier",
            "Asker",
            "second-reply",
            Some(second_ask_id.clone()),
        )
        .remove(0);
    assert!(
        second_reply.error.is_none(),
        "second reply: {second_reply:?}"
    );
    let second_waited = fresh_reply_wait
        .await
        .expect("fresh wait join")
        .expect("fresh wait result")
        .expect("fresh wait message");
    assert_eq!(second_waited.body, "second-reply");
    assert_eq!(
        second_waited.reply_to.as_deref(),
        Some(second_ask_id.as_str())
    );

    // Existing host shutdown behavior must remain: releasing the children
    // settles every sibling, and cleanup shuts the resident runtime down so a
    // post-teardown wait fails with the shutdown message.
    release.cancel();
    for spawn in &spawns {
        let settled = wait_for_job_settled(&rebound, &spawn.job_id, Duration::from_secs(10)).await;
        assert!(
            settled.status.is_settled(),
            "released sibling must settle: {settled:?}"
        );
    }
    application.cleanup().await;
    assert!(
        runtime_a.is_shutdown(),
        "teardown shuts the resident runtime down"
    );
    assert_eq!(
        runtime_a.active_child_count(),
        0,
        "teardown must await every child exit"
    );
    let wait_error = runtime_a
        .wait_message("Main", None, None, None)
        .await
        .expect_err("post-teardown wait must fail");
    assert!(
        wait_error.to_string().contains("orchestration shut down"),
        "{wait_error:#}",
    );
}

/// REAL SMOKE: the `task` tool on a real `Application` with a faux provider
/// spawns a child, the parent messages it through `hub`, the child reads the
/// message via `hub inbox` and completes, and the transcript is durably
/// recorded under `children/<parent-id>/`. Verifies the whole subagent path.
#[tokio::test]
async fn real_application_task_tool_end_to_end_smoke() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let sessions = tempfile::tempdir().expect("sessions");

    // Real faux provider for the child: list the roster, ask Main for a
    // correlated reply, then emit the completion text. The child resolves the
    // provider through normal session machinery.
    let suffix = uuid::Uuid::now_v7().to_string();
    let api = format!("smoke-api-{suffix}");
    let provider = format!("smoke-provider-{suffix}");
    let child_model = Model {
        id: format!("smoke-model-{suffix}"),
        name: "Smoke Child".to_owned(),
        api: api.clone(),
        provider: provider.clone(),
        base_url: "http://localhost:0".to_owned(),
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
        hub_tool_call(
            "ask-1",
            json!({ "op": "ask", "to": "Main", "message": "child-question", "timeoutMs": 5000 }),
        ),
        yield_tool_call("yield-1", "smoke complete"),
    ]);
    let parent_model = child_model.clone();
    let (parent, parent_id, parent_path) =
        real_parent(artifacts.path(), sessions.path(), &parent_model);
    parent
        .attach_resources(hermetic_resources(artifacts.path()))
        .await
        .expect("parent resources");
    let child_root = parent_path
        .parent()
        .expect("parent dir")
        .join("children")
        .join(&parent_id);

    // The child factory notifies the test when the correlated ask begins. The
    // parent replies to the exact committed message id, proving Main remains
    // registered while the child waits.
    let wait_started = Arc::new(Notify::new());
    let factory_wait_started = wait_started.clone();
    let factory: ChildSessionFactory = Arc::new(move |request| {
        let wait_started = factory_wait_started.clone();
        let child_model = child_model.clone();
        Box::pin(async move {
            let before: BeforeToolCallFn = {
                let wait_started = wait_started.clone();
                Arc::new(move |context: BeforeToolCallContext| {
                    let wait_started = wait_started.clone();
                    Box::pin(async move {
                        if context.tool_call.name == "hub"
                            && context.arguments.get("op").and_then(|value| value.as_str())
                                == Some("ask")
                        {
                            wait_started.notify_one();
                        }
                        Ok(BeforeToolCallResult::default())
                    }) as BoxFuture<anyhow::Result<BeforeToolCallResult>>
                })
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
                auth_manager: None,
            })
        })
    });

    let mut config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![task_definition()]),
        artifacts.path(),
    );
    config.idle_ttl = None;
    config.parent_model = parent_model;
    let runtime = OrchestrationRuntime::new(config, factory).expect("runtime");
    let application = Application::new(parent).await;
    application
        .attach_orchestration(runtime.clone())
        .await
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

    // Reply to the child's correlated ask while the task remains running.
    wait_started.notified().await;
    let question = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(message) = runtime
                .inbox("Main", true)
                .expect("Main inbox")
                .into_iter()
                .find(|message| message.from == spawn.agent_id && message.body == "child-question")
            {
                break message;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("child ask delivered to Main");
    let receipts = runtime.send("Main", &spawn.agent_id, "parent-answer", Some(question.id));
    assert!(
        receipts.iter().all(|receipt| receipt.error.is_none()),
        "parent reply must not error: {receipts:?}"
    );
    // The child receives the message (hub wait returns it) and completes with the final text.
    let job = wait_for_job_settled(&runtime, &spawn.job_id, Duration::from_secs(15)).await;
    assert_eq!(job.status, JobStatus::Completed, "smoke job: {job:?}");
    let result = job.result.as_ref().expect("smoke result");
    assert_eq!(result.agent, "task");
    assert_eq!(result.output, "smoke complete");
    assert!(
        result.error.is_none(),
        "smoke result error: {:?}",
        result.error
    );

    // The child transcript is durably recorded under children/<parent-id>/. The
    // recorder names the file with a generated session id (not the agent id),
    // so locate it by scanning the child root for the one .jsonl transcript.
    let child_jsonl = std::fs::read_dir(&child_root)
        .expect("child root dir exists")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .find(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
        .expect("one durable child transcript jsonl under child root");
    let transcript = std::fs::read_to_string(&child_jsonl).expect("durable child transcript jsonl");
    assert!(
        transcript.contains("smoke complete"),
        "the final assistant turn must be recorded in the child transcript"
    );
    assert!(
        transcript.contains("parent-answer"),
        "the correlated Main reply must be recorded in the child transcript"
    );

    application.cleanup().await;
    registration.unregister();
}

fn real_parent(artifacts: &Path, session_dir: &Path, model: &Model) -> (Session, String, PathBuf) {
    let recorder = start_session_in(
        artifacts,
        Some(model),
        Some("off"),
        Some(session_dir),
        None,
        None,
    )
    .expect("parent recorder");
    recorder
        .persist_now()
        .expect("materialize real parent header");
    let parent_id = recorder.id();
    let parent_path = recorder.path();
    let parent = Session::new(SessionOptions {
        model: model.clone(),
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
        auth_manager: None,
    })
    .expect("parent session");
    parent.set_session_dir(session_dir.to_path_buf());
    parent.record(recorder).expect("attach parent recorder");
    (parent, parent_id, parent_path)
}
