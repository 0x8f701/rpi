#!/usr/bin/env node
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server.browser';
import { act, create, type ReactTestRenderer } from 'react-test-renderer';
import {
  SessionView,
  TranscriptItem,
  acceptPendingUser,
  applySessionEvent,
  applySessionSnapshot,
  dispatchComposerPrompt,
  mergeSessionHistory,
  registerPendingUser,
  removePendingUser,
  sendChatSessionAttachmentToTmux,
  sessionEventBelongsTo,
} from '../src/chat/SessionView.tsx';
import type { SessionTranscriptState } from '../src/chat/SessionView.tsx';
import { sessionBindingForMapping } from '../src/chat/ChatApp.tsx';
import { TeamStore, activeProjectMembers, runnerSessionKey } from '../src/chat/team.ts';
import type { ActiveMapping, ProjectRecord, ProjectRunnerMapping } from '../src/chat/mapping.ts';
import { isAuthoritativeSubscriptionCut } from '../src/protocol.ts';
import type { RunnerView } from '../src/chat/runners.ts';
import type { RunnerCommand } from '../src/chat/protocol.ts';
import type { EventFrame } from '../src/types.ts';
import type { Item } from '../src/transcript.ts';
import { dispatchMarkdownPathClick, subscribeMarkdownOpenPath } from '../src/markdown.ts';
import type { ComposerAttachment } from '../src/attachments.ts';
import { rememberTmuxFileDestination } from '../src/tmuxFile.ts';
import { planTurnStreaming } from '../src/turnStreaming.ts';

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

const emptyTranscript = (): SessionTranscriptState => ({ items: [], activeAssistantId: null });

// Ready is the only mapping state that transitions into the session detail.
{
  const binding = sessionBindingForMapping(project, runner, activeMapping);
  check('ready mapping opens a binding', binding !== null);
  check('binding keeps mapped session identity', binding?.mapping.sessionId === 'mapped-session');
  const provisioning: ProjectRunnerMapping = { ...activeMapping, state: 'provisioning' };
  check('provisioning mapping is inert', sessionBindingForMapping(project, runner, provisioning) === null);
  check('unmapped mapping is inert', sessionBindingForMapping(project, runner, undefined) === null);
}

// dispatchComposerPrompt returns structured statuses: empty/disconnected never
// send; a wired turnId yields `accepted`, otherwise `handled`.
{
  const commands: RunnerCommand[] = [];
  let respond: (command: RunnerCommand) => unknown = () => ({ turnId: 'turn-9' });
  const send = async (command: RunnerCommand) => {
    commands.push(command);
    return respond(command);
  };

  const accepted = await dispatchComposerPrompt('  hello runner  ', true, send);
  check('accepted dispatch carries structured status', accepted.status === 'accepted', JSON.stringify(accepted));
  check('accepted dispatch binds authoritative turnId', accepted.status === 'accepted' && accepted.turnId === 'turn-9');
  check('composer sends exact prompt shape', JSON.stringify(commands) === JSON.stringify([
    { type: 'prompt', message: 'hello runner' },
  ]), JSON.stringify(commands));

  respond = () => ({});
  const handled = await dispatchComposerPrompt('  hello runner  ', true, send);
  check('dispatch without turn id is handled', handled.status === 'handled');

  const empty = await dispatchComposerPrompt(' \n ', true, send);
  check('empty composer is rejected', empty.status === 'empty');
  const disconnected = await dispatchComposerPrompt('later', false, send);
  check('disconnected composer is rejected', disconnected.status === 'disconnected');
  check('rejected composer never dispatches', commands.length === 2, String(commands.length));
}

{
  const commands: RunnerCommand[] = [];
  const send = async (command: RunnerCommand) => {
    commands.push(command);
    return {};
  };
  const steered = await dispatchComposerPrompt('steer now', true, send, true);
  check('submit-while-streaming routes steer', steered.status === 'handled'
    && JSON.stringify(commands) === JSON.stringify([{ type: 'steer', message: 'steer now' }]), JSON.stringify(commands));
}

{
  const commands: RunnerCommand[] = [];
  const send = async (command: RunnerCommand) => {
    commands.push(command);
    if (command.type === 'prompt') {
      throw Object.assign(new Error('session is already processing; choose steer or followUp'), {
        rpcCode: 'session_processing',
      });
    }
    return {};
  };
  const converted = await dispatchComposerPrompt('keep this', true, send, false);
  check('typed prompt-failure auto-converts to steer once', converted.status === 'handled'
    && commands.length === 2
    && commands[0]?.type === 'prompt'
    && commands[1]?.type === 'steer'
    && JSON.stringify(commands) === JSON.stringify([
      { type: 'prompt', message: 'keep this' },
      { type: 'steer', message: 'keep this' },
    ]), JSON.stringify(commands));
}

{
  const commands: RunnerCommand[] = [];
  const send = async (command: RunnerCommand) => {
    commands.push(command);
    if (command.type === 'goal_create') return 'started';
    if (command.type === 'goal_get') return { revision: 1, current: null };
    return {};
  };
  const opened = await dispatchComposerPrompt('/goal', true, send);
  check('bare /goal opens the goal overlay', opened.status === 'open_goal');
  check('bare /goal does not send an RPC', commands.length === 0, String(commands.length));
  const created = await dispatchComposerPrompt('/goal create ship it', true, send);
  check('create /goal sends goal_create then goal_get', created.status === 'slash'
    && commands.some((command) => command.type === 'goal_create')
    && commands.some((command) => command.type === 'goal_get'), JSON.stringify(commands));
  const rejected = await dispatchComposerPrompt('/jobs', true, send);
  check('non-goal slash is rejected in chat', rejected.status === 'slash'
    && rejected.status === 'slash' && rejected.text.includes('not available in chat'), JSON.stringify(rejected));
  const clearCommands = commands.length;
  const cleared = await dispatchComposerPrompt('/clear', true, send);
  check('/clear is a slash reject', cleared.status === 'slash'
    && cleared.status === 'slash' && cleared.text.includes('not available in chat'), JSON.stringify(cleared));
  check('/clear never sends a prompt RPC', commands.length === clearCommands
    && !commands.some((command) => command.type === 'prompt'), JSON.stringify(commands));
}

{
  const channel = readFileSync(join(process.cwd(), 'src/chat/BroadcastConversation.tsx'), 'utf8');
  check('Channel /goal does not open Goal', channel.includes("'Open a runner session and use /goal there.'")
    && !channel.includes("status === 'open_goal'")
    && !channel.includes('setGoalOpen'));
  check('Channel composer keeps focus without scrolling', channel.includes('focus({ preventScroll: true })') && !channel.includes('scrollIntoView'));
  check('Channel composer grows through the shared auto-resize controller', channel.includes('createAutoResizeController') && channel.includes('createScrollPin()'));
  const channelTextarea = channel.match(/<textarea[\s\S]*?\/>/)?.[0] ?? '';
  check('Channel composer textarea is never disabled', channelTextarea.includes('aria-label="Channel message"') && !channelTextarea.includes('disabled='));
  check('Channel CommandPicker stays mounted', channel.includes('<CommandPicker') && channel.includes('className="chat-session__composer"'));
}

{
  const view = readFileSync(join(process.cwd(), 'src/chat/SessionView.tsx'), 'utf8');
  const slashArm = view.slice(view.indexOf("result.status === 'slash'"), view.indexOf("result.status === 'open_goal'"));
  check('slash composer result restores submittedDraft', slashArm.includes('setDraft(submittedDraft)'));
}

// Session-scoped event filtering and live assistant/tool projection.
{
  check('owned frame accepted', sessionEventBelongsTo({ type: 'message_start', sessionId: 'mapped-session' }, 'mapped-session'));
  check('other session rejected', !sessionEventBelongsTo({ type: 'message_start', sessionId: 'other' }, 'mapped-session'));
  check('unowned frame rejected', !sessionEventBelongsTo({ type: 'message_start' }, 'mapped-session'));

  let state = { items: [] as Item[], activeAssistantId: null as string | null };
  state = applySessionEvent(state, {
    type: 'message_start',
    sessionId: 'mapped-session',
    itemId: 'assistant-1',
    message: { role: 'assistant' },
  });
  state = applySessionEvent(state, {
    type: 'message_update',
    sessionId: 'mapped-session',
    itemId: 'assistant-1',
    assistantMessageEvent: { type: 'text_delta', delta: 'streamed' },
  });
  const assistant = state.items[0];
  check('assistant stream row created', assistant?.kind === 'assistant' && assistant.status === 'streaming');
  check('assistant delta appended', assistant?.kind === 'assistant' && assistant.blocks[0]?.text === 'streamed');


  state = applySessionEvent(state, {
    type: 'tool_execution_start',
    sessionId: 'mapped-session',
    itemId: 'tool-1',
    toolCallId: 'call-1',
    toolName: 'read',
    args: { path: '/tmp/file' },
  });
  state = applySessionEvent(state, {
    type: 'tool_execution_end',
    sessionId: 'mapped-session',
    toolCallId: 'call-1',
    result: { content: [{ type: 'text', text: 'done' }] },
    isError: false,
  });
  const tool = state.items.find((item) => item.kind === 'toolCard');
  check('tool row completes', tool?.kind === 'toolCard' && tool.status === 'done');
  check('tool row keeps bounded result', tool?.kind === 'toolCard' && tool.result === 'done');
}

// Optimistic user queue: register shows the row immediately, accept binds the
// turnId, and the authoritative user message_start replaces the placeholder.
{
  const registered = registerPendingUser(emptyTranscript(), 'hello runner', 'u-pend-1');
  const pendingRow = registered.items.find((item) => item.kind === 'user');
  check('pending register shows optimistic user row', pendingRow?.kind === 'user'
    && pendingRow.id === 'u-pend-1'
    && pendingRow.text === 'hello runner'
    && pendingRow.optimistic, JSON.stringify(registered.items));
  check('pending register tracks queue entry', registered.pendingUsers?.some((pending) => pending.id === 'u-pend-1' && pending.text === 'hello runner'));

  const reRegistered = registerPendingUser(registered, 'hello runner', 'u-pend-1');
  check('pending register is idempotent', reRegistered.items.filter((item) => item.id === 'u-pend-1').length === 1
    && (reRegistered.pendingUsers ?? []).length === 1);

  const accepted = acceptPendingUser(registered, 'u-pend-1', 'turn-9');
  check('accepted dispatch binds pending turnId', (accepted.pendingUsers ?? []).some((pending) => pending.id === 'u-pend-1' && pending.turnId === 'turn-9'));
  check('accepted dispatch keeps single optimistic row', accepted.items.filter((item) => item.kind === 'user').length === 1);

  const settled = applySessionEvent(accepted, {
    type: 'message_start',
    sessionId: 'mapped-session',
    itemId: 'user-1',
    turnId: 'turn-9',
    message: { role: 'user', content: [{ type: 'text', text: 'hello runner' }] },
  });
  const userRows = settled.items.filter((item): item is Extract<Item, { kind: 'user' }> => item.kind === 'user');
  check('authoritative user replaces optimistic row', userRows.length === 1
    && userRows[0]?.id === 'user-1'
    && userRows[0]?.optimistic === false, JSON.stringify(settled.items));
  check('authoritative settle drains pending queue', (settled.pendingUsers ?? []).length === 0);
}


// Handled and failed dispatches remove their pending row via removePendingUser,
// leaving sibling pendings untouched.
{
  let state = registerPendingUser(emptyTranscript(), 'first draft', 'u-pend-2');
  state = registerPendingUser(state, 'second draft', 'u-pend-3');

  state = removePendingUser(state, 'u-pend-2');
  check('handled removal drops its pending row', !state.items.some((item) => item.id === 'u-pend-2')
    && state.items.some((item) => item.id === 'u-pend-3'), JSON.stringify(state.items));
  check('handled removal drops its queue entry', (state.pendingUsers ?? []).length === 1
    && (state.pendingUsers ?? [])[0]?.id === 'u-pend-3', JSON.stringify(state.pendingUsers));

  state = acceptPendingUser(state, 'u-pend-3', null);
  check('accept without turnId removes the pending row', !state.items.some((item) => item.id === 'u-pend-3')
    && (state.pendingUsers ?? []).length === 0, JSON.stringify(state.pendingUsers));
}

// Real SessionView DOM proof: runner identity, disconnected composer, and
// the Broadcast composer lock survive rendering. The injected `events`
// subscription is a useEffect contract (static markup cannot run it), so the
// exact-session filter is asserted at the source level.
{
  const html = renderToStaticMarkup(createElement(SessionView, {
    runner: { ...runner, state: 'reconnecting' },
    mappingId: activeMapping.id,
    sessionId: activeMapping.sessionId,
    cwd: project.cwd,
    sendCommand: async () => ({}),
    events: () => () => {},
    onTransportError: () => {},
  }));
  check('session DOM labels the runner', html.includes('Session for Runner A'), html);
  check('session DOM mounts the slash picker', html.includes('id="command-btn"') && html.includes('command-picker'), html);
  check('session DOM stages the transcript for the goal overlay', html.includes('chat-session__stage') && html.includes('chat-session__transcript'), html);
  check('closed session does not mount the goal overlay', !html.includes('chat-goal-overlay') && !html.includes('chat-goal-panel'), html);
  check('disconnected composer textarea stays enabled', /<textarea[^>]*aria-label="Session prompt"/.test(html) && !/<textarea[^>]*disabled=""/.test(html), html);
  check('disconnected composer send disabled', html.includes('<button class="chat-button chat-button--primary" disabled=""'), html);

  const locked = renderToStaticMarkup(createElement(SessionView, {
    runner,
    mappingId: activeMapping.id,
    sessionId: activeMapping.sessionId,
    cwd: project.cwd,
    sendCommand: async () => ({}),
    events: () => () => {},
    onTransportError: () => {},
    composerDisabledReason: 'This runner is responding to the current Broadcast.',
  }));
  check('composer lock keeps the textarea enabled', /<textarea[^>]*aria-label="Session prompt"/.test(locked) && !/<textarea[^>]*disabled=""/.test(locked), locked);
  check('composer lock disables the send button', locked.includes('<button class="chat-button chat-button--primary" disabled=""'), locked);
  check('composer lock renders the reason', locked.includes('This runner is responding to the current Broadcast.'), locked);
  check('composer lock reason is the placeholder', locked.includes('placeholder="This runner is responding to the current Broadcast."'), locked);
}

// A→B→A activity strip (the reported defect): ChatApp keys SessionView by
// [runner, session], so returning to a session whose turn is still running
// mounts a fresh view. The injected session subscription never replays and a
// turn's deltas cannot create a turnId, so the fresh view has to be seeded
// from the authoritative per-session TeamSessionState. Static markup is the
// real first paint of that mount, so an unseeded strip is observable there.
// The props are read the way ChatApp reads them: the ActiveProjectMember
// session of the active binding in the one authoritative team snapshot.
{
  const runnerB: RunnerView = {
    ...runner,
    id: 'runner-b',
    label: 'Runner B',
    origin: 'https://runner-b.example',
    authority: 'runner-b.example',
    probe: { sessionId: 'source-session-b', runtimeIncarnation: 'inc-source-session-b', cwd: '/workspace/runner', sessionCount: 1, runnerCodename: null },
  };
  const mappingA: ProjectRunnerMapping = { ...activeMapping, id: 'mapping-live-a', sessionId: 'session-live-a' };
  const mappingB: ProjectRunnerMapping = {
    ...activeMapping,
    id: 'mapping-live-b',
    runnerId: 'runner-b',
    sessionId: 'session-live-b',
    sessionPath: '/workspace/sessions/live-b.jsonl',
  };
  const mappings = [mappingA, mappingB];
  const views = [runner, runnerB];
  const store = new TeamStore({
    runners: {
      snapshot: () => views,
      command: async (_runnerId, command) => ({ type: 'response', command: command.type, success: true, data: {} }),
    },
  });
  store.reconcileInputs([project], mappings, views);

  // A is the session left behind: its turn keeps running while B is shown.
  store.applyRunnerEvent(runner.id, { type: 'turn_started', sessionId: mappingA.sessionId, turnId: 'turn-live-a', sequence: 1 });

  const sessionOf = (mappingId: string) => {
    const member = activeProjectMembers(project.id, mappings, views, store.snapshot().sessions)
      .find((candidate) => candidate.mapping.id === mappingId);
    if (!member) throw new Error(`missing active member ${mappingId}`);
    return member.session;
  };
  const viewOf = (mappingId: string, sessionId: string): string => {
    const session = sessionOf(mappingId);
    return renderToStaticMarkup(createElement(SessionView, {
      runner: mappingId === mappingA.id ? runner : runnerB,
      mappingId,
      sessionId,
      cwd: project.cwd,
      sendCommand: async () => ({}),
      events: () => () => {},
      onTransportError: () => {},
      activeTurnId: session.activeTurnId,
      isStreaming: session.isStreaming,
    }));
  };

  const liveA = sessionOf(mappingA.id);
  check(
    'authoritative A session carries the running turn',
    liveA.activeTurnId === 'turn-live-a' && liveA.isStreaming === true,
    JSON.stringify({ activeTurnId: liveA.activeTurnId, isStreaming: liveA.isStreaming }),
  );
  const returnedToA = viewOf(mappingA.id, mappingA.sessionId);
  check(
    'remount into the live A turn restores the activity strip',
    returnedToA.includes('id="activity-strip"')
      && returnedToA.includes('data-turn-id="turn-live-a"')
      && returnedToA.includes('data-state="working"')
      && returnedToA.includes('Working…'),
    returnedToA,
  );
  check(
    'restored strip is owned by A, never the session that was displayed',
    returnedToA.includes(`data-session-id="${mappingA.sessionId}"`) && !returnedToA.includes('session-live-b'),
    returnedToA,
  );

  // B stays idle: A's live turn never paints into another session's view.
  const liveB = sessionOf(mappingB.id);
  check(
    'idle B session has no authoritative turn',
    liveB.activeTurnId === null && liveB.isStreaming === false,
    JSON.stringify({ activeTurnId: liveB.activeTurnId, isStreaming: liveB.isStreaming }),
  );
  const shownB = viewOf(mappingB.id, mappingB.sessionId);
  check(
    'no cross-session activity strip while B is displayed',
    !shownB.includes('id="activity-strip"') && !shownB.includes('turn-live-a'),
    shownB,
  );

  // Terminal transition: the authoritative turn is gone, so a remount after
  // it must not resurrect activity.
  store.applyRunnerEvent(runner.id, { type: 'turn_terminal', sessionId: mappingA.sessionId, turnId: 'turn-live-a', sequence: 2 });
  const settledA = sessionOf(mappingA.id);
  check(
    'terminal clears the authoritative A turn',
    settledA.activeTurnId === null && settledA.isStreaming === false,
    JSON.stringify({ activeTurnId: settledA.activeTurnId, isStreaming: settledA.isStreaming }),
  );
  const afterTerminal = viewOf(mappingA.id, mappingA.sessionId);
  check(
    'remount after the terminal shows no stale activity',
    !afterTerminal.includes('id="activity-strip"') && !afterTerminal.includes('turn-live-a'),
    afterTerminal,
  );

  // A newer turn wins over the one that already settled.
  store.applyRunnerEvent(runner.id, { type: 'turn_started', sessionId: mappingA.sessionId, turnId: 'turn-live-a2', sequence: 3 });
  const nextA = viewOf(mappingA.id, mappingA.sessionId);
  check(
    'remount shows the newer authoritative turn, not the settled one',
    nextA.includes('data-turn-id="turn-live-a2"') && !nextA.includes('data-turn-id="turn-live-a"'),
    nextA,
  );
  store.dispose();
}

// The injected events subscription filters to the exact session before
// applying frames (source contract; effects do not run in static markup).
{
  const view = readFileSync(join(process.cwd(), 'src/chat/SessionView.tsx'), 'utf8');
  check('SessionView subscribes through the injected events prop', view.includes('useEffect(() => events((frame) => {'));
  check('SessionView filters injected frames to the exact session', view.includes('if (!sessionEventBelongsTo(frame, sessionId)) return;'));
  check('SessionView no longer imports the runner event registry', !view.includes('subscribeRunnerEvents'));
  check('SessionView imports activityStrip helpers', view.includes("from '../activityStrip'") && view.includes('activityStripView') && view.includes('applyActivityStripEvent') && view.includes('emptyActivityStripState') && view.includes('reconcileActivityStrip'));
  const activityStripId = view.indexOf('id="activity-strip"');
  const steeringQueueTag = view.indexOf('<SteeringQueue');
  check('SessionView mounts activity-strip before SteeringQueue', activityStripId >= 0 && steeringQueueTag >= 0 && activityStripId < steeringQueueTag);
  check('SessionView composer-status-row mounts when strip OR queue', view.includes('{(stripView || queuedSteering.length > 0) && ('));
  check('SessionView increments queue generation on every owned queue_update', view.includes('queueGenRef.current += 1'));
  check('SessionView awaits queue_cancel through settleQueueCommand', view.includes('settleQueueCommand') && view.includes("type: 'queue_cancel'") && view.includes('dropQueuedPreviewAfterCancel'));
  check('SessionView cancel does not dropSteeringAt before send', !view.includes('dropSteeringAt') && !view.includes("void sendCommand({ type: 'queue_cancel'"));
  check('SessionView send-now awaits sendCommand and keeps rows on throw', view.includes("type: 'queue_send_now'") && view.includes('onTransportError(failureMessage(error))'));
  check('SessionView serializes queue mutations with exclusive busy lock', view.includes('queueBusyRef') && view.includes('runExclusiveQueueCommand') && view.includes('busy={queueBusy}'));
  check('SessionView does not hardcode SteeringQueue busy false', !view.includes('busy={false}'));
}

// Turn-level composer streaming: inner message_end is not completion;
// turn_terminal is. Snapshots replace the flag from activeTurnId.
{
  check('planTurnStreaming message_end keeps true', planTurnStreaming('message_end', true) === true);
  check('planTurnStreaming turn_terminal is false', planTurnStreaming('turn_terminal', true) === false);
  check('planTurnStreaming snapshot activeTurnId is live', planTurnStreaming('subscription_snapshot', false, 'turn-1') === true);
  check('planTurnStreaming tool_execution_start is true', planTurnStreaming('tool_execution_start', false) === true);

  const view = readFileSync(join(process.cwd(), 'src/chat/SessionView.tsx'), 'utf8');
  check('SessionView imports planTurnStreaming', view.includes("import { planTurnStreaming } from '../turnStreaming'"));
  check(
    'SessionView does not set turnStreaming false on message_end',
    view.includes('setTurnStreaming((current) => planTurnStreaming(type, current, frame.activeTurnId))')
      && !view.includes('setTurnStreaming(false)'),
  );
}


// SessionView transcript pin (the user-reported blank-middle defect): async
// Markdown/Mermaid hydration mutates layout with no React commit and no
// items change, so the /chat lane must wire MarkdownBody's onLayoutChange
// into a pin controller, and item growth must never write scrollTop
// static markup, so the wiring is asserted at the source level; the pin
// DECISION itself is the shared createScrollPin core, regression-tested in
// scrollPin.test.ts (unpinned freeze + return-to-bottom re-pin).
{
  const view = readFileSync(join(process.cwd(), 'src/chat/SessionView.tsx'), 'utf8');
  check('SessionView reuses the shared pin decision core (createScrollPin)', view.includes('createScrollPin()'), '');
  check('SessionView transcript scroll derives pin state from live geometry', view.includes('onScroll(event.currentTarget)'), '');
  check('SessionView item growth no longer writes scrollTop unconditionally', !view.includes('node.scrollTop = node.scrollHeight'), '');
  check('SessionView items effect pins through the controller', view.includes('useEffect(() => {\n    pinIfPinned();\n  }, [transcript.items, pinIfPinned]);'), '');
  check('SessionView transcript container owns the scroll handler', view.includes('onScroll={onTranscriptScroll}'), '');
  check('SessionView forwards pinIfPinned as the Markdown layout callback', view.includes('onLayoutChange={pinIfPinned}'), '');
  check('TranscriptItem keeps onLayoutChange optional for the Broadcast lane', view.includes('onLayoutChange?: () => void'), '');
  check('SessionView keeps composer focus without scrolling the transcript', view.includes('focus({ preventScroll: true })') && !view.includes('scrollIntoView'), '');
  check('SessionView grows the composer through the shared auto-resize controller', view.includes('createAutoResizeController') && view.includes('maxLines: 3'), '');
  check('SessionView never disables the composer textarea', !view.includes('disabled={!composerEnabled}'), '');
  check('SessionView Escape and Stop abort the in-flight turn', view.includes("event.key === 'Escape'") && view.includes("{ type: 'abort' }") && view.includes('chat-button--stop') && view.includes("'■ Stop'") && view.includes("type: 'get_state'") && view.includes('setOptimisticStopped(true)'), '');
  check('SessionView CommandPicker stays mounted on the composer', view.includes('<CommandPicker') && view.includes('connected={connected}'), '');
}

// Frames arriving after a get_messages request cut survive the authoritative
// merge; pending optimistic rows and live rows are preserved.
{
  const history: Item[] = [{ kind: 'user', id: 'history-1', text: 'old', optimistic: false }];
  const requestCut: Item[] = [];
  const live: Item = { kind: 'assistant', id: 'live-1', status: 'streaming', blocks: [{ type: 'text', text: 'new' }] };
  const pendingState = registerPendingUser({ items: [live], activeAssistantId: 'live-1' }, 'queued prompt', 'u-pend-4');
  const merged = mergeSessionHistory(history, pendingState, requestCut);
  check('history merge keeps restored row', merged.items.some((item) => item.id === 'history-1'));
  check('history merge keeps post-cut live row', merged.items.some((item) => item.id === 'live-1'));
  check('history merge keeps pending optimistic row', merged.items.some((item) => item.id === 'u-pend-4' && item.kind === 'user' && item.optimistic), JSON.stringify(merged.items));
  check('history merge keeps pending queue', (merged.pendingUsers ?? []).some((pending) => pending.id === 'u-pend-4' && pending.text === 'queued prompt'), JSON.stringify(merged.pendingUsers));
}

// Snapshot is data, not ACK: a flagged authoritative cut replaces the
// transcript base, keeps genuinely pending optimistic rows, and lets the
// replayed live events that follow paint on top.
{
  check('authoritative cut flag accepts a stamped cut',
    isAuthoritativeSubscriptionCut({ type: 'subscription_snapshot', authoritative: true, messages: [] }));
  check('flagged resync with messages qualifies',
    isAuthoritativeSubscriptionCut({ type: 'session_resync', authoritative: true, messages: [] }));
  check('reconnect snapshot is never authoritative',
    !isAuthoritativeSubscriptionCut({ type: 'subscription_snapshot', authoritative: false, messages: [] }));
  check('unstamped snapshot is never authoritative',
    !isAuthoritativeSubscriptionCut({ type: 'subscription_snapshot', messages: [] }));
  check('cut without messages has no usable authority',
    !isAuthoritativeSubscriptionCut({ type: 'session_resync', authoritative: true }));
  check('ordinary events are not cuts',
    !isAuthoritativeSubscriptionCut({ type: 'message_start', authoritative: true, messages: [] }));

  // Authoritative replace: the cut is the transcript base; in-flight rows
  // and a genuinely pending optimistic user survive; the replayed events
  // that follow paint on top.
  let state = applySessionEvent(emptyTranscript(), {
    type: 'message_start',
    sessionId: 'mapped-session',
    itemId: 'stale-a',
    message: { role: 'assistant' },
  });
  state = registerPendingUser(state, 'still queued', 'u-pend-5');
  state = applySessionSnapshot(state, [
    { id: 'm1', message: { role: 'user', content: [{ type: 'text', text: 'authoritative user' }] } },
    { id: 'm2', message: { role: 'assistant', content: [{ type: 'text', text: 'authoritative reply' }] } },
  ]);
  check('authoritative cut replaces the transcript base',
    state.items.some((item) => item.id === 'm1' && item.kind === 'user')
      && state.items.some((item) => item.id === 'm2' && item.kind === 'assistant'),
    JSON.stringify(state.items));
  check('authoritative cut keeps in-flight rows like a request cut', state.items.some((item) => item.id === 'stale-a' && item.kind === 'assistant' && item.status === 'streaming'), JSON.stringify(state.items));
  check('authoritative cut preserves genuinely pending rows',
    state.items.some((item) => item.id === 'u-pend-5' && item.kind === 'user' && item.optimistic)
      && (state.pendingUsers ?? []).some((pending) => pending.id === 'u-pend-5'),
    JSON.stringify(state.items));
  const painted = applySessionEvent(state, {
    type: 'message_start',
    sessionId: 'mapped-session',
    itemId: 'live-a',
    message: { role: 'assistant' },
  });
  check('replayed live event paints after the cut',
    painted.items.some((item) => item.id === 'live-a' && item.kind === 'assistant')
      && painted.activeAssistantId === 'live-a'
      && painted.items.some((item) => item.id === 'u-pend-5'),
    JSON.stringify(painted.items));

  // Established pending row confirmed inside the cut collapses onto the
  // authoritative twin: no duplicate user row, queue drained.
  const confirmed = applySessionSnapshot(
    registerPendingUser(emptyTranscript(), 'same prompt', 'u-pend-7'),
    [{ id: 'm3', message: { role: 'user', content: [{ type: 'text', text: 'same prompt' }] } }],
  );
  check('confirmed pending collapses onto its authoritative row',
    confirmed.items.filter((item) => item.kind === 'user').length === 1
      && confirmed.items.some((item) => item.kind === 'user' && item.id === 'm3' && !item.optimistic),
    JSON.stringify(confirmed.items));
  check('confirmed pending drains the queue', (confirmed.pendingUsers ?? []).length === 0, JSON.stringify(confirmed.pendingUsers));

  // Non-authoritative reconnect snapshot: transcript untouched, replay
  // events still apply in order.
  const liveBase = applySessionEvent(emptyTranscript(), {
    type: 'message_start',
    sessionId: 'mapped-session',
    itemId: 'live-b',
    message: { role: 'assistant' },
  });
  const kept = applySessionEvent(liveBase, {
    type: 'subscription_snapshot',
    sessionId: 'mapped-session',
    watermark: 9,
    authoritative: false,
    messages: [{ message: { role: 'user', content: [{ type: 'text', text: 'reconnect base' }] } }],
  } as unknown as EventFrame);
  check('non-authoritative snapshot keeps current transcript', kept.items.length === 1
    && kept.items[0]?.id === 'live-b'
    && !kept.items.some((item) => item.kind === 'user'), JSON.stringify(kept.items));
  const replayed = applySessionEvent(kept, {
    type: 'message_start',
    sessionId: 'mapped-session',
    itemId: 'live-c',
    message: { role: 'assistant' },
  });
  check('replay events apply in order after reconnect keep', replayed.items.some((item) => item.id === 'live-b')
    && replayed.items.some((item) => item.id === 'live-c'), JSON.stringify(replayed.items));
}

// /chat Markdown path clicks inherit the exact SessionView owner
// (`data-session-id` + `data-workspace-cwd`). Missing owner stays inert;
// another session is never substituted; App's one subscriber is not doubled.
{
  const cwd = project.cwd;
  const sessionId = activeMapping.sessionId;
  const otherSession = 'other-session';

  const userItem: Item = {
    kind: 'user',
    id: 'user-md',
    text: 'see `src/lib.rs:3` and /workspace/project-a/long.txt',
    optimistic: false,
  };
  const assistantItem: Item = {
    kind: 'assistant',
    id: 'assistant-md',
    status: 'final',
    blocks: [
      { type: 'thinking', thinking: 'check `src/think.ts:1`' },
      { type: 'text', text: 'open `src/app.ts:8` and /workspace/project-a/long.txt' },
    ],
  };
  const customItem: Item = { kind: 'custom', id: 'custom-md', label: 'Notice', text: 'custom `src/custom.ts`' };
  const summaryItem: Item = { kind: 'summary', id: 'summary-md', label: 'Summary', text: 'summary `src/summary.ts`' };
  const ircItem: Item = {
    kind: 'irc',
    id: 'irc-md',
    direction: 'incoming',
    from: 'child',
    to: 'Main',
    body: 'irc `src/irc.ts`',
  };
  const toolItem: Item = {
    kind: 'toolCard',
    id: 'tool-md',
    toolCallId: 'call-md',
    toolName: 'read',
    args: { path: 'src/secret.ts' },
    status: 'done',
    result: 'src/secret.ts:1',
  };

  function renderItem(item: Item, ownerSession = sessionId, ownerCwd = cwd): string {
    return renderToStaticMarkup(createElement(TranscriptItem, { item, sessionId: ownerSession, cwd: ownerCwd }));
  }

  function ownerAttrs(html: string): { sessionId: string | undefined; cwd: string | undefined; count: number } {
    const sessionMatches = [...html.matchAll(/data-session-id="([^"]*)"/g)].map((match) => match[1]);
    const cwdMatches = [...html.matchAll(/data-workspace-cwd="([^"]*)"/g)].map((match) => match[1]);
    return {
      sessionId: sessionMatches[0],
      cwd: cwdMatches[0],
      count: sessionMatches.length,
    };
  }

  const userHtml = renderItem(userItem);
  const assistantHtml = renderItem(assistantItem);
  const customHtml = renderItem(customItem);
  const summaryHtml = renderItem(summaryItem);
  const ircHtml = renderItem(ircItem);
  const toolHtml = renderItem(toolItem);

  for (const [name, html] of [
    ['user', userHtml],
    ['assistant', assistantHtml],
    ['custom', customHtml],
    ['summary', summaryHtml],
    ['irc', ircHtml],
  ] as const) {
    const owner = ownerAttrs(html);
    check(`${name} markdown has exactly one session owner`, owner.count === 1 && owner.sessionId === sessionId, html);
    check(`${name} markdown exposes workspace cwd`, owner.cwd === cwd, html);
  }

  check('assistant thinking markdown inherits the same owner', assistantHtml.includes('chat-session-thinking') && assistantHtml.includes('src/think.ts:1') && ownerAttrs(assistantHtml).sessionId === sessionId, assistantHtml);
  check('assistant text markdown inherits the same owner', assistantHtml.includes('src/app.ts:8') && assistantHtml.includes('chat-session-markdown'), assistantHtml);
  check('assistant [thinking, text] paints thinking before answer',
    assistantHtml.indexOf('chat-session-thinking') < assistantHtml.indexOf('chat-session-markdown'),
    assistantHtml);
  const reversedAssistant = renderItem({
    ...assistantItem,
    id: 'assistant-text-then-thinking',
    blocks: [
      { type: 'text', text: 'visible answer' },
      { type: 'thinking', thinking: 'chain of thought here' },
    ],
  });
  check('assistant [text, thinking] paints thinking before answer',
    reversedAssistant.indexOf('chat-session-thinking') < reversedAssistant.indexOf('chat-session-markdown')
      && reversedAssistant.indexOf('chain of thought here') < reversedAssistant.indexOf('visible answer'),
    reversedAssistant);
  const thinkingOnly = renderItem({
    kind: 'assistant',
    id: 'assistant-thinking-only',
    status: 'final',
    blocks: [{ type: 'thinking', thinking: 'only thought' }],
  });
  check('thinking-only stays a thinking fold',
    thinkingOnly.includes('chat-session-thinking')
      && thinkingOnly.includes('only thought')
      && !thinkingOnly.includes('chat-session-markdown'),
    thinkingOnly);
  check('assistant stamps final status', assistantHtml.includes('data-status="final"'), assistantHtml);
  check('assistant stamps streaming status', renderItem({ ...assistantItem, status: 'streaming' }).includes('data-status="streaming"'));
  check('relative code stays clickable', userHtml.includes('md-code-path') && userHtml.includes('data-md-path="src/lib.rs"') && userHtml.includes('src/lib.rs:3'), userHtml);
  check('user markup renders bare absolute path as plain prose', !userHtml.includes('md-bare-abs') && userHtml.includes('/workspace/project-a/long.txt'), userHtml);
  check('tool cards reuse /web ToolCard with session owner', toolHtml.includes('data-session-id') && toolHtml.includes('tool-card') && toolHtml.includes('<pre'), toolHtml);


  function fakeTarget(opts: {
    html?: string;
    sessionId?: string | null;
    cwd?: string | null;
    kind: 'code';
    text: string;
  }) {
    const parsed = opts.html ? ownerAttrs(opts.html) : { sessionId: undefined, cwd: undefined, count: 0 };
    const session = opts.sessionId === undefined ? parsed.sessionId : (opts.sessionId ?? undefined);
    const workspace = opts.cwd === undefined ? parsed.cwd : (opts.cwd ?? undefined);
    const owner = session || workspace
      ? {
          textContent: null as string | null,
          getAttribute(name: string): string | null {
            if (name === 'data-session-id') return session ?? null;
            if (name === 'data-workspace-cwd') return workspace ?? null;
            return null;
          },
          closest(): unknown { return null; },
        }
      : null;
    const node = {
      textContent: opts.text,
      getAttribute(name: string): string | null {
        if (name === 'href') return null;
        return null;
      },
      closest(sel: string): unknown {
        if (sel === '[data-session-id]') return session ? owner : null;
        if (sel === '[data-workspace-cwd]') return workspace ? owner : null;
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

  const opened: Array<{ path: string; line?: number; sessionId?: string; workspaceCwd?: string }> = [];
  const appHandler = (path: string, line: number | undefined, owner: { sessionId: string; workspaceCwd?: string }) => {
    const sid = owner.sessionId?.trim();
    if (!sid) return;
    opened.push({ path, line, sessionId: sid, workspaceCwd: owner.workspaceCwd });
  };
  const unsubA = subscribeMarkdownOpenPath(appHandler);
  const unsubB = subscribeMarkdownOpenPath(appHandler);

  opened.length = 0;
  const codeEvent = click(fakeTarget({ html: userHtml, kind: 'code', text: 'src/lib.rs:3' }));
  check('relative code under SessionView owner is handled', dispatchMarkdownPathClick(codeEvent) === true);
  check(
    'relative code opens the exact mapped session',
    opened.length === 1
      && opened[0]?.path === 'src/lib.rs'
      && opened[0]?.line === 3
      && opened[0]?.sessionId === sessionId
      && opened[0]?.workspaceCwd === cwd,
    JSON.stringify(opened),
  );
  check('relative code owner cwd is on the rendered ancestor', ownerAttrs(userHtml).cwd === cwd);


  opened.length = 0;
  const thinkingEvent = click(fakeTarget({ html: assistantHtml, kind: 'code', text: 'src/think.ts:1' }));
  check(
    'assistant thinking click keeps the same owner session',
    dispatchMarkdownPathClick(thinkingEvent) === true
      && opened.length === 1
      && opened[0]?.sessionId === sessionId
      && opened[0]?.path === 'src/think.ts'
      && opened[0]?.workspaceCwd === cwd,
    JSON.stringify(opened),
  );

  opened.length = 0;
  const otherEvent = click(fakeTarget({ html: userHtml, sessionId: otherSession, cwd, kind: 'code', text: 'src/lib.rs:3' }));
  check(
    'other session is not substituted for the rendered owner',
    dispatchMarkdownPathClick(otherEvent) === true
      && opened.length === 1
      && opened[0]?.sessionId === otherSession
      && opened[0]?.sessionId !== sessionId,
    JSON.stringify(opened),
  );

  opened.length = 0;
  const missingOwner = click(fakeTarget({ sessionId: null, cwd: null, kind: 'code', text: 'src/lib.rs:3' }));
  check(
    'missing owner stays inert for the App subscriber',
    dispatchMarkdownPathClick(missingOwner) === false && opened.length === 0,
    JSON.stringify(opened),
  );

  opened.length = 0;
  const extraSame = click(fakeTarget({ html: userHtml, kind: 'code', text: 'src/lib.rs:3' }));
  check(
    'ref-counted App subscriber does not double-open the path',
    dispatchMarkdownPathClick(extraSame, appHandler) === true
      && opened.length === 1
      && opened[0]?.sessionId === sessionId,
    JSON.stringify(opened),
  );

  unsubA();
  unsubB();
  opened.length = 0;
  const afterUnsub = click(fakeTarget({ html: userHtml, kind: 'code', text: 'src/lib.rs:3' }));
  check('unsubscribed App handler no longer receives clicks', dispatchMarkdownPathClick(afterUnsub) === false && opened.length === 0, JSON.stringify(opened));
}

{
  const jpegChip: ComposerAttachment = {
    id: 'chip-jpeg',
    name: 'shot.png',
    size: 4,
    mimeType: 'image/jpeg',
    kind: 'image',
    dataBase64: btoa(String.fromCharCode(0xff, 0xd8, 0xff, 0xe0)),
  };
  const videoChip: ComposerAttachment = {
    id: 'chip-video',
    name: 'clip.mp4',
    size: 4,
    mimeType: 'video/mp4',
    kind: 'video',
  };
  const sessionProps = {
    runner,
    mappingId: activeMapping.id,
    sessionId: activeMapping.sessionId,
    cwd: project.cwd,
    sendCommand: async () => ({}),
    events: () => () => {},
    onTransportError: () => {},
  };
  const imageHtml = renderToStaticMarkup(createElement(SessionView, {
    ...sessionProps,
    initialAttachments: [jpegChip],
  }));
  check(
    'chat composer image chip exposes send-to-terminal',
    imageHtml.includes('chat-session__composer-attachments')
      && imageHtml.includes('composer-attachment__tmux')
      && imageHtml.includes('aria-label="Send shot.png to terminal"')
      && imageHtml.includes('title="Send to terminal"'),
    imageHtml,
  );
  const videoHtml = renderToStaticMarkup(createElement(SessionView, {
    ...sessionProps,
    initialAttachments: [videoChip],
  }));
  check(
    'chat composer video chip has no send-to-terminal',
    videoHtml.includes('chat-session__composer-attachments')
      && videoHtml.includes('clip.mp4')
      && !videoHtml.includes('composer-attachment__tmux')
      && !videoHtml.includes('Send clip.mp4 to terminal'),
    videoHtml,
  );

  const store = new Map<string, string>();
  Object.defineProperty(globalThis, 'sessionStorage', {
    configurable: true,
    value: {
      getItem(key: string) {
        return store.has(key) ? store.get(key)! : null;
      },
      setItem(key: string, value: string) {
        store.set(key, String(value));
      },
      removeItem(key: string) {
        store.delete(key);
      },
      clear() {
        store.clear();
      },
    },
  });
  rememberTmuxFileDestination({
    runnerId: 'runner-overlay',
    session: 'agentlo-pi-rs',
    authority: '192.0.2.10:8765',
  });
  const tokenByAuthority: Record<string, string> = {
    '192.0.2.10:8765': 'tok-overlay',
    'runner-a.example': 'tok-page',
  };
  const calls: Array<{ url: string; init: RequestInit }> = [];
  const sent = await sendChatSessionAttachmentToTmux(jpegChip, {
    loadToken: (authority) => tokenByAuthority[authority] ?? '',
    pageProtocol: 'https:',
    fetchImpl: (async (url: string, init?: RequestInit) => {
      calls.push({ url, init: init ?? {} });
      return {
        status: 200,
        json: async () => ({ path: '/tmp/rpi-tmux-file-1-shot.jpg' }),
      } as Response;
    }) as typeof fetch,
  });
  check(
    'chat chip upload posts to overlay authority not the page host',
    calls.length === 1
      && calls[0]?.url === 'https://192.0.2.10:8765/upload/tmux-file'
      && String((calls[0]?.init.headers as Record<string, string> | undefined)?.['X-Tmux-Session']) === encodeURIComponent('agentlo-pi-rs')
      && String((calls[0]?.init.headers as Record<string, string> | undefined)?.Authorization) === 'Bearer tok-overlay',
    JSON.stringify(calls),
  );
  check(
    'chat chip upload uses overlay JPEG name and toasts the path',
    sent.ok === true && sent.ok && sent.path.endsWith('shot.jpg'),
  );

  rememberTmuxFileDestination(null);
  const missingCalls: string[] = [];
  const missing = await sendChatSessionAttachmentToTmux(jpegChip, {
    loadToken: () => 'tok-page',
    pageProtocol: 'https:',
    fetchImpl: (async (url: string) => {
      missingCalls.push(url);
      throw new Error('must not fetch');
    }) as typeof fetch,
  });
  check(
    'chat chip send without overlay destination does not POST',
    missing.ok === false && missing.message.includes('Open a tmux overlay') && missingCalls.length === 0,
    JSON.stringify({ missing, missingCalls }),
  );

  const here = dirname(fileURLToPath(import.meta.url));
  const chatCss = readFileSync(join(here, '../src/chat/styles.css'), 'utf8');
  const phoneBlockStart = chatCss.indexOf('@media (max-width: 719px)');
  const tmuxRule = '.chat-session__composer-attachments .composer-attachment__tmux';
  const desktopTmux = chatCss.indexOf(tmuxRule);
  const phoneTmux = phoneBlockStart >= 0 ? chatCss.indexOf(tmuxRule, phoneBlockStart) : -1;
  check(
    'chat chip tmux control is 20px in the base composer',
    desktopTmux >= 0
      && desktopTmux < phoneBlockStart
      && /width:\s*20px/.test(chatCss.slice(desktopTmux, desktopTmux + 180))
      && /height:\s*20px/.test(chatCss.slice(desktopTmux, desktopTmux + 180)),
    `desktop=${desktopTmux} phoneMedia=${phoneBlockStart}`,
  );
  check(
    'chat chip tmux control stays 20px with a 44px tap pad at 390px',
    phoneTmux >= 0
      && /width:\s*20px/.test(chatCss.slice(phoneTmux, phoneTmux + 220))
      && /padding:\s*12px/.test(chatCss.slice(phoneTmux, phoneTmux + 220)),
    `phoneTmux=${phoneTmux}`,
  );
}

// Mount effects against the real non-replaying TeamStore subscription. Resolve
// React and react-test-renderer from the same installation via esbuild aliases.
{
  const previousWindow = Object.getOwnPropertyDescriptor(globalThis, 'window');
  Object.defineProperty(globalThis, 'window', {
    configurable: true,
    value: {
      setTimeout: globalThis.setTimeout,
      clearTimeout: globalThis.clearTimeout,
      getComputedStyle: () => ({ lineHeight: '20px', paddingTop: '0px', paddingBottom: '0px' }),
    },
  });
  try {
    for (const schedule of ['idle', 'replacement', 'live-tool'] as const) {
      const mapping = { ...activeMapping, id: `handoff-${schedule}`, sessionId: `session-${schedule}` };
      const store = new TeamStore({
        runners: {
          snapshot: () => [runner],
          command: async (_runnerId, command) => ({ type: 'response', command: command.type, success: true, data: {} }),
        },
      });
      let mounted: ReactTestRenderer | undefined;
      let sequence = 0;
      const emit = (type: string, turnId: string, detail: Record<string, unknown> = {}) => {
        store.applyRunnerEvent(runner.id, { ...detail, type, turnId, sessionId: mapping.sessionId, sequence: ++sequence });
      };
      const readAuthority = () => {
        const session = store.snapshot().sessions[runnerSessionKey(runner.id, mapping.sessionId)];
        if (!session || session.mappingId !== mapping.id) return null;
        return { activeTurnId: session.activeTurnId, isStreaming: session.isStreaming };
      };
      const props = {
        runner,
        mappingId: mapping.id,
        sessionId: mapping.sessionId,
        cwd: project.cwd,
        sendCommand: async () => ({ messages: [] }),
        events: (listener: (frame: unknown) => void) => store.subscribeSession(runner.id, mapping.sessionId, listener),
        readAuthority,
        onTransportError: (message: string) => {
          if (message) throw new Error(`${schedule}: unexpected transport error: ${message}`);
        },
      };
      const strips = () => mounted!.root.findAllByProps({ id: 'activity-strip' });
      const checkStrip = (name: string, turnId: string, state: string, text: string, toolCallId?: string) => {
        const nodes = strips();
        const strip = nodes[0];
        check(name, nodes.length === 1
          && strip.props['data-session-id'] === mapping.sessionId
          && strip.props['data-turn-id'] === turnId
          && strip.props['data-state'] === state
          && strip.props['data-tool-call-id'] === toolCallId
          && strip.findByProps({ className: 'activity-strip__text' }).children.join('') === text,
        JSON.stringify(mounted!.toJSON()));
      };
      try {
        store.reconcileInputs([project], [mapping], [runner]);
        emit('turn_started', 'turn-t');
        const staleT = readAuthority()!;
        if (schedule !== 'live-tool') {
          // No view listener exists yet: these frames cannot be replayed to it.
          emit('turn_terminal', 'turn-t');
          if (schedule === 'replacement') emit('turn_started', 'turn-u');
        }
        await act(async () => {
          mounted = create(createElement(SessionView, { ...props, ...staleT }), {
            createNodeMock: () => ({
              scrollTop: 0, scrollHeight: 0, clientHeight: 0,
              value: '', style: {}, focus() {},
            }),
          });
        });
        check(`${schedule}: mount has no transcript load error`,
          mounted!.root.findAllByProps({ className: 'chat-session__status chat-session__status--error' }).length === 0);
        if (schedule === 'idle') {
          check('handoff clears stale initial T when authority is idle', strips().length === 0, JSON.stringify(mounted!.toJSON()));
        } else if (schedule === 'replacement') {
          checkStrip('handoff replaces stale initial T with authority U', 'turn-u', 'working', 'Working…');
        } else {
          checkStrip('mounted live schedule starts on T', 'turn-t', 'working', 'Working…');
          await act(async () => {
            emit('turn_terminal', 'turn-t');
            emit('turn_started', 'turn-u');
          });
          // Commit U once so the later lagging T actually changes prop values;
          // rerendering identical T props would miss a prop-driven regression.
          await act(async () => {
            mounted!.update(createElement(SessionView, { ...props, ...readAuthority()! }));
          });
          await act(async () => {
            emit('tool_execution_start', 'turn-u', { itemId: 'item-u', toolCallId: 'tool-u', toolName: 'read', args: { path: '/tmp/handoff' } });
          });
          checkStrip('live U tool reaches the mounted activity strip', 'turn-u', 'tool', 'Running read…', 'tool-u');
          await act(async () => {
            mounted!.update(createElement(SessionView, { ...props, ...staleT }));
          });
          checkStrip('stale T props cannot replace live U or erase its tool', 'turn-u', 'tool', 'Running read…', 'tool-u');
        }
      } finally {
        try {
          if (mounted) await act(async () => mounted!.unmount());
        } finally {
          store.dispose();
        }
      }
    }
  } finally {
    if (previousWindow) Object.defineProperty(globalThis, 'window', previousWindow);
    else Reflect.deleteProperty(globalThis, 'window');
  }
}

console.log(`\nchatSessionView.test: ${assertions} assertions, ${failures.length} failure(s)`);
if (failures.length > 0) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);