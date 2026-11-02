import type { StorageLike } from '../hostToken';
import { loadTokenForAuthority, saveTokenForAuthority } from '../hostToken';

const HTTP_PROTOCOL = 'http:';
const HTTPS_PROTOCOL = 'https:';

export type PageHttpProtocol = 'http:' | 'https:';

export interface RunnerEndpoint {
  origin: string;
  authority: string;
  webSocketUrl: string;
}

export interface RunnerStatus {
  tokenRequired: boolean;
  version: string;
  codename: string | null;
}

export type RunnerStatusResult =
  | { ok: true; status: RunnerStatus }
  | { ok: false };

export const RUNNER_UNREACHABLE_MESSAGE =
  "Can't reach this machine — check the address. If it uses a self-signed certificate, open that machine's /chat page once and accept it.";

export const RUNNER_TOKEN_REQUIRED_MESSAGE = 'Token required';

export class RunnerAuthRequiredError extends Error {
  readonly kind = 'token_required' as const;

  constructor(message = RUNNER_TOKEN_REQUIRED_MESSAGE) {
    super(message);
    this.name = 'RunnerAuthRequiredError';
  }
}

function resolvePageProtocol(pageProtocol?: PageHttpProtocol): PageHttpProtocol {
  if (pageProtocol === HTTP_PROTOCOL || pageProtocol === HTTPS_PROTOCOL) return pageProtocol;
  const fromLocation = readLocationProtocol();
  return fromLocation === HTTPS_PROTOCOL ? HTTPS_PROTOCOL : HTTP_PROTOCOL;
}

function readLocationProtocol(): string | undefined {
  try {
    const globalLocation = (globalThis as { location?: { protocol?: string } }).location?.protocol;
    if (globalLocation) return globalLocation;
    return (globalThis as { window?: { location?: { protocol?: string } } }).window?.location?.protocol;
  } catch {
    return undefined;
  }
}

export function parseRunnerEndpoint(raw: string, pageProtocol?: PageHttpProtocol): RunnerEndpoint {
  const value = raw.trim();
  if (!value) throw new Error('Runner address is required.');

  let url: URL;
  try {
    url = new URL(value.includes('://') ? value : `${resolvePageProtocol(pageProtocol)}//${value}`);
  } catch {
    throw new Error('Runner address must be an HTTP or HTTPS origin.');
  }
  if (url.protocol !== HTTP_PROTOCOL && url.protocol !== HTTPS_PROTOCOL) {
    throw new Error('Runner address must use HTTP or HTTPS.');
  }
  if (url.username || url.password || url.search || url.hash) {
    throw new Error('Runner address must not contain credentials, a query, or a fragment.');
  }
  if (url.pathname !== '/' && url.pathname !== '/web' && url.pathname !== '/chat') {
    throw new Error('Runner address must be an origin or end in /web or /chat.');
  }
  if (url.port === '' && !url.hostname.includes('.') && url.hostname !== 'localhost') {
    throw new Error('Runner address must include a listen port.');
  }

  const origin = url.origin;
  const webSocketProtocol = url.protocol === HTTPS_PROTOCOL ? 'wss:' : 'ws:';
  return {
    origin,
    authority: url.host,
    webSocketUrl: `${webSocketProtocol}//${url.host}/ws`,
  };
}

export function parseRunnerInviteLink(raw: string, pageProtocol?: PageHttpProtocol): {
  endpoint: RunnerEndpoint;
  token: string;
} {
  const value = raw.trim();
  const hashIndex = value.indexOf('#');
  const withoutHash = hashIndex < 0 ? value : value.slice(0, hashIndex);
  const endpoint = parseRunnerEndpoint(withoutHash, pageProtocol);
  if (hashIndex < 0) return { endpoint, token: '' };
  try {
    return { endpoint, token: decodeURIComponent(value.slice(hashIndex + 1)).trim() };
  } catch (error) {
    throw new Error(
      error instanceof Error
        ? `Runner invite link hash is not decodable (${error.message})`
        : 'Runner invite link hash is not decodable',
    );
  }
}

export function readRunnerStatus(value: unknown): RunnerStatus | null {
  if (!value || typeof value !== 'object') return null;
  const record = value as { tokenRequired?: unknown; version?: unknown; codename?: unknown };
  if (typeof record.tokenRequired !== 'boolean' || typeof record.version !== 'string') return null;
  if (record.codename !== null && record.codename !== undefined && typeof record.codename !== 'string') {
    return null;
  }
  return {
    tokenRequired: record.tokenRequired,
    version: record.version,
    codename: typeof record.codename === 'string' ? record.codename : null,
  };
}

export async function fetchRunnerStatus(
  endpoint: RunnerEndpoint,
  fetchFn: typeof fetch = fetch,
  signal?: AbortSignal,
): Promise<RunnerStatusResult> {
  try {
    const response = await fetchFn(`${endpoint.origin}/status`, { method: 'GET', signal });
    if (!response.ok) return { ok: false };
    const status = readRunnerStatus(await response.json());
    return status ? { ok: true, status } : { ok: false };
  } catch {
    return { ok: false };
  }
}

export function loadRunnerToken(storage: StorageLike | null, authority: string): string {
  return loadTokenForAuthority(storage, authority);
}

export function saveRunnerToken(
  storage: StorageLike | null,
  authority: string,
  token: string,
): void {
  saveTokenForAuthority(storage, authority, token);
}

/** /web pairing link for a runner: the page-origin URL carrying the same
 *  listen token in the fragment, exactly like the /chat invite links. */
export function runnerWebUrl(origin: string, token: string): string {
  const trimmedOrigin = origin.replace(/\/+$/, '');
  return token ? `${trimmedOrigin}/web#${token}` : `${trimmedOrigin}/web`;
}
