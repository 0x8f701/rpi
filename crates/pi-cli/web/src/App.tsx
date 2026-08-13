import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type KeyboardEvent, type PointerEvent } from 'react';
import { useScrollPin } from './scrollPin';
import { createAutoResizeController, type AutoResizeController } from './autoResize';
import { redactSecrets, safeText } from './redact';
import { ansiToPlainText } from './ansi';
import { AnsiText } from './AnsiText';
import { humanToolTitle } from './toolTitle';
import { withResolvers } from './withResolvers';
import { renderBlocks, hydrateMermaid, thinkingSummaryHtml } from './markdown';
import { MarkdownBody } from './MarkdownBody';
import { TodoPanel } from './panels/TodoPanel';
import type { TodoPhase as TodoPhaseWire } from './panels/TodoPanel';
import { SubagentsPanel } from './panels/SubagentsPanel';
import { PersonasPanel } from './panels/PersonasPanel';
import { GoalPanel, type GoalEventWire, type GoalStateWire } from './panels/GoalPanel';
import { WorkflowPanel, dispatchWorkflowEvents } from './panels/WorkflowPanel';
import { SideChatPanel } from './panels/SideChatPanel';
import type { SideChatSnapshot } from './panels/SideChatPanel';
import { SessionPanel } from './panels/SessionPanel';
import { SettingsPanel } from './panels/SettingsPanel';
import { SessionSidebar } from './panels/SessionSidebar';
import { CodeReviewPanel, type CodeReviewOpenArgs } from './panels/CodeReviewPanel';
import type { ContentBlock, EventFrame, RpcResponse } from './types';
import {
  type Item,
  nextId,
  contentText,
  userMessageProjection,
  messagesToItems,
  mergeAuthoritativeItems,
  boundOutput,
  customToItem,
  applyToolSnapshot,
  applyToolResultToItems,
  toolMedia,
  applyJobUpdated,
  applyAgentUpdated,
  applyMessageDelivered,
  resolveTaskCardView,
  resolveTodoCardView,
  parseEditCard,
  compactToolArgs,
  parseCommandCardArgs,
  parseProcessCardArgs,
  parseWriteCardArgs,
  parseReadCardArgs,
  parseHubCard,
  ircTitle,
  IRC_COMPACT_LINE_LIMIT,
  diffLines,
  shouldRestoreStreamingAssistant,
  finalizeStreamingAssistant,
  shouldNormalizeThinkingNewlines,
  normalizeThinkingNewlines,
  unescapeThinkingNewlines,
  BASH_OUTPUT_LINE_LIMIT,
  TOOL_OUTPUT_LINE_LIMIT,
} from './transcript';
import {
  firstString,
  buildRealtimeSessionConfig,
  setupRealtimeCall,
  isRealtimeLiveMode,
  classifyInputTranscriptEvent,
  finalTranscriptText,
  nextInputTranscriptCommit,
  realtimeErrorMessage,
  classifyRealtimeConnectionState,
} from './realtime';
import {
  STT_AUTO_RELEASE_MS,
  STT_MAX_WAV_BYTES,
  STT_SAMPLE_RATE,
  STT_WAV_MIME,
  encodeWavPcm16,
  resampleToSttRate,
  wavToBase64,
} from './stt';
import { STALE_ABORT, isCurrentPending, shouldScheduleReconnect, detachTransportHandlers, ReadyGate } from './socket';
import { PendingRegistry } from './pending';
import {
  loadActiveHost,
  loadInitialAuthorityToken,
  loadTokenForAuthority,
  saveActiveHost,
  saveTokenForAuthority,
  type StorageLike,
} from './hostToken';
import {
  loadSessionPreference,
  saveSessionPreference,
  selectSessionFromCatalog,
  type SessionPreferenceRow,
} from './sessionPreference';
import { routeCommandSession, goalGetCommand, goalJournalCommand } from './goal';
import {
  normalizeCommands,
  filterCommands,
  filterSupportedCommands,
  composeCommandText,
  composeSkillCommandText,
  appendDraft,
  parseSupportedCommand,
  isSkillCandidate,
  primaryCommands,
  skillCandidates,
  pickerIntentFromDraft,
  type CommandEntry,
} from './commands';
import {
  formatCompactReport,
  formatSkillResult,
  resolveSlashAction,
} from './slashDispatch';

const RPI_AUTH_PREFIX = 'rpi-auth.';
// localStorage may throw on access in private mode / blocked cookies; resolve
// a safe backend once. Tokens and session preferences remain listener-scoped;
// the selected listener is restored before the first bootstrap.
const tokenStorage: StorageLike | null = (() => {
  try {
    return typeof window !== 'undefined' && window && window.localStorage ? window.localStorage : null;
  } catch {
    return null;
  }
})();
const pageAuthority = (() => {
  try {
    return typeof window !== 'undefined' && window.location ? window.location.host : '';
  } catch {
    return '';
  }
})();
const initialHostAuthority = loadActiveHost(tokenStorage, pageAuthority);
const RECENT_HOSTS_STORAGE_KEY = 'rpi-web-recent-hosts';
const RECENT_HOSTS_MAX = 10;
const RECONNECT_INITIAL_DELAY = 1000;
const RECONNECT_MAX_DELAY = 15000;
// Heartbeat: a `{type:"ping"}` JSON frame every 30s keeps the connection
// honest. The backend replies to every text frame (unknown types get an error
// `response` frame, which the RPC plumbing ignores for lack of a pending id),
// so a probe reliably produces an inbound message. If no message of ANY kind
// arrives for 60s the socket is presumed dead — silent drops never fire
// onclose — and is proactively closed so the existing reconnect path takes
// over. The reconnect backoff resets only after the connection has stayed up
// for >5s, so flapping connections keep backing off.
//
// Transport-stale fast-ack timeout: a fast-ack command's 30s pending timer
// (DEFAULT_COMMAND_TIMEOUT_MS, see ./pending.ts) fires BEFORE the 60s
// liveness timer when the server swallows the outbound prompt while the
// socket stays OPEN. onTransportStale closes the socket with this code so the
// existing onclose path drains remaining pending and schedules a reconnect;
// onclose skips its generic close toast for this code because the
// transport-stale hook already toasted a truthful "connection unresponsive,
// reconnecting" message. Distinct from HEARTBEAT's 4000 so the two proactive
// close paths keep their own user-facing message.
const HEARTBEAT_PING_INTERVAL_MS = 30000;
const HEARTBEAT_TIMEOUT_MS = 60000;
const HEARTBEAT_STABILITY_MS = 5000;
const TRANSPORT_STALE_CLOSE_CODE = 4001;

/* ------------------------------------------------------------------ *
 * Shared bottom-drawer height for ordinary `.panel` mounts
 * ------------------------------------------------------------------ *
 * Desktop only: one resizer + one localStorage key for every ordinary
 * side panel (todo/goal/workflow/session/settings/subagents/personas/
 * sidechat). Code review owns the full viewport and is
 * excluded. Mobile never mounts the resizer. */
const PANEL_DRAWER_SIZE_KEY = 'rpi-panel-drawer-size';
const PANEL_DRAWER_MIN_VH = 25;
const PANEL_DRAWER_MAX_VH = 90;
const PANEL_DRAWER_DEFAULT_VH = 90;

function clampPanelDrawerVh(vh: number): number {
  if (!Number.isFinite(vh)) return PANEL_DRAWER_DEFAULT_VH;
  return Math.min(PANEL_DRAWER_MAX_VH, Math.max(PANEL_DRAWER_MIN_VH, vh));
}

function readStoredPanelDrawerVh(): number {
  try {
    const raw = window.localStorage.getItem(PANEL_DRAWER_SIZE_KEY);
    if (raw == null || raw === '') return PANEL_DRAWER_DEFAULT_VH;
    const n = Number(raw);
    if (!Number.isFinite(n)) return PANEL_DRAWER_DEFAULT_VH;
    return clampPanelDrawerVh(n);
  } catch {
    return PANEL_DRAWER_DEFAULT_VH;
  }
}

function writeStoredPanelDrawerVh(vh: number): void {
  try {
    window.localStorage.setItem(PANEL_DRAWER_SIZE_KEY, String(clampPanelDrawerVh(vh)));
  } catch {
    /* private mode / blocked storage: height lives in CSS only */
  }
}

function applyPanelDrawerVh(vh: number): void {
  document.documentElement.style.setProperty(
    '--panel-drawer-height',
    `${clampPanelDrawerVh(vh)}vh`,
  );
}

type ConnState = 'off' | 'connecting' | 'on' | 'reconnecting';

// The Item shape, nextId/contentText helpers, and messagesToItems live in
// ./transcript (shared with CollabGuestView) and are re-exported here so
// existing `import { … } from './App'` callers keep compiling.
export { nextId, contentText, messagesToItems, type Item } from './transcript';

export interface LiveNodes {
  textEl: HTMLDivElement | null;
  thinkingDetails: HTMLDetailsElement | null;
  thinkingBody: HTMLDivElement | null;
}

export interface StreamBuffer {
  text: string;
  thinking: string;
}

// Module-level registries for the hot streaming path: deltas are appended to
// the mounted DOM node directly (no React re-render per chunk), with the
// buffer as the catch-up for deltas that arrive before the node mounts.
// Both are keyed by `streamKey(sessionId, itemId)`: equal item ids in
// different sessions can never cross-wire, and a session cutover mid-stream
// cannot route one session's deltas into another session's node.
export const liveNodes = new Map<string, LiveNodes>();
export const streamBuf = new Map<string, StreamBuffer>();

// Stream keys whose thinking body already shows full-buffer-normalized text
// (escaped-newline content re-rendered from the buffer once it qualified via
// shouldNormalizeThinkingNewlines). Subsequent deltas keep the normalized
// form so the DOM never regresses to a literal `\n` mid-stream.
export const normalizedThinkingKeys = new Set<string>();

export function streamKey(sid: string | null, itemId: string): string {
  return `${sid ?? ''}\u0000${itemId}`;
}

/** Append one streaming delta to the mounted live node for `(sid, itemId)`
 *  via direct DOM mutation (textContent), bypassing React per-chunk renders.
 *  Shared by the session transcript and the collab guest view — which never
 *  coexist — so the module-level `liveNodes` registry is never cross-wired. */
/** Append a delta to a live streaming text container. `textContent +=` is
 *  quadratic in the number of chunks (every assignment copies the whole
 *  accumulated string); appending through the existing text node's appendData
 *  is in-place and O(delta), so a long stream (tens of thousands of chunks)
 *  can never stall the renderer on string copies while the socket floods. */
function appendDeltaText(el: HTMLElement, safe: string): void {
  const first = el.firstChild;
  if (first && first.nodeType === Node.TEXT_NODE) {
    (first as Text).appendData(safe);
  } else {
    el.textContent += safe;
  }
}

export function applyDeltaToNode(
  sid: string | null,
  itemId: string,
  delta: string,
  kind: 'text' | 'thinking',
): void {
  const key = streamKey(sid, itemId);
  const node = liveNodes.get(key);
  if (!node) return;
  if (kind === 'text' && node.textEl) {
    appendDeltaText(node.textEl, safeText(delta));
  } else if (kind === 'thinking' && node.thinkingBody && node.thinkingDetails) {
    node.thinkingDetails.hidden = false;
    // Escaped-newline normalization is a whole-text decision, so it reads the
    // accumulated buffer (the caller appends to it BEFORE routing the delta
    // here). Once the buffer qualifies (no real newlines + multiple literal
    // `\n`), the body is re-rendered from the normalized buffer once and the
    // key is marked so later deltas append in the same normalized form —
    // keeping O(delta) appends for the rest of the stream.
    const buf = streamBuf.get(key);
    if (buf && shouldNormalizeThinkingNewlines(buf.thinking)) {
      node.thinkingBody.textContent = normalizeThinkingNewlines(buf.thinking);
      normalizedThinkingKeys.add(key);
    } else if (normalizedThinkingKeys.has(key)) {
      appendDeltaText(node.thinkingBody, safeText(unescapeThinkingNewlines(delta)));
    } else {
      appendDeltaText(node.thinkingBody, safeText(delta));
    }
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

  useEffect(() => {
    const key = streamKey(sid, id);
    const entry: LiveNodes = {
      textEl: textRef.current,
      thinkingDetails: thinkingRef.current,
      thinkingBody: thinkingBodyRef.current,
    };
    liveNodes.set(key, entry);
    const buf = streamBuf.get(key);
    if (buf) {
      if (buf.text && textRef.current) textRef.current.textContent = buf.text;
      if (buf.thinking && thinkingBodyRef.current && thinkingRef.current) {
        thinkingRef.current.hidden = false;
        // Late-mount catch-up writes the WHOLE buffer at once, so the same
        // conservative normalization the live delta path applies fits here.
        thinkingBodyRef.current.textContent = normalizeThinkingNewlines(buf.thinking);
      }
    }
    return () => {
      liveNodes.delete(key);
    };
  }, [sid, id]);

  return (
    <div className="msg msg--assistant">
      {/* `open` = thinking body visible by default while streaming, still
          collapsible via the summary; `hidden` keeps the whole block away
          until the first thinking delta arrives. */}
      <details className="thinking" open hidden ref={thinkingRef}>
        {/* Shared React-free header (markdown.ts): brain icon + Thinking —
            identical for the live stream and restored/final rendering. */}
        <summary className="thinking__summary" dangerouslySetInnerHTML={{ __html: thinkingSummaryHtml() }} />
        <div className="thinking__body" ref={thinkingBodyRef} />
      </details>
      <div className="assistant-text" ref={textRef} />
    </div>
  );
}

export function FinalAssistant({ blocks, onLayoutChange }: { blocks: ContentBlock[]; onLayoutChange?: () => void }) {
  const textRef = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => {
    const node = textRef.current;
    if (!node) return;
    // Commit-time pin, BEFORE paint: the streaming->final render replaces
    // the streamed text with the rendered blocks, which can transiently
    // shrink scrollHeight and let the browser clamp scrollTop mid-content.
    // Re-pinning in the same layout pass (before the clamped position is
    // painted) keeps every painted frame glued to the bottom; the parent's
    // post-paint passive effect would land one frame late.
    onLayoutChange?.();
    // Mermaid fences render asynchronously: hydrate the hosts after
    // dangerouslySetInnerHTML commits. Scoped to the final text node so the
    // streaming path (textContent deltas) is never touched. `onLayoutChange`
    // is invoked synchronously after each host mutation so the transcript
    // pin re-glues before the next frame. Optional callers without a pin
    // controller keep the previous behavior.
    void hydrateMermaid(node, onLayoutChange);
    // Final-block images (markdown `![..](..)` and image content blocks)
    // decode asynchronously and grow layout with no React commit and no
    // delta. A scoped capture load handler re-pins synchronously with the
    // decode; the pin controller preserves a deliberate user scroll-away
    // (unpinned freeze). Data-URL decodes can race ahead of this layout
    // effect, so a complete image pins once immediately instead of waiting
    // for a load event that already fired.
    const imgs = Array.from(node.querySelectorAll<HTMLImageElement>('img.md-image'));
    const listeners: Array<[HTMLImageElement, () => void]> = [];
    for (const img of imgs) {
      if (img.complete) {
        onLayoutChange?.();
        continue;
      }
      const onLoad = () => onLayoutChange?.();
      img.addEventListener('load', onLoad, { capture: true, once: true });
      listeners.push([img, onLoad]);
    }
    if (listeners.length === 0) return;
    return () => {
      for (const [img, onLoad] of listeners) img.removeEventListener('load', onLoad, { capture: true });
    };
  }, [onLayoutChange]);
  return (
    <div className="msg msg--assistant">
      <div className="assistant-text" ref={textRef} dangerouslySetInnerHTML={{ __html: renderBlocks(blocks.filter((b) => b && b.type !== 'thinking')) }} />
    </div>
  );
}

// TUI Todo-DAG task markers (crates/pi-cli/src/todo_dag_view.rs `task_marker`):
// ○ pending, ● in_progress, ✓ completed, × abandoned. Status is conveyed by
// the marker glyph + color; the in-progress task additionally gets the
// --active row highlight.
const TODO_TASK_MARKERS: Record<string, string> = {
  pending: '\u25CB',
  in_progress: '\u25CF',
  completed: '\u2713',
  abandoned: '\u00D7',
};

/** Shared typed orchestration IRC card (host App.tsx and CollabGuestView
 *  render the SAME component so the transcripts can never drift). Title =
 *  direction arrow + party (`IRC ← sender` / `IRC → recipient`, mirroring the
 *  TUI's `OrchestrationIrcView::label`); the body renders through the SHARED
 *  MarkdownBody pipeline (escapeHtml-first, whitelisted links/images, code
 *  fences, Mermaid hosts hydrated after commit) on the visual quote rail —
 *  never as pre-wrapped raw text, so headings/lists/inline code display
 *  structurally and hostile HTML stays literal; replyTo is an independent
 *  muted line (never embedded in the body); compact clamps to 6 visual lines
 *  with an expand toggle up to the 40-line parse bound. Mermaid/image
 *  mutations re-pin via onLayoutChange (same contract as FinalAssistant). */
export function IrcCard({ item, onLayoutChange }: {
  item: Extract<Item, { kind: 'irc' }>;
  onLayoutChange?: () => void;
}) {
  const [expanded, setExpanded] = useState(false);
  const bodyRef = useRef<HTMLDivElement>(null);
  const [overflows, setOverflows] = useState(false);
  useEffect(() => {
    const el = bodyRef.current;
    if (!el) return;
    // The compact clamp hides overflow (scrollHeight > clientHeight); a body
    // that fits fully (or a single long wrapped line) still gets a toggle
    // when it visually overflows.
    const check = () => setOverflows(el.scrollHeight > el.clientHeight + 1);
    check();
    const ro = new ResizeObserver(check);
    ro.observe(el);
    return () => ro.disconnect();
  }, [item.body]);
  const manyLines = item.body.split('\n').length > IRC_COMPACT_LINE_LIMIT;
  const showToggle = manyLines || overflows;
  return (
    <div
      className={`msg msg--irc msg--irc--${item.direction}`}
      data-irc-direction={item.direction}
      data-irc-from={item.from}
      data-irc-to={item.to}
      role="note"
    >
      <div className="msg--irc__head">
        <span className="msg--irc__title">{safeText(ircTitle(item.from, item.to))}</span>
        {item.replyTo ? (
          <span className="msg--irc__reply">reply to {safeText(item.replyTo)}</span>
        ) : null}
      </div>
      <MarkdownBody
        bodyRef={bodyRef}
        className={`msg--irc__body${expanded ? ' is-expanded' : ''}`}
        text={item.body}
        onLayoutChange={onLayoutChange}
      />
      {showToggle && (
        <button
          type="button"
          className="msg--irc__toggle"
          aria-expanded={expanded}
          onClick={() => setExpanded((value) => !value)}
        >
          {expanded ? 'collapse' : 'expand'}
        </button>
      )}
    </div>
  );
}

/** Hub tool-card Markdown fold: wait/send bodies and fallback notes render
 *  through the SHARED MarkdownBody component (escapeHtml-first, whitelisted
 *  links/images, code fences, Mermaid hosts hydrated after commit) — never
 *  as raw text, so Markdown bullets/inline code/path lists display
 *  structurally and hostile HTML stays literal. Compact clamps to
 *  IRC_COMPACT_LINE_LIMIT visual lines with an expand toggle up to the
 *  parse-time bound, mirroring the IRC card fold. Mermaid/image mutations
 *  re-pin via onLayoutChange (same contract as FinalAssistant). */
function HubMarkdownFold({ text, className, onLayoutChange }: {
  text: string;
  className?: string;
  onLayoutChange?: () => void;
}) {
  const [expanded, setExpanded] = useState(false);
  const bodyRef = useRef<HTMLDivElement>(null);
  const [overflows, setOverflows] = useState(false);
  useEffect(() => {
    const el = bodyRef.current;
    if (!el) return;
    // The compact clamp hides overflow (scrollHeight > clientHeight); content
    // that fits fully (or a single long wrapped line) still gets a toggle
    // when it visually overflows.
    const check = () => setOverflows(el.scrollHeight > el.clientHeight + 1);
    check();
    const ro = new ResizeObserver(check);
    ro.observe(el);
    return () => ro.disconnect();
  }, [text]);
  const manyLines = text.split('\n').length > IRC_COMPACT_LINE_LIMIT;
  const showToggle = manyLines || overflows;
  return (
    <div className={className ?? ''}>
      <MarkdownBody
        bodyRef={bodyRef}
        className={`tool-card__hub-md${expanded ? ' is-expanded' : ''}`}
        text={text}
        onLayoutChange={onLayoutChange}
      />
      {showToggle && (
        <button
          type="button"
          className="tool-card__hub-toggle"
          aria-expanded={expanded}
          onClick={() => setExpanded((value) => !value)}
        >
          {expanded ? 'collapse' : 'expand'}
        </button>
      )}
    </div>
  );
}

export function ToolCard({ item, onLayoutChange }: { item: Extract<Item, { kind: 'toolCard' }>; onLayoutChange?: () => void }) {
  const toolName = item.toolName.toLowerCase();
  const taskView = toolName === 'task' ? resolveTaskCardView(item.args, item.details) : null;
  const todoView = toolName === 'todo' ? resolveTodoCardView(item.args, item.details, item.result, item.status) : null;
  const editView = toolName === 'edit' ? parseEditCard(item.args, item.details, item.result) : null;
  const commandView = toolName === 'bash' ? parseCommandCardArgs(item.args) : null;
  const processView = toolName === 'process' ? parseProcessCardArgs(item.args) : null;
  const writeView = toolName === 'write' ? parseWriteCardArgs(item.args, item.result, item.status) : null;
  const readView = toolName === 'read' ? parseReadCardArgs(item.args) : null;
  const hubView = toolName === 'hub' ? parseHubCard(item.args, item.details, item.result, item.status) : null;
  const variant = todoView ? 'todo' : taskView ? 'task' : editView ? 'edit' : commandView ? 'command'
    : processView ? 'process' : writeView ? 'write' : readView ? 'read' : hubView ? 'hub' : 'generic';
  const statusClass = item.status === 'error' ? ' tool-card--error'
    : item.status === 'done' ? ' tool-card--done' : '';
  const isStructuredVariant = variant === 'task' || variant === 'edit';
  const isTodo = variant === 'todo';
  // Todo/command/process/write/read convey status by border color only (the
  // presentation lane forbids a visible "done"/"error" label); task/edit keep
  // their structured state label; every card labels its running state.
  const showStateLabel = isTodo
    ? item.status === 'running'
    : isStructuredVariant || item.status === 'running';
  const stateLabel = item.status === 'running' ? 'running\u2026' : item.status;
  const cardTitle = variant === 'todo' ? 'Todo'
    : variant === 'command' ? 'Command'
    : variant === 'process' ? 'Process'
    : variant === 'write' ? 'Write'
    : variant === 'read' ? 'Read'
    : hubView ? hubView.title
    : safeText(humanToolTitle(item.toolName));
  const titleClass = isStructuredVariant || isTodo ? 'tool-card__name' : 'tool-card__title';
  const media = item.media ?? [];
  return (
    <div
      className={`tool-card tool-card--${variant}${statusClass}`}
      data-tool-id={item.toolCallId}
      data-tool-name={item.toolName}
      data-tool-status={item.status}
    >
      <div className="tool-card__head">
        <span className={titleClass}>{cardTitle}</span>
        {showStateLabel && (
          <span className={`tool-card__state tool-card__state--${item.status}`}>{stateLabel}</span>
        )}
      </div>
      {taskView ? (
        <div className="tool-card__task">
          {taskView.sections.goal !== '' && (
            <div className="tool-card__section">
              <div className="tool-card__section-label">Goal</div>
              <div className="tool-card__section-body">{safeText(taskView.sections.goal)}</div>
            </div>
          )}
          {taskView.sections.constraints !== '' && (
            <div className="tool-card__section">
              <div className="tool-card__section-label">Constraints</div>
              <div className="tool-card__section-body">{safeText(taskView.sections.constraints)}</div>
            </div>
          )}
          {taskView.sections.contract !== '' && (
            <div className="tool-card__section">
              <div className="tool-card__section-label">Contract</div>
              <div className="tool-card__section-body">{safeText(taskView.sections.contract)}</div>
            </div>
          )}
          <div className="tool-card__children">
            {taskView.children.map((child, index) => (
              <div
                key={child.jobId || child.agentId || `${child.name}-${index}`}
                className="tool-card__child"
                data-job-id={child.jobId || undefined}
                data-agent-id={child.agentId || undefined}
                data-status={child.status}
              >
                <div className="tool-card__child-head">
                  <span className="tool-card__child-name">{safeText(child.name)}</span>
                  <span className="tool-card__child-agent">{safeText(child.agent)}</span>
                  <span className="tool-card__child-status">{safeText(child.status)}</span>
                </div>
                <div className="tool-card__child-target">{safeText(child.target)}</div>
                {child.activity ? (
                  <div className="tool-card__child-activity">{safeText(child.activity)}</div>
                ) : null}
                {child.result ? (
                  <div className="tool-card__child-result">{safeText(child.result)}</div>
                ) : null}
              </div>
            ))}
          </div>
          <details className="tool-card__raw">
            <summary>raw args</summary>
            <pre className="tool-card__args">{safeText(JSON.stringify(item.args, null, 2))}</pre>
          </details>
        </div>
      ) : editView ? (
        <div className="tool-card__edit">
          <div className="tool-card__edit-meta">
            <span className="tool-card__edit-path">{safeText(editView.path)}</span>
            <span className="tool-card__edit-op">{safeText(editView.operation)}</span>
          </div>
          {editView.diff !== '' && (
            <pre className="tool-card__diff">
              {diffLines(editView.diff).map((line, index) => (
                <span key={`${index}-${line.kind}`} className={`tool-card__diff-line tool-card__diff-line--${line.kind}`}>
                  {safeText(line.text)}
                  {'\n'}
                </span>
              ))}
            </pre>
          )}
          <details className="tool-card__raw">
            <summary>raw args / details</summary>
            <pre className="tool-card__args">{safeText(JSON.stringify({ args: item.args, details: item.details }, null, 2))}</pre>
          </details>
        </div>
      ) : commandView ? (
        <div className="tool-card__command">
          <div className="tool-card__command-line">
            <span className="tool-card__prompt">$</span>
            <span className="tool-card__command-text">{safeText(commandView.command)}</span>
          </div>
          {item.result !== '' && <pre className="tool-card__output"><AnsiText text={item.result} /></pre>}
        </div>
      ) : processView ? (
        <div className="tool-card__summary">
          <div className="tool-card__summary-line">{safeText(processView.label)}</div>
          {item.status === 'error' && item.result !== '' && (
            <div className="tool-card__summary-error">{safeText(item.result)}</div>
          )}
          {item.status !== 'error' && item.result !== '' && (
            <pre className="tool-card__output"><AnsiText text={item.result} /></pre>
          )}
        </div>
      ) : writeView ? (
        <div className="tool-card__summary">
          <div className="tool-card__summary-path">{safeText(writeView.path)}</div>
          <div className="tool-card__summary-text">{safeText(writeView.summary)}</div>
        </div>
      ) : readView ? (
        <div className="tool-card__summary">
          <div className="tool-card__summary-path">{safeText(readView.path)}</div>
          {item.result !== '' && <pre className="tool-card__output"><AnsiText text={item.result} /></pre>}
        </div>
      ) : todoView ? (
        <div className="tool-card__todo">
          {todoView.error !== '' && (
            <div className="tool-card__summary-error">{safeText(todoView.error)}</div>
          )}
          {todoView.phases.length > 0 ? (
            <div className="tool-card__todo-phases">
              {todoView.phases.map((phase, pIndex) => (
                <div
                  key={`${pIndex}-${phase.name || 'phase'}`}
                  className="tool-card__todo-phase"
                  data-phase-name={phase.name || undefined}
                >
                  {phase.name !== '' && (
                    <div className="tool-card__todo-phase-name">{safeText(phase.name)}</div>
                  )}
                  {phase.tasks.length > 0 && (
                    <ul className="tool-card__todo-tasks">
                      {phase.tasks.map((task, tIndex) => (
                        <li
                          key={`${pIndex}-${tIndex}`}
                          className={`tool-card__todo-task${task.status === 'in_progress' ? ' tool-card__todo-task--active' : ''}`}
                          data-status={task.status}
                        >
                          <span
                            className={`tool-card__todo-marker tool-card__todo-marker--${task.status}`}
                            title={task.status}
                            aria-hidden="true"
                          >
                            {TODO_TASK_MARKERS[task.status] ?? ''}
                          </span>
                          <span className="tool-card__todo-content">{safeText(task.content)}</span>
                          {task.blockedBy.length > 0 && (
                            <span className="tool-card__todo-blocked">
                              blocked by {safeText(task.blockedBy.join(', '))}
                            </span>
                          )}
                        </li>
                      ))}
                    </ul>
                  )}
                </div>
              ))}
            </div>
          ) : todoView.fallback !== '' ? (
            <pre className="tool-card__output"><AnsiText text={todoView.fallback} /></pre>
          ) : (
            <div className="tool-card__todo-empty">no tasks</div>
          )}
          {todoView.completed.length > 0 && (
            <div className="tool-card__todo-completed">
              completed: {safeText(todoView.completed.join(', '))}
            </div>
          )}
          {todoView.phases.length > 0 && (
            <details className="tool-card__raw">
              <summary>raw args / details</summary>
              <pre className="tool-card__args">{safeText(JSON.stringify({ args: item.args, details: item.details }, null, 2))}</pre>
            </details>
          )}
        </div>
      ) : hubView ? (
        // Hub tool card: humanized wait/send/other-op view — the raw
        // `{ids, op, timeoutMs}` args envelope and internal UUIDs never
        // render in the default view (no .tool-card__raw expander). Body and
        // note content render as SAFE Markdown (shared renderMarkdown +
        // hydrateMermaid) with a compact fold + expand toggle; a running
        // wait keeps its fixed 'Waiting' headline + state label.
        <div className="tool-card__hub">
          {hubView.headline !== '' && (
            <div className="tool-card__hub-headline">{safeText(hubView.headline)}</div>
          )}
          {hubView.body !== '' && (
            <div className="tool-card__hub-body">
              <HubMarkdownFold text={hubView.body} onLayoutChange={onLayoutChange} />
              {hubView.metadata ? (
                <span className="tool-card__hub-meta">{safeText(hubView.metadata)}</span>
              ) : null}
            </div>
          )}
          {hubView.note !== '' && (
            <HubMarkdownFold text={hubView.note} className="tool-card__hub-note" onLayoutChange={onLayoutChange} />
          )}
          {hubView.reply && (
            <div className="tool-card__hub-reply">
              <div className="tool-card__hub-headline">{safeText(hubView.reply.headline)}</div>
              <div className="tool-card__hub-body">
                <HubMarkdownFold text={hubView.reply.body} onLayoutChange={onLayoutChange} />
                {hubView.reply.metadata ? (
                  <span className="tool-card__hub-meta">{safeText(hubView.reply.metadata)}</span>
                ) : null}
              </div>
            </div>
          )}
        </div>
      ) : (
        <div className="tool-card__summary">
          {compactToolArgs(item.args) !== '' && (
            <div className="tool-card__summary-line">{safeText(compactToolArgs(item.args))}</div>
          )}
          {item.result !== '' && <pre className="tool-card__output"><AnsiText text={item.result} /></pre>}
          <details className="tool-card__raw">
            <summary>raw args</summary>
            <pre className="tool-card__args">{safeText(JSON.stringify(item.args, null, 2))}</pre>
          </details>
        </div>
      )}
      {media.length > 0 && (
        <div className="tool-media">
          {media.map((asset, index) => asset.kind === 'image' ? (
            <img
              key={`image-${index}`}
              className="tool-media__image"
              src={`data:${asset.mimeType};base64,${asset.data}`}
              alt={asset.alt}
              onLoad={onLayoutChange}
            />
          ) : (
            <video
              key={`video-${index}`}
              className="tool-media__video"
              src={`data:${asset.mimeType};base64,${asset.data}`}
              aria-label={asset.alt}
              controls
              preload="metadata"
              onLoadedMetadata={onLayoutChange}
            />
          ))}
        </div>
      )}
    </div>
  );
}

/** Bash / command-output card — the web mirror of the TUI tool card
 *  (crates/pi-cli/src/tool_card_adapter.rs). When a `command` is present the
 *  header shows the fixed title "Command" (never the raw tool name) and the
 *  body leads with the command line (`$ …`, line-clamped to 2 lines) followed
 *  by the scrollable mono output. `command` omitted renders a bare output
 *  card (unmatched toolResult) with a muted label. Status is conveyed by
 *  border color only — no "done" text label (green = success, red = failure,
 *  default = running/cancelled). Shared by the session transcript and the
 *  collab guest view. */
export function BashCard({ command, label, output, status }: { command?: string; label?: string; output: string; status?: string }) {
  const [copied, setCopied] = useState(false);
  const copyOutput = () => {
    // Copy the redacted plain text — parser plain first, then redact the
    // full plain text so even a credential split across an SGR boundary in
    // the raw output is caught. No ANSI/control escape sequences ever reach
    // the clipboard.
    const plain = redactSecrets(ansiToPlainText(output));
    const flash = () => {
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1200);
    };
    if (navigator.clipboard && typeof navigator.clipboard.writeText === 'function') {
      navigator.clipboard.writeText(plain).then(flash, flash);
    } else {
      const textarea = document.createElement('textarea');
      textarea.value = plain;
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
  const hasCommand = command !== undefined && command !== '';
  const statusClass = status === 'error' ? ' msg--bash--error'
    : status === 'done' ? ' msg--bash--done' : '';
  return (
    <div className={`msg msg--bash${statusClass}`} data-bash-status={status || undefined}>
      <div className="bash-head">
        {hasCommand ? (
          <span className="bash-cmd bash-cmd--title">Command</span>
        ) : (
          <span className="bash-cmd bash-cmd--label">{label ?? 'output'}</span>
        )}
        {status === 'running' && (
          <span className="tool-card__state tool-card__state--running">running…</span>
        )}
        <button type="button" className="bash-copy" onClick={copyOutput}>
          {copied ? 'copied' : 'copy'}
        </button>
      </div>
      {hasCommand && (
        <div className="bash-command-line">
          <span className="bash-prompt">$</span>
          <span className="bash-command-text">{command}</span>
        </div>
      )}
      <pre className="bash-output"><AnsiText text={output} /></pre>
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

// Composer attachment types, classification/limits/order helpers, and the
// image ContentBlock wire mapping live in ./attachments (shared by the three
// intake paths — paste, drag/drop, file picker — and unit-tested there).
import {
  type ComposerAttachment,
  type ReadResult,
  attachmentAccept,
  attachmentsToImageBlocks,
  buildCodeMessage,
  classifyAttachments,
  codeBadgeLabel,
  formatSkipSummary,
  wireFootprint,
  readAccepted,
  readAttachmentsInOrder,
  reconcileIntakeBudget,
  removeSentAttachments,
} from './attachments';
import { PASTED_TEXT_ATTACHMENT_NAME, largeTextDisplay, planLargeTextPaste } from './composerPaste';

/** Live voice settings as projected by the backend's Web wire
 *  (`runtimeSettings.live`): `enabled`, `mode`, `sttConfigured`,
 *  `realtimeConfigured`, `realtimeModel`, `voice`. Endpoint URLs and API
 *  keys are NEVER on this wire — they are server-held, and the browser
 *  reaches both voice paths only through the backend RPC proxy. */
interface LiveSettingsWire {
  enabled?: boolean;
  mode?: string;
  sttConfigured?: boolean;
  realtimeConfigured?: boolean;
  realtimeModel?: string;
  voice?: string;
}

/** Convert a MediaRecorder blob to WAV; returns null when the container is
 *  undecodable. The backend `stt_transcribe` RPC accepts ONLY the WAV
 *  container, so a null here surfaces a bounded error instead of sending the
 *  recorder's native (webm/mp4) bytes. The decoded AudioBuffer is resampled
 *  to the fixed 16 kHz STT rate before encoding (capture devices commonly
 *  run at 48 kHz), so the 30-second decoded-size cap holds for any device. */
async function blobToWav(blob: Blob): Promise<Blob | null> {
  try {
    // webkitAudioContext: Safari's prefixed constructor (same shape).
    const webkitWindow = window as unknown as { webkitAudioContext?: typeof AudioContext };
    const AudioCtor = window.AudioContext ?? webkitWindow.webkitAudioContext;
    if (!AudioCtor) return null;
    const ctx = new AudioCtor();
    try {
      const audioBuffer = await ctx.decodeAudioData(await blob.arrayBuffer());
      const channel = audioBuffer.numberOfChannels > 0 ? audioBuffer.getChannelData(0) : new Float32Array(0);
      const resampled = resampleToSttRate(channel, Math.max(1, Math.round(audioBuffer.sampleRate)));
      return new Blob([encodeWavPcm16(resampled, STT_SAMPLE_RATE)], { type: 'audio/wav' });
    } finally {
      void ctx.close();
    }
  } catch {
    return null;
  }
}
/* ------------------------------------------------------------------ *
 * Composer command picker
 * ------------------------------------------------------------------ */

/** A slash-menu button left of the composer textarea that opens a searchable
 *  popover of the backend `get_commands` catalog. The backend catalog is
 *  authoritative — this component holds no second command list; it only fetch
 *  `get_commands`, normalizes the wire, searches it, and inserts the chosen
 *  draft into the textarea, then focuses it WITHOUT submitting. The user
 *  confirms and Main dispatches.
 *
 *  Two modes share one popover:
 *   - `commands` lists the Web-executable builtins (compact/skill/code-review).
 *     Selecting the `/skill` parent (or opening the picker while the composer
 *     already holds a `/skill` prefix) drills into `skills` mode.
 *   - `skills` lists every loaded skill candidate (`source === "skill"`) with
 *     its bare name + description and a PERSISTENT instruction line ("Select a
 *     skill, then press Enter to run it") — the picker never auto-submits, so
 *     the Enter-to-run contract is stated inline every time skills mode shows.
 *     Selecting a candidate inserts `/skill <name>` via `onSkillSelect` (which
 *     stages the draft + visible feedback) so Main's submit path dispatches
 *     the typed `skill` RPC. A truly empty catalog ("No skills loaded") tells
 *     the user where skills load from and how to reload, distinct from a query
 *     that merely matched nothing.
 *  Prompt/extension dynamic commands never enter the picker surface — the
 *  backend still executes them, but the Web composer does not wire dispatch.
 *  Keyboard (Arrow/Enter/Escape/Back), outside-click dismiss, and ARIA listbox
 *  semantics make it usable without a mouse and on mobile. */
function CommandPicker({
  connected,
  sendCommand,
  onSelect,
  onSkillSelect,
  onError,
  getComposerValue,
}: {
  connected: boolean;
  sendCommand: (command: Record<string, unknown>) => Promise<unknown>;
  onSelect: (text: string) => void;
  onSkillSelect: (text: string) => void;
  onError: (message: string) => void;
  getComposerValue: () => string;
}) {
  const [open, setOpen] = useState(false);
  const [mode, setMode] = useState<'commands' | 'skills'>('commands');
  const [query, setQuery] = useState('');
  const [commands, setCommands] = useState<CommandEntry[]>([]);
  const [loading, setLoading] = useState(false);
  const [activeIndex, setActiveIndex] = useState(0);
  const containerRef = useRef<HTMLDivElement>(null);
  const btnRef = useRef<HTMLButtonElement>(null);
  const searchRef = useRef<HTMLInputElement>(null);
  const listId = 'command-picker-list';
  // Mirrors `open` for selection idempotence: click is the SOLE chooser (see
  // the option's onMouseDown/onClick contract below), so a single press
  // dispatches exactly one choose. The ref is defense in depth — a stale
  // render or a future event path that could double-fire choose must not
  // insert the draft twice (the first choose flips it and closes the popover,
  // so the trailing call becomes a no-op). Also stays true across the /skill
  // drill (which does NOT close), so the follow-up candidate pick proceeds.
  const openRef = useRef(false);

  // The backend get_commands is the catalog authority; fetch on each open so
  // the menu reflects freshly loaded skills (reload/reconnect picks up new
  // candidates without a stale client cache). Cheap local RPC.
  const load = useCallback(() => {
    if (!connected) return;
    setLoading(true);
    sendCommand({ type: 'get_commands' })
      .then((data) => {
        // Backend get_commands is authoritative; narrow to the Web-executable
        // surface (compact/skill/code-review builtins + loaded skill
        // candidates) so every shown item is dispatchable or drills into a
        // dispatchable candidate list.
        setCommands(filterSupportedCommands(normalizeCommands(data)));
        setActiveIndex(0);
        setLoading(false);
      })
      .catch((err: unknown) => {
        openRef.current = false;
        setLoading(false);
        setOpen(false);
        onError(`commands failed: ${err instanceof Error ? err.message : String(err)}`);
      });
  }, [connected, sendCommand, onError]);

  // Opening while the composer holds `/skill [query]` drills straight into
  // skill candidates AND carries the typed tail into the picker search. This
  // makes `/skill research` + Command immediately show matching skills instead
  // of opening an unfiltered list (the composer remains editable until pick).
  const openPicker = useCallback(() => {
    openRef.current = true;
    setOpen(true);
    setActiveIndex(0);
    const intent = pickerIntentFromDraft(getComposerValue() || '');
    setMode(intent.mode);
    setQuery(intent.query);
    load();
  }, [load, getComposerValue]);

  const close = useCallback(() => {
    openRef.current = false;
    setOpen(false);
    setMode('commands');
    setQuery('');
  }, []);

  // The trigger button is a two-state affordance for the popover: a click
  // while CLOSED opens it (re-deriving mode/query from the current composer
  // draft, so `/skill <query>` pre-filters), a click while OPEN closes it.
  // Escape in skills mode only drills BACK — the popover stays open — so the
  // very next trigger click dismisses, and the one after reopens. Keeping the
  // toggle explicit here (instead of an inline ternary) documents that
  // contract: callers must not assume "Command" always opens; when the
  // popover is already open (e.g. after an Escape drill-back) it dismisses.
  // Reopen-after-close re-reads the composer draft, so the prefiltered
  // `/skill <query>` intent survives an Escape-close→Command cycle.
  const togglePicker = useCallback(() => {
    if (open) {
      close();
    } else {
      openPicker();
    }
  }, [open, close, openPicker]);

  const backToCommands = useCallback(() => {
    setMode('commands');
    setQuery('');
    setActiveIndex(0);
    // Re-focus the search box so keyboard nav keeps working after drilling back.
    searchRef.current?.focus();
  }, []);

  const primary = useMemo(() => primaryCommands(commands), [commands]);
  const candidates = useMemo(() => skillCandidates(commands), [commands]);
  const list = useMemo(() => {
    if (mode === 'skills') return filterCommands(candidates, query);
    // Keep the initial menu compact, but once the user types, search the
    // primary commands AND loaded skills. A concrete skill is therefore
    // discoverable directly from the first search box.
    return query.trim() === '' ? primary : filterCommands([...primary, ...candidates], query);
  }, [mode, candidates, primary, query]);
  const safeActive = list.length === 0 ? -1 : Math.min(activeIndex, list.length - 1);

  const choose = useCallback(
    (entry: CommandEntry) => {
      // Called from click (mouse press) and Enter (keyboard) only — never
      // from mousedown (that must not mutate the list mid-gesture). The
      // openRef guard makes a double-fired choose (stale render, future event
      // path) a no-op once the popover has closed. A click-only dispatch
      // (programmatic or assistive tech) has no preceding mousedown and
      // proceeds normally.
      if (!openRef.current) return;
      // Selecting the `/skill` parent in commands mode drills into the loaded
      // skill candidates instead of inserting a bare `/skill ` draft. The user
      // then picks a candidate (or Escape back) — no auto-submit either way.
      if (mode === 'commands' && entry.name === 'skill') {
        setMode('skills');
        setQuery('');
        setActiveIndex(0);
        searchRef.current?.focus();
        return;
      }
      if (isSkillCandidate(entry) && entry.skillName) {
        // Skill candidates go through onSkillSelect (stage + toast in App):
        // insertCommandText would leave the caret at the end with no feedback,
        // and the contract requires a visible "ready — press Enter to run" cue
        // plus the draft selected so a typed replacement is a single keystroke.
        onSkillSelect(composeSkillCommandText(entry.skillName));
      } else {
        onSelect(composeCommandText(entry.name, entry.requiresArguments));
      }
      close();
      // onSkillSelect / onSelect (insertCommandText) already focus the
      // textarea so the user can type the command's argument; do NOT re-focus
      // the trigger button.
    },
    [mode, onSelect, onSkillSelect, close],
  );

  // Focus the search input when the popover opens or drills between modes; restore
  // focus to the trigger button only when the popover closes via keyboard.
  useEffect(() => {
    if (open) searchRef.current?.focus();
  }, [open, mode]);

  // Outside-click dismiss. mousedown (not click) fires before the option's
  // own click would; a press inside the container never dismisses, so the
  // option's mousedown/click selection always runs before any outside blur.
  useEffect(() => {
    if (!open) return;
    function onDown(e: MouseEvent) {
      if (containerRef.current && !containerRef.current.contains(e.target as Node)) close();
    }
    document.addEventListener('mousedown', onDown);
    return () => document.removeEventListener('mousedown', onDown);
  }, [open, close]);

  function onKeyDown(e: KeyboardEvent<HTMLInputElement>) {
    if (e.key === 'ArrowDown') {
      e.preventDefault();
      if (list.length) setActiveIndex((i) => Math.min(i + 1, list.length - 1));
    } else if (e.key === 'ArrowUp') {
      e.preventDefault();
      if (list.length) setActiveIndex((i) => Math.max(i - 1, 0));
    } else if (e.key === 'Enter') {
      e.preventDefault();
      const pick = list[safeActive];
      if (pick) choose(pick);
    } else if (e.key === 'Escape') {
      e.preventDefault();
      if (mode === 'skills') {
        // Drill-back: Escape in skill-candidate mode returns to the command
        // list instead of closing the popover, mirroring a back button.
        backToCommands();
      } else {
        close();
        btnRef.current?.focus();
      }
    } else if (e.key === 'Backspace' && mode === 'skills' && query === '') {
      // Backspace on an empty skill-mode query returns to the command list —
      // a natural drill-out for keyboard/mobile users.
      e.preventDefault();
      backToCommands();
    }
  }
  const placeholder = mode === 'skills' ? 'Search skills…' : 'Search commands or skills…';
  const ariaLabel = mode === 'skills' ? 'Search skills' : 'Search commands or skills';
  // Skills mode distinguishes a catalog with NOTHING loaded (renders the
  // guidance block below) from a query that matched no loaded skill. Commands
  // mode keeps its generic no-match hint.
  const emptyHint =
    mode === 'skills'
      ? candidates.length === 0
        ? 'No skills loaded'
        : 'No skills match'
      : 'No commands match';

  return (
    <div className="command-picker" ref={containerRef}>
      <button
        ref={btnRef}
        id="command-btn"
        type="button"
        title="Search commands and skills"
        aria-haspopup="listbox"
        aria-expanded={open}
        aria-controls={listId}
        disabled={!connected}
        onClick={togglePicker}
      >
        <span aria-hidden="true">/</span>
        <span className="command-btn__label">Command</span>
      </button>
      {open && (
        <div className="command-picker__popover" role="dialog" aria-label="Slash commands">
          {mode === 'skills' && (
            <div className="command-picker__header">
              <button
                type="button"
                className="command-picker__back"
                aria-label="Back to commands"
                onClick={backToCommands}
              >
                ‹ Commands
              </button>
              <span className="command-picker__title">Skills</span>
            </div>
          )}
          {mode === 'skills' && (
            <div className="command-picker__tip" role="status">
              Select a skill, then press Enter to run it
            </div>
          )}
          <input
            ref={searchRef}
            className="command-picker__search"
            type="text"
            placeholder={placeholder}
            value={query}
            aria-label={ariaLabel}
            aria-controls={listId}
            aria-expanded={open}
            aria-autocomplete="list"
            autoComplete="off"
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={onKeyDown}
          />
          <ul id={listId} className="command-picker__list" role="listbox" aria-label={ariaLabel}>
            {loading && <li className="command-picker__hint" role="status">Loading…</li>}
            {!loading && list.length === 0 && (
              mode === 'skills' && candidates.length === 0 ? (
                // A genuinely empty skill catalog, not a query that matched
                // nothing: tell the user where skills load from (repo-relative
                // project dir + user skills directory, no machine paths) and
                // the reload actions that pick up newly added skills.
                <li className="command-picker__hint command-picker__hint--empty" role="status">
                  <span className="command-picker__hint-title">No skills loaded</span>
                  <span className="command-picker__hint-detail">
                    Skills load from your project&apos;s <code>.pi/skills</code> and your
                    user skills directory when a session starts. Restart the listener or
                    open a new session to reload.
                  </span>
                </li>
              ) : (
                <li className="command-picker__hint">{emptyHint}</li>
              )
            )}
            {!loading && list.map((command, i) => (
              <li
                key={command.name}
                role="option"
                className={`command-picker__option${i === safeActive ? ' is-active' : ''}`}
                aria-selected={i === safeActive}
                data-skill-name={isSkillCandidate(command) ? command.skillName : undefined}
                // A real mouse press fires mousedown then click on the SAME
                // row. Choosing on mousedown is WRONG for the /skill drill: it
                // swaps the list to skills mid-gesture, so the trailing click
                // lands on whatever candidate now occupies that spot and
                // selects it — closing the popover instead of showing Skills.
                // mousedown therefore only preventDefaults (keeps focus in the
                // search box); click is the SOLE chooser, so one press-release
                // dispatches exactly one choose, on the row the user pressed.
                // Programmatic/assistive clicks (no mousedown) go through the
                // same click path. choose's openRef guard stays as defense in
                // depth against any future event-path that could double-fire.
                onMouseDown={(e) => { e.preventDefault(); }}
                onClick={() => choose(command)}
                onMouseEnter={() => setActiveIndex(i)}
              >
                {isSkillCandidate(command) ? (
                  <>
                    <span className="command-picker__name">{command.skillName}</span>
                    <span className="command-picker__desc">{command.description}</span>
                  </>
                ) : (
                  <>
                    <span className="command-picker__name">/{command.name}</span>
                    {command.argumentHint && (
                      <span className="command-picker__arg">{command.argumentHint}</span>
                    )}
                    <span className="command-picker__desc">{command.description}</span>
                  </>
                )}
              </li>
            ))}
          </ul>
        </div>
      )}
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
  // Mirror of the streaming flags, read/written synchronously by the hot event
  // path. The delta path (message_update per chunk) previously called
  // setStreamingBySessionId on EVERY chunk; each call produced a NEW state
  // object, forcing a full App re-render per delta (with the whole transcript
  // re-rendered, including renderBlocks/KaTeX of every final message). The
  // mirror lets the same-value transition (true->true during a stream) skip
  // React entirely while the observable state sequence stays identical.
  const streamingBySessionIdRef = useRef<Record<string, boolean>>({});
  const markStreamingFor = useCallback((sid: string | null, on: boolean) => {
    if (!sid) return;
    if (streamingBySessionIdRef.current[sid] === on) return;
    streamingBySessionIdRef.current = { ...streamingBySessionIdRef.current, [sid]: on };
    setStreamingBySessionId((prev) => ({ ...prev, [sid]: on }));
  }, []);
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
  // Restore the last selected listener for this page origin; fall back to the
  // authority that served the page.
  const [hostInput, setHostInput] = useState(initialHostAuthority);
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
  // Todo DAG panel. `activePanel` is the single shared panel-name state;
  // each web panel registers its own name + a mount-point render line below.
  const [activePanel, setActivePanel] = useState<string>('');

  // Revisions captured when `/code-review [from to]` opens the panel. Cleared
  // on close/session reset so a later bare open does not reuse stale revs.
  const [codeReviewOpenArgs, setCodeReviewOpenArgs] = useState<CodeReviewOpenArgs>({});
  // Persistent session sidebar (left rail) + mobile drawer toggle.
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
  // Open/toggle a feature panel. The updater is PURE (no other state setter,
  // no matchMedia read) — the mobile-drawer close below is driven by state in
  // a separate effect, so React never re-runs a side effect inside an
  // updater. On phone widths the session drawer sits at z-index 44 while
  // ordinary panels are 46; still, opening a panel MUST close the drawer so
  // the user isn't left with two competing full-height surfaces and a
  // dead-end return path. Desktop keeps the rail open.
  const openPanel = useCallback((name: string, opts?: { force?: boolean }) => {
    setActivePanel((current) => (!opts?.force && current === name ? '' : name));
  }, []);

  // Any non-empty activePanel (feature-nav toggle, Manage session, /code-review
  // slash command) collapses the mobile drawer at <=720px before paint; the
  // drawer-close is keyed off state here instead of firing from inside
  // openPanel's updater. Desktop (>=721px) keeps the rail open.
  useLayoutEffect(() => {
    if (activePanel !== '' && window.matchMedia('(max-width: 720px)').matches) {
      setSidebarOpen(false);
    }
  }, [activePanel]);

  // Keep the mobile session drawer top edge flush with the live header height
  // (phone chrome wraps; a fixed 48px top left the feature nav under the bar).
  // useLayoutEffect so the INITIAL measurement lands before first paint — a
  // post-paint write made the drawer visibly jump down one frame on a wrapped
  // header; the ResizeObserver + resize listener keep it live afterward.
  useLayoutEffect(() => {
    const header = document.querySelector('header');
    if (!header || typeof ResizeObserver === 'undefined') return;
    const apply = () => {
      const h = Math.round(header.getBoundingClientRect().height);
      document.documentElement.style.setProperty('--app-header-height', `${Math.max(44, h)}px`);
    };
    apply();
    const ro = new ResizeObserver(apply);
    ro.observe(header);
    window.addEventListener('resize', apply);
    return () => {
      ro.disconnect();
      window.removeEventListener('resize', apply);
    };
  }, []);

  // Shared desktop height for ordinary bottom drawers (vh). Applied as
  // --panel-drawer-height on <html>; one resizer serves every ordinary panel.
  const [panelDrawerVh, setPanelDrawerVh] = useState(() =>
    typeof window !== 'undefined' ? readStoredPanelDrawerVh() : PANEL_DRAWER_DEFAULT_VH
  );
  const panelDrawerVhRef = useRef(panelDrawerVh);
  panelDrawerVhRef.current = panelDrawerVh;
  const panelResizeDragRef = useRef<{ startY: number; startVh: number } | null>(null);
  // Active session's derived view (empty until that session has items).
  const [todoPhasesBySessionId, setTodoPhasesBySessionId] = useState<Record<string, TodoPhaseWire[]>>({});
  // Goal panel — current goal snapshot + journal replay (live via
  // goal_updated / goal_usage_charged events, refreshed via goal_get /
  // goal_journal). Keyed by session: every refresh/event carries the owning
  // sessionId, so a background session's goal can never mutate the active
  // panel and an async refreshGoal() can never apply to the wrong session.
  const [goalStateBySessionId, setGoalStateBySessionId] = useState<Record<string, GoalStateWire | null>>({});
  const [goalJournalBySessionId, setGoalJournalBySessionId] = useState<Record<string, GoalEventWire[]>>({});
  // Side chat (parallel /btw sessions) snapshot; polled while the
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

  // Composer file attachments: images -> prompt `images` ContentBlocks; UTF-8
  // text/code files -> filename + fenced-code blocks inside the prompt
  // `message`. Classification/limits/order are enforced in ./attachments.
  const [attachments, setAttachments] = useState<ComposerAttachment[]>([]);
  // Drag/drop drop-target highlight. A depth counter (dragDepthRef) avoids the
  // flicker from dragenter/dragleave firing on child elements of the footer.
  const [dropActive, setDropActive] = useState(false);
  // Hold-to-talk voice: recording flag drives the pulsing mic indicator;
  // transcribing blocks a second capture while the STT round-trip runs.
  const [recording, setRecording] = useState(false);
  const [transcribing, setTranscribing] = useState(false);
  // Live/STT settings surfaced by get_state (`runtimeSettings.live`) when the
  // backend exposes them; null means "not advertised" (mic still shown).
  const [liveSettings, setLiveSettings] = useState<LiveSettingsWire | null>(null);

  const wsRef = useRef<WebSocket | null>(null);
  // Monotonic socket generation: connect() bumps it; every pending command
  // and every onOpen bootstrap continuation stamps/captures it so a socket
  // superseded mid-bootstrap (A dropped, B reconnected healthy) can never
  // applyState / setConnState / refreshGoal / scheduleReconnect on B's behalf
  // — its pending is drained on replace and its continuation bails via
  // STALE_ABORT / `!alive()`. See ./socket.ts for the pure invariant.
  const socketGenRef = useRef(0);
  // Bounded ready gate for auto/background loads (sidebar + session panel
  // `load()` on mount). On a fresh page the sidebar effect runs while
  // `connect()` is still in CONNECTING, so a direct `sendCommand` would reject
  // with `not connected` and surface a persistent `load failed: not
  // connected`. The gate lets those auto-loads WAIT for the current socket's
  // open (bounded by READY_GATE_TIMEOUT_MS) instead of failing immediately;
  // `onOpen` resolves every pending waiter, and unmount clears them. Active
  // actions (New / Switch / rename) bypass the gate and still fail fast. See
  // ./socket.ts ReadyGate for the generation-safety + bounded-wait contract.
  const readyGateRef = useRef<ReadyGate | null>(null);
  // The target rpi listener, restored before initial bootstrap. Tokens and
  // session preferences are scoped to this authority.
  const hostRef = useRef(initialHostAuthority);
  const tokenRef = useRef(token);
  // Pending-command registry (lazily created after removeItem is defined, so
  // its timeout hook can remove optimistic bubbles); see ./pending.ts.
  const pendingRegistryRef = useRef<PendingRegistry | null>(null);
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
  // True once any socket has reached OPEN: distinguishes the initial boot
  // probe/refusal (never opened) from a MID-SESSION abnormal drop (1006), so
  // the close surface can show an accurate cause instead of the token hint.
  const everConnectedRef = useRef(false);
  const optimisticQueueBySessionIdRef = useRef<Record<string, string[]>>({});
  // One coherent transcript scroll-pin state, shared contract with the collab
  // guest view (see ./scrollPin): `transcriptRef` is the scroll container,
  // `transcriptContentRef` the resize-observed content wrapper.
  const { transcriptRef, transcriptContentRef, onTranscriptScroll, pinIfPinned, forcePin } = useScrollPin();
  const promptInputRef = useRef<HTMLTextAreaElement>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const dragDepthRef = useRef(0);
  // Authoritative concurrent attachment budget (queued + in-flight reserved).
  // See onFilesChosen's CONCURRENCY INVARIANT. Reconciled from `attachments`
  // inside every setAttachments updater that touches attachments.
  const intakeBudgetRef = useRef({ count: 0, wire: 0 });
  const mediaStreamRef = useRef<MediaStream | null>(null);
  const mediaRecorderRef = useRef<MediaRecorder | null>(null);
  const mediaChunksRef = useRef<Blob[]>([]);
  const recordingTimerRef = useRef<number | null>(null);
  // Codex Live realtime voice (WebRTC): active while the call is up; the
  // `oai-events` data channel delivers transcript/delegation events for the
  // overlay. The transcript text is accumulated in a ref and written straight
  // into the overlay node (no React re-render per delta — same hot-path
  // decision as the streaming transcript); realtimeDelegation is a rare state
  // change.
  const [realtimeActive, setRealtimeActive] = useState(false);
  const [realtimeDelegation, setRealtimeDelegation] = useState<string | null>(null);
  // Realtime WebRTC connection-state bucket (RTCPeerConnectionState via
  // classifyRealtimeConnectionState). Surfaced in the overlay so the user can
  // SEE a stuck/failed connection instead of staring at a silent mic button;
  // 'disconnected' is recoverable (toast, no teardown) while 'failed' is
  // terminal (toast + teardown).
  const [realtimeConnState, setRealtimeConnState] = useState<string | null>(null);
  // True when the remote audio track's play() rejected (autoplay policy or
  // user gesture missing). The overlay shows a click-to-enable-audio control
  // so the failure is actionable instead of silently swallowed.
  const [realtimeAudioBlocked, setRealtimeAudioBlocked] = useState(false);
  const realtimePcRef = useRef<RTCPeerConnection | null>(null);
  // The `oai-events` RTCDataChannel on the realtime peer connection carries
  // session.update + server events (transcript/delegation/error). It rides
  // the WebRTC DTLS transport (no browser-side Bearer header, no mixed-content
  // block) — the direct sideband WebSocket was removed for those reasons.
  const realtimeDcRef = useRef<RTCDataChannel | null>(null);
  // Synchronous start-guard: getUserMedia/offer/answer is async, so a quick
  // double-click could race two call setups before realtimeActive re-renders.
  const realtimeBusyRef = useRef(false);
  const realtimeAudioRef = useRef<HTMLAudioElement | null>(null);
  const realtimeTranscriptRef = useRef('');
  // Last finalized USER input transcript committed to the composer, used to
  // dedup the two V1 final-event variants (input_transcript.done +
  // input_audio_transcription.completed) for one utterance. Reset per call.
  const lastCommittedTranscriptRef = useRef('');
  const realtimeTranscriptNodeRef = useRef<HTMLDivElement | null>(null);
  // Suppresses RPC error toasts during the reconnection bootstrap sequence
  // (get_state rebind) so a phone waking from sleep doesn't spam "command
  // get_state failed" before the rebind-to-primary lands.
  const bootRef = useRef(false);
  // Orchestration event handlers registered by the Subagents panel. A Set
  // keeps the panel subscription additive alongside other live panels.
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

  const appendToLiveNode = useCallback((sid: string | null, itemId: string, delta: string, kind: 'text' | 'thinking') => {
    applyDeltaToNode(sid, itemId, delta, kind);
    // Auto-scroll only when the streaming session IS the active view: a
    // background session's deltas must never move the active transcript.
    // Whether the active view follows at all is the pin state's decision.
    if (sid === sessionIdRef.current) pinIfPinned();
  }, [pinIfPinned]);

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

  // Pending-command registry: owns the id -> pending map and the per-command
  // bounded timeout timers (see ./pending.ts for the ack contract + classes).
  // Created once per component instance; the onTimeout hook removes the
  // optimistic bubble for a PLAIN timed-out command (a long-op that ran its
  // 10-minute bound, or a stale-gen fast-ack whose socket was already
  // replaced), while drain paths (close/replace/host-switch) intentionally
  // keep bubbles — the frame may already have been delivered and the reconnect
  // path owns retry semantics. A CURRENT-generation fast-ack timeout is an
  // unresponsive transport, not a command stall: onTransportStale closes the
  // socket so the existing onclose path drains remaining pending and schedules
  // a reconnect, and toasts a truthful connection-unresponsive message; the
  // bubble is kept (close/retry semantics, matching the onclose drain).
  if (pendingRegistryRef.current === null) {
    pendingRegistryRef.current = new PendingRegistry({
      scheduler: {
        setTimeout: (fn, ms) => window.setTimeout(fn, ms),
        clearTimeout: (timer) => window.clearTimeout(timer as number),
        now: () => Date.now(),
      },
      isCurrentGen: (gen) => gen === socketGenRef.current,
      onTimeout: (entry) => {
        if (entry.bubbleId) removeItem(entry.bubbleId);
      },
      onTransportStale: () => {
        // The 30s fast-ack pending timer fired while the socket stayed OPEN
        // and before the 60s liveness timer: the server swallowed the frame
        // and the connection is silently dead. Fail closed — surface a
        // truthful "connection unresponsive, reconnecting" message and close
        // THIS socket so the existing onclose path drains remaining pending
        // (truthful connection reason, bubbles kept) and schedules a reconnect
        // with the current backoff. The timed-out command's promise rejects
        // with the connection-unresponsive error (handled in pending.ts), so
        // the send-catch never sees a misleading "command timed out". The
        // bubble is intentionally NOT removed here (close/retry semantics).
        // The registry fires this AT MOST ONCE per generation: it eagerly
        // settles every other current-gen fast-ack pending on the same dead
        // socket with the same truthful message, so onTransportStale (and thus
        // this close + toast) cannot fire twice for one dead socket.
        toast('connection unresponsive, reconnecting…', true);
        const ws = wsRef.current;
        if (ws && ws.readyState === WebSocket.OPEN) {
          try {
            ws.close(TRANSPORT_STALE_CLOSE_CODE, 'connection unresponsive');
          } catch {
            /* already closing; onclose drains + reconnects */
          }
        }
      },
    });
  }
  const pendingRegistry = pendingRegistryRef.current as PendingRegistry;
  // Lazily create the ready gate with the real window scheduler. Like the
  // pending registry, it lives for the App's lifetime; onOpen resolves its
  // waiters and the unmount cleanup clears them.
  if (readyGateRef.current === null) {
    readyGateRef.current = new ReadyGate({
      setTimeout: (fn, ms) => window.setTimeout(fn, ms),
      clearTimeout: (timer) => window.clearTimeout(timer as number),
    });
  }
  const readyGate = readyGateRef.current;

  /** Bounded wait for the current socket to reach OPEN. Auto/background loads
   *  (sidebar + session panel `load()` on mount, sidebar poll) call this
   *  BEFORE `sendCommand` so a mount-before-WebSocket-OPEN does not surface a
   *  persistent `load failed: not connected`: the load waits for `onOpen`
   *  (bounded by READY_GATE_TIMEOUT_MS) and proceeds once the socket is live.
   *  A stale socket cannot trigger the load — `notifyOpen` only fires from the
   *  CURRENT socket's `onOpen` (superseded sockets' `onopen` is detached
   *  first), and `sendCommand` re-checks `readyState === OPEN` at send time.
   *  Active actions do NOT call this; they fail fast via `sendCommand`. */
  const waitForReady = useCallback((): Promise<void> => {
    const ws = wsRef.current;
    if (ws && ws.readyState === WebSocket.OPEN) return Promise.resolve();
    return readyGate.wait();
  }, [readyGate]);

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
    const { promise, resolve, reject } = withResolvers<unknown>();
    const id = `c${++seqRef.current}`;
    // MultiSessionRuntimeManager contract: every command carries a top-level
    // sessionId so the listener routes it to the owning runtime. An explicit
    // `command.sessionId` (lifecycle targeting another session) wins over the
    // ACTIVE session; boot/create commands omit it (null -> no field). The
    // manager strips sessionId BEFORE command deserialization (parse_input),
    // so deny_unknown_fields schemas (workflow_*) accept it safely on the
    // multi-session backend — every command, workflow included, routes to the
    // owning runtime instead of defaulting to the primary.
    // The routing rule lives in ./goal (routeCommandSession) so the
    // background goal_updated / lifecycle A→B cross-write invariant is
    // testable without a socket: an explicit command.sessionId wins over the
    // active session; a null/empty/absent sessionId falls back to active.
    const sid = routeCommandSession(command, sessionIdRef.current);
    const frame = sid ? { ...command, id, sessionId: sid } : { ...command, id };
    // Per-command bounded timeout (see ./pending.ts): the registry arms a
    // class-appropriate timer, names the command + elapsed seconds in the
    // timeout error, and calls back to remove the optimistic bubble only on a
    // timeout (drain paths keep the bubble for retry semantics).
    const type = typeof command.type === 'string' ? command.type : 'command';
    pendingRegistry.add(id, { resolve, reject, bubbleId, gen: socketGenRef.current, type });
    try {
      ws.send(JSON.stringify(frame));
    } catch (cause) {
      pendingRegistry.take(id);
      if (bubbleId) removeItem(bubbleId);
      const message = cause instanceof Error ? cause.message : String(cause);
      reject(new Error(`send failed: ${message}`));
    }
    return promise;
  }, [removeItem]);

  const onResponse = useCallback((frame: RpcResponse) => {
    const id = frame.id || '';
    // take() deletes the entry and clears its timer: a response for an already
    // settled/drained id (timeout fired, socket closed) is a no-op, and a
    // drained entry can never be double-settled.
    const pending = pendingRegistry.take(id);
    if (!pending) return;
    // A response for a command sent on a superseded socket: the close/replace
    // drain already rejected its promise. Drop the stale settlement so it can
    // neither continue a dead bootstrap nor settle a newer socket's pending.
    if (!isCurrentPending(pending.gen, socketGenRef.current)) return;
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

  /** Reject + delete every pending command NOT on the keep generation, clearing
   *  its timeout and keeping optimistic bubbles (the frame may already have
   *  been delivered). Called from onclose when the socket drops — so pending
   *  commands settle IMMEDIATELY with the connection reason instead of hanging
   *  until their timeout or firing a stale continuation — and defensively from
   *  connect() when a new socket supersedes a replaced one (whose onclose was
   *  detached). */
  const rejectPendingExcept = useCallback((keepGen: number, reason: string, rpc = false) => {
    pendingRegistry.drainExcept(keepGen, reason, rpc);
  }, []);

  /** Dispatch a flattened pi_coding::TodoOp over the `todo_op` RPC. */
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

  /* ---------------- side chat ---------------- */

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

  /** Refresh the Goal panel snapshot + journal for ONE session. The
   *  target sid is captured at call time and every response is keyed to it —
   *  an async refreshGoal() can never query one session and paint another. */
  const refreshGoal = useCallback((sid: string | null) => {
    if (!sid) return;
    // Stamp the OWNING sessionId on both RPCs (goalGetCommand/goalJournalCommand)
    // so a background `goal_updated` for B while A is active — or a lifecycle
    // A→B refresh captured before sessionIdRef advances — routes to B's runtime,
    // not the active session's. Without this, sendCommand defaulted to
    // sessionIdRef.current and painted the active session's goal into B's cache.
    sendCommand(goalGetCommand(sid))
      .then((data) => {
        setGoalStateBySessionId((prev) => ({ ...prev, [sid]: data as GoalStateWire }));
      })
      .catch(() => {});
    sendCommand(goalJournalCommand(sid))
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
      streamingBySessionIdRef.current = { ...streamingBySessionIdRef.current, [target]: d.isStreaming as boolean };
      setStreamingBySessionId((prev) => ({ ...prev, [target]: d.isStreaming as boolean }));
    }
    if (Array.isArray(d.todoPhases)) {
      setTodoPhasesBySessionId((prev) => ({ ...prev, [target]: d.todoPhases as TodoPhaseWire[] }));
    }
    if (d.goal && typeof d.goal === 'object') {
      setGoalStateBySessionId((prev) => ({ ...prev, [target]: d.goal as GoalStateWire }));
    }
    // Live/STT settings are global (not per-session); advertise them to the
    // composer's mic when the backend includes them in get_state. When the
    // backend advertises runtimeSettings WITHOUT a live block (e.g. live mode
    // was turned off in settings), clear any stale liveSettings so the
    // composer doesn't keep showing realtime after a switch — never assume
    // realtime when live is absent. A missing runtimeSettings entirely (older
    // backend) leaves liveSettings untouched.
    const runtime = d.runtimeSettings;
    if (runtime && typeof runtime === 'object') {
      const live = runtime.live;
      if (live && typeof live === 'object') {
        setLiveSettings(live);
      } else {
        setLiveSettings(null);
      }
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
   *  items of the target are preserved while the target is still running: the
   *  recorder excludes the partial turn, and later events finalize them.
   *  Falls back to refreshState when the response carries no snapshot. */
  const onLifecycleResult = useCallback(
    (result: unknown): Promise<unknown> => {
      const d = (result || {}) as { sessionId?: string; state?: unknown; messages?: unknown };
      if (typeof d.sessionId === 'string' && d.sessionId !== '') {
        const target = d.sessionId;
        if (d.state !== undefined && d.state !== null) applyState(d.state, target);
        if (Array.isArray(d.messages)) {
          setItemsBySessionId((prev) => {
            const history = messagesToItems(d.messages);
            const isStreaming = (d.state as { isStreaming?: unknown } | null)?.isStreaming === true;
            return { ...prev, [target]: mergeAuthoritativeItems(history, prev[target] ?? [], isStreaming) };
          });
        }
        // Activating a session clears its unread badge.
        setUnreadBySessionId((prev) => ({ ...prev, [target]: 0 }));
        // Code review is session-scoped and never follows a lifecycle cutover.
        // Other panels (especially the Session panel driving this action) stay
        // mounted so they can render the target session snapshot.
        if (activePanel === 'code-review') {
          setActivePanel('');
          setCodeReviewOpenArgs({});
        }
        // Authoritative goal journal for the newly active session (the
        // snapshot's state carries the goal but not the journal); the sid is
        // captured, so a later switch cannot mis-route the response.
        refreshGoal(target);
        sessionIdRef.current = target;
        setSessionId(target);
        // Every successful lifecycle activation becomes this listener's saved
        // session preference. The listener authority itself survives reload.
        saveSessionPreference(tokenStorage, hostRef.current, target);
        return Promise.resolve();
      }
      return refreshState();
    },
    [activePanel, applyState, refreshState, refreshGoal]
  );

  /** Full app reset for a host switch: reject in-flight commands, drop every
   *  session's cached items/streaming/unread/shell state, close panels, and
   *  clear the module streaming registries so the app renders as freshly
   *  loaded against the new host. */
  const resetAllState = useCallback(() => {
    // Reject in-flight commands as transport errors (rpc=true so the generic
    // catch handlers stay quiet) — their responses must never repaint the
    // new host's session state. drainAll clears every timer; bubbles are kept
    // (the frame may already have been delivered to host A).
    pendingRegistry.drainAll('host switched', true);
    streamBuf.clear();
    liveNodes.clear();
    normalizedThinkingKeys.clear();
    optimisticQueueBySessionIdRef.current = {};
    activeAssistantBySessionIdRef.current = {};
    abortPendingBySessionIdRef.current = {};
    sessionIdRef.current = null;
    setSessionId(null);
    setItemsBySessionId({});
    streamingBySessionIdRef.current = {};
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
    setCodeReviewOpenArgs({});
    // Clear composer attachments + drop/intake state so files selected for
    // host A can never be dispatched to host B after a host switch.
    setAttachments([]);
    setDropActive(false);
    dragDepthRef.current = 0;
    intakeBudgetRef.current = { count: 0, wire: 0 };
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
    everConnectedRef.current = true;
    // Resolve every auto-load waiter that registered before this socket opened
    // (mount-before-OPEN sidebar/panel loads, and polls during a disconnect).
    // Only the CURRENT socket's onOpen reaches here — connect()/unmount detach
    // a superseded socket's onopen first — so a stale socket can never trigger
    // a load through the gate.
    readyGate.notifyOpen();
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
    // Capture THIS socket's generation; every bootstrap continuation checks
    // `alive()` so a socket superseded mid-bootstrap (A dropped, B reconnected
    // healthy) bails via STALE_ABORT / `!alive()` and can never applyState /
    // setConnState / refreshGoal / scheduleReconnect on B's behalf. Its pending
    // commands were already rejected by connect()'s replace drain, so the
    // rejection flowing here is caught and discarded, not retried.
    const gen = socketGenRef.current;
    const alive = () => socketGenRef.current === gen;
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
      if (!alive()) throw STALE_ABORT; // superseded: don't re-probe on the new socket
      sessionIdRef.current = null;
      return sendCommand({ type: 'get_state' });
    };
    sendCommand({ type: 'get_state' })
      .catch(rebindToPrimary)
      .then((state) => {
        if (!alive()) throw STALE_ABORT;
        applyState(state);
        const target = sessionIdRef.current;
        if (!target) throw new Error('state response did not bind a session');
        return sendCommand({ type: 'get_messages', sessionId: target }).then((data) => {
          if (!alive()) throw STALE_ABORT;
          if (!data || typeof data !== 'object' || !('messages' in data) || !Array.isArray(data.messages)) {
            throw new Error('messages response missing messages');
          }
          const messages = data.messages;
          return sendCommand({ type: 'get_state', sessionId: target }).then((latestState) => {
            if (!alive()) throw STALE_ABORT;
            const latest = (latestState || {}) as { isStreaming?: boolean };
            applyState(latestState, target);
            if (latest.isStreaming === true) {
              const shouldRestoreAssistant = shouldRestoreStreamingAssistant(messages);
              setItemsBySessionId((prev) => {
                const history = messagesToItems(messages);
                const live = prev[target] ?? [];
                const merged = mergeAuthoritativeItems(history, live);
                let assistantId = activeAssistantBySessionIdRef.current[target];
                const streamingItem = live.find(
                  (item) => item.kind === 'assistant' && item.status === 'streaming' && item.id === assistantId,
                );
                if (streamingItem) return { ...prev, [target]: merged };
                // Application::is_streaming covers the whole run, including
                // tool execution. Only synthesize an assistant when durable
                // history says the next missing record is an assistant; an
                // assistant/toolCall tail needs no empty streaming shell.
                if (!shouldRestoreAssistant) {
                  return { ...prev, [target]: merged };
                }
                assistantId = nextId('a');
                activeAssistantBySessionIdRef.current[target] = assistantId;
                return {
                  ...prev,
                  [target]: [
                    ...merged,
                    { kind: 'assistant', id: assistantId, status: 'streaming', blocks: [] },
                  ],
                };
              });
              delete optimisticQueueBySessionIdRef.current[target];
              return;
            }
            // MessageEnd may have committed between the first get_messages
            // and this settled state snapshot. Re-fetch after observing idle
            // so the replacement cannot use pre-settlement history.
            return sendCommand({ type: 'get_messages', sessionId: target }).then((settledData) => {
              if (!alive()) throw STALE_ABORT;
              if (!settledData || typeof settledData !== 'object' || !('messages' in settledData) || !Array.isArray(settledData.messages)) {
                throw new Error('settled messages response missing messages');
              }
              setItemsBySessionId((prev) => {
                const history = messagesToItems(settledData.messages);
                return { ...prev, [target]: mergeAuthoritativeItems(history, prev[target] ?? [], false) };
              });
              delete optimisticQueueBySessionIdRef.current[target];
              delete activeAssistantBySessionIdRef.current[target];
              const prefix = `${target}\u0000`;
              for (const key of streamBuf.keys()) {
                if (key.startsWith(prefix)) streamBuf.delete(key);
              }
              for (const key of normalizedThinkingKeys) {
                if (key.startsWith(prefix)) normalizedThinkingKeys.delete(key);
              }
              for (const key of liveNodes.keys()) {
                if (key.startsWith(prefix)) liveNodes.delete(key);
              }
            });
          });
        });
      })
      .then(() => {
        if (!alive()) throw STALE_ABORT;
        // Restore the saved session preference for THIS authority, once
        // per connection (never on sidebar polls). The authoritative primary
        // was bound above; session_list now reveals whether the saved (or
        // first-catalog) session differs, and switch_session re-targets it
        // BEFORE the UI is exposed so the first paint already shows the
        // restored session. A missing saved id falls back to the first row;
        // an empty catalog keeps the bound primary (persisted so a later
        // reload with the same catalog restores it). session_list is
        // auxiliary: its failure must not fail the whole bootstrap (the bound
        // primary stays active, preference untouched) — only a STALE_ABORT
        // propagates, so a superseded socket never continues on the new one's
        // behalf.
        return sendCommand({ type: 'session_list', scope: 'all_projects' })
          .then((data) => {
            if (!alive()) throw STALE_ABORT;
            // session_list -> `{ sessions: RpcSessionListRow[] }`; narrow the
            // wire shape defensively (never an unchecked inline cast) — the
            // selector re-validates each row's sessionId/path at runtime.
            const list = data && typeof data === 'object' ? data : null;
            const wireSessions = list !== null && 'sessions' in list ? list.sessions : undefined;
            const rows: SessionPreferenceRow[] = Array.isArray(wireSessions) ? wireSessions : [];
            const saved = loadSessionPreference(tokenStorage, hostRef.current);
            const target = selectSessionFromCatalog(rows, saved);
            if (!target) {
              // No catalog rows: the backend primary is the only session —
              // persist it so a later reload with the same catalog keeps it.
              if (sessionIdRef.current) {
                saveSessionPreference(tokenStorage, hostRef.current, sessionIdRef.current);
              }
              return;
            }
            if (target.sessionId === sessionIdRef.current) return; // already the active session
            // Re-target: switch_session resolves with the authoritative
            // `{sessionId,state,messages}` snapshot; onLifecycleResult
            // consumes it atomically and persists the new preference.
            return sendCommand({ type: 'switch_session', sessionPath: target.path }).then((result) => {
              if (!alive()) throw STALE_ABORT;
              return onLifecycleResult(result);
            });
          })
          .catch((err) => {
            if (err === STALE_ABORT) throw err;
            return; // catalog failure: keep the bound primary, no switch loop
          })
          .then(() => {
            if (!alive()) throw STALE_ABORT;
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
      })
      .catch((err) => {
        // Stale socket (superseded, or its pending was drained on replace):
        // bail WITHOUT scheduling a reconnect or clearing bootRef — the newer,
        // healthy socket owns the reconnect path and the boot flag. Only a
        // LIVE socket's real bootstrap failure (state did not bind, messages
        // missing) reconnects.
        if (!shouldScheduleReconnect(err, alive())) return;
        bootRef.current = false;
        scheduleReconnect();
      });
  }, [applyState, onLifecycleResult, readyGate, refreshGoal, rescheduleSilenceTimer, scheduleReconnect, sendCommand]);

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
      // Detach ALL four handlers (not just onclose) before closing: a
      // CONNECTING old socket whose onopen fires after replacement would
      // otherwise run onOpen against the NEW generation/wsRef and bootstrap a
      // second time, and a late onmessage would route old frames into the new
      // socket. detachTransportHandlers nulls onopen/onmessage/onerror/onclose.
      detachTransportHandlers(old);
      try {
        old.close(1000, 'replaced');
      } catch {
        /* already closed */
      }
    }
    // Advance the socket generation and reject every pending command from a
    // PRIOR socket (defensive: a genuine drop already drained them in onclose,
    // and a host switch drains them in resetAllState; this catches a replaced
    // socket whose onclose was detached). rpc=true keeps this quiet — it only
    // ever fires on bootstrap-path commands, which must stay silent. The new
    // socket's commands (sent from onOpen) stamp the new gen; their bootstrap
    // continuations check `alive()` so a superseded socket can never
    // applyState/setConnState/refreshGoal/scheduleReconnect on the healthy
    // socket's behalf.
    rejectPendingExcept(++socketGenRef.current, 'connection replaced', true);
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
      // IMMEDIATELY reject + delete every pending command sent on this socket —
      // before scheduling the reconnect — so an in-flight prompt settles with
      // the truthful connection reason (close code + server reason; the same
      // text the toast shows) instead of hanging until its timeout or being
      // mislabeled "timed out". The generation is bumped first so bootstrap
      // continuations observe `!alive()` and cannot schedule a second
      // reconnect on top of the one armed below. Bubbles are kept (rpc=false:
      // the send-catch surfaces the error, and the frame may already have been
      // delivered — the reconnect path owns retry semantics).
      const reason = `connection closed (code ${event.code})${event.reason ? `: ${event.reason}` : ''}`;
      rejectPendingExcept(++socketGenRef.current, reason);
      if (event.code === 1006) {
        // 1006 is a transport drop with no close frame. The boot auto-connect
        // with no token is an expected probe on a fresh page load (the user
        // has not typed anything yet). On a tokenless listener it succeeds and
        // never reaches this branch; on a tokened listener it is the expected
        // "no token yet" refusal — stay quiet and let the empty-hint explain
        // the optional-token policy. A refused connection never OPENED; any
        // 1006 on a socket that had opened is a mid-session transport failure
        // and must surface the accurate cause (code only — 1006 carries no
        // reason), never the misleading token hint.
        if (everConnectedRef.current) {
          toast('connection closed (code 1006)', true);
        } else if (!(bootProbeRef.current && !tokenValue)) {
          toast('connection failed (wrong or missing token?). Set the token in the Settings panel.', true);
        }
      } else if (event.code === TRANSPORT_STALE_CLOSE_CODE) {
        // Transport-stale: a current-generation fast-ack pending timer fired
        // while the socket stayed OPEN (the server swallowed the frame; the
        // 30s pending timer beat the 60s liveness timer). onTransportStale
        // already toasted a truthful "connection unresponsive, reconnecting"
        // message and closed this socket; the drain + reconnect below are the
        // existing onclose recovery — no duplicate generic close toast.
      } else if (event.code !== 1000) {
        toast(`connection closed (code ${event.code})${event.reason ? `: ${event.reason}` : ''}`, true);
      }
      scheduleReconnect();
    };
    ws.onerror = () => {
      /* the close event carries the failure */
    };
  }, [clearHeartbeatTimers, onOpen, rejectPendingExcept, rescheduleSilenceTimer, scheduleReconnect, toast]);

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
    everConnectedRef.current = false;
    resetAllState();
    // Load the TARGET host's scoped token (or empty) BEFORE connect — never
    // the legacy/global key, never the previous host's token — so a token
    // saved for A is not sent to B (cross-origin credential leak). Set hostRef
    // and tokenRef/state synchronously so connect() reads the target pair.
    const nextToken = loadTokenForAuthority(tokenStorage, next);
    saveActiveHost(tokenStorage, next);
    hostRef.current = next;
    tokenRef.current = nextToken;
    setToken(nextToken);
    setHostInput(next);
    connect();
  }, [connect, resetAllState]);

  /** Settings-panel token commit: persist under the CURRENT host's scoped key
   *  (rpi-web-token:<authority> — never a global key) so each host's token
   *  stays separate, and reconnect so the new token takes effect
   *  (rpi-auth.<token> subprotocol). Persist even when unchanged so the stored
   *  value reflects the panel, but reconnect only on change. */
  const handleTokenChange = useCallback((nextToken: string) => {
    const trimmed = nextToken.trim();
    const host = hostRef.current; // snapshot the authority this token belongs to
    saveTokenForAuthority(tokenStorage, host, trimmed);
    if (trimmed === tokenRef.current) return; // same token: persisted, no reconnect
    bootProbeRef.current = false;
    everConnectedRef.current = false;
    tokenRef.current = trimmed;
    setToken(trimmed);
    connect();
  }, [connect]);

  /* ---------------- event dispatch ---------------- */
  //
  // ORDERING CONTRACT: every handler referenced by onEvent is declared ABOVE
  // it (executable-safe order) and listed in onEvent's dependency array —
  // there is no use-before-declaration (TDZ) and no suppressed dependency.

  const onMessageStart = useCallback((frame: EventFrame) => {
    const message = (frame.message || {}) as {
      role?: string;
      content?: unknown;
      toolCallId?: string;
      details?: unknown;
      display?: boolean;
      customType?: string;
      command?: string;
      output?: string;
    };
    const role = message.role;
    const sid = frameSession(frame);
    if (role === 'user') {
      const projected = userMessageProjection(message.content);
      const queue = optimisticQueueBySessionIdRef.current;
      const queueId = sid ? (queue[sid] ?? []).shift() : undefined;
      if (queueId) {
        patchItemFor(sid, queueId, (item) => (item.kind === 'user' ? { ...item, optimistic: false } : item));
      } else {
        pushItemFor(sid, {
          kind: 'user',
          id: nextId('u'),
          text: projected.text,
          optimistic: false,
          ...(projected.images.length > 0 ? { images: projected.images } : {}),
          ...(projected.analysis ? { analysis: projected.analysis } : {}),
        });
      }
      return;
    }
    if (role === 'assistant') {
      markStreamingFor(sid, true);
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
      // Tool execution cards already render matched results inline; only
      // unmatched results get a standalone row. Shared with the guest view.
      const text = boundOutput(contentText(message.content), TOOL_OUTPUT_LINE_LIMIT).text;
      const toolCallId = typeof message.toolCallId === 'string' ? message.toolCallId : '';
      const media = toolMedia(message.content, message.details);
      updateItemsFor(sid, (prev) => applyToolResultToItems(prev, toolCallId, text, message.details, media));
      return;
    }
    if (role === 'bashExecution') {
      pushItemFor(sid, {
        kind: 'bash',
        id: nextId('b'),
        command: message.command || '',
        output: boundOutput(message.output || '', BASH_OUTPUT_LINE_LIMIT).text,
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
      const item = customToItem(message);
      if (item) pushItemFor(sid, item);
    }
  }, [frameSession, markStreamingFor, patchItemFor, pushItemFor, updateItemsFor]);

  const onMessageUpdate = useCallback((frame: EventFrame) => {
    const ev = (frame.assistantMessageEvent || {}) as { type?: string; delta?: string; content?: string };
    if (!ev.type) return;
    const sid = frameSession(frame);
    const targetId = sid ? (activeAssistantBySessionIdRef.current[sid] ?? '') : '';
    if (!targetId) return;
    markStreamingFor(sid, true);
    // Namespaced buffer: this session's deltas can never touch another
    // session's catch-up, even with equal item ids.
    const key = streamKey(sid, targetId);
    let buf = streamBuf.get(key);
    if (!buf) {
      buf = { text: '', thinking: '' };
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
      // toolcall_delta carries the model's raw tool-call JSON fragments
      // (e.g. `{"command":"…"}`), which is never user-facing: the runtime's
      // tool_execution_* events drive the structured tool card instead. The
      // deltas are deliberately dropped here (no DOM, no buffer) so no raw
      // JSON can leak into the transcript while the tool streams.
      case 'toolcall_delta':
        break;
      default:
        break; // start/end/done/error: message_end re-renders authoritatively
    }
  }, [appendToLiveNode, frameSession, markStreamingFor]);

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
    normalizedThinkingKeys.delete(key);
    patchItemFor(sid, targetId, (item) =>
      item.kind === 'assistant' ? { ...item, status: 'final' as const, blocks } : item
    );
    // Keep currentAssistant semantics: the streaming node stays mounted until
    // the next assistant message replaces it, so tool cards append nearby.
  }, [frameSession, patchItemFor]);

  const onToolStart = useCallback((frame: EventFrame) => {
    const sid = frameSession(frame);
    markStreamingFor(sid, true);
    pushItemFor(sid, {
      kind: 'toolCard',
      id: nextId('tc'),
      toolCallId: (frame.toolCallId as string) || '',
      toolName: safeText(frame.toolName || 'tool'),
      args: frame.args,
      status: 'running',
      result: '',
    });
  }, [frameSession, markStreamingFor, pushItemFor]);

  const onToolUpdate = useCallback((frame: EventFrame) => {
    const toolCallId = (frame.toolCallId as string) || '';
    const text = contentText((frame.partialResult as { content?: unknown } | undefined)?.content);
    if (!text) return;
    updateItemsFor(frameSession(frame), (prev) => applyToolSnapshot(prev, toolCallId, text));
  }, [frameSession, updateItemsFor]);

  const onToolEnd = useCallback((frame: EventFrame) => {
    const toolCallId = (frame.toolCallId as string) || '';
    const rawResult = frame.result;
    let content: unknown;
    let details: unknown;
    if (rawResult && typeof rawResult === 'object') {
      if ('content' in rawResult) content = rawResult.content;
      if ('details' in rawResult) details = rawResult.details;
    }
    const text = contentText(content);
    const isError = !!frame.isError;
    // Bound the rendered result to its tail like the TUI compact tool card so
    // a huge tool output never dominates the transcript while the error
    // status and trailing lines stay visible. Carry structured details for
    const media = toolMedia(content, details);
    // Task (TaskSpawn[]) / Edit (diff) default views.
    updateItemsFor(frameSession(frame), (prev) =>
      applyToolSnapshot(prev, toolCallId, text, isError ? 'error' : 'done', details, media)
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

  const onBashExecutionEnd = useCallback((frame: EventFrame) => {
    const message = (frame.message || {}) as {
      command?: string;
      output?: string;
      exitCode?: number | null;
      cancelled?: boolean;
    };
    const status: 'done' | 'error' | undefined = message.cancelled === true
      || (typeof message.exitCode === 'number' && message.exitCode !== 0)
      ? 'error'
      : typeof message.exitCode === 'number'
        ? 'done'
        : undefined;
    pushItemFor(frameSession(frame), {
      kind: 'bash',
      id: nextId('b'),
      command: message.command || '',
      output: boundOutput(message.output || '', BASH_OUTPUT_LINE_LIMIT).text,
      ...(status ? { status } : {}),
    });
  }, [frameSession, pushItemFor]);

  const finalizeActiveAssistantFor = useCallback((sid: string | null) => {
    if (!sid) return;
    const targetId = activeAssistantBySessionIdRef.current[sid] ?? '';
    if (targetId === '') return;
    const key = streamKey(sid, targetId);
    const streamedText = streamBuf.get(key)?.text ?? '';
    streamBuf.delete(key);
    normalizedThinkingKeys.delete(key);
    updateItemsFor(sid, (prev) => finalizeStreamingAssistant(prev, targetId, streamedText));
  }, [updateItemsFor]);

  /** Projected extension UI requests. Interactive asks (confirm/input/
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
    // (Todo/Goal/Workflow/Subagents/Side chat/model/thinking).
    const sid = frameSession(frame);
    const active = sid === sessionIdRef.current;
    const setStreaming = (on: boolean) => markStreamingFor(sid, on);
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
      case 'bash_execution_end':
        onBashExecutionEnd(frame);
        break;
      case 'agent_settled':
        confirmAllOptimisticFor(sid);
        setStreaming(false);
        finalizeActiveAssistantFor(sid);
        break;
      case 'run_failed':
        confirmAllOptimisticFor(sid);
        setStreaming(false);
        finalizeActiveAssistantFor(sid);
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
        // Refresh the OWNING session's Todo cache from the authoritative
        // phases payload (background sessions never touch the active panel).
        if (sid && Array.isArray(frame.phases)) {
          setTodoPhasesBySessionId((prev) => ({ ...prev, [sid]: frame.phases as TodoPhaseWire[] }));
        }
        break;
      case 'workflow_updated':
      case 'workflow_status_changed':
      case 'workflow_removed':
        // Workflow panels remount per session and refetch
        // authoritatively; events reach ONLY the ACTIVE session's mounted
        // panel. Background workflow events never mutate active state.
        if (active) {
          dispatchWorkflowEvents(frame as Parameters<typeof dispatchWorkflowEvents>[0]);
        }
        break;
      case 'goal_updated':
        // Every goal mutation (create/pin/unpin/pause/resume/complete/
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
        // Live Task cards (transcript) + Subagents panel share these events.
        // Project child status/activity/result onto matching Task tool cards
        // for the OWNING session (background sessions keep their own cache).
        if (sid) {
          if (frame.type === 'job_updated') {
            updateItemsFor(sid, (prev) => applyJobUpdated(prev, { job: frame.job }));
          } else if (frame.type === 'agent_updated') {
            updateItemsFor(sid, (prev) => applyAgentUpdated(prev, { agent: frame.agent }));
          } else {
            updateItemsFor(sid, (prev) => applyMessageDelivered(prev, { message: frame.message }));
          }
        }
        // Live orchestration events also refresh the active session's mounted
        // Subagents panel; background-session events are never forwarded.
        if (active) {
          subagentsHandlersRef.current.forEach((handler) => handler(frame));
        }
        break;
      case 'extension_ui_request':
        // Projected extension UI events are non-interactive over RPC —
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
    markStreamingFor,
    confirmAllOptimisticFor,
    finalizeActiveAssistantFor,
    onMessageStart,
    onMessageUpdate,
    onMessageEnd,
    onBashExecutionEnd,
    onToolStart,
    onToolUpdate,
    onToolEnd,
    onExtensionUiRequest,
    refreshGoal,
    toast,
    updateItemsFor,
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

  // Single-line composer: grow only to ~3 lines (measured from the input's
  // own line-height + vertical padding, so the cap stays right at any font
  // size) and collapse back to 1 line whenever the content shrinks.
  //
  // Per-keystroke onInput only SCHEDULES the resize here; the layout work
  // (height:auto reset → scrollHeight read → height write, which forces a
  // synchronous reflow per event) is coalesced into ONE pass per animation
  // frame, and the static metrics are measured once and cached (see
  // ./autoResize). submit() flushes so a cleared composer collapses
  // immediately. Declared before `submit` because the submit deps array
  // references flushComposerResize eagerly.
  const autoResizeRef = useRef<AutoResizeController<HTMLTextAreaElement> | null>(null);
  const autoResize = useCallback((input: HTMLTextAreaElement) => {
    autoResizeRef.current ??= createAutoResizeController<HTMLTextAreaElement>({
      maxLines: 3,
      measure: (el) => {
        const style = window.getComputedStyle(el);
        return {
          lineHeight: parseFloat(style.lineHeight) || 20,
          paddingVertical:
            (parseFloat(style.paddingTop) || 0) + (parseFloat(style.paddingBottom) || 0),
        };
      },
    });
    autoResizeRef.current.resize(input);
  }, []);
  const flushComposerResize = useCallback((input: HTMLTextAreaElement) => {
    autoResizeRef.current?.flush(input);
  }, []);

  const submit = useCallback((kind: 'prompt' | 'steer') => {
    const input = promptInputRef.current;
    if (!input) return;
    const text = input.value.trim();
    // Attached images -> prompt `images` ContentBlocks; attached UTF-8 code
    // files -> filename + fenced-code blocks prepended to the prompt `message`
    // (reuses the existing text wire). Both mappings live in ./attachments.
    const codeMessage = buildCodeMessage(attachments);
    const images = attachmentsToImageBlocks(attachments);
    if (!text && codeMessage === '' && images.length === 0) return;

    // Intercept Web-supported slash commands when there are no attachments.
    // Command selection only drafts; this path is the real dispatch. Unknown
    // slashes (parseSupportedCommand → null) fall through as normal prompts.
    // Intercepted commands NEVER get an optimistic user bubble.
    if (kind === 'prompt' && text && attachments.length === 0) {
      const parsed = parseSupportedCommand(text);
      if (parsed) {
        const action = resolveSlashAction(parsed.name, parsed.args);
        const sid = sessionIdRef.current;
        input.value = '';
        flushComposerResize(input);

        if (action.type === 'error') {
          toast(action.message, true);
          return;
        }

        if (action.type === 'code-review') {
          const openArgs: CodeReviewOpenArgs = {};
          if (action.from && action.to) {
            openArgs.from = action.from;
            openArgs.to = action.to;
          }
          setCodeReviewOpenArgs(openArgs);
          openPanel('code-review', { force: true });
          return;
        }

        if (action.type === 'compact') {
          const isSnap = action.mode === 'snap';
          const command: Record<string, unknown> = isSnap
            ? { type: 'snapcompact' }
            : {
                type: 'compact',
                ...(action.customInstructions
                  ? { customInstructions: action.customInstructions }
                  : {}),
              };
          if (sid) command.sessionId = sid;
          const label = isSnap ? 'Snapcompact' : 'Compact';
          sendCommand(command)
            .then((data) => {
              pushItemFor(sid, {
                kind: 'summary',
                id: nextId('s'),
                label: isSnap ? 'snapcompact' : 'compact',
                text: formatCompactReport(data, label),
              });
            })
            .catch((err: Error & { rpc?: boolean }) => {
              // Surface the RPC error as a visible summary bubble (and toast)
              // so /compact failures are actionable rather than silent.
              const message = err.message || String(err);
              pushItemFor(sid, {
                kind: 'summary',
                id: nextId('s'),
                label: isSnap ? 'snapcompact' : 'compact',
                text: message,
              });
              toast(`${label} failed: ${message}`, true);
            });
          return;
        }

        if (action.type === 'skill') {
          const command: Record<string, unknown> = { type: 'skill', name: action.name };
          if (sid) command.sessionId = sid;
          sendCommand(command)
            .then((data) => {
              pushItemFor(sid, {
                kind: 'summary',
                id: nextId('s'),
                label: 'skill',
                text: formatSkillResult(data, action.name),
              });
            })
            .catch((err: Error & { rpc?: boolean }) => {
              const message = err.message || String(err);
              pushItemFor(sid, {
                kind: 'summary',
                id: nextId('s'),
                label: 'skill',
                text: message,
              });
              toast(`skill failed: ${message}`, true);
            });
          return;
        }
      }
    }

    // Code-file fences prepend the user's typed text; image-only sends render
    // the thumbnails themselves (no placeholder text). The bubble carries the
    // same image payloads as the prompt frame (minus the wire-only `type`
    // tag), so the optimistic bubble and the backend's persisted user message
    // render identically and reconcile without flicker.
    const message = [codeMessage, text].filter(Boolean).join('\n\n');
    const bubbleId = nextId('u');
    const sid = sessionIdRef.current;
    pushItemFor(sid, {
      kind: 'user',
      id: bubbleId,
      text: message,
      optimistic: true,
      ...(images.length > 0
        ? { images: images.map(({ data, mimeType }) => ({ mimeType, data })) }
        : {}),
    });
    if (sid) (optimisticQueueBySessionIdRef.current[sid] ??= []).push(bubbleId);
    input.value = '';
    flushComposerResize(input);
    // Capture the sent attachment ids BEFORE the async send so success clears
    // exactly this snapshot by id. A failed transport retains the chips and
    // budget (no clear), and a second intake arriving while the send is in
    // flight is preserved: the success clear filters by id, not array
    // equality, so concurrent additions stay in the queue.
    const sentIds =
      attachments.length > 0 ? new Set(attachments.map((a) => a.id)) : null;
    // Enter resolves to the active run's prompt/steer verb. The primary
    // button is intentionally different while streaming: it becomes Stop,
    // while Enter keeps the typed steering-message shortcut.
    const command: Record<string, unknown> =
      kind === 'steer' ? { type: 'steer', message } : { type: 'prompt', message };
    if (images.length > 0) command.images = images;
    sendCommand(command, bubbleId)
      .then(() => {
        // ACK received: clear only the sent snapshot by id, preserving any
        // concurrent additions, and reconcile the budget from the result.
        if (sentIds) {
          setAttachments((prev) => {
            const next = removeSentAttachments(prev, sentIds);
            intakeBudgetRef.current = reconcileIntakeBudget(next);
            return next;
          });
        }
      })
      .catch((err: Error & { rpc?: boolean }) => {
        // Failed transport: retain the chips and budget for retry.
        if (!err.rpc) toast(`send failed: ${err.message}`, true);
      });
  }, [attachments, flushComposerResize, openPanel, pushItemFor, sendCommand, toast]);

  const abortActiveRun = useCallback(() => {
    const sid = sessionIdRef.current;
    if (!sid || !streamingBySessionIdRef.current[sid]) return;
    abortPendingBySessionIdRef.current[sid] = true;
    sendCommand({ type: 'abort' }).catch(() => {
      abortPendingBySessionIdRef.current[sid] = false;
    });
  }, [sendCommand]);

  /* ---------------- composer: file attachments ---------------- */

  // Unified attachment intake for all three paths (paste / drop / file
  // picker): classify against per-file/aggregate/count limits, read the
  // accepted files in intake order (Promise.all preserves order regardless of
  // async completion), then queue the built attachments and toast every skip —
  // both the synchronous classification rejects and the late read rejects
  // (invalid UTF-8 / unreadable).
  //
  // CONCURRENCY INVARIANT: `attachments` state is a closure snapshot, so two
  // intakes firing before a re-render (e.g. paste while a drop's reads are in
  // flight) would both classify against the same stale budget and could bypass
  // the front-end caps. `intakeBudgetRef` is the authoritative concurrent
  // budget: it is read for classification, the accepted files' wire footprints
  // are RESERVED into it synchronously BEFORE the await, and it is reconciled
  // from the actual `attachments` state inside each setAttachments updater
  // (so remove/send/reset keep it consistent and late skips are released). The
  // backend PAYLOAD_TOO_LARGE frame limit remains the hard backstop.
  const onFilesChosen = useCallback(
    async (fileList: FileList | null) => {
      if (!fileList || fileList.length === 0) return;
      const budget = intakeBudgetRef.current;
      const plan = classifyAttachments(Array.from(fileList), {
        currentCount: budget.count,
        currentWire: budget.wire,
      });
      // Reserve synchronously so a concurrent intake sees the updated budget.
      for (const a of plan.accepted) {
        budget.wire += wireFootprint(a.kind, a.file.size);
        budget.count += 1;
      }
      const skips = [...plan.skipped];
      if (plan.accepted.length > 0) {
        let built: ComposerAttachment[] = [];
        try {
          const results = await readAttachmentsInOrder(plan.accepted, readAccepted);
          for (const r of results as ReadResult[]) {
            if (r.attachment) built.push(r.attachment);
            else if (r.skip) skips.push(r.skip);
          }
        } catch {
          toast('failed to read one or more attachments', true);
        }
        // Always reconcile from the actual state — releases late-skip and
        // read-failure reservations so a fully-invalid batch never leaves a
        // sticky reserved budget. Returning the same `prev` reference when no
        // files built lets React bail out of the re-render while still fixing
        // the authoritative concurrent budget.
        setAttachments((prev) => {
          const next = built.length > 0 ? [...prev, ...built] : prev;
          intakeBudgetRef.current = reconcileIntakeBudget(next);
          return next;
        });
      }
      const summary = formatSkipSummary(skips);
      if (summary) toast(summary, true);
    },
    [toast],
  );
  const onComposerPaste = useCallback(
    (event: React.ClipboardEvent<HTMLTextAreaElement>) => {
      const files = event.clipboardData.files;
      if (files.length > 0) {
        event.preventDefault();
        void onFilesChosen(files);
        return;
      }

      const plan = planLargeTextPaste(event.clipboardData.getData('text/plain'));
      if (plan.type === 'native') return;

      event.preventDefault();
      if (plan.type === 'oversize') {
        toast(`pasted text exceeds the attachment limit (${plan.size} bytes)`, true);
        return;
      }

      const budget = intakeBudgetRef.current;
      const classified = classifyAttachments(
        [{ type: plan.attachment.mimeType, name: plan.attachment.name, size: plan.attachment.size }],
        { currentCount: budget.count, currentWire: budget.wire },
      );
      if (classified.accepted.length === 0) {
        const summary = formatSkipSummary(classified.skipped);
        toast(summary ?? 'pasted text could not be attached', true);
        return;
      }

      setAttachments((prev) => {
        const next = [...prev, plan.attachment];
        intakeBudgetRef.current = reconcileIntakeBudget(next);
        return next;
      });
      toast(`large paste attached as ${PASTED_TEXT_ATTACHMENT_NAME}`);
    },
    [onFilesChosen, toast],
  );


  const removeAttachment = useCallback((id: string) => {
    setAttachments((prev) => {
      const next = prev.filter((attachment) => attachment.id !== id);
      intakeBudgetRef.current = reconcileIntakeBudget(next);
      return next;
    });
  }, []);

  /* ---------------- composer: hold-to-talk voice ---------------- */

  const transcribeAudio = useCallback(
    async (audioBlob: Blob) => {
      setTranscribing(true);
      try {
        // The backend `stt_transcribe` RPC accepts ONLY the WAV container the
        // browser converts first — never the recorder's native container, and
        // never a URL or API key (both stay server-side; the browser sends
        // only the bounded audio and the fixed MIME type).
        const wav = await blobToWav(audioBlob);
        if (!wav) {
          toast('transcription failed: recording could not be converted to WAV', true);
          return;
        }
        const bytes = new Uint8Array(await wav.arrayBuffer());
        const audioBase64 = wavToBase64(bytes);
        if (audioBase64 === null) {
          toast(`transcription failed: recording exceeds the ${STT_MAX_WAV_BYTES}-byte WAV cap`, true);
          return;
        }
        const raw = (await sendCommand({ type: 'stt_transcribe', audioBase64, mimeType: STT_WAV_MIME })) as
          | { text?: unknown }
          | null
          | undefined;
        const text = raw && typeof raw === 'object' && typeof raw.text === 'string' ? raw.text : '';
        // The transcript lands in the composer for review before sending.
        const input = promptInputRef.current;
        if (input && text) {
          const current = input.value;
          const separator = current.trim() ? (current.endsWith('\n') ? '' : '\n') : '';
          input.value = `${current}${separator}${text}`;
          autoResize(input);
          input.focus();
        } else if (!text) {
          toast('transcription failed: no transcript returned', true);
        }
      } catch (err) {
        // Backend errors are bounded and redacted (the server holds the STT
        // credentials); surface them verbatim.
        toast(`transcription failed: ${err instanceof Error ? err.message : String(err)}`, true);
      } finally {
        setTranscribing(false);
      }
    },
    [sendCommand, toast, autoResize]
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
    // Early gate on the backend's configured-ness: when the backend
    // EXPLICITLY reports STT is not configured, fail with an actionable
    // toast BEFORE requesting mic permission / recording (a capture could
    // only be rejected by the backend anyway). A missing live block (older
    // backend) stays conservative and lets the backend decide.
    if (liveSettings && liveSettings.sttConfigured !== true) {
      toast(
        'Live voice is not configured — set Settings.live.sttBaseUrl and sttApiKey (and sttModel) in the terminal',
        true
      );
      return;
    }
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
      // Safety cap: auto-release a stuck press just BEFORE the backend's
      // 30-second utterance bound (the setTimeout can fire late under
      // scheduler pressure; a capture past 30 s would exceed the decoded-size
      // cap and be rejected) instead of recording forever.
      recordingTimerRef.current = window.setTimeout(() => stopRecording(), STT_AUTO_RELEASE_MS);
    } catch (err) {
      if (mediaStreamRef.current) {
        mediaStreamRef.current.getTracks().forEach((track) => track.stop());
        mediaStreamRef.current = null;
      }
      toast(`microphone unavailable: ${err instanceof Error ? err.message : String(err)}`, true);
    }
  }, [recording, transcribing, stopRecording, transcribeAudio, toast, liveSettings]);

  /* ---------------- composer: realtime voice (Codex Live WebRTC) ---------------- */

  /** True when the backend advertises live.enabled and realtime mode (trim +
   *  ASCII lowercase, centralized in realtime.ts). A missing/disabled live
   *  block never selects realtime, and applyState clears stale liveSettings
   *  when runtimeSettings omits live. */
  const isRealtimeMode = isRealtimeLiveMode(liveSettings);

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
  /** Insert a selected slash command into the composer textarea and focus it
   *  WITHOUT submitting — the user confirms and Main dispatches on Enter. The
   *  caret lands at the end so a param-requiring command's trailing space is
   *  immediately typeable. appendDraft preserves existing draft text (own
   *  line, no double newline), mirroring commitTranscriptToComposer's rule. */
  const insertCommandText = useCallback(
    (text: string) => {
      const input = promptInputRef.current;
      if (!input || !text) return;
      input.value = appendDraft(input.value, text);
      autoResize(input);
      const end = input.value.length;
      input.setSelectionRange(end, end);
      input.focus();
    },
    [autoResize]
  );
  /** Stage a picked SKILL candidate in the composer WITHOUT submitting: insert
   *  `/skill <name>`, focus the textarea, and SELECT the whole draft so the
   *  ready state is visible (pressing Enter dispatches, typing replaces the
   *  selection). A toast states the exact draft + Enter-to-run action — the
   *  picker never auto-submits, so this is the explicit confirmation cue. */
  const stageSkillCommandText = useCallback(
    (text: string) => {
      const input = promptInputRef.current;
      if (!input || !text) return;
      input.value = appendDraft(input.value, text);
      autoResize(input);
      const start = input.value.length - text.length;
      input.setSelectionRange(start, input.value.length);
      input.focus();
      toast(`${text} ready — press Enter to run`);
    },
    [autoResize, toast]
  );
  /** `oai-events` data-channel event dispatch: USER input transcript deltas
   *  stream into the overlay, a final transcript commits to the composer
   *  (deduped across the two V1 final-event variants), delegations get a
   *  notification row, errors surface as toasts. Transport-agnostic — the
   *  handler parses JSON frames regardless of transport; the direct sideband
   *  WebSocket was removed (it could carry no browser Bearer header and would
   *  mixed-content-block on HTTPS→http), so events now arrive over the
   *  `oai-events` RTCDataChannel. Assistant OUTPUT transcript events are
   *  intentionally NOT routed here (classifyInputTranscriptEvent returns null
   *  for them) so they can never be committed to the composer as a user
   *  draft; output audio rides the WebRTC track, so output_audio.delta is a
   *  no-op. */
  const handleRealtimeFrame = useCallback(
    (frame: Record<string, unknown>) => {
      const type = typeof frame.type === 'string' ? frame.type : '';
      // USER input transcript (CLIProxyAPI aliases + V1 conversation API).
      // Deltas append to the overlay; finals commit the authoritative
      // utterance to the composer, deduped so the two V1 final variants for
      // one utterance do not double-commit.
      const cls = classifyInputTranscriptEvent(type);
      if (cls === 'delta') {
        const delta = firstString(frame.delta, frame.transcript);
        if (!delta) return;
        realtimeTranscriptRef.current += delta;
        const node = realtimeTranscriptNodeRef.current;
        if (node) node.textContent = realtimeTranscriptRef.current;
        return;
      }
      if (cls === 'final') {
        const text = finalTranscriptText(frame);
        const commit = nextInputTranscriptCommit(text, lastCommittedTranscriptRef.current);
        if (commit) {
          lastCommittedTranscriptRef.current = commit;
          commitTranscriptToComposer(commit);
        }
        return;
      }
      switch (type) {
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
          // V1 nests the detail under `error.message`/`error.code`; fall back
          // to the top-level alias fields. Always surface a bounded message so
          // a configured-but-failed session is never silently swallowed.
          toast(realtimeErrorMessage(frame), true);
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

  /** Tear down the realtime call: close the `oai-events` data channel, close
   *  the RTCPeerConnection, stop the mic tracks, and tell the backend the
   *  session is over. Idempotent; `silent` skips the realtime_stop RPC (unmount
   *  path, where the main socket is already gone). Nulling the channel/pc refs
   *  first means a late data-channel onclose/onerror is a no-op (no duplicate
   *  toast, no re-entrant teardown). */
  const stopRealtime = useCallback(
    (opts?: { silent?: boolean }) => {
      const hadSession = realtimePcRef.current !== null || realtimeDcRef.current !== null;
      const dc = realtimeDcRef.current;
      realtimeDcRef.current = null;
      if (dc) {
        dc.onopen = null;
        dc.onmessage = null;
        dc.onerror = null;
        dc.onclose = null;
        try {
          dc.close();
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
      lastCommittedTranscriptRef.current = '';
      const node = realtimeTranscriptNodeRef.current;
      if (node) node.textContent = '';
      setRealtimeDelegation(null);
      setRealtimeActive(false);
      setRealtimeConnState(null);
      setRealtimeAudioBlocked(false);
      if (hadSession && !opts?.silent) {
        sendCommand({ type: 'realtime_stop' }).catch(() => {});
      }
    },
    [sendCommand]
  );

  /** Start a Codex Live realtime call: mic track -> RTCPeerConnection ->
   *  `oai-events` RTCDataChannel (created BEFORE the offer so it is negotiated
   *  in the SDP) -> SDP offer over the realtime_create_call RPC -> answer.
   *  session.update + server events (transcript/delegation/error) flow over
   *  the data channel, which rides the WebRTC DTLS transport — no browser-side
   *  Bearer header and no HTTPS/http mixed-content block (the reasons the
   *  direct sideband WebSocket was removed). Incoming audio rides the WebRTC
   *  track. */
  const startRealtime = useCallback(async () => {
    if (realtimeActive || realtimeBusyRef.current) return;
    if (liveSettings?.realtimeConfigured !== true) {
      toast(
        'Realtime voice is not configured — set Settings.live.realtimeBaseUrl and realtimeApiKey (and realtimeModel/voice) in the terminal',
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
    realtimeBusyRef.current = true;
    try {
      await setupRealtimeCall({
        getUserMedia: () =>
          navigator.mediaDevices.getUserMedia({ audio: true }).then((s) => {
            mediaStreamRef.current = s;
            return s;
          }),
        createPeerConnection: () => new RTCPeerConnection(),
        sendCreateCall: (sdpOffer) =>
          sendCommand({ type: 'realtime_create_call', sdpOffer }).then((raw) => {
            const r = (raw || {}) as { sdp?: unknown; callId?: unknown };
            const sdp = typeof r?.sdp === 'string' ? r.sdp : '';
            const callId = typeof r?.callId === 'string' ? r.callId : '';
            return { sdp, callId };
          }),
        onPeerConnection: (pcArg) => {
          // Set the pc ref first so a partial-failure catch can release it.
          realtimePcRef.current = pcArg;
          setRealtimeConnState(classifyRealtimeConnectionState(pcArg.connectionState));
          pcArg.ontrack = (event) => {
            const audio = realtimeAudioRef.current;
            if (audio && event.streams.length > 0) {
              audio.srcObject = event.streams[0];
              // Autoplay can reject under the browser's autoplay policy (no
              // prior user gesture, or the gesture was "consumed" by another
              // play). Surface it as an actionable overlay control instead of
              // silently swallowing it — the user clicks #realtime-audio-resume
              // (a fresh gesture) to resume.
              void audio.play().catch(() => {
                if (realtimePcRef.current !== pcArg) return;
                setRealtimeAudioBlocked(true);
                toast('realtime audio blocked by autoplay — click the speaker to enable', true);
              });
            }
          };
          pcArg.onconnectionstatechange = () => {
            const bucket = classifyRealtimeConnectionState(pcArg.connectionState);
            setRealtimeConnState(bucket);
            // 'disconnected' is a transient ICE path loss (recoverable): toast
            // so the user knows audio/events may stall, but do NOT tear down —
            // ICE can re-connect on its own. 'failed' is terminal: toast and
            // tear the call down. 'closed' from a user-initiated stopRealtime
            // is a no-op (the ref is already null).
            if (bucket === 'disconnected') {
              toast('realtime connection interrupted — reconnecting', true);
            } else if (bucket === 'failed') {
              toast('realtime connection failed', true);
              if (realtimePcRef.current === pcArg) stopRealtime();
            } else if (bucket === 'closed') {
              if (realtimePcRef.current === pcArg) stopRealtime();
            } else if (bucket === 'connected') {
              // Connection (re)established — clear any stale interrupt toast
              // state by leaving the bucket visible; audio block stays as-is.
            }
          };
        },
        onDataChannel: (_pcArg, dcArg) => {
          // Set the dc ref first so a partial-failure catch / onclose can
          // release it; wiring happens before createOffer (the channel is
          // created inside setupRealtimeCall before the offer).
          realtimeDcRef.current = dcArg;
          dcArg.onopen = () => {
            // Re-advertise the full model-bearing V1 session config over the
            // data channel so the configured voice takes effect. The Rust
            // create-call POST uses the same nested audio shape but omits
            // model because the upstream create-call endpoint rejects it.
            // The legacy top-level {model, voice} shape was ignored.
            try {
              dcArg.send(JSON.stringify({ type: 'session.update', session: buildRealtimeSessionConfig(model, voice) }));
            } catch {
              /* channel closed between open and send; onclose handles teardown */
            }
          };
          dcArg.onmessage = (event) => {
            try {
              const frame = JSON.parse(String(event.data)) as Record<string, unknown>;
              if (frame && typeof frame === 'object') handleRealtimeFrame(frame);
            } catch {
              // Non-JSON frames are ignored.
            }
          };
          dcArg.onerror = () => {
            if (realtimeDcRef.current === dcArg) toast('realtime data channel error', true);
          };
          dcArg.onclose = () => {
            // An unexpected close mid-call tears the call down (idempotent: a
            // user-initiated stopRealtime nulls the ref first, so this no-ops).
            if (realtimeDcRef.current === dcArg) stopRealtime();
          };
        },
      });
      lastCommittedTranscriptRef.current = '';
      realtimeTranscriptRef.current = '';
      const node = realtimeTranscriptNodeRef.current;
      if (node) node.textContent = '';
      setRealtimeDelegation(null);
      setRealtimeAudioBlocked(false);
      setRealtimeActive(true);
    } catch (err) {
      // A partial setup leaves the pc/dc/mic refs set incrementally via the
      // setup callbacks; release them WITHOUT firing realtime_stop (silent —
      // the call never fully came up).
      stopRealtime({ silent: true });
      toast(`realtime call failed: ${err instanceof Error ? err.message : String(err)}`, true);
    } finally {
      realtimeBusyRef.current = false;
    }
  }, [realtimeActive, liveSettings, sendCommand, handleRealtimeFrame, toast, stopRealtime]);

  /** Resume remote audio after an autoplay-policy block. The overlay's
   *  #realtime-audio-resume button is a fresh user gesture, so play() succeeds
   *  where the initial ontrack play() rejected. Clears the blocked flag on
   *  success; a second rejection re-toasts so the user knows it still needs a
   *  gesture. No-op when no call is up or no audio element is wired. */
  const resumeRealtimeAudio = useCallback(() => {
    const audio = realtimeAudioRef.current;
    if (!audio || !realtimeActive) return;
    void audio.play().then(() => {
      setRealtimeAudioBlocked(false);
    }).catch(() => {
      toast('realtime audio still blocked — tap the speaker again', true);
    });
  }, [realtimeActive, toast]);

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

  // Restore the token for the INITIAL page authority and connect on boot. The
  // legacy global token (rpi-web-token) is migrated to the initial authority's
  // scoped key ONCE and the legacy key deleted — it never leaks to a different
  // host. The ref/state are set synchronously (including empty) so the very
  // first connection already uses the correct token.
  useEffect(() => {
    const saved = loadInitialAuthorityToken(tokenStorage, hostRef.current);
    tokenRef.current = saved;
    setToken(saved);
    connect();
    return () => {
      if (retryTimerRef.current !== null) window.clearTimeout(retryTimerRef.current);
      clearHeartbeatTimers();
      // Reject every pending ready-gate waiter so its bounded timer does not
      // leak past unmount and no load settles into a torn-down component.
      readyGateRef.current?.clear();
      // Drop any pending composer resize work (coalesced per animation frame).
      autoResizeRef.current?.cancel();
      // Release the mic/WebRTC/data-channel resources; the realtime_stop RPC is
      // skipped (silent) because the main socket is closing underneath us.
      stopRealtime({ silent: true });
      const ws = wsRef.current;
      wsRef.current = null;
      // Detach the main socket's handlers before close so a late onopen /
      // onmessage / onerror / onclose cannot fire post-unmount (e.g. a
      // CONNECTING socket whose onopen would otherwise bootstrap into a torn-
      // down component).
      if (ws) {
        detachTransportHandlers(ws);
        try {
          ws.close(1000, 'unload');
        } catch {
          /* already closed */
        }
      }
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Auto-scroll: keep pinned to the bottom while the user is at the bottom.
  // React item commits (new messages, patches, removals) grow/shrink the
  // transcript; follow only when the pin state is live. Async content growth
  // without a commit (images, markdown hydration) is handled by the
  // ResizeObserver inside useScrollPin.
  useEffect(() => {
    pinIfPinned();
  }, [activeItems, pinIfPinned]);

  // Session activation (the initial bootstrap included) pins the activated
  // session's transcript to the bottom UNCONDITIONALLY: a switch must never
  // inherit the previous session's scroll position or pin state. Deltas and
  // item commits for the newly active session keep it pinned from there.
  useEffect(() => {
    forcePin();
  }, [sessionId, forcePin]);

  // Keep the shared drawer CSS variable in sync with state (and the
  // initial localStorage read). Mobile CSS ignores the variable.
  useLayoutEffect(() => {
    applyPanelDrawerVh(panelDrawerVh);
  }, [panelDrawerVh]);

  const onPanelResizerPointerDown = useCallback((e: PointerEvent<HTMLDivElement>) => {
    if (e.button !== 0) return;
    // Desktop only — mobile never shows the resizer, but guard anyway.
    if (typeof window !== 'undefined' && window.matchMedia('(max-width: 768px)').matches) return;
    e.preventDefault();
    const target = e.currentTarget;
    target.setPointerCapture(e.pointerId);
    panelResizeDragRef.current = {
      startY: e.clientY,
      startVh: panelDrawerVhRef.current,
    };
  }, []);

  const onPanelResizerPointerMove = useCallback((e: PointerEvent<HTMLDivElement>) => {
    const drag = panelResizeDragRef.current;
    if (!drag) return;
    // Dragging the top edge UP increases height (negative dy → +vh).
    const dyPx = drag.startY - e.clientY;
    const dyVh = (dyPx / window.innerHeight) * 100;
    const next = clampPanelDrawerVh(drag.startVh + dyVh);
    panelDrawerVhRef.current = next;
    setPanelDrawerVh(next);
    applyPanelDrawerVh(next);
  }, []);

  const endPanelResizeDrag = useCallback((e: PointerEvent<HTMLDivElement>) => {
    if (!panelResizeDragRef.current) return;
    panelResizeDragRef.current = null;
    try {
      e.currentTarget.releasePointerCapture(e.pointerId);
    } catch {
      /* already released */
    }
    writeStoredPanelDrawerVh(panelDrawerVhRef.current);
  }, []);

  const onPanelResizerKeyDown = useCallback((e: KeyboardEvent<HTMLDivElement>) => {
    if (typeof window !== 'undefined' && window.matchMedia('(max-width: 768px)').matches) return;
    const step = e.shiftKey ? 10 : 2;
    let next: number | null = null;
    if (e.key === 'ArrowUp') next = clampPanelDrawerVh(panelDrawerVhRef.current + step);
    else if (e.key === 'ArrowDown') next = clampPanelDrawerVh(panelDrawerVhRef.current - step);
    else if (e.key === 'Home') next = PANEL_DRAWER_MAX_VH;
    else if (e.key === 'End') next = PANEL_DRAWER_MIN_VH;
    if (next == null) return;
    e.preventDefault();
    panelDrawerVhRef.current = next;
    setPanelDrawerVh(next);
    applyPanelDrawerVh(next);
    writeStoredPanelDrawerVh(next);
  }, []);

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
          waitForReady={waitForReady}
          onLifecycleResult={onLifecycleResult}
          activeSessionId={sessionId}
          unreadBySessionId={unreadBySessionId}
          onCollapse={() => setSidebarOpen(false)}
          onReopenRail={() => setSidebarOpen(true)}
          onOpenManage={() => {
            openPanel('session', { force: true });
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
                aria-label={activePanel === 'todo' ? 'Close Todos panel' : 'Open Todos panel'}
                onClick={() => openPanel('todo')}
              >
                Todos
              </button>
              <button
                id="goal-panel-btn"
                type="button"
                className={activePanel === 'goal' ? 'panel-toggle panel-toggle--open' : 'panel-toggle'}
                aria-pressed={activePanel === 'goal'}
                title="Goal panel (objective, status, token budget, pins, journal)"
                aria-label={activePanel === 'goal' ? 'Close Goal panel' : 'Open Goal panel'}
                onClick={() => openPanel('goal')}
              >
                Goal
              </button>
              <button
                id="workflow-toggle-btn"
                type="button"
                className={activePanel === 'workflow' ? 'panel-toggle panel-toggle--open' : 'panel-toggle'}
                aria-pressed={activePanel === 'workflow'}
                title="Workflow panel (list, detail, live workers, create/pause/resume/cancel/integrate/remove)"
                aria-label={activePanel === 'workflow' ? 'Close Workflows panel' : 'Open Workflows panel'}
                onClick={() => openPanel('workflow')}
              >
                Workflows
              </button>
              <button
                id="sidechat-toggle-btn"
                type="button"
                className={activePanel === 'sidechat' ? 'panel-toggle panel-toggle--open' : 'panel-toggle'}
                aria-pressed={activePanel === 'sidechat'}
                title="Side chat: parallel /btw sessions (own tab, transcript, prompt)"
                aria-label={activePanel === 'sidechat' ? 'Close Side chat panel' : 'Open Side chat panel'}
                onClick={() => openPanel('sidechat')}
              >
                Side chat
              </button>
              <button
                id="subagents-toggle-btn"
                type="button"
                className={activePanel === 'subagents' ? 'panel-toggle panel-toggle--open' : 'panel-toggle'}
                aria-pressed={activePanel === 'subagents'}
                title="Subagents: live jobs (id/type/status/activity/elapsed), spawn task, hub message, cancel, output view"
                aria-label={activePanel === 'subagents' ? 'Close Subagents panel' : 'Open Subagents panel'}
                onClick={() => openPanel('subagents')}
              >
                Subagents
              </button>
              <button
                id="personas-toggle-btn"
                type="button"
                className={activePanel === 'personas' ? 'panel-toggle panel-toggle--open' : 'panel-toggle'}
                aria-pressed={activePanel === 'personas'}
                title="Personas: persistent persona definitions (list/view/create/edit/remove/purge/select/run)"
                aria-label={activePanel === 'personas' ? 'Close Personas panel' : 'Open Personas panel'}
                onClick={() => openPanel('personas')}
              >
                Personas
              </button>
              <button
                id="session-toggle-btn"
                type="button"
                className={activePanel === 'session' ? 'panel-toggle panel-toggle--open' : 'panel-toggle'}
                aria-pressed={activePanel === 'session'}
                title="Session panel: current session info, new/switch/fork/clone/rename"
                aria-label={activePanel === 'session' ? 'Close Session panel' : 'Open Session panel'}
                onClick={() => openPanel('session')}
              >
                Session
              </button>
              <button
                id="settings-toggle-btn"
                type="button"
                className={activePanel === 'settings' ? 'panel-toggle panel-toggle--open' : 'panel-toggle'}
                aria-pressed={activePanel === 'settings'}
                title="Settings panel: browse by category, typed edit, draft/apply"
                aria-label={activePanel === 'settings' ? 'Close Settings panel' : 'Open Settings panel'}
                onClick={() => openPanel('settings')}
              >
                Settings
              </button>
            </>
          }
        />
        <div className="app-main">
      <main id="transcript" aria-live="polite" ref={transcriptRef} onScroll={onTranscriptScroll}>
        <div className="transcript-content" ref={transcriptContentRef}>
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
            case 'user': {
              const userImagesList = item.images ?? [];
              const analysis = item.analysis;
              const largeText = item.text !== '' ? largeTextDisplay(item.text) : null;
              return (
                <div key={item.id} className={`msg msg--user${item.optimistic ? ' optimistic' : ''}`}>
                  {userImagesList.length > 0 && (
                    <div className={`msg--user__images${userImagesList.length > 1 ? ' msg--user__images--grid' : ''}`}>
                      {userImagesList.map((image, index) => (
                        <img
                          key={`image-${index}`}
                          className="msg--user__image"
                          src={`data:${image.mimeType};base64,${image.data}`}
                          alt="Attached image"
                          loading="lazy"
                        />
                      ))}
                    </div>
                  )}
                  {largeText ? (
                    <details className="msg--user__large-text">
                      <summary>
                        Large message · {largeText.characters} characters · {largeText.bytes} bytes
                      </summary>
                      <pre>{largeText.preview}</pre>
                    </details>
                  ) : item.text !== '' ? (
                    <MarkdownBody className="msg--user__text" text={item.text} onLayoutChange={pinIfPinned} />
                  ) : null}
                  {analysis && (
                    <details className="msg--user__analysis">
                      <summary className="msg--user__analysis-summary">
                        <span className="msg--user__analysis-label">Image analysis</span>
                        <span className="msg--user__analysis-model">{safeText(analysis.model)}</span>
                      </summary>
                      <div className="msg--user__analysis-body">{safeText(analysis.description)}</div>
                    </details>
                  )}
                </div>
              );
            }
            case 'assistant':
              return item.status === 'streaming' ? (
                <StreamingAssistant key={item.id} sid={sessionId ?? ''} id={item.id} />
              ) : (
                <FinalAssistant key={item.id} blocks={item.blocks} onLayoutChange={pinIfPinned} />
              );
            case 'toolCard':
              return <ToolCard key={item.id} item={item} onLayoutChange={pinIfPinned} />;
            case 'toolResult':
              return (
                <BashCard
                  key={item.id}
                  label="tool output"
                  output={item.text === '' ? '(empty tool result)' : item.text}
                />
              );
            case 'bash':
              return <BashCard key={item.id} command={item.command} output={item.output} status={item.status} />;
            case 'custom':
              return (
                <div key={item.id} className="msg msg--custom" role="note">
                  <span className="msg--custom__label">{safeText(item.label)}</span>
                  <MarkdownBody className="msg--custom__text" text={item.text} onLayoutChange={pinIfPinned} />
                </div>
              );
            case 'irc':
              return <IrcCard key={item.id} item={item} onLayoutChange={pinIfPinned} />;
            case 'summary':
              return (
                <div key={item.id} className="msg msg--summary" role="note">
                  <span className="msg--summary__label">{safeText(item.label)}</span>
                  <MarkdownBody className="msg--summary__text" text={item.text} onLayoutChange={pinIfPinned} />
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
        </div>
      </main>

      {/* panel mount point — every panel is KEYED by (panel, session) so its
          internal drafts/controllers (todo form, goal objective, workflow
          create fields, side-chat prompt, settings draft, subagent message
          drafts) can never survive an A→B session switch as
          though they belonged to B. Each panel re-fetches from the backend
          for the active session on mount (sendCommand injects the top-level
          sessionId).

          Ordinary panels share ONE desktop height resizer (ARIA separator)
          mounted here — never per-panel handlers. Code review is excluded
          (full-viewport, owns its own thread-column resizer). */}
      {activePanel !== '' && activePanel !== 'code-review' && (
        <div
          id="panel-drawer-resizer"
          className="panel-drawer-resizer"
          role="separator"
          aria-orientation="horizontal"
          aria-label="Resize panel height"
          aria-valuemin={PANEL_DRAWER_MIN_VH}
          aria-valuemax={PANEL_DRAWER_MAX_VH}
          aria-valuenow={Math.round(panelDrawerVh)}
          tabIndex={0}
          onPointerDown={onPanelResizerPointerDown}
          onPointerMove={onPanelResizerPointerMove}
          onPointerUp={endPanelResizeDrag}
          onPointerCancel={endPanelResizeDrag}
          onKeyDown={onPanelResizerKeyDown}
        />
      )}
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
      {activePanel === 'personas' && (
        <PersonasPanel
          key={`personas:${sessionId ?? ''}`}
          sendCommand={sendCommand}
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
      {activePanel === 'session' && (
        <SessionPanel
          key={`session:${sessionId ?? ''}`}
          sendCommand={sendCommand}
          waitForReady={waitForReady}
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
      {activePanel === 'code-review' && (
        <CodeReviewPanel
          key={`code-review:${sessionId ?? ''}`}
          sendCommand={sendCommand}
          sessionId={sessionId}
          openArgs={codeReviewOpenArgs}
          onClose={() => {
            setActivePanel('');
            setCodeReviewOpenArgs({});
          }}
        />
      )}

      <footer
        data-drop-active={dropActive ? 'true' : undefined}
        onDragEnter={(e) => {
          // Only file drags activate the drop target; text-drag defaults pass
          // through to the textarea. Some Chromium synthetic DataTransfers
          // expose files but omit the `Files` type, so accept either signal.
          if (e.dataTransfer.files.length === 0 && !Array.from(e.dataTransfer.types).includes('Files')) return;
          e.preventDefault();
          // Depth counter: dragenter/dragleave fire on every child element, so
          // counting entries vs leaves keeps the highlight stable while the
          // pointer moves across the composer's children.
          dragDepthRef.current += 1;
          setDropActive(true);
        }}
        onDragOver={(e) => {
          if (e.dataTransfer.files.length === 0 && !Array.from(e.dataTransfer.types).includes('Files')) return;
          // preventDefault on dragover is required for drop to fire.
          e.preventDefault();
          e.dataTransfer.dropEffect = 'copy';
        }}
        onDragLeave={(e) => {
          if (e.dataTransfer.files.length === 0 && !Array.from(e.dataTransfer.types).includes('Files')) return;
          e.preventDefault();
          dragDepthRef.current = Math.max(0, dragDepthRef.current - 1);
          if (dragDepthRef.current === 0) setDropActive(false);
        }}
        onDrop={(e) => {
          if (e.dataTransfer.files.length === 0 && !Array.from(e.dataTransfer.types).includes('Files')) return;
          e.preventDefault();
          dragDepthRef.current = 0;
          setDropActive(false);
          void onFilesChosen(e.dataTransfer.files);
        }}
      >
        {dropActive && (
          <div className="composer-drop" aria-hidden="true">
            <span className="composer-drop__hint">Drop images or code files to attach</span>
          </div>
        )}
        <div id="composer-main">
          {attachments.length > 0 && (
            <div id="composer-attachments" aria-label="Attached files">
              {attachments.map((attachment) => (
                <span key={attachment.id} className="composer-attachment" title={attachment.name}>
                  {attachment.kind === 'image' && attachment.previewUrl ? (
                    <img className="composer-attachment__thumb" src={attachment.previewUrl} alt="" />
                  ) : (
                    <span className="composer-attachment__badge">
                      {codeBadgeLabel(attachment.name)}
                    </span>
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
              {realtimeConnState && (
                <div id="realtime-conn-state" className="realtime-transcript__conn" data-state={realtimeConnState}>
                  {realtimeConnState}
                </div>
              )}
              {realtimeAudioBlocked && (
                <button
                  id="realtime-audio-resume"
                  type="button"
                  className="realtime-transcript__audio-resume"
                  onClick={resumeRealtimeAudio}
                  title="Enable remote audio"
                >
                  🔊 click to enable audio
                </button>
              )}
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
          <div id="composer-row">
            <CommandPicker
              connected={connState === 'on'}
              sendCommand={sendCommand}
              onSelect={insertCommandText}
              onSkillSelect={stageSkillCommandText}
              onError={(message) => toast(message, true)}
              getComposerValue={() => promptInputRef.current?.value ?? ''}
            />
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
                  abortActiveRun();
                }
              }}
              onInput={(e) => autoResize(e.currentTarget)}
              // File paste uses the normal attachment reader. Large plain-text
              // paste becomes a text attachment so Chromium never lays out
              // millions of textarea glyphs on the main thread.
              onPaste={onComposerPaste}
            />
        <div id="composer-buttons">
          <button
            id="attach-btn"
            type="button"
            title="Attach images or code/text files"
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
          <button
            id="send-btn"
            type="button"
            className={activeStreaming ? 'composer-action composer-action--stop' : 'composer-action composer-action--send'}
            aria-label={activeStreaming ? 'Stop generating' : 'Send message'}
            title={activeStreaming ? 'Stop generating (Esc)' : 'Send message (Enter)'}
            onClick={activeStreaming ? abortActiveRun : () => submit('prompt')}
          >
            <span aria-hidden="true">{activeStreaming ? '■' : '➤'}</span>
            <span className="composer-action__label">{activeStreaming ? 'Stop' : 'Send'}</span>
          </button>
        </div>
        </div>
        </div>
      </footer>

      <input
        ref={fileInputRef}
        type="file"
        accept={attachmentAccept()}
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
