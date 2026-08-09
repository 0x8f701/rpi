#!/usr/bin/env node
// Web feature -> Playwright assertion matrix + validator (hard coverage gate).
//
// The matrix maps every required Web contract to concrete assertion IDs that
// the coverage drivers (E2E.d/web/coverage_test.mjs, coverage_xss.mjs) and
// the sessions lane (E2E.d/web/sessions_test.mjs, feature "multi-session")
// execute against the REAL `rpi --listen` fixture. The validator compares the
// matrix against the executed-assertion evidence the drivers/lane write and
// fails when a required entry is missing, when an ID is duplicated across
// features, or when an executed ID is not declared anywhere in the matrix.
//
// Usage:
//   node E2E.d/web/coverage_matrix.mjs --evidence <a.json> [--evidence <b.json> ...]
//
// Evidence format (one file per driver/lane): { "executed": ["auth.no-token-probe", ...] }
//
// Exit codes: 0 = every required assertion executed, no duplicates;
//             2 = uncovered/duplicate/undeclared assertions.

import fs from 'node:fs';

export const MATRIX = [
  {
    feature: 'auth',
    ids: ['auth.no-token-probe', 'auth.wrong-token-toast', 'auth.good-token-connect'],
  },
  {
    feature: 'reconnect',
    ids: ['reconnect.pill', 'reconnect.auto-on', 'reconnect.transcript-survives', 'reconnect.roundtrip'],
  },
  {
    feature: 'prompt',
    ids: ['prompt.slow-stream-full', 'prompt.fast-roundtrip', 'prompt.new-message-count'],
  },
  {
    feature: 'abort',
    ids: ['abort.early-preserves-text', 'abort.no-tail-render', 'abort.neutral-toast', 'abort.no-error-toast'],
  },
  {
    feature: 'goal',
    ids: [
      'goal.empty-state',
      'goal.create',
      'goal.budget-usage',
      'goal.pin',
      'goal.pause-live',
      'goal.resume',
      'goal.journal-order',
    ],
  },
  {
    feature: 'markdown',
    ids: [
      'md.table-rendered',
      'md.task-list-glyph',
      'md.no-fence-leak',
      'md.no-separator-leak',
      'md.fence-rendered',
      'md.fence-copy',
      'md.link-safe',
      'md.link-blocked',
      'md.image-safe',
      'md.image-blocked',
    ],
  },
  {
    feature: 'mermaid',
    ids: ['mermaid.svg-rendered'],
  },
  {
    feature: 'katex',
    ids: ['katex.rendered'],
  },
  {
    feature: 'xss',
    ids: [
      'xss.inert-text',
      'xss.no-global',
      'xss.no-live-elements',
      'xss.secret-redacted',
      'xss.approval-card',
      'xss.approval-no-exec',
      'xss.approval-no-toast-leak',
      'xss.approval-secret-redacted',
    ],
  },
  {
    feature: 'todo',
    ids: [
      'todo.create-row',
      'todo.complete-status',
      'todo.counts-done',
      'todo.reopen-status',
      'todo.detail-pane',
      'todo.add-via-enter',
      'todo.dep-link',
      'todo.dep-unlink',
      'todo.detail-complete',
      'todo.detail-reopen',
      'todo.clear-selection',
    ],
  },
  {
    feature: 'workflow',
    ids: [
      'workflow.create-row',
      'workflow.live-status',
      'workflow.cancel-status',
      'workflow.create-via-enter',
      'workflow.select-row',
      'workflow.pause',
      'workflow.resume',
      'workflow.integrate',
      'workflow.remove',
    ],
  },
  {
    feature: 'settings',
    ids: [
      'settings.browse-category',
      'settings.secret-redacted-readonly',
      'settings.draft-dirty',
      'settings.apply-persisted',
      'settings.refresh',
      'settings.typed-boolean',
      'settings.typed-enum',
      'settings.typed-number',
      'settings.typed-json',
      'settings.reset',
      'settings.cancel-draft',
      'settings.close',
    ],
  },
  {
    feature: 'sessions',
    ids: [
      'session.sidebar-list',
      'session.rename-panel-header',
      'session.switch-new-id',
      'session.transcript-cleared',
      'session.history-listed',
    ],
  },
  {
    // The real sessions lane (E2E.d/web/sessions.sh -> sessions_test.mjs).
    // Its evidence file is written by the lane itself at
    // $EVIDENCE_ROOT/web-sessions/coverage-assertions.json; the gate fails if
    // any of these T0.1-T7.4 contracts did not execute.
    feature: 'multi-session',
    ids: [
      'T0.1', 'T0.2', 'T0.3',
      'T1.1', 'T1.2', 'T1.3', 'T1.4', 'T1.5', 'T1.6',
      'T2.1', 'T2.2', 'T2.3', 'T2.4', 'T2.5',
      'T3.1', 'T3.2',
      'T4.1', 'T4.2', 'T4.3',
      'T5.1', 'T5.2', 'T5.3',
      'T6.1', 'T6.2', 'T6.3', 'T6.4',
      'T7.1', 'T7.2', 'T7.3', 'T7.4',
    ],
  },
  {
    feature: 'concurrency',
    ids: ['concurrency.second-ws-rpc', 'concurrency.live-event-reflect', 'concurrency.second-page-connects'],
  },
  {
    feature: 'subagents',
    ids: [
      'subagent.spawn-live',
      'subagent.activity-line',
      'subagent.hub-send',
      'subagent.output-view',
      'subagent.cancel',
      'subagent.spawn-via-enter',
      'subagent.message-via-enter',
    ],
  },
  {
    feature: 'side-chat',
    ids: ['sidechat.default-tab', 'sidechat.new-tab', 'sidechat.tab-activated', 'sidechat.prompt-reply'],
  },
  {
    feature: 'maintenance',
    ids: ['maintenance.snapcompact-ab', 'maintenance.rewind-list', 'maintenance.handoff', 'maintenance.queue-cancel'],
  },
  {
    feature: 'desktop navigation',
    ids: ['nav.panel-open-close', 'nav.header-session-name'],
  },
  {
    feature: 'mobile navigation',
    ids: [
      'mobile.viewport-no-hscroll',
      'mobile.sidebar-toggle-visible',
      'mobile.drawer-opens',
      'mobile.composer-above-fold',
      'mobile.touch-targets-44',
      'mobile.thinking-hidden',
    ],
  },
  {
    feature: 'model/thinking switch',
    ids: ['switch.model-roundtrip', 'switch.thinking-roundtrip'],
  },
  {
    feature: 'app',
    ids: [
      'app.tool-card',
      'app.tool-card-done',
      'app.primary-submit-send',
      'app.primary-submit-steer',
      'app.maintenance-close',
    ],
  },
  {
    // Fallback coverage driver (E2E.d/web/coverage_fallback.mjs, lane
    // coverage-fallback) — closes the zero-hit margin on panels the core
    // steering driver does not exhaustively exercise. Evidence at
    // $EVIDENCE_DIR/driver-fallback/coverage-assertions.json. The `fallback.*`
    // namespace is disjoint from every other feature's prefixes.
    feature: 'fallback',
    ids: [
      'fallback.session-refresh',
      'fallback.session-rename-enter',
      'fallback.redact-credential-panel',
      'fallback.session-clone',
      'fallback.session-fork',
      'fallback.session-switch-row',
      'fallback.app-close-session',
      'fallback.goal-unpin',
      'fallback.app-close-goal',
      'fallback.maintenance-compact',
      'fallback.maintenance-rewind-apply',
      'fallback.app-close-maintenance',
      'fallback.sidechat-enter-prompt',
      'fallback.sidechat-tab-switch',
      'fallback.sidechat-tab-close',
      'fallback.app-close-sidechat',
    ],
  },
];

const ALL_IDS = MATRIX.flatMap((f) => f.ids);

function main() {
  const argv = process.argv.slice(2);
  const evidenceFiles = [];
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === '--evidence') evidenceFiles.push(argv[i + 1]);
  }
  if (evidenceFiles.length === 0) {
    console.error('usage: node coverage_matrix.mjs --evidence <executed-ids.json> [--evidence ...]');
    process.exit(2);
  }

  // 1. duplicate IDs inside the matrix are a hard failure.
  const seen = new Map();
  const duplicates = [];
  for (const { feature, ids } of MATRIX) {
    for (const id of ids) {
      if (seen.has(id)) duplicates.push(`${id} (${seen.get(id)} + ${feature})`);
      seen.set(id, feature);
    }
  }
  if (duplicates.length > 0) {
    console.error('[matrix] FAIL: duplicate assertion IDs across features:');
    for (const d of duplicates) console.error(`  - ${d}`);
    process.exit(2);
  }

  // 2. merge executed evidence.
  const executed = new Set();
  for (const file of evidenceFiles) {
    let data;
    try {
      data = JSON.parse(fs.readFileSync(file, 'utf8'));
    } catch (err) {
      console.error(`[matrix] FAIL: cannot read evidence ${file}: ${err.message}`);
      process.exit(2);
    }
    if (!Array.isArray(data.executed)) {
      console.error(`[matrix] FAIL: evidence ${file} lacks "executed" array`);
      process.exit(2);
    }
    for (const id of data.executed) executed.add(id);
  }
  if (executed.size === 0) {
    console.error('[matrix] FAIL: no executed assertions in evidence (drivers never ran)');
    process.exit(2);
  }

  // 3. required entries must be executed; executed IDs must be declared.
  const uncovered = [];
  for (const { feature, ids } of MATRIX) {
    const missing = ids.filter((id) => !executed.has(id));
    for (const id of missing) uncovered.push(id);
  }
  const undeclared = [...executed].filter((id) => !ALL_IDS.includes(id));

  // 4. report.
  console.log('\n[matrix] feature -> assertion coverage:');
  let requiredTotal = 0;
  let executedTotal = 0;
  for (const { feature, ids } of MATRIX) {
    const done = ids.filter((id) => executed.has(id)).length;
    requiredTotal += ids.length;
    executedTotal += done;
    const mark = done === ids.length ? 'ok' : 'UNCOVERED';
    console.log(`  ${feature.padEnd(24)} ${String(done).padStart(2)}/${String(ids.length).padEnd(2)} ${mark}`);
  }
  console.log(`\n[matrix] totals: ${executedTotal}/${requiredTotal} required assertions executed`);
  if (undeclared.length > 0) {
    console.log(`[matrix] warning: ${undeclared.length} executed id(s) not declared in the matrix (drift): ${undeclared.join(', ')}`);
  }

  let failed = false;
  if (uncovered.length > 0) {
    console.error('\n[matrix] FAIL: required assertions never executed:');
    for (const id of uncovered) console.error(`  - ${id}`);
    failed = true;
  }
  if (undeclared.length > 0) {
    console.error('\n[matrix] FAIL: executed assertions not declared in the matrix (matrix drift):');
    for (const id of undeclared) console.error(`  - ${id}`);
    failed = true;
  }
  if (failed) process.exit(2);
  console.log('[matrix] PASS: zero uncovered required assertions, zero duplicates, zero undeclared');
}

main();
