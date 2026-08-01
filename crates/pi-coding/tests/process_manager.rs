#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use base64::Engine as _;
use pi_coding::{
    ProcessEvent, ProcessKey, ProcessManager, ProcessManagerConfig, ProcessOwnerId, ProcessSignal,
    ProcessSpawnSpec, ProcessState, ProcessTerminalSize,
};

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

async fn all_output(
    manager: &ProcessManager,
    owner: &ProcessOwnerId,
    id: &pi_coding::ProcessId,
) -> Vec<u8> {
    let logs = manager
        .logs(owner, id, 0, Some(1024 * 1024), false, None)
        .await
        .expect("read logs");
    logs.chunks
        .into_iter()
        .flat_map(|chunk| chunk.bytes())
        .collect()
}

#[tokio::test]
async fn output_cursor_truncation_and_serialized_events() {
    let directory = tempfile::tempdir().expect("tempdir");
    let manager = ProcessManager::with_config(ProcessManagerConfig {
        max_output_bytes: 5,
        idle_timeout: None,
        ..ProcessManagerConfig::default()
    });
    let owner = ProcessOwnerId::new("owner-a");
    let mut events = manager.subscribe();
    let info = manager
        .spawn(owner.clone(), spec(directory.path(), "printf 123456789"))
        .await
        .expect("spawn");
    let exited = manager
        .wait(&owner, &info.id, Some(Duration::from_secs(3)))
        .await
        .expect("wait");
    assert_eq!(exited.output_start_cursor, 4);
    assert_eq!(exited.output_cursor, 9);
    let logs = manager
        .logs(&owner, &info.id, 0, Some(64), false, None)
        .await
        .expect("logs");
    assert!(logs.lost);
    assert_eq!(logs.lost_bytes, 4);
    let logs_json = serde_json::to_value(&logs).expect("serialize logs");
    assert_eq!(logs_json["requestedCursor"], 0);
    assert_eq!(logs_json["startCursor"], 4);
    assert_eq!(logs_json["cursor"], 9);
    assert_eq!(logs_json["lost"], true);
    assert_eq!(logs_json["lostBytes"], 4);
    assert_eq!(
        logs.chunks
            .iter()
            .flat_map(pi_coding::ProcessLogChunk::bytes)
            .collect::<Vec<_>>(),
        b"56789"
    );

    let started = events.recv().await.expect("started event");
    let output = events.recv().await.expect("output event");
    let exited_event = events.recv().await.expect("exit event");
    assert!(matches!(started, ProcessEvent::ProcessStarted { .. }));
    if let ProcessEvent::ProcessOutput {
        start_cursor,
        cursor,
        ref data_base64,
        ..
    } = output
    {
        assert!(cursor > start_cursor);
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(data_base64)
                .expect("event base64"),
            b"123456789"
        );
        let output_json = serde_json::to_value(&output).expect("serialize output event");
        assert_eq!(output_json["type"], "process_output");
        assert_eq!(output_json["startCursor"], 0);
        assert_eq!(output_json["cursor"], 9);
    } else {
        panic!("expected output event");
    }
    assert!(matches!(exited_event, ProcessEvent::ProcessExited { .. }));
    let serialized = serde_json::to_value(exited_event).expect("serialize event");
    assert_eq!(serialized["type"], "process_exited");
    assert_eq!(serialized["process"]["id"], info.id.as_str());
}

#[tokio::test]
async fn merged_pipe_preserves_stdout_stderr_order() {
    let directory = tempfile::tempdir().expect("tempdir");
    let manager = ProcessManager::new();
    let owner = ProcessOwnerId::new("owner");
    let info = manager
        .spawn(
            owner.clone(),
            spec(directory.path(), "printf A; printf B >&2; printf C"),
        )
        .await
        .expect("spawn");
    manager
        .wait(&owner, &info.id, Some(Duration::from_secs(3)))
        .await
        .expect("wait");
    assert_eq!(all_output(&manager, &owner, &info.id).await, b"ABC");
}

#[tokio::test]
async fn pipe_input_exit_and_owner_isolation() {
    let directory = tempfile::tempdir().expect("tempdir");
    let manager = ProcessManager::new();
    let owner = ProcessOwnerId::new("owner");
    let other = ProcessOwnerId::new("other");
    let info = manager
        .spawn(
            owner.clone(),
            spec(directory.path(), "read line; printf '<%s>' \"$line\""),
        )
        .await
        .expect("spawn");
    assert!(manager.describe(&other, &info.id).is_err());
    assert!(
        manager
            .logs(&other, &info.id, 0, None, false, None)
            .await
            .is_err()
    );
    manager
        .write(&owner, &info.id, b"hello\n".to_vec(), true)
        .await
        .expect("write");
    let exited = manager
        .wait(&owner, &info.id, Some(Duration::from_secs(3)))
        .await
        .expect("wait");
    assert_eq!(exited.exit_code, Some(0));
    assert_eq!(all_output(&manager, &owner, &info.id).await, b"<hello>");
}

#[tokio::test]
async fn pty_input_resize_and_signal() {
    let directory = tempfile::tempdir().expect("tempdir");
    let manager = ProcessManager::new();
    let owner = ProcessOwnerId::new("pty-owner");
    let mut pty_spec = spec(directory.path(), "stty size; read line; printf '<%s>' \"$line\"; sleep 30");
    pty_spec.tty = true;
    pty_spec.terminal_size = Some(ProcessTerminalSize { rows: 24, cols: 80 });
    let info = manager.spawn(owner.clone(), pty_spec).await.expect("spawn PTY");
    manager
        .resize(&owner, &info.id, ProcessTerminalSize { rows: 40, cols: 120 })
        .expect("resize PTY");
    manager
        .write(&owner, &info.id, b"hello".to_vec(), false)
        .await
        .expect("write PTY");
    manager
        .send_keys(&owner, &info.id, &[ProcessKey::Enter])
        .await
        .expect("send key");
    let mut cursor = 0;
    let mut output = Vec::new();
    for _ in 0..20 {
        let logs = manager
            .logs(&owner, &info.id, cursor, None, true, Some(Duration::from_millis(100)))
            .await
            .expect("PTY logs");
        cursor = logs.cursor;
        output.extend(logs.chunks.iter().flat_map(pi_coding::ProcessLogChunk::bytes));
        if output.windows(b"<hello>".len()).any(|window| window == b"<hello>") {
            break;
        }
    }
    assert!(output.windows(b"<hello>".len()).any(|window| window == b"<hello>"));
    manager.signal(&owner, &info.id, ProcessSignal::Sigint).expect("interrupt PTY");
    let exited = manager.wait(&owner, &info.id, Some(Duration::from_secs(3))).await.expect("wait PTY");
    assert!(exited.state.is_terminal());
}

#[tokio::test]
async fn concurrent_spawns_cannot_exceed_cap() {
    let directory = tempfile::tempdir().expect("tempdir");
    let manager = ProcessManager::with_config(ProcessManagerConfig {
        max_processes: 1,
        idle_timeout: None,
        ..ProcessManagerConfig::default()
    });
    let owner = ProcessOwnerId::new("cap-owner");
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(3));
    let mut attempts = Vec::new();
    for _ in 0..2 {
        let manager = manager.clone();
        let owner = owner.clone();
        let barrier = barrier.clone();
        let cwd = directory.path().to_path_buf();
        attempts.push(tokio::spawn(async move {
            barrier.wait().await;
            manager.spawn(owner, spec(&cwd, "sleep 30")).await
        }));
    }
    barrier.wait().await;
    let first = attempts.remove(0).await.expect("first join");
    let second = attempts.remove(0).await.expect("second join");
    assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
    let running = first.ok().or_else(|| second.ok()).expect("one process");
    manager.signal(&owner, &running.id, ProcessSignal::Sigkill).expect("cleanup");
    manager.wait(&owner, &running.id, Some(Duration::from_secs(3))).await.expect("wait cleanup");
    let replacement = manager
        .spawn(owner.clone(), spec(directory.path(), "exit 0"))
        .await
        .expect("cap slot released");
    manager
        .wait(&owner, &replacement.id, Some(Duration::from_secs(3)))
        .await
        .expect("replacement exit");
    let mut invalid = spec(directory.path(), "exit 0");
    invalid.argv = vec!["/definitely/missing/pi-process-test".to_owned()];
    assert!(manager.spawn(owner.clone(), invalid).await.is_err());
    let after_failure = manager
        .spawn(owner.clone(), spec(directory.path(), "exit 0"))
        .await
        .expect("failed spawn reservation released");
    manager
        .wait(&owner, &after_failure.id, Some(Duration::from_secs(3)))
        .await
        .expect("post-failure replacement exit");
}

#[tokio::test]
async fn signal_timeout_and_cap_refusal() {
    let directory = tempfile::tempdir().expect("tempdir");
    let manager = ProcessManager::with_config(ProcessManagerConfig {
        max_processes: 1,
        idle_timeout: None,
        ..ProcessManagerConfig::default()
    });
    let owner = ProcessOwnerId::new("owner");
    let first = manager
        .spawn(owner.clone(), spec(directory.path(), "sleep 30"))
        .await
        .expect("spawn first");
    assert!(
        manager
            .spawn(owner.clone(), spec(directory.path(), "sleep 30"))
            .await
            .is_err()
    );
    manager
        .signal(&owner, &first.id, ProcessSignal::Sigint)
        .expect("signal");
    let signalled = manager
        .wait(&owner, &first.id, Some(Duration::from_secs(3)))
        .await
        .expect("wait signal");
    assert_eq!(signalled.exit_code, Some(130));

    let mut timed_spec = spec(directory.path(), "sleep 30");
    timed_spec.timeout_ms = Some(50);
    let timed = manager
        .spawn(owner.clone(), timed_spec)
        .await
        .expect("spawn timed");
    let timed = manager
        .wait(&owner, &timed.id, Some(Duration::from_secs(3)))
        .await
        .expect("wait timed");
    assert_eq!(timed.state, ProcessState::TimedOut);
    assert_eq!(timed.exit_code, Some(124));
}

#[tokio::test]
async fn idle_expiry_and_process_group_cleanup() {
    let directory = tempfile::tempdir().expect("tempdir");
    let pid_file = directory.path().join("grandchild.pid");
    let manager = ProcessManager::with_config(ProcessManagerConfig {
        idle_timeout: Some(Duration::from_millis(50)),
        idle_scan_interval: Duration::from_millis(10),
        terminate_grace: Duration::from_millis(20),
        ..ProcessManagerConfig::default()
    });
    let owner = ProcessOwnerId::new("owner");
    let script = format!("sleep 30 & echo $! > '{}'; wait", pid_file.display());
    let info = manager
        .spawn(owner.clone(), spec(directory.path(), &script))
        .await
        .expect("spawn");
    let result = manager
        .wait(&owner, &info.id, Some(Duration::from_secs(3)))
        .await
        .expect("wait expiry");
    assert_eq!(result.state, ProcessState::Expired);
    let grandchild: i32 = std::fs::read_to_string(&pid_file)
        .expect("grandchild pid")
        .trim()
        .parse()
        .expect("pid number");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(nix::sys::signal::kill(nix::unistd::Pid::from_raw(grandchild), None).is_err());
}

#[tokio::test]
async fn dropping_last_manager_kills_process_group() {
    let directory = tempfile::tempdir().expect("tempdir");
    let pid_file = directory.path().join("root.pid");
    let pid = {
        let manager = ProcessManager::with_config(ProcessManagerConfig {
            idle_timeout: None,
            ..ProcessManagerConfig::default()
        });
        let owner = ProcessOwnerId::new("owner");
        let script = format!("echo $$ > '{}'; exec sleep 30", pid_file.display());
        manager
            .spawn(owner, spec(directory.path(), &script))
            .await
            .expect("spawn");
        loop {
            if let Ok(text) = std::fs::read_to_string(&pid_file) {
                break text.trim().parse::<i32>().expect("pid");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    for _ in 0..100 {
        if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("process survived ProcessManager drop");
}

#[test]
fn manager_can_be_constructed_without_tokio_runtime() {
    let manager = ProcessManager::new();
    assert!(manager.list(&ProcessOwnerId::new("sync-owner")).is_empty());
}

#[test]
fn debug_redacts_environment_values() {
    let secret = "super-secret-process-token";
    let mut env = BTreeMap::new();
    env.insert("TOKEN".to_owned(), Some(secret.to_owned()));
    let spec = ProcessSpawnSpec {
        argv: vec!["echo".to_owned()],
        cwd: std::env::current_dir().expect("cwd"),
        env,
        tty: false,
        terminal_size: None,
        label: None,
        timeout_ms: None,
        output_bytes: None,
    };
    assert!(!format!("{spec:?}").contains(secret));
}
