// Pure (React-free) transcript normalization shared by the live event stream
// (App.tsx, CollabGuestView.tsx) and the restored message list
// (messagesToItems). Mirrors the TUI's content-visibility and output-bounding
// rules so live events and persisted/restored entries render identically:
//
//   * internal custom messages (`display: false`) never render — they carry
//     system reminders / orchestration scaffolding the TUI hides
//     (crates/pi-cli/src/tui.rs `push_message`: only `display: true` customs
//     surface, and typed IRC customs render their parsed view, not the XML);
//   * typed orchestration IRC customs (`display: true`) render their parsed
//     label + body from `details`, never the raw `<orchestration-message>` XML
//     wrapper (mirrors pi-coding `orchestration_message_view`);
//   * long command/tool output is bounded to its tail (failures surface at the
//     end), with a leading hint reporting the omitted line count — matching
//     the TUI compact tool-card fold (crates/pi-cli/src/tool_card_adapter.rs:
//     `BASH_CARD_OUTPUT_LIMIT = 10`, `DEFAULT_CARD_OUTPUT_LIMIT = 6`).

import type { ContentBlock } from './types';

export type Item =
  | { kind: 'user'; id: string; text: string; optimistic: boolean }
  | { kind: 'assistant'; id: string; status: 'streaming' | 'final'; blocks: ContentBlock[] }
  | { kind: 'toolCard'; id: string; toolCallId: string; toolName: string; args: unknown; status: 'running' | 'done' | 'error'; result: string }
  | { kind: 'toolResult'; id: string; text: string }
  | { kind: 'bash'; id: string; command: string; output: string }
  // Custom display:true backend messages (loops, projected notices, IRC).
  | { kind: 'custom'; id: string; label: string; text: string }
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

// TUI compact tool-card fold limits (crates/pi-cli/src/tool_card_adapter.rs).
// Bash keeps the last 10 lines; every other tool keeps the last 6.
export const BASH_OUTPUT_LINE_LIMIT = 10;
export const TOOL_OUTPUT_LINE_LIMIT = 6;

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
): Item[] {
  return items.map((item) => {
    if (item.kind !== 'toolCard' || item.toolCallId !== toolCallId) return item;
    const limit = item.toolName.toLowerCase() === 'bash' ? BASH_OUTPUT_LINE_LIMIT : TOOL_OUTPUT_LINE_LIMIT;
    return {
      ...item,
      ...(status ? { status } : {}),
      result: boundOutput(output, limit).text,
    };
  });
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

/** Typed view over an orchestration IRC custom message, mirroring the TUI's
 *  `orchestration_irc_view` (crates/pi-cli/src/orchestration_message.rs). The
 *  raw `<orchestration-message>` XML wrapper is never rendered; the label +
 *  body (with reply metadata) come from `details` (which the backend populates
 *  with the clean fields), falling back to stripping the wrapper from
 *  `content` for older snapshots. Returns null for non-IRC customs. */
export function orchestrationIrcView(m: {
  customType?: string;
  content?: unknown;
  details?: unknown;
}): { label: string; text: string } | null {
  if (m.customType !== 'orchestration_message') return null;
  const details = (m.details || {}) as { from?: string; to?: string; body?: string; replyTo?: string };
  const body = typeof details.body === 'string' && details.body !== ''
    ? details.body
    : extractOrchestrationBody(m.content);
  if (!body) return null;
  const from = details.from || '?';
  const to = details.to || '?';
  const label = `IRC \u00b7 ${from} \u2192 ${to}`;
  const text = details.replyTo ? `${body}\n(reply to ${details.replyTo})` : body;
  return { label, text };
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
 *  render their parsed view, never the raw XML wrapper. */
export function customToItem(m: CustomWire): Item | null {
  if (m.display !== true) return null;
  const irc = orchestrationIrcView(m);
  if (irc) {
    return { kind: 'custom', id: nextId('c'), label: irc.label, text: irc.text };
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
 *  (unmatched results remain readable), bashExecution, custom (rendered only
 *  when `display: true` — hidden internal messages never render; typed
 *  orchestration IRC customs render their parsed view), and branchSummary /
 *  compactionSummary (system notices). Unknown/malformed records are skipped
 *  defensively; one bad record never breaks the transcript restore.
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
      };
      switch (m.role) {
        case 'user':
          out.push({ kind: 'user', id: nextId('u'), text: contentText(m.content), optimistic: false });
          break;
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
          out.push({
            kind: 'bash',
            id: nextId('b'),
            command: m.command || '',
            output: boundOutput(m.output || '', BASH_OUTPUT_LINE_LIMIT).text,
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