use std::{collections::BTreeSet, path::PathBuf, sync::Arc, time::Duration};

use anyhow::Result;
use pi_agent::{AbortController, AgentToolResult};
use pi_coding::{
    ExtensionCancellation, ExtensionCapability, ExtensionManifestRuntime, ExtensionMode,
    ExtensionOrigin, ExtensionPermissionSet, ExtensionRuntime, ExtensionRuntimeOptions,
    ExtensionSpecRuntime, ExtensionUiCapability, ExtensionUiContext, ExtensionUiHost,
    ExtensionUiRequest, ExtensionUiResponse, PackageResourceKind, PackageResourceSpec,
    PackageScope, ProcessExtensionManifest, extension_spec_from_package_resource,
};
use serde_json::json;
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

fn write_bun_package(root: &TempDir, entry: &str) -> Result<PathBuf> {
    let manifest = root.path().join("pi-extension.json");
    std::fs::write(
        &manifest,
        serde_json::to_vec_pretty(&json!({
            "schemaVersion": 1,
            "id": "bun-test",
            "runtime": "bun",
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
fn bun_manifest_rejects_untrusted_traversal_and_unsupported_entry() -> Result<()> {
    let untrusted = TempDir::new()?;
    std::fs::write(untrusted.path().join("index.ts"), "export default () => {}")?;
    let manifest = write_bun_package(&untrusted, "index.ts")?;
    let error = extension_spec_from_package_resource(&resource(manifest, false))
        .expect_err("untrusted project Bun extension must fail closed");
    assert!(
        error
            .to_string()
            .contains("untrusted project extension manifest")
    );

    let traversal = TempDir::new()?;
    let manifest = write_bun_package(&traversal, "../outside.ts")?;
    let error = extension_spec_from_package_resource(&resource(manifest, true))
        .expect_err("traversal must fail closed");
    assert!(error.to_string().contains("must remain inside"));

    let unsupported = TempDir::new()?;
    std::fs::write(unsupported.path().join("index.py"), "pass")?;
    let manifest = write_bun_package(&unsupported, "index.py")?;
    let error = extension_spec_from_package_resource(&resource(manifest, true))
        .expect_err("unsupported entry must fail closed");
    assert!(
        error
            .to_string()
            .contains("must end in .ts, .js, .mjs, or .cjs")
    );
    Ok(())
}

#[test]
fn bun_manifest_rejects_process_inexpressible_capabilities() -> Result<()> {
    for capability in ["message_renderers", "provider_metadata"] {
        let root = TempDir::new()?;
        std::fs::write(root.path().join("index.ts"), "export default () => {}")?;
        let manifest = root.path().join("pi-extension.json");
        std::fs::write(
            &manifest,
            serde_json::to_vec_pretty(&json!({
                "schemaVersion": 1,
                "id": "unsupported-bun-capability",
                "runtime": "bun",
                "entry": "index.ts",
                "capabilities": [capability]
            }))?,
        )?;
        let error = extension_spec_from_package_resource(&resource(manifest, true))
            .expect_err("process-inexpressible Bun capability must fail closed");
        assert!(error.to_string().contains(capability), "{error:#}");
    }
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

#[tokio::test]
async fn trusted_bun_extension_registers_and_invokes_real_bun() -> Result<()> {
    let Some(bun) = bun_executable() else {
        return Ok(());
    };
    let root = TempDir::new()?;
    std::fs::write(
        root.path().join("index.ts"),
        r#"
export default function (pi: any) {
  pi.registerCommand("hello", {
    description: "Say hello",
    handler: async (_args: string, ctx: any) => ctx.ui.notify("hello from bun", "info"),
  });
  pi.registerTool({
    name: "echo_bun",
    label: "Echo Bun",
    description: "Echo a value",
    parameters: {
      type: "object",
      properties: { value: { type: "string" } },
      required: ["value"],
    },
    execute: async (_id: string, params: any, _signal: AbortSignal, onUpdate: any) => {
      onUpdate?.({ content: [{ type: "text", text: "partial" }] });
      return { content: [{ type: "text", text: `bun:${params.value}` }] };
    },
  });
  pi.on("session_start", async (event: any) => ({ observed: event.reason }));
}
"#,
    )?;
    let manifest_path = write_bun_package(&root, "index.ts")?;
    let mut spec = extension_spec_from_package_resource(&resource(manifest_path, true))?;
    assert!(matches!(spec.runtime, ExtensionSpecRuntime::Bun { .. }));
    spec.environment.insert(
        "PI_BUN_EXECUTABLE".to_owned(),
        bun.to_string_lossy().into_owned(),
    );

    let runtime = ExtensionRuntime::process(
        Some(Arc::new(NotifyUi)),
        ExtensionRuntimeOptions {
            mode: ExtensionMode::Tui,
            handshake_timeout: Duration::from_secs(10),
            load_timeout: Duration::from_secs(10),
            initialize_timeout: Duration::from_secs(10),
            invocation_timeout: Duration::from_secs(10),
            hook_timeout: Duration::from_secs(10),
            shutdown_timeout: Duration::from_secs(2),
            ..ExtensionRuntimeOptions::default()
        },
    );
    let report = runtime.load(vec![spec]).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert_eq!(runtime.commands()[0].name, "hello");
    assert_eq!(runtime.tools()[0].name, "echo_bun");

    runtime
        .invoke_command("hello", String::new(), None, None)
        .await?;
    let (_, signal) = AbortController::new();
    let result: AgentToolResult = runtime
        .invoke_tool(
            "echo_bun",
            "call-1".to_owned(),
            json!({ "value": "ok" }),
            signal,
            None,
        )
        .await?;
    let encoded = serde_json::to_value(result)?;
    assert_eq!(encoded["content"][0]["text"], "bun:ok");
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
async fn bun_extension_rejects_oversized_outbound_frame() -> Result<()> {
    let Some(bun) = bun_executable() else {
        return Ok(());
    };
    let root = TempDir::new()?;
    std::fs::write(
        root.path().join("index.ts"),
        r#"
export default function (pi: any) {
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
            "id": "oversized-bun",
            "runtime": "bun",
            "entry": "index.ts",
            "capabilities": ["commands"]
        }))?,
    )?;
    let mut spec = extension_spec_from_package_resource(&resource(manifest_path, true))?;
    spec.environment.insert(
        "PI_BUN_EXECUTABLE".to_owned(),
        bun.to_string_lossy().into_owned(),
    );
    let runtime = ExtensionRuntime::process(
        None,
        ExtensionRuntimeOptions {
            max_frame_bytes: 1024,
            ..ExtensionRuntimeOptions::default()
        },
    );
    let report = runtime.load(vec![spec]).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    let error = runtime
        .invoke_command("oversized", String::new(), None, None)
        .await
        .expect_err("oversized Bun response must fail closed");
    assert!(error.to_string().contains("frame exceeds 1024 bytes"), "{error:#}");
    runtime.shutdown().await;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn bun_extension_scrubs_parent_environment_and_kills_descendants() -> Result<()> {
    let Some(bun) = bun_executable() else {
        return Ok(());
    };
    if std::env::var_os("HOME").is_none() {
        return Ok(());
    }
    let root = TempDir::new()?;
    std::fs::write(
        root.path().join("index.ts"),
        r#"
import { spawn } from "node:child_process";

export default function (pi: any) {
  pi.registerCommand("security_probe", {
    handler: () => {
      const child = spawn("/bin/sleep", ["120"], { stdio: "ignore" });
      return {
        home: process.env.HOME ?? null,
        explicit: process.env.EXTENSION_FIXTURE ?? null,
        bunExecutable: process.env.PI_BUN_EXECUTABLE ?? null,
        pid: child.pid,
      };
    },
  });
}
"#,
    )?;
    let manifest_path = write_bun_package(&root, "index.ts")?;
    let mut spec = extension_spec_from_package_resource(&resource(manifest_path, true))?;
    spec.environment.insert(
        "PI_BUN_EXECUTABLE".to_owned(),
        bun.to_string_lossy().into_owned(),
    );
    spec.environment
        .insert("EXTENSION_FIXTURE".to_owned(), "allowed".to_owned());
    let runtime = ExtensionRuntime::process(None, ExtensionRuntimeOptions::default());
    let report = runtime.load(vec![spec]).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    let result = runtime
        .invoke_command("security_probe", String::new(), None, None)
        .await?;
    assert!(result["home"].is_null(), "parent HOME leaked into extension");
    assert_eq!(result["explicit"], "allowed");
    assert!(
        result["bunExecutable"].is_null(),
        "launcher hint leaked into extension"
    );
    let pid = result["pid"].as_i64().expect("spawned child pid") as i32;

    runtime.shutdown().await;

    use nix::{errno::Errno, sys::signal::kill, unistd::Pid};
    let mut exited = false;
    for _ in 0..100 {
        match kill(Pid::from_raw(pid), None) {
            Err(Errno::ESRCH) => {
                exited = true;
                break;
            }
            Ok(()) | Err(Errno::EPERM) => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
    assert!(exited, "extension descendant {pid} survived runtime shutdown");
    Ok(())
}

#[tokio::test]
async fn bun_runtime_unavailable_fails_without_entry_details() -> Result<()> {
    let root = TempDir::new()?;
    let entry = root.path().join("secret-name.ts");
    std::fs::write(&entry, "export default () => {}")?;
    let permissions = ExtensionPermissionSet {
        capabilities: BTreeSet::from([ExtensionCapability::Tools]),
        ui_capabilities: BTreeSet::<ExtensionUiCapability>::new(),
    };
    let mut spec = pi_coding::ExtensionSpec::new_runtime(
        "no-bun",
        ExtensionSpecRuntime::Bun { entry },
        root.path(),
        ExtensionOrigin::Project,
        true,
        permissions,
    );
    spec.environment.insert(
        "PI_BUN_EXECUTABLE".to_owned(),
        root.path()
            .join("missing-bun")
            .to_string_lossy()
            .into_owned(),
    );
    let runtime = ExtensionRuntime::process(None, ExtensionRuntimeOptions::default());
    let report = runtime.load(vec![spec]).await;
    assert_eq!(report.failures.len(), 1);
    let message = &report.failures[0].message;
    assert!(message.contains("Bun runtime is unavailable"));
    assert!(!message.contains("secret-name"));
    Ok(())
}
