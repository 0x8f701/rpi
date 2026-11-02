export type TransportFailureKind = 'cert' | 'auth' | 'closed';

export const RUNNER_CERT_TRUST_HINT =
  "Can't reach this machine — check the address. If it uses a self-signed certificate, tap Trust certificate and confirm the fingerprint.";

export function parseTransportFailure(raw: string): TransportFailureKind {
  const text = raw.toLowerCase();
  if (
    text.includes('ssl')
    || text.includes('cert')
    || text.includes('trust anchor')
    || text.includes('handshake')
    || text.includes('pkix')
    || text.includes('cleartext')
  ) {
    return 'cert';
  }
  if (text.includes('token') || text.includes('unauthorized') || text.includes('auth')) {
    return 'auth';
  }
  return 'closed';
}

export function isClosedTransportFailure(err: unknown): boolean {
  const message = err instanceof Error ? err.message : String(err);
  return parseTransportFailure(message) === 'closed';
}

/** The transport's typed disconnected failure. Every pending command — and
 *  every command queued behind the ready gate — is rejected with this class
 *  when its socket is gone (a close, an auth reset, a retired client). A typed
 *  discriminator, not a prose match: a command timeout, an ownership rejection,
 *  or an unexpected error must surface, never be swallowed as "reconnecting". */
export class RunnerDisconnectedError extends Error {
  constructor() {
    super('Runner is not connected.');
    this.name = 'RunnerDisconnectedError';
  }
}

/** Exact guard for catch sites: true ONLY for `RunnerDisconnectedError`. */
export function isRunnerDisconnected(err: unknown): boolean {
  return err instanceof RunnerDisconnectedError;
}

export function transportFailureMessage(kind: TransportFailureKind, raw: string): string {
  if (kind === 'cert') return RUNNER_CERT_TRUST_HINT;
  if (kind === 'auth') return 'Token required';
  return raw.trim() || 'Runner unreachable.';
}
