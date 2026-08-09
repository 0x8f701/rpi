//! Safe git discovery, argv execution, path validation, and ownership checks.

use std::collections::BTreeSet;
use std::fs;
use std::io::{ErrorKind, Read};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use anyhow::anyhow;
use regex::Regex;
use std::sync::LazyLock;
use uuid::Uuid;

use super::{
    IntegrateOutcome, IntegrateStrategy, RepoDiscovery, WORKFLOW_BRANCH_PREFIX,
    WorkflowWorktreeIdentity, WorktreeError,
};

const MAX_STDOUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;
const CAPTURE_BUFFER_BYTES: usize = 8192;
const BRANCH_ALLOCATION_ATTEMPTS: usize = 32;

static URL_CREDENTIALS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)([a-z][a-z0-9+.-]*://)[^\s/@:]+(?::[^\s/@]*)?@")
        .expect("credential redaction regex")
});
static NAMED_SECRET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)((?:token|password|authorization|credential)[=:]\s*)[^\s]+")
        .expect("secret redaction regex")
});

#[derive(Debug)]
struct WorktreeRecord {
    path: PathBuf,
    head: Option<String>,
    branch: Option<String>,
}

#[derive(Debug)]
struct BoundedCapture {
    bytes: Vec<u8>,
    truncated: bool,
}

pub(super) fn discover_repo(
    source: &Path,
    timeout: Duration,
) -> Result<RepoDiscovery, WorktreeError> {
    let source = canonicalize_no_symlink(source)?;
    let repo_root = match run_git_capture(&source, &["rev-parse", "--show-toplevel"], timeout) {
        Ok(root) => canonicalize_no_symlink(Path::new(root.trim()))?,
        Err(WorktreeError::CommandFailed { .. }) => {
            return Err(WorktreeError::NotGit);
        }
        Err(error) => return Err(error),
    };
    let git_dir = run_git_capture(
        &repo_root,
        &["rev-parse", "--path-format=absolute", "--git-dir"],
        timeout,
    )?;
    let common_git_dir = run_git_capture(
        &repo_root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        timeout,
    )?;
    let git_dir = canonicalize_no_symlink(Path::new(git_dir.trim()))?;
    let common_git_dir = canonicalize_no_symlink(Path::new(common_git_dir.trim()))?;
    let head_commit = rev_parse(&repo_root, "HEAD", timeout)?;

    Ok(RepoDiscovery {
        repo_root,
        common_git_dir,
        git_dir,
        head_commit,
    })
}

pub(super) fn rev_parse(
    cwd: &Path,
    revision: &str,
    timeout: Duration,
) -> Result<String, WorktreeError> {
    run_git_capture(cwd, &["rev-parse", "--verify", revision], timeout)
}

pub(super) fn resolve_commit(
    cwd: &Path,
    revision: &str,
    timeout: Duration,
) -> Result<String, WorktreeError> {
    let commit = format!("{revision}^{{commit}}");
    run_git_capture(cwd, &["rev-parse", "--verify", &commit], timeout)
}

pub(super) fn is_dirty(cwd: &Path, timeout: Duration) -> Result<bool, WorktreeError> {
    Ok(!run_git_capture(cwd, &["status", "--porcelain=v1", "-z"], timeout)?.is_empty())
}

pub(super) fn commit_count_ahead(
    cwd: &Path,
    base: &str,
    head: &str,
    timeout: Duration,
) -> Result<u64, WorktreeError> {
    let range = format!("{base}..{head}");
    let count = run_git_capture(cwd, &["rev-list", "--count", &range], timeout)?;
    count.trim().parse().map_err(|error| {
        WorktreeError::Other(anyhow!("parsing git commit count {count:?}: {error}"))
    })
}

pub(super) fn list_changed_files(
    cwd: &Path,
    base: &str,
    timeout: Duration,
) -> Result<Vec<String>, WorktreeError> {
    let mut files = BTreeSet::new();
    collect_nul_paths(
        &mut files,
        &run_git_capture(
            cwd,
            &["diff", "--name-only", "-z", "--find-renames", base, "HEAD"],
            timeout,
        )?,
    );
    collect_nul_paths(
        &mut files,
        &run_git_capture(cwd, &["diff", "--name-only", "-z"], timeout)?,
    );
    collect_nul_paths(
        &mut files,
        &run_git_capture(cwd, &["diff", "--cached", "--name-only", "-z"], timeout)?,
    );
    collect_nul_paths(
        &mut files,
        &run_git_capture(
            cwd,
            &["ls-files", "--others", "--exclude-standard", "-z"],
            timeout,
        )?,
    );
    Ok(files.into_iter().collect())
}

pub(super) fn merge_conflict_paths(
    cwd: &Path,
    timeout: Duration,
) -> Result<Vec<String>, WorktreeError> {
    let output = run_git_capture(
        cwd,
        &["diff", "--name-only", "-z", "--diff-filter=U"],
        timeout,
    )?;
    let mut files = BTreeSet::new();
    collect_nul_paths(&mut files, &output);
    Ok(files.into_iter().collect())
}

pub(super) fn branch_exists(
    cwd: &Path,
    branch: &str,
    timeout: Duration,
) -> Result<bool, WorktreeError> {
    let reference = format!("refs/heads/{branch}");
    let args = ["show-ref", "--verify", "--quiet", reference.as_str()];
    let output = run_git_allow_fail(cwd, &args, timeout)?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(command_failed(&args, &output)),
    }
}

pub(super) fn allocate_branch_name(
    cwd: &Path,
    workflow_id: &str,
    timeout: Duration,
) -> Result<String, WorktreeError> {
    let base = format!("{WORKFLOW_BRANCH_PREFIX}{workflow_id}");
    if !branch_exists(cwd, &base, timeout)? {
        return Ok(base);
    }
    for _ in 0..BRANCH_ALLOCATION_ATTEMPTS {
        let suffix = Uuid::new_v4().simple().to_string();
        let candidate = format!("{base}-{}", &suffix[..12]);
        if !branch_exists(cwd, &candidate, timeout)? {
            return Ok(candidate);
        }
    }
    Err(WorktreeError::Other(anyhow!(
        "unable to allocate a collision-free branch for workflow {workflow_id}"
    )))
}

pub(super) fn verify_worktree_registration(
    repo_root: &Path,
    worktree_path: &Path,
    branch: &str,
    expected_head: &str,
    timeout: Duration,
) -> Result<(), WorktreeError> {
    let wanted = canonicalize_no_symlink(worktree_path)?;
    let records = worktree_records(repo_root, timeout)?;
    let record = records
        .into_iter()
        .find(|record| canonical_for_comparison(&record.path) == wanted)
        .ok_or_else(|| {
            WorktreeError::Other(anyhow!("recorded worktree is not registered"))
        })?;
    let live_branch = record
        .branch
        .as_deref()
        .ok_or_else(|| WorktreeError::Other(anyhow!("recorded worktree is detached")))?;
    if live_branch != branch {
        return Err(WorktreeError::Other(anyhow!(
            "recorded worktree branch does not match expected branch"
        )));
    }
    if !expected_head.is_empty() && record.head.as_deref() != Some(expected_head) {
        return Err(WorktreeError::Other(anyhow!(
            "recorded worktree HEAD does not match expected commit"
        )));
    }
    Ok(())
}

pub(super) fn worktree_is_registered(
    repo_root: &Path,
    worktree_path: &Path,
    timeout: Duration,
) -> Result<bool, WorktreeError> {
    let wanted = canonical_for_comparison(worktree_path);
    Ok(worktree_records(repo_root, timeout)?
        .into_iter()
        .any(|record| canonical_for_comparison(&record.path) == wanted))
}


pub(super) fn integrate_merge(
    discovery: &RepoDiscovery,
    identity: &WorkflowWorktreeIdentity,
    head: &str,
    timeout: Duration,
) -> Result<IntegrateOutcome, WorktreeError> {
    let args = ["merge", "--no-ff", "--no-edit", "--", &identity.branch];
    let merge = run_git_allow_fail(&discovery.repo_root, &args, timeout)?;
    if merge.status.success() {
        let result = rev_parse(&discovery.repo_root, "HEAD", timeout)?;
        return Ok(IntegrateOutcome::Applied {
            strategy: IntegrateStrategy::Merge,
            result_commit: result.clone(),
            merge_commit: Some(result),
        });
    }

    let conflicts = merge_conflict_paths(&discovery.repo_root, timeout)?;
    if conflicts.is_empty() {
        return Err(command_failed(&args, &merge));
    }
    run_git(&discovery.repo_root, &["merge", "--abort"], timeout)?;
    Ok(IntegrateOutcome::Conflicted {
        strategy: IntegrateStrategy::Merge,
        conflicts,
        workflow_id: identity.workflow_id.clone(),
        branch: identity.branch.clone(),
        head_commit: head.to_owned(),
    })
}

pub(super) fn integrate_rebase(
    discovery: &RepoDiscovery,
    identity: &WorkflowWorktreeIdentity,
    head: &str,
    timeout: Duration,
) -> Result<IntegrateOutcome, WorktreeError> {
    let onto = rev_parse(&discovery.repo_root, "HEAD", timeout)?;
    let args = ["rebase", onto.as_str()];
    let rebase = run_git_allow_fail(&identity.worktree_path, &args, timeout)?;
    if !rebase.status.success() {
        let conflicts = merge_conflict_paths(&identity.worktree_path, timeout)?;
        if conflicts.is_empty() {
            return Err(command_failed(&args, &rebase));
        }
        run_git(&identity.worktree_path, &["rebase", "--abort"], timeout)?;
        return Ok(IntegrateOutcome::Conflicted {
            strategy: IntegrateStrategy::Rebase,
            conflicts,
            workflow_id: identity.workflow_id.clone(),
            branch: identity.branch.clone(),
            head_commit: head.to_owned(),
        });
    }

    let rebased_head = rev_parse(&identity.worktree_path, "HEAD", timeout)?;
    run_git(
        &discovery.repo_root,
        &["merge", "--ff-only", "--", &rebased_head],
        timeout,
    )?;
    let result = rev_parse(&discovery.repo_root, "HEAD", timeout)?;
    Ok(IntegrateOutcome::Applied {
        strategy: IntegrateStrategy::Rebase,
        result_commit: result,
        merge_commit: None,
    })
}

pub(super) fn verify_identity_ownership(
    identity: &WorkflowWorktreeIdentity,
    discovery: &RepoDiscovery,
    timeout: Duration,
) -> Result<(), WorktreeError> {
    let expected_branch = format!("{WORKFLOW_BRANCH_PREFIX}{}", identity.workflow_id);
    let allocated_branch = identity.branch == expected_branch
        || identity
            .branch
            .strip_prefix(&format!("{expected_branch}-"))
            .is_some_and(|suffix| {
                suffix.len() == 12 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
            });
    if !allocated_branch {
        return Err(WorktreeError::ownership(
            &identity.workflow_id,
            "recorded branch is outside the workflow allocation namespace",
        ));
    }
    let live_root = canonicalize_no_symlink(&identity.source_root)
        .unwrap_or_else(|_| identity.source_root.clone());
    if live_root != discovery.repo_root {
        return Err(WorktreeError::ownership(
            &identity.workflow_id,
            "identity source does not match manager source",
        ));
    }
    let worktree = canonicalize_no_symlink(&identity.worktree_path).map_err(|_| {
        WorktreeError::ownership(
            &identity.workflow_id,
            "recorded worktree is missing or traverses a symbolic link",
        )
    })?;
    verify_worktree_registration(
        &discovery.repo_root,
        &worktree,
        &identity.branch,
        "",
        timeout,
    )
    .map_err(|_| {
        WorktreeError::ownership(
            &identity.workflow_id,
            "live git registration does not match the recorded identity",
        )
    })
}

pub(super) fn run_git(
    cwd: &Path,
    args: &[&str],
    timeout: Duration,
) -> Result<(), WorktreeError> {
    let output = run_git_allow_fail(cwd, args, timeout)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(command_failed(args, &output))
    }
}

pub(super) fn run_git_capture(
    cwd: &Path,
    args: &[&str],
    timeout: Duration,
) -> Result<String, WorktreeError> {
    let output = run_git_allow_fail(cwd, args, timeout)?;
    if !output.status.success() {
        return Err(command_failed(args, &output));
    }
    String::from_utf8(output.stdout)
        .map(|text| text.trim_end_matches(['\n', '\r']).to_owned())
        .map_err(|error| {
            WorktreeError::Other(anyhow!("git output was not valid UTF-8: {error}"))
        })
}

pub(super) fn run_git_allow_fail(
    cwd: &Path,
    args: &[&str],
    timeout: Duration,
) -> Result<Output, WorktreeError> {
    let cwd = canonicalize_no_symlink(cwd)?;
    let mut command = Command::new("git");
    // Workflow git operations are automated and isolated: never execute
    // source-repo or configured hooks, and never read host/global config. A
    // hostile `post-checkout` planted in a cloned repo (or a `core.hooksPath`
    // redirect) could escape the managed worktree during `worktree add`, and
    // `post-merge`/`post-commit` during integrate. A hostile `~/.gitconfig` (e.g.
    // `core.fsmonitor` or a redirect) could change git behaviour. `-c
    // core.hooksPath=<null device>` overrides every lower-precedence hooksPath
    // (command-line beats repo/local/global/system) so no hook directory is
    // consulted; `GIT_CONFIG_GLOBAL=<null device>` plus `GIT_CONFIG_NOSYSTEM`
    // fully isolates config from the host environment.
    let null_device = if cfg!(unix) { "/dev/null" } else { "NUL" };
    let hooks_override = format!("core.hooksPath={null_device}");
    command
        .args(["--no-pager", "-c", "color.ui=false", "-c", "advice.detachedHead=false", "-c"])
        .arg(&hooks_override)
        .args(args)
        .current_dir(&cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", null_device)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    let mut child = command
        .spawn()
        .map_err(|error| WorktreeError::Other(anyhow!("spawning git: {error}")))?;
    let stdout = child.stdout.take().ok_or_else(|| {
        WorktreeError::Other(anyhow!("git stdout pipe was not available"))
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        WorktreeError::Other(anyhow!("git stderr pipe was not available"))
    })?;
    let stdout_reader = thread::spawn(move || read_bounded(stdout, MAX_STDOUT_BYTES));
    let stderr_reader = thread::spawn(move || read_bounded(stderr, MAX_STDERR_BYTES));
    let started = Instant::now();

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() >= timeout => {
                terminate_child_tree(&mut child);
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(WorktreeError::Timeout {
                    timeout,
                    args: redact_args(args),
                });
            }
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(error) => {
                terminate_child_tree(&mut child);
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(WorktreeError::Other(anyhow!("waiting for git: {error}")));
            }
        }
    };

    let stdout = join_capture(stdout_reader, "stdout")?;
    let mut stderr = join_capture(stderr_reader, "stderr")?;
    if stdout.truncated {
        return Err(WorktreeError::Other(anyhow!(
            "git stdout exceeded {MAX_STDOUT_BYTES} bytes"
        )));
    }
    if stderr.truncated {
        const MARKER: &[u8] = b"\n[stderr truncated]";
        let keep = MAX_STDERR_BYTES.saturating_sub(MARKER.len());
        stderr.bytes.truncate(keep);
        stderr.bytes.extend_from_slice(MARKER);
    }
    Ok(Output {
        status,
        stdout: stdout.bytes,
        stderr: stderr.bytes,
    })
}

pub(super) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

pub(super) fn path_exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

pub(super) fn canonicalize_no_symlink(path: &Path) -> Result<PathBuf, WorktreeError> {
    reject_symlink_components(path)?;
    fs::canonicalize(path)
        .map_err(|error| WorktreeError::Other(anyhow!("canonicalizing path: {error}")))
}

pub(super) fn canonicalize_or_create_dir(path: &Path) -> Result<PathBuf, WorktreeError> {
    create_dir_chain_no_symlink(path)?;
    canonicalize_no_symlink(path)
}

pub(super) fn reject_if_inside(managed: &Path, source_root: &Path) -> Result<(), String> {
    if managed == source_root || managed.starts_with(source_root) {
        Err("managed root is inside the source worktree".to_owned())
    } else {
        Ok(())
    }
}

pub(super) fn sanitize_path_segment(workflow_id: &str) -> String {
    workflow_id.to_owned()
}

pub(super) fn validate_workflow_id(workflow_id: &str) -> Result<(), WorktreeError> {
    let valid = !workflow_id.is_empty()
        && workflow_id.len() <= 128
        && workflow_id.as_bytes()[0].is_ascii_alphanumeric()
        && workflow_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        && !workflow_id.contains("..")
        && !workflow_id.ends_with('.')
        && !workflow_id.ends_with(".lock");
    if valid {
        Ok(())
    } else {
        Err(WorktreeError::InvalidWorkflowId(workflow_id.to_owned()))
    }
}

fn validate_managed_branch(workflow_id: &str, branch: &str) -> Result<(), WorktreeError> {
    validate_workflow_id(workflow_id)?;
    let base = format!("{WORKFLOW_BRANCH_PREFIX}{workflow_id}");
    let valid = branch == base
        || branch.strip_prefix(&format!("{base}-")).is_some_and(|suffix| {
            suffix.len() == 12 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
        });
    if valid {
        Ok(())
    } else {
        Err(WorktreeError::ownership(
            workflow_id,
            format!("branch {branch} is not allocated for this workflow"),
        ))
    }
}

fn worktree_records(
    repo_root: &Path,
    timeout: Duration,
) -> Result<Vec<WorktreeRecord>, WorktreeError> {
    let output = run_git_capture(
        repo_root,
        &["worktree", "list", "--porcelain", "-z"],
        timeout,
    )?;
    let mut records = Vec::new();
    let mut path = None;
    let mut head = None;
    let mut branch = None;
    for field in output.split('\0') {
        if field.is_empty() {
            if let Some(path) = path.take() {
                records.push(WorktreeRecord {
                    path,
                    head: head.take(),
                    branch: branch.take(),
                });
            }
        } else if let Some(value) = field.strip_prefix("worktree ") {
            if let Some(previous) = path.replace(PathBuf::from(value)) {
                records.push(WorktreeRecord {
                    path: previous,
                    head: head.take(),
                    branch: branch.take(),
                });
            }
        } else if let Some(value) = field.strip_prefix("HEAD ") {
            head = Some(value.to_owned());
        } else if let Some(value) = field.strip_prefix("branch refs/heads/") {
            branch = Some(value.to_owned());
        }
    }
    if let Some(path) = path {
        records.push(WorktreeRecord { path, head, branch });
    }
    Ok(records)
}

fn collect_nul_paths(files: &mut BTreeSet<String>, output: &str) {
    files.extend(
        output
            .split('\0')
            .filter(|path| !path.is_empty())
            .map(str::to_owned),
    );
}

fn canonical_for_comparison(path: &Path) -> PathBuf {
    canonicalize_no_symlink(path).unwrap_or_else(|_| path.to_path_buf())
}

pub(super) fn command_failed(args: &[&str], output: &Output) -> WorktreeError {
    WorktreeError::CommandFailed {
        status: output.status.code().unwrap_or(-1),
        args: redact_args(args),
        stderr: redact_stderr(&output.stderr),
    }
}

fn redact_args(args: &[&str]) -> Vec<String> {
    args.iter()
        .map(|argument| {
            if Path::new(argument).is_absolute() {
                "[absolute-path]".to_owned()
            } else {
                redact_text(argument)
            }
        })
        .collect()
}

fn redact_stderr(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    redact_text(text.trim())
}

fn redact_text(text: &str) -> String {
    let printable: String = text
        .chars()
        .map(|character| {
            if character == '\n' || character == '\r' || character == '\t' || !character.is_control()
            {
                character
            } else {
                '�'
            }
        })
        .collect();
    let without_urls = URL_CREDENTIALS.replace_all(&printable, "$1[redacted]@");
    NAMED_SECRET
        .replace_all(&without_urls, "$1[redacted]")
        .into_owned()
}

fn read_bounded<R: Read>(mut reader: R, limit: usize) -> std::io::Result<BoundedCapture> {
    let mut bytes = Vec::with_capacity(limit.min(CAPTURE_BUFFER_BYTES));
    let mut buffer = [0_u8; CAPTURE_BUFFER_BYTES];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(bytes.len());
        let keep = remaining.min(read);
        bytes.extend_from_slice(&buffer[..keep]);
        truncated |= keep < read;
    }
    Ok(BoundedCapture { bytes, truncated })
}

fn join_capture(
    handle: thread::JoinHandle<std::io::Result<BoundedCapture>>,
    stream: &str,
) -> Result<BoundedCapture, WorktreeError> {
    handle
        .join()
        .map_err(|_| WorktreeError::Other(anyhow!("git {stream} reader panicked")))?
        .map_err(|error| WorktreeError::Other(anyhow!("reading git {stream}: {error}")))
}

fn terminate_child_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        if let Ok(pid) = i32::try_from(child.id()) {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
    let _ = child.kill();
}

fn create_dir_chain_no_symlink(path: &Path) -> Result<(), WorktreeError> {
    let absolute = absolute_path(path)?;
    let mut current = PathBuf::new();
    for component in absolute.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(WorktreeError::Symlink);
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(WorktreeError::Other(anyhow!(
                    "path component is not a directory"
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {
                fs::create_dir(&current).map_err(|create_error| {
                    WorktreeError::Other(anyhow!("creating directory: {create_error}"))
                })?;
            }
            Err(error) => {
                return Err(WorktreeError::Other(anyhow!(
                    "inspecting path component: {error}"
                )));
            }
        }
    }
    Ok(())
}

fn reject_symlink_components(path: &Path) -> Result<(), WorktreeError> {
    let absolute = absolute_path(path)?;
    let mut current = PathBuf::new();
    for component in absolute.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(WorktreeError::Symlink);
            }
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => break,
            Err(error) => {
                return Err(WorktreeError::Other(anyhow!(
                    "inspecting path component: {error}"
                )));
            }
        }
    }
    Ok(())
}

fn absolute_path(path: &Path) -> Result<PathBuf, WorktreeError> {
    if path.is_absolute() {
        Ok(normalize_lexically(path))
    } else {
        let cwd = std::env::current_dir().map_err(|error| WorktreeError::Other(error.into()))?;
        Ok(normalize_lexically(&cwd.join(path)))
    }
}

fn normalize_lexically(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::Path;
    use std::process::{Command, Output};
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt;

    use super::*;

    #[test]
    fn validate_workflow_id_accepts_safe_ids_and_rejects_dangerous_ones() {
        // Accepted: alphanumeric-first, only [alnum - _ .], no "..", no
        // trailing dot, no ".lock" suffix, bounded to 128 bytes.
        for valid in [
            "abc", "a1", "a-b", "a_b", "a.b", "workflow-1_task.2", "A-Z_09",
            &"a".repeat(128),
        ] {
            assert!(
                validate_workflow_id(valid).is_ok(),
        "valid workflow id rejected: {valid:?}"
            );
        }
        // Rejected: empty, non-alphanumeric first byte, path separators,
        // whitespace, ".." traversal, trailing dot, ".lock" suffix, oversize,
        // and disallowed punctuation.
        for invalid in [
            "", "-abc", ".abc", "a/b", "a b", "a..b", "abc.", "abc.lock",
            "a$b", "a#b",
        ] {
            assert!(
                validate_workflow_id(invalid).is_err(),
                "invalid workflow id accepted: {invalid:?}"
            );
        }
        // 129 bytes is one past the limit.
        assert!(
            validate_workflow_id(&"a".repeat(129)).is_err(),
            "workflow id over 128 bytes must be rejected"
        );
        // The error carries the offending id for actionable diagnostics.
        let err = validate_workflow_id("bad/id").expect_err("slash rejected");
        match &err {
            WorktreeError::InvalidWorkflowId(id) => assert_eq!(id, "bad/id"),
            other => panic!("expected InvalidWorkflowId, got {other:?}"),
        }
    }

    #[test]
    fn redact_text_strips_url_credentials_named_secrets_and_control_chars() {
        // URL credentials are redacted in-place.
        assert_eq!(
            redact_text("clone https://user:pass@host.example/repo.git"),
            "clone https://[redacted]@host.example/repo.git"
        );
        // Named secret shapes are redacted.
        let secret = ["sk-", "secret-value-", "12345678"].concat();
        assert_eq!(redact_text(&format!("token={secret}")), "token=[redacted]");
        assert_eq!(
            redact_text("password=hunter2"),
            "password=[redacted]"
        );
        // Control bytes (other than newline/tab/cr) become the replacement char.
        assert_eq!(redact_text("a\x00\x01b"), "a\u{fffd}\u{fffd}b");
        // Newline/tab/cr survive the control-char pass.
        assert_eq!(redact_text("a\nb\tc"), "a\nb\tc");
        // Plain text is untouched.
        assert_eq!(redact_text("just a normal message"), "just a normal message");
    }

    #[test]
    fn redact_args_redacts_absolute_paths_and_passes_relative_through_redact() {
        let dir = tempfile::tempdir().expect("repository parent");
        let absolute_path = dir.path().join("secret/repo").to_string_lossy().into_owned();
        let secret = ["sk-", "leaked-", "1234567890abcdef"].concat();
        let token_arg = format!("token={secret}");
        let redacted = redact_args(&[&absolute_path, "clone", &token_arg]);
        assert_eq!(redacted[0], "[absolute-path]", "absolute path must be redacted");
        assert_eq!(redacted[1], "clone", "plain arg survives");
        assert_eq!(
            redacted[2], "token=[redacted]",
            "secret in a relative arg must still be redacted"
        );
    }

    #[test]
    fn redact_stderr_decodes_and_redacts_secrets() {
        let secret = ["sk-", "abc1234567890", "leak"].concat();
        let stderr = format!("fatal: could not read token={secret}\n");
        let redacted = redact_stderr(stderr.as_bytes());
        assert!(!redacted.contains(&secret), "secret must be redacted");
        assert!(redacted.contains("[redacted]"));
        assert!(redacted.contains("fatal"));
    }

    #[test]
    fn reject_if_inside_blocks_equal_nested_and_allows_disjoint() {
        let source = Path::new("/var/src/repo");
        // Equal -> blocked.
        assert!(reject_if_inside(source, source).is_err());
        // Nested under source -> blocked.
        assert!(reject_if_inside(Path::new("/var/src/repo/sub"), source).is_err());
        // Disjoint -> allowed.
        assert!(reject_if_inside(Path::new("/var/worktrees"), source).is_ok());
        // A sibling that does not start with the source path -> allowed.
        assert!(reject_if_inside(Path::new("/var/src/other"), source).is_ok());
    }

    #[test]
    fn validate_managed_branch_accepts_base_and_hex_suffix_rejects_foreign() {
        let id = "wf-abc";
        let base = format!("{WORKFLOW_BRANCH_PREFIX}{id}");
        // Exact base branch name -> accepted.
        assert!(validate_managed_branch(id, &base).is_ok());
        // base-<12 hex chars> -> accepted (the allocated branch shape).
        assert!(validate_managed_branch(id, &format!("{base}-0123456789ab")).is_ok());
        assert!(validate_managed_branch(id, &format!("{base}-abcdefABCDEF")).is_ok());
        // Wrong suffix lengths / non-hex -> rejected.
        assert!(validate_managed_branch(id, &format!("{base}-0123456789a")).is_err());
        assert!(validate_managed_branch(id, &format!("{base}-zzzzzzzzzzzz")).is_err());
        // A foreign branch (different workflow id) -> rejected.
        assert!(validate_managed_branch(id, &format!("{WORKFLOW_BRANCH_PREFIX}other-wf")).is_err());
        // An invalid workflow id is rejected before the branch is inspected.
        assert!(validate_managed_branch("bad/id", &base).is_err());
    }

    #[test]
    fn collect_nul_paths_splits_on_nul_and_skips_empty_segments() {
        let mut files = BTreeSet::new();
        collect_nul_paths(&mut files, "src/a.rs\0src/b.rs\0README.md");
        assert_eq!(
            files,
            BTreeSet::from(["src/a.rs".to_owned(), "src/b.rs".to_owned(), "README.md".to_owned()]
                .into_iter()
                .collect::<BTreeSet<_>>())
        );

        let mut empty_segments = BTreeSet::new();
        collect_nul_paths(&mut empty_segments, "a.rs\0\0b.rs\0");
        assert_eq!(
            empty_segments,
            ["a.rs".to_owned(), "b.rs".to_owned()].into_iter().collect::<BTreeSet<_>>(),
            "empty NUL segments must be skipped, not stored as empty strings"
        );

        let mut nothing = BTreeSet::new();
        collect_nul_paths(&mut nothing, "");
        assert!(nothing.is_empty(), "empty input produces no paths");
    }

    #[cfg(unix)]
    #[test]
    fn command_failed_redacts_args_and_stderr_in_the_error() {
        let secret = ["sk-", "leaked-", "1234567890abcdef"].concat();
        let output = Output {
            status: std::process::ExitStatus::from_raw(1 << 8),
            stdout: Vec::new(),
            stderr: format!("fatal: token={secret}\n").into_bytes(),
        };
        let dir = tempfile::tempdir().expect("repository parent");
        let absolute_path = dir.path().join("repo").to_string_lossy().into_owned();
        let short_token = ["token=sk-", "leaked"].concat();
        let error = command_failed(&[&absolute_path, "fetch", &short_token], &output);
        let WorktreeError::CommandFailed { status, args, stderr } = error else {
            panic!("expected CommandFailed, got {error:?}");
        };
        assert_eq!(status, 1);
        // Absolute path redacted, secret redacted, plain arg survives.
        assert_eq!(args[0], "[absolute-path]");
        assert_eq!(args[1], "fetch");
        assert_eq!(args[2], "token=[redacted]");
        assert!(!stderr.contains(&secret), "stderr secret must be redacted");
        assert!(stderr.contains("[redacted]"));
    }
}
