/**
 * Code-review RPC wire normalization — pure helpers shared by
 * panels/CodeReviewPanel.tsx and scripts/codeReview.test.ts.
 *
 * Backend projects camelCase ReviewSnapshot + controller state (see
 * code_review_open/snapshot/refresh/comment/abort/close/stop). Absolute
 * ReviewSnapshot.root is NEVER on the wire. These helpers only coerce
 * unknown JSON into a stable view model so a malformed payload never
 * throws in the panel.
 */

import { isTransportError } from './transportError';

/** Transport failures belong to the shared toast, not the review workspace. */
export function shouldRenderReviewError(error: unknown): boolean {
  return !isTransportError(error);
}


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

export const DIFF_TREE_PAD_PX = 8;
export const DIFF_TREE_DEPTH_PX = 14;

export function diffTreePaddingLeft(depth: number): string {
  return `${DIFF_TREE_PAD_PX + depth * DIFF_TREE_DEPTH_PX}px`;
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

export type FileDiffBodyState =
  | 'complete' // parsed in full by the snapshot patch
  | 'preview' // parsed but cut by the global patch cap (seed superseded by pages)
  | 'onDemand' // placeholder: no body in the snapshot, loads via file_diff
  | 'binary'
  | 'byteCapped'; // the file's independent diff hit the backend byte cap

export interface DiffFile {
  path: string;
  previousPath?: string;
  status: FileStatus;
  binary: boolean;
  insertions: number;
  deletions: number;
  bodyState: FileDiffBodyState;
  truncated: boolean;
  message?: string;
  hunks: DiffHunk[];
}

export interface ReviewComment {
  role: 'user' | 'assistant' | 'system';
  text: string;
  partial: boolean;
  /** Model id for assistant replies when the backend stamps one. */
  model?: string;
  /** Finalized thinking of an assistant reply; present only when non-empty. */
  thinking?: string;
  /** True only on auto-submitted user comments (backend-stamped `auto: true`). */
  auto?: boolean;
}
export type ReviewFindingCategory = 'critical' | 'overdesign' | 'naming';

/** Derived display category. `clean` is never stored on a finding. */
export type ReviewCategory = ReviewFindingCategory | 'clean';

export type ReviewFindingSide = 'old' | 'new';

/** One inclusive side-specific source range inside a reviewed hunk. */
export interface ReviewFinding {
  category: ReviewFindingCategory;
  side: ReviewFindingSide;
  startLine: number;
  endLine: number;
  summary: string;
  evidence: string;
}

/** Reviewed hunk. `findings: []` is reviewed and clean. An absent hunk is unreviewed. */
export interface HunkReviewResult {
  identity: HunkIdentity;
  findings: ReviewFinding[];
}

/** Typed selection into the validated findings array of one hunk. */
export interface ReviewFindingRef {
  identity: HunkIdentity;
  findingIndex: number;
}

export type ReviewFindingDirection = 'previous' | 'next';

export type ReviewReplyTerminal = 'completed' | 'failed' | 'cancelled';
export type ReviewReplyStatus = 'idle' | 'streaming' | 'tool' | ReviewReplyTerminal;

export interface ReviewToolCall {
  id: string;
  name: string;
  status: 'running' | 'completed' | 'failed';
}

export interface ReviewThread {
  identity: HunkIdentity;
  comments: ReviewComment[];
  streamingText: string;
  /** Live thinking accumulation of the in-flight reply ('' when none). */
  streamingThinking: string;
  error: string | null;
  stale: boolean;
  /** True only while this thread's current generation is in-flight. */
  isStreaming: boolean;
  /**
   * Abort availability for this hunk. Fail-closed: missing/malformed wire
   * values are false and are never inferred from isStreaming or queued
   * comments. May stay true after a terminal reply while the owned prompt
   * task is still settling.
   */
  isCancellable: boolean;
  /** Model used for the active/last assistant reply on this thread. */
  model?: string;
  /** Controller generation that owns this thread's current/last reply. */
  generation: number;
  /** Stable reply identity (`rN`); empty before the first reply. */
  replyId: string;
  /** Authoritative reply lifecycle from the backend. */
  replyStatus: ReviewReplyStatus;
  toolCalls: ReviewToolCall[];
}

export type CodeReviewAutoState = 'idle' | 'triaging' | 'running' | 'done' | 'failed';

export type CodeReviewHighlightState = 'idle' | 'running' | 'done' | 'failed';

export type CodeReviewStopOperation = 'autoReview' | 'highlight' | 'tldr';

export type CodeReviewTldrState = 'idle' | 'running' | 'done' | 'failed';

/** Per-latest-auto-batch progress counters. Null when the wire omitted them
 *  or any field is not a non-negative integer. Explicit zeros stay zeros. */
export interface AutoReviewCounts {
  submitted: number | null;
  completed: number | null;
  failed: number | null;
}

/**
 * Typed wire error for the latest Auto Review batch. `code: 'cancelled'` is
 * the exact user-Stop terminal category (quiet on the Web); every other code
 * is a business failure that owns one App toast per batch identity.
 */
export interface AutoReviewWireError {
  message: string;
  code: string;
}

export interface CodeReviewSnapshot {
  comparisonLabel: string;
  snapshotId: string;
  reviewId: string;
  generation: number;
  revision: number;
  truncated: boolean;
  error: string | null;
  totalInsertions: number;
  totalDeletions: number;
/** True when the totals cover EVERY changed file (independent bounded numstat);
 *  false/missing → the frontend must not present partial parsed sums as complete. */
  totalsComplete: boolean;
  /** Count of untracked, non-ignored files omitted from the tracked-only review. */
  untrackedNotShown: number;
  files: DiffFile[];
  threads: ReviewThread[];
  /** Aggregate: true when any review reply is active. */
  isStreaming: boolean;
  /** Number of concurrently active review replies. */
  activeCount: number;
  /**
   * Aggregate owned-or-pending work (prompt-task owner or queued comments).
   * Fail-closed: missing/malformed wire values are false and are never
   * inferred from isStreaming/activeCount.
   */
  internalBusy: boolean;
  /** Auto-review batch state; defaults to 'idle' on absent/unknown wire values. */
  autoReviewState: CodeReviewAutoState;
  /** Auto-review progress counters for the latest batch; zeroed defaults. */
  autoReviewCounts: AutoReviewCounts;
  autoReviewError: AutoReviewWireError | null;
  /** Monotonic id of the latest Auto Review batch; 0 before the first batch. */
  autoReviewBatchId: number;
  /** Current review model display name; null when unknown. Never used as identity. */
  reviewModelLabel: string | null;
  /** Current review model provider identity; null when unknown. */
  reviewModelProvider: string | null;
  /** Current review model id identity; null when unknown. */
  reviewModelId: string | null;
  /** Preferred review language tag (settings/explicit RPC); null when unset. */
  reviewLanguageDefault: string | null;
  /** Highlight one-call state; defaults to 'idle' on absent/unknown wire values. */
  highlightState: CodeReviewHighlightState;
  /** Bounded failure copy for the latest Highlight call. */
  highlightError: string | null;
  /** Monotonic generation of the latest Highlight call; 0 before the first. */
  highlightGeneration: number;
  /**
   * Authoritative reviewed hunks. `null` means the wire omitted the field
   * (nothing reviewed). `[]` means no reviewed hunk results. A hunk whose
   * own `findings` is empty was reviewed and is clean. Malformed payloads
   * reject the snapshot; the collection is not truncated at 20 because Auto
   * accumulates one response per hunk.
   */
  reviewFindings: HunkReviewResult[] | null;
  /** TLDR call state; defaults to 'idle' on absent/unknown wire values. */
  tldrState: CodeReviewTldrState;
  /** Monotonic generation of the latest TLDR call; 0 before the first. */
  tldrGeneration: number;
  /** Snapshot the running or last-good TLDR was captured against. */
  tldrSnapshotId: string | null;
  /** Bounded failure copy for the latest TLDR call. Null when none. */
  tldrError: string | null;
  /** Last approved markdown. Empty before the first accepted body. */
  tldrMarkdown: string;

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

const FILE_BODY_STATES: Record<string, true> = {
  complete: true,
  preview: true,
  onDemand: true,
  binary: true,
  byteCapped: true,
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
  const bodyStateRaw = stringField(obj.bodyState);
  const file: DiffFile = {
    path,
    status,
    binary: boolField(obj.binary),
    insertions: uintField(obj.insertions),
    deletions: uintField(obj.deletions),
    bodyState: FILE_BODY_STATES[bodyStateRaw]
      ? (bodyStateRaw as FileDiffBodyState)
      : boolField(obj.binary)
        ? 'binary'
        : 'preview',
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
  const model = optionalString(obj.model);
  const thinking = optionalString(obj.thinking);
  return {
    role,
    text: stringField(obj.text),
    partial: boolField(obj.partial),
    ...(model ? { model } : {}),
    ...(thinking ? { thinking } : {}),
    ...(obj.auto === true ? { auto: true } : {}),
  };
}

function normalizeToolCall(raw: unknown): ReviewToolCall | null {
  const obj = asRecord(raw);
  if (!obj) return null;
  const id = stringField(obj.id);
  if (!id) return null;
  const statusRaw = stringField(obj.status);
  const status: ReviewToolCall['status'] =
    statusRaw === 'completed' || statusRaw === 'failed' ? statusRaw : 'running';
  return { id, name: stringField(obj.name) || 'tool', status };
}

function normalizeReplyStatus(raw: unknown, explicitStreaming: unknown): ReviewReplyStatus {
  const value = stringField(raw);
  if (
    value === 'streaming'
    || value === 'tool'
    || value === 'completed'
    || value === 'failed'
    || value === 'cancelled'
    || value === 'idle'
  ) {
    return value;
  }
  if (explicitStreaming === true) return 'streaming';
  if (explicitStreaming === false) return 'idle';
  return 'idle';
}

const FINDING_CATEGORIES: Record<ReviewFindingCategory, true> = {
  critical: true,
  overdesign: true,
  naming: true,
};

const FINDING_SIDES: Record<ReviewFindingSide, true> = {
  old: true,
  new: true,
};

const AUTO_STATES: Record<CodeReviewAutoState, true> = {
  idle: true,
  triaging: true,
  running: true,
  done: true,
  failed: true,
};

/** One model response may report at most this many findings. The stored collection is larger: Auto accumulates one response per hunk. */
export const CODE_REVIEW_MAX_FINDINGS_PER_RESPONSE = 20;

/** Per-hunk finding bound matches one model response. The stored collection has no hunk-count cap. */

/** Findings inside one reviewed hunk cannot exceed one model response. */
export const CODE_REVIEW_MAX_FINDINGS_PER_HUNK = CODE_REVIEW_MAX_FINDINGS_PER_RESPONSE;

/** Strict finding summary UTF-8 byte cap. */
export const CODE_REVIEW_MAX_FINDING_SUMMARY_BYTES = 512;

/** Strict finding evidence UTF-8 byte cap (same scale as a manual comment). */
export const CODE_REVIEW_MAX_FINDING_EVIDENCE_BYTES = 8_192;

const FINDING_KEYS = ['category', 'side', 'startLine', 'endLine', 'summary', 'evidence'] as const;
const HUNK_RESULT_KEYS = ['identity', 'findings'] as const;
const IDENTITY_KEYS = ['snapshotId', 'path', 'oldStart', 'oldCount', 'newStart', 'newCount', 'contentHash'] as const;
const IDENTITY_RANGE_FIELDS = ['oldStart', 'oldCount', 'newStart', 'newCount'] as const;

/** Auto-review batch state; any absent/unknown wire value falls back to idle. */
function normalizeAutoReviewState(raw: unknown): CodeReviewAutoState {
  const value = stringField(raw);
  return Object.hasOwn(AUTO_STATES, value) ? (value as CodeReviewAutoState) : 'idle';
}

function countField(value: unknown): number | null {
  if (typeof value !== 'number' || !Number.isInteger(value) || value < 0) return null;
  return value;
}

/** Auto-review counters. A missing object or any non-integer/negative field
 *  nulls every counter. Explicit `{submitted:0,completed:0,failed:0}` stays. */
function normalizeAutoReviewCounts(raw: unknown): AutoReviewCounts {
  const absent: AutoReviewCounts = { submitted: null, completed: null, failed: null };
  const obj = asRecord(raw);
  if (!obj) return absent;
  const submitted = countField(obj.submitted);
  const completed = countField(obj.completed);
  const failed = countField(obj.failed);
  if (submitted === null || completed === null || failed === null) return absent;
  return { submitted, completed, failed };
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
  const replyStatus = normalizeReplyStatus(obj.replyStatus, obj.isStreaming);
  const isStreaming = replyStatus === 'streaming' || replyStatus === 'tool';
  const streamingText = isStreaming ? stringField(obj.streamingText) : '';
  const streamingThinking = isStreaming ? stringField(obj.streamingThinking) : '';
  const error = nullableString(obj.error);
  const model = optionalString(obj.model);
  const toolCalls: ReviewToolCall[] = [];
  if (Array.isArray(obj.toolCalls)) {
    for (const entry of obj.toolCalls) {
      const tool = normalizeToolCall(entry);
      if (tool) toolCalls.push(tool);
    }
  }
  return {
    identity,
    comments,
    streamingText,
    streamingThinking,
    error,
    stale: boolField(obj.stale),
    isStreaming,
    isCancellable: boolField(obj.isCancellable),
    generation: uintField(obj.generation),
    replyId: stringField(obj.replyId),
    replyStatus,
    toolCalls,

    ...(model ? { model } : {}),
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
    reviewId: '',
    generation: 0,
    revision: 0,
    truncated: false,
    error,
    totalInsertions: 0,
    totalDeletions: 0,
    totalsComplete: false,
    untrackedNotShown: 0,
    files: [],
    threads: [],
    isStreaming: false,
    activeCount: 0,
    internalBusy: false,
    autoReviewState: 'idle',
    autoReviewCounts: { submitted: null, completed: null, failed: null },
    autoReviewError: null,
    autoReviewBatchId: 0,
    reviewLanguageDefault: null,
    reviewModelLabel: null,
    reviewModelProvider: null,
    reviewModelId: null,
    highlightState: 'idle',
    highlightError: null,
    highlightGeneration: 0,
    reviewFindings: null,
    tldrState: 'idle',
    tldrGeneration: 0,
    tldrSnapshotId: null,
    tldrError: null,
    tldrMarkdown: '',
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
  const findings = normalizeReviewFindings(obj.reviewFindings);
  if (!findings.ok) return emptyCodeReviewSnapshot(findings.error);
  const tldr = normalizeTldrFields(obj);
  if (!tldr.ok) return emptyCodeReviewSnapshot(tldr.error);
  const filesRaw = Array.isArray(obj.files) ? obj.files : [];
  const files: DiffFile[] = [];
  for (const entry of filesRaw) {
    const file = normalizeFile(entry);
    if (file) files.push(file);
  }

  const threads = normalizeThreads(obj.threads);
  const streamingThreadCount = threads.reduce(
    (count, thread) => count + (thread.isStreaming ? 1 : 0),
    0,
  );
  const activeCount =
    obj.activeCount == null || obj.activeCount === ''
      ? streamingThreadCount
      : uintField(obj.activeCount);
  const isStreaming =
    typeof obj.isStreaming === 'boolean' ? obj.isStreaming : activeCount > 0;
  const internalBusy = boolField(obj.internalBusy);

  return {
    comparisonLabel: stringField(obj.comparisonLabel),
    snapshotId: stringField(obj.snapshotId),
    reviewId: stringField(obj.reviewId),
    generation: uintField(obj.generation),
    revision: uintField(obj.revision),
    truncated: boolField(obj.truncated),
    error: nullableString(obj.error),
    totalInsertions: uintField(obj.totalInsertions),
    totalDeletions: uintField(obj.totalDeletions),
    totalsComplete: boolField(obj.totalsComplete),
    untrackedNotShown: uintField(obj.untrackedNotShown),
    files,
    threads,
    isStreaming,
    activeCount,
    internalBusy,
    autoReviewState: normalizeAutoReviewState(obj.autoReviewState),
    autoReviewCounts: normalizeAutoReviewCounts(obj.autoReviewCounts),
    autoReviewError: normalizeAutoReviewError(obj.autoReviewError),
    autoReviewBatchId: uintField(obj.autoReviewBatchId),
    reviewModelLabel: nullableString(obj.reviewModelLabel),
    reviewModelProvider: nullableString(obj.reviewModelProvider),
    reviewModelId: nullableString(obj.reviewModelId),
    reviewLanguageDefault: nullableString(obj.reviewLanguageDefault),
    highlightState: normalizeHighlightState(obj.highlightState),
    highlightError: nullableString(obj.highlightError),
    highlightGeneration: uintField(obj.highlightGeneration),
    reviewFindings: findings.results,
    tldrState: tldr.state,
    tldrGeneration: tldr.generation,
    tldrSnapshotId: tldr.snapshotId,
    tldrError: tldr.error,
    tldrMarkdown: tldr.markdown,

  };
}
/**
 * Strict typed Auto Review error: null unless the wire carries exactly
 * { message: non-empty string, code: non-empty string }. A malformed value
 * (including the retired plain-string shape) nulls the whole error — never a
 * partial record.
 */
function normalizeAutoReviewError(raw: unknown): AutoReviewWireError | null {
  const obj = asRecord(raw);
  if (!obj) return null;
  const message = nullableString(obj.message);
  const code = nullableString(obj.code);
  return message && code ? { message, code } : null;
}

function normalizeHighlightState(value: unknown): CodeReviewHighlightState {
  return value === 'running' || value === 'done' || value === 'failed' ? value : 'idle';
}
const TLDR_STATES: Record<CodeReviewTldrState, true> = {
  idle: true,
  running: true,
  done: true,
  failed: true,
};
const INVALID_TLDR = 'Invalid TLDR fields';

function normalizeTldrFields(obj: Record<string, unknown>): {
  ok: true;
  state: CodeReviewTldrState;
  generation: number;
  snapshotId: string | null;
  error: string | null;
  markdown: string;
} | { ok: false; error: string } {
  const supplied = 'tldrState' in obj || 'tldrGeneration' in obj || 'tldrSnapshotId' in obj || 'tldrError' in obj || 'tldrMarkdown' in obj;
  if (!supplied) {
    return { ok: true, state: 'idle', generation: 0, snapshotId: null, error: null, markdown: '' };
  }
  const snapshotId = typeof obj.tldrSnapshotId === 'string' ? obj.tldrSnapshotId : null;
  const tldrError = typeof obj.tldrError === 'string' ? obj.tldrError : null;
  if (
    typeof obj.tldrState !== 'string'
    || !Object.hasOwn(TLDR_STATES, obj.tldrState)
    || typeof obj.tldrGeneration !== 'number'
    || !Number.isInteger(obj.tldrGeneration)
    || obj.tldrGeneration < 0
    || (obj.tldrSnapshotId != null && snapshotId === null)
    || (obj.tldrError != null && tldrError === null)
    || typeof obj.tldrMarkdown !== 'string'
  ) return { ok: false, error: INVALID_TLDR };
  return {
    ok: true,
    state: obj.tldrState as CodeReviewTldrState,
    generation: obj.tldrGeneration,
    snapshotId,
    error: tldrError,
    markdown: obj.tldrMarkdown,
  };
}

function exactKeys(obj: Record<string, unknown>, keys: readonly string[]): boolean {
  const present = Object.keys(obj);
  if (present.length !== keys.length) return false;
  for (const key of keys) {
    if (!Object.hasOwn(obj, key)) return false;
  }
  return true;
}

function boundedText(value: unknown, maxBytes: number): string | null {
  if (typeof value !== 'string' || value.trim() === '') return null;
  if (new TextEncoder().encode(value).length > maxBytes) return null;
  return value;
}

function strictIdentity(raw: unknown): HunkIdentity | null {
  const obj = asRecord(raw);
  if (!obj || !exactKeys(obj, IDENTITY_KEYS)) return null;
  const snapshotId = obj.snapshotId;
  const path = obj.path;
  const contentHash = obj.contentHash;
  if (typeof snapshotId !== 'string' || snapshotId === '') return null;
  if (typeof path !== 'string' || path === '') return null;
  if (typeof contentHash !== 'string' || contentHash === '') return null;
  const ranges: number[] = [];
  for (const field of IDENTITY_RANGE_FIELDS) {
    const fieldValue = obj[field];
    if (typeof fieldValue !== 'number' || !Number.isInteger(fieldValue) || fieldValue < 0) return null;
    ranges.push(fieldValue);
  }
  return {
    snapshotId,
    path,
    oldStart: ranges[0],
    oldCount: ranges[1],
    newStart: ranges[2],
    newCount: ranges[3],
    contentHash,
  };
}

function strictFinding(raw: unknown): ReviewFinding | null {
  const obj = asRecord(raw);
  if (!obj || !exactKeys(obj, FINDING_KEYS)) return null;
  const category = obj.category;
  const side = obj.side;
  if (typeof category !== 'string' || !Object.hasOwn(FINDING_CATEGORIES, category)) return null;
  if (typeof side !== 'string' || !Object.hasOwn(FINDING_SIDES, side)) return null;
  const startLine = obj.startLine;
  const endLine = obj.endLine;
  if (typeof startLine !== 'number' || !Number.isInteger(startLine) || startLine < 1) return null;
  if (typeof endLine !== 'number' || !Number.isInteger(endLine) || endLine < startLine) return null;
  const summary = boundedText(obj.summary, CODE_REVIEW_MAX_FINDING_SUMMARY_BYTES);
  const evidence = boundedText(obj.evidence, CODE_REVIEW_MAX_FINDING_EVIDENCE_BYTES);
  if (summary == null || evidence == null) return null;
  return {
    category: category as ReviewFindingCategory,
    side: side as ReviewFindingSide,
    startLine,
    endLine,
    summary,
    evidence,
  };
}
function findingInsideIdentity(identity: HunkIdentity, finding: ReviewFinding): boolean {
  const start = finding.side === 'old' ? identity.oldStart : identity.newStart;
  const count = finding.side === 'old' ? identity.oldCount : identity.newCount;
  if (count <= 0) return false;
  return finding.startLine >= start && finding.endLine <= start + count - 1;
}

const INVALID_REVIEW_FINDINGS = 'Invalid review findings';

/**
 * Strict stored collection. Absent is unreviewed (`null`). `[]` is a real
 * empty reviewed-hunk list. A non-array, unknown field, duplicate identity,
 * duplicate finding, or over-cap hunk rejects the whole payload. The 20
 * finding bound is per hunk (one model response), not across the collection.
 */
export function normalizeReviewFindings(
  value: unknown,
): { ok: true; results: HunkReviewResult[] | null } | { ok: false; error: string } {
  if (value == null) return { ok: true, results: null };
  if (!Array.isArray(value)) {
    return { ok: false, error: INVALID_REVIEW_FINDINGS };
  }
  const identities = new Set<string>();
  const fingerprints = new Set<string>();
  const results: HunkReviewResult[] = [];
  for (const raw of value) {
    const entry = asRecord(raw);
    if (!entry || !exactKeys(entry, HUNK_RESULT_KEYS) || !Array.isArray(entry.findings)) {
      return { ok: false, error: INVALID_REVIEW_FINDINGS };
    }
    if (entry.findings.length > CODE_REVIEW_MAX_FINDINGS_PER_HUNK) {
      return { ok: false, error: INVALID_REVIEW_FINDINGS };
    }
    const identity = strictIdentity(entry.identity);
    if (!identity) return { ok: false, error: INVALID_REVIEW_FINDINGS };
    const identityKey = hunkKey(identity);
    if (identities.has(identityKey)) return { ok: false, error: INVALID_REVIEW_FINDINGS };
    identities.add(identityKey);
    const findings: ReviewFinding[] = [];
    for (const rawFinding of entry.findings) {
      const finding = strictFinding(rawFinding);
      if (!finding || !findingInsideIdentity(identity, finding)) return { ok: false, error: INVALID_REVIEW_FINDINGS };
      const fingerprint = [
        identityKey,
        finding.category,
        finding.side,
        String(finding.startLine),
        String(finding.endLine),
        finding.summary,
        finding.evidence,
      ].join('\0');
      if (fingerprints.has(fingerprint)) return { ok: false, error: INVALID_REVIEW_FINDINGS };
      fingerprints.add(fingerprint);
      findings.push(finding);
    }
    results.push({ identity, findings });
  }
  return { ok: true, results };
}


/** Wire payload for `code_review_highlight` (no body fields needed). */
export function buildCodeReviewHighlightPayload() {
  return { type: 'code_review_highlight' as const };
}

/** Wire payload for `code_review_tldr`. `language` rides only for an explicit tag. */
export function buildCodeReviewTldrPayload(language?: string): Record<string, unknown> {
  const payload: Record<string, unknown> = { type: 'code_review_tldr' };
  if (language) payload.language = language;
  return payload;
}

export function buildCodeReviewStopPayload(operation: CodeReviewStopOperation): Record<string, unknown> {
  return { type: 'code_review_stop', operation };
}

/** Path-free empty-state copy when untracked files are omitted from the review. */
export function untrackedNotShownNote(count: number): string | null {
  if (!Number.isFinite(count) || count <= 0) return null;
  const n = Math.floor(count);
  return n === 1
    ? '1 untracked file is not part of the review'
    : `${n} untracked files are not part of the review`;
}

/** Empty comparison copy: tracked-only review with an optional untracked note. */
export function emptyComparisonMessage(untrackedNotShown = 0): string {
  const note = untrackedNotShownNote(untrackedNotShown);
  return note
    ? `No tracked changes — ${note}`
    : 'No changed files in this comparison.';
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

/** True when a thread currently has an in-flight review reply. Leftover
 *  `streamingText` after a terminal generation is display residue, not activity. */
export function threadIsStreaming(thread: ReviewThread | undefined | null): boolean {
  return !!thread?.isStreaming;
}

/** Dead-air rule from the frozen composer-parity contract: while a reply is
 *  in-flight and neither streaming text nor thinking has arrived, the dock
 *  renders a "Thinking…" shimmer so the user sees life immediately. */
export function threadNeedsThinkingPlaceholder(
  thread: ReviewThread | undefined | null,
): boolean {
  return !!thread?.isStreaming && thread.streamingText === '' && thread.streamingThinking === '';
}

/** True when Abort is available for this hunk. Fail-closed: never inferred
 *  from visible live status or leftover streamingText. May stay true after a
 *  terminal Completed/Failed/Cancelled reply while owned/queued work lingers. */
export function threadIsCancellable(thread: ReviewThread | undefined | null): boolean {
  return !!thread?.isCancellable;
}

/** Aggregate owned-or-pending work for close/refresh warnings. Fail-closed:
 *  never inferred from isStreaming/activeCount. Does not lock the composer. */
export function reviewHasInternalBusy(
  snapshot: Pick<CodeReviewSnapshot, 'internalBusy'> | null | undefined,
): boolean {
  return snapshot?.internalBusy === true;
}

/** Composer lock is local comment/abort RPC or a missing snapshot.
 *  Visible live status and internalBusy never disable typing or Comment. */
export function reviewComposerLocked(
  busy: string | null | undefined,
  snapshotId: string | null | undefined,
): boolean {
  return busy != null || !snapshotId;
}

/** Close-panel warning. Null when neither owned/queued work nor drafts exist. */
export function formatReviewCloseWarning(internalBusy: boolean, draftCount: number): string | null {
  const reasons: string[] = [];
  if (internalBusy === true) reasons.push('in-flight review work');
  const drafts = Number.isFinite(draftCount) ? Math.max(0, Math.floor(draftCount)) : 0;
  if (drafts > 0) reasons.push(`${drafts} unsent draft${drafts === 1 ? '' : 's'}`);
  if (reasons.length === 0) return null;
  return `Close anyway? ${reasons.join(' and ')} will be discarded.`;
}

/** Refresh warning. Drafts keep the stale-thread copy; internalBusy-only warns
 *  that owned/queued work is still in flight. Visible live status is unused. */
export function formatReviewRefreshWarning(internalBusy: boolean, draftCount: number): string | null {
  const drafts = Number.isFinite(draftCount) ? Math.max(0, Math.floor(draftCount)) : 0;
  if (drafts > 0) {
    return `Refresh anyway? Your ${drafts} draft${drafts === 1 ? '' : 's'} will be kept, but threads may go stale.`;
  }
  if (internalBusy === true) return 'Refresh anyway? Review work is still in flight.';
  return null;
}


/**
 * Full identity payload for `code_review_abort` — same HunkIdentity fields as
 * `code_review_comment`, plus the command type. Session stamping is the caller's job.
 */
export function buildCodeReviewAbortPayload(
  identity: Pick<
    HunkIdentity,
    'snapshotId' | 'path' | 'oldStart' | 'oldCount' | 'newStart' | 'newCount' | 'contentHash'
  >,
): Record<string, unknown> {
  return {
    type: 'code_review_abort',
    snapshotId: identity.snapshotId,
    path: identity.path,
    oldStart: identity.oldStart,
    oldCount: identity.oldCount,
    newStart: identity.newStart,
    newCount: identity.newCount,
    contentHash: identity.contentHash,
  };
}

/** Header aggregate: `N replies` while any review replies are active; null when idle. */
export function formatActiveRepliesLabel(activeCount: number): string | null {
  const n = Number.isFinite(activeCount) ? Math.max(0, Math.floor(activeCount)) : 0;
  if (n <= 0) return null;
  return `${n} ${n === 1 ? 'reply' : 'replies'}`;
}

// ---------------------------------------------------------------------------
// Per-thread review reply lifecycle. Snapshot polls, abort, and late deltas
// share one in-flight owner keyed by review generation + replyId; completed /
// failed / cancelled cannot be resurrected by an older generation.
// ---------------------------------------------------------------------------


export interface ReviewReplyState {
  generation: number;
  replyId: string;
  status: ReviewReplyStatus;
  streamingText: string;
  streamingThinking: string;
  tools: ReviewToolCall[];
}

export interface ReviewThreadLifecycle {
  key: string;
  identity?: HunkIdentity;
  reply: ReviewReplyState;
  comments: ReviewComment[];
  isCancellable: boolean;
}

export interface ReviewLifecycleState {
  reviewId: string;
  snapshotId: string;
  generation: number;
  revision: number;
  threads: Record<string, ReviewThreadLifecycle>;
}

export type ReviewLifecycleEvent =
  | { type: 'snapshot'; snapshot: CodeReviewSnapshot }
  | { type: 'reply-start'; key: string }
  | { type: 'delta'; key: string; generation: number; replyId?: string; text: string }
  | { type: 'tool-start'; key: string; generation: number; replyId?: string; toolCallId: string; toolName: string }
  | { type: 'tool-end'; key: string; generation: number; replyId?: string; toolCallId: string }
  | { type: 'terminal'; key: string; generation: number; replyId?: string; status: ReviewReplyTerminal }
  | { type: 'abort'; key: string };

const TERMINAL_REPLY: Record<ReviewReplyTerminal, true> = {
  completed: true,
  failed: true,
  cancelled: true,
};

export function reviewReplyIsTerminal(status: ReviewReplyStatus): status is ReviewReplyTerminal {
  return status in TERMINAL_REPLY;
}

export function reviewReplyIsLive(status: ReviewReplyStatus): boolean {
  return status === 'streaming' || status === 'tool';
}

export function emptyReviewLifecycle(
  reviewId = '',
  snapshotId = '',
  generation = 0,
  revision = 0,
): ReviewLifecycleState {
  return { reviewId, snapshotId, generation, revision, threads: {} };
}

function emptyReplyState(generation = 0): ReviewReplyState {
  return { generation, replyId: '', status: 'idle', streamingText: '', streamingThinking: '', tools: [] };
}

function putThreadReply(
  state: ReviewLifecycleState,
  key: string,
  reply: ReviewReplyState,
  comments?: ReviewComment[],
): ReviewLifecycleState {
  const current = state.threads[key];
  return {
    ...state,
    threads: {
      ...state.threads,
      [key]: {
        key,
        identity: current?.identity,
        reply,
        comments: comments ?? current?.comments ?? [],
        isCancellable: reviewReplyIsTerminal(reply.status) ? false : current?.isCancellable ?? false,
      },
    },
  };
}

function threadLifecycle(state: ReviewLifecycleState, key: string): ReviewThreadLifecycle {
  return state.threads[key] ?? {
    key,
    reply: emptyReplyState(state.generation),
    comments: [],
    isCancellable: false,
  };
}

function replySeq(replyId: string): number {
  if (!replyId.startsWith('r')) return 0;
  const n = Number(replyId.slice(1));
  return Number.isFinite(n) ? n : 0;
}

function sameOwner(reply: ReviewReplyState, generation: number, replyId?: string): boolean {
  if (generation !== reply.generation) return false;
  if (replyId && reply.replyId && replyId !== reply.replyId) return false;
  return true;
}

function fromWireThread(thread: ReviewThread): ReviewReplyState {
  const live = reviewReplyIsLive(thread.replyStatus);
  return {
    generation: thread.generation,
    replyId: thread.replyId,
    status: thread.replyStatus,
    streamingText: live ? thread.streamingText : '',
    streamingThinking: live ? thread.streamingThinking : '',
    tools: thread.toolCalls,
  };
}

function reconcileSnapshotThread(
  prev: ReviewThreadLifecycle | undefined,
  thread: ReviewThread,
  retainLongerComments: boolean,
): ReviewThreadLifecycle {
  const key = hunkKey(thread.identity);
  const incoming = fromWireThread(thread);
  if (!prev) {
    return {
      key,
      identity: thread.identity,
      reply: incoming,
      comments: thread.comments,
      isCancellable: thread.isCancellable,
    };
  }
  if (incoming.generation < prev.reply.generation) {
    return prev;
  }
  const incomingSeq = replySeq(incoming.replyId);
  const prevSeq = replySeq(prev.reply.replyId);
  if (incoming.generation === prev.reply.generation && incomingSeq < prevSeq) {
    return prev;
  }
  if (
    incoming.generation === prev.reply.generation
    && incoming.replyId === prev.reply.replyId
    && reviewReplyIsTerminal(prev.reply.status)
    && reviewReplyIsLive(incoming.status)
  ) {
    return prev;
  }
  const comments = retainLongerComments && prev.comments.length > thread.comments.length
    ? prev.comments
    : thread.comments;
  const sameOwnerTerminal =
    incoming.generation === prev.reply.generation
    && incoming.replyId === prev.reply.replyId
    && reviewReplyIsTerminal(prev.reply.status);
  const sameOwnerLive =
    incoming.generation === prev.reply.generation
    && incoming.replyId === prev.reply.replyId
    && reviewReplyIsLive(prev.reply.status)
    && reviewReplyIsLive(incoming.status);
  const incomingHasStream =
    incoming.streamingText.length > 0 || incoming.streamingThinking.length > 0;
  const reply = sameOwnerLive && incomingHasStream
    ? {
        ...incoming,
        streamingText:
          incoming.streamingText.length >= prev.reply.streamingText.length
            ? incoming.streamingText
            : prev.reply.streamingText,
        streamingThinking:
          incoming.streamingThinking.length >= prev.reply.streamingThinking.length
            ? incoming.streamingThinking
            : prev.reply.streamingThinking,
      }
    : incoming;
  return {
    key,
    identity: thread.identity,
    reply,
    comments,
    isCancellable: sameOwnerTerminal
      ? prev.isCancellable && thread.isCancellable
      : thread.isCancellable,
  };
}

function applySnapshotLifecycle(
  state: ReviewLifecycleState,
  snapshot: CodeReviewSnapshot,
): ReviewLifecycleState {
  const cutover =
    (snapshot.reviewId !== '' && snapshot.reviewId !== state.reviewId)
    || (snapshot.snapshotId && snapshot.snapshotId !== state.snapshotId)
    || (snapshot.generation > 0 && snapshot.generation !== state.generation);
  if (cutover) {
    const threads: Record<string, ReviewThreadLifecycle> = {};
    for (const thread of snapshot.threads) {
      const key = hunkKey(thread.identity);
      threads[key] = reconcileSnapshotThread(undefined, thread, false);
    }
    return {
      reviewId: snapshot.reviewId,
      snapshotId: snapshot.snapshotId,
      generation: snapshot.generation,
      revision: snapshot.revision,
      threads,
    };
  }
  const staleRevision = state.revision > 0 && snapshot.revision <= state.revision;
  if (staleRevision && snapshot.threads.length === 0 && Object.keys(state.threads).length > 0) {
    return state;
  }
  const threads: Record<string, ReviewThreadLifecycle> = {};
  for (const thread of snapshot.threads) {
    const key = hunkKey(thread.identity);
    threads[key] = reconcileSnapshotThread(state.threads[key], thread, staleRevision);
  }
  if (staleRevision) {
    for (const [key, prev] of Object.entries(state.threads)) {
      const incoming = threads[key];
      if (!incoming || replySeq(incoming.reply.replyId) < replySeq(prev.reply.replyId)) {
        threads[key] = prev;
      }
    }
  }
  return {
    reviewId: snapshot.reviewId || state.reviewId,
    snapshotId: snapshot.snapshotId || state.snapshotId,
    generation: snapshot.generation || state.generation,
    revision: Math.max(snapshot.revision, state.revision),
    threads,
  };
}

/** Apply one lifecycle event. Terminal generations are monotonic: a completed,
 *  failed, or cancelled owner rejects late deltas, duplicate terminals, and
 *  stale snapshot polls that try to resurrect the same reply. */
export function applyReviewLifecycle(
  state: ReviewLifecycleState,
  event: ReviewLifecycleEvent,
): ReviewLifecycleState {
  switch (event.type) {
    case 'snapshot':
      return applySnapshotLifecycle(state, event.snapshot);
    case 'reply-start': {
      const current = threadLifecycle(state, event.key);
      if (reviewReplyIsLive(current.reply.status) || reviewReplyIsTerminal(current.reply.status)) return state;
      return putThreadReply(state, event.key, {
        generation: current.reply.generation || state.generation,
        replyId: current.reply.replyId,
        status: 'streaming',
        streamingText: '',
        streamingThinking: '',
        tools: [],
      });
    }
    case 'delta': {
      const current = threadLifecycle(state, event.key);
      if (!sameOwner(current.reply, event.generation, event.replyId)) return state;
      if (!reviewReplyIsLive(current.reply.status)) return state;
      return putThreadReply(state, event.key, {
        ...current.reply,
        streamingText: current.reply.streamingText + event.text,
      });
    }
    case 'tool-start': {
      const current = threadLifecycle(state, event.key);
      if (!sameOwner(current.reply, event.generation, event.replyId)) return state;
      if (!reviewReplyIsLive(current.reply.status)) return state;
      const existing = current.reply.tools.find((tool) => tool.id === event.toolCallId);
      if (existing && existing.status !== 'running') return state;
      const newTool = { id: event.toolCallId, name: event.toolName, status: 'running' as const };
      const tools: ReviewToolCall[] = existing
        ? current.reply.tools
        : [...current.reply.tools, newTool];
      return putThreadReply(state, event.key, {
        ...current.reply,
        status: 'tool',
        tools,
      });
    }
    case 'tool-end': {
      const current = threadLifecycle(state, event.key);
      if (!sameOwner(current.reply, event.generation, event.replyId)) return state;
      if (!reviewReplyIsLive(current.reply.status)) return state;
      const tools: ReviewToolCall[] = current.reply.tools.map((tool) =>
        tool.id === event.toolCallId && tool.status === 'running'
          ? { ...tool, status: 'completed' as const }
          : tool,
      );
      const running = tools.some((tool) => tool.status === 'running');
      return putThreadReply(state, event.key, {
        ...current.reply,
        tools,
        status: running ? 'tool' : 'streaming',
      });
    }
    case 'terminal': {
      const current = threadLifecycle(state, event.key);
      if (event.generation < current.reply.generation) return state;
      if (
        reviewReplyIsTerminal(current.reply.status)
        && sameOwner(current.reply, event.generation, event.replyId)
      ) {
        return state;
      }
      if (
        event.replyId
        && current.reply.replyId
        && event.replyId !== current.reply.replyId
        && event.generation === current.reply.generation
      ) {
        return state;
      }
      return putThreadReply(state, event.key, {
        generation: event.generation,
        replyId: event.replyId ?? current.reply.replyId,
        status: event.status,
        streamingText: '',
        streamingThinking: '',
        tools: current.reply.tools.map((tool) =>
          tool.status === 'running' ? { ...tool, status: 'failed' } : tool,
        ),
      });
    }
    case 'abort': {
      const current = threadLifecycle(state, event.key);
      if (!reviewReplyIsLive(current.reply.status)) return state;
      return putThreadReply(state, event.key, {
        ...current.reply,
        status: 'cancelled',
        streamingText: '',
        streamingThinking: '',
        tools: current.reply.tools.map((tool) =>
          tool.status === 'running' ? { ...tool, status: 'failed' } : tool,
        ),
      });
    }
  }
}

export function reviewLifecycleActiveCount(state: ReviewLifecycleState): number {
  let count = 0;
  for (const thread of Object.values(state.threads)) {
    if (reviewReplyIsLive(thread.reply.status)) count += 1;
  }
  return count;
}

/** Project lifecycle onto a normalized snapshot: in-flight flags follow the
 *  generation owner; leftover stream buffers never keep Abort/activity live.
 *  Comment arrays are preserved (reply count unchanged). Visible streaming
 *  and Abort are monotonic: a delayed snapshot cannot resurrect them after
 *  the overlaid owner is terminal and idle. The wire `internalBusy` stays
 *  authoritative (a backend Auto Review/Highlight owner is never zeroed);
 *  thread ownership only adds busy on top of it. */
export function overlayReviewLifecycle(
  snapshot: CodeReviewSnapshot,
  lifecycle: ReviewLifecycleState,
): CodeReviewSnapshot {
  const seen = new Set<string>();
  const threads: ReviewThread[] = snapshot.threads.map((thread) => {
    const key = hunkKey(thread.identity);
    seen.add(key);
    const live = lifecycle.threads[key];
    if (!live) {
      const base = thread.isCancellable ? { ...thread, isCancellable: false } : thread;
      return base.isStreaming
        ? { ...base, isStreaming: false, replyStatus: base.replyStatus === 'idle' ? base.replyStatus : 'completed' }
        : base;
    }
    const streaming = reviewReplyIsLive(live.reply.status);
    return {
      ...thread,
      comments: thread.comments.length >= live.comments.length ? thread.comments : live.comments,
      isStreaming: streaming,
      isCancellable: live.isCancellable,
      replyStatus: live.reply.status,
      replyId: live.reply.replyId || thread.replyId,
      generation: live.reply.generation || thread.generation,
      streamingText: streaming ? live.reply.streamingText : '',
      streamingThinking: streaming ? live.reply.streamingThinking : '',
      toolCalls: live.reply.tools,
    };
  });
  for (const live of Object.values(lifecycle.threads)) {
    if (seen.has(live.key) || !live.identity) continue;
    const streaming = reviewReplyIsLive(live.reply.status);
    threads.push({
      identity: live.identity,
      comments: live.comments,
      streamingText: streaming ? live.reply.streamingText : '',
      streamingThinking: streaming ? live.reply.streamingThinking : '',
      error: live.reply.status === 'failed' ? 'Review failed' : null,
      stale: false,
      isStreaming: streaming,
      isCancellable: live.isCancellable,
      generation: live.reply.generation,
      replyId: live.reply.replyId,
      replyStatus: live.reply.status,
      toolCalls: live.reply.tools,

    });
  }
  const activeCount = threads.reduce((count, thread) => count + (thread.isStreaming ? 1 : 0), 0);
  const owned = activeCount > 0 || threads.some((thread) => thread.isCancellable);
  return {
    ...snapshot,
    threads,
    isStreaming: activeCount > 0,
    activeCount,
    // The wire internalBusy is the backend authority (Auto Review/Highlight
    // batch owners and queued hunk work); thread ownership only ever ADDS
    // busy — a terminal reply lifecycle can never zero a backend owner.
    internalBusy: owned || snapshot.internalBusy,
  };
}


// ---------------------------------------------------------------------------
// Desktop thread-column width (CSS var + localStorage). Bounds are pure so
// the panel/resizer and regression tests share one clamp.
// ---------------------------------------------------------------------------

export const CODE_REVIEW_THREAD_WIDTH_STORAGE_KEY = 'rpi-code-review-thread-width';
export const CODE_REVIEW_THREAD_WIDTH_MIN = 240;
export const CODE_REVIEW_THREAD_WIDTH_MAX = 480;
export const CODE_REVIEW_THREAD_WIDTH_DEFAULT = 280;
export const CODE_REVIEW_THREAD_WIDTH_STEP = 16;

/** Minimal storage surface for width persistence (localStorage-compatible). */
export interface CodeReviewWidthStorage {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
}

/** Clamp a candidate thread width into the supported desktop range. */
export function clampCodeReviewThreadWidth(value: number): number {
  if (!Number.isFinite(value)) return CODE_REVIEW_THREAD_WIDTH_DEFAULT;
  return Math.min(
    CODE_REVIEW_THREAD_WIDTH_MAX,
    Math.max(CODE_REVIEW_THREAD_WIDTH_MIN, Math.round(value)),
  );
}

/** Read + clamp a stored thread width; storage errors / missing → default. */
export function readStoredCodeReviewThreadWidth(
  storage: CodeReviewWidthStorage | null | undefined,
): number {
  if (!storage) return CODE_REVIEW_THREAD_WIDTH_DEFAULT;
  try {
    const raw = storage.getItem(CODE_REVIEW_THREAD_WIDTH_STORAGE_KEY);
    if (raw == null || raw === '') return CODE_REVIEW_THREAD_WIDTH_DEFAULT;
    return clampCodeReviewThreadWidth(Number(raw));
  } catch {
    return CODE_REVIEW_THREAD_WIDTH_DEFAULT;
  }
}

/** Persist a clamped thread width; storage errors are swallowed (private mode). */
export function writeStoredCodeReviewThreadWidth(
  storage: CodeReviewWidthStorage | null | undefined,
  width: number,
): number {
  const next = clampCodeReviewThreadWidth(width);
  if (!storage) return next;
  try {
    storage.setItem(CODE_REVIEW_THREAD_WIDTH_STORAGE_KEY, String(next));
  } catch {
    /* private mode / quota: width still applies in-session */
  }
  return next;
}

/** Keyboard step for the thread-column separator (direction +1 grows, -1 shrinks; resizer is left of the comments column so ArrowLeft maps to +1). */
export function stepCodeReviewThreadWidth(current: number, direction: -1 | 1): number {
  return clampCodeReviewThreadWidth(current + direction * CODE_REVIEW_THREAD_WIDTH_STEP);
}

// ---------------------------------------------------------------------------
// Desktop files-column width (CSS var + localStorage). Mirrors the
// thread-column block above: pure bounds shared by the resizer and tests.
// ---------------------------------------------------------------------------

export const CODE_REVIEW_FILES_WIDTH_STORAGE_KEY = 'rpi-code-review-files-width';
export const CODE_REVIEW_FILES_WIDTH_MIN = 180;
export const CODE_REVIEW_FILES_WIDTH_MAX = 480;
export const CODE_REVIEW_FILES_WIDTH_DEFAULT = 224;
export const CODE_REVIEW_FILES_WIDTH_STEP = 24;

/** Clamp a candidate files width into the supported desktop range. */
export function clampCodeReviewFilesWidth(value: number): number {
  if (!Number.isFinite(value)) return CODE_REVIEW_FILES_WIDTH_DEFAULT;
  return Math.min(
    CODE_REVIEW_FILES_WIDTH_MAX,
    Math.max(CODE_REVIEW_FILES_WIDTH_MIN, Math.round(value)),
  );
}

/** Read + clamp a stored files width; storage errors / missing → default. */
export function readStoredCodeReviewFilesWidth(
  storage: CodeReviewWidthStorage | null | undefined,
): number {
  if (!storage) return CODE_REVIEW_FILES_WIDTH_DEFAULT;
  try {
    const raw = storage.getItem(CODE_REVIEW_FILES_WIDTH_STORAGE_KEY);
    if (raw == null || raw === '') return CODE_REVIEW_FILES_WIDTH_DEFAULT;
    return clampCodeReviewFilesWidth(Number(raw));
  } catch {
    return CODE_REVIEW_FILES_WIDTH_DEFAULT;
  }
}

/** Persist a clamped files width; storage errors are swallowed (private mode). */
export function writeStoredCodeReviewFilesWidth(
  storage: CodeReviewWidthStorage | null | undefined,
  width: number,
): number {
  const next = clampCodeReviewFilesWidth(width);
  if (!storage) return next;
  try {
    storage.setItem(CODE_REVIEW_FILES_WIDTH_STORAGE_KEY, String(next));
  } catch {
    /* private mode / quota: width still applies in-session */
  }
  return next;
}

/** Keyboard step for the files-column separator (direction +1 grows, -1
 *  shrinks; this resizer sits RIGHT of the files column so ArrowRight maps
 *  to +1 — opposite of the thread separator). */
export function stepCodeReviewFilesWidth(current: number, direction: -1 | 1): number {
  return clampCodeReviewFilesWidth(current + direction * CODE_REVIEW_FILES_WIDTH_STEP);
}

// ---------------------------------------------------------------------------
// Finding badges. Counts and the highest category come only from
// snapshot.reviewFindings. A hunk with findings: [] is reviewed-clean and
// contributes nothing. An absent collection is unreviewed.
// ---------------------------------------------------------------------------

/** Sentinel directory key grouping root-level files (no `/` in the path). */
export const CODE_REVIEW_ROOT_DIR_KEY = '(root)';

export interface AutoReviewAggregate {
  issues: number;
  /** Highest category in the group. Display precedence only. */
  category: ReviewFindingCategory;
}

/** Display precedence: higher wins. Not a claim that every counted finding shares it. */
const CATEGORY_RANK: Record<ReviewFindingCategory, number> = {
  critical: 3,
  overdesign: 2,
  naming: 1,
};

function foldFinding(
  existing: AutoReviewAggregate | undefined,
  category: ReviewFindingCategory,
): AutoReviewAggregate {
  return {
    issues: (existing?.issues ?? 0) + 1,
    category: existing && CATEGORY_RANK[existing.category] >= CATEGORY_RANK[category]
      ? existing.category
      : category,
  };
}

function directoryKeys(path: string): string[] {
  const keys: string[] = [];
  let slash = path.lastIndexOf('/');
  while (slash > 0) {
    const dir = path.slice(0, slash);
    keys.push(dir);
    slash = dir.lastIndexOf('/');
  }
  if (keys.length === 0) keys.push(CODE_REVIEW_ROOT_DIR_KEY);
  return keys;
}

/**
 * UI tone for a category. Critical is danger; overdesign and naming keep
 * distinct policy tones. Clean and absence render no mark.
 */
export function reviewCategoryTone(
  category: ReviewCategory | null | undefined,
): 'danger' | 'warn' | 'info' | null {
  if (category === 'critical') return 'danger';
  if (category === 'overdesign') return 'warn';
  if (category === 'naming') return 'info';
  return null;
}

/** Highest category among a hunk's findings; null when the hunk has none. */
export function findingsCategory(
  findings: readonly ReviewFinding[],
): ReviewFindingCategory | null {
  let best: ReviewFindingCategory | null = null;
  for (const finding of findings) {
    if (!best || CATEGORY_RANK[finding.category] > CATEGORY_RANK[best]) best = finding.category;
  }
  return best;
}

/** Issue counts per file path. Unreviewed and reviewed-clean paths are absent. */
export function aggregateFindingsByFile(
  results: readonly HunkReviewResult[] | null,
): Map<string, AutoReviewAggregate> {
  const out = new Map<string, AutoReviewAggregate>();
  if (!results) return out;
  for (const result of results) {
    for (const finding of result.findings) {
      const path = result.identity.path;
      out.set(path, foldFinding(out.get(path), finding.category));
    }
  }
  return out;
}

/** Issue counts per directory prefix, including `(root)` for root-level files. */
export function aggregateFindingsByDirectory(
  results: readonly HunkReviewResult[] | null,
): Map<string, AutoReviewAggregate> {
  const out = new Map<string, AutoReviewAggregate>();
  if (!results) return out;
  for (const result of results) {
    const keys = directoryKeys(result.identity.path);
    for (const finding of result.findings) {
      for (const key of keys) out.set(key, foldFinding(out.get(key), finding.category));
    }
  }
  return out;
}

export interface OrderedReviewFinding {
  resultIndex: number;
  findingIndex: number;
  identity: HunkIdentity;
  finding: ReviewFinding;
  /** Index of the first matching numbered row, or -1 when the hunk body is not loaded. */
  firstRow: number;
  lastRow: number;
  /** File-global index of firstRow. -1 when the current hunk body is absent. */
  fileRow: number;
}

export interface ReviewHunkBody {
  identity: HunkIdentity;
  lines: readonly DiffLine[];
}

function rowNumber(line: DiffLine, side: ReviewFindingSide): number | null {
  if (line.kind === 'meta') return null;
  if (side === 'old') return line.kind === 'addition' ? null : line.oldNo ?? null;
  return line.kind === 'deletion' ? null : line.newNo ?? null;
}

/** Numbered source rows of one hunk that intersect one finding. Meta rows never match. */
export function findingRowIndexes(
  lines: readonly DiffLine[],
  finding: Pick<ReviewFinding, 'side' | 'startLine' | 'endLine'>,
): number[] {
  const indexes: number[] = [];
  for (let index = 0; index < lines.length; index += 1) {
    const lineNo = rowNumber(lines[index], finding.side);
    if (lineNo != null && lineNo >= finding.startLine && lineNo <= finding.endLine) indexes.push(index);
  }
  return indexes;
}
export function findingRowsCoverRange(
  lines: readonly DiffLine[],
  finding: Pick<ReviewFinding, 'side' | 'startLine' | 'endLine'>,
  rows: readonly number[],
): boolean {
  const seen: Record<number, true> = {};
  for (const row of rows) {
    const lineNo = rowNumber(lines[row], finding.side);
    if (lineNo != null) seen[lineNo] = true;
  }
  for (let lineNo = finding.startLine; lineNo <= finding.endLine; lineNo += 1) {
    if (!seen[lineNo]) return false;
  }
  return true;
}

/**
 * Admit only findings owned by the CURRENT snapshot: exact snapshotId plus a
 * path present in the snapshot file list. Old-snapshot results never enter
 * aggregates or navigation; a current result whose hunk body is not loaded
 * stays admitted with fileRow -1 (missing body, not wrong identity).
 */
export function reviewFindingsForSnapshot(
  snapshotId: string,
  files: readonly Pick<DiffFile, 'path'>[],
  results: readonly HunkReviewResult[] | null,
): HunkReviewResult[] | null {
  if (!results) return null;
  const paths = new Set(files.map((file) => file.path));
  return results.filter(
    (result) => result.identity.snapshotId === snapshotId && paths.has(result.identity.path),
  );
}


function sameIdentity(left: HunkIdentity, right: HunkIdentity): boolean {
  return left.snapshotId === right.snapshotId
    && left.path === right.path
    && left.oldStart === right.oldStart
    && left.oldCount === right.oldCount
    && left.newStart === right.newStart
    && left.newCount === right.newCount
    && left.contentHash === right.contentHash;
}

/** Findings for one exact hunk identity, in stored order. */
export function findingsForIdentity(
  results: readonly HunkReviewResult[] | null,
  identity: HunkIdentity,
): ReviewFinding[] {
  if (!results) return [];
  for (const result of results) {
    if (sameIdentity(result.identity, identity)) return result.findings;
  }
  return [];
}

/**
 * Global finding order: snapshot file order, hunk order, first physical
 * matching row, last matching row, old before new, category, then text.
 * Bodies that are not loaded sort after loaded rows of the same hunk and
 * keep stored order among themselves.
 */
export function orderReviewFindings(
  files: readonly Pick<DiffFile, 'path'>[],
  results: readonly HunkReviewResult[] | null,
  bodies: readonly ReviewHunkBody[] = [],
): OrderedReviewFinding[] {
  if (!results) return [];
  const fileOrder = new Map<string, number>();
  files.forEach((file, index) => {
    if (!fileOrder.has(file.path)) fileOrder.set(file.path, index);
  });
  const hunkOrder = new Map<string, number>();
  const bodyByIdentity = new Map<string, { lines: readonly DiffLine[]; fileRow: number }>();
  const fileCursor = new Map<string, number>();
  for (const body of bodies) {
    const key = `${body.identity.snapshotId}\0${hunkKey(body.identity)}`;
    const fileKey = `${body.identity.snapshotId}\0${body.identity.path}`;
    const offset = fileCursor.get(fileKey) ?? 0;
    if (!bodyByIdentity.has(key)) {
      bodyByIdentity.set(key, { lines: body.lines, fileRow: offset });
      hunkOrder.set(key, hunkOrder.size);
    }
    fileCursor.set(fileKey, offset + body.lines.length);
  }
  const ordered: OrderedReviewFinding[] = [];
  results.forEach((result, resultIndex) => {
    if (!fileOrder.has(result.identity.path)) return;
    const body = bodyByIdentity.get(`${result.identity.snapshotId}\0${hunkKey(result.identity)}`);
    result.findings.forEach((finding, findingIndex) => {
      const rows = body ? findingRowIndexes(body.lines, finding) : [];
      if (body && (rows.length === 0 || !findingRowsCoverRange(body.lines, finding, rows))) return;
      ordered.push({
        resultIndex,
        findingIndex,
        identity: result.identity,
        finding,
        firstRow: rows.length > 0 ? rows[0] : -1,
        lastRow: rows.length > 0 ? rows[rows.length - 1] : -1,
        fileRow: rows.length > 0 && body ? body.fileRow + rows[0] : -1,
      });
    });
  });
  const sideRank: Record<ReviewFindingSide, number> = { old: 0, new: 1 };
  ordered.sort((left, right) => {
    const fileDelta = (fileOrder.get(left.identity.path) ?? Number.MAX_SAFE_INTEGER)
      - (fileOrder.get(right.identity.path) ?? Number.MAX_SAFE_INTEGER);
    if (fileDelta !== 0) return fileDelta;
    const hunkDelta = (hunkOrder.get(`${left.identity.snapshotId}\0${hunkKey(left.identity)}`) ?? left.resultIndex)
      - (hunkOrder.get(`${right.identity.snapshotId}\0${hunkKey(right.identity)}`) ?? right.resultIndex);
    if (hunkDelta !== 0) return hunkDelta;
    const leftRow = left.firstRow < 0 ? Number.MAX_SAFE_INTEGER : left.firstRow;
    const rightRow = right.firstRow < 0 ? Number.MAX_SAFE_INTEGER : right.firstRow;
    if (leftRow !== rightRow) return leftRow - rightRow;
    const leftLast = left.lastRow < 0 ? Number.MAX_SAFE_INTEGER : left.lastRow;
    const rightLast = right.lastRow < 0 ? Number.MAX_SAFE_INTEGER : right.lastRow;
    if (leftLast !== rightLast) return leftLast - rightLast;
    const sideDelta = sideRank[left.finding.side] - sideRank[right.finding.side];
    if (sideDelta !== 0) return sideDelta;
    const categoryDelta = CATEGORY_RANK[right.finding.category] - CATEGORY_RANK[left.finding.category];
    if (categoryDelta !== 0) return categoryDelta;
    const summaryDelta = left.finding.summary < right.finding.summary
      ? -1
      : left.finding.summary > right.finding.summary ? 1 : 0;
    if (summaryDelta !== 0) return summaryDelta;
    return left.finding.evidence < right.finding.evidence
      ? -1
      : left.finding.evidence > right.finding.evidence ? 1 : 0;
  });
  return ordered;
}

export function findingRefMatches(ref: ReviewFindingRef | null, entry: OrderedReviewFinding): boolean {
  return ref != null && ref.findingIndex === entry.findingIndex && sameIdentity(ref.identity, entry.identity);
}

/**
 * Previous/next over the ordered list. No wrapping. With nothing selected,
 * next is the first finding and previous is the last. Endpoints stay put.
 */
export function navigateReviewFinding(
  ordered: readonly OrderedReviewFinding[],
  selected: ReviewFindingRef | null,
  direction: ReviewFindingDirection,
): OrderedReviewFinding | null {
  if (ordered.length === 0) return null;
  const index = ordered.findIndex((entry) => findingRefMatches(selected, entry));
  if (index < 0) return direction === 'next' ? ordered[0] : ordered[ordered.length - 1];
  const next = index + (direction === 'next' ? 1 : -1);
  if (next < 0 || next >= ordered.length) return null;
  return ordered[next];
}
/** True when the latest user card is an auto submission. A later manual card is not auto. */
export function threadLatestUserIsAuto(thread: ReviewThread): boolean {
  for (let index = thread.comments.length - 1; index >= 0; index -= 1) {
    const comment = thread.comments[index];
    if (comment.role !== 'user') continue;
    return comment.auto === true;
  }
  return false;
}

/** Exact auto-thread causes for a typed launch rejection. First cause plus a count stays one toast. */
function launchRejectedCause(snapshot: CodeReviewSnapshot): string | null {
  if (snapshot.autoReviewError?.code !== 'launch-rejected') return null;
  const causes: string[] = [];
  for (const thread of snapshot.threads) {
    if (!thread.error || !threadLatestUserIsAuto(thread)) continue;
    causes.push(`${thread.identity.path}: ${thread.error}`);
  }
  if (causes.length === 0) return null;
  return causes.length === 1 ? causes[0] : `${causes[0]} (+${causes.length - 1} more)`;
}

/** True when a stored finding index still names the same hunk's current array. */
export function reviewFindingRefValid(
  results: readonly HunkReviewResult[] | null,
  ref: ReviewFindingRef | null,
): ref is ReviewFindingRef {
  if (!ref || !results || !Number.isInteger(ref.findingIndex) || ref.findingIndex < 0) return false;
  for (const result of results) {
    if (sameIdentity(result.identity, ref.identity)) return ref.findingIndex < result.findings.length;
  }
  return false;
}

/**
 * Drop a selected index when its hunk's accepted findings were replaced.
 * Unrelated hunks keep the selection. An absent collection clears it.
 */
export function invalidateReviewFindingRef(
  previous: readonly HunkReviewResult[] | null,
  next: readonly HunkReviewResult[] | null,
  ref: ReviewFindingRef | null,
): ReviewFindingRef | null {
  if (!reviewFindingRefValid(next, ref)) return null;
  const before = findingsForIdentity(previous, ref.identity);
  const after = findingsForIdentity(next, ref.identity);
  if (before.length !== after.length) return null;
  for (let index = 0; index < before.length; index += 1) {
    const left = before[index];
    const right = after[index];
    if (
      left.category !== right.category
      || left.side !== right.side
      || left.startLine !== right.startLine
      || left.endLine !== right.endLine
      || left.summary !== right.summary
      || left.evidence !== right.evidence
    ) return null;
  }
  return ref;
}





/** Strict per-comment UTF-8 byte cap; mirrors the backend MAX_COMMENT_BYTES. */
export const CODE_REVIEW_MAX_COMMENT_BYTES = 8_192;

export function countFileThreads(threads: ReviewThread[], file: DiffFile): number {
  let total = 0;
  for (const thread of threads) {
    if (thread.identity.path === file.path) {
      total += countThreadComments(thread);
    }
  }
  return total;
}

/**
 * Parse `/code-review` argument tail into open revisions.
 * Accepts 0 tokens (HEAD→working tree), 1 token (`<base>`→working tree),
 * or exactly 2 (`from`, `to` commit-to-commit).
 * Returns an error message string when the arity is wrong.
 */
export function parseCodeReviewArgs(
  args: string,
): { ok: true; from?: string; to?: string; includeWorkingTree?: true } | { ok: false; error: string } {
  const trimmed = args.trim();
  if (!trimmed) return { ok: true };
  const parts = trimmed.split(/\s+/).filter(Boolean);
  if (parts.length === 1) {
    return { ok: true, from: parts[0], includeWorkingTree: true };
  }
  if (parts.length === 2) {
    return { ok: true, from: parts[0], to: parts[1] };
  }
  return {
    ok: false,
    error: 'usage: /code-review [<base> | <from> <to>]',
  };
}

export type CodeReviewOpenCommandResult =
  | { ok: true; command: Record<string, unknown> }
  | { ok: false; error: string };

export function codeReviewOpenCommand(args: {
  commit?: string;
  from?: string;
  to?: string;
  includeWorkingTree?: true;
}): CodeReviewOpenCommandResult {
  // `/commit <ref>` scope: a pinned commit/revision is mutually exclusive
  // with the from/to/includeWorkingTree operands — conflated operands are a
  // REJECTED result, never a silently narrowed scope. An explicitly empty
  // commit still rides the wire and fails clearly on the backend instead of
  // silently opening a working-tree review.
  if (args.commit !== undefined) {
    if (args.from !== undefined || args.to !== undefined || args.includeWorkingTree !== undefined) {
      return {
        ok: false,
        error: 'code_review_open commit excludes from, to, and includeWorkingTree',
      };
    }
    return { ok: true, command: { type: 'code_review_open', commit: args.commit } };
  }
  const command: Record<string, unknown> = { type: 'code_review_open' };
  if (args.from && args.to) {
    command.from = args.from;
    command.to = args.to;
  } else if (args.from && args.includeWorkingTree) {
    command.from = args.from;
    command.includeWorkingTree = true;
  }
  return { ok: true, command };
}

export function codeReviewOpenSnapshotError(data: unknown): string | null {
  const error = normalizeCodeReviewSnapshot(data).error;
  return error && error !== '' ? error : null;
}

// ---------------------------------------------------------------------------
// `/commit` chooser: recent-commit listing (`code_review_commits`) plus the
// picker search/selection decisions. Pure so the node lane exercises them
// without a DOM; panels/CommitPicker.tsx only wires them to events.
// ---------------------------------------------------------------------------

/** Backend list cap (first-parent `git log` rows per response). */
export const CODE_REVIEW_COMMITS_MAX_LIMIT = 100;

/** Git object id: full SHA-1 (40) or SHA-256 (64) lowercase hex — never
 *  assumed 40-only; shortening is the display layer's concern. */
const GIT_HASH_PATTERN = /^(?:[0-9a-f]{40}|[0-9a-f]{64})$/;

export function isValidGitHash(value: unknown): value is string {
  return typeof value === 'string' && GIT_HASH_PATTERN.test(value);
}

/** One strict `code_review_commits` row (camelCase wire). `parentCommit` is
 *  the first parent; root commits carry null + parentCount 0, merges ≥2. */
export interface CodeReviewCommitRow {
  commit: string;
  parentCommit: string | null;
  parentCount: number;
  subject: string;
  author: string;
  authorDate: string;
}

export interface CodeReviewCommitList {
  commits: CodeReviewCommitRow[];
  truncated: boolean;
}

export type CodeReviewCommitListResult =
  | { ok: true; list: CodeReviewCommitList }
  | { ok: false; error: string };

/** Strict row parse: hash-format ids, integer parentCount consistent with
 *  parentCommit (null ⇔ 0); the subject may be empty (git allows it). */
function normalizeCommitRow(raw: unknown): CodeReviewCommitRow | null {
  const record = asRecord(raw);
  if (!record) return null;
  if (!isValidGitHash(record.commit)) return null;
  if (record.parentCommit !== null && !isValidGitHash(record.parentCommit)) return null;
  if (
    typeof record.parentCount !== 'number'
    || !Number.isInteger(record.parentCount)
    || record.parentCount < 0
  ) {
    return null;
  }
  if ((record.parentCommit === null) !== (record.parentCount === 0)) return null;
  if (typeof record.subject !== 'string') return null;
  if (typeof record.author !== 'string' || typeof record.authorDate !== 'string') return null;
  return {
    commit: record.commit,
    parentCommit: record.parentCommit,
    parentCount: record.parentCount,
    subject: record.subject,
    author: record.author,
    authorDate: record.authorDate,
  };
}

/**
 * Strict `code_review_commits` parse. Unusable shapes AND any malformed row
 * fail with an error — rows are never silently omitted and an empty success
 * is only ever a genuinely empty `commits: []` from a healthy backend, never
 * a fallback.
 */
export function normalizeCodeReviewCommits(data: unknown): CodeReviewCommitListResult {
  const record = asRecord(data);
  if (!record) {
    return { ok: false, error: 'code_review_commits response was not an object' };
  }
  const rawCommits = record.commits;
  if (!Array.isArray(rawCommits)) {
    return { ok: false, error: 'code_review_commits response is missing a commits array' };
  }
  if (typeof record.truncated !== 'boolean') {
    return { ok: false, error: 'code_review_commits response is missing a truncated flag' };
  }
  const commits: CodeReviewCommitRow[] = [];
  for (const raw of rawCommits) {
    const row = normalizeCommitRow(raw);
    if (!row) {
      return { ok: false, error: 'code_review_commits response had a malformed commit row' };
    }
    commits.push(row);
  }
  return { ok: true, list: { commits, truncated: record.truncated } };
}

/** Wire payload for `code_review_commits`; limit clamps to the 1..100 cap. */
export function buildCodeReviewCommitsPayload(limit?: number): Record<string, unknown> {
  const payload: Record<string, unknown> = { type: 'code_review_commits' };
  if (limit !== undefined && Number.isFinite(limit) && limit >= 1) {
    payload.limit = Math.min(Math.floor(limit), CODE_REVIEW_COMMITS_MAX_LIMIT);
  }
  return payload;
}

/** Short display label width — backend `short_commit_label`/COMMIT_LABEL_HEX parity. */
export const CODE_REVIEW_COMMIT_LABEL_HEX = 12;

/** Short display label — backend `short_commit_label` parity (12 hex). */
export function shortCommitLabel(commit: string): string {
  return commit.slice(0, CODE_REVIEW_COMMIT_LABEL_HEX);
}

/** Case-insensitive commit search over hash, subject, and author; an
 *  empty/whitespace query returns every row (mirrors filterCommands). */
export function filterCommitRows(
  rows: CodeReviewCommitRow[],
  query: string,
): CodeReviewCommitRow[] {
  const q = query.trim().toLowerCase();
  if (!q) return rows;
  return rows.filter(
    (row) =>
      row.commit.toLowerCase().includes(q) ||
      row.subject.toLowerCase().includes(q) ||
      row.author.toLowerCase().includes(q),
  );
}

export type CommitPickerEnterAction =
  | { kind: 'none' }
  | { kind: 'row'; ref: string }
  | { kind: 'typed'; ref: string }
  | { kind: 'invalid'; message: string };

/**
 * Enter in the commit picker: the active filtered row wins (its full hash);
 * with no row under the caret the typed text becomes the ref directly — a
 * multi-token tail can never be one ref, so it fails with the usage line
 * (interactive_commands.rs COMMIT_INSPECT_USAGE parity).
 */
export function commitPickerEnterAction(
  filteredRows: CodeReviewCommitRow[],
  activeIndex: number,
  query: string,
): CommitPickerEnterAction {
  const row = activeIndex >= 0 ? filteredRows[activeIndex] : undefined;
  if (row) return { kind: 'row', ref: row.commit };
  const ref = query.trim();
  if (!ref) return { kind: 'none' };
  if (/\s/.test(ref)) return { kind: 'invalid', message: 'Usage: /commit [<ref>]' };
  return { kind: 'typed', ref };
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

export function fileChangeStats(file: DiffFile): { insertions: number; deletions: number } {
  if (
    file.hunks.length === 0
    || file.truncated
    || file.bodyState === 'preview'
    || file.bodyState === 'onDemand'
    || file.bodyState === 'byteCapped'
  ) {
    return { insertions: file.insertions, deletions: file.deletions };
  }
  let insertions = 0;
  let deletions = 0;
  for (const hunk of file.hunks) {
    for (const line of hunk.lines) {
      if (line.kind === 'addition') insertions += 1;
      else if (line.kind === 'deletion') deletions += 1;
    }
  }
  return { insertions, deletions };
}

export function loadedDiffChangeStats(loaded: LoadedDiff): { insertions: number; deletions: number } {
  let insertions = 0;
  let deletions = 0;
  for (const hunk of loaded.hunks) {
    for (const line of hunk.lines) {
      if (line.kind === 'addition') insertions += 1;
      else if (line.kind === 'deletion') deletions += 1;
    }
  }
  return { insertions, deletions };
}

export function loadedDiffAuthoritativeForSnapshot(
  loaded: LoadedDiff | null | undefined,
  snapshotId: string,
): loaded is LoadedDiff {
  return !!loaded
    && loaded.snapshotId === snapshotId
    && loaded.hunksComplete
    && !loaded.byteCapped
    && loaded.hunks.length > 0;
}

export function fileListChangeStats(
  file: DiffFile,
  loaded?: LoadedDiff | null,
  snapshotId?: string,
): { insertions: number; deletions: number } {
  const catalog = fileChangeStats(file);
  if (!loaded) return catalog;
  if (!loadedDiffAuthoritativeForSnapshot(loaded, snapshotId ?? loaded.snapshotId)) {
    return catalog;
  }
  const loadedStats = loadedDiffChangeStats(loaded);
  if (loadedStats.insertions + loadedStats.deletions === 0
    && catalog.insertions + catalog.deletions > 0) {
    return catalog;
  }
  return loadedStats;
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
    const { insertions, deletions } = fileChangeStats(file);
    nodes.push({
      id: `file:${rel}`,
      name,
      path: rel,
      kind: 'file',
      depth,
      children: [],
      fileIndex,
      status: file.status,
      insertions,
      deletions,
    });
    if (parentIdx !== null && parentIdx >= 0) {
      nodes[parentIdx].children.push(idx);
      let walk: number | null = parentIdx;
      while (walk !== null) {
        nodes[walk].insertions = nodes[walk].insertions + insertions;
        nodes[walk].deletions = nodes[walk].deletions + deletions;
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
// Bounded single-file diff paging (code_review_file_diff RPC).
// ---------------------------------------------------------------------------

/** Per-render soft cap (matches the backend MAX_FILE_RENDER_LINES). */
export const FILE_RENDER_SOFT_CAP = 4000;
export const FILE_DIFF_MAX_UI_LINES = 20_000;

export interface FileDiffPageHunk {
  /** 0-based hunk index across the file's hunks. */
  index: number;
  header: string;
  oldStart: number;
  oldCount: number;
  newStart: number;
  newCount: number;
  /** SHA-256 over the COMPLETE hunk (never the page fragment). */
  contentHash: string;
  /** Total lines of the complete hunk; comment-ready once merged lines reach it. */
  totalLines: number;
  /** Backend parse was complete (not cut by the per-file byte cap). */
  complete: boolean;
  /** Hunk-local line offset where this page's subset starts. */
  lineStart: number;
  lines: DiffLine[];
}

export interface FileDiffPage {
  snapshotId: string;
  path: string;
  previousPath?: string;
  binary: boolean;
  status: FileStatus;
  hunks: FileDiffPageHunk[];
  cursor: number;
  nextCursor?: number;
  hasMore: boolean;
  totalLines: number;
  hunkCount: number;
  byteCapped: boolean;
}

/** Defensive normalization of a code_review_file_diff response. */
export function normalizeFileDiffPage(raw: unknown): FileDiffPage | null {
  const obj = asRecord(raw);
  if (!obj) return null;
  const path = stringField(obj.path);
  const snapshotId = stringField(obj.snapshotId);
  if (!path || !snapshotId) return null;
  const status = stringField(obj.status);
  const nextCursor =
    typeof obj.nextCursor === 'number' && Number.isFinite(obj.nextCursor)
      ? Math.max(0, Math.floor(obj.nextCursor))
      : undefined;
  const hunks = Array.isArray(obj.hunks)
    ? obj.hunks
        .map((rawHunk): FileDiffPageHunk | null => {
          const h = asRecord(rawHunk);
          if (!h) return null;
          const header = stringField(h.header);
          if (!header) return null;
          const contentHash = stringField(h.contentHash);
          if (!contentHash) return null;
          const lines = Array.isArray(h.lines)
            ? h.lines.map(normalizeLine).filter((l): l is DiffLine => l !== null)
            : [];
          return {
            index: uintField(h.index),
            header,
            oldStart: uintField(h.oldStart),
            oldCount: uintField(h.oldCount),
            newStart: uintField(h.newStart),
            newCount: uintField(h.newCount),
            contentHash,
            totalLines: uintField(h.totalLines),
            complete: boolField(h.complete),
            lineStart: uintField(h.lineStart),
            lines,
          };
        })
        .filter((h): h is FileDiffPageHunk => h !== null)
    : [];
  const hunkCount = uintField(obj.hunkCount);
  const maxIndex = hunks.reduce((max, h) => Math.max(max, h.index + 1), 0);
  return {
    snapshotId,
    path,
    previousPath: optionalString(obj.previousPath),
    binary: boolField(obj.binary),
    status: FILE_STATUSES[status] ? (status as FileStatus) : 'changed',
    hunks,
    cursor: uintField(obj.cursor),
    nextCursor,
    hasMore: boolField(obj.hasMore),
    totalLines: uintField(obj.totalLines),
    hunkCount: Math.max(hunkCount, maxIndex),
    byteCapped: boolField(obj.byteCapped),
  };
}

/**
 * One hunk of the merged per-file diff. The snapshot seeds these from its
 * own hunks (instant first render); backend page descriptors then UPGRADE
 * the metadata (authoritative ranges/content hash/total lines/completeness)
 * and merge line subsets. A hunk is comment-ready only when
 * `complete && lines.length === totalLines` — page-split or byte-capped
 * hunks are never exposed as selectable.
 */
export interface LoadedHunk {
  index: number;
  header: string;
  oldStart: number;
  oldCount: number;
  newCount: number;
  newStart: number;
  contentHash: string;
  /** Lines of the complete hunk (backend descriptor; snapshot seed approx). */
  totalLines: number;
  /** Backend parse was complete (not cut by the per-file byte cap). */
  complete: boolean;
  /** Merged lines across the snapshot seed and pages, in hunk order. */
  lines: DiffLine[];
}

/**
 * Accumulated loaded diff for one file. The merged hunks REPLACE the
 * snapshot's hunk structure for rendering/selection once loaded (catalog
 * stats and snapshot identity stay with the snapshot file entry); `lines` is
 * the derived flat view used for window math. The panel owns one of these
 * per selected file.
 */
export interface LoadedDiff {
  snapshotId: string;
  path: string;
  /** Merged hunks in index order (snapshot seed first, then page merges). */
  hunks: LoadedHunk[];
  /** Derived flat line stream (window math / rendering), hunk order. */
  lines: DiffLine[];
  /** Backend total hunks (0 until the first page arrives). */
  hunkCount: number;
  /** Backend cursor for the next page (=== lines.length when none pending). */
  nextCursor: number;
  hasMoreBackend: boolean;
  totalLines: number;
  /** Snapshot render-cap and/or page byte-cap provenance (content may exist beyond loaded). */
  backendTruncated: boolean;
  /** True when the backend per-file byte cap was hit: no further pages exist. */
  byteCapped: boolean;
  /** True when every hunk descriptor the backend knows has been merged. */
  hunksComplete: boolean;
  loading: boolean;
  error: string | null;
}

export function isDiffPlaceholder(file: DiffFile): boolean {
  return file.bodyState === 'onDemand' && !file.binary && file.hunks.length === 0;
}

/** Content/context line counts of a hunk (meta lines excluded). */
function hunkCounts(hunk: Pick<DiffHunk, 'lines'>): { content: number; context: number } {
  let content = 0;
  let context = 0;
  for (const line of hunk.lines) {
    if (line.kind === 'meta') continue;
    content += 1;
    if (line.kind === 'context') context += 1;
  }
  return { content, context };
}

/**
 * Whether a hunk's parsed body matches its header counts (the same
 * completeness check the backend applies to byte-capped loads). A hunk cut
 * mid-body never satisfies `content === oldCount + newCount - context`, so a
 * globally-truncated snapshot's partial last hunk is detected here too.
 */
export function hunkIsComplete(
  hunk: Pick<DiffHunk, 'oldCount' | 'newCount' | 'lines'>,
): boolean {
  const { content, context } = hunkCounts(hunk);
  return content === hunk.oldCount + hunk.newCount - context;
}

/** True when a merged hunk is comment-ready: complete and fully merged. */
export function loadedHunkReady(hunk: LoadedHunk): boolean {
  return hunk.complete && hunk.lines.length === hunk.totalLines;
}

/** Seed a LoadedDiff from the snapshot's file entry. */
export function initLoadedDiff(snapshotId: string, file: DiffFile): LoadedDiff {
  const hunks: LoadedHunk[] = file.hunks.map((hunk, index) => ({
    index,
    header: hunk.header,
    oldStart: hunk.oldStart,
    oldCount: hunk.oldCount,
    newStart: hunk.newStart,
    newCount: hunk.newCount,
    contentHash: hunk.contentHash,
    totalLines: hunk.lines.length,
    complete: hunkIsComplete(hunk),
    lines: hunk.lines.slice(),
  }));
  const lines: DiffLine[] = [];
  for (const hunk of hunks) for (const line of hunk.lines) lines.push(line);
  const needsPages =
    file.bodyState === 'preview' || file.bodyState === 'onDemand' || file.bodyState === 'byteCapped';
  return {
    snapshotId,
    path: file.path,
    hunks,
    lines,
    hunkCount: 0,
    // The eager per-file fetch starts at cursor 0 (NOT at the seeded line
    // count): the backend pages are the authoritative hunk source and
    // replace/upgrade the snapshot seed. Exact dedupe keeps the merged lines
    // byte-identical to the backend stream (see appendFileDiffPage).
    nextCursor: 0,
    hasMoreBackend: needsPages && !file.binary,
    totalLines: lines.length,
    backendTruncated: needsPages,
    byteCapped: false,
    // The backend stream is complete only once a terminal page (hasMore
    // false) has been consumed — the loop forces at least one page for
    // non-binary files regardless of hasMoreBackend.
    hunksComplete: file.binary,
    loading: false,
    error: null,
  };
}

/** Rebuild the flat line stream from the merged hunks, in index order. */
function flattenHunks(hunks: LoadedHunk[]): DiffLine[] {
  const lines: DiffLine[] = [];
  for (const hunk of hunks) for (const line of hunk.lines) lines.push(line);
  return lines;
}

/** Whether the backend line stream has been fully consumed (a terminal page
 *  with hasMore=false arrived), meaning every hunk's descriptors AND line
 *  subsets are merged. */
function hunksCompleteFor(hasMore: boolean): boolean {
  return !hasMore;
}

/**
 * Merge a backend page into the loaded diff. Stale guards: the page's
 * snapshot id and path must match, and its cursor must equal the expected next
 * cursor (a duplicate or out-of-order page is dropped). On a successful merge
 * every descriptor upgrades its hunk's metadata (header/ranges/content hash/
 * total lines/completeness — the backend is authoritative) and merges the
 * page's line subset at `lineStart`. Exact dedupe rules (identity matches
 * mean byte-identical hunk text, so the seed is a prefix/superset of the
 * page subsets):
 *   - subset fully inside the merged lines → skipped (seed already covers it)
 *   - subset starting at/after the merged end → appended (continuation)
 *   - partial overlap → only the not-yet-merged tail is appended
 * A descriptor whose identity differs from the seed (a globally-truncated
 * partial hunk replaced by the complete per-file hunk) REPLACES the seed
 * wholesale. Page-split hunks therefore merge deterministically by index in
 * cursor order with no duplicated lines.
 */
export function appendFileDiffPage(state: LoadedDiff, page: FileDiffPage): LoadedDiff {
  if (page.snapshotId !== state.snapshotId || page.path !== state.path) return state;
  if (page.cursor !== state.nextCursor) return state;
  let hunks = state.hunks;
  for (const descriptor of page.hunks) {
    const existing = hunks.find((hunk) => hunk.index === descriptor.index);
    if (!existing) {
      hunks = hunks.concat([
        {
          index: descriptor.index,
          header: descriptor.header,
          oldStart: descriptor.oldStart,
          oldCount: descriptor.oldCount,
          newStart: descriptor.newStart,
          newCount: descriptor.newCount,
          contentHash: descriptor.contentHash,
          totalLines: descriptor.totalLines,
          complete: descriptor.complete,
          lines: descriptor.lines.slice(),
        },
      ]);
      continue;
    }
    const identityMatches =
      existing.oldStart === descriptor.oldStart &&
      existing.oldCount === descriptor.oldCount &&
      existing.newStart === descriptor.newStart &&
      existing.newCount === descriptor.newCount &&
      existing.contentHash === descriptor.contentHash;
    if (!identityMatches) {
      // The per-file data supersedes the snapshot seed (e.g. a partial
      // globally-truncated hunk replaced by the complete hunk).
      hunks = hunks.map((hunk) =>
        hunk.index === descriptor.index
          ? {
              index: descriptor.index,
              header: descriptor.header,
              oldStart: descriptor.oldStart,
              oldCount: descriptor.oldCount,
              newStart: descriptor.newStart,
              newCount: descriptor.newCount,
              contentHash: descriptor.contentHash,
              totalLines: descriptor.totalLines,
              complete: descriptor.complete,
              lines: descriptor.lines.slice(),
            }
          : hunk,
      );
      continue;
    }
    // Identity matches: the seed lines are a prefix/superset of the backend
    // subsets (same hash ⟹ same text). Apply the AUTHORITATIVE descriptor
    // metadata (header/ranges/content hash/total lines/completeness), then
    // merge only the not-yet-merged tail: fully-covered subsets dedupe,
    // continuations append, partial overlaps append the missing tail.
    let mergedLines = existing.lines;
    const subsetEnd = descriptor.lineStart + descriptor.lines.length;
    if (subsetEnd > existing.lines.length) {
      const appendFrom = Math.max(descriptor.lineStart, existing.lines.length);
      mergedLines = existing.lines.concat(descriptor.lines.slice(appendFrom - descriptor.lineStart));
    }
    hunks = hunks.map((hunk) =>
      hunk.index === descriptor.index
        ? {
            ...hunk,
            header: descriptor.header,
            oldStart: descriptor.oldStart,
            oldCount: descriptor.oldCount,
            newStart: descriptor.newStart,
            newCount: descriptor.newCount,
            contentHash: descriptor.contentHash,
            totalLines: descriptor.totalLines,
            complete: descriptor.complete,
            lines: mergedLines,
          }
        : hunk,
    );
  }
  hunks.sort((a, b) => a.index - b.index);
  const lines = flattenHunks(hunks);
  const nextCursor = page.nextCursor ?? lines.length;
  const hunkCount = Math.max(state.hunkCount, page.hunkCount);
  return {
    ...state,
    hunks,
    lines,
    hunkCount,
    nextCursor,
    hasMoreBackend: page.hasMore,
    totalLines: Math.max(state.totalLines, page.totalLines),
    backendTruncated: page.byteCapped || state.backendTruncated,
    byteCapped: page.byteCapped || state.byteCapped,
    hunksComplete: hunksCompleteFor(page.hasMore),
    loading: false,
    error: null,
  };
}

export const FILE_WINDOW_CHUNK_LINES = FILE_RENDER_SOFT_CAP;

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

export function planDiffWindow(
  loaded: LoadedDiff,
  windowLimit: number,
  chunk = FILE_WINDOW_CHUNK_LINES,
): DiffWindowPlan {
  const known = Math.max(loaded.lines.length, loaded.totalLines);
  const localAvailable = Math.min(loaded.lines.length, FILE_DIFF_MAX_UI_LINES);
  const backendAvailable = loaded.hasMoreBackend;
  // While backend pages are still arriving, the true total is unknown and
  // the window may plan up to the UI hard cap; once the stream is fully
  // consumed, the loaded total is authoritative and bounds the window.
  const totalAvailable =
    backendAvailable && !loaded.hunksComplete
      ? FILE_DIFF_MAX_UI_LINES
      : Math.min(known, FILE_DIFF_MAX_UI_LINES);
  const current = Math.max(0, Math.min(windowLimit, totalAvailable));
  const hardCapped =
    known > FILE_DIFF_MAX_UI_LINES || (backendAvailable && current >= FILE_DIFF_MAX_UI_LINES);
  const target = Math.max(current, Math.min(current + chunk, totalAvailable));
  const needsFetch = target > localAvailable && backendAvailable && !loaded.hunksComplete;
  return {
    target,
    localAvailable,
    totalAvailable,
    canLoadMore: target > current,
    needsFetch,
    hardCapped,
  };
}


export const DIFF_NEAR_BOTTOM_THRESHOLD_PX = 240;

export interface DiffScrollViewport {
  scrollTop: number;
  clientHeight: number;
  scrollHeight: number;
}

export function isNearDiffBottom(
  viewport: DiffScrollViewport,
  threshold: number = DIFF_NEAR_BOTTOM_THRESHOLD_PX,
): boolean {
  if (viewport.scrollHeight <= 0 || viewport.clientHeight <= 0) return false;
  return viewport.scrollHeight - viewport.scrollTop - viewport.clientHeight <= threshold;
}

export type FileDiffStatusView =
  | { kind: 'none'; text: null } // everything renderable, nothing bounded
  | { kind: 'ui-cap'; text: string } // FILE_DIFF_MAX_UI_LINES reached
  | { kind: 'byte-cap'; text: string }; // backend per-file byte cap reached

export function fileDiffStatusView(loaded: LoadedDiff, windowLimit: number): FileDiffStatusView {
  const cap = FILE_DIFF_MAX_UI_LINES.toLocaleString();
  if (loaded.byteCapped) {
    const loadedLines = Math.min(windowLimit, loaded.lines.length).toLocaleString();
    return {
      kind: 'byte-cap',
      text: `Loaded ${loadedLines} lines — the file body exceeds the backend size limit; later lines are not available and the total line count is unknown.`,
    };
  }
  const plan = planDiffWindow(loaded, windowLimit);
  if (!plan.canLoadMore && plan.hardCapped) {
    const rendered = Math.min(windowLimit, FILE_DIFF_MAX_UI_LINES).toLocaleString();
    if (loaded.hunksComplete) {
      const total = Math.max(loaded.lines.length, loaded.totalLines).toLocaleString();
      return {
        kind: 'ui-cap',
        text: `Loaded ${rendered} of ${total} lines — the UI display limit is ${cap} lines.`,
      };
    }
    return {
      kind: 'ui-cap',
      text: `Loaded ${rendered} lines — the UI display limit is ${cap} lines; the total line count is unknown until loading completes.`,
    };
  }
  return { kind: 'none', text: null };
}

export const LOADED_DIFF_CACHE_MAX_ENTRIES = 8;
export const LOADED_DIFF_CACHE_MAX_BYTES = 32 * 1024 * 1024;

const DIFF_LINE_STRUCTURAL_BYTES = 80;
const DIFF_HUNK_STRUCTURAL_BYTES = 128;
const DIFF_LOADED_STRUCTURAL_BYTES = 256;
/** Estimated retained weight: 2 bytes per UTF-16 code unit for every string
 *  plus the structural constants — deliberately conservative, never under-counts. */

export function estimateLoadedDiffBytes(loaded: LoadedDiff): number {
  let bytes =
    DIFF_LOADED_STRUCTURAL_BYTES
    + 2 * loaded.snapshotId.length
    + 2 * loaded.path.length
    + (loaded.error ? 2 * loaded.error.length : 0);
  for (const hunk of loaded.hunks) {
    bytes += DIFF_HUNK_STRUCTURAL_BYTES;
    bytes += 2 * hunk.header.length;
    bytes += 2 * hunk.contentHash.length;
    for (const line of hunk.lines) {
      bytes += DIFF_LINE_STRUCTURAL_BYTES;
      bytes += 2 * line.text.length;
    }
  }
  return bytes;
}


export interface LoadedDiffCache {
  entries: Record<string, LoadedDiff>;
  order: string[];
}

export interface LoadedDiffCacheGuards {
  currentPath?: string | null;
  inFlightPaths?: readonly string[];
}

export interface LoadedDiffCacheResult {
  cache: LoadedDiffCache;
  evicted: string[];
}

export function emptyLoadedDiffCache(): LoadedDiffCache {
  return { entries: {}, order: [] };
}

function isProtected(path: string, guards: LoadedDiffCacheGuards): boolean {
  if (guards.currentPath && guards.currentPath === path) return true;
  return (guards.inFlightPaths ?? []).includes(path);
}

function enforceLoadedDiffCacheCaps(
  cache: LoadedDiffCache,
  guards: LoadedDiffCacheGuards,
): LoadedDiffCacheResult {
  const evicted: string[] = [];
  let entries = cache.entries;
  let order = cache.order;
  let bytes = 0;
  for (const path of order) bytes += estimateLoadedDiffBytes(entries[path]);
  while (
    (order.length > LOADED_DIFF_CACHE_MAX_ENTRIES || bytes > LOADED_DIFF_CACHE_MAX_BYTES) &&
    order.some((path) => !isProtected(path, guards))
  ) {
    const victimIndex = order.findIndex((path) => !isProtected(path, guards));
    const victim = order[victimIndex];
    bytes -= estimateLoadedDiffBytes(entries[victim]);
    if (entries === cache.entries) entries = { ...cache.entries };
    if (order === cache.order) order = cache.order.slice();
    delete entries[victim];
    order.splice(victimIndex, 1);
    evicted.push(victim);
  }
  if (evicted.length === 0) return { cache, evicted };
  return { cache: { entries, order }, evicted };
}

export function loadedDiffCachePut(
  cache: LoadedDiffCache,
  loaded: LoadedDiff,
  guards: LoadedDiffCacheGuards = {},
): LoadedDiffCacheResult {
  const order = cache.order.filter((path) => path !== loaded.path).concat([loaded.path]);
  const entries = { ...cache.entries, [loaded.path]: loaded };
  return enforceLoadedDiffCacheCaps({ entries, order }, guards);
}

export function loadedDiffCacheTouch(
  cache: LoadedDiffCache,
  path: string,
  guards: LoadedDiffCacheGuards = {},
): LoadedDiffCacheResult {
  if (!cache.entries[path]) return { cache, evicted: [] };
  const order = cache.order.filter((p) => p !== path).concat([path]);
  if (order.length === cache.order.length && order.every((p, i) => p === cache.order[i])) {
    return { cache, evicted: [] };
  }
  return enforceLoadedDiffCacheCaps({ entries: cache.entries, order }, guards);
}

// ---------------------------------------------------------------------------
// Per-file fetch latch + reconnect reconciliation (shared with the panel).
// Pure so the node-runnable regression covers the sticky-error / transport-
// suppression / restart-reopen contract without a DOM.
// ---------------------------------------------------------------------------

/** Per-path load latch for code_review_file_diff (one in-flight fetch per file). */
export interface FileDiffLoadState {
  loading: boolean;
  error: string | null;
}

/** How a code_review_file_diff fetch settled, for latch transition. */
export type FileDiffFetchOutcome =
  | { kind: 'success' }
  | { kind: 'business-error'; message: string }
  | { kind: 'transport-error' }
  | { kind: 'stale' };

/**
 * Next per-file load latch after a code_review_file_diff fetch settles.
 * Successful completion ALONE clears the error: a business failure latches
 * the message inline (explicit Retry is the only same-epoch retry), a
 * transport failure suppresses the inline error (the App toast owns it and
 * the eager loader stalls until a new connection epoch), and a stale return
 * leaves the latch to whichever newer fetch owns the path.
 */
export function nextFileDiffLoadState(
  outcome: FileDiffFetchOutcome,
  current: FileDiffLoadState,
): FileDiffLoadState {
  switch (outcome.kind) {
    case 'success':
      return { loading: false, error: null };
    case 'business-error':
      return { loading: false, error: outcome.message };
    case 'transport-error':
      return { loading: false, error: null };
    case 'stale':
      return current;
  }
  // Exhaustive fallback (unreachable for the defined union): keep the latch.
  return current;
}

/**
 * Whether the eager per-file loader may issue a code_review_file_diff fetch:
 * never while one is loading, while a business error is latched (explicit
 * Retry owns the same-epoch retry), or while a transport stall awaits the
 * next connection epoch.
 */
export function shouldEagerFileDiffFetch(
  loadState: FileDiffLoadState,
  stalled: boolean,
): boolean {
  return !loadState.loading && loadState.error == null && !stalled;
}

/** Reconcile decision for a reconnect/restart probe of the review controller. */
export type ReviewProbeOutcome =
  | { kind: 'preserve' }
  | { kind: 'reopen' }
  | { kind: 'wait' };

const CODE_REVIEW_NOT_OPEN_CODE = 'code_review_not_open';

/**
 * Classify a `code_review_snapshot` probe of the authoritative controller
 * after a new connection epoch. A live snapshot (non-empty snapshotId) means
 * the same-process controller survived — preserve it. Only the backend's
 * typed no-open code proves a process restart lost the controller.
 * Other business failures preserve the retained last-good client snapshot
 * while the caller toasts the cause through the App's single toast surface;
 * transport failures wait for the next epoch without any local surface.
 */
export function classifyReviewProbe(data: unknown, error: unknown): ReviewProbeOutcome {
  if (error) {
    if (!shouldRenderReviewError(error)) return { kind: 'wait' };
    return isNoOpenCodeReviewError(error) ? { kind: 'reopen' } : { kind: 'preserve' };
  }
  const snap = normalizeCodeReviewSnapshot(data);
  return snap.snapshotId ? { kind: 'preserve' } : { kind: 'reopen' };
}

export const CONTEXT_EXPAND_LINES = 20;

export interface DiffGap {
  oldStart: number;
  oldCount: number;
  newStart: number;
  newCount: number;
}

export type GapExpandFrom = 'start' | 'end';

export interface GapExpandState {
  gap: DiffGap;
  fromStart: number;
  fromEnd: number;
}

export function diffGapId(gap: DiffGap): string {
  return `g:${gap.oldStart}:${gap.oldCount}:${gap.newStart}:${gap.newCount}`;
}

function hunkEnd(start: number, count: number): number {
  return count === 0 ? start : start + count;
}

function pushLineGap(
  gaps: DiffGap[],
  oldFrom: number,
  oldTo: number,
  newFrom: number,
  newTo: number,
): void {
  const oldCount = Math.max(0, oldTo - oldFrom);
  const newCount = Math.max(0, newTo - newFrom);
  if (oldCount === 0 && newCount === 0) return;
  gaps.push({
    oldStart: oldCount === 0 ? 0 : oldFrom,
    oldCount,
    newStart: newCount === 0 ? 0 : newFrom,
    newCount,
  });
}

export function reviewDiffGaps(
  hunks: Array<{ oldStart: number; oldCount: number; newStart: number; newCount: number }>,
  oldLineCount: number,
  newLineCount: number,
): DiffGap[] {
  const gaps: DiffGap[] = [];
  if (hunks.length === 0) {
    pushLineGap(
      gaps,
      1,
      oldLineCount === 0 ? 1 : oldLineCount + 1,
      1,
      newLineCount === 0 ? 1 : newLineCount + 1,
    );
    return gaps;
  }
  const first = hunks[0];
  pushLineGap(gaps, 1, first.oldStart, 1, first.newStart);
  for (let i = 0; i < hunks.length - 1; i++) {
    const prev = hunks[i];
    const next = hunks[i + 1];
    pushLineGap(
      gaps,
      hunkEnd(prev.oldStart, prev.oldCount),
      next.oldStart,
      hunkEnd(prev.newStart, prev.newCount),
      next.newStart,
    );
  }
  if (oldLineCount > 0 || newLineCount > 0) {
    const last = hunks[hunks.length - 1];
    const [oldFrom, oldTo] = trailingEdge(last.oldStart, last.oldCount, oldLineCount);
    const [newFrom, newTo] = trailingEdge(last.newStart, last.newCount, newLineCount);
    pushLineGap(gaps, oldFrom, oldTo, newFrom, newTo);
  }
  return gaps;
}

function trailingEdge(start: number, count: number, lineCount: number): [number, number] {
  const from = hunkEnd(start, count);
  if (lineCount === 0) return [from, from];
  return [from, lineCount + 1];
}

export function newGapExpandState(gap: DiffGap): GapExpandState {
  return { gap, fromStart: 0, fromEnd: 0 };
}

export function remainingDiffGap(state: GapExpandState): DiffGap {
  const consumed = state.fromStart + state.fromEnd;
  const oldCount = Math.max(0, state.gap.oldCount - Math.min(consumed, state.gap.oldCount));
  const newCount = Math.max(0, state.gap.newCount - Math.min(consumed, state.gap.newCount));
  return {
    oldStart: oldCount === 0 ? 0 : state.gap.oldStart + state.fromStart,
    oldCount,
    newStart: newCount === 0 ? 0 : state.gap.newStart + state.fromStart,
    newCount,
  };
}

export function gapExpandWindow(
  remaining: DiffGap,
  from: GapExpandFrom,
  take: number,
): { oldStart: number; newStart: number; count: number } | null {
  if (remaining.oldCount === 0 && remaining.newCount === 0) return null;
  const span = Math.max(remaining.newCount, remaining.oldCount);
  const count = Math.max(1, Math.min(Math.floor(take), span, FILE_RENDER_SOFT_CAP));
  if (from === 'start') {
    return { oldStart: remaining.oldStart, newStart: remaining.newStart, count };
  }
  return {
    oldStart: remaining.oldCount === 0 ? 0 : remaining.oldStart + remaining.oldCount - count,
    newStart: remaining.newCount === 0 ? 0 : remaining.newStart + remaining.newCount - count,
    count,
  };
}

export function recordGapExpand(state: GapExpandState, from: GapExpandFrom, count: number): GapExpandState {
  const remaining = remainingDiffGap(state);
  const span = Math.max(remaining.newCount, remaining.oldCount);
  const clamped = Math.max(0, Math.min(Math.floor(count), span));
  if (from === 'start') return { ...state, fromStart: state.fromStart + clamped };
  return { ...state, fromEnd: state.fromEnd + clamped };
}

export function mergeExpandedLines(
  existing: DiffLine[],
  incoming: DiffLine[],
  // Kept for call-site stability; ordering is by line number, not direction.
  _from: GapExpandFrom,
): DiffLine[] {
  const merged = incoming.concat(existing);
  // Source order, not arrival order: 'start' and 'end' windows interleave
  // once both directions have been consumed on one gap.
  return merged.sort(
    (a, b) =>
      (a.newNo ?? a.oldNo ?? 0) - (b.newNo ?? b.oldNo ?? 0) ||
      (a.oldNo ?? a.newNo ?? 0) - (b.oldNo ?? b.newNo ?? 0),
  );
}

export interface FileContextPage {
  snapshotId: string;
  path: string;
  oldLineCount: number;
  newLineCount: number;
  binary: boolean;
  lines: DiffLine[];
}

export function normalizeFileContextPage(raw: unknown): FileContextPage | null {
  const obj = asRecord(raw);
  if (!obj) return null;
  const path = stringField(obj.path);
  const snapshotId = stringField(obj.snapshotId);
  if (!path || !snapshotId) return null;
  const lines = Array.isArray(obj.lines)
    ? obj.lines.map(normalizeLine).filter((line): line is DiffLine => line !== null)
    : [];
  return {
    snapshotId,
    path,
    oldLineCount: uintField(obj.oldLineCount),
    newLineCount: uintField(obj.newLineCount),
    binary: boolField(obj.binary),
    lines,
  };
}

// ---------------------------------------------------------------------------
// Review model switch (composer row). One reducer owns the pick → command →
// ack/typed-reject state machine so the panel keeps the composer usable and
// the select reverts on any reject. Pure so the node lane exercises it.
// ---------------------------------------------------------------------------

/** Wire payload for `code_review_set_model`; session stamping is the caller's job.
 *  `modelId` is the qualified `provider/id` option value. */
export function buildCodeReviewSetModelPayload(modelId: string): Record<string, unknown> {
  return { type: 'code_review_set_model', modelId };
}

/** Display model name from a `{ ok: true, model: string }` ack; null otherwise. */
export function parseCodeReviewSetModelAck(data: unknown): string | null {
  const obj = asRecord(data);
  if (!obj || obj.ok !== true) return null;
  return optionalString(obj.model) ?? null;
}

/** Catalog rows from a `get_available_models` response; malformed rows drop. */
export function normalizeAvailableModels(
  data: unknown,
): Array<{ id: string; name: string; provider: string }> {
  const obj = asRecord(data);
  const rows = Array.isArray(obj?.models) ? obj.models : [];
  const out: Array<{ id: string; name: string; provider: string }> = [];
  for (const row of rows) {
    const rec = asRecord(row);
    if (!rec) continue;
    const id = stringField(rec.id);
    const provider = stringField(rec.provider);
    if (!id || !provider) continue;
    out.push({ id, name: stringField(rec.name), provider });
  }
  return out;
}

/** Grouped `provider/id` option value; empty unless both identity pieces exist. */
export function reviewModelOptionValue(
  provider: string | null | undefined,
  id: string | null | undefined,
): string {
  if (!provider || !id) return '';
  return `${provider}/${id}`;
}

/** Extract the catalog model id from a grouped `provider/id` option value.
 *  Splits at the FIRST slash only: ids may contain '/', providers never do
 *  (same rule as the main composer's onModelChange). */
export function modelIdFromOptionValue(value: string): string {
  const slash = value.indexOf('/');
  return slash >= 0 ? value.slice(slash + 1) : value;
}

/** Bounded inline status: rejects surface once, capped, never as toasts. */
export const REVIEW_STATUS_MAX_CHARS = 240;

export function boundedReviewStatus(message: string, max = REVIEW_STATUS_MAX_CHARS): string {
  const text = typeof message === 'string' ? message.trim() : '';
  if (!text) return '';
  return text.length <= max ? text : `${text.slice(0, max - 1)}…`;
}

export interface ReviewModelState {
  /** Applied model's grouped option value (`provider/id`); '' = unknown. */
  appliedValue: string;
  /** Applied model's display label from the set_model ack; '' = unknown. */
  appliedLabel: string;
  /** In-flight pick's value; null when no switch is in flight. */
  pendingValue: string | null;
  /** Bounded reject copy for the latest failed switch; null when clean. */
  status: string | null;
}

export type ReviewModelEvent =
  | { type: 'pick'; value: string }
  | { type: 'ack'; label: string | null }
  | { type: 'reject'; message: string }
  | { type: 'cancel' };

export function initialReviewModelState(): ReviewModelState {
  return { appliedValue: '', appliedLabel: '', pendingValue: null, status: null };
}

/** Apply one model-switch event. Rejects keep the previous applied value so
 *  the controlled select reverts; acks accept even without a usable label. */
export function applyReviewModelEvent(
  state: ReviewModelState,
  event: ReviewModelEvent,
): ReviewModelState {
  switch (event.type) {
    case 'pick':
      if (!event.value || event.value === state.appliedValue) return state;
      return { ...state, pendingValue: event.value, status: null };
    case 'ack':
      if (state.pendingValue == null) return state;
      return {
        appliedValue: state.pendingValue,
        appliedLabel: event.label ?? '',
        pendingValue: null,
        status: null,
      };
    case 'reject': {
      const status = boundedReviewStatus(event.message) || 'Model switch failed';
      return { ...state, pendingValue: null, status };
    }
    case 'cancel':
      return { ...state, pendingValue: null };
  }
}

/** True while a switch is in flight; the select stays enabled and shows the
 *  pending value (an ack confirms it, a reject reverts it). */
export function reviewModelPending(state: ReviewModelState): boolean {
  return state.pendingValue != null;
}

/** Select value while mounted: the pending pick (optimistic) or applied value. */
export function reviewModelSelectValue(state: ReviewModelState): string {
  return state.pendingValue ?? state.appliedValue;
}

/** Label under/next to the select: ack label, else the grouped value, else ''. */
export function reviewModelLabel(state: ReviewModelState): string {
  if (state.appliedLabel) return state.appliedLabel;
  return state.appliedValue;
}

// ---------------------------------------------------------------------------
// Auto Review request owner. The snapshot batch state is authoritative for
// progress; `pending` is the typed request owner that spans click → ACK → the
// FIRST snapshot carrying the accepted `autoReviewBatchId`. Stale idle
// snapshots (older batch id) can neither clear the owner nor unbusy the
// button, so the ACK→snapshot gap stays a live Stop without a close or a
// duplicate start. The spinner is delayed ≥151ms so fast paths never flash.
// ---------------------------------------------------------------------------

/**
 * Strict `code_review_auto_review` ACK parse: exactly
 * `{ ok: true, state: 'triaging', batchId: non-negative integer }`. Anything
 * else (including the retired `{ok,submitted,skipped}` shape) rejects so the
 * caller clears only this owner and toasts the malformed contract.
 */
export type CodeReviewAutoReviewAck =
  | { ok: true; state: 'triaging'; batchId: number }
  | { ok: false; error: string };

export function parseCodeReviewAutoReviewAck(data: unknown): CodeReviewAutoReviewAck {
  const obj = asRecord(data);
  if (!obj || obj.ok !== true || obj.state !== 'triaging') {
    return { ok: false, error: 'unexpected code_review_auto_review acknowledgement' };
  }
  const batchId = obj.batchId;
  if (typeof batchId !== 'number' || !Number.isInteger(batchId) || batchId < 0) {
    return { ok: false, error: 'code_review_auto_review acknowledgement carries no batchId' };
  }
  return { ok: true, state: 'triaging', batchId };
}

export const AUTO_REVIEW_SPINNER_GATE_MS = 151;

export interface AutoReviewButtonState {
  /** Typed request owner: spans click → ACK → the first snapshot carrying the
   *  accepted batch id. The snapshot's autoReviewState is authoritative once
   *  the owner is released. */
  pending: boolean;
  /** Batch id from the accepted ACK while it awaits a matching snapshot. */
  acceptedBatchId: number | null;
  /** Controller review identity captured at click; a different snapshot
   *  reviewId releases the owner instead of latching Stop. */
  reviewId: string | null;
  wireState: CodeReviewAutoState;
  counts: AutoReviewCounts;
}

export function initialAutoReviewButtonState(): AutoReviewButtonState {
  return {
    pending: false,
    acceptedBatchId: null,
    reviewId: null,
    wireState: 'idle',
    counts: { submitted: 0, completed: 0, failed: 0 },
  };
}

export type AutoReviewButtonEvent =
  | { type: 'click'; reviewId?: string | null }
  | { type: 'ack'; batchId: number }
  | { type: 'reject' }
  | { type: 'snapshot'; state: CodeReviewAutoState; counts: AutoReviewCounts; batchId: number; reviewId?: string };

export function applyAutoReviewButtonEvent(
  state: AutoReviewButtonState,
  event: AutoReviewButtonEvent,
): AutoReviewButtonState {
  switch (event.type) {
    case 'click':
      return state.pending
        ? state
        : {
            ...state,
            pending: true,
            acceptedBatchId: null,
            reviewId: event.reviewId ?? null,
            counts: { submitted: 0, completed: 0, failed: 0 },
          };
    case 'ack':
      // A snapshot already released the owner (a matching state landed before
      // the ACK): keep it released instead of re-busying a settled batch.
      return state.pending && state.acceptedBatchId == null
        ? { ...state, acceptedBatchId: event.batchId }
        : state;
    case 'reject':
      // A rejected request clears only its own owner.
      return state.pending
        ? { ...state, pending: false, acceptedBatchId: null, reviewId: null }
        : state;
    case 'snapshot': {
      if (state.pending) {
        if (
          state.reviewId !== null
          && event.reviewId !== undefined
          && event.reviewId !== state.reviewId
        ) {
          return {
            pending: false,
            acceptedBatchId: null,
            reviewId: null,
            wireState: event.state,
            counts: event.counts,
          };
        }
        const matching =
          state.acceptedBatchId == null
            ? event.state === 'triaging' || event.state === 'running'
            : event.batchId >= (state.acceptedBatchId ?? 0);
        if (!matching) return state;
        return {
          ...state,
          pending: false,
          acceptedBatchId: null,
          reviewId: null,
          wireState: event.state,
          counts: event.counts,
        };
      }
      if (
        state.wireState === event.state &&
        state.counts.submitted === event.counts.submitted &&
        state.counts.completed === event.counts.completed &&
        state.counts.failed === event.counts.failed
      ) {
        return state;
      }
      return { ...state, wireState: event.state, counts: event.counts };
    }
  }
}

/** True while the batch owns the button: pending click, triage, or deep run. */
export function autoReviewBusy(state: AutoReviewButtonState): boolean {
  return state.pending || state.wireState === 'running' || state.wireState === 'triaging';
}

/** Visible stage derived from the typed request owner and wire state.
 *  Counts never decide the phase: pending or `triaging` screens, `running`
 *  reviews, and a released terminal wire state is done. */
export type AutoReviewPhase = 'idle' | 'triaging' | 'running' | 'done';

export interface AutoReviewButtonView {
  busy: boolean;
  phase: AutoReviewPhase;
  /** Header status. Null before the 151ms gate and when idle. */
  progress: string | null;
}

export function autoReviewButtonView(
  state: AutoReviewButtonState,
  spinnerDue: boolean,
): AutoReviewButtonView {
  const busy = autoReviewBusy(state);
  const phase: AutoReviewPhase = busy
    ? (state.pending || state.wireState === 'triaging' ? 'triaging' : 'running')
    : (state.wireState === 'done' || state.wireState === 'failed' ? 'done' : 'idle');
  if (!spinnerDue && phase !== 'done') {
    return { busy, phase, progress: null };
  }
  if (phase === 'triaging') return { busy, phase, progress: 'Screening…' };
  if (phase === 'running' || phase === 'done') {
    const { completed, submitted } = state.counts;
    return {
      busy,
      phase,
      progress: completed === null || submitted === null
        ? 'Review --/--'
        : `Review ${completed}/${submitted}`,
    };
  }
  return { busy, phase, progress: null };
}

/** Highlight identifying status. Pending click and wire `running` share one
 *  label; the Stop verb stays on the button. Null before the 151ms gate. */
export function highlightButtonView(
  pending: boolean,
  wireState: CodeReviewHighlightState,
  spinnerDue: boolean,
): { busy: boolean; progress: string | null } {
  const busy = pending || wireState === 'running';
  if (!busy || !spinnerDue) return { busy, progress: null };
  return { busy, progress: 'Identifying…' };
}

/** Stamped `code_review_auto_review` payload. `language` rides the wire only
 *  for an explicit concrete-tag pick (untouched/no pick → omitted; the backend
 *  then uses the effective configured/default language). `modelId` is the
 *  qualified `provider/id` identity when the caller names a model. */
export function buildCodeReviewAutoReviewPayload(
  language?: string,
  modelId?: string,
): Record<string, unknown> {
  const payload: Record<string, unknown> = { type: 'code_review_auto_review' };
  if (language) payload.language = language;
  if (modelId) payload.modelId = modelId;
  return payload;
}

/** Common reply languages for the mini-select; tags are BCP 47. */
export interface ReviewLanguageOption {
  tag: string;
  label: string;
}

export const REVIEW_LANGUAGE_OPTIONS: ReviewLanguageOption[] = [
  { tag: 'zh', label: '中文' },
  { tag: 'en', label: 'English' },
  { tag: 'ja', label: '日本語' },
  { tag: 'ko', label: '한국어' },
  { tag: 'de', label: 'Deutsch' },
  { tag: 'fr', label: 'Français' },
  { tag: 'es', label: 'Español' },
];

const REVIEW_LANGUAGE_TAGS: Record<string, true> = {
  zh: true,
  en: true,
  ja: true,
  ko: true,
  de: true,
  fr: true,
  es: true,
};

/** Internal select sentinel for "no explicit pick": resolves to the effective
 *  configured/default language and sends no wire override; the select never
 *  renders a visible "Default" option — it shows the effective default tag. */
export const REVIEW_LANGUAGE_DEFAULT_VALUE = '';

/**
 * Selector display value: an explicit user pick (any value, including the
 * no-pick sentinel) wins; otherwise the snapshot's settings default when it
 * is a selectable tag; otherwise the sentinel.
 */
export function reviewLanguageSelection(
  defaultLanguage: string | null | undefined,
  picked: string | null | undefined,
): string {
  if (picked != null) return picked;
  if (defaultLanguage && defaultLanguage in REVIEW_LANGUAGE_TAGS) return defaultLanguage;
  return REVIEW_LANGUAGE_DEFAULT_VALUE;
}

/** Wire `language` value for a user pick; undefined (absent) unless the user
 *  explicitly chose a known concrete tag. Resolution (explicit > settings
 *  default > model default) happens on the backend. */
export function autoReviewLanguagePayload(picked: string | null | undefined): string | undefined {
  if (!picked || !(picked in REVIEW_LANGUAGE_TAGS)) return undefined;
  return picked;
}

// ---------------------------------------------------------------------------
// Highlight request owner + accepted-comment follow event + async error
// toasts. One typed owner per Web action; no second busy authority.
// ---------------------------------------------------------------------------

/**
 * Strict `code_review_tldr` ACK parse: exactly
 * `{ ok: true, state: 'running', tldrGeneration: non-negative integer }`.
 * The generation is the request-owner identity. Anything else rejects so
 * the caller clears only this owner.
 */
export type CodeReviewTldrAck =
  | { ok: true; state: 'running'; generation: number }
  | { ok: false; error: string };

export function parseCodeReviewTldrAck(data: unknown): CodeReviewTldrAck {
  const obj = asRecord(data);
  if (!obj || obj.ok !== true || obj.state !== 'running') {
    return { ok: false, error: 'unexpected code_review_tldr acknowledgement' };
  }
  const generation = obj.tldrGeneration;
  if (typeof generation !== 'number' || !Number.isInteger(generation) || generation < 0) {
    return { ok: false, error: 'code_review_tldr acknowledgement carries no generation' };
  }
  return { ok: true, state: 'running', generation };
}

/**
 * Strict `code_review_highlight` ACK parse: exactly
 * `{ ok: true, state: 'running', highlightGeneration: non-negative integer }`.
 * The generation is the request-owner identity. Anything else rejects so the
 * caller clears only this owner and toasts the malformed contract.
 */
export type CodeReviewHighlightAck =
  | { ok: true; state: 'running'; generation: number }
  | { ok: false; error: string };

export function parseCodeReviewHighlightAck(data: unknown): CodeReviewHighlightAck {
  const obj = asRecord(data);
  if (!obj || obj.ok !== true || obj.state !== 'running') {
    return { ok: false, error: 'unexpected code_review_highlight acknowledgement' };
  }
  const generation = obj.highlightGeneration;
  if (typeof generation !== 'number' || !Number.isInteger(generation) || generation < 0) {
    return { ok: false, error: 'code_review_highlight acknowledgement carries no generation' };
  }
  return { ok: true, state: 'running', generation };
}

/** Highlight click → ACK → matching-snapshot owner, mirroring the Auto Review
 *  button owner without the progress counters (a highlight has none). */
export interface HighlightOwnerState {
  /** Typed request owner spanning click → ACK → matching snapshot. */
  pending: boolean;
  /** Generation from the accepted ACK while it awaits a matching snapshot. */
  acceptedGeneration: number | null;
  /** Controller review identity captured at click; a different snapshot
   *  reviewId releases the owner instead of latching Stop. */
  reviewId: string | null;
}

export function initialHighlightOwnerState(): HighlightOwnerState {
  return { pending: false, acceptedGeneration: null, reviewId: null };
}

export type HighlightOwnerEvent =
  | { type: 'click'; reviewId?: string | null }
  | { type: 'ack'; generation: number }
  | { type: 'reject' }
  | { type: 'snapshot'; generation: number; state: CodeReviewHighlightState; reviewId?: string };

export function applyHighlightOwnerEvent(
  state: HighlightOwnerState,
  event: HighlightOwnerEvent,
): HighlightOwnerState {
  switch (event.type) {
    case 'click':
      return state.pending
        ? state
        : { pending: true, acceptedGeneration: null, reviewId: event.reviewId ?? null };
    case 'ack':
      // A snapshot already released the owner: keep it released.
      return state.pending && state.acceptedGeneration == null
        ? { ...state, acceptedGeneration: event.generation }
        : state;
    case 'reject':
      return state.pending
        ? { pending: false, acceptedGeneration: null, reviewId: null }
        : state;
    case 'snapshot': {
      if (!state.pending) return state;
      if (
        state.reviewId !== null
        && event.reviewId !== undefined
        && event.reviewId !== state.reviewId
      ) {
        return { pending: false, acceptedGeneration: null, reviewId: null };
      }
      const matching = state.acceptedGeneration != null
        && event.generation >= state.acceptedGeneration;
      return matching ? { pending: false, acceptedGeneration: null, reviewId: null } : state;
    }
  }
}

/** Idle snapshot interval. Live Auto Review / Highlight uses the faster cadence. */
export const CODE_REVIEW_SNAPSHOT_POLL_MS = 1500;
/** Snapshot cadence while an Auto Review or Highlight owner is live. */
export const CODE_REVIEW_BATCH_POLL_MS = 200;

export function codeReviewBatchOwnerLive(input: {
  autoReviewPending: boolean;
  highlightPending: boolean;
  tldrPending: boolean;
  autoReviewState: CodeReviewAutoState;
  highlightState: CodeReviewHighlightState;
  tldrState: CodeReviewTldrState;
}): boolean {
  return (
    input.autoReviewPending
    || input.highlightPending
    || input.tldrPending
    || input.autoReviewState === 'triaging'
    || input.autoReviewState === 'running'
    || input.highlightState === 'running'
    || input.tldrState === 'running'
  );
}

export function snapshotPollDelayMs(ownerLive: boolean): number {
  return ownerLive ? CODE_REVIEW_BATCH_POLL_MS : CODE_REVIEW_SNAPSHOT_POLL_MS;
}

/** True while the Highlight run owns the button: request + ACK gap. */
export function highlightOwnerBusy(state: HighlightOwnerState): boolean {
  return state.pending;
}

/** Full-hunk-keyed accepted-comment follow event. `key` is the exact
 *  hunkKeyFor(path, ranges, contentHash) of the submitted hunk; `seq`
 *  monotonically distinguishes repeated accepts on the same hunk. */
export interface CodeReviewFollowEvent {
  key: string;
  seq: number;
}

/** One asynchronous operation failure reaching the App's single toast. */
/** One asynchronous operation failure reaching the App's single toast. */
export interface ReviewErrorNotice {
  kind: 'snapshot' | 'autoReview' | 'highlight' | 'tldr';
  message: string;
  /** Exact typed user-Stop terminal category: quiet, never toasted. */
  cancelled: boolean;
}

/** Last-reported error identity per field; passing it back dedupes repeated
 *  polls of the same failing identity (exactly one toast per identity). */
export interface ReviewErrorReport {
  snapshot: string | null;
  autoReview: string | null;
  highlight: string | null;
  tldr: string | null;
}

export function initialReviewErrorReport(): ReviewErrorReport {
  return { snapshot: null, autoReview: null, highlight: null, tldr: null };
}

/**
 * Diff the snapshot's async error fields against the last-reported
 * identities. snapshot.error is keyed by review+generation+revision+text;
 * autoReviewError by reviewId+batch id+text; highlightError by
 * reviewId+generation+text. `cancelled` marks the typed user-Stop category
 * (code 'cancelled') — the ONLY quiet terminal category.
 */
export function reviewErrorNotices(
  snapshot: CodeReviewSnapshot,
  reported: ReviewErrorReport,
): { notices: ReviewErrorNotice[]; reported: ReviewErrorReport } {
  const notices: ReviewErrorNotice[] = [];
  const next: ReviewErrorReport = { ...reported };
  if (snapshot.error) {
    const key = `${snapshot.reviewId}|${snapshot.generation}|${snapshot.revision}|${snapshot.error}`;
    if (key !== reported.snapshot) {
      next.snapshot = key;
      notices.push({
        kind: 'snapshot',
        message: `Code review: ${snapshot.error}`,
        cancelled: false,
      });
    }
  }
  if (snapshot.autoReviewError) {
    const cause = launchRejectedCause(snapshot);
    const detail = cause ?? snapshot.autoReviewError.message;
    const key = `${snapshot.reviewId}|${snapshot.autoReviewBatchId}|${snapshot.autoReviewError.code}|${detail}`;
    if (key !== reported.autoReview) {
      next.autoReview = key;
      notices.push({
        kind: 'autoReview',
        message: `Auto review: ${detail}`,
        cancelled: snapshot.autoReviewError.code === 'cancelled',
      });
    }
  }
  if (snapshot.highlightError) {
    const key = `${snapshot.reviewId}|${snapshot.highlightGeneration}|${snapshot.highlightError}`;
    if (key !== reported.highlight) {
      next.highlight = key;
      notices.push({
        kind: 'highlight',
        message: `Highlight: ${snapshot.highlightError}`,
        cancelled: false,
      });
    }
  }
  if (snapshot.tldrError) {
    const key = `${snapshot.reviewId}|${snapshot.tldrGeneration}|${snapshot.tldrSnapshotId ?? ''}|${snapshot.tldrError}`;
    if (key !== reported.tldr) {
      next.tldr = key;
      notices.push({
        kind: 'tldr',
        message: `TLDR: ${snapshot.tldrError}`,
        cancelled: false,
      });
    }
  }
  return notices.length > 0 ? { notices, reported: next } : { notices, reported };
}

/** The one documented non-fatal snapshot/close category: the controller was
 *  closed by a competing mount or a backend restart. Match host-stamped
 *  `rpcCode` only — never message prose. */
export function isNoOpenCodeReviewError(err: unknown): boolean {
  if (err == null || typeof err !== 'object' || Array.isArray(err)) return false;
  if (!('rpcCode' in err)) return false;
  return err.rpcCode === CODE_REVIEW_NOT_OPEN_CODE;
}

/** Markdown shown for the pinned snapshot only. A different snapshot id
 *  hides the previous body instead of painting it against the new diff. */
export function tldrMarkdownForSnapshot(
  snapshot: Pick<CodeReviewSnapshot, 'snapshotId' | 'tldrSnapshotId' | 'tldrMarkdown'>,
): string {
  if (!snapshot.tldrSnapshotId || snapshot.tldrSnapshotId !== snapshot.snapshotId) return '';
  return snapshot.tldrMarkdown;
}

/** TLDR button. Pending click and wire `running` share one busy state.
 *  The label stays quiet until the shared 151ms spinner gate. */
export function tldrButtonView(
  pending: boolean,
  wireState: CodeReviewTldrState,
  spinnerDue: boolean,
): { busy: boolean; progress: string | null } {
  const busy = pending || wireState === 'running';
  if (!busy || !spinnerDue) return { busy, progress: null };
  return { busy, progress: 'Summarizing\u2026' };
}
