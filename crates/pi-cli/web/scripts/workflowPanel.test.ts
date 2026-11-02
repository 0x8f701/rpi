#!/usr/bin/env node
// Workflow failure surface regression for src/panels/WorkflowPanel.tsx +
// workflowPanelState.ts: the detail pane must render the LIVE panel snapshot's
// failure first (the supervisor's message may not have landed on the durable
// record yet), falling back to the durable list snapshot; both projections
// bound failure to 800 chars and drop empty/non-string values. Run through
// `npm run build`.
//
// Exit codes: 0 = every assertion held; 1 = a regression.
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server.browser';
import { WorkflowDetailFailure } from '../src/panels/WorkflowPanel.tsx';
import {
  projectWorkflowDetail,
  projectWorkflowListSnapshot,
  type WorkflowPanelSnapshot,
  type WorkflowWireSnapshot,
} from '../src/panels/workflowPanelState.ts';

const failures: string[] = [];
let ran = 0;
function check(name: string, condition: unknown, detail = ''): void {
  ran += 1;
  if (!condition) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

const listWire = (failure?: unknown) => ({
  workflowId: 'wf-1',
  name: 'Safe workflow',
  objective: 'objective',
  status: 'failed',
  generation: 7,
  ...(failure === undefined ? {} : { failure }),
});

const detailWire = (failure?: unknown) => ({
  id: 'wf-1',
  generation: 7,
  name: 'Safe workflow',
  objective: 'objective',
  status: 'failed',
  ...(failure === undefined ? {} : { failure }),
});

// ---- projectWorkflowDetail keeps a live failure, bounded to 800 chars ----
{
  const long = 'x'.repeat(1200);
  const detail = projectWorkflowDetail(detailWire(long));
  check('detail projection keeps a live failure', detail?.failure === `${'x'.repeat(799)}…`, detail?.failure);
  check('detail failure is bounded to 800 chars', (detail?.failure?.length ?? 0) <= 800, detail?.failure);
}

// ---- projectWorkflowDetail drops empty and non-string failures ----
{
  const empty = projectWorkflowDetail(detailWire(''));
  check('detail projection drops empty failure', empty?.failure == null, empty?.failure);
  const nonString = projectWorkflowDetail(detailWire(42));
  check('detail projection drops non-string failure', nonString?.failure == null, nonString?.failure);
  const absent = projectWorkflowDetail(detailWire());
  check('detail projection omits absent failure', absent?.failure == null, absent?.failure);
}

// ---- list projection keeps the durable failure, bounded to 800 chars ----
{
  const long = 'y'.repeat(1200);
  const list = projectWorkflowListSnapshot(listWire(long));
  check('list projection keeps a durable failure', list?.failure === `${'y'.repeat(799)}…`, list?.failure);
  check('list failure is bounded to 800 chars', (list?.failure?.length ?? 0) <= 800, list?.failure);
  const empty = projectWorkflowListSnapshot(listWire(''));
  check('list projection drops empty failure', empty?.failure == null, empty?.failure);
  const nonString = projectWorkflowListSnapshot(listWire(true));
  check('list projection drops non-string failure', nonString?.failure == null, nonString?.failure);
}

// ---- detail pane prefers the live failure over the list snapshot ----
{
  const selected: WorkflowWireSnapshot = {
    workflowId: 'wf-1',
    name: 'Safe workflow',
    objective: 'objective',
    status: 'failed',
    generation: 7,
    failure: 'durable list failure',
  };
  const detail: WorkflowPanelSnapshot = {
    id: 'wf-1',
    generation: 7,
    name: 'Safe workflow',
    objective: 'objective',
    status: 'failed',
    failure: 'live supervisor failure',
    subagents: [],
    activeTasks: [],
  };
  const html = renderToStaticMarkup(createElement(WorkflowDetailFailure, { selected, detail }));
  check('detail pane renders the live failure', html.includes('failure: live supervisor failure'), html);
  check('detail pane never renders the list snapshot failure', !html.includes('durable list failure'), html);
  check('detail pane keeps the failure row class', html.includes('class="workflow-detail__failure"'), html);
}

// ---- detail pane falls back to the list snapshot when no live failure ----
{
  const selected: WorkflowWireSnapshot = {
    workflowId: 'wf-1',
    name: 'Safe workflow',
    objective: 'objective',
    status: 'failed',
    generation: 7,
    failure: 'durable list failure',
  };
  const detail: WorkflowPanelSnapshot = {
    id: 'wf-1',
    generation: 7,
    name: 'Safe workflow',
    objective: 'objective',
    status: 'failed',
    failure: null,
    subagents: [],
    activeTasks: [],
  };
  const html = renderToStaticMarkup(createElement(WorkflowDetailFailure, { selected, detail }));
  check('detail pane falls back to the list failure', html.includes('failure: durable list failure'), html);
}

// ---- detail pane renders nothing when neither surface carries a failure ----
{
  const selected: WorkflowWireSnapshot = {
    workflowId: 'wf-1',
    name: 'Safe workflow',
    objective: 'objective',
    status: 'running',
    generation: 7,
  };
  const html = renderToStaticMarkup(createElement(WorkflowDetailFailure, { selected, detail: null }));
  check('detail pane omits the failure row when absent', html === '', html);
}

console.log(`\nworkflowPanel.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);
