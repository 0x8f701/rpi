#!/usr/bin/env node
// Web feature -> Playwright assertion matrix + validator (hard coverage gate).
//
// The matrix maps every required Web contract to concrete assertion IDs that
// the coverage drivers (E2E.d/web/coverage_test.mjs, coverage_xss.mjs,
// coverage_fallback.mjs) and the lanes that publish executed-assertion
// ledgers (sessions, scroll, projects, presentation, loop_goal,
// session_restore, the File Explorer block of commands_review_test.mjs, and
// the pointer-publishing sessions_workflow / files_latency /
// external_sessions lanes) execute against the
// REAL `rpi --listen` fixture. The validator compares the matrix against the
// executed-assertion evidence the drivers/lane write and
// fails when a required entry is missing, when an ID is duplicated across
// features, or when an executed ID is not declared anywhere in the matrix.
//
// Usage:
//   node E2E.d/web/coverage_matrix.mjs --evidence <a.json> [--evidence <b.json> ...]
//
// Evidence format (one file per driver/lane): the FULL exact ledger —
// { "documented": ["auth.no-token-probe", ...], "executed": [...same ids in
// order...], "counts": {"auth.no-token-probe": 1, ...} }. A ledger carrying
// only { executed: [...] } is a hard failure.
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
      // IN THE LANE'S ACTUAL ASSERTION-SITE (runtime) ORDER — the matrix
      // requires the lane's published executed array to equal its documented
      // array in order.
      'lg.picker-lists-loop-goal',
      'lg.ps-picker-listed',
      'lg.loop-draft-no-submit',
      'lg.goal-draft-no-submit',
      'lg.ps-draft-no-submit',
      'lg.ps-empty-list',
      'lg.goal-panel-open',
      'lg.loop-create',
      'lg.loop-streaming-guard',
      'lg.goal-create-activate',
      'lg.goal-show',
      'lg.goal-pin',
      'lg.goal-pins',
      'lg.goal-pause',
      'lg.goal-resume-activate',
      'lg.goal-complete',
      'lg.goal-drop-error',
      'lg.loop-list',
      'lg.loop-update',
      'lg.loop-delete',
      'lg.loop-cancel-error',
      'lg.ps-args-local-reject',
      'lg.draft-preserved-on-error',
      'lg.alias-no-prompt',
      'lg.usage-error-no-rpc',
      'lg.no-user-bubbles',
      'lg.session-routed',
      'lg.ps-session-routed',
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
      'xss.approval-secret-redacted',
      'xss.approval-no-toast-leak',
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
      'todo.all-rows-counts-converged',
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
      'slash.code-review-base',
      'slash.code-review-usage-error',
      'slash.unknown-falls-through',
    ],
  },
  {
    feature: 'sessions',
    ids: [
      'session.sidebar-list',
      'session.rename-panel-sidebar',
      'session.switch-new-id',
      'session.transcript-cleared',
      'session.history-listed',
    ],
  },
  {
    // The real sessions lane (E2E.d/web/sessions.sh -> sessions_test.mjs).
    // Its evidence file is written by the lane itself at
    // $EVIDENCE_ROOT/web-sessions/coverage-assertions.json; the gate fails if
    // any of these T0.1-T8.3 contracts did not execute.
    feature: 'multi-session',
    ids: [
      // IN ACTUAL EXECUTION (record-site) ORDER — identical to the lane's
      // DOCUMENTED_IDS: T3 attachment cutover before T2 abort isolation, the
      // T8 fork block before the T4 LRU block, and the mobile T7 block last
      // (its header check T7.4 executes before T7.1-T7.3).
      'T0.1', 'T0.2', 'T0.3',
      'T1.1', 'T1.2', 'T1.3', 'T1.4', 'T1.5', 'T1.6',
      'T3.1', 'T3.2',
      'T2.1', 'T2.2', 'T2.3', 'T2.4', 'T2.5',
      'T6.1', 'T6.2', 'T6.3', 'T6.4',
      'T8.1', 'T8.2', 'T8.3',
      'T4.1', 'T4.2', 'T4.3',
      'T5.1', 'T5.2', 'T5.3',
      'T7.4', 'T7.1', 'T7.2', 'T7.3',
    ],
  },
  {
    // The real projects lane (E2E.d/web/projects.sh -> projects_test.mjs):
    // all-project native session catalog + cross-project New-session storage
    // (seeded A/B sessions under one profile tree, project-B switch,
    // explicit-directory New records under the selected project's encoded
    // default dir, on-disk proof). Its evidence
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
    // external_sessions_test.mjs): Web-default Pi/OMP/Codex/Grok discovery
    // + source grouping, foreign clicks import native rpi copies, selected-row
    // HTML export downloads native and Pi content without activating the row,
    // OMP rotation history renders fully once with child sessions excluded,
    // foreign bytes/mtime remain immutable, lineage is reused, duplicate rows
    // stay absent, and sessionImportSources:[] is native-only. Evidence is
    // ONE lane-set commit: both phases stage into one shared nonce-derived
    // staging set, the shell merges the exact per-phase counts + exact full
    // union inside the shared stage, then ONE stage-dir->run-dir rename +
    // ONE current.json pointer; coverage.sh resolves the ledger through the
    // pointer at $EVIDENCE_ROOT/web-external_sessions/current.json.
    feature: 'external sessions',
    ids: [
      'X0.1',
      'X1.1', 'X1.2',
      'X11.1', 'X11.2', 'X11.3', 'X11.4',
      'X2.1', 'X2.2', 'X2.3',
      'X3.1', 'X3.2',
      'X4.1', 'X4.2',
      'X5.1', 'X5.2',
      'X7.1', 'X7.2', 'X7.3', 'X7.4',
      'X9.1', 'X9.2', 'X9.3',
      'X10.1', 'X10.2', 'X10.3',
      'X8.1',
      'X6.1', 'X6.2',
    ],
  },
  {
    // The real sessions_workflow lane (E2E.d/web/sessions_workflow.sh ->
    // sessions_workflow_test.mjs): RPC-only workflow-DAG acceptance —
    // fail-closed planning probe lands TERMINAL failed with a retained
    // non-empty reason, healthy compile->ship DAG completes with an applied
    // integration, held waves show live progress (never completed while a
    // task is open), exact-once counts + task ownership, workflow_list
    // session isolation, completed terminal across re-polls, and panel
    // Cancel/Remove single-flight with zero stale-response resurrections.
    // Evidence at the run dir named by
    // $EVIDENCE_ROOT/web-sessions-workflow/current.json
    // (coverage-assertions.json inside it), resolved by coverage.sh
    // through the pointer; the gate fails if any documented WF0.1-WF5.3
    // contract did not execute exactly once. The `WF` prefix is disjoint
    // from every other feature's prefixes.
    feature: 'workflow DAG acceptance',
    ids: [
      'WF0.1', 'WF0.2', 'WF0.3', 'WF0.4',
      'WF1.1', 'WF1.2',
      'WF2.1', 'WF2.2', 'WF2.3', 'WF2.4', 'WF2.5',
      'WF3.1', 'WF3.2',
      'WF4.1', 'WF4.2',
      'WF5.1', 'WF5.2', 'WF5.3',
    ],
  },
  {
    // The real session_restore lane (E2E.d/web/session_restore.sh ->
    // session_restore_test.mjs): listener SIGTERM/restart re-bind + disk
    // restore + switch correlation (R4.*), page-authority preference reload
    // with per-authority isolation (R5.*), and missing-preference
    // first-catalog-row fallback (R6.*). The lane publishes the FULL
    // documented/executed/counts ledger inside the immutable run dir named
    // by $EVIDENCE_ROOT/web-session-restore/current.json (resolved by
    // coverage.sh through the pointer), in its ACTUAL execution order
    // (R5 -> R6 -> R4; within R4 every contract completes in ID order
    // R4.1..R4.5 — R4.3 records at its actual contract site, the
    // disk-backed parity history restore) — registered here in that same
    // order. The `R` prefix is disjoint from every other feature's
    // prefixes.
    feature: 'session restore',
    ids: [
      'R5.1', 'R5.2', 'R5.3',
      'R6.1', 'R6.2', 'R6.3',
      'R4.1', 'R4.2', 'R4.3', 'R4.4', 'R4.5',
    ],
  },
  {
    // The real files_latency lane (E2E.d/web/files_latency.sh ->
    // files_latency_test.mjs): Files-drawer directory-toggle latency —
    // real pointer click -> one file_explorer_list RPC -> DOM commit by
    // the first animation frame -> exact correlated success response AFTER
    // the commit; zero listing RPCs on collapse/re-expand; the
    // 1500-message transcript parent never re-renders during the toggle;
    // FL6 clicks a transcript link in a fresh subdirectory-cwd session
    // (not the latency parent): exact nested content, unique nearby
    // basename, inert duplicate basename, inert out-of-workspace, the root
    // bare-basename README.md:2 citation stamped (never
    // data-md-resolved="none") and opened via the session-stamped read; zero
    // captured browser errors. The lane publishes explicit exact assertion
    // IDs/counts (FL0.1-FL6.5) in the lane's ACTUAL execution order
    // (FL1.2's mutation-ledger observation window settles before FL1.1's
    // timing gate) alongside the timings — never timings-only.
    // Evidence at the run dir named by
    // $EVIDENCE_ROOT/web-files-latency/current.json, resolved by
    // coverage.sh through the pointer. The `FL` prefix is disjoint from
    // every other feature's prefixes.
    feature: 'files drawer latency',
    ids: [
      'FL0.1',
      'FL1.2', 'FL1.1',
      'FL2.1',
      'FL3.1', 'FL3.2',
      'FL4.1', 'FL4.2',
      'FL6.1', 'FL6.2', 'FL6.3', 'FL6.4', 'FL6.5',
      'FL5.1',
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
      'S3.1', 'S3.3', 'S3.2',
      'S4.1',
      'S5.1', 'S5.2',
      'S6.1', 'S6.3', 'S6.2',
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
    ids: ['nav.panel-open-close', 'nav.header-no-duplicate-session'],
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
      'mobile.header-no-duplicate-session',
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
      'realtime.session-update-transcription',
      'realtime.transcript-delta',
      'realtime.transcript-segment',
      'realtime.transcript-commit',
      'realtime.composer-final-screenshot',
      'realtime.transcript-dedup',
      'realtime.transcription-failed-toast',
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
      // IN THE LANE'S ACTUAL ASSERTION-SITE (runtime) ORDER — the matrix
      // requires the lane's published executed array to equal its documented
      // array in order.
      'pres.composer-equal-height',
      'pres.command-title',
      'pres.command-no-bash-title',
      'pres.command-no-default-args',
      'pres.command-success-green',
      'pres.command-two-line-clamp',
      'pres.command-failure-red',
      'pres.write-summary',
      'pres.read-summary',
      'pres.image-render',
      'pres.grep-oversized-single-call',
      'pres.grep-doom-loop-identity',
      'pres.process-long',
      'pres.process-short',
      'pres.process-summary',
      'pres.process-equal-width',
      'pres.process-error-op',
      'pres.hub-wait-running',
      'pres.activity-tool-visible',
      'pres.hub-wait-timeout',
      'pres.hub-wait-typed-running',
      'pres.hub-wait-typed-irc',
      'pres.hub-send-card',
      'pres.no-raw-json',
      'pres.no-done-text',
      'pres.tool-cards-equal-width',
      'pres.activity-reduced-motion-static',
      'pres.activity-thinking-visible',
      'pres.thinking-streaming-visible',
      'pres.thinking-header-icon',
      'pres.thinking-no-bare-marker',
      'pres.thinking-multiline-body',
      'pres.thinking-final-fold',
      // Thinking + streamed tool call regression journey: a turn streaming
      // reasoning AND a bash tool call (incremental tool_calls fragments)
      // shows the reasoning prose, then the structured Command card — never
      // the raw `{"command":…}` JSON/args, in-flight, after finalize, or as a
      // transient element (MutationObserver watchdog proves zero raw surface).
      'pres.tb.thinking-prose',
      'pres.activity-thinking-tool-precedence',
      'pres.tb.card-live-no-raw',
      'pres.tb.card-done-no-raw',
      'pres.thinking-narrow-no-overflow',
      'pres.activity-mobile-no-overflow',
      'pres.video-controls',
      'pres.media-hostile-rejected',
      'pres.bash-execution-durable',
      'pres.host-switch-hidden-default',
      'pres.session-providers-only',
      'pres.session-no-tmp-uuid',
      'pres.session-search-filter',
      'pres.session-search-clear',
      'pres.host-switch-hidden-mobile',
      'pres.composer-equal-height-mobile',
    ],
  },
  {
    // Fallback coverage driver (E2E.d/web/coverage_fallback.mjs, lane
    // coverage-fallback) — closes the zero-hit margin on panels the core
    // steering driver does not exhaustively exercise. Evidence at
    // $EVIDENCE_DIR/driver-fallback/coverage-assertions.json. The `fallback.*`
    // namespace is disjoint from every other feature's prefixes.
    // fork/clone observables follow the current panel contract (the panel
    // never renders an in-panel status): fork = session-id/active identity
    // change on the remounted panel; clone (refused by the fixture) = exactly
    // one bottom-right error toast naming "clone failed:" and zero panel
    // status. Evidence IDs are unchanged — the matrix gate needs them exact.
    feature: 'fallback',
    ids: [
      'fallback.session-refresh',
      'fallback.session-rename-enter',
      'fallback.redact-credential-panel',
      'fallback.session-fork',
      'fallback.session-clone',
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
  {
    // Real File Explorer browser lane (E2E.d/web/commands_review.sh ->
    // commands_review_test.mjs). Evidence at the immutable run dir named by
    // $EVIDENCE_ROOT/web-commands-review/current.json (coverage-assertions.json
    // inside it), resolved by coverage.sh through the pointer. The
    // `files.*` namespace is disjoint from every other feature's prefixes.
    feature: 'file explorer',
    ids: [
      'files.boot',
      'files.desktop-open',
      'files.git-tree',
      'files.tree-split',
      'files.code-scroll',
      'files.markdown-scroll',
      'files.download',
      'files.hide-reclaim',
      'files.reopen',
      'files.tablet',
      'files.ask',
      'files.selection',
      'files.map',
      'files.fullscreen',
      'files.mobile',
      'files.selection-cache-compaction',
      'files.zero-errors',
    ],
  },
  {
    // runtime_matrix lane (17-session eviction/resume release matrix). The
    // `RM.*` namespace is disjoint from every other feature's prefixes.
    feature: 'runtime release matrix',
    ids: [
      'RM0.1', 'RM0.2',
      'RM1.1',
      'RM2.1', 'RM2.2', 'RM2.3', 'RM2.4',
      'RM3.1', 'RM3.2',
      'RM4.1', 'RM4.2', 'RM4.3',
      'RM5.1', 'RM5.2',
      'RM6.1', 'RM6.2', 'RM6.3',
    ],
  },
  {
    // thinking_level lane (#thinking-select persistence). The `tl.`
    // namespace is disjoint from every other feature's prefixes.
    feature: 'thinking-level persistence',
    ids: [
      'tl.catalog-populated',
      'tl.session-identity',
      'tl.initial-level-displayed',
      'tl.idle-exactly-one-frame',
      'tl.idle-response-effective',
      'tl.idle-authoritative-state',
      'tl.idle-remains-selected',
      'tl.idle-no-error-toast',
      'tl.stream-first-delta',
      'tl.stream-exactly-one-frame',
      'tl.stream-response-effective',
      'tl.stream-remains-selected',
      'tl.stream-no-error-toast',
      'tl.stream-turn-settles',
      'tl.stream-authoritative-state',
      'tl.stream-followup-exactly-one-frame',
      'tl.stream-followup-response-effective',
      'tl.stream-followup-authoritative-state',
      'tl.stream-followup-no-error-toast',
      'tl.next-prompt-roundtrip',
      'tl.next-prompt-live-level',
      'tl.next-prompt-no-error-toast',
      'tl.reload-restores-level',
      'tl.fresh-session-created',
      'tl.fresh-session-level-loaded',
      'tl.session-b-exactly-one-frame',
      'tl.session-b-response-effective',
      'tl.session-b-authoritative-state',
      'tl.switch-back-a-preserved',
      'tl.switch-back-b-preserved',
      'tl.switch-no-error-toast',
      'tl.reload-restores-b',
      'tl.switch-a-after-reload-restored',
      'tl.no-browser-errors',
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

  // 2. consume every evidence ledger with the FULL contract — EVERY ledger
  // (driver or lane) must carry documented/executed/counts; a Set/simple
  // { executed: [...] }-only ledger is a hard failure, never accepted. The
  // executed ids must equal the documented ids IN ORDER (ordered equality,
  // never sorted-set comparison), every documented id must count exactly
  // once, duplicate ids within a ledger are hard failures, and an id
  // appearing in TWO ledgers is a hard failure (cross-ledger duplicates are
  // never merged away — no dedupe anywhere).
  const executedAcrossLedgers = new Map(); // id -> ledger where it first executed
  for (const file of evidenceFiles) {
    let data;
    try {
      data = JSON.parse(fs.readFileSync(file, 'utf8'));
    } catch (err) {
      console.error(`[matrix] FAIL: cannot read evidence ${file}: ${err.message}`);
      process.exit(2);
    }
    if (!Array.isArray(data.executed) || data.executed.some((id) => typeof id !== 'string' || id === '')) {
      console.error(`[matrix] FAIL: evidence ${file} lacks a non-empty-string "executed" array`);
      process.exit(2);
    }
    if (!Array.isArray(data.documented) || data.documented.some((id) => typeof id !== 'string' || id === '')) {
      console.error(`[matrix] FAIL: evidence ${file} lacks a non-empty-string "documented" array (every ledger must publish the full documented/executed/counts shape)`);
      process.exit(2);
    }
    if (typeof data.counts !== 'object' || data.counts === null || Array.isArray(data.counts)) {
      console.error(`[matrix] FAIL: evidence ${file} lacks an object "counts" (per-ID exact-once map required)`);
      process.exit(2);
    }
    const documentedDuplicates = data.documented.filter((id, index) => data.documented.indexOf(id) !== index);
    if (documentedDuplicates.length > 0) {
      console.error(`[matrix] FAIL: evidence ${file} documents duplicate ids: ${[...new Set(documentedDuplicates)].join(', ')}`);
      process.exit(2);
    }
    // ORDERED equality: executed[i] === documented[i] for every i — never a
    // sorted comparison that would silently accept reordered or swapped ids.
    if (data.executed.length !== data.documented.length
      || data.executed.some((id, index) => id !== data.documented[index])) {
      console.error(`[matrix] FAIL: evidence ${file} executed ids are not exactly the documented ids in order (documented=${data.documented.length}, executed=${data.executed.length})`);
      process.exit(2);
    }
    const withinLedger = new Map();
    for (const id of data.executed) withinLedger.set(id, (withinLedger.get(id) || 0) + 1);
    const repeatedWithin = [...withinLedger].filter(([, count]) => count > 1).map(([id]) => id);
    if (repeatedWithin.length > 0) {
      console.error(`[matrix] FAIL: evidence ${file} executes ids more than once (duplicate execution): ${repeatedWithin.join(', ')}`);
      process.exit(2);
    }
    const countKeys = Object.keys(data.counts);
    if (countKeys.length !== data.documented.length
      || countKeys.some((id) => !data.documented.includes(id))
      || countKeys.some((id) => data.counts[id] !== 1)) {
      console.error(`[matrix] FAIL: evidence ${file} counts are not exact-once over the documented ids: ${JSON.stringify(data.counts)}`);
      process.exit(2);
    }
    for (const id of data.executed) {
      if (executedAcrossLedgers.has(id)) {
        console.error(`[matrix] FAIL: executed id ${id} appears in multiple ledgers (${executedAcrossLedgers.get(id)} + ${file}) — cross-ledger duplicates are hard failures, never merged away`);
        process.exit(2);
      }
      executedAcrossLedgers.set(id, file);
    }
  }
  const executed = executedAcrossLedgers;
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
  const undeclared = [...executed.keys()].filter((id) => !ALL_IDS.includes(id));

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
