//! `eval` tool: session-scoped persistent language kernels (Python + JS) with
//! cross-cell state, mirroring OMP's eval semantics.
//!
//! # Python kernel
//!
//! A `python3 -u -I -c <driver>` subprocess speaking a minimal bounded
//! line-based REPL protocol. The client writes `<byte-length>\n<code>` on
//! stdin; the driver answers `<byte-length>\n<JSON frame>` on its stdout
//! carrying `stdout`, `stderr`, `error` (traceback), `error_type`, and
//! `result` (the `repr()` of the last expression, IPython-style). During
//! each cell the driver redirects process fds 1/2 to driver-owned capture
//! pipes (drained concurrently in-driver), so raw fd-level output from user
//! code (`os.write(1, ...)`, `sys.__stdout__.buffer`, subprocesses
//! inheriting the descriptors) is captured, merged into the response frame,
//! and can never desync the framed protocol. The driver enforces ONE
//! aggregate output budget (96 KiB across all streams, 64 KiB per stream,
//! serialized frame ≤ 96 KiB), so the whole frame always stays under the
//! reader's hard 128 KiB bound with headroom even when several streams are
//! large at once. Top-level assignments persist in the driver's namespace
//! across cells (cross-cell state). The child leads its own process group; a
//! per-cell timeout or an abort SIGKILLs the whole group and the next call
//! lazily spawns a fresh kernel ("respawn after error").
//!
//! # JS kernel
//!
//! The embedded QuickJS runtime (rquickjs) — no `node` dependency. QuickJS
//! runtimes are `!Send` without the `parallel` feature, so the kernel runs in
//! a dedicated OS thread owning one [`Runtime`] + [`Context`], speaking the
//! same request/response shape over `std::sync::mpsc` + tokio oneshot
//! channels. Cells are evaluated as *global scripts* in strict mode (the
//! rquickjs `eval` default), so top-level `var`/`let`/`const`/`function`
//! declarations persist across cells exactly like a REPL. A `console.log`
//! shim captures into a per-cell buffer; the completion value is
//! JSON-stringified (falling back to `String()`), matching a Node-style REPL.
//! The runtime gets a 64 MiB memory cap and an interrupt handler enforcing
//! the per-cell deadline; a timeout tears the kernel down and the next call
//! respawns it.
//!
//! # Session scope
//!
//! One Python kernel + one JS kernel per tool instance, lazily spawned on
//! first use and killed on timeout/error (mirrors the `debug`/`mcp`
//! session-scoped registries). State is lost when a kernel dies. Both kernels
//! bound output at 64 KiB per stream; every rendered result runs through the
//! secret redactor.
//!
//! # Security / bounds
//!
//! - Python runs with `-I` (isolated mode): `PYTHONPATH`/`PYTHONSTARTUP` are
//!   ignored and the working directory is removed from `sys.path`, so the
//!   kernel cannot silently import ambient user modules. It still has full
//!   standard-library and filesystem access, like the `bash` tool — the eval
//!   tool is an execution tool, not a sandbox.
//! - JS is an embedded engine with no `require`/`import`, no network, and no
//!   filesystem access; its only ambient surface is the `console` shim.
//! - Per-cell timeout (default 30s, bounded 1..=300s), 64 KiB per output
//!   stream with a single 96 KiB aggregate output budget (Python driver,
//!   enforced before serialization), 128 KiB hard frame bound, and a 64 MiB
//!   JS memory cap.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context as _, Result};
use parking_lot::Mutex;
use rquickjs::{Context, Ctx, FromJs, Function, String as JsString, Value as JsValue, Runtime};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex as AsyncMutex;

use pi_agent::{AbortSignal, AgentTool, AgentToolResult, ToolCallContext, ToolCapability};

use crate::redact::redact_secrets;
use crate::truncate::truncate_tail;

use super::{arg_int, arg_str, check_aborted, s_number, s_object, s_string, text_result};

/// Default per-cell timeout, matching the debug tool's request timeout.
const DEFAULT_CELL_TIMEOUT: Duration = Duration::from_secs(30);
/// Upper bound for the user-supplied `timeout` argument (seconds).
const MAX_CELL_TIMEOUT_SECONDS: i64 = 300;
/// Per-stream output bound (stdout/stderr/result/traceback), enforced inside
/// the Python driver and on the Rust side for the JS kernel.
const MAX_STREAM_BYTES: usize = 64 * 1024;
/// Aggregate output budget the Python driver enforces across ALL frame
/// streams (stdout + stderr + result + traceback) before serialization, so
/// several simultaneously-large streams cannot push the frame toward the
/// hard bound. Mirrors the driver's `MAX_TOTAL`/`MAX_PAYLOAD` (96 KiB).
const MAX_AGGREGATE_STREAM_BYTES: usize = 96 * 1024;
/// Hard bound on a single protocol frame. The Python driver enforces the
/// aggregate budget so the serialized frame stays ≤ 96 KiB; this is a
/// defense-in-depth guard: an oversized frame means a broken/rogue kernel,
/// which is killed.
const MAX_FRAME_BYTES: usize = 128 * 1024;
/// Bound on the length header line (protocol desync guard).
const MAX_HEADER_BYTES: usize = 64;
/// Grace for the child to exit after being killed.
const EXIT_TIMEOUT: Duration = Duration::from_secs(2);
/// JS kernel memory ceiling (matches the extension runtime's cap).
const JS_MEMORY_LIMIT_BYTES: usize = 64 * 1024 * 1024;
/// JS kernel stack ceiling (QuickJS default is 256 KiB; generous but bounded).
const JS_STACK_LIMIT_BYTES: usize = 1024 * 1024;

/// Error classification for a cell that did not complete cleanly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EvalErrorKind {
    /// The code did not parse (`SyntaxError`/`IndentationError`/`TabError`).
    Syntax,
    /// The code parsed but threw while running.
    Runtime,
    /// The per-cell deadline elapsed; the kernel was killed.
    Timeout,
}

/// A classified cell failure: [`EvalErrorKind`] plus the actionable detail
/// (traceback / JS stack / timeout notice).
#[derive(Debug)]
pub(crate) struct CellError {
    pub kind: EvalErrorKind,
    pub text: String,
}

/// The outcome of evaluating one cell: bounded stdout/stderr, the repr of the
/// last expression (when the cell is an expression or ends with one), and an
/// optional classified error.
#[derive(Debug)]
pub(crate) struct CellResult {
    pub stdout: String,
    pub stderr: String,
    pub result: Option<String>,
    pub error: Option<CellError>,
}

impl CellResult {
    /// True when the cell hit the per-cell deadline (the kernel is dead and
    /// the caller must respawn on the next call).
    pub(crate) fn is_timeout(&self) -> bool {
        matches!(
            self.error,
            Some(CellError { kind: EvalErrorKind::Timeout, .. })
        )
    }
}

// ---------------------------------------------------------------------------
// Python kernel
// ---------------------------------------------------------------------------

/// The Python REPL driver, run via `python3 -u -I -c <this source>`. It reads
/// length-prefixed code cells from stdin, executes them in a persistent
/// namespace (cross-cell state), redirects user stdout/stderr into the frame,
/// and answers with a length-prefixed JSON frame. Every stream is truncated
/// at [`MAX_STREAM_BYTES`] so responses stay bounded.
const PYTHON_DRIVER_SOURCE: &str = r#"
import ast, contextlib, io, json, os, sys, threading, time, traceback

# Per-stream cap and the aggregate budget across stdout/stderr/result/
# traceback. The serialized frame is additionally capped at MAX_PAYLOAD, so
# a chatty cell (even across several streams at once) can never produce a
# frame near the reader's hard MAX_FRAME_BYTES bound with headroom.
MAX_STREAM = 64 * 1024
MAX_TOTAL = 96 * 1024
MAX_PAYLOAD = 96 * 1024
_NAMESPACE = {"__name__": "__eval__", "__builtins__": __builtins__}

# The framed protocol is spoken over a SAVED copy of fd 1. During each cell
# process fds 1/2 are redirected to driver-owned capture pipes, so raw
# fd-level writes from user code (os.write, sys.__stdout__.buffer,
# subprocesses inheriting the descriptors) land in the captures and are
# merged into the response frame — they can never interleave with protocol
# frames, and subprocesses (close_fds=True by default) inherit only the
# capture descriptors, never the protocol one.
_SAVED_OUT = os.dup(1)
_SAVED_ERR = os.dup(2)
_PROTO = os.fdopen(_SAVED_OUT, "wb", buffering=0)


class _Capture(object):
    """Redirects one process fd to a driver-owned pipe for the duration of a
    cell. A daemon thread drains the pipe concurrently so a chatty
    subprocess can never deadlock the kernel; captured bytes are bounded
    (the tail is kept) and merged into the response frame."""

    def __init__(self, fd, saved):
        self.fd = fd
        self.saved = saved
        self.read_fd, self.write_fd = os.pipe()
        os.set_blocking(self.read_fd, False)
        self.buf = bytearray()
        self._stop = threading.Event()
        self._thread = None

    def __enter__(self):
        os.dup2(self.write_fd, self.fd)

        def drain():
            while True:
                try:
                    chunk = os.read(self.read_fd, 65536)
                except BlockingIOError:
                    if self._stop.is_set():
                        return
                    time.sleep(0.005)
                    continue
                if not chunk:
                    return
                self.buf.extend(chunk)
                if len(self.buf) > MAX_STREAM:
                    del self.buf[: len(self.buf) - MAX_STREAM]

        self._thread = threading.Thread(target=drain, daemon=True)
        self._thread.start()
        return self

    def __exit__(self, *exc_info):
        os.dup2(self.saved, self.fd)
        os.close(self.write_fd)
        self._stop.set()
        if self._thread is not None:
            self._thread.join(timeout=1.0)
        # One final non-blocking drain: bytes written just before the
        # restore are still buffered in the pipe.
        while True:
            try:
                chunk = os.read(self.read_fd, 65536)
            except BlockingIOError:
                break
            if not chunk:
                break
            self.buf.extend(chunk)
            if len(self.buf) > MAX_STREAM:
                del self.buf[: len(self.buf) - MAX_STREAM]
        os.close(self.read_fd)
        return False

    def text(self):
        return bytes(self.buf).decode("utf-8", "replace")


def _truncate(text, cap):
    if text is None:
        return None
    if len(text) <= cap:
        return text
    marker = "\n...[output truncated at %d KiB]" % max(1, (cap + 1023) // 1024)
    return text[: max(0, cap - len(marker))] + marker


def _read_exact(n):
    data = b""
    while len(data) < n:
        chunk = sys.stdin.buffer.read(n - len(data))
        if not chunk:
            raise EOFError("kernel stdin closed")
        data += chunk
    return data


def _read_message():
    line = sys.stdin.buffer.readline()
    if not line:
        return None
    try:
        n = int(line.strip())
    except ValueError:
        return None
    if n < 0 or n > (1 << 20):
        return None
    return _read_exact(n).decode("utf-8", "replace")


def _send_frame(fields):
    # fields: ordered list of (key, text-or-None). Per-stream caps start at
    # MAX_STREAM and shrink (halving) until the serialized frame fits
    # MAX_PAYLOAD — escaping/control characters can inflate the payload
    # beyond the raw text budget, and the frame must stay bounded regardless.
    cap = MAX_STREAM
    while True:
        budget = MAX_TOTAL
        frame = {}
        for key, text in fields:
            if text is None:
                frame[key] = None
                continue
            text = _truncate(text, min(cap, budget))
            budget -= len(text)
            frame[key] = text
        payload = json.dumps(frame, ensure_ascii=False).encode("utf-8")
        if len(payload) <= MAX_PAYLOAD or cap <= 1024:
            break
        cap //= 2
    # Frame = "<byte-length>\\n" + payload, with no trailing newline, so the
    # reader can consume exactly one frame and start the next header cleanly.
    _PROTO.write(str(len(payload)).encode("ascii") + b"\n")
    _PROTO.write(payload)
    _PROTO.flush()


def _run_cell(code):
    stdout = io.StringIO()
    stderr = io.StringIO()
    result = None
    error = None
    error_type = None
    with _Capture(1, _SAVED_OUT) as raw_out, _Capture(2, _SAVED_ERR) as raw_err:
        try:
            tree = ast.parse(code, mode="exec")
            last = None
            if tree.body and isinstance(tree.body[-1], ast.Expr):
                last = tree.body.pop()
            with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
                exec(compile(tree, "<eval-cell>", "exec"), _NAMESPACE)
                if last is not None:
                    result = eval(compile(ast.Expression(last.value), "<eval-cell>", "eval"), _NAMESPACE)
        except BaseException as exc:
            error = traceback.format_exc()
            error_type = type(exc).__name__
    raw_stdout = raw_out.text()
    raw_stderr = raw_err.text()
    result_repr = None
    if result is not None:
        try:
            result_repr = repr(result)
        except BaseException:
            result_repr = "<unprintable result>"
    _send_frame([
        ("stdout", raw_stdout + stdout.getvalue()),
        ("stderr", raw_stderr + stderr.getvalue()),
        ("error", error),
        ("error_type", error_type),
        ("result", result_repr),
    ])


while True:
    code = _read_message()
    if code is None:
        break
    _run_cell(code)
"#;

/// Resolves the python kernel command: the interpreter (default `python3`,
/// falling back to `python`, resolved on `$PATH`) plus the driver argv.
/// `override_bin` bypasses the default candidate list (used by tests and the
/// notebook tool's override) but still resolves on `$PATH` so a missing
/// interpreter fails actionably.
pub(crate) fn python_command(override_bin: Option<&str>) -> Result<(String, Vec<String>)> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let path = path.to_string_lossy();
    let bin = match override_bin {
        Some(bin) => resolve_python_from(&path, &[bin])?,
        None => resolve_python_from(&path, &["python3", "python"])?,
    };
    let args = vec![
        "-u".to_owned(),
        "-I".to_owned(),
        "-c".to_owned(),
        PYTHON_DRIVER_SOURCE.to_owned(),
    ];
    Ok((bin, args))
}

/// Locates `binary` on the `$PATH` string `path` (mirrors the debug tool's
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

/// Resolves the first candidate that exists on `$PATH`; the error names the
/// interpreter and the install hint so a missing python is actionable.
fn resolve_python_from(path: &str, candidates: &[&str]) -> Result<String> {
    for candidate in candidates {
        if let Some(resolved) = find_in_path(path, candidate) {
            return Ok(resolved.to_string_lossy().into_owned());
        }
    }
    bail!(
        "The eval tool's Python kernel requires `python3` (or `python`) on PATH; \
         install Python 3 and retry"
    )
}

/// SIGKILLs the kernel's process group (spawned with `process_group(0)`, so
/// this reaps any grandchildren too). Mirrors the hooks.rs/debug.rs pattern.
fn kill_process_group(child: &Child) {
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

/// What the framed-response wait ended with: the frame, an abort, or the
/// per-cell deadline.
enum FrameWait {
    Frame(Result<Vec<u8>>),
    Aborted,
    TimedOut,
}

/// A live Python kernel: the driver subprocess plus framed stdin/stdout.
/// Drop kills the whole process group so a kernel is never leaked, even on
/// panic paths.
pub(crate) struct PythonKernel {
    child: Child,
    stdin: AsyncMutex<ChildStdin>,
    stdout: AsyncMutex<BufReader<ChildStdout>>,
    stderr_tail: Arc<Mutex<String>>,
    dead: Arc<AtomicBool>,
}

impl PythonKernel {
    /// Spawns the Python kernel with the resolved `python3`/`python`
    /// interpreter, rooted at the session `cwd`.
    pub(crate) async fn spawn(cwd: &str) -> Result<Self> {
        Self::spawn_with(None, cwd).await
    }

    /// [`Self::spawn`] with an explicit interpreter override (notebook tool
    /// override + tests). `None` resolves `python3`/`python` on `$PATH`.
    pub(crate) async fn spawn_with(python_override: Option<&str>, cwd: &str) -> Result<Self> {
        let (bin, args) = python_command(python_override)?;
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        Self::spawn_raw(&bin, &args, cwd).await
    }

    /// Raw spawn with an explicit command line — test hook for fake kernels
    /// (e.g. a `sh` script that echoes protocol frames) and the notebook
    /// tool's interpreter override.
    ///
    /// The driver keeps the framed protocol on its stdout: during each cell
    /// it redirects process fds 1/2 to driver-owned capture pipes (drained
    /// in-driver), so raw fd-level output from user code (os.write,
    /// `sys.__stdout__.buffer`, subprocesses inheriting the descriptors)
    /// lands in the capture and is merged into the response frame — it can
    /// never interleave with the framed protocol.
    pub(crate) async fn spawn_raw(bin: &str, args: &[&str], cwd: &str) -> Result<Self> {
        let mut command = Command::new(bin);
        command
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.as_std_mut().process_group(0);
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("spawning python kernel (`{bin}`)"))?;
        let stdin = child.stdin.take().context("python kernel stdin unavailable")?;
        let stdout = child.stdout.take().context("python kernel stdout unavailable")?;
        let stderr = child.stderr.take().context("python kernel stderr unavailable")?;
        let kernel = Self {
            child,
            stdin: AsyncMutex::new(stdin),
            stdout: AsyncMutex::new(BufReader::new(stdout)),
            stderr_tail: Arc::new(Mutex::new(String::new())),
            dead: Arc::new(AtomicBool::new(false)),
        };
        kernel.start_stderr_tail(stderr);
        Ok(kernel)
    }

    /// Bounded capture of the kernel's own stderr (driver-level failures;
    /// user stderr is redirected into the response frames by the driver).
    fn start_stderr_tail(&self, stderr: ChildStderr) {
        let tail = self.stderr_tail.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut buf = vec![0u8; 8192];
            loop {
                match reader.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut guard = tail.lock();
                        guard.push_str(&String::from_utf8_lossy(&buf[..n]));
                        if guard.len() > MAX_STREAM_BYTES {
                            let overflow = guard.len() - MAX_STREAM_BYTES;
                            guard.drain(..overflow);
                        }
                    }
                }
            }
        });
    }

    /// Why the kernel is dead: the exit notice plus any redacted stderr the
    /// driver left behind.
    fn death_detail(&self) -> String {
        let mut detail = "python kernel process exited".to_owned();
        let stderr = redact_secrets(&self.stderr_tail.lock());
        if !stderr.trim().is_empty() {
            detail.push_str("\n--- kernel stderr ---\n");
            detail.push_str(&stderr);
        }
        detail
    }

    /// Evaluates one cell: writes the framed code, reads the framed response,
    /// and enforces the per-cell timeout and abort. On timeout/abort the
    /// process group is killed (the caller respawns on the next call).
    pub(crate) async fn eval(
        &mut self,
        code: &str,
        timeout: Duration,
        abort: &AbortSignal,
    ) -> Result<CellResult> {
        if self.dead.load(Ordering::SeqCst) {
            bail!("python kernel is not running: {}", self.death_detail());
        }
        {
            let mut stdin = self.stdin.lock().await;
            stdin
                .write_all(format!("{}\n", code.len()).as_bytes())
                .await
                .context("writing python kernel request")?;
            stdin
                .write_all(code.as_bytes())
                .await
                .context("writing python kernel request")?;
            stdin.flush().await.context("writing python kernel request")?;
        }
        let wait = {
            let mut stdout = self.stdout.lock().await;
            tokio::select! {
                frame = read_frame(&mut stdout) => FrameWait::Frame(frame),
                _ = abort.cancelled() => FrameWait::Aborted,
                _ = tokio::time::sleep(timeout) => FrameWait::TimedOut,
            }
        };
        match wait {
            FrameWait::Frame(Ok(payload)) => parse_cell_result(&payload),
            FrameWait::Frame(Err(error)) => {
                self.dead.store(true, Ordering::SeqCst);
                self.kill_and_reap().await;
                bail!("python kernel protocol error: {error}");
            }
            FrameWait::Aborted => {
                self.dead.store(true, Ordering::SeqCst);
                self.kill_and_reap().await;
                bail!("Operation aborted");
            }
            FrameWait::TimedOut => {
                self.dead.store(true, Ordering::SeqCst);
                self.kill_and_reap().await;
                Ok(CellResult {
                    stdout: String::new(),
                    stderr: String::new(),
                    result: None,
                    error: Some(CellError {
                        kind: EvalErrorKind::Timeout,
                        text: format!(
                            "python cell timed out after {}s; the kernel was killed and will \
                             respawn on the next call",
                            timeout.as_secs()
                        ),
                    }),
                })
            }
        }
    }

    /// SIGKILLs the process group and reaps the child.
    pub(crate) async fn kill_and_reap(&mut self) {
        kill_process_group(&self.child);
        let _ = tokio::time::timeout(EXIT_TIMEOUT, self.child.wait()).await;
    }
}

impl Drop for PythonKernel {
    fn drop(&mut self) {
        // Never leak the kernel, even on panic paths.
        if self.child.try_wait().ok().flatten().is_none() {
            kill_process_group(&self.child);
            let _ = self.child.start_kill();
        }
    }
}

/// Reads one length-prefixed frame: a header line with the payload length,
/// then exactly that many bytes. Bounded so a broken/rogue kernel cannot
/// balloon memory.
async fn read_frame(reader: &mut BufReader<ChildStdout>) -> Result<Vec<u8>> {
    let mut header = Vec::new();
    loop {
        let byte = reader
            .read_u8()
            .await
            .context("reading python kernel response")?;
        if byte == b'\n' {
            break;
        }
        header.push(byte);
        if header.len() > MAX_HEADER_BYTES {
            bail!(
                "python kernel response header exceeds {MAX_HEADER_BYTES} bytes \
                 (protocol desync)"
            );
        }
    }
    let header = std::str::from_utf8(&header).context("invalid python kernel response header")?;
    let len: usize = header
        .trim()
        .parse()
        .context("invalid python kernel response length")?;
    if len > MAX_FRAME_BYTES {
        bail!(
            "python kernel response of {len} bytes exceeds the {MAX_FRAME_BYTES}-byte frame bound"
        );
    }
    let mut payload = vec![0u8; len];
    reader
        .read_exact(&mut payload)
        .await
        .context("reading python kernel response payload")?;
    Ok(payload)
}

/// Decodes a driver response frame into a [`CellResult`], classifying the
/// error from the driver-reported exception type.
fn parse_cell_result(payload: &[u8]) -> Result<CellResult> {
    let frame: Value =
        serde_json::from_slice(payload).context("invalid python kernel response frame")?;
    let stdout = frame
        .get("stdout")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let stderr = frame
        .get("stderr")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let result = frame.get("result").and_then(Value::as_str).map(String::from);
    let error = frame.get("error").and_then(Value::as_str);
    let error_type = frame
        .get("error_type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let error = error.map(|text| CellError {
        kind: classify_python_error(error_type),
        text: text.to_owned(),
    });
    Ok(CellResult {
        stdout,
        stderr,
        result,
        error,
    })
}

fn classify_python_error(error_type: &str) -> EvalErrorKind {
    match error_type {
        "SyntaxError" | "IndentationError" | "TabError" => EvalErrorKind::Syntax,
        _ => EvalErrorKind::Runtime,
    }
}

// ---------------------------------------------------------------------------
// JS kernel (embedded QuickJS, thread-confined)
// ---------------------------------------------------------------------------

/// Per-cell bootstrap: a `console` shim writing into a per-cell buffer and
/// the result serializer. All state lives on `globalThis`; the Rust side
/// resets the buffer before each cell and reads it back afterwards.
const JS_BOOTSTRAP_SOURCE: &str = r#"
globalThis.__piConsoleBuffer = [];
globalThis.__piResetConsole = function () {
  globalThis.__piConsoleBuffer = [];
};
globalThis.__piConsoleLines = function () {
  return globalThis.__piConsoleBuffer.join("\n");
};
function __piFormatArg(v) {
  if (v === undefined) return "undefined";
  if (v === null) return "null";
  if (typeof v === "string") return v;
  if (typeof v === "symbol") return v.toString();
  if (typeof v === "bigint") return v.toString();
  if (typeof v === "function") return "[Function " + (v.name || "anonymous") + "]";
  try { return JSON.stringify(v); } catch (e) {
    try { return String(v); } catch (e2) { return "[unprintable]"; }
  }
}
globalThis.console = {
  log: function () {
    __piConsoleBuffer.push(Array.prototype.map.call(arguments, __piFormatArg).join(" "));
  },
  info: function () {
    __piConsoleBuffer.push(Array.prototype.map.call(arguments, __piFormatArg).join(" "));
  },
  warn: function () {
    __piConsoleBuffer.push(Array.prototype.map.call(arguments, __piFormatArg).join(" "));
  },
  error: function () {
    __piConsoleBuffer.push(Array.prototype.map.call(arguments, __piFormatArg).join(" "));
  },
  debug: function () {
    __piConsoleBuffer.push(Array.prototype.map.call(arguments, __piFormatArg).join(" "));
  }
};
globalThis.__piSerializeResult = function (v) {
  if (v === undefined) return null;
  if (typeof v === "function") return "[Function " + (v.name || "anonymous") + "]";
  if (typeof v === "symbol") return v.toString();
  if (typeof v === "bigint") return v.toString();
  try { return JSON.stringify(v); } catch (e) {
    try { return String(v); } catch (e2) { return "[unprintable result]"; }
  }
};
"#;

/// A request to the JS kernel thread.
struct JsRequest {
    code: String,
    timeout: Duration,
    tx: tokio::sync::oneshot::Sender<CellResult>,
}

/// A live JS kernel: a dedicated OS thread owning the QuickJS runtime. Dropping
/// the kernel closes the request channel, which makes the thread exit after
/// the in-flight cell (the interrupt handler bounds how long that takes). A
/// per-cell timeout ALSO closes the channel (the kernel is killed for parity
/// with the Python kernel), so the next call must spawn a fresh kernel — the
/// timeout notice says exactly that.
struct JsKernel {
    tx: Option<std_mpsc::Sender<JsRequest>>,
    /// Detached on drop; the thread exits via the closed channel.
    _handle: thread::JoinHandle<()>,
}

impl JsKernel {
    fn spawn() -> Result<Self> {
        let (tx, rx) = std_mpsc::channel::<JsRequest>();
        let handle = thread::Builder::new()
            .name("pi-eval-js-kernel".to_owned())
            .spawn(move || js_kernel_main(rx))
            .context("spawning JS kernel thread")?;
        Ok(Self {
            tx: Some(tx),
            _handle: handle,
        })
    }

    async fn eval(
        &mut self,
        code: &str,
        timeout: Duration,
        abort: &AbortSignal,
    ) -> Result<CellResult> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let request_tx = self
            .tx
            .as_ref()
            .ok_or_else(|| anyhow!("JS kernel is not running (it was killed by a timeout); the next call respawns it"))?;
        request_tx
            .send(JsRequest {
                code: code.to_owned(),
                timeout,
                tx,
            })
            .map_err(|_| anyhow!("JS kernel is not running (it crashed or was killed)"))?;
        tokio::select! {
            result = rx => match result {
                Ok(cell) => {
                    // The interrupt handler aborts the cell at the deadline,
                    // so a Timeout-classified result can arrive here before
                    // the select-side sleep fires. Either way the kernel is
                    // killed: close the request channel so the thread exits
                    // (the next call respawns a fresh kernel).
                    if cell.is_timeout() {
                        self.tx = None;
                    }
                    Ok(cell)
                }
                Err(_) => bail!("JS kernel thread exited before answering"),
            },
            _ = abort.cancelled() => bail!("Operation aborted"),
            _ = tokio::time::sleep(timeout) => {
                // Kill the kernel for parity with the Python kernel: close
                // the request channel so the JS thread exits after the
                // in-flight cell aborts (the interrupt handler bounds how
                // long that takes). The next call must spawn a fresh kernel
                // with no cross-cell state — matching the notice below.
                self.tx = None;
                Ok(CellResult {
                    stdout: String::new(),
                    stderr: String::new(),
                    result: None,
                    error: Some(CellError {
                        kind: EvalErrorKind::Timeout,
                        text: format!(
                            "js cell timed out after {}s; the kernel was killed and will respawn \
                             on the next call",
                            timeout.as_secs()
                        ),
                    }),
                })
            }
        }
    }
}

fn js_kernel_main(rx: std_mpsc::Receiver<JsRequest>) {
    let Ok(runtime) = Runtime::new() else {
        return;
    };
    runtime.set_memory_limit(JS_MEMORY_LIMIT_BYTES);
    runtime.set_max_stack_size(JS_STACK_LIMIT_BYTES);
    // Deadline for the interrupt handler: 0 means "never interrupt".
    let deadline = Arc::new(AtomicU64::new(0));
    let handler_deadline = deadline.clone();
    runtime.set_interrupt_handler(Some(Box::new(move || {
        let limit = handler_deadline.load(Ordering::Relaxed);
        limit != 0 && now_millis() >= limit
    })));
    let Ok(context) = Context::full(&runtime) else {
        return;
    };
    if context
        .with(|ctx| ctx.eval::<(), _>(JS_BOOTSTRAP_SOURCE))
        .is_err()
    {
        return;
    }
    while let Ok(request) = rx.recv() {
        deadline.store(
            now_millis().saturating_add(request.timeout.as_millis() as u64),
            Ordering::Relaxed,
        );
        let result = context.with(|ctx| run_js_cell(&ctx, &request.code, &deadline, request.timeout));
        deadline.store(0, Ordering::Relaxed);
        let _ = request.tx.send(result);
    }
}

/// Evaluates one cell in the (thread-confined) QuickJS context: resets the
/// console buffer, evaluates as a global script, serializes the completion
/// value, and classifies any exception (SyntaxError vs runtime vs timeout).
fn run_js_cell(ctx: &Ctx<'_>, code: &str, deadline: &AtomicU64, timeout: Duration) -> CellResult {
    let globals = ctx.globals();
    let _ = globals
        .get::<_, Function>("__piResetConsole")
        .and_then(|reset| reset.call::<(), ()>(()));
    let outcome = ctx.eval::<JsValue, _>(code);
    let stdout = || {
        globals
            .get::<_, Function>("__piConsoleLines")
            .and_then(|lines| lines.call::<_, String>(()))
            .unwrap_or_default()
    };
    match outcome {
        Ok(value) => {
            let result = globals
                .get::<_, Function>("__piSerializeResult")
                .and_then(|serialize| serialize.call::<_, Option<String>>((value,)))
                .ok()
                .flatten();
            CellResult {
                stdout: bounded_text(&stdout()),
                stderr: String::new(),
                result,
                error: None,
            }
        }
        Err(error) => {
            let (kind, text) = match &error {
                rquickjs::Error::Exception => {
                    let value: JsValue = ctx.catch();
                    let (name, message, stack) = exception_parts(ctx, &value);
                    let kind = if deadline_expired(deadline) {
                        EvalErrorKind::Timeout
                    } else if name.as_deref() == Some("SyntaxError") {
                        EvalErrorKind::Syntax
                    } else {
                        EvalErrorKind::Runtime
                    };
                    if kind == EvalErrorKind::Timeout {
                        // The interrupt handler aborted the cell at the
                        // deadline. The notice must match the real semantics:
                        // the kernel is killed (the request channel closes
                        // and the thread exits) and the next call respawns it
                        // — parity with the Python kernel and with the
                        // select-side timeout notice.
                        (
                            EvalErrorKind::Timeout,
                            format!(
                                "js cell timed out after {}s; the kernel was killed and will \
                                 respawn on the next call",
                                timeout.as_secs()
                            ),
                        )
                    } else {
                        (kind, format_exception(&name, &message, &stack))
                    }
                }
                rquickjs::Error::Allocation => (
                    EvalErrorKind::Runtime,
                    "JavaScript kernel out of memory (64 MiB limit)".to_owned(),
                ),
                _ => (EvalErrorKind::Runtime, error.to_string()),
            };
            CellResult {
                stdout: bounded_text(&stdout()),
                stderr: String::new(),
                result: None,
                error: Some(CellError {
                    kind,
                    text: bounded_text(&text),
                }),
            }
        }
    }
}

/// True when the interrupt deadline has been reached (the interrupt handler
/// fired mid-cell).
fn deadline_expired(deadline: &AtomicU64) -> bool {
    let limit = deadline.load(Ordering::Relaxed);
    limit != 0 && now_millis() >= limit
}

/// Pulls `name`/`message`/`stack` off a caught exception value.
fn exception_parts<'js>(
    ctx: &Ctx<'js>,
    value: &JsValue<'js>,
) -> (Option<String>, Option<String>, Option<String>) {
    if let Some(object) = value.as_object() {
        (
            object.get::<_, Option<String>>("name").ok().flatten(),
            object.get::<_, Option<String>>("message").ok().flatten(),
            object.get::<_, Option<String>>("stack").ok().flatten(),
        )
    } else {
        // Non-Error thrown values stringify like `String(value)`.
        let text = JsString::from_js(ctx, value.clone())
            .ok()
            .and_then(|string| string.to_string().ok());
        (None, text, None)
    }
}

/// Formats an exception as `name: message` plus the stack tail (the stack's
/// first line repeats `name: message`).
fn format_exception(
    name: &Option<String>,
    message: &Option<String>,
    stack: &Option<String>,
) -> String {
    let mut text = match (name, message) {
        (Some(name), Some(message)) if !message.is_empty() => format!("{name}: {message}"),
        (Some(name), _) => name.clone(),
        (_, Some(message)) => message.clone(),
        _ => "JavaScript exception".to_owned(),
    };
    if let Some(stack) = stack {
        let lines: Vec<&str> = stack.lines().collect();
        if lines.len() > 1 {
            text.push('\n');
            text.push_str(&lines[1..].join("\n"));
        }
    }
    text
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

/// Bounds a rendered text to [`MAX_STREAM_BYTES`] (tail, like bash output).
fn bounded_text(text: &str) -> String {
    truncate_tail(text, 200, MAX_STREAM_BYTES).content
}

// ---------------------------------------------------------------------------
// Registry (session-scoped, one Python + one JS kernel)
// ---------------------------------------------------------------------------

/// Session-scoped kernel registry: at most one live Python kernel and one live
/// JS kernel per tool instance, lazily spawned on first use and killed by a
/// timeout/error (the next call respawns). Cloning shares the same slot; the
/// tool captures one registry per tool instance (mirrors `debug`/`mcp`).
#[derive(Clone, Default)]
pub(crate) struct EvalRegistry {
    inner: Arc<AsyncMutex<EvalKernels>>,
}

#[derive(Default)]
struct EvalKernels {
    python: Option<PythonKernel>,
    js: Option<JsKernel>,
}

impl EvalRegistry {
    /// An empty registry (no kernels running).
    pub(crate) fn new() -> Self {
        Self::default()
    }
}

// ---------------------------------------------------------------------------
// Tool surface
// ---------------------------------------------------------------------------

/// Builds the `eval` tool: session-scoped persistent Python + JS kernels with
/// cross-cell state (mirrors the `debug`/`mcp` session registries).
pub(crate) fn eval_tool(cwd: &str) -> AgentTool {
    eval_tool_with_python(cwd, None)
}

/// [`eval_tool`] with an explicit python interpreter override (tests).
fn eval_tool_with_python(cwd: &str, python_override: Option<&str>) -> AgentTool {
    let python_override = python_override.map(String::from);
    let description = format!(
        "Evaluate code in a persistent, session-scoped language kernel. `language` is \
         `python` (a real python3 subprocess with the full standard library) or `js` (the \
         embedded QuickJS engine, no Node APIs and no module loading). Globals persist \
         between calls in the same session (cross-cell state): \
         `eval language=python code=\"x = 1\"` then `eval language=python code=\"x + 1\"` \
         returns 2. Output is bounded (64 KiB per stream) and redacted; failures are \
         classified as syntax, runtime, or timeout. A cell that times out (default 30s, \
         max 300s) kills the kernel; the next call starts a fresh one. Python runs \
         isolated from PYTHONPATH but has normal filesystem access — treat it like bash."
    );
    let params = s_object(
        vec![
            (
                "language",
                s_string("Language of the code: `python` or `js`"),
            ),
            (
                "code",
                s_string("Source code to evaluate in the kernel"),
            ),
            (
                "timeout",
                s_number("Per-cell timeout in seconds (default 30, min 1, max 300)"),
            ),
        ],
        vec!["language", "code"],
    );
    let registry = EvalRegistry::new();
    let cwd = cwd.to_owned();
    AgentTool::new("eval", description, params, move |ctx: ToolCallContext| {
        let registry = registry.clone();
        let cwd = cwd.clone();
        let python_override = python_override.clone();
        async move {
            run_eval_with_python(&registry, &cwd, ctx.arguments, ctx.abort, python_override.as_deref())
                .await
        }
    })
    .with_capability(ToolCapability::Exec)
    .with_prompt_guidelines(vec![
        "State persists across eval calls in the same session per language: assignments in one cell are visible in later cells (a kernel killed by timeout/error loses its state and respawns fresh).".to_string(),
        "Prefer many small eval cells over one giant script; each call is bounded at 64 KiB per output stream and redacted.".to_string(),
        "Python is a real python3 subprocess (full standard library, filesystem access); JS is an embedded QuickJS engine with no require/import, no Node APIs, and no network or filesystem access.".to_string(),
    ])
}

/// Entry point: validates `language`/`code` and dispatches against the session
/// kernel slots. A kernel that times out or dies is dropped (killed) and
/// respawns lazily on the next call.
pub(crate) async fn run_eval(
    registry: &EvalRegistry,
    cwd: &str,
    args: Value,
    abort: AbortSignal,
) -> Result<AgentToolResult> {
    run_eval_with_python(registry, cwd, args, abort, None).await
}

async fn run_eval_with_python(
    registry: &EvalRegistry,
    cwd: &str,
    args: Value,
    abort: AbortSignal,
    python_override: Option<&str>,
) -> Result<AgentToolResult> {
    check_aborted(&abort)?;
    let language = arg_str(&args, "language").trim().to_ascii_lowercase();
    if language.is_empty() {
        bail!("eval requires a `language` (`python` or `js`)");
    }
    let code = arg_str(&args, "code");
    if code.trim().is_empty() {
        bail!("eval requires non-empty `code`");
    }
    let timeout = cell_timeout(&args)?;
    let mut kernels = registry.inner.lock().await;
    match language.as_str() {
        "python" => {
            let mut kernel = match kernels.python.take() {
                Some(kernel) => kernel,
                None => PythonKernel::spawn_with(python_override, cwd).await?,
            };
            match kernel.eval(&code, timeout, &abort).await {
                Ok(cell) => {
                    let respawn = cell.is_timeout();
                    if !respawn {
                        kernels.python = Some(kernel);
                    }
                    Ok(render_cell("python", &cell))
                }
                Err(error) => {
                    // The kernel is dropped (killed); respawn on the next call.
                    Err(error)
                }
            }
        }
        "js" => {
            let mut kernel = match kernels.js.take() {
                Some(kernel) => kernel,
                None => JsKernel::spawn()?,
            };
            match kernel.eval(&code, timeout, &abort).await {
                Ok(cell) => {
                    let respawn = cell.is_timeout();
                    if !respawn {
                        kernels.js = Some(kernel);
                    }
                    Ok(render_cell("js", &cell))
                }
                Err(error) => {
                    // The kernel is dropped (thread exits); respawn on the next call.
                    Err(error)
                }
            }
        }
        other => bail!("eval `language` must be `python` or `js` (got `{other}`)"),
    }
}

/// Resolves the `timeout` argument (seconds), defaulting to 30 and clamping
/// to 1..=300. Shared with the notebook tool's `execute` action.
pub(crate) fn cell_timeout(args: &Value) -> Result<Duration> {
    let Some(seconds) = arg_int(args, "timeout")? else {
        return Ok(DEFAULT_CELL_TIMEOUT);
    };
    if seconds < 1 || seconds > MAX_CELL_TIMEOUT_SECONDS {
        bail!("eval `timeout` must be between 1 and {MAX_CELL_TIMEOUT_SECONDS} seconds");
    }
    Ok(Duration::from_secs(seconds as u64))
}

/// Renders a cell outcome for the `eval` tool: a status line, then the
/// non-empty stdout/stderr/result/error sections, redacted.
fn render_cell(language: &str, cell: &CellResult) -> AgentToolResult {
    let mut text = match &cell.error {
        None => format!("{language}: ok\n"),
        Some(error) => match error.kind {
            EvalErrorKind::Syntax => format!("{language}: syntax error\n"),
            EvalErrorKind::Runtime => format!("{language}: runtime error\n"),
            EvalErrorKind::Timeout => format!("{language}: timeout\n"),
        },
    };
    if !cell.stdout.is_empty() {
        text.push_str("stdout:\n");
        text.push_str(&cell.stdout);
        text.push('\n');
    }
    if !cell.stderr.is_empty() {
        text.push_str("stderr:\n");
        text.push_str(&cell.stderr);
        text.push('\n');
    }
    if let Some(result) = &cell.result {
        text.push_str("result: ");
        text.push_str(result);
        text.push('\n');
    }
    if let Some(error) = &cell.error {
        text.push_str("error:\n");
        text.push_str(error.text.trim_end());
        text.push('\n');
    }
    text_result(redact_secrets(&text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Arc;

    use serde_json::json;
    use tempfile::tempdir;

    use pi_agent::{AbortController, AgentToolResult, ToolCallContext, ToolUpdateFn};

    fn tmpdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("pi-eval-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn noop_update() -> ToolUpdateFn {
        Arc::new(|_r: AgentToolResult| {})
    }

    fn make_ctx(args: Value) -> ToolCallContext {
        let (_ctrl, abort) = AbortController::new();
        std::mem::forget(_ctrl);
        ToolCallContext {
            tool_call_id: "eval-test".to_string(),
            arguments: args,
            on_update: noop_update(),
            abort,
            model: None,
        }
    }

    fn text_of(res: &AgentToolResult) -> String {
        match res.content.first() {
            Some(pi_ai::ContentBlock::Text { text, .. }) => text.clone(),
            _ => String::new(),
        }
    }

    /// True when a real `python3` interpreter is available (integration tests
    /// skip cleanly on machines without one, like the debug DAP e2e probe).
    fn python3_available() -> bool {
        std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_ok_and(|out| out.status.success())
    }

    #[cfg(unix)]
    fn sh_available() -> bool {
        std::process::Command::new("sh")
            .arg("-c")
            .arg("true")
            .output()
            .is_ok_and(|out| out.status.success())
    }

    // -----------------------------------------------------------------------
    // Protocol layer (fake kernels, no python3 needed)
    // -----------------------------------------------------------------------

    /// A fake kernel implemented in `sh`: reads the length-prefixed code cell,
    /// discards it, and answers with a canned frame. This validates the Rust
    /// framing (length header + payload round trip) against an independent
    /// implementation.
    #[cfg(unix)]
    #[tokio::test]
    async fn python_kernel_protocol_round_trip_with_fake_sh_kernel() {
        if !sh_available() {
            return;
        }
        let script = r#"
while IFS= read -r n; do
  case "$n" in ''|*[!0-9]*) break;; esac
  head -c "$n" >/dev/null || break
  resp='{"stdout":"fake-out","stderr":"fake-err","error":null,"error_type":null,"result":"canned-result"}'
  printf '%s\n%s' "${#resp}" "$resp"
done
"#;
        let dir = tmpdir();
        let script_path = dir.join("fake_kernel.sh");
        fs::write(&script_path, script).unwrap();
        let mut kernel = PythonKernel::spawn_raw("sh", &[script_path.to_str().unwrap()], &dir.to_string_lossy())
            .await
            .expect("fake kernel spawns");
        let cell = kernel
            .eval("print('ignored')", Duration::from_secs(5), &AbortSignal::none())
            .await
            .expect("fake kernel answers");
        assert_eq!(cell.stdout, "fake-out");
        assert_eq!(cell.stderr, "fake-err");
        assert_eq!(cell.result.as_deref(), Some("canned-result"));
        assert!(cell.error.is_none(), "{:?}", cell.error);
    }

    /// A fake kernel that answers with an error frame: the Rust side must
    /// parse it and classify it as a runtime error.
    #[cfg(unix)]
    #[tokio::test]
    async fn python_kernel_parses_error_frames_from_fake() {
        if !sh_available() {
            return;
        }
        let script = r#"
while IFS= read -r n; do
  case "$n" in ''|*[!0-9]*) break;; esac
  head -c "$n" >/dev/null || break
  resp='{"stdout":"","stderr":"","error":"Traceback (most recent call last):\nZeroDivisionError: division by zero","error_type":"ZeroDivisionError","result":null}'
  printf '%s\n%s' "${#resp}" "$resp"
done
"#;
        let dir = tmpdir();
        let script_path = dir.join("fake_error_kernel.sh");
        fs::write(&script_path, script).unwrap();
        let mut kernel = PythonKernel::spawn_raw("sh", &[script_path.to_str().unwrap()], &dir.to_string_lossy())
            .await
            .expect("fake kernel spawns");
        let cell = kernel
            .eval("1/0", Duration::from_secs(5), &AbortSignal::none())
            .await
            .expect("fake kernel answers");
        let error = cell.error.expect("error classified");
        assert_eq!(error.kind, EvalErrorKind::Runtime);
        assert!(error.text.contains("ZeroDivisionError"));
    }

    /// The hard frame bound: a fake kernel answering with an oversized frame
    /// is treated as a protocol error (and killed).
    #[cfg(unix)]
    #[tokio::test]
    async fn python_kernel_rejects_oversized_frames() {
        if !sh_available() {
            return;
        }
        let script = r#"
while IFS= read -r n; do
  case "$n" in ''|*[!0-9]*) break;; esac
  head -c "$n" >/dev/null || break
  printf '%s\n' "999999"
  head -c 999999 /dev/zero
done
"#;
        let dir = tmpdir();
        let script_path = dir.join("fake_huge_kernel.sh");
        fs::write(&script_path, script).unwrap();
        let mut kernel = PythonKernel::spawn_raw("sh", &[script_path.to_str().unwrap()], &dir.to_string_lossy())
            .await
            .expect("fake kernel spawns");
        let error = kernel
            .eval("x", Duration::from_secs(5), &AbortSignal::none())
            .await
            .expect_err("oversized frame is a protocol error");
        assert!(error.to_string().contains("frame bound"), "{error}");
    }

    // -----------------------------------------------------------------------
    // Real python3 kernel (integration)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn python_kernel_cross_cell_state() {
        if !python3_available() {
            return;
        }
        let cwd = tmpdir().to_string_lossy().into_owned();
        let mut kernel = PythonKernel::spawn(&cwd).await.expect("kernel spawns");
        let first = kernel
            .eval("a = 1", Duration::from_secs(10), &AbortSignal::none())
            .await
            .expect("first cell");
        assert!(first.error.is_none(), "{:?}", first.error);
        let second = kernel
            .eval("a + 1", Duration::from_secs(10), &AbortSignal::none())
            .await
            .expect("second cell");
        assert_eq!(second.result.as_deref(), Some("2"));
    }

    #[tokio::test]
    async fn python_kernel_stdout_stderr_and_expression_result() {
        if !python3_available() {
            return;
        }
        let cwd = tmpdir().to_string_lossy().into_owned();
        let mut kernel = PythonKernel::spawn(&cwd).await.expect("kernel spawns");
        let cell = kernel
            .eval(
                "import sys\nprint('to stdout')\nprint('to stderr', file=sys.stderr)\n40 + 2",
                Duration::from_secs(10),
                &AbortSignal::none(),
            )
            .await
            .expect("cell");
        assert_eq!(cell.stdout.trim_end(), "to stdout");
        assert_eq!(cell.stderr.trim_end(), "to stderr");
        assert_eq!(cell.result.as_deref(), Some("42"));
    }

    #[tokio::test]
    async fn python_kernel_classifies_errors() {
        if !python3_available() {
            return;
        }
        let cwd = tmpdir().to_string_lossy().into_owned();
        let mut kernel = PythonKernel::spawn(&cwd).await.expect("kernel spawns");
        let syntax = kernel
            .eval("def broken(:", Duration::from_secs(10), &AbortSignal::none())
            .await
            .expect("syntax cell");
        assert_eq!(
            syntax.error.as_ref().map(|e| e.kind),
            Some(EvalErrorKind::Syntax),
            "{:?}",
            syntax.error
        );
        let runtime = kernel
            .eval("1 / 0", Duration::from_secs(10), &AbortSignal::none())
            .await
            .expect("runtime cell");
        assert_eq!(
            runtime.error.as_ref().map(|e| e.kind),
            Some(EvalErrorKind::Runtime),
            "{:?}",
            runtime.error
        );
        assert!(runtime.error.unwrap().text.contains("ZeroDivisionError"));
    }

    #[tokio::test]
    async fn python_kernel_timeout_kills_and_next_call_respawns() {
        if !python3_available() {
            return;
        }
        let cwd = tmpdir().to_string_lossy().into_owned();
        let mut kernel = PythonKernel::spawn(&cwd).await.expect("kernel spawns");
        let timed_out = kernel
            .eval(
                "import time\ntime.sleep(30)",
                Duration::from_secs(1),
                &AbortSignal::none(),
            )
            .await
            .expect("timeout is a classified result, not an Err");
        assert_eq!(
            timed_out.error.as_ref().map(|e| e.kind),
            Some(EvalErrorKind::Timeout),
            "{:?}",
            timed_out.error
        );
        // The kernel process was killed; a fresh kernel is spawned per the
        // respawn contract.
        let mut fresh = PythonKernel::spawn(&cwd).await.expect("respawn");
        let cell = fresh
            .eval("1 + 1", Duration::from_secs(10), &AbortSignal::none())
            .await
            .expect("cell after respawn");
        assert_eq!(cell.result.as_deref(), Some("2"));
    }

    #[tokio::test]
    async fn python_kernel_bounds_output() {
        if !python3_available() {
            return;
        }
        let cwd = tmpdir().to_string_lossy().into_owned();
        let mut kernel = PythonKernel::spawn(&cwd).await.expect("kernel spawns");
        let cell = kernel
            .eval(
                "print('x' * 200000)",
                Duration::from_secs(10),
                &AbortSignal::none(),
            )
            .await
            .expect("cell");
        assert!(cell.stdout.len() <= MAX_STREAM_BYTES, "{}", cell.stdout.len());
        assert!(cell.stdout.contains("truncated at 64 KiB"), "{:?}", cell.stdout);
        // The kernel survives a bounded cell.
        let next = kernel
            .eval("1 + 1", Duration::from_secs(10), &AbortSignal::none())
            .await
            .expect("next cell");
        assert_eq!(next.result.as_deref(), Some("2"));
    }

    #[tokio::test]
    async fn python_kernel_aggregate_output_budget_bounds_simultaneous_streams() {
        // P1a: two streams that are each large AT THE SAME TIME used to
        // produce a frame over the 128 KiB hard bound (each stream was
        // capped individually at 64 KiB but the JSON carried all of them),
        // which killed the kernel. The driver now enforces ONE aggregate
        // budget across streams before serialization: the cell returns a
        // classified/truncated result and the kernel survives.
        if !python3_available() {
            return;
        }
        let cwd = tmpdir().to_string_lossy().into_owned();
        let mut kernel = PythonKernel::spawn(&cwd).await.expect("kernel spawns");
        let cell = kernel
            .eval(
                "print('o' * 100000)\nimport sys\nprint('e' * 100000, file=sys.stderr)",
                Duration::from_secs(10),
                &AbortSignal::none(),
            )
            .await
            .expect("cell");
        // Both streams are present and individually bounded.
        assert!(cell.stdout.contains("truncated"), "{:?}", cell.stdout);
        assert!(cell.stderr.contains("truncated"), "{:?}", cell.stderr);
        assert!(cell.stdout.len() <= MAX_STREAM_BYTES, "{}", cell.stdout.len());
        assert!(cell.stderr.len() <= MAX_STREAM_BYTES, "{}", cell.stderr.len());
        // The aggregate stays inside the frame budget.
        assert!(
            cell.stdout.len() + cell.stderr.len() <= MAX_AGGREGATE_STREAM_BYTES,
            "aggregate {} exceeds budget",
            cell.stdout.len() + cell.stderr.len()
        );
        // The kernel survives: the next cell works.
        let next = kernel
            .eval("1 + 1", Duration::from_secs(10), &AbortSignal::none())
            .await
            .expect("next cell");
        assert_eq!(next.result.as_deref(), Some("2"));
    }

    #[tokio::test]
    async fn python_kernel_raw_fd_writes_do_not_desync_protocol() {
        // P1b: writes straight to fd 1/2 (os.write, sys.__stdout__.buffer)
        // bypass the driver's Python-level redirection entirely. They must
        // be captured per cell and never desync the framed protocol.
        if !python3_available() {
            return;
        }
        let cwd = tmpdir().to_string_lossy().into_owned();
        let mut kernel = PythonKernel::spawn(&cwd).await.expect("kernel spawns");
        let cell = kernel
            .eval(
                "import os\nos.write(1, b'raw-fd-out')\nos.write(2, b'raw-fd-err')\n40 + 2",
                Duration::from_secs(10),
                &AbortSignal::none(),
            )
            .await
            .expect("os.write cell");
        assert!(cell.stdout.contains("raw-fd-out"), "{:?}", cell.stdout);
        assert!(cell.stderr.contains("raw-fd-err"), "{:?}", cell.stderr);
        // The expression result still round-trips through the frame.
        assert_eq!(cell.result.as_deref(), Some("42"));

        // sys.__stdout__.buffer.write bypasses redirect_stdout too.
        let cell = kernel
            .eval(
                "import sys\nsys.__stdout__.buffer.write(b'buffer-out')\nsys.__stdout__.buffer.flush()\n",
                Duration::from_secs(10),
                &AbortSignal::none(),
            )
            .await
            .expect("buffer cell");
        assert!(cell.stdout.contains("buffer-out"), "{:?}", cell.stdout);

        // The protocol stayed in sync: the kernel answers a follow-up cell.
        let next = kernel
            .eval("6 * 7", Duration::from_secs(10), &AbortSignal::none())
            .await
            .expect("next cell");
        assert_eq!(next.result.as_deref(), Some("42"));
    }

    #[tokio::test]
    async fn python_kernel_subprocess_stdout_is_captured_and_bounded() {
        // P1b: a subprocess inherits fds 1/2, so its output lands in the
        // capture pipes (it never touches the driver's StringIO capture).
        // It must be attributed to the cell and leave the protocol intact.
        if !python3_available() {
            return;
        }
        let cwd = tmpdir().to_string_lossy().into_owned();
        let mut kernel = PythonKernel::spawn(&cwd).await.expect("kernel spawns");
        let cell = kernel
            .eval(
                "import subprocess\nsubprocess.run(['python3', '-c', 'print(\"sub-out\")'], check=True)",
                Duration::from_secs(10),
                &AbortSignal::none(),
            )
            .await
            .expect("subprocess cell");
        assert!(cell.stdout.contains("sub-out"), "{:?}", cell.stdout);
        // A chatty subprocess is still bounded by the capture cap.
        let cell = kernel
            .eval(
                "import subprocess\nsubprocess.run(['python3', '-c', 'print(\"y\" * 300000)'], check=True)",
                Duration::from_secs(10),
                &AbortSignal::none(),
            )
            .await
            .expect("chatty subprocess cell");
        assert!(cell.stdout.len() <= MAX_STREAM_BYTES, "{}", cell.stdout.len());
        // The kernel survives.
        let next = kernel
            .eval("1 + 1", Duration::from_secs(10), &AbortSignal::none())
            .await
            .expect("next cell");
        assert_eq!(next.result.as_deref(), Some("2"));
    }

    #[test]
    fn python_command_resolution_is_actionable_when_missing() {
        let error = resolve_python_from("/nonexistent/bin", &["python3", "python"])
            .expect_err("missing interpreter must fail");
        let text = error.to_string();
        assert!(text.contains("python3"), "{text}");
        assert!(text.contains("PATH"), "{text}");
        assert!(text.contains("install Python 3"), "{text}");
    }

    // -----------------------------------------------------------------------
    // JS kernel (embedded QuickJS)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn js_kernel_cross_cell_state_and_console() {
        let mut kernel = JsKernel::spawn().expect("js kernel spawns");
        let first = kernel
            .eval("var a = 1", Duration::from_secs(5), &AbortSignal::none())
            .await
            .expect("first cell");
        assert!(first.error.is_none(), "{:?}", first.error);
        let second = kernel
            .eval("a + 1", Duration::from_secs(5), &AbortSignal::none())
            .await
            .expect("second cell");
        assert_eq!(second.result.as_deref(), Some("2"));
        // Global-script semantics: `let` bindings persist across cells too.
        let _ = kernel
            .eval("let b = 10", Duration::from_secs(5), &AbortSignal::none())
            .await
            .expect("let cell");
        let read_b = kernel
            .eval("b * 2", Duration::from_secs(5), &AbortSignal::none())
            .await
            .expect("read b");
        assert_eq!(read_b.result.as_deref(), Some("20"));
        // console.log captures into the per-cell buffer.
        let logged = kernel
            .eval(
                "console.log('hi', 1, {x: 2}); 6 * 7",
                Duration::from_secs(5),
                &AbortSignal::none(),
            )
            .await
            .expect("console cell");
        assert_eq!(logged.stdout.trim_end(), "hi 1 {\"x\":2}");
        assert_eq!(logged.result.as_deref(), Some("42"));
    }

    #[tokio::test]
    async fn js_kernel_classifies_syntax_and_runtime_errors() {
        let mut kernel = JsKernel::spawn().expect("js kernel spawns");
        let syntax = kernel
            .eval("function broken(", Duration::from_secs(5), &AbortSignal::none())
            .await
            .expect("syntax cell");
        assert_eq!(
            syntax.error.as_ref().map(|e| e.kind),
            Some(EvalErrorKind::Syntax),
            "{:?}",
            syntax.error
        );
        let runtime = kernel
            .eval("undefined.foo.bar", Duration::from_secs(5), &AbortSignal::none())
            .await
            .expect("runtime cell");
        assert_eq!(
            runtime.error.as_ref().map(|e| e.kind),
            Some(EvalErrorKind::Runtime),
            "{:?}",
            runtime.error
        );
        assert!(runtime.error.unwrap().text.contains("TypeError"));
        // The kernel survives errors.
        let next = kernel
            .eval("1 + 1", Duration::from_secs(5), &AbortSignal::none())
            .await
            .expect("next cell");
        assert_eq!(next.result.as_deref(), Some("2"));
    }

    #[tokio::test]
    async fn js_kernel_timeout_kills_and_next_call_respawns() {
        let mut kernel = JsKernel::spawn().expect("js kernel spawns");
        let timed_out = kernel
            .eval("while (true) {}", Duration::from_secs(1), &AbortSignal::none())
            .await
            .expect("timeout is a classified result");
        assert_eq!(
            timed_out.error.as_ref().map(|e| e.kind),
            Some(EvalErrorKind::Timeout),
            "{:?}",
            timed_out.error
        );
        // The message must match the real semantics: the kernel IS killed on
        // timeout (channel closed, thread exits) and the next call respawns.
        let notice = timed_out.error.as_ref().map(|e| e.text.as_str()).unwrap_or("");
        assert!(
            notice.contains("killed") && notice.contains("respawn"),
            "timeout notice must describe the kill+respawn contract, got: {notice}"
        );
        // The same kernel object is dead after the timeout: state is NOT
        // retained across a timeout (parity with the Python kernel).
        let dead = kernel
            .eval("1 + 1", Duration::from_secs(1), &AbortSignal::none())
            .await
            .expect_err("a timed-out JS kernel must refuse the next cell");
        assert!(
            format!("{dead:#}").contains("killed"),
            "the dead kernel must say it was killed: {dead:#}"
        );
        let mut fresh = JsKernel::spawn().expect("respawn");
        let cell = fresh
            .eval("1 + 1", Duration::from_secs(5), &AbortSignal::none())
            .await
            .expect("cell after respawn");
        assert_eq!(cell.result.as_deref(), Some("2"));
    }

    #[tokio::test]
    async fn js_kernel_bounds_console_output() {
        let mut kernel = JsKernel::spawn().expect("js kernel spawns");
        let cell = kernel
            .eval(
                "console.log('x'.repeat(200000))",
                Duration::from_secs(5),
                &AbortSignal::none(),
            )
            .await
            .expect("cell");
        assert!(cell.stdout.len() <= MAX_STREAM_BYTES, "{}", cell.stdout.len());
    }

    // -----------------------------------------------------------------------
    // Tool surface
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn eval_tool_cross_cell_state_end_to_end() {
        if !python3_available() {
            return;
        }
        let cwd = tmpdir();
        let tool = crate::tools::create_tool("eval", &cwd.to_string_lossy()).expect("eval tool");
        let first = (tool.execute)(make_ctx(json!({ "language": "python", "code": "x = 10" })))
            .await
            .expect("first call");
        assert!(text_of(&first).contains("python: ok"), "{}", text_of(&first));
        let second = (tool.execute)(make_ctx(json!({ "language": "python", "code": "x + 5" })))
            .await
            .expect("second call");
        let text = text_of(&second);
        assert!(text.contains("python: ok"), "{text}");
        assert!(text.contains("result: 15"), "{text}");

        let js = (tool.execute)(make_ctx(json!({ "language": "js", "code": "let n = 3; n * n" })))
            .await
            .expect("js call");
        let text = text_of(&js);
        assert!(text.contains("js: ok"), "{text}");
        assert!(text.contains("result: 9"), "{text}");
    }

    #[tokio::test]
    async fn eval_tool_classifies_errors_and_redacts() {
        let cwd = tmpdir();
        let tool = crate::tools::create_tool("eval", &cwd.to_string_lossy()).expect("eval tool");
        let syntax = (tool.execute)(make_ctx(json!({ "language": "js", "code": "if (" })))
            .await
            .expect("syntax call");
        let text = text_of(&syntax);
        assert!(text.contains("js: syntax error"), "{text}");
        assert!(text.contains("error:"), "{text}");

        if python3_available() {
            let runtime = (tool.execute)(make_ctx(json!({ "language": "python", "code": "1/0" })))
                .await
                .expect("runtime call");
            let text = text_of(&runtime);
            assert!(text.contains("python: runtime error"), "{text}");
            assert!(text.contains("ZeroDivisionError"), "{text}");
        }
    }

    #[tokio::test]
    async fn eval_tool_timeout_argument_is_honored() {
        if !python3_available() {
            return;
        }
        let cwd = tmpdir();
        let tool = crate::tools::create_tool("eval", &cwd.to_string_lossy()).expect("eval tool");
        let timed_out = (tool.execute)(make_ctx(json!({
            "language": "python",
            "code": "import time; time.sleep(30)",
            "timeout": 1,
        })))
        .await
        .expect("timeout call");
        let text = text_of(&timed_out);
        assert!(text.contains("python: timeout"), "{text}");
        // The kernel respawned: the next call works.
        let next = (tool.execute)(make_ctx(json!({ "language": "python", "code": "2 + 2" })))
            .await
            .expect("call after timeout");
        assert!(text_of(&next).contains("result: 4"), "{}", text_of(&next));
    }

    #[tokio::test]
    async fn eval_tool_missing_python_is_actionable() {
        let cwd = tmpdir();
        let tool = eval_tool_with_python(&cwd.to_string_lossy(), Some("python3-definitely-missing"));
        let error = (tool.execute)(make_ctx(json!({ "language": "python", "code": "1 + 1" })))
            .await
            .expect_err("missing python must fail");
        let text = format!("{error:#}");
        assert!(text.contains("python3"), "{text}");
        assert!(text.contains("install Python 3"), "{text}");
    }

    #[tokio::test]
    async fn eval_tool_rejects_unknown_language_and_empty_code() {
        let cwd = tmpdir();
        let tool = crate::tools::create_tool("eval", &cwd.to_string_lossy()).expect("eval tool");
        let unknown = (tool.execute)(make_ctx(json!({ "language": "ruby", "code": "puts 1" })))
            .await
            .expect_err("unknown language");
        assert!(unknown.to_string().contains("python"), "{unknown}");
        let empty = (tool.execute)(make_ctx(json!({ "language": "js", "code": "  " })))
            .await
            .expect_err("empty code");
        assert!(empty.to_string().contains("code"), "{empty}");
    }
    // -----------------------------------------------------------------------
    // Additional Python kernel contracts
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn python_kernel_unicode_repr_and_stdout_round_trip() {
        if !python3_available() {
            return;
        }
        let cwd = tmpdir().to_string_lossy().into_owned();
        let mut kernel = PythonKernel::spawn(&cwd).await.expect("kernel spawns");
        // A multi-line cell mixing an assignment, a unicode print, and a
        // final expression whose repr contains non-ASCII (emoji + accents).
        let cell = kernel
            .eval(
                "s = \"héllo 🎉\"\nprint(s)\ns",
                Duration::from_secs(10),
                &AbortSignal::none(),
            )
            .await
            .expect("unicode cell");
        assert!(cell.stdout.contains("héllo 🎉"), "{:?}", cell.stdout);
        assert_eq!(cell.result.as_deref(), Some("'héllo 🎉'"));
        assert!(cell.error.is_none(), "{:?}", cell.error);
    }

    #[tokio::test]
    async fn python_kernel_repr_of_various_types() {
        if !python3_available() {
            return;
        }
        let cwd = tmpdir().to_string_lossy().into_owned();
        let mut kernel = PythonKernel::spawn(&cwd).await.expect("kernel spawns");
        let cell = kernel
            .eval("[1, 2, 3]", Duration::from_secs(10), &AbortSignal::none())
            .await
            .expect("list cell");
        assert_eq!(cell.result.as_deref(), Some("[1, 2, 3]"));
        let cell = kernel
            .eval("{'a': 1}", Duration::from_secs(10), &AbortSignal::none())
            .await
            .expect("dict cell");
        assert_eq!(cell.result.as_deref(), Some("{'a': 1}"));
        // A bare `None` expression evaluates to Python None, which the driver
        // does NOT repr (result is null in the frame) — mirroring IPython,
        // which shows nothing for a None value. This defends against a
        // regression that would start surfacing "None" as a result string.
        let cell = kernel
            .eval("None", Duration::from_secs(10), &AbortSignal::none())
            .await
            .expect("none cell");
        assert_eq!(cell.result, None);
        let cell = kernel
            .eval("True", Duration::from_secs(10), &AbortSignal::none())
            .await
            .expect("bool cell");
        assert_eq!(cell.result.as_deref(), Some("True"));
        let cell = kernel
            .eval("'text'", Duration::from_secs(10), &AbortSignal::none())
            .await
            .expect("str cell");
        assert_eq!(cell.result.as_deref(), Some("'text'"));
    }

    // -----------------------------------------------------------------------
    // Additional JS kernel contracts
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn js_kernel_const_and_function_persist_across_cells() {
        let mut kernel = JsKernel::spawn().expect("js kernel spawns");
        // `const` and `function` declarations persist (global-script
        // semantics), mirroring a Node REPL.
        let _ = kernel
            .eval("const PI = 3", Duration::from_secs(5), &AbortSignal::none())
            .await
            .expect("const cell");
        let _ = kernel
            .eval("function double(x) { return x * 2; }", Duration::from_secs(5), &AbortSignal::none())
            .await
            .expect("function cell");
        let read = kernel
            .eval("double(PI)", Duration::from_secs(5), &AbortSignal::none())
            .await
            .expect("read cell");
        assert_eq!(read.result.as_deref(), Some("6"));
        assert!(read.error.is_none(), "{:?}", read.error);
    }

    #[tokio::test]
    async fn js_kernel_memory_cap_rejects_oversized_allocation() {
        let mut kernel = JsKernel::spawn().expect("js kernel spawns");
        // Doubling a string each iteration exceeds the 64 MiB heap cap
        // within ~27 iterations; the runtime reports an Allocation error,
        // classified as Runtime (not Timeout). The loop is bounded so it
        // cannot hang: the allocation fails long before 100 iterations.
        let cell = kernel
            .eval(
                "var s = \"x\"; for (var i = 0; i < 100; i++) s = s + s;",
                Duration::from_secs(15),
                &AbortSignal::none(),
            )
            .await
            .expect("allocation cell");
        let error = cell.error.expect("allocation error classified");
        assert_eq!(error.kind, EvalErrorKind::Runtime, "{:?}", error);
        // The memory cap surfaces as either a JS InternalError ("string too
        // long") or a QuickJS Allocation error ("out of memory"), depending
        // on the engine version; both are Runtime and tear the kernel down.
        let lowered = error.text.to_ascii_lowercase();
        assert!(
            lowered.contains("too long") || lowered.contains("memory") || lowered.contains("alloc"),
            "{:?}",
            error.text
        );
        // The kernel is torn down by the allocation failure; a fresh kernel
        // works normally (respawn contract).
        let mut fresh = JsKernel::spawn().expect("respawn");
        let ok = fresh
            .eval("1 + 1", Duration::from_secs(5), &AbortSignal::none())
            .await
            .expect("cell after respawn");
        assert_eq!(ok.result.as_deref(), Some("2"));
    }

    #[tokio::test]
    async fn js_kernel_thrown_string_is_classified_as_runtime() {
        let mut kernel = JsKernel::spawn().expect("js kernel spawns");
        // A thrown non-Error value (a string) is caught and classified as
        // Runtime (the non-Error path of exception_parts, lines 788-791).
        let cell = kernel
            .eval("throw \"boom\"", Duration::from_secs(5), &AbortSignal::none())
            .await
            .expect("throw cell");
        let error = cell.error.expect("throw classified");
        assert_eq!(error.kind, EvalErrorKind::Runtime, "{:?}", error);
        assert!(error.text.contains("boom"), "{:?}", error.text);
    }

    #[tokio::test]
    async fn js_kernel_error_with_multiline_stack_formats_tail() {
        let mut kernel = JsKernel::spawn().expect("js kernel spawns");
        // An Error thrown from inside a function carries a multi-line stack;
        // format_exception must keep the tail (lines[1..]) and drop the
        // first line which repeats "name: message".
        let cell = kernel
            .eval(
                "function deep() { throw new Error(\"inner\"); }\ndeep();",
                Duration::from_secs(5),
                &AbortSignal::none(),
            )
            .await
            .expect("stack cell");
        let error = cell.error.expect("error classified");
        assert_eq!(error.kind, EvalErrorKind::Runtime, "{:?}", error);
        assert!(error.text.contains("Error: inner"), "{:?}", error.text);
        // The stack tail (at least one extra line) is appended.
        assert!(error.text.lines().count() > 1, "{:?}", error.text);
    }

    // -----------------------------------------------------------------------
    // cell_timeout bounds (shared with the notebook tool)
    // -----------------------------------------------------------------------

    #[test]
    fn cell_timeout_defaults_to_thirty_seconds() {
        assert_eq!(
            cell_timeout(&json!({})).unwrap(),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn cell_timeout_accepts_boundary_values_one_and_three_hundred() {
        assert_eq!(
            cell_timeout(&json!({ "timeout": 1 })).unwrap(),
            Duration::from_secs(1)
        );
        assert_eq!(
            cell_timeout(&json!({ "timeout": 300 })).unwrap(),
            Duration::from_secs(300)
        );
    }

    #[test]
    fn cell_timeout_rejects_out_of_range_values() {
        assert!(cell_timeout(&json!({ "timeout": 0 })).is_err());
        assert!(cell_timeout(&json!({ "timeout": 301 })).is_err());
        assert!(cell_timeout(&json!({ "timeout": -1 })).is_err());
    }

    // -----------------------------------------------------------------------
    // Python frame parse / classify (no real python needed)
    // -----------------------------------------------------------------------

    #[test]
    fn classify_python_error_maps_syntax_families() {
        assert_eq!(classify_python_error("SyntaxError"), EvalErrorKind::Syntax);
        assert_eq!(classify_python_error("IndentationError"), EvalErrorKind::Syntax);
        assert_eq!(classify_python_error("TabError"), EvalErrorKind::Syntax);
        // Anything else is a runtime error.
        assert_eq!(classify_python_error("ZeroDivisionError"), EvalErrorKind::Runtime);
        assert_eq!(classify_python_error(""), EvalErrorKind::Runtime);
    }

    #[test]
    fn parse_cell_result_decodes_full_frame() {
        let frame = br#"{"stdout":"out","stderr":"err","error":null,"error_type":null,"result":"42"}"#;
        let cell = parse_cell_result(frame).expect("frame parses");
        assert_eq!(cell.stdout, "out");
        assert_eq!(cell.stderr, "err");
        assert_eq!(cell.result.as_deref(), Some("42"));
        assert!(cell.error.is_none());
    }

    #[test]
    fn parse_cell_result_classifies_error_from_error_type() {
        let frame = br#"{"stdout":"","stderr":"","error":"Traceback\nSyntaxError: bad","error_type":"SyntaxError","result":null}"#;
        let cell = parse_cell_result(frame).expect("frame parses");
        let error = cell.error.expect("error present");
        assert_eq!(error.kind, EvalErrorKind::Syntax);
        assert!(error.text.contains("SyntaxError"));
        assert!(cell.result.is_none());
    }

    #[test]
    fn parse_cell_result_rejects_invalid_json() {
        assert!(parse_cell_result(b"not json").is_err());
    }

    #[test]
    fn cell_result_is_timeout_detects_timeout_kind_only() {
        let timeout = CellResult {
            stdout: String::new(),
            stderr: String::new(),
            result: None,
            error: Some(CellError { kind: EvalErrorKind::Timeout, text: "t".into() }),
        };
        assert!(timeout.is_timeout());
        let runtime = CellResult {
            stdout: String::new(),
            stderr: String::new(),
            result: None,
            error: Some(CellError { kind: EvalErrorKind::Runtime, text: "r".into() }),
        };
        assert!(!runtime.is_timeout());
        let clean = CellResult {
            stdout: String::new(),
            stderr: String::new(),
            result: Some("x".into()),
            error: None,
        };
        assert!(!clean.is_timeout());
    }
}
