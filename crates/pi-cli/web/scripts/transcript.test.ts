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
  shouldRestoreStreamingAssistant,
  BASH_OUTPUT_LINE_LIMIT,
  TOOL_OUTPUT_LINE_LIMIT,
} from '../src/transcript.ts';
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

// ---- restored: orchestration IRC renders the parsed view, never raw XML ----
{
  const items = messagesToItems([
    {
      role: 'custom',
      customType: 'orchestration_message',
      content: '<orchestration-message id="m1" from="Main">\nhello child\n</orchestration-message>',
      display: true,
      details: { id: 'm1', from: 'Main', to: 'Child', body: 'hello child', replyTo: null },
      timestamp: 7,
    },
  ]);
  const custom = items.find((i) => i.kind === 'custom');
  check('IRC renders the IRC label', custom && custom.label === 'IRC \u00b7 Main \u2192 Child', JSON.stringify(custom));
  check('IRC renders the clean body', custom && custom.text === 'hello child', JSON.stringify(custom));
  check('IRC never renders the raw XML wrapper', custom && !custom.text.includes('<orchestration-message'), JSON.stringify(custom));
}

// ---- restored: IRC without details.body falls back to stripping the wrapper ----
{
  const view = orchestrationIrcView({
    customType: 'orchestration_message',
    content: '<orchestration-message id="m2" from="A">\nbody text\nReplying to message: p1\n</orchestration-message>',
    details: { id: 'm2', from: 'A', to: 'B' },
  });
  check('IRC fallback strips the wrapper and reply line', view && view.label === 'IRC \u00b7 A \u2192 B' && view.text === 'body text', JSON.stringify(view));
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
    details: { id: 'm1', from: 'Main', to: 'Child', body: 'hi' },
  });
  check('customToItem renders IRC view, not XML', irc && irc.kind === 'custom' && irc.label === 'IRC \u00b7 Main \u2192 Child' && irc.text === 'hi' && !irc.text.includes('<orchestration-message'), JSON.stringify(irc));
}

// ---- live/toolResult bounding parity (same helper) ----
{
  const twelve = Array.from({ length: 12 }, (_, i) => String(i + 1)).join('\n');
  const bounded = boundOutput(twelve, TOOL_OUTPUT_LINE_LIMIT).text;
  check('live toolResult bound identically (last 6 + hint)', bounded.endsWith('\n12') && bounded.startsWith('\u2026 6 more lines\n'), bounded);
  const bash30 = boundOutput(Array.from({ length: 30 }, (_, i) => String(i + 1)).join('\n'), BASH_OUTPUT_LINE_LIMIT).text;
  check('live bash bound identically (last 10 + hint)', bash30.endsWith('\n30') && bash30.startsWith('\u2026 20 more lines\n'), bash30);
}

console.log(`\ntranscript.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  for (const f of failures) console.log(`  FAIL ${f}`);
  process.exit(1);
}
process.exit(0);