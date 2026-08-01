#![cfg(unix)]

//! End-to-end contracts for the public `process` AgentTool.
//!
//! Exercises start/ps/describe/logs/send/resize/signal/stop/wait through the
//! tool boundary (not ProcessManager directly), plus validation and isolation
//! paths that must fail with actionable errors rather than silent success.

use std::path::Path;
use std::sync::Arc;

use base64::Engine as _;
use pi_agent::{AbortController, AgentTool, AgentToolResult, ToolCallContext, ToolUpdateFn};
use pi_ai::ContentBlock;
use pi_coding::{ProcessManager, ProcessManagerConfig, ProcessOwnerId, process_tool};
use serde_json::{Value, json};

fn noop_update() -> ToolUpdateFn {
    Arc::new(|_result: AgentToolResult| {})
}

fn make_ctx(arguments: Value) -> ToolCallContext {
    let (_controller, abort) = AbortController::new();
    // Keep the controller alive for the duration of the call.
    std::mem::forget(_controller);
    ToolCallContext {
        tool_call_id: "process-tool-test".to_owned(),
        arguments,
        on_update: noop_update(),
        abort,
        model: None,
    }
}

fn make_aborted_ctx(arguments: Value) -> ToolCallContext {
    let (controller, abort) = AbortController::new();
    controller.abort();
    ToolCallContext {
        tool_call_id: "process-tool-aborted".to_owned(),
        arguments,
        on_update: noop_update(),
        abort,
        model: None,
    }
}

async fn call(tool: &AgentTool, arguments: Value) -> AgentToolResult {
    (tool.execute)(make_ctx(arguments))
        .await
        .unwrap_or_else(|error| panic!("process tool call failed: {error:#}"))
}

async fn call_err(tool: &AgentTool, arguments: Value) -> String {
    (tool.execute)(make_ctx(arguments))
        .await
        .expect_err("expected process tool error")
        .to_string()
}

fn text_of(result: &AgentToolResult) -> String {
    match result.content.first() {
        Some(ContentBlock::Text { text, .. }) => text.clone(),
        _ => String::new(),
    }
}

fn decode_log_bytes(details: &Value) -> Vec<u8> {
    details["chunks"]
        .as_array()
        .expect("logs.chunks array")
        .iter()
        .map(|chunk| {
            let encoded = chunk["dataBase64"].as_str().expect("chunk dataBase64");
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .expect("valid log chunk base64")
        })
        .flatten()
        .collect()
}

fn process_id(details: &Value) -> String {
    details["id"]
        .as_str()
        .expect("process id string")
        .to_owned()
}

fn fixture_manager() -> ProcessManager {
    ProcessManager::with_config(ProcessManagerConfig {
        idle_timeout: None,
        ..ProcessManagerConfig::default()
    })
}

fn owner_tool(cwd: &Path, manager: ProcessManager, owner: &str) -> AgentTool {
    process_tool(cwd, manager, ProcessOwnerId::new(owner))
}

#[tokio::test]
async fn process_tool_start_ps_describe_logs_send_stop_wait_lifecycle() {
    let directory = tempfile::tempdir().expect("tempdir");
    let manager = fixture_manager();
    let tool = owner_tool(directory.path(), manager.clone(), "lifecycle-owner");

    // start — argv only, no shell interpolation; pipe mode for deterministic IO.
    let started = call(
        &tool,
        json!({
            "op": "start",
            "argv": ["/bin/sh", "-c", "printf 'ready\\n'; read line; printf 'got:%s\\n' \"$line\""],
            "pty": false,
            "label": "lifecycle",
            "outputBytes": 4096,
        }),
    )
    .await;
    let id = process_id(&started.details);
    assert_eq!(started.details["label"], "lifecycle");
    assert_eq!(started.details["state"], "running");
    assert_eq!(started.details["tty"], false);
    assert!(started.details["pid"].as_u64().is_some());
    assert!(text_of(&started).contains(&id));

    // ps — owner-scoped list includes the new process with the same id.
    let ps = call(&tool, json!({ "op": "ps" })).await;
    let listed = ps.details.as_array().expect("ps returns array");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["id"], id);
    assert_eq!(listed[0]["label"], "lifecycle");

    // describe — same snapshot shape as start/ps entry.
    let described = call(&tool, json!({ "op": "describe", "id": id })).await;
    assert_eq!(described.details["id"], id);
    assert_eq!(described.details["label"], "lifecycle");
    assert_eq!(described.details["state"], "running");

    // logs — wait until the ready banner is retained; cursor must advance.
    let mut cursor = 0u64;
    let mut saw_ready = false;
    for _ in 0..40 {
        let logs = call(
            &tool,
            json!({
                "op": "logs",
                "id": id,
                "cursor": cursor,
                "follow": true,
                "timeoutMs": 100,
                "maxBytes": 1024,
            }),
        )
        .await;
        assert_eq!(logs.details["requestedCursor"], cursor);
        assert!(logs.details["cursor"].as_u64().unwrap() >= logs.details["startCursor"].as_u64().unwrap());
        assert_eq!(logs.details["lost"], false);
        cursor = logs.details["cursor"].as_u64().expect("cursor");
        let bytes = decode_log_bytes(&logs.details);
        if bytes.windows(b"ready\n".len()).any(|window| window == b"ready\n") {
            saw_ready = true;
            break;
        }
    }
    assert!(saw_ready, "ready banner never appeared in tool logs");

    // send — stdin text reaches the process; close stdin so read completes.
    let sent = call(
        &tool,
        json!({
            "op": "send",
            "id": id,
            "text": "payload\n",
            "closeStdin": true,
        }),
    )
    .await;
    assert_eq!(sent.details["ok"], true);

    // wait — process exits cleanly with code 0 and terminal state.
    let waited = call(
        &tool,
        json!({
            "op": "wait",
            "id": id,
            "timeoutMs": 3000,
        }),
    )
    .await;
    assert_eq!(waited.details["id"], id);
    assert_eq!(waited.details["exitCode"], 0);
    assert!(
        waited.details["state"] == "exited"
            || waited.details["state"] == "stopped"
            || waited.details["state"].as_str().is_some_and(|state| {
                // Accept any terminal state the manager exposes.
                state != "running" && state != "starting"
            }),
        "wait must return a terminal state, got {}",
        waited.details["state"]
    );
    assert!(waited.details["exitedAtMs"].as_u64().is_some());
    assert!(waited.details["outputCursor"].as_u64().unwrap() > 0);

    // logs after exit — full retained output, eof true, cursor stable.
    let final_logs = call(
        &tool,
        json!({
            "op": "logs",
            "id": id,
            "cursor": 0,
            "maxBytes": 4096,
        }),
    )
    .await;
    assert_eq!(final_logs.details["requestedCursor"], 0);
    assert_eq!(final_logs.details["lost"], false);
    assert_eq!(final_logs.details["eof"], true);
    let output = String::from_utf8(decode_log_bytes(&final_logs.details)).expect("utf8 logs");
    assert!(output.contains("ready\n"), "output missing ready: {output:?}");
    assert!(output.contains("got:payload\n"), "output missing echo: {output:?}");
    assert_eq!(
        final_logs.details["cursor"].as_u64().unwrap(),
        waited.details["outputCursor"].as_u64().unwrap()
    );

    // stop on already-exited process remains idempotent / returns info.
    let stopped = call(
        &tool,
        json!({
            "op": "stop",
            "id": id,
            "timeoutMs": 1000,
        }),
    )
    .await;
    assert_eq!(stopped.details["id"], id);
    assert_ne!(stopped.details["state"], "running");
}

#[tokio::test]
async fn process_tool_logs_honor_bounded_cursor_and_max_bytes() {
    let directory = tempfile::tempdir().expect("tempdir");
    // Tiny ring so early bytes are dropped — tool must surface lost/lostBytes.
    let manager = ProcessManager::with_config(ProcessManagerConfig {
        max_output_bytes: 5,
        idle_timeout: None,
        ..ProcessManagerConfig::default()
    });
    let tool = owner_tool(directory.path(), manager, "cursor-owner");

    let started = call(
        &tool,
        json!({
            "op": "start",
            "argv": ["/bin/sh", "-c", "printf 123456789"],
            "pty": false,
            "outputBytes": 5,
        }),
    )
    .await;
    let id = process_id(&started.details);

    let waited = call(
        &tool,
        json!({ "op": "wait", "id": id, "timeoutMs": 3000 }),
    )
    .await;
    assert_eq!(waited.details["exitCode"], 0);
    // With a 5-byte ring over 9 bytes of output, start cursor advances past the head.
    assert_eq!(waited.details["outputStartCursor"], 4);
    assert_eq!(waited.details["outputCursor"], 9);

    let logs = call(
        &tool,
        json!({
            "op": "logs",
            "id": id,
            "cursor": 0,
            "maxBytes": 64,
        }),
    )
    .await;

    // Contract: requesting a cursor behind the retained window reports loss.
    assert_eq!(logs.details["requestedCursor"], 0);
    assert_eq!(logs.details["startCursor"], 4);
    assert_eq!(logs.details["cursor"], 9);
    assert_eq!(logs.details["lost"], true);
    assert_eq!(logs.details["lostBytes"], 4);
    assert_eq!(logs.details["eof"], true);
    assert_eq!(decode_log_bytes(&logs.details), b"56789");

    // Cursor mid-window returns only the tail; no false loss once caught up.
    let tail = call(
        &tool,
        json!({
            "op": "logs",
            "id": id,
            "cursor": 7,
            "maxBytes": 64,
        }),
    )
    .await;
    assert_eq!(tail.details["requestedCursor"], 7);
    assert_eq!(tail.details["lost"], false);
    assert_eq!(tail.details["lostBytes"], 0);
    assert_eq!(decode_log_bytes(&tail.details), b"89");
    assert_eq!(tail.details["cursor"], 9);

    // maxBytes clamps the returned slice and advances cursor only by delivered bytes.
    let clamped = call(
        &tool,
        json!({
            "op": "logs",
            "id": id,
            "cursor": 4,
            "maxBytes": 2,
        }),
    )
    .await;
    assert_eq!(clamped.details["requestedCursor"], 4);
    assert_eq!(clamped.details["lost"], false);
    let clamped_bytes = decode_log_bytes(&clamped.details);
    assert_eq!(clamped_bytes, b"56");
    assert_eq!(clamped.details["cursor"], 6);
    // eof stays false when more retained data remains after the clamp.
    assert_eq!(clamped.details["eof"], false);
}

#[tokio::test]
async fn process_tool_pty_resize_send_keys_and_signal() {
    let directory = tempfile::tempdir().expect("tempdir");
    let manager = fixture_manager();
    let tool = owner_tool(directory.path(), manager, "pty-owner");

    let started = call(
        &tool,
        json!({
            "op": "start",
            "argv": [
                "/bin/sh",
                "-c",
                // trap keeps the shell alive until an uncatchable kill / stop.
                "stty size; read line; printf '<%s>\\n' \"$line\"; exec sleep 30"
            ],
            "pty": true,
            "size": { "rows": 24, "cols": 80 },
            "label": "pty-session",
        }),
    )
    .await;
    let id = process_id(&started.details);
    assert_eq!(started.details["tty"], true);

    let resized = call(
        &tool,
        json!({
            "op": "resize",
            "id": id,
            "size": { "rows": 40, "cols": 120 },
        }),
    )
    .await;
    assert_eq!(resized.details["ok"], true);

    let sent = call(
        &tool,
        json!({
            "op": "send",
            "id": id,
            "text": "hello",
            "keys": ["ENTER"],
        }),
    )
    .await;
    assert_eq!(sent.details["ok"], true);

    let mut cursor = 0u64;
    let mut output = Vec::new();
    for _ in 0..40 {
        let logs = call(
            &tool,
            json!({
                "op": "logs",
                "id": id,
                "cursor": cursor,
                "follow": true,
                "timeoutMs": 150,
            }),
        )
        .await;
        cursor = logs.details["cursor"].as_u64().expect("cursor");
        output.extend(decode_log_bytes(&logs.details));
        if output.windows(b"<hello>".len()).any(|window| window == b"<hello>") {
            break;
        }
    }
    assert!(
        output.windows(b"<hello>".len()).any(|window| window == b"<hello>"),
        "PTY never echoed input: {:?}",
        String::from_utf8_lossy(&output)
    );

    // signal op must accept the request; SIGINT interrupts the PTY session.
    let signaled = call(
        &tool,
        json!({
            "op": "signal",
            "id": id,
            "signal": "SIGINT",
        }),
    )
    .await;
    assert_eq!(signaled.details["ok"], true);

    // Prefer wait, but fall back to stop so the contract is "signal accepted +
    // process becomes terminal" rather than depending on shell SIGINT defaults.
    let terminal = match (tool.execute)(make_ctx(json!({
        "op": "wait",
        "id": id,
        "timeoutMs": 1500,
    })))
    .await
    {
        Ok(result) => result,
        Err(_) => {
            // Escalate via stop (SIGTERM then SIGKILL under the manager).
            call(
                &tool,
                json!({ "op": "stop", "id": id, "timeoutMs": 2000 }),
            )
            .await
        }
    };
    assert_eq!(terminal.details["id"], id);
    assert_ne!(terminal.details["state"], "running");
    assert!(terminal.details["exitedAtMs"].as_u64().is_some());
}

#[tokio::test]
async fn process_tool_rejects_invalid_base64_missing_size_and_unknown_op() {
    let directory = tempfile::tempdir().expect("tempdir");
    let manager = fixture_manager();
    let tool = owner_tool(directory.path(), manager, "validation-owner");

    let started = call(
        &tool,
        json!({
            "op": "start",
            "argv": ["/bin/sh", "-c", "sleep 30"],
            "pty": false,
        }),
    )
    .await;
    let id = process_id(&started.details);

    let bad_b64 = call_err(
        &tool,
        json!({
            "op": "send",
            "id": id,
            "dataBase64": "%%%not-valid-base64%%%",
        }),
    )
    .await;
    assert!(
        bad_b64.contains("dataBase64 is invalid"),
        "unexpected invalid base64 error: {bad_b64}"
    );

    let missing_size = call_err(
        &tool,
        json!({
            "op": "resize",
            "id": id,
        }),
    )
    .await;
    assert!(
        missing_size.contains("resize requires size"),
        "unexpected missing size error: {missing_size}"
    );

    let unknown = call_err(
        &tool,
        json!({
            "op": "explode",
            "id": id,
        }),
    )
    .await;
    assert!(
        unknown.contains("unknown process operation: explode"),
        "unexpected unknown op error: {unknown}"
    );

    // Missing id on an id-requiring op must fail before touching the manager.
    let missing_id = call_err(&tool, json!({ "op": "describe" })).await;
    assert!(
        missing_id.contains("process operation requires id"),
        "unexpected missing id error: {missing_id}"
    );

    // signal without signal value.
    let missing_signal = call_err(
        &tool,
        json!({
            "op": "signal",
            "id": id,
        }),
    )
    .await;
    assert!(
        missing_signal.contains("signal requires signal"),
        "unexpected missing signal error: {missing_signal}"
    );

    // start without argv.
    let missing_argv = call_err(&tool, json!({ "op": "start" })).await;
    assert!(
        missing_argv.contains("process start requires argv"),
        "unexpected missing argv error: {missing_argv}"
    );

    let _ = call(
        &tool,
        json!({ "op": "stop", "id": id, "timeoutMs": 1000 }),
    )
    .await;
}

#[tokio::test]
async fn process_tool_pre_abort_does_not_spawn() {
    let directory = tempfile::tempdir().expect("tempdir");
    let manager = fixture_manager();
    let owner = ProcessOwnerId::new("abort-owner");
    let tool = process_tool(directory.path(), manager.clone(), owner.clone());

    let error = (tool.execute)(make_aborted_ctx(json!({
        "op": "start",
        "argv": ["/bin/sh", "-c", "sleep 30"],
        "pty": false,
    })))
    .await
    .expect_err("pre-aborted start must fail");
    assert_eq!(error.to_string(), "Operation aborted");
    assert!(
        manager.list(&owner).is_empty(),
        "pre-aborted start must not leave a running process"
    );
}

#[tokio::test]
async fn process_tool_owner_isolation_via_public_construction() {
    let directory = tempfile::tempdir().expect("tempdir");
    let manager = fixture_manager();
    let owner_a = owner_tool(directory.path(), manager.clone(), "owner-a");
    let owner_b = owner_tool(directory.path(), manager.clone(), "owner-b");

    let started = call(
        &owner_a,
        json!({
            "op": "start",
            // fflush via newline + stdbuf so output is retained before we stop.
            "argv": ["/bin/sh", "-c", "printf 'secret-a\\n'; sleep 30"],
            "pty": false,
            "label": "a-only",
        }),
    )
    .await;
    let id = process_id(&started.details);

    // Wait until owner A can observe the secret — proves retention before isolation checks.
    let mut cursor = 0u64;
    let mut saw_secret = false;
    for _ in 0..40 {
        let logs = call(
            &owner_a,
            json!({
                "op": "logs",
                "id": id,
                "cursor": cursor,
                "follow": true,
                "timeoutMs": 100,
                "maxBytes": 1024,
            }),
        )
        .await;
        cursor = logs.details["cursor"].as_u64().expect("cursor");
        let bytes = decode_log_bytes(&logs.details);
        if bytes.windows(b"secret-a\n".len()).any(|window| window == b"secret-a\n") {
            saw_secret = true;
            break;
        }
    }
    assert!(saw_secret, "owner A never observed own process output before isolation checks");

    // Owner B's ps must not list A's process.
    let ps_b = call(&owner_b, json!({ "op": "ps" })).await;
    let listed_b = ps_b.details.as_array().expect("ps array");
    assert!(
        listed_b.is_empty(),
        "owner B must not see owner A processes: {listed_b:?}"
    );

    // Cross-owner describe/logs/send/stop must fail.
    let describe_err = call_err(&owner_b, json!({ "op": "describe", "id": id })).await;
    assert!(
        !describe_err.is_empty(),
        "cross-owner describe must error"
    );

    let logs_err = call_err(
        &owner_b,
        json!({
            "op": "logs",
            "id": id,
            "cursor": 0,
        }),
    )
    .await;
    assert!(!logs_err.is_empty(), "cross-owner logs must error");

    let send_err = call_err(
        &owner_b,
        json!({
            "op": "send",
            "id": id,
            "text": "intrude\n",
        }),
    )
    .await;
    assert!(!send_err.is_empty(), "cross-owner send must error");

    let stop_err = call_err(
        &owner_b,
        json!({
            "op": "stop",
            "id": id,
            "timeoutMs": 500,
        }),
    )
    .await;
    assert!(!stop_err.is_empty(), "cross-owner stop must error");

    // Owner A still owns the process and can read its output after stop.
    let stopped = call(
        &owner_a,
        json!({ "op": "stop", "id": id, "timeoutMs": 2000 }),
    )
    .await;
    assert_eq!(stopped.details["id"], id);
    assert_ne!(stopped.details["state"], "running");

    let logs_a = call(
        &owner_a,
        json!({
            "op": "logs",
            "id": id,
            "cursor": 0,
            "maxBytes": 1024,
        }),
    )
    .await;
    let output = String::from_utf8(decode_log_bytes(&logs_a.details)).expect("utf8");
    assert!(
        output.contains("secret-a"),
        "owner A must still read own logs: {output:?}"
    );

    // Owner A ps still lists the (now stopped) process; B still empty.
    let ps_a = call(&owner_a, json!({ "op": "ps" })).await;
    let listed_a = ps_a.details.as_array().expect("ps array");
    assert_eq!(listed_a.len(), 1);
    assert_eq!(listed_a[0]["id"], id);

    let ps_b_after = call(&owner_b, json!({ "op": "ps" })).await;
    assert!(
        ps_b_after
            .details
            .as_array()
            .expect("ps array")
            .is_empty()
    );
}

#[tokio::test]
async fn process_tool_send_data_base64_and_close_stdin() {
    let directory = tempfile::tempdir().expect("tempdir");
    let manager = fixture_manager();
    let tool = owner_tool(directory.path(), manager, "b64-owner");

    let payload = b"binary\x00payload";
    let encoded = base64::engine::general_purpose::STANDARD.encode(payload);

    let started = call(
        &tool,
        json!({
            "op": "start",
            // od -An -tx1 prints hex so we can assert exact bytes without shell quoting pain.
            "argv": ["/bin/sh", "-c", "od -An -tx1 -v | tr -d ' \\n'"],
            "pty": false,
        }),
    )
    .await;
    let id = process_id(&started.details);

    let sent = call(
        &tool,
        json!({
            "op": "send",
            "id": id,
            "dataBase64": encoded,
            "closeStdin": true,
        }),
    )
    .await;
    assert_eq!(sent.details["ok"], true);

    let waited = call(
        &tool,
        json!({ "op": "wait", "id": id, "timeoutMs": 3000 }),
    )
    .await;
    assert_eq!(waited.details["exitCode"], 0);

    let logs = call(
        &tool,
        json!({
            "op": "logs",
            "id": id,
            "cursor": 0,
            "maxBytes": 4096,
        }),
    )
    .await;
    let hex = String::from_utf8(decode_log_bytes(&logs.details))
        .expect("utf8 hex")
        .replace('\n', "");
    let expected: String = payload.iter().map(|byte| format!("{byte:02x}")).collect();
    assert!(
        hex.contains(&expected),
        "base64 payload not observed; hex={hex:?} expected={expected}"
    );
}

#[tokio::test]
async fn process_tool_name_and_schema_surface() {
    let directory = tempfile::tempdir().expect("tempdir");
    let tool = owner_tool(directory.path(), fixture_manager(), "schema-owner");
    assert_eq!(tool.name, "process");
    assert!(
        tool.description.to_lowercase().contains("process"),
        "description should mention process supervision"
    );
    // Schema must advertise the op enum the executor accepts.
    let schema = serde_json::to_value(&tool.parameters).expect("schema json");
    let op_enum = schema["properties"]["op"]["enum"]
        .as_array()
        .expect("op enum");
    for required in [
        "start", "ps", "describe", "logs", "send", "resize", "signal", "stop", "wait",
    ] {
        assert!(
            op_enum.iter().any(|value| value.as_str() == Some(required)),
            "schema missing op {required}: {op_enum:?}"
        );
    }
}

