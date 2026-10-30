// The full macOS proof is macOS-only: Seatbelt types, the BSD sockaddr_in layout
// (sin_len), and the rlimit/pipe helpers are not portable. The whole proof lives
// in a cfg'd module so the harness=false binary still provides a crate-root
// `main` on other platforms (no-op there), while macOS keeps the self-reexec
// proof unchanged.
#[cfg(target_os = "macos")]
mod macos_proof {
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::fs::{File, OpenOptions};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::fs::OpenOptionsExt as _;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use pi_sandbox::{
        AbsolutePath, CredentialPolicy, FileSystemPolicy, HostHttpPolicy, HostPlatform,
        LaunchedUnixHandle, MountSource, NetworkPolicy, PipeStdio, PlatformFactInputs,
        PolicyRevision, PolicySnapshot, PreparedSpawnRequest, RelativePath, ResourceLimits,
        RetainedExecutable, SandboxPolicy, SpawnFamily, SpawnStdio, WritableRoot,
        derive_platform_facts, launch_prepared_unix, prepare_spawn, probe_backend,
    };
    use tempfile::{Builder, TempDir};

    const TEST_NAME: &str = "confined_macos_enforces_literal_seatbelt_and_limits";
    const TARGET_MODE: &str = "__pi_macos_backend_target";
    const ADDRESS_SPACE_LIMIT: u64 = 256 * 1024 * 1024;
    const CPU_LIMIT_SECONDS: u64 = 1;
    const FILE_SIZE_LIMIT: u64 = 4096;
    const OPEN_FILES_LIMIT: u64 = 32;

    struct OwnedRoots {
        _root: TempDir,
        workspace: PathBuf,
        protected: PathBuf,
        agent: PathBuf,
        session: PathBuf,
    }

    impl OwnedRoots {
        fn new() -> Self {
            let root = Builder::new()
                .prefix("pi-macos-backend-")
                .tempdir()
                .expect("create deterministic owned sandbox roots");
            let workspace = root.path().join("workspace");
            let protected = workspace.join("protected");
            let agent = root.path().join("agent");
            let session = root.path().join("session");
            for directory in [&workspace, &protected, &agent, &session] {
                std::fs::create_dir(directory).expect("create owned sandbox directory");
            }
            std::fs::write(protected.join("host-owned.txt"), b"protected")
                .expect("create protected proof file");
            Self {
                _root: root,
                workspace,
                protected,
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
            .expect("open owned directory for caller mount");
        file.into()
    }

    fn pipe_pair() -> (File, OwnedFd) {
        let mut descriptors = [-1; 2];
        assert_eq!(
            unsafe { libc::pipe(descriptors.as_mut_ptr()) },
            0,
            "create hosted proof pipe: {}",
            std::io::Error::last_os_error()
        );
        for descriptor in descriptors {
            let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
            assert!(flags >= 0, "inspect hosted proof pipe descriptor");
            assert_eq!(
                unsafe { libc::fcntl(descriptor, libc::F_SETFD, flags | libc::FD_CLOEXEC) },
                0,
                "set close-on-exec hosted proof pipe"
            );
        }
        let read = unsafe { File::from_raw_fd(descriptors[0]) };
        let write = unsafe { OwnedFd::from_raw_fd(descriptors[1]) };
        (read, write)
    }

    fn build_backend(roots: &OwnedRoots) -> (PathBuf, pi_sandbox::SandboxBackend) {
        let current = std::fs::canonicalize(
            std::env::current_exe().expect("resolve hosted proof executable"),
        )
        .expect("canonical hosted proof executable");
        let facts = derive_platform_facts(
            PlatformFactInputs::new(
                TEST_NAME.to_owned(),
                vec![absolute(&roots.workspace)],
                absolute(&roots.agent),
                absolute(&roots.session),
            )
            .expect("valid platform fact inputs"),
        )
        .expect("macOS platform facts and Seatbelt prerequisites must be available");
        let workspace = WritableRoot::new(
            absolute(&roots.workspace),
            vec![
                RelativePath::parse(PathBuf::from("protected"))
                    .expect("relative protected descendant"),
            ],
        )
        .expect("valid protected workspace root");
        let revision = roots.session.join("sandbox").join("1");
        let home = revision.join("home");
        let tmp = revision.join("tmp");
        let scratch = revision.join("scratch");
        for path in [&home, &tmp, &scratch] {
            std::fs::create_dir_all(path).expect("create revision projection");
        }
        let filesystem = FileSystemPolicy::resolved(
            facts
                .runtime_roots()
                .iter()
                .cloned()
                .chain([absolute(&home), absolute(&tmp)])
                .collect(),
            vec![workspace],
            Vec::new(),
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
        .expect("valid macOS confined policy");
        let snapshot = Arc::new(
            PolicySnapshot::new(
                PolicyRevision::new(1).expect("revision"),
                HostPlatform::Macos,
                policy,
            )
            .expect("policy snapshot"),
        );
        let retained = pi_sandbox::RetainedRevisionRoots::new(
            open_directory(&home),
            absolute(&home),
            open_directory(&tmp),
            absolute(&tmp),
            open_directory(&scratch),
            absolute(&scratch),
        )
        .expect("retained revision roots");
        let backend = pi_sandbox::probe_backend_with_revision_roots(facts, snapshot, retained)
            .expect("/usr/bin/sandbox-exec and a real macOS Seatbelt backend are required");
        (current, backend)
    }

    fn run_target(
        target: &str,
        roots: &OwnedRoots,
        current: &Path,
        backend: &pi_sandbox::SandboxBackend,
        listener_port: Option<u16>,
    ) -> (pi_sandbox::UnixExitStatus, String) {
        let (stdin_read, stdin_write) = pipe_pair();
        let target_file = File::open(current).expect("open retained hosted target executable");
        let retained_target = RetainedExecutable::new(target_file.into(), absolute(current))
            .expect("retain hosted target executable identity");

        drop(stdin_write);
        let (stdout_read, stdout_write) = pipe_pair();
        let (stderr_read, stderr_write) = pipe_pair();
        let stdio = PipeStdio::new(stdin_read.into(), stdout_write, stderr_write)
            .expect("unique child stdio descriptors");
        let mut argv = vec![
            current.as_os_str().to_owned(),
            OsString::from(TARGET_MODE),
            OsString::from(target),
        ];
        if let Some(port) = listener_port {
            argv.push(OsString::from(port.to_string()));
        }
        let request = PreparedSpawnRequest::new(
            SpawnFamily::ManagedPipeProcess,
            argv,
            absolute(&roots.workspace),
            {
                let mut environment = BTreeMap::new();
                environment.insert(
                    OsString::from("HOME"),
                    roots.session.join("sandbox/1/home").into_os_string(),
                );
                environment.insert(
                    OsString::from("TMPDIR"),
                    roots.session.join("sandbox/1/scratch").into_os_string(),
                );
                environment
            },
            SpawnStdio::Piped(stdio),
            Vec::new(),
            vec![
                MountSource::read_write_directory(
                    open_directory(&roots.workspace),
                    absolute(&roots.workspace),
                )
                .expect("workspace mount source"),
            ],
            Some(retained_target),
            None,
        )
        .expect("valid hosted proof spawn request");
        let launched = launch_prepared_unix(
            prepare_spawn(backend, request).expect("prepare real macOS confined spawn"),
        )
        .expect("launch real macOS Seatbelt target");
        let process = match launched {
            LaunchedUnixHandle::Pipe(process) => process,
            LaunchedUnixHandle::Pty(_) => panic!("hosted macOS proof unexpectedly returned a PTY"),
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
        let status = process.wait().expect("wait for hosted macOS proof target");
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

    fn target_main(target: &str, listener_port: Option<u16>) -> ! {
        match target {
            "seatbelt" => {
                let cwd = std::env::current_dir().expect("confined current directory");
                let protected = cwd.join("protected").join("child-write.txt");
                assert!(
                    std::fs::write(&protected, b"must-not-write").is_err(),
                    "literal protected descendant was writable"
                );
                let written = cwd.join("workspace-write-proof.txt");
                std::fs::write(&written, b"workspace-write-ok")
                    .expect("workspace root must be writable");
                assert_eq!(
                    std::fs::read(&written).expect("read workspace proof"),
                    b"workspace-write-ok"
                );
                println!("protected-path=denied workspace-write=ok");
            }
            "home-tmp" => {
                let home = std::env::var("HOME").expect("HOME projected into child");
                let tmpdir = std::env::var("TMPDIR").expect("TMPDIR projected into child");
                assert!(
                    std::fs::write(format!("{home}/home-write-proof.txt"), b"must-not-write")
                        .is_err(),
                    "revision HOME must remain read-only under Seatbelt"
                );
                let scratch_proof = format!("{tmpdir}/scratch-write-proof.txt");
                std::fs::write(&scratch_proof, b"scratch-write-ok")
                    .expect("TMPDIR sealed scratch must be writable under Seatbelt");
                assert_eq!(
                    std::fs::read(&scratch_proof).expect("read scratch proof"),
                    b"scratch-write-ok"
                );
                println!("home-read-only=ok tmpdir-write=ok");
            }
            "network" => {
                let port = listener_port.expect("loopback proof port");
                let address = libc::sockaddr_in {
                    sin_len: std::mem::size_of::<libc::sockaddr_in>() as u8,
                    sin_family: libc::AF_INET as u8,
                    sin_port: port.to_be(),
                    sin_addr: libc::in_addr {
                        s_addr: u32::from_ne_bytes([127, 0, 0, 1]).to_be(),
                    },
                    sin_zero: [0; 8],
                };
                let socket = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
                assert!(socket >= 0, "create loopback proof socket");
                let connected = unsafe {
                    libc::connect(
                        socket,
                        (&raw const address).cast(),
                        std::mem::size_of_val(&address) as libc::socklen_t,
                    )
                };
                let error = std::io::Error::last_os_error();
                unsafe { libc::close(socket) };
                assert_eq!(
                    connected, -1,
                    "NetworkPolicy::None allowed a loopback connect"
                );
                assert_eq!(
                    error.kind(),
                    std::io::ErrorKind::PermissionDenied,
                    "Seatbelt network denial must surface permission denied: {error}"
                );
                println!("network-none=denied");
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
                assert_eq!(error.raw_os_error(), Some(libc::EFBIG));
            }
            "open-files" => {
                let mut files = Vec::new();
                loop {
                    match File::open("/dev/null") {
                        Ok(file) => files.push(file),
                        Err(error) => {
                            assert_eq!(error.raw_os_error(), Some(libc::EMFILE));
                            assert!(files.len() < OPEN_FILES_LIMIT as usize);
                            break;
                        }
                    }
                }
            }
            "address-space" => {
                let requested = ADDRESS_SPACE_LIMIT.saturating_mul(4) as usize;
                let mapping = unsafe {
                    libc::mmap(
                        std::ptr::null_mut(),
                        requested,
                        libc::PROT_READ | libc::PROT_WRITE,
                        libc::MAP_PRIVATE | libc::MAP_ANON,
                        -1,
                        0,
                    )
                };
                assert_eq!(mapping, libc::MAP_FAILED);
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::ENOMEM)
                );
            }
            "cpu-time" => loop {
                std::hint::spin_loop();
            },
            other => panic!("unknown hosted macOS proof target {other:?}"),
        }
        std::process::exit(0)
    }

    fn confined_macos_enforces_literal_seatbelt_and_limits() {
        let roots = OwnedRoots::new();
        let (current, backend) = build_backend(&roots);

        let (seatbelt_status, seatbelt_output) =
            run_target("seatbelt", &roots, &current, &backend, None);
        assert_eq!(
            seatbelt_status.code(),
            Some(0),
            "literal Seatbelt filesystem proof"
        );
        assert!(seatbelt_output.contains("protected-path=denied"));
        assert!(seatbelt_output.contains("workspace-write=ok"));
        assert!(!roots.protected.join("child-write.txt").exists());
        assert_eq!(
            std::fs::read(roots.workspace.join("workspace-write-proof.txt"))
                .expect("read workspace proof from host"),
            b"workspace-write-ok"
        );

        let (home_tmp_status, home_tmp_output) =
            run_target("home-tmp", &roots, &current, &backend, None);
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

        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .expect("bind loopback proof listener");
        let port = listener.local_addr().expect("listener address").port();
        let (network_status, network_output) =
            run_target("network", &roots, &current, &backend, Some(port));
        assert_eq!(
            network_status.code(),
            Some(0),
            "Seatbelt network-none proof"
        );
        assert!(network_output.contains("network-none=denied"));
        drop(listener);

        let (limits_status, limits_output) = run_target("limits", &roots, &current, &backend, None);
        assert_eq!(limits_status.code(), Some(0), "rlimit values hosted proof");
        for label in ["address-space", "cpu-time", "file-size", "open-files"] {
            assert!(
                limits_output.contains(&format!("limit-{label}=ok")),
                "missing exact {label} limit assertion"
            );
        }

        let (file_status, _) = run_target("file-size", &roots, &current, &backend, None);
        assert_eq!(file_status.code(), Some(0), "RLIMIT_FSIZE outcome");
        let (open_status, _) = run_target("open-files", &roots, &current, &backend, None);
        assert_eq!(open_status.code(), Some(0), "RLIMIT_NOFILE outcome");
        let (address_status, _) = run_target("address-space", &roots, &current, &backend, None);
        assert_eq!(address_status.code(), Some(0), "RLIMIT_AS outcome");
        let (cpu_status, _) = run_target("cpu-time", &roots, &current, &backend, None);
        assert_eq!(
            cpu_status.signal(),
            Some(libc::SIGKILL),
            "RLIMIT_CPU outcome"
        );
    }

    pub(crate) fn run() {
        let mut arguments = std::env::args_os();
        let _binary = arguments.next();
        if arguments.next().as_deref() == Some(std::ffi::OsStr::new(TARGET_MODE)) {
            let target = arguments
                .next()
                .expect("hosted macOS proof target name")
                .into_string()
                .expect("UTF-8 target name");
            let listener_port = arguments.next().map(|value| {
                value
                    .into_string()
                    .expect("UTF-8 listener port")
                    .parse::<u16>()
                    .expect("numeric listener port")
            });
            target_main(&target, listener_port);
        }

        println!("running 1 test");
        print!("test {TEST_NAME} ... ");
        std::io::stdout().flush().expect("flush test header");
        let outcome = std::panic::catch_unwind(confined_macos_enforces_literal_seatbelt_and_limits);
        match outcome {
            Ok(()) => println!("ok\n\ntest result: ok. 1 passed; 0 failed"),
            Err(payload) => {
                println!("FAILED\n\ntest result: FAILED. 0 passed; 1 failed");
                std::panic::resume_unwind(payload);
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn main() {
    macos_proof::run()
}

#[cfg(not(target_os = "macos"))]
fn main() {}
