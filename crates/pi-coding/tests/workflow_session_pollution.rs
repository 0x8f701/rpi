//! Regression: workflow/session producers must keep their session records out
//! of the default native session store (the Web sidebar catalog source).
//!
//! Historical bug: workflow test factories recorded the supervisor/parent
//! session and its durable children into the DEFAULT store
//! (`~/.pi/agent/sessions/--<encoded-cwd>--`), so after running the
//! workflow/session suites the real `session_list` / Web sidebar showed
//! supervisor, clone, retry, and internal rows no user created. The fix
//! pattern: every producer calls `Session::set_session_dir(<isolated temp
//! root>)` BEFORE `record()` / `start_new_recording()`, so produced rows land
//! in the temp root and the default store is untouched.
//!
//! This test runs the REAL workflow lifecycle (create -> planning -> worker
//! delegation -> complete) and then asserts:
//!   - the run really produced session records (non-vacuous): a supervisor
//!     parent file plus at least one durable child under the isolated temp
//!     root;
//!   - NO produced session path falls under `pi_coding::native_sessions_root()`
//!     (the default store the sidebar catalogs) — if any producer drops
//!     `set_session_dir`, its rows land there and this test fails;
//!   - the produced files are ordinary v3 session records with the workflow
//!     cwd and parent lineage — exactly the rows that would have polluted the
//!     sidebar had they leaked.

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
    ApplicationRuntimeFactory, ApplicationRuntimeFuture, ChildSessionOptionsSnapshot,
    OrchestrationConfig, OrchestrationRuntime, Session, SessionOptions, WorkflowCreateRequest,
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

/// The workflow worker agent. Matches the canonical `workflow_application`
/// fixture: exactly the `bash` tool so scripted worker turns can do real file
/// work in the owned worktree.
fn definition() -> AgentDefinition {
    AgentDefinition {
        name: "task".into(),
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
        path: None,
        trusted: true,
        kind: pi_coding::AgentDefinitionKind::Agent,
        personality: None,
        soft_budget: None,
    }
}

/// Per-process isolated native session root for workflow test sessions. This
/// test owns its own root (its own process) so the before/after enumeration
/// is exclusive to this test — parallel-safe via `LazyLock`.
fn test_sessions_root() -> PathBuf {
    static ROOT: std::sync::LazyLock<tempfile::TempDir> = std::sync::LazyLock::new(|| {
        tempfile::tempdir().expect("test sessions root")
    });
    ROOT.path().to_path_buf()
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
            let session = Session::new_with_todo_additional_tools_filtered_discovery_workspace_and_uri(
                options,
                Vec::new(),
                Default::default(),
                pi_coding::ResourceDiscovery::Disabled,
                workspace,
                None,
            )?;
            session.set_session_dir(test_sessions_root());
            session.start_new_recording()?;
            let artifacts = std::env::temp_dir()
                .join(format!("pi-workflow-test-artifacts-{}", uuid::Uuid::now_v7()));
            let mut config = OrchestrationConfig::new(
                AgentCatalog::from_agents(vec![definition()]),
                artifacts,
            );
            config.parent_model = snapshot.model.clone();
            config.max_concurrency = pi_coding::DEFAULT_MAX_CONCURRENCY;
            config.idle_ttl = None;
            let orchestration =
                OrchestrationRuntime::new(config, OrchestrationRuntime::child_factory_from_snapshot(snapshot))?;
            Ok(ApplicationRuntimeCandidate::new(session).with_orchestration(orchestration))
        })
    }
}

fn parent_session(repo: &Path, responses: Vec<FauxResponse>) -> (Session, FauxProviderRegistration) {
    let suffix = uuid::Uuid::now_v7().to_string();
    let model = Model {
        id: format!("workflow-pollution-{suffix}"),
        name: "Workflow Pollution Test".into(),
        api: format!("workflow-pollution-api-{suffix}"),
        provider: format!("workflow-pollution-provider-{suffix}"),
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
    session.set_session_dir(test_sessions_root());
    (session, registration)
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

/// Scripted responses for the real supervisor-driven lifecycle: the planning
/// turn builds the 2-task Todo DAG and settles on the plain-text turn (the
/// plan-commit stop hook arms DAG execution), the armed DAG spawns one worker
/// job per ready task, worker A does the real git work in its owned worktree
/// and worker B runs a read-only `git status`, and each settled worker job
/// marks its owned task Done — settling the DAG into Completed.
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

async fn wait_status(
    application: &Application,
    workflow_id: &pi_coding::WorkflowId,
    status: WorkflowStatus,
) -> pi_coding::WorkflowSnapshot {
    tokio::time::timeout(Duration::from_secs(20), async {
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

/// All `.jsonl` session files under `root`, recursively.
fn session_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
                out.push(path);
            }
        }
    }
    out
}

/// Regression: after a REAL workflow run, the produced supervisor + durable
/// child session records live under the isolated temp root — never under the
/// default native session store the Web sidebar catalogs.
#[tokio::test]
async fn workflow_producer_sessions_never_enter_default_store() {
    let f = Fixture::new();
    let (application, _factory, _registration) = app(&f, full_lifecycle_responses()).await;
    let root = test_sessions_root();
    let default_store = pi_coding::native_sessions_root();

    // Run the real lifecycle: create -> planning (2-task Todo DAG) -> armed
    // DAG spawns worker jobs -> workers do real git work -> all tasks done ->
    // completed.
    let created = application
        .workflow_create(WorkflowCreateRequest {
            name: "pollution-guard".into(),
            objective: "ship the feature".into(),
        })
        .await
        .expect("workflow creation must succeed");
    let completed = wait_status(&application, &created.workflow_id, WorkflowStatus::Completed).await;
    assert_eq!(completed.status, WorkflowStatus::Completed);

    // Non-vacuous: the run must have produced session records (a supervisor
    // parent file plus at least one durable child) — otherwise the isolation
    // assertion guards nothing.
    let produced = session_files(&root);
    assert!(
        produced.len() >= 2,
        "workflow run must produce >=2 session records under the temp root, got {}: {produced:?}",
        produced.len()
    );
    let has_child_lineage = produced.iter().any(|path| {
        let header = read_first_line(path);
        header.get("type").and_then(serde_json::Value::as_str) == Some("session")
            && header.get("version").and_then(serde_json::Value::as_u64) == Some(3)
            && header.get("parentSession").is_some()
    });
    assert!(
        has_child_lineage,
        "at least one produced session must carry parent lineage (the durable child shape): {produced:?}"
    );

    // THE regression: NO produced session path may fall under the default
    // native store (the Web sidebar catalog source). A producer that forgets
    // `set_session_dir` writes there and fails this assertion.
    for path in &produced {
        assert!(
            !path.starts_with(&default_store),
            "produced session leaked into the default native store: {path:?} (default store: {default_store:?})"
        );
        assert!(
            path.starts_with(&root),
            "produced session outside the isolated temp root: {path:?} (root: {root:?})"
        );
    }

    // Every produced file is an ordinary v3 session header whose cwd stays
    // inside the test sandbox (worktree or repo) — i.e. exactly the rows
    // that would have polluted the sidebar had they leaked into the default
    // store, and never rows pointing at any real workspace.
    let sandbox = fs::canonicalize(f.sandbox.path()).expect("canonical sandbox");
    for path in &produced {
        let header = read_first_line(path);
        assert_eq!(
            header.get("type").and_then(serde_json::Value::as_str),
            Some("session"),
            "produced file is not a session record: {path:?}"
        );
        assert_eq!(
            header.get("version").and_then(serde_json::Value::as_u64),
            Some(3),
            "produced session is not version 3: {path:?}"
        );
        let cwd = header.get("cwd").and_then(serde_json::Value::as_str).expect("session cwd");
        assert!(
            Path::new(cwd).starts_with(&sandbox),
            "produced session cwd escapes the test sandbox: {cwd:?} (sandbox: {sandbox:?})"
        );
    }
}

fn read_first_line(path: &Path) -> serde_json::Value {
    let content = fs::read_to_string(path).expect("read session file");
    let first = content.lines().next().expect("session file is empty");
    serde_json::from_str(first).expect("session header is JSON")
}
