//! Governed non-PTY foreground Bash execution for confined sandbox modes.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use pi_agent::AbortSignal;

pub(crate) struct ForegroundRunOutput {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub cancelled: bool,
    pub execution_elapsed: Option<Duration>,
}

const CONFINED_RESERVED_ENV_PREFIXES: [&str; 16] = [
    "PI_",
    "RPI_",
    "OPENAI_",
    "ANTHROPIC_",
    "GOOGLE_",
    "GEMINI_",
    "AZURE_",
    "AWS_",
    "GITHUB_",
    "GH_",
    "GITLAB_",
    "NPM_",
    "COHERE_",
    "MISTRAL_",
    "GROQ_",
    "XAI_",
];

const CONFINED_BASELINE_ENV_KEYS: [&str; 10] = [
    "PATH",
    "HOME",
    "TMPDIR",
    "PI_SANDBOX_SCRATCH",
    "USER",
    "LOGNAME",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TZ",
];

#[cfg(unix)]
const FOREGROUND_OUTPUT_GRACE: Duration = Duration::from_millis(250);
#[cfg(unix)]
const FOREGROUND_WAIT_GRACE: Duration = Duration::from_secs(2);

#[cfg(unix)]
struct ProcessGroupGuard {
    process_group: Option<i32>,
}

#[cfg(unix)]
impl ProcessGroupGuard {
    fn new(process_group: i32) -> Self {
        Self {
            process_group: Some(process_group),
        }
    }

    fn disarm(&mut self) {
        self.process_group = None;
    }
}

#[cfg(unix)]
impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if let Some(process_group) = self.process_group.take() {
            terminate_process_group(process_group);
        }
    }
}

/// Admission identity for a confined bash spawn. Approval digests include
/// `timeout_ms`, so an omitted stamp would make every timeout share one grant.
#[must_use]
pub(crate) fn confined_spawn_timeout_ms(timeout: Option<Duration>) -> Option<u64> {
    timeout.map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
}

/// Ordinary requested variables that confined construction can accept. Host
/// ambient values, reserved credential prefixes, and confined-baseline keys
/// are dropped here so spawn authorization stays fail-closed without
/// forwarding the parent environment as an override set.
#[must_use]
pub(crate) fn is_confined_safe_requested_name(name: &str) -> bool {
    if pi_sandbox::EnvironmentVariableName::parse(name.to_owned()).is_err() {
        return false;
    }
    let upper = name.to_ascii_uppercase();
    if CONFINED_RESERVED_ENV_PREFIXES
        .iter()
        .any(|prefix| upper.starts_with(prefix))
    {
        return false;
    }
    !CONFINED_BASELINE_ENV_KEYS
        .iter()
        .any(|key| key.eq_ignore_ascii_case(name))
}

#[must_use]
pub(crate) fn is_confined_safe_pty_requested_name(name: &str) -> bool {
    is_confined_safe_requested_name(name)
        && !name.eq_ignore_ascii_case("TERM")
        && !name.eq_ignore_ascii_case("COLORTERM")
}

#[must_use]
pub(crate) fn confined_safe_requested_env(
    env: &[(String, String)],
) -> BTreeMap<OsString, Option<OsString>> {
    env.iter()
        .filter(|(name, value)| is_confined_safe_requested_name(name) && !value.contains('\0'))
        .map(|(name, value)| (OsString::from(name), Some(OsString::from(value))))
        .collect()
}

/// Runs a finite shell command through the captured generation's governed
/// merged-pipe process boundary. Confined callers never fall back to the host.
#[cfg(unix)]
pub(crate) async fn run_confined_command(
    generation: Arc<crate::application::generation::ApplicationGeneration>,
    auth_manager: Option<Arc<crate::AuthManager>>,
    cwd: &Path,
    argv: &[String],
    env: &[(String, String)],
    timeout: Option<Duration>,
    abort: AbortSignal,
    stream: Arc<dyn Fn(&[u8]) + Send + Sync>,
) -> Result<ForegroundRunOutput> {
    use std::io::Read as _;
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;

    use crate::sandbox::{CredentialAudience, RawSpawnRequest, SpawnAuthorization};

    if abort.is_aborted() {
        return Ok(ForegroundRunOutput {
            exit_code: None,
            timed_out: false,
            cancelled: true,
            execution_elapsed: None,
        });
    }
    let (program, _) = argv
        .split_first()
        .ok_or_else(|| anyhow!("foreground shell argv is empty"))?;
    let is_danger = generation.sandbox().policy_snapshot().policy().mode()
        == pi_sandbox::SandboxMode::DangerFullAccess;
    let executable = generation.sandbox().capabilities().open_managed_read_file(
        crate::fs::CapabilityRootPurpose::ToolExecutable,
        pi_sandbox::AbsolutePath::parse(Path::new(program).to_path_buf())?,
    )?;
    let executable_path = executable.resolved_path().to_path_buf();
    let retained_executable = (!is_danger)
        .then(|| executable.into_file().duplicate_for_spawn_executable())
        .transpose()?;
    let retained_working_directory = (!is_danger)
        .then(|| {
            generation
                .sandbox()
                .fs()
                .confined()?
                .duplicate_for_spawn_working_directory(cwd)
        })
        .transpose()?;
    let family = pi_sandbox::SpawnFamily::ManagedPipeProcess;
    let authorized = crate::process::authorize_spawn_with_auth(
        generation.clone(),
        auth_manager.as_deref(),
        RawSpawnRequest {
            authorization: SpawnAuthorization::Captured(generation.sandbox().clone()),
            family,
            audience: CredentialAudience::new_spawn_family(family)?,
            argv: argv
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    if index == 0 {
                        executable_path.as_os_str().to_owned()
                    } else {
                        OsString::from(value)
                    }
                })
                .collect(),
            cwd: cwd.to_path_buf(),
            requested_env: if is_danger {
                env.iter()
                    .map(|(name, value)| (OsString::from(name), Some(OsString::from(value))))
                    .collect()
            } else {
                confined_safe_requested_env(env)
            },
            requested_grants: BTreeSet::new(),
            stdio: pi_sandbox::StdioMode::Merged,
            terminal_size: None,
            timeout_ms: confined_spawn_timeout_ms(timeout),
            target_fd_mappings: Vec::new(),
            owned_target_fd_mappings: Vec::new(),
            mount_sources: Vec::new(),
            retained_executable,
            retained_working_directory,
        },
    )
    .await?;
    #[cfg(test)]
    assert_eq!(
        authorized.timeout_ms(),
        confined_spawn_timeout_ms(timeout),
        "confined bash spawn admission must stamp the configured timeout"
    );

    let (parent_stdin, child_stdin) = UnixStream::pair()?;
    let (parent_output, child_output) = UnixStream::pair()?;
    let output_shutdown = parent_output.try_clone()?;
    let child_stderr = child_output.try_clone()?;
    let stdio = pi_sandbox::PipeStdio::new(
        OwnedFd::from(child_stdin),
        OwnedFd::from(child_output),
        OwnedFd::from(child_stderr),
    )?;
    let (sandbox, request) =
        authorized.into_prepared_request(pi_sandbox::SpawnStdio::Merged(stdio))?;
    let launched =
        pi_sandbox::launch_prepared_unix(pi_sandbox::prepare_spawn(sandbox.backend(), request)?)?;
    let process = match launched {
        pi_sandbox::LaunchedUnixHandle::Pipe(process) => process,
        pi_sandbox::LaunchedUnixHandle::Pty(pty) => {
            let (process, _master) = pty.into_parts();
            terminate_process_group(process.process_group());
            let _ = process.wait();
            return Err(anyhow!(
                "governed non-PTY shell unexpectedly returned a PTY"
            ));
        }
    };
    // Execution starts the moment the child is launched: the measured window
    // below is the command's real run time. The caller's queue wait is never
    // on this path (confined runs do not take the brush lock), and the
    // configured timeout bounds only this window.
    let execution_started = Instant::now();
    drop(parent_stdin);

    let process_group = process.process_group();
    let mut process_guard = ProcessGroupGuard::new(process_group);
    let mut reader_task = tokio::task::spawn_blocking(move || {
        let mut reader = parent_output;
        let mut buffer = [0_u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => return,
                Ok(count) => stream(&buffer[..count]),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return,
            }
        }
    });
    let mut wait_task = tokio::task::spawn_blocking(move || process.wait());
    let timeout_future = async {
        match timeout {
            Some(duration) => tokio::time::sleep(duration).await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::pin!(timeout_future);

    enum Race {
        Exited(pi_sandbox::UnixExitStatus),
        TimedOut,
        Cancelled,
    }
    let race = tokio::select! {
        biased;
        () = abort.cancelled() => Race::Cancelled,
        () = &mut timeout_future => Race::TimedOut,
        status = &mut wait_task => Race::Exited(
            status
                .map_err(|_| anyhow!("governed foreground shell waiter failed"))??,
        ),
    };
    // Freeze the execution window at the race resolution; the termination and
    // drain cleanup below is not part of the command's run time.
    let execution_elapsed = execution_started.elapsed();
    let outcome = match race {
        Race::Exited(status) => {
            process_guard.disarm();
            ForegroundRunOutput {
                exit_code: Some(status.shell_code()),
                timed_out: false,
                cancelled: false,
                execution_elapsed: Some(execution_elapsed),
            }
        }
        Race::TimedOut | Race::Cancelled => {
            terminate_process_group(process_group);
            process_guard.disarm();
            let _ = tokio::time::timeout(FOREGROUND_WAIT_GRACE, &mut wait_task).await;
            drain_output_reader(&mut reader_task, &output_shutdown).await;
            // Abort wins over timeout once cleanup finishes (matches the
            // sandbox runner): an abort landing during termination/drain
            // reclassifies the outcome as cancelled.
            if abort.is_aborted() {
                return Ok(ForegroundRunOutput {
                    exit_code: None,
                    timed_out: false,
                    cancelled: true,
                    execution_elapsed: Some(execution_elapsed),
                });
            }
            return Ok(ForegroundRunOutput {
                exit_code: None,
                timed_out: true,
                cancelled: false,
                execution_elapsed: Some(execution_elapsed),
            });
        }
    };
    drain_output_reader(&mut reader_task, &output_shutdown).await;
    process_guard.disarm();
    Ok(outcome)
}

#[cfg(unix)]
fn terminate_process_group(process_group: i32) {
    use nix::sys::signal::{Signal, killpg};
    use nix::unistd::Pid;

    let _ = killpg(Pid::from_raw(process_group), Signal::SIGKILL);
}

#[cfg(unix)]
async fn drain_output_reader(
    reader_task: &mut tokio::task::JoinHandle<()>,
    output_shutdown: &std::os::unix::net::UnixStream,
) {
    if tokio::time::timeout(FOREGROUND_OUTPUT_GRACE, &mut *reader_task)
        .await
        .is_ok()
    {
        return;
    }
    let _ = output_shutdown.shutdown(std::net::Shutdown::Both);
    let _ = tokio::time::timeout(FOREGROUND_OUTPUT_GRACE, reader_task).await;
}

#[cfg(not(unix))]
pub(crate) async fn run_confined_command(
    _generation: Arc<crate::application::generation::ApplicationGeneration>,
    _auth_manager: Option<Arc<crate::AuthManager>>,
    _cwd: &Path,
    _argv: &[String],
    _env: &[(String, String)],
    _timeout: Option<Duration>,
    _abort: AbortSignal,
    _stream: Arc<dyn Fn(&[u8]) + Send + Sync>,
) -> Result<ForegroundRunOutput> {
    Err(anyhow!(
        "confined non-PTY foreground shell execution is unavailable on this platform"
    ))
}

#[cfg(test)]
mod tests {
    use super::confined_spawn_timeout_ms;
    use std::time::Duration;

    #[test]
    fn confined_spawn_timeout_ms_stamps_configured_timeout() {
        assert_eq!(confined_spawn_timeout_ms(None), None);
        assert_eq!(
            confined_spawn_timeout_ms(Some(Duration::from_millis(400))),
            Some(400)
        );
        assert_eq!(
            confined_spawn_timeout_ms(Some(Duration::from_secs(2))),
            Some(2_000)
        );
    }
}
