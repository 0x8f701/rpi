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
    let output = Command::new("git").args(["-c", "color.ui=false", "-c", "commit.gpgsign=false", "-c", "init.defaultBranch=main"])
        .args(args).current_dir(cwd).env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_TERMINAL_PROMPT", "0").env("LC_ALL", "C").output().expect("git");
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).expect("utf8").trim().to_owned()
}
fn commit_file(cwd: &Path, relative: &str, contents: &str, message: &str) -> String {
    fs::write(cwd.join(relative), contents).expect("write"); git(cwd, &["add", "--", relative]); git(cwd, &["commit", "-m", message]); git(cwd, &["rev-parse", "HEAD"])
}
fn definition() -> AgentDefinition { AgentDefinition { name: "task".into(), description: "workflow worker".into(), system_prompt: "complete workflow Todo".into(), tools: Some(Vec::new()), autoload_skills: Vec::new(), model: None, thinking_level: Some(ThinkingLevel::Off), source: AgentDefinitionSource::Bundled, path: None, trusted: true } }

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
fn parent_session(repo: &Path, responses: Vec<FauxResponse>) -> (Session, FauxProviderRegistration) {
    let suffix = uuid::Uuid::now_v7().to_string(); let model = Model { id: format!("workflow-{suffix}"), name: "Workflow Test".into(), api: format!("workflow-api-{suffix}"), provider: format!("workflow-provider-{suffix}"), ..Model::default() };
    let registration = register_faux_provider(FauxProviderOptions { api: model.api.clone(), provider: model.provider.clone(), models: vec![model.clone()], chunk_size: 32 }); registration.set_responses(responses);
    let session = Session::new(SessionOptions { model, cwd: repo.to_path_buf(), system_prompt: String::new(), thinking_level: ThinkingLevel::Off, api_key: "faux".into(), compaction: None, stream_options: Default::default(), tools: Some(Vec::new()), before_tool_call: None, after_tool_call: None, stream_fn: None, auth_resolver: None }).expect("session");
    (session, registration)
}
fn planning(objective: &str) -> FauxResponse { FauxResponse { content: vec![ContentBlock::ToolCall(ToolCall { id: "todo-init".into(), name: "todo".into(), arguments: serde_json::json!({"op":"init","items":[objective]}), thought_signature: None })], stop_reason: StopReason::ToolUse, error_message: None } }
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

#[tokio::test]
async fn create_isolates_child_runtime() {
    let f = Fixture::new(); let objective = "delegate task to ship feature"; let (application, _factory, _registration) = app(&f, vec![planning(objective), FauxResponse::text("planned"), FauxResponse::text("worker done")]).await;
    let created = application.workflow_create(WorkflowCreateRequest { name: "ship".into(), objective: objective.into() }).await.expect("create");
    let created = wait_status(&application, &created.workflow_id, WorkflowStatus::Completed).await;
    let path = worktree(&f, created.workflow_id.as_str()); assert!(path.exists() && !path.starts_with(&f.repo)); assert!(created.branch.as_deref().is_some_and(|b| b.starts_with("rpi/workflow/")));
    assert_eq!(created.todo.phases[0].tasks[0].content, objective); application.cleanup().await;
}

#[tokio::test]
async fn plain_text_planning_survives_and_later_todo_mutation_runs() {
    let f = Fixture::new();
    let (application, factory, _registration) = app(
        &f,
        vec![
            FauxResponse::text("planning acknowledged"),
            FauxResponse::text("worker response ".repeat(4_096)),
        ],
    )
    .await;
    let created = application
        .workflow_create(WorkflowCreateRequest {
            name: "deferred-plan".into(),
            objective: "plan before Todo mutation".into(),
        })
        .await
        .expect("plain-text planning must create the workflow");
    assert_eq!(created.status, WorkflowStatus::Planning);
    assert!(created.todo.phases.is_empty());

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
            }],
        }])
        .expect("canonical workflow Todo mutation");
    assert_eq!(result.phases[0].tasks[0].id, "later-root");
    assert!(application.todo_state().phases.is_empty(), "parent Todo must remain unchanged");
    let child = factory.child_application(&created.workflow_id, created.generation).expect("exact workflow child application");
    let jobs = child.orchestration_runtime().expect("child orchestration").jobs(None);
    assert!(jobs.iter().any(|job| job.todo_task_id.as_deref() == Some("later-root") && job.workflow_id.as_deref() == Some(created.workflow_id.as_str()) && job.workflow_generation == Some(created.generation)), "workflow Todo jobs must preserve exact ownership: {jobs:?}");
    let running = wait_status(&application, &created.workflow_id, WorkflowStatus::Running).await;
    assert_eq!(running.todo.phases[0].tasks[0].id, "later-root");
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
async fn provider_planning_error_still_rolls_back_create_safely() {
    let f = Fixture::new();
    let secret = "upstream api_key=workflow-secret";
    let (application, _factory, _registration) = app(&f, vec![FauxResponse::error(secret)]).await;
    let error = application
        .workflow_create(WorkflowCreateRequest {
            name: "provider-error".into(),
            objective: "must not survive provider failure".into(),
        })
        .await
        .expect_err("provider failure must fail workflow creation");
    assert_eq!(error.to_string(), "workflow runtime creation failed");
    assert!(!error.to_string().contains(secret));
    assert!(application.workflow_list().is_empty());
    application.cleanup().await;
}

#[tokio::test]
async fn workflow_detail_is_generation_gated_and_terminal_safe() {
    let f = Fixture::new(); let objective = "detail projection"; let (application, _factory, _registration) = app(&f, vec![planning(objective), FauxResponse::text("planned"), FauxResponse::text("worker done")]).await;
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
async fn restore_adopts_committed_head() {
    let f = Fixture::new(); let (first, _factory, registration) = app(&f, vec![planning("restore task"), FauxResponse::text("planned"), FauxResponse::text("worker")]).await;
    let created = first.workflow_create(WorkflowCreateRequest { name: "restore".into(), objective: "restore task".into() }).await.expect("create"); let created = wait_status(&first, &created.workflow_id, WorkflowStatus::Completed).await; let path = worktree(&f, created.workflow_id.as_str()); let head = commit_file(&path, "restored.txt", "workflow\n", "progress"); first.cleanup().await; drop(registration);
    let (second, _factory, _registration) = app(&f, Vec::new()).await; let restored = second.workflow_get(&created.workflow_id).expect("restore"); assert_eq!(restored.status, WorkflowStatus::Completed); assert_eq!(restored.todo.phases, created.todo.phases); assert_eq!(git(&path, &["rev-parse", "HEAD"]), head); second.cleanup().await;
}

#[tokio::test]
async fn integrate_remove_and_conflict_retention() {
    let f = Fixture::new(); let (application, _factory, _registration) = app(&f, vec![planning("integrate"), FauxResponse::text("planned"), FauxResponse::text("worker")]).await;
    let created = application.workflow_create(WorkflowCreateRequest { name: "integrate".into(), objective: "integrate".into() }).await.expect("create"); let created = wait_status(&application, &created.workflow_id, WorkflowStatus::Completed).await; let path = worktree(&f, created.workflow_id.as_str()); commit_file(&path, "feature.txt", "integrated\n", "feature");
    let integrated = application.workflow_integrate(&created.workflow_id, created.generation).await.expect("integrate"); assert_eq!(integrated.status, WorkflowStatus::Completed); assert!(matches!(integrated.integration, WorkflowIntegration::Applied { .. })); assert_eq!(fs::read_to_string(f.repo.join("feature.txt")).expect("feature"), "integrated\n"); application.workflow_remove(&created.workflow_id, integrated.generation).await.expect("remove"); assert!(!path.exists()); application.cleanup().await;

    let f = Fixture::new(); let (application, _factory, _registration) = app(&f, vec![planning("conflict"), FauxResponse::text("planned"), FauxResponse::text("worker")]).await;
    let created = application.workflow_create(WorkflowCreateRequest { name: "conflict".into(), objective: "conflict".into() }).await.expect("create"); let created = wait_status(&application, &created.workflow_id, WorkflowStatus::Completed).await; let path = worktree(&f, created.workflow_id.as_str()); commit_file(&path, "README.md", "workflow side\n", "workflow"); commit_file(&f.repo, "README.md", "source side\n", "source");
    let conflicted = application.workflow_integrate(&created.workflow_id, created.generation).await.expect("conflict"); assert_eq!(conflicted.status, WorkflowStatus::Conflicted); assert!(matches!(&conflicted.integration, WorkflowIntegration::Conflicted { conflicts } if conflicts.as_slice() == [String::from("README.md")])); assert!(path.exists()); application.cleanup().await;
}

#[tokio::test]
async fn cleanup_preserves_paused_workflow_for_restore() {
    let f = Fixture::new();
    let (first, _factory, registration) = app(
        &f,
        vec![FauxResponse::text("planning acknowledged")],
    )
    .await;
    let created = first
        .workflow_create(WorkflowCreateRequest {
            name: "paused-restore".into(),
            objective: "resume after process exit".into(),
        })
        .await
        .expect("create workflow");
    let paused = first
        .workflow_pause(&created.workflow_id, created.generation)
        .await
        .expect("pause workflow");
    assert_eq!(paused.status, WorkflowStatus::Paused);
    first.cleanup().await;
    drop(registration);

    let (second, _factory, _registration) = app(&f, Vec::new()).await;
    assert_eq!(
        second
            .workflow_get(&created.workflow_id)
            .expect("restored workflow")
            .status,
        WorkflowStatus::Paused
    );
    second.cleanup().await;
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
        vec![FauxResponse::text("planning acknowledged")],
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
    assert_eq!(created.status, WorkflowStatus::Planning);
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
    assert_eq!(
        restored_application.workflow_get(&created.workflow_id).expect("first workflow restored").status,
        WorkflowStatus::Planning
    );
    restored_application.cleanup().await;
    drop(restored_registration);
}

#[tokio::test]
async fn non_git_setup_lists_and_create_fails_safely() -> Result<()> {
    let sandbox = tempfile::tempdir()?; let source = sandbox.path().join("plain"); fs::create_dir(&source)?; let (session, registration) = parent_session(&source, Vec::new()); let snapshot = session.child_session_options_snapshot(); let application = Application::new(session).await;
    application.attach_runtime_factory(Arc::new(TestFactory { snapshot }))?; application.setup_workflows(&source, sandbox.path().join("state"), sandbox.path().join("managed")).await?; assert!(application.workflow_list().is_empty());
    let error = application.workflow_create(WorkflowCreateRequest { name: "no-git".into(), objective: "safe".into() }).await.expect_err("create"); assert_eq!(error.to_string(), "workflow runtime creation failed"); application.cleanup().await; drop(registration); Ok(())
}
