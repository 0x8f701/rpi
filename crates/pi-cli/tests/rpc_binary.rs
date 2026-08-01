use std::io::Write;
use std::process::{Command, Stdio};

fn rpc_bin() -> String {
    env!("CARGO_BIN_EXE_pi-rpc").to_owned()
}

#[test]
fn help_and_version_use_standalone_name() {
    let help = Command::new(rpc_bin())
        .arg("--help")
        .output()
        .expect("pi-rpc --help");
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("pi-rpc"));

    let version = Command::new(rpc_bin())
        .arg("--version")
        .output()
        .expect("pi-rpc --version");
    assert!(version.status.success());
    assert!(String::from_utf8_lossy(&version.stdout).contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn malformed_then_valid_frames_keep_stdout_jsonl() {
    let home = tempfile::tempdir().expect("home");
    let mut child = Command::new(rpc_bin())
        .args(["--offline", "--model", "faux/faux-1"])
        .env("HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn pi-rpc");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"{bad json}\n{\"type\":\"get_state\",\"id\":\"state-1\"}\n")
        .unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("wait pi-rpc");
    assert!(
        output.status.success(),
        "status: {:?}",
        output.status.code()
    );
    let lines = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("stdout line is JSON"))
        .collect::<Vec<_>>();
    assert!(
        lines
            .iter()
            .any(|line| line["command"] == "parse" && line["success"] == false)
    );
    assert!(lines.iter().any(|line| line["id"] == "state-1"
        && line["command"] == "get_state"
        && line["success"] == true));
}
