use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use pi_agent::{
    AbortController, AgentEvent, AgentToolResult, QueueMode, ThinkingLevel, ToolCallContext,
};
use pi_ai::providers::{
    FauxProviderOptions, FauxProviderRegistration, FauxResponse, register_faux_provider,
};
use pi_ai::{ContentBlock, Model, StopReason, Usage};
use pi_coding::{
    AgentCatalog, AgentDefinition, AgentDefinitionSource, AgentRuntimeSettings, Application,
    ApplicationEvent, ApplicationRuntimeCandidate, ApplicationRuntimeFactory,
    ApplicationRuntimeFuture, CompactionSettings, DeliveryOutcome, ExtensionCapability,
    ExtensionMode, ExtensionPermissionSet, ExtensionRuntime, ExtensionRuntimeOptions,
    GoalActivationOutcome, JobStatus, OrchestrationConfig, OrchestrationRuntime,
    OrchestrationSettings, ResourceDiscovery, ResourceManager, ResourceManagerOptions, Session,
    SessionAlreadyProcessing, SessionOptions, StreamingBehavior, TaskItem, ToolSelection,
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

const DANGER_SETTINGS: &str = r#"{"sandbox":{"mode":"danger-full-access"}}"#;

struct HermeticFixture {
    _workspace: tempfile::TempDir,
    cwd: PathBuf,
    agent_dir: tempfile::TempDir,
    session_dir: tempfile::TempDir,
    session: Session,
    registration: FauxProviderRegistration,
    resources: ResourceManager,
}

impl HermeticFixture {
    fn new(
        label: &str,
        responses: Vec<FauxResponse>,
        stream_fn: Option<pi_agent::StreamFn>,
        setup: impl FnOnce(&Path, &Path),
        configure_resources: impl FnOnce(&mut ResourceManagerOptions),
        tools: ToolSelection,
    ) -> Self {
        let workspace = tempfile::tempdir().expect("fixture workspace");
        let workspace_path = workspace.path().to_path_buf();
        Self::in_workspace(
            label,
            responses,
            stream_fn,
            workspace,
            workspace_path,
            setup,
            configure_resources,
            tools,
        )
    }

    fn for_workspace(
        label: &str,
        workspace_path: &Path,
        responses: Vec<FauxResponse>,
        stream_fn: Option<pi_agent::StreamFn>,
    ) -> Self {
        let workspace = tempfile::tempdir().expect("retained fixture workspace owner");
        Self::in_workspace(
            label,
            responses,
            stream_fn,
            workspace,
            workspace_path.to_path_buf(),
            |_, _| {},
            |_| {},
            ToolSelection::default(),
        )
    }

    fn hermetic(
        label: &str,
        responses: Vec<FauxResponse>,
        stream_fn: Option<pi_agent::StreamFn>,
        workspace: tempfile::TempDir,
        workspace_path: PathBuf,
        setup: impl FnOnce(&Path, &Path),
        configure_resources: impl FnOnce(&mut ResourceManagerOptions),
        tools: ToolSelection,
        ambient_tool_set: bool,
        additional_tools: Vec<pi_agent::AgentTool>,
        generation_tools: Vec<pi_agent::AgentTool>,
    ) -> Self {
        let agent_dir = tempfile::tempdir().expect("fixture agent root");
        let session_dir = tempfile::tempdir().expect("fixture session root");
        setup(&workspace_path, agent_dir.path());
        let settings = agent_dir.path().join("settings.json");
        if !settings.exists() {
            std::fs::write(&settings, DANGER_SETTINGS).expect("fixture danger settings");
        }

        let suffix = uuid::Uuid::now_v7().to_string();
        let api = format!("{label}-api-{suffix}");
        let provider = format!("{label}-provider-{suffix}");
        let model = Model {
            id: format!("{label}-model"),
            name: format!("{label} Model"),
            api: api.clone(),
            provider: provider.clone(),
            base_url: "http://localhost:0".to_owned(),
            ..Model::default()
        };
        let registration = register_faux_provider(FauxProviderOptions {
            api,
            provider,
            models: vec![model.clone()],
            chunk_size: 1,
        });
        registration.set_responses(responses);

        let mut resource_options = ResourceManagerOptions::new(&workspace_path);
        resource_options.agent_dir = agent_dir.path().to_path_buf();
        resource_options.project_trust_override = Some(true);
        resource_options.disable_extensions = true;
        resource_options.disable_skills = true;
        resource_options.disable_prompt_templates = true;
        resource_options.disable_themes = true;
        resource_options.disable_context_files = true;
        configure_resources(&mut resource_options);
        let resources = ResourceManager::new(resource_options).expect("fixture resources");

        let session =
            Session::new_with_persistent_and_generation_tools_filtered_discovery_workspace_and_uri(
                SessionOptions {
                    model,
                    cwd: workspace_path.clone(),
                    system_prompt: resources
                        .snapshot()
                        .system_prompt
                        .clone()
                        .unwrap_or_default(),
                    thinking_level: ThinkingLevel::Off,
                    api_key: String::new(),
                    compaction: None,
                    stream_options: Default::default(),
                    tools: if ambient_tool_set { None } else { Some(Vec::new()) },
                    before_tool_call: None,
                    after_tool_call: None,
                    stream_fn,
                    auth_resolver: None,
                    auth_manager: None,
                },
                additional_tools,
                generation_tools,
                tools,
                ResourceDiscovery::Disabled,
                pi_coding::WorkspaceRoots::new(&workspace_path, Vec::<PathBuf>::new())
                    .expect("fixture workspace roots"),
                None,
                false,
                false,
            )
            .expect("fixture session");
        session.set_session_dir(session_dir.path().to_path_buf());
        session
            .start_new_recording()
            .expect("start fixture recording");
        Self {
            _workspace: workspace,
            cwd: workspace_path,
            agent_dir,
            session_dir,
            session,
            registration,
            resources,
        }
    }

    fn in_workspace(
        label: &str,
        responses: Vec<FauxResponse>,
        stream_fn: Option<pi_agent::StreamFn>,
        workspace: tempfile::TempDir,
        workspace_path: PathBuf,
        setup: impl FnOnce(&Path, &Path),
        configure_resources: impl FnOnce(&mut ResourceManagerOptions),
        tools: ToolSelection,
    ) -> Self {
        Self::hermetic(
            label,
            responses,
            stream_fn,
            workspace,
            workspace_path,
            setup,
            configure_resources,
            tools,
            false,
            Vec::new(),
            Vec::new(),
        )
    }

    /// Ambient tool set for MCP-specific fixtures only: `SessionOptions.tools`
    /// stays `None`, so the registry-bound `mcp` tool joins at construction and reload.
    fn in_workspace_ambient(
        label: &str,
        responses: Vec<FauxResponse>,
        stream_fn: Option<pi_agent::StreamFn>,
        workspace: tempfile::TempDir,
        workspace_path: PathBuf,
        setup: impl FnOnce(&Path, &Path),
        configure_resources: impl FnOnce(&mut ResourceManagerOptions),
        tools: ToolSelection,
    ) -> Self {
        Self::hermetic(
            label,
            responses,
            stream_fn,
            workspace,
            workspace_path,
            setup,
            configure_resources,
            tools,
            true,
            Vec::new(),
            Vec::new(),
        )
    }

    fn standard(
        label: &str,
        responses: Vec<FauxResponse>,
        stream_fn: Option<pi_agent::StreamFn>,
    ) -> Self {
        Self::new(
            label,
            responses,
            stream_fn,
            |_, _| {},
            |_| {},
            ToolSelection::default(),
        )
    }

    fn session(&self) -> Session {
        self.session.clone()
    }
    fn registration(&self) -> FauxProviderRegistration {
        self.registration.clone()
    }

    fn model_key(&self) -> (String, String) {
        let model = self.session.model().expect("fixture model");
        (model.provider, model.id)
    }

    fn workspace(&self) -> &Path {
        &self.cwd
    }

    fn agent_dir(&self) -> &Path {
        self.agent_dir.path()
    }

    fn session_dir(&self) -> &Path {
        self.session_dir.path()
    }

    fn resources(&self) -> ResourceManager {
        self.resources.clone()
    }

    async fn attach_resources(&self) {
        self.session
            .attach_resources(self.resources.clone())
            .await
            .expect("attach fixture resources");
    }

    async fn application(&self) -> Application {
        self.attach_resources().await;
        publish_application(ApplicationRuntimeCandidate::new(self.session())).await
    }

    async fn application_with_orchestration(&self, runtime: OrchestrationRuntime) -> Application {
        self.attach_resources().await;
        publish_application(
            ApplicationRuntimeCandidate::new(self.session()).with_orchestration(runtime),
        )
        .await
    }
}

impl Drop for HermeticFixture {
    fn drop(&mut self) {
        self.registration.unregister();
        pi_ai::replace_registered_models(&[self.model_key()], Vec::new());
    }
}

async fn publish_application(candidate: ApplicationRuntimeCandidate) -> Application {
    Application::from_runtime_candidate(candidate)
        .await
        .expect("publish application generation")
}

fn write_quickjs_manifest(
    directory: &Path,
    id: &str,
    entry: &Path,
    capabilities: &[&str],
) -> anyhow::Result<PathBuf> {
    let manifest = directory.join("pi-extension.json");
    std::fs::write(
        &manifest,
        serde_json::to_vec_pretty(&serde_json::json!({
            "schemaVersion": 1,
            "id": id,
            "runtime": "quickjs",
            "entry": entry
                .file_name()
                .and_then(std::ffi::OsStr::to_str)
                .expect("UTF-8 extension entry file name")
                .to_owned(),
            "capabilities": capabilities,
        }))?,
    )?;
    Ok(manifest)
}

fn session_with_responses(responses: Vec<FauxResponse>) -> HermeticFixture {
    HermeticFixture::standard("application-test", responses, None)
}

fn session_with_recorded_contexts(
    responses: Vec<FauxResponse>,
    contexts: Arc<parking_lot::Mutex<Vec<pi_ai::Context>>>,
) -> HermeticFixture {
    let stream_fn: pi_agent::StreamFn = Arc::new(move |model, context, options| {
        let contexts = contexts.clone();
        Box::pin(async move {
            contexts.lock().push(context.clone());
            pi_ai::stream_simple(model, context, options).await
        })
    });
    HermeticFixture::standard("application-context-test", responses, Some(stream_fn))
}

struct RuntimeOwners {
    _agent_dir: tempfile::TempDir,
    _session_dir: tempfile::TempDir,
}

#[derive(Clone, Default)]
struct TestRuntimeFactory {
    explicit_extension_paths: Arc<Vec<PathBuf>>,
    explicit_skill_paths: Arc<Vec<PathBuf>>,
    owners: Arc<parking_lot::Mutex<Vec<RuntimeOwners>>>,
}

impl TestRuntimeFactory {
    fn new(explicit_extension_paths: Vec<PathBuf>, explicit_skill_paths: Vec<PathBuf>) -> Self {
        Self {
            explicit_extension_paths: Arc::new(explicit_extension_paths),
            explicit_skill_paths: Arc::new(explicit_skill_paths),
            owners: Arc::new(parking_lot::Mutex::new(Vec::new())),
        }
    }
}

#[derive(Clone)]
struct FailingRuntimeFactory;

impl ApplicationRuntimeFactory for FailingRuntimeFactory {
    fn build_runtime_candidate(
        &self,
        _cwd: PathBuf,
        _options: SessionOptions,
        _resume: Option<pi_coding::PreparedSessionResume>,
    ) -> ApplicationRuntimeFuture {
        Box::pin(async { anyhow::bail!("candidate build failed") })
    }
}

impl ApplicationRuntimeFactory for TestRuntimeFactory {
    fn build_runtime_candidate(
        &self,
        cwd: PathBuf,
        mut options: SessionOptions,
        resume: Option<pi_coding::PreparedSessionResume>,
    ) -> ApplicationRuntimeFuture {
        let explicit_extension_paths = self.explicit_extension_paths.clone();
        let explicit_skill_paths = self.explicit_skill_paths.clone();
        let owners = self.owners.clone();
        Box::pin(async move {
            options.cwd.clone_from(&cwd);
            let permissions = ExtensionPermissionSet {
                capabilities: BTreeSet::from([ExtensionCapability::Commands]),
                ui_capabilities: BTreeSet::new(),
            };
            let agent_dir = tempfile::tempdir()?;
            let session_dir = tempfile::tempdir()?;
            std::fs::write(agent_dir.path().join("settings.json"), DANGER_SETTINGS)?;
            let mut resource_options = ResourceManagerOptions::new(&cwd);
            resource_options.agent_dir = agent_dir.path().to_path_buf();
            resource_options.project_trust_override = Some(true);
            resource_options.explicit_extension_paths = explicit_extension_paths.as_ref().clone();
            resource_options.explicit_skill_paths = explicit_skill_paths.as_ref().clone();
            resource_options.disable_extensions = true;
            resource_options.disable_skills = true;
            resource_options.disable_prompt_templates = true;
            resource_options.disable_themes = true;
            resource_options.disable_context_files = true;
            let resources = ResourceManager::new(resource_options)?;
            options.system_prompt = resources
                .snapshot()
                .system_prompt
                .clone()
                .unwrap_or_default();
            let session = Session::new_with_additional_tools_filtered_and_discovery(
                options,
                Vec::new(),
                ToolSelection {
                    enable_glob: true,
                    ..ToolSelection::default()
                },
                ResourceDiscovery::Disabled,
            )?;
            session.set_session_dir(session_dir.path().to_path_buf());
            session.attach_resources(resources).await?;
            if let Some(resume) = resume {
                let context = resume.build_context();
                let recorder = resume.into_recorder()?;
                session.load_history(context.messages).await?;
                session.record(recorder)?;
            }
            let runtime = ExtensionRuntime::process(
                None,
                ExtensionRuntimeOptions {
                    mode: ExtensionMode::Tui,
                    invocation_timeout: Duration::from_secs(5),
                    ..ExtensionRuntimeOptions::default()
                },
            );
            owners.lock().push(RuntimeOwners {
                _agent_dir: agent_dir,
                _session_dir: session_dir,
            });
            Ok(ApplicationRuntimeCandidate::new(session).with_extensions(runtime, permissions))
        })
    }
}

#[derive(Clone)]
struct GatedRuntimeFactory {
    inner: TestRuntimeFactory,
    calls: Arc<AtomicUsize>,
    first_entered: CancellationToken,
    release_first: CancellationToken,
}

impl GatedRuntimeFactory {
    fn new() -> Self {
        Self {
            inner: TestRuntimeFactory::new(Vec::new(), Vec::new()),
            calls: Arc::new(AtomicUsize::new(0)),
            first_entered: CancellationToken::new(),
            release_first: CancellationToken::new(),
        }
    }
}

impl ApplicationRuntimeFactory for GatedRuntimeFactory {
    fn build_runtime_candidate(
        &self,
        cwd: PathBuf,
        options: SessionOptions,
        resume: Option<pi_coding::PreparedSessionResume>,
    ) -> ApplicationRuntimeFuture {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        let entered = self.first_entered.clone();
        let release = self.release_first.clone();
        let future = self.inner.build_runtime_candidate(cwd, options, resume);
        Box::pin(async move {
            if call == 1 {
                entered.cancel();
                release.cancelled().await;
            }
            future.await
        })
    }
}

fn write_session(path: &Path, cwd: &Path, id: &str, message: &str) {
    let body = [
        serde_json::json!({
            "type": "session",
            "version": 3,
            "id": id,
            "timestamp": "2026-08-02T00:00:00.000Z",
            "cwd": cwd,
        }),
        serde_json::json!({
            "type": "message",
            "id": format!("{id}-message"),
            "parentId": null,
            "timestamp": "2026-08-02T00:00:01.000Z",
            "message": pi_ai::Message::user_text(message, 1),
        }),
    ];
    std::fs::write(
        path,
        body.into_iter()
            .map(|record| serde_json::to_string(&record).expect("serialize"))
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .expect("write session");
}

fn write_fake_mcp_server(path: &Path, pid_file: &Path) {
    let script = r#"import json, os, sys
with open(sys.argv[1], 'a', encoding='utf-8') as trace:
    trace.write(str(os.getpid()) + '\n')
    trace.flush()
reader = sys.stdin.buffer
writer = sys.stdout.buffer
def receive():
    length = None
    while True:
        line = reader.readline()
        if not line:
            raise SystemExit(0)
        if line in (b'\r\n', b'\n'):
            break
        name, value = line.decode().split(':', 1)
        if name.lower() == 'content-length':
            length = int(value.strip())
    return json.loads(reader.read(length))
def send(value):
    body = json.dumps(value, separators=(',', ':')).encode()
    writer.write(('Content-Length: %d\r\n\r\n' % len(body)).encode() + body)
    writer.flush()
while True:
    message = receive()
    method = message.get('method')
    request_id = message.get('id')
    if method == 'initialize':
        send({'jsonrpc':'2.0','id':request_id,'result':{'protocolVersion':'2024-11-05','capabilities':{'tools':{}},'serverInfo':{'name':'cutover-test','version':'1'}}})
    elif method == 'tools/call':
        text = message.get('params', {}).get('arguments', {}).get('message', '')
        send({'jsonrpc':'2.0','id':request_id,'result':{'content':[{'type':'text','text':text}]}})
    elif method == 'shutdown':
        send({'jsonrpc':'2.0','id':request_id,'result':None})
    elif method == 'notifications/exit':
        break
"#;
    std::fs::write(path, script).expect("fake MCP server script");
    std::fs::write(pid_file, "").expect("fake MCP pid trace");
}

fn read_pid_lines(path: &Path) -> Vec<u32> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.parse().ok())
        .collect()
}

fn process_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

async fn application_mcp_echo(application: &Application, message: &str) -> AgentToolResult {
    let tool = application
        .get_tool_definition("mcp")
        .expect("application MCP tool");
    let (_, abort) = AbortController::new();
    (tool.execute)(ToolCallContext {
        tool_call_id: format!("mcp-{message}"),
        arguments: serde_json::json!({
            "action": "call",
            "server": "fake",
            "tool": "echo",
            "args": { "message": message },
        }),
        on_update: Arc::new(|_| {}),
        abort,
        model: None,
        approval_token: None,
    })
    .await
    .expect("MCP echo")
}

fn result_contains(result: &AgentToolResult, needle: &str) -> bool {
    result
        .content
        .iter()
        .any(|block| matches!(block, ContentBlock::Text { text, .. } if text.contains(needle)))
}

#[tokio::test]
async fn same_cwd_late_loop_persist_failure_preserves_live_mcp_pid() {
    let cwd = tempfile::tempdir().expect("cwd");
    let cwd_path = cwd.path().to_path_buf();
    let source_file = cwd_path.join("source.jsonl");
    let target_file = cwd_path.join("target.jsonl");
    let server = cwd_path.join("fake_mcp.py");
    let pid_file = cwd_path.join("mcp-pids.txt");
    write_session(&source_file, &cwd_path, "source-mcp", "source message");
    write_session(&target_file, &cwd_path, "target-mcp", "target message");
    write_fake_mcp_server(&server, &pid_file);
    let server_setting = server.clone();
    let pid_file_setting = pid_file.clone();
    let fixture = HermeticFixture::in_workspace_ambient(
        "same-cwd-mcp-late-failure",
        Vec::new(),
        None,
        cwd,
        cwd_path,
        move |_, agent_dir| {
            std::fs::write(
                agent_dir.join("settings.json"),
                serde_json::to_vec(&serde_json::json!({
                    "sandbox": { "mode": "danger-full-access" },
                    "mcpServers": [{
                        "name": "fake",
                        "command": "/usr/bin/python3",
                        "args": [server_setting, pid_file_setting],
                    }],
                }))
                .expect("MCP settings JSON"),
            )
            .expect("MCP settings");
        },
        |_| {},
        ToolSelection::default(),
    );
    let session = fixture.session();
    session
        .switch_session(&source_file)
        .await
        .expect("source resume");
    let application = fixture.application().await;

    let before = application_mcp_echo(&application, "before").await;
    assert!(result_contains(&before, "before"), "{before:?}");
    let old_pid = read_pid_lines(&pid_file)[0];
    assert!(process_alive(old_pid), "old MCP client must be live");

    application
        .set_loop_persist_failure_for_test(true)
        .expect("arm loop persistence failure");
    let error = application
        .switch_session(&target_file)
        .await
        .expect_err("late loop persistence must fail same-CWD cutover");
    assert!(
        error.to_string().contains("persist loop scheduler state"),
        "{error:#}"
    );
    assert!(
        process_alive(old_pid),
        "failed preparation must preserve the exact MCP client"
    );
    let reused = application_mcp_echo(&application, "reused").await;
    assert!(result_contains(&reused, "reused"), "{reused:?}");
    assert_eq!(
        read_pid_lines(&pid_file),
        [old_pid],
        "failed cutover must reuse the same PID"
    );

    application
        .set_loop_persist_failure_for_test(false)
        .expect("disarm loop persistence failure");
    assert!(
        !application
            .switch_session(&target_file)
            .await
            .expect("successful retry")
            .cancelled
    );
    assert!(
        !process_alive(old_pid),
        "successful cutover must reap the old MCP client"
    );
    let fresh = application_mcp_echo(&application, "fresh").await;
    assert!(result_contains(&fresh, "fresh"), "{fresh:?}");
    let pids = read_pid_lines(&pid_file);
    assert_eq!(
        pids.len(),
        2,
        "successful cutover must spawn one fresh client: {pids:?}"
    );
    assert_ne!(pids[1], old_pid, "successful cutover must use a fresh PID");
    assert!(process_alive(pids[1]), "fresh MCP client must be live");
}

/// Caller-owned persistent probe: counts calls, returns call-indexed results.
fn persistent_probe_tool(calls: Arc<AtomicUsize>) -> pi_agent::AgentTool {
    pi_agent::AgentTool::new(
        "custom_persistent",
        "Caller-owned persistent probe tool",
        pi_ai::Schema::default(),
        move |_| {
            let calls = calls.clone();
            async move {
                let call = calls.fetch_add(1, Ordering::SeqCst) + 1;
                Ok(AgentToolResult::text(format!("persistent-call-{call}")))
            }
        },
    )
}

/// Persistent custom tools stay callable across attach, bootstrap, and repeated reloads.
#[tokio::test]
async fn persistent_custom_tool_survives_bootstrap_and_repeated_reload() {
    let workspace = tempfile::tempdir().expect("workspace");
    let workspace_path = workspace.path().to_path_buf();
    let calls = Arc::new(AtomicUsize::new(0));
    let fixture = HermeticFixture::hermetic(
        "persistent-custom",
        Vec::new(),
        None,
        workspace,
        workspace_path,
        |_, _| {},
        |_| {},
        ToolSelection::default(),
        false,
        vec![persistent_probe_tool(calls.clone())],
        Vec::new(),
    );
    let application = fixture.application().await;

    async fn probe(application: &Application, expected: &str) {
        let tool = application
            .get_tool_definition("custom_persistent")
            .expect("persistent custom tool published in the generation");
        let (_, abort) = AbortController::new();
        let result = (tool.execute)(ToolCallContext {
            tool_call_id: "persistent-probe".to_owned(),
            arguments: serde_json::json!({}),
            on_update: Arc::new(|_| {}),
            abort,
            model: None,
            approval_token: None,
        })
        .await
        .expect("persistent custom tool call");
        assert!(
            result_contains(&result, expected),
            "persistent custom tool behavior changed: {result:?}"
        );
    }
    let present = |application: &Application| {
        application
            .get_all_tools()
            .into_iter()
            .any(|tool| tool.name == "custom_persistent")
    };

    probe(&application, "persistent-call-1").await;
    assert!(present(&application));
    assert!(application
        .get_active_tool_names()
        .iter()
        .any(|name| name == "custom_persistent"));

    application.reload().await.expect("first reload");
    probe(&application, "persistent-call-2").await;
    assert!(present(&application));
    assert!(application
        .get_active_tool_names()
        .iter()
        .any(|name| name == "custom_persistent"));

    application.reload().await.expect("second reload");
    probe(&application, "persistent-call-3").await;
    assert!(present(&application));
    assert!(application
        .get_active_tool_names()
        .iter()
        .any(|name| name == "custom_persistent"));
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    application.cleanup().await;
}

/// Generation tools active at bootstrap must vanish from all-tools and active when settings disable them.
#[tokio::test]
async fn generation_tools_enabled_at_bootstrap_are_removed_when_settings_disable() {
    let workspace = tempfile::tempdir().expect("workspace");
    let workspace_path = workspace.path().to_path_buf();
    let artifacts = tempfile::tempdir().expect("artifacts");
    let runtime = irc_runtime(artifacts.path());
    let generation_tools = runtime.agent_tools("Main", 0);
    let fixture = HermeticFixture::hermetic(
        "bootstrap-enabled-removal",
        Vec::new(),
        None,
        workspace,
        workspace_path,
        |_, _| {},
        |_| {},
        ToolSelection::default(),
        false,
        Vec::new(),
        generation_tools,
    );
    let application = fixture.application_with_orchestration(runtime).await;
    let names = |application: &Application| {
        application
            .get_all_tools()
            .into_iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>()
    };
    for name in ["task", "hub"] {
        assert!(
            names(&application).iter().any(|tool| tool == name),
            "missing {name} from all_tools at bootstrap"
        );
        assert!(
            application
                .get_active_tool_names()
                .iter()
                .any(|tool| tool == name),
            "missing active {name} at bootstrap"
        );
    }

    fixture
        .resources()
        .settings_manager()
        .expect("settings manager")
        .update_global(|settings| {
            settings.orchestration = Some(OrchestrationSettings {
                tasks: Some(false),
                process: Some(false),
                todo: Some(false),
                glob: Some(false),
                ..OrchestrationSettings::default()
            });
        })
        .expect("disable orchestration settings");
    application.reload().await.expect("disable reload");
    for name in ["task", "hub"] {
        assert!(
            !names(&application).iter().any(|tool| tool == name),
            "{name} must leave all_tools after disabling reload"
        );
        assert!(
            !application
                .get_active_tool_names()
                .iter()
                .any(|tool| tool == name),
            "{name} must leave active tools after disabling reload"
        );
    }
    application.cleanup().await;
}

#[tokio::test]
async fn cross_cwd_switch_replaces_generation_and_preserves_old_session_lease() {
    let source = tempfile::tempdir().expect("source cwd");
    let target = tempfile::tempdir().expect("target cwd");
    let source_file = source.path().join("source.jsonl");
    let target_file = target.path().join("target.jsonl");
    write_session(&source_file, source.path(), "source", "source message");
    write_session(&target_file, target.path(), "target", "target message");
    std::fs::write(source.path().join("source-only.txt"), "source").expect("source marker");
    std::fs::write(target.path().join("target-only.txt"), "target").expect("target marker");
    let target_assets = tempfile::tempdir().expect("target assets");
    let target_skill = target_assets.path().join("target-skill");
    std::fs::create_dir_all(&target_skill).expect("target skill directory");
    std::fs::write(
        target_skill.join("SKILL.md"),
        "---\nname: target-skill\ndescription: target-only runtime resource\n---\ntarget body\n",
    )
    .expect("target skill");
    let extension_entry = target_assets.path().join("target-probe.mjs");
    std::fs::write(
        &extension_entry,
        r#"export default function (pi) {
  pi.registerCommand("target-probe", { handler: () => "target-extension" });
}"#,
    )
    .expect("target extension");
    let extension_manifest = write_quickjs_manifest(
        target_assets.path(),
        "target-probe",
        &extension_entry,
        &["commands"],
    )
    .expect("target extension manifest");
    let has_target_extension = true;

    let fixture =
        HermeticFixture::for_workspace("cross-cwd-source", source.path(), Vec::new(), None);
    let session = fixture.session();
    session
        .switch_session(&source_file)
        .await
        .expect("attach source recording");
    let application = fixture.application().await;
    application
        .attach_runtime_factory(Arc::new(TestRuntimeFactory::new(
            vec![extension_manifest],
            vec![target_skill],
        )))
        .expect("factory");
    let clone = application.clone();
    let retained = application.session();
    let mut events = application.subscribe();
    let source_epoch = application.runtime_epoch();

    let switch = tokio::time::timeout(
        Duration::from_secs(5),
        application.switch_session(&target_file),
    )
    .await
    .expect("cross cwd switch timeout")
    .expect("cross cwd switch");
    assert!(!switch.cancelled);

    assert!(application.runtime_epoch() > source_epoch);
    assert_eq!(clone.session().cwd(), target.path());
    assert_eq!(retained.cwd(), source.path());
    assert_eq!(
        application.resource_generation(),
        Some(1),
        "target runtime must publish its own bootstrap generation",
    );
    assert_eq!(
        application
            .durable_policy_state()
            .expect("target durable policy")
            .snapshot()
            .revision()
            .get(),
        1,
        "target process/tools must capture the target publication",
    );
    assert_eq!(
        application.messages(),
        vec![pi_ai::Message::user_text("target message", 1)]
    );
    let resources = application
        .session()
        .resource_manager()
        .expect("target resources")
        .snapshot();
    assert_eq!(resources.cwd, target.path());
    assert!(
        resources
            .skills
            .iter()
            .any(|skill| skill.name == "target-skill")
    );
    if has_target_extension {
        let runtime = application
            .extension_runtime()
            .expect("target extension runtime");
        assert_eq!(
            runtime
                .invoke_command("target-probe", String::new(), None, None)
                .await
                .expect("target extension command"),
            serde_json::Value::String("target-extension".to_owned())
        );
    }
    let bash = tokio::time::timeout(
        Duration::from_secs(5),
        application.execute_bash("pwd; cat target-only.txt".to_owned(), false),
    )
    .await
    .expect("target bash timeout")
    .expect("target bash");
    assert!(
        bash.output
            .contains(&target.path().to_string_lossy().into_owned())
    );
    assert!(bash.output.contains("target"));
    let glob = application
        .session()
        .get_tool_definition("glob")
        .expect("target glob tool");
    let (_, target_glob_abort) = AbortController::new();
    let target_glob = tokio::time::timeout(
        Duration::from_secs(5),
        (glob.execute)(pi_agent::ToolCallContext {
            tool_call_id: "target-glob".to_owned(),
            arguments: serde_json::json!({"pattern": "*"}),
            on_update: Arc::new(|_| {}),
            abort: target_glob_abort,
            model: None,
            approval_token: None,
        }),
    )
    .await
    .expect("target glob timeout")
    .expect("target glob response");
    let target_glob_text = target_glob
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert!(
        target_glob_text.contains("target-only.txt"),
        "{target_glob:?}"
    );
    assert!(
        !target_glob_text.contains("source-only.txt"),
        "{target_glob:?}"
    );
    let read = application
        .session()
        .get_tool_definition("read")
        .expect("target read tool");
    let (_, target_abort) = AbortController::new();
    let target_read = tokio::time::timeout(
        Duration::from_secs(5),
        (read.execute)(pi_agent::ToolCallContext {
            tool_call_id: "target-read".to_owned(),
            arguments: serde_json::json!({"path": "target-only.txt"}),
            on_update: Arc::new(|_| {}),
            abort: target_abort,
            model: None,
            approval_token: None,
        }),
    )
    .await
    .expect("target read timeout")
    .expect("target read response");
    assert!(!target_read.content.is_empty());
    let barrier = tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .expect("runtime barrier timeout")
        .expect("runtime barrier");
    assert!(matches!(
        barrier,
        ApplicationEvent::RuntimeChanged { epoch } if epoch == application.runtime_epoch()
    ));
    std::mem::forget(retained);
    std::mem::forget(clone);
    std::mem::forget(application);
}

#[tokio::test]
async fn cross_cwd_switch_drains_queued_goal_work_from_old_generation() {
    let source = tempfile::tempdir().expect("source cwd");
    let target = tempfile::tempdir().expect("target cwd");
    let source_file = source.path().join("source.jsonl");
    let target_file = target.path().join("target.jsonl");
    write_session(&source_file, source.path(), "source-goal", "source message");
    write_session(&target_file, target.path(), "target-goal", "target message");
    let target_assets = tempfile::tempdir().expect("target assets");
    let extension_entry = target_assets.path().join("target-probe.mjs");
    std::fs::write(
        &extension_entry,
        r#"export default function (pi) {
  pi.registerCommand("target-probe", { handler: () => "target-extension" });
}"#,
    )
    .expect("target extension");
    let extension_manifest = write_quickjs_manifest(
        target_assets.path(),
        "target-probe",
        &extension_entry,
        &["commands"],
    )
    .expect("target extension manifest");

    let first_started = Arc::new(Notify::new());
    let release_first = CancellationToken::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let stream_started = first_started.clone();
    let stream_release = release_first.clone();
    let stream_calls = calls.clone();
    let stream_fn: pi_agent::StreamFn = Arc::new(move |model, _context, options| {
        let started = stream_started.clone();
        let release = stream_release.clone();
        let calls = stream_calls.clone();
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            let stream = pi_ai::new_assistant_message_event_stream();
            let producer = stream.clone();
            tokio::spawn(async move {
                started.notify_waiters();
                let abort = options.stream.abort_signal;
                tokio::select! {
                    () = release.cancelled() => {}
                    () = async {
                        match abort {
                            Some(signal) => signal.cancelled().await,
                            None => std::future::pending::<()>().await,
                        }
                    } => {}
                }
                let mut message = pi_ai::AssistantMessage::pending(&model);
                message
                    .content
                    .push(ContentBlock::text("source turn finished"));
                message.usage = Usage::default();
                message.stop_reason = StopReason::Stop;
                producer.end(Some(message)).await;
            });
            stream
        })
    });
    let fixture = HermeticFixture::for_workspace(
        "cross-cwd-goal-source",
        source.path(),
        Vec::new(),
        Some(stream_fn),
    );
    let session = fixture.session();
    session
        .switch_session(&source_file)
        .await
        .expect("source resume");
    let application = fixture.application().await;
    application
        .attach_runtime_factory(Arc::new(TestRuntimeFactory::new(
            vec![extension_manifest],
            Vec::new(),
        )))
        .expect("factory");
    application
        .goal_create("generation-safe goal", Some(100))
        .expect("goal metadata");

    application
        .prompt("hold source turn".to_owned(), Vec::new(), None)
        .await
        .expect("source prompt");
    first_started.notified().await;
    assert_eq!(
        application
            .resume_goal_work()
            .await
            .expect("queue goal work"),
        GoalActivationOutcome::Queued
    );

    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        application.switch_session(&target_file),
    )
    .await
    .expect("cross-cwd switch must not hang")
    .expect("cross-cwd switch");
    assert!(!outcome.cancelled);
    assert_eq!(application.session().cwd(), target.path());
    assert!(
        !application.is_streaming(),
        "new generation must not inherit old goal pending work"
    );
    application.wait_for_idle().await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "queued old-generation goal work must not start"
    );
    release_first.cancel();
}

#[tokio::test]
async fn cross_cwd_candidate_failure_preserves_old_runtime() {
    let source = tempfile::tempdir().expect("source cwd");
    let target = tempfile::tempdir().expect("target cwd");
    let source_file = source.path().join("source.jsonl");
    let target_file = target.path().join("target.jsonl");
    write_session(&source_file, source.path(), "source", "source message");
    write_session(&target_file, target.path(), "target", "target message");
    let fixture =
        HermeticFixture::for_workspace("cross-cwd-failure-source", source.path(), Vec::new(), None);
    let session = fixture.session();
    session
        .switch_session(&source_file)
        .await
        .expect("source resume");
    let application = fixture.application().await;
    application
        .attach_runtime_factory(Arc::new(FailingRuntimeFactory))
        .expect("factory");
    let epoch = application.runtime_epoch();
    let error = application
        .switch_session(&target_file)
        .await
        .expect_err("candidate must fail");
    assert!(error.to_string().contains("candidate build failed"));
    assert_eq!(application.runtime_epoch(), epoch);
    assert_eq!(application.session().cwd(), source.path());
    assert_eq!(
        application.messages(),
        [pi_ai::Message::user_text("source message", 1)]
    );
}

#[tokio::test]
async fn cross_cwd_late_loop_persist_failure_preserves_queued_goal_activation() {
    let source = tempfile::tempdir().expect("source cwd");
    let target = tempfile::tempdir().expect("target cwd");
    let source_file = source.path().join("source.jsonl");
    let target_file = target.path().join("target.jsonl");
    write_session(
        &source_file,
        source.path(),
        "source-late-failure",
        "source message",
    );
    write_session(
        &target_file,
        target.path(),
        "target-late-failure",
        "target message",
    );

    let release_first = CancellationToken::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
    let stream_release = release_first.clone();
    let stream_calls = calls.clone();
    let stream_fn: pi_agent::StreamFn = Arc::new(move |model, _context, _options| {
        let release = stream_release.clone();
        let calls = stream_calls.clone();
        let started = started_tx.clone();
        Box::pin(async move {
            let call = calls.fetch_add(1, Ordering::SeqCst) + 1;
            started.send(call).expect("observe provider call");
            let stream = pi_ai::new_assistant_message_event_stream();
            let producer = stream.clone();
            tokio::spawn(async move {
                if call == 1 {
                    release.cancelled().await;
                }
                let mut message = pi_ai::AssistantMessage::pending(&model);
                message
                    .content
                    .push(ContentBlock::text(format!("source turn {call}")));
                message.usage = Usage::default();
                message.stop_reason = StopReason::Stop;
                producer.end(Some(message)).await;
            });
            stream
        })
    });
    let fixture = HermeticFixture::for_workspace(
        "cross-cwd-late-loop-persist",
        source.path(),
        Vec::new(),
        Some(stream_fn),
    );
    let session = fixture.session();
    session
        .switch_session(&source_file)
        .await
        .expect("source resume");
    let application = fixture.application().await;
    application
        .attach_runtime_factory(Arc::new(TestRuntimeFactory::new(Vec::new(), Vec::new())))
        .expect("factory");
    application
        .goal_create("survive late cutover failure", Some(100))
        .expect("goal");
    application
        .prompt("hold old generation".to_owned(), Vec::new(), None)
        .await
        .expect("source prompt");
    assert_eq!(started_rx.recv().await, Some(1));
    assert_eq!(
        application
            .resume_goal_work()
            .await
            .expect("queue goal work"),
        GoalActivationOutcome::Queued,
    );

    let epoch = application.runtime_epoch();
    let session_id = application.state().await.session_id;
    application
        .set_loop_persist_failure_for_test(true)
        .expect("arm loop persist failure");
    let error = application
        .switch_session(&target_file)
        .await
        .expect_err("late loop persistence must fail the switch");
    assert!(
        error.to_string().contains("persist loop scheduler state"),
        "{error:#}"
    );
    assert_eq!(
        application.runtime_epoch(),
        epoch,
        "failed cutover must retain generation"
    );
    assert_eq!(
        application.session().cwd(),
        source.path(),
        "failed cutover must retain cwd"
    );
    assert_eq!(
        application.state().await.session_id,
        session_id,
        "failed cutover must retain session"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "queued activation must not run before the old turn releases"
    );

    application
        .set_loop_persist_failure_for_test(false)
        .expect("disarm loop persist failure");
    release_first.cancel();
    assert_eq!(
        started_rx.recv().await,
        Some(2),
        "queued activation must execute without retry"
    );
    application.wait_for_idle().await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "queued activation must execute exactly once"
    );
    assert_eq!(application.runtime_epoch(), epoch);
    assert_eq!(application.session().cwd(), source.path());
}

fn controlled_goal_stream() -> (
    pi_agent::StreamFn,
    Arc<AtomicUsize>,
    tokio::sync::mpsc::UnboundedReceiver<usize>,
    CancellationToken,
) {
    let release_first = CancellationToken::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let (started_tx, started_rx) = tokio::sync::mpsc::unbounded_channel();
    let stream_release = release_first.clone();
    let stream_calls = calls.clone();
    let stream_fn: pi_agent::StreamFn = Arc::new(move |model, _context, _options| {
        let release = stream_release.clone();
        let calls = stream_calls.clone();
        let started = started_tx.clone();
        Box::pin(async move {
            let call = calls.fetch_add(1, Ordering::SeqCst) + 1;
            started.send(call).expect("observe provider call");
            let stream = pi_ai::new_assistant_message_event_stream();
            let producer = stream.clone();
            tokio::spawn(async move {
                if call == 1 {
                    release.cancelled().await;
                }
                let mut message = pi_ai::AssistantMessage::pending(&model);
                message
                    .content
                    .push(ContentBlock::text(format!("source turn {call}")));
                message.usage = Usage::default();
                message.stop_reason = StopReason::Stop;
                producer.end(Some(message)).await;
            });
            stream
        })
    });
    (stream_fn, calls, started_rx, release_first)
}

#[tokio::test]
async fn dropped_cross_cwd_switch_after_loop_reservation_restores_old_goal_admission() {
    let source = tempfile::tempdir().expect("source cwd");
    let target = tempfile::tempdir().expect("target cwd");
    let source_file = source.path().join("source.jsonl");
    let target_file = target.path().join("target.jsonl");
    write_session(
        &source_file,
        source.path(),
        "source-drop-reservation",
        "source message",
    );
    write_session(
        &target_file,
        target.path(),
        "target-drop-reservation",
        "target message",
    );
    let (stream_fn, calls, mut started, release_first) = controlled_goal_stream();
    let fixture = HermeticFixture::for_workspace(
        "cross-cwd-drop-reservation",
        source.path(),
        Vec::new(),
        Some(stream_fn),
    );
    let session = fixture.session();
    session
        .switch_session(&source_file)
        .await
        .expect("source resume");
    let application = fixture.application().await;
    application
        .attach_runtime_factory(Arc::new(TestRuntimeFactory::new(Vec::new(), Vec::new())))
        .expect("factory");
    application
        .goal_create("survive dropped switch", Some(100))
        .expect("goal");
    application
        .prompt("hold source".to_owned(), Vec::new(), None)
        .await
        .expect("prompt");
    assert_eq!(started.recv().await, Some(1));
    assert_eq!(
        application.resume_goal_work().await.expect("queue goal"),
        GoalActivationOutcome::Queued
    );
    let gate = application.arm_cutover_before_spawn_gate_for_test();
    let switch = tokio::spawn({
        let application = application.clone();
        let target_file = target_file.clone();
        async move { application.switch_session(&target_file).await }
    });
    gate.wait_entered().await;
    switch.abort();
    let _ = switch.await;
    gate.release();
    release_first.cancel();
    assert_eq!(started.recv().await, Some(2));
    application.wait_for_idle().await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(application.session().cwd(), source.path());
    assert!(
        application.loop_list().await.is_ok(),
        "reservation drop must restore loop admission"
    );
}

#[tokio::test]
async fn dropped_cross_cwd_switch_after_commit_still_completes_detached_transaction() {
    let source = tempfile::tempdir().expect("source cwd");
    let target = tempfile::tempdir().expect("target cwd");
    let source_file = source.path().join("source.jsonl");
    let target_file = target.path().join("target.jsonl");
    write_session(
        &source_file,
        source.path(),
        "source-drop-commit",
        "source message",
    );
    write_session(
        &target_file,
        target.path(),
        "target-drop-commit",
        "target message",
    );
    let fixture =
        HermeticFixture::for_workspace("cross-cwd-drop-commit", source.path(), Vec::new(), None);
    let session = fixture.session();
    session
        .switch_session(&source_file)
        .await
        .expect("source resume");
    let application = fixture.application().await;
    application
        .attach_runtime_factory(Arc::new(TestRuntimeFactory::new(Vec::new(), Vec::new())))
        .expect("factory");
    let gate = application.arm_cutover_after_loop_commit_gate_for_test();
    let source_epoch = application.runtime_epoch();
    let mut events = application.subscribe();
    let switch = tokio::spawn({
        let application = application.clone();
        let target_file = target_file.clone();
        async move { application.switch_session(&target_file).await }
    });
    gate.wait_entered().await;
    switch.abort();
    let _ = switch.await;
    gate.release();
    loop {
        match events.recv().await.expect("runtime event") {
            ApplicationEvent::RuntimeChanged { epoch } => {
                assert!(epoch > source_epoch);
                break;
            }
            _ => {}
        }
    }
    assert_eq!(application.session().cwd(), target.path());
    assert_eq!(
        application.messages(),
        [pi_ai::Message::user_text("target message", 1)]
    );
    assert!(application.loop_list().await.is_ok());
}

#[tokio::test]
async fn concurrent_cross_cwd_stale_candidate_cannot_overwrite_newer_runtime() {
    let source = tempfile::tempdir().expect("source cwd");
    let first_target = tempfile::tempdir().expect("first target cwd");
    let second_target = tempfile::tempdir().expect("second target cwd");
    let source_file = source.path().join("source.jsonl");
    let first_file = first_target.path().join("first.jsonl");
    let second_file = second_target.path().join("second.jsonl");
    write_session(
        &source_file,
        source.path(),
        "source-concurrent",
        "source message",
    );
    write_session(
        &first_file,
        first_target.path(),
        "first-concurrent",
        "first message",
    );
    write_session(
        &second_file,
        second_target.path(),
        "second-concurrent",
        "second message",
    );
    let fixture =
        HermeticFixture::for_workspace("cross-cwd-concurrent", source.path(), Vec::new(), None);
    let session = fixture.session();
    session
        .switch_session(&source_file)
        .await
        .expect("source resume");
    let application = fixture.application().await;
    let factory = Arc::new(GatedRuntimeFactory::new());
    application
        .attach_runtime_factory(factory.clone())
        .expect("factory");
    let first = tokio::spawn({
        let application = application.clone();
        let first_file = first_file.clone();
        async move { application.switch_session(&first_file).await }
    });
    factory.first_entered.cancelled().await;
    application
        .switch_session(&second_file)
        .await
        .expect("second switch");
    assert_eq!(application.session().cwd(), second_target.path());
    factory.release_first.cancel();
    let error = first
        .await
        .expect("first task")
        .expect_err("stale candidate must be rejected");
    assert!(
        error
            .to_string()
            .contains("runtime changed during cross-directory cutover staging"),
        "{error:#}"
    );
    assert_eq!(application.session().cwd(), second_target.path());
    assert_eq!(
        application.messages(),
        [pi_ai::Message::user_text("second message", 1)]
    );
}

async fn semantic_application_with_source(
    fixture: &HermeticFixture,
    source: Option<&str>,
    prompt: Option<&str>,
) -> anyhow::Result<(Application, ExtensionRuntime, tempfile::TempDir)> {
    let directory = tempfile::tempdir()?;
    let entry = directory.path().join("extension.mjs");
    if let Some(source) = source {
        std::fs::write(&entry, source)?;
    } else {
        std::fs::copy(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/extensions/quickjs-hook-transforms.mjs"),
            &entry,
        )?;
    }
    let manifest = write_quickjs_manifest(
        directory.path(),
        "hook-transforms",
        &entry,
        &["event_hooks"],
    )?;
    let permissions = ExtensionPermissionSet {
        capabilities: BTreeSet::from([ExtensionCapability::EventHooks]),
        ui_capabilities: BTreeSet::new(),
    };
    let runtime = ExtensionRuntime::process(
        None,
        ExtensionRuntimeOptions {
            mode: ExtensionMode::Tui,
            hook_timeout: Duration::from_secs(10),
            ..ExtensionRuntimeOptions::default()
        },
    );
    let mut options = ResourceManagerOptions::new(fixture.workspace());
    options.agent_dir = fixture.agent_dir().to_path_buf();
    options.project_trust_override = Some(true);
    options.explicit_extension_paths = vec![manifest];
    options.disable_extensions = true;
    options.disable_skills = true;
    options.disable_prompt_templates = true;
    options.disable_themes = true;
    options.disable_context_files = true;
    let resources = ResourceManager::new(options)?;
    fixture.session().attach_resources(resources).await?;
    let application = publish_application(
        ApplicationRuntimeCandidate::new(fixture.session())
            .with_extensions(runtime.clone(), permissions),
    )
    .await;
    if let Some(prompt) = prompt {
        application
            .prompt(prompt.to_owned(), Vec::new(), None)
            .await?;
        application.wait_for_idle().await;
    }
    Ok((application, runtime, directory))
}

#[tokio::test]
async fn publishes_agent_events_and_settled_signal() {
    let fixture = session_with_responses(vec![FauxResponse::text("hello")]);
    let registration = fixture.registration();
    let application = fixture.application().await;
    let mut events = application.subscribe();

    application
        .prompt("say hello".to_owned(), Vec::new(), None)
        .await
        .expect("accept prompt");

    let mut saw_text = false;
    let mut saw_settled = false;
    while !saw_settled {
        let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("event timeout")
            .expect("event channel");
        match event {
            ApplicationEvent::Agent(AgentEvent::MessageUpdate {
                assistant_message_event: pi_ai::AssistantMessageEvent::TextDelta { delta, .. },
                ..
            })
            | ApplicationEvent::AgentPublished {
                event:
                    AgentEvent::MessageUpdate {
                        assistant_message_event: pi_ai::AssistantMessageEvent::TextDelta { delta, .. },
                        ..
                    },
                ..
            } if !delta.is_empty() => saw_text = true,
            ApplicationEvent::AgentSettled => saw_settled = true,
            _ => {}
        }
    }

    application.wait_for_idle().await;
    assert!(saw_text);
    assert_eq!(application.state().await.message_count, 2);
    registration.unregister();
}

#[tokio::test]
async fn rejects_another_prompt_without_streaming_behavior() {
    let long_text = "streaming response ".repeat(200);
    let fixture = session_with_responses(vec![FauxResponse {
        content: vec![ContentBlock::text(long_text)],
        stop_reason: StopReason::Stop,
        error_message: None,
    }]);
    let registration = fixture.registration();
    let application = fixture.application().await;

    application
        .prompt("first".to_owned(), Vec::new(), None)
        .await
        .expect("accept first prompt");
    let error = application
        .prompt("second".to_owned(), Vec::new(), None)
        .await
        .expect_err("reject concurrent prompt");
    assert!(error.downcast_ref::<SessionAlreadyProcessing>().is_some());
    assert!(error.to_string().contains("choose steer or followUp"));

    application.abort().await;
    application.wait_for_idle().await;
    registration.unregister();
}

#[tokio::test]
async fn accepts_steering_during_an_active_run() {
    let long_text = "streaming response ".repeat(200);
    let fixture = session_with_responses(vec![
        FauxResponse {
            content: vec![ContentBlock::text(long_text)],
            stop_reason: StopReason::Stop,
            error_message: None,
        },
        FauxResponse::text("steered answer"),
    ]);
    let registration = fixture.registration();
    let application = fixture.application().await;

    application
        .prompt("first".to_owned(), Vec::new(), None)
        .await
        .expect("accept first prompt");
    application
        .prompt(
            "change direction".to_owned(),
            Vec::new(),
            Some(StreamingBehavior::Steer),
        )
        .await
        .expect("accept steering prompt");

    application.wait_for_idle().await;
    assert!(application.state().await.message_count >= 2);
    registration.unregister();
}

#[tokio::test]
async fn state_tracks_modes_pending_counts_and_new_session_reset() {
    let fixture = session_with_responses(Vec::new());
    let registration = fixture.registration();
    let application = fixture.application().await;
    application.set_steering_mode(QueueMode::All).await;
    application.set_follow_up_mode(QueueMode::All).await;
    application.set_auto_compaction_enabled(true);
    application
        .set_session_name("  State\nTest  ")
        .expect("set name");
    application
        .steer("steer".to_owned(), Vec::new())
        .await
        .expect("queue steer");
    application
        .follow_up("follow".to_owned(), Vec::new())
        .await
        .expect("queue follow-up");

    let queued = application.state().await;
    assert!(!queued.is_streaming);
    assert_eq!(queued.steering_mode, QueueMode::All);
    assert_eq!(queued.follow_up_mode, QueueMode::All);
    assert_eq!(queued.session_name.as_deref(), Some("State Test"));
    assert!(queued.auto_compaction_enabled);
    assert_eq!(queued.pending_message_count, 2);

    assert!(
        !application
            .new_session()
            .await
            .expect("new session")
            .cancelled
    );
    let reset = application.state().await;
    assert!(!reset.is_streaming);
    assert!(!reset.is_compacting);
    assert_eq!(reset.message_count, 0);
    assert_eq!(reset.pending_message_count, 0);
    assert_eq!(reset.session_name, None);
    assert_eq!(reset.steering_mode, QueueMode::All);
    assert_eq!(reset.follow_up_mode, QueueMode::All);
    assert!(reset.session_id.is_some());
    assert!(reset.session_file.is_some());
    registration.unregister();
}

/// `queue_send_now` must never destroy acknowledged non-User steering rows
/// (orchestration mailbox steers are `Message::Custom`): they re-queue while
/// the User rows combine and send. With NO user content the command fails —
/// and the whole batch must still be back in the queue.
#[tokio::test]
async fn send_now_requeues_non_user_steering_instead_of_destroying_it() {
    let fixture = session_with_responses(vec![FauxResponse::text("sent")]);
    let registration = fixture.registration();
    let application = fixture.application().await;
    application
        .steer("user row".to_owned(), Vec::new())
        .await
        .expect("queue user steer");
    application
        .session()
        .steer(pi_ai::Message::Custom(pi_ai::CustomMessage {
            custom_type: "pi.test.mailbox".to_owned(),
            content: "mailbox row".into(),
            display: false,
            details: None,
            timestamp: 0,
        }))
        .await
        .expect("queue custom steer");

    let outcome = application
        .send_queued_steering_now()
        .await
        .expect("send now delivers the user part");
    assert_eq!(outcome.sent, 1, "only the user row is delivered");
    let (steering, follow_up) = application.queued_messages().await;
    assert_eq!(steering.len(), 1, "the custom row must be re-queued");
    assert!(
        format!("{steering:?}").contains("mailbox row"),
        "the re-queued row must keep its content: {steering:?}"
    );
    assert!(follow_up.is_empty());
    application.wait_for_idle().await;
    registration.unregister();
}

#[tokio::test]
async fn send_now_without_user_content_restores_the_whole_batch() {
    let fixture = session_with_responses(vec![FauxResponse::text("unused")]);
    let registration = fixture.registration();
    let application = fixture.application().await;
    application
        .session()
        .steer(pi_ai::Message::Custom(pi_ai::CustomMessage {
            custom_type: "pi.test.mailbox".to_owned(),
            content: "mailbox only".into(),
            display: false,
            details: None,
            timestamp: 0,
        }))
        .await
        .expect("queue custom steer");

    let error = application
        .send_queued_steering_now()
        .await
        .expect_err("no user content must fail the send");
    assert!(
        error.to_string().contains("no user content"),
        "typed failure expected: {error:#}"
    );
    let (steering, follow_up) = application.queued_messages().await;
    assert_eq!(
        steering.len(),
        1,
        "the failed send must restore the acknowledged batch"
    );
    assert!(
        format!("{steering:?}").contains("mailbox only"),
        "the restored row must keep its content: {steering:?}"
    );
    assert!(follow_up.is_empty());
    registration.unregister();
}

#[tokio::test]
async fn navigation_and_fork_events_serialize_as_typed_application_events() {
    let fixture = session_with_responses(vec![FauxResponse::text("answer")]);
    let registration = fixture.registration();
    let application = fixture.application().await;
    application
        .prompt("question".to_owned(), Vec::new(), None)
        .await
        .expect("record turn");
    application.wait_for_idle().await;
    let user_id = application
        .session_entries(None)
        .expect("entries")
        .entries
        .into_iter()
        .find(|entry| matches!(entry.message, Some(pi_ai::Message::User(_))))
        .expect("user entry")
        .id;

    let mut events = application.subscribe();
    let result = application
        .navigate_tree(&user_id, pi_coding::NavigateTreeOptions::default())
        .await
        .expect("navigate");
    assert_eq!(result.editor_text.as_deref(), Some("question"));
    let before = events.recv().await.expect("before tree");
    let after = events.recv().await.expect("after tree");
    assert_eq!(
        serde_json::to_value(before).expect("serialize")["type"],
        "session_before_tree"
    );
    assert_eq!(
        serde_json::to_value(after).expect("serialize")["type"],
        "session_tree"
    );

    let previous = application
        .state()
        .await
        .session_file
        .expect("session file");
    let outcome = application
        .fork_session(&user_id)
        .await
        .expect("fork session");
    assert!(!outcome.cancelled);
    assert_eq!(outcome.text, "question");
    assert_ne!(
        application.state().await.session_file.as_deref(),
        Some(previous.as_str())
    );
    let fork_file = application
        .state()
        .await
        .session_file
        .expect("fork session file");
    let fork_tree = pi_coding::load_session_tree(&fork_file).expect("load persisted fork");
    assert_eq!(
        fork_tree.header.parent_session.as_deref(),
        Some(previous.as_str())
    );
    let before = events.recv().await.expect("before fork");
    let after = events.recv().await.expect("after fork");
    assert_eq!(
        serde_json::to_value(before).expect("serialize")["type"],
        "session_before_fork"
    );
    let serialized = serde_json::to_value(after).expect("serialize");
    assert_eq!(serialized["type"], "session_forked");
    assert_eq!(serialized["editorText"], "question");
    registration.unregister();
}

#[tokio::test]
async fn queue_snapshot_and_drain_preserve_steering_then_follow_up_order() {
    let fixture = session_with_responses(Vec::new());
    let registration = fixture.registration();
    let application = fixture.application().await;
    let mut events = application.subscribe();
    application
        .steer("steer one".to_owned(), Vec::new())
        .await
        .expect("queue first steer");
    application
        .steer("steer two".to_owned(), Vec::new())
        .await
        .expect("queue second steer");
    application
        .follow_up("follow one".to_owned(), Vec::new())
        .await
        .expect("queue follow-up");
    let mut queue_updates = Vec::new();
    while queue_updates.len() < 3 {
        match tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("queue event timeout")
            .expect("queue event")
        {
            ApplicationEvent::Session(pi_coding::SessionEvent::QueueUpdate {
                steering,
                follow_up,
            }) => {
                queue_updates.push((steering.len(), follow_up.len()));
            }
            _ => {}
        }
    }
    assert_eq!(queue_updates, [(1, 0), (2, 0), (2, 1)]);
    let (steering, follow_up) = application.queued_messages().await;
    assert_eq!(steering.len(), 2);
    assert_eq!(follow_up.len(), 1);
    let (steering, follow_up) = application.drain_queued_messages().await;
    let text = |message: &pi_ai::Message| match message {
        pi_ai::Message::User(message) => message
            .content
            .iter()
            .filter_map(|block| {
                if let ContentBlock::Text { text, .. } = block {
                    Some(text.as_str())
                } else {
                    None
                }
            })
            .collect::<String>(),
        other => panic!("unexpected queued message: {other:?}"),
    };
    assert_eq!(
        steering.iter().map(text).collect::<Vec<_>>(),
        ["steer one", "steer two"]
    );
    assert_eq!(
        follow_up.iter().map(text).collect::<Vec<_>>(),
        ["follow one"]
    );
    assert_eq!(application.state().await.pending_message_count, 0);
    loop {
        match tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("drain event timeout")
            .expect("drain event")
        {
            ApplicationEvent::Session(pi_coding::SessionEvent::QueueUpdate {
                steering,
                follow_up,
            }) => {
                assert!(steering.is_empty());
                assert!(follow_up.is_empty());
                break;
            }
            _ => {}
        }
    }
    registration.unregister();
}

#[tokio::test]
async fn foreground_bash_updates_and_end_forward_through_application_events() {
    let fixture = session_with_responses(Vec::new());
    let registration = fixture.registration();
    let application = fixture.application().await;
    let mut events = application.subscribe();
    // Portable multi-write pattern (see tools.rs bash stream tests): first
    // printf must be readable before exit so at least one BashExecutionUpdate
    // is guaranteed before BashExecutionEnd, independent of scheduler timing.
    // Chunks remain raw pipe reads — never assume a single delta equals the
    // full output (session_facade documents the same contract).
    let command = "printf 'a'; sleep 0.05; printf 'pp'";
    let result = application
        .execute_bash(command.to_owned(), false)
        .await
        .expect("application bash");
    assert_eq!(result.output, "app");
    let mut streamed = String::new();
    let mut saw_update = false;
    let mut saw_end = false;
    while !saw_end {
        let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("event timeout")
            .expect("event channel");
        match event {
            ApplicationEvent::Session(pi_coding::SessionEvent::BashExecutionUpdate {
                delta,
                ..
            }) if !delta.is_empty() => {
                saw_update = true;
                streamed.push_str(&delta);
            }
            ApplicationEvent::Session(pi_coding::SessionEvent::BashExecutionEnd { message }) => {
                assert_eq!(message.command, command);
                assert_eq!(message.output, "app");
                let serialized = serde_json::to_value(ApplicationEvent::Session(
                    pi_coding::SessionEvent::BashExecutionEnd {
                        message: message.clone(),
                    },
                ))
                .expect("serialize bash end");
                assert_eq!(serialized["type"], "bash_execution_end");
                assert_eq!(serialized["message"]["exitCode"], 0);
                assert!(serialized["message"].get("excludeFromContext").is_none());
                saw_end = true;
            }
            _ => {}
        }
    }
    assert!(saw_update);
    assert_eq!(streamed, "app");
    registration.unregister();
}

#[tokio::test]
async fn reload_reconfigures_parent_orchestration_tools_and_todo_atomically() {
    let fixture = HermeticFixture::new(
        "reload-tools",
        Vec::new(),
        None,
        |_, agent_dir| {
            std::fs::write(
                agent_dir.join("settings.json"),
                r#"{"sandbox":{"mode":"danger-full-access"},"orchestration":{"tasks":false,"process":false,"todo":false,"glob":false,"maxConcurrency":1}}"#,
            )
            .expect("settings");
        },
        |_| {},
        ToolSelection::default(),
    );
    let resources = fixture.resources();
    let registration = fixture.registration();
    let application = fixture.application().await;

    let tool_names = |application: &Application| {
        application
            .get_all_tools()
            .into_iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>()
    };
    for name in ["task", "hub", "process", "todo", "glob"] {
        assert!(
            !tool_names(&application).iter().any(|tool| tool == name),
            "unexpected {name}"
        );
        assert!(
            !application
                .get_active_tool_names()
                .iter()
                .any(|tool| tool == name),
            "active {name}"
        );
    }

    resources
        .settings_manager()
        .expect("settings manager")
        .update_global(|settings| {
            settings.orchestration = Some(OrchestrationSettings {
                tasks: Some(true),
                process: Some(true),
                todo: Some(true),
                glob: Some(true),
                ..OrchestrationSettings::default()
            });
        })
        .expect("enable settings");
    application.reload().await.expect("enable orchestration");

    let enabled = tool_names(&application);
    let active = application.get_active_tool_names();
    for name in ["task", "hub", "process", "todo", "glob"] {
        assert!(
            enabled.iter().any(|tool| tool == name),
            "missing {name} from all_tools"
        );
        assert!(
            active.iter().any(|tool| tool == name),
            "missing active {name}"
        );
    }
    assert!(application.orchestration_runtime().is_some());

    let todo = application
        .get_all_tools()
        .into_iter()
        .find(|tool| tool.name == "todo")
        .expect("parent todo tool");
    let (_, abort) = AbortController::new();
    let result = (todo.execute)(ToolCallContext {
        tool_call_id: "parent-todo-init".to_owned(),
        arguments: serde_json::json!({
            "op": "init",
            "list": [{ "phase": "Build", "items": ["wire parent todo"] }]
        }),
        on_update: Arc::new(|_| {}),
        abort,
        model: None,
        approval_token: None,
    })
    .await
    .expect("execute parent todo");
    assert_eq!(result.details["op"], "init");
    let state = application.todo_state();
    assert_eq!(state.phases.len(), 1);
    assert_eq!(state.phases[0].name, "Build");
    assert_eq!(state.phases[0].tasks[0].content, "wire parent todo");

    resources
        .settings_manager()
        .expect("settings manager")
        .update_global(|settings| {
            settings.orchestration = Some(OrchestrationSettings {
                tasks: Some(false),
                process: Some(false),
                todo: Some(false),
                glob: Some(false),
                ..OrchestrationSettings::default()
            });
        })
        .expect("disable settings");
    application.reload().await.expect("disable orchestration");
    let disabled = tool_names(&application);
    let active = application.get_active_tool_names();
    for name in ["task", "hub", "process", "todo", "glob"] {
        assert!(
            !disabled.iter().any(|tool| tool == name),
            "stale {name} in all_tools"
        );
        assert!(
            !active.iter().any(|tool| tool == name),
            "stale active {name}"
        );
    }
    assert!(application.orchestration_runtime().is_none());
    assert!(
        application
            .set_active_tools_by_name(&["todo".to_owned()])
            .await
            .is_err()
    );

    application.cleanup().await;
    registration.unregister();
}

#[tokio::test]
async fn reload_applies_agent_enablement_and_model_live() {
    let fixture = HermeticFixture::new(
        "reload-agents",
        Vec::new(),
        None,
        |_, agent_dir| {
            std::fs::create_dir_all(agent_dir.join("agents")).expect("agents dir");
            std::fs::write(
                agent_dir.join("settings.json"),
                r#"{"sandbox":{"mode":"danger-full-access"},"orchestration":{"tasks":true,"process":true,"todo":true,"maxConcurrency":1}}"#,
            )
            .expect("settings");
            std::fs::write(
                agent_dir.join("agents").join("reviewer.md"),
                "---\nname: reviewer\ndescription: review code\n---\nYou review code.\n",
            )
            .expect("reviewer agent");
        },
        |_| {},
        ToolSelection::default(),
    );
    let resources = fixture.resources();
    let registration = fixture.registration();
    let application = fixture.application().await;
    application.reload().await.expect("enable orchestration");

    let runtime = application
        .orchestration_runtime()
        .expect("orchestration runtime");
    let task_tool = runtime
        .agent_tools("Main", 0)
        .into_iter()
        .find(|tool| tool.name == "task")
        .expect("task tool");
    assert!(
        task_tool.description.contains("reviewer —"),
        "enabled reviewer must be advertised: {}",
        task_tool.description
    );
    assert_eq!(
        runtime.select_agent("reviewer", None),
        "reviewer",
        "default selector settings must route matching user text without explicit configuration",
    );

    resources
        .settings_manager()
        .expect("settings manager")
        .update_global(|settings| {
            settings.agents.insert(
                "reviewer".to_owned(),
                AgentRuntimeSettings {
                    enabled: Some(false),
                    model: Some("openai/gpt-4.1".to_owned()),
                    tools: None,
                },
            );
        })
        .expect("disable reviewer");
    application
        .reload()
        .await
        .expect("reload after agent settings save");

    let runtime = application
        .orchestration_runtime()
        .expect("orchestration runtime after reload");
    let task_tool = runtime
        .agent_tools("Main", 0)
        .into_iter()
        .find(|tool| tool.name == "task")
        .expect("task tool");
    assert!(
        !task_tool.description.contains("reviewer —"),
        "disabled reviewer must leave task catalog: {}",
        task_tool.description
    );
    assert!(
        runtime.ensure_agent_enabled("reviewer").is_err(),
        "disabled agent must fail ensure_agent_enabled"
    );

    // Selector catalog must also omit disabled agents.
    let plan = application
        .session()
        .select_for_request("review code")
        .await;
    assert!(
        plan.agents.iter().all(|hit| hit.name != "reviewer"),
        "disabled agent must not be selectable: {plan:?}"
    );

    application.cleanup().await;
    registration.unregister();
}

struct ControlledReloadFixture {
    fixture: HermeticFixture,
    started_count: Arc<AtomicUsize>,
    started: Arc<Notify>,
    release: CancellationToken,
}

fn controlled_reload_session(cwd: &Path) -> ControlledReloadFixture {
    let started_count = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let release = CancellationToken::new();
    let stream_started_count = started_count.clone();
    let stream_started = started.clone();
    let stream_release = release.clone();
    let stream_fn: pi_agent::StreamFn = Arc::new(move |model, _context, options| {
        let started_count = stream_started_count.clone();
        let started = stream_started.clone();
        let release = stream_release.clone();
        Box::pin(async move {
            let stream = pi_ai::new_assistant_message_event_stream();
            let producer = stream.clone();
            tokio::spawn(async move {
                started_count.fetch_add(1, Ordering::SeqCst);
                started.notify_waiters();
                let aborted = match options.stream.abort_signal {
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
                if aborted {
                    message.stop_reason = StopReason::Aborted;
                } else {
                    message
                        .content
                        .push(ContentBlock::text("reload retained result"));
                    message.stop_reason = StopReason::Stop;
                }
                producer.end(Some(message)).await;
            });
            stream
        })
    });
    let fixture =
        HermeticFixture::for_workspace("reload-retention", cwd, Vec::new(), Some(stream_fn));
    ControlledReloadFixture {
        fixture,
        started_count,
        started,
        release,
    }
}

async fn wait_for_reload_child(
    runtime: &OrchestrationRuntime,
    job_id: &str,
    count: &AtomicUsize,
    notify: &Notify,
) {
    let job_ids = [job_id.to_owned()];
    let wait = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let notified = notify.notified();
            if count.load(Ordering::SeqCst) > 0 {
                return;
            }
            let snapshot = runtime
                .jobs(Some(&job_ids))
                .into_iter()
                .next()
                .expect("spawned reload child job");
            assert!(
                !snapshot.status.is_settled(),
                "reload child settled before entering the provider: {snapshot:#?}",
            );
            tokio::select! {
                () = notified => {}
                () = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
        }
    })
    .await;
    if wait.is_err() {
        panic!(
            "reload child did not enter the provider: {:#?}",
            runtime.jobs(Some(&job_ids)),
        );
    }
}

fn orchestration_resources(root: &Path, agent_dir: PathBuf) -> ResourceManager {
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::write(
        agent_dir.join("settings.json"),
        r#"{"sandbox":{"mode":"danger-full-access"},"orchestration":{"tasks":true,"maxConcurrency":1,"maxToolsPerAgent":64}}"#,
    )
    .expect("settings");
    let mut options = ResourceManagerOptions::new(root);
    options.agent_dir = agent_dir;
    options.project_trust_override = Some(true);
    options.disable_extensions = true;
    options.disable_skills = true;
    options.disable_prompt_templates = true;
    options.disable_themes = true;
    options.disable_context_files = true;
    ResourceManager::new(options).expect("resources")
}

#[tokio::test]
async fn context_only_reload_updates_future_children_without_replacing_runtime() {
    let contexts = Arc::new(parking_lot::Mutex::new(Vec::<pi_ai::Context>::new()));
    let started_count = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let release_first = CancellationToken::new();
    let observed_contexts = contexts.clone();
    let observed_count = started_count.clone();
    let observed_started = started.clone();
    let observed_release = release_first.clone();
    let stream_fn: pi_agent::StreamFn = Arc::new(move |model, context, _options| {
        observed_contexts.lock().push(context.clone());
        let count = observed_count.fetch_add(1, Ordering::SeqCst) + 1;
        observed_started.notify_waiters();
        let release = observed_release.clone();
        Box::pin(async move {
            if count == 1 {
                release.cancelled().await;
            }
            let stream = pi_ai::new_assistant_message_event_stream();
            let producer = stream.clone();
            tokio::spawn(async move {
                let mut message = pi_ai::AssistantMessage::pending(&model);
                message.content.push(ContentBlock::text("done"));
                message.stop_reason = StopReason::Stop;
                producer.end(Some(message)).await;
            });
            stream
        })
    });
    let fixture = HermeticFixture::new(
        "context-reload",
        Vec::new(),
        Some(stream_fn),
        |workspace, agent_dir| {
            std::fs::write(
                agent_dir.join("settings.json"),
                r#"{"sandbox":{"mode":"danger-full-access"},"orchestration":{"tasks":true,"maxToolsPerAgent":64}}"#,
            )
            .expect("settings");
            std::fs::write(workspace.join("AGENTS.md"), "first rules").expect("first context");
        },
        |options| {
            options.disable_context_files = false;
        },
        ToolSelection::default(),
    );
    let session = fixture.session();
    let registration = fixture.registration();
    fixture.attach_resources().await;

    let startup_snapshot = session.resource_manager().expect("resources").snapshot();
    let startup_settings = startup_snapshot
        .settings
        .runtime_settings()
        .expect("runtime settings");
    let mut config = OrchestrationConfig::new(
        AgentCatalog::from_agents(startup_snapshot.agents.clone()),
        startup_snapshot.cwd.join(".rpi/artifacts"),
    );
    config.skills = startup_snapshot
        .skills
        .iter()
        .map(pi_coding::OrchestrationSkill::from)
        .collect();
    config.max_concurrency = startup_settings.orchestration_max_concurrency;
    config.max_recursion_depth = startup_settings.orchestration_max_recursion_depth;
    config.mailbox_capacity = startup_settings.orchestration_mailbox_capacity;
    config.max_tools_per_agent = startup_settings.orchestration_max_tools_per_agent;
    config.soft_budget = startup_settings.orchestration_soft_budget;
    config.preferred_agent = startup_snapshot
        .settings
        .orchestration
        .as_ref()
        .and_then(|orchestration| orchestration.preferred_agent.clone());
    config = config.with_selector_settings(
        startup_snapshot
            .settings
            .selector
            .clone()
            .unwrap_or_default(),
    );
    config.agent_settings = startup_snapshot.settings.agents.clone();
    config.parent_model = session.model().expect("context reload model");
    let session_for_parent = session.clone();
    config = config.with_parent_model_provider(Arc::new(move || {
        session_for_parent.model().unwrap_or_default()
    }));
    let startup_factory =
        OrchestrationRuntime::child_factory_from_snapshot(session.child_session_options_snapshot());
    let startup = OrchestrationRuntime::new(config, startup_factory).expect("startup runtime");
    let application =
        publish_application(ApplicationRuntimeCandidate::new(session).with_orchestration(startup))
            .await;
    let initial_group = application
        .orchestration_runtime()
        .expect("initial runtime")
        .group_id()
        .to_owned();

    let first_runtime = application
        .orchestration_runtime()
        .expect("initial runtime");
    let first_spawn = first_runtime
        .spawn_tasks(
            "Main",
            0,
            vec![TaskItem {
                index: 0,
                id: "BeforeReload".to_owned(),
                agent: "task".to_owned(),
                assignment: "report".to_owned(),
                todo_task_id: None,
                ..Default::default()
            }],
        )
        .expect("first child spawn")
        .remove(0);
    wait_for_reload_child(
        &first_runtime,
        &first_spawn.job_id,
        &started_count,
        &started,
    )
    .await;

    std::fs::write(fixture.workspace().join("AGENTS.md"), "second rules").expect("second context");
    application.reload().await.expect("context reload");
    assert_eq!(
        application
            .durable_policy_state()
            .expect("reloaded durable policy")
            .snapshot()
            .revision()
            .get(),
        2,
        "post-reload children must execute under the published revision",
    );
    let replacement = application
        .orchestration_runtime()
        .expect("retained runtime");
    assert_eq!(replacement.group_id(), initial_group);
    let (_controller, abort) = AbortController::new();
    replacement
        .run_tasks(
            "Main",
            0,
            vec![TaskItem {
                index: 0,
                id: "AfterReload".to_owned(),
                agent: "task".to_owned(),
                assignment: "report".to_owned(),
                todo_task_id: None,
                ..Default::default()
            }],
            abort,
        )
        .await
        .expect("second child run");
    assert_eq!(
        replacement.active_child_count(),
        1,
        "first child must remain active"
    );
    release_first.cancel();
    replacement
        .wait_jobs(
            std::slice::from_ref(&first_spawn.job_id),
            Some(Duration::from_secs(2)),
            None,
        )
        .await
        .expect("first child settles");

    let contexts = contexts.lock();
    assert_eq!(contexts.len(), 2, "{contexts:?}");
    assert!(
        contexts[0].system_prompt.contains("first rules"),
        "{:?}",
        contexts[0]
    );
    assert!(
        !contexts[0].system_prompt.contains("second rules"),
        "{:?}",
        contexts[0]
    );
    assert!(
        contexts[1].system_prompt.contains("second rules"),
        "{:?}",
        contexts[1]
    );
    assert!(
        !contexts[1].system_prompt.contains("first rules"),
        "{:?}",
        contexts[1]
    );
    drop(contexts);

    application.cleanup().await;
    registration.unregister();
}

#[tokio::test]
async fn attached_orchestration_rejects_foreign_child_resource_authority() {
    let owner_root = tempfile::tempdir().expect("owner root");
    let foreign_root = tempfile::tempdir().expect("foreign root");
    let owner_controlled = controlled_reload_session(owner_root.path());
    let owner_session = owner_controlled.fixture.session();
    let owner_registration = owner_controlled.fixture.registration();
    let owner_resources = orchestration_resources(
        owner_root.path(),
        owner_controlled.fixture.agent_dir().to_path_buf(),
    );
    owner_session
        .attach_resources(owner_resources.clone())
        .await
        .expect("attach owner resources");

    let foreign_controlled = controlled_reload_session(foreign_root.path());
    let foreign_session = foreign_controlled.fixture.session();
    let foreign_registration = foreign_controlled.fixture.registration();
    let foreign_release = foreign_controlled.release.clone();
    let foreign_resources = orchestration_resources(
        foreign_root.path(),
        foreign_controlled.fixture.agent_dir().to_path_buf(),
    );
    foreign_session
        .attach_resources(foreign_resources)
        .await
        .expect("attach foreign resources");
    let foreign_application =
        publish_application(ApplicationRuntimeCandidate::new(foreign_session.clone())).await;

    let snapshot = owner_resources.snapshot();
    let settings = snapshot
        .settings
        .runtime_settings()
        .expect("runtime settings");
    let mut config = OrchestrationConfig::new(
        AgentCatalog::from_agents(snapshot.agents.clone()),
        snapshot.cwd.join(".rpi/artifacts"),
    );
    config.max_concurrency = settings.orchestration_max_concurrency;
    config.max_tools_per_agent = settings.orchestration_max_tools_per_agent;
    config.parent_model = owner_session.model().expect("owner model");
    config.idle_ttl = None;
    let factory: pi_coding::ChildSessionFactory = Arc::new(move |_| {
        let foreign_session = foreign_session.clone();
        Box::pin(async move { Ok(foreign_session) })
    });
    let runtime = OrchestrationRuntime::new(config, factory).expect("runtime");
    let application = publish_application(
        ApplicationRuntimeCandidate::new(owner_session).with_orchestration(runtime.clone()),
    )
    .await;
    let (_, abort) = AbortController::new();
    let results = runtime
        .run_tasks(
            "Main",
            0,
            vec![TaskItem {
                index: 0,
                id: "ForeignChild".to_owned(),
                agent: "task".to_owned(),
                assignment: "must fail closed".to_owned(),
                todo_task_id: None,
                ..Default::default()
            }],
            abort,
        )
        .await
        .expect("run reports child failure");
    assert_eq!(results.len(), 1, "{results:?}");
    assert!(
        results[0]
            .error
            .as_deref()
            .is_some_and(|error| error.contains("foreign resource authority")),
        "foreign application authority must never be overwritten: {results:?}",
    );

    foreign_release.cancel();
    application.cleanup().await;
    foreign_application.cleanup().await;
    owner_registration.unregister();
    foreign_registration.unregister();
}

#[tokio::test]
async fn equivalent_reload_retains_running_and_retained_jobs() {
    let root = tempfile::tempdir().expect("root");
    let controlled = controlled_reload_session(root.path());
    let session = controlled.fixture.session();
    let registration = controlled.fixture.registration();
    let started_count = controlled.started_count.clone();
    let started = controlled.started.clone();
    let release = controlled.release.clone();
    let resources =
        orchestration_resources(root.path(), controlled.fixture.agent_dir().to_path_buf());
    session
        .attach_resources(resources)
        .await
        .expect("attach resources");
    let application = publish_application(ApplicationRuntimeCandidate::new(session)).await;
    application.reload().await.expect("enable orchestration");
    let before = application.orchestration_runtime().expect("runtime");
    let group_id = before.group_id().to_owned();
    let spawn = before
        .spawn_tasks(
            "Main",
            0,
            vec![TaskItem {
                index: 0,
                id: "ReloadSurvivor".to_owned(),
                agent: "task".to_owned(),
                assignment: "remain active across reload".to_owned(),
                todo_task_id: None,
                ..Default::default()
            }],
        )
        .expect("spawn")
        .remove(0);
    wait_for_reload_child(&before, &spawn.job_id, &started_count, &started).await;

    application.reload().await.expect("equivalent reload");
    let after_running = application
        .orchestration_runtime()
        .expect("retained runtime");
    assert_eq!(after_running.group_id(), group_id);
    assert_eq!(after_running.active_child_count(), 1);
    assert_eq!(
        after_running.jobs(Some(std::slice::from_ref(&spawn.job_id)))[0].status,
        JobStatus::Running,
    );

    release.cancel();
    let settled = after_running
        .wait_jobs(
            std::slice::from_ref(&spawn.job_id),
            Some(Duration::from_secs(2)),
            None,
        )
        .await
        .expect("settled job");
    assert_eq!(settled[0].status, JobStatus::Completed);
    application
        .reload()
        .await
        .expect("equivalent retained reload");
    let after_retained = application
        .orchestration_runtime()
        .expect("runtime after retained reload");
    assert_eq!(after_retained.group_id(), group_id);
    assert_eq!(
        after_retained.jobs(Some(std::slice::from_ref(&spawn.job_id)))[0].status,
        JobStatus::Completed,
    );

    application.cleanup().await;
    registration.unregister();
}

#[tokio::test]
async fn non_equivalent_reload_retains_running_job_until_it_settles() {
    let root = tempfile::tempdir().expect("root");
    let controlled = controlled_reload_session(root.path());
    let session = controlled.fixture.session();
    let registration = controlled.fixture.registration();
    let started_count = controlled.started_count.clone();
    let started = controlled.started.clone();
    let resources =
        orchestration_resources(root.path(), controlled.fixture.agent_dir().to_path_buf());
    session
        .attach_resources(resources.clone())
        .await
        .expect("attach resources");
    let application = publish_application(ApplicationRuntimeCandidate::new(session)).await;
    application.reload().await.expect("enable orchestration");
    let previous = application.orchestration_runtime().expect("runtime");
    let previous_group = previous.group_id().to_owned();
    let spawn = previous
        .spawn_tasks(
            "Main",
            0,
            vec![TaskItem {
                index: 0,
                id: "ReloadCleanup".to_owned(),
                agent: "task".to_owned(),
                assignment: "remain active across non-equivalent reload".to_owned(),
                todo_task_id: None,
                ..Default::default()
            }],
        )
        .expect("spawn")
        .remove(0);
    wait_for_reload_child(&previous, &spawn.job_id, &started_count, &started).await;
    resources
        .settings_manager()
        .expect("settings manager")
        .update_global(|settings| {
            settings.orchestration = Some(OrchestrationSettings {
                tasks: Some(true),
                max_concurrency: Some(2),
                ..OrchestrationSettings::default()
            });
        })
        .expect("change orchestration settings");

    application.reload().await.expect("non-equivalent reload");
    let retained = application
        .orchestration_runtime()
        .expect("retained runtime");
    assert_eq!(retained.group_id(), previous_group);
    assert_eq!(retained.active_child_count(), 1);
    assert_eq!(
        retained.jobs(Some(std::slice::from_ref(&spawn.job_id)))[0].status,
        JobStatus::Running,
    );
    assert!(
        retained
            .list(&spawn.agent_id)
            .iter()
            .any(|agent| agent.id == "Main")
    );

    controlled.release.cancel();
    let settled = retained
        .wait_jobs(
            std::slice::from_ref(&spawn.job_id),
            Some(Duration::from_secs(2)),
            None,
        )
        .await
        .expect("settled retained job");
    assert_eq!(settled[0].status, JobStatus::Completed);
    assert!(!retained.is_shutdown());

    application
        .reload()
        .await
        .expect("apply deferred non-equivalent reload");
    let replacement = application
        .orchestration_runtime()
        .expect("replacement runtime");
    assert_ne!(replacement.group_id(), previous_group);
    assert!(replacement.jobs(None).is_empty());

    application.cleanup().await;
    registration.unregister();
}

/// Contract: a non-equivalent reload must reset the durable sidecar, not only
/// the live in-memory job state. A live job defers the non-equivalent reload:
/// while the job is still Running, `commit_orchestration_candidate` keeps the
/// current runtime and the reset does not run. Only after the live job settles
/// does the deferred non-equivalent reload bind a fresh clean-slate
/// replacement, and `bind_and_reset` empties the shared sidecar. This test
/// simulates a restart immediately after that reset by binding a fresh
/// `OrchestrationRuntime` to the same parent session, then asserting the
/// durable sidecar yields no jobs.
#[tokio::test]
async fn non_equivalent_reload_resets_durable_sidecar_so_restart_recovers_no_jobs() {
    let root = tempfile::tempdir().expect("root");
    let controlled = controlled_reload_session(root.path());
    let session = controlled.fixture.session();
    let registration = controlled.fixture.registration();
    let started_count = controlled.started_count.clone();
    let started = controlled.started.clone();
    let resources =
        orchestration_resources(root.path(), controlled.fixture.agent_dir().to_path_buf());
    session
        .attach_resources(resources.clone())
        .await
        .expect("attach resources");
    let application = publish_application(ApplicationRuntimeCandidate::new(session)).await;
    application.reload().await.expect("enable orchestration");
    let prior = application.orchestration_runtime().expect("runtime");
    let spawn = prior
        .spawn_tasks(
            "Main",
            0,
            vec![TaskItem {
                index: 0,
                id: "ResetSurvivor".to_owned(),
                agent: "task".to_owned(),
                assignment: "must not survive non-equivalent reload into a restart".to_owned(),
                todo_task_id: None,
                ..Default::default()
            }],
        )
        .expect("spawn")
        .remove(0);
    wait_for_reload_child(&prior, &spawn.job_id, &started_count, &started).await;

    resources
        .settings_manager()
        .expect("settings manager")
        .update_global(|settings| {
            settings.orchestration = Some(OrchestrationSettings {
                tasks: Some(true),
                max_concurrency: Some(2),
                ..OrchestrationSettings::default()
            });
        })
        .expect("non-equivalent settings");

    // The live job is still Running, so the non-equivalent reload is deferred:
    // the reset must wait for it to settle first (application.rs:4713 treats
    // the still-active runtime as having active jobs). Settle it, then apply
    // the deferred reload so `bind_and_reset` actually runs.
    controlled.release.cancel();
    let settled = prior
        .wait_jobs(
            std::slice::from_ref(&spawn.job_id),
            Some(Duration::from_secs(2)),
            None,
        )
        .await
        .expect("settled pre-reload job");
    assert_eq!(settled[0].status, JobStatus::Completed);

    application
        .reload()
        .await
        .expect("deferred non-equivalent reload");
    let replacement = application
        .orchestration_runtime()
        .expect("replacement runtime");
    assert!(replacement.jobs(None).is_empty());

    // Simulate a restart immediately after the deferred non-equivalent reload,
    // while the replacement is still the live durable owner: bind a fresh
    // `OrchestrationRuntime` to the same parent session (and therefore the same
    // durable sidecar) and recover. Doing this before `application.cleanup()`
    // is essential: cleanup's own shutdown persist would re-write an empty
    // sidecar and mask a stale-sidecar regression. The fresh recovery must read
    // exactly what the replacement's `bind_and_reset` left on disk.
    let session = application.session();
    let snapshot = session.resource_manager().expect("resources").snapshot();
    let mut config = pi_coding::OrchestrationConfig::new(
        pi_coding::AgentCatalog::from_agents(snapshot.agents.clone()),
        snapshot.cwd.join(".pi").join("artifacts"),
    );
    if let Some(model) = session.model() {
        config.parent_model = model;
    }
    let factory = pi_coding::OrchestrationRuntime::child_factory_from_session(&session);
    let fresh = pi_coding::OrchestrationRuntime::new(config, factory).expect("fresh runtime");
    fresh
        .bind_and_recover(&session)
        .expect("recover durable sidecar after restart");
    assert!(
        fresh.jobs(None).is_empty(),
        "non-equivalent reload must not leave pre-reload jobs recoverable from the durable sidecar"
    );
    assert_eq!(
        fresh.active_child_count(),
        0,
        "no recovered active children"
    );
    assert!(
        fresh.list("Main").is_empty(),
        "no recovered agents from the pre-reload runtime"
    );
    assert!(
        fresh.resolve_read_uri("artifact://ResetSurvivor").is_err(),
        "no recovered artifacts from the pre-reload runtime"
    );

    fresh.shutdown().await;
    application.cleanup().await;
    drop(spawn);
    registration.unregister();
}

#[tokio::test]
async fn extension_session_reducers_cancel_without_mutation_and_transform_tree()
-> anyhow::Result<()> {
    let fixture = session_with_responses(vec![FauxResponse::text("answer")]);
    let registration = fixture.registration();
    let (application, runtime, _temporary) =
        semantic_application_with_source(&fixture, None, Some("question")).await?;
    let user_id = application
        .session_entries(None)?
        .entries
        .into_iter()
        .find(|entry| matches!(entry.message, Some(pi_ai::Message::User(_))))
        .expect("recorded transformed user entry")
        .id;
    let before = application.state().await;
    let history = application.messages();

    let resume_directory = tempfile::tempdir()?;
    let resume_file = resume_directory.path().join("resume.jsonl");
    write_session(
        &resume_file,
        resume_directory.path(),
        "resume",
        "resume message",
    );
    assert!(application.new_session().await?.cancelled);
    assert!(application.switch_session(&resume_file).await?.cancelled);
    let fork = application.fork_session(&user_id).await?;
    assert!(fork.cancelled);
    assert!(fork.text.is_empty());
    assert!(application.clone_session().await?.cancelled);
    let unchanged = application.state().await;
    assert_eq!(unchanged.session_id, before.session_id);
    assert_eq!(unchanged.session_file, before.session_file);
    assert_eq!(application.messages(), history);

    let navigation = application
        .navigate_tree(&user_id, pi_coding::NavigateTreeOptions::default())
        .await?;
    assert!(navigation.changed);
    let summary_id = navigation
        .summary_entry_id
        .expect("extension summary entry");
    let entries = application.session_entries(None)?.entries;
    assert!(
        entries
            .iter()
            .any(|entry| entry.id == summary_id
                && entry.summary.as_deref() == Some("fixture summary"))
    );
    assert!(entries.iter().any(
        |entry| entry.target_id.as_deref() == Some(summary_id.as_str())
            && entry.label.as_deref() == Some("fixture-label")
    ));

    application.cleanup().await;
    runtime.shutdown().await;
    registration.unregister();
    Ok(())
}

#[tokio::test]
async fn extension_fork_skip_conversation_restore_creates_empty_live_context() -> anyhow::Result<()>
{
    let fixture = session_with_responses(vec![FauxResponse::text("answer")]);
    let registration = fixture.registration();
    let source = r#"export default function (pi) {
        pi.on("session_before_fork", () => ({ skipConversationRestore: true }));
    }"#;
    let (application, runtime, _temporary) =
        semantic_application_with_source(&fixture, Some(source), Some("question")).await?;
    let user_id = application
        .session_entries(None)?
        .entries
        .into_iter()
        .find(|entry| matches!(entry.message, Some(pi_ai::Message::User(_))))
        .expect("recorded user entry")
        .id;

    let fork = application.fork_session(&user_id).await?;
    assert!(!fork.cancelled);
    assert_eq!(fork.text, "question");
    assert!(application.messages().is_empty());
    assert!(
        application
            .session_entries(None)?
            .entries
            .iter()
            .all(|entry| entry.message.is_none())
    );

    application.cleanup().await;
    runtime.shutdown().await;
    registration.unregister();
    Ok(())
}

#[tokio::test]
async fn extension_clone_skip_conversation_restore_creates_empty_live_context() -> anyhow::Result<()>
{
    let fixture = session_with_responses(vec![FauxResponse::text("answer")]);
    let registration = fixture.registration();
    let source = r#"export default function (pi) {
        pi.on("session_before_fork", (event) => {
            if (event.position !== "at") throw new Error(`unexpected position: ${event.position}`);
            return { skipConversationRestore: true };
        });
    }"#;
    let (application, runtime, _temporary) =
        semantic_application_with_source(&fixture, Some(source), Some("question")).await?;

    let clone = application.clone_session().await?;
    assert!(!clone.cancelled);
    assert!(application.messages().is_empty());

    application.cleanup().await;
    runtime.shutdown().await;
    registration.unregister();
    Ok(())
}

#[tokio::test]
async fn extension_context_snapshot_strips_model_headers() -> anyhow::Result<()> {
    let secret = "Bearer extension-context-header-secret";
    let fixture = session_with_responses(Vec::new());
    let session = fixture.session();
    let registration = fixture.registration();
    let mut model = session.model().expect("session model");
    model.headers = Some(std::collections::HashMap::from([
        ("Authorization".to_owned(), secret.to_owned()),
        (
            "X-Probe-Secret".to_owned(),
            "probe-extension-secret".to_owned(),
        ),
    ]));
    let expected_id = model.id.clone();
    let expected_provider = model.provider.clone();
    let expected_name = model.name.clone();
    session.set_model(model, String::new());

    let source = r#"
export default function (pi) {
  pi.registerCommand("probe-model", {
    handler: (_args, ctx) => ctx.model ?? null,
  });
}
"#;
    let directory = tempfile::tempdir()?;
    let entry = directory.path().join("extension.mjs");
    std::fs::write(&entry, source)?;
    let manifest = write_quickjs_manifest(
        directory.path(),
        "context-model-probe",
        &entry,
        &["commands"],
    )?;
    let permissions = ExtensionPermissionSet {
        capabilities: BTreeSet::from([ExtensionCapability::Commands]),
        ui_capabilities: BTreeSet::new(),
    };
    let runtime = ExtensionRuntime::process(
        None,
        ExtensionRuntimeOptions {
            mode: ExtensionMode::Tui,
            invocation_timeout: Duration::from_secs(10),
            ..ExtensionRuntimeOptions::default()
        },
    );
    let mut options = ResourceManagerOptions::new(fixture.workspace());
    options.agent_dir = fixture.agent_dir().to_path_buf();
    options.project_trust_override = Some(true);
    options.explicit_extension_paths = vec![manifest];
    options.disable_extensions = true;
    options.disable_skills = true;
    options.disable_prompt_templates = true;
    options.disable_themes = true;
    options.disable_context_files = true;
    session
        .attach_resources(ResourceManager::new(options)?)
        .await?;
    let application = publish_application(
        ApplicationRuntimeCandidate::new(session).with_extensions(runtime.clone(), permissions),
    )
    .await;
    let model_value = runtime
        .invoke_command("probe-model", String::new(), None, None)
        .await?;
    let encoded = serde_json::to_string(&model_value)?;
    assert!(
        !encoded.contains(secret) && !encoded.contains("probe-extension-secret"),
        "extension context model leaked credential headers: {encoded}"
    );
    assert_eq!(model_value["id"], expected_id);
    assert_eq!(model_value["provider"], expected_provider);
    assert_eq!(model_value["name"], expected_name);
    assert!(
        model_value.get("headers").is_none()
            || model_value.get("headers") == Some(&serde_json::Value::Null),
        "headers must be absent/null after host sanitization: {encoded}"
    );

    application.cleanup().await;
    runtime.shutdown().await;
    registration.unregister();
    Ok(())
}

#[tokio::test]
async fn loop_turns_project_events_use_session_path_and_suspend_on_switch() {
    let model_contexts = Arc::new(parking_lot::Mutex::new(Vec::<pi_ai::Context>::new()));
    let fixture = session_with_recorded_contexts(
        vec![FauxResponse::text("loop answer")],
        model_contexts.clone(),
    );
    let session = fixture.session();
    let registration = fixture.registration();
    let first_recorder = pi_coding::start_session_in(
        fixture.workspace(),
        session.model().as_ref(),
        Some("off"),
        Some(fixture.session_dir()),
        Some("loop-first"),
        None,
    )
    .expect("first recorder");
    first_recorder.persist_now().expect("persist first session");
    let first_path = first_recorder.path();
    first_recorder.close().expect("close first recorder");
    session
        .switch_session(&first_path)
        .await
        .expect("attach first recorder");
    let second_recorder = pi_coding::start_session_in(
        fixture.workspace(),
        session.model().as_ref(),
        Some("off"),
        Some(fixture.session_dir()),
        Some("loop-second"),
        None,
    )
    .expect("second recorder");
    let second_path = second_recorder.path();
    second_recorder
        .persist_now()
        .expect("persist second session");
    second_recorder.close().expect("close second session");

    let application = fixture.application().await;
    let mut events = application.subscribe();
    let task = application
        .loop_create(pi_coding::LoopCreateRequest::immediate(
            "3s",
            "run through session",
        ))
        .await
        .expect("create loop");

    let mut saw_fired = false;
    let mut saw_finished = false;
    while !saw_finished {
        let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("loop event timeout")
            .expect("application event channel");
        match event {
            ApplicationEvent::Loop(pi_coding::LoopEvent::Fired { task_id, .. })
                if task_id == task.id =>
            {
                saw_fired = true
            }
            ApplicationEvent::Loop(pi_coding::LoopEvent::Finished { task_id, .. })
                if task_id == task.id =>
            {
                saw_finished = true
            }
            _ => {}
        }
    }
    assert!(saw_fired);
    application.wait_for_idle().await;
    let messages = application.messages();
    let scheduled = messages
        .iter()
        .find_map(|message| match message {
            pi_ai::Message::Custom(custom) if custom.custom_type == "loop_scheduled_turn" => {
                Some(custom)
            }
            _ => None,
        })
        .expect("loop turn must append a typed scheduled message");
    assert!(!scheduled.display);
    assert_eq!(
        scheduled
            .details
            .as_ref()
            .and_then(|details| details["taskId"].as_str()),
        Some(task.id.as_str())
    );
    assert_eq!(
        scheduled
            .details
            .as_ref()
            .and_then(|details| details["prompt"].as_str()),
        Some("run through session")
    );
    assert_eq!(
        scheduled
            .details
            .as_ref()
            .and_then(|details| details["schedule"].as_str()),
        Some("every 3 seconds")
    );
    assert!(
        !messages
            .iter()
            .any(|message| matches!(message, pi_ai::Message::User(_))),
        "scheduled wrapper must not be a public user message"
    );
    let context = model_contexts.lock();
    let model_prompt = context
        .first()
        .and_then(|context| {
            context.messages.iter().find_map(|message| match message {
                pi_ai::Message::User(user) => Some(
                    user.content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<String>(),
                ),
                _ => None,
            })
        })
        .expect("model receives loop wrapper as user content");
    assert!(model_prompt.contains("scheduled task execution"));
    assert!(model_prompt.contains("task "));
    assert!(model_prompt.contains("every 3 seconds"));
    assert!(model_prompt.ends_with("run through session"));
    let assistant = messages
        .iter()
        .find_map(|message| match message {
            pi_ai::Message::Assistant(assistant) => Some(assistant),
            _ => None,
        })
        .expect("loop turn must append an assistant message");
    assert_eq!(assistant.text(), "loop answer");
    drop(context);

    let durable = application
        .loop_create(pi_coding::LoopCreateRequest {
            interval: "1h".to_owned(),
            prompt: "restore only in first session".to_owned(),
            fire_immediately: false,
            durable: true,
        })
        .await
        .expect("create durable loop");
    let switched = application
        .switch_session(&second_path)
        .await
        .expect("switch session");
    assert!(!switched.cancelled);
    assert!(
        application
            .loop_list()
            .await
            .expect("second session loops")
            .is_empty()
    );
    let mut removed_for_switch = false;
    while let Ok(event) = events.try_recv() {
        if matches!(
            event,
            ApplicationEvent::Loop(pi_coding::LoopEvent::Removed {
                task_id,
                reason: pi_coding::LoopRemovalReason::SessionChanged,
            }) if task_id == durable.id
        ) {
            removed_for_switch = true;
        }
    }
    assert!(
        removed_for_switch,
        "session switch must project loop suspension"
    );

    let restored_switch = application
        .switch_session(&first_path)
        .await
        .expect("restore first session");
    assert!(!restored_switch.cancelled);
    let restored = application.loop_list().await.expect("restored loops");
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].id, durable.id);
    assert_eq!(restored[0].prompt, "restore only in first session");

    application.cleanup().await;
    registration.unregister();
}

#[tokio::test]
async fn enabling_orchestration_on_reload_forwards_job_events() {
    let fixture = HermeticFixture::new(
        "reload-orchestration-events",
        vec![FauxResponse::text("done")],
        None,
        |_, agent_dir| {
            std::fs::write(
                agent_dir.join("settings.json"),
                r#"{"sandbox":{"mode":"danger-full-access"},"orchestration":{"tasks":false,"maxConcurrency":1,"maxToolsPerAgent":64}}"#,
            )
            .expect("settings");
        },
        |_| {},
        ToolSelection::default(),
    );
    let resources = fixture.resources();
    let registration = fixture.registration();
    let application = fixture.application().await;
    let mut events = application.subscribe();

    resources
        .settings_manager()
        .expect("settings manager")
        .update_global(|settings| {
            settings.orchestration = Some(OrchestrationSettings {
                tasks: Some(true),
                max_concurrency: Some(1),
                max_tools_per_agent: Some(64),
                ..OrchestrationSettings::default()
            });
        })
        .expect("enable orchestration");
    application
        .reload()
        .await
        .expect("reload enabling orchestration");
    let runtime = application
        .orchestration_runtime()
        .expect("runtime after reload");
    let spawn = runtime
        .spawn_tasks(
            runtime.main_agent_id(),
            0,
            vec![TaskItem {
                index: 0,
                id: "ForwardedChild".to_owned(),
                agent: "task".to_owned(),
                assignment: "prove reload event forwarding".to_owned(),
                todo_task_id: None,
                ..Default::default()
            }],
        )
        .expect("spawn child")
        .remove(0);

    let forwarded = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let ApplicationEvent::Orchestration(pi_coding::OrchestrationEvent::JobUpdated {
                group_id,
                job,
            }) = events.recv().await.expect("application event channel")
                && job.id == spawn.job_id
            {
                break (group_id, job);
            }
        }
    })
    .await
    .expect("forwarded orchestration event timeout");
    assert_eq!(forwarded.0, runtime.group_id());
    assert_eq!(
        forwarded.1.description.as_deref(),
        Some("prove reload event forwarding")
    );

    application.cleanup().await;
    registration.unregister();
}

#[tokio::test]
async fn compact_persists_checkpoint_and_keeps_application_transcript_usable() {
    let contexts = Arc::new(parking_lot::Mutex::new(Vec::<pi_ai::Context>::new()));
    let fixture = session_with_recorded_contexts(
        vec![
            FauxResponse::text("application checkpoint"),
            FauxResponse::text("after compact"),
        ],
        contexts.clone(),
    );
    let session = fixture.session();
    let registration = fixture.registration();
    let recorder = pi_coding::start_session_in(
        fixture.workspace(),
        session.model().as_ref(),
        Some("off"),
        Some(fixture.session_dir()),
        Some("application-compact"),
        None,
    )
    .expect("recorder");
    let mut assistant = pi_ai::AssistantMessage::pending(&session.model().expect("model"));
    assistant.content = vec![ContentBlock::text("old answer")];
    assistant.stop_reason = StopReason::Stop;
    assistant.timestamp = 2;
    let history = vec![
        pi_ai::Message::user_text("old request ".repeat(20), 1),
        pi_ai::Message::Assistant(assistant),
        pi_ai::Message::user_text("recent request", 3),
    ];
    for message in &history {
        recorder
            .record_message(message)
            .expect("record history message");
    }
    session.record(recorder).expect("attach recorder");
    session.load_history(history).await.expect("load history");
    let application = fixture.application().await;
    session.enable_compaction(pi_coding::CompactionSettings {
        enabled: true,
        reserve_tokens: 20,
        keep_recent_tokens: 4,
        snap_keep_turns: 10,
    });

    let result = application
        .compact(Some("preserve decisions"))
        .await
        .expect("compact");
    assert!(
        result.summary.ends_with("application checkpoint"),
        "{}",
        result.summary
    );
    assert!(
        application
            .session_entries(None)
            .expect("session entries")
            .entries
            .iter()
            .any(|entry| entry.entry_type == "compaction"
                && entry.summary.as_deref() == Some(result.summary.as_str()))
    );
    assert!(matches!(
        application.messages().first(),
        Some(pi_ai::Message::CompactionSummary(_))
    ));

    application
        .prompt("continue from checkpoint".into(), Vec::new(), None)
        .await
        .expect("prompt");
    application.wait_for_idle().await;
    assert_eq!(
        application.last_assistant_text().as_deref(),
        Some("after compact")
    );
    let captured = contexts.lock();
    assert_eq!(
        captured.len(),
        2,
        "compact must summarize once and the next prompt must run once"
    );
    let first_prompt = captured[1].messages.first().expect("checkpoint context");
    let pi_ai::Message::User(user) = first_prompt else {
        panic!("checkpoint must project to user context: {first_prompt:?}")
    };
    assert!(user.content.iter().any(|block| matches!(block, ContentBlock::Text { text, .. } if text.contains("application checkpoint"))));

    application.cleanup().await;
    registration.unregister();
}
/// Builds a compact-ready `Application` whose summarizer routes through the
/// REAL OpenAI-completions provider to the caller-supplied `base_url` mock SSE
/// endpoint. The sandbox is `danger-full-access` (same as [`HermeticFixture`])
/// so provider HTTP uses the raw transport and reaches a loopback listener
/// directly — this exercises the exact `Session::complete_summary` ->
/// `stream_fn` -> openai `stream_openai_completions` -> `ChatChunk` decode path
/// through a live SSE exchange, not the in-process faux provider.
async fn external_sse_application(
    model: Model,
    label: &str,
) -> (
    Session,
    Application,
    tempfile::TempDir,
    tempfile::TempDir,
    tempfile::TempDir,
) {
    let workspace = tempfile::tempdir().expect("fixture workspace");
    let agent_dir = tempfile::tempdir().expect("fixture agent dir");
    let session_dir = tempfile::tempdir().expect("fixture session dir");
    std::fs::write(agent_dir.path().join("settings.json"), DANGER_SETTINGS)
        .expect("fixture danger settings");
    let mut resource_options = ResourceManagerOptions::new(workspace.path());
    resource_options.agent_dir = agent_dir.path().to_path_buf();
    resource_options.project_trust_override = Some(true);
    resource_options.disable_extensions = true;
    resource_options.disable_skills = true;
    resource_options.disable_prompt_templates = true;
    resource_options.disable_themes = true;
    resource_options.disable_context_files = true;
    let resources = ResourceManager::new(resource_options).expect("fixture resources");
    let session = Session::new_with_additional_tools_filtered_and_discovery(
        SessionOptions {
            model,
            cwd: workspace.path().to_path_buf(),
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
        ToolSelection::default(),
        ResourceDiscovery::Disabled,
    )
    .expect("fixture session");
    session.set_session_dir(session_dir.path().to_path_buf());
    let session_handle = session.clone();
    session
        .start_new_recording()
        .expect("start fixture recording");
    let recorder = pi_coding::start_session_in(
        workspace.path(),
        session.model().as_ref(),
        Some("off"),
        Some(session_dir.path()),
        Some(label),
        None,
    )
    .expect("explicit recorder");
    session.record(recorder).expect("attach recorder");
    let mut assistant = pi_ai::AssistantMessage::pending(&session.model().expect("model"));
    assistant.content = vec![ContentBlock::text("older answer")];
    assistant.stop_reason = StopReason::Stop;
    assistant.timestamp = 2;
    let history = vec![
        pi_ai::Message::user_text("older request ".repeat(20), 1),
        pi_ai::Message::Assistant(assistant),
        pi_ai::Message::user_text("recent request", 3),
    ];
    session.load_history(history).await.expect("load history");
    session
        .attach_resources(resources.clone())
        .await
        .expect("attach resources");
    let application = publish_application(ApplicationRuntimeCandidate::new(session)).await;
    (
        session_handle,
        application,
        workspace,
        agent_dir,
        session_dir,
    )
}

/// Executes a real `Application::compact` summarization against an
/// OpenAI-compatible SSE endpoint whose text deltas explicitly carry
/// `tool_calls: null` and `reasoning_details: null` (the wire shape of DeepSeek
/// relays and OpenAI gateways on text-only chunks). Before the
/// `null_default_vec` parser fix this exact `invalid type: null, expected a
/// sequence` serde rejection escaped the compact path and aborted
/// summarization; current source must commit the summary and record a
/// compaction entry.
#[tokio::test]
async fn compact_commits_summary_through_openai_sse_with_null_optional_arrays() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock SSE server");
    let address = listener.local_addr().expect("mock SSE address");
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut socket, _) = listener.accept().await.expect("accept mock SSE request");
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            let count = socket
                .read(&mut buffer)
                .await
                .expect("read mock SSE request");
            if count == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..count]);
            if let Some(split) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&bytes[..split]);
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if bytes.len() >= split + 4 + length {
                    break;
                }
            }
        }
        let body = concat!(
            "data: {\"id\":\"compact-sse\",\"model\":\"mock\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Checkpoint summary\",\"tool_calls\":null,\"reasoning_details\":null},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"compact-sse\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" with null optional arrays\",\"tool_calls\":null,\"reasoning_details\":null},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"compact-sse\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5}}\n\n",
            "data: [DONE]\n\n"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write mock SSE response");
    });
    let mut model = Model::default();
    model.id = "compact-null-sse-app".into();
    model.name = "Compact Null SSE App".into();
    model.provider = "openai".into();
    model.api = pi_ai::API_OPENAI_COMPLETIONS.into();
    model.base_url = format!("http://{address}");
    model.context_window = 200;
    model.max_tokens = 96;
    let (session, application, _workspace, _agent_dir, _session_dir) =
        external_sse_application(model, "compact-null-sse-app").await;
    session.enable_compaction(CompactionSettings {
        enabled: true,
        reserve_tokens: 20,
        keep_recent_tokens: 4,
        snap_keep_turns: 10,
    });
    let result = application
        .compact(Some("preserve decisions"))
        .await
        .expect("compact through null SSE");
    assert_eq!(
        result.summary, "Checkpoint summary with null optional arrays",
        "the null-optional-array SSE summary must commit in full"
    );
    assert!(
        matches!(
            application.messages().first(),
            Some(pi_ai::Message::CompactionSummary(_))
        ),
        "compaction must commit a summary to the transcript: {:?}",
        application.messages().first()
    );
    let entries = application
        .session_entries(None)
        .expect("session entries")
        .entries;
    let entry = entries
        .iter()
        .find(|entry| entry.entry_type == "compaction")
        .expect("compaction entry committed");
    assert_eq!(entry.summary.as_deref(), Some(result.summary.as_str()));
    application.cleanup().await;
}

/// Negative companion: a genuinely malformed (non-null) scalar in the optional
/// delta sequence must FAIL the compact path with the contextual provider
/// error, never be silently tolerated, and must leave the transcript untouched
/// (no summary committed).
#[tokio::test]
async fn compact_fails_loudly_on_malformed_optional_array_and_keeps_state() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock SSE server");
    let address = listener.local_addr().expect("mock SSE address");
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut socket, _) = listener.accept().await.expect("accept mock SSE request");
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            let count = socket
                .read(&mut buffer)
                .await
                .expect("read mock SSE request");
            if count == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..count]);
            if let Some(split) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&bytes[..split]);
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if bytes.len() >= split + 4 + length {
                    break;
                }
            }
        }
        // `tool_calls` as a scalar (not null/array): must reject with
        // provider/chunk context, not decode silently.
        let body = concat!(
            "data: {\"id\":\"bad-sse\",\"choices\":[{\"delta\":{\"content\":\"x\",\"tool_calls\":\"not-an-array\"}}]}\n\n",
            "data: [DONE]\n\n"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write mock SSE response");
    });
    let mut model = Model::default();
    model.id = "compact-null-sse-app-bad".into();
    model.name = "Compact Null SSE App Bad".into();
    model.provider = "openai".into();
    model.api = pi_ai::API_OPENAI_COMPLETIONS.into();
    model.base_url = format!("http://{address}");
    model.context_window = 200;
    model.max_tokens = 96;
    let (session, application, _workspace, _agent_dir, _session_dir) =
        external_sse_application(model, "compact-null-sse-app-bad").await;
    session.enable_compaction(CompactionSettings {
        enabled: true,
        reserve_tokens: 20,
        keep_recent_tokens: 4,
        snap_keep_turns: 10,
    });
    let error = application
        .compact(Some("preserve decisions"))
        .await
        .expect_err("malformed optional array must fail compaction");
    let error_text = format!("{error:#}");
    assert!(
        error_text.contains("invalid OpenAI chat-completions stream chunk"),
        "malformed delta must surface with provider context: {error_text}"
    );
    assert!(
        error_text.contains("expected a sequence"),
        "the exact sequence rejection must remain visible: {error_text}"
    );
    assert!(
        !matches!(
            application.messages().first(),
            Some(pi_ai::Message::CompactionSummary(_))
        ),
        "a failed compaction must not commit a summary"
    );
    application.cleanup().await;
}
/// Contract: a user abort (`Application::abort`) during a loop-owned active
/// turn must cancel the loop iteration token BEFORE the session abort so the
/// scheduler classifies the turn as `RunResult::Cancelled` (silent; the loop
/// remains scheduled) instead of `LoopEvent::Failed` ("Loop <id> failed:
/// Request was aborted").
///
/// Plausible bug (old ordering): `session.abort().await` ran first, letting the
/// provider stream fail with "Request was aborted" and the runner settle
/// before the loop iteration token fired — the scheduler then saw
/// `runner_cancel.is_cancelled() == false` and emitted `LoopEvent::Failed` for
/// an expected user cancellation. This test uses an abort-blocked faux stream
/// (the exact provider behavior on Esc) and asserts no `LoopEvent::Failed` and
/// that the loop task remains scheduled.
#[tokio::test]
async fn abort_during_loop_turn_settles_as_cancelled_not_failed() {
    // No queued response: a custom stream blocks until the session is aborted,
    // then fails with "Request was aborted" exactly like a real provider on Esc.
    let stream_fn: pi_agent::StreamFn = Arc::new(move |model, _context, options| {
        Box::pin(async move {
            let stream = pi_ai::new_assistant_message_event_stream();
            let mut partial = pi_ai::AssistantMessage::pending(&model);
            stream
                .push(pi_ai::AssistantMessageEvent::Start {
                    partial: partial.clone(),
                })
                .await;
            if let Some(token) = options.stream.abort_signal.as_ref() {
                token.cancelled().await;
            }
            partial.stop_reason = pi_ai::StopReason::Aborted;
            partial.error_message = Some("Request was aborted".to_owned());
            stream
                .push(pi_ai::AssistantMessageEvent::Error {
                    reason: pi_ai::StopReason::Aborted,
                    error: partial,
                })
                .await;
            stream
        })
    });
    let fixture = HermeticFixture::standard("loop-abort", Vec::new(), Some(stream_fn));
    let registration = fixture.registration();
    let application = fixture.application().await;
    let mut events = application.subscribe();
    let task = application
        .loop_create(pi_coding::LoopCreateRequest::immediate(
            "30m",
            "loop abort turn",
        ))
        .await
        .expect("create loop");

    // Wait for the iteration to fire and the abort-blocked stream to be in-flight.
    let mut saw_fired = false;
    while !saw_fired {
        let event = tokio::time::timeout(Duration::from_secs(3), events.recv())
            .await
            .expect("event timeout")
            .expect("application event channel");
        if let ApplicationEvent::Loop(pi_coding::LoopEvent::Fired { task_id, .. }) = &event {
            if task_id == &task.id {
                saw_fired = true;
            }
        }
    }
    assert!(saw_fired, "loop iteration must fire");

    // User Esc: abort the loop-owned active turn.
    application.abort().await;

    // The turn must settle WITHOUT emitting LoopEvent::Failed/Finished for the
    // task, and the agent must settle (AgentSettled clears the working UI).
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    let mut saw_settled = false;
    while std::time::Instant::now() < deadline {
        let Ok(recv) = tokio::time::timeout(Duration::from_millis(100), events.recv()).await else {
            continue;
        };
        let Ok(event) = recv else { break };
        match event {
            ApplicationEvent::Loop(pi_coding::LoopEvent::Failed {
                task_id, message, ..
            }) if task_id == task.id => {
                panic!("user abort must not emit LoopEvent::Failed: {message}");
            }
            ApplicationEvent::Loop(pi_coding::LoopEvent::Finished { task_id, .. })
                if task_id == task.id =>
            {
                panic!("cancelled iteration must not emit LoopEvent::Finished");
            }
            ApplicationEvent::AgentSettled => saw_settled = true,
            _ => {}
        }
    }
    assert!(
        saw_settled,
        "abort must settle the agent (AgentSettled) to clear the working UI"
    );

    // The loop task remains scheduled; only the active iteration was cancelled.
    let remaining = application.loop_list().await.expect("loop list");
    assert!(
        remaining.iter().any(|t| t.id == task.id),
        "loop task must remain scheduled after iteration cancel: {remaining:?}"
    );

    application.wait_for_idle().await;
    registration.unregister();
}

// ---------------------------------------------------------------------------
// Unsolicited child → Main reply delivery into the live main session model
// context (typed orchestration custom message), with hub-wait deduplication.
// ---------------------------------------------------------------------------

fn irc_task_definition() -> AgentDefinition {
    AgentDefinition {
        name: "task".to_owned(),
        description: "execute an orchestration task".to_owned(),
        system_prompt: "complete the assigned task".to_owned(),
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

fn irc_runtime(artifact_dir: &Path) -> OrchestrationRuntime {
    let config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![irc_task_definition()]),
        artifact_dir,
    );
    OrchestrationRuntime::new(
        config,
        Arc::new(|_request| {
            Box::pin(async { anyhow::bail!("no child session expected in this test") })
        }),
    )
    .expect("orchestration runtime")
}

fn register_irc_worker(runtime: &OrchestrationRuntime) {
    let spawn = runtime
        .spawn_tasks(
            "Main",
            0,
            vec![TaskItem {
                index: 0,
                id: "Worker".to_owned(),
                agent: "task".to_owned(),
                assignment: "establish test sender identity".to_owned(),
                todo_task_id: None,
                ..Default::default()
            }],
        )
        .expect("register Worker sender");
    assert_eq!(spawn.len(), 1, "one Worker registration");
}

#[tokio::test]
async fn application_owns_attached_orchestration_after_source_drop() {
    let fixture = session_with_responses(Vec::new());
    let registration = fixture.registration();
    let artifacts = tempfile::tempdir().expect("artifacts");
    let application = {
        let runtime = irc_runtime(artifacts.path());
        fixture.application_with_orchestration(runtime).await
    };

    let runtime = application
        .orchestration_runtime()
        .expect("attached orchestration runtime");
    register_irc_worker(&runtime);
    let waited = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .wait_message("Main", Some("Worker"), Some(Duration::from_secs(1)), None)
                .await
        }
    });
    tokio::task::yield_now().await;
    let receipt = runtime.send("Worker", "Main", "owner transfer", None)[0].clone();
    assert!(receipt.error.is_none(), "{receipt:?}");
    let message = waited
        .await
        .expect("join wait")
        .expect("live orchestration wait")
        .expect("delivered message");
    assert_eq!(message.body, "owner transfer");

    application.cleanup().await;
    registration.unregister();
}

/// Extract the model-facing text of every message that can carry a steered
/// orchestration delivery from a captured provider context. The session
/// projects the typed custom message into a user message before the provider
/// (`pi_ai::transform::project_session_messages`), so both kinds are scanned;
/// the raw body never leaks anywhere else.
fn irc_custom_texts(context: &pi_ai::Context) -> Vec<String> {
    context
        .messages
        .iter()
        .filter_map(|message| match message {
            pi_ai::Message::Custom(custom) => Some(
                custom
                    .content
                    .to_blocks()
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>(),
            ),
            pi_ai::Message::User(user) => Some(
                user.content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>(),
            ),
            _ => None,
        })
        .collect()
}

async fn wait_for_steered_message(application: &Application, needle: &str) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let (steering, _) = application.queued_messages().await;
            if steering.iter().any(|message| {
                match message {
                pi_ai::Message::Custom(custom) => custom
                    .content
                    .to_blocks()
                    .iter()
                    .any(|block| {
                        matches!(block, ContentBlock::Text { text, .. } if text.contains(needle))
                    }),
                _ => false,
            }
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("steered message never queued");
}

/// An unsolicited child → Main reply lands in the main session's steering
/// queue while Main is idle and reaches the model context on the next run's
/// initial steering drain.
#[tokio::test]
async fn idle_main_receives_unsolicited_child_reply_on_next_run() {
    let contexts = Arc::new(parking_lot::Mutex::new(Vec::<pi_ai::Context>::new()));
    let fixture = session_with_recorded_contexts(
        vec![FauxResponse::text("main turn done")],
        contexts.clone(),
    );
    let registration = fixture.registration();
    let artifacts = tempfile::tempdir().expect("artifacts");
    let runtime = irc_runtime(artifacts.path());
    let application = fixture
        .application_with_orchestration(runtime.clone())
        .await;
    register_irc_worker(&runtime);

    // Unsolicited child → Main reply while Main is idle: truthfully queued
    // (no fake Woken) and steered into the live main session.
    let receipt = runtime.send("Worker", "Main", "async progress from child", None)[0].clone();
    assert_eq!(receipt.outcome, DeliveryOutcome::Queued);
    wait_for_steered_message(&application, "async progress from child").await;

    application
        .prompt("hello".to_owned(), Vec::new(), None)
        .await
        .expect("prompt");
    application.wait_for_idle().await;

    let captured = contexts.lock();
    assert_eq!(captured.len(), 1, "one main turn");
    assert!(
        irc_custom_texts(&captured[0])
            .into_iter()
            .any(|text| text.contains("async progress from child")),
        "the reply must reach the idle main model context on the next run"
    );
    runtime.shutdown().await;
    registration.unregister();
}

/// A child reply committed before orchestration attachment is recovered from
/// the durable Main mailbox and reaches the next model turn exactly once.
#[tokio::test]
async fn recovered_main_reply_reaches_next_model_turn_once() {
    let contexts = Arc::new(parking_lot::Mutex::new(Vec::<pi_ai::Context>::new()));
    let fixture = session_with_recorded_contexts(
        vec![FauxResponse::text("main turn done")],
        contexts.clone(),
    );
    let registration = fixture.registration();
    let application = fixture.application().await;
    let session = application.session();
    let artifacts = tempfile::tempdir().expect("artifacts");
    let runtime_a = irc_runtime(artifacts.path());
    runtime_a
        .bind_and_recover(&session)
        .expect("bind runtime A");
    register_irc_worker(&runtime_a);
    let receipts = runtime_a.send("Worker", "Main", "recovered async progress", None);
    assert!(receipts[0].error.is_none(), "{receipts:?}");
    runtime_a.shutdown().await;

    let runtime_b = irc_runtime(artifacts.path());
    application
        .attach_orchestration(runtime_b.clone())
        .await
        .expect("attach runtime B");
    wait_for_steered_message(&application, "recovered async progress").await;
    application
        .prompt("hello".to_owned(), Vec::new(), None)
        .await
        .expect("prompt");
    application.wait_for_idle().await;

    let captured = contexts.lock();
    assert_eq!(captured.len(), 1, "one main turn");
    let deliveries = irc_custom_texts(&captured[0])
        .into_iter()
        .filter(|text| text.contains("recovered async progress"))
        .count();
    assert_eq!(deliveries, 1, "recovered reply reaches model exactly once");
    assert!(
        runtime_b.inbox("Main", true).expect("inbox").is_empty(),
        "consumed recovered Main mail is durably acknowledged"
    );
    runtime_b.shutdown().await;
    registration.unregister();
}

/// A reply steered while Main is mid-turn reaches the model at its next safe
/// turn boundary (the per-turn steering drain), never the in-flight turn.
#[tokio::test]
async fn running_main_receives_unsolicited_child_reply_at_next_turn_boundary() {
    let started = Arc::new(Notify::new());
    let release = CancellationToken::new();
    let contexts = Arc::new(parking_lot::Mutex::new(Vec::<pi_ai::Context>::new()));
    let stream_started = started.clone();
    let stream_release = release.clone();
    let stream_contexts = contexts.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let stream_calls = calls.clone();
    let stream_fn: pi_agent::StreamFn = Arc::new(move |model, context, options| {
        let started = stream_started.clone();
        let release = stream_release.clone();
        let contexts = stream_contexts.clone();
        let calls = stream_calls.clone();
        Box::pin(async move {
            let call = calls.fetch_add(1, Ordering::SeqCst);
            contexts.lock().push(context);
            let stream = pi_ai::new_assistant_message_event_stream();
            let producer = stream.clone();
            tokio::spawn(async move {
                if call == 0 {
                    started.notify_waiters();
                    let abort = options.stream.abort_signal;
                    tokio::select! {
                        () = release.cancelled() => {}
                        () = async {
                            match abort {
                                Some(signal) => signal.cancelled().await,
                                None => std::future::pending::<()>().await,
                            }
                        } => {}
                    }
                }
                let mut message = pi_ai::AssistantMessage::pending(&model);
                message.content.push(ContentBlock::text(if call == 0 {
                    "first turn"
                } else {
                    "second turn"
                }));
                message.stop_reason = StopReason::Stop;
                producer.end(Some(message)).await;
            });
            stream
        })
    });
    let fixture = HermeticFixture::standard("running-main", Vec::new(), Some(stream_fn));
    let artifacts = tempfile::tempdir().expect("artifacts");
    let runtime = irc_runtime(artifacts.path());
    let application = fixture
        .application_with_orchestration(runtime.clone())
        .await;
    register_irc_worker(&runtime);

    let prompt_application = application.clone();
    let prompt_task = tokio::spawn(async move {
        prompt_application
            .prompt("start working".to_owned(), Vec::new(), None)
            .await
            .expect("prompt");
        prompt_application.wait_for_idle().await;
    });
    started.notified().await;
    let receipt = runtime.send("Worker", "Main", "urgent progress from child", None)[0].clone();
    assert_eq!(receipt.outcome, DeliveryOutcome::Queued);
    wait_for_steered_message(&application, "urgent progress from child").await;
    release.cancel();
    prompt_task.await.expect("prompt task");

    let captured = contexts.lock();
    assert!(captured.len() >= 2, "first turn + steering turn");
    assert!(
        !irc_custom_texts(&captured[0])
            .into_iter()
            .any(|text| text.contains("urgent progress from child")),
        "the in-flight turn must not see the later reply"
    );
    assert!(
        irc_custom_texts(&captured[1])
            .into_iter()
            .any(|text| text.contains("urgent progress from child")),
        "the reply must reach the running main at its next turn boundary"
    );
    runtime.shutdown().await;
}

/// A child reply claimed by an active `hub wait` is delivered through the
/// waiter and NOT also steered into the main session (no duplication), while
/// the `MessageDelivered` presentation event still publishes.
#[tokio::test]
async fn waiter_claimed_child_reply_is_not_duplicated_into_main_context() {
    let contexts = Arc::new(parking_lot::Mutex::new(Vec::<pi_ai::Context>::new()));
    let fixture = session_with_recorded_contexts(
        vec![FauxResponse::text("main turn done")],
        contexts.clone(),
    );
    let registration = fixture.registration();
    let artifacts = tempfile::tempdir().expect("artifacts");
    let runtime = irc_runtime(artifacts.path());
    let application = fixture
        .application_with_orchestration(runtime.clone())
        .await;
    register_irc_worker(&runtime);
    let mut events = application.subscribe();

    // Main blocks in `hub wait from=Worker` (the waiter owns the mailbox).
    let waiter = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .wait_message("Main", Some("Worker"), Some(Duration::from_secs(3)), None)
                .await
        }
    });
    // On the single-threaded test runtime the spawned waiter runs to its
    // first await, which happens after the claim guard is registered.
    tokio::task::yield_now().await;

    let receipt = runtime.send("Worker", "Main", "dedup me", None)[0].clone();
    assert_eq!(
        receipt.outcome,
        DeliveryOutcome::Processed,
        "an active hub wait claims the message"
    );
    let claimed = waiter.await.expect("waiter join").expect("wait succeeds");
    assert_eq!(
        claimed.as_ref().map(|message| message.body.as_str()),
        Some("dedup me")
    );

    // The app event loop must publish the delivery as waiter-claimed and skip
    // steering it into the main session.
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match events.recv().await {
                Ok(ApplicationEvent::Orchestration(
                    pi_coding::OrchestrationEvent::MessageDelivered {
                        waiter_claimed: true,
                        message,
                        ..
                    },
                )) if message.body == "dedup me" => break,
                Ok(_) => {}
                Err(_) => break,
            }
        }
    })
    .await
    .expect("waiter-claimed MessageDelivered event");

    // A subsequent main turn never sees the body as a steered custom message
    // (the waiter already returned it as its own result).
    application
        .prompt("follow up".to_owned(), Vec::new(), None)
        .await
        .expect("prompt");
    application.wait_for_idle().await;
    let captured = contexts.lock();
    for context in captured.iter() {
        assert!(
            !irc_custom_texts(context)
                .into_iter()
                .any(|text| text.contains("dedup me")),
            "waiter-claimed reply must not be duplicated into the main context"
        );
    }
    assert!(
        runtime.inbox("Main", true).expect("inbox").is_empty(),
        "the waiter must have drained the mailbox"
    );
    runtime.shutdown().await;
    registration.unregister();
}

/// The atomic waiter claim wins over an imminent timeout: a message sent
/// while a `hub wait` is registered is handed to the waiter (removed from the
/// mailbox, receipt Woken), not left stranded under a stale pre-check.
#[tokio::test]
async fn waiter_claim_wins_over_imminent_timeout() {
    let contexts = Arc::new(parking_lot::Mutex::new(Vec::<pi_ai::Context>::new()));
    let fixture = session_with_recorded_contexts(
        vec![FauxResponse::text("main turn done")],
        contexts.clone(),
    );
    let registration = fixture.registration();
    let artifacts = tempfile::tempdir().expect("artifacts");
    let runtime = irc_runtime(artifacts.path());
    let application = fixture
        .application_with_orchestration(runtime.clone())
        .await;
    register_irc_worker(&runtime);

    let waiter = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .wait_message(
                    "Main",
                    Some("Worker"),
                    Some(Duration::from_millis(100)),
                    None,
                )
                .await
        }
    });
    tokio::task::yield_now().await;
    let receipt = runtime.send("Worker", "Main", "claimed before timeout", None)[0].clone();
    assert_eq!(
        receipt.outcome,
        DeliveryOutcome::Processed,
        "the atomic claim must hand the message to the waiter"
    );
    let claimed = waiter.await.expect("waiter join").expect("wait succeeds");
    assert_eq!(
        claimed.as_ref().map(|message| message.body.as_str()),
        Some("claimed before timeout"),
        "the waiter must receive the claimed message even under an imminent timeout"
    );
    assert!(
        runtime.inbox("Main", true).expect("inbox").is_empty(),
        "the claimed message must leave the mailbox"
    );
    runtime.shutdown().await;
    registration.unregister();
}

/// A waiter that times out before the send leaves no stale claim: the message
/// is truthfully Queued, the delivery event is NOT claimed-flagged, and the
/// application steers it into the main session (nothing stranded).
#[tokio::test]
async fn timed_out_waiter_does_not_strand_reply() {
    let contexts = Arc::new(parking_lot::Mutex::new(Vec::<pi_ai::Context>::new()));
    let fixture = session_with_recorded_contexts(
        vec![FauxResponse::text("main turn done")],
        contexts.clone(),
    );
    let registration = fixture.registration();
    let artifacts = tempfile::tempdir().expect("artifacts");
    let runtime = irc_runtime(artifacts.path());
    let application = fixture
        .application_with_orchestration(runtime.clone())
        .await;
    register_irc_worker(&runtime);

    let waiter = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .wait_message(
                    "Main",
                    Some("Worker"),
                    Some(Duration::from_millis(50)),
                    None,
                )
                .await
        }
    });
    tokio::task::yield_now().await;
    // The waiter times out and fully unregisters before the send happens.
    let timed_out = tokio::time::timeout(Duration::from_secs(3), waiter)
        .await
        .expect("waiter settles")
        .expect("wait join")
        .expect("wait returns Ok(None) on timeout");
    assert!(timed_out.is_none(), "the short wait must time out");

    let receipt = runtime.send("Worker", "Main", "after the wait ended", None)[0].clone();
    assert_eq!(
        receipt.outcome,
        DeliveryOutcome::Queued,
        "no waiter means a truthful Queued (retained) outcome"
    );
    wait_for_steered_message(&application, "after the wait ended").await;
    application
        .prompt("next turn".to_owned(), Vec::new(), None)
        .await
        .expect("prompt");
    application.wait_for_idle().await;
    let captured = contexts.lock();
    assert!(
        captured.iter().any(|context| irc_custom_texts(context)
            .into_iter()
            .any(|text| text.contains("after the wait ended"))),
        "the reply must reach the main context after a timed-out waiter"
    );
    runtime.shutdown().await;
    registration.unregister();
}

/// A waiter task dropped after the atomic handoff (before returning the
/// message) restores it to the mailbox and republishes the delivery, so the
/// application still steers it into the main session — the message is never
/// stranded under a claim that never consumed it.
#[tokio::test]
async fn dropped_waiter_restores_handed_off_message() {
    let contexts = Arc::new(parking_lot::Mutex::new(Vec::<pi_ai::Context>::new()));
    let fixture = session_with_recorded_contexts(
        vec![FauxResponse::text("main turn done")],
        contexts.clone(),
    );
    let registration = fixture.registration();
    let artifacts = tempfile::tempdir().expect("artifacts");
    let runtime = irc_runtime(artifacts.path());
    let application = fixture
        .application_with_orchestration(runtime.clone())
        .await;
    register_irc_worker(&runtime);

    let waiter = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .wait_message("Main", Some("Worker"), Some(Duration::from_secs(30)), None)
                .await
        }
    });
    tokio::task::yield_now().await;
    let receipt = runtime.send("Worker", "Main", "handed then dropped", None)[0].clone();
    assert_eq!(
        receipt.outcome,
        DeliveryOutcome::Processed,
        "the atomic claim hands the message to the waiter"
    );
    // Kill the waiter task before it can receive: the guard's drop must
    // restore the message to the mailbox and republish the delivery.
    waiter.abort();
    assert!(waiter.await.is_err(), "aborted waiter join");

    // The message is back in the mailbox (never lost) and the republished
    // delivery steers it into the main session.
    assert_eq!(
        runtime.inbox("Main", true).expect("inbox").len(),
        1,
        "restored to the mailbox"
    );
    wait_for_steered_message(&application, "handed then dropped").await;
    application
        .prompt("recover turn".to_owned(), Vec::new(), None)
        .await
        .expect("prompt");
    application.wait_for_idle().await;
    let captured = contexts.lock();
    assert!(
        captured.iter().any(|context| irc_custom_texts(context)
            .into_iter()
            .any(|text| text.contains("handed then dropped"))),
        "the restored reply must still reach the main context"
    );
    runtime.shutdown().await;
    registration.unregister();
}

/// A lagged orchestration event stream replays a UI snapshot
/// (`presentation_events`), never an unconsumed context queue: historical
/// `MessageDelivered` must not be steered into the main session.
#[tokio::test]
async fn lagged_replay_does_not_steer_historical_deliveries() {
    let contexts = Arc::new(parking_lot::Mutex::new(Vec::<pi_ai::Context>::new()));
    let fixture = session_with_recorded_contexts(
        vec![FauxResponse::text("main turn done")],
        contexts.clone(),
    );
    let registration = fixture.registration();
    let artifacts = tempfile::tempdir().expect("artifacts");
    let mut config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![irc_task_definition()]),
        artifacts.path(),
    );
    config.mailbox_capacity = 300;
    let runtime = OrchestrationRuntime::new(
        config,
        Arc::new(|_request| {
            Box::pin(async { anyhow::bail!("no child session expected in this test") })
        }),
    )
    .expect("orchestration runtime");
    let application = fixture
        .application_with_orchestration(runtime.clone())
        .await;
    register_irc_worker(&runtime);

    // Burst enough deliveries to overflow the app's event buffer (256) before
    // its loop can consume any: the receiver lags and the presentation-log
    // replay runs. The retained events are still live deliveries (steered
    // once each); the replayed UI snapshot must never steer or duplicate.
    for index in 0..300 {
        runtime.send("Worker", "Main", &format!("replay-me-{index}"), None);
    }
    // Let the event loop drain the retained events; the queue then holds each
    // exactly once (any historical replay would duplicate entries).
    let bodies = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let (steering, _) = application.queued_messages().await;
            let count = steering.len();
            if count >= 256 {
                break steering;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("retained deliveries settle")
    .into_iter()
    .filter_map(|message| match message {
        pi_ai::Message::Custom(custom) => {
            let text = custom
                .content
                .to_blocks()
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<String>();
            text.find("replay-me-")
                .map(|index| text[index..].lines().next().unwrap_or_default().to_owned())
        }
        _ => None,
    })
    .collect::<Vec<_>>();
    assert_eq!(
        bodies.len(),
        bodies
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        "each retained delivery must be steered exactly once (no replay duplication)"
    );
    assert!(
        bodies.iter().all(|body| body != "replay-me-0"),
        "the dropped prefix must never be replayed from the presentation snapshot"
    );
    runtime.shutdown().await;
    registration.unregister();
}
