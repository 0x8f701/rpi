import AsyncStorage from '@react-native-async-storage/async-storage';

export const MAX_CHANNEL_MESSAGES = 80;
export const MAX_CHANNEL_MESSAGE_CHARS = 8_000;
export const CHANNEL_LOG_STORAGE_KEY = 'rpi-mobile-chat-channel-log:v1';

export interface ChannelLogMessage {
  id: string;
  at: number;
  kind: 'human' | 'runner';
  mappingId: string | null;
  author: string;
  text: string;
}

function boundMessage(message: ChannelLogMessage): ChannelLogMessage {
  return {
    ...message,
    author: message.author.length <= 80 ? message.author : message.author.slice(0, 80),
    text: message.text.length <= MAX_CHANNEL_MESSAGE_CHARS
      ? message.text
      : `${message.text.slice(0, MAX_CHANNEL_MESSAGE_CHARS)}\n…`,
  };
}

export function boundChannelMessages(messages: readonly ChannelLogMessage[]): ChannelLogMessage[] {
  const bounded = messages.map(boundMessage);
  if (bounded.length <= MAX_CHANNEL_MESSAGES) return bounded;
  return bounded.slice(-MAX_CHANNEL_MESSAGES);
}

export function parseChannelLogs(raw: string | null): Record<string, ChannelLogMessage[]> {
  if (!raw) return {};
  try {
    const parsed = JSON.parse(raw) as { logs?: unknown };
    if (!parsed || typeof parsed !== 'object' || !parsed.logs || typeof parsed.logs !== 'object') return {};
    const out: Record<string, ChannelLogMessage[]> = {};
    for (const [projectId, rows] of Object.entries(parsed.logs as Record<string, unknown>)) {
      if (!Array.isArray(rows)) continue;
      const messages: ChannelLogMessage[] = [];
      for (const row of rows) {
        if (!row || typeof row !== 'object') continue;
        const value = row as Record<string, unknown>;
        if (typeof value.id !== 'string' || typeof value.text !== 'string') continue;
        if (value.kind !== 'human' && value.kind !== 'runner') continue;
        messages.push(boundMessage({
          id: value.id,
          at: typeof value.at === 'number' ? value.at : 0,
          kind: value.kind,
          mappingId: typeof value.mappingId === 'string' ? value.mappingId : null,
          author: typeof value.author === 'string' ? value.author : '',
          text: value.text,
        }));
      }
      out[projectId] = boundChannelMessages(messages);
    }
    return out;
  } catch {
    return {};
  }
}

const memoryLogs: Record<string, ChannelLogMessage[]> = {};

export function channelLogsFor(projectId: string): ChannelLogMessage[] {
  return memoryLogs[projectId] ? [...memoryLogs[projectId]] : [];
}

export function replaceChannelLogs(projectId: string, messages: ChannelLogMessage[]): ChannelLogMessage[] {
  memoryLogs[projectId] = boundChannelMessages(messages);
  return channelLogsFor(projectId);
}

export function appendChannelLog(projectId: string, message: ChannelLogMessage): ChannelLogMessage[] {
  return replaceChannelLogs(projectId, [...channelLogsFor(projectId), message]);
}

export function hydrateChannelLogs(raw: string | null): void {
  const parsed = parseChannelLogs(raw);
  for (const [projectId, messages] of Object.entries(parsed)) {
    memoryLogs[projectId] = messages;
  }
}

export function serializeChannelLogs(): string {
  return JSON.stringify({ version: 1, logs: memoryLogs });
}

export async function loadPersistedChannelLogs(): Promise<void> {
  let raw: string | null = null;
  try {
    raw = await AsyncStorage.getItem(CHANNEL_LOG_STORAGE_KEY);
  } catch {
    raw = null;
  }
  hydrateChannelLogs(raw);
}

export async function persistChannelLogs(): Promise<void> {
  try {
    await AsyncStorage.setItem(CHANNEL_LOG_STORAGE_KEY, serializeChannelLogs());
  } catch {
    return;
  }
}
