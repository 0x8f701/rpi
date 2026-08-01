use std::fs;
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

use pi_coding::packages::{PackageManager, PackageScope, PackageUpdateType};
use tempfile::TempDir;

fn run_git(cwd: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(["-c", "user.name=Pi Test", "-c", "user.email=pi@example.test"])
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", cwd)
        .output()
        .expect("start git fixture command");
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
}

fn commit_package(repo: &Path, body: &str) -> String {
    fs::create_dir_all(repo.join("skills")).unwrap();
    fs::write(repo.join("skills/review.md"), body).unwrap();
    run_git(repo, &["add", "."]);
    run_git(repo, &["commit", "-m", body]);
    let output = Command::new("git").args(["rev-parse", "HEAD"]).current_dir(repo).output().unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn local_package(root: &Path) {
    fs::create_dir_all(root.join("skills")).unwrap();
    fs::write(root.join("skills/review.md"), "initial\n").unwrap();
}

fn manager(cwd: &Path, agent_dir: &Path, trusted: bool) -> PackageManager {
    PackageManager::with_agent_dir(cwd, agent_dir, trusted).unwrap()
}

struct GitDaemon(Child);

impl GitDaemon {
    fn start(base: &Path) -> (Self, String) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let child = Command::new("git")
            .args([
                "daemon",
                "--reuseaddr",
                "--export-all",
                "--listen=127.0.0.1",
                &format!("--port={port}"),
                &format!("--base-path={}", base.display()),
                base.to_str().unwrap(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        for _ in 0..100 {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return (Self(child), format!("git://127.0.0.1:{port}"));
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("git daemon did not become ready");
    }
}

impl Drop for GitDaemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn local_preview_detects_resource_change_without_mutation() {
    let sandbox = TempDir::new().unwrap();
    let cwd = sandbox.path().join("workspace");
    let agent_dir = sandbox.path().join("agent");
    let package = sandbox.path().join("local-package");
    fs::create_dir_all(&cwd).unwrap();
    fs::create_dir_all(&agent_dir).unwrap();
    local_package(&package);
    let manager = manager(&cwd, &agent_dir, true);
    manager.install(package.to_str().unwrap(), PackageScope::Global).unwrap();
    let state_path = manager.state_path(PackageScope::Global);
    let settings_path = manager.settings_path(PackageScope::Global);
    let before_state = fs::read(&state_path).unwrap();
    let before_settings = fs::read(&settings_path).unwrap();
    assert!(manager.check_available_updates().unwrap().is_empty());
    fs::write(package.join("skills/added.md"), "new\n").unwrap();
    let first = manager.check_available_updates().unwrap();
    assert_eq!(first, manager.check_available_updates().unwrap());
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].scope, PackageScope::Global);
    assert_eq!(first[0].update_type, PackageUpdateType::LocalChanged);
    assert_eq!(first[0].display_name, "local-package");
    assert_eq!(fs::read(&state_path).unwrap(), before_state);
    assert_eq!(fs::read(&settings_path).unwrap(), before_settings);
    assert!(!agent_dir.join(".package-operation.lock").exists());
}

#[test]
fn installed_path_is_scope_relative_and_project_trust_gated() {
    let sandbox = TempDir::new().unwrap();
    let cwd = sandbox.path().join("workspace");
    let agent_dir = sandbox.path().join("agent");
    let package = cwd.join(".pi/packages/local");
    fs::create_dir_all(&agent_dir).unwrap();
    local_package(&package);
    let trusted = manager(&cwd, &agent_dir, true);
    trusted.install(".pi/packages/local", PackageScope::Project).unwrap();
    assert_eq!(trusted.installed_path("packages/local", PackageScope::Project).unwrap(), Some(fs::canonicalize(&package).unwrap()));
    assert_eq!(trusted.installed_path("packages/missing", PackageScope::Project).unwrap(), None);
    let untrusted = manager(&cwd, &agent_dir, false);
    assert!(untrusted.installed_path("packages/local", PackageScope::Project).unwrap_err().to_string().contains("project is not trusted"));
    assert!(untrusted.check_available_updates().unwrap().is_empty());
}

#[test]
fn git_preview_detects_remote_commit_without_mutation() {
    let sandbox = TempDir::new().unwrap();
    let cwd = sandbox.path().join("workspace");
    let agent_dir = sandbox.path().join("agent");
    let remote = sandbox.path().join("owner/remote.git");
    let author = sandbox.path().join("author");
    fs::create_dir_all(&cwd).unwrap();
    fs::create_dir_all(&agent_dir).unwrap();
    fs::create_dir_all(&author).unwrap();
    fs::create_dir_all(&remote).unwrap();
    run_git(&remote, &["init", "--bare"]);
    run_git(&author, &["init", "-b", "main"]);
    let first_commit = commit_package(&author, "first");
    run_git(&author, &["remote", "add", "origin", remote.to_str().unwrap()]);
    run_git(&author, &["push", "-u", "origin", "main"]);
    run_git(&remote, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    let (_daemon, git_base) = GitDaemon::start(sandbox.path());
    let source = format!("{git_base}/owner/remote.git@main");
    let manager = manager(&cwd, &agent_dir, true);
    let operation = manager.install(&source, PackageScope::Global).unwrap();
    assert_eq!(operation.revision.as_deref(), Some(first_commit.as_str()));
    let checkout = operation.root;
    let state_path = manager.state_path(PackageScope::Global);
    let settings_path = manager.settings_path(PackageScope::Global);
    let before_state = fs::read(&state_path).unwrap();
    let before_settings = fs::read(&settings_path).unwrap();
    assert!(manager.check_available_updates().unwrap().is_empty());
    let second_commit = commit_package(&author, "second");
    run_git(&author, &["push", "origin", "main"]);
    let updates = manager.check_available_updates().unwrap();
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].update_type, PackageUpdateType::Git);
    assert_eq!(updates[0].scope, PackageScope::Global);
    assert!(updates[0].display_name.ends_with("/remote"));
    assert_eq!(fs::read(&state_path).unwrap(), before_state);
    assert_eq!(fs::read(&settings_path).unwrap(), before_settings);
    let installed = Command::new("git").args(["rev-parse", "HEAD"]).current_dir(&checkout).output().unwrap();
    assert_eq!(String::from_utf8(installed.stdout).unwrap().trim(), first_commit);
    assert_ne!(first_commit, second_commit);
}

#[cfg(unix)]
#[test]
fn installed_git_symlink_escape_is_rejected() {
    use std::os::unix::fs::symlink;
    let sandbox = TempDir::new().unwrap();
    let cwd = sandbox.path().join("workspace");
    let agent_dir = sandbox.path().join("agent");
    let outside = sandbox.path().join("outside");
    fs::create_dir_all(&cwd).unwrap();
    fs::create_dir_all(agent_dir.join("git/example.test/owner")).unwrap();
    fs::create_dir_all(&outside).unwrap();
    symlink(&outside, agent_dir.join("git/example.test/owner/repo")).unwrap();
    let manager = manager(&cwd, &agent_dir, true);
    let error = manager.installed_path("git:https://example.test/owner/repo", PackageScope::Global).unwrap_err();
    assert!(error.to_string().contains("escapes its scope root"));
}
