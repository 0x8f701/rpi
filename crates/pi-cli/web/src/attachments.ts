// Pure (React-free) composer attachment intake helpers: classification,
// frame-derived limits, order-preserving read planning, UTF-8 validation, and
// the wire mapping. Shared by the three intake paths — textarea onPaste, footer
// drag/drop, and the hidden file input — so they enforce one consistent
// contract.
//
// TWO ATTACHMENT KINDS, both sent over the existing prompt command wire:
//  * image  -> pi_ai::ContentBlock::Image in the `images` array
//              (`{type:"image",data,mimeType}`, crates/pi-cli/src/modes/rpc.rs
//              `RpcCommand::Prompt`). Only ContentBlock::Image exists — there
//              is no PDF/binary variant, so non-image binaries cannot ride this
//              path as content.
//  * code   -> the file's UTF-8 text, sent as part of the prompt `message` as a
//              filename header + language-fenced code block. Reuses the
//              existing text wire (no new RPC field).
//
// REJECTED at intake with a visible, specific reason: PDFs (no content wire and
//  no Web-side text extraction — the old `[Attached PDF: name, size]` note was
//  misleading: it sent only a filename, not content), other binaries, and
//  files that fail UTF-8 validation.
//
// LIMITS are derived from the backend Web control-plane WebSocket frame cap
// (crates/pi-cli/src/modes/rpc.rs: `MAX_RPC_MESSAGE_BYTES = 4 MiB`): the entire
// `prompt` command JSON — message text (incl. code fences) + every image's
// base64 + the envelope — must fit one frame. We track a combined WIRE
// footprint: image base64 (ceil(raw/3)*4, ~4/3 inflation) + code text bytes, and
// cap it conservatively below the frame so the user's own typed text + envelope
// still fit. The backend still rejects an oversized frame with
// PAYLOAD_TOO_LARGE as the hard backstop.

import { nextId } from './transcript';

/** Mirrors `MAX_RPC_MESSAGE_BYTES` (crates/pi-cli/src/modes/rpc.rs). The whole
 *  prompt command frame must fit under this — the derivation anchor for the
 *  attachment wire budget below. */
export const MAX_PROMPT_FRAME_BYTES = 4 * 1024 * 1024;

/** Reserved for the JSON envelope + the user's own typed message text + per-
 *  file fence/filename overhead, so the wire budget never silently overruns
 *  the frame. ~1 MiB is generous for a prompt + envelope. */
const WIRE_RESERVE = 1 * 1024 * 1024;

/** Combined WIRE footprint cap for all queued attachments (image base64 + code
 *  text). Conservative under the 4 MiB frame (frame minus the reserve above). */
export const MAX_TOTAL_WIRE_BYTES = MAX_PROMPT_FRAME_BYTES - WIRE_RESERVE;

/** Per-file raw-byte cap. Applies to both image raw bytes and code text length:
 *  2 MiB raw image -> ~2.67 MiB base64 (fits the frame); 2 MiB of code text is
 *  already far beyond a sane single-file send. */
export const MAX_FILE_BYTES = 2 * 1024 * 1024;

/** Modest sanity cap on attachment count (the wire budget is the real bound;
 *  this keeps a pathological many-tiny-file batch off the wire). */
export const MAX_ATTACHMENTS = 8;

/** Max length of a sanitized filename in the `File:` header. */
const MAX_FILENAME_LEN = 128;

/** Longest fenced-code language hint produced by `codeLanguage` (e.g.
 *  `systemverilog`). Bounds the language portion of the per-code-file framing. */
const MAX_LANG_LEN = 14;

/** Per-code-file wire overhead allowance for the intake (pre-read) estimate:
 *  the `File: ` prefix (6) + the sanitized filename (up to MAX_FILENAME_LEN
 *  UTF-16 code units; each unit encodes to at most 3 UTF-8 bytes — lone
 *  surrogates and 3-byte BMP chars — so 3 * MAX_FILENAME_LEN never
 *  undercounts a multibyte name) + the 3 framing newlines + the opening and
 *  closing fences (min 3 each) + the language hint (up to MAX_LANG_LEN).
 *  This bounds the real non-text framing of a built code segment so the
 *  pre-read estimate never undercounts it for a normal (min-fence) file; a
 *  pathological all-backticks body grows the fence beyond this and is caught
 *  by the post-read exact aggregateWire plus the backend PAYLOAD_TOO_LARGE
 *  backstop. */
const CODE_WIRE_OVERHEAD = 6 + 3 * MAX_FILENAME_LEN + 3 + 2 * 3 + MAX_LANG_LEN;

export type AttachmentKind = 'image' | 'code';

/** A file attached in the composer.
 *  - image: `dataBase64` (raw base64, no `data:` prefix) + `previewUrl` (full
 *    data URL for the chip thumbnail); rides the prompt `images` array.
 *  - code:  `text` (validated UTF-8) + `language` (fence language hint); sent as
 *    a filename + fenced block inside the prompt `message`. */
export interface ComposerAttachment {
  id: string;
  name: string;
  size: number;
  mimeType: string;
  kind: AttachmentKind;
  dataBase64?: string;
  previewUrl?: string;
  text?: string;
  language?: string;
}

/** Image ContentBlock wire shape — mirrors pi_ai::ContentBlock::Image, which
 *  is internally tagged with camelCase keys: `{"type":"image","data":…,
 *  "mimeType":…}` (the `source`-nested Anthropic shape is NOT what the RPC
 *  parses). */
export interface ImageContentBlock {
  type: 'image';
  data: string;
  mimeType: string;
}

export type AttachSkipReason =
  | 'unsupported' // not an image or recognized text/code file (PDFs, binaries)
  | 'oversize' // exceeds MAX_FILE_BYTES
  | 'too-many' // exceeds MAX_ATTACHMENTS
  | 'over-budget' // would exceed MAX_TOTAL_WIRE_BYTES
  | 'invalid-utf8' // late: code file failed UTF-8 validation (binary garbage)
  | 'unreadable'; // late: FileReader error

export interface AttachSkip {
  name: string;
  size: number;
  type: string;
  reason: AttachSkipReason;
}

export interface AcceptedFile {
  file: File;
  kind: AttachmentKind;
}

export interface AttachPlan {
  /** Accepted files in their original intake (selection/drop/paste) order. */
  accepted: AcceptedFile[];
  /** Rejected files in their original intake order, each with a reason. */
  skipped: AttachSkip[];
}

export interface ClassifyOptions {
  maxFileBytes?: number;
  maxTotalWireBytes?: number;
  maxCount?: number;
  /** Already-queued attachment count (intake while composing). */
  currentCount?: number;
  /** Already-queued combined wire footprint (intake while composing). */
  currentWire?: number;
}

/** Outcome of reading one accepted file: either a built attachment or a late
 *  rejection (invalid UTF-8 / unreadable), reported with a reason. */
export interface ReadResult {
  attachment?: ComposerAttachment;
  skip?: AttachSkip;
}

/** base64 length of `rawBytes` raw bytes, per RFC 4648 (4/3, padded).
 *  Deterministic and allocation-free; used to estimate an image's wire footprint
 *  before it is read. */
export function base64Length(rawBytes: number): number {
  if (rawBytes <= 0) return 0;
  return Math.ceil(rawBytes / 3) * 4;
}

/** Estimated wire footprint of one attachment of `kind` with `size` raw bytes,
 *  used by the aggregate budget check at intake (before reading). */
export function wireFootprint(kind: AttachmentKind, size: number): number {
  return kind === 'image' ? base64Length(size) : size + CODE_WIRE_OVERHEAD;
}

/** Shared UTF-8 encoder for byte-length accounting (code text contributes UTF-8
 *  bytes to the prompt frame, not UTF-16 code units). */
const UTF8_ENC = new TextEncoder();

/** UTF-8 byte length of `s` (vs `s.length`, which counts UTF-16 code units and
 *  undercounts multibyte content). Pure + Node-testable. */
export function utf8ByteLength(s: string): number {
  return UTF8_ENC.encode(s).length;
}

/** Collapse a filename to one bounded, control-char-free line for the `File:`
 *  header so a malicious name (embedded newlines / control chars) cannot inject
 *  a second line or break the fenced wrapper. Pure + testable. */
export function sanitizeFileName(name: string): string {
  const oneLine = name.replace(/[\u0000-\u001F\u007F]+/g, ' ').trim();
  const bounded = oneLine.length > MAX_FILENAME_LEN ? oneLine.slice(0, MAX_FILENAME_LEN) : oneLine;
  return bounded || 'file';
}

/** Known code/text extensions (lowercase, no dot). Covers common languages and
 *  plain-text/doc formats; everything else without a `text/*` type is treated as
 *  a binary and rejected. PDF is intentionally absent. */
const TEXT_EXTENSIONS = new Set([
  'rs', 'ts', 'tsx', 'js', 'jsx', 'mjs', 'cjs', 'py', 'go', 'java', 'kt', 'kts',
  'c', 'cc', 'cpp', 'cxx', 'h', 'hh', 'hpp', 'hxx', 'cs', 'rb', 'php', 'swift',
  'scala', 'clj', 'cljs', 'ex', 'exs', 'erl', 'hs', 'ml', 'fs', 'fsx', 'lua',
  'pl', 'pm', 'r', 'dart', 'groovy', 'gradle', 'jl', 'nim', 'cr', 'zig', 'v',
  'sv', 'elm', 'purs', 'sql', 'sh', 'bash', 'zsh', 'fish', 'ps1', 'bat', 'cmd',
  'html', 'htm', 'css', 'scss', 'sass', 'less', 'xml', 'svg', 'vue', 'svelte',
  'astro', 'json', 'json5', 'jsonc', 'yaml', 'yml', 'toml', 'ini', 'cfg', 'conf',
  'env', 'properties', 'gitignore', 'editorconfig', 'lock', 'log', 'csv', 'tsv',
  'md', 'markdown', 'rst', 'adoc', 'tex', 'txt', 'dockerfile', 'makefile',
  'gitattributes', 'gradle.kts',
]);

/** Special file basenames (no extension) that are plain text. */
const TEXT_BASENAMES = new Set(['dockerfile', 'makefile', '.gitignore', '.editorconfig', '.npmrc', '.env']);

/** Map a filename to a fenced-code language hint (empty string = none). Mirrors
 *  common highlight.js language ids; unknown -> ''. */
export function codeLanguage(name: string): string {
  const lower = name.toLowerCase();
  const base = lower.split('/').pop() ?? lower;
  if (TEXT_BASENAMES.has(base)) {
    if (base === 'dockerfile') return 'dockerfile';
    if (base === 'makefile') return 'makefile';
    return '';
  }
  const ext = base.slice(base.lastIndexOf('.') + 1);
  switch (ext) {
    case 'rs': return 'rust';
    case 'ts': return 'typescript';
    case 'tsx': return 'tsx';
    case 'js': case 'mjs': case 'cjs': return 'javascript';
    case 'jsx': return 'jsx';
    case 'py': return 'python';
    case 'go': return 'go';
    case 'java': return 'java';
    case 'kt': case 'kts': return 'kotlin';
    case 'c': case 'h': return 'c';
    case 'cc': case 'cpp': case 'cxx': case 'hpp': case 'hxx': case 'hh': return 'cpp';
    case 'cs': return 'csharp';
    case 'rb': return 'ruby';
    case 'php': return 'php';
    case 'swift': return 'swift';
    case 'scala': return 'scala';
    case 'clj': case 'cljs': return 'clojure';
    case 'ex': case 'exs': return 'elixir';
    case 'erl': return 'erlang';
    case 'hs': return 'haskell';
    case 'ml': return 'ocaml';
    case 'fs': case 'fsx': return 'fsharp';
    case 'lua': return 'lua';
    case 'pl': case 'pm': return 'perl';
    case 'r': return 'r';
    case 'dart': return 'dart';
    case 'groovy': case 'gradle': return 'groovy';
    case 'jl': return 'julia';
    case 'nim': return 'nim';
    case 'cr': return 'crystal';
    case 'zig': return 'zig';
    case 'v': return 'verilog';
    case 'sv': return 'systemverilog';
    case 'elm': return 'elm';
    case 'purs': return 'purescript';
    case 'sql': return 'sql';
    case 'sh': case 'bash': case 'zsh': return 'bash';
    case 'fish': return 'fish';
    case 'ps1': return 'powershell';
    case 'bat': case 'cmd': return 'bat';
    case 'html': case 'htm': return 'html';
    case 'css': return 'css';
    case 'scss': return 'scss';
    case 'sass': return 'sass';
    case 'less': return 'less';
    case 'xml': case 'svg': return 'xml';
    case 'vue': return 'vue';
    case 'svelte': return 'svelte';
    case 'astro': return 'astro';
    case 'json': case 'json5': case 'jsonc': return 'json';
    case 'yaml': case 'yml': return 'yaml';
    case 'toml': return 'toml';
    case 'ini': case 'cfg': case 'conf': case 'env': case 'properties': return 'ini';
    case 'csv': return 'csv';
    case 'tsv': return 'tsv';
    case 'md': case 'markdown': return 'markdown';
    case 'rst': return 'rst';
    case 'adoc': return 'asciidoc';
    case 'tex': return 'latex';
    case 'txt': return 'text';
    case 'lock': return '';
    case 'log': return 'log';
    default: return '';
  }
}

function extOf(name: string): string {
  const lower = name.toLowerCase();
  const base = lower.split('/').pop() ?? lower;
  const dot = base.lastIndexOf('.');
  return dot >= 0 ? base.slice(dot + 1) : base;
}

/** Image MIME types the backend prompt command reliably carries
 *  (pi_ai::ContentBlock::Image). The backend does not normalize Web image
 *  blocks, so SVG/BMP (which can fail downstream) are rejected here. */
const ALLOWED_IMAGE_MIMES = new Set(['image/png', 'image/jpeg', 'image/gif', 'image/webp']);
const ALLOWED_IMAGE_EXT = /\.(png|jpe?g|gif|webp)$/i;

/** Best-effort image MIME for a file, falling back from the browser-supplied
 *  type to an extension guess and finally `image/png`. Only the four supported
 *  types are produced. */
export function imageMimeType(file: { type: string; name: string }): string {
  if (ALLOWED_IMAGE_MIMES.has(file.type)) return file.type;
  const ext = extOf(file.name);
  if (ext === 'jpg' || ext === 'jpeg') return 'image/jpeg';
  if (ext === 'png' || ext === 'gif' || ext === 'webp') return `image/${ext}`;
  return 'image/png';
}

/** True if `file` is an image the backend prompt command can carry. Restricted
 *  to PNG/JPEG/GIF/WebP (the backend does not normalize other image kinds); the
 *  extension fallback covers pasted/dropped screenshots whose `type` is empty. */
export function isImageFile(file: { type: string; name: string }): boolean {
  if (ALLOWED_IMAGE_MIMES.has(file.type)) return true;
  return ALLOWED_IMAGE_EXT.test(file.name);
}

/** True if `file` is a recognized UTF-8 text/code file (NOT an image). PDFs and
 *  unknown binaries are NOT text files. */
export function isTextFile(file: { type: string; name: string }): boolean {
  if (isImageFile(file)) return false;
  const lower = file.name.toLowerCase();
  const base = lower.split('/').pop() ?? lower;
  if (TEXT_BASENAMES.has(base)) return true;
  if (TEXT_EXTENSIONS.has(extOf(lower))) return true;
  if (file.type.startsWith('text/')) return true;
  // A few app/* types that are genuinely text:
  if (
    file.type === 'application/json' ||
    file.type === 'application/xml' ||
    file.type === 'application/javascript' ||
    file.type === 'application/x-yaml' ||
    file.type === 'application/yaml' ||
    file.type === 'application/x-sh'
  ) {
    return true;
  }
  return false;
}

/** Classify the kind of a file (image / code / null=unsupported). */
export function classifyKind(file: { type: string; name: string }): AttachmentKind | null {
  if (isImageFile(file)) return 'image';
  if (isTextFile(file)) return 'code';
  return null;
}

/** Classify an ordered file list into accepted (read targets with kind) and
 *  skipped (with reasons), enforcing per-file size, count, and aggregate wire
 *  limits. Pure: no I/O, deterministic, fully testable. Intake order is
 *  preserved in both `accepted` and `skipped`. */
export function classifyAttachments(
  files: readonly { type: string; name: string; size: number }[],
  opts: ClassifyOptions = {},
): AttachPlan {
  const maxFileBytes = opts.maxFileBytes ?? MAX_FILE_BYTES;
  const maxTotalWireBytes = opts.maxTotalWireBytes ?? MAX_TOTAL_WIRE_BYTES;
  const maxCount = opts.maxCount ?? MAX_ATTACHMENTS;
  let count = opts.currentCount ?? 0;
  let budget = opts.currentWire ?? 0;

  const accepted: AcceptedFile[] = [];
  const skipped: AttachSkip[] = [];
  for (const file of files) {
    const kind = classifyKind(file);
    if (!kind) {
      skipped.push({ name: file.name, size: file.size, type: file.type, reason: 'unsupported' });
      continue;
    }
    if (file.size > maxFileBytes) {
      skipped.push({ name: file.name, size: file.size, type: file.type, reason: 'oversize' });
      continue;
    }
    if (count >= maxCount) {
      skipped.push({ name: file.name, size: file.size, type: file.type, reason: 'too-many' });
      continue;
    }
    const next = budget + wireFootprint(kind, file.size);
    if (next > maxTotalWireBytes) {
      skipped.push({ name: file.name, size: file.size, type: file.type, reason: 'over-budget' });
      continue;
    }
    budget = next;
    count += 1;
    accepted.push({ file: file as File, kind });
  }
  return { accepted, skipped };
}

/** Validate bytes as UTF-8. Returns `{ok,text}` for valid UTF-8, else
 *  `{ok:false}` (binary / invalid sequence). Pure + Node-testable via the
 *  platform TextDecoder with `fatal: true`. */
export function decodeUtf8OrReject(bytes: Uint8Array): { ok: true; text: string } | { ok: false } {
  try {
    return { ok: true, text: new TextDecoder('utf-8', { fatal: true }).decode(bytes) };
  } catch {
    return { ok: false };
  }
}

/** Choose a fenced-code fence that no backtick run inside `text` can close: the
 *  fence length is one more than the longest run of backticks in `text` (and at
 *  least 3). Guarantees a ``` inside file content can never break out of the
 *  fence and corrupt the prompt. Pure + testable. */
export function safeFence(text: string): string {
  let maxRun = 0;
  let run = 0;
  for (let i = 0; i < text.length; i++) {
    if (text.charCodeAt(i) === 96 /* backtick */) {
      run += 1;
      if (run > maxRun) maxRun = run;
    } else {
      run = 0;
    }
  }
  return '`'.repeat(Math.max(3, maxRun + 1));
}

/** Build the prompt `message` segment for one code attachment: a `File: <name>`
 *  header (filename sanitized to one bounded, control-free line so it cannot
 *  inject a second line or break the wrapper) followed by a safely-fenced code
 *  block with a language hint. Pure. */
export function buildCodeSegment(name: string, language: string, text: string): string {
  const fence = safeFence(text);
  return `File: ${sanitizeFileName(name)}\n${fence}${language}\n${text}\n${fence}`;
}

/** Build the full prompt `message` contribution of all code attachments, in
 *  queue order, separated by blank lines. Pure. */
export function buildCodeMessage(attachments: readonly ComposerAttachment[]): string {
  return attachments
    .filter((a) => a.kind === 'code' && a.text != null)
    .map((a) => buildCodeSegment(a.name, a.language ?? '', a.text as string))
    .join('\n\n');
}

/** Exact wire footprint (UTF-8 bytes) of one built code segment — the `File:`
 *  header + fenced block that `buildCodeSegment` produces. Used by
 *  `aggregateWire` so the post-read budget reflects the real sanitized
 *  filename, fence, and language bytes and never undercounts the built
 *  segment. Pure + Node-testable. */
export function codeSegmentWireBytes(name: string, language: string, text: string): number {
  return utf8ByteLength(buildCodeSegment(name, language, text));
}

/** Read `files` through `read`, returning results in INPUT order regardless of
 *  async completion order (`Promise.all` resolves to the input array's order
 *  by spec). The injected `read` keeps this Node-testable: the real component
 *  passes a FileReader-based reader; tests pass a fake with shuffled delays. */
export async function readAttachmentsInOrder<TFile, T>(
  files: readonly TFile[],
  read: (file: TFile, index: number) => Promise<T>,
): Promise<T[]> {
  return Promise.all(files.map((file, index) => read(file, index)));
}

/** Read one image file into a composer attachment via FileReader. Browser-only
 *  (FileReader); the pure ordering/classification logic is tested separately. */
export function buildImageAttachment(file: File): Promise<ReadResult> {
  return new Promise((resolve) => {
    const reader = new FileReader();
    reader.onload = () => {
      const dataUrl = typeof reader.result === 'string' ? reader.result : '';
      const comma = dataUrl.indexOf(',');
      const dataBase64 = comma >= 0 ? dataUrl.slice(comma + 1) : dataUrl;
      resolve({
        attachment: {
          id: nextId('a'),
          name: file.name,
          size: file.size,
          mimeType: imageMimeType(file),
          kind: 'image',
          dataBase64,
          previewUrl: dataUrl,
        },
      });
    };
    reader.onerror = () =>
      resolve({ skip: { name: file.name, size: file.size, type: file.type, reason: 'unreadable' } });
    reader.readAsDataURL(file);
  });
}

/** Read one code/text file: ArrayBuffer -> UTF-8 validate -> attachment. A
 *  binary/invalid-UTF-8 file is rejected as a late skip (not thrown), so a mixed
 *  batch still queues the valid files. Browser-only (FileReader); the UTF-8
 *  validation itself is tested via `decodeUtf8OrReject`. */
export function buildCodeAttachment(file: File): Promise<ReadResult> {
  return new Promise((resolve) => {
    const reader = new FileReader();
    reader.onload = () => {
      if (!(reader.result instanceof ArrayBuffer)) {
        resolve({ skip: { name: file.name, size: file.size, type: file.type, reason: 'unreadable' } });
        return;
      }
      const decoded = decodeUtf8OrReject(new Uint8Array(reader.result));
      if (!decoded.ok) {
        resolve({ skip: { name: file.name, size: file.size, type: file.type, reason: 'invalid-utf8' } });
        return;
      }
      resolve({
        attachment: {
          id: nextId('a'),
          name: file.name,
          size: file.size,
          mimeType: file.type || 'text/plain',
          kind: 'code',
          text: decoded.text,
          language: codeLanguage(file.name),
        },
      });
    };
    reader.onerror = () =>
      resolve({ skip: { name: file.name, size: file.size, type: file.type, reason: 'unreadable' } });
    reader.readAsArrayBuffer(file);
  });
}

/** Read one accepted file via the reader for its kind. */
export function readAccepted(entry: AcceptedFile): Promise<ReadResult> {
  return entry.kind === 'image' ? buildImageAttachment(entry.file) : buildCodeAttachment(entry.file);
}

/** Map queued IMAGE attachments to the prompt command's `images` ContentBlock
 *  array. Code attachments are NOT images and contribute via `buildCodeMessage`
 *  instead. */
export function attachmentsToImageBlocks(
  attachments: readonly ComposerAttachment[],
): ImageContentBlock[] {
  return attachments
    .filter((a) => a.kind === 'image' && a.dataBase64 != null)
    .map((a) => ({ type: 'image', data: a.dataBase64 as string, mimeType: a.mimeType }));
}

/** Combined wire footprint of queued attachments (for aggregate-limit checks
 *  during follow-on intake), using actual read data. For a built code
 *  attachment the EXACT built segment bytes are counted (via
 *  `codeSegmentWireBytes` — the real sanitized filename + fence + language),
 *  so the post-read budget never undercounts what actually goes on the wire;
 *  a not-yet-read code attachment falls back to the conservative `wireFootprint`
 *  estimate. Image base64 is the exact encoded length. */
export function aggregateWire(attachments: readonly ComposerAttachment[]): number {
  let total = 0;
  for (const a of attachments) {
    if (a.kind === 'image') {
      total += a.dataBase64?.length ?? base64Length(a.size);
    } else if (a.text != null) {
      total += codeSegmentWireBytes(a.name, a.language ?? '', a.text);
    } else {
      total += wireFootprint('code', a.size);
    }
  }
  return total;
}

/** Short chip badge label for a code attachment: the file EXTENSION uppercased
 *  (e.g. "RS", "TS"), or "TXT" for extension-less names. Shows the file's type
 *  at a glance and stays short enough for the compact chip badge. The fence
 *  language is derived separately (codeLanguage) and is not shown here. */
export function codeBadgeLabel(name: string): string {
  const lower = name.toLowerCase();
  const base = lower.split('/').pop() ?? lower;
  const dot = base.lastIndexOf('.');
  if (dot > 0) return base.slice(dot + 1).toUpperCase();
  return 'TXT';
}

/** Format the skipped list into a single visible toast summary, or null when
 *  nothing was skipped. Groups by reason (first-seen order) with a name sample
 *  so a mixed batch reports every skip category at once. */
export function formatSkipSummary(skipped: readonly AttachSkip[]): string | null {
  if (skipped.length === 0) return null;
  const labels: Record<AttachSkipReason, string> = {
    unsupported: 'unsupported (images or text/code files only)',
    oversize: `over ${MAX_FILE_BYTES} bytes`,
    'too-many': `over ${MAX_ATTACHMENTS} attachments`,
    'over-budget': 'exceeds total size limit',
    'invalid-utf8': 'not valid UTF-8 (binary)',
    unreadable: 'could not be read',
  };
  const order: AttachSkipReason[] = [];
  const groups = new Map<AttachSkipReason, string[]>();
  for (const s of skipped) {
    if (!groups.has(s.reason)) {
      groups.set(s.reason, []);
      order.push(s.reason);
    }
    groups.get(s.reason)!.push(s.name);
  }
  const parts = order.map((r) => {
    const names = groups.get(r)!;
    const sample =
      names.length <= 2 ? names.join(', ') : `${names.slice(0, 2).join(', ')} +${names.length - 2} more`;
    return `${names.length} ${labels[r]} (${sample})`;
  });
  return `Skipped ${skipped.length} file(s): ${parts.join('; ')}.`;
}

/** Reconcile the concurrent intake budget from the actual built attachments
 *  after a read completes (success, all-late-skip, or read failure). Replaces
 *  the pre-read reservations with the exact wire footprint of the files that
 *  actually queued, so a fully-invalid batch (every file a late skip) never
 *  leaves a sticky reserved budget that would block a follow-on intake. Pure
 *  + Node-testable. */
export function reconcileIntakeBudget(
  built: readonly ComposerAttachment[],
): { count: number; wire: number } {
  return { count: built.length, wire: aggregateWire(built) };
}

/** Remove the sent attachment ids from the queue, preserving any concurrent
 *  additions. Used on send SUCCESS only: a failed transport retains the chips
 *  and budget for retry. Filtering by id (not array equality) means a second
 *  intake arriving while the send is in flight is never deleted on the
 *  success clear. Pure + Node-testable. */
export function removeSentAttachments(
  prev: readonly ComposerAttachment[],
  sentIds: ReadonlySet<string>,
): ComposerAttachment[] {
  return prev.filter((a) => !sentIds.has(a.id));
}

/** Curated code/text extensions the file picker advertises — real file
 *  extensions only (basenames like `Dockerfile` that the accept attribute
 *  cannot match are excluded). Every entry is a recognized text/code kind so
 *  the picker never advertises an extension the intake would reject. */
const PICKER_CODE_EXTENSIONS = [
  'rs', 'ts', 'tsx', 'js', 'jsx', 'mjs', 'cjs', 'py', 'go', 'java', 'c', 'cc',
  'cpp', 'h', 'hpp', 'cs', 'rb', 'php', 'swift', 'scala', 'clj', 'ex', 'erl',
  'hs', 'ml', 'fs', 'lua', 'pl', 'r', 'dart', 'groovy', 'jl', 'nim', 'zig', 'v',
  'sv', 'elm', 'purs', 'sql', 'sh', 'bash', 'zsh', 'fish', 'ps1', 'bat', 'cmd',
  'html', 'htm', 'css', 'scss', 'sass', 'less', 'xml', 'vue', 'svelte', 'astro',
  'json', 'json5', 'jsonc', 'yaml', 'yml', 'toml', 'ini', 'cfg', 'conf', 'env',
  'properties', 'csv', 'tsv', 'md', 'markdown', 'rst', 'adoc', 'tex', 'txt',
  'log', 'svg',
];

/** The file-picker `accept` value: the supported image MIME types (PNG/JPEG/
 *  GIF/WebP — NOT `image/*`, which would also advertise SVG/BMP the backend
 *  cannot carry) plus the supported code/text extensions. Derived from the
 *  same classification sets so the picker and the intake contract share one
 *  source of truth. */
export function attachmentAccept(): string {
  return [...ALLOWED_IMAGE_MIMES, ...PICKER_CODE_EXTENSIONS.map((e) => `.${e}`)].join(',');
}