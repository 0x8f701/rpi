//! Captures a single coherent snapshot of Git changes via fixed argv, parses
//! it into typed structures, and never mutates the repository. Bare review is
//! HEAD→working tree (tracked staged + unstaged, plus untracked regular files
//! as Added). One ref is that same live worktree compared to a validated base.
//! Two refs stay an exact commit-to-commit comparison. Stash entries are never
//! part of a working-tree snapshot: they are not live review comparisons.
//! A commit scope is a first-parent diff of one resolved commit and is not a
//! `/code-review` operand.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

/// Hard cap on combined git stdout for the review snapshot.
pub const MAX_DIFF_BYTES: usize = 2 * 1024 * 1024;
/// Hard cap on the untracked-file enumeration
/// (`git ls-files --others --exclude-standard -z`). A listing past this
/// bound fails closed instead of silently dropping untracked files.
pub const MAX_UNTRACKED_CATALOG_BYTES: usize = 2 * 1024 * 1024;
/// Hard cap on how many untracked files can enter one snapshot. Each needs a
/// separate bounded `git diff --no-index` invocation, so the count is capped.
pub const MAX_UNTRACKED_FILES: usize = 1_000;
/// Combined byte budget for all untracked file patches in one snapshot.
pub const MAX_UNTRACKED_DIFF_BYTES: usize = 2 * 1024 * 1024;
pub(crate) const MAX_UNTRACKED_PINNED_BYTES: usize = 8 * 1024 * 1024;
const MAX_UNTRACKED_PLACEHOLDER_SAMPLE_BYTES: usize = 64 * 1024;

/// Hard cap on the changed-file catalog (`git diff --name-status -z`) that
/// backfills files a truncated combined patch could not carry. A catalog past
/// this bound means the change set itself is beyond reviewable size: the
/// snapshot fails closed instead of silently omitting files.
pub const MAX_CATALOG_BYTES: usize = 2 * 1024 * 1024;
/// Soft cap on rendered lines per file (binary/oversize still shown as markers).
pub const MAX_FILE_RENDER_LINES: usize = 4_000;
/// Lines loaded by one GitHub-style context expand click.
pub const MAX_CONTEXT_EXPAND_LINES: u32 = 20;
/// Git's binary probe window (`xdiff-interface.c` `FIRST_FEW_BYTES`): a NUL
/// byte within the first 8000 bytes classifies a blob as binary; a NUL past
/// that window is diffed as text. Untracked-file classification must match.
const GIT_BINARY_FIRST_FEW_BYTES: usize = 8000;
const GIT_TIMEOUT: Duration = Duration::from_secs(15);
/// Total bound on collecting both bounded readers' results once git has
/// exited. The deadline is created once after git's status is observed and
/// shared by the sequential stdout/stderr receives, so the second stream gets
/// only the remaining time and the whole collection stays within this single
/// ceiling instead of approaching two of them.
const READER_RESULT_DEADLINE: Duration = Duration::from_secs(3);
const MAX_GIT_METADATA_BYTES: usize = 64 * 1024;
const MAX_SHARED_INDEX_FILES: usize = 128;

/// How a path changed relative to the review comparison source.
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

    /// True when the parsed hunk body matches its header counts. Git emits
    /// exact counts (`old_count = deletions + context`,
    /// `new_count = additions + context`), so a fully received hunk always
    /// satisfies `content_lines == old_count + new_count - context_lines`
    /// while a hunk cut mid-body by a byte cap never does. Meta lines
    /// (`\ No newline at end of file`) are emitted after the body and are
    /// not counted by git, so they neither help nor hurt the check.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        let mut content = 0usize;
        let mut context = 0usize;
        for line in &self.lines {
            match line.kind {
                DiffLineKind::Context => {
                    content += 1;
                    context += 1;
                }
                DiffLineKind::Addition | DiffLineKind::Deletion => content += 1,
                DiffLineKind::Meta => {}
            }
        }
        content == (self.old_count as usize + self.new_count as usize).saturating_sub(context)
    }
}

/// Unchanged region between (or around) unified-diff hunks in old/new line space.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffGap {
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
}

impl DiffGap {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.old_count == 0 && self.new_count == 0
    }

    #[must_use]
    pub fn id(&self) -> String {
        format!(
            "g:{}:{}:{}:{}",
            self.old_start, self.old_count, self.new_start, self.new_count
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GapExpandFrom {
    Start,
    End,
}

/// Gaps before the first hunk, between hunks, and after the last hunk.
/// `old_line_count`/`new_line_count` of 0 omit the trailing (after-last) gap
/// when that side has no known file length; prefix and between gaps still
/// use hunk ranges.
#[must_use]
pub fn review_diff_gaps(hunks: &[DiffHunk], old_line_count: u32, new_line_count: u32) -> Vec<DiffGap> {
    let mut gaps = Vec::new();
    if hunks.is_empty() {
        let old_to = if old_line_count == 0 {
            1
        } else {
            old_line_count.saturating_add(1)
        };
        let new_to = if new_line_count == 0 {
            1
        } else {
            new_line_count.saturating_add(1)
        };
        push_line_gap(&mut gaps, 1, old_to, 1, new_to);
        return gaps;
    }
    let first = &hunks[0];
    push_line_gap(&mut gaps, 1, first.old_start, 1, first.new_start);
    for pair in hunks.windows(2) {
        let prev = &pair[0];
        let next = &pair[1];
        push_line_gap(
            &mut gaps,
            hunk_end(prev.old_start, prev.old_count),
            next.old_start,
            hunk_end(prev.new_start, prev.new_count),
            next.new_start,
        );
    }
    if old_line_count > 0 || new_line_count > 0 {
        let last = hunks.last().expect("non-empty hunks");
        let (old_from, old_to) = trailing_edge(last.old_start, last.old_count, old_line_count);
        let (new_from, new_to) = trailing_edge(last.new_start, last.new_count, new_line_count);
        push_line_gap(&mut gaps, old_from, old_to, new_from, new_to);
    }
    gaps
}

fn hunk_end(start: u32, count: u32) -> u32 {
    if count == 0 {
        start
    } else {
        start.saturating_add(count)
    }
}

fn trailing_edge(start: u32, count: u32, line_count: u32) -> (u32, u32) {
    let from = hunk_end(start, count);
    if line_count == 0 {
        (from, from)
    } else {
        (from, line_count.saturating_add(1))
    }
}

fn push_line_gap(
    gaps: &mut Vec<DiffGap>,
    old_from: u32,
    old_to: u32,
    new_from: u32,
    new_to: u32,
) {
    let old_count = old_to.saturating_sub(old_from);
    let new_count = new_to.saturating_sub(new_from);
    if old_count == 0 && new_count == 0 {
        return;
    }
    gaps.push(DiffGap {
        old_start: if old_count == 0 { 0 } else { old_from },
        old_count,
        new_start: if new_count == 0 { 0 } else { new_from },
        new_count,
    });
}

/// How many lines have been expanded from each end of a gap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GapExpandState {
    pub gap: DiffGap,
    pub from_start: u32,
    pub from_end: u32,
}

impl GapExpandState {
    #[must_use]
    pub fn new(gap: DiffGap) -> Self {
        Self {
            gap,
            from_start: 0,
            from_end: 0,
        }
    }

    #[must_use]
    pub fn remaining(&self) -> DiffGap {
        let consumed = self.from_start.saturating_add(self.from_end);
        let old_count = self.gap.old_count.saturating_sub(consumed.min(self.gap.old_count));
        let new_count = self.gap.new_count.saturating_sub(consumed.min(self.gap.new_count));
        DiffGap {
            old_start: if old_count == 0 {
                0
            } else {
                self.gap.old_start.saturating_add(self.from_start)
            },
            old_count,
            new_start: if new_count == 0 {
                0
            } else {
                self.gap.new_start.saturating_add(self.from_start)
            },
            new_count,
        }
    }

    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.remaining().is_empty()
    }

    pub fn record(&mut self, from: GapExpandFrom, count: u32) {
        let remaining = self.remaining();
        let span = remaining.new_count.max(remaining.old_count);
        let count = count.min(span);
        match from {
            GapExpandFrom::Start => self.from_start = self.from_start.saturating_add(count),
            GapExpandFrom::End => self.from_end = self.from_end.saturating_add(count),
        }
    }
}

/// One bounded window inside a remaining gap (`from` = toward start or end).
#[must_use]
pub fn gap_expand_window(
    remaining: &DiffGap,
    from: GapExpandFrom,
    take: u32,
) -> Option<(u32, u32, u32)> {
    if remaining.is_empty() {
        return None;
    }
    let take = take.clamp(1, MAX_FILE_RENDER_LINES as u32);
    let span = remaining.new_count.max(remaining.old_count);
    let count = take.min(span);
    let (old_start, new_start) = match from {
        GapExpandFrom::Start => (remaining.old_start, remaining.new_start),
        GapExpandFrom::End => {
            let old_start = if remaining.old_count == 0 {
                0
            } else {
                remaining
                    .old_start
                    .saturating_add(remaining.old_count.saturating_sub(count))
            };
            let new_start = if remaining.new_count == 0 {
                0
            } else {
                remaining
                    .new_start
                    .saturating_add(remaining.new_count.saturating_sub(count))
            };
            (old_start, new_start)
        }
    };
    Some((old_start, new_start, count))
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
    pub truncated: bool,
    pub message: Option<String>,
}

impl DiffFile {
    #[must_use]
    pub fn display_path(&self) -> &str {
        &self.path
    }

    #[must_use]
    pub fn body_state(&self) -> &'static str {
        if self.binary {
            "binary"
        } else if self.message.as_deref() == Some(PER_FILE_BYTE_CAP_MESSAGE) {
            "byteCapped"
        } else if self.hunks.iter().any(|hunk| !hunk.is_complete())
            || self.message.as_deref() == Some(PREVIEW_CUT_MESSAGE)
        {
            "preview"
        } else if self.truncated {
            "byteCapped"
        } else if self.hunks.is_empty() && self.message.as_deref() == Some(PLACEHOLDER_MESSAGE) {
            "onDemand"
        } else {
            "complete"
        }
    }

    #[must_use]
    pub fn hunk_is_comment_ready(&self, index: usize) -> bool {
        self.hunks.get(index).is_some_and(DiffHunk::is_complete)
            && !(self.truncated && index + 1 == self.hunks.len())
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

    #[must_use]
    pub fn displayed_change_stats(&self) -> (usize, usize) {
        if self.hunks.is_empty() || self.truncated {
            return (self.insertions, self.deletions);
        }
        let mut insertions = 0usize;
        let mut deletions = 0usize;
        for hunk in &self.hunks {
            for line in &hunk.lines {
                match line.kind {
                    DiffLineKind::Addition => insertions += 1,
                    DiffLineKind::Deletion => deletions += 1,
                    DiffLineKind::Context | DiffLineKind::Meta => {}
                }
            }
        }
        (insertions, deletions)
    }
}

/// Source and target rendered by the code-review page.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ReviewScope {
    #[default]
    WorkingTree,
    BaseToWorkingTree {
        base: String,
    },
    Revisions {
        from: String,
        to: String,
    },
    /// One commit, resolved once to an immutable id. Refresh reuses that id.
    Commit {
        commit: String,
        label: String,
    },
}

impl ReviewScope {
    pub const USAGE: &'static str = "Usage: /code-review [<base> | <from> <to>]";

    pub fn parse(argument: Option<&str>) -> Result<Self, String> {
        let revisions = argument
            .into_iter()
            .flat_map(str::split_whitespace)
            .collect::<Vec<_>>();
        match revisions.as_slice() {
            [] => Ok(Self::WorkingTree),
            [base] => Ok(Self::BaseToWorkingTree {
                base: (*base).to_owned(),
            }),
            [from, to] => Ok(Self::Revisions {
                from: (*from).to_owned(),
                to: (*to).to_owned(),
            }),
            _ => Err(Self::USAGE.to_owned()),
        }
    }

    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::WorkingTree => "HEAD → working tree".to_owned(),
            Self::BaseToWorkingTree { base } => {
                format!("{} → working tree", sanitize_display(base))
            }
            Self::Revisions { from, to } => {
                format!("{} → {}", sanitize_display(from), sanitize_display(to))
            }
            Self::Commit { label, .. } => label.clone(),
        }
    }

    #[must_use]
    pub fn includes_working_tree(&self) -> bool {
        matches!(self, Self::WorkingTree | Self::BaseToWorkingTree { .. })
    }

    fn identity_hint(&self) -> String {
        match self {
            Self::WorkingTree => "working-tree".to_owned(),
            Self::BaseToWorkingTree { base } => format!("base-to-working-tree\0{base}"),
            Self::Revisions { from, to } => format!("revisions\0{from}\0{to}"),
            Self::Commit { commit, .. } => format!("commit\0{commit}"),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ReviewPagingState {
    working_tree: Option<Arc<WorkingTreePagingAuthority>>,
    revisions: Option<PinnedRevisions>,
    paths: BTreeMap<String, ReviewPathBytes>,
    submodule_files: BTreeMap<String, SubmodulePagingFile>,
}

#[derive(Debug)]
struct WorkingTreePagingAuthority {
    root: PathBuf,
    sandbox: GitSandbox,
    base: String,
    untracked: BTreeMap<String, UntrackedPagingBody>,
    submodule_files: BTreeMap<String, SubmodulePagingFile>,
}

#[derive(Clone, Debug)]
struct PinnedRevisions {
    from: String,
    to: String,
}

#[derive(Clone, Debug)]
struct ReviewPathBytes {
    path: Vec<u8>,
    previous_path: Option<Vec<u8>>,
}

#[derive(Debug)]
struct SubmodulePagingAuthority {
    sandbox: IsolatedGitSandbox,
    base: String,
    end: Option<String>,
}

#[derive(Clone, Debug)]
struct SubmodulePagingFile {
    authority: Arc<SubmodulePagingAuthority>,
    path: ReviewPathBytes,
}

impl ReviewPagingState {
    #[must_use]
    pub(crate) fn is_bound(&self) -> bool {
        self.working_tree.is_some() || self.revisions.is_some()
    }

    #[must_use]
    fn working_tree_index(&self) -> Option<PathBuf> {
        self.working_tree
            .as_ref()
            .map(|authority| authority.sandbox.index_path())
    }

    #[must_use]
    fn path_bytes(&self, file: &DiffFile) -> ReviewPathBytes {
        self.paths
            .get(&file.path)
            .cloned()
            .unwrap_or_else(|| ReviewPathBytes {
                path: decode_path_bytes(&file.path),
                previous_path: file.previous_path.as_deref().map(decode_path_bytes),
            })
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
    pub total_insertions: usize,
    pub total_deletions: usize,
    pub summary_complete: bool,
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
            None,
            None,
        );
        Self {
            root,
            scope,
            snapshot_id,
            files: Vec::new(),
            truncated: false,
            total_insertions: 0,
            total_deletions: 0,
            summary_complete: false,
            error: Some(error),
        }
    }

    #[must_use]
    pub fn comparison_label(&self) -> String {
        self.scope.label()
    }

    #[must_use]
    pub fn total_insertions(&self) -> usize {
        self.total_insertions
    }

    #[must_use]
    pub fn total_deletions(&self) -> usize {
        self.total_deletions
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
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(rename_all = "camelCase")]
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

#[cfg(unix)]
fn root_identity_units(root: &Path) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;
    root.as_os_str().as_bytes()
}

#[cfg(windows)]
fn root_identity_units(root: &Path) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    let mut units = Vec::new();
    for unit in root.as_os_str().encode_wide() {
        units.extend_from_slice(&unit.to_le_bytes());
    }
    units
}

/// Stable snapshot identity. `catalog` is the raw changed-file catalog bytes
/// (`git diff --name-status -z` output); hashing it keeps the stale guard
/// sensitive to files beyond a truncated patch's cut point — otherwise adding
/// or renaming a file past the 2 MiB window would reuse an old snapshot id
/// even though the placeholder set changed. `index_fingerprint` is the raw
/// `git ls-files -s -z` output for the review paths (mode + blob oid per
/// entry): the name-status catalog records WHICH paths changed, but a content
/// change to a tracked staged addition beyond the truncation cut would
/// otherwise reuse the old id even though the reviewed content changed.
/// `untracked` is the concatenated `--no-index` patches plus bounded
/// path+size+content-sample records for placeholders that omitted a patch, so
/// an oversized untracked file's content or name still flips the id.
/// `None` is used only for error, empty, and two-revision snapshots.
fn review_snapshot_identity(
    root: &Path,
    comparison: &[u8],
    diff: &[u8],
    truncated: bool,
    catalog: Option<&[u8]>,
    index_fingerprint: Option<&[u8]>,
    untracked: Option<&[u8]>,
) -> String {
    /// Frame one segment with its byte length so segment boundaries never
    /// depend on the segment's content.
    fn segment(hasher: &mut Sha256, bytes: &[u8]) {
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }
    fn optional_segment(hasher: &mut Sha256, bytes: Option<&[u8]>, flag: u8) {
        hasher.update([flag]);
        if let Some(bytes) = bytes {
            hasher.update([1]);
            segment(hasher, bytes);
        } else {
            hasher.update([0]);
        }
    }
    let mut hasher = Sha256::new();
    segment(&mut hasher, &root_identity_units(root));
    segment(&mut hasher, comparison);
    segment(&mut hasher, diff);
    optional_segment(&mut hasher, catalog, 0);
    optional_segment(&mut hasher, index_fingerprint, 1);
    optional_segment(&mut hasher, untracked, 2);
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
            let (insertions, deletions) = file.displayed_change_stats();
            nodes.push(FileTreeNode {
                id: format!("file:{rel}"),
                name,
                path: rel.clone(),
                kind: TreeNodeKind::File { file_index },
                depth,
                children: Vec::new(),
                status: Some(file.status),
                insertions,
                deletions,
            });
            if let Some(parent_idx) = parent_idx {
                nodes[parent_idx].children.push(idx);
                let mut walk = Some(parent_idx);
                while let Some(current) = walk {
                    nodes[current].insertions =
                        nodes[current].insertions.saturating_add(insertions);
                    nodes[current].deletions =
                        nodes[current].deletions.saturating_add(deletions);
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

/// Load the selected working-tree, base-to-working-tree, two-revision, or
/// single-commit comparison for `cwd`.
#[must_use]
pub fn load_review_snapshot_for(cwd: &Path, scope: ReviewScope) -> ReviewSnapshot {
    load_review_snapshot_capture(cwd, scope).0
}

pub(crate) fn load_review_snapshot_capture(
    cwd: &Path,
    scope: ReviewScope,
) -> (ReviewSnapshot, ReviewPagingState) {
    let scope = match scope {
        ReviewScope::Commit { commit, label } if label.is_empty() => ReviewScope::Commit {
            label: sanitize_display(&commit),
            commit,
        },
        other => other,
    };
    let repository = match RepositoryLayout::discover(cwd) {
        Ok(repository) => repository,
        Err(error) => {
            return (
                ReviewSnapshot::empty_for_scope_error(
                    cwd.to_path_buf(),
                    scope,
                    format_git_error("not a git repository", &error),
                ),
                ReviewPagingState::default(),
            );
        }
    };
    let root = repository.root.clone();
    if let Err(error) = repository_attributes_are_safe(&repository) {
        return (
            ReviewSnapshot::empty_for_scope_error(root, scope, error),
            ReviewPagingState::default(),
        );
    }

    let head_path = repository.git_dir.join("HEAD");
    let head_reference =
        match read_small_file(&head_path, MAX_GIT_METADATA_BYTES).and_then(|bytes| {
            String::from_utf8(bytes).map_err(|_| "git HEAD is not valid UTF-8".to_owned())
        }) {
            Ok(head) => strip_output_line_ending(&head).to_owned(),
            Err(error) => {
                return (
                    ReviewSnapshot::empty_for_scope_error(root, scope, error),
                    ReviewPagingState::default(),
                );
            }
        };
    let object_format = match detect_object_format(&repository) {
        Ok(format) => format,
        Err(error) => {
            return (
                ReviewSnapshot::empty_for_scope_error(root, scope, error),
                ReviewPagingState::default(),
            );
        }
    };

    let sandbox = match GitSandbox::create(&repository, &object_format, &head_reference) {
        Ok(sandbox) => sandbox,
        Err(error) => {
            return (
                ReviewSnapshot::empty_for_scope_error(root, scope, error),
                ReviewPagingState::default(),
            );
        }
    };
    match scope {
        ReviewScope::WorkingTree | ReviewScope::BaseToWorkingTree { .. } => {
            let (snapshot, base, untracked, submodule_files) =
                load_working_tree_snapshot(root.clone(), &sandbox, scope);
            let paging = ReviewPagingState {
                working_tree: Some(Arc::new(WorkingTreePagingAuthority {
                    root,
                    sandbox,
                    base,
                    untracked,
                    submodule_files,
                })),
                revisions: None,
                paths: BTreeMap::new(),
                submodule_files: BTreeMap::new(),
            };
            (snapshot, paging)
        }
        ReviewScope::Revisions { from, to } => load_revision_snapshot(root, sandbox, from, to),
        ReviewScope::Commit { commit, .. } => load_commit_snapshot(root, sandbox, commit),
    }
}

fn load_revision_snapshot(
    root: PathBuf,
    sandbox: GitSandbox,
    from: String,
    to: String,
) -> (ReviewSnapshot, ReviewPagingState) {
    let scope = ReviewScope::Revisions {
        from: from.clone(),
        to: to.clone(),
    };
    let from_commit = match resolve_review_commit(&root, &from, "source") {
        Ok(commit) => commit,
        Err(error) => {
            return (
                ReviewSnapshot::empty_for_scope_error(root, scope, error),
                ReviewPagingState::default(),
            );
        }
    };
    let to_commit = match resolve_review_commit(&root, &to, "target") {
        Ok(commit) => commit,
        Err(error) => {
            return (
                ReviewSnapshot::empty_for_scope_error(root, scope, error),
                ReviewPagingState::default(),
            );
        }
    };
    load_pinned_revision_diff(root, sandbox, scope, from_commit, to_commit)
}

fn load_pinned_revision_diff(
    root: PathBuf,
    sandbox: GitSandbox,
    scope: ReviewScope,
    from_commit: String,
    to_commit: String,
) -> (ReviewSnapshot, ReviewPagingState) {
    let comparison = match &scope {
        ReviewScope::Commit { commit, .. } => format!("commit\0{commit}\0{from_commit}\0{to_commit}"),
        _ => format!("revisions\0{from_commit}\0{to_commit}"),
    };
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
    let catalog_output =
        match run_git_bounded(&root, &catalog_args, MAX_CATALOG_BYTES, Some(isolated)) {
            Ok(output) if output.error.is_none() && !output.truncated => output,
            Ok(output) => {
                let error = output
                    .error
                    .unwrap_or_else(|| "changed file catalog exceeded the size limit".to_owned());
                return (
                    ReviewSnapshot::empty_for_scope_error(root, scope, error),
                    ReviewPagingState::default(),
                );
            }
            Err(error) => {
                return (
                    ReviewSnapshot::empty_for_scope_error(root, scope, error),
                    ReviewPagingState::default(),
                );
            }
        };

    let numstat_args = [
        "--literal-pathspecs",
        "diff",
        "--numstat",
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
    let numstat_output =
        match run_git_bounded(&root, &numstat_args, MAX_CATALOG_BYTES, Some(isolated)) {
            Ok(output) if output.error.is_none() && !output.truncated => output,
            Ok(output) => {
                let error = output
                    .error
                    .unwrap_or_else(|| "changed-file summary exceeded the size limit".to_owned());
                return (
                    ReviewSnapshot::empty_for_scope_error(root, scope, error),
                    ReviewPagingState::default(),
                );
            }
            Err(error) => {
                return (
                    ReviewSnapshot::empty_for_scope_error(root, scope, error),
                    ReviewPagingState::default(),
                );
            }
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
        Err(error) => {
            return (
                ReviewSnapshot::empty_for_scope_error(root, scope, error),
                ReviewPagingState::default(),
            );
        }
    };
    let (output, expanded_gitlinks, submodule_files) = match expand_revision_gitlink_diffs(
        &root,
        isolated,
        &from_commit,
        &to_commit,
        &output,
    ) {
        Ok(expanded) => expanded,
        Err(error) => {
            return (
                ReviewSnapshot::empty_for_scope_error(root, scope, error),
                ReviewPagingState::default(),
            );
        }
    };
    let catalog = name_status_catalog(&catalog_output.stdout);
    let paging = ReviewPagingState {
        working_tree: None,
        revisions: Some(PinnedRevisions {
            from: from_commit,
            to: to_commit,
        }),
        paths: catalog
            .into_iter()
            .map(|(display, entry)| {
                (
                    display,
                    ReviewPathBytes {
                        path: entry.path_bytes,
                        previous_path: entry.previous_path_bytes,
                    },
                )
            })
            .collect(),
        submodule_files,
    };
    let mut snapshot = snapshot_from_diff_output(
        root,
        scope,
        comparison.as_bytes(),
        output,
        Some(&catalog_output),
        &numstat_output,
        None,
        None,
    );
    snapshot
        .files
        .retain(|file| !expanded_gitlinks.contains(&file.path));
    (snapshot, paging)
}

const COMMIT_LABEL_HEX: usize = 12;
const COMMIT_LIST_SUBJECT_CHARS: usize = 128;

/// Parents of one already-resolved commit. `left` is the first parent, or the
/// empty tree when the commit is a root.
struct CommitParents {
    commit: String,
    parents: Vec<String>,
    left: String,
}

/// One row for a later commit picker. `commit` is the repository's own object
/// id, not a fixed 40-hex assumption.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitReviewEntry {
    pub commit: String,
    pub parent_commit: Option<String>,
    pub parent_count: u32,
    pub subject: String,
    pub author: String,
    pub author_date: String,
}

/// Bounded recent-commit list. `truncated` is set when another row existed
/// past `limit`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitReviewList {
    pub commits: Vec<CommitReviewEntry>,
    pub truncated: bool,
}

fn short_commit_label(id: &str) -> String {
    let hex: String = id.chars().take(COMMIT_LABEL_HEX).collect();
    sanitize_display(&hex)
}

fn commit_comparison_label(parents: &CommitParents) -> String {
    let right = short_commit_label(&parents.commit);
    match parents.parents.as_slice() {
        [] => format!("empty tree → {right}"),
        [parent] => format!("{} → {right}", short_commit_label(parent)),
        [parent, ..] => {
            format!("first parent {} → {right} (merge)", short_commit_label(parent))
        }
    }
}

fn empty_tree_hash(root: &Path) -> Result<String, String> {
    // `hash-object --stdin` hashes the bytes git reads. The bounded runner
    // closes stdin, which is the empty tree, and does not write a repo object
    // (`-w` is absent). The hash is never passed through `^{commit}`.
    let output = run_git_bounded(
        root,
        &["hash-object", "-t", "tree", "--stdin"],
        MAX_GIT_METADATA_BYTES,
        None,
    )?;
    if output.truncated {
        return Err("empty tree hash exceeded the metadata limit".to_owned());
    }
    if let Some(error) = output.error {
        return Err(error);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let hash = strip_output_line_ending(&text);
    if hash.is_empty() {
        return Err("git hash-object returned an empty tree hash".to_owned());
    }
    Ok(hash.to_owned())
}

fn read_commit_parents(root: &Path, commit: &str) -> Result<CommitParents, String> {
    let output = run_git_bounded(
        root,
        &[
            "rev-list",
            "--parents",
            "-n",
            "1",
            "--end-of-options",
            commit,
        ],
        MAX_GIT_METADATA_BYTES,
        None,
    )?;
    if output.truncated {
        return Err("commit parent list exceeded the metadata limit".to_owned());
    }
    if let Some(error) = output.error {
        return Err(error);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let line = strip_output_line_ending(&text);
    let mut ids = line.split_whitespace();
    let Some(resolved) = ids.next() else {
        return Err(format!("commit '{commit}' has no rev-list record"));
    };
    if resolved != commit {
        return Err(format!(
            "commit parent list resolved '{commit}' as '{resolved}'"
        ));
    }
    let parents: Vec<String> = ids.map(str::to_owned).collect();
    let left = match parents.first() {
        Some(parent) => parent.clone(),
        None => empty_tree_hash(root)?,
    };
    Ok(CommitParents {
        commit: commit.to_owned(),
        parents,
        left,
    })
}

fn load_commit_snapshot(
    root: PathBuf,
    sandbox: GitSandbox,
    revision: String,
) -> (ReviewSnapshot, ReviewPagingState) {
    let requested = ReviewScope::Commit {
        commit: revision.clone(),
        label: sanitize_display(&revision),
    };
    let commit = match resolve_review_commit(&root, &revision, "commit") {
        Ok(commit) => commit,
        Err(error) => {
            return (
                ReviewSnapshot::empty_for_scope_error(root, requested, error),
                ReviewPagingState::default(),
            );
        }
    };
    let parents = match read_commit_parents(&root, &commit) {
        Ok(parents) => parents,
        Err(error) => {
            return (
                ReviewSnapshot::empty_for_scope_error(
                    root,
                    ReviewScope::Commit {
                        commit: commit.clone(),
                        label: short_commit_label(&commit),
                    },
                    error,
                ),
                ReviewPagingState::default(),
            );
        }
    };
    load_pinned_revision_diff(
        root,
        sandbox,
        ReviewScope::Commit {
            commit: commit.clone(),
            label: commit_comparison_label(&parents),
        },
        parents.left,
        commit,
    )
}

fn bounded_subject(subject: &str) -> String {
    let sanitized = sanitize_display(subject);
    if sanitized.chars().count() <= COMMIT_LIST_SUBJECT_CHARS {
        return sanitized;
    }
    let mut bounded: String = sanitized.chars().take(COMMIT_LIST_SUBJECT_CHARS).collect();
    bounded.push_str("...");
    bounded
}

fn parse_commit_log_record(fields: &[&str]) -> Result<CommitReviewEntry, String> {
    let [commit, parents_field, subject, author, author_date] = fields else {
        return Err("commit log record does not have five fields".to_owned());
    };
    if commit.is_empty() {
        return Err("commit log record is missing its id".to_owned());
    }
    let parents: Vec<&str> = parents_field.split_whitespace().collect();
    let parent_count = u32::try_from(parents.len()).unwrap_or(u32::MAX);
    Ok(CommitReviewEntry {
        commit: (*commit).to_owned(),
        parent_commit: parents.first().map(|parent| (*parent).to_owned()),
        parent_count,
        subject: bounded_subject(subject),
        author: sanitize_display(author),
        author_date: sanitize_display(author_date),
    })
}

fn parse_commit_log(stdout: &[u8], limit: usize) -> Result<CommitReviewList, String> {
    let text = String::from_utf8_lossy(stdout);
    let mut commits = Vec::new();
    let mut fields: Vec<&str> = Vec::new();
    for field in text.split('\0') {
        if field.is_empty() && fields.is_empty() {
            continue;
        }
        fields.push(field);
        if fields.len() == 5 {
            commits.push(parse_commit_log_record(&fields)?);
            fields.clear();
        }
    }
    if !fields.is_empty() {
        return Err("commit log ended inside a record".to_owned());
    }
    let truncated = commits.len() > limit;
    commits.truncate(limit);
    Ok(CommitReviewList { commits, truncated })
}

/// Recent commits for a later picker, newest first, first-parent history.
///
/// `limit` is clamped to [`crate::file_explorer::MAX_COMPLETION_ENTRIES`]. The
/// reader asks git for `limit + 1` and reports `truncated` when that extra row
/// exists. The command is read-only: no stage, checkout, or worktree write.
#[must_use]
pub fn list_recent_commits(cwd: &Path, limit: usize) -> Result<CommitReviewList, String> {
    let limit = limit
        .min(crate::file_explorer::MAX_COMPLETION_ENTRIES)
        .max(1);
    let repository = RepositoryLayout::discover(cwd)
        .map_err(|error| format_git_error("not a git repository", &error))?;
    let root = repository.root;
    let fetch = limit.saturating_add(1).to_string();
    let output = run_git_bounded(
        &root,
        &[
            "log",
            "--first-parent",
            "-n",
            fetch.as_str(),
            "-z",
            "--pretty=format:%H%x00%P%x00%s%x00%an%x00%aI",
            "--end-of-options",
        ],
        MAX_GIT_METADATA_BYTES,
        None,
    )?;
    if output.truncated {
        return Err("recent commit list exceeded the metadata limit".to_owned());
    }
    if let Some(error) = output.error {
        return Err(error);
    }
    parse_commit_log(&output.stdout, limit)
}

/// Longest revision or workspace text one revision-resolution error echoes.
/// Both are user- or environment-supplied and can be arbitrarily long, and the
/// real cause must stay visible in a toast and in a one-line panel row.
const MAX_ECHOED_CONTEXT_CHARS: usize = 128;

/// `rev-parse` failures where the call never finished inside its budget: the
/// 15 s runner timeout and the shared 3 s reader-collection deadline. A partial
/// (blobless) clone can spend either budget fetching objects. These are this
/// module's own messages, so matching inside them is exact enough.
const INCOMPLETE_CALL_MARKERS: [&str; 2] = ["git timed out after", "remained open after exit"];

/// What git's diagnostic says about `rev-parse --verify <revision>^{commit}`.
enum RevisionResolution {
    /// Git does not know the revision in this workspace.
    Unknown,
    /// Git resolved an object, but it is not a commit.
    NotACommit,
    /// No revision answer at all: timeout, reader deadline, spawn failure, size
    /// limit, unusable workspace, broken configuration, ...
    Other,
}

/// `strip_prefix` that ignores ASCII case: git spells `fatal:` and `error:` in
/// lower case but capitalizes words such as `Needed a single revision`.
fn strip_git_prefix<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    let head = line.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then(|| &line[prefix.len()..])
}

/// `fatal: ambiguous argument '<spec>': unknown revision or path not in the working tree`
fn is_ambiguous_argument(line: &str) -> bool {
    strip_git_prefix(line, "fatal: ambiguous argument '")
        .and_then(|rest| rest.strip_suffix("': unknown revision or path not in the working tree"))
        .is_some_and(|spec| !spec.is_empty())
}

/// `fatal: no such branch: '<name>'`, an unresolvable `@{upstream}` reference.
fn is_missing_branch(line: &str) -> bool {
    strip_git_prefix(line, "fatal: no such branch: '")
        .and_then(|rest| rest.strip_suffix('\''))
        .is_some_and(|name| !name.is_empty())
}

/// `fatal: Not a valid object name <spec>`
fn is_invalid_object_name(line: &str) -> bool {
    strip_git_prefix(line, "fatal: not a valid object name")
        .is_some_and(|spec| !spec.trim().is_empty())
}

/// `error: <spec>: expected commit type, but the object dereferences to <type> type`
fn is_wrong_object_type(line: &str) -> bool {
    strip_git_prefix(line, "error: ")
        .and_then(|rest| rest.split_once(": expected commit type, but the object dereferences to "))
        .is_some_and(|(spec, tail)| !spec.is_empty() && tail.ends_with(" type"))
}

/// Classify one failed `rev-parse` by the COMPLETE shape of its diagnostic.
///
/// Git answers a revision question with a small set of fixed templates, and only
/// those templates establish a class. A value quoted inside an unrelated
/// diagnostic — a repository-local `core.abbrev` spelled `needed a single
/// revision`, say — is data, not an answer about the revision, so every template
/// is anchored at the start of the line and, where git's wording has a fixed
/// tail, at the end of it too. The runner hands over the first non-empty stderr
/// line only.
fn classify_rev_parse_failure(error: &str) -> RevisionResolution {
    let line = error.lines().next().unwrap_or_default().trim();
    let line = line.strip_suffix('.').unwrap_or(line);
    if line.eq_ignore_ascii_case("fatal: Needed a single revision")
        || is_invalid_object_name(line)
        || is_missing_branch(line)
        || is_ambiguous_argument(line)
    {
        return RevisionResolution::Unknown;
    }
    if is_wrong_object_type(line) {
        return RevisionResolution::NotACommit;
    }
    RevisionResolution::Other
}

/// The git call never finished inside its time budget, so a partial (blobless)
/// clone may have spent that budget fetching objects.
fn is_incomplete_call(error: &str) -> bool {
    INCOMPLETE_CALL_MARKERS
        .iter()
        .any(|marker| error.contains(*marker))
}

/// `value` sanitized for display and capped at [`MAX_ECHOED_CONTEXT_CHARS`]
/// characters, with the cut marked so a clipped ref or path cannot pass for the
/// whole of it.
fn bounded_echo(value: &str) -> String {
    let sanitized = sanitize_display(value);
    if sanitized.chars().count() <= MAX_ECHOED_CONTEXT_CHARS {
        return sanitized;
    }
    let mut bounded: String = sanitized.chars().take(MAX_ECHOED_CONTEXT_CHARS).collect();
    bounded.push_str("...");
    bounded
}

/// Message for one failed `git rev-parse --verify`.
///
/// Only git's own revision answers make the invalid revision wording true: there
/// the existing text is kept verbatim (an E2E lane asserts the substring) with
/// the revision and the consulted workspace named alongside, and an object that
/// exists but is not a commit is named as such rather than claimed missing.
/// Every other cause — the runner timeout, the reader-collection deadline, a
/// spawn failure, the metadata size limit, a workspace git cannot open, broken
/// configuration — is reported as the underlying error with `{label}`, the
/// revision, and the workspace kept in frame, so a transient failure is never
/// shown to the user as an invalid revision.
fn rev_parse_failure(error: &str, revision: &str, label: &str, root: &Path) -> String {
    let cause = sanitize_display(error);
    let revision = bounded_echo(revision);
    let workspace = bounded_echo(&root.display().to_string());
    match classify_rev_parse_failure(&cause) {
        RevisionResolution::Unknown => format!(
            "invalid {label} revision; expected a commit, branch, or tag: '{revision}' was not \
             found in {workspace}"
        ),
        RevisionResolution::NotACommit => format!(
            "invalid {label} revision; expected a commit, branch, or tag: '{revision}' does not \
             resolve to a commit in {workspace}"
        ),
        RevisionResolution::Other => {
            let mut message =
                format!("failed to resolve {label} revision '{revision}' in {workspace}: {cause}");
            if is_incomplete_call(&cause) {
                message.push_str(
                    "; a partial (blobless) clone may have been fetching objects over the network, \
                     so retry once that fetch has completed",
                );
            }
            message
        }
    }
}

fn resolve_review_commit(root: &Path, revision: &str, label: &str) -> Result<String, String> {
    let commit = format!("{revision}^{{commit}}");
    git_rev_parse(root, &["--verify", "--end-of-options", &commit], None)
        .map(|output| strip_output_line_ending(&output).to_owned())
        .map_err(|error| rev_parse_failure(&error, revision, label, root))
}

fn working_tree_fail(
    root: PathBuf,
    scope: ReviewScope,
    base: String,
    error: impl Into<String>,
) -> (
    ReviewSnapshot,
    String,
    BTreeMap<String, UntrackedPagingBody>,
    BTreeMap<String, SubmodulePagingFile>,
) {
    (
        ReviewSnapshot::empty_for_scope_error(root, scope, error),
        base,
        BTreeMap::new(),
        BTreeMap::new(),
    )
}

fn working_tree_comparison_key(scope: &ReviewScope, base: &str, head: &str) -> String {
    match scope {
        ReviewScope::WorkingTree => format!("working-tree\0{head}"),
        ReviewScope::BaseToWorkingTree { .. } => {
            format!("base-to-working-tree\0{base}\0{head}")
        }
        ReviewScope::Revisions { from, to } => format!("revisions\0{from}\0{to}"),
        ReviewScope::Commit { commit, .. } => format!("commit\0{commit}"),
    }
}

fn changed_working_tree_paths(
    root: &Path,
    isolated: IsolatedGitEnvironment<'_>,
    base: &str,
) -> Result<(BTreeSet<String>, BTreeSet<OsString>), String> {
    let args = [
        "diff",
        "--name-only",
        "-z",
        "--no-ext-diff",
        "--no-textconv",
        "--no-renames",
        base,
        "--",
    ];
    let output = run_git_bounded(root, &args, MAX_DIFF_BYTES, Some(isolated))?;
    if output.truncated {
        return Err("changed path list exceeded the size limit".to_owned());
    }
    if let Some(error) = output.error {
        return Err(error);
    }
    let mut paths = BTreeSet::new();
    let mut pathspecs = BTreeSet::new();
    for entry in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        paths.insert(normalize_repo_path(&encode_path_bytes(entry)));
        pathspecs.insert(os_string_from_git_path(entry.to_vec()));
    }
    Ok((paths, pathspecs))
}

fn stage_tracked_working_tree_paths(
    root: &Path,
    isolated: IsolatedGitEnvironment<'_>,
    pathspecs: &BTreeSet<OsString>,
) -> Result<(), String> {
    let mut ls_args: Vec<OsString> = vec![
        "--literal-pathspecs".into(),
        "ls-files".into(),
        "-z".into(),
        "--".into(),
    ];
    ls_args.extend(pathspecs.iter().cloned());
    let output = run_git_bounded_os(root, &ls_args, MAX_DIFF_BYTES, Some(isolated))?;
    if output.truncated {
        return Err("tracked path list exceeded the size limit".to_owned());
    }
    if let Some(error) = output.error {
        return Err(error);
    }
    let index_members: Vec<OsString> = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| os_string_from_git_path(entry.to_vec()))
        .collect();
    if index_members.is_empty() {
        return Ok(());
    }
    let mut add_args: Vec<OsString> = vec![
        "--literal-pathspecs".into(),
        "add".into(),
        "-u".into(),
        "--".into(),
    ];
    add_args.extend(index_members);
    let output = run_git_bounded_os(root, &add_args, MAX_GIT_METADATA_BYTES, Some(isolated))?;
    if output.truncated {
        return Err("git add output exceeded the metadata limit".to_owned());
    }
    if let Some(error) = output.error {
        return Err(error);
    }
    Ok(())
}

fn load_working_tree_snapshot(
    root: PathBuf,
    sandbox: &GitSandbox,
    scope: ReviewScope,
) -> (
    ReviewSnapshot,
    String,
    BTreeMap<String, UntrackedPagingBody>,
    BTreeMap<String, SubmodulePagingFile>,
) {
    let work_tree = root.clone();
    let isolated = sandbox.environment(&work_tree);
    let head = match git_rev_parse(&root, &["--verify", "HEAD"], Some(isolated)) {
        Ok(head) => strip_output_line_ending(&head).to_owned(),
        Err(error) => {
            return working_tree_fail(
                root,
                scope,
                String::new(),
                format_git_error("repository has no commits yet (unborn HEAD)", &error),
            );
        }
    };
    let base = match &scope {
        ReviewScope::WorkingTree => head.clone(),
        ReviewScope::BaseToWorkingTree { base } => {
            match resolve_review_commit(&root, base, "source") {
                Ok(base) => base,
                Err(error) => return working_tree_fail(root, scope, String::new(), error),
            }
        }
        ReviewScope::Revisions { .. } | ReviewScope::Commit { .. } => {
            return working_tree_fail(
                root,
                scope,
                String::new(),
                "working-tree capture cannot load a revision scope",
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
                return working_tree_fail(root, scope, base, error);
            }
            Err(error) => return working_tree_fail(root, scope, base, error),
        }
    }

    let (mut reviewable_paths, mut review_path_args) =
        match changed_working_tree_paths(&root, isolated, &base) {
            Ok(paths) => paths,
            Err(error) => return working_tree_fail(root, scope, base, error),
        };

    let comparison = working_tree_comparison_key(&scope, &base, &head);
    if review_path_args.is_empty() {
        let mut pin_index = 0usize;
        let untracked = match untracked_added_files(
            &root,
            isolated,
            &reviewable_paths,
            &sandbox.directory.path.join("untracked"),
            &mut pin_index,
        ) {
            Ok(untracked) => untracked,
            Err(error) => return working_tree_fail(root, scope, base, error),
        };
        let files = untracked.files;
        let patches = untracked.patches;
        let pinned = untracked.pinned;
        let truncated = untracked.truncated;
        let snapshot_id = review_snapshot_identity(
            &root,
            comparison.as_bytes(),
            &patches,
            truncated,
            None,
            None,
            Some(&patches),
        );
        return (
            ReviewSnapshot {
                root,
                scope,
                snapshot_id,
                files,
                truncated,
                total_insertions: untracked.total_insertions,
                total_deletions: untracked.total_deletions,
                summary_complete: true,
                error: None,
            },
            base,
            pinned,
            BTreeMap::new(),
        );
    }
    let (working_tree_gitlinks, gitlink_paths) =
        match working_tree_gitlink_output(&root, isolated, &base, &review_path_args) {
            Ok((output, paths)) => (output, paths),
            Err(error) => return working_tree_fail(root, scope, base, error),
        };

    if let Err(error) = stage_tracked_working_tree_paths(&root, isolated, &review_path_args) {
        return working_tree_fail(root, scope, base, error);
    }

    // Content fingerprint of the disposable index AFTER staging: `-s -z`
    // records are `MODE OID STAGE\0PATH\0` (raw bytes, never C-quoted, so
    // non-UTF-8 and backslash/tab paths round-trip exactly). Hashing these
    // mode+blob oids into the snapshot identity makes it sensitive to review
    // content the name-status catalog (WHICH paths changed) and a truncated
    // patch (content before the 2 MiB cut) cannot carry — e.g. a staged
    // addition whose content changes beyond the cut, staged or unstaged.
    let mut fingerprint_args: Vec<OsString> = vec![
        "--literal-pathspecs".into(),
        "ls-files".into(),
        "-s".into(),
        "-z".into(),
        "--".into(),
    ];
    fingerprint_args.extend(review_path_args.iter().cloned());
    let fingerprint_output =
        match run_git_bounded_os(&root, &fingerprint_args, MAX_DIFF_BYTES, Some(isolated)) {
            Ok(output) if output.error.is_none() && !output.truncated => output,
            Ok(output) => {
                let error = output
                    .error
                    .unwrap_or_else(|| "tracked path list exceeded the size limit".to_owned());
                return working_tree_fail(root, scope, base, error);
            }
            Err(error) => return working_tree_fail(root, scope, base, error),
        };

    // Rename-aware changed-file catalog, scoped to the review pathspecs so a
    // stale real-index entry (a staged addition later deleted from the
    // worktree, or an index entry manually reverted to HEAD content) can never
    // pair with a review-path deletion and sneak an untracked path into the
    // snapshot. The same `--name-status -z` output both extends the review
    // pathspecs with rename destinations and backfills files a truncated
    // combined patch could not carry. The diff options mirror the final patch
    // so rename pairing is identical between the catalog and the parsed diff.
    // A catalog past its bound fails closed instead of silently omitting files.
    let mut rename_args: Vec<OsString> = vec![
        "--literal-pathspecs".into(),
        "diff".into(),
        "--cached".into(),
        "--name-status".into(),
        "-z".into(),
        "--no-ext-diff".into(),
        "--no-textconv".into(),
        "--no-color".into(),
        "--default-prefix".into(),
        "--no-relative".into(),
        "--find-renames".into(),
        "--histogram".into(),
        base.clone().into(),
        "--".into(),
    ];
    rename_args.extend(review_path_args.iter().cloned());
    let rename_output =
        match run_git_bounded_os(&root, &rename_args, MAX_CATALOG_BYTES, Some(isolated)) {
            Ok(output) if output.error.is_none() && !output.truncated => output,
            Ok(output) => {
                let error = output
                    .error
                    .unwrap_or_else(|| "changed file catalog exceeded the size limit".to_owned());
                return working_tree_fail(root, scope, base, error);
            }
            Err(error) => return working_tree_fail(root, scope, base, error),
        };
    for destination in rename_destination_paths(&rename_output.stdout) {
        reviewable_paths.insert(normalize_repo_path(&encode_path_bytes(&destination)));
        review_path_args.insert(os_string_from_git_path(destination));
    }

    let mut numstat_args: Vec<OsString> = vec![
        "--literal-pathspecs".into(),
        "diff".into(),
        "--cached".into(),
        "--numstat".into(),
        "-z".into(),
        "--no-ext-diff".into(),
        "--no-textconv".into(),
        "--no-color".into(),
        "--default-prefix".into(),
        "--no-relative".into(),
        "--find-renames".into(),
        "--histogram".into(),
        base.clone().into(),
        "--".into(),
    ];
    numstat_args.extend(review_path_args.iter().cloned());
    let numstat_output =
        match run_git_bounded_os(&root, &numstat_args, MAX_CATALOG_BYTES, Some(isolated)) {
            Ok(output) if output.error.is_none() && !output.truncated => output,
            Ok(output) => {
                let error = output
                    .error
                    .unwrap_or_else(|| "changed-file summary exceeded the size limit".to_owned());
                return working_tree_fail(root, scope, base, error);
            }
            Err(error) => return working_tree_fail(root, scope, base, error),
        };

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
        base.as_str(),
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
        Err(error) => return working_tree_fail(root, scope, base, error),
    };
    let output = replace_gitlink_blocks(&output, &gitlink_paths, &working_tree_gitlinks, MAX_DIFF_BYTES);

    // The catalog is scoped to review paths and every rename destination is a
    // review path, so parsed files and placeholders always belong to the
    // review. Untracked regular files join the review as Added entries; their
    // paths never appear in the tracked set (a path with both a tracked
    // deletion and an untracked worktree replacement keeps the tracked
    // entry), so the `.files` lists of both sources are disjoint by
    // construction, and their raw patches plus bounded placeholder
    // fingerprints are hashed into the snapshot identity so content or name
    // changes to an untracked file always flip the stale guard.
    let pin_dir = sandbox.directory.path.join("untracked");
    let mut pin_index = 0usize;
    let untracked = match untracked_added_files(
        &root,
        isolated,
        &reviewable_paths,
        &pin_dir,
        &mut pin_index,
    ) {
        Ok(untracked) => untracked,
        Err(error) => return working_tree_fail(root, scope, base, error),
    };
    let (expanded_output, expanded_gitlink_files, expanded_gitlinks, submodule_files, inner_untracked) =
        match expand_gitlink_diffs(
            &output,
            &gitlink_paths,
            &root,
            MAX_DIFF_BYTES,
            Some(&pin_dir),
            &mut pin_index,
        ) {
            Ok((output, files, gitlinks, submodule_files, inner_untracked)) => {
                (output, files, gitlinks, submodule_files, inner_untracked)
            }
            Err(error) => return working_tree_fail(root, scope, base, error),
        };
    reviewable_paths.extend(expanded_gitlink_files.iter().cloned());

    let mut snapshot = snapshot_from_diff_output(
        root,
        scope.clone(),
        comparison.as_bytes(),
        expanded_output,
        Some(&rename_output),
        &numstat_output,
        Some(&fingerprint_output.stdout),
        Some(&untracked.patches),
    );
    snapshot
        .files
        .retain(|file| !expanded_gitlinks.contains(&file.path));
    snapshot
        .files
        .retain(|file| file.status == FileStatus::Renamed || reviewable_paths.contains(&file.path));
    apply_untracked_placeholder_messages(&mut snapshot.files, &inner_untracked);
    snapshot.files.extend(untracked.files);
    snapshot.truncated |= untracked.truncated;
    snapshot.total_insertions += untracked.total_insertions;
    snapshot.total_deletions += untracked.total_deletions;
    let mut pinned = untracked.pinned;
    pinned.extend(inner_untracked);
    (snapshot, base, pinned, submodule_files)
}


fn snapshot_from_diff_output(
    root: PathBuf,
    scope: ReviewScope,
    comparison: &[u8],
    output: GitOutput,
    catalog_output: Option<&GitOutput>,
    numstat_output: &GitOutput,
    index_fingerprint: Option<&[u8]>,
    untracked: Option<&[u8]>,
) -> ReviewSnapshot {
    if let Some(error) = output.error {
        return ReviewSnapshot::empty_for_scope_error(root, scope, error);
    }
    if let Some(error) = &numstat_output.error {
        return ReviewSnapshot::empty_for_scope_error(root, scope, error.clone());
    }
    if numstat_output.truncated {
        return ReviewSnapshot::empty_for_scope_error(
            root,
            scope,
            "changed-file summary exceeded the size limit",
        );
    }
    let numstat = match numstat_entries(&numstat_output.stdout) {
        Ok(numstat) => numstat,
        Err(error) => return ReviewSnapshot::empty_for_scope_error(root, scope, error),
    };
    let catalog = catalog_output.map(|output| name_status_catalog(&output.stdout));
    let catalog_bytes = catalog_output.map(|output| output.stdout.as_slice());
    let snapshot_id = review_snapshot_identity(
        &root,
        comparison,
        &output.stdout,
        output.truncated,
        catalog_bytes,
        index_fingerprint,
        untracked,
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut files = parse_unified_diff(&stdout);
    if output.truncated {
        if let Some(last) = files.last_mut() {
            last.truncated = true;
            if last.message.is_none() {
                last.message = Some(PREVIEW_CUT_MESSAGE.to_owned());
            }
        }
        // Global truncation is recoverable file-body paging, not data loss:
        // every catalogued path the combined patch could not carry is retained
        // as an on-demand placeholder so no changed file silently disappears.
        if let Some(catalog) = catalog {
            merge_catalog_placeholders(&mut files, &catalog);
        }
    }
    apply_numstat(&mut files, &numstat);
    for file in &mut files {
        if file.hunks.is_empty() || file.truncated {
            continue;
        }
        let (insertions, deletions) = file.displayed_change_stats();
        file.insertions = insertions;
        file.deletions = deletions;
    }
    let total_insertions = files.iter().map(|file| file.insertions).sum();
    let total_deletions = files.iter().map(|file| file.deletions).sum();
    ReviewSnapshot {
        root,
        scope,
        snapshot_id,
        files,
        truncated: output.truncated,
        total_insertions,
        total_deletions,
        summary_complete: true,
        error: None,
    }
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
        if status
            .first()
            .is_some_and(|prefix| matches!(prefix, b'R' | b'C'))
        {
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

/// Capture the working-tree diff of every changed gitlink (mode 160000) path
/// and return it together with the exact changed-path authority: the raw
/// `git diff --raw -z` paths whose old or new mode is 160000. Only paths in
/// this set may ever be removed or replaced by gitlink handling; hunk-body
/// text never classifies a block.
fn working_tree_gitlink_output(
    root: &Path,
    isolated: IsolatedGitEnvironment<'_>,
    base: &str,
    review_paths: &BTreeSet<OsString>,
) -> Result<(GitOutput, BTreeSet<OsString>), String> {
    let mut raw_args: Vec<OsString> = vec![
        "--literal-pathspecs".into(),
        "diff".into(),
        "--raw".into(),
        "-z".into(),
        "--abbrev=64".into(),
        "--no-renames".into(),
        "--no-ext-diff".into(),
        "--no-textconv".into(),
        "--ignore-submodules=none".into(),
        base.into(),
        "--".into(),
    ];
    raw_args.extend(review_paths.iter().cloned());
    let raw = run_git_bounded_os(root, &raw_args, MAX_CATALOG_BYTES, Some(isolated))?;
    if let Some(error) = raw.error {
        return Err(format!("listing changed gitlinks: {error}"));
    }
    if raw.truncated {
        return Err("changed gitlink list exceeded the size limit".to_owned());
    }
    let gitlink_paths = changed_gitlink_paths(&raw.stdout)?;
    if gitlink_paths.is_empty() {
        return Ok((
            GitOutput {
                stdout: Vec::new(),
                truncated: false,
                error: None,
            },
            gitlink_paths,
        ));
    }

    let mut diff_args: Vec<OsString> = vec![
        "--literal-pathspecs".into(),
        "diff".into(),
        "--no-ext-diff".into(),
        "--no-textconv".into(),
        "--no-color".into(),
        "--default-prefix".into(),
        "--no-relative".into(),
        "--no-renames".into(),
        "--ignore-submodules=none".into(),
        "--submodule=short".into(),
        "--full-index".into(),
        base.into(),
        "--".into(),
    ];
    diff_args.extend(gitlink_paths.iter().cloned());
    let output = run_git_bounded_os(root, &diff_args, MAX_DIFF_BYTES, Some(isolated))?;
    if let Some(error) = output.error {
        return Err(format!("capturing changed gitlinks: {error}"));
    }
    if output.truncated {
        return Err("changed gitlink diff exceeded the size limit".to_owned());
    }
    Ok((output, gitlink_paths))
}

fn changed_gitlink_paths(raw: &[u8]) -> Result<BTreeSet<OsString>, String> {
    let mut paths = BTreeSet::new();
    let mut fields = raw.split(|byte| *byte == 0).filter(|field| !field.is_empty());
    while let Some(metadata) = fields.next() {
        let path = fields
            .next()
            .ok_or_else(|| "changed gitlink record is missing its path".to_owned())?;
        let mut parts = metadata.split(|byte| byte.is_ascii_whitespace());
        let old_mode = parts
            .next()
            .and_then(|mode| mode.strip_prefix(b":"))
            .ok_or_else(|| "changed gitlink record has no old mode".to_owned())?;
        let new_mode = parts
            .next()
            .ok_or_else(|| "changed gitlink record has no new mode".to_owned())?;
        if old_mode == b"160000" || new_mode == b"160000" {
            paths.insert(os_string_from_git_path(path.to_vec()));
        }
    }
    Ok(paths)
}

fn index_gitlink_paths(
    root: &Path,
    isolated: IsolatedGitEnvironment<'_>,
    pathspecs: &BTreeSet<OsString>,
) -> Result<BTreeSet<OsString>, String> {
    if pathspecs.is_empty() {
        return Ok(BTreeSet::new());
    }
    let mut args: Vec<OsString> = vec![
        "--literal-pathspecs".into(),
        "ls-files".into(),
        "-s".into(),
        "-z".into(),
        "--".into(),
    ];
    args.extend(pathspecs.iter().cloned());
    let output = run_git_bounded_os(root, &args, MAX_CATALOG_BYTES, Some(isolated))?;
    if output.truncated {
        return Err("index gitlink list exceeded the size limit".to_owned());
    }
    if let Some(error) = output.error {
        return Err(error);
    }
    let mut paths = BTreeSet::new();
    for record in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
    {
        let Some(tab) = record.iter().position(|byte| *byte == b'\t') else {
            continue;
        };
        let mode = record[..tab]
            .split(|byte| byte.is_ascii_whitespace())
            .next()
            .unwrap_or(&[]);
        if mode == b"160000" {
            paths.insert(os_string_from_git_path(record[tab + 1..].to_vec()));
        }
    }
    Ok(paths)
}

/// Decode the `diff --git` header paths of one block to raw path bytes. Both
/// sides are decoded with the same quoting rules as parsed diff paths
/// (`split_diff_path_tokens` + `strip_ab_prefix` + `decode_path_bytes`), so a
/// rename/typechange block can match the changed path authority on either
/// side and non-UTF-8 paths compare byte-for-byte against the raw
/// `git diff --raw -z` discovery set. A malformed header yields `None` and the
/// block is preserved byte-for-byte: gitlink handling never drops a block it
/// cannot positively attribute to a changed gitlink path.
fn diff_block_paths(block: &[String]) -> Option<(Vec<u8>, Vec<u8>)> {
    let header = block.first()?;
    let rest = header.strip_prefix("diff --git ")?;
    let tokens = split_diff_path_tokens(rest);
    match tokens.as_slice() {
        [from, to] => Some((
            decode_path_bytes(&strip_ab_prefix(from)),
            decode_path_bytes(&strip_ab_prefix(to)),
        )),
        [single] => {
            let path = decode_path_bytes(&strip_ab_prefix(single));
            Some((path.clone(), path))
        }
        _ => None,
    }
}

/// A block may be removed or replaced only when one of its decoded `diff --git`
/// paths belongs to the changed-gitlink discovery set. Hunk-body text never
/// classifies a block: a normal file whose content resembles gitlink metadata
/// is preserved byte-for-byte.
fn block_is_changed_gitlink(block: &[String], gitlink_paths: &BTreeSet<OsString>) -> bool {
    let Some((from, to)) = diff_block_paths(block) else {
        return false;
    };
    gitlink_paths.contains(&os_string_from_git_path(from))
        || gitlink_paths.contains(&os_string_from_git_path(to))
}

fn gitlink_protocol_block(block: &[String]) -> bool {
    block.iter().any(|line| {
        line.strip_prefix("new file mode ") == Some("160000")
            || line.strip_prefix("deleted file mode ") == Some("160000")
            || line.strip_prefix("old mode ") == Some("160000")
            || line.strip_prefix("new mode ") == Some("160000")
            || (line.starts_with("index ")
                && line
                    .rsplit_once(' ')
                    .is_some_and(|(_, mode)| mode == "160000"))
            || line.strip_prefix("-Subproject commit ").is_some()
            || line.strip_prefix("+Subproject commit ").is_some()
    })
}

fn changed_gitlink_block_paths(
    block: &[String],
    gitlink_paths: &BTreeSet<OsString>,
) -> BTreeSet<OsString> {
    let Some((from, to)) = diff_block_paths(block) else {
        return BTreeSet::new();
    };
    [from, to]
        .into_iter()
        .map(os_string_from_git_path)
        .filter(|path| gitlink_paths.contains(path))
        .collect()
}

fn replace_gitlink_blocks(
    captured: &GitOutput,
    gitlink_paths: &BTreeSet<OsString>,
    working_tree_gitlinks: &GitOutput,
    max_bytes: usize,
) -> GitOutput {
    if working_tree_gitlinks.stdout.is_empty() {
        return captured.clone();
    }
    let body = String::from_utf8_lossy(&captured.stdout);
    let mut lines = body.lines().peekable();
    // Gitlink short-diffs go first so a truncated parent combined patch
    // cannot cut them off. Expansion then splices inner files even when the
    // 2 MiB parent capture is already full.
    let mut merged = String::with_capacity(captured.stdout.len() + working_tree_gitlinks.stdout.len());
    merged.push_str(&String::from_utf8_lossy(&working_tree_gitlinks.stdout));
    if !merged.ends_with('\n') {
        merged.push('\n');
    }
    while let Some(line) = lines.next() {
        if !line.starts_with("diff --git ") {
            merged.push_str(line);
            merged.push('\n');
            continue;
        }
        let mut block = vec![line.to_owned()];
        while lines.peek().is_some_and(|next| !next.starts_with("diff --git ")) {
            let Some(next) = lines.next() else {
                break;
            };
            block.push(next.to_owned());
        }
        if block_is_changed_gitlink(&block, gitlink_paths) {
            continue;
        }
        for block_line in block {
            merged.push_str(&block_line);
            merged.push('\n');
        }
    }
    let mut stdout = merged.into_bytes();
    let exceeded = stdout.len() > max_bytes;
    stdout.truncate(max_bytes);
    GitOutput {
        stdout,
        truncated: captured.truncated || working_tree_gitlinks.truncated || exceeded,
        error: captured.error.clone().or_else(|| working_tree_gitlinks.error.clone()),
    }
}

/// Expand gitlink (mode 160000) blocks in a working-tree diff by splicing one
/// bounded, config-isolated inner diff per initialized submodule (base = the
/// gitlink's old object id); returns the rewritten output, expanded paths,
/// replaced gitlinks, and each expanded file's child paging authority. Only
/// blocks whose decoded `diff --git` path belongs to the changed-gitlink
/// discovery set are candidates; every other block passes through untouched.
fn expand_gitlink_diffs(
    output: &GitOutput,
    gitlink_paths: &BTreeSet<OsString>,
    workspace_root: &Path,
    max_bytes: usize,
    pin_dir: Option<&Path>,
    pin_index: &mut usize,
) -> Result<
    (
        GitOutput,
        BTreeSet<String>,
        BTreeSet<String>,
        BTreeMap<String, SubmodulePagingFile>,
        BTreeMap<String, UntrackedPagingBody>,
    ),
    String,
> {
    expand_changed_gitlink_diffs(
        output,
        gitlink_paths,
        workspace_root,
        max_bytes,
        GitlinkInner::WorkingTree,
        pin_dir,
        pin_index,
    )
}

fn expand_revision_gitlink_diffs(
    root: &Path,
    isolated: IsolatedGitEnvironment<'_>,
    from: &str,
    to: &str,
    output: &GitOutput,
) -> Result<(GitOutput, BTreeSet<String>, BTreeMap<String, SubmodulePagingFile>), String> {
    if output.truncated || output.error.is_some() {
        return Ok((output.clone(), BTreeSet::new(), BTreeMap::new()));
    }
    let gitlink_paths = revision_gitlink_paths(root, isolated, from, to)?;
    let (output, _, expanded_gitlinks, submodule_files, _) = expand_changed_gitlink_diffs(
        output,
        &gitlink_paths,
        root,
        MAX_DIFF_BYTES,
        GitlinkInner::Revisions,
        None,
        &mut 0,
    )?;
    Ok((output, expanded_gitlinks, submodule_files))
}

fn revision_gitlink_paths(
    root: &Path,
    isolated: IsolatedGitEnvironment<'_>,
    from: &str,
    to: &str,
) -> Result<BTreeSet<OsString>, String> {
    let raw_args = [
        "--literal-pathspecs",
        "diff",
        "--raw",
        "-z",
        "--abbrev=64",
        "--no-renames",
        "--no-ext-diff",
        "--no-textconv",
        "--ignore-submodules=none",
        from,
        to,
        "--",
    ];
    let raw = run_git_bounded(root, &raw_args, MAX_CATALOG_BYTES, Some(isolated))?;
    if let Some(error) = raw.error {
        return Err(format!("listing changed gitlinks: {error}"));
    }
    if raw.truncated {
        return Err("changed gitlink list exceeded the size limit".to_owned());
    }
    changed_gitlink_paths(&raw.stdout)
}

enum GitlinkInner {
    WorkingTree,
    Revisions,
}

fn expand_changed_gitlink_diffs(
    output: &GitOutput,
    gitlink_paths: &BTreeSet<OsString>,
    workspace_root: &Path,
    max_bytes: usize,
    inner: GitlinkInner,
    pin_dir: Option<&Path>,
    pin_index: &mut usize,
) -> Result<
    (
        GitOutput,
        BTreeSet<String>,
        BTreeSet<String>,
        BTreeMap<String, SubmodulePagingFile>,
        BTreeMap<String, UntrackedPagingBody>,
    ),
    String,
> {
    let unchanged = || {
        Ok((
            output.clone(),
            BTreeSet::new(),
            BTreeSet::new(),
            BTreeMap::new(),
            BTreeMap::new(),
        ))
    };
    if output.error.is_some() {
        return unchanged();
    }
    let body = String::from_utf8_lossy(&output.stdout);
    let body = body.strip_suffix('\n').unwrap_or(&body);
    let mut chunks: Vec<Vec<String>> = Vec::new();
    let mut lines = body.split('\n').peekable();
    while let Some(line) = lines.next() {
        if !line.starts_with("diff --git ") {
            chunks.push(vec![line.to_owned()]);
            continue;
        }
        let mut block = vec![line.to_owned()];
        while let Some(next) = lines.peek() {
            if next.starts_with("diff --git ") {
                break;
            }
            block.push(lines.next().unwrap().to_owned());
        }
        chunks.push(block);
    }
    let mut handled_gitlink_paths = BTreeSet::new();
    if matches!(inner, GitlinkInner::Revisions) {
        for chunk in &chunks {
            if !chunk
                .first()
                .is_some_and(|line| line.starts_with("diff --git "))
            {
                continue;
            }
            if block_is_changed_gitlink(chunk, gitlink_paths) && gitlink_protocol_block(chunk) {
                handled_gitlink_paths.extend(changed_gitlink_block_paths(chunk, gitlink_paths));
            }
        }
    }
    let mut splices = Vec::new();
    let mut others = Vec::new();
    let mut dropped_typechange_blob = false;
    for chunk in chunks {
        if !chunk
            .first()
            .is_some_and(|line| line.starts_with("diff --git "))
        {
            others.push((chunk, false));
            continue;
        }
        let block = chunk;
        if matches!(inner, GitlinkInner::Revisions)
            && block_is_changed_gitlink(&block, gitlink_paths)
            && !gitlink_protocol_block(&block)
            && changed_gitlink_block_paths(&block, gitlink_paths)
                .iter()
                .any(|path| handled_gitlink_paths.contains(path))
        {
            dropped_typechange_blob = true;
            continue;
        }
        let expansion = if block_is_changed_gitlink(&block, gitlink_paths) {
            match inner {
                GitlinkInner::WorkingTree => expand_gitlink_block(
                    &block,
                    workspace_root,
                    max_bytes,
                    0,
                    pin_dir,
                    max_bytes,
                    pin_index,
                )?,
                GitlinkInner::Revisions => expand_revision_gitlink_block(
                    &block,
                    workspace_root,
                    max_bytes,
                    0,
                    max_bytes,
                )?,
            }
        } else {
            GitlinkExpansion::Keep
        };
        match expansion {
            GitlinkExpansion::Splice {
                sub_path,
                text,
                files,
                untracked,
                truncated,
            } => splices.push(GitlinkSpliceWrite {
                block,
                sub_path,
                text,
                files,
                untracked,
                truncated,
            }),
            GitlinkExpansion::Truncated => others.push((block, true)),
            GitlinkExpansion::Keep => others.push((block, false)),
        }
    }

    // Complete inner splices occupy the combined budget first. Trailing parent
    // file previews shrink or drop so the rewritten stdout stays ≤ max_bytes.
    // Overflow splices keep their original gitlink block and are not marked
    // expanded: catalog paging still owns those paths.
    let mut stdout = Vec::with_capacity(max_bytes.min(body.len().saturating_add(1)));
    let mut has_expansion = false;
    let mut expansion_truncated = false;
    let mut dropped_preview = false;
    let mut expanded_paths = BTreeSet::new();
    let mut expanded_gitlinks = BTreeSet::new();
    let mut expanded_submodule_files = BTreeMap::new();
    let mut expanded_untracked = BTreeMap::new();
    let mut leftover_gitlinks = Vec::new();
    for splice in splices {
        let bytes = splice_text_bytes(&splice.text);
        if stdout.len().saturating_add(bytes.len()) > max_bytes {
            leftover_gitlinks.push(splice.block);
            expansion_truncated = true;
            continue;
        }
        stdout.extend_from_slice(&bytes);
        has_expansion = true;
        expanded_paths.extend(splice.files.keys().cloned());
        expanded_paths.extend(splice.untracked.keys().cloned());
        for file in parse_unified_diff(&splice.text) {
            expanded_paths.insert(file.path);
        }
        expanded_submodule_files.extend(splice.files);
        expanded_untracked.extend(splice.untracked);
        expanded_gitlinks.insert(splice.sub_path);
        expansion_truncated |= splice.truncated;
    }
    for (block, mark_truncated) in leftover_gitlinks
        .into_iter()
        .map(|block| (block, true))
        .chain(others)
    {
        expansion_truncated |= mark_truncated;
        if append_diff_bytes_fitting(&mut stdout, &diff_block_bytes(&block), max_bytes) {
            dropped_preview = true;
            break;
        }
    }
    if !has_expansion && !expansion_truncated && !dropped_typechange_blob && !dropped_preview {
        return unchanged();
    }
    debug_assert!(stdout.len() <= max_bytes);
    Ok((
        GitOutput {
            stdout,
            truncated: output.truncated || expansion_truncated || dropped_preview,
            error: None,
        },
        expanded_paths,
        expanded_gitlinks,
        expanded_submodule_files,
        expanded_untracked,
    ))
}

struct GitlinkSpliceWrite {
    block: Vec<String>,
    sub_path: String,
    text: String,
    files: BTreeMap<String, SubmodulePagingFile>,
    untracked: BTreeMap<String, UntrackedPagingBody>,
    truncated: bool,
}

fn splice_text_bytes(text: &str) -> Vec<u8> {
    let mut bytes = text.as_bytes().to_vec();
    if !bytes.is_empty() && !bytes.ends_with(&[b'\n']) {
        bytes.push(b'\n');
    }
    bytes
}

fn diff_block_bytes(block: &[String]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for line in block {
        bytes.extend_from_slice(line.as_bytes());
        bytes.push(b'\n');
    }
    bytes
}

fn append_diff_bytes_fitting(stdout: &mut Vec<u8>, bytes: &[u8], max_bytes: usize) -> bool {
    if stdout.len().saturating_add(bytes.len()) <= max_bytes {
        stdout.extend_from_slice(bytes);
        return false;
    }
    if stdout.len() < max_bytes {
        let room = max_bytes - stdout.len();
        stdout.extend_from_slice(&bytes[..room]);
    }
    true
}

const GITLINK_EXPAND_MAX_DEPTH: u32 = 4;

enum GitlinkExpansion {
    Keep,
    Truncated,
    Splice {
        sub_path: String,
        text: String,
        files: BTreeMap<String, SubmodulePagingFile>,
        untracked: BTreeMap<String, UntrackedPagingBody>,
        truncated: bool,
    },
}

fn gitlink_block_is_dirty(block: &[String]) -> bool {
    block.iter().any(|line| {
        line.strip_prefix("+Subproject commit ")
            .is_some_and(|commit| commit.ends_with("-dirty"))
    })
}

fn expand_gitlink_block(
    block: &[String],
    workspace_root: &Path,
    max_bytes: usize,
    depth: u32,
    pin_dir: Option<&Path>,
    remaining: usize,
    pin_index: &mut usize,
) -> Result<GitlinkExpansion, String> {
    if depth >= GITLINK_EXPAND_MAX_DEPTH {
        return Ok(GitlinkExpansion::Keep);
    }
    let Some(sub_path) = scoped_submodule_path(&block[0]) else {
        return Ok(GitlinkExpansion::Keep);
    };
    if !gitlink_block_is_dirty(block) {
        return Ok(GitlinkExpansion::Keep);
    }
    let Some(submodule_dir) = scoped_submodule_dir(workspace_root, &sub_path) else {
        return Ok(GitlinkExpansion::Keep);
    };
    if !submodule_dir.join(".git").exists() {
        return Ok(GitlinkExpansion::Keep);
    }
    let Some(inner_base) = gitlink_old_object_id(&block[1..]) else {
        return Ok(GitlinkExpansion::Keep);
    };
    let sandbox = match IsolatedGitSandbox::discover(&submodule_dir) {
        Ok(sandbox) => sandbox,
        Err(_) => return Ok(GitlinkExpansion::Keep),
    };
    let pathspecs = match changed_working_tree_paths(
        &submodule_dir,
        sandbox.environment(),
        &inner_base,
    ) {
        Ok((_, pathspecs)) => pathspecs,
        Err(_) => return Ok(GitlinkExpansion::Keep),
    };
    let mut tracked = GitOutput {
        stdout: Vec::new(),
        truncated: false,
        error: None,
    };
    if !pathspecs.is_empty() {
        if stage_tracked_working_tree_paths(&submodule_dir, sandbox.environment(), &pathspecs)
            .is_err()
        {
            return Ok(GitlinkExpansion::Keep);
        }
        tracked = match run_git_bounded(
            &submodule_dir,
            &[
                "diff",
                "--cached",
                inner_base.as_str(),
                "-O",
                null_device(),
                "--no-ext-diff",
                "--no-textconv",
                "--no-color",
                "--default-prefix",
                "--no-relative",
                "--find-renames",
                "--histogram",
                "--",
            ],
            max_bytes,
            Some(sandbox.environment()),
        ) {
            Ok(output) if output.error.is_none() => output,
            Ok(_) | Err(_) => return Ok(GitlinkExpansion::Keep),
        };
        if tracked.truncated {
            return Ok(GitlinkExpansion::Truncated);
        }
    }
    let (nested_gitlinks, mut gitlink_paths) = match working_tree_gitlink_output(
        &submodule_dir,
        sandbox.environment(),
        &inner_base,
        &BTreeSet::new(),
    ) {
        Ok(pair) => pair,
        Err(_) => (
            GitOutput {
                stdout: Vec::new(),
                truncated: false,
                error: None,
            },
            BTreeSet::new(),
        ),
    };
    if let Ok(index_paths) =
        index_gitlink_paths(&submodule_dir, sandbox.environment(), &pathspecs)
    {
        gitlink_paths.extend(index_paths);
    }
    let authority = Arc::new(SubmodulePagingAuthority {
        sandbox,
        base: inner_base,
        end: None,
    });
    assemble_working_tree_gitlink_splice(
        sub_path,
        submodule_dir,
        tracked,
        nested_gitlinks,
        gitlink_paths,
        authority,
        max_bytes,
        remaining,
        depth,
        pin_dir,
        pin_index,
    )
}

fn expand_revision_gitlink_block(
    block: &[String],
    workspace_root: &Path,
    max_bytes: usize,
    depth: u32,
    remaining: usize,
) -> Result<GitlinkExpansion, String> {
    if depth >= GITLINK_EXPAND_MAX_DEPTH {
        return Ok(GitlinkExpansion::Keep);
    }
    let Some(sub_path) = scoped_submodule_path(&block[0]) else {
        return Ok(GitlinkExpansion::Keep);
    };
    let checkout = scoped_submodule_dir(workspace_root, &sub_path);
    let sandbox = match &checkout {
        Some(dir) if dir.join(".git").exists() => match IsolatedGitSandbox::discover(dir) {
            Ok(sandbox) => sandbox,
            Err(_) => return Ok(GitlinkExpansion::Keep),
        },
        _ => match submodule_modules_dir(workspace_root, &sub_path) {
            Some(modules) => match IsolatedGitSandbox::from_git_dir(&modules) {
                Ok(sandbox) => sandbox,
                Err(_) => return Ok(GitlinkExpansion::Keep),
            },
            None => return Ok(GitlinkExpansion::Keep),
        },
    };
    let Some(old_oid) = gitlink_old_object_id(&block[1..]) else {
        return Ok(GitlinkExpansion::Keep);
    };
    let Some(new_oid) = gitlink_new_object_id(&block[1..]) else {
        return Ok(GitlinkExpansion::Keep);
    };
    let gitlink_paths = match revision_gitlink_paths(
        sandbox.work_tree(),
        sandbox.environment(),
        &old_oid,
        &new_oid,
    ) {
        Ok(paths) => paths,
        Err(_) => BTreeSet::new(),
    };
    let sub_output = match run_git_bounded(
        sandbox.work_tree(),
        &[
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
            old_oid.as_str(),
            new_oid.as_str(),
            "--",
        ],
        max_bytes,
        Some(sandbox.environment()),
    ) {
        Ok(output) if output.error.is_none() => output,
        Ok(_) | Err(_) => return Ok(GitlinkExpansion::Keep),
    };
    let sandbox_root = sandbox.work_tree().to_path_buf();
    let authority = Arc::new(SubmodulePagingAuthority {
        sandbox,
        base: old_oid,
        end: Some(new_oid),
    });
    assemble_revision_gitlink_splice(
        sub_path,
        sandbox_root,
        sub_output,
        gitlink_paths,
        authority,
        max_bytes,
        remaining,
        depth,
    )
}

fn assemble_working_tree_gitlink_splice(
    sub_path: String,
    submodule_dir: PathBuf,
    tracked: GitOutput,
    nested_gitlinks: GitOutput,
    gitlink_paths: BTreeSet<OsString>,
    authority: Arc<SubmodulePagingAuthority>,
    max_bytes: usize,
    remaining: usize,
    depth: u32,
    pin_dir: Option<&Path>,
    pin_index: &mut usize,
) -> Result<GitlinkExpansion, String> {
    let mut text = String::new();
    let mut files = BTreeMap::new();
    let mut untracked = BTreeMap::new();
    let mut truncated = false;
    let mut taken = BTreeSet::new();
    let mut remaining = remaining;
    match splice_regular_inner_files(&sub_path, &tracked, &authority, &gitlink_paths) {
        GitlinkExpansion::Splice {
            text: tracked_text,
            files: tracked_files,
            truncated: tracked_truncated,
            ..
        } => {
            if tracked_text.len() > remaining {
                return Ok(GitlinkExpansion::Truncated);
            }
            text.push_str(&tracked_text);
            taken.extend(tracked_files.keys().cloned());
            files.extend(tracked_files);
            truncated |= tracked_truncated;
            remaining = remaining.saturating_sub(tracked_text.len());
        }
        GitlinkExpansion::Truncated => return Ok(GitlinkExpansion::Truncated),
        GitlinkExpansion::Keep => {}
    }
    let nested_source = if nested_gitlinks.stdout.is_empty() {
        &tracked
    } else {
        &nested_gitlinks
    };
    match expand_nested_gitlink_blocks(
        nested_source,
        &submodule_dir,
        &sub_path,
        max_bytes,
        remaining,
        depth,
        GitlinkInner::WorkingTree,
        &gitlink_paths,
        pin_dir,
        pin_index,
    ) {
        GitlinkExpansion::Splice {
            text: nested_text,
            files: nested_files,
            untracked: nested_untracked,
            truncated: nested_truncated,
            ..
        } => {
            text.push_str(&nested_text);
            taken.extend(nested_files.keys().cloned());
            taken.extend(nested_untracked.keys().cloned());
            files.extend(nested_files);
            untracked.extend(nested_untracked);
            truncated |= nested_truncated;
            remaining = remaining.saturating_sub(nested_text.len());
        }
        GitlinkExpansion::Truncated => truncated = true,
        GitlinkExpansion::Keep => {}
    }
    let pin_dir = pin_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| authority.sandbox.untracked_pin_dir());
    let inner_taken: BTreeSet<String> = taken
        .iter()
        .filter_map(|path| strip_prefixed_display(path, &sub_path))
        .collect();
    match capture_submodule_untracked(
        &submodule_dir,
        authority.sandbox.environment(),
        &inner_taken,
        &pin_dir,
        remaining,
        pin_index,
    ) {
        Ok(additions) => {
            truncated |= additions.truncated;
            append_prefixed_untracked(
                &sub_path,
                additions,
                &mut text,
                &mut untracked,
            );
        }
        Err(_) => return Ok(GitlinkExpansion::Keep),
    }
    finish_gitlink_splice(sub_path, text, files, untracked, truncated)
}

fn assemble_revision_gitlink_splice(
    sub_path: String,
    submodule_dir: PathBuf,
    sub_output: GitOutput,
    gitlink_paths: BTreeSet<OsString>,
    authority: Arc<SubmodulePagingAuthority>,
    max_bytes: usize,
    remaining: usize,
    depth: u32,
) -> Result<GitlinkExpansion, String> {
    if sub_output.truncated {
        return Ok(GitlinkExpansion::Truncated);
    }
    let mut text = String::new();
    let mut files = BTreeMap::new();
    let mut untracked = BTreeMap::new();
    let mut truncated = false;
    match splice_regular_inner_files(&sub_path, &sub_output, &authority, &gitlink_paths) {
        GitlinkExpansion::Splice {
            text: tracked_text,
            files: tracked_files,
            truncated: tracked_truncated,
            ..
        } => {
            if tracked_text.len() > remaining {
                return Ok(GitlinkExpansion::Truncated);
            }
            text.push_str(&tracked_text);
            files.extend(tracked_files);
            truncated |= tracked_truncated;
        }
        GitlinkExpansion::Truncated => return Ok(GitlinkExpansion::Truncated),
        GitlinkExpansion::Keep => {}
    }
    let remaining = remaining.saturating_sub(text.len());
    match expand_nested_gitlink_blocks(
        &sub_output,
        &submodule_dir,
        &sub_path,
        max_bytes,
        remaining,
        depth,
        GitlinkInner::Revisions,
        &gitlink_paths,
        None,
        &mut 0,
    ) {
        GitlinkExpansion::Splice {
            text: nested_text,
            files: nested_files,
            untracked: nested_untracked,
            truncated: nested_truncated,
            ..
        } => {
            text.push_str(&nested_text);
            files.extend(nested_files);
            untracked.extend(nested_untracked);
            truncated |= nested_truncated;
        }
        GitlinkExpansion::Truncated => truncated = true,
        GitlinkExpansion::Keep => {}
    }
    finish_gitlink_splice(sub_path, text, files, untracked, truncated)
}

fn finish_gitlink_splice(
    sub_path: String,
    text: String,
    files: BTreeMap<String, SubmodulePagingFile>,
    untracked: BTreeMap<String, UntrackedPagingBody>,
    truncated: bool,
) -> Result<GitlinkExpansion, String> {
    if text.is_empty() && files.is_empty() && untracked.is_empty() {
        if truncated {
            return Ok(GitlinkExpansion::Truncated);
        }
        return Ok(GitlinkExpansion::Keep);
    }
    Ok(GitlinkExpansion::Splice {
        sub_path,
        text,
        files,
        untracked,
        truncated,
    })
}

fn splice_regular_inner_files(
    sub_path: &str,
    sub_output: &GitOutput,
    authority: &Arc<SubmodulePagingAuthority>,
    gitlink_paths: &BTreeSet<OsString>,
) -> GitlinkExpansion {
    if sub_output.stdout.is_empty() {
        return GitlinkExpansion::Keep;
    }
    let mut regular = Vec::new();
    let mut files = BTreeMap::new();
    for block in diff_file_blocks(&sub_output.stdout) {
        if block_is_changed_gitlink(&block, gitlink_paths) {
            continue;
        }
        if let Some((from, to)) = diff_block_paths(&block) {
            let display =
                normalize_repo_path(&format!("{sub_path}/{}", encode_path_bytes(&to)));
            let previous_path = (from != to).then_some(from);
            files.insert(
                display,
                SubmodulePagingFile {
                    authority: Arc::clone(authority),
                    path: ReviewPathBytes {
                        path: to,
                        previous_path,
                    },
                },
            );
        }
        regular.extend(block);
        regular.push(String::new());
    }
    if regular.iter().all(|line| line.is_empty()) {
        return GitlinkExpansion::Keep;
    }
    if files.is_empty() {
        return GitlinkExpansion::Keep;
    }
    let Some(text) = prefix_diff_text(&regular.join("\n"), sub_path) else {
        return GitlinkExpansion::Keep;
    };
    GitlinkExpansion::Splice {
        sub_path: sub_path.to_owned(),
        text,
        files,
        untracked: BTreeMap::new(),
        truncated: false,
    }
}

fn expand_nested_gitlink_blocks(
    source: &GitOutput,
    submodule_dir: &Path,
    sub_path: &str,
    max_bytes: usize,
    mut remaining: usize,
    depth: u32,
    inner: GitlinkInner,
    gitlink_paths: &BTreeSet<OsString>,
    pin_dir: Option<&Path>,
    pin_index: &mut usize,
) -> GitlinkExpansion {
    let mut text = String::new();
    let mut files = BTreeMap::new();
    let mut untracked = BTreeMap::new();
    let mut truncated = false;
    let mut spliced = false;
    for block in diff_file_blocks(&source.stdout) {
        if !block_is_changed_gitlink(&block, gitlink_paths) {
            continue;
        }
        if remaining == 0 {
            truncated = true;
            let prefixed = prefix_kept_gitlink_block(&block, sub_path);
            if !prefixed.is_empty() {
                text.push_str(&prefixed);
                spliced = true;
            }
            continue;
        }
        let nested = match inner {
            GitlinkInner::WorkingTree => expand_gitlink_block(
                &block,
                submodule_dir,
                max_bytes,
                depth + 1,
                pin_dir,
                remaining,
                pin_index,
            ),
            GitlinkInner::Revisions => expand_revision_gitlink_block(
                &block,
                submodule_dir,
                max_bytes,
                depth + 1,
                remaining,
            ),
        };
        match nested {
            Ok(GitlinkExpansion::Splice {
                text: nested_text,
                files: nested_files,
                untracked: nested_untracked,
                truncated: nested_truncated,
                ..
            }) => {
                let prefixed = match prefix_diff_text(&nested_text, sub_path) {
                    Some(prefixed) => prefixed,
                    None => {
                        let lines: Vec<String> =
                            nested_text.lines().map(str::to_owned).collect();
                        prefix_kept_gitlink_block(&lines, sub_path)
                    }
                };
                if prefixed.is_empty() {
                    continue;
                }
                if prefixed.len() > remaining {
                    truncated = true;
                    let kept = prefix_kept_gitlink_block(&block, sub_path);
                    if !kept.is_empty() {
                        text.push_str(&kept);
                        spliced = true;
                    }
                    continue;
                }
                remaining = remaining.saturating_sub(prefixed.len());
                text.push_str(&prefixed);
                files.extend(prefix_submodule_files(nested_files, sub_path));
                untracked.extend(prefix_untracked_bodies(nested_untracked, sub_path));
                truncated |= nested_truncated;
                spliced = true;
            }
            Ok(GitlinkExpansion::Keep) | Ok(GitlinkExpansion::Truncated) | Err(_) => {
                let prefixed = prefix_kept_gitlink_block(&block, sub_path);
                if prefixed.len() > remaining {
                    truncated = true;
                    continue;
                }
                if !prefixed.is_empty() {
                    remaining = remaining.saturating_sub(prefixed.len());
                    text.push_str(&prefixed);
                    spliced = true;
                }
                if matches!(nested, Ok(GitlinkExpansion::Truncated)) {
                    truncated = true;
                }
            }
        }
    }
    if !spliced {
        return GitlinkExpansion::Keep;
    }
    GitlinkExpansion::Splice {
        sub_path: sub_path.to_owned(),
        text,
        files,
        untracked,
        truncated,
    }
}

fn diff_file_blocks(stdout: &[u8]) -> Vec<Vec<String>> {
    let body = String::from_utf8_lossy(stdout);
    let body = body.strip_suffix('\n').unwrap_or(&body);
    let mut blocks = Vec::new();
    let mut lines = body.split('\n').peekable();
    while let Some(line) = lines.next() {
        if !line.starts_with("diff --git ") {
            continue;
        }
        let mut block = vec![line.to_owned()];
        while let Some(next) = lines.peek() {
            if next.starts_with("diff --git ") {
                break;
            }
            block.push(lines.next().unwrap().to_owned());
        }
        blocks.push(block);
    }
    blocks
}

fn prefix_diff_text(body: &str, prefix: &str) -> Option<String> {
    let mut spliced = String::with_capacity(body.len());
    for sub_line in body.split('\n') {
        if sub_line.is_empty() {
            continue;
        }
        let is_header = sub_line.starts_with("diff --git ")
            || sub_line.starts_with("--- ")
            || sub_line.starts_with("+++ ")
            || sub_line.starts_with("rename from ")
            || sub_line.starts_with("rename to ")
            || sub_line.starts_with("copy from ")
            || sub_line.starts_with("copy to ");
        if !is_header {
            spliced.push_str(sub_line);
            spliced.push('\n');
            continue;
        }
        spliced.push_str(&expand_inner_header(sub_line, prefix)?);
        spliced.push('\n');
    }
    Some(spliced)
}

fn prefix_diff_block(block: &[String], prefix: &str) -> Option<String> {
    prefix_diff_text(&block.join("\n"), prefix)
}

fn prefix_kept_gitlink_block(block: &[String], prefix: &str) -> String {
    if let Some(prefixed) = prefix_diff_block(block, prefix) {
        return prefixed;
    }
    let mut spliced = String::new();
    for line in block {
        if line.is_empty() {
            continue;
        }
        let is_header = line.starts_with("diff --git ")
            || line.starts_with("--- ")
            || line.starts_with("+++ ")
            || line.starts_with("rename from ")
            || line.starts_with("rename to ")
            || line.starts_with("copy from ")
            || line.starts_with("copy to ");
        if !is_header {
            spliced.push_str(line);
            spliced.push('\n');
            continue;
        }
        match expand_inner_header(line, prefix) {
            Some(rewritten) => {
                spliced.push_str(&rewritten);
                spliced.push('\n');
            }
            None => {
                spliced.push_str(line);
                spliced.push('\n');
            }
        }
    }
    if spliced.is_empty() {
        if let Some(name) = block.first().and_then(|line| scoped_submodule_path(line)) {
            let mut rewritten = format!(
                "diff --git {} {}\n",
                emit_git_path(&format!("a/{prefix}/{name}")),
                emit_git_path(&format!("b/{prefix}/{name}")),
            );
            for line in block.iter().skip(1) {
                if line.is_empty() {
                    continue;
                }
                spliced.push_str(line);
                spliced.push('\n');
            }
            rewritten.push_str(&spliced);
            return rewritten;
        }
    }
    spliced
}

fn prefix_submodule_files(
    files: BTreeMap<String, SubmodulePagingFile>,
    prefix: &str,
) -> BTreeMap<String, SubmodulePagingFile> {
    files
        .into_iter()
        .map(|(path, file)| (format!("{prefix}/{path}"), file))
        .collect()
}

fn prefix_untracked_bodies(
    bodies: BTreeMap<String, UntrackedPagingBody>,
    prefix: &str,
) -> BTreeMap<String, UntrackedPagingBody> {
    bodies
        .into_iter()
        .map(|(path, body)| (format!("{prefix}/{path}"), body))
        .collect()
}

fn strip_prefixed_display(path: &str, prefix: &str) -> Option<String> {
    let rest = path.strip_prefix(prefix)?.strip_prefix('/')?;
    if rest.is_empty() {
        None
    } else {
        Some(rest.to_owned())
    }
}

fn submodule_modules_dir(workspace_root: &Path, sub_path: &str) -> Option<PathBuf> {
    let git_marker = workspace_root.join(".git");
    let git_dir = if git_marker.is_dir() {
        git_marker
    } else if git_marker.is_file() {
        let contents = read_small_file(&git_marker, MAX_GIT_METADATA_BYTES).ok()?;
        let contents = std::str::from_utf8(&contents).ok()?;
        let line = strip_output_line_ending(contents);
        let value = line.strip_prefix("gitdir:")?.trim_start();
        let path = Path::new(value);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            workspace_root.join(path)
        }
    } else {
        return None;
    };
    let by_path = git_dir.join("modules").join(os_string_from_git_path(
        decode_path_bytes(sub_path),
    ));
    if by_path.join("HEAD").is_file() {
        return Some(by_path);
    }
    let config = git_dir.join("config");
    let bytes = read_small_file(&config, MAX_GIT_METADATA_BYTES).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    let mut section = "";
    let mut mapped_name = None;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].trim();
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        if !section.starts_with("submodule ") {
            continue;
        }
        if !name.trim().eq_ignore_ascii_case("path") {
            continue;
        }
        if normalize_repo_path(value.trim()) != sub_path {
            continue;
        }
        let Some(quoted) = section.strip_prefix("submodule ") else {
            continue;
        };
        mapped_name = Some(quoted.trim_matches('"').to_owned());
        break;
    }
    let name = mapped_name?;
    let by_name = git_dir.join("modules").join(os_string_from_git_path(
        decode_path_bytes(&name),
    ));
    if by_name.join("HEAD").is_file() {
        Some(by_name)
    } else {
        None
    }
}

fn capture_submodule_untracked(
    root: &Path,
    isolated: IsolatedGitEnvironment<'_>,
    taken_paths: &BTreeSet<String>,
    pin_dir: &Path,
    remaining: usize,
    pin_index: &mut usize,
) -> Result<UntrackedAdditions, String> {
    let list_args = ["ls-files", "--others", "--exclude-standard", "-z"];
    let listing = run_git_bounded(root, &list_args, MAX_UNTRACKED_CATALOG_BYTES, Some(isolated))?;
    if listing.error.is_some() && listing.stdout.is_empty() {
        return Err(listing
            .error
            .unwrap_or_else(|| "untracked file enumeration failed".to_owned()));
    }
    let listing_truncated = listing.truncated || listing.error.is_some();
    let mut paths: Vec<&[u8]> = listing
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .collect();
    let file_cap = MAX_UNTRACKED_FILES.min(remaining.max(1));
    let count_truncated = paths.len() > file_cap;
    if count_truncated {
        paths.truncate(file_cap);
    }
    let patch_budget = MAX_UNTRACKED_DIFF_BYTES.min(remaining);
    let mut result = ingest_untracked_paths(
        root,
        &paths,
        taken_paths,
        pin_dir,
        patch_budget,
        MAX_UNTRACKED_PINNED_BYTES.min(remaining),
        pin_index,
    )?;
    result.truncated |= listing_truncated || count_truncated;
    Ok(result)
}

fn placeholder_added_diff(display: &str) -> String {
    let a = emit_git_path(&format!("a/{display}"));
    let b = emit_git_path(&format!("b/{display}"));
    format!("diff --git {a} {b}\nnew file mode 100644\n--- /dev/null\n+++ {b}\n")
}

fn apply_untracked_placeholder_messages(
    files: &mut [DiffFile],
    untracked: &BTreeMap<String, UntrackedPagingBody>,
) {
    for file in files {
        match untracked.get(&file.path) {
            Some(UntrackedPagingBody::Copy(_) | UntrackedPagingBody::Unpinned) => {
                file.message = Some(PLACEHOLDER_MESSAGE.to_owned());
            }
            Some(UntrackedPagingBody::Patch { .. }) | None => {}
        }
    }
}

fn append_prefixed_untracked(
    sub_path: &str,
    additions: UntrackedAdditions,
    text: &mut String,
    untracked: &mut BTreeMap<String, UntrackedPagingBody>,
) {
    for (inner_path, body) in additions.pinned {
        let display = format!("{sub_path}/{inner_path}");
        match &body {
            UntrackedPagingBody::Patch { bytes, .. } => {
                let stdout = String::from_utf8_lossy(bytes);
                if let Some(prefixed) = prefix_diff_text(&stdout, sub_path) {
                    text.push_str(&prefixed);
                }
            }
            UntrackedPagingBody::Copy(_) | UntrackedPagingBody::Unpinned => {
                text.push_str(&placeholder_added_diff(&display));
            }
        }
        untracked.insert(display, body);
    }
}

fn strip_submodule_path_bytes(path: &str, sub_path: &str) -> Option<Vec<u8>> {
    let path = decode_path_bytes(path);
    let prefix = decode_path_bytes(sub_path);
    let relative = path.strip_prefix(prefix.as_slice())?.strip_prefix(b"/")?;
    if relative.is_empty() || relative.contains(&0) {
        return None;
    }
    Some(relative.to_vec())
}

fn scoped_submodule_path(header: &str) -> Option<String> {
    let rest = header.strip_prefix("diff --git ")?;
    let tokens = split_diff_path_tokens(rest);
    let side = match tokens.as_slice() {
        [_, to] => to,
        [single] => single,
        _ => return None,
    };
    let path = side
        .strip_prefix("a/")
        .or_else(|| side.strip_prefix("b/"))?;
    if path.is_empty() || path.starts_with('/') {
        return None;
    }
    if path
        .split('/')
        .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return None;
    }
    Some(path.to_owned())
}

fn scoped_submodule_dir(workspace_root: &Path, sub_path: &str) -> Option<PathBuf> {
    let candidate =
        workspace_root.join(os_string_from_git_path(decode_path_bytes(sub_path)));
    let canonical_candidate = fs::canonicalize(&candidate).ok()?;
    let canonical_root = fs::canonicalize(workspace_root).ok()?;
    if !canonical_candidate.starts_with(&canonical_root) {
        return None;
    }
    Some(canonical_candidate)
}

fn gitlink_old_object_id(block: &[String]) -> Option<String> {
    let old = block
        .iter()
        .find_map(|line| line.strip_prefix("-Subproject commit "))
        .or_else(|| {
            let index_line = block.iter().find(|line| line.starts_with("index "))?;
            let rest = index_line.strip_prefix("index ")?.trim_start();
            rest.split_once("..").map(|(old, _)| old.trim())
        })?;
    if old.is_empty() || old.len() > 64 || !old.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    if old.bytes().all(|byte| byte == b'0') {
        return None;
    }
    Some(old.to_owned())
}

fn gitlink_new_object_id(block: &[String]) -> Option<String> {
    let new = block
        .iter()
        .find_map(|line| {
            line.strip_prefix("+Subproject commit ")
                .map(|commit| commit.strip_suffix("-dirty").unwrap_or(commit))
        })
        .or_else(|| {
            let index_line = block.iter().find(|line| line.starts_with("index "))?;
            let rest = index_line.strip_prefix("index ")?.trim_start();
            let rest = rest.split_once("..")?.1;
            rest.split_whitespace().next()
        })?;
    if new.is_empty() || new.len() > 64 || !new.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    if new.bytes().all(|byte| byte == b'0') {
        return None;
    }
    Some(new.to_owned())
}

fn expand_inner_header(line: &str, sub_path: &str) -> Option<String> {
    if let Some(rest) = line.strip_prefix("diff --git ") {
        let tokens = split_diff_path_tokens(rest);
        let (from, to) = match tokens.as_slice() {
            [from, to] => (from, to),
            _ => return None,
        };
        let from = strip_inner_side(from)?;
        let to = strip_inner_side(to)?;
        return Some(format!(
            "diff --git {} {}",
            emit_git_path(&format!("a/{sub_path}/{from}")),
            emit_git_path(&format!("b/{sub_path}/{to}"))
        ));
    }
    for marker in ["rename from ", "rename to ", "copy from ", "copy to "] {
        if let Some(rest) = line.strip_prefix(marker) {
            let path = decode_header_path(rest);
            if path.is_empty() {
                return None;
            }
            return Some(format!(
                "{marker}{}",
                emit_git_path(&format!("{sub_path}/{path}"))
            ));
        }
    }
    let (marker, prefix, rest) = if let Some(rest) = line.strip_prefix("--- ") {
        ("--- ", "a/", rest)
    } else if let Some(rest) = line.strip_prefix("+++ ") {
        ("+++ ", "b/", rest)
    } else {
        return None;
    };

    let (token, suffix) = match rest.split_once('\t') {
        Some((token, suffix)) => (token, Some(suffix)),
        None => (rest, None),
    };
    let decoded = decode_header_path(token);
    if decoded == "/dev/null" {
        return Some(line.to_owned());
    }
    let path = decoded
        .strip_prefix("a/")
        .or_else(|| decoded.strip_prefix("b/"))?;
    if path.is_empty() {
        return None;
    }
    let mut rewritten =
        format!("{marker}{}", emit_git_path(&format!("{prefix}{sub_path}/{path}")));
    if let Some(suffix) = suffix {
        rewritten.push('\t');
        rewritten.push_str(suffix);
    }
    Some(rewritten)
}

fn strip_inner_side(token: &str) -> Option<String> {
    let path = token
        .strip_prefix("a/")
        .or_else(|| token.strip_prefix("b/"))?;
    if path.is_empty() {
        None
    } else {
        Some(path.to_owned())
    }
}

fn emit_git_path(path: &str) -> String {
    let needs_quote =
        |byte: u8| byte < 0x21 || byte == b'"' || byte == b'\\' || byte >= 0x7f;
    let raw = decode_path_bytes(path);
    if !raw.iter().copied().any(needs_quote) {
        return path.to_owned();
    }
    let mut quoted = String::from("\"");
    for byte in raw {
        if needs_quote(byte) {
            quoted.push_str(&format!("\\{byte:03o}"));
        } else {
            quoted.push(byte as char);
        }
    }
    quoted.push('"');
    quoted
}


/// Message on on-demand placeholder entries: the file's body was not carried
/// by the truncated combined patch and loads through `code_review_file_diff`.
const PLACEHOLDER_MESSAGE: &str = "diff omitted: combined diff truncated; loaded on demand";

const PREVIEW_CUT_MESSAGE: &str =
    "diff preview cut by the snapshot size limit; full body loads on demand";

const PER_FILE_BYTE_CAP_MESSAGE: &str = "diff truncated by size limit";

/// One entry in the changed-file catalog: how a destination path changed and
/// where it came from (rename/copy provenance).
#[derive(Clone, Debug, PartialEq, Eq)]
struct CatalogEntry {
    status: FileStatus,
    previous_path: Option<String>,
    path_bytes: Vec<u8>,
    previous_path_bytes: Option<Vec<u8>>,
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
        if status
            .first()
            .is_some_and(|prefix| matches!(prefix, b'R' | b'C'))
        {
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
                    path_bytes: destination.to_vec(),
                    previous_path_bytes: Some(previous.to_vec()),
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
                    path_bytes: destination.to_vec(),
                    previous_path_bytes: None,
                },
            );
        }
    }
    catalog
}

/// Backfill placeholder [`DiffFile`] entries for every catalogued path the
/// truncated combined patch could not carry. Parsed entries are never touched
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
            truncated: false,
            message: Some(PLACEHOLDER_MESSAGE.to_owned()),
        });
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NumStatEntry {
    insertions: usize,
    deletions: usize,
    binary: bool,
}

fn numstat_entries(stdout: &[u8]) -> Result<BTreeMap<String, NumStatEntry>, String> {
    if !stdout.is_empty() && stdout[stdout.len() - 1] != 0 {
        return Err("change summary record is unterminated".to_owned());
    }
    let mut catalog = BTreeMap::new();
    let fields: Vec<&[u8]> = stdout.split(|byte| *byte == 0).collect();
    let body: &[&[u8]] = if stdout.is_empty() {
        &fields[..0]
    } else {
        &fields[..fields.len() - 1]
    };
    let mut index = 0usize;
    while index < body.len() {
        let record = body[index];
        index += 1;
        let mut parts = record.splitn(3, |byte| *byte == b'\t');
        let (Some(insertions_raw), Some(deletions_raw)) = (parts.next(), parts.next()) else {
            return Err("change summary record is malformed".to_owned());
        };
        let path_field = parts.next().unwrap_or(&[]);
        let destination = if path_field.is_empty() {
            let old = body.get(index).filter(|field| !field.is_empty());
            let new = body.get(index + 1).filter(|field| !field.is_empty());
            let (Some(_old), Some(new)) = (old, new) else {
                return Err("change summary rename record is truncated".to_owned());
            };
            index += 2;
            normalize_repo_path(&encode_path_bytes(new))
        } else {
            normalize_repo_path(&encode_path_bytes(path_field))
        };
        let binary = insertions_raw == b"-" && deletions_raw == b"-";
        let parse_count = |value: &[u8]| -> Result<usize, String> {
            let value = std::str::from_utf8(value)
                .map_err(|_| "change summary record is malformed".to_owned())?;
            value
                .parse::<usize>()
                .map_err(|_| "change summary record is malformed".to_owned())
        };
        let entry = if binary {
            NumStatEntry {
                insertions: 0,
                deletions: 0,
                binary: true,
            }
        } else {
            NumStatEntry {
                insertions: parse_count(insertions_raw)?,
                deletions: parse_count(deletions_raw)?,
                binary: false,
            }
        };
        catalog.insert(destination, entry);
    }
    Ok(catalog)
}

fn apply_numstat(files: &mut [DiffFile], numstat: &BTreeMap<String, NumStatEntry>) {
    for file in files.iter_mut() {
        if !file.hunks.is_empty() && !file.truncated {
            continue;
        }
        if let Some(entry) = numstat.get(&file.path) {
            file.insertions = entry.insertions;
            file.deletions = entry.deletions;
            if entry.binary && file.hunks.is_empty() {
                file.binary = true;
            }
        }
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

    fn from_git_dir(git_dir: PathBuf) -> Result<Self, String> {
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
        Ok(Self {
            root: git_dir.clone(),
            git_dir,
            common_dir,
        })
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
    let config =
        String::from_utf8(bytes).map_err(|_| "git config is not valid UTF-8".to_owned())?;
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
            copy_index_preserving_mtime(&source_index, &git_dir.join("index"))?;
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
#[derive(Debug)]
pub(crate) struct IsolatedGitSandbox {
    sandbox: GitSandbox,
    work_tree: PathBuf,
}

impl IsolatedGitSandbox {
    pub(crate) fn discover(cwd: &Path) -> Result<Self, String> {
        Self::from_layout(RepositoryLayout::discover(cwd)?)
    }

    fn from_git_dir(git_dir: &Path) -> Result<Self, String> {
        let git_dir = fs::canonicalize(git_dir)
            .map_err(|error| format!("resolving git directory: {error}"))?;
        Self::from_layout(RepositoryLayout::from_git_dir(git_dir)?)
    }

    fn from_layout(repository: RepositoryLayout) -> Result<Self, String> {
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

    fn untracked_pin_dir(&self) -> PathBuf {
        self.sandbox.directory.path.join("untracked")
    }
}

fn copy_shared_indexes(source: &Path, destination: &Path) -> Result<(), String> {
    let mut copied = 0usize;
    let entries =
        fs::read_dir(source).map_err(|error| format!("reading git index directory: {error}"))?;
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
        copy_index_preserving_mtime(&entry.path(), &destination.join(name))?;
    }
    Ok(())
}

fn copy_index_preserving_mtime(source: &Path, destination: &Path) -> Result<(), String> {
    let source_mtime = fs::metadata(source)
        .and_then(|metadata| metadata.modified())
        .map_err(|error| format!("reading git index metadata: {error}"))?;
    fs::copy(source, destination).map_err(|error| format!("copying git index: {error}"))?;
    let file = fs::OpenOptions::new()
        .write(true)
        .open(destination)
        .map_err(|error| format!("opening copied git index: {error}"))?;
    file.set_times(fs::FileTimes::new().set_modified(source_mtime))
        .map_err(|error| format!("preserving copied git index mtime: {error}"))
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
            let previous =
                old_path.filter(|p| p != "/dev/null" && Some(p.as_str()) != Some(path.as_str()));
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
            header: cap_hunk_header(sanitize_display(line)),
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
                for ch in valid.chars() {
                    if ch == '\\' {
                        output.push_str("\\\\");
                    } else {
                        output.push(ch);
                    }
                }
                break;
            }
            Err(error) => {
                let valid_end = start + error.valid_up_to();
                if valid_end > start {
                    let valid =
                        std::str::from_utf8(&bytes[start..valid_end]).expect("valid prefix");
                    for ch in valid.chars() {
                        if ch == '\\' {
                            output.push_str("\\\\");
                        } else {
                            output.push(ch);
                        }
                    }
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

fn decode_path_bytes(path: &str) -> Vec<u8> {
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] != b'\\' {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        if bytes.get(index + 1) == Some(&b'\\') {
            decoded.push(b'\\');
            index += 2;
            continue;
        }
        if let Some(digits) = bytes.get(index + 1..index + 4)
            && digits.iter().all(|digit| matches!(digit, b'0'..=b'7'))
        {
            let value = u16::from(digits[0] - b'0') * 64
                + u16::from(digits[1] - b'0') * 8
                + u16::from(digits[2] - b'0');
            decoded.push(u8::try_from(value).expect("three octal digits fit in a byte"));
            index += 4;
            continue;
        }
        decoded.push(b'\\');
        index += 1;
    }
    decoded
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

#[derive(Debug)]
enum UntrackedPagingBody {
    Patch { bytes: Vec<u8>, truncated: bool },
    Copy(PathBuf),
    Unpinned,
}

/// One untracked addition captured per path: the parsed [`DiffFile`] entry
/// plus the raw `git diff --no-index` patch bytes that produced it, or a
/// bounded path+metadata+content fingerprint when the body was omitted.
#[derive(Debug)]
struct UntrackedAdditions {
    files: Vec<DiffFile>,
    /// Concatenated raw patches and bounded placeholder fingerprints;
    /// hashed into the snapshot identity so a content or name change to any
    /// untracked file — including oversized placeholders with no patch —
    /// flips the stale guard.
    patches: Vec<u8>,
    pinned: BTreeMap<String, UntrackedPagingBody>,
    truncated: bool,
    total_insertions: usize,
    total_deletions: usize,
}

fn count_untracked_added_lines(abs_path: &Path, display: &str) -> Result<(usize, usize, bool), String> {
    let mut file =
        fs::File::open(abs_path).map_err(|error| format!("reading {display}: {error}"))?;
    let mut probe_remaining = GIT_BINARY_FIRST_FEW_BYTES;
    let mut newlines = 0usize;
    let mut total = 0usize;
    let mut last_byte = 0u8;
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let read = std::io::Read::read(&mut file, &mut chunk)
            .map_err(|error| format!("reading {display}: {error}"))?;
        if read == 0 {
            break;
        }
        if probe_remaining > 0 {
            let probe = read.min(probe_remaining);
            if chunk[..probe].contains(&0) {
                return Ok((0, 0, true));
            }
            probe_remaining -= probe;
        }
        newlines += chunk[..read].iter().filter(|&&byte| byte == b'\n').count();
        last_byte = chunk[read - 1];
        total += read;
        if total > MAX_FILE_DIFF_BYTES {
            return Err(format!(
                "untracked file {display} exceeds the review diff cap; its line count cannot be included in a complete summary"
            ));
        }
    }
    let unterminated = usize::from(total > 0 && last_byte != b'\n');
    Ok((newlines + unterminated, 0, false))
}

/// Bounded identity record for an untracked placeholder that contributed no
/// patch bytes: raw path, file size, and a capped content sample. Never
/// stages, never hashes the whole file, and never reads past
/// [`MAX_UNTRACKED_PLACEHOLDER_SAMPLE_BYTES`].
fn append_untracked_placeholder_identity(
    out: &mut Vec<u8>,
    path: &[u8],
    abs_path: &Path,
    metadata: &fs::Metadata,
) {
    out.extend_from_slice(&(path.len() as u64).to_le_bytes());
    out.extend_from_slice(path);
    out.extend_from_slice(&metadata.len().to_le_bytes());
    let mut file = match fs::File::open(abs_path) {
        Ok(file) => file,
        Err(_) => {
            out.extend_from_slice(&0u64.to_le_bytes());
            out.push(0);
            return;
        }
    };
    let mut sample = Vec::new();
    let mut chunk = [0_u8; 8192];
    let mut truncated = false;
    loop {
        let remaining = MAX_UNTRACKED_PLACEHOLDER_SAMPLE_BYTES.saturating_sub(sample.len());
        if remaining == 0 {
            truncated = metadata.len() > sample.len() as u64;
            break;
        }
        let take = remaining.min(chunk.len());
        match std::io::Read::read(&mut file, &mut chunk[..take]) {
            Ok(0) => break,
            Ok(n) => sample.extend_from_slice(&chunk[..n]),
            Err(_) => {
                out.extend_from_slice(&0u64.to_le_bytes());
                out.push(0);
                return;
            }
        }
    }
    out.extend_from_slice(&(sample.len() as u64).to_le_bytes());
    out.extend_from_slice(&sample);
    out.push(u8::from(truncated));
}

fn untracked_added_files(
    root: &Path,
    isolated: IsolatedGitEnvironment<'_>,
    taken_paths: &BTreeSet<String>,
    pin_dir: &Path,
    pin_index: &mut usize,
) -> Result<UntrackedAdditions, String> {
    let list_args = ["ls-files", "--others", "--exclude-standard", "-z"];
    let listing = match run_git_bounded(root, &list_args, MAX_UNTRACKED_CATALOG_BYTES, Some(isolated)) {
        Ok(output) if output.error.is_none() && !output.truncated => output,
        Ok(output) => Err(output.error.unwrap_or_else(|| {
            "untracked file enumeration exceeded the size limit".to_owned()
        }))?,
        Err(error) => Err(error)?,
    };
    let paths: Vec<&[u8]> = listing
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .collect();
    if paths.len() > MAX_UNTRACKED_FILES {
        return Err(format!(
            "untracked file count exceeds the review limit of {MAX_UNTRACKED_FILES}"
        ));
    }
    ingest_untracked_paths(
        root,
        &paths,
        taken_paths,
        pin_dir,
        MAX_UNTRACKED_DIFF_BYTES,
        MAX_UNTRACKED_PINNED_BYTES,
        pin_index,
    )
}

fn push_degraded_untracked(
    result: &mut UntrackedAdditions,
    path: &[u8],
    display: String,
    abs_path: &Path,
    metadata: &fs::Metadata,
    pin_dir: &Path,
    pin_index: &mut usize,
    pinned_budget: &mut usize,
) {
    let (insertions, deletions, binary) = match count_untracked_added_lines(abs_path, &display) {
        Ok(stats) => stats,
        Err(_) => (0, 0, false),
    };
    append_untracked_placeholder_identity(&mut result.patches, path, abs_path, metadata);
    let pinned = if !binary && metadata.len() <= *pinned_budget as u64 {
        match pin_untracked_copy(pin_dir, *pin_index, abs_path) {
            Some(copy_path) => {
                *pinned_budget -= metadata.len() as usize;
                *pin_index += 1;
                UntrackedPagingBody::Copy(copy_path)
            }
            None => UntrackedPagingBody::Unpinned,
        }
    } else {
        UntrackedPagingBody::Unpinned
    };
    result.pinned.insert(display.clone(), pinned);
    result.files.push(DiffFile {
        path: display,
        previous_path: None,
        status: FileStatus::Added,
        binary,
        insertions,
        deletions,
        hunks: Vec::new(),
        truncated: false,
        message: Some(PLACEHOLDER_MESSAGE.to_owned()),
    });
    result.truncated = true;
    result.total_insertions += insertions;
    result.total_deletions += deletions;
}

fn ingest_untracked_paths(
    root: &Path,
    paths: &[&[u8]],
    taken_paths: &BTreeSet<String>,
    pin_dir: &Path,
    mut budget: usize,
    mut pinned_budget: usize,
    pin_index: &mut usize,
) -> Result<UntrackedAdditions, String> {
    let mut result = UntrackedAdditions {
        files: Vec::new(),
        patches: Vec::new(),
        pinned: BTreeMap::new(),
        truncated: false,
        total_insertions: 0,
        total_deletions: 0,
    };
    for path in paths {
        let os_path = os_string_from_git_path(path.to_vec());
        // Contained, non-symlink, regular-file validation: an untracked path
        // is only reviewable when metadata confirms a symlink-free file
        // inside the work tree at capture time. Anything else (directory,
        // symlink, race-deleted) is skipped.
        let Ok(metadata) = fs::symlink_metadata(root.join(&os_path)) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let display = normalize_repo_path(&encode_path_bytes(path));
        if display.is_empty() || display == "." {
            continue;
        }
        if taken_paths.contains(&display) {
            continue;
        }
        if metadata.len() > MAX_FILE_DIFF_BYTES as u64 {
            push_degraded_untracked(
                &mut result,
                path,
                display,
                &root.join(&os_path),
                &metadata,
                pin_dir,
                pin_index,
                &mut pinned_budget,
            );
            continue;
        }
        if budget == 0 {
            push_degraded_untracked(
                &mut result,
                path,
                display,
                &root.join(&os_path),
                &metadata,
                pin_dir,
                pin_index,
                &mut pinned_budget,
            );
            continue;
        }
        // `--no-index` does not consult the repository; the isolated config
        // environment would break the worktree path binding, so it is run
        let args = [
            OsString::from("diff"),
            OsString::from("--no-ext-diff"),
            OsString::from("--no-textconv"),
            OsString::from("--no-index"),
            OsString::from("--"),
            OsString::from(null_device()),
            os_path.clone(),
        ];
        let output = match run_git_bounded_os(root, &args, MAX_FILE_DIFF_BYTES, None) {
            Ok(output) => output,
            // A spawn/timeout failure is real: fail closed.
            Err(error) => return Err(error),
        };
        // Exit 1 = differences found: the normal outcome for a new file.
        let output = if output.error.is_some() && output.stdout.is_empty() {
            // Unreadable or vanished between listing and diffing: skip.
            continue;
        } else {
            GitOutput {
                stdout: output.stdout,
                truncated: output.truncated,
                error: None,
            }
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        let parsed = parse_unified_diff(&stdout);
        budget = budget.saturating_sub(output.stdout.len());
        result.patches.extend_from_slice(&output.stdout);
        result.truncated |= output.truncated;
        result.pinned.insert(
            display.clone(),
            UntrackedPagingBody::Patch {
                bytes: output.stdout.clone(),
                truncated: output.truncated,
            },
        );
        let parsed_entry = parsed.into_iter().find(|file| file.path == display);
        let mut file = parsed_entry.clone().unwrap_or_else(|| DiffFile {
            path: display.clone(),
            previous_path: None,
            status: FileStatus::Added,
            binary: false,
            insertions: 0,
            deletions: 0,
            hunks: Vec::new(),
            truncated: false,
            message: None,
        });
        // The display path is authoritative; `--no-index` headers may render
        // non-UTF-8 names through lossy conversion.
        file.path = display;
        if file.status == FileStatus::Unknown || file.status == FileStatus::Modified {
            file.status = FileStatus::Added;
        }
        file.previous_path = None;
        if output.truncated && file.message.is_none() {
            file.message = Some(PER_FILE_BYTE_CAP_MESSAGE.to_owned());
        }
        let (insertions, deletions, binary) = if output.truncated || parsed_entry.is_none() {
            match count_untracked_added_lines(&root.join(&os_path), &file.path) {
                Ok(stats) => stats,
                Err(_) => {
                    result.truncated = true;
                    (file.insertions, file.deletions, file.binary)
                }
            }
        } else {
            (file.insertions, file.deletions, file.binary)
        };
        file.insertions = insertions;
        file.deletions = deletions;
        file.binary = binary;
        result.total_insertions += insertions;
        result.total_deletions += deletions;
        result.files.push(file);
    }
    Ok(result)
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

fn pin_untracked_copy(pin_dir: &Path, pin_index: usize, source: &Path) -> Option<PathBuf> {
    fs::create_dir_all(pin_dir).ok()?;
    let destination = pin_dir.join(format!("{pin_index}.body"));
    fs::copy(source, &destination).ok()?;
    Some(destination)
}

#[derive(Debug, Clone)]
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
            "core.quotepath=true",
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

    // One absolute ceiling for the whole reader collection, created once after
    // git's status is observed. Both sequential receives share it: the second
    // stream gets only the remaining time, so total collection stays within
    // READER_RESULT_DEADLINE instead of approaching two of them.
    let reader_deadline = Instant::now() + READER_RESULT_DEADLINE;

    let stdout_result = match receive_reader(stdout_rx, "stdout", reader_deadline) {
        Ok(result) => result,
        Err(error) => {
            terminate_process_tree(&mut child);
            return Err(error);
        }
    };
    let stderr_result = match receive_reader(stderr_rx, "stderr", reader_deadline) {
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

fn spawn_bounded_reader<R>(reader: R, max: usize) -> Receiver<std::io::Result<(Vec<u8>, bool)>>
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
    deadline: Instant,
) -> Result<(Vec<u8>, bool), String> {
    loop {
        match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(result) => return result.map_err(|error| format!("reading git {stream}: {error}")),
            Err(RecvTimeoutError::Timeout) => {
                if Instant::now() >= deadline {
                    return Err(format!("git {stream} remained open after exit"));
                }
                // The reader thread was scheduled late (or the wait returned
                // early); keep waiting until the total deadline. Only a
                // successfully delivered channel message is authoritative.
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(format!("git {stream} reader stopped"));
            }
        }
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
// Per-file paging uses the same fixed-argv bounded diff against snapshot-pinned
// authority: a working-tree snapshot's prepared isolated index and comparison
// base, or resolved revision OIDs plus raw catalog path bytes. It never
// re-reads or re-stages the current worktree under an older snapshot id.
// Pathspecs come from the snapshot's normalized DiffFile entry; the RPC layer
// also enforces snapshot-id and path containment before calling here.

/// Per-file full diff byte cap for the paging RPC. Bounded so a single huge
/// file never streams unbounded git output, but well above the 2 MiB snapshot
/// cap so a file truncated by the global snapshot can still be loaded in full.
pub const MAX_FILE_DIFF_BYTES: usize = 8 * 1024 * 1024;
/// Maximum diff lines returned in one page. Combined with the per-page byte
/// cap, keeps every page well under the 4 MiB WS frame limit.
pub const MAX_FILE_PAGE_LINES: usize = 1000;
pub const MAX_FILE_PAGE_BYTES: usize = 1024 * 1024;
/// Per-line text cap. A single minified line can exceed the page byte cap; the
/// loader truncates such a line and marks it so the page stays frame-safe and
/// the user knows content was elided.
pub const MAX_DIFF_LINE_TEXT_BYTES: usize = 256 * 1024;

pub const MAX_HUNK_HEADER_BYTES: usize = 256;
pub(crate) const HUNK_DESCRIPTOR_FIXED_BYTES: usize = 384;
pub(crate) const MAX_HUNK_DESCRIPTOR_BYTES: usize =
    MAX_HUNK_HEADER_BYTES + HUNK_DESCRIPTOR_FIXED_BYTES;
pub(crate) const LINE_JSON_OVERHEAD_BYTES: usize = 64;
pub(crate) const PAGE_JSON_OVERHEAD_BYTES: usize = 1024;
/// Full parsed diff for a single file, ready to be sliced into pages. The
/// hunk structure is preserved (not flattened): every page carries complete
/// hunk descriptors so the frontend can reconstruct exact hunks and content
/// hashes even when a hunk's lines span page boundaries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDiff {
    pub path: String,
    pub previous_path: Option<String>,
    pub status: FileStatus,
    pub binary: bool,
    /// Complete parsed hunks in display order. A byte-capped load may leave
    /// the LAST hunk incomplete (see [`DiffHunk::is_complete`]); every other
    /// hunk is complete by construction.
    pub hunks: Vec<DiffHunk>,
    pub byte_capped: bool,
    pub error: Option<String>,
}

/// One hunk window inside a bounded page: the COMPLETE hunk's identity
/// (header, ranges, content hash over the full hunk text — never over the
/// page fragment) plus the subset of its lines that fall inside this page's
/// line window. `line_start` is the hunk-local offset of the first carried
/// line, so the frontend can deterministically merge page-split hunks and
/// dedupe against snapshot-seeded prefixes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDiffPageHunk {
    /// 0-based hunk index across the file's hunks.
    pub index: usize,
    pub header: String,
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
    /// SHA-256 of the COMPLETE parsed hunk's unified diff (see
    /// [`HunkIdentity::new`]) — identical on every page carrying this hunk,
    /// so a page-split hunk keeps one stable identity.
    pub content_hash: String,
    /// Total lines of the complete hunk (meta lines included). The frontend
    /// marks the hunk comment-ready only when its merged lines reach this
    /// count.
    pub total_lines: usize,
    /// True when the parsed hunk body is complete (never cut by the per-file
    /// byte cap). An incomplete hunk is never comment-ready.
    pub complete: bool,
    /// Hunk-local line offset where this page's subset starts.
    pub line_start: usize,
    /// The hunk's lines carried by this page, in display order.
    pub lines: Vec<DiffLine>,
}

/// One bounded page of a single file's diff.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDiffPage {
    pub path: String,
    pub snapshot_id: String,
    pub previous_path: Option<String>,
    pub status: FileStatus,
    pub binary: bool,
    /// Hunk windows overlapping `[cursor, next_cursor)`, in hunk order. The
    /// union of their line subsets is exactly the page's line window.
    pub hunks: Vec<FileDiffPageHunk>,
    /// Cursor of the first line in this page (0-based across the full file).
    pub cursor: usize,
    /// Cursor of the next page, or `None` when no more lines remain.
    pub next_cursor: Option<usize>,
    pub has_more: bool,
    /// Total lines in the full file diff (the page is a window over this).
    pub total_lines: usize,
    /// Total hunks in the loaded file diff. The frontend knows the hunk
    /// metadata set is complete once it holds this many hunk descriptors
    /// (or the backend reports no more pages).
    pub hunk_count: usize,
    /// Per-file byte cap was hit during load; even more content exists.
    pub byte_capped: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileBlob {
    pub lines: Vec<String>,
    pub binary: bool,
    pub truncated: bool,
}

impl FileBlob {
    fn empty() -> Self {
        Self {
            lines: Vec::new(),
            binary: false,
            truncated: false,
        }
    }

    fn from_bytes(bytes: &[u8], truncated: bool) -> Self {
        if bytes
            .iter()
            .take(GIT_BINARY_FIRST_FEW_BYTES)
            .any(|&byte| byte == 0)
        {
            return Self {
                lines: Vec::new(),
                binary: true,
                truncated,
            };
        }
        let text = String::from_utf8_lossy(bytes);
        let lines = if text.is_empty() {
            Vec::new()
        } else {
            text.lines().map(str::to_owned).collect()
        };
        Self {
            lines,
            binary: false,
            truncated,
        }
    }
}

/// Snapshot-scoped old/new file text used to fill unified-diff gaps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileContextBlobs {
    pub old: FileBlob,
    pub new: FileBlob,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileContextSlice {
    pub old_line_count: u32,
    pub new_line_count: u32,
    pub binary: bool,
    pub lines: Vec<DiffLine>,
}

impl FileContextBlobs {
    pub fn load(
        cwd: &Path,
        paging: &ReviewPagingState,
        file: &DiffFile,
    ) -> Result<Self, String> {
        if file.binary || file.status == FileStatus::Binary {
            return Ok(Self {
                old: FileBlob {
                    lines: Vec::new(),
                    binary: true,
                    truncated: false,
                },
                new: FileBlob {
                    lines: Vec::new(),
                    binary: true,
                    truncated: false,
                },
            });
        }
        if let Some(authority) = &paging.working_tree {
            return load_working_tree_blobs(authority, file);
        }
        if let Some(revisions) = &paging.revisions {
            return load_revision_blobs(cwd, paging, revisions, file);
        }
        Err("review paging state is unavailable for this snapshot".to_owned())
    }

    pub fn slice(&self, old_start: u32, new_start: u32, count: u32) -> Result<FileContextSlice, String> {
        if self.old.binary || self.new.binary {
            return Err("binary files cannot expand context".to_owned());
        }
        let old_line_count = u32::try_from(self.old.lines.len()).unwrap_or(u32::MAX);
        let new_line_count = u32::try_from(self.new.lines.len()).unwrap_or(u32::MAX);
        if count == 0 {
            return Ok(FileContextSlice {
                old_line_count,
                new_line_count,
                binary: false,
                lines: Vec::new(),
            });
        }
        if old_start == 0 && new_start == 0 {
            return Err("context expand requires an old or new start line".to_owned());
        }
        let cap = count.min(MAX_FILE_RENDER_LINES as u32);
        let old_part = blob_slice(&self.old.lines, old_start, cap);
        let new_part = blob_slice(&self.new.lines, new_start, cap);
        let lines = match (old_start == 0, new_start == 0) {
            (true, true) => Vec::new(),
            (true, false) => new_part
                .iter()
                .enumerate()
                .map(|(index, text)| DiffLine {
                    kind: DiffLineKind::Context,
                    old_no: None,
                    new_no: Some(new_start.saturating_add(index as u32)),
                    text: text.clone(),
                })
                .collect(),
            (false, true) => old_part
                .iter()
                .enumerate()
                .map(|(index, text)| DiffLine {
                    kind: DiffLineKind::Context,
                    old_no: Some(old_start.saturating_add(index as u32)),
                    new_no: None,
                    text: text.clone(),
                })
                .collect(),
            (false, false) => {
                let n = old_part.len().min(new_part.len());
                (0..n)
                    .map(|index| DiffLine {
                        kind: DiffLineKind::Context,
                        old_no: Some(old_start.saturating_add(index as u32)),
                        new_no: Some(new_start.saturating_add(index as u32)),
                        text: new_part[index].clone(),
                    })
                    .collect()
            }
        };
        Ok(FileContextSlice {
            old_line_count,
            new_line_count,
            binary: false,
            lines,
        })
    }
}

/// Gitlink (mode 160000) hunks are only `Subproject commit <oid>` lines
/// (optional `-dirty`). A regular file that merely contains those strings among
/// other lines is not a gitlink and must still load as text.
fn is_gitlink_protocol_line(text: &str) -> bool {
    let Some(oid) = text.strip_prefix("Subproject commit ") else {
        return false;
    };
    let oid = oid.strip_suffix("-dirty").unwrap_or(oid);
    !oid.is_empty() && oid.len() <= 64 && oid.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn is_gitlink_diff_file(file: &DiffFile) -> bool {
    let mut saw_protocol = false;
    for hunk in &file.hunks {
        for line in &hunk.lines {
            match line.kind {
                DiffLineKind::Meta => {}
                DiffLineKind::Context | DiffLineKind::Addition | DiffLineKind::Deletion => {
                    if !is_gitlink_protocol_line(&line.text) {
                        return false;
                    }
                    saw_protocol = true;
                }
            }
        }
    }
    saw_protocol
}

fn ancestor_repo_prefixes(path: &str) -> Vec<&str> {
    let mut prefixes = Vec::new();
    let mut rest = path;
    loop {
        let Some((parent, child)) = rest.rsplit_once('/') else {
            break;
        };
        if parent.is_empty() || child.is_empty() {
            break;
        }
        prefixes.push(parent);
        rest = parent;
    }
    prefixes
}

fn ls_tree_has_gitlink(
    root: &Path,
    isolated: IsolatedGitEnvironment<'_>,
    treeish: &str,
    prefixes: &[&str],
) -> Result<bool, String> {
    if prefixes.is_empty() {
        return Ok(false);
    }
    let mut args: Vec<OsString> = vec![
        "--literal-pathspecs".into(),
        "ls-tree".into(),
        "-z".into(),
        "--full-tree".into(),
        treeish.into(),
        "--".into(),
    ];
    for prefix in prefixes {
        args.push(OsString::from(*prefix));
    }
    let output = run_git_bounded_os(root, &args, MAX_CATALOG_BYTES, Some(isolated))?;
    if output.truncated {
        return Err("gitlink prefix list exceeded the size limit".to_owned());
    }
    if let Some(error) = output.error {
        return Err(error);
    }
    for record in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
    {
        let Some(tab) = record.iter().position(|byte| *byte == b'\t') else {
            continue;
        };
        let mode = record[..tab]
            .split(|byte| byte.is_ascii_whitespace())
            .next()
            .unwrap_or(&[]);
        if mode == b"160000" {
            return Ok(true);
        }
    }
    Ok(false)
}

fn file_path_is_inside_gitlink(
    root: &Path,
    isolated: IsolatedGitEnvironment<'_>,
    scope: &ReviewScope,
    paging: &ReviewPagingState,
    path: &str,
) -> Result<bool, String> {
    let prefixes = ancestor_repo_prefixes(path);
    if prefixes.is_empty() {
        return Ok(false);
    }
    match scope {
        ReviewScope::Revisions { .. } | ReviewScope::Commit { .. } => {
            let Some(revisions) = &paging.revisions else {
                return Ok(false);
            };
            if ls_tree_has_gitlink(root, isolated, &revisions.from, &prefixes)? {
                return Ok(true);
            }
            ls_tree_has_gitlink(root, isolated, &revisions.to, &prefixes)
        }
        ReviewScope::WorkingTree | ReviewScope::BaseToWorkingTree { .. } => {
            let specs: BTreeSet<OsString> = prefixes
                .iter()
                .map(|prefix| OsString::from(*prefix))
                .collect();
            let gitlinks = index_gitlink_paths(root, isolated, &specs)?;
            Ok(prefixes
                .iter()
                .any(|prefix| gitlinks.contains(&OsString::from(*prefix))))
        }
    }
}

fn empty_gitlink_blobs() -> FileContextBlobs {
    FileContextBlobs {
        old: FileBlob::empty(),
        new: FileBlob::empty(),
    }
}

fn blob_slice(lines: &[String], start: u32, count: u32) -> &[String] {
    if start == 0 || count == 0 {
        return &[];
    }
    let index = (start as usize).saturating_sub(1);
    if index >= lines.len() {
        return &[];
    }
    let end = (index + count as usize).min(lines.len());
    &lines[index..end]
}

fn load_working_tree_blobs(
    authority: &WorkingTreePagingAuthority,
    file: &DiffFile,
) -> Result<FileContextBlobs, String> {
    if let Some(source) = authority.submodule_files.get(&file.path) {
        let child = &source.authority;
        return load_pinned_working_tree_blobs(
            child.sandbox.work_tree(),
            child.sandbox.environment(),
            &child.base,
            file,
            source.path.clone(),
            None,
        );
    }
    load_pinned_working_tree_blobs(
        &authority.root,
        authority.sandbox.environment(&authority.root),
        &authority.base,
        file,
        ReviewPathBytes {
            path: decode_path_bytes(&file.path),
            previous_path: file.previous_path.as_deref().map(decode_path_bytes),
        },
        authority.untracked.get(&file.path),
    )
}

fn load_pinned_working_tree_blobs(
    root: &Path,
    isolated: IsolatedGitEnvironment<'_>,
    base: &str,
    file: &DiffFile,
    paths: ReviewPathBytes,
    untracked: Option<&UntrackedPagingBody>,
) -> Result<FileContextBlobs, String> {
    let old = if file.status == FileStatus::Added {
        FileBlob::empty()
    } else {
        git_show_blob(
            root,
            isolated,
            Some(base),
            paths.previous_path.unwrap_or_else(|| paths.path.clone()),
        )?
    };
    let new = if file.status == FileStatus::Deleted {
        FileBlob::empty()
    } else if let Some(body) = untracked {
        untracked_blob(body)?
    } else {
        git_show_blob(root, isolated, None, paths.path)?
    };
    Ok(FileContextBlobs { old, new })
}

fn load_revision_blobs(
    cwd: &Path,
    paging: &ReviewPagingState,
    revisions: &PinnedRevisions,
    file: &DiffFile,
) -> Result<FileContextBlobs, String> {
    if let Some(source) = paging.submodule_files.get(&file.path) {
        return load_submodule_revision_blobs(source, file);
    }
    let repository = RepositoryLayout::discover(cwd)?;
    if let Err(error) = repository_attributes_are_safe(&repository) {
        return Err(error);
    }
    let head_reference = read_small_file(&repository.git_dir.join("HEAD"), MAX_GIT_METADATA_BYTES)
        .and_then(|bytes| {
            String::from_utf8(bytes).map_err(|_| "git HEAD is not valid UTF-8".to_owned())
        })
        .map(|head| strip_output_line_ending(&head).to_owned())?;
    let object_format = detect_object_format(&repository)?;
    let sandbox = GitSandbox::create(&repository, &object_format, &head_reference)?;
    let root = repository.root.clone();
    let isolated = sandbox.environment(&root);
    let bytes = paging.path_bytes(file);
    let old = if file.status == FileStatus::Added {
        FileBlob::empty()
    } else {
        git_show_blob(
            &root,
            isolated,
            Some(revisions.from.as_str()),
            bytes.previous_path.clone().unwrap_or_else(|| bytes.path.clone()),
        )?
    };
    let new = if file.status == FileStatus::Deleted {
        FileBlob::empty()
    } else {
        git_show_blob(&root, isolated, Some(revisions.to.as_str()), bytes.path)?
    };
    Ok(FileContextBlobs { old, new })
}

fn load_submodule_revision_blobs(
    source: &SubmodulePagingFile,
    file: &DiffFile,
) -> Result<FileContextBlobs, String> {
    let child = &source.authority;
    let Some(end) = child.end.as_deref() else {
        return Err("expanded submodule revision end is unavailable".to_owned());
    };
    let isolated = child.sandbox.environment();
    let root = child.sandbox.work_tree();
    let old = if file.status == FileStatus::Added {
        FileBlob::empty()
    } else {
        git_show_blob(
            root,
            isolated,
            Some(child.base.as_str()),
            source
                .path
                .previous_path
                .clone()
                .unwrap_or_else(|| source.path.path.clone()),
        )?
    };
    let new = if file.status == FileStatus::Deleted {
        FileBlob::empty()
    } else {
        git_show_blob(root, isolated, Some(end), source.path.path.clone())?
    };
    Ok(FileContextBlobs { old, new })
}

fn untracked_blob(body: &UntrackedPagingBody) -> Result<FileBlob, String> {
    match body {
        UntrackedPagingBody::Copy(path) => read_blob_file(path),
        UntrackedPagingBody::Patch { bytes, truncated } => {
            // Producer contract: one single-file `--no-index /dev/null <capture>`
            // record. Anything else is a producer bug and fails closed.
            let stdout = String::from_utf8_lossy(bytes);
            let files = parse_unified_diff(&stdout);
            if files.len() > 1 {
                return Err("untracked file body captured more than one diff record".to_owned());
            }
            let Some(file) = files.first() else {
                return Ok(FileBlob::empty());
            };
            let mut lines = Vec::new();
            for hunk in &file.hunks {
                for line in &hunk.lines {
                    if line.kind == DiffLineKind::Addition {
                        lines.push(line.text.clone());
                    }
                }
            }
            Ok(FileBlob {
                lines,
                binary: file.binary,
                truncated: *truncated,
            })
        }
        UntrackedPagingBody::Unpinned => Err(
            "untracked file body was not captured when this snapshot was opened; refresh the review to re-capture it"
                .to_owned(),
        ),
    }
}

fn read_blob_file(path: &Path) -> Result<FileBlob, String> {
    let bytes = fs::read(path).map_err(|error| format!("reading captured file: {error}"))?;
    let truncated = bytes.len() > MAX_FILE_DIFF_BYTES;
    let slice = if truncated {
        &bytes[..MAX_FILE_DIFF_BYTES]
    } else {
        bytes.as_slice()
    };
    Ok(FileBlob::from_bytes(slice, truncated))
}

fn tree_listing_record_is_gitlink(record: &[u8]) -> bool {
    let Some(mode) = record
        .split(|byte| byte.is_ascii_whitespace())
        .find(|field| !field.is_empty())
    else {
        return false;
    };
    mode == b"160000"
}

fn git_show_blob(
    root: &Path,
    isolated: IsolatedGitEnvironment<'_>,
    revision: Option<&str>,
    path: Vec<u8>,
) -> Result<FileBlob, String> {
    if path.is_empty() || path.contains(&0) {
        return Err("invalid context path".to_owned());
    }
    let pathspec = os_string_from_git_path(path.clone());
    let args: Vec<OsString> = match revision {
        Some(revision) => vec![
            "--literal-pathspecs".into(),
            "ls-tree".into(),
            "-z".into(),
            "--full-tree".into(),
            revision.into(),
            "--".into(),
            pathspec,
        ],
        None => vec![
            "--literal-pathspecs".into(),
            "ls-files".into(),
            "-s".into(),
            "-z".into(),
            "--".into(),
            pathspec,
        ],
    };
    let modes = run_git_bounded_os(root, &args, MAX_CATALOG_BYTES, Some(isolated))?;
    if let Some(error) = modes.error {
        return Err(error);
    }
    if modes.truncated {
        return Err("context path mode list exceeded the size limit".to_owned());
    }
    if modes
        .stdout
        .split(|byte| *byte == 0)
        .any(tree_listing_record_is_gitlink)
    {
        return Ok(FileBlob::empty());
    }
    let mut spec = OsString::new();
    match revision {
        None => spec.push(":"),
        Some(revision) => {
            spec.push(revision);
            spec.push(":");
        }
    }
    spec.push(os_string_from_git_path(path));
    let args = vec![
        OsString::from("--literal-pathspecs"),
        OsString::from("show"),
        OsString::from("--no-textconv"),
        OsString::from("--no-ext-diff"),
        OsString::from("--end-of-options"),
        spec,
    ];
    let output = run_git_bounded_os(root, &args, MAX_FILE_DIFF_BYTES, Some(isolated))?;
    if let Some(error) = output.error {
        return Err(error);
    }
    Ok(FileBlob::from_bytes(&output.stdout, output.truncated))
}

impl FileDiff {
    #[must_use]
    pub fn estimated_heap_bytes(&self) -> usize {
        let mut total = std::mem::size_of::<Self>()
            + self.path.capacity()
            + self.previous_path.as_ref().map_or(0, |p| p.capacity())
            + self.error.as_ref().map_or(0, |e| e.capacity());
        total += self.hunks.capacity() * std::mem::size_of::<DiffHunk>();
        for hunk in &self.hunks {
            total += hunk.header.capacity()
                + hunk.lines.capacity() * std::mem::size_of::<DiffLine>();
            for line in &hunk.lines {
                total += line.text.capacity();
            }
        }
        total
    }

    /// Load a single file's full diff for the same comparison the snapshot
    /// used. `file` is the snapshot's [`DiffFile`] entry (path + rename
    /// provenance), so the pathspec is containment-derived and never
    /// attacker-controlled. Output is bounded to [`MAX_FILE_DIFF_BYTES`] and
    /// each line's text is capped to [`MAX_DIFF_LINE_TEXT_BYTES`].
    #[must_use]
    pub fn load(cwd: &Path, scope: &ReviewScope, file: &DiffFile) -> Self {
        let paging = match scope {
            ReviewScope::WorkingTree | ReviewScope::BaseToWorkingTree { .. } => {
                ReviewPagingState::default()
            }
            ReviewScope::Revisions { from, to } => {
                let revisions = resolve_review_commit(cwd, from, "source").and_then(|from| {
                    resolve_review_commit(cwd, to, "target").map(|to| PinnedRevisions { from, to })
                });
                match revisions {
                    Ok(revisions) => ReviewPagingState {
                        working_tree: None,
                        revisions: Some(revisions),
                        paths: BTreeMap::new(),
                        submodule_files: BTreeMap::new(),
                    },
                    Err(error) => {
                        return Self::error(file.path.clone(), file.previous_path.clone(), error);
                    }
                }
            }
            ReviewScope::Commit { .. } => {
                return Self::error(
                    file.path.clone(),
                    file.previous_path.clone(),
                    "commit paging state is unavailable for this snapshot".to_owned(),
                );
            }
        };
        Self::load_with_root(cwd, scope, &paging, file)
    }

    pub(crate) fn load_snapshot_file(
        cwd: &Path,
        scope: &ReviewScope,
        paging: &ReviewPagingState,
        file: &DiffFile,
    ) -> Self {
        Self::load_with_root(cwd, scope, paging, file)
    }

    fn load_with_root(
        cwd: &Path,
        scope: &ReviewScope,
        paging: &ReviewPagingState,
        file: &DiffFile,
    ) -> Self {
        // Display escaping is never path authority: revision snapshots retain
        // the catalog's original NUL-delimited Git bytes, while working-tree
        // snapshot paging uses the prepared sandbox captured at open.
        let path_bytes = paging.path_bytes(file);
        let mut pathspecs: Vec<OsString> = Vec::new();
        if let Some(previous) = &file.previous_path {
            let normalized = normalize_repo_path(previous);
            if normalized.is_empty() || normalized == "." || normalized.contains('\0') {
                return Self::error(
                    file.path.clone(),
                    file.previous_path.clone(),
                    format!("invalid rename source {previous:?}"),
                );
            }
            let bytes = path_bytes
                .previous_path
                .unwrap_or_else(|| normalized.into_bytes());
            pathspecs.push(os_string_from_git_path(bytes));
        }
        let normalized = normalize_repo_path(&file.path);
        if normalized.is_empty() || normalized == "." || normalized.contains('\0') {
            return Self::error(
                file.path.clone(),
                file.previous_path.clone(),
                format!("invalid file path {:?}", file.path),
            );
        }
        if !file.truncated && is_gitlink_diff_file(file) {
            return Self {
                path: file.path.clone(),
                previous_path: file.previous_path.clone(),
                status: file.status,
                binary: false,
                hunks: file.hunks.clone(),
                byte_capped: false,
                error: None,
            };
        }
        pathspecs.push(os_string_from_git_path(path_bytes.path));

        if let Some(source) = paging.submodule_files.get(&file.path) {
            let mut submodule_pathspecs = Vec::new();
            if file.previous_path.is_some() {
                let Some(previous_path) = source.path.previous_path.clone() else {
                    return Self::error(
                        file.path.clone(),
                        file.previous_path.clone(),
                        "expanded submodule rename source is unavailable".to_owned(),
                    );
                };
                submodule_pathspecs.push(os_string_from_git_path(previous_path));
            }
            submodule_pathspecs.push(os_string_from_git_path(source.path.path.clone()));
            let child = &source.authority;
            let Some(end) = child.end.as_deref() else {
                return Self::error(
                    file.path.clone(),
                    file.previous_path.clone(),
                    "expanded submodule revision end is unavailable".to_owned(),
                );
            };
            let output = Self::run_revisions(
                child.sandbox.work_tree(),
                child.sandbox.environment(),
                &submodule_pathspecs,
                &child.base,
                end,
            );
            return match output {
                Ok(output) => Self::from_single_file_output(file, output),
                Err(error) => Self::error(file.path.clone(), file.previous_path.clone(), error),
            };
        }

        if scope.includes_working_tree() {
            if let Some(authority) = &paging.working_tree {
                if let Some(source) = authority.submodule_files.get(&file.path) {
                    let mut submodule_pathspecs = Vec::new();
                    if file.previous_path.is_some() {
                        let Some(previous_path) = source.path.previous_path.clone() else {
                            return Self::error(
                                file.path.clone(),
                                file.previous_path.clone(),
                                "expanded submodule rename source is unavailable".to_owned(),
                            );
                        };
                        submodule_pathspecs.push(os_string_from_git_path(previous_path));
                    }
                    submodule_pathspecs
                        .push(os_string_from_git_path(source.path.path.clone()));
                    let child = &source.authority;
                    let output = Self::run_pinned_working_tree(
                        child.sandbox.work_tree(),
                        child.sandbox.environment(),
                        &submodule_pathspecs,
                        &child.base,
                    );
                    return match output {
                        Ok(output) => Self::from_single_file_output(file, output),
                        Err(error) => {
                            Self::error(file.path.clone(), file.previous_path.clone(), error)
                        }
                    };
                }
                let isolated = authority.sandbox.environment(&authority.root);
                if pathspecs.len() == 1 {
                    if let Some(body) = authority.untracked.get(&file.path) {
                        return match body {
                            UntrackedPagingBody::Patch { bytes, truncated } => {
                                Self::from_single_file_output(
                                    file,
                                    GitOutput {
                                        stdout: bytes.clone(),
                                        truncated: *truncated,
                                        error: None,
                                    },
                                )
                            }
                            UntrackedPagingBody::Copy(copy_path) => {
                                match Self::run_untracked_copy(&authority.root, copy_path) {
                                    Ok(output) => Self::from_single_file_output(file, output),
                                    Err(error) => {
                                        Self::error(
                                            file.path.clone(),
                                            file.previous_path.clone(),
                                            error,
                                        )
                                    }
                                }
                            }
                            UntrackedPagingBody::Unpinned => Self::error(
                                file.path.clone(),
                                file.previous_path.clone(),
                                "untracked file body was not captured when this snapshot was opened; refresh the review to re-capture it".to_owned(),
                            ),
                        };
                    }
                }
                let output = Self::run_pinned_working_tree(
                    &authority.root,
                    isolated,
                    &pathspecs,
                    &authority.base,
                );
                return match output {
                    Ok(output) => Self::from_output(file, output),
                    Err(error) => Self::error(file.path.clone(), file.previous_path.clone(), error),
                };
            }
        }

        let repository = match RepositoryLayout::discover(cwd) {
            Ok(repository) => repository,
            Err(error) => {
                return Self::error(
                    file.path.clone(),
                    file.previous_path.clone(),
                    format_git_error("not a git repository", &error),
                );
            }
        };
        if let Err(error) = repository_attributes_are_safe(&repository) {
            return Self::error(file.path.clone(), file.previous_path.clone(), error);
        }
        let head_reference =
            match read_small_file(&repository.git_dir.join("HEAD"), MAX_GIT_METADATA_BYTES)
                .and_then(|bytes| {
                    String::from_utf8(bytes).map_err(|_| "git HEAD is not valid UTF-8".to_owned())
                }) {
                Ok(head) => strip_output_line_ending(&head).to_owned(),
                Err(error) => {
                    return Self::error(file.path.clone(), file.previous_path.clone(), error);
                }
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
        match file_path_is_inside_gitlink(&root, isolated, scope, paging, &normalized) {
            Ok(true) => {
                return Self::error(
                    file.path.clone(),
                    file.previous_path.clone(),
                    "review paging state is unavailable for this snapshot".to_owned(),
                );
            }
            Ok(false) => {}
            Err(error) => {
                return Self::error(file.path.clone(), file.previous_path.clone(), error);
            }
        }

        let output = match scope {
            ReviewScope::WorkingTree | ReviewScope::BaseToWorkingTree { .. } => {
                match Self::try_untracked_working_tree(&root, Some(isolated), &pathspecs, file) {
                    Some(Ok(output)) => return Self::from_single_file_output(file, output),
                    Some(Err(error)) => {
                        return Self::error(file.path.clone(), file.previous_path.clone(), error);
                    }
                    None => {}
                }
                match Self::resolve_working_tree_base(&root, isolated, scope) {
                    Ok(base) => Self::run_current_working_tree(&root, isolated, &pathspecs, &base),
                    Err(error) => Err(error),
                }
            }
            ReviewScope::Revisions { .. } | ReviewScope::Commit { .. } => match &paging.revisions {
                Some(revisions) => {
                    Self::run_revisions(&root, isolated, &pathspecs, &revisions.from, &revisions.to)
                }
                None => Err("revision paging state is unavailable for this snapshot".to_owned()),
            },
        };
        match output {
            Ok(output) => Self::from_output(file, output),
            Err(error) => Self::error(file.path.clone(), file.previous_path.clone(), error),
        }
    }

    /// On-demand paging for an untracked Added entry, mirroring the snapshot
    /// capture: list the path via `ls-files --others --exclude-standard`
    /// (metadata-only) and, when it is genuinely untracked, capture
    /// `git diff --no-index -- /dev/null <path>` with the same bounded
    /// runner (exit 1 = differences found). `None` means the path is not an
    /// untracked reviewable entry and the tracked pipeline must run.
    fn try_untracked_working_tree(
        root: &Path,
        isolated: Option<IsolatedGitEnvironment<'_>>,
        pathspecs: &[OsString],
        file: &DiffFile,
    ) -> Option<Result<GitOutput, String>> {
        if file.status != FileStatus::Added
            || file.previous_path.is_some()
            || pathspecs.len() != 1
        {
            return None;
        }
        let pathspec = &pathspecs[0];
        let list_args = vec![
            OsString::from("ls-files"),
            OsString::from("--others"),
            OsString::from("--exclude-standard"),
            OsString::from("-z"),
            OsString::from("--"),
            pathspec.clone(),
        ];
        let listing = match run_git_bounded_os(root, &list_args, MAX_UNTRACKED_CATALOG_BYTES, isolated)
        {
            Ok(output) => output,
            Err(error) => return Some(Err(error)),
        };
        if listing
            .stdout
            .split(|byte| *byte == 0)
            .all(|entry| entry.is_empty())
        {
            return None;
        }
        let args = vec![
            OsString::from("diff"),
            OsString::from("--no-ext-diff"),
            OsString::from("--no-textconv"),
            OsString::from("--no-index"),
            OsString::from("--"),
            OsString::from(null_device()),
            pathspec.clone(),
        ];
        // Bounded, pathspec-fixed invocation: `--no-index` never touches the
        // index and the pathspec was already containment-checked in
        // `load_with_root`.
        let mut output = match run_git_bounded_os(root, &args, MAX_FILE_DIFF_BYTES, None) {
            Ok(output) => output,
            Err(error) => return Some(Err(error)),
        };
        // Exit 1 means "differences found": normal for `--no-index` against
        // /dev/null. Only a genuinely empty run with an error is a failure.
        if output.error.is_some() && !output.stdout.is_empty() {
            output.error = None;
        }
        Some(Ok(output))
    }
    fn resolve_working_tree_base(
        root: &Path,
        isolated: IsolatedGitEnvironment<'_>,
        scope: &ReviewScope,
    ) -> Result<String, String> {
        match scope {
            ReviewScope::WorkingTree => git_rev_parse(root, &["--verify", "HEAD"], Some(isolated))
                .map(|head| strip_output_line_ending(&head).to_owned())
                .map_err(|_| "repository has no commits yet (unborn HEAD)".to_owned()),
            ReviewScope::BaseToWorkingTree { base } => resolve_review_commit(root, base, "source"),
            ReviewScope::Revisions { .. } | ReviewScope::Commit { .. } => {
                Err("working-tree paging cannot load a revision scope".to_owned())
            }
        }
    }

    fn run_current_working_tree(
        root: &Path,
        isolated: IsolatedGitEnvironment<'_>,
        pathspecs: &[OsString],
        base: &str,
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
                    return Err(output.error.unwrap_or_else(|| {
                        "git read-tree output exceeded the metadata limit".to_owned()
                    }));
                }
                Err(error) => return Err(error),
            }
        }
        // Mirror the snapshot loader: the copied index already carries staged
        // deletions. Only target paths that remain index members may be
        // updated from the worktree; index-absent paths must stay deleted, and
        // `add -u` must never receive an empty path set (that updates all).
        let mut ls_args: Vec<OsString> = vec![
            "--literal-pathspecs".into(),
            "ls-files".into(),
            "-z".into(),
            "--".into(),
        ];
        ls_args.extend(pathspecs.iter().cloned());
        let index_output = match run_git_bounded_os(root, &ls_args, MAX_DIFF_BYTES, Some(isolated))
        {
            Ok(output) if output.error.is_none() && !output.truncated => output,
            Ok(output) => {
                return Err(output
                    .error
                    .unwrap_or_else(|| "tracked path list exceeded the size limit".to_owned()));
            }
            Err(error) => return Err(error),
        };
        let index_members = index_output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
            .map(|entry| os_string_from_git_path(entry.to_vec()))
            .collect::<Vec<_>>();
        if !index_members.is_empty() {
            let mut add_args: Vec<OsString> = vec![
                "--literal-pathspecs".into(),
                "add".into(),
                "-u".into(),
                "--".into(),
            ];
            add_args.extend(index_members);
            match run_git_bounded_os(root, &add_args, MAX_GIT_METADATA_BYTES, Some(isolated)) {
                Ok(output) if output.error.is_none() && !output.truncated => {}
                Ok(output) => {
                    return Err(output.error.unwrap_or_else(|| {
                        "git add output exceeded the metadata limit".to_owned()
                    }));
                }
                Err(error) => return Err(error),
            }
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
            "--ignore-submodules=none".into(),
            "--find-renames".into(),
            "--histogram".into(),
            base.into(),
            "-O".into(),
            null_device().into(),
            "--".into(),
        ];
        args.extend(pathspecs.iter().cloned());
        run_git_bounded_os(root, &args, MAX_FILE_DIFF_BYTES, Some(isolated))
    }


    fn run_untracked_copy(root: &Path, copy_path: &Path) -> Result<GitOutput, String> {
        let args = vec![
            OsString::from("diff"),
            OsString::from("--no-ext-diff"),
            OsString::from("--no-textconv"),
            OsString::from("--no-index"),
            OsString::from("--"),
            OsString::from(null_device()),
            OsString::from(copy_path),
        ];
        let mut output = run_git_bounded_os(root, &args, MAX_FILE_DIFF_BYTES, None)?;
        if output.error.is_some() && !output.stdout.is_empty() {
            output.error = None;
        }
        Ok(output)
    }
    fn run_pinned_working_tree(
        root: &Path,
        isolated: IsolatedGitEnvironment<'_>,
        pathspecs: &[OsString],
        base: &str,
    ) -> Result<GitOutput, String> {
        let mut args: Vec<OsString> = vec![
            "--literal-pathspecs".into(),
            "diff".into(),
            "--cached".into(),
            "--no-ext-diff".into(),
            "--no-textconv".into(),
            "--no-color".into(),
            "--default-prefix".into(),
            "--no-relative".into(),
            "--ignore-submodules=none".into(),
            "--find-renames".into(),
            "--histogram".into(),
            base.into(),
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
        let from_commit = from;
        let to_commit = to;
        let mut args: Vec<OsString> = vec![
            "--literal-pathspecs".into(),
            "diff".into(),
            "--no-ext-diff".into(),
            "--no-textconv".into(),
            "--no-color".into(),
            "--default-prefix".into(),
            "--no-relative".into(),
            "--ignore-submodules=none".into(),
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
        // Pick the entry matching the snapshot file's path. A rename with two
        // pathspecs still produces one entry whose path == file.path. A
        // producer that renders the patch under a different display path (the
        // `--no-index /dev/null <capture>` untracked body) binds its known
        // identity through `from_single_file_output` before this point, so no
        // parsed entry may ever stand in for another file: a mismatch is a
        // path-specific error, never another file's hunks.
        let entry = files
            .iter()
            .find(|parsed| parsed.path == file.path);
        let Some(entry) = entry else {
            if !file.hunks.is_empty() {
                return Self {
                    path: file.path.clone(),
                    previous_path: file.previous_path.clone(),
                    status: file.status,
                    binary: file.binary,
                    hunks: file.hunks.clone(),
                    byte_capped: false,
                    error: None,
                };
            }
            return Self {
                path: file.path.clone(),
                previous_path: file.previous_path.clone(),
                status: file.status,
                binary: false,
                hunks: Vec::new(),
                byte_capped: false,
                error: Some(
                    "file diff is empty (the path may no longer differ in this comparison)"
                        .to_owned(),
                ),
            };
        };
        let hunks = entry
            .hunks
            .iter()
            .map(|hunk| DiffHunk {
                header: hunk.header.clone(),
                old_start: hunk.old_start,
                old_count: hunk.old_count,
                new_start: hunk.new_start,
                new_count: hunk.new_count,
                lines: hunk
                    .lines
                    .iter()
                    .map(|line| cap_line(line.clone()))
                    .collect(),
            })
            .collect();
        Self {
            path: file.path.clone(),
            previous_path: file.previous_path.clone(),
            status: file.status,
            binary: entry.binary,
            hunks,
            byte_capped: output.truncated,
            error: None,
        }
    }

    /// Load a producer-bound single-file body: `git diff --no-index /dev/null
    /// <capture>` renders the capture path, never the snapshot identity, so
    /// the caller's known file identity is rebound here. More than one parsed
    /// record breaks the producer contract and fails closed.
    fn from_single_file_output(file: &DiffFile, output: GitOutput) -> Self {
        if let Some(error) = output.error {
            return Self::error(file.path.clone(), file.previous_path.clone(), error);
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut files = parse_unified_diff(&stdout);
        if files.len() > 1 {
            return Self::error(
                file.path.clone(),
                file.previous_path.clone(),
                "untracked file body captured more than one diff record".to_owned(),
            );
        }
        let Some(mut parsed) = files.pop() else {
            return Self::error(
                file.path.clone(),
                file.previous_path.clone(),
                "file diff is empty (the path may no longer differ in this comparison)"
                    .to_owned(),
            );
        };
        parsed.path = file.path.clone();
        let hunks = parsed
            .hunks
            .iter()
            .map(|hunk| DiffHunk {
                header: hunk.header.clone(),
                old_start: hunk.old_start,
                old_count: hunk.old_count,
                new_start: hunk.new_start,
                new_count: hunk.new_count,
                lines: hunk
                    .lines
                    .iter()
                    .map(|line| cap_line(line.clone()))
                    .collect(),
            })
            .collect();
        Self {
            path: file.path.clone(),
            previous_path: file.previous_path.clone(),
            status: file.status,
            binary: parsed.binary,
            hunks,
            byte_capped: output.truncated,
            error: None,
        }
    }

    fn error(path: String, previous_path: Option<String>, message: String) -> Self {
        Self {
            path,
            previous_path,
            status: FileStatus::Unknown,
            binary: false,
            hunks: Vec::new(),
            byte_capped: false,
            error: Some(message),
        }
    }
    pub(crate) fn hunk_is_comment_ready(&self, index: usize) -> bool {
        self.hunks.get(index).is_some_and(DiffHunk::is_complete)
            && !(self.byte_capped && index + 1 == self.hunks.len())
    }

    /// The page carries hunk windows: every hunk overlapping `[cursor, next)`
    /// is delivered as a [`FileDiffPageHunk`] with the COMPLETE hunk identity
    /// (header/ranges/content hash over the full hunk) plus the in-window
    /// subset of its lines and the subset's hunk-local offset. A hunk split
    /// across pages therefore keeps one stable identity, and the frontend
    /// merges the subsets deterministically by index.
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
        let mut ranges: Vec<(usize, usize)> = Vec::with_capacity(self.hunks.len());
        let mut offset = 0usize;
        for hunk in &self.hunks {
            let start = offset;
            offset += hunk.lines.len();
            ranges.push((start, offset));
        }
        let total = offset;
        if cursor > total {
            return Err(format!("cursor {cursor} out of range (total {total})"));
        }
        let cap = max_lines.clamp(1, MAX_FILE_PAGE_LINES);
        let page_overhead = PAGE_JSON_OVERHEAD_BYTES
            + 6 * usize::from(snapshot_id.len())
            + 6 * (self.path.len() + self.previous_path.as_ref().map_or(0, String::len));
        let mut page_hunks: Vec<FileDiffPageHunk> = Vec::new();
        let mut page_lines = 0usize;
        let mut page_bytes = page_overhead;
        let mut index = cursor;
        while index < total && page_lines < cap {
            let hunk_pos = ranges.partition_point(|&(_, end)| end <= index);
            let (h_start, h_end) = ranges[hunk_pos];
            let hunk = &self.hunks[hunk_pos];
            let local = index - h_start;
            let mut take = 0usize;
            while take < h_end - index && page_lines + take < cap {
                let line = &hunk.lines[local + take];
                let descriptor = if take == 0 { MAX_HUNK_DESCRIPTOR_BYTES } else { 0 };
                let line_bytes = json_escaped_len(line.text.as_bytes()) + LINE_JSON_OVERHEAD_BYTES;
                if page_lines + take > 0
                    && page_bytes + descriptor + line_bytes > MAX_FILE_PAGE_BYTES
                {
                    break;
                }
                page_bytes += descriptor + line_bytes;
                take += 1;
            }
            if take == 0 {
                break;
            }
            let content_hash = HunkIdentity::new(snapshot_id, &self.path, hunk).content_hash;
            page_hunks.push(FileDiffPageHunk {
                index: hunk_pos,
                header: hunk.header.clone(),
                old_start: hunk.old_start,
                old_count: hunk.old_count,
                new_start: hunk.new_start,
                new_count: hunk.new_count,
                content_hash,
                total_lines: hunk.lines.len(),
                complete: self.hunk_is_comment_ready(hunk_pos),
                line_start: local,
                lines: hunk.lines[local..local + take].to_vec(),
            });
            page_lines += take;
            index += take;
        }
        let next_cursor = index;
        let has_more = next_cursor < total;
        Ok(FileDiffPage {
            path: self.path.clone(),
            snapshot_id: snapshot_id.to_owned(),
            previous_path: self.previous_path.clone(),
            status: self.status,
            binary: self.binary,
            hunks: page_hunks,
            cursor,
            next_cursor: has_more.then_some(next_cursor),
            has_more,
            total_lines: total,
            hunk_count: self.hunks.len(),
            byte_capped: self.byte_capped,
        })
    }
}
const fn json_escape_cost(byte: u8) -> usize {
    match byte {
        b'"' | b'\\' | 0x08 | 0x09 | 0x0A | 0x0C | 0x0D => 2,
        0x00..=0x1F => 6,
        _ => 1,
    }
}

fn json_escaped_len(bytes: &[u8]) -> usize {
    bytes.iter().map(|&byte| json_escape_cost(byte)).sum()
}

const LINE_TRUNCATION_MARKER: &str = " ⋯[line truncated: exceeds byte cap]";
const HUNK_HEADER_TRUNCATION_MARKER: &str = " ⋯[hunk header truncated: exceeds byte cap]";

fn truncate_to_escaped_budget(text: &str, budget: usize, marker: &str) -> Option<String> {
    let marker_cost = json_escaped_len(marker.as_bytes());
    if json_escaped_len(text.as_bytes()) + marker_cost <= budget {
        return None;
    }
    let mut cost = 0usize;
    let mut end = 0usize;
    for (offset, &byte) in text.as_bytes().iter().enumerate() {
        let byte_cost = json_escape_cost(byte);
        if cost + byte_cost + marker_cost > budget {
            break;
        }
        cost += byte_cost;
        end = offset + 1;
    }
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = text[..end].to_owned();
    out.push_str(marker);
    Some(out)
}

fn cap_line(mut line: DiffLine) -> DiffLine {
    if let Some(capped) =
        truncate_to_escaped_budget(&line.text, MAX_DIFF_LINE_TEXT_BYTES, LINE_TRUNCATION_MARKER)
    {
        line.text = capped;
    }
    line
}

fn cap_hunk_header(header: String) -> String {
    truncate_to_escaped_budget(
        &header,
        MAX_HUNK_HEADER_BYTES,
        HUNK_HEADER_TRUNCATION_MARKER,
    )
    .unwrap_or(header)
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
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_output(cwd: &Path, args: &[&str]) -> Vec<u8> {
        let output = Command::new("git")
            .args([
                "-c",
                "color.ui=false",
                "-c",
                "core.hooksPath=.git-test-no-hooks",
            ])
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

    /// The workspace text a review error carries for `path`: review discovery
    /// resolves the workspace through `fs::canonicalize`.
    fn canonical_workspace(path: &Path) -> String {
        path.canonicalize()
            .expect("canonical workspace")
            .display()
            .to_string()
    }

    /// Authority set for a hand-built block: its own decoded `diff --git`
    /// paths, mirroring what `git diff --raw -z` discovery would produce.
    fn gitlink_paths_for(block: &[String]) -> BTreeSet<OsString> {
        let mut paths = BTreeSet::new();
        if let Some((from, to)) = diff_block_paths(block) {
            paths.insert(os_string_from_git_path(from));
            paths.insert(os_string_from_git_path(to));
        }
        paths
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
        write(
            repo.path().join("README.md").as_path(),
            "base\nstaged\nunstaged\n",
        );

        let snapshot = load_review_snapshot(repo.path());

        let error = snapshot.error.as_deref().unwrap_or_default();
        assert!(!error.contains("unknown option 'cached'"), "{error}");
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        let readme = snapshot
            .files
            .iter()
            .find(|f| f.path == "README.md")
            .expect("readme");
        assert_eq!(readme.insertions, 2);
        assert_eq!(
            snapshot
                .files
                .iter()
                .find(|f| f.path == "staged.txt")
                .expect("staged")
                .status,
            FileStatus::Added
        );
        assert_eq!(fs::read(&index).expect("index after"), index_before);
    }

    #[test]
    fn load_review_snapshot_excludes_stashed_files_from_working_tree() {
        // A stash is not part of HEAD→working tree. Stashed files must never
        // appear in the snapshot — not even namespaced `stash@{N}:…` rows.
        let repo = init_repo();
        write(repo.path().join("stashed.txt").as_path(), "stashed-body\n");
        git(repo.path(), &["add", "stashed.txt"]);
        git(repo.path(), &["stash", "push", "-u", "-m", "review stash"]);
        write(repo.path().join("README.md").as_path(), "base\nmodified\n");
        write(repo.path().join("untracked.txt").as_path(), "untracked\n");

        let snapshot = load_review_snapshot(repo.path());
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        assert!(
            snapshot.files.iter().any(|file| file.path == "README.md"),
            "modified working-tree file missing: {:?}",
            snapshot.files
        );
        assert!(
            snapshot.files.iter().any(|file| file.path == "untracked.txt"),
            "real untracked addition missing: {:?}",
            snapshot.files
        );
        assert!(
            snapshot
                .files
                .iter()
                .all(|file| !file.path.starts_with("stash@{")),
            "stash rows must never enter a working-tree snapshot: {:?}",
            snapshot.files
        );
        assert!(
            snapshot
                .files
                .iter()
                .all(|file| !file.path.ends_with("stashed.txt")),
            "stashed file must not leak by suffix: {:?}",
            snapshot.files
        );
    }

    #[test]
    fn load_review_snapshot_excludes_stash_only_clean_worktree() {
        let repo = init_repo();
        write(repo.path().join("stashed-only.txt").as_path(), "body\n");
        git(repo.path(), &["add", "stashed-only.txt"]);
        git(repo.path(), &["stash", "push", "-u", "-m", "only stash"]);

        let snapshot = load_review_snapshot(repo.path());
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        assert!(
            snapshot.files.is_empty(),
            "clean worktree must stay empty despite a stash: {:?}",
            snapshot.files
        );
        assert_eq!(snapshot.total_insertions, 0);
        assert_eq!(snapshot.total_deletions, 0);
    }


    #[test]
    fn review_scope_accepts_zero_one_or_two_revisions() {
        assert_eq!(
            ReviewScope::parse(None).expect("working tree scope"),
            ReviewScope::WorkingTree
        );
        assert_eq!(
            ReviewScope::parse(Some("")).expect("empty args are working tree"),
            ReviewScope::WorkingTree
        );
        assert_eq!(
            ReviewScope::parse(Some("main")).expect("single base"),
            ReviewScope::BaseToWorkingTree {
                base: "main".to_owned(),
            }
        );
        assert_eq!(
            ReviewScope::parse(Some("main feature")).expect("revision scope"),
            ReviewScope::Revisions {
                from: "main".to_owned(),
                to: "feature".to_owned(),
            }
        );
        assert_eq!(
            ReviewScope::parse(Some("a b c")).expect_err("three revisions must fail"),
            ReviewScope::USAGE
        );
    }

    fn commit_scope(revision: &str) -> ReviewScope {
        ReviewScope::Commit {
            commit: revision.to_owned(),
            label: String::new(),
        }
    }

    fn head_id(cwd: &Path) -> String {
        let head = String::from_utf8(git_output(cwd, &["rev-parse", "HEAD"])).expect("head");
        strip_output_line_ending(&head).to_owned()
    }

    fn short_id(id: &str) -> String {
        id.chars().take(12).collect()
    }

    #[test]
    fn code_review_parse_does_not_accept_a_commit_operand() {
        assert_eq!(
            ReviewScope::parse(Some("HEAD")).expect("one token stays a base"),
            ReviewScope::BaseToWorkingTree {
                base: "HEAD".to_owned(),
            }
        );
    }

    #[test]
    fn commit_review_root_diffs_against_empty_tree() {
        let repo = init_repo();
        let root = head_id(repo.path());
        let snapshot = load_review_snapshot_for(repo.path(), commit_scope(&root));
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        assert_eq!(
            snapshot.scope,
            ReviewScope::Commit {
                commit: root.clone(),
                label: format!("empty tree → {}", short_id(&root)),
            }
        );
        assert!(snapshot_paths(&snapshot).contains(&"README.md"));
        assert!(!snapshot.scope.includes_working_tree());
        let again = load_review_snapshot_for(repo.path(), snapshot.scope.clone());
        assert_eq!(again.scope, snapshot.scope);
        assert!(again.error.is_none(), "{:?}", again.error);
    }

    #[test]
    fn commit_review_normal_commit_is_first_parent_diff() {
        let repo = init_repo();
        let parent = head_id(repo.path());
        write(repo.path().join("second.txt").as_path(), "second\n");
        git(repo.path(), &["add", "second.txt"]);
        git(repo.path(), &["commit", "-m", "second"]);
        let child = head_id(repo.path());
        write(repo.path().join("dirty.txt").as_path(), "dirty\n");
        let snapshot = load_review_snapshot_for(repo.path(), commit_scope("HEAD"));
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        assert_eq!(
            snapshot.comparison_label(),
            format!("{} → {}", short_id(&parent), short_id(&child))
        );
        match &snapshot.scope {
            ReviewScope::Commit { commit, .. } => assert_eq!(commit, &child),
            other => panic!("scope was not pinned: {other:?}"),
        }
        assert_eq!(snapshot_paths(&snapshot), vec!["second.txt"]);
    }

    #[test]
    fn commit_review_merge_uses_first_parent_only() {
        let repo = init_repo();
        git(repo.path(), &["checkout", "-b", "side"]);
        write(repo.path().join("side.txt").as_path(), "side\n");
        git(repo.path(), &["add", "side.txt"]);
        git(repo.path(), &["commit", "-m", "side"]);
        git(repo.path(), &["checkout", "main"]);
        write(repo.path().join("main.txt").as_path(), "main\n");
        git(repo.path(), &["add", "main.txt"]);
        git(repo.path(), &["commit", "-m", "main"]);
        let first = head_id(repo.path());
        git(
            repo.path(),
            &["merge", "--no-ff", "side", "-m", "merge side"],
        );
        let merge = head_id(repo.path());
        let snapshot = load_review_snapshot_for(repo.path(), commit_scope(&merge));
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        assert_eq!(
            snapshot.comparison_label(),
            format!("first parent {} → {} (merge)", short_id(&first), short_id(&merge))
        );
        let paths = snapshot_paths(&snapshot);
        assert!(paths.contains(&"side.txt"), "{paths:?}");
        assert!(!paths.contains(&"main.txt"), "{paths:?}");
    }

    #[test]
    fn commit_review_refresh_stays_on_pinned_hash_after_branch_moves() {
        let repo = init_repo();
        write(repo.path().join("pinned.txt").as_path(), "pinned\n");
        git(repo.path(), &["add", "pinned.txt"]);
        git(repo.path(), &["commit", "-m", "pinned"]);
        let pinned = head_id(repo.path());
        let opened = load_review_snapshot_for(repo.path(), commit_scope("main"));
        assert!(opened.error.is_none(), "{:?}", opened.error);
        write(repo.path().join("later.txt").as_path(), "later\n");
        git(repo.path(), &["add", "later.txt"]);
        git(repo.path(), &["commit", "-m", "later"]);
        assert_ne!(head_id(repo.path()), pinned);
        let refreshed = load_review_snapshot_for(repo.path(), opened.scope.clone());
        assert!(refreshed.error.is_none(), "{:?}", refreshed.error);
        match &refreshed.scope {
            ReviewScope::Commit { commit, .. } => assert_eq!(commit, &pinned),
            other => panic!("refresh drifted: {other:?}"),
        }
        assert_eq!(snapshot_paths(&refreshed), vec!["pinned.txt"]);
        assert!(!snapshot_paths(&refreshed).contains(&"later.txt"));
    }

    #[test]
    fn commit_review_rejects_non_git_invalid_and_mixed_shapes() {
        let dir = TempDir::new().expect("tempdir");
        let outside = load_review_snapshot_for(dir.path(), commit_scope("HEAD"));
        let outside_error = outside.error.as_deref().expect("non-git error");
        assert!(
            outside_error.contains("not a git repository"),
            "{outside_error}"
        );
        assert!(!outside.scope.includes_working_tree());
        assert_eq!(outside.comparison_label(), "HEAD");

        let repo = init_repo();
        let invalid = load_review_snapshot_for(repo.path(), commit_scope("missing-commit-ref"));
        let invalid_error = invalid.error.as_deref().expect("invalid commit");
        assert!(
            invalid_error.contains("invalid commit revision"),
            "{invalid_error}"
        );
        assert!(invalid.files.is_empty());

        assert_eq!(
            ReviewScope::parse(Some("HEAD feature extra")).expect_err("mixed usage"),
            ReviewScope::USAGE
        );
        let listed = list_recent_commits(dir.path(), 2).expect_err("non-git list");
        assert!(listed.contains("not a git repository"), "{listed}");
    }

    #[test]
    fn list_recent_commits_reports_parents_and_truncation() {
        let repo = init_repo();
        let root = head_id(repo.path());
        write(repo.path().join("second.txt").as_path(), "second\n");
        git(repo.path(), &["add", "second.txt"]);
        git(repo.path(), &["commit", "-m", "second subject"]);
        let child = head_id(repo.path());
        git(repo.path(), &["checkout", "-b", "side"]);
        write(repo.path().join("side.txt").as_path(), "side\n");
        git(repo.path(), &["add", "side.txt"]);
        git(repo.path(), &["commit", "-m", "side only"]);
        git(repo.path(), &["checkout", "main"]);
        git(
            repo.path(),
            &["merge", "--no-ff", "side", "-m", "merge subject"],
        );
        let merge = head_id(repo.path());
        let listed = list_recent_commits(repo.path(), 2).expect("list");
        assert!(listed.truncated);
        assert_eq!(listed.commits.len(), 2);
        assert_eq!(listed.commits[0].commit, merge);
        assert_eq!(listed.commits[0].parent_commit.as_deref(), Some(child.as_str()));
        assert_eq!(listed.commits[0].parent_count, 2);
        assert_eq!(listed.commits[0].subject, "merge subject");
        assert_eq!(listed.commits[0].author, "Pi Test");
        assert!(!listed.commits[0].author_date.is_empty());
        assert_eq!(listed.commits[1].commit, child);
        assert_eq!(listed.commits[1].parent_commit.as_deref(), Some(root.as_str()));
        assert_eq!(listed.commits[1].parent_count, 1);
        assert!(listed.commits[1].commit.len() >= 4);
        let full = list_recent_commits(repo.path(), 100).expect("full");
        assert!(!full.truncated);
        assert!(full.commits.iter().any(|entry| entry.commit == root && entry.parent_commit.is_none()));
    }

    #[test]
    fn comparison_label_replaces_invisible_format_controls() {
        let revisions = ReviewScope::Revisions {
            from: "main\u{202e}hidden".to_owned(),
            to: "feature\u{200b}branch".to_owned(),
        };
        assert_eq!(revisions.label(), "main hidden → feature branch");

        let base = ReviewScope::BaseToWorkingTree {
            base: "main\u{202e}hidden".to_owned(),
        };
        assert_eq!(base.label(), "main hidden → working tree");
        assert_eq!(
            ReviewScope::WorkingTree.label(),
            "HEAD → working tree"
        );
    }

    fn snapshot_paths(snapshot: &ReviewSnapshot) -> Vec<&str> {
        snapshot
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect()
    }

    fn layered_review_repo() -> (tempfile::TempDir, String) {
        let repo = init_repo();
        write(repo.path().join("unstaged.txt").as_path(), "base-D\n");
        git(repo.path(), &["add", "unstaged.txt"]);
        git(repo.path(), &["commit", "-m", "base A"]);
        let base = String::from_utf8(git_output(repo.path(), &["rev-parse", "HEAD"]))
            .expect("base hash");
        let base = strip_output_line_ending(&base).to_owned();
        write(repo.path().join("committed.txt").as_path(), "committed-B\n");
        git(repo.path(), &["add", "committed.txt"]);
        git(repo.path(), &["commit", "-m", "committed B"]);
        write(repo.path().join("staged.txt").as_path(), "staged-C\n");
        git(repo.path(), &["add", "staged.txt"]);
        write(repo.path().join("unstaged.txt").as_path(), "unstaged-D\n");
        write(repo.path().join("untracked.txt").as_path(), "untracked-E\n");
        (repo, base)
    }

    #[test]
    fn load_review_snapshot_single_ref_includes_committed_staged_unstaged_and_untracked() {
        let (repo, base) = layered_review_repo();
        let snapshot = load_review_snapshot_for(
            repo.path(),
            ReviewScope::BaseToWorkingTree { base: base.clone() },
        );
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        assert_eq!(snapshot.comparison_label(), format!("{base} → working tree"));
        let paths = snapshot_paths(&snapshot);
        assert!(paths.contains(&"committed.txt"), "{paths:?}");
        assert!(paths.contains(&"staged.txt"), "{paths:?}");
        assert!(paths.contains(&"unstaged.txt"), "{paths:?}");
        assert!(paths.contains(&"untracked.txt"), "{paths:?}");
        assert_eq!(
            paths
                .iter()
                .filter(|path| **path == "committed.txt")
                .count(),
            1
        );
        let committed = snapshot
            .files
            .iter()
            .find(|file| file.path == "committed.txt")
            .expect("committed B");
        assert_eq!(committed.status, FileStatus::Added);
        let staged = snapshot
            .files
            .iter()
            .find(|file| file.path == "staged.txt")
            .expect("staged C");
        assert_eq!(staged.status, FileStatus::Added);
        let unstaged = snapshot
            .files
            .iter()
            .find(|file| file.path == "unstaged.txt")
            .expect("unstaged D");
        assert_eq!(unstaged.status, FileStatus::Modified);
        let untracked = snapshot
            .files
            .iter()
            .find(|file| file.path == "untracked.txt")
            .expect("untracked E");
        assert_eq!(untracked.status, FileStatus::Added);
    }

    #[test]
    fn load_review_snapshot_keeps_same_second_same_length_edit() {
        let repo = init_repo();
        let path = repo.path().join("src/lib.rs");
        write(&path, "pub fn answer() -> u32 {\n    41\n}\n");
        git(repo.path(), &["add", "--", "src/lib.rs"]);
        git(repo.path(), &["commit", "-qm", "add answer"]);
        // Same byte length so Git's size check cannot expose the edit.
        // Index mtime <= file mtime, both inside one whole second: the racy
        // case. A copy that stamps "now" moves the index into a later second
        // and Git then trusts the stale cache.
        let instant = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        write(&path, "pub fn answer() -> u32 {\n    42\n}\n");
        let index = repo.path().join(".git/index");
        let before = fs::read(&index).expect("source index");
        for stamped in [&path, &index] {
            let file = fs::OpenOptions::new()
                .write(true)
                .open(stamped)
                .expect("open timestamp target");
            file.set_times(fs::FileTimes::new().set_modified(instant))
                .expect("stamp same-second mtime");
            file.sync_all().expect("sync stamped mtime");
        }
        let index_mtime = fs::metadata(&index)
            .expect("index metadata")
            .modified()
            .expect("index mtime");
        let file_mtime = fs::metadata(&path)
            .expect("file metadata")
            .modified()
            .expect("file mtime");
        assert!(
            index_mtime <= file_mtime,
            "index mtime must not be newer than the worktree file"
        );
        assert_eq!(
            index_mtime
                .duration_since(std::time::UNIX_EPOCH)
                .expect("index unix time")
                .as_secs(),
            file_mtime
                .duration_since(std::time::UNIX_EPOCH)
                .expect("file unix time")
                .as_secs(),
            "index and file must share one whole second"
        );
        let source_diff = git_output(
            repo.path(),
            &["diff", "--name-only", "--no-ext-diff", "--no-textconv", "HEAD"],
        );
        let source_names = String::from_utf8(source_diff).expect("source diff names");
        assert!(
            source_names.lines().any(|line| line == "src/lib.rs"),
            "source git must see the edit before the sandbox copy: {source_names:?}"
        );

        let (snapshot, _paging) =
            load_review_snapshot_capture(repo.path(), ReviewScope::WorkingTree);

        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        let file = snapshot
            .files
            .iter()
            .find(|file| file.path == "src/lib.rs")
            .expect("src/lib.rs");
        assert_eq!(file.status, FileStatus::Modified);
        assert!(file.hunks.iter().flat_map(|hunk| &hunk.lines).any(|line| {
            line.kind == DiffLineKind::Addition && line.text == "    42"
        }));
        assert!(file.hunks.iter().flat_map(|hunk| &hunk.lines).any(|line| {
            line.kind == DiffLineKind::Deletion && line.text == "    41"
        }));
        assert_eq!(fs::read(&index).expect("source index after"), before);
        assert_eq!(
            fs::metadata(&index)
                .expect("index metadata after")
                .modified()
                .expect("index mtime after"),
            index_mtime,
            "snapshot must not rewrite the source index"
        );
    }

    #[test]
    fn load_review_snapshot_zero_arg_excludes_committed_since_head() {
        let (repo, _) = layered_review_repo();
        let snapshot = load_review_snapshot(repo.path());
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        assert_eq!(snapshot.comparison_label(), "HEAD → working tree");
        let paths = snapshot_paths(&snapshot);
        assert!(!paths.contains(&"committed.txt"), "{paths:?}");
        assert!(paths.contains(&"staged.txt"), "{paths:?}");
        assert!(paths.contains(&"unstaged.txt"), "{paths:?}");
        assert!(paths.contains(&"untracked.txt"), "{paths:?}");
    }

    #[test]
    fn load_review_snapshot_two_refs_include_only_committed_range() {
        let (repo, base) = layered_review_repo();
        let snapshot = load_review_snapshot_for(
            repo.path(),
            ReviewScope::Revisions {
                from: base,
                to: "HEAD".to_owned(),
            },
        );
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        let paths = snapshot_paths(&snapshot);
        assert_eq!(paths, vec!["committed.txt"], "{paths:?}");
        assert_eq!(snapshot.files[0].status, FileStatus::Added);
    }

    #[test]
    fn load_review_snapshot_single_ref_head_matches_working_tree_content() {
        let (repo, _) = layered_review_repo();
        let head = load_review_snapshot_for(
            repo.path(),
            ReviewScope::BaseToWorkingTree {
                base: "HEAD".to_owned(),
            },
        );
        let working = load_review_snapshot(repo.path());
        assert!(head.error.is_none(), "{:?}", head.error);
        assert!(working.error.is_none(), "{:?}", working.error);
        assert_eq!(snapshot_paths(&head), snapshot_paths(&working));
        assert_ne!(head.snapshot_id, working.snapshot_id);
        assert_eq!(head.comparison_label(), "HEAD → working tree");
        assert_eq!(working.comparison_label(), "HEAD → working tree");
    }

    #[test]
    fn load_review_snapshot_rejects_invalid_base_and_names_it() {
        let repo = init_repo();
        let invalid = "missing-base-sensitive-marker";
        let snapshot = load_review_snapshot_for(
            repo.path(),
            ReviewScope::BaseToWorkingTree {
                base: invalid.to_owned(),
            },
        );
        let error = snapshot.error.as_deref().expect("invalid base error");
        assert!(
            error.contains("invalid source revision; expected a commit, branch, or tag"),
            "{error}"
        );
        assert!(error.contains(invalid), "{error}");
        assert!(error.contains(&canonical_workspace(repo.path())), "{error}");
        assert_eq!(
            snapshot.comparison_label(),
            format!("{invalid} → working tree")
        );
    }

    #[test]
    fn snapshot_identity_differs_across_working_tree_scopes() {
        let (repo, base) = layered_review_repo();
        let working = load_review_snapshot(repo.path());
        let from_base = load_review_snapshot_for(
            repo.path(),
            ReviewScope::BaseToWorkingTree { base: base.clone() },
        );
        let two_rev = load_review_snapshot_for(
            repo.path(),
            ReviewScope::Revisions {
                from: base,
                to: "HEAD".to_owned(),
            },
        );
        assert!(working.error.is_none(), "{:?}", working.error);
        assert!(from_base.error.is_none(), "{:?}", from_base.error);
        assert!(two_rev.error.is_none(), "{:?}", two_rev.error);
        assert_ne!(working.snapshot_id, from_base.snapshot_id);
        assert_ne!(from_base.snapshot_id, two_rev.snapshot_id);
        assert_ne!(working.snapshot_id, two_rev.snapshot_id);
    }

    #[test]
    fn snapshot_identity_encoding_is_unambiguous_across_segments() {
        let root = Path::new("/repo");
        let identity =
            |catalog: &[u8], index: Option<&[u8]>| {
                review_snapshot_identity(
                    root,
                    b"working-tree",
                    b"diff bytes",
                    false,
                    Some(catalog),
                    index,
                    None,
                )
            };
        assert_ne!(
            identity(b"X", Some(b"Y")),
            identity(b"X\x02Y", None),
            "a tag byte inside catalog content must not impersonate the index segment"
        );
        assert_ne!(
            review_snapshot_identity(root, b"", b"", false, None, None, None),
            review_snapshot_identity(root, b"", b"", false, Some(b""), Some(b""), Some(b"")),
            "absent segments must not fuse with empty bodies"
        );
        assert_ne!(
            review_snapshot_identity(root, b"c", b"d", false, Some(b"c"), None, None),
            review_snapshot_identity(root, b"c", b"d", false, None, Some(b"c"), None),
            "the same bytes in different segments must not collide"
        );
        let nul_heavy = [0u8, 1, 2, 3, 0, b'X'];
        assert_eq!(
            review_snapshot_identity(root, &nul_heavy, b"d", true, Some(&nul_heavy), Some(&nul_heavy), Some(&nul_heavy)),
            review_snapshot_identity(root, &nul_heavy, b"d", true, Some(&nul_heavy), Some(&nul_heavy), Some(&nul_heavy)),
            "identical state must stay idempotent"
        );
        assert_ne!(
            review_snapshot_identity(root, &nul_heavy, b"d", true, Some(&nul_heavy), Some(&nul_heavy), Some(&nul_heavy)),
            review_snapshot_identity(root, &nul_heavy, b"d", false, Some(&nul_heavy), Some(&nul_heavy), Some(&nul_heavy)),
            "truncation must stay part of the identity"
        );
        assert_ne!(
            identity(b"a", Some(b"b")),
            identity(b"ab", None),
            "length frames must pin the catalog/index boundary"
        );
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_identity_hashes_unix_roots_as_raw_bytes() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let first = Path::new("/repo").join(OsStr::from_bytes(b"\xff\xfe"));
        let second = Path::new("/repo").join(OsStr::from_bytes(b"\xff\xfd"));
        let identity = |root: &Path| {
            review_snapshot_identity(root, b"working-tree", b"diff bytes", false, None, None, None)
        };
        assert_eq!(
            first.to_string_lossy(),
            second.to_string_lossy(),
            "precondition: lossy decoding collapses these distinct roots"
        );
        assert_ne!(
            identity(&first),
            identity(&second),
            "distinct non-UTF-8 roots must not share a snapshot identity"
        );
        assert_eq!(
            identity(&first),
            identity(&first),
            "the same root must stay idempotent"
        );
    }

    #[cfg(windows)]
    #[test]
    fn snapshot_identity_hashes_windows_roots_as_framed_wtf16_units() {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;
        let surrogate = PathBuf::from(OsString::from_wide(&[0xD800, 0x0061]));
        let replacement = Path::new("\u{FFFD}a");
        let identity = |root: &Path| {
            review_snapshot_identity(root, b"working-tree", b"diff bytes", false, None, None, None)
        };
        assert_eq!(
            surrogate.to_string_lossy(),
            replacement.to_string_lossy(),
            "precondition: lossy decoding collapses these distinct roots"
        );
        assert_ne!(
            identity(&surrogate),
            identity(replacement),
            "unpaired-surrogate and replacement roots must not share an identity"
        );
        assert_eq!(identity(&surrogate), identity(&surrogate), "idempotent");
    }

    #[test]
    fn file_diff_on_demand_uses_the_same_base() {
        let (repo, base) = layered_review_repo();
        let scope = ReviewScope::BaseToWorkingTree { base: base.clone() };
        let (snapshot, paging) = load_review_snapshot_capture(repo.path(), scope.clone());
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        let committed = snapshot
            .files
            .iter()
            .find(|file| file.path == "committed.txt")
            .expect("committed B");
        let paged = FileDiff::load_snapshot_file(repo.path(), &scope, &paging, committed);
        assert!(paged.error.is_none(), "{:?}", paged.error);
        assert!(
            paged
                .hunks
                .iter()
                .flat_map(|hunk| &hunk.lines)
                .any(|line| line.text.contains("committed-B")),
            "{paged:?}"
        );
        let untracked = snapshot
            .files
            .iter()
            .find(|file| file.path == "untracked.txt")
            .expect("untracked E");
        let paged_untracked = FileDiff::load_snapshot_file(repo.path(), &scope, &paging, untracked);
        assert!(paged_untracked.error.is_none(), "{:?}", paged_untracked.error);
        assert!(
            paged_untracked
                .hunks
                .iter()
                .flat_map(|hunk| &hunk.lines)
                .any(|line| line.text.contains("untracked-E")),
            "{paged_untracked:?}"
        );
    }

    #[test]
    fn load_review_snapshot_compares_two_branches_and_ignores_working_tree() {
        let repo = init_repo();
        git(repo.path(), &["branch", "review-base"]);
        write(repo.path().join("committed.txt").as_path(), "committed\n");
        git(repo.path(), &["add", "committed.txt"]);
        git(repo.path(), &["commit", "-m", "add committed file"]);
        git(repo.path(), &["branch", "review-target"]);
        write(
            repo.path().join("README.md").as_path(),
            "working tree only\n",
        );

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
    fn load_review_snapshot_rejects_invalid_revision_and_names_it() {
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
        assert!(
            error.contains("invalid source revision; expected a commit, branch, or tag"),
            "{error}"
        );
        assert!(error.contains(invalid), "{error}");
        assert!(error.contains(&canonical_workspace(repo.path())), "{error}");
    }

    #[test]
    fn resolve_review_commit_reports_an_unusable_workspace_as_its_cause() {
        // A temp dir outside any repository: git fails for the workspace, not
        // for the revision, so the invalid-revision wording would be a lie.
        let dir = TempDir::new().expect("tempdir");
        let error = resolve_review_commit(dir.path(), "origin/mainnet-beta", "source")
            .expect_err("git cannot resolve a revision outside a repository");
        assert!(!error.contains("invalid source revision"), "{error}");
        assert!(error.contains("not a git repository"), "{error}");
        assert!(error.contains("origin/mainnet-beta"), "{error}");
        assert!(error.contains(&dir.path().display().to_string()), "{error}");
    }

    #[test]
    fn resolve_review_commit_reports_a_broken_config_value_as_its_cause() {
        // A repository-local `core.abbrev` spelling one of git's revision
        // templates: the config failure is a real git diagnostic whose quoted
        // data must never be read as an answer about the revision.
        let repo = init_repo();
        git(
            repo.path(),
            &["config", "core.abbrev", "needed a single revision"],
        );

        let error = resolve_review_commit(repo.path(), "HEAD", "source")
            .expect_err("the broken core.abbrev fails the git call");
        assert!(!error.contains("invalid source revision"), "{error}");
        assert!(error.contains("core.abbrev"), "{error}");
    }

    #[test]
    fn resolve_review_commit_reports_a_missing_upstream_branch_as_unknown() {
        // `x@{upstream}^{commit}` on a branch that does not exist is git's
        // `fatal: no such branch: 'x'`, a genuine missing revision.
        let repo = init_repo();
        let error = resolve_review_commit(repo.path(), "no-such-branch@{upstream}", "source")
            .expect_err("a branch without an upstream cannot resolve");
        assert!(
            error.contains("invalid source revision; expected a commit, branch, or tag"),
            "{error}"
        );
        assert!(error.contains("no-such-branch@{upstream}"), "{error}");
    }

    #[test]
    fn resolve_review_commit_names_a_non_commit_revision_as_such() {
        // `HEAD^{tree}^{commit}` resolves to an object that is not a commit;
        // git's first stderr line is the `error: ... expected commit type`
        // diagnostic, so the revision must not be reported as missing.
        let repo = init_repo();
        let error = resolve_review_commit(repo.path(), "HEAD^{tree}", "source")
            .expect_err("a tree is not a commit");
        assert!(
            error.contains("invalid source revision; expected a commit, branch, or tag"),
            "{error}"
        );
        assert!(error.contains("does not resolve to a commit"), "{error}");
        assert!(!error.contains("was not found"), "{error}");
        assert!(error.contains("HEAD^{tree}"), "{error}");
    }

    #[test]
    fn rev_parse_failure_keeps_runner_failures_as_the_reported_cause() {
        // A timeout, a reader-collection deadline, a spawn failure, and the
        // size limit are four different causes of one failed revision
        // resolution. None of them means the revision does not exist.
        let root = Path::new("/w/psy-node");
        for error in [
            "git timed out after 15s",
            "git stdout remained open after exit",
            "failed to spawn git: No such file or directory (os error 2)",
            "git metadata output exceeded the size limit",
        ] {
            let message = rev_parse_failure(error, "origin/mainnet-beta", "source", root);
            assert!(!message.contains("invalid source revision"), "{message}");
            assert!(message.contains(error), "{message}");
            assert!(message.contains("origin/mainnet-beta"), "{message}");
            assert!(message.contains("/w/psy-node"), "{message}");
        }
    }

    #[test]
    fn rev_parse_failure_classifies_git_revision_shapes() {
        let root = Path::new("/w/psy-node");
        // Complete git templates meaning "no such revision here".
        for error in [
            "fatal: ambiguous argument 'x': unknown revision or path not in the working tree.",
            "fatal: Needed a single revision",
            "fatal: Not a valid object name x",
            "fatal: no such branch: 'x'",
        ] {
            let message = rev_parse_failure(error, "x", "source", root);
            assert!(
                message.contains("invalid source revision; expected a commit, branch, or tag"),
                "{error}: {message}"
            );
            assert!(message.contains("'x'"), "{error}: {message}");
            assert!(message.contains("was not found in /w/psy-node"), "{message}");
        }
        // The revision resolved to an object that is not a commit: an invalid
        // revision, but not a missing one.
        let error = "error: HEAD^{tree}^{commit}: expected commit type, but the object \
                     dereferences to tree type";
        let message = rev_parse_failure(error, "HEAD^{tree}", "source", root);
        assert!(
            message.contains("invalid source revision; expected a commit, branch, or tag"),
            "{message}"
        );
        assert!(message.contains("does not resolve to a commit"), "{message}");
        assert!(!message.contains("was not found"), "{message}");
        // Everything else keeps its cause.
        for error in [
            "fatal: not a git repository (or any of the parent directories): .git",
            "fatal: invalid gitfile format: /w/psy-node/.git",
            "fatal: unable to read config file '.git/config': Permission denied",
            "fatal: Needed a single revision but the warning is long",
        ] {
            let message = rev_parse_failure(error, "x", "source", root);
            assert!(!message.contains("expected a commit, branch, or tag"), "{message}");
            assert!(message.contains(error), "{message}");
        }
    }

    #[test]
    fn rev_parse_failure_never_classifies_data_quoted_by_another_diagnostic() {
        // A repository-local `core.abbrev` can hold any text, including git's
        // own revision wording. Quoted data is not an answer about the revision.
        let root = Path::new("/w/psy-node");
        for error in [
            "fatal: bad numeric config value 'needed a single revision' for 'core.abbrev' in file \
             .git/config: invalid unit",
            "fatal: bad numeric config value 'ambiguous argument ' for 'core.abbrev': invalid unit",
            "fatal: bad numeric config value 'not a valid object name' for 'core.abbrev': invalid unit",
            "fatal: bad numeric config value 'no such branch: 'x'' for 'core.abbrev': invalid unit",
        ] {
            let message = rev_parse_failure(error, "HEAD", "source", root);
            assert!(!message.contains("expected a commit, branch, or tag"), "{message}");
            assert!(message.contains(error), "{message}");
            assert!(message.contains("core.abbrev"), "{message}");
        }
    }

    #[test]
    fn rev_parse_failure_bounds_the_echoed_revision_and_workspace() {
        let revision = "r".repeat(4_096);
        let workspace = "w".repeat(4_096);
        let message = rev_parse_failure(
            "fatal: Needed a single revision",
            &revision,
            "source",
            Path::new(&workspace),
        );
        let over_bound = MAX_ECHOED_CONTEXT_CHARS + 1;
        assert!(!message.contains(&"r".repeat(over_bound)), "{message}");
        assert!(!message.contains(&"w".repeat(over_bound)), "{message}");
        assert!(message.contains("..."), "{message}");
        assert!(
            message.contains("invalid source revision; expected a commit, branch, or tag"),
            "{message}"
        );
        let ceiling = "invalid source revision; expected a commit, branch, or tag: '' was not \
                       found in "
            .len()
            + 2 * (MAX_ECHOED_CONTEXT_CHARS + 3);
        assert!(message.len() <= ceiling, "{} > {ceiling}", message.len());
    }

    #[test]
    fn rev_parse_failure_points_an_unfinished_call_at_a_possible_partial_clone_fetch() {
        // Both budgets a partial clone can exhaust — the runner timeout and the
        // reader-collection deadline — carry the same actionable hint.
        let root = Path::new("/w/psy-node");
        for error in ["git timed out after 15s", "git stdout remained open after exit"] {
            let message = rev_parse_failure(error, "origin/mainnet-beta", "source", root);
            assert!(message.contains(error), "{message}");
            assert!(message.contains("partial (blobless) clone"), "{message}");
        }
    }

    #[test]
    fn load_review_snapshot_resolves_option_looking_revision_as_operand() {
        // A ref whose name looks like a rev-parse option must be treated as an
        // operand by the fixed `--end-of-options` argv in `resolve_review_commit`,
        // not parsed as a flag. Created via plumbing so the test does not rely on
        // porcelain option escaping.
        let repo = init_repo();
        let base =
            String::from_utf8(git_output(repo.path(), &["rev-parse", "HEAD"])).expect("base hash");
        let base = strip_output_line_ending(&base).to_owned();
        write(repo.path().join("target.txt").as_path(), "target\n");
        git(repo.path(), &["add", "target.txt"]);
        git(repo.path(), &["commit", "-m", "target commit"]);
        let head =
            String::from_utf8(git_output(repo.path(), &["rev-parse", "HEAD"])).expect("head hash");
        let head = strip_output_line_ending(&head);
        let option_ref = "--output=evil";
        git(
            repo.path(),
            &["update-ref", &format!("refs/tags/{option_ref}"), head],
        );

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
        let base =
            String::from_utf8(git_output(repo.path(), &["rev-parse", "HEAD"])).expect("base hash");
        let base = strip_output_line_ending(&base).to_owned();
        write(repo.path().join("victim.txt").as_path(), "changed\n");
        git(repo.path(), &["add", "victim.txt"]);
        git(repo.path(), &["commit", "-m", "change filtered file"]);
        let head =
            String::from_utf8(git_output(repo.path(), &["rev-parse", "HEAD"])).expect("head hash");
        let head = strip_output_line_ending(&head).to_owned();

        write(
            repo.path().join(".gitattributes").as_path(),
            "*.txt filter=evil\n",
        );
        let driver = repo.path().join("evil-driver");
        let sentinel = repo.path().join("filter-fired");
        write(&driver, ": > filter-fired\ncat\n");
        let driver = driver.to_str().expect("utf8 driver");
        let driver_command = format!("sh {}", shell_quote(driver));
        git(
            repo.path(),
            &["config", "filter.evil.clean", &driver_command],
        );
        git(
            repo.path(),
            &["config", "filter.evil.process", &driver_command],
        );
        git(repo.path(), &["config", "diff.external", &driver_command]);
        git(repo.path(), &["config", "core.fsmonitor", &driver_command]);

        let snapshot = load_review_snapshot_for(
            repo.path(),
            ReviewScope::Revisions {
                from: base,
                to: head,
            },
        );

        assert!(
            !sentinel.exists(),
            "configured executable was invoked during revision diff"
        );
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        assert_eq!(snapshot.files.len(), 1, "{:?}", snapshot.files);
        assert_eq!(snapshot.files[0].path, "victim.txt");
        assert_eq!(snapshot.files[0].status, FileStatus::Modified);
    }

    fn rev_parse(cwd: &Path) -> String {
        strip_output_line_ending(
            &String::from_utf8(git_output(cwd, &["rev-parse", "HEAD"])).expect("rev-parse"),
        )
        .to_owned()
    }

    fn add_named_submodule(parent: &Path, name: &str) {
        let sub_dir = TempDir::new().expect("submodule tempdir");
        git(sub_dir.path(), &["init"]);
        write(sub_dir.path().join("lib.txt").as_path(), "base\n");
        git(sub_dir.path(), &["add", "lib.txt"]);
        git(sub_dir.path(), &["commit", "-m", "submodule initial"]);
        let sub_path = sub_dir.path().canonicalize().expect("canonical submodule");
        git(
            parent,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                sub_path.to_str().expect("utf8 submodule path"),
                name,
            ],
        );
        git(parent, &["commit", "-qm", &format!("add {name}")]);
    }

    fn submodule_repo_with_revision_inner_lib(lib_body: &str) -> (TempDir, String, String) {
        let parent = init_repo();
        add_named_submodule(parent.path(), "ext");
        let from = rev_parse(parent.path());
        write(parent.path().join("ext/lib.txt").as_path(), lib_body);
        git(&parent.path().join("ext"), &["add", "lib.txt"]);
        git(&parent.path().join("ext"), &["commit", "-qm", "submodule bump"]);
        git(parent.path(), &["add", "ext"]);
        git(parent.path(), &["commit", "-qm", "bump gitlink"]);
        let to = rev_parse(parent.path());
        (parent, from, to)
    }

    fn submodule_repo_with_revision_inner_diff() -> (TempDir, String, String) {
        submodule_repo_with_revision_inner_lib("base\nchanged\n")
    }

    fn synthetic_revision_gitlink_output(parent: &Path) -> (String, GitOutput) {
        let old_sha = strip_output_line_ending(
            &String::from_utf8(git_output(
                &parent.join("ext"),
                &["rev-parse", "HEAD^"],
            ))
            .expect("old sha"),
        )
        .to_owned();
        let new_sha = rev_parse(&parent.join("ext"));
        let gitlink_block = format!(
            "diff --git a/ext b/ext\nindex {old_sha}..{new_sha} 160000\n--- a/ext\n+++ b/ext\n@@ -1 +1 @@\n-Subproject commit {old_sha}\n+Subproject commit {new_sha}\n"
        );
        let output = GitOutput {
            stdout: gitlink_block.clone().into_bytes(),
            truncated: false,
            error: None,
        };
        (gitlink_block, output)
    }

    #[test]
    fn load_revision_snapshot_expands_initialized_submodule_inner_diff() {
        let (parent, from, to) = submodule_repo_with_revision_inner_diff();
        let (snapshot, paging) = load_review_snapshot_capture(
            parent.path(),
            ReviewScope::Revisions { from, to },
        );
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        let inner = snapshot
            .files
            .iter()
            .find(|file| file.path == "ext/lib.txt")
            .unwrap_or_else(|| panic!("submodule inner file absent: {:?}", snapshot.files));
        assert_eq!(inner.status, FileStatus::Modified);
        assert!(
            inner.hunks.iter().any(|hunk| hunk
                .lines
                .iter()
                .any(|line| line.kind == DiffLineKind::Addition && line.text == "changed")),
            "the inner diff must carry the submodule's own hunks: {:?}",
            inner.hunks
        );
        assert!(
            snapshot.files.iter().all(|file| file.path != "ext"),
            "gitlink entry must be replaced by its expanded files: {:?}",
            snapshot
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>()
        );
        let blobs =
            FileContextBlobs::load(parent.path(), &paging, inner).expect("submodule blobs");
        assert_eq!(blobs.old.lines, ["base"]);
        assert_eq!(blobs.new.lines, ["base", "changed"]);
        assert!(paging.submodule_files.contains_key("ext/lib.txt"));
        let loaded = FileDiff::load_snapshot_file(
            parent.path(),
            &snapshot.scope,
            &paging,
            inner,
        );
        assert!(loaded.error.is_none(), "{:?}", loaded.error);
        let loaded_lines: Vec<&str> = loaded
            .hunks
            .iter()
            .flat_map(|hunk| hunk.lines.iter().map(|line| line.text.as_str()))
            .collect();
        assert!(loaded_lines.contains(&"changed"), "{loaded_lines:?}");
        assert!(loaded_lines.contains(&"base"), "{loaded_lines:?}");
    }

    #[test]
    fn ancestor_repo_prefixes_walk_each_parent_segment() {
        assert_eq!(ancestor_repo_prefixes("ext/lib.txt"), ["ext"]);
        assert_eq!(
            ancestor_repo_prefixes("ext/nested/lib.txt"),
            ["ext/nested", "ext"]
        );
        assert!(ancestor_repo_prefixes("lib.txt").is_empty());
        assert!(ancestor_repo_prefixes("").is_empty());
    }

    #[test]
    fn file_diff_load_without_paging_fails_closed_for_expanded_revision_inner_file() {
        let (parent, from, to) = submodule_repo_with_revision_inner_diff();
        let snapshot = load_review_snapshot_for(
            parent.path(),
            ReviewScope::Revisions { from, to },
        );
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        let inner = snapshot
            .files
            .iter()
            .find(|file| file.path == "ext/lib.txt")
            .unwrap_or_else(|| panic!("submodule inner file absent: {:?}", snapshot.files));
        let loaded = FileDiff::load(parent.path(), &snapshot.scope, inner);
        assert!(
            loaded.error.as_deref() == Some("review paging state is unavailable for this snapshot"),
            "FileDiff::load without snapshot paging must not page ext/lib.txt from the parent repo: {:?}",
            loaded
        );
    }

    #[test]
    fn file_diff_load_without_paging_still_pages_parent_nested_file() {
        let repo = init_repo();
        std::fs::create_dir_all(repo.path().join("src")).expect("src dir");
        write(repo.path().join("src/lib.txt").as_path(), "base\n");
        git(repo.path(), &["add", "src/lib.txt"]);
        git(repo.path(), &["commit", "-qm", "add nested"]);
        let from = rev_parse(repo.path());
        write(repo.path().join("src/lib.txt").as_path(), "base\nchanged\n");
        git(repo.path(), &["add", "src/lib.txt"]);
        git(repo.path(), &["commit", "-qm", "edit nested"]);
        let to = rev_parse(repo.path());
        let snapshot = load_review_snapshot_for(
            repo.path(),
            ReviewScope::Revisions { from, to },
        );
        let nested = snapshot
            .files
            .iter()
            .find(|file| file.path == "src/lib.txt")
            .unwrap_or_else(|| panic!("parent nested file absent: {:?}", snapshot.files));
        let loaded = FileDiff::load(repo.path(), &snapshot.scope, nested);
        assert!(loaded.error.is_none(), "{:?}", loaded.error);
        let loaded_lines: Vec<&str> = loaded
            .hunks
            .iter()
            .flat_map(|hunk| hunk.lines.iter().map(|line| line.text.as_str()))
            .collect();
        assert!(loaded_lines.contains(&"changed"), "{loaded_lines:?}");
    }

    #[test]
    fn load_revision_snapshot_expands_submodule_rename_and_pages_from_child() {
        let parent = init_repo();
        add_named_submodule(parent.path(), "ext");
        let from = rev_parse(parent.path());
        git(
            &parent.path().join("ext"),
            &["mv", "lib.txt", "renamed.txt"],
        );
        git(&parent.path().join("ext"), &["commit", "-qm", "rename lib"]);
        git(parent.path(), &["add", "ext"]);
        git(parent.path(), &["commit", "-qm", "bump gitlink"]);
        let to = rev_parse(parent.path());
        let (snapshot, paging) = load_review_snapshot_capture(
            parent.path(),
            ReviewScope::Revisions { from, to },
        );
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        let file = snapshot
            .files
            .iter()
            .find(|file| file.path == "ext/renamed.txt")
            .unwrap_or_else(|| panic!("renamed child missing: {:?}", snapshot.files));
        assert_eq!(file.status, FileStatus::Renamed);
        assert_eq!(file.previous_path.as_deref(), Some("ext/lib.txt"));
        assert!(
            snapshot.files.iter().all(|file| file.path != "ext"),
            "gitlink entry must be replaced: {:?}",
            snapshot
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>()
        );
        assert!(paging.submodule_files.contains_key("ext/renamed.txt"));
        let loaded =
            FileDiff::load_snapshot_file(parent.path(), &snapshot.scope, &paging, file);
        assert!(loaded.error.is_none(), "{:?}", loaded.error);
        assert_eq!(loaded.previous_path.as_deref(), Some("ext/lib.txt"));
        let blobs =
            FileContextBlobs::load(parent.path(), &paging, file).expect("renamed child blobs");
        assert_eq!(blobs.old.lines, ["base"]);
        assert_eq!(blobs.new.lines, ["base"]);
        let tree = FileTree::from_snapshot(&snapshot);
        assert!(
            tree.nodes.iter().any(|node| {
                node.path == "ext" && matches!(node.kind, TreeNodeKind::Directory)
            }),
            "{:?}",
            tree.nodes
        );
        assert!(
            tree.nodes.iter().any(|node| {
                node.path == "ext/renamed.txt" && matches!(node.kind, TreeNodeKind::File { .. })
            }),
            "{:?}",
            tree.nodes
        );
        assert!(
            tree.nodes.iter().all(|node| {
                !(node.path == "ext" && matches!(node.kind, TreeNodeKind::File { .. }))
            }),
            "{:?}",
            tree.nodes
        );
    }

    #[test]
    fn load_revision_snapshot_keeps_deleted_gitlink() {
        let parent = init_repo();
        add_named_submodule(parent.path(), "ext");
        let from = rev_parse(parent.path());
        git(parent.path(), &["rm", "-q", "ext"]);
        git(parent.path(), &["commit", "-qm", "remove submodule"]);
        let to = rev_parse(parent.path());
        let snapshot = load_review_snapshot_for(
            parent.path(),
            ReviewScope::Revisions { from, to },
        );
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        let ext_rows: Vec<&DiffFile> = snapshot
            .files
            .iter()
            .filter(|file| file.path == "ext")
            .collect();
        assert_eq!(ext_rows.len(), 1, "{:?}", snapshot.files);
        assert_eq!(ext_rows[0].status, FileStatus::Deleted);
        assert!(
            snapshot.files.iter().all(|file| file.path != "ext/lib.txt"),
            "{:?}",
            snapshot.files
        );
    }

    #[test]
    fn load_revision_snapshot_replaces_blob_to_gitlink_typechange_once() {
        let parent = init_repo();
        write(parent.path().join("ext").as_path(), "blob\n");
        git(parent.path(), &["add", "ext"]);
        git(parent.path(), &["commit", "-qm", "blob at ext"]);
        let from = rev_parse(parent.path());
        git(parent.path(), &["rm", "-q", "ext"]);
        add_named_submodule(parent.path(), "ext");
        write(parent.path().join("ext/lib.txt").as_path(), "base\nchanged\n");
        git(&parent.path().join("ext"), &["add", "lib.txt"]);
        git(&parent.path().join("ext"), &["commit", "-qm", "inner change"]);
        git(parent.path(), &["add", "ext"]);
        git(parent.path(), &["commit", "-qm", "gitlink at ext"]);
        let to = rev_parse(parent.path());
        let snapshot = load_review_snapshot_for(
            parent.path(),
            ReviewScope::Revisions { from, to },
        );
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        let ext_rows: Vec<&DiffFile> = snapshot
            .files
            .iter()
            .filter(|file| file.path == "ext")
            .collect();
        assert!(
            ext_rows.len() <= 1,
            "blob+gitlink pair must not both appear: {:?}",
            snapshot.files
        );
        let inner = snapshot
            .files
            .iter()
            .any(|file| file.path == "ext/lib.txt");
        if inner {
            assert!(
                ext_rows.is_empty(),
                "expanded inner files replace the gitlink row: {:?}",
                snapshot.files
            );
        } else {
            assert_eq!(ext_rows.len(), 1, "{:?}", snapshot.files);
        }
    }

    #[test]
    fn file_tree_from_two_revision_snapshot_shows_expanded_submodule_files() {
        let (parent, from, to) = submodule_repo_with_revision_inner_diff();
        let snapshot = load_review_snapshot_for(
            parent.path(),
            ReviewScope::Revisions { from, to },
        );
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        let tree = FileTree::from_snapshot(&snapshot);
        let visible: Vec<&str> = tree
            .visible_rows()
            .iter()
            .map(|row| tree.nodes[row.node_index].path.as_str())
            .collect();
        assert!(visible.contains(&"ext"), "{visible:?}");
        assert!(visible.contains(&"ext/lib.txt"), "{visible:?}");
        assert!(
            tree.nodes.iter().any(|node| {
                node.path == "ext" && matches!(node.kind, TreeNodeKind::Directory)
            }),
            "{:?}",
            tree.nodes
        );
        assert!(
            tree.nodes.iter().any(|node| {
                node.path == "ext/lib.txt" && matches!(node.kind, TreeNodeKind::File { .. })
            }),
            "{:?}",
            tree.nodes
        );
        assert!(
            tree.nodes.iter().all(|node| {
                !(node.path == "ext" && matches!(node.kind, TreeNodeKind::File { .. }))
            }),
            "gitlink file node must be gone: {:?}",
            tree.nodes
        );
        let dir = tree
            .nodes
            .iter()
            .find(|node| node.path == "ext" && matches!(node.kind, TreeNodeKind::Directory))
            .expect("directory ext");
        let inner = tree
            .nodes
            .iter()
            .find(|node| {
                node.path == "ext/lib.txt" && matches!(node.kind, TreeNodeKind::File { .. })
            })
            .expect("file ext/lib.txt");
        assert!(inner.insertions > 0, "{inner:?}");
        assert_eq!(dir.insertions, inner.insertions);
        assert_eq!(dir.deletions, inner.deletions);
    }

    #[test]
    fn file_tree_from_two_revision_snapshot_keeps_gitlink_when_modules_dir_removed() {
        let (parent, from, to) = submodule_repo_with_revision_inner_diff();
        git(parent.path(), &["submodule", "deinit", "-f", "ext"]);
        fs::remove_dir_all(parent.path().join(".git/modules/ext")).expect("remove modules dir");
        let snapshot = load_review_snapshot_for(
            parent.path(),
            ReviewScope::Revisions { from, to },
        );
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        let tree = FileTree::from_snapshot(&snapshot);
        let visible: Vec<&str> = tree
            .visible_rows()
            .iter()
            .map(|row| tree.nodes[row.node_index].path.as_str())
            .collect();
        assert!(visible.contains(&"ext"), "{visible:?}");
        assert!(!visible.contains(&"ext/lib.txt"), "{visible:?}");
        assert!(
            tree.nodes.iter().any(|node| {
                node.path == "ext" && matches!(node.kind, TreeNodeKind::File { .. })
            }),
            "{:?}",
            tree.nodes
        );
        assert!(
            tree.nodes.iter().all(|node| node.path != "ext/lib.txt"),
            "{:?}",
            tree.nodes
        );
        assert!(
            tree.nodes.iter().all(|node| {
                !(node.path == "ext" && matches!(node.kind, TreeNodeKind::Directory))
            }),
            "{:?}",
            tree.nodes
        );
    }

    #[test]
    fn load_revision_snapshot_keeps_clean_submodule_sha_bump() {
        let parent = init_repo();
        add_named_submodule(parent.path(), "ext");
        let from = rev_parse(parent.path());
        git(
            &parent.path().join("ext"),
            &["commit", "--allow-empty", "-qm", "empty bump"],
        );
        git(parent.path(), &["add", "ext"]);
        git(parent.path(), &["commit", "-qm", "bump gitlink"]);
        let to = rev_parse(parent.path());
        let (snapshot, paging) = load_review_snapshot_capture(
            parent.path(),
            ReviewScope::Revisions { from, to },
        );
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        let paths: Vec<&str> = snapshot.files.iter().map(|file| file.path.as_str()).collect();
        assert!(paths.contains(&"ext"), "{paths:?}");
        assert!(!paths.contains(&"ext/lib.txt"), "{paths:?}");
        assert!(paging.submodule_files.is_empty());
        assert!(!snapshot.truncated);
        let gitlink = snapshot
            .files
            .iter()
            .find(|file| file.path == "ext")
            .expect("gitlink row");
        let loaded = FileDiff::load_snapshot_file(parent.path(), &snapshot.scope, &paging, gitlink);
        assert!(loaded.error.is_none(), "{:?}", loaded.error);
        assert_eq!(loaded.hunks, gitlink.hunks);
    }

    #[test]
    fn expand_revision_gitlink_diffs_keeps_gitlink_when_inner_capture_exceeds_cap() {
        let mut large = String::from("base\n");
        for index in 0..80 {
            large.push_str(&format!("changed-line-{index:04}\n"));
        }
        let (parent, _from, _to) = submodule_repo_with_revision_inner_lib(&large);
        let (gitlink_block, output) = synthetic_revision_gitlink_output(parent.path());
        let (kept, paths, _, _, _) = expand_changed_gitlink_diffs(
            &output,
            &gitlink_paths_for(&gitlink_block.split('\n').map(str::to_owned).collect::<Vec<_>>()),
            parent.path(),
            gitlink_block.len(),
            GitlinkInner::Revisions,
            None,
            &mut 0,
        )
        .expect("kept");
        assert!(paths.is_empty(), "{paths:?}");
        assert_eq!(kept.stdout, gitlink_block.into_bytes());
        assert!(kept.truncated);
        assert!(kept.error.is_none());
    }

    #[test]
    fn expand_revision_gitlink_diffs_splices_when_inner_diff_is_smaller_than_gitlink() {
        let (parent, _from, _to) = submodule_repo_with_revision_inner_diff();
        let (gitlink_block, output) = synthetic_revision_gitlink_output(parent.path());
        let (expanded, paths, _, _, _) = expand_changed_gitlink_diffs(
            &output,
            &gitlink_paths_for(&gitlink_block.split('\n').map(str::to_owned).collect::<Vec<_>>()),
            parent.path(),
            gitlink_block.len(),
            GitlinkInner::Revisions,
            None,
            &mut 0,
        )
        .expect("spliced");
        assert!(paths.contains("ext/lib.txt"), "{paths:?}");
        assert!(!paths.contains("ext"), "{paths:?}");
        assert!(!expanded.truncated);
        assert!(expanded.error.is_none());
        let expanded_body = String::from_utf8(expanded.stdout).expect("utf8 splice");
        assert!(
            expanded_body.contains("diff --git a/ext/lib.txt b/ext/lib.txt"),
            "{expanded_body}"
        );
        assert!(!expanded_body.contains("diff --git a/ext b/ext"), "{expanded_body}");
    }

    #[test]
    fn load_revision_snapshot_keeps_clean_submodule_bump_when_another_expands() {
        let parent = init_repo();
        add_named_submodule(parent.path(), "ext1");
        add_named_submodule(parent.path(), "ext2");
        let from = rev_parse(parent.path());
        git(
            &parent.path().join("ext1"),
            &["commit", "--allow-empty", "-qm", "empty bump"],
        );
        git(parent.path(), &["add", "ext1"]);
        write(
            parent.path().join("ext2/lib.txt").as_path(),
            "base\nchanged\n",
        );
        git(&parent.path().join("ext2"), &["add", "lib.txt"]);
        git(&parent.path().join("ext2"), &["commit", "-qm", "file bump"]);
        git(parent.path(), &["add", "ext2"]);
        git(parent.path(), &["commit", "-qm", "bump both gitlinks"]);
        let to = rev_parse(parent.path());
        let (snapshot, paging) = load_review_snapshot_capture(
            parent.path(),
            ReviewScope::Revisions { from, to },
        );
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        let paths: Vec<&str> = snapshot.files.iter().map(|file| file.path.as_str()).collect();
        let ext1 = snapshot
            .files
            .iter()
            .find(|file| file.path == "ext1")
            .expect("clean submodule bump must stay a snapshot entry");
        assert_eq!(ext1.status, FileStatus::Modified);
        assert!(!paths.contains(&"ext1/lib.txt"), "{paths:?}");
        assert!(paths.contains(&"ext2/lib.txt"), "{paths:?}");
        assert!(!paths.contains(&"ext2"), "expanded submodule path is replaced: {paths:?}");
        assert!(paging.submodule_files.contains_key("ext2/lib.txt"));
        assert!(!paging.submodule_files.contains_key("ext1"));
        let blobs = FileContextBlobs::load(parent.path(), &paging, ext1)
            .expect("gitlink context must not git-show");
        assert!(blobs.old.lines.is_empty() && blobs.new.lines.is_empty());
    }

    #[test]
    fn expand_revision_gitlink_diffs_fails_closed_on_path_escape() {
        let parent = init_repo();
        let escape_block = "diff --git a/../escape b/../escape\nindex 0123456789abcdef0123456789abcdef01234567..0123456789abcdef0123456789abcdef01234567 160000\n--- a/../escape\n+++ b/../escape\n@@ -1 +1 @@\n-Subproject commit 0123456789abcdef0123456789abcdef01234567\n+Subproject commit 0123456789abcdef0123456789abcdef01234568\n".to_owned();
        let output = GitOutput {
            stdout: escape_block.clone().into_bytes(),
            truncated: false,
            error: None,
        };
        let (kept, paths, _, _, _) = expand_changed_gitlink_diffs(
            &output,
            &gitlink_paths_for(&escape_block.split('\n').map(str::to_owned).collect::<Vec<_>>()),
            parent.path(),
            MAX_DIFF_BYTES,
            GitlinkInner::Revisions,
            None,
            &mut 0,
        )
        .expect("kept");
        assert!(paths.is_empty(), "{paths:?}");
        assert_eq!(kept.stdout, escape_block.into_bytes());
        assert!(!kept.truncated);
        assert!(kept.error.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn load_revision_snapshot_never_executes_submodule_configured_filters() {
        let (parent, from, to) = submodule_repo_with_revision_inner_diff();
        let ext = parent.path().join("ext");
        write(ext.join(".gitattributes").as_path(), "*.txt filter=pwn\n");
        git(&ext, &["config", "filter.pwn.clean", "touch pwned.clean"]);
        git(&ext, &["config", "filter.pwn.process", "touch pwned.process"]);
        git(&ext, &["config", "diff.external", "touch pwned.diff"]);
        git(&ext, &["config", "core.fsmonitor", "touch fsmonitor-ran"]);
        let snapshot = load_review_snapshot_for(
            parent.path(),
            ReviewScope::Revisions { from, to },
        );
        assert!(
            !ext.join("pwned.clean").exists(),
            "clean filter must not run"
        );
        assert!(
            !ext.join("pwned.process").exists(),
            "process filter must not run"
        );
        assert!(
            !ext.join("pwned.diff").exists(),
            "external diff must not run"
        );
        assert!(
            !ext.join("fsmonitor-ran").exists(),
            "fsmonitor command must not run"
        );
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        assert!(
            snapshot.files.iter().any(|file| file.path == "ext/lib.txt"),
            "{:?}",
            snapshot.files
        );
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
            snapshot
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>()
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
        write(
            repo.path().join(".gitattributes").as_path(),
            "*.txt filter=evil\n",
        );
        let driver = repo.path().join("evil-driver");
        let sentinel = repo.path().join("filter-fired");
        write(&driver, ": > filter-fired\ncat\n");
        let driver = driver.to_str().expect("utf8 driver");
        let driver_command = format!("sh {}", shell_quote(driver));
        git(
            repo.path(),
            &["config", "filter.evil.clean", &driver_command],
        );
        git(
            repo.path(),
            &["config", "filter.evil.process", &driver_command],
        );
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
        let result = run_git_bounded(directory.path(), &["pwn"], MAX_GIT_METADATA_BYTES, None)
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
        // Stage the rename (git mv) so both sides are tracked changed paths;
        // the backslash/tab bytes must round-trip through the path-limited
        // staging and rename discovery.
        git(repo.path(), &["mv", old, new]);
        let snapshot = load_review_snapshot(repo.path());
        let display_old = encode_path_bytes(old.as_bytes());
        let display_new = encode_path_bytes(new.as_bytes());
        let renamed = snapshot
            .files
            .iter()
            .find(|file| file.path == display_new)
            .expect("renamed path");
        assert_eq!(renamed.previous_path.as_deref(), Some(display_old.as_str()));
        assert_eq!(renamed.status, FileStatus::Renamed);
    }

    #[test]
    fn load_review_snapshot_includes_untracked_file_as_added() {
        let repo = init_repo();
        write(repo.path().join("notes.md").as_path(), "base\n");
        write(repo.path().join("old.txt").as_path(), "payload\n");
        git(repo.path(), &["add", "notes.md", "old.txt"]);
        git(repo.path(), &["commit", "-m", "add tracked files"]);
        // An unstaged edit to a tracked file, a staged rename of a tracked
        // file (`git mv`, whose destination is a tracked changed path), and
        // an unrelated untracked file captured as an Added entry.
        write(repo.path().join("notes.md").as_path(), "base\nedited\n");
        git(repo.path(), &["mv", "old.txt", "new.txt"]);
        write(repo.path().join("stray.txt").as_path(), "brand new\nbody\n");
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
        let stray = snapshot
            .files
            .iter()
            .find(|file| file.path == "stray.txt")
            .expect("untracked file present as Added");
        assert_eq!(stray.status, FileStatus::Added);
        assert!(stray.previous_path.is_none());
        assert!(!stray.hunks.is_empty(), "untracked patch carries hunks");
        assert!(
            stray
                .hunks
                .iter()
                .flat_map(|hunk| &hunk.lines)
                .any(|line| line.text == "brand new"),
            "untracked body rendered: {:?}",
            stray
        );
        // Never staged: the real index must be byte-identical.
        assert_eq!(fs::read(&index).expect("index after"), index_before);
    }

    #[test]
    fn load_review_snapshot_captures_untracked_only_worktree() {
        let repo = init_repo();
        write(repo.path().join("brand-new.txt").as_path(), "hello\n");
        write(repo.path().join("also-new.rs").as_path(), "fn x() {}\n");
        let index = repo.path().join(".git/index");
        let index_before = fs::read(&index).expect("index before");

        let snapshot = load_review_snapshot(repo.path());

        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        let paths = snapshot
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>();
        assert!(paths.contains(&"brand-new.txt"), "{paths:?}");
        assert!(paths.contains(&"also-new.rs"), "{paths:?}");
        for file in &snapshot.files {
            assert_eq!(
                file.status,
                FileStatus::Added,
                "untracked entry must read as Added: {}",
                file.path
            );
        assert!(
            file.hunks
                .iter()
                .flat_map(|hunk| &hunk.lines)
                .any(|line| line.kind == DiffLineKind::Addition),
            "untracked body must render as additions: {}",
            file.path
        );
        }
        assert_eq!(fs::read(&index).expect("index after"), index_before);
    }

    #[test]
    fn load_review_snapshot_captures_untracked_alongside_tracked_diff() {
        let repo = init_repo();
        write(repo.path().join("README.md").as_path(), "base\nedited\n");
        write(repo.path().join("stray.txt").as_path(), "untracked\n");
        let index = repo.path().join(".git/index");
        let index_before = fs::read(&index).expect("index before");

        let snapshot = load_review_snapshot(repo.path());

        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        let readme = snapshot
            .files
            .iter()
            .find(|file| file.path == "README.md")
            .expect("tracked change present");
        assert_eq!(readme.status, FileStatus::Modified);
        let stray = snapshot
            .files
            .iter()
            .find(|file| file.path == "stray.txt")
            .expect("untracked sibling present");
        assert_eq!(stray.status, FileStatus::Added);
        assert_eq!(
            snapshot
                .files
                .iter()
                .filter(|file| file.path == "stray.txt")
                .count(),
            1,
            "tracked and untracked entry lists are disjoint"
        );
        assert_eq!(fs::read(&index).expect("index after"), index_before);
    }

    #[test]
    fn load_review_snapshot_clean_tree_is_empty() {
        let repo = init_repo();
        let snapshot = load_review_snapshot(repo.path());
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        assert!(snapshot.files.is_empty(), "{:?}", snapshot.files);
        assert!(!snapshot.truncated, "{:?}", snapshot);
    }

    #[test]
    fn load_review_snapshot_excludes_ignored_untracked_files() {
        // gitignore'd build output stays excluded from both the tracked diff
        // and `ls-files --others --exclude-standard`.
        let repo = init_repo();
        write(repo.path().join(".gitignore").as_path(), "build/\n");
        git(repo.path(), &["add", ".gitignore"]);
        git(repo.path(), &["commit", "-m", "ignore build"]);
        write(repo.path().join("build/secret.bin").as_path(), "never\n");
        let index = repo.path().join(".git/index");
        let index_before = fs::read(&index).expect("index before");

        let snapshot = load_review_snapshot(repo.path());

        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        assert!(
            snapshot.files.is_empty(),
            "ignored file must stay excluded: {:?}",
            snapshot.files
        );
        assert_eq!(fs::read(&index).expect("index after"), index_before);
    }


    #[test]
    fn load_review_snapshot_keeps_staged_and_unstaged_tracked_deletions() {
        let repo = init_repo();
        write(repo.path().join("unstaged-gone.txt").as_path(), "base\n");
        write(repo.path().join("staged-gone.txt").as_path(), "base\n");
        git(
            repo.path(),
            &["add", "unstaged-gone.txt", "staged-gone.txt"],
        );
        git(repo.path(), &["commit", "-m", "add deletable files"]);
        // Unstaged deletion (tracked path absent from the worktree only: the
        // copied real index entry is removed by the scoped `git add -u`) and
        // staged deletion (`git rm`: absent from BOTH the real index and the
        // worktree, so the loader never resets or re-adds it — the copied
        // real index already carries the deletion).
        fs::remove_file(repo.path().join("unstaged-gone.txt")).expect("remove unstaged");
        git(repo.path(), &["rm", "-q", "staged-gone.txt"]);
        let index = repo.path().join(".git/index");
        let index_before = fs::read(&index).expect("index before");

        let snapshot = load_review_snapshot(repo.path());

        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        let paths = snapshot
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>();
        assert!(paths.contains(&"unstaged-gone.txt"), "{paths:?}");
        assert!(paths.contains(&"staged-gone.txt"), "{paths:?}");
        for file in &snapshot.files {
            assert_eq!(
                file.status,
                FileStatus::Deleted,
                "tracked deletion must read as Deleted: {}",
                file.path
            );
        }
        assert_eq!(fs::read(&index).expect("index after"), index_before);
    }

    #[test]
    fn load_review_snapshot_keeps_git_rm_cached_deletions_with_worktree_replacement() {
        // `git rm --cached` stages a deletion while the worktree file stays
        // untracked. The review must keep showing the tracked deletion —
        // never an empty snapshot, never a Modified entry — whether the
        // replacement file is unchanged or edited, because re-staging the
        // replacement would silently erase the deletion.
        let repo = init_repo();
        write(repo.path().join("unchanged.txt").as_path(), "base\n");
        write(repo.path().join("edited.txt").as_path(), "base\n");
        git(repo.path(), &["add", "unchanged.txt", "edited.txt"]);
        git(repo.path(), &["commit", "-m", "add deletable files"]);
        git(repo.path(), &["rm", "-q", "--cached", "unchanged.txt"]);
        git(repo.path(), &["rm", "-q", "--cached", "edited.txt"]);
        write(
            repo.path().join("edited.txt").as_path(),
            "base\nedited replacement\n",
        );
        let index = repo.path().join(".git/index");
        let index_before = fs::read(&index).expect("index before");

        let snapshot = load_review_snapshot(repo.path());

        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        assert!(
            !snapshot.files.is_empty(),
            "git rm --cached deletions must not vanish: {:?}",
            snapshot.files
        );
        let paths = snapshot
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>();
        assert!(paths.contains(&"unchanged.txt"), "{paths:?}");
        assert!(paths.contains(&"edited.txt"), "{paths:?}");
        for file in &snapshot.files {
            assert_eq!(
                file.status,
                FileStatus::Deleted,
                "git rm --cached must read as Deleted: {}",
                file.path
            );
        }
        assert_eq!(fs::read(&index).expect("index after"), index_before);
    }

    #[test]
    fn load_review_snapshot_reports_file_replaced_by_directory_as_deletion() {
        let repo = init_repo();
        write(repo.path().join("swap.txt").as_path(), "tracked file\n");
        git(repo.path(), &["add", "swap.txt"]);
        git(repo.path(), &["commit", "-m", "add swap file"]);
        // The tracked file path becomes an untracked directory full of files.
        // The review keeps the tracked file as deleted and captures the
        // replacement directory's untracked regular files as Added entries —
        // without ever staging them.
        fs::remove_file(repo.path().join("swap.txt")).expect("remove tracked file");
        write(
            repo.path().join("swap.txt/bundle.bin").as_path(),
            "0000000000\n",
        );
        write(
            repo.path().join("swap.txt/nested/more.bin").as_path(),
            "more\n",
        );
        let index = repo.path().join(".git/index");
        let index_before = fs::read(&index).expect("index before");

        let snapshot = load_review_snapshot(repo.path());

        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        let swap = snapshot
            .files
            .iter()
            .find(|file| file.path == "swap.txt")
            .expect("swap entry");
        assert_eq!(swap.status, FileStatus::Deleted);
        // Untracked replacement-directory contents join the review as Added.
        for child in ["swap.txt/bundle.bin", "swap.txt/nested/more.bin"] {
            let child_entry = snapshot
                .files
                .iter()
                .find(|file| file.path == child)
                .unwrap_or_else(|| panic!("untracked replacement file missing: {child}"));
            assert_eq!(child_entry.status, FileStatus::Added);
        }
        assert_eq!(fs::read(&index).expect("index after"), index_before);
    }

    #[test]
    fn load_review_snapshot_keeps_submodule_bumps_as_modifications() {
        // A bumped submodule is a tracked gitlink change. The path-limited
        // staging must keep it as a modification and never force-remove the
        // entry as a "file replaced by directory".
        let parent = init_repo();
        let sub_dir = TempDir::new().expect("submodule tempdir");
        git(sub_dir.path(), &["init"]);
        write(sub_dir.path().join("lib.txt").as_path(), "base\n");
        git(sub_dir.path(), &["add", "lib.txt"]);
        git(sub_dir.path(), &["commit", "-m", "submodule initial"]);
        let sub_path = sub_dir.path().canonicalize().expect("canonical submodule");
        git(
            parent.path(),
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                sub_path.to_str().expect("utf8 submodule path"),
                "ext",
            ],
        );
        git(parent.path(), &["commit", "-qm", "add submodule"]);
        write(parent.path().join("ext/lib.txt").as_path(), "base\ncommitted\n");
        git(&parent.path().join("ext"), &["add", "lib.txt"]);
        git(&parent.path().join("ext"), &["commit", "-qm", "submodule bump"]);
        let index = parent.path().join(".git/index");
        let index_before = fs::read(&index).expect("index before");

        let snapshot = load_review_snapshot(parent.path());

        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        let ext = snapshot
            .files
            .iter()
            .find(|file| file.path == "ext")
            .expect("submodule entry");
        assert_eq!(
            ext.status,
            FileStatus::Modified,
            "submodule bump must read as Modified: {:?}",
            snapshot
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(fs::read(&index).expect("index after"), index_before);
    }

    #[test]
    fn file_diff_load_keeps_untracked_only_gitlink_hunks() {
        let parent = init_repo();
        add_named_submodule(parent.path(), "ext");
        add_named_submodule(&parent.path().join("ext"), "nested");
        git(parent.path(), &["add", "ext"]);
        git(parent.path(), &["commit", "-qm", "record nested gitlink"]);
        let from = rev_parse(parent.path());
        write(
            parent.path().join("ext/nested/lib.txt").as_path(),
            "base\ndirty\n",
        );
        let (snapshot, paging) =
            load_review_snapshot_capture(parent.path(), ReviewScope::WorkingTree);
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        assert!(
            snapshot.files.iter().all(|file| file.path != "ext"),
            "nested dirty gitlink must expand: {:?}",
            snapshot
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>()
        );
        let inner = snapshot
            .files
            .iter()
            .find(|file| file.path == "ext/nested/lib.txt")
            .unwrap_or_else(|| panic!("nested inner missing: {:?}", snapshot.files));
        assert_eq!(inner.status, FileStatus::Modified);
        let loaded = FileDiff::load_snapshot_file(
            parent.path(),
            &snapshot.scope,
            &paging,
            inner,
        );
        assert!(loaded.error.is_none(), "{:?}", loaded.error);
        assert!(
            loaded.hunks.iter().any(|hunk| hunk.lines.iter().any(|line| {
                line.kind == DiffLineKind::Addition && line.text == "dirty"
            })),
            "{:?}",
            loaded.hunks
        );

        git(
            &parent.path().join("ext"),
            &["commit", "--allow-empty", "-qm", "empty bump"],
        );
        git(parent.path(), &["add", "ext"]);
        git(parent.path(), &["commit", "-qm", "bump gitlink"]);
        let to = rev_parse(parent.path());
        let (revision, rev_paging) = load_review_snapshot_capture(
            parent.path(),
            ReviewScope::Revisions { from, to },
        );
        assert!(revision.error.is_none(), "{:?}", revision.error);
        let revision_gitlink = revision
            .files
            .iter()
            .find(|file| file.path == "ext")
            .unwrap_or_else(|| panic!("revision gitlink row missing: {:?}", revision.files));
        assert!(
            revision
                .files
                .iter()
                .all(|file| file.path == "ext" || !file.path.starts_with("ext/")),
            "revision gitlink must stay unexpanded: {:?}",
            revision
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>()
        );
        let revision_loaded = FileDiff::load_snapshot_file(
            parent.path(),
            &revision.scope,
            &rev_paging,
            revision_gitlink,
        );
        assert!(revision_loaded.error.is_none(), "{:?}", revision_loaded.error);
        assert_eq!(revision_loaded.hunks, revision_gitlink.hunks);
    }

    #[test]
    fn load_review_snapshot_expands_untracked_only_submodule_files() {
        let parent = init_repo();
        add_named_submodule(parent.path(), "ext");
        write(
            parent.path().join("ext/lib.txt").as_path(),
            "base\ndirty\n",
        );
        write(parent.path().join("ext/scratch.txt").as_path(), "hello\n");
        let (snapshot, paging) =
            load_review_snapshot_capture(parent.path(), ReviewScope::WorkingTree);
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        assert!(
            snapshot.files.iter().all(|file| file.path != "ext"),
            "submodule must not keep a gitlink row: {:?}",
            snapshot
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>()
        );
        let tracked = snapshot
            .files
            .iter()
            .find(|file| file.path == "ext/lib.txt")
            .unwrap_or_else(|| panic!("tracked inner missing: {:?}", snapshot.files));
        assert_eq!(tracked.status, FileStatus::Modified);
        let untracked = snapshot
            .files
            .iter()
            .find(|file| file.path == "ext/scratch.txt")
            .unwrap_or_else(|| panic!("untracked inner missing: {:?}", snapshot.files));
        assert_eq!(untracked.status, FileStatus::Added);
        let loaded = FileDiff::load_snapshot_file(
            parent.path(),
            &snapshot.scope,
            &paging,
            untracked,
        );
        assert!(loaded.error.is_none(), "{:?}", loaded.error);
        assert!(
            loaded.hunks.iter().any(|hunk| hunk.lines.iter().any(|line| {
                line.kind == DiffLineKind::Addition && line.text == "hello"
            })),
            "{:?}",
            loaded.hunks
        );
        let tracked_blobs = FileContextBlobs::load(parent.path(), &paging, tracked)
            .expect("tracked inner blobs");
        assert_eq!(tracked_blobs.old.lines, ["base"]);
        assert_eq!(tracked_blobs.new.lines, ["base", "dirty"]);
        let untracked_blobs = FileContextBlobs::load(parent.path(), &paging, untracked)
            .expect("untracked inner blobs");
        assert!(untracked_blobs.old.lines.is_empty(), "{:?}", untracked_blobs.old.lines);
        assert_eq!(untracked_blobs.new.lines, ["hello"]);
    }

    #[test]
    fn load_review_snapshot_expands_nested_same_oid_dirty_gitlink() {
        let parent = init_repo();
        add_named_submodule(parent.path(), "ext");
        add_named_submodule(&parent.path().join("ext"), "nested");
        git(parent.path(), &["add", "ext"]);
        git(parent.path(), &["commit", "-qm", "record nested gitlink"]);
        write(
            parent.path().join("ext/nested/lib.txt").as_path(),
            "base\ndirty\n",
        );
        let (snapshot, paging) =
            load_review_snapshot_capture(parent.path(), ReviewScope::WorkingTree);
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        assert!(
            snapshot.files.iter().all(|file| file.path != "ext"),
            "parent gitlink must expand: {:?}",
            snapshot
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>()
        );
        let inner = snapshot
            .files
            .iter()
            .find(|file| file.path == "ext/nested/lib.txt")
            .unwrap_or_else(|| panic!("nested content missing: {:?}", snapshot.files));
        assert_eq!(inner.status, FileStatus::Modified);
        let blobs = FileContextBlobs::load(parent.path(), &paging, inner)
            .expect("nested inner blobs");
        assert_eq!(blobs.old.lines, ["base"]);
        assert_eq!(blobs.new.lines, ["base", "dirty"]);
    }

    #[test]
    fn load_review_snapshot_splices_untracked_when_inner_tracked_is_nested_gitlink() {
        let parent = init_repo();
        add_named_submodule(parent.path(), "ext");
        add_named_submodule(&parent.path().join("ext"), "nested");
        git(parent.path(), &["add", "ext"]);
        git(parent.path(), &["commit", "-qm", "record nested gitlink"]);
        write(
            parent.path().join("ext/nested/lib.txt").as_path(),
            "base\ndirty\n",
        );
        write(parent.path().join("ext/scratch.txt").as_path(), "hello\n");
        let (snapshot, paging) =
            load_review_snapshot_capture(parent.path(), ReviewScope::WorkingTree);
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        assert!(
            snapshot.files.iter().all(|file| file.path != "ext"),
            "parent gitlink must expand: {:?}",
            snapshot
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>()
        );
        let nested = snapshot
            .files
            .iter()
            .find(|file| file.path == "ext/nested/lib.txt")
            .unwrap_or_else(|| panic!("nested content missing: {:?}", snapshot.files));
        assert_eq!(nested.status, FileStatus::Modified);
        let untracked = snapshot
            .files
            .iter()
            .find(|file| file.path == "ext/scratch.txt")
            .unwrap_or_else(|| panic!("untracked inner missing: {:?}", snapshot.files));
        assert_eq!(untracked.status, FileStatus::Added);
        let nested_blobs = FileContextBlobs::load(parent.path(), &paging, nested)
            .expect("nested inner blobs");
        assert_eq!(nested_blobs.old.lines, ["base"]);
        assert_eq!(nested_blobs.new.lines, ["base", "dirty"]);
        let untracked_blobs = FileContextBlobs::load(parent.path(), &paging, untracked)
            .expect("untracked inner blobs");
        assert!(
            untracked_blobs.old.lines.is_empty(),
            "{:?}",
            untracked_blobs.old.lines
        );
        assert_eq!(untracked_blobs.new.lines, ["hello"]);
    }

    fn record_nested_gitlink_ancestors(parent: &Path, names: &[&str], depth: usize) {
        for index in (0..depth).rev() {
            let mut dir = parent.to_path_buf();
            for name in &names[..index] {
                dir = dir.join(name);
            }
            git(&dir, &["add", names[index]]);
            git(&dir, &["commit", "-qm", "record nested gitlink"]);
        }
    }

    #[test]
    fn load_review_snapshot_keeps_fifth_nested_gitlink_at_depth_cap() {
        let parent = init_repo();
        let names = ["l1", "l2", "l3", "l4", "l5"];
        add_named_submodule(parent.path(), names[0]);
        for depth in 1..names.len() {
            let mut leaf_parent = parent.path().to_path_buf();
            for name in &names[..depth] {
                leaf_parent = leaf_parent.join(name);
            }
            add_named_submodule(&leaf_parent, names[depth]);
            record_nested_gitlink_ancestors(parent.path(), &names, depth);
        }
        let mut dir = parent.path().to_path_buf();
        for name in names {
            dir = dir.join(name);
            write(dir.join("lib.txt").as_path(), "base\ndirty\n");
        }
        let snapshot = load_review_snapshot(parent.path());
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        let paths: Vec<&str> = snapshot
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect();
        assert!(
            paths.iter().all(|path| *path != "l1"),
            "level 1 must expand: {paths:?}"
        );
        assert!(
            paths.iter().all(|path| *path != "l1/l2"
                && *path != "l1/l2/l3"
                && *path != "l1/l2/l3/l4"),
            "levels 2-4 must expand: {paths:?}"
        );
        let fifth = snapshot
            .files
            .iter()
            .find(|file| file.path == "l1/l2/l3/l4/l5")
            .unwrap_or_else(|| panic!("depth-cap gitlink missing: {paths:?}"));
        assert!(is_gitlink_diff_file(fifth), "{fifth:?}");
        assert!(
            paths.iter().all(|path| *path != "l1/l2/l3/l4/l5/lib.txt"),
            "level 5 must stay a gitlink: {paths:?}"
        );
    }

    #[test]
    fn load_revision_snapshot_expands_via_modules_dir_after_deinit() {
        let (parent, from, to) = submodule_repo_with_revision_inner_diff();
        git(parent.path(), &["submodule", "deinit", "-f", "ext"]);
        assert!(
            parent.path().join(".git/modules/ext/HEAD").is_file(),
            "modules dir must retain HEAD"
        );
        let snapshot = load_review_snapshot_for(
            parent.path(),
            ReviewScope::Revisions { from, to },
        );
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        assert!(
            snapshot.files.iter().any(|file| file.path == "ext/lib.txt"),
            "modules-dir expansion missing: {:?}",
            snapshot.files
        );
        assert!(
            snapshot.files.iter().all(|file| file.path != "ext"),
            "gitlink must be replaced: {:?}",
            snapshot
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn load_revision_snapshot_keeps_gitlink_when_modules_dir_removed() {
        let (parent, from, to) = submodule_repo_with_revision_inner_diff();
        git(parent.path(), &["submodule", "deinit", "-f", "ext"]);
        fs::remove_dir_all(parent.path().join(".git/modules/ext")).expect("remove modules dir");
        let snapshot = load_review_snapshot_for(
            parent.path(),
            ReviewScope::Revisions { from, to },
        );
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        assert!(
            snapshot.files.iter().any(|file| file.path == "ext"),
            "gitlink row must stay: {:?}",
            snapshot.files
        );
        assert!(
            snapshot
                .files
                .iter()
                .all(|file| file.path != "ext/lib.txt"),
            "{:?}",
            snapshot.files
        );
    }

    #[test]
    fn load_revision_snapshot_keeps_gitlink_when_inner_objects_missing() {
        let (parent, from, to) = submodule_repo_with_revision_inner_diff();
        fs::remove_dir_all(parent.path().join(".git/modules/ext/objects"))
            .expect("drop submodule objects");
        let snapshot = load_review_snapshot_for(
            parent.path(),
            ReviewScope::Revisions { from, to },
        );
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        assert!(
            snapshot.files.iter().any(|file| file.path == "ext"),
            "missing objects must keep the gitlink: {:?}",
            snapshot.files
        );
        assert!(
            snapshot
                .files
                .iter()
                .all(|file| file.path != "ext/lib.txt"),
            "{:?}",
            snapshot.files
        );
    }

    #[test]
    fn file_diff_load_does_not_complete_truncated_gitlink() {
        let parent = init_repo();
        add_named_submodule(parent.path(), "ext");
        let from = rev_parse(parent.path());
        git(
            &parent.path().join("ext"),
            &["commit", "--allow-empty", "-qm", "empty bump"],
        );
        git(parent.path(), &["add", "ext"]);
        git(parent.path(), &["commit", "-qm", "bump gitlink"]);
        let to = rev_parse(parent.path());
        let scope = ReviewScope::Revisions {
            from: from.clone(),
            to: to.clone(),
        };
        let (_complete, paging) = load_review_snapshot_capture(parent.path(), scope.clone());
        let old_sha = strip_output_line_ending(
            &String::from_utf8(git_output(
                &parent.path().join("ext"),
                &["rev-parse", "HEAD^"],
            ))
            .expect("old sha"),
        )
        .to_owned();
        let new_sha = rev_parse(&parent.path().join("ext"));
        let mut patch =
            b"diff --git a/aa.txt b/aa.txt\n--- a/aa.txt\n+++ b/aa.txt\n@@ -1 +1 @@\n-old\n+new\n"
                .to_vec();
        patch.extend(
            format!(
                "diff --git a/ext b/ext\nindex {old_sha}..{new_sha} 160000\n--- a/ext\n+++ b/ext\n@@ -1 +1 @@\n-Subproject commit {old_sha}\n"
            )
            .into_bytes(),
        );
        let snapshot = snapshot_from_diff_output(
            parent.path().to_path_buf(),
            scope.clone(),
            b"revisions-truncated-gitlink",
            GitOutput {
                stdout: patch,
                truncated: true,
                error: None,
            },
            Some(&GitOutput {
                stdout: b"M\0aa.txt\0M\0ext\0".to_vec(),
                truncated: false,
                error: None,
            }),
            &GitOutput {
                stdout: b"1\t1\taa.txt\01\t1\text\0".to_vec(),
                truncated: false,
                error: None,
            },
            None,
            None,
        );
        let gitlink = snapshot
            .files
            .last()
            .unwrap_or_else(|| panic!("truncated gitlink missing: {:?}", snapshot.files));
        assert_eq!(gitlink.path, "ext");
        assert!(gitlink.truncated, "{gitlink:?}");
        assert!(is_gitlink_diff_file(gitlink), "{gitlink:?}");
        let protocol: Vec<&DiffLine> = gitlink
            .hunks
            .iter()
            .flat_map(|hunk| hunk.lines.iter())
            .filter(|line| line.kind != DiffLineKind::Meta)
            .collect();
        assert_eq!(protocol.len(), 1, "{:?}", gitlink.hunks);
        assert_eq!(protocol[0].kind, DiffLineKind::Deletion);
        assert!(is_gitlink_protocol_line(&protocol[0].text));

        let loaded = FileDiff::load_snapshot_file(parent.path(), &scope, &paging, gitlink);
        assert!(
            loaded.error.is_some()
                || loaded.byte_capped
                || loaded.hunks != gitlink.hunks,
            "truncated gitlink must not be served as a complete copy of the partial block: {loaded:?}"
        );
        assert!(loaded.error.is_none(), "{:?}", loaded.error);
        assert!(!loaded.byte_capped);
        assert_ne!(loaded.hunks, gitlink.hunks);
        assert!(
            loaded.hunks.iter().any(|hunk| hunk.lines.iter().any(|line| {
                line.kind == DiffLineKind::Addition && is_gitlink_protocol_line(&line.text)
            })),
            "{:?}",
            loaded.hunks
        );
        assert!(
            loaded.hunks.iter().any(|hunk| hunk.lines.iter().any(|line| {
                line.kind == DiffLineKind::Deletion && is_gitlink_protocol_line(&line.text)
            })),
            "{:?}",
            loaded.hunks
        );
    }

    #[test]
    fn load_review_snapshot_expands_initialized_submodule_inner_diff() {
        // An initialized submodule's uncommitted inner edits must enter the
        // parent snapshot as real file entries (`ext/lib.txt`) carrying their
        // own hunks, replacing the unresolvable gitlink block; the comments
        // pipeline then works on them like any regular file.
        let parent = init_repo();
        let sub_dir = TempDir::new().expect("submodule tempdir");
        git(sub_dir.path(), &["init"]);
        write(sub_dir.path().join("lib.txt").as_path(), "base\n");
        git(sub_dir.path(), &["add", "lib.txt"]);
        git(sub_dir.path(), &["commit", "-m", "submodule initial"]);
        let sub_path = sub_dir.path().canonicalize().expect("canonical submodule");
        git(
            parent.path(),
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                sub_path.to_str().expect("utf8 submodule path"),
                "ext",
            ],
        );
        git(parent.path(), &["commit", "-qm", "add submodule"]);
        // Leave the checked-out gitlink at the parent commit and modify its
        // worktree so the direct parent diff carries no `index … 160000` line.
        write(parent.path().join("ext/lib.txt").as_path(), "base\nchanged\n");

        let (snapshot, paging) =
            load_review_snapshot_capture(parent.path(), ReviewScope::WorkingTree);

        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        let inner = snapshot
            .files
            .iter()
            .find(|file| file.path == "ext/lib.txt")
            .unwrap_or_else(|| panic!("submodule inner file absent: {:?}", snapshot.files));
        assert_eq!(inner.status, FileStatus::Modified);
        assert!(
            inner.hunks.iter().any(|hunk| hunk
                .lines
                .iter()
                .any(|line| line.kind == DiffLineKind::Addition && line.text == "changed")),
            "the inner diff must carry the submodule's own hunks: {:?}",
            inner.hunks
        );
        // The gitlink block was replaced by the splice: the bare submodule
        // path itself is no longer a snapshot entry.
        assert!(
            snapshot.files.iter().all(|file| file.path != "ext"),
            "gitlink entry must be replaced by its expanded files: {:?}",
            snapshot.files.iter().map(|file| file.path.as_str()).collect::<Vec<_>>()
        );
        write(
            parent.path().join("ext/lib.txt").as_path(),
            "LIVE-WORKTREE-SHOULD-NOT-APPEAR\n",
        );
        let loaded = FileDiff::load_snapshot_file(
            parent.path(),
            &snapshot.scope,
            &paging,
            inner,
        );
        assert!(loaded.error.is_none(), "{:?}", loaded.error);
        let loaded_lines: Vec<&str> = loaded
            .hunks
            .iter()
            .flat_map(|hunk| hunk.lines.iter().map(|line| line.text.as_str()))
            .collect();
        assert!(loaded_lines.contains(&"changed"), "{loaded_lines:?}");
        assert!(
            !loaded_lines.contains(&"LIVE-WORKTREE-SHOULD-NOT-APPEAR"),
            "{loaded_lines:?}"
        );
        let page = loaded
            .slice_page(&snapshot.snapshot_id, 0, 1)
            .expect("expanded submodule page");
        assert_eq!(page.path, "ext/lib.txt");
        assert!(!page.hunks.is_empty(), "{page:?}");
        let blobs =
            FileContextBlobs::load(parent.path(), &paging, inner).expect("submodule blobs");
        assert_eq!(blobs.old.lines, ["base"]);
        assert_eq!(blobs.new.lines, ["base", "changed"]);
        let slice = blobs.slice(1, 1, 8).expect("submodule context");
        assert_eq!(slice.old_line_count, 1);
        assert_eq!(slice.new_line_count, 2);
    }

    #[test]
    fn load_review_snapshot_keeps_clean_submodule_bump_when_another_expands() {
        // When one submodule's gitlink block expands into real file diffs, a
        // sibling submodule whose checkout is clean (SHA bumped, no inner
        // diff) must keep its gitlink entry — a failed/empty inner capture
        // never silently drops the tracked SHA change from the snapshot.
        let parent = init_repo();
        for name in ["ext1", "ext2"] {
            let sub_dir = TempDir::new().expect("submodule tempdir");
            git(sub_dir.path(), &["init"]);
            write(sub_dir.path().join("lib.txt").as_path(), "base\n");
            git(sub_dir.path(), &["add", "lib.txt"]);
            git(sub_dir.path(), &["commit", "-m", "submodule initial"]);
            let sub_path = sub_dir.path().canonicalize().expect("canonical submodule");
            git(
                parent.path(),
                &[
                    "-c",
                    "protocol.file.allow=always",
                    "submodule",
                    "add",
                    "-q",
                    sub_path.to_str().expect("utf8 submodule path"),
                    name,
                ],
            );
            git(parent.path(), &["commit", "-qm", &format!("add {name}")]);
            write(
                parent.path().join(name).join("lib.txt").as_path(),
                "base\ncommitted\n",
            );
            git(&parent.path().join(name), &["add", "lib.txt"]);
            git(&parent.path().join(name), &["commit", "-qm", "submodule bump"]);
        }
        // ext2 additionally carries an uncommitted inner edit so its gitlink
        // block expands into a real file diff; ext1 stays clean.
        write(
            parent.path().join("ext2/lib.txt").as_path(),
            "base\ncommitted\nchanged\n",
        );

        let snapshot = load_review_snapshot(parent.path());

        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        let paths: Vec<&str> = snapshot.files.iter().map(|file| file.path.as_str()).collect();
        let ext1 = snapshot
            .files
            .iter()
            .find(|file| file.path == "ext1")
            .expect("clean submodule bump must stay a snapshot entry");
        assert_eq!(ext1.status, FileStatus::Modified);
        assert!(!paths.contains(&"ext1/lib.txt"), "{paths:?}");
        assert!(paths.contains(&"ext2/lib.txt"), "{paths:?}");
        assert!(!paths.contains(&"ext2"), "expanded submodule path is replaced: {paths:?}");

        let (snapshot, paging) =
            load_review_snapshot_capture(parent.path(), ReviewScope::WorkingTree);
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        let ext1 = snapshot
            .files
            .iter()
            .find(|file| file.path == "ext1")
            .expect("clean submodule bump must stay a snapshot entry");
        let blobs = FileContextBlobs::load(parent.path(), &paging, ext1);
        let blobs = match blobs {
            Ok(blobs) => blobs,
            Err(error) => panic!("gitlink context must not git-show: {error}"),
        };
        assert!(
            !blobs.old.binary && !blobs.new.binary,
            "gitlink context is empty text, not binary"
        );
        assert!(blobs.old.lines.is_empty() && blobs.new.lines.is_empty());
        let probe = blobs.slice(0, 0, 0).expect("count=0 probe");
        assert_eq!(probe.old_line_count, 0);
        assert_eq!(probe.new_line_count, 0);
        assert!(!probe.binary);
        let show = Command::new("git")
            .args([
                "--no-pager",
                "-c",
                "color.ui=false",
                "show",
                "--no-textconv",
                "--no-ext-diff",
                "--end-of-options",
                "HEAD:ext1",
            ])
            .current_dir(parent.path())
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                parent.path().join("absent-global-git-config"),
            )
            .env("LC_ALL", "C")
            .output()
            .expect("git show gitlink");
        assert!(
            !show.status.success(),
            "parent git show of a gitlink must fail"
        );
        let stderr = String::from_utf8_lossy(&show.stderr);
        assert!(
            stderr.contains("unable to read") || stderr.contains("bad object"),
            "context probe used to toast git's missing gitlink object: {stderr}"
        );
    }

    #[test]
    fn expand_inner_header_prefixes_paths_correctly() {
        assert_eq!(
            expand_inner_header("diff --git a/lib.txt b/lib.txt", "ext").as_deref(),
            Some("diff --git a/ext/lib.txt b/ext/lib.txt")
        );
        assert_eq!(
            expand_inner_header("--- a/lib.txt", "ext").as_deref(),
            Some("--- a/ext/lib.txt")
        );
        assert_eq!(
            expand_inner_header("+++ b/lib.txt", "ext").as_deref(),
            Some("+++ b/ext/lib.txt")
        );
        assert_eq!(
            expand_inner_header("diff --git a/old.txt b/new.txt", "ext").as_deref(),
            Some("diff --git a/ext/old.txt b/ext/new.txt")
        );
        assert_eq!(
            expand_inner_header("--- /dev/null", "ext").as_deref(),
            Some("--- /dev/null")
        );
        assert_eq!(
            expand_inner_header("+++ /dev/null", "ext").as_deref(),
            Some("+++ /dev/null")
        );
        assert_eq!(
            expand_inner_header("--- a/sp ace.txt\t", "ext").as_deref(),
            Some("--- \"a/ext/sp\\040ace.txt\"\t")
        );
        assert_eq!(
            strip_diff_path("--- \"a/ext/sp\\040ace.txt\"\t"),
            "ext/sp ace.txt"
        );
        let quoted = expand_inner_header(
            "diff --git \"a/w\\303\\251ird\" \"b/w\\303\\251ird\"",
            "ext",
        )
        .expect("quoted header rewrites");
        assert_eq!(
            quoted,
            "diff --git \"a/ext/w\\303\\251ird\" \"b/ext/w\\303\\251ird\""
        );
        let (from, to) =
            parse_diff_git_paths(quoted.strip_prefix("diff --git ").expect("header"));
        assert_eq!(from.as_deref(), Some("ext/wéird"));
        assert_eq!(to.as_deref(), Some("ext/wéird"));
        assert_eq!(
            strip_submodule_path_bytes(r"ext/literal\\101.txt", "ext"),
            Some(br"literal\101.txt".to_vec())
        );
        assert_eq!(
            strip_submodule_path_bytes(r"ext/nonutf\377.txt", "ext"),
            Some(b"nonutf\xff.txt".to_vec())
        );
    }

    #[test]
    fn expand_inner_header_fails_closed_on_malformed_lines() {
        assert_eq!(expand_inner_header("diff --git a/lib.txt", "ext"), None);
        assert_eq!(expand_inner_header("diff --git one two three", "ext"), None);
        assert_eq!(expand_inner_header("--- lib.txt", "ext"), None);
        assert_eq!(expand_inner_header("+++ lib.txt", "ext"), None);
        assert_eq!(expand_inner_header("@@ -1 +1 @@", "ext"), None);
    }

    fn submodule_repo_with_dirty_checkout() -> (TempDir, String) {
        let parent = init_repo();
        let sub_dir = TempDir::new().expect("submodule tempdir");
        git(sub_dir.path(), &["init"]);
        write(sub_dir.path().join("lib.txt").as_path(), "base\n");
        write(sub_dir.path().join("wé ird.txt").as_path(), "base\n");
        git(sub_dir.path(), &["add", "lib.txt", "wé ird.txt"]);
        git(sub_dir.path(), &["commit", "-qm", "submodule initial"]);
        let sub_path = sub_dir.path().canonicalize().expect("canonical submodule");
        git(
            parent.path(),
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                sub_path.to_str().expect("utf8 submodule path"),
                "ext",
            ],
        );
        git(parent.path(), &["commit", "-qm", "add submodule"]);
        let output = Command::new("git")
            .arg("-C")
            .arg(parent.path().join("ext"))
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("rev-parse submodule head");
        assert!(output.status.success(), "{output:?}");
        let old_sha = String::from_utf8(output.stdout).expect("utf8 sha");
        let old_sha = old_sha.trim().to_owned();
        git(
            &parent.path().join("ext"),
            &["commit", "--allow-empty", "-qm", "submodule bump"],
        );
        write(parent.path().join("ext/lib.txt").as_path(), "base\nchanged\n");
        write(parent.path().join("ext/wé ird.txt").as_path(), "base\nchanged\n");
        (parent, old_sha)
    }

    #[test]
    fn expanded_submodule_rename_pages_from_child_snapshot() {
        let (parent, _) = submodule_repo_with_dirty_checkout();
        write(parent.path().join("ext/lib.txt").as_path(), "base\n");
        git(
            &parent.path().join("ext"),
            &["mv", "lib.txt", "renamed.txt"],
        );
        let (snapshot, paging) =
            load_review_snapshot_capture(parent.path(), ReviewScope::WorkingTree);
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        let file = snapshot
            .files
            .iter()
            .find(|file| file.path == "ext/renamed.txt")
            .unwrap_or_else(|| panic!("renamed child missing: {:?}", snapshot.files));
        assert_eq!(file.status, FileStatus::Renamed);
        assert_eq!(file.previous_path.as_deref(), Some("ext/lib.txt"));
        let loaded =
            FileDiff::load_snapshot_file(parent.path(), &snapshot.scope, &paging, file);
        assert!(loaded.error.is_none(), "{:?}", loaded.error);
        assert_eq!(loaded.previous_path.as_deref(), Some("ext/lib.txt"));
        let blobs =
            FileContextBlobs::load(parent.path(), &paging, file).expect("renamed child blobs");
        assert_eq!(blobs.old.lines, ["base"]);
        assert_eq!(blobs.new.lines, ["base"]);
    }

    #[test]
    fn load_review_snapshot_expands_dirty_submodule_for_base_scope() {
        let (parent, _old_sha) = submodule_repo_with_dirty_checkout();

        let snapshot = load_review_snapshot_for(
            parent.path(),
            ReviewScope::BaseToWorkingTree {
                base: "HEAD".to_owned(),
            },
        );

        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        let paths: Vec<&str> = snapshot.files.iter().map(|file| file.path.as_str()).collect();
        let inner = snapshot
            .files
            .iter()
            .find(|file| file.path == "ext/lib.txt")
            .expect("base-scope snapshot must carry the expanded inner file");
        assert_eq!(inner.status, FileStatus::Modified);
        assert!(
            inner.hunks.iter().any(|hunk| hunk
                .lines
                .iter()
                .any(|line| line.kind == DiffLineKind::Addition && line.text == "changed")),
            "the inner diff must carry the submodule's own hunks: {:?}",
            inner.hunks
        );
        assert!(paths.contains(&"ext/wé ird.txt"), "{paths:?}");
        assert!(
            !paths.contains(&"ext"),
            "expanded submodule path is replaced: {paths:?}"
        );
    }

    #[test]
    fn load_review_snapshot_expands_dirty_gitlink_when_parent_diff_is_truncated() {
        // Parent combined patches past MAX_DIFF_BYTES used to skip gitlink
        // expansion entirely and catalog the gitlink as an empty-hunk
        // placeholder, so context probe ran git show HASH:gitlink in the
        // parent object store. A dirty initialized submodule must still
        // splice even when a sibling file truncates the parent capture.
        let parent = init_repo();
        add_named_submodule(parent.path(), "ext");
        write(parent.path().join("ext/lib.txt").as_path(), "base\nchanged\n");
        let oversized = "x".repeat(MAX_DIFF_BYTES + 4096);
        write(parent.path().join("huge.txt").as_path(), &format!("{oversized}\n"));
        git(parent.path(), &["add", "huge.txt"]);
        git(parent.path(), &["commit", "-qm", "add huge"]);
        write(
            parent.path().join("huge.txt").as_path(),
            &format!("y{oversized}\n"),
        );

        let (snapshot, paging) =
            load_review_snapshot_capture(parent.path(), ReviewScope::WorkingTree);
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        assert!(
            snapshot.truncated,
            "parent combined patch must truncate: files={:?}",
            snapshot.files.iter().map(|file| file.path.as_str()).collect::<Vec<_>>()
        );
        let paths: Vec<&str> = snapshot.files.iter().map(|file| file.path.as_str()).collect();
        assert!(
            paths.contains(&"ext/lib.txt"),
            "dirty gitlink must still expand when parent snapshot is truncated: {paths:?}"
        );
        assert!(
            !paths.contains(&"ext"),
            "bare gitlink must not remain as a changed row: {paths:?}"
        );
        let inner = snapshot
            .files
            .iter()
            .find(|file| file.path == "ext/lib.txt")
            .expect("expanded inner file");
        let blobs = FileContextBlobs::load(parent.path(), &paging, inner)
            .expect("inner context must load from the child repository");
        assert!(
            blobs.new.lines.iter().any(|line| line.contains("changed")),
            "inner context must be child file text, not a gitlink: {blobs:?}"
        );
        assert!(
            paths.contains(&"huge.txt"),
            "truncated sibling must remain as preview or catalog placeholder: {paths:?}"
        );
    }

    #[test]
    fn expand_gitlink_diffs_keeps_combined_stdout_within_cap_when_parent_truncated() {
        let (parent, old_sha) = submodule_repo_with_dirty_checkout();
        let gitlink_block = format!(
            "diff --git a/ext b/ext\nindex {old_sha}..{old_sha} 160000\n--- a/ext\n+++ b/ext\n@@ -1 +1 @@\n-Subproject commit {old_sha}\n+Subproject commit {old_sha}-dirty\n"
        );
        let mut parent_file = String::from(
            "diff --git a/huge.txt b/huge.txt\nindex 1111111..2222222 100644\n--- a/huge.txt\n+++ b/huge.txt\n@@ -1 +1 @@\n-old\n+",
        );
        parent_file.push_str(&"x".repeat(48 * 1024));
        parent_file.push('\n');
        let cap = 32 * 1024;
        let mut stdout = gitlink_block.clone();
        stdout.push_str(&parent_file);
        let mut bytes = stdout.into_bytes();
        assert!(bytes.len() > cap, "fixture must exceed the combined cap");
        bytes.truncate(cap);
        let output = GitOutput {
            stdout: bytes,
            truncated: true,
            error: None,
        };

        let (expanded, paths, gitlinks, files, _) = expand_gitlink_diffs(
            &output,
            &gitlink_paths_for(&gitlink_block.split('\n').map(str::to_owned).collect::<Vec<_>>()),
            parent.path(),
            cap,
            None,
            &mut 0,
        )
        .expect("expanded");

        assert!(
            expanded.stdout.len() <= cap,
            "combined stdout {} exceeded cap {cap}",
            expanded.stdout.len()
        );
        assert!(expanded.truncated);
        assert!(paths.contains("ext/lib.txt"), "{paths:?}");
        assert!(gitlinks.contains("ext"), "{gitlinks:?}");
        assert!(files.contains_key("ext/lib.txt"), "{files:?}");
        let body = String::from_utf8_lossy(&expanded.stdout);
        assert!(
            body.contains("diff --git a/ext/lib.txt b/ext/lib.txt"),
            "{body}"
        );
        assert!(
            !body.contains("diff --git a/ext b/ext"),
            "bare gitlink must be replaced when the inner splice fits: {body}"
        );
    }

    #[test]
    fn expand_gitlink_diffs_keeps_gitlink_when_inner_capture_exceeds_cap() {
        let (parent, old_sha) = submodule_repo_with_dirty_checkout();
        let gitlink_block = format!(
            "diff --git a/ext b/ext\nindex {old_sha}..{old_sha} 160000\n--- a/ext\n+++ b/ext\n@@ -1 +1 @@\n-Subproject commit {old_sha}\n+Subproject commit {old_sha}-dirty\n"
        );
        let output = GitOutput {
            stdout: gitlink_block.clone().into_bytes(),
            truncated: false,
            error: None,
        };

        let (kept, paths, _, _, _) = expand_gitlink_diffs(
            &output,
            &gitlink_paths_for(&gitlink_block.split('\n').map(str::to_owned).collect::<Vec<_>>()),
            parent.path(),
            gitlink_block.len(),
            None,
            &mut 0,
        )
        .expect("kept");

        assert!(paths.is_empty(), "{paths:?}");
        assert_eq!(kept.stdout, gitlink_block.clone().into_bytes());
        assert!(kept.truncated);
        assert!(kept.error.is_none());
        let mid_cap = gitlink_block.len() + 8192;
        let (expanded, paths, _, _, _) = expand_gitlink_diffs(
            &output,
            &gitlink_paths_for(&gitlink_block.split('\n').map(str::to_owned).collect::<Vec<_>>()),
            parent.path(),
            mid_cap,
            None,
            &mut 0,
        )
        .expect("expanded");
        assert!(paths.contains("ext/lib.txt"), "{paths:?}");
        assert!(paths.contains("ext/wé ird.txt"), "{paths:?}");
        assert!(expanded.stdout.len() <= mid_cap);
        let expanded_body = String::from_utf8(expanded.stdout).expect("utf8 splice");
        assert!(!expanded_body.ends_with("\n\n"), "{expanded_body:?}");
        assert!(
            expanded_body.contains("diff --git a/ext/lib.txt b/ext/lib.txt")
                && expanded_body.contains("--- a/ext/lib.txt")
                && expanded_body.contains("+++ b/ext/lib.txt"),
            "{expanded_body}"
        );
        assert!(
            expanded_body
                .contains("diff --git \"a/ext/w\\303\\251\\040ird.txt\" \"b/ext/w\\303\\251\\040ird.txt\"")
                && expanded_body.contains("--- \"a/ext/w\\303\\251\\040ird.txt\""),
            "{expanded_body}"
        );
        assert!(!expanded.truncated);
    }

    #[test]
    fn expand_gitlink_diffs_fails_closed_on_path_escape() {
        let parent = init_repo();
        let escape_block = "diff --git a/../escape b/../escape\nindex 0123456789abcdef0123456789abcdef01234567..0123456789abcdef0123456789abcdef01234567 160000\n".to_owned();
        let output = GitOutput {
            stdout: escape_block.clone().into_bytes(),
            truncated: false,
            error: None,
        };

        let (kept, paths, _, _, _) = expand_gitlink_diffs(
            &output,
            &gitlink_paths_for(&escape_block.split('\n').map(str::to_owned).collect::<Vec<_>>()),
            parent.path(),
            MAX_DIFF_BYTES,
            None,
            &mut 0,
        )
        .expect("kept");
        assert!(paths.is_empty(), "{paths:?}");
        assert_eq!(kept.stdout, escape_block.into_bytes());
        assert!(!kept.truncated);
        assert!(kept.error.is_none());
    }

    #[test]
    fn expand_gitlink_diffs_keeps_gitlink_on_inner_nonzero_error() {
        let (parent, _old_sha) = submodule_repo_with_dirty_checkout();
        let gitlink_block = "diff --git a/ext b/ext\nindex 1234567890abcdef1234567890abcdef12345678..fedcba091234567890abcdef1234567890abcdef 160000\n--- a/ext\n+++ b/ext\n@@ -1 +1 @@\n-Subproject commit 1234567890abcdef1234567890abcdef12345678\n+Subproject commit fedcba091234567890abcdef1234567890abcdef-dirty\n".to_owned();
        let output = GitOutput {
            stdout: gitlink_block.clone().into_bytes(),
            truncated: false,
            error: None,
        };

        let (kept, paths, _, _, _) = expand_gitlink_diffs(
            &output,
            &gitlink_paths_for(&gitlink_block.split('\n').map(str::to_owned).collect::<Vec<_>>()),
            parent.path(),
            MAX_DIFF_BYTES,
            None,
            &mut 0,
        )
        .expect("inner git error must keep the gitlink row");
        assert!(paths.is_empty(), "{paths:?}");
        assert_eq!(kept.stdout, gitlink_block.into_bytes());
        assert!(kept.error.is_none());
        assert!(!kept.truncated);
    }

    #[test]
    fn expand_gitlink_diffs_keeps_gitlink_without_inner_base() {
        let (parent, _old_sha) = submodule_repo_with_dirty_checkout();
        let added_block = "diff --git a/ext b/ext\nnew file mode 160000\nindex 0000000..1234567890abcdef1234567890abcdef12345678\n--- /dev/null\n+++ b/ext\n@@ -0,0 +1 @@\n+Subproject commit 1234567890abcdef1234567890abcdef12345678-dirty\n".to_owned();
        let output = GitOutput {
            stdout: added_block.clone().into_bytes(),
            truncated: false,
            error: None,
        };

        let (kept, paths, _, _, _) = expand_gitlink_diffs(
            &output,
            &gitlink_paths_for(&added_block.split('\n').map(str::to_owned).collect::<Vec<_>>()),
            parent.path(),
            MAX_DIFF_BYTES,
            None,
            &mut 0,
        )
        .expect("kept");

        assert!(paths.is_empty(), "{paths:?}");
        assert_eq!(kept.stdout, added_block.into_bytes());
        assert!(kept.error.is_none());
        assert!(!kept.truncated);
    }

    #[test]
    fn expand_gitlink_diffs_isolates_submodule_repo_local_config() {
        let (parent, old_sha) = submodule_repo_with_dirty_checkout();
        let ext = parent.path().join("ext");
        git(&ext, &["config", "filter.pwn.clean", "touch pwned.clean"]);
        git(&ext, &["config", "core.fsmonitor", "touch fsmonitor-ran"]);
        write(ext.join(".gitattributes").as_path(), "*.txt filter=pwn\n");
        let gitlink_block = format!(
            "diff --git a/ext b/ext\nindex {old_sha}..{old_sha} 160000\n--- a/ext\n+++ b/ext\n@@ -1 +1 @@\n-Subproject commit {old_sha}\n+Subproject commit {old_sha}-dirty\n"
        );
        let output = GitOutput {
            stdout: gitlink_block.clone().into_bytes(),
            truncated: false,
            error: None,
        };

        let (expanded, paths, _, _, _) = expand_gitlink_diffs(
            &output,
            &gitlink_paths_for(&gitlink_block.split('\n').map(str::to_owned).collect::<Vec<_>>()),
            parent.path(),
            MAX_DIFF_BYTES,
            None,
            &mut 0,
        )
        .expect("expanded");

        assert!(paths.contains("ext/lib.txt"), "{paths:?}");
        let body = String::from_utf8(expanded.stdout).expect("utf8 splice");
        assert!(body.contains("+changed"), "{body}");
        assert!(!ext.join("pwned.clean").exists(), "clean filter must not run");
        assert!(
            !ext.join("fsmonitor-ran").exists(),
            "fsmonitor command must not run"
        );
    }

    #[test]
    fn replace_gitlink_blocks_keeps_normal_file_with_gitlink_like_content() {
        // Gitlink-looking hunk-body text must not classify a normal file.
        let normal = "diff --git a/docs/notes.txt b/docs/notes.txt\nindex 1111111111111111111111111111111111111111..3333333333333333333333333333333333333333 100644\n--- a/docs/notes.txt\n+++ b/docs/notes.txt\n@@ -1,5 +1,5 @@\n base\n-Subproject commit deadbeefdeadbeefdeadbeefdeadbeefdeadbeef\n+Subproject commit beefdeadbeefdeadbeefdeadbeefdeadbeefdead\n index 1111111111111111111111111111111111111111..2222222222222222222222222222222222222222 160000\n deleted file mode 160000\n-index 3333333333333333333333333333333333333333..4444444444444444444444444444444444444444 160000\n+index 5555555555555555555555555555555555555555..6666666666666666666666666666666666666666 160000\n";
        let gitlink = "diff --git a/ext b/ext\nindex 6666666666666666666666666666666666666666..6666666666666666666666666666666666666666 160000\n--- a/ext\n+++ b/ext\n@@ -1 +1 @@\n-Subproject commit 6666666666666666666666666666666666666666\n+Subproject commit 6666666666666666666666666666666666666666-dirty\n";
        let mut authority = BTreeSet::new();
        authority.insert(OsString::from("ext"));
        let merged = replace_gitlink_blocks(
            &GitOutput {
                stdout: normal.as_bytes().to_vec(),
                truncated: false,
                error: None,
            },
            &authority,
            &GitOutput {
                stdout: gitlink.as_bytes().to_vec(),
                truncated: false,
                error: None,
            },
            MAX_DIFF_BYTES,
        );
        let body = String::from_utf8(merged.stdout).expect("utf8 merge");
        assert!(
            body.contains("diff --git a/docs/notes.txt b/docs/notes.txt"),
            "{body}"
        );
        assert!(
            body.contains("-Subproject commit deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"),
            "{body}"
        );
        assert!(
            body.contains("+Subproject commit beefdeadbeefdeadbeefdeadbeefdeadbeefdead"),
            "{body}"
        );
        assert!(
            body.contains(
                " index 1111111111111111111111111111111111111111..2222222222222222222222222222222222222222 160000"
            ),
            "{body}"
        );
        assert!(body.contains(" deleted file mode 160000"), "{body}");
        assert!(
            body.contains(
                "-index 3333333333333333333333333333333333333333..4444444444444444444444444444444444444444 160000"
            ),
            "{body}"
        );
        assert!(
            body.contains(
                "+index 5555555555555555555555555555555555555555..6666666666666666666666666666666666666666 160000"
            ),
            "{body}"
        );
        assert!(body.contains("diff --git a/ext b/ext"), "{body}");
    }

    #[test]
    fn replace_gitlink_blocks_replaces_typechange_exactly_once() {
        // Both typechange directions: each half appears exactly once.
        let blob_to_gitlink = "diff --git a/notes b/notes\ndeleted file mode 100644\nindex 1111111111111111111111111111111111111111..0000000000000000000000000000000000000000\n--- a/notes\n+++ /dev/null\n@@ -1 +0,0 @@\n-content\ndiff --git a/notes b/notes\nnew file mode 160000\nindex 0000000000000000000000000000000000000000..2222222222222222222222222222222222222222\n--- /dev/null\n+++ b/notes\n@@ -0,0 +1 @@\n+Subproject commit 2222222222222222222222222222222222222222\n";
        let gitlink_to_blob = "diff --git a/notes b/notes\ndeleted file mode 160000\nindex 2222222222222222222222222222222222222222..0000000000000000000000000000000000000000\n--- a/notes\n+++ /dev/null\n@@ -1 +0,0 @@\n-Subproject commit 2222222222222222222222222222222222222222\ndiff --git a/notes b/notes\nnew file mode 100644\nindex 0000000000000000000000000000000000000000..3333333333333333333333333333333333333333\n--- /dev/null\n+++ b/notes\n@@ -0,0 +1 @@\n+plain\n";
        let mut authority = BTreeSet::new();
        authority.insert(OsString::from("notes"));
        let merged = replace_gitlink_blocks(
            &GitOutput {
                stdout: blob_to_gitlink.as_bytes().to_vec(),
                truncated: false,
                error: None,
            },
            &authority,
            &GitOutput {
                stdout: blob_to_gitlink.as_bytes().to_vec(),
                truncated: false,
                error: None,
            },
            MAX_DIFF_BYTES,
        );
        let body = String::from_utf8(merged.stdout).expect("utf8 merge");
        assert_eq!(body.matches("diff --git a/notes b/notes").count(), 2, "{body}");
        assert_eq!(body.matches("deleted file mode 100644").count(), 1, "{body}");
        assert_eq!(body.matches("new file mode 160000").count(), 1, "{body}");
        assert_eq!(body.matches("-content").count(), 1, "{body}");
        assert_eq!(
            body.matches("+Subproject commit 2222222222222222222222222222222222222222")
                .count(),
            1,
            "{body}"
        );
        let merged = replace_gitlink_blocks(
            &GitOutput {
                stdout: gitlink_to_blob.as_bytes().to_vec(),
                truncated: false,
                error: None,
            },
            &authority,
            &GitOutput {
                stdout: gitlink_to_blob.as_bytes().to_vec(),
                truncated: false,
                error: None,
            },
            MAX_DIFF_BYTES,
        );
        let body = String::from_utf8(merged.stdout).expect("utf8 merge");
        assert_eq!(body.matches("diff --git a/notes b/notes").count(), 2, "{body}");
        assert_eq!(body.matches("deleted file mode 160000").count(), 1, "{body}");
        assert_eq!(body.matches("new file mode 100644").count(), 1, "{body}");
        assert_eq!(
            body.matches("-Subproject commit 2222222222222222222222222222222222222222")
                .count(),
            1,
            "{body}"
        );
        assert_eq!(body.matches("+plain").count(), 1, "{body}");
    }

    #[test]
    fn load_review_snapshot_keeps_gitlink_like_content_while_sibling_gitlink_changes() {
        let (parent, _old_sha) = submodule_repo_with_dirty_checkout();
        // Normal file whose content resembles gitlink metadata.
        write(
            parent.path().join("docs/notes.txt").as_path(),
            "base\nSubproject commit deadbeefdeadbeefdeadbeefdeadbeefdeadbeef\nindex 1111111111111111111111111111111111111111..2222222222222222222222222222222222222222 160000\ndeleted file mode 160000\n",
        );
        git(parent.path(), &["add", "docs/notes.txt"]);
        git(parent.path(), &["commit", "-qm", "notes with gitlink-like content"]);
        write(
            parent.path().join("docs/notes.txt").as_path(),
            "base\nSubproject commit beefdeadbeefdeadbeefdeadbeefdeadbeefdead\nindex 3333333333333333333333333333333333333333..4444444444444444444444444444444444444444 160000\ndeleted file mode 160000\n",
        );

        let (snapshot, paging) =
            load_review_snapshot_capture(parent.path(), ReviewScope::WorkingTree);

        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        let notes = snapshot
            .files
            .iter()
            .find(|file| file.path == "docs/notes.txt")
            .unwrap_or_else(|| {
                panic!(
                    "gitlink-like content file must survive: {:?}",
                    snapshot.files.iter().map(|file| file.path.as_str()).collect::<Vec<_>>()
                )
            });
        let lines: Vec<&str> = notes
            .hunks
            .iter()
            .flat_map(|hunk| hunk.lines.iter())
            .map(|line| line.text.as_str())
            .collect();
        assert!(
            lines.contains(&"Subproject commit deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"),
            "{lines:?}"
        );
        assert!(
            lines.contains(&"Subproject commit beefdeadbeefdeadbeefdeadbeefdeadbeefdead"),
            "{lines:?}"
        );
        assert!(
            lines.contains(&"index 1111111111111111111111111111111111111111..2222222222222222222222222222222222222222 160000"),
            "{lines:?}"
        );
        assert!(
            lines.contains(&"index 3333333333333333333333333333333333333333..4444444444444444444444444444444444444444 160000"),
            "{lines:?}"
        );
        assert!(lines.contains(&"deleted file mode 160000"), "{lines:?}");
        let blobs = FileContextBlobs::load(parent.path(), &paging, notes)
            .expect("gitlink-like notes must load as text");
        assert!(
            !blobs.old.binary && !blobs.new.binary,
            "gitlink-like notes must not be treated as binary"
        );
        assert!(
            !blobs.old.lines.is_empty() || !blobs.new.lines.is_empty(),
            "regular file with Subproject commit among other lines must not be empty gitlink blobs: old={:?} new={:?}",
            blobs.old.lines,
            blobs.new.lines
        );
        assert!(
            blobs
                .new
                .lines
                .iter()
                .chain(blobs.old.lines.iter())
                .any(|line| line.contains("Subproject commit")),
            "loaded notes blobs must keep gitlink-like text: old={:?} new={:?}",
            blobs.old.lines,
            blobs.new.lines
        );
        // The sibling dirty submodule is still expanded.
        assert!(
            snapshot.files.iter().any(|file| file.path == "ext/lib.txt"),
            "{:?}",
            snapshot.files.iter().map(|file| file.path.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn load_review_snapshot_pages_inner_gitlink_like_file_beside_nested_gitlink() {
        let parent = init_repo();
        add_named_submodule(parent.path(), "ext");
        add_named_submodule(&parent.path().join("ext"), "nested");
        git(parent.path(), &["add", "ext"]);
        git(parent.path(), &["commit", "-qm", "record nested gitlink"]);
        write(
            parent.path().join("ext/lib.txt").as_path(),
            "base\nSubproject commit deadbeefdeadbeefdeadbeefdeadbeefdeadbeef\n",
        );
        write(
            parent.path().join("ext/nested/lib.txt").as_path(),
            "base\ndirty\n",
        );
        let (snapshot, paging) =
            load_review_snapshot_capture(parent.path(), ReviewScope::WorkingTree);
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        let paths: Vec<&str> = snapshot
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect();
        assert!(paths.iter().all(|path| *path != "ext"), "outer gitlink must expand: {paths:?}");
        let notes = snapshot
            .files
            .iter()
            .find(|file| file.path == "ext/lib.txt")
            .unwrap_or_else(|| panic!("inner ordinary file missing: {paths:?}"));
        assert_eq!(notes.status, FileStatus::Modified);
        assert!(
            notes.hunks.iter().any(|hunk| hunk.lines.iter().any(|line| {
                line.kind == DiffLineKind::Addition
                    && line.text
                        == "Subproject commit deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
            })),
            "{:?}",
            notes.hunks
        );
        let nested = snapshot
            .files
            .iter()
            .find(|file| file.path == "ext/nested/lib.txt")
            .unwrap_or_else(|| panic!("nested gitlink content missing: {paths:?}"));
        assert_eq!(nested.status, FileStatus::Modified);
        assert!(
            paging
                .working_tree
                .as_ref()
                .is_some_and(|authority| authority.submodule_files.contains_key("ext/lib.txt")),
            "ordinary inner file must page from the submodule sandbox"
        );
        let blobs = FileContextBlobs::load(parent.path(), &paging, notes)
            .expect("inner ordinary blobs");
        assert!(
            blobs
                .new
                .lines
                .iter()
                .any(|line| line.contains("Subproject commit")),
            "{:?}",
            blobs.new.lines
        );
    }

    #[test]
    fn load_review_snapshot_degrades_oversized_inner_untracked_and_keeps_siblings() {
        let parent = init_repo();
        add_named_submodule(parent.path(), "ext");
        write(
            parent.path().join("ext/lib.txt").as_path(),
            "base\ndirty\n",
        );
        write(parent.path().join("ext/small.txt").as_path(), "ok\n");
        let mut oversized = vec![b'x'; MAX_FILE_DIFF_BYTES + 1];
        oversized[MAX_FILE_DIFF_BYTES] = b'\n';
        fs::write(parent.path().join("ext/huge.txt"), &oversized).expect("oversized untracked");
        let (snapshot, paging) =
            load_review_snapshot_capture(parent.path(), ReviewScope::WorkingTree);
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        let paths: Vec<&str> = snapshot
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect();
        assert!(
            paths.iter().all(|path| *path != "ext"),
            "one oversized inner file must not keep the enclosing gitlink: {paths:?}"
        );
        let tracked = snapshot
            .files
            .iter()
            .find(|file| file.path == "ext/lib.txt")
            .unwrap_or_else(|| panic!("tracked sibling missing: {paths:?}"));
        assert_eq!(tracked.status, FileStatus::Modified);
        let small = snapshot
            .files
            .iter()
            .find(|file| file.path == "ext/small.txt")
            .unwrap_or_else(|| panic!("small untracked sibling missing: {paths:?}"));
        assert_eq!(small.status, FileStatus::Added);
        assert_ne!(small.message.as_deref(), Some(PLACEHOLDER_MESSAGE));
        let huge = snapshot
            .files
            .iter()
            .find(|file| file.path == "ext/huge.txt")
            .unwrap_or_else(|| panic!("oversized placeholder missing: {paths:?}"));
        assert_eq!(huge.status, FileStatus::Added);
        assert!(huge.hunks.is_empty(), "{huge:?}");
        assert_eq!(huge.message.as_deref(), Some(PLACEHOLDER_MESSAGE));
        assert!(
            matches!(
                paging
                    .working_tree
                    .as_ref()
                    .and_then(|authority| authority.untracked.get("ext/huge.txt")),
                Some(UntrackedPagingBody::Unpinned)
            ),
            "oversized inner file must pin as Unpinned placeholder"
        );
        assert!(snapshot.truncated, "over-cap intake must mark truncated");
    }

    #[test]
    fn finish_gitlink_splice_empty_truncated_returns_truncated() {
        match finish_gitlink_splice(
            "ext".to_owned(),
            String::new(),
            BTreeMap::new(),
            BTreeMap::new(),
            true,
        ) {
            Ok(GitlinkExpansion::Truncated) => {}
            Ok(GitlinkExpansion::Keep) => {
                panic!("truncated empty splice must not discard truncated")
            }
            Ok(GitlinkExpansion::Splice { .. }) => panic!("empty splice must not invent rows"),
            Err(error) => panic!("{error}"),
        }
        match finish_gitlink_splice(
            "ext".to_owned(),
            String::new(),
            BTreeMap::new(),
            BTreeMap::new(),
            false,
        ) {
            Ok(GitlinkExpansion::Keep) => {}
            Ok(GitlinkExpansion::Truncated) => panic!("clean empty splice must Keep"),
            Ok(GitlinkExpansion::Splice { .. }) => panic!("clean empty splice must Keep"),
            Err(error) => panic!("{error}"),
        }
    }

    #[test]
    fn append_prefixed_untracked_emits_placeholder_rows_for_copy_and_unpinned() {
        let pin = tempfile::tempdir().expect("pin");
        let copy_path = pin.path().join("0.body");
        fs::write(&copy_path, "copied\n").expect("copy body");
        let mut additions = UntrackedAdditions {
            files: Vec::new(),
            patches: Vec::new(),
            pinned: BTreeMap::new(),
            truncated: false,
            total_insertions: 0,
            total_deletions: 0,
        };
        additions
            .pinned
            .insert("copy.txt".to_owned(), UntrackedPagingBody::Copy(copy_path));
        additions
            .pinned
            .insert("gone.bin".to_owned(), UntrackedPagingBody::Unpinned);
        let mut text = String::new();
        let mut bodies = BTreeMap::new();
        append_prefixed_untracked("ext", additions, &mut text, &mut bodies);
        assert!(text.contains("diff --git a/ext/copy.txt b/ext/copy.txt"), "{text}");
        assert!(text.contains("diff --git a/ext/gone.bin b/ext/gone.bin"), "{text}");
        assert!(matches!(
            bodies.get("ext/copy.txt"),
            Some(UntrackedPagingBody::Copy(_))
        ));
        assert!(matches!(
            bodies.get("ext/gone.bin"),
            Some(UntrackedPagingBody::Unpinned)
        ));
        let parsed = parse_unified_diff(&text);
        assert!(parsed.iter().any(|file| file.path == "ext/copy.txt"), "{parsed:?}");
        assert!(parsed.iter().any(|file| file.path == "ext/gone.bin"), "{parsed:?}");
    }

    #[test]
    fn ingest_untracked_paths_pins_unique_bodies_across_parent_child_and_nested() {
        let parent = init_repo();
        add_named_submodule(parent.path(), "ext");
        add_named_submodule(&parent.path().join("ext"), "nested");
        write(parent.path().join("scratch.txt").as_path(), "parent-body\n");
        write(
            parent.path().join("ext/lib.txt").as_path(),
            "base\ndirty\n",
        );
        write(parent.path().join("ext/child.txt").as_path(), "child-body\n");
        write(
            parent.path().join("ext/nested/lib.txt").as_path(),
            "base\ndirty\n",
        );
        write(
            parent.path().join("ext/nested/inner.txt").as_path(),
            "nested-body\n",
        );
        let pin = tempfile::tempdir().expect("shared pin dir");
        let mut pin_index = 0usize;
        let taken = BTreeSet::new();
        let parent_add = ingest_untracked_paths(
            parent.path(),
            &[b"scratch.txt".as_slice()],
            &taken,
            pin.path(),
            0,
            MAX_UNTRACKED_PINNED_BYTES,
            &mut pin_index,
        )
        .expect("parent pin");
        let child_add = ingest_untracked_paths(
            &parent.path().join("ext"),
            &[b"child.txt".as_slice()],
            &taken,
            pin.path(),
            0,
            MAX_UNTRACKED_PINNED_BYTES,
            &mut pin_index,
        )
        .expect("child pin");
        let nested_add = ingest_untracked_paths(
            &parent.path().join("ext/nested"),
            &[b"inner.txt".as_slice()],
            &taken,
            pin.path(),
            0,
            MAX_UNTRACKED_PINNED_BYTES,
            &mut pin_index,
        )
        .expect("nested pin");
        let parent_copy = match parent_add.pinned.get("scratch.txt") {
            Some(UntrackedPagingBody::Copy(path)) => path.clone(),
            other => panic!("parent Copy missing: {other:?}"),
        };
        let child_copy = match child_add.pinned.get("child.txt") {
            Some(UntrackedPagingBody::Copy(path)) => path.clone(),
            other => panic!("child Copy missing: {other:?}"),
        };
        let nested_copy = match nested_add.pinned.get("inner.txt") {
            Some(UntrackedPagingBody::Copy(path)) => path.clone(),
            other => panic!("nested Copy missing: {other:?}"),
        };
        assert_ne!(parent_copy, child_copy);
        assert_ne!(parent_copy, nested_copy);
        assert_ne!(child_copy, nested_copy);
        assert_eq!(fs::read(&parent_copy).expect("parent body"), b"parent-body\n");
        assert_eq!(fs::read(&child_copy).expect("child body"), b"child-body\n");
        assert_eq!(fs::read(&nested_copy).expect("nested body"), b"nested-body\n");
        assert_eq!(pin_index, 3);
    }

    #[test]
    fn isolated_sandbox_discover_ignores_plain_head_file_in_workdir() {
        let repo = init_repo();
        let sub = repo.path().join("src");
        fs::create_dir_all(&sub).expect("subdir");
        write(sub.join("HEAD").as_path(), "not a git directory\n");
        let sandbox = IsolatedGitSandbox::discover(&sub).expect("enclosing repository");
        let work_tree = fs::canonicalize(sandbox.work_tree()).expect("work tree");
        let root = fs::canonicalize(repo.path()).expect("repo root");
        assert_eq!(work_tree, root);
    }

    fn gitlink_only_diff_file(path: &str) -> DiffFile {
        DiffFile {
            path: path.to_owned(),
            previous_path: None,
            status: FileStatus::Modified,
            binary: false,
            insertions: 1,
            deletions: 1,
            hunks: vec![DiffHunk {
                header: "@@ -1 +1 @@".to_owned(),
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 1,
                lines: vec![
                    DiffLine {
                        kind: DiffLineKind::Deletion,
                        old_no: Some(1),
                        new_no: None,
                        text: "Subproject commit 1234567890abcdef1234567890abcdef12345678"
                            .to_owned(),
                    },
                    DiffLine {
                        kind: DiffLineKind::Addition,
                        old_no: None,
                        new_no: Some(1),
                        text: "Subproject commit fedcba091234567890abcdef1234567890abcdef-dirty"
                            .to_owned(),
                    },
                ],
            }],
            truncated: false,
            message: None,
        }
    }

    fn mixed_subproject_text_diff_file(path: &str) -> DiffFile {
        DiffFile {
            path: path.to_owned(),
            previous_path: None,
            status: FileStatus::Modified,
            binary: false,
            insertions: 1,
            deletions: 1,
            hunks: vec![DiffHunk {
                header: "@@ -1,3 +1,3 @@".to_owned(),
                old_start: 1,
                old_count: 3,
                new_start: 1,
                new_count: 3,
                lines: vec![
                    DiffLine {
                        kind: DiffLineKind::Context,
                        old_no: Some(1),
                        new_no: Some(1),
                        text: "keep".to_owned(),
                    },
                    DiffLine {
                        kind: DiffLineKind::Deletion,
                        old_no: Some(2),
                        new_no: None,
                        text: "Subproject commit deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
                            .to_owned(),
                    },
                    DiffLine {
                        kind: DiffLineKind::Addition,
                        old_no: None,
                        new_no: Some(2),
                        text: "Subproject commit beefdeadbeefdeadbeefdeadbeefdeadbeefdead"
                            .to_owned(),
                    },
                    DiffLine {
                        kind: DiffLineKind::Context,
                        old_no: Some(3),
                        new_no: Some(3),
                        text: "index 1111111111111111111111111111111111111111..2222222222222222222222222222222222222222 160000"
                            .to_owned(),
                    },
                ],
            }],
            truncated: false,
            message: None,
        }
    }

    fn expect_empty_gitlink_blobs(result: Result<FileContextBlobs, String>) {
        let blobs = match result {
            Ok(blobs) => blobs,
            Err(error) => panic!("gitlink context must not git-show: {error}"),
        };
        assert!(
            !blobs.old.binary && !blobs.new.binary,
            "gitlink context is empty text, not binary: {blobs:?}"
        );
        assert!(
            blobs.old.lines.is_empty() && blobs.new.lines.is_empty(),
            "gitlink blobs must be empty: {blobs:?}"
        );
    }

    fn expect_loaded_subproject_text(result: Result<FileContextBlobs, String>) {
        let blobs = result.expect("regular file with Subproject commit among other lines must load");
        assert!(
            !blobs.old.binary && !blobs.new.binary,
            "regular file must not be treated as binary: {blobs:?}"
        );
        assert!(
            !blobs.old.lines.is_empty() || !blobs.new.lines.is_empty(),
            "regular file with Subproject commit among other lines must not be empty gitlink blobs: old={:?} new={:?}",
            blobs.old.lines,
            blobs.new.lines
        );
        assert!(
            blobs
                .old
                .lines
                .iter()
                .chain(blobs.new.lines.iter())
                .any(|line| line.contains("Subproject commit")),
            "loaded blobs must keep gitlink-like text: old={:?} new={:?}",
            blobs.old.lines,
            blobs.new.lines
        );
    }

    #[test]
    fn file_context_blobs_skip_git_show_for_empty_gitlink_hunks() {
        let repo = init_repo();
        add_named_submodule(repo.path(), "ext");
        git(repo.path(), &["branch", "review-base"]);
        git(repo.path(), &["branch", "review-target"]);
        let mut file = gitlink_only_diff_file("ext");
        file.hunks.clear();
        file.truncated = true;

        let (wt_snapshot, wt_paging) =
            load_review_snapshot_capture(repo.path(), ReviewScope::WorkingTree);
        assert!(wt_snapshot.error.is_none(), "{:?}", wt_snapshot.error);
        expect_empty_gitlink_blobs(FileContextBlobs::load(repo.path(), &wt_paging, &file));
        let working_tree = wt_paging
            .working_tree
            .as_ref()
            .expect("working-tree paging");
        expect_empty_gitlink_blobs(load_working_tree_blobs(working_tree, &file));

        let scope = ReviewScope::Revisions {
            from: "review-base".to_owned(),
            to: "review-target".to_owned(),
        };
        let (rev_snapshot, rev_paging) = load_review_snapshot_capture(repo.path(), scope);
        assert!(rev_snapshot.error.is_none(), "{:?}", rev_snapshot.error);
        expect_empty_gitlink_blobs(FileContextBlobs::load(repo.path(), &rev_paging, &file));
        let revisions = rev_paging.revisions.as_ref().expect("revision paging");
        expect_empty_gitlink_blobs(load_revision_blobs(
            repo.path(),
            &rev_paging,
            revisions,
            &file,
        ));

        let mut regular = gitlink_only_diff_file("README.md");
        regular.truncated = true;
        let blobs = FileContextBlobs::load(repo.path(), &wt_paging, &regular)
            .expect("protocol-looking text does not override regular file mode");
        assert!(!blobs.old.lines.is_empty());
        assert_eq!(blobs.old.lines, blobs.new.lines);
    }

    #[test]
    fn tree_listing_record_is_gitlink_reads_mode_without_path_match() {
        assert!(tree_listing_record_is_gitlink(
            b"160000 commit 0123456789abcdef0123456789abcdef01234567\text"
        ));
        assert!(tree_listing_record_is_gitlink(
            b"160000 0123456789abcdef0123456789abcdef01234567 0\text"
        ));
        assert!(tree_listing_record_is_gitlink(
            b"160000 commit 0123456789abcdef0123456789abcdef01234567 ext"
        ));
        assert!(!tree_listing_record_is_gitlink(
            b"100644 blob 0123456789abcdef0123456789abcdef01234567\text"
        ));
        assert!(!tree_listing_record_is_gitlink(b""));
    }

    #[test]
    fn file_context_blobs_skip_git_show_for_gitlink_protocol_hunks() {
        let repo = init_repo();
        add_named_submodule(repo.path(), "ext");
        git(repo.path(), &["branch", "review-base"]);
        git(repo.path(), &["branch", "review-target"]);
        let file = gitlink_only_diff_file("ext");
        assert!(is_gitlink_diff_file(&file), "{file:?}");

        let (wt_snapshot, wt_paging) =
            load_review_snapshot_capture(repo.path(), ReviewScope::WorkingTree);
        assert!(wt_snapshot.error.is_none(), "{:?}", wt_snapshot.error);
        expect_empty_gitlink_blobs(FileContextBlobs::load(repo.path(), &wt_paging, &file));
        let working_tree = wt_paging
            .working_tree
            .as_ref()
            .expect("working-tree paging");
        expect_empty_gitlink_blobs(load_working_tree_blobs(working_tree, &file));

        let scope = ReviewScope::Revisions {
            from: "review-base".to_owned(),
            to: "review-target".to_owned(),
        };
        let (rev_snapshot, rev_paging) = load_review_snapshot_capture(repo.path(), scope);
        assert!(rev_snapshot.error.is_none(), "{:?}", rev_snapshot.error);
        expect_empty_gitlink_blobs(FileContextBlobs::load(repo.path(), &rev_paging, &file));
        let revisions = rev_paging.revisions.as_ref().expect("revision paging");
        expect_empty_gitlink_blobs(load_revision_blobs(
            repo.path(),
            &rev_paging,
            revisions,
            &file,
        ));
    }

    #[test]
    fn file_context_probe_skips_revision_gitlink_when_submodule_objects_are_absent() {
        let (parent, from, to) = submodule_repo_with_revision_inner_diff();
        fs::remove_dir_all(parent.path().join("ext")).expect("remove submodule checkout");
        fs::remove_dir_all(parent.path().join(".git/modules/ext"))
            .expect("remove submodule objects");
        let spec = format!("{from}:ext");
        let show = Command::new("git")
            .args([
                "--no-pager",
                "-c",
                "color.ui=false",
                "show",
                "--no-textconv",
                "--no-ext-diff",
                "--end-of-options",
                spec.as_str(),
            ])
            .current_dir(parent.path())
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                parent.path().join("absent-global-git-config"),
            )
            .env("LC_ALL", "C")
            .output()
            .expect("git show gitlink");
        assert!(
            !show.status.success(),
            "parent git show of a gitlink must fail"
        );
        let stderr = String::from_utf8_lossy(&show.stderr);
        assert!(
            stderr.contains("unable to read") || stderr.contains("bad object"),
            "context probe used to toast git's missing gitlink object: {stderr}"
        );

        let (snapshot, paging) = load_review_snapshot_capture(
            parent.path(),
            ReviewScope::Revisions { from, to },
        );
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        let file = snapshot
            .files
            .iter()
            .find(|file| file.path == "ext")
            .unwrap_or_else(|| panic!("kept gitlink missing: {:?}", snapshot.files));
        assert!(
            snapshot.files.iter().all(|file| file.path != "ext/lib.txt"),
            "absent submodule objects must keep the gitlink row: {:?}",
            snapshot.files.iter().map(|file| file.path.as_str()).collect::<Vec<_>>()
        );
        expect_empty_gitlink_blobs(FileContextBlobs::load(parent.path(), &paging, file));
        let revisions = paging.revisions.as_ref().expect("revision paging");
        expect_empty_gitlink_blobs(load_revision_blobs(
            parent.path(),
            &paging,
            revisions,
            file,
        ));
    }

    #[test]
    fn file_context_blobs_load_regular_file_that_mentions_subproject_commit() {
        let repo = init_repo();
        write(
            repo.path().join("notes.txt").as_path(),
            "keep\nSubproject commit deadbeefdeadbeefdeadbeefdeadbeefdeadbeef\nindex 1111111111111111111111111111111111111111..2222222222222222222222222222222222222222 160000\n",
        );
        git(repo.path(), &["add", "notes.txt"]);
        git(repo.path(), &["commit", "-m", "notes with gitlink-like content"]);
        git(repo.path(), &["branch", "review-base"]);
        git(repo.path(), &["branch", "review-target"]);
        write(
            repo.path().join("notes.txt").as_path(),
            "keep\nSubproject commit beefdeadbeefdeadbeefdeadbeefdeadbeefdead\nindex 1111111111111111111111111111111111111111..2222222222222222222222222222222222222222 160000\n",
        );
        let file = mixed_subproject_text_diff_file("notes.txt");
        assert!(!is_gitlink_diff_file(&file));

        let (wt_snapshot, wt_paging) =
            load_review_snapshot_capture(repo.path(), ReviewScope::WorkingTree);
        assert!(wt_snapshot.error.is_none(), "{:?}", wt_snapshot.error);
        expect_loaded_subproject_text(FileContextBlobs::load(repo.path(), &wt_paging, &file));
        let working_tree = wt_paging
            .working_tree
            .as_ref()
            .expect("working-tree paging");
        expect_loaded_subproject_text(load_working_tree_blobs(working_tree, &file));

        let scope = ReviewScope::Revisions {
            from: "review-base".to_owned(),
            to: "review-target".to_owned(),
        };
        let (rev_snapshot, rev_paging) = load_review_snapshot_capture(repo.path(), scope);
        assert!(rev_snapshot.error.is_none(), "{:?}", rev_snapshot.error);
        expect_loaded_subproject_text(FileContextBlobs::load(repo.path(), &rev_paging, &file));
        let revisions = rev_paging.revisions.as_ref().expect("revision paging");
        expect_loaded_subproject_text(load_revision_blobs(
            repo.path(),
            &rev_paging,
            revisions,
            &file,
        ));
    }

    #[test]
    fn load_review_snapshot_replaces_blob_gitlink_typechange_once() {
        let parent = init_repo();
        let sub_dir = TempDir::new().expect("submodule tempdir");
        git(sub_dir.path(), &["init"]);
        write(sub_dir.path().join("f.txt").as_path(), "sub\n");
        git(sub_dir.path(), &["add", "f.txt"]);
        git(sub_dir.path(), &["commit", "-qm", "submodule initial"]);
        // Replace the tracked blob with a submodule at the same path.
        git(parent.path(), &["rm", "-q", "README.md"]);
        git(
            parent.path(),
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                sub_dir.path().to_str().expect("utf8 submodule path"),
                "README.md",
            ],
        );
        git(parent.path(), &["add", "README.md"]);

        let snapshot = load_review_snapshot(parent.path());

        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        let entries: Vec<&DiffFile> = snapshot
            .files
            .iter()
            .filter(|file| file.path == "README.md")
            .collect();
        // One blob deletion and one gitlink addition, never a duplicated block.
        assert_eq!(entries.len(), 2, "{:?}", snapshot.files);
        assert_eq!(
            entries
                .iter()
                .filter(|file| file.status == FileStatus::Deleted)
                .count(),
            1,
            "{:?}",
            snapshot.files
        );
        assert_eq!(
            entries
                .iter()
                .filter(|file| file.status == FileStatus::Added)
                .count(),
            1,
            "{:?}",
            snapshot.files
        );
        let added = entries
            .iter()
            .find(|file| file.status == FileStatus::Added)
            .expect("gitlink addition half");
        assert!(
            added.hunks.iter().flat_map(|hunk| hunk.lines.iter()).any(|line| {
                line.kind == DiffLineKind::Addition
                    && line.text.starts_with("Subproject commit ")
            }),
            "{:?}",
            added.hunks
        );
    }

    #[test]
    fn load_review_snapshot_tracks_pure_inner_rename() {
        let parent = submodule_repo_with_renamed_file();

        let snapshot = load_review_snapshot(parent.path());

        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        let renamed = snapshot
            .files
            .iter()
            .find(|file| file.path == "ext/new.txt")
            .expect("renamed inner entry");
        assert_eq!(renamed.status, FileStatus::Renamed);
        assert_eq!(renamed.previous_path.as_deref(), Some("ext/old.txt"));
        assert!(
            snapshot
                .files
                .iter()
                .all(|file| file.path != "ext" && file.path != "ext/old.txt"),
            "gitlink and rename source must be replaced: {:?}",
            snapshot.files.iter().map(|file| file.path.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn load_review_snapshot_tracks_content_changing_inner_rename() {
        let parent = submodule_repo_with_renamed_file();
        write(
            parent.path().join("ext/new.txt").as_path(),
            &format!("{INNER_RENAME_BASE_TEXT}changed\n"),
        );

        let snapshot = load_review_snapshot(parent.path());

        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        let renamed = snapshot
            .files
            .iter()
            .find(|file| file.path == "ext/new.txt")
            .expect("renamed inner entry");
        assert_eq!(renamed.status, FileStatus::Renamed);
        assert_eq!(renamed.previous_path.as_deref(), Some("ext/old.txt"));
        assert!(
            renamed
                .hunks
                .iter()
                .any(|hunk| hunk
                    .lines
                    .iter()
                    .any(|line| line.kind == DiffLineKind::Addition && line.text == "changed")),
            "the renamed inner file must carry its hunks: {:?}",
            renamed.hunks
        );
    }

    /// Base content the fixture commits before `git mv`. Appending one line
    /// to this keeps the inner diff's similarity above git's 50% rename
    /// threshold: a content-changing inner rename must still be reported by
    /// git as a rename, not as a delete/add pair.
    const INNER_RENAME_BASE_TEXT: &str = "r1\nr2\nr3\nr4\nr5\nr6\nr7\nr8\nr9\nr10\n";

    fn submodule_repo_with_renamed_file() -> TempDir {
        let parent = init_repo();
        let sub_dir = TempDir::new().expect("submodule tempdir");
        git(sub_dir.path(), &["init"]);
        write(sub_dir.path().join("old.txt").as_path(), INNER_RENAME_BASE_TEXT);
        git(sub_dir.path(), &["add", "old.txt"]);
        git(sub_dir.path(), &["commit", "-qm", "submodule initial"]);
        let sub_path = sub_dir.path().canonicalize().expect("canonical submodule");
        git(
            parent.path(),
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                sub_path.to_str().expect("utf8 submodule path"),
                "ext",
            ],
        );
        git(parent.path(), &["commit", "-qm", "add submodule"]);
        git(&parent.path().join("ext"), &["mv", "old.txt", "new.txt"]);
        parent
    }



    #[cfg(unix)]
    #[test]
    fn load_review_snapshot_bounded_over_large_untracked_build_trees() {
        // Untracked non-ignored files ARE reviewed (as Added), but only
        // through bounded, per-file captures: the 1 TiB sparse canary must
        // become an on-demand placeholder (never an unbounded hash), and the
        // unreadable 0o000 canary must be skipped without failing the
        // snapshot. Nothing is ever staged: the real index stays
        // byte-identical.
        let repo = init_repo();
        write(repo.path().join("README.md").as_path(), "base\nchanged\n");
        let build = repo.path().join("build");
        fs::create_dir(&build).expect("mkdir build");
        let secret = build.join("secret.txt");
        fs::write(&secret, "never read\n").expect("write canary");
        let big = build.join("big.bin");
        let mut permissions = fs::metadata(&secret)
            .expect("canary metadata")
            .permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o000);
        fs::set_permissions(&secret, permissions).expect("chmod canary");
        let big_file = fs::File::create(&big).expect("create sparse canary");
        big_file.set_len(1_u64 << 40).expect("sparse canary size");
        let index = repo.path().join(".git/index");
        let index_before = fs::read(&index).expect("index before");

        let snapshot = load_review_snapshot(repo.path());

        assert!(
            snapshot.error.is_none(),
            "bounded untracked capture failed: {:?}",
            snapshot.error
        );
        let readme = snapshot
            .files
            .iter()
            .find(|file| file.path == "README.md")
            .expect("tracked change present");
        assert_eq!(readme.status, FileStatus::Modified);
        let big_entry = snapshot
            .files
            .iter()
            .find(|file| file.path == "build/big.bin")
            .expect("sparse untracked canary captured as Added");
        assert_eq!(big_entry.status, FileStatus::Added);
        assert!(
            big_entry.hunks.is_empty() && !big_entry.truncated && big_entry.binary,
            "oversized binary canary must be an on-demand binary placeholder: {:?}",
            big_entry
        );
        assert_eq!(big_entry.body_state(), "binary");
        assert_eq!(big_entry.insertions, 0);
        assert_eq!(
            snapshot.total_insertions(),
            1,
            "README.md's single insertion is the only line count"
        );
        assert_eq!(big_entry.message.as_deref(), Some(PLACEHOLDER_MESSAGE));
        assert!(
            !snapshot
                .files
                .iter()
                .any(|file| file.path == "build/secret.txt"),
            "unreadable untracked file must be skipped, not fail the review: {:?}",
            snapshot
                .files
                .iter()
                .map(|file| (file.path.as_str(), file.status, file.message.clone()))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            fs::metadata(&big).expect("canary metadata").len(),
            1_u64 << 40,
            "sparse canary was modified"
        );
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
            &empty_numstat(),
            None,
            None,
        );
        assert!(snapshot.files.is_empty());
        assert_eq!(snapshot.error.as_deref(), Some("fatal: later path failed"));
    }

    #[test]
    fn display_path_encoding_distinguishes_octal_text_from_invalid_bytes() {
        assert_eq!(encode_path_bytes(br"bad\377name"), r"bad\\377name");
        assert_eq!(decode_path_bytes(r"bad\\377name"), br"bad\377name");
        assert_eq!(decode_path_bytes(r"bad\377name"), b"bad\xffname");
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
        let error = run_git_bounded(
            directory.path(),
            &["-c", alias.as_str(), "hold"],
            MAX_GIT_METADATA_BYTES,
            None,
        )
        .expect_err("retained pipe must fail closed");
        assert!(error.contains("remained open"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(50));
        assert!(!directory.path().join("child-finished").exists());
    }

    #[test]
    fn receive_reader_accepts_result_delayed_beyond_one_second() {
        // The reader thread can be descheduled past the old single-shot
        // one-second wait even though git has already exited. A result that
        // arrives around two seconds after the wait starts must still win as
        // long as it lands before the total reader deadline.
        let (sender, receiver) = mpsc::channel();
        let started = Instant::now();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(2100));
            let _ = sender.send(Ok((b"delayed".to_vec(), false)));
        });
        let deadline = Instant::now() + READER_RESULT_DEADLINE;
        let result =
            receive_reader(receiver, "stdout", deadline).expect("delayed result must be accepted");
        let elapsed = started.elapsed();
        assert_eq!(result, (b"delayed".to_vec(), false));
        assert!(
            elapsed >= Duration::from_secs(2),
            "result arrived too early to exercise the delayed path: {elapsed:?}"
        );
        assert!(
            elapsed < READER_RESULT_DEADLINE,
            "delayed result must arrive before the total deadline: {elapsed:?}"
        );
    }

    #[test]
    fn receive_reader_fails_by_deadline_with_stream_label() {
        // A reader that never reports must fail by the fixed ceiling, not
        // loop forever, and the error must name the stream that was stuck.
        let (sender, receiver) = mpsc::channel();
        // Keep the sender alive so the receiver never disconnects: the wait
        // has to be resolved by the total deadline, not by channel teardown.
        let _held = sender;
        let started = Instant::now();
        let deadline = Instant::now() + READER_RESULT_DEADLINE;
        let error = receive_reader(receiver, "stderr", deadline)
            .expect_err("never-reporting reader must fail");
        let elapsed = started.elapsed();
        assert_eq!(error, "git stderr remained open after exit");
        assert!(
            elapsed >= READER_RESULT_DEADLINE,
            "must not fail before the fixed ceiling: {elapsed:?}"
        );
    }

    #[test]
    fn receive_reader_reports_disconnected_reader_stopped() {
        // A reader thread that died without delivering (channel closed) keeps
        // its existing distinct error instead of being misread as a timeout.
        let (sender, receiver) = mpsc::channel();
        drop(sender);
        let deadline = Instant::now() + READER_RESULT_DEADLINE;
        let error = receive_reader(receiver, "stdout", deadline)
            .expect_err("disconnected reader must fail");
        assert_eq!(error, "git stdout reader stopped");
    }

    #[test]
    fn receive_reader_second_stream_gets_only_remaining_shared_deadline() {
        // Regression: the two sequential receives used to each create their
        // own fresh ceiling, so a stdout result landing near the deadline
        // still left stderr a full second ceiling and total collection could
        // approach two deadlines. With one absolute deadline created after
        // git exit and shared by both streams, the second stream must fail
        // immediately (with its own label) once the deadline is exhausted.
        // A short explicit deadline keeps the test fast and deterministic
        // while production uses the 3s READER_RESULT_DEADLINE constant.
        let (stdout_sender, stdout_rx) = mpsc::channel();
        let (stderr_sender, stderr_rx) = mpsc::channel();
        // Keep the stderr sender alive so the second wait is resolved by the
        // shared deadline, not by channel teardown.
        let _held = stderr_sender;
        let deadline = Instant::now() + Duration::from_millis(300);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(250));
            let _ = stdout_sender.send(Ok((b"stdout".to_vec(), false)));
        });

        let started = Instant::now();
        let stdout = receive_reader(stdout_rx, "stdout", deadline)
            .expect("stdout result near the shared deadline must be accepted");
        assert_eq!(stdout, (b"stdout".to_vec(), false));
        let stderr_started = Instant::now();
        let stderr_error = receive_reader(stderr_rx, "stderr", deadline)
            .expect_err("stderr must fail once the shared deadline is exhausted");
        let stderr_elapsed = stderr_started.elapsed();
        let elapsed = started.elapsed();
        assert_eq!(stderr_error, "git stderr remained open after exit");
        assert!(
            elapsed >= Duration::from_millis(250),
            "stdout must have waited until near the shared deadline: {elapsed:?}"
        );
        assert!(
            stderr_elapsed < Duration::from_millis(100),
            "second stream must fail immediately, not get a fresh ceiling: {stderr_elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(1),
            "total collection must stay within one shared ceiling: {elapsed:?}"
        );
    }

    #[test]
    fn load_review_snapshot_reports_non_repo_and_unborn_head() {
        let tmp = TempDir::new().expect("temp");
        assert!(
            load_review_snapshot(tmp.path())
                .error
                .expect("error")
                .contains("not a git")
        );
        let unborn = TempDir::new().expect("temp");
        git(unborn.path(), &["init"]);
        assert!(
            load_review_snapshot(unborn.path())
                .error
                .expect("unborn error")
                .contains("unborn")
        );
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
            "tab\tback\\\\slash name"
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
            total_insertions: 0,
            total_deletions: 0,
            summary_complete: true,
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

    fn sample_hunk(old_start: u32, old_count: u32, new_start: u32, new_count: u32) -> DiffHunk {
        DiffHunk {
            header: format!("@@ -{old_start},{old_count} +{new_start},{new_count} @@"),
            old_start,
            old_count,
            new_start,
            new_count,
            lines: Vec::new(),
        }
    }

    #[test]
    fn review_diff_gaps_exposes_code_above_a_hunk_starting_at_line_40() {
        let hunks = [sample_hunk(40, 8, 40, 8)];
        let gaps = review_diff_gaps(&hunks, 0, 0);
        assert_eq!(gaps.len(), 1, "{gaps:?}");
        assert_eq!(gaps[0].new_start, 1);
        assert_eq!(gaps[0].new_count, 39);
        assert_eq!(gaps[0].old_start, 1);
        assert_eq!(gaps[0].old_count, 39);
        let after = review_diff_gaps(&hunks, 80, 80);
        assert_eq!(after.len(), 2, "{after:?}");
        assert_eq!(after[0].new_count, 39);
        assert_eq!(after[1].new_start, 48);
        assert_eq!(after[1].new_count, 33);
    }

    #[test]
    fn gap_expand_window_reads_upward_from_the_hunk() {
        let gap = DiffGap {
            old_start: 1,
            old_count: 39,
            new_start: 1,
            new_count: 39,
        };
        let mut state = GapExpandState::new(gap);
        let window = gap_expand_window(&state.remaining(), GapExpandFrom::End, 20)
            .expect("window");
        assert_eq!(window, (20, 20, 20));
        state.record(GapExpandFrom::End, 20);
        let again = gap_expand_window(&state.remaining(), GapExpandFrom::End, 20)
            .expect("second window");
        assert_eq!(again, (1, 1, 19));
        state.record(GapExpandFrom::End, 19);
        assert!(state.is_closed());
        assert!(gap_expand_window(&state.remaining(), GapExpandFrom::End, 20).is_none());
    }

    #[test]
    fn review_diff_gaps_between_hunks_and_one_sided_files() {
        let hunks = [sample_hunk(10, 3, 10, 3), sample_hunk(40, 3, 40, 3)];
        let unknown_eof = review_diff_gaps(&hunks, 0, 0);
        assert_eq!(unknown_eof.len(), 2, "{unknown_eof:?}");
        assert_eq!(unknown_eof[0].new_start, 1);
        assert_eq!(unknown_eof[0].new_count, 9);
        assert_eq!(unknown_eof[1].new_start, 13);
        assert_eq!(unknown_eof[1].new_count, 27);
        let known = review_diff_gaps(&hunks, 80, 80);
        assert_eq!(known.len(), 3, "{known:?}");
        assert_eq!(known[2].new_start, 43);
        assert_eq!(known[2].new_count, 38);

        let deleted = [sample_hunk(40, 8, 0, 0)];
        let deleted_gaps = review_diff_gaps(&deleted, 80, 0);
        assert_eq!(deleted_gaps.len(), 2, "{deleted_gaps:?}");
        assert_eq!(deleted_gaps[0].old_start, 1);
        assert_eq!(deleted_gaps[0].old_count, 39);
        assert_eq!(deleted_gaps[0].new_count, 0);
        assert_eq!(deleted_gaps[1].old_start, 48);
        assert_eq!(deleted_gaps[1].old_count, 33);
        assert_eq!(deleted_gaps[1].new_count, 0);

        let added = [sample_hunk(0, 0, 20, 8)];
        let added_gaps = review_diff_gaps(&added, 0, 50);
        assert_eq!(added_gaps.len(), 2, "{added_gaps:?}");
        assert_eq!(added_gaps[0].old_count, 0);
        assert_eq!(added_gaps[0].new_start, 1);
        assert_eq!(added_gaps[0].new_count, 19);
        assert_eq!(added_gaps[1].old_count, 0);
        assert_eq!(added_gaps[1].new_start, 28);
        assert_eq!(added_gaps[1].new_count, 23);
    }

    #[test]
    fn gap_expand_record_clamps_to_remaining_span() {
        let gap = DiffGap {
            old_start: 1,
            old_count: 39,
            new_start: 1,
            new_count: 39,
        };
        let mut state = GapExpandState::new(gap);
        state.record(GapExpandFrom::End, 100);
        assert_eq!(state.from_end, 39);
        assert!(state.is_closed());
        state.record(GapExpandFrom::Start, 5);
        assert_eq!(state.from_start, 0);
        assert_eq!(state.from_end, 39);
    }

    #[test]
    fn file_context_requires_snapshot_paging() {
        let file = diff_file_at("wide.rs", None, FileStatus::Modified);
        let error = FileContextBlobs::load(
            Path::new("."),
            &ReviewPagingState::default(),
            &file,
        )
        .expect_err("unbound paging");
        assert!(
            error.contains("paging state is unavailable"),
            "{error}"
        );
    }

    #[test]
    fn file_context_slice_does_not_change_hunk_identity() {
        let files = parse_unified_diff(
            "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -40,2 +40,2 @@\n context\n-old\n+new\n",
        );
        let hunk = &files[0].hunks[0];
        let before = HunkIdentity::new("snap", "a.rs", hunk);
        let blobs = FileContextBlobs {
            old: FileBlob {
                lines: (1..=50).map(|n| format!("old-{n}")).collect(),
                binary: false,
                truncated: false,
            },
            new: FileBlob {
                lines: (1..=50).map(|n| format!("new-{n}")).collect(),
                binary: false,
                truncated: false,
            },
        };
        let slice = blobs.slice(20, 20, 20).expect("slice");
        assert_eq!(slice.lines.len(), 20);
        assert_eq!(slice.lines[0].new_no, Some(20));
        assert_eq!(slice.lines[0].text, "new-20");
        assert!(slice.lines.iter().all(|line| line.kind == DiffLineKind::Context));
        let after = HunkIdentity::new("snap", "a.rs", hunk);
        assert_eq!(before.content_hash, after.content_hash);
        assert_eq!(before, after);
    }

    #[test]
    fn file_context_reads_pinned_snapshot_not_later_worktree_edits() {
        let repo = init_repo();
        let mut body = String::new();
        for n in 1..=50 {
            body.push_str(&format!("line-{n}\n"));
        }
        write(repo.path().join("wide.rs").as_path(), &body);
        git(repo.path(), &["add", "wide.rs"]);
        git(repo.path(), &["commit", "-m", "add wide"]);
        let mut edited = body.clone();
        edited.push_str("changed-tail\n");
        write(repo.path().join("wide.rs").as_path(), &edited.replace("line-40\n", "line-40-edit\n"));
        let (snapshot, paging) =
            load_review_snapshot_capture(repo.path(), ReviewScope::WorkingTree);
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        let file = snapshot
            .files
            .iter()
            .find(|file| file.path == "wide.rs")
            .expect("wide.rs in snapshot");
        write(
            repo.path().join("wide.rs").as_path(),
            "LIVE-WORKTREE-SHOULD-NOT-APPEAR\n",
        );
        let blobs = FileContextBlobs::load(repo.path(), &paging, file)
            .expect("load blobs");
        let slice = blobs.slice(1, 1, 20).expect("slice");
        assert!(
            slice.lines.iter().any(|line| line.text == "line-1"),
            "{:?}",
            slice.lines
        );
        assert!(
            !slice
                .lines
                .iter()
                .any(|line| line.text.contains("LIVE-WORKTREE")),
            "{:?}",
            slice.lines
        );
        assert_eq!(blobs.new.lines.get(39).map(String::as_str), Some("line-40-edit"));
        let hash_before = HunkIdentity::new(&snapshot.snapshot_id, &file.path, &file.hunks[0]);
        let _ = blobs.slice(1, 1, 20);
        let hash_after = HunkIdentity::new(&snapshot.snapshot_id, &file.path, &file.hunks[0]);
        assert_eq!(hash_before.content_hash, hash_after.content_hash);
    }

    #[test]
    fn file_context_rejects_binary() {
        let blobs = FileContextBlobs {
            old: FileBlob {
                lines: Vec::new(),
                binary: true,
                truncated: false,
            },
            new: FileBlob::empty(),
        };
        let error = blobs.slice(1, 1, 20).expect_err("binary");
        assert!(error.contains("binary"), "{error}");
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

    fn empty_numstat() -> GitOutput {
        GitOutput {
            stdout: Vec::new(),
            truncated: false,
            error: None,
        }
    }

    #[test]
    fn slice_page_rejects_empty_snapshot_id_and_out_of_range_cursor() {
        let diff = FileDiff {
            path: "a.rs".into(),
            previous_path: None,
            status: FileStatus::Modified,
            binary: false,
            hunks: vec![DiffHunk {
                header: "@@ -1,2 +1,2 @@".into(),
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 1,
                lines: vec![
                    DiffLine {
                        kind: DiffLineKind::Context,
                        old_no: Some(1),
                        new_no: Some(1),
                        text: "keep".into(),
                    },
                    DiffLine {
                        kind: DiffLineKind::Addition,
                        old_no: None,
                        new_no: Some(2),
                        text: "added".into(),
                    },
                ],
            }],
            byte_capped: false,
            error: None,
        };
        assert!(
            diff.slice_page("", 0, 10).is_err(),
            "empty snapshot id rejected"
        );
        assert!(
            diff.slice_page("snap", 3, 10).is_err(),
            "cursor past total rejected"
        );
        // cursor == total is valid (returns an empty terminal page).
        let terminal = diff.slice_page("snap", 2, 10).expect("cursor == total ok");
        assert!(terminal.hunks.is_empty());
        assert!(!terminal.has_more);
        assert_eq!(terminal.next_cursor, None);
        assert_eq!(terminal.total_lines, 2);
        assert_eq!(terminal.hunk_count, 1);
    }

    #[test]
    fn slice_page_clamps_max_lines_and_preserves_order() {
        let lines: Vec<DiffLine> = (0..5)
            .map(|i| DiffLine {
                kind: DiffLineKind::Addition,
                old_no: None,
                new_no: Some(i + 1),
                text: format!("line{i}"),
            })
            .collect();
        let diff = FileDiff {
            path: "a.rs".into(),
            previous_path: None,
            status: FileStatus::Modified,
            binary: false,
            hunks: vec![DiffHunk {
                header: "@@ -0,0 +1,5 @@".into(),
                old_start: 0,
                old_count: 0,
                new_start: 1,
                new_count: 5,
                lines,
            }],
            byte_capped: false,
            error: None,
        };
        let page_lines = |page: &FileDiffPage| {
            page.hunks
                .iter()
                .flat_map(|hunk| hunk.lines.iter().map(|l| l.text.clone()))
                .collect::<Vec<_>>()
        };
        let first = diff.slice_page("snap", 0, 2).expect("first page");
        assert_eq!(first.cursor, 0);
        assert_eq!(page_lines(&first), ["line0", "line1"]);
        assert_eq!(first.hunks[0].line_start, 0);
        assert!(first.has_more);
        assert_eq!(first.next_cursor, Some(2));
        let second = diff.slice_page("snap", 2, 2).expect("second page");
        assert_eq!(second.cursor, 2);
        assert_eq!(page_lines(&second), ["line2", "line3"]);
        assert_eq!(second.hunks[0].line_start, 2);
        assert!(second.has_more);
        assert_eq!(second.next_cursor, Some(4));
        let third = diff.slice_page("snap", 4, 2).expect("third page");
        assert_eq!(page_lines(&third), ["line4"]);
        assert_eq!(third.hunks[0].line_start, 4);
        assert!(!third.has_more);
        assert_eq!(third.next_cursor, None);
        // max_lines=0 clamps to 1.
        let one = diff.slice_page("snap", 0, 0).expect("max_lines 0 -> 1");
        assert_eq!(page_lines(&one), ["line0"]);
    }

    #[test]
    fn slice_page_surfaces_error_and_binary_state() {
        let err = FileDiff {
            path: "a.rs".into(),
            previous_path: None,
            status: FileStatus::Unknown,
            binary: false,
            hunks: Vec::new(),
            byte_capped: false,
            error: Some("boom".into()),
        };
        assert!(err.slice_page("snap", 0, 10).is_err());
        let bin = FileDiff {
            path: "a.png".into(),
            previous_path: None,
            status: FileStatus::Binary,
            binary: true,
            hunks: Vec::new(),
            byte_capped: false,
            error: None,
        };
        let page = bin.slice_page("snap", 0, 10).expect("binary page");
        assert!(page.binary);
        assert!(page.hunks.is_empty());
        assert!(!page.has_more);
        assert_eq!(page.hunk_count, 0);
    }

    #[test]
    fn slice_page_hunk_windows_keep_complete_identity_across_page_splits() {
        // A two-hunk file whose hunks split across pages: page 1 carries the
        // tail of hunk 0, page 2 the rest. Every page's descriptor must carry
        // the COMPLETE hunk's identity (hash over the full hunk text, not the
        // page fragment) so the frontend can deterministically merge by index.
        let hunk0_lines: Vec<DiffLine> = (0..6)
            .map(|i| DiffLine {
                kind: DiffLineKind::Addition,
                old_no: None,
                new_no: Some(i + 1),
                text: format!("h0-{i}"),
            })
            .collect();
        let hunk0 = DiffHunk {
            header: "@@ -0,0 +1,6 @@".into(),
            old_start: 0,
            old_count: 0,
            new_start: 1,
            new_count: 6,
            lines: hunk0_lines,
        };
        let hunk1_lines: Vec<DiffLine> = (7..12)
            .map(|i| DiffLine {
                kind: DiffLineKind::Addition,
                old_no: None,
                new_no: Some(i + 1),
                text: format!("h1-{i}"),
            })
            .collect();
        let hunk1 = DiffHunk {
            header: "@@ -0,0 +7,5 @@".into(),
            old_start: 0,
            old_count: 0,
            new_start: 7,
            new_count: 5,
            lines: hunk1_lines,
        };
        let diff = FileDiff {
            path: "a.rs".into(),
            previous_path: None,
            status: FileStatus::Modified,
            binary: false,
            hunks: vec![hunk0, hunk1],
            byte_capped: false,
            error: None,
        };
        let full_hash =
            |index: usize| HunkIdentity::new("snap", "a.rs", &diff.hunks[index]).content_hash;
        // Explicit page-split scenario: page0 = hunk 0 head (cursor 0, max 4),
        // page1 = hunk 0 tail (cursor 4, max 2), page2 = hunk 1 head
        // (cursor 6, max 4), page3 = hunk 1 tail (cursor 10, max 4). A hunk
        // whose lines span pages must keep ONE stable identity — the
        // descriptor hash covers the COMPLETE hunk, never the page fragment.
        let page0 = diff.slice_page("snap", 0, 4).expect("page 0");
        assert_eq!(page0.cursor, 0);
        assert_eq!(page0.next_cursor, Some(4));
        assert!(page0.has_more);
        assert_eq!(page0.hunks.len(), 1);
        assert_eq!(page0.hunks[0].index, 0);
        assert_eq!(page0.hunks[0].line_start, 0);
        assert_eq!(
            page0.hunks[0]
                .lines
                .iter()
                .map(|l| l.text.as_str())
                .collect::<Vec<_>>(),
            ["h0-0", "h0-1", "h0-2", "h0-3"]
        );
        assert_eq!(page0.hunks[0].total_lines, 6);
        assert!(page0.hunks[0].complete);
        assert_eq!(page0.hunks[0].content_hash, full_hash(0));
        assert_eq!(page0.hunk_count, 2);

        let page1 = diff.slice_page("snap", 4, 2).expect("page 1");
        assert_eq!(page1.cursor, 4);
        assert_eq!(page1.next_cursor, Some(6));
        assert!(page1.has_more);
        assert_eq!(page1.hunks.len(), 1);
        assert_eq!(page1.hunks[0].index, 0);
        assert_eq!(page1.hunks[0].line_start, 4);
        assert_eq!(
            page1.hunks[0]
                .lines
                .iter()
                .map(|l| l.text.as_str())
                .collect::<Vec<_>>(),
            ["h0-4", "h0-5"]
        );
        // Hunk 0 keeps one stable identity across its split pages.
        assert_eq!(page1.hunks[0].content_hash, full_hash(0));
        assert_eq!(page1.hunks[0].content_hash, page0.hunks[0].content_hash);

        let page2 = diff.slice_page("snap", 6, 4).expect("page 2");
        assert_eq!(page2.cursor, 6);
        assert_eq!(page2.next_cursor, Some(10));
        assert!(page2.has_more);
        assert_eq!(page2.hunks.len(), 1);
        assert_eq!(page2.hunks[0].index, 1);
        assert_eq!(page2.hunks[0].line_start, 0);
        assert_eq!(
            page2.hunks[0]
                .lines
                .iter()
                .map(|l| l.text.as_str())
                .collect::<Vec<_>>(),
            ["h1-7", "h1-8", "h1-9", "h1-10"]
        );
        assert_eq!(page2.hunks[0].total_lines, 5);
        assert_eq!(page2.hunks[0].content_hash, full_hash(1));
        assert_eq!(page2.hunk_count, 2);

        let page3 = diff.slice_page("snap", 10, 4).expect("page 3");
        assert_eq!(page3.cursor, 10);
        assert_eq!(page3.next_cursor, None);
        assert!(!page3.has_more);
        assert_eq!(page3.hunks.len(), 1);
        assert_eq!(page3.hunks[0].index, 1);
        assert_eq!(page3.hunks[0].line_start, 4);
        assert_eq!(
            page3.hunks[0]
                .lines
                .iter()
                .map(|l| l.text.as_str())
                .collect::<Vec<_>>(),
            ["h1-11"]
        );
        // The split hunk keeps one stable identity on every page carrying it.
        assert_eq!(page3.hunks[0].content_hash, full_hash(1));
        assert_eq!(page3.hunks[0].content_hash, page2.hunks[0].content_hash);
        assert_eq!(page3.hunk_count, 2);
        // Deterministic reconstruction: concat subsets by (index, line_start)
        // reproduces the complete hunk lines byte-for-byte.
        let mut merged0: Vec<String> = Vec::new();
        merged0.extend(
            page0
                .hunks
                .iter()
                .filter(|h| h.index == 0)
                .flat_map(|h| h.lines.iter().map(|l| l.text.clone())),
        );
        merged0.extend(
            page1
                .hunks
                .iter()
                .filter(|h| h.index == 0)
                .flat_map(|h| h.lines.iter().map(|l| l.text.clone())),
        );
        let mut merged1: Vec<String> = Vec::new();
        merged1.extend(
            page2
                .hunks
                .iter()
                .filter(|h| h.index == 1)
                .flat_map(|h| h.lines.iter().map(|l| l.text.clone())),
        );
        merged1.extend(
            page3
                .hunks
                .iter()
                .filter(|h| h.index == 1)
                .flat_map(|h| h.lines.iter().map(|l| l.text.clone())),
        );
        assert_eq!(
            merged0,
            diff.hunks[0]
                .lines
                .iter()
                .map(|l| l.text.clone())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            merged1,
            diff.hunks[1]
                .lines
                .iter()
                .map(|l| l.text.clone())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn slice_page_marks_byte_capped_last_hunk_incomplete() {
        // A per-file load cut by the 8 MiB byte cap mid-hunk: the parsed last
        // hunk's body is shorter than its header counts. Its descriptor must
        // be complete=false (never comment-ready) while earlier hunks stay
        // complete=true.
        let partial = DiffHunk {
            header: "@@ -1,3 +1,3 @@".into(),
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 3,
            lines: vec![
                DiffLine {
                    kind: DiffLineKind::Context,
                    old_no: Some(1),
                    new_no: Some(1),
                    text: "a".into(),
                },
                DiffLine {
                    kind: DiffLineKind::Deletion,
                    old_no: Some(2),
                    new_no: None,
                    text: "b".into(),
                },
                // Cut before the remaining context lines arrived.
            ],
        };
        assert!(!partial.is_complete());
        // A complete `@@ -1,3 +1,3 @@` hunk: old_count = deletions + context
        // (2+1), new_count = additions + context (2+1) → 5 content lines.
        let full = DiffHunk {
            header: "@@ -1,3 +1,3 @@".into(),
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 3,
            lines: vec![
                DiffLine {
                    kind: DiffLineKind::Context,
                    old_no: Some(1),
                    new_no: Some(1),
                    text: "a".into(),
                },
                DiffLine {
                    kind: DiffLineKind::Deletion,
                    old_no: Some(2),
                    new_no: None,
                    text: "b".into(),
                },
                DiffLine {
                    kind: DiffLineKind::Deletion,
                    old_no: Some(3),
                    new_no: None,
                    text: "b2".into(),
                },
                DiffLine {
                    kind: DiffLineKind::Addition,
                    old_no: None,
                    new_no: Some(2),
                    text: "c".into(),
                },
                DiffLine {
                    kind: DiffLineKind::Addition,
                    old_no: None,
                    new_no: Some(3),
                    text: "c2".into(),
                },
            ],
        };
        assert!(full.is_complete());
        let diff = FileDiff {
            path: "big.rs".into(),
            previous_path: None,
            status: FileStatus::Modified,
            binary: false,
            hunks: vec![full, partial],
            byte_capped: true,
            error: None,
        };
        let page = diff.slice_page("snap", 0, 100).expect("page");
        assert_eq!(page.hunks.len(), 2);
        assert!(page.hunks[0].complete);
        assert!(!page.hunks[1].complete);
        assert_eq!(page.hunks[1].total_lines, 2);
        assert!(page.byte_capped);
    }

    #[test]
    fn slice_page_marks_byte_capped_counts_satisfied_last_hunk_incomplete() {
        let counts_satisfied = DiffHunk {
            header: "@@ -1,1 +1,1 @@".into(),
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 1,
            lines: vec![DiffLine {
                kind: DiffLineKind::Context,
                old_no: Some(1),
                new_no: Some(1),
                text: "no-eol tail".into(),
            }],
        };
        assert!(counts_satisfied.is_complete());
        let diff = FileDiff {
            path: "tail.rs".into(),
            previous_path: None,
            status: FileStatus::Modified,
            binary: false,
            hunks: vec![counts_satisfied],
            byte_capped: true,
            error: None,
        };
        let page = diff.slice_page("snap", 0, 100).expect("page");
        assert_eq!(page.hunks.len(), 1);
        assert!(
            !page.hunks[0].complete,
            "a byte-capped final hunk is never comment-ready, counts satisfied or not"
        );
        assert!(page.byte_capped);
    }

    #[test]
    fn slice_page_descriptor_reserved_before_lines_keeps_accumulator_within_cap() {
        let text = "a".repeat(4096);
        let lines: Vec<DiffLine> = (0..300)
            .map(|i| DiffLine {
                kind: DiffLineKind::Addition,
                old_no: None,
                new_no: Some(i + 1),
                text: text.clone(),
            })
            .collect();
        let diff = FileDiff {
            path: "cap.rs".into(),
            previous_path: None,
            status: FileStatus::Modified,
            binary: false,
            hunks: vec![DiffHunk {
                header: "@@ -0,0 +1,300 @@".into(),
                old_start: 0,
                old_count: 0,
                new_start: 1,
                new_count: 300,
                lines,
            }],
            byte_capped: false,
            error: None,
        };
        let reserve = PAGE_JSON_OVERHEAD_BYTES + 6 * (5 + 6);
        let expected_take =
            (MAX_FILE_PAGE_BYTES - reserve - MAX_HUNK_DESCRIPTOR_BYTES) / (4096 + LINE_JSON_OVERHEAD_BYTES);
        let page = diff
            .slice_page("snap", 0, MAX_FILE_PAGE_LINES)
            .expect("page");
        assert_eq!(page.hunks[0].lines.len(), expected_take);
        let accumulator: usize = reserve
            + page
                .hunks
                .iter()
                .map(|h| {
                    MAX_HUNK_DESCRIPTOR_BYTES
                        + h.lines
                            .iter()
                            .map(|l| json_escaped_len(l.text.as_bytes()) + LINE_JSON_OVERHEAD_BYTES)
                            .sum::<usize>()
                })
                .sum::<usize>();
        assert!(
            accumulator <= MAX_FILE_PAGE_BYTES,
            "page accumulator {accumulator} exceeds MAX_FILE_PAGE_BYTES"
        );
        assert!(page.has_more);
        assert_eq!(page.next_cursor, Some(expected_take));
    }

    #[test]
    fn slice_page_second_hunk_reserves_its_descriptor_before_its_first_line() {
        let text = "a".repeat(4096);
        let hunk0_lines: Vec<DiffLine> = (0..251)
            .map(|i| DiffLine {
                kind: DiffLineKind::Addition,
                old_no: None,
                new_no: Some(i + 1),
                text: text.clone(),
            })
            .collect();
        let hunk1_lines: Vec<DiffLine> = (0..5)
            .map(|i| DiffLine {
                kind: DiffLineKind::Addition,
                old_no: None,
                new_no: Some(252 + i),
                text: "b".repeat(3900),
            })
            .collect();
        let diff = FileDiff {
            path: "split.rs".into(),
            previous_path: None,
            status: FileStatus::Modified,
            binary: false,
            hunks: vec![
                DiffHunk {
                    header: "@@ -0,0 +1,251 @@".into(),
                    old_start: 0,
                    old_count: 0,
                    new_start: 1,
                    new_count: 251,
                    lines: hunk0_lines,
                },
                DiffHunk {
                    header: "@@ -0,0 +252,5 @@".into(),
                    old_start: 0,
                    old_count: 0,
                    new_start: 252,
                    new_count: 5,
                    lines: hunk1_lines,
                },
            ],
            byte_capped: false,
            error: None,
        };
        let page0 = diff
            .slice_page("snap", 0, MAX_FILE_PAGE_LINES)
            .expect("page 0");
        assert_eq!(
            page0.hunks.len(),
            1,
            "hunk 1 must not join the byte-bound page: its descriptor no longer fits"
        );
        assert_eq!(page0.hunks[0].index, 0);
        let accumulator: usize = PAGE_JSON_OVERHEAD_BYTES + 6 * (4 + 8)
            + page0
                .hunks
                .iter()
                .map(|h| {
                    MAX_HUNK_DESCRIPTOR_BYTES
                        + h.lines
                            .iter()
                            .map(|l| json_escaped_len(l.text.as_bytes()) + LINE_JSON_OVERHEAD_BYTES)
                            .sum::<usize>()
                })
                .sum::<usize>();
        assert!(
            accumulator <= MAX_FILE_PAGE_BYTES,
            "page accumulator {accumulator} exceeds MAX_FILE_PAGE_BYTES"
        );
        let page1 = diff
            .slice_page("snap", page0.next_cursor.expect("page 0 continues"), MAX_FILE_PAGE_LINES)
            .expect("page 1");
        assert_eq!(page1.hunks[0].index, 1);
        assert_eq!(page1.hunks[0].line_start, 0);
        assert_eq!(page1.hunks[0].lines.len(), 5);
    }

    #[test]
    fn slice_page_first_line_is_always_taken_as_paging_progress() {
        let huge = "x".repeat(MAX_FILE_PAGE_BYTES);
        let lines: Vec<DiffLine> = (0..2)
            .map(|i| DiffLine {
                kind: DiffLineKind::Addition,
                old_no: None,
                new_no: Some(i + 1),
                text: huge.clone(),
            })
            .collect();
        let diff = FileDiff {
            path: "huge-line.rs".into(),
            previous_path: None,
            status: FileStatus::Modified,
            binary: false,
            hunks: vec![DiffHunk {
                header: "@@ -0,0 +1,2 @@".into(),
                old_start: 0,
                old_count: 0,
                new_start: 1,
                new_count: 2,
                lines,
            }],
            byte_capped: false,
            error: None,
        };
        let page = diff.slice_page("snap", 0, 10).expect("page");
        assert_eq!(
            page.hunks[0].lines.len(),
            1,
            "the one-line progress exception takes the oversized first line"
        );
        assert!(page.has_more);
        assert_eq!(page.next_cursor, Some(1));
    }

    #[test]
    fn cap_line_truncates_oversize_text_on_char_boundary() {
        let huge = "x".repeat(MAX_DIFF_LINE_TEXT_BYTES + 200);
        let line = DiffLine {
            kind: DiffLineKind::Addition,
            old_no: None,
            new_no: Some(1),
            text: huge,
        };
        let capped = cap_line(line);
        assert!(capped.text.len() < MAX_DIFF_LINE_TEXT_BYTES + 200);
        assert!(capped.text.contains("⋯[line truncated: exceeds byte cap]"));
        let small = cap_line(DiffLine {
            kind: DiffLineKind::Context,
            old_no: Some(1),
            new_no: Some(1),
            text: "ok".into(),
        });
        assert_eq!(small.text, "ok");
    }

    #[test]
    fn hunk_headers_are_bounded_at_load_with_visible_marker() {
        let long_suffix = "x".repeat(2 * 1024 * 1024);
        let input = format!(
            "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@ {long_suffix}\n-old\n+new\n"
        );
        let files = parse_unified_diff(&input);
        assert_eq!(files.len(), 1);
        let hunk = &files[0].hunks[0];
        assert!(
            json_escaped_len(hunk.header.as_bytes()) <= MAX_HUNK_HEADER_BYTES,
            "retained header must be hard-bounded ({} bytes)",
            hunk.header.len()
        );
        assert!(hunk.header.contains("⋯[hunk header truncated: exceeds byte cap]"));
        assert_eq!(hunk.old_start, 1);
        assert_eq!(hunk.new_start, 1);

        let multibyte = "中".repeat(MAX_HUNK_HEADER_BYTES);
        let input = format!(
            "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@ {multibyte}\n-old\n+new\n"
        );
        let files = parse_unified_diff(&input);
        let header = &files[0].hunks[0].header;
        assert!(std::str::from_utf8(header.as_bytes()).is_ok());
        assert!(json_escaped_len(header.as_bytes()) <= MAX_HUNK_HEADER_BYTES);
        assert!(header.contains("⋯[hunk header truncated: exceeds byte cap]"));
        let diff = FileDiff {
            path: "a.rs".into(),
            previous_path: None,
            status: FileStatus::Modified,
            binary: false,
            hunks: vec![DiffHunk {
                header: files[0].hunks[0].header.clone(),
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 2,
                lines: vec![DiffLine {
                    kind: DiffLineKind::Addition,
                    old_no: None,
                    new_no: Some(1),
                    text: "new".into(),
                }],
            }],
            byte_capped: false,
            error: None,
        };
        let page = diff.slice_page("snap", 0, 10).expect("page");
        let accumulator = PAGE_JSON_OVERHEAD_BYTES + 6 * (4 + 4)
            + MAX_HUNK_DESCRIPTOR_BYTES
            + page
                .hunks
                .iter()
                .map(|h| {
                    h.lines
                        .iter()
                        .map(|l| json_escaped_len(l.text.as_bytes()) + LINE_JSON_OVERHEAD_BYTES)
                        .sum::<usize>()
                })
                .sum::<usize>();
        assert!(
            accumulator <= MAX_FILE_PAGE_BYTES,
            "capped-header page accumulator {accumulator} exceeds the page cap"
        );
        assert_eq!(page.hunks[0].header, files[0].hunks[0].header);
    }

    #[test]
    fn truncated_entry_final_hunk_is_never_comment_ready_even_with_satisfied_counts() {
        // Git-complete final hunk: header counts (1,1) are satisfied by a
        // single context line, so `is_complete` is true and readiness below
        // is decided solely by the truncated-last guard.
        let counts_satisfied = DiffHunk {
            header: "@@ -1,1 +1,1 @@".into(),
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 1,
            lines: vec![DiffLine {
                kind: DiffLineKind::Context,
                old_no: Some(1),
                new_no: Some(1),
                text: "kept".into(),
            }],
        };
        let earlier = DiffHunk {
            header: "@@ -0,0 +1,1 @@".into(),
            old_start: 0,
            old_count: 0,
            new_start: 1,
            new_count: 1,
            lines: vec![DiffLine {
                kind: DiffLineKind::Addition,
                old_no: None,
                new_no: Some(1),
                text: "first".into(),
            }],
        };
        let mut file = DiffFile {
            path: "a.rs".into(),
            previous_path: None,
            status: FileStatus::Modified,
            binary: false,
            insertions: 1,
            deletions: 0,
            hunks: vec![earlier, counts_satisfied],
            truncated: true,
            message: Some(PREVIEW_CUT_MESSAGE.to_owned()),
        };
        assert!(!file.hunk_is_comment_ready(1), "truncated final hunk is never ready");
        assert!(file.hunk_is_comment_ready(0), "earlier hunk of a truncated entry stays ready");
        file.truncated = false;
        assert!(file.hunk_is_comment_ready(1), "complete entry's final hunk is ready");
    }

    #[test]
    fn untracked_binary_file_captures_as_added_binary() {
        let repo = init_repo();
        fs::write(
            repo.path().join("blob.dat"),
            [0u8, 1, 0, 2, 0].as_slice(),
        )
        .expect("write binary");
        let index = repo.path().join(".git/index");
        let index_before = fs::read(&index).expect("index before");

        let snapshot = load_review_snapshot(repo.path());

        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        let entry = snapshot
            .files
            .iter()
            .find(|file| file.path == "blob.dat")
            .expect("binary untracked entry present");
        // The parser flags the "Binary files differ" line; that status is
        // the same one a tracked new binary file reports.
        assert_eq!(entry.status, FileStatus::Binary);
        assert!(entry.binary, "zero-scented blob must read as binary");
        assert!(entry.hunks.is_empty(), "binary entry carries no hunks");
        assert_eq!(fs::read(&index).expect("index after"), index_before);
    }

    #[test]
    fn untracked_files_load_on_demand_through_same_no_index_path() {
        // On-demand paging for an untracked Added entry must capture the same
        // `--no-index` body the snapshot carried, not consult the index.
        let repo = init_repo();
        write(repo.path().join("fresh.md").as_path(), "# new\nfile\n");
        let snapshot = load_review_snapshot(repo.path());
        let entry = snapshot
            .files
            .iter()
            .find(|file| file.path == "fresh.md")
            .expect("untracked entry")
            .clone();
        assert_eq!(entry.status, FileStatus::Added);

        let diff = FileDiff::load(repo.path(), &ReviewScope::WorkingTree, &entry);
        assert!(diff.error.is_none(), "{:?}", diff.error);
        assert!(
            diff.hunks
                .iter()
                .flat_map(|hunk| &hunk.lines)
                .any(|line| line.kind == DiffLineKind::Addition && line.text == "file"),
            "on-demand body lost untracked content: {:?}",
            diff
        );
        let page = diff
            .slice_page(&snapshot.snapshot_id, 0, 1)
            .expect("page");
        assert!(page.has_more, "paged window is a strict subset");
        assert_eq!(page.path, "fresh.md");
        assert_eq!(page.status, FileStatus::Added);
    }

    #[test]
    fn untracked_pages_serve_open_time_body_after_worktree_mutation() {
        let repo = init_repo();
        write(repo.path().join("fresh.md").as_path(), "# new\nfile\n");
        let (snapshot, paging) =
            load_review_snapshot_capture(repo.path(), ReviewScope::WorkingTree);
        let entry = snapshot
            .files
            .iter()
            .find(|file| file.path == "fresh.md")
            .expect("untracked entry")
            .clone();
        assert_eq!(entry.status, FileStatus::Added);

        write(
            repo.path().join("fresh.md").as_path(),
            "# new\nfile\nREWRITTEN\n",
        );
        let diff = FileDiff::load_snapshot_file(repo.path(), &snapshot.scope, &paging, &entry);
        assert!(diff.error.is_none(), "{:?}", diff.error);
        let lines: Vec<&str> = diff
            .hunks
            .iter()
            .flat_map(|hunk| &hunk.lines)
            .map(|line| line.text.as_str())
            .collect();
        assert!(
            lines.iter().all(|text| !text.contains("REWRITTEN")),
            "old snapshot must never page post-open worktree content: {lines:?}"
        );
        assert!(
            lines.iter().any(|text| text.contains("file")),
            "old snapshot must page the open-time body: {lines:?}"
        );

        let (refreshed, refreshed_paging) =
            load_review_snapshot_capture(repo.path(), ReviewScope::WorkingTree);
        assert_ne!(snapshot.snapshot_id, refreshed.snapshot_id);
        let refreshed_entry = refreshed
            .files
            .iter()
            .find(|file| file.path == "fresh.md")
            .expect("refreshed untracked entry")
            .clone();
        let refreshed_diff = FileDiff::load_snapshot_file(
            repo.path(),
            &refreshed.scope,
            &refreshed_paging,
            &refreshed_entry,
        );
        assert!(refreshed_diff.error.is_none(), "{:?}", refreshed_diff.error);
        assert!(
            refreshed_diff
                .hunks
                .iter()
                .flat_map(|hunk| &hunk.lines)
                .any(|line| line.text.contains("REWRITTEN")),
            "refresh must expose the rewritten body: {:?}",
            refreshed_diff
        );
    }

    #[test]
    fn untracked_budget_placeholder_pages_from_pinned_copy_after_mutation() {
        let repo = init_repo();
        let big = format!("{}\n", "a".repeat(3 * 1024 * 1024));
        write(repo.path().join("aaa-budget.txt").as_path(), &big);
        write(repo.path().join("zzz-pinned.txt").as_path(), "pinned v1\n");
        let (snapshot, paging) =
            load_review_snapshot_capture(repo.path(), ReviewScope::WorkingTree);
        let budget_entry = snapshot
            .files
            .iter()
            .find(|file| file.path == "aaa-budget.txt")
            .expect("budget-consuming entry")
            .clone();
        let pinned_entry = snapshot
            .files
            .iter()
            .find(|file| file.path == "zzz-pinned.txt")
            .expect("budget-exhausted placeholder")
            .clone();
        assert!(pinned_entry.hunks.is_empty(), "placeholder must seed empty");

        write(repo.path().join("zzz-pinned.txt").as_path(), "pinned v2 REWRITTEN\n");
        let diff = FileDiff::load_snapshot_file(repo.path(), &snapshot.scope, &paging, &pinned_entry);
        assert!(diff.error.is_none(), "{:?}", diff.error);
        assert!(
            diff.hunks
                .iter()
                .flat_map(|hunk| &hunk.lines)
                .any(|line| line.text.contains("pinned v1")),
            "placeholder must page from the open-time pinned copy: {:?}",
            diff
        );
        assert!(
            diff.hunks
                .iter()
                .flat_map(|hunk| &hunk.lines)
                .all(|line| !line.text.contains("REWRITTEN")),
            "placeholder must never page the post-open body: {:?}",
            diff
        );
        let budget_diff =
            FileDiff::load_snapshot_file(repo.path(), &snapshot.scope, &paging, &budget_entry);
        assert!(budget_diff.error.is_none(), "{:?}", budget_diff.error);
        assert!(
            !budget_diff.hunks.is_empty(),
            "retained open-time patch must page the budget-consuming file"
        );

        let (refreshed, refreshed_paging) =
            load_review_snapshot_capture(repo.path(), ReviewScope::WorkingTree);
        assert_ne!(snapshot.snapshot_id, refreshed.snapshot_id);
        let refreshed_entry = refreshed
            .files
            .iter()
            .find(|file| file.path == "zzz-pinned.txt")
            .expect("refreshed placeholder")
            .clone();
        let refreshed_diff = FileDiff::load_snapshot_file(
            repo.path(),
            &refreshed.scope,
            &refreshed_paging,
            &refreshed_entry,
        );
        assert!(refreshed_diff.error.is_none(), "{:?}", refreshed_diff.error);
        assert!(
            refreshed_diff
                .hunks
                .iter()
                .flat_map(|hunk| &hunk.lines)
                .any(|line| line.text.contains("pinned v2 REWRITTEN")),
            "refresh must re-capture the rewritten placeholder body: {:?}",
            refreshed_diff
        );
    }

    #[test]
    fn untracked_content_change_flips_snapshot_identity() {
        let repo = init_repo();
        write(repo.path().join("evolving.txt").as_path(), "v1\n");
        let snapshot_a = load_review_snapshot(repo.path());
        write(repo.path().join("evolving.txt").as_path(), "v2\nlonger\n");
        let snapshot_b = load_review_snapshot(repo.path());
        assert_ne!(
            snapshot_a.snapshot_id, snapshot_b.snapshot_id,
            "untracked content change must flip the stale guard"
        );
    }

    #[test]
    fn untracked_count_bound_fails_closed() {
        let repo = init_repo();
        for index in 0..=MAX_UNTRACKED_FILES {
            fs::write(
                repo.path().join(format!("u{index:05}.txt")),
                "tiny\n",
            )
            .expect("write bound file");
        }

        let snapshot = load_review_snapshot(repo.path());

        assert_eq!(
            snapshot.error.as_deref(),
            Some(
                format!("untracked file count exceeds the review limit of {MAX_UNTRACKED_FILES}")
                    .as_str()
            )
        );
    }
    #[test]
    fn untracked_only_oversized_file_survives_as_placeholder() {
        // Untracked-only worktrees used to reparse concatenated patches and
        // drop every oversized file (no patch bytes, no catalog). The
        // snapshot must keep the on-demand placeholder so the file never
        // silently vanishes.
        let repo = init_repo();
        let oversized = vec![0u8; MAX_FILE_DIFF_BYTES + 1];
        fs::write(repo.path().join("huge.bin"), &oversized).expect("write oversized");
        write(repo.path().join("tiny.txt").as_path(), "hello\n");
        let index = repo.path().join(".git/index");
        let index_before = fs::read(&index).expect("index before");

        let snapshot = load_review_snapshot(repo.path());

        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        let huge = snapshot
            .files
            .iter()
            .find(|file| file.path == "huge.bin")
            .expect("oversized untracked file present as placeholder");
        assert_eq!(huge.status, FileStatus::Added);
        assert!(huge.hunks.is_empty(), "placeholder carries no hunks");
        assert!(!huge.truncated && huge.binary && huge.body_state() == "binary");
        assert_eq!(huge.insertions, 0);
        assert_eq!(huge.message.as_deref(), Some(PLACEHOLDER_MESSAGE));
        assert_eq!(snapshot.total_insertions(), 1, "tiny.txt's single line");
        let tiny = snapshot
            .files
            .iter()
            .find(|file| file.path == "tiny.txt")
            .expect("small untracked file present");
        assert_eq!(tiny.status, FileStatus::Added);
        assert!(!tiny.hunks.is_empty());
        assert_eq!(fs::read(&index).expect("index after"), index_before);
    }

    #[test]
    fn untracked_only_oversized_placeholder_content_change_flips_snapshot_identity() {
        let repo = init_repo();
        let mut oversized = vec![0u8; MAX_FILE_DIFF_BYTES + 1];
        fs::write(repo.path().join("huge.bin"), &oversized).expect("write oversized");
        let snapshot_a = load_review_snapshot(repo.path());
        assert!(snapshot_a.error.is_none(), "{:?}", snapshot_a.error);
        let huge = snapshot_a
            .files
            .iter()
            .find(|file| file.path == "huge.bin")
            .expect("oversized untracked placeholder");
        assert!(huge.hunks.is_empty() && !huge.truncated && huge.binary, "{huge:?}");

        oversized[0] = 1;
        fs::write(repo.path().join("huge.bin"), &oversized).expect("rewrite oversized");
        let snapshot_b = load_review_snapshot(repo.path());
        assert!(snapshot_b.error.is_none(), "{:?}", snapshot_b.error);
        assert_ne!(
            snapshot_a.snapshot_id, snapshot_b.snapshot_id,
            "oversized untracked placeholder content change must flip snapshot id"
        );
        let snapshot_b_again = load_review_snapshot(repo.path());
        assert_eq!(
            snapshot_b.snapshot_id, snapshot_b_again.snapshot_id,
            "identical oversized placeholder content must keep the identical identity"
        );
    }

    #[test]
    fn untracked_only_oversized_placeholder_name_change_flips_snapshot_identity() {
        let repo = init_repo();
        let oversized = vec![0u8; MAX_FILE_DIFF_BYTES + 1];
        fs::write(repo.path().join("huge.bin"), &oversized).expect("write oversized");
        let snapshot_a = load_review_snapshot(repo.path());
        assert!(snapshot_a.error.is_none(), "{:?}", snapshot_a.error);
        assert!(
            snapshot_a
                .files
                .iter()
                .any(|file| file.path == "huge.bin" && file.hunks.is_empty() && !file.truncated),
            "expected oversized placeholder: {:?}",
            snapshot_a
                .files
                .iter()
                .map(|file| (file.path.as_str(), file.truncated, file.hunks.len()))
                .collect::<Vec<_>>()
        );

        fs::rename(
            repo.path().join("huge.bin"),
            repo.path().join("huge-renamed.bin"),
        )
        .expect("rename oversized");
        let snapshot_b = load_review_snapshot(repo.path());
        assert!(snapshot_b.error.is_none(), "{:?}", snapshot_b.error);
        assert!(
            snapshot_b.files.iter().any(|file| file.path == "huge-renamed.bin"),
            "renamed oversized placeholder missing: {:?}",
            snapshot_b
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>()
        );
        assert_ne!(
            snapshot_a.snapshot_id, snapshot_b.snapshot_id,
            "oversized untracked placeholder name change must flip snapshot id"
        );
    }




    #[test]
    fn load_file_diff_loads_real_working_tree_file() {
        let repo = init_repo();
        write(repo.path().join("src/real.rs").as_path(), "base\n");
        git(repo.path(), &["add", "src/real.rs"]);
        git(repo.path(), &["commit", "-m", "add real"]);
        write(repo.path().join("src/real.rs").as_path(), "base\nedited\n");
        let snapshot = load_review_snapshot(repo.path());
        let real = snapshot
            .files
            .iter()
            .find(|f| f.path == "src/real.rs")
            .expect("real file")
            .clone();
        let diff = FileDiff::load(repo.path(), &ReviewScope::WorkingTree, &real);
        assert!(diff.error.is_none(), "{:?}", diff.error);
        assert!(
            diff.hunks
                .iter()
                .flat_map(|h| h.lines.iter())
                .any(|l| l.text == "edited"),
            "loaded diff lost the edit: {:?}",
            diff
        );
        let page = diff
            .slice_page(&snapshot.snapshot_id, 0, 100)
            .expect("page");
        assert!(
            page.hunks
                .iter()
                .flat_map(|h| h.lines.iter())
                .any(|l| l.text == "edited"),
            "page lost the edit"
        );
        assert_eq!(page.hunk_count, diff.hunks.len());
        assert!(page.hunks.iter().all(|h| h.complete));
    }

    #[test]
    fn load_file_diff_keeps_git_rm_cached_replacement_as_deletion() {
        let repo = init_repo();
        write(repo.path().join("tracked.txt").as_path(), "tracked\n");
        git(repo.path(), &["add", "tracked.txt"]);
        git(repo.path(), &["commit", "-m", "track file"]);
        git(repo.path(), &["rm", "--cached", "tracked.txt"]);
        write(
            repo.path().join("tracked.txt").as_path(),
            "untracked replacement\n",
        );

        let snapshot = load_review_snapshot(repo.path());
        let file = snapshot
            .files
            .iter()
            .find(|file| file.path == "tracked.txt")
            .expect("tracked deletion")
            .clone();
        assert_eq!(file.status, FileStatus::Deleted);
        let diff = FileDiff::load(repo.path(), &ReviewScope::WorkingTree, &file);
        assert!(diff.error.is_none(), "{:?}", diff.error);
        assert!(
            diff.hunks
                .iter()
                .flat_map(|hunk| &hunk.lines)
                .any(|line| { line.kind == DiffLineKind::Deletion && line.text == "tracked" })
        );
        assert!(!diff.hunks.iter().flat_map(|hunk| &hunk.lines).any(|line| {
            line.kind == DiffLineKind::Addition && line.text == "untracked replacement"
        }));
    }

    #[test]
    fn load_file_diff_keeps_file_to_directory_replacement_as_deletion() {
        let repo = init_repo();
        write(repo.path().join("swap.txt").as_path(), "tracked file\n");
        git(repo.path(), &["add", "swap.txt"]);
        git(repo.path(), &["commit", "-m", "track file"]);
        std::fs::remove_file(repo.path().join("swap.txt")).expect("remove tracked file");
        std::fs::create_dir(repo.path().join("swap.txt")).expect("replacement directory");
        write(
            repo.path().join("swap.txt/child.bin").as_path(),
            "untracked child\n",
        );

        let snapshot = load_review_snapshot(repo.path());
        let file = snapshot
            .files
            .iter()
            .find(|file| file.path == "swap.txt")
            .expect("tracked deletion")
            .clone();
        assert_eq!(file.status, FileStatus::Deleted);
        let diff = FileDiff::load(repo.path(), &ReviewScope::WorkingTree, &file);
        assert!(diff.error.is_none(), "{:?}", diff.error);
        assert!(
            diff.hunks
                .iter()
                .flat_map(|hunk| &hunk.lines)
                .any(|line| { line.kind == DiffLineKind::Deletion && line.text == "tracked file" })
        );
        assert!(
            !diff.hunks.iter().flat_map(|hunk| &hunk.lines).any(|line| {
                line.kind == DiffLineKind::Addition && line.text == "untracked child"
            })
        );
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
        let evil = DiffFile {
            path: "../etc/passwd".into(),
            previous_path: None,
            status: FileStatus::Modified,
            binary: false,
            insertions: 0,
            deletions: 0,
            hunks: Vec::new(),
            truncated: false,
            message: None,
        };
        let evil_diff = FileDiff::load(repo.path(), &ReviewScope::WorkingTree, &evil);
        assert!(
            evil_diff.error.is_some() || evil_diff.hunks.is_empty(),
            "traversal path did not fail closed: {:?}",
            evil_diff
        );
        // The traversal must never create a file outside the repo.
        assert!(
            !repo.path().join("../etc/passwd").exists(),
            "traversal escaped the repo root"
        );
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
        assert!(
            snapshot.truncated,
            "snapshot should be globally truncated: {:?}",
            snapshot.error
        );
        let big = snapshot
            .files
            .iter()
            .find(|f| f.path == big_path)
            .expect("big file")
            .clone();

        let diff = FileDiff::load(repo.path(), &ReviewScope::WorkingTree, &big);
        assert!(diff.error.is_none(), "{:?}", diff.error);
        assert!(!diff.binary);
        let total_lines: usize = diff.hunks.iter().map(|h| h.lines.len()).sum();
        assert!(
            total_lines > MAX_FILE_RENDER_LINES,
            "paging loader should recover the full file: got {} lines",
            total_lines
        );
        assert!(
            diff.hunks.iter().all(|h| h.is_complete()),
            "an uncapped per-file load must yield complete hunks"
        );
        // Page through the whole file; the paged line count must equal the
        // full file, every hunk descriptor must keep the complete identity,
        // and the final page has no next cursor.
        let mut cursor = 0usize;
        let mut pages = 0usize;
        let mut seen = 0usize;
        let mut seen_hunks: BTreeSet<usize> = BTreeSet::new();
        loop {
            let page = diff
                .slice_page(&snapshot.snapshot_id, cursor, MAX_FILE_PAGE_LINES)
                .expect("page");
            assert_eq!(page.cursor, cursor);
            assert_eq!(page.hunk_count, diff.hunks.len());
            seen += page.hunks.iter().map(|h| h.lines.len()).sum::<usize>();
            for hunk in &page.hunks {
                assert_eq!(hunk.total_lines, diff.hunks[hunk.index].lines.len());
                assert!(hunk.complete);
                assert_eq!(
                    hunk.content_hash,
                    HunkIdentity::new(&snapshot.snapshot_id, big_path, &diff.hunks[hunk.index])
                        .content_hash,
                    "page descriptor hash must cover the complete hunk"
                );
                seen_hunks.insert(hunk.index);
            }
            pages += 1;
            match page.next_cursor {
                Some(next) => cursor = next,
                None => break,
            }
            if pages > 1000 {
                panic!("paging did not terminate");
            }
        }
        assert_eq!(seen, total_lines, "paged lines must equal the full file");
        assert_eq!(
            seen_hunks.len(),
            diff.hunks.len(),
            "every hunk must be delivered"
        );
        assert!(pages > 1, "the big file must require more than one page");
    }
    #[test]
    fn fifty_thousand_line_diff_pages_to_terminal_bounded_and_complete() {
        let repo = init_repo();
        let lines = "line\n".repeat(50_000);
        write(repo.path().join("long.txt").as_path(), &lines);
        let snapshot = load_review_snapshot(repo.path());
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        assert!(!snapshot.truncated);
        assert!(snapshot.summary_complete);
        assert_eq!(snapshot.total_insertions(), 50_000);
        let file = snapshot
            .files
            .iter()
            .find(|file| file.path == "long.txt")
            .expect("long file entry")
            .clone();
        assert_eq!(file.body_state(), "complete");
        assert!(!file.truncated);
        assert_eq!(file.insertions, 50_000);

        let diff = FileDiff::load(repo.path(), &ReviewScope::WorkingTree, &file);
        assert!(diff.error.is_none(), "{:?}", diff.error);
        assert!(!diff.byte_capped);
        let total_lines: usize = diff.hunks.iter().map(|h| h.lines.len()).sum();
        assert_eq!(total_lines, 50_000);
        assert!(diff.hunks.iter().all(|h| h.is_complete()));
        let mut cursor = 0usize;
        let mut pages = 0usize;
        let mut seen = 0usize;
        let mut hashes = BTreeMap::new();
        loop {
            let page = diff
                .slice_page(&snapshot.snapshot_id, cursor, MAX_FILE_PAGE_LINES)
                .expect("page");
            assert!(
                page.hunks.iter().map(|h| h.lines.len()).sum::<usize>() <= MAX_FILE_PAGE_LINES,
                "page line bound"
            );
            assert!(!page.byte_capped);
            for hunk in &page.hunks {
                assert!(hunk.complete, "comment identity requires complete hunks");
                assert_eq!(hunk.total_lines, diff.hunks[hunk.index].lines.len());
                let identity = hashes.entry(hunk.index).or_insert(hunk.content_hash.clone());
                assert_eq!(*identity, hunk.content_hash, "stable hunk identity");
            }
            seen += page.hunks.iter().map(|h| h.lines.len()).sum::<usize>();
            pages += 1;
            match page.next_cursor {
                Some(next) => cursor = next,
                None => break,
            }
            assert!(pages <= 100, "paging must terminate within 50 pages");
        }
        assert_eq!(seen, 50_000);
        assert!(pages >= 50, "bounded pages: got {pages}");
    }

    #[test]
    fn untracked_oversized_text_file_degrades_to_placeholder() {
        let repo = init_repo();
        let mut oversized = vec![b'x'; MAX_FILE_DIFF_BYTES + 1];
        oversized[MAX_FILE_DIFF_BYTES] = b'\n';
        fs::write(repo.path().join("huge.txt"), &oversized).expect("write oversized text");
        write(repo.path().join("small.txt").as_path(), "ok\n");
        let snapshot = load_review_snapshot(repo.path());
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        let huge = snapshot
            .files
            .iter()
            .find(|file| file.path == "huge.txt")
            .expect("oversized untracked text must stay as a placeholder");
        assert_eq!(huge.status, FileStatus::Added);
        assert!(huge.hunks.is_empty());
        assert_eq!(huge.message.as_deref(), Some(PLACEHOLDER_MESSAGE));
        let small = snapshot
            .files
            .iter()
            .find(|file| file.path == "small.txt")
            .expect("sibling untracked must survive an oversized neighbor");
        assert_eq!(small.status, FileStatus::Added);
        assert_ne!(small.message.as_deref(), Some(PLACEHOLDER_MESSAGE));
        assert!(snapshot.truncated);
    }

    #[test]
    fn untracked_oversized_binary_placeholder_carries_zero_totals() {
        let repo = init_repo();
        let mut oversized = vec![0u8; MAX_FILE_DIFF_BYTES + 1024];
        oversized[0] = b'A';
        fs::write(repo.path().join("huge.bin"), &oversized).expect("write oversized binary");
        let snapshot = load_review_snapshot(repo.path());
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        let huge = snapshot
            .files
            .iter()
            .find(|file| file.path == "huge.bin")
            .expect("oversized binary placeholder");
        assert_eq!(huge.body_state(), "binary");
        assert!(huge.binary);
        assert_eq!(huge.insertions, 0);
        assert_eq!(huge.deletions, 0);
        assert_eq!(snapshot.total_insertions(), 0);
        assert_eq!(snapshot.total_deletions(), 0);
    }

    #[test]
    fn untracked_oversized_text_with_nul_past_probe_window_degrades_to_placeholder() {
        let repo = init_repo();
        let mut oversized = vec![b'x'; MAX_FILE_DIFF_BYTES + 1024];
        oversized[9000] = 0;
        fs::write(repo.path().join("late-nul.bin"), &oversized).expect("write oversized late-NUL");
        let snapshot = load_review_snapshot(repo.path());
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        let huge = snapshot
            .files
            .iter()
            .find(|file| file.path == "late-nul.bin")
            .expect("late-NUL oversized text must stay as a placeholder");
        assert_eq!(huge.status, FileStatus::Added);
        assert!(huge.hunks.is_empty());
        assert_eq!(huge.message.as_deref(), Some(PLACEHOLDER_MESSAGE));
        assert!(snapshot.truncated);
    }

    #[test]
    fn untracked_binary_probe_window_matches_git_first_few_bytes() {
        let dir = TempDir::new().expect("tempdir");
        let inside_path = dir.path().join("nul-inside.bin");
        let mut inside = vec![b'a'; GIT_BINARY_FIRST_FEW_BYTES];
        inside[GIT_BINARY_FIRST_FEW_BYTES - 1] = 0;
        fs::write(&inside_path, &inside).expect("write NUL inside window");
        assert_eq!(
            count_untracked_added_lines(&inside_path, "nul-inside.bin")
                .expect("inside-window probe"),
            (0, 0, true),
            "a NUL within the first 8000 bytes is binary"
        );

        let outside_path = dir.path().join("nul-outside.txt");
        let mut outside = vec![b'a'; GIT_BINARY_FIRST_FEW_BYTES + 1];
        outside[GIT_BINARY_FIRST_FEW_BYTES] = 0;
        fs::write(&outside_path, &outside).expect("write NUL outside window");
        assert_eq!(
            count_untracked_added_lines(&outside_path, "nul-outside.txt")
                .expect("outside-window probe"),
            (1, 0, false),
            "a NUL at byte 8001 is past git's probe window: the file stays text"
        );
    }

    #[test]
    fn rename_delete_untracked_totals_match_authoritative_summary() {
        let repo = init_repo();
        write(repo.path().join("modified.txt").as_path(), "a\nb\nc\n");
        write(repo.path().join("deleted.txt").as_path(), "gone\nlines\n");
        write(
            repo.path().join("rename-old.txt").as_path(),
            "keep\nthe\ncontext\nhere\n",
        );
        git(repo.path(), &["add", "-A"]);
        git(repo.path(), &["commit", "-m", "totals baseline"]);
        write(repo.path().join("modified.txt").as_path(), "a\nB\nc\nd\n");
        fs::remove_file(repo.path().join("deleted.txt")).expect("delete");
        git(repo.path(), &["mv", "rename-old.txt", "rename-new.txt"]);
        write(
            repo.path().join("rename-new.txt").as_path(),
            "keep\nthe\ncontext\nhere\nplus\n",
        );
        fs::write(repo.path().join("untracked.txt"), "one\ntwo\n").expect("untracked");

        let snapshot = load_review_snapshot(repo.path());
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        assert!(snapshot.summary_complete);
        assert_eq!(snapshot.total_insertions(), 5);
        assert_eq!(snapshot.total_deletions(), 3);
        let find = |path: &str| {
            snapshot
                .files
                .iter()
                .find(|file| file.path == path)
                .unwrap_or_else(|| panic!("missing {path}"))
        };
        assert_eq!(find("modified.txt").insertions, 2);
        assert_eq!(find("modified.txt").deletions, 1);
        assert_eq!(find("deleted.txt").body_state(), "complete");
        assert_eq!(find("deleted.txt").deletions, 2);
        let renamed = find("rename-new.txt");
        assert_eq!(renamed.status, FileStatus::Renamed);
        assert_eq!(renamed.insertions, 1);
        assert_eq!(renamed.deletions, 0);
        let untracked = find("untracked.txt");
        assert_eq!(untracked.status, FileStatus::Added);
        assert_eq!(untracked.insertions, 2);
        assert_eq!(untracked.body_state(), "complete");
    }

    #[test]
    fn tracked_binary_file_reports_binary_state_and_zero_totals() {
        let repo = init_repo();
        let mut blob = vec![0u8; 512];
        blob[0] = b'B';
        fs::write(repo.path().join("blob.bin"), &blob).expect("write blob");
        git(repo.path(), &["add", "blob.bin"]);
        git(repo.path(), &["commit", "-m", "binary baseline"]);
        blob[1] = b'C';
        fs::write(repo.path().join("blob.bin"), &blob).expect("rewrite blob");
        let snapshot = load_review_snapshot(repo.path());
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        let entry = snapshot
            .files
            .iter()
            .find(|file| file.path == "blob.bin")
            .expect("binary entry");
        assert_eq!(entry.body_state(), "binary");
        assert!(entry.binary);
        assert_eq!(entry.insertions, 0);
        assert_eq!(entry.deletions, 0);
        assert_eq!(snapshot.total_insertions(), 0);
        assert_eq!(snapshot.total_deletions(), 0);
        assert!(snapshot.summary_complete);
    }
    #[test]
    fn working_tree_snapshot_pages_stay_pinned_after_open_and_refresh_advances() {
        let repo = build_truncated_working_tree_repo();
        let scope = ReviewScope::WorkingTree;
        let (snapshot, paging) = load_review_snapshot_capture(repo.path(), scope.clone());
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        assert!(
            snapshot.truncated,
            "snapshot must exercise placeholder paging"
        );
        let file = snapshot
            .files
            .iter()
            .find(|file| file.path == "zz-later.txt")
            .expect("late placeholder");
        assert!(
            file.hunks.is_empty(),
            "late file must be a placeholder: {file:?}"
        );

        write(
            repo.path().join("zz-later.txt").as_path(),
            "base\nnewer-after-open\n",
        );

        let old_diff = FileDiff::load_snapshot_file(repo.path(), &scope, &paging, file);
        assert!(old_diff.error.is_none(), "{:?}", old_diff.error);
        let old_text = old_diff
            .hunks
            .iter()
            .flat_map(|hunk| &hunk.lines)
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>();
        assert!(
            old_text.contains(&"changed"),
            "old snapshot page lost old content: {old_text:?}"
        );
        assert!(
            !old_text.contains(&"newer-after-open"),
            "old snapshot page read the mutated worktree: {old_text:?}"
        );

        let (refreshed, refreshed_paging) =
            load_review_snapshot_capture(repo.path(), scope.clone());
        assert_ne!(snapshot.snapshot_id, refreshed.snapshot_id);
        let refreshed_file = refreshed
            .files
            .iter()
            .find(|file| file.path == "zz-later.txt")
            .expect("refreshed late file");
        let refreshed_diff =
            FileDiff::load_snapshot_file(repo.path(), &scope, &refreshed_paging, refreshed_file);
        assert!(refreshed_diff.error.is_none(), "{:?}", refreshed_diff.error);
        let refreshed_text = refreshed_diff
            .hunks
            .iter()
            .flat_map(|hunk| &hunk.lines)
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>();
        assert!(
            refreshed_text.contains(&"newer-after-open"),
            "refresh did not advance paging authority: {refreshed_text:?}"
        );
        assert!(
            !refreshed_text.contains(&"changed"),
            "refresh served old content: {refreshed_text:?}"
        );
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
        // Stage the rename so both sides are tracked changed paths (a bare
        // filesystem rename with an untracked destination is out of the
        // tracked-only review contract).
        git(repo.path(), &["mv", "old.txt", "new.txt"]);
        write(
            repo.path().join("new.txt").as_path(),
            &format!("{baseline}renamed\n"),
        );
        // A binary file (fake PNG header) added in the working tree.
        let png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x00IHDR\x00\x00\x00\x00";
        fs::write(repo.path().join("img.png"), png).expect("write png");
        git(repo.path(), &["add", "img.png"]);

        let snapshot = load_review_snapshot(repo.path());
        let renamed = snapshot
            .files
            .iter()
            .find(|f| f.path == "new.txt")
            .expect("renamed")
            .clone();
        assert_eq!(renamed.status, FileStatus::Renamed);
        let rename_diff = FileDiff::load(repo.path(), &ReviewScope::WorkingTree, &renamed);
        assert!(rename_diff.error.is_none(), "{:?}", rename_diff.error);
        assert!(
            rename_diff
                .hunks
                .iter()
                .flat_map(|h| h.lines.iter())
                .any(|l| l.text == "renamed"),
            "rename diff lost content: {:?}",
            rename_diff
        );
        assert_eq!(rename_diff.previous_path.as_deref(), Some("old.txt"));

        let binary = snapshot
            .files
            .iter()
            .find(|f| f.path == "img.png")
            .expect("binary")
            .clone();
        assert!(binary.binary, "snapshot should mark the png binary");
        let binary_diff = FileDiff::load(repo.path(), &ReviewScope::WorkingTree, &binary);
        assert!(binary_diff.error.is_none(), "{:?}", binary_diff.error);
        assert!(
            binary_diff.binary,
            "paging loader should preserve binary flag"
        );
        let binary_page = binary_diff
            .slice_page(&snapshot.snapshot_id, 0, 100)
            .expect("binary page");
        assert!(binary_page.binary);
        assert!(
            binary_page.hunks.is_empty(),
            "binary file has no text hunks"
        );
    }

    #[test]
    fn load_file_diff_two_revision_scope_paginates_single_file() {
        let repo = init_repo();
        git(repo.path(), &["branch", "review-base"]);
        write(repo.path().join("paged.txt").as_path(), "base\n");
        git(repo.path(), &["add", "paged.txt"]);
        git(repo.path(), &["commit", "-m", "add paged"]);
        git(repo.path(), &["branch", "review-target"]);
        let scope = ReviewScope::Revisions {
            from: "review-base".to_owned(),
            to: "review-target".to_owned(),
        };
        let snapshot = load_review_snapshot_for(repo.path(), scope.clone());
        let file = snapshot
            .files
            .iter()
            .find(|f| f.path == "paged.txt")
            .expect("paged")
            .clone();
        let diff = FileDiff::load(repo.path(), &scope, &file);
        assert!(diff.error.is_none(), "{:?}", diff.error);
        let page = diff
            .slice_page(&snapshot.snapshot_id, 0, 100)
            .expect("page");
        assert!(
            page.hunks
                .iter()
                .flat_map(|h| h.lines.iter())
                .any(|l| l.text == "base"),
            "revision diff lost the baseline line"
        );
        assert!(!page.has_more);
    }

    #[test]
    fn revision_snapshot_pages_stay_pinned_after_refs_move() {
        let repo = init_repo();
        git(repo.path(), &["branch", "review-base"]);
        write(
            repo.path().join("paged.txt").as_path(),
            "original target\nsecond line\n",
        );
        git(repo.path(), &["add", "paged.txt"]);
        git(repo.path(), &["commit", "-m", "original review target"]);
        git(repo.path(), &["branch", "review-target"]);
        let scope = ReviewScope::Revisions {
            from: "review-base".to_owned(),
            to: "review-target".to_owned(),
        };
        let (snapshot, paging) = load_review_snapshot_capture(repo.path(), scope.clone());
        let file = snapshot
            .files
            .iter()
            .find(|file| file.path == "paged.txt")
            .expect("paged");

        write(
            repo.path().join("paged.txt").as_path(),
            "moved target\nreplacement\n",
        );
        git(repo.path(), &["add", "paged.txt"]);
        git(repo.path(), &["commit", "-m", "move review target"]);
        let moved =
            String::from_utf8(git_output(repo.path(), &["rev-parse", "HEAD"])).expect("moved oid");
        git(
            repo.path(),
            &[
                "branch",
                "-f",
                "review-target",
                strip_output_line_ending(&moved),
            ],
        );

        let diff = FileDiff::load_snapshot_file(repo.path(), &scope, &paging, file);
        assert!(diff.error.is_none(), "{:?}", diff.error);
        let first = diff
            .slice_page(&snapshot.snapshot_id, 0, 1)
            .expect("first page");
        let later = diff
            .slice_page(
                &snapshot.snapshot_id,
                first.next_cursor.expect("later cursor"),
                MAX_FILE_PAGE_LINES,
            )
            .expect("later page");
        let text = first
            .hunks
            .iter()
            .chain(&later.hunks)
            .flat_map(|hunk| hunk.lines.iter())
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>();
        assert!(text.contains(&"original target"), "{text:?}");
        assert!(text.contains(&"second line"), "{text:?}");
        assert!(!text.contains(&"moved target"), "{text:?}");
        assert!(!text.contains(&"replacement"), "{text:?}");
        assert_eq!(first.snapshot_id, snapshot.snapshot_id);
        assert_eq!(later.snapshot_id, snapshot.snapshot_id);
        assert_eq!(snapshot.comparison_label(), "review-base → review-target");
    }
    #[cfg(unix)]
    #[test]
    fn revision_snapshot_pages_distinguish_literal_octal_text_and_invalid_byte_paths() {
        use std::os::unix::ffi::OsStringExt;

        let repo = init_repo();
        git(repo.path(), &["branch", "review-base"]);
        let invalid = OsString::from_vec(b"collision-\xff.txt".to_vec());
        let literal = OsString::from("collision-\\377.txt");
        fs::write(repo.path().join(&invalid), "invalid byte body\n").expect("write invalid path");
        fs::write(repo.path().join(&literal), "literal octal body\n").expect("write literal path");
        let status = Command::new("git")
            .arg("add")
            .arg("--")
            .arg(&invalid)
            .arg(&literal)
            .current_dir(repo.path())
            .status()
            .expect("git add collision paths");
        assert!(status.success());
        git(
            repo.path(),
            &["commit", "-m", "add colliding display paths"],
        );
        git(repo.path(), &["branch", "review-target"]);

        let scope = ReviewScope::Revisions {
            from: "review-base".to_owned(),
            to: "review-target".to_owned(),
        };
        let (snapshot, paging) = load_review_snapshot_capture(repo.path(), scope.clone());
        let invalid_display = r"collision-\377.txt";
        let literal_display = r"collision-\\377.txt";
        assert_ne!(invalid_display, literal_display);
        assert_eq!(
            snapshot
                .files
                .iter()
                .filter(|file| {
                    matches!(
                        file.path.as_str(),
                        r"collision-\377.txt" | r"collision-\\377.txt"
                    )
                })
                .count(),
            2
        );

        for (display, body) in [
            (invalid_display, "invalid byte body"),
            (literal_display, "literal octal body"),
        ] {
            let file = snapshot
                .files
                .iter()
                .find(|file| file.path == display)
                .expect("catalog file");
            let diff = FileDiff::load_snapshot_file(repo.path(), &scope, &paging, file);
            assert!(diff.error.is_none(), "{display}: {:?}", diff.error);
            let page = diff
                .slice_page(&snapshot.snapshot_id, 0, 100)
                .expect("page");
            assert_eq!(page.path, display);
            assert!(
                page.hunks
                    .iter()
                    .flat_map(|hunk| &hunk.lines)
                    .any(|line| { line.kind == DiffLineKind::Addition && line.text == body }),
                "{display}: {:?}",
                page.hunks
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn revision_snapshot_pages_invalid_utf8_path_with_catalog_bytes() {
        use std::os::unix::ffi::OsStringExt;

        let repo = init_repo();
        git(repo.path(), &["branch", "review-base"]);
        let name = OsString::from_vec(b"invalid-\xff-name.txt".to_vec());
        let path = repo.path().join(&name);
        fs::write(&path, "invalid path body\n").expect("write invalid path");
        let mut add = Command::new("git");
        add.arg("add").arg("--").arg(&name).current_dir(repo.path());
        assert!(add.status().expect("git add invalid path").success());
        git(repo.path(), &["commit", "-m", "add invalid path"]);
        git(repo.path(), &["branch", "review-target"]);

        let scope = ReviewScope::Revisions {
            from: "review-base".to_owned(),
            to: "review-target".to_owned(),
        };
        let (snapshot, paging) = load_review_snapshot_capture(repo.path(), scope.clone());
        let display = r"invalid-\377-name.txt";
        let file = snapshot
            .files
            .iter()
            .find(|file| file.path == display)
            .expect("invalid path catalog association");
        let diff = FileDiff::load_snapshot_file(repo.path(), &scope, &paging, file);
        assert!(diff.error.is_none(), "{:?}", diff.error);
        let page = diff
            .slice_page(&snapshot.snapshot_id, 0, 100)
            .expect("invalid path page");
        assert_eq!(page.path, display);
        assert!(page.hunks.iter().flat_map(|hunk| &hunk.lines).any(|line| {
            line.kind == DiffLineKind::Addition && line.text == "invalid path body"
        }));
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
    fn numstat_entries_parses_counts_renames_binary_and_raw_tab_paths() {
        let stdout = b"2\t1\tsrc/a.rs\00\t0\t\0old.txt\0renamed.txt\0-\t-\tblob.bin\05\t0\tweird\tname.txt\0";
        let entries = numstat_entries(stdout).expect("numstat parse");
        assert_eq!(entries.len(), 4);
        assert_eq!(entries["src/a.rs"].insertions, 2);
        assert_eq!(entries["src/a.rs"].deletions, 1);
        assert!(!entries["src/a.rs"].binary);
        assert_eq!(entries["renamed.txt"].insertions, 0);
        assert_eq!(entries["renamed.txt"].deletions, 0);
        assert!(!entries.contains_key("old.txt"));
        assert!(entries["blob.bin"].binary);
        let raw = numstat_entries(b"3\t0\tbad\xffname\0")
            .expect("non-UTF-8 path must parse");
        let (raw_path, raw_entry) = raw.iter().next().expect("non-UTF-8 path key");
        assert!(raw_path.starts_with("bad"), "{raw_path:?}");
        assert_eq!(decode_path_bytes(raw_path), b"bad\xffname");
        assert_eq!(raw_entry.insertions, 3);
        let raw_rename =
            numstat_entries(b"1\t1\t\0old\xff\0new\xff\0").expect("non-UTF-8 rename parse");
        let (rename_key, _) = raw_rename.iter().next().expect("rename key");
        assert_eq!(decode_path_bytes(rename_key), b"new\xff");
        assert!(!raw_rename.contains_key(&encode_path_bytes(b"old\xff")));
        assert_eq!(entries["blob.bin"].insertions, 0);
        assert_eq!(entries["blob.bin"].deletions, 0);
        assert_eq!(entries["weird\tname.txt"].insertions, 5);
        assert!(numstat_entries(b"onlyone\0").is_err());
        assert!(numstat_entries(b"x\ty\tpath\0").is_err());
        assert!(numstat_entries(b"1\t1\0").is_err());
        assert!(numstat_entries(b"2\t1\tsrc/a.rs").is_err());
        assert!(numstat_entries(b"1\t1\t\0old.txt\0renamed.txt").is_err());
    }

    #[test]
    fn snapshot_fails_closed_on_truncated_or_erroring_numstat() {
        let patch = b"diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-old\n+new\n".to_vec();
        let truncated_numstat = GitOutput {
            stdout: b"1\t1\ta.txt\0".to_vec(),
            truncated: true,
            error: None,
        };
        let snapshot = snapshot_from_diff_output(
            PathBuf::from("repo"),
            ReviewScope::WorkingTree,
            b"working-tree",
            GitOutput {
                stdout: patch.clone(),
                truncated: false,
                error: None,
            },
            None,
            &truncated_numstat,
            None,
            None,
        );
        let error = snapshot.error.expect("truncated numstat must fail the snapshot");
        assert!(error.contains("summary"), "{error}");
        assert!(!snapshot.summary_complete);
        let failing_numstat = GitOutput {
            stdout: Vec::new(),
            truncated: false,
            error: Some("numstat failed".to_owned()),
        };
        let snapshot = snapshot_from_diff_output(
            PathBuf::from("repo"),
            ReviewScope::WorkingTree,
            b"working-tree",
            GitOutput {
                stdout: patch,
                truncated: false,
                error: None,
            },
            None,
            &failing_numstat,
            None,
            None,
        );
        assert_eq!(snapshot.error.as_deref(), Some("numstat failed"));
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
        let numstat = GitOutput {
            stdout: b"2\t1\tsrc/a.rs\00\t0\t\0old2.txt\0new2.txt\00\t1\tsrc/b.rs\05\t0\tadded.txt\00\t1\tdeleted.txt\0".to_vec(),
            truncated: false,
            error: None,
        };
        let snapshot = snapshot_from_diff_output(
            PathBuf::from("repo"),
            ReviewScope::WorkingTree,
            b"working-tree",
            output,
            Some(&catalog),
            &numstat,
            None,
            None,
        );
        assert!(snapshot.truncated);
        assert!(snapshot.summary_complete);
        assert_eq!(snapshot.total_insertions, 7);
        assert_eq!(snapshot.total_deletions, 3);
        let paths = snapshot
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            [
                "src/a.rs",
                "new2.txt",
                "src/b.rs",
                "added.txt",
                "deleted.txt"
            ]
        );
        // Parsed entries keep hunks, status, and provenance untouched.
        let a = &snapshot.files[0];
        assert_eq!(a.path, "src/a.rs");
        assert_eq!(a.body_state(), "complete");
        assert_eq!(a.insertions, 2);
        assert_eq!(a.deletions, 1);
        assert_eq!(a.hunks.len(), 1);
        let renamed = &snapshot.files[1];
        assert_eq!(renamed.path, "new2.txt");
        assert_eq!(renamed.status, FileStatus::Renamed);
        assert_eq!(renamed.previous_path.as_deref(), Some("old2.txt"));
        assert_eq!(
            renamed.body_state(),
            "complete",
            "fully parsed rename keeps its parsed state"
        );
        let b = &snapshot.files[2];
        assert!(b.truncated);
        assert_eq!(b.message.as_deref(), Some(PREVIEW_CUT_MESSAGE));
        assert_eq!(b.body_state(), "preview");
        assert_eq!(b.hunks.len(), 1);
        assert_eq!(b.insertions, 0);
        assert_eq!(b.deletions, 1);
        // Absent catalogued paths become on-demand placeholders.
        let added = &snapshot.files[3];
        assert_eq!(added.path, "added.txt");
        assert_eq!(added.status, FileStatus::Added);
        assert_eq!(added.previous_path, None);
        assert!(added.hunks.is_empty());
        assert!(!added.truncated);
        assert_eq!(added.body_state(), "onDemand");
        assert_eq!(added.message.as_deref(), Some(PLACEHOLDER_MESSAGE));
        assert_eq!(added.insertions, 5);
        let deleted = &snapshot.files[4];
        assert_eq!(deleted.status, FileStatus::Deleted);
        assert!(deleted.hunks.is_empty());
        assert_eq!(deleted.body_state(), "onDemand");
        assert_eq!(deleted.message.as_deref(), Some(PLACEHOLDER_MESSAGE));
        assert_eq!(deleted.deletions, 1);
        // A rename source only ever appears as provenance, never as a
        // placeholder, even when the catalog lists it separately.
        assert!(!snapshot.files.iter().any(|file| file.path == "old2.txt"));
    }

    #[test]
    fn complete_hunk_counts_win_over_inflated_numstat() {
        let patch = b"diff --git a/small.txt b/small.txt\n--- a/small.txt\n+++ b/small.txt\n@@ -1,3 +1,3 @@\n keep\n-old\n+new\n context\n".to_vec();
        let snapshot = snapshot_from_diff_output(
            PathBuf::from("repo"),
            ReviewScope::WorkingTree,
            b"working-tree",
            GitOutput {
                stdout: patch,
                truncated: false,
                error: None,
            },
            None,
            &GitOutput {
                stdout: b"6479\t151\tsmall.txt\0".to_vec(),
                truncated: false,
                error: None,
            },
            None,
            None,
        );
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        assert_eq!(snapshot.files.len(), 1);
        assert_eq!(snapshot.files[0].hunks.len(), 1);
        assert_eq!(snapshot.files[0].insertions, 1);
        assert_eq!(snapshot.files[0].deletions, 1);
        assert_eq!(snapshot.files[0].displayed_change_stats(), (1, 1));
        assert_eq!(snapshot.total_insertions, 1);
        assert_eq!(snapshot.total_deletions, 1);
        let tree = FileTree::from_snapshot(&snapshot);
        let file_node = tree
            .nodes
            .iter()
            .find(|node| node.path == "small.txt")
            .expect("file row");
        assert_eq!(file_node.insertions, 1);
        assert_eq!(file_node.deletions, 1);
    }

    #[test]
    fn from_output_never_serves_another_files_hunks() {
        // Two-file patch; the requested identity is the SECOND file. The
        // removed first-file fallback would serve Cargo.toml's hunks under
        // mod.rs and break comment identity downstream.
        let patch = b"diff --git a/Cargo.toml b/Cargo.toml\n--- a/Cargo.toml\n+++ b/Cargo.toml\n@@ -1,2 +1,3 @@\n deps\n+new-dep\n context\ndiff --git a/mod.rs b/mod.rs\n--- a/mod.rs\n+++ b/mod.rs\n@@ -40,1 +40,2 @@\n ctx\n+added\n".to_vec();
        let parsed = parse_unified_diff(&String::from_utf8_lossy(&patch));
        assert_eq!(parsed.len(), 2);
        let request = DiffFile {
            path: "mod.rs".into(),
            previous_path: None,
            status: FileStatus::Modified,
            binary: false,
            insertions: 0,
            deletions: 0,
            hunks: parsed[1].hunks.clone(),
            truncated: false,
            message: None,
        };
        let served = FileDiff::from_output(
            &request,
            GitOutput {
                stdout: patch,
                truncated: false,
                error: None,
            },
        );
        assert!(served.error.is_none(), "{:?}", served.error);
        assert_eq!(served.path, "mod.rs");
        assert_eq!(served.hunks[0].old_start, 40);
        let texts: Vec<&str> = served
            .hunks
            .iter()
            .flat_map(|hunk| &hunk.lines)
            .map(|line| line.text.as_str())
            .collect();
        assert!(texts.contains(&"added"), "{texts:?}");
        assert!(!texts.contains(&"new-dep"), "{texts:?}");
    }

    #[test]
    fn from_output_absent_path_fails_closed_without_foreign_hunks() {
        let patch = b"diff --git a/Cargo.toml b/Cargo.toml\n--- a/Cargo.toml\n+++ b/Cargo.toml\n@@ -1,2 +1,3 @@\n deps\n+new-dep\n context\n".to_vec();
        let request = DiffFile {
            path: "mod.rs".into(),
            previous_path: None,
            status: FileStatus::Modified,
            binary: false,
            insertions: 0,
            deletions: 0,
            hunks: Vec::new(),
            truncated: false,
            message: None,
        };
        let served = FileDiff::from_output(
            &request,
            GitOutput {
                stdout: patch,
                truncated: false,
                error: None,
            },
        );
        assert!(served.error.is_some(), "{:?}", served.error);
        assert!(served.hunks.is_empty(), "{:?}", served.hunks);
    }

    #[test]
    fn from_single_file_output_binds_no_index_capture_identity() {
        // `git diff --no-index /dev/null <capture>` renders the CAPTURE path,
        // never the snapshot identity: the producer rebinds it here.
        let patch = b"diff --git a/dev/null b/tmp/capture.body\nnew file mode 100644\n--- /dev/null\n+++ b/tmp/capture.body\n@@ -0,0 +1,2 @@\n+one\n+two\n".to_vec();
        let request = DiffFile {
            path: "brand-new.txt".into(),
            previous_path: None,
            status: FileStatus::Added,
            binary: false,
            insertions: 0,
            deletions: 0,
            hunks: Vec::new(),
            truncated: false,
            message: None,
        };
        let served = FileDiff::from_single_file_output(
            &request,
            GitOutput {
                stdout: patch,
                truncated: false,
                error: None,
            },
        );
        assert!(served.error.is_none(), "{:?}", served.error);
        assert_eq!(served.path, "brand-new.txt");
        assert_eq!(served.status, FileStatus::Added);
        let texts: Vec<&str> = served
            .hunks
            .iter()
            .flat_map(|hunk| &hunk.lines)
            .map(|line| line.text.as_str())
            .collect();
        assert_eq!(texts, vec!["one", "two"], "{texts:?}");

        // A multi-record body breaks the producer contract and fails closed.
        let multi = b"diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-old\n+new\ndiff --git a/b.txt b/b.txt\n--- a/b.txt\n+++ b/b.txt\n@@ -1 +1 @@\n-x\n+y\n".to_vec();
        let rejected = FileDiff::from_single_file_output(
            &request,
            GitOutput {
                stdout: multi,
                truncated: false,
                error: None,
            },
        );
        assert!(rejected.error.is_some(), "{:?}", rejected.error);
        assert!(rejected.hunks.is_empty());
    }

    #[test]
    fn truncated_snapshot_identity_includes_catalog_bytes() {
        let patch = || GitOutput {
            stdout:
                b"diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-old\n+new\n"
                    .to_vec(),
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
            &empty_numstat(),
            None,
            None,
        );
        let snapshot_b = snapshot_from_diff_output(
            PathBuf::from("repo"),
            ReviewScope::WorkingTree,
            b"working-tree",
            patch(),
            Some(&catalog_b),
            &empty_numstat(),
            None,
            None,
        );
        let snapshot_a_again = snapshot_from_diff_output(
            PathBuf::from("repo"),
            ReviewScope::WorkingTree,
            b"working-tree",
            patch(),
            Some(&catalog_a),
            &empty_numstat(),
            None,
            None,
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

    #[test]
    fn truncated_working_tree_identity_tracks_review_path_content() {
        // A staged addition whose diff lies ENTIRELY beyond the combined
        // patch's 2 MiB cut: the name-status catalog (which paths changed)
        // and the truncated patch bytes (identical cut point) are the same
        // across content variants, so only the post-staging index fingerprint
        // can tell the snapshot ids apart.
        let repo = init_repo();
        let mut first = vec![b'a'; 3 * 1024 * 1024];
        let mut second = first.clone();
        second[3 * 1024 * 1024 - 9] = b'b'; // differs only beyond the 2 MiB cut
        fs::write(repo.path().join("zz-late.bin"), &first).expect("write staged addition");
        git(repo.path(), &["add", "zz-late.bin"]);
        let snapshot_a = load_review_snapshot(repo.path());
        assert!(snapshot_a.error.is_none(), "{:?}", snapshot_a.error);
        assert!(
            snapshot_a.truncated,
            "3 MiB addition must exceed the snapshot cap"
        );
        // A pure UNSTAGED worktree edit beyond the cut (staged blob unchanged):
        // the review shows the worktree content, so the id must flip.
        fs::write(repo.path().join("zz-late.bin"), &second).expect("write worktree edit");
        let snapshot_b = load_review_snapshot(repo.path());
        assert!(snapshot_b.error.is_none(), "{:?}", snapshot_b.error);
        assert_ne!(
            snapshot_a.snapshot_id, snapshot_b.snapshot_id,
            "worktree content beyond the truncation cut must flip the identity"
        );
        // Re-staging the SAME content does not change the review: idempotent.
        git(repo.path(), &["add", "zz-late.bin"]);
        let snapshot_c = load_review_snapshot(repo.path());
        assert!(snapshot_c.error.is_none(), "{:?}", snapshot_c.error);
        assert_eq!(
            snapshot_b.snapshot_id, snapshot_c.snapshot_id,
            "identical review content must keep the identical identity"
        );
        // Determinism: identical state reproduces the identical identity.
        let snapshot_c_again = load_review_snapshot(repo.path());
        assert_eq!(snapshot_c.snapshot_id, snapshot_c_again.snapshot_id);
    }

    /// Repo whose combined HEAD→working-tree diff exceeds the 2 MiB snapshot
    /// cap. Two ~2.6 MiB single-file diffs (verified path-sorted output:
    /// `big-a.txt` < `big-b.txt` < `rename-new.txt` < `zz-later.txt`) mean the
    /// first file consumes the whole cap, so every later file is a guaranteed
    /// on-demand placeholder regardless of git's exact output order. The
    /// untracked `stray.txt` joins the review as a fully captured Added entry.
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
        // The rename is staged (`git mv`) so both sides are tracked changed
        // paths; a bare filesystem rename whose destination stays untracked
        // is out of the tracked-only review contract.
        git(repo.path(), &["mv", "rename-old.txt", "rename-new.txt"]);
        // A small content edit keeps the rename paired (above the 50%
        // similarity threshold) while giving the loaded diff hunk lines.
        write(
            repo.path().join("rename-new.txt").as_path(),
            &format!("{baseline}renamed\n"),
        );
        write(
            repo.path().join("zz-later.txt").as_path(),
            "base\nchanged\n",
        );
        write(
            repo.path().join("stray.txt").as_path(),
            "untracked, now reviewed\n",
        );
        repo
    }

    #[test]
    fn load_review_snapshot_truncated_patch_lists_every_changed_file() {
        let repo = build_truncated_working_tree_repo();
        let index = repo.path().join(".git/index");
        let index_before = fs::read(&index).expect("index before");

        let snapshot = load_review_snapshot(repo.path());

        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        assert!(
            snapshot.truncated,
            "combined patch must exceed the 2 MiB cap"
        );
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
        assert!(
            paths.contains(&"stray.txt"),
            "untracked file joins the review: {paths:?}"
        );
        // The first big file parses partially; the second big file, the rename
        // and the later file become on-demand placeholders.
        let parsed = snapshot
            .files
            .iter()
            .filter(|file| !file.hunks.is_empty())
            .collect::<Vec<_>>();
        assert_eq!(parsed.len(), 2, "{paths:?}");
        assert!(
            parsed[0].truncated && parsed[0].body_state() == "preview",
            "partially parsed file must carry the internal preview marker"
        );
        assert_eq!(parsed[0].message.as_deref(), Some(PREVIEW_CUT_MESSAGE));
        let placeholders = snapshot
            .files
            .iter()
            .filter(|file| file.hunks.is_empty())
            .collect::<Vec<_>>();
        assert_eq!(placeholders.len(), 3, "{paths:?}");
        for placeholder in placeholders {
            assert!(
                !placeholder.truncated,
                "placeholder is on-demand, never a hard truncation"
            );
            assert_eq!(placeholder.body_state(), "onDemand");
            assert_eq!(placeholder.message.as_deref(), Some(PLACEHOLDER_MESSAGE));
        }
        assert!(snapshot.summary_complete);
        assert_eq!(
            snapshot.total_insertions(),
            snapshot.files.iter().map(|file| file.insertions).sum::<usize>()
        );
        assert_eq!(
            snapshot.total_deletions(),
            snapshot.files.iter().map(|file| file.deletions).sum::<usize>()
        );
        let big_a = snapshot
            .files
            .iter()
            .find(|file| file.path == "big-a.txt")
            .expect("big-a entry");
        assert_eq!(big_a.insertions, 200_000);
        assert_eq!(big_a.deletions, 1);
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
            let page = diff
                .slice_page(&snapshot.snapshot_id, 0, 100)
                .expect("page");
            assert_eq!(page.path, file.path, "page serves the requested file");
            let loaded_lines: usize = diff.hunks.iter().map(|h| h.lines.len()).sum();
            if file.path.starts_with("big-") {
                assert!(
                    loaded_lines > MAX_FILE_RENDER_LINES,
                    "big placeholder must recover the full body: {} lines",
                    loaded_lines
                );
            } else {
                assert!(
                    !diff.hunks.is_empty(),
                    "small placeholder must load its lines"
                );
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
        let from =
            String::from_utf8(git_output(repo.path(), &["rev-parse", "HEAD"])).expect("from hash");
        let from = strip_output_line_ending(&from).to_owned();
        let changed = "changed-line\n".repeat(200_000);
        write(repo.path().join("big-a.txt").as_path(), &changed);
        write(repo.path().join("big-b.txt").as_path(), &changed);
        git(repo.path(), &["mv", "rename-old.txt", "rename-new.txt"]);
        write(
            repo.path().join("rename-new.txt").as_path(),
            &format!("{baseline}renamed\n"),
        );
        write(
            repo.path().join("zz-later.txt").as_path(),
            "base\nchanged\n",
        );
        git(repo.path(), &["add", "-A"]);
        git(repo.path(), &["commit", "-m", "truncation changes"]);

        let scope = ReviewScope::Revisions {
            from,
            to: "HEAD".to_owned(),
        };
        let snapshot = load_review_snapshot_for(repo.path(), scope.clone());

        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        assert!(
            snapshot.truncated,
            "combined patch must exceed the 2 MiB cap"
        );
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
            assert!(!placeholder.truncated);
            assert_eq!(placeholder.body_state(), "onDemand");
            assert_eq!(placeholder.message.as_deref(), Some(PLACEHOLDER_MESSAGE));
        }
        assert!(snapshot.summary_complete);
        assert_eq!(
            snapshot.total_insertions(),
            snapshot.files.iter().map(|file| file.insertions).sum::<usize>()
        );
        assert_eq!(
            snapshot.total_deletions(),
            snapshot.files.iter().map(|file| file.deletions).sum::<usize>()
        );
        let renamed = snapshot
            .files
            .iter()
            .find(|file| file.path == "rename-new.txt")
            .expect("rename placeholder");
        assert_eq!(renamed.status, FileStatus::Renamed);
        assert_eq!(renamed.previous_path.as_deref(), Some("rename-old.txt"));

        // The revision-scope placeholder loads through the paging loader too.
        let (_, paging) = load_review_snapshot_capture(repo.path(), scope.clone());
        for file in snapshot.files.iter().filter(|file| file.hunks.is_empty()) {
            let diff = FileDiff::load_snapshot_file(repo.path(), &scope, &paging, file);
            assert!(diff.error.is_none(), "{}: {:?}", file.path, diff.error);
            let page = diff
                .slice_page(&snapshot.snapshot_id, 0, 100)
                .expect("page");
            assert_eq!(page.path, file.path);
            assert!(!diff.hunks.is_empty(), "placeholder must load its lines");
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
            write(
                repo.path().join(format!("{dir}f{index:04}.txt")).as_path(),
                "base\n",
            );
        }
        git(repo.path(), &["add", "-A"]);
        git(repo.path(), &["commit", "-m", "deep baseline"]);
        let from =
            String::from_utf8(git_output(repo.path(), &["rev-parse", "HEAD"])).expect("from hash");
        let from = strip_output_line_ending(&from).to_owned();
        for index in 0..620 {
            write(
                repo.path().join(format!("{dir}f{index:04}.txt")).as_path(),
                "changed\n",
            );
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
            total_insertions: 0,
            total_deletions: 0,
            summary_complete: true,
            error: None,
        };
        let tree = FileTree::from_snapshot(&snapshot);
        // Default expanded: all five rows (src, a.rs, b.rs, docs, readme.md).
        assert_eq!(tree.visible_rows().len(), 5);
        let src_idx = tree
            .visible_rows()
            .iter()
            .find(|r| tree.nodes[r.node_index].path == "src")
            .expect("src row")
            .node_index;
        let mut collapsed = tree.clone();
        collapsed.set_collapsed(src_idx, true);
        assert!(collapsed.is_collapsed(src_idx));
        assert_eq!(
            collapsed.visible_rows().len(),
            3,
            "collapsing src hides its two files"
        );
        collapsed.set_collapsed(src_idx, false);
        assert_eq!(collapsed.visible_rows().len(), 5);
        // Toggling a file row is a no-op.
        let file_idx = tree
            .visible_rows()
            .iter()
            .find(|r| tree.nodes[r.node_index].path == "src/a.rs")
            .expect("file row")
            .node_index;
        let mut toggled = tree.clone();
        toggled.toggle_collapse(file_idx);
        assert_eq!(toggled.visible_rows().len(), 5);
    }
}
