use std::fs;
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

use pi_agent::ThinkingLevel;
use pi_ai::providers::{FauxProviderOptions, register_faux_provider};
use pi_ai::{Model, SimpleStreamOptions};
use pi_coding::packages::{PackageManager, PackageScope, PackageUpdateType};
use pi_coding::{
    Application, ResourceDiscovery, ResourceManager, ResourceManagerOptions, Session, SessionOptions,
};
use tempfile::TempDir;

fn run_git(cwd: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args([
            "-c",
            "user.name=Pi Test",
            "-c",
            "user.email=pi@example.test",
        ])
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", cwd)
        .output()
        .expect("start git fixture command");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn commit_package(repo: &Path, body: &str) -> String {
    fs::create_dir_all(repo.join("skills")).unwrap();
    fs::write(repo.join("skills/review.md"), body).unwrap();
    run_git(repo, &["add", "."]);
    run_git(repo, &["commit", "-m", body]);
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo)
        .output()
        .unwrap();
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

struct GovernedPackages {
    manager: PackageManager,
    _application: Application,
    _registration: pi_ai::providers::FauxProviderRegistration,
    _sessions: TempDir,
}

/// Git install and update preview spawn `PackageHelper` through a published
/// generation. `PackageManager::with_agent_dir` leaves that context empty.
fn governed_manager(cwd: &Path, agent_dir: &Path) -> GovernedPackages {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("package test runtime");
    runtime.block_on(async {
        let api = "package-update-api".to_owned();
        let mut model = Model::default();
        model.id = "package-update".into();
        model.name = "Package Update".into();
        model.api = api.clone();
        model.provider = "package-update-provider".into();
        model.base_url = "http://127.0.0.1:9".into();
        let registration = register_faux_provider(FauxProviderOptions {
            api,
            provider: model.provider.clone(),
            models: vec![model.clone()],
            chunk_size: 16,
        });
        std::fs::create_dir_all(agent_dir).expect("agent dir");
        std::fs::write(
            agent_dir.join("settings.json"),
            r#"{"sandbox":{"mode":"danger-full-access"}}"#,
        )
        .expect("sandbox settings");
        let session = Session::new_with_additional_tools_filtered_and_discovery(
            SessionOptions {
                model,
                cwd: cwd.to_path_buf(),
                system_prompt: String::new(),
                thinking_level: ThinkingLevel::Off,
                api_key: "faux".into(),
                compaction: None,
                stream_options: SimpleStreamOptions::default(),
                tools: None,
                before_tool_call: None,
                after_tool_call: None,
                stream_fn: None,
                auth_resolver: None,
                auth_manager: None,
            },
            Vec::new(),
            Default::default(),
            ResourceDiscovery::Disabled,
        )
        .expect("package session");
        let sessions = TempDir::new().expect("package sessions");
        session.set_session_dir(sessions.path().to_path_buf());
        let mut options = ResourceManagerOptions::new(cwd);
        options.agent_dir = agent_dir.to_path_buf();
        options.disable_extensions = true;
        options.disable_skills = true;
        options.disable_prompt_templates = true;
        options.disable_themes = true;
        options.disable_context_files = true;
        options.project_trust_override = Some(true);
        session
            .attach_resources(ResourceManager::new(options).expect("package resources"))
            .await
            .expect("attach package resources");
        let application = Application::new(session).await;
        let manager = PackageManager::with_agent_dir(cwd, agent_dir, true)
            .expect("package manager")
            .with_process_context(application.governed_process_context());
        GovernedPackages {
            manager,
            _application: application,
            _registration: registration,
            _sessions: sessions,
        }
    })
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
    manager
        .install(package.to_str().unwrap(), PackageScope::Global)
        .unwrap();
    let state_path = manager.state_path(PackageScope::Global);
    let settings_path = manager.settings_path(PackageScope::Global);
    let before_state = fs::read(&state_path).unwrap();
    let before_settings = fs::read(&settings_path).unwrap();
    assert!(manager.check_available_updates().unwrap().is_empty());
    fs::create_dir_all(package.join("skills/added")).unwrap();
    fs::write(package.join("skills/added/SKILL.md"), "new\n").unwrap();
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
    let package = cwd.join(".rpi/packages/local");
    fs::create_dir_all(&agent_dir).unwrap();
    local_package(&package);
    let trusted = manager(&cwd, &agent_dir, true);
    trusted
        .install(".rpi/packages/local", PackageScope::Project)
        .unwrap();
    assert_eq!(
        trusted
            .installed_path("packages/local", PackageScope::Project)
            .unwrap(),
        Some(fs::canonicalize(&package).unwrap())
    );
    assert_eq!(
        trusted
            .installed_path("packages/missing", PackageScope::Project)
            .unwrap(),
        None
    );
    let untrusted = manager(&cwd, &agent_dir, false);
    assert!(
        untrusted
            .installed_path("packages/local", PackageScope::Project)
            .unwrap_err()
            .to_string()
            .contains("project is not trusted")
    );
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
    run_git(
        &author,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    run_git(&author, &["push", "-u", "origin", "main"]);
    run_git(&remote, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    let (_daemon, git_base) = GitDaemon::start(sandbox.path());
    let source = format!("{git_base}/owner/remote.git@main");
    let governed = governed_manager(&cwd, &agent_dir);
    let manager = &governed.manager;
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
    let installed = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&checkout)
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(installed.stdout).unwrap().trim(),
        first_commit
    );
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
    let error = manager
        .installed_path("git:https://example.test/owner/repo", PackageScope::Global)
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("filesystem entry type is unsupported"),
        "{error}"
    );
}

#[test]
fn local_package_can_be_updated_and_removed_by_absolute_path() {
    let sandbox = TempDir::new().unwrap();
    let cwd = sandbox.path().join("workspace");
    let agent_dir = sandbox.path().join("agent");
    let package = sandbox.path().join("local-package");
    fs::create_dir_all(&cwd).unwrap();
    fs::create_dir_all(&agent_dir).unwrap();
    local_package(&package);
    let source = package.to_str().unwrap();
    let manager = manager(&cwd, &agent_dir, true);

    manager.install(source, PackageScope::Global).unwrap();
    fs::write(package.join("skills/added.md"), "new\n").unwrap();
    let updated = manager.update_one(source).unwrap();
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].source, source);

    assert!(manager.remove(source, PackageScope::Global).unwrap());
    assert!(manager.list().unwrap().is_empty());
}

#[test]
fn npm_sources_fail_closed_without_state_mutation() {
    let sandbox = TempDir::new().unwrap();
    let cwd = sandbox.path().join("workspace");
    let agent_dir = sandbox.path().join("agent");
    fs::create_dir_all(&cwd).unwrap();
    fs::create_dir_all(&agent_dir).unwrap();
    let settings_path = agent_dir.join("settings.json");
    fs::write(&settings_path, r#"{"packages":["npm:future-pkg"]}"#).unwrap();
    let before = fs::read(&settings_path).unwrap();
    let manager = manager(&cwd, &agent_dir, true);

    let install_error = manager
        .install("npm:future-pkg", PackageScope::Global)
        .unwrap_err();
    assert!(
        install_error
            .to_string()
            .contains("npm package sources are not supported yet"),
        "{install_error:#}"
    );

    let path_error = manager
        .installed_path("npm:future-pkg", PackageScope::Global)
        .unwrap_err();
    assert!(
        path_error
            .to_string()
            .contains("npm package sources are not supported yet"),
        "{path_error:#}"
    );

    let update_error = manager.update_all().unwrap_err();
    assert!(
        update_error
            .to_string()
            .contains("npm package sources are not supported yet"),
        "{update_error:#}"
    );

    assert_eq!(fs::read(&settings_path).unwrap(), before);
    assert!(!agent_dir.join("package-state.json").exists());

    let listed = manager.list().unwrap();
    assert_eq!(listed.len(), 1);
    assert!(!listed[0].supported);
    assert_eq!(listed[0].source, "npm:future-pkg");
}
