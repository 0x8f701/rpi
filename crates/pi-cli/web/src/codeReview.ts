/**
 * Code-review RPC wire normalization — pure helpers shared by
 * panels/CodeReviewPanel.tsx and scripts/codeReview.test.ts.
 *
 * Backend projects camelCase ReviewSnapshot + controller state (see
 * code_review_open/snapshot/refresh/comment/abort/close). Absolute
 * ReviewSnapshot.root is NEVER on the wire. These helpers only coerce
 * unknown JSON into a stable view model so a malformed payload never
 * throws in the panel.
 */

export type DiffLineKind = 'context' | 'addition' | 'deletion' | 'meta';

export type FileStatus =
  | 'added'
  | 'deleted'
  | 'modified'
  | 'renamed'
  | 'copied'
  | 'binary'
  | 'changed';

/**
 * Compact one-letter status glyphs for the file tree, mirroring the TUI
 * (crates/pi-cli/src/code_review.rs::FileStatus::as_str): A/D/M/R/C/B/?.
 */
export const FILE_STATUS_LETTERS: Record<FileStatus, string> = {
  added: 'A',
  deleted: 'D',
  modified: 'M',
  renamed: 'R',
  copied: 'C',
  binary: 'B',
  changed: '?',
};

/** Compact tree glyph for a file status; defensive fallback for unknown values. */
export function fileStatusLetter(status: FileStatus): string {
  return FILE_STATUS_LETTERS[status] ?? '?';
}

export interface HunkIdentity {
  snapshotId: string;
  path: string;
  oldStart: number;
  oldCount: number;
  newStart: number;
  newCount: number;
  contentHash: string;
}

export interface DiffLine {
  kind: DiffLineKind;
  oldNo?: number;
  newNo?: number;
  text: string;
}

export interface DiffHunk {
  header: string;
  oldStart: number;
  oldCount: number;
  newStart: number;
  newCount: number;
  contentHash: string;
  lines: DiffLine[];
}

export interface DiffFile {
  path: string;
  previousPath?: string;
  status: FileStatus;
  binary: boolean;
  insertions: number;
  deletions: number;
  truncated: boolean;
  message?: string;
  hunks: DiffHunk[];
}

export interface ReviewComment {
  role: 'user' | 'assistant' | 'system';
  text: string;
  partial: boolean;
}

export interface ReviewThread {
  identity: HunkIdentity;
  comments: ReviewComment[];
  streamingText: string;
  error: string | null;
  stale: boolean;
}

export interface CodeReviewSnapshot {
  comparisonLabel: string;
  snapshotId: string;
  truncated: boolean;
  error: string | null;
  totalInsertions: number;
  totalDeletions: number;
  files: DiffFile[];
  threads: ReviewThread[];
  isStreaming: boolean;
  activeHunk: HunkIdentity | null;
}

const LINE_KINDS: Record<string, true> = {
  context: true,
  addition: true,
  deletion: true,
  meta: true,
};

const FILE_STATUSES: Record<string, true> = {
  added: true,
  deleted: true,
  modified: true,
  renamed: true,
  copied: true,
  binary: true,
  changed: true,
};

const COMMENT_ROLES: Record<string, true> = {
  user: true,
  assistant: true,
  system: true,
};

function asRecord(value: unknown): Record<string, unknown> | null {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return null;
  return value as Record<string, unknown>;
}

function stringField(value: unknown): string {
  return typeof value === 'string' ? value : '';
}

function optionalString(value: unknown): string | undefined {
  return typeof value === 'string' && value.length > 0 ? value : undefined;
}

function boolField(value: unknown, fallback = false): boolean {
  return typeof value === 'boolean' ? value : fallback;
}

/** Coerce to a finite non-negative integer; non-numbers / NaN / negative → 0. */
function uintField(value: unknown): number {
  if (typeof value !== 'number' || !Number.isFinite(value)) return 0;
  if (value < 0) return 0;
  return Math.floor(value);
}

function nullableString(value: unknown): string | null {
  if (value == null) return null;
  return typeof value === 'string' ? value : null;
}

function normalizeIdentity(raw: unknown): HunkIdentity | null {
  const obj = asRecord(raw);
  if (!obj) return null;
  const path = stringField(obj.path);
  const contentHash = stringField(obj.contentHash);
  const snapshotId = stringField(obj.snapshotId);
  // path + contentHash are the minimum identity surface a comment can target;
  // missing either means the wire entry is unusable.
  if (!path || !contentHash) return null;
  return {
    snapshotId,
    path,
    oldStart: uintField(obj.oldStart),
    oldCount: uintField(obj.oldCount),
    newStart: uintField(obj.newStart),
    newCount: uintField(obj.newCount),
    contentHash,
  };
}

function normalizeLine(raw: unknown): DiffLine | null {
  const obj = asRecord(raw);
  if (!obj) return null;
  const kindRaw = stringField(obj.kind);
  const kind = kindRaw in LINE_KINDS ? (kindRaw as DiffLineKind) : 'meta';
  const line: DiffLine = {
    kind,
    text: stringField(obj.text),
  };
  if (typeof obj.oldNo === 'number' && Number.isFinite(obj.oldNo)) {
    line.oldNo = Math.floor(obj.oldNo);
  }
  if (typeof obj.newNo === 'number' && Number.isFinite(obj.newNo)) {
    line.newNo = Math.floor(obj.newNo);
  }
  return line;
}

function normalizeHunk(raw: unknown): DiffHunk | null {
  const obj = asRecord(raw);
  if (!obj) return null;
  const contentHash = stringField(obj.contentHash);
  if (!contentHash) return null;
  const linesRaw = Array.isArray(obj.lines) ? obj.lines : [];
  const lines: DiffLine[] = [];
  for (const entry of linesRaw) {
    const line = normalizeLine(entry);
    if (line) lines.push(line);
  }
  return {
    header: stringField(obj.header),
    oldStart: uintField(obj.oldStart),
    oldCount: uintField(obj.oldCount),
    newStart: uintField(obj.newStart),
    newCount: uintField(obj.newCount),
    contentHash,
    lines,
  };
}

function normalizeFile(raw: unknown): DiffFile | null {
  const obj = asRecord(raw);
  if (!obj) return null;
  const path = stringField(obj.path);
  if (!path) return null;
  const statusRaw = stringField(obj.status);
  const status = statusRaw in FILE_STATUSES ? (statusRaw as FileStatus) : 'changed';
  const hunksRaw = Array.isArray(obj.hunks) ? obj.hunks : [];
  const hunks: DiffHunk[] = [];
  for (const entry of hunksRaw) {
    const hunk = normalizeHunk(entry);
    if (hunk) hunks.push(hunk);
  }
  const file: DiffFile = {
    path,
    status,
    binary: boolField(obj.binary),
    insertions: uintField(obj.insertions),
    deletions: uintField(obj.deletions),
    truncated: boolField(obj.truncated),
    hunks,
  };
  const previousPath = optionalString(obj.previousPath);
  if (previousPath) file.previousPath = previousPath;
  const message = optionalString(obj.message);
  if (message) file.message = message;
  return file;
}

function normalizeComment(raw: unknown): ReviewComment | null {
  const obj = asRecord(raw);
  if (!obj) return null;
  const roleRaw = stringField(obj.role);
  const role = roleRaw in COMMENT_ROLES ? (roleRaw as ReviewComment['role']) : 'system';
  return {
    role,
    text: stringField(obj.text),
    partial: boolField(obj.partial),
  };
}

function normalizeThread(raw: unknown): ReviewThread | null {
  const obj = asRecord(raw);
  if (!obj) return null;
  const identity = normalizeIdentity(obj.identity);
  if (!identity) return null;
  const commentsRaw = Array.isArray(obj.comments) ? obj.comments : [];
  const comments: ReviewComment[] = [];
  for (const entry of commentsRaw) {
    const comment = normalizeComment(entry);
    if (comment) comments.push(comment);
  }
  return {
    identity,
    comments,
    streamingText: stringField(obj.streamingText),
    error: nullableString(obj.error),
    stale: boolField(obj.stale),
  };
}

/**
 * Normalize threads whether the wire sends an array or a map keyed by an
 * opaque id. Map values are normalized in insertion order; malformed entries
 * are dropped.
 */
export function normalizeThreads(raw: unknown): ReviewThread[] {
  if (Array.isArray(raw)) {
    const out: ReviewThread[] = [];
    for (const entry of raw) {
      const thread = normalizeThread(entry);
      if (thread) out.push(thread);
    }
    return out;
  }
  const obj = asRecord(raw);
  if (!obj) return [];
  const out: ReviewThread[] = [];
  for (const key of Object.keys(obj)) {
    const thread = normalizeThread(obj[key]);
    if (thread) out.push(thread);
  }
  return out;
}

/** Empty snapshot used when open/snapshot fails before any data arrives. */
export function emptyCodeReviewSnapshot(error: string | null = null): CodeReviewSnapshot {
  return {
    comparisonLabel: '',
    snapshotId: '',
    truncated: false,
    error,
    totalInsertions: 0,
    totalDeletions: 0,
    files: [],
    threads: [],
    isStreaming: false,
    activeHunk: null,
  };
}

/**
 * Normalize a code_review_* response payload into a stable CodeReviewSnapshot.
 * Returns a snapshot with `error` set (never throws) for unusable shapes so
 * the panel can always render an actionable state.
 */
export function normalizeCodeReviewSnapshot(data: unknown): CodeReviewSnapshot {
  const obj = asRecord(data);
  if (!obj) {
    return emptyCodeReviewSnapshot('Invalid code-review snapshot response');
  }

  const filesRaw = Array.isArray(obj.files) ? obj.files : [];
  const files: DiffFile[] = [];
  for (const entry of filesRaw) {
    const file = normalizeFile(entry);
    if (file) files.push(file);
  }

  const activeHunk = obj.activeHunk == null ? null : normalizeIdentity(obj.activeHunk);

  return {
    comparisonLabel: stringField(obj.comparisonLabel),
    snapshotId: stringField(obj.snapshotId),
    truncated: boolField(obj.truncated),
    error: nullableString(obj.error),
    totalInsertions: uintField(obj.totalInsertions),
    totalDeletions: uintField(obj.totalDeletions),
    files,
    threads: normalizeThreads(obj.threads),
    isStreaming: boolField(obj.isStreaming),
    activeHunk,
  };
}

/** Stable key for a hunk identity (path + ranges + content hash). */
export function hunkKey(
  identity: Pick<
    HunkIdentity,
    'path' | 'oldStart' | 'oldCount' | 'newStart' | 'newCount' | 'contentHash'
  >,
): string {
  return `${identity.path}\0${identity.oldStart},${identity.oldCount},${identity.newStart},${identity.newCount}\0${identity.contentHash}`;
}

/** Find the thread for a hunk, if any. */
export function findThreadForHunk(
  threads: ReviewThread[],
  identity: HunkIdentity,
): ReviewThread | undefined {
  const key = hunkKey(identity);
  return threads.find((thread) => hunkKey(thread.identity) === key);
}

/** Stable key for a hunk that belongs to a file, without an identity object. */
export function hunkKeyFor(file: Pick<DiffFile, 'path'>, hunk: DiffHunk): string {
  return hunkKey({
    path: file.path,
    oldStart: hunk.oldStart,
    oldCount: hunk.oldCount,
    newStart: hunk.newStart,
    newCount: hunk.newCount,
    contentHash: hunk.contentHash,
  });
}

/** Full comment identity for a hunk in a snapshot. */
export function hunkIdentityFor(
  snapshotId: string,
  file: Pick<DiffFile, 'path'>,
  hunk: DiffHunk,
): HunkIdentity {
  return {
    snapshotId,
    path: file.path,
    oldStart: hunk.oldStart,
    oldCount: hunk.oldCount,
    newStart: hunk.newStart,
    newCount: hunk.newCount,
    contentHash: hunk.contentHash,
  };
}

/** Number of comments on a thread (0 when there is no thread). */
export function countThreadComments(thread: ReviewThread | undefined): number {
  return thread ? thread.comments.length : 0;
}

/** Total comment count for a file across all its hunks (file-row badge). */
export function countFileThreads(threads: ReviewThread[], file: DiffFile): number {
  let total = 0;
  for (const hunk of file.hunks) {
    total += countThreadComments(findThreadForHunk(threads, hunkIdentityFor('', file, hunk)));
  }
  return total;
}

/**
 * Parse `/code-review` argument tail into open revisions.
 * Accepts 0 tokens (working tree) or exactly 2 (from, to).
 * Returns an error message string when the arity is wrong.
 */
export function parseCodeReviewArgs(
  args: string,
): { ok: true; from?: string; to?: string } | { ok: false; error: string } {
  const trimmed = args.trim();
  if (!trimmed) return { ok: true };
  const parts = trimmed.split(/\s+/).filter(Boolean);
  if (parts.length === 2) {
    return { ok: true, from: parts[0], to: parts[1] };
  }
  return {
    ok: false,
    error: 'usage: /code-review [from to] — pass zero or two revisions',
  };
}

// ---------------------------------------------------------------------------
// Collapsible file tree (mirrors crates/pi-cli/src/code_review.rs::FileTree).
// Default expanded; directories toggle; filter keeps matched files + ancestors.
// ---------------------------------------------------------------------------

export type TreeNodeKind = 'dir' | 'file';

export interface FileTreeNode {
  id: string;
  name: string;
  path: string;
  kind: TreeNodeKind;
  depth: number;
  children: number[];
  fileIndex?: number;
  status?: FileStatus;
  insertions: number;
  deletions: number;
}

export interface FileTree {
  nodes: FileTreeNode[];
  roots: number[];
  collapsed: Set<string>;
}

export interface VisibleTreeRow {
  nodeIndex: number;
  depth: number;
  expanded: boolean;
  isDir: boolean;
}

/** Build a collapsible path tree over the snapshot's files (default expanded). */
export function buildFileTree(files: DiffFile[]): FileTree {
  const nodes: FileTreeNode[] = [];
  const roots: number[] = [];
  const dirIndex: Map<string, number> = new Map();
  const ensureDir = (dirPath: string): number => {
    const existing = dirIndex.get(dirPath);
    if (existing !== undefined) return existing;
    const components = dirPath.split('/').filter((part) => part.length > 0);
    let parent: number | null = null;
    let built = '';
    for (let depth = 0; depth < components.length; depth++) {
      const component = components[depth];
      built = built.length === 0 ? component : `${built}/${component}`;
      const known = dirIndex.get(built);
      if (known !== undefined) {
        parent = known;
        continue;
      }
      const idx = nodes.length;
      nodes.push({
        id: `dir:${built}`,
        name: component,
        path: built,
        kind: 'dir',
        depth,
        children: [],
        insertions: 0,
        deletions: 0,
      });
      dirIndex.set(built, idx);
      if (parent !== null) nodes[parent].children.push(idx);
      else roots.push(idx);
      parent = idx;
    }
    return dirIndex.get(dirPath) ?? -1;
  };
  files.forEach((file, fileIndex) => {
    const rel = file.path;
    const slash = rel.lastIndexOf('/');
    const parentPath = slash > 0 ? rel.slice(0, slash) : null;
    const name = slash > 0 ? rel.slice(slash + 1) : rel;
    const parentIdx = parentPath !== null ? ensureDir(parentPath) : null;
    const depth = parentPath !== null ? parentPath.split('/').filter((s) => s.length > 0).length : 0;
    const idx = nodes.length;
    nodes.push({
      id: `file:${rel}`,
      name,
      path: rel,
      kind: 'file',
      depth,
      children: [],
      fileIndex,
      status: file.status,
      insertions: file.insertions,
      deletions: file.deletions,
    });
    if (parentIdx !== null && parentIdx >= 0) {
      nodes[parentIdx].children.push(idx);
      let walk: number | null = parentIdx;
      while (walk !== null) {
        nodes[walk].insertions = nodes[walk].insertions + file.insertions;
        nodes[walk].deletions = nodes[walk].deletions + file.deletions;
        const parentPathOf: string = nodes[walk].path;
        const parentSlash: number = parentPathOf.lastIndexOf('/');
        const grand: string | null = parentSlash > 0 ? parentPathOf.slice(0, parentSlash) : null;
        walk = grand !== null ? (dirIndex.get(grand) ?? null) : null;
      }
    } else {
      roots.push(idx);
    }
  });
  const orderKeys: Array<[boolean, string]> = nodes.map((n) => [
    n.kind === 'file',
    n.name.toLowerCase(),
  ]);
  for (const node of nodes) {
    node.children.sort((a, b) => (orderKeys[a] < orderKeys[b] ? -1 : orderKeys[a] > orderKeys[b] ? 1 : 0));
  }
  roots.sort((a, b) => (orderKeys[a] < orderKeys[b] ? -1 : orderKeys[a] > orderKeys[b] ? 1 : 0));
  return { nodes, roots, collapsed: new Set() };
}

export function treeToggleCollapse(tree: FileTree, nodeIndex: number): FileTree {
  const node = tree.nodes[nodeIndex];
  if (!node || node.kind !== 'dir') return tree;
  const collapsed = new Set(tree.collapsed);
  if (collapsed.has(node.id)) collapsed.delete(node.id);
  else collapsed.add(node.id);
  return { ...tree, collapsed };
}

/** Visible rows in depth-first order, honoring the collapse set. */
export function treeVisibleRows(tree: FileTree): VisibleTreeRow[] {
  const rows: VisibleTreeRow[] = [];
  const walk = (indices: number[]) => {
    for (const idx of indices) {
      const node = tree.nodes[idx];
      if (!node) continue;
      const isDir = node.kind === 'dir';
      const expanded = isDir && !tree.collapsed.has(node.id);
      rows.push({ nodeIndex: idx, depth: node.depth, expanded, isDir });
      if (expanded) walk(node.children);
    }
  };
  walk(tree.roots);
  return rows;
}

/**
 * Filter the tree to rows whose file path matches `query` (case-insensitive),
 * keeping every ancestor directory. All kept directories are forced expanded
 * regardless of the collapse set. An empty query returns the full visible rows.
 */
export function treeFilterRows(tree: FileTree, query: string): VisibleTreeRow[] {
  const needle = query.trim().toLowerCase();
  if (!needle) return treeVisibleRows(tree);
  const kept = new Set<number>();
  for (let i = 0; i < tree.nodes.length; i++) {
    const node = tree.nodes[i];
    if (node.kind === 'file' && node.path.toLowerCase().includes(needle)) {
      let walk: number | null = i;
      while (walk !== null) {
        kept.add(walk);
        const parentPath: string = tree.nodes[walk].path;
        const slash: number = parentPath.lastIndexOf('/');
        const grand: string | null = slash > 0 ? parentPath.slice(0, slash) : null;
        const grandIdx: number = grand !== null ? tree.nodes.findIndex((n) => n.path === grand && n.kind === 'dir') : -1;
        walk = grandIdx >= 0 ? grandIdx : null;
      }
    }
  }
  const rows: VisibleTreeRow[] = [];
  const walk = (indices: number[]) => {
    for (const idx of indices) {
      if (!kept.has(idx)) continue;
      const node = tree.nodes[idx];
      if (!node) continue;
      const isDir = node.kind === 'dir';
      rows.push({ nodeIndex: idx, depth: node.depth, expanded: isDir, isDir });
      if (isDir) walk(node.children);
    }
  };
  walk(tree.roots);
  return rows;
}

/**
 * Pure keyboard navigation over visible rows. Returns the next focused row
 * index and an optional action: `toggle` (collapse/expand a dir), `select`
 * (open a file). The component applies the action via its callbacks.
 */
export type TreeKeyboardAction =
  | { kind: 'none' }
  | { kind: 'move'; nextIndex: number }
  | { kind: 'toggle'; nodeIndex: number; nextIndex: number }
  | { kind: 'select'; nodeIndex: number; nextIndex: number };

export function treeKeyboardAction(
  rows: VisibleTreeRow[],
  tree: FileTree,
  currentIndex: number,
  key: string,
): TreeKeyboardAction {
  if (rows.length === 0) return { kind: 'none' };
  const clamp = (i: number) => Math.max(0, Math.min(rows.length - 1, i));
  const cur = currentIndex >= 0 && currentIndex < rows.length ? currentIndex : 0;
  const row = rows[cur];
  const node = row ? tree.nodes[row.nodeIndex] : null;
  if (key === 'ArrowDown') return { kind: 'move', nextIndex: clamp(cur + 1) };
  if (key === 'ArrowUp') return { kind: 'move', nextIndex: clamp(cur - 1) };
  if (key === 'Home') return { kind: 'move', nextIndex: 0 };
  if (key === 'End') return { kind: 'move', nextIndex: rows.length - 1 };
  if (key === 'Enter' || key === ' ') {
    if (!row || !node) return { kind: 'none' };
    if (node.kind === 'dir') return { kind: 'toggle', nodeIndex: row.nodeIndex, nextIndex: cur };
    return { kind: 'select', nodeIndex: row.nodeIndex, nextIndex: cur };
  }
  if (key === 'ArrowRight') {
    if (row && row.isDir && !row.expanded && node) return { kind: 'toggle', nodeIndex: row.nodeIndex, nextIndex: cur };
    if (row && row.isDir && row.expanded && node && node.children.length > 0) {
      const childIdx = cur + 1;
      if (childIdx < rows.length) return { kind: 'move', nextIndex: childIdx };
    }
    return { kind: 'none' };
  }
  if (key === 'ArrowLeft') {
    if (row && row.isDir && row.expanded && node) return { kind: 'toggle', nodeIndex: row.nodeIndex, nextIndex: cur };
    if (row && node && node.depth > 0) {
      const parentPath = node.path.slice(0, node.path.lastIndexOf('/'));
      const parentIdx = rows.findIndex((r) => tree.nodes[r.nodeIndex].path === parentPath && tree.nodes[r.nodeIndex].kind === 'dir');
      if (parentIdx >= 0) return { kind: 'move', nextIndex: parentIdx };
    }
    return { kind: 'none' };
  }
  return { kind: 'none' };
}

/** File index (into the snapshot's files[]) for a visible row, or -1. */
export function treeFileIndexAt(rows: VisibleTreeRow[], tree: FileTree, visibleIndex: number): number {
  const row = rows[visibleIndex];
  if (!row) return -1;
  const node = tree.nodes[row.nodeIndex];
  return node && node.kind === 'file' && node.fileIndex !== undefined ? node.fileIndex : -1;
}

// ---------------------------------------------------------------------------
// Bounded single-file diff paging (code_review_file_diff RPC).
// ---------------------------------------------------------------------------

/** Per-render soft cap (matches the backend MAX_FILE_RENDER_LINES). */
export const FILE_RENDER_SOFT_CAP = 4000;
/** Hard UI memory cap: the panel refuses to load more lines beyond this. */
export const FILE_DIFF_MAX_UI_LINES = 20_000;

export interface FileDiffPage {
  snapshotId: string;
  path: string;
  previousPath?: string;
  binary: boolean;
  status: FileStatus;
  lines: DiffLine[];
  cursor: number;
  nextCursor?: number;
  hasMore: boolean;
  totalLines: number;
  truncated: boolean;
}

/** Defensive normalization of a code_review_file_diff response. */
export function normalizeFileDiffPage(raw: unknown): FileDiffPage | null {
  const obj = asRecord(raw);
  if (!obj) return null;
  const path = stringField(obj.path);
  const snapshotId = stringField(obj.snapshotId);
  if (!path || !snapshotId) return null;
  const lines = Array.isArray(obj.lines) ? obj.lines.map(normalizeLine).filter((l): l is DiffLine => l !== null) : [];
  const status = stringField(obj.status);
  const nextCursor =
    typeof obj.nextCursor === 'number' && Number.isFinite(obj.nextCursor)
      ? Math.max(0, Math.floor(obj.nextCursor))
      : undefined;
  return {
    snapshotId,
    path,
    previousPath: optionalString(obj.previousPath),
    binary: boolField(obj.binary),
    status: FILE_STATUSES[status] ? (status as FileStatus) : 'changed',
    lines,
    cursor: uintField(obj.cursor),
    nextCursor,
    hasMore: boolField(obj.hasMore),
    totalLines: uintField(obj.totalLines),
    truncated: boolField(obj.truncated),
  };
}

/**
 * Accumulated loaded diff for one file: the snapshot's hunk lines (always
 * present) plus any backend pages appended for a globally-truncated file.
 * `nextCursor` is the backend cursor for the next page. The panel owns one of
 * these per selected file.
 */
export interface LoadedDiff {
  snapshotId: string;
  path: string;
  /** Snapshot hunk lines followed by backend-loaded extra lines. */
  lines: DiffLine[];
  /** Count of `lines` that came from the snapshot (hunk-structured). */
  snapshotLineCount: number;
  /** Backend cursor for the next page (=== lines.length when no backend page yet). */
  nextCursor: number;
  hasMoreBackend: boolean;
  totalLines: number;
  /** Snapshot render-cap and/or page byte-cap provenance (content may exist beyond loaded). */
  backendTruncated: boolean;
  /** True when the backend per-file byte cap was hit: no further pages exist. */
  byteCapped: boolean;
  loading: boolean;
  error: string | null;
}

/** A globally-truncated placeholder: the combined patch hit the byte cap and
 *  this file's body is absent from the snapshot (loads via code_review_file_diff). */
export function isDiffPlaceholder(file: DiffFile): boolean {
  return file.truncated && !file.binary && file.hunks.length === 0;
}

/** Initialize a LoadedDiff from the snapshot's file entry. */
export function initLoadedDiff(snapshotId: string, file: DiffFile, snapshotTruncated: boolean): LoadedDiff {
  const lines: DiffLine[] = [];
  for (const hunk of file.hunks) for (const line of hunk.lines) lines.push(line);
  return {
    snapshotId,
    path: file.path,
    lines,
    snapshotLineCount: lines.length,
    nextCursor: lines.length,
    hasMoreBackend: snapshotTruncated && file.truncated,
    totalLines: lines.length,
    backendTruncated: file.truncated,
    byteCapped: false,
    loading: false,
    error: null,
  };
}

/**
 * Merge a backend page into the loaded diff. Stale guards: the page's
 * snapshot id and path must match, and its cursor must equal the expected next
 * cursor (dedup — a duplicate or out-of-order page is dropped). On a successful
 * merge the page's lines append in order and the next cursor advances.
 */
export function appendFileDiffPage(state: LoadedDiff, page: FileDiffPage): LoadedDiff {
  if (page.snapshotId !== state.snapshotId || page.path !== state.path) return state;
  if (page.cursor !== state.nextCursor) return state;
  const lines = state.lines.concat(page.lines);
  const nextCursor = page.nextCursor ?? lines.length;
  return {
    ...state,
    lines,
    nextCursor,
    hasMoreBackend: page.hasMore,
    totalLines: Math.max(state.totalLines, page.totalLines),
    backendTruncated: page.truncated || state.backendTruncated,
    byteCapped: page.truncated || state.byteCapped,
    loading: false,
    error: null,
  };
}

/** Default "Load more" chunk: one soft-cap window (4000 lines). */
export const FILE_LOAD_CHUNK_LINES = FILE_RENDER_SOFT_CAP;

/** Page size requested from code_review_file_diff (backend MAX_FILE_PAGE_LINES). */
export const FILE_PAGE_REQUEST_LINES = 1000;

export interface DiffWindowPlan {
  /** Next visible-line target (never above the UI hard cap). */
  target: number;
  /** Lines renderable right now without backend pages, capped to the UI cap. */
  localAvailable: number;
  /** Total renderable lines (local or backend-extended), capped to the UI cap. */
  totalAvailable: number;
  /** True when the window can still grow (local lines or backend pages). */
  canLoadMore: boolean;
  /** True when reaching `target` requires code_review_file_diff pages. */
  needsFetch: boolean;
  /** True when the full file content exceeds the UI hard cap. */
  hardCapped: boolean;
}

/**
 * Compute the next visible-line window for a loaded diff. `toEnd` jumps to
 * the end (Load full); otherwise the window grows by `chunk`. The target
 * never exceeds FILE_DIFF_MAX_UI_LINES; backend pages are only needed when
 * the locally-available snapshot lines are exhausted and the backend still
 * has more. A window already at the UI cap reports canLoadMore=false.
 */
export function planDiffWindow(
  loaded: LoadedDiff,
  windowLimit: number,
  chunk = FILE_LOAD_CHUNK_LINES,
  toEnd = false,
): DiffWindowPlan {
  const known = Math.max(loaded.lines.length, loaded.totalLines);
  const localAvailable = Math.min(loaded.lines.length, FILE_DIFF_MAX_UI_LINES);
  const backendAvailable = loaded.hasMoreBackend;
  const totalAvailable = backendAvailable ? FILE_DIFF_MAX_UI_LINES : localAvailable;
  const current = Math.max(0, Math.min(windowLimit, totalAvailable));
  const hardCapped =
    known > FILE_DIFF_MAX_UI_LINES || (backendAvailable && current >= FILE_DIFF_MAX_UI_LINES);
  const rawTarget = toEnd ? totalAvailable : current + chunk;
  const target = Math.max(current, Math.min(rawTarget, totalAvailable));
  const needsFetch = target > localAvailable && backendAvailable;
  return {
    target,
    localAvailable,
    totalAvailable,
    canLoadMore: target > current,
    needsFetch,
    hardCapped,
  };
}
