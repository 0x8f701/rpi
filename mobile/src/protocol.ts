// rpi LAN Chat wire contract, Android port of crates/pi-cli/web/src/chat/{auth,protocol}.ts.
// Command frames: {...command, id, sessionId?}. Responses: {type:'response', command, success, data?, error?, sessionId?, id?}.

// The canonical diff line of the RN code-review surface: the snapshot reader
// and the single-file page readers below share ONE line shape, because a panel
// merges both into the same file view.
import type { DiffLine } from './codeReviewSnapshot';

// The diff-line shape is part of this module's public surface: pages, hunks,
// and loaded diffs all expose it, so panels import it from here.
export type { DiffLine };
import { Buffer } from 'buffer/';

export interface RunnerEndpoint {
  origin: string;
  authority: string;
  webSocketUrl: string;
}

/** One runner address split by hand: React Native's URL throws
 *  `URL.<prop> is not implemented` for every accessor but `href`
 *  (node_modules/react-native/Libraries/Blob/URL.js), so the pairing path must
 *  not touch it. `scheme://host[:port][/path][?query]` is the whole of what the
 *  invite contract accepts. */
export interface RunnerAddress {
  /** Lowercased scheme, without its colon. */
  scheme: string;
  /** `host[:port]`, host lowercased, default port dropped. */
  authority: string;
  /** Authority without the port; an IPv6 literal keeps its brackets. */
  host: string;
  /** Path as written, `''` when the address carries none. */
  path: string;
  /** The authority carried a non-empty `user:pass@` userinfo. */
  credentials: boolean;
  /** The address carried a non-empty `?query`. */
  query: boolean;
}

const SCHEME_PREFIX = /^([a-z][a-z0-9+.-]*):\/\//i;
const SPECIAL_SCHEMES: Record<string, true> = { ftp: true, file: true, http: true, https: true, ws: true, wss: true };
const DEFAULT_PORTS: Record<string, number> = { ftp: 21, http: 80, https: 443, ws: 80, wss: 443 };
// WHATWG strips tab/CR/LF from the input, rejects these code points in a host,
// and treats `\` as `/` inside a special scheme's path.
const STRIPPED = /[\t\n\r]/g;
const INVALID_HOST_CHAR = /[%\^|<>\s\0]/;
// No IDNA anywhere in this stack: the runtime URL throws outright, Android's
// Uri.getHost() keeps the decoded Unicode host, and SNIHostName rejects it, so
// a non-ASCII host is refused instead of stored undialable.
const NON_ASCII_HOST = /[^\x00-\x7f]/;
const IPV6_GROUP = /^[0-9a-f]{1,4}$/i;
const IPV4_TAIL = /^(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)(?:\.(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)){3}$/;
const DIGITS = /^[0-9]+$/;

/** WHATWG IPv6 body, brackets already removed: at most one `::` that fills at
 *  least one group, 1-4 hex digits per group, a trailing dotted quad counting
 *  as two groups, eight groups at most. */
function isIpv6Literal(body: string): boolean {
  const compression = body.indexOf('::');
  if (compression !== body.lastIndexOf('::')) return false;
  const groups = compression < 0 ? [body] : [body.slice(0, compression), body.slice(compression + 2)];
  const pieces = groups.flatMap((group) => (group === '' ? [] : group.split(':')));
  if (pieces.some((piece) => piece === '')) return false;
  const last = pieces[pieces.length - 1] ?? '';
  const quad = last.includes('.');
  if (quad && !IPV4_TAIL.test(last)) return false;
  const hex = quad ? pieces.slice(0, -1) : pieces;
  if (hex.some((piece) => !IPV6_GROUP.test(piece))) return false;
  const count = hex.length + (quad ? 2 : 0);
  return compression < 0 ? count === 8 : count < 8;
}

/** Split one runner address. Returns null for anything the pairing path must
 *  refuse: an empty authority (`https:///x`, `//h/x` — an origin must name a
 *  host), a non-ASCII host, port 0, or a port above 65535. The rest follows
 *  WHATWG: tab/CR/LF stripped, `user:pass@` split at the last `@`, port at the
 *  first `:` after a bracketed host, default ports dropped, hosts lowercased.
 *  `fallbackScheme` is assumed when the input carries no `://`; pass null to
 *  reject a scheme-less input instead. */
export function splitRunnerAddress(raw: string, fallbackScheme: string | null): RunnerAddress | null {
  const value = raw.replace(STRIPPED, '').trim();
  const hashIndex = value.indexOf('#');
  let rest = hashIndex < 0 ? value : value.slice(0, hashIndex);
  let scheme: string;
  const prefix = SCHEME_PREFIX.exec(rest);
  if (prefix) {
    scheme = prefix[1].toLowerCase();
    rest = rest.slice(prefix[0].length);
  } else if (fallbackScheme && !rest.includes('://')) {
    scheme = fallbackScheme;
  } else {
    return null;
  }
  const boundary = rest.search(/[/\\?]/);
  const head = boundary < 0 ? rest : rest.slice(0, boundary);
  const tail = boundary < 0 ? '' : rest.slice(boundary);
  // WHATWG splits userinfo at the LAST `@`, then the port at the first `:`
  // that follows the host (past `]` when the host is a bracketed literal).
  const at = head.lastIndexOf('@');
  // Userinfo splits at the FIRST colon: a password may itself start with ':'.
  const userinfo = at < 0 ? '' : head.slice(0, at);
  const colon = userinfo.indexOf(':');
  const user = colon < 0 ? userinfo : userinfo.slice(0, colon);
  const password = colon < 0 ? '' : userinfo.slice(colon + 1);
  const hostText = at < 0 ? head : head.slice(at + 1);
  const bracketed = hostText.startsWith('[');
  const closing = bracketed ? hostText.indexOf(']') : -1;
  if (bracketed && closing < 0) return null;
  const portAt = hostText.indexOf(':', closing + 1);
  const written = portAt < 0 ? hostText : hostText.slice(0, portAt);
  const portText = portAt < 0 ? '' : hostText.slice(portAt + 1);
  if (!written || INVALID_HOST_CHAR.test(written) || NON_ASCII_HOST.test(written)) return null;
  if (bracketed ? !(written.endsWith(']') && isIpv6Literal(written.slice(1, -1))) : /[[\]]/.test(written)) return null;
  if (portText !== '' && !DIGITS.test(portText)) return null;
  const port = portText === '' ? '' : String(Number(portText));
  // WHATWG itself accepts port 0 and keeps it; the pairing path refuses it,
  // since no runner ever listens on the kernel-assigned port.
  if (port !== '' && (Number(port) === 0 || Number(port) > 65535)) return null;
  const special = SPECIAL_SCHEMES[scheme] === true;
  const host = special ? written.toLowerCase() : written;
  const queryAt = tail.indexOf('?');
  const writtenPath = queryAt < 0 ? tail : tail.slice(0, queryAt);
  return {
    scheme,
    authority: port !== '' && DEFAULT_PORTS[scheme] !== Number(port) ? `${host}:${port}` : host,
    host,
    path: special ? writtenPath.replace(/\\/g, '/') : writtenPath,
    credentials: user !== '' || password !== '',
    query: queryAt >= 0 && tail.length > queryAt + 1,
  };
}

export function parseRunnerInviteLink(raw: string): { endpoint: RunnerEndpoint; token: string } {
  const value = raw.trim();
  if (!value) throw new Error('Runner address is required.');
  const hashIndex = value.indexOf('#');
  const address = splitRunnerAddress(value, 'https');
  if (!address) throw new Error('Runner address must be an HTTP or HTTPS origin.');
  if (address.scheme !== 'http' && address.scheme !== 'https') {
    throw new Error('Runner address must use HTTP or HTTPS.');
  }
  if (address.credentials || address.query) {
    throw new Error('Runner address must not contain credentials or a query.');
  }
  if (address.path !== '' && address.path !== '/' && address.path !== '/web' && address.path !== '/chat') {
    throw new Error('Runner address must be an origin or end in /web or /chat.');
  }
  const webSocketProtocol = address.scheme === 'https' ? 'wss:' : 'ws:';
  const endpoint = {
    origin: `${address.scheme}://${address.authority}`,
    authority: address.authority,
    webSocketUrl: `${webSocketProtocol}//${address.authority}/ws`,
  };
  if (hashIndex < 0) return { endpoint, token: '' };
  let token: string;
  try {
    token = decodeURIComponent(value.slice(hashIndex + 1)).trim();
  } catch {
    throw new Error('Runner invite link hash is not decodable.');
  }
  return { endpoint, token };
}

/** Token of a scanned QR, which is either a host pairing link or a bare token.
 *  A link is reduced through `parseRunnerInviteLink` (percent-decoded fragment,
 *  trimmed), so a link without a fragment has no token and yields ''. A bare
 *  token — no `#` fragment and no scheme — is the token itself, trimmed, and
 *  never throws: the scan that produced it is not the place to reject a string
 *  the user may still fix by hand. */
export function parseRunnerInviteToken(raw: string): string {
  const value = raw.trim();
  if (!value) return '';
  if (value.includes('#')) {
    try {
      return parseRunnerInviteLink(value).token;
    } catch {
      return '';
    }
  }
  return value.includes('://') ? '' : value;
}

export type RunnerCommand =
  | { type: 'get_state' }
  | { type: 'get_messages' }
  | { type: 'get_commands' }
  | { type: 'get_available_models' }
  | { type: 'get_available_thinking_levels' }
  | { type: 'get_session_stats' }
  | { type: 'session_list'; scope: 'current' | 'all_projects' }
  | { type: 'new_session'; cwd: string }
  | { type: 'session_delete'; sessionPath: string }
  | { type: 'session_restore'; undoToken: string }
  | { type: 'switch_session'; sessionPath: string }
  | { type: 'set_session_name'; name: string; sessionPath?: string }
  | { type: 'session_fork_latest'; sessionPath: string }
  | { type: 'fork'; entryId: string; sessionPath?: string }
  | { type: 'clone'; sessionPath?: string }
  | { type: 'session_move'; sessionPath: string; targetCwd: string }
  | { type: 'prompt'; message: string }
  | { type: 'steer'; message: string }
  | { type: 'abort' }
  | { type: 'compact'; customInstructions?: string }
  | { type: 'snapcompact' }
  | { type: 'run'; name: string; args?: string }
  | { type: 'skill'; name: string }
  | { type: 'set_model'; provider: string; modelId: string }
  | { type: 'set_thinking_level'; level: string }
  | { type: 'set_steering_mode'; mode: 'all' | 'one-at-a-time' }
  | { type: 'goal_create'; objective: string; tokenBudget?: number; activate?: boolean }
  | { type: 'goal_pause' }
  | { type: 'goal_resume'; activate?: boolean }
  | { type: 'goal_complete' }
  | { type: 'goal_drop' }
  | { type: 'goal_pin'; text: string }
  | { type: 'goal_unpin'; index: number }
  | { type: 'goal_get' }
  | { type: 'goal_journal' }
  | { type: 'todo_op'; op: string; task?: string; phase?: string; items?: string[]; dependsOn?: string[]; cascade?: boolean }
  | { type: 'todo_child_create'; label?: string; idempotencyKey?: string }
  | { type: 'loop_create'; interval: string; prompt: string; fireImmediately: boolean; durable: boolean }
  | { type: 'loop_list' }
  | { type: 'loop_update'; taskId: string; interval?: string; prompt?: string }
  | { type: 'loop_delete'; taskId: string }
  | { type: 'loop_cancel'; taskId: string }
  | { type: 'job_list' }
  | { type: 'job_cancel'; jobIds: string[] }
  | { type: 'file_explorer_list'; path?: string }
  | { type: 'file_explorer_read'; path: string; cursor?: number; maxLines?: number }
  | { type: 'file_explorer_changes' }
  | { type: 'file_explorer_diff'; path: string; cursor?: number; maxLines?: number }
  | { type: 'file_explorer_blame'; path: string; startLine: number; endLine: number }
  | { type: 'code_review_snapshot' }
  | { type: 'tmux_list_sessions' }
  | { type: 'tmux_pane_snapshot'; session: string; lines?: number }
  | { type: 'tmux_send_keys'; session: string; text?: string; key?: TmuxSendKey }
  | { type: 'tmux_session_create'; session: string }
  | { type: 'tmux_session_delete'; session: string }
  | { type: 'tmux_terminal_attach'; session: string; cols: number; rows: number }
  | { type: 'tmux_terminal_input'; terminalId: string; data: string }
  | { type: 'tmux_terminal_resize'; terminalId: string; cols: number; rows: number }
  | { type: 'tmux_terminal_detach'; terminalId: string }
  | { type: 'get_discovered_runners' }
  // Steering tray (crates/pi-cli/web/src/SteeringQueue.tsx + SessionView).
  | { type: 'queue_list' }
  | { type: 'queue_send_now' }
  | { type: 'queue_cancel'; scope?: 'all' | 'steering'; index?: number }
  // Worktree code review (panels/CodeReviewPanel.tsx). Open, refresh and
  // comment answer with the full review snapshot; the two file readers below
  // serve the bounded per-file pages.
  | { type: 'code_review_open'; from?: string; to?: string; includeWorkingTree?: boolean; commit?: string }
  | { type: 'code_review_refresh' }
  | { type: 'code_review_comment'; snapshotId: string; path: string; oldStart: number; oldCount: number; newStart: number; newCount: number; contentHash: string; comment: string; images?: unknown[] }
  | { type: 'code_review_abort'; snapshotId: string; path: string; oldStart: number; oldCount: number; newStart: number; newCount: number; contentHash: string }
  | { type: 'code_review_close'; reviewId?: string }
  | { type: 'code_review_file_diff'; snapshotId: string; path: string; cursor?: number; maxLines?: number }
  | { type: 'code_review_file_context'; snapshotId: string; path: string; oldStart?: number; newStart?: number; count?: number }
  | { type: 'code_review_set_model'; modelId: string }
  // Commit view: commit is mutually exclusive with from/to/includeWorkingTree; code_review_commits rows carry parentCommit (null = root) and epoch-ms authorDate.
  | { type: 'code_review_commits'; limit?: number }
  // Summary (TL;DR) of the pinned snapshot: one bounded markdown document the
  // controller generates as its single summary operation. Stopping reuses the
  // existing code_review_stop with the tldr operation; the optional language
  // resolves like Auto Review on the runner.
  | { type: 'code_review_tldr'; language?: string }
  | { type: 'code_review_stop'; operation: CodeReviewStopOperation }
  // Settings (panels/SettingsPanel.tsx). The draft is server-held; every
  // mutating row names the draftId it stages into.
  | { type: 'settings_inspect' }
  | { type: 'settings_search'; query: string }
  | { type: 'settings_open_draft'; scope: 'global' | 'project' }
  | { type: 'settings_get_draft'; draftId: string }
  | { type: 'settings_set'; draftId: string; key: string; value: unknown }
  | { type: 'settings_reset'; draftId: string; key: string }
  | { type: 'settings_validate'; draftId: string }
  | { type: 'settings_apply'; draftId: string }
  | { type: 'settings_cancel'; draftId: string }
  // Personas (panels/PersonasPanel.tsx). Remove keeps the persona's
  // memory/sessions, purge deletes the whole persona root; both require an
  // explicit confirm on the wire.
  | { type: 'persona_list' }
  | { type: 'persona_get'; name: string }
  | { type: 'persona_create'; name: string; content: string }
  | { type: 'persona_edit'; name: string; content: string }
  | { type: 'persona_remove'; name: string; confirm?: boolean }
  | { type: 'persona_purge'; name: string; confirm?: boolean }
  | { type: 'persona_select'; name: string }
  | { type: 'persona_clear' }
  | { type: 'persona_current' }
  // Supervised processes (panels/ProcessesPanel.tsx, the `/ps` mirror).
  | { type: 'process_spawn'; spec: ProcessSpawnSpec }
  | { type: 'process_list' }
  | { type: 'process_describe'; processId: string }
  | { type: 'process_logs'; processId: string; cursor?: number; limitBytes?: number }
  | { type: 'process_write'; processId: string; dataBase64: string }
  | { type: 'process_keys'; processId: string; keys: ProcessKey[] }
  | { type: 'process_resize'; processId: string; cols: number; rows: number }
  | { type: 'process_signal'; processId: string; signal: ProcessSignal }
  | { type: 'process_stop'; processId: string }
  | { type: 'process_wait'; processId: string; timeoutMs?: number }
  // Workflows (panels/WorkflowPanel.tsx). Every row routes to the owning
  // session's runtime; detail returns the live panel projection.
  | { type: 'workflow_create'; name: string; objective: string }
  | { type: 'workflow_list' }
  | { type: 'workflow_get'; workflowId?: string; name?: string }
  | { type: 'workflow_detail'; workflowId?: string; name?: string }
  | { type: 'workflow_pause'; workflowId: string }
  | { type: 'workflow_resume'; workflowId: string }
  | { type: 'workflow_cancel'; workflowId: string }
  | { type: 'workflow_integrate'; workflowId: string }
  | { type: 'workflow_remove'; workflowId: string }
  | { type: 'workflow_hub_send'; workflowId: string; generation: number; recipient: string; body: string; replyTo?: string }
  // Side chats (panels/SideChatPanel.tsx): detached parallel tabs that never
  // touch the main session or transcript.
  | { type: 'side_chat_new'; name: string }
  | { type: 'side_chat_switch'; name: string }
  | { type: 'side_chat_close'; name?: string }
  | { type: 'side_chat_prompt'; message: string; images?: unknown[] }
  | { type: 'side_chat_list' }
  | { type: 'side_chat_abort' }
  | { type: 'file_conversation_open'; conversationId: string; path: string }
  | { type: 'file_conversation_prompt'; conversationId: string; message: string; selection?: FileConversationRangeWire }
  | { type: 'file_conversation_snapshot'; conversationId: string }
  | { type: 'file_conversation_abort'; conversationId: string }
  | { type: 'file_conversation_close'; conversationId: string }
  // Durable child sessions of the Todo page (panels/TodoChildPanel.tsx).
  | { type: 'todo_child_list'; parentSessionId: string }
  | { type: 'todo_child_select'; parentSessionId: string; childSessionId: string }
  | { type: 'todo_child_send'; parentSessionId: string; childSessionId?: string; message: string }
  | { type: 'todo_child_status'; parentSessionId: string; childSessionId?: string }
  | { type: 'todo_child_stop'; parentSessionId: string; childSessionId?: string }
  | { type: 'todo_child_pause'; parentSessionId: string; childSessionId?: string }
  | { type: 'todo_child_resume'; parentSessionId: string; childSessionId?: string }
  | { type: 'todo_child_cancel'; parentSessionId: string; childSessionId?: string }
  | { type: 'todo_child_archive'; parentSessionId: string; childSessionId: string }
  | { type: 'todo_child_remove'; parentSessionId: string; childSessionId: string }
  | { type: 'todo_child_add_task'; parentSessionId: string; childSessionId: string; content: string; dependsOn?: string[]; agent?: string; role?: string; expectedRevision?: number }
  | { type: 'todo_child_detail'; parentSessionId: string; childSessionId: string }
  | { type: 'todo_child_hub_send'; parentSessionId: string; childSessionId: string; to: string; body: string; replyTo?: string }
  // Session lifecycle beyond the chat shell: OMP-parity handoff, saved-row
  // fork paging, bounded context copy, and HTML export.
  | { type: 'session_handoff'; customInstructions?: string; responseMode?: 'subscribe-handoff-v1' }
  | { type: 'get_fork_messages'; sessionPath?: string; cursor?: string; limit?: number }
  | { type: 'session_copy_context'; sessionPath: string }
  | { type: 'export_html'; outputPath?: string; sessionPath?: string }
  // Path completion and one-shot downloads (PathAutocomplete, FileExplorer).
  | { type: 'path_complete'; path: string; kind: 'directory' | 'file' | 'any'; limit?: number }
  | { type: 'file_explorer_stage_download'; path: string }
  // Orchestration (panels/SubagentsPanel.tsx, PersonasPanel Run).
  | { type: 'job_output'; jobId: string }
  | { type: 'agent_history'; agentId: string; lines?: number }
  | { type: 'task_spawn'; args: unknown }
  | { type: 'hub_send'; to: string; body: string; replyTo?: string };

export interface RuntimeIdentity {
  sessionId: string;
  runtimeIncarnation: string;
}

export interface RpcResponse {
  sessionId?: string;
  turnId?: string;
  id?: string;
  type: string;
  command: string;
  success: boolean;
  data?: unknown;
  error?: string;
}

export function isRpcResponse(value: unknown): value is RpcResponse {
  if (!value || typeof value !== 'object') return false;
  const frame = value as Partial<RpcResponse>;
  return frame.type === 'response' && typeof frame.command === 'string' && typeof frame.success === 'boolean';
}

// ---- Transport contract ported from crates/pi-cli/web/src/{protocol,pending}.ts ----
//
// Command ids are reload-safe: the runner keeps a bounded ledger keyed by the
// caller's command id, so re-admitting an id with an identical payload replays
// the recorded outcome and reusing an id with a different payload is rejected
// as a conflicting request. Bare sequential ids (`m1`, `m2`, ...) collide with
// the retained ledger after an app reload, so every id is namespaced by a
// per-client generation a reload cannot reproduce.

/** Bytes of entropy in the per-client command namespace (12 base36 chars). */
const CLIENT_NAMESPACE_BYTES = 6;

function defaultRandomBytes(n: number): Uint8Array {
  const out = new Uint8Array(n);
  const cryptoObj = (globalThis as { crypto?: { getRandomValues?: (target: Uint8Array) => Uint8Array } }).crypto;
  if (cryptoObj && typeof cryptoObj.getRandomValues === 'function') {
    cryptoObj.getRandomValues(out);
    return out;
  }
  for (let i = 0; i < n; i += 1) {
    out[i] = Math.floor(Math.random() * 256);
  }
  return out;
}

/** Mint a stable per-client (per app run) namespace. Inject `randomBytes` in tests. */
export function createClientNamespace(
  randomBytes: (n: number) => Uint8Array = defaultRandomBytes,
): string {
  const bytes = randomBytes(CLIENT_NAMESPACE_BYTES);
  let namespace = '';
  for (let i = 0; i < bytes.length; i += 1) {
    namespace += bytes[i].toString(36).padStart(2, '0').slice(-2);
  }
  return namespace;
}

/** Command id for `seq` under a client namespace. Never the bare `m<N>` shape. */
export function nextCommandId(namespace: string, seq: number): string {
  return `c${namespace}-${seq}`;
}

/** Fast-ack commands: the runner replies within this window or it is stuck. */
export const DEFAULT_COMMAND_TIMEOUT_MS = 30_000;

/** Long operations remain bounded without inheriting transport authority. */
export const LONG_OP_TIMEOUT_MS = 10 * 60_000;

/** Commands whose response includes legitimately long bounded work. New
 *  long-running commands must be added here explicitly: an unlisted type falls
 *  back to the fast-ack bound rather than silently inheriting the long one. */
const LONG_OP_TYPES: Readonly<Record<string, true>> = {
  compact: true,
  snapcompact: true,
  get_messages: true,
  code_review_snapshot: true,
  new_session: true,
};

/** The bounded timeout class for a command type. */
export function commandTimeoutMs(type: string): number {
  if (LONG_OP_TYPES[type]) return LONG_OP_TIMEOUT_MS;
  return DEFAULT_COMMAND_TIMEOUT_MS;
}

/** User-facing timeout message: names the command and the elapsed seconds. */
export function timeoutErrorMessage(type: string, sentAt: number, now: number): string {
  const elapsed = Math.max(0, Math.round((now - sentAt) / 1000));
  return `${type} timed out after ${elapsed}s (no response from server)`;
}

/** A wire response frame relevant to the ownership decision. */
export interface RpcResponseOwnershipFrame {
  command?: string;
  sessionId?: string;
  success?: boolean;
  data?: unknown;
}

export type ResponseOwnershipVerdict =
  | 'accept'
  | 'command-mismatch'
  | 'source-ownership-mismatch'
  | 'target-ownership-mismatch';

/** Exact rejection message per non-accept verdict. */
export const RESPONSE_OWNERSHIP_REJECTIONS: Readonly<
  Record<Exclude<ResponseOwnershipVerdict, 'accept'>, string>
> = {
  'command-mismatch': 'rpc response command ownership mismatch',
  'source-ownership-mismatch': 'rpc response session ownership mismatch',
  'target-ownership-mismatch': 'rpc response target ownership mismatch',
};

/** Lifecycle commands whose SUCCESS envelope is TARGET-owned: the runner tags
 *  the response with the target runtime's sessionId (the caller routes an
 *  immediate follow-up against it), while failures stay request-source-owned. */
const TARGET_OWNED_LIFECYCLE_TYPES: Readonly<Record<string, true>> = {
  new_session: true,
  switch_session: true,
  fork: true,
  clone: true,
  session_fork_latest: true,
};

/** Ownership gate applied BEFORE settling a pending entry. Command identity is
 *  checked first: a frame that does not NAME the pending command can never own
 *  it. The target-owned exemption exists only for a SUCCESS envelope of a
 *  lifecycle command, verified against the payload sessionId; everything else
 *  — ordinary commands AND lifecycle failures, which the runner tags with the
 *  request source — is request-source-owned. */
export function classifyResponseOwnership(
  frame: RpcResponseOwnershipFrame,
  pending: { type: string; sessionId?: string },
): ResponseOwnershipVerdict {
  if (frame.command !== pending.type) return 'command-mismatch';
  const data: unknown = frame.data;
  const payloadSessionId = data !== null && typeof data === 'object' && 'sessionId' in data
    && typeof data.sessionId === 'string'
    ? data.sessionId
    : '';
  if (frame.success === true && TARGET_OWNED_LIFECYCLE_TYPES[pending.type] === true) {
    if (payloadSessionId === '' || frame.sessionId !== payloadSessionId) {
      return 'target-ownership-mismatch';
    }
    return 'accept';
  }
  if (pending.sessionId && frame.sessionId !== pending.sessionId) {
    return 'source-ownership-mismatch';
  }
  return 'accept';
}

export function readNonemptyString(value: unknown): string | null {
  return typeof value === 'string' && value.trim() ? value : null;
}

export interface TmuxSessionRow {
  name: string;
  windows: number;
  attached: boolean;
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
    if (typeof rowValue.windows !== 'number' || !Number.isInteger(rowValue.windows) || rowValue.windows < 0) return null;
    sessions.push({ name, windows: rowValue.windows, attached: rowValue.attached });
  }
  return { tmuxAvailable: value.tmuxAvailable, sessions };
}

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

export interface RunnerStateData {
  sessionId: string | null;
  runtimeIncarnation: string | null;
  cwd: string;
}

export function parseRunnerState(data: unknown): RunnerStateData | null {
  if (!data || typeof data !== 'object') return null;
  const value = data as Record<string, unknown>;
  const sessionId = readNonemptyString(value.sessionId);
  const runtimeIncarnation = readNonemptyString(value.runtimeIncarnation);
  const cwd = typeof value.cwd === 'string' ? value.cwd : '';
  return { sessionId, runtimeIncarnation, cwd };
}

export interface SessionRow {
  sessionId: string;
  path: string;
  cwd: string;
  name: string | null;
}

export function parseSessionRow(data: unknown): SessionRow | null {
  const value = wireRecord(data);
  if (!value) return null;
  const sessionId = readNonemptyString(value.sessionId);
  const session = wireRecord(value.session);
  const state = wireRecord(value.state);
  if (session?.sessionId !== undefined && session.sessionId !== sessionId) return null;
  if (state?.sessionId !== undefined && state.sessionId !== sessionId) return null;
  const row = session ?? value;
  const rowPath = readNonemptyString(row.path);
  const sessionFile = readNonemptyString(state?.sessionFile);
  if (rowPath !== null && sessionFile !== null && rowPath !== sessionFile) return null;
  const path = rowPath ?? sessionFile;
  const cwd = readNonemptyString(row.cwd) ?? readNonemptyString(state?.cwd);
  if (sessionId === null || path === null || cwd === null) return null;
  const name = typeof row.name === 'string' && row.name ? row.name : null;
  return { sessionId, path, cwd, name };
}

export interface SessionCatalogRow {
  sessionId: string;
  path: string;
  cwd: string;
  name: string | null;
  loaded: boolean;
  runtimeIncarnation: string | null;
  status: string;
}

export function parseSessionCatalog(data: unknown): SessionCatalogRow[] | null {
  if (!data || typeof data !== 'object') return null;
  const value = data as Record<string, unknown>;
  if (!Array.isArray(value.sessions)) return null;
  const rows: SessionCatalogRow[] = [];
  for (const row of value.sessions) {
    if (!row || typeof row !== 'object') return null;
    const rowValue = row as Record<string, unknown>;
    const sessionId = readNonemptyString(rowValue.sessionId);
    const path = readNonemptyString(rowValue.path);
    if (sessionId === null || path === null) return null;
    rows.push({
      sessionId,
      path,
      cwd: typeof rowValue.cwd === 'string' ? rowValue.cwd : '',
      name: typeof rowValue.name === 'string' && rowValue.name ? rowValue.name : null,
      loaded: rowValue.loaded === true,
      runtimeIncarnation: readNonemptyString(rowValue.runtimeIncarnation),
      status: typeof rowValue.status === 'string' ? rowValue.status : 'native',
    });
  }
  return rows;
}

export function parseRuntimeIdentity(data: unknown): RuntimeIdentity | null {
  if (!data || typeof data !== 'object') return null;
  const value = data as Record<string, unknown>;
  const sessionId = readNonemptyString(value.sessionId);
  const runtimeIncarnation = readNonemptyString(value.runtimeIncarnation);
  if (sessionId === null || runtimeIncarnation === null) return null;
  return { sessionId, runtimeIncarnation };
}

export function parseMessages(data: unknown): unknown[] | null {
  if (!data || typeof data !== 'object') return null;
  const value = data as Record<string, unknown>;
  if (!Array.isArray(value.messages)) return null;
  return value.messages;
}

export interface SubscribeFrame {
  type: 'subscribe';
  sessionId: string;
  runtimeIncarnation: string;
  cursor: number;
}

function stripListenPortSuffix(name: string): string {
  const match = /^(.*)-(\d+)$/.exec(name);
  if (!match) return name;
  const port = Number(match[2]);
  if (!Number.isInteger(port) || port < 1024 || port > 65535) return name;
  return match[1];
}

function hostnameFromAuthority(authority: string): string {
  const host = authority.split(':')[0] ?? authority;
  return host || authority;
}

export function runnerDisplayName(record: { codename: string | null; label: string; authority: string }): string {
  const named = record.codename?.trim() || record.label.trim();
  if (named) return stripListenPortSuffix(named);
  return hostnameFromAuthority(record.authority);
}

export interface TmuxTerminalAttachData {
  terminalId: string;
  session: string;
  cols: number;
  rows: number;
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

export function parseTmuxTerminalAttach(data: unknown): TmuxTerminalAttachData | null {
  if (!data || typeof data !== 'object') return null;
  const value = data as Record<string, unknown>;
  const terminalId = readNonemptyString(value.terminalId);
  const session = readNonemptyString(value.session);
  if (terminalId === null || session === null) return null;
  if (typeof value.cols !== 'number' || !Number.isInteger(value.cols) || value.cols < 1) return null;
  if (typeof value.rows !== 'number' || !Number.isInteger(value.rows) || value.rows < 1) return null;
  return { terminalId, session, cols: value.cols, rows: value.rows };
}

export function parseTmuxTerminalOutput(frame: unknown): TmuxTerminalOutputFrame | null {
  if (!frame || typeof frame !== 'object') return null;
  const value = frame as Record<string, unknown>;
  if (value.type !== 'tmux_terminal_output') return null;
  const terminalId = readNonemptyString(value.terminalId);
  if (terminalId === null || typeof value.data !== 'string') return null;
  return { type: 'tmux_terminal_output', terminalId, data: value.data };
}

export function parseTmuxTerminalExit(frame: unknown): TmuxTerminalExitFrame | null {
  if (!frame || typeof frame !== 'object') return null;
  const value = frame as Record<string, unknown>;
  if (value.type !== 'tmux_terminal_exit') return null;
  const terminalId = readNonemptyString(value.terminalId);
  if (terminalId === null || typeof value.code !== 'number' || !Number.isInteger(value.code)) return null;
  return { type: 'tmux_terminal_exit', terminalId, code: value.code };
}

export function encodeTmuxTerminalData(text: string): string {
  const bytes = new TextEncoder().encode(text);
  let binary = '';
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary);
}

export function decodeTmuxTerminalData(data: string): Uint8Array | null {
  try {
    const binary = atob(data);
    const bytes = new Uint8Array(binary.length);
    for (let i = 0; i < binary.length; i += 1) bytes[i] = binary.charCodeAt(i);
    return bytes;
  } catch {
    return null;
  }
}

export interface DiscoveredRunner {
  origin: string;
  host: string;
  port: number;
  label: string;
  version: string | null;
  scheme: string;
  codename: string | null;
}

export function parseDiscoveredRunner(value: unknown): DiscoveredRunner | null {
  if (!value || typeof value !== 'object') return null;
  const row = value as Record<string, unknown>;
  const origin = readNonemptyString(row.origin);
  const host = readNonemptyString(row.host);
  if (origin === null || host === null) return null;
  if (typeof row.port !== 'number' || !Number.isFinite(row.port) || row.port <= 0) return null;
  const scheme = readNonemptyString(row.scheme) ?? 'ws';
  const label = readNonemptyString(row.label) ?? host;
  const version = readNonemptyString(row.version);
  const codename = readNonemptyString(row.codename);
  return { origin, host, port: row.port, label, version, scheme, codename };
}

export function parseDiscoveredRunnerList(data: unknown): DiscoveredRunner[] | null {
  if (!data || typeof data !== 'object') return null;
  const value = data as Record<string, unknown>;
  if (!Array.isArray(value.runners)) return null;
  const runners: DiscoveredRunner[] = [];
  for (const row of value.runners) {
    const parsed = parseDiscoveredRunner(row);
    if (!parsed) return null;
    runners.push(parsed);
  }
  return runners;
}

export function originAuthority(raw: string): string {
  const address = splitRunnerAddress(raw, 'http');
  return address ? address.authority : raw;
}

export function discoveredAlreadyAdded(discovered: DiscoveredRunner, addedAuthorities: string[]): boolean {
  const key = originAuthority(discovered.origin);
  return addedAuthorities.some((authority) => originAuthority(authority) === key);
}

// ---- Panel wire rows ported from crates/pi-cli/web/src ----
//
// The /web panels speak a wider RPC surface than the LAN chat shell. Every row
// added to `RunnerCommand` carries the exact field names the Rust `RpcCommand`
// enum deserializes; a field the Rust side marks `#[serde(default)]` or types
// `Option<…>` stays optional here. `id` and `sessionId` are NOT command
// fields: `RunnerClient.dispatch` stamps them onto the frame and the manager
// strips the session selector before deserialization.
//
// The readers below are the panels' fail-closed front door: a malformed or
// drifting payload yields null (or the one documented empty page) and never a
// fabricated row, so a panel can only render what the runner actually sent.

/** Named key of the frozen `tmux_send_keys` contract. */
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

/** `process_spawn` spec (crates/pi-coding/src/process/mod.rs, camelCase wire). */
export interface ProcessSpawnSpec {
  argv: string[];
  cwd: string;
  env?: Record<string, string | null>;
  tty?: boolean;
  terminalSize?: { rows: number; cols: number } | null;
  label?: string;
  timeoutMs?: number;
  outputBytes?: number;
}

/** Named control key of the supervised-process contract (SCREAMING_SNAKE_CASE). */
export type ProcessKey =
  | 'ENTER'
  | 'TAB'
  | 'ESCAPE'
  | 'CTRL_C'
  | 'CTRL_D'
  | 'UP'
  | 'DOWN'
  | 'LEFT'
  | 'RIGHT';

/** Signal of the supervised-process contract (SCREAMING_SNAKE_CASE). */
export type ProcessSignal = 'SIGINT' | 'SIGTERM' | 'SIGHUP' | 'SIGQUIT' | 'SIGKILL';

/** Field spec of `RUNNER_COMMAND_WIRE`: a bare kind — `string`, `number`
 *  (non-negative safe integer), `boolean`, `string[]`, `array`, `object`,
 *  `any` — a closed `enum:a|b` literal set, and a trailing `?` for an optional
 *  field. */
type WireFieldSpec = string;

/** Runtime mirror of the union, one row per command type.
 *
 *  A row's field set is pinned to the union by `satisfies`: a missing or
 *  renamed field fails as an incomplete `Record`, an invented field as an
 *  excess property, and a missing row fails as an absent mapping key — so
 *  `tsc --noEmit` refuses any drift between the table, the guard built on it,
 *  and the union the panels compile against. The `type?: never` marker keeps
 *  every row's constraint non-empty (excess-property checking is skipped
 *  against a bare `{}`), which is what catches an invented field in a row the
 *  union gives no fields at all. */
export const RUNNER_COMMAND_WIRE = {
  get_state: {},
  get_messages: {},
  get_commands: {},
  get_available_models: {},
  get_available_thinking_levels: {},
  get_session_stats: {},
  session_list: { scope: 'enum:current|all_projects' },
  new_session: { cwd: 'string' },
  session_delete: { sessionPath: 'string' },
  session_restore: { undoToken: 'string' },
  switch_session: { sessionPath: 'string' },
  set_session_name: { name: 'string', sessionPath: 'string?' },
  session_fork_latest: { sessionPath: 'string' },
  fork: { entryId: 'string', sessionPath: 'string?' },
  clone: { sessionPath: 'string?' },
  session_move: { sessionPath: 'string', targetCwd: 'string' },
  prompt: { message: 'string' },
  steer: { message: 'string' },
  abort: {},
  compact: { customInstructions: 'string?' },
  snapcompact: {},
  run: { name: 'string', args: 'string?' },
  skill: { name: 'string' },
  set_model: { provider: 'string', modelId: 'string' },
  set_thinking_level: { level: 'string' },
  set_steering_mode: { mode: 'enum:all|one-at-a-time' },
  goal_create: { objective: 'string', tokenBudget: 'number?', activate: 'boolean?' },
  goal_pause: {},
  goal_resume: { activate: 'boolean?' },
  goal_complete: {},
  goal_drop: {},
  goal_pin: { text: 'string' },
  goal_unpin: { index: 'number' },
  goal_get: {},
  goal_journal: {},
  todo_op: { op: 'string', task: 'string?', phase: 'string?', items: 'string[]?', dependsOn: 'string[]?', cascade: 'boolean?' },
  todo_child_create: { label: 'string?', idempotencyKey: 'string?' },
  loop_create: { interval: 'string', prompt: 'string', fireImmediately: 'boolean', durable: 'boolean' },
  loop_list: {},
  loop_update: { taskId: 'string', interval: 'string?', prompt: 'string?' },
  loop_delete: { taskId: 'string' },
  loop_cancel: { taskId: 'string' },
  job_list: {},
  job_cancel: { jobIds: 'string[]' },
  file_explorer_list: { path: 'string?' },
  file_explorer_read: { path: 'string', cursor: 'number?', maxLines: 'number?' },
  file_explorer_changes: {},
  file_explorer_diff: { path: 'string', cursor: 'number?', maxLines: 'number?' },
  file_explorer_blame: { path: 'string', startLine: 'number', endLine: 'number' },
  code_review_snapshot: {},
  tmux_list_sessions: {},
  tmux_pane_snapshot: { session: 'string', lines: 'number?' },
  tmux_send_keys: {
    session: 'string',
    text: 'string?',
    key: 'enum:enter|escape|ctrl_c|ctrl_d|tab|up|down|left|right?',
  },
  tmux_session_create: { session: 'string' },
  tmux_session_delete: { session: 'string' },
  tmux_terminal_attach: { session: 'string', cols: 'number', rows: 'number' },
  tmux_terminal_input: { terminalId: 'string', data: 'string' },
  tmux_terminal_resize: { terminalId: 'string', cols: 'number', rows: 'number' },
  tmux_terminal_detach: { terminalId: 'string' },
  get_discovered_runners: {},
  queue_list: {},
  queue_send_now: {},
  queue_cancel: { scope: 'enum:all|steering?', index: 'number?' },
  code_review_open: { from: 'string?', to: 'string?', includeWorkingTree: 'boolean?', commit: 'string?' },
  code_review_refresh: {},
  code_review_comment: {
    snapshotId: 'string',
    path: 'string',
    oldStart: 'number',
    oldCount: 'number',
    newStart: 'number',
    newCount: 'number',
    contentHash: 'string',
    comment: 'string',
    images: 'array?',
  },
  code_review_abort: {
    snapshotId: 'string',
    path: 'string',
    oldStart: 'number',
    oldCount: 'number',
    newStart: 'number',
    newCount: 'number',
    contentHash: 'string',
  },
  code_review_close: { reviewId: 'string?' },
  code_review_file_diff: { snapshotId: 'string', path: 'string', cursor: 'number?', maxLines: 'number?' },
  code_review_file_context: {
    snapshotId: 'string',
    path: 'string',
    oldStart: 'number?',
    newStart: 'number?',
    count: 'number?',
  },
  code_review_set_model: { modelId: 'string' },
  code_review_commits: { limit: 'number?' },
  code_review_tldr: { language: 'string?' },
  code_review_stop: { operation: 'enum:autoReview|highlight|tldr' },
  settings_inspect: {},
  settings_search: { query: 'string' },
  settings_open_draft: { scope: 'enum:global|project' },
  settings_get_draft: { draftId: 'string' },
  settings_set: { draftId: 'string', key: 'string', value: 'any' },
  settings_reset: { draftId: 'string', key: 'string' },
  settings_validate: { draftId: 'string' },
  settings_apply: { draftId: 'string' },
  settings_cancel: { draftId: 'string' },
  persona_list: {},
  persona_get: { name: 'string' },
  persona_create: { name: 'string', content: 'string' },
  persona_edit: { name: 'string', content: 'string' },
  persona_remove: { name: 'string', confirm: 'boolean?' },
  persona_purge: { name: 'string', confirm: 'boolean?' },
  persona_select: { name: 'string' },
  persona_clear: {},
  persona_current: {},
  process_spawn: { spec: 'object' },
  process_list: {},
  process_describe: { processId: 'string' },
  process_logs: { processId: 'string', cursor: 'number?', limitBytes: 'number?' },
  process_write: { processId: 'string', dataBase64: 'string' },
  process_keys: { processId: 'string', keys: 'string[]' },
  process_resize: { processId: 'string', cols: 'number', rows: 'number' },
  process_signal: { processId: 'string', signal: 'enum:SIGINT|SIGTERM|SIGHUP|SIGQUIT|SIGKILL' },
  process_stop: { processId: 'string' },
  process_wait: { processId: 'string', timeoutMs: 'number?' },
  workflow_create: { name: 'string', objective: 'string' },
  workflow_list: {},
  workflow_get: { workflowId: 'string?', name: 'string?' },
  workflow_detail: { workflowId: 'string?', name: 'string?' },
  workflow_pause: { workflowId: 'string' },
  workflow_resume: { workflowId: 'string' },
  workflow_cancel: { workflowId: 'string' },
  workflow_integrate: { workflowId: 'string' },
  workflow_remove: { workflowId: 'string' },
  workflow_hub_send: {
    workflowId: 'string',
    generation: 'number',
    recipient: 'string',
    body: 'string',
    replyTo: 'string?',
  },
  side_chat_new: { name: 'string' },
  side_chat_switch: { name: 'string' },
  side_chat_close: { name: 'string?' },
  side_chat_prompt: { message: 'string', images: 'array?' },
  side_chat_list: {},
  side_chat_abort: {},
  file_conversation_open: { conversationId: 'string', path: 'string' },
  file_conversation_prompt: { conversationId: 'string', message: 'string', selection: 'object?' },
  file_conversation_snapshot: { conversationId: 'string' },
  file_conversation_abort: { conversationId: 'string' },
  file_conversation_close: { conversationId: 'string' },
  todo_child_list: { parentSessionId: 'string' },
  todo_child_select: { parentSessionId: 'string', childSessionId: 'string' },
  todo_child_send: { parentSessionId: 'string', childSessionId: 'string?', message: 'string' },
  todo_child_status: { parentSessionId: 'string', childSessionId: 'string?' },
  todo_child_stop: { parentSessionId: 'string', childSessionId: 'string?' },
  todo_child_pause: { parentSessionId: 'string', childSessionId: 'string?' },
  todo_child_resume: { parentSessionId: 'string', childSessionId: 'string?' },
  todo_child_cancel: { parentSessionId: 'string', childSessionId: 'string?' },
  todo_child_archive: { parentSessionId: 'string', childSessionId: 'string' },
  todo_child_remove: { parentSessionId: 'string', childSessionId: 'string' },
  todo_child_add_task: {
    parentSessionId: 'string',
    childSessionId: 'string',
    content: 'string',
    dependsOn: 'string[]?',
    agent: 'string?',
    role: 'string?',
    expectedRevision: 'number?',
  },
  todo_child_detail: { parentSessionId: 'string', childSessionId: 'string' },
  todo_child_hub_send: {
    parentSessionId: 'string',
    childSessionId: 'string',
    to: 'string',
    body: 'string',
    replyTo: 'string?',
  },
  session_handoff: { customInstructions: 'string?', responseMode: 'enum:subscribe-handoff-v1?' },
  get_fork_messages: { sessionPath: 'string?', cursor: 'string?', limit: 'number?' },
  session_copy_context: { sessionPath: 'string' },
  export_html: { outputPath: 'string?', sessionPath: 'string?' },
  path_complete: { path: 'string', kind: 'enum:directory|file|any', limit: 'number?' },
  file_explorer_stage_download: { path: 'string' },
  job_output: { jobId: 'string' },
  agent_history: { agentId: 'string', lines: 'number?' },
  task_spawn: { args: 'any' },
  hub_send: { to: 'string', body: 'string', replyTo: 'string?' },
} satisfies {
  [K in RunnerCommand['type']]: Readonly<
    Record<Exclude<keyof Extract<RunnerCommand, { type: K }>, 'type'>, WireFieldSpec>
  > & { type?: never };
};

/** Kinds accepted by the guard; `any` admits every JSON value. */
function wireFieldMatches(value: unknown, spec: WireFieldSpec): boolean {
  const kind = spec.endsWith('?') ? spec.slice(0, -1) : spec;
  if (kind.startsWith('enum:')) {
    return typeof value === 'string' && kind.slice('enum:'.length).split('|').includes(value);
  }
  switch (kind) {
    case 'string':
      return typeof value === 'string';
    case 'number':
      // Every numeric wire field is an unsigned Rust integer (`u16`..`u64`,
      // `usize`): a negative or fractional value fails deserialization.
      return typeof value === 'number' && Number.isSafeInteger(value) && value >= 0;
    case 'boolean':
      return typeof value === 'boolean';
    case 'string[]':
      return Array.isArray(value) && value.every((item) => typeof item === 'string');
    case 'array':
      return Array.isArray(value);
    case 'object':
      return value !== null && typeof value === 'object' && !Array.isArray(value);
    default:
      return true;
  }
}

/** Fail-closed runtime gate for one outbound command frame.
 *
 *  The union is a compile-time contract; a frame assembled dynamically (a
 *  panel forwarding an action, a command rebuilt from persisted state) must be
 *  validated before dispatch instead of cast, so a malformed frame never
 *  reaches the runner. Unknown EXTRA keys stay permitted: the transport stamps
 *  `id`/`sessionId` there and the runner ignores what it does not declare. */
export function parseRunnerCommand(value: unknown): RunnerCommand | null {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return null;
  const frame = value as Record<string, unknown>;
  const type = frame.type;
  if (typeof type !== 'string' || !Object.prototype.hasOwnProperty.call(RUNNER_COMMAND_WIRE, type)) {
    return null;
  }
  const spec: Readonly<Record<string, WireFieldSpec>> = RUNNER_COMMAND_WIRE[type as RunnerCommand['type']];
  for (const field of Object.keys(spec)) {
    const fieldSpec = spec[field] as WireFieldSpec;
    const present = Object.prototype.hasOwnProperty.call(frame, field) && frame[field] !== undefined;
    if (!present) {
      if (!fieldSpec.endsWith('?')) return null;
      continue;
    }
    if (!wireFieldMatches(frame[field], fieldSpec)) return null;
  }
  return value as RunnerCommand;
}

/** Record view of a payload; a non-object (or an array) fails closed. */
function wireRecord(value: unknown): Record<string, unknown> | null {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return null;
  return value as Record<string, unknown>;
}

/** Wire count: a non-negative safe integer, else null. Absent, fractional,
 *  NaN, and negative values are all "no count" — never a coerced guess. */
function wireCount(value: unknown): number | null {
  return typeof value === 'number' && Number.isSafeInteger(value) && value >= 0 ? value : null;
}

/** Parse every row of a wire list, or null when the payload is not an array or
 *  ANY row is malformed: a panel never renders a half-valid list. */
function wireList<T>(value: unknown, parse: (row: unknown) => T | null): T[] | null {
  if (!Array.isArray(value)) return null;
  const rows: T[] = [];
  for (const item of value) {
    const row = parse(item);
    if (row === null) return null;
    rows.push(row);
  }
  return rows;
}

/** `queue_send_now` data: queued steering prompts combined into one turn. */
export interface SteeringSendNowData {
  sent: number;
  combined: boolean;
}

export function parseSteeringSendNow(data: unknown): SteeringSendNowData | null {
  const value = wireRecord(data);
  if (!value) return null;
  const sent = wireCount(value.sent);
  if (sent === null || typeof value.combined !== 'boolean') return null;
  return { sent, combined: value.combined };
}

/** `queue_cancel` data: queued prompts the drain removed. */
export interface QueueCancelData {
  cancelled: number;
}

export function parseQueueCancelResult(data: unknown): QueueCancelData | null {
  const value = wireRecord(data);
  const cancelled = value ? wireCount(value.cancelled) : null;
  return cancelled === null ? null : { cancelled };
}

/** Canonical success row of a lifecycle command (`new_session`,
 *  `switch_session`, `fork`, `clone`, `session_fork_latest`): the target
 *  identity plus the authoritative row it resolved to. The /web
 *  `lifecycleTargetRow` gate, minus the envelope checks the transport already
 *  made: the state block, the row, and the identity must agree, the row must
 *  be a LOADED native `rpi` row, and `path` must be the state's session file.
 *  Any drift fails closed instead of adopting a foreign target. */
export interface LifecycleTargetData {
  sessionId: string;
  runtimeIncarnation: string;
  path: string;
  cwd: string;
}

export function parseLifecycleTarget(data: unknown): LifecycleTargetData | null {
  const value = wireRecord(data);
  if (!value) return null;
  const sessionId = readNonemptyString(value.sessionId);
  const runtimeIncarnation = readNonemptyString(value.runtimeIncarnation);
  const state = wireRecord(value.state);
  const row = wireRecord(value.session);
  if (sessionId === null || runtimeIncarnation === null || !state || !row) return null;
  const sessionFile = readNonemptyString(state.sessionFile);
  const cwd = readNonemptyString(state.cwd);
  if (sessionFile === null || cwd === null || state.sessionId !== sessionId) return null;
  if (row.sessionId !== sessionId || row.runtimeIncarnation !== runtimeIncarnation) return null;
  if (row.path !== sessionFile || row.cwd !== cwd) return null;
  if (row.status !== 'native' || row.loaded !== true || row.source !== 'rpi') return null;
  return { sessionId, runtimeIncarnation, path: sessionFile, cwd };
}

/** `session_handoff` envelope: the same subscribe-handoff-v1 shape the catalog
 *  switch consumes, but self-describing (the backend mints the new session),
 *  so the native `rpi` source/status, the identity agreement, the subscription
 *  requirement, and the response version are all pinned here. */
export interface SessionHandoffData {
  sessionId: string;
  runtimeIncarnation: string;
  catalogSessionId: string;
  cursor: number;
  notice: string | null;
}

export function parseSessionHandoff(data: unknown): SessionHandoffData | null {
  const value = wireRecord(data);
  if (!value) return null;
  if ('state' in value || 'messages' in value) return null;
  if (value.catalogSource !== 'rpi' || value.catalogStatus !== 'native') return null;
  if (value.subscriptionRequired !== true || value.lifecycleResponseVersion !== 1) return null;
  const sessionId = readNonemptyString(value.sessionId);
  const runtimeIncarnation = readNonemptyString(value.runtimeIncarnation);
  const catalogSessionId = readNonemptyString(value.catalogSessionId);
  const cursor = wireCount(value.cursor);
  if (sessionId === null || runtimeIncarnation === null || catalogSessionId !== sessionId) return null;
  if (cursor === null) return null;
  if (value.notice !== undefined && value.notice !== null && typeof value.notice !== 'string') return null;
  return {
    sessionId,
    runtimeIncarnation,
    catalogSessionId,
    cursor,
    notice: typeof value.notice === 'string' ? value.notice : null,
  };
}

/** One forkable user message of a `get_fork_messages` page. */
export interface ForkMessageWire {
  entryId: string;
  text: string;
}

export interface ForkMessagesPageData {
  messages: ForkMessageWire[];
  nextCursor: string | null;
  truncated: boolean;
  total: number | null;
}

/** Bounded `get_fork_messages` page. A payload that is not an object carrying a
 *  `messages` ARRAY is not a page at all — the backend always serializes that
 *  key (`ForkMessagePage`) — and a page whose rows are not all well-formed
 *  fork candidates is not trustworthy either: answering with a truncated list
 *  would silently overwrite the loaded list's cursor and could offer the wrong
 *  fork candidate, so both fail closed. Duplicate entry ids inside a real page
 *  are dropped (the same candidate is already listed) and a cursor alone
 *  already means truncated. */
export function parseForkMessagesPage(data: unknown): ForkMessagesPageData | null {
  const value = wireRecord(data);
  if (!value || !Array.isArray(value.messages)) return null;
  const rows = value.messages;
  const messages: ForkMessageWire[] = [];
  const seen = new Set<string>();
  for (const item of rows) {
    const row = wireRecord(item);
    const entryId = row ? readNonemptyString(row.entryId) : null;
    if (!row || entryId === null || typeof row.text !== 'string') return null;
    if (seen.has(entryId)) continue;
    seen.add(entryId);
    messages.push({ entryId, text: row.text });
  }
  const nextCursor = readNonemptyString(value.nextCursor);
  return {
    messages,
    nextCursor,
    truncated: value.truncated === true || nextCursor !== null,
    total: wireCount(value.total),
  };
}

export const SESSION_COPY_CONTEXT_FORMAT = 'pi-session-context/1';
export const SESSION_COPY_CONTEXT_MAX_BYTES = 65_536;

export interface SessionCopyContextData {
  text: string;
  truncated: boolean;
}

/** Bounded `session_copy_context` artifact. The /web contract — format stamp,
 *  fixed first three header lines that may not recur, byte cap — is enforced
 *  here as null instead of a throw: a drifted artifact never reaches a
 *  clipboard. */
export function parseSessionCopyContext(data: unknown): SessionCopyContextData | null {
  const value = wireRecord(data);
  if (!value || !('text' in value) || !('format' in value) || !('truncated' in value)) return null;
  const { text, truncated } = value;
  if (typeof text !== 'string' || value.format !== SESSION_COPY_CONTEXT_FORMAT) return null;
  if (typeof truncated !== 'boolean') return null;
  if (new TextEncoder().encode(text).byteLength > SESSION_COPY_CONTEXT_MAX_BYTES) return null;
  const lines = text.split(/\r?\n/);
  if (lines[0] !== '# Pi session context') return null;
  if (lines[1] !== `Format: ${SESSION_COPY_CONTEXT_FORMAT}`) return null;
  if (lines[2] !== `Truncated: ${truncated ? 'yes' : 'no'}`) return null;
  if (lines.slice(3).some((line) => line.startsWith('Format:') || line.startsWith('Truncated:'))) return null;
  return { text, truncated };
}

export interface StagedDownloadData {
  downloadId: string;
  fileName: string;
}

/** One-shot download staging: `export_html` and
 *  `file_explorer_stage_download` both answer with the download id and the
 *  save name the download route serves. */
export function parseStagedDownload(data: unknown): StagedDownloadData | null {
  const value = wireRecord(data);
  if (!value) return null;
  const downloadId = readNonemptyString(value.downloadId);
  const fileName = readNonemptyString(value.fileName);
  return downloadId === null || fileName === null ? null : { downloadId, fileName };
}

/** Response cap of the frozen `path_complete` contract. */
export const PATH_COMPLETE_LIMIT = 100;

export type PathCompletionEntryKind = 'directory' | 'file';

export interface PathCompletionEntry {
  name: string;
  path: string;
  kind: PathCompletionEntryKind;
}

export interface PathCompletionData {
  base: string;
  prefix: string;
  entries: PathCompletionEntry[];
  truncated: boolean;
}

/** `path_complete` reply: entries beyond the cap, and entries of an unknown
 *  kind, are dropped; a malformed payload yields null so completion stays
 *  quiet and manual entry is never blocked. */
export function parsePathCompletion(data: unknown): PathCompletionData | null {
  const value = wireRecord(data);
  if (!value || !Array.isArray(value.entries)) return null;
  if (typeof value.base !== 'string' || typeof value.prefix !== 'string') return null;
  if (typeof value.truncated !== 'boolean') return null;
  const entries: PathCompletionEntry[] = [];
  for (const item of value.entries) {
    const row = wireRecord(item);
    if (!row || typeof row.name !== 'string' || typeof row.path !== 'string') continue;
    if (row.kind !== 'directory' && row.kind !== 'file') continue;
    entries.push({ name: row.name, path: row.path, kind: row.kind });
  }
  if (entries.length > PATH_COMPLETE_LIMIT) {
    return {
      base: value.base,
      prefix: value.prefix,
      entries: entries.slice(0, PATH_COMPLETE_LIMIT),
      truncated: true,
    };
  }
  return { base: value.base, prefix: value.prefix, entries, truncated: value.truncated };
}

export interface TmuxSendResultData {
  session: string;
  sent: true;
}

/** `tmux_send_keys` data: the echoed session and the literal `sent: true`
 *  marker — a bare echo without it fails closed. */
export function parseTmuxSendResult(data: unknown): TmuxSendResultData | null {
  const value = wireRecord(data);
  if (!value) return null;
  const session = readNonemptyString(value.session);
  return session === null || value.sent !== true ? null : { session, sent: true };
}

export type JobWireStatus = 'queued' | 'running' | 'completed' | 'failed' | 'cancelled';

/** Settled job result. `output` is the delivered yield payload as text. */
export interface JobResultWire {
  index: number;
  id: string;
  agent: string;
  status: string;
  output: string;
  usage: { promptTokens: number | null; completionTokens: number | null; totalTokens: number | null } | null;
  error: string | null;
  artifactRef: string;
  historyRef: string;
  artifactUri: string;
}

/** One orchestration job (panels/SubagentsPanel.tsx). */
export interface JobWire {
  id: string;
  agentId: string;
  agent: string;
  parentId: string;
  description: string | null;
  todoTaskId: string | null;
  workflowId: string | null;
  workflowGeneration: number | null;
  status: JobWireStatus;
  createdAt: number;
  startedAt: number | null;
  finishedAt: number | null;
  result: JobResultWire | null;
  softBudgetExhausted: boolean;
}

const JOB_STATUSES: Record<string, true> = {
  queued: true,
  running: true,
  completed: true,
  failed: true,
  cancelled: true,
};

/** Optional wire string: absent, null, and empty all read as null. */
function wireOptionalString(value: unknown): string | null {
  return typeof value === 'string' && value !== '' ? value : null;
}

function parseJobResult(data: unknown): JobResultWire | null {
  const value = wireRecord(data);
  if (!value) return null;
  const index = wireCount(value.index);
  const id = readNonemptyString(value.id);
  const agent = typeof value.agent === 'string' ? value.agent : null;
  const status = typeof value.status === 'string' ? value.status : null;
  const output = typeof value.output === 'string' ? value.output : null;
  if (index === null || id === null || agent === null || status === null || output === null) return null;
  if (typeof value.artifactRef !== 'string' || typeof value.historyRef !== 'string') return null;
  if (typeof value.artifactUri !== 'string') return null;
  if (value.error !== undefined && value.error !== null && typeof value.error !== 'string') return null;
  const usage = wireRecord(value.usage);
  return {
    index,
    id,
    agent,
    status,
    output,
    usage: usage
      ? {
          promptTokens: wireCount(usage.promptTokens),
          completionTokens: wireCount(usage.completionTokens),
          totalTokens: wireCount(usage.totalTokens),
        }
      : null,
    error: wireOptionalString(value.error),
    artifactRef: value.artifactRef,
    historyRef: value.historyRef,
    artifactUri: value.artifactUri,
  };
}

function parseJob(data: unknown): JobWire | null {
  const value = wireRecord(data);
  if (!value) return null;
  const id = readNonemptyString(value.id);
  const agentId = readNonemptyString(value.agentId);
  const agent = typeof value.agent === 'string' ? value.agent : null;
  const parentId = typeof value.parentId === 'string' ? value.parentId : null;
  const status = typeof value.status === 'string' && JOB_STATUSES[value.status] ? value.status : null;
  const createdAt = wireCount(value.createdAt);
  if (id === null || agentId === null || agent === null || parentId === null) return null;
  if (status === null || createdAt === null) return null;
  const startedAt = wireCount(value.startedAt);
  const finishedAt = wireCount(value.finishedAt);
  const workflowGeneration = wireCount(value.workflowGeneration);
  const result = value.result === undefined || value.result === null ? null : parseJobResult(value.result);
  if (value.result !== undefined && value.result !== null && !result) return null;
  return {
    id,
    agentId,
    agent,
    parentId,
    description: wireOptionalString(value.description),
    todoTaskId: wireOptionalString(value.todoTaskId),
    workflowId: wireOptionalString(value.workflowId),
    workflowGeneration,
    status: status as JobWireStatus,
    createdAt,
    startedAt,
    finishedAt,
    result,
    softBudgetExhausted: value.softBudgetExhausted === true,
  };
}

/** `job_output` data: one job snapshot (settled `result.output` is the yield). */
export interface JobOutputData {
  job: JobWire;
}

export function parseJobOutput(data: unknown): JobOutputData | null {
  const value = wireRecord(data);
  const job = value ? parseJob(value.job) : null;
  return job === null ? null : { job };
}

/** `agent_history` data: one child agent's bounded, redacted transcript. */
export interface AgentHistoryData {
  agentId: string;
  text: string;
}

export function parseAgentHistory(data: unknown): AgentHistoryData | null {
  const value = wireRecord(data);
  if (!value) return null;
  const agentId = readNonemptyString(value.agentId);
  if (agentId === null || typeof value.text !== 'string') return null;
  return { agentId, text: value.text };
}

/** Spawned job row of a `task_spawn` batch (`spawns`). */
export interface SpawnedJobWire {
  jobId: string | null;
  agentId: string | null;
}

/** `task_spawn` data: the batch's spawned jobs. Rows that name neither a job
 *  nor an agent are dropped; a payload without a `spawns` array is an empty
 *  batch, never an invented one. */
export function parseTaskSpawnSpawns(data: unknown): SpawnedJobWire[] {
  const value = wireRecord(data);
  const rows = value && Array.isArray(value.spawns) ? value.spawns : [];
  const spawns: SpawnedJobWire[] = [];
  for (const item of rows) {
    const row = wireRecord(item);
    if (!row) continue;
    const jobId = wireOptionalString(row.jobId);
    const agentId = wireOptionalString(row.agentId);
    if (jobId === null && agentId === null) continue;
    spawns.push({ jobId, agentId });
  }
  return spawns;
}

/** One delivery receipt of a hub send (IRC). */
export interface HubReceiptWire {
  to: string;
  outcome: string;
  messageId: string | null;
  error: string | null;
}

function parseHubReceipt(data: unknown): HubReceiptWire | null {
  const value = wireRecord(data);
  if (!value) return null;
  const to = readNonemptyString(value.to);
  const outcome = readNonemptyString(value.outcome);
  if (to === null || outcome === null) return null;
  if (value.messageId !== undefined && value.messageId !== null && typeof value.messageId !== 'string') return null;
  if (value.error !== undefined && value.error !== null && typeof value.error !== 'string') return null;
  return {
    to,
    outcome,
    messageId: wireOptionalString(value.messageId),
    error: wireOptionalString(value.error),
  };
}

/** `hub_send` data: one receipt per recipient. */
export interface HubSendResultData {
  receipts: HubReceiptWire[];
}

export function parseHubSendResult(data: unknown): HubSendResultData | null {
  const value = wireRecord(data);
  const receipts = value ? wireList(value.receipts, parseHubReceipt) : null;
  return receipts === null ? null : { receipts };
}

/** `workflow_hub_send` data: the echoed recipient plus its receipts. */
export interface WorkflowHubSendResultData {
  recipient: string;
  receipts: HubReceiptWire[];
}

export function parseWorkflowHubSendResult(data: unknown): WorkflowHubSendResultData | null {
  const value = wireRecord(data);
  if (!value) return null;
  const recipient = readNonemptyString(value.recipient);
  const receipts = wireList(value.receipts, parseHubReceipt);
  return recipient === null || receipts === null ? null : { recipient, receipts };
}

/** `todo_child_hub_send` data: the bare receipt list the child's send answers. */
export function parseHubReceiptList(data: unknown): HubReceiptWire[] | null {
  return wireList(data, parseHubReceipt);
}

/** One loaded persona (crates/pi-cli personas.ts wire shape). Counts and
 *  `stateError` are persona state kept under the persona root; they are never
 *  path text. */
export interface PersonaRowData {
  name: string;
  description: string;
  source: string;
  trusted: boolean;
  preferred: boolean;
  contractSummary: string;
  memoryEntries: number | null;
  sessionCount: number | null;
  stateError: string | null;
}

export interface PersonaDetailData extends PersonaRowData {
  content: string;
  contentTruncated: boolean;
}

function parsePersonaRow(data: unknown): PersonaRowData | null {
  const value = wireRecord(data);
  if (!value) return null;
  const name = readNonemptyString(value.name);
  if (name === null) return null;
  if (typeof value.description !== 'string' || typeof value.source !== 'string') return null;
  if (typeof value.contractSummary !== 'string') return null;
  if (typeof value.trusted !== 'boolean' || typeof value.preferred !== 'boolean') return null;
  if (value.memoryEntries !== null && wireCount(value.memoryEntries) === null) return null;
  if (value.sessionCount !== null && wireCount(value.sessionCount) === null) return null;
  if (value.stateError !== null && typeof value.stateError !== 'string') return null;
  return {
    name,
    description: value.description,
    source: value.source,
    trusted: value.trusted,
    preferred: value.preferred,
    contractSummary: value.contractSummary,
    memoryEntries: wireCount(value.memoryEntries),
    sessionCount: wireCount(value.sessionCount),
    stateError: wireOptionalString(value.stateError),
  };
}

/** `persona_list` rows: a malformed row is dropped, never rendered. */
export function parsePersonaList(data: unknown): PersonaRowData[] {
  const value = wireRecord(data);
  if (!value || !Array.isArray(value.personas)) return [];
  const rows: PersonaRowData[] = [];
  for (const item of value.personas) {
    const row = parsePersonaRow(item);
    if (row) rows.push(row);
  }
  return rows;
}

/** `persona_get` detail: the row plus its bounded definition content. */
export function parsePersonaDetail(data: unknown): PersonaDetailData | null {
  const row = parsePersonaRow(data);
  const value = wireRecord(data);
  if (!row || !value) return null;
  if (typeof value.content !== 'string' || typeof value.contentTruncated !== 'boolean') return null;
  return { ...row, content: value.content, contentTruncated: value.contentTruncated };
}

/** Human-facing `message` of a persona mutation response. */
export function parsePersonaMessage(data: unknown): string | null {
  const value = wireRecord(data);
  return value ? wireOptionalString(value.message) : null;
}

/** One supervised process (panels/ProcessesPanel.tsx, the `/ps` mirror). */
export interface ProcessInfoData {
  id: string;
  label: string | null;
  state: string;
  pid: number | null;
  exitCode: number | null;
}

/** `process_list` rows. The /web panel coercively defaults each field; here a
 *  row without an id is dropped and an unknown state reads `unknown` rather
 *  than a fabricated live state. */
export function parseProcessList(data: unknown): ProcessInfoData[] | null {
  if (!Array.isArray(data)) return null;
  const processes: ProcessInfoData[] = [];
  for (const item of data) {
    const row = wireRecord(item);
    if (!row) continue;
    const rawId = typeof row.id === 'string' || typeof row.id === 'number' ? String(row.id) : null;
    if (rawId === null || rawId === '') continue;
    processes.push({
      id: rawId,
      label: wireOptionalString(row.label),
      state: readNonemptyString(row.state) ?? 'unknown',
      pid: wireCount(row.pid),
      exitCode: typeof row.exitCode === 'number' && Number.isInteger(row.exitCode) ? row.exitCode : null,
    });
  }
  return processes;
}

/** One captured output chunk of a supervised process. `dataBase64` stays
 *  base64 — the caller owns decoding and the retry cursor. */
export interface ProcessLogChunkData {
  startCursor: number;
  cursor: number;
  stream: string;
  dataBase64: string;
}

/** `process_logs` page. `cursor` is the position to resume from and `eof`
 *  marks the current end of the stream; `lostBytes` is what the ring buffer
 *  dropped before `startCursor`. */
export interface ProcessLogsData {
  requestedCursor: number;
  startCursor: number;
  cursor: number;
  eof: boolean;
  lost: boolean;
  lostBytes: number;
  chunks: ProcessLogChunkData[];
}

function parseProcessLogChunk(data: unknown): ProcessLogChunkData | null {
  const value = wireRecord(data);
  if (!value) return null;
  const startCursor = wireCount(value.startCursor);
  const cursor = wireCount(value.cursor);
  const stream = readNonemptyString(value.stream);
  if (startCursor === null || cursor === null || stream === null) return null;
  if (typeof value.dataBase64 !== 'string') return null;
  return { startCursor, cursor, stream, dataBase64: value.dataBase64 };
}

export function parseProcessLogs(data: unknown): ProcessLogsData | null {
  const value = wireRecord(data);
  if (!value) return null;
  const requestedCursor = wireCount(value.requestedCursor);
  const startCursor = wireCount(value.startCursor);
  const cursor = wireCount(value.cursor);
  const lostBytes = wireCount(value.lostBytes);
  const chunks = wireList(value.chunks, parseProcessLogChunk);
  if (requestedCursor === null || startCursor === null || cursor === null) return null;
  if (lostBytes === null || chunks === null) return null;
  if (typeof value.eof !== 'boolean' || typeof value.lost !== 'boolean') return null;
  return {
    requestedCursor,
    startCursor,
    cursor,
    eof: value.eof,
    lost: value.lost,
    lostBytes,
    chunks,
  };
}

/** Typed contract of one setting's value (kind-tagged; the extra fields are
 *  per-kind, e.g. `nonEmpty` for a string, `min`/`max` for an integer). */
export interface SettingValueTypeData {
  kind: string;
  nonEmpty: boolean;
  min: number | null;
  max: number | null;
}

export interface SettingDefinitionData {
  key: string;
  category: string;
  valueType: SettingValueTypeData;
  defaultJson: string;
  description: string;
  enumValues: string[];
  scopes: string;
  behavior: string;
  secret: boolean;
  trustSensitive: boolean;
}

/** One effective/provenance row of the settings catalog or a draft. */
export interface SettingValueViewData {
  definition: SettingDefinitionData;
  effectiveValue: unknown;
  source: string;
  globalValue: unknown;
  projectValue: unknown;
  sessionOverrideValue: unknown;
  editableGlobal: boolean;
  editableProject: boolean;
  redacted: boolean;
}

export interface SettingsCatalogData {
  projectTrusted: boolean;
  globalPath: string;
  projectPath: string;
  values: SettingValueViewData[];
}

export interface SettingsDraftData {
  draftId: string;
  scope: string;
  dirty: boolean;
  values: SettingValueViewData[];
}

export interface SettingApplyResultData {
  key: string;
  needsReload: boolean;
  needsRestart: boolean;
}

export interface SettingsApplyOutcomeData {
  appliedLive: boolean;
  reloaded: boolean;
  restartRequired: boolean;
  results: SettingApplyResultData[];
}

function parseSettingValueType(data: unknown): SettingValueTypeData | null {
  const value = wireRecord(data);
  if (!value) return null;
  const kind = readNonemptyString(value.kind);
  if (kind === null) return null;
  return {
    kind,
    nonEmpty: value.nonEmpty === true,
    min: typeof value.min === 'number' && Number.isFinite(value.min) ? value.min : null,
    max: typeof value.max === 'number' && Number.isFinite(value.max) ? value.max : null,
  };
}

function parseSettingDefinition(data: unknown): SettingDefinitionData | null {
  const value = wireRecord(data);
  if (!value) return null;
  const key = readNonemptyString(value.key);
  const category = readNonemptyString(value.category);
  const valueType = parseSettingValueType(value.valueType);
  if (key === null || category === null || !valueType) return null;
  if (typeof value.defaultJson !== 'string' || typeof value.description !== 'string') return null;
  if (typeof value.scopes !== 'string' || typeof value.behavior !== 'string') return null;
  if (typeof value.secret !== 'boolean' || typeof value.trustSensitive !== 'boolean') return null;
  const enumValues = wireList(value.enumValues, (item) => (typeof item === 'string' ? item : null));
  if (enumValues === null) return null;
  return {
    key,
    category,
    valueType,
    defaultJson: value.defaultJson,
    description: value.description,
    enumValues,
    scopes: value.scopes,
    behavior: value.behavior,
    secret: value.secret,
    trustSensitive: value.trustSensitive,
  };
}

function parseSettingValueView(data: unknown): SettingValueViewData | null {
  const value = wireRecord(data);
  if (!value) return null;
  const definition = parseSettingDefinition(value.definition);
  const source = readNonemptyString(value.source);
  if (!definition || source === null) return null;
  if (!('effectiveValue' in value) || !('globalValue' in value)) return null;
  if (!('projectValue' in value) || !('sessionOverrideValue' in value)) return null;
  if (typeof value.editableGlobal !== 'boolean' || typeof value.editableProject !== 'boolean') return null;
  if (typeof value.redacted !== 'boolean') return null;
  return {
    definition,
    effectiveValue: value.effectiveValue,
    source,
    globalValue: value.globalValue,
    projectValue: value.projectValue,
    sessionOverrideValue: value.sessionOverrideValue,
    editableGlobal: value.editableGlobal,
    editableProject: value.editableProject,
    redacted: value.redacted,
  };
}

/** `settings_inspect` snapshot: the definitions, their effective values, and
 *  the provenance of each. */
export function parseSettingsCatalog(data: unknown): SettingsCatalogData | null {
  const value = wireRecord(data);
  if (!value) return null;
  const values = wireList(value.values, parseSettingValueView);
  if (values === null || typeof value.projectTrusted !== 'boolean') return null;
  if (typeof value.globalPath !== 'string' || typeof value.projectPath !== 'string') return null;
  return {
    projectTrusted: value.projectTrusted,
    globalPath: value.globalPath,
    projectPath: value.projectPath,
    values,
  };
}

/** Server-held settings draft (`settings_open_draft` / `_get_draft` /
 *  `_set` / `_reset` / `_validate`). */
export function parseSettingsDraft(data: unknown): SettingsDraftData | null {
  const value = wireRecord(data);
  if (!value) return null;
  const draftId = readNonemptyString(value.draftId);
  const values = wireList(value.values, parseSettingValueView);
  if (draftId === null || values === null || typeof value.scope !== 'string') return null;
  if (typeof value.dirty !== 'boolean') return null;
  return { draftId, scope: value.scope, dirty: value.dirty, values };
}

function parseSettingApplyResult(data: unknown): SettingApplyResultData | null {
  const value = wireRecord(data);
  if (!value) return null;
  const key = readNonemptyString(value.key);
  if (key === null || typeof value.needsReload !== 'boolean' || typeof value.needsRestart !== 'boolean') return null;
  return { key, needsReload: value.needsReload, needsRestart: value.needsRestart };
}

/** Rust serializes settings changes as writes with explicit apply outcome flags. */
export function parseSettingsApplyOutcome(data: unknown): SettingsApplyOutcomeData | null {
  const value = wireRecord(data);
  if (!value || typeof value.appliedLive !== 'boolean' || typeof value.reloaded !== 'boolean'
    || typeof value.restartRequired !== 'boolean') return null;
  const results = wireList(value.writes, parseSettingApplyResult);
  if (results === null) return null;
  if (results.some((row) => row.needsReload) && !value.reloaded) return null;
  return {
    appliedLive: value.appliedLive,
    reloaded: value.reloaded,
    restartRequired: value.restartRequired,
    results,
  };
}

export type WorkflowStatusWire =
  | 'queued'
  | 'planning'
  | 'running'
  | 'paused'
  | 'integrating'
  | 'completed'
  | 'failed'
  | 'cancelled'
  | 'conflicted';

const WORKFLOW_STATUSES: Record<string, true> = {
  queued: true,
  planning: true,
  running: true,
  paused: true,
  integrating: true,
  completed: true,
  failed: true,
  cancelled: true,
  conflicted: true,
};

/** List/create/action row of a workflow (workflow_rpc::project_workflow_snapshot).
 *  `worktree` is already reduced to a display-safe label and `integration` is
 *  the wire summary (`applied:<commit>` / `conflicted:<paths>`). */
export interface WorkflowSnapshotData {
  workflowId: string;
  name: string;
  objective: string;
  status: WorkflowStatusWire;
  generation: number;
  worktree: string | null;
  branch: string | null;
  supervisorAgentId: string | null;
  supervisorJobId: string | null;
  failure: string | null;
  integration: string | null;
}

export interface WorkflowAgentActivityWire {
  atMs: number;
  kind: 'task' | 'irc';
  text: string;
}

export interface WorkflowActorWire {
  name: string;
  role: string | null;
  status: string;
  task: string | null;
  taskId: string | null;
  activity: WorkflowAgentActivityWire[];
}

export interface WorkflowJobWire {
  displayName: string;
  jobId: string;
  role: string | null;
  status: JobWireStatus;
  taskSummary: string | null;
  todoTaskId: string | null;
  createdAtMs: number;
  startedAtMs: number | null;
  finishedAtMs: number | null;
  softBudgetExhausted: boolean;
}

export interface WorkflowActiveTaskWire {
  taskId: string | null;
  summary: string;
}

export interface WorkflowActivityWire {
  atMs: number;
  kind: string;
  text: string;
}

export interface WorkflowIrcWire {
  sender: string;
  to: string | null;
  text: string;
  atMs: number;
  messageId: string | null;
  replyTo: string | null;
}

/** Worktree identity as the panel may display it: labels are already reduced
 *  to safe basenames, and an empty label is a legal (redacted) value. */
export interface WorkflowWorktreeWire {
  label: string;
  branch: string;
}

export type WorkflowIntegrateStepWire = 'pending' | 'integrating' | 'applied' | 'conflicted';

export interface WorkflowIntegrationWire {
  step: WorkflowIntegrateStepWire;
  summary: string;
  filesChanged: number;
  insertions: number;
  deletions: number;
  conflicts: string[];
}

/** Live `workflow_detail` projection: supervisor state, planning activity,
 *  active tasks, and workers with per-agent activity. The Todo DAG phases stay
 *  with the Todo surface, which already owns their projection. */
export interface WorkflowDetailData {
  id: string;
  sessionId: string;
  generation: number;
  name: string;
  objective: string;
  status: WorkflowStatusWire;
  failure: string | null;
  supervisor: WorkflowActorWire | null;
  subagents: WorkflowActorWire[];
  jobs: WorkflowJobWire[];
  activeTasks: WorkflowActiveTaskWire[];
  worktree: WorkflowWorktreeWire;
  integration: WorkflowIntegrationWire;
  planningActivity: WorkflowActivityWire[];
  planningStartedAtMs: number | null;
  recentIrc: WorkflowIrcWire[];
}

function parseWorkflowSnapshot(data: unknown): WorkflowSnapshotData | null {
  const value = wireRecord(data);
  if (!value) return null;
  const workflowId = readNonemptyString(value.workflowId);
  const name = typeof value.name === 'string' ? value.name : null;
  const objective = typeof value.objective === 'string' ? value.objective : null;
  const status = typeof value.status === 'string' && WORKFLOW_STATUSES[value.status] ? value.status : null;
  const generation = wireCount(value.generation);
  if (workflowId === null || name === null || objective === null) return null;
  if (status === null || generation === null) return null;
  return {
    workflowId,
    name,
    objective,
    status: status as WorkflowStatusWire,
    generation,
    worktree: wireOptionalString(value.worktree),
    branch: wireOptionalString(value.branch),
    supervisorAgentId: wireOptionalString(value.supervisorAgentId),
    supervisorJobId: wireOptionalString(value.supervisorJobId),
    failure: wireOptionalString(value.failure),
    integration: wireOptionalString(value.integration),
  };
}

/** `workflow_list` rows (`{ workflows: [...] }`). */
export function parseWorkflowList(data: unknown): WorkflowSnapshotData[] | null {
  const value = wireRecord(data);
  return value ? wireList(value.workflows, parseWorkflowSnapshot) : null;
}

/** One workflow row: `workflow_create` / `_get` / `_pause` / `_resume` /
 *  `_cancel` / `_integrate` / `_remove` all answer with this projection. */
export function parseWorkflowSnapshotRow(data: unknown): WorkflowSnapshotData | null {
  return parseWorkflowSnapshot(data);
}

function parseWorkflowAgentActivity(data: unknown): WorkflowAgentActivityWire | null {
  const value = wireRecord(data);
  if (!value) return null;
  const atMs = wireCount(value.atMs);
  if (atMs === null || typeof value.text !== 'string') return null;
  if (value.kind !== 'task' && value.kind !== 'irc') return null;
  return { atMs, kind: value.kind, text: value.text };
}

function parseWorkflowActor(data: unknown): WorkflowActorWire | null {
  const value = wireRecord(data);
  if (!value) return null;
  const name = typeof value.name === 'string' ? value.name : null;
  const status = typeof value.status === 'string' ? value.status : null;
  if (name === null || status === null) return null;
  const rawActivity = value.activity === undefined ? [] : value.activity;
  const activity = wireList(rawActivity, parseWorkflowAgentActivity);
  if (activity === null) return null;
  return {
    name,
    role: wireOptionalString(value.role),
    status,
    task: wireOptionalString(value.task),
    taskId: wireOptionalString(value.taskId),
    activity,
  };
}

function parseWorkflowJob(data: unknown): WorkflowJobWire | null {
  const value = wireRecord(data);
  if (!value) return null;
  const displayName = typeof value.displayName === 'string' ? value.displayName : null;
  const jobId = readNonemptyString(value.jobId);
  const status = typeof value.status === 'string' && JOB_STATUSES[value.status] ? value.status : null;
  const createdAtMs = wireCount(value.createdAtMs);
  if (displayName === null || jobId === null || status === null || createdAtMs === null) return null;
  return {
    displayName,
    jobId,
    role: wireOptionalString(value.role),
    status: status as JobWireStatus,
    taskSummary: wireOptionalString(value.taskSummary),
    todoTaskId: wireOptionalString(value.todoTaskId),
    createdAtMs,
    startedAtMs: wireCount(value.startedAtMs),
    finishedAtMs: wireCount(value.finishedAtMs),
    softBudgetExhausted: value.softBudgetExhausted === true,
  };
}

function parseWorkflowActiveTask(data: unknown): WorkflowActiveTaskWire | null {
  const value = wireRecord(data);
  if (!value) return null;
  const summary = typeof value.summary === 'string' ? value.summary : null;
  return summary === null ? null : { taskId: wireOptionalString(value.taskId), summary };
}

function parseWorkflowActivity(data: unknown): WorkflowActivityWire | null {
  const value = wireRecord(data);
  if (!value) return null;
  const atMs = wireCount(value.atMs);
  const kind = readNonemptyString(value.kind);
  if (atMs === null || kind === null || typeof value.text !== 'string') return null;
  return { atMs, kind, text: value.text };
}

function parseWorkflowIrc(data: unknown): WorkflowIrcWire | null {
  const value = wireRecord(data);
  if (!value) return null;
  const sender = typeof value.sender === 'string' ? value.sender : null;
  const atMs = wireCount(value.atMs);
  if (sender === null || atMs === null || typeof value.text !== 'string') return null;
  return {
    sender,
    to: wireOptionalString(value.to),
    text: value.text,
    atMs,
    messageId: wireOptionalString(value.messageId),
    replyTo: wireOptionalString(value.replyTo),
  };
}

function parseWorkflowWorktree(data: unknown): WorkflowWorktreeWire | null {
  const value = wireRecord(data);
  if (!value) return null;
  if (typeof value.label !== 'string' || typeof value.branch !== 'string') return null;
  return { label: value.label, branch: value.branch };
}

function parseWorkflowIntegration(data: unknown): WorkflowIntegrationWire | null {
  const value = wireRecord(data);
  if (!value) return null;
  const step = value.step;
  if (step !== 'pending' && step !== 'integrating' && step !== 'applied' && step !== 'conflicted') return null;
  if (typeof value.summary !== 'string') return null;
  const filesChanged = wireCount(value.filesChanged);
  const insertions = wireCount(value.insertions);
  const deletions = wireCount(value.deletions);
  const conflicts = wireList(value.conflicts, (item) => (typeof item === 'string' ? item : null));
  if (filesChanged === null || insertions === null || deletions === null || conflicts === null) return null;
  return { step, summary: value.summary, filesChanged, insertions, deletions, conflicts };
}

/** `workflow_detail` projection. Nested rows are parsed individually: a
 *  malformed row fails the whole payload (null) rather than reaching a
 *  half-rendered panel. */
export function parseWorkflowDetail(data: unknown): WorkflowDetailData | null {
  const value = wireRecord(data);
  if (!value) return null;
  const id = readNonemptyString(value.id);
  const name = typeof value.name === 'string' ? value.name : null;
  const objective = typeof value.objective === 'string' ? value.objective : null;
  const status = typeof value.status === 'string' && WORKFLOW_STATUSES[value.status] ? value.status : null;
  const generation = wireCount(value.generation);
  const worktree = parseWorkflowWorktree(value.worktree);
  const integration = parseWorkflowIntegration(value.integration);
  if (id === null || name === null || objective === null) return null;
  if (status === null || generation === null || !worktree || !integration) return null;
  const subagents = wireList(value.subagents, parseWorkflowActor);
  const activeTasks = wireList(value.activeTasks, parseWorkflowActiveTask);
  if (subagents === null || activeTasks === null) return null;
  const jobs = value.jobs === undefined ? [] : wireList(value.jobs, parseWorkflowJob);
  const planningActivity = value.planningActivity === undefined
    ? []
    : wireList(value.planningActivity, parseWorkflowActivity);
  const recentIrc = value.recentIrc === undefined ? [] : wireList(value.recentIrc, parseWorkflowIrc);
  if (jobs === null || planningActivity === null || recentIrc === null) return null;
  const supervisor = value.supervisor === undefined || value.supervisor === null
    ? null
    : parseWorkflowActor(value.supervisor);
  if (value.supervisor !== undefined && value.supervisor !== null && !supervisor) return null;
  return {
    id,
    sessionId: typeof value.sessionId === 'string' ? value.sessionId : '',
    generation,
    name,
    objective,
    status: status as WorkflowStatusWire,
    failure: wireOptionalString(value.failure),
    supervisor,
    subagents,
    jobs,
    activeTasks,
    worktree,
    integration,
    planningActivity,
    planningStartedAtMs: wireCount(value.planningStartedAtMs),
    recentIrc,
  };
}

export type SideChatEntryKindWire = 'user' | 'assistant' | 'tool' | 'system' | 'thinking';

export interface SideChatEntryWire {
  id: string;
  kind: SideChatEntryKindWire;
  role: 'user' | 'assistant' | 'tool' | 'system';
  text: string;
  toolName: string | null;
  toolArgs: unknown;
  output: string | null;
  thinking: string | null;
  isError: boolean;
  isPartial: boolean;
}

export interface SideChatTabWire {
  name: string;
  streaming: boolean;
  status: string;
  entries: SideChatEntryWire[];
}

/** Whole side-chat container. `accepted`/`busy` are stamped only by
 *  `side_chat_prompt`; both stay null on every other answer. */
export interface SideChatSnapshotData {
  active: string;
  maxTabs: number;
  tabs: SideChatTabWire[];
  accepted: boolean | null;
  busy: boolean | null;
}

const SIDE_CHAT_KINDS: Record<string, true> = {
  user: true,
  assistant: true,
  tool: true,
  system: true,
  thinking: true,
};

const SIDE_CHAT_ROLES: Record<string, true> = {
  user: true,
  assistant: true,
  tool: true,
  system: true,
};

function parseSideChatEntry(data: unknown): SideChatEntryWire | null {
  const value = wireRecord(data);
  if (!value) return null;
  const id = readNonemptyString(value.id);
  const kind = typeof value.kind === 'string' && SIDE_CHAT_KINDS[value.kind] ? value.kind : null;
  const role = typeof value.role === 'string' && SIDE_CHAT_ROLES[value.role] ? value.role : null;
  if (id === null || kind === null || role === null) return null;
  if (typeof value.text !== 'string') return null;
  if (typeof value.isError !== 'boolean' || typeof value.isPartial !== 'boolean') return null;
  return {
    id,
    kind: kind as SideChatEntryKindWire,
    role: role as SideChatEntryWire['role'],
    text: value.text,
    toolName: wireOptionalString(value.toolName),
    toolArgs: value.toolArgs,
    output: wireOptionalString(value.output),
    thinking: wireOptionalString(value.thinking),
    isError: value.isError,
    isPartial: value.isPartial,
  };
}

function parseSideChatTab(data: unknown): SideChatTabWire | null {
  const value = wireRecord(data);
  if (!value) return null;
  const name = readNonemptyString(value.name);
  if (name === null || typeof value.streaming !== 'boolean') return null;
  if (typeof value.status !== 'string') return null;
  const entries = wireList(value.entries, parseSideChatEntry);
  if (entries === null) return null;
  return { name, streaming: value.streaming, status: value.status, entries };
}

/** Side-chat snapshot: `side_chat_list` / `_new` / `_switch` / `_close` /
 *  `_abort` answer with the container, `_prompt` with the container plus the
 *  accepted/busy verdict. */
export function parseSideChatSnapshot(data: unknown): SideChatSnapshotData | null {
  const value = wireRecord(data);
  if (!value) return null;
  const active = readNonemptyString(value.active);
  const maxTabs = wireCount(value.maxTabs);
  const tabs = wireList(value.tabs, parseSideChatTab);
  if (active === null || maxTabs === null || tabs === null) return null;
  if (value.accepted !== undefined && value.accepted !== null && typeof value.accepted !== 'boolean') return null;
  if (value.busy !== undefined && value.busy !== null && typeof value.busy !== 'boolean') return null;
  return {
    active,
    maxTabs,
    tabs,
    accepted: typeof value.accepted === 'boolean' ? value.accepted : null,
    busy: typeof value.busy === 'boolean' ? value.busy : null,
  };
}

/** Duplicate identities mean a payload cannot be keyed or trusted row by row. */
function hasUniqueIdentities<T>(rows: readonly T[], identity: (row: T) => string): boolean {
  const seen = new Set<string>();
  for (const row of rows) {
    const key = identity(row);
    if (seen.has(key)) return false;
    seen.add(key);
  }
  return true;
}
// File conversations (FileConversationDock): Explorer chat tabs bound to one client-generated UUID conversationId for the tab's lifetime; transcript rows reuse the side-chat entry parser.
export const FILE_CONVERSATION_CAPACITY_LIMIT = 8;
export const FILE_CONVERSATION_PROMPT_MAX_BYTES = 8_192;

const CONVERSATION_ID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;

/** Exact conversation-id contract: RFC 4122 v4 text; mint with `createConversationId`. */
export function isValidConversationId(value: unknown): value is string {
  return typeof value === 'string' && CONVERSATION_ID_RE.test(value);
}

export function createConversationId(randomBytes: (n: number) => Uint8Array = defaultRandomBytes): string {
  const bytes = randomBytes(16);
  bytes[6] = (bytes[6] & 0x0f) | 0x40;
  bytes[8] = (bytes[8] & 0x3f) | 0x80;
  let hex = '';
  for (const byte of bytes) hex += byte.toString(16).padStart(2, '0');
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}

export interface FileConversationRangeWire {
  startLine: number;
  endLine: number;
}

export interface FileConversationContextWire {
  requestedRange?: FileConversationRangeWire;
  includedRange: FileConversationRangeWire | null;
  truncated: boolean;
}

export interface FileConversationSnapshotData {
  conversationId: string;
  path: string;
  streaming: boolean;
  status: string;
  entries: SideChatEntryWire[];
  context?: FileConversationContextWire;
  accepted?: boolean;
  busy?: boolean;
}

export interface FileConversationCloseData {
  conversationId: string;
  closed: true;
}

export type FileConversationErrorCodeWire =
  | 'file_conversation_capacity'
  | 'file_conversation_superseded'
  | 'file_conversation_not_found'
  | 'file_conversation_binding_mismatch'
  | 'file_conversation_owner_capture_failed'
  | 'file_conversation_selection_too_large'
  | 'file_conversation_selection_out_of_range'
  | 'file_conversation_selection_invalid'
  | 'file_conversation_prompt_empty'
  | 'file_conversation_prompt_too_large'
  | 'file_conversation_read_failed'
  | 'file_conversation_open_failed'
  | 'file_line_too_large'
  | 'file_symlink'
  | 'file_unavailable'
  | 'directory_unavailable';

const FILE_CONVERSATION_ERROR_CODES: Readonly<Record<FileConversationErrorCodeWire, true>> = {
  file_conversation_capacity: true,
  file_conversation_superseded: true,
  file_conversation_not_found: true,
  file_conversation_binding_mismatch: true,
  file_conversation_owner_capture_failed: true,
  file_conversation_selection_too_large: true,
  file_conversation_selection_out_of_range: true,
  file_conversation_selection_invalid: true,
  file_conversation_prompt_empty: true,
  file_conversation_prompt_too_large: true,
  file_conversation_read_failed: true,
  file_conversation_open_failed: true,
  file_line_too_large: true,
  file_symlink: true,
  file_unavailable: true,
  directory_unavailable: true,
};

/** Failure envelope `code`; unknown codes propagate as opaque failures, never success. */
export function parseFileConversationErrorCode(data: unknown): FileConversationErrorCodeWire | null {
  const value = wireRecord(data);
  if (!value) return null;
  const code = value.code;
  return typeof code === 'string'
    && Object.prototype.hasOwnProperty.call(FILE_CONVERSATION_ERROR_CODES, code)
    ? (code as FileConversationErrorCodeWire)
    : null;
}

function parseFileConversationRange(value: unknown): FileConversationRangeWire | null {
  const record = wireRecord(value);
  if (!record) return null;
  const startLine = record.startLine;
  const endLine = record.endLine;
  if (typeof startLine !== 'number' || !Number.isSafeInteger(startLine) || startLine < 1) return null;
  if (typeof endLine !== 'number' || !Number.isSafeInteger(endLine) || endLine < 1) return null;
  if (startLine > endLine) return null;
  return { startLine, endLine };
}

function parseFileConversationContext(value: unknown): FileConversationContextWire | null {
  const record = wireRecord(value);
  if (!record) return null;
  const includedRange = record.includedRange === null
    ? null
    : parseFileConversationRange(record.includedRange);
  if (record.includedRange !== null && includedRange === null) return null;
  if (typeof record.truncated !== 'boolean') return null;
  const context: FileConversationContextWire = { includedRange, truncated: record.truncated };
  if (record.requestedRange !== undefined) {
    const requestedRange = parseFileConversationRange(record.requestedRange);
    if (requestedRange === null) return null;
    context.requestedRange = requestedRange;
  }
  return context;
}

/** Snapshot bound to the open tab: a foreign or non-UUID conversationId is refused; status is the required engine controller string; accepted/busy stay absent unless sent, so an absent or false verdict is never a draft success. */
export function parseFileConversationSnapshot(
  data: unknown,
  conversationId: string,
): FileConversationSnapshotData | null {
  const value = wireRecord(data);
  if (!value) return null;
  if (!isValidConversationId(conversationId) || value.conversationId !== conversationId) return null;
  const path = readNonemptyString(value.path);
  if (path === null) return null;
  if (typeof value.streaming !== 'boolean') return null;
  if (typeof value.status !== 'string') return null;
  const entries = wireList(value.entries, parseSideChatEntry);
  if (entries === null) return null;
  const snapshot: FileConversationSnapshotData = {
    conversationId,
    path,
    streaming: value.streaming,
    status: value.status,
    entries,
  };
  if (value.context !== undefined) {
    const context = parseFileConversationContext(value.context);
    if (context === null) return null;
    snapshot.context = context;
  }
  if (value.accepted !== undefined) {
    if (typeof value.accepted !== 'boolean') return null;
    snapshot.accepted = value.accepted;
  }
  if (value.busy !== undefined) {
    if (typeof value.busy !== 'boolean') return null;
    snapshot.busy = value.busy;
  }
  return snapshot;
}

export function parseFileConversationClose(
  data: unknown,
  conversationId: string,
): FileConversationCloseData | null {
  const value = wireRecord(data);
  if (!value) return null;
  if (!isValidConversationId(conversationId) || value.conversationId !== conversationId) return null;
  if (value.closed !== true) return null;
  return { conversationId, closed: true };
}

export function buildFileConversationPromptPayload(
  conversationId: string,
  message: string,
  selection?: FileConversationRangeWire | null,
): { ok: true; command: Extract<RunnerCommand, { type: 'file_conversation_prompt' }> }
  | { ok: false; error: string } {
  if (!isValidConversationId(conversationId)) {
    return { ok: false, error: 'conversation id must be an RFC 4122 v4 UUID' };
  }
  if (typeof message !== 'string') return { ok: false, error: 'message must be a string' };
  if (Buffer.byteLength(message, 'utf8') > FILE_CONVERSATION_PROMPT_MAX_BYTES) {
    return { ok: false, error: `message exceeds ${FILE_CONVERSATION_PROMPT_MAX_BYTES} bytes` };
  }
  if (selection !== undefined && selection !== null) {
    if (parseFileConversationRange(selection) === null) {
      return { ok: false, error: 'selection must be a positive start<=end line range' };
    }
    return {
      ok: true,
      command: {
        type: 'file_conversation_prompt',
        conversationId,
        message,
        selection: { startLine: selection.startLine, endLine: selection.endLine },
      },
    };
  }
  return { ok: true, command: { type: 'file_conversation_prompt', conversationId, message } };
}

export type TodoChildStatusWire = 'created' | 'idle' | 'running' | 'paused' | 'stopped' | 'failed';
export type TodoChildTaskStatusWire = 'pending' | 'running' | 'completed' | 'failed' | 'interrupted';
export type TodoChildReceiptStatusWire = 'queued' | 'running' | 'completed' | 'failed';
export type TodoChildAgentStatusWire = 'queued' | 'running' | 'idle' | 'parked' | 'aborted';

const TODO_CHILD_STATUSES: Record<string, true> = {
  created: true,
  idle: true,
  running: true,
  paused: true,
  stopped: true,
  failed: true,
};

const TODO_CHILD_TASK_STATUSES: Record<string, true> = {
  pending: true,
  running: true,
  completed: true,
  failed: true,
  interrupted: true,
};

const TODO_CHILD_RECEIPT_STATUSES: Record<string, true> = {
  queued: true,
  running: true,
  completed: true,
  failed: true,
};

const TODO_CHILD_AGENT_STATUSES: Record<string, true> = {
  queued: true,
  running: true,
  idle: true,
  parked: true,
  aborted: true,
};

/** One durable child session row of `todo_child_list`. */
export interface TodoChildManifestEntryData {
  childSessionId: string;
  label: string | null;
  createdAt: number;
  status: TodoChildStatusWire;
  parentEntryId: string | null;
  parentRevision: number;
  kind: 'todo';
}

export interface TodoChildParentWire {
  sessionId: string;
  entryId: string | null;
  revision: number;
}

export interface TodoChildWorkspaceWire {
  workspace: string;
  cwd: string;
}

export interface TodoChildTaskWire {
  id: string;
  content: string;
  dependsOn: string[];
  status: TodoChildTaskStatusWire;
  agent: string | null;
  role: string | null;
  createdAt: number;
  summary: string | null;
}

export interface TodoChildReceiptWire {
  id: string;
  parentSessionId: string;
  childSessionId: string;
  messageId: string;
  acceptedAt: number;
  status: TodoChildReceiptStatusWire;
  summary: string | null;
}

export interface TodoChildAgentWire {
  id: string;
  displayName: string;
  role: string | null;
  status: TodoChildAgentStatusWire;
  taskId: string | null;
}

export interface TodoChildJobWire {
  jobId: string;
  displayName: string;
  role: string | null;
  status: JobWireStatus;
  taskId: string | null;
  summary: string | null;
}

export interface TodoChildActivityWire {
  atMs: number;
  actor: string | null;
  kind: 'task' | 'irc';
  text: string;
}

export interface TodoChildIrcWire {
  messageId: string;
  from: string;
  to: string;
  text: string;
  atMs: number;
  replyTo: string | null;
}

export interface TodoChildTranscriptWire {
  id: string;
  role: string;
  actor: string | null;
  text: string;
  atMs: number | null;
  interrupted: boolean;
}

/** Full `todo_child_detail` snapshot: manifest identity plus the child's
 *  tasks, receipts, agents, jobs, activity feed, IRC, and transcript. `todoId`
 *  is the child's own Todo-DAG identity, so it must equal `childSessionId`. */
export interface TodoChildDetailData {
  parentSessionId: string;
  childSessionId: string;
  todoId: string;
  parent: TodoChildParentWire;
  workspace: TodoChildWorkspaceWire;
  status: TodoChildStatusWire;
  isRunning: boolean;
  revision: number;
  label: string | null;
  createdAt: number;
  lastError: string | null;
  transcriptEntries: number;
  tasks: TodoChildTaskWire[];
  receipts: TodoChildReceiptWire[];
  supervisor: TodoChildAgentWire | null;
  agents: TodoChildAgentWire[];
  jobs: TodoChildJobWire[];
  activity: TodoChildActivityWire[];
  irc: TodoChildIrcWire[];
  transcript: TodoChildTranscriptWire[];
}

/** Lifecycle projection of a child (`todo_child_status` and the
 *  stop/pause/resume/cancel/select answers). */
export interface TodoChildStatusData {
  parentSessionId: string;
  childSessionId: string;
  status: TodoChildStatusWire;
  isRunning: boolean;
  transcriptEntries: number;
  revision: number;
  label: string | null;
  createdAt: number;
  lastError: string | null;
  tasks: TodoChildTaskWire[];
  receipts: TodoChildReceiptWire[];
}

function parseTodoChildManifestEntry(data: unknown): TodoChildManifestEntryData | null {
  const value = wireRecord(data);
  if (!value) return null;
  const childSessionId = readNonemptyString(value.childSessionId);
  const createdAt = wireCount(value.createdAt);
  const parentRevision = wireCount(value.parentRevision);
  const status = typeof value.status === 'string' && TODO_CHILD_STATUSES[value.status] ? value.status : null;
  if (childSessionId === null || createdAt === null || parentRevision === null) return null;
  if (status === null || value.kind !== 'todo') return null;
  if (value.label !== undefined && value.label !== null && typeof value.label !== 'string') return null;
  if (value.parentEntryId !== undefined && value.parentEntryId !== null && typeof value.parentEntryId !== 'string') {
    return null;
  }
  return {
    childSessionId,
    label: wireOptionalString(value.label),
    createdAt,
    status: status as TodoChildStatusWire,
    parentEntryId: wireOptionalString(value.parentEntryId),
    parentRevision,
    kind: 'todo',
  };
}

/** `todo_child_list`: the bare manifest array. A duplicate child identity fails
 *  the whole payload — the picker never lists a phantom child. */
export function parseTodoChildManifest(data: unknown): TodoChildManifestEntryData[] | null {
  const entries = wireList(data, parseTodoChildManifestEntry);
  if (entries === null || !hasUniqueIdentities(entries, (entry) => entry.childSessionId)) return null;
  return entries;
}

function parseTodoChildParent(data: unknown): TodoChildParentWire | null {
  const value = wireRecord(data);
  if (!value) return null;
  const sessionId = readNonemptyString(value.sessionId);
  const revision = wireCount(value.revision);
  if (sessionId === null || revision === null) return null;
  if (value.entryId !== undefined && value.entryId !== null && typeof value.entryId !== 'string') return null;
  return { sessionId, entryId: wireOptionalString(value.entryId), revision };
}

function parseTodoChildWorkspace(data: unknown): TodoChildWorkspaceWire | null {
  const value = wireRecord(data);
  if (!value) return null;
  const workspace = readNonemptyString(value.workspace);
  const cwd = readNonemptyString(value.cwd);
  return workspace === null || cwd === null ? null : { workspace, cwd };
}

function parseTodoChildTask(data: unknown): TodoChildTaskWire | null {
  const value = wireRecord(data);
  if (!value) return null;
  const id = readNonemptyString(value.id);
  const content = typeof value.content === 'string' ? value.content : null;
  const status = typeof value.status === 'string' && TODO_CHILD_TASK_STATUSES[value.status] ? value.status : null;
  const createdAt = wireCount(value.createdAt);
  const dependsOn = wireList(value.dependsOn, (item) => readNonemptyString(item));
  if (id === null || content === null || status === null || createdAt === null || dependsOn === null) return null;
  return {
    id,
    content,
    dependsOn,
    status: status as TodoChildTaskStatusWire,
    agent: wireOptionalString(value.agent),
    role: wireOptionalString(value.role),
    createdAt,
    summary: wireOptionalString(value.summary),
  };
}

function parseTodoChildReceipt(data: unknown): TodoChildReceiptWire | null {
  const value = wireRecord(data);
  if (!value) return null;
  const id = readNonemptyString(value.id);
  const parentSessionId = readNonemptyString(value.parentSessionId);
  const childSessionId = readNonemptyString(value.childSessionId);
  const messageId = readNonemptyString(value.messageId);
  const acceptedAt = wireCount(value.acceptedAt);
  const status = typeof value.status === 'string' && TODO_CHILD_RECEIPT_STATUSES[value.status] ? value.status : null;
  if (id === null || parentSessionId === null || childSessionId === null) return null;
  if (messageId === null || acceptedAt === null || status === null) return null;
  return {
    id,
    parentSessionId,
    childSessionId,
    messageId,
    acceptedAt,
    status: status as TodoChildReceiptStatusWire,
    summary: wireOptionalString(value.summary),
  };
}

function parseTodoChildAgent(data: unknown): TodoChildAgentWire | null {
  const value = wireRecord(data);
  if (!value) return null;
  const id = readNonemptyString(value.id);
  const displayName = typeof value.displayName === 'string' ? value.displayName : null;
  const status = typeof value.status === 'string' && TODO_CHILD_AGENT_STATUSES[value.status] ? value.status : null;
  if (id === null || displayName === null || status === null) return null;
  return {
    id,
    displayName,
    role: wireOptionalString(value.role),
    status: status as TodoChildAgentStatusWire,
    taskId: wireOptionalString(value.taskId),
  };
}

function parseTodoChildJob(data: unknown): TodoChildJobWire | null {
  const value = wireRecord(data);
  if (!value) return null;
  const jobId = readNonemptyString(value.jobId);
  const displayName = typeof value.displayName === 'string' ? value.displayName : null;
  const status = typeof value.status === 'string' && JOB_STATUSES[value.status] ? value.status : null;
  if (jobId === null || displayName === null || status === null) return null;
  return {
    jobId,
    displayName,
    role: wireOptionalString(value.role),
    status: status as JobWireStatus,
    taskId: wireOptionalString(value.taskId),
    summary: wireOptionalString(value.summary),
  };
}

function parseTodoChildActivity(data: unknown): TodoChildActivityWire | null {
  const value = wireRecord(data);
  if (!value) return null;
  const atMs = wireCount(value.atMs);
  const text = typeof value.text === 'string' ? value.text : null;
  if (atMs === null || text === null) return null;
  if (value.kind !== 'task' && value.kind !== 'irc') return null;
  return { atMs, actor: wireOptionalString(value.actor), kind: value.kind, text };
}

function parseTodoChildIrc(data: unknown): TodoChildIrcWire | null {
  const value = wireRecord(data);
  if (!value) return null;
  const messageId = readNonemptyString(value.messageId);
  const from = typeof value.from === 'string' ? value.from : null;
  const to = typeof value.to === 'string' ? value.to : null;
  const text = typeof value.text === 'string' ? value.text : null;
  const atMs = wireCount(value.atMs);
  if (messageId === null || from === null || to === null || text === null || atMs === null) return null;
  return { messageId, from, to, text, atMs, replyTo: wireOptionalString(value.replyTo) };
}

function parseTodoChildTranscript(data: unknown): TodoChildTranscriptWire | null {
  const value = wireRecord(data);
  if (!value) return null;
  const id = readNonemptyString(value.id);
  const role = typeof value.role === 'string' ? value.role : null;
  const text = typeof value.text === 'string' ? value.text : null;
  if (id === null || role === null || text === null) return null;
  if (typeof value.interrupted !== 'boolean') return null;
  return {
    id,
    role,
    actor: wireOptionalString(value.actor),
    text,
    atMs: wireCount(value.atMs),
    interrupted: value.interrupted,
  };
}

/** `todo_child_detail`. Beyond shape, the child's own invariants are enforced:
 *  the parent provenance and `todoId` must match the queried child, every
 *  receipt must belong to it, `isRunning` must agree with `status`, and no
 *  identity may repeat. */
export function parseTodoChildDetail(data: unknown): TodoChildDetailData | null {
  const value = wireRecord(data);
  if (!value) return null;
  const parentSessionId = readNonemptyString(value.parentSessionId);
  const childSessionId = readNonemptyString(value.childSessionId);
  const todoId = readNonemptyString(value.todoId);
  const parent = parseTodoChildParent(value.parent);
  const workspace = parseTodoChildWorkspace(value.workspace);
  const status = typeof value.status === 'string' && TODO_CHILD_STATUSES[value.status] ? value.status : null;
  const revision = wireCount(value.revision);
  const createdAt = wireCount(value.createdAt);
  const transcriptEntries = wireCount(value.transcriptEntries);
  if (parentSessionId === null || childSessionId === null || todoId === null) return null;
  if (!parent || !workspace || status === null || typeof value.isRunning !== 'boolean') return null;
  if (revision === null || createdAt === null || transcriptEntries === null) return null;
  const tasks = wireList(value.tasks, parseTodoChildTask);
  const receipts = wireList(value.receipts, parseTodoChildReceipt);
  const agents = wireList(value.agents, parseTodoChildAgent);
  const jobs = wireList(value.jobs, parseTodoChildJob);
  const activity = wireList(value.activity, parseTodoChildActivity);
  const irc = wireList(value.irc, parseTodoChildIrc);
  const transcript = wireList(value.transcript, parseTodoChildTranscript);
  if (!tasks || !receipts || !agents || !jobs || !activity || !irc || !transcript) return null;
  if (value.label !== undefined && value.label !== null && typeof value.label !== 'string') return null;
  if (value.lastError !== undefined && value.lastError !== null && typeof value.lastError !== 'string') return null;
  const supervisor = value.supervisor === undefined || value.supervisor === null
    ? null
    : parseTodoChildAgent(value.supervisor);
  if (value.supervisor !== undefined && value.supervisor !== null && !supervisor) return null;
  if (parent.sessionId !== parentSessionId || todoId !== childSessionId) return null;
  if (value.isRunning !== (status === 'running') || transcriptEntries < transcript.length) return null;
  for (const receipt of receipts) {
    if (receipt.parentSessionId !== parentSessionId || receipt.childSessionId !== childSessionId) return null;
  }
  if (!hasUniqueIdentities(tasks, (task) => task.id)) return null;
  if (!hasUniqueIdentities(receipts, (receipt) => receipt.id)) return null;
  if (!hasUniqueIdentities(agents, (agent) => agent.id)) return null;
  if (!hasUniqueIdentities(jobs, (job) => job.jobId)) return null;
  if (!hasUniqueIdentities(irc, (message) => message.messageId)) return null;
  if (!hasUniqueIdentities(transcript, (entry) => entry.id)) return null;
  if (supervisor && agents.some((agent) => agent.id === supervisor.id)) return null;
  return {
    parentSessionId,
    childSessionId,
    todoId,
    parent,
    workspace,
    status: status as TodoChildStatusWire,
    isRunning: value.isRunning,
    revision,
    label: wireOptionalString(value.label),
    createdAt,
    lastError: wireOptionalString(value.lastError),
    transcriptEntries,
    tasks,
    receipts,
    supervisor,
    agents,
    jobs,
    activity,
    irc,
    transcript,
  };
}

/** Lifecycle answer of `todo_child_status` / `_select` / `_stop` / `_pause` /
 *  `_resume` / `_cancel`: the child's state without the heavy detail lists. */
export function parseTodoChildStatus(data: unknown): TodoChildStatusData | null {
  const value = wireRecord(data);
  if (!value) return null;
  const parentSessionId = readNonemptyString(value.parentSessionId);
  const childSessionId = readNonemptyString(value.childSessionId);
  const status = typeof value.status === 'string' && TODO_CHILD_STATUSES[value.status] ? value.status : null;
  const revision = wireCount(value.revision);
  const createdAt = wireCount(value.createdAt);
  const transcriptEntries = wireCount(value.transcriptEntries);
  if (parentSessionId === null || childSessionId === null || status === null) return null;
  if (typeof value.isRunning !== 'boolean' || revision === null) return null;
  if (createdAt === null || transcriptEntries === null) return null;
  const tasks = wireList(value.tasks, parseTodoChildTask);
  const receipts = wireList(value.receipts, parseTodoChildReceipt);
  if (!tasks || !receipts) return null;
  if (value.label !== undefined && value.label !== null && typeof value.label !== 'string') return null;
  if (value.lastError !== undefined && value.lastError !== null && typeof value.lastError !== 'string') return null;
  if (value.isRunning !== (status === 'running')) return null;
  for (const receipt of receipts) {
    if (receipt.parentSessionId !== parentSessionId || receipt.childSessionId !== childSessionId) return null;
  }
  if (!hasUniqueIdentities(tasks, (task) => task.id)) return null;
  if (!hasUniqueIdentities(receipts, (receipt) => receipt.id)) return null;
  return {
    parentSessionId,
    childSessionId,
    status: status as TodoChildStatusWire,
    isRunning: value.isRunning,
    transcriptEntries,
    revision,
    label: wireOptionalString(value.label),
    createdAt,
    lastError: wireOptionalString(value.lastError),
    tasks,
    receipts,
  };
}

/** `todo_child_create` identity: the new child's id, but only when the
 *  returned identity and snapshot both claim THIS parent. */
export function parseTodoChildCreateId(data: unknown, parentSessionId: string): string | null {
  const value = wireRecord(data);
  if (!value) return null;
  const identity = wireRecord(value.identity);
  const snapshot = wireRecord(value.snapshot);
  if (!identity || !snapshot) return null;
  const identityChild = readNonemptyString(identity.childSessionId);
  if (identity.parentSessionId !== parentSessionId) return null;
  if (snapshot.parentSessionId !== parentSessionId) return null;
  if (snapshot.childSessionId !== identityChild) return null;
  return identityChild;
}

export type CodeReviewFileStatus =
  | 'added'
  | 'deleted'
  | 'modified'
  | 'renamed'
  | 'copied'
  | 'binary'
  | 'changed';

const CODE_REVIEW_FILE_STATUSES: Record<string, true> = {
  added: true,
  deleted: true,
  modified: true,
  renamed: true,
  copied: true,
  binary: true,
  changed: true,
};

/** One hunk descriptor of a single-file diff page. `lines` starts at
 *  `lineStart` of the COMPLETE hunk, whose identity is `contentHash`. */
export interface CodeReviewFileDiffHunkData {
  index: number;
  header: string;
  oldStart: number;
  oldCount: number;
  newStart: number;
  newCount: number;
  contentHash: string;
  totalLines: number;
  complete: boolean;
  lineStart: number;
  lines: DiffLine[];
}

export interface CodeReviewFileDiffPageData {
  snapshotId: string;
  path: string;
  previousPath: string | null;
  binary: boolean;
  status: CodeReviewFileStatus;
  hunks: CodeReviewFileDiffHunkData[];
  cursor: number;
  nextCursor: number | null;
  hasMore: boolean;
  totalLines: number;
  hunkCount: number;
  byteCapped: boolean;
}

export interface CodeReviewFileContextPageData {
  snapshotId: string;
  path: string;
  oldLineCount: number;
  newLineCount: number;
  binary: boolean;
  lines: DiffLine[];
}

export interface CodeReviewCloseResultData {
  closed: boolean;
  staleReview: boolean;
}

export interface CodeReviewModelAckData {
  model: string;
}

/** Diff line of a review page. Lines are display text, so they coerce (like
 *  the /web reader): an unknown kind reads as `meta`, a missing text as empty,
 *  and non-numeric line numbers as absent. */
function normalizeReviewLine(data: unknown): DiffLine | null {
  const value = wireRecord(data);
  if (!value) return null;
  const kindRaw = value.kind;
  const kind = kindRaw === 'context' || kindRaw === 'addition' || kindRaw === 'deletion' || kindRaw === 'meta'
    ? kindRaw
    : 'meta';
  const line: DiffLine = { kind, text: typeof value.text === 'string' ? value.text : '' };
  if (typeof value.oldNo === 'number' && Number.isFinite(value.oldNo)) line.oldNo = Math.floor(value.oldNo);
  if (typeof value.newNo === 'number' && Number.isFinite(value.newNo)) line.newNo = Math.floor(value.newNo);
  return line;
}

function parseReviewHunk(data: unknown): CodeReviewFileDiffHunkData | null {
  const value = wireRecord(data);
  if (!value) return null;
  const header = readNonemptyString(value.header);
  const contentHash = readNonemptyString(value.contentHash);
  if (header === null || contentHash === null) return null;
  const lines: DiffLine[] = [];
  if (value.lines !== undefined) {
    if (!Array.isArray(value.lines)) return null;
    for (const item of value.lines) {
      const line = normalizeReviewLine(item);
      if (line) lines.push(line);
    }
  }
  return {
    index: wireCount(value.index) ?? 0,
    header,
    oldStart: wireCount(value.oldStart) ?? 0,
    oldCount: wireCount(value.oldCount) ?? 0,
    newStart: wireCount(value.newStart) ?? 0,
    newCount: wireCount(value.newCount) ?? 0,
    contentHash,
    totalLines: wireCount(value.totalLines) ?? 0,
    complete: value.complete === true,
    lineStart: wireCount(value.lineStart) ?? 0,
    lines,
  };
}

/** `code_review_file_diff`: one bounded page of a single file's complete diff
 *  for the open review. Empty of hunks is a legal page (a binary file, or a
 *  page past the last hunk); an unknown snapshot/path is never it. */
export function parseCodeReviewFileDiffPage(data: unknown): CodeReviewFileDiffPageData | null {
  const value = wireRecord(data);
  if (!value) return null;
  const snapshotId = readNonemptyString(value.snapshotId);
  const path = readNonemptyString(value.path);
  if (snapshotId === null || path === null) return null;
  const hunks = value.hunks === undefined ? [] : wireList(value.hunks, parseReviewHunk);
  if (hunks === null) return null;
  const rawNext = value.nextCursor;
  if (rawNext !== undefined && rawNext !== null && wireCount(rawNext) === null) return null;
  const hunkCount = wireCount(value.hunkCount) ?? 0;
  const maxIndex = hunks.reduce((highest, hunk) => Math.max(highest, hunk.index + 1), 0);
  return {
    snapshotId,
    path,
    previousPath: wireOptionalString(value.previousPath),
    binary: value.binary === true,
    status: typeof value.status === 'string' && CODE_REVIEW_FILE_STATUSES[value.status]
      ? (value.status as CodeReviewFileStatus)
      : 'changed',
    hunks,
    cursor: wireCount(value.cursor) ?? 0,
    nextCursor: wireCount(rawNext),
    hasMore: value.hasMore === true,
    totalLines: wireCount(value.totalLines) ?? 0,
    hunkCount: Math.max(hunkCount, maxIndex),
    byteCapped: value.byteCapped === true,
  };
}

/** `code_review_file_context`: a bounded window of unchanged lines around a
 *  hunk. Snapshot identity and path containment are the same as the file diff
 *  page, so a stale window can never be spliced into a newer review. */
export function parseCodeReviewFileContextPage(data: unknown): CodeReviewFileContextPageData | null {
  const value = wireRecord(data);
  if (!value) return null;
  const snapshotId = readNonemptyString(value.snapshotId);
  const path = readNonemptyString(value.path);
  if (snapshotId === null || path === null) return null;
  const lines: DiffLine[] = [];
  if (value.lines !== undefined) {
    if (!Array.isArray(value.lines)) return null;
    for (const item of value.lines) {
      const line = normalizeReviewLine(item);
      if (line) lines.push(line);
    }
  }
  return {
    snapshotId,
    path,
    oldLineCount: wireCount(value.oldLineCount) ?? 0,
    newLineCount: wireCount(value.newLineCount) ?? 0,
    binary: value.binary === true,
    lines,
  };
}

/** `code_review_close`: `closed: false` with `staleReview: true` is the typed
 *  no-op of a retired panel's cleanup — a controller opened after it survives. */
export function parseCodeReviewCloseResult(data: unknown): CodeReviewCloseResultData | null {
  const value = wireRecord(data);
  if (!value || typeof value.closed !== 'boolean') return null;
  return { closed: value.closed, staleReview: value.staleReview === true };
}

/** `code_review_set_model` ack: the display name of the model the review
 *  replies now use. */
export function parseCodeReviewSetModelAck(data: unknown): CodeReviewModelAckData | null {
  const value = wireRecord(data);
  if (!value || value.ok !== true) return null;
  const model = wireOptionalString(value.model);
  return model === null ? null : { model };
}

/** TLDR lifecycle of the controller's single summary operation. */
export type CodeReviewTldrState = 'idle' | 'running' | 'done' | 'failed';

/** Typed stop target of `code_review_stop`; `tldr` targets the summary run. */
export type CodeReviewStopOperation = 'autoReview' | 'highlight' | 'tldr';

/** `code_review_tldr` ack: the controller accepted the start and named the
 *  generation every later snapshot must reach to supersede it. */
export interface CodeReviewTldrAckData {
  state: 'running';
  tldrGeneration: number;
}

/** Fail-closed: a malformed ack is never a running generation. */
export function parseCodeReviewTldrAck(data: unknown): CodeReviewTldrAckData | null {
  const value = wireRecord(data);
  if (!value || value.ok !== true || value.state !== 'running') return null;
  const tldrGeneration = wireCount(value.tldrGeneration);
  return tldrGeneration === null ? null : { state: 'running', tldrGeneration };
}

const COMMIT_DATE_RE = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})$/;

export interface CodeReviewCommitWire {
  commit: string;
  parentCommit: string | null;
  parentCount: number;
  subject: string;
  author: string;
  authorDate: string;
}

export interface CodeReviewCommitsData {
  commits: CodeReviewCommitWire[];
  truncated: boolean;
}

function parseCodeReviewCommit(data: unknown): CodeReviewCommitWire | null {
  const value = wireRecord(data);
  if (!value) return null;
  const commit = readNonemptyString(value.commit);
  const subject = typeof value.subject === 'string' ? value.subject : null;
  const author = typeof value.author === 'string' ? value.author : null;
  const parentCount = wireCount(value.parentCount);
  const authorDate = readNonemptyString(value.authorDate);
  if (commit === null || subject === null || author === null) return null;
  if (parentCount === null || authorDate === null || !COMMIT_DATE_RE.test(authorDate)) return null;
  if (value.parentCommit === null) {
    return { commit, parentCommit: null, parentCount, subject, author, authorDate };
  }
  const parentCommit = readNonemptyString(value.parentCommit);
  if (parentCommit === null) return null;
  return { commit, parentCommit, parentCount, subject, author, authorDate };
}

export function parseCodeReviewCommits(data: unknown): CodeReviewCommitsData | null {
  const value = wireRecord(data);
  if (!value) return null;
  const commits = wireList(value.commits, parseCodeReviewCommit);
  if (commits === null || typeof value.truncated !== 'boolean') return null;
  if (!hasUniqueIdentities(commits, (row) => row.commit)) return null;
  return { commits, truncated: value.truncated };
}

// ---------------------------------------------------------------------------
// Worktree code review — the workspace view model behind
// mobile/src/screens/ReviewPanel.tsx.
//
// Mirrors crates/pi-cli/web/src/codeReview.ts so the Android workspace behaves
// like /web: the same `/code-review` scope grammar, the same fail-closed
// projection of the shared review snapshot, the same hunk identity a
// comment/abort targets, the same bounded per-file diff paging, and the same
// idle-vs-batch snapshot polling cadence.
// ---------------------------------------------------------------------------

/** How the snapshot produced a file's diff body. */
export type CodeReviewBodyState = 'complete' | 'preview' | 'onDemand' | 'binary' | 'byteCapped';

const CODE_REVIEW_BODY_STATES: Record<string, true> = {
  complete: true,
  preview: true,
  onDemand: true,
  binary: true,
  byteCapped: true,
};

/** Reply lifecycle of a thread's current/last generation. */
export type ReviewReplyStatus = 'idle' | 'streaming' | 'tool' | 'completed' | 'failed' | 'cancelled';

/** Auto Review / Highlight run states that make the snapshot poll fast. */
export type ReviewAutoState = 'idle' | 'triaging' | 'running' | 'done' | 'failed';
/** One typed display category. `clean` is derived from an empty finding
 *  list; it is never a finding category on the wire. */
export type ReviewCategory = 'critical' | 'overdesign' | 'naming' | 'clean';
/** A finding category: `clean` is never one. */
export type ReviewFindingCategory = Exclude<ReviewCategory, 'clean'>;
/** Which side of the diff a finding's line range indexes. */
export type ReviewFindingSide = 'old' | 'new';

/** One structured finding: a single side and one inclusive, one-based file
 *  line range, exactly as the backend validated it against the diff rows. */
export interface ReviewFinding {
  category: ReviewFindingCategory;
  side: ReviewFindingSide;
  startLine: number;
  endLine: number;
  summary: string;
  evidence: string;
}

/** One hunk's validated findings, bound to the exact identity the model was
 *  asked about. An empty `findings` is a reviewed-clean hunk. */
export interface HunkReviewResult {
  identity: ReviewHunkIdentity;
  findings: ReviewFinding[];
}

/** Display precedence only: critical > overdesign > naming > clean. */
const REVIEW_CATEGORY_RANK: Record<ReviewCategory, number> = {
  critical: 3,
  overdesign: 2,
  naming: 1,
  clean: 0,
};

/** Literal membership only: a Record lookup would accept inherited keys
 *  like `constructor` or `toString` as a category or side. */
function isReviewFindingCategory(value: unknown): value is ReviewFindingCategory {
  return value === 'critical' || value === 'overdesign' || value === 'naming';
}

function isReviewFindingSide(value: unknown): value is ReviewFindingSide {
  return value === 'old' || value === 'new';
}

/** Findings kept from one reviewed hunk (the backend response budget). */
export const REVIEW_MAX_FINDINGS_PER_HUNK = 20;
/** Strict finding summary UTF-8 byte cap. */
export const REVIEW_MAX_SUMMARY_BYTES = 512;
/** Strict finding evidence UTF-8 byte cap. */
export const REVIEW_MAX_EVIDENCE_BYTES = 8192;
/** Inclusive line coordinates cap (`u32::MAX` on the backend). */
export const REVIEW_MAX_LINE = 4_294_967_295;

export type ReviewHighlightState = 'idle' | 'running' | 'done' | 'failed';

const REVIEW_REPLY_STATUSES: Record<string, true> = {
  idle: true,
  streaming: true,
  tool: true,
  completed: true,
  failed: true,
  cancelled: true,
};

const REVIEW_AUTO_STATES: Record<string, true> = {
  idle: true,
  triaging: true,
  running: true,
  done: true,
  failed: true,
};

const REVIEW_HIGHLIGHT_STATES: Record<string, true> = {
  idle: true,
  running: true,
  done: true,
  failed: true,
};

/** Seed hunk of the snapshot patch. A fetched per-file page descriptor
 *  (`CodeReviewFileDiffHunkData`) upgrades it once the body loads. */
export interface ReviewDiffHunk {
  header: string;
  oldStart: number;
  oldCount: number;
  newStart: number;
  newCount: number;
  contentHash: string;
  lines: DiffLine[];
}

/** Exact target of `code_review_comment` / `code_review_abort`: path, ranges,
 *  content hash, and the snapshot that proved them. */
export interface ReviewHunkIdentity {
  snapshotId: string;
  path: string;
  oldStart: number;
  oldCount: number;
  newStart: number;
  newCount: number;
  contentHash: string;
}

export interface ReviewComment {
  role: string;
  text: string;
  partial: boolean;
  /** Finalized thinking of an assistant reply; '' when the wire has none. */
  thinking: string;
}

export interface ReviewThread {
  identity: ReviewHunkIdentity;
  comments: ReviewComment[];
  /** Live text of the in-flight reply ('' once the reply is terminal). */
  streamingText: string;
  /** Live thinking accumulation of the in-flight reply ('' when none). */
  streamingThinking: string;
  error: string | null;
  isStreaming: boolean;
  /** Abort availability. Fail-closed: never inferred from `isStreaming`. */
  isCancellable: boolean;
  replyStatus: ReviewReplyStatus;
}

export interface ReviewFile {
  path: string;
  previousPath: string | null;
  status: CodeReviewFileStatus;
  binary: boolean;
  bodyState: CodeReviewBodyState;
  insertions: number;
  deletions: number;
  truncated: boolean;
  message: string | null;
  hunks: ReviewDiffHunk[];
}

export interface ReviewSnapshot {
  comparisonLabel: string;
  /** Stable digest of the compared bytes: the stale guard for diff pages. */
  snapshotId: string;
  /** Controller identity `code_review_close` names. */
  reviewId: string;
  error: string | null;
  totalInsertions: number;
  totalDeletions: number;
  /** True when the totals cover EVERY changed file. */
  totalsComplete: boolean;
  /** Untracked files omitted from the tracked-only review. */
  untrackedNotShown: number;
  files: ReviewFile[];
  threads: ReviewThread[];
  /** Concurrently active review replies. */
  activeCount: number;
  /** Owned-or-pending work; never inferred from `activeCount`. */
  internalBusy: boolean;
  autoReviewState: ReviewAutoState;
  highlightState: ReviewHighlightState;
  /** Authoritative per-hunk `reviewFindings`. Malformed, duplicate-identity,
   *  or over-budget rows reject the whole payload. */
  reviewFindings: HunkReviewResult[];
  reviewModelLabel: string | null;
  reviewModelProvider: string | null;
  reviewModelId: string | null;
  /** TLDR lifecycle of the controller's single summary operation. */
  tldrState: CodeReviewTldrState;
  /** Monotonic TLDR operation counter: the ack/snapshot generation guard. */
  tldrGeneration: number;
  /** Snapshot the current/last TLDR ran against; null before any run. A
   *  summary whose id is not THIS snapshot's is stale and never renders. */
  tldrSnapshotId: string | null;
  /** Safe failure text of the last TLDR run. */
  tldrError: string | null;
  /** Last approved TLDR document; '' when none. */
  tldrMarkdown: string;
}

/** Snapshot used before any data arrives and when a payload is unusable, so
 *  the panel always renders an actionable state instead of throwing. */
export function emptyReviewSnapshot(error: string | null = null): ReviewSnapshot {
  return {
    comparisonLabel: '',
    snapshotId: '',
    reviewId: '',
    error,
    totalInsertions: 0,
    totalDeletions: 0,
    totalsComplete: false,
    untrackedNotShown: 0,
    files: [],
    threads: [],
    activeCount: 0,
    internalBusy: false,
    autoReviewState: 'idle',
    highlightState: 'idle',
    reviewFindings: [],
    reviewModelLabel: null,
    reviewModelProvider: null,
    reviewModelId: null,
    tldrState: 'idle',
    tldrGeneration: 0,
    tldrSnapshotId: null,
    tldrError: null,
    tldrMarkdown: '',
  };
}

function parseReviewComment(data: unknown): ReviewComment | null {
  const value = wireRecord(data);
  if (!value) return null;
  return {
    role: wireOptionalString(value.role) ?? 'system',
    text: typeof value.text === 'string' ? value.text : '',
    partial: value.partial === true,
    thinking: typeof value.thinking === 'string' ? value.thinking : '',
  };
}

function parseReviewIdentity(data: unknown): ReviewHunkIdentity | null {
  const value = wireRecord(data);
  if (!value) return null;
  const path = readNonemptyString(value.path);
  const contentHash = readNonemptyString(value.contentHash);
  // path + contentHash are the minimum a comment can target.
  if (path === null || contentHash === null) return null;
  return {
    snapshotId: readNonemptyString(value.snapshotId) ?? '',
    path,
    oldStart: wireCount(value.oldStart) ?? 0,
    oldCount: wireCount(value.oldCount) ?? 0,
    newStart: wireCount(value.newStart) ?? 0,
    newCount: wireCount(value.newCount) ?? 0,
    contentHash,
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

function parseReviewFinding(data: unknown): ReviewFinding | null {
  const value = wireRecord(data);
  if (!value || !wireExactKeys(value, ['category', 'side', 'startLine', 'endLine', 'summary', 'evidence'])) return null;
  const category = value.category;
  if (!isReviewFindingCategory(category)) return null;
  const side = value.side;
  if (!isReviewFindingSide(side)) return null;
  const startLine = wireCount(value.startLine);
  const endLine = wireCount(value.endLine);
  // One-based inclusive file coordinates: zero, reversed, and > u32::MAX fail closed.
  if (startLine === null || endLine === null || startLine < 1 || startLine > endLine || endLine > REVIEW_MAX_LINE) {
    return null;
  }
  const summary = value.summary;
  const evidence = value.evidence;
  if (typeof summary !== 'string' || summary.trim() === '' || reviewUtf8ByteLength(summary) > REVIEW_MAX_SUMMARY_BYTES) {
    return null;
  }
  if (typeof evidence !== 'string' || evidence.trim() === '' || reviewUtf8ByteLength(evidence) > REVIEW_MAX_EVIDENCE_BYTES) {
    return null;
  }
  return {
    category,
    side,
    startLine,
    endLine,
    summary,
    evidence,
  };
}

/** Findings identity: all seven fields present and valid — a stale or partial
 *  identity can never name a hunk the snapshot proved. */
function parseReviewResultIdentity(data: unknown): ReviewHunkIdentity | null {
  const value = wireRecord(data);
  if (!value || !wireExactKeys(value, ['snapshotId', 'path', 'oldStart', 'oldCount', 'newStart', 'newCount', 'contentHash'])) {
    return null;
  }
  const snapshotId = readNonemptyString(value.snapshotId);
  const path = readNonemptyString(value.path);
  const contentHash = readNonemptyString(value.contentHash);
  const oldStart = wireCount(value.oldStart);
  const oldCount = wireCount(value.oldCount);
  const newStart = wireCount(value.newStart);
  const newCount = wireCount(value.newCount);
  if (
    snapshotId === null || path === null || contentHash === null
    || oldStart === null || oldCount === null || newStart === null || newCount === null
  ) return null;
  return { snapshotId, path, oldStart, oldCount, newStart, newCount, contentHash };
}

/** The authoritative per-hunk `reviewFindings` collection. Absent reads as
 *  unreviewed; any malformed row rejects the whole payload — a snapshot never
 *  renders a half-valid finding list. Each hunk carries at most one model
 *  response's findings; there is no snapshot-wide cap that would drop valid
 *  findings from other hunks. */
function parseReviewFindings(data: unknown): HunkReviewResult[] | undefined {
  if (data == null) return [];
  if (!Array.isArray(data)) return undefined;
  const seenIdentities = new Set<string>();
  const results: HunkReviewResult[] = [];
  for (const entry of data) {
    const value = wireRecord(entry);
    if (!value || !wireExactKeys(value, ['identity', 'findings'])) return undefined;
    const identity = parseReviewResultIdentity(value.identity);
    if (!identity) return undefined;
    const key = reviewHunkKeyFor(identity, identity);
    if (seenIdentities.has(key)) return undefined;
    if (!Array.isArray(value.findings) || value.findings.length > REVIEW_MAX_FINDINGS_PER_HUNK) return undefined;
    const seenFindings = new Set<string>();
    const findings: ReviewFinding[] = [];
    for (const raw of value.findings) {
      const finding = parseReviewFinding(raw);
      if (!finding) return undefined;
      const findingKey = JSON.stringify(finding);
      if (seenFindings.has(findingKey)) return undefined;
      seenFindings.add(findingKey);
      findings.push(finding);
    }
    seenIdentities.add(key);
    results.push({ identity, findings });
  }
  return results;
}

/** Highest category among a hunk's findings; `clean` when reviewed empty.
 *  Display precedence, not a shared defect class. */
export function reviewAggregateCategory(findings: readonly ReviewFinding[]): ReviewCategory {
  let best: ReviewCategory = 'clean';
  for (const finding of findings) {
    if (REVIEW_CATEGORY_RANK[finding.category] > REVIEW_CATEGORY_RANK[best]) best = finding.category;
  }
  return best;
}

/** Distinct prose label. Policy findings are never called critical. */
export function reviewCategoryLabel(category: ReviewCategory): string {
  if (category === 'critical') return 'Critical';
  if (category === 'overdesign') return 'Overdesign';
  if (category === 'naming') return 'Naming';
  return 'Clean';
}

/** Stage copy from the authoritative snapshot states. No local phase mirror. */
export function reviewStageFeedback(
  snapshot: Pick<ReviewSnapshot, 'autoReviewState' | 'highlightState'> | null | undefined,
): string | null {
  if (!snapshot) return null;
  if (snapshot.autoReviewState === 'triaging') return 'Auto Review triaging';
  if (snapshot.autoReviewState === 'running') return 'Auto Review running';
  if (snapshot.highlightState === 'running') return 'Highlight running';
  if (snapshot.autoReviewState === 'done') return 'Auto Review done';
  if (snapshot.highlightState === 'done') return 'Highlight done';
  return null;
}

/** Findings of exactly this hunk identity; [] when the hunk was never
 *  reviewed or the identity is stale. */
export function reviewFindingsForHunk(
  reviewFindings: readonly HunkReviewResult[],
  identity: ReviewHunkIdentity,
): ReviewFinding[] {
  return reviewFindings.find((entry) => reviewHunkIdentityEqual(entry.identity, identity))?.findings ?? [];
}

/** The finding a typed reference names right now; undefined when the hunk's
 *  accepted findings changed and the index no longer names this finding. */
export function reviewFindingAtRef(
  reviewFindings: readonly HunkReviewResult[],
  ref: ReviewFindingRef,
): ReviewFinding | undefined {
  return reviewFindingsForHunk(reviewFindings, ref.identity)[ref.findingIndex];
}

/** Typed selection of one finding inside the authoritative collection.
 *  `findingIndex` indexes the validated array; never a summary or category. */
export interface ReviewFindingRef {
  identity: ReviewHunkIdentity;
  findingIndex: number;
}

/** One entry of the derived global finding list: its typed reference, the
 *  finding, and the snapshot positions navigation orders by. */
export interface ReviewFindingEntry {
  ref: ReviewFindingRef;
  finding: ReviewFinding;
  path: string;
  hunkKey: string;
  fileIndex: number;
  hunkIndex: number;
  /** Physical row span inside the hunk's line stream. */
  rowFirst: number;
  rowLast: number;
}

/** Whether ONE row carries a coordinate on the finding's side inside the
 *  inclusive range. Meta rows never match; an addition has no old coordinate
 *  and a deletion no new one, so a wrong-side range cannot claim a row. */
export function reviewFindingCoversRow(
  line: Pick<DiffLine, 'kind' | 'oldNo' | 'newNo'>,
  finding: Pick<ReviewFinding, 'side' | 'startLine' | 'endLine'>,
): boolean {
  if (line.kind === 'meta') return false;
  const number = finding.side === 'old' ? line.oldNo : line.newNo;
  return number !== undefined && number >= finding.startLine && number <= finding.endLine;
}

/** Physical rows of one hunk for a finding whose range the hunk FULLY
 *  covers: every integer in the inclusive range must exist as a numbered row
 *  coordinate on the finding's side. Coverage is proven by walking the actual
 *  rows (the coordinate set is bounded by the row count — no numeric interval
 *  is allocated) and comparing the in-range count to the range width. A
 *  partially represented range — including one whose missing rows are still
 *  pending in a paged body — returns null instead of a guessed span. */
export function reviewFindingRowRange(
  lines: readonly DiffLine[],
  finding: Pick<ReviewFinding, 'side' | 'startLine' | 'endLine'>,
): { first: number; last: number } | null {
  const inRange = new Set<number>();
  let first = -1;
  let last = -1;
  for (let index = 0; index < lines.length; index += 1) {
    if (!reviewFindingCoversRow(lines[index], finding)) continue;
    const number = finding.side === 'old' ? lines[index].oldNo : lines[index].newNo;
    inRange.add(number!);
    if (first === -1) first = index;
    last = index;
  }
  if (first === -1 || inRange.size !== finding.endLine - finding.startLine + 1) return null;
  return { first, last };
}

/** Exact equality of all seven identity fields — the lookup key alone
 *  excludes `snapshotId`, so a stale-snapshot result could otherwise attach
 *  to a hunk of a newer snapshot. */
export function reviewHunkIdentityEqual(
  a: ReviewHunkIdentity,
  b: ReviewHunkIdentity,
): boolean {
  return a.snapshotId === b.snapshotId
    && a.path === b.path
    && a.oldStart === b.oldStart
    && a.oldCount === b.oldCount
    && a.newStart === b.newStart
    && a.newCount === b.newCount
    && a.contentHash === b.contentHash;
}

/** The derived global finding list in navigation order: file position in
 *  `snapshot.files`, hunk position in the file, first matching physical row,
 *  last matching row, old before new, category priority (critical first),
 *  then summary/evidence lexical for deterministic ties. */
export function orderReviewFindings(snapshot: ReviewSnapshot): ReviewFindingEntry[] {
  const entries: ReviewFindingEntry[] = [];
  snapshot.files.forEach((file, fileIndex) => {
    file.hunks.forEach((hunk, hunkIndex) => {
      const identity = reviewHunkIdentityFor(snapshot.snapshotId, file, hunk);
      const result = snapshot.reviewFindings.find(
        (entry) => reviewHunkIdentityEqual(entry.identity, identity),
      );
      if (!result) return;
      const key = reviewHunkKeyFor(file, hunk);
      result.findings.forEach((finding, findingIndex) => {
        const rows = reviewFindingRowRange(hunk.lines, finding);
        entries.push({
          ref: { identity, findingIndex },
          finding,
          path: file.path,
          hunkKey: key,
          fileIndex,
          hunkIndex,
          rowFirst: rows ? rows.first : hunk.lines.length,
          rowLast: rows ? rows.last : hunk.lines.length,
        });
      });
    });
  });
  const categoryRank = (category: ReviewFindingCategory): number =>
    category === 'critical' ? 2 : category === 'overdesign' ? 1 : 0;
  return entries.sort((a, b) =>
    a.fileIndex - b.fileIndex
    || a.hunkIndex - b.hunkIndex
    || a.rowFirst - b.rowFirst
    || a.rowLast - b.rowLast
    || (a.finding.side === b.finding.side ? 0 : a.finding.side === 'old' ? -1 : 1)
    || categoryRank(b.finding.category) - categoryRank(a.finding.category)
    || (a.finding.summary < b.finding.summary ? -1 : a.finding.summary > b.finding.summary ? 1 : 0)
    || (a.finding.evidence < b.finding.evidence ? -1 : a.finding.evidence > b.finding.evidence ? 1 : 0)
  );
}

export type ReviewFindingDirection = 'previous' | 'next';

/** The neighbor finding in the global ordered list. No wrapping: with no
 *  (or filtered-out) selection, next selects the first and previous the last
 *  entry; at a boundary the move is null and its control is disabled. */
export function reviewFindingNeighbor(
  entries: readonly ReviewFindingEntry[],
  selected: ReviewFindingRef | null,
  direction: ReviewFindingDirection,
): ReviewFindingEntry | null {
  if (entries.length === 0) return null;
  if (selected) {
    const index = entries.findIndex((entry) => (
      entry.ref.findingIndex === selected.findingIndex
      && reviewHunkIdentityEqual(entry.ref.identity, selected.identity)
    ));
    if (index !== -1) {
      const next = direction === 'next' ? index + 1 : index - 1;
      return next >= 0 && next < entries.length ? entries[next] : null;
    }
  }
  return direction === 'next' ? entries[0] : entries[entries.length - 1];
}


function parseReviewThread(data: unknown): ReviewThread | null {
  const value = wireRecord(data);
  if (!value) return null;
  const identity = parseReviewIdentity(value.identity);
  if (!identity) return null;
  const comments: ReviewComment[] = [];
  if (Array.isArray(value.comments)) {
    for (const entry of value.comments) {
      const comment = parseReviewComment(entry);
      if (comment) comments.push(comment);
    }
  }
  const rawStatus = value.replyStatus;
  const replyStatus: ReviewReplyStatus =
    typeof rawStatus === 'string' && REVIEW_REPLY_STATUSES[rawStatus]
      ? rawStatus as ReviewReplyStatus
      : value.isStreaming === true ? 'streaming' : 'idle';
  const isStreaming = replyStatus === 'streaming' || replyStatus === 'tool';
  return {
    identity,
    comments,
    streamingText: isStreaming && typeof value.streamingText === 'string' ? value.streamingText : '',
    streamingThinking: isStreaming && typeof value.streamingThinking === 'string'
      ? value.streamingThinking
      : '',
    error: wireOptionalString(value.error),
    isStreaming,
    isCancellable: value.isCancellable === true,
    replyStatus,
  };
}

/** Threads arrive as an array or as a map keyed by an opaque id. */
function reviewThreadRows(raw: unknown): unknown[] {
  if (Array.isArray(raw)) return raw;
  const map = wireRecord(raw);
  return map ? Object.keys(map).map((key) => map[key]) : [];
}

/** Snapshot seed hunk: `contentHash` is the only required identity piece, and
 *  a hunk with no body is legal (its page then supplies the lines). */
function parseReviewSnapshotHunk(data: unknown): ReviewDiffHunk | null {
  const value = wireRecord(data);
  if (!value) return null;
  const contentHash = readNonemptyString(value.contentHash);
  if (contentHash === null) return null;
  const lines: DiffLine[] = [];
  if (value.lines !== undefined) {
    if (!Array.isArray(value.lines)) return null;
    for (const item of value.lines) {
      const line = normalizeReviewLine(item);
      if (line) lines.push(line);
    }
  }
  return {
    header: typeof value.header === 'string' ? value.header : '',
    oldStart: wireCount(value.oldStart) ?? 0,
    oldCount: wireCount(value.oldCount) ?? 0,
    newStart: wireCount(value.newStart) ?? 0,
    newCount: wireCount(value.newCount) ?? 0,
    contentHash,
    lines,
  };
}

function parseReviewFile(data: unknown): ReviewFile | null {
  const value = wireRecord(data);
  if (!value) return null;
  const path = readNonemptyString(value.path);
  if (path === null) return null;
  const hunks: ReviewDiffHunk[] = [];
  if (Array.isArray(value.hunks)) {
    for (const entry of value.hunks) {
      const hunk = parseReviewSnapshotHunk(entry);
      if (hunk) hunks.push(hunk);
    }
  }
  const binary = value.binary === true;
  const rawBody = value.bodyState;
  return {
    path,
    previousPath: wireOptionalString(value.previousPath),
    status: typeof value.status === 'string' && CODE_REVIEW_FILE_STATUSES[value.status]
      ? value.status as CodeReviewFileStatus
      : 'changed',
    binary,
    bodyState: typeof rawBody === 'string' && CODE_REVIEW_BODY_STATES[rawBody]
      ? rawBody as CodeReviewBodyState
      : binary ? 'binary' : 'preview',
    insertions: wireCount(value.insertions) ?? 0,
    deletions: wireCount(value.deletions) ?? 0,
    truncated: value.truncated === true,
    message: wireOptionalString(value.message),
    hunks,
  };
}

/** Normalize a `code_review_open|snapshot|refresh|comment|abort` payload.
 *  Null for an unusable shape (a panel renders `emptyReviewSnapshot` copy);
 *  individual malformed files/hunks/threads drop instead of blanking the
 *  workspace, exactly like the /web reader. */
export function parseReviewSnapshot(data: unknown): ReviewSnapshot | null {
  const value = wireRecord(data);
  if (!value) return null;
  const files: ReviewFile[] = [];
  if (Array.isArray(value.files)) {
    for (const entry of value.files) {
      const file = parseReviewFile(entry);
      if (file) files.push(file);
    }
  }
  const threads: ReviewThread[] = [];
  for (const entry of reviewThreadRows(value.threads)) {
    const thread = parseReviewThread(entry);
    if (thread) threads.push(thread);
  }
  const snapshotId = readNonemptyString(value.snapshotId) ?? '';
  const reviewFindings = parseReviewFindings(value.reviewFindings);
  if (reviewFindings === undefined) return null;
  // Every result must name a hunk of THIS snapshot with all seven identity
  // fields: a stale snapshot id, foreign path, or unmatched range/hash is a
  // foreign result and rejects the whole payload.
  for (const result of reviewFindings) {
    if (result.identity.snapshotId !== snapshotId) return null;
    const file = files.find((entry) => entry.path === result.identity.path);
    const hunk = file?.hunks.find((entry) =>
      entry.oldStart === result.identity.oldStart
      && entry.oldCount === result.identity.oldCount
      && entry.newStart === result.identity.newStart
      && entry.newCount === result.identity.newCount
      && entry.contentHash === result.identity.contentHash);
    if (!file || !hunk) return null;
    // A complete hunk must fully cover each finding's side-specific range
    // with actual numbered rows; a partial/paged body defers that proof to
    // the navigation gate instead of guessing at rows it has not loaded.
    if (reviewHunkParsedComplete(hunk)) {
      for (const finding of result.findings) {
        if (reviewFindingRowRange(hunk.lines, finding) === null) return null;
      }
    }
  }
  const streaming = threads.reduce((count, thread) => count + (thread.isStreaming ? 1 : 0), 0);
  const rawAuto = value.autoReviewState;
  const rawHighlight = value.highlightState;
  const rawTldr = value.tldrState;
  // Supplied TLDR fields are validated exactly; a malformed one rejects the
  // whole payload instead of coercing into a plausible idle snapshot. Absent
  // fields keep their neutral defaults (older projections omit them).
  if (rawTldr !== undefined && rawTldr !== 'idle' && rawTldr !== 'running' && rawTldr !== 'done' && rawTldr !== 'failed') return null;
  if (value.tldrGeneration !== undefined && wireCount(value.tldrGeneration) === null) return null;
  if (value.tldrSnapshotId !== undefined && value.tldrSnapshotId !== null && typeof value.tldrSnapshotId !== 'string') return null;
  if (value.tldrError !== undefined && value.tldrError !== null && typeof value.tldrError !== 'string') return null;
  if (value.tldrMarkdown !== undefined && typeof value.tldrMarkdown !== 'string') return null;
  return {
    comparisonLabel: wireOptionalString(value.comparisonLabel) ?? '',
    snapshotId,
    reviewId: readNonemptyString(value.reviewId) ?? '',
    error: wireOptionalString(value.error),
    totalInsertions: wireCount(value.totalInsertions) ?? 0,
    totalDeletions: wireCount(value.totalDeletions) ?? 0,
    totalsComplete: value.totalsComplete === true,
    untrackedNotShown: wireCount(value.untrackedNotShown) ?? 0,
    files,
    threads,
    // An absent/negative count falls back to the threads that claim life.
    activeCount: wireCount(value.activeCount) ?? streaming,
    internalBusy: value.internalBusy === true,
    autoReviewState: typeof rawAuto === 'string' && REVIEW_AUTO_STATES[rawAuto]
      ? rawAuto as ReviewAutoState
      : 'idle',
    highlightState: typeof rawHighlight === 'string' && REVIEW_HIGHLIGHT_STATES[rawHighlight]
      ? rawHighlight as ReviewHighlightState
      : 'idle',
    reviewFindings,
    reviewModelLabel: wireOptionalString(value.reviewModelLabel),
    reviewModelProvider: wireOptionalString(value.reviewModelProvider),
    reviewModelId: wireOptionalString(value.reviewModelId),
    tldrState: (rawTldr ?? 'idle') as CodeReviewTldrState,
    tldrGeneration: wireCount(value.tldrGeneration) ?? 0,
    tldrSnapshotId: typeof value.tldrSnapshotId === 'string' ? value.tldrSnapshotId : null,
    tldrError: typeof value.tldrError === 'string' ? value.tldrError : null,
    tldrMarkdown: typeof value.tldrMarkdown === 'string' ? value.tldrMarkdown : '',
  };
}

/** Typed `code_review_not_open` rejection: the controller answering this
 *  session is gone (closed elsewhere, or the runner restarted). */
export function isNoOpenCodeReviewFailure(data: unknown): boolean {
  const value = wireRecord(data);
  return value !== null && value.code === 'code_review_not_open';
}

/** Empty-state copy of a comparison with no changed files; untracked files are
 *  outside the tracked-only review, so they are named instead of hidden. */
export function reviewEmptyComparisonMessage(untrackedNotShown: number): string {
  if (!Number.isFinite(untrackedNotShown) || untrackedNotShown <= 0) {
    return 'No changed files in this comparison.';
  }
  const n = Math.floor(untrackedNotShown);
  const note = n === 1
    ? '1 untracked file is not part of the review'
    : `${n} untracked files are not part of the review`;
  return `No tracked changes — ${note}`;
}

/** Stable key of a hunk within one file (ranges + content hash). */
export function reviewHunkKeyFor(
  file: Pick<ReviewFile, 'path'>,
  hunk: Pick<ReviewDiffHunk, 'oldStart' | 'oldCount' | 'newStart' | 'newCount' | 'contentHash'>,
): string {
  return `${file.path}\0${hunk.oldStart},${hunk.oldCount},${hunk.newStart},${hunk.newCount}\0${hunk.contentHash}`;
}

/** Full identity a comment/abort names for a hunk of the open snapshot. */
export function reviewHunkIdentityFor(
  snapshotId: string,
  file: Pick<ReviewFile, 'path'>,
  hunk: Pick<ReviewDiffHunk, 'oldStart' | 'oldCount' | 'newStart' | 'newCount' | 'contentHash'>,
): ReviewHunkIdentity {
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

/** The thread of a hunk identity, if the snapshot carries one. */
export function findReviewThread(
  threads: readonly ReviewThread[],
  identity: ReviewHunkIdentity,
): ReviewThread | undefined {
  const key = reviewHunkKeyFor(identity, identity);
  return threads.find((thread) => reviewHunkKeyFor(thread.identity, thread.identity) === key);
}

/** Comment/abort payloads — session stamping stays with the caller. */
export function buildCodeReviewCommentPayload(
  identity: ReviewHunkIdentity,
  comment: string,
): Extract<RunnerCommand, { type: 'code_review_comment' }> {
  return {
    type: 'code_review_comment',
    snapshotId: identity.snapshotId,
    path: identity.path,
    oldStart: identity.oldStart,
    oldCount: identity.oldCount,
    newStart: identity.newStart,
    newCount: identity.newCount,
    contentHash: identity.contentHash,
    comment,
  };
}

export function buildCodeReviewAbortPayload(
  identity: ReviewHunkIdentity,
): Extract<RunnerCommand, { type: 'code_review_abort' }> {
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

/** Start one TLDR over the controller's current pinned snapshot. `language`
 *  stays optional: the runner resolves its review-language default. */
export function buildCodeReviewTldrPayload(
  language?: string,
): Extract<RunnerCommand, { type: 'code_review_tldr' }> {
  return language === undefined
    ? { type: 'code_review_tldr' }
    : { type: 'code_review_tldr', language };
}

/** Typed stop of one review operation; `tldr` targets the summary run. */
export function buildCodeReviewStopPayload(
  operation: CodeReviewStopOperation,
): Extract<RunnerCommand, { type: 'code_review_stop' }> {
  return { type: 'code_review_stop', operation };
}

export const CODE_REVIEW_SCOPE_USAGE = 'usage: /code-review [<base> | <from> <to>]';

export type CodeReviewScopeMode = 'workingTree' | 'base' | 'range' | 'commit';

/** A revision is one token: the grammar splits the tail on whitespace. */
function isReviewRevision(value: string): boolean {
  return value !== '' && !/\s/.test(value);
}

export function parseCodeReviewScope(
  mode: CodeReviewScopeMode,
  from: string,
  to: string,
): { ok: true; command: Extract<RunnerCommand, { type: 'code_review_open' }> }
| { ok: false; error: string } {
  const base = from.trim();
  const target = to.trim();
  if (mode === 'workingTree') {
    return { ok: true, command: { type: 'code_review_open' } };
  }
  if (mode === 'base') {
    if (!isReviewRevision(base)) return { ok: false, error: CODE_REVIEW_SCOPE_USAGE };
    return { ok: true, command: { type: 'code_review_open', from: base, includeWorkingTree: true } };
  }
  if (mode === 'range') {
    if (!isReviewRevision(base) || !isReviewRevision(target)) {
      return { ok: false, error: CODE_REVIEW_SCOPE_USAGE };
    }
    return { ok: true, command: { type: 'code_review_open', from: base, to: target } };
  }
  if (mode === 'commit') {
    if (!isReviewRevision(base) || target !== '') return { ok: false, error: CODE_REVIEW_SCOPE_USAGE };
    return { ok: true, command: { type: 'code_review_open', commit: base } };
  }
  return { ok: false, error: CODE_REVIEW_SCOPE_USAGE };
}

/** Render soft cap per growth step (backend MAX_FILE_RENDER_LINES). */
export const REVIEW_RENDER_SOFT_CAP = 4000;
/** UI display hard cap for one file's diff. */
export const REVIEW_DIFF_MAX_UI_LINES = 20_000;
/** Page size requested from `code_review_file_diff` (backend MAX_FILE_PAGE_LINES). */
export const REVIEW_PAGE_LINES = 1000;

/** Accumulated diff of one file: snapshot seed hunks upgraded and extended by
 *  `code_review_file_diff` pages. `lines` is the derived flat hunk-order
 *  stream the window math and the renderer walk. */
export interface ReviewLoadedDiff {
  snapshotId: string;
  path: string;
  hunks: CodeReviewFileDiffHunkData[];
  lines: DiffLine[];
  /** Backend total hunks (0 until the first page arrives). */
  hunkCount: number;
  /** Backend cursor for the next page. */
  nextCursor: number;
  hasMore: boolean;
  totalLines: number;
  /** The backend per-file byte cap was hit: no further pages exist. */
  byteCapped: boolean;
  /** Every backend descriptor the stream knows has been merged. */
  hunksComplete: boolean;
}

function flattenReviewHunks(hunks: readonly CodeReviewFileDiffHunkData[]): DiffLine[] {
  const lines: DiffLine[] = [];
  for (const hunk of hunks) for (const line of hunk.lines) lines.push(line);
  return lines;
}

/** Whether a hunk's parsed body matches its header counts (content lines equal
 *  additions + deletions, meta lines excluded). */
function reviewHunkParsedComplete(
  hunk: Pick<ReviewDiffHunk, 'oldCount' | 'newCount' | 'lines'>,
): boolean {
  let content = 0;
  let context = 0;
  for (const line of hunk.lines) {
    if (line.kind === 'meta') continue;
    content += 1;
    if (line.kind === 'context') context += 1;
  }
  return content === hunk.oldCount + hunk.newCount - context;
}

/** Seed the per-file diff from the snapshot entry. The eager page stream
 *  starts at cursor 0: backend descriptors are authoritative and upgrade the
 *  snapshot seed (`preview`/`onDemand`/`byteCapped` bodies are partial). */
export function initReviewLoadedDiff(snapshotId: string, file: ReviewFile): ReviewLoadedDiff {
  const hunks: CodeReviewFileDiffHunkData[] = file.hunks.map((hunk, index) => ({
    index,
    header: hunk.header,
    oldStart: hunk.oldStart,
    oldCount: hunk.oldCount,
    newStart: hunk.newStart,
    newCount: hunk.newCount,
    contentHash: hunk.contentHash,
    totalLines: hunk.lines.length,
    complete: reviewHunkParsedComplete(hunk),
    // The seed lines are the head of the complete hunk.
    lineStart: 0,
    lines: hunk.lines.slice(),
  }));
  const lines = flattenReviewHunks(hunks);
  const paged = file.bodyState === 'preview'
    || file.bodyState === 'onDemand'
    || file.bodyState === 'byteCapped';
  return {
    snapshotId,
    path: file.path,
    hunks,
    lines,
    hunkCount: 0,
    nextCursor: 0,
    hasMore: paged && !file.binary,
    totalLines: lines.length,
    byteCapped: false,
    hunksComplete: file.binary,
  };
}

/** Merge one page into the loaded diff. A page the review can no longer verify
 *  — another snapshot, another path, or an out-of-order/duplicate cursor —
 *  returns the SAME state: a stale page is never spliced in. */
export function appendReviewDiffPage(
  state: ReviewLoadedDiff,
  page: CodeReviewFileDiffPageData,
): ReviewLoadedDiff {
  if (page.snapshotId !== state.snapshotId || page.path !== state.path) return state;
  if (page.cursor !== state.nextCursor) return state;
  let hunks = state.hunks;
  for (const descriptor of page.hunks) {
    const existing = hunks.find((hunk) => hunk.index === descriptor.index);
    if (!existing) {
      hunks = hunks.concat([{ ...descriptor, lines: descriptor.lines.slice() }]);
      continue;
    }
    const identityMatches =
      existing.oldStart === descriptor.oldStart
      && existing.oldCount === descriptor.oldCount
      && existing.newStart === descriptor.newStart
      && existing.newCount === descriptor.newCount
      && existing.contentHash === descriptor.contentHash;
    if (!identityMatches) {
      // The per-file data supersedes the seed (e.g. a truncated partial hunk
      // replaced by the complete one).
      hunks = hunks.map((hunk) => (
        hunk.index === descriptor.index ? { ...descriptor, lines: descriptor.lines.slice() } : hunk
      ));
      continue;
    }
    // Same identity means identical hunk text, so the seed is a prefix of the
    // page subsets: dedupe what is covered and append only the missing tail.
    let merged = existing.lines;
    if (descriptor.lineStart + descriptor.lines.length > existing.lines.length) {
      const from = Math.max(descriptor.lineStart, existing.lines.length);
      merged = existing.lines.concat(descriptor.lines.slice(from - descriptor.lineStart));
    }
    hunks = hunks.map((hunk) => (
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
          lines: merged,
        }
        : hunk
    ));
  }
  const sorted = hunks.slice().sort((a, b) => a.index - b.index);
  const lines = flattenReviewHunks(sorted);
  return {
    ...state,
    hunks: sorted,
    lines,
    hunkCount: Math.max(state.hunkCount, page.hunkCount),
    nextCursor: page.nextCursor ?? lines.length,
    hasMore: page.hasMore,
    totalLines: Math.max(state.totalLines, page.totalLines),
    byteCapped: page.byteCapped || state.byteCapped,
    hunksComplete: !page.hasMore,
  };
}

/** True when a hunk is comment-ready: the backend parsed it in full and the
 *  stream delivered every line of it. */
export function reviewHunkIsReady(
  hunk: Pick<CodeReviewFileDiffHunkData, 'complete' | 'totalLines' | 'lines'>,
): boolean {
  return hunk.complete && hunk.lines.length === hunk.totalLines;
}

export interface ReviewDiffWindowPlan {
  /** Next visible-line target (never above the UI hard cap). */
  target: number;
  /** Lines renderable right now without backend pages, capped to the UI cap. */
  localAvailable: number;
  /** Total renderable lines (local or backend-extended), capped to the UI cap. */
  totalAvailable: number;
  /** True when the window can still grow. */
  canLoadMore: boolean;
  /** True when reaching `target` requires `code_review_file_diff` pages. */
  needsFetch: boolean;
  /** True when the file body exceeds the UI hard cap. */
  hardCapped: boolean;
}

/** Grow the per-file window in soft-cap chunks up to the UI hard cap, exactly
 *  like the /web plan (`FILE_WINDOW_CHUNK_LINES` = the render soft cap). */
export function planReviewDiffWindow(
  loaded: ReviewLoadedDiff,
  windowLimit: number,
  chunk = REVIEW_RENDER_SOFT_CAP,
): ReviewDiffWindowPlan {
  const known = Math.max(loaded.lines.length, loaded.totalLines);
  const localAvailable = Math.min(loaded.lines.length, REVIEW_DIFF_MAX_UI_LINES);
  const backendAvailable = loaded.hasMore;
  // While pages are still arriving the true total is unknown: the window may
  // plan up to the hard cap. Once the stream is consumed the total is bounded.
  const totalAvailable = backendAvailable && !loaded.hunksComplete
    ? REVIEW_DIFF_MAX_UI_LINES
    : Math.min(known, REVIEW_DIFF_MAX_UI_LINES);
  const current = Math.max(0, Math.min(windowLimit, totalAvailable));
  const target = Math.max(current, Math.min(current + chunk, totalAvailable));
  return {
    target,
    localAvailable,
    totalAvailable,
    canLoadMore: target > current,
    needsFetch: target > localAvailable && backendAvailable && !loaded.hunksComplete,
    hardCapped: known > REVIEW_DIFF_MAX_UI_LINES
      || (backendAvailable && current >= REVIEW_DIFF_MAX_UI_LINES),
  };
}

/** Bounded notice when the visible window is capped; null when nothing is
 *  hidden. */
export function reviewDiffStatusText(
  loaded: ReviewLoadedDiff,
  windowLimit: number,
): string | null {
  const cap = REVIEW_DIFF_MAX_UI_LINES.toLocaleString();
  if (loaded.byteCapped) {
    const shown = Math.min(windowLimit, loaded.lines.length).toLocaleString();
    return `Loaded ${shown} lines — the file body exceeds the size limit; later lines are not available.`;
  }
  const plan = planReviewDiffWindow(loaded, windowLimit);
  if (plan.canLoadMore || !plan.hardCapped) return null;
  const rendered = Math.min(windowLimit, REVIEW_DIFF_MAX_UI_LINES).toLocaleString();
  if (loaded.hunksComplete) {
    const total = Math.max(loaded.lines.length, loaded.totalLines).toLocaleString();
    return `Loaded ${rendered} of ${total} lines — the display limit is ${cap} lines.`;
  }
  return `Loaded ${rendered} lines — the display limit is ${cap} lines; the total is unknown until loading completes.`;
}

/** The hunks the window shows, each with its visible line prefix. Hunks
 *  entirely past `windowLimit` drop, and the straddling hunk renders only what
 *  fits: one flat line budget over hunk order, like the /web pane. */
export function windowedReviewHunks(
  loaded: ReviewLoadedDiff,
  windowLimit: number,
): Array<{ hunk: CodeReviewFileDiffHunkData; lines: DiffLine[] }> {
  const limit = Math.max(0, Math.min(Math.floor(windowLimit), REVIEW_DIFF_MAX_UI_LINES));
  const rows: Array<{ hunk: CodeReviewFileDiffHunkData; lines: DiffLine[] }> = [];
  let start = 0;
  for (const hunk of loaded.hunks) {
    const end = start + hunk.lines.length;
    const visible = Math.max(0, Math.min(end, limit) - start);
    if (visible > 0) rows.push({ hunk, lines: hunk.lines.slice(0, visible) });
    start = end;
  }
  return rows;
}

/** Idle snapshot interval; a live Auto Review / Highlight / TLDR run uses the
 *  faster cadence (the app owns no batch trigger itself, the wire state
 *  decides). */
export const CODE_REVIEW_SNAPSHOT_POLL_MS = 1500;
export const CODE_REVIEW_BATCH_POLL_MS = 200;

export function codeReviewSnapshotPollMs(
  snapshot: Pick<ReviewSnapshot, 'autoReviewState' | 'highlightState' | 'tldrState'> | null | undefined,
): number {
  const batchLive = snapshot?.autoReviewState === 'triaging'
    || snapshot?.autoReviewState === 'running'
    || snapshot?.highlightState === 'running'
    || snapshot?.tldrState === 'running';
  return batchLive ? CODE_REVIEW_BATCH_POLL_MS : CODE_REVIEW_SNAPSHOT_POLL_MS;
}

/** Grouped `provider/id` option value of the review model; '' when unknown. */
export function reviewModelOptionValue(
  snapshot: Pick<ReviewSnapshot, 'reviewModelProvider' | 'reviewModelId'> | null | undefined,
): string {
  const provider = snapshot?.reviewModelProvider;
  const id = snapshot?.reviewModelId;
  return provider && id ? `${provider}/${id}` : '';
}
