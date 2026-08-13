import { useCallback, useEffect, useRef, useState } from 'react';
import { useScrollPin } from './scrollPin';
import { createAutoResizeController, type AutoResizeController } from './autoResize';
import { safeText } from './redact';
import {
  type Item,
  messagesToItems,
  nextId,
  contentText,
  applyDeltaToNode,
  streamKey,
  streamBuf,
  normalizedThinkingKeys,
  StreamingAssistant,
  FinalAssistant,
  ToolCard,
  BashCard,
  IrcCard,
  ToastList,
} from './App';
import { MarkdownBody } from './MarkdownBody';
import type { ContentBlock } from './types';
import {
  boundOutput,
  customToItem,
  applyToolSnapshot,
  applyToolResultToItems,
  toolMedia,
  finalizeStreamingAssistant,
  BASH_OUTPUT_LINE_LIMIT,
  TOOL_OUTPUT_LINE_LIMIT,
  userMessageProjection,
} from './transcript';
import {
  CollabGuest,
  type ParsedCollabLink,
  type CollabSnapshot,
  type CollabEventFrame,
  type CollabResponse,
  type CollabConnState,
} from './collab';

/** Unwrap a collab snapshot's recorder entries into the raw `pi_ai::Message`
 *  records `messagesToItems` expects. `SessionEntry` carries
 *  `message: Option<Message>`; metadata entries (e.g. a thinking-level change)
 *  carry no message and are filtered out — one bad record never breaks the
 *  transcript restore. */
function snapshotToItems(snapshot: CollabSnapshot): Item[] {
  const entries = Array.isArray(snapshot.entries) ? snapshot.entries : [];
  const messages = entries
    .map((e) => (e as { message?: unknown } | null | undefined)?.message)
    .filter((m): m is NonNullable<typeof m> => m != null);
  return messagesToItems(messages);
}

/** A toast surfaced to the guest. */
type GuestToast = { id: string; message: string; error: boolean };

export function CollabGuestView({ link }: { link: ParsedCollabLink | null }) {
  // main.tsx parsed the capability fragment exactly once and scrubbed it from
  // the address bar/history before this component mounts. The decoded key is
  // retained only in this in-memory value for the encrypted connection.
  // `link` is null exactly when the document is a collab guest route but the
  // capability fragment failed to parse (or is absent): the early return below
  // renders the safe malformed-link error — the connection effect, composer,
  // and abort all guard on `link` already, and useScrollPin's null-tolerant
  // controller makes the transcript effects no-ops instead of crashes.
  const [status, setStatus] = useState<CollabConnState>('connecting');
  const [items, setItems] = useState<Item[]>([]);
  const [streaming, setStreaming] = useState(false);
  const [toasts, setToasts] = useState<GuestToast[]>([]);

  const guestRef = useRef<CollabGuest | null>(null);
  const sidRef = useRef<string | null>(null);
  const assistantIdRef = useRef<string>('');
  const abortPendingRef = useRef(false);
  const optimisticQueueRef = useRef<string[]>([]);
  // Same coherent transcript scroll-pin state as the host view (see
  // ./scrollPin): `transcriptRef` is the scroll container, `transcriptContentRef`
  // the resize-observed content wrapper.
  const { transcriptRef, transcriptContentRef, onTranscriptScroll, pinIfPinned } = useScrollPin();
  const promptInputRef = useRef<HTMLTextAreaElement>(null);
  // Composer auto-resize controller (see ./autoResize): per-keystroke work is
  // coalesced to one layout pass per animation frame. Declared here so the
  // unmount cleanup below can cancel pending work.
  const autoResizeRef = useRef<AutoResizeController<HTMLTextAreaElement> | null>(null);

  const toast = useCallback((message: string, error = false) => {
    const id = nextId('t');
    setToasts((prev) => [...prev.slice(-4), { id, message: safeText(message), error }]);
    window.setTimeout(() => {
      setToasts((prev) => prev.filter((t) => t.id !== id));
    }, 7000);
  }, []);

  /* ---------------- single-session transcript dispatch ---------------- */
  //
  // The host projects the SAME /ws event frames (message_start /
  // tool_execution_start / run_failed / extension_ui_request / ...) the
  // normal client already consumes, just sealed into collab frames. This
  // dispatch mirrors App's per-session handlers for ONE session (the host
  // session id from the snapshot), reusing the shared streaming-node
  // infrastructure (applyDeltaToNode / streamBuf) so live deltas render
  // exactly as in the host's own transcript.

  const pushItem = useCallback((item: Item) => {
    setItems((prev) => [...prev, item]);
  }, []);

  const patchItem = useCallback((id: string, patch: (item: Item) => Item) => {
    setItems((prev) => prev.map((item) => (item.id === id ? patch(item) : item)));
  }, []);

  const updateItems = useCallback((updater: (prev: Item[]) => Item[]) => {
    setItems(updater);
  }, []);

  const removeItem = useCallback((id: string) => {
    setItems((prev) => prev.filter((item) => item.id !== id));
  }, []);

  const confirmAllOptimistic = useCallback(() => {
    optimisticQueueRef.current = [];
    updateItems((prev) => {
      if (!prev.some((i) => i.kind === 'user' && i.optimistic)) return prev;
      return prev.map((i) => (i.kind === 'user' && i.optimistic ? { ...i, optimistic: false } : i));
    });
  }, [updateItems]);

  const finalizeActiveAssistant = useCallback(() => {
    const targetId = assistantIdRef.current;
    if (targetId === '') return;
    const key = streamKey(sidRef.current, targetId);
    const streamedText = streamBuf.get(key)?.text ?? '';
    streamBuf.delete(key);
    updateItems((prev) => finalizeStreamingAssistant(prev, targetId, streamedText));
  }, [updateItems]);

  const onBashExecutionEnd = useCallback((frame: CollabEventFrame) => {
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
    pushItem({
      kind: 'bash',
      id: nextId('b'),
      command: message.command || '',
      output: boundOutput(message.output || '', BASH_OUTPUT_LINE_LIMIT).text,
      ...(status ? { status } : {}),
    });
  }, [pushItem]);
  const onMessageStart = useCallback((frame: CollabEventFrame) => {
    const message = (frame.message || {}) as {
      role?: string;
      content?: unknown;
      toolCallId?: string;
      details?: unknown;
    };

    switch (message.role) {
      case 'user': {
        const projected = userMessageProjection(message.content);
        const queueId = optimisticQueueRef.current.shift();
        if (queueId) {
          patchItem(queueId, (item) => (item.kind === 'user' ? { ...item, optimistic: false } : item));
        } else {
          pushItem({
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
      case 'assistant': {
        setStreaming(true);
        const id = nextId('a');
        assistantIdRef.current = id;
        pushItem({ kind: 'assistant', id, status: 'streaming', blocks: [] });
        return;
      }
      case 'toolResult': {
        // Host parity (App.tsx onMessageStart toolResult): the toolCard
        // (tool_execution_start/end) already renders this result inline, so
        // suppress the separate toolResult row when a matching card exists —
        // the guest transcript must never show one tool's output twice
        // (restored transcripts merge the same way in messagesToItems).
        // Unmatched results stay readable; details fold onto a matching card.
        const text = boundOutput(contentText(message.content), TOOL_OUTPUT_LINE_LIMIT).text;
        const toolCallId = typeof message.toolCallId === 'string' ? message.toolCallId : '';
        const media = toolMedia(message.content, message.details);
        updateItems((prev) => applyToolResultToItems(prev, toolCallId, text, message.details, media));
        return;
      }
      case 'bashExecution': {
        const m = message as { command?: string; output?: string; exitCode?: number | null; cancelled?: boolean };
        const bashStatus: 'done' | 'error' | undefined =
          m.cancelled === true || (typeof m.exitCode === 'number' && m.exitCode !== 0)
            ? 'error'
            : typeof m.exitCode === 'number'
              ? 'done'
              : undefined;
        pushItem({
          kind: 'bash',
          id: nextId('b'),
          command: m.command || '',
          output: boundOutput(m.output || '', BASH_OUTPUT_LINE_LIMIT).text,
          ...(bashStatus ? { status: bashStatus } : {}),
        });
        return;
      }
      case 'custom': {
        // Mirrors messagesToItems so the guest transcript matches the host:
        // display:false customs never render; display:true customs surface as
        // a labeled card, with typed IRC customs showing their parsed view.
        const item = customToItem(
          message as { display?: boolean; customType?: string; content?: unknown; details?: unknown },
        );
        if (item) pushItem(item);
        return;
      }
      default:
        return;
    }
  }, [patchItem, pushItem, updateItems]);

  const onMessageUpdate = useCallback((frame: CollabEventFrame) => {
    const ev = (frame.assistantMessageEvent || {}) as { type?: string; delta?: string };
    if (!ev.type) return;
    const sid = sidRef.current;
    const targetId = assistantIdRef.current;
    if (!targetId) return;
    setStreaming(true);
    // Namespaced buffer keyed by (sid, item id) — reusing the shared streaming
    // registry so deltas land in the mounted node exactly as in the host.
    const key = streamKey(sid, targetId);
    let buf = streamBuf.get(key);
    if (!buf) {
      buf = { text: '', thinking: '' };
      streamBuf.set(key, buf);
    }
    if (ev.type === 'text_delta' && ev.delta) {
      buf.text += ev.delta;
      applyDeltaToNode(sid, targetId, ev.delta, 'text');
    } else if (ev.type === 'thinking_delta' && ev.delta) {
      buf.thinking += ev.delta;
      applyDeltaToNode(sid, targetId, ev.delta, 'thinking');
    }
    // toolcall_delta (raw tool-call JSON fragments) is deliberately dropped,
    // matching the host transcript: the guest's structured tool card is
    // driven by the tool_execution_* events (onToolStart/Update/End).
    // Follow the stream synchronously with the delta, exactly like the host
    // transcript (the pin state decides whether the view moves at all).
    pinIfPinned();
    // start/end/done/error: message_end re-renders authoritatively.
  }, [pinIfPinned]);

  const onMessageEnd = useCallback((frame: CollabEventFrame) => {
    const message = (frame.message || {}) as { role?: string; content?: ContentBlock[] };
    if (message.role !== 'assistant') return;
    const sid = sidRef.current;
    const targetId = assistantIdRef.current;
    if (!targetId) return;
    // Aborting right after turn_start can yield an authoritative message with
    // empty content while deltas already relayed; fall back to the streamed
    // buffer so watched text is never wiped by the final render.
    const key = streamKey(sid, targetId);
    const buf = streamBuf.get(key);
    const hasRenderable = Array.isArray(message.content)
      ? (message.content as ContentBlock[]).some(
          (b) => b && ((b.type === 'text' && b.text) || (b.type === 'thinking' && b.thinking)),
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
    patchItem(targetId, (item) =>
      item.kind === 'assistant' ? { ...item, status: 'final' as const, blocks } : item,
    );
  }, [patchItem]);

  const onToolStart = useCallback((frame: CollabEventFrame) => {
    setStreaming(true);
    pushItem({
      kind: 'toolCard',
      id: nextId('tc'),
      toolCallId: (frame.toolCallId as string) || '',
      toolName: safeText(frame.toolName || 'tool'),
      args: frame.args,
      status: 'running',
      result: '',
    });
  }, [pushItem]);

  const onToolUpdate = useCallback((frame: CollabEventFrame) => {
    const toolCallId = (frame.toolCallId as string) || '';
    const text = contentText((frame.partialResult as { content?: unknown } | undefined)?.content);
    if (!text) return;
    updateItems((prev) => applyToolSnapshot(prev, toolCallId, text));
  }, [updateItems]);

  const onToolEnd = useCallback((frame: CollabEventFrame) => {
    const toolCallId = (frame.toolCallId as string) || '';
    const rawResult = frame.result;
    let content: unknown;
    let details: unknown;
    if (rawResult && typeof rawResult === 'object') {
      if ('content' in rawResult) content = rawResult.content;
      if ('details' in rawResult) details = rawResult.details;
    }
    const text = contentText(content);
    const media = toolMedia(content, details);
    const isError = !!frame.isError;
    updateItems((prev) => applyToolSnapshot(prev, toolCallId, text, isError ? 'error' : 'done', details, media));
  }, [updateItems]);

  /** Projected extension UI requests. Interactive asks (confirm/
   *  input/select/editor) cannot be answered remotely by design — for ALL
   *  guests (control included) they surface as a read-only notice card pointing
   *  to the terminal. Notifications become toasts. Host-only approval/lifecycle
   *  operations are never actionable from the guest view. */
  const onExtensionUiRequest = useCallback((frame: CollabEventFrame) => {
    const method = typeof frame.method === 'string' ? frame.method : '';
    const title = typeof frame.title === 'string' ? frame.title : '';
    const message = typeof frame.message === 'string' ? frame.message : '';
    const extensionId = typeof frame.extensionId === 'string' ? frame.extensionId : undefined;
    if (['confirm', 'input', 'select', 'editor'].includes(method)) {
      pushItem({
        kind: 'approval',
        id: nextId('ap'),
        method,
        title: title || method,
        message: message || title || '',
        extensionId,
      });
      toast(`Approval needed (${method}${extensionId ? ` · ${extensionId}` : ''}) — answer in the terminal`);
    } else if (method === 'notify') {
      toast(title || message || 'extension notification');
    }
  }, [pushItem, toast]);

  const onEvent = useCallback((frame: CollabEventFrame) => {
    switch (frame.type) {
      case 'turn_start':
        setStreaming(true);
        abortPendingRef.current = false; // a new run starts fresh
        break;
      case 'turn_end':
        break; // flat transcript; nothing to close
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
        confirmAllOptimistic();
        setStreaming(false);
        finalizeActiveAssistant();
        break;
      case 'run_failed':
        confirmAllOptimistic();
        setStreaming(false);
        finalizeActiveAssistant();
        if (abortPendingRef.current) {
          abortPendingRef.current = false;
          toast('run aborted');
        } else {
          toast(typeof frame.message === 'string' ? frame.message : 'run failed', true);
        }
        break;
      case 'extension_ui_request':
        onExtensionUiRequest(frame);
        break;
      default:
        break; // panel-only events (todo/goal/workflow/subagents) have no surface here
    }
  }, [
    confirmAllOptimistic,
    finalizeActiveAssistant,
    onMessageStart,
    onMessageUpdate,
    onMessageEnd,
    onToolStart,
    onBashExecutionEnd,
    onToolUpdate,
    onToolEnd,
    onExtensionUiRequest,
    toast,
  ]);

  /* ---------------- collab connection lifecycle ---------------- */

  const onResponse = useCallback((resp: CollabResponse) => {
    // A failed prompt response means the host refused it (e.g. busy): remove
    // the optimistic bubble the response's echoed id points at, if any.
    if (!resp.success) {
      if (resp.command === 'prompt' && typeof resp.id === 'string' && resp.id) {
        removeItem(resp.id);
      }
      toast(`command ${resp.command} failed: ${resp.error || 'unknown error'}`, true);
    }
  }, [removeItem, toast]);

  const onSnapshot = useCallback((snapshot: CollabSnapshot) => {
    // Authoritative history replace (oldest-first entries). Clears any stale
    // streaming buffers for the guest session so a reconnect never re-renders
    // orphaned deltas; the host re-sends the snapshot as seq 0 on every
    // connection (including reconnect), so this is the single source of truth.
    const sid = typeof snapshot.sessionId === 'string' ? snapshot.sessionId : '';
    sidRef.current = sid || sidRef.current;
    const prefix = `${sid ?? ''}\u0000`;
    for (const key of streamBuf.keys()) {
      if (key.startsWith(prefix)) streamBuf.delete(key);
    }
    for (const key of normalizedThinkingKeys) {
      if (key.startsWith(prefix)) normalizedThinkingKeys.delete(key);
    }
    assistantIdRef.current = '';
    optimisticQueueRef.current = [];
    setStreaming(false);
    setItems(snapshotToItems(snapshot));
  }, []);

  const onStatus = useCallback((state: CollabConnState) => {
    setStatus(state);
    if (state === 'reconnecting' || state === 'connecting') {
      // A reconnect drops the in-flight run state; the next snapshot replaces
      // the transcript authoritatively. Old frames from the prior connection
      // cannot replay (fresh epoch => fresh keys => tag failure).
      setStreaming(false);
      assistantIdRef.current = '';
      optimisticQueueRef.current = [];
      abortPendingRef.current = false;
    }
  }, []);

  const onError = useCallback((message: string) => {
    toast(message, true);
  }, [toast]);

  // Create the guest connection once; tear down on unmount. The role key is
  // held in memory by the CollabGuest and dropped in stop() — it is never
  // persisted or placed on the wire.
  useEffect(() => {
    if (!link) return;
    const guest = new CollabGuest(link, {
      onStatus,
      onSnapshot,
      onEvent,
      onResponse,
      onError,
    });
    guestRef.current = guest;
    guest.start();
    return () => {
      guest.stop();
      guestRef.current = null;
      // Drop any pending composer resize work (coalesced per animation frame).
      autoResizeRef.current?.cancel();
      // Drop any streaming buffers the guest populated so they can never leak
      // into a later (non-guest) mount of the shared registry.
      const prefix = `${sidRef.current ?? ''}\u0000`;
      for (const key of streamBuf.keys()) {
        if (key.startsWith(prefix)) streamBuf.delete(key);
      }
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  /* ---------------- composer ---------------- */

  // Guest composer auto-grow: same coalescing controller as the host view
  // (see ./autoResize) — per-keystroke onInput only schedules the resize, the
  // layout work (height:auto reset → scrollHeight read → height write) runs
  // once per animation frame, and the 180px cap is preserved. submit() flushes
  // so the cleared composer collapses immediately.
  const autoResize = useCallback((input: HTMLTextAreaElement) => {
    autoResizeRef.current ??= createAutoResizeController<HTMLTextAreaElement>({
      maxHeight: 180,
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

  const submit = useCallback(() => {
    const input = promptInputRef.current;
    if (!input || !link || link.role !== 'control') return;
    const text = input.value.trim();
    if (!text) return;
    const bubbleId = nextId('u');
    pushItem({ kind: 'user', id: bubbleId, text, optimistic: true });
    optimisticQueueRef.current.push(bubbleId);
    input.value = '';
    flushComposerResize(input);
    guestRef.current?.sendCommand('prompt', text, bubbleId).catch((err: Error) => {
      removeItem(bubbleId);
      optimisticQueueRef.current = optimisticQueueRef.current.filter((id) => id !== bubbleId);
      toast(`send failed: ${err.message}`, true);
    });
  }, [flushComposerResize, link, pushItem, removeItem, toast]);

  const abort = useCallback(() => {
    if (!link || link.role !== 'control' || !streaming) return;
    abortPendingRef.current = true;
    guestRef.current?.sendCommand('abort').catch(() => {
      abortPendingRef.current = false;
    });
  }, [link, streaming]);

  // Auto-scroll while pinned to the bottom: item commits follow when the pin
  // state is live; async content growth is covered by the ResizeObserver in
  // useScrollPin.
  useEffect(() => {
    pinIfPinned();
  }, [items, pinIfPinned]);

  if (!link) {
    return (
      <div className="empty-hint collab-error">
        This collab link is malformed. Open a valid link of the form
        <code> /collab/ws/&lt;roomId&gt;#c=&lt;key&gt; </code> or
        <code> #v=&lt;key&gt; </code> with a 32-byte base64url key.
      </div>
    );
  }

  const isControl = link.role === 'control';
  const statusLabel =
    status === 'connected' ? 'connected' :
    status === 'connecting' ? 'connecting…' :
    status === 'reconnecting' ? 'reconnecting…' :
    status === 'closed' ? 'offline' : 'error';

  return (
    <div id="collab-guest" data-role={link.role} data-permission={link.role}>
      <header>
        <div className="brand">
          rpi<span className="brand-sub">collab</span>
        </div>
        <span id="conn-state" className="pill" data-state={status}>
          {statusLabel}
        </span>
        <span id="collab-role-badge" className="badge" data-role={link.role}>
          {isControl ? 'control' : 'view-only'}
        </span>
        <span id="stream-badge" className="badge" hidden={!streaming}>
          streaming
        </span>
      </header>

      <main id="transcript" aria-live="polite" ref={transcriptRef} onScroll={onTranscriptScroll}>
        <div className="transcript-content" ref={transcriptContentRef}>
        {items.length === 0 && (
          <div className="empty-hint">
            {status === 'connected'
              ? 'Waiting for the host session transcript…'
              : 'Connecting to the collab room…'}
          </div>
        )}
        {items.map((item) => {
          switch (item.kind) {
            case 'user': {
              const userImagesList = item.images ?? [];
              const analysis = item.analysis;
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
                          onLoad={() => pinIfPinned()}
                        />
                      ))}
                    </div>
                  )}
                  {item.text !== '' && (
                    <MarkdownBody className="msg--user__text" text={item.text} onLayoutChange={pinIfPinned} />
                  )}
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
                <StreamingAssistant key={item.id} sid={sidRef.current ?? ''} id={item.id} />
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
              // Same shared IrcCard as the host (imported from ./App), so the
              // guest transcript can never drift from the host rendering.
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
                  <div className="approval__note">
                    Answer in the terminal — remote answering is disabled for all guests.
                  </div>
                </div>
              );
            default:
              return null;
          }
        })}
        </div>
      </main>

      <footer>
        {isControl ? (
          <>
            <textarea
              id="prompt-input"
              ref={promptInputRef}
              rows={3}
              placeholder="Message the host agent… (Enter to send, Shift+Enter for a newline, Esc to abort)"
              disabled={status !== 'connected'}
              onKeyDown={(e) => {
                if (e.key === 'Enter' && !e.shiftKey) {
                  e.preventDefault();
                  submit();
                } else if (e.key === 'Escape') {
                  abort();
                }
              }}
              onInput={(e) => autoResize(e.currentTarget)}
            />
            <div id="composer-buttons">
              <button
                id="send-btn"
                type="button"
                disabled={status !== 'connected'}
                className={streaming ? 'composer-action composer-action--stop' : 'composer-action composer-action--send'}
                aria-label={streaming ? 'Stop generating' : 'Send message'}
                title={streaming ? 'Stop generating (Esc)' : 'Send message (Enter)'}
                onClick={streaming ? abort : submit}
              >
                <span aria-hidden="true">{streaming ? '■' : '➤'}</span>
                <span className="composer-action__label">{streaming ? 'Stop' : 'Send'}</span>
              </button>
            </div>
          </>
        ) : (
          <div className="collab-viewonly-notice">
            <strong>View-only guest.</strong> Prompt and abort are disabled for view guests. Host-only
            operations (approvals, session lifecycle, settings) are not available to any
            guest — approvals surface as read-only notices to be answered in the terminal.
          </div>
        )}
      </footer>

      <ToastList toasts={toasts} dismiss={(id) => setToasts((prev) => prev.filter((t) => t.id !== id))} />
    </div>
  );
}