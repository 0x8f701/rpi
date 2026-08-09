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
//! - **Sandbox split**: an active sandbox config routes through the existing
//!   `unshare` subprocess runner (`crate::sandbox::run_in_sandbox`); the
//!   in-process brush shell cannot be wrapped by `unshare`, so the sandbox
//!   wins and keeps its subprocess path. This module is only reached when no
//!   sandbox is active.
//! - **Fallback policy**: if brush cannot parse the command, or descendant
//!   reaping for timeout/abort is unavailable (non-Linux), execution falls
//!   back to the plain `/bin/bash` subprocess path so the observable result
//!   is unchanged. Runtime command errors stay in brush (they produce output
//!   and an exit code like bash would).
//! - **Timeout/abort**: brush runs on a dedicated thread with its own
//!   current-thread tokio runtime. On timeout/abort, processes spawned by the
//!   command (descendants of this process that were not present at the start,
//!   guarded by `/proc/<pid>/stat` start times against pid reuse) are
//!   SIGTERM/SIGKILLed, the shell is given a short grace to unwind, and a
//!   pure-builtin busy loop that cannot be interrupted in-process is
//!   abandoned (the tool call returns; the thread dies with the process).
//!   Brush executions are serialized process-wide so descendant attribution
//!   never crosses runs.
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
use std::time::Duration;

use anyhow::{Result, anyhow};
use brush_builtins::{BuiltinSet, ShellBuilderExt};
use brush_core::{
    CommandArg, ExecutionContext, ExecutionResult, ProfileLoadBehavior, RcLoadBehavior, Shell,
    ShellFd, ShellVariable, builtins::BoxFuture, error::Error as BrushError,
    extensions::DefaultShellExtensions, openfiles::{self, OpenFile},
};
use pi_agent::AbortSignal;
use tokio::sync::{Mutex, oneshot};

/// Type of a brush builtin execute function for the default extension set.
type DefaultExecuteFunc =
    brush_core::builtins::CommandExecuteFunc<DefaultShellExtensions>;

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

/// Process-wide serialization of brush executions. Descendant attribution for
/// timeout/abort reaping (baseline snapshot + kill of new descendants) is
/// only sound when no other bash run is spawning descendants concurrently, so
/// brush runs take turns. `run_bash_core` holds the same lock for the
/// sandboxed subprocess path, so a brush timeout can never reap another
/// bash run's children either.
static BASH_EXEC_LOCK: Mutex<()> = Mutex::const_new(());

/// Outcome of an in-process brush execution.
pub(crate) enum BrushRunOutcome {
    /// The command ran to completion through the brush engine.
    Executed { exit_code: Option<i32> },
    /// Cut off by the tool timeout (descendants were reaped).
    TimedOut,
    /// Cut off by the caller's abort signal (descendants were reaped).
    Cancelled,
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

/// Serializes a bash execution (brush or sandboxed subprocess) against other
/// bash executions. `timeout` bounds the wait for the lock itself: a run that
/// cannot even start within its own timeout reports timed out instead of
/// queueing forever behind a predecessor; an aborted call reports cancelled.
/// `None` (no timeout) waits as long as needed.
pub(crate) async fn acquire_bash_exec_lock(
    timeout: Option<Duration>,
    abort: &AbortSignal,
) -> Result<Option<tokio::sync::MutexGuard<'static, ()>>> {
    let lock = BASH_EXEC_LOCK.lock();
    tokio::pin!(lock);
    let abort_fut = abort.cancelled();
    tokio::pin!(abort_fut);
    let timeout_fut = async {
        if let Some(t) = timeout {
            tokio::time::sleep(t).await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    tokio::pin!(timeout_fut);
    Ok(tokio::select! {
        guard = lock => Some(guard),
        _ = &mut abort_fut => None,
        _ = &mut timeout_fut => None,
    })
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
    // Baseline descendants of this process (pid -> start time). Only
    // processes absent from the baseline — or present with a different start
    // time, i.e. a recycled pid — may be reaped on timeout/abort.
    let Some(baseline) = descendant_snapshot() else {
        // Cannot reap safely (no /proc); keep the subprocess path.
        return Ok(BrushRunOutcome::Fallback);
    };

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

    // The brush session runs on a dedicated thread with its own current-thread
    // tokio runtime: long-running commands (external children, quiet gaps)
    // must not block the main runtime, and a busy builtin loop that cannot be
    // interrupted in-process can be abandoned without stalling the app.
    let handle = std::thread::Builder::new()
        .name("pi-brush-bash".to_owned())
        .spawn(move || {
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

    match race {
        Race::Done(ThreadResult::Executed(result)) => {
            ended.store(true, Ordering::Release);
            let _ = reader_task.await;
            Ok(BrushRunOutcome::Executed {
                exit_code: Some(u8::from(result.exit_code) as i32),
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
            kill_new_descendants(&baseline);
            let _ = tokio::time::timeout(BRUSH_EXIT_GRACE, &mut rx).await;
            ended.store(true, Ordering::Release);
            let _ = reader_task.await;
            drop(handle); // detach if still running
            // Abort wins over timeout (matches the sandbox runner).
            if abort.is_aborted() {
                Ok(BrushRunOutcome::Cancelled)
            } else {
                Ok(BrushRunOutcome::TimedOut)
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
/// All refusals write an actionable message to stderr and exit 127. The
/// sandboxed subprocess path (or the parse-fallback subprocess path) is the
/// documented escape for commands that genuinely need the real builtins.
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
        "{name}: not supported in the embedded brush shell ({why}); re-run the command with sandboxed=true to use the isolated subprocess path"
    );
}

// ---------------------------------------------------------------------------
// Descendant tracking (Linux /proc)
// ---------------------------------------------------------------------------

/// pid -> start time (`/proc/<pid>/stat` field 22) for every descendant of
/// this process, recursively.
#[cfg(target_os = "linux")]
type DescendantSnapshot = HashMap<i32, u64>;

#[cfg(target_os = "linux")]
fn descendant_snapshot() -> Option<DescendantSnapshot> {
    if !Path::new("/proc/self").exists() {
        return None;
    }
    let mut snapshot = DescendantSnapshot::new();
    collect_descendants(std::process::id() as i32, &mut snapshot);
    Some(snapshot)
}

#[cfg(target_os = "linux")]
fn collect_descendants(pid: i32, out: &mut DescendantSnapshot) {
    // /proc/<pid>/task/<tid>/children lists the children forked by ONE
    // thread. Commands spawned by the brush thread are forked from that
    // thread, so the whole task directory must be scanned — reading only
    // task/<pid>/children (the main thread) silently misses them.
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return; // process vanished mid-walk; keep what we have
    };
    for task in tasks.flatten() {
        let Ok(tid) = task.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        let Ok(contents) =
            std::fs::read_to_string(format!("/proc/{pid}/task/{tid}/children"))
        else {
            continue;
        };
        for child in contents.split_whitespace() {
            let Ok(child) = child.parse::<i32>() else { continue };
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
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = stat.rfind(')')?;
    let fields: Vec<&str> = stat[close + 1..].split_whitespace().collect();
    fields.get(19).and_then(|field| field.parse().ok())
}

/// Test-only descendant count (all threads), used to assert timeout/abort
/// reaping leaves no orphaned children behind.
#[cfg(all(test, target_os = "linux"))]
pub(crate) fn test_descendant_count() -> usize {
    let mut snapshot = DescendantSnapshot::new();
    collect_descendants(std::process::id() as i32, &mut snapshot);
    snapshot.len()
}

/// SIGTERM then SIGKILL every descendant not present in the baseline (or
/// present with a different start time — the pid was recycled). Best-effort:
/// a process that already exited is a no-op.
#[cfg(target_os = "linux")]
fn kill_new_descendants(baseline: &DescendantSnapshot) {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    let mut current = DescendantSnapshot::new();
    collect_descendants(std::process::id() as i32, &mut current);
    let targets: Vec<i32> = current
        .iter()
        .filter(|(pid, start)| baseline.get(pid) != Some(start))
        .map(|(pid, _)| *pid)
        .collect();
    if targets.is_empty() {
        return;
    }
    for pid in &targets {
        let _ = kill(Pid::from_raw(*pid), Signal::SIGTERM);
    }
    std::thread::sleep(Duration::from_millis(100));
    for pid in &targets {
        let _ = kill(Pid::from_raw(*pid), Signal::SIGKILL);
    }
}
