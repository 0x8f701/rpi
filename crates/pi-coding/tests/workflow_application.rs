use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use pi_agent::ThinkingLevel;
use pi_ai::providers::{FauxProviderOptions, FauxProviderRegistration, FauxResponse, register_faux_provider};
use pi_ai::{ContentBlock, Model, StopReason, ToolCall};
use pi_coding::{
    AgentCatalog, AgentDefinition, AgentDefinitionSource, Application, ApplicationRuntimeCandidate,
    ApplicationRuntimeFactory, ApplicationRuntimeFuture, ChildSessionOptionsSnapshot,
    OrchestrationConfig, OrchestrationRuntime, Session, SessionOptions, TodoItem, TodoPhase,
    TodoStatus, WorkflowCreateRequest, WorkflowIntegration, WorkflowStatus, WorkspaceRoots,
};
use tempfile::TempDir;

struct Fixture { sandbox: TempDir, repo: PathBuf, state: PathBuf, managed: PathBuf }
impl Fixture {
    fn new() -> Self {
        let sandbox = tempfile::tempdir().expect("sandbox");
        let repo = sandbox.path().join("source");
        fs::create_dir_all(&repo).expect("repo");
        git(&repo, &["init"]); git(&repo, &["config", "user.name", "Pi Test"]); git(&repo, &["config", "user.email", "pi@example.test"]);
        fs::write(repo.join("README.md"), "base\n").expect("base"); git(&repo, &["add", "README.md"]); git(&repo, &["commit", "-m", "initial"]);
        let state = sandbox.path().join("state"); let managed = sandbox.path().join("managed");
        Self { sandbox, repo, state, managed }
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
        .args(["-c", "color.ui=false", "-c", "commit.gpgsign=false", "-c", "init.defaultBranch=main"])
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("git");
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).expect("utf8").trim().to_owned()
}
fn commit_file(cwd: &Path, relative: &str, contents: &str, message: &str) -> String {
    fs::write(cwd.join(relative), contents).expect("write"); git(cwd, &["add", "--", relative]); git(cwd, &["commit", "-m", message]); git(cwd, &["rev-parse", "HEAD"])
}
fn definition() -> AgentDefinition { AgentDefinition { name: "task".into(), description: "workflow worker".into(), system_prompt: "complete workflow Todo".into(), tools: Some(Vec::new()), autoload_skills: Vec::new(), model: None, thinking_level: Some(ThinkingLevel::Off), max_turns: None, max_tool_calls: None, timeout_secs: None, disallowed_tools: Vec::new(), capability_ceiling: None, source: AgentDefinitionSource::Bundled, path: None, trusted: true, kind: pi_coding::AgentDefinitionKind::Agent, personality: None, soft_budget: None } }

#[derive(Clone)] struct TestFactory { snapshot: ChildSessionOptionsSnapshot }
impl ApplicationRuntimeFactory for TestFactory {
    fn build_runtime_candidate(&self, _: PathBuf, _: SessionOptions, _: Option<pi_coding::PreparedSessionResume>) -> ApplicationRuntimeFuture { Box::pin(async { anyhow::bail!("unused") }) }
    fn build_trusted_workflow_candidate(&self, cwd: pi_coding::workflow_worktree::TrustedWorkflowCwd, mut options: SessionOptions) -> ApplicationRuntimeFuture {
        let snapshot = self.snapshot.clone(); Box::pin(async move {
            options.cwd = cwd.path().to_path_buf(); options.tools = None;
            let workspace = WorkspaceRoots::new(cwd.path(), Vec::<PathBuf>::new())?;
            let session = Session::new_with_todo_additional_tools_filtered_discovery_workspace_and_uri(options, Vec::new(), Default::default(), pi_coding::ResourceDiscovery::Disabled, workspace, None)?;
            session.start_new_recording()?;
            let artifacts = std::env::temp_dir().join(format!("pi-workflow-test-artifacts-{}", uuid::Uuid::now_v7()));
            let mut config = OrchestrationConfig::new(AgentCatalog::from_agents(vec![definition()]), artifacts);
            config.parent_model = snapshot.model.clone(); config.max_concurrency = pi_coding::DEFAULT_MAX_CONCURRENCY; config.idle_ttl = None;
            let orchestration = OrchestrationRuntime::new(config, OrchestrationRuntime::child_factory_from_snapshot(snapshot))?;
            Ok(ApplicationRuntimeCandidate::new(session).with_orchestration(orchestration))
        })
    }
}

/// Workflow child catalog that additionally bundles `researcher` and `writer`
/// (the bundled default `task` stays the fallback default), so the typed
/// agent-routing E2E can spawn the named researcher.
#[derive(Clone)] struct TypedAgentTestFactory { snapshot: ChildSessionOptionsSnapshot }
impl ApplicationRuntimeFactory for TypedAgentTestFactory {
    fn build_runtime_candidate(&self, _: PathBuf, _: SessionOptions, _: Option<pi_coding::PreparedSessionResume>) -> ApplicationRuntimeFuture { Box::pin(async { anyhow::bail!("unused") }) }
    fn build_trusted_workflow_candidate(&self, cwd: pi_coding::workflow_worktree::TrustedWorkflowCwd, mut options: SessionOptions) -> ApplicationRuntimeFuture {
        let snapshot = self.snapshot.clone(); Box::pin(async move {
            options.cwd = cwd.path().to_path_buf(); options.tools = None;
            let workspace = WorkspaceRoots::new(cwd.path(), Vec::<PathBuf>::new())?;
            let session = Session::new_with_todo_additional_tools_filtered_discovery_workspace_and_uri(options, Vec::new(), Default::default(), pi_coding::ResourceDiscovery::Disabled, workspace, None)?;
            session.start_new_recording()?;
            let artifacts = std::env::temp_dir().join(format!("pi-workflow-test-artifacts-{}", uuid::Uuid::now_v7()));
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
            config.parent_model = snapshot.model.clone(); config.max_concurrency = pi_coding::DEFAULT_MAX_CONCURRENCY; config.idle_ttl = None;
            let orchestration = OrchestrationRuntime::new(config, OrchestrationRuntime::child_factory_from_snapshot(snapshot))?;
            Ok(ApplicationRuntimeCandidate::new(session).with_orchestration(orchestration))
        })
    }
}
fn parent_session(repo: &Path, responses: Vec<FauxResponse>) -> (Session, FauxProviderRegistration) {
    let suffix = uuid::Uuid::now_v7().to_string(); let model = Model { id: format!("workflow-{suffix}"), name: "Workflow Test".into(), api: format!("workflow-api-{suffix}"), provider: format!("workflow-provider-{suffix}"), ..Model::default() };
    let registration = register_faux_provider(FauxProviderOptions { api: model.api.clone(), provider: model.provider.clone(), models: vec![model.clone()], chunk_size: 32 }); registration.set_responses(responses);
    let session = Session::new(SessionOptions { model, cwd: repo.to_path_buf(), system_prompt: String::new(), thinking_level: ThinkingLevel::Off, api_key: "faux".into(), compaction: None, stream_options: Default::default(), tools: Some(Vec::new()), before_tool_call: None, after_tool_call: None, stream_fn: None, auth_resolver: None }).expect("session");
    (session, registration)
}
fn planning(objective: &str) -> FauxResponse { FauxResponse { content: vec![ContentBlock::ToolCall(ToolCall { id: "todo-init".into(), name: "todo".into(), arguments: serde_json::json!({"op":"init","items":[objective]}), thought_signature: None })], stop_reason: StopReason::ToolUse, error_message: None } }

fn task_tool_call(task: &str) -> FauxResponse { FauxResponse { content: vec![ContentBlock::ToolCall(ToolCall { id: "task-delegate".into(), name: "task".into(), arguments: serde_json::json!({"task": task}), thought_signature: None })], stop_reason: StopReason::ToolUse, error_message: None } }

fn todo_done_call(content: &str) -> FauxResponse { FauxResponse { content: vec![ContentBlock::ToolCall(ToolCall { id: "todo-done".into(), name: "todo".into(), arguments: serde_json::json!({"op":"done","task":content}), thought_signature: None })], stop_reason: StopReason::ToolUse, error_message: None } }

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

/// Agent script that completes the full supervisor-driven lifecycle in one
/// planning turn: build the Todo DAG (todo init), delegate the ready task to
/// a worker (task tool), then mark it done once the worker has finished
/// (todo done by content). The trailing text responses keep both the
/// supervisor turn and the delegated worker session fed regardless of which
/// agent consumes them.
fn completing_responses(objective: &str) -> Vec<FauxResponse> {
    vec![
        planning(objective),
        task_tool_call(&format!("execute {objective}")),
        todo_done_call(objective),
        FauxResponse::text("worker done"),
        FauxResponse::text("workflow complete"),
    ]
}
async fn app(
    f: &Fixture,
    responses: Vec<FauxResponse>,
) -> (
    Application,
    Arc<pi_coding::ApplicationWorkflowRuntimeFactory>,
    FauxProviderRegistration,
) {
    let (session, registration) = parent_session(&f.repo, responses);
    let snapshot = session.child_session_options_snapshot();
    let application = Application::new(session).await;
    application
        .attach_runtime_factory(Arc::new(TestFactory { snapshot }))
        .expect("factory");
    let factory = application
        .setup_workflows(&f.repo, &f.state, &f.managed)
        .await
        .expect("setup");
    (application, factory, registration)
}
fn worktree(f: &Fixture, id: &str) -> PathBuf { f.managed.join(pi_coding::workflow_worktree::WORKTREE_ROOT_DIR_NAME).join(id) }
async fn wait_status(application: &Application, workflow_id: &pi_coding::WorkflowId, status: WorkflowStatus) -> pi_coding::WorkflowSnapshot {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = application.workflow_get(workflow_id).expect("workflow");
            if snapshot.status == status { return snapshot; }
            tokio::task::yield_now().await;
        }
    }).await.expect("workflow status timeout")
}
async fn wait_integrated(application: &Application, workflow_id: &pi_coding::WorkflowId) -> pi_coding::WorkflowSnapshot {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = application.workflow_get(workflow_id).expect("workflow");
            if matches!(snapshot.integration, WorkflowIntegration::Applied { .. }) { return snapshot; }
            tokio::task::yield_now().await;
        }
    }).await.expect("workflow auto-integrate timeout")
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
            blocked_by: Vec::new(),
            agent: None,
        }],
    }]
}

#[tokio::test]
async fn create_isolates_child_runtime() {
    let f = Fixture::new(); let objective = "delegate task to ship feature"; let (application, _factory, _registration) = app(&f, completing_responses(objective)).await;
    let created = application.workflow_create(WorkflowCreateRequest { name: "ship".into(), objective: objective.into() }).await.expect("create");
    let created = wait_status(&application, &created.workflow_id, WorkflowStatus::Completed).await;
    let path = worktree(&f, created.workflow_id.as_str()); assert!(path.exists() && !path.starts_with(&f.repo)); assert!(created.branch.as_deref().is_some_and(|b| b.starts_with("rpi/workflow/")));
    assert_eq!(created.todo.phases[0].tasks[0].content, objective); application.cleanup().await;
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
    assert_eq!(created.todo.phases[0].tasks[0].content, "plan before Todo mutation");

    let result = application
        .set_workflow_todos(&created.workflow_id, vec![TodoPhase {
            name: "Build".into(),
            tasks: vec![TodoItem {
                id: "later-root".into(),
                content: "execute later canonical Todo".into(),
                status: TodoStatus::Pending,
                depends_on: Vec::new(),
                ready: true,
                blocked_by: Vec::new(),
                agent: None,
            }],
        }])
        .expect("canonical workflow Todo mutation");
    assert_eq!(result.phases[0].tasks[0].id, "later-root");
    assert!(application.todo_state().phases.is_empty(), "parent Todo must remain unchanged");
    let child = factory.child_application(&created.workflow_id, created.generation).expect("exact workflow child application");
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
    assert!(jobs.iter().any(|job| job.todo_task_id.as_deref() == Some("later-root") && job.workflow_id.as_deref() == Some(created.workflow_id.as_str()) && job.workflow_generation == Some(created.generation)), "workflow Todo jobs must preserve exact ownership: {jobs:?}");
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
    // P0-1 + P0-2 real-application contract: a provider that commits a plan
    // (todo init) and then emits endless tool turns must not keep the
    // workflow in Planning. The bounded planning run stops at the turn
    // budget, the committed DAG is preserved, DAG execution is armed, and
    // the workflow runs.
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
    // P0-1 real-application contract: a provider that NEVER commits a plan
    // (endless successful todo views) is stopped by the planning turn
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
    let f = Fixture::new(); let objective = "detail projection"; let (application, _factory, _registration) = app(&f, completing_responses(objective)).await;
    let created = application.workflow_create(WorkflowCreateRequest { name: "detail".into(), objective: objective.into() }).await.expect("create");
    let completed = wait_status(&application, &created.workflow_id, WorkflowStatus::Completed).await;
    let detail = application.workflow_detail(&completed.workflow_id, completed.generation).expect("detail");
    assert_eq!(detail.workflow_id, completed.workflow_id);
    assert_eq!(detail.generation, completed.generation);
    assert_eq!(detail.status, WorkflowStatus::Completed);
    assert_eq!(detail.todo, completed.todo);
    let stale = application.workflow_detail(&completed.workflow_id, completed.generation + 1).expect_err("stale generation");
    assert_eq!(stale.to_string(), "workflow generation is stale");
    let removed = application.workflow_remove(&completed.workflow_id, completed.generation).await.expect("remove");
    assert_eq!(removed.workflow_id, completed.workflow_id);
    let missing = application.workflow_detail(&completed.workflow_id, completed.generation).expect_err("removed workflow");
    assert_eq!(missing.to_string(), "workflow was not found");
    application.cleanup().await;
}

#[tokio::test]
async fn empty_planning_fails_with_actionable_message_and_detail() {
    let f = Fixture::new();
    // Two plain-text turns: the bounded re-prompt also produces no Todo
    // tasks, so the workflow must fail with the actionable stuck-planning
    // message instead of sitting in Planning forever.
    let (application, _factory, _registration) = app(
        &f,
        vec![
            FauxResponse::text("planning acknowledged"),
            FauxResponse::text("still nothing to plan"),
        ],
    )
    .await;
    let created = application
        .workflow_create(WorkflowCreateRequest {
            name: "empty-planning".into(),
            objective: "must fail when planning produces no tasks".into(),
        })
        .await
        .expect("create workflow");
    let failed = wait_status(&application, &created.workflow_id, WorkflowStatus::Failed).await;
    assert!(failed.todo.phases.is_empty(), "no tasks were planned");
    assert!(
        failed
            .failure
            .as_ref()
            .is_some_and(|failure| failure.message.contains("planning produced no tasks")),
        "failure must be actionable, got {:?}",
        failed.failure
    );
    let detail = application
        .workflow_detail(&failed.workflow_id, failed.generation)
        .expect("failed workflow detail");
    assert_eq!(detail.status, WorkflowStatus::Failed);
    assert!(detail.todo.phases.is_empty());
    application.cleanup().await;
}

#[tokio::test]
async fn restore_adopts_committed_head() {
    let f = Fixture::new(); let (first, _factory, registration) = app(&f, completing_responses("restore task")).await;
    let created = first.workflow_create(WorkflowCreateRequest { name: "restore".into(), objective: "restore task".into() }).await.expect("create"); let created = wait_status(&first, &created.workflow_id, WorkflowStatus::Completed).await; let path = worktree(&f, created.workflow_id.as_str()); let head = commit_file(&path, "restored.txt", "workflow\n", "progress"); first.cleanup().await; drop(registration);
    let (second, _factory, _registration) = app(&f, Vec::new()).await; let restored = second.workflow_get(&created.workflow_id).expect("restore"); assert_eq!(restored.status, WorkflowStatus::Completed); assert_eq!(restored.todo.phases, created.todo.phases); assert_eq!(git(&path, &["rev-parse", "HEAD"]), head); second.cleanup().await;
}

#[tokio::test]
async fn integrate_remove_and_conflict_retention() {
    let f = Fixture::new(); let (application, _factory, _registration) = app(&f, completing_responses("integrate")).await;
    let created = application.workflow_create(WorkflowCreateRequest { name: "integrate".into(), objective: "integrate".into() }).await.expect("create"); let created = wait_status(&application, &created.workflow_id, WorkflowStatus::Completed).await; let path = worktree(&f, created.workflow_id.as_str()); commit_file(&path, "feature.txt", "integrated\n", "feature");
    let integrated = application.workflow_integrate(&created.workflow_id, created.generation).await.expect("integrate"); assert_eq!(integrated.status, WorkflowStatus::Completed); assert!(matches!(integrated.integration, WorkflowIntegration::Applied { .. })); assert_eq!(fs::read_to_string(f.repo.join("feature.txt")).expect("feature"), "integrated\n"); application.workflow_remove(&created.workflow_id, integrated.generation).await.expect("remove"); assert!(!path.exists()); application.cleanup().await;

    let f = Fixture::new(); let (application, _factory, _registration) = app(&f, completing_responses("conflict")).await;
    let created = application.workflow_create(WorkflowCreateRequest { name: "conflict".into(), objective: "conflict".into() }).await.expect("create"); let created = wait_status(&application, &created.workflow_id, WorkflowStatus::Completed).await; let path = worktree(&f, created.workflow_id.as_str()); commit_file(&path, "README.md", "workflow side\n", "workflow"); commit_file(&f.repo, "README.md", "source side\n", "source");
    let conflicted = application.workflow_integrate(&created.workflow_id, created.generation).await.expect("conflict"); assert_eq!(conflicted.status, WorkflowStatus::Conflicted); assert!(matches!(&conflicted.integration, WorkflowIntegration::Conflicted { conflicts } if conflicts.as_slice() == [String::from("README.md")])); assert!(path.exists()); application.cleanup().await;
}

#[tokio::test]
async fn todo_completion_auto_integrates_worktree_back_into_source() {
    let f = Fixture::new();
    let objective = "auto integrate after completion";
    let (application, _factory, _registration) = app(&f, vec![planning(objective), FauxResponse::text("planned")]).await;
    let created = application.workflow_create(WorkflowCreateRequest { name: "auto-integrate".into(), objective: objective.into() }).await.expect("create");
    let running = wait_status(&application, &created.workflow_id, WorkflowStatus::Running).await;
    let path = worktree(&f, created.workflow_id.as_str());
    // The workflow worktree accumulates committed work before the DAG settles.
    let worktree_commit = commit_file(&path, "auto.txt", "auto integrated\n", "worktree work");
    // Mark the canonical Todo DAG complete (as the supervisor agent does with
    // the todo tool): the DAG settles into Completed and the manager
    // auto-integrates the worktree back without a manual `/workflow integrate`.
    application.set_workflow_todos(&running.workflow_id, completed_todo(objective)).expect("canonical workflow Todo completion");
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
    assert_eq!(fs::read_to_string(f.repo.join("auto.txt")).expect("merged file"), "auto integrated\n");
    application.cleanup().await;
}

#[tokio::test]
async fn todo_completion_conflict_lands_conflicted_not_stuck() {
    let f = Fixture::new();
    let objective = "conflict auto integrate";
    let (application, _factory, _registration) = app(&f, vec![planning(objective), FauxResponse::text("planned")]).await;
    let created = application.workflow_create(WorkflowCreateRequest { name: "auto-conflict".into(), objective: objective.into() }).await.expect("create");
    let running = wait_status(&application, &created.workflow_id, WorkflowStatus::Running).await;
    let path = worktree(&f, created.workflow_id.as_str());
    // Both sides move the same file before the DAG settles.
    commit_file(&path, "README.md", "workflow side\n", "workflow");
    commit_file(&f.repo, "README.md", "source side\n", "source");
    application.set_workflow_todos(&running.workflow_id, completed_todo(objective)).expect("canonical workflow Todo completion");
    let conflicted = wait_status(&application, &created.workflow_id, WorkflowStatus::Conflicted).await;
    assert!(matches!(&conflicted.integration, WorkflowIntegration::Conflicted { conflicts } if conflicts.as_slice() == [String::from("README.md")]));
    assert!(path.exists(), "a merge conflict must preserve the worktree for manual resolution");
    assert_eq!(fs::read_to_string(f.repo.join("README.md")).expect("source file"), "source side\n", "the aborted merge must not clobber the source tree");
    application.cleanup().await;
}

#[tokio::test]
async fn paused_workflow_does_not_auto_integrate_until_resume() {
    let f = Fixture::new();
    let objective = "paused no auto integrate";
    let (application, _factory, _registration) = app(&f, vec![planning(objective), FauxResponse::text("planned")]).await;
    let created = application.workflow_create(WorkflowCreateRequest { name: "paused-integrate".into(), objective: objective.into() }).await.expect("create");
    let running = wait_status(&application, &created.workflow_id, WorkflowStatus::Running).await;
    let paused = application.workflow_pause(&created.workflow_id, created.generation).await.expect("pause");
    assert_eq!(paused.status, WorkflowStatus::Paused);
    let path = worktree(&f, created.workflow_id.as_str());
    commit_file(&path, "paused.txt", "paused work\n", "paused work");
    // The DAG settles while paused; the paused runtime never projects a
    // status change, so the merge must NOT run.
    application.set_workflow_todos(&running.workflow_id, completed_todo(objective)).expect("Todo completion while paused");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let snapshot = application.workflow_get(&created.workflow_id).expect("workflow");
    assert_eq!(snapshot.status, WorkflowStatus::Paused);
    assert_eq!(snapshot.integration, WorkflowIntegration::None);
    assert!(!f.repo.join("paused.txt").exists(), "a paused workflow must not merge worktree changes");
    // Resume settles the DAG and auto-integrates the accumulated work.
    let resumed = application.workflow_resume(&created.workflow_id, created.generation).await.expect("resume");
    assert_eq!(resumed.status, WorkflowStatus::Completed);
    let integrated = wait_integrated(&application, &created.workflow_id).await;
    assert_eq!(integrated.status, WorkflowStatus::Completed);
    assert!(matches!(integrated.integration, WorkflowIntegration::Applied { .. }));
    assert_eq!(fs::read_to_string(f.repo.join("paused.txt")).expect("merged after resume"), "paused work\n");
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
    assert_eq!(running.todo.phases[0].tasks[0].content, "resume after process exit");
    let paused = tokio::time::timeout(
        Duration::from_secs(10),
        first.workflow_pause(&created.workflow_id, created.generation),
    )
    .await
    .expect("workflow pause timed out")
    .expect("pause workflow");
    assert_eq!(paused.status, WorkflowStatus::Paused);
    assert_eq!(paused.todo.phases[0].tasks[0].content, "resume after process exit");
    tokio::time::timeout(Duration::from_secs(10), first.cleanup())
        .await
        .expect("initial workflow application cleanup timed out");
    drop(registration);

    let (second, _factory, _registration) = tokio::time::timeout(
        Duration::from_secs(10),
        app(&f, Vec::new()),
    )
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
    assert_eq!(created.todo.phases[0].tasks[0].content, "exercise real public lifecycle");
    assert_eq!(application.workflow_list().len(), 1);
    assert_eq!(application.workflow_get(&created.workflow_id).expect("show workflow").name, "lifecycle");
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
    assert_eq!(resumed.todo.phases[0].tasks[0].content, "exercise real public lifecycle");

    let cancelled = tokio::time::timeout(
        Duration::from_secs(10),
        application.workflow_cancel(&created.workflow_id, created.generation),
    )
    .await
    .expect("workflow cancel timed out")
    .expect("cancel workflow");
    assert_eq!(cancelled.status, WorkflowStatus::Cancelled);
    assert!(path.exists(), "cancel preserves the owned worktree until explicit remove");

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

#[test]
fn workflow_repository_namespace_is_stable_and_repo_scoped() {
    let first = Fixture::new();
    let second = Fixture::new();
    let first_manager = pi_coding::workflow_worktree::WorkflowWorktreeManager::new(&first.repo);
    let second_manager = pi_coding::workflow_worktree::WorkflowWorktreeManager::new(&second.repo);
    let first_namespace = first_manager.repository_namespace().expect("first namespace");
    assert_eq!(
        first_namespace,
        first_manager.repository_namespace().expect("stable namespace")
    );
    assert_eq!(first_namespace.len(), 32);
    assert!(first_namespace.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert_ne!(
        first_namespace,
        second_manager.repository_namespace().expect("second namespace")
    );
}

#[test]
fn session_namespace_is_stable_session_scoped_and_repo_scoped() {
    let first = Fixture::new();
    let second = Fixture::new();
    let first_manager = pi_coding::workflow_worktree::WorkflowWorktreeManager::new(&first.repo);
    let second_manager = pi_coding::workflow_worktree::WorkflowWorktreeManager::new(&second.repo);

    // Deterministic: the same session id always resolves to the same namespace.
    let session_a = first_manager.session_namespace("session-a");
    assert_eq!(session_a, first_manager.session_namespace("session-a"));
    // Session-scoped: a different session id in the same repository differs.
    assert_ne!(session_a, first_manager.session_namespace("session-b"));
    // Repo-scoped: the same session id in a different repository differs.
    assert_ne!(session_a, second_manager.session_namespace("session-a"));
    // The namespace embeds the repository digest.
    let repo_namespace = first_manager.repository_namespace().expect("repo namespace");
    assert!(session_a.starts_with(&format!("{repo_namespace}/")));
    // Foreign/unsafe session ids are encoded filesystem-safely AND
    // collision-free: the separator-mapped form carries a digest
    // disambiguator, so `a/b\c:d` never collapses onto a literal `a-b-c-d`.
    let unsafe_ns = first_manager.session_namespace("a/b\\c:d");
    let disambiguator = unsafe_ns
        .strip_prefix(&format!("{repo_namespace}/a-b-c-d-"))
        .expect("unsafe session id must map to the separator form plus a digest");
    assert_eq!(disambiguator.len(), 6, "digest length: {unsafe_ns}");
    assert!(
        disambiguator.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "digest must be hex: {unsafe_ns}"
    );
    assert_eq!(
        first_manager.session_namespace("a-b-c-d"),
        format!("{repo_namespace}/a-b-c-d"),
        "a separator-free id passes through unchanged"
    );
    assert_ne!(unsafe_ns, first_manager.session_namespace("a-b-c-d"));
}

#[tokio::test]
async fn shared_agent_root_keeps_session_workflows_isolated() {
    let f = Fixture::new();
    let shared = tempfile::tempdir().expect("shared agent root");
    let manager = pi_coding::workflow_worktree::WorkflowWorktreeManager::new(&f.repo);
    let session_a_namespace = manager.session_namespace("session-a");
    let session_b_namespace = manager.session_namespace("session-b");
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
    let application_a = Application::new(session_a).await;
    application_a
        .attach_runtime_factory(Arc::new(TestFactory { snapshot: snapshot_a }))
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
    let running = wait_status(&application_a, &created.workflow_id, WorkflowStatus::Running).await;
    assert_eq!(running.todo.phases[0].tasks[0].content, "remain session isolated");
    application_a.cleanup().await;
    drop(session_a_registration);

    // Session B (different id, same repo) sees an EMPTY workflow list.
    let (session_b, session_b_registration) = parent_session(&f.repo, Vec::new());
    let snapshot_b = session_b.child_session_options_snapshot();
    let application_b = Application::new(session_b).await;
    application_b
        .attach_runtime_factory(Arc::new(TestFactory { snapshot: snapshot_b }))
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
    let application_a_again = Application::new(session_a_again).await;
    application_a_again
        .attach_runtime_factory(Arc::new(TestFactory { snapshot: snapshot_a_again }))
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
    assert_eq!(restored.todo.phases[0].tasks[0].content, "remain session isolated");
    application_a_again.cleanup().await;
    drop(session_a_again_registration);
}

#[tokio::test]
async fn rebind_to_new_session_isolates_and_rebind_to_same_id_restores() {
    let f = Fixture::new();
    let shared = tempfile::tempdir().expect("shared agent root");
    let manager = pi_coding::workflow_worktree::WorkflowWorktreeManager::new(&f.repo);
    let session_a_namespace = manager.session_namespace("session-a");
    let session_b_namespace = manager.session_namespace("session-b");
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
    let application = Application::new(session).await;
    application
        .attach_runtime_factory(Arc::new(TestFactory { snapshot }))
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
    assert_eq!(running.todo.phases[0].tasks[0].content, "rebind session isolation");

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
    assert_eq!(restored.todo.phases[0].tasks[0].content, "rebind session isolation");
    application.cleanup().await;
    drop(registration);
}

#[tokio::test]
async fn shared_agent_root_keeps_repository_workflows_isolated() {
    let first = Fixture::new();
    let second = Fixture::new();
    let shared = tempfile::tempdir().expect("shared agent root");
    let first_namespace = pi_coding::workflow_worktree::WorkflowWorktreeManager::new(&first.repo)
        .repository_namespace()
        .expect("first namespace");
    let second_namespace = pi_coding::workflow_worktree::WorkflowWorktreeManager::new(&second.repo)
        .repository_namespace()
        .expect("second namespace");

    let (first_session, first_registration) = parent_session(
        &first.repo,
        vec![
            planning("remain isolated"),
            FauxResponse::text("planned"),
        ],
    );
    let first_snapshot = first_session.child_session_options_snapshot();
    let first_application = Application::new(first_session).await;
    first_application.attach_runtime_factory(Arc::new(TestFactory { snapshot: first_snapshot })).expect("first factory");
    first_application.setup_workflows(
        &first.repo,
        shared.path().join("workflows").join(&first_namespace),
        shared.path().join("worktrees").join(&first_namespace),
    ).await.expect("first setup");
    let created = first_application.workflow_create(WorkflowCreateRequest {
        name: "first-repo".into(),
        objective: "remain isolated".into(),
    }).await.expect("create first workflow");
    let running = wait_status(&first_application, &created.workflow_id, WorkflowStatus::Running).await;
    assert_eq!(running.todo.phases[0].tasks[0].content, "remain isolated");
    first_application.cleanup().await;
    drop(first_registration);

    let (second_session, second_registration) = parent_session(&second.repo, Vec::new());
    let second_snapshot = second_session.child_session_options_snapshot();
    let second_application = Application::new(second_session).await;
    second_application.attach_runtime_factory(Arc::new(TestFactory { snapshot: second_snapshot })).expect("second factory");
    second_application.setup_workflows(
        &second.repo,
        shared.path().join("workflows").join(&second_namespace),
        shared.path().join("worktrees").join(&second_namespace),
    ).await.expect("second setup");
    assert!(second_application.workflow_list().is_empty());
    second_application.cleanup().await;
    drop(second_registration);

    let (restored_session, restored_registration) = parent_session(&first.repo, Vec::new());
    let restored_snapshot = restored_session.child_session_options_snapshot();
    let restored_application = Application::new(restored_session).await;
    restored_application.attach_runtime_factory(Arc::new(TestFactory { snapshot: restored_snapshot })).expect("restore factory");
    restored_application.setup_workflows(
        &first.repo,
        shared.path().join("workflows").join(&first_namespace),
        shared.path().join("worktrees").join(&first_namespace),
    ).await.expect("restore setup");
    let restored = restored_application.workflow_get(&created.workflow_id).expect("first workflow restored");
    // restore_all auto-continues the restored Running runtime: it must not
    // come back frozen.
    assert_eq!(restored.status, WorkflowStatus::Running);
    assert_eq!(restored.todo.phases[0].tasks[0].content, "remain isolated");
    restored_application.cleanup().await;
    drop(restored_registration);
}

#[tokio::test]
async fn non_git_setup_lists_and_create_fails_safely() -> Result<()> {
    let sandbox = tempfile::tempdir()?; let source = sandbox.path().join("plain"); fs::create_dir(&source)?; let (session, registration) = parent_session(&source, Vec::new()); let snapshot = session.child_session_options_snapshot(); let application = Application::new(session).await;
    application.attach_runtime_factory(Arc::new(TestFactory { snapshot }))?; application.setup_workflows(&source, sandbox.path().join("state"), sandbox.path().join("managed")).await?; assert!(application.workflow_list().is_empty());
    let error = application.workflow_create(WorkflowCreateRequest { name: "no-git".into(), objective: "safe".into() }).await.expect_err("create"); assert_eq!(error.to_string(), "workflow runtime creation failed"); application.cleanup().await; drop(registration); Ok(())
}

#[tokio::test]
async fn workflow_child_todo_mutation_does_not_auto_spawn_jobs() {
    let f = Fixture::new();
    // Objective wording avoids the typed-agent delegation router (P0-C): a
    // delegation verb (`spawn`, `run`, `make`, ...) immediately followed by an
    // agent-name-shaped noun would fail actionably as a missing agent. The
    // contract under test here is BUG-1 — a workflow-child Todo mutation must
    // never auto-arm the DAG — not agent delegation, so the objective is
    // worded to stay inert under delegation parsing.
    let objective = "child todo mutation must not auto-arm the dag";
    let (application, factory, _registration) = app(
        &f,
        vec![planning(objective), FauxResponse::text("planned")],
    )
    .await;
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
    // P0-2: a committed plan explicitly arms Todo DAG execution (the
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
    // supervisor owns DAG execution for workflow children (BUG-1), so the
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
                blocked_by: Vec::new(),
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
        "a workflow-child Todo mutation must never auto-spawn task jobs (BUG-1): {:?}",
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
    let (first, _factory, registration) = app(
        &f,
        vec![planning(objective), FauxResponse::text("planned")],
    )
    .await;
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
    let restored = second.workflow_get(&created.workflow_id).expect("restored workflow");
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
    let (first, _factory, registration) = app(
        &f,
        vec![planning(objective), FauxResponse::text("planned")],
    )
    .await;
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
    let restored = second.workflow_get(&created.workflow_id).expect("restored workflow");
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
    assert!(alpha_path.exists(), "alpha worktree must exist while running");
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
    assert_ne!(alpha_path, beta_path, "concurrent workflows must own distinct worktrees");
    assert_eq!(application.workflow_list().len(), 2);
    assert_eq!(alpha.todo.phases[0].tasks[0].content, "alpha feature");
    assert_eq!(beta.todo.phases[0].tasks[0].content, "beta feature");

    // Settle alpha first: the completed DAG auto-integrates into the source.
    application
        .set_workflow_todos(&alpha.workflow_id, completed_todo("alpha feature"))
        .expect("settle alpha DAG");
    let alpha = wait_integrated(&application, &alpha.workflow_id).await;
    assert_eq!(alpha.status, WorkflowStatus::Completed);
    assert!(matches!(alpha.integration, WorkflowIntegration::Applied { .. }));
    assert_eq!(
        fs::read_to_string(f.repo.join("alpha.txt")).expect("merged alpha"),
        "alpha\n"
    );

    // Alpha's integration must not disturb beta: beta stays Running, its
    // worktree survives, and its commit is not merged into the source.
    let beta_mid = application.workflow_get(&beta.workflow_id).expect("beta mid");
    assert_eq!(beta_mid.status, WorkflowStatus::Running);
    assert!(beta_path.exists(), "alpha's integration must not touch beta's worktree");
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
    assert!(matches!(beta.integration, WorkflowIntegration::Applied { .. }));
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
    assert!(beta_path.exists(), "beta worktree must survive alpha's removal");
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
        error.to_string().contains("workflow lifecycle transition is not allowed"),
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
    assert!(matches!(completed.integration, WorkflowIntegration::Applied { .. }));

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
            error.to_string().contains("workflow lifecycle transition is not allowed"),
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
    let still = application.workflow_get(&completed.workflow_id).expect("workflow");
    assert_eq!(still.status, WorkflowStatus::Completed);
    assert!(matches!(still.integration, WorkflowIntegration::Applied { .. }));
    application.cleanup().await;
}

/// Contract (P0-B): the workflow supervisor carries the objective's explicit
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
    let application = Application::new(session).await;
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
    assert_eq!(running.todo.phases[0].tasks[0].content, "仔细调研pi-coding-agent");
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
        typed.agent, "researcher",
        "the paraphrased plan must still route to researcher through the typed agent field; all child jobs: {:?}",
        jobs.iter()
            .map(|job| (job.agent.as_str(), job.agent_id.as_str(), job.todo_task_id.as_deref(), job.status))
            .collect::<Vec<_>>()
    );
    // The worker child's id is the generated Todo-slot id; only the routed
    // agent name must equal the typed role.
    assert_eq!(typed.agent_id, "Todo1");
    application.cleanup().await;
    drop(registration);
}

/// Contract (P0-C): a workflow objective that explicitly delegates to an
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
        vec![planning("让task处理这个任务"), FauxResponse::text("planned")],
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
    assert_eq!(running.todo.phases[0].tasks[0].content, "让task处理这个任务");
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
                blocked_by: Vec::new(),
                agent: None,
            }],
        }])
        .expect("seed main session todos");
    assert_eq!(application.todo_state().phases[0].tasks[0].id, "parent-only");

    // The workflow plans and completes inside its own child session.
    let created = application
        .workflow_create(WorkflowCreateRequest {
            name: "isolated".into(),
            objective: objective.into(),
        })
        .await
        .expect("create workflow");
    let completed =
        wait_status(&application, &created.workflow_id, WorkflowStatus::Completed).await;

    // The workflow's todo is its own plan — it never inherited the main
    // session's tasks.
    assert_eq!(
        completed.todo.phases[0].tasks[0].content,
        objective,
        "workflow todo must be its own plan, not the parent's tasks"
    );
    assert!(
        completed
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
    let (application, factory, _registration) = app(
        &f,
        vec![planning(objective), FauxResponse::text("planned")],
    )
    .await;
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
            blocked_by: Vec::new(),
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
                .filter(|job| job.todo_task_id.as_deref().is_some_and(|id| ids.contains(&id)))
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
            jobs
                .iter()
                .map(|job| (job.agent_id.as_str(), job.status, job.todo_task_id.as_deref()))
                .collect::<Vec<_>>()
        );
    });
    let jobs = child.orchestration_runtime().expect("child orchestration").jobs(None);
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
            .filter(|job| job.todo_task_id.as_deref().is_some_and(|id| ids.contains(&id)))
            .all(|job| job.workflow_id.as_deref() == Some(created.workflow_id.as_str())),
        "wave jobs must stay workflow-scoped"
    );
    application.cleanup().await;
}
