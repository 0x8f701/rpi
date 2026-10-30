const SESSION_EXPORT_PATH = '/download/session/';
export const MAX_SESSION_EXPORT_BYTES = 16 * 1024 * 1024;
export const SESSION_EXPORT_TIMEOUT_MS = 30_000;
const MAX_EXPORT_FILENAME_LENGTH = 128;

export interface SessionExportAnchor {
  href: string;
  download: string;
  hidden: boolean;
  click(): void;
  remove(): void;
}

export interface SessionExportDocument {
  createElement(tagName: 'a'): SessionExportAnchor;
  body: { appendChild(node: SessionExportAnchor): void };
}

export interface SessionExportResponseLike {
  ok: boolean;
  headers: { get(name: string): string | null };
  blob(): Promise<{ size: number }>;
}

export type SessionExportFetch = (
  input: string,
  init?: { signal?: AbortSignal },
) => Promise<SessionExportResponseLike>;

export interface SessionExportDownloadOptions {
  hostAuthority: string;
  pageProtocol?: string;
  fetchImpl?: SessionExportFetch;
  timeoutMs?: number;
  setTimeout?: (handler: () => void, timeout: number) => number;
  clearTimeout?: (id: number) => void;
  createObjectURL?: (blob: { size: number }) => string;
  revokeObjectURL?: (url: string) => void;
  document?: SessionExportDocument;
}

export function sessionExportUrl(
  pageProtocol: string,
  hostAuthority: string,
  downloadId: string,
): string {
  if (!/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(downloadId)) {
    throw new Error('session export download id is invalid');
  }
  const scheme = pageProtocol === 'https:' ? 'https://' : 'http://';
  return `${scheme}${hostAuthority}${SESSION_EXPORT_PATH}${downloadId}`;
}

export function sessionExportFileName(fileName: string): string {
  const bounded = fileName
    .replace(/[\u0000-\u001f\u007f\\/]+/g, '-')
    .trim()
    .slice(0, MAX_EXPORT_FILENAME_LENGTH);
  if (!bounded.toLowerCase().endsWith('.html')) {
    throw new Error('session export filename is invalid');
  }
  return bounded || 'session.html';
}

function responseLength(response: SessionExportResponseLike): number | null {
  const raw = response.headers.get('content-length');
  if (raw === null) return null;
  const size = Number(raw);
  return Number.isSafeInteger(size) && size >= 0 ? size : null;
}

export async function downloadSessionExport(
  downloadId: string,
  fileName: string,
  options: SessionExportDownloadOptions,
): Promise<void> {
  const safeName = sessionExportFileName(fileName);
  const controller = new AbortController();
  const setTimeoutImpl = options.setTimeout ?? ((handler, timeout) => window.setTimeout(handler, timeout));
  const clearTimeoutImpl = options.clearTimeout ?? ((id) => window.clearTimeout(id));
  const timeout = setTimeoutImpl(
    () => controller.abort(),
    options.timeoutMs ?? SESSION_EXPORT_TIMEOUT_MS,
  );
  const fetchImpl: SessionExportFetch = options.fetchImpl
    ?? ((input, init) => fetch(input, init));
  const createObjectURL = options.createObjectURL ?? ((blob) => URL.createObjectURL(blob as Blob));
  const revokeObjectURL = options.revokeObjectURL ?? ((url) => URL.revokeObjectURL(url));
  const doc = (options.document ?? document) as SessionExportDocument;
  try {
    const response = await fetchImpl(
      sessionExportUrl(options.pageProtocol ?? 'http:', options.hostAuthority, downloadId),
      { signal: controller.signal },
    );
    if (!response.ok) {
      throw new Error('session export download failed');
    }
    if (!(response.headers.get('content-type') || '').toLowerCase().startsWith('text/html')) {
      throw new Error('session export response is invalid');
    }
    const length = responseLength(response);
    if (length === null || length > MAX_SESSION_EXPORT_BYTES) {
      throw new Error('session export exceeds the download limit');
    }
    const blob = await response.blob();
    if (blob.size !== length || blob.size > MAX_SESSION_EXPORT_BYTES) {
      throw new Error('session export exceeds the download limit');
    }
    const objectUrl = createObjectURL(blob);
    try {
      const anchor = doc.createElement('a') as unknown as HTMLElement & SessionExportAnchor;
      anchor.href = objectUrl;
      anchor.download = safeName;
      anchor.hidden = true;
      doc.body.appendChild(anchor);
      anchor.click();
      anchor.remove();
    } finally {
      revokeObjectURL(objectUrl);
    }
  } catch (error) {
    if (controller.signal.aborted) {
      throw new Error('session export download timed out');
    }
    if (error instanceof Error && error.message.startsWith('session export ')) throw error;
    throw new Error('session export download failed');
  } finally {
    clearTimeoutImpl(timeout);
  }
}
