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
//! Launcher trust: `unshare` is resolved **only** from fixed system paths
//! (`/usr/bin`, `/bin`, `/usr/sbin`, `/sbin`) — the inherited `PATH` is
//! never consulted, so a PATH-prepended `unshare` (for example in a writable
//! workspace directory) is not a candidate at all. A candidate is accepted
//! only when its canonical (symlink-resolved) path is one of those fixed
//! identities, the file is a regular executable owned by root, and every
//! ancestor in the complete canonical parent chain is a real directory that
//! is neither group- nor world-writable; candidates inside the working
//! directory (the workspace) are rejected. The file's device/inode identity
//! is recorded at discovery and revalidated immediately before process
//! creation, so a swap between discovery and spawn fails closed.
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
    /// non-Linux platforms, an untrustworthy or missing `unshare` launcher,
    /// and a working directory that is not visible inside the sandbox.
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
            if let Err(reason) = discover_unshare_launcher(cwd) {
                bail!("sandbox launcher is not trustworthy: {reason}");
            }
            // Fail closed at the config boundary: every allowed/denied path
            // must be a clean absolute target (no `..`/`.`/prefix, not `/`).
            // The setup script builds staging targets as `$ROOT$path`, so any
            // surviving `..` would let a nonexistent path escape the private
            // root during `mkdir`/`mount`. `resolve` normalizes already; this
            // guards directly-constructed configs that bypass it.
            for path in &self.allowed_paths {
                if let Err(reason) = path_is_clean(path) {
                    bail!("{reason}. Remove it from sandbox.allowedPaths or disable the sandbox.");
                }
            }
            for path in &self.denied_paths {
                if let Err(reason) = path_is_clean(path) {
                    bail!("{reason}. Remove it from sandbox.deniedPaths or disable the sandbox.");
                }
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
    /// The launcher is resolved from fixed system paths only — the inherited
    /// `PATH` is never consulted — and the resolved canonical path is the
    /// wrapper program. `cwd` is the spawn working directory, used to reject
    /// workspace-contained candidates.
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
    pub fn wrapper_command(&self, cwd: &Path, argv: &[String]) -> Result<Vec<String>> {
        let launcher = discover_unshare_launcher(cwd)
            .map_err(|reason| anyhow!("sandbox launcher is not trustworthy: {reason}"))?;
        self.wrapper_argv(&launcher.path, argv)
    }

    /// Assembles the wrapper argv for a validated fixed-system `unshare`
    /// path. `unshare` is a canonical, already-trusted path — this helper
    /// never consults `PATH`. Kept separate from
    /// [`SandboxConfig::wrapper_command`] so `build_command` can discover
    /// the launcher once and revalidate the exact path that will spawn.
    #[cfg(target_os = "linux")]
    fn wrapper_argv(&self, unshare: &Path, argv: &[String]) -> Result<Vec<String>> {
        let mut wrapped = vec![unshare.to_string_lossy().into_owned()];
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
    pub fn wrapper_command(&self, _cwd: &Path, argv: &[String]) -> Result<Vec<String>> {
        let _ = (self, argv);
        bail!(
            "sandbox is only supported on Linux (this build targets {})",
            std::env::consts::OS
        );
    }

    /// Non-Linux fallback for the argv assembly (only reachable after
    /// discovery already failed): explicit unsupported error.
    #[cfg(not(target_os = "linux"))]
    fn wrapper_argv(&self, _unshare: &Path, _argv: &[String]) -> Result<Vec<String>> {
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
///
/// Returns the validated [`Launcher`] (when a sandbox wrapper was built)
/// alongside the command so the caller can revalidate its device/inode
/// identity immediately before process creation.
fn build_command(
    config: Option<&SandboxConfig>,
    cwd: &Path,
    argv: &[String],
    env: &[(String, String)],
    stdio: SandboxStdio,
) -> Result<(tokio::process::Command, Option<Launcher>)> {
    let (program, args, launcher) = if let Some(config) = config {
        config.validate(cwd)?;
        // Discovered once so the spawn-time revalidation compares against
        // the exact path/identity this wrapper will execute.
        let launcher = discover_unshare_launcher(cwd)
            .map_err(|reason| anyhow!("sandbox launcher is not trustworthy: {reason}"))?;
        let wrapped = config.wrapper_argv(&launcher.path, argv)?;
        (wrapped[0].clone(), wrapped[1..].to_vec(), Some(launcher))
    } else {
        (
            argv.first().cloned().ok_or_else(|| anyhow!("empty sandbox argv"))?,
            argv[1..].to_vec(),
            None,
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
    Ok((command, launcher))
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
    let (mut command, launcher) = build_command(config, cwd, argv, &env, SandboxStdio::Merged)?;
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

    // Revalidate the trusted launcher immediately before process creation:
    // a file swapped since discovery, or replaced by a symlink, fails closed.
    if let Some(launcher) = &launcher {
        launcher
            .revalidate()
            .map_err(|reason| anyhow!("sandbox launcher is not trustworthy: {reason}"))?;
    }
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
    let (mut command, launcher) = build_command(config, cwd, argv, &env, SandboxStdio::Piped)?;
    // Revalidate the trusted launcher immediately before process creation
    // (see [`Launcher::revalidate`]).
    if let Some(launcher) = &launcher {
        launcher
            .revalidate()
            .map_err(|reason| anyhow!("sandbox launcher is not trustworthy: {reason}"))?;
    }
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
            if let Some(path) = normalize_sandbox_path(cwd, raw) {
                allowed_paths.push(path);
            }
        }
    }
    if allowed_paths.is_empty() {
        allowed_paths.push(canonicalize_or(cwd.to_path_buf()));
        allowed_paths.push(canonicalize_or(agent_dir.to_path_buf()));
    }
    let mut denied_paths = Vec::new();
    if let Some(paths) = &settings.denied_paths {
        for raw in paths {
            if let Some(path) = normalize_sandbox_path(cwd, raw) {
                denied_paths.push(path);
            }
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

/// Normalizes a sandbox path so it can never carry a `..` component into the
/// shell transport, where the setup script builds staging targets as
/// `$ROOT$path` and a surviving `..` would let a nonexistent path escape the
/// private root during `mkdir`/`mount` (the 0.2.4 P1: denied paths that do
/// not exist are never `canonicalize`d, so their lexical `..` reached the
/// shell untouched).
///
/// Existing paths are `canonicalize`d FIRST so symlink/`..` semantics follow
/// the real filesystem — a `..` after a symlink is NOT a lexical pop, so
/// collapsing first (the original buggy shape) would mis-resolve
/// `/a/link/../x` to `/a/x` instead of the real target. Lexical collapse
/// runs only as the fallback for paths that do not exist on the host, which
/// is exactly the P1 case, and the collapsed result is re-canonicalized in
/// case the collapse made an existing path resolvable. Returns `None` for
/// empty paths, prefix components (e.g. drive letters), and paths that
/// normalize to the filesystem root — degenerate targets dropped exactly like
/// the existing empty-string skip. The original (untrimmed) `raw` is parsed
/// so valid path names that begin or end with spaces are preserved; only the
/// emptiness check trims. [`SandboxConfig::validate`] independently enforces
/// the same invariant on directly-constructed configs, so a `..` can never
/// reach a spawn.
fn normalize_sandbox_path(cwd: &Path, raw: &str) -> Option<PathBuf> {
    if raw.trim().is_empty() {
        return None;
    }
    let path = PathBuf::from(raw);
    let absolute = if path.is_absolute() { path } else { cwd.join(&path) };
    // Existing path: canonicalize so symlink/`..` semantics follow the real
    // filesystem. A canonical path is absolute with no `..`/`.`/prefix.
    if let Ok(canonical) = std::fs::canonicalize(&absolute) {
        return canonical_root_rejected(canonical);
    }
    // Nonexistent (or an intermediate component is missing): lexically
    // collapse `.`/`..` so no traversal component can reach the shell.
    let normalized = lexically_normalize(&absolute)?;
    // The collapse may have made an existing path resolvable (e.g.
    // `/tmp/missing/../real`); canonicalize once more to keep symlink
    // semantics correct when possible.
    if let Ok(canonical) = std::fs::canonicalize(&normalized) {
        return canonical_root_rejected(canonical);
    }
    Some(normalized)
}

/// Rejects the filesystem root (`canonicalize("/")` is `/`, which would
/// overlay the sandbox root itself if used as a staging target).
fn canonical_root_rejected(canonical: PathBuf) -> Option<PathBuf> {
    (canonical != Path::new("/")).then_some(canonical)
}

/// Lexically collapses `.`/`..` against preceding normal components with root
/// clamping (a `..` above `/` stays `/`). Returns `None` for a prefix
/// component (e.g. a Windows drive letter) or when the result is the
/// filesystem root. Used only as the nonexistent-path fallback in
/// [`normalize_sandbox_path`]; existing paths are canonicalized instead.
fn lexically_normalize(absolute: &Path) -> Option<PathBuf> {
    use std::path::Component;
    let mut stack: Vec<Component> = Vec::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(_) => return None,
            Component::RootDir => {
                stack.clear();
                stack.push(Component::RootDir);
            }
            Component::CurDir => continue,
            Component::ParentDir => match stack.last() {
                Some(Component::Normal(_)) => {
                    stack.pop();
                }
                _ => {} // at the root: clamp, `..` above `/` stays `/`
            },
            Component::Normal(_) => stack.push(component),
        }
    }
    let mut normalized = PathBuf::new();
    for component in &stack {
        normalized.push(component.as_os_str());
    }
    (normalized != Path::new("/")).then_some(normalized)
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

/// True when `path` is a valid sandbox transport target: absolute, with no
/// `..`/prefix components and not the filesystem root. The setup script builds
/// staging paths as `$ROOT$path`, so any `..` component would let a
/// (nonexistent) path escape the private root during `mkdir`/`mount`. This
/// invariant is the fail-closed gate for directly-constructed configs that
/// bypass [`resolve`]'s normalization.
///
/// Only `..` and prefix components are rejected — `.` components are
/// harmless (they never let `$ROOT$path` escape) and `Path::components`
/// skips mid-path `.` anyway, so they are not a traversal risk. The setup
/// script adds its own shell-level `.`/`..` rejection as defense-in-depth.
fn path_is_clean(path: &Path) -> Result<(), String> {
    use std::path::Component;
    if !path.is_absolute() {
        return Err(format!("sandbox path {} is not absolute", path.display()));
    }
    if path == Path::new("/") {
        return Err("sandbox path '/' (filesystem root) cannot be a sandbox target".to_owned());
    }
    for component in path.components() {
        match component {
            Component::Prefix(_) => {
                return Err(format!("sandbox path {} has a prefix component (e.g. drive letter)", path.display()));
            }
            Component::ParentDir => {
                return Err(format!("sandbox path {} contains a '..' component", path.display()));
            }
            Component::RootDir | Component::Normal(_) | Component::CurDir => {}
        }
    }
    Ok(())
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

/// Resolves `name` from the inherited `PATH`. Used only for the `/bin/sh`
/// fallback that runs the setup script *inside* the already-created fresh
/// namespaces — the `unshare` launcher itself is NEVER resolved from `PATH`
/// (see [`discover_unshare_launcher`]), so a PATH-prepended `unshare` is
/// never a sandbox candidate.
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

/// Fixed system locations util-linux installs `unshare` at. Discovery never
/// consults the inherited `PATH`; only these absolute paths are candidates,
/// so a PATH-prepended `unshare` (for example in a writable workspace
/// directory) is not a candidate at all and can never be selected, let alone
/// executed.
#[cfg(target_os = "linux")]
const TRUSTED_UNSHARE_CANDIDATES: [&str; 4] =
    ["/usr/bin/unshare", "/bin/unshare", "/usr/sbin/unshare", "/sbin/unshare"];

/// POSIX `st_mode` file-type bits (see `stat(2)`): the type mask and the
/// regular-file/directory values. Kept as local literals so the trust seam
/// has no dependency surface.
#[cfg(target_os = "linux")]
const S_IFMT: u32 = 0o170000;
#[cfg(target_os = "linux")]
const S_IFREG: u32 = 0o100000;
#[cfg(target_os = "linux")]
const S_IFDIR: u32 = 0o040000;

/// The canonical (symlink-resolved) identities a launcher may have: each
/// fixed candidate canonicalized. Merged-usr systems canonicalize
/// `/bin/unshare` to `/usr/bin/unshare`, so membership is decided on the
/// canonical path — a symlink that resolves outside the fixed set (for
/// example into a workspace or user directory) is rejected.
#[cfg(target_os = "linux")]
fn trusted_launcher_canonicals() -> Vec<PathBuf> {
    TRUSTED_UNSHARE_CANDIDATES
        .iter()
        .filter_map(|candidate| std::fs::canonicalize(candidate).ok())
        .collect()
}

/// Facts about a discovered launcher candidate, gathered once at discovery.
/// Kept separate from the trust decision so every rejection — non-root
/// owner, missing execute bit, writable file or ancestor, workspace
/// containment — is unit-testable without touching the real filesystem.
#[cfg(target_os = "linux")]
#[derive(Clone, Debug)]
struct LauncherProbe {
    /// Canonical (symlink-resolved) path of the candidate file.
    canonical: PathBuf,
    /// `st_mode` of the file itself.
    mode: u32,
    /// `st_uid` of the file.
    uid: u32,
    /// Device id of the file (`st_dev`).
    dev: u64,
    /// Inode of the file (`st_ino`).
    ino: u64,
    /// `st_mode` of every ancestor directory of the canonical chain, from
    /// the parent directory up to and including `/`.
    ancestor_modes: Vec<u32>,
}

/// Gathers the facts [`probe_is_trusted`] needs for `canonical` (a
/// symlink-resolved fixed-system path). Every ancestor is read with
/// `symlink_metadata` and must be a real directory — a component swapped for
/// a symlink after canonicalization fails closed. Returns `None` when the
/// file or an ancestor cannot be stat'd.
#[cfg(target_os = "linux")]
fn probe_candidate(canonical: &Path) -> Option<LauncherProbe> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(canonical).ok()?;
    let mut ancestor_modes = Vec::new();
    let mut dir = canonical.parent()?;
    loop {
        let meta = std::fs::symlink_metadata(dir).ok()?;
        if meta.mode() & S_IFMT != S_IFDIR {
            return None;
        }
        ancestor_modes.push(meta.mode());
        let Some(parent) = dir.parent() else {
            break;
        };
        dir = parent;
    }
    Some(LauncherProbe {
        canonical: canonical.to_path_buf(),
        mode: meta.mode(),
        uid: meta.uid(),
        dev: meta.dev(),
        ino: meta.ino(),
        ancestor_modes,
    })
}

/// Pure trust decision over a [`LauncherProbe`]. A launcher is accepted only
/// when
///
/// - its canonical path is one of the fixed-system identities in `trusted`,
/// - the file is a regular file, owned by root, executable, and not group-
///   or world-writable (a group-writable launcher could be swapped by a
///   same-group attacker),
/// - every ancestor directory in the complete canonical chain is neither
///   group- nor world-writable (a writable ancestor allows replacing the
///   launcher),
/// - the canonical path is not inside `cwd` (a "system" binary living inside
///   the working directory/workspace is user-controlled by definition; the
///   check is skipped when `cwd` is `/`, where it would be vacuous).
///
/// Returns `Ok(())` or a readable rejection reason.
#[cfg(target_os = "linux")]
fn probe_is_trusted(probe: &LauncherProbe, trusted: &[PathBuf], cwd: &Path) -> Result<(), String> {
    if !trusted.iter().any(|candidate| candidate == &probe.canonical) {
        return Err(format!(
            "{} is not a trusted fixed-system unshare identity",
            probe.canonical.display()
        ));
    }
    if probe.mode & S_IFMT != S_IFREG {
        return Err(format!(
            "{} is not a regular file (mode {:o})",
            probe.canonical.display(),
            probe.mode
        ));
    }
    if probe.uid != 0 {
        return Err(format!(
            "{} is not owned by root (uid {})",
            probe.canonical.display(),
            probe.uid
        ));
    }
    if probe.mode & 0o111 == 0 {
        return Err(format!(
            "{} is not executable (mode {:o})",
            probe.canonical.display(),
            probe.mode
        ));
    }
    if probe.mode & 0o022 != 0 {
        return Err(format!(
            "{} is group- or world-writable (mode {:o})",
            probe.canonical.display(),
            probe.mode
        ));
    }
    if let Some(ancestor) = probe.ancestor_modes.iter().find(|mode| **mode & 0o022 != 0) {
        return Err(format!(
            "an ancestor of {} is group- or world-writable (mode {:o})",
            probe.canonical.display(),
            ancestor
        ));
    }
    // The containment check keeps a user-controlled workspace tree from
    // hosting the launcher; with a working directory of `/` it is vacuous
    // (every system path is under `/`), so it is skipped there — the fixed
    // canonical identity and ownership/mode/chain checks still apply.
    if cwd != Path::new("/") && probe.canonical.starts_with(cwd) {
        return Err(format!(
            "{} is inside the working directory (workspace); refusing a workspace-contained launcher",
            probe.canonical.display()
        ));
    }
    Ok(())
}

/// A validated launcher: the canonical path plus the device/inode identity
/// recorded at discovery. [`Launcher::revalidate`] is called immediately
/// before process creation so a file swapped between discovery and spawn
/// (different inode) or replaced by a symlink (no longer a regular file)
/// fails closed.
#[cfg(target_os = "linux")]
#[derive(Clone, Debug)]
struct Launcher {
    path: PathBuf,
    dev: u64,
    ino: u64,
}

#[cfg(target_os = "linux")]
impl Launcher {
    /// Re-checks, immediately before spawn, that `path` is still the exact
    /// file discovered: a regular file (never a symlink) with the same
    /// device and inode. Any mismatch fails closed.
    fn revalidate(&self) -> Result<(), String> {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::symlink_metadata(&self.path).map_err(|error| {
            format!("launcher {} vanished before spawn: {error}", self.path.display())
        })?;
        if meta.mode() & S_IFMT != S_IFREG {
            return Err(format!(
                "launcher {} is no longer a regular file (mode {:o}); refusing to spawn",
                self.path.display(),
                meta.mode()
            ));
        }
        if meta.dev() != self.dev || meta.ino() != self.ino {
            return Err(format!(
                "launcher {} changed identity (device/inode mismatch); refusing to spawn",
                self.path.display()
            ));
        }
        Ok(())
    }
}

/// Resolves the trusted `unshare` launcher for a sandboxed spawn, or a
/// readable reason when no fixed-system candidate passes every check. The
/// inherited `PATH` is never consulted: a PATH-prepended `unshare` is not a
/// candidate at all.
#[cfg(target_os = "linux")]
fn discover_unshare_launcher(cwd: &Path) -> Result<Launcher, String> {
    let cwd = canonicalize_or(cwd.to_path_buf());
    let trusted = trusted_launcher_canonicals();
    let mut rejections = Vec::new();
    for candidate in TRUSTED_UNSHARE_CANDIDATES {
        let Ok(canonical) = std::fs::canonicalize(candidate) else {
            continue; // not installed at this fixed path
        };
        let Some(probe) = probe_candidate(&canonical) else {
            continue;
        };
        match probe_is_trusted(&probe, &trusted, &cwd) {
            Ok(()) => {
                return Ok(Launcher {
                    path: canonical,
                    dev: probe.dev,
                    ino: probe.ino,
                });
            }
            Err(reason) => rejections.push(reason),
        }
    }
    if rejections.is_empty() {
        Err("no `unshare` found in the fixed system paths (/usr/bin, /bin, /usr/sbin, /sbin); install util-linux or disable the sandbox (sandbox.enabled=false)".to_owned())
    } else {
        Err(rejections.join("; "))
    }
}

/// Non-Linux stub: the sandbox is unsupported there, so discovery always
/// fails (only reachable after the platform error in
/// [`SandboxConfig::validate`]).
#[cfg(not(target_os = "linux"))]
fn discover_unshare_launcher(_cwd: &Path) -> Result<Launcher, String> {
    Err("sandbox is only supported on Linux".to_owned())
}

/// Placeholder for non-Linux builds: never constructed (discovery fails
/// first), so revalidation is a no-op. Carries the same `path` field the
/// Linux build has so `build_command` compiles unchanged.
#[cfg(not(target_os = "linux"))]
struct Launcher {
    path: PathBuf,
}

#[cfg(not(target_os = "linux"))]
impl Launcher {
    fn revalidate(&self) -> Result<(), String> {
        Ok(())
    }
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
/// the final private tmpfs and its parent chain survives `pivot_root`; allowed
/// mountpoints are pre-created on the unbound tmpfs root BEFORE any host bind
/// (system or allowed) so the staging `mkdir`/`: >` for a nested allowed path
/// can never reach through an already-bound parent and truncate a host file;
/// system dirs are then bound read-only before the allowed binds so an allowed
/// path nested in a system dir still wins; denied overlays are constructed
/// before any binds and re-applied after them so they always win, and an absent
/// denied path covered by a bind fails closed (exit 95) rather than ever
/// becoming visible or leaking a host path into existence.
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

# Defense-in-depth: every transported path must be lexically contained — no
# `..` or `.` component — so `$ROOT$path` can never escape the private root
# during staging `mkdir`/`mount`. The Rust side normalizes paths already; this
# rejects any `..` that slipped through before a single mount runs. Absoluteness
# is NOT checked here (the existing injection tests deliberately feed hostile
# non-absolute values that must reach the mount step untouched), only traversal.
# Runs in a subshell so the parent's positional arguments stay intact.
(
  i=0
  total=$((ALLOWED_COUNT + DENIED_COUNT))
  while [ "$i" -lt "$total" ]; do
    p=$1
    shift
    i=$((i + 1))
    [ -n "$p" ] || { echo "sandbox: empty path in transport (malformed transport)" >&2; exit 70; }
    case "$p" in
      /..|/../*|*/..|*/../*|..|../*)
        echo "sandbox: path '$p' contains a '..' component (malformed transport)" >&2; exit 70;;
    esac
    case "$p" in
      /.|/./*|*/.|*/./*|.|./*)
        echo "sandbox: path '$p' contains a '.' component (malformed transport)" >&2; exit 70;;
    esac
  done
) || exit 70

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

# Denied paths: empty overlay that wins over allowed and system binds.
# Absent denied paths are constructed here, BEFORE any bind mounts exist at
# their host paths, so mkdir/mount always happen on the tmpfs root and can
# never create the denied path on the host. Runs in a subshell so the
# parent's positional arguments stay intact for the re-application pass
# after the binds (mounts are namespace state and survive the subshell).
: > "$ROOT/.pi-deny-file" 2>/dev/null || true
(
  shift "$ALLOWED_COUNT" 2>/dev/null || true
  i=0
  while [ "$i" -lt "$DENIED_COUNT" ]; do
    p=$1
    shift
    i=$((i + 1))
    [ -n "$p" ] || continue
    if [ -d "$p" ] || [ ! -e "$p" ]; then
      mkdir -p "$ROOT$p" 2>/dev/null || true
      # Absent targets get a read-only overlay: mode-000 alone is bypassed
      # by the userns-root command, so ro makes the empty dir inaccessible.
      opts="mode=000"
      [ -e "$p" ] || opts="mode=000,ro"
      mount -t tmpfs -o "$opts" tmpfs "$ROOT$p" || { echo "sandbox: deny overlay $p failed" >&2; exit 95; }
    else
      mkdir -p "$(dirname "$ROOT$p")" 2>/dev/null || true
      : > "$ROOT$p" 2>/dev/null || true
      mount --bind "$ROOT/.pi-deny-file" "$ROOT$p" || { echo "sandbox: deny overlay $p failed" >&2; exit 95; }
    fi
  done
) || exit 95

# Allowed paths are mounted in TWO passes so staging can never reach through
# an already-bound parent onto the host (the sandbox P1: a single-pass loop
# that creates a mountpoint and binds it in the same iteration lets a bind
# onto a parent — e.g. /foo — make `$ROOT$p` for a later nested allowed path
# — e.g. /foo/bar/baz.txt — resolve through that bind onto the real host
# file, so the `: >`/`mkdir` staging truncates/creates host content before
# the confined command ever runs).
#
# Pass 1 (allowed staging) runs BEFORE any host bind — system or allowed —
# so `$ROOT$p` always names a path on the unbound tmpfs root. The `mkdir`/
# `: >` staging for a nested allowed path lands on the private tmpfs, never
# on a host file a system or allowed parent bind would expose (a system
# bind whose read-only remount silently failed would otherwise be a writable
# parent). Pass 2 (allowed bind), after the system binds, attaches each host
# path to its pre-created mountpoint in list order so a parent bind stacks
# beneath a later child bind; it creates no target, so a parent bind cannot
# funnel a later bind's staging into the host.
#
# Each `$1` is a literal path — no shell re-parsing. Pass 1 runs in a
# subshell so the parent's positional arguments stay intact for the bind
# pass; the created directories/files live on the tmpfs root and survive
# the subshell.
# >>> allowed-staging-begin
(
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
  done
) || { echo "sandbox: allowed mountpoint staging failed" >&2; exit 93; }
# >>> allowed-staging-end

# System binaries/libs visible read-only so commands can execute; user data
# under these roots stays hidden. Bound before allowed paths so an allowed
# path nested in a system dir still wins.
for d in /usr /bin /sbin /lib /lib64; do
  [ -e "$d" ] || continue
  mkdir -p "$ROOT$d"
  mount --bind "$d" "$ROOT$d" 2>/dev/null || { echo "sandbox: bind $d failed" >&2; exit 94; }
  mount -o remount,bind,ro "$ROOT$d" 2>/dev/null || true
done
# >>> allowed-bind-begin
i=0
while [ "$i" -lt "$ALLOWED_COUNT" ]; do
  p=$1
  shift
  i=$((i + 1))
  [ -n "$p" ] && [ -e "$p" ] || continue
  mount --bind "$p" "$ROOT$p" || { echo "sandbox: bind $p failed" >&2; exit 93; }
  if [ "${PI_SANDBOX_READ_ONLY:-0}" = "1" ]; then
    mount -o remount,bind,ro "$ROOT$p" || { echo "sandbox: remount $p read-only failed" >&2; exit 88; }
  fi
done
# >>> allowed-bind-end

# Denied overlays re-applied after the binds: they always win. An absent
# denied path covered by a bind has no mountpoint to attach to, and creating
# one would leak the path onto the host — fail closed instead of running the
# command with the denied path visible.
i=0
while [ "$i" -lt "$DENIED_COUNT" ]; do
  p=$1
  shift
  i=$((i + 1))
  [ -n "$p" ] || continue
  if [ -d "$p" ] || [ ! -e "$p" ]; then
    opts="mode=000"
    [ -e "$p" ] || opts="mode=000,ro"
    mount -t tmpfs -o "$opts" tmpfs "$ROOT$p" || { echo "sandbox: denied path $p is absent on the host and cannot be hidden under a bind; refusing to run" >&2; exit 95; }
  else
    mount --bind "$ROOT/.pi-deny-file" "$ROOT$p" || { echo "sandbox: denied path $p is absent on the host and cannot be hidden under a bind; refusing to run" >&2; exit 95; }
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
            .wrapper_command(Path::new("/tmp"), &["/bin/echo".to_owned(), "hi".to_owned()])
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
            .wrapper_command(
                Path::new("/tmp"),
                &["/bin/bash".to_owned(), "-c".to_owned(), "echo hi".to_owned()],
            )
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
            .wrapper_command(
                Path::new("/tmp"),
                &["/bin/bash".to_owned(), "-c".to_owned(), "echo hi".to_owned()],
            )
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
            .wrapper_command(
                Path::new("/tmp"),
                &[
                    "/bin/bash".to_owned(),
                    "-lc".to_owned(),
                    "echo 'a b'".to_owned(),
                ],
            )
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
            .wrapper_command(Path::new("/tmp"), &["/bin/true".to_owned()])
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
            .wrapper_command(Path::new("/tmp"), &["/bin/echo".to_owned(), "lit".to_owned()])
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

    /// A configured denied path that does not exist on the host is still
    /// denied inside the sandbox — including when it nests under an allowed
    /// parent. The construct-before-binds / re-apply-after-binds dance must
    /// never fall back to creating the denied path on the host: when the
    /// absent path is covered by an allowed bind there is no mountpoint to
    /// attach the overlay to, so the setup script refuses to run (exit 95)
    /// and the host never sees the sentinel. On hosts where namespace setup
    /// itself fails, the fail-closed path (exit 1 or 90-99) proves the same
    /// no-leak property.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "live confinement test: requires Linux unprivileged user namespaces (run with --include-ignored on a supported kernel)"]
    async fn denied_absent_under_allowed_parent_never_leaks_to_host() {
        let dir = tempfile::TempDir::new().expect("allowed parent temp dir");
        // Canonicalize the allowed parent so the sandbox bind matches the
        // path the config names; the absent denied child is never resolved.
        let allowed = dir.path().canonicalize().expect("canonicalize allowed parent");
        let denied = allowed.join("secret");
        // The allowed parent is real content; it must survive the run intact.
        let allowed_file = allowed.join("allowed.txt");
        std::fs::write(&allowed_file, "allowed content").expect("write allowed file");
        let config = SandboxConfig {
            enabled: true,
            network: false,
            allowed_paths: vec![allowed.clone()],
            denied_paths: vec![denied.clone()],
            read_only: false,
        };
        let argv = vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "mkdir -p secret/deep && touch secret/deep/pwned && echo RAN".to_owned(),
        ];
        let outcome = run_in_sandbox(
            Some(&config),
            &allowed,
            &argv,
            Vec::new(),
            None,
            AbortSignal::none(),
            None,
        )
        .await
        .expect("sandboxed run must not error at the runner level");
        match outcome.exit_code {
            Some(95) => {}
            Some(code) if code == 1 || (90..=99).contains(&code) => {
                // Host cannot create the namespaces (or the launcher is
                // missing): the confined command never ran, which still
                // proves the denied path never leaked.
                eprintln!(
                    "denied-absent live-run: sandbox namespace setup unavailable here (exit {code}); the fail-closed path still proved no leak"
                );
            }
            other => panic!("unexpected sandbox outcome: {other:?}"),
        }
        assert!(
            !denied.exists(),
            "the absent denied path must never be created on the host: {}",
            denied.display()
        );
        assert!(
            !allowed.join("secret/deep/pwned").exists(),
            "the sandboxed command must never be able to write under the denied path"
        );
        assert_eq!(
            std::fs::read_to_string(&allowed_file).expect("read allowed file"),
            "allowed content",
            "allowed content must survive the run intact"
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
        let (command, launcher) = match build() {
            Ok(pair) => pair,
            Err(error) => {
                assert!(
                    error.to_string().contains("unshare"),
                    "the only legitimate build failure is the missing-unshare fail-closed error: {error}"
                );
                return;
            }
        };
        assert!(
            launcher.is_some(),
            "a validated launcher identity is recorded for spawn-time revalidation"
        );
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
        let (command, _launcher) = match build_command(
            Some(&config),
            &real,
            &["/bin/true".to_owned()],
            &[],
            SandboxStdio::Merged,
        ) {
            Ok(pair) => pair,
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

    /// Regression: a writable `unshare` prepended to `PATH` — the classic
    /// workspace-escape vector, where the old lookup consulted the inherited
    /// `PATH` — must never be selected, let alone executed, by a sandboxed
    /// spawn.
    ///
    /// Edition 2024 forbids `unsafe`, which `std::env::set_var` requires, so
    /// the hostile environment is built in a child process: the test
    /// re-executes itself with the sentinel directory first on `PATH` and as
    /// its working directory (mirroring the repo's fake-server re-exec test
    /// pattern). The child asserts the wrapper program is the trusted
    /// fixed-system `unshare` — never the sentinel — and runs a real
    /// sandboxed command: the sentinel's marker file must stay absent, both
    /// when the namespaces can be created (the sentinel would run and touch
    /// it) and when the host fails closed on namespace setup (the setup
    /// script's exit 90 or `unshare`'s own failure, still without touching
    /// the marker).
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn path_prepended_unshare_sentinel_is_never_selected_or_executed() {
        const CHILD_ENV: &str = "PI_SANDBOX_SENTINEL_CHILD";
        if std::env::var(CHILD_ENV).is_err() {
            // Parent: build the hostile workspace, then run the real
            // assertions in a child whose environment has the sentinel dir
            // prepended to PATH and used as its working directory.
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::TempDir::new().expect("hostile workspace");
            let sentinel = dir.path().join("unshare");
            let marker = dir.path().join("pwned");
            // The wrapper's environment is env_clear()ed, so the marker path
            // is embedded literally (single-quoted; embedded quotes escaped).
            let marker_quoted = marker.display().to_string().replace('\'', "'\\''");
            std::fs::write(
                &sentinel,
                format!("#!/bin/sh\ntouch '{}'\n", marker_quoted),
            )
            .expect("write sentinel unshare");
            std::fs::set_permissions(&sentinel, std::fs::Permissions::from_mode(0o755))
                .expect("sentinel executable");
            let hostile_path = std::env::join_paths(
                std::iter::once(dir.path().to_path_buf()).chain(std::env::split_paths(
                    &std::env::var_os("PATH").unwrap_or_default(),
                )),
            )
            .expect("hostile PATH");
            let exe = std::env::current_exe().expect("test binary path");
            let status = tokio::process::Command::new(exe)
                // Substring filter (no `--exact`, mirroring the repo's
                // fake-server re-exec pattern) so only this test runs in the
                // child regardless of the harness's module-path naming.
                .arg("path_prepended_unshare_sentinel_is_never_selected_or_executed")
                .env(CHILD_ENV, "1")
                .env("PATH", hostile_path)
                .current_dir(dir.path())
                .status()
                .await
                .expect("run child test");
            assert!(
                status.success(),
                "the child assertions must pass: a PATH-prepended unshare is never selected or executed"
            );
            assert!(
                !marker.exists(),
                "the sentinel unshare must never have been executed: {}",
                marker.display()
            );
            return;
        }
        // Child: the sentinel dir is first on PATH and is the working
        // directory; every assertion below runs in that hostile environment.
        let cwd = std::env::current_dir().expect("hostile working directory");
        let marker = cwd.join("pwned");
        let _ = std::fs::remove_file(&marker);
        let config = SandboxConfig {
            enabled: true,
            network: false,
            allowed_paths: vec![cwd.clone()],
            denied_paths: Vec::new(),
            read_only: false,
        };
        // 1. Never selected: the wrapper program must be a trusted
        // fixed-system unshare, never the PATH-prepended sentinel.
        match config.wrapper_command(&cwd, &["/bin/true".to_owned()]) {
            Ok(argv) => {
                let program = PathBuf::from(argv[0].as_str());
                assert!(
                    trusted_launcher_canonicals()
                        .iter()
                        .any(|candidate| candidate == &program),
                    "the wrapper program {program:?} must be a trusted fixed-system unshare, never the PATH-prepended sentinel: {argv:?}"
                );
                assert!(
                    !program.starts_with(&cwd),
                    "a workspace-contained launcher must never be selected: {program:?}"
                );
                // 2. Never executed: run a real sandboxed command and require
                // the sentinel's marker to stay absent. When the host cannot
                // create the namespaces, `unshare` fails (exit 1) or the
                // setup script fails closed (exit 90-99) before the confined
                // command runs — either way the sentinel never runs.
                let outcome = run_in_sandbox(
                    Some(&config),
                    &cwd,
                    &["/bin/true".to_owned()],
                    Vec::new(),
                    None,
                    AbortSignal::none(),
                    None,
                )
                .await
                .expect("sandboxed run must not error at the runner level");
                match outcome.exit_code {
                    Some(0) => {}
                    Some(code) => {
                        // Any nonzero exit without a marker is the fail-closed
                        // path (unshare could not create the namespaces, or
                        // the setup script aborted before the confined command
                        // ran). The sentinel never ran either way.
                        eprintln!(
                            "sentinel live-run: sandbox namespace setup unavailable here (exit {code}); the fail-closed path still proved the sentinel never ran"
                        );
                    }
                    None => panic!("unexpected sandbox outcome (child killed by signal): {outcome:?}"),
                }
                assert!(
                    !marker.exists(),
                    "the sentinel unshare must never have been executed: {}",
                    marker.display()
                );
            }
            Err(error) => {
                assert!(
                    error.to_string().contains("unshare"),
                    "the only legitimate failure is the trusted-launcher fail-closed error, never sentinel selection: {error}"
                );
                assert!(
                    !marker.exists(),
                    "the sentinel unshare must never have been executed: {}",
                    marker.display()
                );
            }
        }
    }

    /// The trust seam rejects every insecure launcher candidate without
    /// touching the real filesystem: canonical identity, ownership, mode,
    /// writable ancestors, and workspace containment are decided purely from
    /// gathered facts, so no privileged filesystem mutation is needed to
    /// prove the rejections.
    #[cfg(target_os = "linux")]
    #[test]
    fn launcher_trust_rejects_insecure_candidates() {
        // The workspace-contained canonical is deliberately a member of the
        // trusted set: the containment check is defense-in-depth that must
        // fire even when canonical identity would otherwise pass.
        let trusted = vec![
            PathBuf::from("/usr/bin/unshare"),
            PathBuf::from("/home/user/workspace/bin/unshare"),
        ];
        let cwd = Path::new("/home/user/workspace");
        let secure = || LauncherProbe {
            canonical: PathBuf::from("/usr/bin/unshare"),
            mode: 0o100755,
            uid: 0,
            dev: 1,
            ino: 1,
            ancestor_modes: vec![0o755, 0o755, 0o755], // /usr/bin, /usr, /
        };
        let cases: &[(&str, LauncherProbe, &str)] = &[
            (
                "canonical path outside the trusted set",
                LauncherProbe {
                    canonical: PathBuf::from("/usr/local/bin/unshare"),
                    ..secure()
                },
                "trusted fixed-system",
            ),
            (
                "symlink (not a regular file)",
                LauncherProbe {
                    mode: 0o120777,
                    ..secure()
                },
                "regular file",
            ),
            (
                "non-root owner",
                LauncherProbe {
                    uid: 1000,
                    ..secure()
                },
                "root",
            ),
            (
                "missing execute bit",
                LauncherProbe {
                    mode: 0o100644,
                    ..secure()
                },
                "not executable",
            ),
            (
                "world-writable file",
                LauncherProbe {
                    mode: 0o100777,
                    ..secure()
                },
                "group- or world-writable",
            ),
            (
                "group-writable ancestor",
                LauncherProbe {
                    ancestor_modes: vec![0o775, 0o755, 0o755],
                    ..secure()
                },
                "group- or world-writable",
            ),
            (
                "world-writable ancestor",
                LauncherProbe {
                    ancestor_modes: vec![0o755, 0o777, 0o755],
                    ..secure()
                },
                "group- or world-writable",
            ),
            (
                "workspace-contained candidate",
                LauncherProbe {
                    canonical: PathBuf::from("/home/user/workspace/bin/unshare"),
                    ..secure()
                },
                "workspace",
            ),
        ];
        for (name, probe, needle) in cases {
            let error = probe_is_trusted(probe, &trusted, cwd).expect_err(name);
            assert!(
                error.contains(*needle),
                "{name}: expected a rejection mentioning {needle:?}, got: {error}"
            );
        }
        probe_is_trusted(&secure(), &trusted, cwd)
            .expect("a trusted root-owned regular executable with a clean chain is accepted");
    }

    /// The spawn-time revalidation requires the exact same device/inode: a
    /// different recorded identity, a symlink placed at the path, or a
    /// vanished file all fail closed. Uses a scratch file, so no privileged
    /// filesystem mutation is needed.
    #[cfg(target_os = "linux")]
    #[test]
    fn launcher_revalidation_fails_closed_on_identity_change() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = tempfile::TempDir::new().expect("temp dir");
        let file = dir.path().join("launcher");
        std::fs::write(&file, "#!/bin/sh\nexit 0\n").expect("write scratch launcher");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).expect("mode");
        let meta = std::fs::symlink_metadata(&file).expect("stat scratch launcher");
        let launcher = Launcher {
            path: file.clone(),
            dev: meta.dev(),
            ino: meta.ino(),
        };
        launcher
            .revalidate()
            .expect("the same device/inode revalidates");
        // A different recorded identity for the same path must fail closed.
        let swapped = Launcher {
            path: file.clone(),
            dev: meta.dev() ^ 1,
            ino: meta.ino() ^ 1,
        };
        assert!(swapped.revalidate().is_err(), "an identity change must fail closed");
        // A symlink placed at the path must fail closed (it is not the
        // regular file that was discovered).
        let link = dir.path().join("launcher-swapped");
        std::os::unix::fs::symlink(&file, &link).expect("symlink");
        let symlink_launcher = Launcher {
            path: link,
            dev: meta.dev(),
            ino: meta.ino(),
        };
        assert!(symlink_launcher.revalidate().is_err(), "a symlink swap must fail closed");
        // A vanished launcher must fail closed.
        let gone = dir.path().join("never-written");
        let gone_launcher = Launcher {
            path: gone,
            dev: meta.dev(),
            ino: meta.ino(),
        };
        assert!(gone_launcher.revalidate().is_err(), "a vanished launcher must fail closed");
    }

    /// `resolve` must lexically normalize `..`/`.` out of allowed and denied
    /// paths so neither list can carry a traversal component into the shell
    /// transport. Nonexistent paths are never `canonicalize`d, so without
    /// normalization a `..` would reach the setup script verbatim and let
    /// `$ROOT$path` escape the private root during staging (the 0.2.4 P1).
    #[cfg(target_os = "linux")]
    #[test]
    fn resolve_normalizes_traversal_components_out_of_paths() {
        let dir = tempfile::TempDir::new().expect("temp root");
        let root = dir.path().canonicalize().expect("canonicalize temp root");
        let cwd = root.join("work");
        std::fs::create_dir_all(&cwd).expect("create cwd");
        let config = resolve(
            Some(&settings(
                Some(true),
                None,
                Some(vec![
                    "/tmp/allowed/./sub/../final", // -> /tmp/allowed/final
                    "relative/../sibling",         // -> cwd/sibling
                ]),
                Some(vec![
                    "/tmp/denied/with/../traversal",        // -> /tmp/denied/traversal
                    "/tmp/denied/escape/../../target",      // -> /tmp/target
                ]),
            )),
            &cwd,
            &root.join("agent"),
        )
        .expect("config");
        for path in config.allowed_paths.iter().chain(config.denied_paths.iter()) {
            assert!(
                path_is_clean(path).is_ok(),
                "normalized sandbox path must be clean: {path:?}"
            );
        }
        assert!(config.allowed_paths.iter().any(|p| p == Path::new("/tmp/allowed/final")));
        assert!(config.allowed_paths.iter().any(|p| p == &cwd.join("sibling")));
        assert!(config.denied_paths.iter().any(|p| p == Path::new("/tmp/denied/traversal")));
        assert!(config.denied_paths.iter().any(|p| p == Path::new("/tmp/target")));
    }

    /// A denied path that lexically escapes toward the filesystem root is
    /// clamped at `/` and dropped when it normalizes to the root itself:
    /// denying `/` is nonsensical (it would overlay the sandbox root) and a
    /// `..` above `/` can never be a real target.
    #[cfg(target_os = "linux")]
    #[test]
    fn resolve_drops_paths_that_normalize_to_filesystem_root() {
        let dir = tempfile::TempDir::new().expect("temp root");
        let root = dir.path().canonicalize().expect("canonicalize temp root");
        let config = resolve(
            Some(&settings(
                Some(true),
                None,
                Some(vec!["/tmp/allowed"]),
                Some(vec!["/tmp/escape/../../.."]), // -> /, dropped
            )),
            &root.join("work"),
            &root.join("agent"),
        )
        .expect("config");
        assert!(
            config.denied_paths.is_empty(),
            "a denied path that normalizes to '/' is dropped: {config:?}"
        );
    }

    /// `validate` is the fail-closed gate for directly-constructed configs:
    /// it must reject any allowed/denied path carrying `..`/`.`/prefix or that
    /// is non-absolute or the filesystem root — the shapes that would let
    /// `$ROOT$path` escape the private root during staging. A clean absent
    /// denied child is still accepted.
    #[cfg(target_os = "linux")]
    #[test]
    fn validate_rejects_paths_with_traversal_or_anomalies() {
        let allowed = Path::new("/tmp/allowed");
        let base = SandboxConfig {
            enabled: true,
            network: false,
            allowed_paths: vec![allowed.to_path_buf()],
            denied_paths: Vec::new(),
            read_only: false,
        };
        let reject_denied = |denied: &str, needle: &str| {
            let mut config = base.clone();
            config.denied_paths = vec![PathBuf::from(denied)];
            let error = config.validate(allowed).expect_err(denied);
            assert!(error.to_string().contains(needle), "{denied}: expected {needle:?} in: {error}");
        };
        // Traversal (`..`) in denied and allowed paths is the P1 shape.
        reject_denied("/tmp/allowed/../sibling", "'..'");
        let config = SandboxConfig {
            enabled: true,
            network: false,
            allowed_paths: vec![PathBuf::from("/tmp/allowed/../sibling")],
            denied_paths: Vec::new(),
            read_only: false,
        };
        let error = config.validate(Path::new("/tmp/allowed/../sibling")).expect_err("traversal allowed");
        assert!(error.to_string().contains("'..'"), "got: {error}");
        // Non-absolute and root are degenerate transports too. (`.` components
        // are not a traversal risk and are not observable via `Path::components`;
        // the setup script rejects them as defense-in-depth — see
        // `setup_script_rejects_traversal_paths_before_mounting`.)
        reject_denied("relative/denied", "is not absolute");
        reject_denied("/", "filesystem root");
        // A clean denied path (the valid absent-denied-child case) is accepted.
        let mut clean = base.clone();
        clean.denied_paths = vec![PathBuf::from("/tmp/allowed/secret")];
        clean.validate(allowed).expect("a clean absolute denied path is accepted");
    }

    /// Regression for the 0.2.4 P1: a denied path that lexically escapes its
    /// intended parent toward a host sentinel directory must be rejected at
    /// the config boundary BEFORE any spawn — no staging `mkdir`/`mount` may
    /// touch the host, and no sentinel directory may come into existence.
    /// Works on any kernel: `validate()` fails closed inside `build_command`
    /// before namespaces are created.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn denied_traversal_path_is_rejected_before_spawn_and_creates_no_host_dir() {
        let dir = tempfile::TempDir::new().expect("allowed parent temp dir");
        let allowed = dir.path().canonicalize().expect("canonicalize allowed parent");
        // Host sentinel: a directory beside the allowed parent that the
        // traversal would target. It must not pre-exist and must never be
        // created by the sandbox staging. The denied path is built to
        // lexically resolve to EXACTLY this sentinel so the assertion below
        // checks the real traversal target, not an unrelated path.
        let sentinel_name = format!(
            "pi-traversal-sentinel-{}",
            allowed.file_name().expect("name").to_string_lossy()
        );
        let sentinel = allowed
            .parent()
            .expect("allowed parent has a parent")
            .join(&sentinel_name);
        let _ = std::fs::remove_dir(&sentinel);
        // Denied path that lexically carries `..` into the sentinel — the bug
        // shape: nonexistent denied paths are never canonicalized, so the `..`
        // used to reach the shell and escape `$ROOT$path` during staging.
        let denied = allowed.join("..").join(&sentinel_name);
        assert!(
            denied
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir)),
            "the regression denied path must carry a literal `..`"
        );
        let config = SandboxConfig {
            enabled: true,
            network: false,
            allowed_paths: vec![allowed.clone()],
            denied_paths: vec![denied.clone()],
            read_only: false,
        };
        // 1. Config validation rejects the traversal before any spawn.
        let error = config
            .validate(&allowed)
            .expect_err("a traversal denied path must fail validation");
        assert!(error.to_string().contains(".."), "validation must name the traversal: {error}");
        // 2. The runner fails closed before spawning (validate() runs first).
        let err = run_in_sandbox(
            Some(&config),
            &allowed,
            &["/bin/true".to_owned()],
            Vec::new(),
            None,
            AbortSignal::none(),
            None,
        )
        .await
        .expect_err("sandboxed run must fail closed on traversal before spawn");
        assert!(err.to_string().contains(".."), "runner error must name the traversal: {err}");
        // 3. No host directory was created by the rejected spawn.
        assert!(
            !sentinel.exists(),
            "the traversal must never create the host sentinel: {}",
            sentinel.display()
        );
        let _ = std::fs::remove_dir(&sentinel);
    }

    /// Defense-in-depth: the setup script itself rejects any transported path
    /// carrying a `..` (or `.`) component BEFORE a single mount runs, so even
    /// a `..` that bypassed the Rust-side normalization can never make
    /// `$ROOT$path` escape the private root during staging. Runs the script
    /// directly with `/bin/sh`; no namespaces are required because the
    /// validation precedes the first `mount`.
    #[cfg(target_os = "linux")]
    #[test]
    fn setup_script_rejects_traversal_paths_before_mounting() {
        let run = |code: u8, args: &[&str]| {
            let out = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(SANDBOX_SETUP_SCRIPT)
                .arg(args[0])
                .args(&args[1..])
                .output()
                .expect("run setup script");
            let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
            assert_eq!(
                out.status.code(),
                Some(i32::from(code)),
                "expected exit {code}, got {:?}: {stderr}",
                out.status.code()
            );
            stderr
        };
        // A denied path with a `..` component is rejected (exit 70) before the
        // mount step, with a message naming the traversal — never exit 90/95.
        let stderr = run(70, &[
            "pi-sandbox",
            "1",
            "1",
            "/tmp/allowed",
            "/tmp/allowed/../escape",
            "--",
            "/bin/true",
        ]);
        assert!(stderr.contains("'..'"), "the script must name the traversal: {stderr}");
        // A `.` component is rejected the same way.
        let stderr = run(70, &[
            "pi-sandbox",
            "1",
            "0",
            "/tmp/allowed/./sub",
            "--",
            "/bin/true",
        ]);
        assert!(stderr.contains("'.'"), "the script must name the curdir: {stderr}");
        // A clean transport still proceeds to the mount step (exit 90 when
        // mount privileges are unavailable) — the validation is not a false
        // positive on legitimate paths.
        let mount_denied = std::process::Command::new("mount")
            .args(["--make-rprivate", "/"])
            .status();
        if matches!(mount_denied, Ok(status) if !status.success()) {
            let stderr = run(90, &[
                "pi-sandbox",
                "1",
                "1",
                "/tmp/allowed",
                "/tmp/allowed/secret",
                "--",
                "/bin/true",
            ]);
            assert!(
                !stderr.contains("malformed transport"),
                "clean paths must pass the validation: {stderr}"
            );
        }
    }

    /// A valid absent denied child (nonexistent, no traversal) must still be
    /// accepted by the new clean-path gate and travel to the setup script, so
    /// the existing fail-closed semantics (exit 95 when the absent path is
    /// covered by an allowed bind) are preserved. The live exit-95 behavior is
    /// exercised by `denied_absent_under_allowed_parent_never_leaks_to_host`;
    /// here we assert the gate no longer rejects the valid shape.
    #[cfg(target_os = "linux")]
    #[test]
    fn valid_absent_denied_child_is_accepted_by_clean_path_gate() {
        let dir = tempfile::TempDir::new().expect("allowed parent temp dir");
        let allowed = dir.path().canonicalize().expect("canonicalize allowed parent");
        let denied = allowed.join("secret"); // absent, clean
        assert!(!denied.exists(), "the denied child starts absent");
        let config = SandboxConfig {
            enabled: true,
            network: false,
            allowed_paths: vec![allowed.clone()],
            denied_paths: vec![denied.clone()],
            read_only: false,
        };
        config
            .validate(&allowed)
            .expect("a clean absent denied child validates");
        match config.wrapper_command(&allowed, &["/bin/true".to_owned()]) {
            Ok(argv) => {
                assert!(
                    argv.iter().any(|arg| *arg == denied.to_string_lossy()),
                    "the absent denied child must travel to the setup script: {argv:?}"
                );
            }
            Err(error) => {
                // The only legitimate failure is the missing-unshare fail-closed
                // error, never a clean-path rejection.
                assert!(error.to_string().contains("unshare"), "got: {error}");
            }
        }
    }

    /// Static ordering guard for the sandbox P1 fix: the setup script must
    /// mount allowed paths in two passes — a staging pass that pre-creates every
    /// `$ROOT$p` mountpoint on the unbound tmpfs root, and a later bind pass that
    /// attaches the host path. A single pass that creates a mountpoint and binds
    /// it in the same loop lets a bind onto a parent (e.g. /foo) funnel the
    /// `: > "$ROOT$p"`/`mkdir -p "$ROOT$p"` staging for a later nested allowed
    /// path (e.g. /foo/bar/baz.txt) through the parent bind onto the real host
    /// file, truncating/creating host content before the confined command runs.
    /// Guards the script text directly: no namespaces are needed because the
    /// ordering is structural, not behavioral.
    #[cfg(target_os = "linux")]
    #[test]
    fn setup_script_stages_allowed_mountpoints_before_any_allowed_bind() {
        let script = SANDBOX_SETUP_SCRIPT;
        // The two passes are fenced by unique code-adjacent markers so the
        // slices below cover the ACTUAL staging subshell and bind loop, not
        // the explanatory preamble above them.
        let stage_begin = script
            .find("# >>> allowed-staging-begin")
            .expect("allowed staging begin marker present");
        let stage_end = script
            .find("# >>> allowed-staging-end")
            .expect("allowed staging end marker present");
        let bind_begin = script
            .find("# >>> allowed-bind-begin")
            .expect("allowed bind begin marker present");
        let bind_end = script
            .find("# >>> allowed-bind-end")
            .expect("allowed bind end marker present");
        assert!(
            stage_begin < stage_end && bind_begin < bind_end && stage_end < bind_begin,
            "allowed mountpoint staging must precede the allowed bind so a parent bind cannot funnel staging onto the host"
        );
        // Pass 1 (staging) must create mountpoints and must NOT bind. It must
        // also run before the system host binds, so a system bind (whose
        // read-only remount can silently fail, leaving a writable parent)
        // can never expose host content to allowed staging. Anchor on the
        // actual system bind line, not the `for` header.
        let staging = &script[stage_begin..stage_end];
        let system_bind = script
            .find("mount --bind \"$d\" \"$ROOT$d\"")
            .expect("system bind mount line present");
        assert!(
            stage_end < system_bind,
            "allowed staging must run before the system host binds (mount --bind \"$d\"), not just before the allowed bind"
        );
        assert!(
            staging.contains("mkdir -p \"$ROOT$p\"") && staging.contains(": > \"$ROOT$p\""),
            "the staging pass must create directory and file mountpoints"
        );
        assert!(
            !staging.contains("mount --bind \"$p\" \"$ROOT$p\""),
            "the staging pass must not bind allowed paths"
        );
        // Pass 2 (bind) must not create or truncate targets — staging belongs
        // to the earlier subshell.
        let binding = &script[bind_begin..bind_end];
        assert!(
            binding.contains("mount --bind \"$p\" \"$ROOT$p\""),
            "the bind pass must bind allowed paths onto their pre-created mountpoints"
        );
        assert!(
            !binding.contains(": > \"$ROOT$p\"") && !binding.contains("mkdir -p \"$ROOT$p\""),
            "the bind pass must not create/truncate targets; staging belongs to the earlier subshell: {binding}"
        );
    }

    /// Live regression for the sandbox P1: an allowed parent plus a nested
    /// existing host file carrying sentinel content. The old single-pass
    /// allowed loop bound the parent (`/parent`) onto `$ROOT/parent` FIRST,
    /// which made `$ROOT/parent/nested/deep.txt` resolve through that bind
    /// onto the host file; the later `: > "$ROOT$p"` staging for the nested
    /// allowed path then TRUNCATED the host file before the confined command
    /// ran, and the nested bind ended up exposing an empty file. The two-pass
    /// split pre-creates the mountpoint on the unbound tmpfs root, so the host
    /// file is never touched during staging and the nested bind stays usable.
    /// On hosts where namespace setup itself fails, the fail-closed path
    /// (exit 1 or 90-99) still proves the host file was never modified: setup
    /// aborts at its first mount, before any allowed staging.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "live confinement test: requires Linux unprivileged user namespaces (run with --include-ignored on a supported kernel)"]
    async fn allowed_nested_host_file_survives_staging_and_stays_usable() {
        let dir = tempfile::TempDir::new().expect("allowed parent temp dir");
        // Canonicalize so the sandbox bind matches the path the config names.
        let allowed = dir.path().canonicalize().expect("canonicalize allowed parent");
        // Nested existing host file carrying a sentinel the bug would erase.
        let nested_dir = allowed.join("nested");
        std::fs::create_dir_all(&nested_dir).expect("mkdir nested dir");
        let nested = nested_dir.join("deep.txt");
        std::fs::write(&nested, "SENTINEL").expect("write sentinel host file");
        // Both the parent and the nested file are allowed: this is exactly the
        // ordering that let the old single-pass loop truncate the host file.
        let config = SandboxConfig {
            enabled: true,
            network: false,
            allowed_paths: vec![allowed.clone(), nested.clone()],
            denied_paths: Vec::new(),
            read_only: false,
        };
        // The confined command reads the nested file through the bind and
        // writes the result into the (bound, writable) allowed parent, where
        // it is observable on the host after the run. Under the bug the file
        // was already truncated during staging, so this would be empty.
        let result = allowed.join("result.txt");
        let _ = std::fs::remove_file(&result);
        let argv = vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "cat nested/deep.txt > result.txt".to_owned(),
        ];
        let outcome = run_in_sandbox(
            Some(&config),
            &allowed,
            &argv,
            Vec::new(),
            None,
            AbortSignal::none(),
            None,
        )
        .await
        .expect("sandboxed run must not error at the runner level");
        match outcome.exit_code {
            Some(0) => {}
            Some(code) if code == 1 || (90..=99).contains(&code) => {
                // Host cannot create the namespaces (or the launcher is
                // missing): the confined command never ran, which still
                // proves staging never touched the host file.
                eprintln!(
                    "allowed-nested live-run: sandbox namespace setup unavailable here (exit {code}); the fail-closed path still proved the host file was untouched"
                );
                assert_eq!(
                    std::fs::read_to_string(&nested).expect("read host sentinel"),
                    "SENTINEL",
                    "the host sentinel must survive a fail-closed setup abort"
                );
                assert!(!result.exists(), "the confined command never ran, so no result file was produced");
                return;
            }
            other => panic!("unexpected sandbox outcome: {other:?}"),
        }
        // The host sentinel must survive staging intact (the P1 fix).
        assert_eq!(
            std::fs::read_to_string(&nested).expect("read host sentinel"),
            "SENTINEL",
            "staging must never truncate the nested host file: {}",
            nested.display()
        );
        // The nested bind must stay usable: the confined command read the
        // sentinel through it and wrote it back to the allowed parent.
        assert_eq!(
            std::fs::read_to_string(&result).expect("read captured result"),
            "SENTINEL",
            "the nested allowed bind must expose the real host content to the confined command"
        );
        let _ = std::fs::remove_file(&result);
    }
}
