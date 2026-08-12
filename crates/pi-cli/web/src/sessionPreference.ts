/**
 * Listener-scoped session preference — persists the last activated session id
 * per rpi connection authority. App restores the selected listener authority
 * before bootstrap, so reload reads the same listener bucket even when the
 * page origin and WebSocket target use different host strings. Only the
 * non-secret session identity is stored — never tokens, transcripts, or
 * filesystem paths.
 *
 * Storage is passed in as a parameter. Each helper catches storage-operation
 * errors and degrades to '' / no-op, so blocked storage never breaks sessions.
 */
import type { StorageLike } from './hostToken';

const SESSION_PREF_PREFIX = 'rpi-web-session:';


/** The host-scoped localStorage key for `authority`:
 *  `rpi-web-session:<encodeURIComponent(authority.trim())>`. The authority is
 *  encoded so any port/IPv6 colons or unusual chars are safe in a key while
 *  still encoding the EXACT trimmed authority (no origin-wide fallback). */
export function sessionPreferenceKey(authority: string): string {
  return `${SESSION_PREF_PREFIX}${encodeURIComponent(authority.trim())}`;
}

/** Read the saved session id for `authority` from its SCOPED key only;
 *  returns trimmed id or '' when absent/unreadable. Never throws. */
export function loadSessionPreference(storage: StorageLike | null, authority: string): string {
  if (!storage) return '';
  try {
    return (storage.getItem(sessionPreferenceKey(authority)) || '').trim();
  } catch {
    return '';
  }
}

/** Persist `sessionId` for `authority` under the SCOPED key only: sets it when
 *  non-empty, removes it when empty. Never throws. */
export function saveSessionPreference(storage: StorageLike | null, authority: string, sessionId: string): void {
  if (!storage) return;
  const key = sessionPreferenceKey(authority);
  try {
    const value = sessionId.trim();
    if (value) storage.setItem(key, value);
    else storage.removeItem(key);
  } catch {
    /* private mode / quota — preference lives in memory only */
  }
}

/** The session_list catalog fields the selector needs (superset-safe: the
 *  backend RpcSessionListRow carries more, and sessionPreference.ts stays
 *  DOM/React-free so the node test bundles without a UI dependency). */
export interface SessionPreferenceRow {
  sessionId: string;
  path: string;
}

/** Pure catalog selector: the SAVED session's row when it exists in the
 *  catalog, otherwise the FIRST row, otherwise null (empty/unusable catalog).
 *  Rows with empty sessionId/path are skipped (never a restore target), so
 *  junk wire rows can never be switched to. The saved match wins over
 *  position — restoring the user's choice even when it is not the newest
 *  catalog row. */
export function selectSessionFromCatalog(
  rows: readonly SessionPreferenceRow[] | null | undefined,
  savedId: string,
): SessionPreferenceRow | null {
  const saved = savedId.trim();
  let first: SessionPreferenceRow | null = null;
  for (const row of rows ?? []) {
    if (
      !row ||
      typeof row.sessionId !== 'string' ||
      row.sessionId === '' ||
      typeof row.path !== 'string' ||
      row.path === ''
    ) {
      continue;
    }
    if (first === null) first = row;
    if (saved !== '' && row.sessionId === saved) return row;
  }
  return first;
}
