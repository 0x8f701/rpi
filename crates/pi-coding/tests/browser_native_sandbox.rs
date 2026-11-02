#![cfg_attr(not(unix), allow(unused_imports, dead_code))]

use std::path::Path;
use std::sync::Arc;

use pi_agent::{AbortController, AgentTool, AgentToolResult, ToolCallContext, ToolUpdateFn};
use pi_ai::providers::{FauxProviderOptions, FauxProviderRegistration, register_faux_provider};
use pi_ai::{ContentBlock, Model};
use pi_coding::{
    Application, BrowserSandboxError, NativeBrowserSandboxFailure, ResourceManager,
    ResourceManagerOptions, Session, SessionOptions,
};
use serde_json::{Value, json};

fn application_session(cwd: &Path) -> (Session, FauxProviderRegistration) {
    let suffix = uuid::Uuid::now_v7().to_string();
    let api = format!("browser-native-sandbox-api-{suffix}");
    let provider = format!("browser-native-sandbox-provider-{suffix}");
    let model = Model {
        id: format!("browser-native-sandbox-model-{suffix}"),
        name: "Browser native sandbox model".to_owned(),
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
    let session = Session::new(SessionOptions {
        model,
        cwd: cwd.to_path_buf(),
        system_prompt: String::new(),
        thinking_level: pi_agent::ThinkingLevel::Off,
        api_key: String::new(),
        compaction: None,
        stream_options: Default::default(),
        tools: None,
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: None,
        auth_resolver: None,
        auth_manager: None,
    })
    .expect("session");
    (session, registration)
}

async fn browser_application(cwd: &Path) -> (Application, FauxProviderRegistration) {
    let agent_dir = cwd.join(".agent-test");
    std::fs::create_dir_all(&agent_dir).expect("agent directory");
    // Danger mode is the hermetic, launcher-free baseline (it already implies
    // full network access; an explicit `network` would conflict with the
    // mode's policy contract).
    std::fs::write(
        agent_dir.join("settings.json"),
        serde_json::to_vec(&json!({
            "sandbox": {
                "mode": "danger-full-access"
            }
        }))
        .expect("settings json"),
    )
    .expect("settings");
    let mut options = ResourceManagerOptions::new(cwd);
    options.agent_dir = agent_dir;
    options.project_trust_override = Some(true);
    options.disable_extensions = true;
    options.disable_skills = true;
    options.disable_prompt_templates = true;
    options.disable_themes = true;
    options.disable_context_files = true;
    let resources = ResourceManager::new(options).expect("resources");
    let (session, registration) = application_session(cwd);
    session
        .attach_resources(resources)
        .await
        .expect("attach resources");
    (Application::new(session).await, registration)
}

fn browser_tool(application: &Application) -> AgentTool {
    application
        .get_all_tools()
        .into_iter()
        .find(|tool| tool.name == "browser")
        .expect("generation-bound browser tool")
}

fn prepared_arguments(tool: &AgentTool, arguments: Value) -> Value {
    tool.prepare_arguments
        .as_ref()
        .expect("browser argument preparation")(arguments)
    .expect("prepared arguments")
}

async fn execute_approved(tool: &AgentTool, arguments: Value) -> anyhow::Result<AgentToolResult> {
    let arguments = prepared_arguments(tool, arguments);
    let approval = tool
        .prepare_approval
        .as_ref()
        .expect("browser approval preparation")(arguments.clone())
    .await?
    .expect("browser launch approval");
    assert_eq!(approval.summary.target_fds.len(), 2);
    assert_eq!(approval.summary.target_fds[0].target_fd, 3);
    assert_eq!(approval.summary.target_fds[1].target_fd, 4);
    let token = approval.issue().await?;
    let (controller, abort) = AbortController::new();
    std::mem::forget(controller);
    let on_update: ToolUpdateFn = Arc::new(|_result: AgentToolResult| {});
    (tool.execute)(ToolCallContext {
        tool_call_id: "browser-native-sandbox".to_owned(),
        arguments,
        on_update,
        abort,
        model: None,
        approval_token: Some(token),
    })
    .await
}

fn result_text(result: &AgentToolResult) -> &str {
    match result.content.first() {
        Some(ContentBlock::Text { text, .. }) => text,
        _ => "",
    }
}

/// The only live governed-browser proof: it launches a real Chrome via the
/// governed debugging pipe. Chrome is not available on every host (notably
/// the static-gates runner), so the proof is `#[ignore]`d by default — the
/// runner reports it as ignored, never as a green pass. An explicit
/// `--include-ignored` run without `CHROME_PATH` fails loudly instead of
/// silently passing.
#[cfg(unix)]
#[ignore = "live browser proof: requires CHROME_PATH pointing at a real Chrome/Chromium binary (run with --include-ignored)"]
#[tokio::test]
async fn governed_debugging_pipe_returns_browser_version() {
    let chrome = match std::env::var_os("CHROME_PATH") {
        Some(chrome) if !chrome.is_empty() => chrome,
        _ => panic!(
            "governed_debugging_pipe_returns_browser_version requires CHROME_PATH to run; it is #[ignore]d by default"
        ),
    };
    assert!(
        Path::new(&chrome).is_absolute(),
        "CHROME_PATH must be absolute"
    );
    let cwd = tempfile::tempdir().expect("workspace");
    let (application, registration) = browser_application(cwd.path()).await;
    let tool = browser_tool(&application);
    let result = execute_approved(&tool, json!({ "action": "list_tabs" }))
        .await
        .expect("governed browser debugging pipe");
    assert!(result_text(&result).contains("tab(s)"), "{result:?}");
    application.cleanup().await;
    registration.unregister();
}

#[test]
fn browser_errors_are_complete_and_redacted() {
    let secret = ["sk-", "browser-native-sandbox-secret"].concat();
    let native = [
        NativeBrowserSandboxFailure::Timeout { timeout_ms: 15_000 },
        NativeBrowserSandboxFailure::ProcessExited {
            code: Some(9),
            stderr_bytes: secret.len(),
        },
        NativeBrowserSandboxFailure::DebuggingPipeClosed {
            stderr_bytes: secret.len(),
        },
        NativeBrowserSandboxFailure::FrameTooLarge { limit: 1024 * 1024 },
        NativeBrowserSandboxFailure::InvalidFrame,
        NativeBrowserSandboxFailure::CommandRejected,
    ];
    assert_eq!(
        native
            .iter()
            .map(NativeBrowserSandboxFailure::variant_name)
            .collect::<Vec<_>>(),
        [
            "timeout",
            "process-exited",
            "debugging-pipe-closed",
            "frame-too-large",
            "invalid-frame",
            "command-rejected",
        ]
    );
    for failure in native {
        let error = BrowserSandboxError::Native(failure);
        assert_eq!(error.variant_name(), "native");
        assert!(!error.to_string().contains(&secret));
        assert!(!format!("{error:?}").contains(&secret));
    }
    let filesystem_probe = BrowserSandboxError::Native(NativeBrowserSandboxFailure::InvalidFrame);
    assert!(!filesystem_probe.is_filesystem());
    assert!(!format!("{filesystem_probe:?}").contains(&secret));
}

#[cfg(windows)]
#[tokio::test]
async fn windows_browser_is_rejected_before_approval() {
    let cwd = tempfile::tempdir().expect("workspace");
    let (application, registration) = browser_application(cwd.path()).await;
    let tool = browser_tool(&application);
    let arguments = prepared_arguments(&tool, json!({ "action": "list_tabs" }));
    let error = tool
        .prepare_approval
        .as_ref()
        .expect("browser approval preparation")(arguments)
    .await
    .expect_err("Windows browser approval must fail closed");
    assert!(error.downcast_ref::<BrowserSandboxError>().is_some());
    assert!(error.to_string().contains("unsupported"));
    application.cleanup().await;
    registration.unregister();
}

#[test]
fn browser_filesystem_errors_are_typed() {
    let error = BrowserSandboxError::filesystem_denied_for_test();
    assert!(error.is_filesystem());
    assert_eq!(error.variant_name(), "filesystem");
    assert!(error.to_string().contains("filesystem"));
}
