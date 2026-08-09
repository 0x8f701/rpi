//! Real Linux sandbox smoke tests for the bash tool.
//!
//! These tests exercise the actual `unshare` wrapper against the host kernel:
//! mount/pid/net namespaces, tmpfs root + `pivot_root`, bind-mounted allowed
//! paths, and network-off. They require Linux, the util-linux `unshare`
//! binary, and permission to create unprivileged user namespaces (e.g.
//! `kernel.unprivileged_userns_clone=1` / an AppArmor profile that permits
//! it).
//!
//! Live confinement tests are `#[ignore]`d by default: on kernels without
//! unprivileged user namespaces they are reported as IGNORED — an explicit
//! unsupported status, never a fake pass. On a supported kernel run them
//! with `--include-ignored`. If a live test is forced to run on an
//! unsupported kernel it FAILS with an explicit reason instead of passing
//! silently (`PI_TEST_SANDBOX_FORCE_UNSUPPORTED=1` simulates that for CI).
//! The deterministic wrapper-construction, validation, literal-transport,
//! and timeout/abort cleanup contracts always run in `pi_coding::sandbox`'s
//! unit tests (no kernel support required).

use std::path::Path;
use std::sync::Arc;

use pi_agent::{AbortController, AgentToolResult, ToolCallContext, ToolUpdateFn};
use pi_ai::ContentBlock;
use pi_coding::{
    OrchestrationSettings, ResourceManager, ResourceManagerOptions, SandboxConfigFn,
    SandboxSettings, Session, SessionOptions, Settings,
};
use serde_json::json;
use tempfile::TempDir;

/// Returns `Ok(())` when the host can actually run the sandbox: `unshare`
/// exists and an unprivileged user namespace with mounts can be created. Any
/// missing prerequisite is an explicit `Err(reason)`. Live tests refuse to
/// fake-pass on it; `PI_TEST_SANDBOX_FORCE_UNSUPPORTED=1` simulates an
/// unsupported host (deterministic test seam for CI/demos).
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

fn tool_ctx(command: &str, sandboxed: bool) -> ToolCallContext {
    let (controller, abort) = AbortController::new();
    std::mem::forget(controller);
    let on_update: ToolUpdateFn = Arc::new(|_result: AgentToolResult| {});
    ToolCallContext {
        tool_call_id: "sandbox-smoke".to_owned(),
        arguments: json!({ "command": command, "sandboxed": sandboxed }),
        on_update,
        abort,
        model: None,
    }
}

fn text_of(result: &AgentToolResult) -> String {
    match result.content.first() {
        Some(ContentBlock::Text { text, .. }) => text.clone(),
        _ => String::new(),
    }
}

/// `cat /etc/passwd` inside the sandbox fails: the host `/etc` is not among
/// the allowed paths, so the file does not exist in the confined root.
#[ignore = "live confinement test: requires Linux unprivileged user namespaces (run with --include-ignored on a supported kernel)"]
#[tokio::test]
async fn sandboxed_bash_denies_etc_passwd() {
    sandbox_usable().expect("live sandbox test refuses to fake-pass on an unsupported kernel");
    let dir = TempDir::new().expect("temp dir");
    let tool = pi_coding::create_tool("bash", dir.path().to_str().expect("utf8")).expect("bash tool");
    let error = (tool.execute)(tool_ctx("cat /etc/passwd", true))
        .await
        .expect_err("cat /etc/passwd must fail inside the sandbox");
    assert!(
        error.to_string().contains("No such file"),
        "expected /etc/passwd to be invisible: {error}"
    );
}

/// A plain command runs in the sandboxed working directory (which is allowed
/// by default) and can write to it; the sandbox root is a fresh tmpfs.
#[ignore = "live confinement test: requires Linux unprivileged user namespaces (run with --include-ignored on a supported kernel)"]
#[tokio::test]
async fn sandboxed_bash_runs_in_cwd_and_writes() {
    sandbox_usable().expect("live sandbox test refuses to fake-pass on an unsupported kernel");
    let dir = TempDir::new().expect("temp dir");
    let cwd = dir.path().to_str().expect("utf8");
    let tool = pi_coding::create_tool("bash", cwd).expect("bash tool");
    let result = (tool.execute)(tool_ctx("pwd && echo hi", true))
        .await
        .expect("sandboxed echo");
    let text = text_of(&result);
    assert!(
        text.contains("hi"),
        "sandboxed command output missing: {text:?}"
    );
    // The working directory is visible at its host path.
    assert!(text.contains(cwd), "cwd must be visible: {text:?}");

    let written = (tool.execute)(tool_ctx("printf wrote > sandbox-write-test && cat sandbox-write-test", true))
        .await
        .expect("sandboxed write");
    assert!(text_of(&written).contains("wrote"), "write failed: {written:?}");
    assert!(
        dir.path().join("sandbox-write-test").is_file(),
        "write must land in the host working directory (bind mount)"
    );
    std::fs::remove_file(dir.path().join("sandbox-write-test")).expect("cleanup");
}

/// Network is off by default: a fresh net namespace has loopback only, so any
/// non-loopback connect fails with "Network is unreachable".
#[ignore = "live confinement test: requires Linux unprivileged user namespaces (run with --include-ignored on a supported kernel)"]
#[tokio::test]
async fn sandboxed_bash_network_off_by_default() {
    sandbox_usable().expect("live sandbox test refuses to fake-pass on an unsupported kernel");
    let dir = TempDir::new().expect("temp dir");
    let tool = pi_coding::create_tool("bash", dir.path().to_str().expect("utf8")).expect("bash tool");
    let error = (tool.execute)(tool_ctx("(echo > /dev/tcp/93.184.216.34/80)", true))
        .await
        .expect_err("non-loopback connect must fail inside the sandbox");
    assert!(
        error.to_string().contains("Network is unreachable"),
        "expected network-off failure: {error}"
    );
    // DNS resolution also fails (no /etc/nsswitch.conf in the confined root).
    let dns = (tool.execute)(tool_ctx("getent hosts example.com", true))
        .await
        .expect_err("getent must fail inside the sandbox");
    assert!(
        !dns.to_string().contains("Command exited with code 0"),
        "getent must not succeed: {dns}"
    );
}

/// Settings-driven sandbox: `Settings.sandbox.enabled` applies to
/// `execute_bash` (the RPC path, which has no per-call override), and
/// `deniedPaths` hides a path nested inside an allowed path.
#[ignore = "live confinement test: requires Linux unprivileged user namespaces (run with --include-ignored on a supported kernel)"]
#[tokio::test]
async fn sandboxed_bash_honors_settings_allowed_and_denied_paths() {
    sandbox_usable().expect("live sandbox test refuses to fake-pass on an unsupported kernel");
    let dir = TempDir::new().expect("temp dir");
    let cwd = dir.path();
    std::fs::write(cwd.join("allowed.txt"), "open").expect("allowed file");
    std::fs::write(cwd.join("secret.txt"), "closed").expect("secret file");

    let settings = Settings {
        sandbox: Some(SandboxSettings {
            enabled: Some(true),
            network: Some(false),
            allowed_paths: Some(vec![cwd.to_str().expect("utf8").to_owned()]),
            denied_paths: Some(vec![cwd.join("secret.txt").to_str().expect("utf8").to_owned()]),
            ..Default::default()
        }),
        ..Default::default()
    };
    let config = pi_coding::sandbox::resolve(
        settings.sandbox.as_ref(),
        cwd,
        &pi_coding::agent_dir_path(),
    )
    .expect("resolved sandbox config");
    assert!(config.enabled);
    let resolver: SandboxConfigFn = Arc::new(move || Some(config.clone()));

    let chunk: Arc<dyn Fn(String) + Send + Sync> = Arc::new(|_| {});
    let (controller, abort) = AbortController::new();

    // Allowed path: readable and writable.
    let ok = pi_coding::execute_bash(cwd, "cat allowed.txt", None, Some(resolver.clone()), chunk.clone(), abort.clone())
        .await
        .expect("sandboxed cat allowed.txt");
    assert_eq!(ok.exit_code, Some(0), "allowed.txt must be readable: {}", ok.output);

    // Denied path: hidden by the empty overlay even though it sits inside an
    // allowed path (content must not leak; the path resolves to an empty file).
    let (_, abort2) = AbortController::new();
    let denied = pi_coding::execute_bash(cwd, "cat secret.txt", None, Some(resolver), chunk, abort2)
        .await
        .expect("sandboxed cat secret.txt");
    assert!(
        !denied.output.contains("closed"),
        "secret.txt content must be hidden by deniedPaths: {}",
        denied.output
    );
    drop(controller);
}

/// A hostile allowed/denied path containing shell metacharacters is treated
/// as a literal path and never re-evaluated: the setup script consumes path
/// values positionally (no `eval`), so `$(...)`/backticks inside a configured
/// path cannot execute. The sentinel file must not appear and the sandboxed
/// command must still run with its normal paths intact.
#[ignore = "live confinement test: requires Linux unprivileged user namespaces (run with --include-ignored on a supported kernel)"]
#[tokio::test]
async fn sandboxed_bash_never_evaluates_hostile_path_values() {
    sandbox_usable().expect("live sandbox test refuses to fake-pass on an unsupported kernel");
    let dir = TempDir::new().expect("temp dir");
    let cwd = dir.path();
    let sentinel = cwd.join("eval-pwned");
    let sentinel_str = sentinel.to_str().expect("utf8");
    std::fs::write(cwd.join("allowed.txt"), "open").expect("allowed file");

    // Hostile values for both lists: `$(...)` and backticks, with spaces and
    // quotes mixed in. resolve() joins them onto cwd (they are relative), so
    // the payload text survives into the wrapper's path transport untouched.
    let hostile_allowed = format!("$(touch {sentinel_str}) 'quoted'");
    let hostile_denied = format!("`touch {sentinel_str}` \"quoted\"");

    let settings = Settings {
        sandbox: Some(SandboxSettings {
            enabled: Some(true),
            network: Some(false),
            allowed_paths: Some(vec![
                cwd.to_str().expect("utf8").to_owned(),
                hostile_allowed,
            ]),
            denied_paths: Some(vec![hostile_denied]),
            ..Default::default()
        }),
        ..Default::default()
    };
    let config = pi_coding::sandbox::resolve(
        settings.sandbox.as_ref(),
        cwd,
        &pi_coding::agent_dir_path(),
    )
    .expect("resolved sandbox config");
    assert!(config.enabled);
    let resolver: SandboxConfigFn = Arc::new(move || Some(config.clone()));

    let chunk: Arc<dyn Fn(String) + Send + Sync> = Arc::new(|_| {});
    let (controller, abort) = AbortController::new();

    let ok = pi_coding::execute_bash(
        cwd,
        "cat allowed.txt",
        None,
        Some(resolver),
        chunk,
        abort.clone(),
    )
    .await
    .expect("sandboxed cat with hostile configured paths");
    assert_eq!(
        ok.exit_code, Some(0),
        "the sandbox must still run with hostile paths configured: {}",
        ok.output
    );
    assert_eq!(
        ok.output.trim(),
        "open",
        "the normal allowed path must still be mounted: {}",
        ok.output
    );
    assert!(
        !sentinel.exists(),
        "hostile path values must be handled as literals and never executed: {sentinel_str}"
    );
    drop(controller);
}

/// `Settings.sandbox.readOnly`: allowed paths are mounted read-only (bind
/// remount with MS_RDONLY), so reads work and writes fail; without the flag
/// the same path stays writable.
#[ignore = "live confinement test: requires Linux unprivileged user namespaces (run with --include-ignored on a supported kernel)"]
#[tokio::test]
async fn sandboxed_bash_read_only_mode_blocks_writes_to_allowed_paths() {
    sandbox_usable().expect("live sandbox test refuses to fake-pass on an unsupported kernel");
    let dir = TempDir::new().expect("temp dir");
    let cwd = dir.path();
    std::fs::write(cwd.join("allowed.txt"), "open").expect("allowed file");

    let settings = Settings {
        sandbox: Some(SandboxSettings {
            enabled: Some(true),
            network: Some(false),
            read_only: Some(true),
            allowed_paths: Some(vec![cwd.to_str().expect("utf8").to_owned()]),
            denied_paths: None,
            ..Default::default()
        }),
        ..Default::default()
    };
    let config = pi_coding::sandbox::resolve(
        settings.sandbox.as_ref(),
        cwd,
        &pi_coding::agent_dir_path(),
    )
    .expect("resolved sandbox config");
    assert!(config.enabled);
    assert!(config.read_only);
    let resolver: SandboxConfigFn = Arc::new(move || Some(config.clone()));

    let chunk: Arc<dyn Fn(String) + Send + Sync> = Arc::new(|_| {});
    let (controller, abort) = AbortController::new();

    // Reads work in read-only mode.
    let read = pi_coding::execute_bash(
        cwd,
        "cat allowed.txt",
        None,
        Some(resolver.clone()),
        chunk.clone(),
        abort.clone(),
    )
    .await
    .expect("sandboxed read of an allowed path");
    assert!(
        read.output.contains("open"),
        "read must work in read-only mode: {}",
        read.output
    );

    // Writes to the allowed path fail (EROFS propagates to the shell), and
    // nothing lands on the host.
    let (_, abort2) = AbortController::new();
    let write = pi_coding::execute_bash(
        cwd,
        "printf x > forbidden-write && echo wrote",
        None,
        Some(resolver.clone()),
        chunk.clone(),
        abort2,
    )
    .await
    .expect("sandboxed write attempt");
    assert_ne!(write.exit_code, Some(0), "write must fail: {}", write.output);
    assert!(
        !write.output.contains("wrote"),
        "write must fail in read-only mode: {}",
        write.output
    );
    assert!(
        !cwd.join("forbidden-write").exists(),
        "no file may land on the host in read-only mode"
    );

    // Same allowed path without readOnly stays writable (parity check).
    let writable_settings = Settings {
        sandbox: Some(SandboxSettings {
            enabled: Some(true),
            network: Some(false),
            read_only: Some(false),
            allowed_paths: Some(vec![cwd.to_str().expect("utf8").to_owned()]),
            denied_paths: None,
            ..Default::default()
        }),
        ..Default::default()
    };
    let writable_config = pi_coding::sandbox::resolve(
        writable_settings.sandbox.as_ref(),
        cwd,
        &pi_coding::agent_dir_path(),
    )
    .expect("resolved writable config");
    let writable_resolver: SandboxConfigFn = Arc::new(move || Some(writable_config.clone()));
    let (_, abort3) = AbortController::new();
    let writable = pi_coding::execute_bash(
        cwd,
        "printf x > writable-write && cat writable-write",
        None,
        Some(writable_resolver),
        chunk,
        abort3,
    )
    .await
    .expect("sandboxed writable write");
    assert_eq!(writable.exit_code, Some(0), "write must work without readOnly: {}", writable.output);
    assert!(writable.output.contains('x'));
    assert!(cwd.join("writable-write").is_file(), "the write must land on the host");
    let _ = std::fs::remove_file(cwd.join("writable-write"));
    drop(controller);
}

/// The sandboxed command gets a private HOME (`/root` inside the sandbox) and
/// TMPDIR (`/tmp`, the private tmpfs) instead of the host HOME: tools that
/// cache or create temp files get an empty writable location, and the host
/// home stays out of the sandboxed environment (deny-by-default filesystem).
#[ignore = "live confinement test: requires Linux unprivileged user namespaces (run with --include-ignored on a supported kernel)"]
#[tokio::test]
async fn sandboxed_bash_redirects_home_and_tmpdir() {
    sandbox_usable().expect("live sandbox test refuses to fake-pass on an unsupported kernel");
    let dir = TempDir::new().expect("temp dir");
    let cwd = dir.path();
    let host_home = std::env::var("HOME").unwrap_or_default();

    let settings = Settings {
        sandbox: Some(SandboxSettings {
            enabled: Some(true),
            network: Some(false),
            allowed_paths: Some(vec![cwd.to_str().expect("utf8").to_owned()]),
            denied_paths: None,
            ..Default::default()
        }),
        ..Default::default()
    };
    let config = pi_coding::sandbox::resolve(
        settings.sandbox.as_ref(),
        cwd,
        &pi_coding::agent_dir_path(),
    )
    .expect("resolved sandbox config");
    let resolver: SandboxConfigFn = Arc::new(move || Some(config.clone()));

    let chunk: Arc<dyn Fn(String) + Send + Sync> = Arc::new(|_| {});
    let (controller, abort) = AbortController::new();
    let probe = pi_coding::execute_bash(
        cwd,
        "printf 'HOME=%s\\nTMPDIR=%s\\n' \"$HOME\" \"$TMPDIR\" && touch \"$TMPDIR/pi-tmp-probe\" && echo tmp-writable && ls \"$HOME\" >/dev/null 2>&1 && echo home-writable",
        None,
        Some(resolver.clone()),
        chunk.clone(),
        abort.clone(),
    )
    .await
    .expect("sandboxed HOME/TMPDIR probe");
    assert!(
        probe.output.contains("HOME=/root"),
        "HOME must be redirected to the sandbox home: {}",
        probe.output
    );
    assert!(
        probe.output.contains("TMPDIR=/tmp"),
        "TMPDIR must point at the private tmpfs: {}",
        probe.output
    );
    assert!(
        probe.output.contains("tmp-writable"),
        "TMPDIR must be writable: {}",
        probe.output
    );
    assert!(
        probe.output.contains("home-writable"),
        "the sandbox HOME must be readable: {}",
        probe.output
    );
    if !host_home.is_empty() && host_home != "/root" {
        assert!(
            !probe.output.contains(&host_home),
            "the host HOME must not leak into the sandboxed environment: {}",
            probe.output
        );
    }

    // A later sandboxed spawn gets a fresh empty HOME (each spawn builds a new
    // tmpfs root): a file written to $HOME in one command is gone in the next.
    let (_, abort2) = AbortController::new();
    let first = pi_coding::execute_bash(
        cwd,
        "printf marker > \"$HOME/.pi-home-probe\" && cat \"$HOME/.pi-home-probe\"",
        None,
        Some(resolver.clone()),
        chunk.clone(),
        abort2,
    )
    .await
    .expect("sandboxed HOME write");
    assert!(first.output.contains("marker"), "HOME write failed: {}", first.output);
    let (_, abort3) = AbortController::new();
    let second = pi_coding::execute_bash(
        cwd,
        "test -e \"$HOME/.pi-home-probe\" && echo stale || echo fresh",
        None,
        Some(resolver),
        chunk,
        abort3,
    )
    .await
    .expect("sandboxed HOME freshness probe");
    assert!(
        second.output.contains("fresh"),
        "each sandboxed spawn must get a fresh empty HOME: {}",
        second.output
    );
    drop(controller);
}

/// A timed-out sandboxed command is reaped: the process group kill covers
/// `unshare`'s namespaced descendants, so no orphan `sleep` survives.
#[ignore = "live confinement test: requires Linux unprivileged user namespaces (run with --include-ignored on a supported kernel)"]
#[tokio::test]
async fn sandboxed_bash_timeout_kills_the_namespace_tree() {
    sandbox_usable().expect("live sandbox test refuses to fake-pass on an unsupported kernel");
    let dir = TempDir::new().expect("temp dir");
    let tool = pi_coding::create_tool("bash", dir.path().to_str().expect("utf8")).expect("bash tool");
    let (controller, abort) = AbortController::new();
    std::mem::forget(controller);
    let on_update: ToolUpdateFn = Arc::new(|_result: AgentToolResult| {});
    let ctx = ToolCallContext {
        tool_call_id: "sandbox-timeout".to_owned(),
        arguments: json!({ "command": "sleep 30", "sandboxed": true, "timeout": 0.5 }),
        on_update,
        abort,
        model: None,
    };
    let error = (tool.execute)(ctx)
        .await
        .expect_err("sandboxed sleep must time out");
    assert!(
        error.to_string().contains("Command timed out after"),
        "timeout error expected: {error}"
    );
    // Give the group kill a moment, then confirm no namespaced sleep remains.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let leftover = std::process::Command::new("pgrep")
        .args(["-f", "sleep 30"])
        .output()
        .expect("pgrep");
    assert!(
        !leftover.status.success() || leftover.stdout.is_empty(),
        "sandboxed sleep must not survive the timeout: {:?}",
        leftover.stdout
    );
}

/// The per-call `sandboxed` parameter overrides `sandbox.enabled`: `false`
/// disables a settings-enabled sandbox, `true` enables it with settings
/// paths even when the setting is off.
#[ignore = "live confinement test: requires Linux unprivileged user namespaces (run with --include-ignored on a supported kernel)"]
#[tokio::test]
async fn per_call_sandboxed_overrides_settings_enabled() {
    sandbox_usable().expect("live sandbox test refuses to fake-pass on an unsupported kernel");
    let dir = TempDir::new().expect("temp dir");
    let cwd = dir.path().to_str().expect("utf8");
    let settings = Settings {
        sandbox: Some(SandboxSettings {
            enabled: Some(true),
            network: Some(false),
            allowed_paths: Some(vec![cwd.to_owned()]),
            denied_paths: None,
            ..Default::default()
        }),
        ..Default::default()
    };
    let config = pi_coding::sandbox::resolve(
        settings.sandbox.as_ref(),
        Path::new(cwd),
        &pi_coding::agent_dir_path(),
    )
    .expect("resolved sandbox config");
    let resolver: SandboxConfigFn = Arc::new(move || Some(config.clone()));

    let workspace = pi_coding::WorkspaceRoots::new(cwd, Vec::<std::path::PathBuf>::new())
        .expect("workspace");
    let tools = pi_coding::create_coding_tools_for_workspace_with_context_and_resolver(
        workspace,
        None,
        None,
        None,
        None,
        Some(resolver),
        None,
        None,
    );
    let bash = tools
        .into_iter()
        .find(|tool| tool.name == "bash")
        .expect("bash tool");

    // `sandboxed: false` wins over the enabled setting: /etc/passwd reads.
    let unsandboxed = (bash.execute)(tool_ctx("cat /etc/passwd", false))
        .await
        .expect("non-sandboxed cat must succeed");
    assert!(
        text_of(&unsandboxed).contains("root:"),
        "sandboxed=false must disable the sandbox"
    );

    // `sandboxed: true` wins over a disabled setting: denied again.
    let disabled = Settings {
        sandbox: Some(SandboxSettings {
            enabled: Some(false),
            network: Some(false),
            allowed_paths: Some(vec![cwd.to_owned()]),
            denied_paths: None,
            ..Default::default()
        }),
        ..Default::default()
    };
    let off_config = pi_coding::sandbox::resolve(
        disabled.sandbox.as_ref(),
        Path::new(cwd),
        &pi_coding::agent_dir_path(),
    )
    .expect("resolved sandbox config");
    assert!(!off_config.enabled);
    let resolver: SandboxConfigFn = Arc::new(move || Some(off_config.clone()));
    let workspace = pi_coding::WorkspaceRoots::new(cwd, Vec::<std::path::PathBuf>::new())
        .expect("workspace");
    let tools = pi_coding::create_coding_tools_for_workspace_with_context_and_resolver(
        workspace,
        None,
        None,
        None,
        None,
        Some(resolver),
        None,
        None,
    );
    let bash = tools
        .into_iter()
        .find(|tool| tool.name == "bash")
        .expect("bash tool");
    let denied = (bash.execute)(tool_ctx("cat /etc/passwd", true))
        .await
        .expect_err("sandboxed=true must re-enable the sandbox");
    assert!(
        denied.to_string().contains("No such file"),
        "expected /etc/passwd invisible with sandboxed=true: {denied}"
    );
}

// ---------------------------------------------------------------------------
// Orchestration subagent children (`settings.orchestration.sandboxed`, opt-in)
// ---------------------------------------------------------------------------

/// Tool context for a subagent child's bash tool WITHOUT the per-call
/// `sandboxed` override, so the orchestration sandbox resolver (or its
/// absence) decides.
fn child_tool_ctx(command: &str) -> ToolCallContext {
    let (controller, abort) = AbortController::new();
    std::mem::forget(controller);
    let on_update: ToolUpdateFn = Arc::new(|_result: AgentToolResult| {});
    ToolCallContext {
        tool_call_id: "sandbox-child".to_owned(),
        arguments: json!({ "command": command }),
        on_update,
        abort,
        model: None,
    }
}

/// Builds a parent session with a resource manager loaded from
/// `<agent_dir>/settings.json`, then spawns a subagent child through the
/// orchestration child factory and returns the child session. The child's
/// tool set is the default coding set whose bash tool carries the live
/// orchestration sandbox resolver.
async fn spawn_subagent_child(cwd: &Path, agent_dir: &Path) -> Session {
    let resources = ResourceManager::new(ResourceManagerOptions {
        cwd: cwd.to_path_buf(),
        agent_dir: agent_dir.to_path_buf(),
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
    let parent = Session::new(SessionOptions {
        model: pi_ai::Model::default(),
        cwd: cwd.to_path_buf(),
        system_prompt: "parent".to_owned(),
        thinking_level: pi_agent::ThinkingLevel::Off,
        api_key: "test-key".to_owned(),
        compaction: None,
        stream_options: Default::default(),
        tools: None,
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: None,
        auth_resolver: None,
    })
    .expect("parent session");
    parent.attach_resources(resources).await.expect("attach resources");
    let factory = pi_coding::OrchestrationRuntime::child_factory_from_session(&parent);
    let definition = pi_coding::parse_agent_definition(
        Path::new("task.md"),
        "---\nname: task\ndescription: task\n---\nprompt",
        pi_coding::AgentDefinitionSource::Bundled,
        true,
    )
    .expect("definition");
    let request = pi_coding::ChildSessionRequest {
        child_id: "SandboxChild".to_owned(),
        parent_id: "Main".to_owned(),
        max_tools_per_agent: 32,
        depth: 1,
        definition,
        assignment: "probe the sandbox".to_owned(),
        system_prompt: "probe".to_owned(),
        requested_tool_names: None,
        orchestration_tools: Vec::new(),
        thinking_level: None,
        model: pi_ai::Model::default(),
        output_schema: None,
        schema_mode: None,
        yield_state: std::sync::Arc::new(pi_coding::YieldState::default()),
    };
    (factory)(request).await.expect("child session")
}

/// A sandboxed subagent child (`settings.orchestration.sandboxed`) runs its
/// process spawns inside the filesystem sandbox: it cannot read `/etc/passwd`
/// (deny-by-default filesystem) but can read and write its workspace (the
/// bind-mounted allowed path).
#[ignore = "live confinement test: requires Linux unprivileged user namespaces (run with --include-ignored on a supported kernel)"]
#[tokio::test]
async fn sandboxed_subagent_child_is_confined_to_its_workspace() {
    sandbox_usable().expect("live sandbox test refuses to fake-pass on an unsupported kernel");
    let workspace = TempDir::new().expect("workspace");
    let agent_dir = TempDir::new().expect("agent dir");
    let cwd = std::fs::canonicalize(workspace.path())
        .unwrap_or_else(|_| workspace.path().to_path_buf());

    let settings = Settings {
        sandbox: Some(SandboxSettings {
            enabled: Some(true),
            network: Some(false),
            allowed_paths: Some(vec![cwd.to_str().expect("utf8").to_owned()]),
            denied_paths: None,
            ..Default::default()
        }),
        orchestration: Some(OrchestrationSettings {
            sandboxed: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    };
    std::fs::write(
        agent_dir.path().join("settings.json"),
        serde_json::to_string(&settings).expect("serialize settings"),
    )
    .expect("write settings");

    let child = spawn_subagent_child(&cwd, agent_dir.path()).await;
    let bash = child
        .get_all_tools()
        .into_iter()
        .find(|tool| tool.name == "bash")
        .expect("child bash tool");

    // Deny-by-default: /etc/passwd is not visible in the child's sandbox.
    let denied = (bash.execute)(child_tool_ctx("cat /etc/passwd"))
        .await
        .expect_err("sandboxed child must not read /etc/passwd");
    assert!(
        denied.to_string().contains("No such file"),
        "expected /etc/passwd invisible to the sandboxed child: {denied}"
    );

    // The workspace is visible (bind-mounted allowed path) and writable.
    let probe = "child-workspace-probe";
    let written = (bash.execute)(child_tool_ctx(&format!("printf probe > {probe} && cat {probe}")))
        .await
        .expect("sandboxed child writes its workspace");
    assert!(
        text_of(&written).contains("probe"),
        "workspace write failed: {written:?}"
    );
    assert!(
        cwd.join(probe).is_file(),
        "the write must land in the host workspace (bind mount)"
    );
    std::fs::remove_file(cwd.join(probe)).expect("cleanup");
}

/// With `settings.orchestration.sandboxed` off (the default), subagent
/// children keep the current unsandboxed behavior: they can read `/etc/passwd`.
#[tokio::test]
async fn unsandboxed_subagent_child_keeps_current_behavior() {
    let workspace = TempDir::new().expect("workspace");
    let agent_dir = TempDir::new().expect("agent dir");
    let cwd = std::fs::canonicalize(workspace.path())
        .unwrap_or_else(|_| workspace.path().to_path_buf());

    // sandbox.enabled alone must NOT confine children: the opt-in flag is
    // `orchestration.sandboxed`, which stays off here.
    let settings = Settings {
        sandbox: Some(SandboxSettings {
            enabled: Some(true),
            network: Some(false),
            allowed_paths: Some(vec![cwd.to_str().expect("utf8").to_owned()]),
            denied_paths: None,
            ..Default::default()
        }),
        ..Default::default()
    };
    std::fs::write(
        agent_dir.path().join("settings.json"),
        serde_json::to_string(&settings).expect("serialize settings"),
    )
    .expect("write settings");

    let child = spawn_subagent_child(&cwd, agent_dir.path()).await;
    let bash = child
        .get_all_tools()
        .into_iter()
        .find(|tool| tool.name == "bash")
        .expect("child bash tool");
    let read = (bash.execute)(child_tool_ctx("cat /etc/passwd"))
        .await
        .expect("unsandboxed child reads /etc/passwd");
    assert!(
        text_of(&read).contains("root:"),
        "orchestration.sandboxed off must keep children unsandboxed: {read:?}"
    );
}
