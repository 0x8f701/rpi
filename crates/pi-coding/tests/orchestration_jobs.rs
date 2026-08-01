use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use pi_agent::{AbortController, AgentTool, ThinkingLevel, ToolCallContext};
use pi_ai::{Model, StopReason};
use pi_coding::{
    AgentCatalog, AgentDefinition, AgentDefinitionSource, ChildSessionFactory, JobSnapshot,
    JobStatus, OrchestrationConfig, OrchestrationRuntime, Session, SessionOptions, TaskSpawn,
};
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

fn definition() -> AgentDefinition {
    AgentDefinition {
        name: "task".to_owned(),
        description: "background task".to_owned(),
        system_prompt: "complete the assignment".to_owned(),
        tools: Some(Vec::new()),
        autoload_skills: Vec::new(),
        model: None,
        thinking_level: Some(ThinkingLevel::Off),
        source: AgentDefinitionSource::Bundled,
        path: None,
        trusted: true,
    }
}

struct ControlledRuntime {
    runtime: OrchestrationRuntime,
    current: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    started: Arc<Notify>,
    release: CancellationToken,
    aborted: Arc<AtomicUsize>,
}

fn controlled_runtime(artifact_dir: &std::path::Path, max_concurrency: usize) -> ControlledRuntime {
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
    let mut config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![definition()]),
        artifact_dir,
    );
    config.max_concurrency = max_concurrency;
    config.idle_ttl = None;
    config.parent_model = Model {
        id: "supervision-test".to_owned(),
        name: "Supervision Test".to_owned(),
        api: "supervision-test".to_owned(),
        provider: "supervision-test".to_owned(),
        ..Model::default()
    };
    ControlledRuntime {
        runtime: OrchestrationRuntime::new(config, factory).expect("runtime"),
        current,
        peak,
        started,
        release,
        aborted,
    }
}

fn controlled_runtime_with_release(
    artifact_dir: &std::path::Path,
    max_concurrency: usize,
    release: CancellationToken,
) -> ControlledRuntime {
    let controlled = controlled_runtime(artifact_dir, max_concurrency);
    // The standard helper owns its release token inside the stream factory, so
    // use cancellation fan-out to expose one shared release to the test.
    let child_release = controlled.release.clone();
    tokio::spawn(async move {
        release.cancelled().await;
        child_release.cancel();
    });
    controlled
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

async fn wait_for_count(
    count: &AtomicUsize,
    notify: &Notify,
    expected: usize,
    description: &str,
) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let notified = notify.notified();
            if count.load(Ordering::SeqCst) == expected {
                break;
            }
            notified.await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {description}"));
}

#[tokio::test]
async fn task_returns_before_completion_and_hub_retains_job_results() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let controlled = controlled_runtime(artifacts.path(), 1);
    let tools = controlled.runtime.agent_tools("Main", 0);
    let task = tool(&tools, "task");
    let hub = tool(&tools, "hub");

    let task_result = tokio::time::timeout(
        Duration::from_millis(500),
        (task.execute)(context(
            "spawn-background",
            json!({
                "tasks": [
                    { "name": "AsyncOne", "task": "first" },
                    { "name": "AsyncTwo", "task": "second" }
                ]
            }),
        )),
    )
    .await
    .expect("task returned while children were blocked")
    .expect("task spawn result");
    let spawns: Vec<TaskSpawn> =
        serde_json::from_value(task_result.details).expect("task spawn details");
    assert_eq!(spawns.len(), 2);
    assert!(spawns.iter().all(|spawn| spawn.status == JobStatus::Queued));
    assert!(spawns.iter().all(|spawn| !spawn.job_id.is_empty()));

    wait_for_count(
        &controlled.current,
        &controlled.started,
        1,
        "one running child",
    )
    .await;
    let jobs_result = (hub.execute)(context("jobs-running", json!({ "op": "jobs" })))
        .await
        .expect("hub jobs");
    let jobs: Vec<JobSnapshot> =
        serde_json::from_value(jobs_result.details["jobs"].clone()).expect("job snapshots");
    assert_eq!(jobs.iter().filter(|job| job.status == JobStatus::Running).count(), 1);
    assert_eq!(jobs.iter().filter(|job| job.status == JobStatus::Queued).count(), 1);
    assert_eq!(controlled.peak.load(Ordering::SeqCst), 1);

    let running = jobs
        .iter()
        .find(|job| job.status == JobStatus::Running)
        .expect("running job");
    assert!(
        controlled
            .runtime
            .send(&running.agent_id, "Main", "mail remains queued", None)[0]
            .error
            .is_none()
    );
    let timed_out = (hub.execute)(context(
        "wait-job-timeout",
        json!({ "op": "wait", "ids": [running.id], "timeoutMs": 5 }),
    ))
    .await
    .expect("timed job wait");
    let timed_jobs: Vec<JobSnapshot> =
        serde_json::from_value(timed_out.details["jobs"].clone()).expect("timed snapshots");
    assert_eq!(timed_jobs.len(), 1);
    assert_eq!(timed_jobs[0].status, JobStatus::Running);
    assert_eq!(controlled.runtime.inbox("Main", true).len(), 1);

    let mut wait = Box::pin((hub.execute)(context(
        "wait-job",
        json!({ "op": "wait", "ids": [running.id], "timeoutMs": 2000 }),
    )));
    assert!(
        tokio::time::timeout(Duration::from_millis(50), wait.as_mut())
            .await
            .is_err(),
        "hub wait returned before job completion"
    );

    controlled.release.cancel();
    let waited = tokio::time::timeout(Duration::from_secs(2), wait)
        .await
        .expect("hub wait unblocked on completion")
        .expect("hub wait result");
    let waited_jobs: Vec<JobSnapshot> =
        serde_json::from_value(waited.details["jobs"].clone()).expect("waited jobs");
    assert!(waited_jobs.iter().any(|job| job.status == JobStatus::Completed));

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if controlled.runtime.active_child_count() == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("all children settled");
    let retained_result = (hub.execute)(context("jobs-retained", json!({ "op": "jobs" })))
        .await
        .expect("retained hub jobs");
    let retained: Vec<JobSnapshot> = serde_json::from_value(retained_result.details["jobs"].clone())
        .expect("retained snapshots");
    assert_eq!(retained.len(), 2);
    assert!(retained.iter().all(|job| job.status == JobStatus::Completed));
    assert!(retained.iter().all(|job| {
        job.result
            .as_ref()
            .is_some_and(|result| result.output == "retained result")
    }));
    assert_eq!(controlled.runtime.inbox("Main", false).len(), 1);
    assert_eq!(controlled.peak.load(Ordering::SeqCst), 1);

    controlled.runtime.shutdown().await;
}

#[tokio::test]
async fn hub_cancel_terminates_child_and_retains_cancelled_result() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let controlled = controlled_runtime(artifacts.path(), 1);
    let tools = controlled.runtime.agent_tools("Main", 0);
    let task = tool(&tools, "task");
    let hub = tool(&tools, "hub");

    let task_result = tokio::time::timeout(
        Duration::from_millis(500),
        (task.execute)(context(
            "spawn-cancel",
            json!({ "name": "CancelAsync", "task": "block" }),
        )),
    )
    .await
    .expect("task returned before child completion")
    .expect("task spawn result");
    let spawn: TaskSpawn = serde_json::from_value::<Vec<TaskSpawn>>(task_result.details)
        .expect("spawn details")
        .remove(0);
    wait_for_count(
        &controlled.current,
        &controlled.started,
        1,
        "cancellable child",
    )
    .await;

    let cancel = (hub.execute)(context(
        "cancel-job",
        json!({ "op": "cancel", "ids": [spawn.job_id] }),
    ))
    .await
    .expect("hub cancel");
    assert_eq!(cancel.details["cancelled"], json!([spawn.job_id]));

    let waited = (hub.execute)(context(
        "wait-cancelled",
        json!({ "op": "wait", "ids": [spawn.job_id], "timeoutMs": 2000 }),
    ))
    .await
    .expect("wait for cancellation");
    let jobs: Vec<JobSnapshot> =
        serde_json::from_value(waited.details["jobs"].clone()).expect("cancelled jobs");
    assert_eq!(jobs[0].status, JobStatus::Cancelled);
    assert!(jobs[0].finished_at.is_some());
    assert!(jobs[0].result.as_ref().is_some_and(|result| {
        result.status == pi_coding::AgentStatus::Aborted
            && result.error.as_deref() == Some("task cancelled")
    }));
    assert_eq!(controlled.aborted.load(Ordering::SeqCst), 1);

    let retained = (hub.execute)(context(
        "jobs-cancelled",
        json!({ "op": "jobs", "ids": [spawn.job_id] }),
    ))
    .await
    .expect("retained cancelled job");
    assert_eq!(retained.details["jobs"][0]["status"], "cancelled");

    controlled.runtime.shutdown().await;
}

#[tokio::test]
async fn owner_drop_cancels_jobs_and_unblocks_synchronous_waiter() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let ControlledRuntime {
        runtime,
        current,
        started,
        release: _,
        aborted,
        peak: _,
    } = controlled_runtime(artifacts.path(), 1);
    let observer = runtime.clone();
    let runner = runtime.clone();
    let (_, abort) = AbortController::new();
    let run = tokio::spawn(async move {
        runner
            .run_tasks(
                "Main",
                0,
                vec![pi_coding::TaskItem {
                    index: 0,
                    id: "DropAsync".to_owned(),
                    agent: "task".to_owned(),
                    assignment: "remain blocked".to_owned(),
                    todo_task_id: None,
                }],
                abort,
            )
            .await
            .expect("synchronous compatibility waiter")
    });
    wait_for_count(&current, &started, 1, "child before owner drop").await;

    drop(runtime);

    let results = tokio::time::timeout(Duration::from_secs(2), run)
        .await
        .expect("run_tasks unblocked after owner drop")
        .expect("run_tasks join");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].status, pi_coding::AgentStatus::Aborted);
    assert_eq!(aborted.load(Ordering::SeqCst), 1);

    let jobs = observer.jobs(None);
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].status, JobStatus::Cancelled);
    assert!(jobs[0].finished_at.is_some());
    assert!(jobs[0].result.is_some());
}

#[tokio::test]
async fn cancellation_settles_when_child_stream_ignores_abort() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let started_count = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let release = CancellationToken::new();
    let factory_started_count = started_count.clone();
    let factory_started = started.clone();
    let factory_release = release.clone();
    let factory: ChildSessionFactory = Arc::new(move |request| {
        let started_count = factory_started_count.clone();
        let started = factory_started.clone();
        let release = factory_release.clone();
        Box::pin(async move {
            let stream_fn: pi_agent::StreamFn = Arc::new(move |model, _context, _options| {
                let started_count = started_count.clone();
                let started = started.clone();
                let release = release.clone();
                Box::pin(async move {
                    let stream = pi_ai::new_assistant_message_event_stream();
                    let producer = stream.clone();
                    tokio::spawn(async move {
                        started_count.fetch_add(1, Ordering::SeqCst);
                        started.notify_waiters();
                        release.cancelled().await;
                        let mut message = pi_ai::AssistantMessage::pending(&model);
                        message.stop_reason = StopReason::Aborted;
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
    let mut config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![definition()]),
        artifacts.path(),
    );
    config.idle_ttl = None;
    let runtime = OrchestrationRuntime::new(config, factory).expect("runtime");
    let spawn = runtime
        .spawn_tasks(
            "Main",
            0,
            vec![pi_coding::TaskItem {
                index: 0,
                id: "IgnoreAbort".to_owned(),
                agent: "task".to_owned(),
                assignment: "ignore cancellation".to_owned(),
                todo_task_id: None,
            }],
        )
        .expect("spawn")
        .remove(0);
    wait_for_count(&started_count, &started, 1, "abort-ignoring child").await;

    assert_eq!(runtime.cancel_jobs(std::slice::from_ref(&spawn.job_id)), vec![spawn.job_id.clone()]);
    let jobs = tokio::time::timeout(
        Duration::from_secs(4),
        runtime.wait_jobs(std::slice::from_ref(&spawn.job_id), None, None),
    )
    .await
    .expect("abort-ignoring cancellation settled")
    .expect("job wait");
    assert_eq!(jobs[0].status, JobStatus::Cancelled);
    assert!(jobs[0].result.as_ref().is_some_and(|result| {
        result.status == pi_coding::AgentStatus::Aborted
            && result.error.as_deref().is_some_and(|error| {
                error.contains("cancellation timed out after 2s")
            })
    }));
    assert_eq!(runtime.active_child_count(), 0);

    release.cancel();
    tokio::time::timeout(Duration::from_secs(1), runtime.shutdown())
        .await
        .expect("shutdown no longer waits on ignored abort");
}

#[tokio::test]
async fn failed_artifact_write_does_not_publish_artifact_alias() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let controlled = controlled_runtime(artifacts.path(), 1);
    let spawn = controlled
        .runtime
        .spawn_tasks(
            "Main",
            0,
            vec![pi_coding::TaskItem {
                index: 0,
                id: "ArtifactFailure".to_owned(),
                agent: "task".to_owned(),
                assignment: "persist result".to_owned(),
                todo_task_id: None,
            }],
        )
        .expect("spawn")
        .remove(0);
    wait_for_count(
        &controlled.current,
        &controlled.started,
        1,
        "artifact failure child",
    )
    .await;
    let blocked_artifact = artifacts
        .path()
        .join(format!("ArtifactFailure-{}.md", spawn.job_id));
    std::fs::create_dir(&blocked_artifact).expect("block artifact file creation");

    controlled.release.cancel();
    let jobs = controlled
        .runtime
        .wait_jobs(
            std::slice::from_ref(&spawn.job_id),
            Some(Duration::from_secs(2)),
            None,
        )
        .await
        .expect("job wait");
    assert_eq!(jobs[0].status, JobStatus::Failed);
    assert!(jobs[0].result.as_ref().is_some_and(|result| {
        result.error.as_deref().is_some_and(|error| error.contains("writing subagent artifact"))
    }));
    let snapshot = controlled
        .runtime
        .list("Main")
        .into_iter()
        .find(|agent| agent.id == "ArtifactFailure")
        .expect("agent snapshot");
    assert_eq!(snapshot.artifact_ref, None);
    assert_eq!(snapshot.history_ref.as_deref(), Some("history://ArtifactFailure"));
    assert!(controlled.runtime.resolve_agent_reference("ArtifactFailure").is_err());
    assert!(controlled
        .runtime
        .resolve_history_reference("ArtifactFailure")
        .expect("history alias")
        .is_file());

    controlled.runtime.shutdown().await;
}

#[tokio::test]
async fn workflow_job_projection_tracks_running_and_terminal_status() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let controlled = controlled_runtime(artifacts.path(), 1);
    controlled
        .runtime
        .set_workflow_scope(pi_coding::WorkflowRuntimeScope {
            workflow_id: "alpha".to_owned(),
            generation: 4,
        })
        .expect("scope");
    let spawn = controlled
        .runtime
        .spawn_tasks(
            "Main",
            0,
            vec![pi_coding::TaskItem {
                index: 0,
                id: "ScopedWorker".to_owned(),
                agent: "task".to_owned(),
                assignment: "scoped work".to_owned(),
                todo_task_id: Some("todo-scoped".to_owned()),
            }],
        )
        .expect("spawn")
        .remove(0);
    wait_for_count(&controlled.current, &controlled.started, 1, "scoped worker running").await;
    let running = controlled.runtime.workflow_jobs("alpha", 4);
    assert_eq!(running.len(), 1);
    assert_eq!(running[0].job.id, spawn.job_id);
    assert_eq!(running[0].job.status, JobStatus::Running);

    controlled.release.cancel();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let terminal = controlled.runtime.workflow_jobs("alpha", 4);
            if terminal[0].job.status.is_settled() {
                assert_eq!(terminal[0].job.status, JobStatus::Completed);
                assert!(terminal[0].job.finished_at.is_some());
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("scoped projection reached terminal");
    controlled.runtime.shutdown().await;
}

#[tokio::test]
async fn shared_workflow_gate_bounds_two_independent_runtimes() {
    let alpha_artifacts = tempfile::tempdir().expect("alpha artifacts");
    let beta_artifacts = tempfile::tempdir().expect("beta artifacts");
    let release = CancellationToken::new();
    let alpha = controlled_runtime_with_release(alpha_artifacts.path(), 2, release.clone());
    let beta = controlled_runtime_with_release(beta_artifacts.path(), 2, release.clone());
    let global = pi_coding::OrchestrationConcurrencyGate::new(1).expect("global gate");
    alpha
        .runtime
        .set_global_concurrency_gate(global.clone())
        .expect("alpha gate");
    beta.runtime.set_global_concurrency_gate(global).expect("beta gate");
    alpha
        .runtime
        .set_workflow_scope(pi_coding::WorkflowRuntimeScope {
            workflow_id: "alpha".to_owned(),
            generation: 1,
        })
        .expect("alpha scope");
    beta.runtime
        .set_workflow_scope(pi_coding::WorkflowRuntimeScope {
            workflow_id: "beta".to_owned(),
            generation: 1,
        })
        .expect("beta scope");

    let alpha_spawn = alpha
        .runtime
        .spawn_tasks(
            "Main",
            0,
            vec![pi_coding::TaskItem {
                index: 0,
                id: "AlphaWorker".to_owned(),
                agent: "task".to_owned(),
                assignment: "alpha root".to_owned(),
                todo_task_id: Some("alpha-root".to_owned()),
            }],
        )
        .expect("alpha spawn");
    let beta_spawn = beta
        .runtime
        .spawn_tasks(
            "Main",
            0,
            vec![pi_coding::TaskItem {
                index: 0,
                id: "BetaWorker".to_owned(),
                agent: "task".to_owned(),
                assignment: "beta root".to_owned(),
                todo_task_id: Some("beta-root".to_owned()),
            }],
        )
        .expect("beta spawn");

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let running = alpha.current.load(Ordering::SeqCst) + beta.current.load(Ordering::SeqCst);
            let queued = alpha.runtime.jobs(None).iter().chain(beta.runtime.jobs(None).iter()).any(
                |job| job.status == JobStatus::Queued,
            );
            if running == 1 && queued {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("one globally running worker and one queued worker");
    assert_eq!(alpha.current.load(Ordering::SeqCst) + beta.current.load(Ordering::SeqCst), 1);
    assert_eq!(
        alpha.runtime.workflow_jobs("alpha", 1)[0].todo_task_id.as_deref(),
        Some("alpha-root")
    );
    assert_eq!(
        beta.runtime.workflow_jobs("beta", 1)[0].todo_task_id.as_deref(),
        Some("beta-root")
    );

    release.cancel();
    alpha
        .runtime
        .wait_jobs(&[alpha_spawn[0].job_id.clone()], Some(Duration::from_secs(2)), None)
        .await
        .expect("alpha settled");
    beta.runtime
        .wait_jobs(&[beta_spawn[0].job_id.clone()], Some(Duration::from_secs(2)), None)
        .await
        .expect("beta settled");
    alpha.runtime.shutdown().await;
    beta.runtime.shutdown().await;
}

#[tokio::test]
async fn roster_keeps_waiting_child_queued_until_permit_acquired() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let controlled = controlled_runtime(artifacts.path(), 1);
    let spawns = controlled
        .runtime
        .spawn_tasks(
            "Main",
            0,
            vec![
                pi_coding::TaskItem {
                    index: 0,
                    id: "PermitHolder".to_owned(),
                    agent: "task".to_owned(),
                    assignment: "hold permit".to_owned(),
                    todo_task_id: None,
                },
                pi_coding::TaskItem {
                    index: 1,
                    id: "PermitWaiter".to_owned(),
                    agent: "task".to_owned(),
                    assignment: "wait for permit".to_owned(),
                    todo_task_id: None,
                },
            ],
        )
        .expect("spawn batch");
    wait_for_count(
        &controlled.current,
        &controlled.started,
        1,
        "single permit holder",
    )
    .await;

    let roster = controlled.runtime.list("Main");
    assert_eq!(
        roster
            .iter()
            .find(|agent| agent.id == "PermitHolder")
            .expect("holder")
            .status,
        pi_coding::AgentStatus::Running,
    );
    assert_eq!(
        roster
            .iter()
            .find(|agent| agent.id == "PermitWaiter")
            .expect("waiter")
            .status,
        pi_coding::AgentStatus::Queued,
    );
    let jobs = controlled.runtime.jobs(Some(
        &spawns
            .iter()
            .map(|spawn| spawn.job_id.clone())
            .collect::<Vec<_>>(),
    ));
    assert_eq!(jobs.iter().filter(|job| job.status == JobStatus::Running).count(), 1);
    assert_eq!(jobs.iter().filter(|job| job.status == JobStatus::Queued).count(), 1);

    controlled.release.cancel();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if controlled.runtime.active_child_count() == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("children settled");
    controlled.runtime.shutdown().await;
}

#[tokio::test]
async fn sequential_duplicate_names_allocate_suffixes() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let controlled = controlled_runtime(artifacts.path(), 2);
    let tools = controlled.runtime.agent_tools("Main", 0);
    let task = tool(&tools, "task");

    let first = (task.execute)(context(
        "hard-task-1",
        json!({ "name": "hard_task", "task": "first" }),
    ))
    .await
    .expect("first hard_task spawn");
    let first_spawns: Vec<TaskSpawn> =
        serde_json::from_value(first.details).expect("first spawn details");
    assert_eq!(first_spawns.len(), 1);
    assert_eq!(first_spawns[0].agent_id, "hard_task");

    let second = (task.execute)(context(
        "hard-task-2",
        json!({ "name": "hard_task", "task": "second" }),
    ))
    .await
    .expect("second hard_task spawn without retry");
    let second_spawns: Vec<TaskSpawn> =
        serde_json::from_value(second.details).expect("second spawn details");
    assert_eq!(second_spawns.len(), 1);
    assert_eq!(second_spawns[0].agent_id, "hard_task_2");

    let third = (task.execute)(context(
        "hard-task-3",
        json!({ "name": "hard_task", "task": "third" }),
    ))
    .await
    .expect("third hard_task spawn");
    let third_spawns: Vec<TaskSpawn> =
        serde_json::from_value(third.details).expect("third spawn details");
    assert_eq!(third_spawns[0].agent_id, "hard_task_3");

    controlled.release.cancel();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if controlled.runtime.active_child_count() == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("children settled");
    controlled.runtime.shutdown().await;
}

#[tokio::test]
async fn batch_duplicate_names_allocate_deterministic_sequence() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let controlled = controlled_runtime(artifacts.path(), 4);
    let tools = controlled.runtime.agent_tools("Main", 0);
    let task = tool(&tools, "task");

    let result = (task.execute)(context(
        "batch-dupes",
        json!({
            "tasks": [
                { "name": "worker", "task": "one" },
                { "name": "worker", "task": "two" },
                { "name": "helper", "task": "three" },
                { "name": "worker", "task": "four" },
                { "name": "helper", "task": "five" }
            ]
        }),
    ))
    .await
    .expect("batch spawn");
    let spawns: Vec<TaskSpawn> = serde_json::from_value(result.details).expect("spawn details");
    assert_eq!(
        spawns
            .iter()
            .map(|spawn| spawn.agent_id.as_str())
            .collect::<Vec<_>>(),
        vec!["worker", "worker_2", "helper", "worker_3", "helper_2"]
    );

    controlled.release.cancel();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if controlled.runtime.active_child_count() == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("children settled");
    controlled.runtime.shutdown().await;
}

#[tokio::test]
async fn concurrent_duplicate_names_do_not_collide() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let controlled = controlled_runtime(artifacts.path(), 8);
    let runtime = controlled.runtime.clone();

    let mut handles = Vec::new();
    for index in 0..12 {
        let runtime = runtime.clone();
        handles.push(tokio::spawn(async move {
            let tools = runtime.agent_tools("Main", 0);
            let task = tools
                .iter()
                .find(|tool| tool.name == "task")
                .expect("task tool");
            let result = (task.execute)(context(
                &format!("concurrent-{index}"),
                json!({ "name": "shared", "task": format!("work {index}") }),
            ))
            .await
            .expect("spawn");
            let spawns: Vec<TaskSpawn> =
                serde_json::from_value(result.details).expect("spawn details");
            spawns.into_iter().next().expect("one spawn").agent_id
        }));
    }

    let mut ids = Vec::new();
    for handle in handles {
        ids.push(handle.await.expect("join spawn"));
    }
    ids.sort();
    let mut unique = ids.clone();
    unique.dedup();
    assert_eq!(ids, unique, "concurrent allocations must be unique: {ids:?}");
    assert!(ids.contains(&"shared".to_owned()), "base name must be used once");
    assert!(
        ids.iter().any(|id| id.starts_with("shared_")),
        "collisions must receive _N suffixes: {ids:?}"
    );

    controlled.release.cancel();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if controlled.runtime.active_child_count() == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("children settled");
    controlled.runtime.shutdown().await;
}

#[tokio::test]
async fn colliding_with_main_and_retained_agent_ids_suffixes() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let controlled = controlled_runtime(artifacts.path(), 2);
    let tools = controlled.runtime.agent_tools("Main", 0);
    let task = tool(&tools, "task");

    let against_main = (task.execute)(context(
        "main-collision",
        json!({ "name": "Main", "task": "against main" }),
    ))
    .await
    .expect("Main name should suffix");
    let against_main: Vec<TaskSpawn> =
        serde_json::from_value(against_main.details).expect("spawn details");
    assert_eq!(against_main[0].agent_id, "Main_2");

    let first = (task.execute)(context(
        "retain-base",
        json!({ "name": "keeper", "task": "keep me" }),
    ))
    .await
    .expect("keeper spawn");
    let first: Vec<TaskSpawn> = serde_json::from_value(first.details).expect("spawn details");
    assert_eq!(first[0].agent_id, "keeper");

    let job_id_name = (task.execute)(context(
        "job-id-as-name",
        json!({ "name": first[0].job_id, "task": "job id name" }),
    ))
    .await
    .expect("job id reused as name should auto-suffix");
    let job_id_spawns: Vec<TaskSpawn> =
        serde_json::from_value(job_id_name.details).expect("job-id name spawn details");
    assert_eq!(
        job_id_spawns[0].agent_id,
        format!("{}_2", first[0].job_id),
        "colliding job identifier must allocate a deterministic suffix"
    );

    controlled.release.cancel();
    wait_for_count(&controlled.current, &controlled.started, 0, "no running").await;
    // Allow settle to Idle/retained while id remains occupied.
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if controlled.runtime.active_child_count() == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("settled");

    let second = (task.execute)(context(
        "retain-collision",
        json!({ "name": "keeper", "task": "again" }),
    ))
    .await
    .expect("retained agent id should suffix");
    let second: Vec<TaskSpawn> = serde_json::from_value(second.details).expect("spawn details");
    assert_eq!(second[0].agent_id, "keeper_2");

    controlled.release.cancel();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if controlled.runtime.active_child_count() == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("children settled");
    controlled.runtime.shutdown().await;
}

#[tokio::test]
async fn hub_send_by_job_uuid_reaches_canonical_mailbox_once() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let controlled = controlled_runtime(artifacts.path(), 1);
    let tools = controlled.runtime.agent_tools("Main", 0);
    let task = tool(&tools, "task");
    let hub = tool(&tools, "hub");

    let spawn_result = (task.execute)(context(
        "spawn-for-job-uuid-send",
        json!({ "name": "MailTarget", "task": "await mail" }),
    ))
    .await
    .expect("spawn");
    let spawns: Vec<TaskSpawn> =
        serde_json::from_value(spawn_result.details).expect("spawn details");
    assert_eq!(spawns.len(), 1);
    let job_id = spawns[0].job_id.clone();
    let agent_id = spawns[0].agent_id.clone();
    assert!(!job_id.is_empty());
    assert_eq!(agent_id, "MailTarget");
    assert_ne!(job_id, agent_id);

    wait_for_count(
        &controlled.current,
        &controlled.started,
        1,
        "child running for job uuid send",
    )
    .await;

    // Immediate hub send addressed by the spawn job UUID.
    let send = (hub.execute)(context(
        "send-by-job-uuid",
        json!({
            "op": "send",
            "to": job_id,
            "message": "hello via job uuid"
        }),
    ))
    .await
    .expect("hub send by job uuid");
    let receipts = send.details["receipts"]
        .as_array()
        .expect("receipts array");
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0]["to"], agent_id);
    assert_eq!(receipts[0]["requested"], job_id);
    assert_eq!(receipts[0]["outcome"], "woken");
    assert!(receipts[0].get("error").is_none() || receipts[0]["error"].is_null());

    // Canonical running agent receives exactly once through its active Session;
    // job UUID is not a separate mailbox key and no durable duplicate remains.
    assert!(
        controlled.runtime.inbox(&agent_id, true).is_empty(),
        "active delivery must not leave a canonical mailbox duplicate"
    );
    assert!(
        controlled.runtime.inbox(&job_id, true).is_empty(),
        "job uuid must not be a separate mailbox"
    );

    // Settled retained job UUID still resolves to the retained agent.
    controlled.release.cancel();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if controlled.runtime.active_child_count() == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("child settled");

    let retained_jobs = controlled
        .runtime
        .jobs(Some(std::slice::from_ref(&job_id)));
    assert_eq!(retained_jobs.len(), 1);
    assert!(retained_jobs[0].status.is_settled());

    let retained_send = (hub.execute)(context(
        "send-retained-job-uuid",
        json!({
            "op": "send",
            "to": job_id,
            "message": "after settle"
        }),
    ))
    .await
    .expect("send to retained job uuid");
    let retained_receipts = retained_send.details["receipts"]
        .as_array()
        .expect("retained receipts");
    assert_eq!(retained_receipts.len(), 1);
    assert_eq!(retained_receipts[0]["to"], agent_id);
    assert_eq!(retained_receipts[0]["requested"], job_id);
    assert!(
        retained_receipts[0]["error"].is_null()
            || retained_receipts[0].get("error").is_none(),
        "retained job uuid must still resolve: {}",
        retained_receipts[0]
    );
    assert_eq!(
        controlled.runtime.inbox(&agent_id, true).len(),
        1,
        "only the post-settlement delivery remains in the mailbox"
    );

    // Unknown UUID fails with an actionable receipt.
    let unknown = (hub.execute)(context(
        "send-unknown-job-uuid",
        json!({
            "op": "send",
            "to": "00000000-0000-7000-8000-000000000000",
            "message": "no such job"
        }),
    ))
    .await
    .expect("unknown send returns tool result with failed receipt");
    let unknown_receipts = unknown.details["receipts"]
        .as_array()
        .expect("unknown receipts");
    assert_eq!(unknown_receipts.len(), 1);
    assert_eq!(unknown_receipts[0]["outcome"], "failed");
    let error = unknown_receipts[0]["error"]
        .as_str()
        .expect("failed receipt error");
    assert!(
        error.contains("unknown orchestration agent"),
        "unknown uuid error should be actionable: {error}"
    );

    controlled.runtime.shutdown().await;
}

#[tokio::test]
async fn hub_await_reply_from_tracks_canonical_agent_after_job_uuid_send() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let controlled = controlled_runtime(artifacts.path(), 1);
    let tools = controlled.runtime.agent_tools("Main", 0);
    let task = tool(&tools, "task");
    let hub = tool(&tools, "hub");

    let spawn_result = (task.execute)(context(
        "spawn-for-await-from",
        json!({ "name": "Replier", "task": "reply" }),
    ))
    .await
    .expect("spawn");
    let spawns: Vec<TaskSpawn> =
        serde_json::from_value(spawn_result.details).expect("spawn details");
    let job_id = spawns[0].job_id.clone();
    let agent_id = spawns[0].agent_id.clone();

    wait_for_count(
        &controlled.current,
        &controlled.started,
        1,
        "replier running",
    )
    .await;

    // Child replies to Main; await-from must match canonical agent id even when
    // the original send targeted the job UUID.
    let reply_body = "pong-from-child";
    let runtime = controlled.runtime.clone();
    let agent_for_reply = agent_id.clone();
    let reply_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        runtime.send(&agent_for_reply, "Main", reply_body, None)
    });

    let awaited = (hub.execute)(context(
        "send-await-by-job-uuid",
        json!({
            "op": "send",
            "to": job_id,
            "message": "ping",
            "await": true,
            "timeoutMs": 2000
        }),
    ))
    .await
    .expect("await reply after job uuid send");

    let reply_receipts = reply_task.await.expect("reply join");
    assert!(reply_receipts[0].error.is_none());

    let text = awaited
        .content
        .iter()
        .filter_map(|block| match block {
            pi_ai::ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert!(
        text.contains(reply_body),
        "await must observe child reply via canonical from filter: {text}"
    );
    assert!(
        !text.contains("No reply from"),
        "await must not time out when child replied as agent id: {text}"
    );

    controlled.release.cancel();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if controlled.runtime.active_child_count() == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("settled");
    controlled.runtime.shutdown().await;
}
