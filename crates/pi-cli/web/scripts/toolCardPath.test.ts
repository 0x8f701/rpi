#!/usr/bin/env node
// Focused authorized tool-result path-open regression: structured read/write/edit
// args become a File Explorer click through data-md-path + markdownOpenValidatedPath;
// result text, skill URIs, traversal, and out-of-workspace absolutes stay inert.
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server.browser';
import { ToolCard } from '../src/App.tsx';
import {
  dispatchMarkdownPathClick,
  markdownOpenValidatedPath,
  markdownWorkspaceRelativePath,
} from '../src/markdown.ts';
import { authorizedToolResultOpenPath } from '../src/transcript.ts';

const failures: string[] = [];
let ran = 0;
function check(name: string, cond: boolean, detail?: string): void {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

function toolCard(item: Parameters<typeof ToolCard>[0]['item']): string {
  return renderToStaticMarkup(createElement('div', {
    'data-session-id': 'session-a',
    'data-workspace-cwd': '/workspace/project',
  }, createElement(ToolCard, { item })));
}

function attr(html: string, name: string): string | null {
  const match = html.match(new RegExp(`${name}="([^"]*)"`));
  return match ? match[1] : null;
}

{
  const relative = authorizedToolResultOpenPath('read', { path: 'src/lib.rs' });
  check('read relative args authorize exact path', relative?.path === 'src/lib.rs' && relative.line === undefined, JSON.stringify(relative));
  const offset = authorizedToolResultOpenPath('read', { path: 'src/lib.rs', offset: 42 });
  check('read offset becomes 1-indexed line', offset?.path === 'src/lib.rs' && offset.line === 42, JSON.stringify(offset));
  const write = authorizedToolResultOpenPath('write', { file: 'notes.txt', content: 'hello' });
  check('write file alias authorizes path', write?.path === 'notes.txt' && write.line === undefined, JSON.stringify(write));
  const edit = authorizedToolResultOpenPath('edit', { path: 'src/app.ts', operation: 'replace' });
  check('edit args authorize path', edit?.path === 'src/app.ts', JSON.stringify(edit));
  check('bash command is never an open path', authorizedToolResultOpenPath('bash', { command: 'cat src/lib.rs' }) === null);
  check('grep pattern is never an open path', authorizedToolResultOpenPath('grep', { path: 'src/lib.rs', pattern: 'fn' }) === null);
  check('skill URI is never an open path', authorizedToolResultOpenPath('read', { path: 'skill://research' }) === null);
  check('agent URI is never an open path', authorizedToolResultOpenPath('read', { path: 'agent://DeepSeek' }) === null);
  check('history URI is never an open path', authorizedToolResultOpenPath('read', { path: 'history://aa0e8c2e-4b1a-4d3f-9c7e-1f2a3b4c5d6e' }) === null);
  check('artifact URI is never an open path', authorizedToolResultOpenPath('read', { path: 'artifact://aa0e8c2e-4b1a-4d3f-9c7e-1f2a3b4c5d6e' }) === null);
  check('javascript scheme is never an open path', authorizedToolResultOpenPath('read', { path: 'javascript:alert(1)' }) === null);
  check('file scheme is never an open path', authorizedToolResultOpenPath('read', { path: 'file:///etc/passwd' }) === null);
  check('traversal is never an open path', authorizedToolResultOpenPath('read', { path: 'src/../secret.ts' }) === null);
  check('whitespace path is never an open path', authorizedToolResultOpenPath('read', { path: 'src/my file.ts' }) === null);
  check('empty path is never an open path', authorizedToolResultOpenPath('write', { content: 'x' }) === null);
  check('unknown edit path is never an open path', authorizedToolResultOpenPath('edit', { operation: 'replace' }) === null);
}

{
  const html = toolCard({
    kind: 'toolCard',
    id: 'read-1',
    toolCallId: 'read-call',
    toolName: 'read',
    args: { path: 'src/lib.rs', offset: 12 },
    status: 'done',
    result: 'fn main() {}\nsee /etc/passwd and src/secret.ts:1',
  });
  check('authorized read renders path button', html.includes('class="tool-card__summary-path tool-card__path"') && html.includes('data-md-path="src/lib.rs"'), html);
  check('authorized read stamps structured offset line', attr(html, 'data-md-line') === '12', html);
  check('authorized read visible headline stays the structured path', html.includes('src/lib.rs'), html);
  check('read result body stays non-interactive pre', html.includes('class="tool-card__output"') && html.includes('/etc/passwd') && !html.includes('data-md-path="/etc/passwd"'), html);
  check('read result text is not a second path button', (html.match(/data-md-path="/g) ?? []).length === 1, html);
}

{
  const html = toolCard({
    kind: 'toolCard',
    id: 'write-1',
    toolCallId: 'write-call',
    toolName: 'write',
    args: { path: 'notes.txt', content: 'hello /etc/passwd' },
    status: 'done',
    result: 'wrote /etc/passwd',
  });
  check('authorized write renders exact args path', attr(html, 'data-md-path') === 'notes.txt', html);
  check('write result never becomes a path button', !html.includes('data-md-path="/etc/passwd"') && html.includes('wrote /etc/passwd'), html);
}

{
  const html = toolCard({
    kind: 'toolCard',
    id: 'edit-1',
    toolCallId: 'edit-call',
    toolName: 'edit',
    args: { path: 'src/app.ts', operation: 'replace' },
    status: 'done',
    result: '',
    details: { diff: '--- a\n+++ b\n@@\n-src/secret.ts\n+/etc/passwd\n' },
  });
  check('authorized edit renders exact args path', attr(html, 'data-md-path') === 'src/app.ts', html);
  check('edit diff text is not a path button', (html.match(/data-md-path="/g) ?? []).length === 1 && html.includes('/etc/passwd'), html);
}

{
  const skillHtml = toolCard({
    kind: 'toolCard',
    id: 'skill-1',
    toolCallId: 'skill-call',
    toolName: 'read',
    args: { path: 'skill://research' },
    status: 'done',
    result: '---\nname: research\ndescription: "docs"\n---\n# Research\n',
  });
  check('skill read has no path button', !skillHtml.includes('data-md-path') && !skillHtml.includes('tool-card__path'), skillHtml);

  const bashHtml = toolCard({
    kind: 'toolCard',
    id: 'bash-1',
    toolCallId: 'bash-call',
    toolName: 'bash',
    args: { command: 'cat src/lib.rs' },
    status: 'done',
    result: 'src/lib.rs:1',
  });
  check('bash card has no path button', !bashHtml.includes('data-md-path') && !bashHtml.includes('tool-card__path'), bashHtml);

  const hostileHtml = toolCard({
    kind: 'toolCard',
    id: 'hostile-1',
    toolCallId: 'hostile-call',
    toolName: 'read',
    args: { path: 'javascript:alert(1)' },
    status: 'done',
    result: 'nope',
  });
  check('hostile scheme args stay non-clickable', !hostileHtml.includes('data-md-path') && !hostileHtml.includes('tool-card__path'), hostileHtml);
}
function fakeNode(opts: {
  kind: 'path' | 'output' | 'code';
  path?: string | null;
  line?: string | null;
  text?: string | null;
  sessionId?: string;
  workspaceCwd?: string;
}) {
  const owner = {
    textContent: null as string | null,
    getAttribute(name: string): string | null {
      if (name === 'data-session-id') return opts.sessionId ?? null;
      if (name === 'data-workspace-cwd') return opts.workspaceCwd ?? null;
      return null;
    },
    closest(): unknown { return null; },
  };
  const node = {
    textContent: opts.text ?? null,
    getAttribute(name: string): string | null {
      if (name === 'data-md-path') return opts.path ?? null;
      if (name === 'data-md-line') return opts.line ?? null;
      if (name === 'href') return null;
      return null;
    },
    closest(sel: string): unknown {
      if (sel === '[data-session-id]') return opts.sessionId ? owner : null;
      if (sel === '[data-md-path]') return opts.kind === 'path' ? node : null;
      if (sel === 'code.md-code' || sel === '.md-code') return opts.kind === 'code' ? node : null;
      if (sel === '.md-fence') return null;
      if (sel === 'a') return null;
      if (sel === '.md-link-download') return null;
      return null;
    },
  };
  return node;
}

function click(target: unknown) {
  const event = {
    target,
    defaultPrevented: false,
    preventDefault() { event.defaultPrevented = true; },
  };
  return event;
}

{
  const opened: Array<{ path: string; line?: number; sessionId: string }> = [];
  const onOpen = (path: string, line: number | undefined, owner: { sessionId: string }) => {
    opened.push({ path, line, sessionId: owner.sessionId });
  };

  opened.length = 0;
  const authorized = click(fakeNode({
    kind: 'path',
    path: 'src/lib.rs',
    line: '12',
    sessionId: 'session-a',
    workspaceCwd: '/workspace/project',
  }));
  check('structured tool path click is handled', dispatchMarkdownPathClick(authorized, onOpen) === true);
  check(
    'structured tool path opens exact args path and line',
    opened.length === 1 && opened[0].path === 'src/lib.rs' && opened[0].line === 12 && opened[0].sessionId === 'session-a',
    JSON.stringify(opened),
  );

  opened.length = 0;
  const output = click(fakeNode({
    kind: 'output',
    text: '/etc/passwd',
    sessionId: 'session-a',
    workspaceCwd: '/workspace/project',
  }));
  check(
    'plain result text is inert even under a session owner',
    dispatchMarkdownPathClick(output, onOpen) === false && opened.length === 0 && output.defaultPrevented === false,
  );

  opened.length = 0;
  const hostileText = click(fakeNode({
    kind: 'code',
    text: '/etc/passwd',
    sessionId: 'session-a',
    workspaceCwd: '/workspace/project',
  }));
  check(
    'absolute result-looking code stays inert',
    dispatchMarkdownPathClick(hostileText, onOpen) === false && opened.length === 0,
  );

  opened.length = 0;
  const outside = click(fakeNode({
    kind: 'path',
    path: '/elsewhere/secret.ts',
    sessionId: 'session-a',
    workspaceCwd: '/workspace/project',
  }));
  check(
    'out-of-workspace absolute tool path stays inert',
    dispatchMarkdownPathClick(outside, onOpen) === false && opened.length === 0 && outside.defaultPrevented === false,
    JSON.stringify(opened),
  );

  opened.length = 0;
  const traversal = click(fakeNode({
    kind: 'path',
    path: 'src/../secret.ts',
    sessionId: 'session-a',
    workspaceCwd: '/workspace/project',
  }));
  check(
    'traversal tool path stays inert',
    dispatchMarkdownPathClick(traversal, onOpen) === false && opened.length === 0,
  );

  opened.length = 0;
  const noOwner = click(fakeNode({ kind: 'path', path: 'src/lib.rs' }));
  check(
    'structured path without owner stays inert',
    dispatchMarkdownPathClick(noOwner, onOpen) === false && opened.length === 0,
  );
}

{
  const probes: Array<Record<string, unknown>> = [];
  const opens: Array<{ path: string; line?: number }> = [];
  const errors: unknown[] = [];
  const onError = (error: unknown) => { errors.push(error); };
  const validRead = (path: string) => ({
    path,
    lines: ['first line'],
    totalLines: 1,
    sizeBytes: 11,
    truncated: false,
  });

  await markdownOpenValidatedPath(
    'src/lib.rs',
    12,
    { sessionId: 'session-a', workspaceCwd: '/workspace/project' },
    (command) => {
      probes.push(command);
      return Promise.resolve(validRead('src/lib.rs'));
    },
    (path, line) => { opens.push({ path, line }); },
    onError,
  );
  check(
    'authorized tool path probes exact relative file then opens it',
    probes.length === 1
      && probes[0].type === 'file_explorer_read'
      && probes[0].path === 'src/lib.rs'
      && probes[0].sessionId === 'session-a'
      && probes[0].maxLines === 1
      && opens.length === 1
      && opens[0].path === 'src/lib.rs'
      && opens[0].line === 12
      && errors.length === 0,
    JSON.stringify({ probes, opens, errors }),
  );

  probes.length = 0;
  opens.length = 0;
  errors.length = 0;
  await markdownOpenValidatedPath(
    '/elsewhere/secret.ts',
    undefined,
    { sessionId: 'session-a', workspaceCwd: '/workspace/project' },
    (command) => {
      probes.push(command);
      return Promise.resolve(validRead('secret.ts'));
    },
    (path, line) => { opens.push({ path, line }); },
    onError,
  );
  check('out-of-workspace absolute never probes, never opens, never errors', probes.length === 0 && opens.length === 0 && errors.length === 0);
  check(
    'out-of-workspace absolute is not workspace-relative',
    markdownWorkspaceRelativePath('/elsewhere/secret.ts', '/workspace/project') === null,
  );

  probes.length = 0;
  opens.length = 0;
  errors.length = 0;
  await markdownOpenValidatedPath(
    'src/missing.ts',
    undefined,
    { sessionId: 'session-a', workspaceCwd: '/workspace/project' },
    () => Promise.reject(new Error('file is unavailable')),
    (path, line) => { opens.push({ path, line }); },
    onError,
  );
  check('failed File Explorer probe opens nothing and reports exactly once',
    opens.length === 0 && errors.length === 1
      && errors[0] instanceof Error
      && (errors[0] as Error).message === 'file is unavailable',
    JSON.stringify({ opens, errors }));
}

console.log(`\ntoolCardPath.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  for (const f of failures) console.log(`  FAIL ${f}`);
  process.exit(1);
}
process.exit(0);
