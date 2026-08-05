use std::{collections::BTreeSet, path::PathBuf, sync::Arc, time::Duration};

use anyhow::Result;
use pi_agent::{AbortController, AgentToolResult, ToolCapability};
use pi_coding::{
    ExtensionCancellation, ExtensionCapability, ExtensionManifestRuntime, ExtensionMode,
    ExtensionOrigin, ExtensionPermissionSet, ExtensionRuntime, ExtensionRuntimeOptions,
    ExtensionSpec, ExtensionSpecRuntime, ExtensionUiCapability, ExtensionUiContext,
    ExtensionUiHost, ExtensionUiRequest, ExtensionUiResponse, PackageResourceKind,
    PackageResourceSpec, PackageScope, ProcessExtensionManifest,
    extension_spec_from_package_resource,
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

fn bun_required() -> bool {
    matches!(
        std::env::var("BUN_REQUIRED").as_deref(),
        Ok("1" | "true" | "TRUE" | "yes" | "YES")
    )
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

/// Resolve Bun for designated integration tests.
///
/// Returns `Ok(None)` when Bun is optional and absent. Returns `Err` with an
/// actionable message when `BUN_REQUIRED` is set and Bun cannot be resolved —
/// designated tests must not silently skip in that configuration.
const BUN_REQUIRED_ABSENCE_MESSAGE: &str = "BUN_REQUIRED is set but Bun was not found; install Bun on PATH or set PI_BUN_EXECUTABLE to an absolute bun binary";

fn require_bun() -> Result<Option<PathBuf>> {
    if let Some(bun) = bun_executable() {
        return Ok(Some(bun));
    }
    if bun_required() {
        anyhow::bail!("{BUN_REQUIRED_ABSENCE_MESSAGE}");
    }
    Ok(None)
}

fn bun_options() -> ExtensionRuntimeOptions {
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

fn attach_bun(spec: &mut ExtensionSpec, bun: &std::path::Path) {
    spec.environment.insert(
        "PI_BUN_EXECUTABLE".to_owned(),
        bun.to_string_lossy().into_owned(),
    );
}

#[cfg(unix)]
async fn wait_until_dead(pid: i32) -> Result<bool> {
    use nix::{errno::Errno, sys::signal::kill, unistd::Pid};
    for _ in 0..100 {
        match kill(Pid::from_raw(pid), None) {
            Err(Errno::ESRCH) => return Ok(true),
            Ok(()) | Err(Errno::EPERM) => tokio::time::sleep(Duration::from_millis(20)).await,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(false)
}

#[tokio::test]
async fn trusted_versioned_bun_manifest_registers_ts_commands_and_tools() -> Result<()> {
    let Some(bun) = require_bun()? else {
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
  pi.registerCommand("alpha", {
    description: "first chain step",
    handler: async (args: string) => `alpha:${args || "none"}`,
  });
  pi.registerCommand("beta", {
    description: "second chain step",
    handler: async (args: string) => `beta:${args || "none"}`,
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
    assert_eq!(spec.origin, ExtensionOrigin::Project);
    assert!(spec.project_trusted);
    attach_bun(&mut spec, &bun);

    let runtime = ExtensionRuntime::process(Some(Arc::new(NotifyUi)), bun_options());
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
    assert_eq!(runtime.tools()[0].name, "echo_bun");
    assert_eq!(
        runtime.tools()[0].capability,
        ToolCapability::Exec,
        "omitted Bun tool capability must fail safe"
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
    let Some(bun) = require_bun()? else {
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
    attach_bun(&mut spec, &bun);
    let runtime = ExtensionRuntime::process(
        None,
        ExtensionRuntimeOptions {
            max_frame_bytes: 1024,
            ..bun_options()
        },
    );
    let report = runtime.load(vec![spec]).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    let error = runtime
        .invoke_command("oversized", String::new(), None, None)
        .await
        .expect_err("oversized Bun response must fail closed");
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

#[cfg(unix)]
#[tokio::test]
async fn bun_extension_scrubs_parent_environment_and_kills_descendants() -> Result<()> {
    let Some(bun) = require_bun()? else {
        return Ok(());
    };
    // Parent process env is scrubbed via Command::env_clear before launch.
    // Assert ambient secrets (HOME/PATH/USER) and the launcher hint never cross.

    let root = TempDir::new()?;
    std::fs::write(
        root.path().join("index.ts"),
        r#"
import { spawn } from "node:child_process";
import { writeFileSync } from "node:fs";

export default function (pi: any) {
  pi.registerCommand("security_probe", {
    handler: () => {
      // Stay in the extension process group so host killpg reaps descendants.
      const child = spawn("/bin/sleep", ["120"], { stdio: "ignore" });
      return {
        home: process.env.HOME ?? null,
        path: process.env.PATH ?? null,
        user: process.env.USER ?? null,
        explicit: process.env.EXTENSION_FIXTURE ?? null,
        bunExecutable: process.env.PI_BUN_EXECUTABLE ?? null,
        pid: child.pid,
      };
    },
  });
  pi.registerCommand("hang_for_cancel", {
    handler: async (_args: string, ctx: any) => {
      const child = spawn("/bin/sleep", ["120"], { stdio: "ignore" });
      writeFileSync("hang-pid.txt", String(child.pid));
      await new Promise<void>((_resolve, reject) => {
        if (ctx.signal.aborted) {
          reject(new Error("extension invocation was cancelled"));
          return;
        }
        ctx.signal.addEventListener(
          "abort",
          () => reject(new Error("extension invocation was cancelled")),
          { once: true },
        );
      });
    },
  });
}
"#,
    )?;
    let manifest_path = write_bun_package(&root, "index.ts")?;
    let mut spec = extension_spec_from_package_resource(&resource(manifest_path, true))?;
    attach_bun(&mut spec, &bun);
    spec.environment
        .insert("EXTENSION_FIXTURE".to_owned(), "allowed".to_owned());
    let runtime = ExtensionRuntime::process(None, bun_options());
    let report = runtime.load(vec![spec]).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);

    let result = runtime
        .invoke_command("security_probe", String::new(), None, None)
        .await?;
    assert!(
        result["home"].is_null(),
        "parent HOME leaked into extension: {result}"
    );
    assert!(
        result["path"].is_null(),
        "parent PATH leaked into extension: {result}"
    );
    assert!(
        result["user"].is_null(),
        "parent USER leaked into extension: {result}"
    );
    assert_eq!(result["explicit"], "allowed");
    assert!(
        result["bunExecutable"].is_null(),
        "launcher hint leaked into extension"
    );
    let probe_pid = result["pid"].as_i64().expect("spawned child pid") as i32;

    // Cancellation must abort the in-flight invocation; descendant cleanup is
    // completed when the host tears the process group down on shutdown.
    let cancellation = ExtensionCancellation::new();
    let cancel_flag = cancellation.clone();
    let cancel_runtime = runtime.clone();
    let cancel_handle = tokio::spawn(async move {
        cancel_runtime
            .invoke_command(
                "hang_for_cancel",
                String::new(),
                Some(Duration::from_secs(10)),
                Some(cancellation),
            )
            .await
    });
    let hang_pid_path = root.path().join("hang-pid.txt");
    let mut hang_pid = None;
    for _ in 0..200 {
        if hang_pid_path.is_file() {
            let raw = std::fs::read_to_string(&hang_pid_path)?;
            if let Ok(pid) = raw.trim().parse::<i32>() {
                hang_pid = Some(pid);
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let hang_pid = hang_pid.expect("hang command did not publish child pid");
    cancel_flag.cancel();
    let join_result = cancel_handle.await.expect("cancel join");
    let cancelled_error = match join_result {
        Ok(value) => {
            panic!("cancelled hang command must fail, got success value: {value}");
        }
        Err(error) => error,
    };
    let cancelled_message = cancelled_error.to_string();
    assert!(
        cancelled_message.contains("cancelled") || cancelled_message.contains("cancel"),
        "expected cancellation error, got: {cancelled_message}"
    );

    runtime.shutdown().await;

    assert!(
        wait_until_dead(probe_pid).await?,
        "security_probe descendant {probe_pid} survived runtime shutdown"
    );
    assert!(
        wait_until_dead(hang_pid).await?,
        "cancelled hang descendant {hang_pid} survived runtime shutdown / process-group kill"
    );
    Ok(())
}

#[tokio::test]
async fn failed_bun_reload_keeps_prior_generation_after_invalid_update() -> Result<()> {
    let Some(bun) = require_bun()? else {
        return Ok(());
    };
    let good = TempDir::new()?;
    std::fs::write(
        good.path().join("index.ts"),
        r#"
export default function (pi: any) {
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
            "runtime": "bun",
            "entry": "index.ts",
            "capabilities": ["commands"]
        }))?,
    )?;
    let mut good_spec = extension_spec_from_package_resource(&resource(good_manifest, true))?;
    attach_bun(&mut good_spec, &bun);

    let runtime = ExtensionRuntime::process(None, bun_options());
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
        bad.path().join("index.ts"),
        "export default function () { throw new Error('intentional reload failure'); }\n",
    )?;
    let bad_manifest = bad.path().join("pi-extension.json");
    std::fs::write(
        &bad_manifest,
        serde_json::to_vec_pretty(&json!({
            "schemaVersion": 1,
            "id": "broken-ext",
            "runtime": "bun",
            "entry": "index.ts",
            "capabilities": ["commands"]
        }))?,
    )?;
    let mut bad_spec = extension_spec_from_package_resource(&resource(bad_manifest, true))?;
    attach_bun(&mut bad_spec, &bun);

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
    let mut missing_spec = ExtensionSpec::new_runtime(
        "missing-entry",
        ExtensionSpecRuntime::Bun {
            entry: missing_root.path().join("missing.ts"),
        },
        missing_root.path(),
        ExtensionOrigin::Project,
        true,
        permissions,
    );
    attach_bun(&mut missing_spec, &bun);
    let candidate = runtime.stage_reload(vec![missing_spec]).await;
    let staged = candidate.report();
    assert!(
        !staged.failures.is_empty(),
        "missing Bun entry must fail during stage_reload: {staged:?}"
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

#[tokio::test]
async fn bun_runtime_unavailable_fails_without_entry_details() -> Result<()> {
    let root = TempDir::new()?;
    let entry = root.path().join("secret-name.ts");
    std::fs::write(&entry, "export default () => {}")?;
    let permissions = ExtensionPermissionSet {
        capabilities: BTreeSet::from([ExtensionCapability::Tools]),
        ui_capabilities: BTreeSet::<ExtensionUiCapability>::new(),
    };
    let mut spec = ExtensionSpec::new_runtime(
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
    assert!(
        message.contains("Bun runtime is unavailable"),
        "absence must be explicit: {message}"
    );
    assert!(
        !message.contains("secret-name"),
        "unavailable Bun errors must not leak entry paths: {message}"
    );
    Ok(())
}

#[test]
fn bun_required_env_fails_actionably_when_bun_absent() {
    // Pin the fail-closed copy that designated Bun tests must surface.
    assert!(
        BUN_REQUIRED_ABSENCE_MESSAGE.contains("BUN_REQUIRED")
            && BUN_REQUIRED_ABSENCE_MESSAGE.contains("PI_BUN_EXECUTABLE")
            && BUN_REQUIRED_ABSENCE_MESSAGE.contains("install Bun"),
        "BUN_REQUIRED absence guidance must stay actionable"
    );

    // When this process truly lacks Bun and BUN_REQUIRED is engaged, designated
    // tests must surface that same actionable error rather than `return Ok(())`.
    if bun_executable().is_some() {
        return;
    }
    if !bun_required() {
        // Absence without BUN_REQUIRED is an intentional soft skip path.
        let resolved = require_bun().expect("optional Bun absence is Ok(None)");
        assert!(resolved.is_none());
        return;
    }
    let error = require_bun().expect_err("BUN_REQUIRED without Bun must fail");
    assert_eq!(error.to_string(), BUN_REQUIRED_ABSENCE_MESSAGE);
}
