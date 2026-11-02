//! Focused contract: `durable orchestration policy does not match resumed generation`
//!
//! Equality check: `crates/pi-coding/src/orchestration/runtime.rs:2685` in
//! `prepare_recovered_state`, comparing `DurablePolicyState::capture(current
//! generation)` against the persisted sidecar `durable_policy`.
//!
//! Two deterministic, isolated scenarios (tempdirs only — never real ~/.pi):
//! 1. A same-parent sidecar whose persisted policy differs in a *semantic*
//!    field (`credentials.allowed_grants`) must fail closed with the exact
//!    bail, and the differing `PolicySnapshot`/`DurablePolicyState` fields
//!    (mode, roots, denied, scratch, credentials, revision, digest, grant
//!    registry) are surfaced.
//! 2. A sidecar differing only in the internal `revision` counter (all
//!    semantic fields identical) must also fail closed — demonstrating that
//!    the equality is not purely semantic (revision/digest participate).
//!
//! The normal durable-resume path reconstructs the exact persisted policy, so
//! this check only fires on genuine (or `revision`-only) drift and is
//! intentionally fail-closed: it refuses to resume orchestration under a
//! different sandbox authority than the one the sidecar was created under.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pi_agent::{AgentTool, ThinkingLevel};
use pi_ai::{Model, StopReason};
use pi_coding::{
    AgentCatalog, AgentDefinition, AgentDefinitionSource, Application, ApplicationRuntimeCandidate,
    DURABLE_STATE_VERSION, DurablePolicyState, OrchestrationConfig, OrchestrationRuntime,
    ResourceManager, ResourceManagerOptions, Session, SessionOptions, start_session_in,
};
use pi_sandbox::{
    CredentialGrantId, CredentialPolicy, PolicyRevision, PolicySnapshot, SandboxPolicy,
};

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

fn quick_factory(response_text: &'static str) -> pi_coding::ChildSessionFactory {
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
            Session::new_with_additional_tools_filtered_and_discovery(
                SessionOptions {
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

async fn application_with_settings(
    parent: &Session,
    runtime: OrchestrationRuntime,
    agent_dir: PathBuf,
    settings: &str,
) -> anyhow::Result<Application> {
    std::fs::create_dir_all(&agent_dir)?;
    std::fs::write(agent_dir.join("settings.json"), settings)?;
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
    Application::from_runtime_candidate(
        ApplicationRuntimeCandidate::new(parent.clone()).with_orchestration(runtime),
    )
    .await
}

async fn application_with_orchestration(
    parent: &Session,
    runtime: OrchestrationRuntime,
) -> anyhow::Result<Application> {
    let agent_dir = parent
        .session_dir()
        .join("agents")
        .join(uuid::Uuid::now_v7().to_string());
    application_with_settings(
        parent,
        runtime,
        agent_dir,
        r#"{"sandbox":{"mode":"danger-full-access"}}"#,
    )
    .await
}

fn restarted_parent_session(parent: &Session, workspace: &Path) -> anyhow::Result<Session> {
    let (_, parent_path) = parent
        .recorder_info()
        .ok_or_else(|| anyhow::anyhow!("parent recording is unavailable"))?;
    let restarted = Session::new_with_additional_tools_filtered_and_discovery(
        SessionOptions {
            model: test_model(),
            cwd: workspace.to_path_buf(),
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
    restarted.record(pi_coding::resume_session(parent_path)?)?;
    Ok(restarted)
}

fn error_chain(error: &anyhow::Error) -> String {
    error
        .chain()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" :: ")
}

/// Render captured vs persisted snapshot fields for the report.
fn field_diff(captured: &DurablePolicyState, persisted: &DurablePolicyState) -> Vec<String> {
    let mut out = Vec::new();
    let cs = captured.snapshot();
    let ps = persisted.snapshot();
    out.push(format!(
        "mode                        captured={:?} persisted={:?} equal={}",
        cs.policy().mode(),
        ps.policy().mode(),
        cs.policy().mode() == ps.policy().mode()
    ));
    out.push(format!(
        "revision                    captured={:?} persisted={:?} equal={}",
        cs.revision(),
        ps.revision(),
        cs.revision() == ps.revision()
    ));
    out.push(format!(
        "created_on                  captured={:?} persisted={:?} equal={}",
        cs.created_on(),
        ps.created_on(),
        cs.created_on() == ps.created_on()
    ));
    out.push(format!(
        "digest                      captured={:?} persisted={:?} equal={}",
        cs.digest().to_hex(),
        ps.digest().to_hex(),
        cs.digest() == ps.digest()
    ));
    out.push(format!(
        "credentials.allowed_grants  captured={:?} persisted={:?} equal={}",
        cs.policy().credentials(),
        ps.policy().credentials(),
        cs.policy().credentials() == ps.policy().credentials()
    ));
    out.push(format!(
        "credential_grants (registry) captured={:?} persisted={:?} equal={}",
        captured.credential_grants(),
        persisted.credential_grants(),
        captured.credential_grants() == persisted.credential_grants()
    ));
    // FileSystemPolicy only exists in ReadOnly / WorkspaceWrite modes.
    out.push(format!(
        "filesystem (read-only/writable/denied/scratch) captured={:?} persisted={:?}",
        cs.policy().filesystem(),
        ps.policy().filesystem()
    ));
    out
}

#[tokio::test]
async fn durable_policy_mismatch_repro() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let sessions = tempfile::tempdir().expect("sessions");
    let (parent, parent_id, parent_path) = parent_session(artifacts.path(), sessions.path());
    let sidecar = child_root_for(&parent_path, &parent_id).join("orchestration-state.json");

    // Render 1: isolated Application with orchestration sidecar (fresh bind).
    let runtime1 = OrchestrationRuntime::new(
        config(artifacts.path(), vec![task_definition()]),
        quick_factory("unused"),
    )
    .expect("runtime1");
    let _application = application_with_orchestration(&parent, runtime1.clone())
        .await
        .expect("bind and recover render 1");
    assert!(sidecar.is_file(), "sidecar written: {}", sidecar.display());
    let persisted_state: pi_coding::DurableState =
        serde_json::from_slice(&std::fs::read(&sidecar).expect("read sidecar"))
            .expect("decode sidecar");
    // Render-1 sidecar == current-generation captured policy.
    let captured = persisted_state.durable_policy.clone();

    eprintln!("=== RENDER 2: alter persisted sidecar policy (`credentials` field) ===");
    // Semantically altered policy: extra credential grant with a recomputed
    // (valid) digest. danger-full-access has no fs roots/scratch; the semantic
    // differing field here is credentials.allowed_grants.
    let new_creds = CredentialPolicy::new(vec![
        CredentialGrantId::parse("github-extra".to_owned()).unwrap(),
    ])
    .unwrap();
    let altered = DurablePolicyState::new(
        PolicySnapshot::new(
            captured.snapshot().revision(),
            captured.snapshot().created_on(),
            SandboxPolicy::danger_full_access(new_creds).expect("alt policy"),
        )
        .expect("altered snapshot"),
        captured.credential_grants().clone(),
    )
    .expect("altered durable policy");
    let altered_state = pi_coding::DurableState {
        version: DURABLE_STATE_VERSION,
        parent_session_id: persisted_state.parent_session_id.clone(),
        parent_session_path: persisted_state.parent_session_path.clone(),
        group_id: persisted_state.group_id.clone(),
        durable_policy: altered,
        main_mailbox: persisted_state.main_mailbox.clone(),
        delivered_messages: persisted_state.delivered_messages.clone(),
        agents: persisted_state.agents.clone(),
        jobs: persisted_state.jobs.clone(),
    };
    std::fs::write(
        &sidecar,
        serde_json::to_vec(&altered_state).expect("serialize"),
    )
    .expect("overwrite sidecar");

    // Render 2: NEW runtime over the SAME parent, resume "with the current
    // generation" (still P_live) against the altered persisted policy.
    let runtime2 = OrchestrationRuntime::new(
        config(artifacts.path(), vec![task_definition()]),
        quick_factory("unused"),
    )
    .expect("runtime2");
    let err = runtime2
        .bind_and_recover(&parent)
        .expect_err("changed current-generation policy must fail closed");

    let persisted_now = serde_json::from_slice::<pi_coding::DurableState>(
        &std::fs::read(&sidecar).expect("reread sidecar"),
    )
    .expect("decode sidecar")
    .durable_policy;

    eprintln!("=== RESULT ===");
    eprintln!("resume error:                {err:#}");
    eprintln!("captured == persisted?       {}", captured == persisted_now);
    for line in field_diff(&captured, &persisted_now) {
        eprintln!("  {line}");
    }
    assert!(
        captured != persisted_now,
        "the repro requires a genuine policy inequality"
    );
    assert_eq!(
        err.to_string(),
        "durable orchestration policy does not match resumed generation",
        "exact equality-check bail at runtime.rs:2685"
    );
    assert!(matches!(
        pi_coding::DurableResumeError::find_in_chain(&err),
        Some(pi_coding::DurableResumeError::PolicyMismatch)
    ));
    eprintln!("REPRO OK: exact bail reproduced.");
    runtime1.shutdown().await;
    runtime2.shutdown().await;

    // === Sub-scenario 2: revision-only bump, semantic policy identical ===
    // A later generation revision with the same sandbox policy body is the
    // same authority and must recover. A credential change above stays a mismatch.
    eprintln!();
    eprintln!(
        "=== RENDER 3: bump ONLY `revision` (mode/roots/denied/scratch/credentials unchanged) ==="
    );
    let rev_only = DurablePolicyState::new(
        PolicySnapshot::new(
            PolicyRevision::new(captured.snapshot().revision().get() + 1).expect("rev bump"),
            captured.snapshot().created_on(),
            captured.snapshot().policy().clone(),
        )
        .expect("rev-only snapshot"),
        captured.credential_grants().clone(),
    )
    .expect("rev-only durable policy");
    let rev_only_state = pi_coding::DurableState {
        version: DURABLE_STATE_VERSION,
        parent_session_id: persisted_state.parent_session_id.clone(),
        parent_session_path: persisted_state.parent_session_path.clone(),
        group_id: persisted_state.group_id.clone(),
        durable_policy: rev_only,
        main_mailbox: persisted_state.main_mailbox.clone(),
        delivered_messages: persisted_state.delivered_messages.clone(),
        agents: persisted_state.agents.clone(),
        jobs: persisted_state.jobs.clone(),
    };
    std::fs::write(
        &sidecar,
        serde_json::to_vec(&rev_only_state).expect("serialize"),
    )
    .expect("overwrite sidecar (revision-only)");

    let runtime3 = OrchestrationRuntime::new(
        config(artifacts.path(), vec![task_definition()]),
        quick_factory("unused"),
    )
    .expect("runtime3");
    runtime3
        .bind_and_recover(&parent)
        .expect("revision-only bump recovers the same sandbox authority");
    let persisted_rev = serde_json::from_slice::<pi_coding::DurableState>(
        &std::fs::read(&sidecar).expect("reread sidecar"),
    )
    .expect("decode sidecar")
    .durable_policy;
    assert!(
        pi_coding::durable_policy_same_authority(&captured, &persisted_rev),
        "recovered sidecar must keep the sandbox policy body"
    );
    runtime3.shutdown().await;
}

async fn seed_workspace_write_sidecar(
    artifacts: &Path,
    sessions: &Path,
    workspace: &Path,
    agent_dir: &Path,
) -> (Session, PathBuf, PathBuf) {
    let (recorded_parent, parent_id, parent_path) = parent_session(artifacts, sessions);
    let parent = restarted_parent_session(&recorded_parent, workspace)
        .expect("reopen seed parent under workspace A");
    let sidecar = child_root_for(&parent_path, &parent_id).join("orchestration-state.json");
    let runtime = OrchestrationRuntime::new(
        config(artifacts, vec![task_definition()]),
        quick_factory("unused"),
    )
    .expect("workspace-write seed runtime");
    let application = application_with_settings(
        &parent,
        runtime,
        agent_dir.to_path_buf(),
        r#"{"sandbox":{"mode":"workspace-write"}}"#,
    )
    .await
    .expect("seed workspace-write durable sidecar");
    assert!(sidecar.is_file(), "workspace-write sidecar must exist");
    let scratch = application
        .durable_policy_state()
        .expect("workspace-write durable policy")
        .snapshot()
        .policy()
        .filesystem()
        .expect("workspace-write filesystem")
        .writable_roots()
        .iter()
        .find(|root| root.root().as_path().starts_with(sessions))
        .expect("revision scratch writable root")
        .root()
        .as_path()
        .to_path_buf();
    application.cleanup().await;
    (parent, sidecar, scratch)
}

#[tokio::test]
async fn durable_resume_rejects_changed_workspace_root() {
    let root = tempfile::tempdir().expect("root");
    let artifacts = root.path().join("artifacts");
    let sessions = root.path().join("sessions");
    let workspace_a = root.path().join("workspace-a");
    let workspace_b = root.path().join("workspace-b");
    let agent_dir = root.path().join("agent");
    for directory in [
        &artifacts,
        &sessions,
        &workspace_a,
        &workspace_b,
        &agent_dir,
    ] {
        std::fs::create_dir(directory).expect("create changed-workspace fixture directory");
    }
    let (parent, sidecar, scratch) =
        seed_workspace_write_sidecar(&artifacts, &sessions, &workspace_a, &agent_dir).await;
    std::fs::remove_dir_all(&scratch).expect("remove persisted scratch before resume");

    let resumed_parent =
        restarted_parent_session(&parent, &workspace_b).expect("restart under workspace B");
    let runtime = OrchestrationRuntime::new(
        config(&artifacts, vec![task_definition()]),
        quick_factory("unused"),
    )
    .expect("changed-workspace resume runtime");
    let error = match application_with_settings(
        &resumed_parent,
        runtime.clone(),
        agent_dir,
        r#"{"sandbox":{"mode":"workspace-write"}}"#,
    )
    .await
    {
        Ok(application) => {
            application.cleanup().await;
            panic!("changed workspace root must fail closed")
        }
        Err(error) => error,
    };
    let chain = error_chain(&error);
    assert!(
        chain.contains("snapshot.policy.filesystem"),
        "changed workspace error must name snapshot.policy.filesystem: {chain}"
    );
    assert!(matches!(
        pi_coding::DurableResumeError::find_in_chain(&error),
        Some(pi_coding::DurableResumeError::State(_))
    ));
    assert!(
        !runtime.is_durable(),
        "failed resume must not bind durable state"
    );
    assert!(
        runtime.jobs(None).is_empty(),
        "failed resume restores no jobs"
    );
    assert!(
        runtime.list("Main").is_empty(),
        "failed resume restores no agents"
    );
    assert!(
        sidecar.is_file(),
        "failed resume preserves the existing sidecar"
    );
    assert!(
        !scratch.exists(),
        "failed resume must not recreate persisted scratch"
    );
    assert!(
        !workspace_b.join("sandbox").exists(),
        "failed resume must not create scratch under workspace B"
    );
    runtime.shutdown().await;
}
