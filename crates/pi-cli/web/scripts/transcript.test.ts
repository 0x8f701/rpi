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
import {
  messagesToItems,
  boundOutput,
  customToItem,
  orchestrationIrcView,
  applyToolSnapshot,
  applyToolResultToItems,
  toolMedia,
  userImages,
  userMessageProjection,
  parseImageAnalysis,
  mergeAuthoritativeItems,
  applyJobUpdated,
  applyMessageDelivered,
  resolveTaskCardView,
  parseTaskCardArgs,
  resolveTodoCardView,
  parseTodoPhases,
  parseEditCard,
  compactToolArgs,
  parseCommandCardArgs,
  parseProcessCardArgs,
  parseWriteCardArgs,
  parseReadCardArgs,
  parseHubCard,
  parseHeading,
  diffLines,
  shouldRestoreStreamingAssistant,
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
} from '../src/transcript.ts';
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

// ---- limits match the TUI compact tool-card fold ----
check('BASH_OUTPUT_LINE_LIMIT == 10 (TUI BASH_CARD_OUTPUT_LIMIT)', BASH_OUTPUT_LINE_LIMIT === 10);
check('TOOL_OUTPUT_LINE_LIMIT == 6 (TUI DEFAULT_CARD_OUTPUT_LIMIT)', TOOL_OUTPUT_LINE_LIMIT === 6);

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
  check('generic card completes with six-line bounded result', card?.kind === 'toolCard' && card.status === 'done' && card.result.startsWith('\u2026 2 more lines\ngeneric-tail-3') && card.result.endsWith('\ngeneric-tail-8'), JSON.stringify(card));
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
    kind: 'toolCard', id: 'tc', toolCallId: 'call-x', toolName: 'read',
    args: { path: 'a' }, status: 'done', result: 'from end', details: { diff: 'x' },
  };
  const matchedNoDetails = applyToolResultToItems([doneCard], 'call-x', 'from message', undefined);
  check('matched card with no details stays untouched (no toolResult row)', matchedNoDetails.length === 1 && matchedNoDetails[0].kind === 'toolCard' && matchedNoDetails[0].result === 'from end', JSON.stringify(matchedNoDetails));
  const matchedDetails = applyToolResultToItems([doneCard], 'call-x', 'from message', { diff: 'new' });
  check('fold never overwrites existing card details/result', matchedDetails[0].kind === 'toolCard' && matchedDetails[0].details.diff === 'x' && matchedDetails[0].result === 'from end' && matchedDetails.length === 1, JSON.stringify(matchedDetails));

  const bareCard = {
    kind: 'toolCard', id: 'tc2', toolCallId: 'call-y', toolName: 'read',
    args: { path: 'b' }, status: 'running', result: '',
  };
  const folded = applyToolResultToItems([bareCard], 'call-y', 'the result', { diff: 'd' });
  check('matched running card folds details + fills empty result', folded[0].kind === 'toolCard' && folded[0].details && folded[0].details.diff === 'd' && folded[0].result === 'the result' && folded.length === 1, JSON.stringify(folded));

  const unmatched = applyToolResultToItems([doneCard], 'other-call', 'orphan output', undefined);
  check('unmatched toolResult stays readable as a toolResult row', unmatched.length === 2 && unmatched[1].kind === 'toolResult' && unmatched[1].text === 'orphan output', JSON.stringify(unmatched));
  const emptyId = applyToolResultToItems([doneCard], '', 'no id', undefined);
  check('empty toolCallId appends a readable row (never drops output)', emptyId.length === 2 && emptyId[1].kind === 'toolResult' && emptyId[1].text === 'no id', JSON.stringify(emptyId));
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
  check('live tool end retains media on card', completed.kind === 'toolCard' && completed.media?.length === 1, JSON.stringify(completed));

  const restored = messagesToItems([
    { role: 'assistant', content: [{ type: 'toolCall', id: 'media-restored', name: 'read', arguments: { path: 'image.png' } }] },
    { role: 'toolResult', toolCallId: 'media-restored', content: [{ type: 'text', text: 'Read image' }, { type: 'image', mimeType: 'image/png', data: png }], details: {}, isError: false },
  ]);
  const restoredCard = restored.find((item) => item.kind === 'toolCard');
  check('restored tool result retains image media', restoredCard?.kind === 'toolCard' && restoredCard.media?.length === 1, JSON.stringify(restoredCard));
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
    { index: 0, jobId: 'job-a', agentId: 'Alpha', agent: 'reviewer', status: 'queued' },
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
          { id: 't-1', content: 'compile', status: 'in_progress', dependsOn: [], ready: true, blockedBy: [] },
          {
            id: 't-2',
            content: 'test',
            status: 'pending',
            dependsOn: ['t-1'],
            ready: false,
            blockedBy: [{ taskId: 't-1', content: 'compile', status: 'in_progress' }],
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
    '  - test [pending] (Build) id=t-2 blocked by compile (t-1)',
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
  check('todo card surfaces blocking by content, never ids', view.phases[0]?.tasks[1]?.blockedBy.length === 1 && view.phases[0].tasks[1].blockedBy[0] === 'compile', JSON.stringify(view.phases[0]?.tasks[1]));
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
            { id: 'd1', content: 'sketch', status: 'completed', blockedBy: [] },
            { id: 'd2', content: 'review', status: 'abandoned', blockedBy: [] },
          ],
        },
        {
          name: 'Verify',
          tasks: [{ id: 'v1', content: 'test suite', status: 'in_progress', blockedBy: [] }],
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
      phases: [{ name: 'Build', tasks: [{ id: 't1', content: 'compile', status: 'pending', blockedBy: [] }] }],
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
              { id: 't1', content: 'compile', status: 'in_progress', blockedBy: [] },
              { id: 't2', content: 'test', status: 'pending', blockedBy: [] },
              { id: 't3', content: 'lint', status: 'pending', blockedBy: [] },
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
    { phases: [{ name: 'P', tasks: [{ id: 'x', content: 'x'.repeat(300), status: 'pending', blockedBy: [] }] }] },
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
      { id: 'ok', content: 'fine', status: 'pending', blockedBy: [] },
      { id: 'empty', content: '   ', status: 'pending', blockedBy: [] },
      'garbage',
      null,
    ] },
    'not-a-phase',
  ]);
  check('parseTodoPhases skips malformed entries', skipped !== null && skipped.length === 1 && skipped[0].tasks.length === 1 && skipped[0].tasks[0].content === 'fine', JSON.stringify(skipped));
  const unknownStatus = parseTodoPhases([{ name: 'P', tasks: [{ id: 'u', content: 'weird', status: 'bloop', blockedBy: [] }] }]);
  check('parseTodoPhases unknown status normalizes to pending', unknownStatus?.[0]?.tasks[0]?.status === 'pending', JSON.stringify(unknownStatus));
  const blockedByGarbage = parseTodoPhases([{ name: 'P', tasks: [{ id: 'b', content: 'task', status: 'pending', blockedBy: [{ taskId: 'x', content: 'blocker' }, 'junk', null] }] }]);
  check('parseTodoPhases ignores malformed blockedBy entries', blockedByGarbage?.[0]?.tasks[0]?.blockedBy.length === 1 && blockedByGarbage[0].tasks[0].blockedBy[0] === 'blocker', JSON.stringify(blockedByGarbage));
}

// ---- robust heading parse + generic bounds unchanged ----
{
  check('parseHeading accepts ATX with trailing hashes', parseHeading('## Goal ##')?.title === 'Goal');
  check('parseHeading rejects missing space after hashes', parseHeading('##Goal') === null);
  const generic = applyToolSnapshot(
    [{ kind: 'toolCard', id: 'g', toolCallId: 'g1', toolName: 'read', args: { path: 'a' }, status: 'running', result: '' }],
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

// ---- parseProcessCardArgs: argv/label projection (with and without op) ----
{
  const start = parseProcessCardArgs({ op: 'start', argv: ['echo', 'hello', 'world'] });
  check('process start joins argv', start !== null && start.label === 'echo hello world', JSON.stringify(start));
  const noOp = parseProcessCardArgs({ argv: ['pi-web-screenshot-http'], label: 'pi-web-screenshot-http' });
  check('process without op joins argv', noOp !== null && noOp.label === 'pi-web-screenshot-http', JSON.stringify(noOp));
  const labelOnly = parseProcessCardArgs({ label: 'pi-web-screenshot-http' });
  check('process label-only fallback', labelOnly !== null && labelOnly.label === 'pi-web-screenshot-http', JSON.stringify(labelOnly));
  const stop = parseProcessCardArgs({ op: 'stop', id: 'abc123' });
  check('process stop surfaces op+id', stop !== null && stop.label === 'process stop abc123', JSON.stringify(stop));
  const bare = parseProcessCardArgs({ foo: 'bar' });
  check('process null for unrecognised args', bare === null, JSON.stringify(bare));
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

// ---- parseReadCardArgs: path extraction (result renders as output body) ----
{
  const read = parseReadCardArgs({ path: 'README.md' });
  check('parseReadCardArgs extracts path', read !== null && read.path === 'README.md', JSON.stringify(read));
  const fileKey = parseReadCardArgs({ file: 'config.toml' });
  check('parseReadCardArgs uses file key alias', fileKey !== null && fileKey.path === 'config.toml', JSON.stringify(fileKey));
  const noPath = parseReadCardArgs({ offset: 0 });
  check('parseReadCardArgs null when no path', noPath === null, JSON.stringify(noPath));
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
  check('bashExecution cancelled → error status', bash?.kind === 'bash' && bash.status === 'error', JSON.stringify(bash));
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
check('parseReadCardArgs returns path not path JSON', (() => { const r = parseReadCardArgs({ path: 'f' }); return r !== null && r.path === 'f'; })());
check('resolveTodoCardView returns phases not raw args JSON', (() => {
  const v = resolveTodoCardView(
    { op: 'init', list: [{ phase: 'P', items: ['a'] }] },
    { phases: [{ name: 'P', tasks: [{ id: 't1', content: 'a', status: 'pending', blockedBy: [] }] }], storage: 'session', completedTasks: [] },
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

console.log(`\ntranscript.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  for (const f of failures) console.log(`  FAIL ${f}`);
  process.exit(1);
}
process.exit(0);