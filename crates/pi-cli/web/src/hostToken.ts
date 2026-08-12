/**
 * Host-scoped token storage — pure helpers that keep each rpi listener's auth
 * token under a per-authority localStorage key, so a token saved for host A is
 * never sent to a different host B (cross-origin credential leak). Shared by
 * App.tsx's boot/commitHost/handleTokenChange and the node-runnable regression
 * test (scripts/hostToken.test.ts).
 *
 * Storage is passed in as a parameter (no module-level state, no DOM coupling),
 * so the node test drives an in-memory StorageLike directly. Each helper
 * catches storage-operation errors and degrades to '' / no-op, so private mode
 * or blocked cookies never crash the connection flow.
 */

/** The legacy global token key (pre-host-scoping). Read ONLY by the initial
 *  boot loader, migrated to the initial authority's scoped key, then removed —
 *  never used as a fallback afterward, so an old global token can never travel
 *  to a different host. */
export const LEGACY_TOKEN_KEY = 'rpi-web-token';

/** Last listener authority selected by the host input for this page origin. */
export const ACTIVE_HOST_KEY = 'rpi-web-active-host';

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

/** Restore the last selected listener, falling back to the page authority. */
export function loadActiveHost(storage: StorageLike | null, pageAuthority: string): string {
  const fallback = normalizeHostAuthority(pageAuthority);
  if (!storage) return fallback;
  try {
    return normalizeHostAuthority(storage.getItem(ACTIVE_HOST_KEY) || '') || fallback;
  } catch {
    return fallback;
  }
}

/** Persist a non-empty listener authority; never throws. */
export function saveActiveHost(storage: StorageLike | null, authority: string): void {
  if (!storage) return;
  const value = normalizeHostAuthority(authority);
  if (!value) return;
  try {
    storage.setItem(ACTIVE_HOST_KEY, value);
  } catch {
    /* private mode / quota — active listener remains in memory */
  }
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

/** Initial boot loader for the page authority: reads the CURRENT scoped token
 *  first, reads the LEGACY global key ONLY here, prefers an existing non-empty
 *  scoped token over legacy, otherwise saves the trimmed legacy under the
 *  initial authority, then attempts to remove the legacy key REGARDLESS of
 *  which was chosen. Returns the chosen token (even if persistence failed) so
 *  the initial connection works; subsequent hosts never touch legacy. */
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
  // Prefer an existing non-empty scoped token over the legacy global token.
  if (scoped) {
    try {
      storage.removeItem(LEGACY_TOKEN_KEY);
    } catch {
      /* legacy cleanup best-effort */
    }
    return scoped;
  }
  // No scoped token: migrate the legacy token under the initial authority.
  if (legacy) {
    try {
      storage.setItem(scopedKey, legacy);
    } catch {
      /* persistence best-effort — return the token anyway so connect works */
    }
  }
  // Remove the legacy key regardless of whether a migration happened.
  try {
    storage.removeItem(LEGACY_TOKEN_KEY);
  } catch {
    /* legacy cleanup best-effort */
  }
  return legacy;
}