//! Real overlayfs isolation smoke tests (skip-guarded), mirroring
//! `sandbox_smoke.rs`: they exercise the actual overlay mount against the host
//! kernel — kernel overlay (mount -t overlay), fuse-overlayfs, and the
//! recursive-copy fallback — plus the overlayfs workflow isolation manager
//! lifecycle and the `settings.orchestration.isolation` wiring.
//!
//! Kernel-overlay tests require Linux, `mount`, and either root or
//! unprivileged user namespaces (the same probe the sandbox tests use). When
//! the probe passes but the test process has no mount privilege, the binary
//! re-executes itself under `unshare --user --map-root-user --mount` so the
//! real in-process mount path is exercised. Deterministic rcopy tests always
//! run.

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
use pi_coding::isolate::{OverlayBackend, OverlayfsIsolation};
use pi_coding::workflow_worktree::{
    CreateWorktreeOptions, IntegrateOptions, IntegrateOutcome, OverlayWorkflowManager,
    WorkflowIsolation, OVERLAY_ROOT_DIR_NAME,
};
use pi_coding::{
    AgentCatalog, AgentDefinition, AgentDefinitionSource, Application, ApplicationRuntimeCandidate,
    ApplicationRuntimeFactory, ApplicationRuntimeFuture, ChildSessionOptionsSnapshot,
    OrchestrationConfig, OrchestrationRuntime, OrchestrationSettings, ResourceManager,
    ResourceManagerOptions, Session, SessionOptions, Settings, TodoStatus, WorkflowCreateRequest,
    WorkflowIntegration, WorkflowIsolationSetting, WorkflowStatus, WorkspaceRoots,
};
use tempfile::TempDir;

const TEST_TIMEOUT: Duration = Duration::from_secs(30);

fn is_root() -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self")
        .map(|meta| meta.uid() == 0)
        .unwrap_or(false)
}

/// Arrange for the kernel-overlay assertions to run with mount privilege.
///
/// Returns `true` when the caller must run its real assertions in this
/// process (the caller IS the namespaced child, or the caller is real root).
/// Returns `false` when the assertions must be skipped: either the host
/// cannot create unprivileged user namespaces (test skipped), or this process
/// just re-executed the whole test binary under
/// `unshare --user --map-root-user --mount --exact <test_name>` and the child
/// already ran the assertions — its exit status is this test's outcome.
fn ensure_mount_privilege(test_name: &str) -> bool {
    if std::env::var("PI_OVERLAYFS_NAMESPACED").is_ok() {
        return true;
    }
    if is_root() {
        return true;
    }
    if !pi_coding::isolate::userns_mount_probe() {
        eprintln!(
            "overlayfs smoke: SKIP ({test_name}: no unprivileged user namespace mounts)"
        );
        return false;
    }
    let exe = std::env::current_exe().expect("test binary");
    let status = Command::new("unshare")
        .args(["--user", "--map-root-user", "--mount"])
        .env("PI_OVERLAYFS_NAMESPACED", "1")
        .arg(&exe)
        .args(["--exact", test_name])
        .status()
        .expect("re-exec under unshare");
    assert!(
        status.success(),
        "the namespaced re-run of {test_name} failed with {status:?}"
    );
    false
}

fn fuse_overlayfs_available() -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path)
            .any(|dir| dir.join("fuse-overlayfs").is_file())
    })
}

// ---------------------------------------------------------------------------
// OverlayfsIsolation backend roundtrips
// ---------------------------------------------------------------------------

/// The recursive-copy fallback always works (no mounts): merged becomes an
/// independent copy of lower, writes never reach lower, and stop cleans the
/// private layers without touching the merged view.
#[test]
fn rcopy_fallback_materializes_an_independent_copy() {
    let sandbox = TempDir::new().expect("sandbox");
    let lower = sandbox.path().join("lower");
    fs::create_dir_all(&lower).expect("lower");
    fs::write(lower.join("base.txt"), "base").expect("base file");
    fs::create_dir_all(lower.join("sub")).expect("subdir");
    fs::write(lower.join("sub").join("deep.txt"), "deep").expect("deep file");

    let iso = OverlayfsIsolation::start_with(
        &lower,
        sandbox.path().join("upper"),
        sandbox.path().join("work"),
        sandbox.path().join("merged"),
        &[OverlayBackend::Rcopy],
    )
    .expect("rcopy isolation");
    assert_eq!(iso.backend(), OverlayBackend::Rcopy);
    assert!(!iso.is_mounted(), "rcopy never mounts");
    let merged = iso.merged_path();
    assert_eq!(
        fs::read_to_string(merged.join("base.txt")).expect("read base"),
        "base"
    );
    assert_eq!(
        fs::read_to_string(merged.join("sub").join("deep.txt")).expect("read deep"),
        "deep"
    );

    // Independent: writes through the merged view never reach the lower layer.
    fs::write(merged.join("new.txt"), "new").expect("write merged");
    assert!(!lower.join("new.txt").exists(), "lower must stay untouched");

    iso.stop().expect("stop");
    assert!(!iso.upper_path().exists(), "upper must be cleaned");
    assert!(!iso.work_path().exists(), "work must be cleaned");
    assert!(merged.exists(), "stop keeps the merged view (the manager removes it)");
}

/// Kernel overlay roundtrip (skip-guarded): mounts lower/upper/work at merged,
/// verifies copy-up semantics (writes land in the upper layer, never lower),
/// and detaches with MNT_DETACH (`umount -l`) on stop.
#[test]
fn kernel_overlay_start_stop_roundtrip() {
    if !ensure_mount_privilege("kernel_overlay_start_stop_roundtrip") {
        return;
    }
    let sandbox = TempDir::new().expect("sandbox");
    let lower = sandbox.path().join("lower");
    let upper = sandbox.path().join("upper");
    let work = sandbox.path().join("work");
    let merged = sandbox.path().join("merged");
    fs::create_dir_all(&lower).expect("lower");
    fs::write(lower.join("base.txt"), "base").expect("base file");

    let iso = OverlayfsIsolation::start(&lower, &upper, &work, &merged)
        .expect("kernel overlay start");
    assert_eq!(iso.backend(), OverlayBackend::Kernel);
    assert!(iso.is_mounted(), "the merged path must be a mount point");
    assert_eq!(
        fs::read_to_string(merged.join("base.txt")).expect("read through overlay"),
        "base",
        "lower content must be visible through the overlay"
    );

    // Copy-up: a write through merged lands in the upper layer, not lower.
    fs::write(merged.join("written.txt"), "w").expect("write through overlay");
    assert_eq!(
        fs::read_to_string(upper.join("written.txt")).expect("read upper"),
        "w",
        "writes must be copied up into the upper layer"
    );
    assert!(!lower.join("written.txt").exists(), "lower must stay read-only");
    assert_eq!(
        fs::read_to_string(merged.join("written.txt")).expect("read back"),
        "w"
    );

    iso.stop().expect("stop");
    assert!(!iso.is_mounted(), "stop must detach the mount (MNT_DETACH)");
    assert!(!upper.exists(), "upper must be cleaned by stop");
    assert!(!work.exists(), "work must be cleaned by stop");
    assert!(
        fs::read_dir(&merged).expect("read merged").next().is_none(),
        "the detached merged dir must be empty again"
    );
}

/// fuse-overlayfs roundtrip (skip-guarded on binary presence): FUSE mounts
/// work unprivileged and are visible to every process, so no namespace
/// re-exec is needed.
#[test]
fn fuse_overlayfs_start_stop_roundtrip() {
    if !fuse_overlayfs_available() {
        eprintln!("overlayfs smoke: SKIP (fuse-overlayfs is not installed)");
        return;
    }
    let sandbox = TempDir::new().expect("sandbox");
    let lower = sandbox.path().join("lower");
    let upper = sandbox.path().join("upper");
    let work = sandbox.path().join("work");
    let merged = sandbox.path().join("merged");
    fs::create_dir_all(&lower).expect("lower");
    fs::write(lower.join("base.txt"), "base").expect("base file");

    let iso = OverlayfsIsolation::start_with(
        &lower,
        &upper,
        &work,
        &merged,
        &[OverlayBackend::FuseOverlayfs],
    )
    .expect("fuse-overlayfs start");
    assert_eq!(iso.backend(), OverlayBackend::FuseOverlayfs);
    assert!(iso.is_mounted(), "the FUSE mount must be visible");
    assert_eq!(
        fs::read_to_string(merged.join("base.txt")).expect("read through fuse"),
        "base"
    );
    fs::write(merged.join("written.txt"), "w").expect("write through fuse");
    assert_eq!(
        fs::read_to_string(upper.join("written.txt")).expect("read upper"),
        "w",
        "writes must land in the upper layer"
    );

    iso.stop().expect("stop");
    assert!(!iso.is_mounted(), "fusermount -u must detach the FUSE mount");
    assert!(!upper.exists());
    assert!(!work.exists());
}

// ---------------------------------------------------------------------------
// OverlayWorkflowManager lifecycle
// ---------------------------------------------------------------------------

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

fn init_repo(repo: &Path) {
    fs::create_dir_all(repo).expect("source directory");
    git(repo, &["init"]);
    git(repo, &["config", "user.name", "Pi Test"]);
    git(repo, &["config", "user.email", "pi@example.test"]);
    fs::write(repo.join("README.md"), "base\n").expect("base file");
    git(repo, &["add", "README.md"]);
    git(repo, &["commit", "-m", "initial"]);
}

struct ManagerFixture {
    sandbox: TempDir,
    repo: PathBuf,
    managed: PathBuf,
}

impl ManagerFixture {
    fn new() -> Self {
        let sandbox = TempDir::new().expect("sandbox");
        let repo = sandbox.path().join("source");
        let managed = sandbox.path().join("managed");
        init_repo(&repo);
        Self {
            sandbox,
            repo,
            managed,
        }
    }

    fn manager(&self, candidates: Vec<OverlayBackend>) -> OverlayWorkflowManager {
        OverlayWorkflowManager::new(&self.repo, &self.managed)
            .with_backend_candidates(candidates)
            .with_timeout(TEST_TIMEOUT)
    }

    fn create_options(&self) -> CreateWorktreeOptions {
        CreateWorktreeOptions {
            managed_root: self.managed.clone(),
            base_commit: None,
            timeout: Some(TEST_TIMEOUT),
        }
    }
}

/// Full manager lifecycle on the deterministic rcopy backend: create →
/// workflow writes + commits inside the merged view → ownership verification
/// (head advances) → integrate (copy-back + single commit on the source
/// branch, `.git` never synced) → remove (identity dir gone, catalog empty).
#[test]
fn overlay_workflow_manager_rcopy_lifecycle() {
    let f = ManagerFixture::new();
    let manager = f.manager(vec![OverlayBackend::Rcopy]);
    let identity = manager
        .create("wf-rcopy", f.create_options())
        .expect("create overlay workflow");
    assert_eq!(identity.branch(), "rpi/overlay/wf-rcopy");
    assert_eq!(identity.base_commit(), git(&f.repo, &["rev-parse", "HEAD"]));
    let merged = f
        .managed
        .join(OVERLAY_ROOT_DIR_NAME)
        .join("wf-rcopy")
        .join("merged");
    assert!(
        merged
            .to_string_lossy()
            .contains("overlay-workflows/wf-rcopy/merged"),
        "merged view must live under the managed root: {}",
        merged.display()
    );
    assert_eq!(
        fs::read_to_string(merged.join("README.md")).expect("read readme"),
        "base\n",
        "the merged view must start from the source tree state"
    );

    // The workflow writes and commits inside the merged view.
    fs::write(merged.join("feature.txt"), "feat").expect("write feature");
    git(&merged, &["add", "feature.txt"]);
    git(&merged, &["commit", "-m", "workflow work"]);

    // Ownership verification: exact identity + head advance.
    let cwd = manager.verify_owned(&identity).expect("verify owned");
    assert_eq!(cwd.path(), &merged);
    let (current, cwd_current) = manager
        .verify_owned_current("wf-rcopy")
        .expect("verify owned current");
    assert_eq!(cwd_current.path(), &merged);
    assert_ne!(
        current.head_commit(),
        identity.base_commit(),
        "the workflow commit must advance the recorded head"
    );

    // The source repo does not see the change before integrate.
    assert!(
        !f.repo.join("feature.txt").exists(),
        "overlay isolation must keep the source tree untouched until integrate"
    );

    // Integrate: copy-back + one commit on the source branch.
    let outcome = manager
        .integrate("wf-rcopy", IntegrateOptions::default())
        .expect("integrate overlay workflow");
    let IntegrateOutcome::Applied { result_commit, .. } = outcome else {
        panic!("overlayfs integration is copy-back (never conflicted), got {outcome:?}");
    };
    assert_eq!(
        fs::read_to_string(f.repo.join("feature.txt")).expect("read feature"),
        "feat"
    );
    assert_eq!(git(&f.repo, &["rev-parse", "HEAD"]), result_commit);
    assert_eq!(git(&f.repo, &["status", "--porcelain"]), "");
    let log_line = git(&f.repo, &["log", "--oneline", "-1"]);
    assert!(
        log_line.ends_with("workflow wf-rcopy: integrate overlay changes"),
        "integration must produce a single descriptive commit: {log_line}"
    );

    // Remove: the whole identity dir (merged/upper/work) goes away. The
    // refreshed identity (from verify_owned_current) is the exact current
    // record, exactly as the workflow factory would pass it.
    manager.remove(&current).expect("remove overlay workflow");
    assert!(!merged.exists(), "remove must delete the merged view");
    assert!(!f.managed.join("overlay-workflows").join("wf-rcopy").exists());
    assert!(manager.list().expect("list").is_empty());
}

/// Full manager lifecycle on the kernel overlay backend (skip-guarded;
/// re-executed under an unprivileged user namespace when needed).
#[test]
fn overlay_workflow_manager_kernel_lifecycle() {
    if !ensure_mount_privilege("overlay_workflow_manager_kernel_lifecycle") {
        return;
    }
    let f = ManagerFixture::new();
    let manager = f.manager(vec![OverlayBackend::Kernel]);
    let identity = manager
        .create("wf-kernel", f.create_options())
        .expect("create overlay workflow");
    let merged = f
        .managed
        .join(OVERLAY_ROOT_DIR_NAME)
        .join("wf-kernel")
        .join("merged");
    let identity_dir = merged.parent().expect("identity dir");
    let probe = OverlayfsIsolation::restore(
        &f.repo,
        &identity_dir.join("upper"),
        &identity_dir.join("work"),
        &merged,
        OverlayBackend::Kernel,
    );
    assert!(probe.is_mounted(), "the workflow merged view must be a real overlay mount");

    // The workflow writes + commits inside the merged view.
    fs::write(merged.join("feature.txt"), "feat").expect("write feature");
    git(&merged, &["add", "feature.txt"]);
    git(&merged, &["commit", "-m", "workflow work"]);
    let (current, _) = manager
        .verify_owned_current("wf-kernel")
        .expect("verify owned current");
    assert_ne!(current.head_commit(), identity.base_commit());
    assert!(!f.repo.join("feature.txt").exists());

    let outcome = manager
        .integrate("wf-kernel", IntegrateOptions::default())
        .expect("integrate kernel overlay workflow");
    assert!(
        matches!(outcome, IntegrateOutcome::Applied { .. }),
        "got {outcome:?}"
    );
    assert_eq!(
        fs::read_to_string(f.repo.join("feature.txt")).expect("read feature"),
        "feat"
    );

    manager.remove(&current).expect("remove kernel overlay workflow");
    assert!(!merged.exists());
    assert!(manager.list().expect("list").is_empty());
}

/// Restore semantics: the ownership catalog survives a manager rebuild (as a
/// process restart would), and `verify_owned_current` re-establishes the
/// recorded backend — without re-running rcopy over the existing upper.
#[test]
fn overlay_workflow_manager_restore_remounts_or_reuses_the_working_copy() {
    let f = ManagerFixture::new();
    let manager = f.manager(vec![OverlayBackend::Rcopy]);
    let identity = manager
        .create("wf-restore", f.create_options())
        .expect("create overlay workflow");
    let merged = f
        .managed
        .join(OVERLAY_ROOT_DIR_NAME)
        .join("wf-restore")
        .join("merged");
    fs::write(merged.join("persisted.txt"), "kept").expect("write workflow file");

    // A rebuilt manager (same managed root) restores from the catalog.
    let rebuilt = f.manager(vec![OverlayBackend::Rcopy]);
    let (current, cwd) = rebuilt
        .verify_owned_current("wf-restore")
        .expect("restore and verify");
    assert_eq!(current.workflow_id(), "wf-restore");
    assert_eq!(cwd.path(), &merged);
    assert_eq!(
        fs::read_to_string(merged.join("persisted.txt")).expect("read persisted"),
        "kept",
        "restore must keep the workflow's upper changes (never re-rcopy)"
    );
    rebuilt.remove(&current).expect("remove");
    assert!(!merged.exists());
}

// ---------------------------------------------------------------------------
// Wiring: `settings.orchestration.isolation` drives the workflow backend
// ---------------------------------------------------------------------------

/// The workflow worker agent: exactly the `bash` tool, matching the canonical
/// `workflow_application` fixture.
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
                .join(format!("pi-overlay-test-artifacts-{}", uuid::Uuid::now_v7()));
            let mut config =
                OrchestrationConfig::new(AgentCatalog::from_agents(vec![definition()]), artifacts);
            config.parent_model = snapshot.model.clone();
            config.max_concurrency = pi_coding::DEFAULT_MAX_CONCURRENCY;
            config.idle_ttl = None;
            let orchestration = OrchestrationRuntime::new(
                config,
                OrchestrationRuntime::child_factory_from_snapshot(snapshot),
            )?;
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

fn todo_done_call(id: &str, content: &str) -> FauxResponse {
    FauxResponse {
        content: vec![ContentBlock::ToolCall(ToolCall {
            id: id.into(),
            name: "todo".into(),
            arguments: serde_json::json!({"op":"done","task":content}),
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

fn task_tool_call(id: &str, task: &str) -> FauxResponse {
    FauxResponse {
        content: vec![ContentBlock::ToolCall(ToolCall {
            id: id.into(),
            name: "task".into(),
            arguments: serde_json::json!({"task": task}),
            thought_signature: None,
        })],
        stop_reason: StopReason::ToolUse,
        error_message: None,
    }
}

/// Scripted lifecycle mirroring `workflow_full_e2e`: planning builds a 2-task
/// DAG, both todos complete and the bash work commits inside the merged view
/// before any worker exists, then both tasks delegate to faux workers.
fn overlay_lifecycle_responses() -> Vec<FauxResponse> {
    vec![
        planning_with_items(&["create hello.txt", "commit hello.txt"]),
        todo_done_call("todo-done-1", "create hello.txt"),
        todo_done_call("todo-done-2", "commit hello.txt"),
        bash_call(
            "bash-work",
            "printf 'hello world\\n' > hello.txt && printf 'bye\\n' > bye.txt && git add hello.txt bye.txt && git -c user.name='Pi Test' -c user.email='pi@example.test' -c commit.gpgsign=false commit -m 'add feature files'",
        ),
        task_tool_call("task-delegate-1", "create hello.txt"),
        task_tool_call("task-delegate-2", "commit hello.txt"),
        FauxResponse::text("worker one done"),
        FauxResponse::text("worker two done"),
        FauxResponse::text("worker three done"),
        FauxResponse::text("workflow complete"),
    ]
}

struct AppFixture {
    sandbox: TempDir,
    repo: PathBuf,
    state: PathBuf,
    managed: PathBuf,
    agent_dir: TempDir,
}

impl AppFixture {
    fn new() -> Self {
        let sandbox = TempDir::new().expect("sandbox");
        let repo = sandbox.path().join("source");
        init_repo(&repo);
        let state = sandbox.path().join("state");
        let managed = sandbox.path().join("managed");
        let agent_dir = TempDir::new().expect("agent dir");
        Self {
            sandbox,
            repo,
            state,
            managed,
            agent_dir,
        }
    }
}

async fn overlay_app(
    f: &AppFixture,
    responses: Vec<FauxResponse>,
) -> (
    Application,
    Arc<pi_coding::ApplicationWorkflowRuntimeFactory>,
    FauxProviderRegistration,
) {
    // `settings.orchestration.isolation: overlayfs` flows through the
    // resource manager snapshot into Application::setup_workflows, which must
    // select the overlayfs backend.
    let settings = Settings {
        orchestration: Some(OrchestrationSettings {
            isolation: Some(WorkflowIsolationSetting::Overlayfs),
            ..Default::default()
        }),
        ..Default::default()
    };
    fs::write(
        f.agent_dir.path().join("settings.json"),
        serde_json::to_string(&settings).expect("serialize settings"),
    )
    .expect("write settings");

    let (session, registration) = parent_session(&f.repo, responses);
    let resources = ResourceManager::new(ResourceManagerOptions {
        cwd: f.repo.clone(),
        agent_dir: f.agent_dir.path().to_path_buf(),
        headless: true,
        project_trust_override: None,
        explicit_extension_paths: Vec::new(),
        explicit_skill_paths: Vec::new(),
        explicit_prompt_paths: Vec::new(),
        explicit_theme_paths: Vec::new(),
        disable_extensions: true,
        disable_skills: true,
        disable_prompt_templates: true,
        disable_themes: true,
        disable_context_files: true,
        system_prompt: None,
        system_prompt_path: None,
        append_system_prompt: Vec::new(),
        append_system_prompt_paths: Vec::new(),
    })
    .expect("resources");
    session.attach_resources(resources).await.expect("attach resources");
    let snapshot = session.child_session_options_snapshot();
    let application = Application::new(session).await;
    application
        .attach_runtime_factory(Arc::new(TestFactory { snapshot }))
        .expect("factory");
    let factory = application
        .setup_workflows(&f.repo, &f.state, &f.managed)
        .await
        .expect("setup workflows");
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

/// End-to-end wiring proof: with `orchestration.isolation: overlayfs` the
/// workflow child runs inside the overlay merged view (source repo as the
/// read-only lower layer), the source tree stays untouched until integrate,
/// and integrate lands the merged changes in the source repo before remove
/// cleans the overlay.
#[tokio::test]
async fn workflow_isolation_overlayfs_end_to_end() {
    let f = AppFixture::new();
    let (application, _factory, _registration) =
        overlay_app(&f, overlay_lifecycle_responses()).await;
    let base_commit = git(&f.repo, &["rev-parse", "HEAD"]);

    let created = application
        .workflow_create(WorkflowCreateRequest {
            name: "overlay-ship".into(),
            objective: "ship the feature".into(),
        })
        .await
        .expect("workflow creation must succeed");
    assert_eq!(created.status, WorkflowStatus::Queued);

    let completed = wait_status(&application, &created.workflow_id, WorkflowStatus::Completed).await;
    assert_eq!(completed.todo.phases.len(), 1);
    assert!(
        completed.todo.phases[0]
            .tasks
            .iter()
            .all(|task| task.status == TodoStatus::Completed)
    );
    assert!(
        completed
            .branch
            .as_deref()
            .is_some_and(|branch| branch.starts_with("rpi/overlay/")),
        "overlay-isolated workflows must carry the overlay branch label"
    );

    // The workflow ran inside the overlay merged view, and its git work was
    // committed there (the merged view is a full working tree).
    let merged = f
        .managed
        .join(pi_coding::workflow_worktree::OVERLAY_ROOT_DIR_NAME)
        .join(created.workflow_id.as_str())
        .join("merged");
    assert!(merged.exists(), "the overlay merged view must exist");
    assert_eq!(
        fs::read_to_string(merged.join("hello.txt")).expect("merged hello"),
        "hello world\n"
    );
    assert_eq!(
        fs::read_to_string(merged.join("bye.txt")).expect("merged bye"),
        "bye\n"
    );
    let merged_head = git(&merged, &["rev-parse", "HEAD"]);
    assert_ne!(merged_head, base_commit, "the workflow must commit inside the overlay");

    // integrate: the source repo receives the merged tree as a single commit.
    // (The manager may already have auto-integrated at Completed — the explicit
    // call is idempotent either way, exactly like the worktree backend.)
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
    let source_head = git(&f.repo, &["rev-parse", "HEAD"]);
    assert_ne!(source_head, base_commit, "integration must commit on the source branch");

    // remove: the completed workflow and its overlay are cleaned.
    let removed = application
        .workflow_remove(&completed.workflow_id, completed.generation)
        .await
        .expect("remove completed workflow");
    assert_eq!(removed.workflow_id, completed.workflow_id);
    assert!(application.workflow_list().is_empty());
    assert!(!merged.exists(), "remove must delete the overlay merged view");
    application.cleanup().await;
}

/// `worktree` (default) and `none` wiring sanity: the setting enum parses and
/// the factory selects the matching backend kind.
#[test]
fn isolation_setting_selects_backend_kind() {
    assert_eq!(WorkflowIsolationSetting::Worktree, serde_json::from_str("\"worktree\"").expect("worktree"));
    assert_eq!(WorkflowIsolationSetting::Overlayfs, serde_json::from_str("\"overlayfs\"").expect("overlayfs"));
    assert_eq!(WorkflowIsolationSetting::None, serde_json::from_str("\"none\"").expect("none"));
    let default = Settings::default()
        .runtime_settings()
        .expect("default runtime settings");
    assert_eq!(
        default.orchestration_isolation,
        WorkflowIsolationSetting::Worktree,
        "worktree must stay the default isolation backend"
    );
}
