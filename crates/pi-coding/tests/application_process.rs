#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use pi_agent::{AbortController, AgentTool, AgentToolResult, ToolCallContext, ToolUpdateFn};
use pi_ai::{ContentBlock, Model};
use pi_ai::providers::{FauxProviderOptions, register_faux_provider};
use pi_coding::{Application, ApplicationEvent, ProcessSpawnSpec, ProcessState, Session, SessionOptions};
use serde_json::{Value, json};

fn session(cwd: &Path) -> (Session, pi_ai::providers::FauxProviderRegistration) {
    let suffix = uuid::Uuid::now_v7().to_string();
    let api = format!("application-process-api-{suffix}");
    let provider = format!("application-process-provider-{suffix}");
    let model = Model { id: "application-process-model".to_owned(), name: "Application Process Model".to_owned(), api: api.clone(), provider: provider.clone(), ..Model::default() };
    let registration = register_faux_provider(FauxProviderOptions { api, provider, models: vec![model.clone()], chunk_size: 1 });
    let session = Session::new(SessionOptions { model, cwd: cwd.to_path_buf(), system_prompt: String::new(), thinking_level: pi_agent::ThinkingLevel::Off, api_key: String::new(), compaction: None, stream_options: Default::default(), tools: None, before_tool_call: None, after_tool_call: None, stream_fn: None, auth_resolver: None }).expect("session");
    (session, registration)
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
    }
}

fn result_text(result: &AgentToolResult) -> &str {
    match result.content.first() {
        Some(ContentBlock::Text { text, .. }) => text,
        _ => "",
    }
}

fn spec(cwd: &Path, script: &str) -> ProcessSpawnSpec {
    ProcessSpawnSpec { argv: vec!["/bin/sh".to_owned(), "-c".to_owned(), script.to_owned()], cwd: cwd.to_path_buf(), env: BTreeMap::new(), tty: false, terminal_size: None, label: None, timeout_ms: None, output_bytes: None }
}

#[tokio::test]
async fn application_forwards_structured_process_events() {
    let cwd = tempfile::tempdir().expect("tempdir");
    let (session, registration) = session(cwd.path());
    let application = Application::new(session).await;
    let mut events = application.subscribe();
    let process = application.process_spawn(spec(cwd.path(), "printf app-event")).await.expect("spawn");
    application.process_wait(&process.id, Some(Duration::from_secs(3))).await.expect("wait");
    let mut saw_started = false;
    let mut saw_output = false;
    let mut saw_exited = false;
    while !saw_exited {
        let event = tokio::time::timeout(Duration::from_secs(3), events.recv()).await.expect("event timeout").expect("event");
        match event {
            ApplicationEvent::Process(pi_coding::ProcessEvent::ProcessStarted { process: event }) => saw_started = event.id == process.id,
            ApplicationEvent::Process(pi_coding::ProcessEvent::ProcessOutput { id, .. }) => saw_output |= id == process.id,
            ApplicationEvent::Process(pi_coding::ProcessEvent::ProcessExited { process: event }) => saw_exited = event.id == process.id,
            _ => {}
        }
    }
    assert!(saw_started && saw_output && saw_exited);
    registration.unregister();
}

#[tokio::test]
async fn supervised_http_server_like_bash_is_visible_and_application_owned() {
    let cwd = tempfile::tempdir().expect("tempdir");
    let (session, registration) = session(cwd.path());
    let application = Application::new(session).await;
    let bash = bash_tool(&application);
    let prompt = application.session().system_prompt().await;
    assert!(prompt.contains("set background=true"), "bash supervision guidance missing: {prompt}");
    assert!(prompt.contains("never use nohup, setsid, disown, or shell '&'"), "detach prohibition missing: {prompt}");

    let result = (bash.execute)(bash_context(json!({
        "command": "nohup python3 -u -c 'import time; print(\"http-server-ready\", flush=True); time.sleep(30)' &",
        "background": true
    })))
    .await
    .expect("supervised background bash");
    let id: pi_coding::ProcessId = serde_json::from_value(result.details["id"].clone())
        .expect("stable process id");
    assert!(
        result_text(&result).contains("visible in /ps"),
        "background result must direct the caller to the authoritative process list"
    );

    let listed = application.process_list();
    assert_eq!(listed.len(), 1, "background bash must enter Application ProcessManager");
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
            output.extend(batch.chunks.iter().flat_map(pi_coding::ProcessLogChunk::bytes));
            if output.windows(b"http-server-ready".len()).any(|window| window == b"http-server-ready") {
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
    let (session, registration) = session(cwd.path());
    let application = Application::new(session).await;
    let bash = bash_tool(&application);

    let error = (bash.execute)(bash_context(json!({
        "command": "nohup python3 -m http.server 8765 >/dev/null 2>&1 &"
    })))
    .await
    .expect_err("invisible nohup background escape must fail")
    .to_string();
    assert!(error.contains("background=true"), "actionable supervision instruction missing: {error}");
    assert!(error.contains("/ps"), "authoritative process list instruction missing: {error}");
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
    let pid_file = cwd.path().join("application.pid");
    let (session, registration) = session(cwd.path());
    let application = Application::new(session).await;
    let process = application.process_spawn(spec(cwd.path(), &format!("echo $$ > '{}'; exec sleep 30", pid_file.display()))).await.expect("spawn");
    assert!(application.process_describe(&process.id).is_ok());
    let pid = loop {
        if let Ok(text) = std::fs::read_to_string(&pid_file) { break text.trim().parse::<i32>().expect("pid"); }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    drop(application);
    for _ in 0..100 {
        if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err() { registration.unregister(); return; }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("application-owned process survived Application drop");
}

#[tokio::test]
async fn cleanup_is_idempotent_and_kills_owned_process() {
    let cwd = tempfile::tempdir().expect("tempdir");
    let pid_file = cwd.path().join("cleanup.pid");
    let (session, registration) = session(cwd.path());
    let application = Application::new(session).await;
    application
        .process_spawn(spec(
            cwd.path(),
            &format!("echo $$ > '{}'; exec sleep 30", pid_file.display()),
        ))
        .await
        .expect("spawn");
    let pid = loop {
        if let Ok(text) = std::fs::read_to_string(&pid_file) {
            break text.trim().parse::<i32>().expect("pid");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };

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
async fn new_session_stops_owned_process() {
    let cwd = tempfile::tempdir().expect("tempdir");
    let pid_file = cwd.path().join("session-change.pid");
    let (session, registration) = session(cwd.path());
    let application = Application::new(session).await;
    application.process_spawn(spec(cwd.path(), &format!("echo $$ > '{}'; exec sleep 30", pid_file.display()))).await.expect("spawn");
    let pid = loop {
        if let Ok(text) = std::fs::read_to_string(&pid_file) { break text.trim().parse::<i32>().expect("pid"); }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    application.new_session().await.expect("new session");
    for _ in 0..100 {
        if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err() { registration.unregister(); return; }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("process survived logical session change");
}
