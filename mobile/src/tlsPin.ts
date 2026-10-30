import { NativeModules } from 'react-native';

import { splitRunnerAddress } from './protocol';

export type TlsInspect = {
  host: string;
  sha256: string;
  subject: string;
  issuer: string;
  notAfterMs: number;
};

const SHA256_HEX = /^[0-9a-f]{64}$/;

type TlsPinNative = {
  inspect: (origin: string) => Promise<unknown>;
  trust: (origin: string, sha256: string) => Promise<void>;
};

function nativeModule(): TlsPinNative | null {
  const row = NativeModules.TlsPin as TlsPinNative | undefined;
  if (!row || typeof row.inspect !== 'function' || typeof row.trust !== 'function') return null;
  return row;
}

/** Pin key for an origin. An IPv6 literal keeps its brackets, which is exactly
 *  what the platform yields: android.net.Uri.getHost() reverse-scans for the
 *  port separator, so `https://[::1]:8765` keys as `[::1]` on both sides
 *  (TlsPinModule.originHost). Stripping them here would break that match. */
export function tlsPinHost(origin: string): string {
  return splitRunnerAddress(origin, null)?.host.toLowerCase() ?? '';
}

export function parseTlsInspect(raw: unknown): TlsInspect | null {
  if (!raw || typeof raw !== 'object') return null;
  const row = raw as Record<string, unknown>;
  if (typeof row.host !== 'string' || row.host.trim() === '') return null;
  if (typeof row.sha256 !== 'string' || !SHA256_HEX.test(row.sha256)) return null;
  if (typeof row.subject !== 'string' || typeof row.issuer !== 'string') return null;
  if (typeof row.notAfterMs !== 'number' || !Number.isFinite(row.notAfterMs)) return null;
  return {
    host: row.host.trim().toLowerCase(),
    sha256: row.sha256,
    subject: row.subject,
    issuer: row.issuer,
    notAfterMs: row.notAfterMs,
  };
}

export function formatTlsFingerprint(sha256: string): string {
  const compact = sha256.toLowerCase();
  const parts: string[] = [];
  for (let i = 0; i < compact.length; i += 4) parts.push(compact.slice(i, i + 4));
  return parts.join(' ');
}

export async function inspectRunnerTls(origin: string): Promise<TlsInspect> {
  const native = nativeModule();
  if (!native) throw new Error('TLS pin module is unavailable.');
  const parsed = parseTlsInspect(await native.inspect(origin));
  if (!parsed) throw new Error('TLS inspect returned an unreadable certificate.');
  return parsed;
}

export async function trustRunnerTls(origin: string, sha256: string): Promise<void> {
  const native = nativeModule();
  if (!native) throw new Error('TLS pin module is unavailable.');
  if (!SHA256_HEX.test(sha256)) throw new Error('Certificate fingerprint is not a SHA-256 hex digest.');
  await native.trust(origin, sha256);
}
