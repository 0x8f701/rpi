import type { PdfUploadOutcome } from './chatSync';
import type { RunnerCommand } from './protocol';

/** Composer attachment intake: the /web allowlist and per-kind caps, the
 *  WebView picker bridge framing, and the wire mapping (images and video frames
 *  → `images` content blocks, code/text → fenced message, PDF → uploaded text
 *  layer, video → uploaded chronological frame sequence). */

/* ---------------- per-kind caps (mirror crates/pi-cli/web/src/attachments.ts) ---------------- */

/** Per-code-file raw-byte cap (the Web `MAX_FILE_BYTES`). */
export const MAX_CODE_BYTES = 2 * 1024 * 1024;
/** Raw camera-photo input cap. */
export const MAX_IMAGE_INPUT_BYTES = 20 * 1024 * 1024;
/** Processed-image cap: the JPEG the picker encodes is scaled and requantized
 *  until it fits this, then refused when it still does not. */
export const MAX_IMAGE_PROCESSED_BYTES = 2 * 1024 * 1024;
/** Raw-byte cap for a PDF upload (the backend's 32 MiB bound). */
export const MAX_PDF_BYTES = 32 * 1024 * 1024;
/** Raw-byte cap for a video upload (the backend's 64 MiB bound). */
export const MAX_VIDEO_BYTES = 64 * 1024 * 1024;

const MAX_PROMPT_FRAME_BYTES = 4 * 1024 * 1024;
const FRAME_RESERVE_BYTES = 1 * 1024 * 1024;
export const MAX_TOTAL_WIRE_BYTES = MAX_PROMPT_FRAME_BYTES - FRAME_RESERVE_BYTES;
const MAX_ATTACHMENTS = 8;

/** Longest edge a picked image is scaled to, the JPEG quality ladder tried
 *  while the encoded image is over the processed cap, and the decoded-pixel
 *  safety bound that refuses a decode bomb before it is scaled. */
const MAX_IMAGE_EDGE = 2000;
export const MAX_IMAGE_PIXELS = 16 * 1024 * 1024;
const JPEG_QUALITIES = [0.85, 0.7, 0.55];

/** One WebView bridge message is a single binder transaction, so a file is
 *  sliced well under 1 MB. `PICKER_CHUNK_CHARS` is a multiple of 4, so every
 *  base64 slice is a whole number of quads and each chunk decodes on its own. */
const PICKER_CHUNK_CHARS = 256 * 1024;
const PICKER_CHUNK_BYTES = (PICKER_CHUNK_CHARS / 4) * 3;
/** Chunk bound that covers the largest accepted payload (a 64 MiB video). */
const MAX_PICKER_CHUNKS = 384;

/* ---------------- video upload limits (mirror the backend) ---------------- */

const VIDEO_UPLOAD_PATH = '/upload/video';
const VIDEO_UPLOAD_TIMEOUT_MS = 90_000;
const MAX_VIDEO_DURATION_SECONDS = 600;
const MAX_VIDEO_FRAMES = 6;
const MAX_VIDEO_FRAME_BYTES = 384 * 1024;
const MAX_VIDEO_FRAMES_RAW_BYTES = 2 * 1024 * 1024;
const MAX_VIDEO_FRAME_BASE64 = base64Length(MAX_VIDEO_FRAME_BYTES);
const MAX_VIDEO_FRAMES_BASE64 = base64Length(MAX_VIDEO_FRAMES_RAW_BYTES);
const MAX_VIDEO_INSTRUCTION_BYTES = 2048;
const MAX_UPLOAD_ERROR_CHARS = 300;

/* ---------------- tmux-file upload (POST /upload/tmux-file) ---------------- */

const TMUX_FILE_UPLOAD_PATH = '/upload/tmux-file';
const TMUX_FILE_UPLOAD_TIMEOUT_MS = 30_000;

const IMAGE_EXTENSIONS = ['png', 'jpg', 'jpeg', 'gif', 'webp'];
const IMAGE_EXTENSION = new RegExp(`\\.(${IMAGE_EXTENSIONS.join('|')})$`, 'i');
const IMAGE_MIME_TYPES: Record<string, true> = {
  'image/png': true,
  'image/jpeg': true,
  'image/gif': true,
  'image/webp': true,
};
const IMAGE_UPLOAD_EXTENSIONS: Record<string, string> = {
  'image/png': 'png',
  'image/jpeg': 'jpg',
  'image/gif': 'gif',
  'image/webp': 'webp',
};
const IMAGE_LABEL = 'png/jpeg/gif/webp';

const VIDEO_CONTAINERS: Record<string, true> = {
  mkv: true,
  mp4: true,
  webm: true,
  mov: true,
  avi: true,
  ogg: true,
};
const VIDEO_MIME_TYPES: Record<string, string> = {
  mkv: 'video/x-matroska',
  mp4: 'video/mp4',
  webm: 'video/webm',
  mov: 'video/quicktime',
  avi: 'video/x-msvideo',
  ogg: 'video/ogg',
};
const VIDEO_LABEL = 'mkv/mp4/webm/mov/avi/ogg';

const TEXT_EXTENSIONS: Record<string, true> = {
  rs: true, ts: true, tsx: true, js: true, jsx: true, mjs: true, cjs: true, py: true,
  go: true, java: true, kt: true, kts: true, c: true, cc: true, cpp: true, cxx: true,
  h: true, hh: true, hpp: true, hxx: true, cs: true, rb: true, php: true, swift: true,
  scala: true, clj: true, cljs: true, ex: true, exs: true, erl: true, hs: true, ml: true,
  fs: true, fsx: true, lua: true, pl: true, pm: true, r: true, dart: true, groovy: true,
  gradle: true, jl: true, nim: true, cr: true, zig: true, v: true, sv: true, elm: true,
  purs: true, sql: true, sh: true, bash: true, zsh: true, fish: true, ps1: true, bat: true,
  cmd: true, html: true, htm: true, css: true, scss: true, sass: true, less: true,
  xml: true, svg: true, vue: true, svelte: true, astro: true, json: true, json5: true,
  jsonc: true, yaml: true, yml: true, toml: true, ini: true, cfg: true, conf: true,
  env: true, properties: true, lock: true, log: true, csv: true, tsv: true, md: true,
  markdown: true, rst: true, adoc: true, tex: true, txt: true,
};

const TEXT_BASENAMES: Record<string, true> = {
  dockerfile: true,
  makefile: true,
  '.gitignore': true,
  '.editorconfig': true,
  '.npmrc': true,
  '.env': true,
};

const TEXT_APPLICATION_MIMES: Record<string, true> = {
  'application/json': true,
  'application/xml': true,
  'application/javascript': true,
  'application/x-yaml': true,
  'application/yaml': true,
  'application/x-sh': true,
};

const LANGUAGE_HINTS: Record<string, string> = {
  rs: 'rust', ts: 'typescript', tsx: 'tsx', js: 'javascript', mjs: 'javascript', cjs: 'javascript',
  jsx: 'jsx', py: 'python', go: 'go', java: 'java', kt: 'kotlin', kts: 'kotlin', c: 'c', h: 'c',
  cc: 'cpp', cpp: 'cpp', cxx: 'cpp', hpp: 'cpp', hxx: 'cpp', hh: 'cpp', cs: 'csharp', rb: 'ruby',
  php: 'php', swift: 'swift', scala: 'scala', clj: 'clojure', cljs: 'clojure', ex: 'elixir',
  exs: 'elixir', erl: 'erlang', hs: 'haskell', ml: 'ocaml', fs: 'fsharp', fsx: 'fsharp', lua: 'lua',
  pl: 'perl', pm: 'perl', r: 'r', dart: 'dart', groovy: 'groovy', gradle: 'groovy', jl: 'julia',
  nim: 'nim', cr: 'crystal', zig: 'zig', v: 'verilog', sv: 'systemverilog', elm: 'elm',
  purs: 'purescript', sql: 'sql', sh: 'bash', bash: 'bash', zsh: 'bash', fish: 'fish',
  ps1: 'powershell', bat: 'bat', cmd: 'bat', html: 'html', htm: 'html', css: 'css', scss: 'scss',
  sass: 'sass', less: 'less', xml: 'xml', svg: 'xml', vue: 'vue', svelte: 'svelte', astro: 'astro',
  json: 'json', json5: 'json', jsonc: 'json', yaml: 'yaml', yml: 'yaml', toml: 'toml', ini: 'ini',
  cfg: 'ini', conf: 'ini', env: 'ini', properties: 'ini', csv: 'csv', tsv: 'tsv', md: 'markdown',
  markdown: 'markdown', rst: 'rst', adoc: 'asciidoc', tex: 'latex', txt: 'text', log: 'log',
};

const MAX_FILENAME_LEN = 128;
const MAX_PDF_INSTRUCTION_BYTES = 2048;

export type AttachmentKind = 'image' | 'video' | 'code' | 'pdf';

export type AttachmentSkipReason =
  | 'unsupported'
  | 'oversize'
  | 'image-oversize'
  | 'image-processing'
  | 'resized-oversize'
  | 'video-oversize'
  | 'pdf-oversize'
  | 'too-many'
  | 'over-budget'
  | 'invalid-utf8'
  | 'unreadable';

const SKIP_REASONS: Record<string, true> = {
  unsupported: true,
  oversize: true,
  'image-oversize': true,
  'image-processing': true,
  'resized-oversize': true,
  'video-oversize': true,
  'pdf-oversize': true,
  'too-many': true,
  'over-budget': true,
  'invalid-utf8': true,
  unreadable: true,
};

export interface AttachmentSkip {
  name: string;
  size: number;
  reason: AttachmentSkipReason;
}

/** One extracted frame of the backend's video preprocessing response: raw
 *  base64 JPEG (no `data:` prefix), the same payload shape as an image block. */
export interface VideoFrame {
  index: number;
  timestampSeconds: number;
  mimeType: string;
  data: string;
}

export interface Attachment {
  id: string;
  name: string;
  size: number;
  mimeType: string;
  kind: AttachmentKind;
  state: 'ready' | 'uploading' | 'error';
  /** Failed-upload reason shown on the chip (state 'error'). */
  error?: string;
  base64?: string;
  previewUrl?: string;
  text?: string;
  language?: string;
  instruction?: string;
  /** pdf/video: the raw bytes the upload (and its retry) POSTs. Released once
   *  the attachment is ready. */
  bytes?: Uint8Array;
  /** video: the extracted chronological frames (ready state). */
  frames?: VideoFrame[];
  durationSeconds?: number;
}

export interface ImageBlock {
  type: 'image';
  data: string;
  mimeType: string;
}

export type AttachmentPrompt = Extract<RunnerCommand, { type: 'prompt' | 'steer' }> & {
  images?: ImageBlock[];
};

export interface PickBegin {
  type: 'begin';
  name: string;
  mimeType: string;
  size: number;
  /** `base64`/`text` carry the payload as chunk strings; `bytes` is the same
   *  base64 framing, decoded straight into a byte buffer on the RN side (the
   *  raw-byte upload sources: PDF and video). */
  encoding: 'base64' | 'text' | 'bytes';
  total: number;
}

export type PickEvent =
  | PickBegin
  | { type: 'chunk'; data: string }
  | { type: 'end' }
  | { type: 'reject'; name: string; size: number; reason: AttachmentSkipReason }
  | { type: 'batch-end' };

export interface AcceptResult {
  items: Attachment[];
  /** Structured rejection for the batch summary; null when accepted. */
  skip: AttachmentSkip | null;
  /** One-file sentence for the single-file intake paths. */
  error: string | null;
  upload: { id: string } | null;
}

export interface UploadResult {
  items: Attachment[];
  error: string | null;
}

export type PromptBuild = { ok: true; command: AttachmentPrompt } | { ok: false; error: string };

export function parsePickEvent(raw: unknown): PickEvent | null {
  let value: unknown = raw;
  if (typeof raw === 'string') {
    try {
      value = JSON.parse(raw);
    } catch {
      return null;
    }
  }
  if (value === null || typeof value !== 'object' || Array.isArray(value)) return null;
  const row = value as Record<string, unknown>;
  if (row.type === 'chunk') return typeof row.data === 'string' ? { type: 'chunk', data: row.data } : null;
  if (row.type === 'end') return { type: 'end' };
  if (row.type === 'batch-end') return { type: 'batch-end' };
  if (row.type === 'begin') {
    if (typeof row.name !== 'string' || row.name === '') return null;
    if (typeof row.size !== 'number' || !Number.isFinite(row.size) || row.size < 0) return null;
    if (row.encoding !== 'base64' && row.encoding !== 'text' && row.encoding !== 'bytes') return null;
    if (typeof row.total !== 'number' || !Number.isInteger(row.total) || row.total < 0 || row.total > MAX_PICKER_CHUNKS) {
      return null;
    }
    return {
      type: 'begin',
      name: row.name,
      mimeType: typeof row.mimeType === 'string' ? row.mimeType : '',
      size: row.size,
      encoding: row.encoding,
      total: row.total,
    };
  }
  if (row.type === 'reject') {
    if (typeof row.name !== 'string' || typeof row.size !== 'number' || !Number.isFinite(row.size)) return null;
    if (typeof row.reason !== 'string' || SKIP_REASONS[row.reason] !== true) return null;
    return { type: 'reject', name: row.name, size: row.size, reason: row.reason as AttachmentSkipReason };
  }
  return null;
}

/** Size cap for one attachment kind. */
export function attachmentSizeCap(kind: AttachmentKind): number {
  if (kind === 'image') return MAX_IMAGE_INPUT_BYTES;
  if (kind === 'video') return MAX_VIDEO_BYTES;
  if (kind === 'pdf') return MAX_PDF_BYTES;
  return MAX_CODE_BYTES;
}

function sizeSkipReason(kind: AttachmentKind): AttachmentSkipReason {
  if (kind === 'image') return 'image-oversize';
  if (kind === 'video') return 'video-oversize';
  if (kind === 'pdf') return 'pdf-oversize';
  return 'oversize';
}

export function attachmentSkipMessage(reason: AttachmentSkipReason, name: string): string {
  const label = sanitizeFileName(name);
  switch (reason) {
    case 'unsupported':
      return `${label} is not supported — attach an image (${IMAGE_LABEL}), a video (${VIDEO_LABEL}), a PDF, or a UTF-8 text/code file.`;
    case 'oversize':
      return `${label} is over the ${formatBytes(MAX_CODE_BYTES)} per-file attachment limit.`;
    case 'image-oversize':
      return `${label} is over the ${formatBytes(MAX_IMAGE_INPUT_BYTES)} image input limit.`;
    case 'image-processing':
      return `${label} could not be decoded or resized safely — pick a smaller image.`;
    case 'resized-oversize':
      return `${label} was scaled down to ${MAX_IMAGE_EDGE}px but is still too large to attach — pick a smaller image.`;
    case 'video-oversize':
      return `${label} is over the ${formatBytes(MAX_VIDEO_BYTES)} video upload limit.`;
    case 'pdf-oversize':
      return `${label} is over the ${formatBytes(MAX_PDF_BYTES)} PDF upload limit.`;
    case 'too-many':
      return `At most ${MAX_ATTACHMENTS} attachments per message.`;
    case 'over-budget':
      return `${label} does not fit the ${formatBytes(MAX_TOTAL_WIRE_BYTES)} attachment budget — remove an attachment or pick a smaller file.`;
    case 'invalid-utf8':
      return `${label} is not valid UTF-8 (binary content).`;
    case 'unreadable':
      return `${label} could not be read.`;
  }
}

/** Format a batch's skipped files into ONE grouped summary naming the files, or
 *  null when nothing was skipped. Grouped by reason in first-seen order, so a
 *  mixed batch reports every skip category at once instead of one message per
 *  file. */
export function formatSkipSummary(skipped: readonly AttachmentSkip[]): string | null {
  if (skipped.length === 0) return null;
  const labels: Record<AttachmentSkipReason, string> = {
    unsupported: 'unsupported (images, videos, PDFs, or text/code files only)',
    oversize: `over the ${formatBytes(MAX_CODE_BYTES)} per-file limit`,
    'image-oversize': `over the ${formatBytes(MAX_IMAGE_INPUT_BYTES)} image input limit`,
    'image-processing': 'could not be decoded or resized safely',
    'resized-oversize': `still too large after scaling to ${MAX_IMAGE_EDGE}px`,
    'video-oversize': `over the ${formatBytes(MAX_VIDEO_BYTES)} video upload limit`,
    'pdf-oversize': `over the ${formatBytes(MAX_PDF_BYTES)} PDF upload limit`,
    'too-many': `over the ${MAX_ATTACHMENTS}-attachment limit`,
    'over-budget': 'exceeds the total attachment budget',
    'invalid-utf8': 'not valid UTF-8 (binary content)',
    unreadable: 'could not be read',
  };
  const order: AttachmentSkipReason[] = [];
  const groups = new Map<AttachmentSkipReason, string[]>();
  for (const skip of skipped) {
    const names = groups.get(skip.reason);
    if (names === undefined) {
      groups.set(skip.reason, [sanitizeFileName(skip.name)]);
      order.push(skip.reason);
    } else {
      names.push(sanitizeFileName(skip.name));
    }
  }
  const parts = order.map((reason) => {
    const names = groups.get(reason) as string[];
    const sample = names.length <= 2
      ? names.join(', ')
      : `${names.slice(0, 2).join(', ')} +${names.length - 2} more`;
    return `${names.length} ${labels[reason]} (${sample})`;
  });
  return `Skipped ${skipped.length} file(s): ${parts.join('; ')}.`;
}

function extOf(name: string): string {
  const lower = name.toLowerCase();
  const base = lower.split('/').pop() ?? lower;
  return base.slice(base.lastIndexOf('.') + 1);
}

function classifyKind(file: { name: string; mimeType: string }): AttachmentKind | null {
  if (IMAGE_MIME_TYPES[file.mimeType] === true || IMAGE_EXTENSION.test(file.name)) return 'image';
  const ext = extOf(file.name);
  if (VIDEO_CONTAINERS[ext] === true) return 'video';
  const lower = file.name.toLowerCase();
  const base = lower.split('/').pop() ?? lower;
  if (ext === 'pdf') return 'pdf';
  if (TEXT_BASENAMES[base] === true || TEXT_EXTENSIONS[ext] === true) return 'code';
  if (TEXT_APPLICATION_MIMES[file.mimeType] === true) return 'code';
  return file.mimeType.startsWith('text/') ? 'code' : null;
}

function sniffImageMime(base64: string): string | null {
  const head = base64ToBytes(base64.slice(0, 24));
  if (head[0] === 0xff && head[1] === 0xd8 && head[2] === 0xff) return 'image/jpeg';
  if (head[0] === 0x89 && head[1] === 0x50 && head[2] === 0x4e && head[3] === 0x47) return 'image/png';
  if (head[0] === 0x47 && head[1] === 0x49 && head[2] === 0x46 && head[3] === 0x38) return 'image/gif';
  if (
    head[0] === 0x52 && head[1] === 0x49 && head[2] === 0x46 && head[3] === 0x46
    && head[8] === 0x57 && head[9] === 0x45 && head[10] === 0x42 && head[11] === 0x50
  ) {
    return 'image/webp';
  }
  return null;
}

function codeLanguage(name: string): string {
  const base = name.toLowerCase().split('/').pop() ?? '';
  if (base === 'dockerfile') return 'dockerfile';
  if (base === 'makefile') return 'makefile';
  const dot = base.lastIndexOf('.');
  return dot < 0 ? '' : LANGUAGE_HINTS[base.slice(dot + 1)] ?? '';
}

function sanitizeFileName(name: string): string {
  const oneLine = name.replace(/[\u0000-\u001F\u007F]+/g, ' ').trim();
  const bounded = oneLine.length > MAX_FILENAME_LEN ? oneLine.slice(0, MAX_FILENAME_LEN) : oneLine;
  return bounded || 'file';
}

function safeFence(text: string): string {
  let maxRun = 0;
  let run = 0;
  for (let index = 0; index < text.length; index += 1) {
    if (text.charCodeAt(index) === 96) {
      run += 1;
      if (run > maxRun) maxRun = run;
    } else {
      run = 0;
    }
  }
  return '`'.repeat(Math.max(3, maxRun + 1));
}

function buildCodeSegment(name: string, language: string, text: string): string {
  const fence = safeFence(text);
  return `File: ${sanitizeFileName(name)}\n${fence}${language}\n${text}\n${fence}`;
}

function formatBytes(size: number): string {
  if (size >= 1024 * 1024) return `${(size / (1024 * 1024)).toFixed(1)} MiB`;
  if (size >= 1024) return `${Math.round(size / 1024)} KiB`;
  return `${size} B`;
}

/** UTF-8 byte length of `s` (the wire counts UTF-8 bytes, not UTF-16 units). */
function utf8ByteLength(s: string): number {
  return new TextEncoder().encode(s).length;
}

/** Conservative stand-in for the transport's longest command id (`c` + 12
 *  base36 namespace characters + `-` + the sequence digits), so the
 *  pre-dispatch frame check never depends on predicting the next counter. */
export const MAX_COMMAND_ID = `c${'z'.repeat(12)}-${'9'.repeat(19)}`;

/** EXACT serialized bytes of a prompt command frame as the transport puts it
 *  on the wire: `JSON.stringify({...command, id, sessionId})` in the same key
 *  order and UTF-8 encoding. JSON escaping (quotes, backslashes, control
 *  characters) and multibyte characters are counted exactly as the runner
 *  reads them — a raw-text estimate is not a frame bound. Passing
 *  `MAX_COMMAND_ID` as `id` makes the check a conservative upper bound that
 *  never depends on predicting the command counter. */
export function commandFrameBytes(
  command: Record<string, unknown>,
  id: string,
  sessionId: string | null,
): number {
  const frame = sessionId ? { ...command, id, sessionId } : { ...command, id };
  return utf8ByteLength(JSON.stringify(frame));
}

function videoMetaLabel(item: Attachment): string {
  const frames = item.frames?.length ?? 0;
  const duration = item.durationSeconds;
  if (frames > 0 && duration !== undefined && Number.isFinite(duration)) {
    return `${frames} frame${frames === 1 ? '' : 's'} · ${duration.toFixed(1)} s`;
  }
  if (frames > 0) return `${frames} frame${frames === 1 ? '' : 's'}`;
  return 'video';
}

export function attachmentMeta(item: Attachment): string {
  if (item.state === 'error') return `${formatBytes(item.size)} · upload failed`;
  if (item.state === 'uploading') return `${formatBytes(item.size)} · uploading…`;
  if (item.kind === 'video') return videoMetaLabel(item);
  const label = item.kind === 'image'
    ? item.mimeType.slice('image/'.length).toUpperCase()
    : item.kind === 'pdf' ? 'PDF' : 'code';
  return `${formatBytes(item.size)} · ${label}`;
}

export function base64Length(rawBytes: number): number {
  return rawBytes <= 0 ? 0 : Math.ceil(rawBytes / 3) * 4;
}

const BASE64_ALPHABET = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';

/** Decode one base64 chunk (padding and any trailing partial quad ignored), so
 *  the picker's 4-char-aligned slices decode independently. */
export function base64ToBytes(base64: string): Uint8Array {
  const clean = base64.replace(/=+$/, '');
  const bytes = new Uint8Array(Math.floor((clean.length * 3) / 4));
  let accumulator = 0;
  let bits = 0;
  let written = 0;
  for (let index = 0; index < clean.length; index += 1) {
    const value = BASE64_ALPHABET.indexOf(clean[index]);
    if (value < 0) continue;
    accumulator = (accumulator << 6) | value;
    bits += 6;
    if (bits >= 8) {
      bits -= 8;
      bytes[written] = (accumulator >> bits) & 0xff;
      written += 1;
    }
  }
  return written === bytes.length ? bytes : bytes.subarray(0, written);
}

/** Wire estimate of the INCOMING pick at admission time: images larger than the
 *  processed cap reserve that cap (the picker normalizes them down to it), and
 *  raw PDF/video bytes never ride the prompt frame, so they contribute nothing
 *  yet — what a settled upload actually produces is measured by
 *  `aggregateWire` at the settle-time and submit-time re-checks. */
function wireFootprint(kind: AttachmentKind, size: number): number {
  if (kind === 'image') return base64Length(Math.min(size, MAX_IMAGE_PROCESSED_BYTES));
  if (kind === 'code') return size;
  return 0;
}

/** Combined wire footprint of the queued attachments (image base64 + video
 *  frame base64 + code text as its exact fenced segment + ready PDF text). Used
 *  for the post-upload re-check that keeps a batch inside one prompt frame. */
function aggregateWire(items: readonly Attachment[]): number {
  let total = 0;
  for (const item of items) {
    if (item.kind === 'image') {
      total += item.base64?.length ?? 0;
    } else if (item.kind === 'video') {
      for (const frame of item.frames ?? []) total += frame.data.length;
    } else if (item.kind === 'pdf') {
      total += item.state === 'ready' ? utf8ByteLength(pdfMarkerText(item)) : 0;
    } else if (item.text !== undefined) {
      total += utf8ByteLength(buildCodeSegment(item.name, item.language ?? '', item.text));
    } else {
      total += item.size;
    }
  }
  return total;
}

function imageBlocks(items: readonly Attachment[]): ImageBlock[] {
  const blocks: ImageBlock[] = [];
  for (const item of items) {
    if (item.kind === 'image' && item.base64 !== undefined) {
      blocks.push({ type: 'image', data: item.base64, mimeType: item.mimeType });
      continue;
    }
    if (item.kind === 'video' && item.state === 'ready') {
      for (const frame of item.frames ?? []) {
        blocks.push({ type: 'image', data: frame.data, mimeType: frame.mimeType });
      }
    }
  }
  return blocks;
}

function buildCodeMessage(items: readonly Attachment[]): string {
  return items
    .filter((item) => item.kind === 'code' && item.text !== undefined)
    .map((item) => buildCodeSegment(item.name, item.language ?? '', item.text as string))
    .join('\n\n');
}

/** Bounded marker for ONE ready video: the backend instruction when present
 *  (bounded server-side, re-capped here), else a fixed-shape local marker
 *  naming the file, the frame count, and each frame's timestamp. Empty when the
 *  video carries no frames. */
function videoMarkerText(item: Attachment): string {
  const frames = item.frames ?? [];
  if (frames.length === 0) return '';
  const instruction = (item.instruction ?? '').trim();
  if (instruction !== '' && utf8ByteLength(instruction) <= MAX_VIDEO_INSTRUCTION_BYTES) return instruction;
  const times = frames
    .filter((frame) => Number.isFinite(frame.timestampSeconds))
    .map((frame) => `${frame.timestampSeconds.toFixed(2)}s`)
    .join(', ');
  return `[Attached video: ${sanitizeFileName(item.name)} — the following ${frames.length} chronological frame${frames.length === 1 ? '' : 's'} (at ${times}) extracted from the video; analyze them in order as a video sequence.]`;
}

function buildVideoMessage(items: readonly Attachment[]): string {
  return items
    .filter((item) => item.kind === 'video' && item.state === 'ready')
    .map(videoMarkerText)
    .filter((marker) => marker !== '')
    .join('\n\n');
}

/** Bounded marker + extracted text for ONE ready PDF. Empty when the PDF has no
 *  extracted text. */
function pdfMarkerText(item: Attachment): string {
  const text = (item.text ?? '').trim();
  if (text === '') return '';
  const instruction = (item.instruction ?? '').trim();
  const header = instruction !== '' && utf8ByteLength(instruction) <= MAX_PDF_INSTRUCTION_BYTES
    ? instruction
    : `[PDF attachment: ${sanitizeFileName(item.name)} — extracted text layer; analyze the following as the document contents.]`;
  return `${header}\n${text}`;
}

function buildPdfMessage(items: readonly Attachment[]): string {
  return items
    .filter((item) => item.kind === 'pdf' && item.state === 'ready' && (item.text ?? '').trim() !== '')
    .map(pdfMarkerText)
    .join('\n\n');
}

let nextAttachmentId = 0;

export function acceptPicked(header: PickBegin, data: string | Uint8Array, queued: readonly Attachment[]): AcceptResult {
  const refuse = (reason: AttachmentSkipReason): AcceptResult => ({
    items: [...queued],
    skip: { name: sanitizeFileName(header.name), size: header.size, reason },
    error: attachmentSkipMessage(reason, header.name),
    upload: null,
  });
  const kind = classifyKind(header);
  if (!kind) return refuse('unsupported');
  if (header.size > attachmentSizeCap(kind)) return refuse(sizeSkipReason(kind));
  if (queued.length >= MAX_ATTACHMENTS) return refuse('too-many');
  const encoding = kind === 'code' ? 'text' : kind === 'image' ? 'base64' : 'bytes';
  if (header.encoding !== encoding) return refuse('unreadable');
  if (encoding === 'bytes') {
    if (typeof data === 'string' || data.length !== header.size) return refuse('unreadable');
  } else if (typeof data !== 'string') {
    return refuse('unreadable');
  } else if (encoding === 'base64') {
    if (data.length !== base64Length(header.size)) return refuse('unreadable');
  } else if (data.length > header.size) {
    return refuse('unreadable');
  }
  const container = extOf(header.name);
  const mimeType = kind === 'image'
    ? sniffImageMime(data as string)
    : kind === 'video' ? VIDEO_MIME_TYPES[container]
      : kind === 'pdf' ? 'application/pdf' : 'text/plain';
  if (!mimeType) return refuse('unreadable');
  // Admission counts the SAME wire data the final submit does: a READY PDF's
  // extracted text and a READY video's frames ride the prompt frame, so they
  // must spend the budget even though their raw bytes never do. The incoming
  // pick is estimated with `wireFootprint` (an image is clamped to what the
  // picker normalizes it down to; raw PDF/video bytes ride nothing).
  if (wireFootprint(kind, header.size) + aggregateWire(queued) > MAX_TOTAL_WIRE_BYTES) return refuse('over-budget');
  nextAttachmentId += 1;
  const item: Attachment = {
    id: `att${nextAttachmentId}`,
    name: sanitizeFileName(header.name),
    size: header.size,
    mimeType,
    kind,
    state: kind === 'pdf' || kind === 'video' ? 'uploading' : 'ready',
  };
  if (kind === 'image') {
    const built: Attachment = { ...item, base64: data as string, previewUrl: `data:${item.mimeType};base64,${data}` };
    return { items: [...queued, built], skip: null, error: null, upload: null };
  }
  if (kind === 'code') {
    const built: Attachment = { ...item, text: data as string, language: codeLanguage(header.name) };
    return { items: [...queued, built], skip: null, error: null, upload: null };
  }
  const pending: Attachment = { ...item, bytes: data as Uint8Array };
  return { items: [...queued, pending], skip: null, error: null, upload: { id: item.id } };
}

export function removeAttachment(items: readonly Attachment[], id: string): Attachment[] {
  return items.filter((item) => item.id !== id);
}

/** Re-arm a failed chip for another upload attempt (the retained bytes are the
 *  retry source). */
export function markUploading(items: readonly Attachment[], id: string): UploadResult {
  return {
    items: items.map((item) => (item.id === id ? { ...item, state: 'uploading' as const, error: undefined } : item)),
    error: null,
  };
}

/** Settle ONE PDF upload against the queue: a success stores the extracted text
 *  and drops the raw bytes, a failure KEEPS the chip in an error state (the
 *  reason is both on the chip and returned for the toast), and a chip that is
 *  gone (removed mid-upload) settles silently. A chip whose extracted text does
 *  not fit the batch budget is rejected from the QUEUED row, so it keeps the
 *  bytes its Retry re-POSTs instead of settling with nothing left to send. */
export function completePdfUpload(items: readonly Attachment[], id: string, outcome: PdfUploadOutcome): UploadResult {
  const item = items.find((row) => row.id === id);
  if (!item) return { items: [...items], error: null };
  if (!outcome.ok) {
    return {
      items: items.map((row) => (row.id === id ? { ...row, state: 'error' as const, error: outcome.message } : row)),
      error: `${item.name} failed to upload: ${outcome.message}`,
    };
  }
  const settled: Attachment[] = items.map((row) => (row.id === id
    ? { ...row, state: 'ready' as const, text: outcome.text, instruction: outcome.instruction, bytes: undefined }
    : row));
  if (aggregateWire(settled) > MAX_TOTAL_WIRE_BYTES) {
    return {
      items: items.map((row) => (row.id === id
        ? {
            ...row,
            state: 'error' as const,
            error: 'extracted PDF text exceeds the total send size limit — remove other attachments or this PDF',
          }
        : row)),
      error: `${item.name} failed to upload: extracted text exceeds the total send size limit`,
    };
  }
  return { items: settled, error: null };
}

/** Settle ONE video upload against the queue. Same contract as
 *  `completePdfUpload`; the ready chip carries the extracted frames (its
 *  thumbnail is the first frame) and releases the raw video bytes, while a chip
 *  rejected by the batch budget keeps those bytes for its Retry. */
export function completeVideoUpload(items: readonly Attachment[], id: string, outcome: VideoUploadOutcome): UploadResult {
  const item = items.find((row) => row.id === id);
  if (!item) return { items: [...items], error: null };
  if (!outcome.ok) {
    return {
      items: items.map((row) => (row.id === id ? { ...row, state: 'error' as const, error: outcome.message } : row)),
      error: `${item.name} failed to upload: ${outcome.message}`,
    };
  }
  const frames = outcome.frames;
  const settled: Attachment[] = items.map((row) => (row.id === id
    ? {
        ...row,
        state: 'ready' as const,
        error: undefined,
        bytes: undefined,
        mimeType: outcome.mimeType,
        size: outcome.sizeBytes > 0 ? outcome.sizeBytes : row.size,
        frames,
        durationSeconds: outcome.durationSeconds,
        instruction: outcome.instruction,
        previewUrl: frames.length > 0 ? `data:${frames[0].mimeType};base64,${frames[0].data}` : undefined,
      }
    : row));
  if (aggregateWire(settled) > MAX_TOTAL_WIRE_BYTES) {
    return {
      items: items.map((row) => (row.id === id
        ? {
            ...row,
            state: 'error' as const,
            error: 'extracted video frames exceed the total send size limit — remove other attachments or this video',
          }
        : row)),
      error: `${item.name} failed to upload: extracted frames exceed the total send size limit`,
    };
  }
  return { items: settled, error: null };
}

export function buildPromptCommand(
  items: readonly Attachment[],
  message: string,
  kind: 'prompt' | 'steer',
  sessionId: string | null = null,
): PromptBuild {
  const uploading = items.find((item) => item.state === 'uploading');
  if (uploading) return { ok: false, error: `${uploading.name} is still uploading — wait for it to finish.` };
  const failed = items.find((item) => item.state === 'error');
  if (failed) {
    return {
      ok: false,
      error: `${failed.name} failed to upload${failed.error ? `: ${failed.error}` : ''} — retry or remove it.`,
    };
  }
  // Final bound, measured on the payload that will actually be serialized (the
  // same accounting the intake and every upload settle use): a queue assembled
  // by any path must never exceed one prompt frame.
  if (aggregateWire(items) > MAX_TOTAL_WIRE_BYTES) {
    return {
      ok: false,
      error: `Attachments exceed the ${formatBytes(MAX_TOTAL_WIRE_BYTES)} prompt budget — remove one or pick a smaller file.`,
    };
  }
  const text = [buildCodeMessage(items), buildVideoMessage(items), buildPdfMessage(items), message.trim()]
    .filter((segment) => segment !== '')
    .join('\n\n');
  const images = imageBlocks(items);
  if (text === '' && images.length === 0) return { ok: false, error: 'Nothing to attach or send.' };
  const command = kind === 'steer'
    ? { type: 'steer' as const, message: text, ...(images.length > 0 ? { images } : {}) }
    : { type: 'prompt' as const, message: text, ...(images.length > 0 ? { images } : {}) };
  // Byte-exact serialized frame check — the same JSON escaping and UTF-8 the
  // transport serializes. Control characters (a U+0001 escapes to six bytes)
  // and multibyte text inflate the frame far beyond the raw text, so the
  // attachment estimate alone is not a bound. Rejected BEFORE the send.
  if (commandFrameBytes(command, MAX_COMMAND_ID, sessionId) >= MAX_PROMPT_FRAME_BYTES) {
    return {
      ok: false,
      error: `message and attachments exceed the ${formatBytes(MAX_PROMPT_FRAME_BYTES)} send limit — shorten the message or remove some attachments`,
    };
  }
  return { ok: true, command };
}

/* ---------------- large-text paste intake ---------------- */

/** Name of the attachment a large pasted block becomes. */
export const PASTED_TEXT_NAME = 'pasted-text.txt';
const LARGE_PASTE_BYTES = 8 * 1024;
const LARGE_PASTE_LINES = 32;

export type PastedTextPlan =
  | { type: 'native' }
  | { type: 'oversize'; size: number }
  | { type: 'attachment'; text: string; size: number };

/** Classify one composer edit. A paste lands as a single contiguous insertion,
 *  so the inserted text is the delta between the previous and the next draft
 *  (common prefix + suffix stripped). Multiline-or-large insertions become a
 *  `pasted-text.txt` attachment instead of materializing in the input; anything
 *  over the code cap is refused with its size. A typing edit is native. */
export function planPastedText(previous: string, next: string): PastedTextPlan {
  if (next.length <= previous.length) return { type: 'native' };
  const shortest = Math.min(previous.length, next.length);
  let head = 0;
  while (head < shortest && previous[head] === next[head]) head += 1;
  let tail = 0;
  const maxTail = Math.min(previous.length - head, next.length - head);
  while (tail < maxTail && previous[previous.length - 1 - tail] === next[next.length - 1 - tail]) tail += 1;
  const inserted = next.slice(head, next.length - tail);
  if (inserted === '') return { type: 'native' };
  let lines = 1;
  for (let index = 0; index < inserted.length; index += 1) {
    const code = inserted.charCodeAt(index);
    if (code === 13) {
      lines += 1;
      if (inserted.charCodeAt(index + 1) === 10) index += 1;
    } else if (code === 10) {
      lines += 1;
    }
  }
  const size = utf8ByteLength(inserted);
  if (lines < LARGE_PASTE_LINES && size < LARGE_PASTE_BYTES) return { type: 'native' };
  if (size > MAX_CODE_BYTES) return { type: 'oversize', size };
  return { type: 'attachment', text: inserted, size };
}

/* ---------------- picker bridge stream ---------------- */

export interface PickStream {
  header: PickBegin;
  text: string[];
  bytes: Uint8Array | null;
  written: number;
}

/** Open the accumulator for one picked file. A `bytes` payload is decoded
 *  straight into a preallocated buffer; a header above the largest per-kind cap
 *  allocates nothing (acceptPicked refuses it on the cap) so a hostile or
 *  corrupt `begin` cannot make the app allocate unbounded memory. */
export function beginPickStream(header: PickBegin): PickStream {
  const allocate = header.encoding === 'bytes' && header.size <= MAX_VIDEO_BYTES;
  return { header, text: [], bytes: allocate ? new Uint8Array(header.size) : null, written: 0 };
}

export function pushPickChunk(stream: PickStream, data: string): void {
  if (stream.header.encoding !== 'bytes') {
    if (stream.text.length >= stream.header.total) return;
    stream.text.push(data);
    return;
  }
  const target = stream.bytes;
  if (target === null) return;
  const decoded = base64ToBytes(data);
  if (stream.written + decoded.length > target.length) return;
  target.set(decoded, stream.written);
  stream.written += decoded.length;
}

/** Payload of a finished stream: the joined text for `text`/`base64` (the
 *  picker slices base64 on 4-char boundaries, so the join is exact) or the
 *  filled byte buffer for `bytes`. */
export function finishPickStream(stream: PickStream): string | Uint8Array {
  if (stream.header.encoding !== 'bytes') return stream.text.join('');
  const target = stream.bytes;
  if (target === null) return new Uint8Array(0);
  return target.subarray(0, stream.written);
}

/* ---------------- video upload client (POST /upload/video) ---------------- */

export interface VideoUploadData {
  frames: VideoFrame[];
  instruction: string;
  durationSeconds: number;
  mimeType: string;
  sizeBytes: number;
}

export type VideoUploadOutcome = ({ ok: true } & VideoUploadData) | { ok: false; message: string };

/** Strictly validate ONE wire frame; ANY violation rejects the whole response
 *  (never silently filtered), so a malformed backend cannot smuggle a
 *  non-JPEG block or an unbounded payload past the client. */
function validateVideoFrame(value: unknown, position: number): VideoFrame | null {
  if (!value || typeof value !== 'object') return null;
  const record = value as Record<string, unknown>;
  if (record.index !== position) return null;
  if (record.mimeType !== 'image/jpeg') return null;
  if (typeof record.timestampSeconds !== 'number' || !Number.isFinite(record.timestampSeconds) || record.timestampSeconds < 0) return null;
  if (typeof record.data !== 'string' || record.data === '' || record.data.length > MAX_VIDEO_FRAME_BASE64) return null;
  return { index: position, timestampSeconds: record.timestampSeconds, mimeType: 'image/jpeg', data: record.data };
}

/** Strictly validate a 2xx upload body: bounded instruction, positive duration
 *  within the backend's cap, 1..MAX_VIDEO_FRAMES frames with chronological
 *  indices, and per-frame + aggregate base64 within the encoded caps. */
function validateVideoUploadData(body: unknown): VideoUploadData | null {
  if (!body || typeof body !== 'object') return null;
  const record = body as Record<string, unknown>;
  if (typeof record.instruction !== 'string' || utf8ByteLength(record.instruction) > MAX_VIDEO_INSTRUCTION_BYTES) return null;
  const duration = record.durationSeconds;
  if (typeof duration !== 'number' || !Number.isFinite(duration) || duration <= 0 || duration > MAX_VIDEO_DURATION_SECONDS) return null;
  if (!Array.isArray(record.frames) || record.frames.length === 0 || record.frames.length > MAX_VIDEO_FRAMES) return null;
  const frames: VideoFrame[] = [];
  let encoded = 0;
  for (let index = 0; index < record.frames.length; index += 1) {
    const frame = validateVideoFrame(record.frames[index], index);
    if (frame === null) return null;
    encoded += frame.data.length;
    if (encoded > MAX_VIDEO_FRAMES_BASE64) return null;
    frames.push(frame);
  }
  return {
    frames,
    instruction: record.instruction,
    durationSeconds: duration,
    mimeType: typeof record.mimeType === 'string' && record.mimeType !== '' ? record.mimeType : 'video/unknown',
    sizeBytes: typeof record.sizeBytes === 'number' && Number.isFinite(record.sizeBytes) ? record.sizeBytes : 0,
  };
}

/** Map a non-2xx upload response to an actionable bounded message: the
 *  backend's own `{"error": ...}` when present, else a status-specific
 *  fallback that tells the user what to do. */
function videoUploadError(status: number, body: { error?: unknown } | null): string {
  if (typeof body?.error === 'string' && body.error.trim() !== '') {
    return body.error.slice(0, MAX_UPLOAD_ERROR_CHARS);
  }
  switch (status) {
    case 400: return 'the video is not decodable';
    case 401: case 403: return 'authentication failed — check the runner token';
    case 411: return 'the upload was rejected: missing content length';
    case 413: return `the video is over the ${formatBytes(MAX_VIDEO_BYTES)} server upload limit`;
    case 415: return `unsupported video container (${VIDEO_LABEL})`;
    case 422: return `the video is longer than the ${MAX_VIDEO_DURATION_SECONDS} s limit`;
    case 429: return 'the runner is busy with other video uploads — try again shortly';
    case 503: return 'video analysis is unavailable: ffmpeg is not installed on the runner';
    case 504: return 'video processing timed out on the server';
    default: return `video upload failed (HTTP ${status})`;
  }
}

export function parseVideoUploadResponse(
  status: number,
  body: unknown,
): VideoUploadOutcome {
  if (status >= 200 && status < 300) {
    const data = validateVideoUploadData(body);
    if (data) return { ok: true, ...data };
    return { ok: false, message: 'the video upload endpoint returned an unexpected response' };
  }
  return { ok: false, message: videoUploadError(status, (body ?? {}) as { error?: unknown } | null) };
}

/** POST /upload/video: the raw video bytes (never base64 on the wire), the
 *  percent-encoded `X-Video-Name` header, bearer auth, and a bounded
 *  upload+probe+extract budget. The response body is parsed fail-closed. */
export async function uploadVideoAttachment(
  origin: string,
  token: string,
  name: string,
  bytes: Uint8Array,
  timeoutMs = VIDEO_UPLOAD_TIMEOUT_MS,
): Promise<VideoUploadOutcome> {
  const headers: Record<string, string> = {
    'Content-Type': 'application/octet-stream',
    'X-Video-Name': encodeURIComponent(name),
  };
  if (token !== '') headers.Authorization = `Bearer ${token}`;
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    const response = await fetch(`${origin}${VIDEO_UPLOAD_PATH}`, {
      method: 'POST',
      headers,
      body: bytes,
      signal: controller.signal,
    });
    const body = await response.json().catch(() => null);
    return parseVideoUploadResponse(response.status, body);
  } catch {
    if (controller.signal.aborted) return { ok: false, message: 'the upload timed out' };
    return { ok: false, message: 'the upload failed — check the connection and try again' };
  } finally {
    clearTimeout(timer);
  }
}

/* ---------------- send-to-terminal client (POST /upload/tmux-file) ---------------- */

/** The tmux session a composer attachment is sent to. The RN terminal screen
 *  publishes the session it attached to (the same contract the Web client keeps
 *  in sessionStorage), so the composer never guesses a destination. */
export interface TmuxFileDestination {
  runnerId: string;
  session: string;
  origin: string;
  token: string;
}

let tmuxDestination: TmuxFileDestination | null = null;

export function rememberTmuxFileDestination(destination: TmuxFileDestination | null): void {
  const usable = destination !== null
    && destination.session !== ''
    && destination.origin !== ''
    && !destination.origin.includes('#');
  tmuxDestination = usable ? { ...(destination as TmuxFileDestination) } : null;
}

export function tmuxFileDestination(): TmuxFileDestination | null {
  return tmuxDestination === null ? null : { ...tmuxDestination };
}

/** Only the two kinds whose bytes the app holds locally can be sent to a
 *  terminal: an image (its encoded block) or a code/text file (its text). */
export function canSendAttachmentToTmux(item: Attachment): boolean {
  return (item.kind === 'image' && item.base64 !== undefined)
    || (item.kind === 'code' && item.text !== undefined);
}

export async function sendAttachmentToTmux(
  item: Attachment,
  destination: TmuxFileDestination | null,
  timeoutMs = TMUX_FILE_UPLOAD_TIMEOUT_MS,
): Promise<{ ok: true; path: string } | { ok: false; message: string }> {
  if (destination === null) return { ok: false, message: 'Open a tmux terminal first' };
  let bytes: Uint8Array;
  let name = item.name;
  if (item.kind === 'image' && item.base64 !== undefined) {
    bytes = base64ToBytes(item.base64);
    const extension = IMAGE_UPLOAD_EXTENSIONS[item.mimeType];
    if (extension !== undefined) {
      const dot = item.name.lastIndexOf('.');
      const stem = dot > 0 ? item.name.slice(0, dot) : item.name;
      name = `${stem === '' ? 'file' : stem}.${extension}`;
    }
  } else if (item.kind === 'code' && item.text !== undefined) {
    bytes = new TextEncoder().encode(item.text);
  } else {
    return { ok: false, message: 'this attachment cannot be sent to a terminal' };
  }
  const headers: Record<string, string> = {
    'Content-Type': 'application/octet-stream',
    'X-File-Name': encodeURIComponent(name),
    'X-Tmux-Session': encodeURIComponent(destination.session),
  };
  if (destination.token !== '') headers.Authorization = `Bearer ${destination.token}`;
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    const response = await fetch(`${destination.origin}${TMUX_FILE_UPLOAD_PATH}`, {
      method: 'POST',
      headers,
      body: bytes,
      signal: controller.signal,
    });
    const body = await response.json().catch(() => null) as { path?: unknown; error?: unknown } | null;
    if (response.ok) {
      if (typeof body?.path === 'string' && body.path !== '') return { ok: true, path: body.path };
      return { ok: false, message: 'the tmux-file upload endpoint returned an unexpected response' };
    }
    if (typeof body?.error === 'string' && body.error.trim() !== '') {
      return { ok: false, message: body.error.slice(0, MAX_UPLOAD_ERROR_CHARS) };
    }
    return { ok: false, message: `tmux-file upload failed (HTTP ${response.status})` };
  } catch {
    if (controller.signal.aborted) return { ok: false, message: 'the tmux-file upload timed out' };
    return { ok: false, message: 'tmux-file upload failed — check the connection and try again' };
  } finally {
    clearTimeout(timer);
  }
}

/* ---------------- picker accept list ---------------- */

/** The file-picker `accept` value: the supported image MIME types, the video
 *  MIME types + container extensions the backend upload accepts, PDF, and the
 *  recognized text/code extensions — never a wildcard, which would advertise
 *  kinds the intake refuses. Derived from the classification tables the intake gates
 *  with, so the picker and the intake share one source of truth. */
export function attachmentAccept(): string {
  return [
    ...Object.keys(IMAGE_MIME_TYPES),
    ...Object.values(VIDEO_MIME_TYPES),
    ...Object.keys(VIDEO_CONTAINERS).map((ext) => `.${ext}`),
    'application/pdf',
    '.pdf',
    ...Object.keys(TEXT_EXTENSIONS).map((ext) => `.${ext}`),
  ].join(',');
}

/* ---------------- picker ---------------- */

export function attachmentPickerHtml(options: {
  background: string;
  foreground: string;
  border: string;
}): string {
  const config = JSON.stringify({
    caps: {
      code: MAX_CODE_BYTES,
      image: MAX_IMAGE_INPUT_BYTES,
      pdf: MAX_PDF_BYTES,
      video: MAX_VIDEO_BYTES,
    },
    maxImageOutputBytes: MAX_IMAGE_PROCESSED_BYTES,
    maxImageEdge: MAX_IMAGE_EDGE,
    maxImagePixels: MAX_IMAGE_PIXELS,
    jpegQualities: JPEG_QUALITIES,
    chunkChars: PICKER_CHUNK_CHARS,
    chunkBytes: PICKER_CHUNK_BYTES,
    textExtensions: Object.keys(TEXT_EXTENSIONS),
    textBasenames: Object.keys(TEXT_BASENAMES),
    textMimes: Object.keys(TEXT_APPLICATION_MIMES),
    imageMimes: Object.keys(IMAGE_MIME_TYPES),
    imageExtensions: IMAGE_EXTENSIONS,
    videoExtensions: Object.keys(VIDEO_CONTAINERS),
  });
  return `<!doctype html>
<html><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1, maximum-scale=1, user-scalable=no">
<style>
html,body{margin:0;height:100%;overflow:hidden}
body{display:flex;align-items:center;justify-content:center;background:${options.background};border:1px solid ${options.border};box-sizing:border-box}
span{font:600 13px system-ui,sans-serif;color:${options.foreground}}
#f{position:fixed;inset:0;width:100%;height:100%;opacity:0;font-size:100px}
</style></head>
<body>
<span>Attach</span>
<input id="f" type="file" multiple accept="${attachmentAccept()}">
<script>
var CFG = ${config};
function post(m){window.ReactNativeWebView.postMessage(JSON.stringify(m))}
function refuse(file,reason){post({type:'reject',name:file.name,size:file.size,reason:reason})}
function rawSize(b64){var pad=(b64.match(/=+$/)||[''])[0].length;return Math.floor(b64.length*3/4)-pad}
function base64(bytes){var s='';for(var i=0;i<bytes.length;i+=0x8000){s+=String.fromCharCode.apply(null,bytes.subarray(i,i+0x8000))}return btoa(s)}
function extension(file){var n=file.name.toLowerCase(),d=n.lastIndexOf('.');return d<0?'':n.slice(d+1)}
function isText(file){var n=file.name.toLowerCase(),b=n.split('/').pop();
return CFG.textBasenames.indexOf(b)>=0||CFG.textExtensions.indexOf(extension(file))>=0||file.type.indexOf('text/')===0||CFG.textMimes.indexOf(file.type)>=0}
function isImage(file){return CFG.imageMimes.indexOf(file.type)>=0||CFG.imageExtensions.indexOf(extension(file))>=0}
function isVideo(file){return CFG.videoExtensions.indexOf(extension(file))>=0}
function isPdf(file){return extension(file)==='pdf'}
function kindOf(file){if(isImage(file))return 'image';if(isVideo(file))return 'video';if(isPdf(file))return 'pdf';if(isText(file))return 'code';return null}
function sizeReason(kind){return kind==='image'?'image-oversize':kind==='video'?'video-oversize':kind==='pdf'?'pdf-oversize':'oversize'}
function send(name,mimeType,encoding,payload,size){var total=Math.ceil(payload.length/CFG.chunkChars);
post({type:'begin',name:name,mimeType:mimeType,size:size,encoding:encoding,total:total});
for(var i=0;i<payload.length;i+=CFG.chunkChars){post({type:'chunk',data:payload.slice(i,i+CFG.chunkChars)})}post({type:'end'})}
function sendBytes(name,mimeType,bytes){var total=Math.ceil(bytes.length/CFG.chunkBytes);
post({type:'begin',name:name,mimeType:mimeType,size:bytes.length,encoding:'bytes',total:total});
for(var i=0;i<bytes.length;i+=CFG.chunkBytes){post({type:'chunk',data:base64(bytes.subarray(i,i+CFG.chunkBytes))})}post({type:'end'})}
function jpegName(name){var d=name.lastIndexOf('.');return (d>0?name.slice(0,d):name)+'.jpg'}
function shrink(file,bytes,done){
var url=URL.createObjectURL(new Blob([bytes])),image=new Image();
image.onerror=function(){URL.revokeObjectURL(url);refuse(file,'unreadable');done()};
image.onload=function(){try{
URL.revokeObjectURL(url);
var width=image.naturalWidth||image.width,height=image.naturalHeight||image.height;
if(!(width>0)||!(height>0)||width*height>CFG.maxImagePixels){refuse(file,'image-processing');done();return}
var scale=Math.min(1,CFG.maxImageEdge/Math.max(width,height)),original=base64(bytes);
if(scale===1&&bytes.length<=CFG.maxImageOutputBytes){send(file.name,file.type||'','base64',original,file.size);done();return}
var canvas=document.createElement('canvas'),targetWidth=Math.max(1,Math.round(width*scale)),targetHeight=Math.max(1,Math.round(height*scale));
canvas.width=targetWidth;canvas.height=targetHeight;
var context=canvas.getContext('2d');
context.fillStyle='#ffffff';context.fillRect(0,0,targetWidth,targetHeight);
context.drawImage(image,0,0,targetWidth,targetHeight);
var data='';
for(var i=0;i<CFG.jpegQualities.length;i++){data=canvas.toDataURL('image/jpeg',CFG.jpegQualities[i]);
if(rawSize(data.slice(data.indexOf(',')+1))<=CFG.maxImageOutputBytes)break}
var encoded=data.slice(data.indexOf(',')+1),size=rawSize(encoded);
if(size>CFG.maxImageOutputBytes){refuse(file,'resized-oversize');done();return}
send(jpegName(file.name),'image/jpeg','base64',encoded,size);done()}catch(e){refuse(file,'unreadable');done()}};
image.src=url}
function handle(file,done){var kind=kindOf(file);
if(!kind){refuse(file,'unsupported');done();return}
if(file.size>CFG.caps[kind]){refuse(file,sizeReason(kind));done();return}
var reader=new FileReader();
reader.onerror=function(){refuse(file,'unreadable');done()};
reader.onload=function(){var bytes=new Uint8Array(reader.result);
if(kind==='image'){shrink(file,bytes,done);return}
if(kind==='code'){var text;try{text=new TextDecoder('utf-8',{fatal:true}).decode(bytes)}catch(e){refuse(file,'invalid-utf8');done();return}
send(file.name,file.type||'','text',text,file.size);done();return}
sendBytes(file.name,file.type||'',bytes);done()};
reader.readAsArrayBuffer(file)}
var queue=[];
function drain(){var file=queue.shift();
if(!file){post({type:'batch-end'});return}
var settled=false;handle(file,function(){if(settled)return;settled=true;setTimeout(drain,0)})}
document.getElementById('f').addEventListener('change',function(event){
queue=queue.concat(Array.prototype.slice.call(event.target.files||[]));event.target.value='';drain()});
</script>
</body></html>`;
}