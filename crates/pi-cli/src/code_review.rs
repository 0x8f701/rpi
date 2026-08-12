//! Bounded git diff acquisition and unified-diff model for `/code-review`.
//!
//! Captures a single coherent HEAD→working-tree snapshot (tracked staged +
//! unstaged changes) via fixed argv, parses it into typed structures, and never
//! mutates the repository.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

/// Hard cap on combined git stdout for the review snapshot.
pub const MAX_DIFF_BYTES: usize = 2 * 1024 * 1024;
/// Hard cap on the changed-file catalog (`git diff --name-status -z`) that
/// backfills files a truncated combined patch could not carry. A catalog past
/// this bound means the change set itself is beyond reviewable size: the
/// snapshot fails closed instead of silently omitting files.
pub const MAX_CATALOG_BYTES: usize = 2 * 1024 * 1024;
/// Soft cap on rendered lines per file (binary/oversize still shown as markers).
pub const MAX_FILE_RENDER_LINES: usize = 4_000;
const GIT_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_GIT_METADATA_BYTES: usize = 64 * 1024;
const MAX_SHARED_INDEX_FILES: usize = 128;

/// How a path changed relative to HEAD.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Deleted,
    Modified,
    Renamed,
    Copied,
    Binary,
    Unknown,
}

impl FileStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Added => "A",
            Self::Deleted => "D",
            Self::Modified => "M",
            Self::Renamed => "R",
            Self::Copied => "C",
            Self::Binary => "B",
            Self::Unknown => "?",
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Deleted => "deleted",
            Self::Modified => "modified",
            Self::Renamed => "renamed",
            Self::Copied => "copied",
            Self::Binary => "binary",
            Self::Unknown => "changed",
        }
    }
}

/// One line inside a unified diff hunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffLineKind {
    Context,
    Addition,
    Deletion,
    Meta,
}

/// A single displayable diff line with optional old/new numbers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
    pub text: String,
}

/// One unified-diff hunk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffHunk {
    pub header: String,
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
    pub lines: Vec<DiffLine>,
}

impl DiffHunk {
    /// Reconstruct the exact display-safe unified hunk supplied to reviewers.
    #[must_use]
    pub fn unified_diff(&self) -> String {
        let mut diff = String::from(&self.header);
        for line in &self.lines {
            diff.push('\n');
            match line.kind {
                DiffLineKind::Context => diff.push(' '),
                DiffLineKind::Addition => diff.push('+'),
                DiffLineKind::Deletion => diff.push('-'),
                DiffLineKind::Meta => {}
            }
            diff.push_str(&line.text);
        }
        diff
    }
}

/// One changed file in the review snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffFile {
    pub path: String,
    pub previous_path: Option<String>,
    pub status: FileStatus,
    pub binary: bool,
    pub insertions: usize,
    pub deletions: usize,
    pub hunks: Vec<DiffHunk>,
    /// True when the file body was truncated for rendering safety.
    pub truncated: bool,
    pub message: Option<String>,
}

impl DiffFile {
    #[must_use]
    pub fn display_path(&self) -> &str {
        &self.path
    }

    #[must_use]
    pub fn rendered_lines(&self) -> Vec<&DiffLine> {
        let mut out = Vec::new();
        for hunk in &self.hunks {
            for line in &hunk.lines {
                out.push(line);
                if out.len() >= MAX_FILE_RENDER_LINES {
                    return out;
                }
            }
        }
        out
    }
}

/// Source and target rendered by the code-review page.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ReviewScope {
    #[default]
    WorkingTree,
    Revisions { from: String, to: String },
}

impl ReviewScope {
    pub fn parse(argument: Option<&str>) -> Result<Self, String> {
        let revisions = argument
            .into_iter()
            .flat_map(str::split_whitespace)
            .collect::<Vec<_>>();
        match revisions.as_slice() {
            [] => Ok(Self::WorkingTree),
            [from, to] => Ok(Self::Revisions {
                from: (*from).to_owned(),
                to: (*to).to_owned(),
            }),
            _ => Err("Usage: /code-review [<from> <to>]".to_owned()),
        }
    }

    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::WorkingTree => "HEAD → working tree".to_owned(),
            Self::Revisions { from, to } => {
                format!("{} → {}", sanitize_display(from), sanitize_display(to))
            }
        }
    }

    fn identity_hint(&self) -> String {
        match self {
            Self::WorkingTree => "working-tree".to_owned(),
            Self::Revisions { from, to } => format!("revisions\0{from}\0{to}"),
        }
    }
}

/// Full review snapshot for a working directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewSnapshot {
    pub root: PathBuf,
    pub scope: ReviewScope,
    /// Stable digest of repository identity, comparison endpoints, and diff bytes.
    pub snapshot_id: String,
    pub files: Vec<DiffFile>,
    pub truncated: bool,
    pub error: Option<String>,
}

impl ReviewSnapshot {
    #[must_use]
    pub fn empty_with_error(root: PathBuf, error: impl Into<String>) -> Self {
        Self::empty_for_scope_error(root, ReviewScope::WorkingTree, error)
    }

    #[must_use]
    pub fn empty_for_scope_error(
        root: PathBuf,
        scope: ReviewScope,
        error: impl Into<String>,
    ) -> Self {
        let error = error.into();
        let identity_hint = scope.identity_hint();
        let snapshot_id = review_snapshot_identity(
            &root,
            identity_hint.as_bytes(),
            error.as_bytes(),
            false,
            None,
        );
        Self {
            root,
            scope,
            snapshot_id,
            files: Vec::new(),
            truncated: false,
            error: Some(error),
        }
    }

    #[must_use]
    pub fn comparison_label(&self) -> String {
        self.scope.label()
    }

    #[must_use]
    pub fn total_insertions(&self) -> usize {
        self.files.iter().map(|f| f.insertions).sum()
    }

    #[must_use]
    pub fn total_deletions(&self) -> usize {
        self.files.iter().map(|f| f.deletions).sum()
    }

    /// Stable identity for one hunk within this captured snapshot.
    #[must_use]
    pub fn hunk_identity(&self, file: &DiffFile, hunk: &DiffHunk) -> HunkIdentity {
        HunkIdentity::new(&self.snapshot_id, &file.path, hunk)
    }
}

/// Stable key for a review thread. Snapshot identity prevents an old answer
/// from being attached to a newer capture; the content digest permits exact
/// hunk migration when an unrelated part of the snapshot changes.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HunkIdentity {
    pub snapshot_id: String,
    pub path: String,
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
    pub content_hash: String,
}

impl HunkIdentity {
    #[must_use]
    pub fn new(snapshot_id: &str, path: &str, hunk: &DiffHunk) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(hunk.unified_diff().as_bytes());
        Self {
            snapshot_id: snapshot_id.to_owned(),
            path: path.to_owned(),
            old_start: hunk.old_start,
            old_count: hunk.old_count,
            new_start: hunk.new_start,
            new_count: hunk.new_count,
            content_hash: format!("{:x}", hasher.finalize()),
        }
    }

    #[must_use]
    pub fn matches_across_snapshots(&self, other: &Self) -> bool {
        self.path == other.path
            && self.old_start == other.old_start
            && self.old_count == other.old_count
            && self.new_start == other.new_start
            && self.new_count == other.new_count
            && self.content_hash == other.content_hash
    }
}

/// Stable snapshot identity. `catalog` is the raw changed-file catalog bytes
/// (`git diff --name-status -z` output); hashing it keeps the stale guard
/// sensitive to files beyond a truncated patch's cut point — otherwise adding
/// or renaming a file past the 2 MiB window would reuse an old snapshot id
/// even though the placeholder set changed. `None` is used only for error and
/// empty snapshots that never acquired a catalog.
fn review_snapshot_identity(
    root: &Path,
    comparison: &[u8],
    diff: &[u8],
    truncated: bool,
    catalog: Option<&[u8]>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(root.to_string_lossy().as_bytes());
    hasher.update([0]);
    hasher.update(comparison);
    hasher.update([0]);
    hasher.update(diff);
    match catalog {
        Some(bytes) => {
            hasher.update([1]);
            hasher.update(bytes);
        }
        None => hasher.update([0]),
    }
    hasher.update([u8::from(truncated)]);
    format!("{:x}", hasher.finalize())
}

/// Hierarchical node used by the left-hand file tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TreeNodeKind {
    Directory,
    File { file_index: usize },
}

/// One node in the collapsible path tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileTreeNode {
    pub id: String,
    pub name: String,
    pub path: String,
    pub kind: TreeNodeKind,
    pub depth: usize,
    pub children: Vec<usize>,
    pub status: Option<FileStatus>,
    pub insertions: usize,
    pub deletions: usize,
}

/// Collapsible file tree over a [`ReviewSnapshot`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileTree {
    pub nodes: Vec<FileTreeNode>,
    pub roots: Vec<usize>,
    pub collapsed: BTreeSet<String>,
}

/// One currently visible row in the tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VisibleTreeRow {
    pub node_index: usize,
    pub depth: usize,
    pub expanded: bool,
    pub is_dir: bool,
}

impl FileTree {
    #[must_use]
    pub fn from_snapshot(snapshot: &ReviewSnapshot) -> Self {
        let mut nodes = Vec::new();
        let mut roots = Vec::new();
        let mut dir_index: BTreeMap<String, usize> = BTreeMap::new();

        let ensure_dir = |nodes: &mut Vec<FileTreeNode>,
                          roots: &mut Vec<usize>,
                          dir_index: &mut BTreeMap<String, usize>,
                          dir_path: &str|
         -> usize {
            if let Some(&idx) = dir_index.get(dir_path) {
                return idx;
            }
            let components: Vec<&str> = dir_path
                .split('/')
                .filter(|part| !part.is_empty())
                .collect();
            let mut parent: Option<usize> = None;
            let mut built = String::new();
            for (depth, component) in components.iter().enumerate() {
                if !built.is_empty() {
                    built.push('/');
                }
                built.push_str(component);
                if let Some(&existing) = dir_index.get(&built) {
                    parent = Some(existing);
                    continue;
                }
                let idx = nodes.len();
                nodes.push(FileTreeNode {
                    id: format!("dir:{built}"),
                    name: (*component).to_owned(),
                    path: built.clone(),
                    kind: TreeNodeKind::Directory,
                    depth,
                    children: Vec::new(),
                    status: None,
                    insertions: 0,
                    deletions: 0,
                });
                dir_index.insert(built.clone(), idx);
                if let Some(parent_idx) = parent {
                    nodes[parent_idx].children.push(idx);
                } else {
                    roots.push(idx);
                }
                parent = Some(idx);
            }
            *dir_index.get(dir_path).expect("dir just inserted")
        };

        for (file_index, file) in snapshot.files.iter().enumerate() {
            let rel = normalize_repo_path(&file.path);
            let (parent_path, name) = match rel.rsplit_once('/') {
                Some((parent, name)) if !parent.is_empty() => {
                    (Some(parent.to_owned()), name.to_owned())
                }
                _ => (None, rel.clone()),
            };
            let parent_idx = parent_path
                .as_deref()
                .map(|parent| ensure_dir(&mut nodes, &mut roots, &mut dir_index, parent));
            let depth = parent_path
                .as_ref()
                .map(|p| p.split('/').filter(|s| !s.is_empty()).count())
                .unwrap_or(0);
            let idx = nodes.len();
            nodes.push(FileTreeNode {
                id: format!("file:{rel}"),
                name,
                path: rel.clone(),
                kind: TreeNodeKind::File { file_index },
                depth,
                children: Vec::new(),
                status: Some(file.status),
                insertions: file.insertions,
                deletions: file.deletions,
            });
            if let Some(parent_idx) = parent_idx {
                nodes[parent_idx].children.push(idx);
                let mut walk = Some(parent_idx);
                while let Some(current) = walk {
                    nodes[current].insertions =
                        nodes[current].insertions.saturating_add(file.insertions);
                    nodes[current].deletions =
                        nodes[current].deletions.saturating_add(file.deletions);
                    let parent_path = nodes[current]
                        .path
                        .rsplit_once('/')
                        .map(|(parent, _)| parent.to_owned());
                    walk = parent_path.and_then(|p| dir_index.get(&p).copied());
                }
            } else {
                roots.push(idx);
            }
        }

        let order_keys: Vec<(bool, String)> = nodes
            .iter()
            .map(|n| {
                let is_file = matches!(n.kind, TreeNodeKind::File { .. });
                (is_file, n.name.to_ascii_lowercase())
            })
            .collect();
        for node in &mut nodes {
            node.children
                .sort_by(|&a, &b| order_keys[a].cmp(&order_keys[b]));
        }
        roots.sort_by(|&a, &b| order_keys[a].cmp(&order_keys[b]));

        Self {
            nodes,
            roots,
            collapsed: BTreeSet::new(),
        }
    }

    #[must_use]
    pub fn is_collapsed(&self, node_index: usize) -> bool {
        self.nodes
            .get(node_index)
            .is_some_and(|node| self.collapsed.contains(&node.id))
    }

    pub fn toggle_collapse(&mut self, node_index: usize) {
        let Some(node) = self.nodes.get(node_index) else {
            return;
        };
        if !matches!(node.kind, TreeNodeKind::Directory) {
            return;
        }
        let id = node.id.clone();
        if !self.collapsed.remove(&id) {
            self.collapsed.insert(id);
        }
    }

    pub fn set_collapsed(&mut self, node_index: usize, collapsed: bool) {
        let Some(node) = self.nodes.get(node_index) else {
            return;
        };
        if !matches!(node.kind, TreeNodeKind::Directory) {
            return;
        }
        if collapsed {
            self.collapsed.insert(node.id.clone());
        } else {
            self.collapsed.remove(&node.id);
        }
    }

    #[must_use]
    pub fn visible_rows(&self) -> Vec<VisibleTreeRow> {
        let mut rows = Vec::new();
        fn walk(tree: &FileTree, indices: &[usize], rows: &mut Vec<VisibleTreeRow>) {
            for &idx in indices {
                let Some(node) = tree.nodes.get(idx) else {
                    continue;
                };
                let is_dir = matches!(node.kind, TreeNodeKind::Directory);
                let expanded = is_dir && !tree.collapsed.contains(&node.id);
                rows.push(VisibleTreeRow {
                    node_index: idx,
                    depth: node.depth,
                    expanded,
                    is_dir,
                });
                if expanded {
                    walk(tree, &node.children, rows);
                }
            }
        }
        walk(self, &self.roots, &mut rows);
        rows
    }

    #[must_use]
    pub fn first_file_visible_index(&self) -> Option<usize> {
        self.visible_rows()
            .into_iter()
            .position(|row| matches!(self.nodes[row.node_index].kind, TreeNodeKind::File { .. }))
    }

    #[must_use]
    pub fn file_index_at_visible(&self, visible_index: usize) -> Option<usize> {
        let row = self.visible_rows().get(visible_index).copied()?;
        match self.nodes.get(row.node_index).map(|n| &n.kind) {
            Some(TreeNodeKind::File { file_index }) => Some(*file_index),
            _ => None,
        }
    }
}

/// Load HEAD→working-tree diff for `cwd` (must be inside a git work tree).
#[must_use]
pub fn load_review_snapshot(cwd: &Path) -> ReviewSnapshot {
    load_review_snapshot_for(cwd, ReviewScope::WorkingTree)
}

/// Load the selected working-tree or two-revision comparison for `cwd`.
#[must_use]
pub fn load_review_snapshot_for(cwd: &Path, scope: ReviewScope) -> ReviewSnapshot {
    let repository = match RepositoryLayout::discover(cwd) {
        Ok(repository) => repository,
        Err(error) => {
            return ReviewSnapshot::empty_for_scope_error(
                cwd.to_path_buf(),
                scope,
                format_git_error("not a git repository", &error),
            );
        }
    };
    let root = repository.root.clone();
    if let Err(error) = repository_attributes_are_safe(&repository) {
        return ReviewSnapshot::empty_for_scope_error(root, scope, error);
    }

    let head_path = repository.git_dir.join("HEAD");
    let head_reference = match read_small_file(&head_path, MAX_GIT_METADATA_BYTES)
        .and_then(|bytes| {
            String::from_utf8(bytes).map_err(|_| "git HEAD is not valid UTF-8".to_owned())
        }) {
        Ok(head) => strip_output_line_ending(&head).to_owned(),
        Err(error) => return ReviewSnapshot::empty_for_scope_error(root, scope, error),
    };
    let object_format = match detect_object_format(&repository) {
        Ok(format) => format,
        Err(error) => return ReviewSnapshot::empty_for_scope_error(root, scope, error),
    };

    let sandbox = match GitSandbox::create(&repository, &object_format, &head_reference) {
        Ok(sandbox) => sandbox,
        Err(error) => return ReviewSnapshot::empty_for_scope_error(root, scope, error),
    };
    match scope {
        ReviewScope::WorkingTree => load_working_tree_snapshot(root, sandbox),
        ReviewScope::Revisions { from, to } => {
            load_revision_snapshot(root, sandbox, from, to)
        }
    }
}

fn load_revision_snapshot(
    root: PathBuf,
    sandbox: GitSandbox,
    from: String,
    to: String,
) -> ReviewSnapshot {
    let scope = ReviewScope::Revisions {
        from: from.clone(),
        to: to.clone(),
    };
    let from_commit = match resolve_review_commit(&root, &from, "source") {
        Ok(commit) => commit,
        Err(error) => return ReviewSnapshot::empty_for_scope_error(root, scope, error),
    };
    let to_commit = match resolve_review_commit(&root, &to, "target") {
        Ok(commit) => commit,
        Err(error) => return ReviewSnapshot::empty_for_scope_error(root, scope, error),
    };
    let comparison = format!("revisions\0{from_commit}\0{to_commit}");
    let isolated = sandbox.environment(&root);

    // Changed-file catalog first: a complete bounded list of what the
    // comparison changes, used to backfill files a truncated combined patch
    // could not carry. Diff options mirror the patch below so rename pairing
    // is identical. A catalog past its bound fails closed instead of silently
    // omitting files.
    let catalog_args = [
        "--literal-pathspecs",
        "diff",
        "--name-status",
        "-z",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
        "--default-prefix",
        "--no-relative",
        "--find-renames",
        "--histogram",
        "-O",
        null_device(),
        from_commit.as_str(),
        to_commit.as_str(),
        "--",
    ];
    let catalog_output = match run_git_bounded(&root, &catalog_args, MAX_CATALOG_BYTES, Some(isolated)) {
        Ok(output) if output.error.is_none() && !output.truncated => output,
        Ok(output) => {
            let error = output
                .error
                .unwrap_or_else(|| "changed file catalog exceeded the size limit".to_owned());
            return ReviewSnapshot::empty_for_scope_error(root, scope, error);
        }
        Err(error) => return ReviewSnapshot::empty_for_scope_error(root, scope, error),
    };

    let args = [
        "--literal-pathspecs",
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
        "--default-prefix",
        "--no-relative",
        "--find-renames",
        "--histogram",
        "-O",
        null_device(),
        from_commit.as_str(),
        to_commit.as_str(),
        "--",
    ];
    let output = match run_git_bounded(&root, &args, MAX_DIFF_BYTES, Some(isolated)) {
        Ok(output) => output,
        Err(error) => return ReviewSnapshot::empty_for_scope_error(root, scope, error),
    };
    snapshot_from_diff_output(root, scope, comparison.as_bytes(), output, Some(&catalog_output))
}

fn resolve_review_commit(root: &Path, revision: &str, label: &str) -> Result<String, String> {
    let commit = format!("{revision}^{{commit}}");
    git_rev_parse(root, &["--verify", "--end-of-options", &commit], None)
        .map(|output| strip_output_line_ending(&output).to_owned())
        .map_err(|_| format!("invalid {label} revision; expected a commit, branch, or tag"))
}

fn load_working_tree_snapshot(root: PathBuf, sandbox: GitSandbox) -> ReviewSnapshot {
    let scope = ReviewScope::WorkingTree;
    let isolated = sandbox.environment(&root);
    let head = match git_rev_parse(&root, &["--verify", "HEAD"], Some(isolated)) {
        Ok(head) => strip_output_line_ending(&head).to_owned(),
        Err(error) => {
            return ReviewSnapshot::empty_for_scope_error(
                root,
                scope,
                format_git_error("repository has no commits yet (unborn HEAD)", &error),
            );
        }
    };

    if !sandbox.index_path().is_file() {
        let args = ["read-tree", head.as_str()];
        match run_git_bounded(&root, &args, MAX_GIT_METADATA_BYTES, Some(isolated)) {
            Ok(output) if output.error.is_none() && !output.truncated => {}
            Ok(output) => {
                let error = output.error.unwrap_or_else(|| {
                    "git read-tree output exceeded the metadata limit".to_owned()
                });
                return ReviewSnapshot::empty_for_scope_error(root, scope, error);
            }
            Err(error) => return ReviewSnapshot::empty_for_scope_error(root, scope, error),
        }
    }

    // Capture only paths that currently differ from HEAD before staging the
    // disposable index. This output scales with the review instead of the repo.
    let changed_args = [
        "diff",
        "--name-only",
        "-z",
        "--no-ext-diff",
        "--no-textconv",
        "--no-renames",
        head.as_str(),
        "--",
    ];
    let changed_output = match run_git_bounded(&root, &changed_args, MAX_DIFF_BYTES, Some(isolated)) {
        Ok(output) if output.error.is_none() && !output.truncated => output,
        Ok(output) => {
            let error = output
                .error
                .unwrap_or_else(|| "changed path list exceeded the size limit".to_owned());
            return ReviewSnapshot::empty_for_scope_error(root, scope, error);
        }
        Err(error) => return ReviewSnapshot::empty_for_scope_error(root, scope, error),
    };
    let mut reviewable_paths = index_path_set(&changed_output.stdout);
    let mut review_path_args = nul_path_args(&changed_output.stdout)
        .into_iter()
        .collect::<BTreeSet<_>>();

    // Stage the full working tree in the disposable index so Git can discover
    // working-tree renames. The final patch remains path-limited below.
    let add_args = ["add", "-A", "--", "."];
    match run_git_bounded(&root, &add_args, MAX_GIT_METADATA_BYTES, Some(isolated)) {
        Ok(output) if output.error.is_none() && !output.truncated => {}
        Ok(output) => {
            let error = output
                .error
                .unwrap_or_else(|| "git add output exceeded the metadata limit".to_owned());
            return ReviewSnapshot::empty_for_scope_error(root, scope, error);
        }
        Err(error) => return ReviewSnapshot::empty_for_scope_error(root, scope, error),
    }

    // Rename-aware changed-file catalog: the same `--name-status -z` output
    // both extends the review pathspecs with rename destinations and backfills
    // files a truncated combined patch could not carry. The diff options
    // mirror the final patch so rename pairing is identical between the
    // catalog and the parsed diff. A catalog past its bound fails closed
    // instead of silently omitting files.
    let rename_args = [
        "diff",
        "--cached",
        "--name-status",
        "-z",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
        "--default-prefix",
        "--no-relative",
        "--find-renames",
        "--histogram",
        head.as_str(),
        "--",
    ];
    let rename_output = match run_git_bounded(&root, &rename_args, MAX_CATALOG_BYTES, Some(isolated)) {
        Ok(output) if output.error.is_none() && !output.truncated => output,
        Ok(output) => {
            let error = output
                .error
                .unwrap_or_else(|| "changed file catalog exceeded the size limit".to_owned());
            return ReviewSnapshot::empty_for_scope_error(root, scope, error);
        }
        Err(error) => return ReviewSnapshot::empty_for_scope_error(root, scope, error),
    };
    let catalog = name_status_catalog(&rename_output.stdout);
    for destination in rename_destination_paths(&rename_output.stdout) {
        reviewable_paths.insert(normalize_repo_path(&encode_path_bytes(&destination)));
        review_path_args.insert(os_string_from_git_path(destination));
    }

    let comparison = format!("working-tree\0{head}");
    if review_path_args.is_empty() {
        return ReviewSnapshot {
            root: root.clone(),
            scope,
            snapshot_id: review_snapshot_identity(&root, comparison.as_bytes(), &[], false, None),
            files: Vec::new(),
            truncated: false,
            error: None,
        };
    }

    // Object-to-object diff only: no worktree conversion filters are consulted.
    let mut args = [
        "--literal-pathspecs",
        "diff",
        "--cached",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
        "--default-prefix",
        "--no-relative",
        "--find-renames",
        "--histogram",
        head.as_str(),
        "-O",
        null_device(),
        "--",
    ]
    .into_iter()
    .map(OsString::from)
    .collect::<Vec<_>>();
    args.extend(review_path_args);
    let output = match run_git_bounded_os(&root, &args, MAX_DIFF_BYTES, Some(isolated)) {
        Ok(output) => output,
        Err(error) => return ReviewSnapshot::empty_for_scope_error(root, scope, error),
    };

    let mut snapshot =
        snapshot_from_diff_output(root, scope, comparison.as_bytes(), output, Some(&rename_output));
    // Synthetic untracked additions staged only for rename discovery never
    // enter the path-limited patch, so their placeholder entries (added only
    // when the patch truncated) are dropped here alongside any parsed ghosts.
    snapshot.files.retain(|file| {
        file.status == FileStatus::Renamed || reviewable_paths.contains(&file.path)
    });
    snapshot
}

fn snapshot_from_diff_output(
    root: PathBuf,
    scope: ReviewScope,
    comparison: &[u8],
    output: GitOutput,
    catalog_output: Option<&GitOutput>,
) -> ReviewSnapshot {
    if let Some(error) = output.error {
        return ReviewSnapshot::empty_for_scope_error(root, scope, error);
    }

    let catalog = catalog_output.map(|output| name_status_catalog(&output.stdout));
    let catalog_bytes = catalog_output.map(|output| output.stdout.as_slice());
    let snapshot_id =
        review_snapshot_identity(&root, comparison, &output.stdout, output.truncated, catalog_bytes);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut files = parse_unified_diff(&stdout);
    if output.truncated {
        if let Some(last) = files.last_mut() {
            last.truncated = true;
            if last.message.is_none() {
                last.message = Some("diff truncated by size limit".to_owned());
            }
        }
        // Global truncation is recoverable file-body paging, not data loss:
        // every catalogued path the combined patch could not carry is retained
        // as an on-demand placeholder so no changed file silently disappears.
        if let Some(catalog) = catalog {
            merge_catalog_placeholders(&mut files, &catalog);
        }
    }
    ReviewSnapshot {
        root,
        scope,
        snapshot_id,
        files,
        truncated: output.truncated,
        error: None,
    }
}

fn nul_path_args(stdout: &[u8]) -> Vec<OsString> {
    stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| os_string_from_git_path(entry.to_vec()))
        .collect()
}

fn rename_destination_paths(stdout: &[u8]) -> Vec<Vec<u8>> {
    let fields = stdout
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>();
    let mut destinations = Vec::new();
    let mut index = 0;
    while index < fields.len() {
        let status = fields[index];
        index += 1;
        if index >= fields.len() {
            break;
        }
        if status.first().is_some_and(|prefix| matches!(prefix, b'R' | b'C')) {
            index += 1;
            if index >= fields.len() {
                break;
            }
            destinations.push(fields[index].to_vec());
        }
        index += 1;
    }
    destinations
}

/// Message on on-demand placeholder entries: the file's body was not carried
/// by the truncated combined patch and loads through `code_review_file_diff`.
const PLACEHOLDER_MESSAGE: &str = "diff omitted: combined diff truncated; loaded on demand";

/// One entry in the changed-file catalog: how a destination path changed and
/// where it came from (rename/copy provenance).
#[derive(Clone, Debug, PartialEq, Eq)]
struct CatalogEntry {
    status: FileStatus,
    previous_path: Option<String>,
}

/// Parse `git diff --name-status -z --find-renames` output (NUL-separated
/// `STATUS\0PATH` records, `R/C<score>\0OLD\0NEW` for renames/copies) into a
/// catalog keyed by normalized destination path. Keys round-trip through the
/// same `encode_path_bytes` + `normalize_repo_path` pipeline as parsed diff
/// paths, so non-UTF-8 and backslash/tab paths compare byte-for-byte against
/// parsed [`DiffFile`]s.
fn name_status_catalog(stdout: &[u8]) -> BTreeMap<String, CatalogEntry> {
    let fields = stdout
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>();
    let mut catalog = BTreeMap::new();
    let mut index = 0usize;
    while index < fields.len() {
        let status = fields[index];
        index += 1;
        if index >= fields.len() {
            break;
        }
        if status.first().is_some_and(|prefix| matches!(prefix, b'R' | b'C')) {
            let previous = fields[index];
            index += 1;
            if index >= fields.len() {
                break;
            }
            let destination = fields[index];
            index += 1;
            catalog.insert(
                normalize_repo_path(&encode_path_bytes(destination)),
                CatalogEntry {
                    status: if status.first() == Some(&b'R') {
                        FileStatus::Renamed
                    } else {
                        FileStatus::Copied
                    },
                    previous_path: Some(normalize_repo_path(&encode_path_bytes(previous))),
                },
            );
        } else {
            let destination = fields[index];
            index += 1;
            let status = match status.first().copied() {
                Some(b'A') => FileStatus::Added,
                Some(b'D') => FileStatus::Deleted,
                Some(b'X') => FileStatus::Unknown,
                _ => FileStatus::Modified,
            };
            catalog.insert(
                normalize_repo_path(&encode_path_bytes(destination)),
                CatalogEntry {
                    status,
                    previous_path: None,
                },
            );
        }
    }
    catalog
}

/// Backfill placeholder [`DiffFile`] entries for every catalogued path the
/// truncated combined patch could not carry. Parsed entries are never touched
/// — their hunks and identities stay intact; a path counts as covered when it
/// is a parsed destination or the previous path of a parsed rename/copy, so
/// rename sources never get spurious placeholder entries even if the catalog
/// and the patch paired differently. Placeholders carry empty hunks, the
/// `truncated` marker, and an explicit on-demand message.
fn merge_catalog_placeholders(files: &mut Vec<DiffFile>, catalog: &BTreeMap<String, CatalogEntry>) {
    let covered: BTreeSet<String> = files
        .iter()
        .flat_map(|file| {
            std::iter::once(file.path.clone()).chain(file.previous_path.iter().cloned())
        })
        .collect();
    for (path, entry) in catalog {
        if covered.contains(path) {
            continue;
        }
        files.push(DiffFile {
            path: path.clone(),
            previous_path: entry.previous_path.clone(),
            status: entry.status,
            binary: false,
            insertions: 0,
            deletions: 0,
            hunks: Vec::new(),
            truncated: true,
            message: Some(PLACEHOLDER_MESSAGE.to_owned()),
        });
    }
}

fn os_string_from_git_path(path: Vec<u8>) -> OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(path)
    }
    #[cfg(not(unix))]
    {
        OsString::from(String::from_utf8_lossy(&path).into_owned())
    }
}

/// Parse `git ls-files -z` output (raw NUL-separated repo-relative paths) into a
/// normalized set. `ls-files -z` never C-quotes, so backslash/tab/newline path
/// components round-trip as raw bytes. Each entry is run through the same
/// `encode_path_bytes` + `normalize_repo_path` pipeline the parser applies to
/// decoded diff paths, so non-UTF-8 paths (escaped as `\ooo`) and paths with
/// backslash/tab components compare byte-for-byte against the snapshot.
fn index_path_set(stdout: &[u8]) -> BTreeSet<String> {
    stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| normalize_repo_path(&encode_path_bytes(entry)))
        .collect()
}

fn repository_attributes_are_safe(repository: &RepositoryLayout) -> Result<(), String> {
    for mut directory in repository.root.ancestors().map(Path::to_path_buf) {
        directory.push(".gitattributes");
        check_regular_small_file_if_present(&directory)?;
    }
    for path in [
        repository.git_dir.join("info/attributes"),
        repository.common_dir.join("info/attributes"),
    ] {
        check_regular_small_file_if_present(&path)?;
    }
    Ok(())
}

fn check_regular_small_file_if_present(path: &Path) -> Result<(), String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("reading {}: {error}", path.display())),
    };
    if !metadata.file_type().is_file() {
        return Err(format!(
            "cannot safely review repository with non-regular attributes file {}",
            path.display()
        ));
    }
    let _ = read_small_file(path, MAX_GIT_METADATA_BYTES)?;
    Ok(())
}

#[derive(Debug)]
struct RepositoryLayout {
    root: PathBuf,
    git_dir: PathBuf,
    common_dir: PathBuf,
}

impl RepositoryLayout {
    fn discover(cwd: &Path) -> Result<Self, String> {
        let start = fs::canonicalize(cwd)
            .map_err(|error| format!("resolving working directory: {error}"))?;
        if !start.is_dir() {
            return Err("working directory is not a directory".to_owned());
        }

        for root in start.ancestors() {
            let marker = root.join(".git");
            let metadata = match fs::metadata(&marker) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(format!("reading .git metadata: {error}")),
            };
            let git_dir = if metadata.is_dir() {
                fs::canonicalize(&marker)
                    .map_err(|error| format!("resolving git directory: {error}"))?
            } else if metadata.is_file() {
                let contents = read_small_file(&marker, MAX_GIT_METADATA_BYTES)?;
                let contents = std::str::from_utf8(&contents)
                    .map_err(|_| ".git file is not valid UTF-8".to_owned())?;
                let line = strip_output_line_ending(contents);
                let value = line
                    .strip_prefix("gitdir:")
                    .map(str::trim_start)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| "invalid .git file".to_owned())?;
                let path = Path::new(value);
                let path = if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    root.join(path)
                };
                fs::canonicalize(path)
                    .map_err(|error| format!("resolving git directory: {error}"))?
            } else {
                continue;
            };

            let common_file = git_dir.join("commondir");
            let common_dir = if common_file.is_file() {
                let contents = read_small_file(&common_file, MAX_GIT_METADATA_BYTES)?;
                let contents = std::str::from_utf8(&contents)
                    .map_err(|_| "git commondir is not valid UTF-8".to_owned())?;
                let value = strip_output_line_ending(contents);
                let path = Path::new(value);
                let path = if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    git_dir.join(path)
                };
                fs::canonicalize(path)
                    .map_err(|error| format!("resolving common git directory: {error}"))?
            } else {
                git_dir.clone()
            };

            return Ok(Self {
                root: root.to_path_buf(),
                git_dir,
                common_dir,
            });
        }

        Err("not a git repository".to_owned())
    }
}

fn read_small_file(path: &Path, max: usize) -> Result<Vec<u8>, String> {
    let metadata =
        fs::metadata(path).map_err(|error| format!("reading {}: {error}", path.display()))?;
    if metadata.len() > max as u64 {
        return Err(format!("{} exceeds {max} bytes", path.display()));
    }
    fs::read(path).map_err(|error| format!("reading {}: {error}", path.display()))
}

fn strip_output_line_ending(value: &str) -> &str {
    value
        .strip_suffix("\r\n")
        .or_else(|| value.strip_suffix('\n'))
        .unwrap_or(value)
}

fn detect_object_format(repository: &RepositoryLayout) -> Result<String, String> {
    let config = repository.common_dir.join("config");
    let bytes = read_small_file(&config, MAX_GIT_METADATA_BYTES)?;
    let config = String::from_utf8(bytes).map_err(|_| "git config is not valid UTF-8".to_owned())?;
    let mut section = "";
    let mut repository_format_version = 0u32;
    let mut object_format = None;
    for line in config.lines() {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].trim();
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        if section.eq_ignore_ascii_case("core")
            && name.trim().eq_ignore_ascii_case("repositoryformatversion")
        {
            repository_format_version = value
                .trim()
                .parse()
                .map_err(|_| "invalid git repository format version".to_owned())?;
        } else if section.eq_ignore_ascii_case("extensions")
            && name.trim().eq_ignore_ascii_case("objectformat")
        {
            object_format = Some(value.trim().to_owned());
        }
    }
    let object_format = object_format.unwrap_or_else(|| "sha1".to_owned());
    if repository_format_version > 1 {
        return Err(format!(
            "unsupported git repository format version: {repository_format_version}"
        ));
    }
    match object_format.as_str() {
        "sha1" | "sha256" => Ok(object_format),
        _ => Err(format!("unsupported git object format: {object_format}")),
    }
}

#[derive(Debug)]
struct TemporaryDirectory {
    path: PathBuf,
}

impl TemporaryDirectory {
    fn create() -> Result<Self, String> {
        let path = std::env::temp_dir().join(format!(
            "pi-code-review-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(&path)
            .map_err(|error| format!("creating isolated git directory: {error}"))?;
        Ok(Self { path })
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[derive(Debug)]
struct GitSandbox {
    directory: TemporaryDirectory,
}

impl GitSandbox {
    fn create(
        repository: &RepositoryLayout,
        object_format: &str,
        head_reference: &str,
    ) -> Result<Self, String> {
        if !matches!(object_format, "sha1" | "sha256") {
            return Err(format!("unsupported git object format: {object_format}"));
        }
        let directory = TemporaryDirectory::create()?;
        let git_dir = &directory.path;
        fs::create_dir_all(git_dir.join("objects/info"))
            .map_err(|error| format!("creating isolated object directory: {error}"))?;
        // Git requires the refs namespace even when HEAD is unborn and its
        // target reference does not exist yet.
        fs::create_dir_all(git_dir.join("refs"))
            .map_err(|error| format!("creating isolated refs directory: {error}"))?;

        let objects = fs::canonicalize(repository.common_dir.join("objects"))
            .map_err(|error| format!("resolving git object directory: {error}"))?;
        let objects = objects
            .to_str()
            .ok_or_else(|| "git object directory is not valid UTF-8".to_owned())?;
        if objects.contains(['\n', '\r']) {
            return Err("git object directory contains a line break".to_owned());
        }
        fs::write(
            git_dir.join("objects/info/alternates"),
            format!("{objects}\n"),
        )
        .map_err(|error| format!("writing isolated object alternates: {error}"))?;

        let config = if object_format == "sha256" {
            "[core]\n\trepositoryformatversion = 1\n\tbare = false\n[extensions]\n\tobjectformat = sha256\n"
        } else {
            "[core]\n\trepositoryformatversion = 0\n\tbare = false\n"
        };
        fs::write(git_dir.join("config"), config)
            .map_err(|error| format!("writing isolated git config: {error}"))?;
        fs::write(git_dir.join("HEAD"), format!("{head_reference}\n"))
            .map_err(|error| format!("writing isolated HEAD: {error}"))?;
        if let Some(reference) = head_reference.strip_prefix("ref: ") {
            if reference.contains("..") || reference.starts_with('/') || reference.contains('\\') {
                return Err("invalid HEAD reference".to_owned());
            }
            let source = repository.common_dir.join(reference);
            if source.is_file() {
                let destination = git_dir.join(reference);
                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|error| format!("creating isolated HEAD reference: {error}"))?;
                }
                fs::copy(source, destination)
                    .map_err(|error| format!("copying isolated HEAD reference: {error}"))?;
            } else {
                let packed_refs = repository.common_dir.join("packed-refs");
                if packed_refs.is_file() {
                    fs::copy(packed_refs, git_dir.join("packed-refs"))
                        .map_err(|error| format!("copying packed refs: {error}"))?;
                }
            }
        }

        let source_index = repository.git_dir.join("index");
        if source_index.is_file() {
            fs::copy(&source_index, git_dir.join("index"))
                .map_err(|error| format!("copying git index: {error}"))?;
            copy_shared_indexes(&repository.git_dir, git_dir)?;
            if repository.common_dir != repository.git_dir {
                copy_shared_indexes(&repository.common_dir, git_dir)?;
            }
        }

        Ok(Self { directory })
    }

    fn index_path(&self) -> PathBuf {
        self.directory.path.join("index")
    }

    fn environment<'a>(&'a self, work_tree: &'a Path) -> IsolatedGitEnvironment<'a> {
        IsolatedGitEnvironment {
            git_dir: &self.directory.path,
            work_tree,
        }
    }
}

/// Disposable Git directory with copied HEAD/index and no repository-local
/// executable config. Footer metadata collection uses this so worktree checks
/// cannot invoke clean/process filters even on racily-clean paths.
pub(crate) struct IsolatedGitSandbox {
    sandbox: GitSandbox,
    work_tree: PathBuf,
}

impl IsolatedGitSandbox {
    pub(crate) fn discover(cwd: &Path) -> Result<Self, String> {
        let repository = RepositoryLayout::discover(cwd)?;
        let head = read_small_file(&repository.git_dir.join("HEAD"), MAX_GIT_METADATA_BYTES)?;
        let head = String::from_utf8(head).map_err(|_| "git HEAD is not valid UTF-8".to_owned())?;
        let head = strip_output_line_ending(&head);
        let object_format = detect_object_format(&repository)?;
        let sandbox = GitSandbox::create(&repository, &object_format, head)?;
        Ok(Self {
            sandbox,
            work_tree: repository.root,
        })
    }

    pub(crate) fn work_tree(&self) -> &Path {
        &self.work_tree
    }

    pub(crate) fn environment(&self) -> IsolatedGitEnvironment<'_> {
        self.sandbox.environment(&self.work_tree)
    }

    pub(crate) fn index_path(&self) -> PathBuf {
        self.sandbox.index_path()
    }
}

fn copy_shared_indexes(source: &Path, destination: &Path) -> Result<(), String> {
    let mut copied = 0usize;
    let entries = fs::read_dir(source)
        .map_err(|error| format!("reading git index directory: {error}"))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("reading git index entry: {error}"))?;
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with("sharedindex.") {
            continue;
        }
        let file_type = entry
            .file_type()
            .map_err(|error| format!("reading shared index metadata: {error}"))?;
        if !file_type.is_file() {
            continue;
        }
        copied = copied.saturating_add(1);
        if copied > MAX_SHARED_INDEX_FILES {
            return Err("too many shared git index files".to_owned());
        }
        fs::copy(entry.path(), destination.join(name))
            .map_err(|error| format!("copying shared git index: {error}"))?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub(crate) struct IsolatedGitEnvironment<'a> {
    pub(crate) git_dir: &'a Path,
    pub(crate) work_tree: &'a Path,
}

#[cfg(windows)]
const fn null_device() -> &'static str {
    "NUL"
}

#[cfg(not(windows))]
const fn null_device() -> &'static str {
    "/dev/null"
}

/// Parse a unified diff body into typed files/hunks/lines.
#[must_use]
pub fn parse_unified_diff(input: &str) -> Vec<DiffFile> {
    let mut files = Vec::new();
    let mut current: Option<DiffFile> = None;
    let mut current_hunk: Option<DiffHunk> = None;
    let mut old_line: u32 = 0;
    let mut new_line: u32 = 0;
    let mut pending_old: Option<String> = None;
    let mut pending_new: Option<String> = None;

    let flush_hunk = |file: &mut DiffFile, hunk: &mut Option<DiffHunk>| {
        if let Some(hunk) = hunk.take() {
            file.hunks.push(hunk);
        }
    };
    let flush_file =
        |files: &mut Vec<DiffFile>, file: &mut Option<DiffFile>, hunk: &mut Option<DiffHunk>| {
            if let Some(mut file) = file.take() {
                flush_hunk(&mut file, hunk);
                finalize_file_stats(&mut file);
                files.push(file);
            }
        };

    for raw in input.lines() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);

        if let Some(rest) = line.strip_prefix("diff --git ") {
            flush_file(&mut files, &mut current, &mut current_hunk);
            pending_old = None;
            pending_new = None;
            let (old_path, new_path) = parse_diff_git_paths(rest);
            let path = new_path
                .clone()
                .filter(|p| p != "/dev/null")
                .or_else(|| old_path.clone().filter(|p| p != "/dev/null"))
                .unwrap_or_else(|| "unknown".to_owned());
            let previous = old_path
                .filter(|p| p != "/dev/null" && Some(p.as_str()) != Some(path.as_str()));
            current = Some(DiffFile {
                path: normalize_repo_path(&path),
                previous_path: previous.map(|p| normalize_repo_path(&p)),
                status: FileStatus::Modified,
                binary: false,
                insertions: 0,
                deletions: 0,
                hunks: Vec::new(),
                truncated: false,
                message: None,
            });
            continue;
        }

        if current.is_none() {
            if line.starts_with("--- ") {
                pending_old = Some(strip_diff_path(line));
            } else if line.starts_with("+++ ") {
                pending_new = Some(strip_diff_path(line));
                let old_path = pending_old.clone();
                let new_path = pending_new.clone();
                let path = new_path
                    .clone()
                    .filter(|p| p != "/dev/null")
                    .or_else(|| old_path.clone().filter(|p| p != "/dev/null"))
                    .unwrap_or_else(|| "unknown".to_owned());
                let previous = old_path
                    .filter(|p| p != "/dev/null" && Some(p.as_str()) != Some(path.as_str()));
                let status = match (
                    pending_old.as_deref() == Some("/dev/null"),
                    pending_new.as_deref() == Some("/dev/null"),
                ) {
                    (true, false) => FileStatus::Added,
                    (false, true) => FileStatus::Deleted,
                    _ if previous.is_some() => FileStatus::Renamed,
                    _ => FileStatus::Modified,
                };
                current = Some(DiffFile {
                    path: normalize_repo_path(&path),
                    previous_path: previous.map(|p| normalize_repo_path(&p)),
                    status,
                    binary: false,
                    insertions: 0,
                    deletions: 0,
                    hunks: Vec::new(),
                    truncated: false,
                    message: None,
                });
            }
            continue;
        }

        let file = current.as_mut().expect("current file");

        if line.starts_with("new file mode ") {
            file.status = FileStatus::Added;
            continue;
        }
        if line.starts_with("deleted file mode ") {
            file.status = FileStatus::Deleted;
            continue;
        }
        if line.starts_with("rename from ") {
            file.status = FileStatus::Renamed;
            let from = decode_header_path(line.trim_start_matches("rename from "));
            if !from.is_empty() {
                file.previous_path = Some(normalize_repo_path(&from));
            }
            continue;
        }
        if line.starts_with("rename to ") {
            file.status = FileStatus::Renamed;
            let to = decode_header_path(line.trim_start_matches("rename to "));
            if !to.is_empty() {
                file.path = normalize_repo_path(&to);
            }
            continue;
        }
        if line.starts_with("copy from ") {
            file.status = FileStatus::Copied;
            let from = decode_header_path(line.trim_start_matches("copy from "));
            if !from.is_empty() {
                file.previous_path = Some(normalize_repo_path(&from));
            }
            continue;
        }
        if line.starts_with("copy to ") {
            file.status = FileStatus::Copied;
            let to = decode_header_path(line.trim_start_matches("copy to "));
            if !to.is_empty() {
                file.path = normalize_repo_path(&to);
            }
            continue;
        }
        if line.starts_with("Binary files ") || line.starts_with("GIT binary patch") {
            flush_hunk(file, &mut current_hunk);
            file.binary = true;
            file.status = FileStatus::Binary;
            file.message = Some(sanitize_display(line));
            continue;
        }
        if line.starts_with("--- ") {
            let path = strip_diff_path(line);
            if path == "/dev/null" {
                if file.status == FileStatus::Modified {
                    file.status = FileStatus::Added;
                }
            } else if file.previous_path.is_none()
                && path != file.path
                && file.status != FileStatus::Added
            {
                file.previous_path = Some(normalize_repo_path(&path));
            }
            continue;
        }
        if line.starts_with("+++ ") {
            let path = strip_diff_path(line);
            if path == "/dev/null" {
                if matches!(file.status, FileStatus::Modified | FileStatus::Unknown) {
                    file.status = FileStatus::Deleted;
                }
            } else if path != file.path && file.status != FileStatus::Deleted {
                file.path = normalize_repo_path(&path);
            }
            continue;
        }
        if line.starts_with("@@") {
            flush_hunk(file, &mut current_hunk);
            if let Some((hunk, old_start, new_start)) = parse_hunk_header(line) {
                old_line = old_start;
                new_line = new_start;
                current_hunk = Some(hunk);
            }
            continue;
        }
        if line.starts_with('\\') {
            if let Some(hunk) = current_hunk.as_mut() {
                hunk.lines.push(DiffLine {
                    kind: DiffLineKind::Meta,
                    old_no: None,
                    new_no: None,
                    text: sanitize_display(line),
                });
            }
            continue;
        }

        let Some(hunk) = current_hunk.as_mut() else {
            continue;
        };

        if let Some(text) = line.strip_prefix('+') {
            hunk.lines.push(DiffLine {
                kind: DiffLineKind::Addition,
                old_no: None,
                new_no: Some(new_line),
                text: sanitize_display(text),
            });
            new_line = new_line.saturating_add(1);
        } else if let Some(text) = line.strip_prefix('-') {
            hunk.lines.push(DiffLine {
                kind: DiffLineKind::Deletion,
                old_no: Some(old_line),
                new_no: None,
                text: sanitize_display(text),
            });
            old_line = old_line.saturating_add(1);
        } else if let Some(text) = line.strip_prefix(' ') {
            hunk.lines.push(DiffLine {
                kind: DiffLineKind::Context,
                old_no: Some(old_line),
                new_no: Some(new_line),
                text: sanitize_display(text),
            });
            old_line = old_line.saturating_add(1);
            new_line = new_line.saturating_add(1);
        } else if line.is_empty() {
            hunk.lines.push(DiffLine {
                kind: DiffLineKind::Context,
                old_no: Some(old_line),
                new_no: Some(new_line),
                text: String::new(),
            });
            old_line = old_line.saturating_add(1);
            new_line = new_line.saturating_add(1);
        }
    }

    flush_file(&mut files, &mut current, &mut current_hunk);
    files
}

fn finalize_file_stats(file: &mut DiffFile) {
    let mut insertions = 0usize;
    let mut deletions = 0usize;
    for hunk in &file.hunks {
        for line in &hunk.lines {
            match line.kind {
                DiffLineKind::Addition => insertions += 1,
                DiffLineKind::Deletion => deletions += 1,
                DiffLineKind::Context | DiffLineKind::Meta => {}
            }
        }
    }
    file.insertions = insertions;
    file.deletions = deletions;
    if file.binary {
        file.status = FileStatus::Binary;
    } else if file.previous_path.is_some()
        && matches!(file.status, FileStatus::Modified | FileStatus::Unknown)
    {
        file.status = FileStatus::Renamed;
    }
    if file.hunks.iter().map(|h| h.lines.len()).sum::<usize>() > MAX_FILE_RENDER_LINES {
        file.truncated = true;
    }
}

fn parse_hunk_header(line: &str) -> Option<(DiffHunk, u32, u32)> {
    let rest = line.strip_prefix("@@")?;
    let (ranges, _suffix) = rest.split_once("@@")?;
    let ranges = ranges.trim();
    let mut parts = ranges.split_whitespace();
    let old = parts.next()?.strip_prefix('-')?;
    let new = parts.next()?.strip_prefix('+')?;
    let (old_start, old_count) = parse_range(old)?;
    let (new_start, new_count) = parse_range(new)?;
    Some((
        DiffHunk {
            header: sanitize_display(line),
            old_start,
            old_count,
            new_start,
            new_count,
            lines: Vec::new(),
        },
        old_start,
        new_start,
    ))
}

fn parse_range(spec: &str) -> Option<(u32, u32)> {
    if let Some((start, count)) = spec.split_once(',') {
        Some((start.parse().ok()?, count.parse().ok()?))
    } else {
        Some((spec.parse().ok()?, 1))
    }
}

fn parse_diff_git_paths(rest: &str) -> (Option<String>, Option<String>) {
    let tokens = split_diff_path_tokens(rest);
    if tokens.len() >= 2 {
        return (
            Some(strip_ab_prefix(&tokens[0])),
            Some(strip_ab_prefix(&tokens[1])),
        );
    }
    if tokens.len() == 1 {
        let path = strip_ab_prefix(&tokens[0]);
        return (Some(path.clone()), Some(path));
    }
    (None, None)
}

fn split_diff_path_tokens(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let bytes = input.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        while bytes[index].is_ascii_whitespace() {
            index += 1;
            if index == bytes.len() {
                return tokens;
            }
        }
        if bytes[index] == b'"' {
            let start = index;
            index += 1;
            let mut escaped = false;
            while index < bytes.len() {
                let byte = bytes[index];
                index += 1;
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    break;
                }
            }
            tokens.push(decode_git_path(&input[start..index]));
        } else {
            let start = index;
            while index < bytes.len() && !bytes[index].is_ascii_whitespace() {
                index += 1;
            }
            tokens.push(input[start..index].to_owned());
        }
    }
    tokens
}

fn strip_diff_path(line: &str) -> String {
    let rest = line
        .strip_prefix("--- ")
        .or_else(|| line.strip_prefix("+++ "))
        .unwrap_or(line);
    let path = if rest.starts_with('"') {
        quoted_token(rest).unwrap_or(rest)
    } else {
        rest.split('\t').next().unwrap_or(rest)
    };
    let decoded = decode_git_path(path);
    if decoded == "/dev/null" {
        return decoded;
    }
    strip_ab_prefix(&decoded)
}

fn quoted_token(input: &str) -> Option<&str> {
    let bytes = input.as_bytes();
    if bytes.first().copied()? != b'"' {
        return None;
    }
    let mut escaped = false;
    for (index, byte) in bytes.iter().copied().enumerate().skip(1) {
        if escaped {
            escaped = false;
        } else if byte == b'\\' {
            escaped = true;
        } else if byte == b'"' {
            return Some(&input[..=index]);
        }
    }
    None
}

fn decode_header_path(path: &str) -> String {
    decode_git_path(quoted_token(path).unwrap_or(path))
}

fn strip_ab_prefix(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("a/").or_else(|| path.strip_prefix("b/")) {
        normalize_repo_path(rest)
    } else {
        normalize_repo_path(path)
    }
}

fn decode_git_path(input: &str) -> String {
    let Some(quoted) = input
        .strip_prefix('"')
        .and_then(|input| input.strip_suffix('"'))
    else {
        return input.to_owned();
    };

    let bytes = quoted.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] != b'\\' {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        index += 1;
        let Some(&escaped) = bytes.get(index) else {
            decoded.push(b'\\');
            break;
        };
        index += 1;
        match escaped {
            b'a' => decoded.push(0x07),
            b'b' => decoded.push(0x08),
            b't' => decoded.push(b'\t'),
            b'n' => decoded.push(b'\n'),
            b'v' => decoded.push(0x0b),
            b'f' => decoded.push(0x0c),
            b'r' => decoded.push(b'\r'),
            b'"' => decoded.push(b'"'),
            b'\\' => decoded.push(b'\\'),
            b'0'..=b'7' => {
                let mut value = escaped - b'0';
                for _ in 0..2 {
                    let Some(&digit @ b'0'..=b'7') = bytes.get(index) else {
                        break;
                    };
                    value = value.saturating_mul(8).saturating_add(digit - b'0');
                    index += 1;
                }
                decoded.push(value);
            }
            other => decoded.push(other),
        }
    }
    encode_path_bytes(&decoded)
}

fn encode_path_bytes(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len());
    let mut start = 0usize;
    while start < bytes.len() {
        match std::str::from_utf8(&bytes[start..]) {
            Ok(valid) => {
                output.push_str(valid);
                break;
            }
            Err(error) => {
                let valid_end = start + error.valid_up_to();
                if valid_end > start {
                    output.push_str(std::str::from_utf8(&bytes[start..valid_end]).expect("valid prefix"));
                }
                let invalid_len = error.error_len().unwrap_or(bytes.len() - valid_end);
                for byte in &bytes[valid_end..valid_end + invalid_len] {
                    output.push_str(&format!("\\{:03o}", byte));
                }
                start = valid_end + invalid_len;
            }
        }
    }
    output
}

/// Normalize to a repo-relative Git path without `..` escape.
#[must_use]
pub fn normalize_repo_path(path: &str) -> String {
    let mut parts = Vec::new();
    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                let _ = parts.pop();
            }
            part => parts.push(part),
        }
    }
    if parts.is_empty() {
        ".".to_owned()
    } else {
        parts.join("/")
    }
}

fn sanitize_display(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_control() && ch != '\t' || is_invisible_format_control(ch) {
                ' '
            } else {
                ch
            }
        })
        .collect()
}

const fn is_invisible_format_control(ch: char) -> bool {
    matches!(
        ch,
        '\u{00ad}'
            | '\u{061c}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206f}'
            | '\u{feff}'
    )
}

fn format_git_error(context: &str, detail: &str) -> String {
    let detail = detail
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or(context);
    let detail = sanitize_display(detail);
    if detail.is_empty() {
        context.to_owned()
    } else if detail.contains(context) {
        detail
    } else {
        format!("{context}: {detail}")
    }
}

fn git_rev_parse(
    cwd: &Path,
    args: &[&str],
    isolated: Option<IsolatedGitEnvironment<'_>>,
) -> Result<String, String> {
    let mut full = vec!["rev-parse"];
    full.extend_from_slice(args);
    let output = run_git_bounded(cwd, &full, MAX_GIT_METADATA_BYTES, isolated)?;
    if let Some(error) = output.error {
        return Err(error);
    }
    if output.truncated {
        return Err("git metadata output exceeded the size limit".to_owned());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[derive(Debug)]
pub(crate) struct GitOutput {
    pub(crate) stdout: Vec<u8>,
    pub(crate) truncated: bool,
    pub(crate) error: Option<String>,
}

/// Run git with fixed argv, no shell, bounded output, isolated config, and a
/// process-group timeout so descendants cannot retain output pipes forever.
fn run_git_bounded(
    cwd: &Path,
    args: &[&str],
    max_stdout: usize,
    isolated: Option<IsolatedGitEnvironment<'_>>,
) -> Result<GitOutput, String> {
    let args = args.iter().map(OsString::from).collect::<Vec<_>>();
    run_git_bounded_os(cwd, &args, max_stdout, isolated)
}

fn run_git_bounded_os(
    cwd: &Path,
    args: &[OsString],
    max_stdout: usize,
    isolated: Option<IsolatedGitEnvironment<'_>>,
) -> Result<GitOutput, String> {
    run_git_bounded_os_timeout(cwd, args, max_stdout, isolated, GIT_TIMEOUT)
}

/// `run_git_bounded` with a caller-supplied timeout. `pub(crate)` so the
/// composer footer can collect `git status` with a tight bound off the render
/// path while reusing the same isolated, bounded, process-group-killing runner.
pub(crate) fn run_git_bounded_timeout(
    cwd: &Path,
    args: &[&str],
    max_stdout: usize,
    isolated: Option<IsolatedGitEnvironment<'_>>,
    timeout: Duration,
) -> Result<GitOutput, String> {
    let args = args.iter().map(OsString::from).collect::<Vec<_>>();
    run_git_bounded_os_timeout(cwd, &args, max_stdout, isolated, timeout)
}

fn run_git_bounded_os_timeout(
    cwd: &Path,
    args: &[OsString],
    max_stdout: usize,
    isolated: Option<IsolatedGitEnvironment<'_>>,
    timeout: Duration,
) -> Result<GitOutput, String> {
    let executable = git_executable();
    let mut command = Command::new(executable);
    command
        .args([
            "--no-pager",
            "-c",
            "color.ui=false",
            "-c",
            "core.quotepath=false",
            "-c",
            "diff.external=",
            "-c",
            "diff.mnemonicprefix=false",
            "-c",
            "diff.autoRefreshIndex=false",
        ])
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GC_AUTO", "0")
        .env("LC_ALL", "C")
        .env_remove("GIT_CONFIG")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_VALUE_0")
        .env_remove("GIT_CONFIG_SYSTEM")
        .env_remove("GIT_ATTR_NOSYSTEM")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_DIR")
        .env_remove("GIT_EXTERNAL_DIFF")
        .env_remove("GIT_PAGER")
        .env_remove("GIT_ASKPASS")
        .env_remove("SSH_ASKPASS")
        .env_remove("PAGER")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(isolated) = isolated {
        command
            .env("GIT_DIR", isolated.git_dir)
            .env("GIT_WORK_TREE", isolated.work_tree);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    let mut child = command
        .spawn()
        .map_err(|error| format!("failed to spawn git: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "git stdout pipe unavailable".to_owned())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "git stderr pipe unavailable".to_owned())?;
    let stdout_rx = spawn_bounded_reader(stdout, max_stdout);
    let stderr_rx = spawn_bounded_reader(stderr, MAX_GIT_METADATA_BYTES);

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() >= timeout => {
                terminate_process_tree(&mut child);
                return Err(format!("git timed out after {}s", timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(error) => {
                terminate_process_tree(&mut child);
                return Err(format!("waiting for git failed: {error}"));
            }
        }
    };

    let stdout_result = match receive_reader(stdout_rx, "stdout") {
        Ok(result) => result,
        Err(error) => {
            terminate_process_tree(&mut child);
            return Err(error);
        }
    };
    let stderr_result = match receive_reader(stderr_rx, "stderr") {
        Ok(result) => result,
        Err(error) => {
            terminate_process_tree(&mut child);
            return Err(error);
        }
    };
    let stderr = String::from_utf8_lossy(&stderr_result.0);
    let error = if status.success() {
        None
    } else {
        let message = stderr
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("git command failed");
        Some(sanitize_display(message))
    };

    Ok(GitOutput {
        stdout: stdout_result.0,
        truncated: stdout_result.1,
        error,
    })
}

fn git_executable() -> OsString {
    let Some(path) = std::env::var_os("PATH") else {
        return OsString::from("git");
    };
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(if cfg!(windows) { "git.exe" } else { "git" });
        if candidate.is_file() {
            return candidate.into_os_string();
        }
    }
    OsString::from("git")
}

fn spawn_bounded_reader<R>(
    reader: R,
    max: usize,
) -> Receiver<std::io::Result<(Vec<u8>, bool)>>
where
    R: std::io::Read + Send + 'static,
{
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(read_bounded(reader, max));
    });
    receiver
}

fn receive_reader(
    receiver: Receiver<std::io::Result<(Vec<u8>, bool)>>,
    stream: &str,
) -> Result<(Vec<u8>, bool), String> {
    match receiver.recv_timeout(Duration::from_secs(1)) {
        Ok(result) => result.map_err(|error| format!("reading git {stream}: {error}")),
        Err(RecvTimeoutError::Timeout) => Err(format!("git {stream} remained open after exit")),
        Err(RecvTimeoutError::Disconnected) => Err(format!("git {stream} reader stopped")),
    }
}

fn terminate_process_tree(child: &mut Child) {
    #[cfg(unix)]
    if let Ok(pid) = i32::try_from(child.id()) {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn read_bounded<R: std::io::Read>(mut reader: R, max: usize) -> std::io::Result<(Vec<u8>, bool)> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 8192];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        let remaining = max.saturating_sub(buffer.len());
        if remaining == 0 {
            truncated = true;
            let mut sink = [0_u8; 8192];
            while reader.read(&mut sink)? > 0 {}
            break;
        }
        let take = read.min(remaining);
        buffer.extend_from_slice(&chunk[..take]);
        if take < read {
            truncated = true;
            let mut sink = [0_u8; 8192];
            while reader.read(&mut sink)? > 0 {}
            break;
        }
    }
    Ok((buffer, truncated))
}

// ---------------------------------------------------------------------------
// Bounded single-file diff paging (read-only RPC surface).
//
// The snapshot acquires the whole HEAD→working-tree (or two-revision) diff
// under a 2 MiB total cap, so a single large file can be truncated. Files the
// combined patch could not carry stay listed as on-demand placeholders (see
// `merge_catalog_placeholders`), never silently omitted. The paging RPC
// re-runs the SAME fixed-argv, isolated-sandbox, bounded git diff scoped to
// one file's pathspec (with its rename provenance), parses it under a higher
// per-file byte cap, and serves ordered pages out of an in-memory cache.
// Pathspecs are derived from the snapshot's normalized DiffFile entry, never
// attacker-controlled; the RPC layer additionally enforces snapshot-id and
// path-containment before calling here.
// ---------------------------------------------------------------------------

/// Per-file full diff byte cap for the paging RPC. Bounded so a single huge
/// file never streams unbounded git output, but well above the 2 MiB snapshot
/// cap so a file truncated by the global snapshot can still be loaded in full.
pub const MAX_FILE_DIFF_BYTES: usize = 8 * 1024 * 1024;
/// Maximum diff lines returned in one page. Combined with the per-page byte
/// cap, keeps every page well under the 4 MiB WS frame limit.
pub const MAX_FILE_PAGE_LINES: usize = 1000;
/// Per-page wire byte cap (sum of line text + numbers + JSON overhead). The
/// page accumulator stops at this bound even when the line-count cap has not
/// been reached, so a page of long lines cannot blow the WS frame.
pub const MAX_FILE_PAGE_BYTES: usize = 1024 * 1024;
/// Per-line text cap. A single minified line can exceed the page byte cap; the
/// loader truncates such a line and marks it so the page stays frame-safe and
/// the user knows content was elided.
pub const MAX_DIFF_LINE_TEXT_BYTES: usize = 256 * 1024;

/// Full parsed diff for a single file, ready to be sliced into pages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDiff {
    pub path: String,
    pub previous_path: Option<String>,
    pub status: FileStatus,
    pub binary: bool,
    /// All diff lines across hunks, in display order.
    pub lines: Vec<DiffLine>,
    /// True when the per-file byte cap was hit — even more content exists
    /// beyond what was loaded. The frontend surfaces this as a hard cap.
    pub truncated: bool,
    pub error: Option<String>,
}

/// One bounded page of a single file's diff.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDiffPage {
    pub path: String,
    pub snapshot_id: String,
    pub previous_path: Option<String>,
    pub status: FileStatus,
    pub binary: bool,
    pub lines: Vec<DiffLine>,
    /// Cursor of the first line in this page (0-based across the full file).
    pub cursor: usize,
    /// Cursor of the next page, or `None` when no more lines remain.
    pub next_cursor: Option<usize>,
    pub has_more: bool,
    /// Total lines in the full file diff (the page is a window over this).
    pub total_lines: usize,
    /// Per-file byte cap was hit during load; even more content exists.
    pub truncated: bool,
}

impl FileDiff {
    /// Load a single file's full diff for the same comparison the snapshot
    /// used. `file` is the snapshot's [`DiffFile`] entry (path + rename
    /// provenance), so the pathspec is containment-derived and never
    /// attacker-controlled. Output is bounded to [`MAX_FILE_DIFF_BYTES`] and
    /// each line's text is capped to [`MAX_DIFF_LINE_TEXT_BYTES`].
    #[must_use]
    pub fn load(cwd: &Path, scope: &ReviewScope, file: &DiffFile) -> Self {
        Self::load_with_root(cwd, scope, file)
    }

    fn load_with_root(cwd: &Path, scope: &ReviewScope, file: &DiffFile) -> Self {
        // Pathspecs come from the snapshot's normalized file entry; re-normalize
        // for defense in depth and reject empty / traversal / NUL.
        let mut pathspecs: Vec<OsString> = Vec::new();
        if let Some(previous) = &file.previous_path {
            let normalized = normalize_repo_path(previous);
            if normalized.is_empty() || normalized == "." || normalized.contains('\0') {
                return Self::error(file.path.clone(), file.previous_path.clone(), format!("invalid rename source {previous:?}"));
            }
            pathspecs.push(os_string_from_git_path(normalized.into_bytes()));
        }
        let normalized = normalize_repo_path(&file.path);
        if normalized.is_empty() || normalized == "." || normalized.contains('\0') {
            return Self::error(file.path.clone(), file.previous_path.clone(), format!("invalid file path {:?}", file.path));
        }
        pathspecs.push(os_string_from_git_path(normalized.into_bytes()));

        let repository = match RepositoryLayout::discover(cwd) {
            Ok(repository) => repository,
            Err(error) => {
                return Self::error(file.path.clone(), file.previous_path.clone(), format_git_error("not a git repository", &error));
            }
        };
        if let Err(error) = repository_attributes_are_safe(&repository) {
            return Self::error(file.path.clone(), file.previous_path.clone(), error);
        }
        let head_reference = match read_small_file(&repository.git_dir.join("HEAD"), MAX_GIT_METADATA_BYTES)
            .and_then(|bytes| String::from_utf8(bytes).map_err(|_| "git HEAD is not valid UTF-8".to_owned()))
        {
            Ok(head) => strip_output_line_ending(&head).to_owned(),
            Err(error) => return Self::error(file.path.clone(), file.previous_path.clone(), error),
        };
        let object_format = match detect_object_format(&repository) {
            Ok(format) => format,
            Err(error) => return Self::error(file.path.clone(), file.previous_path.clone(), error),
        };
        let sandbox = match GitSandbox::create(&repository, &object_format, &head_reference) {
            Ok(sandbox) => sandbox,
            Err(error) => return Self::error(file.path.clone(), file.previous_path.clone(), error),
        };
        let root = repository.root.clone();
        let isolated = sandbox.environment(&root);

        let output = match scope {
            ReviewScope::WorkingTree => Self::run_working_tree(&root, isolated, &pathspecs),
            ReviewScope::Revisions { from, to } => Self::run_revisions(&root, isolated, &pathspecs, from, to),
        };
        match output {
            Ok(output) => Self::from_output(file, output),
            Err(error) => Self::error(file.path.clone(), file.previous_path.clone(), error),
        }
    }

    fn run_working_tree(
        root: &Path,
        isolated: IsolatedGitEnvironment<'_>,
        pathspecs: &[OsString],
    ) -> Result<GitOutput, String> {
        let head = git_rev_parse(root, &["--verify", "HEAD"], Some(isolated))
            .map(|head| strip_output_line_ending(&head).to_owned())
            .map_err(|_| "repository has no commits yet (unborn HEAD)".to_owned())?;
        let sandbox_index = isolated.git_dir.join("index");
        if !sandbox_index.is_file() {
            let args = ["read-tree", head.as_str()];
            match run_git_bounded(root, &args, MAX_GIT_METADATA_BYTES, Some(isolated)) {
                Ok(output) if output.error.is_none() && !output.truncated => {}
                Ok(output) => {
                    return Err(output.error.unwrap_or_else(|| "git read-tree output exceeded the metadata limit".to_owned()));
                }
                Err(error) => return Err(error),
            }
        }
        // Re-base only the target paths on HEAD in the disposable index: a
        // copied real index may already have staged a rename's source
        // deletion, in which case `git add -- <source>` fails ("did not match
        // any files") because the path exists in neither the index nor the
        // worktree. Resetting the path entries to HEAD keeps every pathspec
        // addressable so rename detection pairs both sides exactly like the
        // snapshot; other index entries stay untouched. `git reset` tolerates
        // unmatched pathspecs silently, so a stale or malicious path still
        // fails closed at the `git add` step below.
        let mut reset_args: Vec<OsString> = vec![
            "--literal-pathspecs".into(),
            "reset".into(),
            "-q".into(),
            "HEAD".into(),
            "--".into(),
        ];
        reset_args.extend(pathspecs.iter().cloned());
        match run_git_bounded_os(root, &reset_args, MAX_GIT_METADATA_BYTES, Some(isolated)) {
            Ok(output) if output.error.is_none() && !output.truncated => {}
            Ok(output) => {
                return Err(output.error.unwrap_or_else(|| "git reset output exceeded the metadata limit".to_owned()));
            }
            Err(error) => return Err(error),
        }
        // Stage only the target paths in the disposable sandbox index so the
        // HEAD-vs-working-tree content for these paths is reviewable exactly
        // like the snapshot, without staging the whole tree.
        let mut add_args: Vec<OsString> = vec!["--literal-pathspecs".into(), "add".into(), "--".into()];
        add_args.extend(pathspecs.iter().cloned());
        match run_git_bounded_os(root, &add_args, MAX_GIT_METADATA_BYTES, Some(isolated)) {
            Ok(output) if output.error.is_none() && !output.truncated => {}
            Ok(output) => {
                return Err(output.error.unwrap_or_else(|| "git add output exceeded the metadata limit".to_owned()));
            }
            Err(error) => return Err(error),
        }
        let mut args: Vec<OsString> = vec![
            "--literal-pathspecs".into(),
            "diff".into(),
            "--cached".into(),
            "--no-ext-diff".into(),
            "--no-textconv".into(),
            "--no-color".into(),
            "--default-prefix".into(),
            "--no-relative".into(),
            "--find-renames".into(),
            "--histogram".into(),
            head.into(),
            "-O".into(),
            null_device().into(),
            "--".into(),
        ];
        args.extend(pathspecs.iter().cloned());
        run_git_bounded_os(root, &args, MAX_FILE_DIFF_BYTES, Some(isolated))
    }

    fn run_revisions(
        root: &Path,
        isolated: IsolatedGitEnvironment<'_>,
        pathspecs: &[OsString],
        from: &str,
        to: &str,
    ) -> Result<GitOutput, String> {
        let from_commit = resolve_review_commit(root, from, "source")?;
        let to_commit = resolve_review_commit(root, to, "target")?;
        let mut args: Vec<OsString> = vec![
            "--literal-pathspecs".into(),
            "diff".into(),
            "--no-ext-diff".into(),
            "--no-textconv".into(),
            "--no-color".into(),
            "--default-prefix".into(),
            "--no-relative".into(),
            "--find-renames".into(),
            "--histogram".into(),
            "-O".into(),
            null_device().into(),
            from_commit.into(),
            to_commit.into(),
            "--".into(),
        ];
        args.extend(pathspecs.iter().cloned());
        run_git_bounded_os(root, &args, MAX_FILE_DIFF_BYTES, Some(isolated))
    }

    fn from_output(file: &DiffFile, output: GitOutput) -> Self {
        if let Some(error) = output.error {
            return Self::error(file.path.clone(), file.previous_path.clone(), error);
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let files = parse_unified_diff(&stdout);
        // Pick the entry matching the snapshot file's path; a rename with two
        // pathspecs still produces one entry whose path == file.path.
        let entry = files
            .iter()
            .find(|parsed| parsed.path == file.path)
            .or_else(|| files.first());
        let Some(entry) = entry else {
            return Self {
                path: file.path.clone(),
                previous_path: file.previous_path.clone(),
                status: file.status,
                binary: false,
                lines: Vec::new(),
                truncated: false,
                error: Some("file diff is empty (the path may no longer differ in this comparison)".to_owned()),
            };
        };
        let mut lines: Vec<DiffLine> = Vec::new();
        for hunk in &entry.hunks {
            for line in &hunk.lines {
                lines.push(cap_line(line.clone()));
            }
        }
        Self {
            path: entry.path.clone(),
            previous_path: entry.previous_path.clone(),
            status: entry.status,
            binary: entry.binary,
            lines,
            truncated: output.truncated,
            error: None,
        }
    }

    fn error(path: String, previous_path: Option<String>, message: String) -> Self {
        Self {
            path,
            previous_path,
            status: FileStatus::Unknown,
            binary: false,
            lines: Vec::new(),
            truncated: false,
            error: Some(message),
        }
    }

    /// Slice a bounded page out of the loaded diff. `cursor` must be in range;
    /// `max_lines` is clamped to [`MAX_FILE_PAGE_LINES`]. The page also stops
    /// at [`MAX_FILE_PAGE_BYTES`] so a page of long lines never exceeds the WS
    /// frame budget. At least one line is always returned when the cursor is
    /// in range and the file is not binary/empty.
    pub fn slice_page(
        &self,
        snapshot_id: &str,
        cursor: usize,
        max_lines: usize,
    ) -> Result<FileDiffPage, String> {
        if snapshot_id.is_empty() {
            return Err("missing snapshot id".to_owned());
        }
        if let Some(error) = &self.error {
            return Err(error.clone());
        }
        let total = self.lines.len();
        if cursor > total {
            return Err(format!("cursor {cursor} out of range (total {total})"));
        }
        let cap = max_lines.clamp(1, MAX_FILE_PAGE_LINES);
        let mut page: Vec<DiffLine> = Vec::new();
        let mut bytes = 0usize;
        let mut index = cursor;
        while index < total {
            if page.len() >= cap {
                break;
            }
            let line = &self.lines[index];
            // 32 bytes covers line numbers + kind tag + JSON overhead.
            let line_bytes = line.text.len() + 32;
            if !page.is_empty() && bytes + line_bytes > MAX_FILE_PAGE_BYTES {
                break;
            }
            bytes += line_bytes;
            page.push(line.clone());
            index += 1;
        }
        let next_cursor = index;
        let has_more = next_cursor < total;
        Ok(FileDiffPage {
            path: self.path.clone(),
            snapshot_id: snapshot_id.to_owned(),
            previous_path: self.previous_path.clone(),
            status: self.status,
            binary: self.binary,
            lines: page,
            cursor,
            next_cursor: has_more.then_some(next_cursor),
            has_more,
            total_lines: total,
            truncated: self.truncated,
        })
    }
}

/// Truncate a diff line's text to [`MAX_DIFF_LINE_TEXT_BYTES`] on a UTF-8
/// boundary, appending a visible marker so the elision is never silent.
fn cap_line(mut line: DiffLine) -> DiffLine {
    if line.text.len() > MAX_DIFF_LINE_TEXT_BYTES {
        let mut end = MAX_DIFF_LINE_TEXT_BYTES;
        while end > 0 && !line.text.is_char_boundary(end) {
            end -= 1;
        }
        line.text.truncate(end);
        line.text.push_str(" ⋯[line truncated: exceeds byte cap]");
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    fn write(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(path, body).expect("write");
    }

    fn git(cwd: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args([
                "-c",
                "color.ui=false",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
                "-c",
                "core.hooksPath=.git-test-no-hooks",
            ])
            .args(args)
            .current_dir(cwd)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", cwd.join("absent-global-git-config"))
            .env("LC_ALL", "C")
            .env("GIT_AUTHOR_NAME", "Pi Test")
            .env("GIT_AUTHOR_EMAIL", "pi@example.test")
            .env("GIT_COMMITTER_NAME", "Pi Test")
            .env("GIT_COMMITTER_EMAIL", "pi@example.test")
            .output()
            .expect("git");
        assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    }

    fn git_output(cwd: &Path, args: &[&str]) -> Vec<u8> {
        let output = Command::new("git")
            .args(["-c", "color.ui=false", "-c", "core.hooksPath=.git-test-no-hooks"])
            .args(args)
            .current_dir(cwd)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", cwd.join("absent-global-git-config"))
            .env("LC_ALL", "C")
            .output()
            .expect("git output");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    fn init_repo() -> TempDir {
        let dir = TempDir::new().expect("tempdir");
        git(dir.path(), &["init"]);
        write(dir.path().join("README.md").as_path(), "base\n");
        git(dir.path(), &["add", "README.md"]);
        git(dir.path(), &["commit", "-m", "initial"]);
        dir
    }

    #[cfg(unix)]
    fn shell_quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }


    #[test]
    fn parse_unified_diff_tracks_line_numbers_status_rename_and_binary() {
        let input = r#"diff --git a/src/a.rs b/src/a.rs
index 111..222 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,3 +1,4 @@
 keep
-old
+new
+added
 context
diff --git a/old_name.txt b/new_name.txt
similarity index 90%
rename from old_name.txt
rename to new_name.txt
diff --git a/assets/logo.png b/assets/logo.png
new file mode 100644
Binary files /dev/null and b/assets/logo.png differ
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
--- a/gone.txt
+++ /dev/null
@@ -1 +0,0 @@
-bye
"#;
        let files = parse_unified_diff(input);
        assert_eq!(files.len(), 4);
        assert_eq!(files[0].path, "src/a.rs");
        assert_eq!(files[0].insertions, 2);
        assert_eq!(files[0].deletions, 1);
        assert_eq!(files[0].hunks[0].lines[0].kind, DiffLineKind::Context);
        assert_eq!(files[1].path, "new_name.txt");
        assert_eq!(files[1].previous_path.as_deref(), Some("old_name.txt"));
        assert_eq!(files[1].status, FileStatus::Renamed);
        assert!(files[2].binary);
        assert_eq!(files[3].status, FileStatus::Deleted);
    }

    #[test]
    fn load_review_snapshot_loads_staged_tracked_and_unstaged_changes() {
        let repo = init_repo();
        write(repo.path().join("staged.txt").as_path(), "staged\n");
        git(repo.path(), &["add", "staged.txt"]);
        write(repo.path().join("README.md").as_path(), "base\nstaged\n");
        git(repo.path(), &["add", "README.md"]);
        let index = repo.path().join(".git/index");
        let index_before = fs::read(&index).expect("index before");
        write(repo.path().join("README.md").as_path(), "base\nstaged\nunstaged\n");

        let snapshot = load_review_snapshot(repo.path());

        let error = snapshot.error.as_deref().unwrap_or_default();
        assert!(!error.contains("unknown option 'cached'"), "{error}");
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        let readme = snapshot.files.iter().find(|f| f.path == "README.md").expect("readme");
        assert_eq!(readme.insertions, 2);
        assert_eq!(snapshot.files.iter().find(|f| f.path == "staged.txt").expect("staged").status, FileStatus::Added);
        assert_eq!(fs::read(&index).expect("index after"), index_before);
    }
    #[test]
    fn review_scope_requires_zero_or_two_revisions() {
        assert_eq!(
            ReviewScope::parse(None).expect("working tree scope"),
            ReviewScope::WorkingTree
        );
        assert_eq!(
            ReviewScope::parse(Some("main feature")).expect("revision scope"),
            ReviewScope::Revisions {
                from: "main".to_owned(),
                to: "feature".to_owned(),
            }
        );
        assert_eq!(
            ReviewScope::parse(Some("main")).expect_err("one revision must fail"),
            "Usage: /code-review [<from> <to>]"
        );
        assert_eq!(
            ReviewScope::parse(Some("a b c")).expect_err("three revisions must fail"),
            "Usage: /code-review [<from> <to>]"
        );
    }

    #[test]
    fn comparison_label_replaces_invisible_format_controls() {
        let scope = ReviewScope::Revisions {
            from: "main\u{202e}hidden".to_owned(),
            to: "feature\u{200b}branch".to_owned(),
        };

        assert_eq!(scope.label(), "main hidden → feature branch");
    }

    #[test]
    fn load_review_snapshot_compares_two_branches_and_ignores_working_tree() {
        let repo = init_repo();
        git(repo.path(), &["branch", "review-base"]);
        write(repo.path().join("committed.txt").as_path(), "committed\n");
        git(repo.path(), &["add", "committed.txt"]);
        git(repo.path(), &["commit", "-m", "add committed file"]);
        git(repo.path(), &["branch", "review-target"]);
        write(repo.path().join("README.md").as_path(), "working tree only\n");

        let snapshot = load_review_snapshot_for(
            repo.path(),
            ReviewScope::Revisions {
                from: "review-base".to_owned(),
                to: "review-target".to_owned(),
            },
        );

        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        assert_eq!(snapshot.comparison_label(), "review-base → review-target");
        assert_eq!(snapshot.files.len(), 1, "{:?}", snapshot.files);
        assert_eq!(snapshot.files[0].path, "committed.txt");
        assert_eq!(snapshot.files[0].status, FileStatus::Added);
        assert!(!snapshot.files.iter().any(|file| file.path == "README.md"));
    }

    #[test]
    fn load_review_snapshot_accepts_commit_hash_and_tag() {
        let repo = init_repo();
        let from = String::from_utf8(git_output(repo.path(), &["rev-parse", "HEAD"]))
            .expect("commit hash");
        let from = strip_output_line_ending(&from);
        write(repo.path().join("tagged.txt").as_path(), "tagged\n");
        git(repo.path(), &["add", "tagged.txt"]);
        git(repo.path(), &["commit", "-m", "tagged commit"]);
        git(repo.path(), &["tag", "review-tag"]);

        let snapshot = load_review_snapshot_for(
            repo.path(),
            ReviewScope::Revisions {
                from: from.to_owned(),
                to: "review-tag".to_owned(),
            },
        );

        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        assert_eq!(snapshot.files.len(), 1, "{:?}", snapshot.files);
        assert_eq!(snapshot.files[0].path, "tagged.txt");
    }

    #[test]
    fn load_review_snapshot_rejects_invalid_revision_without_echoing_it() {
        let repo = init_repo();
        let invalid = "missing-revision-sensitive-marker";
        let snapshot = load_review_snapshot_for(
            repo.path(),
            ReviewScope::Revisions {
                from: invalid.to_owned(),
                to: "HEAD".to_owned(),
            },
        );

        let error = snapshot.error.expect("invalid revision error");
        assert_eq!(
            error,
            "invalid source revision; expected a commit, branch, or tag"
        );
        assert!(!error.contains(invalid));
    }

    #[test]
    fn load_review_snapshot_resolves_option_looking_revision_as_operand() {
        // A ref whose name looks like a rev-parse option must be treated as an
        // operand by the fixed `--end-of-options` argv in `resolve_review_commit`,
        // not parsed as a flag. Created via plumbing so the test does not rely on
        // porcelain option escaping.
        let repo = init_repo();
        let base = String::from_utf8(git_output(repo.path(), &["rev-parse", "HEAD"]))
            .expect("base hash");
        let base = strip_output_line_ending(&base).to_owned();
        write(repo.path().join("target.txt").as_path(), "target\n");
        git(repo.path(), &["add", "target.txt"]);
        git(repo.path(), &["commit", "-m", "target commit"]);
        let head = String::from_utf8(git_output(repo.path(), &["rev-parse", "HEAD"]))
            .expect("head hash");
        let head = strip_output_line_ending(&head);
        let option_ref = "--output=evil";
        git(repo.path(), &["update-ref", &format!("refs/tags/{option_ref}"), head]);

        let snapshot = load_review_snapshot_for(
            repo.path(),
            ReviewScope::Revisions {
                from: base,
                to: option_ref.to_owned(),
            },
        );

        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        assert_eq!(snapshot.files.len(), 1, "{:?}", snapshot.files);
        assert_eq!(snapshot.files[0].path, "target.txt");
        assert_eq!(snapshot.files[0].status, FileStatus::Added);
    }

    #[cfg(unix)]
    #[test]
    fn load_revision_snapshot_never_executes_configured_filters_or_diff_helpers() {
        // The two-revision path runs an object-to-object diff through the
        // isolated sandbox with fixed argv, so repository-local executable
        // config (clean/process filters, external diff, fsmonitor) must never
        // be invoked even when present in the real repository.
        let repo = init_repo();
        write(repo.path().join("victim.txt").as_path(), "base\n");
        git(repo.path(), &["add", "victim.txt"]);
        git(repo.path(), &["commit", "-m", "add filtered file"]);
        let base = String::from_utf8(git_output(repo.path(), &["rev-parse", "HEAD"]))
            .expect("base hash");
        let base = strip_output_line_ending(&base).to_owned();
        write(repo.path().join("victim.txt").as_path(), "changed\n");
        git(repo.path(), &["add", "victim.txt"]);
        git(repo.path(), &["commit", "-m", "change filtered file"]);
        let head = String::from_utf8(git_output(repo.path(), &["rev-parse", "HEAD"]))
            .expect("head hash");
        let head = strip_output_line_ending(&head).to_owned();

        write(repo.path().join(".gitattributes").as_path(), "*.txt filter=evil\n");
        let driver = repo.path().join("evil-driver");
        let sentinel = repo.path().join("filter-fired");
        write(&driver, ": > filter-fired\ncat\n");
        let driver = driver.to_str().expect("utf8 driver");
        let driver_command = format!("sh {}", shell_quote(driver));
        git(repo.path(), &["config", "filter.evil.clean", &driver_command]);
        git(repo.path(), &["config", "filter.evil.process", &driver_command]);
        git(repo.path(), &["config", "diff.external", &driver_command]);
        git(repo.path(), &["config", "core.fsmonitor", &driver_command]);

        let snapshot = load_review_snapshot_for(
            repo.path(),
            ReviewScope::Revisions {
                from: base,
                to: head,
            },
        );

        assert!(!sentinel.exists(), "configured executable was invoked during revision diff");
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        assert_eq!(snapshot.files.len(), 1, "{:?}", snapshot.files);
        assert_eq!(snapshot.files[0].path, "victim.txt");
        assert_eq!(snapshot.files[0].status, FileStatus::Modified);
    }

    #[test]
    fn load_review_snapshot_keeps_changes_beyond_large_tracked_index_metadata() {
        let repo = init_repo();
        let payload = "x".repeat(48);
        let mut late_path = None;
        for index in 0..1_600 {
            let path = format!("tracked/{index:04}-{payload}.txt");
            write(repo.path().join(&path).as_path(), "base\n");
            late_path = Some(path);
        }
        git(repo.path(), &["add", "tracked"]);
        git(repo.path(), &["commit", "-m", "large tracked index"]);
        let late_path = late_path.expect("late tracked path");
        let tracked = git_output(repo.path(), &["ls-files", "-z"]);
        assert!(tracked.len() > MAX_GIT_METADATA_BYTES);
        write(repo.path().join(&late_path).as_path(), "changed\n");

        let snapshot = load_review_snapshot(repo.path());

        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        assert!(
            snapshot.files.iter().any(|file| file.path == late_path),
            "changed path after the first metadata window was omitted: {:?}",
            snapshot.files.iter().map(|file| file.path.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn load_review_snapshot_preserves_directories_named_a_and_b() {
        let repo = init_repo();
        write(repo.path().join("a/alpha.txt").as_path(), "base\n");
        write(repo.path().join("b/beta.txt").as_path(), "base\n");
        git(repo.path(), &["add", "a/alpha.txt", "b/beta.txt"]);
        git(repo.path(), &["commit", "-m", "add prefix directories"]);
        write(repo.path().join("a/alpha.txt").as_path(), "changed\n");
        write(repo.path().join("b/beta.txt").as_path(), "changed\n");
        let snapshot = load_review_snapshot(repo.path());
        let paths = snapshot
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>();
        assert!(paths.contains(&"a/alpha.txt"), "{paths:?}");
        assert!(paths.contains(&"b/beta.txt"), "{paths:?}");
    }

    #[cfg(unix)]
    #[test]
    fn load_review_snapshot_never_executes_configured_filters_or_diff_helpers() {
        let repo = init_repo();
        write(repo.path().join("victim.txt").as_path(), "base\n");
        git(repo.path(), &["add", "victim.txt"]);
        git(repo.path(), &["commit", "-m", "add filtered file"]);
        write(repo.path().join(".gitattributes").as_path(), "*.txt filter=evil\n");
        let driver = repo.path().join("evil-driver");
        let sentinel = repo.path().join("filter-fired");
        write(&driver, ": > filter-fired\ncat\n");
        let driver = driver.to_str().expect("utf8 driver");
        let driver_command = format!("sh {}", shell_quote(driver));
        git(repo.path(), &["config", "filter.evil.clean", &driver_command]);
        git(repo.path(), &["config", "filter.evil.process", &driver_command]);
        git(repo.path(), &["config", "diff.external", &driver_command]);
        git(repo.path(), &["config", "core.fsmonitor", &driver_command]);
        write(repo.path().join("victim.txt").as_path(), "changed\n");
        let index = repo.path().join(".git/index");
        let index_before = fs::read(&index).expect("index before");
        let snapshot = load_review_snapshot(repo.path());
        assert!(!sentinel.exists(), "configured executable was invoked");
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        assert!(snapshot.files.iter().any(|file| file.path == "victim.txt"));
        assert_eq!(fs::read(&index).expect("index after"), index_before);
    }

    #[cfg(unix)]
    #[test]
    fn git_runner_isolates_global_and_system_config() {
        let directory = TempDir::new().expect("temp");
        let global = directory.path().join("global.gitconfig");
        let system = directory.path().join("system.gitconfig");
        write(&global, "[alias]\n\tpwn = !touch global-fired\n");
        write(&system, "[alias]\n\tpwn = !touch system-fired\n");
        let output = Command::new("git")
            .arg("--version")
            .env("GIT_CONFIG_GLOBAL", &global)
            .env("GIT_CONFIG_SYSTEM", &system)
            .output()
            .expect("git version");
        assert!(output.status.success());
        let result = run_git_bounded(
            directory.path(),
            &["pwn"],
            MAX_GIT_METADATA_BYTES,
            None,
        )
        .expect("bounded git");
        assert!(result.error.is_some());
        assert!(!directory.path().join("global-fired").exists());
        assert!(!directory.path().join("system-fired").exists());
    }

    #[cfg(unix)]
    #[test]
    fn load_review_snapshot_decodes_backslash_and_tab_rename_paths() {
        let repo = init_repo();
        let old = "old\\name\t.txt";
        let new = "new\\name\t.txt";
        write(repo.path().join(old).as_path(), "same\n");
        git(repo.path(), &["add", old]);
        git(repo.path(), &["commit", "-m", "add unusual path"]);
        fs::rename(repo.path().join(old), repo.path().join(new)).expect("rename unusual path");
        let snapshot = load_review_snapshot(repo.path());
        let renamed = snapshot.files.iter().find(|file| file.path == new).expect("renamed path");
        assert_eq!(renamed.previous_path.as_deref(), Some(old));
        assert_eq!(renamed.status, FileStatus::Renamed);
    }

    #[test]
    fn load_review_snapshot_keeps_edit_and_rename_but_drops_untracked() {
        let repo = init_repo();
        write(repo.path().join("notes.md").as_path(), "base\n");
        write(repo.path().join("old.txt").as_path(), "payload\n");
        git(repo.path(), &["add", "notes.md", "old.txt"]);
        git(repo.path(), &["commit", "-m", "add tracked files"]);
        // Unstaged edit to a tracked file, a bare working-tree rename of a
        // tracked file, and an unrelated untracked file left alone.
        write(repo.path().join("notes.md").as_path(), "base\nedited\n");
        fs::rename(repo.path().join("old.txt"), repo.path().join("new.txt"))
            .expect("rename tracked file");
        write(repo.path().join("stray.txt").as_path(), "ignored by review\n");
        let index = repo.path().join(".git/index");
        let index_before = fs::read(&index).expect("index before");

        let snapshot = load_review_snapshot(repo.path());

        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        let notes = snapshot
            .files
            .iter()
            .find(|file| file.path == "notes.md")
            .expect("edited tracked file present");
        assert_eq!(notes.status, FileStatus::Modified);
        let renamed = snapshot
            .files
            .iter()
            .find(|file| file.path == "new.txt")
            .expect("renamed tracked file present");
        assert_eq!(renamed.previous_path.as_deref(), Some("old.txt"));
        assert_eq!(renamed.status, FileStatus::Renamed);
        assert!(
            !snapshot.files.iter().any(|file| file.path == "stray.txt"),
            "unrelated untracked file must be absent: {:?}",
            snapshot.files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>()
        );
        assert_eq!(fs::read(&index).expect("index after"), index_before);
    }

    #[test]
    fn snapshot_fails_closed_on_nonzero_git_with_partial_patch() {
        let output = GitOutput {
            stdout: b"diff --git a/file.txt b/file.txt\n--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-old\n+new\n".to_vec(),
            truncated: false,
            error: Some("fatal: later path failed".to_owned()),
        };
        let snapshot = snapshot_from_diff_output(
            PathBuf::from("repo"),
            ReviewScope::WorkingTree,
            b"working-tree",
            output,
            None,
        );
        assert!(snapshot.files.is_empty());
        assert_eq!(snapshot.error.as_deref(), Some("fatal: later path failed"));
    }

    #[cfg(unix)]
    #[test]
    fn git_runner_kills_descendants_that_retain_output_pipes() {
        let directory = TempDir::new().expect("temp");
        let script = directory.path().join("hold-stderr");
        write(&script, "(sleep 30; : > child-finished) >&2 &\nexit 0\n");
        let script = script.to_str().expect("utf8 script");
        let alias = format!("alias.hold=!sh {}", shell_quote(script));
        let started = Instant::now();
        let error = run_git_bounded(directory.path(), &["-c", alias.as_str(), "hold"], MAX_GIT_METADATA_BYTES, None)
            .expect_err("retained pipe must fail closed");
        assert!(error.contains("remained open"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(50));
        assert!(!directory.path().join("child-finished").exists());
    }

    #[test]
    fn load_review_snapshot_reports_non_repo_and_unborn_head() {
        let tmp = TempDir::new().expect("temp");
        assert!(load_review_snapshot(tmp.path()).error.expect("error").contains("not a git"));
        let unborn = TempDir::new().expect("temp");
        git(unborn.path(), &["init"]);
        assert!(load_review_snapshot(unborn.path()).error.expect("unborn error").contains("unborn"));
    }

    #[test]
    fn load_review_snapshot_marks_oversize_output() {
        let data = vec![b'x'; MAX_DIFF_BYTES + 64];
        let (bytes, truncated) =
            read_bounded(std::io::Cursor::new(data), MAX_DIFF_BYTES).expect("read");
        assert_eq!(bytes.len(), MAX_DIFF_BYTES);
        assert!(truncated);
    }

    #[test]
    fn c_quoted_decoder_handles_standard_octal_and_non_utf8_bytes() {
        assert_eq!(
            decode_git_path(r#""tab\tback\\slash\040name""#),
            "tab\tback\\slash name"
        );
        assert_eq!(decode_git_path(r#""bad\377name""#), r"bad\377name");
    }

    #[test]
    fn file_tree_collapse_keeps_selection_targets_valid() {
        let snapshot = ReviewSnapshot {
            root: PathBuf::from("repo"),
            scope: ReviewScope::WorkingTree,
            snapshot_id: "snapshot".to_owned(),
            files: vec![DiffFile {
                path: "src/a.rs".into(),
                previous_path: None,
                status: FileStatus::Modified,
                binary: false,
                insertions: 2,
                deletions: 1,
                hunks: Vec::new(),
                truncated: false,
                message: None,
            }],
            truncated: false,
            error: None,
        };
        let tree = FileTree::from_snapshot(&snapshot);
        assert!(tree.first_file_visible_index().is_some());
    }

    #[test]
    fn normalize_repo_path_strips_traversal_and_preserves_backslashes() {
        assert_eq!(normalize_repo_path(r"src\foo.rs"), r"src\foo.rs");
        assert_eq!(normalize_repo_path("../etc/passwd"), "etc/passwd");
        assert_eq!(normalize_repo_path("/abs/path"), "abs/path");
    }

    #[test]
    fn hunk_identity_is_stable_and_content_sensitive() {
        let files = parse_unified_diff(
            "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-old\n+new\n",
        );
        let file = &files[0];
        let hunk = &file.hunks[0];
        let first = HunkIdentity::new("snapshot-a", &file.path, hunk);
        let same = HunkIdentity::new("snapshot-a", &file.path, hunk);
        let next_snapshot = HunkIdentity::new("snapshot-b", &file.path, hunk);
        assert_eq!(first, same);
        assert!(first.matches_across_snapshots(&next_snapshot));
        let changed_files = parse_unified_diff(
            "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-old\n+other\n",
        );
        let changed = HunkIdentity::new("snapshot-a", "a.rs", &changed_files[0].hunks[0]);
        assert_ne!(first.content_hash, changed.content_hash);
        assert!(!first.matches_across_snapshots(&changed));
    }

    // ---- Bounded single-file diff paging ----

    fn diff_file_at(path: &str, previous: Option<&str>, status: FileStatus) -> DiffFile {
        DiffFile {
            path: path.to_owned(),
            previous_path: previous.map(str::to_owned),
            status,
            binary: false,
            insertions: 0,
            deletions: 0,
            hunks: Vec::new(),
            truncated: false,
            message: None,
        }
    }

    #[test]
    fn slice_page_rejects_empty_snapshot_id_and_out_of_range_cursor() {
        let diff = FileDiff {
            path: "a.rs".into(),
            previous_path: None,
            status: FileStatus::Modified,
            binary: false,
            lines: vec![
                DiffLine { kind: DiffLineKind::Context, old_no: Some(1), new_no: Some(1), text: "keep".into() },
                DiffLine { kind: DiffLineKind::Addition, old_no: None, new_no: Some(2), text: "added".into() },
            ],
            truncated: false,
            error: None,
        };
        assert!(diff.slice_page("", 0, 10).is_err(), "empty snapshot id rejected");
        assert!(diff.slice_page("snap", 3, 10).is_err(), "cursor past total rejected");
        // cursor == total is valid (returns an empty terminal page).
        let terminal = diff.slice_page("snap", 2, 10).expect("cursor == total ok");
        assert!(terminal.lines.is_empty());
        assert!(!terminal.has_more);
        assert_eq!(terminal.next_cursor, None);
        assert_eq!(terminal.total_lines, 2);
    }

    #[test]
    fn slice_page_clamps_max_lines_and_preserves_order() {
        let lines: Vec<DiffLine> = (0..5)
            .map(|i| DiffLine { kind: DiffLineKind::Addition, old_no: None, new_no: Some(i + 1), text: format!("line{i}") })
            .collect();
        let diff = FileDiff {
            path: "a.rs".into(),
            previous_path: None,
            status: FileStatus::Modified,
            binary: false,
            lines,
            truncated: false,
            error: None,
        };
        let first = diff.slice_page("snap", 0, 2).expect("first page");
        assert_eq!(first.cursor, 0);
        assert_eq!(first.lines.len(), 2);
        assert_eq!(first.lines[0].text, "line0");
        assert_eq!(first.lines[1].text, "line1");
        assert!(first.has_more);
        assert_eq!(first.next_cursor, Some(2));
        let second = diff.slice_page("snap", 2, 2).expect("second page");
        assert_eq!(second.cursor, 2);
        assert_eq!(second.lines.iter().map(|l| l.text.clone()).collect::<Vec<_>>(), ["line2", "line3"]);
        assert!(second.has_more);
        assert_eq!(second.next_cursor, Some(4));
        let third = diff.slice_page("snap", 4, 2).expect("third page");
        assert_eq!(third.lines.len(), 1);
        assert_eq!(third.lines[0].text, "line4");
        assert!(!third.has_more);
        assert_eq!(third.next_cursor, None);
        // max_lines=0 clamps to 1.
        let one = diff.slice_page("snap", 0, 0).expect("max_lines 0 -> 1");
        assert_eq!(one.lines.len(), 1);
    }

    #[test]
    fn slice_page_surfaces_error_and_binary_state() {
        let err = FileDiff {
            path: "a.rs".into(),
            previous_path: None,
            status: FileStatus::Unknown,
            binary: false,
            lines: Vec::new(),
            truncated: false,
            error: Some("boom".into()),
        };
        assert!(err.slice_page("snap", 0, 10).is_err());
        let bin = FileDiff {
            path: "a.png".into(),
            previous_path: None,
            status: FileStatus::Binary,
            binary: true,
            lines: Vec::new(),
            truncated: false,
            error: None,
        };
        let page = bin.slice_page("snap", 0, 10).expect("binary page");
        assert!(page.binary);
        assert!(page.lines.is_empty());
        assert!(!page.has_more);
    }

    #[test]
    fn cap_line_truncates_oversize_text_on_char_boundary() {
        let huge = "x".repeat(MAX_DIFF_LINE_TEXT_BYTES + 200);
        let line = DiffLine { kind: DiffLineKind::Addition, old_no: None, new_no: Some(1), text: huge };
        let capped = cap_line(line);
        assert!(capped.text.len() < MAX_DIFF_LINE_TEXT_BYTES + 200);
        assert!(capped.text.contains("⋯[line truncated: exceeds byte cap]"));
        let small = cap_line(DiffLine { kind: DiffLineKind::Context, old_no: Some(1), new_no: Some(1), text: "ok".into() });
        assert_eq!(small.text, "ok");
    }

    #[test]
    fn load_file_diff_loads_real_working_tree_file() {
        let repo = init_repo();
        write(repo.path().join("src/real.rs").as_path(), "base\n");
        git(repo.path(), &["add", "src/real.rs"]);
        git(repo.path(), &["commit", "-m", "add real"]);
        write(repo.path().join("src/real.rs").as_path(), "base\nedited\n");
        let snapshot = load_review_snapshot(repo.path());
        let real = snapshot.files.iter().find(|f| f.path == "src/real.rs").expect("real file").clone();
        let diff = FileDiff::load(repo.path(), &ReviewScope::WorkingTree, &real);
        assert!(diff.error.is_none(), "{:?}", diff.error);
        assert!(diff.lines.iter().any(|l| l.text == "edited"));
        let page = diff.slice_page(&snapshot.snapshot_id, 0, 100).expect("page");
        assert!(page.lines.iter().any(|l| l.text == "edited"));
    }

    #[test]
    fn load_file_diff_rejects_traversal_path_fail_closed() {
        let repo = init_repo();
        write(repo.path().join("real.txt").as_path(), "base\n");
        git(repo.path(), &["add", "real.txt"]);
        git(repo.path(), &["commit", "-m", "add real"]);
        write(repo.path().join("real.txt").as_path(), "base\nedited\n");
        // A malicious file entry whose raw path traverses outside the repo.
        // normalize_repo_path collapses ".." to "etc/passwd"; the loader never
        // reads outside the work tree and git diff reports no such change.
        let evil = DiffFile { path: "../etc/passwd".into(), previous_path: None, status: FileStatus::Modified, binary: false, insertions: 0, deletions: 0, hunks: Vec::new(), truncated: false, message: None };
        let evil_diff = FileDiff::load(repo.path(), &ReviewScope::WorkingTree, &evil);
        assert!(evil_diff.error.is_some() || evil_diff.lines.is_empty(), "traversal path did not fail closed: {:?}", evil_diff);
        // The traversal must never create a file outside the repo.
        assert!(!repo.path().join("../etc/passwd").exists(), "traversal escaped the repo root");
    }

    #[test]
    fn load_file_diff_loads_single_file_after_global_truncation() {
        // Build a repo whose HEAD→working-tree diff exceeds the 2 MiB global
        // snapshot cap, so the snapshot is truncated, then verify the paging
        // loader still loads the big file's full diff page by page.
        let repo = init_repo();
        let big_path = "big.txt";
        let baseline = "base\n".repeat(64);
        write(repo.path().join(big_path).as_path(), &baseline);
        git(repo.path(), &["add", big_path]);
        git(repo.path(), &["commit", "-m", "big baseline"]);
        // ~3 MiB of working-tree additions → total diff > 2 MiB global cap.
        let changed = "changed-line\n".repeat(250_000);
        write(repo.path().join(big_path).as_path(), &changed);

        let snapshot = load_review_snapshot(repo.path());
        assert!(snapshot.truncated, "snapshot should be globally truncated: {:?}", snapshot.error);
        let big = snapshot.files.iter().find(|f| f.path == big_path).expect("big file").clone();

        let diff = FileDiff::load(repo.path(), &ReviewScope::WorkingTree, &big);
        assert!(diff.error.is_none(), "{:?}", diff.error);
        assert!(!diff.binary);
        assert!(
            diff.lines.len() > MAX_FILE_RENDER_LINES,
            "paging loader should recover the full file: got {} lines",
            diff.lines.len()
        );
        // Page through the whole file; the paged line count must equal the
        // full file and the final page has no next cursor.
        let mut cursor = 0usize;
        let mut pages = 0usize;
        let mut seen = 0usize;
        loop {
            let page = diff.slice_page(&snapshot.snapshot_id, cursor, MAX_FILE_PAGE_LINES).expect("page");
            assert_eq!(page.cursor, cursor);
            seen += page.lines.len();
            pages += 1;
            match page.next_cursor {
                Some(next) => cursor = next,
                None => break,
            }
            if pages > 1000 {
                panic!("paging did not terminate");
            }
        }
        assert_eq!(seen, diff.lines.len(), "paged lines must equal the full file");
        assert!(pages > 1, "the big file must require more than one page");
    }

    #[test]
    fn load_file_diff_handles_rename_and_binary_entries() {
        let repo = init_repo();
        // A multi-line baseline so a small append keeps git's rename
        // similarity above the default 50% threshold (a tiny single-line
        // file with a content edit drops below it and reads as delete+add).
        let baseline = "alpha\nbeta\ngamma\ndelta\nepsilon\nzeta\neta\ntheta\n";
        write(repo.path().join("old.txt").as_path(), baseline);
        git(repo.path(), &["add", "old.txt"]);
        git(repo.path(), &["commit", "-m", "add old"]);
        fs::rename(repo.path().join("old.txt"), repo.path().join("new.txt")).expect("rename");
        write(repo.path().join("new.txt").as_path(), &format!("{baseline}renamed\n"));
        // A binary file (fake PNG header) added in the working tree.
        let png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x00IHDR\x00\x00\x00\x00";
        fs::write(repo.path().join("img.png"), png).expect("write png");
        git(repo.path(), &["add", "img.png"]);

        let snapshot = load_review_snapshot(repo.path());
        let renamed = snapshot.files.iter().find(|f| f.path == "new.txt").expect("renamed").clone();
        assert_eq!(renamed.status, FileStatus::Renamed);
        let rename_diff = FileDiff::load(repo.path(), &ReviewScope::WorkingTree, &renamed);
        assert!(rename_diff.error.is_none(), "{:?}", rename_diff.error);
        assert!(rename_diff.lines.iter().any(|l| l.text == "renamed"), "rename diff lost content: {:?}", rename_diff.lines);
        assert_eq!(rename_diff.previous_path.as_deref(), Some("old.txt"));

        let binary = snapshot.files.iter().find(|f| f.path == "img.png").expect("binary").clone();
        assert!(binary.binary, "snapshot should mark the png binary");
        let binary_diff = FileDiff::load(repo.path(), &ReviewScope::WorkingTree, &binary);
        assert!(binary_diff.error.is_none(), "{:?}", binary_diff.error);
        assert!(binary_diff.binary, "paging loader should preserve binary flag");
        let binary_page = binary_diff.slice_page(&snapshot.snapshot_id, 0, 100).expect("binary page");
        assert!(binary_page.binary);
        assert!(binary_page.lines.is_empty(), "binary file has no text lines");
    }

    #[test]
    fn load_file_diff_two_revision_scope_paginates_single_file() {
        let repo = init_repo();
        git(repo.path(), &["branch", "review-base"]);
        write(repo.path().join("paged.txt").as_path(), "base\n");
        git(repo.path(), &["add", "paged.txt"]);
        git(repo.path(), &["commit", "-m", "add paged"]);
        git(repo.path(), &["branch", "review-target"]);
        let scope = ReviewScope::Revisions { from: "review-base".to_owned(), to: "review-target".to_owned() };
        let snapshot = load_review_snapshot_for(repo.path(), scope.clone());
        let file = snapshot.files.iter().find(|f| f.path == "paged.txt").expect("paged").clone();
        let diff = FileDiff::load(repo.path(), &scope, &file);
        assert!(diff.error.is_none(), "{:?}", diff.error);
        let page = diff.slice_page(&snapshot.snapshot_id, 0, 100).expect("page");
        assert!(page.lines.iter().any(|l| l.text == "base"));
        assert!(!page.has_more);
    }

    // ---- Complete changed-file catalog across global truncation ----

    #[test]
    fn name_status_catalog_parses_statuses_and_rename_provenance() {
        let stdout =
            b"M\0src/a.rs\0A\0new.txt\0D\0gone.txt\0R100\0old.txt\0renamed.txt\0C50\0src/orig.rs\0src/copy.rs\0T\0typechange.txt\0";
        let catalog = name_status_catalog(stdout);
        assert_eq!(catalog.len(), 6);
        assert_eq!(catalog["src/a.rs"].status, FileStatus::Modified);
        assert_eq!(catalog["src/a.rs"].previous_path, None);
        assert_eq!(catalog["new.txt"].status, FileStatus::Added);
        assert_eq!(catalog["gone.txt"].status, FileStatus::Deleted);
        let renamed = &catalog["renamed.txt"];
        assert_eq!(renamed.status, FileStatus::Renamed);
        assert_eq!(renamed.previous_path.as_deref(), Some("old.txt"));
        let copied = &catalog["src/copy.rs"];
        assert_eq!(copied.status, FileStatus::Copied);
        assert_eq!(copied.previous_path.as_deref(), Some("src/orig.rs"));
        assert_eq!(catalog["typechange.txt"].status, FileStatus::Modified);
    }

    #[test]
    fn truncated_combined_patch_backfills_catalog_placeholders() {
        let patch = b"diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,3 +1,4 @@\n keep\n-old\n+new\n+added\n context\ndiff --git a/old2.txt b/new2.txt\nsimilarity index 100%\nrename from old2.txt\nrename to new2.txt\ndiff --git a/src/b.rs b/src/b.rs\n--- a/src/b.rs\n+++ b/src/b.rs\n@@ -1,3 +1,3 @@\n-keep\n+gone\n".to_vec();
        let output = GitOutput {
            stdout: patch,
            truncated: true,
            error: None,
        };
        let catalog = GitOutput {
            stdout: b"M\0src/a.rs\0M\0src/b.rs\0R100\0old2.txt\0new2.txt\0D\0old2.txt\0A\0added.txt\0D\0deleted.txt\0".to_vec(),
            truncated: false,
            error: None,
        };
        let snapshot = snapshot_from_diff_output(
            PathBuf::from("repo"),
            ReviewScope::WorkingTree,
            b"working-tree",
            output,
            Some(&catalog),
        );
        assert!(snapshot.truncated);
        let paths = snapshot
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            ["src/a.rs", "new2.txt", "src/b.rs", "added.txt", "deleted.txt"]
        );
        // Parsed entries keep hunks, status, and provenance untouched.
        let a = &snapshot.files[0];
        assert_eq!(a.path, "src/a.rs");
        assert!(!a.truncated);
        assert_eq!(a.insertions, 2);
        assert_eq!(a.deletions, 1);
        assert_eq!(a.hunks.len(), 1);
        let renamed = &snapshot.files[1];
        assert_eq!(renamed.path, "new2.txt");
        assert_eq!(renamed.status, FileStatus::Renamed);
        assert_eq!(renamed.previous_path.as_deref(), Some("old2.txt"));
        assert!(!renamed.truncated, "fully parsed rename keeps its parsed state");
        // The last parsed file carries the partial-body marker.
        let b = &snapshot.files[2];
        assert!(b.truncated);
        assert_eq!(b.message.as_deref(), Some("diff truncated by size limit"));
        assert_eq!(b.hunks.len(), 1);
        // Absent catalogued paths become on-demand placeholders.
        let added = &snapshot.files[3];
        assert_eq!(added.path, "added.txt");
        assert_eq!(added.status, FileStatus::Added);
        assert_eq!(added.previous_path, None);
        assert!(added.hunks.is_empty());
        assert!(added.truncated);
        assert_eq!(added.message.as_deref(), Some(PLACEHOLDER_MESSAGE));
        let deleted = &snapshot.files[4];
        assert_eq!(deleted.status, FileStatus::Deleted);
        assert!(deleted.hunks.is_empty());
        assert!(deleted.truncated);
        assert_eq!(deleted.message.as_deref(), Some(PLACEHOLDER_MESSAGE));
        // A rename source only ever appears as provenance, never as a
        // placeholder, even when the catalog lists it separately.
        assert!(!snapshot.files.iter().any(|file| file.path == "old2.txt"));
    }

    #[test]
    fn truncated_snapshot_identity_includes_catalog_bytes() {
        let patch = || GitOutput {
            stdout: b"diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-old\n+new\n".to_vec(),
            truncated: true,
            error: None,
        };
        let catalog_a = GitOutput {
            stdout: b"A\0a.txt\0M\0b.txt\0".to_vec(),
            truncated: false,
            error: None,
        };
        let catalog_b = GitOutput {
            stdout: b"A\0a.txt\0M\0b.txt\0A\0c.txt\0".to_vec(),
            truncated: false,
            error: None,
        };
        let snapshot_a = snapshot_from_diff_output(
            PathBuf::from("repo"),
            ReviewScope::WorkingTree,
            b"working-tree",
            patch(),
            Some(&catalog_a),
        );
        let snapshot_b = snapshot_from_diff_output(
            PathBuf::from("repo"),
            ReviewScope::WorkingTree,
            b"working-tree",
            patch(),
            Some(&catalog_b),
        );
        let snapshot_a_again = snapshot_from_diff_output(
            PathBuf::from("repo"),
            ReviewScope::WorkingTree,
            b"working-tree",
            patch(),
            Some(&catalog_a),
        );
        assert_ne!(
            snapshot_a.snapshot_id, snapshot_b.snapshot_id,
            "a file added past the truncation point must change the snapshot id"
        );
        assert_eq!(
            snapshot_a.snapshot_id, snapshot_a_again.snapshot_id,
            "identical diff and catalog stay idempotent"
        );
    }

    /// Repo whose combined HEAD→working-tree diff exceeds the 2 MiB snapshot
    /// cap. Two ~2.6 MiB single-file diffs (verified path-sorted output:
    /// `big-a.txt` < `big-b.txt` < `rename-new.txt` < `zz-later.txt`) mean the
    /// first file consumes the whole cap, so every later file is a guaranteed
    /// on-demand placeholder regardless of git's exact output order. The
    /// untracked `stray.txt` must never appear.
    fn build_truncated_working_tree_repo() -> TempDir {
        let repo = init_repo();
        let baseline = "alpha\nbeta\ngamma\ndelta\nepsilon\nzeta\neta\ntheta\n";
        write(repo.path().join("big-a.txt").as_path(), "base\n");
        write(repo.path().join("big-b.txt").as_path(), "base\n");
        write(repo.path().join("rename-old.txt").as_path(), baseline);
        write(repo.path().join("zz-later.txt").as_path(), "base\n");
        git(repo.path(), &["add", "-A"]);
        git(repo.path(), &["commit", "-m", "truncation baseline"]);
        // ~2.6 MiB of additions per big file → each single-file diff exceeds
        // the 2 MiB combined-patch cap.
        let changed = "changed-line\n".repeat(200_000);
        write(repo.path().join("big-a.txt").as_path(), &changed);
        write(repo.path().join("big-b.txt").as_path(), &changed);
        fs::rename(repo.path().join("rename-old.txt"), repo.path().join("rename-new.txt"))
            .expect("rename");
        // A small content edit keeps the rename paired (above the 50%
        // similarity threshold) while giving the loaded diff hunk lines.
        write(
            repo.path().join("rename-new.txt").as_path(),
            &format!("{baseline}renamed\n"),
        );
        write(repo.path().join("zz-later.txt").as_path(), "base\nchanged\n");
        write(repo.path().join("stray.txt").as_path(), "untracked, never reviewed\n");
        repo
    }

    #[test]
    fn load_review_snapshot_truncated_patch_lists_every_changed_file() {
        let repo = build_truncated_working_tree_repo();
        let index = repo.path().join(".git/index");
        let index_before = fs::read(&index).expect("index before");

        let snapshot = load_review_snapshot(repo.path());

        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        assert!(snapshot.truncated, "combined patch must exceed the 2 MiB cap");
        let paths = snapshot
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>();
        for expected in ["big-a.txt", "big-b.txt", "rename-new.txt", "zz-later.txt"] {
            assert!(
                paths.contains(&expected),
                "changed file missing after truncation: {paths:?}"
            );
        }
        assert!(!paths.contains(&"stray.txt"), "untracked file must stay excluded: {paths:?}");
        // The first big file parses partially; the second big file, the rename
        // and the later file become on-demand placeholders.
        let parsed = snapshot
            .files
            .iter()
            .filter(|file| !file.hunks.is_empty())
            .collect::<Vec<_>>();
        assert_eq!(parsed.len(), 1, "{paths:?}");
        assert!(parsed[0].truncated, "partially parsed file must carry the marker");
        let placeholders = snapshot
            .files
            .iter()
            .filter(|file| file.hunks.is_empty())
            .collect::<Vec<_>>();
        assert_eq!(placeholders.len(), 3, "{paths:?}");
        for placeholder in placeholders {
            assert!(placeholder.truncated, "placeholder must be marked truncated");
            assert_eq!(placeholder.message.as_deref(), Some(PLACEHOLDER_MESSAGE));
        }
        let renamed = snapshot
            .files
            .iter()
            .find(|file| file.path == "rename-new.txt")
            .expect("rename placeholder");
        assert_eq!(renamed.status, FileStatus::Renamed);
        assert_eq!(renamed.previous_path.as_deref(), Some("rename-old.txt"));

        // Every placeholder loads a readable bounded page through the paging
        // loader, including the truncated big file and the rename.
        let scope = ReviewScope::WorkingTree;
        for file in snapshot.files.iter().filter(|file| file.hunks.is_empty()) {
            let diff = FileDiff::load(repo.path(), &scope, file);
            assert!(diff.error.is_none(), "{}: {:?}", file.path, diff.error);
            let page = diff.slice_page(&snapshot.snapshot_id, 0, 100).expect("page");
            assert_eq!(page.path, file.path, "page serves the requested file");
            if file.path.starts_with("big-") {
                assert!(
                    diff.lines.len() > MAX_FILE_RENDER_LINES,
                    "big placeholder must recover the full body: {} lines",
                    diff.lines.len()
                );
            } else {
                assert!(!diff.lines.is_empty(), "small placeholder must load its lines");
            }
        }
        assert_eq!(
            fs::read(&index).expect("index after"),
            index_before,
            "review must never mutate the real index"
        );
    }

    #[test]
    fn load_review_snapshot_revision_truncated_patch_lists_every_changed_file() {
        let repo = init_repo();
        let baseline = "alpha\nbeta\ngamma\ndelta\nepsilon\nzeta\neta\ntheta\n";
        write(repo.path().join("big-a.txt").as_path(), "base\n");
        write(repo.path().join("big-b.txt").as_path(), "base\n");
        write(repo.path().join("rename-old.txt").as_path(), baseline);
        write(repo.path().join("zz-later.txt").as_path(), "base\n");
        git(repo.path(), &["add", "-A"]);
        git(repo.path(), &["commit", "-m", "truncation baseline"]);
        let from = String::from_utf8(git_output(repo.path(), &["rev-parse", "HEAD"]))
            .expect("from hash");
        let from = strip_output_line_ending(&from).to_owned();
        let changed = "changed-line\n".repeat(200_000);
        write(repo.path().join("big-a.txt").as_path(), &changed);
        write(repo.path().join("big-b.txt").as_path(), &changed);
        git(repo.path(), &["mv", "rename-old.txt", "rename-new.txt"]);
        write(
            repo.path().join("rename-new.txt").as_path(),
            &format!("{baseline}renamed\n"),
        );
        write(repo.path().join("zz-later.txt").as_path(), "base\nchanged\n");
        git(repo.path(), &["add", "-A"]);
        git(repo.path(), &["commit", "-m", "truncation changes"]);

        let scope = ReviewScope::Revisions {
            from,
            to: "HEAD".to_owned(),
        };
        let snapshot = load_review_snapshot_for(repo.path(), scope.clone());

        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        assert!(snapshot.truncated, "combined patch must exceed the 2 MiB cap");
        let paths = snapshot
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>();
        for expected in ["big-a.txt", "big-b.txt", "rename-new.txt", "zz-later.txt"] {
            assert!(
                paths.contains(&expected),
                "revision changed file missing after truncation: {paths:?}"
            );
        }
        let placeholders = snapshot
            .files
            .iter()
            .filter(|file| file.hunks.is_empty())
            .collect::<Vec<_>>();
        assert_eq!(placeholders.len(), 3, "{paths:?}");
        for placeholder in placeholders {
            assert!(placeholder.truncated);
            assert_eq!(placeholder.message.as_deref(), Some(PLACEHOLDER_MESSAGE));
        }
        let renamed = snapshot
            .files
            .iter()
            .find(|file| file.path == "rename-new.txt")
            .expect("rename placeholder");
        assert_eq!(renamed.status, FileStatus::Renamed);
        assert_eq!(renamed.previous_path.as_deref(), Some("rename-old.txt"));

        // The revision-scope placeholder loads through the paging loader too.
        for file in snapshot.files.iter().filter(|file| file.hunks.is_empty()) {
            let diff = FileDiff::load(repo.path(), &scope, file);
            assert!(diff.error.is_none(), "{}: {:?}", file.path, diff.error);
            let page = diff.slice_page(&snapshot.snapshot_id, 0, 100).expect("page");
            assert_eq!(page.path, file.path);
            assert!(!diff.lines.is_empty(), "placeholder must load its lines");
        }
    }

    #[cfg(unix)]
    #[test]
    fn revision_catalog_overflow_fails_closed_with_explicit_error() {
        // 620 files under 15 nested 255-byte directories → ~2.4 MiB of
        // `git diff --name-status -z` output, past MAX_CATALOG_BYTES, so the
        // review must fail closed instead of silently omitting files.
        let repo = init_repo();
        let mut dir = String::new();
        for _ in 0..15 {
            dir.push_str(&"d".repeat(255));
            dir.push('/');
        }
        for index in 0..620 {
            write(repo.path().join(format!("{dir}f{index:04}.txt")).as_path(), "base\n");
        }
        git(repo.path(), &["add", "-A"]);
        git(repo.path(), &["commit", "-m", "deep baseline"]);
        let from = String::from_utf8(git_output(repo.path(), &["rev-parse", "HEAD"]))
            .expect("from hash");
        let from = strip_output_line_ending(&from).to_owned();
        for index in 0..620 {
            write(repo.path().join(format!("{dir}f{index:04}.txt")).as_path(), "changed\n");
        }
        git(repo.path(), &["add", "-A"]);
        git(repo.path(), &["commit", "-m", "deep changes"]);

        let snapshot = load_review_snapshot_for(
            repo.path(),
            ReviewScope::Revisions {
                from,
                to: "HEAD".to_owned(),
            },
        );

        let error = snapshot.error.expect("catalog overflow must fail closed");
        assert!(error.contains("catalog"), "{error}");
        assert!(
            snapshot.files.is_empty(),
            "overflow must never silently omit a subset: {:?}",
            snapshot.files
        );
    }

    #[test]
    fn file_tree_default_expanded_and_collapse_hides_children() {
        let snapshot = ReviewSnapshot {
            root: PathBuf::from("repo"),
            scope: ReviewScope::WorkingTree,
            snapshot_id: "snap".to_owned(),
            files: vec![
                diff_file_at("src/a.rs", None, FileStatus::Modified),
                diff_file_at("src/b.rs", None, FileStatus::Added),
                diff_file_at("docs/readme.md", None, FileStatus::Modified),
            ],
            truncated: false,
            error: None,
        };
        let tree = FileTree::from_snapshot(&snapshot);
        // Default expanded: all five rows (src, a.rs, b.rs, docs, readme.md).
        assert_eq!(tree.visible_rows().len(), 5);
        let src_idx = tree.visible_rows().iter().find(|r| tree.nodes[r.node_index].path == "src").expect("src row").node_index;
        let mut collapsed = tree.clone();
        collapsed.set_collapsed(src_idx, true);
        assert!(collapsed.is_collapsed(src_idx));
        assert_eq!(collapsed.visible_rows().len(), 3, "collapsing src hides its two files");
        collapsed.set_collapsed(src_idx, false);
        assert_eq!(collapsed.visible_rows().len(), 5);
        // Toggling a file row is a no-op.
        let file_idx = tree.visible_rows().iter().find(|r| tree.nodes[r.node_index].path == "src/a.rs").expect("file row").node_index;
        let mut toggled = tree.clone();
        toggled.toggle_collapse(file_idx);
        assert_eq!(toggled.visible_rows().len(), 5);
    }
}
