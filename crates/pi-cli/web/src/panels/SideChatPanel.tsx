import { useCallback, useEffect, useMemo, useRef, useState, type MutableRefObject, type Ref, type UIEvent } from 'react';
import { composerBeforeInputShouldSubmit, composerEnterShouldPreventDefault, composerEnterShouldSubmit, nativeEventShiftKey, nextPasteEnterGuard } from '../composerPaste';
import {
  ConversationEntryView,
  cancelConversationStreamsForOwner,
  cancelConversationStreamsForScope,
  conversationPathContext,
  conversationStreamCache,
  conversationStreamKey,
} from './ConversationEntries';
import { nextId } from '../transcript';
import {
  admitBuiltAttachments,
  aggregateWire,
  attachmentsToImageBlocks,
  classifyImageAttachments,
  formatSkipSummary,
  imageAttachmentAccept,
  mergeIntakeResults,
  placeholderFor,
  readAccepted,
  readAttachmentsInOrder,
  type AttachSkip,
  type ComposerAttachment,
  type ImageContentBlock,
  type ReadResult,
} from '../attachments';
import { createScrollPin, type ScrollPinController } from '../scrollPin';

export function admitLiveSideChatIntake(
  latest: readonly ComposerAttachment[],
  built: readonly ComposerAttachment[],
  completedIds: readonly string[],
): { next: ComposerAttachment[]; skipped: AttachSkip[] } {
  const completed = new Set(completedIds);
  const liveBuilt = built.filter((item) => latest.some((chip) => chip.id === item.id));
  const base = latest.filter((attachment) => !completed.has(attachment.id));
  return admitBuiltAttachments(base, liveBuilt);
}

export function sideChatCanSubmit(args: {
  streaming: boolean;
  prompt: string;
  attachments: readonly ComposerAttachment[];
}): boolean {
  if (args.streaming) return false;
  if (args.attachments.some((attachment) => attachment.kind === 'image' && attachment.dataBase64 == null)) {
    return false;
  }
  if (!args.prompt.trim() && attachmentsToImageBlocks(args.attachments).length === 0) {
    return false;
  }
  return true;
}

/** `side_chat_prompt` ACKs stamp `accepted`. A busy reject is `accepted: false`
 *  (and `busy: true`) with RPC success — the typed draft must stay. Missing
 *  `accepted` matches Android: treat as accepted. */
export function sideChatPromptAccepted(data: unknown): boolean {
  if (data === null || typeof data !== 'object') return true;
  return (data as { accepted?: unknown }).accepted !== false;
}

export const SIDE_CHAT_STT_CLOSED_TOAST = 'voice recording discarded: Side chat closed';

export type SideChatSttInsertPlan =
  | { kind: 'awaitSwitch'; tabId: string }
  | { kind: 'discardClosed' };

export type SideChatSttSwitchAck =
  | { kind: 'ok'; active: string; tabs: readonly string[] }
  | { kind: 'discardClosed' }
  | { kind: 'malformed' }
  | { kind: 'error'; message: string; toasted: boolean };

export type SideChatSttInsertResult =
  | { kind: 'insert' }
  | { kind: 'discardClosed' }
  | { kind: 'error'; message: string; toasted: boolean };

export type SideChatSttInsertToastPlan =
  | { kind: 'closed' }
  | { kind: 'silent' }
  | { kind: 'error'; message: string };

export type SideChatInsertTranscript = (text: string, tabId: string) => Promise<SideChatSttInsertResult>;

export function applySideChatSttInsert(args: {
  text: string;
  tabId: string;
  tabs: readonly string[];
}): SideChatSttInsertPlan {
  if (!args.text) return { kind: 'discardClosed' };
  if (!args.tabs.includes(args.tabId)) return { kind: 'discardClosed' };
  return { kind: 'awaitSwitch', tabId: args.tabId };
}

export function settleSideChatSttAfterSwitch(args: {
  tabId: string;
  ack: Exclude<SideChatSttSwitchAck, { kind: 'error' }>;
}): 'insert' | 'discardClosed' | 'malformed' {
  if (args.ack.kind === 'malformed') return 'malformed';
  if (args.ack.kind === 'discardClosed') return 'discardClosed';
  if (args.ack.active !== args.tabId) return 'discardClosed';
  if (!args.ack.tabs.includes(args.tabId)) return 'discardClosed';
  return 'insert';
}

export async function resolveSideChatSttInsert(args: {
  text: string;
  tabId: string;
  tabs: readonly string[];
  switchTab: (tabId: string) => Promise<SideChatSttSwitchAck>;
}): Promise<SideChatSttInsertResult> {
  const planned = applySideChatSttInsert({
    text: args.text,
    tabId: args.tabId,
    tabs: args.tabs,
  });
  if (planned.kind === 'discardClosed') return { kind: 'discardClosed' };
  const ack = await args.switchTab(planned.tabId);
  if (ack.kind === 'error') {
    return { kind: 'error', message: ack.message, toasted: ack.toasted };
  }
  const settled = settleSideChatSttAfterSwitch({ tabId: args.tabId, ack });
  if (settled === 'malformed') {
    return {
      kind: 'error',
      message: `side chat switch: malformed ACK (tab=${args.tabId})`,
      toasted: false,
    };
  }
  if (settled === 'discardClosed') return { kind: 'discardClosed' };
  return { kind: 'insert' };
}

export function planSideChatSttInsertToast(outcome: SideChatSttInsertResult): SideChatSttInsertToastPlan {
  if (outcome.kind === 'discardClosed') return { kind: 'closed' };
  if (outcome.kind === 'error') {
    if (outcome.toasted) return { kind: 'silent' };
    return { kind: 'error', message: outcome.message };
  }
  return { kind: 'silent' };
}

export function planSideChatSwitchCatchToast(args: {
  toastOwned: boolean;
  message: string;
}): SideChatSttInsertToastPlan {
  if (args.toastOwned) return { kind: 'silent' };
  return { kind: 'error', message: args.message };
}

export function finishSideChatSttInsertAfterAwait(args: {
  decision: SideChatSttInsertResult;
  live: boolean;
}): SideChatSttInsertResult {
  if (args.decision.kind === 'error') return args.decision;
  if (!args.live) return { kind: 'discardClosed' };
  return args.decision;
}

export function planSideChatSttInsertThrowToast(
  err: unknown,
  ctx: { sessionId: string | null; tabId: string },
): string {
  const detail = err instanceof Error ? err.message : String(err);
  return `side chat switch: ${detail} (session=${ctx.sessionId ?? ''} tab=${ctx.tabId})`;
}

export function readSideChatSwitchAck(data: unknown): { active: string; tabs: string[] } | null {
  if (!data || typeof data !== 'object') return null;
  const active = (data as { active?: unknown }).active;
  const tabs = (data as { tabs?: unknown }).tabs;
  if (typeof active !== 'string' || active === '') return null;
  if (!Array.isArray(tabs)) return null;
  const names: string[] = [];
  for (const tab of tabs) {
    if (!tab || typeof tab !== 'object') continue;
    const name = (tab as { name?: unknown }).name;
    if (typeof name === 'string' && name !== '') names.push(name);
  }
  return { active, tabs: names };
}

export const SIDE_CHAT_NAME_MAX_CHARS = 32;
const SIDE_CHAT_NAME_ADJECTIVES = [
  'amber', 'brisk', 'coral', 'dusk', 'ember', 'flint', 'grove', 'haze',
  'ivory', 'jade', 'keen', 'lunar', 'maple', 'north', 'olive', 'pearl',
] as const;
const SIDE_CHAT_NAME_NOUNS = [
  'otter', 'willow', 'quartz', 'cedar', 'heron', 'pebble', 'ridge', 'sable',
  'thistle', 'umber', 'violet', 'wren', 'yarrow', 'zephyr', 'aspen', 'birch',
] as const;
const SIDE_CHAT_RESERVED_NAMES = new Set(['new', 'list', 'close', 'btw']);

function pickSideChatWord(words: readonly string[], random: () => number): string {
  const index = Math.min(words.length - 1, Math.max(0, Math.floor(random() * words.length)));
  return words[index] ?? words[0]!;
}

export function generateSideChatName(
  existingNames: readonly string[],
  random: () => number = Math.random,
): string {
  const taken = new Set(
    [...existingNames, ...SIDE_CHAT_RESERVED_NAMES].map((name) => name.toLowerCase()),
  );
  const adjective = pickSideChatWord(SIDE_CHAT_NAME_ADJECTIVES, random);
  const noun = pickSideChatWord(SIDE_CHAT_NAME_NOUNS, random);
  const base = `${adjective}_${noun}`;
  let candidate = base.length <= SIDE_CHAT_NAME_MAX_CHARS ? base : base.slice(0, SIDE_CHAT_NAME_MAX_CHARS);
  let serial = 2;
  while (taken.has(candidate.toLowerCase())) {
    const suffix = `_${serial}`;
    const stem = base.slice(0, Math.max(1, SIDE_CHAT_NAME_MAX_CHARS - suffix.length));
    candidate = `${stem}${suffix}`;
    serial += 1;
  }
  return candidate;
}

export type SideChatEntryKind = 'user' | 'assistant' | 'tool' | 'system' | 'thinking';

export interface SideChatEntryWire {
  id: string;
  kind: SideChatEntryKind;
  role: 'user' | 'assistant' | 'tool' | 'system';
  text: string;
  toolName?: string | null;
  toolArgs?: unknown;
  output?: string | null;
  thinking?: string | null;
  isError: boolean;
  isPartial: boolean;
}

export interface SideChatTabWire {
  name: string;
  streaming: boolean;
  status: string;
  entries: SideChatEntryWire[];
}

export interface SideChatSnapshot {
  active: string;
  maxTabs: number;
  tabs: SideChatTabWire[];
  accepted?: boolean;
  busy?: boolean;
}

export type SideChatSwitchResult =
  | { kind: 'ok'; snapshot: SideChatSnapshot }
  | { kind: 'discardClosed' }
  | { kind: 'error'; message: string; toasted: boolean };

interface SideChatPanelProps {
  snapshot: SideChatSnapshot | null;
  onNew: (name: string) => void;
  onSwitch: (name: string) => Promise<SideChatSwitchResult> | void;
  onClose: (name?: string) => void;
  onPrompt: (message: string, images: ImageContentBlock[]) => void | boolean | Promise<void | boolean>;
  onAbort: () => void;
  onClosePanel: () => void;
  promptTextareaRef?: Ref<HTMLTextAreaElement>;
  insertTranscriptRef?: MutableRefObject<SideChatInsertTranscript | null>;
  isRecording?: boolean;
  isTranscribing?: boolean;
  onStartRecording?: () => void;
  onStopRecording?: () => void;
  onToast?: (message: string, error: boolean) => void;
}

/**
 * Side chat drawer: multi-tab list of parallel `/btw` sessions, the active
 * tab's transcript, and a prompt box. Polling is owned by App (an effect
 * while this panel is open); this component is presentational.
 */
export function SideChatPanel({
  snapshot,
  onNew,
  onSwitch,
  onClose,
  onPrompt,
  onAbort,
  onClosePanel,
  promptTextareaRef,
  insertTranscriptRef,
  isRecording = false,
  isTranscribing = false,
  onStartRecording,
  onStopRecording,
  onToast,
}: SideChatPanelProps) {
  const [prompt, setPrompt] = useState('');
  const transcriptRef = useRef<HTMLDivElement>(null);
  const pinRef = useRef<ScrollPinController | null>(null);
  if (pinRef.current === null) pinRef.current = createScrollPin();
  const activeNameRef = useRef('');
  const pinIfPinned = useCallback(() => {
    pinRef.current?.followIfPinned(transcriptRef.current);
  }, []);
  const onTranscriptScroll = useCallback((event: UIEvent<HTMLDivElement>) => {
    pinRef.current?.onScroll(event.currentTarget);
  }, []);
  const pasteEnterGuardUntilRef = useRef(0);
  const composerComposingRef = useRef(false);
  const composerEnterHandledRef = useRef(false);

  const [attachments, setAttachments] = useState<ComposerAttachment[]>([]);
  const attachInputRef = useRef<HTMLInputElement>(null);
  const pendingIntakeIdsRef = useRef(new Set<string>());

  const tabs = snapshot?.tabs ?? [];
  const activeName = snapshot?.active ?? '';
  const active = tabs.find((tab) => tab.name === activeName) ?? tabs[0];
  const streaming = active?.streaming ?? false;
  const pathContext = useMemo(() => conversationPathContext(active?.entries ?? []), [active]);
  const snapshotRef = useRef(snapshot);
  snapshotRef.current = snapshot;
  const streamTabRef = useRef(activeName);
  const streamScopeRef = useRef(`side-${nextId('stream')}`);

  useEffect(() => {
    if (streamTabRef.current !== activeName) {
      cancelConversationStreamsForOwner(streamScopeRef.current, 'side', streamTabRef.current);
      streamTabRef.current = activeName;
    }
  }, [activeName]);
  useEffect(() => () => cancelConversationStreamsForScope(streamScopeRef.current), []);

  useEffect(() => {
    if (!insertTranscriptRef) return;
    let live = true;
    insertTranscriptRef.current = async (text: string, tabId: string) => {
      const current = snapshotRef.current;
      const raw = await resolveSideChatSttInsert({
        text,
        tabId,
        tabs: (current?.tabs ?? []).map((tab) => tab.name),
        switchTab: async (name) => {
          const switched = await Promise.resolve(onSwitch(name));
          if (!switched) return { kind: 'discardClosed' };
          if (switched.kind === 'ok') {
            const parsed = readSideChatSwitchAck(switched.snapshot);
            if (!parsed) return { kind: 'malformed' };
            return { kind: 'ok', active: parsed.active, tabs: parsed.tabs };
          }
          if (switched.kind === 'error') {
            return { kind: 'error', message: switched.message, toasted: switched.toasted };
          }
          return { kind: 'discardClosed' };
        },
      });
      const decision = finishSideChatSttInsertAfterAwait({ decision: raw, live });
      if (decision.kind !== 'insert') return decision;
      setPrompt((currentPrompt) => {
        const separator = currentPrompt.trim() ? (currentPrompt.endsWith('\n') ? '' : '\n') : '';
        return `${currentPrompt}${separator}${text}`;
      });
      return { kind: 'insert' };
    };
    return () => {
      live = false;
      insertTranscriptRef.current = null;
    };
  }, [insertTranscriptRef, onSwitch]);

  useEffect(() => {
    if (activeNameRef.current !== activeName) {
      activeNameRef.current = activeName;
      pinRef.current?.forcePin(transcriptRef.current);
      return;
    }
    pinIfPinned();
  }, [activeName, active?.entries, streaming, pinIfPinned]);
  useEffect(() => {
    const el = transcriptRef.current;
    if (!el || typeof ResizeObserver === 'undefined') return;
    const observer = new ResizeObserver(() => {
      pinRef.current?.handleResize(el);
    });
    observer.observe(el);
    return () => observer.disconnect();
  }, []);

  const onAttachFiles = useCallback((fileList: FileList | null) => {
    if (!fileList || fileList.length === 0) return;
    const current = attachments;
    const plan = classifyImageAttachments(Array.from(fileList), {
      currentCount: current.length,
      currentWire: aggregateWire(current),
    });
    const skips: AttachSkip[] = [...plan.skipped];
    const accepted = plan.accepted;
    if (accepted.length === 0) {
      const summary = formatSkipSummary(skips);
      if (summary) onToast?.(summary, true);
      return;
    }
    const tasks = accepted.map((entry) => {
      const id = nextId('sc-att');
      return { id, entry, placeholder: placeholderFor(entry, id) };
    });
    const placeholderIds = new Set(tasks.map((task) => task.id));
    for (const task of tasks) pendingIntakeIdsRef.current.add(task.id);
    setAttachments([...current, ...tasks.map((task) => task.placeholder)]);
    void (async () => {
      try {
        const results = await readAttachmentsInOrder(accepted, readAccepted);
        const readById = new Map<string, ReadResult>();
        results.forEach((result, index) => readById.set(tasks[index]!.id, result));
        const merged = mergeIntakeResults(tasks, readById);
        skips.push(...merged.skips);
        const batchIds = tasks.map((task) => task.id);
        setAttachments((latest) => admitLiveSideChatIntake(latest, merged.built, batchIds).next);
        for (const id of batchIds) pendingIntakeIdsRef.current.delete(id);
      } catch (err) {
        for (const id of placeholderIds) pendingIntakeIdsRef.current.delete(id);
        setAttachments((latest) => latest.filter((attachment) => !placeholderIds.has(attachment.id)));
        onToast?.(err instanceof Error ? err.message : String(err), true);
      }
      const summary = formatSkipSummary(skips);
      if (summary) onToast?.(summary, true);
    })();
  }, [attachments, onToast]);

  const removeAttachment = (id: string) => {
    pendingIntakeIdsRef.current.delete(id);
    setAttachments((latest) => latest.filter((item) => item.id !== id));
  };

  const sendingRef = useRef(false);
  const submit = () => {
    if (sendingRef.current || !sideChatCanSubmit({ streaming, prompt, attachments })) return;
    const images = attachmentsToImageBlocks(attachments);
    const payload = prompt.trim();
    const sentIds = new Set(attachments.map((item) => item.id));
    sendingRef.current = true;
    pendingIntakeIdsRef.current.clear();
    void Promise.resolve()
      .then(() => onPrompt(payload, images))
      .then((ok) => {
        if (ok === false) return;
        setPrompt((current) => current.trim() === payload ? '' : current);
        setAttachments((current) => current.filter((item) => !sentIds.has(item.id)));
      })
      .finally(() => {
        sendingRef.current = false;
      });
  };

  return (
    <section id="side-chat-panel" aria-label="Side chat">
      <div className="side-chat">
        <div className="side-chat__head">
          <span className="side-chat__title">Side chat</span>
          <button type="button" className="panel-close" onClick={onClosePanel} title="Close panel" aria-label="Close side chat panel">
            ×
          </button>
        </div>
        <div className="side-chat__tabs" role="tablist" aria-label="Side chat tabs">
          {tabs.map((tab) => (
            <div key={tab.name} className={`side-chat__tab${tab.name === activeName ? ' side-chat__tab--active' : ''}`} role="presentation">
              <button
                type="button"
                role="tab"
                aria-selected={tab.name === activeName}
                className="side-chat__tab-select"
                onClick={() => { void onSwitch(tab.name); }}
                title={tab.status}
              >
                {tab.name}
                {tab.streaming && <span className="side-chat__streaming" title="streaming">●</span>}
              </button>
              <button
                type="button"
                className="side-chat__tab-close"
                aria-label={`close tab ${tab.name}`}
                title={`Close ${tab.name}`}
                onClick={() => onClose(tab.name)}
              >
                ×
              </button>
            </div>
          ))}
          <button
            type="button"
            className="side-chat__new"
            disabled={tabs.length >= (snapshot?.maxTabs ?? 8)}
            onClick={() => onNew(generateSideChatName(tabs.map((tab) => tab.name)))}
            title="New side-chat tab"
            aria-label="New side-chat tab"
          >
            <span aria-hidden="true">+</span>
            <span>New</span>
          </button>
        </div>
        <div className="side-chat__transcript" ref={transcriptRef} aria-live="polite" onScroll={onTranscriptScroll}>
          {!active || active.entries.length === 0 ? (
            <div className="side-chat__empty">
              No messages yet in “{activeName || 'default'}”. Each tab is an isolated parallel agent forked from the main session.
            </div>
          ) : (
            active.entries.map((entry) => (
              <ConversationEntryView
                key={entry.id}
                entry={entry}
                pathContext={pathContext}
                streamKey={conversationStreamKey(streamScopeRef.current, 'side', activeName, entry.id)}
                streaming={conversationStreamCache}
                onLayoutChange={pinIfPinned}
              />
            ))
          )}
          {streaming && <div className="side-chat__typing">…</div>}
        </div>
        <div className="side-chat__composer">
          {attachments.length > 0 && (
            <ul className="side-chat__chips" aria-label="Attached images">
              {attachments.map((attachment) => (
                <li key={attachment.id} className="side-chat__chip">
                  <span>{attachment.name}</span>
                  <button
                    type="button"
                    aria-label={`Remove ${attachment.name}`}
                    onClick={() => removeAttachment(attachment.id)}
                  >
                    ×
                  </button>
                </li>
              ))}
            </ul>
          )}
          <input
            ref={attachInputRef}
            type="file"
            accept={imageAttachmentAccept()}
            multiple
            hidden
            onChange={(e) => {
              onAttachFiles(e.target.files);
              e.target.value = '';
            }}
          />
          <div className="side-chat__composer-row">
          <textarea
            ref={promptTextareaRef}
            value={prompt}
            enterKeyHint="send"
            onChange={(e) => setPrompt(e.target.value)}
            onCompositionStart={() => {
              composerComposingRef.current = true;
            }}
            onCompositionEnd={() => {
              composerComposingRef.current = false;
            }}
            onKeyDown={(e) => {
              if ((e.ctrlKey || e.metaKey) && !e.altKey && (e.key === 'v' || e.key === 'V')) {
                pasteEnterGuardUntilRef.current = nextPasteEnterGuard(performance.now());
              }
              if (e.key === 'Escape' && streaming) {
                e.preventDefault();
                onAbort();
                return;
              }
              if (streaming) return;
              const enterDecision = {
                key: e.key,
                shiftKey: e.shiftKey,
                isComposing: e.nativeEvent.isComposing,
                keyCode: e.nativeEvent.keyCode,
                which: e.nativeEvent.which,
                compositionActive: composerComposingRef.current,
                now: performance.now(),
                pasteGuardUntil: pasteEnterGuardUntilRef.current,
              };
              if (composerEnterShouldPreventDefault(enterDecision)) {
                e.preventDefault();
              }
              if (composerEnterShouldSubmit(enterDecision)) {
                composerEnterHandledRef.current = true;
                queueMicrotask(() => {
                  composerEnterHandledRef.current = false;
                });
                submit();
              }
            }}
            onBeforeInput={(e) => {
              if (streaming) return;
              if (
                !composerBeforeInputShouldSubmit({
                  inputType: e.nativeEvent.inputType,
                  shiftKey: nativeEventShiftKey(e.nativeEvent),
                  compositionActive: composerComposingRef.current,
                  keydownHandled: composerEnterHandledRef.current,
                  now: performance.now(),
                  pasteGuardUntil: pasteEnterGuardUntilRef.current,
                })
              ) {
                return;
              }
              e.preventDefault();
              submit();
            }}
            onPaste={() => {
              pasteEnterGuardUntilRef.current = nextPasteEnterGuard(performance.now());
            }}
            placeholder={streaming ? 'Side agent is busy — Esc or Stop to interrupt' : 'Message the side agent… (Enter to send)'}
            rows={2}
            aria-label="Side chat prompt"
          />
          <button
            type="button"
            className="side-chat__composer-action side-chat__composer-action--attach"
            disabled={streaming}
            onClick={() => attachInputRef.current?.click()}
            title="Attach images"
            aria-label="Attach files"
          >
            <span aria-hidden="true">📎</span>
          </button>
          <button
            type="button"
            className={`side-chat__composer-action side-chat__composer-action--mic${isRecording ? ' recording' : isTranscribing ? ' transcribing' : ''}`}
            disabled={isTranscribing}
            aria-label="Microphone"
            aria-pressed={isRecording}
            title={
              isRecording
                ? 'Recording… release to transcribe'
                : isTranscribing
                  ? 'Transcribing…'
                  : 'Hold to talk (voice to text)'
            }
            onPointerDown={(e) => {
              if (e.button !== 0 || streaming) return;
              e.preventDefault();
              onStartRecording?.();
            }}
            onPointerUp={() => onStopRecording?.()}
            onPointerLeave={() => onStopRecording?.()}
            onPointerCancel={() => onStopRecording?.()}
            onContextMenu={(e) => e.preventDefault()}
          >
            <span aria-hidden="true">🎤</span>
          </button>
          {streaming ? (
            <button
              type="button"
              className="side-chat__composer-action side-chat__composer-action--stop"
              onClick={onAbort}
              title="Stop generating (Esc)"
              aria-label="Stop generating"
            >
              <span aria-hidden="true">■</span>
              <span>Stop</span>
            </button>
          ) : (
            <button
              type="button"
              className="side-chat__composer-action side-chat__composer-action--send"
              onClick={submit}
              disabled={!sideChatCanSubmit({ streaming: false, prompt, attachments })}
              title="Send side prompt"
              aria-label="Send message"
            >
              <span aria-hidden="true">➤</span>
              <span>Send</span>
            </button>
          )}
          </div>
        </div>
      </div>
    </section>
  );
}
