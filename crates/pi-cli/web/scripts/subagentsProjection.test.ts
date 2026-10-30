#!/usr/bin/env node
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
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
import { historyTranscriptRows, historyRowTag, SubagentHistoryList } from '../src/subagentHistory.tsx';
import { act, create, type ReactTestRenderer } from 'react-test-renderer';
import { cssBlock, phoneSheetRects, rectsIntersect, subagentSheetTaskShrinks } from '../src/subagentSheetLayout.ts';

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

{
  const parsed = historyTranscriptRows([
    'user: steer the child',
    'assistant: working on it',
    'assistant · bash rg -n "verify_state_updates"',
    '[tool: bash] · ok',
    'assistant · read seed.txt',
    'assistant · hub message=go, op=send',
    '[tool: read] · ok',
    '[tool: hub] · error',
    '[bash] ls',
  ].join('\n'));
  check('role rows keep muted tags and text', parsed[0]?.kind === 'text' && parsed[0].kind === 'text' && parsed[0].role === 'user' && parsed[0].text === 'steer the child');
  check('assistant prose is a text row', parsed[1]?.kind === 'text' && parsed[1].kind === 'text' && parsed[1].role === 'assistant' && parsed[1].text === 'working on it');
  check('call and result merge into one bash row',
    parsed[2]?.kind === 'call'
    && parsed[2].kind === 'call'
    && parsed[2].name === 'bash'
    && parsed[2].summary === 'rg -n "verify_state_updates"'
    && parsed[2].status.kind === 'ok');
  check('tool rows are labeled with the humanized action, not the wire tag',
    historyRowTag(parsed[2]) === 'Command'
    && historyRowTag(parsed[3]) === 'Read'
    && historyRowTag(parsed[4]) === 'Hub'
    && historyRowTag(parsed[5]) === 'Command');
  check('later results match pending calls by name, not adjacency',
    parsed[3]?.kind === 'call' && parsed[3].kind === 'call' && parsed[3].name === 'read' && parsed[3].status.kind === 'ok'
    && parsed[4]?.kind === 'call' && parsed[4].kind === 'call' && parsed[4].name === 'hub' && parsed[4].status.kind === 'error');
  check('named command line is a call row', parsed[5]?.kind === 'call' && parsed[5].kind === 'call' && parsed[5].name === 'bash' && parsed[5].summary === 'ls');
  check('merged history is six rows', parsed.length === 6, String(parsed.length));
  check('unknown role tokens are not role rows', (() => {
    const unknownRole = historyTranscriptRows('vendor: notice');
    const first = unknownRole[0];
    return unknownRole.length === 1 && first?.kind === 'line' && first.text === 'vendor: notice';
  })());

  // Two-space-indented continuation lines carry extra evidence (multi-line
  // assistant prose, bounded tool output preview). They extend the row they
  // belong to — a call row the result line MERGED into, which is not always
  // the last row — and a leading orphan (a stream slice that starts
  // mid-block) degrades to a plain row.
  const continued = historyTranscriptRows([
    '  orphan continuation from a partial stream head',
    'assistant: first prose line',
    '  second prose line',
    'assistant · read a.txt',
    'assistant · read b.txt',
    '[tool: read] · ok',
    '  a.txt:1:hello',
    '[tool: read] · ok',
    '  b.txt:2:world',
  ].join('\n'));
  check('continuation lines extend the row they belong to',
    continued.length === 4
    && continued[0]?.kind === 'line'
    && continued[0].kind === 'line'
    && continued[0].text === 'orphan continuation from a partial stream head'
    && continued[1]?.kind === 'text'
    && continued[1].kind === 'text'
    && continued[1].text === 'first prose line\nsecond prose line'
    && continued[2]?.kind === 'call'
    && continued[2].kind === 'call'
    && continued[2].summary === 'a.txt'
    && continued[2].output === 'a.txt:1:hello'
    && continued[3]?.kind === 'call'
    && continued[3].kind === 'call'
    && continued[3].summary === 'b.txt'
    && continued[3].output === 'b.txt:2:world',
    JSON.stringify(continued));

  const continuedHtml = renderToStaticMarkup(createElement(SubagentHistoryList, {
    text: [
      'assistant: first prose line',
      '  second prose line',
      'assistant · read a.txt',
      '[tool: read] · ok',
      '  a.txt:1:hello',
      '  a.txt:2:world',
    ].join('\n'),
    error: '',
  }));
  check('multi-line prose stays multi-line through the markdown pipeline',
    continuedHtml.includes('<p class="md-p">first prose line</p>')
    && continuedHtml.includes('<p class="md-p">second prose line</p>'));
  check('tool output preview stays out of the collapsed row',
    !/<pre[^>]*data-history-output(?:=|\s|>)/.test(continuedHtml)
    && !continuedHtml.includes('a.txt:1:hello'));

  // Bug 1 repro fixtures: targets longer than the old 64-char preview cut, so
  // the pre-fix renderer produced `real_data/…`-style ellipsis stubs.
  const longPath = 'real_data/psy_node_common/src/realm/processor/proposal_store_impl.rs';
  const longCommand = `nl -ba ${longPath}`;
  const grepCommand = 'grep -n verify_state_updates crates/pi-coding/src/orchestration/runtime.rs';
  const rows = renderToStaticMarkup(createElement(SubagentHistoryList, {
    text: [
      'user: steer the child',
      `assistant · bash ${longCommand}`,
      '[tool: bash] · ok',
      `assistant · read ${longPath}`,
      '[tool: read] · ok',
      `assistant · grep ${grepCommand}`,
      '[tool: grep] · ok',
      'Reply from Main: go',
    ].join('\n'),
    error: '',
  }));
  check('tool rows carry the full command and path — no basename stub, no ellipsis',
    rows.includes(longCommand)
    && rows.includes(longPath)
    && rows.includes(grepCommand)
    && !rows.includes('\u2026')
    && !rows.includes('nl · proposal_store_impl.rs'),
    rows.slice(0, 900));
  check('the tool outcome is its own labeled chip, never folded into the target body',
    rows.includes('data-history-status="ok"')
    && !rows.includes(' · ok</span>'),
    rows.slice(0, 500));

  const prose = renderToStaticMarkup(createElement(SubagentHistoryList, {
    text: 'user: steer the child\nassistant: shipped **bold** with `code`',
    error: '',
  }));
  check('prose renders through the shared Markdown pipeline, not flat role text',
    prose.includes('class="subagent-details__history-md"')
    && prose.includes('<p class="md-p">shipped <strong>bold</strong> with <code class="md-code">code</code></p>')
    && !prose.includes('**bold**'),
    prose.slice(0, 600));
  check('user prose keeps its scanning tag; assistant prose carries none',
    prose.includes('>user</span>') && !prose.includes('>assistant</span>'));

  check('successful output stays collapsed behind one labeled Output control',
    (continuedHtml.match(/<button[^>]*data-history-output-toggle/g) ?? []).length === 1
    && continuedHtml.includes('aria-expanded="false"')
    && !/<pre[^>]*data-history-output(?:=|\s|>)/.test(continuedHtml)
    && !continuedHtml.includes('a.txt:1:hello'));
  const failedHtml = renderToStaticMarkup(createElement(SubagentHistoryList, {
    text: 'assistant · bash cargo test\n[tool: bash] · error\n  thread panicked at src/main.rs:2',
    error: '',
  }));
  check('failed output is open and visible without interaction',
    failedHtml.includes('data-history-output')
    && failedHtml.includes('thread panicked')
    && failedHtml.includes('aria-expanded="true"'));
  check('rows are containers, not buttons — markdown links must not nest in a control',
    !rows.includes('class="subagent-details__history-row" aria-expanded')
    && /<div[^>]*class="subagent-details__history-row"[^>]*data-history-kind="call"/.test(rows),
    rows.slice(0, 400));
  check('unparsed wire lines stay plain rows without a control',
    rows.includes('Reply from Main: go')
    && !rows.slice(rows.indexOf('data-history-kind="line"')).includes('data-history-output-toggle'),
    rows.slice(-400));

  const html = renderToStaticMarkup(createElement(SubagentHistoryList, {
    text: 'user: hello there\nassistant · bash rg -n "verify_state_updates"\n[tool: bash] · ok',
    error: '',
  }));
  check('transcript is compact labeled rows, not a wrapped pre', !html.includes('<pre') && html.includes('subagent-details__transcript') && html.includes('subagent-details__history-tag') && html.includes('subagent-details__history-text'));
  check('role tag and body are separate elements', html.includes('>user</span>') && html.includes('<p class="md-p">hello there</p>') && !html.includes('user: hello there<'));
  check('merged tool row is one command row',
    html.includes('>Command</span>')
    && html.includes('verify_state_updates')
    && (html.match(/data-history-kind="call"/g) || []).length === 1
    && !html.includes('[tool: bash]')
    && !html.includes('>assistant</span>'),
    html.slice(0, 800));

  const invocationHtml = renderToStaticMarkup(createElement(SubagentHistoryList, {
    text: 'assistant: working on it\nassistant · Read FEATURES.md\nassistant · bash cargo build\nassistant · hub message=go, op=send',
    error: '',
  }));
  check('assistant prose renders unlabeled through the markdown pipeline',
    !invocationHtml.includes('>assistant</span>') && invocationHtml.includes('<p class="md-p">working on it</p>'));
  check('invocation rows use the humanized tool name as their tag',
    invocationHtml.includes('>Read</span>')
    && invocationHtml.includes('>FEATURES.md</span>')
    && invocationHtml.includes('>Command</span>')
    && invocationHtml.includes('>cargo build</span>')
    && invocationHtml.includes('>Hub</span>'));

  const empty = renderToStaticMarkup(createElement(SubagentHistoryList, { text: '', error: '' }));
  check('empty transcript is a compact placeholder row', empty.includes('(no transcript yet — waiting for child activity)') && !empty.includes('<pre'));

  const here = dirname(fileURLToPath(import.meta.url));
  const styles = readFileSync(join(here, '../src/styles.css'), 'utf8');
  const bodyStart = styles.indexOf('.subagent-details__body {\n  flex: 1 1 auto');
  const bodyRule = bodyStart >= 0 ? styles.slice(bodyStart, styles.indexOf('}', bodyStart)) : '';
  const headStart = styles.indexOf('.subagent-details__head {');
  const headRule = headStart >= 0 ? styles.slice(headStart, styles.indexOf('}', headStart)) : '';
  const sheetStart = styles.indexOf('.subagent-details {\n  display: flex');
  const sheetRule = sheetStart >= 0 ? styles.slice(sheetStart, styles.indexOf('}', sheetStart)) : '';
  const historyRowStart = styles.indexOf('.subagent-details__history-row {\n  display: flex');
  const historyRow = historyRowStart >= 0 ? styles.slice(historyRowStart, styles.indexOf('}', historyRowStart)) : '';
  const phoneMqStart = styles.indexOf('@media (max-width: 768px) {');
  const phoneMq = phoneMqStart >= 0 ? styles.slice(phoneMqStart, styles.indexOf('\n}\n\n@media (max-width: 480px)', phoneMqStart)) : '';
  const phoneBody = cssBlock(styles, '.subagent-details__body {\n    -webkit-overflow-scrolling: touch;');
  const phoneSheet = cssBlock(styles, '.subagent-details {\n    max-height: 94dvh;');
  const phoneBackdrop = cssBlock(styles, '.subagent-details-backdrop { padding: 8px; align-items: stretch; }');
  check('sheet is a flex column with overflow hidden and min-height 0', sheetRule.includes('flex-direction: column') && sheetRule.includes('overflow: hidden') && sheetRule.includes('min-height: 0'));
  check('sheet header is not the scroller', headRule.includes('flex: none'));
  check('sheet body is the only overflow-y auto region', bodyRule.includes('flex: 1 1 auto') && bodyRule.includes('min-height: 0') && bodyRule.includes('overflow-y: auto'));
  check('static CSS: row is a column container whose first line holds label, body, outcome and control',
    historyRow.includes('flex-direction: column')
    && historyRow.includes('min-height: 0')
    && historyRow.includes('padding: 4px 0')
    && cssBlock(styles, '.subagent-details__history-line {').includes('display: flex'));
  check('phone history rows stay 44px after the compact base rule',
    styles.lastIndexOf('@media (max-width: 768px) {\n  .subagent-details__history-row {\n    min-height: 44px;')
      > styles.indexOf('.subagent-details__history-row {\n  display: flex'));
  check('static CSS: output preview stays bounded and scrolls',
    cssBlock(styles, '.subagent-details__history-output {').includes('max-height: 12em')
    && cssBlock(styles, '.subagent-details__history-output {').includes('overflow-y: auto')
    && cssBlock(styles, '.subagent-details__history-output {').includes('white-space: pre-wrap'));
  check('static CSS: phone sheet overrides sit after the base sheet rules, so they are not defeated',
    !phoneMq.includes('.subagent-details {\n    max-height: 94dvh')
    && styles.indexOf('.subagent-details {\n    max-height: 94dvh') > sheetStart
    && phoneSheet.includes('max-height: 94dvh')
    && phoneSheet.includes('min-height: 0')
    && phoneBackdrop.includes('align-items: stretch')
    && phoneBody.includes('-webkit-overflow-scrolling: touch'),
    JSON.stringify({ phoneSheet, phoneBackdrop, phoneBody }));
  const sectionStart = styles.indexOf('.subagent-details__section {\n  display: flex');
  const section = sectionStart >= 0 ? styles.slice(sectionStart, styles.indexOf('}', sectionStart)) : '';
  const descriptionStart = styles.indexOf('.subagent-details__description {');
  const description = descriptionStart >= 0 ? styles.slice(descriptionStart, styles.indexOf('}', descriptionStart)) : '';
  const shipped = phoneSheetRects(styles);
  check('TASK/activity sections are content-sized in normal flow',
    section.includes('flex: none')
    && !section.includes('min-height: 0')
    && !styles.includes('.subagent-details__section--transcript { flex: 1; }')
    && !description.includes('position: absolute')
    && !subagentSheetTaskShrinks(styles)
    && styles.includes('.subagent-details__body > * {'));
  check('CJK TASK bounding box does not intersect LATEST ACTIVITY',
    !rectsIntersect(shipped.task, shipped.activity)
    && shipped.task.bottom <= shipped.activity.top);
  check('Subagents hides the standing height-resizer line',
    styles.includes('body:has(#subagents-panel) .panel-drawer-resizer')
    && styles.includes('body:has(#subagents-panel) .file-explorer-resizer'));
  check('Subagents counts stay one ellipsized line',
    styles.includes('.subagents-panel__counts { min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }')
    && styles.includes('.subagents-panel { width: 460px; overflow-y: hidden; border-top: none; }'));

  // Bug 2 (390x844): the backdrop is a descendant of the fixed panel, so the
  // panel's own z-index decides the modal's layer against the root-level
  // composer footer — whatever the backdrop declares. These are static
  // rule/source contracts, not a rendered journey: they pin the numbers the
  // browser resolves, and the marker class that only exists while details is
  // open.
  const zIndexOf = (css: string): number => {
    const match = /z-index:\s*(\d+)/.exec(css);
    return match ? Number(match[1]) : Number.NaN;
  };
  const footerZ = zIndexOf(cssBlock(styles, 'footer {\n  display: flex'));
  const backdropStart = styles.indexOf('.subagent-details-backdrop {\n  position: fixed;');
  const backdropRule = backdropStart >= 0 ? styles.slice(backdropStart, styles.indexOf('}', backdropStart)) : '';
  const backdropZ = zIndexOf(backdropRule);
  const drawerZ = zIndexOf(cssBlock(styles, '.subagents-panel,\n.code-review-panel {'));
  const raisedStart = styles.indexOf('.subagents-panel--details {');
  const raisedZ = zIndexOf(cssBlock(styles, '.subagents-panel--details {'));
  check('static CSS: the modal backdrop is trapped in the panel stacking context, under the root composer footer',
    backdropZ === 60 && drawerZ === 46 && footerZ === 51 && drawerZ < footerZ,
    JSON.stringify({ backdropZ, drawerZ, footerZ }));
  check('static CSS: opening details lifts the panel above the footer, every backdrop cap and toasts, below the mermaid viewer',
    raisedStart >= 0
    && !styles.slice(Math.max(0, raisedStart - 400), raisedStart).includes('@media')
    && raisedZ > footerZ
    && raisedZ > drawerZ
    && raisedZ > backdropZ
    && raisedZ < 70,
    JSON.stringify({ raisedStart, raisedZ, footerZ, drawerZ, backdropZ }));
  const panelSource = readFileSync(join(here, '../src/panels/SubagentsPanel.tsx'), 'utf8');
  check('static source: the panel asks for the core history line cap (200)',
    /const HISTORY_LINES = (\d+);/.exec(panelSource)?.[1] === '200');
  const panelModifier = /className=\{detailsJob \? '([^']*)' : '([^']*)'\}/.exec(panelSource);
  check('static source: the panel carries the details-open modifier only while the modal is open',
    panelModifier !== null
    && (panelModifier[1] ?? '').includes('subagents-panel--details')
    && !(panelModifier[2] ?? '').includes('subagents-panel--details'),
    String(panelModifier));
}

// Mounted behavior (react-test-renderer, same harness as chatSessionView.test.ts):
// exact poll sequence — closed ok→error opens; collapsed error→error stays
// closed; opened ok→ok stays open.
{
  const previousActEnv = Object.getOwnPropertyDescriptor(globalThis, 'IS_REACT_ACT_ENVIRONMENT');
  Object.defineProperty(globalThis, 'IS_REACT_ACT_ENVIRONMENT', { configurable: true, writable: true, value: true });
  const longOutput = Array.from({ length: 12 }, (_, i) => `line-${i}: ${'x'.repeat(60)}`).join('\n  ');
  const target = 'nl -ba real_data/psy_node_common/src/realm/processor/proposal_store_impl.rs';
  const callText = (status: 'ok' | 'error') => `assistant · bash ${target}\n[tool: bash] · ${status}\n  ${longOutput}`;
  let mounted: ReactTestRenderer | undefined;
  const outputPres = () => mounted!.root.findAllByProps({ 'data-history-output': true });
  const outputToggles = () => mounted!.root.findAllByProps({ 'data-history-output-toggle': true });
  try {
    await act(async () => {
      mounted = create(createElement(SubagentHistoryList, { text: callText('ok'), error: '' }), { createNodeMock: () => null });
    });
    check('mounted: successful output starts collapsed',
      outputPres().length === 0 && outputToggles().length === 1 && outputToggles()[0].props['aria-expanded'] === false);
    await act(async () => {
      mounted!.update(createElement(SubagentHistoryList, { text: callText('error'), error: '' }));
    });
    check('mounted: ok→error transition auto-opens the failed output',
      outputPres().length === 1 && outputToggles()[0].props['aria-expanded'] === true);
    await act(async () => { outputToggles()[0].props.onClick(); });
    check('mounted: user collapse closes the failed output', outputPres().length === 0);
    await act(async () => {
      mounted!.update(createElement(SubagentHistoryList, { text: callText('error'), error: '' }));
    });
    check('mounted: error→error refresh stays collapsed', outputPres().length === 0);
    await act(async () => {
      mounted!.update(createElement(SubagentHistoryList, { text: callText('ok'), error: '' }));
    });
    check('mounted: error→ok keeps the user collapse', outputPres().length === 0);
    await act(async () => { outputToggles()[0].props.onClick(); });
    check('mounted: Output reveals the full bounded body',
      outputPres().length === 1 && String(outputPres()[0].props.children).includes('line-11:'));
    await act(async () => {
      mounted!.update(createElement(SubagentHistoryList, { text: callText('ok'), error: '' }));
    });
    check('mounted: ok→ok refresh keeps the opened output',
      outputPres().length === 1 && outputToggles()[0].props['aria-expanded'] === true);
  } finally {
    if (mounted) await act(async () => { mounted!.unmount(); });
    if (previousActEnv) Object.defineProperty(globalThis, 'IS_REACT_ACT_ENVIRONMENT', previousActEnv);
    else delete (globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT;
  }
}

if (failures.length > 0) {
  console.error(`subagentsProjection.test: ${failures.length}/${ran} failure(s)`);
  for (const failure of failures) console.error(`- ${failure}`);
  process.exit(1);
}
console.log(`subagentsProjection.test: ${ran} assertions passed`);
process.exit(0);
