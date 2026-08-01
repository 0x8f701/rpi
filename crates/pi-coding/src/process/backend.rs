use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use parking_lot::Mutex;
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

use super::{ProcessSignal, ProcessStream, ProcessTerminalSize};

const PROCESS_CHANNEL_CAPACITY: usize = 128;

pub(super) struct BackendProcess {
    pub pid: Option<u32>,
    pub controller: Arc<BackendController>,
    pub output_rx: mpsc::Receiver<(ProcessStream, Vec<u8>)>,
    pub exit_rx: oneshot::Receiver<i32>,
}

pub(super) struct BackendController {
    pid: Option<u32>,
    stdin: Mutex<Option<mpsc::Sender<Vec<u8>>>>,
    pty_master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    fallback_killer: Mutex<Option<Box<dyn ChildKiller + Send + Sync>>>,
    #[cfg(unix)]
    output_shutdown: Mutex<Option<std::os::unix::net::UnixStream>>,
}

impl std::fmt::Debug for BackendController {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BackendController")
            .field("pid", &self.pid)
            .field("stdin_open", &self.stdin.lock().is_some())
            .field("pty", &self.pty_master.lock().is_some())
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
        let master = self.pty_master.lock();
        let master = master
            .as_ref()
            .ok_or_else(|| anyhow!("process is not attached to a PTY"))?;
        master.resize(PtySize {
            rows: size.rows,
            cols: size.cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        Ok(())
    }

    pub fn signal(&self, signal: ProcessSignal) -> Result<()> {
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            use nix::sys::signal::{Signal, killpg};
            use nix::unistd::Pid;

            let native = match signal {
                ProcessSignal::Sigint => Signal::SIGINT,
                ProcessSignal::Sigterm => Signal::SIGTERM,
                ProcessSignal::Sighup => Signal::SIGHUP,
                ProcessSignal::Sigquit => Signal::SIGQUIT,
                ProcessSignal::Sigkill => Signal::SIGKILL,
            };
            match killpg(Pid::from_raw(pid as i32), native) {
                Ok(()) => return Ok(()),
                Err(nix::errno::Errno::ESRCH) => return Ok(()),
                Err(error) if signal != ProcessSignal::Sigkill => return Err(error.into()),
                Err(_) => {}
            }
        }

        if signal == ProcessSignal::Sigkill {
            if let Some(killer) = self.fallback_killer.lock().as_mut() {
                killer.kill()?;
                return Ok(());
            }
            return Ok(());
        }
        Err(anyhow!("signal {signal:?} is not supported by this process backend"))
    }
}

pub(super) async fn spawn(
    argv: &[String],
    cwd: &Path,
    env: &BTreeMap<String, String>,
    tty: bool,
    terminal_size: ProcessTerminalSize,
) -> Result<BackendProcess> {
    if tty {
        spawn_pty(argv, cwd, env, terminal_size)
    } else {
        spawn_pipe(argv, cwd, env).await
    }
}

fn spawn_pty(
    argv: &[String],
    cwd: &Path,
    env: &BTreeMap<String, String>,
    terminal_size: ProcessTerminalSize,
) -> Result<BackendProcess> {
    let pair = native_pty_system().openpty(PtySize {
        rows: terminal_size.rows,
        cols: terminal_size.cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let mut command = CommandBuilder::new(&argv[0]);
    command.cwd(cwd);
    command.env_clear();
    for argument in &argv[1..] {
        command.arg(argument);
    }
    for (key, value) in env {
        command.env(key, value);
    }

    let mut child = pair
        .slave
        .spawn_command(command)
        .context("spawning PTY process")?;
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
                Ok(0) => break,
                Ok(read) => {
                    if output_tx
                        .blocking_send((ProcessStream::Combined, buffer[..read].to_vec()))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::yield_now();
                }
                Err(_) => break,
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
        stdin: Mutex::new(Some(stdin_tx)),
        pty_master: Mutex::new(Some(pair.master)),
        fallback_killer: Mutex::new(Some(killer)),
        #[cfg(unix)]
        output_shutdown: Mutex::new(None),
    });
    Ok(BackendProcess {
        pid,
        controller,
        output_rx,
        exit_rx,
    })
}

async fn spawn_pipe(
    argv: &[String],
    cwd: &Path,
    env: &BTreeMap<String, String>,
) -> Result<BackendProcess> {
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]);
    command.current_dir(cwd);
    command.env_clear();
    command.envs(env);
    command.stdin(Stdio::piped());
    command.kill_on_drop(true);

    #[cfg(unix)]
    let (merged_reader, output_shutdown) = {
        use std::os::fd::OwnedFd;
        use std::os::unix::net::UnixStream;
        let (reader, writer) = UnixStream::pair()?;
        let reader_shutdown = reader.try_clone()?;
        let stderr_writer = writer.try_clone()?;
        command.stdout(Stdio::from(OwnedFd::from(writer)));
        command.stderr(Stdio::from(OwnedFd::from(stderr_writer)));
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
        (reader, reader_shutdown)
    };
    #[cfg(not(unix))]
    {
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
    }

    let mut child = command
        .spawn()
        .context("spawning pipe process")?;
    let pid = child.id();
    let stdin = child.stdin.take().ok_or_else(|| anyhow!("missing child stdin"))?;
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

    #[cfg(unix)]
    tokio::task::spawn_blocking(move || {
        let mut reader = merged_reader;
        let mut buffer = [0_u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    if output_tx
                        .blocking_send((ProcessStream::Combined, buffer[..read].to_vec()))
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

    #[cfg(not(unix))]
    {
        use tokio::io::AsyncReadExt;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("missing child stdout"))?;
        let stderr = child.stderr.take().ok_or_else(|| anyhow!("missing child stderr"))?;
        for (stream, mut reader) in [
            (ProcessStream::Stdout, Box::new(stdout) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
            (ProcessStream::Stderr, Box::new(stderr) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
        ] {
            let output_tx = output_tx.clone();
            tokio::spawn(async move {
                let mut buffer = [0_u8; 8192];
                loop {
                    match reader.read(&mut buffer).await {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            if output_tx.send((stream, buffer[..read].to_vec())).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
    }

    tokio::spawn(async move {
        let code = match child.wait().await {
            Ok(status) => exit_code(status),
            Err(_) => -1,
        };
        let _ = exit_tx.send(code);
    });

    let controller = Arc::new(BackendController {
        pid,
        stdin: Mutex::new(Some(stdin_tx)),
        pty_master: Mutex::new(None),
        fallback_killer: Mutex::new(None),
        #[cfg(unix)]
        output_shutdown: Mutex::new(Some(output_shutdown)),
    });
    Ok(BackendProcess {
        pid,
        controller,
        output_rx,
        exit_rx,
    })
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    -1
}
