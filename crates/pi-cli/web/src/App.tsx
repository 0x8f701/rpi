import { useCallback, useEffect, useRef, useState } from 'react';
import { safeJson, safeText } from './redact';
import { renderBlocks, hydrateMermaid } from './markdown';
import { TodoPanel } from './panels/TodoPanel';
import type { TodoPhase as TodoPhaseWire } from './panels/TodoPanel';
import { SubagentsPanel } from './panels/SubagentsPanel';
import { GoalPanel } from './panels/GoalPanel';
import type { GoalEventWire, GoalStateWire } from './panels/GoalPanel';
import { WorkflowPanel, dispatchWorkflowEvents } from './panels/WorkflowPanel';
import { SideChatPanel } from './panels/SideChatPanel';
import type { SideChatSnapshot } from './panels/SideChatPanel';
import { MaintenancePanel } from './panels/MaintenancePanel';
import { SessionPanel } from './panels/SessionPanel';
import { SettingsPanel } from './panels/SettingsPanel';
import { SessionSidebar } from './panels/SessionSidebar';
import type { ContentBlock, EventFrame, RpcResponse } from './types';
import {
  type Item,
  nextId,
  contentText,
  messagesToItems,
  boundOutput,
  customToItem,
  applyToolSnapshot,
  shouldRestoreStreamingAssistant,
  BASH_OUTPUT_LINE_LIMIT,
  TOOL_OUTPUT_LINE_LIMIT,
} from './transcript';

const RPI_AUTH_PREFIX = 'rpi-auth.';
const TOKEN_STORAGE_KEY = 'rpi-web-token';
const RECENT_HOSTS_STORAGE_KEY = 'rpi-web-recent-hosts';
const RECENT_HOSTS_MAX = 10;
const RECONNECT_INITIAL_DELAY = 1000;
const RECONNECT_MAX_DELAY = 15000;
const COMMAND_TIMEOUT_MS = 30000;
// Heartbeat: a `{type:"ping"}` JSON frame every 30s keeps the connection
// honest. The backend replies to every text frame (unknown types get an error
// `response` frame, which the RPC plumbing ignores for lack of a pending id),
// so a probe reliably produces an inbound message. If no message of ANY kind
// arrives for 60s the socket is presumed dead — silent drops never fire
// onclose — and is proactively closed so the existing reconnect path takes
// over. The reconnect backoff resets only after the connection has stayed up
// for >5s, so flapping connections keep backing off.
const HEARTBEAT_PING_INTERVAL_MS = 30000;
const HEARTBEAT_TIMEOUT_MS = 60000;
const HEARTBEAT_STABILITY_MS = 5000;

type ConnState = 'off' | 'connecting' | 'on' | 'reconnecting';

// The Item shape, nextId/contentText helpers, and messagesToItems live in
// ./transcript (shared with CollabGuestView) and are re-exported here so
// existing `import { … } from './App'` callers keep compiling.
export { nextId, contentText, messagesToItems, type Item } from './transcript';

interface Pending {
  resolve: (value: unknown) => void;
  reject: (reason: Error) => void;
  bubbleId?: string;
  timer: number;
}

export interface LiveNodes {
  textEl: HTMLDivElement | null;
  thinkingDetails: HTMLDetailsElement | null;
  thinkingBody: HTMLDivElement | null;
  toolcallEl: HTMLDivElement | null;
}

export interface StreamBuffer {
  text: string;
  thinking: string;
  toolcall: string;
}

// Module-level registries for the hot streaming path: deltas are appended to
// the mounted DOM node directly (no React re-render per chunk), with the
// buffer as the catch-up for deltas that arrive before the node mounts.
// Both are keyed by `streamKey(sessionId, itemId)`: equal item ids in
// different sessions can never cross-wire, and a session cutover mid-stream
// cannot route one session's deltas into another session's node.
export const liveNodes = new Map<string, LiveNodes>();
export const streamBuf = new Map<string, StreamBuffer>();

export function streamKey(sid: string | null, itemId: string): string {
  return `${sid ?? ''}\u0000${itemId}`;
}

/** Append one streaming delta to the mounted live node for `(sid, itemId)`
 *  via direct DOM mutation (textContent), bypassing React per-chunk renders.
 *  Shared by the session transcript and the collab guest view — which never
 *  coexist — so the module-level `liveNodes` registry is never cross-wired. */
export function applyDeltaToNode(
  sid: string | null,
  itemId: string,
  delta: string,
  kind: 'text' | 'thinking' | 'toolcall',
): void {
  const node = liveNodes.get(streamKey(sid, itemId));
  if (!node) return;
  if (kind === 'text' && node.textEl) {
    node.textEl.textContent += safeText(delta);
  } else if (kind === 'thinking' && node.thinkingBody && node.thinkingDetails) {
    node.thinkingDetails.hidden = false;
    node.thinkingBody.textContent += safeText(delta);
  } else if (kind === 'toolcall' && node.toolcallEl) {
    node.toolcallEl.textContent += safeText(delta);
  }
}

/** Event types that bump a BACKGROUND session's unread badge. High-frequency
 *  deltas (message_update / tool_execution_update) stay silent so a slow
 *  background stream counts once, not per chunk. */
const UNREAD_EVENT_TYPES: Record<string, true> = {
  turn_start: true,
  message_start: true,
  message_end: true,
  agent_settled: true,
  run_failed: true,
  tool_execution_start: true,
  tool_execution_end: true,
  todo_updated: true,
  todo_reminder: true,
  workflow_updated: true,
  workflow_status_changed: true,
  workflow_removed: true,
  goal_updated: true,
  goal_usage_charged: true,
  job_updated: true,
  agent_updated: true,
  message_delivered: true,
  extension_ui_request: true,
};

// nextId, contentText, and messagesToItems (plus the output-bounding and
// custom-visibility helpers used by the live handlers below) live in
// ./transcript so live events and restored entries share one normalization.

/* ------------------------------------------------------------------ *
 * Small presentational components
 * ------------------------------------------------------------------ */

export function StreamingAssistant({ sid, id }: { sid: string; id: string }) {
  const textRef = useRef<HTMLDivElement>(null);
  const thinkingRef = useRef<HTMLDetailsElement>(null);
  const thinkingBodyRef = useRef<HTMLDivElement>(null);
  const toolcallRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const key = streamKey(sid, id);
    const entry: LiveNodes = {
      textEl: textRef.current,
      thinkingDetails: thinkingRef.current,
      thinkingBody: thinkingBodyRef.current,
      toolcallEl: toolcallRef.current,
    };
    liveNodes.set(key, entry);
    const buf = streamBuf.get(key);
    if (buf) {
      if (buf.text && textRef.current) textRef.current.textContent = buf.text;
      if (buf.thinking && thinkingBodyRef.current && thinkingRef.current) {
        thinkingRef.current.hidden = false;
        thinkingBodyRef.current.textContent = buf.thinking;
      }
      if (buf.toolcall && toolcallRef.current) toolcallRef.current.textContent = buf.toolcall;
    }
    return () => {
      liveNodes.delete(key);
    };
  }, [sid, id]);

  return (
    <div className="msg msg--assistant">
      <details className="thinking" hidden ref={thinkingRef}>
        <summary className="thinking__summary">thinking</summary>
        <div className="thinking__body" ref={thinkingBodyRef} />
      </details>
      <div className="assistant-text" ref={textRef} />
      <div className="assistant-toolcall" ref={toolcallRef} />
    </div>
  );
}

export function FinalAssistant({ blocks }: { blocks: ContentBlock[] }) {
  const textRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    // Mermaid fences render asynchronously: hydrate the hosts after
    // dangerouslySetInnerHTML commits. Scoped to the final text node so the
    // streaming path (textContent deltas) is never touched.
    if (textRef.current) void hydrateMermaid(textRef.current);
  }, []);
  return (
    <div className="msg msg--assistant">
      <div className="assistant-text" ref={textRef} dangerouslySetInnerHTML={{ __html: renderBlocks(blocks) }} />
    </div>
  );
}

export function ToolCard({ item }: { item: Extract<Item, { kind: 'toolCard' }> }) {
  const stateLabel = item.status === 'running' ? 'running…' : item.status;
  return (
    <div className={`tool-card${item.status === 'error' ? ' tool-card--error' : ''}`} data-tool-id={item.toolCallId}>
      <div className="tool-card__head">
        <span className="tool-card__name">{item.toolName}</span>
        <span className={`tool-card__state tool-card__state--${item.status}`}>{stateLabel}</span>
      </div>
      <pre className="tool-card__args" dangerouslySetInnerHTML={{ __html: safeJson(item.args) }} />
      {item.result !== '' && <div className="tool-card__result">{item.result}</div>}
    </div>
  );
}

/** Bash / tool-output card: command in a header bar (green `$` prompt + copy
 *  button), output in a scrollable mono body — the web mirror of the TUI tool
 *  card (crates/pi-cli/src/tool_card_adapter.rs). Shared by the session
 *  transcript and the collab guest view. `command` omitted renders a bare
 *  output card (unmatched toolResult) with a muted label. */
export function BashCard({ command, label, output, status }: { command?: string; label?: string; output: string; status?: string }) {
  const [copied, setCopied] = useState(false);
  const copyOutput = () => {
    const flash = () => {
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1200);
    };
    if (navigator.clipboard && typeof navigator.clipboard.writeText === 'function') {
      navigator.clipboard.writeText(output).then(flash, flash);
    } else {
      const textarea = document.createElement('textarea');
      textarea.value = output;
      textarea.style.position = 'fixed';
      textarea.style.opacity = '0';
      document.body.appendChild(textarea);
      textarea.select();
      try {
        document.execCommand('copy');
      } catch {
        /* clipboard unavailable — nothing else to do */
      }
      document.body.removeChild(textarea);
      flash();
    }
  };
  const head = command !== undefined ? (
    <span className="bash-cmd">
      <span className="bash-prompt">$</span>
      {command}
    </span>
  ) : (
    <span className="bash-cmd bash-cmd--label">{label ?? 'output'}</span>
  );
  return (
    <div className="msg msg--bash">
      <div className="bash-head">
        {head}
        {status && <span className={`tool-card__state tool-card__state--${status}`}>{status === 'running' ? 'running…' : status}</span>}
        <button type="button" className="bash-copy" onClick={copyOutput}>
          {copied ? 'copied' : 'copy'}
        </button>
      </div>
      <pre className="bash-output">{output}</pre>
    </div>
  );
}

export function ToastList({ toasts, dismiss }: { toasts: Array<{ id: string; message: string; error: boolean }>; dismiss: (id: string) => void }) {
  return (
    <div id="toasts">
      {toasts.map((t) => (
        <div key={t.id} className={`toast${t.error ? ' toast--error' : ''}`} onClick={() => dismiss(t.id)}>
          {t.message}
        </div>
      ))}
    </div>
  );
}

/* ------------------------------------------------------------------ *
 * Composer attachments + voice (file upload / hold-to-talk)
 * ------------------------------------------------------------------ */

/** A file attached in the composer. Images ride the prompt command's `images`
 *  ContentBlock array; PDFs become a text note prepended to the message.
 *  `dataBase64` is the raw base64 WITHOUT the `data:...;base64,` prefix. */
interface ComposerAttachment {
  id: string;
  name: string;
  size: number;
  mimeType: string;
  kind: 'image' | 'pdf';
  dataBase64: string;
  /** Full data URL for the thumbnail chip (images only). */
  previewUrl?: string;
}

/** Image ContentBlock wire shape — mirrors pi_ai::ContentBlock::Image, which
 *  is internally tagged with camelCase keys: `{"type":"image","data":…,
 *  "mimeType":…}` (the `source`-nested Anthropic shape is NOT what the RPC
 *  parses). */
interface ImageContentBlock {
  type: 'image';
  data: string;
  mimeType: string;
}

/** Live/STT settings, read defensively from get_state (`runtimeSettings.live`)
 *  when the backend exposes them. `mode` switches the mic between the
 *  hold-to-talk STT flow and the Codex Live realtime (WebRTC) flow; the
 *  secret keys (sttApiKey/realtimeApiKey) are redacted server-side, so the
 *  STT path probes `settings_inspect` at record time when base URL is
 *  missing. */
interface LiveSettingsWire {
  enabled?: boolean;
  mode?: string;
  sttBaseUrl?: string;
  sttApiKey?: string;
  sttModel?: string;
  realtimeBaseUrl?: string;
  realtimeApiKey?: string;
  realtimeModel?: string;
  voice?: string;
}

/** ASCII header chunks for the WAV RIFF container. */
const ASCII_ENC = new TextEncoder();

/** 16-bit PCM mono WAV from a decoded AudioBuffer (the same container the
 *  backend's SttClient sends to /v1/audio/transcriptions). */
function encodeWavPcm16(buffer: AudioBuffer): ArrayBuffer {
  const numChannels = 1;
  const sampleRate = Math.max(1, Math.round(buffer.sampleRate));
  const channel = buffer.getChannelData(0);
  const bytesPerSample = 2;
  const blockAlign = numChannels * bytesPerSample;
  const dataSize = channel.length * blockAlign;
  const out = new ArrayBuffer(44 + dataSize);
  const view = new DataView(out);
  const bytes = new Uint8Array(out);
  bytes.set(ASCII_ENC.encode('RIFF'), 0);
  view.setUint32(4, 36 + dataSize, true);
  bytes.set(ASCII_ENC.encode('WAVE'), 8);
  bytes.set(ASCII_ENC.encode('fmt '), 12);
  view.setUint32(16, 16, true);
  view.setUint16(20, 1, true); // PCM
  view.setUint16(22, numChannels, true);
  view.setUint32(24, sampleRate, true);
  view.setUint32(28, sampleRate * blockAlign, true);
  view.setUint16(32, blockAlign, true);
  view.setUint16(34, 16, true);
  bytes.set(ASCII_ENC.encode('data'), 36);
  view.setUint32(40, dataSize, true);
  let offset = 44;
  for (let i = 0; i < channel.length; i++) {
    const s = Math.max(-1, Math.min(1, channel[i]));
    view.setInt16(offset, s < 0 ? s * 0x8000 : s * 0x7fff, true);
    offset += 2;
  }
  return out;
}

/** Convert a MediaRecorder blob to WAV; returns null when the container is
 *  undecodable so the caller falls back to the raw recording. */
async function blobToWav(blob: Blob): Promise<Blob | null> {
  try {
    // webkitAudioContext: Safari's prefixed constructor (same shape).
    const webkitWindow = window as unknown as { webkitAudioContext?: typeof AudioContext };
    const AudioCtor = window.AudioContext ?? webkitWindow.webkitAudioContext;
    if (!AudioCtor) return null;
    const ctx = new AudioCtor();
    try {
      const audioBuffer = await ctx.decodeAudioData(await blob.arrayBuffer());
      return new Blob([encodeWavPcm16(audioBuffer)], { type: 'audio/wav' });
    } finally {
      void ctx.close();
    }
  } catch {
    return null;
  }
}

/** First non-empty string among `values` (empty strings and non-strings are
 *  skipped) — used to read defensive field variants off realtime sideband
 *  events without guessing the exact key the backend chose. */
function firstString(...values: unknown[]): string {
  for (const value of values) {
    if (typeof value === 'string' && value.trim()) return value.trim();
  }
  return '';
}

/** Sideband WebSocket URL for a realtime call: CLIProxyAPI serves
 *  `/v1/realtime?call_id=...` under the configured base. An http(s) base is
 *  converted to ws(s), a scheme-less host is assumed http, and a trailing
 *  `/v1` is de-duplicated (Hyper parity, mirroring `transcriptions_url`). */
function sidebandRealtimeWsUrl(baseUrl: string, callId: string): string {
  const trimmed = baseUrl.trim().replace(/\/+$/, '');
  const withScheme = /^https?:\/\//i.test(trimmed) ? trimmed : `http://${trimmed}`;
  const origin = withScheme.replace(/^https?:\/\//i, '').replace(/\/v1$/i, '');
  const scheme = /^https:/i.test(withScheme) ? 'wss' : 'ws';
  return `${scheme}://${origin}/v1/realtime?call_id=${encodeURIComponent(callId)}`;
}

/* ------------------------------------------------------------------ *
 * App
 * ------------------------------------------------------------------ */

export function App() {
  const [connState, setConnState] = useState<ConnState>('off');
  // Multi-session (Main contract): per-session item caches plus per-session
  // streaming/unread state. The ACTIVE session's view renders from
  // itemsBySessionId[activeSessionId]; previously-visited sessions restore
  // their cached items on switch-back (no server replay), unseen sessions
  // start empty. Events without a sessionId route to the ACTIVE session
  // (current single-Application behavior); the backend sessionId contract
  // (MultiSessionRuntimeManager) routes background sessions when it lands.
  const [itemsBySessionId, setItemsBySessionId] = useState<Record<string, Item[]>>({});
  const [streamingBySessionId, setStreamingBySessionId] = useState<Record<string, boolean>>({});
  const [unreadBySessionId, setUnreadBySessionId] = useState<Record<string, number>>({});
  // Per-session shell/feature state (Main contract: model, thinking, session
  // name, Todo, Goal, Side chat are session-scoped and must never leak across
  // sessions). The ACTIVE session's slots drive the header + panels; events
  // for background sessions update only the owning slot's cache/unread.
  const [sessionNameBySessionId, setSessionNameBySessionId] = useState<Record<string, string>>({});
  const [modelKeyBySessionId, setModelKeyBySessionId] = useState<Record<string, string>>({});
  const [thinkingLevelBySessionId, setThinkingLevelBySessionId] = useState<Record<string, string>>({});
  const [models, setModels] = useState<Array<{ id: string; name: string; provider: string }>>([]);
  const [levels, setLevels] = useState<string[]>([]);
  const [token, setToken] = useState('');
  // Host shown in the header input (controlled): defaults to the host that
  // served the page; commitHost() keeps it in sync with hostRef.
  const [hostInput, setHostInput] = useState(() => (typeof window !== 'undefined' ? window.location.host : ''));
  // Recently connected hosts (most recent first, capped) — the header input's
  // datalist suggestions. Persisted under `rpi-web-recent-hosts`.
  const [recentHosts, setRecentHosts] = useState<string[]>(() => {
    try {
      const raw = window.localStorage.getItem(RECENT_HOSTS_STORAGE_KEY);
      if (raw) {
        const parsed = JSON.parse(raw);
        if (Array.isArray(parsed)) {
          return parsed.filter((h) => typeof h === 'string' && h !== '').slice(0, RECENT_HOSTS_MAX);
        }
      }
    } catch {
      /* private mode or corrupt storage */
    }
    return [];
  });
  const [toasts, setToasts] = useState<Array<{ id: string; message: string; error: boolean }>>([]);
  // D89: Todo DAG panel. `activePanel` is the single shared panel-name state;
  // each web panel registers its own name + a mount-point render line below.
  const [activePanel, setActivePanel] = useState<string>('');
  // D92: persistent session sidebar (left rail) + mobile drawer toggle.
  const [sessionId, setSessionId] = useState<string | null>(null);
  // Mirrors `sessionId` so callbacks can read the active session synchronously
  // (the React state lags one render). Same-id refreshes (reconnect, settings
  // apply, rename) keep the active session; a different non-empty id is a
  // session cutover that switches the ACTIVE view (per-session caches below).
  const sessionIdRef = useRef<string | null>(null);
  // One visibility state drives BOTH the mobile drawer and the desktop
  // collapsible rail: open by default on desktop, closed on mobile.
  const [sidebarOpen, setSidebarOpen] = useState(() =>
    typeof window !== 'undefined' && window.matchMedia('(min-width: 721px)').matches
  );
  // Active session's derived view (empty until that session has items).
  const [todoPhasesBySessionId, setTodoPhasesBySessionId] = useState<Record<string, TodoPhaseWire[]>>({});
  // D90: Goal panel — current goal snapshot + journal replay (live via
  // goal_updated / goal_usage_charged events, refreshed via goal_get /
  // goal_journal). Keyed by session: every refresh/event carries the owning
  // sessionId, so a background session's goal can never mutate the active
  // panel and an async refreshGoal() can never apply to the wrong session.
  const [goalStateBySessionId, setGoalStateBySessionId] = useState<Record<string, GoalStateWire | null>>({});
  const [goalJournalBySessionId, setGoalJournalBySessionId] = useState<Record<string, GoalEventWire[]>>({});
  // D94: side chat (parallel /btw sessions) snapshot; polled while the
  // 'sidechat' panel is open, keyed by owning session.
  const [sideChatBySessionId, setSideChatBySessionId] = useState<Record<string, SideChatSnapshot | null>>({});

  // Active-session derived view (empty until that session has items).
  const activeItems = sessionId ? (itemsBySessionId[sessionId] ?? []) : [];
  const activeStreaming = sessionId ? !!streamingBySessionId[sessionId] : false;
  const activeSessionName = sessionId ? (sessionNameBySessionId[sessionId] ?? '') : '';
  const activeModelKey = sessionId ? (modelKeyBySessionId[sessionId] ?? '') : '';
  const activeThinkingLevel = sessionId ? (thinkingLevelBySessionId[sessionId] ?? '') : '';
  const activeTodoPhases = sessionId ? (todoPhasesBySessionId[sessionId] ?? []) : [];
  const activeGoalState = sessionId ? (goalStateBySessionId[sessionId] ?? null) : null;
  const activeGoalJournal = sessionId ? (goalJournalBySessionId[sessionId] ?? []) : [];
  const activeSideChat = sessionId ? (sideChatBySessionId[sessionId] ?? null) : null;

  // Composer file attachments (images -> ContentBlocks, PDFs -> text note).
  const [attachments, setAttachments] = useState<ComposerAttachment[]>([]);
  // Hold-to-talk voice: recording flag drives the pulsing mic indicator;
  // transcribing blocks a second capture while the STT round-trip runs.
  const [recording, setRecording] = useState(false);
  const [transcribing, setTranscribing] = useState(false);
  // Live/STT settings surfaced by get_state (`runtimeSettings.live`) when the
  // backend exposes them; null means "not advertised" (mic still shown).
  const [liveSettings, setLiveSettings] = useState<LiveSettingsWire | null>(null);

  const wsRef = useRef<WebSocket | null>(null);
  // The target rpi instance (host:port). Defaults to the host that served the
  // page; commitHost() updates it (and the header input) on change. The
  // token comes from the Settings panel (rpi-web-token in localStorage).
  // Both are refs so connect() reads them synchronously (React state lags one
  // render).
  const hostRef = useRef(typeof window !== 'undefined' ? window.location.host : '');
  const tokenRef = useRef(token);
  const pendingRef = useRef(new Map<string, Pending>());
  const seqRef = useRef(0);
  const delayRef = useRef(RECONNECT_INITIAL_DELAY);
  const retryTimerRef = useRef<number | null>(null);
  // Heartbeat bookkeeping: lastMessageAtRef is touched on EVERY inbound frame
  // (event, response, or the ping probe's error-response); the silence timer
  // fires exactly HEARTBEAT_TIMEOUT_MS after the last message and closes the
  // socket when nothing has arrived. pingTimerRef drives the 30s probe. All
  // three live only while a socket is open (armed in onOpen, cleared on
  // close/replace/unload), plus the stability timer that resets the backoff
  // once a fresh connection has stayed alive >5s.
  const lastMessageAtRef = useRef(0);
  const pingTimerRef = useRef<number | null>(null);
  const silenceTimerRef = useRef<number | null>(null);
  const stabilityTimerRef = useRef<number | null>(null);
  // Per-session streaming assistant id + optimistic bubble queue: switching
  // sessions swaps the bucket, so each session's in-flight state is isolated.
  const activeAssistantBySessionIdRef = useRef<Record<string, string>>({});
  // Per-session abort-pending flag so a run_failed for one session's abort
  // never mislabels another session's failure.
  const abortPendingBySessionIdRef = useRef<Record<string, boolean>>({});
  const bootProbeRef = useRef(true);
  const optimisticQueueBySessionIdRef = useRef<Record<string, string[]>>({});
  const transcriptRef = useRef<HTMLDivElement>(null);
  const nearBottomRef = useRef(true);
  const promptInputRef = useRef<HTMLTextAreaElement>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const mediaStreamRef = useRef<MediaStream | null>(null);
  const mediaRecorderRef = useRef<MediaRecorder | null>(null);
  const mediaChunksRef = useRef<Blob[]>([]);
  const recordingTimerRef = useRef<number | null>(null);
  // Codex Live realtime voice (WebRTC): active while the call is up; the
  // sideband WS delivers transcript/delegation events for the overlay. The
  // transcript text is accumulated in a ref and written straight into the
  // overlay node (no React re-render per delta — same hot-path decision as
  // the streaming transcript); realtimeDelegation is a rare state change.
  const [realtimeActive, setRealtimeActive] = useState(false);
  const [realtimeDelegation, setRealtimeDelegation] = useState<string | null>(null);
  const realtimePcRef = useRef<RTCPeerConnection | null>(null);
  const realtimeWsRef = useRef<WebSocket | null>(null);
  // Synchronous start-guard: getUserMedia/offer/answer is async, so a quick
  // double-click could race two call setups before realtimeActive re-renders.
  const realtimeBusyRef = useRef(false);
  const realtimeAudioRef = useRef<HTMLAudioElement | null>(null);
  const realtimeTranscriptRef = useRef('');
  const realtimeTranscriptNodeRef = useRef<HTMLDivElement | null>(null);
  // Suppresses RPC error toasts during the reconnection bootstrap sequence
  // (get_state rebind) so a phone waking from sleep doesn't spam "command
  // get_state failed" before the rebind-to-primary lands.
  const bootRef = useRef(false);
  // D93: orchestration event handlers registered by the Subagents panel
  // (job_updated / agent_updated / message_delivered). A Set keeps the panel
  // subscription additive alongside other live panels.
  const subagentsHandlersRef = useRef(new Set<(frame: EventFrame) => void>());

  const subscribeSubagentsEvents = useCallback((handler: (frame: EventFrame) => void) => {
    subagentsHandlersRef.current.add(handler);
    return () => {
      subagentsHandlersRef.current.delete(handler);
    };
  }, []);

  const toast = useCallback((message: string, error = false) => {
    const id = nextId('t');
    setToasts((prev) => [...prev.slice(-4), { id, message: safeText(message), error }]);
    window.setTimeout(() => {
      setToasts((prev) => prev.filter((t) => t.id !== id));
    }, 7000);
  }, []);

  const appendToLiveNode = useCallback((sid: string | null, itemId: string, delta: string, kind: 'text' | 'thinking' | 'toolcall') => {
    applyDeltaToNode(sid, itemId, delta, kind);
    // Auto-scroll only when the streaming session IS the active view: a
    // background session's deltas must never move the active transcript.
    if (sid === sessionIdRef.current && nearBottomRef.current && transcriptRef.current) {
      transcriptRef.current.scrollTop = transcriptRef.current.scrollHeight;
    }
  }, []);

  /** Mutate ONE session's item cache (null sid = active session). */
  const updateItemsFor = useCallback((sid: string | null, updater: (prev: Item[]) => Item[]) => {
    if (!sid) return;
    setItemsBySessionId((prev) => ({ ...prev, [sid]: updater(prev[sid] ?? []) }));
  }, []);

  const pushItemFor = useCallback(
    (sid: string | null, item: Item) => updateItemsFor(sid, (prev) => [...prev, item]),
    [updateItemsFor]
  );

  const patchItemFor = useCallback(
    (sid: string | null, id: string, patch: (item: Item) => Item) =>
      updateItemsFor(sid, (prev) => prev.map((item) => (item.id === id ? patch(item) : item))),
    [updateItemsFor]
  );

  const removeItemFor = useCallback(
    (sid: string | null, id: string) => updateItemsFor(sid, (prev) => prev.filter((item) => item.id !== id)),
    [updateItemsFor]
  );

  // Active-session shorthand (the pre-contract event streams target the
  // active session; the backend sessionId contract passes an explicit sid).
  const removeItem = useCallback((id: string) => removeItemFor(sessionIdRef.current, id), [removeItemFor]);

  /** The session an event frame belongs to: explicit sessionId wins, else the
   *  active session (pre-contract behavior). */
  const frameSession = useCallback((frame: EventFrame): string | null => {
    const sid = (frame as { sessionId?: unknown }).sessionId;
    return typeof sid === 'string' && sid !== '' ? sid : sessionIdRef.current;
  }, []);

  /** Bump the unread badge when an event targets a NON-active session. */
  const markBackgroundEvent = useCallback((sid: string | null) => {
    if (!sid || sid === sessionIdRef.current) return;
    setUnreadBySessionId((prev) => ({ ...prev, [sid]: (prev[sid] ?? 0) + 1 }));
  }, []);



  /* ---------------- RPC plumbing ---------------- */

  const sendCommand = useCallback((command: Record<string, unknown>, bubbleId?: string): Promise<unknown> => {
    const ws = wsRef.current;
    if (!ws || ws.readyState !== WebSocket.OPEN) {
      if (bubbleId) removeItem(bubbleId);
      return Promise.reject(new Error('not connected'));
    }
    const { promise, resolve, reject } = Promise.withResolvers<unknown>();
    const id = `c${++seqRef.current}`;
    // MultiSessionRuntimeManager contract: every command carries a top-level
    // sessionId so the listener routes it to the owning runtime. An explicit
    // `command.sessionId` (lifecycle targeting another session) wins over the
    // ACTIVE session; boot/create commands omit it (null -> no field). The
    // manager strips sessionId BEFORE command deserialization (parse_input),
    // so deny_unknown_fields schemas (workflow_*) accept it safely on the
    // multi-session backend — every command, workflow included, routes to the
    // owning runtime instead of defaulting to the primary.
    const explicitSid =
      typeof command.sessionId === 'string' && command.sessionId !== ''
        ? command.sessionId
        : null;
    const sid = explicitSid ?? sessionIdRef.current;
    const frame = sid ? { ...command, id, sessionId: sid } : { ...command, id };
    const timer = window.setTimeout(() => {
      if (pendingRef.current.delete(id)) {
        if (bubbleId) removeItem(bubbleId);
        reject(new Error('command timed out'));
      }
    }, COMMAND_TIMEOUT_MS);
    pendingRef.current.set(id, { resolve, reject, bubbleId, timer });
    ws.send(JSON.stringify(frame));
    return promise;
  }, [removeItem]);

  const onResponse = useCallback((frame: RpcResponse) => {
    const pending = pendingRef.current.get(frame.id || '');
    if (!pending) return;
    pendingRef.current.delete(frame.id || '');
    window.clearTimeout(pending.timer);
    if (frame.success) {
      pending.resolve(frame.data || {});
    } else {
      if (pending.bubbleId) removeItem(pending.bubbleId);
      if (!bootRef.current) toast(`command ${frame.command} failed: ${frame.error || 'unknown error'}`, true);
      const error = new Error(frame.error || 'rpc failed');
      (error as Error & { rpc?: boolean }).rpc = true;
      pending.reject(error);
    }
  }, [removeItem, toast]);

  /** D89: dispatch a flattened pi_coding::TodoOp over the `todo_op` RPC. */
  const runTodoOp = useCallback((op: Record<string, unknown>) => {
    // Capture the target session at SEND time; the response applies to that
    // session's cache only (a concurrent switch cannot mis-route it).
    const sid = sessionIdRef.current;
    sendCommand({ type: 'todo_op', ...op })
      .then((data) => {
        const d = data as { phases?: TodoPhaseWire[] };
        if (Array.isArray(d.phases) && sid) {
          setTodoPhasesBySessionId((prev) => ({ ...prev, [sid]: d.phases as TodoPhaseWire[] }));
        }
      })
      .catch((err: Error & { rpc?: boolean }) => {
        if (!err.rpc) toast(`todo op failed: ${err.message}`, true);
      });
  }, [sendCommand, toast]);

  /* ---------------- D94: side chat + maintenance ---------------- */

  /** Snapshot responses are applied to the sid captured at SEND time, so a
   *  session cutover mid-poll can never mis-route the reply. */
  const refreshSideChat = useCallback((sid: string | null) => {
    sendCommand({ type: 'side_chat_list' })
      .then((data) => {
        if (!sid) return;
        setSideChatBySessionId((prev) => ({ ...prev, [sid]: data as SideChatSnapshot }));
      })
      .catch((err: Error & { rpc?: boolean }) => {
        if (!err.rpc) toast(`side chat: ${err.message}`, true);
      });
  }, [sendCommand, toast]);

  const sideChatNew = useCallback((name: string) => {
    const sid = sessionIdRef.current;
    sendCommand({ type: 'side_chat_new', name })
      .then((data) => {
        if (!sid) return;
        setSideChatBySessionId((prev) => ({ ...prev, [sid]: data as SideChatSnapshot }));
      })
      .catch((err: Error & { rpc?: boolean }) => {
        if (!err.rpc) toast(`side chat new: ${err.message}`, true);
      });
  }, [sendCommand, toast]);

  const sideChatSwitch = useCallback((name: string) => {
    const sid = sessionIdRef.current;
    sendCommand({ type: 'side_chat_switch', name })
      .then((data) => {
        if (!sid) return;
        setSideChatBySessionId((prev) => ({ ...prev, [sid]: data as SideChatSnapshot }));
      })
      .catch((err: Error & { rpc?: boolean }) => {
        if (!err.rpc) toast(`side chat switch: ${err.message}`, true);
      });
  }, [sendCommand, toast]);

  const sideChatClose = useCallback((name?: string) => {
    const sid = sessionIdRef.current;
    const command = name ? { type: 'side_chat_close', name } : { type: 'side_chat_close' };
    sendCommand(command)
      .then((data) => {
        if (!sid) return;
        setSideChatBySessionId((prev) => ({ ...prev, [sid]: data as SideChatSnapshot }));
      })
      .catch((err: Error & { rpc?: boolean }) => {
        if (!err.rpc) toast(`side chat close: ${err.message}`, true);
      });
  }, [sendCommand, toast]);

  const sideChatPrompt = useCallback((message: string) => {
    const sid = sessionIdRef.current;
    sendCommand({ type: 'side_chat_prompt', message })
      .then((data) => {
        if (!sid) return;
        const snapshot = data as SideChatSnapshot;
        setSideChatBySessionId((prev) => ({ ...prev, [sid]: snapshot }));
        if (snapshot.busy) toast('side chat is busy — wait for the current reply', true);
      })
      .catch((err: Error & { rpc?: boolean }) => {
        if (!err.rpc) toast(`side chat send failed: ${err.message}`, true);
      });
  }, [sendCommand, toast]);

  /** Maintenance panel RPC: the panel receives sendCommand directly (its
   *  commands target the active session via the top-level sessionId). */

  // Poll the side-chat snapshot while the panel is open (the side agent runs
  // detached; snapshots drain controller events on the server). The polled
  // session is captured per tick; the panel remounts on session switch, so
  // its responses can never land in another session's cache.
  useEffect(() => {
    if (activePanel !== 'sidechat') return;
    const poll = () => refreshSideChat(sessionIdRef.current);
    poll();
    const timer = window.setInterval(poll, 2000);
    return () => window.clearInterval(timer);
  }, [activePanel, refreshSideChat, sessionId]);

  /** D90: refresh the Goal panel snapshot + journal for ONE session. The
   *  target sid is captured at call time and every response is keyed to it —
   *  an async refreshGoal() can never query one session and paint another. */
  const refreshGoal = useCallback((sid: string | null) => {
    if (!sid) return;
    sendCommand({ type: 'goal_get' })
      .then((data) => {
        setGoalStateBySessionId((prev) => ({ ...prev, [sid]: data as GoalStateWire }));
      })
      .catch(() => {});
    sendCommand({ type: 'goal_journal' })
      .then((data) => {
        if (Array.isArray(data)) {
          setGoalJournalBySessionId((prev) => ({ ...prev, [sid]: data as GoalEventWire[] }));
        }
      })
      .catch(() => {});
  }, [sendCommand]);

  /* ---------------- state application ---------------- */

  /** Apply a session state snapshot to ONE session's slots. The target is the
   *  explicit `targetSid` (lifecycle snapshots, refreshState captures) when
   *  given, else the state's own sessionId, else the active session. Every
   *  field is written to the target session's cache only — model/thinking/
   *  name/Todo/Goal/streaming never leak across sessions. A target that
   *  differs from the currently active session is a session cutover that
   *  switches the ACTIVE view and clears the target's unread badge. */
  const applyState = useCallback((data: unknown, targetSid?: string | null) => {
    const d = (data || {}) as {
      model?: { id?: string; provider?: string } | null;
      thinkingLevel?: string;
      isStreaming?: boolean;
      sessionName?: string | null;
      sessionId?: string | null;
      todoPhases?: TodoPhaseWire[];
      goal?: GoalStateWire;
      runtimeSettings?: { live?: LiveSettingsWire };
    };
    const target =
      targetSid && targetSid !== ''
        ? targetSid
        : typeof d.sessionId === 'string' && d.sessionId !== ''
          ? d.sessionId
          : sessionIdRef.current;
    if (!target) return;
    if (d.sessionName) {
      setSessionNameBySessionId((prev) => ({ ...prev, [target]: d.sessionName as string }));
    }
    if (d.model && d.model.id) {
      setModelKeyBySessionId((prev) => ({ ...prev, [target]: `${d.model?.provider}/${d.model?.id}` }));
    }
    if (d.thinkingLevel) {
      setThinkingLevelBySessionId((prev) => ({ ...prev, [target]: d.thinkingLevel as string }));
    }
    if (typeof d.isStreaming === 'boolean') {
      setStreamingBySessionId((prev) => ({ ...prev, [target]: d.isStreaming as boolean }));
    }
    if (Array.isArray(d.todoPhases)) {
      setTodoPhasesBySessionId((prev) => ({ ...prev, [target]: d.todoPhases as TodoPhaseWire[] }));
    }
    if (d.goal && typeof d.goal === 'object') {
      setGoalStateBySessionId((prev) => ({ ...prev, [target]: d.goal as GoalStateWire }));
    }
    // Live/STT settings are global (not per-session); advertise them to the
    // composer's mic when the backend includes them in get_state.
    const live = d.runtimeSettings && typeof d.runtimeSettings === 'object' ? d.runtimeSettings.live : undefined;
    if (live && typeof live === 'object') {
      setLiveSettings(live);
    }
    // Session cutover (previous non-empty id -> DIFFERENT non-empty id): the
    // active view switches to the new session's cache. Same-id refreshes
    // (reconnect, settings apply, rename) keep the transcript untouched.
    if (sessionIdRef.current !== target) {
      // Activating a session clears its unread badge.
      setUnreadBySessionId((prev) => ({ ...prev, [target]: 0 }));
    }
    sessionIdRef.current = target;
    setSessionId(target);
  }, []);

  // Re-fetch get_state into the app shell; panels call this after mutations
  // (settings apply, rename) so the header stays authoritative. The target
  // sid is captured at SEND time so the response applies to the session that
  // was queried, even if the active session changes while in flight.
  const refreshState = useCallback(() => {
    const sid = sessionIdRef.current;
    return sendCommand({ type: 'get_state' })
      .then((data) => applyState(data, sid))
      .catch(() => {});
  }, [applyState, sendCommand]);

  /** MultiSessionRuntimeManager lifecycle contract: switch_session/new_session
   *  resolve with `{ sessionId, state, messages }` for the TARGET session.
   *  Consume that snapshot ATOMICALLY (apply target state + replace the target
   *  session's transcript from the authoritative backend messages) instead of
   *  re-querying with the source sessionId (which would snap back). In-flight
   *  items of the target (streaming assistant + unconfirmed optimistic
   *  bubbles) are preserved across the replace: the recorder's messages()
   *  excludes the partial turn, and message_end finalizes the preserved item.
   *  Falls back to refreshState when the response carries no snapshot
   *  (pre-contract backend). */
  const onLifecycleResult = useCallback(
    (result: unknown): Promise<unknown> => {
      const d = (result || {}) as { sessionId?: string; state?: unknown; messages?: unknown };
      if (typeof d.sessionId === 'string' && d.sessionId !== '') {
        const target = d.sessionId;
        if (d.state !== undefined && d.state !== null) applyState(d.state, target);
        if (Array.isArray(d.messages)) {
          // Backend Vec<Message> -> renderable Items (never a raw cast).
          setItemsBySessionId((prev) => {
            const history = messagesToItems(d.messages);
            const inflight = (prev[target] ?? []).filter(
              (i) => (i.kind === 'assistant' && i.status === 'streaming') || (i.kind === 'user' && i.optimistic)
            );
            return { ...prev, [target]: inflight.length > 0 ? [...history, ...inflight] : history };
          });
        }
        // Activating a session clears its unread badge.
        setUnreadBySessionId((prev) => ({ ...prev, [target]: 0 }));
        // Authoritative goal journal for the newly active session (the
        // snapshot's state carries the goal but not the journal); the sid is
        // captured, so a later switch cannot mis-route the response.
        refreshGoal(target);
        sessionIdRef.current = target;
        setSessionId(target);
        return Promise.resolve();
      }
      return refreshState();
    },
    [applyState, refreshState, refreshGoal]
  );

  /** close_session: drop a session's client cache/unread/streaming state after
   *  the runtime is closed (non-destructive; busy closes surface refusal). */
  const removeSessionState = useCallback((sid: string) => {
    setItemsBySessionId((prev) => {
      if (!(sid in prev)) return prev;
      const next = { ...prev };
      delete next[sid];
      return next;
    });
    setUnreadBySessionId((prev) => {
      if (!(sid in prev)) return prev;
      const next = { ...prev };
      delete next[sid];
      return next;
    });
    setStreamingBySessionId((prev) => {
      if (!(sid in prev)) return prev;
      const next = { ...prev };
      delete next[sid];
      return next;
    });
    setTodoPhasesBySessionId((prev) => {
      if (!(sid in prev)) return prev;
      const next = { ...prev };
      delete next[sid];
      return next;
    });
    setGoalStateBySessionId((prev) => {
      if (!(sid in prev)) return prev;
      const next = { ...prev };
      delete next[sid];
      return next;
    });
    setGoalJournalBySessionId((prev) => {
      if (!(sid in prev)) return prev;
      const next = { ...prev };
      delete next[sid];
      return next;
    });
    setSideChatBySessionId((prev) => {
      if (!(sid in prev)) return prev;
      const next = { ...prev };
      delete next[sid];
      return next;
    });
    setSessionNameBySessionId((prev) => {
      if (!(sid in prev)) return prev;
      const next = { ...prev };
      delete next[sid];
      return next;
    });
    setModelKeyBySessionId((prev) => {
      if (!(sid in prev)) return prev;
      const next = { ...prev };
      delete next[sid];
      return next;
    });
    setThinkingLevelBySessionId((prev) => {
      if (!(sid in prev)) return prev;
      const next = { ...prev };
      delete next[sid];
      return next;
    });
    delete optimisticQueueBySessionIdRef.current[sid];
    delete activeAssistantBySessionIdRef.current[sid];
    delete abortPendingBySessionIdRef.current[sid];
    // Drop the closed session's streaming buffers (namespace prefix) so its
    // text can never re-appear under a later session.
    const prefix = `${sid}\u0000`;
    for (const key of streamBuf.keys()) {
      if (key.startsWith(prefix)) streamBuf.delete(key);
    }
  }, []);

  const onCloseSession = useCallback(
    (sid: string) => {
      // Explicit target: close_session routes to row.sessionId's runtime.
      sendCommand({ type: 'close_session', sessionId: sid })
        .then(() => removeSessionState(sid))
        .catch((err: Error & { rpc?: boolean }) => {
          // Non-destructive: a busy close is refused server-side WITHOUT
          // cancelling work. RPC failures are already surfaced by onResponse
          // ("command close_session failed: session is busy: ...") — that
          // toast IS the busy-refusal surfacing; only non-RPC transport
          // failures need a message here.
          if (!err.rpc) toast(`close session failed: ${err.message}`, true);
        });
    },
    [removeSessionState, sendCommand, toast]
  );

  /** Full app reset for a host switch: reject in-flight commands, drop every
   *  session's cached items/streaming/unread/shell state, close panels, and
   *  clear the module streaming registries so the app renders as freshly
   *  loaded against the new host. */
  const resetAllState = useCallback(() => {
    // Reject in-flight commands as transport errors (rpc=true so the generic
    // catch handlers stay quiet) — their responses must never repaint the
    // new host's session state.
    for (const [, pending] of pendingRef.current) {
      window.clearTimeout(pending.timer);
      const error = new Error('host switched');
      (error as Error & { rpc?: boolean }).rpc = true;
      pending.reject(error);
    }
    pendingRef.current.clear();
    streamBuf.clear();
    liveNodes.clear();
    optimisticQueueBySessionIdRef.current = {};
    activeAssistantBySessionIdRef.current = {};
    abortPendingBySessionIdRef.current = {};
    sessionIdRef.current = null;
    setSessionId(null);
    setItemsBySessionId({});
    setStreamingBySessionId({});
    setUnreadBySessionId({});
    setSessionNameBySessionId({});
    setModelKeyBySessionId({});
    setThinkingLevelBySessionId({});
    setTodoPhasesBySessionId({});
    setGoalStateBySessionId({});
    setGoalJournalBySessionId({});
    setSideChatBySessionId({});
    setModels([]);
    setLevels([]);
    setToasts([]);
    setActivePanel('');
  }, []);

  /* ---------------- connection ---------------- */

  const scheduleReconnect = useCallback(() => {
    if (retryTimerRef.current !== null) window.clearTimeout(retryTimerRef.current);
    const delay = delayRef.current;
    delayRef.current = Math.min(delayRef.current * 2, RECONNECT_MAX_DELAY);
    setConnState('reconnecting');
    retryTimerRef.current = window.setTimeout(() => {
      retryTimerRef.current = null;
      connect();
    }, delay);
  }, []);

  /** Restart the silence window from the last received message. */
  const rescheduleSilenceTimer = useCallback(() => {
    if (silenceTimerRef.current !== null) window.clearTimeout(silenceTimerRef.current);
    const remaining = Math.max(HEARTBEAT_TIMEOUT_MS - (Date.now() - lastMessageAtRef.current), 1);
    silenceTimerRef.current = window.setTimeout(() => {
      silenceTimerRef.current = null;
      const ws = wsRef.current;
      if (!ws || ws.readyState !== WebSocket.OPEN) return;
      if (Date.now() - lastMessageAtRef.current >= HEARTBEAT_TIMEOUT_MS) {
        // No message of any kind for the whole window: the socket is silently
        // dead (no onclose fires). Close it so the existing onclose path
        // schedules the reconnect with the current backoff.
        ws.close(4000, 'heartbeat timeout');
        return;
      }
      // A message arrived after this timer was scheduled; slide the window.
      rescheduleSilenceTimer();
    }, remaining);
  }, []);

  /** Stop every heartbeat/stability timer (socket closed or being replaced). */
  const clearHeartbeatTimers = useCallback(() => {
    if (pingTimerRef.current !== null) window.clearInterval(pingTimerRef.current);
    pingTimerRef.current = null;
    if (silenceTimerRef.current !== null) window.clearTimeout(silenceTimerRef.current);
    silenceTimerRef.current = null;
    if (stabilityTimerRef.current !== null) window.clearTimeout(stabilityTimerRef.current);
    stabilityTimerRef.current = null;
  }, []);

  const onOpen = useCallback(() => {
    // Heartbeat: arm the 30s ping probe and the 60s silence window from the
    // open moment; every inbound frame slides the window (see connect).
    bootRef.current = true;
    lastMessageAtRef.current = Date.now();
    rescheduleSilenceTimer();
    if (pingTimerRef.current === null) {
      pingTimerRef.current = window.setInterval(() => {
        const ws = wsRef.current;
        if (!ws || ws.readyState !== WebSocket.OPEN) return;
        try {
          ws.send(JSON.stringify({ type: 'ping' }));
        } catch {
          /* socket died between the check and the send; onclose handles it */
        }
      }, HEARTBEAT_PING_INTERVAL_MS);
    }
    // The reconnect backoff resets ONLY after the connection has proven
    // stable for >5s; a connection that dies inside the window keeps its
    // accumulated delay so flapping connections keep backing off.
    stabilityTimerRef.current = window.setTimeout(() => {
      stabilityTimerRef.current = null;
      const ws = wsRef.current;
      if (ws && ws.readyState === WebSocket.OPEN) {
        delayRef.current = RECONNECT_INITIAL_DELAY;
      }
    }, HEARTBEAT_STABILITY_MS);
    // Bootstrap the authoritative active session BEFORE exposing the
    // connected UI. The first get_state probes with the current (possibly
    // stale) active session id; on a server restart the stale id is rejected
    // ("unknown session"), so the fallback re-probes with NO sessionId to
    // route to the authoritative primary. A second state snapshot after
    // get_messages closes the settlement race: settled sessions replace the
    // transcript outright, while a still-running session preserves or
    // recreates its streaming assistant so later deltas/message_end remain
    // routable.
    const rebindToPrimary = () => {
      sessionIdRef.current = null;
      return sendCommand({ type: 'get_state' });
    };
    sendCommand({ type: 'get_state' })
      .catch(rebindToPrimary)
      .then((state) => {
        applyState(state);
        const target = sessionIdRef.current;
        if (!target) throw new Error('state response did not bind a session');
        return sendCommand({ type: 'get_messages', sessionId: target }).then((data) => {
          if (!data || typeof data !== 'object' || !('messages' in data) || !Array.isArray(data.messages)) {
            throw new Error('messages response missing messages');
          }
          const messages = data.messages;
          return sendCommand({ type: 'get_state', sessionId: target }).then((latestState) => {
            const latest = (latestState || {}) as { isStreaming?: boolean };
            applyState(latestState, target);
            if (latest.isStreaming === true) {
              const shouldRestoreAssistant = shouldRestoreStreamingAssistant(messages);
              setItemsBySessionId((prev) => {
                const history = messagesToItems(messages);
                let assistantId = activeAssistantBySessionIdRef.current[target];
                const streamingItem = (prev[target] ?? []).find(
                  (item) => item.kind === 'assistant' && item.status === 'streaming' && item.id === assistantId,
                );
                if (streamingItem) return { ...prev, [target]: [...history, streamingItem] };
                // Application::is_streaming covers the whole run, including
                // tool execution. Only synthesize an assistant when durable
                // history says the next missing record is an assistant; an
                // assistant/toolCall tail needs no empty streaming shell.
                if (!shouldRestoreAssistant) {
                  return { ...prev, [target]: history };
                }
                assistantId = nextId('a');
                activeAssistantBySessionIdRef.current[target] = assistantId;
                return {
                  ...prev,
                  [target]: [...history, { kind: 'assistant', id: assistantId, status: 'streaming', blocks: [] }],
                };
              });
              delete optimisticQueueBySessionIdRef.current[target];
              return;
            }
            // MessageEnd may have committed between the first get_messages
            // and this settled state snapshot. Re-fetch after observing idle
            // so the replacement cannot use pre-settlement history.
            return sendCommand({ type: 'get_messages', sessionId: target }).then((settledData) => {
              if (!settledData || typeof settledData !== 'object' || !('messages' in settledData) || !Array.isArray(settledData.messages)) {
                throw new Error('settled messages response missing messages');
              }
              setItemsBySessionId((prev) => ({ ...prev, [target]: messagesToItems(settledData.messages) }));
              delete optimisticQueueBySessionIdRef.current[target];
              delete activeAssistantBySessionIdRef.current[target];
              const prefix = `${target}\u0000`;
              for (const key of streamBuf.keys()) {
                if (key.startsWith(prefix)) streamBuf.delete(key);
              }
              for (const key of liveNodes.keys()) {
                if (key.startsWith(prefix)) liveNodes.delete(key);
              }
            });
          });
        });
      })
      .then(() => {
        setConnState('on');
        bootRef.current = false;
        // Goal snapshot + journal for the now-bound session (the sid is
        // derived from the state response, so the refresh targets the right
        // runtime even on the very first connect).
        refreshGoal(sessionIdRef.current);
        sendCommand({ type: 'get_available_models' })
          .then((data) => {
            const list = (data as { models?: Array<{ id: string; name: string; provider: string }> }).models || [];
            setModels(list.filter((m) => m && m.id && m.provider));
          })
          .catch(() => {});
        sendCommand({ type: 'get_available_thinking_levels' })
          .then((data) => {
            const list = (data as { levels?: string[] }).levels || [];
            setLevels(list);
          })
          .catch(() => {});
      })
      .catch(() => {
        bootRef.current = false;
        scheduleReconnect();
      });
  }, [applyState, refreshGoal, rescheduleSilenceTimer, scheduleReconnect, sendCommand]);

  const connect = useCallback(() => {
    if (retryTimerRef.current !== null) {
      window.clearTimeout(retryTimerRef.current);
      retryTimerRef.current = null;
    }
    // Any timers still running belong to the previous socket (its onclose was
    // detached below); stop them so they can neither ping nor kill the new one.
    clearHeartbeatTimers();
    const old = wsRef.current;
    wsRef.current = null;
    if (old) {
      old.onclose = null;
      try {
        old.close(1000, 'replaced');
      } catch {
        /* already closed */
      }
    }
    // The host comes from the header input (hostRef) and the token from the
    // Settings panel (tokenRef) — both kept in sync with React state so this
    // callback never re-creates across renders.
    const tokenValue = tokenRef.current;
    const hostValue = hostRef.current;
    setConnState('connecting');
    const protocols = tokenValue ? [`${RPI_AUTH_PREFIX}${tokenValue}`] : [];
    let ws: WebSocket;
    try {
      const scheme = location.protocol === 'https:' ? 'wss://' : 'ws://';
      ws = new WebSocket(`${scheme}${hostValue}/ws`, protocols);
    } catch (err) {
      setConnState('off');
      toast(`cannot open WebSocket: ${(err as Error).message} — token must be a single token without spaces`, true);
      scheduleReconnect();
      return;
    }
    wsRef.current = ws;
    ws.onopen = onOpen;
    ws.onmessage = (event) => {
      // Any inbound frame is proof of life: slide the silence window.
      lastMessageAtRef.current = Date.now();
      rescheduleSilenceTimer();
      onMessage(event.data as string);
    };
    ws.onclose = (event) => {
      if (event.target !== wsRef.current) return; // superseded by a newer socket
      wsRef.current = null;
      clearHeartbeatTimers();
      setConnState('off');
      if (event.code === 1006) {
        // The boot auto-connect with no token is an expected probe on a fresh
        // page load (the user has not typed anything yet). On a tokenless
        // listener it succeeds and never reaches this branch; on a tokened
        // listener it is the expected "no token yet" refusal — stay quiet and
        // let the empty-hint explain the optional-token policy.
        if (!(bootProbeRef.current && !tokenValue)) {
          toast('connection failed (wrong or missing token?). Set the token in the Settings panel.', true);
        }
      } else if (event.code !== 1000) {
        toast(`connection closed (code ${event.code})${event.reason ? `: ${event.reason}` : ''}`, true);
      }
      scheduleReconnect();
    };
    ws.onerror = () => {
      /* the close event carries the failure */
    };
  }, [clearHeartbeatTimers, onOpen, rescheduleSilenceTimer, scheduleReconnect, toast]);

  /** Commit a host typed in the header: persist to recent hosts (max 10,
   *  most recent first) and, on a real change, fully reset the app and
   *  reconnect to the new host. */
  const commitHost = useCallback((raw: string) => {
    const next = raw.trim();
    if (!next) return;
    setRecentHosts((prev) => {
      const list = [next, ...prev.filter((h) => h !== next)].slice(0, RECENT_HOSTS_MAX);
      try {
        window.localStorage.setItem(RECENT_HOSTS_STORAGE_KEY, JSON.stringify(list));
      } catch {
        /* private mode: recents live in state only */
      }
      return list;
    });
    if (next === hostRef.current) return;
    bootProbeRef.current = false;
    resetAllState();
    hostRef.current = next;
    setHostInput(next);
    connect();
  }, [connect, resetAllState]);

  /** Settings-panel token commit: persist under `rpi-web-token` and reconnect
   *  so the new token takes effect (rpi-auth.<token> subprotocol). */
  const handleTokenChange = useCallback((nextToken: string) => {
    const trimmed = nextToken.trim();
    if (trimmed === tokenRef.current) return; // same token: nothing to reconnect
    bootProbeRef.current = false;
    tokenRef.current = trimmed;
    setToken(trimmed);
    try {
      window.localStorage.setItem(TOKEN_STORAGE_KEY, trimmed);
    } catch {
      /* private mode: token lives in state only */
    }
    connect();
  }, [connect]);

  /* ---------------- event dispatch ---------------- */
  //
  // ORDERING CONTRACT: every handler referenced by onEvent is declared ABOVE
  // it (executable-safe order) and listed in onEvent's dependency array —
  // there is no use-before-declaration (TDZ) and no suppressed dependency.

  const onMessageStart = useCallback((frame: EventFrame) => {
    const message = (frame.message || {}) as { role?: string; content?: unknown };
    const role = message.role;
    const sid = frameSession(frame);
    if (role === 'user') {
      const text = contentText(message.content);
      const queue = optimisticQueueBySessionIdRef.current;
      const queueId = sid ? (queue[sid] ?? []).shift() : undefined;
      if (queueId) {
        patchItemFor(sid, queueId, (item) => (item.kind === 'user' ? { ...item, optimistic: false } : item));
      } else {
        pushItemFor(sid, { kind: 'user', id: nextId('u'), text, optimistic: false });
      }
      return;
    }
    if (role === 'assistant') {
      setStreamingBySessionId((prev) => (sid ? { ...prev, [sid]: true } : prev));
      const id = nextId('a');
      // Synchronous (not itemsRef): message_start -> deltas -> message_end can
      // all arrive before React commits the new item, so the streaming path
      // must not depend on the post-render itemsRef snapshot. Per-session so
      // a session cutover mid-stream cannot cross-wire the assistant node.
      if (sid) activeAssistantBySessionIdRef.current[sid] = id;
      pushItemFor(sid, { kind: 'assistant', id, status: 'streaming', blocks: [] });
      return;
    }
    if (role === 'toolResult') {
      // The toolCard (tool_execution_start/end) already renders this result
      // inline — tool_execution_end carries the same AgentToolResult the
      // toolResult message does, bounded identically. Suppress the separate
      // toolResult card when a matching card exists so the transcript never
      // shows the same tool output twice (restored transcripts merge the
      // same way in messagesToItems); unmatched results stay readable.
      const text = boundOutput(contentText(message.content), TOOL_OUTPUT_LINE_LIMIT).text;
      if (message && typeof message === 'object' && 'toolCallId' in message && typeof message.toolCallId === 'string' && message.toolCallId !== '') {
        updateItemsFor(sid, (prev) =>
          prev.some((i) => i.kind === 'toolCard' && i.toolCallId === message.toolCallId)
            ? prev
            : [...prev, { kind: 'toolResult', id: nextId('r'), text }]
        );
      } else {
        pushItemFor(sid, { kind: 'toolResult', id: nextId('r'), text });
      }
      return;
    }
    if (role === 'bashExecution') {
      const m = message as { command?: string; output?: string };
      pushItemFor(sid, {
        kind: 'bash',
        id: nextId('b'),
        command: m.command || '',
        output: boundOutput(m.output || '', BASH_OUTPUT_LINE_LIMIT).text,
      });
      return;
    }
    if (role === 'custom') {
      // Mirrors messagesToItems so live events and restored entries stay
      // identical: display:false customs (internal system reminders /
      // orchestration scaffolding) never render; display:true customs surface
      // as a labeled card, with typed IRC customs showing their parsed view
      // (never the raw <orchestration-message> XML wrapper). The custom
      // message also arrives as message_end; onMessageEnd ignores non-assistant
      // roles, so this pushes exactly once.
      const item = customToItem(
        message as { display?: boolean; customType?: string; content?: unknown; details?: unknown },
      );
      if (item) pushItemFor(sid, item);
    }
  }, [frameSession, patchItemFor, pushItemFor, updateItemsFor]);

  const onMessageUpdate = useCallback((frame: EventFrame) => {
    const ev = (frame.assistantMessageEvent || {}) as { type?: string; delta?: string; content?: string };
    if (!ev.type) return;
    const sid = frameSession(frame);
    const targetId = sid ? (activeAssistantBySessionIdRef.current[sid] ?? '') : '';
    if (!targetId) return;
    setStreamingBySessionId((prev) => (sid ? { ...prev, [sid]: true } : prev));
    // Namespaced buffer: this session's deltas can never touch another
    // session's catch-up, even with equal item ids.
    const key = streamKey(sid, targetId);
    let buf = streamBuf.get(key);
    if (!buf) {
      buf = { text: '', thinking: '', toolcall: '' };
      streamBuf.set(key, buf);
    }
    switch (ev.type) {
      case 'text_delta':
        if (ev.delta) {
          buf.text += ev.delta;
          appendToLiveNode(sid, targetId, ev.delta, 'text');
        }
        break;
      case 'thinking_delta':
        if (ev.delta) {
          buf.thinking += ev.delta;
          appendToLiveNode(sid, targetId, ev.delta, 'thinking');
        }
        break;
      case 'toolcall_delta':
        if (ev.delta) {
          buf.toolcall += ev.delta;
          appendToLiveNode(sid, targetId, ev.delta, 'toolcall');
        }
        break;
      default:
        break; // start/end/done/error: message_end re-renders authoritatively
    }
  }, [appendToLiveNode, frameSession]);

  const onMessageEnd = useCallback((frame: EventFrame) => {
    const message = (frame.message || {}) as { role?: string; content?: ContentBlock[] };
    if (message.role !== 'assistant') return;
    const sid = frameSession(frame);
    const targetId = sid ? (activeAssistantBySessionIdRef.current[sid] ?? '') : '';
    if (!targetId) return;
    // Aborting right after turn_start can yield an authoritative message with
    // empty content while deltas were already relayed; fall back to the
    // streamed buffer so watched text is never wiped by the final render.
    const key = streamKey(sid, targetId);
    const buf = streamBuf.get(key);
    const hasRenderable = Array.isArray(message.content)
      ? (message.content as ContentBlock[]).some(
          (b) => b && ((b.type === 'text' && b.text) || (b.type === 'thinking' && b.thinking))
        )
      : false;
    const blocks: ContentBlock[] = hasRenderable
      ? (Array.isArray(message.content) ? message.content : []) as ContentBlock[]
      : [
          ...(buf && buf.text ? [{ type: 'text', text: buf.text } as ContentBlock] : []),
          ...(buf && buf.thinking ? [{ type: 'thinking', thinking: buf.thinking } as ContentBlock] : []),
        ];
    streamBuf.delete(key);
    patchItemFor(sid, targetId, (item) =>
      item.kind === 'assistant' ? { ...item, status: 'final' as const, blocks } : item
    );
    // Keep currentAssistant semantics: the streaming node stays mounted until
    // the next assistant message replaces it, so tool cards append nearby.
  }, [frameSession, patchItemFor]);

  const onToolStart = useCallback((frame: EventFrame) => {
    const sid = frameSession(frame);
    setStreamingBySessionId((prev) => (sid ? { ...prev, [sid]: true } : prev));
    pushItemFor(sid, {
      kind: 'toolCard',
      id: nextId('tc'),
      toolCallId: (frame.toolCallId as string) || '',
      toolName: safeText(frame.toolName || 'tool'),
      args: frame.args,
      status: 'running',
      result: '',
    });
  }, [frameSession, pushItemFor]);

  const onToolUpdate = useCallback((frame: EventFrame) => {
    const toolCallId = (frame.toolCallId as string) || '';
    const text = contentText((frame.partialResult as { content?: unknown } | undefined)?.content);
    if (!text) return;
    updateItemsFor(frameSession(frame), (prev) => applyToolSnapshot(prev, toolCallId, text));
  }, [frameSession, updateItemsFor]);

  const onToolEnd = useCallback((frame: EventFrame) => {
    const toolCallId = (frame.toolCallId as string) || '';
    const text = contentText((frame.result as { content?: unknown } | undefined)?.content);
    const isError = !!frame.isError;
    // Bound the rendered result to its tail like the TUI compact tool card so
    // a huge tool output never dominates the transcript while the error
    // status and trailing lines stay visible.
    updateItemsFor(frameSession(frame), (prev) =>
      applyToolSnapshot(prev, toolCallId, text, isError ? 'error' : 'done')
    );
  }, [frameSession, updateItemsFor]);

  const confirmAllOptimisticFor = useCallback(
    (sid: string | null) => {
      if (!sid) return;
      optimisticQueueBySessionIdRef.current[sid] = [];
      updateItemsFor(sid, (prev) => {
        if (!prev.some((i) => i.kind === 'user' && i.optimistic)) return prev;
        return prev.map((i) => (i.kind === 'user' && i.optimistic ? { ...i, optimistic: false } : i));
      });
    },
    [updateItemsFor]
  );

  /** D94: projected extension UI requests. Interactive asks (confirm/input/
   * select/editor) cannot be answered remotely by design, so they surface as
   * a transcript notice card + toast pointing the user to the terminal.
   * Notifications become toasts. All other projections are ignored. The item
   * is pushed to the OWNING session's cache; toasts fire only when the owning
   * session is the active view (a background session's approval must never
   * interrupt the active session's user). */
  const onExtensionUiRequest = useCallback((frame: EventFrame) => {
    const method = typeof frame.method === 'string' ? frame.method : '';
    const title = typeof frame.title === 'string' ? frame.title : '';
    const message = typeof frame.message === 'string' ? frame.message : '';
    const extensionId = typeof frame.extensionId === 'string' ? frame.extensionId : undefined;
    const owner = frameSession(frame);
    const active = owner === sessionIdRef.current;
    if (['confirm', 'input', 'select', 'editor'].includes(method)) {
      pushItemFor(owner, {
        kind: 'approval',
        id: nextId('ap'),
        method,
        title: title || method,
        message: message || title || '',
        extensionId,
      });
      if (active) {
        toast(`Approval needed (${method}${extensionId ? ` · ${extensionId}` : ''}) — answer in the terminal`);
      }
    } else if (method === 'notify' && active) {
      toast(title || message || 'extension notification');
    }
  }, [frameSession, pushItemFor, toast]);

  const onEvent = useCallback((frame: EventFrame) => {
    // Route every event to its session: explicit sessionId wins (backend
    // contract), else the active session. Background-session events update
    // ONLY the owning session's cache/unread — never the active panel's state
    // (Todo/Goal/Workflow/Subagents/Side chat/Maintenance/model/thinking).
    const sid = frameSession(frame);
    const active = sid === sessionIdRef.current;
    const setStreaming = (on: boolean) =>
      setStreamingBySessionId((prev) => (sid ? { ...prev, [sid]: on } : prev));
    switch (frame.type) {
      case 'turn_start':
        setStreaming(true);
        if (sid) abortPendingBySessionIdRef.current[sid] = false; // a new run starts fresh
        break;
      case 'turn_end':
        // v1 keeps a flat transcript; nothing to close.
        break;
      case 'message_start':
        onMessageStart(frame);
        break;
      case 'message_update':
        onMessageUpdate(frame);
        break;
      case 'message_end':
        onMessageEnd(frame);
        break;
      case 'tool_execution_start':
        onToolStart(frame);
        break;
      case 'tool_execution_update':
        onToolUpdate(frame);
        break;
      case 'tool_execution_end':
        onToolEnd(frame);
        break;
      case 'agent_settled':
        confirmAllOptimisticFor(sid);
        setStreaming(false);
        break;
      case 'run_failed':
        confirmAllOptimisticFor(sid);
        setStreaming(false);
        if (sid && abortPendingBySessionIdRef.current[sid]) {
          abortPendingBySessionIdRef.current[sid] = false;
          if (active) toast('run aborted');
        } else if (active) {
          toast(typeof frame.message === 'string' ? frame.message : 'run failed', true);
        }
        // A background session's failure never toasts (only its unread badge
        // bumps below): abort/toast state is isolated per session.
        break;
      case 'todo_updated':
      case 'todo_reminder':
        // D89: refresh the OWNING session's Todo cache from the authoritative
        // phases payload (background sessions never touch the active panel).
        if (sid && Array.isArray(frame.phases)) {
          setTodoPhasesBySessionId((prev) => ({ ...prev, [sid]: frame.phases as TodoPhaseWire[] }));
        }
        break;
      case 'workflow_updated':
      case 'workflow_status_changed':
      case 'workflow_removed':
        // D91: workflow panels remount per session and refetch
        // authoritatively; events reach ONLY the ACTIVE session's mounted
        // panel. Background workflow events never mutate active state.
        if (active) {
          dispatchWorkflowEvents(frame as Parameters<typeof dispatchWorkflowEvents>[0]);
        }
        break;
      case 'goal_updated':
        // D90: every goal mutation (create/pin/unpin/pause/resume/complete/
        // drop) pushes the resulting snapshot into the OWNING session's cache;
        // refresh that session's journal too (sid captured -> no race).
        if (sid && frame.state && typeof frame.state === 'object') {
          setGoalStateBySessionId((prev) => ({ ...prev, [sid]: frame.state as GoalStateWire }));
        }
        refreshGoal(sid);
        break;
      case 'goal_usage_charged':
        if (sid && frame.state && typeof frame.state === 'object') {
          setGoalStateBySessionId((prev) => ({ ...prev, [sid]: frame.state as GoalStateWire }));
        }
        break;
      case 'job_updated':
      case 'agent_updated':
      case 'message_delivered':
        // D93: live orchestration events refresh the Subagents panel — only
        // for the ACTIVE session (the mounted panel's handler belongs to the
        // active session; background events are never forwarded).
        if (active) {
          subagentsHandlersRef.current.forEach((handler) => handler(frame));
        }
        break;
      case 'extension_ui_request':
        // D94: projected extension UI events are non-interactive over RPC —
        // remote answering is rejected by design, so interactive asks render
        // as a notice card + toast ("answer in the terminal"). Item push is
        // owning-session scoped inside onExtensionUiRequest.
        onExtensionUiRequest(frame);
        break;
      default:
        break; // session/process/workflow events: v1 ignores
    }
    // Background activity bumps the owning session's unread badge (one bump
    // per meaningful event; high-frequency deltas stay silent).
    if (sid && !active && UNREAD_EVENT_TYPES[frame.type]) {
      markBackgroundEvent(sid);
    }
  }, [
    frameSession,
    markBackgroundEvent,
    confirmAllOptimisticFor,
    onMessageStart,
    onMessageUpdate,
    onMessageEnd,
    onToolStart,
    onToolUpdate,
    onToolEnd,
    onExtensionUiRequest,
    refreshGoal,
    toast,
  ]);

  const onMessage = useCallback((raw: string) => {
    let frame: EventFrame | RpcResponse;
    try {
      frame = JSON.parse(raw);
    } catch {
      return;
    }
    if (frame && frame.type === 'response') {
      onResponse(frame as RpcResponse);
      return;
    }
    if (!frame || typeof frame.type !== 'string') return;
    onEvent(frame as EventFrame);
  }, [onResponse, onEvent]);

  /* ---------------- composer ---------------- */

  const submit = useCallback((kind: 'prompt' | 'steer') => {
    const input = promptInputRef.current;
    if (!input) return;
    const text = input.value.trim();
    // Attached images become ContentBlocks; PDFs become a text note prepended
    // to the message so the model still sees the attachment context.
    const images: ImageContentBlock[] = [];
    const pdfNotes: string[] = [];
    for (const attachment of attachments) {
      if (attachment.kind === 'image') {
        images.push({ type: 'image', data: attachment.dataBase64, mimeType: attachment.mimeType });
      } else {
        pdfNotes.push(`[Attached PDF: ${attachment.name}, ${attachment.size} bytes]`);
      }
    }
    if (!text && images.length === 0 && pdfNotes.length === 0) return;
    const message = pdfNotes.length === 0 ? text : text ? `${pdfNotes.join('\n')}\n\n${text}` : pdfNotes.join('\n');
    const bubbleId = nextId('u');
    const sid = sessionIdRef.current;
    pushItemFor(sid, {
      kind: 'user',
      id: bubbleId,
      text: message || (images.length > 0 ? '(image attached)' : ''),
      optimistic: true,
    });
    if (sid) (optimisticQueueBySessionIdRef.current[sid] ??= []).push(bubbleId);
    input.value = '';
    autoResize(input);
    if (attachments.length > 0) setAttachments([]);
    // Enter and the primary button both resolve to the active run's verb: a
    // fresh prompt while idle, a steering message while a stream is in flight.
    // The dedicated Steer/Follow up controls were removed so the phone-width
    // composer's textarea stays dominant; queued follow-up is intentionally
    // dropped in favor of this simple default send/steer flow.
    const command: Record<string, unknown> =
      kind === 'steer' ? { type: 'steer', message } : { type: 'prompt', message };
    if (images.length > 0) command.images = images;
    sendCommand(command, bubbleId).catch((err: Error & { rpc?: boolean }) => {
      if (!err.rpc) toast(`send failed: ${err.message}`, true);
    });
  }, [attachments, pushItemFor, sendCommand, toast]);

  // Single-line composer: grow only to ~3 lines (measured from the input's
  // own line-height + vertical padding, so the cap stays right at any font
  // size) and collapse back to 1 line whenever the content shrinks.
  const autoResize = useCallback((input: HTMLTextAreaElement) => {
    input.style.height = 'auto';
    const style = window.getComputedStyle(input);
    const lineHeight = parseFloat(style.lineHeight) || 20;
    const pad = (parseFloat(style.paddingTop) || 0) + (parseFloat(style.paddingBottom) || 0);
    input.style.height = `${Math.min(input.scrollHeight, Math.round(lineHeight * 3 + pad))}px`;
  }, []);

  /* ---------------- composer: file attachments ---------------- */

  const onFilesChosen = useCallback((fileList: FileList | null) => {
    if (!fileList || fileList.length === 0) return;
    let skipped = 0;
    for (const file of Array.from(fileList)) {
      const isPdf = file.type === 'application/pdf' || /\.pdf$/i.test(file.name);
      const isImage = !isPdf && file.type.startsWith('image/');
      if (!isImage && !isPdf) {
        skipped += 1;
        continue;
      }
      const reader = new FileReader();
      reader.onload = () => {
        const dataUrl = typeof reader.result === 'string' ? reader.result : '';
        const comma = dataUrl.indexOf(',');
        const dataBase64 = comma >= 0 ? dataUrl.slice(comma + 1) : dataUrl;
        setAttachments((prev) => [
          ...prev,
          {
            id: nextId('a'),
            name: file.name,
            size: file.size,
            mimeType: isPdf ? 'application/pdf' : file.type || 'image/png',
            kind: isImage ? 'image' : 'pdf',
            dataBase64,
            previewUrl: isImage ? dataUrl : undefined,
          },
        ]);
      };
      reader.readAsDataURL(file);
    }
    if (skipped > 0) toast(`${skipped} file(s) skipped — attach images or PDFs only`, true);
  }, [toast]);

  const removeAttachment = useCallback((id: string) => {
    setAttachments((prev) => prev.filter((attachment) => attachment.id !== id));
  }, []);

  /* ---------------- composer: hold-to-talk voice ---------------- */

  /** Resolve STT endpoint settings: prefer what get_state advertised, then
   *  probe the settings catalog (`live.sttBaseUrl` / `live.sttModel` are
   *  non-secret; `live.sttApiKey` is redacted server-side, so an endpoint
   *  requiring auth must have its key set elsewhere). */
  const resolveSttSettings = useCallback(async (): Promise<{ baseUrl: string; apiKey: string; model: string } | null> => {
    let baseUrl = (liveSettings?.sttBaseUrl ?? '').trim();
    const apiKey = (liveSettings?.sttApiKey ?? '').trim();
    let model = (liveSettings?.sttModel ?? '').trim();
    if (!baseUrl) {
      try {
        // RPC responses mirror SettingsPanel's SettingsCatalogWire.
        const catalog = (await sendCommand({ type: 'settings_inspect' })) as {
          values?: Array<{ definition?: { key?: string }; effectiveValue?: unknown }>;
        };
        const views = Array.isArray(catalog?.values) ? catalog.values : [];
        for (const view of views) {
          const key = view?.definition?.key;
          if (key === 'live.sttBaseUrl' && typeof view.effectiveValue === 'string') {
            baseUrl = view.effectiveValue.trim();
          } else if (key === 'live.sttModel' && typeof view.effectiveValue === 'string') {
            model = view.effectiveValue.trim();
          }
        }
      } catch {
        // Catalog unavailable (older backend): fall through to the error path.
      }
    }
    if (!baseUrl) return null;
    return { baseUrl, apiKey, model: model || 'whisper-1' };
  }, [liveSettings, sendCommand]);

  const transcribeAudio = useCallback(
    async (audioBlob: Blob) => {
      const settings = await resolveSttSettings();
      if (!settings) {
        toast(
          'Live voice is not configured — set Settings.live.sttBaseUrl (and sttApiKey if the endpoint requires it) in the terminal',
          true
        );
        return;
      }
      setTranscribing(true);
      try {
        // Prefer the WAV container the backend's SttClient sends; fall back to
        // the recorder's native container when decoding is unavailable.
        const wav = await blobToWav(audioBlob);
        const fileBlob = wav ?? audioBlob;
        const form = new FormData();
        form.append('file', fileBlob, wav ? 'voice.wav' : 'voice.webm');
        form.append('model', settings.model);
        const headers: Record<string, string> = {};
        if (settings.apiKey) headers.Authorization = `Bearer ${settings.apiKey}`;
        const base = settings.baseUrl.replace(/\/+$/, '');
        const response = await fetch(`${base}/v1/audio/transcriptions`, { method: 'POST', headers, body: form });
        if (!response.ok) {
          const body = (await response.text().catch(() => '')).slice(0, 300);
          toast(`transcription failed (${response.status}): ${body || response.statusText}`, true);
          return;
        }
        // The transcript lands in the composer for review before sending.
        const payload = (await response.json().catch(() => null)) as { text?: unknown } | null;
        const text = payload && typeof payload === 'object' && typeof payload.text === 'string' ? payload.text : '';
        const input = promptInputRef.current;
        if (input && text) {
          const current = input.value;
          const separator = current.trim() ? (current.endsWith('\n') ? '' : '\n') : '';
          input.value = `${current}${separator}${text}`;
          autoResize(input);
          input.focus();
        }
      } catch (err) {
        // A fetch TypeError ("Failed to fetch") usually means CORS or an
        // unreachable endpoint — the STT server must allow this web origin.
        toast(`transcription failed: ${err instanceof Error ? err.message : String(err)}`, true);
      } finally {
        setTranscribing(false);
      }
    },
    [resolveSttSettings, toast, autoResize]
  );

  const stopRecording = useCallback(() => {
    if (recordingTimerRef.current !== null) {
      window.clearTimeout(recordingTimerRef.current);
      recordingTimerRef.current = null;
    }
    const recorder = mediaRecorderRef.current;
    mediaRecorderRef.current = null;
    if (mediaStreamRef.current) {
      mediaStreamRef.current.getTracks().forEach((track) => track.stop());
      mediaStreamRef.current = null;
    }
    if (recorder && recorder.state !== 'inactive') {
      try {
        recorder.stop();
      } catch {
        setRecording(false);
      }
    } else {
      setRecording(false);
    }
  }, []);

  const startRecording = useCallback(async () => {
    if (recording || transcribing) return;
    if (!navigator.mediaDevices?.getUserMedia) {
      toast('microphone requires HTTPS (or localhost). Use https:// or connect via 127.0.0.1.', true);
      return;
    }
    try {
      const stream = await navigator.mediaDevices.getUserMedia({ audio: true });
      const supported = ['audio/webm;codecs=opus', 'audio/webm', 'audio/mp4'].filter(
        (mime) => typeof MediaRecorder !== 'undefined' && MediaRecorder.isTypeSupported(mime)
      );
      const recorder = new MediaRecorder(stream, supported.length > 0 ? { mimeType: supported[0] } : undefined);
      mediaStreamRef.current = stream;
      mediaRecorderRef.current = recorder;
      mediaChunksRef.current = [];
      recorder.ondataavailable = (e) => {
        if (e.data && e.data.size > 0) mediaChunksRef.current.push(e.data);
      };
      recorder.onstop = () => {
        setRecording(false);
        const chunks = mediaChunksRef.current;
        mediaChunksRef.current = [];
        if (chunks.length === 0) return;
        const blob = new Blob(chunks, { type: recorder.mimeType || 'audio/webm' });
        void transcribeAudio(blob);
      };
      recorder.start();
      setRecording(true);
      // Safety cap: auto-release a stuck press after 60s instead of recording
      // forever.
      recordingTimerRef.current = window.setTimeout(() => stopRecording(), 60000);
    } catch (err) {
      if (mediaStreamRef.current) {
        mediaStreamRef.current.getTracks().forEach((track) => track.stop());
        mediaStreamRef.current = null;
      }
      toast(`microphone unavailable: ${err instanceof Error ? err.message : String(err)}`, true);
    }
  }, [recording, transcribing, stopRecording, transcribeAudio, toast]);

  /* ---------------- composer: realtime voice (Codex Live WebRTC) ---------------- */

  /** True when the backend advertises realtime mode; the mic becomes a
   *  click-to-toggle call button instead of hold-to-talk STT. */
  const isRealtimeMode = liveSettings?.mode === 'realtime';

  /** Commit a final transcript to the composer textarea, mirroring the STT
   *  flow's commit so both voice paths land the draft the same way. */
  const commitTranscriptToComposer = useCallback(
    (text: string) => {
      const input = promptInputRef.current;
      if (!input || !text) return;
      const current = input.value;
      const separator = current.trim() ? (current.endsWith('\n') ? '' : '\n') : '';
      input.value = `${current}${separator}${text}`;
      autoResize(input);
      input.focus();
    },
    [autoResize]
  );

  /** Sideband event dispatch: transcript deltas stream into the overlay, a
   *  final transcript commits to the composer, delegations get a notification
   *  row, errors surface as toasts. Output audio is delivered over WebRTC
   *  (not this socket), so output_audio.delta is intentionally a no-op. */
  const handleRealtimeFrame = useCallback(
    (frame: Record<string, unknown>) => {
      const type = typeof frame.type === 'string' ? frame.type : '';
      switch (type) {
        case 'transcript.delta': {
          const delta = firstString(frame.delta);
          if (!delta) return;
          realtimeTranscriptRef.current += delta;
          const node = realtimeTranscriptNodeRef.current;
          if (node) node.textContent = realtimeTranscriptRef.current;
          break;
        }
        case 'transcript.done': {
          const text = firstString(frame.transcript, frame.text, frame.delta);
          if (text) commitTranscriptToComposer(text);
          break;
        }
        case 'delegation.created': {
          const d = frame.delegation;
          const delegationText =
            typeof d === 'string'
              ? firstString(d)
              : d && typeof d === 'object'
                ? firstString(
                    (d as Record<string, unknown>).description,
                    (d as Record<string, unknown>).task,
                    (d as Record<string, unknown>).id
                  )
                : '';
          setRealtimeDelegation(delegationText || 'Delegation created');
          break;
        }
        case 'error': {
          toast(firstString(frame.message) || 'realtime session error', true);
          break;
        }
        case 'output_audio.delta':
          // Played by WebRTC automatically — nothing to render here.
          break;
        default:
          break;
      }
    },
    [commitTranscriptToComposer, toast]
  );

  /** Tear down the realtime call: close the sideband WS, close the
   *  RTCPeerConnection, stop the mic tracks, and tell the backend the session
   *  is over. Idempotent; `silent` skips the realtime_stop RPC (unmount path,
   *  where the main socket is already gone). */
  const stopRealtime = useCallback(
    (opts?: { silent?: boolean }) => {
      const hadSession = realtimePcRef.current !== null || realtimeWsRef.current !== null;
      const ws = realtimeWsRef.current;
      realtimeWsRef.current = null;
      if (ws) {
        ws.onopen = null;
        ws.onmessage = null;
        ws.onerror = null;
        ws.onclose = null;
        try {
          ws.close();
        } catch {
          /* already closed */
        }
      }
      const pc = realtimePcRef.current;
      realtimePcRef.current = null;
      if (pc) {
        pc.ontrack = null;
        pc.onconnectionstatechange = null;
        try {
          pc.close();
        } catch {
          /* already closed */
        }
      }
      if (mediaStreamRef.current) {
        mediaStreamRef.current.getTracks().forEach((track) => track.stop());
        mediaStreamRef.current = null;
      }
      const audio = realtimeAudioRef.current;
      if (audio) audio.srcObject = null;
      realtimeTranscriptRef.current = '';
      const node = realtimeTranscriptNodeRef.current;
      if (node) node.textContent = '';
      setRealtimeDelegation(null);
      setRealtimeActive(false);
      if (hadSession && !opts?.silent) {
        sendCommand({ type: 'realtime_stop' }).catch(() => {});
      }
    },
    [sendCommand]
  );

  /** Start a Codex Live realtime call: mic track -> RTCPeerConnection ->
   *  SDP offer over the realtime_create_call RPC -> answer -> sideband WS for
   *  transcript/delegation events (incoming audio rides the WebRTC track). */
  const startRealtime = useCallback(async () => {
    if (realtimeActive || realtimeBusyRef.current) return;
    const baseUrl = (liveSettings?.realtimeBaseUrl ?? '').trim();
    if (!baseUrl) {
      toast(
        'Realtime voice is not configured — set Settings.live.realtimeBaseUrl (and realtimeModel/voice) in the terminal',
        true
      );
      return;
    }
    if (!navigator.mediaDevices?.getUserMedia) {
      toast('microphone requires HTTPS (or localhost). Use https:// or connect via 127.0.0.1.', true);
      return;
    }
    if (typeof RTCPeerConnection === 'undefined') {
      toast('This browser does not support WebRTC (RTCPeerConnection)', true);
      return;
    }
    const model = (liveSettings?.realtimeModel ?? '').trim() || 'gpt-realtime-1.5';
    const voice = (liveSettings?.voice ?? '').trim() || 'sol';
    let stream: MediaStream | null = null;
    let pc: RTCPeerConnection | null = null;
    let ws: WebSocket | null = null;
    realtimeBusyRef.current = true;
    try {
      stream = await navigator.mediaDevices.getUserMedia({ audio: true });
      pc = new RTCPeerConnection();
      realtimePcRef.current = pc;
      mediaStreamRef.current = stream;
      for (const track of stream.getTracks()) pc.addTrack(track, stream);
      pc.ontrack = (event) => {
        const audio = realtimeAudioRef.current;
        if (audio && event.streams.length > 0) {
          audio.srcObject = event.streams[0];
          void audio.play().catch(() => {});
        }
      };
      pc.onconnectionstatechange = () => {
        if (pc && (pc.connectionState === 'failed' || pc.connectionState === 'closed')) {
          stopRealtime();
        }
      };
      const offer = await pc.createOffer();
      await pc.setLocalDescription(offer);
      const result = (await sendCommand({ type: 'realtime_create_call', sdpOffer: offer.sdp ?? '' })) as {
        sdp?: unknown;
        callId?: unknown;
      };
      const sdp = typeof result?.sdp === 'string' ? result.sdp : '';
      const callId = typeof result?.callId === 'string' ? result.callId : '';
      if (!sdp || !callId) {
        throw new Error('realtime_create_call returned no SDP answer or call id');
      }
      await pc.setRemoteDescription({ type: 'answer', sdp });
      // Sideband events (transcript/delegation) ride a separate WS; audio
      // flows over the WebRTC connection itself.
      const socket = new WebSocket(sidebandRealtimeWsUrl(baseUrl, callId));
      ws = socket;
      realtimeWsRef.current = socket;
      socket.onopen = () => {
        // Advertise the session config the backend negotiated for.
        socket.send(JSON.stringify({ type: 'session.update', session: { model, voice } }));
      };
      socket.onmessage = (event) => {
        try {
          const frame = JSON.parse(String(event.data)) as Record<string, unknown>;
          if (frame && typeof frame === 'object') handleRealtimeFrame(frame);
        } catch {
          // Non-JSON frames (pings/heartbeats) are ignored.
        }
      };
      socket.onerror = () => {
        if (realtimeWsRef.current === socket) toast('realtime sideband connection failed', true);
      };
      socket.onclose = () => {
        if (realtimeWsRef.current === socket) realtimeWsRef.current = null;
      };
      realtimeTranscriptRef.current = '';
      const node = realtimeTranscriptNodeRef.current;
      if (node) node.textContent = '';
      setRealtimeDelegation(null);
      setRealtimeActive(true);
    } catch (err) {
      if (ws) {
        realtimeWsRef.current = null;
        try {
          ws.onclose = null;
          ws.close();
        } catch {
          /* already closed */
        }
      }
      if (pc) {
        realtimePcRef.current = null;
        try {
          pc.ontrack = null;
          pc.close();
        } catch {
          /* already closed */
        }
      }
      if (stream) {
        mediaStreamRef.current = null;
        stream.getTracks().forEach((track) => track.stop());
      }
      toast(`realtime call failed: ${err instanceof Error ? err.message : String(err)}`, true);
    } finally {
      realtimeBusyRef.current = false;
    }
  }, [realtimeActive, liveSettings, sendCommand, handleRealtimeFrame, toast, stopRealtime]);

  // If the backend stops advertising realtime mode while a call is up
  // (settings changed), tear the call down cleanly.
  useEffect(() => {
    if (realtimeActive && !isRealtimeMode) stopRealtime();
  }, [realtimeActive, isRealtimeMode, stopRealtime]);

  const onModelChange = useCallback((key: string) => {
    if (!key) return;
    const slash = key.indexOf('/');
    const provider = key.slice(0, slash);
    const modelId = key.slice(slash + 1);
    sendCommand({ type: 'set_model', provider, modelId })
      .then(() => sendCommand({ type: 'get_state' }))
      .then(applyState)
      .catch(() => {});
  }, [applyState, sendCommand]);

  const onThinkingChange = useCallback((level: string) => {
    if (!level) return;
    sendCommand({ type: 'set_thinking_level', level })
      .then(() => sendCommand({ type: 'get_state' }))
      .then(applyState)
      .catch(() => {});
  }, [applyState, sendCommand]);

  /* ---------------- effects ---------------- */

  // Restore the token and connect on boot.
  useEffect(() => {
    let saved = '';
    try {
      saved = window.localStorage.getItem(TOKEN_STORAGE_KEY) || '';
    } catch {
      /* private mode */
    }
    if (saved) {
      // Set the ref synchronously so the very first connection already uses
      // the saved token (React state would lag one render).
      tokenRef.current = saved.trim();
      setToken(tokenRef.current);
    }
    connect();
    return () => {
      if (retryTimerRef.current !== null) window.clearTimeout(retryTimerRef.current);
      clearHeartbeatTimers();
      // Release the mic/WebRTC/sideband resources; the realtime_stop RPC is
      // skipped (silent) because the main socket is closing underneath us.
      stopRealtime({ silent: true });
      const ws = wsRef.current;
      wsRef.current = null;
      if (ws) ws.close(1000, 'unload');
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Auto-scroll: keep pinned to the bottom while the user is at the bottom.
  const onTranscriptScroll = useCallback(() => {
    const el = transcriptRef.current;
    if (!el) return;
    nearBottomRef.current = el.scrollHeight - el.scrollTop - el.clientHeight < 80;
  }, []);

  useEffect(() => {
    if (nearBottomRef.current && transcriptRef.current) {
      transcriptRef.current.scrollTop = transcriptRef.current.scrollHeight;
    }
  }, [activeItems]);

  /* ---------------- render ---------------- */

  return (
    <>
      <header>
        <div className="brand">
          rpi<span className="brand-sub">web</span>
        </div>
        <span id="conn-state" className="pill" data-state={connState}>
          {connState === 'on' ? 'connected' : connState === 'connecting' ? 'connecting…' : connState === 'reconnecting' ? 'reconnecting…' : 'offline'}
        </span>
        <span id="stream-badge" className="badge" hidden={!activeStreaming}>
          streaming
        </span>
        <span id="session-name" className="session-name" title={activeSessionName}>
          {activeSessionName ? `session: ${activeSessionName}` : ''}
        </span>
        <select
          id="model-select"
          title="Model (set_model)"
          disabled={models.length === 0}
          value={activeModelKey}
          onChange={(e) => onModelChange(e.target.value)}
        >
          {models.length === 0 && <option value="">model…</option>}
          {models.map((m) => (
            <option key={`${m.provider}/${m.id}`} value={`${m.provider}/${m.id}`}>
              {m.name || m.id}
            </option>
          ))}
        </select>
        <select
          id="thinking-select"
          title="Thinking level (set_thinking_level)"
          disabled={levels.length === 0}
          value={activeThinkingLevel}
          onChange={(e) => onThinkingChange(e.target.value)}
        >
          {levels.length === 0 && <option value="">thinking…</option>}
          {levels.map((level) => (
            <option key={level} value={level}>
              {level}
            </option>
          ))}
        </select>
        <input
          id="host-input"
          type="text"
          list="recent-hosts"
          placeholder="host:port"
          title="rpi listener host:port — the page auto-connects on load; Enter or blur reconnects to the typed host"
          autoComplete="off"
          spellCheck={false}
          value={hostInput}
          onChange={(e) => setHostInput(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Enter') {
              commitHost(hostInput);
              e.currentTarget.blur();
            }
          }}
          onBlur={() => {
            if (hostInput.trim() === '') setHostInput(hostRef.current);
            commitHost(hostInput);
          }}
        />
        <datalist id="recent-hosts">
          {recentHosts.map((host) => (
            <option key={host} value={host} />
          ))}
        </datalist>
        <button
          id="sidebar-toggle-btn"
          type="button"
          className={sidebarOpen ? 'panel-toggle panel-toggle--open' : 'panel-toggle'}
          aria-pressed={sidebarOpen}
          title="Toggle the session sidebar"
          onClick={() => setSidebarOpen((open) => !open)}
        >
          ☰
        </button>
      </header>

      <div className={`app-layout${sidebarOpen ? ' app-layout--drawer-open' : ''}`}>
        {sidebarOpen && (
          <div className="sidebar-backdrop" onClick={() => setSidebarOpen(false)} />
        )}
        <SessionSidebar
          sendCommand={sendCommand}
          onLifecycleResult={onLifecycleResult}
          activeSessionId={sessionId}
          unreadBySessionId={unreadBySessionId}
          onReopenRail={() => setSidebarOpen(true)}
          onCloseSession={onCloseSession}
          onOpenManage={() => {
            setSidebarOpen(false);
            setActivePanel('session');
          }}
          onSwitchComplete={() => {
            // Mobile: closing the drawer after a pick; desktop keeps the rail.
            if (window.matchMedia('(max-width: 720px)').matches) setSidebarOpen(false);
          }}
          featureNav={
            <>
              <button
                id="todos-toggle-btn"
                type="button"
                className={activePanel === 'todo' ? 'panel-toggle panel-toggle--open' : 'panel-toggle'}
                aria-pressed={activePanel === 'todo'}
                title="Todo DAG panel (phases, tasks, dependencies, live updates)"
                onClick={() => setActivePanel(activePanel === 'todo' ? '' : 'todo')}
              >
                Todos
              </button>
              <button
                id="goal-panel-btn"
                type="button"
                className={activePanel === 'goal' ? 'panel-toggle panel-toggle--open' : 'panel-toggle'}
                aria-pressed={activePanel === 'goal'}
                title="Goal panel (objective, status, token budget, pins, journal)"
                onClick={() => setActivePanel(activePanel === 'goal' ? '' : 'goal')}
              >
                Goal
              </button>
              <button
                id="workflow-toggle-btn"
                type="button"
                className={activePanel === 'workflow' ? 'panel-toggle panel-toggle--open' : 'panel-toggle'}
                aria-pressed={activePanel === 'workflow'}
                title="Workflow panel (list, detail, live workers, create/pause/resume/cancel/integrate/remove)"
                onClick={() => setActivePanel(activePanel === 'workflow' ? '' : 'workflow')}
              >
                Workflows
              </button>
              <button
                id="sidechat-toggle-btn"
                type="button"
                className={activePanel === 'sidechat' ? 'panel-toggle panel-toggle--open' : 'panel-toggle'}
                aria-pressed={activePanel === 'sidechat'}
                title="Side chat: parallel /btw sessions (own tab, transcript, prompt)"
                onClick={() => setActivePanel(activePanel === 'sidechat' ? '' : 'sidechat')}
              >
                Side chat
              </button>
              <button
                id="maintenance-toggle-btn"
                type="button"
                className={activePanel === 'maintenance' ? 'panel-toggle panel-toggle--open' : 'panel-toggle'}
                aria-pressed={activePanel === 'maintenance'}
                title="Maintenance: compact (A→B tokens), snapcompact, rewind, handoff, queue"
                onClick={() => setActivePanel(activePanel === 'maintenance' ? '' : 'maintenance')}
              >
                Maintain
              </button>
              <button
                id="subagents-toggle-btn"
                type="button"
                className={activePanel === 'subagents' ? 'panel-toggle panel-toggle--open' : 'panel-toggle'}
                aria-pressed={activePanel === 'subagents'}
                title="Subagents: live jobs (id/type/status/activity/elapsed), spawn task, hub message, cancel, output view"
                onClick={() => setActivePanel(activePanel === 'subagents' ? '' : 'subagents')}
              >
                Subagents
              </button>
              <button
                id="session-toggle-btn"
                type="button"
                className={activePanel === 'session' ? 'panel-toggle panel-toggle--open' : 'panel-toggle'}
                aria-pressed={activePanel === 'session'}
                title="Session panel: current session info, new/switch/fork/clone/rename"
                onClick={() => setActivePanel(activePanel === 'session' ? '' : 'session')}
              >
                Session
              </button>
              <button
                id="settings-toggle-btn"
                type="button"
                className={activePanel === 'settings' ? 'panel-toggle panel-toggle--open' : 'panel-toggle'}
                aria-pressed={activePanel === 'settings'}
                title="Settings panel: browse by category, typed edit, draft/apply"
                onClick={() => setActivePanel(activePanel === 'settings' ? '' : 'settings')}
              >
                Settings
              </button>
            </>
          }
        />
        <div className="app-main">
      <main id="transcript" aria-live="polite" ref={transcriptRef} onScroll={onTranscriptScroll}>
        {activeItems.length === 0 && (
          <div className="empty-hint">
            {connState === 'on' ? (
              <>Send a prompt to start the session.</>
            ) : (
              <>
                Connecting to the control plane… The token is optional: leave it blank when
                the listener runs without <code>--listen-token-file</code> (the page
                auto-connects); if the listener was started with a token, set it under
                Settings → Connection and the page reconnects immediately.
              </>
            )}
          </div>
        )}
        {activeItems.map((item) => {
          switch (item.kind) {
            case 'user':
              return (
                <div key={item.id} className={`msg msg--user${item.optimistic ? ' optimistic' : ''}`}>
                  {item.text}
                </div>
              );
            case 'assistant':
              return item.status === 'streaming' ? (
                <StreamingAssistant key={item.id} sid={sessionId ?? ''} id={item.id} />
              ) : (
                <FinalAssistant key={item.id} blocks={item.blocks} />
              );
            case 'toolCard':
              return <ToolCard key={item.id} item={item} />;
            case 'toolResult':
              return (
                <BashCard
                  key={item.id}
                  label="tool output"
                  output={item.text === '' ? '(empty tool result)' : item.text}
                />
              );
            case 'bash':
              return <BashCard key={item.id} command={item.command} output={item.output} />;
            case 'custom':
              return (
                <div key={item.id} className="msg msg--custom" role="note">
                  <span className="msg--custom__label">{safeText(item.label)}</span>
                  <span className="msg--custom__text">{safeText(item.text)}</span>
                </div>
              );
            case 'summary':
              return (
                <div key={item.id} className="msg msg--summary" role="note">
                  <span className="msg--summary__label">{safeText(item.label)}</span>
                  <span className="msg--summary__text">{safeText(item.text)}</span>
                </div>
              );
            case 'approval':
              return (
                <div key={item.id} className="approval" role="status">
                  <div className="approval__head">
                    <span className="approval__method">{item.method}</span>
                    <span className="approval__title">{safeText(item.title)}</span>
                    {item.extensionId && <span className="approval__extension">{item.extensionId}</span>}
                  </div>
                  {item.message !== '' && <div className="approval__question">{safeText(item.message)}</div>}
                  <div className="approval__note">Answer in the terminal — remote answering is disabled by design.</div>
                </div>
              );
          }
        })}
      </main>

      {/* panel mount point — every panel is KEYED by (panel, session) so its
          internal drafts/controllers (todo form, goal objective, workflow
          create fields, side-chat prompt, settings draft, maintenance result,
          subagent message drafts) can never survive an A→B session switch as
          though they belonged to B. Each panel re-fetches from the backend
          for the active session on mount (sendCommand injects the top-level
          sessionId). */}
      {activePanel === 'todo' && (
        <TodoPanel
          key={`todo:${sessionId ?? ''}`}
          phases={activeTodoPhases}
          onOp={runTodoOp}
          onClose={() => setActivePanel('')}
        />
      )}
      {activePanel === 'subagents' && (
        <SubagentsPanel
          key={`subagents:${sessionId ?? ''}`}
          sendCommand={sendCommand}
          subscribeEvents={subscribeSubagentsEvents}
          onClose={() => setActivePanel('')}
        />
      )}
      {activePanel === 'goal' && (
        <GoalPanel
          key={`goal:${sessionId ?? ''}`}
          state={activeGoalState}
          journal={activeGoalJournal}
          sendCommand={sendCommand}
          onChanged={() => refreshGoal(sessionIdRef.current)}
          onClose={() => setActivePanel('')}
        />
      )}
      {activePanel === 'workflow' && (
        <WorkflowPanel
          key={`workflow:${sessionId ?? ''}`}
          sendCommand={sendCommand}
          onClose={() => setActivePanel('')}
        />
      )}
      {activePanel === 'sidechat' && (
        <SideChatPanel
          key={`sidechat:${sessionId ?? ''}`}
          snapshot={activeSideChat}
          onNew={sideChatNew}
          onSwitch={sideChatSwitch}
          onClose={sideChatClose}
          onPrompt={sideChatPrompt}
          onClosePanel={() => setActivePanel('')}
        />
      )}
      {activePanel === 'maintenance' && (
        <MaintenancePanel
          key={`maintenance:${sessionId ?? ''}`}
          rpc={sendCommand}
          onClosePanel={() => setActivePanel('')}
        />
      )}
      {activePanel === 'session' && (
        <SessionPanel
          key={`session:${sessionId ?? ''}`}
          sendCommand={sendCommand}
          refreshState={refreshState}
          onLifecycleResult={onLifecycleResult}
          onClose={() => setActivePanel('')}
        />
      )}
      {activePanel === 'settings' && (
        <SettingsPanel
          key={`settings:${sessionId ?? ''}`}
          sendCommand={sendCommand}
          refreshState={refreshState}
          token={token}
          onTokenChange={handleTokenChange}
          onClose={() => setActivePanel('')}
        />
      )}

      <footer>
        <div id="composer-main">
          {attachments.length > 0 && (
            <div id="composer-attachments" aria-label="Attached files">
              {attachments.map((attachment) => (
                <span key={attachment.id} className="composer-attachment" title={attachment.name}>
                  {attachment.kind === 'image' && attachment.previewUrl ? (
                    <img className="composer-attachment__thumb" src={attachment.previewUrl} alt="" />
                  ) : (
                    <span className="composer-attachment__badge">PDF</span>
                  )}
                  <span className="composer-attachment__meta">
                    <span className="composer-attachment__name">{safeText(attachment.name)}</span>
                    <span className="composer-attachment__size">{attachment.size} bytes</span>
                  </span>
                  <button
                    type="button"
                    className="composer-attachment__remove"
                    title="Remove attachment"
                    aria-label={`Remove ${attachment.name}`}
                    onClick={() => removeAttachment(attachment.id)}
                  >
                    ✕
                  </button>
                </span>
              ))}
            </div>
          )}
          {isRealtimeMode && realtimeActive && (
            <div id="realtime-transcript" role="status" aria-live="polite">
              <div className="realtime-transcript__head">
                <span className="realtime-transcript__dot" aria-hidden="true" />
                <span className="realtime-transcript__label">realtime voice</span>
                {(liveSettings?.realtimeModel || liveSettings?.voice) && (
                  <span className="realtime-transcript__model">
                    {[liveSettings?.realtimeModel, liveSettings?.voice].filter(Boolean).join(' · ')}
                  </span>
                )}
              </div>
              {realtimeDelegation && (
                <div className="realtime-transcript__delegation">delegation created: {safeText(realtimeDelegation)}</div>
              )}
              <div
                ref={(node) => {
                  realtimeTranscriptNodeRef.current = node;
                  if (node) node.textContent = realtimeTranscriptRef.current;
                }}
                className="realtime-transcript__text"
              />
            </div>
          )}
          <textarea
            id="prompt-input"
            ref={promptInputRef}
            rows={1}
            placeholder="Message the agent… (Enter to send, Shift+Enter for a newline, Esc to abort)"
            onKeyDown={(e) => {
              if (e.key === 'Enter' && !e.shiftKey) {
                e.preventDefault();
                submit(activeStreaming ? 'steer' : 'prompt');
              } else if (e.key === 'Escape') {
                if (activeStreaming && sessionId) {
                  abortPendingBySessionIdRef.current[sessionId] = true;
                  sendCommand({ type: 'abort' }).catch(() => {});
                }
              }
            }}
            onInput={(e) => autoResize(e.currentTarget)}
          />
        </div>
        <div id="composer-buttons">
          <button
            id="attach-btn"
            type="button"
            title="Attach an image or PDF"
            onClick={() => fileInputRef.current?.click()}
          >
            📎
          </button>
          <button
            id="mic-btn"
            type="button"
            aria-pressed={isRealtimeMode ? realtimeActive : recording}
            disabled={!isRealtimeMode && transcribing}
            className={
              isRealtimeMode
                ? realtimeActive
                  ? 'recording'
                  : ''
                : recording
                  ? 'recording'
                  : transcribing
                    ? 'transcribing'
                    : ''
            }
            title={
              isRealtimeMode
                ? realtimeActive
                  ? 'Realtime voice on — click to stop'
                  : 'Realtime voice — click to talk'
                : recording
                  ? 'Recording… release to transcribe'
                  : transcribing
                    ? 'Transcribing…'
                    : 'Hold to talk (voice to text)'
            }
            onClick={
              isRealtimeMode
                ? () => {
                    if (realtimeActive) stopRealtime();
                    else void startRealtime();
                  }
                : undefined
            }
            onPointerDown={
              isRealtimeMode
                ? undefined
                : (e) => {
                    if (e.button !== 0) return;
                    e.preventDefault();
                    void startRecording();
                  }
            }
            onPointerUp={
              isRealtimeMode
                ? undefined
                : (e) => {
                    if (e.button !== 0) return;
                    e.preventDefault();
                    stopRecording();
                  }
            }
            onPointerLeave={isRealtimeMode ? undefined : stopRecording}
            onPointerCancel={isRealtimeMode ? undefined : stopRecording}
            onContextMenu={(e) => e.preventDefault()}
          >
            🎤
            {(isRealtimeMode ? realtimeActive : recording) && <span className="mic-indicator" aria-hidden="true" />}
          </button>
          <button id="send-btn" type="button" onClick={() => submit(activeStreaming ? 'steer' : 'prompt')}>
            {activeStreaming ? 'Steer' : 'Send'}
          </button>
          {activeStreaming && (
            <button id="abort-btn" type="button" title="Abort the active run (Esc)" onClick={() => {
              if (sessionId) abortPendingBySessionIdRef.current[sessionId] = true;
              sendCommand({ type: 'abort' }).catch(() => {});
            }}>
              Abort
            </button>
          )}
        </div>
      </footer>

      <input
        ref={fileInputRef}
        type="file"
        accept="image/*,application/pdf"
        multiple
        hidden
        onChange={(e) => {
          onFilesChosen(e.target.files);
          e.target.value = '';
        }}
      />

        </div>
      </div>

      {/* Remote audio for the realtime call: ontrack wires the WebRTC stream
          into this hidden element (play() is called on the click gesture). */}
      <audio ref={realtimeAudioRef} autoPlay playsInline hidden />

      <ToastList
        toasts={toasts}
        dismiss={(id) => setToasts((prev) => prev.filter((t) => t.id !== id))}
      />
    </>
  );
}
