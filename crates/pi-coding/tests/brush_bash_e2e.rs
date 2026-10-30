//! End-to-end coverage for the bash tool's embedded brush-shell default path
//! (OMP/pi parity) with REAL commands through the public tool boundary.
//!
//! The bash tool is governed: it fail-closes without a captured application
//! generation ("bash requires an application generation"). These tests drive
//! the SAME engine the agent runtime uses — the bash tool retrieved from a
//! hermetic published `Application` generation (danger-full-access sandbox, no
//! bubblewrap dependency) — with real commands and real observable markers:
//!
//! * **brush execution** — `BASH_VERSION` is a well-known shell variable real
//!   bash sets; the embedded brush session skips well-known vars, so its
//!   absence proves the default path executed through brush, not a
//!   `/bin/bash` subprocess.
//! * **env rebuild** — `$PWD` mirrors the working directory, exactly like the
//!   subprocess path.
//! * **host guards** — builtins that would replace/stop/mutate the host
//!   process are refused with actionable errors.
//! * **fallback** — a command brush cannot parse requires the governed bash
//!   subprocess path rather than escaping to an ambient host launch.
//! * **timeout reaping** — a bounded `sleep` times out through brush and the
//!   external child is reaped, not orphaned.
//!
//! Linux-only: brush's descendant reaping for timeout/abort relies on /proc,
//! so the in-process path is enabled on Linux only (elsewhere
//! `run_brush_command` reports `Fallback` and the subprocess path stays in
//! charge — documented policy in `tools/bash/brush.rs`).
//!
//! PTY cases use the same published generation: scratch spill and canonical
//! bash executable authority come from the production foreground-shell path.

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use pi_agent::{
    AbortController, AgentTool, AgentToolResult, ThinkingLevel, ToolCallContext, ToolUpdateFn,
};
use pi_ai::providers::{FauxProviderOptions, register_faux_provider};
use pi_ai::{ContentBlock, Model, SimpleStreamOptions};
use pi_coding::{
    Application, ApplicationRuntimeCandidate, ResourceDiscovery, ResourceManager,
    ResourceManagerOptions, Session, SessionOptions, ToolSelection,
};
use serde_json::{Value, json};
use tempfile::TempDir;

fn noop_update() -> ToolUpdateFn {
    Arc::new(|_result: AgentToolResult| {})
}

fn make_ctx(arguments: Value) -> ToolCallContext {
    let (controller, abort) = AbortController::new();
    std::mem::forget(controller);
    ToolCallContext {
        tool_call_id: "brush-bash-e2e".to_owned(),
        arguments,
        on_update: noop_update(),
        abort,
        model: None,
        approval_token: None,
    }
}

fn text_of(result: &AgentToolResult) -> String {
    match result.content.first() {
        Some(ContentBlock::Text { text, .. }) => text.clone(),
        _ => String::new(),
    }
}

async fn call(tool: &AgentTool, arguments: Value) -> AgentToolResult {
    (tool.execute)(make_ctx(arguments))
        .await
        .unwrap_or_else(|error| panic!("bash call failed: {error:#}"))
}

async fn call_err(tool: &AgentTool, arguments: Value) -> String {
    (tool.execute)(make_ctx(arguments))
        .await
        .expect_err("expected bash tool error")
        .to_string()
}

fn session_options(cwd: &Path, model: Model) -> SessionOptions {
    SessionOptions {
        model,
        cwd: cwd.to_path_buf(),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: "faux".to_owned(),
        compaction: None,
        stream_options: SimpleStreamOptions::default(),
        tools: None,
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: None,
        auth_resolver: None,
        auth_manager: None,
    }
}

/// Agent configuration and SessionStore directories kept OUTSIDE the writable
/// `cwd` workspace and held alive for the application's lifetime, so the
/// published generation's revision scratch directories (created by the normal
/// Application bootstrap under the agent home) survive every tool call. A
/// process-lifetime tempdir root hosts per-call subdirs (hermetic
/// published-generation pattern used across the crate's integration suites).
fn external_config_roots() -> (PathBuf, PathBuf) {
    static ROOT: LazyLock<TempDir> =
        LazyLock::new(|| tempfile::tempdir().expect("brush bash e2e config root"));
    let suffix = uuid::Uuid::now_v7().to_string();
    let base = ROOT.path().join(suffix);
    let agent_dir = base.join("agent");
    let session_dir = base.join("sessions");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    (agent_dir, session_dir)
}

/// Builds a hermetic published `Application` generation (danger-full-access
/// sandbox, no extensions/skills/themes/context-files, faux provider) and
/// returns it together with the bash tool retrieved from the application's
/// tool catalog. The returned `Application` and the shared config `TempDir`
/// must outlive every tool call.
async fn bash_tool(cwd: &Path) -> (Application, AgentTool) {
    let suffix = uuid::Uuid::now_v7().to_string();
    let api = format!("brush-bash-e2e-api-{suffix}");
    let provider = format!("brush-bash-e2e-provider-{suffix}");
    let model = Model {
        id: "brush-bash-e2e".to_owned(),
        name: "Brush Bash E2E".to_owned(),
        api: api.clone(),
        provider: provider.clone(),
        ..Model::default()
    };
    let _registration = register_faux_provider(FauxProviderOptions {
        api,
        provider,
        models: vec![model.clone()],
        chunk_size: 1,
    });
    let (agent_dir, session_dir) = external_config_roots();
    std::fs::write(
        agent_dir.join("settings.json"),
        r#"{"sandbox":{"mode":"danger-full-access"}}"#,
    )
    .expect("danger settings");
    let mut resource_options = ResourceManagerOptions::new(cwd);
    resource_options.agent_dir = agent_dir;
    resource_options.disable_extensions = true;
    resource_options.disable_skills = true;
    resource_options.disable_prompt_templates = true;
    resource_options.disable_themes = true;
    resource_options.disable_context_files = true;
    let resources = ResourceManager::new(resource_options).expect("resources");
    let session = Session::new_with_additional_tools_filtered_and_discovery(
        session_options(cwd, model.clone()),
        Vec::new(),
        ToolSelection::default(),
        ResourceDiscovery::Disabled,
    )
    .expect("session");
    session.set_session_dir(session_dir);
    session
        .attach_resources(resources)
        .await
        .expect("attach resources");
    let application =
        Application::from_runtime_candidate(ApplicationRuntimeCandidate::new(session))
            .await
            .expect("publish application generation");
    let bash = application
        .get_all_tools()
        .into_iter()
        .find(|tool| tool.name == "bash")
        .expect("bash tool is in the published application catalog");
    (application, bash)
}

/// Contract: the default (unsandboxed) bash tool execution runs a REAL command
/// through the embedded brush shell. `BASH_VERSION` is absent in brush (the
/// session skips well-known vars), so `shell=brush` proves the in-process path
/// executed rather than a `/bin/bash` subprocess.
#[tokio::test]
async fn bash_real_command_runs_through_embedded_brush() {
    let cwd = TempDir::new().expect("cwd");
    let (_application, tool) = bash_tool(cwd.path()).await;
    let result = call(
        &tool,
        json!({ "command": "printf 'shell=%s' \"${BASH_VERSION:-brush}\"" }),
    )
    .await;
    assert_eq!(
        text_of(&result),
        "shell=brush",
        "BASH_VERSION absence proves the embedded brush path ran"
    );
}

/// Contract: the brush session rebuilds the environment explicitly — `$PWD`
/// mirrors the working directory, exactly like the subprocess path. The
/// application-catalog bash tool exposes no PI_* session metadata (matches the
/// standalone guard), so the live-metadata contract under test is the working
/// directory rebuild the embedded brush session always mirrors.
#[tokio::test]
async fn bash_brush_rebuilds_env_and_pwd() {
    let cwd = TempDir::new().expect("cwd");
    let (_application, tool) = bash_tool(cwd.path()).await;
    let result = call(&tool, json!({ "command": "printf 'pwd=%s' \"$PWD\"" })).await;
    assert_eq!(
        text_of(&result),
        format!("pwd={}", cwd.path().display()),
        "brush must rebuild $PWD to the working directory"
    );
}

/// Contract: builtins that would replace/stop/mutate the host process are
/// refused with an actionable message; legitimate signal listing passes
/// through, and `exec` inside a subshell still works (brush spawns a child).
#[tokio::test]
async fn bash_brush_rejects_host_dangerous_builtins() {
    let cwd = TempDir::new().expect("cwd");
    let (_application, tool) = bash_tool(cwd.path()).await;
    for command in ["exec sleep 0.1", "suspend", "ulimit -n", "umask 077"] {
        let err = call_err(&tool, json!({ "command": command })).await;
        assert!(
            err.contains("not supported in the embedded brush shell"),
            "{command}: {err}"
        );
    }
    // kill of the host pid ($$ is the rpi process in-process) is refused…
    let err = call_err(&tool, json!({ "command": "kill -9 $$" })).await;
    assert!(err.contains("refusing to signal the host process"), "{err}");
    // …but legitimate kill uses pass through the guarded builtin.
    let result = call(&tool, json!({ "command": "kill -l" })).await;
    assert!(text_of(&result).contains("SIGTERM"), "{}", text_of(&result));
    // `exec` inside a subshell still works (brush spawns a child there).
    let result = call(&tool, json!({ "command": "(exec echo subshell-ok)" })).await;
    assert_eq!(text_of(&result), "subshell-ok\n");
}

/// Contract: a command rejected by the embedded parser reaches governed Bash
/// syntax handling instead of an ambient host launch or the removed stub.
#[tokio::test]
async fn bash_brush_parse_failure_runs_governed_foreground_fallback() {
    let cwd = TempDir::new().expect("cwd");
    let (_application, tool) = bash_tool(cwd.path()).await;
    let error = call_err(&tool, json!({ "command": "echo )" })).await;

    assert!(error.contains("syntax error"), "{error}");
    assert!(
        !error.contains("requires governed subprocess execution"),
        "{error}"
    );
}

/// Contract: bash-only syntax the embedded parser rejects still runs through
/// the governed non-PTY fallback, including ordinary host environment.
#[tokio::test]
async fn bash_unparsable_command_runs_governed_foreground_fallback() {
    let cwd = TempDir::new().expect("cwd");
    let (_application, tool) = bash_tool(cwd.path()).await;
    let result = call(
        &tool,
        json!({ "command": "printf 'fallback-ok\\n' |& cat" }),
    )
    .await;
    let text = text_of(&result);
    assert!(
        text.contains("fallback-ok"),
        "governed fallback must execute bash-only syntax: {text}"
    );
}

/// Contract: timeout on the governed non-PTY fallback reports timed out and
/// cannot hang or replace that outcome with a reader I/O error.
#[tokio::test]
async fn bash_foreground_fallback_timeout_stays_timed_out() {
    let cwd = TempDir::new().expect("cwd");
    let (_application, tool) = bash_tool(cwd.path()).await;
    let started = std::time::Instant::now();
    let err = call_err(
        &tool,
        json!({ "command": "sleep 30 |& cat", "timeout": 0.5 }),
    )
    .await;
    assert!(err.contains("timed out"), "{err}");
    assert!(
        !err.contains("output reader failed"),
        "timeout must not be replaced by a reader I/O error: {err}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "fallback timeout cleanup must not hang: {:?}",
        started.elapsed()
    );
}

/// Contract: a bounded run that exceeds the tool timeout reports timed out and
/// brush reaps the external child it spawned (no orphaned `sleep` survives).
#[tokio::test]
async fn bash_brush_timeout_reaps_descendants() {
    let cwd = TempDir::new().expect("cwd");
    let (_application, tool) = bash_tool(cwd.path()).await;
    let err = call_err(&tool, json!({ "command": "sleep 30", "timeout": 0.5 })).await;
    assert!(err.contains("timed out"), "{err}");
    // The external child must be reaped. Give brush a moment to finish the
    // process-group teardown; if a `sleep 30` is still alive, the reaping
    // regressed.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let survivors = std::process::Command::new("pgrep")
            .arg("-f")
            .arg("^sleep 30$")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if !survivors {
            break;
        }
        if std::time::Instant::now() >= deadline {
            panic!("brush timeout left an orphaned `sleep 30` behind");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // The tool stays usable after the timeout.
    let result = call(&tool, json!({ "command": "printf ok" })).await;
    assert_eq!(text_of(&result), "ok");
}

/// Contract: `pty: true` runs a simple command in a pseudo-terminal and
/// returns its merged stdout+stderr through the normal success path. The PTY
/// line discipline may fold newlines (`\n` → `\r\n`), so the assertion uses
/// `contains` rather than exact text.
#[tokio::test]
async fn bash_pty_runs_simple_command() {
    let cwd = TempDir::new().expect("cwd");
    let (_application, tool) = bash_tool(cwd.path()).await;
    let result = call(&tool, json!({ "command": "printf 'pty-ok'", "pty": true })).await;
    assert!(
        text_of(&result).contains("pty-ok"),
        "pty command output: {:?}",
        text_of(&result)
    );
}

/// Contract: `input` is delivered to the PTY's stdin (followed by a newline)
/// before output is read, so an interactive command that reads stdin sees it —
/// the sudo-password path. PTY echo is on by default, so the input may appear
/// in the merged output as well; the assertion targets the command's own
/// reply.
#[tokio::test]
async fn bash_pty_forwards_input_to_stdin() {
    let cwd = TempDir::new().expect("cwd");
    let (_application, tool) = bash_tool(cwd.path()).await;
    let result = call(
        &tool,
        json!({
            "command": "read line; printf 'got:%s' \"$line\"",
            "pty": true,
            "input": "secret-line",
            "timeout": 10
        }),
    )
    .await;
    assert!(
        text_of(&result).contains("got:secret-line"),
        "pty input reply: {:?}",
        text_of(&result)
    );
}

/// Contract: with no `input`, the PTY's stdin still reaches EOF, so a command
/// that reads stdin to EOF (`cat`) completes instead of hanging until the
/// timeout. The bash tool presents a successful empty stream as `(no output)`;
/// a regression surfaces as a "timed out" error — never a suite hang.
#[tokio::test]
async fn bash_pty_stdin_reaches_eof_without_input() {
    let cwd = TempDir::new().expect("cwd");
    let (_application, tool) = bash_tool(cwd.path()).await;
    let result = call(
        &tool,
        json!({ "command": "cat", "pty": true, "timeout": 10 }),
    )
    .await;
    assert_eq!(
        text_of(&result),
        "(no output)",
        "cat with closed stdin must finish with the canonical empty output"
    );
}
