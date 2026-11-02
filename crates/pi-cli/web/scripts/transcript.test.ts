#!/usr/bin/env node
// Focused transcript-normalization regression for src/transcript.ts — the
// pure rules shared by the live event stream (App.tsx, CollabGuestView.tsx)
// and the restored message list (messagesToItems). Run through `npm run build`,
// which bundles this file with Vite's installed esbuild into a disposable
// Node-compatible module before executing the focused assertions.
//
// Exit codes: 0 = every assertion held; 1 = a parity rule regressed.
//
// Each scenario mirrors a real backend wire shape (pi-ai `Message` is
// `#[serde(tag = "role")]`, so bashExecution carries top-level command/output;
// `public_message` in crates/pi-cli/src/modes/rpc.rs rewrites loop scheduled
// turns to display:true customs with the clean prompt; orchestration IRC
// customs arrive display:true with the raw `<orchestration-message>` XML in
// `content` and the clean fields in `details`).
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server.browser';
import { TaskToolCardBody, ToastList, ToolCard, BashCard } from '../src/App.tsx';
import { redactedClipboardText } from '../src/ansi.ts';
import {
  messagesToItems,
  boundOutput,
  customToItem,
  orchestrationIrcView,
  applyToolSnapshot,
  applyToolStartToItems,
  applyToolResultToItems,
  toolMedia,
  userImages,
  userMessageProjection,
  parseImageAnalysis,
  mergeAuthoritativeItems,
  applyJobUpdated,
  applyJobSnapshot,
  taskJobIds,
  applyMessageDelivered,
  applyAgentUpdated,
  resolveTaskCardView,
  taskCardStatus,
  parseTaskCardArgs,
  firstLinePreview,
  taskChildActionLine,
  resolveTodoCardView,
  parseTodoPhases,
  parseEditCard,
  compactToolArgs,
  parseCommandCardArgs,
  parseProcessCardArgs,
  parseWriteCardArgs,
  parseReadCard,
  parseHubCard,
  parseHeading,
  diffLines,
  shouldRestoreStreamingAssistant,
  planReconnectTurn,
  planStreamStallReconcile,
  planSettledStreamRefillRequest,
  planStreamWatchdogTick,
  STREAM_WATCHDOG_MS,
  withoutSessionWatchdogTimer,
  createStreamWatchdog,
  reduceStreamStall,
  planStreamStallChrome,
  planLiveSequenceGap,
  planLiveGapCatchUp,
  planStreamCatchUpToast,
  messagesFromCatchUpResponse,
  planGapCatchUpMergeStreaming,
  liveHasStreamingAssistant,
  finalizeStreamingAssistant,
  shouldNormalizeThinkingNewlines,
  normalizeThinkingNewlines,
  unescapeThinkingNewlines,
  ircProjection,
  ircDirection,
  ircTitle,
  boundIrcBody,
  boundHubBody,
  BASH_OUTPUT_LINE_LIMIT,
  TOOL_OUTPUT_LINE_LIMIT,
  IRC_BODY_LINE_LIMIT,
  IRC_COMPACT_LINE_LIMIT,
  transcriptPathContext,
} from '../src/transcript.ts';
import {
  activityStripView,
  applyActivityStripEvent,
  emptyActivityStripState,
} from '../src/activityStrip.ts';
import { isBusyCapacityError } from '../src/types.ts';

// The hub tool card renders body/note through the shared markdown renderer;
// the renderer's behavior for hub-shaped bodies is asserted in
// markdown.test.ts (that bundle aliases mermaid), so this file keeps the
// parse-layer contract only.
const failures = [];
let ran = 0;
function check(name, cond, detail) {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}
const transportToastMarkup = renderToStaticMarkup(createElement(ToastList, {
  toasts: [{ id: 'transport-outage', message: 'connection closed, reconnecting…', error: true, kind: 'transport' }],
  dismiss: () => {},
}));
check(
  'transport toast exposes typed DOM authority',
  transportToastMarkup.includes('data-toast-kind="transport"'),
  transportToastMarkup,
);

// ---- limits match the TUI compact tool-card fold ----
check('BASH_OUTPUT_LINE_LIMIT == 10 (TUI BASH_CARD_OUTPUT_LIMIT)', BASH_OUTPUT_LINE_LIMIT === 10);
check('TOOL_OUTPUT_LINE_LIMIT == 6 (TUI DEFAULT_CARD_OUTPUT_LIMIT)', TOOL_OUTPUT_LINE_LIMIT === 6);

{
  const context = transcriptPathContext([
    {
      kind: 'toolCard',
      id: 'grep',
      toolCallId: 'call-grep',
      toolName: 'grep',
      args: {},
      status: 'done',
      result: 'src/runtime/process_block.rs:472:fn handle(',
    },
    {
      kind: 'assistant',
      id: 'cite',
      status: 'final',
      blocks: [{ type: 'text', text: 'see `process_block.rs:472-480`' }],
    },
  ]);
  check(
    'transcriptPathContext includes grep then the assistant cite',
    context.includes('src/runtime/process_block.rs:472:fn handle(')
      && context.includes('see `process_block.rs:472-480`'),
    context,
  );
  const fromArgs = transcriptPathContext([
    {
      kind: 'toolCard',
      id: 'read',
      toolCallId: 'call-read',
      toolName: 'read',
      args: { path: 'src/runtime/process_block.rs' },
      status: 'done',
      result: 'fn handle() {',
    },
    {
      kind: 'assistant',
      id: 'cite-after-read',
      status: 'final',
      blocks: [{ type: 'text', text: 'see `process_block.rs:472-480`' }],
    },
  ]);
  check(
    'transcriptPathContext keeps a tool-card args path',
    fromArgs.includes('src/runtime/process_block.rs')
      && fromArgs.includes('see `process_block.rs:472-480`'),
    fromArgs,
  );
}

// ---- boundOutput: tail kept, leading hint, edge cases ----
{
  const empty = boundOutput('', 10);
  check('boundOutput empty', empty.text === '' && !empty.bounded && empty.omitted === 0);
  const short = boundOutput('a\nb\nc', 10);
  check('boundOutput under limit unchanged', short.text === 'a\nb\nc' && !short.bounded);
  const long = boundOutput(Array.from({ length: 30 }, (_, i) => String(i + 1)).join('\n'), 10);
  check(
    'boundOutput keeps the tail (last 10)',
    long.bounded && long.omitted === 20 && long.text.endsWith('\n30') && !long.text.startsWith('1\n'),
    long.text,
  );
  check('boundOutput hint reports omitted count', long.text.startsWith('\u2026 20 more lines\n'), long.text);
  check('boundOutput single-line pluralization', boundOutput('x\ny', 1).text === '\u2026 1 more line\ny');
  const exactTrailing = boundOutput(Array.from({ length: 10 }, (_, i) => String(i + 1)).join('\n') + '\n', 10);
  check('boundOutput exact limit with terminal newline stays unbounded', exactTrailing.text === Array.from({ length: 10 }, (_, i) => String(i + 1)).join('\n') && !exactTrailing.bounded && exactTrailing.omitted === 0, exactTrailing.text);
  const overTrailing = boundOutput(Array.from({ length: 11 }, (_, i) => String(i + 1)).join('\n') + '\n', 10);
  const crlfTrailing = boundOutput('a\r\nb\r\n', 2);
  check('boundOutput normalizes CRLF terminators like Rust lines', crlfTrailing.text === 'a\nb' && !crlfTrailing.bounded, crlfTrailing.text);
  check('boundOutput terminal newline does not count as omitted content', overTrailing.text === '\u2026 1 more line\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11', overTrailing.text);
}

// ---- cumulative live snapshots replace and remain bounded ----
{
  const running = [{
    kind: 'toolCard', id: 'card', toolCallId: 'call', toolName: 'bash', args: {}, status: 'running', result: '',
  }];
  const first = applyToolSnapshot(running, 'call', Array.from({ length: 12 }, (_, i) => `line-${i + 1}`).join('\n'));
  const second = applyToolSnapshot(first, 'call', Array.from({ length: 20 }, (_, i) => `line-${i + 1}`).join('\n'));
  const card = second[0];
  check('cumulative live snapshot replaces prior content', card.kind === 'toolCard' && !card.result.includes('line-1\nline-2') && card.result.endsWith('line-20'), JSON.stringify(card));
  check('live bash snapshot uses ten-line bound', card.kind === 'toolCard' && card.result.startsWith('\u2026 10 more lines\nline-11'), JSON.stringify(card));
  const finished = applyToolSnapshot(second, 'call', Array.from({ length: 20 }, (_, i) => `line-${i + 1}`).join('\n'), 'error')[0];
  check('final snapshot preserves bound and status', finished.kind === 'toolCard' && finished.status === 'error' && finished.result === card.result, JSON.stringify(finished));
}

// ---- missing Command arguments settle the running card immediately ----
{
  const running = [{
    kind: 'toolCard', id: 'command-card', toolCallId: 'command-call', toolName: 'bash', args: {}, status: 'running', result: '',
  }];
  const settled = applyToolSnapshot(
    running,
    'command-call',
    'missing required argument: command',
    'error',
  )[0];
  check(
    'empty Command settles as explicit schema error',
    settled.kind === 'toolCard'
      && settled.status === 'error'
      && settled.result.includes('missing required argument')
      && settled.result.includes('command'),
    JSON.stringify(settled),
  );
}

// ---- duplicate start/end/toolResult frames reconcile by toolCallId ----
{
  const wait = {
    kind: 'toolCard', id: 'wait-card', toolCallId: 'wait-call', toolName: 'hub',
    args: { op: 'wait' }, status: 'running', result: '',
  };
  const duplicateStart = applyToolStartToItems(applyToolStartToItems([], wait), {
    ...wait,
    id: 'late-duplicate-card',
  });
  check(
    'duplicate tool start keeps one logical card',
    duplicateStart.filter((item) => item.kind === 'toolCard' && item.toolCallId === 'wait-call').length === 1,
    JSON.stringify(duplicateStart),
  );
  const ended = applyToolSnapshot(
    duplicateStart,
    'wait-call',
    'orchestration shut down',
    'error',
    { status: 'failed' },
  );
  const duplicatedResult = applyToolResultToItems(
    [...ended, { ...wait, id: 'restored-duplicate', status: 'error', result: 'orchestration shut down' }],
    'wait-call',
    'orchestration shut down',
    { status: 'failed' },
  );
  const waitCards = duplicatedResult.filter(
    (item) => item.kind === 'toolCard' && item.toolCallId === 'wait-call',
  );
  check('duplicate tool end/result collapses to one card', waitCards.length === 1, JSON.stringify(duplicatedResult));
  check(
    'duplicate tool end/result preserves terminal error',
    waitCards[0]?.kind === 'toolCard'
      && waitCards[0].status === 'error'
      && waitCards[0].result === 'orchestration shut down',
    JSON.stringify(waitCards[0]),
  );
}

// ---- authoritative snapshots keep live tool cards until history catches up ----
{
  const live = [
    { kind: 'user', id: 'optimistic', text: 'queued', optimistic: true },
    { kind: 'assistant', id: 'streaming', status: 'streaming', blocks: [] },
    { kind: 'toolCard', id: 'search', toolCallId: 'search-call', toolName: 'web_search', args: { query: 'rust' }, status: 'done', result: 'release notes' },
    { kind: 'toolCard', id: 'read', toolCallId: 'read-call', toolName: 'read', args: { path: 'seed.txt' }, status: 'done', result: 'seed' },
  ];
  const staleHistory = [{ kind: 'user', id: 'history-user', text: 'earlier', optimistic: false }];
  const preserved = mergeAuthoritativeItems(staleHistory, live);
  check('stale authoritative snapshot preserves optimistic user', preserved.some((item) => item.kind === 'user' && item.id === 'optimistic'), JSON.stringify(preserved));
  check('stale authoritative snapshot preserves streaming assistant', preserved.some((item) => item.kind === 'assistant' && item.id === 'streaming'), JSON.stringify(preserved));
  check('stale authoritative snapshot preserves every live tool card', preserved.filter((item) => item.kind === 'toolCard').length === 2, JSON.stringify(preserved));

  const durableHistory = [
    ...staleHistory,
    { kind: 'toolCard', id: 'durable-search', toolCallId: 'search-call', toolName: 'web_search', args: { query: 'rust' }, status: 'done', result: 'durable release notes' },
  ];
  const caughtUp = mergeAuthoritativeItems(durableHistory, live);
  check('caught-up snapshot replaces matching live card by toolCallId', caughtUp.filter((item) => item.kind === 'toolCard' && item.toolCallId === 'search-call').length === 1, JSON.stringify(caughtUp));
  check('caught-up snapshot keeps other not-yet-durable tool cards', caughtUp.some((item) => item.kind === 'toolCard' && item.toolCallId === 'read-call'), JSON.stringify(caughtUp));
  const settled = mergeAuthoritativeItems(durableHistory, live, false);
  check('settled authoritative snapshot drops stale optimistic and streaming items', !settled.some((item) => item.id === 'optimistic' || item.id === 'streaming'), JSON.stringify(settled));
  check('settled authoritative snapshot still preserves not-yet-durable tool cards', settled.some((item) => item.kind === 'toolCard' && item.toolCallId === 'read-call'), JSON.stringify(settled));

  const routed = { kind: 'summary', id: 'todo-route-receipt-1', label: 'todo', text: 'Routed to Todo · child-1' };
  const restoredRoute = mergeAuthoritativeItems(durableHistory, [...live, routed], false);
  check(
    'settled authoritative restore preserves the live durable Todo route receipt projection',
    restoredRoute.filter((item) => item.kind === 'summary' && item.id === routed.id).length === 1,
    JSON.stringify(restoredRoute),
  );
  const dedupedRoute = mergeAuthoritativeItems([...durableHistory, routed], [...live, routed], false);
  check(
    'authoritative Todo route projection deduplicates by durable receipt identity',
    dedupedRoute.filter((item) => item.kind === 'summary' && item.id === routed.id).length === 1,
    JSON.stringify(dedupedRoute),
  );
}

{
  const live = [
    { kind: 'user', id: 'optimistic', text: 'queued', optimistic: true },
  ];
  const samePromptHistory = [
    { kind: 'user', id: 'durable-prompt', text: 'queued', optimistic: false },
  ];
  const dedupedPrompt = mergeAuthoritativeItems(samePromptHistory, live);
  check(
    'authoritative user replaces the matching optimistic bubble during streaming reconnect',
    dedupedPrompt.filter((item) => item.kind === 'user' && item.text === 'queued').length === 1
      && !dedupedPrompt.some((item) => item.kind === 'user' && item.id === 'optimistic'),
    JSON.stringify(dedupedPrompt),
  );
  const duplicatePrompts = mergeAuthoritativeItems(
    [
      { kind: 'user', id: 'durable-1', text: 'same', optimistic: false },
      { kind: 'user', id: 'durable-2', text: 'same', optimistic: false },
    ],
    [
      { kind: 'user', id: 'live-durable', text: 'same', optimistic: false },
      { kind: 'user', id: 'live-optimistic', text: 'same', optimistic: true },
    ],
  );
  check(
    'duplicate prompt text uses occurrence counts instead of dropping a distinct optimistic turn',
    duplicatePrompts.filter((item) => item.kind === 'user' && item.text === 'same').length === 2
      && !duplicatePrompts.some((item) => item.id === 'live-optimistic'),
    JSON.stringify(duplicatePrompts),
  );
  const emptyIdLive = [
    { kind: 'toolCard', id: 'unidentified', toolCallId: '', toolName: 'read', args: {}, status: 'running', result: '' },
  ];
  check(
    'unidentified running card survives only while the backend reports an in-flight run',
    mergeAuthoritativeItems([], emptyIdLive, true).length === 1
      && mergeAuthoritativeItems([], emptyIdLive, false).length === 0,
  );
  const runningHistoryOnly = {
    kind: 'toolCard' as const,
    id: 'tc-hist',
    toolCallId: 'task-hist-call',
    toolName: 'task',
    args: { agent: 'reviewer', name: 'Hist', task: 'historical work' },
    status: 'running' as const,
    result: '',
    details: [{ index: 0, jobId: 'job-old', agentId: 'Alpha', agent: 'reviewer', status: 'running' }],
  };
  const coldRunningHistory = mergeAuthoritativeItems([runningHistoryOnly], [], false);
  check(
    'running-only history is kept when live has no durable items',
    coldRunningHistory.length === 1 && coldRunningHistory[0]?.id === 'tc-hist',
    JSON.stringify(coldRunningHistory.map((item) => item.id)),
  );
  const lastGood = [
    { kind: 'user' as const, id: 'u-keep', text: 'hello', optimistic: false },
    {
      kind: 'assistant' as const,
      id: 'a-keep',
      status: 'final' as const,
      blocks: [{ type: 'text', text: 'world' }],
    },
  ];
  const emptySettled = mergeAuthoritativeItems([], lastGood, false);
  check(
    'empty settled reconnect history keeps last-good durable turns',
    emptySettled.some((item) => item.id === 'u-keep') && emptySettled.some((item) => item.id === 'a-keep'),
    JSON.stringify(emptySettled),
  );
  const emptyColdFull = mergeAuthoritativeItems(
    [],
    [...lastGood, { kind: 'assistant' as const, id: 'stream', status: 'streaming' as const, blocks: [] }],
    true,
  );
  check(
    'empty cold-full snapshot keeps last-good durable turns and the streaming assistant',
    emptyColdFull.some((item) => item.id === 'u-keep')
      && emptyColdFull.some((item) => item.id === 'a-keep')
      && emptyColdFull.some((item) => item.id === 'stream'),
    JSON.stringify(emptyColdFull),
  );
}

// ---- restored: legitimate user/assistant turns remain ----
{
  const items = messagesToItems([
    { role: 'user', content: [{ type: 'text', text: 'hello user' }], timestamp: 1 },
    { role: 'assistant', content: [{ type: 'text', text: 'hi assistant' }], timestamp: 2 },
  ]);
  const user = items.find((i) => i.kind === 'user');
  const asst = items.find((i) => i.kind === 'assistant');
  check('restored user text remains', user && user.text === 'hello user', JSON.stringify(user));
  check('restored assistant block remains', asst && asst.blocks.length === 1, JSON.stringify(asst));
}


{
  const items = messagesToItems([
    { id: 'entry-user', message: { role: 'user', content: [{ type: 'text', text: 'stable user' }] } },
    { id: 'entry-assistant', message: { role: 'assistant', content: [{ type: 'text', text: 'stable assistant' }] } },
  ]);
  check('recorder entry id owns restored user item', items[0]?.id === 'entry-user', JSON.stringify(items));
  check('recorder entry id owns restored assistant item', items[1]?.id === 'entry-assistant', JSON.stringify(items));
}
// ---- restored: a failed turn surfaces its persisted errorMessage (never an empty hole) ----
{
  const items = messagesToItems([
    { id: 'entry-fail', message: { role: 'assistant', content: [], stopReason: 'error', errorMessage: 'session HTTP transport failed', timestamp: 3 } },
  ]);
  check(
    'restored empty-content error turn keeps an assistant item with errorMessage',
    items.length === 1 && items[0].kind === 'assistant' && items[0].errorMessage === 'session HTTP transport failed',
    JSON.stringify(items),
  );
}
{
  const items = messagesToItems([
    { id: 'entry-fail-empty-text', message: { role: 'assistant', content: [{ type: 'text', text: '' }], stopReason: 'error', errorMessage: 'mock upstream failed', timestamp: 3 } },
  ]);
  check(
    'restored empty-text error turn becomes a failure row without an empty bubble',
    items.length === 1 && items[0].kind === 'assistant' && items[0].blocks.length === 0
      && items[0].errorMessage === 'mock upstream failed',
    JSON.stringify(items),
  );
}
{
  const items = messagesToItems([
    { id: 'entry-abort', message: { role: 'assistant', content: [], stopReason: 'aborted', errorMessage: 'Request was aborted', timestamp: 3 } },
  ]);
  check(
    'restored aborted turns stay off the transcript (TUI reserved status-row contract)',
    items.length === 0,
    JSON.stringify(items),
  );
}
{
  const items = messagesToItems([
    { id: 'entry-partial', message: { role: 'assistant', content: [{ type: 'text', text: 'partial answer' }], stopReason: 'error', errorMessage: 'OpenAI stream ended without finish_reason', timestamp: 3 } },
  ]);
  check(
    'restored partial-content error turn renders its blocks without an error row (TUI parity)',
    items.length === 1 && items[0].kind === 'assistant' && items[0].blocks.length === 1 && items[0].errorMessage === undefined,
    JSON.stringify(items),
  );
}
// ---- restored: bashExecution reads top-level command/output (not nested) ----
{
  const items = messagesToItems([
    { role: 'bashExecution', command: 'seq 1 30', output: Array.from({ length: 30 }, (_, i) => String(i + 1)).join('\n'), timestamp: 3 },
  ]);
  const bash = items.find((i) => i.kind === 'bash');
  check('restored bash keeps the command (top-level, not m.content)', bash && bash.command === 'seq 1 30', JSON.stringify(bash));
  check('restored bash output is bounded to the tail', bash && bash.output.endsWith('\n30') && bash.output.startsWith('\u2026 20 more lines\n'), JSON.stringify(bash));
}

// ---- restored: unmatched toolResult stays readable and bounded ----
{
  const twelve = Array.from({ length: 12 }, (_, i) => String(i + 1)).join('\n');
  const items = messagesToItems([{
    role: 'toolResult', toolCallId: 'orphan-call', toolName: 'read',
    content: [{ type: 'text', text: twelve }], isError: true, timestamp: 4,
  }]);
  const tr = items.find((i) => i.kind === 'toolResult');
  check('unmatched restored toolResult remains readable', items.length === 1 && tr?.kind === 'toolResult', JSON.stringify(items));
  check('unmatched restored toolResult bounded to last 6 lines', tr && tr.text.endsWith('\n12') && tr.text.startsWith('\u2026 6 more lines\n'), JSON.stringify(tr));
}

// ---- restored: durable bash toolCall + result becomes the live toolCard shape ----
{
  const output = Array.from({ length: 15 }, (_, i) => `bash-card-${i + 1}`).join('\n');
  const items = messagesToItems([
    {
      role: 'assistant',
      content: [{ type: 'toolCall', id: 'bash-call', name: 'bash', arguments: { command: 'seq 1 15' } }],
      timestamp: 5,
    },
    {
      role: 'toolResult', toolCallId: 'bash-call', toolName: 'bash',
      content: [{ type: 'text', text: output }], isError: true, timestamp: 6,
    },
  ]);
  const card = items.find((i) => i.kind === 'toolCard');
  const bashCommand = card?.kind === 'toolCard'
    && card.args !== null
    && typeof card.args === 'object'
    && 'command' in card.args
    ? card.args.command
    : undefined;
  check('toolCall-only assistant emits no invisible assistant shell', !items.some((i) => i.kind === 'assistant'), JSON.stringify(items));
  check('restored bash card preserves id/name/command', card?.kind === 'toolCard' && card.toolCallId === 'bash-call' && card.toolName === 'bash' && bashCommand === 'seq 1 15', JSON.stringify(card));
  check('restored bash error card uses ten-line bounded tail', card?.kind === 'toolCard' && card.status === 'error' && card.result.startsWith('\u2026 5 more lines\nbash-card-6') && card.result.endsWith('\nbash-card-15'), JSON.stringify(card));
}

// ---- restored: generic tool correlation is id-driven, not output inference ----
{
  const output = Array.from({ length: 8 }, (_, i) => `generic-tail-${i + 1}`).join('\n');
  const items = messagesToItems([
    {
      role: 'assistant',
      content: [
        { type: 'text', text: 'generic preface' },
        { type: 'toolCall', id: 'generic-call', name: 'read', arguments: { path: 'fixture.txt' } },
      ],
      timestamp: 7,
    },
    {
      role: 'toolResult', toolCallId: 'generic-call', toolName: 'untrusted-result-name',
      content: [{ type: 'text', text: output }], isError: false, timestamp: 8,
    },
  ]);
  const assistant = items.find((i) => i.kind === 'assistant');
  const card = items.find((i) => i.kind === 'toolCard');
  const genericPath = card?.kind === 'toolCard'
    && card.args !== null
    && typeof card.args === 'object'
    && 'path' in card.args
    ? card.args.path
    : undefined;
  check('mixed assistant keeps visible blocks but removes toolCall block', assistant?.kind === 'assistant' && assistant.blocks.length === 1 && assistant.blocks[0]?.type === 'text', JSON.stringify(assistant));
  check('generic card metadata comes from matching assistant toolCall', card?.kind === 'toolCard' && card.toolCallId === 'generic-call' && card.toolName === 'read' && genericPath === 'fixture.txt', JSON.stringify(card));
  check('read card retains full upstream-bounded result', card?.kind === 'toolCard' && card.status === 'done' && card.result === output, JSON.stringify(card));
}


// ---- reconnect: whole-run streaming does not imply an assistant shell ----
check(
  'reconnect restores assistant after user tail',
  shouldRestoreStreamingAssistant([{ role: 'user', content: [] }]),
);
check(
  'reconnect restores assistant after toolResult tail',
  shouldRestoreStreamingAssistant([{ role: 'assistant', content: [] }, { role: 'toolResult', content: [] }]),
);
check(
  'reconnect does not create phantom assistant during tool stage',
  !shouldRestoreStreamingAssistant([{
    role: 'assistant',
    content: [{ type: 'toolCall', id: 'call', name: 'read', arguments: {} }],
  }]),
);

{
  const streaming = { kind: 'assistant' as const, id: 'msg-server', status: 'streaming' as const, blocks: [] };
  const midTurn = planReconnectTurn({
    isStreaming: true,
    liveItems: [streaming],
    activeAssistantId: 'msg-server',
  });
  check('mid-turn reconnect keeps the server assistant id',
    midTurn.kind === 'streaming' && midTurn.assistantId === 'msg-server');
  const noShell = planReconnectTurn({
    isStreaming: true,
    liveItems: [{ kind: 'user', id: 'u1', text: 'go', optimistic: false }],
    activeAssistantId: undefined,
  });
  check('mid-turn reconnect never mints a client-only assistant id',
    noShell.kind === 'streaming' && noShell.assistantId === null);
  const settled = planReconnectTurn({
    isStreaming: false,
    liveItems: [streaming],
    activeAssistantId: 'msg-server',
  });
  check('finished turn reconnect is settled history',
    settled.kind === 'settled');
  check('idle snapshot with a streaming assistant is a settled refill',
    planStreamStallReconcile({ snapshotStreaming: false, liveHasStreamingAssistant: true }) === 'refill-settled');
  check('still-streaming snapshot does not refill',
    planStreamStallReconcile({ snapshotStreaming: true, liveHasStreamingAssistant: true }) === 'idle');
  check('settled snapshot without a streaming assistant is idle',
    planStreamStallReconcile({ snapshotStreaming: false, liveHasStreamingAssistant: false }) === 'idle');
  check('omitted streaming flag does not refill',
    planStreamStallReconcile({ snapshotStreaming: undefined, liveHasStreamingAssistant: true }) === 'idle');
  check('watchdog settlement requests refill while another session reconnect-refills',
    planSettledStreamRefillRequest({
      snapshotStreaming: false,
      liveHasStreamingAssistant: true,
      reconnectRefillSessionId: 'alpha',
      targetSessionId: 'beta',
    }) === 'request');
  check('same-session reconnect refill owns the settled snapshot',
    planSettledStreamRefillRequest({
      snapshotStreaming: false,
      liveHasStreamingAssistant: true,
      reconnectRefillSessionId: 'alpha',
      targetSessionId: 'alpha',
    }) === 'idle');
  check('settled stall without a reconnect refill still requests',
    planSettledStreamRefillRequest({
      snapshotStreaming: false,
      liveHasStreamingAssistant: true,
      reconnectRefillSessionId: null,
      targetSessionId: 'beta',
    }) === 'request');
  check('still-streaming snapshot does not request a settled refill',
    planSettledStreamRefillRequest({
      snapshotStreaming: true,
      liveHasStreamingAssistant: true,
      reconnectRefillSessionId: 'alpha',
      targetSessionId: 'beta',
    }) === 'idle');
  check('watchdog SLA is one 5s heartbeat-stability window', STREAM_WATCHDOG_MS === 5000);
  check('watchdog stays idle while deltas are fresh',
    planStreamWatchdogTick({ streaming: true, lastEventAt: 1000, now: 1000 + STREAM_WATCHDOG_MS - 1, watchdogMs: STREAM_WATCHDOG_MS }) === 'idle');
  check('watchdog get_state after silence with streaming still on',
    planStreamWatchdogTick({ streaming: true, lastEventAt: 1000, now: 1000 + STREAM_WATCHDOG_MS, watchdogMs: STREAM_WATCHDOG_MS }) === 'get_state');
  check('watchdog stays idle when the turn already settled',
    planStreamWatchdogTick({ streaming: false, lastEventAt: 0, now: 1000 + STREAM_WATCHDOG_MS, watchdogMs: STREAM_WATCHDOG_MS }) === 'idle');
  check('two sessions tick independently from their own lastEventAt',
    planStreamWatchdogTick({ streaming: true, lastEventAt: 0, now: 9000, watchdogMs: STREAM_WATCHDOG_MS }) === 'get_state'
    && planStreamWatchdogTick({ streaming: true, lastEventAt: 8000, now: 9000, watchdogMs: STREAM_WATCHDOG_MS }) === 'idle');
  const twoTimers = { alpha: 1, beta: 2 };
  const afterAlpha = withoutSessionWatchdogTimer(twoTimers, 'alpha');
  check('dropping one session watchdog timer leaves the other',
    afterAlpha.beta === 2 && afterAlpha.alpha === undefined && twoTimers.alpha === 1);
  check('dropping a missing session watchdog timer does not delete siblings',
    withoutSessionWatchdogTimer({ beta: 2 }, 'alpha').beta === 2);
  {
    let now = 0;
    let nextTimerId = 1;
    const pending: { id: number; at: number; fn: () => void }[] = [];
    const streaming: Record<string, boolean> = { alpha: true, beta: true };
    const silenced: string[] = [];
    const watchdog = createStreamWatchdog({
      watchdogMs: STREAM_WATCHDOG_MS,
      now: () => now,
      schedule: (fn, ms) => {
        const id = nextTimerId;
        nextTimerId += 1;
        pending.push({ id, at: now + ms, fn });
        return id;
      },
      cancel: (id) => {
        const index = pending.findIndex((timer) => timer.id === id);
        if (index >= 0) pending.splice(index, 1);
      },
      isStreaming: (sid) => streaming[sid] === true,
      onSilence: (sid) => { silenced.push(sid); },
    });
    const fireDue = (): void => {
      pending.sort((left, right) => left.at - right.at);
      while (pending.length > 0 && pending[0].at <= now) {
        const timer = pending.shift();
        if (timer) timer.fn();
      }
    };
    watchdog.arm('alpha');
    now = 1000;
    watchdog.arm('beta');
    now = STREAM_WATCHDOG_MS;
    fireDue();
    check('watchdog get_state fires only for the silent session',
      silenced.length === 1 && silenced[0] === 'alpha' && pending.length === 1);
    streaming.alpha = false;
    watchdog.clear('alpha');
    now = 1000 + STREAM_WATCHDOG_MS;
    fireDue();
    check('clearing alpha leaves beta silence and does not re-fire alpha',
      silenced.length === 2 && silenced[1] === 'beta' && pending.length === 0);
    now += STREAM_WATCHDOG_MS;
    fireDue();
    check('invalidated alpha does not get_state after beta silence',
      silenced.length === 2 && silenced[0] === 'alpha' && silenced[1] === 'beta');
    streaming.gamma = true;
    watchdog.arm('gamma');
    now += STREAM_WATCHDOG_MS / 2;
    watchdog.arm('gamma');
    now += STREAM_WATCHDOG_MS / 2;
    fireDue();
    check('a live re-arm delays get_state past the first silence window',
      silenced.length === 2 && pending.length === 1);
    now += STREAM_WATCHDOG_MS / 2;
    fireDue();
    check('re-armed session get_state after a full silent window',
      silenced.length === 3 && silenced[2] === 'gamma' && pending.length === 0);
    streaming.delta = false;
    watchdog.arm('delta');
    now += STREAM_WATCHDOG_MS;
    fireDue();
    check('arm on a non-streaming session never schedules get_state',
      silenced.length === 3 && pending.length === 0);
    streaming.wake = true;
    now = 12_000;
    watchdog.arm('wake');
    now = 12_000 + STREAM_WATCHDOG_MS;
    watchdog.probe('wake');
    check('wake probe fires silence without waiting for the timer',
      silenced.length === 4 && silenced[3] === 'wake');
    now = 20_000;
    watchdog.arm('wake');
    now = 20_000 + STREAM_WATCHDOG_MS - 1;
    watchdog.probe('wake');
    check('wake probe before the 5s window does not fire',
      silenced.length === 4);
  }
  {
    const sid = 'stall-session';
    const working = applyActivityStripEvent(emptyActivityStripState(sid), { type: 'turn_started', turnId: 'turn-stall' }, sid);
    let stalled = false;
    check('live streaming has Working chrome, not Reconnecting',
      activityStripView(working, 'on', stalled)?.state === 'working'
      && planStreamStallChrome(stalled) === 'hidden');
    check('a quiet stretch alone never paints Reconnecting (subagent waits are healthy)',
      reduceStreamStall(false, { type: 'watchdog_probe_alive' }) === false);
    stalled = reduceStreamStall(stalled, { type: 'watchdog_probe_failed' });
    const silentPulse = activityStripView(working, 'on', stalled);
    check('a failed silence probe pulses Reconnecting while connState stays on',
      stalled === true
      && planStreamStallChrome(stalled) === 'pulse'
      && silentPulse?.state === 'reconnecting'
      && silentPulse?.text === 'Reconnecting…');
    check('the next successful probe clears the stall (probe answered = transport alive)',
      reduceStreamStall(stalled, { type: 'watchdog_probe_alive' }) === false);
    stalled = reduceStreamStall(stalled, { type: 'socket_drop', streaming: true });
    const dropPulse = activityStripView(working, 'reconnecting', stalled);
    check('socket drop keeps the Reconnecting pulse',
      stalled === true
      && dropPulse?.state === 'reconnecting'
      && planStreamStallChrome(stalled) === 'pulse');
    const reconnectOn = activityStripView(working, 'on', stalled);
    check('connState on during refill still pulses until refill finishes',
      reconnectOn?.state === 'reconnecting'
      && planStreamStallChrome(stalled) === 'pulse');
    stalled = reduceStreamStall(stalled, { type: 'refill_finished' });
    const settled = applyActivityStripEvent(working, { type: 'turn_terminal', turnId: 'turn-stall', status: 'succeeded' }, sid);
    check('refill finish clears pulse and the strip',
      stalled === false
      && planStreamStallChrome(stalled) === 'hidden'
      && activityStripView(settled, 'on', stalled) === null);
    stalled = reduceStreamStall(true, { type: 'live_event' });
    check('live tokens clear stall chrome',
      stalled === false && planStreamStallChrome(stalled) === 'hidden');
    stalled = reduceStreamStall(true, { type: 'abort' });
    check('abort clears stall chrome', stalled === false);
    check('idle snapshot without refill clears stall',
      reduceStreamStall(true, { type: 'snapshot_idle', refill: 'idle' }) === false);
    check('idle snapshot that requests refill keeps stall',
      reduceStreamStall(true, { type: 'snapshot_idle', refill: 'request' }) === true);
  }
  {
    const failedItems = [{
      kind: 'toolCard',
      id: 'tc-fail',
      toolCallId: 'task-fail-call',
      toolName: 'task',
      args: { context: '# Goal\nreview', tasks: [{ name: 'child-1', agent: 'glm', task: 'audit the realm' }] },
      status: 'running',
      result: '',
      details: [{ index: 0, jobId: 'job-f1', agentId: 'child-1', agent: 'glm', status: 'running' }],
    }];
    const afterFail = applyJobUpdated(failedItems, {
      job: {
        id: 'job-f1',
        agentId: 'child-1',
        agent: 'glm',
        status: 'failed',
        result: {
          output: '调查完成（只读）。以下逐条裁定……',
          error: 'subagent exited without calling yield tool',
        },
      },
    });
    const failedChild = resolveTaskCardView(afterFail[0].args, afterFail[0].details)?.children[0];
    check('failed child keeps its partial output as result',
      !!failedChild && failedChild.result === '调查完成（只读）。以下逐条裁定……', JSON.stringify(failedChild));
    check('failed child carries the failure reason separately',
      !!failedChild && failedChild.error === 'subagent exited without calling yield tool', JSON.stringify(failedChild));
    const failDom = renderToStaticMarkup(createElement(TaskToolCardBody, {
      item: afterFail[0],
      taskView: resolveTaskCardView(afterFail[0].args, afterFail[0].details),
    }));
    check('failed child renders a red error line, never FAILED with no explanation',
      failDom.includes('tool-card__child-error')
        && failDom.includes('subagent exited without calling yield tool'),
      failDom.slice(0, 500));
  }
  check('liveHasStreamingAssistant detects a streaming bubble',
    liveHasStreamingAssistant([streaming]) === true
      && liveHasStreamingAssistant([{ kind: 'assistant', id: 'done', status: 'final', blocks: [] }]) === false);
  check('in-order sequence applies', planLiveSequenceGap({ cursor: 4, sequence: 5 }) === 'apply');
  check('duplicate sequence is ignored', planLiveSequenceGap({ cursor: 7, sequence: 7 }) === 'duplicate');
  check('skipped sequence is a gap', planLiveSequenceGap({ cursor: 10, sequence: 12 }) === 'gap');
  check('first live gap starts catch-up', planLiveGapCatchUp({ recoveryInFlight: false }) === 'start');
  check('later live gaps coalesce onto the in-flight catch-up', planLiveGapCatchUp({ recoveryInFlight: true }) === 'coalesce');
  check('busy_capacity catch-up stays silent', planStreamCatchUpToast({
    toastOwned: false,
    transport: false,
    busyCapacity: true,
  }) === 'silent');
  check('owned or transport catch-up stays silent', planStreamCatchUpToast({
    toastOwned: true,
    transport: false,
    busyCapacity: false,
  }) === 'silent'
    && planStreamCatchUpToast({
      toastOwned: false,
      transport: true,
      busyCapacity: false,
    }) === 'silent');
  check('other catch-up failures still toast', planStreamCatchUpToast({
    toastOwned: false,
    transport: false,
    busyCapacity: false,
  }) === 'toast');
  check('catch-up merge uses snapshot streaming, not a stale local flag',
    planGapCatchUpMergeStreaming(false) === false
      && planGapCatchUpMergeStreaming(true) === true
      && planGapCatchUpMergeStreaming(undefined) === false);
  check('valid catch-up messages pass through',
    JSON.stringify(messagesFromCatchUpResponse({ messages: [{ role: 'user' }] }))
      === JSON.stringify([{ role: 'user' }]));
  let missingMessages = false;
  try { messagesFromCatchUpResponse({}); } catch (error) {
    missingMessages = error instanceof Error && error.message === 'messages response missing messages';
  }
  let nonArrayMessages = false;
  try { messagesFromCatchUpResponse({ messages: 'nope' }); } catch (error) {
    nonArrayMessages = error instanceof Error && error.message === 'messages response missing messages';
  }
  check('malformed catch-up messages fail closed', missingMessages && nonArrayMessages);
  check('capacity-like prose without typed rpcCode still toasts',
    planStreamCatchUpToast({
      toastOwned: false,
      transport: false,
      busyCapacity: isBusyCapacityError(new Error('too many concurrent RPC commands (limit 16)')),
    }) === 'toast');
}

// ---- restored: display:false custom (raw system-reminder) is hidden ----
{
  const items = messagesToItems([
    {
      role: 'custom',
      customType: 'loop_scheduled_turn',
      content: '<system-reminder>\ninternal scaffolding\n</system-reminder>\n\necho hello',
      display: false,
      details: { taskId: 'abc', prompt: 'echo hello', schedule: 'every 5 minutes' },
      timestamp: 5,
    },
  ]);
  check('restored display:false custom is absent', items.length === 0, JSON.stringify(items));
}

// ---- restored: loop scheduled turn (post public_message) renders the clean prompt ----
{
  const items = messagesToItems([
    {
      role: 'custom',
      customType: 'Loop abc \u00b7 every 5 minutes',
      content: 'echo hello',
      display: true,
      details: { taskId: 'abc', prompt: 'echo hello', schedule: 'every 5 minutes' },
      timestamp: 6,
    },
  ]);
  const custom = items.find((i) => i.kind === 'custom');
  check('loop turn renders the friendly label', custom && custom.label === 'Loop abc \u00b7 every 5 minutes', JSON.stringify(custom));
  check('loop turn renders the clean prompt', custom && custom.text === 'echo hello', JSON.stringify(custom));
  check('loop turn has no system-reminder', custom && !custom.text.includes('system-reminder'), JSON.stringify(custom));
}

// ---- restored: orchestration IRC restores as a typed irc item, never XML ----
{
  const items = messagesToItems([
    {
      role: 'custom',
      customType: 'orchestration_message',
      content: '<orchestration-message id="m1" from="Main">\nhello child\nReplying to message: parent-9\n</orchestration-message>',
      display: true,
      details: { id: 'm1', from: 'Main', to: 'Child', body: 'hello child', replyTo: 'parent-9' },
      timestamp: 7,
    },
  ]);
  const irc = items.find((i) => i.kind === 'irc');
  check('IRC restores as a typed irc item (not kind custom)', irc !== undefined && irc.kind === 'irc', JSON.stringify(irc));
  if (irc && irc.kind === 'irc') {
    check('IRC outgoing direction + parties (Main → Child)', irc.direction === 'outgoing' && irc.from === 'Main' && irc.to === 'Child', JSON.stringify(irc));
    check('IRC renders the clean body', irc.body === 'hello child', JSON.stringify(irc));
    check('IRC replyTo is independent typed metadata', irc.replyTo === 'parent-9' && !irc.body.includes('reply to'), JSON.stringify(irc));
    check('IRC never renders the raw XML wrapper', !irc.body.includes('<orchestration-message') && !irc.body.includes('Replying to message'), JSON.stringify(irc));
  }
}

// ---- restored: IRC without details.body falls back to stripping the wrapper ----
{
  const view = orchestrationIrcView({
    customType: 'orchestration_message',
    content: '<orchestration-message id="m2" from="A">\nbody text\nReplying to message: p1\n</orchestration-message>',
    details: { id: 'm2', from: 'A', to: 'B' },
  });
  check('IRC fallback strips the wrapper and reply line', view && view.from === 'A' && view.to === 'B' && view.body === 'body text', JSON.stringify(view));
  // The fallback must NOT guess reply metadata from the body (typed-details
  // driven only): the `Replying to message: p1` trailer is stripped, never
  // promoted to replyTo.
  check('IRC fallback never guesses replyTo from the body', view !== null && view.replyTo === undefined, JSON.stringify(view));
}

// ---- restored: non-IRC custom with display:false never leaks its content ----
{
  const items = messagesToItems([
    { role: 'custom', customType: 'pi.goal.active', content: '<system-reminder>\nactive goal\n</system-reminder>', display: false, details: {}, timestamp: 8 },
  ]);
  check('restored active-goal (display:false) is absent', items.length === 0, JSON.stringify(items));
}

// ---- live/customToItem parity: identical visibility to messagesToItems ----
{
  const hidden = customToItem({ role: 'custom', customType: 'loop_scheduled_turn', content: '<system-reminder>x</system-reminder>', display: false });
  check('customToItem hides display:false', hidden === null, JSON.stringify(hidden));
  const irc = customToItem({
    role: 'custom',
    customType: 'orchestration_message',
    content: '<orchestration-message id="m1" from="Main">\nhi\n</orchestration-message>',
    display: true,
    details: { id: 'm1', from: 'Main', to: 'Child', body: 'hi', replyTo: 'p-1' },
  });
  check('customToItem returns a typed irc item', irc !== null && irc.kind === 'irc', JSON.stringify(irc));
  if (irc && irc.kind === 'irc') {
    check('customToItem IRC keeps direction/parties/body', irc.direction === 'outgoing' && irc.from === 'Main' && irc.to === 'Child' && irc.body === 'hi', JSON.stringify(irc));
    check('customToItem IRC keeps replyTo independent of body', irc.replyTo === 'p-1' && !irc.body.includes('reply to'), JSON.stringify(irc));
    check('customToItem IRC never leaks the XML wrapper', !irc.body.includes('<orchestration-message'), JSON.stringify(irc));
  }
  // A display:true NON-IRC custom must stay a plain labeled custom — the
  // typed IRC classification never mislabels ordinary notices.
  const plain = customToItem({ role: 'custom', customType: 'loop_scheduled_turn', content: 'echo hello', display: true, details: {} });
  check('ordinary display:true custom stays kind custom', plain !== null && plain.kind === 'custom' && plain.label === 'loop_scheduled_turn' && plain.text === 'echo hello', JSON.stringify(plain));
}

// ---- typed IRC direction + title vocabulary (TUI label mirror) ----
{
  check('incoming child→Main is incoming + title IRC ← Child', ircDirection('Child', 'Main') === 'incoming' && ircTitle('Child', 'Main') === 'IRC ← Child');
  check('outgoing Main→Child is outgoing + title IRC → Child', ircDirection('Main', 'Child') === 'outgoing' && ircTitle('Main', 'Child') === 'IRC → Child');
  check('child→child transit keeps both parties', ircDirection('A', 'B') === 'outgoing' && ircTitle('A', 'B') === 'IRC A → B');
}

// ---- ircProjection: the shared hub-card/IRC-row typed parser ----
{
  const typed = ircProjection({ id: 'm-1', from: 'Main', to: 'Child', body: 'hello child', replyTo: 'parent-9' });
  check('ircProjection reads typed fields', typed !== null && typed.from === 'Main' && typed.to === 'Child' && typed.body === 'hello child' && typed.replyTo === 'parent-9', JSON.stringify(typed));
  check('ircProjection rejects null (wait timeout)', ircProjection(null) === null);
  check('ircProjection rejects missing from', ircProjection({ id: 'x' }) === null);
  check('ircProjection rejects empty id', ircProjection({ id: '', from: 'Main' }) === null);
  const noReply = ircProjection({ id: 'm-2', from: 'Alpha', to: 'Beta', body: 'hi', replyTo: '' });
  check('ircProjection drops empty replyTo', noReply !== null && noReply.replyTo === undefined, JSON.stringify(noReply));
  // Typed fields only — a body that merely CONTAINS `reply to` prose never
  // derives replyTo metadata from the text (typed-details driven).
  const prose = ircProjection({ id: 'm-3', from: 'Main', to: 'Child', body: 'reply to X\nstarts_with reply to' });
  check('ircProjection never derives replyTo from prose', prose !== null && prose.replyTo === undefined && prose.body.includes('reply to X'), JSON.stringify(prose));
}

// ---- IRC body bounding: 40-line parse bound, 6-line compact default ----
{
  check('IRC_BODY_LINE_LIMIT == 40', IRC_BODY_LINE_LIMIT === 40);
  check('IRC_COMPACT_LINE_LIMIT == 6', IRC_COMPACT_LINE_LIMIT === 6);
  const long = Array.from({ length: 45 }, (_, i) => `line-${i + 1}`).join('\n');
  const bounded = boundIrcBody(long);
  check('boundIrcBody keeps the head + hint', bounded.startsWith('line-1\n') && bounded.endsWith('\u2026 5 more lines') && bounded.split('\n').length === 41, bounded);
  check('boundIrcBody passes short bodies through', boundIrcBody('short body') === 'short body');
  const item = customToItem({
    role: 'custom',
    customType: 'orchestration_message',
    display: true,
    details: { id: 'm9', from: 'Child', to: 'Main', body: long },
  });
  check('customToItem bounds the irc body at parse time', item !== null && item.kind === 'irc' && item.body.endsWith('\u2026 5 more lines'), JSON.stringify(item));
}

// ---- live/toolResult bounding parity (same helper) ----
{
  const twelve = Array.from({ length: 12 }, (_, i) => String(i + 1)).join('\n');
  const bounded = boundOutput(twelve, TOOL_OUTPUT_LINE_LIMIT).text;
  check('live toolResult bound identically (last 6 + hint)', bounded.endsWith('\n12') && bounded.startsWith('\u2026 6 more lines\n'), bounded);
  const bash30 = boundOutput(Array.from({ length: 30 }, (_, i) => String(i + 1)).join('\n'), BASH_OUTPUT_LINE_LIMIT).text;
  check('live bash bound identically (last 10 + hint)', bash30.endsWith('\n30') && bash30.startsWith('\u2026 20 more lines\n'), bash30);
}

// ---- toolResult/toolCard dedupe parity (host suppression; collab parity) ----
{
  const doneCard = {
    kind: 'toolCard', id: 'tc', toolCallId: 'call-x', toolName: 'edit',
    args: { path: 'a' }, status: 'done', result: 'from end', details: { diff: 'x' },
  };
  const matchedNoDetails = applyToolResultToItems([doneCard], 'call-x', 'from message', undefined);
  check('matched card with no details stays untouched (no toolResult row)', matchedNoDetails.length === 1 && matchedNoDetails[0].kind === 'toolCard' && matchedNoDetails[0].result === 'from end', JSON.stringify(matchedNoDetails));
  const matchedDetails = applyToolResultToItems([doneCard], 'call-x', 'from message', { diff: 'new' });
  check('fold never overwrites existing card details/result', matchedDetails[0].kind === 'toolCard' && matchedDetails[0].details.diff === 'x' && matchedDetails[0].result === 'from end' && matchedDetails.length === 1, JSON.stringify(matchedDetails));

  const bareCard = {
    kind: 'toolCard', id: 'tc2', toolCallId: 'call-y', toolName: 'edit',
    args: { path: 'b' }, status: 'running', result: '',
  };
  const folded = applyToolResultToItems([bareCard], 'call-y', 'the result', { diff: 'd' });
  check('matched running card folds details + fills empty result', folded[0].kind === 'toolCard' && folded[0].details && folded[0].details.diff === 'd' && folded[0].result === 'the result' && folded.length === 1, JSON.stringify(folded));

  const unmatched = applyToolResultToItems([doneCard], 'other-call', 'orphan output', undefined);
  check('unmatched toolResult stays readable as a toolResult row', unmatched.length === 2 && unmatched[1].kind === 'toolResult' && unmatched[1].text === 'orphan output', JSON.stringify(unmatched));
  const emptyId = applyToolResultToItems([doneCard], '', 'no id', undefined);
  check('empty toolCallId appends a readable row (never drops output)', emptyId.length === 2 && emptyId[1].kind === 'toolResult' && emptyId[1].text === 'no id', JSON.stringify(emptyId));
  const distinctCallIds = applyToolResultToItems(
    applyToolResultToItems([doneCard], 'orphan-a', 'identical output', undefined),
    'orphan-b',
    'identical output',
    undefined,
  );
  check(
    'distinct unmatched toolCallIds preserve identical result text separately',
    distinctCallIds.filter((item) => item.kind === 'toolResult' && item.text === 'identical output').length === 2,
    JSON.stringify(distinctCallIds),
  );
  const duplicateCallId = applyToolResultToItems(distinctCallIds, 'orphan-a', 'changed duplicate frame', undefined);
  check(
    'same unmatched toolCallId remains idempotent regardless of result text',
    duplicateCallId.length === distinctCallIds.length,
    JSON.stringify(duplicateCallId),
  );
  const emptyIds = applyToolResultToItems(
    applyToolResultToItems([doneCard], '', 'identical empty-id output', undefined),
    '',
    'identical empty-id output',
    undefined,
  );
  check(
    'empty toolCallIds never collapse identical result text',
    emptyIds.filter((item) => item.kind === 'toolResult' && item.text === 'identical empty-id output').length === 2,
    JSON.stringify(emptyIds),
  );

  const readCard = {
    kind: 'toolCard', id: 'tc3', toolCallId: 'call-r', toolName: 'read',
    args: { path: 'c' }, status: 'running', result: '',
  };
  const foldedRead = applyToolResultToItems([readCard], 'call-r', 'the result', { media: [{ kind: 'video' }] });
  check('read fold strips generic details', foldedRead[0].kind === 'toolCard' && foldedRead[0].details === undefined && foldedRead[0].result === 'the result', JSON.stringify(foldedRead));
}

// ---- tool result media: safe image/video projection, hostile rejection ----
{
  const png = 'iVBORw0KGgo=';
  const image = toolMedia([{ type: 'image', mimeType: 'image/png', data: png }]);
  check('image ContentBlock survives tool projection', image.length === 1 && image[0].kind === 'image' && image[0].mimeType === 'image/png', JSON.stringify(image));

  const video = toolMedia([], {
    media: [{ kind: 'video', mimeType: 'video/webm', data: 'GkXfo0FCQw==', name: 'capture.webm', sizeBytes: 7 }],
  });
  check('bounded video details survive tool projection', video.length === 1 && video[0].kind === 'video' && video[0].alt === 'capture.webm', JSON.stringify(video));

  const hostile = toolMedia(
    [{ type: 'image', mimeType: 'image/svg+xml', data: png }],
    { media: [
      { kind: 'video', mimeType: 'text/html', data: 'PGgxPg==', name: 'x', sizeBytes: 4 },
      { kind: 'video', mimeType: 'video/webm', data: 'not base64', name: 'x', sizeBytes: 4 },
      { kind: 'video', mimeType: 'video/webm', data: 'GkXfo0FCQw==', name: 'x', sizeBytes: 3 * 1024 * 1024 },
    ] },
  );
  check('hostile MIME/base64/oversize media is rejected', hostile.length === 0, JSON.stringify(hostile));

  const card = { kind: 'toolCard', id: 'media', toolCallId: 'media-call', toolName: 'read', args: { path: 'image.png' }, status: 'running', result: '' };
  const completed = applyToolSnapshot([card], 'media-call', 'Read image', 'done', {}, image)[0];
  check('live read end retains validated image media', completed.kind === 'toolCard'
    && completed.media?.length === 1 && completed.media[0].kind === 'image'
    && completed.result === 'Read image', JSON.stringify(completed));

  const restored = messagesToItems([
    { role: 'assistant', content: [{ type: 'toolCall', id: 'media-restored', name: 'read', arguments: { path: 'image.png' } }] },
    { role: 'toolResult', toolCallId: 'media-restored', content: [{ type: 'text', text: 'Read image' }, { type: 'image', mimeType: 'image/png', data: png }], details: {}, isError: false },
  ]);
  const restoredCard = restored.find((item) => item.kind === 'toolCard');
  check('restored read result retains validated image media', restoredCard?.kind === 'toolCard'
    && restoredCard.media?.length === 1 && restoredCard.media[0].mimeType === 'image/png'
    && restoredCard.result === 'Read image', JSON.stringify(restoredCard));
}

// ---- user message images: extraction, allowlist, order, restore ----
{
  const png = 'iVBORw0KGgo=';
  const webp = 'UklGRi4AAABXRUJQVlA4ICIAAAAQAA==';
  const gif = 'R0lGODlhAQABAIAAAAUEBA==';
  const jpeg = '/9j/4AAQSkZJRg==';

  // Order preserved; text-only blocks ignored; every allowlisted MIME passes.
  const images = userImages([
    { type: 'text', text: 'look at this' },
    { type: 'image', mimeType: 'image/png', data: png },
    { type: 'image', mimeType: 'image/webp', data: webp },
    { type: 'image', mimeType: 'image/gif', data: gif },
    { type: 'image', mimeType: 'image/jpeg', data: jpeg },
  ]);
  check(
    'userImages preserves block order and allowlisted MIMEs',
    images.length === 4
      && images[0].mimeType === 'image/png' && images[0].data === png
      && images[1].mimeType === 'image/webp'
      && images[2].mimeType === 'image/gif'
      && images[3].mimeType === 'image/jpeg',
    JSON.stringify(images),
  );

  // Hostile blocks: wrong type, non-allowlisted MIME, invalid base64 charset,
  // wrong base64 length, oversize payload — all rejected, never rendered.
  const hostile = userImages([
    { type: 'text', text: 'hello' },
    { type: 'image', mimeType: 'image/svg+xml', data: png },
    { type: 'image', mimeType: 'image/bmp', data: png },
    { type: 'image', mimeType: 'image/png', data: 'not base64!' },
    { type: 'image', mimeType: 'image/png', data: 'AAAAA' },
    { type: 'image', mimeType: 'image/png', data: 'A'.repeat(3 * 1024 * 1024 + 4) },
    { type: 'image', mimeType: 'image/png' },
    { type: 'video', mimeType: 'image/png', data: png },
  ]);
  check('hostile user image blocks are rejected (MIME/base64/size/type)', hostile.length === 0, JSON.stringify(hostile));
  check('oversize at the exact 3 MiB cap is still accepted', userImages([{ type: 'image', mimeType: 'image/png', data: 'A'.repeat(3 * 1024 * 1024) }]).length === 1);
  check('userImages(non-array) returns []', userImages(null).length === 0 && userImages('str').length === 0);
}

// ---- restored: user message with image blocks renders images in order ----
{
  const png = 'iVBORw0KGgo=';
  const webp = 'UklGRi4AAABXRUJQVlA4ICIAAAAQAA==';
  const items = messagesToItems([
    { role: 'user', content: [{ type: 'text', text: 'look at this' }, { type: 'image', mimeType: 'image/png', data: png }, { type: 'image', mimeType: 'image/webp', data: webp }] },
    { role: 'user', content: [{ type: 'text', text: 'plain only' }] },
    { role: 'user', content: [{ type: 'image', mimeType: 'image/svg+xml', data: png }, { type: 'text', text: 'hostile image dropped' }] },
  ]);
  const withImages = items[0];
  const plain = items[1];
  const hostile = items[2];
  check(
    'restored user item carries text + images in block order',
    withImages.kind === 'user'
      && withImages.text === 'look at this'
      && (withImages.images?.length ?? 0) === 2
      && withImages.images?.[0]?.mimeType === 'image/png'
      && withImages.images?.[0]?.data === png
      && withImages.images?.[1]?.mimeType === 'image/webp',
    JSON.stringify(withImages),
  );
  check('restored text-only user item has no images field', plain.kind === 'user' && plain.images === undefined, JSON.stringify(plain));
  check('restored hostile image block is dropped, text kept', hostile.kind === 'user' && hostile.text === 'hostile image dropped' && hostile.images === undefined, JSON.stringify(hostile));
}

// ---- reconcile identity: images participate, empty-text multi-image never collapses ----
{
  const imgA = 'iVBORw0KGgo=';
  const imgB = 'aGVsbG8=';
  const imgC = 'UklGRi4AAABXRUJQVlA4ICIAAAAQA';

  // 1. An optimistic image-only bubble whose durable twin exists in history
  //    dedups to exactly one bubble (ACK/reconnect parity).
  const deduped = mergeAuthoritativeItems(
    [{ kind: 'user', id: 'h-a', text: '', optimistic: false, images: [{ mimeType: 'image/png', data: imgA }] }],
    [{ kind: 'user', id: 'l-a', text: '', optimistic: true, images: [{ mimeType: 'image/png', data: imgA }] }],
  );
  check(
    'image-only optimistic bubble dedups against its durable twin',
    deduped.filter((item) => item.kind === 'user').length === 1 && !deduped.some((item) => item.id === 'l-a'),
    JSON.stringify(deduped),
  );

  // 2. THE reported bug: history already holds OTHER image-only sends (empty
  //    text), and a NEW different-image send is still optimistic. Text-only
  //    identity would drop the in-flight bubble (surplus on ''); image-aware
  //    identity preserves it because imgC is not in history.
  const preserved = mergeAuthoritativeItems(
    [
      { kind: 'user', id: 'h-a', text: '', optimistic: false, images: [{ mimeType: 'image/png', data: imgA }] },
      { kind: 'user', id: 'h-b', text: '', optimistic: false, images: [{ mimeType: 'image/png', data: imgB }] },
    ],
    [{ kind: 'user', id: 'l-c', text: '', optimistic: true, images: [{ mimeType: 'image/png', data: imgC }] }],
  );
  check(
    'distinct in-flight image-only bubble survives other image-only history',
    preserved.filter((item) => item.kind === 'user').length === 3 && preserved.some((item) => item.id === 'l-c'),
    JSON.stringify(preserved),
  );

  // 3. Text + image identity: a text+image bubble dedups against its durable
  //    twin (same text AND same image) while a same-text-different-image
  //    message stays separate.
  const mixed = mergeAuthoritativeItems(
    [{ kind: 'user', id: 'h-mixed', text: 'same', optimistic: false, images: [{ mimeType: 'image/png', data: imgA }] }],
    [
      { kind: 'user', id: 'l-mixed', text: 'same', optimistic: true, images: [{ mimeType: 'image/png', data: imgA }] },
      { kind: 'user', id: 'l-other', text: 'same', optimistic: true, images: [{ mimeType: 'image/png', data: imgB }] },
    ],
  );
  check(
    'same-text different-image sends are distinct identities',
    mixed.filter((item) => item.kind === 'user' && item.text === 'same').length === 2
      && !mixed.some((item) => item.id === 'l-mixed')
      && mixed.some((item) => item.id === 'l-other'),
    JSON.stringify(mixed),
  );

  // 4. Both in-flight image-only sends whose durable twins exist both dedup
  //    (occurrence counts preserved per identity).
  const both = mergeAuthoritativeItems(
    [
      { kind: 'user', id: 'h-a', text: '', optimistic: false, images: [{ mimeType: 'image/png', data: imgA }] },
      { kind: 'user', id: 'h-b', text: '', optimistic: false, images: [{ mimeType: 'image/png', data: imgB }] },
    ],
    [
      { kind: 'user', id: 'l-a', text: '', optimistic: true, images: [{ mimeType: 'image/png', data: imgA }] },
      { kind: 'user', id: 'l-b', text: '', optimistic: true, images: [{ mimeType: 'image/png', data: imgB }] },
    ],
  );
  check(
    'both image-only optimistic bubbles dedup against their twins',
    both.filter((item) => item.kind === 'user').length === 2
      && !both.some((item) => item.id === 'l-a' || item.id === 'l-b'),
    JSON.stringify(both),
  );
}

// ---- run settle/failure hides live thinking while preserving streamed text ----
{
  const streaming = [{
    kind: 'assistant', id: 'live-a', status: 'streaming', blocks: [],
  }];
  const finalized = finalizeStreamingAssistant(streaming, 'live-a', 'partial visible answer');
  check(
    'settle finalizes streaming assistant and preserves visible text',
    finalized[0].kind === 'assistant'
      && finalized[0].status === 'final'
      && finalized[0].blocks.length === 1
      && finalized[0].blocks[0].type === 'text'
      && finalized[0].blocks[0].text === 'partial visible answer',
    JSON.stringify(finalized),
  );
  const durable = [{
    kind: 'assistant', id: 'live-b', status: 'streaming', blocks: [{ type: 'text', text: 'authoritative' }],
  }];
  const preserved = finalizeStreamingAssistant(durable, 'live-b', 'stale buffer');
  check(
    'settle keeps authoritative assistant blocks over stale stream buffer',
    preserved[0].kind === 'assistant'
      && preserved[0].status === 'final'
      && preserved[0].blocks[0].type === 'text'
      && preserved[0].blocks[0].text === 'authoritative',
    JSON.stringify(preserved),
  );
}

// ---- thinking newline normalization: conservative literal \n handling ----
// The live delta path (App.tsx applyDeltaToNode) and the final markdown path
// (markdown.ts renderBlocks) share these rules, so both must agree on every
// case below. A literal `\n` here means the TWO characters backslash+n.
{
  check('thinking normalize leaves plain text untouched', normalizeThinkingNewlines('reasoning step one') === 'reasoning step one');
  check('thinking normalize leaves real newlines untouched', normalizeThinkingNewlines('line one\nline two') === 'line one\nline two');
  check('thinking normalize leaves a single literal \\n (code) untouched', normalizeThinkingNewlines('print("a\\nb")') === 'print("a\\nb")');
  check('thinking normalize converts multiple literal \\n to real newlines', normalizeThinkingNewlines('a\\nb\\nc') === 'a\nb\nc');
  check('thinking normalize is identity when real newlines coexist with literal \\n', normalizeThinkingNewlines('a\nb\\nc\\nd') === 'a\nb\\nc\\nd');
  check('thinking normalize is idempotent', normalizeThinkingNewlines(normalizeThinkingNewlines('a\\nb\\nc')) === 'a\nb\nc');
  check('thinking normalize handles CRLF as already multi-line', normalizeThinkingNewlines('a\r\nb\\nc') === 'a\r\nb\\nc');
  check('thinking shouldNormalize false for empty', !shouldNormalizeThinkingNewlines(''));
  check('thinking shouldNormalize false for one literal \\n', !shouldNormalizeThinkingNewlines('a\\nb'));
  check('thinking shouldNormalize true for two literal \\n separators', shouldNormalizeThinkingNewlines('a\\nb\\nc'));
  check('thinking shouldNormalize true for \\n\\n (adjacent separators)', shouldNormalizeThinkingNewlines('a\\n\\nb'));
  check('thinking shouldNormalize false when a real newline exists', !shouldNormalizeThinkingNewlines('a\nb\\nc\\nd'));
  check('thinking unescape replaces every literal \\n', unescapeThinkingNewlines('a\\nb\\nc') === 'a\nb\nc');
  check('thinking unescape leaves plain text alone', unescapeThinkingNewlines('plain') === 'plain');
}

// ---- Task multi-child payload: Goal/Constraints/Contract + children ----
{
  const args = {
    context: '# Goal\nShip the Task card\n\n# Constraints\nBe precise\n\n# Contract\nKeep stable ids',
    tasks: [
      { name: 'Alpha', agent: 'reviewer', task: 'Review the adapter thoroughly with acceptance criteria.' },
      { name: 'Beta', task: 'Render the card with status and result.' },
    ],
  };
  const spawns = [
    { index: 0, jobId: 'job-a', agentId: 'Alpha', agent: 'runtime-reviewer', status: 'queued' },
    { index: 1, jobId: 'job-b', agentId: 'Beta', agent: 'task', status: 'queued' },
  ];
  const view = resolveTaskCardView(args, spawns);
  check('task multi payload parses two children', !!view && view.children.length === 2, JSON.stringify(view));
  check('task Goal section from context heading', !!view && view.sections.goal.includes('Ship the Task card'), JSON.stringify(view?.sections));
  check('task Constraints section from context heading', !!view && view.sections.constraints.includes('Be precise'), JSON.stringify(view?.sections));
  check('task Contract section from context heading', !!view && view.sections.contract.includes('Keep stable ids'), JSON.stringify(view?.sections));
  check('task child name/agent/target', !!view && view.children[0].name === 'Alpha' && view.children[0].agent === 'reviewer' && view.children[0].target.includes('Review the adapter'), JSON.stringify(view?.children?.[0]));
  check('task spawn details retain jobId/agentId', !!view && view.children[0].jobId === 'job-a' && view.children[0].agentId === 'Alpha' && view.children[0].status === 'queued', JSON.stringify(view?.children?.[0]));

  const items = [{
    kind: 'toolCard',
    id: 'tc',
    toolCallId: 'task-call',
    toolName: 'task',
    args,
    status: 'done',
    result: '[0] Alpha (reviewer) — queued as job job-a',
    details: spawns,
  }];
  const afterJob = applyJobUpdated(items, {
    job: {
      id: 'job-a',
      agentId: 'Alpha',
      agent: 'reviewer',
      parentId: 'Main',
      todoTaskId: 'todo-a',
      status: 'completed',
      result: { output: 'Adapter review looks good.', error: null },
    },
  });
  const afterMsg = applyMessageDelivered(afterJob, {
    message: { from: 'Alpha', body: 'halfway through review', to: 'Main' },
  });
  const live = resolveTaskCardView(afterMsg[0].args, afterMsg[0].details);
  check('task child result sentence from job_updated', !!live && live.children[0].result === 'Adapter review looks good.', JSON.stringify(live?.children?.[0]));
  check('task child activity from message_delivered', !!live && live.children[0].activity === 'halfway through review', JSON.stringify(live?.children?.[0]));
  check('task child status updated from job', !!live && live.children[0].status === 'completed', JSON.stringify(live?.children?.[0]));
  check('task child projects authoritative parent and Todo provenance', !!live
    && live.children[0].parentId === 'Main'
    && live.children[0].todoTaskId === 'todo-a', JSON.stringify(live?.children?.[0]));
  check('live Task snapshot preserves authored name/agent/target', !!live
    && live.children[0].name === 'Alpha'
    && live.children[0].agent === 'reviewer'
    && live.children[0].target.includes('Review the adapter'), JSON.stringify(live?.children?.[0]));
  const allSettled = applyJobUpdated(afterJob, {
    job: { id: 'job-b', agentId: 'Beta', agent: 'task', status: 'completed' },
  });
  check('task parent settles when every authoritative child settles', taskCardStatus(allSettled[0]) === 'done', JSON.stringify(allSettled[0]));
  const restoredSettled = applyJobSnapshot(items, [{
    id: 'job-a',
    agentId: 'Alpha',
    agent: 'reviewer',
    status: 'completed',
    result: { output: 'restored output', error: null },
  }, {
    id: 'job-b',
    agentId: 'Beta',
    agent: 'task',
    status: 'completed',
  }]);
  const restoredView = resolveTaskCardView(restoredSettled[0].args, restoredSettled[0].details);
  check('task job_list snapshot reconciles restored queued children', !!restoredView
    && restoredView.children.every((child) => child.status === 'completed')
    && restoredView.children[0].result === 'restored output'
    && taskCardStatus(restoredSettled[0]) === 'done', JSON.stringify(restoredView));
  const runningBeforeReconnect = applyJobUpdated(items, {
    job: { id: 'job-a', agentId: 'Alpha', agent: 'reviewer', status: 'running' },
  });
  const cancelledOnReconnect = applyJobSnapshot(runningBeforeReconnect, [{
    id: 'job-a',
    agentId: 'Alpha',
    agent: 'reviewer',
    status: 'cancelled',
    result: { output: '', error: 'task cancelled' },
  }, {
    id: 'job-b',
    agentId: 'Beta',
    agent: 'task',
    status: 'completed',
  }]);
  const cancelledView = resolveTaskCardView(cancelledOnReconnect[0].args, cancelledOnReconnect[0].details);
  check('reconnect job_list removes stale Running child state', !!cancelledView
    && cancelledView.children[0].status === 'cancelled'
    && cancelledView.children[0].result === 'task cancelled'
    && taskCardStatus(cancelledOnReconnect[0]) === 'cancelled', JSON.stringify(cancelledView));
  const staleRunningSnapshot = applyJobSnapshot(cancelledOnReconnect, [{
    id: 'job-a',
    agentId: 'Alpha',
    agent: 'reviewer',
    status: 'running',
  }]);
  const monotonicView = resolveTaskCardView(staleRunningSnapshot[0].args, staleRunningSnapshot[0].details);
  check('stale reconnect snapshot cannot regress terminal Task child to Running', !!monotonicView
    && monotonicView.children[0].status === 'cancelled'
    && taskCardStatus(staleRunningSnapshot[0]) === 'cancelled', JSON.stringify(monotonicView));
  const mixedRunning = applyJobUpdated(items, {
    job: { id: 'job-a', agentId: 'Alpha', agent: 'reviewer', status: 'completed' },
  });
  check('task parent remains running while another authoritative child is queued', taskCardStatus(mixedRunning[0]) === 'running', JSON.stringify(mixedRunning[0]));
  const failed = applyJobUpdated(items, {
    job: { id: 'job-a', agentId: 'Alpha', agent: 'reviewer', status: 'failed' },
  });
  const failedSettled = applyJobUpdated(failed, {
    job: { id: 'job-b', agentId: 'Beta', agent: 'task', status: 'completed' },
  });
  check('task parent errors when any settled child failed', taskCardStatus(failedSettled[0]) === 'error', JSON.stringify(failedSettled[0]));

  const completedAndCancelled = applyJobUpdated(afterJob, {
    job: { id: 'job-b', agentId: 'Beta', agent: 'task', status: 'cancelled' },
  });
  check('task parent cancellation is explicit for completed plus cancelled children', taskCardStatus(completedAndCancelled[0]) === 'cancelled', JSON.stringify(completedAndCancelled[0]));

  const renamedWithoutStatusMutation = applyAgentUpdated(runningBeforeReconnect, {
    agent: { id: 'Alpha', displayName: 'Alpha revived', status: 'idle' },
  });
  const renamed = resolveTaskCardView(renamedWithoutStatusMutation[0].args, renamedWithoutStatusMutation[0].details);
  check('agent_updated changes Task display name without settling its job',
    renamed?.children[0].name === 'Alpha revived' && renamed.children[0].status === 'running',
    JSON.stringify(renamed?.children[0]));

  const exactInvalidArgs = {
    name: 'final-rollback-review',
    agent: 'reviewer',
    context: 'Review the complete rollback design against source.',
    task: 'Perform the meticulous final review with exact evidence and corrections.',
    outputSchema: {
      type: 'object',
      properties: { findings: { type: 'array', items: { type: 'object' } } },
      required: ['findings', 'summary'],
      summary: { type: 'string' },
    },
    schemaMode: 'strict',
  };
  const exactError = 'task.outputSchema required property "summary" is missing from $.properties';
  const rejected = { ...items[0], args: exactInvalidArgs, status: 'error', result: exactError, details: undefined };
  const rejectedView = resolveTaskCardView(rejected.args, rejected.details, rejected.status);
  check('exact invalid Goal Task hides args-only running reviewer', !!rejectedView && rejectedView.children.length === 0, JSON.stringify(rejectedView));
  check('exact invalid Goal Task retains actionable preflight error', rejected.result === exactError && boundOutput(rejected.result, 6).text === rejected.result, rejected.result);
  const partialRejected = { ...rejected, details: [spawns[0]] };
  const partialView = resolveTaskCardView(partialRejected.args, partialRejected.details, partialRejected.status);
  check('task rejection retains only authoritative partial spawn evidence', !!partialView && partialView.children.length === 1 && partialView.children[0].jobId === 'job-a', JSON.stringify(partialView));
  const rejectedDom = {
    kind: 'toolCard' as const,
    id: 'task-rejected',
    toolCallId: 'task-rejected-call',
    toolName: 'task',
    args: exactInvalidArgs,
    status: 'error' as const,
    result: exactError,
  };
  const taskView = resolveTaskCardView(rejectedDom.args, undefined, rejectedDom.status);
  const html = taskView ? renderToStaticMarkup(createElement(TaskToolCardBody, { item: rejectedDom, taskView })) : '';
  check('rejected Task DOM renders exact bounded schema error', html.includes('task.outputSchema required property &quot;summary&quot; is missing from $.properties'), html);
  check('rejected Task DOM renders zero reviewer child rows', !html.includes('class="tool-card__child"'), html);
  const previewView = resolveTaskCardView(rejectedDom.args, undefined);
  check('preview view still carries args-only children before parent status', !!previewView && previewView.children.length === 1, JSON.stringify(previewView));
  const settledHtml = previewView
    ? renderToStaticMarkup(createElement(TaskToolCardBody, { item: rejectedDom, taskView: previewView }))
    : '';
  check('TaskToolCardBody settles unrelated error through item status/result, not preview children',
    settledHtml.includes('task.outputSchema required property &quot;summary&quot; is missing from $.properties')
      && !settledHtml.includes('class="tool-card__child"'),
    settledHtml);
}


// ---- Task card: long-Chinese delegation (Goal/Constraints/Contract + DeepSeek child) ----
// Mirrors the user's real delegation structure: three ATX sections with dense
// CJK prose plus one DeepSeek child. Guards the structured default view: each
// section lands in its own bucket (no cross-contamination), long Chinese
// survives verbatim (no mid-code-unit cuts; wrap is a CSS concern exercised
// by the core web E2E), and live frames drive queued→running→completed and
// queued→running→cancelled status updates.
{
  const context = [
    '# Goal',
    '验证用户给出的长中文 Task delegation（Goal/Constraints/Contract + DeepSeek child）在 Web transcript 中是否正确、可读、无溢出地结构化渲染。',
    '',
    '# Constraints',
    '只用 DeepSeek。共享工作树并发修改；不得回滚、格式化、提交或运行全套测试。先读现有 Task card实现与现有真实E2E；不要重复实现。默认只改测试/E2E；仅发现明确产品bug时，先消息Main再做最小产品修复。不得把TUI ASCII边框作为Web要求；Web应使用现有panel/card tokens。保留redaction/bounds/raw collapsed。',
    '',
    '# Contract',
    '正确渲染：标题Task；Goal/Constraints/Contract独立区块；child name/agent/target/status；长中文正常wrap；无水平overflow；raw JSON默认折叠；running→completed/cancelled状态可更新；desktop和390px mobile可读。',
  ].join('\n');
  const target = '完成长中文 Task delegation 的 focused 验证：构造 Goal/Constraints/Contract + DeepSeek child 结构；在 Chromium 验证 desktop 与 390px mobile 的 DOM、wrap、overflow、raw collapsed 与状态；复用现有 core lane fixture，避免重复 lane；给出代码证据与真实 Chromium evidence。';
  const args = { context, tasks: [{ name: 'DeepSeek', agent: 'deepseek', task: target }] };
  const view = parseTaskCardArgs(args);
  check('zh task parses the DeepSeek child (name/agent/status)', !!view && view.children.length === 1 && view.children[0].name === 'DeepSeek' && view.children[0].agent === 'deepseek' && view.children[0].status === 'queued', JSON.stringify(view));
  check('zh Goal section holds the long-Chinese goal verbatim', !!view && view.sections.goal === '验证用户给出的长中文 Task delegation（Goal/Constraints/Contract + DeepSeek child）在 Web transcript 中是否正确、可读、无溢出地结构化渲染。', JSON.stringify(view?.sections));
  check('zh Constraints section holds the long-Chinese constraints verbatim', !!view && view.sections.constraints.startsWith('只用 DeepSeek。') && view.sections.constraints.endsWith('保留redaction/bounds/raw collapsed。'), JSON.stringify(view?.sections));
  check('zh Contract section holds the long-Chinese contract verbatim', !!view && view.sections.contract.startsWith('正确渲染：标题Task；') && view.sections.contract.endsWith('desktop和390px mobile可读。'), JSON.stringify(view?.sections));
  check('zh sections stay independent (no cross-contamination)', !!view && !view.sections.goal.includes('只用 DeepSeek') && !view.sections.constraints.includes('验证用户给出的') && !view.sections.contract.includes('不用重复实现') && view.sections.constraints.includes('不得把TUI ASCII边框作为Web要求') && view.sections.contract.includes('running→completed/cancelled状态可更新'), JSON.stringify(view?.sections));
  check('zh child target preserved verbatim (no truncation)', !!view && view.children[0].target === target, JSON.stringify(view?.children?.[0]));

  // Live frames: queued → running → completed (job_updated) with result prose.
  const items = [{
    kind: 'toolCard',
    id: 'tc-zh',
    toolCallId: 'task-zh-call',
    toolName: 'task',
    args,
    status: 'running',
    result: '',
    details: [{ index: 0, jobId: 'job-zh', agentId: 'DeepSeek', agent: 'deepseek', status: 'queued' }],
  }];
  const afterRunning = applyJobUpdated(items, {
    job: { id: 'job-zh', agentId: 'DeepSeek', agent: 'deepseek', status: 'running' },
  });
  const running = resolveTaskCardView(afterRunning[0].args, afterRunning[0].details);
  check('zh child updates queued → running via job_updated', !!running && running.children[0].jobId === 'job-zh' && running.children[0].status === 'running', JSON.stringify(running?.children?.[0]));
  const afterCompleted = applyJobUpdated(afterRunning, {
    job: {
      id: 'job-zh',
      agentId: 'DeepSeek',
      agent: 'deepseek',
      status: 'completed',
      result: { output: '验证完成：desktop 与 390px mobile 均无水平溢出，raw JSON 默认折叠。', error: null },
    },
  });
  const completed = resolveTaskCardView(afterCompleted[0].args, afterCompleted[0].details);
  check('zh child updates running → completed with result sentence', !!completed && completed.children[0].status === 'completed' && completed.children[0].result === '验证完成：desktop 与 390px mobile 均无水平溢出，raw JSON 默认折叠。', JSON.stringify(completed?.children?.[0]));

  // Live frames: queued → running → cancelled (job_updated) on a fresh card.
  const itemsCancel = [{
    kind: 'toolCard',
    id: 'tc-zh-cancel',
    toolCallId: 'task-zh-cancel-call',
    toolName: 'task',
    args,
    status: 'running',
    result: '',
    details: [{ index: 0, jobId: 'job-zh-cancel', agentId: 'DeepSeek', agent: 'deepseek', status: 'running' }],
  }];
  const afterCancelled = applyJobUpdated(itemsCancel, {
    job: { id: 'job-zh-cancel', agentId: 'DeepSeek', agent: 'deepseek', status: 'cancelled' },
  });
  const cancelled = resolveTaskCardView(afterCancelled[0].args, afterCancelled[0].details);
  check('zh child updates running → cancelled via job_updated', !!cancelled && cancelled.children[0].status === 'cancelled', JSON.stringify(cancelled?.children?.[0]));

  // Child→Main IRC body becomes the live activity line on the card.
  const afterMessage = applyMessageDelivered(afterCancelled, {
    message: { from: 'DeepSeek', body: '已完成桌面验证，正在检查 390px mobile 渲染。', to: 'Main' },
  });
  const withActivity = resolveTaskCardView(afterMessage[0].args, afterMessage[0].details);
  check('zh child activity line from message_delivered', !!withActivity && withActivity.children[0].activity === '已完成桌面验证，正在检查 390px mobile 渲染。', JSON.stringify(withActivity?.children?.[0]));
  const zhCard = {
    kind: 'toolCard',
    id: 'tc-zh-fold',
    toolCallId: 'task-zh-fold',
    toolName: 'task',
    args,
    status: 'running',
    result: '',
    details: [{ index: 0, jobId: 'job-zh', agentId: 'DeepSeek', agent: 'deepseek', status: 'running' }],
  };
  const zhView = resolveTaskCardView(zhCard.args, zhCard.details, zhCard.status);
  const zhHtml = zhView ? renderToStaticMarkup(createElement(TaskToolCardBody, { item: zhCard, taskView: zhView })) : '';
  check('zh Goal folds to a one-line preview by default',
    zhHtml.includes('tool-card__section-preview')
      && zhHtml.includes('tool-card__section-summary')
      && zhHtml.includes(firstLinePreview(view.sections.goal))
      && zhHtml.includes('tool-card__child-action'),
    zhHtml.slice(0, 400));
  check('zh child action line is the assignment preview until activity arrives',
    !!zhView && taskChildActionLine(zhView.children[0]) === firstLinePreview(target));
  check('activity wins the child action line over the assignment',
    !!withActivity && taskChildActionLine(withActivity.children[0]) === '已完成桌面验证，正在检查 390px mobile 渲染。');

}

// ---- Edit details.diff semantic lines + redaction surface ----
{
  const edit = parseEditCard(
    { path: 'src/lib.rs', operation: 'replace' },
    { diff: '@@ -1,2 +1,2 @@\n-old\n+new\napi_key=sk-live-super-secret-value-do-not-leak\n' },
    'success',
  );
  check('edit path and operation', !!edit && edit.path === 'src/lib.rs' && edit.operation === 'replace', JSON.stringify(edit));
  check('edit details.diff is preferred over result text', !!edit && edit.diff.includes('+new') && !edit.diff.includes('success'), JSON.stringify(edit));
  const lines = diffLines(edit.diff);
  check('edit diff classifies add/del/meta', lines.some((l) => l.kind === 'add') && lines.some((l) => l.kind === 'del') && lines.some((l) => l.kind === 'meta'), JSON.stringify(lines));
  // Redaction is applied at render (safeText); the pure model still carries the
  // raw diff string so tests can assert details.diff selection independently.
  check('edit model keeps details.diff content for styling', !!edit && edit.diff.includes('api_key='), JSON.stringify(edit));
}

// ---- Todo tool card: user example → structured phases, never the summary prose ----
// The reported regression: the todo card dumped the TUI summary prose
// (`Remaining items … id=… ready`). The card must parse the phases snapshot
// from the result details instead and hide that prose.
{
  const args = { op: 'init', list: [{ phase: 'Build', items: ['compile', 'test'] }] };
  const details = {
    phases: [
      {
        name: 'Build',
        tasks: [
          { id: 't-1', content: 'compile', status: 'in_progress', dependsOn: [], ready: true, waitingOn: [] },
          {
            id: 't-2',
            content: 'test',
            status: 'pending',
            dependsOn: ['t-1'],
            ready: false,
            waitingOn: [{ taskId: 't-1', content: 'compile', status: 'in_progress' }],
          },
        ],
      },
    ],
    storage: 'session',
    completedTasks: [],
  };
  const result = [
    'Remaining items (2):',
    '  - compile [in_progress] (Build) id=t-1 ready',
    '  - test [pending] (Build) id=t-2 waiting on compile (t-1)',
    'Overall: 0/2 done, 2 open.',
    'Active phase 1/1 "Build" (0/2).',
  ].join('\n');
  const view = resolveTodoCardView(args, details, result, 'done');
  check('todo card parses the phases snapshot', view.phases.length === 1 && view.phases[0].name === 'Build', JSON.stringify(view.phases));
  check('todo card keeps task content and status', (() => {
    const tasks = view.phases[0]?.tasks ?? [];
    return tasks.length === 2
      && tasks[0].content === 'compile' && tasks[0].status === 'in_progress'
      && tasks[1].content === 'test' && tasks[1].status === 'pending';
  })(), JSON.stringify(view.phases));
  check('todo card surfaces dependency waiting by content, never ids', view.phases[0]?.tasks[1]?.waitingOn.length === 1 && view.phases[0].tasks[1].waitingOn[0] === 'compile', JSON.stringify(view.phases[0]?.tasks[1]));
  check('todo card op from args', view.op === 'init', JSON.stringify(view));
  check('todo card hides the summary prose when phases exist', view.fallback === '', JSON.stringify(view));
  check('todo card never leaks ids/ready into the view model', !JSON.stringify(view).includes('t-1') && !JSON.stringify(view).includes('"ready"'), JSON.stringify(view));
  check('todo card has no error on success', view.error === '', JSON.stringify(view));
}

// ---- Todo tool card: multi-phase, completed/abandoned, completedTasks ----
{
  const view = resolveTodoCardView(
    { op: 'done' },
    {
      phases: [
        {
          name: 'Design',
          tasks: [
            { id: 'd1', content: 'sketch', status: 'completed', waitingOn: [] },
            { id: 'd2', content: 'review', status: 'abandoned', waitingOn: [] },
          ],
        },
        {
          name: 'Verify',
          tasks: [{ id: 'v1', content: 'test suite', status: 'in_progress', waitingOn: [] }],
        },
      ],
      storage: 'session',
      completedTasks: [{ phase: 'Design', content: 'sketch' }],
    },
    'Remaining items (1):\n  - test suite [in_progress] (Verify)\nOverall: 1/3 done, 1 open.',
    'done',
  );
  check('todo multi-phase parses both phases', view.phases.length === 2 && view.phases[1].name === 'Verify', JSON.stringify(view.phases));
  check('todo completed status preserved', view.phases[0]?.tasks[0]?.status === 'completed', JSON.stringify(view.phases[0]));
  check('todo abandoned status preserved', view.phases[0]?.tasks[1]?.status === 'abandoned', JSON.stringify(view.phases[0]));
  check('todo in-progress marks the current task', view.phases[1]?.tasks[0]?.status === 'in_progress', JSON.stringify(view.phases[1]));
  check('todo completedTasks surfaces the transition content', view.completed.length === 1 && view.completed[0] === 'sketch', JSON.stringify(view.completed));
}

// ---- Todo tool card: failed op keeps bounded error + parsed phases ----
{
  const view = resolveTodoCardView(
    { op: 'start', task: 'missing' },
    {
      phases: [{ name: 'Build', tasks: [{ id: 't1', content: 'compile', status: 'pending', waitingOn: [] }] }],
      storage: 'session',
      completedTasks: [],
    },
    'Errors: Task "missing" not found',
    'error',
  );
  check('todo error keeps the bounded error prose', view.error === 'Errors: Task "missing" not found', JSON.stringify(view));
  check('todo error keeps parsed phases', view.phases.length === 1 && view.phases[0].tasks[0].content === 'compile', JSON.stringify(view.phases));
  check('todo error never falls back to the summary prose', view.fallback === '', JSON.stringify(view));
}

// ---- Todo tool card: malformed shape falls back safely, never raw JSON ----
{
  const view = resolveTodoCardView(
    { op: 'view', extra: { nested: 'secret' } },
    { phases: 'not-an-array', storage: 'session' },
    'Todo list is empty.',
    'done',
  );
  check('todo malformed phases fall back to bounded prose', view.phases.length === 0 && view.fallback === 'Todo list is empty.', JSON.stringify(view));
  check('todo fallback never contains raw args JSON', !view.fallback.includes('"op"') && !view.fallback.includes('secret'), JSON.stringify(view));
  check('todo fallback stays silent without result text', resolveTodoCardView({ op: 'view' }, null, '', 'done').fallback === '', '');
}

// ---- Todo tool card: running init previews pending phases from args ----
{
  const view = resolveTodoCardView(
    { op: 'init', list: [{ phase: 'Build', items: ['compile', 'test'] }] },
    undefined,
    '',
    'running',
  );
  check('todo running init previews pending phases', view.phases.length === 1 && view.phases[0].name === 'Build' && view.phases[0].tasks.length === 2 && view.phases[0].tasks[0].status === 'pending', JSON.stringify(view.phases));
  check('todo running card has no fallback', view.fallback === '', JSON.stringify(view));
  const flat = resolveTodoCardView({ op: 'init', phase: 'Plan', items: ['ship it'] }, undefined, '', 'running');
  check('todo flat init previews one phase', flat.phases.length === 1 && flat.phases[0].name === 'Plan' && flat.phases[0].tasks[0].content === 'ship it', JSON.stringify(flat.phases));
  const partialProse = resolveTodoCardView({ op: 'start', task: 't1' }, undefined, 'Remaining items (1):\n  - compile [in_progress] (Build) id=t1 ready', 'running');
  check('todo running card never streams the summary prose', partialProse.fallback === '', JSON.stringify(partialProse));
}

// ---- Todo tool card: restored wire renders structured, append shape + bounding ----
{
  const items = messagesToItems([
    {
      role: 'assistant',
      content: [{ type: 'toolCall', id: 'todo-call-1', name: 'todo', arguments: { op: 'append', phase: 'Build', items: ['lint'] } }],
    },
    {
      role: 'toolResult',
      toolCallId: 'todo-call-1',
      content: [{ type: 'text', text: 'Remaining items (3):\n  - compile [in_progress] (Build)\n  - test [pending] (Build)\n  - lint [pending] (Build)\nOverall: 0/3 done, 3 open.' }],
      details: {
        phases: [
          {
            name: 'Build',
            tasks: [
              { id: 't1', content: 'compile', status: 'in_progress', waitingOn: [] },
              { id: 't2', content: 'test', status: 'pending', waitingOn: [] },
              { id: 't3', content: 'lint', status: 'pending', waitingOn: [] },
            ],
          },
        ],
        storage: 'session',
        completedTasks: [],
      },
      isError: false,
    },
  ]);
  const card = items.find((item) => item.kind === 'toolCard' && item.toolCallId === 'todo-call-1');
  const view = card ? resolveTodoCardView(card.args, card.details, card.result, card.status) : null;
  check('restored todo card renders structured phases', !!view && view.phases.length === 1 && view.phases[0].tasks.length === 3 && view.phases[0].tasks[2].content === 'lint', JSON.stringify(view));
  check('restored todo card op from args', !!view && view.op === 'append', JSON.stringify(view));
  const long = resolveTodoCardView(
    { op: 'view' },
    { phases: [{ name: 'P', tasks: [{ id: 'x', content: 'x'.repeat(300), status: 'pending', waitingOn: [] }] }] },
    '',
    'done',
  );
  check('todo task content is bounded', long.phases[0]?.tasks[0]?.content.length < 300 && long.phases[0].tasks[0].content.endsWith('\u2026'), JSON.stringify(long.phases[0]));
}

// ---- parseTodoPhases: defensive malformed handling ----
{
  check('parseTodoPhases null for non-array', parseTodoPhases({ phases: [] }) === null);
  check('parseTodoPhases null for empty array', parseTodoPhases([]) === null);
  const skipped = parseTodoPhases([
    { name: 'P', tasks: [
      { id: 'ok', content: 'fine', status: 'pending', waitingOn: [] },
      { id: 'empty', content: '   ', status: 'pending', waitingOn: [] },
      'garbage',
      null,
    ] },
    'not-a-phase',
  ]);
  check('parseTodoPhases skips malformed entries', skipped !== null && skipped.length === 1 && skipped[0].tasks.length === 1 && skipped[0].tasks[0].content === 'fine', JSON.stringify(skipped));
  const unknownStatus = parseTodoPhases([{ name: 'P', tasks: [{ id: 'u', content: 'weird', status: 'bloop', waitingOn: [] }] }]);
  check('parseTodoPhases unknown status normalizes to pending', unknownStatus?.[0]?.tasks[0]?.status === 'pending', JSON.stringify(unknownStatus));
  const waitingOnGarbage = parseTodoPhases([{ name: 'P', tasks: [{ id: 'b', content: 'task', status: 'pending', waitingOn: [{ taskId: 'x', content: 'dependency', status: 'in_progress' }, 'junk', null] }] }]);
  check('parseTodoPhases ignores malformed waitingOn entries', waitingOnGarbage?.[0]?.tasks[0]?.waitingOn.length === 1 && waitingOnGarbage[0].tasks[0].waitingOn[0] === 'dependency', JSON.stringify(waitingOnGarbage));
}

// ---- robust heading parse + generic bounds unchanged ----
{
  check('parseHeading accepts ATX with trailing hashes', parseHeading('## Goal ##')?.title === 'Goal');
  check('parseHeading rejects missing space after hashes', parseHeading('##Goal') === null);
  const generic = applyToolSnapshot(
    [{ kind: 'toolCard', id: 'g', toolCallId: 'g1', toolName: 'grep', args: { pattern: 'TODO' }, status: 'running', result: '' }],
    'g1',
    Array.from({ length: 12 }, (_, i) => `g-${i + 1}`).join('\n'),
    'done',
  )[0];
  check('generic tool still uses six-line bound', generic.kind === 'toolCard' && generic.result.startsWith('\u2026 6 more lines\ng-7') && generic.result.endsWith('\ng-12'), JSON.stringify(generic));
  const restored = messagesToItems([
    {
      role: 'assistant',
      content: [{ type: 'toolCall', id: 'edit-call', name: 'edit', arguments: { path: 'f.txt' } }],
    },
    {
      role: 'toolResult',
      toolCallId: 'edit-call',
      toolName: 'edit',
      content: [{ type: 'text', text: '1 replacement in f.txt' }],
      details: { diff: '@@\n-old\n+new' },
      isError: false,
    },
  ]);
  const card = restored.find((i) => i.kind === 'toolCard');
  check('restored edit card carries details.diff', card?.kind === 'toolCard' && card.details && typeof card.details === 'object' && 'diff' in card.details && card.details.diff.includes('+new'), JSON.stringify(card));
}

// ---- compactToolArgs: mirrors TUI compact_tool_arguments ----
check('compactToolArgs extracts command', compactToolArgs({ command: 'git status' }) === 'git status');
check('compactToolArgs extracts path', compactToolArgs({ path: 'src/main.rs' }) === 'src/main.rs');
check('compactToolArgs extracts pattern', compactToolArgs({ pattern: 'TODO' }) === 'TODO');
check('compactToolArgs bounds to 60 chars', compactToolArgs({ command: 'x'.repeat(61) }) === 'x'.repeat(57) + '...');
check('compactToolArgs empty for no match', compactToolArgs({ foo: 'bar' }) === '');
check('compactToolArgs null → empty', compactToolArgs(null) === '');

// ---- parseCommandCardArgs: bash tool command extraction ----
{
  const cmd = parseCommandCardArgs({ command: 'cargo build --release' });
  check('parseCommandCardArgs extracts command', cmd !== null && cmd.command === 'cargo build --release', JSON.stringify(cmd));
  const empty = parseCommandCardArgs({ command: '' });
  check('parseCommandCardArgs null for empty command', empty === null, JSON.stringify(empty));
  const noCmd = parseCommandCardArgs({ foo: 'bar' });
  check('parseCommandCardArgs null when no command field', noCmd === null, JSON.stringify(noCmd));
}

// ---- command copy: both render paths, redaction, clipboard success/failure ----
{
  const multiline = 'cargo test -- --nocapture\n--features foo';
  const secretCmd = 'export token = supersecret';
  const hostileCmd = '<img src=x onerror=alert(1)> echo ok';
  const outputSecret = 'stdout sk-ABCDEFGHIJKLMNOPQRST';
  const parsed = parseCommandCardArgs({ command: multiline });
  check(
    'command copy uses the parsed command string, not clamped DOM text',
    parsed !== null && parsed.command === multiline,
    JSON.stringify(parsed),
  );
  check(
    'multiline command copy payload is exact',
    redactedClipboardText(multiline) === multiline,
  );
  check(
    'secret command copy payload is redacted, never the raw secret',
    redactedClipboardText(secretCmd) === 'export token=[REDACTED]'
      && !redactedClipboardText(secretCmd).includes('supersecret'),
  );
  check(
    'ToolCard commandView and BashCard command share the same redacted payload',
    parsed !== null
      && redactedClipboardText(parsed.command) === redactedClipboardText(multiline),
  );
  check(
    'output-only copy redacts secrets and is not used for command copy',
    redactedClipboardText(outputSecret) === 'stdout [REDACTED]'
      && redactedClipboardText(multiline) !== redactedClipboardText(outputSecret),
  );

  const bashToolItem = {
    kind: 'toolCard',
    id: 'bash-copy-1',
    toolCallId: 'bash-copy-call',
    toolName: 'bash',
    args: { command: secretCmd },
    status: 'done',
    result: outputSecret,
  };
  const toolHtml = renderToStaticMarkup(createElement(ToolCard, { item: bashToolItem }));
  check(
    'ToolCard bash commandView markup includes command-copy with Copy command',
    toolHtml.includes('class="tool-card__command-copy"')
      && toolHtml.includes('aria-label="Copy command"')
      && toolHtml.includes('aria-live="polite"')
      && toolHtml.includes('type="button"')
      && toolHtml.includes('>copy</button>'),
    toolHtml,
  );
  check(
    'ToolCard command copy sits in the head, not on the output',
    toolHtml.indexOf('tool-card__command-copy') < toolHtml.indexOf('tool-card__output')
      && toolHtml.includes('class="tool-card__head"'),
    toolHtml,
  );
  check(
    'ToolCard visible command is safeText, never the raw secret or output',
    toolHtml.includes('export token=[REDACTED]')
      && !toolHtml.includes('supersecret')
      && toolHtml.includes('Command')
      && toolHtml.includes('$'),
    toolHtml,
  );

  const multilineItem = { ...bashToolItem, id: 'bash-copy-ml', args: { command: multiline }, result: outputSecret };
  const multilineHtml = renderToStaticMarkup(createElement(ToolCard, { item: multilineItem }));
  check(
    'ToolCard multiline command renders both lines in the command text',
    multilineHtml.includes('cargo test -- --nocapture')
      && multilineHtml.includes('--features foo')
      && !multilineHtml.includes('sk-ABCDEFGHIJKLMNOPQRST'),
    multilineHtml,
  );

  const hostileItem = { ...bashToolItem, id: 'bash-copy-h', args: { command: hostileCmd }, result: 'ok' };
  const hostileHtml = renderToStaticMarkup(createElement(ToolCard, { item: hostileItem }));
  check(
    'ToolCard hostile command stays inert text',
    hostileHtml.includes('&lt;img src=x onerror=alert(1)&gt; echo ok')
      && !hostileHtml.includes('<img src=x'),
    hostileHtml,
  );

  const writeItem = {
    kind: 'toolCard',
    id: 'write-1',
    toolCallId: 'write-call',
    toolName: 'write',
    args: { path: 'a.rs', content: 'fn main() {}' },
    status: 'done',
    result: 'wrote a.rs',
  };
  const writeHtml = renderToStaticMarkup(createElement(ToolCard, { item: writeItem }));
  check(
    'ToolCard non-bash has no command-copy button',
    !writeHtml.includes('tool-card__command-copy') && !writeHtml.includes('aria-label="Copy command"'),
    writeHtml,
  );

  const bashCardHtml = renderToStaticMarkup(createElement(BashCard, {
    command: secretCmd,
    output: outputSecret,
    status: 'done',
  }));
  check(
    'BashCard with command includes bash-copy Copy command and $ + safe command',
    bashCardHtml.includes('class="bash-copy"')
      && bashCardHtml.includes('aria-label="Copy command"')
      && bashCardHtml.includes('aria-live="polite"')
      && bashCardHtml.includes('class="bash-cmd bash-cmd--title">Command</span>')
      && bashCardHtml.includes('class="bash-prompt">$</span>')
      && bashCardHtml.includes('export token=[REDACTED]')
      && !bashCardHtml.includes('supersecret'),
    bashCardHtml,
  );
  check(
    'BashCard command copy does not use the output as the button label payload',
    bashCardHtml.includes('>copy</button>')
      && bashCardHtml.indexOf('bash-copy') < bashCardHtml.indexOf('bash-output'),
    bashCardHtml,
  );

  const outputOnlyHtml = renderToStaticMarkup(createElement(BashCard, {
    label: 'tool output',
    output: outputSecret,
  }));
  check(
    'BashCard without command keeps Copy output aria-label',
    outputOnlyHtml.includes('class="bash-copy"')
      && outputOnlyHtml.includes('aria-label="Copy output"')
      && !outputOnlyHtml.includes('aria-label="Copy command"')
      && !outputOnlyHtml.includes('bash-command-text')
      && outputOnlyHtml.includes('tool output'),
    outputOnlyHtml,
  );

  const bashHostileHtml = renderToStaticMarkup(createElement(BashCard, {
    command: hostileCmd,
    output: 'ok',
  }));
  check(
    'BashCard hostile command stays inert text',
    bashHostileHtml.includes('&lt;img src=x onerror=alert(1)&gt; echo ok')
      && !bashHostileHtml.includes('<img src=x'),
    bashHostileHtml,
  );
}


// ---- parseProcessCardArgs: argv/label projection (with and without op) ----
{
  const start = parseProcessCardArgs({ op: 'start', argv: ['echo', 'hello', 'world'] });
  check('process start renders simple argv', start !== null && start.label === 'echo hello world', JSON.stringify(start));
  check('process start command preserves simple argv', start !== null && start.command === 'echo hello world', JSON.stringify(start));
  const quoted = parseProcessCardArgs({
    op: 'start',
    argv: ['printf', '%s', 'hello world', '', `a'b`, '$(dangerous)', 'x;y', 'line\nbreak'],
  });
  const quotedCommand = `printf %s 'hello world' '' 'a'"'"'b' '$(dangerous)' 'x;y' 'line\nbreak'`;
  check('process start shell-quotes spaces, empty argv, quotes, metacharacters, and newlines', quoted?.command === quotedCommand, JSON.stringify(quoted));
  check('process start label mirrors the safe copyable command', quoted?.label === quotedCommand, JSON.stringify(quoted));
  const malformedStart = parseProcessCardArgs({ op: 'start', argv: ['echo', 42] });
  check('process start rejects non-string argv without stringifying it', malformedStart?.command === 'process start', JSON.stringify(malformedStart));
  const emptyStart = parseProcessCardArgs({ op: 'start', argv: [] });
  check('process start empty argv uses the bounded fallback', emptyStart?.command === 'process start', JSON.stringify(emptyStart));
  check('bare empty argv stays unrecognized', parseProcessCardArgs({ argv: [] }) === null);
  const noOp = parseProcessCardArgs({ argv: ['pi-web-screenshot-http'], label: 'pi-web-screenshot-http' });
  check('process without op joins argv', noOp !== null && noOp.label === 'pi-web-screenshot-http', JSON.stringify(noOp));
  check('process without op command is the argv join', noOp !== null && noOp.command === 'pi-web-screenshot-http', JSON.stringify(noOp));
  const labelOnly = parseProcessCardArgs({ label: 'pi-web-screenshot-http' });
  check('process label-only fallback', labelOnly !== null && labelOnly.label === 'pi-web-screenshot-http', JSON.stringify(labelOnly));
  check('process label-only command is the label', labelOnly !== null && labelOnly.command === 'pi-web-screenshot-http', JSON.stringify(labelOnly));
  const stop = parseProcessCardArgs({ op: 'stop', id: 'abc123' });
  check('process stop surfaces op+id', stop !== null && stop.label === 'process stop abc123', JSON.stringify(stop));
  check('process stop command is the op+id invocation', stop !== null && stop.command === 'process stop abc123', JSON.stringify(stop));
  const bare = parseProcessCardArgs({ foo: 'bar' });
  check('process null for unrecognised args', bare === null, JSON.stringify(bare));
}

// ---- process card copy: ToolCard renders the process-copy action ----
{
  const processItem = {
    kind: 'toolCard',
    id: 'process-copy-1',
    toolCallId: 'process-copy-call',
    toolName: 'process',
    args: { op: 'start', argv: ['echo', 'hello', 'world'] },
    status: 'done',
    result: 'Process started: abc123 (visible in /ps)',
  };
  const processHtml = renderToStaticMarkup(createElement(ToolCard, { item: processItem }));
  check(
    'ToolCard process card includes process-copy with Copy command',
    processHtml.includes('class="tool-card__process-copy"')
      && processHtml.includes('aria-label="Copy command"')
      && processHtml.includes('aria-live="polite"')
      && processHtml.includes('type="button"')
      && processHtml.includes('>copy</button>'),
    processHtml,
  );
  check(
    'ToolCard process copy sits in the head, not on the output',
    processHtml.indexOf('tool-card__process-copy') < processHtml.indexOf('tool-card__output')
      && processHtml.includes('class="tool-card__head"'),
    processHtml,
  );
  check(
    'ToolCard process card shows the argv join label in the summary line',
    processHtml.includes('class="tool-card__summary-line">echo hello world'),
    processHtml,
  );

  const processStopItem = {
    kind: 'toolCard',
    id: 'process-copy-2',
    toolCallId: 'process-copy-call-2',
    toolName: 'process',
    args: { op: 'stop', id: 'abc123' },
    status: 'done',
    result: 'stopped',
  };
  const processStopHtml = renderToStaticMarkup(createElement(ToolCard, { item: processStopItem }));
  check(
    'ToolCard process stop card copies the op+id invocation',
    processStopHtml.includes('class="tool-card__process-copy"')
      && processStopHtml.includes('process stop abc123'),
    processStopHtml,
  );
}

// ---- parseWriteCardArgs: path + success/error summary ----
{
  const done = parseWriteCardArgs({ path: 'src/main.rs', content: 'fn main() {}' }, 'Successfully wrote 12 bytes', 'done');
  check('write done extracts path', done !== null && done.path === 'src/main.rs', JSON.stringify(done));
  check('write done summary from result', done !== null && done.summary === 'Successfully wrote 12 bytes', JSON.stringify(done));
  check('write done does NOT include content', done !== null && !done.summary.includes('fn main'), JSON.stringify(done));
  const error = parseWriteCardArgs({ path: 'out.txt', content: 'data' }, 'Permission denied', 'error');
  check('write error shows error result', error !== null && error.summary === 'Permission denied', JSON.stringify(error));
  const running = parseWriteCardArgs({ file: 'out.txt' }, '', 'running');
  check('write running shows writing hint', running !== null && running.summary === 'writing\u2026', JSON.stringify(running));
  check('write uses file key as path alias', running !== null && running.path === 'out.txt', JSON.stringify(running));
  const noPath = parseWriteCardArgs({ content: 'data' }, '', 'done');
  check('write null when no path', noPath === null, JSON.stringify(noPath));
}

// ---- parseReadCard: typed compact/expanded projection ----
{
  const read = parseReadCard({ path: 'README.md' }, 'hello');
  check('parseReadCard extracts path', read !== null && read.path === 'README.md', JSON.stringify(read));
  const fileKey = parseReadCard({ file: 'config.toml' }, 'x');
  check('parseReadCard uses file key alias', fileKey !== null && fileKey.path === 'config.toml', JSON.stringify(fileKey));
  const noPath = parseReadCard({ offset: 0 }, 'x');
  check('parseReadCard null when no path', noPath === null, JSON.stringify(noPath));
}

// ---- parseHubCard: wait running — fixed human title, no raw envelope ----
// Reported regression: the web transcript showed the raw
// `hub / running… / {ids, op, timeoutMs}` card. The running wait must show a
// fixed human title + clear waiting feedback and never expose ids/timeoutMs.
{
  const args = { op: 'wait', ids: ['00000000-0000-7000-8000-0000000000ab'], timeoutMs: 60000 };
  const running = parseHubCard(args, undefined, '', 'running');
  check('hub wait running: fixed "Waiting" title', running.title === 'Waiting', JSON.stringify(running));
  check('hub wait running: clear waiting feedback', running.headline === 'Waiting for a job to complete\u2026', JSON.stringify(running));
  check('hub wait running: no raw envelope in view', !JSON.stringify(running).includes('timeoutMs') && !JSON.stringify(running).includes('"ids"') && !JSON.stringify(running).includes('00000000'), JSON.stringify(running));
  const messageWait = parseHubCard({ op: 'wait', timeoutMs: 5000 }, undefined, '', 'running');
  check('hub wait message running copy', messageWait.title === 'Waiting' && messageWait.headline === 'Waiting for an agent message\u2026', JSON.stringify(messageWait));
  check('hub wait running hides timeoutMs entirely', !JSON.stringify(messageWait).includes('5000') && !JSON.stringify(messageWait).includes('timeoutMs'), JSON.stringify(messageWait));
}

// ---- parseHubCard: readable from shown, internal UUID from hidden ----
{
  const named = parseHubCard({ op: 'wait', from: 'Main', timeoutMs: 5000 }, undefined, '', 'running');
  check('hub wait running shows a readable from name', named.headline.includes('from Main'), JSON.stringify(named));
  const uuidFrom = parseHubCard({ op: 'wait', from: '00000000-0000-7000-8000-0000000000cd', timeoutMs: 5000 }, undefined, '', 'running');
  check('hub wait running never exposes an internal UUID from', !uuidFrom.headline.includes('00000000'), JSON.stringify(uuidFrom));
  check('hub wait running falls back to generic copy without from', uuidFrom.headline === 'Waiting for an agent message\u2026', JSON.stringify(uuidFrom));
}

// ---- parseHubCard: settled wait with typed details.message ----
{
  const view = parseHubCard(
    { op: 'wait', from: 'Main', timeoutMs: 20000 },
    {
      op: 'wait',
      message: {
        id: 'm1',
        from: 'Main',
        to: 'Child',
        body: 'hello child',
        replyTo: 'parent-9',
        timestamp: 7,
      },
    },
    '[m1] Main: hello child',
    'done',
  );
  check('hub wait typed: fixed "IRC" title', view.title === 'IRC', JSON.stringify(view));
  check('hub wait typed: direction headline', view.headline === 'from Main', JSON.stringify(view));
  check('hub wait typed: body from the typed projection', view.body === 'hello child', JSON.stringify(view));
  check('hub wait typed: reply metadata', view.metadata === 'reply to parent-9', JSON.stringify(view));
  check('hub wait typed: typed flag', view.typed === true, JSON.stringify(view));
  check('hub wait typed: model-facing prose never leaks', !JSON.stringify(view).includes('[m1] Main:'), JSON.stringify(view));
  check('hub wait typed: no raw envelope keys', !JSON.stringify(view).includes('timeoutMs') && !JSON.stringify(view).includes('"ids"'), JSON.stringify(view));
}

// ---- parseHubCard: timeout / job-wait / malformed → concise prose ----
{
  const timeout = parseHubCard(
    { op: 'wait', timeoutMs: 500 },
    { op: 'wait', message: null },
    'No message before timeout.',
    'done',
  );
  check('hub wait timeout: fixed human title', timeout.title === 'Waiting', JSON.stringify(timeout));
  check('hub wait timeout: concise copy, not raw JSON', timeout.note === 'No message before timeout.' && !JSON.stringify(timeout).includes('timeoutMs'), JSON.stringify(timeout));
  const jobWait = parseHubCard(
    { op: 'wait', ids: ['job-1'], timeoutMs: 500 },
    { op: 'wait', jobs: [] },
    'No job completed before timeout.',
    'done',
  );
  check('hub wait job-wait: concise jobs copy', jobWait.note === 'No job completed before timeout.' && !JSON.stringify(jobWait).includes('job-1'), JSON.stringify(jobWait));
  const malformed = parseHubCard(
    { op: 'wait', timeoutMs: 500 },
    { op: 'wait', message: { id: 'x' } },
    '',
    'done',
  );
  check('hub wait malformed projection: default concise copy', malformed.title === 'Waiting' && malformed.note === 'No message received.' && malformed.typed === false, JSON.stringify(malformed));
  const noNote = parseHubCard({ op: 'wait' }, null, '', 'done');
  check('hub wait empty result: never empty card', noNote.note === 'No message received.', JSON.stringify(noNote));
}

// ---- parseHubCard: send — no regression (outgoing message + outcome) ----
{
  const secret = ['s', 'k-', 'abcdefghijklmnop1234'].concat();
  const view = parseHubCard(
    { op: 'send', to: 'DeepSeekRenderingFinal', message: `token=${secret}` },
    { op: 'send', receipts: [{ to: 'DeepSeekRenderingFinal', outcome: 'woken' }] },
    '- DeepSeekRenderingFinal: woken',
    'done',
  );
  check('hub send: fixed "IRC" title', view.title === 'IRC', JSON.stringify(view));
  check('hub send: recipient headline', view.headline === 'to DeepSeekRenderingFinal', JSON.stringify(view));
  check('hub send: outgoing message body kept', view.body === `token=${secret}`, JSON.stringify(view));
  check('hub send: outcome label from receipts (woken → injected)', view.note === 'injected', JSON.stringify(view));
  check('hub send: no raw args envelope (to/message/receipts) in the view', !JSON.stringify(view).includes('"to":') && !JSON.stringify(view).includes('"message":') && !JSON.stringify(view).includes('"receipts"'), JSON.stringify(view));
  const failed = parseHubCard(
    { op: 'send', to: '00000000-0000-7000-8000-000000000099', message: 'no recipient' },
    { op: 'send', receipts: [{ to: '00000000-0000-7000-8000-000000000099', outcome: 'failed', error: 'unknown orchestration agent' }] },
    '- 00000000-0000-7000-8000-000000000099: failed — unknown orchestration agent',
    'done',
  );
  check('hub send failed receipt: failed outcome label', failed.note === 'failed', JSON.stringify(failed));
  check('hub send mixed receipts: partial label', parseHubCard({ op: 'send', to: 'all', message: 'x' }, { op: 'send', receipts: [{ outcome: 'woken' }, { outcome: 'failed' }] }, '', 'done').note === 'partial', '');
  check('hub send no receipts: falls back to bounded result text', parseHubCard({ op: 'send', to: 'a', message: 'x' }, {}, '- a: delivered', 'done').note === '- a: delivered', '');
}

// ---- parseHubCard: send with await — typed reply frame, not prose ----
{
  const view = parseHubCard(
    { op: 'send', to: 'Child', message: 'please inspect', await: true, timeoutMs: 5000 },
    {
      op: 'send',
      receipts: [{ to: 'Child', outcome: 'woken' }],
      reply: {
        id: 'm-reply',
        from: 'Child',
        to: 'Main',
        body: 'found three crates',
        replyTo: 'm-send',
        timestamp: 9,
      },
    },
    'Reply from Child: found three crates',
    'done',
  );
  check('hub send reply: typed reply frame headline', view.reply?.headline === 'from Child', JSON.stringify(view.reply));
  check('hub send reply: typed reply body', view.reply?.body === 'found three crates', JSON.stringify(view.reply));
  check('hub send reply: metadata renders', view.reply?.metadata === 'reply to m-send', JSON.stringify(view.reply));
  check('hub send reply: model-facing "Reply from" prose never leaks', !JSON.stringify(view).includes('Reply from'), JSON.stringify(view));
  check('hub send reply: typed flag', view.typed === true, JSON.stringify(view));
}

// ---- parseHubCard: other ops + malformed args — safe fallback, no raw JSON ----
{
  const list = parseHubCard({ op: 'list' }, { op: 'list', peers: [{ id: 'Alpha', status: 'idle' }] }, '- Alpha: idle', 'done');
  check('hub list: "Hub" title + op headline', list.title === 'Hub' && list.headline === 'hub list', JSON.stringify(list));
  check('hub list: bounded result prose, never raw peers JSON', !JSON.stringify(list).includes('"peers"') && !JSON.stringify(list).includes('"id":"Alpha"'), JSON.stringify(list));
  const garbage = parseHubCard('not-an-object', { op: 'wait', message: null }, 'boom', 'error');
  check('hub malformed args: safe fallback view', garbage.title === 'Hub' && garbage.note === 'boom', JSON.stringify(garbage));
  const empty = parseHubCard(null, null, '', 'running');
  check('hub null args: never dumps anything', empty.title === 'Hub' && empty.headline === 'hub' && empty.body === '' && empty.note === '', JSON.stringify(empty));
}

// ---- parseHubCard: bounds — long bodies/results stay bounded head-first ----
// Hub bodies/notes are Markdown-rendered by the view, so the bound keeps the
// LEADING lines (a tail cut would slice through Markdown lists/code fences)
// plus an omitted-line hint at the IRC_BODY_LINE_LIMIT ceiling; the compact
// default clamps visually to IRC_COMPACT_LINE_LIMIT lines behind an expand
// toggle operating inside that ceiling.
{
  const twelve = Array.from({ length: 12 }, (_, i) => `body-line-${i + 1}`).join('\n');
  const typed = parseHubCard(
    { op: 'wait', timeoutMs: 5000 },
    { op: 'wait', message: { id: 'm1', from: 'Main', to: 'Child', body: twelve, replyTo: null } },
    '',
    'done',
  );
  check('hub wait typed: body under the fold passes through unbounded', typed.body === twelve, typed.body.slice(0, 80));
  const over = Array.from({ length: 45 }, (_, i) => `body-line-${i + 1}`).join('\n');
  const typedOver = parseHubCard(
    { op: 'wait', timeoutMs: 5000 },
    { op: 'wait', message: { id: 'm1', from: 'Main', to: 'Child', body: over, replyTo: null } },
    '',
    'done',
  );
  check('hub wait typed: body bounded head-first to IRC_BODY_LINE_LIMIT', typedOver.body.startsWith('body-line-1\nbody-line-2') && typedOver.body.endsWith('\u2026 5 more lines') && typedOver.body.split('\n').length === IRC_BODY_LINE_LIMIT + 1, typedOver.body.slice(-120));
  const longResult = Array.from({ length: 12 }, (_, i) => `line-${i + 1}`).join('\n');
  const timeout = parseHubCard({ op: 'wait', timeoutMs: 500 }, { op: 'wait', message: null }, longResult, 'done');
  check('hub wait timeout: short result passes through unbounded', timeout.note === longResult, timeout.note);
  const send = parseHubCard({ op: 'send', to: 'Main', message: twelve }, { op: 'send', receipts: [{ outcome: 'queued' }] }, '', 'done');
  check('hub send: outgoing message under the fold passes through unbounded', send.body === twelve, send.body.slice(0, 80));
}

// ---- boundHubBody: markdown-aware head bound (hub card fold) ----
{
  const empty = boundHubBody('');
  check('boundHubBody empty', empty.text === '' && !empty.bounded && empty.omitted === 0);
  const short = 'one\ntwo';
  check('boundHubBody under limit unchanged', boundHubBody(short).text === short && !boundHubBody(short).bounded);
  const long = Array.from({ length: 45 }, (_, i) => `line-${i + 1}`).join('\n');
  const bound = boundHubBody(long);
  check('boundHubBody keeps the head + hint', bound.bounded && bound.omitted === 5 && bound.text.startsWith('line-1\nline-2') && bound.text.endsWith('\u2026 5 more lines'), bound.text.slice(-80));
  check('boundHubBody caps total lines at IRC_BODY_LINE_LIMIT + 1', bound.text.split('\n').length === IRC_BODY_LINE_LIMIT + 1, String(bound.text.split('\n').length));
  check('boundHubBody single-line pluralization', boundHubBody('x\ny\n').text === 'x\ny' && !boundHubBody('x\ny\n').bounded);
  check('boundHubBody normalizes CRLF like boundIrcBody', boundHubBody('a\r\nb\r\n').text === 'a\nb');
  const atLimit = Array.from({ length: IRC_BODY_LINE_LIMIT }, (_, i) => `x${i + 1}`).join('\n');
  check('boundHubBody exact limit stays unbounded', boundHubBody(atLimit).text === atLimit && !boundHubBody(atLimit).bounded);
  check('boundHubBody and boundIrcBody share the same fold', boundIrcBody(long) === boundHubBody(long).text);
}

// ---- parseHubCard: markdown bodies pass through for the shared renderer ----
// The hub card renders body/note through renderMarkdown (asserted in
// markdown.test.ts with hub-shaped fixtures); the parse layer must preserve
// the markdown verbatim — never re-bounded, trimmed away, or munged — so
// bullets/inline code/path lists arrive at the renderer intact.
{
  const body = [
    '- checked `crates/pi-cli/web/src/App.tsx`',
    '- verified the compact clamp',
    '- `renderMarkdown` escapes hostile HTML',
    '',
    'Path list:',
    '- `crates/pi-cli/web/src/transcript.ts`',
    '- `crates/pi-cli/web/src/styles.css`',
  ].join('\n');
  const view = parseHubCard(
    { op: 'wait', timeoutMs: 5000 },
    { op: 'wait', message: { id: 'm1', from: 'Main', to: 'Child', body, replyTo: null } },
    '',
    'done',
  );
  check('hub wait typed: markdown body preserved verbatim for the renderer', view.body === body, view.body.slice(0, 120));
  check('hub wait typed: raw envelope still never leaks into the markdown view', !JSON.stringify(view).includes('timeoutMs') && !JSON.stringify(view).includes('m1') && !JSON.stringify(view).includes('"args"'), JSON.stringify(view).slice(0, 160));
}

// ---- restored hub wait wire renders the typed card via messagesToItems ----
{
  const items = messagesToItems([
    {
      role: 'assistant',
      content: [{ type: 'toolCall', id: 'hub-call-1', name: 'hub', arguments: { op: 'wait', from: 'Main', timeoutMs: 20000 } }],
    },
    {
      role: 'toolResult',
      toolCallId: 'hub-call-1',
      content: [{ type: 'text', text: '[m1] Main: hello child' }],
      details: { op: 'wait', message: { id: 'm1', from: 'Main', to: 'Child', body: 'hello child', replyTo: null, timestamp: 7 } },
      isError: false,
    },
  ]);
  const card = items.find((item) => item.kind === 'toolCard' && item.toolCallId === 'hub-call-1');
  const view = card && card.kind === 'toolCard' ? parseHubCard(card.args, card.details, card.result, card.status) : null;
  check('restored hub wait renders the typed IRC card', !!view && view.title === 'IRC' && view.headline === 'from Main' && view.body === 'hello child', JSON.stringify(view));
  check('restored hub wait never leaks the result prose', !!view && !view.note.includes('[m1]') && view.note === '', JSON.stringify(view));
}

// ---- bashExecution exitCode/cancelled → bash Item status ----
{
  const items = messagesToItems([
    { role: 'bashExecution', command: 'true', output: '', exitCode: 0, timestamp: 1 },
  ]);
  const bash = items.find((i) => i.kind === 'bash');
  check('bashExecution exitCode 0 → done status', bash?.kind === 'bash' && bash.status === 'done', JSON.stringify(bash));
}
{
  const items = messagesToItems([
    { role: 'bashExecution', command: 'false', output: '', exitCode: 1, timestamp: 2 },
  ]);
  const bash = items.find((i) => i.kind === 'bash');
  check('bashExecution exitCode 1 → error status', bash?.kind === 'bash' && bash.status === 'error', JSON.stringify(bash));
}
{
  const items = messagesToItems([
    { role: 'bashExecution', command: 'sleep 30', output: '', cancelled: true, timestamp: 3 },
  ]);
  const bash = items.find((i) => i.kind === 'bash');
  check('bashExecution cancelled → cancelled status', bash?.kind === 'bash' && bash.status === 'cancelled', JSON.stringify(bash));
}
{
  const items = messagesToItems([
    { role: 'bashExecution', command: 'echo hi', output: 'hi', timestamp: 4 },
  ]);
  const bash = items.find((i) => i.kind === 'bash');
  check('bashExecution no exitCode → undefined status', bash?.kind === 'bash' && bash.status === undefined, JSON.stringify(bash));
}

// ---- bash tool call → toolCard with done status (not bashExecution) ----
{
  const items = messagesToItems([
    { role: 'assistant', content: [{ type: 'toolCall', id: 'bash-1', name: 'bash', arguments: { command: 'ls -la' } }] },
    { role: 'toolResult', toolCallId: 'bash-1', content: [{ type: 'text', text: 'total 0' }], isError: false },
  ]);
  const card = items.find((i) => i.kind === 'toolCard' && i.toolCallId === 'bash-1');
  check('bash toolCall → toolCard done', card?.kind === 'toolCard' && card.status === 'done', JSON.stringify(card));
  check('bash toolCall args preserved for projection', card?.kind === 'toolCard' && card.args && card.args.command === 'ls -la', JSON.stringify(card));
}

// ---- no raw args JSON in default projection shapes ----
check('compactToolArgs never returns JSON', !compactToolArgs({ command: 'x' }).startsWith('{'));
check('parseCommandCardArgs returns command not JSON', parseCommandCardArgs({ command: 'x' })?.command === 'x');
check('parseWriteCardArgs returns path+summary not content JSON', (() => { const w = parseWriteCardArgs({ path: 'f', content: 'big' }, 'ok', 'done'); return w !== null && !w.summary.includes('big'); })());
check('parseReadCard returns path not path JSON', (() => { const r = parseReadCard({ path: 'f' }, 'ok'); return r !== null && r.path === 'f'; })());
check('resolveTodoCardView returns phases not raw args JSON', (() => {
  const v = resolveTodoCardView(
    { op: 'init', list: [{ phase: 'P', items: ['a'] }] },
    { phases: [{ name: 'P', tasks: [{ id: 't1', content: 'a', status: 'pending', waitingOn: [] }] }], storage: 'session', completedTasks: [] },
    'Remaining items (1):\n  - a [pending] (P) id=t1 ready',
    'done',
  );
  const text = JSON.stringify(v);
  return v.phases.length === 1 && !text.includes('t1') && !text.includes('ready') && !text.includes('"list"');
})());

// ---- user message projection: caption + images, image-only, multi-image order ----
// Reported regression: the user bubble dumped the raw `<attachment>` transport
// wrapper and the auto-vision `[Image analyzed by …]` description as if they
// were the user's own text. The typed projection splits the three surfaces:
// image previews (validated, in order), the user's REAL caption, and an
// optional typed analysis that never masquerades as user text.
{
  const png = 'iVBORw0KGgo=';
  const jpeg = '/9j/4AAQSkZJRg==';

  // caption + image: caption kept verbatim, image extracted, no analysis.
  const ci = userMessageProjection([
    { type: 'text', text: '这个图是什么' },
    { type: 'image', mimeType: 'image/png', data: png },
  ]);
  check('caption+image: caption kept, image extracted, no analysis', ci.text === '这个图是什么' && ci.images.length === 1 && ci.images[0].mimeType === 'image/png' && ci.analysis === undefined, JSON.stringify(ci));

  // image-only: empty caption, NO placeholder text, image present.
  const io = userMessageProjection([{ type: 'image', mimeType: 'image/jpeg', data: jpeg }]);
  check('image-only: empty caption (no placeholder), one image', io.text === '' && io.images.length === 1 && io.analysis === undefined, JSON.stringify(io));

  // multiple images: order preserved, any caption kept.
  const multi = userMessageProjection([
    { type: 'image', mimeType: 'image/png', data: png },
    { type: 'text', text: 'compare these' },
    { type: 'image', mimeType: 'image/jpeg', data: jpeg },
  ]);
  check('multi-image order: images in block order, caption kept', multi.text === 'compare these' && multi.images.length === 2 && multi.images[0].mimeType === 'image/png' && multi.images[1].mimeType === 'image/jpeg', JSON.stringify(multi));

  // hostile image blocks rejected by the projection (same allowlist as userImages).
  const hostile = userMessageProjection([
    { type: 'image', mimeType: 'image/svg+xml', data: png },
    { type: 'text', text: 'hostile image dropped' },
  ]);
  check('projection drops hostile image, keeps caption', hostile.text === 'hostile image dropped' && hostile.images.length === 0, JSON.stringify(hostile));

  // non-array content is safe.
  check('projection(non-array) → empty caption, no images', userMessageProjection(null).text === '' && userMessageProjection(null).images.length === 0);
}

// ---- auto-vision analysis is typed, never user caption ----
// The backend emits `[Image analyzed by {model}: {description}]` into the MODEL
// context only; durable history keeps originals. When it reaches the client
// (legacy/old binary), it must surface as a labeled typed `analysis`, NOT as
// the user's text. A user typing a similar line that is NOT the exact format
// stays as their caption.
{
  const png = 'iVBORw0KGgo=';
  const desc = 'A screenshot of a code editor with a Rust function.';

  // exact backend marker → typed analysis, dropped from caption.
  const p = userMessageProjection([
    { type: 'text', text: '这个图是什么' },
    { type: 'image', mimeType: 'image/png', data: png },
    { type: 'text', text: `[Image analyzed by vision-model-1: ${desc}]` },
  ]);
  check('analysis marker → typed analysis (model + description)', p.analysis !== undefined && p.analysis.model === 'vision-model-1' && p.analysis.description === desc, JSON.stringify(p));
  check('analysis marker is NOT in the user caption', p.text === '这个图是什么' && !p.text.includes('Image analyzed'), JSON.stringify(p));
  check('analysis marker does not consume the image', p.images.length === 1 && p.images[0].mimeType === 'image/png', JSON.stringify(p));

  // first marker wins; a second marker is ignored (backend emits one).
  const two = userMessageProjection([
    { type: 'image', mimeType: 'image/png', data: png },
    { type: 'text', text: '[Image analyzed by m1: first]' },
    { type: 'text', text: '[Image analyzed by m2: second]' },
  ]);
  check('first analysis marker wins', two.analysis !== undefined && two.analysis.model === 'm1' && two.analysis.description === 'first', JSON.stringify(two));
  check('second analysis marker dropped (not kept as caption)', two.text === '' && !two.text.includes('Image analyzed'), JSON.stringify(two));

  // marker without a model or description is not recognized → kept as caption.
  const empty = userMessageProjection([{ type: 'text', text: '[Image analyzed by : ]' }]);
  check('malformed analysis marker (empty model/desc) kept as caption', empty.text === '[Image analyzed by : ]' && empty.analysis === undefined, JSON.stringify(empty));

  // a user line that only LOOKS like the marker (no closing ]) stays caption.
  const lookalike = userMessageProjection([{ type: 'text', text: '[Image analyzed by m: hello world' }]);
  check('lookalike (no closing bracket) kept as caption', lookalike.text === '[Image analyzed by m: hello world' && lookalike.analysis === undefined, JSON.stringify(lookalike));

  // parseImageAnalysis unit: exact shape only.
  check('parseImageAnalysis exact', parseImageAnalysis('[Image analyzed by gpt-4o: a screen with code]')?.model === 'gpt-4o');
  check('parseImageAnalysis rejects non-string', parseImageAnalysis(42) === null && parseImageAnalysis(null) === null);
  check('parseImageAnalysis rejects unrelated text', parseImageAnalysis('just a normal message') === null);
}

// ---- legacy <attachment> transport wrapper is stripped only when images prove it ----
// Old binaries wrapped image transport in `<attachment>…</attachment>` scaffolding.
// Structurally provable (balanced tags over the whole block): dropped entirely
// from the caption ONLY when the message carries real image blocks (its inner
// text is transport scaffolding, not the user's caption). A hostile user-typed
// `<attachment>` with no images stays as their literal text.
{
  const png = 'iVBORw0KGgo=';

  // wrapper + image: wrapper dropped entirely, the separate caption block kept.
  const wrapped = userMessageProjection([
    { type: 'text', text: '<attachment type="image/png">ref-1</attachment>' },
    { type: 'image', mimeType: 'image/png', data: png },
    { type: 'text', text: '这个图是什么' },
  ]);
  check('wrapper + image: wrapper dropped, caption kept', wrapped.text === '这个图是什么' && !wrapped.text.includes('<attachment') && !wrapped.text.includes('ref-1') && wrapped.images.length === 1, JSON.stringify(wrapped));

  // wrapper that embeds text inside it: the inner text is transport
  // scaffolding, not the user's caption → dropped entirely (image-only bubble).
  const innerCaption = userMessageProjection([
    { type: 'text', text: '<attachment>这个图是什么</attachment>' },
    { type: 'image', mimeType: 'image/png', data: png },
  ]);
  check('wrapper with inner text drops it (transport scaffolding, not caption)', innerCaption.text === '' && innerCaption.images.length === 1, JSON.stringify(innerCaption));

  // self-closing wrapper dropped when images present.
  const selfClose = userMessageProjection([
    { type: 'text', text: '<attachment src="img.png"/>' },
    { type: 'image', mimeType: 'image/png', data: png },
  ]);
  check('self-closing wrapper dropped (images present)', selfClose.text === '' && selfClose.images.length === 1, JSON.stringify(selfClose));

  // hostile: user literally types <attachment> with NO image → preserved verbatim.
  const hostile = userMessageProjection([{ type: 'text', text: '<attachment>hello</attachment>' }]);
  check('hostile <attachment> with no image kept as literal caption', hostile.text === '<attachment>hello</attachment>' && hostile.images.length === 0, JSON.stringify(hostile));

  // hostile: unbalanced <attachment> (no close) with image → preserved (not structurally a wrapper).
  const unbalanced = userMessageProjection([
    { type: 'text', text: '<attachment>not closed' },
    { type: 'image', mimeType: 'image/png', data: png },
  ]);
  check('unbalanced <attachment> kept (not structurally a wrapper)', unbalanced.text === '<attachment>not closed', JSON.stringify(unbalanced));

  // wrapper + analysis marker + image: both stripped from caption, analysis typed.
  const both = userMessageProjection([
    { type: 'text', text: '<attachment>ref</attachment>' },
    { type: 'image', mimeType: 'image/png', data: png },
    { type: 'text', text: '[Image analyzed by vm: a diagram]' },
    { type: 'text', text: '这个图是什么' },
  ]);
  check('wrapper + analysis + image: caption is user text only', both.text === '这个图是什么' && both.analysis?.model === 'vm' && both.analysis?.description === 'a diagram' && both.images.length === 1, JSON.stringify(both));
}

// ---- restored user message projects through the typed surface (reload parity) ----
// messagesToItems must use the same userMessageProjection as the live path so
// reload restores image previews + real caption + collapsed analysis, never the
// raw wrapper or the description-as-user-text.
{
  const png = 'iVBORw0KGgo=';
  const items = messagesToItems([
    { role: 'user', content: [{ type: 'text', text: '这个图是什么' }, { type: 'image', mimeType: 'image/png', data: png }, { type: 'text', text: '[Image analyzed by vm: screen with code]' }] },
    { role: 'user', content: [{ type: 'image', mimeType: 'image/png', data: png }] },
    { role: 'user', content: [{ type: 'text', text: '<attachment>ref</attachment>' }, { type: 'image', mimeType: 'image/png', data: png }, { type: 'text', text: 'plain caption' }] },
  ]);
  const r0 = items[0];
  const r1 = items[1];
  const r2 = items[2];
  check('restore: caption+image+analysis typed', r0.kind === 'user' && r0.text === '这个图是什么' && r0.images?.length === 1 && r0.analysis?.model === 'vm' && r0.analysis?.description === 'screen with code', JSON.stringify(r0));
  check('restore: image-only has empty text + image, no analysis', r1.kind === 'user' && r1.text === '' && r1.images?.length === 1 && (r1.analysis ?? undefined) === undefined, JSON.stringify(r1));
  check('restore: wrapper dropped, caption kept, no analysis', r2.kind === 'user' && r2.text === 'plain caption' && r2.images?.length === 1 && (r2.analysis ?? undefined) === undefined, JSON.stringify(r2));
}

// ---- optimistic→ACK reconcile: analysis does not break identity (durable twin keeps analysis) ----
// The optimistic bubble carries the user's caption + images but NO analysis
// (analysis is a backend projection). The durable twin carries the same
// caption + images plus analysis. Identity is caption+images only (analysis is
// not user-authored), so the optimistic bubble dedups against its durable twin
// and the durable item — which carries the analysis — is kept.
{
  const png = 'iVBORw0KGgo=';
  const optimistic = { kind: 'user', id: 'l-1', text: '这个图是什么', optimistic: true, images: [{ mimeType: 'image/png', data: png }] };
  const durable = { kind: 'user', id: 'h-1', text: '这个图是什么', optimistic: false, images: [{ mimeType: 'image/png', data: png }], analysis: { model: 'vm', description: 'a screen' } };
  const merged = mergeAuthoritativeItems([durable], [optimistic]);
  const users = merged.filter((i) => i.kind === 'user');
  check('optimistic dedups against durable twin (analysis on durable does not block dedup)', users.length === 1 && !users.some((i) => i.id === 'l-1'), JSON.stringify(merged));
  check('merged keeps the durable item with its analysis', users.some((i) => i.kind === 'user' && i.id === 'h-1' && i.analysis?.model === 'vm'), JSON.stringify(merged));
}

// ---- typed Read projection: compact+expanded, media stripped, internal URIs, live/restored parity ----
{
  const body = Array.from({ length: 10 }, (_, n) => `line-${n + 1}`).join('\n');
  const truncated = `${body}\n\n[4320 more lines in file. Use offset=3885 to continue.]`;
  const ordinary = parseReadCard({ path: 'big.txt' }, truncated);
  check('ordinary read keeps path vocabulary', ordinary !== null && ordinary.path === 'big.txt' && ordinary.kind === 'file' && ordinary.title === 'Read', JSON.stringify(ordinary));
  check('ordinary read headline uses path/range/count', ordinary !== null && ordinary.headline === 'big.txt · lines 1-3884 · 10 returned', ordinary?.headline);
  check('ordinary compact is six-row tail with omission marker', ordinary !== null && ordinary.compactBody === ['… 4 more lines', 'line-5', 'line-6', 'line-7', 'line-8', 'line-9', 'line-10'].join('\n'), ordinary?.compactBody);
  check('ordinary expanded retains full bounded source', ordinary !== null && ordinary.expandedBody === body, ordinary?.expandedBody);
  check('ordinary compact omits four lines', ordinary !== null && ordinary.omitted === 4, String(ordinary?.omitted));
  check('ordinary expanded has no offset notice', ordinary !== null && !ordinary.expandedBody.includes('Use offset='), ordinary?.expandedBody);

  const skillSource = '---\nname: research\ndescription: "Deep-dive codebase researcher. Produces structured docs."\n---\n# Research\n\nBody line.';
  const skill = parseReadCard({ path: 'skill://research' }, skillSource);
  check('bare skill uses name title', skill !== null && skill.kind === 'skill' && skill.title === 'research' && skill.path === 'research', JSON.stringify(skill));
  check('skill description is first paragraph', skill !== null && skill.skill?.description === 'Deep-dive codebase researcher. Produces structured docs.', skill?.skill?.description);
  check('skill body follows description', skill !== null && skill.expandedBody.includes('# Research') && skill.expandedBody.includes('Body line.'), skill?.expandedBody);
  check('skill compact is head-first body', skill !== null && skill.compactBody.split('\n')[0] === '# Research', skill?.compactBody);
  check('skill YAML never renders', skill !== null && !skill.compactBody.includes('name:') && !skill.expandedBody.includes('description:') && !skill.expandedBody.includes('---'), skill?.expandedBody);

  const longBody = Array.from({ length: 20 }, (_, n) => `body-${n + 1}`).join('\n');
  const longSkill = parseReadCard({ path: 'skill://long' }, `---\nname: long\ndescription: "Lead description."\n---\n${longBody}`);
  check('long skill compact keeps description, head body, and omission marker', longSkill !== null && longSkill.skill?.description === 'Lead description.' && longSkill.compactBody.startsWith('body-1') && longSkill.compactBody.endsWith('… 14 more lines') && !longSkill.compactBody.includes('body-20'), longSkill?.compactBody);
  check('long skill expanded retains full body', longSkill !== null && longSkill.expandedBody.includes('body-20') && longSkill.omitted === 14, `${longSkill?.omitted}:${longSkill?.expandedBody}`);

  const folded = parseReadCard({ path: 'skill://folded' }, '---\nname: folded\ndescription: >\n  Folded line one\n  folds into a\n  single line\n---\n# Folded\n\nBody.');
  check('folded skill description joins spaces', folded !== null && folded.skill?.description === 'Folded line one folds into a single line', folded?.skill?.description);
  const literal = parseReadCard({ path: 'skill://literal' }, '---\nname: literal\ndescription: |\n  Literal line one\n  keeps the newline\n---\n# Literal\n\nBody.');
  check('literal skill description keeps newlines', literal !== null && literal.skill?.description === 'Literal line one\nkeeps the newline', literal?.skill?.description);

  const sub = parseReadCard({ path: 'skill://research/asset.md' }, '---\nname: asset\ndescription: "not the skill"\n---\nasset body');
  check('skill subresource stays raw file', sub !== null && sub.kind === 'file' && sub.title === 'Read' && sub.expandedBody.includes('name: asset'), JSON.stringify(sub));
  const missing = parseReadCard({ path: 'skill://missing' }, 'not found');
  check('frontmatter-less skill stays raw', missing !== null && missing.kind === 'skill' && missing.title === 'Read' && missing.expandedBody === 'not found' && missing.skill === undefined, JSON.stringify(missing));

  const agent = parseReadCard({ path: 'agent://DeepSeek' }, 'child output');
  check('agent URI classifies without opaque key', agent !== null && agent.kind === 'agent' && agent.path === 'DeepSeek' && !agent.path.includes('agent://') && !agent.headline.includes('agent://'), JSON.stringify(agent));
  const history = parseReadCard({ path: 'history://aa0e8c2e-4b1a-4d3f-9c7e-1f2a3b4c5d6e' }, 'history body');
  check('history UUID key is omitted', history !== null && history.kind === 'history' && history.path === 'History' && !history.path.includes('history://') && !history.headline.includes('aa0e8c2e'), JSON.stringify(history));
  const artifact = parseReadCard({ path: 'artifact://aa0e8c2e-4b1a-4d3f-9c7e-1f2a3b4c5d6e' }, 'artifact body');
  check('artifact UUID key is omitted', artifact !== null && artifact.kind === 'artifact' && artifact.path === 'Artifact' && !artifact.headline.includes('artifact://'), JSON.stringify(artifact));

  const sentinel = 'binary-payload-sentinel'.repeat(8);
  const live = applyToolSnapshot(
    [{ kind: 'toolCard', id: 'tc1', toolCallId: 'media', toolName: 'read', args: { path: 'clip.mp4' }, status: 'running', result: '' }],
    'media',
    'Video attachment loaded',
    'done',
    { media: [{ kind: 'video', mimeType: 'video/mp4', data: sentinel, name: 'clip.mp4', sizeBytes: 12 }] },
    [{ kind: 'video', mimeType: 'video/mp4', data: sentinel, alt: 'clip.mp4' }],
  )[0];
  check('live read snapshot retains bounded video media', live.kind === 'toolCard'
    && live.media?.length === 1 && live.details === undefined
    && live.result === 'Video attachment loaded', JSON.stringify(live));
  const liveView = live.kind === 'toolCard' ? parseReadCard(live.args, live.result) : null;
  check('live read view has no sentinel in text body', liveView !== null
    && liveView.expandedBody === 'Video attachment loaded'
    && !JSON.stringify(liveView).includes(sentinel), JSON.stringify(liveView));

  const restored = messagesToItems([
    { role: 'assistant', content: [{ type: 'toolCall', id: 'read-1', name: 'read', arguments: { path: 'big.txt' } }] },
    { role: 'toolResult', toolCallId: 'read-1', content: [{ type: 'text', text: truncated }], details: { media: [{ kind: 'video', mimeType: 'video/mp4', data: sentinel, name: 'clip.mp4', sizeBytes: 12 }] } },
  ]);
  const restoredCard = restored.find((item) => item.kind === 'toolCard');
  check('restored read keeps full bounded result', restoredCard?.kind === 'toolCard' && restoredCard.result === truncated, restoredCard && restoredCard.kind === 'toolCard' ? restoredCard.result : '');
  check('restored read strips details but keeps validated media', restoredCard?.kind === 'toolCard' && restoredCard.details === undefined, JSON.stringify(restoredCard));
  const restoredView = restoredCard?.kind === 'toolCard' ? parseReadCard(restoredCard.args, restoredCard.result) : null;
  check('live/restored compact parity', restoredView !== null && ordinary !== null && restoredView.compactBody === ordinary.compactBody && restoredView.expandedBody === ordinary.expandedBody, restoredView?.compactBody);
  check('restored projection has no sentinel or URI key', restoredView !== null && !JSON.stringify(restoredView).includes(sentinel) && !JSON.stringify(restored).includes('data:') && !JSON.stringify(restoredView).includes('agent://'), JSON.stringify(restoredView));

  const skillRestored = messagesToItems([
    { role: 'assistant', content: [{ type: 'toolCall', id: 's', name: 'read', arguments: { path: 'skill://research' } }] },
    { role: 'toolResult', toolCallId: 's', content: [{ type: 'text', text: skillSource }] },
  ]);
  const skillCard = skillRestored.find((item) => item.kind === 'toolCard');
  const skillRestoredView = skillCard?.kind === 'toolCard' ? parseReadCard(skillCard.args, skillCard.result) : null;
  check('restored skill matches live projection', skillRestoredView !== null && skill !== null && skillRestoredView.title === skill.title && skillRestoredView.expandedBody === skill.expandedBody && skillRestoredView.compactBody === skill.compactBody, JSON.stringify(skillRestoredView));
}

// ---- Rejected Task card: cross-call identity contamination is impossible ----
// A rejected/no-spawn Task card (terminal `error`, no spawn details) carries
// only args-preview children (name/agent/task — no jobId/agentId). Correlation
// must require an authoritative spawn identity with exact jobId, so an
// unrelated same-agent job_updated / job_list snapshot / message_delivered /
// agent_updated can never latch onto the rejected card, flip its terminal
// status, or fabricate children. The card stays byte-for-byte terminal with
// zero projected children. A successful card with authoritative spawn details
// carrying an exact jobId still correlates and updates normally.
{
  // The exact reported rejected args: agent/name/task, status error, no details.
  const rejected = {
    kind: 'toolCard' as const,
    id: 'rejected-glm',
    toolCallId: 'rejected-glm-call',
    toolName: 'task',
    args: { agent: 'glm', name: 'glm', task: 'bad' },
    status: 'error' as const,
    result: 'rejected: a batch tasks[] call requires a context briefing',
    details: undefined,
  };
  const snapshot = (item) => ({
    kind: item.kind, id: item.id, toolCallId: item.toolCallId, toolName: item.toolName,
    args: item.args, status: item.status, result: item.result, details: item.details,
  });
  const before = snapshot(rejected);

  // (1) Unrelated same-agent job_updated (running) must NOT attach.
  const afterJob = applyJobUpdated([rejected], {
    job: { id: 'job-second', agentId: 'glm', agent: 'glm', status: 'running' },
  });
  check('rejected card unchanged by unrelated same-agent job_updated (byte-for-byte)', JSON.stringify(snapshot(afterJob[0])) === JSON.stringify(before), JSON.stringify(snapshot(afterJob[0])));
  check('rejected card status stays error after unrelated job_updated', afterJob[0].status === 'error', JSON.stringify(afterJob[0].status));
  check('rejected card keeps no spawn details after unrelated job_updated', afterJob[0].details === undefined, JSON.stringify(afterJob[0].details));
  const jobView = resolveTaskCardView(afterJob[0].args, afterJob[0].details, afterJob[0].status);
  check('rejected card projects zero children after unrelated job_updated', !!jobView && jobView.children.length === 0, JSON.stringify(jobView));
  check('rejected card taskCardStatus stays error after unrelated job_updated', taskCardStatus(afterJob[0]) === 'error', JSON.stringify(taskCardStatus(afterJob[0])));

  // (2) Reconnect snapshot (multiple same-agent jobs) must NOT attach either.
  const afterSnap = applyJobSnapshot([rejected], [
    { id: 'job-second', agentId: 'glm', agent: 'glm', status: 'running' },
    { id: 'job-third', agentId: 'glm', agent: 'glm', status: 'completed', result: { output: 'unrelated', error: null } },
  ]);
  check('rejected card unchanged by same-agent job_list snapshot (byte-for-byte)', JSON.stringify(snapshot(afterSnap[0])) === JSON.stringify(before), JSON.stringify(snapshot(afterSnap[0])));
  check('rejected card status stays error after snapshot', afterSnap[0].status === 'error', JSON.stringify(afterSnap[0].status));
  const snapView = resolveTaskCardView(afterSnap[0].args, afterSnap[0].details, afterSnap[0].status);
  check('rejected card projects zero children after snapshot', !!snapView && snapView.children.length === 0, JSON.stringify(snapView));

  // name/agent/task never authorize attachment across every update channel.
  const afterMsg = applyMessageDelivered([rejected], {
    message: { from: 'glm', body: 'unrelated child→Main body', to: 'Main' },
  });
  check('rejected card unchanged by same-name message_delivered', JSON.stringify(snapshot(afterMsg[0])) === JSON.stringify(before), JSON.stringify(snapshot(afterMsg[0])));
  check('rejected card status stays error after message_delivered', afterMsg[0].status === 'error', JSON.stringify(afterMsg[0].status));

  const afterAgent = applyAgentUpdated([rejected], {
    agent: { id: 'glm', displayName: 'GLM Agent', status: 'running' },
  });
  check('rejected card unchanged by same-name agent_updated', JSON.stringify(snapshot(afterAgent[0])) === JSON.stringify(before), JSON.stringify(snapshot(afterAgent[0])));
  check('rejected card status stays error after agent_updated', afterAgent[0].status === 'error', JSON.stringify(afterAgent[0].status));

  // (3) Authoritative spawn details carrying the exact jobId DO correlate:
  // a matching job_updated updates the child status/result and settles parent.
  const spawned = {
    ...rejected,
    status: 'running' as const,
    result: '',
    details: [{ index: 0, jobId: 'job-real', agentId: 'glm', agent: 'glm', status: 'queued' }],
  };
  const afterReal = applyJobUpdated([spawned], {
    job: { id: 'job-real', agentId: 'glm', agent: 'glm', status: 'completed', result: { output: 'real child output', error: null } },
  });
  const realView = resolveTaskCardView(afterReal[0].args, afterReal[0].details, afterReal[0].status);
  check('authoritative spawn correlates with exact jobId (one child)', !!realView && realView.children.length === 1, JSON.stringify(realView));
  check('authoritative child keeps exact jobId after matching job_updated', !!realView && realView.children[0].jobId === 'job-real', JSON.stringify(realView?.children?.[0]));
  check('authoritative child status updated from matching job_updated', !!realView && realView.children[0].status === 'completed', JSON.stringify(realView?.children?.[0]));
  check('authoritative child result sentence from matching job_updated', !!realView && realView.children[0].result === 'real child output', JSON.stringify(realView?.children?.[0]));
  check('authoritative parent settles to done after matching job_updated', taskCardStatus(afterReal[0]) === 'done', JSON.stringify(taskCardStatus(afterReal[0])));

  // An unrelated job with the SAME agentId but a DIFFERENT jobId must NOT
  // attach to the spawned card: name/agent never authorizes, only exact jobId.
  const spawnedBefore = snapshot(spawned);
  const afterUnrelated = applyJobUpdated([spawned], {
    job: { id: 'job-other', agentId: 'glm', agent: 'glm', status: 'running' },
  });
  check('spawned card unchanged by same-agent different-jobId job_updated', JSON.stringify(snapshot(afterUnrelated[0])) === JSON.stringify(spawnedBefore), JSON.stringify(snapshot(afterUnrelated[0])));
  check('spawned card status stays running after unrelated job_updated', afterUnrelated[0].status === 'running', JSON.stringify(afterUnrelated[0].status));
  const unrelatedView = resolveTaskCardView(afterUnrelated[0].args, afterUnrelated[0].details, afterUnrelated[0].status);
  check('spawned card projects the original queued child after unrelated job_updated', !!unrelatedView && unrelatedView.children.length === 1 && unrelatedView.children[0].jobId === 'job-real' && unrelatedView.children[0].status === 'queued', JSON.stringify(unrelatedView));
}

// ---- job_list snapshot: empty is authoritative, undefined/non-array is a no-op ----
// Missing/malformed jobs must never mass-cancel (RPC failure). An array,
// including `[]`, is authoritative: listed jobs patch by exact jobId, then
// every spawned queued/running child absent from that set is cancelled
// without inventing a result. name/agent/task never attach. Rejected cards
// and terminal children stay byte-for-byte.
{
  const snapshot = (item) => ({
    kind: item.kind, id: item.id, toolCallId: item.toolCallId, toolName: item.toolName,
    args: item.args, status: item.status, result: item.result, details: item.details,
  });
  const spawnedRunning = {
    kind: 'toolCard',
    id: 'tc-snap',
    toolCallId: 'task-snap',
    toolName: 'task',
    args: { agent: 'reviewer', name: 'Alpha', task: 'Review the adapter' },
    status: 'running',
    result: '',
    details: [{ index: 0, jobId: 'job-a', agentId: 'Alpha', agent: 'reviewer', status: 'running' }],
  };
  const before = snapshot(spawnedRunning);

  const emptySnap = applyJobSnapshot([spawnedRunning], []);
  const emptyView = resolveTaskCardView(emptySnap[0].args, emptySnap[0].details, emptySnap[0].status);
  check('empty authoritative job_list cancels omitted queued/running spawned children',
    !!emptyView && emptyView.children.length === 1 && emptyView.children[0].jobId === 'job-a'
    && emptyView.children[0].status === 'cancelled' && emptyView.children[0].result === undefined
    && taskCardStatus(emptySnap[0]) === 'cancelled', JSON.stringify(emptyView));

  // Reconnect omission-cancel must keep the child's DOM provenance. Real
  // frame sequence: task_spawn receipt (TaskSpawn wire carries only
  // index/jobId/agentId/agent/status) -> live job_updated with the full
  // JobSnapshot (parentId/todoTaskId, runtime.rs publish_job) -> backend
  // killed without WS close -> reconnect job_list omits the job -> the card
  // settles Cancelled while every identity attribute remains on the element.
  const spawnedRecon = {
    kind: 'toolCard',
    id: 'tc-recon',
    toolCallId: 'task-recon-call',
    toolName: 'task',
    args: { agent: 'reviewer', name: 'Recon', task: 'reconnect work' },
    status: 'running',
    result: '',
    details: [{ index: 0, jobId: 'job-r1', agentId: 'Alpha', agent: 'reviewer', status: 'queued' }],
  };
  const withProvenance = applyJobUpdated([spawnedRecon], {
    job: { id: 'job-r1', agentId: 'Alpha', agent: 'reviewer', parentId: 'Main', todoTaskId: 'todo-r1', status: 'running' },
  });
  const reconCut = taskJobIds(withProvenance);
  const reconCancelled = applyJobSnapshot(withProvenance, [], reconCut);
  const reconView = resolveTaskCardView(reconCancelled[0].args, reconCancelled[0].details, reconCancelled[0].status);
  const reconHtml = reconView
    ? renderToStaticMarkup(createElement(TaskToolCardBody, { item: reconCancelled[0], taskView: reconView }))
    : '';
  check('omission-cancelled child keeps DOM provenance and settles Cancelled',
    reconCut.has('job-r1')
    && !!reconView && reconView.children[0].jobId === 'job-r1' && reconView.children[0].status === 'cancelled'
    && taskCardStatus(reconCancelled[0]) === 'cancelled'
    && reconHtml.includes('data-job-id="job-r1"')
    && reconHtml.includes('data-agent-id="Alpha"')
    && reconHtml.includes('data-parent-id="Main"')
    && reconHtml.includes('data-todo-task-id="todo-r1"')
    && reconHtml.includes('data-status="cancelled"'), reconHtml);
  // A child that never received a job_updated (backend died between spawn
  // receipt and first publish) still settles Cancelled with the identity it
  // provably has (jobId/agentId); unknown parent identity renders no attr.
  const bareCut = taskJobIds([spawnedRecon]);
  const bareCancelled = applyJobSnapshot([spawnedRecon], [], bareCut);
  const bareView = resolveTaskCardView(bareCancelled[0].args, bareCancelled[0].details, bareCancelled[0].status);
  const bareHtml = bareView
    ? renderToStaticMarkup(createElement(TaskToolCardBody, { item: bareCancelled[0], taskView: bareView }))
    : '';
  check('omission-cancelled child without prior job_updated keeps jobId/agentId and settles Cancelled',
    bareCut.has('job-r1')
    && !!bareView && bareView.children[0].jobId === 'job-r1' && bareView.children[0].agentId === 'Alpha'
    && bareView.children[0].status === 'cancelled'
    && taskCardStatus(bareCancelled[0]) === 'cancelled'
    && bareHtml.includes('data-job-id="job-r1"')
    && bareHtml.includes('data-agent-id="Alpha"')
    && !bareHtml.includes('data-parent-id')
    && !bareHtml.includes('data-todo-task-id'), bareHtml);

  const undefSnap = applyJobSnapshot([spawnedRunning], undefined);
  check('undefined job_list is a no-op (no mass-cancel on RPC failure)',
    JSON.stringify(snapshot(undefSnap[0])) === JSON.stringify(before)
    && resolveTaskCardView(undefSnap[0].args, undefSnap[0].details, undefSnap[0].status)?.children[0].status === 'running',
    JSON.stringify(snapshot(undefSnap[0])));
  const malformedSnap = applyJobSnapshot([spawnedRunning], { jobs: [] });
  check('non-array job_list is a no-op',
    JSON.stringify(snapshot(malformedSnap[0])) === JSON.stringify(before), JSON.stringify(snapshot(malformedSnap[0])));

  const unrelatedSnap = applyJobSnapshot([spawnedRunning], [
    { id: 'job-other', agentId: 'Alpha', agent: 'reviewer', status: 'running' },
  ]);
  const unrelatedView = resolveTaskCardView(unrelatedSnap[0].args, unrelatedSnap[0].details, unrelatedSnap[0].status);
  check('unrelated job_list does not attach by agent/name/task',
    !!unrelatedView && unrelatedView.children.length === 1 && unrelatedView.children[0].jobId === 'job-a'
    && unrelatedView.children[0].status === 'cancelled', JSON.stringify(unrelatedView));

  const rejected = {
    kind: 'toolCard',
    id: 'rejected-snap',
    toolCallId: 'rejected-snap-call',
    toolName: 'task',
    args: { agent: 'glm', name: 'glm', task: 'bad' },
    status: 'error',
    result: 'rejected: a batch tasks[] call requires a context briefing',
    details: undefined,
  };
  const rejectedBefore = snapshot(rejected);
  const rejectedEmpty = applyJobSnapshot([rejected], []);
  check('rejected card unchanged by empty authoritative job_list (byte-for-byte)',
    JSON.stringify(snapshot(rejectedEmpty[0])) === JSON.stringify(rejectedBefore), JSON.stringify(snapshot(rejectedEmpty[0])));
  const rejectedUnrelated = applyJobSnapshot([rejected], [
    { id: 'job-second', agentId: 'glm', agent: 'glm', status: 'running' },
  ]);
  check('rejected card unchanged by unrelated job_list (byte-for-byte)',
    JSON.stringify(snapshot(rejectedUnrelated[0])) === JSON.stringify(rejectedBefore), JSON.stringify(snapshot(rejectedUnrelated[0])));

  const provenanced = applyJobSnapshot([spawnedRunning], [{
    id: 'job-a',
    agentId: 'Alpha',
    agent: 'reviewer',
    parentId: 'Main',
    todoTaskId: 'todo-a',
    status: 'completed',
    result: { output: 'ok', error: null },
  }]);
  const provenancedView = resolveTaskCardView(provenanced[0].args, provenanced[0].details, provenanced[0].status);
  const provenancedHtml = provenancedView
    ? renderToStaticMarkup(createElement(TaskToolCardBody, { item: provenanced[0], taskView: provenancedView }))
    : '';
  check('successful snapshot provenance is DOM-ready (job/parent/todo data attrs)',
    provenancedHtml.includes('data-job-id="job-a"')
    && provenancedHtml.includes('data-agent-id="Alpha"')
    && provenancedHtml.includes('data-parent-id="Main"')
    && provenancedHtml.includes('data-todo-task-id="todo-a"')
    && provenancedHtml.includes('data-status="completed"'), provenancedHtml);
  const terminalEmpty = applyJobSnapshot(provenanced, []);
  const terminalView = resolveTaskCardView(terminalEmpty[0].args, terminalEmpty[0].details, terminalEmpty[0].status);
  check('empty snapshot preserves terminal Task child (no cancel of completed)',
    !!terminalView && terminalView.children[0].status === 'completed' && terminalView.children[0].jobId === 'job-a',
    JSON.stringify(terminalView));

  const failedRead = parseReadCard({ path: '/tmp/missing.txt' }, 'ENOENT: no such file or directory', 'error');
  check('failed Read headline is the redacted path and never says returned',
    failedRead !== null && failedRead.headline === '/tmp/missing.txt' && failedRead.linesReturned === 0
    && !failedRead.headline.includes('returned') && !failedRead.compactBody.includes('returned')
    && !failedRead.expandedBody.includes('returned'), JSON.stringify(failedRead));
  check('failed Read retains the actionable body',
    failedRead !== null && failedRead.expandedBody.includes('ENOENT: no such file or directory')
    && failedRead.compactBody.includes('ENOENT: no such file or directory'), JSON.stringify(failedRead));
  const cancelledRead = parseReadCard({ path: 'secret.txt' }, 'read cancelled', 'cancelled');
  check('cancelled Read headline is the path with zero returned lines',
    cancelledRead !== null && cancelledRead.headline === 'secret.txt' && cancelledRead.linesReturned === 0
    && !cancelledRead.headline.includes('returned') && cancelledRead.expandedBody === 'read cancelled',
    JSON.stringify(cancelledRead));
  const oneLine = parseReadCard({ path: 'one.txt' }, 'only line', 'done');
  check('successful one-line read reports 1 returned',
    oneLine !== null && oneLine.headline === 'one.txt · 1 returned' && oneLine.linesReturned === 1
    && oneLine.expandedBody === 'only line', JSON.stringify(oneLine));
}

// ---- job_list request-window cut: only request-time jobIds omission-settle ----
// A Task spawn/job_updated landing between the job_list request and its
// response is authoritative and must NOT be cancelled by the older list.
// Children captured at the request cut still omission-settle; listed jobs
// patch regardless of cut membership.
{
  const taskCard = (id, children) => ({
    kind: 'toolCard',
    id,
    toolCallId: `${id}-call`,
    toolName: 'task',
    args: { agent: 'reviewer', name: id, task: 'work' },
    status: 'running',
    result: '',
    details: children,
  });
  const oldCard = taskCard('tc-old', [
    { index: 0, jobId: 'job-old', agentId: 'Alpha', agent: 'reviewer', status: 'running' },
  ]);
  const newCard = taskCard('tc-new', [
    { index: 0, jobId: 'job-new', agentId: 'Beta', agent: 'reviewer', status: 'running' },
  ]);
  // Cut captured at request time: only job-old known. job-new arrives inside
  // the request window (live job_updated), then the older job_list (which
  // never saw either job as settled) answers with an unrelated listing.
  const cut = taskJobIds([oldCard]);
  check('taskJobIds captures exact request-time Task jobIds',
    cut.size === 1 && cut.has('job-old') && !cut.has('job-new'), JSON.stringify([...cut]));
  const reconciled = applyJobSnapshot([oldCard, newCard], [
    { id: 'job-other', agentId: 'Gamma', agent: 'reviewer', status: 'running' },
  ], cut);
  const oldView = resolveTaskCardView(reconciled[0].args, reconciled[0].details, reconciled[0].status);
  const newView = resolveTaskCardView(reconciled[1].args, reconciled[1].details, reconciled[1].status);
  check('request-cut child absent from job_list is omission-settled',
    !!oldView && oldView.children[0].jobId === 'job-old' && oldView.children[0].status === 'cancelled',
    JSON.stringify(oldView));
  check('request-window child absent from job_list stays authoritative (not cancelled)',
    !!newView && newView.children[0].jobId === 'job-new' && newView.children[0].status === 'running'
    && taskCardStatus(reconciled[1]) !== 'cancelled', JSON.stringify(newView));
  // Listed jobs patch by exact jobId even without cut membership.
  const patched = applyJobSnapshot([newCard], [
    { id: 'job-new', agentId: 'Beta', agent: 'reviewer', status: 'completed', result: { output: 'done', error: null } },
  ], new Set());
  const patchedView = resolveTaskCardView(patched[0].args, patched[0].details, patched[0].status);
  check('listed job patches by exact jobId regardless of cut membership',
    !!patchedView && patchedView.children[0].status === 'completed' && patchedView.children[0].result === 'done',
    JSON.stringify(patchedView));
  // Legacy two-arg behavior (no cut) still omission-settles every
  // queued/running child.
  const legacy = applyJobSnapshot([oldCard, newCard], []);
  const legacyNewView = resolveTaskCardView(legacy[1].args, legacy[1].details, legacy[1].status);
  check('no cut preserves legacy omission-settle for all queued/running children',
    !!legacyNewView && legacyNewView.children[0].status === 'cancelled', JSON.stringify(legacyNewView));
}

// ---- App cold-cache bootstrap integration: historical child settles on reconnect ----
// Simulates the exact App bootstrap ordering (streaming preserveInflight=true
// and settled false branches): the mirror starts EMPTY (cold cache), the
// received authoritative history is materialized into the mirror BEFORE the
// request cut is captured, an in-window spawn lands between request and
// response, and job_list=[] then omission-settles only the cut member. A cut
// captured from the pre-merge cold mirror (the regression) would be empty and
// leave the historical child Running forever.
{
  const historyCard = {
    kind: 'toolCard',
    id: 'tc-hist',
    toolCallId: 'task-hist-call',
    toolName: 'task',
    args: { agent: 'reviewer', name: 'Hist', task: 'historical work' },
    status: 'running',
    result: '',
    details: [{ index: 0, jobId: 'job-old', agentId: 'Alpha', agent: 'reviewer', status: 'running' }],
  };
  const windowCard = {
    kind: 'toolCard',
    id: 'tc-window',
    toolCallId: 'task-window-call',
    toolName: 'task',
    args: { agent: 'reviewer', name: 'Window', task: 'in-window work' },
    status: 'running',
    result: '',
    details: [{ index: 0, jobId: 'job-new', agentId: 'Beta', agent: 'reviewer', status: 'running' }],
  };
  for (const preserveInflight of [true, false]) {
    const branch = preserveInflight ? 'streaming' : 'settled';
    // Cold cache: no items for the target session before bootstrap.
    let mirrorItems = [];
    // Bootstrap step 1: materialize the received authoritative history.
    mirrorItems = mergeAuthoritativeItems([historyCard], mirrorItems, preserveInflight);
    // Bootstrap step 2: capture the request cut from the exact merged state.
    const cut = taskJobIds(mirrorItems);
    check(`${branch} cold-cache cut includes the historical running child`,
      cut.size === 1 && cut.has('job-old'), JSON.stringify([...cut]));
    // In-window spawn between request issue and response.
    mirrorItems = [...mirrorItems, windowCard];
    // Response: merge history again (window events retained) and reconcile.
    const merged = mergeAuthoritativeItems([historyCard], mirrorItems, preserveInflight);
    check(`${branch} bootstrap merge keeps history and in-window Task cards`,
      merged.length === 2 && merged[0]?.id === 'tc-hist' && merged[1]?.id === 'tc-window',
      JSON.stringify(merged.map((item) => item.id)));
    const reconciled = applyJobSnapshot(merged, [], cut);
    check(`${branch} bootstrap job_list=[] keeps both Task cards`,
      reconciled.length === 2,
      JSON.stringify(reconciled.map((item) => item.id)));
    const oldView = resolveTaskCardView(reconciled[0]?.args, reconciled[0]?.details, reconciled[0]?.status);
    const newView = resolveTaskCardView(reconciled[1]?.args, reconciled[1]?.details, reconciled[1]?.status);
    check(`${branch} bootstrap omission-settles the historical child (job_list=[])`,
      !!oldView && oldView.children[0].jobId === 'job-old' && oldView.children[0].status === 'cancelled',
      JSON.stringify(oldView));
    check(`${branch} bootstrap keeps the in-window child authoritative`,
      !!newView && newView.children[0].jobId === 'job-new' && newView.children[0].status === 'running',
      JSON.stringify(newView));
  }
}

// ---- Lifecycle switch: history merge must not resurrect a settled child ----
// Simulates the exact switch_session defect: a live projection already
// omission-settled job-old to cancelled (authoritative job_list=[] from an
// earlier generation), then a switch merges disk history whose recorder
// still carries queued/running. The merge must keep the terminal projection;
// a follow-up authoritative job_list=[] then leaves it byte-for-byte.
{
  const historyCard = {
    kind: 'toolCard',
    id: 'tc-hist',
    toolCallId: 'task-hist-call',
    toolName: 'task',
    args: { agent: 'reviewer', name: 'Hist', task: 'historical work' },
    status: 'running',
    result: '',
    details: [{ index: 0, jobId: 'job-old', agentId: 'Alpha', agent: 'reviewer', status: 'running' }],
  };
  const settledCard = {
    ...historyCard,
    details: [{ index: 0, jobId: 'job-old', agentId: 'Alpha', agent: 'reviewer', status: 'cancelled' }],
  };
  for (const preserveInflight of [true, false]) {
    const branch = preserveInflight ? 'streaming' : 'settled';
    const resurrected = mergeAuthoritativeItems([historyCard], [settledCard], preserveInflight);
    const view = resolveTaskCardView(resurrected[0].args, resurrected[0].details, resurrected[0].status);
    check(`${branch} lifecycle merge keeps an already-settled child terminal`,
      !!view && view.children[0].jobId === 'job-old' && view.children[0].status === 'cancelled',
      JSON.stringify(view));
    const reconciled = applyJobSnapshot(resurrected, [], taskJobIds(resurrected));
    const reconciledView = resolveTaskCardView(reconciled[0].args, reconciled[0].details, reconciled[0].status);
    check(`${branch} authoritative empty job_list keeps the settled child terminal`,
      !!reconciledView && reconciledView.children[0].status === 'cancelled',
      JSON.stringify(reconciledView));
  }
  // A genuinely running child (no terminal projection) still merges as
  // running; the guard only blocks regression.
  const stillRunning = mergeAuthoritativeItems([historyCard], [historyCard], false);
  const runningView = resolveTaskCardView(stillRunning[0].args, stillRunning[0].details, stillRunning[0].status);
  check('merge keeps a running child running when no terminal projection exists',
    !!runningView && runningView.children[0].status === 'running', JSON.stringify(runningView));
}

// ---- session handoff divider: customType 'handoff' + data.text ----
{
  const doc = '<handoff-context>\nSession: alpha\nGoal: ship it\n</handoff-context>';
  const divider = customToItem({ role: 'custom', customType: 'handoff', data: { text: doc } });
  check('handoff divider renders from data.text', divider !== null && divider.kind === 'handoff-divider' && divider.text === doc, JSON.stringify(divider));
  // The backend record may omit display — the divider renders regardless
  // (the backend mints it deliberately as the new session's first record).
  const withoutDisplay = customToItem({ role: 'custom', customType: 'handoff', data: { text: doc }, display: false });
  check('handoff divider ignores the display flag', withoutDisplay !== null && withoutDisplay.kind === 'handoff-divider' && withoutDisplay.text === doc);
  const items = messagesToItems([{ role: 'custom', customType: 'handoff', data: { text: doc }, timestamp: 9 }]);
  const restored = items.find((i) => i.kind === 'handoff-divider');
  check('messagesToItems restores the handoff divider from data.text', restored !== undefined && restored.kind === 'handoff-divider' && restored.text === doc, JSON.stringify(items));
  const fallback = customToItem({ role: 'custom', customType: 'handoff', content: [{ type: 'text', text: 'plain fallback doc' }] });
  check('handoff divider falls back to content when data.text is absent', fallback !== null && fallback.kind === 'handoff-divider' && fallback.text === 'plain fallback doc', JSON.stringify(fallback));
  // Non-handoff customs keep their existing shapes.
  const plain = customToItem({ role: 'custom', customType: 'notice', content: 'hello', display: true });
  check('non-handoff customs stay plain labeled customs', plain !== null && plain.kind === 'custom' && plain.label === 'notice');
  const loop = customToItem({ role: 'custom', customType: 'loop_scheduled_turn', content: '<system-reminder>x</system-reminder>', display: false });
  check('display:false non-handoff customs stay hidden', loop === null);
}

console.log(`\ntranscript.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  for (const f of failures) console.log(`  FAIL ${f}`);
  process.exit(1);
}
process.exit(0);