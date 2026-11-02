#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use pi_agent::{AbortController, AgentTool, AgentToolResult, ToolCallContext, ToolUpdateFn};
use pi_ai::providers::{FauxProviderOptions, FauxResponse, register_faux_provider};
use pi_ai::{ContentBlock, Model, StopReason, ToolCall};
use pi_coding::{
    Application, ApplicationEvent, ProcessKey, ProcessSpawnSpec, ProcessState, ResourceDiscovery,
    ResourceManager, ResourceManagerOptions, Session, SessionOptions, ToolSelection,
};
use serde_json::{Value, json};

/// Hermetic session following the shared published-generation pattern: the
/// agent configuration and SessionStore live OUTSIDE the writable `cwd`
/// workspace so the published sandbox generation has no home/workspace
/// symlink-masking or SessionStore authority failures. The directory is a
/// `tempfile::TempDir` that stays alive for the caller's test body.
async fn session(
    cwd: &Path,
) -> (
    Session,
    pi_ai::providers::FauxProviderRegistration,
    tempfile::TempDir,
) {
    let suffix = uuid::Uuid::now_v7().to_string();
    let api = format!("application-process-api-{suffix}");
    let provider = format!("application-process-provider-{suffix}");
    let model = Model {
        id: "application-process-model".to_owned(),
        name: "Application Process Model".to_owned(),
        api: api.clone(),
        provider: provider.clone(),
        ..Model::default()
    };
    let registration = register_faux_provider(FauxProviderOptions {
        api,
        provider,
        models: vec![model.clone()],
        chunk_size: 1,
    });

    let agent = tempfile::tempdir().expect("agent dir");
    let session_dir = agent.path().join("sessions");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    std::fs::write(
        agent.path().join("settings.json"),
        r#"{"sandbox":{"mode":"danger-full-access"}}"#,
    )
    .expect("danger settings");

    let mut resource_options = ResourceManagerOptions::new(cwd);
    resource_options.agent_dir = agent.path().to_path_buf();
    resource_options.disable_extensions = true;
    resource_options.disable_skills = true;
    resource_options.disable_prompt_templates = true;
    resource_options.disable_themes = true;
    resource_options.disable_context_files = true;
    let resources = ResourceManager::new(resource_options).expect("resources");

    let session = Session::new_with_additional_tools_filtered_and_discovery(
        SessionOptions {
            model,
            cwd: cwd.to_path_buf(),
            system_prompt: String::new(),
            thinking_level: pi_agent::ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: Default::default(),
            tools: None,
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
            auth_manager: None,
        },
        Vec::new(),
        ToolSelection::default(),
        ResourceDiscovery::Disabled,
    )
    .expect("session");
    session.set_session_dir(session_dir);
    session
        .start_new_recording()
        .expect("session recording");
    session
        .attach_resources(resources)
        .await
        .expect("attach resources");
    (session, registration, agent)
}

fn bash_tool(application: &Application) -> AgentTool {
    application
        .get_all_tools()
        .into_iter()
        .find(|tool| tool.name == "bash")
        .expect("application bash tool")
}

fn bash_context(arguments: Value) -> ToolCallContext {
    let (_controller, abort) = AbortController::new();
    let on_update: ToolUpdateFn = std::sync::Arc::new(|_result: AgentToolResult| {});
    ToolCallContext {
        tool_call_id: "application-bash-process".to_owned(),
        arguments,
        on_update,
        abort,
        model: None,
        approval_token: None,
    }
}

fn result_text(result: &AgentToolResult) -> &str {
    match result.content.first() {
        Some(ContentBlock::Text { text, .. }) => text,
        _ => "",
    }
}

fn spec(cwd: &Path, script: &str) -> ProcessSpawnSpec {
    ProcessSpawnSpec {
        argv: vec!["/bin/sh".to_owned(), "-c".to_owned(), script.to_owned()],
        cwd: cwd.to_path_buf(),
        env: BTreeMap::new(),
        tty: false,
        terminal_size: None,
        label: None,
        timeout_ms: None,
        output_bytes: None,
    }
}

#[tokio::test]
async fn application_forwards_structured_process_events() {
    let cwd = tempfile::tempdir().expect("tempdir");
    let (session, registration, _agent) = session(cwd.path()).await;
    let application = Application::new(session).await;
    let mut events = application.subscribe();
    let process = application
        .process_spawn(spec(cwd.path(), "printf app-event"))
        .await
        .expect("spawn");
    application
        .process_wait(&process.id, Some(Duration::from_secs(3)))
        .await
        .expect("wait");
    let mut saw_started = false;
    let mut saw_output = false;
    let mut saw_exited = false;
    while !saw_exited {
        let event = tokio::time::timeout(Duration::from_secs(3), events.recv())
            .await
            .expect("event timeout")
            .expect("event");
        match event {
            ApplicationEvent::Process(pi_coding::ProcessEvent::ProcessStarted {
                process: event,
            }) => saw_started = event.id == process.id,
            ApplicationEvent::Process(pi_coding::ProcessEvent::ProcessOutput { id, .. }) => {
                saw_output |= id == process.id
            }
            ApplicationEvent::Process(pi_coding::ProcessEvent::ProcessExited {
                process: event,
            }) => saw_exited = event.id == process.id,
            _ => {}
        }
    }
    assert!(saw_started && saw_output && saw_exited);
    registration.unregister();
}

#[tokio::test]
async fn supervised_http_server_like_bash_is_visible_and_application_owned() {
    let cwd = tempfile::tempdir().expect("tempdir");
    let (session, registration, _agent) = session(cwd.path()).await;
    let application = Application::new(session).await;
    let bash = bash_tool(&application);
    let prompt = application.session().system_prompt().await;
    assert!(
        prompt.contains("set background=true"),
        "bash supervision guidance missing: {prompt}"
    );
    assert!(
        prompt.contains("never use nohup, setsid, disown, or shell '&'"),
        "detach prohibition missing: {prompt}"
    );

    let result = (bash.execute)(bash_context(json!({
        "command": "nohup python3 -u -c 'import time; print(\"http-server-ready\", flush=True); time.sleep(30)' &",
        "background": true
    })))
    .await
    .expect("supervised background bash");
    let id: pi_coding::ProcessId =
        serde_json::from_value(result.details["id"].clone()).expect("stable process id");
    assert!(
        result_text(&result).contains("visible in /ps"),
        "background result must direct the caller to the authoritative process list"
    );

    let listed = application.process_list();
    assert_eq!(
        listed.len(),
        1,
        "background bash must enter Application ProcessManager"
    );
    assert_eq!(listed[0].id, id);
    assert_eq!(listed[0].owner_id, application.session().process_owner_id());
    assert_eq!(listed[0].state, ProcessState::Running);
    assert!(application.process_describe(&id).is_ok());

    let logs = tokio::time::timeout(Duration::from_secs(3), async {
        let mut cursor = 0;
        let mut output = Vec::new();
        loop {
            let batch = application
                .process_logs(&id, cursor, None, true, Some(Duration::from_millis(200)))
                .await
                .expect("process logs");
            cursor = batch.cursor;
            output.extend(
                batch
                    .chunks
                    .iter()
                    .flat_map(pi_coding::ProcessLogChunk::bytes),
            );
            if output
                .windows(b"http-server-ready".len())
                .any(|window| window == b"http-server-ready")
            {
                break output;
            }
        }
    })
    .await
    .expect("server readiness log timeout");
    assert!(String::from_utf8_lossy(&logs).contains("http-server-ready"));

    let pid = listed[0].pid.expect("managed pid") as i32;
    application.cleanup().await;
    assert!(
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err(),
        "Application cleanup must stop bash-supervised server"
    );
    registration.unregister();
}

#[tokio::test]
async fn unsupervised_detach_is_rejected_but_ordinary_foreground_bash_is_unchanged() {
    let cwd = tempfile::tempdir().expect("tempdir");
    let (session, registration, _agent) = session(cwd.path()).await;
    let application = Application::new(session).await;
    let bash = bash_tool(&application);

    let error = (bash.execute)(bash_context(json!({
        "command": "nohup python3 -m http.server 8765 >/dev/null 2>&1 &"
    })))
    .await
    .expect_err("invisible nohup background escape must fail")
    .to_string();
    assert!(
        error.contains("background=true"),
        "actionable supervision instruction missing: {error}"
    );
    assert!(
        error.contains("/ps"),
        "authoritative process list instruction missing: {error}"
    );
    assert!(application.process_list().is_empty());

    let foreground = (bash.execute)(bash_context(json!({
        "command": "printf '%s\\n' 'quoted & text' escaped\\&value"
    })))
    .await
    .expect("ordinary foreground bash");
    assert_eq!(result_text(&foreground), "quoted & text\nescaped&value\n");
    assert!(application.process_list().is_empty());
    registration.unregister();
}

#[tokio::test]
async fn dropping_application_kills_owned_process() {
    let cwd = tempfile::tempdir().expect("tempdir");
    let (session, registration, _agent) = session(cwd.path()).await;
    let application = Application::new(session).await;
    let process = application
        .process_spawn(spec(cwd.path(), "exec sleep 30"))
        .await
        .expect("spawn");
    let pid = process.pid.expect("managed pid") as i32;
    assert!(application.process_describe(&process.id).is_ok());
    drop(application);
    for _ in 0..100 {
        if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err() {
            registration.unregister();
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("application-owned process survived Application drop");
}

#[tokio::test]
async fn cleanup_is_idempotent_and_kills_owned_process() {
    let cwd = tempfile::tempdir().expect("tempdir");
    let (session, registration, _agent) = session(cwd.path()).await;
    let application = Application::new(session).await;
    let process = application
        .process_spawn(spec(cwd.path(), "exec sleep 30"))
        .await
        .expect("spawn");
    let pid = process.pid.expect("managed pid") as i32;

    application.cleanup().await;
    application.cleanup().await;

    for _ in 0..100 {
        if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err() {
            registration.unregister();
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("application-owned process survived repeated cleanup");
}

#[tokio::test]
async fn cleanup_returns_early_for_cooperative_process() {
    // The published fixture uses the default 1s terminate grace. A cooperative
    // `exec sleep 30` must settle materially sooner; the old fixed grace sleep
    // always consumed the full second and fails the bound below.
    let cwd = tempfile::tempdir().expect("tempdir");
    let (session, registration, _agent) = session(cwd.path()).await;
    let application = Application::new(session).await;
    let process = application
        .process_spawn(spec(cwd.path(), "exec sleep 30"))
        .await
        .expect("spawn cooperative process");
    let pid = process.pid.expect("managed pid") as i32;

    let start = Instant::now();
    application.cleanup().await;
    let elapsed = start.elapsed();

    // 750ms tolerates scheduler jitter while staying well under the 1s grace
    // the old fixed sleep always consumed.
    assert!(
        elapsed < Duration::from_millis(750),
        "cleanup took {elapsed:?}; expected early cooperative shutdown",
    );
    assert!(
        application.process_list().is_empty(),
        "terminal owner process must be removed from the registry",
    );
    for _ in 0..100 {
        if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err() {
            registration.unregister();
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("cooperative process survived cleanup");
}

#[tokio::test]
async fn cleanup_waits_for_grace_then_kills_sigterm_ignoring_process_group() {
    let cwd = tempfile::tempdir().expect("tempdir");
    let ready = cwd.path().join("sigterm-trap-ready");
    let (session, registration, _agent) = session(cwd.path()).await;
    let application = Application::new(session).await;
    let script = format!(
        "trap '' TERM; : > '{}'; while :; do sleep 1; done",
        ready.display()
    );
    let process = application
        .process_spawn(spec(cwd.path(), &script))
        .await
        .expect("spawn SIGTERM-ignoring process group");
    let pid = process.pid.expect("managed pid") as i32;

    // Do not race cleanup against shell startup: publication of this file is
    // sequenced after the shell installs the ignored SIGTERM disposition that
    // its subsequently exec'd `sleep` children inherit.
    let ready_deadline = Instant::now() + Duration::from_secs(3);
    while !ready.exists() {
        assert!(
            Instant::now() < ready_deadline,
            "SIGTERM-ignoring process never became ready"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let start = Instant::now();
    application.cleanup().await;
    let elapsed = start.elapsed();
    let processes = application.process_list();
    let mut pid_dead = false;
    for _ in 0..100 {
        if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err() {
            pid_dead = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    registration.unregister();

    assert!(
        elapsed >= Duration::from_millis(750),
        "cleanup took only {elapsed:?}; SIGKILL must wait for the 1s grace",
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "cleanup took {elapsed:?}; SIGKILL settlement must remain under the shared final bound",
    );
    assert!(
        processes.is_empty(),
        "SIGKILL-confirmed owner process must be removed from the registry",
    );
    assert!(pid_dead, "SIGTERM-ignoring process survived cleanup");
}

#[tokio::test]
async fn new_session_stops_owned_process() {
    let cwd = tempfile::tempdir().expect("tempdir");
    let (session, registration, _agent) = session(cwd.path()).await;
    let application = Application::new(session).await;
    let process = application
        .process_spawn(spec(cwd.path(), "exec sleep 30"))
        .await
        .expect("spawn");
    let pid = process.pid.expect("managed pid") as i32;
    assert!(
        !application
            .new_session()
            .await
            .expect("new session")
            .cancelled
    );
    for _ in 0..100 {
        if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err() {
            registration.unregister();
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("process survived logical session change");
}

#[tokio::test]
async fn direct_pty_input_stays_out_of_application_events_and_session_history() {
    let directory = tempfile::tempdir().expect("tempdir");
    let (session, _registration, _agent) = session(directory.path()).await;
    let application = Application::new(session.clone()).await;
    let mut events = application.subscribe();
    let mut pty = spec(directory.path(), "read line; printf '<%s>' \"$line\"");
    pty.tty = true;
    let process = application.process_spawn(pty).await.expect("spawn PTY");

    application
        .process_write(&process.id, b"secret-input".to_vec(), false)
        .await
        .expect("write printable bytes");
    application
        .process_send_keys(&process.id, &[ProcessKey::Enter])
        .await
        .expect("write Enter");
    let exited = application
        .process_wait(&process.id, Some(Duration::from_secs(3)))
        .await
        .expect("wait PTY");
    assert_eq!(exited.exit_code, Some(0));

    while let Ok(event) = events.try_recv() {
        let serialized = serde_json::to_string(&event).expect("serialize event");
        assert!(!serialized.contains("secret-input"), "{serialized}");
    }
    assert!(
        session
            .history()
            .iter()
            .all(|message| !format!("{message:?}").contains("secret-input")),
        "direct PTY input must not enter session history",
    );
}

#[tokio::test]
async fn same_cwd_switch_session_stops_owned_process_and_drops_old_output() {
    let cwd = tempfile::tempdir().expect("tempdir");
    let sessions = tempfile::tempdir().expect("session dir");
    let (session, registration, _agent) = session(cwd.path()).await;
    session.set_session_dir(sessions.path().to_path_buf());
    let application = Application::new(session).await;

    let first = pi_coding::start_session_in(
        cwd.path(),
        None,
        Some("off"),
        Some(sessions.path()),
        Some("same-cwd-first"),
        None,
    )
    .expect("first recorder");
    first.persist_now().expect("persist first");
    let first_path = first.path();
    first.close().expect("close first");
    application
        .switch_session(&first_path)
        .await
        .expect("attach first");

    let process = application
        .process_spawn(spec(cwd.path(), "exec sleep 30"))
        .await
        .expect("spawn");
    let pid = process.pid.expect("managed pid") as i32;

    let second = pi_coding::start_session_in(
        cwd.path(),
        None,
        Some("off"),
        Some(sessions.path()),
        Some("same-cwd-second"),
        None,
    )
    .expect("second recorder");
    second.persist_now().expect("persist second");
    let second_path = second.path();
    second.close().expect("close second");

    let outcome = application
        .switch_session(&second_path)
        .await
        .expect("same cwd switch");
    assert!(!outcome.cancelled);
    assert!(
        application
            .process_list()
            .iter()
            .all(|info| info.state == ProcessState::Exited),
        "same-cwd switch must stop the prior logical session's owned processes: {:?}",
        application.process_list()
    );
    for _ in 0..100 {
        if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err() {
            registration.unregister();
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("process survived same-cwd logical session switch");
}

#[tokio::test]
async fn task_abort_keeps_supervised_process_then_explicit_stop_cleans_up() {
    let cwd = tempfile::tempdir().expect("tempdir");
    let (session, registration, _agent) = session(cwd.path()).await;
    let application = Application::new(session).await;
    let process = application
        .process_spawn(spec(cwd.path(), "exec sleep 30"))
        .await
        .expect("spawn supervised process");
    let pid = process.pid.expect("managed pid") as i32;

    // The exact task-interrupt entry point the TUI Esc/Ctrl-C path calls.
    application.abort().await;

    let listed = application.process_list();
    assert_eq!(
        listed.len(),
        1,
        "task abort must keep the supervised process listed"
    );
    assert_eq!(listed[0].id, process.id);
    assert_eq!(listed[0].state, ProcessState::Running);
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None)
        .expect("supervised process must stay alive across a task abort");

    // Only an explicit /ps stop terminates it. The ProcessExited event must
    // land on the application broadcast BEFORE the stop response returns, so
    // a Web client observing process_list cannot miss the terminal event.
    let mut events = application.subscribe();
    let stopped = application
        .process_stop(&process.id, Some(Duration::from_secs(3)))
        .await
        .expect("explicit stop");
    assert!(stopped.state.is_terminal());
    assert!(
        application.process_list().iter().all(|row| row.id != process.id),
        "explicit stop must drop the process from the live registry"
    );
    loop {
        let event = tokio::time::timeout(Duration::from_secs(3), events.recv())
            .await
            .expect("process_exited event after stop")
            .expect("event broadcast");
        if let ApplicationEvent::Process(pi_coding::ProcessEvent::ProcessExited {
            process: ref exited,
        }) = event
            && exited.id == process.id
        {
            break;
        }
    }
    for _ in 0..100 {
        if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err() {
            registration.unregister();
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("supervised process survived an explicit stop");
}

#[tokio::test]
async fn task_abort_cancels_in_flight_turn_but_keeps_supervised_process_alive() {
    let cwd = tempfile::tempdir().expect("tempdir");
    let (session, registration, _agent) = session(cwd.path()).await;
    let application = Application::new(session).await;

    // A supervised process owned by the session, started before the turn.
    let process = application
        .process_spawn(spec(cwd.path(), "sleep 30"))
        .await
        .expect("spawn supervised process");
    let pid = process.pid.expect("managed pid") as i32;

    // The agent's first turn issues a long-running foreground `bash` tool call
    // (the turn's foreground command); the second response ends the run.
    registration.set_responses(vec![
        FauxResponse {
            content: vec![ContentBlock::ToolCall(ToolCall {
                id: "call-sleep".to_owned(),
                name: "bash".to_owned(),
                arguments: json!({ "command": "sleep 30" }),
                thought_signature: None,
            })],
            stop_reason: StopReason::ToolUse,
            error_message: None,
        },
        FauxResponse {
            content: vec![ContentBlock::text("settled")],
            stop_reason: StopReason::Stop,
            error_message: None,
        },
    ]);

    let runner = application.clone();
    let run = tokio::spawn(async move {
        runner
            .prompt("run the foreground command".to_owned(), Vec::new(), None)
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        while !application.is_streaming() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("turn must start");
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Task interrupt mid-turn: the foreground bash child is cancelled, the
    // turn settles, and the supervised process must survive untouched.
    application.abort().await;
    tokio::time::timeout(Duration::from_secs(3), run)
        .await
        .expect("aborted turn must settle");

    let listed = application.process_list();
    assert_eq!(
        listed.len(),
        1,
        "task abort must keep the supervised process listed"
    );
    assert_eq!(listed[0].id, process.id);
    assert_eq!(listed[0].state, ProcessState::Running);
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None)
        .expect("supervised process must stay alive across an in-flight task abort");

    // Only an explicit /ps stop terminates it.
    application
        .process_stop(&process.id, Some(Duration::from_secs(3)))
        .await
        .expect("explicit stop");
    for _ in 0..100 {
        if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err() {
            registration.unregister();
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("supervised process survived an explicit stop");
}

#[tokio::test]
async fn concurrent_process_logs_are_not_corrupted() {
    let cwd = tempfile::tempdir().expect("tempdir");
    let (session, registration, _agent) = session(cwd.path()).await;
    let application = Application::new(session).await;
    let mut spawned = Vec::new();
    for index in 0..10 {
        let process = application
            .process_spawn(spec(
                cwd.path(),
                &format!("echo {index}_A; echo {index}_B; sleep 2"),
            ))
            .await
            .expect("spawn");
        spawned.push((process.id, index));
    }
    for (id, _) in &spawned {
        application
            .process_wait(id, Some(Duration::from_secs(6)))
            .await
            .expect("process wait");
    }
    for (id, index) in &spawned {
        let batch = application
            .process_logs(id, 0, Some(65536), false, None)
            .await
            .expect("process logs");
        let text = String::from_utf8_lossy(
            &batch
                .chunks
                .iter()
                .flat_map(|chunk| chunk.bytes())
                .collect::<Vec<u8>>(),
        )
        .into_owned();
        assert!(
            text.contains(&format!("{index}_A")),
            "process {index} lost line A: {text:?}"
        );
        assert!(
            text.contains(&format!("{index}_B")),
            "process {index} lost line B: {text:?}"
        );
        assert!(
            !text.contains('\u{0}'),
            "process {index} output contains NUL padding: {text:?}"
        );
    }
    registration.unregister();
    application.cleanup().await;
}
