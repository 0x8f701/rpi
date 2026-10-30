// Session panel — current session info plus the session lifecycle
// actions: new session, fork (from forkable user messages), clone, and
// rename. The saved-session catalog lives in the sidebar (SessionSidebar).
//
// Wire shapes mirror pi-cli/src/modes/rpc.rs (serde camelCase):
//   get_state -> RpcSessionState (now includes cwd)
//   get_session_stats -> SessionStats
//   get_fork_messages -> { messages: [{ entryId, text }], nextCursor?, truncated, total? }
//   new_session / fork / clone / set_session_name
// EVERY string derived from the model/catalog passes through safeText().

import { useCallback, useEffect, useRef, useState } from 'react';
import { safeText } from '../redact';
import { isTransportError, isTransportErrorMessage } from '../transportError';
import { PATH_COMPLETE_LIMIT, PathAutocomplete, type PathCompleteRequest, type PathCompleteResponse } from '../PathAutocomplete';
import { parseAuthoritativeNativeSessionRow } from '../sessionSwitch';

/** Max visible path length in the Session panel (mobile-safe bound). */
export const SESSION_PATH_DISPLAY_MAX = 72;

/**
 * Backend-authoritative path field from get_state (`cwd` / `sessionFile`).
 * Missing/empty/non-string -> Unavailable (never guess). Present values are
 * redacted via safeText and mid-truncated for mobile/narrow layouts; full
 * value stays available for the title/tooltip.
 */
export function formatSessionPath(value: unknown): { text: string; title: string; available: boolean } {
  if (typeof value !== 'string') {
    return { text: 'Unavailable', title: '', available: false };
  }
  const trimmed = value.trim();
  if (trimmed === '') {
    return { text: 'Unavailable', title: '', available: false };
  }
  const safe = safeText(trimmed);
  if (safe.length <= SESSION_PATH_DISPLAY_MAX) {
    return { text: safe, title: safe, available: true };
  }
  // Keep head + tail so both project root and file name remain readable.
  const head = Math.max(20, Math.floor(SESSION_PATH_DISPLAY_MAX * 0.45));
  const tail = SESSION_PATH_DISPLAY_MAX - head - 1;
  const text = `${safe.slice(0, head)}…${safe.slice(-tail)}`;
  return { text, title: safe, available: true };
}

/** Parent directory of a session file path (Unix or Windows separators). */
export function sessionDirectoryOf(sessionFile: unknown): string | null {
  if (typeof sessionFile !== 'string') return null;
  const trimmed = sessionFile.trim();
  if (trimmed === '') return null;
  const cleaned = trimmed.replace(/[\\/]+$/, '');
  const idx = Math.max(cleaned.lastIndexOf('/'), cleaned.lastIndexOf('\\'));
  if (idx <= 0) return null;
  return cleaned.slice(0, idx);
}

/** Project/workspace label: last segment of cwd, or null when unavailable. */
export function projectLabelOf(cwd: unknown): string | null {
  if (typeof cwd !== 'string') return null;
  const trimmed = cwd.trim();
  if (trimmed === '') return null;
  const cleaned = trimmed.replace(/[\\/]+$/, '');
  if (cleaned === '') return null;
  const idx = Math.max(cleaned.lastIndexOf('/'), cleaned.lastIndexOf('\\'));
  const base = (idx >= 0 ? cleaned.slice(idx + 1) : cleaned).trim();
  return base || null;
}

/**
 * Concise New-session ownership/storage hint from get_state fields only.
 * Never invents paths — missing fields become "Unavailable". The New-session
 * directory is ALWAYS an explicit user selection; the current project/cwd is
 * informational only and never prefills or bounds the target.
 */
export function newSessionLocationHint(cwd: unknown, sessionFile: unknown): string {
  const project = projectLabelOf(cwd);
  const cwdDisp = formatSessionPath(cwd);
  const fileDisp = formatSessionPath(sessionFile);
  const dirRaw = sessionDirectoryOf(sessionFile);
  const dirDisp = dirRaw ? formatSessionPath(dirRaw) : { text: 'Unavailable', title: '', available: false };
  const projectPart = project ? safeText(project) : 'Unavailable';
  const cwdPart = cwdDisp.available ? cwdDisp.text : 'Unavailable';
  const storePart = dirDisp.available
    ? dirDisp.text
    : fileDisp.available
      ? fileDisp.text
      : 'Unavailable';
  return `New session requires a working directory you explicitly select (any existing local directory). Current session: project ${projectPart} (cwd ${cwdPart}), stores under ${storePart}.`;
}

/**
 * Validate a `path_complete` reply into the frozen wire shape. Valid entries
 * are capped at PATH_COMPLETE_LIMIT — exceeding the cap yields exactly
 * PATH_COMPLETE_LIMIT entries with `truncated: true`. Anything malformed
 * (including error payloads) yields null so completion stays quiet and
 * manual input is never affected. Extra keys are ignored.
 */
export function parsePathCompleteResponse(raw: unknown): PathCompleteResponse | null {
  if (typeof raw !== 'object' || raw === null) return null;
  const r = raw as Record<string, unknown>;
  if (typeof r.base !== 'string' || typeof r.prefix !== 'string' || typeof r.truncated !== 'boolean' || !Array.isArray(r.entries)) {
    return null;
  }
  const entries: PathCompleteResponse['entries'] = [];
  for (const item of r.entries) {
    if (typeof item !== 'object' || item === null) continue;
    const e = item as Record<string, unknown>;
    if (typeof e.name !== 'string' || typeof e.path !== 'string' || (e.kind !== 'directory' && e.kind !== 'file')) continue;
    entries.push({ name: e.name, path: e.path, kind: e.kind });
  }
  if (entries.length > PATH_COMPLETE_LIMIT) {
    return { base: r.base, prefix: r.prefix, entries: entries.slice(0, PATH_COMPLETE_LIMIT), truncated: true };
  }
  return { base: r.base, prefix: r.prefix, entries, truncated: r.truncated };
}

/**
 * Workdir suggestions: one bounded `path_complete` probe through the
 * existing sendCommand socket, pinned to directory kind and the owning
 * session. Errors and malformed replies stay quiet (null) so completion
 * never blocks manual entry or the New action.
 */
export async function sendPathCompleteRequest(
  sendCommand: (command: Record<string, unknown>) => Promise<unknown>,
  sessionId: string | null | undefined,
  req: PathCompleteRequest,
): Promise<PathCompleteResponse | null> {
  try {
    const command: Record<string, unknown> = { ...req, kind: 'directory' };
    if (sessionId) command.sessionId = sessionId;
    return parsePathCompleteResponse(await sendCommand(command));
  } catch {
    return null;
  }
}

export interface SessionInfo {
  model?: { id?: string; name?: string; provider?: string } | null;
  thinkingLevel?: string;
  sessionFile?: string | null;
  sessionId?: string | null;
  sessionName?: string | null;
  cwd?: string;
  messageCount?: number;
  pendingMessageCount?: number;
  isStreaming?: boolean;
}

export interface SessionTokenStatsWire {
  input?: number;
  output?: number;
  cacheRead?: number;
  cacheWrite?: number;
  total?: number;
}

export interface SessionStatsWire {
  sessionFile?: string | null;
  sessionId?: string | null;
  userMessages?: number;
  assistantMessages?: number;
  toolCalls?: number;
  toolResults?: number;
  totalMessages?: number;
  tokens?: SessionTokenStatsWire;
  cost?: number;
  contextUsage?: { tokens?: number | null; contextWindow?: number; percent?: number | null } | null;
}

export interface SessionRowWire {
  source?: string;
  sessionId: string;
  name?: string | null;
  cwd?: string;
  displayTime?: string;
  modifiedEpoch?: number;
  summary?: string;
  path: string;
  size?: number;
  messageCount?: number | null;
  /** Catalog status: 'native', 'foreign', or 'already_imported' (a foreign
   *  row whose native rpi copy already exists on this machine). */
  status?: string;
  /** The native imported copy's session path — present on
   *  status:"already_imported" rows. Clicking such a row switches to the
   *  native session (never re-imports). */
  nativePath?: string;
  /** Authoritative runtime id of the native imported copy. */
  nativeSessionId?: string;
  /** Present (true) when the multi-session manager currently has this
   *  session loaded (session_list `loaded` overlay). loaded alone never
   *  proves readiness; a loaded row also carries runtimeIncarnation. */
  loaded?: boolean;
  /** Current resident runtime when loaded:true; omit/null otherwise. */
  runtimeIncarnation?: string | null;
  /** True when the row is an unnamed, tiny, native rpi session recorded under
   *  the OS temp root (historical test-harness shape). The sidebar hides
   *  these by default but keeps them searchable; loaded/active rows always
   *  stay visible. Recoverable view signal — never a deletion. */
  temporary?: boolean;
  /** Backend-classified external agent/subagent session. The sidebar groups
   * these under its default-collapsed Agent sessions subgroup. */
  agentSession?: boolean;
  /** Bootstrap/control-only agent row. Hidden by default, but searchable;
   * active or loaded rows are forced visible under Agent sessions. */
  bootstrapOnly?: boolean;
}

export function sessionRowKey(
  row: Pick<SessionRowWire, 'source' | 'sessionId' | 'path'>,
): string {
  return `${row.source || 'rpi'}::${row.sessionId}::${row.path}`;
}

/**
 * Validated authoritative lifecycle target from a SUCCESSFUL new_session /
 * fork / clone full-snapshot response (frozen cross-node contract; failures
 * stay source-owned rejections and never reach this parser). The wire `data`
 * must carry the target runtime identity, the target's authoritative state
 * snapshot, and the backend's authoritative `session` row (SessionRowWire
 * shaped, native): every identity field — data.sessionId, state.sessionId,
 * session.sessionId, runtimeIncarnation, cwd, path/sessionFile, and
 * source/status/loaded — must agree EXACTLY. ANY mismatch or missing field
 * throws before the caller mutates anything: no activation, no row
 * injection, and never a frontend-guessed row.
 */
export interface LifecycleTarget {
  sessionId: string;
  runtimeIncarnation: string;
  /** Target runtime's authoritative RpcSessionState snapshot. */
  state: Record<string, unknown>;
  /** Backend-authored native row for the new child, ready for the sidebar. */
  session: SessionRowWire;
}

export function parseLifecycleResult(result: unknown): LifecycleTarget {
  const d = (result ?? {}) as {
    sessionId?: unknown;
    runtimeIncarnation?: unknown;
    state?: unknown;
    session?: unknown;
  };
  if (typeof d.sessionId !== 'string' || d.sessionId === '') {
    throw new Error('lifecycle response is missing the target sessionId');
  }
  if (typeof d.runtimeIncarnation !== 'string' || d.runtimeIncarnation === '') {
    throw new Error('lifecycle response is missing the target runtime incarnation');
  }
  if (!d.state || typeof d.state !== 'object' || Array.isArray(d.state)) {
    throw new Error('lifecycle response is missing the target state snapshot');
  }
  const state = d.state as Record<string, unknown>;
  if (state.sessionId !== d.sessionId) {
    throw new Error('lifecycle target state sessionId disagrees with the response');
  }
  const stateCwd = state.cwd;
  if (typeof stateCwd !== 'string' || stateCwd === '') {
    throw new Error('lifecycle target state is missing cwd');
  }
  const stateSessionFile = state.sessionFile;
  if (typeof stateSessionFile !== 'string' || stateSessionFile === '') {
    throw new Error('lifecycle target state is missing sessionFile');
  }
  if (!d.session || typeof d.session !== 'object' || Array.isArray(d.session)) {
    throw new Error('lifecycle response is missing the authoritative session row');
  }
  return {
    sessionId: d.sessionId,
    runtimeIncarnation: d.runtimeIncarnation,
    state,
    session: parseAuthoritativeNativeSessionRow(d.session, {
      sessionId: d.sessionId,
      runtimeIncarnation: d.runtimeIncarnation,
      cwd: stateCwd,
      path: stateSessionFile,
    }),
  };
}

export function isLoadedCurrentSession(
  row: Pick<SessionRowWire, 'loaded' | 'sessionId'>,
  activeSessionId: string | null | undefined,
  hasLoadedActiveRow = false,
): boolean {
  if (row.sessionId !== activeSessionId) return false;
  return row.loaded === true || !hasLoadedActiveRow;
}

export interface ForkMessageWire {
  entryId: string;
  text: string;
}

export interface ForkMessagesPageWire {
  messages: ForkMessageWire[];
  nextCursor?: string;
  truncated: boolean;
  total?: number;
}

/** Parse a bounded `get_fork_messages` page. Extra keys are ignored. */
export function parseForkMessagesPage(response: unknown): ForkMessagesPageWire {
  if (!response || typeof response !== 'object') {
    return { messages: [], truncated: false };
  }
  const data = response as Record<string, unknown>;
  const messages: ForkMessageWire[] = [];
  if (Array.isArray(data.messages)) {
    const seen = new Set<string>();
    for (const item of data.messages) {
      if (!item || typeof item !== 'object') continue;
      const row = item as Record<string, unknown>;
      if (typeof row.entryId !== 'string' || typeof row.text !== 'string') continue;
      if (seen.has(row.entryId)) continue;
      seen.add(row.entryId);
      messages.push({ entryId: row.entryId, text: row.text });
    }
  }
  const nextCursor = typeof data.nextCursor === 'string' && data.nextCursor.length > 0
    ? data.nextCursor
    : undefined;
  const truncated = data.truncated === true || nextCursor !== undefined;
  const total = typeof data.total === 'number' && Number.isFinite(data.total)
    ? data.total
    : undefined;
  return { messages, nextCursor, truncated, total };
}

/** Append an older page, dropping duplicate entryIds already loaded. */
export function mergeForkMessagePages(
  current: ForkMessageWire[],
  incoming: ForkMessageWire[],
): ForkMessageWire[] {
  if (incoming.length === 0) return current;
  const seen = new Set(current.map((message) => message.entryId));
  const appended = incoming.filter((message) => {
    if (seen.has(message.entryId)) return false;
    seen.add(message.entryId);
    return true;
  });
  return appended.length === 0 ? current : current.concat(appended);
}

/** Fork controls stay enabled for a nonempty page or a sticky selected entry. */
export function forkControlsEnabled(
  messages: ForkMessageWire[],
  forkEntry: string,
): boolean {
  return messages.length > 0 || forkEntry.length > 0;
}

/** Sticky selection: only default when unset. A selected entry from an
 *  older page is retained even when the newest page omits it, and an empty
 *  page never clears it. */
export function nextForkEntrySelection(
  messages: ForkMessageWire[],
  currentForkEntry: string,
): string | null {
  if (currentForkEntry) return currentForkEntry;
  return messages[0]?.entryId ?? null;
}

/** A paged response belongs only to the exact session, cursor, component,
 * and request generation that issued it. */
export function ownsForkPageResponse(
  expectedSession: string,
  currentSession: string | null,
  expectedCursor: string,
  currentCursor: string | undefined,
  expectedRequest: number,
  currentRequest: number,
  expectedMount: number,
  currentMount: number,
): boolean {
  return expectedSession === currentSession &&
    expectedCursor === currentCursor &&
    expectedRequest === currentRequest &&
    expectedMount === currentMount;
}

/** Distance from the listbox bottom that triggers the next older fork page.
 *  Small enough to feel like the true end, far enough that the fetch lands
 *  before the user keeps scrolling. */
export const FORK_LIST_NEAR_BOTTOM_PX = 48;

/**
 * True when the bounded fork listbox has scrolled within `threshold` px of
 * its bottom and older pages remain, so the next page may load. Degenerate
 * geometry (zero height, non-finite) never triggers. Pure so the paging
 * regression exercises the exact handler rule.
 */
export function isNearForkListBottom(
  viewport: { scrollTop: number; clientHeight: number; scrollHeight: number },
  threshold: number = FORK_LIST_NEAR_BOTTOM_PX,
): boolean {
  if (!Number.isFinite(viewport.scrollTop) || !Number.isFinite(viewport.clientHeight) || !Number.isFinite(viewport.scrollHeight)) return false;
  if (viewport.scrollHeight <= 0 || viewport.clientHeight <= 0) return false;
  return viewport.scrollHeight - viewport.scrollTop - viewport.clientHeight <= threshold;
}

/**
 * True when the loaded rows do not fill the bounded listbox viewport (no
 * scrollbar), so near-bottom paging can never fire on its own and exactly
 * one automatic prefetch of the next older page is warranted. Pure.
 */
export function isForkListShort(
  viewport: { clientHeight: number; scrollHeight: number },
): boolean {
  if (!Number.isFinite(viewport.clientHeight) || !Number.isFinite(viewport.scrollHeight)) return false;
  if (viewport.clientHeight <= 0) return false;
  return viewport.scrollHeight <= viewport.clientHeight;
}

/** One display row of the fork listbox. `sticky` marks a synthetic row for
 *  a selected entry that came from an older page the current window omits
 *  (the Fork action stays anchored to it and it must stay visible). */
export interface ForkListOption {
  entryId: string;
  text: string;
  sticky: boolean;
}

/** Listbox display rows: loaded messages newest-first, plus a leading
 *  synthetic row when the sticky selection is outside the loaded window.
 *  The Fork action uses the selected entry even when no page window shows it. */
export function forkListOptions(
  messages: ForkMessageWire[],
  selectedEntry: string,
): ForkListOption[] {
  const rows: ForkListOption[] = messages.map((f) => ({ entryId: f.entryId, text: f.text, sticky: false }));
  if (selectedEntry !== '' && !messages.some((f) => f.entryId === selectedEntry)) {
    rows.unshift({ entryId: selectedEntry, text: selectedEntry, sticky: true });
  }
  return rows;
}

/** Option element id (stable index-based ids keep aria-activedescendant
 *  valid even when entryIds contain arbitrary characters). */
export function forkOptionId(index: number, sticky = false): string {
  return sticky ? 'session-fork-option-sticky' : `session-fork-option-${index}`;
}

interface SessionListWire {
  sessions: SessionRowWire[];
}

interface SessionPanelProps {
  sessionId: string;
  sendCommand: (command: Record<string, unknown>) => Promise<unknown>;
  /** Bounded wait for authoritative session bootstrap. The mount auto-load
   *  awaits this before `sendCommand` so raw WebSocket OPEN cannot race a
   *  session-scoped request; active lifecycle/rename actions bypass it. */
  waitForReady: () => Promise<void>;
  /** Re-fetch get_state into the app shell (header session name etc.). */
  refreshState: () => Promise<unknown>;
  /** Consume a lifecycle RPC result atomically: the successful
   *  new_session/fork/clone full snapshot `{sessionId,runtimeIncarnation,
   *  state,messages,session}` (frozen MultiSessionRuntimeManager contract).
   *  App validates the authoritative target + `session` row (App-owned
   *  parseLifecycleResult) and REJECTS any mismatch before mutating — the
   *  caller surfaces exactly one failure toast. */
  onLifecycleResult: (result: unknown) => Promise<unknown>;
  /** App-owned bottom-right toast. Every user-action, lifecycle, and local
   *  validation failure surfaces here exactly once; background panel loads
   *  stay silent and never render an in-panel status above Current session. */
  onToast: (message: string, error?: boolean) => void;
  /** Hand off to a NEW session: the backend renders the handoff envelope,
   *  creates a session seeded with the <handoff-context> doc, and answers
   *  with the subscribe-handoff-v1 envelope; App switches the active view.
   *  Returns false when a handoff is already in progress. Streaming
   *  refusals surface as exactly one error toast (App-owned). */
  onSessionHandoff: (customInstructions: string | null) => boolean;
  onClose: () => void;
}

function formatTokens(stats: SessionStatsWire | null): string {
  if (!stats || !stats.tokens) return '—';
  const t = stats.tokens;
  const parts: string[] = [];
  if (typeof t.input === 'number') parts.push(`${t.input} in`);
  if (typeof t.output === 'number') parts.push(`${t.output} out`);
  if (typeof t.cacheRead === 'number' && t.cacheRead > 0) parts.push(`${t.cacheRead} cache-read`);
  if (typeof t.cacheWrite === 'number' && t.cacheWrite > 0) parts.push(`${t.cacheWrite} cache-write`);
  return parts.length > 0 ? parts.join(' · ') : '—';
}


/**
 * Shared session-list refresh contract for the sidebar catalog.
 * A valid response replaces the catalog, including a valid empty catalog.
 * Transport and protocol failures preserve the known-good array by reference.
 */
export async function refreshSessionList(
  sendCommand: (command: Record<string, unknown>) => Promise<unknown>,
  current: SessionRowWire[],
): Promise<{ sessions: SessionRowWire[]; error: string | null; errorCause: unknown | null }> {
  try {
    const data = await sendCommand({ type: 'session_list', scope: 'all_projects' });
    const list = data as Partial<SessionListWire> | null;
    if (!Array.isArray(list?.sessions)) {
      return {
        sessions: current,
        error: 'session_list response is missing a sessions array',
        errorCause: null,
      };
    }
    return { sessions: list.sessions, error: null, errorCause: null };
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    return { sessions: current, error: message, errorCause: err };
  }
}

/**
 * True when an in-flight session catalog load may still commit state or
 * surface a visible error. A newer overlapping request, or a replaced
 * panel/sidebar instance (effect cleanup/restart or unmount), makes the
 * older load stale: its success, valid empty catalog, and error must not win.
 */
export function isLiveSessionLoad(
  requestGen: number,
  currentRequestGen: number,
  mountedGen: number,
  currentMountedGen: number,
): boolean {
  return requestGen === currentRequestGen && mountedGen === currentMountedGen;
}
function isSessionPanelTransportError(error: unknown): boolean {
  return isTransportError(error)
    || (typeof error === 'string' && isTransportErrorMessage(error));
}

/** True when a rejection still needs the panel's OWN toast. The shared
 *  `toastOwned` metadata marks failures the App/global layer already
 *  surfaced — the mid-session persistent outage toast, and rpc failures
 *  stamped by rpcFailureToastOwned — and the panel must never duplicate
 *  those. Immediate user-action transport failures (never-connected /
 *  CONNECTING hold, send failures before any outage toast) carry NO
 *  toastOwned and are the panel's to surface exactly once. Ownership is
 *  metadata-based, never message matching. */
 export function shouldToastSessionActionError(error: unknown): boolean {
   if (!error || typeof error !== 'object' || !('toastOwned' in error)) return true;
   return error.toastOwned !== true;
 }

export function SessionPanel({ sessionId, sendCommand, waitForReady, refreshState, onLifecycleResult, onToast, onSessionHandoff, onClose }: SessionPanelProps) {
  const [info, setInfo] = useState<SessionInfo | null>(null);
  const [stats, setStats] = useState<SessionStatsWire | null>(null);
  const [forkMessages, setForkMessages] = useState<ForkMessageWire[]>([]);
  const [forkNextCursor, setForkNextCursor] = useState<string | undefined>(undefined);
  const [rename, setRename] = useState('');
  const [forkEntry, setForkEntry] = useState('');
  const [forkPickerOpen, setForkPickerOpen] = useState(false);
  /** The New-session directory input. Manage New is fully explicit: the
   *  input starts empty on every panel mount (the App remounts the panel on
   *  a real session cut) and never prefills or inherits the current
   *  session's cwd. */
  const [newSessionCwd, setNewSessionCwd] = useState('');
  const [renaming, setRenaming] = useState(false);
  const renameInputRef = useRef<HTMLInputElement>(null);
  const [busy, setBusy] = useState(false);
  const busyRef = useRef(false);
  /** Older-page paging UI state: a request is in flight (single-flight
   *  mirror of forkLoadingOlderRef) and a latched business error (the
   *  loaded list is retained; scroll paging pauses and an explicit Retry
   *  re-issues the same cursor). */
  const [forkLoadingOlder, setForkLoadingOlder] = useState(false);
  const [forkOlderError, setForkOlderError] = useState(false);
  const mountedRef = useRef(true);
  const mountedGenRef = useRef(0);
  const requestGenRef = useRef(0);
  const forkEntryRef = useRef(forkEntry);
  const forkNextCursorRef = useRef<string | undefined>(undefined);
  const forkLoadingOlderRef = useRef(false);
  const forkOlderRequestRef = useRef(0);
  const forkListRef = useRef<HTMLDivElement>(null);
  const forkOlderErrorRef = useRef(false);
  /** Short-list prefetch budget: one automatic page pull per fresh page
   *  state (reset by every full load) so a non-overflowing window still
   *  reaches older pages exactly once without an unbounded fetch chain. */
  const forkShortPrefetchRef = useRef(false);
  forkOlderErrorRef.current = forkOlderError;
  const forkSessionRef = useRef<string | null>(null);
  const loadRef = useRef<(opts?: { waitForReady?: boolean }) => Promise<void>>(async () => {});
  forkEntryRef.current = forkEntry;

  /** Re-fetch state/stats/fork messages. Auto callers (mount effect)
   *  pass `waitForReady: true` (default) so raw WebSocket OPEN waits for
   *  authoritative session binding instead of surfacing a persistent load
   *  error. A gate timeout or transient post-gate disconnect is swallowed;
   *  reopening or manual refresh retries. Active lifecycle/rename actions pass
   *  `waitForReady: false` and fail fast. */
  const load = useCallback(async (opts?: { waitForReady?: boolean }): Promise<void> => {
    const wait = opts?.waitForReady !== false;
    const mountedGen = mountedGenRef.current;
    if (wait) {
      try {
        await waitForReady();
      } catch {
        return; // gate timeout during disconnect — silent; re-open re-mounts
      }
    }
    if (mountedGen !== mountedGenRef.current) return;
    const requestGen = ++requestGenRef.current;
    try {
      const [stateData, statsData, forkData] = await Promise.all([
        sendCommand({ type: 'get_state', sessionId }),
        sendCommand({ type: 'get_session_stats', sessionId }),
        sendCommand({ type: 'get_fork_messages', sessionId }),
      ]);
      if (!isLiveSessionLoad(requestGen, requestGenRef.current, mountedGen, mountedGenRef.current)) return;
      // These wire shapes are the rpi control plane's own RPC contract
      // (RpcSessionState / SessionStats / get_fork_messages).
      const info = stateData as SessionInfo;
      const stats = statsData as SessionStatsWire | null;
      const page = parseForkMessagesPage(forkData);
      setInfo(info || null);
      setStats(stats);
      setForkMessages(page.messages);
      setForkNextCursor(page.nextCursor);
      forkOlderRequestRef.current += 1;
      forkLoadingOlderRef.current = false;
      forkNextCursorRef.current = page.nextCursor;
      // A fresh page state gets its own short-list prefetch budget and
      // clears any latched older-page error and in-flight indicator.
      forkShortPrefetchRef.current = false;
      setForkLoadingOlder(false);
      setForkOlderError(false);
      // A different session resets the sticky selection; the same session
      // keeps the selected entry even when a page omits it.
      const sessionKey = typeof info?.sessionId === 'string' ? info.sessionId : null;
      const currentForkEntry = forkEntryRef.current;
      if (forkSessionRef.current !== sessionKey) {
        forkSessionRef.current = sessionKey;
        setForkEntry(page.messages[0]?.entryId ?? '');
      } else if (page.messages.length > 0 && (currentForkEntry === '' || page.messages.some((f) => f.entryId === currentForkEntry))) {
        setForkEntry(currentForkEntry === '' ? page.messages[0].entryId : currentForkEntry);
      }
    } catch (err) {
      // Transport loss during any background/manual refresh is transient. The
      // next poll, reconnect, or explicit refresh retries while the last
      // complete info/stats/fork snapshot remains intact.
      if (!isLiveSessionLoad(requestGen, requestGenRef.current, mountedGen, mountedGenRef.current)) return;
      if (isSessionPanelTransportError(err)) return;
      throw err;
    }
  }, [sendCommand, sessionId, waitForReady]);
  loadRef.current = load;

  useEffect(() => {
    const instanceGen = ++mountedGenRef.current;
    mountedRef.current = true;
    loadRef.current().catch(() => {
      // Background panel load failure: silent by contract. The last-good
      // panel snapshot is preserved and the next poll, reconnect, or manual
      // refresh retries; no in-panel status is ever rendered above Current
      // session.
    });
    return () => {
      mountedRef.current = false;
      if (mountedGenRef.current === instanceGen) mountedGenRef.current += 1;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  /** Single toast owner for panel action failures. A rejection the App
   *  already surfaced (`toastOwned`: the mid-session persistent outage
   *  toast, or an rpcFailureToastOwned business failure) stays silent.
   *  Every other user-action rejection — transport family included — is
   *  toasted here exactly once; background loads never reach this gate. */
  const reportFailure = useCallback((error: unknown, label: string) => {
    if (!shouldToastSessionActionError(error)) return;
    onToast(`${label} failed: ${safeText(error instanceof Error ? error.message : String(error))}`, true);
  }, [onToast]);

  const run = useCallback(
    async (action: () => Promise<unknown>, label: string) => {
      if (busyRef.current) return;
      busyRef.current = true;
      setBusy(true);
      try {
        await action();
        // Post-success refreshes are best-effort background loads: a
        // transport loss or RPC failure must never re-present a committed
        // action as failed. The last-good panel snapshot is preserved and
        // the next poll or explicit refresh retries.
        await refreshState().catch(() => {});
        await load({ waitForReady: false }).catch(() => {});
      } catch (err) {
        reportFailure(err, label);
      } finally {
        busyRef.current = false;
        if (mountedRef.current) setBusy(false);
      }
    },
    [load, refreshState, reportFailure]
  );

  /** Lifecycle actions (new/clone/fork) resolve with the TARGET session's
   *  full snapshot: consume it ATOMICALLY through onLifecycleResult (which
   *  validates the frozen contract, switches the active view + transcript,
   *  and injects the authoritative `session` row into the sidebar) instead
   *  of refreshState — re-querying get_state with the SOURCE sessionId
   *  would snap the view back. NO post-lifecycle panel load here: the panel
   *  is keyed by the active session id, so the lifecycle cut unmounts THIS
   *  panel and mounts the keyed TARGET panel, whose mount effect owns its
   *  own load. A load from this (already stale) instance would re-query
   *  with its render-captured source sessionId and race the target's
   *  authoritative mount load. */
  const runLifecycle = useCallback(
    async (action: () => Promise<unknown>, label: string) => {
      if (busyRef.current) return;
      busyRef.current = true;
      setBusy(true);
      try {
        const result = await action();
        await onLifecycleResult(result);
      } catch (err) {
        reportFailure(err, label);
      } finally {
        busyRef.current = false;
        if (mountedRef.current) setBusy(false);
      }
    },
    [onLifecycleResult, reportFailure]
  );

  /** Scroll/prefetch-driven older-page load. Single-flight
   *  (forkLoadingOlderRef), exact stale guards (session, cursor, request and
   *  mount generation), merge with dedupe, and a terminal stop when the
   *  last page carries no cursor. A business failure LATCHES
   *  forkOlderError: the loaded list is retained untouched, near-bottom
   *  scrolling pauses (no toast spam from an unusable list), and the
   *  explicit Retry re-issues the SAME cursor. */
  const loadOlder = useCallback(async (): Promise<void> => {
    const cursor = forkNextCursorRef.current;
    const sessionKey = forkSessionRef.current;
    if (!cursor || !sessionKey || forkLoadingOlderRef.current) return;
    const mountedGen = mountedGenRef.current;
    const requestGen = ++forkOlderRequestRef.current;
    forkLoadingOlderRef.current = true;
    setForkLoadingOlder(true);
    try {
      const data = await sendCommand({ type: 'get_fork_messages', cursor, sessionId });
      const page = parseForkMessagesPage(data);
      if (!mountedRef.current || !ownsForkPageResponse(
        sessionKey,
        forkSessionRef.current,
        cursor,
        forkNextCursorRef.current,
        requestGen,
        forkOlderRequestRef.current,
        mountedGen,
        mountedGenRef.current,
      )) return;
      setForkMessages((old) => mergeForkMessagePages(old, page.messages));
      setForkNextCursor(page.nextCursor);
      forkNextCursorRef.current = page.nextCursor;
      setForkOlderError(false);
    } catch (err) {
      if (mountedRef.current && ownsForkPageResponse(
        sessionKey,
        forkSessionRef.current,
        cursor,
        forkNextCursorRef.current,
        requestGen,
        forkOlderRequestRef.current,
        mountedGen,
        mountedGenRef.current,
      )) {
        setForkOlderError(true);
        reportFailure(err, 'load more');
      }
    } finally {
      if (requestGen === forkOlderRequestRef.current) {
        forkLoadingOlderRef.current = false;
        if (mountedRef.current) setForkLoadingOlder(false);
      }
    }
  }, [sendCommand, sessionId]);

  const onNew = () => {
    // Manage New is explicit-only: the trimmed input IS the selected
    // directory and always travels on the wire. An empty input never
    // submits (the button is disabled and Enter is a no-op).
    const value = newSessionCwd.trim();
    if (value === '') return;
    runLifecycle(() => sendCommand({ type: 'new_session', cwd: value }), 'new session');
  };
  const sendPathComplete = useCallback(
    (req: PathCompleteRequest) => sendPathCompleteRequest(sendCommand, sessionId, req),
    [sendCommand, sessionId],
  );
  // Clone/Fork are CURRENT-runtime actions and carry NO explicit owner: the
  // panel's `sessionId` prop is captured at render time and goes stale the
  // instant a lifecycle cut (e.g. New B while panel A is mounted) swaps the
  // active session before this instance unmounts. Omitting it lets
  // App.sendCommand route the frame through the SYNCHRONOUS
  // displayState.commandTarget — always the session the user is actually
  // looking at. Saved-row fork/clone (SessionSidebar) keep sessionPath.
  const onClone = () => runLifecycle(() => sendCommand({ type: 'clone' }), 'clone');
  const onFork = () => {
    if (!forkEntry) {
      onToast('no forkable message selected', true);
      return;
    }
    setForkPickerOpen(false);
    runLifecycle(() => sendCommand({ type: 'fork', entryId: forkEntry }), 'fork');
  };
  const onRename = () => {
    // Read the live DOM value (not the state) so a freshly typed name is
    // committed even if the controlled-input state update hasn't rendered.
    const name = (renameInputRef.current?.value ?? rename).trim();
    if (!name) {
      onToast('session name cannot be empty', true);
      return;
    }
    setRenaming(true);
    run(() => sendCommand({ type: 'set_session_name', name, sessionId }), 'rename').finally(() => {
      if (mountedRef.current) setRenaming(false);
    });
  };

  /** Bounded listbox display rows: loaded messages newest-first, plus a
   *  synthetic leading row when the sticky selection comes from an older
   *  page the current window omits (the Fork action stays anchored to it
   *  and the selection must remain visible/selectable). */
  const forkRows = forkListOptions(forkMessages, forkEntry);
  const activeOptionIndex = forkRows.findIndex((row) => row.entryId === forkEntry);
  const forkListEnabled = forkControlsEnabled(forkMessages, forkEntry) && !busy;

  /** Near-bottom scroll paging. A latched older-page error pauses the
   *  scroll trigger (the explicit Retry is the bounded retry path); the
   *  geometry decision is pure and unit-tested. */
  const onForkListScroll = useCallback((): void => {
    if (forkOlderErrorRef.current) return;
    const el = forkListRef.current;
    if (!el || !isNearForkListBottom(el)) return;
    void loadOlder();
  }, [loadOlder]);

  /** Keyboard selection model for the listbox (single-select, wrapping,
   *  Home/End); Enter activates the selected entry (forks). A no-selection
   *  listbox ignores keys; ArrowDown/Up anchor to the first/last row. */
  const onForkListKeyDown = (event: { key: string; preventDefault: () => void }): void => {
    if (!forkListEnabled || forkRows.length === 0) return;
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault();
      const current = forkRows.findIndex((row) => row.entryId === forkEntry);
      const delta = event.key === 'ArrowDown' ? 1 : -1;
      const start = current < 0
        ? (delta === 1 ? 0 : forkRows.length - 1)
        : (current + delta + forkRows.length) % forkRows.length;
      setForkEntry(forkRows[start].entryId);
    } else if (event.key === 'Home') {
      event.preventDefault();
      setForkEntry(forkRows[0].entryId);
    } else if (event.key === 'End') {
      event.preventDefault();
      setForkEntry(forkRows[forkRows.length - 1].entryId);
    } else if (event.key === 'Enter') {
      event.preventDefault();
      onFork();
    }
  };

  // Short-list prefetch: a window that does not overflow the bounded
  // viewport can never produce a near-bottom scroll, so pull exactly one
  // more page once per fresh page state. The budget flag is consumed when
  // the prefetch is ISSUED (a failed pull latches the Retry row instead of
  // auto-retrying) and re-armed only by the next full page load.
  useEffect(() => {
    const el = forkListRef.current;
    if (!el || forkShortPrefetchRef.current) return;
    if (!isForkListShort(el)) return;
    forkShortPrefetchRef.current = true;
    void loadOlder();
  }, [forkMessages, forkNextCursor, loadOlder]);

  // Keep the selected option visible after merges and keyboard moves; the
  // mini-DOM test harness omits scrollIntoView, so the call is optional.
  useEffect(() => {
    const el = forkListRef.current;
    if (!el || activeOptionIndex < 0) return;
    const option = el.querySelector(`#${forkOptionId(activeOptionIndex)}`);
    if (option && typeof option.scrollIntoView === 'function') {
      option.scrollIntoView({ block: 'nearest' });
    }
  }, [activeOptionIndex, forkMessages]);

  const tokens = formatTokens(stats);
  const contextPct =
    stats?.contextUsage?.percent != null ? `${Math.round(stats.contextUsage.percent)}%` : '—';

  return (
    <section id="session-panel" className="panel" aria-label="Session panel">
      <div className="panel__head">
        <span className="panel__title">Session</span>
        <span className="panel__hint">
          lifecycle: new · fork · clone · rename
        </span>
        <button
          id="session-refresh-btn"
          type="button"
          onClick={() => {
            load().catch((err: unknown) => {
              if (mountedRef.current) {
                reportFailure(err, 'refresh');
              }
            });
          }}
          disabled={busy}
        >
          Refresh
        </button>
        <button id="session-close-btn" type="button" className="panel-close" onClick={onClose} title="Close panel" aria-label="Close session panel">
          {'\u00D7'}
        </button>
      </div>

      <div className="panel__body">
        <div className="session-info">
          <h3 className="panel__subhead">Current session</h3>
          <dl className="session-info__grid">
            <dt>Name</dt>
            <dd data-testid="session-name-value">{info?.sessionName ? safeText(info.sessionName) : '—'}</dd>
            <dt>Session id</dt>
            <dd>{info?.sessionId ? safeText(info.sessionId) : '—'}</dd>
            <dt>Model</dt>
            <dd>
              {info?.model?.id
                ? safeText(`${info.model.provider || '?'}/${info.model.id}`)
                : '—'}
            </dd>
            <dt>Thinking</dt>
            <dd>{info?.thinkingLevel ? safeText(info.thinkingLevel) : '—'}</dd>
            <dt>Project</dt>
            <dd data-testid="session-project-value">
              {(() => {
                const label = projectLabelOf(info?.cwd);
                return label ? safeText(label) : 'Unavailable';
              })()}
            </dd>
            <dt>Working directory</dt>
            <dd
              data-testid="session-cwd-value"
              className="session-info__path"
              title={formatSessionPath(info?.cwd).title}
            >
              {formatSessionPath(info?.cwd).text}
            </dd>
            <dt>Session file</dt>
            <dd
              data-testid="session-file-value"
              className="session-info__path"
              title={formatSessionPath(info?.sessionFile).title}
            >
              {formatSessionPath(info?.sessionFile).text}
            </dd>
            <dt>Messages</dt>
            <dd>
              {typeof info?.messageCount === 'number' ? info.messageCount : '—'}
              {typeof info?.pendingMessageCount === 'number' && info.pendingMessageCount > 0
                ? ` (${info.pendingMessageCount} pending)`
                : ''}
            </dd>
            <dt>Tokens</dt>
            <dd>{safeText(tokens)}</dd>
            <dt>Context</dt>
            <dd>{safeText(contextPct)}</dd>
          </dl>

          <h3 className="panel__subhead">New session directory</h3>
          <p
            className="session-info__new-hint"
            data-testid="session-new-location-hint"
            title={(() => {
              const cwdFull = formatSessionPath(info?.cwd);
              const fileFull = formatSessionPath(info?.sessionFile);
              const dirRaw = sessionDirectoryOf(info?.sessionFile);
              const dirFull = dirRaw ? formatSessionPath(dirRaw) : null;
              const parts: string[] = [];
              if (cwdFull.available) parts.push(`cwd: ${cwdFull.title}`);
              if (dirFull?.available) parts.push(`session dir: ${dirFull.title}`);
              else if (fileFull.available) parts.push(`session file: ${fileFull.title}`);
              return parts.join(' · ');
            })()}
          >
            {newSessionLocationHint(info?.cwd, info?.sessionFile)}
          </p>

          <div className="session-actions">
            <span className="session-rename session-fork">
              <PathAutocomplete
                id="session-new-cwd-input"
                value={newSessionCwd}
                onChange={setNewSessionCwd}
                sendComplete={sendPathComplete}
                kind="directory"
                placeholder="Select a working directory…"
                ariaLabel="New session working directory"
                disabled={busy}
                autoFocus
                onEnter={onNew}
              />
              <button
                id="session-new-btn"
                type="button"
                onClick={onNew}
                disabled={busy || newSessionCwd.trim() === ''}
                title="Start a fresh session in the explicitly selected directory (new_session)"
              >
                New session
              </button>
            </span>
            <span className="session-secondary">
              <button id="session-clone-btn" type="button" onClick={onClone} disabled={busy} title="Clone the current branch (clone)">
                Clone
              </button>
              <button
                id="session-handoff-btn"
                type="button"
                disabled={busy}
                title="Hand off to a new session: copies this session's handoff context into a fresh session and switches to it"
                onClick={() => { onSessionHandoff(null); }}
              >
                Hand off…
              </button>
            </span>
            <span className="session-fork">
              <button
                id="session-fork-picker-btn"
                type="button"
                aria-expanded={forkPickerOpen}
                disabled={forkRows.length === 0 || busy}
                onClick={() => setForkPickerOpen((open) => !open)}
              >
                {forkRows.length === 0
                  ? 'No forkable messages'
                  : safeText((forkRows.find((row) => row.entryId === forkEntry)?.text || forkEntry).slice(0, 48))}
              </button>
              <div
                  className="session-fork__list"
                  hidden={!forkPickerOpen}
                  id="session-fork-list"
                  ref={forkListRef}
                  role={forkPickerOpen ? 'listbox' : undefined}
                  aria-label={forkPickerOpen ? 'Fork from a previous user message' : undefined}
                  aria-activedescendant={forkPickerOpen && activeOptionIndex >= 0 ? forkOptionId(activeOptionIndex, forkRows[activeOptionIndex]?.sticky === true) : undefined}
                  tabIndex={forkPickerOpen && forkListEnabled ? 0 : -1}
                  onScroll={onForkListScroll}
                  onKeyDown={onForkListKeyDown}
                >
                  {forkRows.map((row, index) => (
                    <div
                      key={row.entryId}
                      id={forkOptionId(index, row.sticky)}
                      role="option"
                      aria-selected={row.entryId === forkEntry}
                      className={row.entryId === forkEntry ? 'session-fork__option session-fork__option--selected' : 'session-fork__option'}
                      onClick={() => {
                        setForkEntry(row.entryId);
                        setForkPickerOpen(false);
                      }}
                    >
                      {safeText(row.text.length > 60 ? `${row.text.slice(0, 60)}…` : row.text)}
                    </div>
                  ))}
                {forkLoadingOlder && <div className="session-fork__loading" role="presentation">Loading…</div>}
              </div>
              {forkOlderError && (
                <div className="session-fork__error" role="status">
                  <span>Older messages failed to load</span>
                  <button id="session-fork-retry-btn" type="button" onClick={() => void loadOlder()} disabled={busy}>Retry</button>
                </div>
              )}
              <button id="session-fork-btn" type="button" onClick={onFork} disabled={!forkControlsEnabled(forkMessages, forkEntry) || busy}>
                Fork
              </button>
            </span>
            <span className="session-rename">
              <input
                id="session-rename-input"
                ref={renameInputRef}
                type="text"
                value={rename}
                placeholder="Rename this session…"
                onChange={(e) => setRename(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === 'Enter') onRename();
                }}
                disabled={busy}
              />
              <button id="session-rename-btn" type="button" onClick={onRename} disabled={busy || renaming}>
                Rename
              </button>
            </span>
          </div>
        </div>

      </div>
    </section>
  );
}
