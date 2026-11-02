/**
 * Host-scoped token storage — pure helpers that keep each rpi listener's auth
 * token under a per-authority localStorage key, so a token saved for host A is
 * never sent to a different host B. Shared by App.tsx and the node-runnable
 * regression test (scripts/hostToken.test.ts).
 *
 * Storage is passed in as a parameter (no module-level state, no DOM coupling),
 * so the node test drives an in-memory StorageLike directly. Each helper
 * catches storage-operation errors and degrades to '' / no-op, so private mode
 * or blocked cookies never crash the connection flow.
 */

/** The legacy global token key (pre-host-scoping). Read ONLY by the initial
 *  boot loader and removed after successful scoped migration. A failed scoped
 *  write retains it solely so a StrictMode remount can retry the same initial
 *  authority; arbitrary later host loads never consult it. */
export const LEGACY_TOKEN_KEY = 'rpi-web-token';
const HOST_TOKEN_PREFIX = 'rpi-web-token:';

/** Minimal localStorage-like backend the helpers need. */
export interface StorageLike {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
  removeItem(key: string): void;
}

/** The exact trimmed authority (host[:port]) connect() opens the WebSocket to.
 *  Kept EXACT (no lowercasing) so two distinct authorities never share a token
 *  bucket. */
export function normalizeHostAuthority(raw: string): string {
  return raw.trim();
}

/** The host-scoped localStorage key for `authority`:
 *  `rpi-web-token:<encodeURIComponent(authority.trim())>`. The authority is
 *  encoded so any port/IPv6 colons or unusual chars are safe in a key while
 *  still encoding the EXACT trimmed authority (no origin-wide fallback). */
export function hostTokenKey(authority: string): string {
  return `${HOST_TOKEN_PREFIX}${encodeURIComponent(normalizeHostAuthority(authority))}`;
}
/** Read the token for `authority` from the SCOPED key only (never legacy);
 *  returns trimmed token or '' when absent/unreadable. */
export function loadTokenForAuthority(storage: StorageLike | null, authority: string): string {
  if (!storage) return '';
  try {
    return (storage.getItem(hostTokenKey(authority)) || '').trim();
  } catch {
    return '';
  }
}

/** Persist `token` for `authority` under the SCOPED key only (never touches the
 *  legacy key): sets it when non-empty, removes it when empty. Never throws. */
export function saveTokenForAuthority(storage: StorageLike | null, authority: string, token: string): void {
  if (!storage) return;
  const key = hostTokenKey(authority);
  try {
    const value = token.trim();
    if (value) storage.setItem(key, value);
    else storage.removeItem(key);
  } catch {
    /* private mode / quota — token lives in ref/state only */
  }
}

/** Initial boot loader for the page authority. A legacy token is removed only
 *  after the chosen token is durably present under the scoped key. If the
 *  scoped write fails, the legacy value remains available for the next
 *  StrictMode mount/retry while this call still returns it for connection. */
export function loadInitialAuthorityToken(storage: StorageLike | null, initialAuthority: string): string {
  if (!storage) return '';
  const scopedKey = hostTokenKey(initialAuthority);
  let scoped = '';
  let legacy = '';
  try {
    scoped = (storage.getItem(scopedKey) || '').trim();
  } catch {
    scoped = '';
  }
  try {
    legacy = (storage.getItem(LEGACY_TOKEN_KEY) || '').trim();
  } catch {
    legacy = '';
  }
  if (scoped) {
    try {
      storage.removeItem(LEGACY_TOKEN_KEY);
    } catch {
      /* legacy cleanup best-effort */
    }
    return scoped;
  }
  if (!legacy) return '';

  let migrated = false;
  try {
    storage.setItem(scopedKey, legacy);
    migrated = true;
  } catch {
    /* retain legacy as the retry source */
  }
  if (migrated) {
    try {
      storage.removeItem(LEGACY_TOKEN_KEY);
    } catch {
      /* legacy cleanup best-effort */
    }
  }
  return legacy;
}