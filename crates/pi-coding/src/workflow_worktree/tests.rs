use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use tempfile::TempDir;

use super::git::{branch_exists, is_dirty};
use super::*;

const TEST_TIMEOUT: Duration = Duration::from_secs(30);

struct Fixture {
    sandbox: TempDir,
    repo: PathBuf,
    managed: PathBuf,
    catalog: PathBuf,
}

impl Fixture {
    fn new(source_name: &str, managed_name: &str) -> Self {
        let sandbox = TempDir::new().expect("temporary sandbox");
        let repo = sandbox.path().join(source_name);
        let managed = sandbox.path().join(managed_name);
        let catalog = sandbox.path().join("catalog with spaces.json");
        init_repo(&repo);
        Self {
            sandbox,
            repo,
            managed,
            catalog,
        }
    }

    fn manager(&self) -> WorkflowWorktreeManager {
        WorkflowWorktreeManager::new(&self.repo)
            .with_catalog_path(&self.catalog)
            .with_timeout(TEST_TIMEOUT)
    }

    fn create_options(&self) -> CreateWorktreeOptions {
        CreateWorktreeOptions {
            managed_root: self.managed.clone(),
            base_commit: None,
            timeout: Some(TEST_TIMEOUT),
        }
    }
}

fn fixture_git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args([
            "-c",
            "color.ui=false",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
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
    assert!(first.worktree_path.to_string_lossy().contains("managed root with spaces"));

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
    assert!(status.changed_files.iter().any(|path| path == "file with spaces.txt"));
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
    let error = manager.remove(&forged).expect_err("foreign removal rejected");
    assert!(matches!(error, WorktreeError::OwnershipMismatch { .. }));
    assert!(foreign_path.exists());
    assert!(owned.worktree_path.exists());
    assert!(branch_exists(&fixture.repo, foreign_branch, TEST_TIMEOUT).expect("foreign branch"));
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
    assert!(branch_exists(&fixture.repo, &identity.branch, TEST_TIMEOUT).expect("feature branch"));
}

#[test]
fn dirty_worktree_integration_fails_without_ignoring_uncommitted_files() {
    let fixture = Fixture::new("source", "managed");
    let manager = fixture.manager();
    let identity = manager
        .create("dirty-worktree", fixture.create_options())
        .expect("dirty worktree");
    fs::write(identity.worktree_path.join("untracked.txt"), "not committed\n")
        .expect("untracked workflow file");

    let error = manager
        .integrate("dirty-worktree", IntegrateOptions::default())
        .expect_err("dirty workflow must fail closed");
    assert!(matches!(error, WorktreeError::DirtyWorktree(ref id) if id == "dirty-worktree"));
    assert!(identity.worktree_path.join("untracked.txt").exists());
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
    assert!(branch_exists(&fixture.repo, &identity.branch, TEST_TIMEOUT).expect("owned branch"));
    assert!(!is_dirty(&fixture.repo, TEST_TIMEOUT).expect("source clean after abort"));
    assert_eq!(
        fs::read_to_string(fixture.repo.join("README.md")).expect("source content"),
        "source side\n"
    );
    assert_eq!(
        fs::read_to_string(identity.worktree_path.join("README.md"))
            .expect("workflow content"),
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
    assert!(branch_exists(&fixture.repo, &remove.branch, TEST_TIMEOUT).expect("remove branch intact"));
    fs::write(&fixture.catalog, catalog_backup).expect("restore catalog");

    manager.remove(&remove).expect("remove owned identity");
    assert!(!remove.worktree_path.exists());
    assert!(!branch_exists(&fixture.repo, &remove.branch, TEST_TIMEOUT).expect("removed branch"));
    assert!(keep.worktree_path.exists());
    assert!(branch_exists(&fixture.repo, &keep.branch, TEST_TIMEOUT).expect("kept branch"));
    assert!(foreign_path.exists());
    assert!(branch_exists(&fixture.repo, foreign_branch, TEST_TIMEOUT).expect("foreign branch"));

    assert_eq!(manager.list().expect("catalog entries"), vec![keep]);
}

#[test]
fn discovery_dirty_base_invalid_ids_and_managed_root_fail_closed() {
    let sandbox = TempDir::new().expect("temporary sandbox");
    let plain = sandbox.path().join("plain");
    fs::create_dir(&plain).expect("plain directory");
    assert!(matches!(
        WorkflowWorktreeManager::new(&plain).discover(),
        Err(WorktreeError::NotGit)
    ));

    let fixture = Fixture::new("source", "managed");
    let manager = fixture.manager();
    for invalid in ["", "../escape", "slash/id", ".hidden", "bad.lock", "nul\0id"] {
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
    commit_file(&identity.worktree_path, "dirty.txt", "workflow\n", "workflow change");
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

    assert_eq!(manager.prune_stale().expect("prune stale entry"), vec!["stale"]);
    assert!(manager.list().expect("catalog after prune").is_empty());
}

#[test]
fn zero_timeout_is_typed_and_does_not_leak_absolute_paths() {
    let fixture = Fixture::new("source secret timeout", "managed secret timeout");
    let manager = WorkflowWorktreeManager::new(&fixture.repo).with_timeout(Duration::ZERO);
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

    commit_file(&identity.worktree_path, "advance.txt", "advanced\n", "advance head");
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
    assert_eq!(restarted.list().expect("refreshed catalog"), vec![refreshed.clone()]);
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
    assert!(matches!(
        WorkflowWorktreeManager::new(link).discover(),
        Err(WorktreeError::Symlink)
    ));
}

#[test]
fn unborn_repository_create_fails_closed_without_partial_worktree() {
    // `git init` with no HEAD commit (unborn). `discover` resolves the repo root
    // and common git dir but `rev-parse --verify HEAD` fails, so `create` must
    // fail closed with a typed command error and leave no managed worktree dir.
    let sandbox = TempDir::new().expect("temporary sandbox");
    let repo = sandbox.path().join("unborn");
    let managed = sandbox.path().join("managed");
    fs::create_dir_all(&repo).expect("unborn repo dir");
    fixture_git(&repo, &["init"]);
    fixture_git(&repo, &["config", "user.name", "Pi Test"]);
    fixture_git(&repo, &["config", "user.email", "pi@example.test"]);
    // Deliberately no commit: HEAD is unborn.
    let manager = WorkflowWorktreeManager::new(&repo)
        .with_catalog_path(sandbox.path().join("catalog.json"))
        .with_timeout(TEST_TIMEOUT);
    let options = CreateWorktreeOptions {
        managed_root: managed.clone(),
        base_commit: None,
        timeout: Some(TEST_TIMEOUT),
    };
    assert!(
        matches!(manager.create("unborn", options), Err(WorktreeError::CommandFailed { .. })),
        "unborn repo must fail closed with a typed git command error"
    );
    // Fail-closed: no managed worktree directory was created.
    assert!(
        !managed.join(WORKTREE_ROOT_DIR_NAME).exists(),
        "no partial worktree may be left after an unborn-repo failure"
    );
    assert!(
        !sandbox.path().join("catalog.json").exists(),
        "unborn failure must not persist a partial ownership catalog"
    );
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
    let default_hook = fixture.repo.join(".git").join("hooks").join("post-checkout");
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
    fs::set_permissions(&redirect_hook, fs::Permissions::from_mode(0o755)).expect("chmod redirect hook");
    fixture_git(&fixture.repo, &["config", "core.hooksPath", redirect_dir.to_str().unwrap()]);

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
    assert!(!marker_default.exists(), "integrate must not run post-merge/post-commit hooks");
    assert!(!marker_redirect.exists(), "integrate must not honor hostile core.hooksPath");
}
