#!/usr/bin/env node
import { createElement } from 'react';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { renderToStaticMarkup } from 'react-dom/server.browser';
import {
  CHAT_FILE_EXPLORER_TOGGLE_ID,
  applyChatFileExplorerToggle,
  applyChatMarkdownOpenPath,
  beginMarkdownProbeAttempt,
  boundedVisibleRunnerError,
  chatMarkdownExplorerOwnerKey,
  markdownProbeErrorText,
  chatMarkdownOpenFailureText,
  readGetStateCwd,
  runChatMarkdownOpenProbe,
  sessionBindingForOwner,
  shouldRetryMarkdownFileUnavailable,
  chatFileExplorerSendOptions,
} from '../src/chat/ChatApp.tsx';
import { SessionView } from '../src/chat/SessionView.tsx';
import type { ActiveMapping, ProjectRecord } from '../src/chat/mapping.ts';
import { RunnerCommandError, type RunnerView } from '../src/chat/runners.ts';
import { emptyFileExplorerSlot, emptyFileExplorerPageCache, fileExplorerApplyCachedPage, fileExplorerBeginCachedPage, fileExplorerCommandForSession, fileExplorerReadCommand, fileExplorerSyncPageCacheWithSlots, readFileExplorerSlot, writeModuleFileExplorerPageCache } from '../src/fileExplorer.ts';
import { dispatchMarkdownPathClick, markdownOpenValidatedPath, markdownOpenMissingFileVerdict, subscribeMarkdownOpenPath } from '../src/markdown.ts';
import { FILE_EXPLORER_CLOSE_BTN_ID, FILE_EXPLORER_REFRESH_BTN_ID, FILE_EXPLORER_TOGGLE_ID, FILE_EXPLORER_TREE_TOGGLE_ID, FileExplorerPanel, fileExplorerControlId, fileExplorerHandleDocumentKeyDown, fileExplorerKeydownScope, fileExplorerLineId } from '../src/panels/FileExplorerPanel.tsx';

const failures: string[] = [];
let assertions = 0;

function check(name: string, condition: boolean, detail = ''): void {
  assertions += 1;
  if (!condition) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

const project: ProjectRecord = {
  id: 'project-a',
  label: 'Project A',
  cwd: '/workspace/project-a',
  createdAt: 1,
  updatedAt: 1,
};

const runner: RunnerView = {
  id: 'runner-a',
  label: 'Runner A',
  origin: 'https://runner-a.example',
  authority: 'runner-a.example',
  createdAt: 1,
  updatedAt: 1,
  connectionEpoch: 1,
  state: 'connected',
  error: '',
  probe: { sessionId: 'source-session', runtimeIncarnation: 'inc-source-session', cwd: '/workspace/runner', sessionCount: 1, runnerCodename: null },
};

const activeMapping: ActiveMapping = {
  id: 'mapping-a',
  projectId: project.id,
  runnerId: runner.id,
  operationId: 'operation-a',
  sourceSessionId: 'source-session',
  canaryId: 'canary-a',
  createId: 'create-a',
  sessionName: 'Project A',
  createdAt: 1,
  updatedAt: 1,
  state: 'active',
  sessionId: 'mapped-session',
  sessionPath: '/workspace/sessions/mapped.jsonl',
};

{
  const events = () => () => {};
  const html = renderToStaticMarkup(createElement(SessionView, {
    runner,
    mappingId: activeMapping.id,
    sessionId: activeMapping.sessionId,
    cwd: project.cwd,
    sendCommand: async () => ({}),
    events,
    onTransportError: () => {},
  }));
  check('session chrome no longer mounts the Files toggle', !html.includes(`id="${FILE_EXPLORER_TOGGLE_ID}"`), html);
  check('session chrome labels the runner', html.includes('Session for Runner A'), html);
}
{
  const ownerA = { mappingId: 'mapping-a', runnerId: 'runner-a', sessionId: 'session-shared', workspaceCwd: '/workspace/project-a' };
  let slots = {};
  slots = applyChatMarkdownOpenPath(slots, 'src/lib.rs', 3, ownerA);
  const opened = readFileExplorerSlot(slots, 'mapping-a');
  check('owner click opens the exact mapping slot', opened.mode === 'open' && opened.activePath === 'src/lib.rs' && opened.tabs[0]?.line === 3, JSON.stringify(opened));
  check('same session on another mapping stays closed', readFileExplorerSlot(slots, 'mapping-b').mode === 'closed');

  const missing = applyChatMarkdownOpenPath(slots, 'src/lib.rs', 3, { ...ownerA, mappingId: '  ' });
  check('blank owner mapping stays inert', readFileExplorerSlot(missing, 'mapping-a').tabs.length === 1 && Object.keys(missing).length === Object.keys(slots).length);

  const traversal = applyChatMarkdownOpenPath({}, '/workspace/project-a/src/../secret.ts', undefined, ownerA);
  check('traversal outside cwd stays inert', Object.keys(traversal).length === 0, JSON.stringify(traversal));

  const outside = applyChatMarkdownOpenPath({}, '/workspace/other/secret.ts', undefined, ownerA);
  check('absolute path outside cwd stays inert', Object.keys(outside).length === 0, JSON.stringify(outside));

  const relativeAbs = applyChatMarkdownOpenPath({}, '/workspace/project-a/long.txt', undefined, ownerA);
  check(
    'absolute descendant becomes workspace-relative',
    readFileExplorerSlot(relativeAbs, 'mapping-a').activePath === 'long.txt',
    JSON.stringify(relativeAbs),
  );
}

{
  const ownerA = { mappingId: 'mapping-a', runnerId: 'runner-a', sessionId: 'shared', workspaceCwd: '/workspace/a' };
  const ownerB = { mappingId: 'mapping-b', runnerId: 'runner-b', sessionId: 'shared', workspaceCwd: '/workspace/b' };
  let slots = applyChatMarkdownOpenPath({}, 'a.ts', undefined, ownerA);
  slots = applyChatMarkdownOpenPath(slots, 'b.ts', undefined, ownerB);
  const slotA = readFileExplorerSlot(slots, 'mapping-a');
  const slotB = readFileExplorerSlot(slots, 'mapping-b');
  check('mapping A keeps its own tab', slotA.activePath === 'a.ts' && slotA.tabs.length === 1);
  check('mapping B keeps its own tab despite duplicate session', slotB.activePath === 'b.ts' && slotB.tabs.length === 1);

  const closedA = applyChatFileExplorerToggle(slots, 'mapping-a');
  check('Files toggle close preserves A tabs', readFileExplorerSlot(closedA, 'mapping-a').mode === 'closed' && readFileExplorerSlot(closedA, 'mapping-a').activePath === 'a.ts');
  check('closing A does not close B', readFileExplorerSlot(closedA, 'mapping-b').mode === 'open');
  const reopenedA = applyChatFileExplorerToggle(closedA, 'mapping-a');
  check('Files toggle reopen restores A tabs', readFileExplorerSlot(reopenedA, 'mapping-a').mode === 'open' && readFileExplorerSlot(reopenedA, 'mapping-a').activePath === 'a.ts');
  check('empty toggle mapping is inert', applyChatFileExplorerToggle(reopenedA, null) === reopenedA);
}

{
  const opened: Array<{ path: string; line?: number; mappingId: string }> = [];
  const handler = (path: string, line: number | undefined, owner: { mappingId?: string; runnerId?: string; sessionId: string; workspaceCwd?: string }) => {
    const next = applyChatMarkdownOpenPath({}, path, line, owner);
    const slot = readFileExplorerSlot(next, owner.mappingId);
    if (slot.mode === 'closed' || !owner.mappingId) return;
    opened.push({ path: slot.activePath ?? path, line, mappingId: owner.mappingId });
  };
  const unsub = subscribeMarkdownOpenPath(handler);

  function fakeTarget(opts: { mappingId?: string | null; runnerId?: string | null; sessionId?: string | null; cwd?: string | null; kind: 'code'; text: string }) {
    const mappingId = opts.mappingId ?? undefined;
    const runnerId = opts.runnerId ?? undefined;
    const session = opts.sessionId ?? undefined;
    const workspace = opts.cwd ?? undefined;
    const owner = session || workspace
      ? {
          textContent: null as string | null,
          getAttribute(name: string): string | null {
            if (name === 'data-mapping-id') return mappingId ?? null;
            if (name === 'data-runner-id') return runnerId ?? null;
            if (name === 'data-session-id') return session ?? null;
            if (name === 'data-workspace-cwd') return workspace ?? null;
            return null;
          },
          closest(): unknown { return null; },
        }
      : null;
    const node = {
      textContent: opts.text,
      getAttribute(): string | null { return null; },
      closest(sel: string): unknown {
        if (sel === '[data-session-id]') return session ? owner : null;
        if (sel === 'code.md-code' || sel === '.md-code') return opts.kind === 'code' ? node : null;
        if (sel === '.md-fence') return null;
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

  const owned = click(fakeTarget({
    mappingId: 'mapping-a',
    runnerId: 'runner-a',
    sessionId: 'session-shared',
    cwd: '/workspace/project-a',
    kind: 'code',
    text: 'src/lib.rs:3',
  }));
  check('subscribed Chat handler prevents navigation', dispatchMarkdownPathClick(owned) === true && owned.defaultPrevented === true);
  check(
    'subscribed Chat handler opens the exact rendered mapping',
    opened.length === 1 && opened[0]?.path === 'src/lib.rs' && opened[0]?.line === 3 && opened[0]?.mappingId === 'mapping-a',
    JSON.stringify(opened),
  );

  opened.length = 0;
  const missing = click(fakeTarget({ sessionId: null, cwd: null, kind: 'code', text: 'src/lib.rs:3' }));
  check('missing owner stays inert and navigates as a normal click', dispatchMarkdownPathClick(missing) === false && missing.defaultPrevented === false && opened.length === 0);


  unsub();
}

{
  const projectA: ProjectRecord = { ...project, id: 'project-a', cwd: '/workspace/project-a' };
  const projectB: ProjectRecord = { ...project, id: 'project-b', label: 'Project B', cwd: '/workspace/project-b' };
  const runnerA = { ...runner, id: 'runner-a', label: 'Runner A' };
  const runnerB = { ...runner, id: 'runner-b', label: 'Runner B', authority: 'runner-b.example', origin: 'https://runner-b.example' };
  const mappingA: ActiveMapping = { ...activeMapping, id: 'mapping-a', projectId: projectA.id, runnerId: runnerA.id, sessionId: 'session-shared', sessionPath: '/workspace/sessions/a.jsonl' };
  const mappingB: ActiveMapping = { ...activeMapping, id: 'mapping-b', projectId: projectB.id, runnerId: runnerB.id, sessionId: 'session-shared', sessionPath: '/workspace/sessions/b.jsonl' };
  const ownerARef = { mappingId: mappingA.id, runnerId: runnerA.id, sessionId: mappingA.sessionId, workspaceCwd: projectA.cwd };
  const ownerAKey = chatMarkdownExplorerOwnerKey(ownerARef, '/workspace/project-a/src/lib.rs');
  check('rendered owner uses mapping identity despite duplicate session', ownerAKey === mappingA.id);

  const ownerA = sessionBindingForOwner([projectA, projectB], [runnerA, runnerB], [mappingB, mappingA], ownerARef);
  const sendA = chatFileExplorerSendOptions(ownerA);
  const relative = applyChatMarkdownOpenPath({}, '/workspace/project-a/src/lib.rs', 12, ownerARef);
  const slotA = readFileExplorerSlot(relative, mappingA.id);
  const command = fileExplorerCommandForSession(sendA?.sourceSessionId ?? '', fileExplorerReadCommand(slotA.activePath ?? ''));
  check(
    'duplicate session routes Files command to rendered runner A',
    ownerA?.runner.id === runnerA.id
      && ownerA?.mapping.id === mappingA.id
      && sendA?.sourceSessionId === 'session-shared'
      && sendA?.workspaceCwd === '/workspace/project-a'
      && slotA.activePath === 'src/lib.rs'
      && readFileExplorerSlot(relative, mappingB.id).mode === 'closed'
      && command.type === 'file_explorer_read'
      && command.path === 'src/lib.rs'
      && command.sessionId === 'session-shared',
    JSON.stringify({ ownerA, sendA, slotA, command }),
  );

  check('session-only owner is rejected', sessionBindingForOwner([projectA, projectB], [runnerA, runnerB], [mappingA, mappingB], { sessionId: 'session-shared' }) === null);
}
{
  const ownerA = {
    mappingId: 'mapping-a',
    runnerId: 'runner-a',
    sessionId: 'session-shared',
    workspaceCwd: '/workspace/project-a',
  };
  const probes: Array<Record<string, unknown>> = [];
  const chatOpens: Array<{ path?: string; mappingId?: string }> = [];
  const chatErrors: unknown[] = [];
  const chatOpen = (path: string, _line: number | undefined, owner: { mappingId?: string }) => {
    chatOpens.push({ path, mappingId: owner.mappingId });
  };
  const chatOnError = (error: unknown) => { chatErrors.push(error); };

  await markdownOpenValidatedPath(
    'src/lib.rs',
    12,
    ownerA,
    async (probe) => {
      probes.push({ ...probe, runnerId: ownerA.runnerId, mappingId: ownerA.mappingId });
      return { path: 'src/lib.rs', lines: ['one'], totalLines: 1, sizeBytes: 1, truncated: false };
    },
    chatOpen,
    chatOnError,
  );
  check(
    'exact rendered owner validates then opens',
    probes.length === 1
      && probes[0].type === 'file_explorer_read'
      && probes[0].path === 'src/lib.rs'
      && probes[0].runnerId === 'runner-a'
      && probes[0].mappingId === 'mapping-a'
      && probes[0].maxLines === 1
      && chatOpens.length === 1
      && chatOpens[0].path === 'src/lib.rs'
      && chatOpens[0].mappingId === 'mapping-a'
      && chatErrors.length === 0,
    JSON.stringify({ probes, chatOpens, chatErrors }),
  );

  chatOpens.length = 0;
  chatErrors.length = 0;
  await markdownOpenValidatedPath(
    'src/missing.ts',
    undefined,
    ownerA,
    () => Promise.reject(new Error('file is unavailable')),
    chatOpen,
    chatOnError,
  );
  check('probe rejection reaches the Chat error owner exactly once and opens nothing',
    chatOpens.length === 0 && chatErrors.length === 1
      && chatErrors[0] instanceof Error
      && (chatErrors[0] as Error).message === 'file is unavailable',
    JSON.stringify({ chatOpens, chatErrors }));

  chatOpens.length = 0;
  chatErrors.length = 0;
  await markdownOpenValidatedPath(
    'src/lib.rs',
    undefined,
    ownerA,
    () => Promise.resolve({ path: 'src/other.ts', lines: [], totalLines: 1, sizeBytes: 1, truncated: false }),
    chatOpen,
    chatOnError,
  );
  check(
    'mismatched echoed path reports the typed missing-file verdict to the chat error owner',
    chatOpens.length === 0
      && chatErrors.length === 1
      && markdownOpenMissingFileVerdict(true, chatErrors[0]) === true
      && markdownProbeErrorText(chatErrors[0]).length > 0,
    JSON.stringify({ chatOpens, chatErrors }),
  );

  probes.length = 0;
  chatOpens.length = 0;
  chatErrors.length = 0;
  await markdownOpenValidatedPath(
    '/workspace/project-a/src/lib.rs',
    3,
    ownerA,
    (probe) => {
      probes.push({ ...probe });
      return Promise.resolve({ path: 'src/lib.rs', lines: [], totalLines: 1, sizeBytes: 1, truncated: false });
    },
    chatOpen,
    chatOnError,
  );
  check(
    'absolute descendant probes relatively and preserves mapping owner',
    probes.length === 1
      && probes[0].path === 'src/lib.rs'
      && chatOpens.length === 1
      && chatOpens[0].mappingId === 'mapping-a'
      && chatErrors.length === 0,
    JSON.stringify({ probes, chatOpens, chatErrors }),
  );

  // Promise.reject(null/undefined) through the real validation gate: the
  // error owner fires exactly once with that exact value, the open never
  // happens, nothing throws, and the production formatter renders the
  // fixed fallback (never 'null'/'undefined').
  chatOpens.length = 0;
  chatErrors.length = 0;
  await markdownOpenValidatedPath(
    'src/lib.rs',
    undefined,
    ownerA,
    () => Promise.reject(null),
    chatOpen,
    chatOnError,
  );
  check('null rejection reaches the chat error owner once and renders the fixed fallback',
    chatOpens.length === 0 && chatErrors.length === 1 && chatErrors[0] === null
      && markdownProbeErrorText(chatErrors[0]) === 'file open failed',
    JSON.stringify({ chatOpens, chatErrors }));
  chatOpens.length = 0;
  chatErrors.length = 0;
  await markdownOpenValidatedPath(
    'src/lib.rs',
    undefined,
    ownerA,
    () => Promise.reject(undefined),
    chatOpen,
    chatOnError,
  );
  check('undefined rejection reaches the chat error owner once and renders the fixed fallback',
    chatOpens.length === 0 && chatErrors.length === 1 && chatErrors[0] === undefined && markdownProbeErrorText(chatErrors[0]) === 'file open failed',
    JSON.stringify({ chatOpens, chatErrors }));
}

{
  // The Chat form-error channel renders every runner error through the
  // SAME bounded/redacted formatter: absolute paths never reach the DOM,
  // unbounded errors cap at MAX_RUNNER_ERROR_CHARS, and a bounded-empty
  // message falls back to the fixed literal the handler passes.
  const pathError = 'probe failed: /workspace/project-a/src/lib.rs:3';
  check('chat probe error formatting redacts absolute paths', boundedVisibleRunnerError(pathError) === 'probe failed: [path]', boundedVisibleRunnerError(pathError));
  check('chat probe error formatting redacts windows paths', boundedVisibleRunnerError('read C:\\workspace\\secret key failed') === 'read [path] key failed', boundedVisibleRunnerError('read C:\\workspace\\secret key failed'));
  const capped = boundedVisibleRunnerError('x'.repeat(300));
  check('chat probe error formatting caps long messages at 240', capped.length === 240 && capped.endsWith('…'), `${capped.length}:${capped.slice(-4)}`);
  check('chat probe error formatting collapses whitespace', boundedVisibleRunnerError('a\n\n  b') === 'a b');
  check('chat bounded-empty message falls back to the fixed literal', (boundedVisibleRunnerError('') || 'file open failed') === 'file open failed');
}


{
  check('empty helper starts closed', emptyFileExplorerSlot().mode === 'closed');
  const here = dirname(fileURLToPath(import.meta.url));
  const app = readFileSync(join(here, '../src/chat/ChatApp.tsx'), 'utf8');
  const mdStart = app.indexOf('const onMarkdownOpenPath');
  const mdEnd = app.indexOf('useEffect(() => subscribeMarkdownOpenPath(onMarkdownOpenPath)', mdStart);
  const markdownHandler = mdStart >= 0 && mdEnd > mdStart ? app.slice(mdStart, mdEnd) : '';
  const probeStart = app.indexOf('export function shouldRetryMarkdownFileUnavailable');
  const probeEnd = app.indexOf('export function chatProjectCompletionBinding');
  const markdownProbe = probeStart >= 0 && probeEnd > probeStart ? app.slice(probeStart, probeEnd) : '';

  check('ChatApp owns fileExplorerByMappingId', app.includes('fileExplorerByMappingId') && app.includes('useState<Record<string, FileExplorerSessionSlot>>'));
  check('ChatApp resolves exact mapping plus runner plus session', app.includes('sessionBindingForOwner') && app.includes('candidate.id === mappingId') && app.includes('candidate.runnerId === runnerId') && app.includes('candidate.sessionId === sessionId'));
  check(
    'ChatApp markdown probe rejections route through markdownProbeErrorText to the dedicated probe slot',
    markdownHandler.includes('setMarkdownProbeError(chatMarkdownOpenFailureText(error, path))')
      && !markdownHandler.includes('setFormError(')
      && !markdownHandler.includes('String(error)'),
    markdownHandler.slice(-400),
  );
  check(
    'Chat markdown retry branches only on rpcCode file_unavailable, never message regex',
    markdownProbe.includes("error.rpcCode === 'file_unavailable'")
      && markdownProbe.includes("send(options.runnerId, { type: 'get_state' }")
      && markdownProbe.includes('workspaceCwd: readGetStateCwd(response.data)')
      && !markdownProbe.includes('|| workspaceCwd')
      && !/file is unavailable/.test(markdownProbe)
      && !/error\.message/.test(markdownProbe),
    markdownProbe.slice(0, 400),
  );
  check(
    'probe error text is bounded, redacted, and falls back to the fixed literal',
    markdownProbeErrorText(null) === 'file open failed'
      && markdownProbeErrorText(undefined) === 'file open failed'
      && markdownProbeErrorText(new Error('boom')) === 'boom'
      && markdownProbeErrorText('probe failed: /workspace/project-a/src/lib.rs:3') === 'probe failed: [path]'
      && markdownProbeErrorText('x'.repeat(300)).length === 240,
    JSON.stringify({ null: markdownProbeErrorText(null), err: markdownProbeErrorText(new Error('boom')) }),
  );
  check(
    'transient probe rejections are distinguished from file verdicts by error kind, never message text',
    markdownProbeErrorText(new RunnerCommandError('Runner is not connected.', 'transport')) === 'file open failed'
      && markdownProbeErrorText(new RunnerCommandError('subscribe timed out.', 'transport')) === 'file open failed'
      && markdownProbeErrorText(new RunnerCommandError('invalid envelope', 'protocol')) === 'file open failed'
      && markdownProbeErrorText(new RunnerCommandError('file is unavailable', 'remote')) === 'file is unavailable',
    JSON.stringify({
      transport: markdownProbeErrorText(new RunnerCommandError('Runner is not connected.', 'transport')),
      protocol: markdownProbeErrorText(new RunnerCommandError('invalid envelope', 'protocol')),
      remote: markdownProbeErrorText(new RunnerCommandError('file is unavailable', 'remote')),
    }),
  );
  check(
    'probe slot clears on a new probe (target change) and on probe success, gated to the current attempt',
    markdownProbe.includes('if (isCurrentAttempt()) onProbeStarted();')
      && markdownHandler.includes("if (isCurrentAttempt()) setMarkdownProbeError('');")
      && (markdownHandler.match(/setMarkdownProbeError\(''\);/g)?.length ?? 0) === 2,
    markdownHandler.slice(-600),
  );
  check(
    'stale probe completions are gated on all three commit paths (send, success, rejection)',
    /const attempt = beginMarkdownProbeAttempt\(markdownProbeAttemptRef\);/.test(markdownHandler)
      && markdownHandler.includes('const isCurrentAttempt = attempt.isCurrent;')
      && markdownHandler.includes('if (isCurrentAttempt()) setMarkdownProbeError(\'\');')
      && markdownHandler.includes('if (!isCurrentAttempt()) return;\n        setMarkdownProbeError(\'\');')
      && markdownHandler.includes('if (!isCurrentAttempt()) return;\n        setMarkdownProbeError(chatMarkdownOpenFailureText(error, path));')
      && markdownProbe.includes('if (!isCurrentAttempt()) return;'),
    markdownHandler.slice(-900),
  );
  check(
    'probe error renders in its own session-toast row, never gated by an open form',
    app.includes('<ChatSessionToast text={markdownProbeError}')
      && app.includes("onDismiss={() => setMarkdownProbeError('')}"),
  );
  check(
    'ChatApp markdown handler has no session-only fallback',
    !/openFileInDrawer\(path, line, (sessionId|activeSession)/.test(app) && !app.includes('sessionBindingForSessionId('),
  );
  check('Chat validation probe rides the owner mapping', app.includes("send(options.runnerId, command as RunnerCommand, {") && app.includes("sourceSessionId: options.sourceSessionId,") && markdownHandler.includes('registry.command(runnerId, command, options)'));
  check('ChatApp mounts FileExplorerPanel for the explorer owner', app.includes('<FileExplorerPanel') && app.includes('sessionId={fileExplorerMappingId}') && app.includes('workspaceCwd={fileExplorerOwner.project.cwd}'));
  check('ChatApp explorer sends via owner mapping', app.includes('sendCommand={sendFileExplorerCommand') && app.includes('sendCommandForBinding(fileExplorerOwner, command)'));
  check('Chat conversation send stays on selected mapping', app.includes('sendCommand={sendSessionCommand}') && app.includes('sendCommandForBinding(activeSession, command)'));
  check('Chat Files toggle lives in the ChatApp context Files pane', app.includes("id={pane === 'files' ? CHAT_FILE_EXPLORER_TOGGLE_ID : undefined}") && app.includes('openFileExplorer') && app.includes("'Files'"));
  check('Chat close preserves tabs via drawer mode', app.includes("applyDrawerMode(slot, 'closed')") && app.includes('applyChatFileExplorerOpen'));
  check('Chat Files stays embedded in the context pane', app.includes('className="chat-context__owned-pane chat-context__files"') && app.includes('overlay={false}'));
  check('Chat Files uses exclusive panes', app.includes('exclusivePanes={true}'));
  check('Chat Files has no closed-explorer reopen button', !app.includes('{!fileExplorerOpen && ('));
  check('Chat never routes tree geometry through mapping-slot state', !app.includes('onViewStateChange') && !app.includes('viewState={') && app.includes('sessionId={fileExplorerMappingId}'));
  check('Chat download uses the verified runner origin', app.includes('downloadOrigin={fileExplorerOwner.runner.origin}') && !app.includes('downloadToken='));
}
// ---- Markdown probe attempt gate: stale completions in both orderings ----
{
  // Drives the exact gate ChatApp's onMarkdownOpenPath wiring uses: every
  // commit path (toast clear on send, success open, rejection toast) checks
  // the claimed attempt, so a slower earlier probe can never commit after a
  // newer probe started — in either completion order.
  function wireProbe(attemptRef: { current: number }, state: { toast: string; opened: string[] }) {
    const attempt = beginMarkdownProbeAttempt(attemptRef);
    const isCurrent = attempt.isCurrent;
    return {
      start: () => { if (isCurrent()) state.toast = ''; },
      succeed: (path: string) => { if (!isCurrent()) return; state.toast = ''; state.opened.push(path); },
      fail: (error: unknown) => { if (!isCurrent()) return; state.toast = chatMarkdownOpenFailureText(error, 'cited.rs'); },
    };
  }

  // Ordering 1 — the stale probe's FAILURE lands after the newer probe's
  // success: suppressed, the newer success keeps the toast clear.
  {
    const attemptRef = { current: 0 };
    const state = { toast: 'previous toast', opened: [] as string[] };
    const stale = wireProbe(attemptRef, state);
    const current = wireProbe(attemptRef, state);
    stale.start();
    current.start();
    current.succeed('b.ts');
    stale.fail(new Error('file is unavailable'));
    check('stale rejection never overwrites the newer probe success',
      state.toast === '' && state.opened.join(',') === 'b.ts', JSON.stringify(state));
  }

  // Ordering 2 — the stale probe's SUCCESS lands after the newer probe's
  // failure: suppressed, the drawer never opens for the stale target and
  // the newer failure toast survives.
  {
    const attemptRef = { current: 0 };
    const state = { toast: '', opened: [] as string[] };
    const stale = wireProbe(attemptRef, state);
    const current = wireProbe(attemptRef, state);
    stale.start();
    current.start();
    current.fail(new Error('file is unavailable'));
    stale.succeed('a.ts');
    check('stale success never overwrites the newer failure or steals the drawer',
      state.toast === 'file is unavailable' && state.opened.length === 0, JSON.stringify(state));
  }

  // Mirror seeds — the gate is generation-based, never identity-based: the
  // SECOND-claimed attempt wins even when the first owner's completion lands
  // last, and the first attempt's late success is suppressed.
  {
    const attemptRef = { current: 0 };
    const state = { toast: '', opened: [] as string[] };
    const first = wireProbe(attemptRef, state);
    const second = wireProbe(attemptRef, state);
    first.start();
    second.start();
    second.succeed('b.ts');
    first.succeed('a.ts');
    check('newer attempt wins regardless of which owner started first',
      state.toast === '' && state.opened.join(',') === 'b.ts', JSON.stringify(state));
  }

  // The gate only suppresses superseded attempts: the current attempt's own
  // commits always land.
  {
    const attemptRef = { current: 0 };
    const state = { toast: '', opened: [] as string[] };
    const solo = wireProbe(attemptRef, state);
    solo.start();
    solo.fail(new Error('file is unavailable'));
    check('current attempt commits its own rejection', state.toast === 'file is unavailable', state.toast);
  }
}

// ---- Unique per-route toggle id ----
{
  check('Chat Files toggle id is its own constant', CHAT_FILE_EXPLORER_TOGGLE_ID === 'chat-file-explorer-toggle-btn');
  check('Chat Files toggle id never collides with the App toggle', CHAT_FILE_EXPLORER_TOGGLE_ID !== FILE_EXPLORER_TOGGLE_ID);
  const here2 = dirname(fileURLToPath(import.meta.url));
  const chatApp = readFileSync(join(here2, '../src/chat/ChatApp.tsx'), 'utf8');
  check('ChatApp renders the unique Chat toggle id', chatApp.includes("id={pane === 'files' ? CHAT_FILE_EXPLORER_TOGGLE_ID : undefined}") && !chatApp.includes('id={FILE_EXPLORER_TOGGLE_ID}'));
  check(
    'ChatApp never binds the shared App toggle id in code',
    !chatApp.includes('id={FILE_EXPLORER_TOGGLE_ID}')
      && !chatApp.includes('toggleId={FILE_EXPLORER_TOGGLE_ID}')
      && chatApp.replace(/CHAT_FILE_EXPLORER_TOGGLE_ID/g, '').includes('id={FILE_EXPLORER_TOGGLE_ID}') === false,
  );
  check('ChatApp hands its own toggle id to the panel for Escape focus restore', chatApp.includes('toggleId={CHAT_FILE_EXPLORER_TOGGLE_ID}'));

  // Chat Escape focus: the panel resolves its Escape focus owner from the
  // panel root by the Chat toggle id ChatApp passes as `toggleId`.
  let focusedChat = 0;
  let chatClosed = 0;
  const chatToggle = { focus() { focusedChat += 1; } } as HTMLElement;
  const chatPanelRoot = {
    querySelector(selectors: string): HTMLElement | null {
      return selectors === `#${CHAT_FILE_EXPLORER_TOGGLE_ID}` ? chatToggle : null;
    },
  } as unknown as HTMLElement;
  const chatScope = fileExplorerKeydownScope(chatPanelRoot);
  const chatFocusTarget = chatScope?.querySelector(`#${CHAT_FILE_EXPLORER_TOGGLE_ID}`) as HTMLElement | null;
  check(
    'Escape inside the Chat drawer closes and refocuses the Chat Files toggle',
    chatScope === chatPanelRoot
      && chatFocusTarget === chatToggle
      && fileExplorerHandleDocumentKeyDown({ key: 'Escape', preventDefault() {} }, () => { chatClosed += 1; }, chatFocusTarget) === true
      && chatClosed === 1
      && focusedChat === 1,
  );
  check(
    'Chat scoped lookup keys on the Chat toggle id, never the shared App default',
    chatPanelRoot.querySelector(`#${FILE_EXPLORER_TOGGLE_ID}`) === null
      && chatPanelRoot.querySelector(`#${CHAT_FILE_EXPLORER_TOGGLE_ID}`) === chatToggle,
  );
}

// ---- Per-route DOM id derivation ----
// App (default toggleId) and Chat (CHAT_FILE_EXPLORER_TOGGLE_ID) render in
// separate documents; every drawer DOM id (refresh, tree toggle, drawer
// close, expand, code line anchors) is derived from the route's toggle id,
// keeping per-route ids unique against any document-wide lookup. App's
// default derivation stays the historical constants. The old title hide id
// is gone: Files is static text and drawer-close-btn is the only close.
{
  const panelProps = {
    sendCommand: () => Promise.resolve({}),
    hostAuthority: '127.0.0.1:1',
    pageProtocol: 'http:',
    connectionEpoch: 0,
    onOpenPath: () => {},
    onClose: () => {},
  };
  const seedFile = (sessionId: string, path: string, lines: string[]) => {
    fileExplorerSyncPageCacheWithSlots({
      [sessionId]: { tabs: [{ path, view: 'file' as const }], activePath: path, mode: 'open' },
    });
    const request = { sessionId, path, view: 'file' as const, generation: 1 };
    let cache = emptyFileExplorerPageCache();
    cache = fileExplorerBeginCachedPage(cache, request);
    cache = fileExplorerApplyCachedPage(cache, request, { path, lines, truncated: false });
    writeModuleFileExplorerPageCache(cache);
  };
  seedFile('sess-route-ids', 'src/a.ts', ['const a = 1;']);
  const slot = { tabs: [{ path: 'src/a.ts', view: 'file' as const }], activePath: 'src/a.ts', mode: 'open' as const };
  const appHtml = renderToStaticMarkup(createElement(FileExplorerPanel, { ...panelProps, sessionId: 'sess-route-ids', slot, onToggleExpanded: () => {} }));
  const chatHtml = renderToStaticMarkup(createElement(FileExplorerPanel, { ...panelProps, sessionId: 'sess-route-ids', slot, toggleId: CHAT_FILE_EXPLORER_TOGGLE_ID }));
  const ids = (html: string): Set<string> => new Set(Array.from(html.matchAll(/ id="([^"]+)"/g), (m) => m[1]));
  const appIds = ids(appHtml);
  const chatIds = ids(chatHtml);
  const overlap = [...appIds].filter((id) => chatIds.has(id));
  check('per-route drawer ids stay unique across App and Chat variants', overlap.length === 0, overlap.join(','));
  check(
    'App drawer keeps the historical control ids',
    appIds.has('file-explorer-refresh-btn')
      && appIds.has('file-explorer-tree-toggle')
      && appIds.has('file-explorer-drawer-close-btn')
      && appIds.has('file-explorer-expand-btn')
      && appIds.has('file-explorer-line-1')
      && !appIds.has('file-explorer-hide-btn'),
  );
  check(
    'Chat drawer derives every control id from its toggle id',
    chatIds.has(`${CHAT_FILE_EXPLORER_TOGGLE_ID}-refresh-btn`)
      && chatIds.has(`${CHAT_FILE_EXPLORER_TOGGLE_ID}-tree-toggle`)
      && chatIds.has(`${CHAT_FILE_EXPLORER_TOGGLE_ID}-drawer-close-btn`)
      && !chatIds.has(`${CHAT_FILE_EXPLORER_TOGGLE_ID}-expand-btn`)
      && !chatIds.has(`${CHAT_FILE_EXPLORER_TOGGLE_ID}-hide-btn`)
      && chatIds.has(`${CHAT_FILE_EXPLORER_TOGGLE_ID}-line-1`),
  );
  check(
    'Chat drawer never emits the App default ids',
    !chatIds.has('file-explorer-hide-btn')
      && !chatIds.has(FILE_EXPLORER_REFRESH_BTN_ID)
      && !chatIds.has(FILE_EXPLORER_TREE_TOGGLE_ID)
      && !chatIds.has(FILE_EXPLORER_CLOSE_BTN_ID)
      && !chatIds.has('file-explorer-expand-btn')
      && !chatIds.has('file-explorer-line-1'),
  );
  check(
    'default toggleId derivation equals the exported App constants',
    fileExplorerLineId(FILE_EXPLORER_TOGGLE_ID, 7) === 'file-explorer-line-7'
      && fileExplorerLineId(FILE_EXPLORER_TOGGLE_ID, 299) === 'file-explorer-line-299'
      && fileExplorerControlId(FILE_EXPLORER_TOGGLE_ID, 'refresh-btn') === FILE_EXPLORER_REFRESH_BTN_ID
      && fileExplorerControlId(FILE_EXPLORER_TOGGLE_ID, 'tree-toggle') === FILE_EXPLORER_TREE_TOGGLE_ID
      && fileExplorerControlId(FILE_EXPLORER_TOGGLE_ID, 'drawer-close-btn') === FILE_EXPLORER_CLOSE_BTN_ID
      && fileExplorerControlId(FILE_EXPLORER_TOGGLE_ID, 'expand-btn') === 'file-explorer-expand-btn',
  );
  check(
    'chat toggleId derivation is route-prefixed and has no fullscreen control',
    fileExplorerLineId(CHAT_FILE_EXPLORER_TOGGLE_ID, 7) === `${CHAT_FILE_EXPLORER_TOGGLE_ID}-line-7`
      && fileExplorerControlId(CHAT_FILE_EXPLORER_TOGGLE_ID, 'refresh-btn') === `${CHAT_FILE_EXPLORER_TOGGLE_ID}-refresh-btn`
      && fileExplorerControlId(CHAT_FILE_EXPLORER_TOGGLE_ID, 'tree-toggle') === `${CHAT_FILE_EXPLORER_TOGGLE_ID}-tree-toggle`
      && fileExplorerControlId(CHAT_FILE_EXPLORER_TOGGLE_ID, 'drawer-close-btn') === `${CHAT_FILE_EXPLORER_TOGGLE_ID}-drawer-close-btn`
      && !chatHtml.includes('Expand file explorer to the full app area'),
  );
  check('Chat embed has no fullscreen button', !chatHtml.includes('file-explorer-expand-btn') && !chatHtml.includes(`${CHAT_FILE_EXPLORER_TOGGLE_ID}-expand-btn`));
}

function remoteRpcError(code: string | undefined, message = 'file is unavailable'): RunnerCommandError {
  return new RunnerCommandError(message, 'remote', {
    type: 'response',
    command: 'file_explorer_read',
    success: false,
    error: message,
    data: code === undefined ? {} : { code },
  });
}

{
  check(
    'retry helper is true only for rpcCode file_unavailable before retry',
    shouldRetryMarkdownFileUnavailable(remoteRpcError('file_unavailable'), false) === true
      && shouldRetryMarkdownFileUnavailable(remoteRpcError('file_unavailable'), true) === false
      && shouldRetryMarkdownFileUnavailable(remoteRpcError('busy_capacity'), false) === false
      && shouldRetryMarkdownFileUnavailable(remoteRpcError(undefined), false) === false
      && shouldRetryMarkdownFileUnavailable(new RunnerCommandError('file is unavailable', 'transport'), false) === false,
  );
  check(
    'remote missing file names the bounded path',
    chatMarkdownOpenFailureText(remoteRpcError('file_unavailable'), 'cited.rs') === '文件已不存在：cited.rs'
    && chatMarkdownOpenFailureText(new Error('file is unavailable'), 'cited.rs') === 'file is unavailable',
  );
  check(
    'readGetStateCwd takes cwd from get_state data',
    readGetStateCwd({ cwd: '<workspace>/fresh' }) === '<workspace>/fresh'
      && readGetStateCwd({ cwd: '  <workspace>/fresh  ' }) === '<workspace>/fresh'
      && readGetStateCwd({ code: 'file_unavailable' }) === ''
      && readGetStateCwd(null) === ''
      && readGetStateCwd('cwd') === '',
  );

  const ownerA = {
    mappingId: activeMapping.id,
    runnerId: runner.id,
    sessionId: activeMapping.sessionId,
    workspaceCwd: project.cwd,
  };
  const ownerBinding = { project, runner, mapping: activeMapping };

  async function driveProbe(send: (command: { type: string }) => Promise<{ type: string; command: string; success: boolean; data?: unknown }>) {
    const sent: string[] = [];
    const opened: string[] = [];
    const failed: unknown[] = [];
    let settle: () => void = () => {};
    const finished = new Promise<void>((resolve) => { settle = resolve; });
    runChatMarkdownOpenProbe({
      path: 'src/lib.rs',
      line: 1,
      owner: ownerA,
      ownerBinding,
      workspaceCwd: project.cwd,
      retried: false,
      isCurrentAttempt: () => true,
      send: async (_runnerId, command) => {
        sent.push(command.type);
        return send(command);
      },
      onProbeStarted: () => {},
      onOpened: (relative) => { opened.push(relative); settle(); },
      onFailed: (error) => { failed.push(error); settle(); },
    });
    await finished;
    return { sent, opened, failed };
  }

  {
    let reads = 0;
    const result = await driveProbe(async (command) => {
      if (command.type === 'file_explorer_read') {
        reads += 1;
        if (reads === 1) throw remoteRpcError('file_unavailable');
        return {
          type: 'response',
          command: 'file_explorer_read',
          success: true,
          data: { path: 'src/lib.rs', lines: ['ok'], totalLines: 1, sizeBytes: 1, truncated: false },
        };
      }
      if (command.type === 'get_state') {
        return { type: 'response', command: 'get_state', success: true, data: { cwd: '<workspace>/project-a' } };
      }
      throw new Error(`unexpected ${command.type}`);
    });
    check(
      'file_unavailable retries get_state then read once',
      result.sent.join(',') === 'file_explorer_read,get_state,file_explorer_read'
        && result.opened.join(',') === 'src/lib.rs'
        && result.failed.length === 0,
      JSON.stringify(result),
    );
  }

  {
    const result = await driveProbe(async (command) => {
      if (command.type === 'file_explorer_read') throw remoteRpcError('busy_capacity');
      throw new Error(`unexpected ${command.type}`);
    });
    check(
      'busy_capacity with file-unavailable prose does not retry',
      result.sent.join(',') === 'file_explorer_read'
        && result.opened.length === 0
        && result.failed.length === 1,
      JSON.stringify(result.sent),
    );
  }

  {
    const result = await driveProbe(async (command) => {
      if (command.type === 'file_explorer_read') throw remoteRpcError('file_unavailable');
      if (command.type === 'get_state') {
        return { type: 'response', command: 'get_state', success: true, data: { cwd: '<workspace>/project-a' } };
      }
      throw new Error(`unexpected ${command.type}`);
    });
    check(
      'file_unavailable retries only once',
      result.sent.filter((type) => type === 'file_explorer_read').length === 2
        && result.sent.filter((type) => type === 'get_state').length === 1
        && result.opened.length === 0
        && result.failed.length === 1
        && shouldRetryMarkdownFileUnavailable(result.failed[0], true) === false,
      JSON.stringify(result.sent),
    );
  }

  {
    const result = await driveProbe(async (command) => {
      if (command.type === 'file_explorer_read') throw remoteRpcError('file_unavailable');
      if (command.type === 'get_state') throw new Error('get_state refresh failed');
      throw new Error(`unexpected ${command.type}`);
    });
    check(
      'get_state refresh failure is surfaced',
      result.sent.join(',') === 'file_explorer_read,get_state'
        && result.opened.length === 0
        && result.failed.length === 1
        && result.failed[0] instanceof Error
        && result.failed[0].message === 'get_state refresh failed',
      JSON.stringify(result),
    );
  }
}

console.log(`\nchatMarkdownExplorer.test: ${assertions} assertions, ${failures.length} failure(s)`);
if (failures.length > 0) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);
