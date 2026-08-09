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

const RPI_AUTH_PREFIX = 'rpi-auth.';
const TOKEN_STORAGE_KEY = 'rpi-web-token';
const RECONNECT_MAX_DELAY = 15000;
const COMMAND_TIMEOUT_MS = 30000;

type ConnState = 'off' | 'connecting' | 'on' | 'reconnecting';

export type Item =
  | { kind: 'user'; id: string; text: string; optimistic: boolean }
  | { kind: 'assistant'; id: string; status: 'streaming' | 'final'; blocks: ContentBlock[] }
  | { kind: 'toolCard'; id: string; toolCallId: string; toolName: string; args: unknown; status: 'running' | 'done' | 'error'; result: string }
  | { kind: 'toolResult'; id: string; text: string }
  | { kind: 'bash'; id: string; command: string; output: string }
  // Custom display:true backend messages (loops, projected notices).
  | { kind: 'custom'; id: string; label: string; text: string }
  // branchSummary / compactionSummary backend messages (system notices).
  | { kind: 'summary'; id: string; label: string; text: string }
  | { kind: 'approval'; id: string; method: string; title: string; message: string; extensionId?: string };

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

export function nextId(prefix: string): string {
  return `${prefix}${Date.now().toString(36)}${Math.random().toString(36).slice(2, 8)}`;
}

export function contentText(content: unknown): string {
  if (!Array.isArray(content)) return '';
  return content
    .filter((b) => b && b.type === 'text' && typeof b.text === 'string')
    .map((b) => b.text as string)
    .join('\n');
}

/** Convert backend-authoritative lifecycle messages (Vec<Message> with
 *  role/content blocks) into renderable Items, reusing the same role/content
 *  rules as the live event stream. Deterministic per message.
 *
 *  Covers every `pi_ai::Message` role: user, assistant (text/thinking/image/
 *  toolCall blocks), toolResult, bashExecution, custom (rendered only when
 *  `display: true` — hidden internal messages never render), branchSummary
 *  and compactionSummary (system notices). Unknown/malformed records are
 *  skipped defensively; one bad record never breaks the transcript restore. */
export function messagesToItems(messages: unknown): Item[] {
  const list = Array.isArray(messages) ? messages : [];
  const out: Item[] = [];
  for (const raw of list) {
    try {
      const m = (raw || {}) as {
        role?: string;
        content?: unknown;
        display?: boolean;
        customType?: string;
        summary?: string;
        command?: string;
        output?: string;
      };
      switch (m.role) {
        case 'user':
          out.push({ kind: 'user', id: nextId('u'), text: contentText(m.content), optimistic: false });
          break;
        case 'assistant':
          out.push({
            kind: 'assistant',
            id: nextId('a'),
            status: 'final',
            blocks: (Array.isArray(m.content) ? m.content : []) as ContentBlock[],
          });
          break;
        case 'toolResult':
          out.push({ kind: 'toolResult', id: nextId('r'), text: contentText(m.content) });
          break;
        case 'bashExecution': {
          const cmd = (m.content || {}) as { command?: string; output?: string };
          out.push({ kind: 'bash', id: nextId('b'), command: cmd.command || '', output: cmd.output || '' });
          break;
        }
        case 'custom':
          // display:false custom messages are internal (loop scheduling,
          // orchestration IRC) and NEVER render; display:true ones surface as
          // a labeled notice card, mirroring the TUI's push_message rules.
          if (m.display === true) {
            out.push({
              kind: 'custom',
              id: nextId('c'),
              label: typeof m.customType === 'string' ? m.customType : 'notice',
              text: contentText(m.content),
            });
          }
          break;
        case 'branchSummary':
          out.push({
            kind: 'summary',
            id: nextId('s'),
            label: 'Branch summary',
            text: typeof m.summary === 'string' ? m.summary : '',
          });
          break;
        case 'compactionSummary':
          out.push({
            kind: 'summary',
            id: nextId('s'),
            label: 'Compaction summary',
            text: typeof m.summary === 'string' ? m.summary : '',
          });
          break;
        default:
          break; // unknown roles are ignored safely
      }
    } catch {
      /* one malformed record never breaks the whole transcript restore */
    }
  }
  return out;
}

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

  const wsRef = useRef<WebSocket | null>(null);
  const pendingRef = useRef(new Map<string, Pending>());
  const seqRef = useRef(0);
  const delayRef = useRef(1000);
  const retryTimerRef = useRef<number | null>(null);
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
      toast(`command ${frame.command} failed: ${frame.error || 'unknown error'}`, true);
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

  const onOpen = useCallback(() => {
    delayRef.current = 1000;
    setConnState('on');
    sendCommand({ type: 'get_state' })
      .then((data) => {
        applyState(data);
        // Goal snapshot + journal for the now-known session (the sid is
        // derived from the state response, so the refresh targets the right
        // runtime even on the very first connect).
        refreshGoal(sessionIdRef.current);
      })
      .catch(() => {});
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
  }, [applyState, refreshGoal, sendCommand]);

  const connect = useCallback(() => {
    if (retryTimerRef.current !== null) {
      window.clearTimeout(retryTimerRef.current);
      retryTimerRef.current = null;
    }
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
    let tokenValue = '';
    const tokenInput = document.getElementById('token-input') as HTMLInputElement | null;
    if (tokenInput) tokenValue = tokenInput.value.trim();
    setToken(tokenValue);
    try {
      window.sessionStorage.setItem(TOKEN_STORAGE_KEY, tokenValue);
    } catch {
      /* private mode: token lives in the field only */
    }
    setConnState('connecting');
    const protocols = tokenValue ? [`${RPI_AUTH_PREFIX}${tokenValue}`] : [];
    let ws: WebSocket;
    try {
      const scheme = location.protocol === 'https:' ? 'wss://' : 'ws://';
      ws = new WebSocket(`${scheme}${location.host}/ws`, protocols);
    } catch (err) {
      setConnState('off');
      toast(`cannot open WebSocket: ${(err as Error).message} — token must be a single token without spaces`, true);
      scheduleReconnect();
      return;
    }
    wsRef.current = ws;
    ws.onopen = onOpen;
    ws.onmessage = (event) => onMessage(event.data as string);
    ws.onclose = (event) => {
      if (event.target !== wsRef.current) return; // superseded by a newer socket
      wsRef.current = null;
      setConnState('off');
      if (event.code === 1006) {
        // The boot auto-connect with no token is an expected probe on a fresh
        // page load (the user has not typed anything yet) — stay quiet and
        // let the empty-hint explain the requirement.
        if (!(bootProbeRef.current && !tokenValue)) {
          toast('connection failed (wrong or missing token?). Enter the token and press Connect.', true);
        }
      } else if (event.code !== 1000) {
        toast(`connection closed (code ${event.code})${event.reason ? `: ${event.reason}` : ''}`, true);
      }
      scheduleReconnect();
    };
    ws.onerror = () => {
      /* the close event carries the failure */
    };
  }, [onOpen, scheduleReconnect, toast]);

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
      pushItemFor(sid, { kind: 'toolResult', id: nextId('r'), text: contentText(message.content) });
      return;
    }
    if (role === 'bashExecution') {
      const m = message as { command?: string; output?: string };
      pushItemFor(sid, { kind: 'bash', id: nextId('b'), command: m.command || '', output: m.output || '' });
    }
  }, [frameSession, patchItemFor, pushItemFor]);

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
    updateItemsFor(frameSession(frame), (prev) =>
      prev.map((item) =>
        item.kind === 'toolCard' && item.toolCallId === toolCallId
          ? { ...item, result: item.result === '' ? text : `${item.result}\n${text}` }
          : item
      )
    );
  }, [frameSession, updateItemsFor]);

  const onToolEnd = useCallback((frame: EventFrame) => {
    const toolCallId = (frame.toolCallId as string) || '';
    const text = contentText((frame.result as { content?: unknown } | undefined)?.content);
    const isError = !!frame.isError;
    updateItemsFor(frameSession(frame), (prev) =>
      prev.map((item) =>
        item.kind === 'toolCard' && item.toolCallId === toolCallId
          ? { ...item, status: isError ? 'error' : 'done', result: text }
          : item
      )
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

  const submit = useCallback((kind: 'prompt' | 'steer' | 'followup') => {
    const input = promptInputRef.current;
    if (!input) return;
    const text = input.value.trim();
    if (!text) return;
    const bubbleId = nextId('u');
    const sid = sessionIdRef.current;
    pushItemFor(sid, { kind: 'user', id: bubbleId, text, optimistic: true });
    if (sid) (optimisticQueueBySessionIdRef.current[sid] ??= []).push(bubbleId);
    input.value = '';
    autoResize(input);
    const command =
      kind === 'steer'
        ? { type: 'steer', message: text }
        : kind === 'followup'
          ? { type: 'follow_up', message: text }
          : { type: 'prompt', message: text };
    sendCommand(command, bubbleId).catch((err: Error & { rpc?: boolean }) => {
      if (!err.rpc) toast(`send failed: ${err.message}`, true);
    });
  }, [pushItemFor, sendCommand, toast]);

  const autoResize = useCallback((input: HTMLTextAreaElement) => {
    input.style.height = 'auto';
    input.style.height = `${Math.min(input.scrollHeight, 180)}px`;
  }, []);

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
      saved = window.sessionStorage.getItem(TOKEN_STORAGE_KEY) || '';
    } catch {
      /* private mode */
    }
    if (saved) {
      setToken(saved);
      const input = document.getElementById('token-input') as HTMLInputElement | null;
      if (input) input.value = saved;
    }
    connect();
    return () => {
      if (retryTimerRef.current !== null) window.clearTimeout(retryTimerRef.current);
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
          id="token-input"
          type="password"
          placeholder="auth token (rpi-auth.<token>)"
          autoComplete="off"
          spellCheck={false}
          defaultValue={token}
          onChange={() => {
            bootProbeRef.current = false;
          }}
          onKeyDown={(e) => {
            if (e.key === 'Enter') {
              bootProbeRef.current = false;
              connect();
            }
          }}
        />
        <button id="connect-btn" type="button" onClick={() => {
          bootProbeRef.current = false;
          connect();
        }}>
          Connect
        </button>
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
            Connect to the control plane, then send a prompt.
            <br />
            Requires <code>rpi --listen &lt;addr&gt; --listen-token-file &lt;token&gt;</code> — browsers
            always send an Origin header, so a tokenless loopback listener deliberately refuses browser
            connections. Enter the token above to authenticate.
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
                <div key={item.id} className="msg msg--bash">
                  <pre className="bash-output">{item.text === '' ? '(empty tool result)' : item.text}</pre>
                </div>
              );
            case 'bash':
              return (
                <div key={item.id} className="msg msg--bash">
                  <div className="bash-cmd">$ {item.command}</div>
                  <pre className="bash-output">{item.output}</pre>
                </div>
              );
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
          onClose={() => setActivePanel('')}
        />
      )}

      <footer>
        <textarea
          id="prompt-input"
          ref={promptInputRef}
          rows={3}
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
        <div id="composer-buttons">
          <button id="send-btn" type="button" onClick={() => submit(activeStreaming ? 'steer' : 'prompt')}>
            {activeStreaming ? 'Steer' : 'Send'}
          </button>
          <button id="steer-btn" type="button" title="Queue a steering message for the active run" onClick={() => submit('steer')}>
            Steer
          </button>
          <button id="followup-btn" type="button" title="Queue a follow-up message for the active run" onClick={() => submit('followup')}>
            Follow up
          </button>
          <button id="abort-btn" type="button" disabled={!activeStreaming} title="Abort the active run (Esc)" onClick={() => {
            if (sessionId) abortPendingBySessionIdRef.current[sessionId] = true;
            sendCommand({ type: 'abort' }).catch(() => {});
          }}>
            Abort
          </button>
        </div>
      </footer>

        </div>
      </div>

      <ToastList
        toasts={toasts}
        dismiss={(id) => setToasts((prev) => prev.filter((t) => t.id !== id))}
      />
    </>
  );
}
