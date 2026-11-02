use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use pi_agent::ThinkingLevel;
use pi_ai::providers::{
    FauxProviderOptions, FauxProviderRegistration, FauxResponse, register_faux_provider,
};
use pi_ai::{ContentBlock, Model, StopReason, ToolCall};
use pi_coding::{
    AgentCatalog, AgentDefinition, AgentDefinitionSource, Application, ApplicationEvent,
    ApplicationRuntimeCandidate, ApplicationRuntimeFactory, ApplicationRuntimeFuture,
    ChildSessionOptionsSnapshot, OrchestrationConfig, OrchestrationRuntime, Session,
    SessionOptions, TodoItem, TodoPhase, TodoStatus, WorkflowCreateRequest, WorkflowIntegration,
    WorkflowStatus, WorkspaceRoots,
};
use tempfile::TempDir;

struct Fixture {
    sandbox: TempDir,
    repo: PathBuf,
    state: PathBuf,
    managed: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let sandbox = tempfile::tempdir().expect("sandbox");
        let repo = sandbox.path().join("source");
        fs::create_dir_all(&repo).expect("repo");
        git(&repo, &["init"]);
        git(&repo, &["config", "user.name", "Pi Test"]);
        git(&repo, &["config", "user.email", "pi@example.test"]);
        fs::write(repo.join("README.md"), "base\n").expect("base");
        git(&repo, &["add", "README.md"]);
        git(&repo, &["commit", "-m", "initial"]);
        let state = sandbox.path().join("state");
        let managed = sandbox.path().join("managed");
        Self {
            sandbox,
            repo,
            state,
            managed,
        }
    }
}
fn git(cwd: &Path, args: &[&str]) -> String {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let output = Command::new("git")
        .env_clear()
        .env("PATH", path)
        .env("HOME", cwd)
        .env("USERPROFILE", cwd)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", cwd.join("absent-global-git-config"))
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "Pi Test")
        .env("GIT_AUTHOR_EMAIL", "pi@example.test")
        .env("GIT_COMMITTER_NAME", "Pi Test")
        .env("GIT_COMMITTER_EMAIL", "pi@example.test")
        .env("LC_ALL", "C")
        .args([
            "-c",
            "color.ui=false",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("utf8")
        .trim()
        .to_owned()
}
fn commit_file(cwd: &Path, relative: &str, contents: &str, message: &str) -> String {
    fs::write(cwd.join(relative), contents).expect("write");
    git(cwd, &["add", "--", relative]);
    git(cwd, &["commit", "-m", message]);
    git(cwd, &["rev-parse", "HEAD"])
}
fn definition() -> AgentDefinition {
    AgentDefinition {
        name: "task".into(),
        description: "workflow worker".into(),
        system_prompt: "complete workflow Todo".into(),
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

/// Per-process isolated native session root for workflow test sessions, so
/// supervisor/parent `start_new_recording()` never writes into the real
/// `~/.pi/agent/sessions` tree (the Web sidebar catalog source). Shared
/// across tests; parallel-safe via `LazyLock`, isolated from HOME.
fn test_sessions_root() -> PathBuf {
    static ROOT: std::sync::LazyLock<tempfile::TempDir> =
        std::sync::LazyLock::new(|| tempfile::tempdir().expect("test sessions root"));
    ROOT.path().to_path_buf()
}

async fn attach_isolated_resources(session: &Session) -> Result<()> {
    let agent_dir = test_sessions_root()
        .join("agents")
        .join(uuid::Uuid::now_v7().to_string());
    fs::create_dir_all(&agent_dir)?;
    fs::write(
        agent_dir.join("settings.json"),
        r#"{"sandbox":{"mode":"danger-full-access"}}"#,
    )?;
    let mut options = pi_coding::ResourceManagerOptions::new(session.cwd());
    options.agent_dir = agent_dir;
    options.disable_extensions = true;
    options.disable_skills = true;
    options.disable_prompt_templates = true;
    options.disable_themes = true;
    options.disable_context_files = true;
    session
        .attach_resources(pi_coding::ResourceManager::new(options)?)
        .await
}

#[derive(Clone)]
struct TestFactory {
    snapshot: ChildSessionOptionsSnapshot,
    mailbox_capacity: usize,
}
impl TestFactory {
    fn new(snapshot: ChildSessionOptionsSnapshot) -> Self {
        Self {
            snapshot,
            mailbox_capacity: pi_coding::DEFAULT_MAILBOX_CAPACITY,
        }
    }

    fn with_mailbox_capacity(
        snapshot: ChildSessionOptionsSnapshot,
        mailbox_capacity: usize,
    ) -> Self {
        Self {
            snapshot,
            mailbox_capacity,
        }
    }
}
impl ApplicationRuntimeFactory for TestFactory {
    fn build_runtime_candidate(
        &self,
        _: PathBuf,
        _: SessionOptions,
        _: Option<pi_coding::PreparedSessionResume>,
    ) -> ApplicationRuntimeFuture {
        Box::pin(async { anyhow::bail!("unused") })
    }
    fn build_trusted_workflow_candidate(
        &self,
        cwd: pi_coding::workflow_worktree::TrustedWorkflowCwd,
        mut options: SessionOptions,
    ) -> ApplicationRuntimeFuture {
        let snapshot = self.snapshot.clone();
        let mailbox_capacity = self.mailbox_capacity;
        Box::pin(async move {
            options.cwd = cwd.path().to_path_buf();
            options.tools = Some(Vec::new());
            let workspace = WorkspaceRoots::new(cwd.path(), Vec::<PathBuf>::new())?;
            let session =
                Session::new_with_todo_additional_tools_filtered_discovery_workspace_and_uri(
                    options,
                    Vec::new(),
                    Default::default(),
                    pi_coding::ResourceDiscovery::Disabled,
                    workspace,
                    None,
                    true,
                )?;
            session.set_session_dir(test_sessions_root());
            session.start_new_recording()?;
            attach_isolated_resources(&session).await?;
            let artifacts = std::env::temp_dir().join(format!(
                "pi-workflow-test-artifacts-{}",
                uuid::Uuid::now_v7()
            ));
            let mut config =
                OrchestrationConfig::new(AgentCatalog::from_agents(vec![definition()]), artifacts);
            config.parent_model = snapshot.model.clone();
            config.max_concurrency = pi_coding::DEFAULT_MAX_CONCURRENCY;
            config.idle_ttl = None;
            config.mailbox_capacity = mailbox_capacity;
            let orchestration = OrchestrationRuntime::new(
                config,
                OrchestrationRuntime::child_factory_from_snapshot(snapshot),
            )?;
            Ok(ApplicationRuntimeCandidate::new(session).with_orchestration(orchestration))
        })
    }
}

/// Workflow child catalog that additionally bundles `researcher` and `writer`
/// (the bundled default `task` stays the fallback default), so the typed
/// agent-routing E2E can spawn the named researcher.
#[derive(Clone)]
struct TypedAgentTestFactory {
    snapshot: ChildSessionOptionsSnapshot,
}
impl ApplicationRuntimeFactory for TypedAgentTestFactory {
    fn build_runtime_candidate(
        &self,
        _: PathBuf,
        _: SessionOptions,
        _: Option<pi_coding::PreparedSessionResume>,
    ) -> ApplicationRuntimeFuture {
        Box::pin(async { anyhow::bail!("unused") })
    }
    fn build_trusted_workflow_candidate(
        &self,
        cwd: pi_coding::workflow_worktree::TrustedWorkflowCwd,
        mut options: SessionOptions,
    ) -> ApplicationRuntimeFuture {
        let snapshot = self.snapshot.clone();
        Box::pin(async move {
            options.cwd = cwd.path().to_path_buf();
            options.tools = Some(Vec::new());
            let workspace = WorkspaceRoots::new(cwd.path(), Vec::<PathBuf>::new())?;
            let session =
                Session::new_with_todo_additional_tools_filtered_discovery_workspace_and_uri(
                    options,
                    Vec::new(),
                    Default::default(),
                    pi_coding::ResourceDiscovery::Disabled,
                    workspace,
                    None,
                    true,
                )?;
            session.set_session_dir(test_sessions_root());
            session.start_new_recording()?;
            attach_isolated_resources(&session).await?;
            let artifacts = std::env::temp_dir().join(format!(
                "pi-workflow-test-artifacts-{}",
                uuid::Uuid::now_v7()
            ));
            let mut researcher = definition();
            researcher.name = "researcher".into();
            researcher.system_prompt = "RESEARCHER_PROMPT".into();
            let mut writer = definition();
            writer.name = "writer".into();
            writer.system_prompt = "WRITER_PROMPT".into();
            let mut config = OrchestrationConfig::new(
                AgentCatalog::from_agents(vec![definition(), researcher, writer]),
                artifacts,
            );
            config.parent_model = snapshot.model.clone();
            config.max_concurrency = pi_coding::DEFAULT_MAX_CONCURRENCY;
            config.idle_ttl = None;
            let orchestration = OrchestrationRuntime::new(
                config,
                OrchestrationRuntime::child_factory_from_snapshot(snapshot),
            )?;
            Ok(ApplicationRuntimeCandidate::new(session).with_orchestration(orchestration))
        })
    }
}
fn parent_session(
    repo: &Path,
    responses: Vec<FauxResponse>,
) -> (Session, FauxProviderRegistration) {
    let suffix = uuid::Uuid::now_v7().to_string();
    let model = Model {
        id: format!("workflow-{suffix}"),
        name: "Workflow Test".into(),
        api: format!("workflow-api-{suffix}"),
        provider: format!("workflow-provider-{suffix}"),
        ..Model::default()
    };
    let registration = register_faux_provider(FauxProviderOptions {
        api: model.api.clone(),
        provider: model.provider.clone(),
        models: vec![model.clone()],
        chunk_size: 32,
    });
    registration.set_responses(responses);
    let session = Session::new_with_additional_tools_filtered_and_discovery(
        SessionOptions {
            model,
            cwd: repo.to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: "faux".into(),
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
    .expect("session");
    session.set_session_dir(test_sessions_root());
    // The parent session must carry a live recorder: `Application::new_session`
    // (session cutover) requires the active runtime to have a recorded session
    // identity, and the workflow child factory already starts recordings for
    // its own sessions. Without this the rebind test's `new_session` panics
    // with "session recording is unavailable".
    session
        .start_new_recording()
        .expect("parent session recording");
    (session, registration)
}

async fn application_with_isolated_resources(session: Session) -> Application {
    attach_isolated_resources(&session)
        .await
        .expect("attach isolated resources");
    let application = Application::new(session).await;
    application.with_hermetic_repository_executor(
        pi_coding::process::HermeticRepositoryExecutor::default_executor(),
    );
    application
}
fn planning(objective: &str) -> FauxResponse {
    FauxResponse {
        content: vec![ContentBlock::ToolCall(ToolCall {
            id: "todo-init".into(),
            name: "todo".into(),
            arguments: serde_json::json!({"op":"init","items":[objective]}),
            thought_signature: None,
        })],
        stop_reason: StopReason::ToolUse,
        error_message: None,
    }
}

fn todo_done_call(content: &str) -> FauxResponse {
    FauxResponse {
        content: vec![ContentBlock::ToolCall(ToolCall {
            id: "todo-done".into(),
            name: "todo".into(),
            arguments: serde_json::json!({"op":"done","task":content}),
            thought_signature: None,
        })],
        stop_reason: StopReason::ToolUse,
        error_message: None,
    }
}

fn todo_view_call(id: &str) -> FauxResponse {
    FauxResponse {
        content: vec![ContentBlock::ToolCall(ToolCall {
            id: id.into(),
            name: "todo".into(),
            arguments: serde_json::json!({"op": "view"}),
            thought_signature: None,
        })],
        stop_reason: StopReason::ToolUse,
        error_message: None,
    }
}

/// Agent script that settles the supervisor-driven lifecycle with no git
/// commits: the supervisor builds the Todo DAG (`todo init`), which commits
/// the plan. The first successful `todo init` ends planning, arms DAG
/// execution, and spawns the ready task's worker. The DAG-armed worker then
/// marks its owned task Done (`todo done` by content). Auto-integrate then
/// refuses the empty worktree, so the durable record lands Failed with
/// integration None. Trailing text responses keep the worker session fed.
/// The supervisor only consumes the planning response; the worker consumes
/// the rest.
fn completing_responses(objective: &str) -> Vec<FauxResponse> {
    vec![
        planning(objective),
        todo_done_call(objective),
        FauxResponse::text("worker done"),
        FauxResponse::text("workflow complete"),
    ]
}
async fn app_with_mailbox_capacity(
    f: &Fixture,
    responses: Vec<FauxResponse>,
    mailbox_capacity: usize,
) -> (
    Application,
    Arc<pi_coding::ApplicationWorkflowRuntimeFactory>,
    FauxProviderRegistration,
) {
    let (session, registration) = parent_session(&f.repo, responses);
    let snapshot = session.child_session_options_snapshot();
    let application = application_with_isolated_resources(session).await;
    application
        .attach_runtime_factory(Arc::new(TestFactory::with_mailbox_capacity(
            snapshot,
            mailbox_capacity,
        )))
        .expect("factory");
    let factory = application
        .setup_workflows(&f.repo, &f.state, &f.managed)
        .await
        .expect("setup");
    (application, factory, registration)
}
async fn app(
    f: &Fixture,
    responses: Vec<FauxResponse>,
) -> (
    Application,
    Arc<pi_coding::ApplicationWorkflowRuntimeFactory>,
    FauxProviderRegistration,
) {
    app_with_mailbox_capacity(f, responses, pi_coding::DEFAULT_MAILBOX_CAPACITY).await
}
fn worktree(f: &Fixture, id: &str) -> PathBuf {
    f.managed
        .join(pi_coding::workflow_worktree::WORKTREE_ROOT_DIR_NAME)
        .join(id)
}
async fn wait_status(
    application: &Application,
    workflow_id: &pi_coding::WorkflowId,
    status: WorkflowStatus,
) -> pi_coding::WorkflowSnapshot {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = application.workflow_get(workflow_id).expect("workflow");
            if snapshot.status == status {
                return snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("workflow status timeout")
}

async fn spawn_workflow_message_target(orchestration: &OrchestrationRuntime, target: &str) {
    let spawn = orchestration
        .spawn_tasks(
            orchestration.main_agent_id(),
            0,
            vec![pi_coding::TaskItem {
                index: 0,
                id: target.to_owned(),
                agent: "task".to_owned(),
                assignment: "establish workflow message target".to_owned(),
                todo_task_id: None,
                ..Default::default()
            }],
        )
        .expect("spawn workflow message target");
    orchestration
        .wait_jobs(
            &[spawn[0].job_id.clone()],
            Some(Duration::from_secs(5)),
            None,
        )
        .await
        .expect("settle workflow message target");
}

#[tokio::test]
async fn workflow_hub_send_delivers_to_resident_runtime_with_correlated_receipt() {
    let fixture = Fixture::new();
    let (application, factory, _registration) = app(&fixture, Vec::new()).await;
    let created = application
        .workflow_create(WorkflowCreateRequest {
            name: "resident-send".into(),
            objective: "hold resident runtime".into(),
        })
        .await
        .expect("create workflow");
    let child = factory
        .child_application(&created.workflow_id, created.generation)
        .expect("resident workflow child");
    let orchestration = child.orchestration_runtime().expect("child orchestration");
    spawn_workflow_message_target(&orchestration, "InboxTarget").await;
    let receipts = application
        .workflow_hub_send(
            created.workflow_id.as_str(),
            created.generation,
            "InboxTarget",
            "status?",
            None,
        )
        .expect("resident workflow send");
    assert_eq!(receipts.len(), 1, "{receipts:?}");
    let receipt = &receipts[0];
    assert_eq!(receipt.to, "InboxTarget");
    assert_eq!(receipt.outcome, pi_coding::DeliveryOutcome::Revived);
    assert!(receipt.error.is_none(), "{receipts:?}");
    let message_id = receipt.message_id.as_deref().expect("message id");
    assert!(
        orchestration
            .delivered_messages()
            .iter()
            .any(|message| message.id == message_id && message.body == "status?"),
        "receipt must correlate to durable delivered history"
    );
    application.cleanup().await;
}

#[tokio::test]
async fn workflow_hub_send_delivery_failures_remain_receipts() {
    let fixture = Fixture::new();
    let (application, factory, registration) =
        app_with_mailbox_capacity(&fixture, Vec::new(), 1).await;
    let created = application
        .workflow_create(WorkflowCreateRequest {
            name: "receipt-failures".into(),
            objective: "hold resident runtime".into(),
        })
        .await
        .expect("create workflow");
    let child = factory
        .child_application(&created.workflow_id, created.generation)
        .expect("resident workflow child");
    let orchestration = child.orchestration_runtime().expect("child orchestration");

    let unknown = application
        .workflow_hub_send(
            created.workflow_id.as_str(),
            created.generation,
            "ghost-agent",
            "ping",
            None,
        )
        .expect("unknown recipient remains a receipt");
    assert_eq!(unknown[0].outcome, pi_coding::DeliveryOutcome::Failed);
    assert!(unknown[0].message_id.is_some(), "{unknown:?}");
    assert!(
        unknown[0]
            .error
            .as_deref()
            .is_some_and(|error| error.contains("unknown orchestration agent")),
        "{unknown:?}"
    );

    spawn_workflow_message_target(&orchestration, "InboxTarget").await;
    registration.unregister();
    let first = application
        .workflow_hub_send(
            created.workflow_id.as_str(),
            created.generation,
            "Main",
            "first",
            None,
        )
        .expect("fill mailbox");
    assert_eq!(first[0].outcome, pi_coding::DeliveryOutcome::Queued);
    assert!(first[0].error.is_none(), "{first:?}");
    let full = application
        .workflow_hub_send(
            created.workflow_id.as_str(),
            created.generation,
            "Main",
            "second",
            None,
        )
        .expect("full mailbox remains a receipt");
    assert_eq!(full[0].outcome, pi_coding::DeliveryOutcome::Failed);
    assert!(full[0].message_id.is_some(), "{full:?}");
    assert!(
        full[0]
            .error
            .as_deref()
            .is_some_and(|error| error.contains("full")),
        "{full:?}"
    );

    orchestration.inbox("Main", false).expect("drain mailbox");
    let (parent_id, parent_path) = child.session().recorder_info().expect("child recorder");
    let sidecar = parent_path
        .parent()
        .expect("child recorder parent")
        .join("children")
        .join(parent_id)
        .join("orchestration-state.json");
    std::fs::remove_file(&sidecar).expect("remove sidecar");
    std::fs::create_dir(&sidecar).expect("block sidecar writes");
    let persistence = application
        .workflow_hub_send(
            created.workflow_id.as_str(),
            created.generation,
            "InboxTarget",
            "persist-fail",
            None,
        )
        .expect("persistence failure remains a receipt");
    assert_eq!(persistence[0].outcome, pi_coding::DeliveryOutcome::Failed);
    assert!(persistence[0].message_id.is_some(), "{persistence:?}");
    assert!(persistence[0].error.is_some(), "{persistence:?}");
    application.cleanup().await;
}
async fn wait_integrated(
    application: &Application,
    workflow_id: &pi_coding::WorkflowId,
) -> pi_coding::WorkflowSnapshot {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = application.workflow_get(workflow_id).expect("workflow");
            if matches!(snapshot.integration, WorkflowIntegration::Applied { .. }) {
                return snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("workflow auto-integrate timeout")
}
fn completed_todo(objective: &str) -> Vec<TodoPhase> {
    vec![TodoPhase {
        name: "Build".into(),
        tasks: vec![TodoItem {
            id: "root".into(),
            content: objective.to_owned(),
            status: TodoStatus::Completed,
            depends_on: Vec::new(),
            ready: true,
            waiting_on: Vec::new(),
            agent: None,
        }],
    }]
}

#[tokio::test]
async fn create_isolates_child_runtime() {
    let f = Fixture::new();
    let objective = "delegate task to ship feature";
    let (application, _factory, _registration) = app(&f, completing_responses(objective)).await;
    let created = application
        .workflow_create(WorkflowCreateRequest {
            name: "ship".into(),
            objective: objective.into(),
        })
        .await
        .expect("create");
    let created = wait_status(
        &application,
        &created.workflow_id,
        WorkflowStatus::Failed,
    )
    .await;
    assert!(
        created.failure.as_ref().is_some_and(|failure| {
            failure.message.contains("no commits to integrate")
        }),
        "a finished workflow with nothing to merge stays unmerged: {created:?}"
    );
    let path = worktree(&f, created.workflow_id.as_str());
    assert!(path.exists() && !path.starts_with(&f.repo));
    assert!(
        created
            .branch
            .as_deref()
            .is_some_and(|b| b.starts_with("rpi/workflow/"))
    );
    assert_eq!(created.todo.phases[0].tasks[0].content, objective);
    application.cleanup().await;
}

#[tokio::test]
async fn later_todo_mutation_runs_and_keeps_workflow_ownership() {
    let f = Fixture::new();
    let (application, factory, _registration) = app(
        &f,
        vec![
            planning("plan before Todo mutation"),
            FauxResponse::text("planning acknowledged"),
        ],
    )
    .await;
    let created = application
        .workflow_create(WorkflowCreateRequest {
            name: "deferred-plan".into(),
            objective: "plan before Todo mutation".into(),
        })
        .await
        .expect("workflow creation must succeed");
    // The supervisor-driven plan builds the Todo DAG and the workflow runs.
    let created = wait_status(&application, &created.workflow_id, WorkflowStatus::Running).await;
    assert_eq!(
        created.todo.phases[0].tasks[0].content,
        "plan before Todo mutation"
    );

    let result = application
        .set_workflow_todos(
            &created.workflow_id,
            vec![TodoPhase {
                name: "Build".into(),
                tasks: vec![TodoItem {
                    id: "later-root".into(),
                    content: "execute later canonical Todo".into(),
                    status: TodoStatus::Pending,
                    depends_on: Vec::new(),
                    ready: true,
                    waiting_on: Vec::new(),
                    agent: None,
                }],
            }],
        )
        .expect("canonical workflow Todo mutation");
    assert_eq!(result.phases[0].tasks[0].id, "later-root");
    assert!(
        application.todo_state().phases.is_empty(),
        "parent Todo must remain unchanged"
    );
    let child = factory
        .child_application(&created.workflow_id, created.generation)
        .expect("exact workflow child application");
    let jobs = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let jobs = child
                .orchestration_runtime()
                .expect("child orchestration")
                .jobs(None);
            if jobs.iter().any(|job| {
                job.todo_task_id.as_deref() == Some("later-root")
                    && job.workflow_id.as_deref() == Some(created.workflow_id.as_str())
                    && job.workflow_generation == Some(created.generation)
            }) {
                break jobs;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("workflow Todo job projection timed out");
    assert!(
        jobs.iter()
            .any(|job| job.todo_task_id.as_deref() == Some("later-root")
                && job.workflow_id.as_deref() == Some(created.workflow_id.as_str())
                && job.workflow_generation == Some(created.generation)),
        "workflow Todo jobs must preserve exact ownership: {jobs:?}"
    );
    let running = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = application
                .workflow_get(&created.workflow_id)
                .expect("workflow snapshot");
            if snapshot
                .todo
                .phases
                .first()
                .and_then(|phase| phase.tasks.first())
                .is_some_and(|task| task.id == "later-root")
            {
                break snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("canonical Todo projection timed out");
    assert_eq!(running.todo.phases[0].tasks[0].id, "later-root");
    assert_eq!(running.status, WorkflowStatus::Running);
    application.cleanup().await;
}

#[tokio::test]
async fn workflow_todo_mutation_fails_safely_for_missing_workflow() {
    let f = Fixture::new();
    let (application, _factory, _registration) = app(&f, Vec::new()).await;
    let error = application
        .set_workflow_todos(&pi_coding::WorkflowId::new("missing"), Vec::new())
        .expect_err("missing workflow");
    assert_eq!(error.to_string(), "workflow was not found");
    assert!(application.todo_state().phases.is_empty());
    application.cleanup().await;
}

#[tokio::test]
async fn endless_tool_turns_after_commit_preserve_dag_and_run_within_bound() {
    // A provider that commits a plan and then emits endless tool turns must
    // not keep the workflow in Planning. The bounded planning run stops at the
    // turn budget, preserves the committed DAG, and arms its execution.
    let f = Fixture::new();
    let objective = "endless turns after commit";
    let mut responses = vec![planning(objective)];
    responses.extend((0..10).map(|index| todo_view_call(&format!("view-{index}"))));
    responses.push(FauxResponse::text("tail"));
    let (application, factory, _registration) = app(&f, responses).await;
    let created = application
        .workflow_create(WorkflowCreateRequest {
            name: "endless-after-commit".into(),
            objective: objective.into(),
        })
        .await
        .expect("create workflow");
    let running = wait_status(&application, &created.workflow_id, WorkflowStatus::Running).await;
    assert_eq!(running.todo.phases[0].tasks[0].content, objective);
    let task_id = running.todo.phases[0].tasks[0].id.clone();
    let child = factory
        .child_application(&created.workflow_id, created.generation)
        .expect("exact workflow child application");
    // The plan-commit moves the status to Running immediately, while DAG
    // execution is armed when the bounded planning run ends; wait for the
    // armed worker job to exist.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let jobs = child
                .orchestration_runtime()
                .expect("child orchestration")
                .jobs(None);
            if jobs.iter().any(|job| {
                job.todo_task_id.as_deref() == Some(task_id.as_str())
                    && job.workflow_id.as_deref() == Some(created.workflow_id.as_str())
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the preserved plan must arm Todo DAG execution");
    application.cleanup().await;
}

#[tokio::test]
async fn endless_tool_turns_without_commit_fail_at_planning_bound() {
    // A provider that never commits a plan is stopped by the planning turn
    // budget, and the empty-DAG workflow fails naming the bound instead of
    // sitting in Planning forever.
    let f = Fixture::new();
    let (application, _factory, _registration) = app(
        &f,
        (0..10)
            .map(|index| todo_view_call(&format!("view-{index}")))
            .collect(),
    )
    .await;
    let created = application
        .workflow_create(WorkflowCreateRequest {
            name: "endless-no-commit".into(),
            objective: "must fail at the planning bound".into(),
        })
        .await
        .expect("create workflow");
    let failed = wait_status(&application, &created.workflow_id, WorkflowStatus::Failed).await;
    assert!(failed.todo.phases.is_empty(), "no plan was committed");
    assert!(
        failed
            .failure
            .as_ref()
            .is_some_and(|failure| failure.message.contains("planning exceeded the bound")),
        "failure must name the tripped bound, got {:?}",
        failed.failure
    );
    application.cleanup().await;
}

#[tokio::test]
async fn provider_planning_error_surfaces_failed_status() {
    let f = Fixture::new();
    let secret = "upstream api_key=workflow-secret";
    let (application, _factory, _registration) = app(&f, vec![FauxResponse::error(secret)]).await;
    // With async supervisor start, workflow creation succeeds immediately
    // (Queued). The provider error surfaces later through the projection
    // sink as a Failed status update.
    let created = application
        .workflow_create(WorkflowCreateRequest {
            name: "provider-error".into(),
            objective: "must not survive provider failure".into(),
        })
        .await
        .expect("workflow creation should succeed with async start");
    let snapshot = wait_status(&application, &created.workflow_id, WorkflowStatus::Failed).await;
    assert!(!format!("{snapshot:?}").contains(secret));
    application.cleanup().await;
}

#[tokio::test]
async fn workflow_detail_is_generation_gated_and_terminal_safe() {
    let f = Fixture::new();
    let objective = "detail projection";
    let (application, _factory, _registration) = app(&f, completing_responses(objective)).await;
    let created = application
        .workflow_create(WorkflowCreateRequest {
            name: "detail".into(),
            objective: objective.into(),
        })
        .await
        .expect("create");
    let settled = wait_status(
        &application,
        &created.workflow_id,
        WorkflowStatus::Failed,
    )
    .await;
    assert!(
        settled.failure.as_ref().is_some_and(|failure| {
            failure.message.contains("no commits to integrate")
        }),
        "detail stays on the unmerged failure: {settled:?}"
    );
    let detail = application
        .workflow_detail(&settled.workflow_id, settled.generation)
        .expect("detail");
    assert_eq!(detail.workflow_id, settled.workflow_id);
    assert_eq!(detail.generation, settled.generation);
    assert_eq!(detail.status, WorkflowStatus::Failed);
    assert_eq!(detail.todo, settled.todo);
    let stale = application
        .workflow_detail(&settled.workflow_id, settled.generation + 1)
        .expect_err("stale generation");
    assert_eq!(stale.to_string(), "workflow generation is stale");
    let removed = application
        .workflow_remove(&settled.workflow_id, settled.generation)
        .await
        .expect("remove");
    assert_eq!(removed.workflow_id, settled.workflow_id);
    let missing = application
        .workflow_detail(&settled.workflow_id, settled.generation)
        .expect_err("removed workflow");
    assert_eq!(missing.to_string(), "workflow was not found");
    application.cleanup().await;
}

#[tokio::test]
async fn empty_planning_fails_with_actionable_message_and_detail() {

    let f = Fixture::new();
    // Two plain-text turns: the bounded re-prompt also produces no typed
    // Todo plan, so the workflow must fail with a stable stuck-planning
    // message that names the planner identity, never sitting in Planning
    // and never inventing tasks from prose.
    let (application, _factory, _registration) = app(
        &f,
        vec![
            FauxResponse::text("planning acknowledged"),
            FauxResponse::text("still nothing to plan"),
        ],
    )
    .await;
    let planner = application.session().model().expect("planner model");
    let created = application
        .workflow_create(WorkflowCreateRequest {
            name: "empty-planning".into(),
            objective: "must fail when planning produces no tasks".into(),
        })
        .await
        .expect("create workflow");
    let failed = wait_status(&application, &created.workflow_id, WorkflowStatus::Failed).await;
    assert_eq!(failed.planning_prompts_consumed, 2);
    assert!(failed.todo.phases.is_empty(), "no tasks were planned");
    let failure = failed
        .failure
        .as_ref()
        .map(|failure| failure.message.as_str())
        .expect("empty-plan failure");
    assert!(
        failure.contains("planning produced no tasks"),
        "failure must be actionable, got {failure:?}"
    );
    assert!(
        failure.contains("no typed Todo plan was committed"),
        "failure must name the missing typed plan, got {failure:?}"
    );
    assert!(
        failure.contains(&format!("model {} ({})", planner.id, planner.api)),
        "failure must stamp planner identity, got {failure:?}"
    );
    assert!(
        !failure.contains("must fail when planning produces no tasks"),
        "failure must not include user/objective text, got {failure:?}"
    );
    let detail = application
        .workflow_detail(&failed.workflow_id, failed.generation)
        .expect("failed workflow detail");
    assert_eq!(detail.status, WorkflowStatus::Failed);
    assert!(detail.todo.phases.is_empty());
    application.cleanup().await;
}

#[tokio::test]
async fn empty_first_planning_turn_repairs_on_typed_todo_init() {
    let f = Fixture::new();
    let objective = "repair with typed todo init";
    // First pass is prose-only; the single corrective turn commits a typed
    // todo init and the workflow must run that DAG instead of failing.
    let (application, _factory, _registration) = app(
        &f,
        vec![
            FauxResponse::text("planning acknowledged"),
            planning(objective),
            FauxResponse::text("planned"),
        ],
    )
    .await;
    let created = application
        .workflow_create(WorkflowCreateRequest {
            name: "repair-planning".into(),
            objective: objective.into(),
        })
        .await
        .expect("create workflow");
    let running = wait_status(&application, &created.workflow_id, WorkflowStatus::Running).await;
    assert_eq!(running.planning_prompts_consumed, 2);
    assert_eq!(
        running.todo.phases.iter().flat_map(|phase| &phase.tasks).count(),
        1,
        "the corrective turn must commit the typed plan, got {:?}",
        running.todo
    );
    application.cleanup().await;
}

#[tokio::test]
async fn restore_adopts_committed_head_and_projects_terminal_detail() {
    let f = Fixture::new();
    let (first, _factory, registration) = app(&f, completing_responses("restore task")).await;
    let created = first
        .workflow_create(WorkflowCreateRequest {
            name: "restore".into(),
            objective: "restore task".into(),
        })
        .await
        .expect("create");
    let created = wait_status(&first, &created.workflow_id, WorkflowStatus::Failed).await;
    let path = worktree(&f, created.workflow_id.as_str());
    let head = commit_file(&path, "restored.txt", "workflow\n", "progress");
    first.cleanup().await;
    drop(registration);

    let (second, _factory, _registration) = app(&f, Vec::new()).await;
    let restored = second.workflow_get(&created.workflow_id).expect("restore");
    assert_eq!(restored.status, WorkflowStatus::Failed);
    assert_eq!(restored.todo.phases, created.todo.phases);
    assert_eq!(git(&path, &["rev-parse", "HEAD"]), head);
    let detail = second
        .workflow_detail(&restored.workflow_id, restored.generation)
        .expect("terminal restored detail");
    assert_eq!(detail.status, WorkflowStatus::Failed);
    assert_eq!(detail.todo, restored.todo);
    assert!(
        detail.supervisor.is_none(),
        "durable fallback cannot fabricate a live supervisor"
    );
    assert!(
        detail.subagents.is_empty(),
        "durable fallback cannot fabricate live subagents"
    );
    assert!(
        detail.jobs.is_empty(),
        "durable fallback cannot fabricate live jobs"
    );
    assert!(
        detail.irc.is_empty(),
        "durable fallback cannot fabricate live IRC"
    );
    second.cleanup().await;
}

#[tokio::test]
async fn integrate_remove_and_conflict_retention() {
    let f = Fixture::new();
    let (application, _factory, _registration) = app(&f, completing_responses("integrate")).await;
    let created = application
        .workflow_create(WorkflowCreateRequest {
            name: "integrate".into(),
            objective: "integrate".into(),
        })
        .await
        .expect("create");
    let created = wait_status(&application, &created.workflow_id, WorkflowStatus::Failed).await;
    assert!(
        created.failure.as_ref().is_some_and(|failure| {
            failure.message.contains("no commits to integrate")
        }),
        "empty completion stays retryable: {created:?}"
    );
    let path = worktree(&f, created.workflow_id.as_str());
    commit_file(&path, "feature.txt", "integrated\n", "feature");
    let integrated = application
        .workflow_integrate(&created.workflow_id, created.generation)
        .await
        .expect("integrate");
    assert_eq!(integrated.status, WorkflowStatus::Completed);
    assert!(matches!(
        integrated.integration,
        WorkflowIntegration::Applied { .. }
    ));
    assert_eq!(
        fs::read_to_string(f.repo.join("feature.txt")).expect("feature"),
        "integrated\n"
    );
    application
        .workflow_remove(&created.workflow_id, integrated.generation)
        .await
        .expect("remove");
    assert!(!path.exists());
    application.cleanup().await;

    let f = Fixture::new();
    let (application, _factory, _registration) = app(&f, completing_responses("conflict")).await;
    let created = application
        .workflow_create(WorkflowCreateRequest {
            name: "conflict".into(),
            objective: "conflict".into(),
        })
        .await
        .expect("create");
    let created = wait_status(&application, &created.workflow_id, WorkflowStatus::Failed).await;
    let path = worktree(&f, created.workflow_id.as_str());
    commit_file(&path, "README.md", "workflow side\n", "workflow");
    commit_file(&f.repo, "README.md", "source side\n", "source");
    let conflicted = application
        .workflow_integrate(&created.workflow_id, created.generation)
        .await
        .expect("conflict");
    assert_eq!(conflicted.status, WorkflowStatus::Conflicted);
    assert!(
        matches!(&conflicted.integration, WorkflowIntegration::Conflicted { conflicts } if conflicts.as_slice() == [String::from("README.md")])
    );
    assert!(path.exists());
    application.cleanup().await;
}

#[tokio::test]
async fn todo_completion_auto_integrates_worktree_back_into_source() {
    let f = Fixture::new();
    let objective = "auto integrate after completion";
    let (application, _factory, _registration) =
        app(&f, vec![planning(objective), FauxResponse::text("planned")]).await;
    let created = application
        .workflow_create(WorkflowCreateRequest {
            name: "auto-integrate".into(),
            objective: objective.into(),
        })
        .await
        .expect("create");
    let running = wait_status(&application, &created.workflow_id, WorkflowStatus::Running).await;
    let path = worktree(&f, created.workflow_id.as_str());
    // The workflow worktree accumulates committed work before the DAG settles.
    let worktree_commit = commit_file(&path, "auto.txt", "auto integrated\n", "worktree work");
    // Mark the canonical Todo DAG complete (as the supervisor agent does with
    // the todo tool): the DAG settles into Completed and the manager
    // auto-integrates the worktree back without a manual `/workflow integrate`.
    application
        .set_workflow_todos(&running.workflow_id, completed_todo(objective))
        .expect("canonical workflow Todo completion");
    let integrated = wait_integrated(&application, &created.workflow_id).await;
    assert_eq!(integrated.status, WorkflowStatus::Completed);
    let result_commit = match &integrated.integration {
        WorkflowIntegration::Applied { result_commit } => result_commit.clone(),
        other => panic!("expected applied auto-integration, got {other:?}"),
    };
    // The worktree branch was merged back with a --no-ff merge commit: the
    // source repo HEAD is the merge result, its second parent is the
    // workflow's commit, and the merged file is present in the source tree.
    assert_eq!(git(&f.repo, &["rev-parse", "HEAD"]), result_commit);
    assert_eq!(git(&f.repo, &["rev-parse", "HEAD^2"]), worktree_commit);
    assert_eq!(
        fs::read_to_string(f.repo.join("auto.txt")).expect("merged file"),
        "auto integrated\n"
    );
    application.cleanup().await;
}

#[tokio::test]
async fn todo_completion_without_commits_fails_unmerged() {
    let f = Fixture::new();
    let objective = "missing commits stay unmerged";
    let (application, _factory, _registration) =
        app(&f, vec![planning(objective), FauxResponse::text("planned")]).await;
    let created = application
        .workflow_create(WorkflowCreateRequest {
            name: "missing-commits".into(),
            objective: objective.into(),
        })
        .await
        .expect("create");
    let running = wait_status(&application, &created.workflow_id, WorkflowStatus::Running).await;
    application
        .set_workflow_todos(&running.workflow_id, completed_todo(objective))
        .expect("canonical workflow Todo completion");
    let failed = wait_status(&application, &created.workflow_id, WorkflowStatus::Failed).await;
    assert_eq!(
        failed.integration,
        WorkflowIntegration::None,
        "zero new commits must not persist Completed+None"
    );
    assert_eq!(
        failed
            .failure
            .as_ref()
            .map(|failure| failure.message.as_str()),
        Some(
            "workflow has no commits to integrate; commit the worktree changes before integration"
        )
    );
    application.cleanup().await;
}

#[tokio::test]
async fn todo_completion_conflict_lands_conflicted_not_stuck() {
    let f = Fixture::new();
    let objective = "conflict auto integrate";
    let (application, _factory, _registration) =
        app(&f, vec![planning(objective), FauxResponse::text("planned")]).await;
    let created = application
        .workflow_create(WorkflowCreateRequest {
            name: "auto-conflict".into(),
            objective: objective.into(),
        })
        .await
        .expect("create");
    let running = wait_status(&application, &created.workflow_id, WorkflowStatus::Running).await;
    let path = worktree(&f, created.workflow_id.as_str());
    // Both sides move the same file before the DAG settles.
    commit_file(&path, "README.md", "workflow side\n", "workflow");
    commit_file(&f.repo, "README.md", "source side\n", "source");
    application
        .set_workflow_todos(&running.workflow_id, completed_todo(objective))
        .expect("canonical workflow Todo completion");
    let conflicted = wait_status(
        &application,
        &created.workflow_id,
        WorkflowStatus::Conflicted,
    )
    .await;
    assert!(
        matches!(&conflicted.integration, WorkflowIntegration::Conflicted { conflicts } if conflicts.as_slice() == [String::from("README.md")])
    );
    assert!(
        path.exists(),
        "a merge conflict must preserve the worktree for manual resolution"
    );
    assert_eq!(
        fs::read_to_string(f.repo.join("README.md")).expect("source file"),
        "source side\n",
        "the aborted merge must not clobber the source tree"
    );
    application.cleanup().await;
}

#[tokio::test]
async fn paused_workflow_does_not_auto_integrate_until_resume() {
    let f = Fixture::new();
    let objective = "paused no auto integrate";
    let (application, _factory, _registration) =
        app(&f, vec![planning(objective), FauxResponse::text("planned")]).await;
    let created = application
        .workflow_create(WorkflowCreateRequest {
            name: "paused-integrate".into(),
            objective: objective.into(),
        })
        .await
        .expect("create");
    let running = wait_status(&application, &created.workflow_id, WorkflowStatus::Running).await;
    let paused = application
        .workflow_pause(&created.workflow_id, created.generation)
        .await
        .expect("pause");
    assert_eq!(paused.status, WorkflowStatus::Paused);
    let path = worktree(&f, created.workflow_id.as_str());
    commit_file(&path, "paused.txt", "paused work\n", "paused work");
    // The DAG settles while paused; the paused runtime never projects a
    // status change, so the merge must NOT run.
    application
        .set_workflow_todos(&running.workflow_id, completed_todo(objective))
        .expect("Todo completion while paused");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let snapshot = application
        .workflow_get(&created.workflow_id)
        .expect("workflow");
    assert_eq!(snapshot.status, WorkflowStatus::Paused);
    assert_eq!(snapshot.integration, WorkflowIntegration::None);
    assert!(
        !f.repo.join("paused.txt").exists(),
        "a paused workflow must not merge worktree changes"
    );
    // Resume settles the DAG and auto-integrates the accumulated work.
    let resumed = application
        .workflow_resume(&created.workflow_id, created.generation)
        .await
        .expect("resume");
    assert_eq!(resumed.status, WorkflowStatus::Completed);
    let integrated = wait_integrated(&application, &created.workflow_id).await;
    assert_eq!(integrated.status, WorkflowStatus::Completed);
    assert!(matches!(
        integrated.integration,
        WorkflowIntegration::Applied { .. }
    ));
    assert_eq!(
        fs::read_to_string(f.repo.join("paused.txt")).expect("merged after resume"),
        "paused work\n"
    );
    application.cleanup().await;
}

#[tokio::test]
async fn cleanup_preserves_paused_workflow_for_restore() {
    let f = Fixture::new();
    let (first, _factory, registration) = tokio::time::timeout(
        Duration::from_secs(10),
        app(
            &f,
            vec![
                planning("resume after process exit"),
                FauxResponse::text("planned"),
            ],
        ),
    )
    .await
    .expect("initial workflow application setup timed out");
    let created = tokio::time::timeout(
        Duration::from_secs(10),
        first.workflow_create(WorkflowCreateRequest {
            name: "paused-restore".into(),
            objective: "resume after process exit".into(),
        }),
    )
    .await
    .expect("workflow create timed out")
    .expect("create workflow");
    // Wait for the supervisor-driven planning turn to build the Todo DAG and
    // reach Running before pausing, so the paused record carries tasks.
    let running = tokio::time::timeout(
        Duration::from_secs(10),
        wait_status(&first, &created.workflow_id, WorkflowStatus::Running),
    )
    .await
    .expect("workflow running timed out");
    assert_eq!(
        running.todo.phases[0].tasks[0].content,
        "resume after process exit"
    );
    let paused = tokio::time::timeout(
        Duration::from_secs(10),
        first.workflow_pause(&created.workflow_id, created.generation),
    )
    .await
    .expect("workflow pause timed out")
    .expect("pause workflow");
    assert_eq!(paused.status, WorkflowStatus::Paused);
    assert_eq!(
        paused.todo.phases[0].tasks[0].content,
        "resume after process exit"
    );
    tokio::time::timeout(Duration::from_secs(10), first.cleanup())
        .await
        .expect("initial workflow application cleanup timed out");
    drop(registration);

    let (second, _factory, _registration) =
        tokio::time::timeout(Duration::from_secs(10), app(&f, Vec::new()))
            .await
            .expect("restored workflow application setup timed out");
    assert_eq!(
        second
            .workflow_get(&created.workflow_id)
            .expect("restored workflow")
            .status,
        WorkflowStatus::Paused
    );
    tokio::time::timeout(Duration::from_secs(10), second.cleanup())
        .await
        .expect("restored workflow application cleanup timed out");
}

/// Cleanup regression: `Application::cleanup` must abort (and await) the
/// workflow manager event forwarder exactly once. The forwarder spawned by
/// `attach_workflow_manager` holds a strong `WorkflowManager` reference and
/// publishes every manager event as `ApplicationEvent::Workflow`, so a live
/// forwarder after cleanup would both leak workflow events into the
/// application broadcast and keep the manager (and its store/registry) alive
/// indefinitely.
///
/// The manager event channel closing is the observable proof: once cleanup
/// takes the application's manager slot, the only surviving strong reference
/// can be the forwarder's — so a channel that stays open after cleanup means
/// the forwarder never stopped and can still forward events at any later
/// time.
#[tokio::test]
async fn cleanup_aborts_workflow_event_forwarder_and_releases_manager() {
    let f = Fixture::new();
    let (application, _factory, _registration) = app(&f, Vec::new()).await;

    let manager = application.workflow_manager().expect("manager");
    let mut manager_events = manager.subscribe();
    // The manager is alive before cleanup: its event channel is open (an
    // empty, open channel reports `Empty`; a dropped manager reports
    // `Closed`).
    assert!(
        matches!(
            manager_events.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ),
        "workflow manager event channel must be open before cleanup"
    );
    // Release the test's own strong reference; after cleanup the manager
    // must be held by no one.
    drop(manager);

    let mut app_events = application.subscribe();
    // Drain the application broadcast to quiescence so buffered workflow
    // events from setup cannot be mistaken for post-cleanup leaks.
    let _ = tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            match app_events.recv().await {
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    })
    .await;

    application.cleanup().await;
    // Cleanup is idempotent and must never abort the forwarder twice.
    application.cleanup().await;

    // The manager event channel must close: cleanup aborted/awaited the
    // forwarder (the only other strong reference), so the manager is
    // dropped. Without the fix the forwarder keeps the manager alive and
    // this times out.
    let closed = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match manager_events.recv().await {
                Ok(_) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    })
    .await;
    assert!(
        closed.is_ok(),
        "workflow manager event channel stayed open after cleanup: the workflow event forwarder leaked the manager"
    );

    // No `ApplicationEvent::Workflow` may be published after cleanup returns
    // (cleanup awaits the forwarder, so anything emitted during teardown was
    // already drained).
    let quiet = tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            match app_events.recv().await {
                Ok(ApplicationEvent::Workflow(_)) => return Err(()),
                Ok(_) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
            }
        }
    })
    .await;
    assert!(
        matches!(quiet, Err(_) | Ok(Ok(()))),
        "workflow event leaked into the application broadcast after cleanup"
    );
}

#[tokio::test]
async fn real_runtime_resume_cancel_and_remove_lifecycle_is_bounded() {
    let f = Fixture::new();
    let (application, _factory, _registration) = tokio::time::timeout(
        Duration::from_secs(10),
        app(
            &f,
            vec![
                planning("exercise real public lifecycle"),
                FauxResponse::text("planned"),
            ],
        ),
    )
    .await
    .expect("workflow application setup timed out");
    let created = tokio::time::timeout(
        Duration::from_secs(10),
        application.workflow_create(WorkflowCreateRequest {
            name: "lifecycle".into(),
            objective: "exercise real public lifecycle".into(),
        }),
    )
    .await
    .expect("workflow create timed out")
    .expect("create workflow");
    let created = wait_status(&application, &created.workflow_id, WorkflowStatus::Running).await;
    assert_eq!(
        created.todo.phases[0].tasks[0].content,
        "exercise real public lifecycle"
    );
    assert_eq!(application.workflow_list().len(), 1);
    assert_eq!(
        application
            .workflow_get(&created.workflow_id)
            .expect("show workflow")
            .name,
        "lifecycle"
    );
    let path = worktree(&f, created.workflow_id.as_str());
    assert!(path.exists());

    let paused = tokio::time::timeout(
        Duration::from_secs(10),
        application.workflow_pause(&created.workflow_id, created.generation),
    )
    .await
    .expect("workflow pause timed out")
    .expect("pause workflow");
    assert_eq!(paused.status, WorkflowStatus::Paused);

    let resumed = tokio::time::timeout(
        Duration::from_secs(10),
        application.workflow_resume(&created.workflow_id, created.generation),
    )
    .await
    .expect("workflow resume timed out")
    .expect("resume workflow");
    assert_eq!(resumed.status, WorkflowStatus::Running);
    assert_eq!(
        resumed.todo.phases[0].tasks[0].content,
        "exercise real public lifecycle"
    );

    let cancelled = tokio::time::timeout(
        Duration::from_secs(10),
        application.workflow_cancel(&created.workflow_id, created.generation),
    )
    .await
    .expect("workflow cancel timed out")
    .expect("cancel workflow");
    assert_eq!(cancelled.status, WorkflowStatus::Cancelled);
    assert!(
        path.exists(),
        "cancel preserves the owned worktree until explicit remove"
    );

    let removed = tokio::time::timeout(
        Duration::from_secs(10),
        application.workflow_remove(&created.workflow_id, created.generation),
    )
    .await
    .expect("workflow remove timed out")
    .expect("remove workflow");
    assert_eq!(removed.workflow_id, created.workflow_id);
    assert!(application.workflow_list().is_empty());
    assert!(!path.exists());
    tokio::time::timeout(Duration::from_secs(10), application.cleanup())
        .await
        .expect("workflow application cleanup timed out");
}

#[tokio::test]
async fn workflow_repository_namespace_is_stable_and_repo_scoped() {
    let first = Fixture::new();
    let second = Fixture::new();
    let (first_session, first_registration) = parent_session(&first.repo, Vec::new());
    let first_application = application_with_isolated_resources(first_session).await;
    let (second_session, second_registration) = parent_session(&second.repo, Vec::new());
    let second_application = application_with_isolated_resources(second_session).await;
    let first_context = first_application
        .repository_process_context(&first.repo, &first.managed)
        .expect("first repository context");
    let second_context = second_application
        .repository_process_context(&second.repo, &second.managed)
        .expect("second repository context");
    let first_namespace =
        pi_coding::workflow_worktree::repository_namespace(&first_context, &first.repo)
            .expect("first namespace");
    assert_eq!(
        first_namespace,
        pi_coding::workflow_worktree::repository_namespace(&first_context, &first.repo)
            .expect("stable namespace")
    );
    assert_eq!(first_namespace.len(), 32);
    assert!(first_namespace.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert_ne!(
        first_namespace,
        pi_coding::workflow_worktree::repository_namespace(&second_context, &second.repo)
            .expect("second namespace")
    );
    first_application.cleanup().await;
    second_application.cleanup().await;
    drop(first_registration);
    drop(second_registration);
}

#[tokio::test]
async fn session_namespace_is_stable_session_scoped_and_repo_scoped() {
    let first = Fixture::new();
    let second = Fixture::new();
    let (first_session, first_registration) = parent_session(&first.repo, Vec::new());
    let first_application = application_with_isolated_resources(first_session).await;
    let (second_session, second_registration) = parent_session(&second.repo, Vec::new());
    let second_application = application_with_isolated_resources(second_session).await;
    let first_context = first_application
        .repository_process_context(&first.repo, &first.managed)
        .expect("first repository context");
    let second_context = second_application
        .repository_process_context(&second.repo, &second.managed)
        .expect("second repository context");
    let namespace = |context: &pi_coding::GovernedProcessContext, repo: &Path, session| {
        pi_coding::workflow_worktree::session_namespace(Some(context), repo, session)
    };
    let session_a = namespace(&first_context, &first.repo, "session-a");
    assert_eq!(
        session_a,
        namespace(&first_context, &first.repo, "session-a")
    );
    assert_ne!(
        session_a,
        namespace(&first_context, &first.repo, "session-b")
    );
    assert_ne!(
        session_a,
        namespace(&second_context, &second.repo, "session-a")
    );
    let repo_namespace =
        pi_coding::workflow_worktree::repository_namespace(&first_context, &first.repo)
            .expect("repo namespace");
    assert!(session_a.starts_with(&format!("{repo_namespace}/")));
    let unsafe_ns = namespace(&first_context, &first.repo, "a/b\\c:d");
    let disambiguator = unsafe_ns
        .strip_prefix(&format!("{repo_namespace}/a-b-c-d-"))
        .expect("unsafe session id must map to the separator form plus a digest");
    assert_eq!(disambiguator.len(), 6, "digest length: {unsafe_ns}");
    assert!(
        disambiguator.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "digest must be hex: {unsafe_ns}"
    );
    assert_eq!(
        namespace(&first_context, &first.repo, "a-b-c-d"),
        format!("{repo_namespace}/a-b-c-d")
    );
    assert_ne!(unsafe_ns, namespace(&first_context, &first.repo, "a-b-c-d"));
    first_application.cleanup().await;
    second_application.cleanup().await;
    drop(first_registration);
    drop(second_registration);
}

/// Foreign cwd: a repository helper invocation whose cwd is outside the
/// retained source/managed authority is rejected by the governed bridge
/// (`prepare_repository_spawn`) before any executor runs.
#[tokio::test]
async fn repository_helper_rejects_foreign_cwd_before_execution() {
    let f = Fixture::new();
    let (session, registration) = parent_session(&f.repo, Vec::new());
    let application = application_with_isolated_resources(session).await;
    let context = application
        .repository_process_context(&f.repo, &f.managed)
        .expect("repository context");
    let foreign = tempfile::tempdir().expect("foreign cwd");
    let error = pi_coding::workflow_worktree::repository_namespace(&context, foreign.path())
        .expect_err("foreign cwd must be rejected before execution");
    assert!(
        error.to_string().contains("outside retained authority"),
        "foreign cwd must fail at the bridge, not execution: {error}"
    );
    application.cleanup().await;
    drop(registration);
}

/// Stale cwd: a repository helper invocation whose cwd is under the retained
/// source authority but no longer exists is rejected by the governed bridge
/// (retained working directory lease) before any executor runs.
#[tokio::test]
async fn repository_helper_rejects_stale_cwd_before_execution() {
    let f = Fixture::new();
    let (session, registration) = parent_session(&f.repo, Vec::new());
    let application = application_with_isolated_resources(session).await;
    let context = application
        .repository_process_context(&f.repo, &f.managed)
        .expect("repository context");
    let stale = f.repo.join("removed-subdir");
    fs::create_dir_all(&stale).expect("create stale");
    fs::remove_dir_all(&stale).expect("remove stale");
    let error = pi_coding::workflow_worktree::repository_namespace(&context, &stale)
        .expect_err("stale cwd must be rejected before execution");
    let message = error.to_string();
    // The bridge rejects at the retained working directory lease (a capability
    // failure), never reaching git execution: the message names the capability
    // failure, not a git command failure.
    assert!(
        !message.contains("git") && !message.contains("CommandFailed"),
        "stale cwd must fail at the bridge, not git execution: {message}"
    );
    application.cleanup().await;
    drop(registration);
}

/// Valid bridge: the repository-authorized context executes a governed git
/// helper against the retained source authority and returns a stable,
/// repo-scoped namespace (the positive counterpart to the rejection tests).
#[tokio::test]
async fn repository_helper_valid_bridge_executes_against_retained_authority() {
    let f = Fixture::new();
    let (session, registration) = parent_session(&f.repo, Vec::new());
    let application = application_with_isolated_resources(session).await;
    let context = application
        .repository_process_context(&f.repo, &f.managed)
        .expect("repository context");
    let namespace = pi_coding::workflow_worktree::repository_namespace(&context, &f.repo)
        .expect("valid bridge executes");
    assert_eq!(namespace.len(), 32);
    assert!(namespace.bytes().all(|byte| byte.is_ascii_hexdigit()));
    application.cleanup().await;
    drop(registration);
}

#[tokio::test]
async fn shared_agent_root_keeps_session_workflows_isolated() {
    let f = Fixture::new();
    let shared = tempfile::tempdir().expect("shared agent root");
    let session_a_namespace =
        pi_coding::workflow_worktree::session_namespace(None, &f.repo, "session-a");
    let session_b_namespace =
        pi_coding::workflow_worktree::session_namespace(None, &f.repo, "session-b");
    assert_ne!(session_a_namespace, session_b_namespace);

    // Session A creates a workflow in its own namespace.
    let (session_a, session_a_registration) = parent_session(
        &f.repo,
        vec![
            planning("remain session isolated"),
            FauxResponse::text("planned"),
        ],
    );
    let snapshot_a = session_a.child_session_options_snapshot();
    let application_a = application_with_isolated_resources(session_a).await;
    application_a
        .attach_runtime_factory(Arc::new(TestFactory::new(snapshot_a)))
        .expect("session A factory");
    application_a
        .setup_workflows(
            &f.repo,
            shared.path().join("workflows").join(&session_a_namespace),
            shared.path().join("worktrees").join(&session_a_namespace),
        )
        .await
        .expect("session A setup");
    let created = application_a
        .workflow_create(WorkflowCreateRequest {
            name: "session-a".into(),
            objective: "remain session isolated".into(),
        })
        .await
        .expect("create workflow in session A");
    let running = wait_status(
        &application_a,
        &created.workflow_id,
        WorkflowStatus::Running,
    )
    .await;
    assert_eq!(
        running.todo.phases[0].tasks[0].content,
        "remain session isolated"
    );
    application_a.cleanup().await;
    drop(session_a_registration);

    // Session B (different id, same repo) sees an EMPTY workflow list.
    let (session_b, session_b_registration) = parent_session(&f.repo, Vec::new());
    let snapshot_b = session_b.child_session_options_snapshot();
    let application_b = application_with_isolated_resources(session_b).await;
    application_b
        .attach_runtime_factory(Arc::new(TestFactory::new(snapshot_b)))
        .expect("session B factory");
    application_b
        .setup_workflows(
            &f.repo,
            shared.path().join("workflows").join(&session_b_namespace),
            shared.path().join("worktrees").join(&session_b_namespace),
        )
        .await
        .expect("session B setup");
    assert!(application_b.workflow_list().is_empty());
    application_b.cleanup().await;
    drop(session_b_registration);

    // Resumed session A (same id) restores its workflow, still Running.
    let (session_a_again, session_a_again_registration) = parent_session(&f.repo, Vec::new());
    let snapshot_a_again = session_a_again.child_session_options_snapshot();
    let application_a_again = application_with_isolated_resources(session_a_again).await;
    application_a_again
        .attach_runtime_factory(Arc::new(TestFactory::new(snapshot_a_again)))
        .expect("session A resume factory");
    application_a_again
        .setup_workflows(
            &f.repo,
            shared.path().join("workflows").join(&session_a_namespace),
            shared.path().join("worktrees").join(&session_a_namespace),
        )
        .await
        .expect("session A resume setup");
    let restored = application_a_again
        .workflow_get(&created.workflow_id)
        .expect("session A workflow restored");
    assert_eq!(restored.status, WorkflowStatus::Running);
    assert_eq!(
        restored.todo.phases[0].tasks[0].content,
        "remain session isolated"
    );
    application_a_again.cleanup().await;
    drop(session_a_again_registration);
}

#[tokio::test]
async fn rebind_to_new_session_isolates_and_rebind_to_same_id_restores() {
    let f = Fixture::new();
    let shared = tempfile::tempdir().expect("shared agent root");
    let session_a_namespace =
        pi_coding::workflow_worktree::session_namespace(None, &f.repo, "session-a");
    let session_b_namespace =
        pi_coding::workflow_worktree::session_namespace(None, &f.repo, "session-b");
    assert_ne!(session_a_namespace, session_b_namespace);
    let roots_a = (
        shared.path().join("workflows").join(&session_a_namespace),
        shared.path().join("worktrees").join(&session_a_namespace),
    );
    let roots_b = (
        shared.path().join("workflows").join(&session_b_namespace),
        shared.path().join("worktrees").join(&session_b_namespace),
    );

    // Session A creates a workflow in its own namespace.
    let (session, registration) = parent_session(
        &f.repo,
        vec![
            planning("rebind session isolation"),
            FauxResponse::text("planned"),
        ],
    );
    let snapshot = session.child_session_options_snapshot();
    let application = application_with_isolated_resources(session).await;
    application
        .attach_runtime_factory(Arc::new(TestFactory::new(snapshot)))
        .expect("factory");
    application
        .setup_workflows(&f.repo, &roots_a.0, &roots_a.1)
        .await
        .expect("session A setup");
    let created = application
        .workflow_create(WorkflowCreateRequest {
            name: "rebind-a".into(),
            objective: "rebind session isolation".into(),
        })
        .await
        .expect("create workflow in session A");
    let running = wait_status(&application, &created.workflow_id, WorkflowStatus::Running).await;
    assert_eq!(
        running.todo.phases[0].tasks[0].content,
        "rebind session isolation"
    );

    // A fresh session rebinds to its own roots and must start with an EMPTY
    // workflow list — the old session's workflows are invisible (T43
    // session-scoped storage). The CLI derives the roots from the new
    // recorder id via `workflow_storage_roots`; here the rebind receives the
    // session-B roots directly.
    let outcome = application.new_session().await.expect("new session");
    assert!(!outcome.cancelled);
    application
        .rebind_workflows(&roots_b.0, &roots_b.1)
        .await
        .expect("rebind to the new session roots");
    assert!(
        application.workflow_list().is_empty(),
        "a fresh session must not see the old session's workflows"
    );

    // Resuming the SAME session id rebinds back to its roots and restores
    // its own workflows.
    application
        .rebind_workflows(&roots_a.0, &roots_a.1)
        .await
        .expect("rebind back to session A roots");
    let restored = application
        .workflow_get(&created.workflow_id)
        .expect("session A workflow restored after rebind");
    assert_eq!(restored.status, WorkflowStatus::Running);
    assert_eq!(
        restored.todo.phases[0].tasks[0].content,
        "rebind session isolation"
    );
    application.cleanup().await;
    drop(registration);
}

#[tokio::test]
async fn shared_agent_root_keeps_repository_workflows_isolated() {
    let first = Fixture::new();
    let second = Fixture::new();
    let shared = tempfile::tempdir().expect("shared agent root");
    let first_namespace =
        pi_coding::workflow_worktree::session_namespace(None, &first.repo, "repository");
    let second_namespace =
        pi_coding::workflow_worktree::session_namespace(None, &second.repo, "repository");

    let (first_session, first_registration) = parent_session(
        &first.repo,
        vec![planning("remain isolated"), FauxResponse::text("planned")],
    );
    let first_snapshot = first_session.child_session_options_snapshot();
    let first_application = application_with_isolated_resources(first_session).await;
    first_application
        .attach_runtime_factory(Arc::new(TestFactory::new(first_snapshot)))
        .expect("first factory");
    first_application
        .setup_workflows(
            &first.repo,
            shared.path().join("workflows").join(&first_namespace),
            shared.path().join("worktrees").join(&first_namespace),
        )
        .await
        .expect("first setup");
    let created = first_application
        .workflow_create(WorkflowCreateRequest {
            name: "first-repo".into(),
            objective: "remain isolated".into(),
        })
        .await
        .expect("create first workflow");
    let running = wait_status(
        &first_application,
        &created.workflow_id,
        WorkflowStatus::Running,
    )
    .await;
    assert_eq!(running.todo.phases[0].tasks[0].content, "remain isolated");
    first_application.cleanup().await;
    drop(first_registration);

    let (second_session, second_registration) = parent_session(&second.repo, Vec::new());
    let second_snapshot = second_session.child_session_options_snapshot();
    let second_application = application_with_isolated_resources(second_session).await;
    second_application
        .attach_runtime_factory(Arc::new(TestFactory::new(second_snapshot)))
        .expect("second factory");
    second_application
        .setup_workflows(
            &second.repo,
            shared.path().join("workflows").join(&second_namespace),
            shared.path().join("worktrees").join(&second_namespace),
        )
        .await
        .expect("second setup");
    assert!(second_application.workflow_list().is_empty());
    second_application.cleanup().await;
    drop(second_registration);

    let (restored_session, restored_registration) = parent_session(&first.repo, Vec::new());
    let restored_snapshot = restored_session.child_session_options_snapshot();
    let restored_application = application_with_isolated_resources(restored_session).await;
    restored_application
        .attach_runtime_factory(Arc::new(TestFactory::new(restored_snapshot)))
        .expect("restore factory");
    restored_application
        .setup_workflows(
            &first.repo,
            shared.path().join("workflows").join(&first_namespace),
            shared.path().join("worktrees").join(&first_namespace),
        )
        .await
        .expect("restore setup");
    let restored = restored_application
        .workflow_get(&created.workflow_id)
        .expect("first workflow restored");
    // restore_all auto-continues the restored Running runtime: it must not
    // come back frozen.
    assert_eq!(restored.status, WorkflowStatus::Running);
    assert_eq!(restored.todo.phases[0].tasks[0].content, "remain isolated");
    restored_application.cleanup().await;
    drop(restored_registration);
}

#[tokio::test]
async fn non_git_setup_lists_and_create_fails_safely() -> Result<()> {
    let sandbox = tempfile::tempdir()?;
    let source = sandbox.path().join("plain");
    fs::create_dir(&source)?;
    let (session, registration) = parent_session(&source, Vec::new());
    let snapshot = session.child_session_options_snapshot();
    let application = application_with_isolated_resources(session).await;
    application.attach_runtime_factory(Arc::new(TestFactory::new(snapshot)))?;
    application
        .setup_workflows(
            &source,
            sandbox.path().join("state"),
            sandbox.path().join("managed"),
        )
        .await?;
    assert!(application.workflow_list().is_empty());
    let error = application
        .workflow_create(WorkflowCreateRequest {
            name: "no-git".into(),
            objective: "safe".into(),
        })
        .await
        .expect_err("create");
    assert!(
        error
            .to_string()
            .starts_with("workflow runtime creation failed: "),
        "create error must chain the underlying reason: {error}"
    );
    assert!(
        error.to_string().contains("not inside a git repository"),
        "the actionable not-git reason must surface: {error}"
    );
    application.cleanup().await;
    drop(registration);
    Ok(())
}

#[tokio::test]
async fn workflow_child_todo_mutation_does_not_auto_spawn_jobs() {
    let f = Fixture::new();
    // Objective wording avoids the typed-agent delegation router: a
    // delegation verb (`spawn`, `run`, `make`, ...) immediately followed by an
    // agent-name-shaped noun would fail actionably as a missing agent. This
    // test verifies that a workflow-child Todo mutation never auto-arms the
    // DAG, so the objective stays inert under delegation parsing.
    let objective = "child todo mutation must not auto-arm the dag";
    let (application, factory, _registration) =
        app(&f, vec![planning(objective), FauxResponse::text("planned")]).await;
    let created = application
        .workflow_create(WorkflowCreateRequest {
            name: "no-auto-arm".into(),
            objective: objective.into(),
        })
        .await
        .expect("create workflow");
    let created = wait_status(&application, &created.workflow_id, WorkflowStatus::Running).await;
    let child = factory
        .child_application(&created.workflow_id, created.generation)
        .expect("exact workflow child application");
    let orchestration = child.orchestration_runtime().expect("child orchestration");
    // A committed plan explicitly arms Todo DAG execution (the
    // supervisor owns DAG execution for workflow children), so planning
    // itself spawns the ready task's worker job.
    assert!(
        orchestration.jobs(None).iter().any(|job| {
            job.todo_task_id.is_some()
                && job.workflow_id.as_deref() == Some(created.workflow_id.as_str())
                && job.workflow_generation == Some(created.generation)
        }),
        "a committed plan must arm DAG execution and spawn the ready task's job: {:?}",
        orchestration.jobs(None)
    );

    // Mutating the child's canonical Todo through the mutation-transaction
    // path (session set_todos -> commit hook) must NOT auto-arm the DAG: the
    // supervisor owns DAG execution for workflow children, so the
    // replaced DAG must never spawn jobs on its own.
    child
        .set_todos(vec![TodoPhase {
            name: "Build".into(),
            tasks: vec![TodoItem {
                id: "mutated-root".into(),
                content: "must not auto-spawn".into(),
                status: TodoStatus::Pending,
                depends_on: Vec::new(),
                ready: true,
                waiting_on: Vec::new(),
                agent: None,
            }],
        }])
        .expect("child Todo mutation");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        orchestration
            .jobs(None)
            .iter()
            .all(|job| job.todo_task_id.as_deref() != Some("mutated-root")),
        "a workflow-child Todo mutation must never auto-spawn task jobs: {:?}",
        orchestration.jobs(None)
    );
    assert_eq!(
        child.todo_state().phases[0].tasks[0].id,
        "mutated-root",
        "the canonical Todo still reflects the mutation"
    );
    application.cleanup().await;
}

#[tokio::test]
async fn restored_running_workflow_continues_after_relaunch_and_resume_is_effective() {
    let f = Fixture::new();
    let objective = "continue after relaunch";
    // First run: the supervisor builds the Todo DAG and the workflow runs.
    let (first, _factory, registration) =
        app(&f, vec![planning(objective), FauxResponse::text("planned")]).await;
    let created = first
        .workflow_create(WorkflowCreateRequest {
            name: "restore-continue".into(),
            objective: objective.into(),
        })
        .await
        .expect("create workflow");
    let created = wait_status(&first, &created.workflow_id, WorkflowStatus::Running).await;
    assert_eq!(created.todo.phases[0].tasks[0].content, objective);
    first.cleanup().await;
    drop(registration);

    // Relaunch: restore_all re-creates the runtime and the factory
    // auto-continues the restored Running workflow (RestoreContinue re-arms
    // Todo DAG execution over the stored tasks).
    let (second, _factory, _registration) = app(&f, Vec::new()).await;
    let restored = second
        .workflow_get(&created.workflow_id)
        .expect("restored workflow");
    assert_eq!(restored.status, WorkflowStatus::Running);
    assert_eq!(restored.todo.phases[0].tasks[0].content, objective);

    // /workflow resume is effective for the restored Running workflow.
    let resumed = second
        .workflow_resume(&created.workflow_id, created.generation)
        .await
        .expect("resume restored workflow");
    assert_eq!(resumed.status, WorkflowStatus::Running);
    assert_eq!(resumed.todo.phases[0].tasks[0].content, objective);
    second.cleanup().await;
}

#[tokio::test]
async fn restored_paused_workflow_resume_runs_after_relaunch() {
    let f = Fixture::new();
    let objective = "resume the paused workflow";
    let (first, _factory, registration) =
        app(&f, vec![planning(objective), FauxResponse::text("planned")]).await;
    let created = first
        .workflow_create(WorkflowCreateRequest {
            name: "restore-paused-resume".into(),
            objective: objective.into(),
        })
        .await
        .expect("create workflow");
    let created = wait_status(&first, &created.workflow_id, WorkflowStatus::Running).await;
    let paused = first
        .workflow_pause(&created.workflow_id, created.generation)
        .await
        .expect("pause workflow");
    assert_eq!(paused.status, WorkflowStatus::Paused);
    first.cleanup().await;
    drop(registration);

    let (second, _factory, _registration) = app(&f, Vec::new()).await;
    let restored = second
        .workflow_get(&created.workflow_id)
        .expect("restored workflow");
    assert_eq!(restored.status, WorkflowStatus::Paused);
    let resumed = second
        .workflow_resume(&created.workflow_id, created.generation)
        .await
        .expect("resume restored paused workflow");
    assert_eq!(resumed.status, WorkflowStatus::Running);
    assert_eq!(resumed.todo.phases[0].tasks[0].content, objective);
    second.cleanup().await;
}

/// Two workflows created in one session run simultaneously in independent
/// worktrees, integrate into the same source repository without touching each
/// other's work, and are removed independently. Guards the concurrent-owner
/// contract: worktree registration, branch management, and integration must
/// stay scoped per workflow id/generation.
#[tokio::test]
async fn concurrent_workflows_complete_in_isolated_worktrees_and_both_integrate() {
    let f = Fixture::new();
    // The two workflow supervisors share the faux response queue, and under
    // the plan/execution split each committed plan arms DAG execution (which
    // spawns a worker job that consumes the next queue entry). To keep both
    // workflows deterministically Running, alpha's responses are seeded at
    // app creation and beta's are appended only after alpha is Running —
    // alpha's worker then finds an empty queue (fails silently, task stays
    // open) and beta's responses are never stolen.
    let (application, _factory, registration) = app(
        &f,
        vec![
            planning("alpha feature"),
            FauxResponse::text("alpha planned"),
        ],
    )
    .await;

    let alpha = application
        .workflow_create(WorkflowCreateRequest {
            name: "alpha".into(),
            objective: "alpha feature".into(),
        })
        .await
        .expect("create alpha");
    let alpha = wait_status(&application, &alpha.workflow_id, WorkflowStatus::Running).await;
    let alpha_path = worktree(&f, alpha.workflow_id.as_str());
    assert!(
        alpha_path.exists(),
        "alpha worktree must exist while running"
    );
    commit_file(&alpha_path, "alpha.txt", "alpha\n", "alpha feature work");

    registration.append_response(planning("beta feature"));
    registration.append_response(FauxResponse::text("beta planned"));
    let beta = application
        .workflow_create(WorkflowCreateRequest {
            name: "beta".into(),
            objective: "beta feature".into(),
        })
        .await
        .expect("create beta");
    let beta = wait_status(&application, &beta.workflow_id, WorkflowStatus::Running).await;
    let beta_path = worktree(&f, beta.workflow_id.as_str());
    assert!(beta_path.exists(), "beta worktree must exist while running");
    commit_file(&beta_path, "beta.txt", "beta\n", "beta feature work");

    // Both workflows are live at the same time with distinct worktrees and
    // their own open Todo DAGs.
    assert_ne!(
        alpha_path, beta_path,
        "concurrent workflows must own distinct worktrees"
    );
    assert_eq!(application.workflow_list().len(), 2);
    assert_eq!(alpha.todo.phases[0].tasks[0].content, "alpha feature");
    assert_eq!(beta.todo.phases[0].tasks[0].content, "beta feature");

    // Settle alpha first: the completed DAG auto-integrates into the source.
    application
        .set_workflow_todos(&alpha.workflow_id, completed_todo("alpha feature"))
        .expect("settle alpha DAG");
    let alpha = wait_integrated(&application, &alpha.workflow_id).await;
    assert_eq!(alpha.status, WorkflowStatus::Completed);
    assert!(matches!(
        alpha.integration,
        WorkflowIntegration::Applied { .. }
    ));
    assert_eq!(
        fs::read_to_string(f.repo.join("alpha.txt")).expect("merged alpha"),
        "alpha\n"
    );

    // Alpha's integration must not disturb beta: beta stays Running, its
    // worktree survives, and its commit is not merged into the source.
    let beta_mid = application
        .workflow_get(&beta.workflow_id)
        .expect("beta mid");
    assert_eq!(beta_mid.status, WorkflowStatus::Running);
    assert!(
        beta_path.exists(),
        "alpha's integration must not touch beta's worktree"
    );
    assert!(
        !f.repo.join("beta.txt").exists(),
        "alpha's integration must not merge beta's commit"
    );

    // Settle beta: both integrations now land in the same source repository.
    application
        .set_workflow_todos(&beta.workflow_id, completed_todo("beta feature"))
        .expect("settle beta DAG");
    let beta = wait_integrated(&application, &beta.workflow_id).await;
    assert_eq!(beta.status, WorkflowStatus::Completed);
    assert!(matches!(
        beta.integration,
        WorkflowIntegration::Applied { .. }
    ));
    assert_eq!(
        fs::read_to_string(f.repo.join("beta.txt")).expect("merged beta"),
        "beta\n"
    );
    assert_eq!(
        fs::read_to_string(f.repo.join("alpha.txt")).expect("alpha still merged"),
        "alpha\n"
    );

    // Removal is independent: removing alpha leaves beta's record and worktree
    // in place until its own remove.
    application
        .workflow_remove(&alpha.workflow_id, alpha.generation)
        .await
        .expect("remove alpha");
    assert!(!alpha_path.exists(), "alpha worktree must be gone");
    assert_eq!(application.workflow_list().len(), 1);
    assert!(
        beta_path.exists(),
        "beta worktree must survive alpha's removal"
    );
    application
        .workflow_remove(&beta.workflow_id, beta.generation)
        .await
        .expect("remove beta");
    assert!(application.workflow_list().is_empty());
    assert!(!beta_path.exists());
    application.cleanup().await;
}

/// Invalid lifecycle transitions and stale generations are rejected through
/// the public Application surface: integrate on a Running workflow, pause/
/// resume/cancel on a terminal workflow, and any operation with an outdated
/// generation must fail without mutating state.
#[tokio::test]
async fn invalid_and_stale_transitions_fail_at_the_application_surface() {
    let f = Fixture::new();
    let (application, _factory, _registration) = app(
        &f,
        vec![planning("transition guard"), FauxResponse::text("planned")],
    )
    .await;
    let created = application
        .workflow_create(WorkflowCreateRequest {
            name: "guards".into(),
            objective: "transition guard".into(),
        })
        .await
        .expect("create workflow");
    let running = wait_status(&application, &created.workflow_id, WorkflowStatus::Running).await;

    // Integrate on a non-completed workflow is rejected before any runtime
    // call (only Completed/Paused/Conflicted may integrate).
    let error = application
        .workflow_integrate(&running.workflow_id, running.generation)
        .await
        .expect_err("integrate while Running must fail");
    assert!(
        error
            .to_string()
            .contains("workflow lifecycle transition is not allowed"),
        "integrate-while-running error: {error:#}"
    );

    // A stale generation fails before touching the runtime at all.
    let error = application
        .workflow_pause(&running.workflow_id, running.generation + 1)
        .await
        .expect_err("stale generation must fail");
    assert_eq!(error.to_string(), "workflow generation is stale");

    // The workflow is still Running and pause with the current generation works.
    let path = worktree(&f, running.workflow_id.as_str());
    commit_file(&path, "guard.txt", "guard\n", "guard work");
    let paused = application
        .workflow_pause(&running.workflow_id, running.generation)
        .await
        .expect("pause running workflow");
    assert_eq!(paused.status, WorkflowStatus::Paused);

    // Resume settles the DAG and auto-integrates to Completed.
    application
        .set_workflow_todos(&running.workflow_id, completed_todo("transition guard"))
        .expect("settle while paused");
    let completed = application
        .workflow_resume(&running.workflow_id, running.generation)
        .await
        .expect("resume settles completed workflow");
    assert_eq!(completed.status, WorkflowStatus::Completed);
    assert!(matches!(
        completed.integration,
        WorkflowIntegration::Applied { .. }
    ));

    // Terminal workflows reject pause, resume, and cancel. (Integrate on a
    // Completed workflow stays allowed: it is the idempotent re-integrate
    // path exercised by the full-lifecycle test.)
    for action in ["pause", "resume", "cancel"] {
        let error = match action {
            "pause" => {
                application
                    .workflow_pause(&completed.workflow_id, completed.generation)
                    .await
            }
            "resume" => {
                application
                    .workflow_resume(&completed.workflow_id, completed.generation)
                    .await
            }
            "cancel" => {
                application
                    .workflow_cancel(&completed.workflow_id, completed.generation)
                    .await
            }
            _ => unreachable!("unknown transition action"),
        }
        .expect_err(&format!("{action} on Completed must fail"));
        assert!(
            error
                .to_string()
                .contains("workflow lifecycle transition is not allowed"),
            "{action} on Completed error: {error:#}"
        );
    }
    // The idempotent re-integrate path keeps working on the terminal record.
    let reintegrated = application
        .workflow_integrate(&completed.workflow_id, completed.generation)
        .await
        .expect("re-integrate a Completed workflow is idempotent");
    assert_eq!(reintegrated.status, WorkflowStatus::Completed);

    // The terminal record is untouched by the rejected transitions.
    let still = application
        .workflow_get(&completed.workflow_id)
        .expect("workflow");
    assert_eq!(still.status, WorkflowStatus::Completed);
    assert!(matches!(
        still.integration,
        WorkflowIntegration::Applied { .. }
    ));
    application.cleanup().await;
}

/// The workflow supervisor carries the objective's explicit
/// agent role into the Todo DAG as a typed routing contract. A planner that
/// paraphrases the task and DROPS the `researcher` mention from the content
/// still routes the worker job to `researcher` through the `agents` array of
/// the todo init call.
#[tokio::test]
async fn workflow_typed_agent_role_survives_planner_paraphrase() {
    let f = Fixture::new();
    let objective = "你让researcher仔细调研pi-coding-agent";
    let responses = vec![
        // Scripted planner: paraphrased content WITHOUT the agent name, but
        // the typed `agents` field preserves the researcher role.
        FauxResponse {
            content: vec![ContentBlock::ToolCall(ToolCall {
                id: "todo-init".into(),
                name: "todo".into(),
                arguments: serde_json::json!({"op":"init","list":[{"phase":"Plan","items":["仔细调研pi-coding-agent"],"agents":["researcher"]}]}),
                thought_signature: None,
            })],
            stop_reason: StopReason::ToolUse,
            error_message: None,
        },
        FauxResponse::text("worker done"),
        FauxResponse::text("workflow complete"),
        FauxResponse::text("tail"),
    ];
    let (session, registration) = parent_session(&f.repo, responses);
    let snapshot = session.child_session_options_snapshot();
    let application = application_with_isolated_resources(session).await;
    application
        .attach_runtime_factory(Arc::new(TypedAgentTestFactory { snapshot }))
        .expect("factory");
    let factory = application
        .setup_workflows(&f.repo, &f.state, &f.managed)
        .await
        .expect("setup");
    let created = application
        .workflow_create(WorkflowCreateRequest {
            name: "typed-role".into(),
            objective: objective.into(),
        })
        .await
        .expect("create workflow");
    let running = wait_status(&application, &created.workflow_id, WorkflowStatus::Running).await;
    assert_eq!(
        running.todo.phases[0].tasks[0].content,
        "仔细调研pi-coding-agent"
    );
    assert_eq!(
        running.todo.phases[0].tasks[0].agent.as_deref(),
        Some("researcher"),
        "the committed Todo must carry the typed agent role"
    );
    let task_id = running.todo.phases[0].tasks[0].id.clone();
    let child = factory
        .child_application(&created.workflow_id, created.generation)
        .expect("exact workflow child application");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let jobs = child
                .orchestration_runtime()
                .expect("child orchestration")
                .jobs(None);
            if jobs.iter().any(|job| {
                job.todo_task_id.as_deref() == Some(task_id.as_str())
                    && job.workflow_id.as_deref() == Some(created.workflow_id.as_str())
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the paraphrased plan must arm a worker job");
    let jobs = child
        .orchestration_runtime()
        .expect("child orchestration")
        .jobs(None);
    let typed = jobs
        .iter()
        .find(|job| job.todo_task_id.as_deref() == Some(task_id.as_str()))
        .unwrap_or_else(|| {
            panic!(
                "no job for task {task_id:?}; all child jobs: {:?}",
                jobs.iter()
                    .map(|job| (
                        job.agent.as_str(),
                        job.agent_id.as_str(),
                        job.todo_task_id.as_deref(),
                        job.status,
                    ))
                    .collect::<Vec<_>>()
            )
        });
    assert_eq!(
        typed.agent,
        "researcher",
        "the paraphrased plan must still route to researcher through the typed agent field; all child jobs: {:?}",
        jobs.iter()
            .map(|job| (
                job.agent.as_str(),
                job.agent_id.as_str(),
                job.todo_task_id.as_deref(),
                job.status
            ))
            .collect::<Vec<_>>()
    );
    // The worker child's id is the generated Todo-slot id; only the routed
    // agent name must equal the typed role.
    assert_eq!(typed.agent_id, "Todo1");
    application.cleanup().await;
    drop(registration);
}

/// A workflow objective that explicitly delegates to an
/// agent absent from the workflow child catalog fails actionably at planning
/// time — naming the missing definition — instead of silently spawning the
/// bundled `task` agent. A delegation to a present agent plans normally.
#[tokio::test]
async fn workflow_objective_naming_missing_agent_fails_actionably() {
    let f = Fixture::new();
    // The TestFactory child catalog only bundles `task`.
    let (application, _factory, _registration) = app(&f, Vec::new()).await;
    let created = application
        .workflow_create(WorkflowCreateRequest {
            name: "missing-agent".into(),
            objective: "你让researcher仔细调研pi-coding-agent".into(),
        })
        .await
        .expect("workflow creation succeeds with async supervisor start");
    let failed = wait_status(&application, &created.workflow_id, WorkflowStatus::Failed).await;
    let message = failed
        .failure
        .as_ref()
        .expect("workflow failure message")
        .message
        .clone();
    assert!(message.contains("researcher"), "{message}");
    assert!(message.contains("not defined"), "{message}");
    assert!(message.contains("workflow agent catalog"), "{message}");
    assert!(
        failed.todo.phases.is_empty(),
        "no plan may be committed for a missing explicit agent"
    );
    application.cleanup().await;

    // Positive control: the same child catalog accepts a delegation to the
    // present bundled `task` agent and plans normally.
    let f = Fixture::new();
    let (application, _factory, _registration) = app(
        &f,
        vec![
            planning("让task处理这个任务"),
            FauxResponse::text("planned"),
        ],
    )
    .await;
    let created = application
        .workflow_create(WorkflowCreateRequest {
            name: "present-agent".into(),
            objective: "让task处理这个任务".into(),
        })
        .await
        .expect("create workflow");
    let running = wait_status(&application, &created.workflow_id, WorkflowStatus::Running).await;
    assert_eq!(
        running.todo.phases[0].tasks[0].content,
        "让task处理这个任务"
    );
    application.cleanup().await;
}

/// Session isolation of todos across the workflow boundary. A workflow runs
/// in its own child session (fresh recording), so its planning todo must
/// never leak into — or clobber — the main session's todo, and a new
/// workflow must never inherit the main session's todos.
#[tokio::test]
async fn workflow_todo_is_isolated_from_main_session_todo() {
    let f = Fixture::new();
    let objective = "ship isolated feature";
    let (application, _factory, _registration) = app(&f, completing_responses(objective)).await;

    // Seed the MAIN session with its own distinct todos.
    application
        .set_todos(vec![TodoPhase {
            name: "Parent".into(),
            tasks: vec![TodoItem {
                id: "parent-only".into(),
                content: "main session task".into(),
                status: TodoStatus::Pending,
                depends_on: Vec::new(),
                ready: true,
                waiting_on: Vec::new(),
                agent: None,
            }],
        }])
        .expect("seed main session todos");
    assert_eq!(
        application.todo_state().phases[0].tasks[0].id,
        "parent-only"
    );

    // The workflow plans and completes inside its own child session.
    let created = application
        .workflow_create(WorkflowCreateRequest {
            name: "isolated".into(),
            objective: objective.into(),
        })
        .await
        .expect("create workflow");
    let settled = wait_status(&application, &created.workflow_id, WorkflowStatus::Failed).await;

    // The workflow's todo is its own plan — it never inherited the main
    // session's tasks.
    assert_eq!(
        settled.todo.phases[0].tasks[0].content, objective,
        "workflow todo must be its own plan, not the parent's tasks"
    );
    assert!(
        settled
            .todo
            .phases
            .iter()
            .flat_map(|phase| &phase.tasks)
            .all(|task| task.id != "parent-only"),
        "workflow must not inherit the main session's todos"
    );

    // The main session's todos survive the workflow lifecycle untouched.
    let main = application.todo_state();
    assert_eq!(main.phases.len(), 1, "main todo must not be clobbered");
    assert_eq!(main.phases[0].name, "Parent");
    assert_eq!(main.phases[0].tasks[0].id, "parent-only");
    application.cleanup().await;
}

#[tokio::test]
async fn workflow_child_fans_out_ready_wave_to_concurrent_workers() {
    // User-visible parallelism contract: with several INDEPENDENT ready
    // tasks the workflow child must spawn the whole ready wave at once — one
    // worker job per ready task, all registered in the same batch — instead
    // of one-at-a-time. The coordinator (todo_execution.rs) selects up to
    // `max_concurrency` ready candidates per reconcile and spawn_tasks
    // launches the batch together; this pins that contract end-to-end in the
    // workflow child (the exact "T2.6/T2.7/T2.8 ready together" scenario).
    let f = Fixture::new();
    let objective = "wave of ready todos";
    let (application, factory, _registration) =
        app(&f, vec![planning(objective), FauxResponse::text("planned")]).await;
    let created = application
        .workflow_create(WorkflowCreateRequest {
            name: "wave".into(),
            objective: objective.into(),
        })
        .await
        .expect("create workflow");
    let running = wait_status(&application, &created.workflow_id, WorkflowStatus::Running).await;
    let child = factory
        .child_application(&created.workflow_id, created.generation)
        .expect("exact workflow child application");

    // Replace the canonical DAG with four independent ready tasks (width 4,
    // below the DEFAULT_MAX_CONCURRENCY=4 ceiling used by the TestFactory).
    let ids = ["root-alpha", "root-beta", "root-gamma", "root-delta"];
    let tasks = ids
        .iter()
        .map(|id| TodoItem {
            id: (*id).to_owned(),
            content: format!("execute {id}"),
            status: TodoStatus::Pending,
            depends_on: Vec::new(),
            ready: true,
            waiting_on: Vec::new(),
            agent: None,
        })
        .collect();
    application
        .set_workflow_todos(
            &running.workflow_id,
            vec![TodoPhase {
                name: "Wave".into(),
                tasks,
            }],
        )
        .expect("canonical workflow Todo wave");

    // The whole ready wave spawns in one batch: every ready task gets an
    // owned worker job, all present together in the child runtime. A stale
    // owner for the ORIGINAL planned DAG may coexist (the mutation replaced
    // the DAG mid-flight), so the wave contract is: all four replacement
    // tasks are owned simultaneously — no wave member is missing. The
    // workers settle (Failed when the faux response queue runs dry), which
    // is fine: this test pins the spawn wave, not worker success.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let jobs = child
                .orchestration_runtime()
                .expect("child orchestration")
                .jobs(None);
            let owned = jobs
                .iter()
                .filter(|job| {
                    job.todo_task_id
                        .as_deref()
                        .is_some_and(|id| ids.contains(&id))
                })
                .count();
            if owned == ids.len() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        let jobs = child
            .orchestration_runtime()
            .expect("child orchestration")
            .jobs(None);
        panic!(
            "the ready wave must spawn one worker job per ready task; observed jobs: {:?}",
            jobs.iter()
                .map(|job| (
                    job.agent_id.as_str(),
                    job.status,
                    job.todo_task_id.as_deref()
                ))
                .collect::<Vec<_>>()
        );
    });
    let jobs = child
        .orchestration_runtime()
        .expect("child orchestration")
        .jobs(None);
    let linked = jobs
        .iter()
        .filter_map(|job| job.todo_task_id.as_deref())
        .collect::<Vec<_>>();
    for id in ids {
        assert!(
            linked.contains(&id),
            "every ready task needs an owned worker job: missing {id} in {linked:?}"
        );
    }
    assert!(
        jobs.iter()
            .filter(|job| job
                .todo_task_id
                .as_deref()
                .is_some_and(|id| ids.contains(&id)))
            .all(|job| job.workflow_id.as_deref() == Some(created.workflow_id.as_str())),
        "wave jobs must stay workflow-scoped"
    );
    application.cleanup().await;
}
