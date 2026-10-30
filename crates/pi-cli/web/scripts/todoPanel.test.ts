#!/usr/bin/env node
// Focused pure/component regression for the ordinary Todo panel convergence
// contract (T4): after every task completes, the global counters, every
// phase header counter, and every rendered row must converge to the same
// authoritative projection — zero open/active/waiting with the exact done
// count. Rows and counters derive from the SAME phases array, so a stale
// counter, a hidden incomplete row, or a phase/global mismatch fails here.
//
// The fixture mirrors the real-listener E2E lane
// (E2E.d/web/coverage_test.mjs `todo.all-rows-counts-converged`): exactly
// two rows — "dep target task" and "web e2e task" — in phase "Plan", both
// terminal completed. The projection path (projectTodoPhases +
// applyTodoEvent) is exercised too: a stale/foreign todo_updated frame must
// never regress a converged snapshot.
//
// Run via esbuild (focused, no project-wide build):
//   esbuild scripts/todoPanel.test.ts --bundle --platform=node --format=esm \
//     --loader:.css=empty --outfile=.web-test-build/todoPanel.test.mjs \
//   && node .web-test-build/todoPanel.test.mjs
//
// Exit codes: 0 = every assertion held; 1 = a regression.
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server.browser';
import { TodoPanel } from '../src/panels/TodoPanel.tsx';
import {
  applyTodoEvent,
  projectTodoPhases,
  type TodoPhase,
  type TodoSessionSnapshot,
} from '../src/panels/todoPanelState.ts';

const failures: string[] = [];
let ran = 0;
function check(name: string, condition: unknown, detail = ''): void {
  ran += 1;
  if (!condition) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

/** Wire shape mirrors pi_coding::TodoPhase (serde camelCase). */
const completedWire = [
  {
    name: 'Plan',
    tasks: [
      { id: 't-1', content: 'dep target task', status: 'completed', dependsOn: [], ready: false, waitingOn: [] },
      { id: 't-2', content: 'web e2e task', status: 'completed', dependsOn: [], ready: false, waitingOn: [] },
    ],
  },
];

const mixedWire = [
  {
    name: 'Plan',
    tasks: [
      { id: 't-1', content: 'dep target task', status: 'completed', dependsOn: [], ready: false, waitingOn: [] },
      { id: 't-2', content: 'web e2e task', status: 'in_progress', dependsOn: [], ready: true, waitingOn: [] },
    ],
  },
];

const multiPhaseWire = [
  {
    name: 'Plan',
    tasks: [
      { id: 'p-1', content: 'first planned task', status: 'completed', dependsOn: [], ready: false, waitingOn: [] },
      { id: 'p-2', content: 'second planned task', status: 'completed', dependsOn: [], ready: false, waitingOn: [] },
    ],
  },
  {
    name: 'Ship',
    tasks: [
      { id: 's-1', content: 'shipping task', status: 'in_progress', dependsOn: [], ready: true, waitingOn: [] },
    ],
  },
];

function render(phases: TodoPhase[], executionStatus: 'dormant' | 'active' | 'settled' | 'failed' = 'settled'): string {
  return renderToStaticMarkup(
    createElement(TodoPanel, {
      phases,
      executionStatus,
      failureReason: null,
      onOp: () => {},
      parentSessionId: 'parent-session',
      sendCommand: async () => ({}),
      onClose: () => {},
    }),
  );
}

function rows(html: string): string[] {
  return [...html.matchAll(/class="todo-task(?:\s|")/g)].map(() => 'row');
}

function bullets(html: string): string[] {
  return [...html.matchAll(/todo-task__bullet--([a-z_]+)/g)].map((m) => m[1]);
}

function globalCounts(html: string): string[] {
  return [...html.matchAll(/todo-panel__counts-item">([^<]+)</g)].map((m) => m[1]);
}

function phaseCounts(html: string): string[] {
  return [...html.matchAll(/todo-phase__counts">([^<]+)</g)].map((m) => m[1]);
}

// ---- converged fixture: every row terminal, counters exact ----
{
  const phases = projectTodoPhases(completedWire);
  check('fixture projects exactly two tasks', phases.length === 1 && phases[0].tasks.length === 2, JSON.stringify(phases));
  const html = render(phases);
  check('converged: exactly two rendered task rows', rows(html).length === 2, html);
  check('converged: every rendered row is terminal completed', bullets(html).length === 2 && bullets(html).every((b) => b === 'completed'), bullets(html).join(','));
  const counts = globalCounts(html);
  check(
    'converged: global counts are 0 open · 0 active · 0 waiting · 2 done',
    counts.length === 4 && counts[0] === '0 open' && counts[1] === '0 active' && counts[2] === '0 waiting' && counts[3] === '2 done',
    counts.join(' | '),
  );
  const phase = phaseCounts(html);
  check('converged: phase header reports 0 open · 2 done', phase.length === 1 && phase[0] === '0 open · 2 done', phase.join(' | '));
  check('converged: row content identity is exact', html.includes('dep target task') && html.includes('web e2e task'));
}

// ---- mixed fixture: counters track rows, not a frozen display ----
{
  const phases = projectTodoPhases(mixedWire);
  const html = render(phases);
  check('mixed: two rows rendered', rows(html).length === 2, html);
  check('mixed: one completed + one in_progress bullet', bullets(html).sort().join(',') === 'completed,in_progress', bullets(html).join(','));
  const counts = globalCounts(html);
  check(
    'mixed: global counts are 1 open · 1 active · 0 waiting · 1 done',
    counts.length === 4 && counts[0] === '1 open' && counts[1] === '1 active' && counts[2] === '0 waiting' && counts[3] === '1 done',
    counts.join(' | '),
  );
  const phase = phaseCounts(html);
  check('mixed: phase header reports 1 open · 1 done', phase.length === 1 && phase[0] === '1 open · 1 done', phase.join(' | '));
}

// ---- multi-phase fixture: phase-local counts must not repeat global totals ----
{
  const phases = projectTodoPhases(multiPhaseWire);
  const html = render(phases, 'active');
  const counts = globalCounts(html);
  check(
    'multi-phase: global counts aggregate all three rows',
    counts.length === 4 && counts[0] === '1 open' && counts[1] === '1 active' && counts[2] === '0 waiting' && counts[3] === '2 done',
    counts.join(' | '),
  );
  const phase = phaseCounts(html);
  check(
    'multi-phase: each header reports its own distinct row totals',
    phase.length === 2 && phase[0] === '0 open · 2 done' && phase[1] === '1 open · 0 done',
    phase.join(' | '),
  );
}

// ---- projection path: applyTodoEvent converges and rejects stale/foreign ----
{
  const base: TodoSessionSnapshot = { sessionId: 'sess-a', phases: [], sequence: 0, executionStatus: 'dormant', failureReason: null };
  const finalFrame = {
    type: 'todo_updated' as const,
    sessionId: 'sess-a',
    sequence: 5,
    phases: completedWire,
    executionStatus: 'settled',
  };
  const converged = applyTodoEvent(base, finalFrame);
  check(
    'projection: final frame converges to the two exact completed tasks',
    converged.phases.length === 1 &&
      converged.phases[0].tasks.length === 2 &&
      converged.phases[0].tasks.map((task) => task.id).join(',') === 't-1,t-2' &&
      converged.phases[0].tasks.every((task) => task.status === 'completed'),
    JSON.stringify(converged.phases),
  );
  check('projection: sequence and execution status advance', converged.sequence === 5 && converged.executionStatus === 'settled');
  const convergedHtml = render(converged.phases, converged.executionStatus);
  check(
    'projection: accepted frame renders exact rows and converged counts',
    rows(convergedHtml).length === 2 &&
      bullets(convergedHtml).join(',') === 'completed,completed' &&
      globalCounts(convergedHtml).join('|') === '0 open|0 active|0 waiting|2 done' &&
      phaseCounts(convergedHtml).join('|') === '0 open · 2 done' &&
      convergedHtml.includes('dep target task') &&
      convergedHtml.includes('web e2e task'),
    convergedHtml,
  );
  const stale = applyTodoEvent(converged, {
    type: 'todo_updated' as const,
    sessionId: 'sess-a',
    sequence: 3,
    phases: mixedWire,
    executionStatus: 'active',
  });
  check(
    'projection: stale sequence frame preserves the rendered converged snapshot',
    stale === converged && render(stale.phases, stale.executionStatus) === convergedHtml,
  );
  const foreign = applyTodoEvent(converged, {
    type: 'todo_updated' as const,
    sessionId: 'sess-b',
    sequence: 9,
    phases: mixedWire,
    executionStatus: 'active',
  });
  check(
    'projection: foreign session frame preserves the rendered converged snapshot',
    foreign === converged && render(foreign.phases, foreign.executionStatus) === convergedHtml,
  );
}

if (failures.length > 0) {
  console.error(`todoPanel.test: ${failures.length} failure(s) of ${ran} checks`);
  for (const failure of failures) console.error(`  FAIL ${failure}`);
  process.exit(1);
}
console.log(`todoPanel.test: all ${ran} checks passed`);
