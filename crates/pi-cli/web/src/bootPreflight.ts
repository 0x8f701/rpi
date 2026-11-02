/**
 * Bounded unauthenticated `/status` preflight for the App's empty-token boot.
 *
 * A token-gated listener refuses the tokenless WebSocket handshake with 401,
 * so blindly connecting would only produce a silent reconnect loop. This
 * module owns the pure boot decision: given the strict-parsed `/status` body
 * and the stored token, decide whether to connect or hold in the actionable
 * `token_required` state. Shared by App.tsx and the node-runnable regression
 * test (scripts/bootPreflight.test.ts).
 *
 * Failure policy: the preflight endpoint is auxiliary. When it is
 * unreachable, times out, or returns a malformed body the decision degrades
 * to the historical behavior (attempt the connection) so proxy/degraded
 * setups are never bricked by the probe. No secret value is ever logged.
 */

import {
  parseRunnerEndpoint,
  fetchRunnerStatus,
  type PageHttpProtocol,
  type RunnerStatus,
} from './chat/auth';

/** Bounded wait for the preflight fetch; one attempt, no retries. */
export const BOOT_PREFLIGHT_TIMEOUT_MS = 4000;

/** Boot decision projected from the preflight + stored token.
 *  - `connect`: open the WebSocket now (saved token, tokenless listener, or
 *    an unavailable/malformed preflight — the historical attempt).
 *  - `token_required`: hold. No socket is opened; the UI shows the
 *    actionable message and Settings → Connection hosts the token input. */
export type BootConnDecision = 'connect' | 'token_required';

/**
 * Pure boot decision. A non-empty stored token ALWAYS connects immediately
 * (the preflight would only add latency; a wrong token keeps the existing
 * actionable close-surface failure). With an empty token, only a strict
 * preflight body claiming `tokenRequired: true` holds the boot.
 */
export function decideBootConnection(
  status: RunnerStatus | null | undefined,
  token: string,
): BootConnDecision {
  if (token !== '') return 'connect';
  if (status !== null && status !== undefined && status.tokenRequired) return 'token_required';
  return 'connect';
}

/** Token commit while the boot is held in `token_required`: a non-empty
 *  token releases the hold and reconnects exactly once; an empty save stays
 *  held (connecting tokenless would only be refused again). */
export type TokenHoldAction = 'stay-held' | 'release-and-connect';

export function tokenCommitWhileHeld(trimmedToken: string): TokenHoldAction {
  return trimmedToken !== '' ? 'release-and-connect' : 'stay-held';
}

/** Consecutive never-opened reconnect failures after which the reconnect
 *  loop RE-PROBES /status instead of blind-connecting: a listener restart
 *  can flip a tokenless-accepted endpoint into token-required mode, and the
 *  refused handshakes would otherwise loop silently forever (the page had
 *  connected once, so the ever-connected close surface is suppressed). */
export const TOKEN_REPROBE_FAILURES = 3;

/** True when the reconnect loop should re-run the /status preflight. The
 *  counter counts consecutive reconnect attempts whose socket closed before
 *  onopen; every TOKEN_REPROBE_FAILURES-th failure re-probes once. */
export function shouldReprobeTokenRequirement(consecutiveNeverOpened: number): boolean {
  return consecutiveNeverOpened > 0 && consecutiveNeverOpened % TOKEN_REPROBE_FAILURES === 0;
}

/** Bounded hold re-probe cadence (ms) while the boot is held in
 *  token_required: the listener may have restarted tokenless (or auth was
 *  disabled), and a hold that never re-probes would pin the page in Settings
 *  even though a reconnect would now succeed. */
export const TOKEN_HOLD_REPROBE_MS = 10_000;

/** Pure hold re-probe decision: the stored token + a fresh /status body
 *  re-decide the boot. A hold releases when the decision flips to connect
 *  (listener no longer token-required, or the probe is unavailable — the
 *  never-brick policy); it stays held only on a definitive token_required. */
export function shouldReleaseTokenHold(
  status: RunnerStatus | null | undefined,
  token: string,
): boolean {
  return decideBootConnection(status, token) === 'connect';
}

/** Wake / network-return action. Empty-token pages must not open a socket
 *  while boot preflight is still the owner of the first connection.
 *
 *  An OPEN socket is always probed with an application ping. Application-message
 *  age is not liveness: hidden-tab timer throttling freezes lastMessageAt while
 *  the browser still auto-pongs server RFC6455 pings (listen idle is 30s of no
 *  inbound frames including those pongs). A 15s stale-on-wake tear-down was
 *  self-inflicted reconnect on every brief tab hide. */
export type WakeTransportAction = 'preflight' | 'ping' | 'reconnect';

export function wakeTransportAction(options: {
  tokenHold: boolean;
  socketOpen: boolean;
  hasStoredToken: boolean;
}): WakeTransportAction {
  if (options.tokenHold || (!options.socketOpen && !options.hasStoredToken)) return 'preflight';
  if (options.socketOpen) return 'ping';
  return 'reconnect';
}

export interface BootPreflightOptions {
  /** Injectable fetch for the node test (defaults to global fetch). */
  fetchFn?: typeof fetch;
  /** Bounded wait in ms; the single attempt aborts past it. */
  timeoutMs?: number;
  /** Page HTTP protocol; defaults to the serving page's `location.protocol`. */
  pageProtocol?: PageHttpProtocol;
}

/** Outcome of one bounded authenticated `POST /rpc` with the stored token.
 *  - `unauthorized`: listener is up and rejected the bearer (HTTP 401).
 *  - `accepted`: listener is up and the bearer passed `authorized()`.
 *  - `unreachable`: network/timeout/malformed — not evidence of a bad token. */
export type ListenAuthProbe = 'unauthorized' | 'accepted' | 'unreachable';

/**
 * Distinguish a wrong stored token from a down listener. Browser WebSocket
 * 401 surfaces as 1006 with no close reason, the same shape as connection
 * refused. `POST /rpc` with `Authorization: Bearer` is gated by the same
 * `authorized()` check: 401 is the only auth-hold evidence. The token is
 * never logged. Failure degrades to `unreachable` so reconnect continues.
 */
export async function probeListenAuth(
  authority: string,
  token: string,
  options: BootPreflightOptions = {},
): Promise<ListenAuthProbe> {
  if (authority.trim() === '' || token === '') return 'unreachable';
  const timeoutMs = options.timeoutMs ?? BOOT_PREFLIGHT_TIMEOUT_MS;
  try {
    const endpoint = parseRunnerEndpoint(authority, options.pageProtocol);
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), timeoutMs);
    try {
      const fetchFn = options.fetchFn ?? fetch;
      const response = await fetchFn(`${endpoint.origin}/rpc`, {
        method: 'POST',
        headers: {
          Authorization: `Bearer ${token}`,
          'Content-Type': 'application/json',
        },
        body: '{}',
        signal: controller.signal,
      });
      return response.status === 401 ? 'unauthorized' : 'accepted';
    } finally {
      clearTimeout(timer);
    }
  } catch {
    return 'unreachable';
  }
}

/**
 * One bounded, unauthenticated `GET /status` against the serving page
 * authority. Resolves `null` for any unreachable/malformed outcome (the
 * caller then connects as before) and never rejects. The strict body
 * contract is `readRunnerStatus` (`{ tokenRequired: boolean, version: string,
 * codename: string | null }` — nothing else is trusted).
 */
export async function fetchBootStatus(
  authority: string,
  options: BootPreflightOptions = {},
): Promise<RunnerStatus | null> {
  const timeoutMs = options.timeoutMs ?? BOOT_PREFLIGHT_TIMEOUT_MS;
  try {
    const endpoint = parseRunnerEndpoint(authority, options.pageProtocol);
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), timeoutMs);
    try {
      const fetchFn = options.fetchFn ?? fetch;
      const result = await fetchRunnerStatus(endpoint, fetchFn, controller.signal);
      return result.ok ? result.status : null;
    } finally {
      clearTimeout(timer);
    }
  } catch {
    return null;
  }
}