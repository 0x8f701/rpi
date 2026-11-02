#!/usr/bin/env node
// File conversation dock: presentational only. The owner passes one snapshot,
// the draft, and send/stop. Enter sends, Shift+Enter does not, Esc stops only
// while this snapshot is streaming, and a sibling snapshot's stream never
// becomes this dock's Stop. Entry bodies reuse ConversationEntryView, so
// markdown, thinking, and tool cards match SideChatPanel. Run through
// `npm run build` (esbuild + node, mermaid aliased).
import { createElement, useState, type ReactElement } from 'react';
import { act, create, type ReactTestRenderer } from 'react-test-renderer';
import { renderToStaticMarkup } from 'react-dom/server.browser';
import {
  FileConversationDock,
  type FileConversationDockProps,
  type FileConversationSnapshot,
} from '../src/panels/FileConversationDock.tsx';
import { resetConversationStreamStateForTest } from '../src/panels/ConversationEntries.tsx';
import { SideChatPanel, type SideChatEntryWire } from '../src/panels/SideChatPanel.tsx';
const failures: string[] = [];
let ran = 0;
function check(name: string, cond: unknown, detail = ''): void {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

function wireEntry(entry: Partial<SideChatEntryWire> & Pick<SideChatEntryWire, 'role' | 'text'>): SideChatEntryWire {
  return {
    id: entry.id ?? `e-${entry.role}`,
    kind: entry.kind ?? entry.role,
    role: entry.role,
    text: entry.text,
    toolName: entry.toolName,
    toolArgs: entry.toolArgs,
    output: entry.output,
    thinking: entry.thinking,
    isError: entry.isError ?? false,
    isPartial: entry.isPartial ?? false,
  };
}

function snapshot(overrides: Partial<FileConversationSnapshot> = {}): FileConversationSnapshot {
  return {
    conversationId: 'conv-a',
    path: 'src/app.ts',
    streaming: false,
    status: 'idle',
    entries: [],
    ...overrides,
  };
}

function dockProps(overrides: Partial<FileConversationDockProps> = {}): FileConversationDockProps {
  return {
    snapshot: snapshot(),
    draft: '',
    onDraft: () => {},
    onSend: () => {},
    onStop: () => {},
    onError: () => {},
    sessionId: 'sess-1',
    cwd: '/work/repo',
    ...overrides,
  };
}

function buttonByLabel(root: ReactTestRenderer, label: string) {
  return root.root.findAll((node) => node.type === 'button' && node.props['aria-label'] === label)[0];
}

function textareaOf(root: ReactTestRenderer) {
  return root.root.findByType('textarea');
}

function keyEvent(key: string, extras: Record<string, unknown> = {}) {
  let prevented = false;
  return {
    key,
    shiftKey: false,
    ctrlKey: false,
    metaKey: false,
    altKey: false,
    nativeEvent: { isComposing: false, keyCode: key === 'Enter' ? 13 : 0, which: key === 'Enter' ? 13 : 0 },
    preventDefault() { prevented = true; },
    get prevented() { return prevented; },
    ...extras,
  };
}

{
  const html = renderToStaticMarkup(createElement(FileConversationDock, dockProps({
    snapshot: snapshot({
      entries: [
        wireEntry({ role: 'user', text: '**bold** and `code`' }),
        wireEntry({ role: 'assistant', text: 'see `app.ts`' }),
        wireEntry({ id: 'think', role: 'assistant', kind: 'thinking', text: '', thinking: 'weigh **this**' }),
        wireEntry({ id: 'tool', role: 'tool', kind: 'tool', text: '', toolName: 'bash', output: 'ok' }),
      ],
      context: { includedRange: { startLine: 1, endLine: 40 }, truncated: true },
    }),
    selection: { startLine: 12, endLine: 18 },
  })));
  check('markdown body is shared, not a raw text node', html.includes('<strong>bold</strong>') && html.includes('<code class="md-code">code</code>'), html.slice(0, 400));
  check('thinking fold is the shared details block', html.includes('class="thinking"') && html.includes('<strong>this</strong>'));
  check('tool entry is a tool card', html.includes('tool-card') && html.includes('bash'));
  check('owner session and cwd stamp path chips', html.includes('data-session-id="sess-1"') && html.includes('data-workspace-cwd="/work/repo"'));
  check('selection disclosure wins over truncated page context', html.includes('Selection lines 12–18') && !html.includes('Context truncated'));
  check('send failures are not an inline dock error', !html.includes('file-conversation__error') && !html.includes('file_conversation_superseded'));
  check('conversation id is the owner snapshot, not internal state', html.includes('data-conversation-id="conv-a"'));
  check('no attachment or microphone controls', !html.includes('Attach files') && !html.includes('Microphone') && !html.includes('side-chat__composer-action'));
  check('idle composer is Send, not Stop', html.includes('file-conversation__action--send') && html.includes('➤') && !html.includes('file-conversation__action--stop'));
}

{
  const streaming = renderToStaticMarkup(createElement(FileConversationDock, dockProps({
    snapshot: snapshot({ streaming: true, status: 'streaming' }),
    draft: 'kept',
  })));
  check('streaming composer replaces Send with ■ Stop', streaming.includes('file-conversation__action--stop') && streaming.includes('■') && !streaming.includes('file-conversation__action--send'));
  check('truncated context discloses the included range', renderToStaticMarkup(createElement(FileConversationDock, dockProps({
    snapshot: snapshot({ context: { includedRange: { startLine: 1, endLine: 400 }, truncated: true } }),
  }))).includes('Context truncated to lines 1–400'));
  check('rejected selection discloses that it was not included', renderToStaticMarkup(createElement(FileConversationDock, dockProps({
    snapshot: snapshot({
      context: { requestedRange: { startLine: 1, endLine: 9000 }, includedRange: null, truncated: false },
    }),
  }))).includes('Selection lines 1–9000 was not included'));
}

void (async () => {
  const sent: number[] = [];
  const stopped: string[] = [];
  const drafts: string[] = [];
  const errors: string[] = [];
  let renderer: ReactTestRenderer;
  function Host({ streaming }: { streaming: boolean }): ReactElement {
    const [draft, setDraft] = useState('what does main do?');
    return createElement(FileConversationDock, dockProps({
      snapshot: snapshot({ conversationId: 'conv-a', streaming }),
      draft,
      onDraft: (next) => {
        drafts.push(next);
        setDraft(next);
      },
      onSend: () => { sent.push(1); },
      onStop: () => { stopped.push('conv-a'); },
      onError: (message) => { errors.push(message); },
    }));
  }
  await act(async () => {
    renderer = create(createElement(Host, { streaming: false }));
  });
  const shift = keyEvent('Enter', { shiftKey: true });
  await act(async () => { textareaOf(renderer!).props.onKeyDown(shift); });
  check('Shift+Enter does not send', sent.length === 0 && shift.prevented === false);

  const enter = keyEvent('Enter');
  await act(async () => { textareaOf(renderer!).props.onKeyDown(enter); });
  check('Enter sends once', sent.length === 1 && enter.prevented === true);

  await act(async () => { textareaOf(renderer!).props.onChange({ target: { value: 'still here' } }); });
  check('typing reports the draft to the owner', drafts.at(-1) === 'still here');

  const escIdle = keyEvent('Escape');
  await act(async () => { textareaOf(renderer!).props.onKeyDown(escIdle); });
  check('Esc while idle does not stop', stopped.length === 0 && escIdle.prevented === false);

  await act(async () => { renderer!.update(createElement(Host, { streaming: true })); });
  const enterBusy = keyEvent('Enter');
  await act(async () => { textareaOf(renderer!).props.onKeyDown(enterBusy); });
  check('Enter while streaming does not send or stop', sent.length === 1 && stopped.length === 0);
  const escBusy = keyEvent('Escape');
  await act(async () => { textareaOf(renderer!).props.onKeyDown(escBusy); });
  check('Esc stops only the streaming snapshot', stopped.length === 1 && stopped[0] === 'conv-a' && escBusy.prevented === true);
  await act(async () => { buttonByLabel(renderer!, 'Stop generating').props.onClick(); });
  check('Stop button addresses the same snapshot handler', stopped.length === 2);

  let otherStops = 0;
  await act(async () => {
    renderer!.update(createElement(FileConversationDock, dockProps({
      snapshot: snapshot({ conversationId: 'conv-b', streaming: false, path: 'src/other.ts' }),
      draft: 'other',
      onStop: () => { otherStops += 1; },
      onSend: () => Promise.reject(new Error('file send failed')),
      onError: (message) => { errors.push(message); },
    })));
  });
  check('switched snapshot shows its own id and path', renderer!.root.findByType('section').props['data-conversation-id'] === 'conv-b' && renderer!.toJSON() != null);
  const escOther = keyEvent('Escape');
  await act(async () => { textareaOf(renderer!).props.onKeyDown(escOther); });
  check('Esc on an idle sibling does not stop the previous stream', otherStops === 0 && stopped.length === 2 && escOther.prevented === false);
  const rejected = keyEvent('Enter');
  await act(async () => { textareaOf(renderer!).props.onKeyDown(rejected); });
  check('rejected send reaches onError', errors.length === 1 && errors[0] === 'file send failed', errors.join('|'));
  await act(async () => { renderer!.unmount(); });
  resetConversationStreamStateForTest();
{
  const side = renderToStaticMarkup(createElement(SideChatPanel, {
    snapshot: { active: 't1', maxTabs: 8, tabs: [{ name: 't1', streaming: false, status: 'idle', entries: [wireEntry({ role: 'assistant', text: '**bold**' })] }] },
    onNew: () => {},
    onSwitch: () => {},
    onClose: () => {},
    onPrompt: () => {},
    onAbort: () => {},
    onClosePanel: () => {},
  }));
  check('/btw still renders the shared markdown body', side.includes('<strong>bold</strong>') && side.includes('side-chat__entry--assistant'));
  check('/btw keeps attach, mic, and its own Send', side.includes('Attach files') && side.includes('Microphone') && side.includes('side-chat__composer-action--send'));
  check('/btw is not a file conversation', !side.includes('file-conversation') && !side.includes('data-conversation-id'));
  check('blank draft cannot send', renderToStaticMarkup(createElement(FileConversationDock, dockProps({ draft: '   ' }))).includes('disabled'));
}

console.log(`\nfileConversationDock.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);
})().catch((err: unknown) => {
  console.error(err);
  process.exit(1);
});
