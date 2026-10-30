import { useCallback, useEffect, useRef, useState, type PointerEvent, type UIEvent } from 'react';
import { useDesktopActions } from '../desktopBridge';
import { BashCard, IrcCard, ToolCard } from '../App';
import {
  attachmentAccept,
  classifyAttachments,
  codeBadgeLabel,
  formatSkipSummary,
  readAccepted,
  wireFootprint,
  type ComposerAttachment,
} from '../attachments';
import { appendDraft, parseSlashLine } from '../commands';
import { CommandPicker } from '../CommandPicker';
import { createAutoResizeController, type AutoResizeController } from '../autoResize';
import { composerBeforeInputShouldSubmit, composerEnterShouldPreventDefault, composerEnterShouldSubmit, nativeEventShiftKey, nextPasteEnterGuard } from '../composerPaste';
import { loadTokenForAuthority, type StorageLike } from '../hostToken';
import { MarkdownBody } from '../MarkdownBody';
import { thinkingFirstBlocks } from '../markdown';
import { createScrollPin, type ScrollPinController } from '../scrollPin';
import { safeText } from '../redact';
export { safeText };
import {
  applyToolResultToItems,
  applyToolSnapshot,
  applyToolStartToItems,
  boundOutput,
  contentText,
  customToItem,
  mergeAuthoritativeItems,
  messagesToItems,
  nextId,
  normalizeThinkingNewlines,
  toolCardCompletionStatus,
  transcriptPathContext,
  type Item,
} from '../transcript';
import { isAuthoritativeSubscriptionCut } from '../protocol';
import { planTurnStreaming } from '../turnStreaming';
import { SteeringQueue } from '../SteeringQueue';
import { dropQueuedPreviewAfterCancel, runExclusiveQueueCommand, settleQueueCommand, steeringPreviewsFromMessages } from '../steeringQueueHelpers';
import {
  activityStripView,
  applyActivityStripEvent,
  emptyActivityStripState,
  reconcileActivityStrip,
} from '../activityStrip';
import {
  applyComposerAbortTap,
  composerActionClickKind,
  shouldHoldOptimisticStop,
} from '../composerAbort';
import {
  composerSubmitWire,
  dispatchComposerSubmit,
  planComposerSubmitKind,
} from '../composerSubmit';
import { formatGoalResult, goalWire, resolveSlashAction } from '../slashDispatch';
import { canSendAttachmentToTmux, sendComposerAttachmentToTmux } from '../tmuxFile';
import type { ContentBlock, EventFrame } from '../types';
import { ChatGoalPanel } from './ChatGoalPanel';
import type { RunnerCommand } from './protocol';
import { RunnerCommandError, type RunnerView } from './runners';


const MAX_TRANSCRIPT_ITEMS = 200;
const MAX_MESSAGE_CHARS = 80_000;
const MAX_TOOL_TEXT_CHARS = 8_000;
const MAX_TOOL_TEXT_LINES = 40;
const MAX_STATUS_CHARS = 240;
function ignoreCommandCatalog(): void {}


export type SessionCommandSender = (command: RunnerCommand) => Promise<unknown>;

export interface SessionViewProps {
  runner: RunnerView;
  mappingId: string;
  cwd: string;
  sessionId: string;
  events: (callback: (frame: unknown) => void) => () => void;
  composerDisabledReason?: string;
  onTransportError: (message: string) => void;
  sendCommand: SessionCommandSender;
  initialAttachments?: ComposerAttachment[];
  /**
   * Authoritative current turn of the owning session at FIRST PAINT. The
   * injected subscription never replays and a turn's deltas cannot create a
   * turnId, so a keyed remount into an already-live turn (A→B→A) seeds the
   * activity strip from this state instead of waiting for the next
   * `turn_started`. Ongoing truth comes from `readAuthority`, not from these
   * props: a render can commit after the session state already moved on.
   */
  activeTurnId?: string | null;
  isStreaming?: boolean;
  /**
   * Synchronous read of the owning session's CURRENT authoritative activity,
   * bound to this exact runner/session. Called once immediately after this
   * view registers its session listener — the subscription does not replay,
   * so a terminal or a replacement turn that landed while the view had no
   * listener is only visible here — and again for recovery cuts that update
   * the session state without delivering a turn frame. Must read live state
   * on every call, never a rendered snapshot.
   */
  readAuthority?: () => SessionAuthority | null;
}

/** What the owning session state knows about its current turn, right now. */
export interface SessionAuthority {
  activeTurnId: string | null;
  isStreaming: boolean;
}

function sessionTokenStorage(): StorageLike | null {
  try {
    return typeof localStorage === 'undefined' ? null : localStorage;
  } catch {
    return null;
  }
}

export function sendChatSessionAttachmentToTmux(
  attachment: ComposerAttachment,
  opts: {
    loadToken: (authority: string) => string;
    pageProtocol?: string;
    fetchImpl?: typeof fetch;
  },
): ReturnType<typeof sendComposerAttachmentToTmux> {
  return sendComposerAttachmentToTmux(attachment, {
    loadToken: opts.loadToken,
    pageProtocol: opts.pageProtocol,
    fetchImpl: opts.fetchImpl,
  });
}

async function ingestComposerFiles(
  fileList: FileList,
  current: readonly ComposerAttachment[],
): Promise<{ attachments: ComposerAttachment[]; skipMessage: string | null }> {
  const currentWire = current.reduce((sum, item) => sum + wireFootprint(item.kind, item.size), 0);
  const plan = classifyAttachments(Array.from(fileList), {
    currentCount: current.length,
    currentWire,
  });
  const accepted = plan.accepted.filter((entry) => entry.kind === 'image' || entry.kind === 'code');
  const skipped = [
    ...plan.skipped,
    ...plan.accepted
      .filter((entry) => entry.kind !== 'image' && entry.kind !== 'code')
      .map((entry) => ({
        name: entry.file.name,
        size: entry.file.size,
        type: entry.file.type,
        reason: 'unsupported' as const,
      })),
  ];
  const read = await Promise.all(accepted.map((entry) => readAccepted(entry)));
  const attachments: ComposerAttachment[] = [];
  for (const result of read) {
    if (result.skip) {
      skipped.push(result.skip);
      continue;
    }
    if (result.attachment) attachments.push(result.attachment);
  }
  return { attachments, skipMessage: formatSkipSummary(skipped) };
}
export type ComposerDispatchResult =
  | { status: 'empty' }
  | { status: 'disconnected' }
  | { status: 'accepted'; turnId: string; data: unknown }
  | { status: 'handled'; data: unknown }
  | { status: 'slash'; text: string }
  | { status: 'open_goal' };

export interface PendingOptimisticUser {
  id: string;
  text: string;
  turnId?: string;
}

export interface SessionTranscriptState {
  items: Item[];
  activeAssistantId: string | null;
  pendingUsers?: PendingOptimisticUser[];
}

function isContentBlock(value: unknown): value is ContentBlock {
  return value !== null && typeof value === 'object' && 'type' in value && typeof value.type === 'string';
}


function boundedText(value: string, limit: number): string {
  if (value.length <= limit) return value;
  return `${value.slice(0, limit)}\n… truncated`;
}

function boundedToolText(value: unknown): string {
  let text: string;
  if (typeof value === 'string') {
    text = value;
  } else {
    try {
      text = JSON.stringify(value, null, 2) ?? '';
    } catch {
      text = String(value ?? '');
    }
  }
  return boundedText(boundOutput(text, MAX_TOOL_TEXT_LINES).text, MAX_TOOL_TEXT_CHARS);
}

function boundItems(items: Item[]): Item[] {
  const bounded = items.map((item): Item => {
    switch (item.kind) {
      case 'user':
        return {
          ...item,
          text: boundedText(item.text, MAX_MESSAGE_CHARS),
          images: undefined,
        };
      case 'assistant':
        return {
          ...item,
          blocks: item.blocks.map((block) => {
            if (block.type === 'text' && typeof block.text === 'string') {
              return { ...block, text: boundedText(block.text, MAX_MESSAGE_CHARS) };
            }
            if (block.type === 'thinking' && typeof block.thinking === 'string') {
              return { ...block, thinking: boundedText(block.thinking, MAX_MESSAGE_CHARS) };
            }
            return block;
          }),
        };
      case 'toolCard':
        return {
          ...item,
          args: boundedToolText(item.args),
          result: boundedToolText(item.result),
          details: undefined,
          media: undefined,
        };
      case 'toolResult':
        return { ...item, text: boundedToolText(item.text) };
      case 'bash':
        return {
          ...item,
          command: boundedToolText(item.command),
          output: boundedToolText(item.output),
        };
      case 'custom':
      case 'summary':
      case 'handoff-divider':
        return { ...item, text: boundedText(item.text, MAX_MESSAGE_CHARS) };
      case 'irc':
        return { ...item, body: boundedText(item.body, MAX_MESSAGE_CHARS) };
      case 'approval':
        return { ...item, message: boundedText(item.message, MAX_MESSAGE_CHARS) };
    }
  });
  return bounded.length > MAX_TRANSCRIPT_ITEMS
    ? bounded.slice(bounded.length - MAX_TRANSCRIPT_ITEMS)
    : bounded;
}

function nextActiveAssistant(items: Item[]): string | null {
  for (let index = items.length - 1; index >= 0; index -= 1) {
    const item = items[index];
    if (item.kind === 'assistant' && item.status === 'streaming') return item.id;
  }
  return null;
}

function appendAssistantBlock(blocks: ContentBlock[], type: 'text' | 'thinking', delta: string): ContentBlock[] {
  if (!delta) return blocks;
  const field = type === 'text' ? 'text' : 'thinking';
  const last = blocks.at(-1);
  if (last?.type === type) {
    const previous = typeof last[field] === 'string' ? last[field] : '';
    const next = blocks.slice();
    next[next.length - 1] = { ...last, [field]: boundedText(`${previous}${delta}`, MAX_MESSAGE_CHARS) };
    return next;
  }
  return [...blocks, type === 'text'
    ? { type: 'text', text: boundedText(delta, MAX_MESSAGE_CHARS) }
    : { type: 'thinking', thinking: boundedText(delta, MAX_MESSAGE_CHARS) }];
}

function eventMessage(frame: EventFrame): Record<string, unknown> {
  const message = frame.message;
  if (message === null || typeof message !== 'object' || Array.isArray(message)) return {};
  return Object.fromEntries(Object.entries(message));
}

function appendMessageItem(items: Item[], frame: EventFrame): Item[] {
  const message = eventMessage(frame);
  const role = typeof message.role === 'string' ? message.role : '';
  if (role === 'toolResult') {
    const toolCallId = typeof message.toolCallId === 'string' ? message.toolCallId : '';
    const text = boundedToolText(contentText(message.content));
    return applyToolResultToItems(items, toolCallId, text);
  }
  if (role === 'custom') {
    const item = customToItem(message);
    return item ? [...items, frame.itemId ? { ...item, id: frame.itemId } : item] : items;
  }
  const restored = messagesToItems([{ id: frame.itemId, message }]);
  return restored.length > 0 ? [...items, ...restored] : items;
}

export function sessionEventBelongsTo(frame: unknown, sessionId: string): frame is EventFrame {
  if (!frame || typeof frame !== 'object' || !('sessionId' in frame)) return false;
  return typeof frame.sessionId === 'string' && frame.sessionId === sessionId;
}


function eventTurnId(frame: EventFrame): string {
  const record = frame as unknown as Record<string, unknown>;
  if (typeof record.turnId === 'string' && record.turnId !== '') return record.turnId;
  if (typeof record.turn_id === 'string' && record.turn_id !== '') return record.turn_id;
  const message = eventMessage(frame);
  if (typeof message.turnId === 'string' && message.turnId !== '') return message.turnId;
  if (typeof message.turn_id === 'string' && message.turn_id !== '') return message.turn_id;
  return '';
}

export function promptTurnId(data: unknown): string | null {
  if (typeof data === 'string') {
    const trimmed = data.trim();
    return trimmed === '' ? null : trimmed;
  }
  if (!data || typeof data !== 'object' || Array.isArray(data)) return null;
  const record = data as Record<string, unknown>;
  const direct = record.turnId ?? record.turn_id;
  if (typeof direct === 'string' && direct.trim() !== '') return direct;
  const turn = record.turn;
  if (turn && typeof turn === 'object' && !Array.isArray(turn)) {
    const nested = turn as Record<string, unknown>;
    const nestedId = nested.id ?? nested.turnId ?? nested.turn_id;
    if (typeof nestedId === 'string' && nestedId.trim() !== '') return nestedId;
  }
  return null;
}

function pendingUsersOf(current: SessionTranscriptState): PendingOptimisticUser[] {
  return current.pendingUsers ?? [];
}

export function registerPendingUser(
  current: SessionTranscriptState,
  text: string,
  id: string,
): SessionTranscriptState {
  const pendingUsers = pendingUsersOf(current);
  if (current.items.some((item) => item.id === id) || pendingUsers.some((pending) => pending.id === id)) {
    return current.pendingUsers ? current : { ...current, pendingUsers };
  }
  const items = boundItems([
    ...current.items,
    { kind: 'user', id, text, optimistic: true },
  ]);
  return {
    items,
    activeAssistantId: current.activeAssistantId,
    pendingUsers: [...pendingUsers, { id, text }],
  };
}

export function removePendingUser(
  current: SessionTranscriptState,
  pendingId: string,
): SessionTranscriptState {
  const items = boundItems(current.items.filter((item) => item.id !== pendingId));
  return {
    items,
    activeAssistantId: nextActiveAssistant(items),
    pendingUsers: pendingUsersOf(current).filter((pending) => pending.id !== pendingId),
  };
}

export function acceptPendingUser(
  current: SessionTranscriptState,
  pendingId: string,
  turnId: string | null,
): SessionTranscriptState {
  if (!pendingUsersOf(current).some((pending) => pending.id === pendingId)) return {
    ...current,
    pendingUsers: pendingUsersOf(current),
  };
  if (!turnId) return removePendingUser(current, pendingId);
  return {
    items: current.items,
    activeAssistantId: current.activeAssistantId,
    pendingUsers: pendingUsersOf(current).map((pending) => (
      pending.id === pendingId ? { ...pending, turnId } : pending
    )),
  };
}

function settlePendingUserFromEvent(
  items: Item[],
  pendingUsers: PendingOptimisticUser[],
  frame: EventFrame,
): { items: Item[]; pendingUsers: PendingOptimisticUser[] } {
  const restored = messagesToItems([{ id: frame.itemId, message: eventMessage(frame) }]);
  if (restored.length === 0) return { items, pendingUsers };
  const first = restored[0];
  const authoritativeId = typeof frame.itemId === 'string' && frame.itemId !== '' ? frame.itemId : first.id;
  const authoritative: Item = first.kind === 'user' ? { ...first, id: authoritativeId } : first;
  if (items.some((item) => item.id === authoritative.id)) {
    return {
      items: items.map((item) => item.id === authoritative.id ? authoritative : item),
      pendingUsers: pendingUsers.filter((pending) => pending.id !== authoritative.id),
    };
  }
  if (authoritative.kind !== 'user') {
    return { items: [...items, ...restored], pendingUsers };
  }
  const turnId = eventTurnId(frame);
  const pendingIndex = pendingUsers.findIndex((pending) => {
    if (pending.text !== authoritative.text) return false;
    if (pending.turnId && turnId && pending.turnId !== turnId) return false;
    return true;
  });
  if (pendingIndex < 0) {
    return { items: [...items, authoritative], pendingUsers };
  }
  const pending = pendingUsers[pendingIndex];
  const nextPending = pendingUsers.filter((_, index) => index !== pendingIndex);
  const existingIndex = items.findIndex((item) => item.id === pending.id);
  if (existingIndex < 0) {
    return { items: [...items, authoritative], pendingUsers: nextPending };
  }
  return {
    items: items.map((item, index) => index === existingIndex ? authoritative : item),
    pendingUsers: nextPending,
  };
}

export function applySessionEvent(
  current: SessionTranscriptState,
  frame: EventFrame,
): SessionTranscriptState {
  let items = current.items;
  let activeAssistantId = current.activeAssistantId;
  let pendingUsers = current.pendingUsers ?? [];
  switch (frame.type) {
    case 'message_start': {
      const message = eventMessage(frame);
      if (message.role === 'assistant') {
        const id = typeof frame.itemId === 'string' && frame.itemId !== '' ? frame.itemId : nextId('a');
        const existing = items.findIndex((item) => item.id === id);
        const assistant: Item = { kind: 'assistant', id, status: 'streaming', blocks: [] };
        if (existing >= 0) {
          items = items.map((item, index) => index === existing ? assistant : item);
        } else {
          items = [...items, assistant];
        }
        activeAssistantId = id;
      } else if (message.role === 'user') {
        const settled = settlePendingUserFromEvent(items, pendingUsers, frame);
        items = settled.items;
        pendingUsers = settled.pendingUsers;
      } else {
        items = appendMessageItem(items, frame);
      }
      break;
    }
    case 'message_update': {
      const rawEvent = frame.assistantMessageEvent;
      const event = rawEvent !== null && typeof rawEvent === 'object' && !Array.isArray(rawEvent)
        ? Object.fromEntries(Object.entries(rawEvent))
        : {};
      const id = typeof frame.itemId === 'string' && frame.itemId !== ''
        ? frame.itemId
        : activeAssistantId;
      const delta = typeof event.delta === 'string' ? event.delta : '';
      if (id && delta && (event.type === 'text_delta' || event.type === 'thinking_delta')) {
        items = items.map((item) => item.kind === 'assistant' && item.id === id
          ? {
              ...item,
              blocks: appendAssistantBlock(
                item.blocks,
                event.type === 'text_delta' ? 'text' : 'thinking',
                delta,
              ),
            }
          : item);
      }
      break;
    }
    case 'message_end': {
      const message = eventMessage(frame);
      if (message.role !== 'assistant') break;
      const id = typeof frame.itemId === 'string' && frame.itemId !== ''
        ? frame.itemId
        : activeAssistantId;
      if (!id) break;
      const finalBlocks = Array.isArray(message.content)
        ? message.content.filter(isContentBlock).filter((block) => block.type !== 'toolCall')
        : [];
      items = items.map((item) => item.kind === 'assistant' && item.id === id
        ? {
            ...item,
            status: 'final',
            blocks: finalBlocks.length > 0 ? finalBlocks : item.blocks,
          }
        : item);
      if (activeAssistantId === id) activeAssistantId = null;
      break;
    }
    case 'tool_execution_start': {
      const toolCallId = typeof frame.toolCallId === 'string' ? frame.toolCallId : '';
      items = applyToolStartToItems(items, {
        kind: 'toolCard',
        id: typeof frame.itemId === 'string' && frame.itemId !== '' ? frame.itemId : nextId('tc'),
        toolCallId,
        toolName: typeof frame.toolName === 'string' && frame.toolName !== '' ? frame.toolName : 'tool',
        args: boundedToolText(frame.args),
        status: 'running',
        result: '',
      });
      break;
    }
    case 'tool_execution_update': {
      const toolCallId = typeof frame.toolCallId === 'string' ? frame.toolCallId : '';
      const rawPartial = frame.partialResult;
      const partial = rawPartial !== null && typeof rawPartial === 'object' && !Array.isArray(rawPartial)
        ? Object.fromEntries(Object.entries(rawPartial))
        : {};
      const text = contentText(partial.content);
      if (toolCallId && text) items = applyToolSnapshot(items, toolCallId, boundedToolText(text));
      break;
    }
    case 'tool_execution_end': {
      const toolCallId = typeof frame.toolCallId === 'string' ? frame.toolCallId : '';
      const rawResult = frame.result;
      const result = rawResult !== null && typeof rawResult === 'object' && !Array.isArray(rawResult)
        ? Object.fromEntries(Object.entries(rawResult))
        : {};
      const text = contentText(result.content);
      if (toolCallId) {
        items = applyToolSnapshot(
          items,
          toolCallId,
          boundedToolText(text),
          toolCardCompletionStatus(frame.isError === true, result.content, result.details),
        );
      }
      break;
    }
    case 'bash_execution_end': {
      const message = eventMessage(frame);
      const exitCode = typeof message.exitCode === 'number' ? message.exitCode : null;
      items = [...items, {
        kind: 'bash',
        id: typeof frame.itemId === 'string' && frame.itemId !== '' ? frame.itemId : nextId('b'),
        command: boundedToolText(message.command),
        output: boundedToolText(message.output),
        status: message.cancelled === true ? 'cancelled' : exitCode === null ? undefined : exitCode === 0 ? 'done' : 'error',
      }];
      break;
    }
    case 'turn_terminal':
      items = items.map((item) => item.kind === 'assistant' && item.status === 'streaming'
        ? { ...item, status: 'final' }
        : item);
      activeAssistantId = null;
      break;
    default:
      return current;
  }
  const bounded = boundItems(items);
  return {
    items: bounded,
    activeAssistantId: activeAssistantId ?? nextActiveAssistant(bounded),
    pendingUsers,
  };
}

export function mergeSessionHistory(
  history: Item[],
  current: SessionTranscriptState,
  requestCut: Item[],
): SessionTranscriptState {
  const cutById = new Map(requestCut.map((item) => [item.id, item]));
  const pendingUsers = current.pendingUsers ?? [];
  let items = mergeAuthoritativeItems(boundItems(history), current.items);
  for (const item of current.items) {
    if (cutById.get(item.id) === item) continue;
    const index = items.findIndex((candidate) => candidate.id === item.id);
    if (index < 0) items = [...items, item];
    else items = items.map((candidate, candidateIndex) => candidateIndex === index ? item : candidate);
  }
  items = boundItems(items);
  return { items, activeAssistantId: nextActiveAssistant(items), pendingUsers };
}

/**
 * Consume a flagged authoritative subscription cut: the snapshot `messages`
 * are the transcript base (the single authoritative transcript), rebuilt with
 * the same rules as the get_messages restore. Genuinely pending optimistic
 * rows and in-flight live rows survive via mergeAuthoritativeItems; pendings
 * the cut already settled (their optimistic row collapsed onto an
 * authoritative twin) drain from the queue. Replayed live events apply on
 * top, in order, after this base lands.
 */
export function applySessionSnapshot(
  current: SessionTranscriptState,
  messages: unknown[],
): SessionTranscriptState {
  const history = messagesToItems(messages);
  const items = mergeAuthoritativeItems(boundItems(history), current.items);
  const pendingUsers = pendingUsersOf(current).filter((pending) => (
    items.some((item) => item.id === pending.id)
  ));
  return {
    items,
    activeAssistantId: nextActiveAssistant(items),
    pendingUsers,
  };
}

export async function dispatchComposerPrompt(
  draft: string,
  connected: boolean,
  sendCommand: SessionCommandSender,
  streaming = false,
): Promise<ComposerDispatchResult> {
  const message = draft.trim();
  if (!message) return { status: 'empty' };
  if (!connected) return { status: 'disconnected' };
  const slash = parseSlashLine(message);
  if (slash) {
    if (slash.name !== 'goal') {
      return { status: 'slash', text: `/${slash.name} is not available in chat.` };
    }
    const action = resolveSlashAction(slash.name, slash.args);
    if (action.type === 'error') throw new Error(action.message);
    if (action.type !== 'goal') {
      return { status: 'slash', text: `/${slash.name} is not available in chat.` };
    }
    if (action.action.op === 'show' && !action.action.explicit) {
      return { status: 'open_goal' };
    }
    const data = await sendCommand(goalWire(action.action) as RunnerCommand);
    const activateWork = action.action.op === 'create' || action.action.op === 'resume';
    const state = activateWork ? await sendCommand({ type: 'goal_get' }) : undefined;
    const result = formatGoalResult(action.action, data, state);
    if (!result.ok) throw new Error(result.message);
    return { status: 'slash', text: result.text };
  }
  const kind = planComposerSubmitKind({ streaming, requested: 'prompt' });
  const outcome = await dispatchComposerSubmit({
    kind,
    send: (submitKind) => sendCommand(composerSubmitWire(submitKind, message)),
    onConvertToSteer: () => {},
  });
  const data = outcome.data;
  const turnId = promptTurnId(data);
  if (turnId) return { status: 'accepted', turnId, data };
  return { status: 'handled', data };
}

function messagesFromResponse(data: unknown): unknown[] {
  if (!data || typeof data !== 'object' || !('messages' in data) || !Array.isArray(data.messages)) {
    throw new Error('get_messages response is missing a messages array.');
  }
  return data.messages;
}

function failureMessage(error: unknown): string {
  const message = error instanceof Error ? error.message : String(error);
  return boundedText(safeText(message || 'Command failed.'), MAX_STATUS_CHARS);
}

function AssistantMessage({
  item,
  runnerId,
  mappingId,
  sessionId,
  cwd,
  pathContext,
  onLayoutChange,
}: {
  item: Extract<Item, { kind: 'assistant' }>;
  runnerId: string;
  mappingId: string;
  sessionId: string;
  cwd: string;
  pathContext?: string;
  /** Transcript pin callback: async Markdown/Mermaid hydration and image
   *  decode mutate layout with no React commit, so the pin controller must
   *  hear about them the same task they happen. */
  onLayoutChange?: () => void;
}) {
  const visibleBlocks = thinkingFirstBlocks(item.blocks.filter((block) =>
    (block.type === 'text' && typeof block.text === 'string' && block.text !== '')
    || (block.type === 'thinking' && typeof block.thinking === 'string' && block.thinking !== '')));
  return (
    <article
      className="chat-session-message chat-session-message--assistant"
      data-status={item.status}
      data-session-id={sessionId}
      data-runner-id={runnerId}
      data-mapping-id={mappingId}
      data-workspace-cwd={cwd}
    >
      <span className="chat-session-message__role">Assistant</span>
      {visibleBlocks.map((block, index) => block.type === 'thinking' ? (
        <details className="chat-session-thinking" key={`${item.id}-thinking-${index}`}>
          <summary>Thinking</summary>
          <MarkdownBody
            className="thinking__body"
            text={normalizeThinkingNewlines(block.thinking ?? '')}
            pathContext={pathContext}
            onLayoutChange={onLayoutChange}
          />
        </details>
      ) : (
        <MarkdownBody className="chat-session-markdown" key={`${item.id}-text-${index}`} text={block.text ?? ''} pathContext={pathContext} onLayoutChange={onLayoutChange} />
      ))}
      {visibleBlocks.length === 0 && item.status === 'streaming' && <span className="chat-session-typing">…</span>}
      {item.errorMessage ? <div className="msg--assistant__error" role="note">{safeText(item.errorMessage)}</div> : null}
    </article>
  );
}

export function TranscriptItem({ item, runnerId, mappingId, sessionId, cwd, pathContext, onLayoutChange }: { item: Item; runnerId: string; mappingId: string; sessionId: string; cwd: string; pathContext?: string; /** Transcript pin callback for async Markdown/Mermaid layout; optional so the Broadcast lane (no pin controller) reuses the component unchanged. */ onLayoutChange?: () => void }) {
  switch (item.kind) {
    case 'user':
      return (
        <article
          className="chat-session-message chat-session-message--user"
          data-session-id={sessionId}
          data-runner-id={runnerId}
          data-mapping-id={mappingId}
          data-workspace-cwd={cwd}
        >
          <span className="chat-session-message__role">You</span>
          <MarkdownBody className="chat-session-markdown" text={item.text} pathContext={pathContext} onLayoutChange={onLayoutChange} />
        </article>
      );
    case 'assistant':
      return <AssistantMessage item={item} runnerId={runnerId} mappingId={mappingId} sessionId={sessionId} cwd={cwd} pathContext={pathContext} onLayoutChange={onLayoutChange} />;
    case 'toolCard':
      return (
        <div className="chat-session__tool" data-session-id={sessionId} data-runner-id={runnerId} data-mapping-id={mappingId} data-workspace-cwd={cwd}>
          <ToolCard item={item} onLayoutChange={onLayoutChange} />
        </div>
      );
    case 'toolResult':
      return <BashCard label="tool output" output={item.text === '' ? '(empty tool result)' : item.text} itemId={item.id} />;
    case 'bash':
      return <BashCard command={item.command} output={item.output} status={item.status} itemId={item.id} />;
    case 'custom':
    case 'summary':
      return (
        <article
          className="chat-session-message chat-session-message--notice"
          data-session-id={sessionId}
          data-runner-id={runnerId}
          data-mapping-id={mappingId}
          data-workspace-cwd={cwd}
        >
          <span className="chat-session-message__role">{safeText(item.label)}</span>
          <MarkdownBody className="chat-session-markdown" text={item.text} pathContext={pathContext} onLayoutChange={onLayoutChange} />
        </article>
      );
    case 'irc':
      return (
        <div data-session-id={sessionId} data-runner-id={runnerId} data-mapping-id={mappingId} data-workspace-cwd={cwd}>
          <IrcCard item={item} onLayoutChange={onLayoutChange} />
        </div>
      );

    case 'approval':
      return (
        <article className="chat-session-message chat-session-message--notice">
          <span className="chat-session-message__role">{safeText(item.title)}</span>
          <p>{safeText(item.message)}</p>
        </article>
      );
  }
}

export function SessionView({
  runner,
  mappingId,
  sessionId,
  cwd,
  sendCommand,
  events,
  onTransportError,
  composerDisabledReason,
  initialAttachments,
  activeTurnId,
  isStreaming,
  readAuthority,
}: SessionViewProps) {
  const connected = runner.state === 'connected';
  const composerLocked = Boolean(composerDisabledReason);
  const composerEnabled = connected && !composerLocked;
  const [transcript, setTranscript] = useState<SessionTranscriptState>({ items: [], activeAssistantId: null });
  const transcriptRef = useRef(transcript);
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState('');
  const [draft, setDraft] = useState('');
  const [sending, setSending] = useState(false);
  const sendingRef = useRef(false);
  const [composerStatus, setComposerStatus] = useState('');
  const [attachments, setAttachments] = useState<ComposerAttachment[]>(() => initialAttachments ?? []);
  const [goalOpen, setGoalOpen] = useState(false);
  const composerRef = useRef<HTMLTextAreaElement | null>(null);
  const fileInputRef = useRef<HTMLInputElement | null>(null);
  const autoResizeRef = useRef<AutoResizeController<HTMLTextAreaElement> | null>(null);
  const pasteEnterGuardUntilRef = useRef(0);
  const composerComposingRef = useRef(false);
  const composerEnterHandledRef = useRef(false);
  const transcriptNodeRef = useRef<HTMLDivElement>(null);
  const [turnStreaming, setTurnStreaming] = useState(false);
  const streaming = turnStreaming || transcript.items.some((item) => item.kind === 'assistant' && item.status === 'streaming');
  const pathContext = transcriptPathContext(transcript.items);
  const [aborting, setAborting] = useState(false);
  const [optimisticStopped, setOptimisticStopped] = useState(false);
  const abortGestureClickRef = useRef(false);
  const stopArmed = !optimisticStopped && (streaming || aborting);
  useEffect(() => {
    if (!streaming) {
      setAborting(false);
      setOptimisticStopped(false);
    }
  }, [streaming]);
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
  // Bottom-pin state for the /chat transcript. This lane is a plain scroll
  // container (no `.transcript-content` wrapper), so it reuses the SHARED
  // pin decision core (createScrollPin) directly instead of the full
  // useScrollPin container+wrapper observer pair: the scroll handler derives
  // the pin state from live geometry, and the items effect plus the
  // MarkdownBody layout callbacks (async Markdown/Mermaid hydration, image
  // decode) are the only other writers — all through the controller, so an
  // unpinned (scrolled-up) viewport is never moved against the user.
  const pinRef = useRef<ScrollPinController | null>(null);
  if (pinRef.current === null) pinRef.current = createScrollPin();
  const pinIfPinned = useCallback(() => {
    pinRef.current?.followIfPinned(transcriptNodeRef.current);
  }, []);
  // The only reader of pin state: genuine user scrolling (or the scroll
  // event a programmatic pin produces, which always lands at the bottom).
  const onTranscriptScroll = useCallback((event: UIEvent<HTMLDivElement>) => {
    pinRef.current?.onScroll(event.currentTarget);
  }, []);

  const updateTranscript = useCallback((update: (current: SessionTranscriptState) => SessionTranscriptState) => {
    setTranscript((current) => {
      const next = update(current);
      transcriptRef.current = next;
      return next;
    });
  }, []);

  const [queuedSteering, setQueuedSteering] = useState<string[]>([]);
  const queuedSteeringRef = useRef<string[]>([]);
  queuedSteeringRef.current = queuedSteering;
  const queueGenRef = useRef(0);
  const queueBusyRef = useRef(false);
  const [queueBusy, setQueueBusy] = useState(false);
  // The owning session state owns which turn is live; the frames this view
  // subscribes to only carry that turn's detail. The props seed the first
  // paint, and the handoff effect below re-reads that same authority once the
  // listener is registered. Props are never reconciled again after that: a
  // render can commit after the session state has already moved on, and
  // letting it write would clobber live frames.
  const [activityStrip, setActivityStrip] = useState(() => reconcileActivityStrip(
    emptyActivityStripState(sessionId),
    sessionId,
    isStreaming === true,
    activeTurnId ?? '',
  ));
  useEffect(() => events((frame) => {
    if (!sessionEventBelongsTo(frame, sessionId)) return;
    const type = typeof frame.type === 'string' ? frame.type : '';
    setTurnStreaming((current) => planTurnStreaming(type, current, frame.activeTurnId));
    setActivityStrip((current) => {
      const applied = applyActivityStripEvent(current, frame, sessionId);
      if (frame.type === 'subscription_snapshot' || frame.type === 'session_resync') {
        const raw = frame.activeTurnId;
        const activeTurnId = typeof raw === 'string' && raw !== '' ? raw : '';
        return reconcileActivityStrip(applied, sessionId, Boolean(activeTurnId), activeTurnId);
      }
      return applied;
    });
    if (frame && typeof frame === 'object' && 'type' in frame && (frame as { type?: unknown }).type === 'queue_update') {
      queueGenRef.current += 1;
      const previews = steeringPreviewsFromMessages((frame as { steering?: unknown }).steering);
      queuedSteeringRef.current = previews;
      setQueuedSteering(previews);
      return;
    }
    // Snapshot is data, not ACK: a flagged authoritative cut is the
    // transcript base and applies BEFORE the replayed events runners
    // dispatch right after it. A non-authoritative reconnect snapshot keeps
    // the current transcript and only its replay events paint.
    if (isAuthoritativeSubscriptionCut(frame)) {
      updateTranscript((current) => applySessionSnapshot(current, frame.messages));
      return;
    }
    updateTranscript((current) => applySessionEvent(current, frame as EventFrame));
  }), [events, sessionId, updateTranscript]);

  // Declared after the subscription effect so it runs in the same flush,
  // immediately after that listener is registered: register-then-read leaves
  // no window in which a frame could land in the session state unseen. The
  // subscription never replays, so a turn that started or ended between this
  // view's first paint and its registration is only visible through this
  // read — a missed terminal clears the strip and a missed replacement turn
  // takes it over, while frames delivered after registration stay
  // authoritative because nothing writes the strip from a render.
  useEffect(() => {
    const authority = readAuthority?.();
    if (!authority) return;
    setActivityStrip((current) => reconcileActivityStrip(
      current,
      sessionId,
      authority.isStreaming === true,
      authority.activeTurnId ?? '',
    ));
  }, [readAuthority, sessionId]);

  useEffect(() => {
    if (!connected) {
      setLoading(false);
      return;
    }
    let cancelled = false;
    const requestCut = transcriptRef.current.items.slice();
    setLoading(true);
    setLoadError('');
    void sendCommand({ type: 'get_messages' })
      .then((data) => {
        if (cancelled) return;
        const history = messagesToItems(messagesFromResponse(data));
        updateTranscript((current) => mergeSessionHistory(history, current, requestCut));
        onTransportError('');
      })
      .catch((error) => {
        if (cancelled) return;
        const message = failureMessage(error);
        if (error instanceof RunnerCommandError && error.kind === 'transport') onTransportError(message);
        else setLoadError(message);
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [connected, onTransportError, sendCommand, updateTranscript]);

  // Pin, don't jump: transcript growth (new items, restored history) keeps
  // the viewport glued to the bottom ONLY while the user is pinned; a
  // manual scroll-up is never stolen. A fresh controller starts pinned, so
  // the initial history load lands at the bottom.
  useEffect(() => {
    pinIfPinned();
  }, [transcript.items, pinIfPinned]);

  // Container-only resize coverage — the one piece the shared useScrollPin
  // observer pair adds beyond the callbacks above: composer/header wrapping
  // and window resizes change this transcript's own clientHeight without any
  // React commit, so neither the items effect nor a Markdown layout callback
  // fires. Content growth is deliberately NOT observed here — item commits
  // run pinIfPinned via the effect above and async Markdown/Mermaid/image
  // layout reports through TranscriptItem's onLayoutChange — so a content
  // observer would only duplicate those calls. While pinned the viewport
  // follows the new bottom; unpinned it never moves (manual upscroll is
  // never stolen). The observer is disconnected on unmount; ChatApp remounts
  // this component per session via key, so the owner change is an unmount.
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

  const abortTurn = useCallback(() => {
    const tap = applyComposerAbortTap(sessionId);
    if (!tap.command) return;
    abortGestureClickRef.current = true;
    window.setTimeout(() => {
      abortGestureClickRef.current = false;
    }, 0);
    setOptimisticStopped(true);
    setAborting(false);
    void sendCommand({ type: 'abort' })
      .then(() => sendCommand({ type: 'get_state' }))
      .then((state) => {
        const stillStreaming = (state as { isStreaming?: unknown } | null)?.isStreaming === true;
        if (shouldHoldOptimisticStop(true, stillStreaming)) return;
      })
      .catch((error) => {
        setOptimisticStopped(false);
        setComposerStatus(failureMessage(error));
      });
    keepComposerFocus();
  }, [keepComposerFocus, sendCommand, sessionId]);

  const submit = useCallback(async () => {
    if (sendingRef.current || composerLocked) return;
    const submittedDraft = draft;
    const acceptedText = submittedDraft.trim();
    if (!acceptedText) return;
    if (!connected) {
      onTransportError('Runner is not connected.');
      return;
    }
    const slash = parseSlashLine(acceptedText);
    const isSlash = slash !== null;
    const localId = isSlash ? null : nextId('u');
    sendingRef.current = true;
    setSending(true);
    setComposerStatus('');
    if (localId) {
      updateTranscript((current) => registerPendingUser(current, acceptedText, localId));
    }
    setDraft((current) => current === submittedDraft ? '' : current);
    try {
      const result = await dispatchComposerPrompt(submittedDraft, connected, sendCommand, streaming);
      if (result.status === 'accepted' && localId) {
        updateTranscript((current) => acceptPendingUser(current, localId, result.turnId));
        onTransportError('');
      } else if (result.status === 'handled' && localId) {
        setQueuedSteering((current) => (
          current.includes(acceptedText) ? current : [...current, acceptedText]
        ));
        setComposerStatus('Queued as steering — it will send when the current turn finishes.');
        onTransportError('');
      } else if (result.status === 'disconnected') {
        if (localId) updateTranscript((current) => removePendingUser(current, localId));
        setDraft(submittedDraft);
        onTransportError('Runner is not connected.');
      } else if (result.status === 'slash') {
        setDraft(submittedDraft);
        setComposerStatus(result.text);
        onTransportError('');
      } else if (result.status === 'open_goal') {
        setGoalOpen(true);
        onTransportError('');
      }
    } catch (error) {
      if (localId) updateTranscript((current) => removePendingUser(current, localId));
      setDraft(submittedDraft);
      const message = failureMessage(error);
      if (error instanceof RunnerCommandError && error.kind === 'transport') onTransportError(message);
      else setComposerStatus(message);
    } finally {
      sendingRef.current = false;
      setSending(false);
      const input = composerRef.current;
      if (input) autoResizeRef.current?.flush(input);
      keepComposerFocus();
    }
  }, [composerLocked, connected, draft, keepComposerFocus, onTransportError, sendCommand, streaming, updateTranscript]);

  const insertSlashDraft = useCallback((text: string) => {
    setDraft((current) => appendDraft(current, text));
    keepComposerFocus();
  }, [keepComposerFocus]);

  const ingestFiles = useCallback((fileList: FileList | null) => {
    if (!fileList || fileList.length === 0) return;
    void ingestComposerFiles(fileList, attachments).then((result) => {
      if (result.attachments.length > 0) {
        setAttachments((current) => [...current, ...result.attachments]);
      }
      if (result.skipMessage) setComposerStatus(result.skipMessage);
    });
  }, [attachments]);

  const sendAttachmentToTerminal = useCallback((attachment: ComposerAttachment) => {
    void sendChatSessionAttachmentToTmux(attachment, {
      loadToken: (authority) => loadTokenForAuthority(sessionTokenStorage(), authority),
      pageProtocol: typeof location !== 'undefined' ? location.protocol : 'http:',
    }).then((result) => {
      if (result.ok) return;
      setComposerStatus(result.message);
    });
  }, []);


  const sendQueuedNow = useCallback(() => {
    void runExclusiveQueueCommand(queueBusyRef, setQueueBusy, async () => {
      const gen = queueGenRef.current;
      await settleQueueCommand(
        () => sendCommand({ type: 'queue_send_now', sessionId } as RunnerCommand),
        gen,
        () => queueGenRef.current,
        (error) => onTransportError(failureMessage(error)),
      );
    });
  }, [onTransportError, sendCommand, sessionId]);

  const cancelQueuedAt = useCallback((index: number) => {
    void runExclusiveQueueCommand(queueBusyRef, setQueueBusy, async () => {
      const preview = queuedSteeringRef.current[index];
      const gen = queueGenRef.current;
      await settleQueueCommand(
        () => sendCommand({ type: 'queue_cancel', scope: 'steering', index, sessionId } as RunnerCommand),
        gen,
        () => queueGenRef.current,
        (error) => onTransportError(failureMessage(error)),
        () => {
          const next = dropQueuedPreviewAfterCancel(queuedSteeringRef.current, index, preview);
          queuedSteeringRef.current = next;
          setQueuedSteering(next);
        },
      );
    });
  }, [onTransportError, sendCommand, sessionId]);

  const stripView = activityStripView(activityStrip, runner.state === 'connected' ? 'on' : runner.state, false);

  useDesktopActions({
    attach: composerEnabled ? () => fileInputRef.current?.click() : undefined,
    stop: connected && stopArmed ? abortTurn : undefined,
  });

  return (
    <section className="chat-session" aria-label={`Session for ${safeText(runner.label)}`}>
      <div className="chat-session__conversation">
        <div className="chat-session__stage">
          <div className="chat-session__transcript" ref={transcriptNodeRef} onScroll={onTranscriptScroll} aria-live="polite">
            {loading && transcript.items.length === 0 && <p className="chat-session__status" role="status">Loading transcript…</p>}
            {loadError && <p className="chat-session__status chat-session__status--error" role="alert">{loadError}</p>}
            {!loading && !loadError && transcript.items.length === 0 && (
              <p className="chat-session__status">No messages yet. Send a prompt to start this conversation.</p>
            )}
            {transcript.items.map((item) => (
              <TranscriptItem item={item} key={item.id} runnerId={runner.id} mappingId={mappingId} sessionId={sessionId} cwd={cwd} pathContext={pathContext} onLayoutChange={pinIfPinned} />
            ))}
          </div>
          {goalOpen && (
            <div className="chat-goal-overlay" role="dialog" aria-modal="true" aria-label="Goal">
              <button
                className="chat-button chat-button--quiet chat-goal-overlay__close"
                type="button"
                aria-label="Close goal"
                onClick={() => {
                  setGoalOpen(false);
                  keepComposerFocus();
                }}
              >
                ×
              </button>
              <ChatGoalPanel sendCommand={sendCommand} events={events} />
            </div>
          )}
        </div>
        <div className="chat-session__composer">
          {(stripView || queuedSteering.length > 0) && (
            <div className="composer-status-row" id="composer-status-row">
            {stripView && (
            <div
              id="activity-strip"
              role="status"
              aria-live="polite"
              className={stripView.state === 'reconnecting' ? 'chat-slow-pulse' : undefined}
              data-state={stripView.state}
              data-session-id={sessionId}
              data-turn-id={activityStrip.turnId || undefined}
              data-tool-call-id={activityStrip.tools.at(-1)?.id || undefined}
              data-job-id={activityStrip.children.at(-1)?.jobId || undefined}
              data-agent-id={activityStrip.children.at(-1)?.agentId || undefined}
              title={stripView.text}
            >
              <span className="activity-strip__dot" aria-hidden="true" />
              <span className="activity-strip__text">{stripView.text}</span>
            </div>
            )}
            <SteeringQueue
              previews={queuedSteering}
              busy={queueBusy}
              onSendNow={sendQueuedNow}
              onCancelAt={cancelQueuedAt}
            />
            </div>
          )}
          {attachments.length > 0 && (
            <div className="chat-session__composer-attachments" aria-label="Attached files">
              {attachments.map((attachment) => (
                <span
                  key={attachment.id}
                  className="composer-attachment"
                  title={attachment.name}
                >
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
                  {canSendAttachmentToTmux(attachment) && (
                    <button
                      type="button"
                      className="composer-attachment__tmux"
                      title="Send to terminal"
                      aria-label={`Send ${attachment.name} to terminal`}
                      onClick={() => sendAttachmentToTerminal(attachment)}
                    >
                      <span aria-hidden="true">↗</span>
                    </button>
                  )}
                  <button
                    type="button"
                    className="composer-attachment__remove"
                    title="Remove attachment"
                    aria-label={`Remove ${attachment.name}`}
                    onClick={() => setAttachments((current) => current.filter((item) => item.id !== attachment.id))}
                  >
                    ✕
                  </button>
                </span>
              ))}
            </div>
          )}
          <div className="chat-session__composer-row">
          <CommandPicker
            connected={connected}
            sendCommand={(command) => sendCommand(command as RunnerCommand)}
            onSelect={insertSlashDraft}
            onSkillSelect={insertSlashDraft}
            onError={(message) => setComposerStatus(message)}
            getComposerValue={() => composerRef.current?.value ?? draft}
            onCatalog={ignoreCommandCatalog}
          />
          <textarea
            ref={composerRef}
            aria-label="Session prompt"
            enterKeyHint="send"
            onChange={(event) => setDraft(event.target.value)}
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
              if (event.key === 'Escape') {
                if (stopArmed) {
                  event.preventDefault();
                  abortTurn();
                }
                return;
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
            onPaste={(event) => {
              pasteEnterGuardUntilRef.current = nextPasteEnterGuard(performance.now());
              const files = event.clipboardData?.files;
              if (!files || files.length === 0) return;
              event.preventDefault();
              ingestFiles(files);
            }}
            placeholder={composerLocked
              ? composerDisabledReason
              : connected
                ? 'Message the agent… (Enter to send, Shift+Enter for a newline, Esc to stop)'
                : 'Reconnect this runner to send prompts.'}
            rows={1}
            value={draft}
          />
          <button
            className="chat-icon-button chat-session__attach"
            type="button"
            title="Attach images or code/text files"
            aria-label="Attach files"
            onClick={() => fileInputRef.current?.click()}
          >
            <span aria-hidden="true">📎</span>
          </button>
          <button
            className={`chat-button chat-button--primary${stopArmed ? ' chat-button--stop' : ''}`}
            disabled={!stopArmed && (!composerEnabled || sending || draft.trim() === '')}
            data-aborting={aborting ? 'true' : undefined}
            aria-pressed={aborting ? true : undefined}
            onPointerDown={(event: PointerEvent<HTMLButtonElement>) => {
              if (!stopArmed) return;
              if (event.button !== 0) return;
              event.preventDefault();
              abortTurn();
            }}
            onClick={() => {
              const click = composerActionClickKind({
                stopArmed,
                abortGestureClick: abortGestureClickRef.current,
              });
              abortGestureClickRef.current = false;
              if (click === 'ignore') return;
              if (click === 'abort') abortTurn();
              else void submit();
            }}
            type="button"
            aria-label={stopArmed ? 'Stop generating' : 'Send message'}
            title={stopArmed ? 'Stop generating (Esc)' : 'Send message (Enter)'}
          >
            {stopArmed ? '■ Stop' : sending ? 'Sending…' : 'Send'}
          </button>
          </div>
          <input
            ref={fileInputRef}
            type="file"
            accept={attachmentAccept()}
            multiple
            hidden
            onChange={(event) => {
              ingestFiles(event.target.files);
              event.target.value = '';
            }}
          />
        </div>
        {composerLocked && <p className="chat-session__status" role="status">{composerDisabledReason}</p>}
        {composerStatus && <p className="chat-session__feedback" role="alert">{composerStatus}</p>}
      </div>
    </section>
  );
}
