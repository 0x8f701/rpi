// Server-side sync for the /chat LAN mapping document.
//
// The browser-local mapping document ties projects, runners, and runner
// pairing tokens to one browser profile. The listener mirrors the same
// document under agent_dir/chat-mapping/sync.json (chat_sync.rs), so a phone
// scanning the pairing QR adopts the desktop's whole setup — including runner
// tokens, which live at the same trust level as the listener's own token file.
import { loadTokenForAuthority, saveTokenForAuthority, type StorageLike } from '../hostToken';
import { safeText } from '../redact';
import type { RpcResponse } from '../types';

export interface MappingSyncPayload {
  mapping: unknown;
  runners: unknown[];
  tokens: Record<string, string>;
}

export interface MappingSyncGetResult {
  revision: number;
  savedAtMs: number;
  document: MappingSyncPayload | null;
}

export type MappingSyncPutResult =
  | { ok: true; revision: number; savedAtMs: number }
  | { conflict: true; revision: number; savedAtMs: number; document: MappingSyncPayload };

/** Envelope guard for a sync document (the `document` field of
 *  chat_mapping_get and of a chat_mapping_put conflict). `isMappingDocument`
 *  validates the mapping body — its shape is owned by mapping.ts — so a
 *  malformed payload is REJECTED (the caller throws, or treats the document
 *  as absent) instead of a fabricated empty payload being adopted. */
export function isSyncPayload(
  value: unknown,
  isMappingDocument: (mapping: unknown) => boolean,
): value is MappingSyncPayload {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) return false;
  const raw = value as Record<string, unknown>;
  if (!Array.isArray(raw.runners)) return false;
  if (raw.tokens === null || typeof raw.tokens !== 'object' || Array.isArray(raw.tokens)) return false;
  for (const token of Object.values(raw.tokens)) {
    if (typeof token !== 'string') return false;
  }
  return isMappingDocument(raw.mapping);
}

const TOKEN_PREFIX = 'rpi-web-token:';

export function collectRunnerTokens(storage: StorageLike | null): Record<string, string> {
  const dom = storage as Storage | null;
  if (!dom || typeof dom.length !== 'number' || typeof dom.key !== 'function') return {};
  const tokens: Record<string, string> = {};
  try {
    for (let index = 0; index < dom.length; index += 1) {
      const key = dom.key(index);
      if (!key || !key.startsWith(TOKEN_PREFIX)) continue;
      const authority = decodeURIComponent(key.slice(TOKEN_PREFIX.length));
      const value = dom.getItem(key);
      if (authority && value) tokens[authority] = value;
    }
  } catch {
    return tokens;
  }
  return tokens;
}

export function applyRunnerTokens(storage: StorageLike | null, tokens: Record<string, string>): void {
  for (const [authority, token] of Object.entries(tokens)) {
    if (!authority || typeof token !== 'string' || token === '') continue;
    if (loadTokenForAuthority(storage, authority) === token) continue;
    saveTokenForAuthority(storage, authority, token);
  }
}

function authHeaders(token: string): HeadersInit {
  return { 'Content-Type': 'application/json', Authorization: `Bearer ${token}` };
}

export async function fetchMappingSync(
  origin: string,
  token: string,
  isMappingDocument: (mapping: unknown) => boolean,
): Promise<MappingSyncGetResult> {
  const response = await fetch(`${origin}/rpc`, {
    method: 'POST',
    headers: authHeaders(token),
    body: JSON.stringify({ type: 'chat_mapping_get' }),
  });
  if (!response.ok) throw new Error(`chat_mapping_get failed: ${response.status}`);
  const body = (await response.json()) as RpcResponse;
  if (body.success !== true) throw new Error(body.error || 'chat_mapping_get failed');
  return getPayload(body.data, isMappingDocument);
}

function getPayload(data: unknown, isMappingDocument: (mapping: unknown) => boolean): MappingSyncGetResult {
  if (data === null || typeof data !== 'object' || Array.isArray(data)) {
    throw new Error('chat_mapping_get returned an invalid payload.');
  }
  const raw = data as Record<string, unknown>;
  if (typeof raw.revision !== 'number' || typeof raw.savedAtMs !== 'number') {
    throw new Error('chat_mapping_get returned an invalid payload.');
  }
  if (raw.document === null || raw.document === undefined) {
    return { revision: raw.revision, savedAtMs: raw.savedAtMs, document: null };
  }
  if (!isSyncPayload(raw.document, isMappingDocument)) {
    throw new Error('chat_mapping_get returned an invalid payload.');
  }
  return { revision: raw.revision, savedAtMs: raw.savedAtMs, document: raw.document };
}

export async function pushMappingSync(
  origin: string,
  token: string,
  payload: MappingSyncPayload,
  baseRevision: number,
  isMappingDocument: (mapping: unknown) => boolean,
): Promise<MappingSyncPutResult> {
  const response = await fetch(`${origin}/rpc`, {
    method: 'POST',
    headers: authHeaders(token),
    body: JSON.stringify({ type: 'chat_mapping_put', document: payload, baseRevision }),
  });
  if (!response.ok) throw new Error(`chat_mapping_put failed: ${response.status}`);
  const body = (await response.json()) as RpcResponse;
  if (body.success !== true) throw new Error(body.error || 'chat_mapping_put failed');
  return putPayload(body.data, isMappingDocument);
}

function putPayload(data: unknown, isMappingDocument: (mapping: unknown) => boolean): MappingSyncPutResult {
  if (data === null || typeof data !== 'object' || Array.isArray(data)) {
    throw new Error('chat_mapping_put returned an invalid payload.');
  }
  const raw = data as Record<string, unknown>;
  if (typeof raw.revision !== 'number' || typeof raw.savedAtMs !== 'number') {
    throw new Error('chat_mapping_put returned an invalid payload.');
  }
  if (raw.ok === true) {
    return { ok: true, revision: raw.revision, savedAtMs: raw.savedAtMs };
  }
  if (raw.conflict === true) {
    if (!isSyncPayload(raw.document, isMappingDocument)) {
      throw new Error('chat_mapping_put returned an invalid payload.');
    }
    return { conflict: true, revision: raw.revision, savedAtMs: raw.savedAtMs, document: raw.document };
  }
  throw new Error('chat_mapping_put returned an invalid payload.');
}

/** Exact non-fatal sync failure categories the boot/push flow may swallow
 *  silently: fetch TypeError (page offline / listener unreachable) and a
 *  listener too old for the chat_mapping endpoints (HTTP 404) or for this
 *  RPC envelope (HTTP 501). 401, 400, 5xx, and malformed JSON shapes are
 *  real failures and MUST surface via the caller's formError slot. */
export function isSyncNetworkFailure(error: unknown): boolean {
  if (error instanceof TypeError) return true;
  return error instanceof Error
    && /^chat_mapping_(get|put) failed: (404|501)$/.test(error.message);
}


export function syncErrorMessage(error: unknown): string {
  return safeText(error instanceof Error ? error.message : String(error));
}