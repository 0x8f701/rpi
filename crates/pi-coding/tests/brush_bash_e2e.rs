//! End-to-end coverage for the bash tool's embedded brush-shell default path
//! (OMP/pi parity) with REAL commands through the public `create_tool("bash",
//! …)` boundary.
//!
//! The in-crate unit tests prove the brush session model against the private
//! `bash_tool` helper; these tests drive the SAME engine through the public
//! tool boundary the agent runtime uses, with real commands and real
//! observable markers:
//!
//! * **brush execution** — `BASH_VERSION` is a well-known shell variable real
//!   bash sets; the embedded brush session skips well-known vars, so its
//!   absence proves the default (unsandboxed) path executed through brush,
//!   not a `/bin/bash` subprocess.
//! * **env rebuild** — live session metadata and `$PWD` mirror the
//!   subprocess path.
//! * **host guards** — builtins that would replace/stop/mutate the host
//!   process are refused with actionable errors.
//! * **fallback** — a command brush cannot parse falls back to the plain
//!   `/bin/bash` subprocess path for an identical observable result.
//! * **timeout reaping** — a bounded `sleep` times out through brush and the
//!   external child is reaped, not orphaned.
//!
//! Linux-only: brush's descendant reaping for timeout/abort relies on /proc,
//! so the in-process path is enabled on Linux only (elsewhere
//! `run_brush_command` reports `Fallback` and the subprocess path stays in
//! charge — documented policy in `tools/bash/brush.rs`).

#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use pi_agent::{AbortController, AgentTool, AgentToolResult, ToolCallContext, ToolUpdateFn};
use pi_ai::ContentBlock;
use pi_coding::{SessionEnvFn, create_tool, create_tool_with_session_env};
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

/// Contract: the default (unsandboxed) bash tool execution runs a REAL command
/// through the embedded brush shell. `BASH_VERSION` is absent in brush (the
/// session skips well-known vars), so `shell=brush` proves the in-process path
/// executed rather than a `/bin/bash` subprocess.
#[tokio::test]
async fn bash_real_command_runs_through_embedded_brush() {
    let cwd = TempDir::new().expect("cwd");
    let tool = create_tool("bash", cwd.path().to_str().expect("utf8")).expect("bash tool builds");
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

/// Contract: the brush session rebuilds the environment explicitly — live
/// session metadata (`PI_MODEL`) is visible and `$PWD` mirrors the working
/// directory, exactly like the subprocess path.
#[tokio::test]
async fn bash_brush_rebuilds_env_and_pwd() {
    let cwd = TempDir::new().expect("cwd");
    let session_env: SessionEnvFn = Arc::new(|| {
        HashMap::from([("PI_MODEL".to_owned(), "session-value".to_owned())])
    });
    let tool = create_tool_with_session_env(
        "bash",
        cwd.path().to_str().expect("utf8"),
        Some(session_env),
    )
    .expect("bash tool builds");
    let result = call(
        &tool,
        json!({ "command": "printf 'model=%s pwd=%s' \"${PI_MODEL:-unset}\" \"$PWD\"" }),
    )
    .await;
    assert_eq!(
        text_of(&result),
        format!("model=session-value pwd={}", cwd.path().display())
    );
}

/// Contract: builtins that would replace/stop/mutate the host process are
/// refused with an actionable message; legitimate signal listing passes
/// through, and `exec` inside a subshell still works (brush spawns a child).
#[tokio::test]
async fn bash_brush_rejects_host_dangerous_builtins() {
    let cwd = TempDir::new().expect("cwd");
    let tool = create_tool("bash", cwd.path().to_str().expect("utf8")).expect("bash tool builds");
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

/// Contract: a command brush cannot parse falls back to the plain `/bin/bash`
/// subprocess path, which reports bash's own syntax error (documented
/// fallback policy) — the observable result is unchanged for the caller.
#[tokio::test]
async fn bash_brush_falls_back_to_subprocess_when_parse_fails() {
    let cwd = TempDir::new().expect("cwd");
    let tool = create_tool("bash", cwd.path().to_str().expect("utf8")).expect("bash tool builds");
    let err = call_err(&tool, json!({ "command": "echo )" })).await;
    assert!(
        err.contains("syntax error"),
        "expected bash syntax error via the subprocess fallback: {err}"
    );
}

/// Contract: a bounded run that exceeds the tool timeout reports timed out and
/// brush reaps the external child it spawned (no orphaned `sleep` survives).
#[tokio::test]
async fn bash_brush_timeout_reaps_descendants() {
    let cwd = TempDir::new().expect("cwd");
    let tool = create_tool("bash", cwd.path().to_str().expect("utf8")).expect("bash tool builds");
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
    let tool = create_tool("bash", cwd.path().to_str().expect("utf8")).expect("bash tool builds");
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
    let tool = create_tool("bash", cwd.path().to_str().expect("utf8")).expect("bash tool builds");
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
/// timeout. A regression surfaces as a "timed out" error — never a suite
/// hang.
#[tokio::test]
async fn bash_pty_stdin_reaches_eof_without_input() {
    let cwd = TempDir::new().expect("cwd");
    let tool = create_tool("bash", cwd.path().to_str().expect("utf8")).expect("bash tool builds");
    let result = call(&tool, json!({ "command": "cat", "pty": true, "timeout": 10 })).await;
    assert!(
        text_of(&result).trim().is_empty(),
        "cat with closed stdin must produce no output: {:?}",
        text_of(&result)
    );
}
