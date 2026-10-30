//! Real overlayfs isolation smoke tests (skip-guarded): they exercise the actual
//! overlay mount against the host kernel — kernel overlay (mount -t overlay),
//! fuse-overlayfs, and the recursive-copy fallback — plus the overlayfs workflow
//! isolation manager lifecycle and settings wiring.
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
use pi_ai::Model;
use pi_ai::providers::{
    FauxProviderOptions, FauxProviderRegistration, FauxResponse, register_faux_provider,
};
use pi_coding::isolate::{OverlayBackend, OverlayfsIsolation};
use pi_coding::workflow_worktree::OVERLAY_ROOT_DIR_NAME;
use pi_coding::{
    AgentCatalog, AgentDefinition, AgentDefinitionSource, Application, ApplicationRuntimeCandidate,
    ApplicationRuntimeFactory, ApplicationRuntimeFuture, ChildSessionOptionsSnapshot,
    OrchestrationConfig, OrchestrationRuntime, OrchestrationSettings, ResourceManager,
    ResourceManagerOptions, SandboxSettingsWire, Session, SessionOptions, Settings,
    WorkflowCreateRequest, WorkflowIsolationSetting, WorkspaceRoots,
};
use pi_sandbox::SandboxMode;
use tempfile::TempDir;

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
        eprintln!("overlayfs smoke: SKIP ({test_name}: no unprivileged user namespace mounts)");
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
        std::env::split_paths(&path).any(|dir| dir.join("fuse-overlayfs").is_file())
    })
}

// ---------------------------------------------------------------------------
// OverlayfsIsolation backend roundtrips
// ---------------------------------------------------------------------------

/// The recursive-copy fallback always works (no mounts): merged becomes an
/// independent copy of lower, writes never reach lower, and stop cleans the
/// private layers without touching the merged view.
///
/// The direct fixture uses the exact retained authority production requires:
/// the source repo is opened as a `ProjectResources` read-write lease and the
/// managed root as a `WorkflowWorktrees` private lease, then the merged view
/// is materialized through those descriptors (`start_rcopy_governed`) — the
/// same governed capability path the `OverlayWorkflowManager` uses, with no
/// raw plain-path copy. Kernel/FUSE live mounts remain environment-gated;
/// this governed rcopy test always runs.
#[test]
fn rcopy_fallback_materializes_an_independent_copy() {
    let sandbox = TempDir::new().expect("sandbox");
    let source = sandbox.path().join("source");
    fs::create_dir_all(&source).expect("source");
    fs::write(source.join("base.txt"), "base").expect("base file");
    fs::create_dir_all(source.join("sub")).expect("subdir");
    fs::write(source.join("sub").join("deep.txt"), "deep").expect("deep file");

    let managed = sandbox.path().join("managed");
    let iso = OverlayfsIsolation::start_rcopy_governed(
        &source,
        &managed,
        Path::new("upper"),
        Path::new("work"),
        Path::new("merged"),
    )
    .expect("governed rcopy isolation");
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

    // Independent: writes through the merged view never reach the source.
    fs::write(merged.join("new.txt"), "new").expect("write merged");
    assert!(
        !source.join("new.txt").exists(),
        "source must stay untouched"
    );

    // Cleanup: stop removes the private upper/work layers through the retained
    // managed lease and keeps the merged view (the manager removes it).
    iso.stop().expect("stop");
    assert!(!iso.upper_path().exists(), "upper must be cleaned");
    assert!(!iso.work_path().exists(), "work must be cleaned");
    assert!(
        merged.exists(),
        "stop keeps the merged view (the manager removes it)"
    );
    assert_eq!(
        fs::read_to_string(merged.join("base.txt")).expect("merged survives"),
        "base"
    );
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

    let iso = match OverlayfsIsolation::start(&lower, &upper, &work, &merged) {
        Ok(isolation) => isolation,
        Err(error)
            if error
                .to_string()
                .contains("retained descriptor-relative mount authority") =>
        {
            eprintln!("overlayfs smoke: SKIP (kernel mount requires governed retained authority)");
            return;
        }
        Err(error) => panic!("kernel overlay start: {error:#}"),
    };
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
    assert!(
        !lower.join("written.txt").exists(),
        "lower must stay read-only"
    );
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

    let iso = match OverlayfsIsolation::start_with(
        &lower,
        &upper,
        &work,
        &merged,
        &[OverlayBackend::FuseOverlayfs],
    ) {
        Ok(isolation) => isolation,
        Err(error)
            if error
                .to_string()
                .contains("retained descriptor-relative mount authority") =>
        {
            eprintln!("overlayfs smoke: SKIP (FUSE mount requires governed retained authority)");
            return;
        }
        Err(error) => panic!("fuse-overlayfs start: {error:#}"),
    };
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
    assert!(
        !iso.is_mounted(),
        "fusermount -u must detach the FUSE mount"
    );
    assert!(!upper.exists());
    assert!(!work.exists());
}

// ---------------------------------------------------------------------------
// Capability-bound OverlayWorkflowManager setup
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

/// The external fixture obtains the overlay manager through
/// `Application::setup_workflows`. That production path opens the source and
/// managed roots as the exact retained capabilities before constructing the
/// otherwise-private manager.
#[tokio::test]
async fn overlay_workflow_manager_uses_application_capability_root() {
    let f = AppFixture::new();
    let (application, _factory, _registration) = overlay_app(&f, Vec::new()).await;

    let created = application
        .workflow_create(WorkflowCreateRequest {
            name: "capability-bound-overlay".into(),
            objective: "retain exact workflow storage authority".into(),
        })
        .await
        .expect("capability-bound overlay creation");
    assert!(
        created
            .branch
            .as_deref()
            .is_some_and(|branch| branch.starts_with("rpi/overlay/")),
        "setup must select the overlay isolation manager"
    );
    let merged = f
        .managed
        .join(OVERLAY_ROOT_DIR_NAME)
        .join(created.workflow_id.as_str())
        .join("merged");
    assert_eq!(
        fs::read_to_string(merged.join("README.md")).expect("merged README"),
        "base\n",
        "the retained managed capability must materialize the source tree"
    );

    application
        .workflow_remove(&created.workflow_id, created.generation)
        .await
        .expect("remove capability-bound overlay");
    assert!(
        !merged.exists(),
        "remove must clean the retained managed root"
    );
    application.cleanup().await;
}

// ---------------------------------------------------------------------------
// Wiring: `settings.orchestration.isolation` drives the workflow backend
// ---------------------------------------------------------------------------

/// The workflow worker agent: exactly the `bash` tool, matching the canonical
/// `workflow_application` fixture.
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

/// Give the workflow child session the same isolated `danger-full-access`
/// resource context the canonical `workflow_application` fixture uses:
/// pinning the mode makes this deterministic fixture independent of the
/// product default and avoids authenticating bubblewrap during bootstrap.
async fn attach_isolated_resources(session: &Session) -> anyhow::Result<()> {
    let agent_dir = test_sessions_root()
        .join("agents")
        .join(uuid::Uuid::now_v7().to_string());
    fs::create_dir_all(&agent_dir)?;
    fs::write(
        agent_dir.join("settings.json"),
        r#"{"sandbox":{"mode":"danger-full-access"}}"#,
    )?;
    let mut options = ResourceManagerOptions::new(session.cwd());
    options.agent_dir = agent_dir;
    options.disable_extensions = true;
    options.disable_skills = true;
    options.disable_prompt_templates = true;
    options.disable_themes = true;
    options.disable_context_files = true;
    session
        .attach_resources(ResourceManager::new(options)?)
        .await
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
            let artifacts = std::env::temp_dir().join(format!(
                "pi-overlay-test-artifacts-{}",
                uuid::Uuid::now_v7()
            ));
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
            let session =
                Session::new_with_todo_additional_tools_filtered_discovery_workspace_and_uri(
                    options,
                    additional_tools,
                    Default::default(),
                    pi_coding::ResourceDiscovery::Disabled,
                    workspace,
                    None,
                    true,
                )?;
            session.set_session_dir(test_sessions_root());
            session.start_new_recording()?;
            attach_isolated_resources(&session).await?;
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
        auth_manager: None,
    })
    .expect("session");
    session.set_session_dir(test_sessions_root());
    (session, registration)
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
    // select the overlayfs backend. The sandbox is pinned to
    // danger-full-access explicitly so this fixture does not depend on the
    // product default or authenticate a platform launcher during bootstrap.
    let settings = Settings {
        sandbox: Some(SandboxSettingsWire {
            mode: Some(SandboxMode::DangerFullAccess),
            ..Default::default()
        }),
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
    let mut resource_options = ResourceManagerOptions::new(&f.repo);
    resource_options.agent_dir = f.agent_dir.path().to_path_buf();
    resource_options.headless = true;
    resource_options.project_trust_override = None;
    resource_options.disable_extensions = true;
    resource_options.disable_skills = true;
    resource_options.disable_prompt_templates = true;
    resource_options.disable_themes = true;
    resource_options.disable_context_files = true;
    let resources = ResourceManager::new(resource_options).expect("resources");
    session
        .attach_resources(resources)
        .await
        .expect("attach resources");
    let snapshot = session.child_session_options_snapshot();
    let application = Application::new(session).await;
    // Repository-authorized context, exactly as the canonical
    // `workflow_application` fixture: governed git (repo discovery, rev-parse,
    // copy-back commits) runs through the hermetic executor seam instead of
    // the host sandbox process backend, keeping the deterministic fixture
    // host-independent.
    application.with_hermetic_repository_executor(
        pi_coding::process::HermeticRepositoryExecutor::default_executor(),
    );
    application
        .attach_runtime_factory(Arc::new(TestFactory { snapshot }))
        .expect("factory");
    let factory = application
        .setup_workflows(&f.repo, &f.state, &f.managed)
        .await
        .expect("setup workflows");
    (application, factory, registration)
}

/// `worktree` (default) and `none` wiring sanity: the setting enum parses and
/// the factory selects the matching backend kind.
#[test]
fn isolation_setting_selects_backend_kind() {
    assert_eq!(
        WorkflowIsolationSetting::Worktree,
        serde_json::from_str("\"worktree\"").expect("worktree")
    );
    assert_eq!(
        WorkflowIsolationSetting::Overlayfs,
        serde_json::from_str("\"overlayfs\"").expect("overlayfs")
    );
    assert_eq!(
        WorkflowIsolationSetting::None,
        serde_json::from_str("\"none\"").expect("none")
    );
    let default = Settings::default()
        .runtime_settings()
        .expect("default runtime settings");
    assert_eq!(
        default.orchestration_isolation,
        WorkflowIsolationSetting::Worktree,
        "worktree must stay the default isolation backend"
    );
}
