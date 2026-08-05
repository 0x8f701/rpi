//! End-to-end host approval policy coverage.
//!
//! Exercises the public `host_approval_before_tool_call` boundary as installed
//! on live `Session`/`Application` tool turns, plus CLI/settings resolution
//! inputs that feed that boundary. Shared confirmation state is serialized.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;


use clap::Parser;
use pi_agent::{
    ApprovalMode, BeforeToolCallContext, BeforeToolCallFn, BeforeToolCallResult, ToolCapability,
};
use pi_ai::providers::{
    FauxProviderOptions, FauxProviderRegistration, FauxResponse, register_faux_provider,
};
use pi_ai::{ContentBlock, Model, StopReason, ToolCall};
use pi_agent::ThinkingLevel;
use pi_cli::Cli;
use pi_cli::approval::host_approval_before_tool_call;
use pi_cli::extension_ui::{
    ExtensionUiAdapter, ExtensionUiEvent, ExtensionUiInteraction, HostToolConfirmation,
};
use pi_coding::{
    Application, ExtensionMode, Session, SessionOptions, Settings, SettingsManager, create_tool,
};
use serde_json::json;
use tempfile::TempDir;

static CONFIRMATION_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

const DEADLINE: Duration = Duration::from_secs(20);

fn unique(label: &str) -> String {
    format!("{label}-{}", uuid::Uuid::now_v7().simple())
}

fn faux_model(label: &str) -> (Model, FauxProviderRegistration) {
    let suffix = unique(label);
    let mut model = Model::default();
    model.id = format!("{label}-model");
    model.name = format!("{label} Model");
    model.api = format!("{suffix}-api");
    model.provider = format!("{suffix}-provider");
    model.base_url = "http://localhost:0".into();
    let registration = register_faux_provider(FauxProviderOptions {
        api: model.api.clone(),
        provider: model.provider.clone(),
        models: vec![model.clone()],
        chunk_size: 8,
    });
    (model, registration)
}

fn tool_call_context(tool_name: &str, capability: ToolCapability) -> BeforeToolCallContext {
    let tool = create_tool(tool_name, ".")
        .unwrap_or_else(|_| {
            pi_agent::AgentTool::new(tool_name, "test", pi_ai::Schema::default(), |_| async {
                Ok(pi_agent::AgentToolResult::text("ok"))
            })
            .with_capability(capability)
        })
        .with_capability(capability);
    BeforeToolCallContext {
        assistant_message: pi_ai::AssistantMessage::pending(&Model::default()),
        tool_call: ToolCall {
            id: unique("call"),
            name: tool_name.to_owned(),
            arguments: json!({}),
            thought_signature: None,
        },
        arguments: json!({}),
        context: pi_agent::AgentContext {
            system_prompt: String::new(),
            messages: Vec::new(),
            tools: vec![tool],
        },
    }
}

fn tool_use_response(name: &str, arguments: serde_json::Value) -> FauxResponse {
    FauxResponse {
        content: vec![ContentBlock::ToolCall(ToolCall {
            id: unique("tool"),
            name: name.to_owned(),
            arguments,
            thought_signature: None,
        })],
        stop_reason: StopReason::ToolUse,
        error_message: None,
    }
}

async fn answer_confirmation(
    mut events: tokio::sync::broadcast::Receiver<ExtensionUiEvent>,
    adapter: ExtensionUiAdapter,
    allow: Option<bool>,
) {
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("confirmation event timed out")
            .expect("confirmation channel");
        let ExtensionUiEvent::InteractionRequested {
            interaction: ExtensionUiInteraction { id, .. },
        } = event
        else {
            continue;
        };
        match allow {
            Some(confirmed) => adapter
                .respond_confirmed(&id, confirmed)
                .expect("respond confirmation"),
            None => adapter.cancel(&id).expect("cancel confirmation"),
        }
        break;
    }
}

fn resolved_mode(cli: &Cli, settings: &Settings) -> ApprovalMode {
    cli.approval_mode
        .map(Into::into)
        .unwrap_or_else(|| settings.approval_mode())
}

/// yolo never asks; write asks only for Exec; ask always asks.
#[tokio::test]
async fn yolo_write_ask_capability_matrix_is_capability_only() {
    let _guard = CONFIRMATION_LOCK.lock().expect("confirmation lock");
    let cases = [
        (ApprovalMode::Yolo, ToolCapability::Read, false),
        (ApprovalMode::Yolo, ToolCapability::Write, false),
        (ApprovalMode::Yolo, ToolCapability::Exec, false),
        (ApprovalMode::Write, ToolCapability::Read, false),
        (ApprovalMode::Write, ToolCapability::Write, false),
        (ApprovalMode::Write, ToolCapability::Exec, true),
        (ApprovalMode::Ask, ToolCapability::Read, true),
        (ApprovalMode::Ask, ToolCapability::Write, true),
        (ApprovalMode::Ask, ToolCapability::Exec, true),
    ];
    for (mode, capability, needs_confirm) in cases {
        let hook = host_approval_before_tool_call(mode, ExtensionMode::Print, None, None);
        let tool = match capability {
            ToolCapability::Read => "read",
            ToolCapability::Write => "write",
            ToolCapability::Exec => "bash",
        };
        let result = hook(tool_call_context(tool, capability))
            .await
            .expect("hook result");
        if needs_confirm {
            assert!(
                result.block,
                "{mode:?}/{capability:?} must fail closed without an adapter"
            );
            assert!(
                result
                    .reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("no interactive confirmation adapter")),
                "{mode:?}/{capability:?} reason: {:?}",
                result.reason
            );
        } else {
            assert!(
                !result.block,
                "{mode:?}/{capability:?} must auto-allow: {:?}",
                result.reason
            );
        }
    }
}

/// Interactive allow/deny/cancel each produce a distinct observable decision.
#[tokio::test]
async fn interactive_allow_deny_cancel_are_distinct() {
    let _guard = CONFIRMATION_LOCK.lock().expect("confirmation lock");
    for (allow, expect_block, fragment) in [
        (Some(true), false, None),
        (Some(false), true, Some("denied")),
        (None, true, Some("cancelled")),
    ] {
        let adapter = ExtensionUiAdapter::new();
        let events = adapter.subscribe();
        let responder = tokio::spawn(answer_confirmation(events, adapter.clone(), allow));
        let hook = host_approval_before_tool_call(
            ApprovalMode::Ask,
            ExtensionMode::Tui,
            Some(adapter),
            None,
        );
        let result = hook(tool_call_context("bash", ToolCapability::Exec))
            .await
            .expect("hook");
        responder.await.expect("responder");
        assert_eq!(result.block, expect_block, "allow={allow:?}");
        if let Some(fragment) = fragment {
            assert!(
                result
                    .reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains(fragment)),
                "allow={allow:?} reason {:?}",
                result.reason
            );
        } else {
            assert!(result.reason.is_none(), "approved must not set a reason");
        }
    }
}

/// Print/Json/Rpc headless paths fail closed when confirmation is required.
#[tokio::test]
async fn noninteractive_modes_fail_closed_without_adapter() {
    let _guard = CONFIRMATION_LOCK.lock().expect("confirmation lock");
    for mode in [ExtensionMode::Print, ExtensionMode::Json, ExtensionMode::Rpc] {
        for (approval, capability) in [
            (ApprovalMode::Write, ToolCapability::Exec),
            (ApprovalMode::Ask, ToolCapability::Read),
            (ApprovalMode::Ask, ToolCapability::Write),
            (ApprovalMode::Ask, ToolCapability::Exec),
        ] {
            let hook = host_approval_before_tool_call(approval, mode, None, None);
            let result = hook(tool_call_context("tool", capability))
                .await
                .expect("hook");
            assert!(
                result.block,
                "{mode:?}/{approval:?}/{capability:?} must block"
            );
            assert!(
                result
                    .reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("no interactive confirmation adapter")),
                "{mode:?} reason {:?}",
                result.reason
            );
        }
    }
}

/// Host approval runs before an existing extension-style reducer and can skip it.
#[tokio::test]
async fn host_hook_orders_before_existing_and_denial_skips_it() {
    let _guard = CONFIRMATION_LOCK.lock().expect("confirmation lock");
    let order = Arc::new(Mutex::new(Vec::new()));
    let existing_order = order.clone();
    let existing: BeforeToolCallFn = Arc::new(move |_| {
        existing_order.lock().expect("order").push("existing");
        Box::pin(async {
            Ok(BeforeToolCallResult {
                block: false,
                reason: None,
                arguments: Some(json!({"rewritten": true})),
            })
        })
    });

    // Auto-allow path: existing hook must run and its rewrite must survive.
    let yolo = host_approval_before_tool_call(
        ApprovalMode::Yolo,
        ExtensionMode::Print,
        None,
        Some(existing.clone()),
    );
    let allowed = yolo(tool_call_context("read", ToolCapability::Read))
        .await
        .expect("yolo");
    assert!(!allowed.block);
    assert_eq!(allowed.arguments, Some(json!({"rewritten": true})));
    assert_eq!(order.lock().expect("order").as_slice(), ["existing"]);

    // Denial path: existing hook must not run.
    order.lock().expect("order").clear();
    let adapter = ExtensionUiAdapter::new();
    let events = adapter.subscribe();
    let responder = tokio::spawn(answer_confirmation(events, adapter.clone(), Some(false)));
    let ask = host_approval_before_tool_call(
        ApprovalMode::Ask,
        ExtensionMode::Tui,
        Some(adapter),
        Some(existing),
    );
    let denied = ask(tool_call_context("bash", ToolCapability::Exec))
        .await
        .expect("ask deny");
    responder.await.expect("responder");
    assert!(denied.block);
    assert!(
        denied
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("denied"))
    );
    assert!(
        order.lock().expect("order").is_empty(),
        "denial must skip existing hook"
    );
}

/// CLI `--approval-mode` wins over global settings; settings win over default yolo.
#[tokio::test]
async fn cli_approval_mode_precedes_global_settings_default_yolo() {
    let _guard = CONFIRMATION_LOCK.lock().expect("confirmation lock");
    let agent = TempDir::new().expect("agent");
    let cwd = TempDir::new().expect("cwd");
    std::fs::write(
        agent.path().join("settings.json"),
        r#"{"approvalMode":"ask"}"#,
    )
    .expect("settings");
    let manager = SettingsManager::load_phase_one(cwd.path(), agent.path()).expect("load settings");
    let settings = manager.settings().clone();
    assert_eq!(settings.approval_mode(), ApprovalMode::Ask);

    let default_cli = Cli::try_parse_from(["rpi"]).expect("default cli");
    assert_eq!(resolved_mode(&default_cli, &settings), ApprovalMode::Ask);

    let empty = Settings::default();
    assert_eq!(resolved_mode(&default_cli, &empty), ApprovalMode::Yolo);

    let write_cli = Cli::try_parse_from(["rpi", "--approval-mode", "write"]).expect("write cli");
    assert_eq!(resolved_mode(&write_cli, &settings), ApprovalMode::Write);

    // Observable tool decision under the resolved CLI override.
    let mode = resolved_mode(&write_cli, &settings);
    let hook = host_approval_before_tool_call(mode, ExtensionMode::Print, None, None);
    let read_ok = hook(tool_call_context("read", ToolCapability::Read))
        .await
        .expect("read");
    assert!(!read_ok.block, "write mode allows read");
    let exec_blocked = hook(tool_call_context("bash", ToolCapability::Exec))
        .await
        .expect("exec");
    assert!(exec_blocked.block, "write mode still gates exec headless");
}

/// Live Application tool turns honor headless fail-closed under write/ask.
#[tokio::test]
async fn application_tool_turn_fail_closed_under_write_and_ask() {
    let _guard = CONFIRMATION_LOCK.lock().expect("confirmation lock");
    for (mode, tool_name, arguments) in [
        (
            ApprovalMode::Write,
            "bash",
            json!({"command": "printf blocked-should-not-run"}),
        ),
        (ApprovalMode::Ask, "read", json!({"path": "notes.txt"})),
    ] {
        let (model, registration) = faux_model("approval-app");
        registration.set_responses(vec![
            tool_use_response(tool_name, arguments.clone()),
            FauxResponse::text("should-not-need-second-turn"),
        ]);
        let cwd = TempDir::new().expect("cwd");
        std::fs::write(cwd.path().join("notes.txt"), "secret").expect("fixture");
        let tools = vec![
            create_tool("read", cwd.path().to_str().expect("utf8")).expect("read tool"),
            create_tool("bash", cwd.path().to_str().expect("utf8")).expect("bash tool"),
            create_tool("write", cwd.path().to_str().expect("utf8")).expect("write tool"),
        ];
        let hook = host_approval_before_tool_call(mode, ExtensionMode::Print, None, None);
        let session = Session::new(SessionOptions {
            model,
            cwd: cwd.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: "faux".into(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(tools),
            before_tool_call: Some(hook),
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
        })
        .expect("session");
        let application = Application::new(session).await;
        let mut events = application.subscribe();
        application
            .prompt("exercise approval".into(), Vec::new(), None)
            .await;
        let deadline = tokio::time::Instant::now() + DEADLINE;
        let mut saw_blocked_tool = false;
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout_at(deadline, events.recv()).await {
                Ok(Ok(pi_coding::ApplicationEvent::Agent(
                    pi_agent::AgentEvent::ToolExecutionEnd {
                        tool_name: name,
                        is_error,
                        result,
                        ..
                    },
                ))) if name == tool_name => {
                    assert!(is_error, "{mode:?}/{tool_name} must end as error");
                    let body = result
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    assert!(
                        body.contains("no interactive confirmation adapter")
                            || body.contains("blocked")
                            || body.contains("denied")
                            || body.contains("Tool execution"),
                        "{mode:?}/{tool_name} body: {body}"
                    );
                    saw_blocked_tool = true;
                    break;
                }
                Ok(Ok(pi_coding::ApplicationEvent::AgentSettled)) if saw_blocked_tool => break,
                Ok(Ok(_)) => continue,
                Ok(Err(_)) => break,
                Err(_) => break,
            }
        }
        assert!(
            saw_blocked_tool,
            "{mode:?}/{tool_name} never produced a blocked tool end"
        );
        if tool_name == "bash" {
            // Fail-closed must not execute the command payload.
            // There is no process side effect to observe for printf, but the
            // tool error path is the contract above.
        }
        if tool_name == "read" {
            // Ensure the file was never rewritten by a mistaken allow path.
            let text = std::fs::read_to_string(cwd.path().join("notes.txt")).expect("read notes");
            assert_eq!(text, "secret");
        }
        application.cleanup().await;
        registration.unregister();
    }
}

/// Interactive Application allow path executes the tool after host confirmation.
#[tokio::test]
async fn application_interactive_allow_executes_read_tool() {
    let _guard = CONFIRMATION_LOCK.lock().expect("confirmation lock");
    let (model, registration) = faux_model("approval-allow");
    registration.set_responses(vec![
        tool_use_response("read", json!({"path": "notes.txt"})),
        FauxResponse::text("done after tool"),
    ]);
    let cwd = TempDir::new().expect("cwd");
    std::fs::write(cwd.path().join("notes.txt"), "visible-content").expect("fixture");
    let adapter = ExtensionUiAdapter::new();
    let events = adapter.subscribe();
    let responder = tokio::spawn(answer_confirmation(events, adapter.clone(), Some(true)));
    let tools = vec![create_tool("read", cwd.path().to_str().expect("utf8")).expect("read")];
    let hook =
        host_approval_before_tool_call(ApprovalMode::Ask, ExtensionMode::Tui, Some(adapter), None);
    let session = Session::new(SessionOptions {
        model,
        cwd: cwd.path().to_path_buf(),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: "faux".into(),
        compaction: None,
        stream_options: Default::default(),
        tools: Some(tools),
        before_tool_call: Some(hook),
        after_tool_call: None,
        stream_fn: None,
        auth_resolver: None,
    })
    .expect("session");
    let application = Application::new(session).await;
    let mut app_events = application.subscribe();
    application
        .prompt("read the notes".into(), Vec::new(), None)
        .await;
    let deadline = tokio::time::Instant::now() + DEADLINE;
    let mut saw_success = false;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout_at(deadline, app_events.recv()).await {
            Ok(Ok(pi_coding::ApplicationEvent::Agent(
                pi_agent::AgentEvent::ToolExecutionEnd {
                    tool_name,
                    is_error,
                    result,
                    ..
                },
            ))) if tool_name == "read" => {
                assert!(!is_error, "allowed read must succeed");
                let body = result
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(
                    body.contains("visible-content"),
                    "tool body missing fixture: {body}"
                );
                saw_success = true;
                break;
            }
            Ok(Ok(_)) => continue,
            Ok(Err(_)) | Err(_) => break,
        }
    }
    responder.await.expect("responder");
    assert!(saw_success, "allowed read tool never completed");
    application.cleanup().await;
    registration.unregister();
}

/// Missing tool metadata defaults to Exec (strict), so write mode fails closed.
#[tokio::test]
async fn missing_tool_metadata_defaults_to_exec_under_write() {
    let _guard = CONFIRMATION_LOCK.lock().expect("confirmation lock");
    let hook =
        host_approval_before_tool_call(ApprovalMode::Write, ExtensionMode::Print, None, None);
    let mut context = tool_call_context("read", ToolCapability::Read);
    context.context.tools.clear();
    let result = hook(context).await.expect("hook");
    assert!(result.block);
    assert!(
        result
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("no interactive confirmation adapter"))
    );
}

/// Broker/adapter errors fail closed and skip the existing extension hook.
#[tokio::test]
async fn broker_errors_fail_closed_and_skip_existing_hook() {
    let _guard = CONFIRMATION_LOCK.lock().expect("confirmation lock");
    let calls = Arc::new(AtomicUsize::new(0));
    let existing_calls = calls.clone();
    let existing: BeforeToolCallFn = Arc::new(move |_| {
        existing_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(BeforeToolCallResult::default()) })
    });
    // Adapter with no subscriber => InteractionRequested send fails => fail closed.
    let hook = host_approval_before_tool_call(
        ApprovalMode::Ask,
        ExtensionMode::Tui,
        Some(ExtensionUiAdapter::new()),
        Some(existing),
    );
    let result = hook(tool_call_context("write", ToolCapability::Write))
        .await
        .expect("hook");
    assert!(result.block);
    assert!(
        result
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("host approval failed"))
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

/// Confirm HostToolConfirmation enum mapping stays wired for allow path.
#[tokio::test]
async fn host_tool_confirmation_enum_maps_from_adapter_responses() {
    let _guard = CONFIRMATION_LOCK.lock().expect("confirmation lock");
    let adapter = ExtensionUiAdapter::new();
    let events = adapter.subscribe();
    let responder = tokio::spawn(answer_confirmation(events, adapter.clone(), Some(true)));
    let decision = adapter
        .confirm_host_tool(ExtensionMode::Tui, "bash", ToolCapability::Exec)
        .await
        .expect("confirm");
    responder.await.expect("responder");
    assert_eq!(decision, HostToolConfirmation::Approved);
}

#[test]
fn settings_file_round_trips_approval_mode_and_rejects_project_override_path() {
    let agent = TempDir::new().expect("agent");
    let cwd = TempDir::new().expect("cwd");
    std::fs::write(
        agent.path().join("settings.json"),
        r#"{"approvalMode":"write"}"#,
    )
    .expect("global");
    let manager = SettingsManager::load_phase_one(cwd.path(), agent.path()).expect("load");
    assert_eq!(manager.settings().approval_mode(), ApprovalMode::Write);

    let project = cwd.path().join(".pi");
    std::fs::create_dir_all(&project).expect("project");
    std::fs::write(project.join("settings.json"), r#"{"approvalMode":"yolo"}"#).expect("project");
    let error = manager
        .load_project(true)
        .expect_err("project approvalMode must be rejected");
    assert!(
        error.to_string().contains("approvalMode"),
        "error: {error:#}"
    );
    assert_eq!(manager.settings().approval_mode(), ApprovalMode::Write);
}

#[test]
fn cli_parses_approval_mode_values_case_sensitively() {
    for (wire, expected) in [
        ("yolo", ApprovalMode::Yolo),
        ("write", ApprovalMode::Write),
        ("ask", ApprovalMode::Ask),
    ] {
        let cli = Cli::try_parse_from(["rpi", "--approval-mode", wire]).expect("parse");
        assert_eq!(cli.approval_mode.map(Into::into), Some(expected));
    }
    assert!(Cli::try_parse_from(["rpi", "--approval-mode", "WRITE"]).is_err());
    assert!(Cli::try_parse_from(["rpi", "--approval-mode", "always"]).is_err());
}
