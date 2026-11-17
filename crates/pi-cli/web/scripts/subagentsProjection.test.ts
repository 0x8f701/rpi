#!/usr/bin/env node
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server.browser';
import {
  applyAuthoritativeOrchestrationSnapshot,
  applyOrchestrationEvent,
  beginOrchestrationRefresh,
  failOrchestrationRefresh,
  initialOrchestrationProjection,
  MAX_ORCHESTRATION_EVENT_BUFFER,
  normalizedEventGroup,
} from '../src/orchestrationProjection.ts';
import { deriveProjectAggregate, runnerActivityJobs, type TeamSessionState } from '../src/chat/team.ts';
import { projectAgentHistory } from '../src/panels/SubagentsPanel.tsx';
import { renderSubagentHistory } from '../src/App.tsx';
import { dispatchMarkdownPathClick } from '../src/markdown.ts';


const failures: string[] = [];
let ran = 0;
function check(name: string, condition: boolean, detail = '') {
  ran += 1;
  if (!condition) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

function job(id: string, agent: string) {
  return {
    id,
    agentId: `${agent}-agent`,
    agent,
    parentId: 'Main',
    description: `${agent} live task`,
    status: 'running' as const,
    createdAt: 1,
  };
}

function agent(id: string, status: string) {
  return {
    id,
    displayName: id,
    agent: id.replace('-agent', ''),
    parentId: 'Main',
    status,
    createdAt: 1,
    lastActivity: 2,
    unread: 0,
  };
}

function message(id: string, from: string, body: string) {
  return { id, from, to: 'Main', body, timestamp: 3 };
}

function initialize(groupId = 'current-group') {
  let state = beginOrchestrationRefresh(initialOrchestrationProjection(), 1);
  state = applyAuthoritativeOrchestrationSnapshot(state, 1, {
    groupId,
    jobs: [job('gpt-live', 'gpt')],
    agents: [agent('gpt-agent', 'running')],
    messages: [],
  });
  return state;
}

check(
  'snake-case group is preferred',
  normalizedEventGroup({ type: 'job_updated', group_id: 'snake', groupId: 'camel' }) === 'snake',
);
check(
  'camel-case group fallback is retained',
  normalizedEventGroup({ type: 'job_updated', groupId: 'camel' }) === 'camel',
);

{
  let state = beginOrchestrationRefresh(initialOrchestrationProjection(), 1);
  state = applyOrchestrationEvent(state, {
    type: 'job_updated',
    groupId: 'old-group',
    job: job('stale-glm', 'glm'),
  });
  state = applyAuthoritativeOrchestrationSnapshot(state, 1, {
    groupId: 'current-group',
    jobs: [job('gpt-live', 'gpt'), job('glm-live', 'glm')],
    agents: [],
    messages: [],
  });
  state = applyOrchestrationEvent(state, {
    type: 'job_updated',
    groupId: 'old-group',
    job: job('stale-glm', 'glm'),
  });
  check(
    'authoritative list drops buffered and late previous-group jobs',
    Object.keys(state.jobs).sort().join(',') === 'glm-live,gpt-live',
    JSON.stringify(state),
  );
  check(
    'live foreign-group event after initialization marks one repair',
    state.needsRefresh === true && !('stale-glm' in state.jobs),
    JSON.stringify(state),
  );
}

{
  let state = initialize();
  state = beginOrchestrationRefresh(state, 2);
  state = applyOrchestrationEvent(state, {
    type: 'job_updated',
    groupId: 'current-group',
    job: { ...job('gpt-live', 'gpt'), status: 'completed' },
  });
  state = applyOrchestrationEvent(state, {
    type: 'job_updated',
    groupId: 'current-group',
    job: job('glm-spawned-after-capture', 'glm'),
  });
  state = applyOrchestrationEvent(state, {
    type: 'agent_updated',
    groupId: 'current-group',
    agent: agent('gpt-agent', 'completed'),
  });
  state = applyOrchestrationEvent(state, {
    type: 'message_delivered',
    groupId: 'current-group',
    message: message('message-after-capture', 'gpt-agent', 'settled after capture'),
  });
  state = applyOrchestrationEvent(state, {
    type: 'job_updated',
    groupId: 'foreign-group',
    job: job('foreign-after-capture', 'writer'),
  });
  state = applyAuthoritativeOrchestrationSnapshot(state, 2, {
    groupId: 'current-group',
    jobs: [job('gpt-live', 'gpt')],
    agents: [agent('gpt-agent', 'running')],
    messages: [],
  });
  check(
    'stale snapshot cannot regress completed job to running',
    state.jobs['gpt-live']?.job.status === 'completed',
    JSON.stringify(state.jobs['gpt-live']),
  );
  check(
    'job spawned after snapshot capture survives response delivery',
    state.jobs['glm-spawned-after-capture']?.job.status === 'running',
    JSON.stringify(state.jobs),
  );
  check(
    'agent update after snapshot capture survives response delivery',
    state.agents['gpt-agent']?.status === 'completed',
    JSON.stringify(state.agents),
  );
  check(
    'message after snapshot capture survives and updates activity',
    state.messages.some((entry) => entry.id === 'message-after-capture')
      && state.jobs['gpt-live']?.activity === 'settled after capture',
    JSON.stringify(state),
  );
  check(
    'foreign event during initialized refresh remains rejected',
    !('foreign-after-capture' in state.jobs),
    JSON.stringify(state.jobs),
  );
}

{
  let state = initialize();
  state = applyOrchestrationEvent(state, {
    type: 'message_delivered',
    groupId: 'current-group',
    message: message('progress-1', 'gpt-agent', 'progress'),
  });
  state = beginOrchestrationRefresh(state, 2);
  state = applyOrchestrationEvent(state, {
    type: 'job_updated',
    groupId: 'current-group',
    job: { ...job('gpt-live', 'gpt'), status: 'running' },
  });
  const retained = applyAuthoritativeOrchestrationSnapshot(state, 2, {
    groupId: 'current-group',
    jobs: [job('gpt-live', 'gpt')],
    agents: [agent('gpt-agent', 'running')],
    messages: [],
  });
  check(
    'message activity attaches by agent and survives snapshot replacement',
    retained.jobs['gpt-live']?.activity === 'progress' && retained.jobs['gpt-live']?.activityAt === 3,
    JSON.stringify(retained.jobs['gpt-live']),
  );
}

{
  let state = beginOrchestrationRefresh(initialOrchestrationProjection(), 1);
  state = applyAuthoritativeOrchestrationSnapshot(state, 1, {
    groupId: 'current-group',
    enabled: true,
    jobs: [
      job('running-job', 'gpt'),
      { ...job('draining-job', 'gpt'), id: 'draining-job', status: 'cancelling' as const },
    ],
    agents: [agent('gpt-agent', 'running')],
    messages: [],
    catalog: [],
  });
  state = applyOrchestrationEvent(state, {
    type: 'message_delivered',
    groupId: 'current-group',
    message: message('drain-progress', 'gpt-agent', 'still draining'),
  });
  check(
    'cancelling outranks running for the same agent activity',
    state.jobs['draining-job']?.activity === 'still draining'
      && state.jobs['running-job']?.activity == null,
    JSON.stringify(state.jobs),
  );
}

{
  const jobView = (id: string, status: 'running' | 'cancelling' | 'cancelled') => ({
    job: { ...job(id, 'gpt'), id, status, createdAt: status === 'cancelling' ? 9 : 1 },
    activity: null,
    activityAt: null,
  });
  const session: TeamSessionState = {
    key: 'runner/session',
    mappingId: 'map-1',
    projectId: 'project-1',
    runnerId: 'runner',
    sessionId: 'session',
    sessionPath: '/session',
    codename: null,
    connection: 'connected',
    lastSequence: 0,
    activeTurnId: null,
    isStreaming: false,
    orchestrationEnabled: true,
    bootstrapped: true,
    refreshError: '',
    goalError: '',
    goal: null,
    orchestration: {
      ...initialOrchestrationProjection(),
      groupId: 'current-group',
      jobs: {
        'running-job': jobView('running-job', 'running'),
        'draining-job': jobView('draining-job', 'cancelling'),
        'done-job': jobView('done-job', 'cancelled'),
      },
    },
  };
  const aggregate = deriveProjectAggregate('project-1', { 'runner/session': session });
  check(
    'cancelling counts active and is not aliased to running or cancelled',
    aggregate.activeJobCount === 2
      && aggregate.jobCounts.cancelling === 1
      && aggregate.jobCounts.running === 1
      && aggregate.jobCounts.cancelled === 1,
    JSON.stringify(aggregate.jobCounts),
  );
  const activity = runnerActivityJobs(session);
  check(
    'cancelling stays in the live activity prefix',
    activity[0]?.status === 'cancelling' && activity[2]?.status === 'cancelled',
    JSON.stringify(activity.map((entry) => entry.status)),
  );
}

{
  let state = initialize();
  state = beginOrchestrationRefresh(state, 2);
  state = applyOrchestrationEvent(state, {
    type: 'job_updated',
    groupId: 'current-group',
    job: { ...job('gpt-live', 'gpt'), status: 'completed' },
  });
  state = applyOrchestrationEvent(state, {
    type: 'job_updated',
    groupId: 'foreign-group',
    job: job('foreign-overlap', 'writer'),
  });
  state = beginOrchestrationRefresh(state, 3);
  const before = state;
  state = applyAuthoritativeOrchestrationSnapshot(state, 2, {
    groupId: 'current-group',
    jobs: [job('stale-response', 'writer')],
    agents: [],
    messages: [],
  });
  check('out-of-order snapshot response is ignored', state === before, JSON.stringify(state));
  state = applyAuthoritativeOrchestrationSnapshot(state, 3, {
    groupId: 'current-group',
    jobs: [job('gpt-live', 'gpt')],
    agents: [agent('gpt-agent', 'running')],
    messages: [],
  });
  check(
    'same-group event before overlapping request survives newest snapshot',
    state.jobs['gpt-live']?.job.status === 'completed',
    JSON.stringify(state.jobs),
  );
  check(
    'foreign-group event before overlapping request remains rejected',
    !('foreign-overlap' in state.jobs),
    JSON.stringify(state.jobs),
  );
}

{
  let state = initialize();
  state = beginOrchestrationRefresh(state, 2);
  state = applyOrchestrationEvent(state, {
    type: 'job_updated',
    groupId: 'current-group',
    job: { ...job('gpt-live', 'gpt'), status: 'completed' },
  });
  state = failOrchestrationRefresh(state, 2);
  check(
    'failed snapshot request replays buffered current-group update',
    state.jobs['gpt-live']?.job.status === 'completed' && state.requestId === null,
    JSON.stringify(state),
  );
}

{
  let state = beginOrchestrationRefresh(initialOrchestrationProjection(), 1);
  state = applyAuthoritativeOrchestrationSnapshot(state, 1, {
    groupId: '',
    jobs: [job('disabled-job', 'gpt')],
    agents: [agent('gpt-agent', 'running')],
    messages: [],
  });
  state = applyOrchestrationEvent(state, {
    type: 'job_updated',
    job: job('groupless-job', 'glm'),
  });
  check(
    'empty authoritative group remains uninitialized and rejects groupless events',
    state.groupId === null && Object.keys(state.jobs).length === 0,
    JSON.stringify(state),
  );
}

{
  let projection = beginOrchestrationRefresh(initialOrchestrationProjection(), 1);
  for (let index = 0; index < MAX_ORCHESTRATION_EVENT_BUFFER + 1; index += 1) {
    projection = applyOrchestrationEvent(projection, {
      type: 'job_updated',
      groupId: 'overflow-group',
      job: job(`overflow-${index}`, 'gpt'),
    });
  }
  check(
    'buffer is capped and marks one repair',
    projection.buffered.length === 128 && projection.needsRefresh,
    JSON.stringify({ length: projection.buffered.length, needsRefresh: projection.needsRefresh }),
  );
  check(
    'clearing repair at request start prevents a duplicate follow-up',
    beginOrchestrationRefresh(projection, 9).needsRefresh === false,
  );
}

// ---- agent_history wire → projectAgentHistory validation ------------------
const trunc = (reason = 'none', omittedRows = 0) => ({ reason, omittedRows });
const wire = (messages: unknown, agentId = 'child-agent', truncation: unknown = trunc()) => ({
  agentId,
  messages,
  truncation,
});
const rejection = (data: unknown, agentId = 'child-agent'): string => {
  try {
    projectAgentHistory(data, agentId);
  } catch (error) {
    return error instanceof Error ? error.message : String(error);
  }
  return '';
};

check('null payload is rejected', rejection(null).includes('returned history for nothing'));
check('array payload is rejected', rejection([]).includes('returned history for nothing'));
check(
  'history for a different agent is never rendered for this child',
  rejection(wire([], 'other-agent')).includes('returned history for other-agent'),
);
check('missing messages array is rejected', rejection({ agentId: 'child-agent', truncation: trunc() }).includes('no messages array'));
check('missing truncation is rejected', rejection({ agentId: 'child-agent', messages: [] }).includes('no truncation'));
check(
  'non-integer omittedRows is rejected',
  rejection(wire([], 'child-agent', trunc('limit', 1.5))).includes('invalid omittedRows'),
);
check(
  'negative omittedRows is rejected',
  rejection(wire([], 'child-agent', trunc('bytes', -1))).includes('invalid omittedRows'),
);
check(
  'unknown truncation reason is rejected',
  rejection(wire([], 'child-agent', trunc('mystery', 3))).includes('invalid truncation reason'),
);
check(
  'untruncated payload that still reports omitted rows is rejected',
  rejection(wire([], 'child-agent', trunc('none', 2))).includes('untruncated but omitted rows'),
);

{
  const history = projectAgentHistory(
    wire(
      [
        { id: 'u-1', message: { role: 'user', content: [{ type: 'text', text: 'steer the child' }] } },
        {
          id: 'a-1',
          message: {
            role: 'assistant',
            content: [
              { type: 'thinking', thinking: 'plan first' },
              { type: 'text', text: 'see docs/report.md for context' },
              { type: 'toolCall', id: 'call-1', name: 'bash', arguments: { command: 'rg -n needle src/' } },
              { type: 'toolCall', id: 'call-2', name: 'read', arguments: { path: 'src/lib.rs' } },
            ],
          },
        },
        { message: { role: 'toolResult', toolCallId: 'call-2', content: [{ type: 'text', text: 'fn lib() {}' }], isError: false } },
        { message: { role: 'toolResult', toolCallId: 'call-1', content: [{ type: 'text', text: 'src/x.rs:1:needle' }], isError: false } },
        { message: { role: 'bashExecution', command: 'make verify', output: 'ok', exitCode: 0 } },
        { message: { role: 'toolResult', toolCallId: 'orphan', content: [{ type: 'text', text: '' }], isError: true } },
      ],
      'child-agent',
      trunc('limit_and_bytes', 4),
    ),
    'child-agent',
  );
  check('truncation is carried through verbatim',
    history.truncation.reason === 'limit_and_bytes' && history.truncation.omittedRows === 4,
    JSON.stringify(history.truncation));
  const kinds = history.items.map((item) => item.kind).join(',');
  check('user/assistant/toolCard/bash/toolResult project as items',
    kinds === 'user,assistant,toolCard,toolCard,bash,toolResult', kinds);
  const assistant = history.items.find((item) => item.kind === 'assistant');
  check('assistant keeps stable entry id and thinking block',
    assistant?.kind === 'assistant'
      && assistant.entryId === 'a-1'
      && assistant.blocks.some((block) => block.type === 'thinking' && block.thinking === 'plan first'),
    JSON.stringify(assistant));
  const cards = history.items.filter((item) => item.kind === 'toolCard');
  check('tool results pair back to their calls by toolCallId, not adjacency',
    cards.length === 2
      && cards[0].kind === 'toolCard' && cards[0].toolCallId === 'call-1' && cards[0].status === 'done' && cards[0].result.includes('src/x.rs:1:needle')
      && cards[1].kind === 'toolCard' && cards[1].toolCallId === 'call-2' && cards[1].result.includes('fn lib() {}'),
    JSON.stringify(cards));
  const bashArgs = cards[0].kind === 'toolCard' && cards[0].args !== null && typeof cards[0].args === 'object' && 'command' in cards[0].args
    ? cards[0].args.command
    : undefined;
  const readArgs = cards[1].kind === 'toolCard' && cards[1].args !== null && typeof cards[1].args === 'object' && 'path' in cards[1].args
    ? cards[1].args.path
    : undefined;
  check('call args survive for the card detail views',
    bashArgs === 'rg -n needle src/' && readArgs === 'src/lib.rs');
  const bash = history.items.find((item) => item.kind === 'bash');
  check('bash execution row keeps command, output, and done status',
    bash?.kind === 'bash' && bash.command === 'make verify' && bash.output === 'ok' && bash.status === 'done',
    JSON.stringify(bash));
  const orphan = history.items.find((item) => item.kind === 'toolResult');
  check('orphan tool result is kept as its own bounded row', orphan?.kind === 'toolResult' && orphan.text === '', JSON.stringify(orphan));
}

// Required row contract: every row is `{ id?, message: { role } }`. A
// malformed row rejects the WHOLE payload (never a half-valid/empty success
// that would overwrite last-good), and the role is a closed enum.
check(
  'a row without its message record rejects the payload',
  rejection(wire([{ id: 'x' }])).includes('invalid message row'),
  rejection(wire([{ id: 'x' }])),
);
check(
  'a non-record message rejects the payload',
  rejection(wire([{ id: 'x', message: null }])).includes('invalid message row'),
  rejection(wire([{ id: 'x', message: null }])),
);
check(
  'a non-string message role rejects the payload',
  rejection(wire([{ id: 'x', message: { role: 7 } }])).includes('invalid message row'),
  rejection(wire([{ id: 'x', message: { role: 7 } }])),
);
check(
  'an unknown message role rejects the payload',
  rejection(wire([{ message: { role: 'vendor' } }])).includes('invalid message row'),
  rejection(wire([{ message: { role: 'vendor' } }])),
);
check(
  'one malformed row rejects the whole payload (no half-valid list)',
  rejection(wire([
    { message: { role: 'user', content: [{ type: 'text', text: 'real row' }] } },
    { id: 'bad' },
  ])).includes('invalid message row'),
  rejection(wire([
    { message: { role: 'user', content: [{ type: 'text', text: 'real row' }] } },
    { id: 'bad' },
  ])),
);
check(
  'a row without an id remains legal',
  projectAgentHistory(
    wire([{ message: { role: 'user', content: [{ type: 'text', text: 'only' }] } }]),
    'child-agent',
  ).items.length === 1,
);
check(
  'an empty messages array remains a legal empty response',
  projectAgentHistory(wire([]), 'child-agent').items.length === 0,
);

// Display contract: user/assistant/toolResult content is a block array; bash
// command/output are strings; custom display is a boolean. These are the
// fields `public_history_message` always emits and the renderer depends on.
check(
  'non-array message content rejects the payload',
  rejection(wire([{ message: { role: 'assistant', content: 42 } }])).includes('invalid message row'),
);
check(
  'a non-record content block rejects the payload',
  rejection(wire([{ message: { role: 'assistant', content: [null] } }])).includes('invalid message row'),
);
check(
  'a block without a type rejects the payload',
  rejection(wire([{ message: { role: 'assistant', content: [{}] } }])).includes('invalid message row'),
);
check(
  'a text block without text rejects the payload',
  rejection(wire([{ message: { role: 'assistant', content: [{ type: 'text' }] } }])).includes('invalid message row'),
);
check(
  'an image block without mimeType rejects the payload',
  rejection(wire([{ message: { role: 'assistant', content: [{ type: 'image', data: 'aGk=' }] } }])).includes('invalid message row'),
);
check(
  'a toolCall block without id/name rejects the payload',
  rejection(wire([{ message: { role: 'assistant', content: [{ type: 'toolCall', arguments: {} }] } }])).includes('invalid message row'),
);
check(
  'an unknown block type rejects the payload',
  rejection(wire([{ message: { role: 'assistant', content: [{ type: 'vendor' }] } }])).includes('invalid message row'),
);
check(
  'bash command/output must be strings',
  rejection(wire([{ message: { role: 'bashExecution', command: 42, output: 'ok' } }])).includes('invalid message row')
    && rejection(wire([{ message: { role: 'bashExecution', command: 'ls', output: null } }])).includes('invalid message row'),
);
check(
  'custom display must be a boolean',
  rejection(wire([{ message: { role: 'custom', customType: 'notice', display: 'yes', content: 'hi' } }])).includes('invalid message row'),
);
{
  const user = projectAgentHistory(
    wire([{ message: { role: 'user', content: [{ type: 'text', text: 'hi' }, { type: 'image', data: 'aGk=', mimeType: 'image/png' }] } }]),
    'child-agent',
  ).items[0];
  check('valid text and image blocks project',
    user?.kind === 'user' && user.text === 'hi' && user.images?.length === 1,
    JSON.stringify(user));
  const tool = projectAgentHistory(
    wire([
      { message: { role: 'assistant', content: [{ type: 'toolCall', id: 'call-1', name: 'bash', arguments: { command: 'ls' } }] } },
      { message: { role: 'toolResult', toolCallId: 'call-1', content: [{ type: 'text', text: 'ok' }], isError: false } },
    ]),
    'child-agent',
  ).items[0];
  check('valid toolCall/toolResult controls still project',
    tool?.kind === 'toolCard' && tool.toolCallId === 'call-1' && tool.status === 'done' && tool.result === 'ok',
    JSON.stringify(tool));
  const bash = projectAgentHistory(
    wire([{ message: { role: 'bashExecution', command: 'ls', output: 'ok' } }]),
    'child-agent',
  ).items[0];
  check('valid bash command/output projects', bash?.kind === 'bash' && bash.command === 'ls' && bash.output === 'ok');
  const custom = projectAgentHistory(
    wire([{ message: { role: 'custom', customType: 'notice', display: true, content: 'hi' } }]),
    'child-agent',
  ).items[0];
  check('valid custom display projects', custom?.kind === 'custom' && custom.label === 'notice');
}

// ---- renderSubagentHistory → real SurfaceItems (SSR) ------------------------
{
  const history = projectAgentHistory(
    wire([
      { id: 'u-1', message: { role: 'user', content: [{ type: 'text', text: 'steer the child' }] } },
      {
        id: 'a-1',
        message: {
          role: 'assistant',
          content: [
            { type: 'thinking', thinking: 'plan first' },
            { type: 'text', text: 'details live in [the tracker](https://example.com/tracker/1)' },
            { type: 'toolCall', id: 'call-1', name: 'bash', arguments: { command: 'rg -n needle src/' } },
          ],
        },
      },
      { message: { role: 'toolResult', toolCallId: 'call-1', content: [{ type: 'text', text: 'src/x.rs:1:needle' }], isError: false } },
      { message: { role: 'bashExecution', command: 'make verify', output: 'all green', exitCode: 0 } },
      { message: { role: 'toolResult', toolCallId: 'orphan', content: [{ type: 'text', text: '' }], isError: true } },
      { id: 'u-2', message: { role: 'user', content: [{ type: 'text', text: 'with image' }, { type: 'image', mimeType: 'image/png', data: 'aGk=' }] } },
    ]),
    'child-agent',
  );
  const html = renderToStaticMarkup(renderSubagentHistory(history.items));
  check('thinking renders through the shared thinking fold',
    html.includes('class="thinking"') && html.includes('plan first'), html.slice(0, 800));
  check('assistant prose renders through the markdown pipeline',
    html.includes('details live in'), html.slice(0, 800));
  check('external URL survives ownerless rendering as a real anchor',
    html.includes('href="https://example.com/tracker/1"'), html.slice(0, 1200));
  check('tool call renders as the shared ToolCard with command and result',
    html.includes('tool-card') && html.includes('rg -n needle src/') && html.includes('src/x.rs:1:needle'),
    html.slice(0, 2000));
  check('bash execution renders as the shared BashCard with done status',
    html.includes('msg--bash') && html.includes('make verify') && html.includes('all green'));
  check('empty tool result renders its explicit empty marker, not nothing',
    html.includes('(empty tool result)'));
  check('user image renders as an inline data image',
    html.includes('msg--user__image') && html.includes('data:image/png;base64,aGk='));
  check('rows carry the ownerless session stamp',
    html.includes('data-session-id=""') && html.includes('data-workspace-cwd=""'));
  check('no flat history-row markup leaks back in',
    !html.includes('data-history-kind') && !html.includes('subagent-details__history'));
}

// ---- ownerless click boundary ----------------------------------------------
interface MiniNode {
  nodeType: number;
  tag: string;
  textContent: string | null;
  attrs: Record<string, string>;
  parent: MiniNode | null;
  getAttribute(name: string): string | null;
  closest(selector: string): MiniNode | null;
}
function miniNode(tag: string, attrs: Record<string, string>, textContent: string | null, parent: MiniNode | null): MiniNode {
  const node: MiniNode = {
    nodeType: 1,
    tag,
    textContent,
    attrs,
    parent,
    getAttribute(name: string) { return attrs[name] ?? null; },
    closest(selector: string) {
      const attr = /^\[([a-zA-Z0-9_-]+)(?:="([^"]*)")?\]$/.exec(selector);
      const cls = selector.includes('.') ? /^(?:(\w+)?\.)?([\w-]+)$/.exec(selector) : null;
      let current: MiniNode | null = node;
      while (current !== null) {
        if (attr) {
          const value = current.attrs[attr[1]];
          if (value !== undefined && (attr[2] === undefined || value === attr[2])) return current;
        } else if (cls && (cls[1] === undefined || current.tag === cls[1])
            && (current.attrs.class ?? '').split(' ').includes(cls[2])) {
          return current;
        } else if (!attr && !cls && current.tag === selector) {
          return current;
        }
        current = current.parent;
      }
      return null;
    },
  };
  return node;
}
function click(target: MiniNode) {
  const event = { target, defaultPrevented: false, preventDefault() { this.defaultPrevented = true; } };
  return { event, handled: dispatchMarkdownPathClick(event, () => { throw new Error('ownerless pane must never open a path'); }) };
}
{
  const pane = miniNode('div', { 'data-path-navigation': 'disabled', 'data-details-history': '' }, null, null);
  const row = miniNode('div', { 'data-session-id': '', 'data-workspace-cwd': '' }, null, pane);
  const relAnchor = miniNode('a', { href: 'docs/report.md' }, 'the report', row);
  const rel = click(relAnchor);
  check('relative markdown link inside the disabled pane is swallowed',
    rel.handled === true && rel.event.defaultPrevented === true);
  const extAnchor = miniNode('a', { href: 'https://example.com/tracker/1' }, 'tracker', row);
  const ext = click(extAnchor);
  check('external URL inside the disabled pane keeps default navigation',
    ext.handled === false && ext.event.defaultPrevented === false);
  const fragAnchor = miniNode('a', { href: '#section' }, 'section', row);
  const frag = click(fragAnchor);
  check('same-document fragment keeps default navigation',
    frag.handled === false && frag.event.defaultPrevented === false);
  const pathChip = miniNode('span', { 'data-md-path': 'src/lib.rs', 'data-md-line': '12' }, 'src/lib.rs:12', row);
  const chip = click(pathChip);
  check('structured tool path chip inside the disabled pane is swallowed',
    chip.handled === true && chip.event.defaultPrevented === true);
  const codePath = miniNode('code', { class: 'md-code' }, 'src/lib.rs:3', row);
  const code = click(codePath);
  check('code-span path inside the disabled pane is swallowed',
    code.handled === true && code.event.defaultPrevented === true);
}


if (failures.length > 0) {
  console.error(`subagentsProjection.test: ${failures.length}/${ran} failure(s)`);
  for (const failure of failures) console.error(`- ${failure}`);
  process.exit(1);
}
console.log(`subagentsProjection.test: ${ran} assertions passed`);
process.exit(0);
