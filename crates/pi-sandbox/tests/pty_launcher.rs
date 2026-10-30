#![cfg(unix)]

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pi_sandbox::{
    AbsolutePath, CredentialPolicy, HostPlatform, LaunchedUnixHandle, PipeStdio,
    PlatformFactInputs, PolicyRevision, PolicySnapshot, PreparedSpawnRequest, RetainedExecutable,
    SandboxError, SandboxPolicy, SpawnFamily, SpawnStdio, SpawnValidationError, TerminalSize,
    derive_platform_facts, launch_prepared_unix, prepare_spawn, probe_backend,
};

fn absolute(path: &Path) -> AbsolutePath {
    AbsolutePath::parse(path.to_path_buf()).expect("absolute UTF-8 path")
}

fn pipe_pair() -> (File, OwnedFd) {
    let mut descriptors = [-1; 2];
    assert_eq!(
        unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) },
        0,
        "create close-on-exec pipe: {}",
        std::io::Error::last_os_error()
    );
    let read = unsafe { File::from_raw_fd(descriptors[0]) };
    let write = unsafe { OwnedFd::from_raw_fd(descriptors[1]) };
    (read, write)
}

fn danger_backend(root: &Path) -> pi_sandbox::SandboxBackend {
    let workspace = root.join("workspace");
    let agent = root.join("agent");
    let session = root.join("session");
    for directory in [&workspace, &agent, &session] {
        std::fs::create_dir(directory).expect("create danger backend directory");
    }
    let facts = derive_platform_facts(
        PlatformFactInputs::new(
            "danger-public-launcher-tests".to_owned(),
            vec![absolute(&workspace)],
            absolute(&agent),
            absolute(&session),
        )
        .expect("danger platform fact inputs"),
    )
    .expect("derive danger platform facts");
    let policy = SandboxPolicy::danger_full_access(
        CredentialPolicy::new(Vec::new()).expect("empty credential policy"),
    )
    .expect("danger policy");
    let snapshot = Arc::new(
        PolicySnapshot::new(
            PolicyRevision::new(1).expect("revision"),
            if cfg!(target_os = "macos") {
                HostPlatform::Macos
            } else {
                HostPlatform::Linux
            },
            policy,
        )
        .expect("danger snapshot"),
    );
    probe_backend(facts, snapshot).expect("probe danger backend")
}

fn request(
    root: &Path,
    family: SpawnFamily,
    script: &str,
    stdio: SpawnStdio,
    retained_executable: Option<RetainedExecutable>,
) -> PreparedSpawnRequest {
    PreparedSpawnRequest::new(
        family,
        vec![
            OsString::from("/bin/sh"),
            OsString::from("-c"),
            OsString::from(script),
        ],
        absolute(&root.join("workspace")),
        BTreeMap::new(),
        stdio,
        Vec::new(),
        Vec::new(),
        retained_executable,
        None,
    )
    .expect("valid danger spawn request")
}

fn wait_with_timeout(
    process: pi_sandbox::LaunchedUnixProcess,
    timeout: Duration,
) -> pi_sandbox::UnixExitStatus {
    let process_group = process.process_group();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = sender.send(process.wait());
    });
    match receiver.recv_timeout(timeout) {
        Ok(status) => status.expect("wait for danger child"),
        Err(error) => {
            unsafe { libc::kill(-process_group, libc::SIGKILL) };
            let _ = receiver.recv_timeout(Duration::from_secs(2));
            panic!("danger child wait timed out: {error}");
        }
    }
}

fn read_until(reader: &mut File, expected: &str, timeout: Duration) -> String {
    let flags = unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_GETFL) };
    assert!(
        flags >= 0,
        "read PTY flags: {}",
        std::io::Error::last_os_error()
    );
    assert_eq!(
        unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
        0,
        "set PTY nonblocking: {}",
        std::io::Error::last_os_error()
    );
    let deadline = Instant::now() + timeout;
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 256];
    while Instant::now() < deadline {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(length) => {
                bytes.extend_from_slice(&buffer[..length]);
                let output = String::from_utf8_lossy(&bytes);
                if output.contains(expected) {
                    return output.into_owned();
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
            Err(error) => panic!("read PTY output: {error}"),
        }
    }
    panic!(
        "PTY output did not contain {expected:?} before timeout: {:?}",
        String::from_utf8_lossy(&bytes)
    );
}

#[test]
fn terminal_size_contract_is_nonzero_and_stable() {
    let size = TerminalSize::new(37, 119).expect("valid terminal size");
    assert_eq!(size.rows(), 37);
    assert_eq!(size.columns(), 119);
    assert!(TerminalSize::new(0, 119).is_err());
    assert!(TerminalSize::new(37, 0).is_err());
}

#[test]
fn unix_pty_handle_surface_is_descriptor_owned() {
    assert!(std::mem::needs_drop::<pi_sandbox::UnixPtyMaster>());
    assert!(std::mem::needs_drop::<pi_sandbox::LaunchedUnixHandle>());
}

#[test]
fn danger_backend_prepares_and_launches_real_pipe_and_pty_processes() {
    let root = tempfile::tempdir().expect("danger root");
    let backend = danger_backend(root.path());

    let (stdin_read, stdin_write) = pipe_pair();
    drop(stdin_write);
    let (mut stdout_read, stdout_write) = pipe_pair();
    let (mut stderr_read, stderr_write) = pipe_pair();
    let pipe_request = request(
        root.path(),
        SpawnFamily::ManagedPipeProcess,
        "printf 'danger-pipe-ok\\n'",
        SpawnStdio::Piped(
            PipeStdio::new(stdin_read.into(), stdout_write, stderr_write).expect("pipe stdio"),
        ),
        None,
    );
    let pipe = match launch_prepared_unix(
        prepare_spawn(&backend, pipe_request).expect("prepare danger pipe"),
    )
    .expect("launch danger pipe")
    {
        LaunchedUnixHandle::Pipe(process) => process,
        LaunchedUnixHandle::Pty(_) => panic!("danger pipe returned PTY"),
    };
    let pipe_status = wait_with_timeout(pipe, Duration::from_secs(5));
    let mut pipe_stdout = String::new();
    stdout_read
        .read_to_string(&mut pipe_stdout)
        .expect("read danger pipe stdout");
    let mut pipe_stderr = String::new();
    stderr_read
        .read_to_string(&mut pipe_stderr)
        .expect("read danger pipe stderr");
    assert_eq!(pipe_status.code(), Some(0));
    assert_eq!(pipe_stdout, "danger-pipe-ok\n");
    assert!(
        pipe_stderr.is_empty(),
        "danger pipe stderr: {pipe_stderr:?}"
    );

    let pty_request = request(
        root.path(),
        SpawnFamily::ManagedPtyProcess,
        "printf 'danger-pty-ok\\n'",
        SpawnStdio::Pty(TerminalSize::new(24, 80).expect("PTY size")),
        None,
    );
    let (pty_process, pty_master) = match launch_prepared_unix(
        prepare_spawn(&backend, pty_request).expect("prepare danger PTY"),
    )
    .expect("launch danger PTY")
    {
        LaunchedUnixHandle::Pty(pty) => pty.into_parts(),
        LaunchedUnixHandle::Pipe(_) => panic!("danger PTY returned pipe"),
    };
    let mut pty_reader = pty_master.try_clone_reader().expect("clone PTY reader");
    let pty_output = read_until(&mut pty_reader, "danger-pty-ok", Duration::from_secs(5));
    let pty_status = wait_with_timeout(pty_process, Duration::from_secs(5));
    assert_eq!(pty_status.code(), Some(0));
    assert!(
        pty_output.contains("danger-pty-ok"),
        "PTY output: {pty_output:?}"
    );
    drop(pty_master);

    let retained_shell = std::fs::canonicalize("/bin/sh").expect("canonical retained shell");
    let retained_file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC)
        .open(&retained_shell)
        .expect("open retained shell");
    let retained = RetainedExecutable::new(retained_file.into(), absolute(&retained_shell))
        .expect("retain shell");
    let retained_request = PreparedSpawnRequest::new(
        SpawnFamily::ManagedPipeProcess,
        vec![
            OsString::from(retained_shell.as_os_str()),
            OsString::from("-c"),
            OsString::from("exit 0"),
        ],
        absolute(&root.path().join("workspace")),
        BTreeMap::new(),
        SpawnStdio::Null,
        Vec::new(),
        Vec::new(),
        Some(retained),
        None,
    )
    .expect("retained shell matches argv0");
    let error = match prepare_spawn(&backend, retained_request) {
        Ok(_) => panic!("danger backend must reject retained executable handles"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        SandboxError::InvalidSpawn {
            failure: SpawnValidationError::InvalidRetainedExecutable
        }
    ));
}

#[test]
fn pty_resize_is_visible_to_child_and_wait_reaps_process() {
    let root = tempfile::tempdir().expect("danger root");
    let backend = danger_backend(root.path());
    let pty_request = request(
        root.path(),
        SpawnFamily::ManagedPtyProcess,
        "printf 'ready\\n'; IFS= read -r line; stty size",
        SpawnStdio::Pty(TerminalSize::new(24, 80).expect("initial PTY size")),
        None,
    );
    let (process, master) = match launch_prepared_unix(
        prepare_spawn(&backend, pty_request).expect("prepare resize PTY"),
    )
    .expect("launch resize PTY")
    {
        LaunchedUnixHandle::Pty(pty) => pty.into_parts(),
        LaunchedUnixHandle::Pipe(_) => panic!("resize PTY returned pipe"),
    };
    let child_pid = process.pid();
    let mut reader = master.try_clone_reader().expect("clone resize reader");
    let ready = read_until(&mut reader, "ready", Duration::from_secs(5));
    assert!(
        ready.contains("ready"),
        "PTY synchronization output: {ready:?}"
    );
    master
        .resize(TerminalSize::new(41, 132).expect("resized PTY size"))
        .expect("resize PTY master");
    let mut writer = master.try_clone_writer().expect("clone resize writer");
    writer.write_all(b"continue\n").expect("release PTY child");
    writer.flush().expect("flush PTY input");
    drop(writer);
    let resized = read_until(&mut reader, "41 132", Duration::from_secs(5));
    let status = wait_with_timeout(process, Duration::from_secs(5));
    assert_eq!(status.code(), Some(0));
    assert!(
        resized.contains("41 132"),
        "resized PTY output: {resized:?}"
    );
    drop(master);
    let mut byte = [0_u8; 1];
    match reader.read(&mut byte) {
        Ok(0) => {}
        Err(error) if error.raw_os_error() == Some(libc::EIO) => {}
        other => panic!("PTY master must reach EOF after child exit: {other:?}"),
    }
    assert_eq!(
        unsafe {
            libc::waitpid(
                child_pid as libc::pid_t,
                std::ptr::null_mut(),
                libc::WNOHANG,
            )
        },
        -1,
        "wait must reap the PTY child"
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}
