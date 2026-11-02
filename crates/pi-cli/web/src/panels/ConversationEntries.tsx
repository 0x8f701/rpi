// Neutral conversation body and partial-markdown stream cache shared by
// SideChatPanel and FileConversationDock. No tabs, drafts, or RPC.
import { memo, useEffect, useRef } from 'react';
import { MarkdownBody } from '../MarkdownBody';
import { ToolCard } from '../App';
import { hydrateMermaid, renderStreamingMarkdown, thinkingSummaryHtml } from '../markdown';
import {
  BASH_OUTPUT_LINE_LIMIT,
  TOOL_OUTPUT_LINE_LIMIT,
  boundOutput,
  normalizeThinkingNewlines,
  type Item,
} from '../transcript';
import type { SideChatEntryWire } from './SideChatPanel';

export function conversationStreamKey(
  sessionId: string,
  kind: 'side' | 'file',
  ownerId: string,
  entryId: string,
): string {
  return `${sessionId}\u0000${kind}\u0000${ownerId}\u0000${entryId}`;
}

interface ConversationStreamCommit {
  text: string;
  html: string;
}

interface ConversationStreamOwner {
  innerHTML: string;
  isConnected: boolean;
}

interface ConversationStreamJob {
  key: string;
  text: string;
  owner: ConversationStreamOwner;
  onCommit?: () => void;
}

const conversationStreamCommitted = new Map<string, ConversationStreamCommit>();
const conversationStreamPending = new Map<string, ConversationStreamJob>();
let conversationStreamRaf: number | null = null;
let conversationStreamParseCount = 0;
let conversationStreamRafSeq = 0;
const conversationStreamFallback = new Map<number, FrameRequestCallback>();

function conversationStreamRequestFrame(callback: FrameRequestCallback): number {
  if (typeof requestAnimationFrame === 'function') return requestAnimationFrame(callback);
  conversationStreamRafSeq += 1;
  conversationStreamFallback.set(conversationStreamRafSeq, callback);
  return conversationStreamRafSeq;
}

function conversationStreamCancelFrame(handle: number): void {
  if (typeof cancelAnimationFrame === 'function') {
    cancelAnimationFrame(handle);
    return;
  }
  conversationStreamFallback.delete(handle);
}

function parseConversationStreamMarkdown(text: string): string {
  conversationStreamParseCount += 1;
  return renderStreamingMarkdown(text);
}

function commitConversationStreamJob(job: ConversationStreamJob): void {
  if (!job.owner.isConnected) return;
  const hit = conversationStreamCommitted.get(job.key);
  if (hit && hit.text === job.text) {
    if (job.owner.innerHTML !== hit.html) job.owner.innerHTML = hit.html;
    job.onCommit?.();
    if (typeof HTMLElement !== 'undefined' && job.owner instanceof HTMLElement) {
      void hydrateMermaid(job.owner, job.onCommit);
    }
    return;
  }
  const html = parseConversationStreamMarkdown(job.text);
  conversationStreamCommitted.set(job.key, { text: job.text, html });
  job.owner.innerHTML = html;
  job.onCommit?.();
  if (typeof HTMLElement !== 'undefined' && job.owner instanceof HTMLElement) {
    void hydrateMermaid(job.owner, job.onCommit);
  }
}

function armConversationStreamRaf(): void {
  if (conversationStreamRaf != null) return;
  conversationStreamRaf = conversationStreamRequestFrame(() => {
    conversationStreamRaf = null;
    const jobs = [...conversationStreamPending.values()];
    conversationStreamPending.clear();
    for (const job of jobs) commitConversationStreamJob(job);
  });
}

export function peekConversationStreamHtml(key: string, text: string): string {
  const hit = conversationStreamCommitted.get(key);
  if (hit && hit.text === text) return hit.html;
  if (hit) return hit.html;
  const html = parseConversationStreamMarkdown(text);
  conversationStreamCommitted.set(key, { text, html });
  return html;
}

export function scheduleConversationStream(
  key: string,
  text: string,
  owner: ConversationStreamOwner,
  onCommit?: () => void,
): void {
  const hit = conversationStreamCommitted.get(key);
  if (hit && hit.text === text && !conversationStreamPending.has(key)) {
    if (owner.isConnected && owner.innerHTML !== hit.html) owner.innerHTML = hit.html;
    if (typeof HTMLElement !== 'undefined' && owner instanceof HTMLElement) {
      void hydrateMermaid(owner, onCommit);
    }
    return;
  }
  conversationStreamPending.set(key, { key, text, owner, onCommit });
  armConversationStreamRaf();
}

export function cancelConversationStream(key: string, owner?: ConversationStreamOwner | null): void {
  const pending = conversationStreamPending.get(key);
  if (pending && owner && pending.owner !== owner) return;
  if (pending) conversationStreamPending.delete(key);
  conversationStreamCommitted.delete(key);
}

export function cancelConversationStreamsForOwner(sessionId: string, kind: 'side' | 'file', ownerId: string): void {
  const prefix = `${sessionId}\u0000${kind}\u0000${ownerId}\u0000`;
  for (const key of [...conversationStreamPending.keys()]) {
    if (key.startsWith(prefix)) cancelConversationStream(key);
  }
  for (const key of [...conversationStreamCommitted.keys()]) {
    if (key.startsWith(prefix)) conversationStreamCommitted.delete(key);
  }
}
export function cancelConversationStreamsForScope(scopeId: string): void {
  const prefix = `${scopeId}\u0000`;
  for (const key of [...conversationStreamPending.keys()]) {
    if (key.startsWith(prefix)) cancelConversationStream(key);
  }
  for (const key of [...conversationStreamCommitted.keys()]) {
    if (key.startsWith(prefix)) conversationStreamCommitted.delete(key);
  }
}

export function cancelAllConversationStreams(): void {
  conversationStreamPending.clear();
  conversationStreamCommitted.clear();
  if (conversationStreamRaf != null) {
    conversationStreamCancelFrame(conversationStreamRaf);
    conversationStreamRaf = null;
  }
  conversationStreamFallback.clear();
}

export function conversationStreamParseCountForTest(): number {
  return conversationStreamParseCount;
}

export function resetConversationStreamStateForTest(): void {
  cancelAllConversationStreams();
  conversationStreamParseCount = 0;
}

export function flushConversationStreamRafsForTest(): void {
  if (conversationStreamRaf != null) {
    conversationStreamCancelFrame(conversationStreamRaf);
    conversationStreamRaf = null;
  }
  conversationStreamFallback.clear();
  const jobs = [...conversationStreamPending.values()];
  conversationStreamPending.clear();
  for (const job of jobs) commitConversationStreamJob(job);
}

export interface ConversationStreamCache {
  peek: (key: string, text: string) => string;
  schedule: (key: string, text: string, owner: HTMLElement, onCommit?: () => void) => void;
  cancel: (key: string, owner?: HTMLElement | null) => void;
}

export const conversationStreamCache: ConversationStreamCache = {
  peek: peekConversationStreamHtml,
  schedule: scheduleConversationStream,
  cancel: cancelConversationStream,
};

export function conversationEntryToolCard(entry: SideChatEntryWire): Extract<Item, { kind: 'toolCard' }> {
  const toolName = typeof entry.toolName === 'string' ? entry.toolName : '';
  const output = typeof entry.output === 'string' ? entry.output : '';
  const lineLimit = toolName.toLowerCase() === 'bash' ? BASH_OUTPUT_LINE_LIMIT : TOOL_OUTPUT_LINE_LIMIT;
  return {
    kind: 'toolCard',
    id: entry.id,
    toolCallId: entry.id,
    toolName,
    args: entry.toolArgs ?? {},
    status: entry.isPartial ? 'running' : entry.isError ? 'error' : 'done',
    result: boundOutput(output, lineLimit).text,
  };
}

export function conversationPathContext(entries: readonly SideChatEntryWire[]): string {
  const parts: string[] = [];
  for (const entry of entries) {
    if (entry.text !== '') parts.push(entry.text);
    if (typeof entry.thinking === 'string' && entry.thinking !== '') parts.push(entry.thinking);
    if (typeof entry.output === 'string' && entry.output !== '') parts.push(entry.output);
  }
  return parts.join('\n');
}

function StreamingEntryBody({
  streamKey,
  text,
  className,
  onLayoutChange,
  stream,
}: {
  streamKey: string;
  text: string;
  className?: string;
  onLayoutChange?: () => void;
  stream: ConversationStreamCache;
}) {
  const ownerRef = useRef<HTMLDivElement>(null);
  const initialHtml = stream.peek(streamKey, text);
  useEffect(() => {
    const owner = ownerRef.current;
    if (!owner) return;
    stream.schedule(streamKey, text, owner, onLayoutChange);
  }, [stream, streamKey, text, onLayoutChange]);
  useEffect(() => {
    const owner = ownerRef.current;
    return () => stream.cancel(streamKey, owner);
  }, [stream, streamKey]);
  return <div ref={ownerRef} className={className} dangerouslySetInnerHTML={{ __html: initialHtml }} />;
}

const ConversationThinkingFold = memo(function ConversationThinkingFold({
  thinking,
  pathContext,
  onLayoutChange,
}: {
  thinking: string;
  pathContext?: string;
  onLayoutChange?: () => void;
}) {
  const normalized = normalizeThinkingNewlines(thinking);
  return (
    <details className="thinking" open>
      <summary
        className="thinking__summary"
        dangerouslySetInnerHTML={{ __html: thinkingSummaryHtml() }}
      />
      <MarkdownBody
        className="thinking__body"
        text={normalized}
        pathContext={pathContext ?? normalized}
        onLayoutChange={onLayoutChange}
      />
    </details>
  );
});

export function ConversationEntryView({
  entry,
  pathContext,
  streamKey,
  streaming,
  onLayoutChange,
  sessionId,
  cwd,
}: {
  entry: SideChatEntryWire;
  pathContext: string;
  streamKey: string;
  streaming?: ConversationStreamCache;
  onLayoutChange?: () => void;
  sessionId?: string;
  cwd?: string;
}) {
  const partialAssistant = entry.kind === 'assistant' && entry.isPartial && streaming != null;
  return (
    <div
      className={`side-chat__entry side-chat__entry--${entry.role}${entry.isError ? ' side-chat__entry--error' : ''}${entry.isPartial ? ' side-chat__entry--partial' : ''}`}
      data-session-id={sessionId}
      data-workspace-cwd={cwd}
    >
      {entry.kind === 'thinking' ? (
        <ConversationThinkingFold
          thinking={entry.thinking || entry.text}
          pathContext={pathContext}
          onLayoutChange={onLayoutChange}
        />
      ) : entry.kind === 'tool' ? (
        <ToolCard item={conversationEntryToolCard(entry)} onLayoutChange={onLayoutChange} />
      ) : partialAssistant ? (
        <StreamingEntryBody
          streamKey={streamKey}
          className="side-chat__text"
          text={entry.text}
          onLayoutChange={onLayoutChange}
          stream={streaming}
        />
      ) : (
        <MarkdownBody
          className="side-chat__text"
          text={entry.text}
          pathContext={pathContext}
          onLayoutChange={onLayoutChange}
        />
      )}
    </div>
  );
}
