import { parseTransportFailure } from './transportError';

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

function authHeaders(token: string): Record<string, string> {
  return { 'Content-Type': 'application/json', Authorization: `Bearer ${token}` };
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
  const body = await response.json() as { success?: unknown; error?: unknown; data?: unknown };
  if (body.success !== true) throw new Error(typeof body.error === 'string' ? body.error : 'chat_mapping_get failed');
  return getPayload(body.data, isMappingDocument);
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
  const body = await response.json() as { success?: unknown; error?: unknown; data?: unknown };
  if (body.success !== true) throw new Error(typeof body.error === 'string' ? body.error : 'chat_mapping_put failed');
  return putPayload(body.data, isMappingDocument);
}

const PDF_UPLOAD_PATH = '/upload/pdf';
const PDF_UPLOAD_TIMEOUT_MS = 45_000;
const MAX_PDF_ERROR_CHARS = 300;

export type PdfUploadOutcome =
  | { ok: true; text: string; instruction: string }
  | { ok: false; message: string };

function pdfUploadError(status: number, body: { error?: unknown } | null): string {
  if (typeof body?.error === 'string' && body.error.trim() !== '') {
    return body.error.slice(0, MAX_PDF_ERROR_CHARS);
  }
  switch (status) {
    case 400: return 'the PDF has no extractable text (scanned or encrypted)';
    case 401: case 403: return 'authentication failed — check the runner token';
    case 411: return 'the upload was rejected: missing content length';
    case 413: return 'the PDF is over the server upload limit';
    case 415: return 'unsupported PDF file — attach a .pdf document';
    case 429: return 'the runner is busy with other PDF uploads — try again shortly';
    case 503: return 'PDF extraction is unavailable: pdftotext (poppler-utils) is not installed on the runner';
    case 504: return 'PDF processing timed out on the server';
    default: return `PDF upload failed (HTTP ${status})`;
  }
}

/** POST /upload/pdf for one PDF's text layer; the name header must be percent-encoded (the backend percent-decodes it) and uses the same bearer auth as /rpc. */
export async function uploadPdfAttachment(
  origin: string,
  token: string,
  name: string,
  bytes: Uint8Array,
  timeoutMs = PDF_UPLOAD_TIMEOUT_MS,
): Promise<PdfUploadOutcome> {
  const headers: Record<string, string> = {
    'Content-Type': 'application/pdf',
    'X-Pdf-Name': encodeURIComponent(name),
  };
  if (token !== '') headers.Authorization = `Bearer ${token}`;
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    const response = await fetch(`${origin}${PDF_UPLOAD_PATH}`, {
      method: 'POST',
      headers,
      body: bytes,
      signal: controller.signal,
    });
    const body = await response.json().catch(() => null) as { text?: unknown; instruction?: unknown; error?: unknown } | null;
    if (response.ok) {
      if (typeof body?.text === 'string' && body.text.trim() !== '') {
        return { ok: true, text: body.text, instruction: typeof body.instruction === 'string' ? body.instruction : '' };
      }
      return { ok: false, message: 'the PDF upload endpoint returned an unexpected response' };
    }
    return { ok: false, message: pdfUploadError(response.status, body) };
  } catch {
    if (controller.signal.aborted) return { ok: false, message: 'the upload timed out' };
    return { ok: false, message: 'the upload failed — check the connection and try again' };
  } finally {
    clearTimeout(timer);
  }
}

export function isSyncNetworkFailure(error: unknown): boolean {
  if (error instanceof TypeError) return true;
  return error instanceof Error
    && (/^chat_mapping_(get|put) failed: (404|501)$/.test(error.message)
      || error.message === 'fetch is not defined');
}

export function parseRpcFetchError(error: unknown): string {
  const message = error instanceof Error ? error.message : String(error);
  if (parseTransportFailure(message) === 'cert') return message;
  return message;
}

export function parseImportedRunner(raw: unknown): {
  id: string;
  label: string;
  origin: string;
  authority: string;
  createdAt: number;
} | null {
  if (!raw || typeof raw !== 'object') return null;
  const row = raw as Record<string, unknown>;
  if (typeof row.id !== 'string' || !row.id) return null;
  if (typeof row.authority !== 'string' || !row.authority) return null;
  if (typeof row.origin !== 'string' || !row.origin || row.origin.includes('#')) return null;
  const stamp = typeof row.createdAt === 'number'
    ? row.createdAt
    : (typeof row.updatedAt === 'number' ? row.updatedAt : Date.now());
  return {
    id: row.id,
    label: typeof row.label === 'string' ? row.label : '',
    origin: row.origin,
    authority: row.authority,
    createdAt: stamp,
  };
}
