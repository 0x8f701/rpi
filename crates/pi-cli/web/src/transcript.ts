// Pure (React-free) transcript normalization shared by the live event stream
// (App.tsx, CollabGuestView.tsx) and the restored message list
// (messagesToItems). Mirrors the TUI's content-visibility and output-bounding
// rules so live events and persisted/restored entries render identically:
//
//   * internal custom messages (`display: false`) never render — they carry
//     system reminders / orchestration scaffolding the TUI hides
//     (crates/pi-cli/src/tui.rs `push_message`: only `display: true` customs
//     surface, and typed IRC customs render their parsed view, not the XML);
//   * typed orchestration IRC customs (`display: true`) render as typed `irc`
//     items (direction/from/to/body/replyTo) driven by `details`, never the
//     raw `<orchestration-message>` XML wrapper (mirrors pi-coding
//     `orchestration_message_view` and the TUI's `orchestration_irc_view`);
//   * long command/tool output is bounded to its tail (failures surface at the
//     end), with a leading hint reporting the omitted line count — matching
//     the TUI compact tool-card fold (crates/pi-cli/src/tool_card_adapter.rs:
//     `BASH_CARD_OUTPUT_LIMIT = 10`, `DEFAULT_CARD_OUTPUT_LIMIT = 6`);
//   * Task, Edit, and Todo tools use structured default views (Goal/
//     Constraints/Contract + children; path/op + details.diff; compact
//     phase/task list) instead of dumping raw JSON args or the TUI summary
//     prose — web mirror of job_card_adapter / tool_card_adapter (reference
//     only; this file never changes TUI behavior).

import type { ContentBlock } from './types';

export type ToolCardStatus = 'running' | 'done' | 'error';

/** Live child row for a Task tool card (spawn ids + runtime status/activity). */
export interface TaskCardChild {
  name: string;
  agent: string;
  /** Complete self-contained briefing / target summary (bounded). */
  target: string;
  jobId?: string;
  agentId?: string;
  status: string;
  activity?: string;
  result?: string;
}

export interface ToolMedia {
  kind: 'image' | 'video';
  mimeType: string;
  data: string;
  alt: string;
}

/** One image payload on a user message: MIME + raw base64 (no `data:` prefix).
 *  Same wire shape as pi_ai::ContentBlock::Image minus the `type` tag, shared
 *  by the optimistic bubble (from composer attachments) and the restored
 *  message list (from user content blocks) so both render identically. */
export interface UserImage {
  mimeType: string;
  data: string;
}

/** Typed auto-vision analysis projected out of a user message: the model id
 *  that produced the description and the description itself. Extracted from
 *  the single text block the backend's vision-delegation path emits —
 *  `[Image analyzed by {model}: {description}]` (crates/pi-coding/src/
 *  session.rs `delegate_vision_images_with`) — so the description is NEVER
 *  rendered as the user's own text. It surfaces only as a clearly labeled,
 *  default-collapsed "Image analysis" card, and only when a real description
 *  was actually produced. */
export interface UserAnalysis {
  model: string;
  description: string;
}

export type Item =
  | { kind: 'user'; id: string; text: string; optimistic: boolean; images?: UserImage[]; analysis?: UserAnalysis }
  | { kind: 'assistant'; id: string; status: 'streaming' | 'final'; blocks: ContentBlock[] }
  | {
      kind: 'toolCard';
      id: string;
      toolCallId: string;
      toolName: string;
      args: unknown;
      status: ToolCardStatus;
      result: string;
      /** Structured AgentToolResult.details from live end or restored toolResult. */
      details?: unknown;
      media?: ToolMedia[];
    }
  | { kind: 'toolResult'; id: string; text: string }
  | { kind: 'bash'; id: string; command: string; output: string; status?: 'done' | 'error' }
  // Custom display:true backend messages (loops, projected notices).
  | { kind: 'custom'; id: string; label: string; text: string }
  // Typed orchestration IRC messages (child↔Main IRC, rendered as a typed
  // card by the shared IrcCard component — never the raw XML wrapper, the
  // customType label, or the plain `IRC · from → to` label).
  | {
      kind: 'irc';
      id: string;
      /** 'incoming' when addressed to Main (child → Main), else 'outgoing'. */
      direction: 'incoming' | 'outgoing';
      from: string;
      to: string;
      /** Body bounded to IRC_BODY_LINE_LIMIT lines at parse time. */
      body: string;
      /** Independent reply metadata (typed details only, never body-guessed). */
      replyTo?: string;
    }
  // branchSummary / compactionSummary backend messages (system notices).
  | { kind: 'summary'; id: string; label: string; text: string }
  | { kind: 'approval'; id: string; method: string; title: string; message: string; extensionId?: string };

export function nextId(prefix: string): string {
  return `${prefix}${Date.now().toString(36)}${Math.random().toString(36).slice(2, 8)}`;
}

export function contentText(content: unknown): string {
  // CustomMessageContent serializes either as a plain string (Text variant,
  // e.g. a loop scheduled turn's clean prompt after `public_message`) or as
  // an array of content blocks (Blocks variant). User/assistant/toolResult
  // content is always a block array; custom content may be either.
  if (typeof content === 'string') return content;
  if (!Array.isArray(content)) return '';
  return content
    .filter((b) => b && b.type === 'text' && typeof b.text === 'string')
    .map((b) => b.text as string)
    .join('\n');
}

const MAX_MEDIA_BASE64_CHARS = 3 * 1024 * 1024;
const BASE64_PAYLOAD = /^[A-Za-z0-9+/]+={0,2}$/;

function safeBase64(data: unknown): string | null {
  if (typeof data !== 'string' || data === '' || data.length > MAX_MEDIA_BASE64_CHARS) return null;
  return BASE64_PAYLOAD.test(data) && data.length % 4 === 0 ? data : null;
}

/** Validate one wire ContentBlock value as a renderable image: `type: "image"`
 *  with an allowlisted MIME (PNG/JPEG/GIF/WebP — the kinds the backend prompt
 *  command carries) and bounded base64 (3 MiB cap, strict charset). Shared by
 *  tool-result media and user-message images so both apply one contract. */
function imagePayload(value: unknown): UserImage | null {
  const record = (value || {}) as { type?: unknown; mimeType?: unknown; data?: unknown };
  if (record.type !== 'image') return null;
  const data = safeBase64(record.data);
  const mimeType = typeof record.mimeType === 'string' ? record.mimeType : '';
  if (!data || !/^image\/(png|jpeg|gif|webp)$/.test(mimeType)) return null;
  return { mimeType, data };
}

/** Extract the image blocks of a user message content array (same wire shape
 *  as the composer's prompt `images`), in block order, validated against the
 *  MIME allowlist + base64 budget. Non-image blocks are ignored; malformed
 *  blocks are skipped defensively — one bad block never breaks the restore.
 *  Low-level extractor kept for the focused tests; the user item's caption +
 *  images + analysis are projected together by `userMessageProjection` (used
 *  by both the live event stream and `messagesToItems`) so the three surfaces
 *  never diverge. */
export function userImages(content: unknown): UserImage[] {
  if (!Array.isArray(content)) return [];
  const images: UserImage[] = [];
  for (const block of content) {
    const payload = imagePayload(block);
    if (payload) images.push(payload);
  }
  return images;
}

/** The exact wire marker the backend's vision-delegation path emits when an
 *  active model lacks image support and `settings.visionModel` is configured:
 *  one text block `[Image analyzed by {model}: {description}]` replaces the
 *  image blocks in the MODEL context only (crates/pi-coding/src/session.rs
 *  `delegate_vision_images_with`). The durable history keeps the ORIGINAL
 *  images and must NOT carry this marker; when it does reach the client it is
 *  a legacy/old-binary or transport artifact, never the user's own text. This
 *  parser recognizes that ONE owned format structurally — the whole trimmed
 *  block must match — so a user who types a similar-looking line is never
 *  misclassified (their caption survives verbatim). */
const IMAGE_ANALYSIS_RE = /^\[Image analyzed by (?<model>.+?): (?<description>.+)\]$/s;

/** Parse one text block as the backend's vision-delegation marker, returning
 *  the typed `{ model, description }` projection or null when the block is not
 *  that exact shape. `s` flag lets a multi-line description match; the closing
 *  `]` anchor keeps a partial `[Image analyzed by …` (no closer) from matching. */
export function parseImageAnalysis(text: unknown): UserAnalysis | null {
  if (typeof text !== 'string') return null;
  const match = IMAGE_ANALYSIS_RE.exec(text.trim());
  if (!match || !match.groups) return null;
  const model = match.groups.model ?? '';
  const description = match.groups.description ?? '';
  if (model === '' || description.trim() === '') return null;
  return { model, description };
}

/** A legacy attachment-transport wrapper block: a text block that is ENTIRELY a
 *  balanced `<attachment ...>...</attachment>` (or self-closing
 *  `<attachment .../>`) XML element. Old binaries wrapped image transport in
 *  this scaffolding; current code never emits it. Recognized structurally
 *  (balanced tags over the whole block) so a user who literally types
 *  `<attachment>` as their message is preserved — the strip only applies when
 *  the message also carries real image blocks, proving an attachment was
 *  actually transported. The wrapper is dropped entirely (its inner text is
 *  transport scaffolding, not the user's caption). */
const ATTACHMENT_WRAPPER_RE = /^<attachment(\s[^>]*)?>[\s\S]*?<\/attachment>\s*$/i;
const ATTACHMENT_SELF_CLOSE_RE = /^<attachment(\s[^>]*)?\/>\s*$/i;


/** Typed projection of a user message's content blocks into the three
 *  display surfaces — the user's REAL caption, validated image previews, and
 *  an optional auto-vision analysis — shared by the live event stream
 *  (App.tsx `onMessage`) and the restored message list (`messagesToItems`) so
 *  both render identically.
 *
 *  Caption policy (never a fragile body heuristic): walk text blocks in order
 *  and keep only the user's own text. A block matching the backend's exact
 *  `[Image analyzed by …]` marker becomes typed `analysis` (never caption). A
 *  block that is structurally a balanced `<attachment>` wrapper is transport
 *  scaffolding and is dropped from the caption — but ONLY when the message
 *  carries real image blocks, so a hostile user-typed `<attachment>` with no
 *  images stays as their literal text. Everything else is the user's caption,
 *  joined with `\n`. The first analysis marker wins (the backend emits one
 *  description for all images); later markers are ignored. */
export function userMessageProjection(content: unknown): {
  text: string;
  images: UserImage[];
  analysis?: UserAnalysis;
} {
  if (!Array.isArray(content)) return { text: '', images: [] };
  // First pass: collect validated images so the wrapper-strip decision is
  // based on whether the WHOLE message carried a real attachment transport,
  // not on whether an image block happened to precede the wrapper text.
  const images: UserImage[] = [];
  for (const block of content) {
    const payload = imagePayload(block);
    if (payload) images.push(payload);
  }
  // Second pass: classify text blocks in order into caption / analysis /
  // transport wrapper. The user's real caption is everything that is neither
  // the backend's analysis marker nor a structurally-provable attachment
  // wrapper (only stripped when images proved a transport occurred).
  const captionParts: string[] = [];
  let analysis: UserAnalysis | undefined;
  const hasImages = images.length > 0;
  for (const block of content) {
    if (imagePayload(block)) continue;
    const record = (block || {}) as { type?: unknown; text?: unknown };
    if (record.type !== 'text' || typeof record.text !== 'string') continue;
    const parsedAnalysis = parseImageAnalysis(record.text);
    if (parsedAnalysis) {
      // First marker wins (the backend emits one description for all images);
      // later markers are dropped, never kept as caption.
      if (!analysis) analysis = parsedAnalysis;
      continue;
    }
    if (hasImages && (ATTACHMENT_WRAPPER_RE.test(record.text) || ATTACHMENT_SELF_CLOSE_RE.test(record.text))) {
      // A structurally-provable <attachment> wrapper is transport scaffolding
      // (a reference marker / image transport envelope), never the user's
      // own text — drop it entirely. The user's real caption lives in the
      // other (non-wrapper) text blocks; preserving inner text would
      // re-introduce transport noise as the caption.
      continue;
    }
    captionParts.push(record.text);
  }
  const text = captionParts.join('\n');
  return { text, images, ...(analysis ? { analysis } : {}) };
}

export function toolMedia(content: unknown, details?: unknown): ToolMedia[] {
  const media: ToolMedia[] = [];
  if (Array.isArray(content)) {
    for (const block of content) {
      const payload = imagePayload(block);
      if (payload) media.push({ kind: 'image', ...payload, alt: 'Tool result image' });
    }
  }
  const detailMedia = (details as { media?: unknown } | null)?.media;
  if (!Array.isArray(detailMedia)) return media;
  for (const raw of detailMedia) {
    const value = (raw || {}) as { kind?: unknown; mimeType?: unknown; data?: unknown; name?: unknown; sizeBytes?: unknown };
    const data = safeBase64(value.data);
    const mimeType = typeof value.mimeType === 'string' ? value.mimeType : '';
    const sizeBytes = typeof value.sizeBytes === 'number' ? value.sizeBytes : -1;
    if (value.kind !== 'video' || !/^video\/(mp4|webm|ogg)$/.test(mimeType) || !data) continue;
    if (!Number.isSafeInteger(sizeBytes) || sizeBytes < 0 || sizeBytes > 2 * 1024 * 1024) continue;
    const name = typeof value.name === 'string' && value.name !== '' ? value.name : 'Tool result video';
    media.push({ kind: 'video', mimeType, data, alt: name });
  }
  return media;
}


// TUI compact tool-card fold limits (crates/pi-cli/src/tool_card_adapter.rs).
// Bash keeps the last 10 lines; every other tool keeps the last 6.
export const BASH_OUTPUT_LINE_LIMIT = 10;
export const TOOL_OUTPUT_LINE_LIMIT = 6;

/** Typed IRC content parse-time body bound — the typed IRC card AND the hub
 *  wait/send tool card share this fold: the expander reveals at most this
 *  many lines (the compact default clamps to IRC_COMPACT_LINE_LIMIT visual
 *  lines; the two bounds together cap the card's worst-case height). */
export const IRC_BODY_LINE_LIMIT = 40;
/** Compact default: the first 6 visual lines render, the rest behind an
 *  explicit expand toggle (mirrors the TUI compact tool-card fold). */
export const IRC_COMPACT_LINE_LIMIT = 6;

export type IrcDirection = 'incoming' | 'outgoing';

/** Direction from the session anchor ('Main'), mirroring the TUI's
 *  `OrchestrationIrcView::label` (crates/pi-cli/src/orchestration_message.rs):
 *  a message addressed to Main is incoming (child → Main); everything else —
 *  from Main or child→child transit — is outgoing. */
export function ircDirection(_from: string, to: string): IrcDirection {
  return to === 'Main' ? 'incoming' : 'outgoing';
}

/** Typed card title, mirroring the TUI label vocabulary: incoming shows the
 *  sender (`IRC ← Child`), outgoing shows the recipient (`IRC → Agent`),
 *  child→child transit shows both parties. Never renders the customType
 *  (`orchestration_message`), the raw XML wrapper, or the old flat
 *  `IRC · from → to` label. */
export function ircTitle(from: string, to: string): string {
  if (to === 'Main') return `IRC ← ${boundText(from, 60) || '?'}`;
  if (from === 'Main') return `IRC → ${boundText(to, 60) || '?'}`;
  return `IRC ${boundText(from, 60) || '?'} → ${boundText(to, 60) || '?'}`;
}

/** Markdown-aware head bound shared by the hub wait/send tool card and the
 *  typed IRC card. Keeps the LEADING lines plus an omitted-line hint — a
 *  message's leading content is the primary payload, and a tail cut (like
 *  boundOutput) would slice through Markdown lists/code fences and render
 *  broken blocks. The result is the absolute ceiling the card can ever
 *  display; the compact clamp and the expander both operate inside it. */
export function boundHubBody(body: string): BoundedOutput {
  const text = body.replace(/\r\n/g, '\n').replace(/\r/g, '\n').trim();
  if (text === '') return { text: '', omitted: 0, bounded: false };
  const lines = text.split('\n');
  if (lines.length <= IRC_BODY_LINE_LIMIT) return { text, omitted: 0, bounded: false };
  const omitted = lines.length - IRC_BODY_LINE_LIMIT;
  return {
    text: `${lines.slice(0, IRC_BODY_LINE_LIMIT).join('\n')}\n\u2026 ${omitted} more line${omitted === 1 ? '' : 's'}`,
    omitted,
    bounded: true,
  };
}

/** Bound a typed IRC body to its head plus an omitted-line hint — the same
 *  fold as the hub card (boundHubBody). */
export function boundIrcBody(body: string): string {
  return boundHubBody(body).text;
}

/** Bound prose for task context sections / child target summaries. */
export const TASK_SECTION_CHAR_LIMIT = 480;
export const TASK_TARGET_CHAR_LIMIT = 220;
export const TASK_ACTIVITY_CHAR_LIMIT = 140;
export const TASK_RESULT_CHAR_LIMIT = 240;
export const EDIT_DIFF_LINE_LIMIT = 80;
/** Per-task content budget for the Todo card (the TUI Todo DAG panel wraps
 *  task lines per width; the web keeps one generous bounded line). */
export const TODO_TASK_CHAR_LIMIT = 240;

export interface BoundedOutput {
  text: string;
  omitted: number;
  bounded: boolean;
}

/** Bound long command/tool output to its visible tail. The tail is kept so
 *  failures (which surface at the end) stay visible; a leading hint reports
 *  the omitted line count so nearby legitimate content is never silently
 *  dropped. Applied identically to live events and restored entries. */
export function boundOutput(output: string, lineLimit: number): BoundedOutput {
  if (!output) return { text: '', omitted: 0, bounded: false };
  const withoutTerminalNewline = output.replace(/\r?\n$/, '');
  const lines = withoutTerminalNewline.split(/\r?\n/);
  const normalized = lines.join('\n');
  if (lines.length <= lineLimit) return { text: normalized, omitted: 0, bounded: false };
  const omitted = lines.length - lineLimit;
  const tail = lines.slice(-lineLimit).join('\n');
  return {
    text: `\u2026 ${omitted} more line${omitted === 1 ? '' : 's'}\n${tail}`,
    omitted,
    bounded: true,
  };
}

export function applyToolSnapshot(
  items: Item[],
  toolCallId: string,
  output: string,
  status?: 'done' | 'error',
  details?: unknown,
  media?: ToolMedia[],
): Item[] {
  return items.map((item) => {
    if (item.kind !== 'toolCard' || item.toolCallId !== toolCallId) return item;
    const limit = item.toolName.toLowerCase() === 'bash' ? BASH_OUTPUT_LINE_LIMIT : TOOL_OUTPUT_LINE_LIMIT;
    return {
      ...item,
      ...(status ? { status } : {}),
      result: boundOutput(output, limit).text,
      ...(details !== undefined ? { details } : {}),
      ...(media && media.length > 0 ? { media } : {}),
    };
  });
}

/** Fold a live toolResult message into the transcript exactly like the host
 *  view (App.tsx onMessageStart toolResult suppression): when a matching
 *  toolCard exists the result renders inline on the card — details folded on
 *  when missing, never overwriting existing result/details — and NO separate
 *  toolResult row is appended; unmatched results stay readable as a bounded
 *  toolResult item. Shared with CollabGuestView so the guest transcript can
 *  never show one tool's output twice (host parity). `text` must already be
 *  bounded by the caller (TOOL_OUTPUT_LINE_LIMIT). */
export function applyToolResultToItems(
  items: Item[],
  toolCallId: string,
  text: string,
  details?: unknown,
  media?: ToolMedia[],
): Item[] {
  if (toolCallId === '') {
    return [...items, { kind: 'toolResult', id: nextId('r'), text }];
  }
  const hasCard = items.some((item) => item.kind === 'toolCard' && item.toolCallId === toolCallId);
  if (!hasCard) return [...items, { kind: 'toolResult', id: nextId('r'), text }];
  if (details === undefined && (!media || media.length === 0)) return items;
  return items.map((item) =>
    item.kind === 'toolCard' && item.toolCallId === toolCallId
      ? {
          ...item,
          details: item.details ?? details,
          result: item.result || text,
          ...(media && media.length > 0 ? { media: item.media?.length ? item.media : media } : {}),
        }
      : item,
  );
}

/** Identity of a user item for optimistic↔durable reconcile: the typed text
 *  plus every image payload (mimeType + base64). Text alone is NOT enough —
 *  two image-only sends share an empty text and must never collapse onto each
 *  other, while an image-bearing optimistic bubble must still dedup against
 *  its persisted twin (same base64 echo). */
function userItemIdentity(item: { text: string; images?: UserImage[] }): string {
  const images = item.images ?? [];
  let key = item.text;
  for (const image of images) {
    key += `\u0000${image.mimeType}\u0000${image.data}`;
  }
  return key;
}

export function mergeAuthoritativeItems(history: Item[], live: Item[], preserveInflight = true): Item[] {
  const historicalToolCallIds = new Set(
    history
      .filter((item): item is Extract<Item, { kind: 'toolCard' }> => item.kind === 'toolCard')
      .map((item) => item.toolCallId)
      .filter((toolCallId) => toolCallId !== ''),
  );
  const historicalUserCounts = new Map<string, number>();
  const liveDurableUserCounts = new Map<string, number>();
  for (const item of history) {
    if (item.kind === 'user') {
      const identity = userItemIdentity(item);
      historicalUserCounts.set(identity, (historicalUserCounts.get(identity) ?? 0) + 1);
    }
  }
  for (const item of live) {
    if (item.kind === 'user' && !item.optimistic) {
      const identity = userItemIdentity(item);
      liveDurableUserCounts.set(identity, (liveDurableUserCounts.get(identity) ?? 0) + 1);
    }
  }
  const durableOptimisticSurplus = new Map<string, number>();
  for (const [identity, count] of historicalUserCounts) {
    const surplus = count - (liveDurableUserCounts.get(identity) ?? 0);
    if (surplus > 0) durableOptimisticSurplus.set(identity, surplus);
  }
  const transient = live.filter((item) => {
    if (preserveInflight && item.kind === 'assistant' && item.status === 'streaming') return true;
    if (preserveInflight && item.kind === 'user' && item.optimistic) {
      const identity = userItemIdentity(item);
      const surplus = durableOptimisticSurplus.get(identity) ?? 0;
      if (surplus === 0) return true;
      durableOptimisticSurplus.set(identity, surplus - 1);
      return false;
    }
    if (item.kind !== 'toolCard') return false;
    if (item.toolCallId === '') return preserveInflight;
    return !historicalToolCallIds.has(item.toolCallId);
  });
  return transient.length === 0 ? history : [...history, ...transient];
}

export function shouldRestoreStreamingAssistant(messages: unknown): boolean {
  const list = Array.isArray(messages) ? messages : [];
  for (let index = list.length - 1; index >= 0; index -= 1) {
    const message = list[index];
    if (!message || typeof message !== 'object' || !('role' in message)) continue;
    if (message.role === 'user' || message.role === 'toolResult') return true;
    if (message.role === 'assistant') return false;
  }
  return false;
}

/** Count occurrences of the literal two-character sequence `\n` (backslash
 *  followed by n) — the escaped-newline artifact models sometimes emit in
 *  reasoning content. Skips the `n` of a match so `\n\n` counts twice. */
function countLiteralBackslashN(text: string): number {
  let count = 0;
  for (let i = 0; i < text.length - 1; i += 1) {
    if (text.charCodeAt(i) === 0x5c /* \ */ && text.charCodeAt(i + 1) === 0x6e /* n */) {
      count += 1;
      i += 1;
    }
  }
  return count;
}

/** Conservative escaped-newline detection for thinking text, shared by the
 *  live delta path (App.tsx `applyDeltaToNode` / StreamingAssistant catch-up)
 *  and the final markdown path (markdown.ts `renderBlocks`) so streaming and
 *  restored thinking bodies render identically.
 *
 *  A thinking body is normalized to real multiple lines ONLY when it has no
 *  real newline yet AND contains at least two literal `\n` separators — a
 *  single backslash-n (e.g. `print("a\nb")` in a code snippet) is never
 *  rewritten, and text that already renders multi-line is left untouched. */
export function shouldNormalizeThinkingNewlines(text: string): boolean {
  if (text === '') return false;
  if (text.indexOf('\n') !== -1) return false;
  return countLiteralBackslashN(text) >= 2;
}

/** Replace every literal `\n` with a real newline. Only used once a thinking
 *  stream has committed to normalized form (see shouldNormalizeThinkingNewlines),
 *  so a single backslash-n in code can never be rewritten by this alone. */
export function unescapeThinkingNewlines(text: string): string {
  return text.replace(/\\n/g, '\n');
}

/** Conservative thinking-text normalization: identity unless the WHOLE text
 *  qualifies (no real newlines + multiple literal `\n` separators), in which
 *  case every literal `\n` becomes a real newline. */
export function normalizeThinkingNewlines(text: string): string {
  return shouldNormalizeThinkingNewlines(text) ? unescapeThinkingNewlines(text) : text;
}

/** Finalize a live assistant when a run settles without `message_end`.
 * Streamed visible text is preserved (early-abort contract); final rendering
 * hides any persisted thinking blocks without deleting recorder content. */
export function finalizeStreamingAssistant(items: Item[], targetId: string, streamedText: string): Item[] {
  if (targetId === '') return items;
  return items.map((item) => {
    if (item.kind !== 'assistant' || item.id !== targetId || item.status !== 'streaming') return item;
    const blocks = item.blocks.length > 0
      ? item.blocks
      : streamedText !== ''
        ? [{ type: 'text', text: streamedText } as ContentBlock]
        : [];
    return { ...item, status: 'final', blocks };
  });
}

function asRecord(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null;
}

function stringField(value: unknown, ...keys: string[]): string {
  const record = asRecord(value);
  if (!record) return '';
  for (const key of keys) {
    const raw = record[key];
    if (typeof raw === 'string' && raw.trim() !== '') return raw;
  }
  return '';
}

/** Bound a free-form string to a character budget without mid-code-unit cuts. */
export function boundText(input: string, limit: number): string {
  const text = input.replace(/\r\n/g, '\n').replace(/\r/g, '\n').trim();
  if (text.length <= limit) return text;
  if (limit <= 1) return '\u2026';
  return `${text.slice(0, limit - 1).trimEnd()}\u2026`;
}

/** Robust ATX heading parse: optional indent ≤3, 1–6 hashes, required space,
 *  optional trailing hashes. Returns null when the line is not a heading. */
export function parseHeading(line: string): { level: number; title: string } | null {
  const match = /^( {0,3})(#{1,6})([ \t]+)(.*?)(?:[ \t]+#+[ \t]*)?$/.exec(line);
  if (!match) return null;
  const title = (match[4] || '').trim();
  if (title === '') return null;
  return { level: match[2].length, title };
}

export interface TaskContextSections {
  goal: string;
  constraints: string;
  contract: string;
}

export interface TaskCardView {
  sections: TaskContextSections;
  children: TaskCardChild[];
}

export interface EditCardView {
  path: string;
  operation: string;
  diff: string;
}

export type DiffLineKind = 'add' | 'del' | 'meta' | 'ctx';

export interface DiffLineView {
  kind: DiffLineKind;
  text: string;
}

function normalizeSectionKey(title: string): 'goal' | 'constraints' | 'contract' | null {
  const key = title.trim().toLowerCase().replace(/[^a-z0-9]+/g, ' ').trim();
  if (key === 'goal' || key === 'goals' || key.startsWith('goal ')) return 'goal';
  if (
    key === 'constraints'
    || key === 'constraint'
    || key === 'constraints preferences'
    || key === 'constraints and preferences'
    || key.startsWith('constraint')
  ) {
    return 'constraints';
  }
  if (key === 'contract' || key === 'acceptance' || key === 'acceptance criteria' || key.startsWith('contract')) {
    return 'contract';
  }
  return null;
}

/** Split a task `context` string into Goal / Constraints / Contract bodies.
 *  Unrecognized leading prose folds into Goal so a free-form briefing still
 *  surfaces; known headings are matched case-insensitively with ATX levels. */
export function parseTaskContextSections(context: string): TaskContextSections {
  const sections: TaskContextSections = { goal: '', constraints: '', contract: '' };
  const lines = (context || '').replace(/\r\n/g, '\n').replace(/\r/g, '\n').split('\n');
  let current: keyof TaskContextSections | null = null;
  const buckets: Record<keyof TaskContextSections, string[]> = {
    goal: [],
    constraints: [],
    contract: [],
  };
  const leading: string[] = [];
  for (const line of lines) {
    const heading = parseHeading(line);
    if (heading) {
      const key = normalizeSectionKey(heading.title);
      if (key) {
        current = key;
        continue;
      }
    }
    if (current) buckets[current].push(line);
    else leading.push(line);
  }
  if (leading.some((line) => line.trim() !== '')) {
    buckets.goal = [...leading, ...buckets.goal];
  }
  sections.goal = boundText(buckets.goal.join('\n'), TASK_SECTION_CHAR_LIMIT);
  sections.constraints = boundText(buckets.constraints.join('\n'), TASK_SECTION_CHAR_LIMIT);
  sections.contract = boundText(buckets.contract.join('\n'), TASK_SECTION_CHAR_LIMIT);
  return sections;
}

function taskChildFromArgs(item: unknown, index: number): TaskCardChild | null {
  const record = asRecord(item);
  if (!record) return null;
  const target = stringField(record, 'task', 'assignment', 'prompt').trim();
  if (target === '') return null;
  const name = stringField(record, 'name', 'id') || `child-${index + 1}`;
  const agent = stringField(record, 'agent') || 'task';
  return {
    name,
    agent,
    target: boundText(target, TASK_TARGET_CHAR_LIMIT),
    status: 'queued',
  };
}

/** Parse a Task tool args object into the default-view card model. Mirrors
 *  `task_delegation_request` (batch `tasks[]` + required context, or single
 *  `task`) so the web never dumps the raw JSON payload as the primary view. */
export function parseTaskCardArgs(args: unknown): TaskCardView | null {
  const record = asRecord(args);
  if (!record) return null;
  const tasks = Array.isArray(record.tasks) ? record.tasks : null;
  if (tasks) {
    const children = tasks
      .map((item, index) => taskChildFromArgs(item, index))
      .filter((child): child is TaskCardChild => child !== null);
    if (children.length === 0) return null;
    return {
      sections: parseTaskContextSections(stringField(record, 'context')),
      children,
    };
  }
  const single = taskChildFromArgs(record, 0);
  if (!single) return null;
  return {
    sections: parseTaskContextSections(stringField(record, 'context')),
    children: [single],
  };
}

function spawnList(details: unknown): unknown[] | null {
  if (Array.isArray(details)) return details;
  const record = asRecord(details);
  if (record && Array.isArray(record.spawns)) return record.spawns;
  return null;
}

/** Merge TaskSpawn[] details (tool end / restored toolResult) into children
 *  by index, retaining jobId/agentId and the spawn status. */
export function mergeTaskSpawnDetails(children: TaskCardChild[], details: unknown): TaskCardChild[] {
  const spawns = spawnList(details);
  if (!spawns || spawns.length === 0) return children;
  return children.map((child, index) => {
    const byIndex = asRecord(spawns[index]);
    const byName = asRecord(spawns.find((entry) => {
      const record = asRecord(entry);
      if (!record) return false;
      const agentId = stringField(record, 'agentId', 'agent_id');
      return agentId !== '' && (agentId === child.agentId || agentId === child.name);
    }));
    const spawn = byIndex || byName;
    if (!spawn) return child;
    const jobId = stringField(spawn, 'jobId', 'job_id') || child.jobId;
    const agentId = stringField(spawn, 'agentId', 'agent_id') || child.agentId || child.name;
    const agent = stringField(spawn, 'agent') || child.agent;
    const status = stringField(spawn, 'status') || child.status;
    return {
      ...child,
      jobId: jobId || undefined,
      agentId: agentId || undefined,
      agent: agent || child.agent,
      status: status || child.status,
    };
  });
}

function projectedChildren(details: unknown): TaskCardChild[] | null {
  const record = asRecord(details);
  if (!record || !Array.isArray(record.children)) return null;
  const children = record.children
    .map((entry, index): TaskCardChild | null => {
      const child = asRecord(entry);
      if (!child) return null;
      const target = stringField(child, 'target', 'task');
      if (target === '') return null;
      return {
        name: stringField(child, 'name') || `child-${index + 1}`,
        agent: stringField(child, 'agent') || 'task',
        target: boundText(target, TASK_TARGET_CHAR_LIMIT),
        jobId: stringField(child, 'jobId', 'job_id') || undefined,
        agentId: stringField(child, 'agentId', 'agent_id') || undefined,
        status: stringField(child, 'status') || 'queued',
        activity: stringField(child, 'activity') || undefined,
        result: stringField(child, 'result') || undefined,
      };
    })
    .filter((child): child is TaskCardChild => child !== null);
  return children.length > 0 ? children : null;
}

/** Resolve the Task card default view from args + optional details.
 *  Mirrors job_card_adapter: request children from tool start args, spawn
 *  ids/status from TaskSpawn[] end details, live rows from projected children. */
export function resolveTaskCardView(args: unknown, details?: unknown): TaskCardView | null {
  const base = parseTaskCardArgs(args);
  if (!base) return null;
  const withSpawns = mergeTaskSpawnDetails(base.children, details);
  const live = projectedChildren(details);
  if (!live) return { sections: base.sections, children: withSpawns };
  const byKey = new Map<string, TaskCardChild>();
  for (const child of withSpawns) {
    byKey.set(child.jobId || child.agentId || child.name, child);
  }
  const children = live.map((child) => {
    const prev = (child.jobId && byKey.get(child.jobId))
      || (child.agentId && byKey.get(child.agentId))
      || byKey.get(child.name);
    if (!prev) return child;
    return {
      ...prev,
      ...child,
      target: child.target || prev.target,
      activity: child.activity || prev.activity,
      result: child.result || prev.result,
    };
  });
  return { sections: base.sections, children };
}

/** Edit tool default view: path + operation + details.diff (not raw JSON). */
export function parseEditCard(args: unknown, details?: unknown, resultText = ''): EditCardView | null {
  const record = asRecord(args);
  if (!record) return null;
  const path = stringField(record, 'path', 'file');
  const operation = stringField(record, 'operation', 'op') || 'edit';
  const detailsRecord = asRecord(details);
  const diffRaw = detailsRecord && typeof detailsRecord.diff === 'string'
    ? detailsRecord.diff
    : '';
  const diff = boundOutput(diffRaw || resultText, EDIT_DIFF_LINE_LIMIT).text;
  return { path: path || '(unknown path)', operation, diff };
}

/** Compact one-line tool-arguments summary, mirroring the TUI's
 *  `compact_tool_arguments` (crates/pi-cli/src/human_event_renderer.rs):
 *  extracts the first of `command`/`path`/`pattern` as a string and bounds
 *  it to 60 chars. Never dumps raw JSON. */
export function compactToolArgs(args: unknown): string {
  const record = asRecord(args);
  if (!record) return '';
  for (const key of ['command', 'path', 'pattern']) {
    const value = record[key];
    if (typeof value === 'string' && value !== '') {
      return value.length <= 60 ? value : `${value.slice(0, 57)}...`;
    }
  }
  return '';
}

/** Bash / command tool card view: extracts the `command` string from args.
 *  Returns null when no command is present (the card falls back to generic). */
export function parseCommandCardArgs(args: unknown): { command: string } | null {
  const record = asRecord(args);
  if (!record) return null;
  const command = typeof record.command === 'string' ? record.command : '';
  return command === '' ? null : { command };
}

/** Process tool card view: extracts a human-readable label from the `op` +
 *  `argv` / `id` / `label` args. `start` joins argv into a command line;
 *  other ops surface as `process {op} {id}`. When no `op` is present but
 *  `argv` or `label` exists (some process tools carry a bare argv/label
 *  without an op wrapper), the argv is joined or the label is used directly.
 *  Never dumps the raw args JSON. */
export function parseProcessCardArgs(args: unknown): { label: string } | null {
  const record = asRecord(args);
  if (!record) return null;
  const op = typeof record.op === 'string' ? record.op : '';
  if (op === 'start' || (op === '' && Array.isArray(record.argv))) {
    const argv = Array.isArray(record.argv) ? record.argv : null;
    if (argv && argv.length > 0) {
      const parts = argv.filter((v): v is string => typeof v === 'string' && v !== '');
      if (parts.length > 0) return { label: parts.join(' ') };
    }
    if (op === 'start') return { label: 'process start' };
  }
  if (op !== '') {
    const id = typeof record.id === 'string' ? record.id : '';
    return { label: id !== '' ? `process ${op} ${id}` : `process ${op}` };
  }
  const label = typeof record.label === 'string' ? record.label : '';
  if (label !== '') return { label };
  return null;
}

/** Write tool card view: path from args + a success/error summary derived
 *  from the result text. Never shows the raw `content` JSON. */
export function parseWriteCardArgs(
  args: unknown,
  result: string,
  status: ToolCardStatus,
): { path: string; summary: string } | null {
  const record = asRecord(args);
  if (!record) return null;
  const path = stringField(record, 'path', 'file');
  if (path === '') return null;
  let summary: string;
  if (status === 'error') {
    summary = result !== '' ? result : 'write failed';
  } else if (status === 'done') {
    summary = result !== '' ? result : 'wrote successfully';
  } else {
    summary = 'writing\u2026';
  }
  return { path, summary: boundText(summary, 120) };
}

/** Read tool card view: path from args. The result (bounded file content)
 *  renders as the output body. Never shows the raw `path` args JSON. */
export function parseReadCardArgs(args: unknown): { path: string } | null {
  const record = asRecord(args);
  if (!record) return null;
  const path = stringField(record, 'path', 'file');
  return path === '' ? null : { path };
}

/** Hub tool card default view. The `hub` tool's raw args carry an op
 *  envelope with internal ids / timeouts (`{ids, op, timeoutMs}`) that must
 *  never render as a raw JSON dump. A running `hub wait` shows a fixed human
 *  title ('Waiting') plus clear waiting feedback; settled waits with a typed
 *  mailbox projection (`details.message` / `details.reply` — the same
 *  id/from/to/body/replyTo vocabulary the TUI's
 *  `orchestration_irc_view_from_json` reads) render the typed incoming IRC
 *  summary; timeouts and job-waits fall back to concise prose. Send cards
 *  keep the outgoing message + delivery outcome (no regression) and render a
 *  typed await-reply frame when one arrived. Status borders stay intact
 *  (running/error/done) so the card never looks stuck.
 *
 * Body/note content is bounded head-first via boundHubBody and rendered as
 *  SAFE Markdown (the shared renderMarkdown pipeline — escapeHtml-first,
 *  whitelisted links/images, code fences, Mermaid hydration) by the hub tool
 *  card, never as raw text: Markdown bullets/inline code/path lists display
 *  structurally, hostile HTML stays literal, and the compact default clamps
 *  to IRC_COMPACT_LINE_LIMIT visual lines behind an expand toggle up to the
 *  IRC_BODY_LINE_LIMIT parse bound (the same fold as the typed IRC card). */
export interface HubCardView {
  /** The hub op ('wait' | 'send' | …); '' when the args carry no op. */
  op: string;
  /** Fixed human title: 'Waiting' for a wait in progress or an unfulfilled
   *  wait, 'IRC' for a settled typed message/reply, else 'Hub'. */
  title: string;
  /** Headline line — running-wait copy, IRC direction, or the op label. */
  headline: string;
  /** Typed IRC body (incoming message / outgoing send / await reply),
   *  bounded head-first to IRC_BODY_LINE_LIMIT lines; rendered as Markdown
   *  with a compact fold + expand toggle (never a raw dump). */
  body: string;
  /** Muted reply metadata ('reply to <id>') when the projection carries it. */
  metadata?: string;
  /** Bounded fallback prose (timeout copy, send outcome, other-op result),
   *  same Markdown fold as the body. */
  note: string;
  /** True when a typed details.message/details.reply drove this view. */
  typed: boolean;
  /** Typed await-reply frame rendered below an outgoing send frame. */
  reply?: { headline: string; body: string; metadata?: string };
}

/** Internal hub ids are opaque UUIDs that must never surface in the default
 *  view; only short readable agent names (harness roster ids) are shown. */
const UUID_LIKE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

function readableAgentId(value: string): string {
  const trimmed = value.trim();
  if (trimmed === '' || UUID_LIKE.test(trimmed)) return '';
  return trimmed;
}

/** Typed mailbox projection carried by hub wait/send tool details
 *  (`details.message` / `details.reply`) and by orchestration custom-message
 *  details. Shared by the hub tool-result card (parseHubCard) and the typed
 *  orchestration IRC rows so both parse identically. Mirrors the TUI's
 *  `orchestration_irc_view_from_json`: requires a non-empty `id` + `from`
 *  and reads body/replyTo directly — never parsed from model-facing prose,
 *  so a control-prose result can never leak into the typed view. Returns
 *  null for null (wait timeout), missing, or malformed projections. */
export function ircProjection(value: unknown): { from: string; to: string; body: string; replyTo?: string } | null {
  const record = asRecord(value);
  if (!record) return null;
  const id = stringField(record, 'id');
  const from = stringField(record, 'from');
  if (id === '' || from === '') return null;
  const replyTo = stringField(record, 'replyTo');
  return {
    from,
    to: stringField(record, 'to'),
    body: stringField(record, 'body'),
    ...(replyTo !== '' ? { replyTo } : {}),
  };
}

/** Delivery outcome label from `details.receipts`, mirroring the TUI's
 *  `hub_send_outcome_label`: queued / injected (woken) / revived / failed /
 *  partial; unknown outcomes fail closed as delivered. */
function hubSendOutcome(details: unknown): string {
  const record = asRecord(details);
  const receipts = record && Array.isArray(record.receipts) ? record.receipts : null;
  if (!receipts || receipts.length === 0) return '';
  let anyFailed = false;
  let anySuccess = false;
  let unanimous: string | null = null;
  for (const raw of receipts) {
    const receipt = asRecord(raw);
    const outcome = receipt ? stringField(receipt, 'outcome') : '';
    const label = outcome === 'queued' ? 'queued'
      : outcome === 'woken' ? 'injected'
      : outcome === 'revived' ? 'revived'
      : outcome === 'failed' ? 'failed'
      : 'delivered';
    if (label === 'failed') anyFailed = true;
    else anySuccess = true;
    unanimous = unanimous === null ? label : unanimous === label ? unanimous : '';
  }
  if (anyFailed && anySuccess) return 'partial';
  if (anyFailed) return 'failed';
  if (unanimous !== null && unanimous !== '') return unanimous;
  return 'delivered';
}

/** Hub tool card default view: never the raw `{ids, op, timeoutMs}` args
 *  envelope. Wait cards keep a fixed human title ('Waiting' / 'IRC') with
 *  clear waiting feedback; typed mailbox projections render the incoming IRC
 *  summary; every other op falls back to the op label + bounded result text.
 *  Bodies/notes are bounded head-first (boundHubBody) and rendered as safe
 *  Markdown by the view, never raw. */
export function parseHubCard(
  args: unknown,
  details?: unknown,
  result = '',
  status: ToolCardStatus = 'running',
): HubCardView {
  const record = asRecord(args);
  const op = record ? stringField(record, 'op') : '';
  const detailsRecord = asRecord(details);
  const message = detailsRecord && 'message' in detailsRecord ? detailsRecord.message : undefined;
  const reply = detailsRecord && 'reply' in detailsRecord ? detailsRecord.reply : undefined;
  const typedMessage = ircProjection(message);
  const typedReply = ircProjection(reply);
  const boundedNote = boundHubBody(result).text.trim();

  if (op === 'wait') {
    if (status === 'running') {
      const ids = record && Array.isArray(record.ids) ? record.ids : null;
      if (ids && ids.length > 0) {
        return {
          op: 'wait',
          title: 'Waiting',
          headline: 'Waiting for a job to complete\u2026',
          body: '',
          note: '',
          typed: false,
        };
      }
      const fromName = readableAgentId(record ? stringField(record, 'from') : '');
      return {
        op: 'wait',
        title: 'Waiting',
        headline: fromName !== ''
          ? `Waiting for a message from ${boundText(fromName, 60)}\u2026`
          : 'Waiting for an agent message\u2026',
        body: '',
        note: '',
        typed: false,
      };
    }
    if (typedMessage) {
      return {
        op: 'wait',
        title: 'IRC',
        headline: typedMessage.from !== '' ? `from ${boundText(typedMessage.from, 60)}` : 'incoming message',
        body: boundHubBody(typedMessage.body).text,
        ...(typedMessage.replyTo ? { metadata: `reply to ${typedMessage.replyTo}` } : {}),
        note: '',
        typed: true,
      };
    }
    // Timeout (`message: null`), job-wait, or a malformed projection: concise
    // prose — never the raw ids/op/timeoutMs envelope.
    return {
      op: 'wait',
      title: 'Waiting',
      headline: '',
      body: '',
      note: boundedNote !== '' ? boundedNote : 'No message received.',
      typed: false,
    };
  }

  if (op === 'send') {
    const to = record ? stringField(record, 'to') : '';
    const messageText = record ? stringField(record, 'message') : '';
    const outcome = hubSendOutcome(details);
    return {
      op: 'send',
      title: 'IRC',
      headline: to !== '' ? `to ${boundText(to, 60)}` : 'outgoing',
      body: boundHubBody(messageText).text,
      note: outcome !== '' ? outcome : boundedNote,
      typed: typedReply !== null,
      ...(typedReply
        ? {
            reply: {
              headline: typedReply.from !== '' ? `from ${boundText(typedReply.from, 60)}` : 'reply',
              body: boundHubBody(typedReply.body).text,
              ...(typedReply.replyTo ? { metadata: `reply to ${typedReply.replyTo}` } : {}),
            },
          }
        : {}),
    };
  }

  // Other hub ops (list / inbox / cancel / read_history / unknown): safe
  // human fallback — the op label + bounded result text, never raw JSON.
  return {
    op,
    title: 'Hub',
    headline: op !== '' ? `hub ${op}` : 'hub',
    body: '',
    note: boundedNote,
    typed: false,
  };
}

export type TodoTaskStatus = 'pending' | 'in_progress' | 'completed' | 'abandoned';

/** One todo task on the card. Internal `id`/`ready`/`agent` never render;
 *  blocking reasons surface by their task content, not dependency ids. */
export interface TodoCardTaskView {
  content: string;
  status: TodoTaskStatus;
  /** Human-readable blocking task contents; empty when unblocked. */
  blockedBy: string[];
}

export interface TodoCardPhaseView {
  name: string;
  tasks: TodoCardTaskView[];
}

/** Structured default view for the `todo` tool card. The tool returns its
 *  phases snapshot in the result `details` (`TodoToolDetails`: phases +
 *  completedTasks) while the result text carries the TUI summary prose
 *  (`Remaining items … id=… ready`), which the card replaces with the compact
 *  phase/task list below. */
export interface TodoCardView {
  /** The `todo` op that produced this snapshot ('' when unknown). */
  op: string;
  phases: TodoCardPhaseView[];
  /** Contents of tasks completed by this op (details.completedTasks). */
  completed: string[];
  /** Bounded failure prose when the op errored (status 'error'). */
  error: string;
  /** Bounded backend prose only when no structured phases were parseable. */
  fallback: string;
}

const TODO_TASK_STATUS: Record<string, true> = {
  pending: true,
  in_progress: true,
  completed: true,
  abandoned: true,
};

function todoTaskStatus(value: unknown): TodoTaskStatus {
  return typeof value === 'string' && TODO_TASK_STATUS[value] === true
    ? (value as TodoTaskStatus)
    : 'pending';
}

function todoTaskFromRecord(raw: unknown): TodoCardTaskView | null {
  const record = asRecord(raw);
  if (!record) return null;
  const content = typeof record.content === 'string' ? record.content.trim() : '';
  if (content === '') return null;
  const blockedBy: string[] = [];
  if (Array.isArray(record.blockedBy)) {
    for (const reason of record.blockedBy) {
      const detail = asRecord(reason);
      const text = detail ? stringField(detail, 'content') : '';
      if (text !== '') blockedBy.push(boundText(text, TASK_ACTIVITY_CHAR_LIMIT));
    }
  }
  return {
    content: boundText(content, TODO_TASK_CHAR_LIMIT),
    status: todoTaskStatus(record.status),
    blockedBy,
  };
}

/** Parse the phases snapshot of a `todo` tool result (`details.phases`, the
 *  pi-coding `TodoPhase[]` wire: `{name, tasks:[{content,status,blockedBy}]}`
 *  with camelCase keys). Defensive: malformed tasks/phases are skipped; a
 *  missing or empty array yields null so the caller can fall back. */
export function parseTodoPhases(value: unknown): TodoCardPhaseView[] | null {
  if (!Array.isArray(value)) return null;
  const phases: TodoCardPhaseView[] = [];
  for (const rawPhase of value) {
    const record = asRecord(rawPhase);
    if (!record) continue;
    const name = typeof record.name === 'string' ? record.name.trim() : '';
    const tasksRaw = Array.isArray(record.tasks) ? record.tasks : [];
    const tasks: TodoCardTaskView[] = [];
    for (const rawTask of tasksRaw) {
      const task = todoTaskFromRecord(rawTask);
      if (task) tasks.push(task);
    }
    if (name === '' && tasks.length === 0) continue;
    phases.push({ name, tasks });
  }
  return phases.length > 0 ? phases : null;
}

function todoPhaseFromInit(raw: unknown): TodoCardPhaseView | null {
  const record = asRecord(raw);
  if (!record) return null;
  const name = typeof record.phase === 'string' ? record.phase.trim() : '';
  const items = Array.isArray(record.items) ? record.items : [];
  const tasks: TodoCardTaskView[] = [];
  for (const item of items) {
    if (typeof item !== 'string') continue;
    const content = item.trim();
    if (content === '') continue;
    tasks.push({ content: boundText(content, TODO_TASK_CHAR_LIMIT), status: 'pending', blockedBy: [] });
  }
  if (name === '' && tasks.length === 0) return null;
  return { name, tasks };
}

/** Parse `todo init` args (TodoOp::Init) into a pending-phases preview:
 *  `list: [{phase, items[]}]` or the flat `items[]` + `phase`. Renders the
 *  running card before the end details arrive. */
function todoPhasesFromInitArgs(record: Record<string, unknown>): TodoCardPhaseView[] | null {
  const list = Array.isArray(record.list) ? record.list : null;
  if (list) {
    const phases: TodoCardPhaseView[] = [];
    for (const raw of list) {
      const phase = todoPhaseFromInit(raw);
      if (phase) phases.push(phase);
    }
    return phases.length > 0 ? phases : null;
  }
  const phase = typeof record.phase === 'string' ? record.phase.trim() : '';
  const items = Array.isArray(record.items) ? record.items : [];
  const tasks: TodoCardTaskView[] = [];
  for (const item of items) {
    if (typeof item !== 'string') continue;
    const content = item.trim();
    if (content === '') continue;
    tasks.push({ content: boundText(content, TODO_TASK_CHAR_LIMIT), status: 'pending', blockedBy: [] });
  }
  if (phase === '' && tasks.length === 0) return null;
  return [{ name: phase, tasks }];
}

/** Resolve the Todo tool card default view. Phases come from the end `details`
 *  (`TodoToolDetails.phases` — the same wire the TodoPanel renders),
 *  falling back to `init` args for the running card; `completedTasks`
 *  contents surface the transitions of this op; failures show the bounded
 *  error prose alongside any parsed phases. When no structured phases exist
 *  the bounded backend prose renders as the fallback body — raw args/details
 *  JSON is never the default view. */
export function resolveTodoCardView(
  args: unknown,
  details?: unknown,
  result = '',
  status?: ToolCardStatus,
): TodoCardView {
  const record = asRecord(args);
  const op = record ? stringField(record, 'op') : '';
  const detailsRecord = asRecord(details);
  const phases = parseTodoPhases(
    detailsRecord && 'phases' in detailsRecord ? detailsRecord.phases : undefined,
  ) ?? (record ? todoPhasesFromInitArgs(record) : null) ?? [];
  const completed: string[] = [];
  const completedTasks = detailsRecord && Array.isArray(detailsRecord.completedTasks)
    ? detailsRecord.completedTasks
    : [];
  for (const raw of completedTasks) {
    const transition = asRecord(raw);
    const text = transition ? stringField(transition, 'content') : '';
    if (text !== '' && !completed.includes(text)) completed.push(boundText(text, TASK_ACTIVITY_CHAR_LIMIT));
  }
  let error = '';
  let fallback = '';
  if (status === 'error') {
    error = result !== '' ? boundOutput(result, TOOL_OUTPUT_LINE_LIMIT).text : 'todo op failed';
  } else if (status !== 'running' && phases.length === 0) {
    // Settled cards without a parseable phases snapshot keep the bounded
    // backend prose ("Todo list is empty." / "Todo list cleared."); a running
    // card never streams the summary prose while it is still executing.
    fallback = result !== '' ? boundOutput(result, TOOL_OUTPUT_LINE_LIMIT).text : '';
  }
  return { op, phases, completed, error, fallback };
}

export function classifyDiffLine(line: string): DiffLineKind {
  if (line.startsWith('+++') || line.startsWith('---') || line.startsWith('@@') || line.startsWith('diff ')) {
    return 'meta';
  }
  if (line.startsWith('+')) return 'add';
  if (line.startsWith('-')) return 'del';
  return 'ctx';
}

export function diffLines(diff: string): DiffLineView[] {
  if (!diff) return [];
  return diff.replace(/\r\n/g, '\n').replace(/\r/g, '\n').split('\n').map((text) => ({
    kind: classifyDiffLine(text),
    text,
  }));
}

function childMatchesJob(child: TaskCardChild, job: Record<string, unknown>): boolean {
  const jobId = stringField(job, 'id', 'jobId', 'job_id');
  const agentId = stringField(job, 'agentId', 'agent_id');
  if (child.jobId && jobId && child.jobId === jobId) return true;
  if (child.agentId && agentId && child.agentId === agentId) return true;
  if (child.name && agentId && child.name === agentId) return true;
  return false;
}

function jobResultSummary(job: Record<string, unknown>): string | undefined {
  const result = asRecord(job.result);
  if (!result) return undefined;
  const output = stringField(result, 'output');
  const error = stringField(result, 'error');
  // Prefer the delivered child output; never promote a generic system/error
  // envelope as the child's prose when an output payload is present.
  if (output.trim() !== '') return boundText(output, TASK_RESULT_CHAR_LIMIT);
  if (error.trim() !== '') return boundText(error, TASK_RESULT_CHAR_LIMIT);
  return undefined;
}

function withProjectedChildren(item: Extract<Item, { kind: 'toolCard' }>, children: TaskCardChild[]): Item {
  const spawns = spawnList(item.details);
  return {
    ...item,
    details: {
      ...(spawns ? { spawns } : {}),
      children,
    },
  };
}

/** Patch Task tool cards from a live `job_updated` frame. */
export function applyJobUpdated(items: Item[], frame: { job?: unknown }): Item[] {
  const job = asRecord(frame.job);
  if (!job) return items;
  const status = stringField(job, 'status');
  return items.map((item) => {
    if (item.kind !== 'toolCard' || item.toolName.toLowerCase() !== 'task') return item;
    const view = resolveTaskCardView(item.args, item.details);
    if (!view) return item;
    let changed = false;
    const children = view.children.map((child) => {
      if (!childMatchesJob(child, job)) return child;
      changed = true;
      return {
        ...child,
        jobId: stringField(job, 'id') || child.jobId,
        agentId: stringField(job, 'agentId', 'agent_id') || child.agentId,
        agent: stringField(job, 'agent') || child.agent,
        status: status || child.status,
        result: jobResultSummary(job) ?? child.result,
      };
    });
    if (!changed) return item;
    return withProjectedChildren(item, children);
  });
}

/** Patch Task tool cards from a live `agent_updated` frame (name/status only). */
export function applyAgentUpdated(items: Item[], frame: { agent?: unknown }): Item[] {
  const agent = asRecord(frame.agent);
  if (!agent) return items;
  const agentId = stringField(agent, 'id');
  if (agentId === '') return items;
  const status = stringField(agent, 'status');
  const displayName = stringField(agent, 'displayName', 'display_name');
  return items.map((item) => {
    if (item.kind !== 'toolCard' || item.toolName.toLowerCase() !== 'task') return item;
    const view = resolveTaskCardView(item.args, item.details);
    if (!view) return item;
    let changed = false;
    const children = view.children.map((child) => {
      if (child.agentId !== agentId && child.name !== agentId) return child;
      changed = true;
      return {
        ...child,
        agentId,
        name: displayName || child.name,
        status: status || child.status,
      };
    });
    if (!changed) return item;
    return withProjectedChildren(item, children);
  });
}

/** Attach a child→Main IRC body as the live activity line on matching Task cards. */
export function applyMessageDelivered(items: Item[], frame: { message?: unknown }): Item[] {
  const message = asRecord(frame.message);
  if (!message) return items;
  const from = stringField(message, 'from');
  const body = stringField(message, 'body');
  if (from === '' || body === '') return items;
  const activity = boundText(body, TASK_ACTIVITY_CHAR_LIMIT);
  return items.map((item) => {
    if (item.kind !== 'toolCard' || item.toolName.toLowerCase() !== 'task') return item;
    const view = resolveTaskCardView(item.args, item.details);
    if (!view) return item;
    let changed = false;
    const children = view.children.map((child) => {
      if (child.agentId !== from && child.name !== from) return child;
      changed = true;
      return { ...child, activity };
    });
    if (!changed) return item;
    return withProjectedChildren(item, children);
  });
}

/** Strip the `<orchestration-message …>…</orchestration-message>` wrapper and
 *  its trailing `Replying to message:` line, mirroring pi-coding's
 *  `extract_orchestration_body_from_wrapper`. Returns null when the content
 *  is not a wrapped orchestration message (so a non-IRC custom never reaches
 *  this path). */
function extractOrchestrationBody(content: unknown): string | null {
  const text = contentText(content);
  if (!text) return null;
  const trimmed = text.trim();
  const open = '<orchestration-message';
  const close = '</orchestration-message>';
  const start = trimmed.indexOf(open);
  if (start < 0) return null;
  const afterOpen = trimmed.slice(start + open.length);
  const gt = afterOpen.indexOf('>');
  if (gt < 0) return null;
  const inner = afterOpen.slice(gt + 1);
  const end = inner.lastIndexOf(close);
  if (end < 0) return null;
  let body = inner.slice(0, end).trim();
  const replyIdx = body.lastIndexOf('\nReplying to message:');
  if (replyIdx >= 0) body = body.slice(0, replyIdx).trimEnd();
  return body || null;
}

/** Old-snapshot fallback only: read the `from` attribute off the
 *  `<orchestration-message>` open tag so the direction label survives
 *  snapshots that predate typed details. Never consulted when details exist. */
function wrapperSender(content: unknown): string {
  const text = contentText(content);
  if (!text) return '';
  const open = text.indexOf('<orchestration-message');
  if (open < 0) return '';
  const tag = text.slice(open + '<orchestration-message'.length, text.indexOf('>', open));
  const match = /(?:^|\s)from="([^"]+)"/.exec(tag);
  return match ? match[1] : '';
}

/** Typed fields of an orchestration IRC custom message, mirroring the TUI's
 *  `OrchestrationIrcView` (crates/pi-cli/src/orchestration_message.rs). */
export interface IrcItemView {
  direction: IrcDirection;
  from: string;
  to: string;
  body: string;
  replyTo?: string;
}

/** Typed view over an orchestration IRC custom message. The raw
 *  `<orchestration-message>` XML wrapper is never rendered; the typed fields
 *  come from `details` through `ircProjection` (the SAME shared helper the
 *  hub wait/send tool-result card uses, so a control-prose result can never
 *  leak typed metadata), falling back to stripping the wrapper from `content`
 *  for older snapshots. Reply metadata is details-driven only — the fallback
 *  never guesses it from the body. Returns null for non-IRC customs. */
export function orchestrationIrcView(m: {
  customType?: string;
  content?: unknown;
  details?: unknown;
}): IrcItemView | null {
  if (m.customType !== 'orchestration_message') return null;
  const projection = ircProjection(m.details);
  // Typed `details.body` wins; an empty body falls back to stripping the
  // wrapper for older snapshots that only carry the XML in `content`.
  const body = projection && projection.body !== ''
    ? projection.body
    : extractOrchestrationBody(m.content);
  if (!body) return null;
  const from = projection ? projection.from : wrapperSender(m.content) || '?';
  const to = projection ? projection.to : '';
  return {
    direction: ircDirection(from, to),
    from,
    to,
    body,
    ...(projection && projection.replyTo ? { replyTo: projection.replyTo } : {}),
  };
}

interface CustomWire {
  role?: string;
  content?: unknown;
  display?: boolean;
  customType?: string;
  details?: unknown;
}

/** Normalize a custom message into a renderable item, or null when it must
 *  stay hidden. `display: false` customs carry internal system reminders /
 *  orchestration scaffolding and never render (mirrors the TUI's
 *  `push_message`: only `display: true` customs surface). Typed IRC customs
 *  become `kind: 'irc'` items (rendered by the shared IrcCard component —
 *  direction/from/to/body/replyTo, body bounded to IRC_BODY_LINE_LIMIT lines);
 *  every other display:true custom keeps the plain labeled row unchanged. */
export function customToItem(m: CustomWire): Item | null {
  if (m.display !== true) return null;
  const irc = orchestrationIrcView(m);
  if (irc) {
    return {
      kind: 'irc',
      id: nextId('i'),
      direction: irc.direction,
      from: irc.from,
      to: irc.to,
      body: boundIrcBody(irc.body),
      ...(irc.replyTo ? { replyTo: irc.replyTo } : {}),
    };
  }
  return {
    kind: 'custom',
    id: nextId('c'),
    label: typeof m.customType === 'string' ? m.customType : 'notice',
    text: contentText(m.content),
  };
}

/** Convert backend-authoritative lifecycle messages (Vec<Message> with
 *  role/content blocks) into renderable Items, reusing the same role/content
 *  rules as the live event stream. Deterministic per message.
 *
 *  Recognized roles: user, assistant (visible blocks plus durable toolCall
 *  cards correlated with later toolResult messages by toolCallId), toolResult
 *  (unmatched results remain readable; details carried onto the matched card),
 *  bashExecution, custom (rendered only when `display: true` — hidden internal
 *  messages never render; typed orchestration IRC customs render their parsed
 *  view), and branchSummary / compactionSummary (system notices).
 *  Unknown/malformed records are skipped defensively; one bad record never
 *  breaks the transcript restore.
 */
export function messagesToItems(messages: unknown): Item[] {
  const list = Array.isArray(messages) ? messages : [];
  const out: Item[] = [];
  const pendingToolCards = new Map<string, number>();
  for (const raw of list) {
    try {
      const m = (raw || {}) as {
        role?: string;
        content?: unknown;
        display?: boolean;
        customType?: string;
        summary?: string;
        command?: string;
        output?: string;
        details?: unknown;
        toolCallId?: string;
        isError?: boolean;
        exitCode?: number | null;
        cancelled?: boolean;
      };
      switch (m.role) {
        case 'user': {
          const projected = userMessageProjection(m.content);
          out.push({
            kind: 'user',
            id: nextId('u'),
            text: projected.text,
            optimistic: false,
            ...(projected.images.length > 0 ? { images: projected.images } : {}),
            ...(projected.analysis ? { analysis: projected.analysis } : {}),
          });
          break;
        }
        case 'assistant': {
          const content = Array.isArray(m.content) ? m.content : [];
          const visibleBlocks: ContentBlock[] = [];
          const toolCalls: Array<{ id: string; name: string; arguments: unknown }> = [];
          for (const rawBlock of content) {
            const block = (rawBlock || {}) as { type?: string; id?: string; name?: string; arguments?: unknown };
            if (block.type === 'toolCall') {
              toolCalls.push({
                id: typeof block.id === 'string' ? block.id : '',
                name: typeof block.name === 'string' && block.name !== '' ? block.name : 'tool',
                arguments: block.arguments,
              });
            } else {
              visibleBlocks.push(rawBlock as ContentBlock);
            }
          }
          if (visibleBlocks.length > 0) {
            out.push({ kind: 'assistant', id: nextId('a'), status: 'final', blocks: visibleBlocks });
          }
          for (const toolCall of toolCalls) {
            const cardIndex = out.length;
            out.push({
              kind: 'toolCard',
              id: nextId('tc'),
              toolCallId: toolCall.id,
              toolName: toolCall.name,
              args: toolCall.arguments,
              status: 'running',
              result: '',
            });
            if (toolCall.id !== '') pendingToolCards.set(toolCall.id, cardIndex);
          }
          break;
        }
        case 'toolResult': {
          const toolCallId = typeof m.toolCallId === 'string' ? m.toolCallId : '';
          const result = contentText(m.content);
          const media = toolMedia(m.content, m.details);
          const cardIndex = toolCallId === '' ? undefined : pendingToolCards.get(toolCallId);
          const card = cardIndex === undefined ? undefined : out[cardIndex];
          if (card?.kind === 'toolCard') {
            const limit = card.toolName.toLowerCase() === 'bash'
              ? BASH_OUTPUT_LINE_LIMIT
              : TOOL_OUTPUT_LINE_LIMIT;
            const completedCard: Item = {
              ...card,
              status: m.isError === true ? 'error' : 'done',
              result: boundOutput(result, limit).text,
              details: m.details,
              ...(media.length > 0 ? { media } : {}),
            };
            out.splice(cardIndex as number, 1, completedCard);
            pendingToolCards.delete(toolCallId);
          } else {
            out.push({
              kind: 'toolResult',
              id: nextId('r'),
              text: boundOutput(result, TOOL_OUTPUT_LINE_LIMIT).text,
            });
          }
          break;
        }
        case 'bashExecution': {
          // BashExecutionMessage serializes command/output at the top level
          // (pi-ai `Message` is `#[serde(tag = "role")]`), not nested in
          // `content`. Bound the output to its tail like the TUI compact card.
          // exitCode/cancelled (camelCase wire) drive the status border:
          // non-zero exit or cancellation → error, otherwise → done.
          const bashStatus: 'done' | 'error' | undefined =
            m.cancelled === true || (typeof m.exitCode === 'number' && m.exitCode !== 0)
              ? 'error'
              : typeof m.exitCode === 'number'
                ? 'done'
                : undefined;
          out.push({
            kind: 'bash',
            id: nextId('b'),
            command: m.command || '',
            output: boundOutput(m.output || '', BASH_OUTPUT_LINE_LIMIT).text,
            ...(bashStatus ? { status: bashStatus } : {}),
          });
          break;
        }
        case 'custom': {
          const item = customToItem(m);
          if (item) out.push(item);
          break;
        }
        case 'branchSummary':
          out.push({
            kind: 'summary',
            id: nextId('s'),
            label: 'Branch summary',
            text: typeof m.summary === 'string' ? m.summary : '',
          });
          break;
        case 'compactionSummary':
          out.push({
            kind: 'summary',
            id: nextId('s'),
            label: 'Compaction summary',
            text: typeof m.summary === 'string' ? m.summary : '',
          });
          break;
        default:
          break; // unknown roles are ignored safely
      }
    } catch {
      /* one malformed record never breaks the whole transcript restore */
    }
  }
  return out;
}
