// Presentational file-conversation dock. Owner supplies one snapshot, the
// draft, and send/stop. No conversation id, active tab, or RPC.
import { useCallback, useEffect, useMemo, useRef, type KeyboardEvent as ReactKeyboardEvent, type UIEvent } from 'react';
import {
  composerBeforeInputShouldSubmit,
  composerEnterShouldPreventDefault,
  composerEnterShouldSubmit,
  nativeEventShiftKey,
  nextPasteEnterGuard,
} from '../composerPaste';
import { safeText } from '../redact';
import { nextId } from '../transcript';
import { createScrollPin, type ScrollPinController } from '../scrollPin';
import {
  ConversationEntryView,
  cancelConversationStreamsForScope,
  conversationPathContext,
  conversationStreamCache,
  conversationStreamKey,
} from './ConversationEntries';
import type { SideChatEntryWire } from './SideChatPanel';

export interface FileConversationRange {
  startLine: number;
  endLine: number;
}

export interface FileConversationContextWire {
  requestedRange?: FileConversationRange;
  includedRange: FileConversationRange | null;
  truncated: boolean;
}

/** One file conversation. The owner binds conversationId; the dock never switches it. */
export interface FileConversationSnapshot {
  conversationId: string;
  path: string;
  streaming: boolean;
  status: string;
  entries: SideChatEntryWire[];
  context?: FileConversationContextWire;
  accepted?: boolean;
  busy?: boolean;
}

export interface FileConversationDockProps {
  snapshot: FileConversationSnapshot | null;
  draft: string;
  onDraft: (draft: string) => void;
  onSend: () => void | boolean | Promise<void | boolean>;
  onStop: () => void;
  onError: (message: string) => void;
  sessionId: string | null;
  cwd?: string | null;
  selection?: FileConversationRange | null;
}

function formatRange(range: FileConversationRange): string {
  return range.startLine === range.endLine
    ? `line ${range.startLine}`
    : `lines ${range.startLine}–${range.endLine}`;
}

function contextDisclosure(
  context: FileConversationContextWire | undefined,
  selection: FileConversationRange | null | undefined,
): string | null {
  if (selection) {
    return `Selection ${formatRange(selection)}`;
  }
  if (!context) return null;
  if (context.requestedRange && context.includedRange == null) {
    return `Selection ${formatRange(context.requestedRange)} was not included`;
  }
  if (context.truncated && context.includedRange) {
    return `Context truncated to ${formatRange(context.includedRange)}`;
  }
  if (context.includedRange) {
    return `Context ${formatRange(context.includedRange)}`;
  }
  return null;
}

export function FileConversationDock({
  snapshot,
  draft,
  onDraft,
  onSend,
  onError,
  onStop,
  sessionId,
  cwd = null,
  selection = null,
}: FileConversationDockProps) {
  const transcriptRef = useRef<HTMLDivElement>(null);
  const pinRef = useRef<ScrollPinController | null>(null);
  if (pinRef.current === null) pinRef.current = createScrollPin();
  const conversationIdRef = useRef('');
  const streamScopeRef = useRef(`file-${nextId('stream')}`);
  const pasteEnterGuardUntilRef = useRef(0);
  const composerComposingRef = useRef(false);
  const composerEnterHandledRef = useRef(false);
  const sendingRef = useRef(false);
  const conversationId = snapshot?.conversationId ?? '';
  const streaming = snapshot?.streaming ?? false;
  const entries = snapshot?.entries ?? [];
  const pathContext = useMemo(() => conversationPathContext(entries), [entries]);
  const disclosure = contextDisclosure(snapshot?.context, selection);
  const ownerSessionId = sessionId ?? '';
  const ownerCwd = cwd ?? undefined;

  const pinIfPinned = useCallback(() => {
    pinRef.current?.followIfPinned(transcriptRef.current);
  }, []);
  const onTranscriptScroll = useCallback((event: UIEvent<HTMLDivElement>) => {
    pinRef.current?.onScroll(event.currentTarget);
  }, []);

  useEffect(() => {
    if (conversationIdRef.current !== conversationId) {
      conversationIdRef.current = conversationId;
      pinRef.current?.forcePin(transcriptRef.current);
      return;
    }
    pinIfPinned();
  }, [conversationId, entries, streaming, pinIfPinned]);

  useEffect(() => {
    const el = transcriptRef.current;
    if (!el || typeof ResizeObserver === 'undefined') return;
    const observer = new ResizeObserver(() => {
      pinRef.current?.handleResize(el);
    });
    observer.observe(el);
    return () => observer.disconnect();
  }, []);
  useEffect(() => () => cancelConversationStreamsForScope(streamScopeRef.current), []);
  const canSend = !streaming && draft.trim() !== '';
  const submit = () => {
    if (sendingRef.current || !canSend) return;
    sendingRef.current = true;
    void Promise.resolve()
      .then(() => onSend())
      .catch((err: unknown) => {
        onError(err instanceof Error ? err.message : String(err));
      })
      .finally(() => {
        sendingRef.current = false;
      });
  };

  const onComposerKeyDown = (event: ReactKeyboardEvent<HTMLTextAreaElement>) => {
    if ((event.ctrlKey || event.metaKey) && !event.altKey && (event.key === 'v' || event.key === 'V')) {
      pasteEnterGuardUntilRef.current = nextPasteEnterGuard(performance.now());
    }
    if (event.key === 'Escape' && streaming) {
      event.preventDefault();
      onStop();
      return;
    }
    if (streaming) return;
    const enterDecision = {
      key: event.key,
      shiftKey: event.shiftKey,
      isComposing: event.nativeEvent.isComposing,
      keyCode: event.nativeEvent.keyCode,
      which: event.nativeEvent.which,
      compositionActive: composerComposingRef.current,
      now: performance.now(),
      pasteGuardUntil: pasteEnterGuardUntilRef.current,
    };
    if (composerEnterShouldPreventDefault(enterDecision)) event.preventDefault();
    if (composerEnterShouldSubmit(enterDecision)) {
      composerEnterHandledRef.current = true;
      queueMicrotask(() => {
        composerEnterHandledRef.current = false;
      });
      submit();
    }
  };

  return (
    <section
      className="file-conversation"
      aria-label="File conversation"
      data-conversation-id={conversationId}
      data-session-id={ownerSessionId}
      data-workspace-cwd={ownerCwd}
    >
      <div className="file-conversation__head">
        <span>File</span>
        <span className="file-conversation__path" title={snapshot?.path ?? ''}>
          {safeText(snapshot?.path ?? 'No file')}
        </span>
      </div>
      {disclosure && (
        <div className="file-conversation__context">{disclosure}</div>
      )}
      <div
        className="file-conversation__transcript side-chat__transcript"
        ref={transcriptRef}
        aria-live="polite"
        onScroll={onTranscriptScroll}
      >
        {entries.length === 0 ? (
          <div className="side-chat__empty">Ask about this file. The conversation stays with this file tab.</div>
        ) : (
          entries.map((entry) => (
            <ConversationEntryView
              key={entry.id}
              entry={entry}
              pathContext={pathContext}
              streamKey={conversationStreamKey(streamScopeRef.current, 'file', conversationId, entry.id)}
              streaming={conversationStreamCache}
              onLayoutChange={pinIfPinned}
              sessionId={ownerSessionId}
              cwd={ownerCwd}
            />
          ))
        )}
        {streaming && <div className="side-chat__typing">…</div>}
      </div>
      <div className="file-conversation__composer">
        <textarea
          className="file-conversation__input"
          value={draft}
          enterKeyHint="send"
          onChange={(event) => onDraft(event.target.value)}
          onCompositionStart={() => {
            composerComposingRef.current = true;
          }}
          onCompositionEnd={() => {
            composerComposingRef.current = false;
          }}
          onKeyDown={onComposerKeyDown}
          onBeforeInput={(event) => {
            if (streaming) return;
            if (
              !composerBeforeInputShouldSubmit({
                inputType: event.nativeEvent.inputType,
                shiftKey: nativeEventShiftKey(event.nativeEvent),
                compositionActive: composerComposingRef.current,
                keydownHandled: composerEnterHandledRef.current,
                now: performance.now(),
                pasteGuardUntil: pasteEnterGuardUntilRef.current,
              })
            ) {
              return;
            }
            event.preventDefault();
            submit();
          }}
          onPaste={() => {
            pasteEnterGuardUntilRef.current = nextPasteEnterGuard(performance.now());
          }}
          placeholder={streaming ? 'Answering — Esc or Stop to interrupt' : 'Ask about this file… (Enter to send)'}
          rows={2}
          aria-label="File question"
        />
        <div className="file-conversation__actions">
          {streaming ? (
            <button
              type="button"
              className="file-conversation__action file-conversation__action--stop"
              onClick={onStop}
              title="Stop generating (Esc)"
              aria-label="Stop generating"
            >
              <span aria-hidden="true">■</span>
              Stop
            </button>
          ) : (
            <button
              type="button"
              className="file-conversation__action file-conversation__action--send"
              onClick={submit}
              disabled={!canSend}
              title="Send file question (Enter)"
              aria-label="Send message"
            >
              <span aria-hidden="true">➤</span>
              Send
            </button>
          )}
          <span className="file-conversation__hint">
            {streaming
              ? 'Enter to send · Shift+Enter for a newline · Esc to stop'
              : 'Enter to send · Shift+Enter for a newline'}
          </span>
        </div>
      </div>
    </section>
  );
}
