//! End-to-end workflow full-lifecycle test at the pi-coding Application level.
//!
//! Covers the STABLE workflow contract end to end:
//!   create -> planning (todo tool builds a 2-task DAG) -> subagent execution
//!   (task-tool delegations with faux workers completing real work) ->
//!   all todos done -> completed -> integrate (the source repo receives the
//!   merge) -> remove (auto-cancels a non-terminal leftover first),
//! plus a resume/restore scenario (T43 session namespace): durable records
//! exist under the session-scoped store, the application is dropped, and a
//! rebuild with the SAME session id restores the workflow and auto-continues
//! the restored Running runtime (T26 RestoreContinue re-arms Todo DAG
//! execution over the stored tasks).
//!
//! Deterministic by construction: faux provider responses are scripted (the
//! todo-done calls and the scripted bash work are consumed before any worker
//! session exists, so there is nothing to race), the repo/worktrees/state all
//! live in a temp sandbox, and every wait is bounded. The integrate assertion
//! accepts EITHER the manual integrate step (stable contract) OR the
//! auto-integration the manager runs the moment the DAG settles into
//! Completed (T53) — both end with the merged change present in the source
//! repo, and the test reports which one it observed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use pi_agent::ThinkingLevel;
use pi_ai::providers::{
    FauxProviderOptions, FauxProviderRegistration, FauxResponse, register_faux_provider,
};
use pi_ai::{ContentBlock, Model, StopReason, ToolCall};
use pi_coding::{
    AgentCatalog, AgentDefinition, AgentDefinitionSource, Application, ApplicationRuntimeCandidate,
    ApplicationRuntimeFactory, ApplicationRuntimeFuture, ChildSessionOptionsSnapshot, JobStatus,
    OrchestrationConfig, OrchestrationRuntime, Session, SessionOptions, TodoStatus,
    WorkflowCreateRequest, WorkflowIntegration, WorkflowStatus, WorkspaceRoots,
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

/// The workflow worker agent. It gets exactly the `bash` tool so scripted
/// worker turns can do real file work in the owned worktree; every other
/// harness behavior matches the canonical `workflow_application` fixture.
fn definition() -> AgentDefinition {
    AgentDefinition { name: "task".into(),
    description: "workflow worker".into(),
    system_prompt: "complete workflow Todo".into(),
    tools: Some(vec!["bash".to_owned()]),
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

#[derive(Clone)]
struct TestFactory {
    snapshot: ChildSessionOptionsSnapshot,
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
        Box::pin(async move {
            options.cwd = cwd.path().to_path_buf();
            options.tools = None;
            let workspace = WorkspaceRoots::new(cwd.path(), Vec::<PathBuf>::new())?;
            let artifacts = std::env::temp_dir()
                .join(format!("pi-workflow-test-artifacts-{}", uuid::Uuid::now_v7()));
            let mut config =
                OrchestrationConfig::new(AgentCatalog::from_agents(vec![definition()]), artifacts);
            config.parent_model = snapshot.model.clone();
            config.max_concurrency = pi_coding::DEFAULT_MAX_CONCURRENCY;
            config.idle_ttl = None;
            // Workers execute in the OWNED workflow worktree (mirroring the
            // real CLI blueprint), not the parent session's directory: the
            // workflow's subagent execution must land files and commits in
            // the worktree the manager auto-integrates.
            let mut worker_snapshot = snapshot.clone();
            worker_snapshot.cwd = cwd.path().to_path_buf();
            let orchestration = OrchestrationRuntime::new(
                config,
                OrchestrationRuntime::child_factory_from_snapshot(worker_snapshot),
            )?;
            // Mirror the real CLI blueprint (session_run_blueprint.rs): the
            // workflow supervisor session carries the orchestration tools
            // (task delegation + hub IRC), so scripted task-tool calls really
            // spawn faux worker jobs instead of erroring with an unknown tool.
            let additional_tools = orchestration.agent_tools("Main", 0);
            let session = Session::new_with_todo_additional_tools_filtered_discovery_workspace_and_uri(
                options,
                additional_tools,
                Default::default(),
                pi_coding::ResourceDiscovery::Disabled,
                workspace,
                None,
            )?;
            session.start_new_recording()?;
            Ok(ApplicationRuntimeCandidate::new(session).with_orchestration(orchestration))
        })
    }
}

fn parent_session(repo: &Path, responses: Vec<FauxResponse>) -> (Session, FauxProviderRegistration) {
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
    let session = Session::new(SessionOptions {
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
    })
    .expect("session");
    (session, registration)
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

fn planning_with_items(items: &[&str]) -> FauxResponse {
    FauxResponse {
        content: vec![ContentBlock::ToolCall(ToolCall {
            id: "todo-init".into(),
            name: "todo".into(),
            arguments: serde_json::json!({"op":"init","items": items}),
            thought_signature: None,
        })],
        stop_reason: StopReason::ToolUse,
        error_message: None,
    }
}

fn bash_call(id: &str, command: &str) -> FauxResponse {
    FauxResponse {
        content: vec![ContentBlock::ToolCall(ToolCall {
            id: id.into(),
            name: "bash".into(),
            arguments: serde_json::json!({"command": command}),
            thought_signature: None,
        })],
        stop_reason: StopReason::ToolUse,
        error_message: None,
    }
}

/// Scripted responses for the full supervisor-driven lifecycle with two
/// tasks under the plan/execution phase split. Ordering is deterministic by
/// construction:
///  1. The planning turn builds the 2-task DAG (todo init) and then settles
///     on the plain-text turn — the plan-commit stop hook terminates the
///     planning run the moment the model leaves Todo-land, so the workflow
///     arms DAG execution and moves to Running immediately (P0-2).
///  2. The armed DAG spawns one worker job per ready task. Worker A does the
///     real work: it writes both feature files and makes the single git
///     commit, so the owned worktree is committed and clean by the time the
///     DAG settles — a hard requirement now that the manager auto-integrates
///     the worktree the moment the DAG settles into Completed (T53): the
///     merge must never race in-flight worker git operations (shared index
///     lock) or uncommitted files. Worker B only runs a read-only `git
///     status` (always exit 0), so the two concurrent workers never contend
///     on the git index.
///  3. Each settled worker job makes the supervisor mark its owned Todo task
///     Done (execution-supervision), settling the DAG into Completed.
fn full_lifecycle_responses() -> Vec<FauxResponse> {
    vec![
        planning_with_items(&["create hello.txt", "commit hello.txt"]),
        FauxResponse::text("plan complete"),
        bash_call(
            "bash-work",
            "printf 'hello world\\n' > hello.txt && printf 'bye\\n' > bye.txt && git add hello.txt bye.txt && git -c user.name='Pi Test' -c user.email='pi@example.test' -c commit.gpgsign=false commit -m 'add feature files'",
        ),
        FauxResponse::text("worker one done"),
        bash_call("bash-verify", "git status --porcelain"),
        FauxResponse::text("worker two done"),
    ]
}

async fn app_with_roots(
    f: &Fixture,
    responses: Vec<FauxResponse>,
    store_root: &Path,
    managed_root: &Path,
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
        .setup_workflows(&f.repo, store_root, managed_root)
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
    app_with_roots(f, responses, &f.state, &f.managed).await
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
    tokio::time::timeout(Duration::from_secs(10), async {
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

#[tokio::test]
async fn full_lifecycle_create_plan_execute_complete_integrate_remove() {
    let f = Fixture::new();
    let (application, factory, _registration) = app(&f, full_lifecycle_responses()).await;
    let base_commit = git(&f.repo, &["rev-parse", "HEAD"]);

    // create: the workflow is durably created as Queued and the supervisor
    // starts planning asynchronously (no plan yet).
    let created = application
        .workflow_create(WorkflowCreateRequest {
            name: "ship".into(),
            objective: "ship the feature".into(),
        })
        .await
        .expect("workflow creation must succeed");
    assert_eq!(created.status, WorkflowStatus::Queued);
    assert!(created.todo.phases.is_empty(), "no plan exists before planning");

    // planning + subagent execution + completion: the supervisor's planning
    // turn builds the 2-task Todo DAG and stops once the plan is committed
    // (P0-2); the armed DAG then spawns worker jobs per task, the workers do
    // the real file/commit work, and each settled job makes the supervisor
    // mark its owned task Done.
    let completed = wait_status(&application, &created.workflow_id, WorkflowStatus::Completed).await;
    assert_eq!(completed.todo.phases.len(), 1);
    let tasks = &completed.todo.phases[0].tasks;
    assert_eq!(tasks.len(), 2, "planning must produce exactly the 2 scripted tasks");
    assert!(
        tasks.iter().all(|task| task.status == TodoStatus::Completed),
        "all planned tasks must be completed, got {:?}",
        tasks
    );
    let path = worktree(&f, completed.workflow_id.as_str());
    assert!(path.exists() && !path.starts_with(&f.repo));
    assert!(completed
        .branch
        .as_deref()
        .is_some_and(|branch| branch.starts_with("rpi/workflow/")));

    // Subagent execution evidence: the DAG-armed execution spawned
    // workflow-scoped worker jobs that settle (bounded wait).
    let child = factory
        .child_application(&created.workflow_id, created.generation)
        .expect("exact workflow child application");
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let jobs = child
                .orchestration_runtime()
                .expect("child orchestration")
                .jobs(None);
            if jobs.iter().any(|job| {
                job.workflow_id.as_deref() == Some(created.workflow_id.as_str())
                    && job.workflow_generation == Some(created.generation)
                    && job.status.is_settled()
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("task-tool delegation jobs must settle");

    // The delegated work landed in the owned worktree and is committed and
    // clean on the workflow branch (the manager auto-integrates at Completed,
    // so the worktree must never be dirty at that point — T53).
    assert_eq!(
        fs::read_to_string(path.join("hello.txt")).expect("worktree hello"),
        "hello world\n"
    );
    assert_eq!(
        fs::read_to_string(path.join("bye.txt")).expect("worktree bye"),
        "bye\n"
    );
    let worktree_head = git(&path, &["rev-parse", "HEAD"]);
    assert_ne!(worktree_head, base_commit, "the feature work must be committed");
    assert_eq!(
        git(&path, &["status", "--porcelain"]),
        "",
        "the worktree must be clean when the workflow completes"
    );

    // integrate: the source repo receives the merge. The manager auto-
    // integrates when the DAG settles into Completed (T53) — wait briefly for
    // that to land if it is going to — and the explicit integrate call below
    // is idempotent when the branch is already merged (worktree HEAD == repo
    // HEAD → Applied). Both the auto-integrated and the manual-integrate
    // paths end with the merged change present in the source repo.
    let auto_integrated = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = application
                .workflow_get(&created.workflow_id)
                .expect("workflow");
            if snapshot.integration != WorkflowIntegration::None {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .is_ok();
    eprintln!(
        "[workflow_full_e2e] integrate step observed: {}",
        if auto_integrated {
            "auto-integrated at Completed (T53)"
        } else {
            "manual integrate (stable contract)"
        }
    );
    let integrated = application
        .workflow_integrate(&completed.workflow_id, completed.generation)
        .await
        .expect("integrate completed workflow");
    assert!(
        matches!(integrated.integration, WorkflowIntegration::Applied { .. }),
        "integration must be applied, got {:?}",
        integrated.integration
    );
    assert_eq!(
        fs::read_to_string(f.repo.join("hello.txt")).expect("merged hello"),
        "hello world\n"
    );
    assert_eq!(
        fs::read_to_string(f.repo.join("bye.txt")).expect("merged bye"),
        "bye\n"
    );

    // remove: the completed workflow and its owned worktree are removed.
    let removed = application
        .workflow_remove(&completed.workflow_id, completed.generation)
        .await
        .expect("remove completed workflow");
    assert_eq!(removed.workflow_id, completed.workflow_id);
    assert!(application.workflow_list().is_empty());
    assert!(!path.exists(), "remove must delete the owned worktree");
    application.cleanup().await;
}

#[tokio::test]
async fn remove_auto_cancels_non_terminal_leftover() {
    let f = Fixture::new();
    let objective = "leave behind a running workflow";
    let (application, _factory, _registration) = app(
        &f,
        vec![planning(objective), FauxResponse::text("planned")],
    )
    .await;
    let created = application
        .workflow_create(WorkflowCreateRequest {
            name: "auto-cancel".into(),
            objective: objective.into(),
        })
        .await
        .expect("create workflow");
    let running = wait_status(&application, &created.workflow_id, WorkflowStatus::Running).await;
    assert_eq!(running.todo.phases[0].tasks[0].content, objective);
    let path = worktree(&f, created.workflow_id.as_str());
    assert!(path.exists());

    // A direct remove of a non-terminal (Running) workflow auto-cancels first:
    // the runtime is cancelled, then the record and owned worktree are removed.
    let removed = application
        .workflow_remove(&created.workflow_id, created.generation)
        .await
        .expect("remove auto-cancels non-terminal leftover");
    assert_eq!(removed.status, WorkflowStatus::Cancelled);
    assert!(application.workflow_list().is_empty());
    assert!(!path.exists());
    application.cleanup().await;
}

#[tokio::test]
async fn resume_same_session_namespace_restores_and_continues() {
    let f = Fixture::new();
    let shared = tempfile::tempdir().expect("shared agent root");
    // T43: the session namespace scopes the durable store and managed
    // worktrees. Rebuilding the application with the SAME session id resolves
    // the same namespace, so restore_all finds the persisted workflow.
    let namespace = pi_coding::workflow_worktree::WorkflowWorktreeManager::new(&f.repo)
        .session_namespace("resume-session");
    let store_root = shared.path().join("workflows").join(&namespace);
    let managed_root = shared.path().join("worktrees").join(&namespace);
    let objective = "resume after relaunch and continue";

    // First incarnation: the supervisor plans a 1-task DAG and the workflow
    // reaches Running with the task still open (the supervisor owns DAG
    // execution and has not delegated it yet).
    let (first, _factory, registration) = app_with_roots(
        &f,
        vec![planning(objective), FauxResponse::text("planned")],
        &store_root,
        &managed_root,
    )
    .await;
    let created = first
        .workflow_create(WorkflowCreateRequest {
            name: "resume-e2e".into(),
            objective: objective.into(),
        })
        .await
        .expect("create workflow");
    let running = wait_status(&first, &created.workflow_id, WorkflowStatus::Running).await;
    assert_eq!(running.todo.phases[0].tasks[0].content, objective);
    let task_id = running.todo.phases[0].tasks[0].id.clone();

    // The workflow is durably persisted under the session-scoped store, and
    // its owned worktree survives application teardown.
    let record = store_root
        .join("records")
        .join(format!("{}.json", created.workflow_id.as_str()));
    assert!(
        record.exists(),
        "durable workflow record must exist under the session-scoped store"
    );
    let record_text = fs::read_to_string(&record).expect("read workflow record");
    assert!(
        record_text.contains(objective),
        "workflow record must carry the objective"
    );
    let worktree_path = managed_root
        .join(pi_coding::workflow_worktree::WORKTREE_ROOT_DIR_NAME)
        .join(created.workflow_id.as_str());
    assert!(worktree_path.exists(), "owned worktree must survive teardown");

    first.cleanup().await;
    drop(registration);

    // Second incarnation with the SAME session id namespace: restore_all
    // brings the workflow back and auto-continues the restored Running
    // runtime (T26 RestoreContinue re-arms Todo DAG execution over the
    // stored tasks). The restored runtime is not frozen: it either still
    // shows Running (the re-armed DAG's worker is in flight) or has already
    // settled to Completed once the worker finished and the supervisor
    // marked the owned task Done (execution-supervision). Either way the
    // task is the stored one — never lost, never re-planned.
    let (second, factory, _registration) = app_with_roots(
        &f,
        vec![FauxResponse::text("resumed worker done")],
        &store_root,
        &managed_root,
    )
    .await;
    let restored = second
        .workflow_get(&created.workflow_id)
        .expect("restored workflow");
    assert!(
        matches!(
            restored.status,
            WorkflowStatus::Running | WorkflowStatus::Completed
        ),
        "restored workflow must be Running or Completed, got {:?}",
        restored.status
    );
    assert_eq!(restored.todo.phases[0].tasks[0].content, objective);
    assert_eq!(restored.todo.phases[0].tasks[0].id, task_id);

    // The resumed execution really runs: a workflow-scoped job for the open
    // task is spawned by the re-armed DAG and settles (bounded wait).
    let child = factory
        .child_application(&created.workflow_id, created.generation)
        .expect("exact workflow child application");
    let settled = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let jobs = child
                .orchestration_runtime()
                .expect("child orchestration")
                .jobs(None);
            if let Some(job) = jobs.iter().find(|job| {
                job.workflow_id.as_deref() == Some(created.workflow_id.as_str())
                    && job.workflow_generation == Some(created.generation)
                    && job.todo_task_id.as_deref() == Some(task_id.as_str())
                    && job.status.is_settled()
            }) {
                break job.clone();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("restored Running workflow must re-arm DAG execution (T26 RestoreContinue)");
    assert_eq!(settled.status, JobStatus::Completed);
    assert_eq!(settled.agent, "task");

    // The restored workflow completes only when the DAG truly settles: the
    // settled worker job makes the supervisor mark the task Done, settling
    // the DAG into Completed.
    let completed = wait_status(&second, &created.workflow_id, WorkflowStatus::Completed).await;
    assert_eq!(
        completed.todo.phases[0].tasks[0].status,
        TodoStatus::Completed,
        "the completed workflow must carry the settled task"
    );
    second.cleanup().await;
}

/// Contract: a workflow exited MID-PLANNING survives process teardown and
/// recovers (the U7 case). The first incarnation exits the instant the
/// durable record is observed in Planning — while the initial planning turn
/// is still streaming — so the workflow never completes before exit. The
/// durable record (Queued/Planning, or Running with the open planned task if
/// the fast faux planner raced past planning) is restored in the SAME session
/// namespace: a restored Planning/Queued runtime re-runs the bounded planning
/// flow and commits the plan (P0-2), a restored Running runtime re-arms Todo
/// DAG execution (T26); the armed DAG spawns a worker for the open task whose
/// settlement the supervisor marks Done (execution-supervision). Either path
/// lands the workflow in Running or Completed with the planned DAG rebuilt
/// and a settling task job — the workflow is never lost.
#[tokio::test]
async fn restart_mid_planning_restores_workflow_and_planning_continues() {
    let f = Fixture::new();
    let shared = tempfile::tempdir().expect("shared agent root");
    let namespace = pi_coding::workflow_worktree::WorkflowWorktreeManager::new(&f.repo)
        .session_namespace("mid-planning-session");
    let store_root = shared.path().join("workflows").join(&namespace);
    let managed_root = shared.path().join("worktrees").join(&namespace);
    let objective = "survive an exit during planning";

    // First incarnation: the initial planning turn streams a large plain-text
    // reply (chunked by the faux provider), so the workflow sits durably in
    // Planning for a while; the trailing todo-init + plan text would finish
    // planning if we stayed.
    let (first, _factory, registration) = app_with_roots(
        &f,
        vec![
            FauxResponse::text("x".repeat(256 * 1024)),
            planning(objective),
            FauxResponse::text("planned"),
        ],
        &store_root,
        &managed_root,
    )
    .await;
    let created = first
        .workflow_create(WorkflowCreateRequest {
            name: "mid-plan".into(),
            objective: objective.into(),
        })
        .await
        .expect("create workflow");

    // Exit the instant the workflow is observed mid-planning (U7): the record
    // is Planning while the turn is in flight (or Queued/Running if the faux
    // planner already raced ahead) — never Completed.
    let mid = wait_status(&first, &created.workflow_id, WorkflowStatus::Planning).await;
    assert_ne!(mid.status, WorkflowStatus::Completed, "must exit before completion");
    first.cleanup().await;
    drop(registration);

    // The durable record survives teardown under the session-scoped store.
    let record = store_root
        .join("records")
        .join(format!("{}.json", created.workflow_id.as_str()));
    assert!(
        record.exists(),
        "durable workflow record must exist after mid-planning exit"
    );
    let record_text = fs::read_to_string(&record).expect("read workflow record");
    assert!(
        record_text.contains(objective),
        "workflow record must carry the objective"
    );
    let exit_status = serde_json::from_str::<serde_json::Value>(&record_text)
        .expect("record json")["record"]["status"]
        .as_str()
        .expect("record status")
        .to_owned();
    assert!(
        matches!(exit_status.as_str(), "queued" | "planning" | "running"),
        "unexpected durable status after mid-planning exit: {exit_status}"
    );
    eprintln!("[workflow_full_e2e] mid-planning exit left durable status: {exit_status}");

    // Second incarnation with the SAME session namespace. A restored
    // Planning/Queued record re-runs the bounded planning flow: the replanning
    // turn builds the DAG (todo-init) and settles on the plain-text turn
    // (P0-2 plan-commit), then the DAG is armed for execution. A restored
    // Running record re-arms Todo DAG execution (T26). Either way the armed
    // DAG spawns one worker for the open task, which settles on the plain
    // text reply; the supervisor marks the task Done on settlement.
    let responses = match exit_status.as_str() {
        "queued" | "planning" => vec![
            planning(objective),
            FauxResponse::text("plan complete"),
            FauxResponse::text("resumed worker done"),
        ],
        _ => vec![FauxResponse::text("resumed worker done")],
    };
    let (second, factory, _registration) =
        app_with_roots(&f, responses, &store_root, &managed_root).await;

    let restored = second
        .workflow_get(&created.workflow_id)
        .expect("restored workflow");
    assert_eq!(restored.workflow_id, created.workflow_id);
    // The recovered runtime reaches Running (while the task is open) or
    // Completed (once the settled worker lets the supervisor mark the task
    // Done and the DAG settles) — both are valid recovery outcomes.
    let outcome = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let snapshot = second
                .workflow_get(&created.workflow_id)
                .expect("workflow");
            if matches!(
                snapshot.status,
                WorkflowStatus::Running | WorkflowStatus::Completed
            ) {
                break snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("restored workflow must reach Running or Completed");
    assert_eq!(
        outcome.todo.phases[0].tasks[0].content, objective,
        "planning must rebuild the planned DAG after restart"
    );
    if outcome.status == WorkflowStatus::Completed {
        assert!(
            outcome
                .todo
                .phases
                .iter()
                .flat_map(|phase| &phase.tasks)
                .all(|task| task.status == TodoStatus::Completed),
            "a Completed restore must have every planned task done"
        );
    }

    // The recovered execution really runs: a workflow-scoped task job settles
    // (bounded wait) — the DAG-armed worker on both the Running re-arm path
    // and the re-planned path.
    let child = factory
        .child_application(&created.workflow_id, created.generation)
        .expect("exact workflow child application");
    let settled = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let jobs = child
                .orchestration_runtime()
                .expect("child orchestration")
                .jobs(None);
            if let Some(job) = jobs.iter().find(|job| {
                job.workflow_id.as_deref() == Some(created.workflow_id.as_str())
                    && job.workflow_generation == Some(created.generation)
                    && job.status.is_settled()
            }) {
                break job.clone();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("restored workflow must continue executing and settle its task job");
    assert_eq!(settled.status, JobStatus::Completed);
    assert_eq!(settled.agent, "task");
    second.cleanup().await;
}

/// Contract: the workflow store is scoped to the session namespace — a
/// rebuild with a DIFFERENT session namespace (T43) must not see workflows
/// recorded by another session, and the other session's durable record and
/// owned worktree must remain untouched on disk.
#[tokio::test]
async fn restart_different_session_namespace_isolates_workflows() {
    let f = Fixture::new();
    let shared = tempfile::tempdir().expect("shared agent root");
    let namespace_a = pi_coding::workflow_worktree::WorkflowWorktreeManager::new(&f.repo)
        .session_namespace("session-a");
    let store_a = shared.path().join("workflows").join(&namespace_a);
    let managed_a = shared.path().join("worktrees").join(&namespace_a);
    let namespace_b = pi_coding::workflow_worktree::WorkflowWorktreeManager::new(&f.repo)
        .session_namespace("session-b");
    let store_b = shared.path().join("workflows").join(&namespace_b);
    let managed_b = shared.path().join("worktrees").join(&namespace_b);
    let objective = "private to session a";

    // Session A creates a workflow that reaches Running.
    let (first, _factory, registration) = app_with_roots(
        &f,
        vec![planning(objective), FauxResponse::text("planned")],
        &store_a,
        &managed_a,
    )
    .await;
    let created = first
        .workflow_create(WorkflowCreateRequest {
            name: "session-a-only".into(),
            objective: objective.into(),
        })
        .await
        .expect("create workflow");
    let running = wait_status(&first, &created.workflow_id, WorkflowStatus::Running).await;
    assert_eq!(running.todo.phases[0].tasks[0].content, objective);
    first.cleanup().await;
    drop(registration);

    // The durable record and owned worktree live under session A's namespace.
    let record_a = store_a
        .join("records")
        .join(format!("{}.json", created.workflow_id.as_str()));
    assert!(record_a.exists(), "session A record must exist");
    let worktree_a = managed_a
        .join(pi_coding::workflow_worktree::WORKTREE_ROOT_DIR_NAME)
        .join(created.workflow_id.as_str());
    assert!(worktree_a.exists(), "session A worktree must survive teardown");

    // Session B (a DIFFERENT namespace) sees an empty workflow store and
    // cannot resolve session A's workflow at all.
    let (second, _factory, _registration) =
        app_with_roots(&f, Vec::new(), &store_b, &managed_b).await;
    assert!(
        second.workflow_list().is_empty(),
        "session B must not see session A workflows"
    );
    let missing = second.workflow_get(&created.workflow_id);
    assert!(missing.is_err(), "session B must not resolve session A workflows");
    assert!(!store_b.join("records").exists() || fs::read_dir(store_b.join("records")).expect("records b").next().is_none());
    second.cleanup().await;

    // Session A's data is untouched: still visible from its own namespace.
    let record_text = fs::read_to_string(&record_a).expect("read session A record");
    assert!(record_text.contains(objective), "session A record must be intact");
    assert!(worktree_a.exists(), "session A worktree must be intact");
}

/// Contract: a workflow that completed AND was integrated before process
/// teardown stays Completed with its merge applied across a restart in the
/// same session namespace — the durable record and the merged source repo
/// both survive, and restore never re-arms a terminal workflow.
#[tokio::test]
async fn restart_after_integrate_keeps_completed_workflow() {
    let f = Fixture::new();
    let shared = tempfile::tempdir().expect("shared agent root");
    let namespace = pi_coding::workflow_worktree::WorkflowWorktreeManager::new(&f.repo)
        .session_namespace("completed-session");
    let store_root = shared.path().join("workflows").join(&namespace);
    let managed_root = shared.path().join("worktrees").join(&namespace);

    // First incarnation: the full lifecycle through Completed + integrate.
    let (first, _factory, registration) = app_with_roots(
        &f,
        full_lifecycle_responses(),
        &store_root,
        &managed_root,
    )
    .await;
    let created = first
        .workflow_create(WorkflowCreateRequest {
            name: "shipped".into(),
            objective: "ship the feature".into(),
        })
        .await
        .expect("create workflow");
    let completed = wait_status(&first, &created.workflow_id, WorkflowStatus::Completed).await;
    let integrated = first
        .workflow_integrate(&completed.workflow_id, completed.generation)
        .await
        .expect("integrate completed workflow");
    assert!(
        matches!(integrated.integration, WorkflowIntegration::Applied { .. }),
        "integration must be applied, got {:?}",
        integrated.integration
    );
    assert_eq!(
        fs::read_to_string(f.repo.join("hello.txt")).expect("merged hello"),
        "hello world\n"
    );
    first.cleanup().await;
    drop(registration);

    // Second incarnation with the SAME session namespace: the workflow is
    // still listed as Completed with its merge applied, and the source repo
    // keeps the merged change.
    let (second, _factory, _registration) =
        app_with_roots(&f, Vec::new(), &store_root, &managed_root).await;
    let restored = second
        .workflow_get(&created.workflow_id)
        .expect("restored completed workflow");
    assert_eq!(restored.status, WorkflowStatus::Completed);
    assert_eq!(restored.todo.phases[0].tasks[0].content, "create hello.txt");
    assert_eq!(restored.todo.phases[0].tasks[0].status, TodoStatus::Completed);
    assert!(
        matches!(restored.integration, WorkflowIntegration::Applied { .. }),
        "restored integration must stay applied, got {:?}",
        restored.integration
    );
    assert_eq!(second.workflow_list().len(), 1);
    assert_eq!(
        fs::read_to_string(f.repo.join("hello.txt")).expect("merged hello after restart"),
        "hello world\n"
    );
    assert_eq!(
        fs::read_to_string(f.repo.join("bye.txt")).expect("merged bye after restart"),
        "bye\n"
    );
    second.cleanup().await;
}
