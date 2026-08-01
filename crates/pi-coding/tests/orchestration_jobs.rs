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
    let collision = (task.execute)(context(
        "reject-job-id-agent-id-collision",
        json!({ "name": spawns[0].job_id, "task": "ambiguous id" }),
    ))
    .await
    .expect_err("retained job id cannot be reused as an agent id");
    assert!(collision.to_string().contains("conflicts with an existing job identifier"));

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
                },
                pi_coding::TaskItem {
                    index: 1,
                    id: "PermitWaiter".to_owned(),
                    agent: "task".to_owned(),
                    assignment: "wait for permit".to_owned(),
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
