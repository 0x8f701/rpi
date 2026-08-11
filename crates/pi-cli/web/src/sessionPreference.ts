/**
 * Host-scoped session preference — pure helpers that persist the last
 * ACTIVATED session id per rpi listener authority under a per-authority
 * localStorage key, so a session selected on host A is never restored on
 * host B (mirrors the hostToken.ts authority isolation for auth tokens).
 * Shared by App.tsx's bootstrap restoration / onLifecycleResult and the
 * node-runnable regression test (scripts/sessionPreference.test.ts).
 *
 * Only the NON-SECRET session identity (sessionId) is stored — never tokens,
 * transcripts, or filesystem paths. Storage is passed in as a parameter (no
 * module-level state, no DOM coupling), so the node test drives an in-memory
 * StorageLike directly. Each helper catches storage-operation errors and
 * degrades to '' / no-op, so private mode or blocked cookies never crash the
 * session flow.
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
