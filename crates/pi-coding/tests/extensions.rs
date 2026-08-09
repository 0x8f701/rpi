use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::Result;
use anyhow::anyhow;
use pi_agent::{AbortController, AgentToolResult, ThinkingLevel, ToolCapability};
use pi_ai::{
    AssistantMessageEvent, AssistantMessageEventStream, ContentBlock, Context, Message, Model,
    SimpleStreamOptions, StopReason, UserMessage, get_api_provider, is_extension_provider,
    stream_simple,
};
use pi_coding::{
    ExtensionActionHost, ExtensionCancellation, ExtensionCapability, ExtensionContextSnapshot,
    ExtensionEvent, ExtensionInputReduction, ExtensionInstanceId, ExtensionManifestRuntime,
    ExtensionMode, ExtensionOrigin, ExtensionPermissionSet, ExtensionRuntime,
    ExtensionRuntimeAction, ExtensionRuntimeOptions, ExtensionSpec, ExtensionSpecRuntime,
    ExtensionThemeDescriptor, ExtensionUiCapability, ExtensionUiContext,
    ExtensionUiHost, ExtensionUiRequest, ExtensionUiResponse,
    PackageResourceKind, PackageResourceSpec, PackageScope,
    ProcessExtensionManifest, Session, SessionOptions, UiNotificationLevel, UiWidgetPlacement,
    WorkingIndicatorOptions, extension_spec_from_package_resource,
};
use serde_json::{Value, json};
use tempfile::TempDir;

fn resource(path: PathBuf, trusted: bool) -> PackageResourceSpec {
    PackageResourceSpec {
        kind: PackageResourceKind::Extension,
        path,
        package_id: "test-package".to_owned(),
        scope: PackageScope::Project,
        trusted,
    }
}

fn write_quickjs_package(root: &TempDir, entry: &str) -> Result<PathBuf> {
    let manifest = root.path().join("pi-extension.json");
    std::fs::write(
        &manifest,
        serde_json::to_vec_pretty(&json!({
            "schemaVersion": 1,
            "id": "quickjs-test",
            "runtime": "quickjs",
            "entry": entry,
            "capabilities": ["commands", "tools", "event_hooks", "ui"],
            "uiCapabilities": ["notify"]
        }))?,
    )?;
    Ok(manifest)
}

#[test]
fn legacy_process_manifest_deserializes_unchanged() -> Result<()> {
    let manifest: ProcessExtensionManifest = serde_json::from_value(json!({
        "schemaVersion": 1,
        "id": "legacy",
        "executable": "./main",
        "arguments": ["serve"],
        "capabilities": ["tools"],
        "uiCapabilities": []
    }))?;
    assert_eq!(
        manifest.runtime,
        ExtensionManifestRuntime::Process {
            executable: PathBuf::from("./main"),
            arguments: vec!["serve".to_owned()],
        }
    );
    Ok(())
}

#[test]
fn quickjs_manifest_rejects_untrusted_traversal_and_unsupported_entry() -> Result<()> {
    let untrusted = TempDir::new()?;
    std::fs::write(untrusted.path().join("index.mjs"), "export default () => {}")?;
    let manifest = write_quickjs_package(&untrusted, "index.mjs")?;
    let error = extension_spec_from_package_resource(&resource(manifest, false))
        .expect_err("untrusted project QuickJS extension must fail closed");
    assert!(
        error
            .to_string()
            .contains("untrusted project extension manifest")
    );

    let traversal = TempDir::new()?;
    let manifest = write_quickjs_package(&traversal, "../outside.mjs")?;
    let error = extension_spec_from_package_resource(&resource(manifest, true))
        .expect_err("traversal must fail closed");
    assert!(error.to_string().contains("must remain inside"));

    let unsupported = TempDir::new()?;
    std::fs::write(unsupported.path().join("index.py"), "pass")?;
    let manifest = write_quickjs_package(&unsupported, "index.py")?;
    let error = extension_spec_from_package_resource(&resource(manifest, true))
        .expect_err("unsupported entry must fail closed");
    assert!(
        error
            .to_string()
            .contains("must end in .js or .mjs"),
        "{error:#}"
    );
    Ok(())
}

#[derive(Default)]
struct NotifyUi;

impl ExtensionUiHost for NotifyUi {
    fn request(
        &self,
        _context: ExtensionUiContext,
        request: ExtensionUiRequest,
        _cancellation: ExtensionCancellation,
    ) -> pi_coding::ExtensionFuture<'_, Result<ExtensionUiResponse>> {
        Box::pin(async move {
            assert!(matches!(request, ExtensionUiRequest::Notify { .. }));
            Ok(ExtensionUiResponse::Acknowledged)
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
async fn trusted_quickjs_manifest_registers_commands_tools_and_event_hooks() -> Result<()> {
    let root = TempDir::new()?;
    std::fs::write(
        root.path().join("index.mjs"),
        r#"
export default function (pi) {
  pi.registerCommand("hello", {
    description: "Say hello",
    handler: async (_args, ctx) => ctx.ui.notify("hello from quickjs", "info"),
  });
  pi.registerCommand("alpha", {
    description: "first chain step",
    handler: async (args) => `alpha:${args || "none"}`,
  });
  pi.registerCommand("beta", {
    description: "second chain step",
    handler: async (args) => `beta:${args || "none"}`,
  });
  pi.registerTool({
    name: "echo_quickjs",
    label: "Echo QuickJS",
    description: "Echo a value",
    parameters: {
      type: "object",
      properties: { value: { type: "string" } },
      required: ["value"],
    },
    execute: async (_id, params, _signal, onUpdate) => {
      onUpdate?.({ content: [{ type: "text", text: "partial" }] });
      return { content: [{ type: "text", text: `qjs:${params.value}` }] };
    },
  });
  pi.on("session_start", async (event) => ({ observed: event.reason }));
}
"#,
    )?;
    let manifest_path = write_quickjs_package(&root, "index.mjs")?;
    let spec = extension_spec_from_package_resource(&resource(manifest_path, true))?;
    assert!(matches!(spec.runtime, ExtensionSpecRuntime::QuickJs { .. }));
    assert_eq!(spec.origin, ExtensionOrigin::Project);
    assert!(spec.project_trusted);

    let runtime = ExtensionRuntime::process(Some(Arc::new(NotifyUi)), quickjs_options());
    let report = runtime.load(vec![spec]).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert_eq!(runtime.generation(), 1, "first trusted load must mint generation 1");

    let command_names = runtime
        .commands()
        .into_iter()
        .map(|command| command.name)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        command_names,
        BTreeSet::from([
            "hello".to_owned(),
            "alpha".to_owned(),
            "beta".to_owned()
        ])
    );
    assert_eq!(runtime.tools()[0].name, "echo_quickjs");
    assert_eq!(
        runtime.tools()[0].capability,
        ToolCapability::Exec,
        "omitted QuickJS tool capability must fail safe"
    );

    runtime
        .invoke_command("hello", String::new(), None, None)
        .await?;

    // /run and /chain dispatch is ordered sequential invoke_command; defend that
    // contract here without a PTY so CI stays deterministic.
    let run = runtime
        .invoke_command("alpha", "hello".to_owned(), None, None)
        .await?;
    assert_eq!(run, json!("alpha:hello"));

    let mut chain = Vec::new();
    for (name, args) in [("alpha", "one"), ("beta", "two")] {
        let value = runtime
            .invoke_command(name, args.to_owned(), None, None)
            .await?;
        chain.push((name.to_owned(), value));
    }
    assert_eq!(
        chain,
        vec![
            ("alpha".to_owned(), json!("alpha:one")),
            ("beta".to_owned(), json!("beta:two")),
        ]
    );

    let (_, signal) = AbortController::new();
    let result: AgentToolResult = runtime
        .invoke_tool(
            "echo_quickjs",
            "call-1".to_owned(),
            json!({ "value": "ok" }),
            signal,
            None,
        )
        .await?;
    let encoded = serde_json::to_value(result)?;
    assert_eq!(encoded["content"][0]["text"], "qjs:ok");
    let outcomes = runtime
        .emit(pi_coding::ExtensionEvent::new(
            "session_start",
            json!({ "reason": "test" }),
        ))
        .await;
    assert_eq!(outcomes.len(), 1);
    assert_eq!(
        outcomes[0].result.as_ref().expect("hook result")["observed"],
        "test"
    );
    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn quickjs_extension_rejects_oversized_outbound_frame() -> Result<()> {
    let root = TempDir::new()?;
    std::fs::write(
        root.path().join("index.mjs"),
        r#"
export default function (pi) {
  pi.registerCommand("oversized", {
    handler: () => "x".repeat(4096),
  });
}
"#,
    )?;
    let manifest_path = root.path().join("pi-extension.json");
    std::fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&json!({
            "schemaVersion": 1,
            "id": "oversized-quickjs",
            "runtime": "quickjs",
            "entry": "index.mjs",
            "capabilities": ["commands"]
        }))?,
    )?;
    let spec = extension_spec_from_package_resource(&resource(manifest_path, true))?;
    let runtime = ExtensionRuntime::process(
        None,
        ExtensionRuntimeOptions {
            max_frame_bytes: 1024,
            ..quickjs_options()
        },
    );
    let report = runtime.load(vec![spec]).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    let error = runtime
        .invoke_command("oversized", String::new(), None, None)
        .await
        .expect_err("oversized QuickJS response must fail closed");
    assert!(
        error.to_string().contains("frame exceeds 1024 bytes"),
        "{error:#}"
    );
    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn project_local_untrusted_executable_extension_is_refused() -> Result<()> {
    let root = TempDir::new()?;
    let executable = root.path().join("payload.sh");
    std::fs::write(&executable, "#!/bin/sh\necho 'should-not-run'\n")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&executable)?.permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions)?;
    }
    let manifest = root.path().join("pi-extension.json");
    std::fs::write(
        &manifest,
        serde_json::to_vec_pretty(&json!({
            "schemaVersion": 1,
            "id": "untrusted-exec",
            "runtime": "process",
            "executable": "payload.sh",
            "capabilities": ["commands"]
        }))?,
    )?;

    // Package discovery refuses untrusted project manifests before launch.
    let package_error = extension_spec_from_package_resource(&resource(manifest, false))
        .expect_err("untrusted project executable extension must fail closed");
    assert!(
        package_error
            .to_string()
            .contains("untrusted project extension manifest"),
        "{package_error:#}"
    );

    // Direct launch path also refuses project-local untrusted process specs.
    let permissions = ExtensionPermissionSet {
        capabilities: BTreeSet::from([ExtensionCapability::Commands]),
        ui_capabilities: BTreeSet::<ExtensionUiCapability>::new(),
    };
    let spec = ExtensionSpec::new_runtime(
        "untrusted-exec",
        ExtensionSpecRuntime::Process { executable },
        root.path(),
        ExtensionOrigin::Project,
        false,
        permissions,
    );
    let runtime = ExtensionRuntime::process(None, ExtensionRuntimeOptions::default());
    let report = runtime.load(vec![spec]).await;
    assert_eq!(report.failures.len(), 1, "{:?}", report.failures);
    assert!(
        report.failures[0]
            .message
            .contains("refusing to execute untrusted project extension"),
        "{}",
        report.failures[0].message
    );
    assert!(runtime.commands().is_empty());
    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn quickjs_sandbox_exposes_no_process_or_host_globals() -> Result<()> {
    // The in-process QuickJS runtime replaces the former Bun child-process
    // security model: there is no subprocess whose environment could be
    // scrubbed, so the sandbox contract is "no Node/Bun-style escape hatches".
    // Extension code must not see process/require/fetch or other host globals.
    let root = TempDir::new()?;
    std::fs::write(
        root.path().join("index.mjs"),
        r#"
export default function (pi) {
  pi.registerCommand("sandbox_probe", {
    handler: () => ({
      process: typeof process,
      require: typeof require,
      fetch: typeof fetch,
      Buffer: typeof Buffer,
      console: typeof console,
      setTimeout: typeof setTimeout,
      global: typeof global,
    }),
  });
}
"#,
    )?;
    let manifest_path = write_quickjs_package(&root, "index.mjs")?;
    let spec = extension_spec_from_package_resource(&resource(manifest_path, true))?;
    let runtime = ExtensionRuntime::process(None, quickjs_options());
    let report = runtime.load(vec![spec]).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);

    let result = runtime
        .invoke_command("sandbox_probe", String::new(), None, None)
        .await?;
    for key in [
        "process",
        "require",
        "fetch",
        "Buffer",
        "console",
        "setTimeout",
        "global",
    ] {
        assert_eq!(
            result[key],
            json!("undefined"),
            "sandbox leaked host global {key}: {result}"
        );
    }
    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn failed_quickjs_reload_keeps_prior_generation_after_invalid_update() -> Result<()> {
    let good = TempDir::new()?;
    std::fs::write(
        good.path().join("index.mjs"),
        r#"
export default function (pi) {
  pi.registerCommand("stable", {
    handler: async () => "stable-ok",
  });
}
"#,
    )?;
    let good_manifest = good.path().join("pi-extension.json");
    std::fs::write(
        &good_manifest,
        serde_json::to_vec_pretty(&json!({
            "schemaVersion": 1,
            "id": "stable-ext",
            "runtime": "quickjs",
            "entry": "index.mjs",
            "capabilities": ["commands"]
        }))?,
    )?;
    let good_spec = extension_spec_from_package_resource(&resource(good_manifest, true))?;

    let runtime = ExtensionRuntime::process(None, quickjs_options());
    let loaded = runtime.load(vec![good_spec]).await;
    assert!(loaded.failures.is_empty(), "{:?}", loaded.failures);
    let generation = runtime.generation();
    assert_eq!(generation, 1);
    assert_eq!(
        runtime
            .invoke_command("stable", String::new(), None, None)
            .await?,
        json!("stable-ok")
    );

    // Invalid candidate: factory throws during load so validate-then-apply discards.
    let bad = TempDir::new()?;
    std::fs::write(
        bad.path().join("index.mjs"),
        "export default function () { throw new Error('intentional reload failure'); }\n",
    )?;
    let bad_manifest = bad.path().join("pi-extension.json");
    std::fs::write(
        &bad_manifest,
        serde_json::to_vec_pretty(&json!({
            "schemaVersion": 1,
            "id": "broken-ext",
            "runtime": "quickjs",
            "entry": "index.mjs",
            "capabilities": ["commands"]
        }))?,
    )?;
    let bad_spec = extension_spec_from_package_resource(&resource(bad_manifest, true))?;

    let failed = runtime.reload(vec![bad_spec]).await;
    assert_eq!(failed.failures.len(), 1, "{:?}", failed.failures);
    assert!(
        failed.failures[0]
            .message
            .to_ascii_lowercase()
            .contains("intentional reload failure")
            || failed.failures[0]
                .message
                .to_ascii_lowercase()
                .contains("load"),
        "reload failure should be actionable: {}",
        failed.failures[0].message
    );
    assert_eq!(
        runtime.generation(),
        generation,
        "failed reload must keep the prior generation"
    );
    assert_eq!(
        runtime
            .commands()
            .iter()
            .map(|command| command.name.as_str())
            .collect::<Vec<_>>(),
        vec!["stable"]
    );
    assert_eq!(
        runtime
            .invoke_command("stable", String::new(), None, None)
            .await?,
        json!("stable-ok"),
        "prior generation must remain fully callable after invalid reload"
    );

    // stage + discard path also preserves the live generation.
    let missing_root = TempDir::new()?;
    let permissions = ExtensionPermissionSet {
        capabilities: BTreeSet::from([ExtensionCapability::Commands]),
        ui_capabilities: BTreeSet::<ExtensionUiCapability>::new(),
    };
    let missing_spec = ExtensionSpec::new_runtime(
        "missing-entry",
        ExtensionSpecRuntime::QuickJs {
            entry: missing_root.path().join("missing.mjs"),
        },
        missing_root.path(),
        ExtensionOrigin::Project,
        true,
        permissions,
    );
    let candidate = runtime.stage_reload(vec![missing_spec]).await;
    let staged = candidate.report();
    assert!(
        !staged.failures.is_empty(),
        "missing QuickJS entry must fail during stage_reload: {staged:?}"
    );
    assert!(
        staged.loaded.is_empty(),
        "failed stage must not report loaded instances"
    );
    assert_eq!(runtime.generation(), generation);
    runtime.discard_reload(candidate).await;
    assert_eq!(runtime.generation(), generation);
    assert_eq!(
        runtime
            .invoke_command("stable", String::new(), None, None)
            .await?,
        json!("stable-ok")
    );

    runtime.shutdown().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// In-process QuickJS runtime (Phase 1). These tests never require Bun: the
// QuickJS runtime is embedded, so the whole suite runs without external tools.
// ---------------------------------------------------------------------------

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/extensions")
}

fn quickjs_package(root: &TempDir, entry: &str, capabilities: &[&str]) -> Result<PathBuf> {
    std::fs::copy(fixture_dir().join(entry), root.path().join(entry))?;
    let manifest = root.path().join("pi-extension.json");
    std::fs::write(
        &manifest,
        serde_json::to_vec_pretty(&json!({
            "schemaVersion": 1,
            "id": "quickjs-test",
            "runtime": "quickjs",
            "entry": entry,
            "capabilities": capabilities,
        }))?,
    )?;
    Ok(manifest)
}

/// `quickjs_package` variant that also grants the extension's UI capabilities
/// (the manifest's `uiCapabilities` field), which the host enforces per
/// request via `ExtensionPermissionSet::allows_ui`.
fn quickjs_package_with_ui(
    root: &TempDir,
    entry: &str,
    capabilities: &[&str],
    ui_capabilities: &[&str],
) -> Result<PathBuf> {
    std::fs::copy(fixture_dir().join(entry), root.path().join(entry))?;
    let manifest = root.path().join("pi-extension.json");
    std::fs::write(
        &manifest,
        serde_json::to_vec_pretty(&json!({
            "schemaVersion": 1,
            "id": "quickjs-test",
            "runtime": "quickjs",
            "entry": entry,
            "capabilities": capabilities,
            "uiCapabilities": ui_capabilities,
        }))?,
    )?;
    Ok(manifest)
}

fn quickjs_options() -> ExtensionRuntimeOptions {
    ExtensionRuntimeOptions {
        mode: ExtensionMode::Tui,
        handshake_timeout: Duration::from_secs(10),
        load_timeout: Duration::from_secs(10),
        initialize_timeout: Duration::from_secs(10),
        invocation_timeout: Duration::from_secs(10),
        hook_timeout: Duration::from_secs(10),
        shutdown_timeout: Duration::from_secs(5),
        ..ExtensionRuntimeOptions::default()
    }
}

/// A minimal but valid context snapshot for the test action hosts. The host
/// contract rejects real snapshot errors, so the fixtures must answer with a
/// valid snapshot rather than a shortcut error.
fn test_context_snapshot() -> ExtensionContextSnapshot {
    ExtensionContextSnapshot {
        session_name: None,
        session_id: None,
        session_file: None,
        is_idle: true,
        project_trusted: true,
        has_pending_messages: false,
        context_usage: None,
        active_tools: Vec::new(),
        all_tools: Vec::new(),
        commands: Vec::new(),
        flag_values: std::collections::BTreeMap::new(),
        system_prompt: String::new(),
        model: None,
        thinking_level: pi_agent::ThinkingLevel::default(),
    }
}

/// Action host that answers `SetSessionName` so the JS-side action round-trip
/// can be exercised end to end.
#[derive(Default)]
struct RecordingActionHost;

impl ExtensionActionHost for RecordingActionHost {
    fn context_snapshot(&self) -> pi_coding::ExtensionFuture<'_, Result<ExtensionContextSnapshot>> {
        Box::pin(async { Ok(test_context_snapshot()) })
    }

    fn request(
        &self,
        _instance: ExtensionInstanceId,
        action: ExtensionRuntimeAction,
        _cancellation: ExtensionCancellation,
    ) -> pi_coding::ExtensionFuture<'_, Result<Value>> {
        Box::pin(async move {
            match action {
                ExtensionRuntimeAction::SetSessionName { name } => Ok(json!({ "name": name })),
                other => Err(anyhow!("unexpected extension action {other:?}")),
            }
        })
    }
}

/// Action host for the Phase 4 surface: records every action (so tests can
/// assert exact payload shapes and use an action round-trip as a readiness
/// signal) and answers each action with a deterministic, echo-style value.
/// `SetModel` returns the boolean the acceptance contract expects.
#[derive(Default)]
struct Phase4ActionHost {
    actions: Arc<Mutex<Vec<ExtensionRuntimeAction>>>,
}

impl ExtensionActionHost for Phase4ActionHost {
    fn context_snapshot(&self) -> pi_coding::ExtensionFuture<'_, Result<ExtensionContextSnapshot>> {
        Box::pin(async { Ok(test_context_snapshot()) })
    }

    fn request(
        &self,
        _instance: ExtensionInstanceId,
        action: ExtensionRuntimeAction,
        _cancellation: ExtensionCancellation,
    ) -> pi_coding::ExtensionFuture<'_, Result<Value>> {
        let actions = self.actions.clone();
        Box::pin(async move {
            actions.lock().unwrap().push(action.clone());
            match action {
                ExtensionRuntimeAction::SetModel { .. } => Ok(json!(true)),
                ExtensionRuntimeAction::SetThinkingLevel { level } => {
                    Ok(json!({ "level": format!("{level:?}") }))
                }
                ExtensionRuntimeAction::SetActiveTools { tool_names } => {
                    Ok(json!({ "tools": tool_names }))
                }
                ExtensionRuntimeAction::SetLabel { entry_id, label } => {
                    Ok(json!({ "entryId": entry_id, "label": label }))
                }
                ExtensionRuntimeAction::AppendEntry { custom_type, data } => {
                    Ok(json!({ "customType": custom_type, "data": data }))
                }
                ExtensionRuntimeAction::SendMessage {
                    message,
                    delivery,
                    trigger_turn,
                } => Ok(json!({
                    "customType": message.custom_type,
                    "delivery": format!("{delivery:?}"),
                    "triggerTurn": trigger_turn,
                })),
                ExtensionRuntimeAction::SendUserMessage { content, delivery } => {
                    Ok(json!({ "content": content, "delivery": format!("{delivery:?}") }))
                }
                ExtensionRuntimeAction::Abort => Ok(json!({ "action": "abort" })),
                ExtensionRuntimeAction::Shutdown => Ok(json!({ "action": "shutdown" })),
                ExtensionRuntimeAction::Compact { custom_instructions } => {
                    Ok(json!({ "customInstructions": custom_instructions }))
                }
                ExtensionRuntimeAction::WaitForIdle => Ok(json!({ "action": "wait_for_idle" })),
                ExtensionRuntimeAction::Reload => Ok(json!({ "action": "reload" })),
                other => Err(anyhow!("unexpected extension action {other:?}")),
            }
        })
    }
}

#[test]
fn quickjs_manifest_rejects_typescript_entry() -> Result<()> {
    let root = TempDir::new()?;
    std::fs::write(root.path().join("index.ts"), "export default () => {}\n")?;
    let manifest = root.path().join("pi-extension.json");
    std::fs::write(
        &manifest,
        serde_json::to_vec_pretty(&json!({
            "schemaVersion": 1,
            "id": "quickjs-ts",
            "runtime": "quickjs",
            "entry": "index.ts",
            "capabilities": ["commands"]
        }))?,
    )?;
    let error = extension_spec_from_package_resource(&resource(manifest, true))
        .expect_err("QuickJS must reject TypeScript entries actionably");
    assert!(
        error.to_string().contains(".js or .mjs"),
        "rejection must name the supported extensions: {error:#}"
    );
    assert!(
        error.to_string().contains("TypeScript"),
        "rejection must explain why .ts is unsupported: {error:#}"
    );
    Ok(())
}

#[test]
fn quickjs_manifest_rejects_process_inexpressible_capabilities() -> Result<()> {
    for capability in ["message_renderers", "provider_metadata"] {
        let root = TempDir::new()?;
        std::fs::write(root.path().join("index.mjs"), "export default () => {}\n")?;
        let manifest = root.path().join("pi-extension.json");
        std::fs::write(
            &manifest,
            serde_json::to_vec_pretty(&json!({
                "schemaVersion": 1,
                "id": "unsupported-quickjs-capability",
                "runtime": "quickjs",
                "entry": "index.mjs",
                "capabilities": [capability]
            }))?,
        )?;
        let error = extension_spec_from_package_resource(&resource(manifest, true))
            .expect_err("process-inexpressible QuickJS capability must fail closed");
        assert!(error.to_string().contains(capability), "{error:#}");
    }
    Ok(())
}

#[tokio::test]
async fn trusted_quickjs_manifest_registers_js_commands_and_tools() -> Result<()> {
    let root = TempDir::new()?;
    let manifest_path = quickjs_package(
        &root,
        "quickjs-basic.mjs",
        &["commands", "tools", "session_actions", "event_hooks"],
    )?;
    let spec = extension_spec_from_package_resource(&resource(manifest_path, true))?;
    assert!(
        matches!(spec.runtime, ExtensionSpecRuntime::QuickJs { .. }),
        "quickjs manifest must produce a QuickJs spec runtime"
    );
    assert_eq!(spec.origin, ExtensionOrigin::Project);
    assert!(spec.project_trusted);

    let runtime = ExtensionRuntime::process(None, quickjs_options());
    let report = runtime.load(vec![spec]).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert_eq!(runtime.generation(), 1, "first trusted load must mint generation 1");

    let command_names = runtime
        .commands()
        .into_iter()
        .map(|command| command.name)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        command_names,
        BTreeSet::from([
            "hello".to_owned(),
            "alpha".to_owned(),
            "beta".to_owned(),
            "rename".to_owned()
        ])
    );
    assert_eq!(runtime.tools()[0].name, "echo_quickjs");
    assert_eq!(
        runtime.tools()[0].capability,
        ToolCapability::Exec,
        "omitted QuickJS tool capability must fail safe"
    );

    let value = runtime
        .invoke_command("hello", "quickjs".to_owned(), None, None)
        .await?;
    assert_eq!(value, json!("hello:quickjs"));

    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn quickjs_works_without_bun() -> Result<()> {
    let root = TempDir::new()?;
    let manifest_path = quickjs_package(
        &root,
        "quickjs-basic.mjs",
        &["commands", "tools", "session_actions", "event_hooks"],
    )?;
    let mut spec = extension_spec_from_package_resource(&resource(manifest_path, true))?;
    spec.environment.insert(
        "PI_BUN_EXECUTABLE".to_owned(),
        root.path()
            .join("missing-bun")
            .to_string_lossy()
            .into_owned(),
    );

    let runtime = ExtensionRuntime::process(None, quickjs_options());
    let report = runtime.load(vec![spec]).await;
    assert!(
        report.failures.is_empty(),
        "QuickJS must not depend on a Bun binary: {:?}",
        report.failures
    );
    assert_eq!(
        runtime
            .invoke_command("hello", String::new(), None, None)
            .await?,
        json!("hello:world")
    );
    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn quickjs_invokes_registered_command_and_tool() -> Result<()> {
    let root = TempDir::new()?;
    let manifest_path = quickjs_package(
        &root,
        "quickjs-basic.mjs",
        &["commands", "tools", "session_actions", "event_hooks"],
    )?;
    let spec = extension_spec_from_package_resource(&resource(manifest_path, true))?;
    let runtime = ExtensionRuntime::process(None, quickjs_options());
    runtime.set_action_host(Arc::new(RecordingActionHost))?;
    let report = runtime.load(vec![spec]).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);

    // /run and /chain dispatch is ordered sequential invoke_command; defend that
    // contract here without a PTY so CI stays deterministic.
    let mut chain = Vec::new();
    for (name, args) in [("alpha", "one"), ("beta", "two")] {
        let value = runtime
            .invoke_command(name, args.to_owned(), None, None)
            .await?;
        chain.push((name.to_owned(), value));
    }
    assert_eq!(
        chain,
        vec![
            ("alpha".to_owned(), json!("alpha:one")),
            ("beta".to_owned(), json!("beta:two")),
        ]
    );

    // Tool invocation: arguments bridge JSON -> JS, the result bridges back.
    let (_, signal) = AbortController::new();
    let result: AgentToolResult = runtime
        .invoke_tool(
            "echo_quickjs",
            "call-1".to_owned(),
            json!({ "value": "ok" }),
            signal,
            None,
        )
        .await?;
    let encoded = serde_json::to_value(result)?;
    assert_eq!(encoded["content"][0]["text"], "qjs:ok");

    // Session action round-trip: JS -> host action channel -> JS promise.
    let value = runtime
        .invoke_command("rename", "session-a".to_owned(), None, None)
        .await?;
    assert_eq!(value, json!({ "name": "session-a" }));

    runtime.shutdown().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// In-process QuickJS Phase 4: tool onUpdate streaming, session actions over
// the action channel, and the AbortController/AbortSignal cancellation shim.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn quickjs_tool_updates_stream_to_the_host_before_the_result() -> Result<()> {
    let root = TempDir::new()?;
    let manifest_path = quickjs_package(
        &root,
        "quickjs-phase4.mjs",
        &["commands", "tools", "session_actions"],
    )?;
    let spec = extension_spec_from_package_resource(&resource(manifest_path, true))?;
    let runtime = ExtensionRuntime::process(None, quickjs_options());
    runtime.set_action_host(Arc::new(Phase4ActionHost::default()))?;
    let report = runtime.load(vec![spec]).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);

    // The tool's execute calls onUpdate twice then returns; both Update frames
    // must reach the host's on_update handler in order, before the result.
    let updates = Arc::new(Mutex::new(Vec::new()));
    let (_, signal) = AbortController::new();
    let recorded = updates.clone();
    let result: AgentToolResult = runtime
        .invoke_tool(
            "phase4_stream",
            "call-1".to_owned(),
            json!({ "value": "ok" }),
            signal,
            Some(Arc::new(move |update: AgentToolResult| {
                recorded.lock().unwrap().push(update);
            })),
        )
        .await?;

    let received = updates.lock().unwrap().clone();
    assert_eq!(received.len(), 2, "both onUpdate calls must stream to the host");
    assert_eq!(
        serde_json::to_value(&received[0])?["content"][0]["text"],
        "partial",
        "the first Update frame must carry the first partial payload"
    );
    assert_eq!(
        serde_json::to_value(&received[1])?["content"][0]["text"],
        "second",
        "the second Update frame must carry the second partial payload"
    );
    assert_eq!(
        serde_json::to_value(&received[1])?["details"]["step"],
        2,
        "the second Update frame must carry its details payload"
    );
    let encoded = serde_json::to_value(result)?;
    assert_eq!(
        encoded["content"][0]["text"], "final:ok",
        "the result must arrive after the updates"
    );

    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn quickjs_session_actions_resolve_with_the_host_result() -> Result<()> {
    let root = TempDir::new()?;
    let manifest_path = quickjs_package(
        &root,
        "quickjs-phase4.mjs",
        &["commands", "tools", "session_actions"],
    )?;
    let spec = extension_spec_from_package_resource(&resource(manifest_path, true))?;
    let runtime = ExtensionRuntime::process(None, quickjs_options());
    let host = Arc::new(Phase4ActionHost::default());
    runtime.set_action_host(host.clone())?;
    let report = runtime.load(vec![spec]).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);

    // Every session action (pi + ctx) round-trips the host result back.
    let value = runtime
        .invoke_command("phase4_session_actions", String::new(), None, None)
        .await?;
    let results = value
        .as_object()
        .expect("session action results must be an object");
    assert_eq!(results["setModel"], json!(true), "pi.setModel must resolve with the host's boolean");
    assert_eq!(results["setThinkingLevel"], json!({ "level": "High" }));
    assert_eq!(
        results["setActiveTools"],
        json!({ "tools": ["tool-a", "tool-b"] })
    );
    assert_eq!(
        results["setLabel"],
        json!({ "entryId": "entry-1", "label": "label-x" })
    );
    assert_eq!(
        results["appendEntry"],
        json!({ "customType": "custom-type", "data": { "key": "value" } })
    );
    assert_eq!(
        results["sendMessage"],
        json!({ "customType": "custom", "delivery": "Steer", "triggerTurn": false })
    );
    assert_eq!(
        results["sendUserMessage"],
        json!({ "content": "hello", "delivery": "FollowUp" })
    );
    assert_eq!(results["abort"], json!({ "action": "abort" }));
    assert_eq!(results["shutdown"], json!({ "action": "shutdown" }));
    assert_eq!(
        results["compact"],
        json!({ "customInstructions": "trim it" })
    );
    assert_eq!(results["waitForIdle"], json!({ "action": "wait_for_idle" }));
    assert_eq!(results["reload"], json!({ "action": "reload" }));

    // The recorded actions carry the exact host-side payloads, proving the
    // JS -> ExtensionRuntimeAction mapping (camelCase fields, delivery
    // variants, optional data) matches the process bridge's shapes.
    let recorded = host.actions.lock().unwrap().clone();
    assert!(recorded.iter().any(|action| matches!(action,
        ExtensionRuntimeAction::SetModel { model } if model.id == "test-model"
    )));
    assert!(recorded.iter().any(|action| matches!(action,
        ExtensionRuntimeAction::SetThinkingLevel { level } if *level == pi_agent::ThinkingLevel::High
    )));
    assert!(recorded.iter().any(|action| matches!(action,
        ExtensionRuntimeAction::SetActiveTools { tool_names }
            if tool_names == &vec!["tool-a".to_owned(), "tool-b".to_owned()]
    )));
    assert!(recorded.iter().any(|action| matches!(action,
        ExtensionRuntimeAction::SetLabel { entry_id, label }
            if entry_id == "entry-1" && label.as_deref() == Some("label-x")
    )));
    assert!(recorded.iter().any(|action| matches!(action,
        ExtensionRuntimeAction::AppendEntry { custom_type, data }
            if custom_type == "custom-type" && data.as_ref().is_some_and(|data| data["key"] == "value")
    )));
    assert!(recorded.iter().any(|action| matches!(action,
        ExtensionRuntimeAction::SendMessage { message, delivery, trigger_turn }
            if message.custom_type == "custom"
                && *delivery == pi_coding::ExtensionMessageDelivery::Steer
                && !*trigger_turn
    )));
    assert!(recorded.iter().any(|action| matches!(action,
        ExtensionRuntimeAction::SendUserMessage { content, delivery }
            if content == &pi_ai::CustomMessageContent::Text("hello".to_owned())
                && *delivery == pi_coding::ExtensionMessageDelivery::FollowUp
    )));
    assert!(recorded.iter().any(|action| matches!(action,
        ExtensionRuntimeAction::Compact { custom_instructions }
            if custom_instructions.as_deref() == Some("trim it")
    )));
    assert!(recorded.iter().any(|action| matches!(action, ExtensionRuntimeAction::Abort)));
    assert!(recorded.iter().any(|action| matches!(action, ExtensionRuntimeAction::Shutdown)));
    assert!(recorded.iter().any(|action| matches!(action, ExtensionRuntimeAction::WaitForIdle)));
    assert!(recorded.iter().any(|action| matches!(action, ExtensionRuntimeAction::Reload)));

    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn quickjs_set_model_resolves_with_the_host_boolean() -> Result<()> {
    let root = TempDir::new()?;
    let manifest_path = quickjs_package(
        &root,
        "quickjs-phase4.mjs",
        &["commands", "tools", "session_actions"],
    )?;
    let spec = extension_spec_from_package_resource(&resource(manifest_path, true))?;
    let runtime = ExtensionRuntime::process(None, quickjs_options());
    runtime.set_action_host(Arc::new(Phase4ActionHost::default()))?;
    let report = runtime.load(vec![spec]).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);

    let value = runtime
        .invoke_command("phase4_set_model", String::new(), None, None)
        .await?;
    assert_eq!(
        value,
        json!(true),
        "pi.setModel must resolve with the host's boolean"
    );

    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn quickjs_cancel_settles_a_pending_invocation_promptly() -> Result<()> {
    let root = TempDir::new()?;
    let manifest_path = quickjs_package(
        &root,
        "quickjs-phase4.mjs",
        &["commands", "tools", "session_actions"],
    )?;
    let spec = extension_spec_from_package_resource(&resource(manifest_path, true))?;
    let runtime = ExtensionRuntime::process(None, quickjs_options());
    let host = Arc::new(Phase4ActionHost::default());
    runtime.set_action_host(host.clone())?;
    let report = runtime.load(vec![spec]).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);

    // phase4_hang awaits a never-settling promise and publishes readiness by
    // round-tripping a session action *after* registering its abort listener,
    // so the test only cancels once the listener is definitely registered.
    let cancellation = ExtensionCancellation::new();
    let cancel_flag = cancellation.clone();
    let cancel_runtime = runtime.clone();
    let cancel_handle = tokio::spawn(async move {
        cancel_runtime
            .invoke_command(
                "phase4_hang",
                String::new(),
                Some(Duration::from_secs(30)),
                Some(cancellation),
            )
            .await
    });

    let readiness = tokio::time::sleep(Duration::from_secs(5));
    tokio::pin!(readiness);
    loop {
        let ready = host
            .actions
            .lock()
            .unwrap()
            .iter()
            .any(|action| matches!(action, ExtensionRuntimeAction::SetActiveTools { .. }));
        if ready {
            break;
        }
        tokio::select! {
            _ = &mut readiness => {
                panic!("hang command never reached its readiness action");
            }
            _ = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
    }
    cancel_flag.cancel();

    // The shim must settle the invocation well under the 30s host timeout.
    let settled = tokio::time::timeout(Duration::from_secs(5), cancel_handle)
        .await
        .expect("cancelled invocation must settle promptly via the abort shim");
    let join_result = settled.expect("cancel join");
    let message = join_result
        .expect_err("cancelled hang command must fail, got a success value")
        .to_string();
    assert!(
        message.contains("cancelled") || message.contains("cancel"),
        "expected a cancellation error, got: {message}"
    );

    // The shim settled the invocation: the extension is no longer wedged and
    // accepts new work promptly (without the shim the active future would
    // block every subsequent request until its host-side timeout).
    assert_eq!(
        runtime
            .invoke_command(
                "probe",
                String::new(),
                Some(Duration::from_secs(5)),
                None,
            )
            .await?,
        json!("phase4-probe-ok")
    );

    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn quickjs_session_actions_require_the_capability() -> Result<()> {
    let root = TempDir::new()?;
    std::fs::write(
        root.path().join("index.mjs"),
        r#"
export default function (pi) {
  pi.registerCommand("capability-probe", {
    handler: async () => {
      await pi.setModel({ id: "m", name: "M", api: "a", provider: "p", baseUrl: "u", contextWindow: 1, maxTokens: 1 });
      return "unreachable";
    },
  });
}
"#,
    )?;
    let manifest = root.path().join("pi-extension.json");
    std::fs::write(
        &manifest,
        serde_json::to_vec_pretty(&json!({
            "schemaVersion": 1,
            "id": "quickjs-no-session-actions",
            "runtime": "quickjs",
            "entry": "index.mjs",
            "capabilities": ["commands"]
        }))?,
    )?;
    let spec = extension_spec_from_package_resource(&resource(manifest, true))?;
    let runtime = ExtensionRuntime::process(None, quickjs_options());
    let report = runtime.load(vec![spec]).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);

    // Without the session_actions capability the request bridge throws
    // synchronously, mirroring the process bridge's requireCapability(
    // "session_actions", "session action").
    let error = runtime
        .invoke_command("capability-probe", String::new(), None, None)
        .await
        .expect_err("pi.setModel must require the session_actions capability");
    let message = error.to_string();
    assert!(
        message.contains("requires the session_actions capability"),
        "expected a capability-gate rejection, got: {message}",
    );

    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn quickjs_memory_and_interrupt_are_wired() -> Result<()> {
    let root = TempDir::new()?;
    let manifest_path = quickjs_package(&root, "quickjs-smoke.mjs", &["commands"])?;
    let spec = extension_spec_from_package_resource(&resource(manifest_path, true))?;
    let runtime = ExtensionRuntime::process(
        None,
        ExtensionRuntimeOptions {
            // Short JS-side deadline: the interrupt handler must fire well
            // before the host-side request timeout so the smoke test is fast.
            invocation_timeout: Duration::from_millis(750),
            ..quickjs_options()
        },
    );
    let report = runtime.load(vec![spec]).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);

    // Interrupt: a bytecode spin must be killed by set_interrupt_handler.
    let spin_error = runtime
        .invoke_command(
            "spin",
            String::new(),
            Some(Duration::from_secs(15)),
            None,
        )
        .await
        .expect_err("spin must be interrupted by the host deadline");
    assert!(
        spin_error.to_string().contains("interrupt"),
        "spin failure must surface the interrupt: {spin_error:#}"
    );

    // The runtime must stay usable after an interrupt.
    assert_eq!(
        runtime
            .invoke_command("probe", String::new(), Some(Duration::from_secs(5)), None)
            .await?,
        json!("probe-ok")
    );

    // Memory: an allocation bomb must be rejected by set_memory_limit.
    let memory_error = runtime
        .invoke_command(
            "allocate",
            String::new(),
            Some(Duration::from_secs(15)),
            None,
        )
        .await
        .expect_err("allocate must be rejected by the memory limit");
    assert!(
        memory_error.to_string().contains("out of memory")
            || memory_error.to_string().contains("memory"),
        "allocation failure must mention memory: {memory_error:#}"
    );

    // The runtime must stay usable after the OOM rejection.
    assert_eq!(
        runtime
            .invoke_command("probe", String::new(), Some(Duration::from_secs(5)), None)
            .await?,
        json!("probe-ok")
    );

    runtime.shutdown().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// In-process QuickJS event hooks (Phase 2). These tests assert the
// authoritative event semantics against the event-matrix .mjs fixture.
// ---------------------------------------------------------------------------

/// The authoritative event allow-list (`crate::quickjs_host`'s
/// `SUPPORTED_EVENTS`). The quickjs runtime must deliver exactly these names
/// and nothing else. Names that were once allow-listed but never produced
/// (`project_trust`, `resources_discover`, `overlay_open`, `overlay_close`)
/// are rejected at registration instead and covered by
/// [`removed_event_names_fail_registration`].
const AUTHORITATIVE_EVENT_NAMES: [&str; 32] = [
    "trust_decision",
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

/// Load a quickjs fixture with the given capabilities and assert the load
/// succeeds; the returned `TempDir` keeps the fixture's working directory
/// alive for the lifetime of the runtime.
async fn load_quickjs_fixture(
    fixture: &str,
    capabilities: &[&str],
) -> Result<(TempDir, ExtensionRuntime)> {
    let root = TempDir::new()?;
    let manifest_path = quickjs_package(&root, fixture, capabilities)?;
    let spec = extension_spec_from_package_resource(&resource(manifest_path, true))?;
    let runtime = ExtensionRuntime::process(None, quickjs_options());
    let report = runtime.load(vec![spec]).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    Ok((root, runtime))
}

fn hook_value(outcomes: &[pi_coding::ExtensionHookOutcome]) -> &Value {
    assert_eq!(outcomes.len(), 1, "expected exactly one hook outcome");
    outcomes[0].result.as_ref().expect("successful hook result")
}

#[tokio::test]
async fn quickjs_delivers_all_authoritative_event_names_and_payloads() -> Result<()> {
    let (_root, runtime) =
        load_quickjs_fixture("quickjs-event-matrix.mjs", &["event_hooks"]).await?;

    for event_name in AUTHORITATIVE_EVENT_NAMES {
        let payload = json!({
            "marker": format!("marker-{event_name}"),
            "payload": { "event": event_name },
            "headers": { "x-original": "yes" },
            "input": { "event": event_name },
        });
        let value =
            hook_value(&runtime.emit(ExtensionEvent::new(event_name, payload)).await).clone();
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
async fn quickjs_hook_return_values_apply_semantic_reductions() -> Result<()> {
    let (_root, runtime) =
        load_quickjs_fixture("quickjs-hook-transforms.mjs", &["event_hooks"]).await?;

    // Advisory emit path: the merged reduction crosses the runtime boundary
    // with the same shapes the process bridge produces.
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
        let value = hook_value(&runtime.emit(ExtensionEvent::new(event_name, payload)).await).clone();
        for (key, expected_value) in expected.as_object().expect("expected object") {
            assert_eq!(&value[key], expected_value, "event {event_name} key {key}");
        }
    }

    let tree = hook_value(
        &runtime
            .emit(ExtensionEvent::new(
                "session_before_tree",
                json!({ "preparation": { "targetId": "entry" } }),
            ))
            .await,
    )
    .clone();
    assert_eq!(tree["cancel"], false);
    assert_eq!(tree["summary"]["summary"], "fixture summary");
    assert_eq!(tree["customInstructions"], "fixture instructions");
    assert_eq!(tree["replaceInstructions"], true);
    assert_eq!(tree["label"], "fixture-label");

    // Semantic reducer path: the host applies the same reductions it applies
    // to process-hosted hook return values.
    let start = runtime
        .reduce_before_agent_start(
            json!({ "prompt": "hello", "systemPromptOptions": {} }),
            "base".to_owned(),
        )
        .await?;
    assert_eq!(start.system_prompt, "base\nfixture-system");
    assert_eq!(start.messages.len(), 1);
    assert_eq!(start.messages[0].custom_type, "fixture.before-agent");
    let context = runtime.reduce_context(Vec::new()).await?;
    assert_eq!(
        serde_json::to_value(&context)?[0]["content"][0]["text"],
        "fixture-context"
    );
    assert_eq!(
        runtime
            .reduce_provider_request(json!({ "model": "fixture" }))
            .await?["transformedByFixture"],
        true
    );
    let headers = runtime
        .reduce_provider_headers(
            [
                ("x-original".to_owned(), Some("yes".to_owned())),
                ("x-remove".to_owned(), Some("old".to_owned())),
            ]
            .into_iter()
            .collect(),
        )
        .await?;
    assert_eq!(headers.get("x-fixture"), Some(&Some("yes".to_owned())));
    assert_eq!(headers.get("x-remove"), Some(&None));

    let blocked = runtime
        .reduce_tool_call("call-1", "danger", json!({}))
        .await?;
    assert!(blocked.block);
    assert_eq!(blocked.reason.as_deref(), Some("blocked by fixture"));
    let result = runtime
        .reduce_tool_result("call-1", "read", json!({}), Vec::new(), None, true)
        .await?;
    assert!(!result.is_error);
    assert_eq!(serde_json::to_value(&result.content[0])?["text"], "fixture-result");
    assert_eq!(result.details, Some(json!({ "replaced": true })));
    assert_eq!(result.usage.expect("fixture usage").cache_read, 3);
    let message = runtime
        .reduce_message_end(Message::user_text("original", 1))
        .await?;
    assert_eq!(
        serde_json::to_value(message)?["content"][0]["text"],
        "fixture-message"
    );

    assert!(runtime
        .reduce_before_switch(json!({ "reason": "resume" }))
        .await?);
    let fork = runtime
        .reduce_before_fork(json!({ "entryId": "entry", "position": "at" }))
        .await?;
    assert!(fork.cancel && fork.skip_conversation_restore);
    assert!(runtime
        .reduce_before_compact(json!({ "reason": "manual", "willRetry": false }))
        .await?
        .cancel);
    let tree = runtime
        .reduce_before_tree(json!({ "preparation": { "targetId": "entry" } }))
        .await?;
    assert!(!tree.cancel);
    assert_eq!(tree.summary.expect("tree summary").summary, "fixture summary");
    assert_eq!(
        tree.custom_instructions.as_deref(),
        Some("fixture instructions")
    );
    assert_eq!(tree.replace_instructions, Some(true));
    assert_eq!(tree.label.as_deref(), Some("fixture-label"));
    assert_eq!(
        runtime
            .reduce_input("original".to_owned(), Vec::new(), "interactive", None)
            .await?,
        ExtensionInputReduction::Continue {
            text: "fixture-input".to_owned(),
            images: Vec::new(),
        }
    );

    runtime.shutdown().await;
    Ok(())
}

/// Producer-coverage guard for the QuickJS event allow-list.
///
/// `SUPPORTED_EVENTS` (quickjs_host.rs) gates which names `pi.on` accepts.
/// This test pins the invariant that every allow-listed name is backed by a
/// real production producer (a Rust path that emits it) — so a new
/// allow-list entry without a producer, or a producer wired to a name
/// dropped from the list, fails loudly instead of silently drifting. Names
/// without a producer are rejected at registration (see
/// [`removed_event_names_fail_registration`]) so extensions fail fast
/// instead of registering on an event that never fires.
///
/// All 32 names, each with its emitting Rust path:
/// - `trust_decision` — application.rs `resolve_project_trust_with_hooks` →
///   extensions.rs `reduce_trust_decision`
/// - `session_start` — extensions.rs `finish_reload`
/// - `session_info_changed` — session.rs `set_session_name` →
///   application.rs `session_extension_event`
/// - `session_before_switch` — application.rs `reduce_before_switch`
///   (resume/new session change paths)
/// - `session_before_fork` — application.rs `reduce_before_fork`
/// - `session_before_compact` — application.rs `reduce_before_compact`
/// - `session_compact` — application.rs `session_extension_event`
///   (CompactionEnd)
/// - `session_shutdown` — extensions.rs `prepare_reload` +
///   `shutdown_with_reason`
/// - `session_before_tree` — application.rs `reduce_before_tree`
/// - `session_tree` — application.rs `navigate_tree`
/// - `context` — application.rs `reduce_context`
/// - `before_provider_request` / `before_provider_headers` /
///   `after_provider_response` — application.rs provider stream hooks
/// - `before_agent_start` — application.rs `reduce_before_agent_start`
/// - `agent_start` / `agent_end` / `turn_start` / `turn_end` /
///   `message_start` / `message_update` / `message_end` /
///   `tool_execution_start` / `tool_execution_update` /
///   `tool_execution_end` — application.rs `agent_extension_event`
///   (pi-agent `AgentEvent`)
/// - `agent_settled` — application.rs `finish_parent_turn` /
///   `finish_todo_cycle_if_idle`
/// - `model_select` — session.rs `set_model` → application.rs
///   `session_extension_event`
/// - `thinking_level_select` — session.rs `set_thinking_level` →
///   application.rs `session_extension_event`
/// - `tool_call` / `tool_result` / `input` / `user_bash` — application.rs
///   reducers
#[test]
fn every_allow_listed_event_name_has_a_producer() {
    let allow_list: [&str; 32] = [
        "trust_decision", "session_start",
        "session_info_changed",
        "session_before_switch", "session_before_fork", "session_before_compact", "session_compact",
        "session_shutdown", "session_before_tree", "session_tree", "context",
        "before_provider_request", "before_provider_headers", "after_provider_response",
        "before_agent_start", "agent_start", "agent_end", "agent_settled", "turn_start", "turn_end",
        "message_start", "message_update", "message_end", "tool_execution_start",
        "tool_execution_update", "tool_execution_end", "model_select", "thinking_level_select",
        "tool_call", "tool_result", "user_bash", "input",
    ];
    let producer_backed: [&str; 32] = [
        "trust_decision", "session_start", "session_info_changed",
        "session_before_switch", "session_before_fork", "session_before_compact", "session_compact",
        "session_shutdown", "session_before_tree", "session_tree", "context",
        "before_provider_request", "before_provider_headers", "after_provider_response",
        "before_agent_start", "agent_start", "agent_end", "agent_settled", "turn_start", "turn_end",
        "message_start", "message_update", "message_end", "tool_execution_start",
        "tool_execution_update", "tool_execution_end", "model_select", "thinking_level_select",
        "tool_call", "tool_result", "user_bash", "input",
    ];

    let mut allow_set: std::collections::BTreeSet<&str> = allow_list.into_iter().collect();
    let producer_set: std::collections::BTreeSet<&str> = producer_backed.into_iter().collect();

    // No duplicates inside any single list.
    assert_eq!(allow_set.len(), allow_list.len(), "duplicate allow-list names");
    assert_eq!(
        producer_set.len(),
        producer_backed.len(),
        "duplicate producer-backed names"
    );
    // The allow-list is exactly the producer-backed set.
    for name in &producer_set {
        assert!(
            allow_set.remove(name),
            "producer-backed name {name:?} is missing from the allow-list"
        );
    }
    assert!(
        allow_set.is_empty(),
        "allow-list names without a producer: {allow_set:?}"
    );
}

/// Every name that was once allow-listed but has no production producer is
/// rejected at `pi.on` registration with an actionable error, so extensions
/// fail fast instead of silently never firing.
#[tokio::test]
async fn removed_event_names_fail_registration() -> Result<()> {
    for removed in [
        "project_trust",
        "resources_discover",
        "overlay_open",
        "overlay_close",
    ] {
        let root = TempDir::new()?;
        std::fs::write(
            root.path().join("index.mjs"),
            format!("export default function (pi) {{ pi.on(\"{removed}\", () => null); }}\n"),
        )?;
        let manifest = root.path().join("pi-extension.json");
        std::fs::write(
            &manifest,
            serde_json::to_vec_pretty(&json!({
                "schemaVersion": 1,
                "id": format!("quickjs-removed-{removed}"),
                "runtime": "quickjs",
                "entry": "index.mjs",
                "capabilities": ["event_hooks"]
            }))?,
        )?;
        let spec = extension_spec_from_package_resource(&resource(manifest, true))?;
        let runtime = ExtensionRuntime::process(None, quickjs_options());
        let report = runtime.load(vec![spec]).await;
        assert_eq!(report.failures.len(), 1, "{removed}: {:?}", report.failures);
        assert!(
            report.failures[0]
                .message
                .contains(&format!("unsupported extension event {removed}")),
            "{removed}: {}",
            report.failures[0].message
        );
        runtime.shutdown().await;
    }
    Ok(())
}

#[tokio::test]
async fn quickjs_event_registration_fails_closed() -> Result<()> {
    // Unknown event names are rejected by the authoritative allow-list at
    // registration time (mirrors the process bridge's supportedEvents set).
    let root = TempDir::new()?;
    std::fs::write(
        root.path().join("index.mjs"),
        "export default function (pi) { pi.on(\"not_an_event\", () => null); }\n",
    )?;
    let manifest = root.path().join("pi-extension.json");
    std::fs::write(
        &manifest,
        serde_json::to_vec_pretty(&json!({
            "schemaVersion": 1,
            "id": "quickjs-bad-event",
            "runtime": "quickjs",
            "entry": "index.mjs",
            "capabilities": ["event_hooks"]
        }))?,
    )?;
    let spec = extension_spec_from_package_resource(&resource(manifest, true))?;
    let runtime = ExtensionRuntime::process(None, quickjs_options());
    let report = runtime.load(vec![spec]).await;
    assert_eq!(report.failures.len(), 1, "{:?}", report.failures);
    assert!(
        report.failures[0].message.contains("unsupported extension event not_an_event"),
        "{}",
        report.failures[0].message
    );
    runtime.shutdown().await;

    // Event registration without the event_hooks capability is rejected
    // (mirrors the process bridge's requireCapability gate).
    let root = TempDir::new()?;
    std::fs::write(
        root.path().join("index.mjs"),
        "export default function (pi) { pi.on(\"session_start\", () => null); }\n",
    )?;
    let manifest = root.path().join("pi-extension.json");
    std::fs::write(
        &manifest,
        serde_json::to_vec_pretty(&json!({
            "schemaVersion": 1,
            "id": "quickjs-no-cap",
            "runtime": "quickjs",
            "entry": "index.mjs",
            "capabilities": ["commands"]
        }))?,
    )?;
    let spec = extension_spec_from_package_resource(&resource(manifest, true))?;
    let runtime = ExtensionRuntime::process(None, quickjs_options());
    let report = runtime.load(vec![spec]).await;
    assert_eq!(report.failures.len(), 1, "{:?}", report.failures);
    assert!(
        report.failures[0].message.contains("event_hooks"),
        "{}",
        report.failures[0].message
    );
    runtime.shutdown().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// In-process QuickJS ctx.ui (Phase 3). These tests assert the ctx.ui contract
// against the .mjs UI fixture: wire shapes, dialog cancel semantics, and the
// capability gates.
// ---------------------------------------------------------------------------

/// Every `ExtensionUiCapability` the quickjs-ui fixture exercises, granted via
/// the manifest's `uiCapabilities` field.
const QUICKJS_UI_CAPABILITIES: [&str; 14] = [
    "select", "confirm", "input", "editor", "notify", "status", "widget", "title",
    "set_editor_text", "editor_text", "working", "hidden_thinking", "theme", "tools_expanded",
];

/// Scripted UI host: records every request and auto-answers interactive
/// dialogs and queries so the QuickJS ctx.ui round trip can be asserted end to
/// end without a TUI. Specific titles exercise the cancellation and
/// slow-response paths (mirroring the TUI's dialog responses).
#[derive(Default)]
struct ScriptedUi {
    requests: Mutex<Vec<ExtensionUiRequest>>,
}

impl ScriptedUi {
    fn requests(&self) -> Vec<ExtensionUiRequest> {
        self.requests.lock().expect("scripted ui mutex").clone()
    }
}

impl ExtensionUiHost for ScriptedUi {
    fn request(
        &self,
        _context: ExtensionUiContext,
        request: ExtensionUiRequest,
        _cancellation: ExtensionCancellation,
    ) -> pi_coding::ExtensionFuture<'_, Result<ExtensionUiResponse>> {
        self.requests
            .lock()
            .expect("scripted ui mutex")
            .push(request.clone());
        Box::pin(async move {
            Ok(match request {
                ExtensionUiRequest::Select { title, options } if title == "abort" => {
                    ExtensionUiResponse::Cancelled
                }
                ExtensionUiRequest::Select { options, .. } => ExtensionUiResponse::Selected {
                    value: options.first().map(|option| option.value.clone()),
                },
                ExtensionUiRequest::Confirm { title, .. } if title == "slow" => {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    ExtensionUiResponse::Confirmed { confirmed: true }
                }
                ExtensionUiRequest::Confirm { title, .. } if title == "decline" => {
                    ExtensionUiResponse::Confirmed { confirmed: false }
                }
                ExtensionUiRequest::Confirm { .. } => {
                    ExtensionUiResponse::Confirmed { confirmed: true }
                }
                ExtensionUiRequest::Input { title, value, .. } if title == "escape" => {
                    ExtensionUiResponse::Cancelled
                }
                ExtensionUiRequest::Input { value, .. } => ExtensionUiResponse::Input {
                    value: value.or_else(|| Some("typed-value".to_owned())),
                },
                ExtensionUiRequest::Editor { title, .. } if title == "quit" => {
                    ExtensionUiResponse::Cancelled
                }
                ExtensionUiRequest::Editor { prefill, .. } => ExtensionUiResponse::Edited {
                    value: prefill.map(|prefill| format!("{prefill}!")),
                },
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
                ExtensionUiRequest::GetToolsExpanded => {
                    ExtensionUiResponse::ToolsExpanded { expanded: true }
                }
                ExtensionUiRequest::OverlayOpen { .. } => ExtensionUiResponse::OverlayOpened,
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

/// Load the quickjs-ui fixture with the full UI surface and a scripted UI
/// host; the returned runtime auto-answers every dialog. The `TempDir` keeps
/// the fixture's working directory alive for the lifetime of the runtime.
async fn load_quickjs_ui_fixture(
    ui: Arc<ScriptedUi>,
) -> Result<(TempDir, ExtensionRuntime)> {
    let root = TempDir::new()?;
    let manifest_path = quickjs_package_with_ui(
        &root,
        "quickjs-ui.mjs",
        &["commands", "ui"],
        &QUICKJS_UI_CAPABILITIES,
    )?;
    let spec = extension_spec_from_package_resource(&resource(manifest_path, true))?;
    let runtime = ExtensionRuntime::process(Some(ui), quickjs_options());
    let report = runtime.load(vec![spec]).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    Ok((root, runtime))
}

#[tokio::test]
async fn quickjs_ctx_ui_round_trips_through_the_scripted_host() -> Result<()> {
    let ui = Arc::new(ScriptedUi::default());
    let (_root, runtime) = load_quickjs_ui_fixture(ui.clone()).await?;

    // The handler awaited every interactive dialog and query; each promise
    // resolved with the host's answer.
    let result = runtime
        .invoke_command("exercise-ui", String::new(), None, None)
        .await?;
    let obj = result
        .as_object()
        .expect("exercise-ui must return an object");
    assert_eq!(obj["confirmed"], json!(true));
    assert_eq!(obj["selected"], json!("one"));
    assert_eq!(obj["typed"], json!("typed-value"));
    assert_eq!(obj["edited"], json!("prefill!"));
    assert_eq!(obj["editorText"], json!("editor-text"));
    assert_eq!(obj["themes"][0]["name"], json!("dark"));
    assert_eq!(obj["themes"][1]["path"], json!("/themes/light.json"));
    assert_eq!(obj["theme"]["name"], json!("dark"));
    assert_eq!(obj["setThemeResult"], json!({ "success": true }));
    assert_eq!(obj["missingTheme"], Value::Null);
    assert_eq!(obj["toolsExpanded"], json!(true));

    // Every request crossed the boundary in the process bridge's wire shape.
    let requests = ui.requests();
    assert!(requests.iter().any(|request| matches!(request,
        ExtensionUiRequest::Confirm { title, message } if title == "Continue?" && message == "Proceed?"
    )));
    assert!(requests.iter().any(|request| matches!(request,
        ExtensionUiRequest::Select { title, options, .. } if title == "Pick"
            && options.len() == 2
            && options[0].value == "one" && options[0].label == "One"
            && options[0].description.as_deref() == Some("first")
            && options[1].value == "two" && options[1].label == "two"
    )));
    assert!(requests.iter().any(|request| matches!(request,
        ExtensionUiRequest::Input { title, placeholder, value } if title == "Name"
            && placeholder.as_deref() == Some("hint") && value.is_none()
    )));
    assert!(requests.iter().any(|request| matches!(request,
        ExtensionUiRequest::Editor { title, prefill } if title == "Edit" && prefill.as_deref() == Some("prefill")
    )));
    assert!(requests.iter().any(|request| matches!(request,
        ExtensionUiRequest::Notify { message, level } if message == "hello from quickjs" && *level == UiNotificationLevel::Info
    )));
    assert!(requests.iter().any(|request| matches!(request,
        ExtensionUiRequest::Notify { message, level } if message == "warning from quickjs" && *level == UiNotificationLevel::Warning
    )));
    assert!(requests.iter().any(|request| matches!(request,
        ExtensionUiRequest::Status { key, text } if key == "status-key" && text.as_deref() == Some("running")
    )));
    assert!(requests.iter().any(|request| matches!(request,
        ExtensionUiRequest::Widget { key, lines, placement } if key == "widget-key"
            && lines.as_deref() == Some(&vec!["line 1".to_owned(), "line 2".to_owned()])
            && *placement == UiWidgetPlacement::BelowEditor
    )));
    assert!(requests.iter().any(|request| matches!(request,
        ExtensionUiRequest::Title { title } if title == "quickjs title"
    )));
    assert!(requests.iter().any(|request| matches!(request,
        ExtensionUiRequest::SetEditorText { text } if text == "editor text"
    )));
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
        ExtensionUiRequest::PasteToEditor { text } if text == "pasted"
    )));
    assert!(requests.iter().any(|request| matches!(request,
        ExtensionUiRequest::SetToolsExpanded { expanded } if *expanded
    )));
    assert!(requests.iter().any(|request| matches!(request,
        ExtensionUiRequest::SetTheme { name } if name == "dark"
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
async fn quickjs_ui_cancellation_maps_cancelled_dialogs() -> Result<()> {
    let ui = Arc::new(ScriptedUi::default());
    let (_root, runtime) = load_quickjs_ui_fixture(ui.clone()).await?;

    // A cancelled dialog maps to undefined for select/input/editor (Null
    // across the JSON boundary) and false for confirm.
    assert_eq!(
        runtime
            .invoke_command("confirm-no", String::new(), None, None)
            .await?,
        json!(false)
    );
    assert_eq!(
        runtime
            .invoke_command("select-cancel", String::new(), None, None)
            .await?,
        Value::Null
    );
    assert_eq!(
        runtime
            .invoke_command("input-cancel", String::new(), None, None)
            .await?,
        Value::Null
    );
    assert_eq!(
        runtime
            .invoke_command("editor-cancel", String::new(), None, None)
            .await?,
        Value::Null
    );

    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn quickjs_ui_request_timeout_rejects_the_promise() -> Result<()> {
    let ui = Arc::new(ScriptedUi::default());
    let (_root, runtime) = load_quickjs_ui_fixture(ui.clone()).await?;

    // { timeout: 1 } must fail the UI promise host-side ("UI request timed
    // out") even though the scripted host would answer after 250ms.
    let error = runtime
        .invoke_command("confirm-timeout", String::new(), None, None)
        .await
        .expect_err("a per-request timeout must fail the UI promise");
    let message = error.to_string();
    assert!(
        message.contains("UI request timed out"),
        "expected the host-side request timeout, got: {message}",
    );

    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn quickjs_ui_rejects_when_no_ui_host_is_bound() -> Result<()> {
    let root = TempDir::new()?;
    let manifest_path = quickjs_package_with_ui(
        &root,
        "quickjs-ui.mjs",
        &["commands", "ui"],
        &QUICKJS_UI_CAPABILITIES,
    )?;
    let spec = extension_spec_from_package_resource(&resource(manifest_path, true))?;
    // No UI adapter bound: the interactive gate must reject through the host
    // round trip with an actionable message.
    let runtime = ExtensionRuntime::process(None, quickjs_options());
    let report = runtime.load(vec![spec]).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    let error = runtime
        .invoke_command("confirm-no", String::new(), None, None)
        .await
        .expect_err("ctx.ui.confirm must reject when no UI host is bound");
    let message = error.to_string();
    assert!(
        message.contains("no extension UI adapter") || message.contains("ui_unavailable"),
        "expected a no-UI-adapter rejection, got: {message}",
    );
    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn quickjs_ui_rejects_ungranted_ui_capabilities() -> Result<()> {
    let root = TempDir::new()?;
    let manifest_path = quickjs_package_with_ui(&root, "quickjs-ui.mjs", &["commands", "ui"], &[])?;
    let spec = extension_spec_from_package_resource(&resource(manifest_path, true))?;
    let ui = Arc::new(ScriptedUi::default());
    let runtime = ExtensionRuntime::process(Some(ui.clone()), quickjs_options());
    let report = runtime.load(vec![spec]).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    let error = runtime
        .invoke_command("confirm-no", String::new(), None, None)
        .await
        .expect_err("ctx.ui.confirm must reject without the Confirm UI capability");
    let message = error.to_string();
    assert!(
        message.contains("Confirm") && message.contains("not granted"),
        "expected a permission-denied rejection for the ungranted UI capability, got: {message}",
    );
    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn quickjs_ui_requires_the_ui_capability() -> Result<()> {
    let root = TempDir::new()?;
    let manifest_path = quickjs_package(&root, "quickjs-ui.mjs", &["commands"])?;
    let spec = extension_spec_from_package_resource(&resource(manifest_path, true))?;
    let ui = Arc::new(ScriptedUi::default());
    let runtime = ExtensionRuntime::process(Some(ui.clone()), quickjs_options());
    let report = runtime.load(vec![spec]).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    // Without the ui capability the request bridge throws synchronously,
    // mirroring the process bridge's requireCapability("ui", "UI action").
    let error = runtime
        .invoke_command("confirm-no", String::new(), None, None)
        .await
        .expect_err("ctx.ui.confirm must require the ui capability");
    let message = error.to_string();
    assert!(
        message.contains("requires the ui capability"),
        "expected a capability-gate rejection, got: {message}",
    );
    runtime.shutdown().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Process-extension sandbox (`settings.sandbox`): process extensions run
// inside the filesystem sandbox when the sandbox is enabled. The live
// confinement test exercises the real `unshare` wrapper and is `#[ignore]`d by
// default (explicit unsupported status, never a fake pass); the fail-closed
// validation test always runs because it needs no kernel support.
// ---------------------------------------------------------------------------

/// Returns `Ok(())` when the host can actually run the sandbox: `unshare`
/// exists and an unprivileged user namespace with mounts can be created. Any
/// missing prerequisite is an explicit `Err(reason)`; live tests refuse to
/// fake-pass on it.
fn sandbox_usable() -> Result<(), String> {
    if std::env::var_os("PI_TEST_SANDBOX_FORCE_UNSUPPORTED").is_some() {
        return Err("PI_TEST_SANDBOX_FORCE_UNSUPPORTED=1 (deterministic test seam)".to_owned());
    }
    let unshare = std::process::Command::new("unshare").arg("--version").output();
    match unshare {
        Ok(output) if output.status.success() => {}
        Ok(output) => return Err(format!("unshare --version failed: {output:?}")),
        Err(error) => return Err(format!("unshare is not installed: {error}")),
    }
    let probe = std::process::Command::new("unshare")
        .args(["--user", "--map-root-user", "--mount", "--pid", "--fork", "--net", "--mount-proc"])
        .arg("sh")
        .arg("-c")
        .arg("exit 0")
        .status();
    match probe {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!(
            "unprivileged user namespace probe failed ({status}): kernel.unprivileged_userns_clone or an AppArmor profile likely blocks it"
        )),
        Err(error) => Err(format!("unprivileged user namespace probe errored: {error}")),
    }
}

/// Writes a minimal process-extension fixture executable: a POSIX shell script
/// speaking the extension JSONL protocol (reply to the host hello, then answer
/// every host request with a success response). With `check_passwd` it first
/// fails closed (exit 3, with the reason on stderr) when `/etc/passwd` is not
/// readable — which is exactly what happens inside the sandbox, because `/etc`
/// is not among the allowed paths. A plain script keeps the test harness's
/// output off the protocol pipe (the JSONL protocol rejects foreign lines).
fn write_process_extension_fixture(root: &TempDir, check_passwd: bool) -> Result<std::path::PathBuf> {
    let fixture = root.path().join("sandbox-fixture.sh");
    let body = r#"#!/bin/sh
# Minimal process-extension protocol fixture (JSONL over stdio).
if [ "$PI_EXTENSION_SANDBOX_FIXTURE_CHECK" = "passwd" ]; then
  if ! cat /etc/passwd >/dev/null 2>&1; then
    echo "sandbox fixture: /etc/passwd is not readable inside the sandbox" >&2
    sleep 0.1
    exit 3
  fi
fi
IFS= read -r _host_hello || exit 4
echo '{"type":"hello","protocolVersion":1,"manifest":{"id":"sandbox-fixture","name":"sandbox fixture","version":"1.0.0","capabilities":[],"uiCapabilities":[]}}'
while IFS= read -r line; do
  case "$line" in
    *'"type":"request"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
      [ -n "$id" ] || continue
      printf '{"type":"response","id":"%s","result":{"status":"success","value":null}}\n' "$id"
      ;;
    *'"type":"shutdown"'*) break ;;
  esac
done
exit 0
"#;
    std::fs::write(&fixture, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&fixture)?.permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&fixture, permissions)?;
    }
    Ok(fixture)
}

/// A process-extension spec backed by the shell-script fixture. `check_passwd`
/// makes the fixture fail closed when `/etc/passwd` is not readable.
fn process_extension_spec(root: &TempDir, check_passwd: bool) -> Result<ExtensionSpec> {
    let fixture = write_process_extension_fixture(root, check_passwd)?;
    let mut environment = std::collections::BTreeMap::new();
    if check_passwd {
        environment.insert("PI_EXTENSION_SANDBOX_FIXTURE_CHECK".to_owned(), "passwd".to_owned());
    }
    let mut spec = ExtensionSpec::new(
        "sandbox-fixture",
        fixture,
        root.path(),
        ExtensionOrigin::Project,
        true,
        ExtensionPermissionSet {
            capabilities: BTreeSet::new(),
            ui_capabilities: BTreeSet::new(),
        },
    );
    spec.environment = environment;
    Ok(spec)
}

/// A live `settings.sandbox` resolver confining process extensions to `cwd`,
/// with optional denied paths.
fn sandbox_extension_resolver(
    cwd: &std::path::Path,
    denied: Vec<std::path::PathBuf>,
) -> pi_coding::SandboxConfigFn {
    let config = pi_coding::SandboxConfig {
        enabled: true,
        network: false,
        allowed_paths: vec![cwd.to_path_buf()],
        denied_paths: denied,
        read_only: false,
    };
    Arc::new(move || Some(config.clone()))
}

/// A process extension that tries to read `/etc/passwd` fails to load under
/// `sandbox.enabled` (the file is not visible in the confined root), while the
/// same extension loads fine without the sandbox and a non-passwd-checking
/// extension loads fine inside it — proving the sandbox itself does not break
/// process extensions.
#[ignore = "live confinement test: requires Linux unprivileged user namespaces (run with --include-ignored on a supported kernel)"]
#[tokio::test]
async fn sandboxed_process_extension_cannot_read_etc_passwd() -> Result<()> {
    sandbox_usable().expect("live sandbox test refuses to fake-pass on an unsupported kernel");
    let root = TempDir::new()?;
    let check_spec = process_extension_spec(&root, true)?;
    let plain_spec = process_extension_spec(&root, false)?;

    // Control: without the sandbox the passwd-checking extension loads.
    let plain_runtime = ExtensionRuntime::process(None, ExtensionRuntimeOptions::default());
    let report = plain_runtime.load(vec![check_spec.clone()]).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    plain_runtime.shutdown().await;

    // Sandboxed: the same extension fails closed — /etc/passwd is invisible.
    let sandboxed = ExtensionRuntime::process(None, ExtensionRuntimeOptions::default());
    sandboxed.set_process_sandbox(Some(sandbox_extension_resolver(root.path(), Vec::new())));
    let report = sandboxed.load(vec![check_spec]).await;
    assert_eq!(report.failures.len(), 1, "{:?}", report.failures);
    let message = &report.failures[0].message;
    assert!(
        message.contains("exited before its handshake"),
        "expected the extension process to fail its handshake: {message}"
    );
    assert!(
        message.contains("/etc/passwd is not readable"),
        "the failure must surface the fixture's actionable stderr: {message}"
    );
    sandboxed.shutdown().await;

    // Sandboxed + a fixture that does not touch /etc: loads fine.
    let healthy = ExtensionRuntime::process(None, ExtensionRuntimeOptions::default());
    healthy.set_process_sandbox(Some(sandbox_extension_resolver(root.path(), Vec::new())));
    let report = healthy.load(vec![plain_spec]).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    healthy.shutdown().await;
    Ok(())
}

/// Fail-closed validation: an extension whose working directory would be
/// invisible inside the sandbox is refused before the child starts, with the
/// actionable `sandbox.deniedPaths` error. This contract needs no kernel
/// support (validation runs before any namespace is created), so it always
/// runs.
#[tokio::test]
async fn sandboxed_process_extension_denied_cwd_fails_closed() -> Result<()> {
    let root = TempDir::new()?;
    let spec = process_extension_spec(&root, false)?;
    let runtime = ExtensionRuntime::process(None, ExtensionRuntimeOptions::default());
    runtime.set_process_sandbox(Some(sandbox_extension_resolver(
        root.path(),
        vec![root.path().to_path_buf()],
    )));
    let report = runtime.load(vec![spec]).await;
    assert_eq!(report.failures.len(), 1, "{:?}", report.failures);
    assert!(
        report.failures[0].message.contains("sandbox.deniedPaths"),
        "expected the fail-closed denied-paths error: {:?}",
        report.failures[0].message
    );
    runtime.shutdown().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// registerProvider / unregisterProvider (extension-registered providers)
// ---------------------------------------------------------------------------

/// Serializes the provider tests: every provider fixture registers into the
/// shared (process-global) pi-ai provider registry, so tests sharing fixture
/// apis must not race each other's registration/unregistration.
static PROVIDER_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Like `quickjs_package`, but with a caller-chosen extension id so each test
/// owns a distinct pi-ai registry source id.
fn provider_spec(
    root: &TempDir,
    id: &str,
    fixture: &str,
    capabilities: &[&str],
) -> Result<ExtensionSpec> {
    std::fs::copy(fixture_dir().join(fixture), root.path().join(fixture))?;
    let manifest = root.path().join("pi-extension.json");
    std::fs::write(
        &manifest,
        serde_json::to_vec_pretty(&json!({
            "schemaVersion": 1,
            "id": id,
            "runtime": "quickjs",
            "entry": fixture,
            "capabilities": capabilities,
        }))?,
    )?;
    extension_spec_from_package_resource(&resource(manifest, true))
}

/// A quickjs provider fixture registering the SAME extension id, provider id,
/// and api as every other isolation fixture, but yielding a distinguishable
/// `response-{marker}` text. Two runtimes loaded from these specs differ only
/// in their per-runtime pi-ai namespace — exactly the collision this test
/// suite guards against.
fn isolation_provider_spec(root: &TempDir, id: &str, marker: &str) -> Result<ExtensionSpec> {
    let entry = format!("isolation-provider-{marker}.mjs");
    std::fs::write(
        root.path().join(&entry),
        format!(
            "export default function (pi) {{\n\
             \x20 pi.registerProvider({{\n\
             \x20   id: \"isolation-llm\",\n\
             \x20   api: \"isolation-shared-api\",\n\
             \x20   stream: async function* () {{\n\
             \x20     yield {{ type: \"start\" }};\n\
             \x20     yield {{ type: \"text\", text: \"response-{marker}\" }};\n\
             \x20     yield {{ type: \"done\", stopReason: \"stop\" }};\n\
             \x20   }},\n\
             \x20 }});\n\
             }}\n"
        ),
    )?;
    let manifest = root.path().join("pi-extension.json");
    std::fs::write(
        &manifest,
        serde_json::to_vec_pretty(&json!({
            "schemaVersion": 1,
            "id": id,
            "runtime": "quickjs",
            "entry": entry,
            "capabilities": ["provider"],
        }))?,
    )?;
    extension_spec_from_package_resource(&resource(manifest, true))
}

/// Run a session's own stream dispatch (the namespace-aware wrapper stored in
/// `child_session_options_snapshot`) and return the terminal message text.
async fn session_stream_text(
    session: &Session,
    model: Model,
) -> pi_ai::AssistantMessage {
    let snapshot = session.child_session_options_snapshot();
    let stream = (snapshot.stream_fn)(model, Context::default(), SimpleStreamOptions::default()).await;
    stream.result().await.expect("session stream must terminate")
}

fn provider_model(api: &str, provider: &str) -> Model {
    let mut model = Model::default();
    model.id = "fixture-model".into();
    model.name = "Fixture Model".into();
    model.api = api.into();
    model.provider = provider.into();
    model.base_url = "http://localhost:0".into();
    model
}

async fn collect_provider_events(stream: &AssistantMessageEventStream) -> Vec<AssistantMessageEvent> {
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    events
}

#[tokio::test]
async fn quickjs_provider_registers_and_resolves_by_api() -> Result<()> {
    let _guard = PROVIDER_TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let root = TempDir::new()?;
    let spec = provider_spec(&root, "provider-llm", "quickjs-provider.mjs", &["provider"])?;
    let runtime = ExtensionRuntime::process(None, quickjs_options());
    let report = runtime.load(vec![spec]).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);

    let providers = runtime.providers();
    assert_eq!(providers.len(), 5, "the fixture registers five providers");
    let llm = providers
        .iter()
        .find(|provider| provider.id == "fixture-llm")
        .expect("fixture-llm descriptor");
    assert_eq!(llm.api, "fixture-llm-api");
    assert_eq!(llm.label.as_deref(), Some("Fixture LLM"));
    assert_eq!(llm.capabilities, vec!["streaming".to_owned()]);

    // Resolution: a model carrying the extension's api routes to the
    // extension's stream through the shared provider registry.
    assert!(
        get_api_provider("fixture-llm-api").is_some(),
        "the extension provider must be resolvable by its api"
    );
    let model = provider_model("fixture-llm-api", "fixture-llm");
    let stream = stream_simple(model, Context::default(), SimpleStreamOptions::default()).await;
    let events = collect_provider_events(&stream).await;
    assert!(
        events.iter().any(|event| matches!(
            event,
            AssistantMessageEvent::Done { reason: StopReason::Stop, .. }
        )),
        "the routed stream must complete with Done"
    );
    let result = stream.result().await.expect("terminal message");
    assert_eq!(result.api, "fixture-llm-api");
    assert_eq!(result.provider, "fixture-llm");

    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn quickjs_provider_streams_scripted_events_end_to_end() -> Result<()> {
    let _guard = PROVIDER_TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let root = TempDir::new()?;
    let spec = provider_spec(&root, "provider-e2e", "quickjs-provider.mjs", &["provider"])?;
    let runtime = ExtensionRuntime::process(None, quickjs_options());
    let report = runtime.load(vec![spec]).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);

    let model = provider_model("fixture-llm-api", "fixture-llm");
    let mut context = Context::default();
    context.messages = vec![
        Message::User(UserMessage {
            content: vec![ContentBlock::text("hi")],
            timestamp: 1,
        }),
        Message::User(UserMessage {
            content: vec![ContentBlock::text("again")],
            timestamp: 2,
        }),
    ];
    let mut options = SimpleStreamOptions::default();
    options.stream.session_id = Some("sess-1".into());
    options.stream.temperature = Some(0.5);
    let stream = stream_simple(model, context, options).await;
    let events = collect_provider_events(&stream).await;
    let result = stream.result().await.expect("terminal message");

    // The event sequence must mirror the scripted JS stream: Start, thinking,
    // text ("hello" + " world" delta), tool call, echo text, Done.
    let mut seen = Vec::new();
    for event in &events {
        match event {
            AssistantMessageEvent::Start { .. } => seen.push("start"),
            AssistantMessageEvent::ThinkingEnd { content, .. } if content == "hmm" => {
                seen.push("thinking")
            }
            AssistantMessageEvent::TextDelta { delta, .. } if delta == "hello" => {
                seen.push("text")
            }
            AssistantMessageEvent::TextDelta { delta, .. } if delta == " world" => {
                seen.push("delta")
            }
            AssistantMessageEvent::ToolCallEnd { tool_call, .. } if tool_call.name == "lookup" => {
                seen.push("tool_call")
            }
            AssistantMessageEvent::Done { .. } => seen.push("done"),
            _ => {}
        }
    }
    assert_eq!(
        seen,
        vec!["start", "thinking", "text", "delta", "tool_call", "done"],
        "the translated event stream must preserve the scripted order"
    );

    assert_eq!(result.stop_reason, StopReason::Stop);
    // Blocks: thinking, text ("hello world"), tool call, echo text.
    assert_eq!(result.content.len(), 4);
    assert_eq!(result.content[1], ContentBlock::text("hello world"));
    assert!(matches!(
        result.content[2],
        ContentBlock::ToolCall(ref call)
            if call.name == "lookup" && call.arguments["q"] == "rust"
    ));
    // The (sessionId, messages, options) arguments must reach the JS stream.
    let echo = match &result.content[3] {
        ContentBlock::Text { text, .. } => text,
        _ => panic!("the last block must be the echo text"),
    };
    assert!(echo.contains("session=sess-1"), "session id must reach JS: {echo}");
    assert!(echo.contains("messages=2"), "messages must reach JS: {echo}");

    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn quickjs_provider_accepts_a_sync_iterable() -> Result<()> {
    let _guard = PROVIDER_TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let root = TempDir::new()?;
    let spec = provider_spec(&root, "provider-sync", "quickjs-provider.mjs", &["provider"])?;
    let runtime = ExtensionRuntime::process(None, quickjs_options());
    let report = runtime.load(vec![spec]).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);

    let model = provider_model("fixture-sync-api", "fixture-sync");
    let stream = stream_simple(model, Context::default(), SimpleStreamOptions::default()).await;
    let events = collect_provider_events(&stream).await;
    assert!(
        events.iter().any(|event| matches!(
            event,
            AssistantMessageEvent::TextEnd { content, .. } if content == "sync"
        )),
        "the sync iterable's text event must stream through: {events:?}"
    );
    let result = stream.result().await.expect("terminal message");
    assert_eq!(result.text(), "sync");

    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn quickjs_provider_js_error_surfaces_as_typed_stream_error() -> Result<()> {
    let _guard = PROVIDER_TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let root = TempDir::new()?;
    let spec = provider_spec(&root, "provider-failing", "quickjs-provider.mjs", &["provider"])?;
    let runtime = ExtensionRuntime::process(None, quickjs_options());
    let report = runtime.load(vec![spec]).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);

    let model = provider_model("fixture-failing-api", "fixture-failing");
    let stream = stream_simple(model, Context::default(), SimpleStreamOptions::default()).await;
    let events = collect_provider_events(&stream).await;
    let terminal = events
        .iter()
        .find_map(AssistantMessageEvent::terminal_message)
        .expect("the failing stream must still produce a terminal message");
    assert_eq!(terminal.stop_reason, StopReason::Error, "{terminal:?}");
    let error = terminal
        .error_message
        .as_deref()
        .expect("the failure must carry an error message");
    assert!(error.contains("boom"), "the JS error must surface: {error}");
    assert!(
        !error.contains("abc123"),
        "secret-looking text must be redacted: {error}"
    );
    assert_eq!(
        stream.result().await.map(|m| m.stop_reason),
        Some(StopReason::Error)
    );

    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn quickjs_provider_error_event_is_a_typed_stream_error() -> Result<()> {
    let _guard = PROVIDER_TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let root = TempDir::new()?;
    let spec = provider_spec(
        &root,
        "provider-error-event",
        "quickjs-provider.mjs",
        &["provider"],
    )?;
    let runtime = ExtensionRuntime::process(None, quickjs_options());
    let report = runtime.load(vec![spec]).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);

    let model = provider_model("fixture-error-event-api", "fixture-error-event");
    let stream = stream_simple(model, Context::default(), SimpleStreamOptions::default()).await;
    let events = collect_provider_events(&stream).await;
    let terminal = events
        .iter()
        .find_map(AssistantMessageEvent::terminal_message)
        .expect("the error-event stream must still produce a terminal message");
    assert_eq!(terminal.stop_reason, StopReason::Error);
    assert_eq!(
        terminal.error_message.as_deref(),
        Some("provider says no"),
        "the JS error event text must surface verbatim"
    );

    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn quickjs_provider_non_iterable_result_fails_actionably() -> Result<()> {
    let _guard = PROVIDER_TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let root = TempDir::new()?;
    let spec = provider_spec(
        &root,
        "provider-not-iterable",
        "quickjs-provider.mjs",
        &["provider"],
    )?;
    let runtime = ExtensionRuntime::process(None, quickjs_options());
    let report = runtime.load(vec![spec]).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);

    let model = provider_model("fixture-not-iterable-api", "fixture-not-iterable");
    let stream = stream_simple(model, Context::default(), SimpleStreamOptions::default()).await;
    let events = collect_provider_events(&stream).await;
    let terminal = events
        .iter()
        .find_map(AssistantMessageEvent::terminal_message)
        .expect("the non-iterable stream must still produce a terminal message");
    assert_eq!(terminal.stop_reason, StopReason::Error);
    assert!(
        terminal
            .error_message
            .as_deref()
            .is_some_and(|error| error.contains("must return an async iterator")),
        "the driver failure must be actionable: {:?}",
        terminal.error_message
    );

    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn quickjs_provider_unregister_removes_and_reregister_replaces() -> Result<()> {
    let _guard = PROVIDER_TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let root = TempDir::new()?;
    let spec = provider_spec(
        &root,
        "provider-unregister",
        "quickjs-provider-unregister.mjs",
        &["provider"],
    )?;
    let runtime = ExtensionRuntime::process(None, quickjs_options());
    let report = runtime.load(vec![spec]).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);

    // The unregistered provider is gone; the re-registered one replaced its
    // api (last registration wins within the load phase).
    let providers = runtime.providers();
    assert_eq!(providers.len(), 1, "{providers:?}");
    assert_eq!(providers[0].id, "replaced");
    assert_eq!(providers[0].api, "replaced-api-v2");
    assert!(get_api_provider("gone-api").is_none());
    assert!(get_api_provider("replaced-api-v1").is_none());
    assert!(get_api_provider("replaced-api-v2").is_some());

    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn quickjs_provider_unregistered_api_resolution_fails_actionably() -> Result<()> {
    let _guard = PROVIDER_TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let root = TempDir::new()?;
    let spec = provider_spec(
        &root,
        "provider-unresolved",
        "quickjs-provider-unregister.mjs",
        &["provider"],
    )?;
    let runtime = ExtensionRuntime::process(None, quickjs_options());
    let report = runtime.load(vec![spec]).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);

    // A model configured with the unregistered api resolves to the standard
    // actionable error stream instead of the extension.
    let model = provider_model("gone-api", "gone");
    let stream = stream_simple(model, Context::default(), SimpleStreamOptions::default()).await;
    let events = collect_provider_events(&stream).await;
    let terminal = events
        .iter()
        .find_map(AssistantMessageEvent::terminal_message)
        .expect("unresolved apis still yield a terminal error message");
    assert_eq!(terminal.stop_reason, StopReason::Error);
    assert!(
        terminal.error_message.as_deref().is_some_and(|error| {
            error.contains("No API provider registered for api: gone-api")
        }),
        "resolution must fail actionably: {:?}",
        terminal.error_message
    );

    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn quickjs_provider_invalid_registrations_fail_at_load() -> Result<()> {
    let _guard = PROVIDER_TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let cases = [
        (
            "provider-invalid-id",
            "provider-invalid-id.mjs",
            "provider id",
        ),
        (
            "provider-no-stream",
            "provider-no-stream.mjs",
            "registerProvider requires a stream function",
        ),
        (
            "provider-duplicate",
            "provider-duplicate.mjs",
            "duplicate provider",
        ),
        (
            "provider-unregister-unknown",
            "provider-unregister-unknown.mjs",
            "cannot unregister unknown provider",
        ),
    ];
    for (id, fixture, expected) in cases {
        let root = TempDir::new()?;
        let spec = provider_spec(&root, id, fixture, &["provider"])?;
        let runtime = ExtensionRuntime::process(None, quickjs_options());
        let report = runtime.load(vec![spec]).await;
        assert_eq!(report.failures.len(), 1, "{id} must fail at load");
        let message = report.failures[0].message.to_ascii_lowercase();
        assert!(
            message.contains(&expected.to_ascii_lowercase()),
            "unexpected {id} failure: {}",
            report.failures[0].message
        );
        assert!(runtime.providers().is_empty());
        runtime.shutdown().await;
    }
    Ok(())
}

#[tokio::test]
async fn quickjs_provider_registration_is_load_phase_only() -> Result<()> {
    let _guard = PROVIDER_TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let root = TempDir::new()?;
    let spec = provider_spec(
        &root,
        "provider-runtime-register",
        "provider-runtime-register.mjs",
        &["provider", "event_hooks"],
    )?;
    let runtime = ExtensionRuntime::process(None, quickjs_options());
    let report = runtime.load(vec![spec]).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert!(runtime.providers().is_empty(), "no provider registered at load");

    // A runtime hook calling registerProvider must be rejected: the hook
    // fails with the load-phase gate and the provider never becomes
    // resolvable.
    let outcomes = runtime
        .emit(ExtensionEvent::new("session_start", json!({ "reason": "test" })))
        .await;
    assert_eq!(outcomes.len(), 1, "{outcomes:?}");
    assert!(
        outcomes[0]
            .result
            .as_ref()
            .is_err_and(|error| error.contains("load phase")),
        "the hook must fail with the load-phase gate: {:?}",
        outcomes[0].result
    );
    assert!(runtime.providers().is_empty());
    assert!(get_api_provider("late-api").is_none());

    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn quickjs_provider_reload_replaces_the_previous_generation() -> Result<()> {
    let _guard = PROVIDER_TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let root = TempDir::new()?;
    let first = provider_spec(&root, "provider-reload", "quickjs-provider.mjs", &["provider"])?;
    let runtime = ExtensionRuntime::process(None, quickjs_options());
    let report = runtime.load(vec![first]).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert!(get_api_provider("fixture-llm-api").is_some());

    // Reload with a different extension: the old generation's providers must
    // be unregistered and the new one's registered.
    let second = provider_spec(
        &root,
        "provider-reload",
        "provider-registration.mjs",
        &["provider"],
    )?;
    let report = runtime.load(vec![second]).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert!(
        get_api_provider("fixture-llm-api").is_none(),
        "retired provider must be unregistered"
    );
    assert!(
        get_api_provider("provider-registration-api").is_some(),
        "the new generation's provider must resolve"
    );
    let providers = runtime.providers();
    assert_eq!(providers.len(), 1);
    assert_eq!(providers[0].id, "provider-registration");

    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn quickjs_provider_shutdown_unregisters_resolution() -> Result<()> {
    let _guard = PROVIDER_TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let root = TempDir::new()?;
    let spec = provider_spec(&root, "provider-shutdown", "quickjs-provider.mjs", &["provider"])?;
    let runtime = ExtensionRuntime::process(None, quickjs_options());
    let report = runtime.load(vec![spec]).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert!(get_api_provider("fixture-llm-api").is_some());
    runtime.shutdown().await;
    assert!(
        get_api_provider("fixture-llm-api").is_none(),
        "shutdown must drop the runtime's provider entries"
    );
    Ok(())
}

#[tokio::test]
async fn concurrent_runtimes_same_api_dispatch_isolation() -> Result<()> {
    let _guard = PROVIDER_TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    // Two runtimes loaded from the SAME extension id and registering the SAME
    // api: only their per-runtime pi-ai namespace differs.
    let root_a = TempDir::new()?;
    let root_b = TempDir::new()?;
    let spec_a = isolation_provider_spec(&root_a, "isolation-shared", "alpha")?;
    let spec_b = isolation_provider_spec(&root_b, "isolation-shared", "beta")?;

    let runtime_a = ExtensionRuntime::process(None, quickjs_options());
    let report_a = runtime_a.load(vec![spec_a]).await;
    assert!(report_a.failures.is_empty(), "{:?}", report_a.failures);
    let runtime_b = ExtensionRuntime::process(None, quickjs_options());
    let report_b = runtime_b.load(vec![spec_b]).await;
    assert!(report_b.failures.is_empty(), "{:?}", report_b.failures);

    let api = "isolation-shared-api";
    assert!(is_extension_provider(api), "the api must be extension-owned");
    // While two runtimes own the api, an unscoped lookup must fail closed —
    // never an arbitrary pick between the runtimes.
    assert!(
        get_api_provider(api).is_none(),
        "ambiguous unscoped lookup must fail closed"
    );

    let model = provider_model(api, "isolation-shared");
    let cwd = tempfile::tempdir().expect("cwd");
    let build_session = |name: &str| {
        Session::new(SessionOptions {
            model: model.clone(),
            cwd: cwd.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: "extension".into(),
            compaction: None,
            stream_options: Default::default(),
            tools: None,
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
        })
        .unwrap_or_else(|error| panic!("build session {name}: {error}"))
    };
    let session_a = build_session("a");
    session_a.set_provider_namespace(Some(runtime_a.provider_namespace().to_owned()));
    let session_b = build_session("b");
    session_b.set_provider_namespace(Some(runtime_b.provider_namespace().to_owned()));

    // Concurrent lookups return each session's own runtime response.
    let (result_a, result_b) = tokio::join!(
        session_stream_text(&session_a, model.clone()),
        session_stream_text(&session_b, model.clone()),
    );
    assert!(
        result_a.text().contains("response-alpha"),
        "session a must stream its own runtime: {}",
        result_a.text()
    );
    assert!(
        result_b.text().contains("response-beta"),
        "session b must stream its own runtime: {}",
        result_b.text()
    );
    assert!(!result_a.text().contains("beta"));
    assert!(!result_b.text().contains("alpha"));

    // Shutting down A leaves B fully functional and never routes B through A.
    runtime_a.shutdown().await;
    let result_b = session_stream_text(&session_b, model.clone()).await;
    assert!(
        result_b.text().contains("response-beta"),
        "b must survive a's shutdown: {}",
        result_b.text()
    );
    // A's session now fails closed with a contextual error — it must not fall
    // through to B's registration.
    let result_a = session_stream_text(&session_a, model.clone()).await;
    assert_eq!(result_a.stop_reason, StopReason::Error);
    let message = result_a.error_message.as_deref().unwrap_or_default();
    assert!(
        message.contains(api),
        "the fail-closed error must name the api: {message}"
    );
    assert!(
        message.contains("extension runtime"),
        "the fail-closed error must be contextual: {message}"
    );

    // Unscoped resolution is unambiguous again and points at the survivor.
    let survivor = get_api_provider(api).expect("single owner resolves after shutdown");
    let stream = (survivor.stream_simple)(
        model.clone(),
        Context::default(),
        SimpleStreamOptions::default(),
    )
    .await;
    assert!(
        stream
            .result()
            .await
            .expect("terminal message")
            .text()
            .contains("response-beta")
    );

    runtime_b.shutdown().await;
    assert!(get_api_provider(api).is_none(), "shutdown drops the survivor");
    Ok(())
}

// ---------------------------------------------------------------------------
// Extension-rendered overlays (SAFE surface)
// ---------------------------------------------------------------------------

/// Load a quickjs fixture that registers an overlay and grants the `overlays`
/// capability plus the `overlay` UI capability.
async fn load_quickjs_overlay_fixture(
    source: &str,
    ui: Option<Arc<ScriptedUi>>,
) -> Result<(TempDir, ExtensionRuntime)> {
    let root = TempDir::new()?;
    std::fs::write(root.path().join("overlay.mjs"), source)?;
    let manifest = root.path().join("pi-extension.json");
    std::fs::write(
        &manifest,
        serde_json::to_vec_pretty(&json!({
            "schemaVersion": 1,
            "id": "quickjs-test",
            "runtime": "quickjs",
            "entry": "overlay.mjs",
            "capabilities": ["overlays", "ui", "event_hooks"],
            "uiCapabilities": ["overlay"],
        }))?,
    )?;
    let spec = extension_spec_from_package_resource(&resource(manifest, true))?;
    let runtime = ExtensionRuntime::process(
        ui.map(|ui| -> Arc<dyn ExtensionUiHost> { ui }),
        quickjs_options(),
    );
    let report = runtime.load(vec![spec]).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    Ok((root, runtime))
}

fn overlay_fixture() -> String {
    let token = ["gh", "p_", "ABCDEFGHIJKLMNOPQRSTUVWXYZ", "abcdefghij"].concat();
    [
        r#"
export default function (pi) {
  pi.registerOverlay({
    id: "chat",
    title: "Side Chat",
    render: (ctx) => [
      "hello from overlay",
      { text: "styled row", style: "accent" },
      { text: "secret "#,
        token.as_str(),
        r#"", style: "error" },
    ],
  });
}
"#,
    ]
    .concat()
}

#[tokio::test]
async fn quickjs_register_overlay_is_listed_and_renders_sanitized_rows() -> Result<()> {
    let source = overlay_fixture();
    let (_root, runtime) = load_quickjs_overlay_fixture(&source, None).await?;

    let overlays = runtime.overlays();
    assert_eq!(overlays.len(), 1);
    assert_eq!(overlays[0].id, "chat");
    assert_eq!(overlays[0].title, "Side Chat");

    let output = runtime.invoke_overlay_render("chat").await?;
    let rows = &output.rows;
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0], pi_coding::OverlayRow::Plain("hello from overlay".to_owned()));
    assert_eq!(
        rows[1],
        pi_coding::OverlayRow::Styled {
            text: "styled row".to_owned(),
            style: Some("accent".to_owned()),
        }
    );
    // Secret-shaped text is redacted before it becomes displayable.
    let redacted = rows[2].text();
    let token_prefix = ["gh", "p_"].concat();
    assert!(!redacted.contains(token_prefix.as_str()), "secret must be redacted: {redacted}");
    assert!(redacted.contains("[REDACTED]"), "{redacted}");
    // The fixture returns a bare rows array; the interactive output shape
    // (`{ rows, input? }`) is normalized host-side, so `input` stays absent.
    assert!(output.input.is_none(), "array render has no input section");

    // Unknown overlay ids fail actionably.
    let error = runtime
        .invoke_overlay_render("missing")
        .await
        .expect_err("unknown overlay must fail");
    assert!(error.to_string().contains("unknown extension overlay"), "{error:#}");
    runtime.shutdown().await;
    Ok(())
}

const INTERACTIVE_OVERLAY_FIXTURE: &str = r#"
export default function (pi) {
  pi.registerOverlay({
    id: "chat",
    title: "Side Chat",
    input: { placeholder: "Ask the side agent…", multiline: false },
    render: (ctx) => ({
      rows: ["hello from overlay"],
      input: { value: "initial draft" },
    }),
    onSubmit: (text, ctx) => ({ submitted: text }),
    onKey: (action, ctx) => ({ action }),
  });
}
"#;

#[tokio::test]
async fn quickjs_interactive_overlay_render_and_callbacks_round_trip() -> Result<()> {
    let (_root, runtime) =
        load_quickjs_overlay_fixture(INTERACTIVE_OVERLAY_FIXTURE, None).await?;
    let submitted_secret = ["gh", "p_", "ABCDEFGHIJKLMNOPQRSTUVWXYZ", "abcdefghij"].concat();
    let submitted_text = format!("check {submitted_secret}");

    // The registration-time input declaration lands in the descriptor.
    let overlays = runtime.overlays();
    let descriptor = overlays.iter().find(|overlay| overlay.id == "chat").expect("overlay");
    let declaration = descriptor.input.as_ref().expect("input declaration");
    assert_eq!(declaration.placeholder.as_deref(), Some("Ask the side agent…"));
    assert!(!declaration.multiline);

    // Render returns rows plus the open-time initial draft.
    let output = runtime.invoke_overlay_render("chat").await?;
    assert_eq!(output.rows.len(), 1);
    assert_eq!(output.rows[0].text(), "hello from overlay");
    let initial = output.input.as_ref().expect("render input");
    assert_eq!(initial.value, "initial draft");

    // onSubmit receives exactly the submitted text (one submit channel).
    let submitted = runtime
        .invoke_overlay_event(
            "chat",
            pi_coding::OverlayEvent::Submit {
                text: submitted_text.clone(),
            },
        )
        .await?;
    assert_eq!(submitted["submitted"], json!(submitted_text));

    // onKey receives the limited semantic action id, never a raw key event.
    for action in [
        pi_coding::OverlayKeyAction::ScrollUp,
        pi_coding::OverlayKeyAction::Abort,
        pi_coding::OverlayKeyAction::ToggleMode,
    ] {
        let outcome = runtime
            .invoke_overlay_event("chat", pi_coding::OverlayEvent::Key { action })
            .await?;
        let expected = serde_json::to_value(action)?;
        assert_eq!(outcome["action"], expected, "{action:?}");
    }

    // An overlay without a declared callback fails actionably on that event.
    let bare_source = overlay_fixture();
    let (_root, bare) = load_quickjs_overlay_fixture(&bare_source, None).await?;
    let error = bare
        .invoke_overlay_event(
            "chat",
            pi_coding::OverlayEvent::Submit {
                text: "x".to_owned(),
            },
        )
        .await
        .expect_err("overlay without onSubmit must fail actionably");
    assert!(
        format!("{error:#}").contains("no onSubmit callback"),
        "{error:#}"
    );
    bare.shutdown().await;

    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn quickjs_overlay_render_enforces_row_and_length_bounds() -> Result<()> {
    let source = r#"
export default function (pi) {
  pi.registerOverlay({
    id: "big",
    title: "Big",
    render: () => {
      const rows = [];
      for (let i = 0; i < 150; i++) rows.push("row-" + i);
      rows.push("x".repeat(500));
      return rows;
    },
  });
}
"#;
    let (_root, runtime) = load_quickjs_overlay_fixture(source, None).await?;
    let output = runtime.invoke_overlay_render("big").await?;
    let rows = &output.rows;
    assert_eq!(rows.len(), pi_coding::OVERLAY_MAX_ROWS, "rows must be capped at 100");
    assert!(
        rows.iter().all(|row| row.text().chars().count() <= pi_coding::OVERLAY_MAX_ROW_CHARS),
        "every row must be truncated to 200 chars"
    );
    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn quickjs_overlay_open_round_trips_through_ui_host() -> Result<()> {
    let ui = Arc::new(ScriptedUi::default());
    let source = overlay_fixture();
    let (_root, runtime) = load_quickjs_overlay_fixture(&source, Some(ui.clone())).await?;
    runtime.open_overlay("chat").await?;
    let requests = ui.requests();
    assert!(
        requests.iter().any(|request| matches!(
            request,
            ExtensionUiRequest::OverlayOpen { id, title, .. } if id == "chat" && title.as_deref() == Some("Side Chat")
        )),
        "open_overlay must issue OverlayOpen with the registered title: {requests:?}"
    );
    let error = runtime
        .open_overlay("missing")
        .await
        .expect_err("unknown overlay must fail");
    assert!(error.to_string().contains("unknown extension overlay"), "{error:#}");
    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn quickjs_overlay_lifecycle_events_are_rejected_at_registration() -> Result<()> {
    // overlay_open/overlay_close have no host->extension producer (overlay
    // lifecycle is owned by the TUI page stack, not the extension runtime),
    // so they are not allow-listed: registering on them fails fast with an
    // actionable error instead of silently never firing.
    let root = TempDir::new()?;
    std::fs::write(
        root.path().join("overlay.mjs"),
        "export default function (pi) {\n\
         pi.registerOverlay({ id: \"chat\", title: \"Side Chat\", render: () => [] });\n\
         pi.on(\"overlay_open\", () => null);\n\
         pi.on(\"overlay_close\", () => null);\n\
         }\n",
    )?;
    let manifest = root.path().join("pi-extension.json");
    std::fs::write(
        &manifest,
        serde_json::to_vec_pretty(&json!({
            "schemaVersion": 1,
            "id": "quickjs-overlay-events",
            "runtime": "quickjs",
            "entry": "overlay.mjs",
            "capabilities": ["event_hooks", "overlays"],
        }))?,
    )?;
    let spec = extension_spec_from_package_resource(&resource(manifest, true))?;
    let runtime = ExtensionRuntime::process(None, quickjs_options());
    let report = runtime.load(vec![spec]).await;
    assert_eq!(report.failures.len(), 1, "{:?}", report.failures);
    assert!(
        report.failures[0]
            .message
            .contains("unsupported extension event overlay_open"),
        "{}",
        report.failures[0].message
    );
    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn quickjs_overlay_registration_requires_overlays_capability() -> Result<()> {
    let root = TempDir::new()?;
    std::fs::write(
        root.path().join("overlay.mjs"),
        r#"export default function (pi) { pi.registerOverlay({ id: "x", title: "X", render: () => [] }); }"#,
    )?;
    let manifest = root.path().join("pi-extension.json");
    std::fs::write(
        &manifest,
        serde_json::to_vec_pretty(&json!({
            "schemaVersion": 1,
            "id": "quickjs-test",
            "runtime": "quickjs",
            "entry": "overlay.mjs",
            "capabilities": [],
        }))?,
    )?;
    let spec = extension_spec_from_package_resource(&resource(manifest, true))?;
    let runtime = ExtensionRuntime::process(None, quickjs_options());
    let report = runtime.load(vec![spec]).await;
    assert_eq!(report.failures.len(), 1, "{:?}", report.failures);
    assert!(
        report.failures[0].message.contains("overlays capability"),
        "{:?}",
        report.failures[0].message
    );
    runtime.shutdown().await;
    Ok(())
}
