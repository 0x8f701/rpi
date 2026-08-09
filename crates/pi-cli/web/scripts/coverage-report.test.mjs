#!/usr/bin/env node
// Synthetic regression for scripts/coverage-report.mjs — exercises the real
// reporter end to end (spawned as a subprocess) against hand-built fixtures:
//
//   1. happy path:   an inline source map whose `sources` mix the BARE shape
//                    (`src/App.tsx`) and the PREFIXED shape
//                    (`../../crates/pi-cli/web/src/panels/Log.tsx`) must be
//                    canonicalized under --web-root/src, produce REAL nonzero
//                    totals, meet thresholds, and exit 0 with an LCOV report
//                    containing SF/DA/FN records.
//   2. zero-source:  a payload whose map carries no `src/...` entry (so the
//                    measured set under web-root/src is empty) must FAIL
//                    closed (exit 2) instead of passing on 0/0.
//   3. empty-expected: a web root with no src/ tree must FAIL closed (exit 2).
//
// The V8 payloads are structurally identical to a Playwright stopJSCoverage()
// dump; the bundle script embeds a real base64 data-URI source map whose VLQ
// mappings point every generated line onto its original source line, so the
// reporter's v8-to-istanbul conversion produces genuine line/function/branch
// coverage (100% on these tiny fixtures).
//
// Usage: node scripts/coverage-report.test.mjs
// Exit codes: 0 = all scenarios behaved as specified; 1 = regression found.
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const REPORTER = path.join(HERE, 'coverage-report.mjs');
const WEB_DIR = path.resolve(HERE, '..');

const B64 = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';
function vlq(value) {
  let v = value < 0 ? (-value << 1) | 1 : value << 1;
  let out = '';
  do {
    let digit = v & 31;
    v >>>= 5;
    if (v > 0) digit |= 32;
    out += B64[digit];
  } while (v > 0);
  return out;
}

// One generated line per original source. Each line carries two segments
// (col 0 -> original line start, col lineLen -> original line end) so a V8
// range spanning the whole generated line remaps onto the whole original
// line (v8-to-istanbul's originalEndPositionFor walks to the next segment).
function buildMappings(bundleLines, origLineCols) {
  const lines = [];
  let prevSrc = 0;
  let prevOrigLine = 0;
  let prevOrigCol = 0;
  bundleLines.forEach((text, i) => {
    const segs = [
      [0, i, 0, 0],
      [text.length, i, 0, origLineCols[i]],
    ];
    let prevGenCol = 0;
    lines.push(
      segs
        .map(([genCol, src, origLine, origCol]) => {
          const encoded =
            vlq(genCol - prevGenCol) +
            vlq(src - prevSrc) +
            vlq(origLine - prevOrigLine) +
            vlq(origCol - prevOrigCol);
          prevGenCol = genCol;
          prevSrc = src;
          prevOrigLine = origLine;
          prevOrigCol = origCol;
          return encoded;
        })
        .join(',')
    );
  });
  return lines.join(';');
}

// Build a synthetic web root + dist bundle + coverage payload, returning
// everything the reporter needs plus the temp dir for cleanup.
function makeFixture({ srcFiles, mapSources, sourcesContent }) {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'cov-report-test-'));
  const webRoot = path.join(tmp, 'web-root');
  const distDir = path.join(tmp, 'dist');
  const payloadsDir = path.join(tmp, 'payloads');
  const outDir = path.join(tmp, 'report');
  for (const [rel, content] of Object.entries(srcFiles)) {
    const p = path.join(webRoot, rel);
    fs.mkdirSync(path.dirname(p), { recursive: true });
    fs.writeFileSync(p, content);
  }

  // One generated line per mapped source; original sources are 3 lines each.
  const bundleLines = mapSources.map((_, i) => `function Fn${i}(){return ${i + 1}}`);
  const bundle = bundleLines.join('\n') + '\n';
  const origLineCols = sourcesContent.map((c) => c.split('\n')[0].length);
  const map = {
    version: 3,
    file: 'index.html',
    sources: mapSources,
    sourcesContent,
    names: [],
    mappings: buildMappings(bundleLines, origLineCols),
  };
  const b64 = Buffer.from(JSON.stringify(map)).toString('base64');
  const source = `${bundle}//# sourceMappingURL=data:application/json;charset=utf-8;base64,${b64}`;

  // V8-style ranges covering each full generated line (offsets are exact:
  // every line's start is the previous start + length + 1 newline).
  const starts = [];
  let pos = 0;
  for (const l of bundleLines) {
    starts.push(pos);
    pos += l.length + 1;
  }
  const functions = bundleLines.map((_, i) => ({
    functionName: `Fn${i}`,
    ranges: [{ startOffset: starts[i], endOffset: (i + 1 < starts.length ? starts[i + 1] : pos) - 1, count: 1 }],
    isBlockCoverage: true,
  }));

  fs.mkdirSync(distDir, { recursive: true });
  fs.writeFileSync(path.join(distDir, 'index.html'), `<!doctype html><html><body><script>${source}</script></body></html>`);
  fs.mkdirSync(payloadsDir, { recursive: true });
  fs.writeFileSync(
    path.join(payloadsDir, 'lane-1.json'),
    JSON.stringify([{ url: 'http://127.0.0.1:0/web', source, functions }])
  );
  return { tmp, webRoot, distPath: path.join(distDir, 'index.html'), payloadsDir, outDir };
}

function runReporter({ webRoot, distPath, payloadsDir, outDir }) {
  return spawnSync(process.execPath, [REPORTER, '--payloads', payloadsDir, '--dist', distPath, '--web-root', webRoot, '--out', outDir], {
    cwd: WEB_DIR,
    encoding: 'utf8',
  });
}

const APP_TSX = 'export function App() {\n  return 1;\n}\n';
const LOG_TSX = 'export function Log() {\n  return 2;\n}\n';

const failures = [];
function check(name, cond, detail) {
  if (cond) {
    console.log(`PASS  ${name}`);
  } else {
    failures.push(name);
    console.error(`FAIL  ${name}${detail ? ` — ${detail}` : ''}`);
  }
}

let ran = 0;
// ---- 1. happy path: bare + prefixed source shapes, real nonzero totals ----
{
  const fix = makeFixture({
    srcFiles: { 'src/App.tsx': APP_TSX, 'src/panels/Log.tsx': LOG_TSX },
    mapSources: ['src/App.tsx', '../../crates/pi-cli/web/src/panels/Log.tsx'],
    sourcesContent: [APP_TSX, LOG_TSX],
  });
  try {
    const res = runReporter(fix);
    const out = `${res.stdout}\n${res.stderr}`;
    check('happy: exits 0 with thresholds met', res.status === 0, `status=${res.status} stderr=${res.stderr}`);
    check('happy: reports thresholds met', /thresholds met/.test(out), out);
    const summaryPath = path.join(fix.outDir, 'coverage-summary.json');
    check('happy: coverage-summary.json written', fs.existsSync(summaryPath));
    const lcovPath = path.join(fix.outDir, 'lcov.info');
    const lcov = fs.existsSync(lcovPath) ? fs.readFileSync(lcovPath, 'utf8') : '';
    check('happy: lcov.info has SF/DA/FN records', /^SF:/m.test(lcov) && /^DA:/m.test(lcov) && /^FN:/m.test(lcov), lcovPath);
    if (fs.existsSync(summaryPath)) {
      const summary = JSON.parse(fs.readFileSync(summaryPath, 'utf8'));
      const t = summary.total;
      const keyed =
        t &&
        t.lines &&
        t.lines.total > 0 &&
        t.lines.pct > 0 &&
        t.functions &&
        t.functions.total > 0 &&
        Number.isFinite(t.functions.pct) &&
        t.branches &&
        t.branches.total > 0 &&
        t.statements &&
        t.statements.total > 0;
      check('happy: totals are real and nonzero', Boolean(keyed), JSON.stringify(t));
      const keys = Object.keys(summary);
      check(
        'happy: bare src/App.tsx canonicalized under web-root/src',
        keys.includes(path.join(fix.webRoot, 'src', 'App.tsx')),
        keys.join(', ')
      );
      check(
        'happy: prefixed .../web/src/panels/Log.tsx canonicalized under web-root/src',
        keys.includes(path.join(fix.webRoot, 'src', 'panels', 'Log.tsx')),
        keys.join(', ')
      );
    }
  } finally {
    fs.rmSync(fix.tmp, { recursive: true, force: true });
  }
  ran += 1;
}

// ---- 2. zero-source: no src/... entry in the map must fail closed ----
{
  const fix = makeFixture({
    srcFiles: { 'src/App.tsx': APP_TSX, 'src/panels/Log.tsx': LOG_TSX },
    mapSources: ['vendor/lib.js', '../outside/helper.ts'],
    sourcesContent: ['export const a = 1;\n', 'export const b = 2;\n'],
  });
  try {
    const res = runReporter(fix);
    const out = `${res.stdout}\n${res.stderr}`;
    check('zero-source: exits 2 (fail-closed)', res.status === 2, `status=${res.status} stdout=${res.stdout}`);
    check(
      'zero-source: reports missing/unmapped Web src coverage',
      /expected sources missing|no Web src coverage|filtered Web source coverage is empty/.test(out),
      out
    );
  } finally {
    fs.rmSync(fix.tmp, { recursive: true, force: true });
  }
  ran += 1;
}

// ---- 3. empty-expected: no src/ tree must fail closed ----
{
  const fix = makeFixture({
    srcFiles: {},
    mapSources: ['src/App.tsx', '../../crates/pi-cli/web/src/panels/Log.tsx'],
    sourcesContent: [APP_TSX, LOG_TSX],
  });
  try {
    const res = runReporter(fix);
    const out = `${res.stdout}\n${res.stderr}`;
    check('empty-expected: exits 2 (fail-closed)', res.status === 2, `status=${res.status}`);
    check('empty-expected: reports empty expected set', /expected Web source set is empty/.test(out), out);
  } finally {
    fs.rmSync(fix.tmp, { recursive: true, force: true });
  }
  ran += 1;
}

console.log(`\ncoverage-report.test: ${ran} scenarios, ${failures.length} failure(s)`);
process.exit(failures.length === 0 ? 0 : 1);
