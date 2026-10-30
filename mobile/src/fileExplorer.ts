export const FILE_EXPLORER_DEFAULT_MAX_LINES = 400;
export const FILE_EXPLORER_HARD_MAX_LINES = 2000;
export const FILE_EXPLORER_MAX_PREVIEW_BYTES = 256 * 1024;
export const FILE_EXPLORER_TOO_LARGE_MESSAGE = 'File is too large to display.';

export type FileExplorerEntryKind = 'file' | 'dir';
export type FileExplorerChangeStatus = 'modified' | 'added' | 'deleted' | 'renamed';

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
  truncated: boolean;
}

export interface FileExplorerDiffResult {
  path: string;
  lines: string[];
  nextCursor?: number;
  totalLines: number;
  truncated: boolean;
}

/** One porcelain line. `commit` is null only for the zero oid (uncommitted). */
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

export interface FileExplorerBlameRequest {
  path: string;
  startLine: number;
  endLine: number;
}

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

export function fileExplorerIsOversized(sizeBytes: number | undefined): boolean {
  return sizeBytes != null && sizeBytes > FILE_EXPLORER_MAX_PREVIEW_BYTES;
}

export function clampMaxLines(maxLines: number): number {
  if (!Number.isFinite(maxLines)) return FILE_EXPLORER_DEFAULT_MAX_LINES;
  const n = Math.floor(maxLines);
  if (n < 1) return FILE_EXPLORER_DEFAULT_MAX_LINES;
  return Math.min(n, FILE_EXPLORER_HARD_MAX_LINES);
}

export function parentExplorerPath(path: string): string | null {
  const trimmed = path.replace(/\\/g, '/').replace(/\/+$/, '');
  if (!trimmed || trimmed === '.') return null;
  const slash = trimmed.lastIndexOf('/');
  if (slash <= 0) return '';
  return trimmed.slice(0, slash);
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

export function normalizeChanges(raw: unknown): FileExplorerChangesResult | null {
  const rec = asRecord(raw);
  if (!rec || !Array.isArray(rec.files)) return null;
  const files: FileExplorerChange[] = [];
  for (const item of rec.files) {
    const row = asRecord(item);
    if (!row) continue;
    const path = stringField(row.path);
    const status = stringField(row.status);
    if (!path || !CHANGE_STATUSES[status]) continue;
    const change: FileExplorerChange = { path, status: status as FileExplorerChangeStatus };
    const oldPath = optionalString(row.oldPath);
    if (oldPath) change.oldPath = oldPath;
    files.push(change);
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

/** Unix seconds Date can format: safe integer, at most 8.64e12. */
const MAX_BLAME_TIME_SECONDS = 8.64e12;

function exactUint(value: unknown): number | null {
  if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 0) return null;
  return value;
}

function blameTime(value: unknown): number | null {
  const time = exactUint(value);
  if (time == null || time > MAX_BLAME_TIME_SECONDS) return null;
  return time;
}

/** Full git oid, or null for the all-zero uncommitted id. Anything else fails. */
function blameCommit(value: unknown): string | null | undefined {
  if (value === null) return null;
  if (typeof value !== 'string' || value.length === 0) return undefined;
  if (/^0+$/.test(value)) return null;
  if (!/^[0-9a-fA-F]{4,64}$/.test(value)) return undefined;
  return value;
}

/**
 * Fail closed unless `raw` is exactly the requested path and inclusive
 * 1-based range, with one ordered line per requested current line.
 * A short, long, gapped, or foreign payload is null — never a partial.
 */
export function normalizeBlame(
  raw: unknown,
  request: FileExplorerBlameRequest,
): FileExplorerBlameResult | null {
  const rec = asRecord(raw);
  if (!rec || rec.path !== request.path || !Array.isArray(rec.lines)) return null;
  if (!Number.isInteger(request.startLine) || !Number.isInteger(request.endLine)) return null;
  if (request.startLine < 1 || request.endLine < request.startLine) return null;
  const count = request.endLine - request.startLine + 1;
  if (count > FILE_EXPLORER_HARD_MAX_LINES || rec.lines.length !== count) return null;
  const lines: FileExplorerBlameLine[] = [];
  for (let index = 0; index < count; index += 1) {
    const row = asRecord(rec.lines[index]);
    if (!row) return null;
    const commit = blameCommit(row.commit);
    const time = blameTime(row.time);
    const originalLine = exactUint(row.originalLine);
    const currentLine = exactUint(row.currentLine);
    if (commit === undefined || typeof row.author !== 'string' || typeof row.text !== 'string') return null;
    if (time == null || originalLine == null || originalLine < 1 || currentLine == null) return null;
    if (currentLine !== request.startLine + index) return null;
    lines.push({
      commit,
      author: row.author,
      time,
      originalLine,
      currentLine,
      text: row.text,
    });
  }
  return { path: request.path, lines };
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
