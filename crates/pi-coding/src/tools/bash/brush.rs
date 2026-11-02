//! Embedded brush-shell execution for the bash tool (OMP/pi parity).
//!
//! The default (unsandboxed) bash tool execution runs the command through an
//! embedded `brush_core::Shell` session instead of a `/bin/bash` subprocess,
//! matching the OMP/pi shell engine (earendil-works/brush). This module owns
//! everything specific to that in-process execution:
//!
//! - **Session model**: one brush session per bash tool invocation, discarded
//!   after the command. Cross-command shell state (variables, `cd`, aliases,
//!   functions, history) is NOT carried over — identical to the `/bin/bash -c`
//!   subprocess semantics the tool had before.
//! - **Environment model** (mirrors OMP's pi-shell): `do_not_inherit_env` —
//!   the host environment is NOT inherited; the tool rebuilds the environment
//!   explicitly (inherited vars minus the PI_* session keys, plus the live
//!   session metadata and the non-interactive command contract, plus `PWD`
//!   derived from the working directory). `skip_well_known_vars` — shell
//!   sensitive variables (PS1, PWD, SHLVL, bash function exports, ...) are
//!   not auto-initialized. No profile/rc loading (`ProfileLoadBehavior::Skip`
//!   + `RcLoadBehavior::Skip`).
//! - **I/O**: stdin is `/dev/null` (matches the subprocess path's
//!   `Stdio::null()`); stdout+stderr are merged onto one pipe in arrival
//!   order (writer duplicated for fd 1 and fd 2), read by a poll-based reader
//!   that streams into the caller's `OutputAccumulator` and never blocks on a
//!   quiet command or a detached descendant holding the pipe open.
//! - **Authority boundary**: Brush is used only for danger-full-access
//!   generations. Confined generations execute through the governed subprocess
//!   boundary because an in-process shell shares the host's authority.
//! - **Timeout/abort**: brush runs on a dedicated `pi-brush-bash` thread with
//!   its own current-thread tokio runtime. On timeout/abort, only processes
//!   forked by that thread (and their descendants) that were not present in
//!   the pre-command baseline — guarded by `/proc/<pid>/stat` start times
//!   against pid reuse — are SIGTERM/SIGKILLed. Enumeration is rooted at
//!   `/proc/<pid>/task/<brush_tid>/children`, so concurrent children of other
//!   threads (PDF extractors, converters, etc.) are not in that snapshot.
//!   TERM/KILL is skipped unless that TID still has the start time captured
//!   at publish, and each descendant pid is re-checked by start time before
//!   each signal. This is **not** a strict zero-attribution guarantee:
//!   brush-core owned-shell builtins run on `spawn_blocking` workers outside
//!   this TID (`brush-core` 0.5.0 `commands.rs`
//!   `execute_via_builtin_in_owned_shell`, entered from `interp.rs`
//!   pipeline `OwnedShell`); children can reparent off the TID; a TID can
//!   still race reuse between `/proc` walks; and there is no rescan after
//!   SIGTERM for children born in that window. The session-tree mutex is
//!   **scheduling only** (one danger bash at a time per session). It is
//!   not the ownership boundary: sibling `pi-brush-bash` threads already have
//!   disjoint TID roots. The shell is given a short grace to unwind, and a
//!   pure-builtin busy loop that cannot be interrupted in-process is
//!   abandoned (the tool call returns; the thread dies with the process).
//!   Coordinator and durable-subagent sessions each own a lock and may
//!   run concurrently. Confined subprocess and governed PTY runs never
//!   queue on that lock.
//! - **Host guards**: in-process execution shares the rpi process, so
//!   builtins that would replace/stop/mutate the host are refused with an
//!   actionable message (`exec`, `suspend`, `ulimit`, `umask`) and `kill` is
//!   guarded against targeting the host pid (`$$`). `exec` in a subshell is
//!   still allowed (brush spawns a child there). These are the only
//!   observable deviations from the subprocess path; each is documented here.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use brush_builtins::{BuiltinSet, ShellBuilderExt};
use brush_core::{
    CommandArg, ExecutionContext, ExecutionResult, ProfileLoadBehavior, RcLoadBehavior, Shell,
    ShellFd, ShellVariable,
    builtins::BoxFuture,
    error::Error as BrushError,
    extensions::DefaultShellExtensions,
    openfiles::{self, OpenFile},
};
use pi_agent::AbortSignal;
use tokio::sync::{Mutex, oneshot};

/// Type of a brush builtin execute function for the default extension set.
type DefaultExecuteFunc = brush_core::builtins::CommandExecuteFunc<DefaultShellExtensions>;

/// Original `kill` execute function, captured on first use so the guarded
/// replacement can pass legitimate signals through. All sessions share the
/// same function pointer (same crate code), so one `OnceLock` is enough.
static ORIGINAL_KILL_EXECUTE: OnceLock<DefaultExecuteFunc> = OnceLock::new();
/// Original `exec` execute function (see [`guarded_exec_execute`]).
static ORIGINAL_EXEC_EXECUTE: OnceLock<DefaultExecuteFunc> = OnceLock::new();

/// Grace period after killing a timed-out/aborted command's descendants:
/// killed children unwind the shell's awaits, letting it finish normally.
const BRUSH_EXIT_GRACE: Duration = Duration::from_secs(2);
/// Idle poll window for the output reader. While the command runs, quiet
/// periods are tolerated (the reader just polls again); once the run is over,
/// an idle window means the pipe has been fully drained.
const BRUSH_READER_IDLE_MS: u16 = 200;

/// Outcome of an in-process brush execution.
pub(crate) enum BrushRunOutcome {
    /// The command ran to completion through the brush engine.
    Executed {
        exit_code: Option<i32>,
        /// Monotonic execution duration measured at the engine's command
        /// start (after the TID baseline), never including the caller's
        /// queue wait for the session-tree brush lock.
        execution_elapsed: Duration,
    },
    /// Cut off by the tool timeout (descendants were reaped).
    TimedOut { execution_elapsed: Duration },
    /// Cut off by the caller's abort signal (descendants were reaped).
    Cancelled { execution_elapsed: Duration },
    /// brush cannot take this command (parse failure, or descendant reaping
    /// unavailable on this platform) — the caller falls back to the
    /// `/bin/bash` subprocess path so the observable result is unchanged.
    Fallback,
}

/// Result sent from the brush thread back to the caller.
enum ThreadResult {
    Executed(ExecutionResult),
    /// The command did not parse; the subprocess fallback should run it.
    ParseFailed,
    /// The brush engine itself failed (runtime/build/run error).
    Failed(String),
}

/// Runs `command` through an embedded brush shell session.
///
/// `env` is the explicit environment for the session (no host inheritance).
/// Output is streamed to `stream` in arrival order (stdout+stderr merged).
/// The optional `timeout` and `abort` cut the run off; either way the
/// command's descendants are reaped. Parse failures and non-Linux targets
/// report [`BrushRunOutcome::Fallback`].
pub(crate) async fn run_brush_command(
    cwd: &Path,
    command: &str,
    env: &[(String, String)],
    timeout: Option<Duration>,
    abort: AbortSignal,
    stream: Arc<dyn Fn(&[u8]) + Send + Sync>,
) -> Result<BrushRunOutcome> {
    #[cfg(not(target_os = "linux"))]
    {
        // Descendant reaping for timeout/abort relies on /proc, which is
        // Linux-only; on other platforms the subprocess path stays in charge.
        let _ = (cwd, command, env, timeout, abort, stream);
        return Ok(BrushRunOutcome::Fallback);
    }
    #[cfg(target_os = "linux")]
    run_brush_command_impl(cwd, command, env, timeout, abort, stream).await
}

/// The start-boundary outcome of the wait for a session-tree brush
/// execution lock. Only the danger-mode brush path waits here; confined
/// subprocess and governed PTY runs never queue on this lock.
#[derive(Debug)]
pub(crate) enum BashExecLockStart {
    /// The lock is held; the command may start executing. Execution timing
    /// begins only after this point.
    Acquired(tokio::sync::OwnedMutexGuard<()>),
    /// The caller aborted while queued; nothing ran.
    Aborted,
}

/// The wait for same-session danger-mode bash scheduling is unbounded and
/// abort-responsive: the lock holder is itself bounded by its own execution
/// timeout or the user's Stop, so the queue drains in finite time and a
/// queued command must run rather than fail without executing. The lock is
/// not descendant ownership: each `pi-brush-bash` TID is already a disjoint
/// reap root. Two clones of one session share one lock; coordinator and each
/// durable subagent own distinct locks. An aborted call reports
/// [`BashExecLockStart::Aborted`].
pub(crate) async fn acquire_bash_exec_lock(
    lock: Arc<Mutex<()>>,
    abort: &AbortSignal,
) -> Result<BashExecLockStart> {
    // Abort wins at the pre-start boundary: an already-ready abort resolves
    // before the lock future is even constructed, so it beats a free lock.
    if abort.is_aborted() {
        return Ok(BashExecLockStart::Aborted);
    }
    let lock = lock.lock_owned();
    tokio::pin!(lock);
    let abort_fut = abort.cancelled();
    tokio::pin!(abort_fut);
    // Biased polling order is the boundary invariant: an abort racing a
    // free lock resolves as aborted.
    Ok(tokio::select! {
        biased;
        _ = &mut abort_fut => BashExecLockStart::Aborted,
        guard = lock => BashExecLockStart::Acquired(guard),
    })
}

/// Kernel identity of the dedicated `pi-brush-bash` thread: the reap root
/// TID plus `/proc/<pid>/task/<tid>/stat` start time (field 22). A later
/// walk or kill uses this pair so a recycled TID after thread exit cannot
/// become another thread's reap root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BrushThreadIdentity {
    tid: i32,
    start_time: u64,
}

#[cfg(target_os = "linux")]
impl BrushThreadIdentity {
    fn current() -> Option<Self> {
        let tid = nix::unistd::gettid().as_raw();
        let start_time = thread_start_time(std::process::id() as i32, tid)?;
        Some(Self { tid, start_time })
    }

    fn is_live(self) -> bool {
        thread_start_time(std::process::id() as i32, self.tid) == Some(self.start_time)
    }
}

/// One-shot baseline handoff between the async caller and the brush thread.
///
/// The brush thread must not execute the command until the caller has
/// captured the pre-command process baseline (the reap-root snapshot), and
/// must not wait by polling if the caller never gets there: a caller future
/// dropped after the thread published its identity used to leave a
/// `pi-brush-bash` thread spinning on an atomic forever. Ownership is the
/// cancellation mechanism: the caller end owns the gate sender. Releasing
/// the worker is one explicit send; every other exit path of the caller
/// (fallback, task abort, future drop, panic unwind) drops the sender,
/// which makes the worker's blocking `recv` return at once. The worker then
/// exits before building its runtime or spawning any child shell, and it
/// waits in a futex sleep — never a yield loop — so an orphaned worker
/// costs no CPU.
struct BaselineCaller {
    identity_rx: oneshot::Receiver<BrushThreadIdentity>,
    gate_tx: mpsc::Sender<()>,
}

impl BaselineCaller {
    /// Waits (asynchronously, waker-based) for the worker's kernel identity.
    /// `Err` means the worker ended before publishing — the caller falls
    /// back to the subprocess path.
    async fn wait_for_identity(&mut self) -> Result<BrushThreadIdentity> {
        (&mut self.identity_rx)
            .await
            .map_err(|_| anyhow!("brush thread ended before publishing its identity"))
    }

    /// Releases the worker to run the command (baseline captured). Consumes
    /// `self`: after the release there is no caller end left to drop, so no
    /// later exit path can cancel the worker by accident.
    fn release(self) {
        let _ = self.gate_tx.send(());
    }
}

/// Worker-side half of [`baseline_handoff`], owned by the brush thread.
struct BaselineWorker {
    identity_tx: oneshot::Sender<BrushThreadIdentity>,
    gate_rx: mpsc::Receiver<()>,
}

impl BaselineWorker {
    /// Publishes `identity`, then blocks until the caller releases the gate.
    /// Returns `false` when the caller end was dropped first: the worker
    /// must exit without building a runtime or executing the command.
    fn wait_for_baseline(self, identity: BrushThreadIdentity) -> bool {
        let _ = self.identity_tx.send(identity);
        self.gate_rx.recv() == Ok(())
    }
}

/// Creates both halves of the baseline handoff gate.
fn baseline_handoff() -> (BaselineCaller, BaselineWorker) {
    let (identity_tx, identity_rx) = oneshot::channel::<BrushThreadIdentity>();
    let (gate_tx, gate_rx) = mpsc::channel::<()>();
    (
        BaselineCaller {
            identity_rx,
            gate_tx,
        },
        BaselineWorker {
            identity_tx,
            gate_rx,
        },
    )
}

/// Joins the brush thread off the async runtime: `JoinHandle::join` parks
/// its caller, so a direct join inside this future would block a tokio
/// worker. Runs the join in `spawn_blocking` instead. Callers drop the
/// baseline gate first, so a worker still parked on the handoff exits
/// before the join has anything to wait on.
#[cfg(target_os = "linux")]
async fn join_brush_thread(handle: std::thread::JoinHandle<()>) {
    let _ = tokio::task::spawn_blocking(move || handle.join()).await;
}

#[cfg(target_os = "linux")]
async fn run_brush_command_impl(
    cwd: &Path,
    command: &str,
    env: &[(String, String)],
    timeout: Option<Duration>,
    abort: AbortSignal,
    stream: Arc<dyn Fn(&[u8]) + Send + Sync>,
) -> Result<BrushRunOutcome> {
    // /proc is required for thread-scoped descendant reaping. The baseline
    // itself is taken after the brush thread publishes its TID (below).
    if !Path::new("/proc/self").exists() {
        return Ok(BrushRunOutcome::Fallback);
    }

    // Merged stdout+stderr capture: one pipe, writer duplicated for fd 1 and
    // fd 2 so interleaving order is preserved (same design as the sandbox
    // runner's merged stream). stdin is /dev/null, matching the subprocess
    // path's `Stdio::null()`.
    let (reader, writer) = std::io::pipe().map_err(|e| anyhow!("{}", e))?;
    let writer_err = writer.try_clone().map_err(|e| anyhow!("{}", e))?;
    let mut fds: HashMap<ShellFd, OpenFile> = HashMap::new();
    fds.insert(0, openfiles::null().map_err(|e| anyhow!("{}", e))?);
    fds.insert(1, OpenFile::from(writer));
    fds.insert(2, OpenFile::from(writer_err));

    let cwd_path = cwd.to_path_buf();
    let cwd_display = cwd.to_string_lossy().into_owned();
    let command = command.to_owned();
    let env = env.to_vec();
    let (tx, mut rx) = oneshot::channel::<ThreadResult>();

    // Brush thread TID is the reap root: only children forked by that thread
    // (and their descendants) may be killed on timeout/abort. The thread
    // publishes its TID through the baseline handoff, blocks until the
    // caller captures the pre-command baseline, then runs the command.
    let (mut baseline_caller, baseline_worker) = baseline_handoff();

    // The brush session runs on a dedicated thread with its own current-thread
    // tokio runtime: long-running commands (external children, quiet gaps)
    // must not block the main runtime, and a busy builtin loop that cannot be
    // interrupted in-process can be abandoned without stalling the app.
    let handle = std::thread::Builder::new()
        .name("pi-brush-bash".to_owned())
        .spawn(move || {
            // Publish this thread's kernel identity, then block until the
            // caller has captured the pre-command baseline. If the caller's
            // future was dropped first, the gate sender is gone and this
            // returns false: exit before any runtime or child shell exists.
            // The wait is a blocking recv (futex sleep), never a poll loop.
            let Some(identity) = BrushThreadIdentity::current() else {
                return;
            };
            if !baseline_worker.wait_for_baseline(identity) {
                return;
            }
            let send = |result: ThreadResult| {
                let _ = tx.send(result);
            };
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    send(ThreadResult::Failed(format!("brush runtime: {err}")));
                    return;
                }
            };
            let thread_result = rt.block_on(async move {
                let mut builder = Shell::builder()
                    .default_builtins(BuiltinSet::BashMode)
                    .do_not_inherit_env(true)
                    .skip_well_known_vars(true)
                    .profile(ProfileLoadBehavior::Skip)
                    .rc(RcLoadBehavior::Skip)
                    .interactive(false)
                    .login(false)
                    .working_dir(cwd_path)
                    .fds(fds)
                    .shell_name("bash".to_owned());
                for (name, value) in &env {
                    if name == "PWD" {
                        continue; // derived from the working directory below
                    }
                    let mut var = ShellVariable::new(value.clone());
                    var.export();
                    builder = builder.var(name.clone(), var);
                }
                // $PWD mirrors the subprocess path, where bash derives it
                // from the working directory: skip_well_known_vars leaves it
                // unset, and any inherited PWD would be stale (the rebuilt
                // environment keeps the host's value).
                let mut var = ShellVariable::new(cwd_display);
                var.export();
                builder = builder.var("PWD", var);
                let mut shell = match builder.build().await {
                    Ok(shell) => shell,
                    Err(err) => {
                        return ThreadResult::Failed(format!("brush shell: {err}"));
                    }
                };
                install_host_guards(&mut shell);
                // Parse check first: commands brush cannot parse fall back to
                // the subprocess path (same observable result).
                if shell.parse_string(command.as_str()).is_err() {
                    return ThreadResult::ParseFailed;
                }
                match shell.run_dash_c_command(command.as_str()).await {
                    Ok(result) => ThreadResult::Executed(result),
                    Err(err) => ThreadResult::Failed(format!("brush run: {err}")),
                }
            });
            send(thread_result);
        })
        .map_err(|e| anyhow!("{}", e))?;

    // Wait for the brush thread to publish its identity (an async await on
    // the handoff, not a poll loop), snapshot that thread's (normally empty)
    // children, then release the thread to run the command.
    let identity = match baseline_caller.wait_for_identity().await {
        Ok(identity) => identity,
        Err(_) => {
            // Thread died before publishing an identity; nothing to reap. Drop
            // the gate first (a worker parked on the handoff exits at once),
            // then join off the async runtime. The thread has already ended
            // (or is unwinding), so the join is prompt.
            drop(baseline_caller);
            join_brush_thread(handle).await;
            return Ok(BrushRunOutcome::Fallback);
        }
    };
    if !identity.is_live() {
        drop(baseline_caller);
        join_brush_thread(handle).await;
        return Ok(BrushRunOutcome::Fallback);
    }
    let Some(baseline) = descendant_snapshot(identity.tid) else {
        // Should be unreachable after the /proc check above; if the thread's
        // task dir vanished, cancel the worker (gate drop — it exits before
        // any command runs) and fall back rather than reaping process-wide.
        drop(baseline_caller);
        join_brush_thread(handle).await;
        return Ok(BrushRunOutcome::Fallback);
    };
    // Execution begins here: releasing the baseline gate lets the brush
    // thread run the command. The measured window below is the command's
    // real execution duration; the caller's queue wait for the brush lock
    // is already outside it. The gate sender is consumed by the release, so
    // no later caller exit path can disconnect the worker.
    let execution_started = Instant::now();
    baseline_caller.release();

    // Reader: polls the merged pipe and streams chunks into the accumulator.
    // While the command runs, quiet periods are tolerated; once `ended` is
    // set (run finished or abandoned), an idle window completes the drain. A
    // detached descendant holding the pipe open cannot hang the tool call.
    let ended = Arc::new(AtomicBool::new(false));
    let ended_flag = ended.clone();
    let reader_task = tokio::task::spawn_blocking(move || {
        use std::os::fd::AsFd;
        let mut reader = reader;
        let mut buf = [0u8; 32 * 1024];
        loop {
            let ready = {
                let mut fds = [nix::poll::PollFd::new(
                    reader.as_fd(),
                    nix::poll::PollFlags::POLLIN,
                )];
                nix::poll::poll(&mut fds, nix::poll::PollTimeout::from(BRUSH_READER_IDLE_MS))
            };
            match ready {
                Ok(0) => {
                    if ended_flag.load(Ordering::Acquire) {
                        break; // run over and output idle: drained
                    }
                    continue; // command still running quietly
                }
                Ok(_) => {}
                Err(_) => break,
            }
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break, // EOF or closed
                Ok(n) => stream(&buf[..n]),
            }
        }
    });

    enum Race {
        Done(ThreadResult),
        TimedOut,
        Aborted,
    }
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
    let race = tokio::select! {
        res = &mut rx => match res {
            Ok(result) => Race::Done(result),
            Err(_) => Race::Done(ThreadResult::Failed(
                "brush thread ended without a result".to_owned(),
            )),
        },
        _ = &mut timeout_fut => Race::TimedOut,
        _ = &mut abort_fut => Race::Aborted,
    };

    // Freeze the execution window at the race resolution: cleanup (descendant
    // reaping, drain) below is not part of the command's run time.
    let execution_elapsed = execution_started.elapsed();
    match race {
        Race::Done(ThreadResult::Executed(result)) => {
            ended.store(true, Ordering::Release);
            let _ = reader_task.await;
            Ok(BrushRunOutcome::Executed {
                exit_code: Some(u8::from(result.exit_code) as i32),
                execution_elapsed,
            })
        }
        Race::Done(ThreadResult::ParseFailed) => {
            ended.store(true, Ordering::Release);
            let _ = reader_task.await;
            Ok(BrushRunOutcome::Fallback)
        }
        Race::Done(ThreadResult::Failed(message)) => {
            ended.store(true, Ordering::Release);
            let _ = reader_task.await;
            Err(anyhow!("{message}"))
        }
        Race::TimedOut | Race::Aborted => {
            // Kill the command's descendants, then give the shell a short
            // grace to unwind (killed children resolve its awaits). A
            // pure-builtin busy loop cannot be interrupted in-process, so the
            // thread is abandoned after the grace; the tool call still
            // returns, and the thread dies with the process.
            kill_new_descendants(&baseline, identity);
            let _ = tokio::time::timeout(BRUSH_EXIT_GRACE, &mut rx).await;
            ended.store(true, Ordering::Release);
            let _ = reader_task.await;
            drop(handle); // detach if still running
            // Abort wins over timeout (matches the sandbox runner).
            if abort.is_aborted() {
                Ok(BrushRunOutcome::Cancelled {
                    execution_elapsed,
                })
            } else {
                Ok(BrushRunOutcome::TimedOut {
                    execution_elapsed,
                })
            }
        }
    }
}

/// Replaces host-dangerous builtins on a freshly built shell:
///
/// - `exec` would replace the rpi process (brush's exec is a real
///   `exec(2)`); refused except inside a subshell, where brush spawns a
///   child instead.
/// - `suspend` would SIGSTOP the rpi process; refused.
/// - `ulimit`/`umask` would mutate the rpi process's own resource limits /
///   file-mode mask (persistent host state the subprocess path isolates);
///   refused.
/// - `kill` is allowed, but refuses to signal the host pid (`$$` expands to
///   the rpi process id in-process); legitimate targets pass through.
///
/// All refusals write an actionable message to stderr and exit 127.
fn install_host_guards(shell: &mut Shell<DefaultShellExtensions>) {
    if let Some(reg) = shell.builtins().get("kill").cloned() {
        let _ = ORIGINAL_KILL_EXECUTE.set(reg.execute_func);
        let mut guarded = reg;
        guarded.execute_func = guarded_kill_execute;
        shell.register_builtin("kill", guarded);
    }
    if let Some(reg) = shell.builtins().get("exec").cloned() {
        let _ = ORIGINAL_EXEC_EXECUTE.set(reg.execute_func);
        let mut guarded = reg;
        guarded.execute_func = guarded_exec_execute;
        shell.register_builtin("exec", guarded);
    }
    for name in ["suspend", "ulimit", "umask"] {
        if let Some(reg) = shell.builtins().get(name).cloned() {
            let mut guarded = reg;
            guarded.execute_func = refuse_host_mutating_builtin;
            shell.register_builtin(name, guarded);
        }
    }
}

/// `exec`: refused at the top level (would replace the host process), but a
/// subshell `exec` passes through — brush spawns a child there, which is safe.
fn guarded_exec_execute(
    context: ExecutionContext<'_, DefaultShellExtensions>,
    args: Vec<CommandArg>,
) -> BoxFuture<'_, Result<ExecutionResult, BrushError>> {
    Box::pin(async move {
        if context.shell.is_subshell()
            && let Some(original) = ORIGINAL_EXEC_EXECUTE.get().copied()
        {
            return original(context, args).await;
        }
        host_refusal(&context, "exec", "it would replace the rpi host process");
        Ok(ExecutionResult::new(127))
    })
}

/// `kill`: refuses signaling the host pid (`$$` in-process is the rpi
/// process); every other target passes through to brush's kill builtin.
fn guarded_kill_execute(
    context: ExecutionContext<'_, DefaultShellExtensions>,
    args: Vec<CommandArg>,
) -> BoxFuture<'_, Result<ExecutionResult, BrushError>> {
    Box::pin(async move {
        // Scan EVERY numeric pid argument, not just the first: brush's kill
        // applies the signal to ALL listed targets, so `kill 1234 $$` would
        // otherwise pass a first-pid check and then signal the host process.
        // kill's own parsing skips options/signal specs like -9 and job specs
        // starting with '%', so numeric args with those prefixes are not pids.
        let host_pid = std::process::id() as i64;
        let targets_host = args.iter().any(|arg| {
            let text = arg.to_string();
            if text.starts_with('%') || text.starts_with('-') {
                return false;
            }
            text.parse::<i64>().is_ok_and(|pid| pid == host_pid)
        });
        if targets_host {
            host_refusal(
                &context,
                "kill",
                &format!(
                    "refusing to signal the host process (pid {host_pid}); $$ is the rpi process in the embedded shell"
                ),
            );
            return Ok(ExecutionResult::new(127));
        }
        match ORIGINAL_KILL_EXECUTE.get().copied() {
            Some(original) => original(context, args).await,
            None => {
                host_refusal(&context, "kill", "internal: kill builtin unavailable");
                Ok(ExecutionResult::new(127))
            }
        }
    })
}

/// `suspend`/`ulimit`/`umask`: refused because they mutate the host process
/// (SIGSTOP, resource limits, file-mode mask) that the subprocess path would
/// isolate.
fn refuse_host_mutating_builtin(
    context: ExecutionContext<'_, DefaultShellExtensions>,
    _args: Vec<CommandArg>,
) -> BoxFuture<'_, Result<ExecutionResult, BrushError>> {
    Box::pin(async move {
        host_refusal(
            &context,
            &context.command_name,
            "it would mutate host rpi process state in the embedded shell",
        );
        Ok(ExecutionResult::new(127))
    })
}

/// Writes the standard refusal message to the shell's stderr.
fn host_refusal(context: &ExecutionContext<'_, DefaultShellExtensions>, name: &str, why: &str) {
    let mut stderr = context.stderr();
    let _ = writeln!(
        stderr,
        "{name}: not supported in the embedded brush shell ({why})"
    );
}

// ---------------------------------------------------------------------------
// Descendant tracking (Linux /proc)
// ---------------------------------------------------------------------------

/// pid -> start time (`/proc/<pid>/stat` field 22) for every descendant under
/// a scoped root (brush thread children + their recursive descendants).
#[cfg(target_os = "linux")]
type DescendantSnapshot = HashMap<i32, u64>;

/// Snapshot of processes currently in the brush thread's descendant tree.
/// Rooted at children forked by `brush_tid` of this process; deeper levels
/// include every thread of each child process (grandchildren may be forked by
/// any thread of a child).
#[cfg(target_os = "linux")]
fn descendant_snapshot(brush_tid: i32) -> Option<DescendantSnapshot> {
    if !Path::new("/proc/self").exists() {
        return None;
    }
    let mut snapshot = DescendantSnapshot::new();
    collect_thread_descendants(std::process::id() as i32, brush_tid, &mut snapshot);
    Some(snapshot)
}

/// Top-level collector rooted at a single thread of `pid`: reads only
/// `/proc/<pid>/task/<tid>/children`, then recurses with
/// [`collect_descendants`] so grandchildren forked by any thread of a child
/// process are included. This is the brush reap scope — never the whole
/// process's every-thread children list.
#[cfg(target_os = "linux")]
fn collect_thread_descendants(pid: i32, tid: i32, out: &mut DescendantSnapshot) {
    let Ok(contents) = std::fs::read_to_string(format!("/proc/{pid}/task/{tid}/children")) else {
        return; // thread or process vanished; keep what we have
    };
    for child in contents.split_whitespace() {
        let Ok(child) = child.parse::<i32>() else {
            continue;
        };
        if let Some(start) = process_start_time(child) {
            out.insert(child, start);
        }
        collect_descendants(child, out);
    }
}

/// Recursively collect every child of `pid` across all of its threads.
/// Used for levels below the brush thread: a grandchild may be forked by any
/// thread of an intermediate child process, so the full task directory must
/// be scanned — reading only `task/<pid>/children` (the main thread) would
/// silently miss them.
#[cfg(target_os = "linux")]
fn collect_descendants(pid: i32, out: &mut DescendantSnapshot) {
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return; // process vanished mid-walk; keep what we have
    };
    for task in tasks.flatten() {
        let Ok(tid) = task.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        let Ok(contents) = std::fs::read_to_string(format!("/proc/{pid}/task/{tid}/children"))
        else {
            continue;
        };
        for child in contents.split_whitespace() {
            let Ok(child) = child.parse::<i32>() else {
                continue;
            };
            if let Some(start) = process_start_time(child) {
                out.insert(child, start);
            }
            collect_descendants(child, out);
        }
    }
}

/// Start time of a process from `/proc/<pid>/stat` (field 22; index 19 after
/// the parenthesized `comm` field, which may itself contain spaces).
#[cfg(target_os = "linux")]
fn process_start_time(pid: i32) -> Option<u64> {
    stat_start_time(&format!("/proc/{pid}/stat"))
}

/// Start time of one thread of `pid` from `/proc/<pid>/task/<tid>/stat`.
#[cfg(target_os = "linux")]
fn thread_start_time(pid: i32, tid: i32) -> Option<u64> {
    stat_start_time(&format!("/proc/{pid}/task/{tid}/stat"))
}

#[cfg(target_os = "linux")]
fn stat_start_time(path: &str) -> Option<u64> {
    let stat = std::fs::read_to_string(path).ok()?;
    let close = stat.rfind(')')?;
    let fields: Vec<&str> = stat[close + 1..].split_whitespace().collect();
    fields.get(19).and_then(|field| field.parse().ok())
}

/// SIGTERM then SIGKILL descendants under `identity` that are not in the
/// baseline (or that reused a pid). Best-effort, not a strict guarantee:
/// owned-shell builtins, reparenting, residual TID-reuse between walks, and
/// children born after this one-shot snapshot (no rescan after TERM) are
/// outside cleanup. Identity is re-checked before the walk, after it, and
/// before each signal; each pid is re-checked by start time before TERM/KILL.
#[cfg(target_os = "linux")]
fn kill_new_descendants(baseline: &DescendantSnapshot, identity: BrushThreadIdentity) {
    kill_new_descendants_checked(baseline, identity, identity);
}

#[cfg(target_os = "linux")]
fn kill_new_descendants_checked(
    baseline: &DescendantSnapshot,
    published: BrushThreadIdentity,
    root: BrushThreadIdentity,
) {
    if !published.is_live() {
        return;
    }
    if !root.is_live() {
        return;
    }
    let mut current = DescendantSnapshot::new();
    collect_thread_descendants(std::process::id() as i32, root.tid, &mut current);
    if !root.is_live() {
        return;
    }
    let targets: Vec<(i32, u64)> = current
        .iter()
        .filter(|(pid, start)| baseline.get(pid) != Some(start))
        .map(|(pid, start)| (*pid, *start))
        .collect();
    if targets.is_empty() {
        return;
    }
    signal_matching_descendants(&targets, nix::sys::signal::Signal::SIGTERM);
    std::thread::sleep(Duration::from_millis(100));
    signal_matching_descendants(&targets, nix::sys::signal::Signal::SIGKILL);
}

#[cfg(target_os = "linux")]
fn signal_matching_descendants(targets: &[(i32, u64)], signal: nix::sys::signal::Signal) {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    for &(pid, start) in targets {
        if process_start_time(pid) != Some(start) {
            continue;
        }
        let _ = kill(Pid::from_raw(pid), signal);
    }
}

// ---------------------------------------------------------------------------
// Tests (Linux /proc thread-scoped descendant contract)
// ---------------------------------------------------------------------------

/// Test-only holder for a session-tree brush execution lock: lets a test own
/// that tree's lock while another command runs to prove path bypass (confined
/// subprocess and governed PTY runs never queue behind this lock).
#[cfg(test)]
pub(crate) async fn test_hold_bash_exec_lock(
    lock: Arc<Mutex<()>>,
) -> tokio::sync::OwnedMutexGuard<()> {
    lock.lock_owned().await
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::time::Duration;

    fn process_alive(pid: i32) -> bool {
        // /proc/<pid> remains for zombies until reaped; only non-zombie
        // states count as still live (Z = dead, waiting for waitpid).
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        let Some(close) = stat.rfind(')') else {
            return false;
        };
        let state = stat[close + 1..].split_whitespace().next().unwrap_or("");
        state != "Z" && !state.is_empty()
    }

    fn this_thread_identity() -> BrushThreadIdentity {
        BrushThreadIdentity::current().expect("live test thread start time")
    }

    fn publish_thread_identity(worker: BaselineWorker) -> bool {
        let Some(identity) = BrushThreadIdentity::current() else {
            return false;
        };
        worker.wait_for_baseline(identity)
    }

    /// A child forked by a sibling thread must not appear in the brush-thread
    /// subtree — this is the contract that prevents timeout reaping from
    /// SIGKILLing concurrent PDF/converter children.
    #[test]
    fn thread_scoped_snapshot_excludes_sibling_thread_children() {
        let pid = std::process::id() as i32;
        let self_tid = nix::unistd::gettid().as_raw();

        let (child_tx, child_rx) = mpsc::channel::<i32>();
        let sibling = std::thread::Builder::new()
            .name("sibling-spawner".to_owned())
            .spawn(move || {
                let mut child = Command::new("sleep")
                    .arg("30")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("spawn sibling sleep");
                let child_pid = child.id() as i32;
                let _ = child_tx.send(child_pid);
                let _ = child.wait();
            })
            .expect("spawn sibling thread");

        let sibling_child = child_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("sibling child pid");
        assert!(
            process_alive(sibling_child),
            "sibling child {sibling_child} must be alive"
        );

        let mut brush_scope = DescendantSnapshot::new();
        collect_thread_descendants(pid, self_tid, &mut brush_scope);
        assert!(
            !brush_scope.contains_key(&sibling_child),
            "sibling thread child {sibling_child} must not appear in this thread's subtree: {brush_scope:?}"
        );

        // Process-wide scan still sees it (documents the old bug surface).
        let mut whole = DescendantSnapshot::new();
        collect_descendants(pid, &mut whole);
        assert!(
            whole.contains_key(&sibling_child),
            "process-wide scan must still observe sibling child {sibling_child}"
        );

        // Cleanup: SIGKILL the sibling child so the thread can exit.
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(sibling_child),
            nix::sys::signal::Signal::SIGKILL,
        );
        let _ = sibling.join();
    }

    /// Grandchildren forked by a direct child of this thread must still be
    /// enumerated (and therefore reaped) under the thread-scoped collector.
    #[test]
    fn thread_scoped_snapshot_includes_grandchildren() {
        let pid = std::process::id() as i32;
        let self_tid = nix::unistd::gettid().as_raw();

        // `sh -c 'sleep 300 & wait'` keeps `sh` as the direct child and
        // `sleep` as a grandchild (sh does not exec, so both stay live).
        let mut sh = Command::new("sh")
            .arg("-c")
            .arg("sleep 300 & echo $!; wait")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sh with sleep grandchild");
        let sh_pid = sh.id() as i32;

        // Read the grandchild pid that sh printed.
        let mut stdout = sh.stdout.take().expect("sh stdout");
        let mut buf = String::new();
        // Bounded read: one line with the background sleep pid.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while buf
            .lines()
            .next()
            .map(|l| l.trim().is_empty())
            .unwrap_or(true)
            && std::time::Instant::now() < deadline
        {
            use std::io::Read;
            let mut tmp = [0u8; 64];
            match stdout.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => buf.push_str(&String::from_utf8_lossy(&tmp[..n])),
                Err(_) => break,
            }
        }
        let grandchild: i32 = buf
            .lines()
            .next()
            .expect("grandchild pid line")
            .trim()
            .parse()
            .expect("grandchild pid number");
        assert!(
            process_alive(grandchild),
            "grandchild sleep {grandchild} must be alive"
        );

        let mut snap = DescendantSnapshot::new();
        collect_thread_descendants(pid, self_tid, &mut snap);
        assert!(
            snap.contains_key(&sh_pid),
            "direct child sh {sh_pid} must be in thread scope: {snap:?}"
        );
        assert!(
            snap.contains_key(&grandchild),
            "grandchild sleep {grandchild} must be in thread scope: {snap:?}"
        );

        // kill_new_descendants with empty baseline must reap both.
        let empty = DescendantSnapshot::new();
        kill_new_descendants(&empty, this_thread_identity());

        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            if !process_alive(sh_pid) && !process_alive(grandchild) {
                let _ = sh.wait();
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(sh_pid),
            nix::sys::signal::Signal::SIGKILL,
        );
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(grandchild),
            nix::sys::signal::Signal::SIGKILL,
        );
        let _ = sh.wait();
        panic!("thread-scoped reap left sh={sh_pid} and/or sleep={grandchild} alive");
    }

    /// Sibling children must survive a kill_new_descendants pass scoped to
    /// this thread — the PDF race root-cause regression.
    #[test]
    fn kill_new_descendants_does_not_reap_sibling_thread_children() {
        let (child_tx, child_rx) = mpsc::channel::<i32>();
        let sibling = std::thread::Builder::new()
            .name("sibling-spawner".to_owned())
            .spawn(move || {
                let mut child = Command::new("sleep")
                    .arg("30")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("spawn sibling sleep");
                let child_pid = child.id() as i32;
                let _ = child_tx.send(child_pid);
                let _ = child.wait();
            })
            .expect("spawn sibling thread");

        let sibling_child = child_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("sibling child pid");

        // Empty baseline + this thread's TID: should not touch sibling child.
        let empty = DescendantSnapshot::new();
        kill_new_descendants(&empty, this_thread_identity());

        std::thread::sleep(Duration::from_millis(150));
        assert!(
            process_alive(sibling_child),
            "sibling child {sibling_child} must survive brush-scoped reap"
        );

        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(sibling_child),
            nix::sys::signal::Signal::SIGKILL,
        );
        let _ = sibling.join();
    }

    /// TERM/KILL must not follow a TID whose start time no longer matches
    /// the captured identity: that is the TID-reuse hazard after thread exit.
    #[test]
    fn kill_new_descendants_skips_stale_tid_identity() {
        let (child_tx, child_rx) = mpsc::channel::<(BrushThreadIdentity, i32)>();
        let sibling = std::thread::Builder::new()
            .name("tid-reuse-spawner".to_owned())
            .spawn(move || {
                let identity = BrushThreadIdentity::current().expect("sibling identity");
                let mut child = Command::new("sleep")
                    .arg("30")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("spawn sibling sleep");
                let child_pid = child.id() as i32;
                let _ = child_tx.send((identity, child_pid));
                let _ = child.wait();
            })
            .expect("spawn sibling thread");

        let (live_identity, sibling_child) = child_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("sibling identity and child pid");
        assert!(
            process_alive(sibling_child),
            "sibling child {sibling_child} must be alive"
        );
        let stale = BrushThreadIdentity {
            tid: live_identity.tid,
            start_time: live_identity.start_time.wrapping_add(1),
        };
        let empty = DescendantSnapshot::new();
        kill_new_descendants(&empty, stale);
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            process_alive(sibling_child),
            "stale TID identity must not reap sibling child {sibling_child}"
        );
        kill_new_descendants(&empty, live_identity);
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            if !process_alive(sibling_child) {
                let _ = sibling.join();
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(sibling_child),
            nix::sys::signal::Signal::SIGKILL,
        );
        let _ = sibling.join();
        panic!("matching identity left sibling child {sibling_child} alive");
    }

    /// If the published identity is still live but the walk identity is
    /// already stale, collection and signaling must not run (TID reuse
    /// between the first live check and the `/proc` walk).
    #[test]
    fn kill_new_descendants_skips_when_root_identity_diverges() {
        let (child_tx, child_rx) = mpsc::channel::<(BrushThreadIdentity, i32)>();
        let sibling = std::thread::Builder::new()
            .name("tid-diverge-spawner".to_owned())
            .spawn(move || {
                let identity = BrushThreadIdentity::current().expect("sibling identity");
                let mut child = Command::new("sleep")
                    .arg("30")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("spawn sibling sleep");
                let child_pid = child.id() as i32;
                let _ = child_tx.send((identity, child_pid));
                let _ = child.wait();
            })
            .expect("spawn sibling thread");

        let (live_identity, sibling_child) = child_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("sibling identity and child pid");
        let stale = BrushThreadIdentity {
            tid: live_identity.tid,
            start_time: live_identity.start_time.wrapping_add(1),
        };
        let empty = DescendantSnapshot::new();
        kill_new_descendants_checked(&empty, live_identity, stale);
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            process_alive(sibling_child),
            "divergent walk identity must not reap sibling child {sibling_child}"
        );
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(sibling_child),
            nix::sys::signal::Signal::SIGKILL,
        );
        let _ = sibling.join();
    }

    /// A live pid whose recorded start time no longer matches must not be
    /// signaled (pid reuse between snapshot and TERM/KILL).
    #[test]
    fn signal_matching_descendants_skips_stale_pid_start_time() {
        let mut child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let pid = child.id() as i32;
        let start = process_start_time(pid).expect("child start time");
        signal_matching_descendants(
            &[(pid, start.wrapping_add(1))],
            nix::sys::signal::Signal::SIGKILL,
        );
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            process_alive(pid),
            "stale start time must not SIGKILL {pid}"
        );
        signal_matching_descendants(&[(pid, start)], nix::sys::signal::Signal::SIGKILL);
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            if !process_alive(pid) {
                let _ = child.wait();
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
        panic!("matching start time left child {pid} alive");
    }

    #[test]
    fn kill_new_descendants_sigkills_term_resistant_child_after_root_exits() {
        let (tx, rx) = mpsc::channel::<(BrushThreadIdentity, i32, i32)>();
        let worker = std::thread::Builder::new()
            .name("pi-brush-bash".to_owned())
            .spawn(move || {
                let identity = this_thread_identity();
                let mut parent = Command::new("sleep")
                    .arg("30")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("spawn TERM-responsive parent");
                let parent_pid = parent.id() as i32;
                let mut child = Command::new("sh")
                    .arg("-c")
                    .arg(r#"trap "" TERM; exec sleep 30"#)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("spawn TERM-resistant child");
                let child_pid = child.id() as i32;
                let _ = tx.send((identity, parent_pid, child_pid));
                let _ = parent.wait();
                let _ = child.wait();
            })
            .expect("spawn brush worker");

        let (identity, parent_pid, child_pid) = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("worker identity and child pids");
        assert!(process_alive(parent_pid), "TERM-responsive parent must start live");
        assert!(process_alive(child_pid), "TERM-resistant child must start live");
        let empty = DescendantSnapshot::new();
        kill_new_descendants_checked(&empty, identity, identity);
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            if !process_alive(parent_pid) && !process_alive(child_pid) {
                let _ = worker.join();
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(parent_pid),
            nix::sys::signal::Signal::SIGKILL,
        );
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(child_pid),
            nix::sys::signal::Signal::SIGKILL,
        );
        let _ = worker.join();
        panic!("TERM-resistant child {child_pid} survived after root exit during grace");
    }

    fn session_lock() -> Arc<Mutex<()>> {
        Arc::new(Mutex::new(()))
    }

    /// The wait is unbounded while the lock is held elsewhere, and resolves
    /// promptly once the holder releases — a queued command must run rather
    /// than fail at an arbitrary bound.
    #[tokio::test]
    async fn exec_lock_waits_until_holder_releases() {
        let lock = session_lock();
        let holder = lock.clone().lock_owned().await;
        let (_controller, abort) = pi_agent::AbortController::new();
        let waiter = tokio::spawn({
            let abort = abort.clone();
            async move { acquire_bash_exec_lock(lock, &abort).await }
        });
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            !waiter.is_finished(),
            "held lock must keep the unbounded wait pending"
        );
        drop(holder);
        let outcome = tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("waiter resolves after release")
            .expect("acquire result");
        assert!(
            matches!(outcome, Ok(BashExecLockStart::Acquired(_))),
            "released lock must resolve as acquired, got {outcome:?}"
        );
    }

    /// A caller aborting while queued resolves as `Aborted` with the lock
    /// still held by the predecessor; nothing was acquired.
    #[tokio::test]
    async fn exec_lock_reports_aborted_while_queued() {
        let lock = session_lock();
        let holder = lock.clone().lock_owned().await;
        let (controller, abort) = pi_agent::AbortController::new();
        let waiter = tokio::spawn({
            let abort = abort.clone();
            async move { acquire_bash_exec_lock(lock, &abort).await }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        controller.abort();
        let outcome = tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("aborted locked waiter resolves")
            .expect("acquire result");
        assert!(
            matches!(outcome, Ok(BashExecLockStart::Aborted)),
            "abort while queued must resolve as aborted, got {outcome:?}"
        );
        drop(holder);
    }

    /// A free lock is acquired: only after `Acquired` may execution timing
    /// begin (the acquisition itself must not consume the run's window).
    #[tokio::test]
    async fn exec_lock_acquires_when_free() {
        let lock = session_lock();
        let (_controller, abort) = pi_agent::AbortController::new();
        let started = Instant::now();
        let outcome = tokio::time::timeout(
            Duration::from_secs(2),
            acquire_bash_exec_lock(lock, &abort),
        )
        .await
        .expect("free lock acquires promptly")
        .expect("acquire result");
        assert!(
            matches!(outcome, BashExecLockStart::Acquired(_)),
            "free lock must resolve as acquired, got {outcome:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "free-lock acquisition must resolve promptly, took {:?}",
            started.elapsed()
        );
    }

    /// An already-ready abort wins over a free lock: the call resolves as
    /// `Aborted` without ever constructing the lock future, so no guard is
    /// retained. Proves the abort-precedence boundary at the pre-start edge.
    #[tokio::test]
    async fn exec_lock_pre_aborted_beats_free_lock() {
        let (controller, abort) = pi_agent::AbortController::new();
        controller.abort();
        let outcome = tokio::time::timeout(
            Duration::from_millis(500),
            acquire_bash_exec_lock(session_lock(), &abort),
        )
        .await
        .expect("pre-aborted acquire resolves immediately")
        .expect("acquire result");
        assert!(
            matches!(outcome, BashExecLockStart::Aborted),
            "pre-aborted signal must resolve as aborted, got {outcome:?}"
        );
    }

    /// Distinct session-tree locks do not queue on each other: holding one
    /// tree's lock must not delay acquire on another tree.
    #[tokio::test]
    async fn exec_lock_distinct_session_trees_do_not_queue() {
        let coordinator = session_lock();
        let subagent = session_lock();
        let _held = coordinator.lock_owned().await;
        let (_controller, abort) = pi_agent::AbortController::new();
        let started = Instant::now();
        let outcome = tokio::time::timeout(
            Duration::from_millis(200),
            acquire_bash_exec_lock(subagent, &abort),
        )
        .await
        .expect("other-tree acquire must not wait on this tree's lock")
        .expect("acquire result");
        assert!(
            matches!(outcome, BashExecLockStart::Acquired(_)),
            "other session tree must acquire immediately, got {outcome:?}"
        );
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "cross-tree acquire must not wait on the held lock: {:?}",
            started.elapsed()
        );
    }

    /// The orphaned-caller regression: the worker has published its TID and
    /// is parked on the baseline gate when the async caller is dropped
    /// (task abort / future drop). The gate sender's drop must cancel the
    /// worker — it joins within a bound, never executes, and no yield-spin
    /// thread (the six-day orphan burning ~55% CPU) is left behind.
    #[tokio::test]
    async fn baseline_gate_cancels_worker_when_caller_future_drops() {
        let (mut caller, worker) = baseline_handoff();
        let executed = Arc::new(AtomicBool::new(false));
        let executed_worker = executed.clone();

        let handle = std::thread::Builder::new()
            .name("pi-brush-bash-gate-test".to_owned())
            .spawn(move || {
                if !publish_thread_identity(worker) {
                    return; // caller dropped before the baseline handoff
                }
                executed_worker.store(true, Ordering::Release);
            })
            .expect("spawn gate worker thread");

        // Deterministic seam: wait_for_identity resolves only after the worker
        // has published its identity, i.e. it is now parked on the gate.
        let _identity = caller
            .wait_for_identity()
            .await
            .expect("worker published identity");

        // Caller future dropped / tokio task aborted: every exit path
        // releases the gate by dropping it.
        drop(caller);

        let join = tokio::task::spawn_blocking(move || handle.join());
        let joined = tokio::time::timeout(Duration::from_secs(5), join)
            .await
            .expect("worker must exit promptly once the caller drops")
            .expect("join task completes");
        assert!(joined.is_ok(), "worker thread must not panic");
        assert!(
            !executed.load(Ordering::Acquire),
            "cancelled worker must never reach execution"
        );
    }

    /// Dropping the caller before the worker publishes its TID also
    /// cancels: the TID send becomes a no-op and the disconnected gate ends
    /// the wait at once.
    #[tokio::test]
    async fn baseline_gate_cancels_worker_when_caller_drops_before_publish() {
        let (caller, worker) = baseline_handoff();
        let executed = Arc::new(AtomicBool::new(false));
        let executed_worker = executed.clone();

        let handle = std::thread::Builder::new()
            .name("pi-brush-bash-gate-test".to_owned())
            .spawn(move || {
                if !publish_thread_identity(worker) {
                    return;
                }
                executed_worker.store(true, Ordering::Release);
            })
            .expect("spawn gate worker thread");
        drop(caller);

        let join = tokio::task::spawn_blocking(move || handle.join());
        let joined = tokio::time::timeout(Duration::from_secs(5), join)
            .await
            .expect("worker must exit promptly on a pre-publish drop")
            .expect("join task completes");
        assert!(joined.is_ok());
        assert!(!executed.load(Ordering::Acquire));
    }

    /// Normal handoff: the release lets the parked worker run exactly once.
    #[tokio::test]
    async fn baseline_gate_release_runs_worker_exactly_once() {
        let (mut caller, worker) = baseline_handoff();
        let executed = Arc::new(AtomicBool::new(false));
        let executed_worker = executed.clone();

        let handle = std::thread::Builder::new()
            .name("pi-brush-bash-gate-test".to_owned())
            .spawn(move || {
                if !publish_thread_identity(worker) {
                    return;
                }
                executed_worker.store(true, Ordering::Release);
            })
            .expect("spawn gate worker thread");

        let _identity = caller
            .wait_for_identity()
            .await
            .expect("worker published identity");
        caller.release();

        let join = tokio::task::spawn_blocking(move || handle.join());
        let joined = tokio::time::timeout(Duration::from_secs(5), join)
            .await
            .expect("released worker must finish promptly")
            .expect("join task completes");
        assert!(joined.is_ok());
        assert!(
            executed.load(Ordering::Acquire),
            "released worker must execute"
        );
    }

    /// The async fallback join must not block a runtime worker: through the
    /// shared helper, a worker still parked on the baseline gate joins
    /// promptly once the gate is dropped first — the exact order the early
    /// fallback paths use (gate drop, then off-runtime join).
    #[tokio::test]
    async fn fallback_join_helper_joins_parked_worker_after_gate_drop() {
        let (mut caller, worker) = baseline_handoff();

        let handle = std::thread::Builder::new()
            .name("pi-brush-bash-gate-test".to_owned())
            .spawn(move || {
                if !publish_thread_identity(worker) {
                    return;
                }
            })
            .expect("spawn gate worker thread");

        // Deterministic seam: the worker has published its TID and is
        // parked on the baseline gate.
        let _identity = caller
            .wait_for_identity()
            .await
            .expect("worker published identity");

        // Cancellation drops the gate first; the join inside the helper
        // (spawn_blocking) must then resolve promptly — it must never park
        // the calling runtime worker on a thread join.
        drop(caller);
        tokio::time::timeout(Duration::from_secs(5), join_brush_thread(handle))
            .await
            .expect("fallback join must resolve promptly after the gate drops");
    }
}
