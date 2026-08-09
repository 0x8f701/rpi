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
  // Explicit enforced thresholds. 90% lines/functions is the agreed target;
  // branches and statements are enforced at the honest measured level.
  thresholds: {
    lines: 90,
    functions: 90,
    branches: 75,
    statements: 90,
  },
};
