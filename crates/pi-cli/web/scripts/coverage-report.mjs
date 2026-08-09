#!/usr/bin/env node
// Web coverage reporter — merges per-lane V8 coverage payloads, converts them
// to Istanbul format through the bundle's inline source map (so line/function/
// branch positions resolve to the original TypeScript/TSX sources), emits
// text + JSON summary + lcov reports, and enforces explicit thresholds.
//
// Usage:
//   node scripts/coverage-report.mjs \
//     --payloads <dir with *.json V8 coverage payloads> \
//     --dist <built coverage index.html> \
//     --web-root <crates/pi-cli/web absolute path> \
//     [--config <coverage.config.mjs>] [--out <report dir>]
//
// Exit codes: 0 = thresholds met; 2 = coverage/threshold/mapping failure.
//
// A payload is one Playwright `stopJSCoverage()` dump ({ url, source,
// functions: [...] } per script). The bundle's inline data-URI source map is
// extracted from each script's source and handed to v8-to-istanbul, which
// remaps every executed range to the original sources.
import fs from 'node:fs';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import v8ToIstanbul from 'v8-to-istanbul';
import libCoverage from 'istanbul-lib-coverage';
import libReport from 'istanbul-lib-report';
import reports from 'istanbul-reports';

const { createCoverageMap } = libCoverage;

function usage() {
  console.error('usage: node scripts/coverage-report.mjs --payloads <dir> --dist <index.html> --web-root <dir> [--config <file>] [--out <dir>]');
  process.exit(2);
}

const argv = process.argv.slice(2);
function opt(name) {
  const idx = argv.indexOf(name);
  return idx >= 0 ? argv[idx + 1] : undefined;
}
const payloadsDir = opt('--payloads');
const distIndex = opt('--dist');
const webRoot = opt('--web-root');
const configPath = opt('--config');
const outDir = opt('--out') || path.join(payloadsDir || '.', 'report');
if (!payloadsDir || !distIndex || !webRoot) usage();

// ---- config (defaults; overridable via coverage.config.mjs) ----
const defaultConfig = {
  include: ['src/**/*.{ts,tsx}'],
  // src/types.ts is a type-only module: zero runtime statements, so it never
  // appears in V8 coverage and is excluded from the expected-file set.
  exclude: ['src/types.ts'],
  thresholds: { lines: 90, functions: 90, branches: 75, statements: 90 },
};
let config = defaultConfig;
if (configPath) {
  config = { ...defaultConfig, ...(await import(pathToFileURL(path.resolve(configPath)).href)).default };
}

const SOURCE_FILE_RE = /\.(ts|tsx)$/;
const MAPPED_SOURCE_RE = /(?:^|\/)src\/(.+\.(?:ts|tsx))$/;

/** Walk webRoot/src and return the expected instrumented source set. */
function expectedSources() {
  const root = path.join(webRoot, 'src');
  const out = [];
  const walk = (dir) => {
    for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
      const full = path.join(dir, entry.name);
      if (entry.isDirectory()) walk(full);
      else if (SOURCE_FILE_RE.test(entry.name)) out.push(path.normalize(full));
    }
  };
  if (fs.existsSync(root)) walk(root);
  return out.filter((f) => !config.exclude.includes(path.relative(webRoot, f).replace(/\\/g, '/')));
}

/** Extract the inline data-URI source map from a bundle script. */
function extractInlineMap(source) {
  const m = source.match(/\/\/# sourceMappingURL=data:application\/json;charset=utf-8;base64,([A-Za-z0-9+/=]+)/);
  if (!m) return null;
  try {
    return JSON.parse(Buffer.from(m[1], 'base64').toString('utf8'));
  } catch {
    return null;
  }
}

/** Resolve source-map entries, canonicalizing project sources below webRoot/src. */
function absolutizeMapSources(map, distFile) {
  const distDir = path.dirname(path.resolve(distFile));
  const base = path.join(distDir, path.dirname(map.file || '.'));
  return {
    ...map,
    sourceRoot: '',
    sources: map.sources.map((source) => {
      const normalized = source.replace(/\\/g, '/');
      const projectSource = normalized.match(MAPPED_SOURCE_RE);
      if (projectSource) return path.join(webRoot, 'src', projectSource[1]);
      if (!normalized.includes('/') && SOURCE_FILE_RE.test(normalized)) {
        return path.join(webRoot, 'src', normalized);
      }
      return path.isAbsolute(source) ? source : path.resolve(base, source);
    }),
  };
}

// ---- 1. load payloads ----
if (!fs.existsSync(payloadsDir)) {
  console.error(`coverage-report: FAIL: payloads dir missing: ${payloadsDir}`);
  process.exit(2);
}
const payloadFiles = fs
  .readdirSync(payloadsDir)
  .filter((f) => f.endsWith('.json'))
  .sort();
if (payloadFiles.length === 0) {
  console.error(`coverage-report: FAIL: no coverage payloads in ${payloadsDir} (did any lane run Playwright?)`);
  process.exit(2);
}

const coverageMap = createCoverageMap();
let mapSeen = false;
const perPayload = [];
for (const file of payloadFiles) {
  let payload;
  try {
    payload = JSON.parse(fs.readFileSync(path.join(payloadsDir, file), 'utf8'));
  } catch (err) {
    console.error(`coverage-report: FAIL: unparseable payload ${file}: ${err.message}`);
    process.exit(2);
  }
  if (!Array.isArray(payload) || payload.length === 0) {
    console.error(`coverage-report: FAIL: payload ${file} is empty (lane produced no V8 coverage)`);
    process.exit(2);
  }
  let convertible = 0;
  for (const entry of payload) {
    if (!entry || !entry.source || !Array.isArray(entry.functions) || entry.functions.length === 0) continue;
    const rawMap = extractInlineMap(entry.source);
    if (!rawMap) continue; // non-bundle script (about:blank etc.)
    mapSeen = true;
    const map = absolutizeMapSources(rawMap, distIndex);
    const script = v8ToIstanbul(distIndex, 0, {
      source: entry.source,
      sourceMap: { sourcemap: map },
    });
    await script.load();
    script.applyCoverage(entry.functions);
    coverageMap.merge(script.toIstanbul());
    convertible += 1;
  }
  perPayload.push({ file, entries: payload.length, convertible });
  if (convertible === 0) {
    console.error(`coverage-report: FAIL: payload ${file} contains no convertible bundle script (source mapping unavailable)`);
    process.exit(2);
  }
}
if (!mapSeen) {
  console.error('coverage-report: FAIL: no bundle script carried an inline source map (coverage build misconfigured)');
  process.exit(2);
}

// ---- 2. source-mapping verification over expected src files ----
const expected = expectedSources();
if (expected.length === 0) {
  console.error('coverage-report: FAIL: expected Web source set is empty');
  process.exit(2);
}
const coveredRel = new Set(
  Object.keys(coverageMap.data)
    .filter((k) => path.resolve(k).startsWith(webRoot + path.sep))
    .map((k) => path.relative(webRoot, k))
);
const missing = expected.filter((f) => !coveredRel.has(path.relative(webRoot, f)));
if (missing.length > 0) {
  console.error('coverage-report: FAIL: expected sources missing from coverage (source mapping or collection gap):');
  for (const f of missing) console.error(`  - ${path.relative(webRoot, f)}`);
  process.exit(2);
}
if (coveredRel.size === 0) {
  console.error('coverage-report: FAIL: source map produced no Web src coverage');
  process.exit(2);
}

// ---- 3. filter to src files and report ----
const srcOnly = createCoverageMap();
for (const f of expected) {
  const abs = path.resolve(f);
  if (coverageMap.data[abs]) srcOnly.addFileCoverage(coverageMap.fileCoverageFor(abs));
}
if (srcOnly.files().length === 0) {
  console.error('coverage-report: FAIL: filtered Web source coverage is empty');
  process.exit(2);
}
const totals = srcOnly.getCoverageSummary();
const summary = totals.toJSON();

fs.mkdirSync(outDir, { recursive: true });

const context = libReport.createContext({
  dir: outDir,
  coverageMap: srcOnly,
  defaultSummarizer: 'flat',
});

console.log('\n[coverage] per-file (source-mapped to crates/pi-cli/web/src):');
console.log('  ' + ['file', 'lines', 'functions', 'branches', 'statements'].map((h) => h.padEnd(11)).join(''));
for (const file of srcOnly.files().sort()) {
  const s = srcOnly.fileCoverageFor(file).toSummary();
  const rel = path.relative(webRoot, file).replace(/\\/g, '/');
  const fmt = (x) => `${String(x.pct).padStart(3)}% (${x.covered}/${x.total})`.padEnd(30);
  console.log(`  ${rel.padEnd(34)}${fmt(s.lines)}${fmt(s.functions)}${fmt(s.branches)}${fmt(s.statements)}`);
}
console.log('\n[coverage] totals (src/**/*.{ts,tsx}):');
for (const m of ['lines', 'functions', 'branches', 'statements']) {
  const v = summary[m];
  console.log(`  ${m.padEnd(11)} ${String(v.pct).padStart(3)}% (${v.covered}/${v.total})`);
}

// istanbul reporters: text-summary, json-summary, lcov
reports.create('text-summary', {}).execute(context);
reports.create('json-summary', { file: 'coverage-summary.json' }).execute(context);
reports.create('lcov', {}).execute(context);
const lcovPath = path.join(outDir, 'lcov.info');
const lcov = fs.existsSync(lcovPath) ? fs.readFileSync(lcovPath, 'utf8') : '';
if (!/^SF:/m.test(lcov) || !/^DA:/m.test(lcov) || !/^FN:/m.test(lcov)) {
  console.error('coverage-report: FAIL: LCOV report contains no source, line, or function records');
  process.exit(2);
}

// ---- 4. thresholds ----
const failures = [];
for (const metric of ['lines', 'functions', 'branches', 'statements']) {
  const want = config.thresholds[metric];
  const measured = summary[metric];
  const got = Number(measured.pct);
  if (measured.total === 0 || !Number.isFinite(got)) {
    failures.push(`${metric}: coverage total is zero or non-finite`);
  } else if (typeof want === 'number' && got < want) {
    failures.push(`${metric}: ${got}% < required ${want}%`);
  }
}
if (failures.length > 0) {
  console.error('\n[coverage] THRESHOLD FAILURE:');
  for (const f of failures) console.error(`  - ${f}`);
  console.error(`  report: ${outDir}`);
  process.exit(2);
}

console.log(`\n[coverage] thresholds met (lines/functions/branches/statements >= ${config.thresholds.lines}/${config.thresholds.functions}/${config.thresholds.branches}/${config.thresholds.statements}%)`);
console.log(`[coverage] report outputs: ${outDir}/coverage-summary.json, ${outDir}/lcov.info, ${outDir}/lcov-report/`);
console.log(`[coverage] payload sources: ${perPayload.map((p) => `${p.file}(${p.convertible} scripts)`).join(', ')}`);
