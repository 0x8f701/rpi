#!/usr/bin/env node
// Focused pure-contract regression for the authoritative Todo child Web panel.
import fs from 'node:fs';
import path from 'node:path';
import {
  admitsTodoChildDetailResponse,
  buildTodoChildCommand,
  isTodoChildTransportOperationError,
  matchesTodoChildSelection,
  parseTodoChildDetailSnapshot,
  parseTodoChildLifecycleSnapshot,
  projectTodoChildLifecycleSnapshot,
  parseTodoChildManifest,
  parseTodoChildSelectedId,
  retainTodoChildOperationErrorAfterRecovery,
  selectedOrFirstTodoChildId,
  todoChildCapabilities,
  todoChildPanelOperationError,
} from '../src/panels/TodoChildPanel.tsx';

const failures: string[] = [];
let ran = 0;
function check(name: string, condition: unknown, detail = ''): void {
  ran += 1;
  if (!condition) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

const parentSessionId = 'parent-session';
const childSessionId = 'child-session';
const detailWire = (revision = 7) => ({
  parentSessionId,
  childSessionId,
  todoId: childSessionId,
  parent: { sessionId: parentSessionId, entryId: 'entry-1', revision: 12 },
  workspace: { workspace: '/workspace', cwd: '/workspace' },
  status: 'running',
  isRunning: true,
  revision,
  label: 'autonomous child',
  createdAt: 1,
  lastError: null,
  transcriptEntries: 1,
  tasks: [{
    id: 'task-1', content: 'work', dependsOn: [], status: 'running', agent: null,
    role: null, createdAt: 2, summary: null,
  }],
  receipts: [{
    id: 'receipt-1', parentSessionId, childSessionId, messageId: 'message-1',
    acceptedAt: 3, status: 'queued', summary: null,
  }],
  supervisor: { id: 'supervisor-1', displayName: 'main', role: 'supervisor', status: 'idle', taskId: null },
  agents: [{ id: 'agent-1', displayName: 'worker', role: 'writer', status: 'running', taskId: 'task-1' }],
  jobs: [{ jobId: 'job-1', displayName: 'worker', role: 'writer', status: 'running', taskId: 'task-1', summary: null }],
  activity: [{ atMs: 4, actor: 'worker', kind: 'task', text: 'working' }],
  irc: [{ messageId: 'irc-1', from: 'main', to: 'worker', text: 'continue', atMs: 5, replyTo: null }],
  transcript: [{ id: 'transcript-1', role: 'assistant', actor: 'worker', text: 'progress', atMs: 6, interrupted: false }],
});

{
  const manifest = parseTodoChildManifest([
    { childSessionId, label: 'one', createdAt: 1, status: 'running', parentEntryId: 'entry-1', parentRevision: 12, kind: 'todo' },
  ]);
  check('manifest accepts authoritative todo rows', manifest?.[0]?.childSessionId === childSessionId);
  check('manifest rejects duplicate child identity', parseTodoChildManifest([
    { childSessionId, createdAt: 1, status: 'running', parentRevision: 12, kind: 'todo' },
    { childSessionId, createdAt: 2, status: 'idle', parentRevision: 13, kind: 'todo' },
  ]) === null);
  check('manifest rejects ordinary/non-todo rows', parseTodoChildManifest([
    { childSessionId, createdAt: 1, status: 'running', parentRevision: 12, kind: 'workflow' },
  ]) === null);
}

{
  const snapshot = parseTodoChildDetailSnapshot(detailWire());
  check('detail parses complete frozen projection', snapshot?.revision === 7);
  check('detail matches exact parent+child identity', !!snapshot && matchesTodoChildSelection(snapshot, parentSessionId, childSessionId));
  check('foreign parent detail fails closed', parseTodoChildDetailSnapshot({
    ...detailWire(), parentSessionId: 'foreign-parent',
  }) === null);
  check('foreign child detail fails closed', parseTodoChildDetailSnapshot({
    ...detailWire(), childSessionId: 'foreign-child',
  }) === null);
  check('receipt identity mismatch fails closed', parseTodoChildDetailSnapshot({
    ...detailWire(), receipts: [{ ...detailWire().receipts[0], childSessionId: 'foreign-child' }],
  }) === null);
  check('duplicate nested identity fails closed', parseTodoChildDetailSnapshot({
    ...detailWire(), tasks: [detailWire().tasks[0], detailWire().tasks[0]],
  }) === null);
  check('supervisor is not duplicated in agent roster', parseTodoChildDetailSnapshot({
    ...detailWire(), agents: [{ ...detailWire().supervisor }],
  }) === null);
}

{
  const snapshot = parseTodoChildDetailSnapshot(detailWire());
  if (!snapshot) throw new Error('fixture failed to parse');
  const base = {
    snapshot, parentSessionId, childSessionId, requestSequence: 4,
    currentRequestSequence: 4, selectionSequence: 2, currentSelectionSequence: 2,
    currentRevision: 7,
  };
  check('current exact response admitted', admitsTodoChildDetailResponse(base));
  check('late request response rejected', !admitsTodoChildDetailResponse({ ...base, requestSequence: 3 }));
  check('prior selection response rejected', !admitsTodoChildDetailResponse({ ...base, selectionSequence: 1 }));
  check('older revision response rejected', !admitsTodoChildDetailResponse({
    ...base, snapshot: { ...snapshot, revision: 6 },
  }));
  check('foreign identity response rejected', !admitsTodoChildDetailResponse({
    ...base, childSessionId: 'other-child',
  }));
}

{
  const current = parseTodoChildDetailSnapshot(detailWire());
  const stopped = parseTodoChildLifecycleSnapshot({
    ...detailWire(),
    status: 'stopped',
    isRunning: false,
    transcriptEntries: 2,
    receipts: [{ ...detailWire().receipts[0], status: 'failed', summary: 'stopped before execution' }],
  });
  if (!current || !stopped) throw new Error('lifecycle projection fixture failed to parse');
  const projected = projectTodoChildLifecycleSnapshot(current, stopped, parentSessionId, childSessionId);
  check('authoritative stopped action projects selected detail immediately', projected?.status === 'stopped' && projected.isRunning === false);
  check('stopped action projection keeps detail-only collections', projected?.irc[0]?.messageId === 'irc-1' && projected.transcript[0]?.id === 'transcript-1');
  check('stopped action projection consumes authoritative receipt settlement', projected?.receipts[0]?.status === 'failed');
  check('foreign lifecycle response fails closed', projectTodoChildLifecycleSnapshot(
    current,
    { ...stopped, parentSessionId: 'foreign-parent' },
    parentSessionId,
    childSessionId,
  ) === null);
  check('regressing lifecycle revision fails closed', projectTodoChildLifecycleSnapshot(
    current,
    { ...stopped, revision: current.revision - 1 },
    parentSessionId,
    childSessionId,
  ) === null);
  check('pre-lifecycle detail response cannot repaint stopped projection', !admitsTodoChildDetailResponse({
    snapshot: current,
    parentSessionId,
    childSessionId,
    requestSequence: 4,
    currentRequestSequence: 5,
    selectionSequence: 2,
    currentSelectionSequence: 2,
    currentRevision: projected?.revision ?? 7,
  }));
}

{
  const command = buildTodoChildCommand(parentSessionId, 'todo_child_add_task', {
    childSessionId, content: 'work', expectedRevision: 7,
  });
  check('command includes exact parent identity', command.parentSessionId === parentSessionId);
  check('command includes exact child identity', command.childSessionId === childSessionId);
  check('command type cannot be overridden by extra fields',
    buildTodoChildCommand(parentSessionId, 'todo_child_send', { type: 'todo_op' }).type === 'todo_child_send');
  check('parent identity cannot be overridden by extra fields',
    buildTodoChildCommand(parentSessionId, 'todo_child_send', { parentSessionId: 'foreign-parent' }).parentSessionId === parentSessionId);
}

{
  check('running exposes active controls', JSON.stringify(todoChildCapabilities('running')) === JSON.stringify({
    canPause: true, canResume: false, canStop: true, canCancel: true, canArchive: false,
    canSend: true, canAddTask: true, canHubSend: true,
  }));
  check('paused keeps durable queue controls but blocks child IRC', JSON.stringify(todoChildCapabilities('paused')) === JSON.stringify({
    canPause: false, canResume: true, canStop: true, canCancel: true, canArchive: false,
    canSend: true, canAddTask: true, canHubSend: false,
  }));
  check('failed is resumable and blocks child IRC', JSON.stringify(todoChildCapabilities('failed')) === JSON.stringify({
    canPause: false, canResume: true, canStop: true, canCancel: true, canArchive: false,
    canSend: true, canAddTask: true, canHubSend: false,
  }));
  check('stopped exposes archive only', JSON.stringify(todoChildCapabilities('stopped')) === JSON.stringify({
    canPause: false, canResume: false, canStop: false, canCancel: false, canArchive: true,
    canSend: false, canAddTask: false, canHubSend: false,
  }));
}

{
  const entries = parseTodoChildManifest([
    { childSessionId, label: 'one', createdAt: 1, status: 'running', parentRevision: 12, kind: 'todo' },
    { childSessionId: 'child-two', label: 'two', createdAt: 2, status: 'idle', parentRevision: 12, kind: 'todo' },
  ]);
  if (!entries) throw new Error('selection fixture failed to parse');
  check('authoritative selected child restores exact identity',
    parseTodoChildSelectedId({ parentSessionId, childSessionId }, parentSessionId, entries) === childSessionId);
  check('foreign parent selection fails closed',
    parseTodoChildSelectedId({ parentSessionId: 'other-parent', childSessionId }, parentSessionId, entries) === null);
  check('unlisted child selection fails closed',
    parseTodoChildSelectedId({ parentSessionId, childSessionId: 'missing-child' }, parentSessionId, entries) === null);
  check('successful stale selection falls back to first manifest child',
    selectedOrFirstTodoChildId(
      { parentSessionId, childSessionId: 'missing-child' },
      parentSessionId,
      entries,
    ) === childSessionId);
  check('foreign successful status does not choose a fallback child',
    selectedOrFirstTodoChildId(
      { parentSessionId: 'other-parent', childSessionId },
      parentSessionId,
      entries,
    ) === null);
}

{
  const droppedSend = todoChildPanelOperationError(
    new Error('send failed: connection closed (code 1006)'),
    'todo_child_send failed',
  );
  const currentRpcFailure = todoChildPanelOperationError(
    Object.assign(new Error('connection closed by policy'), { rpc: true }),
    'todo_child_send failed',
  );
  const currentValidationFailure = todoChildPanelOperationError(
    null,
    'invalid todo_child_hub_send response',
  );
  check('connection drop rejection is classified as transport-owned',
    droppedSend.source === 'transport'
      && isTodoChildTransportOperationError(droppedSend.message));
  check('successful recovery clears obsolete transport operation error',
    retainTodoChildOperationErrorAfterRecovery(droppedSend) === null);
  check('successful recovery retains current RPC operation error',
    retainTodoChildOperationErrorAfterRecovery(currentRpcFailure) === currentRpcFailure);
  check('successful recovery retains current validation operation error',
    retainTodoChildOperationErrorAfterRecovery(currentValidationFailure) === currentValidationFailure);
}

{
  const root = process.cwd();
  const app = fs.readFileSync(path.join(root, 'src/App.tsx'), 'utf8');
  const tasks = fs.readFileSync(path.join(root, 'src/panels/TodoPanel.tsx'), 'utf8');
  const children = fs.readFileSync(path.join(root, 'src/panels/TodoChildPanel.tsx'), 'utf8');
  const detailView = fs.readFileSync(path.join(root, 'src/components/TodoChildDetail.tsx'), 'utf8');
  const styles = fs.readFileSync(path.join(root, 'src/styles.css'), 'utf8');
  const pending = fs.readFileSync(path.join(root, 'src/pending.ts'), 'utf8');


  const todoSources = `${app}\n${tasks}\n${children}\n${detailView}`;
  const routeStart = app.indexOf("if (action.type === 'todo')");
  const routeBlock = routeStart >= 0 ? app.slice(routeStart, routeStart + 2400) : '';

  check('sidebar exposes exactly one Todo entry',
    (app.match(/id="todo-toggle-btn"/g) || []).length === 1
      && !app.includes('id="todos-toggle-btn"')
      && !app.includes('id="todo-child-toggle-btn"'));
  check('Workflows remain a separate top-level module',
    app.includes('id="workflow-toggle-btn"') && app.includes('>\n                Workflows\n'));
  check('Todo navigation has only the unified top-level panel value',
    app.includes("openPanel('todo')")
      && !app.includes("openPanel('todo-child')")
      && !app.includes("setActivePanel('todo-child')")
      && !/activePanel\s*===\s*['"]todo-child['"]/.test(app));
  check('/todo force-opens the unified panel without an alias-focus action',
    routeBlock.includes('acceptCommand()')
      && routeBlock.includes("openPanel('todo', { force: true })")
      && !app.includes('setTodoChildFocusRequest')
      && !app.includes('todoChildFocusRequest'));
  check('App has no Todo child focus or scroll side effect',
    !app.includes("getElementById('todo-child-list')")
      && !app.includes("getElementById('todo-child-list')?.focus")
      && !app.includes("querySelector('#todo-child-list')?.scrollIntoView"));
  check('App mounts only one TodoPanel and no sibling TodoChildPanel',
    (app.match(/<TodoPanel\b/g) || []).length === 1
      && !app.includes('<TodoChildPanel')
      && (todoSources.match(/id="todo-panel"/g) || []).length === 1);
  check('unified Todo ARIA is not DAG-only',
    tasks.includes('aria-label="Todo panel"')
      && !tasks.includes('aria-label="Todo DAG panel"'));
  check('TodoPanel embeds the sole child controller after the labelled Tasks section',
    (tasks.match(/<TodoChildPanel\b/g) || []).length === 1
      && /<section\s+className="todo-panel__tasks"\s+aria-labelledby="todo-tasks-heading"[\s\S]*<TodoChildPanel\b/.test(tasks)
      && tasks.includes('id="todo-tasks-heading"')
      && tasks.includes('parentSessionId={parentSessionId}')
      && tasks.includes('sendCommand={sendCommand}'));
  check('embedded child controller retains simultaneous detail projection',
    children.includes('className="todo-child-section"')
      && children.includes('id="todo-child-list"')
      && children.includes('tabIndex={-1}')
      && children.includes('id="todo-child-detail"')
      && children.includes('<TodoChildDetail snapshot={detail} />'));
  check('selected child renders every complete collection with ordinary Tasks still mounted',
    ['Tasks', 'Receipts', 'Agents', 'Jobs', 'Activity', 'IRC', 'Transcript']
      .every((label) => detailView.includes(`<DetailSection label="${label}"`))
      && tasks.includes('className="todo-panel__tasks"')
      && tasks.includes('<TodoChildPanel'));
  check('obsolete exclusive Todo roots, tabs, callbacks, and classes are absent',
    !todoSources.includes('id="todo-child-panel"')
      && !todoSources.includes('id="todo-child-close-btn"')
      && !todoSources.includes('id="todo-child-sessions-tab"')
      && !todoSources.includes('id="todo-tasks-tab"')
      && !todoSources.includes('onShowChildSessions')
      && !todoSources.includes('onShowTasks')
      && !todoSources.includes('role="tablist"')
      && !todoSources.includes('todo-child-panel')
      && !styles.includes('.todo-child-panel')
      && !styles.includes('.todo-module__tab'));
  check('unified Todo drawer and resizer use the approved desktop width',
    /\.todo-panel\s*\{[^}]*width:\s*min\(720px,\s*94vw\)/s.test(styles)
      && /body:has\(\.todo-panel\)\s+\.panel-drawer-resizer\s*\{[^}]*width:\s*min\(720px,\s*94vw\)/s.test(styles));
  check('successful stale backend selection uses the first manifest child',
    children.includes('selectedOrFirstTodoChildId(data, owner, entries)'));
  check('foreign status cannot trigger first-child fallback',
    children.includes("asString(row.parentSessionId) !== parentSessionId) return null"));
  check('child selection stays panel/backend authoritative, not mirrored in App',
    !app.includes('selectedTodoChild') && !app.includes('todoChildSelection'));
  check('production Web has no todo-child slash alias',
    !app.includes("action.type === 'todo-child'")
      && !app.includes("openPanel('todo-child')")
      && !app.includes("setActivePanel('todo-child')"));
  check('Todo child RPC failures stay panel-owned instead of duplicating global toasts',
    app.includes('rpcFailureToastOwned')
      && pending.includes("type.startsWith('todo_child_')")
      && children.includes("setListError(err.message || 'todo_child_status failed')"));

  check('successful reconnect clears only transport-owned toasts',
    app.includes("kind?: 'transport'")
      && app.includes("entry.kind !== 'transport'"));

  const createBlock = children.slice(
    children.indexOf('const createChild = useCallback'),
    children.indexOf('const sendMessage = useCallback'),
  );
  const sendBlock = children.slice(
    children.indexOf('const sendMessage = useCallback'),
    children.indexOf('const addTask = useCallback'),
  );
  const addTaskBlock = children.slice(
    children.indexOf('const addTask = useCallback'),
    children.indexOf('const sendHub = useCallback'),
  );
  const hubBlock = children.slice(
    children.indexOf('const sendHub = useCallback'),
    children.indexOf('const selectedBusy'),
  );
  check('createChild/sendMessage/addTask/sendHub claim exclusive refs',
    children.includes('const creatingRef = useRef(false);')
      && children.includes('const sendingRef = useRef(false);')
      && children.includes('const addingTaskRef = useRef(false);')
      && children.includes('const hubSendingRef = useRef(false);')
      && createBlock.includes('if (!owner || creatingRef.current) return;')
      && sendBlock.includes('if (!owner || !childSessionId || !message || sendingRef.current) return;')
      && addTaskBlock.includes('if (!owner || !childSessionId || !content || addingTaskRef.current) return;')
      && hubBlock.includes('if (!owner || !childSessionId || !to || !body || hubSendingRef.current) return;'));
  check('createChild claims creatingRef after validation and before sendCommand',
    createBlock.indexOf('if (!owner || creatingRef.current) return;') < createBlock.indexOf('creatingRef.current = true;')
      && createBlock.indexOf('creatingRef.current = true;') < createBlock.indexOf("sendCommand(command('todo_child_create'")
      && createBlock.includes('setCreating(true);')
      && createBlock.includes('creatingRef.current = false;')
      && createBlock.includes('if (mountedRef.current) setCreating(false);')
      && !/if\s*\(\s*!label\s*\)\s*return/.test(createBlock));
  check('sendMessage claims sendingRef after validation and before sendCommand',
    sendBlock.indexOf('if (!owner || !childSessionId || !message || sendingRef.current) return;') < sendBlock.indexOf('sendingRef.current = true;')
      && sendBlock.indexOf('sendingRef.current = true;') < sendBlock.indexOf("sendCommand(command('todo_child_send'")
      && sendBlock.includes('setSending(true);')
      && sendBlock.includes('sendingRef.current = false;')
      && sendBlock.includes('if (mountedRef.current) setSending(false);'));
  check('addTask claims addingTaskRef after validation and before sendCommand',
    addTaskBlock.indexOf('if (!owner || !childSessionId || !content || addingTaskRef.current) return;') < addTaskBlock.indexOf('addingTaskRef.current = true;')
      && addTaskBlock.indexOf('addingTaskRef.current = true;') < addTaskBlock.indexOf("sendCommand(command('todo_child_add_task'")
      && addTaskBlock.includes('setAddingTask(true);')
      && addTaskBlock.includes('addingTaskRef.current = false;')
      && addTaskBlock.includes('if (mountedRef.current) setAddingTask(false);'));
  check('sendHub claims hubSendingRef after validation and before sendCommand',
    hubBlock.indexOf('if (!owner || !childSessionId || !to || !body || hubSendingRef.current) return;') < hubBlock.indexOf('hubSendingRef.current = true;')
      && hubBlock.indexOf('hubSendingRef.current = true;') < hubBlock.indexOf("sendCommand(command('todo_child_hub_send'")
      && hubBlock.includes('setHubSending(true);')
      && hubBlock.includes('hubSendingRef.current = false;')
      && hubBlock.includes('if (mountedRef.current) setHubSending(false);'));
  check('ACK-only trim-match clears drafts without touching hubTo',
    createBlock.includes("setCreateLabel((current) => current.trim() === label ? '' : current)")
      && sendBlock.includes("setSendBody((current) => current.trim() === message ? '' : current)")
      && addTaskBlock.includes("setTaskContent((current) => current.trim() === content ? '' : current)")
      && addTaskBlock.includes("setTaskDependsOn((current) => current === rawDependsOn ? '' : current)")
      && addTaskBlock.includes("setTaskAgent((current) => current === rawAgent ? '' : current)")
      && addTaskBlock.includes("setTaskRole((current) => current === rawRole ? '' : current)")
      && hubBlock.includes("setHubBody((current) => current.trim() === body ? '' : current)")
      && hubBlock.includes("setHubReplyTo((current) => current.trim() === replyTo ? '' : current)")
      && !hubBlock.includes('setHubTo(')
      && createBlock.indexOf("sendCommand(command('todo_child_create'") < createBlock.indexOf('setCreateLabel((current)')
      && sendBlock.indexOf("sendCommand(command('todo_child_send'") < sendBlock.indexOf('setSendBody((current)')
      && addTaskBlock.indexOf("sendCommand(command('todo_child_add_task'") < addTaskBlock.indexOf('setTaskContent((current)')
      && hubBlock.indexOf("sendCommand(command('todo_child_hub_send'") < hubBlock.indexOf('setHubBody((current)'));
  check('invalid create response still errors and does not clear',
    createBlock.includes("setCreateError(todoChildPanelOperationError(null, 'invalid todo_child_create response'))")
      && createBlock.includes('todo_child_create failed')
      && createBlock.indexOf("setCreateError(todoChildPanelOperationError(null, 'invalid todo_child_create response'))")
        < createBlock.indexOf('setCreateLabel((current)')
      && createBlock.slice(
        createBlock.indexOf("setCreateError(todoChildPanelOperationError(null, 'invalid todo_child_create response'))"),
        createBlock.indexOf('setCreateLabel((current)'),
      ).includes('return;'));
  check('mutator catch strings stay panel-owned',
    sendBlock.includes("setSendError(todoChildPanelOperationError(err, 'todo_child_send failed'))")
      && addTaskBlock.includes("setTaskError(todoChildPanelOperationError(err, 'todo_child_add_task failed'))")
      && hubBlock.includes("setHubError(todoChildPanelOperationError(null, 'invalid todo_child_hub_send response'))")
      && hubBlock.includes("setHubError(todoChildPanelOperationError(err, 'todo_child_hub_send failed'))"));
}

console.log(`\ntodoChildPanel.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);
