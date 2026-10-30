import {
  attachmentPickerHtml,
  base64ToBytes,
  MAX_CODE_BYTES,
  MAX_IMAGE_INPUT_BYTES,
  MAX_IMAGE_PIXELS,
  MAX_PDF_BYTES,
  MAX_VIDEO_BYTES,
} from '../src/attachments';

/**
 * The attach button IS the generated picker WebView: these tests run the real
 * inline script against a stub DOM, so what is asserted is the control's
 * behaviour (what it posts, what it refuses, how it slices the bridge
 * payloads), never the source text of the template.
 */

interface FakeFile {
  name: string;
  type: string;
  size: number;
  bytes: Uint8Array;
}

interface Posted {
  type: string;
  [key: string]: unknown;
}

interface Harness {
  accept: string;
  posted: Posted[];
  canvases: Array<{ width: number; height: number }>;
  choose: (files: FakeFile[]) => void;
  settle: () => Promise<void>;
}

function fakeFile(name: string, type: string, bytes: Uint8Array, size = bytes.length): FakeFile {
  return { name, type, size, bytes };
}

function encoded(bytes: number): string {
  return Buffer.alloc(bytes).toString('base64');
}

function runPicker(options: {
  decoded?: { width: number; height: number };
  encodedBytes?: number;
} = {}): Harness {
  const html = attachmentPickerHtml({ background: '#000000', foreground: '#ffffff', border: '#333333' });
  const script = html.slice(html.indexOf('<script>') + '<script>'.length, html.indexOf('</script>'));
  const accept = /accept="([^"]*)"/.exec(html)?.[1] ?? '';
  const posted: Posted[] = [];
  const canvases: Array<{ width: number; height: number }> = [];
  let onChange: ((event: unknown) => void) | null = null;
  const input = { value: 'picked', addEventListener: (type: string, handler: (event: unknown) => void) => {
    if (type === 'change') onChange = handler;
  } };

  const document = {
    getElementById: () => input,
    createElement: () => {
      const canvas = {
        width: 0,
        height: 0,
        getContext: () => ({ fillStyle: '', fillRect: () => {}, drawImage: () => {} }),
        toDataURL: () => `data:image/jpeg;base64,${encoded(options.encodedBytes ?? 1024)}`,
      };
      canvases.push(canvas);
      return canvas;
    },
  };

  class FakeFileReader {
    onload: (() => void) | null = null;
    onerror: (() => void) | null = null;
    result: ArrayBuffer | null = null;
    readAsArrayBuffer(file: FakeFile): void {
      this.result = file.bytes.slice().buffer;
      this.onload?.();
    }
  }

  class FakeImage {
    onload: (() => void) | null = null;
    onerror: (() => void) | null = null;
    naturalWidth = 0;
    naturalHeight = 0;
    set src(_value: string) {
      const decoded = options.decoded;
      if (!decoded) {
        this.onerror?.();
        return;
      }
      this.naturalWidth = decoded.width;
      this.naturalHeight = decoded.height;
      this.onload?.();
    }
  }

  const window = { ReactNativeWebView: { postMessage: (raw: string) => posted.push(JSON.parse(raw) as Posted) } };
  const URL = { createObjectURL: () => 'blob:0', revokeObjectURL: () => {} };
  class FakeBlob {
    constructor(public readonly parts: unknown[]) {}
  }

  // The attach control is a generated WebView script: executing it against a
  // stub DOM is the only way to assert its behaviour rather than its source.
  // eslint-disable-next-line no-new-func
  const run = new Function(
    'window',
    'document',
    'FileReader',
    'Image',
    'URL',
    'Blob',
    'btoa',
    script,
  ) as (...args: unknown[]) => void;
  run(window, document, FakeFileReader, FakeImage, URL, FakeBlob, (value: string) => Buffer.from(value, 'binary').toString('base64'));

  return {
    accept,
    posted,
    canvases,
    choose: (files) => {
      if (!onChange) throw new Error('the picker never registered a change handler');
      onChange({ target: { files, value: 'picked' } });
    },
    settle: async () => {
      for (let tick = 0; tick < 50; tick += 1) {
        if (posted.some((event) => event.type === 'batch-end')) return;
        const { promise, resolve } = Promise.withResolvers<void>();
        setTimeout(resolve, 0);
        await promise;
      }
      throw new Error('the picker never drained its selection');
    },
  };
}

function reassemble(posted: Posted[]): { header: Posted; text: string; bytes: Uint8Array } {
  const header = posted.find((event) => event.type === 'begin') as Posted;
  const chunks = posted.filter((event) => event.type === 'chunk').map((event) => event.data as string);
  if (header.encoding !== 'bytes') {
    return { header, text: chunks.join(''), bytes: new Uint8Array(0) };
  }
  const parts = chunks.map(base64ToBytes);
  const bytes = new Uint8Array(parts.reduce((total, part) => total + part.length, 0));
  let at = 0;
  for (const part of parts) {
    bytes.set(part, at);
    at += part.length;
  }
  return { header, text: '', bytes };
}

function reasons(posted: Posted[]): Array<[string, string]> {
  return posted
    .filter((event) => event.type === 'reject')
    .map((event) => [event.name as string, event.reason as string]);
}

it('advertises the supported kinds in the picker accept list instead of a wildcard', () => {
  const { accept } = runPicker();
  expect(accept).not.toContain('*');
  const entries = accept.split(',');
  for (const entry of ['image/png', 'image/jpeg', 'image/gif', 'image/webp', 'video/mp4', 'video/x-matroska', 'video/quicktime', '.mkv', '.mov', 'application/pdf', '.pdf', '.ts', '.md', '.json']) {
    expect(entries).toContain(entry);
  }
  expect(entries).not.toContain('.exe');
});

it('refuses an unsupported binary without reading it', async () => {
  const picker = runPicker();
  picker.choose([fakeFile('installer.exe', 'application/octet-stream', new Uint8Array([0x4d, 0x5a]))]);
  await picker.settle();
  expect(reasons(picker.posted)).toEqual([['installer.exe', 'unsupported']]);
  expect(picker.posted.some((event) => event.type === 'begin' || event.type === 'chunk')).toBe(false);
  expect(picker.posted.filter((event) => event.type === 'batch-end')).toHaveLength(1);
});

it('refuses each kind one byte over its own cap and accepts it at the cap', async () => {
  const over = [
    fakeFile('huge.png', 'image/png', new Uint8Array(0), MAX_IMAGE_INPUT_BYTES + 1),
    fakeFile('big.pdf', 'application/pdf', new Uint8Array(0), MAX_PDF_BYTES + 1),
    fakeFile('long.mkv', 'video/x-matroska', new Uint8Array(0), MAX_VIDEO_BYTES + 1),
    fakeFile('notes.txt', 'text/plain', new Uint8Array(0), MAX_CODE_BYTES + 1),
  ];
  const picker = runPicker();
  picker.choose(over);
  await picker.settle();
  expect(reasons(picker.posted)).toEqual([
    ['huge.png', 'image-oversize'],
    ['big.pdf', 'pdf-oversize'],
    ['long.mkv', 'video-oversize'],
    ['notes.txt', 'oversize'],
  ]);
  expect(picker.posted.some((event) => event.type === 'begin')).toBe(false);

  const within = runPicker();
  const payload = new Uint8Array([1, 2, 3, 4]);
  within.choose([fakeFile('clip.mp4', 'video/mp4', payload, MAX_VIDEO_BYTES)]);
  await within.settle();
  expect(within.posted.some((event) => event.type === 'reject')).toBe(false);
  expect(reassemble(within.posted).header.encoding).toBe('bytes');
});

it('streams a video as chunked raw bytes that reassemble exactly', async () => {
  const payload = new Uint8Array(300 * 1024 + 5);
  for (let index = 0; index < payload.length; index += 1) payload[index] = (index * 31) % 251;
  const picker = runPicker();
  picker.choose([fakeFile('clip.mkv', 'video/x-matroska', payload)]);
  await picker.settle();

  const { header, bytes } = reassemble(picker.posted);
  expect(header).toMatchObject({ name: 'clip.mkv', mimeType: 'video/x-matroska', size: payload.length, encoding: 'bytes' });
  expect(header.total).toBe(2);
  expect(picker.posted.filter((event) => event.type === 'chunk')).toHaveLength(2);
  expect(Buffer.from(bytes).equals(Buffer.from(payload))).toBe(true);
  expect(picker.posted[picker.posted.length - 1]).toEqual({ type: 'batch-end' });
});

it('streams a PDF as raw bytes and a code file as UTF-8 text', async () => {
  const pdf = Buffer.from('%PDF-1.7 body bytes', 'utf8');
  const picker = runPicker();
  picker.choose([fakeFile('report.pdf', 'application/pdf', new Uint8Array(pdf))]);
  await picker.settle();
  const pdfStream = reassemble(picker.posted);
  expect(pdfStream.header.encoding).toBe('bytes');
  expect(Buffer.from(pdfStream.bytes).equals(pdf)).toBe(true);

  const code = runPicker();
  code.choose([fakeFile('module.ts', 'text/typescript', new Uint8Array(Buffer.from('const answer = 42;', 'utf8')))]);
  await code.settle();
  const codeStream = reassemble(code.posted);
  expect(codeStream.header).toMatchObject({ encoding: 'text', name: 'module.ts' });
  expect(codeStream.text).toBe('const answer = 42;');
  expect(codeStream.header.size).toBe(Buffer.byteLength('const answer = 42;', 'utf8'));

  const binary = runPicker();
  binary.choose([fakeFile('dump.txt', 'text/plain', new Uint8Array([0xff, 0xfe, 0x00, 0x01]))]);
  await binary.settle();
  expect(reasons(binary.posted)).toEqual([['dump.txt', 'invalid-utf8']]);
  expect(binary.posted.some((event) => event.type === 'begin')).toBe(false);
});

it('refuses an image over the pixel ceiling and scales the rest to the 2000px edge', async () => {
  const bomb = runPicker({ decoded: { width: 5000, height: 5000 } });
  bomb.choose([fakeFile('bomb.png', 'image/png', new Uint8Array(1024))]);
  await bomb.settle();
  expect(5000 * 5000).toBeGreaterThan(MAX_IMAGE_PIXELS);
  expect(reasons(bomb.posted)).toEqual([['bomb.png', 'image-processing']]);
  expect(bomb.canvases).toHaveLength(0);

  // A 4000px-wide photo over the processed cap is re-encoded at the 2000px edge
  // (not 2048) and reported with its PROCESSED size.
  const wide = runPicker({ decoded: { width: 4000, height: 1000 }, encodedBytes: 512 * 1024 });
  wide.choose([fakeFile('wide.png', 'image/png', new Uint8Array(3 * 1024 * 1024))]);
  await wide.settle();
  expect(wide.canvases).toHaveLength(1);
  expect(wide.canvases[0]).toMatchObject({ width: 2000, height: 500 });
  const wideStream = reassemble(wide.posted);
  expect(wideStream.header).toMatchObject({ name: 'wide.jpg', mimeType: 'image/jpeg', encoding: 'base64', size: 512 * 1024 });

  // A small photo passes through untouched, with no canvas work at all.
  const small = runPicker({ decoded: { width: 800, height: 600 } });
  small.choose([fakeFile('small.png', 'image/png', new Uint8Array([1, 2, 3, 4]))]);
  await small.settle();
  expect(small.canvases).toHaveLength(0);
  expect(reassemble(small.posted).header).toMatchObject({ name: 'small.png', encoding: 'base64', size: 4 });
});

it('refuses an image that stays over the processed cap after the quality ladder', async () => {
  const picker = runPicker({ decoded: { width: 4000, height: 1000 }, encodedBytes: 4 * 1024 * 1024 });
  picker.choose([fakeFile('noisy.png', 'image/png', new Uint8Array(5 * 1024 * 1024))]);
  await picker.settle();
  expect(reasons(picker.posted)).toEqual([['noisy.png', 'resized-oversize']]);
  expect(picker.posted.some((event) => event.type === 'begin')).toBe(false);
});
