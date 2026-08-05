use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result};
use pi_agent::{AbortSignal, ToolCapability};
use pi_ai::ContentBlock;
use pi_coding::{
    ExtensionActionHost, ExtensionCancellation, ExtensionCapability, ExtensionContextSnapshot,
    ExtensionContextUsage, ExtensionEvent, ExtensionFlagType, ExtensionFuture, ExtensionMode,
    ExtensionOrigin, ExtensionPermissionSet, ExtensionRuntime, ExtensionRuntimeAction,
    ExtensionRuntimeOptions, ExtensionSpec, ExtensionSpecRuntime, ExtensionThemeDescriptor,
    ExtensionUiCapability, ExtensionUiContext, ExtensionUiHost, ExtensionUiRequest,
    ExtensionUiResponse, WorkingIndicatorOptions,
};
use serde_json::{Value, json};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/extensions");
const EVENT_NAMES: [&str; 33] = [
    "project_trust",
    "resources_discover",
    "session_start",
    "session_info_changed",
    "session_before_switch",
    "session_before_fork",
    "session_before_compact",
    "session_compact",
    "session_shutdown",
    "session_before_tree",
    "session_tree",
    "context",
    "before_provider_request",
    "before_provider_headers",
    "after_provider_response",
    "before_agent_start",
    "agent_start",
    "agent_end",
    "agent_settled",
    "turn_start",
    "turn_end",
    "message_start",
    "message_update",
    "message_end",
    "tool_execution_start",
    "tool_execution_update",
    "tool_execution_end",
    "model_select",
    "thinking_level_select",
    "tool_call",
    "tool_result",
    "user_bash",
    "input",
];

#[derive(Default)]
struct RecordingUi {
    requests: Mutex<Vec<ExtensionUiRequest>>,
}

impl RecordingUi {
    fn requests(&self) -> Vec<ExtensionUiRequest> {
        self.requests.lock().expect("UI request mutex").clone()
    }
}

impl ExtensionUiHost for RecordingUi {
    fn request(
        &self,
        _context: ExtensionUiContext,
        request: ExtensionUiRequest,
        _cancellation: ExtensionCancellation,
    ) -> pi_coding::ExtensionFuture<'_, Result<ExtensionUiResponse>> {
        self.requests
            .lock()
            .expect("UI request mutex")
            .push(request);
        Box::pin(async { Ok(ExtensionUiResponse::Acknowledged) })
    }

    fn clear_extension(
        &self,
        _instance: pi_coding::ExtensionInstanceId,
    ) -> pi_coding::ExtensionFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

#[derive(Default)]
struct RecordingActions {
    actions: Mutex<Vec<ExtensionRuntimeAction>>,
}

impl RecordingActions {
    fn actions(&self) -> Vec<ExtensionRuntimeAction> {
        self.actions.lock().expect("action mutex").clone()
    }
}

impl ExtensionActionHost for RecordingActions {
    fn context_snapshot(&self) -> ExtensionFuture<'_, Result<ExtensionContextSnapshot>> {
        Box::pin(async {
            Ok(ExtensionContextSnapshot {
                session_name: Some("fixture snapshot".to_owned()),
                session_id: Some("fixture-session-id".to_owned()),
                session_file: Some("/tmp/fixture-session.jsonl".to_owned()),
                is_idle: true,
                project_trusted: true,
                has_pending_messages: false,
                context_usage: Some(ExtensionContextUsage {
                    tokens: Some(123),
                    context_window: 8_192,
                    percent: Some(1.5),
                }),
                active_tools: vec!["read".to_owned()],
                all_tools: vec!["read".to_owned(), "fixture_tool".to_owned()],
                commands: vec![pi_coding::ExtensionCommandDescriptor {
                    name: "snapshot".to_owned(),
                    description: Some("Snapshot".to_owned()),
                }],
                flag_values: std::collections::BTreeMap::new(),
                system_prompt: "fixture system".to_owned(),
                model: None,
                thinking_level: pi_agent::ThinkingLevel::Low,
            })
        })
    }

    fn request(
        &self,
        _instance: pi_coding::ExtensionInstanceId,
        action: ExtensionRuntimeAction,
        _cancellation: ExtensionCancellation,
    ) -> ExtensionFuture<'_, Result<Value>> {
        self.actions.lock().expect("action mutex").push(action.clone());
        Box::pin(async move {
            Ok(match action {
                ExtensionRuntimeAction::SetModel { .. } => Value::Bool(true),
                _ => Value::Null,
            })
        })
    }
}

fn bun_executable() -> Option<PathBuf> {
    if let Some(configured) = std::env::var_os("PI_BUN_EXECUTABLE") {
        let configured = PathBuf::from(configured);
        if configured.is_file() {
            return Some(configured);
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(if cfg!(windows) { "bun.exe" } else { "bun" }))
        .find(|candidate| candidate.is_file())
}

fn options() -> ExtensionRuntimeOptions {
    ExtensionRuntimeOptions {
        mode: ExtensionMode::Tui,
        handshake_timeout: Duration::from_secs(10),
        load_timeout: Duration::from_secs(10),
        initialize_timeout: Duration::from_secs(10),
        invocation_timeout: Duration::from_secs(10),
        hook_timeout: Duration::from_secs(10),
        shutdown_timeout: Duration::from_secs(2),
        ..ExtensionRuntimeOptions::default()
    }
}

fn bun_spec(
    id: &str,
    fixture: &str,
    bun: &Path,
    capabilities: impl IntoIterator<Item = ExtensionCapability>,
    ui_capabilities: impl IntoIterator<Item = ExtensionUiCapability>,
) -> ExtensionSpec {
    let entry = PathBuf::from(FIXTURES).join(fixture);
    let mut spec = ExtensionSpec::new_runtime(
        id,
        ExtensionSpecRuntime::Bun { entry },
        PathBuf::from(FIXTURES),
        ExtensionOrigin::Project,
        true,
        ExtensionPermissionSet {
            capabilities: capabilities.into_iter().collect::<BTreeSet<_>>(),
            ui_capabilities: ui_capabilities.into_iter().collect::<BTreeSet<_>>(),
        },
    );
    spec.environment.insert(
        "PI_BUN_EXECUTABLE".to_owned(),
        bun.to_string_lossy().into_owned(),
    );
    spec
}

fn hook_value(outcomes: &[pi_coding::ExtensionHookOutcome]) -> &Value {
    assert_eq!(outcomes.len(), 1, "expected exactly one hook outcome");
    outcomes[0].result.as_ref().expect("successful hook result")
}

async fn emit_value(runtime: &ExtensionRuntime, event_name: &str, data: Value) -> Value {
    hook_value(&runtime.emit(ExtensionEvent::new(event_name, data)).await).clone()
}

#[tokio::test]
async fn real_bun_delivers_all_authoritative_event_names_and_payloads() -> Result<()> {
    let Some(bun) = bun_executable() else {
        return Ok(());
    };
    let runtime = ExtensionRuntime::process(None, options());
    let report = runtime
        .load(vec![bun_spec(
            "event-matrix",
            "event-matrix.ts",
            &bun,
            [ExtensionCapability::EventHooks],
            [],
        )])
        .await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);

    for event_name in EVENT_NAMES {
        let payload = json!({
            "marker": format!("marker-{event_name}"),
            "payload": { "event": event_name },
            "headers": { "x-original": "yes" },
            "input": { "event": event_name },
        });
        let value = emit_value(&runtime, event_name, payload).await;
        assert_eq!(value["event"]["type"], event_name);
        assert_eq!(value["event"]["marker"], format!("marker-{event_name}"));
        assert_eq!(value["event"]["payload"]["event"], event_name);
        assert_eq!(value["event"]["headers"]["x-original"], "yes");
        assert_eq!(value["event"]["input"]["event"], event_name);
        let expected_signal =
            matches!(event_name, "session_before_compact" | "session_before_tree");
        assert_eq!(value["hasSignal"], expected_signal, "event {event_name}");
    }
    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn representative_orca_extension_observes_context_and_background_ui() -> Result<()> {
    let Some(bun) = bun_executable() else {
        return Ok(());
    };
    let ui = Arc::new(RecordingUi::default());
    let runtime = ExtensionRuntime::process(Some(ui.clone()), options());
    let report = runtime
        .load(vec![bun_spec(
            "orca-agent-status",
            "orca-agent-status.ts",
            &bun,
            [
                ExtensionCapability::Commands,
                ExtensionCapability::EventHooks,
                ExtensionCapability::Ui,
            ],
            [ExtensionUiCapability::Notify, ExtensionUiCapability::Title],
        )])
        .await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);

    let value = emit_value(&runtime, "session_start", json!({ "reason": "startup" })).await;
    assert_eq!(value["observedType"], "session_start");
    assert!(value["sessionId"].is_string());
    assert!(value["idle"].is_boolean());
    assert!(value.get("sessionName").is_some());

    let requests = ui.requests();
    assert!(requests.iter().any(|request| matches!(request,
        ExtensionUiRequest::Notify { message, .. } if message == "orca fixture started"
    )));
    assert!(requests.iter().any(|request| matches!(request,
        ExtensionUiRequest::Title { title } if title == "orca fixture"
    )));

    for event_name in [
        "before_agent_start",
        "agent_start",
        "tool_execution_start",
        "tool_call",
        "tool_execution_end",
        "message_end",
        "agent_settled",
        "agent_end",
        "session_shutdown",
    ] {
        let value = emit_value(&runtime, event_name, json!({ "marker": event_name })).await;
        assert_eq!(value["observedType"], event_name);
    }
    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn hook_return_values_cross_the_real_bun_process_boundary() -> Result<()> {
    let Some(bun) = bun_executable() else {
        return Ok(());
    };
    let runtime = ExtensionRuntime::process(None, options());
    let report = runtime
        .load(vec![bun_spec(
            "hook-transforms",
            "hook-transforms.ts",
            &bun,
            [ExtensionCapability::EventHooks],
            [],
        )])
        .await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);

    let cases = [
        (
            "before_agent_start",
            json!({ "prompt": "hello", "systemPrompt": "base" }),
            json!({ "systemPrompt": "base\nfixture-system", "message": { "customType": "fixture.before-agent", "content": "fixture message", "display": true, "details": { "prompt": "hello" } } }),
        ),
        (
            "context",
            json!({ "messages": [] }),
            json!({ "messages": [{ "role": "user", "content": [{ "type": "text", "text": "fixture-context" }], "timestamp": 17 }] }),
        ),
        (
            "before_provider_request",
            json!({ "payload": { "model": "fixture" } }),
            json!({ "model": "fixture", "transformedByFixture": true }),
        ),
        (
            "before_provider_headers",
            json!({ "headers": { "x-original": "yes", "x-remove": "old" } }),
            json!({ "headers": { "x-original": "yes", "x-fixture": "yes", "x-remove": null } }),
        ),
        (
            "message_end",
            json!({ "message": { "role": "assistant", "content": [] } }),
            json!({ "message": { "role": "assistant", "content": [{ "type": "text", "text": "fixture-message" }] } }),
        ),
        (
            "tool_call",
            json!({ "toolName": "danger", "toolCallId": "call-1", "input": {} }),
            json!({ "block": true, "reason": "blocked by fixture" }),
        ),
        (
            "tool_result",
            json!({ "toolName": "read", "toolCallId": "call-1", "input": {}, "content": [], "isError": true }),
            json!({ "content": [{ "type": "text", "text": "fixture-result" }], "details": { "replaced": true }, "isError": false, "usage": { "input": 1, "output": 2, "cacheRead": 3, "cacheWrite": 4 } }),
        ),
        (
            "session_before_switch",
            json!({ "reason": "resume" }),
            json!({ "cancel": true }),
        ),
        (
            "session_before_fork",
            json!({ "entryId": "entry", "position": "at" }),
            json!({ "cancel": true, "skipConversationRestore": true }),
        ),
        (
            "session_before_compact",
            json!({ "reason": "manual", "willRetry": false }),
            json!({ "cancel": true }),
        ),
        (
            "input",
            json!({ "text": "original", "source": "interactive" }),
            json!({ "action": "transform", "text": "fixture-input" }),
        ),
    ];
    for (event_name, payload, expected) in cases {
        let value = emit_value(&runtime, event_name, payload).await;
        for (key, expected_value) in expected.as_object().expect("expected object") {
            assert_eq!(&value[key], expected_value, "event {event_name} key {key}");
        }
    }

    let tree = emit_value(
        &runtime,
        "session_before_tree",
        json!({ "preparation": { "targetId": "entry" } }),
    )
    .await;
    assert_eq!(tree["cancel"], false);
    assert_eq!(tree["summary"]["summary"], "fixture summary");
    assert_eq!(tree["customInstructions"], "fixture instructions");
    assert_eq!(tree["replaceInstructions"], true);
    assert_eq!(tree["label"], "fixture-label");
    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn real_bun_semantic_reducers_apply_transforms_and_cancellations() -> Result<()> {
    let Some(bun) = bun_executable() else { return Ok(()); };
    let runtime = ExtensionRuntime::process(None, options());
    let report = runtime.load(vec![bun_spec(
        "hook-transforms", "hook-transforms.ts", &bun,
        [ExtensionCapability::EventHooks], [],
    )]).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);

    let start = runtime.reduce_before_agent_start(
        json!({ "prompt": "hello", "systemPromptOptions": {} }), "base".to_owned(),
    ).await?;
    assert_eq!(start.system_prompt, "base\nfixture-system");
    assert_eq!(start.messages.len(), 1);
    assert_eq!(start.messages[0].custom_type, "fixture.before-agent");
    let context = runtime.reduce_context(Vec::new()).await?;
    assert_eq!(serde_json::to_value(&context)?[0]["content"][0]["text"], "fixture-context");
    assert_eq!(runtime.reduce_provider_request(json!({ "model": "fixture" })).await?["transformedByFixture"], true);
    let headers = runtime.reduce_provider_headers([
        ("x-original".to_owned(), Some("yes".to_owned())),
        ("x-remove".to_owned(), Some("old".to_owned())),
    ].into_iter().collect()).await?;
    assert_eq!(headers.get("x-fixture"), Some(&Some("yes".to_owned())));
    assert_eq!(headers.get("x-remove"), Some(&None));

    let blocked = runtime.reduce_tool_call("call-1", "danger", json!({})).await?;
    assert!(blocked.block);
    assert_eq!(blocked.reason.as_deref(), Some("blocked by fixture"));
    let result = runtime.reduce_tool_result("call-1", "read", json!({}), Vec::new(), None, true).await?;
    assert!(!result.is_error);
    assert_eq!(result.content[0], pi_ai::ContentBlock::text("fixture-result"));
    assert_eq!(result.details, Some(json!({ "replaced": true })));
    assert_eq!(result.usage.expect("fixture usage").cache_read, 3);
    let message = runtime.reduce_message_end(pi_ai::Message::user_text("original", 1)).await?;
    assert_eq!(serde_json::to_value(message)?["content"][0]["text"], "fixture-message");

    assert!(runtime.reduce_before_switch(json!({ "reason": "resume" })).await?);
    let fork = runtime.reduce_before_fork(json!({ "entryId": "entry", "position": "at" })).await?;
    assert!(fork.cancel && fork.skip_conversation_restore);
    assert!(runtime.reduce_before_compact(json!({ "reason": "manual", "willRetry": false })).await?.cancel);
    let tree = runtime.reduce_before_tree(json!({ "preparation": { "targetId": "entry" } })).await?;
    assert!(!tree.cancel);
    assert_eq!(tree.summary.expect("tree summary").summary, "fixture summary");
    assert_eq!(tree.custom_instructions.as_deref(), Some("fixture instructions"));
    assert_eq!(tree.replace_instructions, Some(true));
    assert_eq!(tree.label.as_deref(), Some("fixture-label"));
    assert_eq!(runtime.reduce_input("original".to_owned(), Vec::new(), "interactive", None).await?,
        pi_coding::ExtensionInputReduction::Continue { text: "fixture-input".to_owned(), images: Vec::new() });

    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn failed_reload_preserves_active_generation_and_discards_candidate() -> Result<()> {
    let Some(bun) = bun_executable() else {
        return Ok(());
    };
    let runtime = ExtensionRuntime::process(None, options());
    let first = runtime
        .load(vec![bun_spec(
            "event-matrix",
            "event-matrix.ts",
            &bun,
            [ExtensionCapability::EventHooks],
            [],
        )])
        .await;
    assert!(first.failures.is_empty(), "{:?}", first.failures);
    let active_generation = runtime.generation();

    let failed = runtime
        .reload(vec![bun_spec(
            "load-failure",
            "load-failure.ts",
            &bun,
            [],
            [],
        )])
        .await;
    assert_eq!(failed.failures.len(), 1);
    assert!(
        failed.failures[0]
            .message
            .contains("intentional fixture load failure")
    );
    assert_eq!(runtime.generation(), active_generation);
    let value = emit_value(
        &runtime,
        "agent_settled",
        json!({ "marker": "still-active" }),
    )
    .await;
    assert_eq!(value["event"]["type"], "agent_settled");
    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn inexpressible_tool_and_dead_registration_categories_fail_closed() -> Result<()> {
    let Some(bun) = bun_executable() else {
        return Ok(());
    };
    let cases = [
        (
            "unsupported-tool-fields",
            "unsupported-tool-fields.ts",
            vec![ExtensionCapability::Tools],
            "registerTool.constrainedSampling",
        ),
        (
            "invalid-tool-capability",
            "invalid-tool-capability.ts",
            vec![ExtensionCapability::Tools],
            "registerTool capability must be read, write, or exec",
        ),
        (
            "provider-registration",
            "provider-registration.ts",
            vec![ExtensionCapability::ProviderMetadata],
            "provider",
        ),
        (
            "renderer-registration",
            "renderer-registration.ts",
            vec![ExtensionCapability::MessageRenderers],
            "renderer",
        ),
    ];
    for (id, fixture, capabilities, expected) in cases {
        let runtime = ExtensionRuntime::process(None, options());
        let report = runtime
            .load(vec![bun_spec(id, fixture, &bun, capabilities, [])])
            .await;
        assert_eq!(
            report.failures.len(),
            1,
            "{id} must not load as a dead registration"
        );
        let message = report.failures[0].message.to_ascii_lowercase();
        assert!(
            message.contains(&expected.to_ascii_lowercase()),
            "unexpected {id} failure: {}",
            report.failures[0].message
        );
        assert!(runtime.tools().is_empty());
        assert!(runtime.provider_metadata().is_empty());
        assert!(runtime.message_renderers().is_empty());
        runtime.shutdown().await;
    }
    Ok(())
}

#[tokio::test]
async fn real_bun_routes_session_actions_through_the_versioned_action_host() -> Result<()> {
    let Some(bun) = bun_executable() else {
        return Ok(());
    };
    let actions = Arc::new(RecordingActions::default());
    let runtime = ExtensionRuntime::process(None, options());
    runtime.set_action_host(actions.clone())?;
    let report = runtime
        .load(vec![bun_spec(
            "action-protocol",
            "action-protocol.ts",
            &bun,
            [ExtensionCapability::Commands, ExtensionCapability::SessionActions],
            [],
        )])
        .await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);

    let snapshot = runtime
        .invoke_command("snapshot", String::new(), None, None)
        .await?;
    assert_eq!(snapshot["thinkingLevel"], "low");
    assert_eq!(snapshot["sessionName"], "fixture snapshot");
    assert_eq!(snapshot["activeTools"], json!(["read"]));
    assert_eq!(snapshot["allTools"], json!(["read", "fixture_tool"]));
    assert_eq!(snapshot["commands"][0]["name"], "snapshot");

    assert_eq!(
        runtime
            .invoke_command("actions", String::new(), None, None)
            .await?,
        Value::Bool(true)
    );
    let recorded = actions.actions();
    assert!(recorded.iter().any(|action| matches!(
        action,
        ExtensionRuntimeAction::SetActiveTools { tool_names }
            if tool_names == &["fixture_tool".to_owned()]
    )));
    assert!(recorded.iter().any(|action| matches!(
        action,
        ExtensionRuntimeAction::SetThinkingLevel { level }
            if *level == pi_agent::ThinkingLevel::High
    )));
    assert!(recorded.iter().any(|action| matches!(
        action,
        ExtensionRuntimeAction::SetSessionName { name } if name == "fixture session"
    )));
    assert!(recorded.iter().any(|action| matches!(
        action,
        ExtensionRuntimeAction::AppendEntry { custom_type, data }
            if custom_type == "fixture-entry" && data.as_ref().is_some_and(|value| value["persisted"] == true)
    )));
    assert!(recorded.iter().any(|action| matches!(
        action,
        ExtensionRuntimeAction::SetModel { model } if model.id == "fixture-model"
    )));
    assert_eq!(
        recorded
            .iter()
            .filter(|action| matches!(action, ExtensionRuntimeAction::SendMessage { .. }))
            .count(),
        2
    );
    assert!(recorded.iter().any(|action| matches!(
        action,
        ExtensionRuntimeAction::SendUserMessage { .. }
    )));
    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn process_inexpressible_ui_factories_are_actionably_rejected() -> Result<()> {
    let Some(bun) = bun_executable() else {
        return Ok(());
    };
    let runtime = ExtensionRuntime::process(None, options());
    let report = runtime
        .load(vec![bun_spec(
            "unsupported-ui",
            "unsupported-ui.ts",
            &bun,
            [ExtensionCapability::Commands, ExtensionCapability::Ui],
            [],
        )])
        .await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    for command in runtime.commands() {
        let error = runtime
            .invoke_command(&command.name, String::new(), None, None)
            .await
            .expect_err("unsupported UI method must reject");
        let message = error.to_string();
        assert!(
            message.contains("unavailable in the process-hosted ExtensionAPI")
                || message.contains("setWidget only supports string arrays in the process host")
                || message.contains("unsupported by the process extension protocol"),
            "command {} returned non-actionable error: {message}",
            command.name,
        );
    }
    runtime.shutdown().await;
    Ok(())
}

#[derive(Default)]
struct ValueRecordingUi {
    requests: Mutex<Vec<ExtensionUiRequest>>,
}

impl ValueRecordingUi {
    fn requests(&self) -> Vec<ExtensionUiRequest> {
        self.requests.lock().expect("UI request mutex").clone()
    }
}

impl ExtensionUiHost for ValueRecordingUi {
    fn request(
        &self,
        _context: ExtensionUiContext,
        request: ExtensionUiRequest,
        _cancellation: ExtensionCancellation,
    ) -> pi_coding::ExtensionFuture<'_, Result<ExtensionUiResponse>> {
        self.requests
            .lock()
            .expect("UI request mutex")
            .push(request.clone());
        Box::pin(async move {
            Ok(match request {
                ExtensionUiRequest::GetEditorText => ExtensionUiResponse::EditorText {
                    value: "editor-text".to_owned(),
                },
                ExtensionUiRequest::GetAllThemes => ExtensionUiResponse::Themes {
                    themes: vec![
                        ExtensionThemeDescriptor {
                            name: "dark".to_owned(),
                            path: None,
                        },
                        ExtensionThemeDescriptor {
                            name: "light".to_owned(),
                            path: Some("/themes/light.json".to_owned()),
                        },
                    ],
                },
                ExtensionUiRequest::GetTheme { name } => ExtensionUiResponse::Theme {
                    theme: (name == "dark").then(|| ExtensionThemeDescriptor {
                        name,
                        path: None,
                    }),
                },
                ExtensionUiRequest::SetTheme { name } => ExtensionUiResponse::ThemeSet {
                    success: name == "dark",
                    error: (name != "dark").then(|| "unknown theme".to_owned()),
                },
                ExtensionUiRequest::GetToolsExpanded => ExtensionUiResponse::ToolsExpanded {
                    expanded: true,
                },
                _ => ExtensionUiResponse::Acknowledged,
            })
        })
    }

    fn clear_extension(
        &self,
        _instance: pi_coding::ExtensionInstanceId,
    ) -> pi_coding::ExtensionFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn real_bun_extension_apis_perform_observable_host_actions() -> Result<()> {
    let Some(bun) = bun_executable() else {
        return Ok(());
    };
    let ui = Arc::new(ValueRecordingUi::default());
    let runtime = ExtensionRuntime::process(Some(ui.clone()), options());
    let report = runtime
        .load(vec![bun_spec(
            "extension-apis",
            "extension-apis.ts",
            &bun,
            [
                ExtensionCapability::Commands,
                ExtensionCapability::Ui,
                ExtensionCapability::SessionActions,
            ],
            [
                ExtensionUiCapability::EditorText,
                ExtensionUiCapability::Working,
                ExtensionUiCapability::HiddenThinking,
                ExtensionUiCapability::Theme,
                ExtensionUiCapability::ToolsExpanded,
            ],
        )])
        .await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);

    assert!(
        runtime.shortcuts().iter().any(|shortcut| {
            shortcut.key == "ctrl+k"
                && shortcut.description.as_deref() == Some("Unsupported process shortcut")
        }),
        "shortcut registration must preserve its truthful descriptor: {:?}",
        runtime.shortcuts(),
    );
    let flags = runtime.flags();
    assert!(
        flags.iter().any(|flag| {
            flag.name == "verbose"
                && flag.r#type == ExtensionFlagType::Boolean
                && flag.default == Some(json!(true))
        }),
        "flag registration must preserve its declared type and default: {:?}",
        flags,
    );

    let shortcut_error = runtime
        .invoke_shortcut("ctrl+k", None, None)
        .await
        .expect_err("shortcuts must not be advertised as invokable without a real dispatcher");
    assert!(shortcut_error.to_string().contains("unsupported"));

    let result = runtime
        .invoke_command("exercise-apis", String::new(), None, None)
        .await?;
    let obj = result
        .as_object()
        .expect("exercise-apis must return an object");
    assert_eq!(obj["editorText"], json!("editor-text"));
    assert_eq!(obj["themes"][0]["name"], json!("dark"));
    assert_eq!(obj["themes"][1]["path"], json!("/themes/light.json"));
    assert_eq!(obj["theme"]["name"], json!("dark"));
    assert_eq!(obj["setThemeResult"], json!({ "success": true }));
    assert_eq!(obj["missingTheme"], Value::Null);
    assert_eq!(obj["toolsExpanded"], json!(true));
    let verbose_error = obj["verboseError"].as_str().expect("registered flag must surface unsupported error text");
    assert!(
        verbose_error.contains("unsupported") && verbose_error.contains("verbose"),
        "registered flag must fail with an actionable unsupported error, got: {verbose_error}"
    );
    assert_eq!(obj["missing"], Value::Null, "unknown flags follow upstream undefined semantics");

    let requests = ui.requests();
    assert!(requests.iter().any(|request| matches!(request,
        ExtensionUiRequest::SetWorkingMessage { message } if message.as_deref() == Some("working")
    )));
    assert!(requests.iter().any(|request| matches!(request,
        ExtensionUiRequest::SetWorkingVisible { visible } if *visible
    )));
    assert!(requests.iter().any(|request| matches!(request,
        ExtensionUiRequest::SetWorkingIndicator { options: Some(WorkingIndicatorOptions { frames: Some(frames), interval_ms: Some(120) }) }
            if frames == &["·".to_owned(), "●".to_owned()]
    )));
    assert!(requests.iter().any(|request| matches!(request,
        ExtensionUiRequest::SetHiddenThinkingLabel { label } if label.as_deref() == Some("thinking…")
    )));
    assert!(requests.iter().any(|request| matches!(request,
        ExtensionUiRequest::SetToolsExpanded { expanded } if *expanded
    )));
    assert!(requests.iter().any(|request| matches!(request,
        ExtensionUiRequest::SetTheme { name } if name == "dark"
    )));
    assert!(requests.iter().any(|request| matches!(request,
        ExtensionUiRequest::PasteToEditor { text } if text == "pasted"
    )));
    assert!(requests.iter().any(|request| matches!(request,
        ExtensionUiRequest::GetEditorText
    )));
    assert!(requests.iter().any(|request| matches!(request,
        ExtensionUiRequest::GetAllThemes
    )));
    assert!(requests.iter().any(|request| matches!(request,
        ExtensionUiRequest::GetTheme { name } if name == "dark"
    )));
    assert!(requests.iter().any(|request| matches!(request,
        ExtensionUiRequest::GetToolsExpanded
    )));

    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn real_bun_extension_apis_reject_ungranted_ui_capabilities() -> Result<()> {
    let Some(bun) = bun_executable() else {
        return Ok(());
    };
    let ui = Arc::new(ValueRecordingUi::default());
    let runtime = ExtensionRuntime::process(Some(ui.clone()), options());
    let report = runtime
        .load(vec![bun_spec(
            "extension-apis",
            "extension-apis.ts",
            &bun,
            [ExtensionCapability::Commands, ExtensionCapability::Ui],
            [],
        )])
        .await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    let error = runtime
        .invoke_command("get-editor-text", String::new(), None, None)
        .await
        .expect_err("getEditorText must reject without the EditorText capability");
    let message = error.to_string();
    assert!(
        message.contains("EditorText") && message.contains("not granted"),
        "expected a permission-denied rejection for the ungranted UI capability, got: {message}",
    );
    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn real_bun_extension_apis_reject_when_no_ui_host_is_bound() -> Result<()> {
    let Some(bun) = bun_executable() else {
        return Ok(());
    };
    let mut no_ui_options = options();
    no_ui_options.mode = ExtensionMode::Print;
    let runtime = ExtensionRuntime::process(None, no_ui_options);
    let report = runtime
        .load(vec![bun_spec(
            "extension-apis",
            "extension-apis.ts",
            &bun,
            [ExtensionCapability::Commands, ExtensionCapability::Ui],
            [ExtensionUiCapability::EditorText],
        )])
        .await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    let error = runtime
        .invoke_command("get-editor-text", String::new(), None, None)
        .await
        .expect_err("getEditorText must reject when no UI host is bound");
    let message = error.to_string();
    assert!(
        message.contains("no extension UI adapter") || message.contains("ui_unavailable"),
        "expected a no-UI-adapter rejection, got: {message}",
    );
    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn real_bun_tools_register_and_execute_across_the_process_boundary() -> Result<()> {
    let Some(bun) = bun_executable() else {
        return Ok(());
    };
    let runtime = ExtensionRuntime::process(None, options());
    let report = runtime
        .load(vec![bun_spec(
            "bun-tool",
            "bun-tool.ts",
            &bun,
            [ExtensionCapability::Tools],
            [],
        )])
        .await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);

    let tools = runtime.tools();
    let echo = tools
        .iter()
        .find(|tool| tool.name == "bun_echo")
        .expect("bun_echo must be registered by the bun extension");
    assert_eq!(echo.label, "Bun Echo");
    assert_eq!(echo.description, "Echo text through the Bun extension host");
    assert_eq!(echo.capability, ToolCapability::Read);
    assert_eq!(
        runtime
            .agent_tools()
            .into_iter()
            .find(|tool| tool.name == "bun_echo")
            .expect("bun_echo AgentTool")
            .capability,
        ToolCapability::Read
    );
    let schema = serde_json::to_value(&echo.parameters).context("serializing tool schema")?;
    assert_eq!(schema["type"], json!("object"));
    assert_eq!(schema["properties"]["text"]["type"], json!("string"));
    assert_eq!(schema["required"], json!(["text"]));

    let result = runtime
        .invoke_tool(
            "bun_echo",
            "call-1".to_owned(),
            json!({ "text": "hello-e2e" }),
            AbortSignal::none(),
            None,
        )
        .await
        .context("invoking bun_echo through the real bun process")?;
    let echoed = result
        .content
        .iter()
        .find_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .expect("tool result must carry a text block");
    assert_eq!(
        echoed, "hello-e2e",
        "tool result must round-trip across the process boundary"
    );

    let error = runtime
        .invoke_tool(
            "bun_echo",
            "call-2".to_owned(),
            json!({ "text": 42 }),
            AbortSignal::none(),
            None,
        )
        .await
        .expect_err("non-string text must surface an error from the bun host");
    assert!(
        !error.to_string().is_empty(),
        "bun host error must be actionable"
    );

    runtime.shutdown().await;
    Ok(())
}
