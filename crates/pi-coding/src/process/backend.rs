use std::io::{self, Read, Write};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use parking_lot::Mutex;
use tokio::sync::{mpsc, oneshot, watch};

use super::{AuthorizedSpawn, ProcessSignal, ProcessStream, ProcessTerminalSize};

const PROCESS_CHANNEL_CAPACITY: usize = 128;

pub(super) struct BackendProcess {
    pub pid: Option<u32>,
    pub controller: Arc<BackendController>,
    pub output_rx: mpsc::Receiver<(ProcessStream, Vec<u8>)>,
    pub exit_rx: oneshot::Receiver<i32>,
}

pub(super) struct BackendController {
    pid: Option<u32>,
    process_group: Option<i32>,
    stdin: Mutex<Option<mpsc::Sender<Vec<u8>>>>,
    pty: Mutex<Option<BackendPty>>,
    #[cfg(windows)]
    fallback_killer: Mutex<Option<Box<dyn portable_pty::ChildKiller + Send + Sync>>>,
    #[cfg(unix)]
    output_shutdown: Mutex<Option<std::os::unix::net::UnixStream>>,
}

#[cfg(unix)]
enum BackendPty {
    Confined(Arc<pi_sandbox::UnixPtyMaster>),
}

#[cfg(windows)]
enum BackendPty {
    Portable(Box<dyn portable_pty::MasterPty + Send>),
}

impl std::fmt::Debug for BackendController {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BackendController")
            .field("pid", &self.pid)
            .field("process_group", &self.process_group)
            .field("stdin_open", &self.stdin.lock().is_some())
            .field("pty", &self.pty.lock().is_some())
            .finish()
    }
}

impl BackendController {
    pub async fn write(&self, bytes: Vec<u8>) -> Result<()> {
        let sender = self
            .stdin
            .lock()
            .clone()
            .ok_or_else(|| anyhow!("process stdin is closed"))?;
        sender
            .send(bytes)
            .await
            .map_err(|_| anyhow!("process stdin is closed"))
    }

    pub fn close_stdin(&self) {
        self.stdin.lock().take();
    }

    pub fn close_output(&self) {
        #[cfg(unix)]
        if let Some(stream) = self.output_shutdown.lock().take() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }

    pub fn resize(&self, size: ProcessTerminalSize) -> Result<()> {
        let pty = self.pty.lock();
        let pty = pty
            .as_ref()
            .ok_or_else(|| anyhow!("process is not attached to a PTY"))?;
        match pty {
            #[cfg(unix)]
            BackendPty::Confined(master) => master.resize(sandbox_terminal_size(size)?)?,
            #[cfg(windows)]
            BackendPty::Portable(master) => master.resize(portable_pty::PtySize {
                rows: size.rows,
                cols: size.cols,
                pixel_width: 0,
                pixel_height: 0,
            })?,
        }
        Ok(())
    }

    pub fn signal(&self, signal: ProcessSignal) -> Result<()> {
        #[cfg(unix)]
        if let Some(group) = self.process_group {
            use nix::sys::signal::{Signal, killpg};
            use nix::unistd::Pid;

            let native = match signal {
                ProcessSignal::Sigint => Signal::SIGINT,
                ProcessSignal::Sigterm => Signal::SIGTERM,
                ProcessSignal::Sighup => Signal::SIGHUP,
                ProcessSignal::Sigquit => Signal::SIGQUIT,
                ProcessSignal::Sigkill => Signal::SIGKILL,
            };
            return match killpg(Pid::from_raw(group), native) {
                Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
                Err(error) => Err(error.into()),
            };
        }

        #[cfg(windows)]
        if signal == ProcessSignal::Sigkill {
            if let Some(killer) = self.fallback_killer.lock().as_mut() {
                killer.kill()?;
            }
            return Ok(());
        }

        Err(anyhow!(
            "signal {signal:?} is not supported by this process backend"
        ))
    }
}

pub(super) async fn spawn(authorized: AuthorizedSpawn) -> Result<BackendProcess> {
    #[cfg(unix)]
    {
        if authorized.stdio() == pi_sandbox::StdioMode::Pty {
            return spawn_pty(authorized).await;
        }
        spawn_pipe(authorized).await
    }
    #[cfg(windows)]
    {
        if authorized.stdio() == pi_sandbox::StdioMode::Pty {
            return spawn_windows_danger_pty(authorized);
        }
        let child = spawn_windows_danger_pipe(authorized)
            .map_err(WindowsDangerPipeLaunchError::into_anyhow)?;
        adapt_windows_danger_pipe(child).await
    }
}

#[cfg(unix)]
async fn spawn_pipe(authorized: AuthorizedSpawn) -> Result<BackendProcess> {
    let managed = managed_pipe_stdio()?;
    let (sandbox, request) =
        authorized.into_prepared_request(pi_sandbox::SpawnStdio::Merged(managed.stdio))?;
    let prepared = pi_sandbox::prepare_spawn(sandbox.backend(), request)?;
    let launched = pi_sandbox::launch_prepared_unix(prepared)?;
    adapt_launched_unix(
        launched,
        Some(managed.parent_stdin),
        Some(managed.parent_output),
        Some(managed.output_shutdown),
    )
}

#[cfg(unix)]
async fn spawn_pty(authorized: AuthorizedSpawn) -> Result<BackendProcess> {
    let size = authorized.terminal_size().unwrap_or_default();
    let (sandbox, request) = authorized.into_prepared_request(pi_sandbox::SpawnStdio::Pty(size))?;
    let prepared = pi_sandbox::prepare_spawn(sandbox.backend(), request)?;
    let launched = pi_sandbox::launch_prepared_unix(prepared)?;
    adapt_launched_unix(launched, None, None, None)
}

pub(super) fn sandbox_terminal_size(size: ProcessTerminalSize) -> Result<pi_sandbox::TerminalSize> {
    pi_sandbox::TerminalSize::new(size.rows, size.cols).map_err(Into::into)
}

#[cfg(unix)]
struct ManagedPipeStdio {
    stdio: pi_sandbox::PipeStdio,
    parent_stdin: std::fs::File,
    parent_output: std::fs::File,
    output_shutdown: std::os::unix::net::UnixStream,
}

#[cfg(unix)]
fn managed_pipe_stdio() -> Result<ManagedPipeStdio> {
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;

    let (parent_stdin, child_stdin) = UnixStream::pair()?;
    let (parent_output, child_output) = UnixStream::pair()?;
    let output_shutdown = parent_output.try_clone()?;
    let child_stderr = child_output.try_clone()?;
    let stdio = pi_sandbox::PipeStdio::new(
        OwnedFd::from(child_stdin),
        OwnedFd::from(child_output),
        OwnedFd::from(child_stderr),
    )?;
    Ok(ManagedPipeStdio {
        stdio,
        parent_stdin: std::fs::File::from(OwnedFd::from(parent_stdin)),
        parent_output: std::fs::File::from(OwnedFd::from(parent_output)),
        output_shutdown,
    })
}

#[cfg(unix)]
fn adapt_launched_unix(
    launched: pi_sandbox::LaunchedUnixHandle,
    parent_stdin: Option<std::fs::File>,
    parent_output: Option<std::fs::File>,
    output_shutdown: Option<std::os::unix::net::UnixStream>,
) -> Result<BackendProcess> {
    let (process, pty, stdin_writer, output_reader) = match launched {
        pi_sandbox::LaunchedUnixHandle::Pipe(process) => {
            (process, None, parent_stdin, parent_output)
        }
        pi_sandbox::LaunchedUnixHandle::Pty(pty) => {
            let (process, master) = pty.into_parts();
            let master = Arc::new(master);
            let stdin = master.try_clone_writer()?;
            let output = master.try_clone_reader()?;
            (
                process,
                Some(BackendPty::Confined(master)),
                Some(stdin),
                Some(output),
            )
        }
    };
    let pid = Some(process.pid());
    let process_group = Some(process.process_group());
    let (stdin_tx, stdin_rx) = mpsc::channel(PROCESS_CHANNEL_CAPACITY);
    let (output_tx, output_rx) = mpsc::channel(PROCESS_CHANNEL_CAPACITY);
    let (exit_tx, exit_rx) = oneshot::channel();

    if let Some(writer) = stdin_writer {
        spawn_stdin_writer(writer, stdin_rx);
    }
    if let Some(reader) = output_reader {
        spawn_output_reader(reader, output_tx, ProcessStream::Combined);
    }
    tokio::task::spawn_blocking(move || {
        let code = process
            .wait()
            .map_or(-1, pi_sandbox::UnixExitStatus::shell_code);
        let _ = exit_tx.send(code);
    });

    let controller = Arc::new(BackendController {
        pid,
        process_group,
        stdin: Mutex::new(Some(stdin_tx)),
        pty: Mutex::new(pty),
        output_shutdown: Mutex::new(output_shutdown),
    });
    Ok(BackendProcess {
        pid,
        controller,
        output_rx,
        exit_rx,
    })
}

#[cfg(unix)]
fn spawn_stdin_writer(mut writer: std::fs::File, mut receiver: mpsc::Receiver<Vec<u8>>) {
    tokio::task::spawn_blocking(move || {
        while let Some(bytes) = receiver.blocking_recv() {
            if writer.write_all(&bytes).is_err() || writer.flush().is_err() {
                break;
            }
        }
    });
}

#[cfg(unix)]
fn spawn_output_reader(
    mut reader: std::fs::File,
    sender: mpsc::Sender<(ProcessStream, Vec<u8>)>,
    stream: ProcessStream,
) {
    tokio::task::spawn_blocking(move || {
        let mut buffer = [0_u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    if sender
                        .blocking_send((stream, buffer[..read].to_vec()))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });
}
pub(crate) struct GovernedChild {
    pid: u32,
    controller: Arc<BackendController>,
    exit: watch::Receiver<Option<Result<i32, pi_sandbox::SandboxError>>>,
    stdin: Option<tokio::fs::File>,
    stdout: Option<tokio::fs::File>,
    stderr: Option<tokio::fs::File>,
}

impl GovernedChild {
    pub(crate) const fn pid(&self) -> u32 {
        self.pid
    }

    pub(crate) fn take_stdin(&mut self) -> Option<tokio::fs::File> {
        self.stdin.take()
    }

    pub(crate) fn take_stdout(&mut self) -> Option<tokio::fs::File> {
        self.stdout.take()
    }

    pub(crate) fn take_stderr(&mut self) -> Option<tokio::fs::File> {
        self.stderr.take()
    }

    pub(crate) fn exit_status(&self) -> Option<Result<i32, pi_sandbox::SandboxError>> {
        self.exit.borrow().clone()
    }

    pub(crate) fn try_wait(&self) -> Result<Option<i32>> {
        match self.exit_status() {
            Some(Ok(code)) => Ok(Some(code)),
            Some(Err(error)) => Err(error.into()),
            None => Ok(None),
        }
    }

    pub(crate) async fn kill(&mut self) -> Result<()> {
        self.signal(ProcessSignal::Sigkill)
    }
    pub(crate) fn signal(&self, signal: ProcessSignal) -> Result<()> {
        self.controller.signal(signal)
    }

    pub(crate) async fn wait(&mut self) -> Result<i32, pi_sandbox::SandboxError> {
        loop {
            if let Some(outcome) = self.exit.borrow_and_update().clone() {
                return outcome;
            }
            if self.exit.changed().await.is_err() {
                return Err(pi_sandbox::SandboxError::System {
                    operation: pi_sandbox::SandboxIoOperation::WaitChild,
                    kind: pi_sandbox::SandboxIoKind::BrokenPipe,
                    raw_os_error: None,
                });
            }
        }
    }

    pub(crate) async fn shutdown(
        mut self,
        grace: Duration,
    ) -> Result<i32, pi_sandbox::SandboxError> {
        self.stdin.take();
        self.stdout.take();
        self.stderr.take();
        let _ = self.signal(ProcessSignal::Sigterm);
        match tokio::time::timeout(grace, self.wait()).await {
            Ok(outcome) => outcome,
            Err(_) => {
                let _ = self.signal(ProcessSignal::Sigkill);
                self.wait().await
            }
        }
    }
}

impl Drop for GovernedChild {
    fn drop(&mut self) {
        if self.exit.borrow().is_none() {
            let _ = self.controller.signal(ProcessSignal::Sigkill);
        }
    }
}

#[cfg(unix)]
pub(crate) fn launch_authorized_pipe(
    authorized: AuthorizedSpawn,
) -> Result<GovernedChild, pi_sandbox::SandboxError> {
    let parent = governed_pipe_stdio()?;
    let (sandbox, request) =
        authorized.into_prepared_request(pi_sandbox::SpawnStdio::Piped(parent.stdio))?;
    let prepared = pi_sandbox::prepare_spawn(sandbox.backend(), request)?;
    let process = match pi_sandbox::launch_prepared_unix(prepared)? {
        pi_sandbox::LaunchedUnixHandle::Pipe(process) => process,
        pi_sandbox::LaunchedUnixHandle::Pty(_) => {
            return Err(pi_sandbox::SandboxError::InvalidSpawn {
                failure: pi_sandbox::SpawnValidationError::FamilyStdioMismatch,
            });
        }
    };
    let pid = process.pid();
    let controller = Arc::new(BackendController {
        pid: Some(pid),
        process_group: Some(process.process_group()),
        stdin: Mutex::new(None),
        pty: Mutex::new(None),
        output_shutdown: Mutex::new(None),
    });
    let (exit_tx, exit) = watch::channel(None);
    tokio::task::spawn_blocking(move || {
        let outcome = process.wait().map(pi_sandbox::UnixExitStatus::shell_code);
        let _ = exit_tx.send(Some(outcome));
    });
    Ok(GovernedChild {
        pid,
        controller,
        exit,
        stdin: Some(tokio::fs::File::from_std(parent.stdin)),
        stdout: Some(tokio::fs::File::from_std(parent.stdout)),
        stderr: Some(tokio::fs::File::from_std(parent.stderr)),
    })
}

#[cfg(unix)]
struct GovernedPipeStdio {
    stdio: pi_sandbox::PipeStdio,
    stdin: std::fs::File,
    stdout: std::fs::File,
    stderr: std::fs::File,
}

#[cfg(unix)]
fn governed_pipe_stdio() -> Result<GovernedPipeStdio, pi_sandbox::SandboxError> {
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;

    let (stdin, child_stdin) = UnixStream::pair().map_err(channel_error)?;
    let (stdout, child_stdout) = UnixStream::pair().map_err(channel_error)?;
    let (stderr, child_stderr) = UnixStream::pair().map_err(channel_error)?;
    let stdio = pi_sandbox::PipeStdio::new(
        OwnedFd::from(child_stdin),
        OwnedFd::from(child_stdout),
        OwnedFd::from(child_stderr),
    )?;
    Ok(GovernedPipeStdio {
        stdio,
        stdin: std::fs::File::from(OwnedFd::from(stdin)),
        stdout: std::fs::File::from(OwnedFd::from(stdout)),
        stderr: std::fs::File::from(OwnedFd::from(stderr)),
    })
}

#[cfg(unix)]
pub(crate) struct BrowserDebuggingPipes {
    mappings: Vec<pi_sandbox::TargetFdMapping>,
    canonical: Vec<pi_sandbox::CanonicalTargetFdMapping>,
    debugger_writer: std::fs::File,
    debugger_reader: std::fs::File,
}

#[cfg(unix)]
impl BrowserDebuggingPipes {
    pub(crate) fn canonical_mappings(&self) -> &[pi_sandbox::CanonicalTargetFdMapping] {
        &self.canonical
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        Vec<pi_sandbox::TargetFdMapping>,
        std::fs::File,
        std::fs::File,
    ) {
        (self.mappings, self.debugger_writer, self.debugger_reader)
    }
}

#[cfg(unix)]
pub(crate) fn browser_debugging_pipes() -> Result<BrowserDebuggingPipes, pi_sandbox::SandboxError> {
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;

    let (child_read, debugger_writer) = UnixStream::pair().map_err(channel_error)?;
    let (debugger_reader, child_write) = UnixStream::pair().map_err(channel_error)?;
    let mappings = vec![
        pi_sandbox::TargetFdMapping::new(
            OwnedFd::from(child_read),
            pi_sandbox::TargetFd::new(3)?,
            pi_sandbox::TargetFdDirection::ParentWrites,
        )?,
        pi_sandbox::TargetFdMapping::new(
            OwnedFd::from(child_write),
            pi_sandbox::TargetFd::new(4)?,
            pi_sandbox::TargetFdDirection::ParentReads,
        )?,
    ];
    let mut canonical = mappings
        .iter()
        .map(pi_sandbox::TargetFdMapping::canonical)
        .collect::<Vec<_>>();
    canonical.sort_unstable();
    Ok(BrowserDebuggingPipes {
        mappings,
        canonical,
        debugger_writer: std::fs::File::from(OwnedFd::from(debugger_writer)),
        debugger_reader: std::fs::File::from(OwnedFd::from(debugger_reader)),
    })
}

#[cfg(unix)]
pub(crate) struct GovernedPipeProcess {
    pid: u32,
    controller: Arc<BackendController>,
    exit: watch::Receiver<Option<Result<i32, pi_sandbox::SandboxError>>>,
    debugger_writer: Option<std::fs::File>,
    debugger_reader: Option<std::fs::File>,
    stderr: Option<std::fs::File>,
}

#[cfg(unix)]
impl GovernedPipeProcess {
    pub(crate) const fn pid(&self) -> u32 {
        self.pid
    }

    pub(crate) fn take_debugger_writer(&mut self) -> Option<std::fs::File> {
        self.debugger_writer.take()
    }

    pub(crate) fn take_debugger_reader(&mut self) -> Option<std::fs::File> {
        self.debugger_reader.take()
    }

    pub(crate) fn take_stderr(&mut self) -> Option<std::fs::File> {
        self.stderr.take()
    }

    pub(crate) fn exit_status(&self) -> Option<Result<i32, pi_sandbox::SandboxError>> {
        self.exit.borrow().clone()
    }

    pub(crate) fn signal(&self, signal: ProcessSignal) -> Result<()> {
        self.controller.signal(signal)
    }

    pub(crate) async fn wait(&mut self) -> Result<i32, pi_sandbox::SandboxError> {
        loop {
            if let Some(outcome) = self.exit.borrow_and_update().clone() {
                return outcome;
            }
            if self.exit.changed().await.is_err() {
                return Err(pi_sandbox::SandboxError::System {
                    operation: pi_sandbox::SandboxIoOperation::WaitChild,
                    kind: pi_sandbox::SandboxIoKind::BrokenPipe,
                    raw_os_error: None,
                });
            }
        }
    }

    pub(crate) async fn shutdown(
        mut self,
        grace: Duration,
    ) -> Result<i32, pi_sandbox::SandboxError> {
        let _ = self.signal(ProcessSignal::Sigterm);
        match tokio::time::timeout(grace, self.wait()).await {
            Ok(outcome) => outcome,
            Err(_) => {
                let _ = self.signal(ProcessSignal::Sigkill);
                self.wait().await
            }
        }
    }
}

#[cfg(unix)]
pub(crate) async fn launch_authorized_browser_pipe(
    authorized: AuthorizedSpawn,
    debugger_writer: std::fs::File,
    debugger_reader: std::fs::File,
    deadline: tokio::time::Instant,
) -> Result<GovernedPipeProcess, pi_sandbox::SandboxError> {
    if tokio::time::Instant::now() >= deadline {
        return Err(deadline_error());
    }
    let browser_stdio = browser_pipe_stdio()?;
    let (sandbox, request) =
        authorized.into_prepared_request(pi_sandbox::SpawnStdio::Piped(browser_stdio.stdio))?;
    let prepared = pi_sandbox::prepare_spawn(sandbox.backend(), request)?;
    let launched = pi_sandbox::launch_prepared_unix(prepared)?;
    let process = match launched {
        pi_sandbox::LaunchedUnixHandle::Pipe(process) => process,
        pi_sandbox::LaunchedUnixHandle::Pty(_) => {
            return Err(pi_sandbox::SandboxError::InvalidSpawn {
                failure: pi_sandbox::SpawnValidationError::FamilyStdioMismatch,
            });
        }
    };
    let pid = process.pid();
    let process_group = Some(process.process_group());
    let (exit_tx, exit) = watch::channel(None);
    tokio::task::spawn_blocking(move || {
        let outcome = process.wait().map(pi_sandbox::UnixExitStatus::shell_code);
        let _ = exit_tx.send(Some(outcome));
    });
    drain_file(browser_stdio.stdout);
    let controller = Arc::new(BackendController {
        pid: Some(pid),
        process_group,
        stdin: Mutex::new(None),
        pty: Mutex::new(None),
        output_shutdown: Mutex::new(None),
    });
    let launched = GovernedPipeProcess {
        pid,
        controller,
        exit,
        debugger_writer: Some(debugger_writer),
        debugger_reader: Some(debugger_reader),
        stderr: Some(browser_stdio.stderr),
    };
    if tokio::time::Instant::now() >= deadline {
        let _ = launched.signal(ProcessSignal::Sigkill);
        return Err(deadline_error());
    }
    Ok(launched)
}

#[cfg(unix)]
struct BrowserPipeStdio {
    stdio: pi_sandbox::PipeStdio,
    stdout: std::fs::File,
    stderr: std::fs::File,
}

#[cfg(unix)]
fn browser_pipe_stdio() -> Result<BrowserPipeStdio, pi_sandbox::SandboxError> {
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;

    let (parent_stdin, child_stdin) = UnixStream::pair().map_err(channel_error)?;
    drop(parent_stdin);
    let (stdout, child_stdout) = UnixStream::pair().map_err(channel_error)?;
    let (stderr, child_stderr) = UnixStream::pair().map_err(channel_error)?;
    let stdio = pi_sandbox::PipeStdio::new(
        OwnedFd::from(child_stdin),
        OwnedFd::from(child_stdout),
        OwnedFd::from(child_stderr),
    )?;
    Ok(BrowserPipeStdio {
        stdio,
        stdout: std::fs::File::from(OwnedFd::from(stdout)),
        stderr: std::fs::File::from(OwnedFd::from(stderr)),
    })
}

#[cfg(unix)]
fn drain_file(mut file: std::fs::File) {
    tokio::task::spawn_blocking(move || {
        let mut buffer = [0_u8; 8192];
        loop {
            match file.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    });
}

#[cfg(unix)]
fn channel_error(error: io::Error) -> pi_sandbox::SandboxError {
    pi_sandbox::SandboxError::from_io(pi_sandbox::SandboxIoOperation::CreateChannel, &error)
}

#[cfg(unix)]
fn deadline_error() -> pi_sandbox::SandboxError {
    pi_sandbox::SandboxError::System {
        operation: pi_sandbox::SandboxIoOperation::ExecuteTarget,
        kind: pi_sandbox::SandboxIoKind::TimedOut,
        raw_os_error: None,
    }
}

#[cfg(windows)]
enum WindowsDangerPipeLaunchError {
    Authorization(anyhow::Error),
    Spawn(io::Error),
}

#[cfg(windows)]
impl WindowsDangerPipeLaunchError {
    fn into_anyhow(self) -> anyhow::Error {
        match self {
            Self::Authorization(error) => error,
            Self::Spawn(error) => error.into(),
        }
    }

    fn into_sandbox(self) -> pi_sandbox::SandboxError {
        match self {
            Self::Authorization(error) => anyhow_to_sandbox(error),
            Self::Spawn(error) => pi_sandbox::SandboxError::from_io(
                pi_sandbox::SandboxIoOperation::ExecuteTarget,
                &error,
            ),
        }
    }
}

#[cfg(windows)]
pub(crate) fn launch_authorized_pipe(
    authorized: AuthorizedSpawn,
) -> Result<GovernedChild, pi_sandbox::SandboxError> {
    let mut child = spawn_windows_danger_pipe(authorized)
        .map_err(WindowsDangerPipeLaunchError::into_sandbox)?;
    let pid = child.id().unwrap_or(0);
    let stdin = child.stdin.take().ok_or_else(windows_channel_error)?;
    let stdout = child.stdout.take().ok_or_else(windows_channel_error)?;
    let stderr = child.stderr.take().ok_or_else(windows_channel_error)?;
    let (exit_tx, exit) = watch::channel(None);
    tokio::spawn(async move {
        let outcome = child
            .wait()
            .await
            .map_err(|error| {
                pi_sandbox::SandboxError::from_io(pi_sandbox::SandboxIoOperation::WaitChild, &error)
            })
            .map(|status| status.code().unwrap_or(1));
        let _ = exit_tx.send(Some(outcome));
    });
    let controller = Arc::new(BackendController {
        pid: Some(pid),
        process_group: None,
        stdin: Mutex::new(None),
        pty: Mutex::new(None),
        fallback_killer: Mutex::new(None),
    });
    Ok(GovernedChild {
        pid,
        controller,
        exit,
        stdin: Some(tokio::fs::File::from_std(stdin.into_std().map_err(
            |error| {
                pi_sandbox::SandboxError::from_io(
                    pi_sandbox::SandboxIoOperation::CreateChannel,
                    &error,
                )
            },
        )?)),
        stdout: Some(tokio::fs::File::from_std(stdout.into_std().map_err(
            |error| {
                pi_sandbox::SandboxError::from_io(
                    pi_sandbox::SandboxIoOperation::CreateChannel,
                    &error,
                )
            },
        )?)),
        stderr: Some(tokio::fs::File::from_std(stderr.into_std().map_err(
            |error| {
                pi_sandbox::SandboxError::from_io(
                    pi_sandbox::SandboxIoOperation::CreateChannel,
                    &error,
                )
            },
        )?)),
    })
}

#[cfg(windows)]
fn windows_channel_error() -> pi_sandbox::SandboxError {
    pi_sandbox::SandboxError::System {
        operation: pi_sandbox::SandboxIoOperation::CreateChannel,
        kind: pi_sandbox::SandboxIoKind::BrokenPipe,
        raw_os_error: None,
    }
}

#[cfg(windows)]
fn anyhow_to_sandbox(error: anyhow::Error) -> pi_sandbox::SandboxError {
    error
        .downcast::<pi_sandbox::SandboxError>()
        .unwrap_or_else(|_| windows_channel_error())
}

#[cfg(windows)]
fn spawn_windows_danger_pipe(
    authorized: AuthorizedSpawn,
) -> Result<tokio::process::Child, WindowsDangerPipeLaunchError> {
    use std::process::Stdio;
    use tokio::process::Command;

    require_windows_danger(&authorized).map_err(WindowsDangerPipeLaunchError::Authorization)?;
    let mut command = Command::new(&authorized.argv()[0]);
    command.args(&authorized.argv()[1..]);
    command.current_dir(authorized.cwd().as_path());
    command.env_clear();
    command.envs(authorized.environment());
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command.kill_on_drop(true);
    command.spawn().map_err(WindowsDangerPipeLaunchError::Spawn)
}

#[cfg(windows)]
async fn adapt_windows_danger_pipe(mut child: tokio::process::Child) -> Result<BackendProcess> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let pid = child.id();
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("missing child stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("missing child stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("missing child stderr"))?;
    let (stdin_tx, mut stdin_rx) = mpsc::channel::<Vec<u8>>(PROCESS_CHANNEL_CAPACITY);
    let (output_tx, output_rx) = mpsc::channel(PROCESS_CHANNEL_CAPACITY);
    let (exit_tx, exit_rx) = oneshot::channel();
    tokio::spawn(async move {
        let mut stdin = stdin;
        while let Some(bytes) = stdin_rx.recv().await {
            if stdin.write_all(&bytes).await.is_err() || stdin.flush().await.is_err() {
                break;
            }
        }
    });
    for (stream, mut reader) in [
        (
            ProcessStream::Stdout,
            Box::new(stdout) as Box<dyn tokio::io::AsyncRead + Unpin + Send>,
        ),
        (
            ProcessStream::Stderr,
            Box::new(stderr) as Box<dyn tokio::io::AsyncRead + Unpin + Send>,
        ),
    ] {
        let output_tx = output_tx.clone();
        tokio::spawn(async move {
            let mut buffer = [0_u8; 8192];
            loop {
                match reader.read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        if output_tx
                            .send((stream, buffer[..read].to_vec()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        });
    }
    tokio::spawn(async move {
        let code = child
            .wait()
            .await
            .ok()
            .and_then(|status| status.code())
            .unwrap_or(-1);
        let _ = exit_tx.send(code);
    });
    let controller = Arc::new(BackendController {
        pid,
        process_group: None,
        stdin: Mutex::new(Some(stdin_tx)),
        pty: Mutex::new(None),
        fallback_killer: Mutex::new(None),
    });
    Ok(BackendProcess {
        pid,
        controller,
        output_rx,
        exit_rx,
    })
}

#[cfg(windows)]
fn spawn_windows_danger_pty(authorized: AuthorizedSpawn) -> Result<BackendProcess> {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};

    require_windows_danger(&authorized)?;
    let size = authorized.terminal_size().unwrap_or_default();
    let pair = native_pty_system().openpty(PtySize {
        rows: size.rows(),
        cols: size.columns(),
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let mut command = CommandBuilder::new(&authorized.argv()[0]);
    command.cwd(authorized.cwd().as_path());
    command.env_clear();
    for argument in &authorized.argv()[1..] {
        command.arg(argument);
    }
    for (key, value) in authorized.environment() {
        command.env(key, value);
    }
    let mut child = pair.slave.spawn_command(command)?;
    let pid = child.process_id();
    let killer = child.clone_killer();
    let mut reader = pair.master.try_clone_reader()?;
    let writer = pair.master.take_writer()?;
    let (stdin_tx, mut stdin_rx) = mpsc::channel::<Vec<u8>>(PROCESS_CHANNEL_CAPACITY);
    let (output_tx, output_rx) = mpsc::channel(PROCESS_CHANNEL_CAPACITY);
    let (exit_tx, exit_rx) = oneshot::channel();
    tokio::task::spawn_blocking(move || {
        let mut buffer = [0_u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    if output_tx
                        .blocking_send((ProcessStream::Combined, buffer[..read].to_vec()))
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    });
    tokio::task::spawn_blocking(move || {
        let mut writer = writer;
        while let Some(bytes) = stdin_rx.blocking_recv() {
            if writer.write_all(&bytes).is_err() || writer.flush().is_err() {
                break;
            }
        }
    });
    tokio::task::spawn_blocking(move || {
        let code = child.wait().map_or(-1, |status| status.exit_code() as i32);
        let _ = exit_tx.send(code);
    });
    let controller = Arc::new(BackendController {
        pid,
        process_group: None,
        stdin: Mutex::new(Some(stdin_tx)),
        pty: Mutex::new(Some(BackendPty::Portable(pair.master))),
        fallback_killer: Mutex::new(Some(killer)),
    });
    Ok(BackendProcess {
        pid,
        controller,
        output_rx,
        exit_rx,
    })
}

#[cfg(windows)]
fn require_windows_danger(authorized: &AuthorizedSpawn) -> Result<()> {
    use pi_sandbox::{HostPlatform, SandboxError, SandboxMode};

    if !authorized.canonical_target_fd_mappings().is_empty() {
        return Err(SandboxError::InvalidSpawn {
            failure: pi_sandbox::SpawnValidationError::FamilyTargetMappingMismatch,
        }
        .into());
    }
    let policy = authorized.sandbox().policy_snapshot().policy();
    if policy.mode() != SandboxMode::DangerFullAccess {
        return Err(SandboxError::UnsupportedMode {
            platform: HostPlatform::Windows,
            mode: policy.mode(),
        }
        .into());
    }
    Ok(())
}
