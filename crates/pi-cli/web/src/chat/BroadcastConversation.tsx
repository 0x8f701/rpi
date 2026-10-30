import { useCallback, useEffect, useMemo, useRef, useState, type UIEvent } from 'react';
import { createAutoResizeController, type AutoResizeController } from '../autoResize';
import { MarkdownBody } from '../MarkdownBody';
import { appendDraft, parseSlashLine } from '../commands';
import { CommandPicker } from '../CommandPicker';
import { composerBeforeInputShouldSubmit, composerEnterShouldPreventDefault, composerEnterShouldSubmit, nativeEventShiftKey, nextPasteEnterGuard } from '../composerPaste';
import { createScrollPin, type ScrollPinController } from '../scrollPin';
import { safeText } from '../redact';
import type { ProjectRecord } from './mapping';
import type { RunnerCommand } from './protocol';
import { runnerDisplayName, runnerMentionName } from './runners';
import { TranscriptItem } from './SessionView';
import { transcriptPathContext } from '../transcript';
import {
  insertMentionToken,
  resolveChannelRecipients,
  runnerMentionToken,
  trailingMentionQuery,
  type ChannelMentionMember,
} from './chatMentions';
import type { ChannelMessage } from './chatChannelLog';
import {
  type ActiveProjectMember,
  type BroadcastAttempt,
  type BroadcastAttemptStatus,
  type BroadcastResponseCard,
  type BroadcastRun,
} from './team';

export interface BroadcastConversationProps {
  project: ProjectRecord;
  runs: readonly BroadcastRun[];
  messages: readonly ChannelMessage[];
  members: readonly ActiveProjectMember[];
  disabledReason: string;
  sendCommand: (command: RunnerCommand) => Promise<unknown>;
  onSend: (message: string, mappingIds?: readonly string[]) => Promise<void>;
  onCancelTarget: (runId: string, mappingId: string) => Promise<void>;
  onRetryTarget: (runId: string, mappingId: string) => Promise<void>;
  onViewRunner: (mappingId: string) => void;
}

const ATTEMPT_LABEL: Record<BroadcastAttemptStatus, string> = {
  dispatching: 'Sending',
  streaming: 'Responding',
  cancelling: 'Cancelling',
  succeeded: 'Done',
  failed: 'Failed',
  cancelled: 'Cancelled',
  interrupted: 'Interrupted',
};

function ignoreCommandCatalog(): void {}

function currentAttempt(card: BroadcastResponseCard): BroadcastAttempt | undefined {
  return card.attempts.find((attempt) => attempt.id === card.currentAttemptId) ?? card.attempts.at(-1);
}

function mentionMembersOf(members: readonly ActiveProjectMember[]): ChannelMentionMember[] {
  const peers = members.map((member) => member.runner);
  return members.map((member) => ({
    mappingId: member.mapping.id,
    mention: runnerMentionToken(runnerMentionName(member.runner, peers)),
  }));
}

function liveCards(run: BroadcastRun): BroadcastResponseCard[] {
  return run.responses.filter((card) => {
    const attempt = currentAttempt(card);
    if (!attempt) return false;
    return attempt.status !== 'succeeded';
  });
}


export function BroadcastConversation({
  project,
  runs,
  messages,
  members,
  disabledReason,
  sendCommand,
  onSend,
  onCancelTarget,
  onRetryTarget,
  onViewRunner,
}: BroadcastConversationProps) {
  const [draft, setDraft] = useState('');
  const [sending, setSending] = useState(false);
  const sendingRef = useRef(false);
  const [mentionError, setMentionError] = useState('');
  const [expanded, setExpanded] = useState<Record<string, boolean>>({});
  const pasteEnterGuardUntilRef = useRef(0);
  const composerComposingRef = useRef(false);
  const composerEnterHandledRef = useRef(false);
  const composerRef = useRef<HTMLTextAreaElement | null>(null);
  const autoResizeRef = useRef<AutoResizeController<HTMLTextAreaElement> | null>(null);
  const transcriptNodeRef = useRef<HTMLDivElement>(null);
  const pinRef = useRef<ScrollPinController | null>(null);
  if (pinRef.current === null) pinRef.current = createScrollPin();
  const composerLocked = disabledReason !== '';
  const pickerConnected = members.some((member) => member.runner.state === 'connected');
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
  const keepComposerFocus = useCallback(() => {
    composerRef.current?.focus({ preventScroll: true });
  }, []);
  const pinIfPinned = useCallback(() => {
    pinRef.current?.followIfPinned(transcriptNodeRef.current);
  }, []);
  const onTranscriptScroll = useCallback((event: UIEvent<HTMLDivElement>) => {
    pinRef.current?.onScroll(event.currentTarget);
  }, []);
  useEffect(() => {
    pinIfPinned();
  }, [messages, runs, pinIfPinned]);
  useEffect(() => {
    const el = transcriptNodeRef.current;
    if (!el || typeof ResizeObserver === 'undefined') return;
    const observer = new ResizeObserver(() => {
      pinRef.current?.handleResize(el);
    });
    observer.observe(el);
    return () => observer.disconnect();
  }, []);
  useEffect(() => () => {
    autoResizeRef.current?.cancel();
  }, []);
  const mentionMembers = useMemo(() => mentionMembersOf(members), [members]);
  const mentionPeers = useMemo(() => members.map((member) => member.runner), [members]);
  const mentionQuery = trailingMentionQuery(draft);
  const mentionChoices = mentionQuery === null
    ? []
    : members.filter((member) => {
      const token = runnerMentionToken(runnerMentionName(member.runner, mentionPeers)).toLowerCase();
      const shown = runnerMentionToken(runnerDisplayName(member.runner)).toLowerCase();
      const query = mentionQuery.toLowerCase();
      return token.startsWith(query) || shown.startsWith(query);
    });

  const commandErrorMessage = (error: unknown): string => (
    error instanceof Error && error.message !== '' ? error.message : String(error)
  );

  const submit = useCallback(async () => {
    if (sendingRef.current || composerLocked) return;
    const message = draft.trim();
    if (!message) return;
    const slash = parseSlashLine(message);
    if (slash) {
      setMentionError(slash.name === 'goal'
        ? 'Open a runner session and use /goal there.'
        : `/${slash.name} is not available in chat.`);
      return;
    }
    const plan = resolveChannelRecipients(message, mentionMembers);
    if (plan.kind === 'unmatched') {
      setMentionError(`No runner named @${plan.token}.`);
      return;
    }
    if (plan.kind === 'ambiguous') {
      setMentionError(`@${plan.token} matches more than one runner.`);
      return;
    }
    setMentionError('');
    sendingRef.current = true;
    setSending(true);
    try {
      await onSend(message, plan.kind === 'mentioned' ? plan.mappingIds : undefined);
      setDraft('');
    } catch (error) {
      setMentionError(commandErrorMessage(error));
    } finally {
      sendingRef.current = false;
      setSending(false);
      const input = composerRef.current;
      if (input) autoResizeRef.current?.flush(input);
      keepComposerFocus();
    }
  }, [composerLocked, draft, keepComposerFocus, mentionMembers, onSend]);

  const insertSlashDraft = useCallback((text: string) => {
    setDraft((current) => appendDraft(current, text));
    keepComposerFocus();
  }, [keepComposerFocus]);

  return (
    <section className="chat-broadcast" aria-label={`Channel ${safeText(project.label)}`}>
      <div className="chat-broadcast__transcript" ref={transcriptNodeRef} onScroll={onTranscriptScroll} aria-live="polite">
        {messages.length === 0 && runs.length === 0 && (
          <p className="chat-session__status">Send one message to the agent team. @ a runner to assign work.</p>
        )}
        {messages.map((message) => (
          <article
            className={message.kind === 'human'
              ? 'chat-session-message chat-session-message--user'
              : 'chat-session-message chat-session-message--assistant'}
            key={message.id}
          >
            <span className="chat-session-message__role">
              {message.kind === 'human' ? 'You' : `@${message.author}`}
            </span>
            <MarkdownBody className="chat-session-markdown" text={message.text} onLayoutChange={pinIfPinned} />
          </article>
        ))}
        {runs.map((run) => {
          return (
          <article className="chat-broadcast-group" data-run-id={run.id} key={run.id}>
            {liveCards(run).map((card) => {
              const attempt = currentAttempt(card);
              if (!attempt) return null;
              const prior = card.attempts.filter((row) => row.id !== attempt.id);
              const showPrior = expanded[card.mappingId] === true;
              const canCancel = attempt.status === 'dispatching' || attempt.status === 'streaming';
              const canRetry = (attempt.status === 'failed' || attempt.status === 'cancelled' || attempt.status === 'interrupted')
                && members.some((member) => member.mapping.id === card.mappingId && member.runner.state === 'connected');
              const attemptPathContext = transcriptPathContext(attempt.transcript.items);
              return (
                <article
                  className="chat-broadcast-card"
                  key={JSON.stringify([run.id, card.mappingId])}
                  data-runner-id={card.runnerId}
                  data-session-id={card.sessionId}
                  data-turn-id={attempt.turnId ?? ''}
                >
                  <header className="chat-broadcast-card__head">
                    <span className="chat-status-dot" data-tone={attempt.status === 'succeeded' ? 'ok' : attempt.status === 'failed' || attempt.status === 'interrupted' ? 'error' : attempt.status === 'cancelled' ? 'muted' : 'warn'} aria-hidden="true" />
                    <strong>{safeText(card.runnerLabel)}</strong>
                    <span className="chat-runner-state" data-state={attempt.status}>{ATTEMPT_LABEL[attempt.status]}</span>
                    {attempt.modelLabel && <span className="chat-broadcast-card__model">{safeText(attempt.modelLabel)}</span>}
                  </header>
                  {attempt.error && <p className="chat-form-error" role="status">{safeText(attempt.error)}</p>}
                  {attempt.transcript.items.map((item) => (
                    <TranscriptItem item={item} key={item.id} runnerId={card.runnerId} mappingId={card.mappingId} sessionId={card.sessionId} cwd={project.cwd} pathContext={attemptPathContext} onLayoutChange={pinIfPinned} />
                  ))}
                  <div className="chat-broadcast-card__actions">
                    {canCancel && (
                      <button
                        className="chat-button chat-button--compact"
                        type="button"
                        onClick={() => {
                          void Promise.resolve(onCancelTarget(run.id, card.mappingId)).catch((error) => {
                            setMentionError(commandErrorMessage(error));
                          });
                        }}
                      >
                        Cancel
                      </button>
                    )}
                    {canRetry && (
                      <button
                        className="chat-button chat-button--compact"
                        type="button"
                        onClick={() => {
                          void Promise.resolve(onRetryTarget(run.id, card.mappingId)).catch((error) => {
                            setMentionError(commandErrorMessage(error));
                          });
                        }}
                      >
                        Retry
                      </button>
                    )}
                    <button
                      className="chat-button chat-button--compact chat-button--quiet"
                      type="button"
                      onClick={() => onViewRunner(card.mappingId)}
                    >
                      View conversation
                    </button>
                    {prior.length > 0 && (
                      <button
                        className="chat-button chat-button--compact chat-button--quiet"
                        type="button"
                        onClick={() => setExpanded((current) => ({ ...current, [card.mappingId]: !showPrior }))}
                      >
                        {showPrior ? 'Hide earlier attempts' : `${prior.length} earlier attempt${prior.length === 1 ? '' : 's'}`}
                      </button>
                    )}
                  </div>
                  {showPrior && prior.map((row) => {
                    const rowPathContext = transcriptPathContext(row.transcript.items);
                    return (
                    <div className="chat-broadcast-card__prior" key={row.id}>
                      <span className="chat-runner-state" data-state={row.status}>{ATTEMPT_LABEL[row.status]}</span>
                      {row.error && <p className="chat-form-error">{safeText(row.error)}</p>}
                      {row.transcript.items.map((item) => (
                        <TranscriptItem item={item} key={item.id} runnerId={card.runnerId} mappingId={card.mappingId} sessionId={card.sessionId} cwd={project.cwd} pathContext={rowPathContext} onLayoutChange={pinIfPinned} />
                      ))}
                    </div>
                    );
                  })}
                </article>
              );
            })}
            </article>
          );
        })}
      </div>
      {mentionQuery !== null && mentionChoices.length > 0 && (
        <ul className="chat-mention-menu" id="chat-channel-mentions" role="listbox" aria-label="Mention a runner">
          {mentionChoices.map((member) => {
            const mention = runnerMentionToken(runnerMentionName(member.runner, mentionPeers));
            return (
              <li key={member.mapping.id} role="none">
                <button
                  className="chat-mention-menu__item"
                  role="option"
                  type="button"
                  onClick={() => {
                    setDraft(insertMentionToken(draft, mention));
                    setMentionError('');
                    keepComposerFocus();
                  }}
                >
                  @{mention}
                </button>
              </li>
            );
          })}
        </ul>
      )}
      {mentionError && <p className="chat-form-error" role="alert">{mentionError}</p>}
      <div className="chat-session__composer">
        <div className="chat-session__composer-row">
        <CommandPicker
          connected={pickerConnected}
          sendCommand={(command) => sendCommand(command as RunnerCommand)}
          onSelect={insertSlashDraft}
          onSkillSelect={insertSlashDraft}
          onError={(message) => setMentionError(message)}
          getComposerValue={() => composerRef.current?.value ?? draft}
          onCatalog={ignoreCommandCatalog}
        />
        <textarea
          ref={composerRef}
          aria-label="Channel message"
          enterKeyHint="send"
          onChange={(event) => {
            setDraft(event.target.value);
            setMentionError('');
          }}
          onInput={(event) => autoResize(event.currentTarget)}
          onCompositionStart={() => {
            composerComposingRef.current = true;
          }}
          onCompositionEnd={() => {
            composerComposingRef.current = false;
          }}
          onKeyDown={(event) => {
            if ((event.ctrlKey || event.metaKey) && !event.altKey && (event.key === 'v' || event.key === 'V')) {
              pasteEnterGuardUntilRef.current = nextPasteEnterGuard(performance.now());
            }
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
            if (composerEnterShouldPreventDefault(enterDecision)) {
              event.preventDefault();
            }
            if (!composerEnterShouldSubmit(enterDecision)) return;
            composerEnterHandledRef.current = true;
            queueMicrotask(() => {
              composerEnterHandledRef.current = false;
            });
            void submit();
          }}
          onBeforeInput={(event) => {
            if (!composerBeforeInputShouldSubmit({
              inputType: event.nativeEvent.inputType,
              shiftKey: nativeEventShiftKey(event.nativeEvent),
              compositionActive: composerComposingRef.current,
              keydownHandled: composerEnterHandledRef.current,
              now: performance.now(),
              pasteGuardUntil: pasteEnterGuardUntilRef.current,
            })) return;
            event.preventDefault();
            void submit();
          }}
          onPaste={() => {
            pasteEnterGuardUntilRef.current = nextPasteEnterGuard(performance.now());
          }}
          placeholder={composerLocked
            ? disabledReason
            : 'Message the agent… (Enter to send, Shift+Enter for a newline)'}
          rows={1}
          value={draft}
        />
        <button
          className="chat-button chat-button--primary"
          disabled={composerLocked || sending || draft.trim() === ''}
          onClick={() => void submit()}
          type="button"
        >
          {sending ? 'Sending…' : 'Send'}
        </button>
        </div>
      </div>
    </section>
  );
}
