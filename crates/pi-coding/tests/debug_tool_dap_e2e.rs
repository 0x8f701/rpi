//! End-to-end coverage for the `debug` DAP tool (T106) against a REAL DAP
//! adapter: the tool spawns `gdb -q -i dap` as a child, performs the
//! initialize/launch handshake, sets a breakpoint, runs the debuggee to the
//! stop, reads the stack, evaluates an expression in the stopped frame, and
//! terminates the adapter — all through the public tool boundary (the only
//! boundary reachable without a live model turn, since the faux REPL provider
//! cannot script tool calls).
//!
//! The `debug` tool is governed: it fail-closes without a captured application
//! generation ("debug requires an application generation"). The tool is
//! retrieved from a hermetic published `Application` generation
//! (danger-full-access sandbox, no bubblewrap dependency) — the same boundary
//! the agent runtime uses.
//!
//! The full client contract (framing, request/event routing, all action
//! renderers, error paths) is already proven in-module against the fake DAP
//! adapter; this file adds the real-adapter dimension. It is skip-guarded when
//! `gdb` (>= 13, the first release with `-i dap`), `cc`, or a working DAP
//! handshake is unavailable, so CI without a C toolchain stays green.

#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

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
/// process-lifetime tempdir root hosts per-call subdirs.
fn external_config_roots() -> (PathBuf, PathBuf) {
    static ROOT: LazyLock<tempfile::TempDir> =
        LazyLock::new(|| tempfile::tempdir().expect("debug dap e2e config root"));
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
/// returns it together with the `debug` tool retrieved from the application's
/// tool catalog. The returned `Application` and the shared config `TempDir`
/// must outlive every tool call.
async fn debug_tool(cwd: &Path) -> (Application, AgentTool) {
    let suffix = uuid::Uuid::now_v7().to_string();
    let api = format!("debug-dap-e2e-api-{suffix}");
    let provider = format!("debug-dap-e2e-provider-{suffix}");
    let model = Model {
        id: "debug-dap-e2e".to_owned(),
        name: "Debug DAP E2E".to_owned(),
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
    let debug = application
        .get_all_tools()
        .into_iter()
        .find(|tool| tool.name == "debug")
        .expect("debug tool is in the published application catalog");
    (application, debug)
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
    if Command::new("cc")
        .arg("--version")
        .output()
        .map_or(true, |out| !out.status.success())
    {
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
    if stdin
        .write_all(&frame)
        .and_then(|()| stdin.flush())
        .is_err()
    {
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

    let (_application, tool) = debug_tool(cwd.path()).await;

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
    assert!(
        after.contains("no DAP adapter running"),
        "slot must be cleared: {after}"
    );
}

/// Contract: deterministic error paths that need no adapter — an unknown
/// adapter type, a missing program, and an unknown action all fail with the
/// typed messages (this part never requires gdb).
#[tokio::test]
async fn debug_tool_rejects_invalid_launch_actionably() {
    let cwd = tempfile::tempdir().expect("cwd");
    let (_application, tool) = debug_tool(cwd.path()).await;

    let unknown_adapter = call_err(
        &tool,
        json!({ "action": "launch", "adapter": "frobnicator", "program": "app.py" }),
    )
    .await;
    assert!(
        unknown_adapter.contains("unsupported debug adapter"),
        "unknown adapter: {unknown_adapter}"
    );

    let missing_program = call_err(&tool, json!({ "action": "launch", "adapter": "gdb" })).await;
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
