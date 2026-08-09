//! Opt-in Linux filesystem sandbox for process spawns: the bash tool,
//! process extensions, and orchestration subagent children.
//!
//! The sandbox is **confinement, not isolation**: the command still runs as
//! the same user with the same privileges, but inside fresh Linux namespaces
//! (`unshare`) so that
//!
//! - only the configured allowed paths are visible (bind-mounted read-write,
//!   or read-only when `sandbox.readOnly` is set),
//! - everything else on the host filesystem is denied (tmpfs root + `pivot_root`),
//! - the command gets a private, empty `HOME` and `TMPDIR` under the sandbox
//!   root instead of the host home (codex/claude parity),
//! - the network is off by default (fresh net namespace, loopback only),
//! - `/proc` reflects only the sandbox's own PID namespace.
//!
//! System binaries/libraries under `/usr`, `/bin`, `/sbin`, `/lib`, `/lib64`
//! are bind-mounted read-only so commands can execute; user data under those
//! roots is still hidden. Denied paths are overlaid with an empty tmpfs mount
//! so they are invisible even when nested inside an allowed path.
//!
//! Requires Linux and the `unshare` command (util-linux). Unprivileged users
//! additionally need user namespaces (`kernel.unprivileged_userns_clone` or
//! equivalent); the wrapper maps the caller to root *inside* the namespaces
//! only — there is no privilege escalation on the host.
//!
//! Platform note: on non-Linux targets every sandbox entry point returns the
//! explicit "sandbox unsupported on this platform" error.
//!
//! [`run_in_sandbox`] runs a command to completion (merged output, timeout,
//! abort); [`spawn_piped`] starts a long-lived protocol child (process
//! extensions). Both share the same fail-closed validation and wrapper
//! construction, so every sandboxed spawn honors the same allowed/denied path
//! semantics regardless of the caller.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use pi_agent::AbortSignal;

use crate::settings::SandboxSettings;

/// Resolved sandbox configuration (`settings.sandbox` + the per-call
/// `sandboxed` override). All paths are absolute.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxConfig {
    /// Whether the sandbox is active for the next spawn.
    pub enabled: bool,
    /// Share the host network (false = fresh net namespace, loopback only).
    pub network: bool,
    /// Paths visible inside the sandbox (bind-mounted read-write).
    pub allowed_paths: Vec<PathBuf>,
    /// Paths hidden inside the sandbox even when nested under an allowed path.
    pub denied_paths: Vec<PathBuf>,
    /// Mount the allowed paths read-only inside the sandbox (bind mounts with
    /// `MS_RDONLY`). The sandbox's private HOME/TMPDIR stay writable.
    pub read_only: bool,
}

impl SandboxConfig {
    /// Default configuration for a per-call `sandboxed: true` override when no
    /// `settings.sandbox` block exists: the working directory plus the agent
    /// directory, network off.
    #[must_use]
    pub fn default_for(cwd: &Path, agent_dir: &Path) -> Self {
        Self {
            enabled: true,
            network: false,
            allowed_paths: vec![canonicalize_or(cwd.to_path_buf()), canonicalize_or(agent_dir.to_path_buf())],
            denied_paths: Vec::new(),
            read_only: false,
        }
    }

    /// Rejects configurations that cannot be honored, with actionable errors:
    /// non-Linux platforms, a missing `unshare`, and a working directory that
    /// is not visible inside the sandbox.
    pub fn validate(&self, cwd: &Path) -> Result<()> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (self, cwd);
            bail!(
                "sandbox is only supported on Linux (this build targets {})",
                std::env::consts::OS
            );
        }
        #[cfg(target_os = "linux")]
        {
            if look_path("unshare").is_none() {
                bail!(
                    "sandbox requires the `unshare` command (util-linux). Install util-linux or disable the sandbox (sandbox.enabled=false)."
                );
            }
            let cwd = canonicalize_or(cwd.to_path_buf());
            let visible = self
                .allowed_paths
                .iter()
                .any(|allowed| path_is_within(&cwd, allowed));
            if !visible {
                bail!(
                    "sandbox working directory {} is not inside any sandbox.allowedPaths entry. Add it to sandbox.allowedPaths or disable the sandbox.",
                    cwd.display()
                );
            }
            let hidden = self
                .denied_paths
                .iter()
                .any(|denied| path_is_within(&cwd, denied));
            if hidden {
                bail!(
                    "sandbox working directory {} is inside a sandbox.deniedPaths entry and would be invisible to the command. Remove it from sandbox.deniedPaths or disable the sandbox.",
                    cwd.display()
                );
            }
        }
        #[cfg(target_os = "linux")]
        {
            Ok(())
        }
    }

    /// Builds the wrapper argv that confines `argv` (a program plus its
    /// arguments, passed through unquoted as trailing argv) inside fresh
    /// mount/pid/net namespaces. The wrapper is spawned through the same
    /// `tokio::process::Command` machinery as a plain spawn, so timeouts,
    /// aborts, and process-group kills keep working unchanged.
    ///
    /// The allowed/denied path lists travel as positional arguments between
    /// the script name and the confined command (`allowed_count`,
    /// `denied_count`, allowed paths, denied paths, `--` marker, then
    /// `argv`). Both counts precede every path so the setup script can
    /// validate the entire transport before mounting anything. Positional
    /// parameters reach the setup script byte-for-byte — the shell never
    /// re-parses them — so configured paths containing spaces, quotes, `$`,
    /// backticks, or newlines are used verbatim and can never execute as
    /// shell code. Only paths and the command are passed on the command
    /// line — never secrets.
    #[cfg(target_os = "linux")]
    pub fn wrapper_command(&self, argv: &[String]) -> Result<Vec<String>> {
        let unshare = look_path("unshare").ok_or_else(|| {
            anyhow!(
                "sandbox requires the `unshare` command (util-linux). Install util-linux or disable the sandbox (sandbox.enabled=false)."
            )
        })?;
        let mut wrapped = vec![unshare];
        // Unprivileged mount/pid/net namespaces require a user namespace that
        // maps the caller to root inside the sandbox. Same uid on the host —
        // no privilege escalation. Real root skips the mapping so host-root
        // files stay visible to root-run agents.
        if !is_root_euid() {
            wrapped.push("--user".to_owned());
            wrapped.push("--map-root-user".to_owned());
        }
        wrapped.push("--mount".to_owned());
        wrapped.push("--pid".to_owned());
        wrapped.push("--fork".to_owned());
        wrapped.push("--mount-proc".to_owned());
        if !self.network {
            wrapped.push("--net".to_owned());
        }
        let sh = if path_exists("/bin/sh") {
            "/bin/sh".to_owned()
        } else {
            look_path("sh").ok_or_else(|| anyhow!("sandbox requires /bin/sh"))?
        };
        wrapped.push(sh);
        wrapped.push("-c".to_owned());
        wrapped.push(SANDBOX_SETUP_SCRIPT.to_owned());
        wrapped.push("pi-sandbox".to_owned()); // `$0` for the setup script
        // Path transport, parsed positionally by the setup script: allowed
        // count, denied count, then the literal allowed paths, then the
        // literal denied paths, a `--` separator, then the confined command.
        // Both counts precede any path so the script can validate the whole
        // transport before a single mount happens.
        wrapped.push(self.allowed_paths.len().to_string());
        wrapped.push(self.denied_paths.len().to_string());
        wrapped.extend(self.allowed_paths.iter().map(|path| path.to_string_lossy().into_owned()));
        wrapped.extend(self.denied_paths.iter().map(|path| path.to_string_lossy().into_owned()));
        wrapped.push("--".to_owned());
        wrapped.extend(argv.iter().cloned());
        Ok(wrapped)
    }

    /// Environment entries for the sandboxed spawn: only the read-only
    /// marker. The allowed/denied path lists no longer travel in the
    /// environment — they are positional arguments on the wrapper command
    /// line (see [`SandboxConfig::wrapper_command`]), so no shell
    /// re-evaluation of environment values can touch them. The marker is a
    /// literal `"1"` the setup script compares against; it is never
    /// re-parsed.
    #[must_use]
    pub fn wrapper_env(&self) -> Vec<(String, String)> {
        let mut env = Vec::new();
        if self.read_only {
            env.push(("PI_SANDBOX_READ_ONLY".to_owned(), "1".to_owned()));
        }
        env
    }

    /// Non-Linux fallback: identical signature, explicit unsupported error.
    #[cfg(not(target_os = "linux"))]
    pub fn wrapper_command(&self, argv: &[String]) -> Result<Vec<String>> {
        let _ = (self, argv);
        bail!(
            "sandbox is only supported on Linux (this build targets {})",
            std::env::consts::OS
        );
    }
}

/// Outcome of a sandboxed run: exit status plus timeout/abort flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SandboxRunOutcome {
    /// Process exit code; `None` when the child was killed by a signal, timed
    /// out, or aborted.
    pub exit_code: Option<i32>,
    /// True when the run was cut off by the timeout.
    pub timed_out: bool,
    /// True when the run was cut off by the abort signal.
    pub cancelled: bool,
}

/// How a sandboxed child's stdio is wired.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SandboxStdio {
    /// stdin null; stdout+stderr merged onto one pipe in arrival order
    /// (run-to-completion commands, e.g. the bash tool).
    Merged,
    /// stdin/stdout/stderr all piped (long-lived protocol children, e.g.
    /// process extensions).
    Piped,
}

/// Builds the `tokio::process::Command` for `argv` in `cwd`: the `unshare`
/// wrapper when `config` is present (fail-closed validation first), a plain
/// spawn otherwise. The environment is fully controlled by the caller (`env`
/// is applied after `env_clear`); the sandbox's own environment entries (the
/// read-only marker) are appended last so a host environment can never spoof
/// them, and the allowed/denied path lists travel as wrapper argv, which
/// environment variables cannot influence at all.
fn build_command(
    config: Option<&SandboxConfig>,
    cwd: &Path,
    argv: &[String],
    env: &[(String, String)],
    stdio: SandboxStdio,
) -> Result<tokio::process::Command> {
    let (program, args) = if let Some(config) = config {
        config.validate(cwd)?;
        let wrapped = config.wrapper_command(argv)?;
        (wrapped[0].clone(), wrapped[1..].to_vec())
    } else {
        (
            argv.first().cloned().ok_or_else(|| anyhow!("empty sandbox argv"))?,
            argv[1..].to_vec(),
        )
    };
    let mut command = tokio::process::Command::new(program);
    command.args(args);
    command.current_dir(cwd);
    command.env_clear();
    command.envs(env.iter().cloned());
    match stdio {
        SandboxStdio::Merged => {}
        SandboxStdio::Piped => {
            command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        }
    }
    command.kill_on_drop(true);
    // Run in its own process group; on cancel/timeout kill the whole tree
    // (including `unshare`'s namespaced descendants).
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
    if let Some(config) = config {
        for (key, value) in config.wrapper_env() {
            command.env(key, value);
        }
    }
    Ok(command)
}

/// Runs `argv` to completion — inside the sandbox when `config` is `Some`
/// (fail-closed validation first), as a plain spawn otherwise — streaming
/// merged stdout+stderr in arrival order through `on_chunk`. Enforces the
/// optional timeout and abort; on either, the whole process group is killed
/// so namespaced descendants cannot linger. Spawn and wait I/O errors
/// propagate as `Err`; a nonzero exit is encoded in the outcome.
///
/// This is the shared runner behind the bash tool and every other
/// run-to-completion sandboxed spawn; long-lived protocol children use
/// [`spawn_piped`] instead.
pub async fn run_in_sandbox(
    config: Option<&SandboxConfig>,
    cwd: &Path,
    argv: &[String],
    env: Vec<(String, String)>,
    timeout: Option<Duration>,
    abort: AbortSignal,
    on_chunk: Option<Arc<dyn Fn(&[u8]) + Send + Sync>>,
) -> Result<SandboxRunOutcome> {
    let mut command = build_command(config, cwd, argv, &env, SandboxStdio::Merged)?;
    // Merge stdout+stderr onto one stream so child write order is preserved
    // (pi/Go: same pipe on both fds, shared onData handler). UnixStream::pair
    // gives us ordered interleaving without a new dependency; a cloned end is
    // shut down after the exit grace so a held-open descendant can't hang us.
    #[cfg(unix)]
    let (merged_reader, reader_shutdown) = {
        use std::os::fd::OwnedFd;
        use std::os::unix::net::UnixStream;
        let (reader, writer) = UnixStream::pair().map_err(|e| anyhow!("{}", e))?;
        let writer_err = writer.try_clone().map_err(|e| anyhow!("{}", e))?;
        let reader_shutdown = reader.try_clone().map_err(|e| anyhow!("{}", e))?;
        command.stdout(Stdio::from(OwnedFd::from(writer)));
        command.stderr(Stdio::from(OwnedFd::from(writer_err)));
        (reader, reader_shutdown)
    };
    #[cfg(not(unix))]
    {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
    }
    // Run-to-completion commands are non-interactive. Never let a child race
    // the parent for terminal input; interactive services belong in
    // ProcessManager, where stdin is explicitly piped and controlled through
    // process_send.
    command.stdin(Stdio::null());

    let mut child = command.spawn().map_err(|e| anyhow!("{}", e))?;

    #[cfg(unix)]
    let reader_task = {
        use std::io::Read;
        let mut reader = merged_reader;
        let on_chunk = on_chunk.clone();
        tokio::task::spawn_blocking(move || {
            let mut buf = [0u8; 32 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if let Some(on_chunk) = &on_chunk {
                            on_chunk(&buf[..n]);
                        }
                    }
                }
            }
        })
    };
    #[cfg(not(unix))]
    let reader_task = {
        use tokio::io::AsyncReadExt;
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let on_chunk_stdout = on_chunk.clone();
        let on_chunk_stderr = on_chunk.clone();
        let t1 = tokio::spawn(async move {
            let mut buf = [0u8; 32 * 1024];
            loop {
                match stdout.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if let Some(on_chunk) = &on_chunk_stdout {
                            on_chunk(&buf[..n]);
                        }
                    }
                }
            }
        });
        let t2 = tokio::spawn(async move {
            let mut buf = [0u8; 32 * 1024];
            loop {
                match stderr.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if let Some(on_chunk) = &on_chunk_stderr {
                            on_chunk(&buf[..n]);
                        }
                    }
                }
            }
        });
        tokio::spawn(async move {
            let _ = t1.await;
            let _ = t2.await;
        })
    };

    // Wait with optional timeout and abort; abort wins over timeout.
    enum RunOutcome {
        Exited(std::io::Result<std::process::ExitStatus>),
        TimedOut,
        Aborted,
    }
    let outcome = {
        let wait = child.wait();
        tokio::pin!(wait);
        if let Some(t) = timeout {
            let sleep = tokio::time::sleep(t);
            tokio::pin!(sleep);
            let abort_fut = abort.cancelled();
            tokio::pin!(abort_fut);
            tokio::select! {
                res = &mut wait => RunOutcome::Exited(res),
                _ = &mut sleep => RunOutcome::TimedOut,
                _ = &mut abort_fut => RunOutcome::Aborted,
            }
        } else {
            let abort_fut = abort.cancelled();
            tokio::pin!(abort_fut);
            tokio::select! {
                res = &mut wait => RunOutcome::Exited(res),
                _ = &mut abort_fut => RunOutcome::Aborted,
            }
        }
    };

    // On timeout/abort kill the process group (best-effort child kill); the
    // reader finishes when the stream closes or we shut it down below. The
    // child runs in its own process group (process_group(0) above), so the
    // killpg reaps the whole tree — including `unshare`'s namespaced
    // descendants — and a timed-out sandboxed command cannot linger. The pid
    // must be captured before `kill()` because tokio fuses the child there.
    if matches!(outcome, RunOutcome::TimedOut | RunOutcome::Aborted) {
        #[cfg(unix)]
        let child_pid = child.id();
        let _ = child.kill().await;
        #[cfg(unix)]
        if let Some(pid) = child_pid {
            use nix::sys::signal::{Signal, killpg};
            use nix::unistd::Pid;
            let _ = killpg(Pid::from_raw(pid as i32), Signal::SIGKILL);
        }
    }

    // Drain remaining merged output. A held-open stream from a detached
    // descendant can't hang us past the idle grace — shut the local end down
    // so the blocking reader unblocks (Go closes the pipe reader the same way).
    {
        tokio::pin!(reader_task);
        let finished = tokio::select! {
            r = &mut reader_task => { let _ = r; true }
            _ = tokio::time::sleep(EXIT_STDIO_GRACE) => false,
        };
        if !finished {
            #[cfg(unix)]
            {
                use std::net::Shutdown;
                let _ = reader_shutdown.shutdown(Shutdown::Both);
            }
            let _ = reader_task.await;
        }
    }
    // Reap the child so it doesn't linger.
    let wait_status = child.wait().await;

    // Abort wins over timeout; a wait I/O error only surfaces when neither fired.
    let (exit_code, timed_out, wait_err, outcome_aborted) = match outcome {
        RunOutcome::Exited(Ok(s)) => (s.code(), false, None, false),
        RunOutcome::Exited(Err(e)) => (None, false, Some(e), false),
        RunOutcome::TimedOut => (None, true, None, false),
        RunOutcome::Aborted => (None, false, None, true),
    };
    let cancelled = abort.is_aborted() || outcome_aborted;

    if let Some(err) = wait_err {
        if !cancelled && !timed_out {
            // The reap may also surface it; prefer the original wait error.
            if let Err(e) = wait_status {
                return Err(anyhow!("{}", e));
            }
            return Err(anyhow!("{}", err));
        }
    }

    Ok(SandboxRunOutcome {
        exit_code,
        timed_out,
        cancelled,
    })
}

/// Spawns `argv` inside the sandbox with piped stdin/stdout/stderr (when
/// `config` is `Some`, fail-closed validation first) and returns the child.
/// Intended for long-lived protocol children such as process extensions whose
/// lifetime is owned by the caller — run-to-completion commands use
/// [`run_in_sandbox`]. The caller keeps the process-group semantics: killing
/// the returned child's group reaps `unshare`'s namespaced descendants.
pub fn spawn_piped(
    config: Option<&SandboxConfig>,
    cwd: &Path,
    argv: &[String],
    env: impl IntoIterator<Item = (String, String)>,
) -> Result<tokio::process::Child> {
    let env = env.into_iter().collect::<Vec<_>>();
    let mut command = build_command(config, cwd, argv, &env, SandboxStdio::Piped)?;
    command.spawn().map_err(|e| anyhow!("{}", e))
}

/// Idle grace kept reading merged stdout/stderr after the process exits so
/// output a detached descendant writes past exit is captured.
const EXIT_STDIO_GRACE: Duration = Duration::from_millis(200);

/// Resolves `settings.sandbox` into a [`SandboxConfig`], or `None` when the
/// setting is absent. Relative allowed/denied paths resolve from `cwd`; when
/// `allowedPaths` is absent or empty the default is `[cwd, agent_dir]`.
#[must_use]
pub fn resolve(settings: Option<&SandboxSettings>, cwd: &Path, agent_dir: &Path) -> Option<SandboxConfig> {
    let settings = settings?;
    let mut allowed_paths = Vec::new();
    if let Some(paths) = &settings.allowed_paths {
        for raw in paths {
            if raw.trim().is_empty() {
                continue;
            }
            allowed_paths.push(canonicalize_or(absolutize(cwd, raw)));
        }
    }
    if allowed_paths.is_empty() {
        allowed_paths.push(canonicalize_or(cwd.to_path_buf()));
        allowed_paths.push(canonicalize_or(agent_dir.to_path_buf()));
    }
    let mut denied_paths = Vec::new();
    if let Some(paths) = &settings.denied_paths {
        for raw in paths {
            if raw.trim().is_empty() {
                continue;
            }
            denied_paths.push(canonicalize_or(absolutize(cwd, raw)));
        }
    }
    Some(SandboxConfig {
        enabled: settings.enabled.unwrap_or(false),
        network: settings.network.unwrap_or(false),
        allowed_paths,
        denied_paths,
        read_only: settings.read_only.unwrap_or(false),
    })
}

fn absolutize(cwd: &Path, raw: &str) -> PathBuf {
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

fn canonicalize_or(path: PathBuf) -> PathBuf {
    std::fs::canonicalize(&path).unwrap_or(path)
}

/// True when `path` is at or below `allowed` (the bind-mounted subtree). The
/// working directory must live inside an allowed path, otherwise its inode
/// would be detached from the sandbox root after `pivot_root`.
fn path_is_within(path: &Path, allowed: &Path) -> bool {
    let allowed = canonicalize_or(allowed.to_path_buf());
    path.starts_with(&allowed)
}

/// True when the effective uid is root. Read from `/proc/self` ownership to
/// avoid unsafe libc calls (the workspace forbids `unsafe`); when `/proc` is
/// unavailable we conservatively assume a non-root caller (the user-namespace
/// mapping also works for root, it only narrows visibility of other users'
/// files).
#[cfg(target_os = "linux")]
fn is_root_euid() -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self")
        .map(|meta| meta.uid() == 0)
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn path_exists(path: &str) -> bool {
    std::fs::metadata(path).is_ok()
}

#[cfg(target_os = "linux")]
fn look_path(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

/// Mount setup executed as PID 1 of the fresh mount/pid/net namespaces (root
/// via the user namespace mapping unless the caller is real root). It builds a
/// tmpfs root, bind-mounts allowed paths and read-only system dirs, overlays
/// denied paths, and pivots so the host root is unreachable. Fails closed:
/// every step exits with a distinct code and an actionable message on `stderr`.
///
/// Path transport: the allowed/denied path lists arrive as positional
/// arguments (`allowed_count`, `denied_count`, allowed paths, denied paths,
/// `--`, then the confined command). Both counts precede every path so the
/// whole transport can be validated before a single mount. Positional
/// parameters are never re-parsed by the shell, so hostile path values —
/// `$(...)`, backticks, spaces, quotes, newlines — stay literal. Malformed
/// input exits 70 before the confined command runs.
///
/// Order matters: `/tmp` is mounted before allowed paths so an allowed path
/// under `/tmp` (the common case for temp working directories) lands inside
/// the final private tmpfs and its parent chain survives `pivot_root`; system
/// dirs are bound read-only before allowed paths so an allowed path nested in
/// a system dir still wins; denied overlays come last so they always win.
#[cfg(target_os = "linux")]
const SANDBOX_SETUP_SCRIPT: &str = r#"#!/bin/sh
set -eu

# Path transport: two counts, the allowed paths, the denied paths, a `--`
# separator, then the confined command — all as positional arguments. argv is
# byte-exact; the shell never re-parses it, so configured paths containing
# spaces, quotes, `$`, backticks, or newlines are used verbatim and can never
# execute as shell code. Counts are validated before anything is mounted and
# the separator/command presence is checked before exec: any malformed
# transport exits 70 without running the confined command.
[ "$#" -ge 1 ] || { echo "sandbox: missing allowed count (malformed transport)" >&2; exit 70; }
ALLOWED_COUNT=$1
shift
[ "$#" -ge 1 ] || { echo "sandbox: missing denied count (malformed transport)" >&2; exit 70; }
DENIED_COUNT=$1
shift
# Counts must be non-negative decimal integers without leading zeros (dash
# treats `$((08))` as a bad octal and would abort with an opaque error).
# Two-stage check: reject anything with a non-digit, then anything that
# starts with `0` followed by more digits (`0` itself is valid).
case "$ALLOWED_COUNT" in
  ""|*[!0-9]*)
    echo "sandbox: invalid allowed count '$ALLOWED_COUNT' (malformed transport)" >&2
    exit 70;;
esac
case "$ALLOWED_COUNT" in
  0[0-9]*)
    echo "sandbox: invalid allowed count '$ALLOWED_COUNT' (malformed transport)" >&2
    exit 70;;
esac
case "$DENIED_COUNT" in
  ""|*[!0-9]*)
    echo "sandbox: invalid denied count '$DENIED_COUNT' (malformed transport)" >&2
    exit 70;;
esac
case "$DENIED_COUNT" in
  0[0-9]*)
    echo "sandbox: invalid denied count '$DENIED_COUNT' (malformed transport)" >&2
    exit 70;;
esac
# Remaining args = allowed paths + denied paths + `--` + at least one command
# arg, so the counts plus 2 must fit; truncated transports fail here.
[ $((ALLOWED_COUNT + DENIED_COUNT + 2)) -le "$#" ] || { echo "sandbox: path counts $ALLOWED_COUNT/$DENIED_COUNT exceed the $# remaining args (malformed transport)" >&2; exit 70; }

# The process starts in the host working directory. Its absolute path is
# preserved (allowed paths are bind-mounted at their host paths), so we can
# `cd` back into it after pivot_root: a cwd inode from the detached old root
# would bypass every sandbox mount on relative lookups.
CWD=$(pwd)

mount --make-rprivate / || { echo "sandbox: mount --make-rprivate / failed" >&2; exit 90; }

ROOT=/tmp/pi-sandbox.$$
mkdir -p "$ROOT"
mount -t tmpfs -o mode=700 tmpfs "$ROOT" || { echo "sandbox: tmpfs root mount failed" >&2; exit 91; }

mkdir -p "$ROOT/proc"
mount -t proc proc "$ROOT/proc" || { echo "sandbox: proc mount failed" >&2; exit 92; }

# Minimal device nodes (bind-mounted: mknod is blocked on tmpfs in user namespaces).
mkdir -p "$ROOT/dev"
for dev in null zero full random urandom tty; do
  rm -f "$ROOT/dev/$dev"
  : > "$ROOT/dev/$dev" 2>/dev/null || true
  mount --bind "/dev/$dev" "$ROOT/dev/$dev" 2>/dev/null || echo "sandbox: /dev/$dev unavailable" >&2
done

# Private tmp (before allowed paths: temp working directories are common, and
# their parent chain must live in the final /tmp mount to survive pivot_root).
mkdir -p "$ROOT/tmp"
mount -t tmpfs -o mode=1777 tmpfs "$ROOT/tmp" || { echo "sandbox: tmp mount failed" >&2; exit 96; }

# System binaries/libs visible read-only so commands can execute; user data
# under these roots stays hidden. Bound before allowed paths so an allowed
# path nested in a system dir still wins.
for d in /usr /bin /sbin /lib /lib64; do
  [ -e "$d" ] || continue
  mkdir -p "$ROOT$d"
  mount --bind "$d" "$ROOT$d" 2>/dev/null || { echo "sandbox: bind $d failed" >&2; exit 94; }
  mount -o remount,bind,ro "$ROOT$d" 2>/dev/null || true
done

# Allowed paths (read-write unless PI_SANDBOX_READ_ONLY=1): the agent's
# working data. Each `$1` is a literal path — no shell re-parsing.
i=0
while [ "$i" -lt "$ALLOWED_COUNT" ]; do
  p=$1
  shift
  i=$((i + 1))
  [ -n "$p" ] && [ -e "$p" ] || continue
  if [ -d "$p" ]; then
    mkdir -p "$ROOT$p"
  else
    mkdir -p "$(dirname "$ROOT$p")"
    : > "$ROOT$p"
  fi
  mount --bind "$p" "$ROOT$p" || { echo "sandbox: bind $p failed" >&2; exit 93; }
  if [ "${PI_SANDBOX_READ_ONLY:-0}" = "1" ]; then
    mount -o remount,bind,ro "$ROOT$p" || { echo "sandbox: remount $p read-only failed" >&2; exit 88; }
  fi
done

# Denied paths: empty overlay (wins over allowed and system binds). Directories
# become empty tmpfs mounts; files are hidden behind an empty placeholder file.
i=0
while [ "$i" -lt "$DENIED_COUNT" ]; do
  p=$1
  shift
  i=$((i + 1))
  [ -n "$p" ] && [ -e "$p" ] || continue
  if [ -d "$p" ]; then
    mkdir -p "$ROOT$p"
    mount -t tmpfs -o mode=000 tmpfs "$ROOT$p" || { echo "sandbox: deny overlay $p failed" >&2; exit 95; }
  else
    mkdir -p "$(dirname "$ROOT$p")"
    : > "$ROOT/.pi-deny-file" 2>/dev/null || true
    mount --bind "$ROOT/.pi-deny-file" "$ROOT$p" || { echo "sandbox: deny overlay $p failed" >&2; exit 95; }
  fi
done

# The `--` separator ends the path transport; the confined command follows and
# is passed through untouched. Both are required (see the count check above).
[ "${1:-}" = "--" ] || { echo "sandbox: missing '--' separator (malformed transport)" >&2; exit 70; }
shift
[ "$#" -gt 0 ] || { echo "sandbox: no command to run (malformed transport)" >&2; exit 70; }

# Fresh HOME + TMPDIR inside the sandbox (codex/claude parity): the host home
# is normally not an allowed path, so tools that cache to $HOME or write temp
# files to $TMPDIR get a private, empty, writable location under the sandbox
# root instead of failing or leaking into the host home. Created before
# pivot_root so they land in the final sandbox root; the private /tmp tmpfs is
# already mounted above (empty, mode 1777).
mkdir -p "$ROOT/root"
export HOME=/root
export TMPDIR=/tmp

# Detach the host root: the sandbox sees only the tmpfs root and its mounts.
mkdir -p "$ROOT/.oldroot"
pivot_root "$ROOT" "$ROOT/.oldroot" || { echo "sandbox: pivot_root failed" >&2; exit 97; }
umount -l /.oldroot || { echo "sandbox: failed to detach the old root (refusing to run)" >&2; exit 98; }

# Re-enter the working directory through the new root's bind mount so the
# confined command's cwd is the sandboxed copy (see CWD capture above).
cd "$CWD" || { echo "sandbox: working directory $CWD is not visible in the sandbox (is it denied?)" >&2; exit 99; }

exec "$@"
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Map, Value};

    fn settings(
        enabled: Option<bool>,
        network: Option<bool>,
        allowed: Option<Vec<&str>>,
        denied: Option<Vec<&str>>,
    ) -> SandboxSettings {
        SandboxSettings {
            enabled,
            network,
            read_only: None,
            allowed_paths: allowed.map(|paths| paths.into_iter().map(str::to_owned).collect()),
            denied_paths: denied.map(|paths| paths.into_iter().map(str::to_owned).collect()),
            extra: Map::new(),
        }
    }

    fn empty() -> SandboxSettings {
        SandboxSettings {
            enabled: None,
            network: None,
            read_only: None,
            allowed_paths: None,
            denied_paths: None,
            extra: Map::new(),
        }
    }

    #[test]
    fn resolve_absent_setting_returns_none() {
        assert_eq!(resolve(None, Path::new("/w"), Path::new("/a")), None);
    }

    #[test]
    fn resolve_defaults_allowed_paths_to_cwd_and_agent_dir() {
        let config = resolve(Some(&empty()), Path::new("/tmp/w"), Path::new("/tmp/a")).expect("config");
        assert!(!config.enabled);
        assert!(!config.network);
        assert_eq!(config.allowed_paths.len(), 2);
        assert!(config.allowed_paths.iter().any(|p| p == Path::new("/tmp/w")));
        assert!(config.allowed_paths.iter().any(|p| p == Path::new("/tmp/a")));
        assert!(config.denied_paths.is_empty());
    }

    #[test]
    fn resolve_applies_enabled_network_and_denied_paths() {
        let config = resolve(
            Some(&settings(
                Some(true),
                Some(true),
                Some(vec!["/tmp/w", "relative"]),
                Some(vec!["/tmp/w/secret"]),
            )),
            Path::new("/tmp/w"),
            Path::new("/tmp/a"),
        )
        .expect("config");
        assert!(config.enabled);
        assert!(config.network);
        assert!(config.allowed_paths.iter().any(|p| p == Path::new("/tmp/w")));
        assert!(
            config.allowed_paths.iter().any(|p| p == Path::new("/tmp/w/relative")),
            "relative allowed paths resolve from cwd"
        );
        assert!(config.denied_paths.iter().any(|p| p == Path::new("/tmp/w/secret")));
    }

    #[test]
    fn resolve_empty_allowed_list_falls_back_to_defaults() {
        let config = resolve(
            Some(&settings(Some(true), None, Some(vec![]), None)),
            Path::new("/tmp/w"),
            Path::new("/tmp/a"),
        )
        .expect("config");
        assert_eq!(config.allowed_paths.len(), 2);
    }

    #[test]
    fn resolve_maps_read_only_flag() {
        let mut with_read_only = settings(Some(true), None, None, None);
        with_read_only.read_only = Some(true);
        let config = resolve(Some(&with_read_only), Path::new("/tmp/w"), Path::new("/tmp/a"))
            .expect("config");
        assert!(config.read_only, "readOnly=true must flow into the config");

        let writable = resolve(Some(&settings(Some(true), None, None, None)), Path::new("/tmp/w"), Path::new("/tmp/a"))
            .expect("config");
        assert!(!writable.read_only, "readOnly defaults to false");
    }

    #[test]
    fn wrapper_env_carries_only_the_read_only_marker() {
        let config = SandboxConfig {
            enabled: true,
            network: false,
            allowed_paths: vec![PathBuf::from("/tmp/w")],
            denied_paths: Vec::new(),
            read_only: true,
        };
        let env = config.wrapper_env();
        assert_eq!(
            env,
            vec![("PI_SANDBOX_READ_ONLY".to_owned(), "1".to_owned())],
            "a read-only config carries exactly the read-only marker"
        );
        // A writable config carries no environment at all: the path lists
        // travel as positional args on the wrapper command line, not in the
        // environment (so no env value is ever re-parsed by the shell).
        let writable = SandboxConfig {
            enabled: true,
            network: false,
            allowed_paths: vec![PathBuf::from("/tmp/w")],
            denied_paths: vec![PathBuf::from("/tmp/w/secret")],
            read_only: false,
        };
        assert!(
            writable.wrapper_env().is_empty(),
            "a writable config must not set any PI_SANDBOX_* variables"
        );
    }

    #[test]
    fn default_for_enables_sandbox_with_cwd_and_agent_dir() {
        let config = SandboxConfig::default_for(Path::new("/tmp/w"), Path::new("/tmp/a"));
        assert!(config.enabled);
        assert!(!config.network);
        assert_eq!(config.allowed_paths, vec![
            PathBuf::from("/tmp/w"),
            PathBuf::from("/tmp/a")
        ]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn wrapper_command_carries_paths_as_positional_args() {
        let config = SandboxConfig {
            enabled: true,
            network: false,
            allowed_paths: vec![PathBuf::from("/tmp/w"), PathBuf::from("/tmp/a")],
            denied_paths: vec![PathBuf::from("/tmp/w/secret")],
            read_only: false,
        };
        let argv = config
            .wrapper_command(&["/bin/echo".to_owned(), "hi".to_owned()])
            .expect("wrapper argv");
        let i = argv
            .iter()
            .position(|arg| arg == "pi-sandbox")
            .expect("script name");
        assert_eq!(argv[i + 1], "2", "allowed count precedes both path lists");
        assert_eq!(argv[i + 2], "1", "denied count precedes both path lists");
        assert_eq!(argv[i + 3], "/tmp/w");
        assert_eq!(argv[i + 4], "/tmp/a");
        assert_eq!(argv[i + 5], "/tmp/w/secret");
        assert_eq!(
            argv[i + 6], "--",
            "a `--` marker separates the path transport from the command"
        );
        assert_eq!(argv[i + 7], "/bin/echo");
        assert_eq!(argv[i + 8], "hi");
        assert_eq!(argv.len(), i + 9);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn wrapper_command_shape_network_off() {
        let config = SandboxConfig {
            enabled: true,
            network: false,
            allowed_paths: vec![PathBuf::from("/tmp/w")],
            denied_paths: Vec::new(),
            read_only: false,
        };
        let argv = config
            .wrapper_command(&["/bin/bash".to_owned(), "-c".to_owned(), "echo hi".to_owned()])
            .expect("wrapper argv");
        let position = |flag: &str| argv.iter().position(|arg| arg == flag).expect(flag);
        // Namespace flags, in order, with --net present (network off by default).
        assert!(
            argv[0].ends_with("/unshare"),
            "wrapper argv[0] is the resolved unshare path: {argv:?}"
        );
        assert!(position("--user") < position("--mount"));
        assert!(position("--mount") < position("--pid"));
        assert!(position("--pid") < position("--fork"));
        assert!(position("--fork") < position("--mount-proc"));
        assert!(position("--mount-proc") < position("--net"));
        // The inner command lands as trailing argv untouched (no shell
        // quoting), behind the path transport: allowed count, denied count,
        // allowed paths, denied paths, `--`, then the command.
        assert_eq!(argv[position("--net") + 1], "/bin/sh");
        let tail: Vec<&String> = argv[position("--net") + 2..].iter().collect();
        assert_eq!(tail[0], "-c");
        assert_eq!(tail[1], &SANDBOX_SETUP_SCRIPT.to_owned());
        assert_eq!(tail[2], "pi-sandbox");
        assert_eq!(tail[3], "1"); // allowed count
        assert_eq!(tail[4], "0"); // denied count
        assert_eq!(tail[5], "/tmp/w"); // allowed path
        assert_eq!(tail[6], "--"); // transport separator
        assert_eq!(tail[7], "/bin/bash");
        assert_eq!(tail[8], "-c");
        assert_eq!(tail[9], "echo hi");
        assert_eq!(argv.len(), position("--net") + 12);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn wrapper_command_omits_net_when_network_shared() {
        let config = SandboxConfig {
            enabled: true,
            network: true,
            allowed_paths: vec![PathBuf::from("/tmp/w")],
            denied_paths: Vec::new(),
            read_only: false,
        };
        let argv = config
            .wrapper_command(&["/bin/bash".to_owned(), "-c".to_owned(), "echo hi".to_owned()])
            .expect("wrapper argv");
        assert!(
            !argv.iter().any(|arg| arg == "--net"),
            "network=true must share the host network: {argv:?}"
        );
        // Inner command is still the trailing argv.
        assert_eq!(argv[argv.len() - 3], "/bin/bash");
        assert_eq!(argv[argv.len() - 2], "-c");
        assert_eq!(argv[argv.len() - 1], "echo hi");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn wrapper_command_multi_word_shell_args_preserved() {
        let config = SandboxConfig {
            enabled: true,
            network: false,
            allowed_paths: vec![PathBuf::from("/tmp/w")],
            denied_paths: Vec::new(),
            read_only: false,
        };
        let argv = config
            .wrapper_command(&[
                "/bin/bash".to_owned(),
                "-lc".to_owned(),
                "echo 'a b'".to_owned(),
            ])
            .expect("wrapper argv");
        assert_eq!(argv[argv.len() - 3], "/bin/bash");
        assert_eq!(argv[argv.len() - 2], "-lc");
        assert_eq!(argv[argv.len() - 1], "echo 'a b'");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn wrapper_command_transports_hostile_paths_literally() {
        // Paths configured with shell metacharacters must arrive as single
        // literal argv elements: the setup script consumes them positionally,
        // so `$(...)`/backticks inside a path value can never be executed.
        let hostile_allowed = "$(touch /tmp/pwned) '`touch /tmp/pwned`'";
        let hostile_denied = "`touch /tmp/pwned` \"$HOME\"\nwith\nnewlines";
        let config = SandboxConfig {
            enabled: true,
            network: false,
            allowed_paths: vec![PathBuf::from(hostile_allowed)],
            denied_paths: vec![PathBuf::from(hostile_denied)],
            read_only: false,
        };
        let argv = config
            .wrapper_command(&["/bin/true".to_owned()])
            .expect("wrapper argv");
        let i = argv
            .iter()
            .position(|arg| arg == "pi-sandbox")
            .expect("script name");
        assert_eq!(argv[i + 1], "1");
        assert_eq!(argv[i + 2], "1");
        assert_eq!(
            argv[i + 3], hostile_allowed,
            "hostile allowed path must arrive as one literal arg: {argv:?}"
        );
        assert_eq!(
            argv[i + 4], hostile_denied,
            "hostile denied path must arrive as one literal arg: {argv:?}"
        );
        assert_eq!(argv[i + 5], "--");
        assert_eq!(argv[i + 6], "/bin/true");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn wrapper_command_literal_transport_is_mutation_resistant() {
        // Every shell metacharacter class a hostile path value can carry —
        // spaces, single/double quotes, newlines, `$(...)`, backticks, and
        // operators — must arrive as ONE byte-exact argv element, exactly
        // once. These assertions are the mutation target for "interpolate
        // shell syntax": re-quoting a value, joining values into one string,
        // or splitting them re-parses them, which changes an element's
        // content, the element count, or the layout and fails here.
        let hostile_allowed = "sp ace 'sq' \"dq\"\n$(touch /tmp/pwned) `touch /tmp/pwned` ; && | *";
        let hostile_denied = "back`tick` $() \"dq\" 'sq'\nsecond";
        let config = SandboxConfig {
            enabled: true,
            network: false,
            allowed_paths: vec![PathBuf::from(hostile_allowed)],
            denied_paths: vec![PathBuf::from(hostile_denied)],
            read_only: false,
        };
        let argv = config
            .wrapper_command(&["/bin/echo".to_owned(), "lit".to_owned()])
            .expect("wrapper argv");
        let i = argv
            .iter()
            .position(|arg| arg == "pi-sandbox")
            .expect("script name");
        assert_eq!(argv[i + 1], "1", "allowed count");
        assert_eq!(argv[i + 2], "1", "denied count");
        assert_eq!(
            argv[i + 3], hostile_allowed,
            "hostile allowed path must arrive byte-exact as one element: {argv:?}"
        );
        assert_eq!(
            argv[i + 4], hostile_denied,
            "hostile denied path must arrive byte-exact as one element: {argv:?}"
        );
        assert_eq!(argv[i + 5], "--");
        assert_eq!(argv[i + 6], "/bin/echo");
        assert_eq!(argv[i + 7], "lit");
        assert_eq!(
            argv.len(),
            i + 8,
            "shell interpolation would add or drop elements; layout must stay exact: {argv:?}"
        );
        // Each hostile value appears exactly once in the whole argv — a
        // re-parse would split it into fragments (or drop it).
        assert_eq!(
            argv.iter().filter(|arg| *arg == &hostile_allowed).count(),
            1,
            "hostile allowed path must appear exactly once"
        );
        assert_eq!(
            argv.iter().filter(|arg| *arg == &hostile_denied).count(),
            1,
            "hostile denied path must appear exactly once"
        );
        // No path element may be a shell-quoted re-wrapping (`'…'`/`"…"`):
        // the setup script consumes values positionally and must never see a
        // wrapper that a later `eval`/re-parse could strip.
        for arg in &argv[i + 3..=i + 4] {
            assert!(
                !(arg.starts_with('\'') && arg.ends_with('\''))
                    && !(arg.starts_with('"') && arg.ends_with('"')),
                "path values must never be shell-quoted wrappers: {arg:?}"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn setup_script_never_re_evaluates_path_values() {
        // The path transport must never eval/indirectly re-parse configured
        // values: `eval` would turn `$(...)`/backticks inside a path into
        // command execution. Guard the script text itself (static, always
        // runs; the ignored live smoke test proves non-execution end to end
        // on kernels with unprivileged user namespaces).
        assert!(
            !SANDBOX_SETUP_SCRIPT.contains("eval"),
            "sandbox setup script must not contain eval"
        );
        assert!(
            !SANDBOX_SETUP_SCRIPT.contains("PI_SANDBOX_ALLOWED_")
                && !SANDBOX_SETUP_SCRIPT.contains("PI_SANDBOX_DENIED_"),
            "sandbox setup script must not read indexed path env vars"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn setup_script_rejects_malformed_transport_before_mounting() {
        // Malformed transports exit 70 before any mount runs (count
        // validation is the first thing the script does), so this is safe to
        // exercise directly even when running as root.
        let run = |args: &[&str]| {
            std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(SANDBOX_SETUP_SCRIPT)
                .arg(args[0])
                .args(&args[1..])
                .status()
                .expect("run setup script")
                .code()
        };
        // Layout: <allowed_count> <denied_count> <allowed...> <denied...> -- <command...>
        assert_eq!(run(&["pi-sandbox"]), Some(70), "missing counts");
        assert_eq!(run(&["pi-sandbox", "1"]), Some(70), "missing denied count");
        assert_eq!(
            run(&["pi-sandbox", "x", "0", "--", "/bin/true"]),
            Some(70),
            "non-numeric allowed count"
        );
        assert_eq!(
            run(&["pi-sandbox", "0", "-1", "--", "/bin/true"]),
            Some(70),
            "negative denied count"
        );
        assert_eq!(
            run(&["pi-sandbox", "2", "0", "--", "/bin/true"]),
            Some(70),
            "allowed count exceeds the supplied paths"
        );
        assert_eq!(
            run(&["pi-sandbox", "0", "0", "/bin/true"]),
            Some(70),
            "missing -- separator and command"
        );
        assert_eq!(run(&["pi-sandbox", "0", "0", "--"]), Some(70), "missing command");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn setup_script_fails_closed_when_mounts_are_denied() {
        // The script's first mount (`mount --make-rprivate /`) requires mount
        // privileges. In an environment without them — exactly what the
        // wrapper hits when unprivileged user namespaces are unavailable —
        // the script must fail closed with its distinct code 90 BEFORE the
        // confined command runs, and hostile path values must have passed
        // transport validation untouched (never executed). Under root (or a
        // mount-capable user namespace) the script would proceed to
        // `pivot_root`, which must never run inside a unit test; those hosts
        // run the live namespace tests instead.
        let mount_denied = std::process::Command::new("mount")
            .args(["--make-rprivate", "/"])
            .status();
        if !matches!(mount_denied, Ok(status) if !status.success()) {
            eprintln!(
                "setup-script fail-closed test: skipped (mount privileges available here; the live namespace tests cover the confinement contract)"
            );
            return;
        }
        let sentinel = std::env::temp_dir().join(format!("pi-sandbox-eval-{}", std::process::id()));
        let _ = std::fs::remove_file(&sentinel);
        let sentinel_display = sentinel.display().to_string();
        let hostile_allowed = format!(
            "$(touch {s}) 'quoted' \"double\"\nnewline `touch {s}`",
            s = sentinel_display
        );
        let run = |args: &[&str]| {
            std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(SANDBOX_SETUP_SCRIPT)
                .arg(args[0])
                .args(&args[1..])
                .output()
                .expect("run setup script")
        };
        let out = run(&["pi-sandbox", "1", "0", &hostile_allowed, "--", "/bin/true"]);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            out.status.code(),
            Some(90),
            "mount denial must fail closed with code 90, never run the confined command: {stderr}"
        );
        assert!(
            !stderr.contains("malformed transport"),
            "hostile path values must pass transport validation untouched: {stderr}"
        );
        assert!(
            !sentinel.exists(),
            "hostile path values must never be executed: {}",
            sentinel.display()
        );
        let _ = std::fs::remove_file(&sentinel);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn setup_script_treats_literal_double_dash_path_as_transport_data() {
        // A configured path whose literal value is `--` must be consumed by
        // the positional loop as a path and must not confuse the `--`
        // separator check (same mount-denied precondition as above: exit 90,
        // never the malformed-transport exit 70).
        let mount_denied = std::process::Command::new("mount")
            .args(["--make-rprivate", "/"])
            .status();
        if !matches!(mount_denied, Ok(status) if !status.success()) {
            eprintln!(
                "setup-script `--`-path test: skipped (mount privileges available here; live tests cover)"
            );
            return;
        }
        let run = |args: &[&str]| {
            std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(SANDBOX_SETUP_SCRIPT)
                .arg(args[0])
                .args(&args[1..])
                .status()
                .expect("run setup script")
                .code()
        };
        // Layout: <allowed=1> <denied=0> <allowed path "--"> <separator "--">
        // <command>. Well-formed transport -> 90 (mount denied), never 70.
        assert_eq!(
            run(&["pi-sandbox", "1", "0", "--", "--", "/bin/true"]),
            Some(90),
            "a literal `--` path value is transport data, not the separator"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn validate_rejects_cwd_outside_allowed_paths() {
        let config = SandboxConfig {
            enabled: true,
            network: false,
            allowed_paths: vec![PathBuf::from("/tmp/w")],
            denied_paths: Vec::new(),
            read_only: false,
        };
        let error = config
            .validate(Path::new("/elsewhere"))
            .expect_err("cwd outside allowed paths must fail");
        assert!(error.to_string().contains("sandbox.allowedPaths"), "got: {error}");
        // Sibling denial: a cwd beside the allowed path (here: a path that
        // merely shares the `/tmp/w` prefix as components) is just as
        // invisible inside the sandbox.
        let error = config
            .validate(Path::new("/tmp/w2"))
            .expect_err("a sibling cwd must fail");
        assert!(error.to_string().contains("sandbox.allowedPaths"), "got: {error}");
        // A descendant of the allowed path is the only shape that validates.
        config.validate(Path::new("/tmp/w/sub")).expect("descendant cwd is fine");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn validate_rejects_cwd_inside_denied_paths() {
        let config = SandboxConfig {
            enabled: true,
            network: false,
            allowed_paths: vec![PathBuf::from("/tmp/w")],
            denied_paths: vec![PathBuf::from("/tmp/w/secret")],
            read_only: false,
        };
        let error = config
            .validate(Path::new("/tmp/w/secret/deep"))
            .expect_err("cwd inside denied paths must fail");
        assert!(error.to_string().contains("sandbox.deniedPaths"), "got: {error}");
        // A sibling of the denied path stays fine.
        config.validate(Path::new("/tmp/w/other")).expect("sibling cwd is fine");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn validate_accepts_cwd_inside_allowed_path() {
        let config = SandboxConfig {
            enabled: true,
            network: false,
            allowed_paths: vec![PathBuf::from("/tmp/w")],
            denied_paths: Vec::new(),
            read_only: false,
        };
        // Canonicalized: /tmp may be a symlink on some systems, so probe the
        // real path first and rebuild the config against it.
        let real = std::fs::canonicalize("/tmp").unwrap_or_else(|_| PathBuf::from("/tmp"));
        let config = SandboxConfig {
            enabled: true,
            network: false,
            allowed_paths: vec![real.clone()],
            denied_paths: Vec::new(),
            read_only: false,
        };
        config.validate(&real.join("subdir")).expect("cwd inside allowed path is fine");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn build_command_fails_closed_when_config_does_not_validate() {
        // The spawn path must refuse to build the wrapper before anything
        // spawns when validation fails (here: cwd outside the allowed paths)
        // — on any kernel, no user namespaces involved.
        let config = SandboxConfig {
            enabled: true,
            network: false,
            allowed_paths: vec![PathBuf::from("/tmp/w")],
            denied_paths: Vec::new(),
            read_only: false,
        };
        let error = build_command(
            Some(&config),
            Path::new("/elsewhere"),
            &["/bin/true".to_owned()],
            &[],
            SandboxStdio::Merged,
        )
        .expect_err("a config whose cwd is invisible must fail closed");
        assert!(error.to_string().contains("sandbox.allowedPaths"), "got: {error}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn build_command_wraps_with_unshare_exactly_as_production_executes() {
        // With a valid config the produced Command is the exact unshare
        // wrapper plan (program + argv) that production spawns. When unshare
        // is absent from PATH the same call is the fail-closed error instead.
        let real = std::fs::canonicalize("/tmp").unwrap_or_else(|_| PathBuf::from("/tmp"));
        let config = SandboxConfig {
            enabled: true,
            network: false,
            allowed_paths: vec![real.clone()],
            denied_paths: Vec::new(),
            read_only: false,
        };
        let build = || {
            build_command(
                Some(&config),
                &real.join("subdir"),
                &["/bin/true".to_owned(), "hi".to_owned()],
                &[],
                SandboxStdio::Merged,
            )
        };
        let command = match build() {
            Ok(command) => command,
            Err(error) => {
                assert!(
                    error.to_string().contains("unshare"),
                    "the only legitimate build failure is the missing-unshare fail-closed error: {error}"
                );
                return;
            }
        };
        let program = command.as_std().get_program().to_string_lossy().into_owned();
        assert!(
            program.ends_with("/unshare") || program == "unshare",
            "wrapper program must be the resolved unshare: {program}"
        );
        let args: Vec<String> = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.iter().any(|arg| arg == "--net"),
            "network off must add --net: {args:?}"
        );
        if is_root_euid() {
            assert!(
                !args.iter().any(|arg| arg == "--user"),
                "real root skips the user-namespace mapping: {args:?}"
            );
        } else {
            assert!(
                args.iter().any(|arg| arg == "--user")
                    && args.iter().any(|arg| arg == "--map-root-user"),
                "unprivileged callers map root inside the sandbox: {args:?}"
            );
        }
        assert_eq!(
            args[args.len() - 2], "/bin/true",
            "the confined command arrives as trailing literal argv: {args:?}"
        );
        assert_eq!(args[args.len() - 1], "hi");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn production_backend_selection_remains_unshare() {
        let real = std::fs::canonicalize("/tmp").unwrap_or_else(|_| PathBuf::from("/tmp"));
        let config = SandboxConfig {
            enabled: true,
            network: false,
            allowed_paths: vec![real.clone()],
            denied_paths: Vec::new(),
            read_only: false,
        };
        let command = match build_command(
            Some(&config),
            &real,
            &["/bin/true".to_owned()],
            &[],
            SandboxStdio::Merged,
        ) {
            Ok(command) => command,
            Err(error) => {
                assert!(
                    error.to_string().contains("unshare"),
                    "the unchanged backend must fail only on missing unshare: {error}"
                );
                return;
            }
        };
        let program = command.as_std().get_program().to_string_lossy();
        assert!(
            program.ends_with("/unshare") || program == "unshare",
            "current policy must still select unshare, never Codex: {program}"
        );
        let args: Vec<_> = command.as_std().get_args().map(|arg| arg.to_string_lossy()).collect();
        assert!(args.iter().any(|arg| arg == SANDBOX_SETUP_SCRIPT));
        assert!(!args.iter().any(|arg| arg.contains("codex-linux-sandbox")));
    }

    /// Runs `/bin/sh -c 'sleep 30.NNN & wait'` through `run_in_sandbox` with
    /// `config: None` (plain spawn — no namespaces required) and asserts the
    /// timeout/abort kill reaps the WHOLE process group, not just the shell.
    /// This is the exact shared runner + kill path production uses for
    /// sandboxed spawns, so the cleanup contract is deterministic on any
    /// Linux host.
    #[cfg(target_os = "linux")]
    async fn assert_run_in_sandbox_group_cleanup(
        marker: &str,
        outcome: SandboxRunOutcome,
    ) {
        // Give the killpg a moment to land, then assert no process carrying
        // the marker command line survives.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let leftover = std::process::Command::new("pgrep")
            .args(["-f", marker])
            .output()
            .expect("pgrep");
        assert!(
            !leftover.status.success() || leftover.stdout.is_empty(),
            "{outcome:?}: the process group must be reaped, no `{marker}` may survive: {:?}",
            leftover.stdout
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn run_in_sandbox_timeout_kills_process_group() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let marker = "sleep 30.001";
        let argv = vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            format!("{marker} & wait"),
        ];
        let (controller, abort) = pi_agent::AbortController::new();
        std::mem::forget(controller);
        let outcome = run_in_sandbox(
            None,
            dir.path(),
            &argv,
            Vec::new(),
            Some(Duration::from_millis(400)),
            abort,
            None,
        )
        .await
        .expect("run must not error");
        assert!(outcome.timed_out, "expected a timeout outcome: {outcome:?}");
        assert_eq!(outcome.exit_code, None, "timeout must not report an exit code");
        assert_run_in_sandbox_group_cleanup(marker, outcome).await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn run_in_sandbox_abort_kills_process_group() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let marker = "sleep 30.002";
        let argv = vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            format!("{marker} & wait"),
        ];
        let (controller, abort) = pi_agent::AbortController::new();
        let cwd = dir.path().to_path_buf();
        let argv: Vec<String> = vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            format!("{marker} & wait"),
        ];
        let handle = tokio::spawn(async move {
            run_in_sandbox(
                None,
                &cwd,
                &argv,
                Vec::new(),
                Some(Duration::from_secs(60)),
                abort,
                None,
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        controller.abort();
        let outcome = handle.await.expect("run task").expect("run must not error");
        assert!(outcome.cancelled, "expected an abort outcome: {outcome:?}");
        assert_eq!(outcome.exit_code, None, "abort must not report an exit code");
        assert_run_in_sandbox_group_cleanup(marker, outcome).await;
    }

    #[test]
    fn unknown_sandbox_settings_fields_survive_round_trip() {
        let raw = r#"{"enabled":true,"network":false,"allowedPaths":["/tmp/w"],"future":1}"#;
        let parsed: SandboxSettings = serde_json::from_str(raw).expect("parse");
        assert_eq!(parsed.enabled, Some(true));
        assert_eq!(parsed.extra.get("future"), Some(&Value::from(1)));
        let serialized = serde_json::to_string(&parsed).expect("serialize");
        let round: serde_json::Value = serde_json::from_str(&serialized).expect("json");
        assert_eq!(round["future"], 1);
    }
}
