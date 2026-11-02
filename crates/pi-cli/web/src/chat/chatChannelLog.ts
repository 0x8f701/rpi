import { contentText, type Item } from '../transcript';
import type { StorageLike } from '../hostToken';
import { runnerMentionToken } from './chatMentions';

export const MAX_CHANNEL_MESSAGES = 80;
export const MAX_WAKE_LOG_MESSAGES = 12;
export const MAX_WAKE_TEXT_CHARS = 4_000;
export const MAX_CHANNEL_MESSAGE_CHARS = 8_000;
export const CHAT_CHANNEL_LOG_STORAGE_PREFIX = 'rpi-web-chat-channel-log:v1:';
const CHANNEL_LOG_STORAGE_VERSION = 1;
const MAX_STORED_AUTHOR_CHARS = 80;
const MAX_STORED_ID_CHARS = 128;

export interface ChannelMessage {
  id: string;
  at: number;
  kind: 'human' | 'runner';
  mappingId: string | null;
  author: string;
  text: string;
}

export interface WakeBriefInput {
  channelLabel: string;
  selfMention: string;
  teammateMentions: readonly string[];
  messages: readonly ChannelMessage[];
}

export type ChannelLogErrorKind = 'corrupt' | 'quota' | 'io';

export interface ChannelLogError {
  kind: ChannelLogErrorKind;
}

export type ChannelLogLoadResult =
  | { ok: true; logs: Record<string, ChannelMessage[]> }
  | { ok: false; error: ChannelLogError; logs: Record<string, ChannelMessage[]> };

export type ChannelLogSaveResult =
  | { ok: true }
  | { ok: false; error: ChannelLogError };

export function boundChannelMessage(message: ChannelMessage): ChannelMessage {
  return {
    id: message.id,
    at: message.at,
    kind: message.kind,
    mappingId: message.mappingId,
    author: message.author.length <= MAX_STORED_AUTHOR_CHARS
      ? message.author
      : message.author.slice(0, MAX_STORED_AUTHOR_CHARS),
    text: message.text.length <= MAX_CHANNEL_MESSAGE_CHARS
      ? message.text
      : `${message.text.slice(0, MAX_CHANNEL_MESSAGE_CHARS)}\n…`,
  };
}

export function boundChannelMessages(messages: readonly ChannelMessage[]): ChannelMessage[] {
  const bounded = messages.map(boundChannelMessage);
  if (bounded.length <= MAX_CHANNEL_MESSAGES) return bounded;
  return bounded.slice(-MAX_CHANNEL_MESSAGES);
}

export function boundWakeText(text: string): string {
  const trimmed = text.trim();
  if (trimmed.length <= MAX_WAKE_TEXT_CHARS) return trimmed;
  return `${trimmed.slice(0, MAX_WAKE_TEXT_CHARS)}\n…`;
}

export function assistantPlainText(items: readonly Item[]): string {
  const parts: string[] = [];
  for (const item of items) {
    if (item.kind !== 'assistant') continue;
    const text = contentText(item.blocks).trim();
    if (text) parts.push(text);
  }
  return parts.join('\n').trim();
}

export function composeWakeBrief(input: WakeBriefInput): string {
  const channel = runnerMentionToken(input.channelLabel);
  const lines = [`You are @${input.selfMention} in #${channel}.`];
  if (input.teammateMentions.length === 0) {
    lines.push('You are the only runner in this channel.');
  } else {
    lines.push(`Teammates: ${input.teammateMentions.map((name) => `@${name}`).join(', ')}.`);
  }
  const recent = input.messages.slice(-MAX_WAKE_LOG_MESSAGES);
  if (recent.length > 0) {
    lines.push('Channel so far:');
    for (const message of recent) {
      const body = boundWakeText(message.text);
      if (message.kind === 'human') lines.push(`You: ${body}`);
      else lines.push(`@${message.author}: ${body}`);
    }
  }
  lines.push('Reply in this channel. @ a teammate to hand off work.');
  return lines.join('\n');
}

export function channelLogStorageKey(authority: string): string {
  return `${CHAT_CHANNEL_LOG_STORAGE_PREFIX}${encodeURIComponent(authority.trim())}`;
}

export function channelLogErrorMessage(error: ChannelLogError): string {
  switch (error.kind) {
    case 'corrupt':
      return 'Channel log could not be read.';
    case 'quota':
      return 'Channel log could not be saved.';
    case 'io':
      return 'Channel log storage failed.';
  }
}

export function loadChannelLogs(storage: StorageLike | null, authority: string): ChannelLogLoadResult {
  if (!storage || authority.trim() === '') return { ok: true, logs: {} };
  try {
    const raw = storage.getItem(channelLogStorageKey(authority));
    if (!raw) return { ok: true, logs: {} };
    const parsed: unknown = JSON.parse(raw);
    if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) {
      return { ok: false, error: { kind: 'corrupt' }, logs: {} };
    }
    const document = parsed as { version?: unknown; projects?: unknown };
    if (document.version !== CHANNEL_LOG_STORAGE_VERSION) return { ok: true, logs: {} };
    if (!document.projects || typeof document.projects !== 'object' || Array.isArray(document.projects)) {
      return { ok: false, error: { kind: 'corrupt' }, logs: {} };
    }
    const logs: Record<string, ChannelMessage[]> = {};
    for (const [projectId, rows] of Object.entries(document.projects as Record<string, unknown>)) {
      if (!projectId || projectId.length > MAX_STORED_ID_CHARS) {
        return { ok: false, error: { kind: 'corrupt' }, logs: {} };
      }
      if (!Array.isArray(rows)) {
        return { ok: false, error: { kind: 'corrupt' }, logs: {} };
      }
      const messages: ChannelMessage[] = [];
      for (const row of rows) {
        const message = parseStoredChannelMessage(row);
        if (!message) return { ok: false, error: { kind: 'corrupt' }, logs: {} };
        messages.push(message);
      }
      if (messages.length > 0) logs[projectId] = boundChannelMessages(messages);
    }
    return { ok: true, logs };
  } catch (error) {
    return { ok: false, error: channelLogErrorFromUnknown(error), logs: {} };
  }
}

export function saveChannelLogs(
  storage: StorageLike | null,
  authority: string,
  logs: Record<string, readonly ChannelMessage[]>,
): ChannelLogSaveResult {
  if (!storage || authority.trim() === '') return { ok: true };
  const key = channelLogStorageKey(authority);
  try {
    const projects: Record<string, ChannelMessage[]> = {};
    for (const [projectId, rows] of Object.entries(logs)) {
      if (!projectId || projectId.length > MAX_STORED_ID_CHARS) continue;
      const messages = boundChannelMessages(rows);
      if (messages.length > 0) projects[projectId] = messages;
    }
    storage.setItem(key, JSON.stringify({ version: CHANNEL_LOG_STORAGE_VERSION, projects }));
    return { ok: true };
  } catch (error) {
    return { ok: false, error: channelLogErrorFromUnknown(error) };
  }
}

function channelLogErrorFromUnknown(error: unknown): ChannelLogError {
  if (isQuotaExceeded(error)) return { kind: 'quota' };
  if (error instanceof SyntaxError) return { kind: 'corrupt' };
  return { kind: 'io' };
}

function isQuotaExceeded(error: unknown): boolean {
  if (typeof DOMException !== 'undefined' && error instanceof DOMException && error.name === 'QuotaExceededError') {
    return true;
  }
  return error instanceof Error && error.name === 'QuotaExceededError';
}

function parseStoredChannelMessage(value: unknown): ChannelMessage | null {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return null;
  const row = value as {
    id?: unknown;
    at?: unknown;
    kind?: unknown;
    mappingId?: unknown;
    author?: unknown;
    text?: unknown;
  };
  if (typeof row.id !== 'string' || row.id.trim() === '' || row.id.length > MAX_STORED_ID_CHARS) return null;
  if (typeof row.at !== 'number' || !Number.isFinite(row.at)) return null;
  if (row.kind !== 'human' && row.kind !== 'runner') return null;
  if (row.mappingId !== null && (typeof row.mappingId !== 'string' || row.mappingId.length > MAX_STORED_ID_CHARS)) return null;
  if (typeof row.author !== 'string' || row.author.trim() === '') return null;
  if (typeof row.text !== 'string') return null;
  return boundChannelMessage({
    id: row.id,
    at: row.at,
    kind: row.kind,
    mappingId: row.mappingId,
    author: row.author,
    text: row.text,
  });
}
