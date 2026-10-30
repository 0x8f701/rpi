import type { EventFrame, RpcResponse } from '../types';

export const RUNNER_AUTH_PROTOCOL_PREFIX = 'rpi-auth.';
export const RUNNER_COMMAND_TIMEOUT_MS = 15_000;
export const TMUX_TERMINAL_RESIZE_TIMEOUT_MS = 8_000;
export const RUNNER_RECONNECT_INITIAL_MS = 250;
export const RUNNER_RECONNECT_MAX_MS = 15_000;

export type RunnerCommand =
  | { type: 'get_state' }
  | { type: 'get_messages' }
  | { type: 'session_list'; scope: 'current' | 'all_projects' }
  | { type: 'new_session'; cwd: string }
  | { type: 'set_session_name'; name: string; sessionPath: string }
  | { type: 'session_delete'; sessionPath: string }
  | { type: 'switch_session'; sessionPath: string }
  // /web composer prompt (App.tsx:3928) and rpc.rs Prompt.message — not `text`.
  | { type: 'prompt'; message: string }
  | { type: 'steer'; message: string }
  | { type: 'abort' }
  | { type: 'queue_send_now'; sessionId: string }
  | { type: 'queue_cancel'; scope: 'steering'; index: number; sessionId: string }
  | { type: 'get_commands' }
  | { type: 'goal_create'; objective: string; tokenBudget?: number; activate?: boolean }
  | { type: 'goal_pause' }
  | { type: 'goal_resume'; activate?: boolean }
  | { type: 'goal_complete' }
  | { type: 'goal_drop' }
  | { type: 'goal_pin'; text: string }
  | { type: 'goal_unpin'; index: number }
  | { type: 'goal_get' }
  | { type: 'goal_journal' }
  | { type: 'job_list' }
  | { type: 'file_explorer_list'; path?: string; sessionId?: string }
  | { type: 'file_explorer_search'; query: string; sessionId?: string }
  | { type: 'file_explorer_read'; path: string; cursor?: number; maxLines?: number; sessionId?: string }
  | { type: 'file_explorer_changes'; sessionId?: string }
  | { type: 'file_explorer_diff'; path: string; cursor?: number; maxLines?: number; sessionId?: string }
  | { type: 'file_explorer_stage_download'; path: string; sessionId?: string }
  // PathAutocomplete.tsx — one parent-directory listing per request; the
  // SessionPanel/ChatApp sendComplete closures supply `sessionId` so relative
  // paths resolve against that runner session's cwd.
  | { type: 'path_complete'; path: string; kind: 'directory' | 'file' | 'any'; limit?: number; sessionId?: string }
  // Tmux panel (TmuxPanel.tsx). The three snapshot/send shapes are frozen
  // with the Rust runner side; the response parsers below reject any drift
  // (return null) so the panel only ever renders a validated payload.
  | { type: 'tmux_list_sessions' }
  | { type: 'tmux_pane_snapshot'; session: string; lines?: number }
  | { type: 'tmux_send_keys'; session: string; text?: string; key?: TmuxSendKey }
  // Session lifecycle (TmuxPreviewBoard create/delete controls).
  | { type: 'tmux_session_create'; session: string }
  | { type: 'tmux_session_delete'; session: string }
  // Interactive PTY overlay (TmuxTerminal.tsx + docs/tmux-web-terminal-design.md).
  | { type: 'tmux_terminal_attach'; session: string; cols: number; rows: number; fullHistory?: boolean; cachedHistoryKey?: string }
  | { type: 'tmux_terminal_input'; terminalId: string; data: string }
  | { type: 'tmux_terminal_resize'; terminalId: string; cols: number; rows: number; fitWindow?: boolean }
  | { type: 'tmux_terminal_detach'; terminalId: string };

/** Canonical success row of one full-snapshot lifecycle command
 *  (`new_session` / `switch_session` on the Chat wire). Shared by
 *  runners.ts (settlement ownership gate) and mapping.ts (identity
 *  extraction) so the two can never drift. */
export interface LifecycleTargetRow {
  sessionId: string;
  path: string;
  cwd: string;
}

/** One shared FULL lifecycle row validator (backend contract:
 *  session_runtime_manager.rs snapshot_response + lifecycle_target_session_row).
 *  A success response is accepted only when every canonical field agrees:
 *  exact target id on the envelope, data, state, and row; the exact nonempty
 *  runtimeIncarnation shared by data and the authoritative row; row.path
 *  equal to state.sessionFile; row.cwd equal to state.cwd AND the requested
 *  directory (when the caller supplies one — new_session carries the cwd,
 *  switch_session does not); row.status exactly 'native'; row.loaded exactly
 *  true; and the row's native source EXACTLY 'rpi' — the backend always
 *  stamps lifecycle targets `source: "rpi"` (lifecycle_target_session_row),
 *  so a missing, null, empty, non-string, or foreign source fails closed.
 *  When `expected` is supplied (the stored mapping identity), the row must
 *  match it exactly — a switch back to a different live row fails closed.
 *  Any drift, absence, or empty field returns null. */
export function lifecycleTargetRow(
  response: RpcResponse,
  command: 'new_session' | 'switch_session',
  requestCwd: string | null,
  expected?: { sessionId: string; path: string },
): LifecycleTargetRow | null {
  if (!response.success || response.command !== command) return null;
  const envelope = readNonemptyString(response.sessionId);
  if (envelope === null) return null;
  if (!response.data || typeof response.data !== 'object') return null;
  const data = response.data as Record<string, unknown>;
  if (data.sessionId !== envelope) return null;
  const incarnation = readNonemptyString(data.runtimeIncarnation);
  if (incarnation === null) return null;
  const state = data.state;
  if (!state || typeof state !== 'object') return null;
  const stateValue = state as Record<string, unknown>;
  if (stateValue.sessionId !== envelope) return null;
  const sessionFile = readNonemptyString(stateValue.sessionFile);
  if (sessionFile === null) return null;
  if (typeof stateValue.cwd !== 'string' || stateValue.cwd === '') return null;
  const row = data.session;
  if (!row || typeof row !== 'object') return null;
  const rowValue = row as Record<string, unknown>;
  if (rowValue.sessionId !== envelope || rowValue.runtimeIncarnation !== data.runtimeIncarnation) {
    return null;
  }
  const path = readNonemptyString(rowValue.path);
  if (path === null || path !== sessionFile) return null;
  if (typeof rowValue.cwd !== 'string' || rowValue.cwd !== stateValue.cwd) return null;
  if (requestCwd !== null && (stateValue.cwd !== requestCwd || rowValue.cwd !== requestCwd)) return null;
  if (rowValue.status !== 'native') return null;
  if (rowValue.loaded !== true) return null;
  // The canonical native source is UNCONDITIONAL: the backend stamps every
  // lifecycle target row `source: "rpi"` (lifecycle_target_session_row), so
  // a missing, null, empty, non-string, or foreign provenance is a drift
  // that fails closed — the row is never accepted without it.
  if (rowValue.source !== 'rpi') return null;
  if (expected && (rowValue.sessionId !== expected.sessionId || path !== expected.path)) {
    return null;
  }
  return { sessionId: envelope, path, cwd: rowValue.cwd };
}

export interface RunnerRuntimeIdentity {
  sessionId: string;
  runtimeIncarnation: string;
}

export interface RunnerProbe {
  sessionId: string | null;
  runtimeIncarnation: string | null;
  cwd: string;
  sessionCount: number;
  runnerCodename: string | null;
}

export function runnerProtocols(token: string): string[] {
  const value = token.trim();
  return value ? [`${RUNNER_AUTH_PROTOCOL_PREFIX}${value}`] : [];
}

export function isRpcResponse(value: unknown): value is RpcResponse {
  if (!value || typeof value !== 'object') return false;
  const frame = value as Partial<RpcResponse>;
  return frame.type === 'response'
    && typeof frame.command === 'string'
    && typeof frame.success === 'boolean';
}

/** App.tsx:3446-3450 — responses are `type === 'response'`; anything else
 *  with a string `type` is an event. Session identity is `sessionId`
 *  (App.tsx:2072-2074 `frameSession`). */
export function isRunnerEventFrame(value: unknown): value is EventFrame {
  if (!value || typeof value !== 'object') return false;
  const frame = value as { type?: unknown };
  return typeof frame.type === 'string' && frame.type !== 'response';
}

export function readNonemptyString(value: unknown): string | null {
  return typeof value === 'string' && value.trim() ? value : null;
}

export function readRuntimeIdentity(
  sessionId: unknown,
  runtimeIncarnation: unknown,
): RunnerRuntimeIdentity | null {
  const id = readNonemptyString(sessionId);
  const incarnation = readNonemptyString(runtimeIncarnation);
  if (!id || !incarnation) return null;
  return { sessionId: id, runtimeIncarnation: incarnation };
}

export function runtimeIdentityKey(identity: RunnerRuntimeIdentity): string {
  return `${identity.sessionId.length}:${identity.sessionId}:${identity.runtimeIncarnation.length}:${identity.runtimeIncarnation}`;
}

export function identitiesFromRpcData(
  command: string,
  data: unknown,
): RunnerRuntimeIdentity[] {
  if (!data || typeof data !== 'object') return [];
  if (command === 'session_list') {
    if (!('sessions' in data) || !Array.isArray(data.sessions)) return [];
    const identities: RunnerRuntimeIdentity[] = [];
    for (const row of data.sessions) {
      if (!row || typeof row !== 'object') continue;
      if (!('loaded' in row) || row.loaded !== true) continue;
      const identity = readRuntimeIdentity(
        'sessionId' in row ? row.sessionId : null,
        'runtimeIncarnation' in row ? row.runtimeIncarnation : null,
      );
      if (identity) identities.push(identity);
    }
    return identities;
  }
  if (command === 'get_state' || command === 'new_session' || command === 'switch_session') {
    const identity = readRuntimeIdentity(
      'sessionId' in data ? data.sessionId : null,
      'runtimeIncarnation' in data ? data.runtimeIncarnation : null,
    );
    return identity ? [identity] : [];
  }
  return [];
}

export function readRunnerProbe(state: unknown, sessions: unknown): RunnerProbe {
  if (!state || typeof state !== 'object') {
    throw new Error('get_state response is missing runner state.');
  }
  const sessionId = 'sessionId' in state ? readNonemptyString(state.sessionId) : null;
  const runtimeIncarnation = 'runtimeIncarnation' in state ? readNonemptyString(state.runtimeIncarnation) : null;
  if (sessionId && !runtimeIncarnation) {
    throw new Error('get_state response is missing runtime incarnation.');
  }
  const cwd = 'cwd' in state && typeof state.cwd === 'string' ? state.cwd : '';
  const runnerCodename = 'runnerCodename' in state ? readNonemptyString(state.runnerCodename) : null;

  if (!sessions || typeof sessions !== 'object') {
    throw new Error('session_list response is missing a sessions array.');
  }
  const sessionRows = 'sessions' in sessions ? sessions.sessions : undefined;
  if (!Array.isArray(sessionRows)) {
    throw new Error('session_list response is missing a sessions array.');
  }
  return {
    sessionId,
    runtimeIncarnation: sessionId ? runtimeIncarnation : null,
    cwd,
    sessionCount: sessionRows.length,
    runnerCodename,
  };
}

export function nextReconnectDelay(current: number): number {
  return Math.min(current * 2, RUNNER_RECONNECT_MAX_MS);
}

// ---- tmux panel response shapes (TmuxPanel.tsx + chatTmuxPanel.test.ts).
// The wire contract is frozen with the Rust runner side; every parser here
// fails closed — any missing, wrong-typed, or drifted field rejects the
// whole payload (null), so the panel renders either a fully validated
// payload or a fixed neutral line, never a partial frame. ----

/** One row of `tmux_list_sessions` data (frozen contract). */
export interface TmuxSessionRow {
  name: string;
  windows: number;
  attached: boolean;
  createdEpoch: number;
  paneCount: number;
}

export interface TmuxSessionListData {
  tmuxAvailable: boolean;
  sessions: TmuxSessionRow[];
}

export interface TmuxPaneSnapshotData {
  session: string;
  pane: string;
  width: number;
  height: number;
  content: string;
}

/** `tmux_send_keys` success data: echo of the target session + literal true. */
export interface TmuxSendResultData {
  session: string;
  sent: true;
}

/** Named key of the tmux_send_keys wire contract. */
export type TmuxSendKey =
  | 'enter'
  | 'escape'
  | 'ctrl_c'
  | 'ctrl_d'
  | 'tab'
  | 'up'
  | 'down'
  | 'left'
  | 'right';

/** tmux_list_sessions data: a boolean availability flag plus a sessions
 *  array. Drop nothing — one malformed row rejects the WHOLE payload, so a
 *  drifting runner can never leave a half-rendered session list. */
export function parseTmuxSessionList(data: unknown): TmuxSessionListData | null {
  if (!data || typeof data !== 'object') return null;
  const value = data as Record<string, unknown>;
  if (typeof value.tmuxAvailable !== 'boolean' || !Array.isArray(value.sessions)) return null;
  const sessions: TmuxSessionRow[] = [];
  for (const row of value.sessions) {
    if (!row || typeof row !== 'object') return null;
    const rowValue = row as Record<string, unknown>;
    const name = readNonemptyString(rowValue.name);
    if (name === null || typeof rowValue.attached !== 'boolean') return null;
    const windows = rowValue.windows;
    const createdEpoch = rowValue.createdEpoch;
    const paneCount = rowValue.paneCount;
    if (typeof windows !== 'number' || !Number.isInteger(windows) || windows < 0) return null;
    if (typeof createdEpoch !== 'number' || !Number.isInteger(createdEpoch) || createdEpoch < 0) return null;
    if (typeof paneCount !== 'number' || !Number.isInteger(paneCount) || paneCount < 0) return null;
    sessions.push({ name, windows, attached: rowValue.attached, createdEpoch, paneCount });
  }
  return { tmuxAvailable: value.tmuxAvailable, sessions };
}

/** tmux_pane_snapshot data: a nonempty session/pane id, positive integer
 *  pane geometry (a real pane is never 0-sized — 0, fractional, NaN, or a
 *  string is drift), and non-empty content kept verbatim (whitespace is
 *  meaningful terminal output, so it is never trimmed). */
export function parseTmuxPaneSnapshot(data: unknown): TmuxPaneSnapshotData | null {
  if (!data || typeof data !== 'object') return null;
  const value = data as Record<string, unknown>;
  const session = readNonemptyString(value.session);
  const pane = readNonemptyString(value.pane);
  if (session === null || pane === null) return null;
  if (typeof value.width !== 'number' || !Number.isInteger(value.width) || value.width < 1) return null;
  if (typeof value.height !== 'number' || !Number.isInteger(value.height) || value.height < 1) return null;
  if (typeof value.content !== 'string' || value.content.length === 0) return null;
  return { session, pane, width: value.width, height: value.height, content: value.content };
}

/** tmux_send_keys data: the echoed session name and the literal `sent: true`
 *  success marker — a string "true" or a bare echo without `sent` fails. */
export function parseTmuxSendResult(data: unknown): TmuxSendResultData | null {
  if (!data || typeof data !== 'object') return null;
  const value = data as Record<string, unknown>;
  const session = readNonemptyString(value.session);
  if (session === null || value.sent !== true) return null;
  return { session, sent: true };
}

/** `tmux_terminal_attach` success data: PTY id, geometry, and pane history identity. */
export interface TmuxTerminalAttachData {
  terminalId: string;
  session: string;
  cols: number;
  rows: number;
  paneKey: string;
  historyKey: string;
  historyBytes: number;
  historyComplete: boolean;
}

export interface TmuxTerminalOutputFrame {
  type: 'tmux_terminal_output';
  terminalId: string;
  data: string;
}

export interface TmuxTerminalExitFrame {
  type: 'tmux_terminal_exit';
  terminalId: string;
  code: number;
}

export interface TmuxTerminalHistoryFrame {
  type: 'tmux_terminal_history';
  terminalId: string;
  paneKey: string;
  historyKey: string;
  data: string;
  final: boolean;
}


/** tmux_terminal_attach data: nonempty ids, positive geometry, pane history stamp. */
export function parseTmuxTerminalAttach(data: unknown): TmuxTerminalAttachData | null {
  if (!data || typeof data !== 'object') return null;
  const value = data as Record<string, unknown>;
  const terminalId = readNonemptyString(value.terminalId);
  const session = readNonemptyString(value.session);
  const paneKey = readNonemptyString(value.paneKey);
  const historyKey = readNonemptyString(value.historyKey);
  if (terminalId === null || session === null || paneKey === null || historyKey === null) return null;
  if (typeof value.cols !== 'number' || !Number.isInteger(value.cols) || value.cols < 1) return null;
  if (typeof value.rows !== 'number' || !Number.isInteger(value.rows) || value.rows < 1) return null;
  if (typeof value.historyBytes !== 'number' || !Number.isInteger(value.historyBytes) || value.historyBytes < 0) {
    return null;
  }
  if (typeof value.historyComplete !== 'boolean') return null;
  return {
    terminalId,
    session,
    cols: value.cols,
    rows: value.rows,
    paneKey,
    historyKey,
    historyBytes: value.historyBytes,
    historyComplete: value.historyComplete,
  };
}

/** Streamed PTY output: matching type, nonempty terminalId, base64 `data` string. */
export function parseTmuxTerminalOutput(frame: unknown): TmuxTerminalOutputFrame | null {
  if (!frame || typeof frame !== 'object') return null;
  const value = frame as Record<string, unknown>;
  if (value.type !== 'tmux_terminal_output') return null;
  const terminalId = readNonemptyString(value.terminalId);
  if (terminalId === null || typeof value.data !== 'string') return null;
  return { type: 'tmux_terminal_output', terminalId, data: value.data };
}

/** tmux client exit: matching type, nonempty terminalId, integer exit code. */
export function parseTmuxTerminalExit(frame: unknown): TmuxTerminalExitFrame | null {
  if (!frame || typeof frame !== 'object') return null;
  const value = frame as Record<string, unknown>;
  if (value.type !== 'tmux_terminal_exit') return null;
  const terminalId = readNonemptyString(value.terminalId);
  if (terminalId === null || typeof value.code !== 'number' || !Number.isInteger(value.code)) return null;
  return { type: 'tmux_terminal_exit', terminalId, code: value.code };
}

/** History replay: ANSI capture bytes as base64, streamed before live output. */
export function parseTmuxTerminalHistory(frame: unknown): TmuxTerminalHistoryFrame | null {
  if (!frame || typeof frame !== 'object') return null;
  const value = frame as Record<string, unknown>;
  if (value.type !== 'tmux_terminal_history') return null;
  const terminalId = readNonemptyString(value.terminalId);
  const paneKey = readNonemptyString(value.paneKey);
  const historyKey = readNonemptyString(value.historyKey);
  if (terminalId === null || paneKey === null || historyKey === null) return null;
  if (typeof value.data !== 'string' || typeof value.final !== 'boolean') return null;
  return {
    type: 'tmux_terminal_history',
    terminalId,
    paneKey,
    historyKey,
    data: value.data,
    final: value.final,
  };
}
