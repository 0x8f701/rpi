use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use tempfile::TempDir;

use super::git::{branch_exists as git_branch_exists, is_dirty as git_is_dirty};
use super::*;

const TEST_TIMEOUT: Duration = Duration::from_secs(30);
fn repository_test_executor() -> crate::process::RepositoryTestExecutor {
    Arc::new(|authorized, stdout_bytes, stderr_bytes| {
        assert!(matches!(
            authorized.audience(),
            crate::sandbox::CredentialAudience::SpawnFamily {
                family: pi_sandbox::SpawnFamily::RepositoryHelper,
            }
        ));
        assert_eq!(authorized.stdio(), pi_sandbox::StdioMode::Piped);
        assert!(authorized.timeout_ms().is_some());
        let argv = authorized.argv();
        let program = PathBuf::from(argv.first().expect("git program"));
        let mut command = Command::new(program);
        command.args(&argv[1..]);
        command.current_dir(authorized.cwd().as_path());
        command.env_clear();
        command.envs(authorized.environment());
        let output = command
            .output()
            .expect("execute governed repository fixture");
        let stdout_capped = output.stdout.len() > stdout_bytes;
        let stderr_capped = output.stderr.len() > stderr_bytes;
        Ok(crate::process::GovernedProcessOutput {
            status: output.status.code(),
            stdout: output.stdout.into_iter().take(stdout_bytes).collect(),
            stderr: output.stderr.into_iter().take(stderr_bytes).collect(),
            timed_out: false,
            cancelled: false,
            stdout_capped,
            stderr_capped,
        })
    })
}
#[test]
fn repository_test_executor_rejects_foreign_generation_before_execution() {
    let first = Fixture::new("source-a", "managed-a");
    let second = Fixture::new("source-b", "managed-b");
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = executions.clone();
    let executor: crate::process::RepositoryTestExecutor = Arc::new(move |_, _, _| {
        observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        unreachable!("foreign authority must reject before test execution")
    });
    let manager =
        WorkflowWorktreeManager::new(first.source_lease.clone(), first.managed_lease.clone())
            .with_timeout(TEST_TIMEOUT)
            .with_process_context(
                second
                    .application
                    .governed_process_context()
                    .with_repository_test_executor(executor),
            );

    let error = manager.discover().expect_err("foreign generation rejected");
    assert!(
        error.to_string().contains("capability does not cover"),
        "unexpected rejection: {error}"
    );
    assert_eq!(executions.load(std::sync::atomic::Ordering::SeqCst), 0);
}

struct Fixture {
    sandbox: TempDir,
    repo: PathBuf,
    managed: PathBuf,
    _agent: TempDir,
    _sessions: TempDir,
    _faux: pi_ai::providers::FauxProviderRegistration,
    application: crate::Application,
    source_lease: crate::fs::CapabilityRootLease,
    managed_lease: crate::fs::CapabilityRootLease,
    catalog: PathBuf,
}

impl Fixture {
    fn new(source_name: &str, managed_name: &str) -> Self {
        let sandbox = TempDir::new().expect("temporary sandbox");
        let repo = sandbox.path().join(source_name);
        let managed = sandbox.path().join(managed_name);
        let catalog = managed
            .join(super::catalog::CATALOG_DIR_NAME)
            .join("worktrees.json");
        init_repo(&repo);
        let (agent, sessions, faux, application) = published_application(&repo);
        let capabilities = application
            .capture_generation()
            .sandbox()
            .capabilities()
            .clone();
        let source_lease = capabilities
            .open_root(
                crate::fs::CapabilityRootPurpose::ProjectResources,
                pi_sandbox::AbsolutePath::parse(repo.clone()).expect("absolute source root"),
                crate::fs::CapabilityAccess::ReadWrite,
                crate::fs::RootCreation::Existing,
            )
            .expect("source lease");
        let managed_lease = capabilities
            .open_root(
                crate::fs::CapabilityRootPurpose::WorkflowWorktrees,
                pi_sandbox::AbsolutePath::parse(managed.clone()).expect("absolute managed root"),
                crate::fs::CapabilityAccess::PrivateReadWrite,
                crate::fs::RootCreation::CreateDirectory,
            )
            .expect("managed lease");
        Self {
            sandbox,
            repo,
            managed,
            _agent: agent,
            _sessions: sessions,
            _faux: faux,
            application,
            source_lease,
            managed_lease,
            catalog,
        }
    }

    fn manager(&self) -> WorkflowWorktreeManager {
        WorkflowWorktreeManager::new(self.source_lease.clone(), self.managed_lease.clone())
            .with_timeout(TEST_TIMEOUT)
            .with_process_context(
                self.application
                    .governed_process_context()
                    .with_repository_test_executor(repository_test_executor()),
            )
    }

    fn create_options(&self) -> CreateWorktreeOptions {
        CreateWorktreeOptions {
            managed_root: self.managed.clone(),
            base_commit: None,
            timeout: Some(TEST_TIMEOUT),
        }
    }
}

fn published_application(
    cwd: &Path,
) -> (
    TempDir,
    TempDir,
    pi_ai::providers::FauxProviderRegistration,
    crate::Application,
) {
    let agent = TempDir::new().expect("agent directory");
    let sessions = TempDir::new().expect("session directory");
    fs::write(
        agent.path().join("settings.json"),
        r#"{"sandbox":{"mode":"danger-full-access"}}"#,
    )
    .expect("sandbox settings");
    let mut resources = crate::ResourceManagerOptions::new(cwd);
    resources.agent_dir = agent.path().to_path_buf();
    resources.project_trust_override = Some(true);
    resources.disable_extensions = true;
    resources.disable_skills = true;
    resources.disable_prompt_templates = true;
    resources.disable_themes = true;
    resources.disable_context_files = true;
    let resources = crate::ResourceManager::new(resources).expect("isolated resources");
    let faux = pi_ai::providers::register_faux_provider(Default::default());
    let workspace =
        crate::WorkspaceRoots::new(cwd, Vec::<PathBuf>::new()).expect("isolated workspace");
    let session = crate::Session::new_with_additional_tools_filtered_discovery_workspace_and_uri(
        crate::SessionOptions {
            model: faux.model(None).expect("worktree fixture model"),
            cwd: cwd.to_path_buf(),
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
        },
        Vec::new(),
        crate::ToolSelection::default(),
        crate::ResourceDiscovery::Disabled,
        workspace,
        None,
    )
    .expect("test session");
    session.set_session_dir(sessions.path().to_path_buf());
    let application = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("application runtime")
            .block_on(async {
                session
                    .attach_resources(resources)
                    .await
                    .expect("attach isolated resources");
                crate::Application::from_runtime_candidate(crate::ApplicationRuntimeCandidate::new(
                    session,
                ))
                .await
                .expect("publish isolated application")
            })
    })
    .join()
    .expect("application fixture thread");
    (agent, sessions, faux, application)
}

fn branch_exists(manager: &WorkflowWorktreeManager, cwd: &Path, branch: &str) -> bool {
    git_branch_exists(
        manager.process_context().expect("governed process context"),
        cwd,
        branch,
        TEST_TIMEOUT,
    )
    .expect("branch query")
}

fn is_dirty(manager: &WorkflowWorktreeManager, cwd: &Path) -> bool {
    git_is_dirty(
        manager.process_context().expect("governed process context"),
        cwd,
        TEST_TIMEOUT,
    )
    .expect("status query")
}

fn fixture_git(cwd: &Path, args: &[&str]) -> String {
    let null_device = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let hooks_path = format!("core.hooksPath={null_device}");
    let output = Command::new("git")
        .args([
            "-c",
            "color.ui=false",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
            "-c",
            hooks_path.as_str(),
        ])
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", null_device)
        .env("LC_ALL", "C")
        .output()
        .expect("execute fixture git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("git output utf8")
        .trim()
        .to_owned()
}

fn init_repo(repo: &Path) {
    fs::create_dir_all(repo).expect("source directory");
    fixture_git(repo, &["init"]);
    fixture_git(repo, &["config", "user.name", "Pi Test"]);
    fixture_git(repo, &["config", "user.email", "pi@example.test"]);
    fs::write(repo.join("README.md"), "base\n").expect("base file");
    fixture_git(repo, &["add", "README.md"]);
    fixture_git(repo, &["commit", "-m", "initial"]);
}

fn commit_file(cwd: &Path, relative: &str, contents: &str, message: &str) -> String {
    let path = cwd.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("file parent");
    }
    fs::write(&path, contents).expect("write commit file");
    fixture_git(cwd, &["add", "--", relative]);
    fixture_git(cwd, &["commit", "-m", message]);
    fixture_git(cwd, &["rev-parse", "HEAD"])
}

#[test]
fn two_isolated_worktrees_support_concurrent_edits_and_space_argv() {
    let fixture = Fixture::new("source repo with spaces", "managed root with spaces");
    let manager = fixture.manager();
    let first = manager
        .create("workflow-a", fixture.create_options())
        .expect("first worktree");
    let second = manager
        .create("workflow-b", fixture.create_options())
        .expect("second worktree");

    assert_ne!(first.worktree_path, second.worktree_path);
    assert!(!first.worktree_path.starts_with(&fixture.repo));
    assert!(!second.worktree_path.starts_with(&fixture.repo));
    assert!(
        first
            .worktree_path
            .to_string_lossy()
            .contains("managed root with spaces")
    );

    let barrier = Arc::new(Barrier::new(2));
    let first_path = first.worktree_path.clone();
    let first_barrier = Arc::clone(&barrier);
    let first_thread = thread::spawn(move || {
        first_barrier.wait();
        fs::write(first_path.join("first edit.txt"), "first\n").expect("first edit");
    });
    let second_path = second.worktree_path.clone();
    let second_thread = thread::spawn(move || {
        barrier.wait();
        fs::write(second_path.join("second edit.txt"), "second\n").expect("second edit");
    });
    first_thread.join().expect("first edit thread");
    second_thread.join().expect("second edit thread");

    assert!(first.worktree_path.join("first edit.txt").exists());
    assert!(!first.worktree_path.join("second edit.txt").exists());
    assert!(second.worktree_path.join("second edit.txt").exists());
    assert!(!second.worktree_path.join("first edit.txt").exists());
    assert!(!fixture.repo.join("first edit.txt").exists());
    assert!(!fixture.repo.join("second edit.txt").exists());

    commit_file(
        &first.worktree_path,
        "file with spaces.txt",
        "argv safe\n",
        "commit file with spaces",
    );
    let status = manager.inspect("workflow-a").expect("inspect first");
    assert_eq!(status.ahead_commits, 1);
    assert!(
        status
            .changed_files
            .iter()
            .any(|path| path == "file with spaces.txt")
    );
}

#[test]
fn foreign_worktree_removal_is_rejected_without_touching_either_tree() {
    let fixture = Fixture::new("source", "managed");
    let manager = fixture.manager();
    let owned = manager
        .create("owned", fixture.create_options())
        .expect("owned worktree");

    let foreign_branch = "foreign/not-managed";
    let foreign_path = fixture.sandbox.path().join("foreign worktree with spaces");
    fixture_git(&fixture.repo, &["branch", foreign_branch]);
    fixture_git(
        &fixture.repo,
        &[
            "worktree",
            "add",
            "--",
            foreign_path.to_str().expect("foreign path utf8"),
            foreign_branch,
        ],
    );

    let forged = WorkflowWorktreeIdentity {
        workflow_id: owned.workflow_id.clone(),
        source_root: owned.source_root.clone(),
        common_git_dir: owned.common_git_dir.clone(),
        worktree_path: foreign_path.clone(),
        branch: foreign_branch.to_owned(),
        base_commit: owned.base_commit.clone(),
        head_commit: owned.head_commit.clone(),
        created_at_ms: owned.created_at_ms,
    };
    let error = manager
        .remove(&forged)
        .expect_err("foreign removal rejected");
    assert!(matches!(error, WorktreeError::OwnershipMismatch { .. }));
    assert!(foreign_path.exists());
    assert!(owned.worktree_path.exists());
    assert!(branch_exists(&manager, &fixture.repo, foreign_branch));
}

#[test]
fn clean_merge_integration_applies_committed_workflow_change() {
    let fixture = Fixture::new("source with spaces", "managed");
    let manager = fixture.manager();
    let identity = manager
        .create("feature", fixture.create_options())
        .expect("feature worktree");
    let workflow_head = commit_file(
        &identity.worktree_path,
        "feature.txt",
        "integrated\n",
        "workflow feature",
    );

    let status = manager.inspect("feature").expect("feature status");
    assert!(!status.dirty);
    assert_eq!(status.ahead_commits, 1);
    assert_eq!(status.identity.head_commit, workflow_head);

    let outcome = manager
        .integrate(
            "feature",
            IntegrateOptions {
                strategy: IntegrateStrategy::Merge,
                require_clean_base: true,
                timeout: Some(TEST_TIMEOUT),
            },
        )
        .expect("merge integration");
    let IntegrateOutcome::Applied {
        result_commit,
        merge_commit,
        ..
    } = outcome
    else {
        panic!("expected applied integration");
    };
    assert_eq!(merge_commit.as_deref(), Some(result_commit.as_str()));
    assert_eq!(
        fs::read_to_string(fixture.repo.join("feature.txt")).expect("integrated file"),
        "integrated\n"
    );
    assert!(identity.worktree_path.exists());
    assert!(branch_exists(&manager, &fixture.repo, &identity.branch));
}

#[test]
fn empty_workflow_branch_integration_is_missing_commits() {
    let fixture = Fixture::new("source", "managed");
    let manager = fixture.manager();
    let identity = manager
        .create("empty-branch", fixture.create_options())
        .expect("empty worktree");

    let error = manager
        .integrate("empty-branch", IntegrateOptions::default())
        .expect_err("empty branch must not apply");
    assert!(
        matches!(error, WorktreeError::MissingCommits),
        "empty branch must be MissingCommits, got {error:?}"
    );
    let message = error.to_string();
    assert!(
        message.contains("no commits"),
        "missing-commits message must name commits: {message}"
    );
    assert!(
        !message.contains(fixture.repo.to_string_lossy().as_ref()),
        "missing-commits must not leak the source path: {message}"
    );
    assert!(
        !message.contains(identity.worktree_path.to_string_lossy().as_ref()),
        "missing-commits must not leak the worktree path: {message}"
    );
    assert!(identity.worktree_path.exists());
    assert!(!fixture.repo.join("untracked.txt").exists());
}

#[test]
fn uncommitted_files_without_commits_are_missing_commits() {
    let fixture = Fixture::new("source", "managed");
    let manager = fixture.manager();
    let identity = manager
        .create("uncommitted-worktree", fixture.create_options())
        .expect("uncommitted worktree");
    fs::write(
        identity.worktree_path.join("untracked.txt"),
        "not committed\n",
    )
    .expect("untracked workflow file");

    let error = manager
        .integrate("uncommitted-worktree", IntegrateOptions::default())
        .expect_err("files without commits must fail closed");
    assert!(
        matches!(error, WorktreeError::MissingCommits),
        "uncommitted files with no commits must be MissingCommits, got {error:?}"
    );
    let message = error.to_string();
    assert!(
        message.contains("no commits"),
        "missing-commits message must name commits: {message}"
    );
    assert!(identity.worktree_path.join("untracked.txt").exists());
    assert!(!fixture.repo.join("untracked.txt").exists());
}

#[test]
fn dirty_worktree_with_commits_fails_without_ignoring_uncommitted_files() {
    let fixture = Fixture::new("source", "managed");
    let manager = fixture.manager();
    let identity = manager
        .create("dirty-worktree", fixture.create_options())
        .expect("dirty worktree");
    commit_file(
        &identity.worktree_path,
        "committed.txt",
        "committed\n",
        "workflow commit",
    );
    fs::write(
        identity.worktree_path.join("untracked.txt"),
        "not committed\n",
    )
    .expect("untracked workflow file");

    let error = manager
        .integrate("dirty-worktree", IntegrateOptions::default())
        .expect_err("dirty workflow with commits must fail closed");
    assert!(matches!(&error, WorktreeError::DirtyWorktree(id) if id == "dirty-worktree"));
    assert!(identity.worktree_path.join("untracked.txt").exists());
    assert!(!fixture.repo.join("committed.txt").exists());
}

#[test]
fn repeated_merge_integration_of_committed_work_is_idempotent() {
    let fixture = Fixture::new("source", "managed");
    let manager = fixture.manager();
    let identity = manager
        .create("repeat-merge", fixture.create_options())
        .expect("feature worktree");
    commit_file(
        &identity.worktree_path,
        "feature.txt",
        "integrated\n",
        "workflow feature",
    );

    let first = manager
        .integrate(
            "repeat-merge",
            IntegrateOptions {
                strategy: IntegrateStrategy::Merge,
                require_clean_base: true,
                timeout: Some(TEST_TIMEOUT),
            },
        )
        .expect("first merge");
    let IntegrateOutcome::Applied {
        result_commit: first_commit,
        ..
    } = first
    else {
        panic!("expected applied integration, got {first:?}");
    };
    assert_eq!(
        fs::read_to_string(fixture.repo.join("feature.txt")).expect("integrated file"),
        "integrated\n"
    );

    let second = manager
        .integrate(
            "repeat-merge",
            IntegrateOptions {
                strategy: IntegrateStrategy::Merge,
                require_clean_base: true,
                timeout: Some(TEST_TIMEOUT),
            },
        )
        .expect("repeated merge of committed work");
    let IntegrateOutcome::Applied {
        result_commit: second_commit,
        ..
    } = second
    else {
        panic!("retry of committed work must stay Applied, got {second:?}");
    };
    assert_eq!(
        second_commit, first_commit,
        "idempotent retry must not create a second merge commit"
    );
    assert_eq!(
        fs::read_to_string(fixture.repo.join("feature.txt")).expect("still integrated"),
        "integrated\n"
    );
}

#[test]
fn rebase_integration_retry_when_heads_match_stays_applied() {
    let fixture = Fixture::new("source", "managed");
    let manager = fixture.manager();
    let identity = manager
        .create("repeat-rebase", fixture.create_options())
        .expect("feature worktree");
    commit_file(
        &identity.worktree_path,
        "rebased.txt",
        "rebased\n",
        "workflow rebase",
    );

    let first = manager
        .integrate(
            "repeat-rebase",
            IntegrateOptions {
                strategy: IntegrateStrategy::Rebase,
                require_clean_base: true,
                timeout: Some(TEST_TIMEOUT),
            },
        )
        .expect("first rebase");
    let IntegrateOutcome::Applied {
        result_commit: first_commit,
        ..
    } = first
    else {
        panic!("expected applied rebase, got {first:?}");
    };
    assert_eq!(
        fs::read_to_string(fixture.repo.join("rebased.txt")).expect("rebased file"),
        "rebased\n"
    );

    let second = manager
        .integrate(
            "repeat-rebase",
            IntegrateOptions {
                strategy: IntegrateStrategy::Rebase,
                require_clean_base: true,
                timeout: Some(TEST_TIMEOUT),
            },
        )
        .expect("repeated rebase of committed work");
    let IntegrateOutcome::Applied {
        result_commit: second_commit,
        ..
    } = second
    else {
        panic!("retry when heads already match must stay Applied, got {second:?}");
    };
    assert_eq!(second_commit, first_commit);
}

#[test]
fn merge_retry_after_source_advances_preserves_original_result() {
    let fixture = Fixture::new("source", "managed");
    let manager = fixture.manager();
    let identity = manager
        .create("advance-merge", fixture.create_options())
        .expect("feature worktree");
    commit_file(
        &identity.worktree_path,
        "feature.txt",
        "integrated\n",
        "workflow feature",
    );
    let first = manager
        .integrate("advance-merge", IntegrateOptions::default())
        .expect("first merge");
    let IntegrateOutcome::Applied {
        result_commit: first_commit,
        ..
    } = first
    else {
        panic!("expected applied, got {first:?}");
    };
    commit_file(&fixture.repo, "later.txt", "later\n", "later source");
    let retry = manager
        .integrate("advance-merge", IntegrateOptions::default())
        .expect("retry after source advanced");
    let IntegrateOutcome::Applied {
        result_commit: retry_commit,
        ..
    } = retry
    else {
        panic!("retry must stay Applied, got {retry:?}");
    };
    assert_eq!(retry_commit, first_commit);
    assert_eq!(
        fs::read_to_string(fixture.repo.join("later.txt")).expect("later source preserved"),
        "later\n"
    );
}

#[test]
fn merge_retry_with_dirty_source_after_applied_is_noop() {
    let fixture = Fixture::new("source", "managed");
    let manager = fixture.manager();
    let identity = manager
        .create("dirty-source-retry", fixture.create_options())
        .expect("feature worktree");
    commit_file(
        &identity.worktree_path,
        "feature.txt",
        "integrated\n",
        "workflow feature",
    );
    let first = manager
        .integrate("dirty-source-retry", IntegrateOptions::default())
        .expect("first merge");
    let IntegrateOutcome::Applied {
        result_commit: first_commit,
        ..
    } = first
    else {
        panic!("expected applied, got {first:?}");
    };
    fs::write(fixture.repo.join("uncommitted.txt"), "dirty\n").expect("dirty source");
    let retry = manager
        .integrate("dirty-source-retry", IntegrateOptions::default())
        .expect("retry with dirty source after applied");
    let IntegrateOutcome::Applied {
        result_commit: retry_commit,
        ..
    } = retry
    else {
        panic!("retry must stay Applied, got {retry:?}");
    };
    assert_eq!(retry_commit, first_commit);
    assert_eq!(
        fs::read_to_string(fixture.repo.join("uncommitted.txt")).expect("dirty preserved"),
        "dirty\n"
    );
}

#[test]
fn merge_retry_after_source_advances_and_becomes_dirty_preserves_result() {
    let fixture = Fixture::new("source", "managed");
    let manager = fixture.manager();
    let identity = manager
        .create("advance-dirty-retry", fixture.create_options())
        .expect("feature worktree");
    commit_file(
        &identity.worktree_path,
        "feature.txt",
        "integrated\n",
        "workflow feature",
    );
    let first = manager
        .integrate("advance-dirty-retry", IntegrateOptions::default())
        .expect("first merge");
    let IntegrateOutcome::Applied {
        result_commit: first_commit,
        ..
    } = first
    else {
        panic!("expected applied, got {first:?}");
    };
    commit_file(&fixture.repo, "later.txt", "later\n", "later source");
    fs::write(fixture.repo.join("uncommitted.txt"), "dirty\n").expect("dirty source");
    let retry = manager
        .integrate("advance-dirty-retry", IntegrateOptions::default())
        .expect("retry after source advanced and became dirty");
    let IntegrateOutcome::Applied {
        result_commit: retry_commit,
        ..
    } = retry
    else {
        panic!("retry must stay Applied, got {retry:?}");
    };
    assert_eq!(retry_commit, first_commit);
    assert_eq!(
        fs::read_to_string(fixture.repo.join("later.txt")).expect("later source preserved"),
        "later\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.repo.join("uncommitted.txt")).expect("dirty preserved"),
        "dirty\n"
    );
}

#[test]
fn conflicting_merge_aborts_source_and_preserves_owned_resources() {
    let fixture = Fixture::new("source", "managed");
    let manager = fixture.manager();
    let identity = manager
        .create("conflict", fixture.create_options())
        .expect("conflict worktree");
    let workflow_head = commit_file(
        &identity.worktree_path,
        "README.md",
        "workflow side\n",
        "workflow conflict",
    );
    commit_file(
        &fixture.repo,
        "README.md",
        "source side\n",
        "source conflict",
    );

    let outcome = manager
        .integrate(
            "conflict",
            IntegrateOptions {
                strategy: IntegrateStrategy::Merge,
                require_clean_base: true,
                timeout: Some(TEST_TIMEOUT),
            },
        )
        .expect("typed conflict");
    let IntegrateOutcome::Conflicted {
        conflicts,
        workflow_id,
        branch,
        head_commit,
        ..
    } = outcome
    else {
        panic!("expected conflicted integration");
    };
    assert_eq!(conflicts, vec!["README.md"]);
    assert_eq!(workflow_id, identity.workflow_id);
    assert_eq!(branch, identity.branch);
    assert_eq!(head_commit, workflow_head);
    assert!(identity.worktree_path.exists());
    assert!(branch_exists(&manager, &fixture.repo, &identity.branch));
    assert!(!is_dirty(&manager, &fixture.repo));
    assert_eq!(
        fs::read_to_string(fixture.repo.join("README.md")).expect("source content"),
        "source side\n"
    );
    assert_eq!(
        fs::read_to_string(identity.worktree_path.join("README.md")).expect("workflow content"),
        "workflow side\n"
    );
}

#[test]
fn removal_requires_exact_catalog_identity_and_cleans_only_owned_resources() {
    let fixture = Fixture::new("source", "managed");
    let manager = fixture.manager();
    let keep = manager
        .create("keep", fixture.create_options())
        .expect("keep worktree");
    let remove = manager
        .create("remove", fixture.create_options())
        .expect("remove worktree");

    let foreign_branch = "foreign/keep";
    let foreign_path = fixture.sandbox.path().join("foreign");
    fixture_git(&fixture.repo, &["branch", foreign_branch]);
    fixture_git(
        &fixture.repo,
        &[
            "worktree",
            "add",
            "--",
            foreign_path.to_str().expect("foreign path utf8"),
            foreign_branch,
        ],
    );

    let catalog_backup = fs::read(&fixture.catalog).expect("catalog bytes");
    fs::remove_file(&fixture.catalog).expect("remove catalog");
    let missing_catalog_error = manager.remove(&remove).expect_err("catalog is required");
    assert!(matches!(
        &missing_catalog_error,
        WorktreeError::NotRegistered(id) if id == "remove"
    ));
    assert!(remove.worktree_path.exists());
    assert!(branch_exists(&manager, &fixture.repo, &remove.branch));
    fs::write(&fixture.catalog, catalog_backup).expect("restore catalog");

    manager.remove(&remove).expect("remove owned identity");
    assert!(!remove.worktree_path.exists());
    assert!(!branch_exists(&manager, &fixture.repo, &remove.branch));
    assert!(keep.worktree_path.exists());
    assert!(branch_exists(&manager, &fixture.repo, &keep.branch));
    assert!(foreign_path.exists());
    assert!(branch_exists(&manager, &fixture.repo, foreign_branch));

    assert_eq!(manager.list().expect("catalog entries"), vec![keep]);
}

#[test]
fn discovery_dirty_base_invalid_ids_and_managed_root_fail_closed() {
    let sandbox = TempDir::new().expect("temporary sandbox");
    let plain = sandbox.path().join("plain");
    fs::create_dir(&plain).expect("plain directory");
    let fixture = Fixture::new("source", "managed");
    let capabilities = fixture
        .application
        .capture_generation()
        .sandbox()
        .capabilities()
        .clone();
    let plain_lease = capabilities
        .open_root(
            crate::fs::CapabilityRootPurpose::ProjectResources,
            pi_sandbox::AbsolutePath::parse(plain.clone()).expect("absolute plain root"),
            crate::fs::CapabilityAccess::ReadWrite,
            crate::fs::RootCreation::Existing,
        )
        .expect("plain lease");
    assert!(matches!(
        WorkflowWorktreeManager::new(plain_lease, fixture.managed_lease.clone())
            .with_process_context(
                fixture
                    .application
                    .governed_process_context()
                    .with_repository_test_executor(repository_test_executor()),
            )
            .discover(),
        Err(WorktreeError::NotGit)
    ));

    let manager = fixture.manager();
    for invalid in [
        "",
        "../escape",
        "slash/id",
        ".hidden",
        "bad.lock",
        "nul\0id",
    ] {
        assert!(matches!(
            manager.create(invalid, fixture.create_options()),
            Err(WorktreeError::InvalidWorkflowId(_))
        ));
    }
    let inside = CreateWorktreeOptions {
        managed_root: fixture.repo.join("inside"),
        base_commit: None,
        timeout: Some(TEST_TIMEOUT),
    };
    assert!(manager.create("inside", inside).is_err());

    let identity = manager
        .create("dirty", fixture.create_options())
        .expect("dirty worktree");
    commit_file(
        &identity.worktree_path,
        "dirty.txt",
        "workflow\n",
        "workflow change",
    );
    fs::write(fixture.repo.join("untracked.txt"), "dirty source\n").expect("dirty source");
    assert!(matches!(
        manager.integrate("dirty", IntegrateOptions::default()),
        Err(WorktreeError::DirtyBase)
    ));
}

#[test]
fn public_debug_and_serialized_conflict_do_not_expose_internal_paths() {
    let fixture = Fixture::new("source secret path", "managed secret path");
    let manager = fixture.manager();
    let identity = manager
        .create("redaction", fixture.create_options())
        .expect("redaction worktree");
    let debug = format!("{identity:?}");
    assert!(!debug.contains(fixture.repo.to_string_lossy().as_ref()));
    assert!(!debug.contains(fixture.managed.to_string_lossy().as_ref()));

    let outcome = IntegrateOutcome::Conflicted {
        strategy: IntegrateStrategy::Merge,
        conflicts: vec!["README.md".to_owned()],
        workflow_id: identity.workflow_id.clone(),
        branch: identity.branch.clone(),
        head_commit: identity.head_commit.clone(),
    };
    let serialized = serde_json::to_string(&outcome).expect("serialize conflict");
    assert!(!serialized.contains(fixture.repo.to_string_lossy().as_ref()));
    assert!(!serialized.contains(fixture.managed.to_string_lossy().as_ref()));
}

#[test]
fn branch_collision_allocates_distinct_owned_branch() {
    let fixture = Fixture::new("source", "managed");
    let manager = fixture.manager();
    fixture_git(&fixture.repo, &["branch", "rpi/workflow/collision"]);
    let identity = manager
        .create("collision", fixture.create_options())
        .expect("collision-safe allocation");
    assert_ne!(identity.branch, "rpi/workflow/collision");
    assert!(identity.branch.starts_with("rpi/workflow/collision-"));
}

#[test]
fn stale_owned_metadata_is_pruned_only_after_resources_are_gone() {
    let fixture = Fixture::new("source", "managed");
    let manager = fixture.manager();
    let identity = manager
        .create("stale", fixture.create_options())
        .expect("stale worktree");
    let worktree = identity.worktree_path.to_string_lossy().into_owned();
    fixture_git(
        &fixture.repo,
        &["worktree", "remove", "--force", "--", &worktree],
    );
    fixture_git(&fixture.repo, &["branch", "-D", "--", &identity.branch]);

    assert_eq!(
        manager.prune_stale().expect("prune stale entry"),
        vec!["stale"]
    );
    assert!(manager.list().expect("catalog after prune").is_empty());
}

#[test]
fn zero_timeout_is_typed_and_does_not_leak_absolute_paths() {
    let fixture = Fixture::new("source secret timeout", "managed secret timeout");
    let manager = fixture.manager().with_timeout(Duration::ZERO);
    let error = manager.discover().expect_err("zero timeout");
    assert!(matches!(error, WorktreeError::Timeout { .. }));
    let rendered = error.to_string();
    assert!(!rendered.contains(fixture.repo.to_string_lossy().as_ref()));
    assert!(!rendered.contains(fixture.managed.to_string_lossy().as_ref()));
}

#[test]
fn trusted_capability_requires_exact_live_owned_identity() {
    let fixture = Fixture::new("source", "managed");
    let manager = fixture.manager();
    let identity = manager
        .create("trusted", fixture.create_options())
        .expect("trusted worktree");
    let capability = manager.verify_owned(&identity).expect("trusted capability");
    assert_eq!(capability.path(), identity.worktree_path);
    assert_eq!(capability.workflow_id(), "trusted");
    let debug = format!("{capability:?}");
    assert!(!debug.contains(identity.worktree_path.to_string_lossy().as_ref()));

    commit_file(
        &identity.worktree_path,
        "advance.txt",
        "advanced\n",
        "advance head",
    );
    assert!(matches!(
        manager.verify_owned(&identity),
        Err(WorktreeError::OwnershipMismatch { .. })
    ));
}
#[test]
fn committed_workflow_restart_adopts_descendant_head_and_preserves_exact_identity() {
    let fixture = Fixture::new("source", "managed");
    let creator = fixture.manager();
    let original = creator
        .create("restart", fixture.create_options())
        .expect("restart worktree");
    let advanced_head = commit_file(
        &original.worktree_path,
        "restart.txt",
        "committed before restart\n",
        "advance before restart",
    );

    let restarted = fixture.manager();
    let (refreshed, capability) = restarted
        .verify_owned_current("restart")
        .expect("adopt descendant head after restart");
    assert_eq!(refreshed.head_commit, advanced_head);
    assert_eq!(capability.path(), original.worktree_path);
    assert_eq!(
        restarted.list().expect("refreshed catalog"),
        vec![refreshed.clone()]
    );
    assert!(restarted.verify_owned(&refreshed).is_ok());
    assert!(matches!(
        restarted.verify_owned(&original),
        Err(WorktreeError::OwnershipMismatch { .. })
    ));
}

#[test]
fn current_ownership_rejects_rewind_and_preserves_recorded_head() {
    let fixture = Fixture::new("source", "managed");
    let manager = fixture.manager();
    let original = manager
        .create("rewind", fixture.create_options())
        .expect("rewind worktree");
    let advanced_head = commit_file(
        &original.worktree_path,
        "advance.txt",
        "advanced\n",
        "advance",
    );
    let (advanced, _) = manager
        .verify_owned_current("rewind")
        .expect("adopt advanced head");
    assert_eq!(advanced.head_commit, advanced_head);

    fixture_git(
        &original.worktree_path,
        &["reset", "--hard", &original.head_commit],
    );
    assert!(matches!(
        manager.verify_owned_current("rewind"),
        Err(WorktreeError::OwnershipMismatch { .. })
    ));
    assert_eq!(
        manager.list().expect("catalog after rejected rewind"),
        vec![advanced]
    );
}

#[cfg(unix)]
#[test]
fn symlinked_managed_root_is_rejected() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new("source", "real-managed");
    fs::create_dir_all(&fixture.managed).expect("real managed root");
    let link = fixture.sandbox.path().join("linked-managed");
    symlink(&fixture.managed, &link).expect("managed symlink");
    let options = CreateWorktreeOptions {
        managed_root: link,
        base_commit: None,
        timeout: Some(TEST_TIMEOUT),
    };
    assert!(matches!(
        fixture.manager().create("managed-link", options),
        Err(WorktreeError::Symlink)
    ));
}

#[cfg(unix)]
#[test]
fn symlinked_source_component_is_rejected() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new("source", "managed");
    let link = fixture.sandbox.path().join("linked-source");
    symlink(&fixture.repo, &link).expect("source symlink");
    let capabilities = fixture
        .application
        .capture_generation()
        .sandbox()
        .capabilities()
        .clone();
    assert!(
        capabilities
            .open_root(
                crate::fs::CapabilityRootPurpose::ProjectResources,
                pi_sandbox::AbsolutePath::parse(link).expect("absolute linked root"),
                crate::fs::CapabilityAccess::ReadWrite,
                crate::fs::RootCreation::Existing,
            )
            .is_err()
    );
}

#[test]
fn unborn_repository_create_fails_closed_without_partial_worktree() {
    let sandbox = TempDir::new().expect("temporary sandbox");
    let repo = sandbox.path().join("unborn");
    let managed = sandbox.path().join("managed");
    fs::create_dir_all(&repo).expect("unborn repo dir");
    fixture_git(&repo, &["init"]);
    fixture_git(&repo, &["config", "user.name", "Pi Test"]);
    fixture_git(&repo, &["config", "user.email", "pi@example.test"]);
    let (agent, sessions, faux, application) = published_application(&repo);
    let capabilities = application
        .capture_generation()
        .sandbox()
        .capabilities()
        .clone();
    let source_lease = capabilities
        .open_root(
            crate::fs::CapabilityRootPurpose::ProjectResources,
            pi_sandbox::AbsolutePath::parse(repo.clone()).expect("absolute source root"),
            crate::fs::CapabilityAccess::ReadWrite,
            crate::fs::RootCreation::Existing,
        )
        .expect("source lease");
    let managed_lease = capabilities
        .open_root(
            crate::fs::CapabilityRootPurpose::WorkflowWorktrees,
            pi_sandbox::AbsolutePath::parse(managed.clone()).expect("absolute managed root"),
            crate::fs::CapabilityAccess::PrivateReadWrite,
            crate::fs::RootCreation::CreateDirectory,
        )
        .expect("managed lease");
    let manager = WorkflowWorktreeManager::new(source_lease, managed_lease)
        .with_timeout(TEST_TIMEOUT)
        .with_process_context(
            application
                .governed_process_context()
                .with_repository_test_executor(repository_test_executor()),
        );
    let _retained_fixture_authority = (&agent, &sessions, &faux, &application);
    let options = CreateWorktreeOptions {
        managed_root: managed.clone(),
        base_commit: None,
        timeout: Some(TEST_TIMEOUT),
    };
    let error = manager
        .create("unborn", options)
        .expect_err("unborn repository must fail closed before any worktree is created");
    // The unborn case is classified to a fixed, actionable category before the
    // runtime factory can mask the raw `git rev-parse --verify HEAD` failure.
    // The category never carries the absolute repo path or git stderr.
    assert!(
        matches!(error, WorktreeError::UnbornHead),
        "unborn repository must classify to UnbornHead, got {error:?}"
    );
    let message = error.to_string();
    assert!(
        message.contains("no commits"),
        "unborn message must be actionable: {message}"
    );
    assert!(
        !message.contains(&*repo.to_string_lossy()),
        "unborn message must not leak the source path: {message}"
    );
    assert!(
        !message.contains(&*managed.to_string_lossy()),
        "unborn message must not leak the managed path: {message}"
    );
    assert!(!managed.join(WORKTREE_ROOT_DIR_NAME).exists());
    assert!(!sandbox.path().join("catalog.json").exists());
}

/// Shared fixture for the broken-HEAD classification negatives: a committed
/// repo with a deliberately broken HEAD plus any per-test ref manipulation.
/// Holds the sandbox/agent/application alive and exposes the hermetic-executor
/// worktree manager + create options + paths, so each test only applies its
/// distinct ref setup and behavioral assertions.
struct BrokenHeadFixture {
    _sandbox: TempDir,
    _agent: TempDir,
    _sessions: TempDir,
    _faux: pi_ai::providers::FauxProviderRegistration,
    _application: crate::Application,
    manager: WorkflowWorktreeManager,
    options: CreateWorktreeOptions,
    managed: PathBuf,
    sandbox: PathBuf,
}

/// Build a committed repo, apply `distinct_refs` (the per-test HEAD/branch/tag
/// manipulation), publish the application, and assemble the hermetic-executor
/// worktree manager over it. The catalog/worktree paths stay under the fixture
/// so each test can assert no partial mutation.
fn broken_head_fixture(
    repo_name: &str,
    managed_name: &str,
    distinct_refs: impl FnOnce(&Path),
) -> BrokenHeadFixture {
    let sandbox = TempDir::new().expect("temporary sandbox");
    let repo = sandbox.path().join(repo_name);
    let managed = sandbox.path().join(managed_name);
    fs::create_dir_all(&repo).expect("repo dir");
    fixture_git(&repo, &["init"]);
    fixture_git(&repo, &["config", "user.name", "Pi Test"]);
    fixture_git(&repo, &["config", "user.email", "pi@example.test"]);
    fs::write(repo.join("README.md"), "base\n").expect("base file");
    fixture_git(&repo, &["add", "README.md"]);
    fixture_git(&repo, &["commit", "-m", "committed base"]);
    distinct_refs(&repo);
    let (agent, sessions, faux, application) = published_application(&repo);
    let capabilities = application
        .capture_generation()
        .sandbox()
        .capabilities()
        .clone();
    let source_lease = capabilities
        .open_root(
            crate::fs::CapabilityRootPurpose::ProjectResources,
            pi_sandbox::AbsolutePath::parse(repo.clone()).expect("absolute source root"),
            crate::fs::CapabilityAccess::ReadWrite,
            crate::fs::RootCreation::Existing,
        )
        .expect("source lease");
    let managed_lease = capabilities
        .open_root(
            crate::fs::CapabilityRootPurpose::WorkflowWorktrees,
            pi_sandbox::AbsolutePath::parse(managed.clone()).expect("absolute managed root"),
            crate::fs::CapabilityAccess::PrivateReadWrite,
            crate::fs::RootCreation::CreateDirectory,
        )
        .expect("managed lease");
    let manager = WorkflowWorktreeManager::new(source_lease, managed_lease)
        .with_timeout(TEST_TIMEOUT)
        .with_process_context(
            application
                .governed_process_context()
                .with_repository_test_executor(repository_test_executor()),
        );
    let options = CreateWorktreeOptions {
        managed_root: managed.clone(),
        base_commit: None,
        timeout: Some(TEST_TIMEOUT),
    };
    let sandbox_path = sandbox.path().to_path_buf();
    BrokenHeadFixture {
        _sandbox: sandbox,
        _agent: agent,
        _sessions: sessions,
        _faux: faux,
        _application: application,
        manager,
        options,
        managed,
        sandbox: sandbox_path,
    }
}

/// Negative classification: a `git rev-parse --verify HEAD` failure that is
/// NOT an unborn repository (the repo HAS commits but HEAD points at a
/// non-existent branch ref) must NOT be classified as `UnbornHead`. The
/// independent `for-each-ref` (ALL refs) probe finds the committed branch, so
/// the original `CommandFailed` is preserved for generic fail-closed masking.
/// This guards against the over-broad "same argv implies UnbornHead" rule.
#[test]
fn broken_head_failure_is_not_classified_as_unborn_head() {
    let fixture = broken_head_fixture("broken-head", "managed-broken", |repo| {
        // The repo HAS a commit on `main`, but HEAD is repointed at a non-existent
        // branch so `git rev-parse --verify HEAD` fails with the same argv as an
        // unborn repo — without the independent commit probe this would be a
        // false-positive `UnbornHead`.
        fixture_git(repo, &["symbolic-ref", "HEAD", "refs/heads/nonexistent"]);
    });
    let error = fixture
        .manager
        .create("broken-head", fixture.options.clone())
        .expect_err("broken HEAD must fail closed, never silently create a worktree");
    assert!(
        matches!(error, WorktreeError::CommandFailed { .. }),
        "a committed repo with a broken HEAD must stay CommandFailed (generic masking), got {error:?}"
    );
    assert!(
        !matches!(error, WorktreeError::UnbornHead),
        "a repo WITH commits must never classify to UnbornHead: {error:?}"
    );
    assert!(!fixture.managed.join(WORKTREE_ROOT_DIR_NAME).exists());
}

/// Negative classification: a committed repo that has NO local branch heads
/// (the branch was deleted) but DOES have a tag — so commits are reachable via
/// `refs/tags/*` — must NOT be classified as `UnbornHead` even though
/// `git rev-parse --verify HEAD` fails (HEAD is repointed at a missing ref).
/// The independent `for-each-ref` (ALL refs) probe finds the tag, so the
/// original `CommandFailed` is preserved for generic fail-closed masking. This
/// guards against a `refs/heads`-only proof, which a tag-only repo would defeat.
#[test]
fn tag_only_repo_with_broken_head_is_not_classified_as_unborn_head() {
    let fixture = broken_head_fixture("tag-only", "managed-tag", |repo| {
        // A tag keeps the commit reachable via refs/tags/* even after the local
        // branch is deleted and HEAD is repointed at a missing ref.
        fixture_git(repo, &["tag", "v1"]);
        fixture_git(repo, &["symbolic-ref", "HEAD", "refs/heads/nonexistent"]);
        fixture_git(repo, &["branch", "-D", "main"]);
    });
    let error = fixture
        .manager
        .create("tag-only", fixture.options.clone())
        .expect_err(
            "a tag-only repo with a broken HEAD must fail closed, never silently create a worktree",
        );
    assert!(
        matches!(error, WorktreeError::CommandFailed { .. }),
        "a tag-only committed repo must stay CommandFailed (generic masking), got {error:?}"
    );
    assert!(
        !matches!(error, WorktreeError::UnbornHead),
        "a repo WITH a reachable commit (via tag) must never classify to UnbornHead: {error:?}"
    );
    assert!(!fixture.managed.join(WORKTREE_ROOT_DIR_NAME).exists());
    assert!(!fixture.sandbox.join("catalog.json").exists());
}

#[cfg(unix)]
#[test]
fn hostile_hooks_and_config_do_not_execute_during_worktree_creation() {
    use std::os::unix::fs::PermissionsExt;

    // A cloned repo may carry a hostile `post-checkout` hook in `.git/hooks`
    // (supply-chain attack) or redirect hooks via a `core.hooksPath` config to a
    // malicious directory. Workflow worktree creation must never execute such
    // hooks: they could write outside the managed worktree and escape isolation.
    let fixture = Fixture::new("source", "managed");
    let marker_default = fixture.sandbox.path().join("hostile-default.marker");
    let marker_redirect = fixture.sandbox.path().join("hostile-redirect.marker");

    // Hostile hook #1: default hooks dir.
    fs::create_dir_all(fixture.repo.join(".git").join("hooks")).expect("hooks dir");
    let default_hook = fixture
        .repo
        .join(".git")
        .join("hooks")
        .join("post-checkout");
    fs::write(
        &default_hook,
        format!("#!/bin/sh\necho pwned > {}\n", marker_default.display()),
    )
    .expect("write default hook");
    fs::set_permissions(&default_hook, fs::Permissions::from_mode(0o755)).expect("chmod hook");

    // Hostile hook #2: a redirected hooks dir selected via repo `core.hooksPath`.
    let redirect_dir = fixture.sandbox.path().join("evil-hooks");
    fs::create_dir_all(&redirect_dir).expect("redirect hooks dir");
    let redirect_hook = redirect_dir.join("post-checkout");
    fs::write(
        &redirect_hook,
        format!("#!/bin/sh\necho pwned > {}\n", marker_redirect.display()),
    )
    .expect("write redirect hook");
    fs::set_permissions(&redirect_hook, fs::Permissions::from_mode(0o755))
        .expect("chmod redirect hook");
    fixture_git(
        &fixture.repo,
        &["config", "core.hooksPath", redirect_dir.to_str().unwrap()],
    );

    // Worktree creation must succeed without running either hostile hook.
    let identity = fixture
        .manager()
        .create("hostile", fixture.create_options())
        .expect("worktree created without executing hooks");
    assert!(identity.worktree_path.exists());
    assert!(
        !marker_default.exists(),
        "workflow worktree creation must not run the source repo post-checkout hook"
    );
    assert!(
        !marker_redirect.exists(),
        "workflow worktree creation must not honor a hostile core.hooksPath redirect"
    );

    // Integrate also runs git in the source repo; assert it too suppresses hooks
    // by committing a clean change and integrating (merge) without triggering them.
    commit_file(&identity.worktree_path, "feature.txt", "done\n", "feature");
    let outcome = fixture
        .manager()
        .integrate("hostile", IntegrateOptions::default())
        .expect("integrate");
    assert!(matches!(outcome, IntegrateOutcome::Applied { .. }));
    assert!(
        !marker_default.exists(),
        "integrate must not run post-merge/post-commit hooks"
    );
    assert!(
        !marker_redirect.exists(),
        "integrate must not honor hostile core.hooksPath redirect"
    );
}

#[test]
fn session_namespace_encoding_is_collision_free_and_stable() {
    let fixture = Fixture::new("source", "managed");
    let manager = fixture.manager();
    // `proj/abc`, `proj\abc` (on unix a backslash is a legal id char), and
    // `proj:abc` all mapped to `proj-abc` before the digest disambiguator;
    // each must now get its own namespace.
    let slash = manager.session_namespace("proj/abc");
    let backslash = manager.session_namespace("proj\\abc");
    let colon = manager.session_namespace("proj:abc");
    let dash = manager.session_namespace("proj-abc");
    let mut namespaces = vec![slash.clone(), backslash, colon, dash];
    for (index, namespace) in namespaces.iter().enumerate() {
        assert!(
            !namespaces[..index].contains(&namespace),
            "distinct session ids must not share a namespace: {namespace}"
        );
    }
    // The same id resolves deterministically.
    assert_eq!(manager.session_namespace("proj/abc"), slash);
    // Native UUID ids carry no separators and pass through unchanged.
    let uuid = "123e4567-e89b-12d3-a456-426614174000";
    let repo = manager.repository_namespace().expect("git discovery");
    assert_eq!(manager.session_namespace(uuid), format!("{repo}/{uuid}"));
    // Empty id falls back to the stable placeholder.
    assert_eq!(manager.session_namespace(""), format!("{repo}/session"));
}

/// Committed-HEAD regression through the production `WorkflowWorktreeManager`
/// (the exact manager `ApplicationWorkflowRuntimeFactory::create` drives via
/// `setup_workflows`): create → list → inspect(show) → remove(cancel) must all
/// succeed against a temporary repository with a real committed base, the
/// inverse of the unborn classification above. No real repository history is
/// mutated: every commit lands in the temporary sandbox.
#[test]
fn committed_repository_create_list_show_cancel_succeeds_through_production_manager() {
    let fixture = Fixture::new("committed-source", "committed-managed");
    let manager = fixture.manager();

    // create: a committed base lets `discover_repo` resolve HEAD and the
    // worktree is allocated under the managed root, outside the source tree.
    let identity = manager
        .create("committed", fixture.create_options())
        .expect("committed repository must create a worktree");
    assert!(identity.worktree_path.exists());
    assert!(!identity.worktree_path.starts_with(&fixture.repo));
    assert!(identity.branch.starts_with(WORKFLOW_BRANCH_PREFIX));
    assert!(!identity.base_commit.is_empty());
    assert!(!identity.head_commit.is_empty());

    // list: the catalog records exactly the owned identity.
    assert_eq!(
        manager.list().expect("catalog list"),
        vec![identity.clone()]
    );

    // show: inspect projects the live owned worktree status.
    let status = manager
        .inspect("committed")
        .expect("committed repository must inspect the owned worktree");
    assert_eq!(status.identity.workflow_id, identity.workflow_id);
    assert_eq!(status.identity.branch, identity.branch);
    assert!(!status.dirty);

    // cancel: remove destroys only the owned worktree and branch.
    manager
        .remove(&identity)
        .expect("committed repository must remove");
    assert!(!identity.worktree_path.exists());
    assert!(!branch_exists(&manager, &fixture.repo, &identity.branch));
    assert!(
        manager
            .list()
            .expect("catalog empty after remove")
            .is_empty()
    );
}

/// Production-governed success regression: drive `WorkflowWorktreeManager::create`
/// under the parent Application's PRODUCTION `governed_process_context()` —
/// WITHOUT the hermetic `repository_test_executor` seam every other worktree
/// test uses — so git runs through the real sandbox repository-helper spawn
/// path exactly like `rpi`. Under the default `DangerFullAccess` sandbox the
/// RawHost backend must accept the spawn (no retained working directory) and
/// the worktree must materialize, record the workflow branch, and remove
/// cleanly. This is the `/workflow create` immediate-create blocker
/// regression: it failed before the `run_with_abort` DangerFullAccess guard
/// with `InvalidRetainedWorkingDirectory`. Reuses the committed `Fixture`.
#[test]
fn production_governed_worktree_create_succeeds_through_repository_helper_spawn() {
    let fixture = Fixture::new("governed-source", "governed-managed");
    // NO `.with_repository_test_executor(...)` — the production governed
    // repository-helper spawn path, matching the real rpi E2E.
    let manager =
        WorkflowWorktreeManager::new(fixture.source_lease.clone(), fixture.managed_lease.clone())
            .with_timeout(TEST_TIMEOUT)
            .with_process_context(fixture.application.governed_process_context());
    let _retained_fixture_authority = (
        &fixture._agent,
        &fixture._sessions,
        &fixture._faux,
        &fixture.application,
    );
    let identity = manager
        .create("governed", fixture.create_options())
        .expect("production governed worktree create must succeed under the default DangerFullAccess sandbox (NO hermetic executor, the real rpi repository-helper spawn path)");
    assert!(
        identity.worktree_path.exists(),
        "production worktree create must materialize the worktree"
    );
    assert!(
        identity.branch.starts_with(WORKFLOW_BRANCH_PREFIX),
        "production worktree create must record the workflow branch"
    );
    manager
        .remove(&identity)
        .expect("production governed worktree must remove cleanly");
    assert!(
        !identity.worktree_path.exists(),
        "remove must destroy the worktree"
    );
    assert!(
        !branch_exists(&manager, &fixture.repo, &identity.branch),
        "remove must delete the workflow branch"
    );
    assert!(
        manager
            .list()
            .expect("catalog readable after remove")
            .is_empty(),
        "catalog must be empty after remove"
    );
}

/// Security invariant: under DangerFullAccess a hermetic repository executor
/// MUST NOT observe a foreign or stale cwd. The cwd-provenance + staleness
/// check inside `prepare_repository_spawn` runs unconditionally (even when the
/// retained-working-directory binding is skipped for RawHost), so a foreign
/// cwd is rejected ("outside retained authority") and a stale (removed) cwd
/// fails the descendant-lease open BEFORE the executor is reached. The
/// executor is also structurally gated off under DangerFullAccess (no
/// retained-authority binding). A panicking executor canary proves
/// non-observation: if it were invoked the test would panic.
#[cfg(unix)]
#[test]
fn danger_full_access_hermetic_executor_cannot_observe_foreign_or_stale_cwd() {
    let fixture = Fixture::new("foreign-source", "foreign-managed");
    // Panicking executor canary: invocation under DangerFullAccess would
    // violate the retained-authority invariant and panic the test.
    let panicking_executor: crate::process::RepositoryTestExecutor = Arc::new(|_, _, _| {
        panic!(
            "hermetic repository executor must not observe a foreign/stale cwd under DangerFullAccess"
        )
    });
    let context = fixture
        .application
        .governed_process_context()
        .with_repository_authorities(fixture.source_lease.clone(), fixture.managed_lease.clone())
        .with_repository_test_executor(panicking_executor);
    let _retained_fixture_authority = (
        &fixture._agent,
        &fixture._sessions,
        &fixture._faux,
        &fixture.application,
    );
    // Foreign cwd: outside the retained source/managed authority.
    let foreign = tempfile::tempdir().expect("foreign cwd");
    let error = repository_namespace(&context, foreign.path())
        .expect_err("foreign cwd must be rejected before the hermetic executor");
    let message = error.to_string();
    assert!(
        message.contains("outside retained authority"),
        "foreign cwd must fail at the retained-authority bridge, not the executor: {message}"
    );
    // Stale cwd: under the retained source authority but no longer exists.
    let stale = fixture.repo.join("removed-subdir");
    fs::create_dir_all(&stale).expect("create stale");
    fs::remove_dir_all(&stale).expect("remove stale");
    let error = repository_namespace(&context, &stale)
        .expect_err("stale cwd must be rejected before the hermetic executor");
    let message = error.to_string();
    assert!(
        !message.contains("git") && !message.contains("CommandFailed"),
        "stale cwd must fail at the retained-authority bridge, not git execution: {message}"
    );
}

/// Worktree namespace isolation: the repository namespace is stable for a
/// given committed repo (same-CWD) and distinct across repositories
/// (cross-CWD), so two managers over different repos never collide on the
/// same managed-root catalog namespace. The session namespace layers a
/// session-id segment on top, keeping same-repo sessions distinct.
#[test]
fn worktree_namespace_is_repo_scoped_and_session_isolated_same_and_cross_cwd() {
    let alpha = Fixture::new("alpha-source", "alpha-managed");
    let beta = Fixture::new("beta-source", "beta-managed");

    // Same-CWD: the repository namespace is stable across two managers over
    // the same committed repo (deterministic digest of the common git dir).
    let alpha_repo = alpha
        .manager()
        .repository_namespace()
        .expect("alpha repository namespace");
    let alpha_repo_again = alpha
        .manager()
        .repository_namespace()
        .expect("alpha repository namespace replay");
    assert_eq!(alpha_repo, alpha_repo_again, "same cwd must be stable");

    // Cross-CWD: a different committed repo derives a different namespace.
    let beta_repo = beta
        .manager()
        .repository_namespace()
        .expect("beta repository namespace");
    assert_ne!(alpha_repo, beta_repo, "cross cwd must isolate");

    // Session isolation: same repo, distinct session ids stay distinct; the
    // same session id is stable. The repository segment is shared but the
    // session segment disambiguates.
    let alpha_manager = alpha.manager();
    let session_a = alpha_manager.session_namespace("session-a");
    let session_b = alpha_manager.session_namespace("session-b");
    assert_ne!(
        session_a, session_b,
        "same repo distinct sessions must isolate"
    );
    assert_eq!(
        alpha_manager.session_namespace("session-a"),
        session_a,
        "same session id must be stable"
    );
    assert!(session_a.starts_with(&format!("{alpha_repo}/")));
    assert!(session_b.starts_with(&format!("{alpha_repo}/")));

    // The managed-root catalog never crosses repositories: alpha's create
    // only ever records into alpha's catalog, never beta's.
    let identity = alpha
        .manager()
        .create("alpha-workflow", alpha.create_options())
        .expect("alpha create");
    assert_eq!(
        alpha.manager().list().expect("alpha catalog"),
        vec![identity.clone()]
    );
    assert!(
        beta.manager().list().expect("beta catalog").is_empty(),
        "cross-cwd catalog must stay isolated"
    );
    alpha.manager().remove(&identity).expect("alpha remove");
}
