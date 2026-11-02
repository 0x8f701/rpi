/**
 * Shared typed transport-error classifier for App, SessionSidebar, and
 * SessionPanel. Transient socket failures stay off local error/status so
 * App remains the one-toast-per-outage authority. RPC/business errors
 * (`rpc === true`) are never classified as transport, even when their
 * message looks like a socket failure.
 */

const EXACT_TRANSPORT_MESSAGES: Record<string, true> = {
  'not connected': true,
  'connection replaced': true,
  'host switched': true,
};

const TRANSPORT_MESSAGE_PREFIXES = [
  'send failed:',
  'connection closed',
  'connection failed',
  'connection unresponsive',
] as const;

/** True for the established transport-failure message family. Empty and
 *  non-string values are never transport. */
export function isTransportErrorMessage(message: string): boolean {
  if (typeof message !== 'string' || message.length === 0) return false;
  if (EXACT_TRANSPORT_MESSAGES[message]) return true;
  for (const prefix of TRANSPORT_MESSAGE_PREFIXES) {
    if (message.startsWith(prefix)) return true;
  }
  return false;
}

/** True when `error` is a tagged transport failure or an untagged non-RPC
 *  error whose message matches the transport family. Non-objects are never
 *  transport. `transport === true` wins; `rpc === true` is always business. */
export function isTransportError(error: unknown): boolean {
  if (!error || typeof error !== 'object') return false;
  const candidate = error as { message?: unknown; rpc?: unknown; transport?: unknown };
  if (candidate.transport === true) return true;
  if (candidate.rpc === true || typeof candidate.message !== 'string') return false;
  return isTransportErrorMessage(candidate.message);
}

/** Shared catch-handler gate: whether a command rejection must raise the
 *  handler's OWN toast. Transport failures (socket drop, replaced,
 *  not-connected, send-failed) are owned by the transport layer's single
 *  outage toast; rpc-tagged errors are owned by onResponse (toastOwned) or
 *  stay quiet during bootstrap. Everything else (plain business errors,
 *  bounded command timeouts) surfaces exactly once from the handler. Used by
 *  the five side-chat catch handlers in App.tsx. */
export function shouldToastUnhandledCommandError(error: unknown): boolean {
  const candidate = error as { rpc?: unknown };
  return candidate.rpc !== true && !isTransportError(error);
}

/** Ownership of one rejection reaching a USER-OWNED auxiliary-probe catch
 *  (App markdown path open). Order matters:
 *  1. 'none' for rejections already surfaced by the rejecting layer
 *     (`toastOwned` — onResponse / owned drains).
 *  2. 'none' for transport-family failures — the shared persistent outage
 *     toast is the single authority, so a local toast would duplicate it.
 *  3. 'none' for drain-shaped rejections (`rpc === true`) whose message is
 *     STILL exactly in the transport family (e.g. the connect() replace
 *     drain rejects 'connection replaced' with rpc=true to keep the
 *     bootstrap path quiet). This does NOT weaken isTransportError's
 *     rpc-business invariant — the global classifier is unchanged; only
 *     this ownership gate special-cases drained outage reasons.
 *  'local' for everything else, surfacing exactly once from the handler:
 *  quiet rpc business failures from auxiliary panel-owned probes, bounded
 *  command timeouts, plain errors, and the 'bootstrap failed' drain
 *  (traced: rpc=true, toastOwned=false, reason outside the transport
 *  family; nothing else surfaces it — the reconnect banner owns recovery,
 *  the probe failure names its own cause). Null/undefined rejections
 *  (Promise.reject(null)) are 'local' — the handler renders its fixed
 *  fallback message. */
export function markdownOpenRejectionOwnership(error: unknown): 'none' | 'local' {
  if ((error as { toastOwned?: boolean } | null)?.toastOwned === true) return 'none';
  if (isTransportError(error)) return 'none';
  if (error && typeof error === 'object') {
    const candidate = error as { rpc?: unknown; message?: unknown };
    if (candidate.rpc === true && isTransportErrorMessage(typeof candidate.message === 'string' ? candidate.message : '')) return 'none';
  }
  return 'local';
}
