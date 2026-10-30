#!/usr/bin/env node
import {
  TMUX_FILE_UPLOAD_PATH,
  attachmentTmuxFileBytes,
  canSendAttachmentToTmux,
  parseTmuxFileUploadResponse,
  planTmuxFileUpload,
  rememberTmuxFileDestination,
  tmuxFileDestination,
  tmuxFileUploadHeaders,
  tmuxFileUploadName,
  tmuxFileUploadUrl,
  sendComposerAttachmentToTmux,
  uploadTmuxFile,
  uploadTmuxOverlayFile,
} from '../src/tmuxFile.ts';
import type { ComposerAttachment } from '../src/attachments.ts';

const failures: string[] = [];
let ran = 0;
function check(name: string, condition: boolean, detail = ''): void {
  ran += 1;
  if (!condition) failures.push(detail ? `${name}: ${detail}` : name);
}

const store = new Map<string, string>();
Object.defineProperty(globalThis, 'sessionStorage', {
  configurable: true,
  value: {
    getItem(key: string) {
      return store.has(key) ? store.get(key)! : null;
    },
    setItem(key: string, value: string) {
      store.set(key, String(value));
    },
    removeItem(key: string) {
      store.delete(key);
    },
    clear() {
      store.clear();
    },
  },
});

check('upload path is /upload/tmux-file', TMUX_FILE_UPLOAD_PATH === '/upload/tmux-file');
check(
  'upload URL is origin-scoped',
  tmuxFileUploadUrl('https:', '192.0.2.10:8765') === 'https://192.0.2.10:8765/upload/tmux-file',
);
const headers = tmuxFileUploadHeaders('shot.png', 'agentlo-pi-rs', 'tok');
check(
  'headers carry file name and session',
  headers['X-File-Name'] === encodeURIComponent('shot.png')
    && headers['X-Tmux-Session'] === encodeURIComponent('agentlo-pi-rs')
    && headers.Authorization === 'Bearer tok',
);

const image: ComposerAttachment = {
  id: '1',
  name: 'shot.png',
  size: 8,
  mimeType: 'image/png',
  kind: 'image',
  dataBase64: btoa('\x89PNG'),
};
const jpegNamedPng: ComposerAttachment = {
  id: '1j',
  name: 'shot.png',
  size: 4,
  mimeType: 'image/jpeg',
  kind: 'image',
  dataBase64: btoa(String.fromCharCode(0xff, 0xd8, 0xff, 0xe0)),
};
const code: ComposerAttachment = {
  id: '2',
  name: 'main.rs',
  size: 4,
  mimeType: 'text/plain',
  kind: 'code',
  text: 'fn x',
};
const video: ComposerAttachment = {
  id: '3',
  name: 'clip.mp4',
  size: 4,
  mimeType: 'video/mp4',
  kind: 'video',
};
const pdf: ComposerAttachment = {
  id: '4',
  name: 'doc.pdf',
  size: 8,
  mimeType: 'application/pdf',
  kind: 'pdf',
  text: 'extracted pdf text',
};
check('image chips can send to tmux', canSendAttachmentToTmux(image) === true);
check('code chips can send to tmux', canSendAttachmentToTmux(code) === true);
check('video chips cannot send to tmux', canSendAttachmentToTmux(video) === false);
check('pdf chips cannot send extracted text as a pdf file', canSendAttachmentToTmux(pdf) === false);
check(
  'image bytes decode from dataBase64',
  attachmentTmuxFileBytes(image)?.length === 4,
);
check('code bytes encode UTF-8 text', new TextDecoder().decode(attachmentTmuxFileBytes(code) ?? new Uint8Array()) === 'fn x');
check(
  'JPEG bytes with a .png name upload as .jpg',
  tmuxFileUploadName(jpegNamedPng) === 'shot.jpg',
);

const ok = parseTmuxFileUploadResponse(200, { path: '/tmp/rpi-tmux-file-x-shot.jpg' });
check('success parser reads path', ok.ok === true && ok.ok && ok.path.endsWith('shot.jpg'));
const fail = parseTmuxFileUploadResponse(415, { error: 'unsupported tmux file' });
check('error parser reads error', fail.ok === false && !fail.ok && fail.message.includes('unsupported'));

rememberTmuxFileDestination({
  runnerId: 'runner-other',
  session: 'agentlo-pi-rs',
  authority: '192.0.2.10:8765',
});
const remembered = tmuxFileDestination();
check(
  'destination carries authority with session',
  remembered !== null
    && remembered.runnerId === 'runner-other'
    && remembered.session === 'agentlo-pi-rs'
    && remembered.authority === '192.0.2.10:8765',
);

const tokenByAuthority: Record<string, string> = {
  '192.0.2.10:8765': 'tok-overlay',
  '192.0.2.99:8765': 'tok-page',
};
const planned = planTmuxFileUpload(jpegNamedPng, remembered, (authority) => tokenByAuthority[authority] ?? '');
check(
  'plan posts to overlay authority not the page host',
  planned.ok === true
    && planned.ok
    && planned.hostAuthority === '192.0.2.10:8765'
    && planned.session === 'agentlo-pi-rs'
    && planned.name === 'shot.jpg'
    && planned.token === 'tok-overlay',
);

const calls: Array<{ url: string; init: RequestInit }> = [];
if (planned.ok) {
  const sent = await uploadTmuxFile(planned.name, planned.bytes, {
    hostAuthority: planned.hostAuthority,
    token: planned.token,
    session: planned.session,
    pageProtocol: 'https:',
    fetchImpl: (async (url: string, init?: RequestInit) => {
      calls.push({ url, init: init ?? {} });
      return {
        status: 200,
        json: async () => ({ path: '/tmp/rpi-tmux-file-1-shot.jpg' }),
      } as Response;
    }) as typeof fetch,
  });
  check(
    'upload POST hits overlay authority',
    calls[0]?.url === 'https://192.0.2.10:8765/upload/tmux-file',
  );
  check('upload action returns path', sent.ok === true && sent.ok && sent.path.includes('shot.jpg'));
}

const missing = planTmuxFileUpload(image, null, () => 'tok-page');
check('empty destination does not plan an upload', missing.ok === false && missing.message.includes('Open a tmux overlay'));

const noSession = await uploadTmuxFile('shot.png', new Uint8Array([1]), {
  hostAuthority: '192.0.2.99:8765',
  token: '',
  session: '',
  fetchImpl: (async () => {
    throw new Error('must not fetch');
  }) as typeof fetch,
});
check('empty session does not POST', noSession.ok === false && noSession.message.includes('Open a tmux overlay'));

rememberTmuxFileDestination({
  runnerId: 'runner-other',
  session: 'agentlo-pi-rs',
  authority: '192.0.2.10:8765',
});
const composerCalls: Array<{ url: string; init: RequestInit }> = [];
const composerSent = await sendComposerAttachmentToTmux(jpegNamedPng, {
  loadToken: (authority) => tokenByAuthority[authority] ?? '',
  pageProtocol: 'https:',
  fetchImpl: (async (url: string, init?: RequestInit) => {
    composerCalls.push({ url, init: init ?? {} });
    return {
      status: 200,
      json: async () => ({ path: '/tmp/rpi-tmux-file-2-shot.jpg' }),
    } as Response;
  }) as typeof fetch,
});
check(
  'composer send helper posts to overlay destination',
  composerCalls[0]?.url === 'https://192.0.2.10:8765/upload/tmux-file'
    && composerSent.ok === true
    && composerSent.ok
    && composerSent.path.endsWith('shot.jpg'),
);

const overlayCalls: Array<{ url: string; init: RequestInit }> = [];
const overlayFile = new File([new Uint8Array([104, 105])], 'note.txt', { type: 'text/plain' });
const overlaySent = await uploadTmuxOverlayFile(
  overlayFile,
  {
    runnerId: 'runner-x3',
    session: 'agentlo-pi-rs',
    authority: '192.0.2.10:8765',
  },
  (authority) => tokenByAuthority[authority] ?? '',
  {
    pageProtocol: 'https:',
    fetchImpl: (async (url: string, init?: RequestInit) => {
      overlayCalls.push({ url, init: init ?? {} });
      return {
        status: 200,
        json: async () => ({ path: '/tmp/rpi-tmux-file-overlay-note.txt' }),
      } as Response;
    }) as typeof fetch,
  },
);
check(
  'overlay file helper posts to overlay authority and session',
  overlayCalls[0]?.url === 'https://192.0.2.10:8765/upload/tmux-file'
    && overlaySent.ok === true
    && overlaySent.ok
    && overlaySent.path.endsWith('note.txt'),
);
const overlayHeaders = (overlayCalls[0]?.init.headers ?? {}) as Record<string, string>;
check(
  'overlay file helper names the overlay session',
  overlayHeaders['X-Tmux-Session'] === encodeURIComponent('agentlo-pi-rs')
    && overlayHeaders['X-File-Name'] === encodeURIComponent('note.txt'),
);
const overlayMissing = await uploadTmuxOverlayFile(
  overlayFile,
  null,
  () => 'tok-page',
  {
    fetchImpl: (async () => {
      throw new Error('must not fetch');
    }) as typeof fetch,
  },
);
check(
  'overlay file helper refuses a missing destination',
  overlayMissing.ok === false && overlayMissing.message.includes('Open a tmux overlay'),
);

const unreadFile = new File([new Uint8Array([104, 105])], 'note.txt', { type: 'text/plain' });
Object.defineProperty(unreadFile, 'arrayBuffer', {
  value: async () => {
    throw new Error('read failed');
  },
});
const overlayUnread = await uploadTmuxOverlayFile(
  unreadFile,
  {
    runnerId: 'runner-x3',
    session: 'agentlo-pi-rs',
    authority: '192.0.2.10:8765',
  },
  () => 'tok-x3',
  {
    fetchImpl: (async () => {
      throw new Error('must not fetch');
    }) as typeof fetch,
  },
);
check(
  'overlay file helper toasts a file-read failure',
  overlayUnread.ok === false && overlayUnread.message === 'could not read the selected file',
);

const overlayTokenThrow = await uploadTmuxOverlayFile(
  overlayFile,
  {
    runnerId: 'runner-x3',
    session: 'agentlo-pi-rs',
    authority: '192.0.2.10:8765',
  },
  () => {
    throw new Error('token');
  },
  {
    fetchImpl: (async () => {
      throw new Error('must not fetch');
    }) as typeof fetch,
  },
);
check(
  'overlay file helper toasts a token-load failure',
  overlayTokenThrow.ok === false && overlayTokenThrow.message === 'could not load the runner token',
);

if (failures.length > 0) {
  console.error(`tmuxFile: ${ran} checks, ${failures.length} FAILURES`);
  for (const failure of failures) console.error(`  - ${failure}`);
  process.exit(1);
}
console.log(`tmuxFile: ${ran} checks, 0 failures`);
process.exit(0);
