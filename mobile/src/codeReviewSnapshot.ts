export type DiffLineKind = 'context' | 'addition' | 'deletion' | 'meta';

export interface DiffLine {
  kind: DiffLineKind;
  text: string;
  oldNo?: number;
  newNo?: number;
}

export interface DiffHunk {
  header: string;
  contentHash: string;
  lines: DiffLine[];
}

export interface DiffFile {
  path: string;
  status: string;
  insertions: number;
  deletions: number;
  truncated: boolean;
  hunks: DiffHunk[];
}

export interface ReviewComment {
  role: string;
  text: string;
}

export interface ReviewThread {
  path: string;
  comments: ReviewComment[];
}

/** One typed display category. `clean` is derived from an empty finding
 *  list; it is never a finding category on the wire. */
export type ReviewCategory = 'critical' | 'overdesign' | 'naming' | 'clean';
/** A finding category: `clean` is never one. */
export type ReviewFindingCategory = Exclude<ReviewCategory, 'clean'>;
/** Which side of the diff a finding's line range indexes. */
export type ReviewFindingSide = 'old' | 'new';

/** One structured finding, mirroring mobile/src/protocol.ts: a single side
 *  and one inclusive, one-based file line range, exactly as the backend
 *  validated it against the diff rows. */
export interface ReviewFinding {
  category: ReviewFindingCategory;
  side: ReviewFindingSide;
  startLine: number;
  endLine: number;
  summary: string;
  evidence: string;
}

/** One hunk's validated findings, bound to the exact hunk identity. */
export interface HunkReviewResult {
  identity: { snapshotId: string; path: string; oldStart: number; oldCount: number; newStart: number; newCount: number; contentHash: string };
  findings: ReviewFinding[];
}

/** Findings kept from one reviewed hunk (the backend response budget); there
 *  is no snapshot-wide cap that would drop valid findings from other hunks. */
export const REVIEW_MAX_FINDINGS_PER_HUNK = 20;
/** Strict finding summary UTF-8 byte cap. */
export const REVIEW_MAX_SUMMARY_BYTES = 512;
/** Strict finding evidence UTF-8 byte cap. */
export const REVIEW_MAX_EVIDENCE_BYTES = 8192;
/** Inclusive line coordinates cap (`u32::MAX` on the backend). */
export const REVIEW_MAX_LINE = 4_294_967_295;

export interface CodeReviewSnapshot {
  comparisonLabel: string;
  error: string | null;
  totalInsertions: number;
  totalDeletions: number;
  files: DiffFile[];
  threads: ReviewThread[];
  /** Typed auto-review stage. Unknown values stay idle. */
  autoReviewState: 'idle' | 'triaging' | 'running' | 'done' | 'failed';
  /** Typed highlight stage. Unknown values stay idle. */
  highlightState: 'idle' | 'running' | 'done' | 'failed';
  /** Authoritative per-hunk `reviewFindings`. Malformed, duplicate-identity,
   *  or over-budget rows reject the whole snapshot. */
  reviewFindings: HunkReviewResult[];
  /** TLDR lifecycle of the controller's single summary operation; mirrors
   *  mobile/src/protocol.ts ReviewSnapshot. Unknown values stay idle. */
  tldrState: 'idle' | 'running' | 'done' | 'failed';
  /** Monotonic TLDR operation counter. */
  tldrGeneration: number;
  /** Snapshot the current/last TLDR ran against; null before any run. */
  tldrSnapshotId: string | null;
  /** Safe failure text of the last TLDR run. */
  tldrError: string | null;
  /** Last approved TLDR document; '' when none. */
  tldrMarkdown: string;
}

const LINE_KINDS: Record<string, DiffLineKind> = {
  context: 'context',
  addition: 'addition',
  deletion: 'deletion',
  meta: 'meta',
};

function asRecord(value: unknown): Record<string, unknown> | null {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return null;
  return value as Record<string, unknown>;
}

function stringField(value: unknown): string {
  return typeof value === 'string' ? value : '';
}

function uintField(value: unknown): number {
  if (typeof value !== 'number' || !Number.isFinite(value) || value < 0) return 0;
  return Math.floor(value);
}

function boolField(value: unknown): boolean {
  return value === true;
}

function normalizeLine(raw: unknown): DiffLine | null {
  const rec = asRecord(raw);
  if (!rec) return null;
  const kind = LINE_KINDS[stringField(rec.kind)] ?? 'meta';
  const line: DiffLine = { kind, text: stringField(rec.text) };
  if (typeof rec.oldNo === 'number' && Number.isFinite(rec.oldNo)) line.oldNo = Math.floor(rec.oldNo);
  if (typeof rec.newNo === 'number' && Number.isFinite(rec.newNo)) line.newNo = Math.floor(rec.newNo);
  return line;
}

function normalizeHunk(raw: unknown): DiffHunk | null {
  const rec = asRecord(raw);
  if (!rec) return null;
  const contentHash = stringField(rec.contentHash);
  if (!contentHash) return null;
  const lines: DiffLine[] = [];
  if (Array.isArray(rec.lines)) {
    for (const entry of rec.lines) {
      const line = normalizeLine(entry);
      if (line) lines.push(line);
    }
  }
  return { header: stringField(rec.header), contentHash, lines };
}

function normalizeFile(raw: unknown): DiffFile | null {
  const rec = asRecord(raw);
  if (!rec) return null;
  const path = stringField(rec.path);
  if (!path) return null;
  const hunks: DiffHunk[] = [];
  if (Array.isArray(rec.hunks)) {
    for (const entry of rec.hunks) {
      const hunk = normalizeHunk(entry);
      if (hunk) hunks.push(hunk);
    }
  }
  return {
    path,
    status: stringField(rec.status) || 'changed',
    insertions: uintField(rec.insertions),
    deletions: uintField(rec.deletions),
    truncated: boolField(rec.truncated),
    hunks,
  };
}

function normalizeComment(raw: unknown): ReviewComment | null {
  const rec = asRecord(raw);
  if (!rec) return null;
  return { role: stringField(rec.role) || 'system', text: stringField(rec.text) };
}

function normalizeThread(raw: unknown): ReviewThread | null {
  const rec = asRecord(raw);
  if (!rec) return null;
  const identity = asRecord(rec.identity);
  const path = stringField(identity?.path ?? rec.path);
  if (!path) return null;
  const comments: ReviewComment[] = [];
  if (Array.isArray(rec.comments)) {
    for (const entry of rec.comments) {
      const comment = normalizeComment(entry);
      if (comment) comments.push(comment);
    }
  }
  return { path, comments };
}

export function emptyCodeReviewSnapshot(error: string | null = null): CodeReviewSnapshot {
  return {
    comparisonLabel: '',
    error,
    totalInsertions: 0,
    totalDeletions: 0,
    files: [],
    threads: [],
    autoReviewState: 'idle',
    highlightState: 'idle',
    reviewFindings: [],
    tldrState: 'idle',
    tldrGeneration: 0,
    tldrSnapshotId: null,
    tldrError: null,
    tldrMarkdown: '',
  };
}

/** Exact-key check: a finding payload rejects unknown fields fail-closed. */
function wireExactKeys(value: Record<string, unknown>, keys: readonly string[]): boolean {
  const wanted: Record<string, true> = {};
  for (const key of keys) wanted[key] = true;
  for (const key of Object.keys(value)) {
    if (!wanted[key]) return false;
  }
  return Object.keys(value).length === keys.length;
}

/** UTF-8 byte length of a finding string, for the exact wire bounds. */
function reviewUtf8ByteLength(text: string): number {
  let bytes = 0;
  for (let i = 0; i < text.length; i += 1) {
    const code = text.charCodeAt(i);
    if (code < 0x80) bytes += 1;
    else if (code < 0x800) bytes += 2;
    else if (code >= 0xD800 && code <= 0xDBFF && i + 1 < text.length) {
      const next = text.charCodeAt(i + 1);
      if (next >= 0xDC00 && next <= 0xDFFF) {
        bytes += 4;
        i += 1;
        continue;
      }
      bytes += 3;
    } else bytes += 3;
  }
  return bytes;
}

/** Literal membership only: a Record lookup would accept inherited keys
 *  like `constructor` or `toString` as a category or side. */
function isReviewFindingCategory(value: unknown): value is ReviewFindingCategory {
  return value === 'critical' || value === 'overdesign' || value === 'naming';
}

function isReviewFindingSide(value: unknown): value is ReviewFindingSide {
  return value === 'old' || value === 'new';
}

function parseReviewFinding(data: unknown): ReviewFinding | null {
  const rec = asRecord(data);
  if (!rec || !wireExactKeys(rec, ['category', 'side', 'startLine', 'endLine', 'summary', 'evidence'])) return null;
  const category = rec.category;
  if (!isReviewFindingCategory(category)) return null;
  const side = rec.side;
  if (!isReviewFindingSide(side)) return null;
  if (typeof rec.startLine !== 'number' || !Number.isSafeInteger(rec.startLine)) return null;
  if (typeof rec.endLine !== 'number' || !Number.isSafeInteger(rec.endLine)) return null;
  // One-based inclusive file coordinates: zero, reversed, and > u32::MAX fail closed.
  if (rec.startLine < 1 || rec.startLine > rec.endLine || rec.endLine > REVIEW_MAX_LINE) return null;
  const summary = rec.summary;
  const evidence = rec.evidence;
  if (typeof summary !== 'string' || summary.trim() === '' || reviewUtf8ByteLength(summary) > REVIEW_MAX_SUMMARY_BYTES) return null;
  if (typeof evidence !== 'string' || evidence.trim() === '' || reviewUtf8ByteLength(evidence) > REVIEW_MAX_EVIDENCE_BYTES) return null;
  return {
    category,
    side,
    startLine: rec.startLine,
    endLine: rec.endLine,
    summary,
    evidence,
  };
}

/** Findings identity: all seven fields present and valid. */
function parseReviewResultIdentity(data: unknown): HunkReviewResult['identity'] | null {
  const rec = asRecord(data);
  if (!rec || !wireExactKeys(rec, ['snapshotId', 'path', 'oldStart', 'oldCount', 'newStart', 'newCount', 'contentHash'])) return null;
  const snapshotId = stringField(rec.snapshotId).trim();
  const path = stringField(rec.path).trim();
  const contentHash = stringField(rec.contentHash).trim();
  if (snapshotId === '' || path === '' || contentHash === '') return null;
  const numbers: number[] = [];
  for (const field of ['oldStart', 'oldCount', 'newStart', 'newCount'] as const) {
    const value = rec[field];
    if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 0) return null;
    numbers.push(value);
  }
  return {
    snapshotId,
    path,
    oldStart: numbers[0],
    oldCount: numbers[1],
    newStart: numbers[2],
    newCount: numbers[3],
    contentHash,
  };
}

/** The authoritative per-hunk `reviewFindings` collection. Absent reads as
 *  unreviewed; any malformed row rejects the whole snapshot. Each hunk
 *  carries at most one model response's findings; there is no snapshot-wide
 *  cap that would drop valid findings from other hunks. */
function parseReviewFindings(data: unknown): HunkReviewResult[] | null {
  if (data == null) return [];
  if (!Array.isArray(data)) return null;
  const seenIdentities = new Set<string>();
  const results: HunkReviewResult[] = [];
  for (const entry of data) {
    const rec = asRecord(entry);
    if (!rec || !wireExactKeys(rec, ['identity', 'findings'])) return null;
    const identity = parseReviewResultIdentity(rec.identity);
    if (!identity) return null;
    const identityKey = `${identity.snapshotId}\0${identity.path}\0${identity.oldStart},${identity.oldCount},${identity.newStart},${identity.newCount}\0${identity.contentHash}`;
    if (seenIdentities.has(identityKey)) return null;
    if (!Array.isArray(rec.findings) || rec.findings.length > REVIEW_MAX_FINDINGS_PER_HUNK) return null;
    const seenFindings = new Set<string>();
    const findings: ReviewFinding[] = [];
    for (const raw of rec.findings) {
      const finding = parseReviewFinding(raw);
      if (!finding) return null;
      const findingKey = JSON.stringify(finding);
      if (seenFindings.has(findingKey)) return null;
      seenFindings.add(findingKey);
      findings.push(finding);
    }
    seenIdentities.add(identityKey);
    results.push({ identity, findings });
  }
  return results;
}

export function normalizeCodeReviewSnapshot(data: unknown): CodeReviewSnapshot {
  const rec = asRecord(data);
  if (!rec) return emptyCodeReviewSnapshot('Invalid code-review snapshot response');
  const files: DiffFile[] = [];
  if (Array.isArray(rec.files)) {
    for (const entry of rec.files) {
      const file = normalizeFile(entry);
      if (file) files.push(file);
    }
  }
  const threads: ReviewThread[] = [];
  const rawThreads = rec.threads;
  const threadMap = asRecord(rawThreads);
  const threadRows = Array.isArray(rawThreads)
    ? rawThreads
    : threadMap
      ? Object.keys(threadMap).map((key) => threadMap[key])
      : rawThreads == null ? [] : null;
  if (threadRows === null) return emptyCodeReviewSnapshot('Invalid code-review snapshot response');
  for (const entry of threadRows) {
    const thread = normalizeThread(entry);
    if (thread) threads.push(thread);
  }
  const reviewFindings = parseReviewFindings(rec.reviewFindings);
  if (reviewFindings === null) {
    return emptyCodeReviewSnapshot('Invalid code-review snapshot response');
  }
  const autoReviewState = rec.autoReviewState;
  const highlightState = rec.highlightState;
  // Supplied TLDR fields are validated exactly; a malformed one rejects the
  // whole snapshot instead of coercing into a plausible idle one. Absent
  // fields keep their neutral defaults (older projections omit them).
  if (rec.tldrState !== undefined && rec.tldrState !== 'idle' && rec.tldrState !== 'running' && rec.tldrState !== 'done' && rec.tldrState !== 'failed') {
    return emptyCodeReviewSnapshot('Invalid code-review snapshot response');
  }
  if (rec.tldrGeneration !== undefined && (typeof rec.tldrGeneration !== 'number' || !Number.isSafeInteger(rec.tldrGeneration) || rec.tldrGeneration < 0)) {
    return emptyCodeReviewSnapshot('Invalid code-review snapshot response');
  }
  if (rec.tldrSnapshotId !== undefined && rec.tldrSnapshotId !== null && typeof rec.tldrSnapshotId !== 'string') {
    return emptyCodeReviewSnapshot('Invalid code-review snapshot response');
  }
  if (rec.tldrError !== undefined && rec.tldrError !== null && typeof rec.tldrError !== 'string') {
    return emptyCodeReviewSnapshot('Invalid code-review snapshot response');
  }
  if (rec.tldrMarkdown !== undefined && typeof rec.tldrMarkdown !== 'string') {
    return emptyCodeReviewSnapshot('Invalid code-review snapshot response');
  }
  return {
    comparisonLabel: stringField(rec.comparisonLabel),
    error: typeof rec.error === 'string' ? rec.error : null,
    totalInsertions: uintField(rec.totalInsertions),
    totalDeletions: uintField(rec.totalDeletions),
    files,
    threads,
    autoReviewState: autoReviewState === 'triaging' || autoReviewState === 'running' || autoReviewState === 'done' || autoReviewState === 'failed'
      ? autoReviewState
      : 'idle',
    highlightState: highlightState === 'running' || highlightState === 'done' || highlightState === 'failed'
      ? highlightState
      : 'idle',
    reviewFindings,
    tldrState: rec.tldrState ?? 'idle',
    tldrGeneration: typeof rec.tldrGeneration === 'number' ? rec.tldrGeneration : 0,
    tldrSnapshotId: typeof rec.tldrSnapshotId === 'string' ? rec.tldrSnapshotId : null,
    tldrError: typeof rec.tldrError === 'string' ? rec.tldrError : null,
    tldrMarkdown: typeof rec.tldrMarkdown === 'string' ? rec.tldrMarkdown : '',
  };
}

export function parseAvailableModels(data: unknown): Array<{ id: string; name: string; provider: string }> {
  const rec = asRecord(data);
  const rows = rec && Array.isArray(rec.models) ? rec.models : [];
  const out: Array<{ id: string; name: string; provider: string }> = [];
  for (const row of rows) {
    const item = asRecord(row);
    if (!item) continue;
    const id = stringField(item.id);
    const provider = stringField(item.provider);
    if (!id || !provider) continue;
    out.push({ id, name: stringField(item.name), provider });
  }
  return out;
}

export function parseThinkingLevels(data: unknown): string[] | null {
  const rec = asRecord(data);
  if (!rec || !Array.isArray(rec.levels)) return null;
  return rec.levels.filter((level): level is string => typeof level === 'string' && level.length > 0);
}
