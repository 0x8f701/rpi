//! `debug` tool: session-scoped DAP (Debug Adapter Protocol) client.
//!
//! Drives a DAP adapter (`gdb`, `lldb-dap`, `debugpy`) over child-process
//! stdio with `Content-Length` framing (shared with the LSP and MCP clients
//! via [`framing`](super::framing)). DAP is JSON-RPC 2.0 with a different
//! envelope: requests carry a `seq`, responses echo it as `request_seq`, and
//! events carry `type: "event"` plus an `event` name.
//!
//! ## Session lifecycle
//!
//! One adapter per session: `launch` resolves the adapter binary on `$PATH`,
//! spawns the process as a process-group leader, and runs the
//! `initialize` → `launch` → `initialized` handshake. The program does **not**
//! start until `configurationDone` — sent by the first `continue_` — so
//! breakpoints can be set between `launch` and `continue_`. `terminate` (or
//! dropping the session, e.g. at session end) kills the whole process group,
//! debuggee included.
//!
//! A background reader task owns the adapter's stdout: responses are routed
//! to the in-flight request by `request_seq` (a pending-response map), events
//! are queued for the actions that wait on them (`stopped`/`exited`/
//! `terminated`), `output` events accumulate into a bounded debuggee-output
//! buffer, and adapter→client requests (e.g. `runInTerminal`) are answered
//! with a failure response so the adapter never blocks. Adapter stderr is
//! captured (bounded) and redacted (see [`redact_secrets`]) before it is
//! embedded in launch-failure and adapter-death errors.
//!
//! ## Actions
//!
//! - `launch` — spawn the adapter (`gdb`/`lldb-dap`/`debugpy`); the adapter
//!   binary must exist on `$PATH` (fail actionably otherwise).
//! - `set_breakpoint` — 1-based `file:line`; applies while the program is
//!   paused or before it starts.
//! - `continue_` (alias `continue`) — send `configurationDone` on first
//!   resume, otherwise `continue` on the stopped thread; then wait for the
//!   next stop or exit (`wait_ms` bounds the wait).
//! - `pause` — interrupt the running program and wait for the stop.
//! - `step_over` / `step_in` / `step_out` — single-step a stopped thread.
//! - `stack_trace` — frames; the first frames carry their `scopes` so the
//!   `variables_reference` values can feed `variables`.
//! - `variables` — expand a scope reference.
//! - `evaluate` — evaluate an expression in a frame.
//! - `threads` — list adapter threads.
//! - `terminate` — DAP `disconnect` (terminateDebuggee) then kill the group.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{oneshot, Mutex as AsyncMutex, MutexGuard as AsyncMutexGuard, Notify};
use tokio::time::Instant;

use pi_agent::{AbortSignal, AgentTool, AgentToolResult, ToolCallContext, ToolCapability};

use crate::redact::redact_secrets;
use crate::truncate::truncate_tail;

use super::framing::{encode_message, read_message};
use super::{arg_int, arg_str, check_aborted, s_array, s_number, s_object, s_string, text_result};

/// Implemented actions, listed in the schema and in validation errors.
const ACTIONS: &str = "launch, set_breakpoint, continue_, pause, step_over, step_in, step_out, stack_trace, variables, evaluate, threads, terminate";
/// Per-request timeout, matching OMP's `DEFAULT_REQUEST_TIMEOUT_MS`.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Default bound for `continue_`/`pause` waiting for the next stop/exit.
const DEFAULT_EVENT_WAIT: Duration = Duration::from_secs(30);
/// Bound for step actions waiting for the step's `stopped` event.
const STEP_WAIT: Duration = Duration::from_secs(30);
/// Grace for the adapter to exit after `disconnect` before it is killed.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// Grace for the child to exit after being killed.
const EXIT_TIMEOUT: Duration = Duration::from_secs(2);
/// Cap on captured adapter stderr so a chatty adapter cannot balloon memory.
const STDERR_CAP: usize = 64 * 1024;
/// Cap on queued DAP events (oldest are dropped).
const EVENT_QUEUE_CAP: usize = 256;
/// Cap on accumulated debuggee output text from `output` events.
const OUTPUT_CAP: usize = 256 * 1024;
/// Output byte budget for rendered tool results (matches mcp's cap).
const OUTPUT_MAX_BYTES: usize = 32 * 1024;
/// Cap on rendered stack frames (DAP `levels` default).
const MAX_FRAMES: usize = 50;
/// Frames whose scopes are fetched and rendered (cheap; avoids N+1 requests
/// for deep stacks).
const MAX_SCOPES_FRAMES: usize = 10;
/// Cap on rendered variables per scope.
const MAX_VARIABLES: usize = 200;
/// Cap on rendered threads.
const MAX_THREADS: usize = 100;

/// One supported DAP adapter: the adapter argv prefix (first element resolved
/// on `$PATH`) and the DAP launch `type`.
struct AdapterSpec {
    adapter: &'static str,
    command: &'static [&'static str],
    dap_type: &'static str,
}

/// Maps an adapter name to its process command and launch `type`.
fn adapter_spec(adapter: &str) -> Option<&'static AdapterSpec> {
    match adapter {
        "gdb" => Some(&AdapterSpec {
            adapter: "gdb",
            command: &["gdb", "-q", "-i", "dap"],
            dap_type: "gdb",
        }),
        "lldb-dap" => Some(&AdapterSpec {
            adapter: "lldb-dap",
            command: &["lldb-dap"],
            dap_type: "lldb-dap",
        }),
        "debugpy" => Some(&AdapterSpec {
            adapter: "debugpy",
            command: &["python3", "-m", "debugpy.adapter"],
            dap_type: "debugpy",
        }),
        _ => None,
    }
}

/// Locates `binary` on the `$PATH` string `path` (mirrors the LSP client's
/// lookup).
fn find_in_path(path: &str, binary: &str) -> Option<PathBuf> {
    for dir in std::env::split_paths(path) {
        let candidate = dir.join(binary);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Resolves the adapter process command: validates the adapter name and
/// requires its executable to exist on `$PATH` (fail actionably). Returns
/// `(resolved binary, remaining argv)`.
fn resolve_adapter_command(adapter: &str, path: &str) -> Result<(String, Vec<String>)> {
    let spec = adapter_spec(adapter).ok_or_else(|| {
        anyhow!(
            "unsupported debug adapter `{adapter}` (supported: gdb, lldb-dap, debugpy)"
        )
    })?;
    let binary = spec.command[0];
    let executables: &[&str] = if binary == "python3" {
        // debugpy runs via `python3 -m debugpy.adapter`; accept python too.
        &["python3", "python"]
    } else {
        std::slice::from_ref(&binary)
    };
    let resolved = executables
        .iter()
        .find_map(|candidate| find_in_path(path, candidate))
        .ok_or_else(|| {
            if binary == "python3" {
                anyhow!(
                    "DAP adapter `debugpy` requires python3 (or python) on PATH to run `{}`; \
                     install Python with the debugpy package",
                    spec.command.join(" ")
                )
            } else {
                anyhow!(
                    "DAP adapter binary `{binary}` not found in PATH (required for adapter \
                     `{adapter}`); install it and retry"
                )
            }
        })?;
    let mut argv: Vec<String> = spec.command.iter().map(|arg| (*arg).to_owned()).collect();
    argv[0] = resolved.to_string_lossy().into_owned();
    Ok((argv.remove(0), argv))
}

/// Resolves a tool path against the session cwd (absolute paths pass through).
fn resolve_path(cwd: &str, path: &str) -> String {
    let p = Path::new(path);
    if p.is_absolute() {
        path.to_owned()
    } else {
        Path::new(cwd).join(p).to_string_lossy().into_owned()
    }
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

/// The last observed program stop (from a `stopped` event).
#[derive(Clone)]
struct StoppedInfo {
    thread_id: i64,
    reason: String,
}

/// Program lifecycle as observed from responses and events.
#[derive(Default)]
struct SessionState {
    /// True once `configurationDone` has been sent (the program may run).
    started: bool,
    /// The most recent `stopped` event; `None` while the program runs.
    last_stop: Option<StoppedInfo>,
    /// True after `exited`/`terminated` or adapter stream death.
    terminated: bool,
    /// Breakpoints per source path (1-based lines), as last sent to the
    /// adapter.
    breakpoints: BTreeMap<String, Vec<i64>>,
}

/// A live DAP adapter session: spawned child + framed stdin/stdout, a
/// background reader task routing responses to in-flight requests and
/// queueing events, bounded stderr/output capture. Drop kills the whole
/// process group (adapter + debuggee).
pub(crate) struct DebugSession {
    child: Child,
    stdin: Arc<AsyncMutex<ChildStdin>>,
    next_seq: Arc<AtomicI64>,
    pending: Arc<Mutex<HashMap<i64, oneshot::Sender<anyhow::Result<Value>>>>>,
    events: Arc<Mutex<VecDeque<Value>>>,
    output: Arc<Mutex<String>>,
    stderr_tail: Arc<Mutex<String>>,
    notify: Arc<Notify>,
    dead: Arc<AtomicBool>,
    death_error: Arc<Mutex<Option<String>>>,
    state: Mutex<SessionState>,
    /// DAP launch `type` (gdb | lldb-dap | debugpy).
    adapter: String,
}

impl DebugSession {
    /// Spawns an already-configured command as a DAP adapter and starts the
    /// stdout reader / stderr capture tasks. The child leads its own process
    /// group so a kill reaps the adapter *and* its debuggee.
    pub(crate) async fn spawn_command(
        mut command: Command,
        adapter: &str,
    ) -> Result<Self> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.as_std_mut().process_group(0);
        }
        // Route through the crate's ETXTBSY-retrying spawn: re-executing a
        // just-touched binary (e.g. a test binary under parallel load) can
        // transiently fail with "Text file busy"; the retry resolves it.
        let mut child = super::spawn_with_etxtbsy_retry(&mut command, adapter)
            .await
            .context("spawning DAP adapter process")?;
        let stdin = child.stdin.take().context("DAP adapter stdin unavailable")?;
        let stdout = child.stdout.take().context("DAP adapter stdout unavailable")?;
        let stderr = child.stderr.take().context("DAP adapter stderr unavailable")?;
        let session = Self {
            child,
            stdin: Arc::new(AsyncMutex::new(stdin)),
            next_seq: Arc::new(AtomicI64::new(0)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            events: Arc::new(Mutex::new(VecDeque::new())),
            output: Arc::new(Mutex::new(String::new())),
            stderr_tail: Arc::new(Mutex::new(String::new())),
            notify: Arc::new(Notify::new()),
            dead: Arc::new(AtomicBool::new(false)),
            death_error: Arc::new(Mutex::new(None)),
            state: Mutex::new(SessionState::default()),
            adapter: adapter.to_owned(),
        };
        session.start_readers(stdout, stderr);
        Ok(session)
    }

    /// Spawns the bounded stderr-tail capture and the stdout reader task.
    fn start_readers(&self, stdout: ChildStdout, stderr: ChildStderr) {
        let tail = self.stderr_tail.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 8192];
            let mut reader = BufReader::new(stderr);
            loop {
                match reader.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut guard = tail.lock();
                        guard.push_str(&String::from_utf8_lossy(&buf[..n]));
                        if guard.len() > STDERR_CAP {
                            let overflow = guard.len() - STDERR_CAP;
                            guard.drain(..overflow);
                        }
                    }
                }
            }
        });

        let stdin = self.stdin.clone();
        let next_seq = self.next_seq.clone();
        let pending = self.pending.clone();
        let events = self.events.clone();
        let output = self.output.clone();
        let notify = self.notify.clone();
        let dead = self.dead.clone();
        let death_error = self.death_error.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            loop {
                match read_message("DAP", &mut reader, super::framing::DEFAULT_MAX_MESSAGE_BYTES)
                    .await
                {
                    Ok(message) => match message.get("type").and_then(Value::as_str) {
                        Some("response") => {
                            let request_seq = message.get("request_seq").and_then(Value::as_i64);
                            if let Some(seq) = request_seq
                                && let Some(tx) = pending.lock().remove(&seq)
                            {
                                let outcome = if message
                                    .get("success")
                                    .and_then(Value::as_bool)
                                    .unwrap_or(false)
                                {
                                    Ok(message.get("body").cloned().unwrap_or(Value::Null))
                                } else {
                                    Err(anyhow!(dap_error_message(&message)))
                                };
                                let _ = tx.send(outcome);
                            }
                        }
                        Some("event") => {
                            if message.get("event").and_then(Value::as_str) == Some("output") {
                                // Debuggee output: accumulate into the bounded
                                // buffer instead of the event queue.
                                if let Some(text) = message
                                    .pointer("/body/output")
                                    .and_then(Value::as_str)
                                {
                                    let mut guard = output.lock();
                                    guard.push_str(text);
                                    if guard.len() > OUTPUT_CAP {
                                        let overflow = guard.len() - OUTPUT_CAP;
                                        guard.drain(..overflow);
                                    }
                                }
                            } else {
                                let mut queue = events.lock();
                                queue.push_back(message);
                                while queue.len() > EVENT_QUEUE_CAP {
                                    queue.pop_front();
                                }
                            }
                            notify.notify_waiters();
                        }
                        Some("request") => {
                            // Adapter→client request (e.g. runInTerminal): we
                            // cannot honor it, so answer a failure response to
                            // keep the adapter's protocol loop moving.
                            let request_seq = message.get("seq").and_then(Value::as_i64).unwrap_or(0);
                            let command =
                                message.get("command").and_then(Value::as_str).unwrap_or("unknown");
                            let seq = next_seq.fetch_add(1, Ordering::SeqCst) + 1;
                            let response = json!({
                                "seq": seq,
                                "type": "response",
                                "request_seq": request_seq,
                                "success": false,
                                "command": command,
                                "message": format!(
                                    "rpi cannot honor DAP client request `{command}`"
                                ),
                            });
                            if let Ok(bytes) = encode_message(&response) {
                                let mut guard = stdin.lock().await;
                                let _ = guard.write_all(&bytes).await;
                                let _ = guard.flush().await;
                            }
                        }
                        _ => {}
                    },
                    Err(error) => {
                        dead.store(true, Ordering::SeqCst);
                        *death_error.lock() = Some(error.to_string());
                        let pending_now = std::mem::take(&mut *pending.lock());
                        for (_, tx) in pending_now {
                            let _ = tx.send(Err(anyhow!("DAP adapter exited: {error}")));
                        }
                        notify.notify_waiters();
                        break;
                    }
                }
            }
        });
    }

    /// Sends a DAP request and waits for the matching response, bounded by
    /// [`REQUEST_TIMEOUT`].
    pub(crate) async fn request(&self, command: &str, args: Value) -> Result<Value> {
        self.request_timeout(command, args, REQUEST_TIMEOUT).await
    }

    /// [`Self::request`] bounded by `timeout`.
    pub(crate) async fn request_timeout(
        &self,
        command: &str,
        args: Value,
        timeout: Duration,
    ) -> Result<Value> {
        if self.dead.load(Ordering::SeqCst) {
            bail!("DAP adapter is not running: {}", self.death_detail());
        }
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let (tx, rx) = oneshot::channel();
        self.pending.lock().insert(seq, tx);
        let message = json!({ "seq": seq, "type": "request", "command": command, "arguments": args });
        let bytes = match encode_message(&message) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.pending.lock().remove(&seq);
                return Err(error);
            }
        };
        let write = {
            let mut stdin = self.stdin.lock().await;
            let write = stdin.write_all(&bytes).await;
            let flush = stdin.flush().await;
            write.and_then(|_| flush)
        };
        if let Err(error) = write {
            self.pending.lock().remove(&seq);
            bail!("writing DAP request `{command}` to adapter: {error}");
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => {
                // The reader died without answering (it fails pending waiters
                // with the exit error, so this only fires on a race).
                bail!("DAP adapter closed while waiting for `{command}` response")
            }
            Err(_elapsed) => {
                self.pending.lock().remove(&seq);
                bail!("DAP request `{command}` timed out after {}s", timeout.as_secs());
            }
        }
    }

    /// Runs the DAP `initialize` + `launch` handshake and waits for the
    /// adapter's `initialized` event. The program stays paused before start
    /// until `configurationDone` (sent by the first `continue_`).
    pub(crate) async fn launch(&self, args: &Value, abort: &AbortSignal) -> Result<()> {
        let from = self.event_watermark();
        let adapter = self.adapter.clone();
        let initialize = json!({
            "adapterID": adapter,
            "clientID": "rpi",
            "clientName": "rpi",
            "linesStartAt1": true,
            "columnsStartAt1": true,
            "pathFormat": "path",
            "supportsVariableType": true,
            "supportsVariablePaging": false,
            "supportsMemoryReferences": false,
            "supportsInvalidatedEvent": false,
        });
        self.request("initialize", initialize)
            .await
            .map_err(|error| self.launch_failure("initialize", error))?;
        let launch_args = build_launch_params(&adapter, args)?;
        self.request("launch", launch_args)
            .await
            .map_err(|error| self.launch_failure("launch", error))?;
        self.wait_for_event(
            |message| message.get("event").and_then(Value::as_str) == Some("initialized"),
            from,
            REQUEST_TIMEOUT,
            abort,
        )
        .await
        .map_err(|error| self.launch_failure("waiting for the initialized event", error))?;
        Ok(())
    }

    /// Formats a launch failure, embedding the (redacted) adapter stderr
    /// tail like the MCP/LSP initialize-failure paths do.
    fn launch_failure(&self, stage: &str, error: anyhow::Error) -> anyhow::Error {
        let stderr = redact_secrets(&self.stderr_tail());
        let mut message = format!(
            "DAP adapter `{}` failed to launch during {stage}: {error}",
            self.adapter
        );
        if !stderr.trim().is_empty() {
            message.push_str("\n--- adapter stderr ---\n");
            message.push_str(&stderr);
        }
        // The error text is adapter-controlled; run the whole message through
        // the redactor, not just the tail.
        anyhow!(redact_secrets(&message))
    }

    /// Redacted adapter-stderr tail for diagnostics (see [`redact_secrets`]).
    fn stderr_tail(&self) -> String {
        self.stderr_tail.lock().clone()
    }

    /// Why the session is dead: the reader's exit error plus any redacted
    /// stderr the adapter left behind.
    fn death_detail(&self) -> String {
        let mut detail = self
            .death_error
            .lock()
            .clone()
            .unwrap_or_else(|| "adapter process exited".to_owned());
        let stderr = redact_secrets(&self.stderr_tail());
        if !stderr.trim().is_empty() {
            detail.push_str("\n--- adapter stderr ---\n");
            detail.push_str(&stderr);
        }
        detail
    }

    /// Waits for the next queued event matching `matches` that was queued at
    /// or after the `from` watermark (see [`Self::event_watermark`]), bounded
    /// by `timeout` and the abort signal. Consumed events update the session
    /// state (stopped/exited/terminated/continued).
    async fn wait_for_event<F>(
        &self,
        matches: F,
        from: usize,
        timeout: Duration,
        abort: &AbortSignal,
    ) -> Result<Value>
    where
        F: Fn(&Value) -> bool,
    {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(event) = self.pop_event(&matches, from) {
                self.apply_event(&event);
                return Ok(event);
            }
            if self.dead.load(Ordering::SeqCst) {
                bail!("DAP adapter exited: {}", self.death_detail());
            }
            let now = Instant::now();
            if now >= deadline {
                bail!(
                    "timed out after {}s waiting for a DAP event",
                    timeout.as_secs()
                );
            }
            // Register the notification before re-checking the queue so a
            // wakeup between the check and the await cannot be lost.
            let notified = self.notify.notified();
            if let Some(event) = self.pop_event(&matches, from) {
                self.apply_event(&event);
                return Ok(event);
            }
            if self.dead.load(Ordering::SeqCst) {
                bail!("DAP adapter exited: {}", self.death_detail());
            }
            tokio::select! {
                _ = notified => {}
                _ = abort.cancelled() => bail!("Operation aborted"),
                _ = tokio::time::sleep_until(deadline) => {}
            }
        }
    }

    /// The number of events currently queued — the watermark for waits: a
    /// wait started with `from = event_watermark()` only observes events
    /// queued after that moment, so stale stops from an earlier run segment
    /// (e.g. a `continue_ wait_ms=0` whose stop arrived later) are never
    /// misreported by a subsequent `pause`/`step` wait.
    fn event_watermark(&self) -> usize {
        self.events.lock().len()
    }

    /// Removes the first queued event at or after `from` matching `matches`
    /// (arrival order), leaving non-matching events for other waiters.
    fn pop_event(&self, matches: &dyn Fn(&Value) -> bool, from: usize) -> Option<Value> {
        let mut queue = self.events.lock();
        let offset = queue.iter().skip(from).position(matches)?;
        queue.remove(from + offset)
    }

    /// Updates the session state from a consumed event.
    fn apply_event(&self, event: &Value) {
        let mut state = self.state.lock();
        match event.get("event").and_then(Value::as_str) {
            Some("stopped") => {
                let thread_id = event
                    .pointer("/body/threadId")
                    .and_then(Value::as_i64)
                    .unwrap_or(-1);
                let reason = event
                    .pointer("/body/reason")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_owned();
                state.last_stop = Some(StoppedInfo { thread_id, reason });
            }
            Some("continued") => state.last_stop = None,
            Some("exited") => {
                state.terminated = true;
                state.last_stop = None;
            }
            Some("terminated") => {
                state.terminated = true;
                state.last_stop = None;
            }
            _ => {}
        }
    }

    /// Drains the accumulated debuggee output (from `output` events) since
    /// the last report.
    fn drain_output(&self) -> String {
        let mut guard = self.output.lock();
        std::mem::take(&mut *guard)
    }

    /// Best-effort DAP `disconnect` (terminating the debuggee), then kills
    /// the whole process group and reaps the child. Never fails the caller.
    pub(crate) async fn terminate(mut self) -> String {
        let _ = tokio::time::timeout(
            SHUTDOWN_TIMEOUT,
            self.request(
                "disconnect",
                json!({ "restart": false, "terminateDebuggee": true }),
            ),
        )
        .await;
        kill_adapter(&self.child);
        match tokio::time::timeout(EXIT_TIMEOUT, self.child.wait()).await {
            Ok(Ok(status)) => format!("DAP adapter terminated ({status})."),
            _ => "DAP adapter terminated (killed).".to_owned(),
        }
    }
}

impl Drop for DebugSession {
    fn drop(&mut self) {
        // Never leak the adapter (or its debuggee), even on panic paths.
        if self.child.try_wait().ok().flatten().is_none() {
            kill_adapter(&self.child);
            let _ = self.child.start_kill();
        }
    }
}

/// SIGKILLs the adapter's process group (spawned with `process_group(0)`, so
/// this also reaps the debuggee). Mirrors the hooks.rs kill_process_group
/// pattern.
fn kill_adapter(child: &Child) {
    #[cfg(unix)]
    {
        use nix::sys::signal::{killpg, Signal};
        use nix::unistd::Pid;
        if let Some(pid) = child.id() {
            let _ = killpg(Pid::from_raw(pid as i32), Signal::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = child;
}

/// Formats a DAP error response (`success: false`) into an actionable
/// message.
fn dap_error_message(message: &Value) -> String {
    let command = message.get("command").and_then(Value::as_str).unwrap_or("?");
    let detail = message
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("DAP error response");
    let mut text = format!("DAP request `{command}` failed: {detail}");
    if let Some(body_error) = message.pointer("/body/error/message").and_then(Value::as_str)
        && body_error != detail
    {
        text.push_str(&format!(" — {body_error}"));
    }
    text
}

/// Builds the DAP `launch` request arguments from the tool call: the adapter
/// `type`, `program` (resolved against the session cwd), optional `args`/
/// `cwd`, and any `launch_args` pass-through object (which overrides the
/// defaults).
fn build_launch_params(adapter: &str, args: &Value) -> Result<Value> {
    let program = arg_str(args, "program");
    let program = program.trim();
    if program.is_empty() {
        bail!("debug launch requires a `program` path (the debuggee)");
    }
    let mut params = serde_json::Map::new();
    params.insert("type".to_owned(), json!(adapter));
    params.insert("request".to_owned(), json!("launch"));
    params.insert("program".to_owned(), json!(program));
    if let Some(program_args) = args.get("args").and_then(Value::as_array) {
        let strings = program_args
            .iter()
            .map(Value::as_str)
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| anyhow!("debug launch `args` must be an array of strings"))?;
        params.insert("args".to_owned(), json!(strings));
    }
    let cwd = arg_str(args, "cwd");
    if !cwd.trim().is_empty() {
        params.insert("cwd".to_owned(), json!(cwd.trim()));
    }
    if let Some(extra) = args.get("launch_args").and_then(Value::as_object) {
        for (key, value) in extra {
            params.insert(key.clone(), value.clone());
        }
    }
    Ok(Value::Object(params))
}

// ---------------------------------------------------------------------------
// Registry (session-scoped, one adapter)
// ---------------------------------------------------------------------------

/// Session-scoped DAP adapter registry: at most one live adapter, spawned by
/// `launch` and killed by `terminate` (or drop, e.g. at session end). Cloning
/// shares the same slot; the tool captures one registry per tool instance.
#[derive(Clone, Default)]
pub(crate) struct DebugRegistry {
    inner: Arc<AsyncMutex<Option<DebugSession>>>,
}

impl DebugRegistry {
    /// An empty registry (no adapter running).
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Locks the session slot for an action.
    pub(crate) async fn lock(&self) -> AsyncMutexGuard<'_, Option<DebugSession>> {
        self.inner.lock().await
    }
}

// ---------------------------------------------------------------------------
// Tool surface
// ---------------------------------------------------------------------------

/// Builds the `debug` tool: session-scoped DAP debugging (gdb, lldb-dap,
/// debugpy) with one live adapter per session.
pub(crate) fn debug_tool(cwd: &str) -> AgentTool {
    let description = format!(
        "Drive a Debug Adapter Protocol (DAP) adapter over stdio for a session-scoped debugging \
         session. Actions: {ACTIONS}. `launch` resolves the adapter binary on PATH, spawns it \
         and runs the initialize+launch handshake (the program starts on the first continue_); \
         `set_breakpoint file=... line=N` sets a 1-based breakpoint while the program is paused \
         or before it starts; `continue_` (alias `continue`) resumes and waits for the next stop \
         or exit; `pause` interrupts a running program; `step_over`/`step_in`/`step_out` \
         single-step the stopped thread; `stack_trace` lists frames plus their scopes (their \
         variables_reference feeds `variables`); `variables variables_reference=N` expands a \
         scope; `evaluate expression=...` evaluates in the stopped frame; `threads` lists \
         adapter threads; `terminate` disconnects and kills the adapter (and its debuggee). One \
         adapter per session: launching while one is active fails until `terminate`."
    );
    let params = s_object(
        vec![
            (
                "action",
                s_string(&format!("Debug action to run. One of: {ACTIONS}")),
            ),
            (
                "adapter",
                s_string("DAP adapter type for launch: gdb, lldb-dap, debugpy"),
            ),
            (
                "program",
                s_string("Debuggee program path (required for launch; resolved against the session cwd)"),
            ),
            (
                "args",
                s_array(s_string("Program argument"), "Array of program arguments for the debuggee (launch)"),
            ),
            (
                "cwd",
                s_string("Working directory for the debuggee (launch; default: session cwd)"),
            ),
            (
                "launch_args",
                s_string("Optional JSON object merged into the DAP launch request (e.g. env, stopOnEntry); overrides defaults"),
            ),
            (
                "adapter_args",
                s_array(s_string("Adapter argument"), "Optional extra arguments appended to the adapter command line (launch)"),
            ),
            (
                "file",
                s_string("Source file path for set_breakpoint (resolved against the session cwd)"),
            ),
            (
                "line",
                s_number("1-based line number for set_breakpoint (DAP lines are 1-based)"),
            ),
            (
                "thread",
                s_number("Thread id for stack_trace/continue_/pause/step actions; defaults to the last stopped thread"),
            ),
            (
                "variables_reference",
                s_number("Scope reference (from `debug stack_trace`) to expand for `variables`"),
            ),
            (
                "expression",
                s_string("Expression to evaluate in the stopped frame (evaluate)"),
            ),
            (
                "frame_id",
                s_number("Frame id for evaluate; defaults to the top frame (0) when stopped"),
            ),
            (
                "wait_ms",
                s_number("How long continue_/pause wait for the next stop event (0 returns immediately); default 30000"),
            ),
            (
                "levels",
                s_number("Max stack frames for stack_trace; default 50"),
            ),
        ],
        vec!["action"],
    );
    let registry = DebugRegistry::new();
    let cwd = cwd.to_owned();
    AgentTool::new("debug", description, params, move |ctx: ToolCallContext| {
        let registry = registry.clone();
        let cwd = cwd.clone();
        async move { run_debug(&registry, &cwd, ctx.arguments, ctx.abort).await }
    })
    .with_capability(ToolCapability::Write)
    .with_prompt_guidelines(vec![
        "Debug flow: `debug launch adapter=gdb program=<path>` → `debug set_breakpoint file=<path> line=<n>` → `debug continue_` (waits for the next stop) → `debug stack_trace` → `debug variables variables_reference=<ref>` → `debug step_over` / `debug evaluate expression=...` → `debug terminate`.".to_string(),
        "DAP line numbers are 1-based; breakpoints must be set while the program is paused or before it starts.".to_string(),
        "Launching an adapter spawns a real debugger process that runs the debuggee — terminate it when done; never point it at sensitive infrastructure.".to_string(),
    ])
}

/// Entry point: validates the action and dispatches against the session slot.
pub(crate) async fn run_debug(
    registry: &DebugRegistry,
    cwd: &str,
    args: Value,
    abort: AbortSignal,
) -> Result<AgentToolResult> {
    check_aborted(&abort)?;
    let action = arg_str(&args, "action");
    let action = action.trim();
    if action.is_empty() {
        bail!("debug action is required (one of: {ACTIONS})");
    }
    let mut slot = registry.lock().await;
    match action {
        "launch" => run_launch(&mut slot, cwd, &args, &abort).await,
        "terminate" => {
            let session = slot
                .take()
                .ok_or_else(|| anyhow!("no DAP adapter running to terminate"))?;
            let report = session.terminate().await;
            Ok(text_result(report))
        }
        "set_breakpoint" | "continue_" | "continue" | "pause" | "step_over" | "step_in"
        | "step_out" | "stack_trace" | "variables" | "evaluate" | "threads" => {
            let session = slot.as_ref().ok_or_else(|| {
                anyhow!(
                    "no DAP adapter running — `debug launch` first (adapter=gdb|lldb-dap|debugpy, \
                     program=<path>)"
                )
            })?;
            run_session_action(session, cwd, action, &args, &abort).await
        }
        other => bail!("unknown debug action `{other}` (expected one of: {ACTIONS})"),
    }
}

/// `launch`: validate the adapter/program, resolve the adapter binary on
/// `$PATH`, spawn the session, and run the handshake. On any failure the
/// local session drops (killing the spawned process); the slot is only
/// populated on success.
async fn run_launch(
    slot: &mut AsyncMutexGuard<'_, Option<DebugSession>>,
    cwd: &str,
    args: &Value,
    abort: &AbortSignal,
) -> Result<AgentToolResult> {
    if slot.is_some() {
        bail!(
            "a DAP adapter is already running for this session; `debug terminate` it first"
        );
    }
    let adapter = arg_str(args, "adapter");
    let adapter = adapter.trim();
    if adapter.is_empty() {
        bail!("debug launch requires an `adapter` (one of: gdb, lldb-dap, debugpy)");
    }
    let program = arg_str(args, "program");
    let program = program.trim();
    if program.is_empty() {
        bail!("debug launch requires a `program` path (the debuggee)");
    }
    let path = std::env::var_os("PATH")
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (binary, base_args) = resolve_adapter_command(adapter, &path)?;
    let mut command = Command::new(&binary);
    command.args(&base_args);
    if let Some(extra) = args.get("adapter_args").and_then(Value::as_array) {
        let extra = extra
            .iter()
            .map(Value::as_str)
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| anyhow!("debug launch `adapter_args` must be an array of strings"))?;
        command.args(extra);
    }
    command.current_dir(cwd);
    let session = DebugSession::spawn_command(command, adapter).await?;
    session.launch(args, abort).await?;
    let program_abs = resolve_path(cwd, program);
    **slot = Some(session);
    Ok(text_result(format!(
        "DAP adapter `{adapter}` launched; program {program_abs} is ready. Set breakpoints \
         with `debug set_breakpoint file=<path> line=<n>`, then `debug continue_` starts it."
    )))
}

/// Dispatches a non-launch/non-terminate action against the live session.
async fn run_session_action(
    session: &DebugSession,
    cwd: &str,
    action: &str,
    args: &Value,
    abort: &AbortSignal,
) -> Result<AgentToolResult> {
    match action {
        "set_breakpoint" => run_set_breakpoint(session, cwd, args).await,
        "continue_" | "continue" => run_continue(session, args, abort).await,
        "pause" => run_pause(session, args, abort).await,
        "step_over" | "step_in" | "step_out" => run_step(session, action, abort).await,
        "stack_trace" => run_stack_trace(session, args).await,
        "variables" => run_variables(session, args).await,
        "evaluate" => run_evaluate(session, args).await,
        "threads" => run_threads(session).await,
        other => bail!("unknown debug action `{other}` (expected one of: {ACTIONS})"),
    }
}

/// Parses the `wait_ms` argument (0 = don't wait; default 30s).
fn wait_duration(args: &Value) -> Result<Duration> {
    match arg_int(args, "wait_ms")? {
        Some(ms) if ms >= 0 => Ok(Duration::from_millis(ms as u64)),
        Some(_) => bail!("debug `wait_ms` must be >= 0"),
        None => Ok(DEFAULT_EVENT_WAIT),
    }
}

/// True for events that end a run: a stop, the program exiting, or the
/// adapter terminating the session.
fn is_stop_event(message: &Value) -> bool {
    matches!(
        message.get("event").and_then(Value::as_str),
        Some("stopped" | "exited" | "terminated")
    )
}

/// Waits for the next stop/exit event queued after `from` and renders the
/// outcome (stop location via a one-frame stackTrace probe, plus any debuggee
/// output accumulated since the last report).
async fn wait_for_stop_and_report(
    session: &DebugSession,
    prefix: &str,
    from: usize,
    wait: Duration,
    abort: &AbortSignal,
) -> Result<AgentToolResult> {
    let event = session
        .wait_for_event(is_stop_event, from, wait, abort)
        .await
        .map_err(|error| {
            // A wait that fails while the program is still running should not
            // lose the accumulated debuggee output.
            let output = session.drain_output();
            if output.trim().is_empty() {
                error
            } else {
                let mut message = error.to_string();
                message.push_str("\n--- debuggee output ---\n");
                message.push_str(&tail_output(&output));
                anyhow!(message)
            }
        })?;
    match event.get("event").and_then(Value::as_str) {
        Some("stopped") => render_stopped(session, prefix, &event).await,
        Some("exited") | Some("terminated") => render_exited(&event, &session.drain_output()),
        _ => unreachable!("is_stop_event matched"),
    }
}

/// Renders a `stopped` event: reason, thread, the top frame's location
/// (best-effort probe), the adapter's description, and recent debuggee
/// output.
async fn render_stopped(
    session: &DebugSession,
    prefix: &str,
    event: &Value,
) -> Result<AgentToolResult> {
    let reason = event
        .pointer("/body/reason")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let description = event.pointer("/body/description").and_then(Value::as_str);
    let text = event.pointer("/body/text").and_then(Value::as_str);
    let thread_id = event.pointer("/body/threadId").and_then(Value::as_i64).unwrap_or(-1);
    let mut out = format!("{prefix} (reason: {reason}, thread {thread_id})");
    // Probe the top frame for the stop location; failures are non-fatal.
    if let Ok(body) = session
        .request_timeout(
            "stackTrace",
            json!({ "threadId": thread_id, "startFrame": 0, "levels": 1 }),
            REQUEST_TIMEOUT,
        )
        .await
        && let Some(frame) = body.pointer("/stackFrames/0")
    {
        let name = frame.get("name").and_then(Value::as_str).unwrap_or("?");
        let line = frame.get("line").and_then(Value::as_i64).unwrap_or(0);
        let source = frame
            .pointer("/source/path")
            .and_then(Value::as_str)
            .or_else(|| frame.pointer("/source/name").and_then(Value::as_str))
            .unwrap_or("?");
        out.push_str(&format!(" at {name} ({source}:{line})"));
    }
    if let Some(description) = description {
        out.push_str(&format!("\n{description}"));
    }
    if let Some(text) = text {
        out.push_str(&format!("\n{text}"));
    }
    let output = session.drain_output();
    if !output.trim().is_empty() {
        out.push_str(&format!("\n--- debuggee output ---\n{}", tail_output(&output)));
    }
    Ok(text_result(bounded(&out)))
}

/// Renders an `exited`/`terminated` event plus recent debuggee output.
fn render_exited(event: &Value, output: &str) -> Result<AgentToolResult> {
    let exit_code = event.pointer("/body/exitCode").and_then(Value::as_i64);
    let mut out = match event.get("event").and_then(Value::as_str) {
        Some("exited") => match exit_code {
            Some(code) => format!("Program exited with code {code}."),
            None => "Program exited.".to_owned(),
        },
        _ => "Program terminated.".to_owned(),
    };
    if !output.trim().is_empty() {
        out.push_str(&format!("\n--- debuggee output ---\n{}", tail_output(output)));
    }
    Ok(text_result(out))
}

/// Trims debuggee output to a bounded tail for embedding in results.
fn tail_output(output: &str) -> String {
    truncate_tail(output, 50, 16 * 1024).content
}

/// Bounds a rendered result to [`OUTPUT_MAX_BYTES`], noting truncation.
fn bounded(text: &str) -> String {
    let result = crate::truncate::truncate_head(text, 0, OUTPUT_MAX_BYTES);
    if result.truncated {
        format!("{}\n[output truncated]", result.content)
    } else {
        result.content
    }
}

/// `set_breakpoint`: adds a 1-based file:line breakpoint (per-source lists
/// are replaced wholesale on the adapter, so earlier breakpoints in the same
/// file survive). Allowed while the program is paused or before it starts.
async fn run_set_breakpoint(session: &DebugSession, cwd: &str, args: &Value) -> Result<AgentToolResult> {
    let file = arg_str(args, "file");
    let file = file.trim();
    if file.is_empty() {
        bail!("debug set_breakpoint requires a `file` path");
    }
    let line = arg_int(args, "line")?
        .ok_or_else(|| anyhow!("debug set_breakpoint requires a `line` (1-based)"))?;
    if line < 1 {
        bail!("debug set_breakpoint `line` must be >= 1 (DAP lines are 1-based)");
    }
    let abs = resolve_path(cwd, file);
    {
        let mut state = session.state.lock();
        if state.terminated {
            bail!("debug session is terminated; `debug launch` to start a new one");
        }
        if state.started && state.last_stop.is_none() {
            bail!("program is running; `debug pause` first, then set breakpoints");
        }
        let lines = state.breakpoints.entry(abs.clone()).or_default();
        if lines.contains(&line) {
            return Ok(text_result(format!("Breakpoint already set at {abs}:{line}")));
        }
        lines.push(line);
        lines.sort_unstable();
    }
    let lines = session.state.lock().breakpoints.get(&abs).cloned().unwrap_or_default();
    let source_name = Path::new(&abs)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| abs.clone());
    let request = json!({
        "source": { "name": source_name, "path": abs },
        "breakpoints": lines.iter().map(|l| json!({ "line": l })).collect::<Vec<_>>(),
        "linesStartAt1": true,
        "sourceModified": false,
    });
    let result = session.request("setBreakpoints", request).await?;
    let breakpoints = result
        .get("breakpoints")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut out = format!("Breakpoints set for {abs}:");
    for bp in &breakpoints {
        let verified = bp.get("verified").and_then(Value::as_bool).unwrap_or(false);
        let line = bp.get("line").and_then(Value::as_i64).unwrap_or(0);
        let id = bp.get("id").and_then(Value::as_i64);
        out.push_str(&format!(
            "\n- {abs}:{line} {}",
            if verified { "verified" } else { "NOT verified" }
        ));
        if let Some(id) = id {
            out.push_str(&format!(" (id {id})"));
        }
        if let Some(message) = bp.get("message").and_then(Value::as_str)
            && !message.is_empty()
        {
            out.push_str(&format!(" — {message}"));
        }
    }
    if breakpoints.is_empty() {
        out.push_str("\n(no breakpoints reported by the adapter)");
    }
    Ok(text_result(out))
}

/// What the next `continue_` must do: start the program (`configurationDone`)
/// or resume the stopped thread.
enum Resume {
    Start,
    Continue(i64),
}

/// `continue_`: send `configurationDone` on first resume, otherwise
/// `continue` on the stopped thread, then wait for the next stop/exit.
async fn run_continue(
    session: &DebugSession,
    args: &Value,
    abort: &AbortSignal,
) -> Result<AgentToolResult> {
    // Capture the watermark before the resume request so the events it
    // triggers (output, stopped/exited) are observed by the wait below.
    let from = session.event_watermark();
    let resume = {
        let state = session.state.lock();
        if state.terminated {
            bail!("debug session is terminated; `debug launch` to start a new one");
        }
        if state.started && state.last_stop.is_none() {
            bail!("program is already running; `debug pause` to interrupt it, then continue_");
        }
        match &state.last_stop {
            Some(stop) => Resume::Continue(stop.thread_id),
            None => Resume::Start,
        }
    };
    match resume {
        Resume::Start => {
            session.request("configurationDone", json!({})).await?;
            session.state.lock().started = true;
        }
        Resume::Continue(thread_id) => {
            session
                .request("continue", json!({ "threadId": thread_id }))
                .await?;
            session.state.lock().last_stop = None;
        }
    }
    let wait = wait_duration(args)?;
    if wait.is_zero() {
        return Ok(text_result(
            "Program resumed (wait_ms=0, not waiting). Use `debug pause` to interrupt, or run \
             `debug continue_` again to wait for the next stop.",
        ));
    }
    wait_for_stop_and_report(session, "Program stopped", from, wait, abort).await
}

/// `pause`: interrupt the running program and wait for the stop.
async fn run_pause(
    session: &DebugSession,
    args: &Value,
    abort: &AbortSignal,
) -> Result<AgentToolResult> {
    // Capture before the pause request so its `stopped` event is observed.
    let from = session.event_watermark();
    {
        let state = session.state.lock();
        if state.terminated {
            bail!("debug session is terminated; `debug launch` to start a new one");
        }
        if !state.started {
            bail!("program has not started yet; `debug continue_` starts it, then pause");
        }
        if state.last_stop.is_some() {
            bail!("program is already stopped; use continue_ or the step actions");
        }
    }
    // DAP `pause` needs a thread id; with no recorded stop, ask the adapter.
    let body = session.request("threads", json!({})).await?;
    let thread_id = body
        .pointer("/threads/0/id")
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow!("adapter reported no threads; cannot pause"))?;
    session.request("pause", json!({ "threadId": thread_id })).await?;
    let wait = wait_duration(args)?;
    if wait.is_zero() {
        return Ok(text_result("Pause requested (wait_ms=0, not waiting)."));
    }
    wait_for_stop_and_report(session, "Paused", from, wait, abort).await
}

/// `step_over`/`step_in`/`step_out`: single-step the stopped thread and wait
/// for the step's `stopped` event.
async fn run_step(
    session: &DebugSession,
    action: &str,
    abort: &AbortSignal,
) -> Result<AgentToolResult> {
    // Capture before the step request so its `stopped` event is observed.
    let from = session.event_watermark();
    let stop = {
        let state = session.state.lock();
        if state.terminated {
            bail!("debug session is terminated; `debug launch` to start a new one");
        }
        match &state.last_stop {
            Some(stop) => stop.clone(),
            None if state.started => {
                bail!("program is running; `debug pause` first, then step")
            }
            None => bail!("program has not started yet; `debug continue_` starts it, then step"),
        }
    };
    let command = match action {
        "step_over" => "next",
        "step_in" => "stepIn",
        "step_out" => "stepOut",
        _ => unreachable!("run_step dispatch"),
    };
    // `next`/`stepIn` accept a granularity; `stepOut` has none per the spec.
    let params = match command {
        "next" | "stepIn" => json!({ "threadId": stop.thread_id, "granularity": "line" }),
        _ => json!({ "threadId": stop.thread_id }),
    };
    session.request(command, params).await?;
    session.state.lock().last_stop = None;
    wait_for_stop_and_report(session, "Stepped", from, STEP_WAIT, abort).await
}

/// `stack_trace`: frames plus the scopes of the first frames (their
/// `variables_reference` values feed the `variables` action).
async fn run_stack_trace(session: &DebugSession, args: &Value) -> Result<AgentToolResult> {
    let thread_id = {
        let state = session.state.lock();
        if state.terminated {
            bail!("debug session is terminated; `debug launch` to start a new one");
        }
        match &state.last_stop {
            Some(stop) => stop.thread_id,
            None if state.started => {
                bail!("program is running (no stopped frame); `debug pause` first")
            }
            None => bail!("program has not started yet; `debug continue_` starts it"),
        }
    };
    let thread_id = arg_int(args, "thread")?.unwrap_or(thread_id);
    let levels = arg_int(args, "levels")?
        .unwrap_or(MAX_FRAMES as i64)
        .clamp(1, 1000) as usize;
    let body = session
        .request(
            "stackTrace",
            json!({ "threadId": thread_id, "startFrame": 0, "levels": levels }),
        )
        .await?;
    let frames = body
        .get("stackFrames")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let total = body
        .get("totalFrames")
        .and_then(Value::as_i64)
        .unwrap_or(frames.len() as i64);
    let mut out = format!("Stack (thread {thread_id}, {} of {total} frames):", frames.len());
    let scope_frames = frames.len().min(MAX_SCOPES_FRAMES);
    for (index, frame) in frames.iter().enumerate() {
        let id = frame.get("id").and_then(Value::as_i64).unwrap_or(-1);
        let name = frame.get("name").and_then(Value::as_str).unwrap_or("?");
        let line = frame.get("line").and_then(Value::as_i64).unwrap_or(0);
        let column = frame.get("column").and_then(Value::as_i64).unwrap_or(0);
        let source = frame
            .pointer("/source/path")
            .and_then(Value::as_str)
            .or_else(|| frame.pointer("/source/name").and_then(Value::as_str))
            .unwrap_or("?");
        out.push_str(&format!("\n#{index} {name} at {source}:{line}:{column} (frame {id})"));
        if index < scope_frames {
            match session
                .request_timeout("scopes", json!({ "frameId": id }), REQUEST_TIMEOUT)
                .await
            {
                Ok(body) => {
                    let scopes = body
                        .get("scopes")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    for scope in &scopes {
                        let scope_name = scope.get("name").and_then(Value::as_str).unwrap_or("?");
                        let reference = scope
                            .get("variablesReference")
                            .and_then(Value::as_i64)
                            .unwrap_or(0);
                        out.push_str(&format!(
                            "\n    scope {scope_name} (variables_reference {reference})"
                        ));
                    }
                }
                Err(error) => out.push_str(&format!("\n    (scopes unavailable: {error})")),
            }
        }
    }
    Ok(text_result(bounded(&out)))
}

/// `variables`: expands a scope (or nested) `variablesReference`.
async fn run_variables(session: &DebugSession, args: &Value) -> Result<AgentToolResult> {
    let reference = arg_int(args, "variables_reference")?.ok_or_else(|| {
        anyhow!("debug variables requires `variables_reference` (a scope reference from `debug stack_trace`)")
    })?;
    if reference <= 0 {
        bail!("debug variables `variables_reference` must be a positive scope reference (0 means no children)");
    }
    let body = session
        .request("variables", json!({ "variablesReference": reference, "filter": "named" }))
        .await?;
    let variables = body
        .get("variables")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let shown = variables.len().min(MAX_VARIABLES);
    let mut out = format!("Variables (reference {reference}, {shown} shown):");
    for variable in variables.iter().take(MAX_VARIABLES) {
        let name = variable.get("name").and_then(Value::as_str).unwrap_or("?");
        let value = variable.get("value").and_then(Value::as_str).unwrap_or("");
        let mut line = format!("{name} = {value}");
        if let Some(vtype) = variable.get("type").and_then(Value::as_str) {
            line.push_str(&format!(" ({vtype})"));
        }
        let nested = variable.get("variablesReference").and_then(Value::as_i64).unwrap_or(0);
        if nested > 0 {
            line.push_str(&format!(" [nested reference {nested}]"));
        }
        out.push_str(&format!("\n{line}"));
    }
    if variables.len() > MAX_VARIABLES {
        out.push_str(&format!("\n[{} more variables omitted]", variables.len() - MAX_VARIABLES));
    }
    Ok(text_result(bounded(&out)))
}

/// `evaluate`: evaluates an expression in the stopped frame (frame 0 by
/// default when stopped; no frame when the program is not stopped).
async fn run_evaluate(session: &DebugSession, args: &Value) -> Result<AgentToolResult> {
    let expression = arg_str(args, "expression");
    let expression = expression.trim();
    if expression.is_empty() {
        bail!("debug evaluate requires an `expression`");
    }
    let frame_id = {
        let state = session.state.lock();
        if state.terminated {
            bail!("debug session is terminated; `debug launch` to start a new one");
        }
        match arg_int(args, "frame_id")? {
            Some(frame_id) => Some(frame_id),
            None if state.last_stop.is_some() => Some(0), // top frame
            None => None,
        }
    };
    let mut params = json!({ "expression": expression, "context": "repl" });
    if let Some(frame_id) = frame_id {
        params["frameId"] = json!(frame_id);
    }
    let body = session.request("evaluate", params).await?;
    let result = body.get("result").and_then(Value::as_str).unwrap_or("");
    let mut out = result.to_owned();
    if let Some(vtype) = body.get("type").and_then(Value::as_str) {
        out.push_str(&format!(" ({vtype})"));
    }
    Ok(text_result(out))
}

/// `threads`: lists the adapter's threads.
async fn run_threads(session: &DebugSession) -> Result<AgentToolResult> {
    let body = session.request("threads", json!({})).await?;
    let threads = body
        .get("threads")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if threads.is_empty() {
        return Ok(text_result("No threads."));
    }
    let shown = threads.len().min(MAX_THREADS);
    let mut out = format!("Threads ({shown}):");
    for thread in threads.iter().take(MAX_THREADS) {
        let id = thread.get("id").and_then(Value::as_i64).unwrap_or(-1);
        let name = thread.get("name").and_then(Value::as_str).unwrap_or("?");
        out.push_str(&format!("\n- {id}: {name}"));
    }
    Ok(text_result(bounded(&out)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead as _, Write as _};

    use serde_json::json;

    // -----------------------------------------------------------------------
    // Fake DAP adapter
    // -----------------------------------------------------------------------
    //
    // The tests below spawn a fake DAP adapter by re-executing this test
    // binary with `--exact tools::debug::tests::fake_dap_adapter_process` and
    // `PI_FAKE_DAP_ADAPTER=1`. The test then acts as a minimal adapter
    // speaking initialize/launch/setBreakpoints/configurationDone/continue/
    // stackTrace/scopes/variables/evaluate/threads/disconnect over
    // Content-Length framing (implemented independently of the client's
    // framing so a framing asymmetry fails the test).

    /// Runs the fake adapter loop when invoked via the env-var re-exec trick;
    /// a silent no-op when the test suite runs it directly.
    #[test]
    fn fake_dap_adapter_process() {
        if std::env::var_os("PI_FAKE_DAP_ADAPTER").is_none() {
            return;
        }
        // Boom mode (PI_FAKE_DAP_BOOM=1): log credential-shaped text to
        // stderr, then refuse initialize — exercises the stderr-tail
        // redaction in the launch-failure error path.
        let boom = std::env::var_os("PI_FAKE_DAP_BOOM").is_some();
        // Runs-forever mode (PI_FAKE_DAP_RUNS_FOREVER=1): a program with no
        // breakpoints runs indefinitely instead of exiting, so `pause` can be
        // exercised against a genuinely running program.
        let runs_forever = std::env::var_os("PI_FAKE_DAP_RUNS_FOREVER").is_some();
        if boom {
            let ghp = ["gh", "p_", "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij0123456789"].concat();
            let sk = ["s", "k-", "abcdefghijklmnop1234"].concat();
            eprintln!("fake dap adapter: token={ghp} {sk}");
            // Let the client's stderr reader drain the line before the
            // initialize error is answered, so the tail is captured.
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let stdin = std::io::stdin();
        let stdout = std::io::stdout();
        let mut reader = std::io::BufReader::new(stdin.lock());
        let mut writer = std::io::BufWriter::new(stdout.lock());
        let mut seq = 0i64;

        fn send(writer: &mut std::io::BufWriter<std::io::StdoutLock<'_>>, seq: &mut i64, body: &Value) {
            *seq += 1;
            let mut message = body.clone();
            message["seq"] = json!(*seq);
            let bytes = sync_encode(&message).expect("fake adapter encode");
            writer.write_all(&bytes).expect("fake adapter write");
            writer.flush().expect("fake adapter flush");
        }

        let mut breakpoints: Vec<i64> = Vec::new();
        loop {
            let message = sync_read_message(&mut reader).expect("fake adapter read");
            let command = message.get("command").and_then(Value::as_str);
            let request_seq = message.get("seq").and_then(Value::as_i64).unwrap_or(0);
            let respond = |writer: &mut std::io::BufWriter<std::io::StdoutLock<'_>>,
                           seq: &mut i64,
                           body: Value,
                           success: bool,
                           detail: Option<&str>| {
                let mut response = json!({
                    "type": "response",
                    "request_seq": request_seq,
                    "success": success,
                    "command": command,
                });
                if success {
                    response["body"] = body;
                } else if let Some(detail) = detail {
                    response["message"] = json!(detail);
                }
                send(writer, seq, &response);
            };
            match command {
                Some("initialize") if boom => respond(&mut writer, &mut seq, json!(null), false, Some("initialize refused by fake adapter")),
                Some("initialize") => respond(
                    &mut writer,
                    &mut seq,
                    json!({ "supportsConfigurationDoneRequest": true, "supportsSetVariable": false }),
                    true,
                    None,
                ),
                Some("launch") => {
                    respond(&mut writer, &mut seq, json!(null), true, None);
                    // The adapter signals configuration readiness.
                    send(
                        &mut writer,
                        &mut seq,
                        &json!({ "type": "event", "event": "initialized", "body": {} }),
                    );
                }
                Some("setBreakpoints") => {
                    let lines: Vec<i64> = message
                        .pointer("/arguments/breakpoints")
                        .and_then(Value::as_array)
                        .map(|bps| {
                            bps.iter()
                                .filter_map(|bp| bp.get("line").and_then(Value::as_i64))
                                .collect()
                        })
                        .unwrap_or_default();
                    breakpoints = lines.clone();
                    let bps: Vec<Value> = lines
                        .iter()
                        .enumerate()
                        .map(|(index, line)| {
                            json!({ "id": 1000 + index as i64, "verified": true, "line": line })
                        })
                        .collect();
                    respond(&mut writer, &mut seq, json!({ "breakpoints": bps }), true, None);
                }
                Some("configurationDone") => {
                    respond(&mut writer, &mut seq, json!(null), true, None);
                    // The program starts: print, then stop at the first
                    // breakpoint. With no breakpoints the program runs to
                    // completion — except in runs-forever mode, where it keeps
                    // running so `pause` can interrupt it.
                    send(
                        &mut writer,
                        &mut seq,
                        &json!({
                            "type": "event",
                            "event": "output",
                            "body": { "category": "stdout", "output": "hello from fake debuggee\n" }
                        }),
                    );
                    if let Some(&first) = breakpoints.first() {
                        send(
                            &mut writer,
                            &mut seq,
                            &json!({
                                "type": "event",
                                "event": "stopped",
                                "body": {
                                    "reason": "breakpoint",
                                    "description": "Paused on breakpoint",
                                    "threadId": 1,
                                    "allThreadsStopped": true,
                                    "hitBreakpointIds": [1000],
                                    "line": first
                                }
                            }),
                        );
                    } else if !runs_forever {
                        send(
                            &mut writer,
                            &mut seq,
                            &json!({ "type": "event", "event": "exited", "body": { "exitCode": 0 } }),
                        );
                        send(
                            &mut writer,
                            &mut seq,
                            &json!({ "type": "event", "event": "terminated", "body": {} }),
                        );
                    }
                }
                Some("continue") => {
                    // Resuming from a stop runs the program to completion.
                    respond(&mut writer, &mut seq, json!({ "allThreadsContinued": true }), true, None);
                    send(
                        &mut writer,
                        &mut seq,
                        &json!({ "type": "event", "event": "exited", "body": { "exitCode": 0 } }),
                    );
                    send(
                        &mut writer,
                        &mut seq,
                        &json!({ "type": "event", "event": "terminated", "body": {} }),
                    );
                }
                Some("pause") => {
                    respond(&mut writer, &mut seq, json!(null), true, None);
                    send(
                        &mut writer,
                        &mut seq,
                        &json!({
                            "type": "event",
                            "event": "stopped",
                            "body": { "reason": "pause", "threadId": 1, "allThreadsStopped": true }
                        }),
                    );
                }
                Some("next") | Some("stepIn") | Some("stepOut") => {
                    respond(&mut writer, &mut seq, json!(null), true, None);
                    send(
                        &mut writer,
                        &mut seq,
                        &json!({
                            "type": "event",
                            "event": "stopped",
                            "body": { "reason": "step", "threadId": 1, "allThreadsStopped": true }
                        }),
                    );
                }
                Some("stackTrace") => respond(
                    &mut writer,
                    &mut seq,
                    json!({
                        "stackFrames": [
                            { "id": 1, "name": "main", "source": { "name": "main.py", "path": "/fake/main.py" }, "line": 42, "column": 1 },
                            { "id": 2, "name": "helper", "source": { "name": "lib.py", "path": "/fake/lib.py" }, "line": 7, "column": 1 }
                        ],
                        "totalFrames": 2
                    }),
                    true,
                    None,
                ),
                Some("scopes") => {
                    let frame_id = message.pointer("/arguments/frameId").and_then(Value::as_i64).unwrap_or(0);
                    let reference = if frame_id == 1 { 10 } else { 20 };
                    respond(
                        &mut writer,
                        &mut seq,
                        json!({ "scopes": [{ "name": "Local", "variablesReference": reference, "expensive": false }] }),
                        true,
                        None,
                    );
                }
                Some("variables") => {
                    let reference = message.pointer("/arguments/variablesReference").and_then(Value::as_i64).unwrap_or(0);
                    match reference {
                        10 => respond(
                            &mut writer,
                            &mut seq,
                            json!({
                                "variables": [
                                    { "name": "x", "value": "42", "type": "int", "variablesReference": 0 },
                                    { "name": "items", "value": "List of length 2", "type": "list", "variablesReference": 11, "namedVariables": 2 }
                                ]
                            }),
                            true,
                            None,
                        ),
                        11 => respond(
                            &mut writer,
                            &mut seq,
                            json!({ "variables": [{ "name": "0", "value": "a", "type": "str", "variablesReference": 0 }] }),
                            true,
                            None,
                        ),
                        20 => respond(
                            &mut writer,
                            &mut seq,
                            json!({ "variables": [{ "name": "y", "value": "7", "type": "int", "variablesReference": 0 }] }),
                            true,
                            None,
                        ),
                        _ => respond(&mut writer, &mut seq, json!(null), false, Some("unknown variablesReference")),
                    }
                }
                Some("evaluate") => {
                    let expression = message.pointer("/arguments/expression").and_then(Value::as_str).unwrap_or("");
                    let (result, vtype) = match expression {
                        "x" => ("42", "int"),
                        "x + 1" => ("43", "int"),
                        other => (other, "str"),
                    };
                    respond(
                        &mut writer,
                        &mut seq,
                        json!({ "result": result, "type": vtype, "variablesReference": 0 }),
                        true,
                        None,
                    );
                }
                Some("threads") => respond(
                    &mut writer,
                    &mut seq,
                    json!({ "threads": [{ "id": 1, "name": "MainThread" }] }),
                    true,
                    None,
                ),
                Some("disconnect") => {
                    respond(&mut writer, &mut seq, json!(null), true, None);
                    return; // adapter exits
                }
                _ => respond(&mut writer, &mut seq, json!(null), false, Some("unknown command")),
            }
        }
    }

    /// Independent sync framing writer for the fake adapter, kept separate
    /// from the async client writer so a framing asymmetry fails the tests.
    fn sync_encode(body: &Value) -> std::io::Result<Vec<u8>> {
        let json = serde_json::to_vec(body)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let mut out = format!("Content-Length: {}\r\n\r\n", json.len()).into_bytes();
        out.extend_from_slice(&json);
        Ok(out)
    }

    /// Independent sync framing reader for the fake adapter.
    fn sync_read_message(reader: &mut impl std::io::BufRead) -> std::io::Result<Value> {
        let mut header = String::new();
        let mut content_length = None;
        loop {
            header.clear();
            if reader.read_line(&mut header)? == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "fake adapter stdin closed",
                ));
            }
            if header == "\r\n" || header == "\n" {
                break;
            }
            if let Some((name, value)) = header.trim_end().split_once(':') {
                if name.trim().eq_ignore_ascii_case("Content-Length") {
                    content_length = Some(value.trim().parse().map_err(|e| {
                        std::io::Error::new(std::io::ErrorKind::InvalidData, e)
                    })?);
                }
            }
        }
        let length = content_length.ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "missing Content-Length")
        })?;
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body)?;
        serde_json::from_slice(&body)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// Spawns the fake adapter (this test binary in fake-adapter mode).
    /// `runs_forever` puts the fake in runs-forever mode (see
    /// [`fake_dap_adapter_process`]).
    async fn spawn_fake_session_with(boom: bool, runs_forever: bool) -> DebugSession {
        let exe = std::env::current_exe().expect("test binary path");
        let mut command = Command::new(exe);
        command
            .arg("tools::debug::tests::fake_dap_adapter_process")
            .arg("--nocapture")
            .env("PI_FAKE_DAP_ADAPTER", "1");
        if boom {
            command.env("PI_FAKE_DAP_BOOM", "1");
        }
        if runs_forever {
            command.env("PI_FAKE_DAP_RUNS_FOREVER", "1");
        }
        DebugSession::spawn_command(command, "gdb")
            .await
            .expect("fake adapter spawn")
    }

    /// Spawns the fake adapter in normal mode.
    async fn spawn_fake_session(boom: bool) -> DebugSession {
        spawn_fake_session_with(boom, false).await
    }

    /// Launches a fake session into a fresh registry slot.
    async fn registry_with_launched_fake() -> DebugRegistry {
        let registry = DebugRegistry::new();
        let session = spawn_fake_session(false).await;
        session
            .launch(&json!({ "program": "/fake/main.py", "args": ["--flag"] }), &AbortSignal::none())
            .await
            .expect("fake adapter launches");
        *registry.lock().await = Some(session);
        registry
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_command_recovers_from_transient_etxtbsy() {
        // execve of an executable that is open for writing returns ETXTBSY
        // ("Text file busy") — the exact condition that flakes DAP adapter
        // spawns under parallel load (the test binary is re-exec'd as the
        // fake adapter). The session spawn must retry until the transient
        // condition clears instead of failing on the first attempt. The
        // running test binary itself cannot be opened for writing (open(2)
        // already fails with ETXTBSY), so a copy is used.
        let exe = std::env::current_exe().expect("test binary path");
        let dir = tempfile::tempdir().expect("tmp dir");
        let copy = dir.path().join("fake-adapter-copy");
        std::fs::copy(&exe, &copy).expect("copy test binary");
        let write_handle = std::fs::OpenOptions::new()
            .write(true)
            .open(&copy)
            .expect("open copied binary for writing");
        // Probe: while the handle is open, a bare spawn must hit ETXTBSY.
        let mut probe = std::process::Command::new(&copy);
        let probe_error = probe
            .arg("--version")
            .spawn()
            .expect_err("bare spawn of a write-open executable must fail with ETXTBSY");
        assert_eq!(
            probe_error.raw_os_error(),
            Some(nix::errno::Errno::ETXTBSY as i32),
            "{probe_error}"
        );
        drop(probe_error);
        // Release the handle shortly after the first retry attempt, so the
        // retry loop (not the first attempt) succeeds — the recovery path.
        let closer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(5));
            drop(write_handle);
        });
        let mut command = Command::new(&copy);
        command
            .arg("tools::debug::tests::fake_dap_adapter_process")
            .arg("--nocapture")
            .env("PI_FAKE_DAP_ADAPTER", "1");
        let session = DebugSession::spawn_command(command, "gdb")
            .await
            .expect("spawn must recover from transient ETXTBSY");
        closer.join().expect("closer thread");
        // The recovered session is live: a launch round trip works.
        session
            .launch(
                &json!({ "program": "/fake/main.py" }),
                &AbortSignal::none(),
            )
            .await
            .expect("launch over recovered session");
    }

    // -----------------------------------------------------------------------
    // Adapter resolution
    // -----------------------------------------------------------------------

    #[test]
    fn resolve_adapter_command_rejects_unknown_adapter() {
        let err = resolve_adapter_command("frobnicator", "/nonexistent")
            .unwrap_err()
            .to_string();
        assert!(err.contains("frobnicator"), "{err}");
        for supported in ["gdb", "lldb-dap", "debugpy"] {
            assert!(err.contains(supported), "missing {supported} in: {err}");
        }
    }

    #[test]
    fn resolve_adapter_command_missing_binary_is_actionable() {
        let err = resolve_adapter_command("gdb", "/nonexistent").unwrap_err().to_string();
        assert!(err.contains("gdb"), "{err}");
        assert!(err.contains("PATH"), "{err}");
        let err = resolve_adapter_command("debugpy", "/nonexistent")
            .unwrap_err()
            .to_string();
        assert!(err.contains("debugpy"), "{err}");
        assert!(err.contains("python3"), "{err}");
    }

    #[test]
    fn resolve_adapter_command_finds_binary_on_path() {
        let dir = tempfile::tempdir().expect("tmp");
        let gdb = dir.path().join("gdb");
        std::fs::write(&gdb, "#!/bin/sh\n").expect("write fake gdb");
        let path = dir.path().to_string_lossy().into_owned();
        let (binary, argv) = resolve_adapter_command("gdb", &path).expect("resolves");
        assert_eq!(binary, gdb.to_string_lossy());
        assert_eq!(argv, vec!["-q".to_owned(), "-i".to_owned(), "dap".to_owned()]);
    }

    #[test]
    fn build_launch_params_merges_pass_through_args() {
        let params = build_launch_params(
            "debugpy",
            &json!({
                "program": "app.py",
                "args": ["-v"],
                "cwd": "/tmp/work",
                "launch_args": { "stopOnEntry": true, "env": { "K": "V" } }
            }),
        )
        .expect("launch params");
        assert_eq!(params["type"], "debugpy");
        assert_eq!(params["request"], "launch");
        assert_eq!(params["program"], "app.py");
        assert_eq!(params["args"], json!(["-v"]));
        assert_eq!(params["cwd"], "/tmp/work");
        assert_eq!(params["stopOnEntry"], true);
        assert_eq!(params["env"], json!({ "K": "V" }));
        // The pass-through object can override defaults.
        let overridden = build_launch_params(
            "gdb",
            &json!({ "program": "a.out", "launch_args": { "request": "attach" } }),
        )
        .expect("launch params");
        assert_eq!(overridden["request"], "attach");
    }

    // -----------------------------------------------------------------------
    // Session protocol round trip
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn session_launch_breakpoint_continue_stack_round_trip() {
        let session = spawn_fake_session(false).await;
        let abort = AbortSignal::none();
        session
            .launch(&json!({ "program": "/fake/main.py" }), &abort)
            .await
            .expect("launch handshake");

        let body = session
            .request(
                "setBreakpoints",
                json!({
                    "source": { "name": "main.py", "path": "/fake/main.py" },
                    "breakpoints": [{ "line": 42 }],
                    "linesStartAt1": true
                }),
            )
            .await
            .expect("setBreakpoints");
        assert_eq!(body["breakpoints"][0]["verified"], true);
        assert_eq!(body["breakpoints"][0]["line"], 42);

        // Capture the watermark before the request so the stop events it
        // triggers are observed by the wait below.
        let from = session.event_watermark();
        session.request("configurationDone", json!({})).await.expect("configurationDone");
        let event = session
            .wait_for_event(is_stop_event, from, REQUEST_TIMEOUT, &abort)
            .await
            .expect("stop event");
        assert_eq!(event["event"], "stopped");
        assert_eq!(event["body"]["reason"], "breakpoint");
        assert_eq!(event["body"]["threadId"], 1);

        let body = session
            .request("stackTrace", json!({ "threadId": 1, "startFrame": 0, "levels": 50 }))
            .await
            .expect("stackTrace");
        let frames = body["stackFrames"].as_array().expect("frames");
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0]["name"], "main");
        assert_eq!(frames[0]["line"], 42);

        let body = session.request("scopes", json!({ "frameId": 1 })).await.expect("scopes");
        assert_eq!(body["scopes"][0]["variablesReference"], 10);

        let body = session
            .request("variables", json!({ "variablesReference": 10 }))
            .await
            .expect("variables");
        assert_eq!(body["variables"][0]["name"], "x");
        assert_eq!(body["variables"][0]["value"], "42");
        assert_eq!(body["variables"][1]["variablesReference"], 11);

        let body = session
            .request("evaluate", json!({ "expression": "x + 1", "frameId": 1 }))
            .await
            .expect("evaluate");
        assert_eq!(body["result"], "43");

        let body = session.request("threads", json!({})).await.expect("threads");
        assert_eq!(body["threads"][0]["id"], 1);

        // The watermark is taken BEFORE the continue request so the
        // exited/terminated pair the adapter sends after its response is
        // observed together: taking it after the response round trip can
        // split the pair (exited queued, watermark read, terminated queued),
        // making the wait report "terminated" instead of "exited".
        let from = session.event_watermark();
        session
            .request("continue", json!({ "threadId": 1 }))
            .await
            .expect("continue");
        let event = session
            .wait_for_event(is_stop_event, from, REQUEST_TIMEOUT, &abort)
            .await
            .expect("exit event");
        assert_eq!(event["event"], "exited");
        assert_eq!(event["body"]["exitCode"], 0);

        let report = session.terminate().await;
        assert!(report.contains("terminated"), "{report}");
    }

    #[tokio::test]
    async fn session_launch_failure_redacts_adapter_stderr() {
        // A fake adapter that logs credential-shaped text to stderr and then
        // refuses initialize: the launch error must embed the stderr tail
        // but never the secret values.
        let session = spawn_fake_session(true).await;
        let err = session
            .launch(&json!({ "program": "/fake/main.py" }), &AbortSignal::none())
            .await
            .expect_err("initialize must fail");
        let text = err.to_string();
        assert!(text.contains("failed to launch"), "{text}");
        assert!(text.contains("--- adapter stderr ---"), "{text}");
        let ghp = ["gh", "p_", "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij0123456789"].concat();
        let sk = ["s", "k-", "abcdefghijklmnop1234"].concat();
        for secret in [ghp.as_str(), sk.as_str()] {
            assert!(!text.contains(secret), "{secret} leaked: {text}");
        }
        assert!(text.contains("[REDACTED]"), "redaction marker missing: {text}");
    }

    // -----------------------------------------------------------------------
    // Tool surface
    // -----------------------------------------------------------------------

    /// Concatenates the text blocks of a tool result.
    fn result_text(result: &AgentToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|block| match block {
                pi_ai::ContentBlock::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    fn tool_context(arguments: Value) -> pi_agent::ToolCallContext {
        pi_agent::ToolCallContext {
            tool_call_id: "1".to_owned(),
            arguments,
            on_update: Arc::new(|_| {}),
            abort: AbortSignal::none(),
            model: None,
        }
    }

    async fn tool_text(tool: &AgentTool, arguments: Value) -> Result<String> {
        let result = (tool.execute)(tool_context(arguments)).await?;
        Ok(result_text(&result))
    }

    async fn run_text(registry: &DebugRegistry, cwd: &str, arguments: Value) -> Result<String> {
        let result = run_debug(registry, cwd, arguments, AbortSignal::none()).await?;
        Ok(result_text(&result))
    }

    #[tokio::test]
    async fn debug_tool_built_errors_without_adapter() {
        let tool = debug_tool("/tmp");
        let err = tool_text(&tool, json!({ "action": "threads" }))
            .await
            .expect_err("no adapter must fail");
        assert!(err.to_string().contains("no DAP adapter running"), "{err}");
        assert!(err.to_string().contains("launch"), "{err}");
    }

    #[tokio::test]
    async fn debug_tool_launch_validation_errors() {
        let registry = DebugRegistry::new();
        let cwd = "/tmp";

        let err = run_text(&registry, cwd, json!({})).await.expect_err("missing action");
        assert!(err.to_string().contains("action is required"), "{err}");

        let err = run_text(
            &registry,
            cwd,
            json!({ "action": "launch", "adapter": "frobnicator", "program": "app.py" }),
        )
        .await
        .expect_err("unknown adapter");
        assert!(err.to_string().contains("unsupported debug adapter"), "{err}");

        let err = run_text(&registry, cwd, json!({ "action": "launch", "adapter": "gdb" }))
            .await
            .expect_err("missing program");
        assert!(err.to_string().contains("program"), "{err}");

        let err = run_text(&registry, cwd, json!({ "action": "launch", "program": "app.py" }))
            .await
            .expect_err("missing adapter");
        assert!(err.to_string().contains("adapter"), "{err}");

        let err = run_text(&registry, cwd, json!({ "action": "fly" }))
            .await
            .expect_err("unknown action");
        assert!(err.to_string().contains("unknown debug action `fly`"), "{err}");
        assert!(err.to_string().contains("launch"), "{err}");
    }

    #[tokio::test]
    async fn debug_tool_rejects_second_launch_while_running() {
        let registry = registry_with_launched_fake().await;
        let err = run_text(
            &registry,
            "/tmp",
            json!({ "action": "launch", "adapter": "gdb", "program": "app.py" }),
        )
        .await
        .expect_err("second launch must fail");
        assert!(err.to_string().contains("already running"), "{err}");
        assert!(err.to_string().contains("terminate"), "{err}");
    }

    #[tokio::test]
    async fn debug_tool_actions_round_trip() {
        let registry = registry_with_launched_fake().await;
        let cwd = "/tmp";

        // set_breakpoint while the program is paused before start.
        let text = run_text(
            &registry,
            cwd,
            json!({ "action": "set_breakpoint", "file": "main.py", "line": 42 }),
        )
        .await
        .expect("set_breakpoint");
        assert!(text.contains("main.py:42 verified"), "{text}");

        // continue_ sends configurationDone; the fake stops at the breakpoint.
        let text = run_text(&registry, cwd, json!({ "action": "continue_" }))
            .await
            .expect("continue_");
        assert!(text.contains("Program stopped"), "{text}");
        assert!(text.contains("reason: breakpoint"), "{text}");
        assert!(text.contains("main (/fake/main.py:42)"), "{text}");
        assert!(text.contains("hello from fake debuggee"), "{text}");

        // stack_trace: frames plus scopes with their references.
        let text = run_text(&registry, cwd, json!({ "action": "stack_trace" }))
            .await
            .expect("stack_trace");
        assert!(text.contains("Stack (thread 1, 2 of 2 frames)"), "{text}");
        assert!(text.contains("#0 main at /fake/main.py:42:1 (frame 1)"), "{text}");
        assert!(text.contains("scope Local (variables_reference 10)"), "{text}");

        // variables: expand a scope reference.
        let text = run_text(
            &registry,
            cwd,
            json!({ "action": "variables", "variables_reference": 10 }),
        )
        .await
        .expect("variables");
        assert!(text.contains("x = 42 (int)"), "{text}");
        assert!(text.contains("items"), "{text}");
        assert!(text.contains("[nested reference 11]"), "{text}");

        // evaluate in the top frame.
        let text = run_text(
            &registry,
            cwd,
            json!({ "action": "evaluate", "expression": "x" }),
        )
        .await
        .expect("evaluate");
        assert_eq!(text.trim(), "42 (int)");

        // threads.
        let text = run_text(&registry, cwd, json!({ "action": "threads" }))
            .await
            .expect("threads");
        assert!(text.contains("1: MainThread"), "{text}");

        // step_over completes with a new stopped event.
        let text = run_text(&registry, cwd, json!({ "action": "step_over" }))
            .await
            .expect("step_over");
        assert!(text.contains("Stepped"), "{text}");
        assert!(text.contains("reason: step"), "{text}");

        // continue_ again: the fake program finishes.
        let text = run_text(&registry, cwd, json!({ "action": "continue" }))
            .await
            .expect("continue alias");
        assert!(text.contains("Program exited with code 0"), "{text}");

        // terminate kills the adapter; the slot is freed for a relaunch.
        let text = run_text(&registry, cwd, json!({ "action": "terminate" }))
            .await
            .expect("terminate");
        assert!(text.contains("terminated"), "{text}");
        let err = run_text(&registry, cwd, json!({ "action": "threads" }))
            .await
            .expect_err("slot cleared after terminate");
        assert!(err.to_string().contains("no DAP adapter running"), "{err}");
    }

    #[tokio::test]
    async fn debug_tool_continue_before_launch_errors() {
        let registry = DebugRegistry::new();
        let err = run_text(&registry, "/tmp", json!({ "action": "continue_" }))
            .await
            .expect_err("no adapter");
        assert!(err.to_string().contains("no DAP adapter running"), "{err}");
    }

    #[tokio::test]
    async fn debug_tool_set_breakpoint_validation_errors() {
        let registry = registry_with_launched_fake().await;
        let cwd = "/tmp";
        let err = run_text(&registry, cwd, json!({ "action": "set_breakpoint", "line": 42 }))
            .await
            .expect_err("missing file");
        assert!(err.to_string().contains("`file`"), "{err}");
        let err = run_text(&registry, cwd, json!({ "action": "set_breakpoint", "file": "a.py" }))
            .await
            .expect_err("missing line");
        assert!(err.to_string().contains("`line`"), "{err}");
        let err = run_text(
            &registry,
            cwd,
            json!({ "action": "set_breakpoint", "file": "a.py", "line": 0 }),
        )
        .await
        .expect_err("zero line");
        assert!(err.to_string().contains("1-based"), "{err}");
    }

    #[tokio::test]
    async fn debug_tool_variables_requires_reference() {
        let registry = registry_with_launched_fake().await;
        let err = run_text(&registry, "/tmp", json!({ "action": "variables" }))
            .await
            .expect_err("missing reference");
        assert!(err.to_string().contains("variables_reference"), "{err}");
    }

    #[tokio::test]
    async fn debug_tool_continue_with_wait_ms_zero_returns_immediately() {
        // Runs-forever mode: the program does not exit after configurationDone,
        // so it is genuinely running after a no-wait continue.
        let registry = DebugRegistry::new();
        let session = spawn_fake_session_with(false, true).await;
        session
            .launch(&json!({ "program": "/fake/main.py" }), &AbortSignal::none())
            .await
            .expect("launch");
        *registry.lock().await = Some(session);
        let text = run_text(
            &registry,
            "/tmp",
            json!({ "action": "continue_", "wait_ms": 0 }),
        )
        .await
        .expect("continue_ without wait");
        assert!(text.contains("wait_ms=0"), "{text}");
        // The program is running: a second continue_ must be rejected.
        let err = run_text(&registry, "/tmp", json!({ "action": "continue_" }))
            .await
            .expect_err("already running");
        assert!(err.to_string().contains("already running"), "{err}");
        // pause interrupts and waits for the stop.
        let text = run_text(&registry, "/tmp", json!({ "action": "pause" }))
            .await
            .expect("pause");
        assert!(text.contains("Paused"), "{text}");
        assert!(text.contains("reason: pause"), "{text}");
        // Clean up.
        let text = run_text(&registry, "/tmp", json!({ "action": "terminate" }))
            .await
            .expect("terminate");
        assert!(text.contains("terminated"), "{text}");
    }
}
