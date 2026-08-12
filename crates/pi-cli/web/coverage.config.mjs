// Web coverage configuration — read by scripts/coverage-report.mjs and the
// hard coverage command (E2E.d/web/coverage.sh).
//
// Coverage is measured on the real web client sources (src/**/*.{ts,tsx})
// through a real `rpi --listen` fixture + real Playwright assertions. Totals
// are Istanbul line/function/branch/statement percentages over the included
// files only (node_modules and the dist bundle are never counted).
export default {
  // Relative to crates/pi-cli/web.
  include: ['src/**/*.{ts,tsx}'],
  exclude: [
    // Type-only module: no runtime statements, so it never appears in V8
    // coverage and would otherwise trip the expected-file check.
    'src/types.ts',
  ],
  // Per-file hard thresholds. Each entry keys on the source path relative to
  // crates/pi-cli/web (e.g. "src/scrollPin.ts") and is enforced AFTER the
  // global totals, against the SAME source-mapped Istanbul per-file summary
  // the per-file table prints. A metric listed here must be met, or the
  // coverage command fails (exit 2) — there is no per-file skip, no ignore,
  // and the global thresholds below are unchanged.
  //
  // scrollPin.ts — the streaming-transcript bottom-pin state machine + hook.
  // The scroll regression (stream deltas via direct textContent DOM mutation
  // bypassing the activeItems effect) is root-caused here, so it carries its
  // own hard gate instead of hiding in the global average. All four metrics
  // are gated at the spec target (>=90%): the refactored module is a DOM-free
  // decision core (remainingToBottom / isPinned / createScrollPin) plus a
  // thin useScrollPin hook, and the real browser lanes exercise every branch
  // that is reachable in a real app:
  //   - followIfPinned's `el && pinned` short-circuit — both blocks hit
  //     (pinned deltas take the true path; unpinned deltas take the false
  //     path during the scroll lane's scroll-away phase).
  //   - hook pinIfPinned / forcePin / onTranscriptScroll (event.currentTarget,
  //     so no unreachable null guard), lazy-init `pinRef.current === null`
  //     (both renders), pure isPinned/remainingToBottom, and the
  //     ResizeObserver callback (resizes fire while streaming in the scroll
  //     lane, including container clientHeight changes).
  // The only strictly browser-uncoverable block is forcePin's `if (!el)
  // return false` null path (App calls forcePin post-mount with refs
  // attached — unit-test-covered, not E2E-covered); with ~40-60 V8 blocks /
  // 14 functions the honest worst-case measured through the existing lanes is
  // ~93-95% branches and ~92.9% functions, so 90% on every metric holds with
  // margin and is the "reliable" level the spec's "branches 若可靠也 >=90%"
  // clause calls for. Two extra driver phases (a malformed /collab/ws URL
  // that drives CollabGuestView's !link early return so the ResizeObserver
  // effect's null guard runs, and a page-reload that runs the observer
  // disconnect cleanup) would raise the margin to ~96%+/100% but are NOT
  // required to clear 90 — they belong to the coverage-driver owners, not
  // this config.
  fileThresholds: {
    'src/scrollPin.ts': {
      lines: 90,
      functions: 90,
      branches: 90,
      statements: 90,
    },
  },
  // Explicit enforced thresholds. 90% lines/functions is the agreed target;
  // branches and statements are enforced at the honest measured level.
  thresholds: {
    lines: 90,
    functions: 90,
    branches: 75,
    statements: 90,
  },
};