//! PTY execution for the bash tool (opt-in interactive mode).
//!
//! The default bash paths run commands with stdin wired to `/dev/null` and no
//! controlling terminal, so interactive programs like `sudo` (which prompt for
//! a password on the controlling terminal) fail or hang. With `pty: true` the
//! bash tool instead spawns the command in a pseudo-terminal so it owns a real
//! controlling terminal and can prompt. This module owns that PTY session:
//!
//! - **Spawn**: the command runs through the same `shell -c` argv the normal
//!   path uses, on the slave side of a `portable-pty` pair. portable-pty's
//!   `pre_exec` does `setsid()` + `TIOCSCTTY`, so the child is a session
//!   leader with the PTY as its controlling terminal and its pid is the
//!   process group id — `killpg(child_pid, …)` reaps the whole tree.
//! - **I/O**: stdout+stderr are merged by the PTY itself (the slave is the
//!   child's stdin/stdout/stderr). Output is read from the master and
//!   streamed through the same `stream` callback as the other paths; the
//!   optional `input` string (e.g. a sudo password) is written to the master
//!   up front, followed by a newline. With no input, the VEOF character
//!   (EOT, 0x04) is written instead — a PTY has no half-close, so dropping
//!   the writer alone cannot signal EOF; the line discipline turns a VEOF at
//!   the start of a line into a zero-length read, a clean EOF the child
//!   observes.
//! - **Timeout/abort**: the async supervisor races the master reader against
//!   the timeout and the abort signal. On either, the child's process group
//!   is SIGTERMed, output keeps draining through a short grace, then the
//!   group is SIGKILLed. When the group is gone, every slave fd is closed and
//!   the master read returns EIO, which unblocks the reader thread and lets
//!   the drain complete — the same bounded-grace policy as `brush`.
//! - **Fallback**: a spawn failure (openpty, reader/writer, or fork/exec
//!   error) reports [`PtyRunOutcome::SpawnFailed`] so `run_bash_core` falls
//!   back to the normal execution paths with a note in the output — unless
//!   `input` was configured, in which case the call errors (the normal paths
//!   wire stdin to `/dev/null` and would hang the command on input that
//!   never arrives). Real-time keyboard forwarding from the TUI is
//!   intentionally out of scope for now: stdin is provided up front via the
//!   `input` parameter.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use pi_agent::AbortSignal;

#[cfg(unix)]
use std::ffi::OsString;
#[cfg(unix)]
use std::io::{Read, Write};
#[cfg(unix)]
use std::time::Instant;

#[cfg(unix)]
use nix::sys::signal::{Signal, killpg};
#[cfg(unix)]
use nix::unistd::Pid;
#[cfg(unix)]
use portable_pty::{CommandBuilder, ExitStatus, PtySize, native_pty_system};
#[cfg(unix)]
use tokio::sync::mpsc;

/// Grace period after SIGTERM: the dying command (e.g. sudo restoring the
/// terminal) unwinds while the remaining output still drains. Mirrors
/// `brush::BRUSH_EXIT_GRACE`.
const PTY_EXIT_GRACE: Duration = Duration::from_secs(2);
/// Hard bound on draining output after the group is dead (or killed), so an
/// escaped descendant that inherited the slave fd cannot stall the tool call
/// past this window.
const PTY_DRAIN_BOUND: Duration = Duration::from_secs(1);
/// PTY geometry for spawned sessions. 24 rows × 80 cols is the classic
/// default; password prompts (sudo) render identically at any size.
const PTY_ROWS: u16 = 24;
const PTY_COLS: u16 = 80;
/// Master read chunk size.
const PTY_READ_CHUNK: usize = 32 * 1024;

/// Outcome of a PTY run.
pub(crate) enum PtyRunOutcome {
    /// The command ran to completion. `exit_code` is `None` when the child
    /// was killed by a signal, matching the sandbox runner's convention (the
    /// bash tool treats a signal-killed child as success).
    Executed { exit_code: Option<i32> },
    /// Cut off by the tool timeout; the process group was reaped.
    TimedOut,
    /// Cut off by the caller's abort signal; the process group was reaped.
    Cancelled,
    /// The PTY could not be created or the child could not be spawned. The
    /// caller falls back to the normal execution paths with a note, unless
    /// input was configured — then the call errors (see the module docs).
    SpawnFailed(String),
}

/// Runs `argv` (the `shell -c <command>` vector from the normal path) in a
/// PTY, streaming the merged output through `stream`. The optional `input` is
/// written to the PTY's stdin (followed by a newline) before output is read —
/// e.g. a sudo password; with no `input`, a VEOF (EOT, 0x04) is written so
/// the child's stdin observes a clean EOF instead of blocking forever.
/// `timeout` and `abort` cut the run off and reap the child's process group.
/// This is the opt-in interactive path; the default (no `pty` argument) never
/// reaches it.
#[cfg(unix)]
pub(crate) async fn run_pty_command(
    cwd: &Path,
    argv: &[String],
    env: &[(String, String)],
    input: Option<&str>,
    timeout: Option<Duration>,
    abort: AbortSignal,
    stream: Arc<dyn Fn(&[u8]) + Send + Sync>,
) -> PtyRunOutcome {
    // 1. Open the PTY pair (master + slave).
    let pty_system = native_pty_system();
    let pair = match pty_system.openpty(PtySize {
        rows: PTY_ROWS,
        cols: PTY_COLS,
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(pair) => pair,
        Err(err) => return PtyRunOutcome::SpawnFailed(format!("openpty failed: {err:#}")),
    };

    // 2. Build the command: the same argv as the normal path, an explicit
    //    environment (no host inheritance, mirroring the subprocess path's
    //    `env_clear` + `envs`), and the working directory.
    let mut builder =
        CommandBuilder::from_argv(argv.iter().map(OsString::from).collect::<Vec<_>>());
    builder.env_clear();
    for (key, value) in env {
        builder.env(key, value);
    }
    builder.cwd(cwd);

    // 3. Spawn on the slave side; the child is a session leader with the PTY
    //    as its controlling terminal (see the module docs). Drop the parent's
    //    slave handle so the master read returns EIO once the child and its
    //    process group are gone.
    let mut child = match pair.slave.spawn_command(builder) {
        Ok(child) => child,
        Err(err) => return PtyRunOutcome::SpawnFailed(format!("spawn failed: {err:#}")),
    };
    drop(pair.slave);

    // 4. Upfront input (e.g. a sudo password) — or a true EOF when there is
    //    none. The master writer is taken and dropped in every case. With
    //    input, it is written followed by a newline before output is read:
    //    the tty line discipline buffers it until the program's prompt read.
    //    With no input, the VEOF character (EOT, 0x04) is sent: a PTY has no
    //    half-close, so dropping the writer alone cannot signal EOF — the
    //    master stays open through `pair.master` and the reader clone and the
    //    slave would block on read forever. The line discipline turns a VEOF
    //    at the start of a line into a zero-length read, a clean EOF the
    //    child observes (matching the `/dev/null` stdin of the normal paths).
    //    A write/flush failure means the child cannot read its input; kill
    //    the group and report a spawn failure so the caller does not hang
    //    waiting on a stuck child.
    match pair.master.take_writer() {
        Ok(mut writer) => {
            let write_result = if let Some(input) = input {
                writer
                    .write_all(input.as_bytes())
                    .and_then(|()| writer.write_all(b"\n"))
                    .and_then(|()| writer.flush())
            } else {
                writer.write_all(b"\x04").and_then(|()| writer.flush())
            };
            if let Err(err) = write_result {
                let _ = kill_group(&mut child, Signal::SIGKILL);
                return PtyRunOutcome::SpawnFailed(format!("pty write failed: {err:#}"));
            }
        }
        Err(err) => {
            let _ = kill_group(&mut child, Signal::SIGKILL);
            return PtyRunOutcome::SpawnFailed(format!("pty writer failed: {err:#}"));
        }
    }

    // 5. Reader: block on the master and forward chunks through a channel.
    //    The read returns EIO once every slave fd is closed (the child and
    //    its group are gone), which unblocks the thread; a timeout/abort kill
    //    triggers the same unblock, so the reader never needs polling.
    let (chunk_tx, mut chunk_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let reader = match pair.master.try_clone_reader() {
        Ok(reader) => reader,
        Err(err) => {
            let _ = kill_group(&mut child, Signal::SIGKILL);
            return PtyRunOutcome::SpawnFailed(format!("pty reader failed: {err:#}"));
        }
    };
    let reader_task = tokio::task::spawn_blocking(move || {
        let mut reader = reader;
        let mut buf = vec![0u8; PTY_READ_CHUNK];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break, // EOF or EIO: the slave side is gone
                Ok(n) => {
                    if chunk_tx.send(buf[..n].to_vec()).is_err() {
                        break; // supervisor is gone
                    }
                }
            }
        }
    });

    // 6. Supervisor: race the output stream against the timeout and abort.
    //    Timeout is measured from spawn (like the other paths). Chunks are
    //    streamed here, on the async side, so the stream callback never runs
    //    on the blocking reader thread.
    let timeout_fut = async {
        if let Some(t) = timeout {
            tokio::time::sleep(t).await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    tokio::pin!(timeout_fut);
    let abort_fut = abort.cancelled();
    tokio::pin!(abort_fut);

    enum Race {
        Eof,
        TimedOut,
        Aborted,
    }
    let race = loop {
        tokio::select! {
            maybe = chunk_rx.recv() => match maybe {
                Some(chunk) => stream(&chunk),
                None => break Race::Eof,
            },
            _ = &mut timeout_fut => break Race::TimedOut,
            _ = &mut abort_fut => break Race::Aborted,
        }
    };

    let outcome = match race {
        Race::Eof => {
            // The master read returned EIO: every slave fd is closed, so the
            // child has exited (or detached itself from the tty). Reap with a
            // bounded poll; a child that outlived its terminal (e.g. a
            // daemonized process) is killed so the tool call cannot hang.
            let status = match reap_with_timeout(&mut child, PTY_DRAIN_BOUND).await {
                Some(status) => Some(status),
                None => {
                    let _ = kill_group(&mut child, Signal::SIGTERM);
                    let status = reap_with_timeout(&mut child, PTY_EXIT_GRACE).await;
                    match status {
                        Some(status) => Some(status),
                        None => {
                            let _ = kill_group(&mut child, Signal::SIGKILL);
                            reap_with_timeout(&mut child, PTY_DRAIN_BOUND).await
                        }
                    }
                }
            };
            let exit_code = status
                .filter(|s| s.signal().is_none())
                .map(|s| s.exit_code() as i32);
            PtyRunOutcome::Executed { exit_code }
        }
        Race::TimedOut => {
            kill_and_drain(&mut child, &mut chunk_rx, &stream).await;
            PtyRunOutcome::TimedOut
        }
        Race::Aborted => {
            kill_and_drain(&mut child, &mut chunk_rx, &stream).await;
            PtyRunOutcome::Cancelled
        }
    };

    // 7. In every case above the channel closed (reader thread finished) or a
    //    drain bound elapsed. A reader still blocked on an escaped
    //    descendant's slave fd is abandoned — same policy as brush's
    //    uninterruptible busy loop; the thread dies with the process.
    drop(pair.master);
    drop(reader_task);
    outcome
}

/// Non-unix stub: PTY sessions need `killpg` for group reaping, so on other
/// platforms the caller falls back to the normal execution paths.
#[cfg(not(unix))]
pub(crate) async fn run_pty_command(
    cwd: &Path,
    argv: &[String],
    env: &[(String, String)],
    input: Option<&str>,
    timeout: Option<Duration>,
    abort: AbortSignal,
    stream: Arc<dyn Fn(&[u8]) + Send + Sync>,
) -> PtyRunOutcome {
    let _ = (cwd, argv, env, input, timeout, abort, stream);
    PtyRunOutcome::SpawnFailed("pty mode is only supported on unix".to_owned())
}

/// SIGTERM then SIGKILL the child's process group, draining output between
/// the signals so the agent still sees everything the dying command produced.
/// The master read returns EIO once the group is gone, so `drain_output`
/// completes; an escaped descendant that inherited the slave fd is bounded by
/// `PTY_DRAIN_BOUND`.
#[cfg(unix)]
async fn kill_and_drain(
    child: &mut Box<dyn portable_pty::Child + Send + Sync>,
    rx: &mut mpsc::UnboundedReceiver<Vec<u8>>,
    stream: &Arc<dyn Fn(&[u8]) + Send + Sync>,
) {
    let _ = kill_group(child, Signal::SIGTERM);
    drain_output(rx, stream, PTY_EXIT_GRACE).await;
    let _ = kill_group(child, Signal::SIGKILL);
    drain_output(rx, stream, PTY_DRAIN_BOUND).await;
    // Reap so the child does not linger as a zombie.
    let _ = reap_with_timeout(child, PTY_DRAIN_BOUND).await;
}

/// Streams PTY chunks from `rx` to `stream` until the channel closes (the
/// master read hit EIO) or `bound` elapses, whichever comes first.
#[cfg(unix)]
async fn drain_output(
    rx: &mut mpsc::UnboundedReceiver<Vec<u8>>,
    stream: &Arc<dyn Fn(&[u8]) + Send + Sync>,
    bound: Duration,
) {
    let sleep = tokio::time::sleep(bound);
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            maybe = rx.recv() => match maybe {
                Some(chunk) => stream(&chunk),
                None => break,
            },
            _ = &mut sleep => break,
        }
    }
}

/// Polls `child.try_wait()` until it reports an exit status or `bound`
/// elapses, whichever comes first. Errors are treated as "not exited yet".
#[cfg(unix)]
async fn reap_with_timeout(
    child: &mut Box<dyn portable_pty::Child + Send + Sync>,
    bound: Duration,
) -> Option<ExitStatus> {
    let deadline = Instant::now() + bound;
    loop {
        if let Some(status) = child.try_wait().ok().flatten() {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Sends `signal` to the child's process group. The child is a session leader
/// (portable-pty `setsid` in `pre_exec`), so its pid is the group id and
/// `killpg` reaps the whole tree. Best-effort: a group that already exited
/// (ESRCH) or a failed signal is a no-op.
#[cfg(unix)]
fn kill_group(child: &mut Box<dyn portable_pty::Child + Send + Sync>, signal: Signal) -> bool {
    match child.process_id() {
        Some(pid) => killpg(Pid::from_raw(pid as i32), signal).is_ok(),
        None => false,
    }
}
