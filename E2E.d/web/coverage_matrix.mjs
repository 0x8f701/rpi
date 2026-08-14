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
    // The real loop_goal lane (E2E.d/web/loop_goal.sh -> loop_goal_test.mjs):
    // composer picker /loop + /goal, draft-no-auto-submit, real-listener RPC
    // execution (loop create/list/update/delete/cancel, goal
    // create/show/pin/pins/pause/resume/complete/drop with activate-work TUI
    // parity), the /loop streaming guard, local usage errors with no RPC, no
    // user bubbles, and sessionId-stamped routing. Its evidence file is
    // written by the lane itself at
    // $EVIDENCE_ROOT/web-loop_goal/coverage-assertions.json; the gate fails
    // if any of these lg.* contracts did not execute.
    feature: 'loop + goal composer',
    ids: [
      'lg.picker-lists-loop-goal',
      'lg.loop-draft-no-submit',
      'lg.goal-draft-no-submit',
      'lg.goal-panel-open',
      'lg.ps-picker-listed',
      'lg.ps-draft-no-submit',
      'lg.ps-empty-list',
      'lg.ps-args-local-reject',
      'lg.ps-session-routed',
      'lg.loop-create',
      'lg.loop-streaming-guard',
      'lg.loop-list',
      'lg.loop-update',
      'lg.loop-delete',
      'lg.loop-cancel-error',
      'lg.goal-create-activate',
      'lg.goal-show',
      'lg.goal-pin',
      'lg.goal-pins',
      'lg.goal-pause',
      'lg.goal-resume-activate',
      'lg.goal-complete',
      'lg.goal-drop-error',
      'lg.usage-error-no-rpc',
      'lg.draft-preserved-on-error',
      'lg.alias-no-prompt',
      'lg.no-user-bubbles',
      'lg.session-routed',
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
      'md.extra-blockquote',
      'md.extra-hr',
      'md.extra-ol',
      'md.extra-nested-list',
      'md.extra-fence-langs',
      'md.extra-mermaid-empty',
      'md.extra-mermaid-error',
      'md.extra-mermaid-no-residue',
      'md.extra-currency',
      'md.extra-image-relative',
      'md.extra-url-policy',
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
    feature: 'slash commands',
    ids: [
      'slash.snapcompact',
      'slash.compact-llm',
      'slash.compact-bare',
      'slash.snapcompact-tail',
      'slash.skill-usage-error',
      'slash.skill-rpc',
      'slash.code-review-open',
      'slash.code-review-close',
      'slash.code-review-range',
      'slash.code-review-usage-error',
      'slash.unknown-falls-through',
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
      'T4.1', 'T4.2', 'T4.3',
      'T5.1', 'T5.2', 'T5.3',
      'T6.1', 'T6.2', 'T6.3', 'T6.4',
      'T7.1', 'T7.2', 'T7.3', 'T7.4',
    ],
  },
  {
    // The real projects lane (E2E.d/web/projects.sh -> projects_test.mjs):
    // all-project native session catalog + cross-project New-session storage
    // (seeded A/B sessions under one profile tree, project-B switch, New
    // records under B's encoded default dir, on-disk proof). Its evidence
    // file is written by the lane itself at
    // $EVIDENCE_ROOT/web-projects/coverage-assertions.json; the gate fails
    // if any of these P0.1-P4.2 contracts did not execute.
    feature: 'all-project session catalog',
    ids: [
      'P0.1', 'P0.2', 'P0.3',
      'P1.1', 'P1.2',
      'P2.1', 'P2.2', 'P2.3',
      'P3.1', 'P3.2', 'P3.3', 'P3.4',
      'P4.1', 'P4.2',
    ],
  },
  {
    // The real external_sessions lane (E2E.d/web/external_sessions.sh ->
    // external_sessions_test.mjs): Web-only default OMP/Codex/Grok discovery
    // + source grouping, foreign click imports a native rpi copy, OMP
    // rotation chain (parentSession early/middle/final history) renders fully
    // and once with task/subagent child sessions excluded, foreign
    // bytes/mtime immutable, lineage reuse, no duplicate rows, and explicit
    // sessionImportSources:[] native-only. Evidence at
    // $EVIDENCE_ROOT/web-external_sessions/coverage-assertions.json.
    feature: 'external sessions',
    ids: [
      'X0.1',
      'X1.1', 'X1.2',
      'X2.1', 'X2.2', 'X2.3',
      'X3.1', 'X3.2',
      'X4.1', 'X4.2',
      'X5.1', 'X5.2',
      'X7.1', 'X7.2', 'X7.3', 'X7.4',
      'X8.1',
      'X6.1', 'X6.2',
    ],
  },
  {
    // The real scroll lane (E2E.d/web/scroll.sh -> scroll_test.mjs). Its
    // evidence file is written by the lane itself at
    // $EVIDENCE_ROOT/web-scroll/coverage-assertions.json; the gate fails if
    // any documented S0.1-S7.3 contract did not execute.
    feature: 'scroll pinning',
    ids: [
      'S0.1', 'S0.2', 'S0.3',
      'S1.1',
      'S2.1', 'S2.2', 'S2.3',
      'S3.1', 'S3.2', 'S3.3',
      'S4.1',
      'S5.1', 'S5.2',
      'S6.1', 'S6.2', 'S6.3',
      'S7.1', 'S7.2', 'S7.3',
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
    // Real Codex Live realtime flow (coverage_test.mjs): the App's
    // real startRealtime -> setupRealtimeCall path with stubbed WebRTC
    // platform APIs, a REAL realtime_create_call RPC through the Rust proxy
    // to the mock's /v1/realtime/calls endpoint (OpenAI-Alpha + Bearer
    // asserted), oai-events transcript delta/final (deduped), error toast and
    // delegation frames, and the stop path.
    feature: 'realtime',
    ids: [
      'realtime.overlay-visible',
      'realtime.proxy-alpha-header',
      'realtime.transcript-delta',
      'realtime.transcript-commit',
      'realtime.transcript-dedup',
      'realtime.error-toast',
      'realtime.delegation',
      'realtime.stop',
      'realtime.conn-state-connected',
      'realtime.session-update',
      'realtime.error-message-only',
      'realtime.error-code-only',
      'realtime.error-bare',
      'realtime.dc-error',
      'realtime.conn-disconnected',
      'realtime.conn-failed-teardown',
    ],
  },
  {
    feature: 'app',
    ids: [
      'app.tool-card',
      'app.tool-card-done',
      'app.tool-card-settled',
      'app.tool-card-web-search',
      'app.tool-card-edit',
      'app.tool-card-reconnect',
      'app.primary-action-send',
      'app.primary-action-stop',
    ],
  },
  {
    // Real-wire transcript/tool-title branch journeys (coverage_test.mjs
    // Phase P): bash_execution_end status/output-bounding variants via the
    // raw second-session Bash RPC, and unknown tool names whose wire values
    // exercise humanToolTitle's Title-Case / acronym / credential-redaction
    // branches through real tool_execution dispatch plus the generic
    // fallback tool-card view (compact args line, collapsed raw, dispatch
    // error). Also the structured Todo tool-card contract: a real `todo`
    // tool call renders the phase/task list in both the running frame
    // (init-args projection) and the settled frame (details.phases), never
    // the raw args JSON as the default view. The `wire.*` namespace is
    // disjoint from every other feature's prefixes.
    feature: 'wire transcript entries',
    ids: [
      'wire.bash-done',
      'wire.bash-empty',
      'wire.bash-error',
      'wire.bash-bound-tail',
      'wire.title-snake',
      'wire.title-acronym',
      'wire.title-kebab',
      'wire.tool-error-card',
      // Real `todo` tool execution journey (coverage_test.mjs Phase P): the
      // structured Todo card — running frame projects the init args via
      // resolveTodoCardView (phase/task list with typed status markers, never
      // the raw args JSON as the default view), settled frame projects
      // details.phases via parseTodoPhases.
      'wire.todo-card-running',
      'wire.todo-structured-view',
    ],
  },
  {
    // Presentation regression lane (E2E.d/web/presentation.sh ->
    // presentation_test.mjs): real-browser DOM assertions (computed styles,
    // bounding rects, naturalWidth, video attributes — never source text) for
    // the Command/process/write/read tool-card presentation, the Thinking
    // streaming lifecycle, the composer control equal-height, the session
    // sidebar provider grouping + search, and the inline image/video media
    // contracts. Includes the Thinking + toolcall no-raw-JSON guard: a
    // think+bash turn must never render the removed `.assistant-toolcall`
    // surface or raw `{"command":…}` args — in-flight, after finalize, nor as
    // a transient element — with the structured Command card the only tool
    // presentation (MutationObserver watchdog). Evidence at
    // $EVIDENCE_ROOT/web-presentation/coverage-assertions.json. The `pres.*`
    // namespace is disjoint from every other feature's prefixes.
    feature: 'presentation',
    ids: [
      'pres.composer-equal-height',
      'pres.composer-equal-height-mobile',
      'pres.command-title',
      'pres.command-no-bash-title',
      'pres.command-no-default-args',
      'pres.command-success-green',
      'pres.command-two-line-clamp',
      'pres.command-failure-red',
      'pres.write-summary',
      'pres.read-summary',
      'pres.image-render',
      'pres.process-long',
      'pres.process-short',
      'pres.process-summary',
      'pres.process-equal-width',
      'pres.process-error-op',
      'pres.no-raw-json',
      'pres.no-done-text',
      'pres.tool-cards-equal-width',
      'pres.hub-wait-running',
      'pres.hub-wait-timeout',
      'pres.hub-wait-typed-running',
      'pres.hub-wait-typed-irc',
      'pres.hub-send-card',
      'pres.thinking-streaming-visible',
      'pres.thinking-header-icon',
      'pres.thinking-no-bare-marker',
      'pres.thinking-multiline-body',
      'pres.thinking-final-hidden',
      // Thinking + streamed tool call regression journey: a turn streaming
      // reasoning AND a bash tool call (incremental tool_calls fragments)
      // shows the reasoning prose, then the structured Command card — never
      // the raw `{"command":…}` JSON/args, in-flight, after finalize, or as a
      // transient element (MutationObserver watchdog proves zero raw surface).
      'pres.tb.thinking-prose',
      'pres.tb.card-live-no-raw',
      'pres.tb.card-done-no-raw',
      'pres.thinking-narrow-no-overflow',
      'pres.bash-execution-durable',
      'pres.session-providers-only',
      'pres.session-no-tmp-uuid',
      'pres.session-search-filter',
      'pres.session-search-clear',
      'pres.video-controls',
      'pres.media-hostile-rejected',
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
