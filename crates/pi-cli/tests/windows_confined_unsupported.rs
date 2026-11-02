#[cfg(target_os = "windows")]
mod windows_proof {

    use std::collections::BTreeMap;
    use std::io::Write as _;
    use std::path::Path;
    use std::time::Duration;

    use pi_coding::{
        Application, ApplicationRuntimeCandidate, ProcessSpawnSpec, ResourceManager,
        ResourceManagerOptions, Session, SessionOptions,
    };
    use tempfile::{Builder, TempDir};

    const TEST_NAME: &str = "absent_and_explicit_danger_launch_but_confined_fails";

    struct OwnedApplicationRoots {
        _root: TempDir,
        workspace: std::path::PathBuf,
        agent: std::path::PathBuf,
        sessions: std::path::PathBuf,
    }

    impl OwnedApplicationRoots {
        fn new(label: &str) -> Self {
            let root = Builder::new()
                .prefix(&format!("pi-windows-backend-{label}-"))
                .tempdir()
                .expect("create deterministic owned application roots");
            let workspace = root.path().join("workspace");
            let agent = root.path().join("agent");
            let sessions = root.path().join("sessions");
            for directory in [&workspace, &agent, &sessions] {
                std::fs::create_dir(directory).expect("create owned application directory");
            }
            Self {
                _root: root,
                workspace,
                agent,
                sessions,
            }
        }
    }

    async fn session(roots: &OwnedApplicationRoots) -> Session {
        let mut options = ResourceManagerOptions::new(&roots.workspace);
        options.agent_dir = roots.agent.clone();
        options.project_trust_override = Some(true);
        options.disable_extensions = true;
        options.disable_skills = true;
        options.disable_prompt_templates = true;
        options.disable_themes = true;
        options.disable_context_files = true;
        let resources = ResourceManager::new(options).expect("create deterministic resources");
        let session = Session::new(SessionOptions {
            model: pi_ai::Model::default(),
            cwd: roots.workspace.clone(),
            system_prompt: String::new(),
            thinking_level: pi_agent::ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
            auth_manager: None,
        })
        .expect("create deterministic session");
        session.set_session_dir(roots.sessions.clone());
        session
            .attach_resources(resources)
            .await
            .expect("attach deterministic resources");
        session
    }

    async fn build_application(
        label: &str,
        sandbox_json: Option<&str>,
    ) -> (OwnedApplicationRoots, anyhow::Result<Application>) {
        let roots = OwnedApplicationRoots::new(label);
        if let Some(sandbox_json) = sandbox_json {
            std::fs::write(
                roots.agent.join("settings.json"),
                format!(r#"{{"sandbox":{sandbox_json}}}"#),
            )
            .expect("write owned sandbox settings");
        }
        let application = Application::from_runtime_candidate(ApplicationRuntimeCandidate::new(
            session(&roots).await,
        ))
        .await;
        (roots, application)
    }

    fn assert_confined_startup_rejected(error: &anyhow::Error, mode: &str) {
        let text = format!("{error:#}");
        assert!(
            text.contains("sandbox mode") && text.contains("unsupported on Windows"),
            "{mode} must fail as an unsupported Windows confinement mode before any raw process construction: {text}"
        );
        assert!(
            !text.contains("cmd.exe")
                && !text.contains("ExecuteTarget")
                && !text.contains("CreateProcess"),
            "{mode} reached raw executable construction instead of failing at sandbox startup: {text}"
        );
    }

    fn pipe_spec(cwd: &Path) -> ProcessSpawnSpec {
        ProcessSpawnSpec {
            argv: vec![
                "cmd.exe".to_owned(),
                "/D".to_owned(),
                "/S".to_owned(),
                "/C".to_owned(),
                "<nul set /p =danger-pipe-ok".to_owned(),
            ],
            cwd: cwd.to_path_buf(),
            env: BTreeMap::new(),
            tty: false,
            terminal_size: None,
            label: Some("danger-pipe-proof".to_owned()),
            timeout_ms: Some(5_000),
            output_bytes: Some(64 * 1024),
        }
    }

    fn pty_spec(cwd: &Path) -> ProcessSpawnSpec {
        ProcessSpawnSpec {
            argv: vec![
                "cmd.exe".to_owned(),
                "/D".to_owned(),
                "/S".to_owned(),
                "/C".to_owned(),
                "echo danger-pty-ok".to_owned(),
            ],
            cwd: cwd.to_path_buf(),
            env: BTreeMap::new(),
            tty: true,
            terminal_size: Some(pi_coding::ProcessTerminalSize { rows: 24, cols: 80 }),
            label: Some("danger-pty-proof".to_owned()),
            timeout_ms: Some(5_000),
            output_bytes: Some(64 * 1024),
        }
    }

    async fn output(application: &Application, id: &pi_coding::ProcessId) -> Vec<u8> {
        application
            .process_logs(id, 0, Some(64 * 1024), false, None)
            .await
            .expect("read launched process output")
            .chunks
            .into_iter()
            .flat_map(|chunk| chunk.bytes())
            .collect()
    }

    async fn absent_and_explicit_danger_launch_but_confined_fails() {
        let (_absent_roots, absent) = build_application("absent", None).await;
        let absent = absent.expect("omitted sandbox mode must default to danger on Windows");
        absent.cleanup().await;

        let (_confined_roots, confined) =
            build_application("confined", Some(r#"{"mode":"workspace-write"}"#)).await;
        let confined_error = match confined {
            Ok(_) => panic!("explicit confined mode unexpectedly started on Windows"),
            Err(error) => error,
        };
        assert_confined_startup_rejected(&confined_error, "explicit workspace-write mode");

        let (danger_roots, danger) =
            build_application("danger", Some(r#"{"mode":"danger-full-access"}"#)).await;
        let application = danger.expect("explicit danger must start on Windows");

        let pipe = application
            .process_spawn(pipe_spec(&danger_roots.workspace))
            .await
            .expect("explicit danger pipe launch");
        let pipe = application
            .process_wait(&pipe.id, Some(Duration::from_secs(5)))
            .await
            .expect("wait explicit danger pipe");
        assert_eq!(pipe.exit_code, Some(0));
        assert_eq!(output(&application, &pipe.id).await, b"danger-pipe-ok");

        let pty = application
            .process_spawn(pty_spec(&danger_roots.workspace))
            .await
            .expect("explicit danger PTY launch");
        let pty = application
            .process_wait(&pty.id, Some(Duration::from_secs(5)))
            .await
            .expect("wait explicit danger PTY");
        assert_eq!(pty.exit_code, Some(0));
        let pty_output = output(&application, &pty.id).await;
        assert!(
            pty_output
                .windows(b"danger-pty-ok".len())
                .any(|window| window == b"danger-pty-ok"),
            "explicit danger PTY output missing: {:?}",
            String::from_utf8_lossy(&pty_output)
        );

        application.cleanup().await;
    }

    pub(crate) fn run() {
        println!("running 1 test");
        print!("test {TEST_NAME} ... ");
        std::io::stdout().flush().expect("flush test header");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("create hosted Windows test runtime");
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(absent_and_explicit_danger_launch_but_confined_fails());
        }));
        match outcome {
            Ok(()) => println!("ok\n\ntest result: ok. 1 passed; 0 failed"),
            Err(payload) => {
                println!("FAILED\n\ntest result: FAILED. 0 passed; 1 failed");
                std::panic::resume_unwind(payload);
            }
        }
    }
}

#[cfg(target_os = "windows")]
fn main() {
    windows_proof::run();
}

#[cfg(not(target_os = "windows"))]
fn main() {}
