//! End-to-end Main supervision of child jobs via hub/task tools and IRC.
//!
//! Contracts defended:
//! - task spawn returns job/agent IDs immediately (queued)
//! - jobs snapshots transition queued → running → terminal
//! - hub wait wakes on first watched completion without consuming unrelated mailbox mail
//! - cancel settles one job while the sibling is retained
//! - send/inbox/list honor group isolation and mailbox bounds
//! - parked delivery receipts stay Queued (no false revival without session resume)
//! - owner shutdown cancels remaining children

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use pi_agent::{AbortController, AgentTool, ThinkingLevel, ToolCallContext};
use pi_ai::{Model, StopReason};
use pi_coding::{
    AgentCatalog, AgentDefinition, AgentDefinitionSource, AgentStatus, ChildSessionFactory,
    DeliveryOutcome, JobSnapshot, JobStatus, OrchestrationConfig, OrchestrationRuntime, Session,
    SessionOptions, TaskSpawn,
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

fn controlled_runtime(
    artifact_dir: &std::path::Path,
    max_concurrency: usize,
    mailbox_capacity: usize,
    idle_ttl: Option<Duration>,
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
                                .push(pi_ai::ContentBlock::text("supervised result"));
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
    config.mailbox_capacity = mailbox_capacity;
    config.idle_ttl = idle_ttl;
    config.parent_model = Model {
        id: "supervision-e2e".to_owned(),
        name: "Supervision E2E".to_owned(),
        api: "supervision-e2e".to_owned(),
        provider: "supervision-e2e".to_owned(),
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

async fn wait_for_job_status(
    runtime: &OrchestrationRuntime,
    job_id: &str,
    status: JobStatus,
    description: &str,
) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if runtime
                .jobs(Some(&[job_id.to_owned()]))
                .into_iter()
                .any(|job| job.status == status)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {description}"));
}

/// Full Main supervision scenario: two concurrent children, IRC between them,
/// wait/cancel isolation, retained sibling result, truthful park receipts,
/// mailbox bounds, group isolation, and owner shutdown cleanup.
#[tokio::test]
async fn main_supervises_two_children_with_irc_cancel_and_park() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let controlled = controlled_runtime(artifacts.path(), 2, 2, Some(Duration::from_millis(40)));
    let tools = controlled.runtime.agent_tools("Main", 0);
    let task = tool(&tools, "task");
    let hub = tool(&tools, "hub");

    // 1) Spawn returns IDs immediately while children are still blocked.
    let task_result = tokio::time::timeout(
        Duration::from_millis(500),
        (task.execute)(context(
            "spawn-pair",
            json!({
                "tasks": [
                    { "name": "Alpha", "task": "first supervised child" },
                    { "name": "Beta", "task": "second supervised child" }
                ]
            }),
        )),
    )
    .await
    .expect("task returned before children finished")
    .expect("task spawn result");
    let mut spawns: Vec<TaskSpawn> =
        serde_json::from_value(task_result.details).expect("task spawn details");
    spawns.sort_by_key(|spawn| spawn.index);
    assert_eq!(spawns.len(), 2);
    assert!(spawns.iter().all(|spawn| spawn.status == JobStatus::Queued));
    assert!(spawns.iter().all(|spawn| !spawn.job_id.is_empty()));
    assert_eq!(spawns[0].agent_id, "Alpha");
    assert_eq!(spawns[1].agent_id, "Beta");
    let alpha_job = spawns[0].job_id.clone();
    let beta_job = spawns[1].job_id.clone();

    // 2) Both children become running under concurrency=2.
    wait_for_count(
        &controlled.current,
        &controlled.started,
        2,
        "two concurrent children",
    )
    .await;
    wait_for_job_status(
        &controlled.runtime,
        &alpha_job,
        JobStatus::Running,
        "alpha running",
    )
    .await;
    wait_for_job_status(
        &controlled.runtime,
        &beta_job,
        JobStatus::Running,
        "beta running",
    )
    .await;
    let jobs_result = (hub.execute)(context("jobs-running", json!({ "op": "jobs" })))
        .await
        .expect("hub jobs");
    let running_jobs: Vec<JobSnapshot> =
        serde_json::from_value(jobs_result.details["jobs"].clone()).expect("running snapshots");
    assert_eq!(running_jobs.len(), 2);
    assert!(running_jobs.iter().all(|job| job.status == JobStatus::Running));
    assert_eq!(controlled.peak.load(Ordering::SeqCst), 2);

    // 3) Inter-agent IRC while both are running: delivery is Queued (not woken).
    let alpha_to_beta = controlled.runtime.send("Alpha", "Beta", "alpha-to-beta", None);
    assert_eq!(alpha_to_beta.len(), 1);
    assert_eq!(alpha_to_beta[0].outcome, DeliveryOutcome::Queued);
    assert!(alpha_to_beta[0].error.is_none());
    let beta_to_main = controlled
        .runtime
        .send("Beta", "Main", "beta-status-update", None);
    assert_eq!(beta_to_main[0].outcome, DeliveryOutcome::Woken);
    assert!(beta_to_main[0].error.is_none());

    // Mail to Main must remain queued and must not be drained by job wait.
    assert_eq!(controlled.runtime.inbox("Main", true).len(), 1);
    assert_eq!(controlled.runtime.inbox("Beta", true).len(), 1);
    assert_eq!(controlled.runtime.inbox("Beta", true)[0].body, "alpha-to-beta");

    // 4) hub wait on Alpha must not return while Alpha is still running, and
    //    must not consume Main's unrelated mailbox message.
    let mut wait_alpha = Box::pin((hub.execute)(context(
        "wait-alpha",
        json!({ "op": "wait", "ids": [alpha_job], "timeoutMs": 2000 }),
    )));
    assert!(
        tokio::time::timeout(Duration::from_millis(40), wait_alpha.as_mut())
            .await
            .is_err(),
        "hub wait returned before Alpha settled"
    );
    assert_eq!(
        controlled.runtime.inbox("Main", true).len(),
        1,
        "job wait must not drain mailbox"
    );

    // 5) Cancel Beta; Alpha continues. Wait for Beta settlement, retain Alpha.
    let cancel = (hub.execute)(context(
        "cancel-beta",
        json!({ "op": "cancel", "ids": [beta_job] }),
    ))
    .await
    .expect("hub cancel beta");
    assert_eq!(cancel.details["cancelled"], json!([beta_job]));

    let waited_beta = tokio::time::timeout(
        Duration::from_secs(2),
        (hub.execute)(context(
            "wait-beta-cancelled",
            json!({ "op": "wait", "ids": [beta_job], "timeoutMs": 2000 }),
        )),
    )
    .await
    .expect("wait beta timeout")
    .expect("wait beta result");
    let beta_jobs: Vec<JobSnapshot> =
        serde_json::from_value(waited_beta.details["jobs"].clone()).expect("beta jobs");
    assert_eq!(beta_jobs.len(), 1);
    assert_eq!(beta_jobs[0].status, JobStatus::Cancelled);
    assert!(beta_jobs[0].finished_at.is_some());
    assert!(beta_jobs[0].result.as_ref().is_some_and(|result| {
        result.status == AgentStatus::Aborted && result.error.as_deref() == Some("task cancelled")
    }));
    assert_eq!(controlled.aborted.load(Ordering::SeqCst), 1);

    // Alpha still running after Beta cancel.
    let alpha_still = controlled.runtime.jobs(Some(&[alpha_job.clone()]));
    assert_eq!(alpha_still[0].status, JobStatus::Running);

    // Main mailbox still holds the Beta status update (wait/cancel did not eat it).
    let main_peek = controlled.runtime.inbox("Main", true);
    assert_eq!(main_peek.len(), 1);
    assert_eq!(main_peek[0].from, "Beta");
    assert_eq!(main_peek[0].body, "beta-status-update");

    // 6) Release Alpha and wait for completion via hub.
    controlled.release.cancel();
    let waited_alpha = tokio::time::timeout(Duration::from_secs(2), wait_alpha)
        .await
        .expect("hub wait alpha unblocked")
        .expect("hub wait alpha result");
    let alpha_jobs: Vec<JobSnapshot> =
        serde_json::from_value(waited_alpha.details["jobs"].clone()).expect("alpha jobs");
    assert!(
        alpha_jobs
            .iter()
            .any(|job| job.id == alpha_job && job.status == JobStatus::Completed)
    );
    assert!(alpha_jobs.iter().any(|job| {
        job.id == alpha_job
            && job
                .result
                .as_ref()
                .is_some_and(|result| result.output == "supervised result")
    }));

    // Retained job table: Alpha completed, Beta cancelled.
    let retained = (hub.execute)(context("jobs-final", json!({ "op": "jobs" })))
        .await
        .expect("final jobs");
    let retained_jobs: Vec<JobSnapshot> =
        serde_json::from_value(retained.details["jobs"].clone()).expect("retained");
    assert_eq!(retained_jobs.len(), 2);
    let alpha = retained_jobs
        .iter()
        .find(|job| job.id == alpha_job)
        .expect("alpha retained");
    let beta = retained_jobs
        .iter()
        .find(|job| job.id == beta_job)
        .expect("beta retained");
    assert_eq!(alpha.status, JobStatus::Completed);
    assert_eq!(
        alpha.result.as_ref().map(|result| result.output.as_str()),
        Some("supervised result")
    );
    assert_eq!(beta.status, JobStatus::Cancelled);

    // Drain Main inbox explicitly; prove prior waits left it intact until now.
    let inbox = (hub.execute)(context("main-inbox", json!({ "op": "inbox", "peek": false })))
        .await
        .expect("main inbox");
    let messages: Vec<Value> =
        serde_json::from_value(inbox.details["messages"].clone()).expect("messages");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["from"], "Beta");
    assert_eq!(messages[0]["body"], "beta-status-update");
    assert!(controlled.runtime.inbox("Main", true).is_empty());

    // 7) Roster via hub list — Alpha idle with unread IRC from earlier, Beta aborted.
    let list = (hub.execute)(context("main-list", json!({ "op": "list" })))
        .await
        .expect("hub list");
    let peers: Vec<Value> = serde_json::from_value(list.details["peers"].clone()).expect("peers");
    let alpha_peer = peers
        .iter()
        .find(|peer| peer["id"] == "Alpha")
        .expect("alpha peer");
    let beta_peer = peers
        .iter()
        .find(|peer| peer["id"] == "Beta")
        .expect("beta peer");
    assert_eq!(alpha_peer["status"], "idle");
    assert_eq!(beta_peer["status"], "aborted");
    // Beta was aborted mid-run; Alpha still holds the queued IRC message.
    assert_eq!(
        controlled.runtime.inbox("Beta", true).len(),
        1,
        "aborted agent retains queued mail until drained"
    );

    // 8) Parked delivery is truthful: mail queues without claiming execution revival.
    // Status stays Parked because no live session resumes.
    controlled.runtime.park("Alpha").expect("park alpha");
    let parked = controlled
        .runtime
        .list("Main")
        .into_iter()
        .find(|peer| peer.id == "Alpha")
        .expect("alpha listed");
    assert_eq!(parked.status, AgentStatus::Parked);
    let parked_mail = controlled
        .runtime
        .send("Main", "Alpha", "mail while parked", None);
    assert_eq!(parked_mail[0].outcome, DeliveryOutcome::Queued);
    assert!(parked_mail[0].error.is_none());
    let still_parked = controlled
        .runtime
        .list("Main")
        .into_iter()
        .find(|peer| peer.id == "Alpha")
        .expect("alpha after parked mail");
    assert_eq!(
        still_parked.status,
        AgentStatus::Parked,
        "parked delivery must not claim revival without session resume"
    );
    // Mailbox capacity is 2: first parked message occupies one slot.
    assert_eq!(
        controlled
            .runtime
            .send("Main", "Alpha", "second", None)[0]
            .outcome,
        DeliveryOutcome::Queued
    );
    assert_eq!(
        controlled
            .runtime
            .send("Main", "Alpha", "overflow", None)[0]
            .outcome,
        DeliveryOutcome::Failed
    );
    assert_eq!(controlled.runtime.inbox("Alpha", true).len(), 2);
    assert_eq!(
        controlled
            .runtime
            .list("Main")
            .into_iter()
            .find(|peer| peer.id == "Alpha")
            .expect("alpha final")
            .status,
        AgentStatus::Parked
    );

    // 9) Group isolation: a second runtime cannot see or message this group.
    let other_artifacts = tempfile::tempdir().expect("other artifacts");
    let other = controlled_runtime(other_artifacts.path(), 1, 2, None);
    assert!(
        other
            .runtime
            .list("Main")
            .iter()
            .all(|peer| peer.id != "Alpha" && peer.id != "Beta")
    );
    assert_eq!(
        other.runtime.send("Main", "Alpha", "cross-group leak", None)[0].outcome,
        DeliveryOutcome::Failed
    );
    other.runtime.shutdown().await;

    // 10) Owner shutdown cleans the group.
    controlled.runtime.shutdown().await;
    assert_eq!(controlled.runtime.active_child_count(), 0);
    assert!(controlled.runtime.list("Main").is_empty());
    assert_eq!(
        controlled
            .runtime
            .send("Main", "Alpha", "after shutdown", None)[0]
            .outcome,
        DeliveryOutcome::Failed
    );
}

/// Owner drop cancels in-flight jobs and unblocks synchronous waiters.
#[tokio::test]
async fn owner_shutdown_cancels_inflight_and_clears_roster() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let ControlledRuntime {
        runtime,
        current,
        started,
        release: _,
        aborted,
        peak: _,
    } = controlled_runtime(artifacts.path(), 1, 8, None);
    let tools = runtime.agent_tools("Main", 0);
    let task = tool(&tools, "task");
    let hub = tool(&tools, "hub");

    let spawn = tokio::time::timeout(
        Duration::from_millis(500),
        (task.execute)(context(
            "spawn-shutdown",
            json!({ "name": "Linger", "task": "stay blocked" }),
        )),
    )
    .await
    .expect("spawn returned")
    .expect("spawn ok");
    let job_id = serde_json::from_value::<Vec<TaskSpawn>>(spawn.details)
        .expect("spawns")
        .remove(0)
        .job_id;
    wait_for_count(&current, &started, 1, "linger running").await;

    // Mail while running is Queued.
    assert_eq!(
        runtime.send("Main", "Linger", "ping", None)[0].outcome,
        DeliveryOutcome::Queued
    );

    let wait = (hub.execute)(context(
        "wait-during-shutdown",
        json!({ "op": "wait", "ids": [job_id.clone()], "timeoutMs": 2000 }),
    ));
    let waiter = tokio::spawn(wait);
    tokio::task::yield_now().await;

    runtime.shutdown().await;

    // After shutdown the group is gone; wait may error or observe cancel.
    let waited = tokio::time::timeout(Duration::from_secs(2), waiter)
        .await
        .expect("waiter joined")
        .expect("hub wait join");
    match waited {
        Ok(result) => {
            let jobs: Vec<JobSnapshot> =
                serde_json::from_value(result.details["jobs"].clone()).unwrap_or_default();
            assert!(
                jobs.is_empty()
                    || jobs.iter().all(|job| job.status.is_settled()),
                "post-shutdown wait must not report running jobs: {jobs:?}"
            );
        }
        Err(error) => {
            let message = error.to_string();
            assert!(
                message.contains("shut down")
                    || message.contains("aborted")
                    || message.contains("unknown"),
                "unexpected shutdown wait error: {message}"
            );
        }
    }
    assert!(aborted.load(Ordering::SeqCst) >= 1 || runtime.active_child_count() == 0);
    assert!(runtime.list("Main").is_empty());
    assert_eq!(runtime.active_child_count(), 0);
}
