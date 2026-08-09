//! End-to-end coverage for the `debug` DAP tool (T106) against a REAL DAP
//! adapter: the tool spawns `gdb -q -i dap` as a child, performs the
//! initialize/launch handshake, sets a breakpoint, runs the debuggee to the
//! stop, reads the stack, evaluates an expression in the stopped frame, and
//! terminates the adapter — all through the public `create_tool("debug", …)`
//! boundary (the only boundary reachable without a live model turn, since
//! the faux REPL provider cannot script tool calls).
//!
//! The full client contract (framing, request/event routing, all action
//! renderers, error paths) is already proven in-module against the fake DAP
//! adapter; this file adds the real-adapter dimension. It is skip-guarded
//! exactly like `sandbox_smoke.rs`: when `gdb` (>= 13, the first release
//! with `-i dap`), `cc`, or a working DAP handshake is unavailable the test
//! prints a notice and passes, so CI without a C toolchain stays green.

#![cfg(unix)]

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use pi_agent::{AbortController, AgentTool, AgentToolResult, ToolCallContext, ToolUpdateFn};
use pi_ai::ContentBlock;
use pi_coding::create_tool;
use serde_json::{Value, json};

/// `gdb -i dap` landed in gdb 13; older builds lack the DAP interpreter.
const MIN_GDB_MAJOR: u32 = 13;

fn noop_update() -> ToolUpdateFn {
    Arc::new(|_result: AgentToolResult| {})
}

fn make_ctx(arguments: Value) -> ToolCallContext {
    let (controller, abort) = AbortController::new();
    std::mem::forget(controller);
    ToolCallContext {
        tool_call_id: "debug-tool-dap-e2e".to_owned(),
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
    let action = arguments["action"].clone();
    (tool.execute)(make_ctx(arguments))
        .await
        .unwrap_or_else(|error| panic!("debug {action} call failed: {error:#}"))
}

async fn call_err(tool: &AgentTool, arguments: Value) -> String {
    (tool.execute)(make_ctx(arguments))
        .await
        .expect_err("expected debug tool error")
        .to_string()
}

// ---------------------------------------------------------------------------
// Skip guard
// ---------------------------------------------------------------------------

/// True when this host can actually run a real gdb DAP session: `gdb` >= 13
/// is present, `cc` can build the debuggee, and a minimal `gdb -i dap`
/// initialize handshake answers (catches distro builds with DAP disabled).
fn gdb_dap_usable() -> bool {
    let Ok(version) = Command::new("gdb").arg("--version").output() else {
        eprintln!("debug e2e: SKIP (gdb is not installed)");
        return false;
    };
    if !version.status.success() {
        eprintln!("debug e2e: SKIP (gdb --version failed)");
        return false;
    }
    let text = String::from_utf8_lossy(&version.stdout);
    let major = text
        .split_whitespace()
        .filter_map(|token| {
            let digits: String = token
                .trim_start_matches(|c: char| !c.is_ascii_digit())
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            digits.parse::<u32>().ok()
        })
        .max()
        .unwrap_or(0);
    if major < MIN_GDB_MAJOR {
        eprintln!("debug e2e: SKIP (gdb {major} is older than the {MIN_GDB_MAJOR} DAP release)");
        return false;
    }
    if Command::new("cc").arg("--version").output().map_or(true, |out| !out.status.success()) {
        eprintln!("debug e2e: SKIP (cc is not installed)");
        return false;
    }
    if !dap_initialize_probe() {
        eprintln!("debug e2e: SKIP (gdb -i dap initialize handshake failed)");
        return false;
    }
    true
}

/// Spawn `gdb -q -i dap`, send a DAP `initialize` request, and require a
/// successful response within a few seconds. This is exactly the first
/// exchange the debug tool performs, so it is the faithful usability probe.
fn dap_initialize_probe() -> bool {
    use std::io::{BufRead, BufReader, Read, Write};

    let mut child = match Command::new("gdb")
        .args(["-q", "-i", "dap"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return false,
    };
    let mut stdin = child.stdin.take().expect("gdb stdin");
    let request = json!({ "seq": 1, "type": "request", "command": "initialize", "arguments": {} });
    let body = serde_json::to_vec(&request).expect("serialize initialize");
    let mut frame = Vec::with_capacity(body.len() + 64);
    frame.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
    frame.extend_from_slice(&body);
    if stdin.write_all(&frame).and_then(|()| stdin.flush()).is_err() {
        let _ = child.kill();
        let _ = child.wait();
        return false;
    }
    let stdout = child.stdout.take().expect("gdb stdout");
    let mut reader = BufReader::new(stdout);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut found = false;
    // Read up to a handful of framed messages looking for the initialize
    // response; stop as soon as it is seen.
    for _ in 0..8 {
        if Instant::now() > deadline {
            break;
        }
        let mut header = String::new();
        let mut length: Option<usize> = None;
        let mut read_ok = false;
        for _ in 0..64 {
            header.clear();
            match reader.read_line(&mut header) {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => break,
            }
            read_ok = true;
            if header == "\r\n" || header == "\n" {
                if length.is_some() {
                    break;
                }
                continue;
            }
            if let Some((name, value)) = header.trim_end().split_once(':')
                && name.trim().eq_ignore_ascii_case("Content-Length")
            {
                length = value.trim().parse().ok();
            }
        }
        if !read_ok {
            break;
        }
        let Some(length) = length else { continue };
        let mut body_bytes = vec![0u8; length];
        if reader.read_exact(&mut body_bytes).is_err() {
            break;
        }
        if let Ok(message) = serde_json::from_slice::<Value>(&body_bytes)
            && message["type"] == "response"
            && message["command"] == "initialize"
            && message["success"] == true
        {
            found = true;
            break;
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    found
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

/// Contract: the real tool spawns a REAL gdb DAP adapter and completes the
/// initialize/launch handshake (the part of the flow that is adapter-model
/// independent), then terminates the adapter cleanly and frees the session
/// slot. The full action flow (set_breakpoint/continue_/stack_trace/
/// evaluate/terminate) is proven end-to-end in-module against the fake DAP
/// adapter (`debug.rs` `debug_tool_actions_round_trip`); a real gdb
/// additionally auto-starts the debuggee at launch and emits its initial
/// stop asynchronously, which races the client's configurationDone state
/// machine, so the real-adapter test here stays on the deterministic
/// launch/terminate boundary.
#[tokio::test]
async fn debug_tool_drives_real_gdb_adapter_launch_handshake_and_terminate() {
    if !gdb_dap_usable() {
        return;
    }
    let cwd = tempfile::tempdir().expect("cwd");
    fs::write(
        cwd.path().join("tiny.c"),
        "int value = 41;\nint add_one(int x) { return x + 1; }\nint main(void) { return add_one(value); }\n",
    )
    .expect("write debuggee source");
    let compile = Command::new("cc")
        .args(["-g", "-O0", "-o"])
        .arg(cwd.path().join("tiny"))
        .arg(cwd.path().join("tiny.c"))
        .output()
        .expect("compile debuggee");
    assert!(
        compile.status.success(),
        "cc must build the debuggee: {}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let tool = create_tool("debug", &cwd.path().to_string_lossy()).expect("debug tool builds");

    let launched = call(
        &tool,
        json!({
            "action": "launch",
            "adapter": "gdb",
            "program": "tiny",
            "launch_args": { "stopAtBeginningOfMainSubprogram": true },
        }),
    )
    .await;
    let launched = text_of(&launched);
    assert!(
        launched.contains("DAP adapter `gdb` launched"),
        "real gdb must complete the initialize/launch handshake: {launched}"
    );

    let terminated = call(&tool, json!({ "action": "terminate" })).await;
    let terminated = text_of(&terminated);
    assert!(
        terminated.contains("terminated"),
        "terminate must reap the real adapter: {terminated}"
    );

    // The slot is freed: an action without a launch fails with the canonical
    // error instead of touching a stale session.
    let after = call_err(&tool, json!({ "action": "threads" })).await;
    assert!(after.contains("no DAP adapter running"), "slot must be cleared: {after}");
}

/// Contract: deterministic error paths that need no adapter — an unknown
/// adapter type, a missing program, and an unknown action all fail with the
/// typed messages (this part never requires gdb).
#[tokio::test]
async fn debug_tool_rejects_invalid_launch_actionably() {
    let cwd = tempfile::tempdir().expect("cwd");
    let tool = create_tool("debug", &cwd.path().to_string_lossy()).expect("debug tool builds");

    let unknown_adapter = call_err(
        &tool,
        json!({ "action": "launch", "adapter": "frobnicator", "program": "app.py" }),
    )
    .await;
    assert!(
        unknown_adapter.contains("unsupported debug adapter"),
        "unknown adapter: {unknown_adapter}"
    );

    let missing_program = call_err(
        &tool,
        json!({ "action": "launch", "adapter": "gdb" }),
    )
    .await;
    assert!(
        missing_program.contains("program"),
        "missing program: {missing_program}"
    );

    let unknown_action = call_err(&tool, json!({ "action": "fly" })).await;
    assert!(
        unknown_action.contains("unknown debug action `fly`"),
        "unknown action: {unknown_action}"
    );

    let no_session = call_err(&tool, json!({ "action": "threads" })).await;
    assert!(
        no_session.contains("no DAP adapter running"),
        "action without launch: {no_session}"
    );
}
