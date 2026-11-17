import { redactSecrets } from './redact';

export interface ContentBlock {
  type: string;
  text?: string;
  thinking?: string;
  /** Image block payload (assistant transcript images). */
  data?: string;
  mimeType?: string;
}

export type ToolCardStatus = 'running' | 'done' | 'cancelled' | 'error';

export type TranscriptItem =
  | { kind: 'user'; id: string; text: string }
  | { kind: 'assistant'; id: string; status: 'streaming' | 'final'; blocks: ContentBlock[] }
  | { kind: 'tool'; id: string; toolCallId: string; toolName: string; args: string; result: string; status: ToolCardStatus }
  | { kind: 'bash'; id: string; command: string; output: string; status?: 'done' | 'cancelled' | 'error' }
  | { kind: 'notice'; id: string; text: string };

export interface TranscriptState {
  items: TranscriptItem[];
  activeAssistantId: string | null;
}

let nextOrdinal = 0;
function nextId(prefix: string): string {
  nextOrdinal += 1;
  return `${prefix}${nextOrdinal}`;
}

export function emptyTranscript(): TranscriptState {
  return { items: [], activeAssistantId: null };
}

export function assistantPlainText(items: readonly TranscriptItem[]): string {
  const parts: string[] = [];
  for (const item of items) {
    if (item.kind !== 'assistant') continue;
    const text = item.blocks
      .map((block) => (typeof block.text === 'string' ? block.text : ''))
      .join('')
      .trim();
    if (text) parts.push(text);
  }
  return parts.join('\n').trim();
}

export function channelTurnItems(
  items: readonly TranscriptItem[],
  priorAssistantIds: ReadonlySet<string>,
): TranscriptItem[] {
  return items.filter((item) => item.kind !== 'assistant' || !priorAssistantIds.has(item.id));
}

export function channelTerminalReplyText(
  frame: unknown,
  items: readonly TranscriptItem[],
  priorAssistantIds?: ReadonlySet<string>,
): string {
  const scoped = priorAssistantIds ? channelTurnItems(items, priorAssistantIds) : items;
  const text = assistantPlainText(scoped);
  if (text) return text;
  const value = asRecord(frame);
  if (value.status === 'succeeded' || value.status === 'aborted') return '';
  return (typeof value.message === 'string' ? value.message.trim() : '')
    || 'The runner reported that the turn failed.';
}

function asRecord(value: unknown): Record<string, unknown> {
  return value && typeof value === 'object' && !Array.isArray(value)
    ? value as Record<string, unknown>
    : {};
}

function blockText(block: unknown): string {
  const row = asRecord(block);
  if (typeof row.text === 'string') return row.text;
  if (typeof row.thinking === 'string') return row.thinking;
  return '';
}

function contentPlain(content: unknown): string {
  if (typeof content === 'string') return content;
  if (!Array.isArray(content)) return '';
  return content.map(blockText).filter(Boolean).join('\n');
}

function appendBlock(blocks: ContentBlock[], type: 'text' | 'thinking', delta: string): ContentBlock[] {
  const last = blocks[blocks.length - 1];
  if (type === 'text' && last?.type === 'text') {
    return [...blocks.slice(0, -1), { type: 'text', text: `${last.text ?? ''}${delta}` }];
  }
  if (type === 'thinking' && last?.type === 'thinking') {
    return [...blocks.slice(0, -1), { type: 'thinking', thinking: `${last.thinking ?? ''}${delta}` }];
  }
  return [...blocks, type === 'text' ? { type: 'text', text: delta } : { type: 'thinking', thinking: delta }];
}

/** Bounded preview of an orphan tool result's body. */
const ORPHAN_RESULT_PREVIEW_CHARS = 800;

/** Read-only notice text for a tool result whose call is not in the loaded
 *  history: names the tool and keeps the redacted output visible instead of
 *  presenting a running card or dropping the row. */
function orphanToolResultText(message: Record<string, unknown>, result: string): string {
  const toolName = typeof message.toolName === 'string' && message.toolName !== '' ? message.toolName : 'tool';
  const preview = result.length > ORPHAN_RESULT_PREVIEW_CHARS
    ? `${result.slice(0, ORPHAN_RESULT_PREVIEW_CHARS)}…`
    : result;
  return `${toolName} result without its call in the loaded history (read-only): ${preview}`;
}

export function messagesToItems(messages: unknown): TranscriptItem[] {
  const list = Array.isArray(messages) ? messages : [];
  const out: TranscriptItem[] = [];
  const pending = new Map<string, number>();
  for (const raw of list) {
    const wrapper = asRecord(raw);
    const message = 'message' in wrapper ? asRecord(wrapper.message) : wrapper;
    const stableId = typeof wrapper.id === 'string' && wrapper.id ? wrapper.id : undefined;
    const role = typeof message.role === 'string' ? message.role : '';
    switch (role) {
      case 'user':
        out.push({
          kind: 'user',
          id: stableId ?? nextId('u'),
          text: redactSecrets(contentPlain(message.content)),
        });
        break;
      case 'assistant': {
        const content = Array.isArray(message.content) ? message.content : [];
        const blocks: ContentBlock[] = [];
        for (const rawBlock of content) {
          const block = asRecord(rawBlock);
          if (block.type === 'toolCall') {
            const toolCallId = typeof block.id === 'string' ? block.id : '';
            pending.set(toolCallId, out.length);
            out.push({
              kind: 'tool',
              // Stable identity: a rebuilt history (each agent_history poll,
              // each get_messages reload) must not remount every card's fold
              // and copy state, so a real toolCallId names the card.
              id: toolCallId !== '' ? `tool-call:${toolCallId}` : nextId('tc'),
              toolCallId,
              toolName: typeof block.name === 'string' && block.name ? block.name : 'tool',
              args: redactSecrets(JSON.stringify(block.arguments ?? {})),
              result: '',
              status: 'running',
            });
          } else {
            blocks.push(rawBlock as ContentBlock);
          }
        }
        if (blocks.length > 0) {
          out.push({ kind: 'assistant', id: stableId ?? nextId('a'), status: 'final', blocks });
        }
        break;
      }
      case 'toolResult': {
        const toolCallId = typeof message.toolCallId === 'string' ? message.toolCallId : '';
        const result = redactSecrets(contentPlain(message.content));
        const index = pending.get(toolCallId);
        const card = index === undefined ? undefined : out[index];
        if (card?.kind === 'tool') {
          out[index as number] = {
            ...card,
            result,
            status: message.isError === true ? 'error' : 'done',
          };
          pending.delete(toolCallId);
        } else {
          // A result whose call fell outside the loaded window (suffix
          // truncation keeps results after their call was dropped) is shown
          // read-only, never silently dropped.
          out.push({
            kind: 'notice',
            id: stableId ?? nextId('n'),
            text: orphanToolResultText(message, result),
          });
        }
        break;
      }
      case 'bashExecution':
        out.push({
          kind: 'bash',
          id: stableId ?? nextId('b'),
          command: redactSecrets(typeof message.command === 'string' ? message.command : ''),
          output: redactSecrets(typeof message.output === 'string' ? message.output : ''),
          status: message.cancelled === true
            ? 'cancelled'
            : typeof message.exitCode === 'number'
              ? (message.exitCode === 0 ? 'done' : 'error')
              : undefined,
        });
        break;
      default:
        break;
    }
  }
  return out;
}

export function applySessionEvent(current: TranscriptState, frame: unknown): TranscriptState {
  const value = asRecord(frame);
  const type = typeof value.type === 'string' ? value.type : '';
  let items = current.items;
  let activeAssistantId = current.activeAssistantId;
  switch (type) {
    case 'subscription_snapshot':
    case 'session_resync': {
      if (Array.isArray(value.messages)) {
        items = messagesToItems(value.messages);
        activeAssistantId = null;
      }
      break;
    }
    case 'message_start': {
      const message = asRecord(value.message);
      if (message.role === 'assistant') {
        const id = typeof value.itemId === 'string' && value.itemId ? value.itemId : nextId('a');
        const assistant: TranscriptItem = { kind: 'assistant', id, status: 'streaming', blocks: [] };
        const existing = items.findIndex((item) => item.id === id);
        items = existing >= 0
          ? items.map((item, index) => (index === existing ? assistant : item))
          : [...items, assistant];
        activeAssistantId = id;
      } else if (message.role === 'user') {
        items = [...items, {
          kind: 'user',
          id: typeof value.itemId === 'string' && value.itemId ? value.itemId : nextId('u'),
          text: redactSecrets(contentPlain(message.content)),
        }];
      }
      break;
    }
    case 'message_update': {
      const event = asRecord(value.assistantMessageEvent);
      const id = typeof value.itemId === 'string' && value.itemId ? value.itemId : activeAssistantId;
      const delta = typeof event.delta === 'string' ? event.delta : '';
      if (id && delta && (event.type === 'text_delta' || event.type === 'thinking_delta')) {
        items = items.map((item) => (
          item.kind === 'assistant' && item.id === id
            ? {
                ...item,
                blocks: appendBlock(item.blocks, event.type === 'text_delta' ? 'text' : 'thinking', delta),
              }
            : item
        ));
      }
      break;
    }
    case 'message_end': {
      const message = asRecord(value.message);
      if (message.role !== 'assistant') break;
      const id = typeof value.itemId === 'string' && value.itemId ? value.itemId : activeAssistantId;
      if (!id) break;
      const finalBlocks = Array.isArray(message.content)
        ? (message.content as ContentBlock[]).filter((block) => block && block.type !== 'toolCall')
        : [];
      items = items.map((item) => (
        item.kind === 'assistant' && item.id === id
          ? { ...item, status: 'final', blocks: finalBlocks.length > 0 ? finalBlocks : item.blocks }
          : item
      ));
      if (activeAssistantId === id) activeAssistantId = null;
      break;
    }
    case 'tool_execution_start': {
      const toolCallId = typeof value.toolCallId === 'string' ? value.toolCallId : '';
      items = [...items, {
        kind: 'tool',
        id: typeof value.itemId === 'string' && value.itemId ? value.itemId : nextId('tc'),
        toolCallId,
        toolName: typeof value.toolName === 'string' && value.toolName ? value.toolName : 'tool',
        args: redactSecrets(typeof value.args === 'string' ? value.args : JSON.stringify(value.args ?? {})),
        result: '',
        status: 'running',
      }];
      break;
    }
    case 'tool_execution_end': {
      const toolCallId = typeof value.toolCallId === 'string' ? value.toolCallId : '';
      const result = asRecord(value.result);
      const text = redactSecrets(contentPlain(result.content));
      items = items.map((item) => (
        item.kind === 'tool' && item.toolCallId === toolCallId
          ? { ...item, result: text, status: value.isError === true ? 'error' : 'done' }
          : item
      ));
      break;
    }
    case 'bash_execution_end': {
      const message = asRecord(value.message);
      items = [...items, {
        kind: 'bash',
        id: typeof value.itemId === 'string' && value.itemId ? value.itemId : nextId('b'),
        command: redactSecrets(typeof message.command === 'string' ? message.command : ''),
        output: redactSecrets(typeof message.output === 'string' ? message.output : ''),
        status: message.cancelled === true
          ? 'cancelled'
          : typeof message.exitCode === 'number'
            ? (message.exitCode === 0 ? 'done' : 'error')
            : undefined,
      }];
      break;
    }
    case 'turn_terminal':
      items = items.map((item) => (
        item.kind === 'assistant' && item.status === 'streaming' ? { ...item, status: 'final' } : item
      ));
      break;
    default:
      break;
  }
  return { items, activeAssistantId };
}
