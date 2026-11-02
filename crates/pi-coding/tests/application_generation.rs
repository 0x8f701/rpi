//! Immutable application-generation publication proofs (D5 integration file).
//!
//! One `ApplicationPublication` owns the sole live `ArcSwap<ApplicationGeneration>`
//! (C11, §11): bootstrap publishes revision one with a Bootstrap origin, every
//! reload binds the captured base revision and swaps exactly once after all
//! fallible staging, consumers capture one pointer-identical aggregate, and
//! mixed provenance or failed staging never reaches the pointer. Durable
//! policy state captures the exact generation; reconstruction revalidates it
//! and installs the aggregate through the production workflow restore
//! callsite before any child recovery, and every error domain (state/
//! credential/filesystem/sandbox/http/generation) keeps its payload.
//!
//! The fixtures are deterministic on every host: a faux provider with no
//! resolved HTTP authorities plus `sandbox.mode = danger-full-access` skips
//! the launcher probe entirely (no bubblewrap dependency), so bootstrap,
//! reload, failure-injection, and durable restore run identically on the
//! static-gates and hosted runners.

use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::FutureExt;
use pi_agent::ThinkingLevel;
use pi_ai::providers::{
    FauxProviderOptions, FauxProviderRegistration, FauxResponse, register_faux_provider,
};
use pi_ai::{ContentBlock, Model, StopReason, ToolCall};
use pi_coding::sandbox::http::SessionHttpError;
use pi_coding::{
    AgentCatalog, AgentDefinition, AgentDefinitionSource, Application, ApplicationRuntimeCandidate,
    ApplicationRuntimeFactory, ApplicationRuntimeFuture, ChildSessionOptionsSnapshot,
    CredentialConfigError, DEFAULT_MAX_CONCURRENCY, DurablePolicyState, DurableResumeError,
    DurableStateValidationError, FsError, OrchestrationConfig, OrchestrationRuntime,
    ResourceDiscovery, ResourceManager, ResourceManagerOptions, SandboxConfigError,
    SandboxGenerationError, Session, SessionOptions, WorkflowCreateRequest, WorkflowStatus,
    WorkspaceRoots,
};
use pi_sandbox::{
    AbsolutePath, CredentialPolicy, HostPlatform, PlatformFactInputs, PolicyRevision,
    PolicySnapshot, SandboxError, SandboxPolicy, SessionScope, derive_platform_facts,
    probe_backend,
};

/// Danger mode skips the launcher probe, so bootstrap succeeds on any host;
/// the faux provider resolves no HTTP authorities, so the danger-mode
/// requirement of an empty host-http policy holds.
const DANGER_SETTINGS: &str = r#"{"sandbox":{"mode":"danger-full-access"}}"#;

const DANGER_NO_ISOLATION_SETTINGS: &str =
    r#"{"sandbox":{"mode":"danger-full-access"},"orchestration":{"isolation":"none"}}"#;

/// Zero address space is rejected by `ResourceLimits` as a configuration
/// error, deterministically, before any scratch/root side effect.
const BROKEN_LIMITS_SETTINGS: &str =
    r#"{"sandbox":{"mode":"danger-full-access","limits":{"addressSpaceBytes":0}}}"#;

/// Builds a deterministic session + resource manager. The settings file lives
/// in the agent home so `SettingsManager::load_phase_one` (used by bootstrap
/// and reload) picks it up without touching the real user agent dir.
fn fixture(
    agent_settings: Option<&str>,
) -> (
    tempfile::TempDir,
    Session,
    FauxProviderRegistration,
    ResourceManager,
) {
    let root = tempfile::tempdir().expect("fixture root");
    let agent_dir = root.path().join("agent-home");
    let cwd = root.path().join("cwd");
    let session_dir = root.path().join("sessions");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&session_dir).expect("sessions");
    if let Some(settings) = agent_settings {
        std::fs::write(agent_dir.join("settings.json"), settings).expect("settings file");
    }

    let suffix = uuid::Uuid::now_v7().to_string();
    let api = format!("generation-proof-api-{suffix}");
    let provider = format!("generation-proof-provider-{suffix}");
    let model = Model {
        id: "generation-proof-model".to_owned(),
        name: "Generation Proof Model".to_owned(),
        api: api.clone(),
        provider: provider.clone(),
        ..Model::default()
    };
    let registration = register_faux_provider(FauxProviderOptions {
        api,
        provider,
        models: vec![model.clone()],
        chunk_size: 1,
    });

    let mut options = ResourceManagerOptions::new(&cwd);
    options.agent_dir = agent_dir.clone();
    options.disable_extensions = true;
    options.disable_skills = true;
    options.disable_prompt_templates = true;
    options.disable_themes = true;
    options.disable_context_files = true;
    let resources = ResourceManager::new(options).expect("resource manager");

    let session = Session::new_with_additional_tools_filtered_and_discovery(
        SessionOptions {
            model,
            cwd,
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
        ResourceDiscovery::Disabled,
    )
    .expect("session");
    session.set_session_dir(session_dir);
    (root, session, registration, resources)
}

/// Bootstraps a live application through the fallible product constructor.
async fn bootstrap(
    agent_settings: Option<&str>,
) -> (
    tempfile::TempDir,
    Application,
    FauxProviderRegistration,
    ResourceManager,
) {
    let (root, session, registration, resources) = fixture(agent_settings);
    session
        .attach_resources(resources.clone())
        .await
        .expect("attach resources");
    let application =
        Application::from_runtime_candidate(ApplicationRuntimeCandidate::new(session))
            .await
            .expect("bootstrap application");
    (root, application, registration, resources)
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|message| (*message).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "validated Application bootstrap".to_owned())
}

/// Extracts the error from a fallible result without ever formatting the Ok
/// value (the Ok payloads here — `Application`, `SandboxBackend` — do not
/// implement `Debug`, so `Result::expect_err` cannot be used).
fn expect_err_text<T, E: std::fmt::Display>(result: Result<T, E>, message: &str) -> E {
    match result {
        Err(error) => error,
        Ok(_) => panic!("{message}: expected an error, got Ok"),
    }
}

/// Per-process isolated native session root for the workflow child sessions,
/// so child `start_new_recording()` never writes into the real agent tree.
fn test_sessions_root() -> PathBuf {
    static ROOT: std::sync::LazyLock<tempfile::TempDir> =
        std::sync::LazyLock::new(|| tempfile::tempdir().expect("test sessions root"));
    ROOT.path().to_path_buf()
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

/// A git-backed fixture tree: the workflow source repo plus the workflow
/// store (`state`) and managed-worktree (`managed`) roots.
fn workflow_fixture() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
    let sandbox = tempfile::tempdir().expect("sandbox");
    let repo = sandbox.path().join("source");
    std::fs::create_dir_all(&repo).expect("repo");
    git(&repo, &["init"]);
    git(&repo, &["config", "user.name", "Pi Test"]);
    git(&repo, &["config", "user.email", "pi@example.test"]);
    std::fs::write(repo.join("README.md"), "base\n").expect("base");
    git(&repo, &["add", "README.md"]);
    git(&repo, &["commit", "-m", "initial"]);
    let state = sandbox.path().join("state");
    let managed = sandbox.path().join("managed");
    (sandbox, repo, state, managed)
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

fn planning(objective: &str) -> FauxResponse {
    FauxResponse {
        content: vec![ContentBlock::ToolCall(ToolCall {
            id: "todo-init".into(),
            name: "todo".into(),
            arguments: serde_json::json!({"op": "init", "items": [objective]}),
            thought_signature: None,
        })],
        stop_reason: StopReason::ToolUse,
        error_message: None,
    }
}

/// Runtime factory for the workflow child sessions. The explicit empty base
/// tool set avoids constructing ambient, generation-bound coding tools before
/// the candidate Application has published its first generation. Bootstrap
/// then stages the workflow orchestration/Todo tools normally. The child's
/// resource manager uses the same deterministic danger-mode agent dir as the
/// parent, so bootstrap and durable resume never probe a launcher on any host.
/// Candidate failures are retained only in this test fixture because the
/// user-facing WorkflowManager intentionally returns a redacted error.
#[derive(Clone)]
struct TestFactory {
    snapshot: ChildSessionOptionsSnapshot,
    agent_dir: PathBuf,
    last_failure: Arc<Mutex<Option<String>>>,
}

impl TestFactory {
    fn new(snapshot: ChildSessionOptionsSnapshot, agent_dir: PathBuf) -> Self {
        Self {
            snapshot,
            agent_dir,
            last_failure: Arc::new(Mutex::new(None)),
        }
    }

    fn failure(&self) -> String {
        self.last_failure
            .lock()
            .expect("failure capture")
            .clone()
            .unwrap_or_else(|| "no test-factory failure captured".to_owned())
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
        let agent_dir = self.agent_dir.clone();
        let last_failure = self.last_failure.clone();
        *last_failure.lock().expect("failure capture") = None;
        Box::pin(async move {
            let result = async {
                options.cwd = cwd.path().to_path_buf();
                options.tools = Some(Vec::new());
                let workspace = WorkspaceRoots::new(cwd.path(), Vec::<PathBuf>::new())?;
                let session =
                    Session::new_with_todo_additional_tools_filtered_discovery_workspace_and_uri(
                        options,
                        Vec::new(),
                        Default::default(),
                        ResourceDiscovery::Disabled,
                        workspace,
                        None,
                        true,
                    )?;
                session.set_session_dir(test_sessions_root());
                session.start_new_recording()?;
                let mut rm_options = ResourceManagerOptions::new(cwd.path());
                rm_options.agent_dir = agent_dir;
                rm_options.disable_extensions = true;
                rm_options.disable_skills = true;
                rm_options.disable_prompt_templates = true;
                rm_options.disable_themes = true;
                rm_options.disable_context_files = true;
                let resources = ResourceManager::new(rm_options)?;
                session.attach_resources(resources).await?;
                let artifacts = std::env::temp_dir().join(format!(
                    "pi-durable-proof-artifacts-{}",
                    uuid::Uuid::now_v7()
                ));
                let mut config = OrchestrationConfig::new(
                    AgentCatalog::from_agents(vec![definition()]),
                    artifacts,
                );
                config.parent_model = snapshot.model.clone();
                config.max_concurrency = DEFAULT_MAX_CONCURRENCY;
                config.idle_ttl = None;
                let orchestration = OrchestrationRuntime::new(
                    config,
                    OrchestrationRuntime::child_factory_from_snapshot(snapshot),
                )?;
                Ok(ApplicationRuntimeCandidate::new(session).with_orchestration(orchestration))
            }
            .await;
            if let Err(error) = &result {
                *last_failure.lock().expect("failure capture") = Some(format!("{error:#}"));
            }
            result
        })
    }
}

async fn wait_status(
    application: &Application,
    workflow_id: &pi_coding::WorkflowId,
    status: WorkflowStatus,
) -> pi_coding::WorkflowSnapshot {
    tokio::time::timeout(Duration::from_secs(15), async {
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

/// Bootstrap publishes revision one with the Bootstrap origin: the resource
/// generation is exactly one, and the first reload must advance that
/// revision-one base (a bootstrap that published any other revision — or an
/// origin that did not bind revision one — fails here).
#[tokio::test]
async fn bootstrap_publishes_revision_one_with_bootstrap_origin() {
    let (root, application, _registration, _resources) = bootstrap(Some(DANGER_SETTINGS)).await;
    assert_eq!(
        application.resource_generation(),
        Some(1),
        "bootstrap must publish revision one"
    );
    let snapshot = application.resource_snapshot().expect("resource snapshot");
    assert_eq!(snapshot.generation, 1);

    let reloaded = application.reload().await.expect("reload");
    assert_eq!(
        reloaded.generation, 2,
        "the first reload must advance the revision-one bootstrap base"
    );
    assert_eq!(application.resource_generation(), Some(2));
    let _ = &root;
}

/// Reload stages invisibly, then swaps the single publication pointer exactly
/// once: consumers capture one pointer-identical aggregate; a failed reload
/// leaves the active aggregate byte-for-byte untouched.
#[tokio::test]
async fn reload_stages_invisibly_then_swaps_exactly_once() {
    let (root, application, _registration, _resources) = bootstrap(Some(DANGER_SETTINGS)).await;
    assert_eq!(application.resource_generation(), Some(1));

    let before = application.resource_snapshot().expect("snapshot before");
    let retained = before.clone(); // a consumer that captured the aggregate once

    let reloaded = application.reload().await.expect("reload");
    assert_eq!(reloaded.generation, 2);

    let after = application.resource_snapshot().expect("snapshot after");
    assert_eq!(after.generation, 2);
    assert!(
        !Arc::ptr_eq(&before, &after),
        "reload must publish a new aggregate"
    );
    // Every capture after the swap returns the identical Arc — pointer-
    // identical publication.
    assert!(
        Arc::ptr_eq(
            &after,
            &application.resource_snapshot().expect("snapshot again")
        ),
        "all captures must observe the same pointer-identical aggregate"
    );
    // Old in-flight work retains the old immutable generation.
    assert!(
        Arc::ptr_eq(&retained, &before),
        "captured aggregates must not be mutated"
    );
    assert_eq!(retained.generation, 1);

    // A failing reload stages invisibly: nothing is swapped and the active
    // aggregate (and its pointer) stays exactly as it was.
    let agent_dir = root.path().join("agent-home");
    std::fs::write(agent_dir.join("settings.json"), BROKEN_LIMITS_SETTINGS)
        .expect("break settings");
    let error = application
        .reload()
        .await
        .expect_err("broken-settings reload must fail");
    let message = format!("{error:#}");
    assert!(
        message.contains("limit") || message.contains("configuration"),
        "unexpected reload failure: {message}"
    );
    assert_eq!(
        application.resource_generation(),
        Some(2),
        "a failed reload must not advance the published generation"
    );
    assert!(
        Arc::ptr_eq(
            &after,
            &application
                .resource_snapshot()
                .expect("snapshot after failure")
        ),
        "a failed reload must not swap the publication pointer"
    );
}

/// Every reload binds the *captured* base revision: the second reload must
/// advance the revision-two publication (not rebase onto the bootstrap), and
/// each reported generation must match the aggregate actually published.
#[tokio::test]
async fn reload_origin_binds_captured_base_revision() {
    let (_root, application, _registration, _resources) = bootstrap(Some(DANGER_SETTINGS)).await;
    assert_eq!(application.resource_generation(), Some(1));

    let first = application.reload().await.expect("first reload");
    assert_eq!(first.generation, 2);
    assert_eq!(application.resource_generation(), Some(2));

    let second = application.reload().await.expect("second reload");
    assert_eq!(
        second.generation, 3,
        "the reload origin must bind the captured base revision"
    );
    assert_eq!(
        application.resource_generation(),
        Some(3),
        "the published aggregate must match the reload result"
    );
}

/// A product bootstrap failure returns an error (never a panic) and leaves no
/// side effects: the revision root is never created and no resource
/// generation is published.
#[tokio::test]
async fn product_bootstrap_failure_returns_error_before_side_effects() {
    let (root, session, _registration, resources) = fixture(Some(BROKEN_LIMITS_SETTINGS));
    session
        .attach_resources(resources.clone())
        .await
        .expect("attach resources");
    let error = expect_err_text(
        Application::from_runtime_candidate(ApplicationRuntimeCandidate::new(session)).await,
        "invalid limits must fail the product bootstrap",
    );
    let message = format!("{error:#}");
    assert!(
        message.contains("configuration") || message.contains("limit"),
        "the failure must be a configuration-domain error, got: {message}"
    );
    assert!(
        !root.path().join("sessions").join("sandbox").exists(),
        "a failed bootstrap must not create any revision root"
    );
    assert_eq!(
        resources.generation(),
        1,
        "a failed bootstrap must not publish a resource generation"
    );
}

/// The convenience constructor (`Application::new`) panics — with a message
/// that names the bootstrap — when the product bootstrap fails, instead of
/// silently degrading.
#[tokio::test]
async fn convenience_constructor_panics_on_bootstrap_failure() {
    let (root, session, _registration, resources) = fixture(Some(BROKEN_LIMITS_SETTINGS));
    session
        .attach_resources(resources.clone())
        .await
        .expect("attach resources");
    let outcome = AssertUnwindSafe(Application::new(session))
        .catch_unwind()
        .await;
    let payload = match outcome {
        Err(payload) => payload,
        Ok(_) => panic!("Application::new must panic on bootstrap failure"),
    };
    let message = panic_message(&payload);
    assert!(
        message.contains("validated Application bootstrap"),
        "the panic must name the failing bootstrap, got: {message}"
    );
}

/// A runtime probe failure is returned in the runtime domain
/// (`SandboxError`), never reported as an invalid configuration.
///
/// This is a deterministic domain proof through the public sandbox surface:
/// `probe_backend` must reject a snapshot created for a platform that differs
/// from the sealed platform facts with `SandboxError::UnsupportedMode` — a
/// `SandboxError` (runtime), not a policy/configuration error.
///
/// The full product path under an explicitly configured confined mode (bootstrap must
/// classify a launcher-probe failure as a runtime staging failure, never as
/// invalid configuration) is a live launcher-dependent surface; the
/// deterministic danger-mode product path is covered by the other generation
/// tests in this file, so this test intentionally stops at the public sandbox
/// domain proof.
#[tokio::test]
async fn runtime_probe_failure_is_not_reported_as_invalid_configuration() {
    let (root, _session, _registration, _resources) = fixture(None);
    let cwd = AbsolutePath::parse(root.path().join("cwd")).expect("cwd");
    let agent_dir = AbsolutePath::parse(root.path().join("agent-home")).expect("agent dir");
    let session_dir = AbsolutePath::parse(root.path().join("sessions")).expect("sessions");
    let scope = SessionScope::new(
        "probe-domain-test".to_owned(),
        vec![cwd],
        agent_dir,
        session_dir,
    )
    .expect("session scope");
    let inputs = PlatformFactInputs::new(
        scope.session_id().to_owned(),
        scope.workspace_roots().to_vec(),
        scope.agent_dir().clone(),
        scope.session_dir().clone(),
    )
    .expect("platform inputs");
    let facts = derive_platform_facts(inputs).expect("platform facts");
    let credentials = CredentialPolicy::new(Vec::new()).expect("credential policy");
    let policy = SandboxPolicy::danger_full_access(credentials).expect("danger policy");
    let other_platform = match facts.platform() {
        HostPlatform::Linux => HostPlatform::Macos,
        HostPlatform::Macos => HostPlatform::Windows,
        HostPlatform::Windows => HostPlatform::Linux,
    };
    let snapshot = Arc::new(
        PolicySnapshot::new(
            PolicyRevision::new(1).expect("revision"),
            other_platform,
            policy,
        )
        .expect("snapshot"),
    );
    let error = expect_err_text(
        probe_backend(facts, snapshot),
        "cross-platform probe must fail",
    );
    assert!(
        matches!(error, SandboxError::UnsupportedMode { .. }),
        "the runtime probe failure must be a SandboxError, got {error:?}"
    );
}

/// Session policy bootstrap owns creation of the configured session store
/// capability root. A missing path is created with private authority before
/// generation publication and receives the revision sandbox directories.
#[tokio::test]
async fn session_policy_inputs_create_owned_session_capability_paths() {
    let (root, session, _registration, resources) = fixture(Some(DANGER_SETTINGS));
    let session_dir = root.path().join("configured-sessions");
    session.set_session_dir(session_dir.clone());
    session
        .attach_resources(resources.clone())
        .await
        .expect("attach resources");
    let application =
        Application::from_runtime_candidate(ApplicationRuntimeCandidate::new(session))
            .await
            .expect("bootstrap creates the owned session capability root");
    assert!(
        session_dir.is_dir(),
        "session capability root must be created"
    );
    assert!(
        session_dir.join("sandbox").join("1").is_dir(),
        "revision roots must live under the configured session capability"
    );
    assert_eq!(
        resources.generation(),
        1,
        "bootstrap publishes revision one"
    );
    application.cleanup().await;
}

/// Durable policy state captures the exact bootstrap generation, and every
/// error domain keeps its typed payload: a tampered durable snapshot is
/// rejected with the digest-mismatch message, and the state/credential/
/// sandbox/http resume domains each preserve their own error text. A change
/// that collapses domains or swallows payloads fails here.
#[tokio::test]
async fn durable_resume_errors_preserve_all_domains() {
    let (_root, application, _registration, _resources) = bootstrap(Some(DANGER_SETTINGS)).await;
    let state = application
        .durable_policy_state()
        .expect("capture durable policy state");
    assert_eq!(
        state.snapshot().revision().get(),
        1,
        "durable state must capture the bootstrap revision"
    );

    // State domain through the durable wire: a tampered digest is rejected
    // and the digest-mismatch payload survives (it is not collapsed into a
    // generic parse error).
    let json = serde_json::to_string(&state).expect("serialize durable state");
    let mut value: serde_json::Value = serde_json::from_str(&json).expect("durable json");
    let digest = value["snapshot"]["digest"]
        .as_str()
        .expect("digest field")
        .to_owned();
    let flipped = format!(
        "{}{}",
        if digest.starts_with('0') { '1' } else { '0' },
        &digest[1..]
    );
    value["snapshot"]["digest"] = serde_json::Value::String(flipped);
    let error = serde_json::from_str::<DurablePolicyState>(&value.to_string())
        .expect_err("a tampered durable snapshot must be rejected");
    assert!(
        error.to_string().contains("digest"),
        "the digest-mismatch payload must survive the State domain, got: {error}"
    );

    // State domain enum: the typed policy payload is preserved.
    let state_error =
        DurableStateValidationError::Policy(pi_sandbox::PolicyValidationError::DigestMismatch);
    assert!(
        state_error
            .to_string()
            .contains("durable policy snapshot is invalid"),
        "{state_error}"
    );
    assert!(
        state_error.to_string().contains("digest mismatch"),
        "{state_error}"
    );

    // Credential domain enum: the credential payload is preserved.
    let credential_error =
        DurableStateValidationError::Credential(CredentialConfigError::UnknownGrantReference);
    assert!(
        credential_error
            .to_string()
            .contains("durable credential registry is invalid"),
        "{credential_error}"
    );
    assert!(
        credential_error.to_string().contains("unknown definition"),
        "{credential_error}"
    );

    // Sandbox and HTTP resume domains keep their own error text; routing them
    // through the State domain (or dropping the payload) fails here.
    let sandbox = DurableResumeError::Sandbox(SandboxError::ProxyRoutedUnavailable);
    assert!(
        sandbox
            .to_string()
            .contains("durable sandbox representation or backend probe failed"),
        "{sandbox}"
    );
    assert!(sandbox.to_string().contains("proxy-routed"), "{sandbox}");

    let http = DurableResumeError::Http(SessionHttpError::MissingHostHttpDestination);
    assert!(
        http.to_string()
            .contains("durable HTTP authority staging failed"),
        "{http}"
    );
    assert!(
        http.to_string().contains("missing a required destination"),
        "{http}"
    );

    // Filesystem domain: the identity payload is preserved.
    let filesystem = DurableResumeError::Filesystem(FsError::IdentityChanged);
    assert!(
        filesystem
            .to_string()
            .contains("durable filesystem revalidation failed"),
        "{filesystem}"
    );
    assert!(
        filesystem.to_string().contains("identity changed"),
        "{filesystem}"
    );

    // Generation domain: the configuration payload is preserved through the
    // wrapped construction error.
    let generation = DurableResumeError::Generation(SandboxGenerationError::Config(
        SandboxConfigError::Policy(pi_sandbox::PolicyValidationError::DigestMismatch),
    ));
    assert!(
        generation
            .to_string()
            .contains("durable aggregate generation construction failed"),
        "{generation}"
    );
    assert!(
        generation.to_string().contains("configuration is invalid"),
        "{generation}"
    );
    assert!(
        generation.to_string().contains("digest mismatch"),
        "{generation}"
    );

    let state_wrapped = DurableResumeError::State(DurableStateValidationError::Policy(
        pi_sandbox::PolicyValidationError::DigestMismatch,
    ));
    assert!(
        state_wrapped
            .to_string()
            .contains("durable state validation failed"),
        "{state_wrapped}"
    );
}

/// Durable resume reconstructs the exact generation through the production
/// restore callsite: a paused workflow persists its durable policy, and
/// re-opening the workflow store runs `restore_all` →
/// `ApplicationWorkflowRuntimeFactory::restore` →
/// `Application::from_durable_runtime_candidate` →
/// `resume_durable_generation`, which must revalidate the digest and install
/// the reconstructed aggregate BEFORE any child runtime continuation (the
/// restored record stays live and non-terminal). The restored record must
/// carry the exact persisted revision, digest, and credential-grant registry.
/// If `resume_durable_generation` is removed or stubbed, the restore fails
/// and the record is marked Failed — this test fails there.
#[tokio::test]
async fn durable_resume_reconstructs_exact_generation_before_child_restore() {
    // Phase 1 — exact payload round-trip through the durable wire: capture,
    // serialize, reconstruct, and compare the revision, digest, and grants.
    let (_root, application, _registration, _resources) = bootstrap(Some(DANGER_SETTINGS)).await;
    let state = application
        .durable_policy_state()
        .expect("capture durable policy state");
    assert_eq!(state.snapshot().revision().get(), 1);
    let json = serde_json::to_string(&state).expect("serialize durable state");
    let restored: DurablePolicyState = serde_json::from_str(&json).expect("reconstruct");
    assert_eq!(
        restored, state,
        "reconstruction must reproduce the exact durable generation"
    );
    assert_eq!(restored.snapshot().revision(), state.snapshot().revision());
    assert_eq!(restored.snapshot().digest(), state.snapshot().digest());
    assert_eq!(restored.credential_grants(), state.credential_grants());
    assert_eq!(
        restored.snapshot().revision().get(),
        1,
        "reconstruction is read-only: it must not advance the revision"
    );

    // Phase 2 — the production workflow/orchestration restore callsite.
    let (sandbox, repo, state_dir, managed) = workflow_fixture();

    // App 1: create a workflow, drive it to Running, then pause it so the
    // persisted record is non-terminal and carries the exact durable policy.
    let (root1, session1, registration1, resources1) = fixture(Some(DANGER_NO_ISOLATION_SETTINGS));
    let agent_dir = root1.path().join("agent-home");
    session1
        .attach_resources(resources1)
        .await
        .expect("attach resources");
    let first =
        Application::from_runtime_candidate(ApplicationRuntimeCandidate::new(session1.clone()))
            .await
            .expect("first application");
    let snapshot1 = session1.child_session_options_snapshot();
    let factory1 = Arc::new(TestFactory::new(snapshot1, agent_dir.clone()));
    first
        .attach_runtime_factory(factory1.clone())
        .expect("attach runtime factory");
    registration1.set_responses(vec![
        planning("resume after process exit"),
        FauxResponse::text("planned"),
    ]);
    let workflow_factory = first
        .setup_workflows(&repo, &state_dir, &managed)
        .await
        .expect("setup workflows");
    let created = match first
        .workflow_create(WorkflowCreateRequest {
            name: "durable-restore".into(),
            objective: "resume after process exit".into(),
        })
        .await
    {
        Ok(created) => created,
        Err(error) => panic!(
            "create workflow: {error}; child candidate failure: {}",
            factory1.failure()
        ),
    };
    let running = wait_status(&first, &created.workflow_id, WorkflowStatus::Running).await;
    assert_eq!(running.status, WorkflowStatus::Running);
    let paused = first
        .workflow_pause(&created.workflow_id, created.generation)
        .await
        .expect("pause workflow");
    assert_eq!(paused.status, WorkflowStatus::Paused);
    let persisted = paused.durable_policy;
    assert_eq!(
        persisted.snapshot().revision().get(),
        1,
        "the persisted durable policy is the bootstrap revision"
    );
    first.cleanup().await;
    drop(first);
    drop(registration1);
    let _ = (&root1,);

    // App 2 over the same store: setup_workflows runs restore_all, which
    // calls the factory restore with the persisted durable policy. The
    // reconstructed aggregate must be installed (restore succeeds, record
    // stays Paused — not Failed) and must be the exact persisted generation.
    let (root2, session2, registration2, resources2) = fixture(Some(DANGER_NO_ISOLATION_SETTINGS));
    session2
        .attach_resources(resources2)
        .await
        .expect("attach resources");
    let second =
        Application::from_runtime_candidate(ApplicationRuntimeCandidate::new(session2.clone()))
            .await
            .expect("second application");
    let snapshot2 = session2.child_session_options_snapshot();
    let factory2 = Arc::new(TestFactory::new(snapshot2, agent_dir));
    second
        .attach_runtime_factory(factory2.clone())
        .expect("attach runtime factory");
    second
        .setup_workflows(&repo, &state_dir, &managed)
        .await
        .expect("restore workflows");
    let restored = second
        .workflow_get(&created.workflow_id)
        .expect("restored workflow");
    assert_eq!(
        restored.status,
        WorkflowStatus::Paused,
        "durable resume must install the reconstructed generation before any \
         child recovery; a stubbed/removed resume marks the record Failed; \
         child candidate failure: {}",
        factory2.failure()
    );
    assert_eq!(
        restored.durable_policy, persisted,
        "the resumed generation must reconstruct the exact revision, digest, and grant registry"
    );
    assert_eq!(restored.durable_policy.snapshot().revision().get(), 1);
    assert_eq!(
        restored.durable_policy.snapshot().digest(),
        persisted.snapshot().digest()
    );
    assert_eq!(
        restored.durable_policy.credential_grants(),
        persisted.credential_grants()
    );
    second.cleanup().await;
    let _ = (sandbox, root2, registration2);
}
