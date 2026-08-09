use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use tempfile::TempDir;

fn rpi_bin() -> String {
    env!("CARGO_BIN_EXE_rpi").to_owned()
}

fn run(agent_dir: &Path, cwd: &Path, args: &[&str]) -> (bool, String, String) {
    let output = Command::new(rpi_bin())
        .args(args)
        .env("PI_CODING_AGENT_DIR", agent_dir)
        .env("HOME", agent_dir)
        .env("USERPROFILE", agent_dir)
        .env("PI_SKIP_VERSION_CHECK", "1")
        .env_remove("PI_OFFLINE")
        .env_remove("PI_PROFILE")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("DEEPSEEK_API_KEY")
        .env_remove("GEMINI_API_KEY")
        .env_remove("AWS_ACCESS_KEY_ID")
        .env_remove("AWS_SECRET_ACCESS_KEY")
        .env_remove("GOOGLE_APPLICATION_CREDENTIALS")
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .expect("run rpi package command");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn list_previews_local_resource_changes_without_applying_them() {
    let sandbox = TempDir::new().unwrap();
    let agent_dir = sandbox.path().join("agent");
    let cwd = sandbox.path().join("workspace");
    let package = sandbox.path().join("local-package");
    fs::create_dir_all(&agent_dir).unwrap();
    fs::create_dir_all(&cwd).unwrap();
    fs::create_dir_all(package.join("skills")).unwrap();
    fs::write(package.join("skills/review.md"), "initial\n").unwrap();

    let source = package.to_str().unwrap();
    let (installed, _, install_error) = run(&agent_dir, &cwd, &["install", source]);
    assert!(installed, "install succeeds: {install_error}");
    let state = agent_dir.join("package-state.json");
    let settings = agent_dir.join("settings.json");
    let state_before = fs::read(&state).unwrap();
    let settings_before = fs::read(&settings).unwrap();

    let (listed, initial, initial_error) = run(&agent_dir, &cwd, &["list"]);
    assert!(listed, "list succeeds: {initial_error}");
    assert!(initial.contains("installed"));
    assert!(!initial.contains("update-available"));

    fs::write(package.join("skills/added.md"), "new\n").unwrap();
    let (previewed, output, preview_error) = run(&agent_dir, &cwd, &["list"]);
    assert!(previewed, "preview list succeeds: {preview_error}");
    assert!(output.contains("installed update-available"), "{output}");
    assert_eq!(fs::read(&state).unwrap(), state_before);
    assert_eq!(fs::read(&settings).unwrap(), settings_before);
}
