#!/usr/bin/env node
// Web feature -> Playwright assertion matrix + validator (hard coverage gate).
//
// The matrix maps every required Web contract to concrete assertion IDs that
// the coverage drivers (E2E.d/web/coverage_test.mjs, coverage_xss.mjs)
// execute against the REAL `rpi --listen` fixture. The validator compares the
// matrix against the executed-assertion evidence the drivers write and fails
// when a required entry is missing, when an ID is duplicated across features,
// or when an executed ID is not declared anywhere in the matrix.
//
// Usage:
//   node E2E.d/web/coverage_matrix.mjs --evidence <a.json> [--evidence <b.json> ...]
//
// Evidence format (one file per driver): { "executed": ["auth.no-token-probe", ...] }
//
// Exit codes: 0 = every required assertion executed, no duplicates;
//             2 = uncovered/duplicate/undeclared assertions.

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
    feature: 'markdown',
    ids: ['md.table-rendered', 'md.task-list-glyph', 'md.no-fence-leak', 'md.no-separator-leak'],
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
    ids: ['todo.create-row', 'todo.complete-status', 'todo.counts-done', 'todo.reopen-status', 'todo.detail-pane'],
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
    feature: 'workflow',
    ids: ['workflow.create-row', 'workflow.live-status', 'workflow.cancel-status'],
  },
  {
    feature: 'settings',
    ids: ['settings.browse-category', 'settings.secret-redacted-readonly', 'settings.draft-dirty', 'settings.apply-persisted'],
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
    feature: 'concurrency',
    ids: ['concurrency.second-ws-rpc', 'concurrency.live-event-reflect', 'concurrency.second-page-connects'],
  },
  {
    feature: 'subagents',
    ids: ['subagent.spawn-live', 'subagent.activity-line', 'subagent.hub-send', 'subagent.output-view', 'subagent.cancel'],
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
      data = JSON.parse(require('node:fs').readFileSync(file, 'utf8'));
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
    for (const id of missing) uncovered.push(`${feature}.${id}`);
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
