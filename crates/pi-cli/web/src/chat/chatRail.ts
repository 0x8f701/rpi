import { normalizeHostAuthority, type StorageLike } from '../hostToken';

export const CHAT_RAIL_FOCUS_STORAGE_PREFIX = 'rpi-web-chat-rail:v1:';

export type ChatRailFocus =
  | { kind: 'project'; id: string }
  | { kind: 'runner'; id: string };

export function chatRailFocusStorageKey(authority: string): string {
  return `${CHAT_RAIL_FOCUS_STORAGE_PREFIX}${encodeURIComponent(normalizeHostAuthority(authority))}`;
}

export function parseChatRailFocus(raw: string): ChatRailFocus | null {
  const trimmed = raw.trim();
  if (!trimmed || trimmed.length > 256) return null;
  let parsed: unknown;
  try {
    parsed = JSON.parse(trimmed);
  } catch {
    return null;
  }
  if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) return null;
  const row = parsed as { kind?: unknown; id?: unknown };
  if (row.kind !== 'project' && row.kind !== 'runner') return null;
  if (typeof row.id !== 'string') return null;
  const id = row.id.trim();
  if (!id || id.length > 128 || /[/\\\n]/.test(id)) return null;
  return { kind: row.kind, id };
}

export function loadChatRailFocus(storage: StorageLike | null, authority: string): ChatRailFocus | null {
  if (!storage) return null;
  try {
    return parseChatRailFocus(storage.getItem(chatRailFocusStorageKey(authority)) || '');
  } catch {
    return null;
  }
}

export function saveChatRailFocus(
  storage: StorageLike | null,
  authority: string,
  focus: ChatRailFocus | null,
): void {
  if (!storage) return;
  const key = chatRailFocusStorageKey(authority);
  try {
    if (!focus) {
      storage.removeItem(key);
      return;
    }
    const id = focus.id.trim();
    if (!id || id.length > 128 || /[/\\\n]/.test(id)) {
      storage.removeItem(key);
      return;
    }
    storage.setItem(key, JSON.stringify({ kind: focus.kind, id }));
  } catch {
    /* private mode / quota — selection lives in memory only */
  }
}

export function selectStoredRailFocus<T extends { id: string }>(
  rows: readonly T[],
  saved: ChatRailFocus | null,
): T | null {
  if (!saved) return null;
  return rows.find((row) => row.id === saved.id) ?? null;
}
