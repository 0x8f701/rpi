// Pure (React-free) composer attachment intake helpers: classification,
// frame-derived limits, order-preserving read planning, UTF-8 validation, the
// wire mapping, and the backend video upload client. Shared by the three
// intake paths — textarea onPaste, footer drag/drop, and the hidden file
// input — so they enforce one consistent contract.
//
// THREE ATTACHMENT KINDS, sent over the existing prompt command wire:
//  * image  -> pi_ai::ContentBlock::Image in the `images` array
//              (`{type:"image",data,mimeType}`, crates/pi-cli/src/modes/rpc.rs
//              `RpcCommand::Prompt`). Only ContentBlock::Image exists — there
//              is no PDF/binary variant, so non-image binaries cannot ride this
//              path as content.
//  * video  -> raw bytes NEVER ride the prompt frame. The file is POSTed to
//              the backend's authenticated `POST /upload/video` endpoint (raw
//              body + `X-Video-Name` header; crates/pi-cli/src/modes/
//              video_upload.rs + listen.rs), which validates the container,
//              probes duration, and deterministically extracts bounded
//              chronological JPEG frames via ffmpeg. The returned frames are
//              fed back through the EXISTING `images` array as plain image
//              ContentBlocks, and the backend-generated bounded instruction
//              text (per-frame `Frame at <timestamp>` order) is prepended to
//              the prompt `message` as the explicit chronological-video
//              marker. Backend processing failures (unsupported container,
//              not a video, ffmpeg absent, timeouts) surface as an
//              error-state chip with an actionable message — never a silently
//              dropped attachment.
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
// footprint: image base64 (ceil(raw/3)*4, ~4/3 inflation) + code text bytes +
// extracted video frame base64 (the backend bounds one video's frames to
// ~2.7 MiB base64; the settle-time and submit-time checks enforce the
// aggregate), and cap it conservatively below the frame so the user's own
// typed text + envelope still fit. The backend still rejects an oversized
// frame with PAYLOAD_TOO_LARGE as the hard backstop.

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

/* ---------------- video upload limits (mirror the backend) ---------------- */

/** Raw-byte cap for a video upload, mirroring the backend's 64 MiB 413 bound
 *  (crates/pi-cli/src/modes/video_upload.rs). Raw video bytes NEVER ride the
 *  prompt frame — only the extracted JPEG frames do — so this is an upload
 *  transport bound, not a wire-budget contribution. */
export const MAX_VIDEO_BYTES = 64 * 1024 * 1024;

/** Max video duration the backend accepts (422 beyond this). */
export const MAX_VIDEO_DURATION_SECONDS = 600;

/** Max frames the backend extracts per video. */
export const MAX_VIDEO_FRAMES = 6;

/** Per-frame raw JPEG cap the backend enforces. */
export const MAX_VIDEO_FRAME_BYTES = 384 * 1024;

/** Total raw JPEG cap across one video's frames. */
export const MAX_VIDEO_FRAMES_RAW_BYTES = 2 * 1024 * 1024;

/** Encoded (base64) cap for ONE frame: ceil(per-frame raw / 3) * 4. Mirrors
 *  the backend's per-frame JPEG bound; the upload-response validator rejects
 *  any frame whose base64 exceeds it. */
export const MAX_VIDEO_FRAME_BASE64 = base64Length(MAX_VIDEO_FRAME_BYTES);

/** Encoded (base64) cap across ALL frames of one video: the backend bounds
 *  the aggregate so the frames fit the 4 MiB prompt frame with headroom; the
 *  upload-response validator rejects a response whose frame base64 exceeds
 *  it. */
export const MAX_VIDEO_FRAMES_BASE64 = base64Length(MAX_VIDEO_FRAMES_RAW_BYTES);

/** Defensive cap on the backend-generated instruction text: the backend
 *  bounds it, but the Web side re-checks so a pathological response can never
 *  bloat the prompt frame. The upload-response validator REJECTS an
 *  over-cap instruction; a larger instruction on an already-built attachment
 *  is replaced by the bounded local fallback marker. */
export const MAX_VIDEO_INSTRUCTION_BYTES = 2048;

/** Defensive cap on the backend's error string (path-scrubbed there; bounded
 *  here again so a hostile body can never flood the chip/toast). */
const MAX_VIDEO_ERROR_CHARS = 300;

/** Authenticated backend upload endpoint (same listener as the WebSocket). */
export const VIDEO_UPLOAD_PATH = '/upload/video';

/** Bounded upload+probe+extract round-trip budget (backend probe 20 s +
 *  extract 60 s + transport slack). The fetch is aborted past this and
 *  surfaces an actionable timeout on the chip. */
export const VIDEO_UPLOAD_TIMEOUT_MS = 90_000;

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

export type AttachmentKind = 'image' | 'video' | 'code';

/** Video attachment lifecycle. `uploading` = the POST /upload/video round-trip
 *  (upload + container validation + ffmpeg probe + frame extraction) is in
 *  flight; `ready` = frames are available and the video can be submitted;
 *  `error` = the backend rejected or failed to process the video (the chip
 *  shows `videoError`; submit is blocked until the chip is removed). */
export type VideoAttachmentState = 'uploading' | 'ready' | 'error';

/** One extracted frame from the backend's video preprocessing response
 *  (`crates/pi-cli/src/modes/video_upload.rs`). `data` is raw base64 JPEG
 *  (no `data:` prefix) — the same payload shape as an image ContentBlock. */
export interface VideoUploadFrame {
  index: number;
  timestampSeconds: number;
  mimeType: string;
  width?: number;
  height?: number;
  sizeBytes?: number;
  data: string;
}

/** A file attached in the composer.
 *  - image: `dataBase64` (raw base64, no `data:` prefix) + `previewUrl` (full
 *    data URL for the chip thumbnail); rides the prompt `images` array.
 *  - video: raw bytes never ride the prompt frame; the backend returns
 *    `frames` (chronological JPEG base64) + `instruction` (bounded marker).
 *    The frames ride the prompt `images` array and the instruction prepends
 *    the prompt `message`. `previewUrl` is a data URL of the FIRST frame for
 *    the chip thumbnail. `videoState`/`videoError` drive the chip lifecycle.
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
  /** Video only: lifecycle state (see VideoAttachmentState). */
  videoState?: VideoAttachmentState;
  /** Video only: actionable, bounded, path-scrubbed failure reason. */
  videoError?: string;
  /** Video only: opaque backend attachment id (keying/dedupe only). */
  attachmentId?: string;
  /** Video only: detected container (mkv|mp4|webm|mov|avi|ogg). */
  container?: string;
  /** Video only: probed duration in seconds. */
  durationSeconds?: number;
  /** Video only: extracted chronological JPEG frames. */
  frames?: VideoUploadFrame[];
  /** Video only: bounded chronological-frame instruction from the backend. */
  instruction?: string;
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
  | 'unsupported' // not an image, video, or recognized text/code file (PDFs, binaries)
  | 'oversize' // image/code exceeds MAX_FILE_BYTES
  | 'video-oversize' // video exceeds MAX_VIDEO_BYTES (upload cap)
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
  maxVideoBytes?: number;
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
 *  used by the aggregate budget check at intake (before reading). A video
 *  reserves ZERO wire here: raw video bytes never ride the prompt frame, and
 *  the extracted frames' footprint is unknown until the backend returns them —
 *  it is added exactly when the upload settles (and the settle-time check
 *  enforces the aggregate cap then). */
export function wireFootprint(kind: AttachmentKind, size: number): number {
  if (kind === 'image') return base64Length(size);
  if (kind === 'video') return 0;
  return size + CODE_WIRE_OVERHEAD;
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

/** Video containers the backend upload endpoint accepts (mkv|mp4|webm|mov|avi|
 *  ogg; anything else is a 415 there). Matches
 *  crates/pi-cli/src/modes/video_upload.rs exactly so the picker never
 *  advertises a container the backend rejects. */
const VIDEO_CONTAINERS = new Set(['mkv', 'mp4', 'webm', 'mov', 'avi', 'ogg']);

/** Video MIME types advertised in the file picker. video/x-matroska is
 *  explicitly included (pi-web-access 0.22 omits MKV). */
const ALLOWED_VIDEO_MIMES = new Set([
  'video/x-matroska',
  'video/mp4',
  'video/webm',
  'video/quicktime',
  'video/x-msvideo',
  'video/ogg',
]);

/** Container of a video filename (lowercased extension), or null when the
 *  name does not carry a supported container extension. */
export function videoContainerOf(name: string): string | null {
  const lower = name.toLowerCase();
  const base = lower.split('/').pop() ?? lower;
  const dot = base.lastIndexOf('.');
  if (dot < 0) return null;
  const ext = base.slice(dot + 1);
  return VIDEO_CONTAINERS.has(ext) ? ext : null;
}

/** True if `file` is a video the backend upload endpoint can process. The
 *  supported container EXTENSION is required — the backend rejects every
 *  other container with 415, so a browser `video/*` MIME (video/mpeg,
 *  video/x-flv, ...) must never broaden intake past the six supported
 *  containers or the picker/drop would accept files the upload cannot. The
 *  extension rule also covers drag/drop and picker files whose `type` is
 *  empty or octet-stream. */
export function isVideoFile(file: { type: string; name: string }): boolean {
  return videoContainerOf(file.name) !== null;
}

/** Classify the kind of a file (image / video / code / null=unsupported).
 *  Image wins over video (no overlap); video wins over text so a video
 *  container name is never misread as code — the backend is the authority on
 *  actual content, and a non-video file named `*.mkv` fails there with an
 *  actionable error chip instead of a misleading code attachment. */
export function classifyKind(file: { type: string; name: string }): AttachmentKind | null {
  if (isImageFile(file)) return 'image';
  if (isVideoFile(file)) return 'video';
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
  const maxVideoBytes = opts.maxVideoBytes ?? MAX_VIDEO_BYTES;
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
    // Kind-specific raw-byte cap: the 2 MiB image/code bound vs the 64 MiB
    // video upload bound (raw video never rides the prompt wire).
    const sizeCap = kind === 'video' ? maxVideoBytes : maxFileBytes;
    if (file.size > sizeCap) {
      skipped.push({
        name: file.name,
        size: file.size,
        type: file.type,
        reason: kind === 'video' ? 'video-oversize' : 'oversize',
      });
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

/** Bounded marker for ONE ready video attachment: the backend-generated
 *  instruction when present (bounded server-side, re-capped here defensively),
 *  else a fixed-shape local fallback naming the file and the chronological
 *  frame contract. Empty when the video has no frames (defensive). The marker
 *  is explicit and structural so the model receives a clear "these are
 *  chronological video frames" instruction with every sampled frame. */
export function videoMarkerText(a: ComposerAttachment): string {
  const frames = a.frames?.length ?? 0;
  if (frames === 0) return '';
  const instruction = a.instruction?.trim() ?? '';
  if (instruction !== '' && utf8ByteLength(instruction) <= MAX_VIDEO_INSTRUCTION_BYTES) {
    return instruction;
  }
  const name = sanitizeFileName(a.name);
  // Per-video marker binds to ITS frames: name + frame count + each frame's
  // timestamp, so a multi-video/multi-image batch never maps markers to the
  // wrong frame sequence ambiguously.
  const times = (a.frames ?? [])
    .filter((f) => Number.isFinite(f.timestampSeconds))
    .map((f) => `${f.timestampSeconds.toFixed(2)}s`)
    .join(', ');
  return `[Attached video: ${name} — the following ${frames} chronological frame${frames === 1 ? '' : 's'} (at ${times}) extracted from the video; analyze them in order as a video sequence.]`;
}

/** Build the prompt `message` contribution of all READY video attachments, in
 *  queue order, separated by blank lines: one bounded chronological-frame
 *  marker per video. Uploading/error videos contribute nothing (submit blocks
 *  on them before this can matter). Pure. */
export function buildVideoMessage(attachments: readonly ComposerAttachment[]): string {
  return attachments
    .filter((a) => a.kind === 'video' && a.videoState === 'ready')
    .map(videoMarkerText)
    .filter((m) => m !== '')
    .join('\n\n');
}

/** Short chip meta line for a ready video: "6 frames · 12.3 s" (or just the
 *  frame count when the duration is unknown). Pure + testable. */
export function videoMetaLabel(a: ComposerAttachment): string {
  const frames = a.frames?.length ?? 0;
  const dur = a.durationSeconds;
  if (frames > 0 && dur != null && Number.isFinite(dur)) {
    return `${frames} frame${frames === 1 ? '' : 's'} · ${dur.toFixed(1)} s`;
  }
  if (frames > 0) return `${frames} frame${frames === 1 ? '' : 's'}`;
  return 'video';
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

/* ---------------- video upload client (POST /upload/video) ---------------- */

/** Structural subset of `fetch`'s Response the upload client consumes —
 *  satisfied by the real Response and by Node-test fakes. */
export interface VideoUploadResponseLike {
  status: number;
  ok: boolean;
  json(): Promise<unknown>;
}

/** Structural subset of `fetch` the upload client needs. Injected so the
 *  Node tests can fake the transport (timeouts, non-JSON bodies, statuses). */
export type VideoFetch = (
  url: string,
  init: { method: string; headers: Record<string, string>; body: Blob; signal: AbortSignal },
) => Promise<VideoUploadResponseLike>;

export interface VideoUploadOptions {
  /** The listener authority the WebSocket uses (host[:port]); the upload goes
   *  to the SAME listener, matching the backend's /rpc token/auth scope. */
  hostAuthority: string;
  /** Bearer token for the listener; empty = tokenless same-origin (the
   *  backend accepts that exactly like /rpc). */
  token: string;
  /** Page protocol ('https:' -> https/wss scheme). Defaults to 'http:'. */
  pageProtocol?: string;
  /** Injected fetch (Node tests); defaults to the global fetch. */
  fetchImpl?: VideoFetch;
  /** Round-trip bound (aborts past this). Defaults to VIDEO_UPLOAD_TIMEOUT_MS. */
  timeoutMs?: number;
}

/** Success payload of the backend's video preprocessing endpoint
 *  (crates/pi-cli/src/modes/video_upload.rs). */
export interface VideoUploadData {
  attachmentId: string;
  name: string;
  container: string;
  mimeType: string;
  sizeBytes: number;
  durationSeconds: number;
  frameCount: number;
  framesBase64Bytes: number;
  frames: VideoUploadFrame[];
  instruction: string;
}

export type VideoUploadOutcome =
  | { ok: true; attachment: ComposerAttachment }
  | { ok: false; status: number; message: string };

/** Upload URL for the video endpoint on the listener authority, mirroring the
 *  WebSocket scheme derivation (`ws(s)://host/ws` -> `http(s)://host/upload/
 *  video`). Pure + testable. */
export function videoUploadUrl(pageProtocol: string, hostAuthority: string): string {
  const scheme = pageProtocol === 'https:' ? 'https://' : 'http://';
  return `${scheme}${hostAuthority}${VIDEO_UPLOAD_PATH}`;
}

/** Strictly validate ONE wire frame object into the typed VideoUploadFrame.
 *  Returns null on ANY violation — a hostile/malformed frame rejects the
 *  WHOLE response (never silently filtered): mimeType must be exactly
 *  image/jpeg (the backend emits JPEG only; a non-JPEG block would mislabel
 *  content for the vision path), index must be an integer in [0,
 *  MAX_VIDEO_FRAMES), timestamp must be finite and non-negative, and the
 *  base64 data must be non-empty and within the per-frame encoded cap (the
 *  memory/wire bound). */
function validateVideoUploadFrame(value: unknown): VideoUploadFrame | null {
  const record = (value ?? {}) as Record<string, unknown>;
  if (typeof record.data !== 'string' || record.data === '') return null;
  if (record.data.length > MAX_VIDEO_FRAME_BASE64) return null;
  if (record.mimeType !== 'image/jpeg') return null;
  if (
    typeof record.index !== 'number' ||
    !Number.isInteger(record.index) ||
    record.index < 0 ||
    record.index >= MAX_VIDEO_FRAMES
  ) {
    return null;
  }
  if (
    typeof record.timestampSeconds !== 'number' ||
    !Number.isFinite(record.timestampSeconds) ||
    record.timestampSeconds < 0
  ) {
    return null;
  }
  // Display metadata is validated, never silently accepted: width and height
  // must be POSITIVE integers when present (zero/fractional is malformed),
  // and sizeBytes a positive integer within the raw per-frame cap (the
  // decoded bound for the base64 payload). A present-but-invalid value
  // rejects the whole response — no silent filtering.
  let width: number | undefined;
  if ('width' in record) {
    if (typeof record.width !== 'number' || !Number.isInteger(record.width) || record.width <= 0) return null;
    width = record.width;
  }
  let height: number | undefined;
  if ('height' in record) {
    if (typeof record.height !== 'number' || !Number.isInteger(record.height) || record.height <= 0) return null;
    height = record.height;
  }
  let sizeBytes: number | undefined;
  if ('sizeBytes' in record) {
    if (
      typeof record.sizeBytes !== 'number' ||
      !Number.isInteger(record.sizeBytes) ||
      record.sizeBytes <= 0 ||
      record.sizeBytes > MAX_VIDEO_FRAME_BYTES
    ) {
      return null;
    }
    sizeBytes = record.sizeBytes;
  }
  return {
    index: record.index,
    timestampSeconds: record.timestampSeconds,
    mimeType: 'image/jpeg',
    width,
    height,
    sizeBytes,
    data: record.data,
  };
}

/** Strictly validate the whole 2xx upload body into typed VideoUploadData, or
 *  null when ANY part violates the contract (reject, never silently filter):
 *  attachmentId non-empty; bounded instruction; sane duration; 1..6 frames
 *  each strictly valid with UNIQUE CHRONOLOGICAL indices (index === position,
 *  which implies bounds + uniqueness); per-frame base64 within cap; aggregate
 *  base64 within the encoded total cap; and frameCount/framesBase64Bytes
 *  consistent with the frames when present. A compromised/malformed backend
 *  can therefore never blow memory/wire or smuggle non-JPEG blocks past the
 *  client. */
function validateVideoUploadData(record: Record<string, unknown>): VideoUploadData | null {
  if (typeof record.attachmentId !== 'string' || record.attachmentId === '') return null;
  if (
    typeof record.instruction !== 'string' ||
    utf8ByteLength(record.instruction) > MAX_VIDEO_INSTRUCTION_BYTES
  ) {
    return null;
  }
  // The backend rejects non-positive durations, so a success payload must
  // carry a strictly positive finite duration (bounded by the max).
  if (
    typeof record.durationSeconds !== 'number' ||
    !Number.isFinite(record.durationSeconds) ||
    record.durationSeconds <= 0 ||
    record.durationSeconds > MAX_VIDEO_DURATION_SECONDS
  ) {
    return null;
  }
  if (!Array.isArray(record.frames) || record.frames.length === 0 || record.frames.length > MAX_VIDEO_FRAMES) {
    return null;
  }
  const frames: VideoUploadFrame[] = [];
  let totalBase64 = 0;
  for (let i = 0; i < record.frames.length; i++) {
    const frame = validateVideoUploadFrame(record.frames[i]);
    // index === position enforces chronological order + uniqueness.
    if (frame === null || frame.index !== i) return null;
    totalBase64 += frame.data.length;
    if (totalBase64 > MAX_VIDEO_FRAMES_BASE64) return null;
    frames.push(frame);
  }
  if (typeof record.frameCount === 'number' && record.frameCount !== frames.length) return null;
  if (typeof record.framesBase64Bytes === 'number' && record.framesBase64Bytes !== totalBase64) return null;
  return {
    attachmentId: record.attachmentId,
    name: typeof record.name === 'string' ? record.name : '',
    container: typeof record.container === 'string' ? record.container : '',
    mimeType: typeof record.mimeType === 'string' ? record.mimeType : 'video/unknown',
    sizeBytes: typeof record.sizeBytes === 'number' ? record.sizeBytes : 0,
    durationSeconds: record.durationSeconds,
    frameCount: frames.length,
    framesBase64Bytes: totalBase64,
    frames,
    instruction: record.instruction,
  };
}

/** Map a non-2xx upload response to an actionable bounded message: the
 *  backend's own `{"error": "..."}` (path-scrubbed + bounded there, re-capped
 *  here defensively) when present, else a status-specific fallback that tells
 *  the user what to do (ffmpeg absent, too large, wrong container...). */
function errorMessageForStatus(status: number, body: unknown): string {
  const record = (body ?? {}) as { error?: unknown };
  if (typeof record.error === 'string' && record.error.trim() !== '') {
    return record.error.slice(0, MAX_VIDEO_ERROR_CHARS);
  }
  switch (status) {
    case 401:
    case 403:
      return 'authentication failed — check the token in Settings';
    case 411:
      return 'video upload rejected: missing content length';
    case 413:
      return `video too large (max ${Math.round(MAX_VIDEO_BYTES / (1024 * 1024))} MiB)`;
    case 415:
      return 'unsupported video container (mkv/mp4/webm/mov/avi/ogg)';
    case 422:
      return `video too long (max ${MAX_VIDEO_DURATION_SECONDS} s)`;
    case 400:
      return 'not a decodable video';
    case 503:
      return 'video analysis unavailable: ffmpeg is not installed on the server';
    case 504:
      return 'video processing timed out on the server';
    default:
      return `video upload failed (HTTP ${status})`;
  }
}

/** Parse an upload response into typed success data or a bounded error
 *  message. Pure + Node-testable (the App passes the real fetch Response; the
 *  tests pass fakes). */
export function parseVideoUploadResponse(
  status: number,
  body: unknown,
): { ok: true; data: VideoUploadData } | { ok: false; message: string } {
  if (status >= 200 && status < 300) {
    const data = validateVideoUploadData((body ?? {}) as Record<string, unknown>);
    if (data) return { ok: true, data };
    return { ok: false, message: 'unexpected response from the video upload endpoint' };
  }
  return { ok: false, message: errorMessageForStatus(status, body) };
}

/** Build the `ready` composer attachment from a successful upload response.
 *  The caller overrides `id` with the placeholder's id so the in-place chip
 *  patch finds it. The thumbnail is a data URL of the FIRST frame. */
export function buildVideoAttachment(file: File, data: VideoUploadData): ComposerAttachment {
  const frames = data.frames;
  return {
    id: '',
    name: data.name || file.name,
    size: data.sizeBytes || file.size,
    mimeType: data.mimeType,
    kind: 'video',
    videoState: 'ready',
    attachmentId: data.attachmentId,
    container: data.container,
    durationSeconds: data.durationSeconds,
    frames,
    instruction: data.instruction,
    // Thumbnail = data URL of the FIRST (t=0) frame; absent when the
    // (defensive) empty-frames case slips through.
    previewUrl:
      frames.length > 0 ? `data:${frames[0]!.mimeType};base64,${frames[0]!.data}` : undefined,
  };
}

/** Build the video-upload request headers: `X-Video-Name` carries the
 *  PERCENT-ENCODED UTF-8 filename (header values are WebIDL ByteStrings —
 *  a raw Unicode `file.name` would throw `TypeError: Value is not a valid
 *  ByteString` and surface as a bogus network failure; the backend
 *  percent-decodes the value, bounded, before sanitizing). `Authorization:
 *  Bearer` is added when the listener is tokened. Pure + Node-testable. */
export function videoUploadHeaders(name: string, token: string): Record<string, string> {
  const headers: Record<string, string> = { 'X-Video-Name': encodeURIComponent(name) };
  if (token !== '') headers.Authorization = `Bearer ${token}`;
  return headers;
}

/** POST a video file to the backend's authenticated /upload/video endpoint:
 *  raw bytes (never base64), `X-Video-Name` header, Bearer token when the
 *  listener has one, bounded by an abort timeout. Browser-only (fetch +
 *  File); the parsing/classification logic is tested separately with an
 *  injected fetch. */
export async function uploadVideoFile(file: File, opts: VideoUploadOptions): Promise<VideoUploadOutcome> {
  const timeoutMs = opts.timeoutMs ?? VIDEO_UPLOAD_TIMEOUT_MS;
  const fetchImpl = opts.fetchImpl ?? ((url: string, init: Parameters<VideoFetch>[1]) => fetch(url, init));
  const url = videoUploadUrl(opts.pageProtocol ?? 'http:', opts.hostAuthority);
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  const headers = videoUploadHeaders(file.name, opts.token);
  try {
    const response = await fetchImpl(url, {
      method: 'POST',
      headers,
      body: file,
      signal: controller.signal,
    });
    let body: unknown = null;
    try {
      body = await response.json();
    } catch {
      // Non-JSON error body (proxies, plain-text 5xx): the status fallback
      // below still yields an actionable message.
    }
    const parsed = parseVideoUploadResponse(response.status, body);
    if (!parsed.ok) return { ok: false, status: response.status, message: parsed.message };
    return { ok: true, attachment: buildVideoAttachment(file, parsed.data) };
  } catch {
    if (controller.signal.aborted) {
      return {
        ok: false,
        status: 0,
        message: 'video upload timed out — the server took too long to process it',
      };
    }
    // Transport/network failure: never surface the browser's raw error (it can
    // embed the host/URL); a bounded generic keeps the chip and toast safe.
    return { ok: false, status: 0, message: 'video upload failed — check the connection and try again' };
  } finally {
    clearTimeout(timer);
  }
}

/** Wire + count footprint of an accepted batch. Used to RESERVE the budget
 *  synchronously before the reads/upload (see App's CONCURRENCY INVARIANT)
 *  and to RELEASE it identically when the intake is discarded as stale: a
 *  discarded intake never pushes state, so the updater reconcile never runs —
 *  the symmetric release prevents a leaked reservation from blocking
 *  follow-on intake. Videos reserve 0 wire (raw bytes never ride the prompt
 *  frame). Pure + Node-testable. */
export function intakeReservation(accepted: readonly AcceptedFile[]): { count: number; wire: number } {
  let wire = 0;
  for (const a of accepted) {
    wire += wireFootprint(a.kind, a.file.size);
  }
  return { count: accepted.length, wire };
}

/** One admission/reservation unit: attachment count + wire footprint. */
export interface BudgetReservation {
  count: number;
  wire: number;
}

/** Concurrent attachment-budget tracker. The ADMISSION budget is the exact
 *  QUEUED footprint (recomputed from real state inside every setAttachments
 *  updater) PLUS the SUM of every OUTSTANDING intake reservation (registered
 *  synchronously before each intake's await). Overlapping intakes therefore
 *  can never over-admit: one intake's settle recomputes the queued exact but
 *  PRESERVES the other in-flight intakes' reservations (a naive reconcile
 *  from queued state alone would silently drop them and let a third intake
 *  bypass the front-end caps). Each intake reserves and later releases ONLY
 *  its own key. Pure + Node-testable (React-free; App wires it to a ref). */
export class IntakeBudgetTracker {
  private queued: BudgetReservation = { count: 0, wire: 0 };
  private readonly outstanding = new Map<string, BudgetReservation>();

  /** Budget a NEW intake is admitted against: queued exact + outstanding
   *  sum. */
  admission(): BudgetReservation {
    let count = this.queued.count;
    let wire = this.queued.wire;
    for (const reservation of this.outstanding.values()) {
      count += reservation.count;
      wire += reservation.wire;
    }
    return { count, wire };
  }

  /** Register an intake's reservation BEFORE its await. Idempotent per key
   *  (a re-reserve replaces). */
  reserve(intakeId: string, reservation: BudgetReservation): void {
    this.outstanding.set(intakeId, reservation);
  }

  /** Recompute the queued exact from real state. PRESERVES every outstanding
   *  reservation — call inside setAttachments updaters. */
  setQueued(queued: BudgetReservation): void {
    this.queued = queued;
  }

  /** Drop ONE intake's reservation (its settle or stale discard). */
  release(intakeId: string): void {
    this.outstanding.delete(intakeId);
  }

  /** Reset everything (composer reset: queued + all outstanding). */
  reset(): void {
    this.queued = { count: 0, wire: 0 };
    this.outstanding.clear();
  }
}

/** Chip placeholder for one accepted file, queued BEFORE its read/upload
 *  settles so chips appear immediately in intake order (videos show the
 *  `uploading` state; image/code placeholders flip to their real content as
 *  soon as the FileReader returns). The placeholder id is patched in place by
 *  the settle paths, preserving queue order. */
export function placeholderFor(entry: AcceptedFile, id: string): ComposerAttachment {
  if (entry.kind === 'video') {
    return {
      id,
      name: entry.file.name,
      size: entry.file.size,
      mimeType: entry.file.type || 'video/unknown',
      kind: 'video',
      videoState: 'uploading',
    };
  }
  if (entry.kind === 'image') {
    return { id, name: entry.file.name, size: entry.file.size, mimeType: imageMimeType(entry.file), kind: 'image' };
  }
  return { id, name: entry.file.name, size: entry.file.size, mimeType: entry.file.type || 'text/plain', kind: 'code' };
}

/** Merge per-task read results + video placeholders back into INTAKE order
 *  for the single state push: a built image/code read becomes the attachment
 *  (id overridden to the task id), a late skip is collected (never built),
 *  and a video becomes its uploading placeholder. A non-video task with NO
 *  read result (defensive: the reader returned neither) is dropped.
 *
 *  This is the invariant that makes submit's ready check sound: state only
 *  ever receives FULLY-BUILT image/code attachments and uploading VIDEO
 *  placeholders. A half-read image can never be submitted as a phantom — an
 *  in-queue image/code always carries its payload, and the only guarded
 *  in-flight kind is the video's explicit `videoState`. Pure +
 *  Node-testable (the delayed-reader submit regression lives here). */
export function mergeIntakeResults(
  tasks: readonly { id: string; entry: AcceptedFile }[],
  readById: ReadonlyMap<string, ReadResult>,
): { built: ComposerAttachment[]; skips: AttachSkip[] } {
  const built: ComposerAttachment[] = [];
  const skips: AttachSkip[] = [];
  for (const task of tasks) {
    const result = readById.get(task.id);
    if (result === undefined) {
      if (task.entry.kind === 'video') built.push(placeholderFor(task.entry, task.id));
      continue;
    }
    if (result.attachment) {
      built.push({ ...result.attachment, id: task.id });
    } else if (result.skip) {
      skips.push(result.skip);
    }
  }
  return { built, skips };
}

/** Pure settle of ONE video upload outcome against the CURRENT queue.
 *  `next` is the queue after the settle; `surface` tells the caller whether
 *  the failure must be toasted.
 *
 *  A STALE outcome — the chip was removed or the composer reset while the
 *  upload was in flight, so `id` is no longer queued — returns the queue
 *  UNCHANGED with `surface: false`: a late async result is silent, it never
 *  resurrects a removed chip and never toasts a phantom failure (the removal
 *  already released the budget). A live failure flips the chip to the
 *  actionable error state and surfaces the toast; a live success patches the
 *  ready attachment (frames + instruction + thumbnail) and, when the frames
 *  push the aggregate wire over the cap, flips to an error state AND drops
 *  the frames so the wire invariant is restored immediately. Pure +
 *  Node-testable. */
export function settleVideoOutcome(
  prev: readonly ComposerAttachment[],
  id: string,
  outcome: VideoUploadOutcome,
): { next: ComposerAttachment[]; surface: boolean } {
  if (!prev.some((a) => a.id === id)) {
    return { next: prev as ComposerAttachment[], surface: false };
  }
  if (outcome.ok) {
    let next = prev.map((a) => (a.id === id ? { ...outcome.attachment, id } : a));
    if (aggregateWire(next) > MAX_TOTAL_WIRE_BYTES) {
      next = next.map((a) =>
        a.id === id
          ? {
              ...a,
              frames: undefined,
              previewUrl: undefined,
              videoState: 'error' as const,
              videoError:
                'extracted video frames exceed the total send size limit — remove other attachments or this video',
            }
          : a,
      );
    }
    return { next, surface: false };
  }
  return {
    next: prev.map((a) =>
      a.id === id ? { ...a, videoState: 'error' as const, videoError: outcome.message } : a,
    ),
    surface: true,
  };
}

/** Exact wire footprint (base64 characters) of a video's extracted frames —
 *  the part of a video attachment that actually rides the prompt `images`
 *  array. Raw video bytes contribute nothing. */
export function videoFramesWire(frames: readonly VideoUploadFrame[] | undefined): number {
  let total = 0;
  for (const frame of frames ?? []) {
    total += frame.data.length;
  }
  return total;
}

/** Fixed maximum length of the `c<N>` command id sendCommand assigns
 *  (`c${++seqRef}`). Real ids are `c` + a small decimal counter — far shorter
 *  in any real session — but the pre-dispatch frame-byte check must NEVER
 *  undercount, and it cannot know the real id without predicting the counter.
 *  Using this fixed max-length stand-in makes the check an upper bound on the
 *  true frame bytes. */
export const MAX_COMMAND_ID = `c${'9'.repeat(23)}`;

/** Composer context an intake was captured under. `gen` is a monotonically
 *  increasing intake-generation counter bumped on every full composer reset
 *  (host switch); `host`/`token` are the listener authority the files were
 *  selected against; `session` the active session at selection time. */
export interface IntakeContext {
  gen: number;
  host: string;
  token: string;
  session: string | null;
}

/** True when an intake that STARTED under `captured` must be discarded
 *  because the composer context changed while its reads/upload were in
 *  flight (host switch bumps the generation; a session or authority change
 *  shows up in the captured values). Files selected on host A must never be
 *  pushed into host B's composer or uploaded to B's listener — a stale
 *  intake is dropped SILENTLY (no chips, no uploads, no toasts). Pure +
 *  Node-testable. */
export function isStaleIntake(captured: IntakeContext, current: IntakeContext): boolean {
  return (
    captured.gen !== current.gen ||
    captured.host !== current.host ||
    captured.token !== current.token ||
    captured.session !== current.session
  );
}

/** EXACT serialized bytes of a prompt command frame as sendCommand puts it on
 *  the wire: `JSON.stringify({ ...command, id, sessionId })` with the SAME
 *  key order and UTF-8 encoding — the byte-exact counterpart of
 *  crates/pi-cli/src/modes/rpc.rs `read_jsonl`'s `MAX_RPC_MESSAGE_BYTES`
 *  boundary (the backend rejects a frame at exactly the cap, LF not counted).
 *  JSON escaping (`"`, `\`, control chars) and multibyte UTF-8 are counted
 *  exactly as the transport serializes them. For the pre-dispatch cap check
 *  pass `MAX_COMMAND_ID` as `id` — a conservative upper bound that never
 *  depends on predicting the next command counter. Pure + Node-testable. */
export function commandFrameBytes(
  command: Record<string, unknown>,
  id: string,
  sessionId: string | null,
): number {
  const frame = sessionId ? { ...command, id, sessionId } : { ...command, id };
  return utf8ByteLength(JSON.stringify(frame));
}

/** Map queued attachments to the prompt command's `images` ContentBlock array:
 *  image attachments in queue order, then each READY video's extracted frames
 *  in chronological (index) order at the video's queue position. Code
 *  attachments are NOT images and contribute via `buildCodeMessage` instead.
 *  A video that is still uploading, errored, or frame-less contributes no
 *  blocks (submit blocks before this can matter). */
export function attachmentsToImageBlocks(
  attachments: readonly ComposerAttachment[],
): ImageContentBlock[] {
  const blocks: ImageContentBlock[] = [];
  for (const a of attachments) {
    if (a.kind === 'image') {
      if (a.dataBase64 != null) blocks.push({ type: 'image', data: a.dataBase64, mimeType: a.mimeType });
    } else if (a.kind === 'video' && a.videoState === 'ready') {
      for (const frame of a.frames ?? []) {
        blocks.push({ type: 'image', data: frame.data, mimeType: frame.mimeType });
      }
    }
  }
  return blocks;
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
    } else if (a.kind === 'video') {
      // Exact extracted-frame base64 once the backend has returned them;
      // zero before the upload settles (raw video never rides the wire).
      total += videoFramesWire(a.frames);
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
    unsupported: 'unsupported (images, videos, or text/code files only)',
    oversize: `over ${MAX_FILE_BYTES} bytes`,
    'video-oversize': `over ${Math.round(MAX_VIDEO_BYTES / (1024 * 1024))} MiB (video upload cap)`,
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

/** Video container extensions the file picker advertises — the exact set the
 *  backend upload endpoint accepts (415 otherwise), so the picker never
 *  advertises a container the backend rejects. */
const PICKER_VIDEO_EXTENSIONS = ['mkv', 'mp4', 'webm', 'mov', 'avi', 'ogg'];

/** The file-picker `accept` value: the supported image MIME types (PNG/JPEG/
 *  GIF/WebP — NOT `image/*`, which would also advertise SVG/BMP the backend
 *  cannot carry), the supported video MIME types + container extensions
 *  (video/x-matroska included — pi-web-access 0.22 omits MKV), and the
 *  supported code/text extensions. Derived from the same classification sets
 *  so the picker and the intake contract share one source of truth. */
export function attachmentAccept(): string {
  return [
    ...ALLOWED_IMAGE_MIMES,
    ...ALLOWED_VIDEO_MIMES,
    ...PICKER_VIDEO_EXTENSIONS.map((e) => `.${e}`),
    ...PICKER_CODE_EXTENSIONS.map((e) => `.${e}`),
  ].join(',');
}