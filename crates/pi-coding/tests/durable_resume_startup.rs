//! Application constructor contract used by TUI/`--resume` durable startup.
//!
//! Shared constructor: `ApplicationRuntimeCandidate` + optional orchestration,
//! then `Application::from_runtime_candidate`. Sidecar present → resume at the
//! persisted revision. No sidecar → `build()` revision 1. Never
//! `attach_orchestration` after a successful constructor. Semantic durable-
//! policy inequality stays fail-closed.
//!
//! Isolated HOME/temp agent/session dirs only. Never writes `~/.pi`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use pi_agent::ThinkingLevel;
use pi_ai::{Model, StopReason};
use pi_coding::{
    AgentCatalog, AgentDefinition, AgentDefinitionSource, Application, ApplicationRuntimeCandidate,
    DURABLE_STATE_VERSION, DurablePolicyState, JobStatus, OrchestrationConfig,
    OrchestrationRuntime, ResourceManager, ResourceManagerOptions, Session, SessionOptions,
    TaskItem, resume_session, start_session_in,
};
use pi_sandbox::{
    CredentialGrantId, CredentialPolicy, PolicyRevision, PolicySnapshot, SandboxPolicy,
};
use tokio::sync::Barrier;
use tokio_util::sync::CancellationToken;

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
        id: "durable-orch-e2e".to_owned(),
        name: "Durable Orch E2E".to_owned(),
        api: pi_ai::API_FAUX.to_owned(),
        provider: "faux".to_owned(),
        ..Model::default()
    }
}

fn quick_factory(cwd: PathBuf, response_text: &'static str) -> pi_coding::ChildSessionFactory {
    Arc::new(move |request| {
        let cwd = cwd.clone();
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
            Session::new_with_additional_tools_filtered_and_discovery(
                SessionOptions {
                    model: request.model,
                    cwd,
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
                },
                Vec::new(),
                Default::default(),
                pi_coding::ResourceDiscovery::Disabled,
            )
        })
    })
}

fn held_factory(
    cwd: PathBuf,
    started: Arc<Barrier>,
    release: CancellationToken,
) -> pi_coding::ChildSessionFactory {
    Arc::new(move |request| {
        let cwd = cwd.clone();
        let started = started.clone();
        let release = release.clone();
        Box::pin(async move {
            let stream_fn: pi_agent::StreamFn = Arc::new(move |model, _ctx, options| {
                let started = started.clone();
                let release = release.clone();
                Box::pin(async move {
                    let stream = pi_ai::new_assistant_message_event_stream();
                    let producer = stream.clone();
                    tokio::spawn(async move {
                        started.wait().await;
                        let was_aborted = match options.stream.abort_signal {
                            Some(abort) => tokio::select! {
                                () = release.cancelled() => false,
                                () = abort.cancelled() => true,
                            },
                            None => {
                                release.cancelled().await;
                                false
                            }
                        };
                        let mut message = pi_ai::AssistantMessage::pending(&model);
                        message.stop_reason = if was_aborted {
                            StopReason::Aborted
                        } else {
                            message.content.push(pi_ai::ContentBlock::text("released"));
                            StopReason::Stop
                        };
                        producer.end(Some(message)).await;
                    });
                    stream
                })
            });
            Session::new_with_additional_tools_filtered_and_discovery(
                SessionOptions {
                    model: request.model,
                    cwd,
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
                },
                Vec::new(),
                Default::default(),
                pi_coding::ResourceDiscovery::Disabled,
            )
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
    let parent = Session::new_with_additional_tools_filtered_and_discovery(
        SessionOptions {
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
            auth_manager: None,
        },
        Vec::new(),
        Default::default(),
        pi_coding::ResourceDiscovery::Disabled,
    )
    .expect("parent session");
    parent.set_session_dir(session_dir.to_path_buf());
    parent.record(recorder).expect("attach parent recorder");
    (parent, parent_id, parent_path)
}

fn child_root_for(parent_path: &Path, parent_id: &str) -> PathBuf {
    parent_path
        .parent()
        .expect("parent dir")
        .join("children")
        .join(parent_id)
}

async fn attach_isolated_resources(parent: &Session) -> anyhow::Result<()> {
    let agent_dir = parent
        .session_dir()
        .join("agents")
        .join(uuid::Uuid::now_v7().to_string());
    std::fs::create_dir_all(&agent_dir)?;
    std::fs::write(
        agent_dir.join("settings.json"),
        r#"{"sandbox":{"mode":"danger-full-access"}}"#,
    )?;
    let mut options = ResourceManagerOptions::new(parent.cwd());
    options.agent_dir = agent_dir;
    options.disable_extensions = true;
    options.disable_skills = true;
    options.disable_prompt_templates = true;
    options.disable_themes = true;
    options.disable_context_files = true;
    parent
        .attach_resources(ResourceManager::new(options)?)
        .await?;
    Ok(())
}

async fn application_with_orchestration(
    parent: &Session,
    runtime: OrchestrationRuntime,
) -> anyhow::Result<Application> {
    attach_isolated_resources(parent).await?;
    Application::from_runtime_candidate(
        ApplicationRuntimeCandidate::new(parent.clone()).with_orchestration(runtime),
    )
    .await
}

fn restarted_parent_session(parent: &Session) -> anyhow::Result<Session> {
    let (_, parent_path) = parent
        .recorder_info()
        .ok_or_else(|| anyhow::anyhow!("parent recording is unavailable"))?;
    let restarted = Session::new_with_additional_tools_filtered_and_discovery(
        SessionOptions {
            model: test_model(),
            cwd: parent.cwd().to_path_buf(),
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
        },
        Vec::new(),
        Default::default(),
        pi_coding::ResourceDiscovery::Disabled,
    )?;
    restarted.set_session_dir(parent.session_dir());
    restarted.record(resume_session(parent_path)?)?;
    Ok(restarted)
}

fn load_sidecar(sidecar: &Path) -> pi_coding::DurableState {
    serde_json::from_slice(&std::fs::read(sidecar).expect("read sidecar")).expect("decode sidecar")
}

fn write_sidecar(sidecar: &Path, state: &pi_coding::DurableState) {
    std::fs::write(
        sidecar,
        serde_json::to_vec(state).expect("serialize sidecar"),
    )
    .expect("overwrite sidecar");
}

fn bump_sidecar_revision(
    sidecar: &Path,
    persisted: &pi_coding::DurableState,
) -> DurablePolicyState {
    let captured = persisted.durable_policy.clone();
    let bumped = DurablePolicyState::new(
        PolicySnapshot::new(
            PolicyRevision::new(2).expect("revision 2"),
            captured.snapshot().created_on(),
            captured.snapshot().policy().clone(),
        )
        .expect("revision-2 snapshot"),
        captured.credential_grants().clone(),
    )
    .expect("revision-2 durable policy");
    write_sidecar(
        sidecar,
        &pi_coding::DurableState {
            version: DURABLE_STATE_VERSION,
            parent_session_id: persisted.parent_session_id.clone(),
            parent_session_path: persisted.parent_session_path.clone(),
            group_id: persisted.group_id.clone(),
            durable_policy: bumped.clone(),
            main_mailbox: persisted.main_mailbox.clone(),
            delivered_messages: persisted.delivered_messages.clone(),
            agents: persisted.agents.clone(),
            jobs: persisted.jobs.clone(),
        },
    );
    bumped
}

async fn bind_revision_one_sidecar(
    artifacts: &Path,
    parent: &Session,
    sidecar: &Path,
) -> pi_coding::DurableState {
    let runtime = OrchestrationRuntime::new(
        config(artifacts, vec![task_definition()]),
        quick_factory(artifacts.to_path_buf(), "unused"),
    )
    .expect("seed runtime");
    let application = application_with_orchestration(parent, runtime)
        .await
        .expect("bind sidecar at revision 1");
    assert!(sidecar.is_file(), "sidecar written: {}", sidecar.display());
    let persisted = load_sidecar(sidecar);
    assert_eq!(
        persisted.group_id,
        application
            .orchestration_runtime()
            .expect("seed runtime")
            .group_id(),
    );
    assert_eq!(
        persisted.durable_policy.snapshot().revision().get(),
        1,
        "fresh bind writes revision 1"
    );
    application.cleanup().await;
    persisted
}

fn format_error_chain(error: &anyhow::Error) -> String {
    let mut chain = error.to_string();
    for cause in error.chain().skip(1) {
        chain.push_str(" :: ");
        chain.push_str(&cause.to_string());
    }
    chain
}

#[tokio::test]
async fn from_runtime_candidate_resumes_sidecar_revision() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let sessions = tempfile::tempdir().expect("sessions");
    let (parent, parent_id, parent_path) = parent_session(artifacts.path(), sessions.path());
    let sidecar = child_root_for(&parent_path, &parent_id).join("orchestration-state.json");

    let persisted = bind_revision_one_sidecar(artifacts.path(), &parent, &sidecar).await;
    let persisted_group_id = persisted.group_id.clone();
    bump_sidecar_revision(&sidecar, &persisted);

    let resume_runtime = OrchestrationRuntime::new(
        config(artifacts.path(), vec![task_definition()]),
        quick_factory(artifacts.path().to_path_buf(), "unused"),
    )
    .expect("resume runtime");
    let resumed_parent = restarted_parent_session(&parent).expect("reopen parent");
    let application = application_with_orchestration(&resumed_parent, resume_runtime)
        .await
        .expect("legal TUI resume must accept sidecar revision 2");
    assert_eq!(
        application
            .orchestration_runtime()
            .expect("resumed runtime")
            .group_id(),
        persisted_group_id,
        "durable construction must restore the exact orchestration group identity",
    );
    assert_eq!(
        application
            .durable_policy_state()
            .expect("resumed durable policy")
            .snapshot()
            .revision()
            .get(),
        2,
        "from_runtime_candidate must resume the persisted sidecar revision"
    );
    application.cleanup().await;
}

#[tokio::test]
async fn resumed_application_preserves_running_jobs_for_revival_and_accepts_new_work() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let sessions = tempfile::tempdir().expect("sessions");
    let (parent, parent_id, parent_path) = parent_session(artifacts.path(), sessions.path());
    let sidecar = child_root_for(&parent_path, &parent_id).join("orchestration-state.json");
    let started = Arc::new(Barrier::new(3));
    let release = CancellationToken::new();
    let seed_runtime = OrchestrationRuntime::new(
        config(artifacts.path(), vec![task_definition()]),
        held_factory(
            artifacts.path().to_path_buf(),
            started.clone(),
            release.clone(),
        ),
    )
    .expect("seed runtime");
    let seed_application = application_with_orchestration(&parent, seed_runtime.clone())
        .await
        .expect("seed application");
    let spawns = seed_runtime
        .spawn_tasks(
            "Main",
            0,
            vec![
                TaskItem {
                    index: 0,
                    id: "InterruptedAlpha".to_owned(),
                    agent: "task".to_owned(),
                    assignment: "remain active across captured restart state".to_owned(),
                    ..Default::default()
                },
                TaskItem {
                    index: 1,
                    id: "InterruptedBeta".to_owned(),
                    agent: "task".to_owned(),
                    assignment: "remain active across captured restart state".to_owned(),
                    ..Default::default()
                },
            ],
        )
        .expect("spawn running jobs");
    tokio::time::timeout(Duration::from_secs(3), started.wait())
        .await
        .expect("both jobs entered the child stream");
    let job_ids = spawns
        .iter()
        .map(|spawn| spawn.job_id.clone())
        .collect::<Vec<_>>();
    let running = seed_runtime.jobs(Some(&job_ids));
    assert_eq!(running.len(), 2);
    assert!(
        running.iter().all(|job| job.status == JobStatus::Running),
        "{running:#?}"
    );
    let interrupted_sidecar = std::fs::read(&sidecar).expect("capture running sidecar");

    seed_application.cleanup().await;
    std::fs::write(&sidecar, interrupted_sidecar).expect("restore process-interrupted sidecar");

    let restarted_parent = restarted_parent_session(&parent).expect("reopen parent");
    let resumed_runtime = OrchestrationRuntime::new(
        config(artifacts.path(), vec![task_definition()]),
        quick_factory(artifacts.path().to_path_buf(), "fresh work completed"),
    )
    .expect("resumed runtime");
    let resumed_application =
        application_with_orchestration(&restarted_parent, resumed_runtime.clone())
            .await
            .expect("resumed application");
    let recovered = resumed_runtime.jobs(Some(&job_ids));
    assert_eq!(recovered.len(), 2);
    assert!(
        recovered.iter().all(|job| job.status == JobStatus::Running),
        "recovered jobs keep Running status for revival: {recovered:#?}"
    );
    assert!(
        recovered.iter().all(|job| job.result.is_none()),
        "recovered jobs get no synthetic error result: {recovered:#?}"
    );
    assert_eq!(
        recovered
            .iter()
            .map(|job| job.id.clone())
            .collect::<std::collections::BTreeSet<_>>(),
        job_ids
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>(),
        "recovered jobs preserve exact job IDs"
    );

    let mut continued = Vec::new();
    for job_id in &job_ids {
        let snapshots = tokio::time::timeout(
            Duration::from_secs(5),
            resumed_runtime.wait_jobs(std::slice::from_ref(job_id), None, None),
        )
        .await
        .expect("recovered job continues after restart")
        .expect("recovered job wait result");
        continued.extend(snapshots);
    }
    assert_eq!(continued.len(), 2);
    assert!(
        continued
            .iter()
            .all(|job| job.status == JobStatus::Completed),
        "recovered jobs must continue to Completed, not Cancelled: {continued:#?}"
    );
    assert_eq!(
        continued
            .iter()
            .map(|job| job.id.clone())
            .collect::<std::collections::BTreeSet<_>>(),
        job_ids
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>(),
        "continuation preserves exact job IDs"
    );
    for job in &continued {
        let output = job
            .result
            .as_ref()
            .map(|result| result.output.as_str())
            .unwrap_or("");
        assert!(
            output.starts_with("fresh work completed"),
            "revived child must continue on the recovered identity: {job:#?}"
        );
        assert!(
            !output.contains("orchestration shut down"),
            "revived child must not observe runtime shutdown: {job:#?}"
        );
    }

    let fresh = resumed_runtime
        .spawn_tasks(
            "Main",
            0,
            vec![TaskItem {
                index: 0,
                id: "FreshAfterRestart".to_owned(),
                agent: "task".to_owned(),
                assignment: "prove the resumed runtime accepts new work".to_owned(),
                ..Default::default()
            }],
        )
        .expect("spawn fresh post-restart work");
    let fresh_ids = vec![fresh[0].job_id.clone()];
    let fresh_result = tokio::time::timeout(
        Duration::from_secs(3),
        resumed_runtime.wait_jobs(&fresh_ids, None, None),
    )
    .await
    .expect("fresh post-restart wait completes")
    .expect("fresh post-restart wait result");
    assert_eq!(fresh_result.len(), 1);
    assert_eq!(fresh_result[0].status, JobStatus::Completed);
    let fresh_output = fresh_result[0]
        .result
        .as_ref()
        .map(|result| result.output.as_str())
        .expect("fresh post-restart output");
    assert!(
        fresh_output.starts_with("fresh work completed"),
        "{fresh_output:?}"
    );
    resumed_application.cleanup().await;
    release.cancel();
}

#[tokio::test]
async fn durable_constructor_keeps_runtime_owner_alive_for_new_task() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let sessions = tempfile::tempdir().expect("sessions");
    let (parent, _, _) = parent_session(artifacts.path(), sessions.path());
    let seed_runtime = OrchestrationRuntime::new(
        config(artifacts.path(), vec![task_definition()]),
        quick_factory(artifacts.path().to_path_buf(), "seed completed"),
    )
    .expect("seed runtime");
    let seed_application = application_with_orchestration(&parent, seed_runtime)
        .await
        .expect("seed durable sidecar");
    seed_application
        .cleanup()
        .await
        .expect("close seed application");

    let restarted_parent = restarted_parent_session(&parent).expect("reopen parent");
    let resumed_runtime = OrchestrationRuntime::new(
        config(artifacts.path(), vec![task_definition()]),
        quick_factory(artifacts.path().to_path_buf(), "resumed task completed"),
    )
    .expect("resumed runtime");
    let observation = resumed_runtime.clone();
    drop(observation.clone());
    assert!(
        !observation.is_shutdown(),
        "dropping a non-owner clone must be inert"
    );

    let resumed_application = application_with_orchestration(&restarted_parent, resumed_runtime)
        .await
        .expect("construct durable resumed application");
    assert!(
        !observation.is_shutdown(),
        "durable construction must move the sole runtime owner into the application",
    );
    let spawn = observation
        .spawn_tasks(
            "Main",
            0,
            vec![TaskItem {
                index: 0,
                id: "ConstructorChild".to_owned(),
                agent: "task".to_owned(),
                assignment: "complete after durable constructor return".to_owned(),
                ..Default::default()
            }],
        )
        .expect("spawn after durable constructor")
        .remove(0);
    let settled = tokio::time::timeout(
        Duration::from_secs(3),
        observation.wait_jobs(std::slice::from_ref(&spawn.job_id), None, None),
    )
    .await
    .expect("post-constructor wait completes")
    .expect("post-constructor wait result");
    assert_eq!(settled.len(), 1);
    assert_eq!(settled[0].id, spawn.job_id);
    assert_eq!(settled[0].agent_id, spawn.agent_id);
    assert_eq!(settled[0].parent_id, "Main");
    assert_eq!(settled[0].status, JobStatus::Completed);
    let output = settled[0]
        .result
        .as_ref()
        .map(|result| result.output.as_str())
        .unwrap_or_default();
    assert!(output.starts_with("resumed task completed"), "{output:?}");
    assert!(!output.contains("orchestration shut down"), "{output:?}");

    resumed_application
        .cleanup()
        .await
        .expect("drop sole runtime owner");
    assert!(
        observation.is_shutdown(),
        "terminal application cleanup must shut down the sole runtime owner",
    );
}

#[tokio::test]
async fn explicit_shutdown_with_running_child_cancels_waiters_and_clears_registry() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let sessions = tempfile::tempdir().expect("sessions");
    let (parent, _, _) = parent_session(artifacts.path(), sessions.path());
    let started = Arc::new(Barrier::new(2));
    let release = CancellationToken::new();
    let runtime = OrchestrationRuntime::new(
        config(artifacts.path(), vec![task_definition()]),
        held_factory(
            artifacts.path().to_path_buf(),
            started.clone(),
            release.clone(),
        ),
    )
    .expect("runtime");
    let application = application_with_orchestration(&parent, runtime.clone())
        .await
        .expect("application");
    let spawn = runtime
        .spawn_tasks(
            "Main",
            0,
            vec![TaskItem {
                index: 0,
                id: "ShutdownChild".to_owned(),
                agent: "task".to_owned(),
                assignment: "remain running until explicit shutdown".to_owned(),
                ..Default::default()
            }],
        )
        .expect("spawn")
        .remove(0);
    tokio::time::timeout(Duration::from_secs(3), started.wait())
        .await
        .expect("child entered stream");
    assert_eq!(
        runtime.jobs(Some(std::slice::from_ref(&spawn.job_id)))[0].status,
        JobStatus::Running
    );

    let wait = tokio::spawn({
        let runtime = runtime.clone();
        async move { runtime.wait_message("Main", None, None, None).await }
    });
    tokio::task::yield_now().await;
    runtime.shutdown().await;
    assert!(
        runtime.is_shutdown(),
        "explicit shutdown must cancel waiters"
    );
    assert_eq!(runtime.active_child_count(), 0);
    let wait_error = wait
        .await
        .expect("wait join")
        .expect_err("no-abort wait must fail");
    assert!(
        wait_error.to_string().contains("orchestration shut down"),
        "{wait_error:#}"
    );
    application.cleanup().await;
    release.cancel();
}

#[tokio::test]
async fn explicit_shutdown_reopen_yields_zero_running_revival() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let sessions = tempfile::tempdir().expect("sessions");
    let (parent, parent_id, parent_path) = parent_session(artifacts.path(), sessions.path());
    let sidecar = child_root_for(&parent_path, &parent_id).join("orchestration-state.json");
    let started = Arc::new(Barrier::new(2));
    let release = CancellationToken::new();
    let runtime = OrchestrationRuntime::new(
        config(artifacts.path(), vec![task_definition()]),
        held_factory(
            artifacts.path().to_path_buf(),
            started.clone(),
            release.clone(),
        ),
    )
    .expect("runtime");
    let application = application_with_orchestration(&parent, runtime.clone())
        .await
        .expect("application");
    let spawn = runtime
        .spawn_tasks(
            "Main",
            0,
            vec![TaskItem {
                index: 0,
                id: "TerminalShutdownChild".to_owned(),
                agent: "task".to_owned(),
                assignment: "terminate durably during explicit shutdown".to_owned(),
                ..Default::default()
            }],
        )
        .expect("spawn")
        .remove(0);
    tokio::time::timeout(Duration::from_secs(3), started.wait())
        .await
        .expect("child entered stream");

    runtime.shutdown().await.expect("terminal shutdown");
    let terminal = load_sidecar(&sidecar);
    let terminal_job = terminal
        .jobs
        .iter()
        .find(|job| job.id == spawn.job_id)
        .expect("terminal job persisted");
    assert_eq!(terminal_job.status, JobStatus::Cancelled);
    assert_eq!(
        terminal_job
            .result
            .as_ref()
            .and_then(|result| result.error.as_deref()),
        Some("orchestration shut down")
    );

    let restarted_parent = restarted_parent_session(&parent).expect("reopen parent");
    let resumed_runtime = OrchestrationRuntime::new(
        config(artifacts.path(), vec![task_definition()]),
        quick_factory(artifacts.path().to_path_buf(), "must not run"),
    )
    .expect("resumed runtime");
    let resumed_application =
        application_with_orchestration(&restarted_parent, resumed_runtime.clone())
            .await
            .expect("resumed application");
    let recovered = resumed_runtime.jobs(Some(std::slice::from_ref(&spawn.job_id)));
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].status, JobStatus::Cancelled);
    assert_eq!(resumed_runtime.active_child_count(), 0);

    resumed_application
        .cleanup()
        .await
        .expect("cleanup resumed application");
    application
        .cleanup()
        .await
        .expect("cleanup source application");
    release.cancel();
}

#[tokio::test]
async fn checkpoint_for_restart_preserves_running_job_without_shutdown_error() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let sessions = tempfile::tempdir().expect("sessions");
    let (parent, parent_id, parent_path) = parent_session(artifacts.path(), sessions.path());
    let sidecar = child_root_for(&parent_path, &parent_id).join("orchestration-state.json");
    let started = Arc::new(Barrier::new(2));
    let release = CancellationToken::new();
    let seed_runtime = OrchestrationRuntime::new(
        config(artifacts.path(), vec![task_definition()]),
        held_factory(
            artifacts.path().to_path_buf(),
            started.clone(),
            release.clone(),
        ),
    )
    .expect("seed runtime");
    let seed_application = application_with_orchestration(&parent, seed_runtime.clone())
        .await
        .expect("seed application");
    let spawn = seed_runtime
        .spawn_tasks(
            "Main",
            0,
            vec![TaskItem {
                index: 0,
                id: "CheckpointChild".to_owned(),
                agent: "task".to_owned(),
                assignment: "remain running across checkpoint".to_owned(),
                ..Default::default()
            }],
        )
        .expect("spawn")
        .remove(0);
    tokio::time::timeout(Duration::from_secs(3), started.wait())
        .await
        .expect("child entered stream");
    Application::checkpoint_process_for_restart(std::slice::from_ref(&seed_application), None)
        .expect("checkpoint application process graph");
    seed_application.cleanup().await.expect("restart cleanup");
    let checkpointed: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&sidecar).expect("read production checkpoint sidecar"),
    )
    .expect("parse production checkpoint sidecar");
    let persisted_job = checkpointed["jobs"]
        .as_array()
        .expect("checkpoint jobs")
        .iter()
        .find(|job| job["id"] == spawn.job_id)
        .expect("checkpointed running job");
    assert_eq!(persisted_job["status"], "running");
    assert!(persisted_job["finishedAt"].is_null());
    assert!(persisted_job["result"].is_null());

    let restarted_parent = restarted_parent_session(&parent).expect("reopen parent");
    let resumed_runtime = OrchestrationRuntime::new(
        config(artifacts.path(), vec![task_definition()]),
        quick_factory(artifacts.path().to_path_buf(), "fresh work completed"),
    )
    .expect("resumed runtime");
    let checkpointed_group_id = checkpointed["groupId"]
        .as_str()
        .expect("checkpointed group identity")
        .to_owned();
    let resumed_application =
        application_with_orchestration(&restarted_parent, resumed_runtime.clone())
            .await
            .expect("resumed application");
    assert_eq!(
        resumed_runtime.group_id(),
        checkpointed_group_id,
        "process restart must preserve group identity before revival",
    );
    let recovered = resumed_runtime.jobs(Some(std::slice::from_ref(&spawn.job_id)));
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].id, spawn.job_id);
    assert_eq!(recovered[0].status, JobStatus::Running);
    assert!(recovered[0].result.is_none());
    let settled = tokio::time::timeout(
        Duration::from_secs(5),
        resumed_runtime.wait_jobs(std::slice::from_ref(&spawn.job_id), None, None),
    )
    .await
    .expect("continuation wait")
    .expect("continuation result");
    assert_eq!(settled[0].status, JobStatus::Completed);
    let output = settled[0]
        .result
        .as_ref()
        .map(|result| result.output.as_str())
        .unwrap_or("");
    assert!(output.starts_with("fresh work completed"), "{output:?}");
    assert!(!output.contains("orchestration shut down"), "{output:?}");
    resumed_application.cleanup().await;
    release.cancel();
}

#[tokio::test]
async fn new_with_extensions_then_attach_accepts_same_authority_revision_drift() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let sessions = tempfile::tempdir().expect("sessions");
    let (parent, parent_id, parent_path) = parent_session(artifacts.path(), sessions.path());
    let sidecar = child_root_for(&parent_path, &parent_id).join("orchestration-state.json");

    let persisted = bind_revision_one_sidecar(artifacts.path(), &parent, &sidecar).await;
    bump_sidecar_revision(&sidecar, &persisted);

    // Old TUI path: bootstrap revision 1 without reading the sidecar, then
    // attach_orchestration. Prefer Application::new (no ExtensionRuntime) over
    // new_with_orchestration, which would bind_and_recover inside build().
    let drifted_parent = restarted_parent_session(&parent).expect("reopen parent");
    attach_isolated_resources(&drifted_parent)
        .await
        .expect("isolated resources");
    let application = Application::new(drifted_parent.clone()).await;
    assert_eq!(
        application
            .durable_policy_state()
            .expect("bootstrap durable policy")
            .snapshot()
            .revision()
            .get(),
        1,
        "Application::new must bootstrap revision 1 without the sidecar"
    );

    let attach_runtime = OrchestrationRuntime::new(
        config(artifacts.path(), vec![task_definition()]),
        quick_factory(artifacts.path().to_path_buf(), "unused"),
    )
    .expect("attach runtime");
    // A later generation counter with the same sandbox body is the same
    // authority. attach_orchestration goes through bind_and_recover, which
    // accepts that revision-only difference and rewrites the sidecar to the
    // live generation. Credential changes stay fail-closed in
    // from_runtime_candidate_rejects_semantic_credential_drift.
    application
        .attach_orchestration(attach_runtime.clone())
        .await
        .expect("same-authority revision drift must recover");
    let recovered = load_sidecar(&sidecar);
    let live = application
        .durable_policy_state()
        .expect("live durable policy after attach");
    assert!(
        pi_coding::durable_policy_same_authority(&live, &recovered.durable_policy),
        "recovered sidecar must keep the sandbox policy body"
    );
    assert_eq!(
        recovered.durable_policy.snapshot().revision(),
        live.snapshot().revision(),
        "recovery persists the live generation revision"
    );
    application.cleanup().await;
    attach_runtime.shutdown().await;
}

#[tokio::test]
async fn from_runtime_candidate_rejects_semantic_credential_drift() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let sessions = tempfile::tempdir().expect("sessions");
    let (parent, parent_id, parent_path) = parent_session(artifacts.path(), sessions.path());
    let sidecar = child_root_for(&parent_path, &parent_id).join("orchestration-state.json");

    let persisted = bind_revision_one_sidecar(artifacts.path(), &parent, &sidecar).await;
    let captured = persisted.durable_policy.clone();
    let drifted_creds = CredentialPolicy::new(vec![
        CredentialGrantId::parse("github-extra".to_owned()).expect("grant id"),
    ])
    .expect("drifted credentials");
    let drifted = DurablePolicyState::new(
        PolicySnapshot::new(
            captured.snapshot().revision(),
            captured.snapshot().created_on(),
            SandboxPolicy::danger_full_access(drifted_creds).expect("drifted policy"),
        )
        .expect("drifted snapshot"),
        captured.credential_grants().clone(),
    )
    .expect("drifted durable policy");
    write_sidecar(
        &sidecar,
        &pi_coding::DurableState {
            version: DURABLE_STATE_VERSION,
            parent_session_id: persisted.parent_session_id.clone(),
            parent_session_path: persisted.parent_session_path.clone(),
            group_id: persisted.group_id.clone(),
            durable_policy: drifted,
            main_mailbox: persisted.main_mailbox.clone(),
            delivered_messages: persisted.delivered_messages.clone(),
            agents: persisted.agents.clone(),
            jobs: persisted.jobs.clone(),
        },
    );

    let resume_runtime = OrchestrationRuntime::new(
        config(artifacts.path(), vec![task_definition()]),
        quick_factory(artifacts.path().to_path_buf(), "unused"),
    )
    .expect("resume runtime");
    let resumed_parent = restarted_parent_session(&parent).expect("reopen parent");
    let err = match application_with_orchestration(&resumed_parent, resume_runtime.clone()).await {
        Ok(application) => {
            application.cleanup().await;
            panic!("semantic credential drift must fail closed, not resume");
        }
        Err(error) => error,
    };
    let chain = format_error_chain(&err);
    assert!(
        chain.contains("durable")
            || chain.contains("credential")
            || chain.contains("policy")
            || chain.contains("grant"),
        "credential drift must reject as durable/credential/policy, got: {chain}"
    );
    resume_runtime.shutdown().await;
}
