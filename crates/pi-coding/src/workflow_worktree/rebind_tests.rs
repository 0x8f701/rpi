//! Behavioral coverage for the session worktree root rebind.
//!
//! Every fixture is built at runtime under a temp directory: a real git repo
//! with a real linked worktree, a hand-written ownership catalog, and a
//! workflow store — all moved by renaming the checkout, then rebound through
//! the public two-phase API.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

use super::*;

const SESSION_ID: &str = "0f0e1d2c-3b4a-5968-8796-aabbccddeeff";
const WORKFLOW_ID: &str = "wf-1";
const BRANCH: &str = "rpi/workflow/wf-1";

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("run git fixture");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("utf8 git output")
        .trim()
        .to_owned()
}

fn namespace_digest(cwd: &Path) -> String {
    session_namespace(None, cwd, "digest-probe")
        .split('/')
        .next()
        .expect("repository digest")
        .to_owned()
}

fn session_dir(agent_dir: &Path, root: &str, digest: &str) -> PathBuf {
    agent_dir.join(root).join(digest).join(SESSION_ID)
}

fn old_store_dir(agent_dir: &Path, old_digest: &str) -> PathBuf {
    session_dir(agent_dir, WORKFLOW_STORE_ROOT_DIR_NAME, old_digest)
}

fn new_store_dir(agent_dir: &Path, new_digest: &str) -> PathBuf {
    session_dir(agent_dir, WORKFLOW_STORE_ROOT_DIR_NAME, new_digest)
}

fn old_managed_dir(agent_dir: &Path, old_digest: &str) -> PathBuf {
    session_dir(agent_dir, WORKTREE_ROOT_DIR_NAME, old_digest)
}

fn new_managed_dir(agent_dir: &Path, new_digest: &str) -> PathBuf {
    session_dir(agent_dir, WORKTREE_ROOT_DIR_NAME, new_digest)
}

fn catalog_path(managed: &Path) -> PathBuf {
    managed
        .join(super::catalog::CATALOG_DIR_NAME)
        .join("worktrees.json")
}

fn read_catalog(managed: &Path) -> serde_json::Value {
    serde_json::from_str(&fs::read_to_string(catalog_path(managed)).expect("catalog file"))
        .expect("catalog json")
}

fn write_catalog(managed: &Path, catalog: &serde_json::Value) {
    fs::create_dir_all(managed.join(super::catalog::CATALOG_DIR_NAME)).expect("catalog dir");
    fs::write(catalog_path(managed), serde_json::to_vec(catalog).expect("serialize"))
        .expect("write catalog");
}

struct Fixture {
    _base: TempDir,
    agent_dir: PathBuf,
    /// Canonical old checkout path (renamed away during `move_repo`).
    old_repo: PathBuf,
    /// Canonical new checkout path (only valid after `move_repo`).
    new_repo: PathBuf,
    old_digest: String,
    new_digest: String,
    head_commit: String,
    worktree_path: PathBuf,
}

impl Fixture {
    /// Builds a committed repo, optionally with a managed worktree and
    /// ownership catalog, a workflow store, and then renames the checkout to
    /// its new location. Everything is staged against the OLD repo path
    /// before the rename, mirroring the offline move contract.
    fn build(with_worktree: bool) -> Self {
        let base = TempDir::new().expect("fixture base");
        let agent_dir = base.path().join("agent");
        fs::create_dir_all(&agent_dir).expect("agent dir");
        let repo_source = base.path().join("repo-old");
        fs::create_dir_all(&repo_source).expect("repo dir");
        git(&repo_source, &["init"]);
        git(&repo_source, &["config", "user.email", "test@example.com"]);
        git(&repo_source, &["config", "user.name", "Pi Test"]);
        fs::write(repo_source.join("file.txt"), "one\n").expect("tracked file");
        git(&repo_source, &["add", "."]);
        git(&repo_source, &["commit", "-m", "init"]);
        let head_commit = git(&repo_source, &["rev-parse", "HEAD"]);
        let old_repo = repo_source.canonicalize().expect("canonical old repo");
        let old_digest = namespace_digest(&old_repo);
        let store = old_store_dir(&agent_dir, &old_digest);
        fs::create_dir_all(&store).expect("old store dir");
        fs::write(store.join("state.json"), "{\"workflow\":\"state\"}\n").expect("store state");

        let mut worktree_path = PathBuf::new();
        if with_worktree {
            let managed = old_managed_dir(&agent_dir, &old_digest);
            let worktrees = managed.join(WORKTREE_ROOT_DIR_NAME);
            fs::create_dir_all(&worktrees).expect("managed worktrees dir");
            let worktree = worktrees.join(WORKFLOW_ID);
            git(
                &old_repo,
                &[
                    "worktree",
                    "add",
                    "-b",
                    BRANCH,
                    worktree.to_str().expect("utf8 worktree path"),
                    &head_commit,
                ],
            );
            let catalog = serde_json::json!({
                "entries": {
                    WORKFLOW_ID: {
                        "workflowId": WORKFLOW_ID,
                        "sourceRoot": old_repo,
                        "commonGitDir": old_repo.join(".git"),
                        "worktreePath": worktree,
                        "branch": BRANCH,
                        "baseCommit": head_commit,
                        "headCommit": head_commit,
                        "createdAtMs": 42_u64,
                    }
                }
            });
            write_catalog(&managed, &catalog);
            worktree_path = worktree;
        }

        let new_repo_target = base.path().join("repo-new");
        fs::rename(&old_repo, &new_repo_target).expect("move repo");
        let new_repo = new_repo_target.canonicalize().expect("canonical new repo");
        let new_digest = namespace_digest(&new_repo);
        Self {
            _base: base,
            agent_dir,
            old_repo,
            new_repo,
            old_digest,
            new_digest,
            head_commit,
            worktree_path,
        }
    }

    /// Committed repo with one managed worktree, a catalog entry, and a
    /// workflow store, with the checkout renamed to its new location.
    fn with_worktree() -> Self {
        Self::build(true)
    }

    /// Workflow store only: no managed root, no worktree, no catalog.
    fn store_only() -> Self {
        Self::build(false)
    }

    fn new_worktree_path(&self) -> PathBuf {
        new_managed_dir(&self.agent_dir, &self.new_digest)
            .join(WORKTREE_ROOT_DIR_NAME)
            .join(WORKFLOW_ID)
    }
}

#[test]
fn rebind_relocates_roots_and_rewrites_catalog() {
    let fixture = Fixture::with_worktree();
    let rebind = rebind_session_worktree_roots(
        &fixture.agent_dir,
        &fixture.old_repo,
        &fixture.new_repo,
        SESSION_ID,
    )
    .expect("rebind");
    assert!(rebind, "a fresh relocation must report true");

    // Old namespace fully removed under both roots.
    assert!(!old_managed_dir(&fixture.agent_dir, &fixture.old_digest).exists());
    assert!(!old_store_dir(&fixture.agent_dir, &fixture.old_digest).exists());

    // Catalog: exact StoredWorktreeIdentity round-trip with rewritten paths.
    let catalog = read_catalog(&new_managed_dir(&fixture.agent_dir, &fixture.new_digest));
    let entry = &catalog["entries"][WORKFLOW_ID];
    assert_eq!(
        entry["sourceRoot"].as_str().expect("sourceRoot"),
        fixture.new_repo.to_str().expect("utf8 new repo")
    );
    assert_eq!(
        entry["commonGitDir"].as_str().expect("commonGitDir"),
        fixture.new_repo.join(".git").to_str().expect("utf8 git dir")
    );
    assert_eq!(
        entry["worktreePath"].as_str().expect("worktreePath"),
        fixture.new_worktree_path().to_str().expect("utf8 wt")
    );
    // No random identities: id, branch, commits, and timestamp survive.
    assert_eq!(entry["workflowId"].as_str().expect("id"), WORKFLOW_ID);
    assert_eq!(entry["branch"].as_str().expect("branch"), BRANCH);
    assert_eq!(
        entry["headCommit"].as_str().expect("head"),
        fixture.head_commit
    );
    assert_eq!(
        entry["baseCommit"].as_str().expect("base"),
        fixture.head_commit
    );
    assert_eq!(entry["createdAtMs"].as_u64().expect("createdAtMs"), 42);

    // The relocated worktree's git pointer now targets the moved checkout.
    let pointer = fs::read_to_string(fixture.new_worktree_path().join(".git")).expect(".git file");
    assert_eq!(
        pointer.trim(),
        format!(
            "gitdir: {}",
            fixture.new_repo.join(".git/worktrees").join(WORKFLOW_ID).display()
        )
    );

    // The workflow store moved with its contents intact.
    assert!(
        new_store_dir(&fixture.agent_dir, &fixture.new_digest)
            .join("state.json")
            .exists()
    );
    // The staged repository lock never carries over.
    assert!(!new_managed_dir(&fixture.agent_dir, &fixture.new_digest)
        .join(super::catalog::REPO_LOCK_FILE_NAME)
        .exists());
}

#[test]
fn rebind_preserves_dirty_and_active_worktrees() {
    let fixture = Fixture::with_worktree();
    // Dirty state: a tracked modification plus an untracked file.
    let tracked = fixture.worktree_path.join("file.txt");
    fs::write(&tracked, "dirty working copy\n").expect("dirty tracked file");
    let untracked = fixture.worktree_path.join("notes.txt");
    fs::write(&untracked, "untracked work\n").expect("untracked file");

    let rebind = rebind_session_worktree_roots(
        &fixture.agent_dir,
        &fixture.old_repo,
        &fixture.new_repo,
        SESSION_ID,
    )
    .expect("rebind");
    assert!(rebind);

    let moved_worktree = fixture.new_worktree_path();
    assert_eq!(
        fs::read_to_string(moved_worktree.join("file.txt")).expect("dirty tracked content"),
        "dirty working copy\n"
    );
    assert_eq!(
        fs::read_to_string(moved_worktree.join("notes.txt")).expect("untracked content"),
        "untracked work\n"
    );
    // The worktree stays an active git checkout of the moved repository.
    let pointer = fs::read_to_string(moved_worktree.join(".git")).expect(".git pointer");
    assert!(
        pointer
            .trim()
            .strip_prefix("gitdir: ")
            .is_some_and(|target| Path::new(target).starts_with(&fixture.new_repo)),
        "pointer must target the moved repository: {pointer}"
    );
}

#[test]
fn rebind_refuses_foreign_target_namespace() {
    let fixture = Fixture::with_worktree();
    let target = new_managed_dir(&fixture.agent_dir, &fixture.new_digest);
    fs::create_dir_all(&target).expect("foreign target");
    fs::write(target.join("foreign.txt"), "not ours\n").expect("foreign marker");
    let old_catalog = read_catalog(&old_managed_dir(&fixture.agent_dir, &fixture.old_digest));

    let error = rebind_session_worktree_roots(
        &fixture.agent_dir,
        &fixture.old_repo,
        &fixture.new_repo,
        SESSION_ID,
    )
    .expect_err("foreign target must fail closed");
    assert!(
        error.to_string().contains("refusing"),
        "unexpected error: {error}"
    );
    // Fail closed means fail clean: the old namespace is untouched.
    assert_eq!(read_catalog(&old_managed_dir(&fixture.agent_dir, &fixture.old_digest)), old_catalog);
    assert!(target.join("foreign.txt").exists());
    assert!(fixture.worktree_path.exists());
}

#[test]
fn rollback_restores_the_old_namespace() {
    let fixture = Fixture::with_worktree();
    let old_catalog = read_catalog(&old_managed_dir(&fixture.agent_dir, &fixture.old_digest));
    let prepared = prepare_session_worktree_root_rebind(
        &fixture.agent_dir,
        &fixture.old_repo,
        &fixture.new_repo,
        SESSION_ID,
    )
    .expect("prepare")
    .expect("a fresh relocation prepares a handle");
    prepared.rollback().expect("rollback");

    // Old namespace fully intact and still authoritative.
    assert_eq!(read_catalog(&old_managed_dir(&fixture.agent_dir, &fixture.old_digest)), old_catalog);
    assert!(fixture.worktree_path.exists());
    assert!(old_store_dir(&fixture.agent_dir, &fixture.old_digest)
        .join("state.json")
        .exists());
    // Staged copies removed.
    assert!(!new_managed_dir(&fixture.agent_dir, &fixture.new_digest).exists());
    assert!(!new_store_dir(&fixture.agent_dir, &fixture.new_digest).exists());

    // After a rollback the rebind can run cleanly again.
    let replay = rebind_session_worktree_roots(
        &fixture.agent_dir,
        &fixture.old_repo,
        &fixture.new_repo,
        SESSION_ID,
    )
    .expect("rebind after rollback");
    assert!(replay);
}

#[test]
fn repeated_rebinds_are_idempotent_noops() {
    let fixture = Fixture::with_worktree();
    assert!(
        rebind_session_worktree_roots(
            &fixture.agent_dir,
            &fixture.old_repo,
            &fixture.new_repo,
            SESSION_ID,
        )
        .expect("first rebind")
    );
    for _ in 0..2 {
        assert!(
            !rebind_session_worktree_roots(
                &fixture.agent_dir,
                &fixture.old_repo,
                &fixture.new_repo,
                SESSION_ID,
            )
            .expect("repeat rebind"),
            "a completed rebind must resolve to a no-op"
        );
    }
    // State is stable across the repeats: exactly one relocated namespace.
    let catalog = read_catalog(&new_managed_dir(&fixture.agent_dir, &fixture.new_digest));
    assert_eq!(
        catalog["entries"][WORKFLOW_ID]["sourceRoot"]
            .as_str()
            .expect("sourceRoot"),
        fixture.new_repo.to_str().expect("utf8 new repo")
    );
    assert!(!old_managed_dir(&fixture.agent_dir, &fixture.old_digest).exists());
}

#[test]
fn same_cwd_rebind_is_a_noop() {
    let fixture = Fixture::store_only();
    // Rebuild the old checkout so both spellings exist (pre-move state).
    let restored = fixture._base.path().join("repo-restored");
    fs::rename(&fixture.new_repo, &restored).expect("restore repo");
    let restored = restored.canonicalize().expect("canonical restored repo");

    let rebind = rebind_session_worktree_roots(
        &fixture.agent_dir,
        &restored,
        &restored,
        SESSION_ID,
    )
    .expect("same-cwd rebind");
    assert!(!rebind, "identical old/new cwd must be a no-op");
    assert!(
        old_store_dir(&fixture.agent_dir, &fixture.old_digest)
            .join("state.json")
            .exists(),
        "the store must stay in place"
    );
}

#[test]
fn store_only_session_relocates_without_catalog() {
    let fixture = Fixture::store_only();
    assert!(
        rebind_session_worktree_roots(
            &fixture.agent_dir,
            &fixture.old_repo,
            &fixture.new_repo,
            SESSION_ID,
        )
        .expect("store-only rebind")
    );
    assert!(new_store_dir(&fixture.agent_dir, &fixture.new_digest)
        .join("state.json")
        .exists());
    assert!(!old_store_dir(&fixture.agent_dir, &fixture.old_digest).exists());
    // Idempotent replay.
    assert!(
        !rebind_session_worktree_roots(
            &fixture.agent_dir,
            &fixture.old_repo,
            &fixture.new_repo,
            SESSION_ID,
        )
        .expect("repeat store-only rebind")
    );
}

#[test]
fn foreign_catalog_entry_fails_closed() {
    let fixture = Fixture::with_worktree();
    let managed = old_managed_dir(&fixture.agent_dir, &fixture.old_digest);
    let foreign = fixture
        ._base
        .path()
        .join("elsewhere")
        .join("foreign-worktree");
    fs::create_dir_all(&foreign).expect("foreign worktree dir");
    let catalog = serde_json::json!({
        "entries": {
            WORKFLOW_ID: {
                "workflowId": WORKFLOW_ID,
                "sourceRoot": fixture.old_repo,
                "commonGitDir": fixture.old_repo.join(".git"),
                "worktreePath": foreign,
                "branch": BRANCH,
                "baseCommit": fixture.head_commit,
                "headCommit": fixture.head_commit,
                "createdAtMs": 42_u64,
            }
        }
    });
    write_catalog(&managed, &catalog);

    let error = rebind_session_worktree_roots(
        &fixture.agent_dir,
        &fixture.old_repo,
        &fixture.new_repo,
        SESSION_ID,
    )
    .expect_err("catalog outside the managed roots must fail closed");
    assert!(
        error.to_string().contains("outside the session's managed roots"),
        "unexpected error: {error}"
    );
    // Nothing moved.
    assert!(managed.join(super::catalog::CATALOG_DIR_NAME).exists());
    assert!(fixture.worktree_path.exists());
}