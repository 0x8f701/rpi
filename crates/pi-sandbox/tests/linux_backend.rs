#[cfg(target_os = "linux")]
mod linux_proof {

    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::fs::{File, OpenOptions};
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use pi_sandbox::{
        AbsolutePath, CredentialPolicy, FileSystemPolicy, HostHttpPolicy, HostPlatform,
        LaunchedUnixHandle, MountSource, NetworkPolicy, PipeStdio, PlatformFactInputs,
        PolicyRevision, PolicySnapshot, PreparedSpawnRequest, RequestedPolicy, ResourceLimits,
        RetainedExecutable, RetainedWorkingDirectory, SandboxMode, SandboxPolicy, SessionScope,
        SpawnFamily, SpawnStdio, WritableRoot, derive_platform_facts, launch_prepared_unix,
        prepare_spawn, probe_backend, resolve_policy_snapshot,
    };
    use tempfile::{Builder, TempDir};

    const TEST_NAME: &str = "confined_linux_enforces_empty_root_fd_mounts_limits_nnp_and_seccomp";
    const TARGET_MODE: &str = "__pi_linux_backend_target";
    const ADDRESS_SPACE_LIMIT: u64 = 256 * 1024 * 1024;
    const CPU_LIMIT_SECONDS: u64 = 1;
    const FILE_SIZE_LIMIT: u64 = 4096;
    const OPEN_FILES_LIMIT: u64 = 32;

    struct OwnedRoots {
        _root: TempDir,
        workspace: PathBuf,
        agent: PathBuf,
        session: PathBuf,
    }

    impl OwnedRoots {
        fn new() -> Self {
            let root = Builder::new()
                .prefix("pi-linux-backend-")
                .tempdir()
                .expect("create deterministic owned sandbox roots");
            let workspace = root.path().join("workspace");
            let agent = root.path().join("agent");
            let session = root.path().join("session");
            for directory in [&workspace, &agent, &session] {
                std::fs::create_dir(directory).expect("create owned sandbox directory");
            }
            Self {
                _root: root,
                workspace,
                agent,
                session,
            }
        }

        fn home_nested() -> Self {
            let root = Builder::new()
                .prefix("pi-linux-backend-home-")
                .tempdir()
                .expect("create HOME-nested sandbox roots");
            let workspace = root.path().join("home");
            let agent = workspace.join(".rpi");
            let session = agent.join("sessions").join("session-1");
            for directory in [&workspace, &agent, &session] {
                std::fs::create_dir_all(directory).expect("create HOME-nested sandbox directory");
            }
            Self {
                _root: root,
                workspace,
                agent,
                session,
            }
        }
    }

    fn absolute(path: &Path) -> AbsolutePath {
        AbsolutePath::parse(path.to_path_buf()).expect("absolute UTF-8 path")
    }

    fn open_directory(path: &Path) -> OwnedFd {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY)
            .open(path)
            .expect("open owned directory for FD-backed mount");
        file.into()
    }

    fn pipe_pair() -> (File, OwnedFd) {
        let mut descriptors = [-1; 2];
        assert_eq!(
            unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) },
            0,
            "create close-on-exec test pipe: {}",
            std::io::Error::last_os_error()
        );
        let read = unsafe { File::from_raw_fd(descriptors[0]) };
        let write = unsafe { OwnedFd::from_raw_fd(descriptors[1]) };
        (read, write)
    }

    fn workspace_mount(roots: &OwnedRoots) -> Vec<MountSource> {
        vec![
            MountSource::read_write_directory(
                open_directory(&roots.workspace),
                absolute(&roots.workspace),
            )
            .expect("workspace mount source"),
        ]
    }

    fn run_target_with_backend(
        target: &str,
        roots: &OwnedRoots,
        current: &Path,
        mount_sources: Vec<MountSource>,
        backend: &pi_sandbox::SandboxBackend,
    ) -> (pi_sandbox::UnixExitStatus, String) {
        run_target_with_arguments(target, roots, current, mount_sources, backend, Vec::new())
    }

    fn run_target_with_arguments(
        target: &str,
        roots: &OwnedRoots,
        current: &Path,
        mount_sources: Vec<MountSource>,
        backend: &pi_sandbox::SandboxBackend,
        extra_arguments: Vec<OsString>,
    ) -> (pi_sandbox::UnixExitStatus, String) {
        let target_file = File::open(current).expect("open retained hosted target executable");
        let retained_target = RetainedExecutable::new(target_file.into(), absolute(current))
            .expect("retain hosted target executable identity");
        let retained_working_directory = RetainedWorkingDirectory::new(
            open_directory(&roots.workspace),
            absolute(&roots.workspace),
        )
        .expect("retain hosted working directory identity");

        let (stdin_read, stdin_write) = pipe_pair();
        drop(stdin_write);
        let (stdout_read, stdout_write) = pipe_pair();
        let (stderr_read, stderr_write) = pipe_pair();
        let stdio = PipeStdio::new(stdin_read.into(), stdout_write, stderr_write)
            .expect("unique child stdio descriptors");
        let mut arguments = vec![
            current.as_os_str().to_owned(),
            OsString::from(TARGET_MODE),
            OsString::from(target),
            roots._root.path().as_os_str().to_owned(),
        ];
        arguments.extend(extra_arguments);
        let request = PreparedSpawnRequest::new(
            SpawnFamily::ManagedPipeProcess,
            arguments,
            absolute(&roots.workspace),
            vec![
                (OsString::from("HOME"), OsString::from("/run/pi/home")),
                (OsString::from("TMPDIR"), OsString::from("/tmp")),
            ]
            .into_iter()
            .collect(),
            SpawnStdio::Piped(stdio),
            Vec::new(),
            mount_sources,
            Some(retained_target),
            Some(retained_working_directory),
        )
        .expect("valid hosted proof spawn request");
        let launched = launch_prepared_unix(
            prepare_spawn(backend, request).expect("prepare real Linux confined spawn"),
        )
        .expect("launch real Linux confined target");
        let process = match launched {
            LaunchedUnixHandle::Pipe(process) => process,
            LaunchedUnixHandle::Pty(_) => panic!("hosted Linux proof unexpectedly returned a PTY"),
        };
        let stdout_reader = std::thread::spawn(move || {
            let mut stdout = String::new();
            stdout_read
                .take(64 * 1024)
                .read_to_string(&mut stdout)
                .expect("read hosted proof stdout");
            stdout
        });
        let stderr_reader = std::thread::spawn(move || {
            let mut stderr = String::new();
            stderr_read
                .take(64 * 1024)
                .read_to_string(&mut stderr)
                .expect("read hosted proof stderr");
            stderr
        });
        let process_group = process.process_group();
        let (wait_sender, wait_receiver) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let _ = wait_sender.send(process.wait());
        });
        let status = match wait_receiver.recv_timeout(Duration::from_secs(10)) {
            Ok(status) => status.expect("wait for hosted Linux proof target"),
            Err(error) => {
                unsafe { libc::kill(-process_group, libc::SIGKILL) };
                let _ = wait_receiver.recv_timeout(Duration::from_secs(2));
                panic!("hosted Linux proof target wait timed out: {error}");
            }
        };
        let stdout = stdout_reader
            .join()
            .expect("join hosted proof stdout reader");
        let stderr = stderr_reader
            .join()
            .expect("join hosted proof stderr reader");
        assert!(
            stderr.is_empty(),
            "hosted proof target emitted stderr for {target}: {stderr:?}"
        );
        (status, stdout)
    }

    fn connect_loopback(port: u16) -> std::io::Result<()> {
        let stream = TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
            Duration::from_secs(2),
        )?;
        stream.set_nodelay(true)?;
        Ok(())
    }

    fn assert_ptrace_is_blocked() {
        let ptrace_result = unsafe {
            libc::ptrace(
                libc::PTRACE_TRACEME,
                0,
                std::ptr::null_mut::<libc::c_void>(),
                std::ptr::null_mut::<libc::c_void>(),
            )
        };
        assert_eq!(ptrace_result, -1, "seccomp unexpectedly allowed ptrace");
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EPERM),
            "blocked ptrace must fail with EPERM"
        );
    }

    fn target_main(target: &str, owned_root: &Path, extra_arguments: &[OsString]) -> ! {
        match target {
            "filesystem" => {
                assert!(
                    std::fs::metadata("/root").is_err(),
                    "empty-root sandbox exposed the host /root"
                );
                let sentinel = owned_root.join("host-root-only-sentinel");
                assert!(
                    std::fs::metadata(&sentinel).is_err(),
                    "empty-root sandbox exposed an undeclared host-root sentinel"
                );
                let written = std::env::current_dir()
                    .expect("confined current directory")
                    .join("workspace-write-proof.txt");
                std::fs::write(&written, b"workspace-write-ok")
                    .expect("workspace root must be writable");
                assert_eq!(
                    std::fs::read(&written).expect("read workspace proof"),
                    b"workspace-write-ok"
                );
                println!("hidden-root=ok workspace-write=ok");
            }
            "denied-file" => {
                let denied = owned_root.join("denied-file");
                let metadata = std::fs::metadata(&denied).expect("denied mask metadata");
                assert_eq!(metadata.len(), 0, "denied mask must contain no host bytes");
                assert_eq!(metadata.permissions().mode() & 0o777, 0, "denied mask mode");
                assert!(
                    std::fs::read(&denied).is_err(),
                    "denied file content must remain unreadable"
                );
                println!("denied-file=masked");
            }
            "devices" => {
                let mut null = OpenOptions::new()
                    .write(true)
                    .open("/dev/null")
                    .expect("confined null device must be writable");
                null.write_all(b"device-proof")
                    .expect("confined null device write");
                let mut zero =
                    File::open("/dev/zero").expect("confined zero device must be readable");
                let mut byte = [1_u8; 1];
                zero.read_exact(&mut byte)
                    .expect("confined zero device read");
                assert_eq!(byte, [0]);
                println!("devices=usable");
            }
            "cwd-descriptor" => {
                let current = std::env::current_dir().expect("confined current directory");
                for entry in
                    std::fs::read_dir("/proc/self/fd").expect("confined descriptor directory")
                {
                    let entry = entry.expect("confined descriptor entry");
                    let Ok(target) = std::fs::read_link(entry.path()) else {
                        continue;
                    };
                    assert_ne!(
                        target, current,
                        "retained host cwd descriptor must close before target exec"
                    );
                }
                println!("cwd-descriptor=closed");
            }
            "home-tmp" => {
                let home = std::env::var("HOME").expect("HOME projected into child");
                let tmpdir = std::env::var("TMPDIR").expect("TMPDIR projected into child");
                assert!(
                    std::fs::write(format!("{home}/home-write-proof.txt"), b"must-not-write")
                        .is_err(),
                    "revision HOME must remain read-only"
                );
                let scratch_proof = format!("{tmpdir}/scratch-write-proof.txt");
                std::fs::write(&scratch_proof, b"scratch-write-ok")
                    .expect("TMPDIR sealed scratch must be writable");
                assert_eq!(
                    std::fs::read(&scratch_proof).expect("read scratch proof"),
                    b"scratch-write-ok"
                );
                println!("home-read-only=ok tmpdir-write=ok");
            }
            "limits" => {
                let mut limit = libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                };
                for (resource, expected, label) in [
                    (libc::RLIMIT_AS, ADDRESS_SPACE_LIMIT, "address-space"),
                    (libc::RLIMIT_CPU, CPU_LIMIT_SECONDS, "cpu-time"),
                    (libc::RLIMIT_FSIZE, FILE_SIZE_LIMIT, "file-size"),
                    (libc::RLIMIT_NOFILE, OPEN_FILES_LIMIT, "open-files"),
                ] {
                    assert_eq!(
                        unsafe { libc::getrlimit(resource, &mut limit) },
                        0,
                        "read {label} limit: {}",
                        std::io::Error::last_os_error()
                    );
                    assert_eq!(limit.rlim_cur as u64, expected, "{label} soft limit");
                    assert_eq!(limit.rlim_max as u64, expected, "{label} hard limit");
                    println!("limit-{label}=ok");
                }
            }
            "nnp-seccomp" => {
                assert_eq!(
                    unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) },
                    1,
                    "PR_SET_NO_NEW_PRIVS was not enforced"
                );
                assert_ptrace_is_blocked();
                println!("nnp=ok seccomp-ptrace=blocked");
            }
            "file-size" => {
                unsafe { libc::signal(libc::SIGXFSZ, libc::SIG_IGN) };
                let path = std::env::current_dir()
                    .expect("confined current directory")
                    .join("file-size-limit-proof.bin");
                let mut file = File::create(&path).expect("create file-size proof");
                let bytes = vec![b'x'; FILE_SIZE_LIMIT as usize + 1];
                let error = file
                    .write_all(&bytes)
                    .expect_err("RLIMIT_FSIZE must reject a file larger than the configured limit");
                assert_eq!(
                    error.raw_os_error(),
                    Some(libc::EFBIG),
                    "RLIMIT_FSIZE must surface EFBIG"
                );
            }
            "open-files" => {
                let minimum = i32::try_from(OPEN_FILES_LIMIT).expect("open-files limit");
                let duplicated =
                    unsafe { libc::fcntl(libc::STDOUT_FILENO, libc::F_DUPFD_CLOEXEC, minimum) };
                assert_eq!(
                    duplicated, -1,
                    "RLIMIT_NOFILE must reject a descriptor at the ceiling"
                );
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::EINVAL),
                    "F_DUPFD_CLOEXEC minimum at RLIMIT_NOFILE must fail with EINVAL"
                );
            }
            "address-space" => {
                let requested = ADDRESS_SPACE_LIMIT.saturating_mul(4) as usize;
                let mapping = unsafe {
                    libc::mmap(
                        std::ptr::null_mut(),
                        requested,
                        libc::PROT_READ | libc::PROT_WRITE,
                        libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                        -1,
                        0,
                    )
                };
                assert_eq!(
                    mapping,
                    libc::MAP_FAILED,
                    "RLIMIT_AS must reject an allocation above the configured address space"
                );
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::ENOMEM),
                    "RLIMIT_AS must surface ENOMEM"
                );
            }
            "cpu-time" => loop {
                std::hint::spin_loop();
            },
            "read-only-fd-proof" => {
                let current = std::env::current_dir().expect("confined current directory");
                let dot = std::ffi::CString::new(".").unwrap();
                let directory_fd = unsafe {
                    libc::open(
                        dot.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
                    )
                };
                assert!(directory_fd >= 0, "open confined cwd descriptor");
                let directory = unsafe { OwnedFd::from_raw_fd(directory_fd) };
                let descriptor_link = std::fs::read_link(
                    Path::new("/proc/self/fd").join(directory.as_raw_fd().to_string()),
                )
                .expect("resolve confined cwd descriptor");
                assert_eq!(
                    descriptor_link, current,
                    "controlled descriptor must identify cwd"
                );

                let name = std::ffi::CString::new("fd-authority-proof.txt").unwrap();
                let created = unsafe {
                    libc::openat(
                        directory.as_raw_fd(),
                        name.as_ptr(),
                        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                        0o644,
                    )
                };
                assert_eq!(
                    created, -1,
                    "read-only cwd descriptor must deny openat create"
                );
                assert!(
                    std::fs::write("cwd-write-proof.txt", b"must-not-write").is_err(),
                    "read-only cwd must deny relative writes",
                );
                println!("read-only-fd=ok");
            }
            "workspace-write-full-network" => {
                let port = extra_arguments[0]
                    .to_string_lossy()
                    .parse::<u16>()
                    .expect("loopback listener port");
                let sentinel = owned_root.join("network-host-sentinel");
                assert!(
                    std::fs::read(&sentinel).is_err(),
                    "full network must not expose undeclared host files"
                );
                std::fs::write("full-network-workspace-proof.txt", b"workspace-write-ok")
                    .expect("workspace-write remains writable with full network");
                connect_loopback(port)
                    .expect("full-access network reaches parent loopback listener");
                assert_ptrace_is_blocked();
                println!("full-network=connected workspace=written sentinel=hidden ptrace=blocked");
            }
            "read-only-network" => {
                let expected_network = extra_arguments[0].to_string_lossy();
                let port = extra_arguments[1]
                    .to_string_lossy()
                    .parse::<u16>()
                    .expect("loopback listener port");
                assert_eq!(
                    std::fs::read("read-only-input.txt").expect("read workspace in read-only mode"),
                    b"read-only-ok"
                );
                assert!(
                    std::fs::write("read-only-network-write.txt", b"must-not-write").is_err(),
                    "read-only workspace rejects writes for every network mode"
                );
                match expected_network.as_ref() {
                    "none" => {
                        let error = connect_loopback(port).expect_err(
                            "network-none must reject loopback socket creation/connect",
                        );
                        assert_eq!(
                            error.raw_os_error(),
                            Some(libc::EPERM),
                            "network-none socket must fail with EPERM"
                        );
                        println!("network-none=eperm filesystem=read-only");
                    }
                    "full" => {
                        connect_loopback(port)
                            .expect("full-access network reaches parent loopback listener");
                        println!("network-full=connected filesystem=read-only");
                    }
                    other => panic!("unknown read-only network expectation {other:?}"),
                }
            }
            "home-store-deny" => {
                let store = owned_root.join("home").join(".rpi");
                let secret = store.join("teams.json");
                assert!(
                    std::fs::read(&secret).is_err(),
                    "denied store base must hide host credential marker"
                );
                assert!(
                    std::fs::write(store.join("teams-new.json"), b"must-not-write").is_err(),
                    "denied store base must reject writes"
                );
                std::fs::write("control.txt", b"workspace-write-ok")
                    .expect("HOME workspace control marker must stay writable");
                let scratch_marker = "/run/pi/scratch/home-store-scratch-ok";
                std::fs::write(scratch_marker, b"scratch-write-ok")
                    .expect("retained /run/pi/scratch must stay writable after store deny");
                assert_eq!(
                    std::fs::read(scratch_marker).expect("read projected scratch"),
                    b"scratch-write-ok"
                );
                println!("store-hidden=ok workspace-write=ok scratch-write=ok");
            }
            other => panic!("unknown hosted Linux proof target {other:?}"),
        }
        std::process::exit(0)
    }

    fn probe_backend_with_test_revision_roots(
        facts: pi_sandbox::PlatformFacts,
        snapshot: Arc<PolicySnapshot>,
        roots: &OwnedRoots,
    ) -> Result<pi_sandbox::SandboxBackend, pi_sandbox::SandboxError> {
        let revision = roots
            .session
            .join("sandbox")
            .join(snapshot.revision().get().to_string());
        let home = revision.join("home");
        let tmp = revision.join("tmp");
        let scratch = revision.join("scratch");
        for path in [&home, &tmp, &scratch] {
            std::fs::create_dir_all(path).expect("create revision projection");
        }
        let retained = pi_sandbox::RetainedRevisionRoots::new(
            open_directory(&home),
            absolute(&home),
            open_directory(&tmp),
            absolute(&tmp),
            open_directory(&scratch),
            absolute(&scratch),
        )?;
        pi_sandbox::probe_backend_with_revision_roots(facts, snapshot, retained)
    }
    fn resolved_backend(
        roots: &OwnedRoots,
        test_name: &str,
        mode: SandboxMode,
        network: NetworkPolicy,
        revision: u64,
    ) -> pi_sandbox::SandboxBackend {
        let facts = derive_platform_facts(
            PlatformFactInputs::new(
                test_name.to_owned(),
                vec![absolute(&roots.workspace)],
                absolute(&roots.agent),
                absolute(&roots.session),
            )
            .expect("valid platform fact inputs"),
        )
        .expect("Linux platform facts and backend prerequisites must be available");
        let revision_root = roots.session.join("sandbox").join(revision.to_string());
        let home = revision_root.join("home");
        let tmp = revision_root.join("tmp");
        let scratch = revision_root.join("scratch");
        for path in [&home, &tmp, &scratch] {
            std::fs::create_dir_all(path).expect("create revision projection");
        }
        let scope = SessionScope::new(
            test_name.to_owned(),
            vec![absolute(&roots.workspace)],
            absolute(&roots.agent),
            absolute(&roots.session),
        )
        .expect("valid session scope");
        let requested = RequestedPolicy::new(
            mode,
            network,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            HostHttpPolicy::new(Vec::new()).expect("empty host HTTP policy"),
            CredentialPolicy::new(Vec::new()).expect("empty credential policy"),
            ResourceLimits::confined_defaults(),
        )
        .expect("valid requested policy");
        let snapshot = Arc::new(
            resolve_policy_snapshot(
                PolicyRevision::new(revision).expect("revision"),
                HostPlatform::Linux,
                &scope,
                requested,
                &facts,
                absolute(&home),
                absolute(&tmp),
                absolute(&scratch),
                Vec::new(),
            )
            .expect("resolve confined policy snapshot"),
        );
        probe_backend_with_test_revision_roots(facts, snapshot, roots)
            .expect("trusted bubblewrap launcher and real Linux backend are required")
    }
    fn resolved_backend_with_private_roots(
        roots: &OwnedRoots,
        test_name: &str,
        mode: SandboxMode,
        network: NetworkPolicy,
        revision: u64,
        private_roots: Vec<AbsolutePath>,
    ) -> pi_sandbox::SandboxBackend {
        let facts = derive_platform_facts(
            PlatformFactInputs::new(
                test_name.to_owned(),
                vec![absolute(&roots.workspace)],
                absolute(&roots.agent),
                absolute(&roots.session),
            )
            .expect("valid platform fact inputs"),
        )
        .expect("Linux platform facts and backend prerequisites must be available");
        let revision_root = roots.session.join("sandbox").join(revision.to_string());
        let home = revision_root.join("home");
        let tmp = revision_root.join("tmp");
        let scratch = revision_root.join("scratch");
        for path in [&home, &tmp, &scratch] {
            std::fs::create_dir_all(path).expect("create revision projection");
        }
        let scope = SessionScope::new(
            test_name.to_owned(),
            vec![absolute(&roots.workspace)],
            absolute(&roots.agent),
            absolute(&roots.session),
        )
        .expect("valid session scope");
        let requested = RequestedPolicy::new(
            mode,
            network,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            HostHttpPolicy::new(Vec::new()).expect("empty host HTTP policy"),
            CredentialPolicy::new(Vec::new()).expect("empty credential policy"),
            ResourceLimits::confined_defaults(),
        )
        .expect("valid requested policy");
        let snapshot = Arc::new(
            resolve_policy_snapshot(
                PolicyRevision::new(revision).expect("revision"),
                HostPlatform::Linux,
                &scope,
                requested,
                &facts,
                absolute(&home),
                absolute(&tmp),
                absolute(&scratch),
                private_roots,
            )
            .expect("resolve confined policy snapshot"),
        );
        probe_backend_with_test_revision_roots(facts, snapshot, roots)
            .expect("trusted bubblewrap launcher and real Linux backend are required")
    }
    fn resolved_backend_with_denied_paths(
        roots: &OwnedRoots,
        test_name: &str,
        mode: SandboxMode,
        network: NetworkPolicy,
        revision: u64,
        denied_paths: Vec<AbsolutePath>,
    ) -> pi_sandbox::SandboxBackend {
        let facts = derive_platform_facts(
            PlatformFactInputs::new(
                test_name.to_owned(),
                vec![absolute(&roots.workspace)],
                absolute(&roots.agent),
                absolute(&roots.session),
            )
            .expect("valid platform fact inputs"),
        )
        .expect("Linux platform facts and backend prerequisites must be available");
        let revision_root = roots.session.join("sandbox").join(revision.to_string());
        let home = revision_root.join("home");
        let tmp = revision_root.join("tmp");
        let scratch = revision_root.join("scratch");
        for path in [&home, &tmp, &scratch] {
            std::fs::create_dir_all(path).expect("create revision projection");
        }
        let scope = SessionScope::new(
            test_name.to_owned(),
            vec![absolute(&roots.workspace)],
            absolute(&roots.agent),
            absolute(&roots.session),
        )
        .expect("valid session scope");
        let requested = RequestedPolicy::new(
            mode,
            network,
            Vec::new(),
            Vec::new(),
            denied_paths,
            HostHttpPolicy::new(Vec::new()).expect("empty host HTTP policy"),
            CredentialPolicy::new(Vec::new()).expect("empty credential policy"),
            ResourceLimits::confined_defaults(),
        )
        .expect("valid requested policy");
        let snapshot = Arc::new(
            resolve_policy_snapshot(
                PolicyRevision::new(revision).expect("revision"),
                HostPlatform::Linux,
                &scope,
                requested,
                &facts,
                absolute(&home),
                absolute(&tmp),
                absolute(&scratch),
                Vec::new(),
            )
            .expect("resolve confined policy snapshot"),
        );
        probe_backend_with_test_revision_roots(facts, snapshot, roots)
            .expect("trusted bubblewrap launcher and real Linux backend are required")
    }


    fn loopback_listener() -> (TcpListener, u16) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback listener");
        listener
            .set_nonblocking(true)
            .expect("set loopback listener nonblocking");
        let port = listener.local_addr().expect("listener address").port();
        (listener, port)
    }

    fn accept_with_timeout(listener: &TcpListener) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match listener.accept() {
                Ok((_stream, _address)) => return,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    panic!("loopback listener did not receive a connection before timeout")
                }
                Err(error) => panic!("accept loopback connection: {error}"),
            }
        }
    }

    fn workspace_write_full_network_allows_socket_but_keeps_filesystem_confined() {
        let roots = OwnedRoots::new();
        let current =
            std::fs::canonicalize("/proc/self/exe").expect("resolve hosted proof executable");
        std::fs::write(
            roots._root.path().join("network-host-sentinel"),
            b"host-only",
        )
        .expect("create host-only sentinel");
        let backend = resolved_backend(
            &roots,
            "workspace-write-full-network",
            SandboxMode::WorkspaceWrite,
            NetworkPolicy::FullAccess,
            11,
        );
        let (listener, port) = loopback_listener();
        // The policy mounts the workspace itself. A caller mount of that root
        // covers the mandatory protected descendants (.git, .rpi, .agents).
        let (status, output) = run_target_with_arguments(
            "workspace-write-full-network",
            &roots,
            &current,
            Vec::new(),
            &backend,
            vec![OsString::from(port.to_string())],
        );
        assert_eq!(
            status.code(),
            Some(0),
            "workspace-write full-network target"
        );
        accept_with_timeout(&listener);
        assert!(
            output.contains("full-network=connected"),
            "target output: {output:?}"
        );
        assert!(
            output.contains("workspace=written"),
            "target output: {output:?}"
        );
        assert!(
            output.contains("sentinel=hidden"),
            "target output: {output:?}"
        );
        assert!(
            output.contains("ptrace=blocked"),
            "target output: {output:?}"
        );
        assert_eq!(
            std::fs::read(roots.workspace.join("full-network-workspace-proof.txt"))
                .expect("read workspace full-network proof"),
            b"workspace-write-ok"
        );
    }

    fn read_only_network_modes_preserve_read_only_filesystem() {
        for (label, network) in [
            ("none", NetworkPolicy::None),
            ("full", NetworkPolicy::FullAccess),
        ] {
            let roots = OwnedRoots::new();
            std::fs::write(roots.workspace.join("read-only-input.txt"), b"read-only-ok")
                .expect("create read-only input");
            let current =
                std::fs::canonicalize("/proc/self/exe").expect("resolve hosted proof executable");
            let backend = resolved_backend(
                &roots,
                &format!("read-only-network-{label}"),
                SandboxMode::ReadOnly,
                network,
                if label == "none" { 12 } else { 13 },
            );
            let (listener, port) = loopback_listener();
            let (status, output) = run_target_with_arguments(
                "read-only-network",
                &roots,
                &current,
                Vec::new(),
                &backend,
                vec![OsString::from(label), OsString::from(port.to_string())],
            );
            assert_eq!(status.code(), Some(0), "read-only {label} target");
            assert!(
                !roots.workspace.join("read-only-network-write.txt").exists(),
                "read-only {label} must not create host workspace output"
            );
            if label == "full" {
                accept_with_timeout(&listener);
                assert!(
                    output.contains("network-full=connected"),
                    "target output: {output:?}"
                );
            } else {
                assert!(
                    output.contains("network-none=eperm"),
                    "target output: {output:?}"
                );
                assert!(
                    matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
                );
            }
            assert!(
                output.contains("filesystem=read-only"),
                "target output: {output:?}"
            );
        }
    }

    fn home_workspace_denied_store_keeps_scratch_and_control_writable() {
        let roots = OwnedRoots::home_nested();
        std::fs::write(roots.agent.join("teams.json"), b"confidential-default")
            .expect("default store sentinel");
        let current =
            std::fs::canonicalize("/proc/self/exe").expect("resolve hosted proof executable");
        let backend = resolved_backend_with_private_roots(
            &roots,
            "home-store-deny",
            SandboxMode::WorkspaceWrite,
            NetworkPolicy::None,
            21,
            vec![absolute(&roots.agent)],
        );
        // The workspace is already a writable root. Mounting it again would
        // cover the private agent store that sits inside the home workspace.
        let (status, output) = run_target_with_backend(
            "home-store-deny",
            &roots,
            &current,
            Vec::new(),
            &backend,
        );
        assert_eq!(status.code(), Some(0), "home-store-deny target");
        assert!(
            output.contains("store-hidden=ok"),
            "target output: {output:?}"
        );
        assert!(
            output.contains("workspace-write=ok"),
            "target output: {output:?}"
        );
        assert!(
            output.contains("scratch-write=ok"),
            "target output: {output:?}"
        );
        assert_eq!(
            std::fs::read(roots.workspace.join("control.txt")).expect("host control marker"),
            b"workspace-write-ok"
        );
        assert!(
            !roots.agent.join("teams-new.json").exists(),
            "denied store must not accept a new host file"
        );
        let scratch_host = roots
            .session
            .join("sandbox")
            .join("21")
            .join("scratch")
            .join("home-store-scratch-ok");
        assert_eq!(
            std::fs::read(&scratch_host).expect("host scratch via retained mount"),
            b"scratch-write-ok"
        );
    }


    fn confined_linux_enforces_empty_root_fd_mounts_limits_nnp_and_seccomp() {
        let roots = OwnedRoots::new();
        let current =
            std::fs::canonicalize("/proc/self/exe").expect("resolve hosted proof executable");
        let facts = derive_platform_facts(
            PlatformFactInputs::new(
                TEST_NAME.to_owned(),
                vec![absolute(&roots.workspace)],
                absolute(&roots.agent),
                absolute(&roots.session),
            )
            .expect("valid platform fact inputs"),
        )
        .expect("Linux platform facts and backend prerequisites must be available");
        let revision = roots.session.join("sandbox").join("1");
        let home = revision.join("home");
        let tmp = revision.join("tmp");
        let scratch = revision.join("scratch");
        for path in [&home, &tmp, &scratch] {
            std::fs::create_dir_all(path).expect("create revision projection");
        }
        let denied_file = roots._root.path().join("denied-file");
        std::fs::write(&denied_file, b"host-secret").expect("create denied file");
        let filesystem = FileSystemPolicy::resolved(
            facts
                .runtime_roots()
                .iter()
                .cloned()
                .chain([absolute(&home), absolute(&tmp)])
                .collect(),
            vec![
                WritableRoot::new(absolute(&roots.workspace), Vec::new())
                    .expect("valid workspace root"),
            ],
            vec![absolute(&denied_file)],
            absolute(&scratch),
        )
        .expect("valid resolved confined filesystem policy");
        let policy = SandboxPolicy::workspace_write(
            filesystem,
            NetworkPolicy::None,
            HostHttpPolicy::new(Vec::new()).expect("empty host HTTP policy"),
            CredentialPolicy::new(Vec::new()).expect("empty credential policy"),
            ResourceLimits::new(
                ADDRESS_SPACE_LIMIT,
                CPU_LIMIT_SECONDS,
                FILE_SIZE_LIMIT,
                OPEN_FILES_LIMIT,
            )
            .expect("limits only reduce confined defaults"),
        )
        .expect("valid Linux confined policy");
        let snapshot = Arc::new(
            PolicySnapshot::new(
                PolicyRevision::new(1).expect("revision"),
                HostPlatform::Linux,
                policy,
            )
            .expect("policy snapshot"),
        );
        let backend = probe_backend_with_test_revision_roots(facts.clone(), snapshot, &roots)
            .expect("trusted bubblewrap launcher and real Linux backend are required");
        let host_only = roots._root.path().join("host-root-only-sentinel");
        std::fs::write(&host_only, b"must-not-be-visible")
            .expect("create undeclared host-root sentinel");
        std::fs::set_permissions(&roots.workspace, std::fs::Permissions::from_mode(0o700))
            .expect("set workspace permissions");

        let (filesystem_status, filesystem_output) = run_target_with_backend(
            "filesystem",
            &roots,
            &current,
            workspace_mount(&roots),
            &backend,
        );
        assert_eq!(filesystem_status.code(), Some(0), "filesystem hosted proof");
        assert!(filesystem_output.contains("hidden-root=ok"));
        assert!(filesystem_output.contains("workspace-write=ok"));
        assert_eq!(
            std::fs::read(roots.workspace.join("workspace-write-proof.txt"))
                .expect("read workspace proof from host"),
            b"workspace-write-ok"
        );
        let (denied_status, denied_output) = run_target_with_backend(
            "denied-file",
            &roots,
            &current,
            workspace_mount(&roots),
            &backend,
        );
        assert_eq!(denied_status.code(), Some(0), "denied file hosted proof");
        assert!(denied_output.contains("denied-file=masked"));
        assert_eq!(
            std::fs::read(&denied_file).expect("host denied file remains readable"),
            b"host-secret"
        );
        let (devices_status, devices_output) = run_target_with_backend(
            "devices",
            &roots,
            &current,
            workspace_mount(&roots),
            &backend,
        );
        assert_eq!(
            devices_status.code(),
            Some(0),
            "device projection hosted proof"
        );
        assert!(devices_output.contains("devices=usable"));
        let (cwd_descriptor_status, cwd_descriptor_output) = run_target_with_backend(
            "cwd-descriptor",
            &roots,
            &current,
            workspace_mount(&roots),
            &backend,
        );
        assert_eq!(
            cwd_descriptor_status.code(),
            Some(0),
            "retained cwd descriptor hosted proof"
        );
        assert!(cwd_descriptor_output.contains("cwd-descriptor=closed"));

        let (home_tmp_status, home_tmp_output) = run_target_with_backend(
            "home-tmp",
            &roots,
            &current,
            workspace_mount(&roots),
            &backend,
        );
        assert_eq!(home_tmp_status.code(), Some(0), "home/tmpdir hosted proof");
        assert!(home_tmp_output.contains("home-read-only=ok"));
        assert!(home_tmp_output.contains("tmpdir-write=ok"));
        assert!(
            !roots
                .session
                .join("sandbox/1/home/home-write-proof.txt")
                .exists(),
            "revision HOME stayed read-only on the host backing dir"
        );
        assert_eq!(
            std::fs::read(
                roots
                    .session
                    .join("sandbox/1/scratch/scratch-write-proof.txt")
            )
            .expect("read scratch proof from host"),
            b"scratch-write-ok"
        );

        let (limits_status, limits_output) = run_target_with_backend(
            "limits",
            &roots,
            &current,
            workspace_mount(&roots),
            &backend,
        );
        assert_eq!(limits_status.code(), Some(0), "rlimit values hosted proof");
        for label in ["address-space", "cpu-time", "file-size", "open-files"] {
            assert!(
                limits_output.contains(&format!("limit-{label}=ok")),
                "missing exact {label} limit assertion"
            );
        }

        let (nnp_status, nnp_output) = run_target_with_backend(
            "nnp-seccomp",
            &roots,
            &current,
            workspace_mount(&roots),
            &backend,
        );
        assert_eq!(nnp_status.code(), Some(0), "NNP/seccomp hosted proof");
        assert!(nnp_output.contains("nnp=ok"));
        assert!(nnp_output.contains("seccomp-ptrace=blocked"));

        let (file_status, _) = run_target_with_backend(
            "file-size",
            &roots,
            &current,
            workspace_mount(&roots),
            &backend,
        );
        assert_eq!(file_status.code(), Some(0), "RLIMIT_FSIZE outcome");
        let (open_status, _) = run_target_with_backend(
            "open-files",
            &roots,
            &current,
            workspace_mount(&roots),
            &backend,
        );
        assert_eq!(open_status.code(), Some(0), "RLIMIT_NOFILE outcome");
        let (address_status, _) = run_target_with_backend(
            "address-space",
            &roots,
            &current,
            workspace_mount(&roots),
            &backend,
        );
        assert_eq!(address_status.code(), Some(0), "RLIMIT_AS outcome");
        let (cpu_status, _) = run_target_with_backend(
            "cpu-time",
            &roots,
            &current,
            workspace_mount(&roots),
            &backend,
        );
        // RLIMIT_CPU with soft == hard == cpu_seconds terminates an infinite-CPU
        // target by the kernel's hard-limit SIGKILL (with soft == hard the soft
        // SIGXCPU does not get to terminate first). SIGKILL is the canonical
        // unblockable CPU-cap kill, so observing it is the direct, non-weakening
        // proof that the configured 1s limit terminated the spin-loop target.
        // shell_code() is used because the bwrap launcher normalizes a
        // signal-killed child into shell convention 128 + signal (it may re-raise
        // the signal -> WIFSIGNALED, or exit with 128 + signal -> WIFEXITED); both
        // yield 128 + SIGKILL, and a clean exit is impossible for the spin loop so
        // 137 unambiguously attributes termination to the RLIMIT_CPU hard cap.
        assert_eq!(
            cpu_status.shell_code(),
            128 + libc::SIGKILL,
            "RLIMIT_CPU must hard-kill the spin-loop target (shell code 128+SIGKILL)"
        );
        // Read-only descriptor authority proof: the workspace is mounted
        // read-only, so neither a descriptor resolving to the sandbox cwd nor
        // a normal cwd write can create a file on the host. Under the old
        // fchdir-to-host-fd behavior this fails even if that fd is closed
        // before exec, because the child cwd would point at the host directory.
        let read_only_scope = SessionScope::new(
            format!("{TEST_NAME}-readonly"),
            vec![absolute(&roots.workspace)],
            absolute(&roots.agent),
            absolute(&roots.session),
        )
        .expect("valid read-only session scope");
        let read_only_requested = RequestedPolicy::new(
            SandboxMode::ReadOnly,
            NetworkPolicy::None,
            Vec::new(),
            Vec::new(),
            vec![absolute(&denied_file)],
            HostHttpPolicy::new(Vec::new()).expect("empty host HTTP policy"),
            CredentialPolicy::new(Vec::new()).expect("empty credential policy"),
            ResourceLimits::new(
                ADDRESS_SPACE_LIMIT,
                CPU_LIMIT_SECONDS,
                FILE_SIZE_LIMIT,
                OPEN_FILES_LIMIT,
            )
            .expect("limits only reduce confined defaults"),
        )
        .expect("valid read-only requested policy");
        let read_only_snapshot = Arc::new(
            resolve_policy_snapshot(
                PolicyRevision::new(1).expect("revision"),
                HostPlatform::Linux,
                &read_only_scope,
                read_only_requested,
                &facts,
                absolute(&home),
                absolute(&tmp),
                absolute(&scratch),
                Vec::new(),
            )
            .expect("resolve read-only policy snapshot"),
        );
        let read_only_backend =
            probe_backend_with_test_revision_roots(facts, read_only_snapshot, &roots)
                .expect("trusted bubblewrap launcher for read-only proof");
        let (ro_fd_status, ro_fd_output) = run_target_with_backend(
            "read-only-fd-proof",
            &roots,
            &current,
            Vec::new(),
            &read_only_backend,
        );
        assert_eq!(ro_fd_status.code(), Some(0), "read-only descriptor proof");
        assert!(
            ro_fd_output.contains("read-only-fd=ok"),
            "read-only descriptor proof output: {ro_fd_output:?}"
        );
        assert!(
            !ro_fd_output.contains("descriptor-authority=leaked"),
            "descriptor authority leaked through sandbox cwd: {ro_fd_output:?}"
        );
        assert!(
            !ro_fd_output.contains("cwd-write=leaked"),
            "cwd write leaked to host in read-only mode: {ro_fd_output:?}"
        );
        assert!(
            !roots.workspace.join("fd-authority-proof.txt").exists(),
            "descriptor-authority write must not create a host file"
        );
        assert!(
            !roots.workspace.join("cwd-write-proof.txt").exists(),
            "cwd write must not create a host file in read-only mode"
        );
    }

    pub(crate) fn run() {
        let mut arguments = std::env::args_os();
        let _binary = arguments.next();
        if arguments.next().as_deref() == Some(std::ffi::OsStr::new(TARGET_MODE)) {
            let target = arguments
                .next()
                .expect("hosted Linux proof target name")
                .into_string()
                .expect("UTF-8 target name");
            let owned_root = PathBuf::from(arguments.next().expect("owned root path"));
            let extra_arguments = arguments.collect::<Vec<_>>();
            target_main(&target, &owned_root, &extra_arguments);
        }

        let selected = std::env::args()
            .skip(1)
            .find(|argument| !argument.starts_with('-') && argument != TARGET_MODE);
        let tests: [(&str, fn()); 4] = [
            (
                TEST_NAME,
                confined_linux_enforces_empty_root_fd_mounts_limits_nnp_and_seccomp,
            ),
            (
                "workspace_write_full_network_allows_socket_but_keeps_filesystem_confined",
                workspace_write_full_network_allows_socket_but_keeps_filesystem_confined,
            ),
            (
                "read_only_network_modes_preserve_read_only_filesystem",
                read_only_network_modes_preserve_read_only_filesystem,
            ),
            (
                "home_workspace_denied_store_keeps_scratch_and_control_writable",
                home_workspace_denied_store_keeps_scratch_and_control_writable,
            ),
        ];
        let selected_tests = match selected.as_deref() {
            Some(name) => {
                let matched = tests
                    .into_iter()
                    .filter(|(test_name, _)| *test_name == name)
                    .collect::<Vec<_>>();
                if matched.is_empty() {
                    panic!("no linux_backend test named {name}");
                }
                matched
            }
            None => tests.into_iter().collect(),
        };
        println!("running {} tests", selected_tests.len());
        let mut passed = 0;
        for (name, test) in selected_tests {
            print!("test {name} ... ");
            std::io::stdout().flush().expect("flush test header");
            match std::panic::catch_unwind(test) {
                Ok(()) => {
                    passed += 1;
                    println!("ok");
                }
                Err(payload) => {
                    println!("FAILED\n\ntest result: FAILED. {passed} passed; 1 failed");
                    std::panic::resume_unwind(payload);
                }
            }
        }
        println!("\ntest result: ok. {passed} passed; 0 failed");
    }
}

#[cfg(target_os = "linux")]
fn main() {
    let child_args = std::env::args_os().skip(1).collect::<Vec<_>>();
    if child_args == ["__sandbox-child", "3", "4"] {
        match pi_sandbox::exec_child_target(3, 4) {
            Ok(never) => match never {},
            Err(error) => panic!("sandbox child launch failed: {error}"),
        }
    }
    linux_proof::run()
}

#[cfg(not(target_os = "linux"))]
fn main() {}
