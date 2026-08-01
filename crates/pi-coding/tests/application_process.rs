#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use pi_ai::Model;
use pi_ai::providers::{FauxProviderOptions, register_faux_provider};
use pi_coding::{Application, ApplicationEvent, ProcessSpawnSpec, Session, SessionOptions};

fn session(cwd: &Path) -> (Session, pi_ai::providers::FauxProviderRegistration) {
    let suffix = uuid::Uuid::now_v7().to_string();
    let api = format!("application-process-api-{suffix}");
    let provider = format!("application-process-provider-{suffix}");
    let model = Model { id: "application-process-model".to_owned(), name: "Application Process Model".to_owned(), api: api.clone(), provider: provider.clone(), ..Model::default() };
    let registration = register_faux_provider(FauxProviderOptions { api, provider, models: vec![model.clone()], chunk_size: 1 });
    let session = Session::new(SessionOptions { model, cwd: cwd.to_path_buf(), system_prompt: String::new(), thinking_level: pi_agent::ThinkingLevel::Off, api_key: String::new(), compaction: None, stream_options: Default::default(), tools: None, before_tool_call: None, after_tool_call: None, stream_fn: None, auth_resolver: None }).expect("session");
    (session, registration)
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
