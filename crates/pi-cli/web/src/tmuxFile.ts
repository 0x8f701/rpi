import type { ComposerAttachment } from './attachments';

export const TMUX_FILE_UPLOAD_PATH = '/upload/tmux-file';
export const TMUX_FILE_DESTINATION_KEY = 'rpi-tmux-file-destination';

export interface TmuxFileDestination {
  runnerId: string;
  session: string;
  authority: string;
}

export function parseTmuxFileDestination(raw: unknown): TmuxFileDestination | null {
  if (raw === null || typeof raw !== 'object') return null;
  const record = raw as { runnerId?: unknown; session?: unknown; authority?: unknown };
  if (typeof record.runnerId !== 'string' || record.runnerId === '') return null;
  if (typeof record.session !== 'string' || record.session === '') return null;
  if (typeof record.authority !== 'string' || record.authority === '') return null;
  return { runnerId: record.runnerId, session: record.session, authority: record.authority };
}

export function rememberTmuxFileDestination(destination: TmuxFileDestination | null): void {
  const storage = readSessionStorage();
  if (!storage) return;
  if (!destination) {
    storage.removeItem(TMUX_FILE_DESTINATION_KEY);
    return;
  }
  const parsed = parseTmuxFileDestination(destination);
  if (!parsed) {
    storage.removeItem(TMUX_FILE_DESTINATION_KEY);
    return;
  }
  storage.setItem(TMUX_FILE_DESTINATION_KEY, JSON.stringify(parsed));
}

export function tmuxFileDestination(): TmuxFileDestination | null {
  const raw = readSessionStorage()?.getItem(TMUX_FILE_DESTINATION_KEY);
  if (!raw) return null;
  try {
    return parseTmuxFileDestination(JSON.parse(raw));
  } catch {
    return null;
  }
}

export function tmuxFileUploadUrl(pageProtocol: string, hostAuthority: string): string {
  const scheme = pageProtocol === 'https:' ? 'https://' : 'http://';
  return `${scheme}${hostAuthority}${TMUX_FILE_UPLOAD_PATH}`;
}

export function tmuxFileUploadHeaders(
  name: string,
  session: string,
  token: string,
): Record<string, string> {
  const headers: Record<string, string> = {
    'X-File-Name': encodeURIComponent(name),
    'X-Tmux-Session': encodeURIComponent(session),
  };
  if (token !== '') headers.Authorization = `Bearer ${token}`;
  return headers;
}

export function tmuxFileUploadName(attachment: ComposerAttachment): string {
  const extension = imageUploadExtension(attachment.mimeType);
  if (attachment.kind !== 'image' || extension === null) return attachment.name;
  const dot = attachment.name.lastIndexOf('.');
  const stem = dot > 0 ? attachment.name.slice(0, dot) : attachment.name;
  const safeStem = stem === '' ? 'file' : stem;
  return `${safeStem}.${extension}`;
}

export function attachmentTmuxFileBytes(attachment: ComposerAttachment): Uint8Array<ArrayBuffer> | null {
  if (attachment.kind === 'image' && attachment.dataBase64) {
    return decodeBase64(attachment.dataBase64);
  }
  if (attachment.kind === 'code' && attachment.text) {
    return new TextEncoder().encode(attachment.text);
  }
  return null;
}

export function canSendAttachmentToTmux(attachment: ComposerAttachment): boolean {
  return attachmentTmuxFileBytes(attachment) !== null;
}

export function planTmuxFileUpload(
  attachment: ComposerAttachment,
  destination: TmuxFileDestination | null,
  loadToken: (authority: string) => string,
):
  | {
      ok: true;
      name: string;
      bytes: Uint8Array<ArrayBuffer>;
      hostAuthority: string;
      token: string;
      session: string;
    }
  | { ok: false; message: string } {
  if (!destination) {
    return { ok: false, message: 'Open a tmux overlay first' };
  }
  const bytes = attachmentTmuxFileBytes(attachment);
  if (!bytes) {
    return { ok: false, message: 'this attachment cannot be sent to a terminal' };
  }
  return {
    ok: true,
    name: tmuxFileUploadName(attachment),
    bytes,
    hostAuthority: destination.authority,
    token: loadToken(destination.authority),
    session: destination.session,
  };
}

export function parseTmuxFileUploadResponse(
  status: number,
  body: unknown,
): { ok: true; path: string } | { ok: false; message: string } {
  if (status >= 200 && status < 300) {
    const path = (body as { path?: unknown } | null)?.path;
    if (typeof path === 'string' && path !== '') return { ok: true, path };
    return { ok: false, message: 'unexpected response from the tmux-file upload endpoint' };
  }
  const error = (body as { error?: unknown } | null)?.error;
  if (typeof error === 'string' && error !== '') return { ok: false, message: error };
  return { ok: false, message: `tmux-file upload failed (${status})` };
}

export async function sendComposerAttachmentToTmux(
  attachment: ComposerAttachment,
  opts: {
    loadToken: (authority: string) => string;
    pageProtocol?: string;
    fetchImpl?: typeof fetch;
  },
): Promise<{ ok: true; path: string } | { ok: false; message: string }> {
  const planned = planTmuxFileUpload(attachment, tmuxFileDestination(), opts.loadToken);
  if (!planned.ok) return planned;
  return uploadTmuxFile(planned.name, planned.bytes, {
    hostAuthority: planned.hostAuthority,
    token: planned.token,
    session: planned.session,
    pageProtocol: opts.pageProtocol,
    fetchImpl: opts.fetchImpl,
  });
}

export async function uploadTmuxOverlayFile(
  file: File,
  destination: TmuxFileDestination | null,
  loadToken: (authority: string) => string,
  opts?: {
    pageProtocol?: string;
    fetchImpl?: typeof fetch;
  },
): Promise<{ ok: true; path: string } | { ok: false; message: string }> {
  if (!destination) {
    return { ok: false, message: 'Open a tmux overlay first' };
  }
  const name = file.name.trim() === '' ? 'file' : file.name;
  let bytes: Uint8Array<ArrayBuffer>;
  try {
    bytes = new Uint8Array(await file.arrayBuffer());
  } catch {
    return { ok: false, message: 'could not read the selected file' };
  }
  let token: string;
  try {
    token = loadToken(destination.authority);
  } catch {
    return { ok: false, message: 'could not load the runner token' };
  }
  return uploadTmuxFile(name, bytes, {
    hostAuthority: destination.authority,
    token,
    session: destination.session,
    pageProtocol: opts?.pageProtocol,
    fetchImpl: opts?.fetchImpl,
  });
}

export async function uploadTmuxFile(
  name: string,
  bytes: Uint8Array<ArrayBuffer>,
  opts: {
    hostAuthority: string;
    token: string;
    session: string;
    pageProtocol?: string;
    fetchImpl?: typeof fetch;
  },
): Promise<{ ok: true; path: string } | { ok: false; message: string }> {
  if (opts.session === '' || opts.hostAuthority === '') {
    return { ok: false, message: 'Open a tmux overlay first' };
  }
  const fetchImpl = opts.fetchImpl ?? ((url: string, init?: RequestInit) => fetch(url, init));
  const url = tmuxFileUploadUrl(opts.pageProtocol ?? 'http:', opts.hostAuthority);
  const headers = tmuxFileUploadHeaders(name, opts.session, opts.token);
  try {
    const response = await fetchImpl(url, {
      method: 'POST',
      headers,
      body: bytes,
    });
    let parsed: unknown = null;
    try {
      parsed = await response.json();
    } catch {
      parsed = null;
    }
    return parseTmuxFileUploadResponse(response.status, parsed);
  } catch {
    return { ok: false, message: 'tmux-file upload failed — check the connection and try again' };
  }
}

function imageUploadExtension(mimeType: string): string | null {
  switch (mimeType) {
    case 'image/jpeg':
      return 'jpg';
    case 'image/png':
      return 'png';
    case 'image/gif':
      return 'gif';
    case 'image/webp':
      return 'webp';
    default:
      return null;
  }
}

function readSessionStorage(): Storage | null {
  try {
    if (typeof sessionStorage === 'undefined') return null;
    return sessionStorage;
  } catch {
    return null;
  }
}

function decodeBase64(data: string): Uint8Array<ArrayBuffer> | null {
  try {
    const binary = atob(data);
    const bytes = new Uint8Array(binary.length);
    for (let i = 0; i < binary.length; i += 1) bytes[i] = binary.charCodeAt(i);
    return bytes;
  } catch {
    return null;
  }
}
