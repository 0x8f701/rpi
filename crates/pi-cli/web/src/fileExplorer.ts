/**
 * File-explorer RPC wire shapes + pure helpers.
 *
 * Frozen contract: local://file-explorer-contract.md
 * Commands (snake_case type, camelCase fields; sessionId at transport):
 *   file_explorer_list | file_explorer_search | read | changes | diff | blame | stage_download | map
 *
 * App stamps sessionId via sendCommand. This module never HTML-escapes —
 * the panel renders through React text + safeText().
 */

import { sessionExportUrl, type SessionExportDocument } from './sessionExport';
import { isTransportError } from './transportError';
import { fileStatusLetter, type FileStatus } from './codeReview';
import { rpcErrorCode, type RpcErrorCode } from './types';
import { utf8ByteLength } from './attachments';
import type {
  FileConversationContextWire,
  FileConversationSnapshot,
} from './panels/FileConversationDock';
import type { SideChatEntryKind, SideChatEntryWire } from './panels/SideChatPanel';

export const FILE_EXPLORER_DEFAULT_MAX_LINES = 400;
export const FILE_EXPLORER_HARD_MAX_LINES = 2000;
export const FILE_EXPLORER_MAX_PREVIEW_BYTES = 256 * 1024;
export const FILE_EXPLORER_TOO_LARGE_MESSAGE = 'File is too large to display.';
export const FILE_EXPLORER_SIZE_UNAVAILABLE_MESSAGE = 'File size is unavailable.';

export function fileExplorerIsOversized(sizeBytes: number | undefined): boolean {
  return sizeBytes != null && sizeBytes > FILE_EXPLORER_MAX_PREVIEW_BYTES;
}

const MAX_DOWNLOAD_FILENAME_LENGTH = 128;

/** Caps mirrored from the TUI file viewer (crates/pi-cli/src/file_viewer.rs
 *  MAX_OPEN_FILES = 8: "opening a ninth file evicts the oldest tab"). */
export const FILE_EXPLORER_MAX_OPEN_TABS = 8;
/** Sessions the drawer tracks and the module page cache retains. The fifth
 *  session slot evicts the least-recently-updated one (and its cache keys),
 *  so A→B→A within cap keeps tabs and settled pages. */
export const FILE_EXPLORER_MAX_SESSIONS = 4;
/** Page-cache caps: 8 tabs × 2 views × 4 sessions, plus aggregate retained
 *  line/byte estimates so load-more accumulation stays bounded even under
 *  the entry cap. */
export const FILE_EXPLORER_MAX_CACHE_ENTRIES = FILE_EXPLORER_MAX_OPEN_TABS * 2 * FILE_EXPLORER_MAX_SESSIONS;
export const FILE_EXPLORER_MAX_CACHE_LINES = 40000;
export const FILE_EXPLORER_MAX_CACHE_BYTES = 8 * 1024 * 1024;
/** Per-entry retained-line budget for the cache bound: an entry that grows
 *  past this (a single huge file accumulated via load-more) is compacted to
 *  its LAST FILE_EXPLORER_DEFAULT_MAX_LINES lines — shifting startCursor so
 *  the loaded end stays coherent — instead of being evicted. Without this,
 *  one oversized entry can never fit the aggregate caps and the bound would
 *  evict the just-written key itself. */
export const FILE_EXPLORER_MAX_LINES_PER_ENTRY = FILE_EXPLORER_DEFAULT_MAX_LINES;

/** Desktop tree/content split bounds (percent). Owned here so the session
 *  slot's persisted inner tree width and the panel share ONE clamp
 *  authority; FileExplorerPanel re-exports them for its render surface. */
export const FILE_EXPLORER_TREE_PCT_MIN = 22;
export const FILE_EXPLORER_TREE_PCT_MAX = 55;
export const FILE_EXPLORER_TREE_PCT_DEFAULT = 34;
export const FILE_EXPLORER_TREE_PCT_STEP = 2;

/** Clamp a candidate tree percent into the supported desktop range. */
export function clampFileExplorerTreePct(value: number): number {
  if (!Number.isFinite(value)) return FILE_EXPLORER_TREE_PCT_DEFAULT;
  return Math.min(
    FILE_EXPLORER_TREE_PCT_MAX,
    Math.max(FILE_EXPLORER_TREE_PCT_MIN, Math.round(value)),
  );
}

/** Keyboard step: +1 grows the tree (ArrowRight), -1 shrinks (ArrowLeft). */
export function stepFileExplorerTreePct(current: number, direction: -1 | 1): number {
  return clampFileExplorerTreePct(current + direction * FILE_EXPLORER_TREE_PCT_STEP);
}

export type FileExplorerEntryKind = 'file' | 'dir';

export type FileExplorerChangeStatus = Extract<FileStatus, 'modified' | 'added' | 'deleted' | 'renamed'>;
export type FileExplorerTabView = 'file' | 'diff';
export type FileExplorerDrawerMode = 'closed' | 'collapsed' | 'open';

export interface FileExplorerEntry {
  name: string;
  path: string;
  kind: FileExplorerEntryKind;
  sizeBytes?: number;
}

export interface FileExplorerListResult {
  entries: FileExplorerEntry[];
  truncated: boolean;
}

export interface FileExplorerReadResult {
  path: string;
  lines: string[];
  nextCursor?: number;
  /** Backend logical-line count for the whole file. Always present. */
  totalLines: number;
  sizeBytes: number;
  truncated: boolean;
}

export interface FileExplorerChange {
  path: string;
  status: FileExplorerChangeStatus;
  oldPath?: string;
}

export interface FileExplorerChangesResult {
  files: FileExplorerChange[];
  isRepo: boolean;
  /** Backend caps the counted file list and reports truncation. */
  truncated: boolean;
}

export interface FileExplorerDiffResult {
  path: string;
  lines: string[];
  nextCursor?: number;
  /** Backend logical-line count for the whole diff. Always present. */
  totalLines: number;
  truncated: boolean;
}

/** Git line attribution for one inclusive source range. `commit` is the full
 *  oid, or null only for the zero oid (uncommitted). `time` is author-time
 *  Unix seconds. No email. */
export interface FileExplorerBlameLine {
  commit: string | null;
  author: string;
  time: number;
  originalLine: number;
  currentLine: number;
  text: string;
}

export interface FileExplorerBlameResult {
  path: string;
  lines: FileExplorerBlameLine[];
}

export const FILE_EXPLORER_BLAME_MAX_LINES = 2000;

export interface FileExplorerStageDownloadResult {
  downloadId: string;
  fileName: string;
}

export interface FileExplorerTab {
  path: string;
  line?: number;
  /** Set only by an open call that explicitly requested this line.
   *  Reopening without a line clears it so a retained line cannot keep
   *  forcing Source. */
  lineRequest?: number;
  view: FileExplorerTabView;
  /** File-conversation id bound to THIS tab's lifetime (allocated lazily
   *  on the first Ask-send). One id per open tab: re-activating the tab
   *  keeps it, closing the tab retires it via the slot-patch cleanup, and
   *  a later reopened tab is a NEW conversation. The slot map is the only
   *  registry — no shadow active-conversation state. */
  conversationId?: string;
}

export interface FileExplorerSessionSlot {
  tabs: FileExplorerTab[];
  activePath: string | null;
  mode: FileExplorerDrawerMode;
  /** Drawer view geometry is NOT stored here: it lives in the module
   *  view-geometry external store (fileExplorerViewCache below), keyed by
   *  the same owner key as this slot map, so directory toggles and tree
   *  scroll never rerender the App/Chat slot holders. */
}

export function emptyFileExplorerSlot(): FileExplorerSessionSlot {
  return { tabs: [], activePath: null, mode: 'closed' };
}

export function readFileExplorerSlot(
  bySession: Record<string, FileExplorerSessionSlot>,
  sessionId: string | null,
): FileExplorerSessionSlot {
  if (!sessionId) return emptyFileExplorerSlot();
  return bySession[sessionId] ?? emptyFileExplorerSlot();
}

/* ------------------------------------------------------------------ *
 * Command builders (wire: snake_case type, camelCase fields)
 * ------------------------------------------------------------------ */

export function fileExplorerListCommand(path?: string): Record<string, unknown> {
  const command: Record<string, unknown> = { type: 'file_explorer_list' };
  if (path != null && path !== '') command.path = path;
  return command;
}

/** Workspace search. Matches file and directory names and relative paths. */
export function fileExplorerSearchCommand(query: string): Record<string, unknown> {
  return { type: 'file_explorer_search', query };
}

/** `''` plus every ancestor through `path`, so a search hit can be expanded. */
export function fileExplorerAncestorPaths(path: string): string[] {
  const parts = path.split('/').filter((part) => part.length > 0 && part !== '.' && part !== '..');
  const chain = [''];
  let acc = '';
  for (const part of parts) {
    acc = acc ? `${acc}/${part}` : part;
    chain.push(acc);
  }
  return chain;
}

export function fileExplorerExpandDirectoryChain(
  expandedDirs: Record<string, true>,
  path: string,
): Record<string, true> {
  const next = { ...expandedDirs };
  for (const item of fileExplorerAncestorPaths(path)) next[item] = true;
  return next;
}

export function fileExplorerReadCommand(
  path: string,
  cursor?: number,
  maxLines?: number,
): Record<string, unknown> {
  const command: Record<string, unknown> = { type: 'file_explorer_read', path };
  if (cursor != null) command.cursor = cursor;
  if (maxLines != null) command.maxLines = clampMaxLines(maxLines);
  return command;
}

export function fileExplorerChangesCommand(): Record<string, unknown> {
  return { type: 'file_explorer_changes' };
}

export function fileExplorerDiffCommand(
  path: string,
  cursor?: number,
  maxLines?: number,
): Record<string, unknown> {
  const command: Record<string, unknown> = { type: 'file_explorer_diff', path };
  if (cursor != null) command.cursor = cursor;
  if (maxLines != null) command.maxLines = clampMaxLines(maxLines);
  return command;
}

export function fileExplorerBlameCommand(
  path: string,
  startLine: number,
  endLine: number,
): Record<string, unknown> {
  return { type: 'file_explorer_blame', path, startLine, endLine };
}

export function fileExplorerStageDownloadCommand(path: string): Record<string, unknown> {
  return { type: 'file_explorer_stage_download', path };
}

export function fileExplorerMapCommand(path: string): Record<string, unknown> {
  return { type: 'file_explorer_map', path };
}

export function clampMaxLines(maxLines: number): number {
  if (!Number.isFinite(maxLines)) return FILE_EXPLORER_DEFAULT_MAX_LINES;
  const n = Math.floor(maxLines);
  if (n < 1) return FILE_EXPLORER_DEFAULT_MAX_LINES;
  return Math.min(n, FILE_EXPLORER_HARD_MAX_LINES);
}

export function fileExplorerCommandForSession(
  sessionId: string,
  command: Record<string, unknown>,
): Record<string, unknown> {
  return { ...command, sessionId };
}

export function fileExplorerCursorForLine(
  line: number | undefined,
  pageLines = FILE_EXPLORER_DEFAULT_MAX_LINES,
): number | undefined {
  if (line == null || !Number.isFinite(line) || line <= pageLines) return undefined;
  const boundedPageLines = clampMaxLines(pageLines);
  return Math.floor((Math.floor(line) - 1) / boundedPageLines) * boundedPageLines;
}

export function fileExplorerShouldRestoreFileScroll(line: number | undefined): boolean {
  return line == null;
}

/* ------------------------------------------------------------------ *
 * Defensive wire normalization (never throws)
 * ------------------------------------------------------------------ */

const ENTRY_KINDS: Record<string, true> = { file: true, dir: true };
const CHANGE_STATUSES: Record<string, true> = {
  modified: true,
  added: true,
  deleted: true,
  renamed: true,
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

function uintField(value: unknown): number {
  if (typeof value !== 'number' || !Number.isFinite(value) || value < 0) return 0;
  return Math.floor(value);
}

function optionalUint(value: unknown): number | undefined {
  if (typeof value !== 'number' || !Number.isFinite(value) || value < 0) return undefined;
  return Math.floor(value);
}

function stringLines(value: unknown): string[] {
  if (!Array.isArray(value)) return [];
  const lines: string[] = [];
  for (const item of value) {
    if (typeof item === 'string') lines.push(item);
  }
  return lines;
}

function normalizeEntry(raw: unknown): FileExplorerEntry | null {
  const rec = asRecord(raw);
  if (!rec) return null;
  const name = stringField(rec.name);
  const path = stringField(rec.path);
  const kind = stringField(rec.kind);
  if (!name || !path || !ENTRY_KINDS[kind]) return null;
  const entry: FileExplorerEntry = { name, path, kind: kind as FileExplorerEntryKind };
  if (rec.sizeBytes != null) entry.sizeBytes = uintField(rec.sizeBytes);
  return entry;
}

export function normalizeList(raw: unknown): FileExplorerListResult | null {
  const rec = asRecord(raw);
  if (!rec || !Array.isArray(rec.entries)) return null;
  const entries: FileExplorerEntry[] = [];
  for (const item of rec.entries) {
    const entry = normalizeEntry(item);
    if (entry) entries.push(entry);
  }
  return { entries, truncated: boolField(rec.truncated) };
}

export function normalizeRead(raw: unknown): FileExplorerReadResult | null {
  const rec = asRecord(raw);
  if (!rec) return null;
  const path = stringField(rec.path);
  if (!path) return null;
  const lines = stringLines(rec.lines);
  const result: FileExplorerReadResult = {
    path,
    lines,
    totalLines: Math.max(uintField(rec.totalLines), lines.length),
    sizeBytes: uintField(rec.sizeBytes),
    truncated: boolField(rec.truncated),
  };
  const nextCursor = optionalUint(rec.nextCursor);
  if (nextCursor != null) result.nextCursor = nextCursor;
  return result;
}

function normalizeChange(raw: unknown): FileExplorerChange | null {
  const rec = asRecord(raw);
  if (!rec) return null;
  const path = stringField(rec.path);
  const status = stringField(rec.status);
  if (!path || !CHANGE_STATUSES[status]) return null;
  const change: FileExplorerChange = { path, status: status as FileExplorerChangeStatus };
  const oldPath = optionalString(rec.oldPath);
  if (oldPath) change.oldPath = oldPath;
  return change;
}

export function normalizeChanges(raw: unknown): FileExplorerChangesResult | null {
  const rec = asRecord(raw);
  if (!rec || !Array.isArray(rec.files)) return null;
  const files: FileExplorerChange[] = [];
  for (const item of rec.files) {
    const change = normalizeChange(item);
    if (change) files.push(change);
  }
  return { files, isRepo: boolField(rec.isRepo), truncated: boolField(rec.truncated) };
}

export function normalizeDiff(raw: unknown): FileExplorerDiffResult | null {
  const rec = asRecord(raw);
  if (!rec) return null;
  const path = stringField(rec.path);
  if (!path) return null;
  const lines = stringLines(rec.lines);
  const result: FileExplorerDiffResult = {
    path,
    lines,
    totalLines: Math.max(uintField(rec.totalLines), lines.length),
    truncated: boolField(rec.truncated),
  };
  const nextCursor = optionalUint(rec.nextCursor);
  if (nextCursor != null) result.nextCursor = nextCursor;
  return result;
}

function blameCommit(value: unknown): string | null | undefined {
  if (value === null) return null;
  if (typeof value !== 'string' || !/^(?:[0-9a-fA-F]{40}|[0-9a-fA-F]{64})$/.test(value)) return undefined;
  if (/^0+$/.test(value)) return undefined;
  return value;
}

function blameLineNumber(value: unknown): number | null {
  if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 1) return null;
  return value;
}

function normalizeBlameLine(raw: unknown): FileExplorerBlameLine | null {
  const rec = asRecord(raw);
  if (!rec) return null;
  const commit = blameCommit(rec.commit);
  if (commit === undefined) return null;
  if (typeof rec.author !== 'string') return null;
  if (typeof rec.time !== 'number' || !Number.isSafeInteger(rec.time) || rec.time < 0 || rec.time > 8.64e12) return null;
  const originalLine = blameLineNumber(rec.originalLine);
  const currentLine = blameLineNumber(rec.currentLine);
  if (originalLine == null || currentLine == null) return null;
  if (typeof rec.text !== 'string') return null;
  if ('email' in rec || 'authorMail' in rec || 'author-mail' in rec) return null;
  return {
    commit,
    author: rec.author,
    time: rec.time,
    originalLine,
    currentLine,
    text: rec.text,
  };
}

/** Strict blame parse: exact path, ordered current lines covering the
 *  requested inclusive range one-for-one, frozen per-line fields. Anything
 *  else is unusable (null) — the caller reports it and writes nothing. */
export function normalizeBlame(
  raw: unknown,
  expected: { path: string; startLine: number; endLine: number },
): FileExplorerBlameResult | null {
  const rec = asRecord(raw);
  if (!rec) return null;
  if (stringField(rec.path) !== expected.path) return null;
  if (!Array.isArray(rec.lines)) return null;
  const count = expected.endLine - expected.startLine + 1;
  if (
    !Number.isInteger(expected.startLine)
    || !Number.isInteger(expected.endLine)
    || expected.startLine < 1
    || count < 1
    || count > FILE_EXPLORER_BLAME_MAX_LINES
    || rec.lines.length !== count
  ) return null;
  const lines: FileExplorerBlameLine[] = [];
  for (let index = 0; index < rec.lines.length; index += 1) {
    const line = normalizeBlameLine(rec.lines[index]);
    if (!line || line.currentLine !== expected.startLine + index) return null;
    lines.push(line);
  }
  return { path: expected.path, lines };
}

export function normalizeStageDownload(raw: unknown): FileExplorerStageDownloadResult | null {
  const rec = asRecord(raw);
  if (!rec) return null;
  const downloadId = stringField(rec.downloadId);
  const fileName = stringField(rec.fileName);
  if (!downloadId || !fileName) return null;
  return { downloadId, fileName };
}

export function appendPagedLines<T extends { lines: string[]; nextCursor?: number; truncated: boolean; totalLines?: number }>(
  current: T,
  page: { lines: string[]; nextCursor?: number; truncated: boolean; totalLines?: number },
): T {
  const next: T = {
    ...current,
    lines: current.lines.concat(page.lines),
    truncated: page.truncated,
  };
  if (page.nextCursor != null) next.nextCursor = page.nextCursor;
  else delete next.nextCursor;
  if (page.totalLines != null) {
    next.totalLines = Math.max(current.totalLines ?? 0, page.totalLines, next.lines.length);
  }
  return next;
}

/* ------------------------------------------------------------------ *
 * Page-request ownership: exact session + requested path/view + latest
 * request generation. Canonical path identity lets the backend's
 * normalized returned path ('x') match a user-opened './x' tab so
 * load-more appends instead of resetting, while the tab identity stays
 * user-owned. Failures settle loading and keep prior visible content.
 * ------------------------------------------------------------------ */

export function fileExplorerCanonicalPath(path: string): string {
  const trimmed = path.trim().replace(/\\/g, '/');
  if (!trimmed) return '';
  const absolute = trimmed.startsWith('/');
  const out: string[] = [];
  for (const segment of trimmed.split('/')) {
    if (segment === '' || segment === '.') continue;
    if (segment === '..') {
      if (out.length > 0 && out[out.length - 1] !== '..') out.pop();
      else if (!absolute) out.push('..');
      continue;
    }
    out.push(segment);
  }
  const joined = out.join('/');
  return absolute ? `/${joined}` : joined;
}

export function fileExplorerSamePath(a: string, b: string): boolean {
  return fileExplorerCanonicalPath(a) === fileExplorerCanonicalPath(b);
}

export interface FileExplorerPageRequest {
  sessionId: string;
  path: string;
  view: FileExplorerTabView;
  cursor?: number;
  generation: number;
  /** Authoritative connection generation captured when the request started.
   *  A transport fail stores this as transportFailedEpoch so a later epoch
   *  can retry once without mount-local last-seen. */
  connectionEpoch?: number;
}

export interface FileExplorerPagePayload {
  path: string;
  lines: string[];
  nextCursor?: number;
  truncated: boolean;
  sizeBytes?: number;
  totalLines?: number;
}

export type FileExplorerPageError = {
  message: string;
  code?: RpcErrorCode;
};

export interface FileExplorerPageState {
  kind: FileExplorerTabView;
  path: string;
  startCursor: number;
  lines: string[];
  nextCursor?: number;
  /** Backend logical-line count; footer is loaded-end / totalLines. */
  totalLines: number;
  truncated: boolean;
  sizeBytes?: number;
  /** Diff parser state immediately before `lines[0]`, retained across cache compaction. */
  diffStartState?: FileExplorerDiffParseState;
  loading: boolean;
  error: FileExplorerPageError | null;
  /**
   * Transport drop settled this page without a local error. Not a success
   * (empty or otherwise): retry when transportFailedEpoch differs from the
   * current authoritative connectionEpoch. Eager remount fetch must not
   * treat this as a cache miss that spins.
   */
  transportIncomplete: boolean;
  /** Epoch that produced this transport latch. Cleared on begin/apply. */
  transportFailedEpoch?: number;
  /** Request cursor that failed (undefined = first page). Load-more retry
   *  must reuse this so the next apply appends instead of reloading page 1. */
  transportCursor?: number;
  generation: number;
  sessionId: string;
  /** Restored per-tab content scroll; lives on this cache slot. */
  scrollTop?: number;
  /** Monotonic LRU stamp from the module touch clock; the cache bound
   *  evicts the smallest stamps first. */
  touch: number;
}

/** State after issuing a page request: loading, keeping prior visible
 *  lines only when the same tab (canonical path + view) is reloaded.
 *  Clears a prior transport-incomplete latch (epoch + cursor) so the
 *  in-flight request owns the slot. */
export function fileExplorerBeginPage(
  state: FileExplorerPageState | null,
  request: FileExplorerPageRequest,
): FileExplorerPageState {
  if (state && state.kind === request.view && fileExplorerSamePath(state.path, request.path)) {
    return {
      ...state,
      loading: true,
      error: null,
      transportIncomplete: false,
      transportFailedEpoch: undefined,
      transportCursor: undefined,
      generation: request.generation,
      sessionId: request.sessionId,
      touch: nextPageCacheTouch(),
    };
  }
  return {
    kind: request.view,
    path: request.path,
    startCursor: request.cursor ?? 0,
    lines: [],
    totalLines: 0,
    truncated: false,
    loading: true,
    error: null,
    transportIncomplete: false,
    transportFailedEpoch: undefined,
    transportCursor: undefined,
    generation: request.generation,
    sessionId: request.sessionId,
    touch: nextPageCacheTouch(),
  };
}

/** Response identity for an ESTABLISHED page: an exact canonical match,
 *  or a workspace-RELATIVE echo for an ABSOLUTE request (the backend
 *  answers absolute reads with the workspace-relative form). Deliberately
 *  strict — the unique-basename relaxation lives only in
 *  [`fileExplorerResolvedRequestPath`] and applies to the one-time probe,
 *  never to pagination: once a page identity exists (even single-segment),
 *  every later response must match it exactly or it is rejected as a
 *  homonym mix. */
export function fileExplorerResponseMatchesRequest(requestPath: string, pagePath: string): boolean {
  const request = fileExplorerCanonicalPath(requestPath);
  const response = fileExplorerCanonicalPath(pagePath);
  if (!request || !response) return false;
  if (request === response) return true;
  return !response.startsWith('/') && request.startsWith('/') && request.endsWith(`/${response}`);
}

/** Raw echo gate BEFORE canonicalization: only separator normalization is
 *  applied, so `..` and drive-prefix components cannot be silently erased
 *  (`C:/../b/init.rs` must never read as `b/init.rs`) while names that
 *  merely CONTAIN `..` (`a..b/init.rs`) stay eligible. */
function fileExplorerEchoIsSafeRelative(pagePath: string): boolean {
  const raw = pagePath.trim().replace(/\\/g, '/');
  if (!raw || raw.startsWith('/') || /^[A-Za-z]:/.test(raw)) return false;
  return !raw.split('/').some((segment) => segment === '..');
}

/** ONE-TIME probe resolution for the markdown open path (never page
 *  validation): the workspace-relative path a successful read echo proves,
 *  or `null`. An exact canonical match returns the request; a
 *  workspace-RELATIVE echo is accepted for an ABSOLUTE request; and a
 *  single-segment request accepts a deeper relative echo ONLY when the raw
 *  echo is strictly workspace-relative (no `..`, no drive prefix) and ends
 *  with the exact segment — the server's unique-basename fallback identity.
 *  Callers open the returned RESOLVED path so the drawer's established
 *  identity is the resolved path and pagination validates strictly. */
export function fileExplorerResolvedRequestPath(requestPath: string, pagePath: string): string | null {
  const request = fileExplorerCanonicalPath(requestPath);
  const response = fileExplorerCanonicalPath(pagePath);
  if (!request || !response) return null;
  if (request === response) return request;
  if (response.startsWith('/')) return null;
  if (request.startsWith('/') && request.endsWith(`/${response}`)) return request;
  if (
    !request.includes('/')
    && fileExplorerEchoIsSafeRelative(pagePath)
    && response.endsWith(`/${request}`)
  ) {
    return response;
  }
  return null;
}

/** Loaded logical-line end (exclusive cursor). */
export function fileExplorerLoadedEnd(state: Pick<FileExplorerPageState, 'startCursor' | 'lines'>): number {
  return state.startCursor + state.lines.length;
}

function fileExplorerResolveTotalLines(
  state: FileExplorerPageState,
  page: FileExplorerPagePayload,
  loadedEnd: number,
): number {
  const fromPage = page.totalLines != null ? uintField(page.totalLines) : 0;
  return Math.max(state.totalLines, fromPage, loadedEnd);
}

/** Distance from the content-viewport bottom that triggers the next page:
 *  one viewport-fraction of remaining scroll, bounded so a degenerate
 *  viewport (0 height / NaN geometry) never triggers. 160px ≈ 8 code lines
 *  at the 12px/1.45 metrics — close enough that the user has effectively
 *  reached the bottom, far enough that the fetch lands before they do. */
export const FILE_EXPLORER_NEXT_PAGE_NEAR_BOTTOM_PX = 160;

/** Scroll-driven load-more decision. True when the content viewport has
 *  scrolled within the threshold of the bottom, a next page exists, and no
 *  page request is in flight. Pure so the threshold math is unit-tested
 *  without DOM. */
export function fileExplorerShouldLoadNext(options: {
  scrollTop: number;
  clientHeight: number;
  scrollHeight: number;
  hasNext: boolean;
  isLoading: boolean;
}): boolean {
  const { scrollTop, clientHeight, scrollHeight, hasNext, isLoading } = options;
  if (!hasNext || isLoading) return false;
  if (!Number.isFinite(scrollTop) || !Number.isFinite(clientHeight) || !Number.isFinite(scrollHeight)) return false;
  const remaining = scrollHeight - scrollTop - clientHeight;
  return remaining <= FILE_EXPLORER_NEXT_PAGE_NEAR_BOTTOM_PX;
}

/** Identity of one in-flight read/diff page (session + path + view + cursor). */
export function fileExplorerInflightKey(
  sessionId: string,
  path: string,
  view: FileExplorerTabView,
  cursor?: number,
): string {
  return `${sessionId}\0${fileExplorerCanonicalPath(path)}\0${view}\0${cursor ?? ''}`;
}

/** One process-module generation. Every request is unique; never reused
 *  after eviction or remount. Wrap at MAX_SAFE_INTEGER fails closed. */
let moduleRequestGeneration = 0;

export function nextFileExplorerRequestGeneration(): number {
  if (moduleRequestGeneration >= Number.MAX_SAFE_INTEGER) {
    throw new Error('file explorer request generation exhausted');
  }
  moduleRequestGeneration += 1;
  return moduleRequestGeneration;
}

export function fileExplorerPeekRequestGeneration(): number {
  return moduleRequestGeneration;
}

export interface FileExplorerInflightRequest {
  identity: string;
  request: FileExplorerPageRequest;
  promise: Promise<FileExplorerPageCache>;
}

const moduleInflight = new Map<string, FileExplorerInflightRequest>();

export function fileExplorerPeekInflight(
  identity: string,
  live: Map<string, FileExplorerInflightRequest> = moduleInflight,
): FileExplorerInflightRequest | null {
  return live.get(identity) ?? null;
}

/** Acquire ONE shared promise per exact identity. Existing live promises are
 *  reused (StrictMode replay / close-reopen). A and B coexist. */
export function fileExplorerAcquireInflight(
  identity: string,
  create: () => { request: FileExplorerPageRequest; promise: Promise<FileExplorerPageCache> },
  live: Map<string, FileExplorerInflightRequest> = moduleInflight,
): FileExplorerInflightRequest {
  const existing = live.get(identity);
  if (existing) return existing;
  const created = create();
  const entry: FileExplorerInflightRequest = { identity, request: created.request, promise: created.promise };
  live.set(identity, entry);
  return entry;
}

const moduleDirInflight = new Map<string, Promise<unknown>>();

/** Acquire ONE shared list promise per session+path. StrictMode replay and
 *  close-reopen reuse the live promise; generation is assigned once in create. */
export function fileExplorerAcquireDirInflight(
  identity: string,
  create: () => Promise<unknown>,
  live: Map<string, Promise<unknown>> = moduleDirInflight,
): Promise<unknown> {
  const existing = live.get(identity);
  if (existing) return existing;
  const promise = create().finally(() => {
    if (live.get(identity) === promise) live.delete(identity);
  });
  live.set(identity, promise);
  return promise;
}

export function fileExplorerPeekDirInflight(
  identity: string,
  live: Map<string, Promise<unknown>> = moduleDirInflight,
): Promise<unknown> | null {
  return live.get(identity) ?? null;
}

/** Exact deletion after the module cache has settled (success or transport). */
export function fileExplorerReleaseInflight(
  identity: string,
  live: Map<string, FileExplorerInflightRequest> = moduleInflight,
): void {
  live.delete(identity);
}

/** Host-stamped rpcCode only; unknown or malformed codes are dropped. */
function hostRpcCode(error: unknown): RpcErrorCode | undefined {
  if (error == null || typeof error !== 'object' || Array.isArray(error) || !('rpcCode' in error)) return undefined;
  const rpcCode: unknown = error.rpcCode;
  return rpcErrorCode({ code: rpcCode });
}

function inflightPageError(error: unknown): FileExplorerPageError | null {
  if (error == null || isTransportError(error)) return null;
  const message = error instanceof Error ? (error.message || 'request failed') : String(error);
  const code = hostRpcCode(error);
  return code ? { message, code } : { message };
}

/** Parse a read/diff payload independently of any mounted panel and write the
 *  module page cache once against the CURRENT cache + original generation.
 *  Never begins a new request here: a newer cursor/generation is a no-op. */
export function fileExplorerSettleInflightCache(
  request: FileExplorerPageRequest,
  data: unknown,
  error?: unknown,
): FileExplorerPageCache {
  return updateFileExplorerPageCache((current) => {
    if (error !== undefined) {
      return fileExplorerFailCachedPage(current, request, inflightPageError(error));
    }
    if (request.view === 'diff') {
      const parsed = normalizeDiff(data);
      return parsed
        ? fileExplorerApplyCachedPage(current, request, parsed)
        : fileExplorerFailCachedPage(current, request, { message: 'unusable diff response' });
    }
    const parsed = normalizeRead(data);
    return parsed
      ? fileExplorerApplyCachedPage(current, request, parsed)
      : fileExplorerFailCachedPage(current, request, { message: 'unusable file response' });
  });
}

export function fileExplorerCanLoadMore(state: FileExplorerPageState): boolean {
  return !state.loading
    && !state.transportIncomplete
    && state.nextCursor != null
    && fileExplorerLoadedEnd(state) < state.totalLines;
}

/** True when the loaded window and nextCursor disagree with totalLines. */
export function fileExplorerTruncationInconsistent(state: FileExplorerPageState): boolean {
  const end = fileExplorerLoadedEnd(state);
  if (end < state.totalLines && state.nextCursor == null) return true;
  if (end >= state.totalLines && state.nextCursor != null) return true;
  return false;
}

/** Apply a read/diff response. Returns the state unchanged when the
 *  request is stale (superseded generation, other session, other tab
 *  path/view). A returned path that does not canonically match the
 *  requested path is rejected: loading settles, prior content stays. */
export function fileExplorerApplyPage(
  state: FileExplorerPageState | null,
  request: FileExplorerPageRequest,
  page: FileExplorerPagePayload,
): FileExplorerPageState | null {
  if (
    !state
    || state.generation !== request.generation
    || state.sessionId !== request.sessionId
    || state.kind !== request.view
    || !fileExplorerSamePath(state.path, request.path)
  ) {
    return state;
  }
  if (!fileExplorerResponseMatchesRequest(request.path, page.path)) {
    return { ...state, loading: false, error: { message: 'response path mismatch' }, transportIncomplete: false, transportFailedEpoch: undefined, transportCursor: undefined };
  }
  if (request.cursor == null) {
    const next: FileExplorerPageState = {
      kind: state.kind,
      path: request.path,
      startCursor: request.cursor ?? 0,
      lines: page.lines,
      nextCursor: page.nextCursor,
      totalLines: fileExplorerResolveTotalLines(state, page, page.lines.length),
      truncated: page.truncated,
      sizeBytes: page.sizeBytes,
      loading: false,
      error: null,
      transportIncomplete: false,
      transportFailedEpoch: undefined,
      transportCursor: undefined,
      generation: state.generation,
      sessionId: state.sessionId,
      touch: nextPageCacheTouch(),
    };
    if (state.scrollTop != null) next.scrollTop = state.scrollTop;
    return next;
  }
  const merged = appendPagedLines(state, page);
  return {
    ...merged,
    totalLines: fileExplorerResolveTotalLines(state, page, fileExplorerLoadedEnd(merged)),
    sizeBytes: page.sizeBytes,
    loading: false,
    error: null,
    transportIncomplete: false,
    transportFailedEpoch: undefined,
    transportCursor: undefined,
    touch: nextPageCacheTouch(),
  };
}

/** Settle a failed read/diff: keep prior visible content, clear loading.
 *  `error` null (transport) latches transportIncomplete, stores the
 *  request connectionEpoch and the failed cursor. A later authoritative
 *  epoch retries that exact cursor (load-more appends). Begin clears the
 *  latch. Stale requests leave the state untouched. */
export function fileExplorerFailPage(
  state: FileExplorerPageState | null,
  request: FileExplorerPageRequest,
  error: FileExplorerPageError | null,
): FileExplorerPageState | null {
  if (
    !state
    || state.generation !== request.generation
    || state.sessionId !== request.sessionId
    || state.kind !== request.view
    || !fileExplorerSamePath(state.path, request.path)
  ) {
    return state;
  }
  if (error == null) {
    return {
      ...state,
      loading: false,
      error: null,
      transportIncomplete: true,
      transportFailedEpoch: request.connectionEpoch ?? 0,
      transportCursor: request.cursor,
    };
  }
  return {
    ...state,
    loading: false,
    error,
    transportIncomplete: false,
    transportFailedEpoch: undefined,
    transportCursor: undefined,
  };
}

/* ------------------------------------------------------------------ *
 * Per tab/path/view/session page cache. Tiny external store: getSnapshot
 * returns a stable immutable object; update(fn) is atomic; subscribers
 * (useSyncExternalStore) re-render. begin/apply/fail/scroll/settle write
 * only through update, so a local panel cannot clobber an unrelated
 * session's settlement.
 * ------------------------------------------------------------------ */

export type FileExplorerPageCache = Record<string, FileExplorerPageState>;

let modulePageCache: FileExplorerPageCache = {};
const cacheListeners: Array<() => void> = [];

export function emptyFileExplorerPageCache(): FileExplorerPageCache {
  return {};
}

export function subscribeFileExplorerPageCache(onStoreChange: () => void): () => void {
  cacheListeners.push(onStoreChange);
  return () => {
    const index = cacheListeners.indexOf(onStoreChange);
    if (index >= 0) cacheListeners.splice(index, 1);
  };
}

export function getFileExplorerPageCacheSnapshot(): FileExplorerPageCache {
  return modulePageCache;
}

/** Atomic store update. `fn` sees the current snapshot and must return a
 *  new object (or the same ref for a no-op). Bounded + admission-pruned
 *  before publish. */
export function updateFileExplorerPageCache(
  fn: (prev: FileExplorerPageCache) => FileExplorerPageCache,
): FileExplorerPageCache {
  const prev = modulePageCache;
  let next = fn(prev);
  next = fileExplorerBoundPageCache(next);
  if (admittedCacheIdentities) next = fileExplorerPruneCacheToAdmission(next, admittedCacheIdentities);
  if (next === prev) return prev;
  modulePageCache = next;
  for (const listener of cacheListeners) listener();
  return next;
}

/** Monotonic LRU clock: every page write (request begin / response apply)
 *  stamps its entry; the bound below evicts the smallest stamps first. */
let pageCacheTouchClock = 0;
function nextPageCacheTouch(): number {
  return ++pageCacheTouchClock;
}

/** Aggregate retained cache estimate. `bytes` is a runtime-heap estimate:
 *  UTF-16 code units per line plus one separator — NOT the wire size. */
export interface FileExplorerPageCacheEstimate {
  entries: number;
  sessions: number;
  lines: number;
  bytes: number;
}

function pageEntriesEstimate(entries: Array<[string, FileExplorerPageState]>): FileExplorerPageCacheEstimate {
  const sessions = new Set<string>();
  let lines = 0;
  let bytes = 0;
  for (const [key, state] of entries) {
    sessions.add(key.slice(0, key.indexOf('\0')));
    for (const line of state.lines) {
      lines += 1;
      bytes += (line.length + 1) * 2;
    }
  }
  return { entries: entries.length, sessions: sessions.size, lines, bytes };
}

export function fileExplorerPageCacheEstimate(cache: FileExplorerPageCache): FileExplorerPageCacheEstimate {
  return pageEntriesEstimate(Object.entries(cache));
}

function pageCacheRecency(state: FileExplorerPageState): number {
  return typeof state.touch === 'number' && Number.isFinite(state.touch) ? state.touch : 0;
}

/** Compact one oversized entry to its last FILE_EXPLORER_MAX_LINES_PER_ENTRY
 *  lines, shifting startCursor so the loaded end stays coherent. The written
 *  key is never a victim: an entry that alone exceeds the aggregate caps is
 *  trimmed in place instead of being evicted. */
function compactOversizedEntry(state: FileExplorerPageState): FileExplorerPageState {
  if (state.lines.length <= FILE_EXPLORER_MAX_LINES_PER_ENTRY) return state;
  const dropCount = state.lines.length - FILE_EXPLORER_MAX_LINES_PER_ENTRY;
  const keep = state.lines.slice(dropCount);
  return {
    ...state,
    startCursor: state.startCursor + dropCount,
    lines: keep,
    diffStartState: state.kind === 'diff'
      ? fileExplorerDiffStateAfter(state.lines.slice(0, dropCount), state.diffStartState)
      : undefined,
  };
}

/** Deterministically bound a page cache (panel state AND the registry run
 *  through this on every write). While any cap — sessions, entries,
 *  aggregate lines, aggregate bytes — is exceeded, evict the
 *  least-recently-touched session in full, else the single LRU entry.
 *  Within the caps nothing is evicted, so A→B→A paging stays intact.
 *  `protectedKey` (the just-written key) is never a victim: it is
 *  proactively compacted to the per-entry line budget when oversized, and
 *  the eviction loop skips it, so a single huge file accumulated via
 *  load-more can never be dropped by the write that produced it. */
export function fileExplorerBoundPageCache(
  cache: FileExplorerPageCache,
  protectedKey?: string,
): FileExplorerPageCache {
  const all = Object.entries(cache);
  if (all.length === 0) return cache;
  all.sort((a, b) => pageCacheRecency(a[1]) - pageCacheRecency(b[1]) || (a[0] < b[0] ? -1 : 1));
  const sessionOf = (key: string): string => key.slice(0, key.indexOf('\0'));
  const sessionOrder = (entries: Array<[string, FileExplorerPageState]>): string[] => {
    const firstSeen = new Map<string, number>();
    for (const [key, state] of entries) {
      const session = sessionOf(key);
      if (!firstSeen.has(session)) firstSeen.set(session, pageCacheRecency(state));
    }
    return [...firstSeen.keys()].sort((a, b) => (firstSeen.get(a) ?? 0) - (firstSeen.get(b) ?? 0));
  };
  const kept = all.slice();
  let changed = false;
  // Proactively compact the protected (just-written) key when it ALONE
  // exceeds the aggregate caps (a single huge file accumulated via
  // load-more) so it can never be the cause of a cap overflow — the written
  // key is never a victim. Entries within the caps (e.g. a 500-line page)
  // are left untouched.
  if (protectedKey != null) {
    const index = kept.findIndex(([key]) => key === protectedKey);
    if (index >= 0) {
      const [key, state] = kept[index];
      const alone = pageEntriesEstimate([[key, state]]);
      if (alone.lines > FILE_EXPLORER_MAX_CACHE_LINES || alone.bytes > FILE_EXPLORER_MAX_CACHE_BYTES) {
        const compacted = compactOversizedEntry(state);
        if (compacted !== state) {
          kept[index] = [key, compacted];
          changed = true;
        }
      }
    }
  }
  while (kept.length > 0) {
    const sessions = sessionOrder(kept);
    const est = pageEntriesEstimate(kept);
    if (
      est.entries <= FILE_EXPLORER_MAX_CACHE_ENTRIES
      && sessions.length <= FILE_EXPLORER_MAX_SESSIONS
      && est.lines <= FILE_EXPLORER_MAX_CACHE_LINES
      && est.bytes <= FILE_EXPLORER_MAX_CACHE_BYTES
    ) {
      break;
    }
    if (sessions.length > FILE_EXPLORER_MAX_SESSIONS) {
      // Evict the least-recently-touched session — never the protected
      // key's session; skip to the next when it is the LRU.
      const victim = sessions[0];
      const target = protectedKey != null && victim === sessionOf(protectedKey)
        ? sessions.find((s) => s !== victim)
        : victim;
      if (target == null) break;
      for (let index = kept.length - 1; index >= 0; index -= 1) {
        if (sessionOf(kept[index][0]) === target) kept.splice(index, 1);
      }
      changed = true;
    } else {
      // Evict the LRU entry — never the protected key itself; when it is
      // the LRU, evict the next entry instead (it is already compacted, so
      // if it is the only entry the caps hold).
      if (protectedKey != null && kept[0][0] === protectedKey) {
        if (kept.length === 1) break;
        kept.splice(1, 1);
      } else {
        kept.shift();
      }
      changed = true;
    }
  }
  if (!changed) return cache;
  const out: FileExplorerPageCache = {};
  for (const [key, state] of kept) out[key] = state;
  return out;
}

/** Admission identity for one session+path: both views (file/diff) share
 *  it, so a tab drop removes the pair. */
export function fileExplorerPageCacheIdentity(sessionId: string, path: string): string {
  return `${sessionId}\0${fileExplorerCanonicalPath(path)}`;
}

export function fileExplorerPruneCacheToAdmission(
  cache: FileExplorerPageCache,
  admitted: Set<string>,
): FileExplorerPageCache {
  const out: FileExplorerPageCache = {};
  for (const [key, state] of Object.entries(cache)) {
    if (admitted.has(key.slice(0, key.lastIndexOf('\0')))) out[key] = state;
  }
  return out;
}

/** Admission derived from the App's slot map — the single source of truth
 *  for what the registry retains. Null until the App syncs once, so pure
 *  helper callers and unit tests that never sync keep prior behavior. */
let admittedCacheIdentities: Set<string> | null = null;

/** Re-derive admission from the App's slot map (every session's open tabs)
 *  and drop registry keys that lost admission: closed tabs, tabs evicted by
 *  the 8-tab cap, and evicted sessions. Called from App's single
 *  slot-update path; a later stale panel-state write cannot resurrect
 *  dropped keys because every registry write re-prunes to admission. */
export function fileExplorerSyncPageCacheWithSlots(slots: Record<string, FileExplorerSessionSlot>): void {
  const admitted = new Set<string>();
  for (const [sessionId, slot] of Object.entries(slots)) {
    for (const tab of slot.tabs) admitted.add(fileExplorerPageCacheIdentity(sessionId, tab.path));
  }
  admittedCacheIdentities = admitted;
  updateFileExplorerPageCache((prev) => fileExplorerPruneCacheToAdmission(prev, admitted));
  updateFileExplorerDirCache((prev) => {
    const admittedSessions = new Set(Object.keys(slots));
    const next: FileExplorerDirCache = {};
    for (const [key, state] of Object.entries(prev)) {
      if (admittedSessions.has(key.slice(0, key.indexOf('\0')))) next[key] = state;
    }
    return Object.keys(next).length === Object.keys(prev).length ? prev : next;
  });
  // Drawer view geometry shares the slot map's key space (owner key):
  // evicted sessions/mappings drop their view store entry here too.
  fileExplorerSyncViewCacheWithSlots(slots);
  // Conversation drafts + ask surfaces are per-path view state: a tab that
  // actually left the slot map (close / 8-tab eviction / evicted session)
  // drops both. Mode/hide switches keep every tab and therefore keep them.
  pruneFileExplorerConversationViewState(admitted);
}
/** Snapshot copy of the module store for tests/helpers. React reads the
 *  live object via getFileExplorerPageCacheSnapshot. */
export function readModuleFileExplorerPageCache(): FileExplorerPageCache {
  return { ...getFileExplorerPageCacheSnapshot() };
}
export function fileExplorerSanitizePageCache(cache: FileExplorerPageCache): FileExplorerPageCache {
  const next: FileExplorerPageCache = {};
  for (const [key, state] of Object.entries(cache)) {
    if (!state.loading) next[key] = state;
  }
  return next;
}

/** Replace the store contents (tests / wholesale restore). Goes through
 *  update() so listeners fire and admission still prunes. */
export function writeModuleFileExplorerPageCache(cache: FileExplorerPageCache): void {
  updateFileExplorerPageCache(() => cache);
}

export function fileExplorerPageCacheKey(
  sessionId: string,
  path: string,
  view: FileExplorerTabView,
): string {
  return `${sessionId}\0${fileExplorerCanonicalPath(path)}\0${view}`;
}

export function fileExplorerReadCachedPage(
  cache: FileExplorerPageCache,
  sessionId: string,
  path: string,
  view: FileExplorerTabView,
): FileExplorerPageState | null {
  return cache[fileExplorerPageCacheKey(sessionId, path, view)] ?? null;
}

function writeCachedPage(
  cache: FileExplorerPageCache,
  request: FileExplorerPageRequest,
  next: FileExplorerPageState | null,
  current: FileExplorerPageState | null,
): FileExplorerPageCache {
  if (next === current) return cache;
  const key = fileExplorerPageCacheKey(request.sessionId, request.path, request.view);
  if (next == null) {
    if (cache[key] == null) return cache;
    const copy = { ...cache };
    delete copy[key];
    return copy;
  }
  return fileExplorerBoundPageCache({ ...cache, [key]: next }, key);
}

export function fileExplorerBeginCachedPage(
  cache: FileExplorerPageCache,
  request: FileExplorerPageRequest,
): FileExplorerPageCache {
  const current = fileExplorerReadCachedPage(cache, request.sessionId, request.path, request.view);
  return writeCachedPage(cache, request, fileExplorerBeginPage(current, request), current);
}

export function fileExplorerApplyCachedPage(
  cache: FileExplorerPageCache,
  request: FileExplorerPageRequest,
  page: FileExplorerPagePayload,
): FileExplorerPageCache {
  const current = fileExplorerReadCachedPage(cache, request.sessionId, request.path, request.view);
  return writeCachedPage(cache, request, fileExplorerApplyPage(current, request, page), current);
}

/** Persist content-pane scrollTop on the existing per-tab cache slot. */
export function fileExplorerSetCachedScrollTop(
  cache: FileExplorerPageCache,
  sessionId: string,
  path: string,
  view: FileExplorerTabView,
  scrollTop: number,
): FileExplorerPageCache {
  const current = fileExplorerReadCachedPage(cache, sessionId, path, view);
  if (!current) return cache;
  const nextTop = Math.max(0, Math.floor(scrollTop));
  if (current.scrollTop === nextTop) return cache;
  return writeCachedPage(
    cache,
    { sessionId, path, view, generation: current.generation },
    { ...current, scrollTop: nextTop },
    current,
  );
}


export function fileExplorerFailCachedPage(
  cache: FileExplorerPageCache,
  request: FileExplorerPageRequest,
  error: FileExplorerPageError | null,
): FileExplorerPageCache {
  const current = fileExplorerReadCachedPage(cache, request.sessionId, request.path, request.view);
  return writeCachedPage(cache, request, fileExplorerFailPage(current, request, error), current);
}

/** False when this identity already has a settled content window covering
 *  `cursor` (including A→B→A first-page). True for a cold key, a
 *  load-more (`cursor === nextCursor`) / a start outside the loaded
 *  range, and any settled page that never produced content (failed or
 *  empty) — those must refetch on close/reopen instead of pinning the
 *  error/empty state forever. In-flight keys do not refetch.
 *  Transport-incomplete pages are not an eager cache miss: they retry
 *  when transportFailedEpoch differs from the current connectionEpoch. */
export function fileExplorerShouldFetchPage(
  state: FileExplorerPageState | null,
  identity: {
    sessionId: string;
    path: string;
    view: FileExplorerTabView;
    cursor?: number;
  },
): boolean {
  if (!state) return true;
  if (
    state.sessionId !== identity.sessionId
    || state.kind !== identity.view
    || !fileExplorerSamePath(state.path, identity.path)
  ) {
    return true;
  }
  if (state.loading) return false;
  if (state.transportIncomplete) return false;
  const requested = identity.cursor;
  if (requested != null && requested === state.nextCursor) return true;
  // A settled page that never produced content — a failed read/diff or a
  // successful-but-empty file — is not a cache hit: close/reopen and
  // A→B→A remounts must refetch it instead of pinning the error/empty
  // state forever. Only a settled page with lines and no error counts as
  // a covered window.
  if (state.error != null || state.lines.length === 0) return true;
  const start = state.startCursor;
  const end = start + state.lines.length;
  const cursor = requested ?? 0;
  return cursor < start || cursor >= end;
}

/** One directory listing slot. Transport drops latch `transportIncomplete`
 *  and stamp `transportFailedEpoch` so a later connectionEpoch can reload
 *  without leaving the tree spinning. Per-path `generation` rejects stale
 *  older list responses. */
export interface FileExplorerDirState {
  entries: FileExplorerEntry[];
  truncated: boolean;
  loading: boolean;
  error: string | null;
  errorCode?: RpcErrorCode;
  transportIncomplete: boolean;
  transportFailedEpoch?: number;
  /** Per session+path list generation. Stale older responses cannot overwrite. */
  generation: number;
}

export function fileExplorerEmptyDirState(): FileExplorerDirState {
  return { entries: [], truncated: false, loading: false, error: null, transportIncomplete: false, generation: 0 };
}

export function fileExplorerDirInflightKey(sessionId: string, path: string): string {
  return `${sessionId}\0dir\0${fileExplorerCanonicalPath(path)}`;
}

export function fileExplorerBeginDirState(prev: FileExplorerDirState | undefined, generation: number): FileExplorerDirState {
  const unavailable = prev?.errorCode === 'directory_unavailable';
  return {
    entries: prev?.entries ?? [],
    truncated: prev?.truncated ?? false,
    loading: true,
    error: null,
    errorCode: unavailable ? 'directory_unavailable' : undefined,
    transportIncomplete: false,
    transportFailedEpoch: undefined,
    generation,
  };
}

/** True when `generation` still owns this dir slot. */
export function fileExplorerDirGenerationMatches(state: FileExplorerDirState | undefined, generation: number): boolean {
  return state != null && state.generation === generation;
}
export function fileExplorerFailDirState(
  prev: FileExplorerDirState | undefined,
  error: string | null,
  connectionEpoch: number,
  generation: number,
  errorCode?: RpcErrorCode,
): FileExplorerDirState {
  if (prev == null || prev.generation !== generation) {
    return prev ?? fileExplorerEmptyDirState();
  }
  if (error == null) {
    return {
      entries: prev.entries,
      truncated: prev.truncated,
      loading: false,
      error: null,
      errorCode: prev.errorCode === 'directory_unavailable' ? 'directory_unavailable' : undefined,
      transportIncomplete: true,
      transportFailedEpoch: connectionEpoch,
      generation,
    };
  }
  if (errorCode === 'directory_unavailable') {
    return {
      entries: [],
      truncated: false,
      loading: false,
      error: null,
      errorCode,
      transportIncomplete: false,
      generation,
    };
  }
  return {
    entries: [],
    truncated: false,
    loading: false,
    error,
    errorCode,
    transportIncomplete: false,
    generation,
  };
}

export function fileExplorerApplyDirState(
  prev: FileExplorerDirState | undefined,
  entries: FileExplorerEntry[],
  truncated: boolean,
  generation: number,
): FileExplorerDirState {
  if (prev == null || prev.generation !== generation) {
    return prev ?? fileExplorerEmptyDirState();
  }
  return { entries, truncated, loading: false, error: null, errorCode: undefined, transportIncomplete: false, generation };
}

export type FileExplorerDirCache = Record<string, FileExplorerDirState>;

let moduleDirCache: FileExplorerDirCache = {};
const dirCacheListeners: Array<() => void> = [];

export function subscribeFileExplorerDirCache(onStoreChange: () => void): () => void {
  dirCacheListeners.push(onStoreChange);
  return () => {
    const index = dirCacheListeners.indexOf(onStoreChange);
    if (index >= 0) dirCacheListeners.splice(index, 1);
  };
}

export function getFileExplorerDirCacheSnapshot(): FileExplorerDirCache {
  return moduleDirCache;
}

export function updateFileExplorerDirCache(
  fn: (prev: FileExplorerDirCache) => FileExplorerDirCache,
): FileExplorerDirCache {
  const prev = moduleDirCache;
  const next = fn(prev);
  if (next === prev) return prev;
  moduleDirCache = next;
  for (const listener of dirCacheListeners) listener();
  return next;
}

export function fileExplorerDirsForSession(
  cache: FileExplorerDirCache,
  sessionId: string | null,
): Record<string, FileExplorerDirState> {
  if (!sessionId) return {};
  const prefix = `${sessionId}\0dir\0`;
  const dirs: Record<string, FileExplorerDirState> = {};
  for (const [key, state] of Object.entries(cache)) {
    if (key.startsWith(prefix)) dirs[key.slice(prefix.length)] = state;
  }
  return dirs;
}

export function fileExplorerWriteDirState(
  cache: FileExplorerDirCache,
  sessionId: string,
  path: string,
  update: (prev: FileExplorerDirState | undefined) => FileExplorerDirState,
): FileExplorerDirCache {
  const key = fileExplorerDirInflightKey(sessionId, path);
  const previous = cache[key];
  const state = update(previous);
  if (state === previous) return cache;
  return { ...cache, [key]: state };
}

/** Paths whose listing is transport-stuck and not currently in flight. */
export function fileExplorerTransportIncompleteDirPaths(
  dirs: Record<string, FileExplorerDirState>,
): string[] {
  const paths: string[] = [];
  for (const [path, node] of Object.entries(dirs)) {
    if (node.transportIncomplete && !node.loading) paths.push(path);
  }
  return paths;
}

/** Transport-stuck dirs whose failed epoch is not the current one. */
export function fileExplorerRetryableTransportDirPaths(
  dirs: Record<string, FileExplorerDirState>,
  connectionEpoch: number,
): string[] {
  const paths: string[] = [];
  for (const [path, node] of Object.entries(dirs)) {
    if (
      node.transportIncomplete
      && !node.loading
      && node.transportFailedEpoch !== connectionEpoch
    ) {
      paths.push(path);
    }
  }
  return paths;
}

/** Cursor to retry for a transport-incomplete page. Load-more keeps the
 *  failed request cursor; first-page transport uses the identity cursor. */
export function fileExplorerTransportRetryCursor(
  state: Pick<FileExplorerPageState, 'transportIncomplete' | 'transportCursor'> | null,
  fallback?: number,
): number | undefined {
  if (state != null && state.transportIncomplete) return state.transportCursor;
  return fallback;
}

/** True when the page is transport-stuck on a different epoch than the
 *  current authoritative connection. Same failed epoch never auto-retries;
 *  begin clears the latch so a new fail can stamp a new epoch. Close/reopen
 *  after the epoch advanced still retries because the latch lives on the
 *  module cache, not a mount-local last-seen. */
export function fileExplorerShouldRetryTransportPage(
  state: Pick<FileExplorerPageState, 'loading' | 'transportIncomplete' | 'transportFailedEpoch'> | null,
  connectionEpoch: number,
): boolean {
  if (state == null || state.loading || !state.transportIncomplete) return false;
  return state.transportFailedEpoch !== connectionEpoch;
}

/** True when any mounted directory listing is transport-stuck on a
 *  different epoch than the current authoritative connection. */
export function fileExplorerShouldRetryTransportDirs(
  dirs: Record<string, FileExplorerDirState>,
  connectionEpoch: number,
): boolean {
  return fileExplorerRetryableTransportDirPaths(dirs, connectionEpoch).length > 0;
}

/* ------------------------------------------------------------------ *
 * Tab / drawer session state (keyed by sessionId in the App)
 * ------------------------------------------------------------------ */

export function openFileTab(
  tabs: FileExplorerTab[],
  path: string,
  line?: number,
  view: FileExplorerTabView = 'file',
): { tabs: FileExplorerTab[]; activePath: string } {
  const normalized = path.trim();
  const index = tabs.findIndex((tab) => tab.path === normalized);
  if (index >= 0) {
    const current = tabs[index];
    const reopened: FileExplorerTab = {
      path: current.path,
      line: line ?? current.line,
      view,
    };
    if (line != null) reopened.lineRequest = line;
    // Tab identity persists across re-activation: the conversation id is
    // bound to the tab's LIFETIME, not to the last open call.
    if (current.conversationId != null) reopened.conversationId = current.conversationId;
    const next = tabs.slice();
    next[index] = reopened;
    return { tabs: next, activePath: normalized };
  }
  const tab: FileExplorerTab = { path: normalized, view };
  if (line != null) {
    tab.line = line;
    tab.lineRequest = line;
  }
  return { tabs: [...tabs, tab], activePath: normalized };
}

export function closeFileTab(
  tabs: FileExplorerTab[],
  activePath: string | null,
  path: string,
): { tabs: FileExplorerTab[]; activePath: string | null } {
  const closedIndex = tabs.findIndex((tab) => tab.path === path);
  if (closedIndex < 0) return { tabs, activePath };
  const next = tabs.filter((tab) => tab.path !== path);
  if (activePath !== path) return { tabs: next, activePath };
  const fallback = next[Math.min(closedIndex, next.length - 1)];
  return { tabs: next, activePath: fallback ? fallback.path : null };
}

export function applyOpenPath(
  slot: FileExplorerSessionSlot,
  path: string,
  line?: number,
  view: FileExplorerTabView = 'file',
): FileExplorerSessionSlot {
  const opened = openFileTab(slot.tabs, path, line, view);
  return { ...slot, tabs: opened.tabs, activePath: opened.activePath, mode: 'open' };
}

export function applySelectTab(
  slot: FileExplorerSessionSlot,
  path: string,
  view?: FileExplorerTabView,
): FileExplorerSessionSlot {
  const tab = slot.tabs.find((candidate) => candidate.path === path);
  if (!tab) return slot;
  const tabs = view == null || tab.view === view
    ? slot.tabs
    : slot.tabs.map((candidate) => candidate.path === path ? { ...candidate, view } : candidate);
  return { ...slot, tabs, activePath: path, mode: slot.mode === 'closed' ? 'open' : slot.mode };
}

export function applyCloseTab(
  slot: FileExplorerSessionSlot,
  path: string,
): FileExplorerSessionSlot {
  const closed = closeFileTab(slot.tabs, slot.activePath, path);
  return { ...slot, tabs: closed.tabs, activePath: closed.activePath };
}

export function applyDrawerMode(
  slot: FileExplorerSessionSlot,
  mode: FileExplorerDrawerMode,
): FileExplorerSessionSlot {
  return { ...slot, mode };
}

/* ------------------------------------------------------------------ *
 * File conversation (Explorer Ask dock): wire builders + strict parsers.
 *
 * Every command is session-stamped by the caller through
 * fileExplorerCommandForSession. The conversation id is owned by the tab
 * (FileExplorerTab.conversationId); snapshots are the single source of
 * status/streaming state — no mirrored flags.
 * ------------------------------------------------------------------ */

export interface FileConversationLineRange {
  startLine: number;
  endLine: number;
}

export function fileConversationOpenCommand(conversationId: string, path: string): Record<string, unknown> {
  return { type: 'file_conversation_open', conversationId, path };
}

export function fileConversationPromptCommand(
  conversationId: string,
  message: string,
  selection?: FileConversationLineRange,
): Record<string, unknown> {
  return {
    type: 'file_conversation_prompt',
    conversationId,
    message,
    ...(selection
      ? { selection: { startLine: selection.startLine, endLine: selection.endLine } }
      : {}),
  };
}

export function fileConversationSnapshotCommand(conversationId: string): Record<string, unknown> {
  return { type: 'file_conversation_snapshot', conversationId };
}

export function fileConversationAbortCommand(conversationId: string): Record<string, unknown> {
  return { type: 'file_conversation_abort', conversationId };
}

export function fileConversationCloseCommand(conversationId: string): Record<string, unknown> {
  return { type: 'file_conversation_close', conversationId };
}

/** Strict file-conversation close acknowledgement: the backend answers
 *  `{ conversationId, closed: true }` (file_conversation.rs close). A close
 *  that resolves without BOTH the requested conversation id and `closed:
 *  true` is never success — the host surfaces a rejection through its
 *  central error layer instead of accepting a silent no-op. */
export function fileConversationCloseAckValid(data: unknown, conversationId: string): boolean {
  if (data == null || typeof data !== 'object') return false;
  if (!('conversationId' in data) || !('closed' in data)) return false;
  return data.conversationId === conversationId && data.closed === true;
}

/** Hard byte ceiling for the prompt message itself, mirroring the backend
 *  FILE_CONVERSATION_PROMPT_MAX_BYTES (8192). The UI rejects an oversized
 *  draft BEFORE sending so the draft stays editable. */
export const FILE_CONVERSATION_PROMPT_MAX_BYTES = 8192;

/** Client-side mirror of the backend prompt ceiling; a rejection happens
 *  before any RPC so the draft is preserved untouched. */
export function fileConversationPromptTooLarge(message: string): boolean {
  return utf8ByteLength(message) > FILE_CONVERSATION_PROMPT_MAX_BYTES;
}

/** Client-generated conversation id (UUID v4; falls back to getRandomValues
 *  hex on engines without randomUUID). */
export function newFileConversationId(): string {
  if (typeof crypto !== 'undefined' && typeof crypto.randomUUID === 'function') {
    return crypto.randomUUID();
  }
  const bytes = new Uint8Array(16);
  if (typeof crypto !== 'undefined' && typeof crypto.getRandomValues === 'function') {
    crypto.getRandomValues(bytes);
  } else {
    for (let index = 0; index < bytes.length; index += 1) {
      bytes[index] = Math.floor(Math.random() * 256);
    }
  }
  bytes[6] = (bytes[6] & 0x0f) | 0x40;
  bytes[8] = (bytes[8] & 0x3f) | 0x80;
  const hex = Array.from(bytes, (byte) => byte.toString(16).padStart(2, '0')).join('');
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}

/** Prompt ACK is success only when the wire stamps `accepted: true` and the
 *  conversation is not busy. Absent `accepted`, `accepted: false`, or
 *  `busy: true` keeps the typed draft. */
export function fileConversationPromptAccepted(snapshot: { accepted?: boolean; busy?: boolean }): boolean {
  return snapshot.accepted === true && snapshot.busy !== true;
}

function conversationRangeField(value: unknown): FileConversationLineRange | null {
  const record = asRecord(value);
  if (!record) return null;
  const startLine = uintField(record.startLine);
  const endLine = uintField(record.endLine);
  if (startLine < 1 || endLine < startLine) return null;
  return { startLine, endLine };
}

/** Strict context validation: absent is fine; present-but-malformed (null,
 *  missing truncated, malformed ranges) rejects the WHOLE snapshot — no
 *  partial context is invented. */
function conversationContextField(value: unknown): FileConversationContextWire | null {
  const record = asRecord(value);
  if (!record) return null;
  if (typeof record.truncated !== 'boolean') return null;
  if (!('includedRange' in record)) return null;
  const includedRange = record.includedRange == null
    ? null
    : conversationRangeField(record.includedRange);
  if (record.includedRange != null && includedRange == null) return null;
  let requestedRange: FileConversationLineRange | undefined;
  if (record.requestedRange != null) {
    const parsed = conversationRangeField(record.requestedRange);
    if (!parsed) return null;
    requestedRange = parsed;
  }
  return {
    includedRange,
    truncated: record.truncated,
    ...(requestedRange ? { requestedRange } : {}),
  };
}

const CONVERSATION_ENTRY_KINDS: Record<string, true> = {
  user: true,
  assistant: true,
  tool: true,
  system: true,
  thinking: true,
};

const CONVERSATION_ENTRY_ROLES: Record<string, true> = {
  user: true,
  assistant: true,
  tool: true,
  system: true,
};

function normalizeConversationEntry(raw: unknown): SideChatEntryWire | null {
  const record = asRecord(raw);
  if (!record) return null;
  const id = stringField(record.id);
  const kind = typeof record.kind === 'string' ? record.kind : '';
  const role = typeof record.role === 'string' ? record.role : '';
  if (!id || !CONVERSATION_ENTRY_KINDS[kind] || !CONVERSATION_ENTRY_ROLES[role]) return null;
  if (typeof record.text !== 'string') return null;
  if (typeof record.isError !== 'boolean' || typeof record.isPartial !== 'boolean') return null;
  const entry: SideChatEntryWire = {
    id,
    kind: kind as SideChatEntryKind,
    role: role as SideChatEntryWire['role'],
    text: record.text,
    isError: record.isError,
    isPartial: record.isPartial,
  };
  if ('toolName' in record) {
    if (record.toolName != null && typeof record.toolName !== 'string') return null;
    entry.toolName = record.toolName ?? null;
  }
  if ('toolArgs' in record) entry.toolArgs = record.toolArgs;
  if ('output' in record) {
    if (record.output != null && typeof record.output !== 'string') return null;
    entry.output = record.output ?? null;
  }
  if ('thinking' in record) {
    if (record.thinking != null && typeof record.thinking !== 'string') return null;
    entry.thinking = record.thinking ?? null;
  }
  return entry;
}

/** Strict snapshot parse. Identity comes FIRST: conversationId AND the
 *  canonical expected path must match exactly, and the response shape must
 *  be fully well-formed — anything else rejects the whole response (the
 *  caller reports it through the host's central error surface; nothing is
 *  written). */
export function normalizeFileConversationSnapshot(
  raw: unknown,
  expected: { conversationId: string; path: string },
): FileConversationSnapshot | null {
  const record = asRecord(raw);
  if (!record) return null;
  const conversationId = stringField(record.conversationId);
  const path = stringField(record.path);
  if (!conversationId || !path) return null;
  if (conversationId !== expected.conversationId) return null;
  if (fileExplorerCanonicalPath(path) !== fileExplorerCanonicalPath(expected.path)) return null;
  if (typeof record.streaming !== 'boolean') return null;
  if (typeof record.status !== 'string') return null;
  if (!Array.isArray(record.entries)) return null;
  const entries: SideChatEntryWire[] = [];
  for (const rawEntry of record.entries) {
    const entry = normalizeConversationEntry(rawEntry);
    if (!entry) return null;
    entries.push(entry);
  }
  let context: FileConversationContextWire | undefined;
  if (record.context != null) {
    const parsed = conversationContextField(record.context);
    if (!parsed) return null;
    context = parsed;
  }
  if ('accepted' in record && typeof record.accepted !== 'boolean') return null;
  if ('busy' in record && typeof record.busy !== 'boolean') return null;
  return {
    conversationId,
    path,
    streaming: record.streaming,
    status: record.status,
    entries,
    ...(context ? { context } : {}),
    ...('accepted' in record ? { accepted: record.accepted as boolean } : {}),
    ...('busy' in record ? { busy: record.busy as boolean } : {}),
  };
}

/** One conversation whose owning tab left the slot map: the close RPC must
 *  retire the backend controller for the tab's own id. */
export interface FileExplorerConversationClosure {
  ownerKey: string;
  path: string;
  conversationId: string;
}

/** Conversation ids whose owning tab dropped from the slot map (explicit
 *  tab close, 8-tab eviction, evicted session). Read from the PRE-patch
 *  slot map plus patch.droppedPathsBySession so the host closes the tab's
 *  OWN id — never a lookup against the post-delete state. */
export function fileExplorerConversationClosuresFromPatch(
  prev: Record<string, FileExplorerSessionSlot>,
  patch: FileExplorerSlotsPatch,
): FileExplorerConversationClosure[] {
  const closures: FileExplorerConversationClosure[] = [];
  const seen = new Set<string>();
  for (const [ownerKey, droppedPaths] of Object.entries(patch.droppedPathsBySession)) {
    const tabs = prev[ownerKey]?.tabs ?? [];
    for (const dropped of droppedPaths) {
      const tab = tabs.find((candidate) => fileExplorerCanonicalPath(candidate.path) === dropped);
      if (!tab?.conversationId || seen.has(tab.conversationId)) continue;
      seen.add(tab.conversationId);
      closures.push({ ownerKey, path: tab.path, conversationId: tab.conversationId });
    }
  }
  return closures;
}

/** Close RPCs in flight, keyed by conversation id: a StrictMode replay of
 *  the same slot patch must not double-fire. */
const conversationClosesInFlight = new Set<string>();

/** User-readable message for one close failure: the stamped Error message
 *  when present, else a named fallback (the panel's errorMessage shape). */
function conversationErrorMessage(error: unknown): string {
  if (error != null && typeof error === 'object' && 'message' in error) {
    const message: unknown = error.message;
    if (typeof message === 'string' && message.trim() !== '') return message;
  }
  return 'file conversation close failed';
}

/** Typed close outcomes only: the exact backend not_found code means the
 *  conversation was already retired — the close is COMPLETE. Every other
 *  failure propagates to the host's central error surface (no global
 *  catch, no swallowing). */
export function fileConversationCloseAlreadyRetired(error: unknown): boolean {
  return hostRpcCode(error) === 'file_conversation_not_found';
}

export interface FileConversationCloseAck {
  conversationId: string;
  closed: true;
}

/** Fire-and-forget close for every conversation id whose tab left the slot
 *  map. Called from the hosts' single slot-update path, next to the page-
 *  cache sync. `send` receives the RAW close command plus the owning slot
 *  key (App: the victim session id it stamps; Chat: the victim mapping id
 *  it routes through its captured original binding). Returns a promise that
 *  settles after every close. Success is only `fileConversationCloseAckValid`
 *  for that conversation; the exact not-found code is the only non-fatal
 *  rejection. This helper never writes React state. */
export function closeFileConversationsDroppedBySlotPatch(
  prev: Record<string, FileExplorerSessionSlot>,
  patch: FileExplorerSlotsPatch,
  send: (command: Record<string, unknown>, ownerKey: string) => unknown,
  onError: (message: string) => void,
): Promise<void> {
  const pending: Promise<void>[] = [];
  for (const closure of fileExplorerConversationClosuresFromPatch(prev, patch)) {
    if (conversationClosesInFlight.has(closure.conversationId)) continue;
    conversationClosesInFlight.add(closure.conversationId);
    const finish = () => {
      conversationClosesInFlight.delete(closure.conversationId);
    };
    const rejectAck = (data: unknown): void => {
      if (!fileConversationCloseAckValid(data, closure.conversationId)) {
        onError(`unexpected close acknowledgement for conversation ${closure.conversationId}`);
      }
    };
    try {
      const result = send(fileConversationCloseCommand(closure.conversationId), closure.ownerKey);
      const promise = result != null && typeof (result as Promise<unknown>).then === 'function'
        ? result as Promise<unknown>
        : Promise.resolve(result);
      pending.push(promise.then(
        (data) => {
          rejectAck(data);
        },
        (error: unknown) => {
          if (!fileConversationCloseAlreadyRetired(error)) onError(conversationErrorMessage(error));
        },
      ).finally(finish));
    } catch (error: unknown) {
      finish();
      if (!fileConversationCloseAlreadyRetired(error)) onError(conversationErrorMessage(error));
    }
  }
  return Promise.all(pending).then(() => undefined);
}

/** One merged workspace-tree row: a filesystem entry or a Git-only change
 *  (deleted/renamed old path) injected into the correct virtual directory
 *  hierarchy, with the effective status resolved for that path. */
export interface FileExplorerTreeRow extends FileExplorerEntry {
  change?: FileExplorerChange;
  /** True only for a Git-only path that cannot be opened as source: backend
   *  deleted paths and rename-old paths. Added / modified / rename-new stay
   *  ordinary even when absent from a truncated listing. */
  gitOnly?: boolean;
  /** Listed filesystem file exceeds the backend preview response cap. */
  oversized?: boolean;
  /** Git-injected ordinary source row has no authoritative listing size. */
  sizeUnknown?: boolean;
  /** Descendant change count for directory rows (0 omitted). */
  changeCount?: number;
  /** Aggregate status tone for a directory with descendants. */
  changeTone?: FileExplorerChangeStatus;
}

const CHANGE_TONE_RANK: Record<FileExplorerChangeStatus, number> = {
  deleted: 4,
  renamed: 3,
  added: 2,
  modified: 1,
};

function fileExplorerDirPrefix(dirPath: string): string {
  const canonical = fileExplorerCanonicalPath(dirPath);
  return canonical ? `${canonical}/` : '';
}

/** Parent directory of a workspace-relative path (`''` at the root). */
export function fileExplorerParentDir(path: string): string {
  const canonical = fileExplorerCanonicalPath(path);
  const slash = canonical.lastIndexOf('/');
  return slash < 0 ? '' : canonical.slice(0, slash);
}

function fileExplorerChangeTargets(change: FileExplorerChange): string[] {
  const targets = [change.path];
  if (change.oldPath && !fileExplorerSamePath(change.oldPath, change.path)) {
    targets.push(change.oldPath);
  }
  return targets;
}

/** Count/tone of Git changes whose path (or rename old path) sits under `dirPath`.
 *  One logical change counts once (a rename is not old+new). */
export function fileExplorerDirAggregate(
  dirPath: string,
  changes: readonly FileExplorerChange[],
): { count: number; tone?: FileExplorerChangeStatus } {
  const prefix = fileExplorerDirPrefix(dirPath);
  const canonicalDir = fileExplorerCanonicalPath(dirPath);
  let count = 0;
  let tone: FileExplorerChangeStatus | undefined;
  let rank = 0;
  for (const change of changes) {
    let under = false;
    for (const target of fileExplorerChangeTargets(change)) {
      const canonical = fileExplorerCanonicalPath(target);
      if (!canonical) continue;
      const isUnder = canonicalDir
        ? canonical === canonicalDir || canonical.startsWith(prefix)
        : !canonical.startsWith('/');
      if (isUnder && canonical !== canonicalDir) {
        under = true;
        break;
      }
    }
    if (!under) continue;
    count += 1;
    const nextRank = CHANGE_TONE_RANK[change.status] ?? 0;
    if (nextRank > rank) {
      rank = nextRank;
      tone = change.status;
    }
  }
  return { count, tone };
}

/** One direct-child injection candidate for a canonical directory:
 *  a change target whose canonical path descends from that directory.
 *  Precomputed once per changes array so the recursive tree render never
 *  rescans the catalog per subtree. */
export interface FileExplorerChangeChild {
  readonly change: FileExplorerChange;
  /** The raw change target — rows keep the wire path identity. */
  readonly target: string;
  /** Direct-child segment of `target` under the directory. */
  readonly child: string;
  /** Resolved child path (canonical prefix + child; root keeps relative). */
  readonly childPath: string;
  /** Canonical identity used for the seen-set. */
  readonly key: string;
  /** True when the directory is the target's immediate parent (file row);
   *  false = virtual intermediate dir row. */
  readonly immediate: boolean;
  /** Deleted-path / rename-old row (diff-only, never download). */
  readonly gitOnly: boolean;
}

/** Memoized per-changes-array index for the merged tree: exact-path and
 *  rename-old lookups, per-directory descendant aggregates, and per-directory
 *  direct-child injection candidates. MEASURED (node microbench, 500-change
 *  cap + 33 expanded subtrees): the brute-force rescan cost ~235ms per
 *  panel render because every visible subtree re-scanned all changes per
 *  row; one O(changes × depth) build + O(1) lookups replaces that. */
export interface FileExplorerChangeIndex {
  /** First change whose `path` canonically matches (insertion order kept). */
  readonly byPath: ReadonlyMap<string, FileExplorerChange>;
  /** First change whose rename `oldPath` canonically matches. */
  readonly byOldPath: ReadonlyMap<string, FileExplorerChange>;
  /** Descendant aggregate per canonical dir (one rename = 1; loudest tone). */
  readonly dirAggregates: ReadonlyMap<string, { count: number; tone?: FileExplorerChangeStatus }>;
  /** Direct-child injection candidates per canonical dir, in changes order
   *  (a rename contributes both endpoints). */
  readonly childrenByDir: ReadonlyMap<string, readonly FileExplorerChangeChild[]>;
}

/** Build the change index once per changes array (panel useMemo). */
export function fileExplorerBuildChangeIndex(changes: readonly FileExplorerChange[]): FileExplorerChangeIndex {
  const byPath = new Map<string, FileExplorerChange>();
  const byOldPath = new Map<string, FileExplorerChange>();
  const dirAggregates = new Map<string, { count: number; tone?: FileExplorerChangeStatus }>();
  const childrenByDir = new Map<string, FileExplorerChangeChild[]>();
  for (const change of changes) {
    const pathKey = fileExplorerCanonicalPath(change.path);
    if (pathKey && !byPath.has(pathKey)) byPath.set(pathKey, change);
    if (change.oldPath && !fileExplorerSamePath(change.oldPath, change.path)) {
      const oldKey = fileExplorerCanonicalPath(change.oldPath);
      if (oldKey && !byOldPath.has(oldKey)) byOldPath.set(oldKey, change);
    }
    const touched = new Set<string>();
    for (const target of fileExplorerChangeTargets(change)) {
      const canonical = fileExplorerCanonicalPath(target);
      if (!canonical) continue;
      // Strict ancestor dirs of this target; root '' only for relative
      // targets (absolute paths are excluded from the workspace root, same
      // as fileExplorerDirAggregate). `touched` is per CHANGE, not per
      // target: one logical rename counts once per directory.
      const absolute = canonical.startsWith('/');
      const segments = (absolute ? canonical.slice(1) : canonical).split('/');
      const base = absolute ? '/' : '';
      for (let i = 0; i < segments.length - 1; i++) {
        const dir = base + segments.slice(0, i + 1).join('/');
        if (touched.has(dir)) continue;
        touched.add(dir);
        const entry = dirAggregates.get(dir) ?? { count: 0 };
        entry.count += 1;
        const rank = CHANGE_TONE_RANK[change.status] ?? 0;
        const currentRank = entry.tone ? CHANGE_TONE_RANK[entry.tone] ?? 0 : 0;
        if (rank > currentRank) entry.tone = change.status;
        dirAggregates.set(dir, entry);
      }
      // Injection candidates: every strict ancestor dir sees this target
      // as a direct child on its path.
      for (let i = 0; i < segments.length; i++) {
        const dir = base + segments.slice(0, i).join('/');
        const child = segments[i];
        const childPath = dir ? `${dir}/${child}` : child;
        const list = childrenByDir.get(dir) ?? [];
        list.push({
          change,
          target,
          child,
          childPath,
          key: fileExplorerCanonicalPath(childPath),
          immediate: i === segments.length - 1,
          gitOnly: (change.status === 'deleted' && fileExplorerSamePath(target, change.path))
            || (change.status === 'renamed' && change.oldPath != null && fileExplorerSamePath(target, change.oldPath)),
        });
        childrenByDir.set(dir, list);
      }
    }
  }
  return { byPath, byOldPath, dirAggregates, childrenByDir };
}

function fileExplorerPickChangeFromIndex(
  path: string,
  index: FileExplorerChangeIndex,
): FileExplorerChange | undefined {
  const key = fileExplorerCanonicalPath(path);
  return index.byPath.get(key) ?? index.byOldPath.get(key);
}

/**
 * Merge one workspace directory listing with the change catalog into ONE
 * consistent tree (gitui/Code Review style; no bottom orphan section).
 *
 *  - Filesystem entries keep their kind; rows with a same-path change carry
 *    its status for the colored marker.
 *  - Deleted paths and rename-old paths absent from the listing inject as
 *    gitOnly file rows (diff-only, never download).
 *  - Added / modified / rename-new paths absent from a truncated listing
 *    inject as ordinary openable files.
 *  - Catalog-only and `unavailable` directories hoist their git-only files
 *    here and are not listed. A mixed ordinary descendant stays expandable.
 *  - Directory rows aggregate descendant change count + tone (one rename = 1).
 *
 * `index` (optional) is the memoized per-changes-array index — the recursive
 * tree render passes the panel's shared one down. Omitted: built here once,
 * which is still strictly cheaper than the per-row rescan it replaces.
 */
export function fileExplorerMergeTree(
  dirPath: string,
  entries: readonly FileExplorerEntry[],
  changes: readonly FileExplorerChange[],
  index: FileExplorerChangeIndex = fileExplorerBuildChangeIndex(changes),
  unavailable: Readonly<Record<string, true>> = {},
): FileExplorerTreeRow[] {
  const canonicalDir = fileExplorerCanonicalPath(dirPath);
  const rows: FileExplorerTreeRow[] = [];
  const seen = new Set<string>();
  const dropped = (path: string): boolean => unavailable[fileExplorerCanonicalPath(path)] === true;
  const labelUnder = (target: string): string => {
    const canonical = fileExplorerCanonicalPath(target);
    const prefix = canonicalDir ? `${canonicalDir}/` : '';
    return canonical.startsWith(prefix) && canonical.length > prefix.length
      ? canonical.slice(prefix.length)
      : fileExplorerBasename(target);
  };

  const pushDir = (name: string, path: string): void => {
    const key = fileExplorerCanonicalPath(path);
    if (!key && path !== '') return;
    if (seen.has(key) || dropped(path)) return;
    seen.add(key);
    const aggregate = index.dirAggregates.get(key) ?? (key === '' ? index.dirAggregates.get('') : undefined);
    const selfChange = fileExplorerPickChangeFromIndex(path, index);
    const row: FileExplorerTreeRow = { name, path, kind: 'dir' };
    if (selfChange) {
      row.change = selfChange;
      row.changeTone = selfChange.status;
      row.changeCount = Math.max(aggregate?.count ?? 0, 1);
    } else if (aggregate && aggregate.count > 0) {
      row.changeCount = aggregate.count;
      row.changeTone = aggregate.tone;
    }
    rows.push(row);
  };
  const gitOnlyTree = (dirKey: string): boolean => {
    const children = index.childrenByDir.get(dirKey) ?? [];
    return children.length > 0 && children.every((child) => (
      child.gitOnly && (child.immediate || gitOnlyTree(child.key))
    ));
  };
  const hoistGitOnly = (dirKey: string): void => {
    for (const child of index.childrenByDir.get(dirKey) ?? []) {
      if (seen.has(child.key)) continue;
      if (!child.immediate || dropped(child.childPath)) {
        hoistGitOnly(child.key);
        continue;
      }
      if (!child.gitOnly) continue;
      seen.add(child.key);
      rows.push({
        name: labelUnder(child.target),
        path: child.target,
        kind: 'file',
        change: child.change,
        gitOnly: true,
      });
    }
  };

  for (const entry of entries) {
    const key = fileExplorerCanonicalPath(entry.path);
    if (seen.has(key)) continue;
    if (entry.kind === 'dir') {
      if (dropped(entry.path)) {
        seen.add(key);
        hoistGitOnly(key);
        continue;
      }
      pushDir(entry.name, entry.path);
      continue;
    }
    seen.add(key);
    const change = fileExplorerPickChangeFromIndex(entry.path, index);
    rows.push({ ...entry, ...(change ? { change } : {}), ...(fileExplorerIsOversized(entry.sizeBytes) ? { oversized: true } : {}) });
  }

  const handled = new Set<string>();
  for (const item of index.childrenByDir.get(canonicalDir) ?? []) {
    if (seen.has(item.key) || handled.has(item.key)) continue;
    handled.add(item.key);
    if (!item.immediate || dropped(item.childPath)) {
      if (dropped(item.childPath) || gitOnlyTree(item.key)) {
        seen.add(item.key);
        hoistGitOnly(item.key);
      } else if (!seen.has(item.key)) {
        pushDir(item.child, item.childPath);
      }
      continue;
    }
    seen.add(item.key);
    const row: FileExplorerTreeRow = {
      name: fileExplorerBasename(item.target) || item.child,
      path: item.target,
      kind: 'file',
      change: item.change,
    };
    if (item.gitOnly) row.gitOnly = true;
    else row.sizeUnknown = true;
    rows.push(row);
  }

  rows.sort((a, b) => {
    if (a.kind !== b.kind) return a.kind === 'dir' ? -1 : 1;
    return a.name.localeCompare(b.name);
  });
  return rows;
}

/** Drawer view geometry: expanded dirs, inner tree split, tree scroll.
 *  HOT PATH authority: this state lives in the module external store below
 *  (keyed by the panel's owner key — App sessionId / Chat mapping id, the
 *  same key space as the session-slot map), NOT on the App/Chat session
 *  slot. A directory toggle or tree scroll writes the store synchronously
 *  and rerenders only the panel (useSyncExternalStore), never the App/Chat
 *  slot holders or the transcript. Close/unmount (drawer DOM absent) and
 *  A→B switches read the store back; owner eviction prunes it through the
 *  existing slot sync. Fresh dirs/changes refetch — only view geometry is
 *  retained, never stale payloads. */

/** 'ask' attaches the file-conversation dock to the path: desktop renders
 *  the dock ALONGSIDE the source/preview pane (source stays visible), mobile
 *  switches panes through the same surface radio row. Only the file view
 *  offers it — a diff view always renders source (selection never derives
 *  from diff rows). */
export type FileExplorerContentSurface = 'source' | 'preview' | 'ask' | 'map';

export interface FileExplorerViewState {
  /** Expanded workspace directory paths ('' = root). */
  expandedDirs: Record<string, true>;
  /** Inner tree/content split percent (22–55, default 34). */
  treePct: number;
  /** Workspace tree viewport scroll offset, restored after dirs settle. */
  treeScrollTop: number;
  /** Per-path content surface. Absent previewable path → preview. */
  surfaceByPath: Record<string, FileExplorerContentSurface>;
  /** Per-path unsent Ask composer draft. Same store as the geometry above
   *  (no mirrored authority): synchronous per-keystroke writes rerender only
   *  the panel, and the existing slot sync prunes dropped tabs' drafts. */
  conversationDraftByPath: Record<string, string>;
}

export function emptyFileExplorerViewState(): FileExplorerViewState {
  return {
    expandedDirs: { '': true },
    treePct: FILE_EXPLORER_TREE_PCT_DEFAULT,
    treeScrollTop: 0,
    surfaceByPath: {},
    conversationDraftByPath: {},
  };
}

/** Pure patch merge onto a view (same clamps the slot path used to apply). */
export function fileExplorerApplyViewPatch(
  view: FileExplorerViewState,
  patch: Partial<FileExplorerViewState>,
): FileExplorerViewState {
  return {
    expandedDirs: patch.expandedDirs ?? view.expandedDirs,
    treePct: patch.treePct != null ? clampFileExplorerTreePct(patch.treePct) : view.treePct,
    treeScrollTop: patch.treeScrollTop != null && Number.isFinite(patch.treeScrollTop)
      ? Math.max(0, Math.floor(patch.treeScrollTop))
      : view.treeScrollTop,
    surfaceByPath: patch.surfaceByPath ?? view.surfaceByPath ?? {},
    conversationDraftByPath: patch.conversationDraftByPath ?? view.conversationDraftByPath ?? {},
  };
}

/** Read one path's Ask draft. Pure. */
export function fileExplorerConversationDraft(
  view: FileExplorerViewState,
  path: string,
): string {
  return view.conversationDraftByPath?.[path] ?? '';
}

/** Write one path's Ask draft (pure; the panel calls this inside
 *  updateFileExplorerViewCache so a keystroke never touches App/Chat state). */
export function fileExplorerWithConversationDraft(
  view: FileExplorerViewState,
  path: string,
  draft: string,
): FileExplorerViewState {
  const current = view.conversationDraftByPath ?? {};
  if ((current[path] ?? '') === draft) return view;
  const conversationDraftByPath = { ...current };
  if (draft === '') delete conversationDraftByPath[path];
  else conversationDraftByPath[path] = draft;
  return { ...view, conversationDraftByPath };
}

/* ------------------------------------------------------------------ *
 * View-geometry external store. Same shape as the page/dir caches: one
 * module-lifetime map, useSyncExternalStore subscribers, synchronous
 * publish. One writer: the panel, through updateFileExplorerViewCache.
 * ------------------------------------------------------------------ */

export type FileExplorerViewCache = Record<string, FileExplorerViewState>;

let moduleViewCache: FileExplorerViewCache = {};
const viewCacheListeners: Array<() => void> = [];

export function emptyFileExplorerViewCache(): FileExplorerViewCache {
  return {};
}

export function subscribeFileExplorerViewCache(onStoreChange: () => void): () => void {
  viewCacheListeners.push(onStoreChange);
  return () => {
    const index = viewCacheListeners.indexOf(onStoreChange);
    if (index >= 0) viewCacheListeners.splice(index, 1);
  };
}

export function getFileExplorerViewCacheSnapshot(): FileExplorerViewCache {
  return moduleViewCache;
}

function publishFileExplorerViewCache(): void {
  for (const listener of viewCacheListeners) listener();
}

/** View for one owner key (defaults when absent). Pure. */
export function fileExplorerViewForOwner(
  cache: FileExplorerViewCache,
  ownerKey: string,
): FileExplorerViewState {
  return cache[ownerKey] ?? emptyFileExplorerViewState();
}

/** Synchronous view-store update. The updater runs EXACTLY ONCE against the
 *  current snapshot, so two rapid toggles can never lose an update, and the
 *  write lands before the caller starts a loadDir RPC. */
export function updateFileExplorerViewCache(
  ownerKey: string,
  update: (view: FileExplorerViewState) => FileExplorerViewState,
): void {
  const current = moduleViewCache[ownerKey] ?? emptyFileExplorerViewState();
  const next = update(current);
  if (next === current) return;
  moduleViewCache = { ...moduleViewCache, [ownerKey]: next };
  publishFileExplorerViewCache();
}

/** Patch one owner's view (clamped); the panel's scroll/split writer. */
export function patchFileExplorerViewCache(
  ownerKey: string,
  patch: Partial<FileExplorerViewState>,
): void {
  updateFileExplorerViewCache(ownerKey, (view) => fileExplorerApplyViewPatch(view, patch));
}

/** Toggle one directory's expansion. Pure; `expanded` reports the
 *  post-toggle state so the caller decides whether to loadDir. */
export function fileExplorerToggleExpandedDir(
  view: FileExplorerViewState,
  path: string,
): { view: FileExplorerViewState; expanded: boolean } {
  const expandedDirs = { ...view.expandedDirs };
  if (expandedDirs[path]) {
    delete expandedDirs[path];
    return { view: { ...view, expandedDirs }, expanded: false };
  }
  expandedDirs[path] = true;
  return { view: { ...view, expandedDirs }, expanded: true };
}

/** Drop view entries whose owner lost its App/Chat slot (session/mapping
 *  eviction or the bounded-slot cap). Called from the existing slot-sync
 *  path so eviction never leaks view geometry. */
export function fileExplorerSyncViewCacheWithSlots(slots: Record<string, FileExplorerSessionSlot>): void {
  const next: FileExplorerViewCache = {};
  let changed = false;
  for (const [ownerKey, view] of Object.entries(moduleViewCache)) {
    if (slots[ownerKey] != null) next[ownerKey] = view;
    else changed = true;
  }
  if (!changed) return;
  moduleViewCache = next;
  publishFileExplorerViewCache();
}

/** Snapshot copy of the module view store for tests/helpers. */
export function readModuleFileExplorerViewCache(): FileExplorerViewCache {
  return { ...moduleViewCache };
}

/** Replace the store contents (tests / wholesale reset). Publishes so
 *  subscribers re-render. */
export function writeModuleFileExplorerViewCache(cache: FileExplorerViewCache): void {
  moduleViewCache = cache;
  publishFileExplorerViewCache();
}

/** Prune per-path conversation drafts and ask surfaces whose tab left the
 *  slot map. Runs on the same admission set as the page cache (owner key +
 *  canonical path); a no-op view is returned by reference so unchanged
 *  owners never publish. */
function pruneFileExplorerConversationViewState(admitted: Set<string>): void {
  for (const ownerKey of Object.keys(moduleViewCache)) {
    updateFileExplorerViewCache(ownerKey, (view) => {
      let changed = false;
      const surfaceByPath: Record<string, FileExplorerContentSurface> = {};
      for (const [path, surface] of Object.entries(view.surfaceByPath ?? {})) {
        if (surface !== 'ask' || admitted.has(fileExplorerPageCacheIdentity(ownerKey, path))) {
          surfaceByPath[path] = surface;
        } else {
          changed = true;
        }
      }
      const conversationDraftByPath: Record<string, string> = {};
      for (const [path, draft] of Object.entries(view.conversationDraftByPath ?? {})) {
        if (admitted.has(fileExplorerPageCacheIdentity(ownerKey, path))) {
          conversationDraftByPath[path] = draft;
        } else {
          changed = true;
        }
      }
      if (!changed) return view;
      return { ...view, surfaceByPath, conversationDraftByPath };
    });
  }
}

/** Workspace-root label for self-describing out-of-workspace absolutes; a
 *  host absolute path is NEVER rendered raw (privacy contract). */
export const FILE_EXPLORER_WORKSPACE_LABEL = '<workspace>';

/** UI display identity: absolute requests keep their canonical internal
 *  identity, but the panel renders a workspace-relative path when the
 *  absolute path sits under `workspaceCwd`; an outside-workspace absolute
 *  path renders the symbolic workspace label prefix. */
export function fileExplorerDisplayPath(path: string, workspaceCwd?: string): string {
  const canonical = fileExplorerCanonicalPath(path);
  if (!canonical) return canonical;
  if (!canonical.startsWith('/')) return canonical;
  const root = workspaceCwd ? fileExplorerCanonicalPath(workspaceCwd) : '';
  if (root && root.startsWith('/')) {
    if (canonical === root) return '.';
    if (root.endsWith('/') ? canonical.startsWith(root) : canonical.startsWith(`${root}/`)) {
      return canonical.slice(root.length).replace(/^\//, '');
    }
  }
  return `${FILE_EXPLORER_WORKSPACE_LABEL}${canonical}`;
}

/* ------------------------------------------------------------------ *
 * Explicit caps. Tabs: FILE_EXPLORER_MAX_OPEN_TABS per session (TUI
 * file_viewer parity — the ninth open evicts the oldest non-active tab).
 * Sessions: the slot map holds at most FILE_EXPLORER_MAX_SESSIONS slots
 * with move-to-end LRU recency. Eviction exposes dropped identities so
 * the page cache (fileExplorerSyncPageCacheWithSlots) drops the file AND
 * diff keys for evicted/closed path+session. One source of truth: the
 * slot map derived admission.
 * ------------------------------------------------------------------ */

export interface FileExplorerSlotsPatch {
  slots: Record<string, FileExplorerSessionSlot>;
  /** Session slots dropped from the map (least-recently-updated first). */
  evictedSessions: string[];
  /** Canonical paths each session lost from its tabs (close inside the
   *  updater + oldest-tab eviction + evicted sessions' full tab sets). */
  droppedPathsBySession: Record<string, string[]>;
}

function canonicalTabIds(tabs: FileExplorerTab[]): string[] {
  const ids: string[] = [];
  for (const tab of tabs) {
    const id = fileExplorerCanonicalPath(tab.path);
    if (!ids.includes(id)) ids.push(id);
  }
  return ids;
}

/** Cap one session's tabs at FILE_EXPLORER_MAX_OPEN_TABS, evicting the
 *  oldest-inserted tabs; the active tab is never a victim. */
export function fileExplorerCapSessionTabs(
  slot: FileExplorerSessionSlot,
): { slot: FileExplorerSessionSlot; evictedPaths: string[] } {
  if (slot.tabs.length <= FILE_EXPLORER_MAX_OPEN_TABS) return { slot, evictedPaths: [] };
  const tabs = slot.tabs.slice();
  const evictedPaths: string[] = [];
  while (tabs.length > FILE_EXPLORER_MAX_OPEN_TABS) {
    const index = tabs.findIndex((tab) => tab.path !== slot.activePath);
    if (index < 0) break;
    const [tab] = tabs.splice(index, 1);
    evictedPaths.push(fileExplorerCanonicalPath(tab.path));
  }
  return { slot: { ...slot, tabs }, evictedPaths };
}

/** The App's single slot-update path: run `updater` on `sessionId`'s slot,
 *  cap its tabs, refresh the session's LRU recency (move to end), and
 *  bound the slot map at FILE_EXPLORER_MAX_SESSIONS by evicting the least-
 *  recently-updated session slot. Pure; callers must call
 *  fileExplorerSyncPageCacheWithSlots(patch.slots) so evicted tabs and
 *  sessions drop their page-cache keys. */
export function updateFileExplorerSlot(
  prev: Record<string, FileExplorerSessionSlot>,
  sessionId: string,
  updater: (slot: FileExplorerSessionSlot) => FileExplorerSessionSlot,
): FileExplorerSlotsPatch {
  const existed = sessionId in prev;
  const current = existed ? prev[sessionId] : emptyFileExplorerSlot();
  const capped = fileExplorerCapSessionTabs(updater(current));
  const keptIds = new Set(canonicalTabIds(capped.slot.tabs));
  const droppedPaths = canonicalTabIds(current.tabs).filter((id) => !keptIds.has(id));
  const slots: Record<string, FileExplorerSessionSlot> = {};
  for (const [sid, slot] of Object.entries(prev)) {
    if (sid !== sessionId) slots[sid] = slot;
  }
  if (existed || capped.slot.tabs.length > 0 || capped.slot.mode !== 'closed') slots[sessionId] = capped.slot;
  const droppedPathsBySession: Record<string, string[]> = droppedPaths.length > 0 ? { [sessionId]: droppedPaths } : {};
  const evictedSessions: string[] = [];
  while (Object.keys(slots).length > FILE_EXPLORER_MAX_SESSIONS) {
    const victim = Object.keys(slots)[0];
    delete slots[victim];
    evictedSessions.push(victim);
    const victimPaths = canonicalTabIds(prev[victim]?.tabs ?? []);
    if (victimPaths.length > 0) droppedPathsBySession[victim] = victimPaths;
  }
  return { slots, evictedSessions, droppedPathsBySession };
}

export function fileExplorerBasename(path: string): string {
  const trimmed = path.replace(/\\/g, '/').replace(/\/+$/, '');
  const slash = trimmed.lastIndexOf('/');
  return slash >= 0 ? trimmed.slice(slash + 1) : trimmed;
}

export type FileExplorerPreviewKind = 'markdown' | 'html' | 'svg';

export function fileExplorerPreviewKind(path: string): FileExplorerPreviewKind | null {
  const base = fileExplorerBasename(path).toLowerCase();
  if (base.endsWith('.md') || base.endsWith('.markdown')) return 'markdown';
  if (base.endsWith('.html') || base.endsWith('.htm')) return 'html';
  if (base.endsWith('.svg')) return 'svg';
  return null;
}

export function fileExplorerIsMarkdownPath(path: string): boolean {
  return fileExplorerPreviewKind(path) === 'markdown';
}

export function fileExplorerContentSurface(
  view: FileExplorerViewState,
  path: string,
  fileKind: FileExplorerTabView,
): FileExplorerContentSurface {
  if (fileKind !== 'file') return 'source';
  const stored = view.surfaceByPath?.[path];
  // Map used to replace the file. It is a header dialog now, so a stored
  // map surface falls through to Source or Preview.
  if (stored === 'ask') return stored;
  // Explicit Ask attach wins for ANY file (previewable or not).
  if (fileExplorerPreviewKind(path) == null) return 'source';
  if (stored == null || stored === 'map') return 'preview';
  return stored;
}

export function fileExplorerWithPathSurface(
  view: FileExplorerViewState,
  path: string,
  surface: FileExplorerContentSurface,
): FileExplorerViewState {
  const surfaceByPath = { ...(view.surfaceByPath ?? {}) };
  if (surface === 'preview') delete surfaceByPath[path];
  else surfaceByPath[path] = surface;
  return { ...view, surfaceByPath };
}

/* ------------------------------------------------------------------ *
 * One-hop dependency map. Frozen with crates/pi-cli/src/dependency_map.rs:
 * request {type:file_explorer_map, path}; response path + nodes +
 * outgoing + incoming + coverage. Targets are the tagged discriminant
 * file{path} | external{specifier} | unresolved{reason}. Purpose rows are
 * verbatim source documentation, never a generated description. A payload
 * that fails a bound is unusable (null) — the caller reports it and writes
 * nothing. visible_entries_only and incomplete stay as the backend sent
 * them; this normalizer never rewrites them to a zero-dependent claim.
 * ------------------------------------------------------------------ */

/** The backend's ONLY serialized bound (pi-cli/src/dependency_map.rs): a
 *  payload above 256 KiB fails the request server-side, so the web mirror
 *  rejects exactly what the backend could never have sent — no tighter
 *  node/edge/declaration caps that would reject a valid hub-file map. */
export const FILE_EXPLORER_MAP_MAX_PAYLOAD_BYTES = 256 * 1024;
export const FILE_EXPLORER_MAP_MAX_PATH_CHARS = 4096;
export const FILE_EXPLORER_MAP_MAX_TEXT_CHARS = 4096;

const MAP_LANGUAGES: Record<string, true> = {
  rust: true,
  typescript: true,
  tsx: true,
  javascript: true,
  solidity: true,
  unsupported: true,
};

const MAP_PARSE: Record<string, true> = {
  ok: true,
  incomplete: true,
  unsupported: true,
};

const MAP_EDGE_KINDS: Record<string, true> = {
  import: true,
  export_from: true,
  use: true,
  mod: true,
  sol_import: true,
};

export type FileExplorerMapLanguage = 'rust' | 'typescript' | 'tsx' | 'javascript' | 'solidity' | 'unsupported';
export type FileExplorerMapParse = 'ok' | 'incomplete' | 'unsupported';
export type FileExplorerMapEdgeKind = 'import' | 'export_from' | 'use' | 'mod' | 'sol_import';

export interface FileExplorerMapDeclaration {
  kind: string;
  name: string | null;
  line: number;
  doc: string | null;
}

export interface FileExplorerMapPurpose {
  line: number;
  text: string;
}

export type FileExplorerMapTarget =
  | { type: 'file'; path: string }
  | { type: 'external'; specifier: string }
  | { type: 'unresolved'; reason: string };

export interface FileExplorerMapNode {
  path: string;
  language: FileExplorerMapLanguage;
  parse: FileExplorerMapParse;
  declarations: FileExplorerMapDeclaration[];
  purpose: FileExplorerMapPurpose[];
}

export interface FileExplorerMapEdge {
  source: string;
  line: number;
  specifier: string;
  kind: FileExplorerMapEdgeKind;
  target: FileExplorerMapTarget;
}

export interface FileExplorerMapCoverage {
  scannedFiles: number;
  unsupportedFiles: number;
  unreadableFiles: number;
  unreadableDirectories: number;
  incompleteFiles: number;
  dynamicOmitted: number;
  truncated: boolean;
  visibleEntriesOnly: boolean;
  incomplete: boolean;
}

export interface FileExplorerMapResult {
  path: string;
  nodes: FileExplorerMapNode[];
  outgoing: FileExplorerMapEdge[];
  incoming: FileExplorerMapEdge[];
  coverage: FileExplorerMapCoverage;
}

export interface FileExplorerMapRequest {
  ownerKey: string;
  path: string;
  generation: number;
}

function mapLine(value: unknown): number | null {
  if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 1 || value > 10_000_000) return null;
  return value;
}

function mapCount(value: unknown): number | null {
  if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 0 || value > 5_000_000) return null;
  return value;
}

function boundedText(value: unknown, max: number): string | null {
  if (typeof value !== 'string' || value.length === 0 || value.length > max) return null;
  return value;
}

function normalizeMapDeclaration(raw: unknown): FileExplorerMapDeclaration | null {
  const rec = asRecord(raw);
  if (!rec) return null;
  const kind = boundedText(rec.kind, 64);
  const line = mapLine(rec.line);
  if (!kind || line == null) return null;
  let name: string | null = null;
  if (rec.name != null) {
    name = boundedText(rec.name, 512);
    if (name == null) return null;
  }
  let doc: string | null = null;
  if (rec.doc != null) {
    doc = boundedText(rec.doc, FILE_EXPLORER_MAP_MAX_TEXT_CHARS);
    if (doc == null) return null;
  }
  return { kind, name, line, doc };
}

function normalizeMapPurpose(raw: unknown): FileExplorerMapPurpose | null {
  const rec = asRecord(raw);
  if (!rec) return null;
  const line = mapLine(rec.line);
  const text = boundedText(rec.text, FILE_EXPLORER_MAP_MAX_TEXT_CHARS);
  if (line == null || text == null) return null;
  return { line, text };
}

function normalizeMapTarget(raw: unknown): FileExplorerMapTarget | null {
  const rec = asRecord(raw);
  if (!rec) return null;
  const type = stringField(rec.type);
  if (type === 'file') {
    const path = boundedText(rec.path, FILE_EXPLORER_MAP_MAX_PATH_CHARS);
    return path ? { type: 'file', path } : null;
  }
  if (type === 'external') {
    const specifier = boundedText(rec.specifier, FILE_EXPLORER_MAP_MAX_PATH_CHARS);
    return specifier ? { type: 'external', specifier } : null;
  }
  if (type === 'unresolved') {
    const reason = boundedText(rec.reason, FILE_EXPLORER_MAP_MAX_TEXT_CHARS);
    return reason ? { type: 'unresolved', reason } : null;
  }
  return null;
}

function normalizeMapNode(raw: unknown): FileExplorerMapNode | null {
  const rec = asRecord(raw);
  if (!rec) return null;
  const path = boundedText(rec.path, FILE_EXPLORER_MAP_MAX_PATH_CHARS);
  const language = stringField(rec.language);
  const parse = stringField(rec.parse);
  if (!path || !MAP_LANGUAGES[language] || !MAP_PARSE[parse]) return null;
  if (!Array.isArray(rec.declarations)) return null;
  if (!Array.isArray(rec.purpose)) return null;
  const declarations: FileExplorerMapDeclaration[] = [];
  for (const item of rec.declarations) {
    const declaration = normalizeMapDeclaration(item);
    if (!declaration) return null;
    declarations.push(declaration);
  }
  const purpose: FileExplorerMapPurpose[] = [];
  for (const item of rec.purpose) {
    const row = normalizeMapPurpose(item);
    if (!row) return null;
    purpose.push(row);
  }
  return {
    path,
    language: language as FileExplorerMapLanguage,
    parse: parse as FileExplorerMapParse,
    declarations,
    purpose,
  };
}

function normalizeMapEdge(raw: unknown): FileExplorerMapEdge | null {
  const rec = asRecord(raw);
  if (!rec) return null;
  const source = boundedText(rec.source, FILE_EXPLORER_MAP_MAX_PATH_CHARS);
  const line = mapLine(rec.line);
  const specifier = boundedText(rec.specifier, FILE_EXPLORER_MAP_MAX_PATH_CHARS);
  const kind = stringField(rec.kind);
  const target = normalizeMapTarget(rec.target);
  if (!source || line == null || !specifier || !MAP_EDGE_KINDS[kind] || !target) return null;
  return { source, line, specifier, kind: kind as FileExplorerMapEdgeKind, target };
}

function normalizeMapCoverage(raw: unknown): FileExplorerMapCoverage | null {
  const rec = asRecord(raw);
  if (!rec) return null;
  const scannedFiles = mapCount(rec.scanned_files);
  const unsupportedFiles = mapCount(rec.unsupported_files);
  const unreadableFiles = mapCount(rec.unreadable_files);
  const unreadableDirectories = mapCount(rec.unreadable_directories);
  const incompleteFiles = mapCount(rec.incomplete_files);
  const dynamicOmitted = mapCount(rec.dynamic_omitted);
  if (
    scannedFiles == null
    || unsupportedFiles == null
    || unreadableFiles == null
    || unreadableDirectories == null
    || incompleteFiles == null
    || dynamicOmitted == null
    || typeof rec.truncated !== 'boolean'
    || typeof rec.visible_entries_only !== 'boolean'
    || typeof rec.incomplete !== 'boolean'
  ) return null;
  return {
    scannedFiles,
    unsupportedFiles,
    unreadableFiles,
    unreadableDirectories,
    incompleteFiles,
    dynamicOmitted,
    truncated: rec.truncated,
    visibleEntriesOnly: rec.visible_entries_only,
    incomplete: rec.incomplete,
  };
}

/** Strict map parse. `expectedPath` must be the request path; a canonical
 *  mismatch, an unknown target discriminant, or any over-bound collection
 *  rejects the whole payload. */
export function normalizeFileExplorerMap(
  raw: unknown,
  expectedPath: string,
): FileExplorerMapResult | null {
  const rec = asRecord(raw);
  if (!rec) return null;
  const path = boundedText(rec.path, FILE_EXPLORER_MAP_MAX_PATH_CHARS);
  if (!path || !fileExplorerSamePath(path, expectedPath)) return null;
  let payloadJson: string | undefined;
  try {
    payloadJson = JSON.stringify(raw);
  } catch {
    payloadJson = undefined;
  }
  const payloadBytes = typeof payloadJson === 'string' ? utf8ByteLength(payloadJson) : null;
  if (payloadBytes == null || payloadBytes > FILE_EXPLORER_MAP_MAX_PAYLOAD_BYTES) return null;
  if (!Array.isArray(rec.nodes)) return null;
  if (!Array.isArray(rec.outgoing)) return null;
  if (!Array.isArray(rec.incoming)) return null;
  const nodes: FileExplorerMapNode[] = [];
  const seen = new Set<string>();
  for (const item of rec.nodes) {
    const node = normalizeMapNode(item);
    if (!node) return null;
    const id = fileExplorerCanonicalPath(node.path);
    if (!id || seen.has(id)) return null;
    seen.add(id);
    nodes.push(node);
  }
  if (!seen.has(fileExplorerCanonicalPath(path))) return null;
  const outgoing: FileExplorerMapEdge[] = [];
  for (const item of rec.outgoing) {
    const edge = normalizeMapEdge(item);
    if (!edge || !fileExplorerSamePath(edge.source, path)) return null;
    outgoing.push(edge);
  }
  const incoming: FileExplorerMapEdge[] = [];
  for (const item of rec.incoming) {
    const edge = normalizeMapEdge(item);
    if (!edge || edge.target.type !== 'file' || !fileExplorerSamePath(edge.target.path, path)) return null;
    incoming.push(edge);
  }
  const coverage = normalizeMapCoverage(rec.coverage);
  if (!coverage) return null;
  return { path, nodes, outgoing, incoming, coverage };
}

/** True only for the request that is still the rendered owner, path, and
 *  generation. A switched session, tab, or newer map request is stale. */
export function fileExplorerMapResponseCurrent(
  request: FileExplorerMapRequest,
  live: { ownerKey: string; path: string | null; generation: number },
): boolean {
  return live.ownerKey === request.ownerKey
    && live.path != null
    && fileExplorerSamePath(live.path, request.path)
    && live.generation === request.generation;
}

const MERMAID_LABEL_UNSAFE = /["#<>{}|\\\n\r]/g;

/** Mermaid node label: quotes, fences, and HTML-significant characters
 *  become spaces. The caller still wraps the result in double quotes. */
export function fileExplorerMapMermaidLabel(text: string): string {
  const cleaned = text.replace(MERMAID_LABEL_UNSAFE, ' ').replace(/\s+/g, ' ').trim();
  const bounded = cleaned.length > 160 ? `${cleaned.slice(0, 157)}...` : cleaned;
  return bounded || 'file';
}

export interface FileExplorerMapDiagram {
  /** ```mermaid fence body. Opaque ids only; labels are escaped. */
  source: string;
  /** Opaque flowchart id → canonical file path. External and unresolved
   *  targets are absent: they are not openable files. */
  filesById: Record<string, string>;
}

/** One-hop flowchart. Ids are `n0`… assigned in node order, never derived
 *  from a path, so a hostile path cannot become a click target or HTML. */
export function fileExplorerMapDiagram(map: FileExplorerMapResult): FileExplorerMapDiagram {
  const filesById: Record<string, string> = {};
  const idByPath = new Map<string, string>();
  const lines = ['flowchart LR'];
  map.nodes.forEach((node, index) => {
    const id = `n${index}`;
    const canonical = fileExplorerCanonicalPath(node.path);
    filesById[id] = node.path;
    idByPath.set(canonical, id);
    const marker = fileExplorerSamePath(node.path, map.path) ? 'selected ' : '';
    lines.push(`  ${id}["${fileExplorerMapMermaidLabel(`${marker}${fileExplorerBasename(node.path) || node.path}`)}"]`);
  });
  let external = 0;
  let unresolved = 0;
  const emit = (edge: FileExplorerMapEdge) => {
    const from = idByPath.get(fileExplorerCanonicalPath(edge.source));
    if (!from) return;
    let to: string;
    if (edge.target.type === 'file') {
      const target = idByPath.get(fileExplorerCanonicalPath(edge.target.path));
      if (!target) return;
      to = target;
    } else if (edge.target.type === 'external') {
      to = `e${external}`;
      external += 1;
      lines.push(`  ${to}["${fileExplorerMapMermaidLabel(`external ${edge.target.specifier}`)}"]`);
    } else {
      to = `u${unresolved}`;
      unresolved += 1;
      lines.push(`  ${to}["${fileExplorerMapMermaidLabel(`unresolved ${edge.target.reason}`)}"]`);
    }
    lines.push(`  ${from} -->|"${fileExplorerMapMermaidLabel(edge.kind)}"| ${to}`);
  };
  for (const edge of map.outgoing) emit(edge);
  for (const edge of map.incoming) emit(edge);
  return { source: lines.join('\n'), filesById };
}

export type FileExplorerTreeTone =
  | 'dir'
  | 'typescript'
  | 'javascript'
  | 'rust'
  | 'solidity'
  | 'python'
  | 'go'
  | 'markdown'
  | 'json'
  | 'css'
  | 'html'
  | 'image'
  | 'config'
  | 'shell'
  | 'document'
  | 'file';

const TREE_TONE_BY_EXT: Record<string, FileExplorerTreeTone> = {
  ts: 'typescript',
  tsx: 'typescript',
  mts: 'typescript',
  cts: 'typescript',
  js: 'javascript',
  jsx: 'javascript',
  mjs: 'javascript',
  cjs: 'javascript',
  rs: 'rust',
  py: 'python',
  pyi: 'python',
  go: 'go',
  sol: 'solidity',
  md: 'markdown',
  markdown: 'markdown',
  json: 'json',
  jsonc: 'json',
  css: 'css',
  scss: 'css',
  sass: 'css',
  less: 'css',
  html: 'html',
  htm: 'html',
  svg: 'image',
  png: 'image',
  jpg: 'image',
  jpeg: 'image',
  gif: 'image',
  webp: 'image',
  ico: 'image',
  toml: 'config',
  yaml: 'config',
  yml: 'config',
  ini: 'config',
  lock: 'config',
  sh: 'shell',
  bash: 'shell',
  zsh: 'shell',
  fish: 'shell',
  pdf: 'document',
  txt: 'document',
  rst: 'document',
};

/** exa-style type tone from the listing kind + basename. */
export function fileExplorerTreeTone(kind: FileExplorerEntryKind, name: string): FileExplorerTreeTone {
  if (kind === 'dir') return 'dir';
  const base = fileExplorerBasename(name).toLowerCase();
  if (base === 'dockerfile' || base === 'makefile' || base === 'cmakelists.txt') return 'config';
  const dot = base.lastIndexOf('.');
  if (dot <= 0 || dot === base.length - 1) return 'file';
  const extension = base.slice(dot + 1);
  return Object.hasOwn(TREE_TONE_BY_EXT, extension) ? TREE_TONE_BY_EXT[extension] : 'file';
}



export function fileExplorerNormalizeTreeQuery(query: string): string {
  return query.trim().toLowerCase();
}

export function fileExplorerTreeQueryMatches(query: string, name: string, path: string): boolean {
  const needle = fileExplorerNormalizeTreeQuery(query);
  if (!needle) return true;
  return name.toLowerCase().includes(needle) || path.toLowerCase().includes(needle);
}

/** True when a loaded listing under `dirPath` contains a name/path match. */
export function fileExplorerLoadedDescendantMatches(
  dirPath: string,
  query: string,
  dirs: Record<string, Pick<FileExplorerDirState, 'entries'>>,
): boolean {
  const needle = fileExplorerNormalizeTreeQuery(query);
  if (!needle) return true;
  const canonicalDir = fileExplorerCanonicalPath(dirPath);
  const prefix = canonicalDir ? `${canonicalDir}/` : '';
  for (const [path, state] of Object.entries(dirs)) {
    const canonical = fileExplorerCanonicalPath(path);
    const under = canonicalDir === ''
      ? true
      : canonical === canonicalDir || canonical.startsWith(prefix);
    if (!under) continue;
    for (const entry of state.entries) {
      if (fileExplorerTreeQueryMatches(query, entry.name, entry.path)) return true;
    }
  }
  return false;
}

export function fileExplorerStatusLetter(status: FileExplorerChangeStatus): string {
  return fileStatusLetter(status);
}



/* ------------------------------------------------------------------ *
 * Staged download — reuse GET /download/session/<uuid> + hidden <a>
 * (sessionExport.ts URL + filename-scrub rules; no .html suffix required).
 * ------------------------------------------------------------------ */

export function fileExplorerDownloadFileName(fileName: string): string {
  const bounded = fileName
    .replace(/[\u0000-\u001f\u007f\\/]+/g, '-')
    .trim()
    .slice(0, MAX_DOWNLOAD_FILENAME_LENGTH);
  return bounded || 'download';
}

export interface FileExplorerDownloadOptions {
  /** Exact verified origin (`https://host:port`). App uses location.origin;
   *  Chat uses the runner registry origin after scheme fallback. */
  downloadOrigin: string;
  document?: SessionExportDocument;
}

export function fileExplorerDownloadHref(downloadOrigin: string, downloadId: string): string {
  const origin = downloadOrigin.replace(/\/+$/, '');
  return sessionExportUrl(
    origin.startsWith('https:') ? 'https:' : 'http:',
    origin.replace(/^https?:\/\//, ''),
    downloadId,
  );
}

/** Stage-download GET is a hidden-anchor navigation to the exact origin. */
export function triggerFileExplorerDownload(
  downloadId: string,
  fileName: string,
  options: FileExplorerDownloadOptions,
): void {
  const url = fileExplorerDownloadHref(options.downloadOrigin, downloadId);
  const safeName = fileExplorerDownloadFileName(fileName);
  const doc = (options.document ?? document) as SessionExportDocument;
  const anchor = doc.createElement('a');
  anchor.href = url;
  anchor.download = safeName;
  anchor.hidden = true;
  doc.body.appendChild(anchor);
  anchor.click();
  anchor.remove();
}

export type FileExplorerDiffRowKind = 'addition' | 'deletion' | 'context' | 'meta' | 'hunk';

export interface FileExplorerDiffRow {
  kind: FileExplorerDiffRowKind;
  oldNo?: number;
  newNo?: number;
  prefix: string;
  text: string;
}

export interface FileExplorerDiffParseState {
  oldNo: number;
  newNo: number;
  oldRemaining: number;
  newRemaining: number;
  inHunk: boolean;
}

const FILE_EXPLORER_DIFF_INITIAL_STATE: FileExplorerDiffParseState = {
  oldNo: 0,
  newNo: 0,
  oldRemaining: 0,
  newRemaining: 0,
  inHunk: false,
};

function fileExplorerDiffHunk(line: string): FileExplorerDiffParseState | null {
  const match = /^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@/.exec(line);
  if (!match) return null;
  const oldRemaining = match[2] == null ? 1 : Number(match[2]);
  const newRemaining = match[4] == null ? 1 : Number(match[4]);
  return {
    oldNo: Number(match[1]),
    newNo: Number(match[3]),
    oldRemaining,
    newRemaining,
    inHunk: oldRemaining > 0 || newRemaining > 0,
  };
}

function fileExplorerDiffAdvance(state: FileExplorerDiffParseState, line: string): FileExplorerDiffParseState {
  const hunk = fileExplorerDiffHunk(line);
  if (hunk) return hunk;
  if (!state.inHunk || line.startsWith('\\ No newline at end of file')) return state;
  let { oldNo, newNo, oldRemaining, newRemaining } = state;
  if (line.startsWith('+')) {
    newNo += 1;
    newRemaining = Math.max(0, newRemaining - 1);
  } else if (line.startsWith('-')) {
    oldNo += 1;
    oldRemaining = Math.max(0, oldRemaining - 1);
  } else {
    oldNo += 1;
    newNo += 1;
    oldRemaining = Math.max(0, oldRemaining - 1);
    newRemaining = Math.max(0, newRemaining - 1);
  }
  return { oldNo, newNo, oldRemaining, newRemaining, inHunk: oldRemaining > 0 || newRemaining > 0 };
}

export function fileExplorerDiffStateAfter(
  lines: readonly string[],
  initial: FileExplorerDiffParseState = FILE_EXPLORER_DIFF_INITIAL_STATE,
): FileExplorerDiffParseState {
  let state = { ...initial };
  for (const line of lines) state = fileExplorerDiffAdvance(state, line);
  return state;
}


export function parseFileExplorerUnifiedDiff(
  lines: readonly string[],
  initial: FileExplorerDiffParseState = FILE_EXPLORER_DIFF_INITIAL_STATE,
): FileExplorerDiffRow[] {
  const rows: FileExplorerDiffRow[] = [];
  let state = { ...initial };
  for (const line of lines) {
    const hunk = fileExplorerDiffHunk(line);
    if (hunk) {
      rows.push({ kind: 'hunk', prefix: '', text: line });
      state = hunk;
      continue;
    }
    if (line.startsWith('\\ No newline at end of file')) {
      rows.push({ kind: 'meta', prefix: '\\', text: line.slice(1).trimStart() });
      continue;
    }
    if (!state.inHunk) {
      rows.push({ kind: 'meta', prefix: '', text: line });
      continue;
    }
    if (line.startsWith('+')) {
      rows.push({ kind: 'addition', newNo: state.newNo, prefix: '+', text: line.slice(1) });
    } else if (line.startsWith('-')) {
      rows.push({ kind: 'deletion', oldNo: state.oldNo, prefix: '-', text: line.slice(1) });
    } else {
      rows.push({ kind: 'context', oldNo: state.oldNo, newNo: state.newNo, prefix: ' ', text: line.startsWith(' ') ? line.slice(1) : line });
    }
    state = fileExplorerDiffAdvance(state, line);
  }
  return rows;
}

export interface FileExplorerDiffHunkGroup {
  header: FileExplorerDiffRow | null;
  rows: FileExplorerDiffRow[];
  startIndex: number;
}

export function groupFileExplorerDiffRows(
  rows: readonly FileExplorerDiffRow[],
): FileExplorerDiffHunkGroup[] {
  const groups: FileExplorerDiffHunkGroup[] = [];
  let current: FileExplorerDiffHunkGroup | null = null;
  let prelude: FileExplorerDiffHunkGroup | null = null;
  for (let index = 0; index < rows.length; index += 1) {
    const row = rows[index];
    if (row.kind === 'hunk') {
      if (prelude) {
        groups.push(prelude);
        prelude = null;
      }
      if (current) groups.push(current);
      current = { header: row, rows: [], startIndex: index };
      continue;
    }
    if (!current) {
      if (!prelude) prelude = { header: null, rows: [], startIndex: index };
      prelude.rows.push(row);
      continue;
    }
    current.rows.push(row);
  }
  if (prelude) groups.push(prelude);
  if (current) groups.push(current);
  return groups;
}
