// Web external-session discovery / secure import E2E lane — PLAYWRIGHT-ONLY
// (E2E.d/web/external_sessions.sh).
//
// Environment:
//   RPI_URL                    http://127.0.0.1:<port>/web
//   RPI_TOKEN                  token file content (rpi-auth.<token> subprotocol)
//   RPI_CHROME                 system Chrome executable (optional)
//   RPI_EVIDENCE               evidence dir for screenshots + assertion ids
//   RPI_PHASE                  "default" | "native_only"
//   RPI_SEED_META              absolute path to seed-meta.json from the shell
//   RPI_NATIVE_SESSIONS_ROOT   absolute path to the fixture native sessions tree
//
// Assertion matrix (feature -> IDs, used by the coverage report):
//   X0  X0.1 boot: page title + WS connected
//   X1  X1.1 sidebar lists OMP + Codex + Grok foreign rows (default Web sources)
//       X1.2 sidebar provider groups include rpi, OMP, Codex, and Grok (no tmp/UUID/source)
//   X2  X2.1 click foreign Codex row activates a native import copy
//       X2.2 Session panel session-file is under the native tree (not .codex)
//       X2.3 imported transcript restores the foreign Codex user/assistant text
//   X3  X3.1 foreign Codex source bytes unchanged after import
//       X3.2 foreign Codex source mtime unchanged after import
//   X4  X4.1 exactly one import_*.jsonl under the native sessions tree
//       X4.2 no duplicate logical row (foreign Codex path gone after import;
//            at most one sidebar row for the imported native id)
//   X5  X5.1 leave the imported session then re-select the native copy
//       X5.2 re-select reuses the same native session file (lineage reuse;
//            import_*.jsonl count still 1)
//   X7  X7.1 click the rotated OMP leaf row -> distinct native import copy
//       X7.2 imported transcript renders the full parentSession chain
//            (early + middle + final user/assistant turns, ordered once)
//       X7.3 exactly one import_*.jsonl carrying the whole chain; no handoff
//            custom-message text, no child-session text
//       X7.4 task/subagent child session row absent and child text never
//            rendered
//   X8  X8.1 all four OMP source files (early/mid/final/child) byte+mtime
//            immutable after chain import
//   X6  X6.1 sessionImportSources:[] — foreign OMP/Codex/Grok rows absent
//       X6.2 native seed still listed under native-only policy
//
// Every passing contract records its machine-readable ID; on full success of
// the active phase the lane writes
// $RPI_EVIDENCE/coverage-assertions-<phase>.json. The shell merges both phase
// files into coverage-assertions.json for the coverage matrix. The matrix
// feature "external sessions" requires ALL of DOCUMENTED_IDS; the lane fails
// before writing evidence unless every ID for the active phase executed.
//
// No skips. No soft passes. Exit 2 on assertion failure (exit 1 reserved for
// playwright/npm setup failures in web_run_playwright).

import { chromium } from 'playwright';
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';
const phase = process.env.RPI_PHASE || 'default';
const seedMetaPath = process.env.RPI_SEED_META || '';
const nativeSessionsRoot = process.env.RPI_NATIVE_SESSIONS_ROOT || '';

const PHASE_IDS = {
  default: [
    'X0.1',
    'X1.1', 'X1.2',
    'X2.1', 'X2.2', 'X2.3',
    'X3.1', 'X3.2',
    'X4.1', 'X4.2',
    'X5.1', 'X5.2',
    'X7.1', 'X7.2', 'X7.3', 'X7.4',
    'X8.1',
  ],
  native_only: [
    'X6.1', 'X6.2',
  ],
};

// Full documented matrix across both phases (coverage gate).
const DOCUMENTED_IDS = [...PHASE_IDS.default, ...PHASE_IDS.native_only];
const executed = new Set();

function record(id) {
  executed.add(id);
  console.log(`[web-external-sessions:assert] ${id}`);
}

function fail(message) {
  // Exit 2 (not 1): run.sh/web_run_playwright treat 1 as setup failure.
  console.error(`web-external-sessions: FAIL: ${message}`);
  process.exit(2);
}

function loadSeedMeta() {
  if (!seedMetaPath || !fs.existsSync(seedMetaPath)) {
    fail(`RPI_SEED_META missing or unreadable: ${seedMetaPath}`);
  }
  return JSON.parse(fs.readFileSync(seedMetaPath, 'utf8'));
}

function fileFingerprint(filePath) {
  const st = fs.statSync(filePath);
  const buf = fs.readFileSync(filePath);
  return {
    size: st.size,
    mtimeMs: st.mtimeMs,
    mtimeNs: (st.mtimeNs !== undefined ? String(st.mtimeNs) : null),
    sha256: crypto.createHash('sha256').update(buf).digest('hex'),
    bytes: buf,
  };
}

function listImportFiles(rootDir) {
  const out = [];
  const walk = (dir) => {
    let entries;
    try {
      entries = fs.readdirSync(dir, { withFileTypes: true });
    } catch {
      return;
    }
    for (const entry of entries) {
      const full = path.join(dir, entry.name);
      if (entry.isDirectory()) walk(full);
      else if (entry.isFile() && entry.name.startsWith('import_') && entry.name.endsWith('.jsonl')) {
        out.push(full);
      }
    }
  };
  if (rootDir) walk(rootDir);
  return out.sort();
}

async function waitFor(page, fn, id, timeoutMs = 30000, arg) {
  try {
    await page.waitForFunction(fn, arg, { timeout: timeoutMs });
  } catch {
    fail(`${id} (timeout ${timeoutMs}ms)`);
  }
  const tid = typeof id === 'string' ? (id.match(/^X\d+\.\d+/) || [null])[0] : null;
  if (tid) record(tid);
}

async function waitRowActive(page, sid, id) {
  await waitFor(
    page,
    (s) => {
      const rows = [...document.querySelectorAll('.session-sidebar__row')];
      const row = rows.find((candidate) =>
        candidate.querySelector('.session-sidebar__switch')?.dataset.sessionId === s
      );
      return row?.classList.contains('session-sidebar__row--active') === true;
    },
    `${id}: session ${sid} never became active`,
    30000,
    sid
  );
}

async function sidebarSnapshot(page) {
  return page.evaluate(() => {
    const providerGroups = [...document.querySelectorAll('[data-group-kind="provider"] .session-sidebar__group-label')]
      .map((el) => (el.textContent || '').trim());
    const groups = [...document.querySelectorAll('.session-sidebar__group-label')]
      .map((el) => (el.textContent || '').trim());
    const rows = [...document.querySelectorAll('.session-sidebar__switch')].map((r) => ({
      sessionId: r.dataset.sessionId || '',
      source: r.dataset.sessionSource || '',
      title: (r.querySelector('.session-sidebar__name')?.textContent || '').trim(),
      summary: (r.querySelector('.session-sidebar__summary')?.textContent || '').trim(),
    }));
    return { groups, providerGroups, rows };
  });
}

async function panelSessionFileTitle(page) {
  return page.evaluate(() => {
    const el = document.querySelector('[data-testid="session-file-value"]');
    return el ? el.getAttribute('title') || el.textContent || '' : '';
  });
}

async function openSessionPanel(page, id) {
  const open = await page.evaluate(() => document.getElementById('session-panel') !== null);
  if (!open) {
    await page.click('#session-toggle-btn');
  }
  await waitFor(page, () => document.getElementById('session-panel') !== null, `${id}: session panel did not open`);
}

async function clickSidebarSession(page, sessionId) {
  const clicked = await page.evaluate((sid) => {
    const row = [...document.querySelectorAll('.session-sidebar__switch')]
      .find((r) => r.dataset.sessionId === sid);
    if (!row) return false;
    row.click();
    return true;
  }, sessionId);
  if (!clicked) fail(`sidebar row not found for sessionId=${sessionId}`);
}

async function activeTranscript(page) {
  return page.evaluate(() => document.getElementById('transcript')?.textContent || '');
}

async function connectAndBoot(page) {
  await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
  await waitFor(page, () => document.title === 'rpi web', 'boot: page title missing');
  await waitFor(
    page,
    () => document.getElementById('conn-state')?.dataset.state === 'on',
    'boot: WS did not reach "connected"'
  );
}

async function runDefaultPhase(page, meta) {
  /* ---------------- X0: boot ---------------- */
  await connectAndBoot(page);
  // Newest native seed is preferred; tolerate primary runtime until catalog
  // restore settles on the seeded native id when available.
  await waitFor(
    page,
    (sid) => {
      const rows = [...document.querySelectorAll('.session-sidebar__switch')];
      return rows.some((r) => r.dataset.sessionId === sid);
    },
    'X0.1: native seed row never appeared after boot',
    30000,
    meta.nativeId
  );
  record('X0.1');
  await page.screenshot({ path: `${evidence}/external-x0-boot.png`, fullPage: true });

  /* ---------------- X1: default discovery + grouping ---------------- */
  await waitFor(
    page,
    (ids) => {
      const rows = [...document.querySelectorAll('.session-sidebar__switch')];
      const present = new Set(rows.map((r) => r.dataset.sessionId || ''));
      return ids.every((id) => present.has(id));
    },
    'X1.1: foreign OMP/Codex/Grok rows never appeared',
    30000,
    [meta.ompId, meta.codexId, meta.grokId]
  );
  const snap1 = await sidebarSnapshot(page);
  for (const id of [meta.ompId, meta.codexId, meta.grokId, meta.nativeId]) {
    if (!snap1.rows.some((r) => r.sessionId === id)) {
      fail(`X1.1: missing session row ${id} (got ${snap1.rows.map((r) => r.sessionId).join(', ')})`);
    }
  }
  const ompRow = snap1.rows.find((r) => r.sessionId === meta.ompId);
  const codexRow = snap1.rows.find((r) => r.sessionId === meta.codexId);
  const grokRow = snap1.rows.find((r) => r.sessionId === meta.grokId);
  if (!ompRow || ompRow.source !== 'omp') {
    fail(`X1.1: OMP row source is not omp (got ${ompRow && ompRow.source})`);
  }
  if (!codexRow || codexRow.source !== 'codex') {
    fail(`X1.1: Codex row source is not codex (got ${codexRow && codexRow.source})`);
  }
  // Grok wire label is "grok/hyper" (SessionSourceKind::label).
  if (!grokRow || (grokRow.source !== 'grok/hyper' && grokRow.source !== 'grok')) {
    fail(`X1.1: Grok row source is not grok/hyper (got ${grokRow && grokRow.source})`);
  }
  record('X1.1');

  const requiredProviders = ['rpi', 'OMP', 'Codex', 'Grok'];
  for (const g of requiredProviders) {
    if (!snap1.providerGroups.includes(g)) {
      fail(`X1.2: missing provider group "${g}" (got ${snap1.providerGroups.join(', ')})`);
    }
  }
  const forbidden = snap1.providerGroups.filter(
    (g) => !['rpi', 'Codex', 'Grok', 'OMP'].includes(g)
  );
  if (forbidden.length > 0) {
    fail(`X1.2: non-provider top-level groups found: ${forbidden.join(', ')}`);
  }
  record('X1.2');
  await page.screenshot({ path: `${evidence}/external-x1-discovery.png`, fullPage: true });

  /* ---- Search exercise: provider/title/id filtering + active visibility ---- */
  // The active session (native seed) must stay visible during every filter.
  const activeSid = await page.evaluate(() => {
    const row = document.querySelector('.session-sidebar__row--active .session-sidebar__switch');
    return row?.dataset.sessionId || '';
  });
  const allIds = await page.evaluate(() =>
    [...document.querySelectorAll('.session-sidebar__switch')].map((r) => r.dataset.sessionId || '')
  );
  if (!activeSid || !allIds.includes(activeSid)) {
    fail(`search: active session ${activeSid} not found before search (got ${allIds.join(', ')})`);
  }

  // Filter by provider label "Codex" — only Codex rows + active remain.
  await page.fill('#session-sidebar-search', 'Codex');
  await waitFor(
    page,
    (expected) => document.querySelectorAll('.session-sidebar__switch').length < expected,
    'search: Codex filter did not reduce row count',
    10000,
    allIds.length
  );
  {
    const filtered = await page.evaluate(() =>
      [...document.querySelectorAll('.session-sidebar__switch')].map((r) => r.dataset.sessionId || '')
    );
    if (!filtered.includes(activeSid)) {
      fail(`search: active session ${activeSid} disappeared during Codex filter (got ${filtered.join(', ')})`);
    }
  }

  // Clear restores the full list.
  await page.click('#session-sidebar-search-clear');
  await waitFor(
    page,
    (expected) => {
      const ids = [...document.querySelectorAll('.session-sidebar__switch')].map((r) => r.dataset.sessionId || '');
      return ids.length >= expected;
    },
    'search: clear did not restore all rows',
    10000,
    allIds.length
  );

  // Filter by session id — only that row + active remain.
  await page.fill('#session-sidebar-search', meta.ompId);
  await waitFor(
    page,
    (sid) => [...document.querySelectorAll('.session-sidebar__switch')]
      .some((r) => r.dataset.sessionId === sid),
    'search: omp id filter did not keep the omp row',
    10000,
    meta.ompId
  );
  {
    const filtered = await page.evaluate(() =>
      [...document.querySelectorAll('.session-sidebar__switch')].map((r) => r.dataset.sessionId || '')
    );
    if (!filtered.includes(activeSid)) {
      fail(`search: active session ${activeSid} disappeared during id filter (got ${filtered.join(', ')})`);
    }
  }

  // Clear restores again.
  await page.click('#session-sidebar-search-clear');
  await waitFor(
    page,
    (expected) => {
      const ids = [...document.querySelectorAll('.session-sidebar__switch')].map((r) => r.dataset.sessionId || '');
      return ids.length >= expected;
    },
    'search: clear after id filter did not restore all rows',
    10000,
    allIds.length
  );
  /* ---------------- X2/X3: click foreign Codex -> native import ---------------- */
  if (!fs.existsSync(meta.codexPath)) {
    fail(`X3 precondition: codex foreign file missing at ${meta.codexPath}`);
  }
  const beforeForeign = fileFingerprint(meta.codexPath);
  const importsBefore = listImportFiles(nativeSessionsRoot);

  await clickSidebarSession(page, meta.codexId);

  // After import the actionable row is the native copy: foreign Codex path is
  // coalesced away and the active session id becomes the imported native id
  // (import_*.jsonl header id), not the foreign source id.
  await waitFor(
    page,
    (ids) => {
      const active = document.querySelector('.session-sidebar__row--active .session-sidebar__switch');
      if (!active) return false;
      const sid = active.dataset.sessionId || '';
      const source = active.dataset.sessionSource || '';
      const isNative = source === 'pi' || source === 'native' || source === 'primary';
      return isNative && sid !== ids.foreignId && sid !== ids.priorId;
    },
    'X2.1: foreign Codex click never activated a distinct native copy',
    30000,
    { foreignId: meta.codexId, priorId: activeSid }
  );
  record('X2.1');

  await openSessionPanel(page, 'X2.2');
  await waitFor(
    page,
    () => {
      const el = document.querySelector('[data-testid="session-file-value"]');
      const title = el ? el.getAttribute('title') || el.textContent || '' : '';
      if (!title.endsWith('.jsonl')) return false;
      if (title.includes('/.codex/') || title.includes('\\codex\\')) return false;
      return title.includes('import_');
    },
    'X2.2: panel session-file never pointed at the native import copy',
    30000
  );
  const sessionFile = await panelSessionFileTitle(page);
  if (sessionFile.includes('/.codex/') || sessionFile.includes('\\codex\\')) {
    fail(`X2.2: active session file is still the foreign Codex path (${sessionFile})`);
  }
  record('X2.2');

  await waitFor(
    page,
    () => {
      const text = document.getElementById('transcript')?.textContent || '';
      return text.includes('Codex external prompt') && text.includes('Codex external reply');
    },
    'X2.3: imported transcript never restored Codex external messages',
    30000
  );
  const transcript = await activeTranscript(page);
  if (!transcript.includes('Codex external prompt') || !transcript.includes('Codex external reply')) {
    fail('X2.3: imported transcript missing Codex external prompt/reply');
  }
  record('X2.3');
  await page.screenshot({ path: `${evidence}/external-x2-import.png`, fullPage: true });

  /* ---------------- X3: foreign source immutable ---------------- */
  const afterForeign = fileFingerprint(meta.codexPath);
  if (afterForeign.sha256 !== beforeForeign.sha256 || afterForeign.size !== beforeForeign.size) {
    fail('X3.1: foreign Codex source bytes changed after import');
  }
  // Compare full buffer equality as well (hard contract, not just hash).
  if (!afterForeign.bytes.equals(beforeForeign.bytes)) {
    fail('X3.1: foreign Codex source byte content changed after import');
  }
  record('X3.1');
  if (afterForeign.mtimeMs !== beforeForeign.mtimeMs) {
    fail(`X3.2: foreign Codex mtime changed (${beforeForeign.mtimeMs} -> ${afterForeign.mtimeMs})`);
  }
  if (beforeForeign.mtimeNs !== null && afterForeign.mtimeNs !== beforeForeign.mtimeNs) {
    fail(`X3.2: foreign Codex mtimeNs changed (${beforeForeign.mtimeNs} -> ${afterForeign.mtimeNs})`);
  }
  record('X3.2');

  // Also prove OMP + Grok foreign files were not touched by the Codex import.
  for (const [label, p] of [['omp', meta.ompPath], ['grok', meta.grokPath]]) {
    if (!fs.existsSync(p)) fail(`${label} foreign source missing after import: ${p}`);
  }

  /* ---------------- X4: single import + no duplicate logical row ---------------- */
  const deadline = Date.now() + 15000;
  let importsAfter = listImportFiles(nativeSessionsRoot);
  while (importsAfter.length <= importsBefore.length && Date.now() < deadline) {
    await new Promise((r) => setTimeout(r, 200));
    importsAfter = listImportFiles(nativeSessionsRoot);
  }
  const newImports = importsAfter.filter((p) => !importsBefore.includes(p));
  if (newImports.length !== 1) {
    fail(`X4.1: expected exactly one new import_*.jsonl (before=${importsBefore.length}, after=${importsAfter.length}, new=${newImports.join(', ')})`);
  }
  if (importsAfter.length !== importsBefore.length + 1) {
    fail(`X4.1: native import count is not +1 (before=${importsBefore.length}, after=${importsAfter.length})`);
  }
  const importedPath = newImports[0];
  if (!sessionFile.includes(path.basename(importedPath)) && sessionFile !== importedPath) {
    // Panel may show the absolute path; require the active file to be the new import.
    if (path.resolve(sessionFile) !== path.resolve(importedPath)) {
      // Accept when the panel path equals the import file even if string form differs.
      const panelBase = path.basename(sessionFile);
      const importBase = path.basename(importedPath);
      if (panelBase !== importBase) {
        fail(`X4.1: panel session-file ${sessionFile} is not the new import ${importedPath}`);
      }
    }
  }
  record('X4.1');

  // Foreign Codex row must be coalesced away once the native copy is listed.
  await waitFor(
    page,
    (args) => {
      const rows = [...document.querySelectorAll('.session-sidebar__switch')].map((r) => ({
        sessionId: r.dataset.sessionId || '',
        source: r.dataset.sessionSource || '',
      }));
      const foreignCodex = rows.filter((r) => r.sessionId === args.codexId && r.source === 'codex');
      if (foreignCodex.length !== 0) return false;
      const importBase = args.importBase;
      // At most one row whose title/path identity maps to the import (by id).
      const nativeLike = rows.filter((r) => r.source === 'pi' || r.source === 'native' || r.source === 'primary');
      // No duplicate sessionId among all rows.
      const ids = rows.map((r) => r.sessionId);
      return new Set(ids).size === ids.length && nativeLike.length >= 1 && importBase.length > 0;
    },
    'X4.2: foreign Codex row still listed or duplicate session ids present',
    30000,
    { codexId: meta.codexId, importBase: path.basename(importedPath) }
  );
  const snap4 = await sidebarSnapshot(page);
  if (snap4.rows.some((r) => r.sessionId === meta.codexId && r.source === 'codex')) {
    fail('X4.2: foreign Codex logical row still present after import');
  }
  const idCounts = new Map();
  for (const r of snap4.rows) {
    idCounts.set(r.sessionId, (idCounts.get(r.sessionId) || 0) + 1);
  }
  for (const [sid, count] of idCounts) {
    if (count > 1) fail(`X4.2: duplicate logical rows for sessionId=${sid} (count=${count})`);
  }
  record('X4.2');
  await page.screenshot({ path: `${evidence}/external-x4-no-dup.png`, fullPage: true });

  /* ---------------- X5: re-select native copy -> lineage reuse ---------------- */
  const importedHeader = JSON.parse(fs.readFileSync(importedPath, 'utf8').split('\n', 1)[0]);
  const importedNativeId = importedHeader.id;
  if (!importedNativeId) fail('X5 precondition: imported native session header missing id');

  // Leave the imported session via New, then re-select the native import row.
  await page.click('#sidebar-new-session-btn');
  await waitFor(
    page,
    (importedId) => {
      const active = document.querySelector('.session-sidebar__row--active .session-sidebar__switch');
      const sid = active?.dataset.sessionId || '';
      return sid !== '' && sid !== importedId;
    },
    'X5.1: New session never left the imported native copy',
    30000,
    importedNativeId
  );

  // The imported native row must still be listed (lineage retained on disk).
  await waitFor(
    page,
    (importedId) => [...document.querySelectorAll('.session-sidebar__switch')]
      .some((r) => r.dataset.sessionId === importedId),
    'X5.1: imported native row disappeared after New',
    30000,
    importedNativeId
  );
  await clickSidebarSession(page, importedNativeId);
  await waitRowActive(page, importedNativeId, 'X5.1');
  record('X5.1');

  await openSessionPanel(page, 'X5.2');
  await waitFor(
    page,
    (expectedBase) => {
      const el = document.querySelector('[data-testid="session-file-value"]');
      const title = el ? el.getAttribute('title') || el.textContent || '' : '';
      return title.endsWith(expectedBase) || title.includes(expectedBase);
    },
    'X5.2: re-select did not restore the same native import file',
    30000,
    path.basename(importedPath)
  );
  const sessionFile2 = await panelSessionFileTitle(page);
  if (path.basename(sessionFile2) !== path.basename(importedPath)
    && path.resolve(sessionFile2) !== path.resolve(importedPath)) {
    fail(`X5.2: re-select opened a different session file (${sessionFile2} vs ${importedPath})`);
  }
  const importsReuse = listImportFiles(nativeSessionsRoot);
  if (importsReuse.length !== importsAfter.length) {
    fail(`X5.2: re-select created extra import files (before=${importsAfter.length}, after=${importsReuse.length})`);
  }
  if (!importsReuse.includes(importedPath)) {
    fail(`X5.2: original import file missing after re-select (${importedPath})`);
  }
  // Foreign still immutable after re-select.
  const afterReuse = fileFingerprint(meta.codexPath);
  if (afterReuse.sha256 !== beforeForeign.sha256 || afterReuse.mtimeMs !== beforeForeign.mtimeMs) {
    fail('X5.2: foreign Codex source mutated during native re-select');
  }
  record('X5.2');
  await page.screenshot({ path: `${evidence}/external-x5-reuse.png`, fullPage: true });

  /* ---------------- X7: OMP rotation chain loads fully, ordered once ------- */
  // The seeded OMP logical conversation is rotated across three
  // `parentSession`-linked files (early -> middle -> final). Clicking the leaf
  // must transparently import ONE native copy whose transcript renders every
  // file's user/assistant turns in order, exactly once each — while the
  // depth-3 task/subagent child session stays excluded.
  for (const p of [meta.ompEarlyPath, meta.ompMidPath, meta.ompFinalPath, meta.ompChildPath]) {
    if (!fs.existsSync(p)) fail(`X7 precondition: OMP fixture missing at ${p}`);
  }
  const ompBefore = {};
  for (const [label, p] of [['early', meta.ompEarlyPath], ['mid', meta.ompMidPath],
    ['final', meta.ompFinalPath], ['child', meta.ompChildPath]]) {
    ompBefore[label] = fileFingerprint(p);
  }
  const ompImportsBefore = listImportFiles(nativeSessionsRoot);

  await clickSidebarSession(page, meta.ompId);

  await waitFor(
    page,
    (ids) => {
      const active = document.querySelector('.session-sidebar__row--active .session-sidebar__switch');
      if (!active) return false;
      const sid = active.dataset.sessionId || '';
      const source = active.dataset.sessionSource || '';
      const isNative = source === 'pi' || source === 'native' || source === 'primary';
      return isNative && !ids.includes(sid);
    },
    'X7.1: OMP leaf click never activated a distinct native copy',
    30000,
    [meta.ompId, meta.ompEarlyId, meta.ompMidId, meta.ompChildId, importedNativeId, activeSid]
  );
  record('X7.1');

  // Full chain transcript: early, middle, AND final user/assistant turns all
  // render, in chronological order, each exactly once.
  await waitFor(
    page,
    (needles) => {
      const text = document.getElementById('transcript')?.textContent || '';
      return needles.every((needle) => text.includes(needle));
    },
    'X7.2: imported transcript never restored the full OMP rotation chain',
    30000,
    ['OMP early prompt', 'OMP early reply', 'OMP middle prompt', 'OMP middle reply',
      'OMP final prompt', 'OMP final reply']
  );
  const chainTranscript = await activeTranscript(page);
  const chainNeedles = [
    ['OMP early prompt', 'OMP early reply'],
    ['OMP middle prompt', 'OMP middle reply'],
    ['OMP final prompt', 'OMP final reply'],
  ];
  for (const [prompt, reply] of chainNeedles) {
    const promptAt = chainTranscript.indexOf(prompt);
    const replyAt = chainTranscript.indexOf(reply);
    if (promptAt === -1 || replyAt === -1) fail(`X7.2: chain turn missing (${prompt})`);
    if (promptAt >= replyAt) fail(`X7.2: chain turn out of order (${prompt} before ${reply})`);
  }
  const earlyAt = chainTranscript.indexOf('OMP early prompt');
  const middleAt = chainTranscript.indexOf('OMP middle prompt');
  const finalAt = chainTranscript.indexOf('OMP final prompt');
  if (!(earlyAt < middleAt && middleAt < finalAt)) {
    fail(`X7.2: chain order not chronological (early=${earlyAt} middle=${middleAt} final=${finalAt})`);
  }
  for (const needle of ['OMP early prompt', 'OMP early reply', 'OMP middle prompt',
    'OMP middle reply', 'OMP final prompt', 'OMP final reply']) {
    const occurrences = chainTranscript.split(needle).length - 1;
    if (occurrences !== 1) fail(`X7.2: chain text "${needle}" rendered ${occurrences} times (expected 1)`);
  }
  record('X7.2');

  // Exactly one new import containing the whole chain; handoff custom
  // messages and child-session content never reach the native file.
  const ompDeadline = Date.now() + 15000;
  let ompImportsAfter = listImportFiles(nativeSessionsRoot);
  while (ompImportsAfter.length <= ompImportsBefore.length && Date.now() < ompDeadline) {
    await new Promise((r) => setTimeout(r, 200));
    ompImportsAfter = listImportFiles(nativeSessionsRoot);
  }
  const ompNewImports = ompImportsAfter.filter((p) => !ompImportsBefore.includes(p));
  if (ompNewImports.length !== 1) {
    fail(`X7.3: expected exactly one OMP chain import (new=${ompNewImports.join(', ')})`);
  }
  const ompImportBody = fs.readFileSync(ompNewImports[0], 'utf8');
  for (const needle of ['OMP early prompt', 'OMP early reply', 'OMP middle prompt',
    'OMP middle reply', 'OMP final prompt', 'OMP final reply']) {
    if (!ompImportBody.includes(needle)) fail(`X7.3: chained import missing ${needle}`);
  }
  if (ompImportBody.includes('handoff context')) {
    fail('X7.3: handoff custom-message text leaked into the chained import');
  }
  if (ompImportBody.includes('OMP child prompt')) {
    fail('X7.3: task/subagent child content leaked into the chained import');
  }
  record('X7.3');

  // Child session: never a sidebar row, never a rendered message.
  const snap7 = await sidebarSnapshot(page);
  if (snap7.rows.some((r) => r.sessionId === meta.ompChildId)) {
    fail(`X7.4: child session ${meta.ompChildId} listed in the sidebar`);
  }
  if (chainTranscript.includes('OMP child prompt')) {
    fail('X7.4: child session text rendered in the transcript');
  }
  record('X7.4');
  await page.screenshot({ path: `${evidence}/external-x7-omp-chain.png`, fullPage: true });

  /* ---------------- X8: OMP sources immutable after chain import ----------- */
  for (const [label, p] of [['early', meta.ompEarlyPath], ['mid', meta.ompMidPath],
    ['final', meta.ompFinalPath], ['child', meta.ompChildPath]]) {
    const after = fileFingerprint(p);
    if (after.sha256 !== ompBefore[label].sha256 || after.size !== ompBefore[label].size) {
      fail(`X8.1: OMP ${label} source bytes changed after chain import`);
    }
    if (!after.bytes.equals(ompBefore[label].bytes)) {
      fail(`X8.1: OMP ${label} source byte content changed after chain import`);
    }
    if (after.mtimeMs !== ompBefore[label].mtimeMs) {
      fail(`X8.1: OMP ${label} source mtime changed (${ompBefore[label].mtimeMs} -> ${after.mtimeMs})`);
    }
  }
  record('X8.1');

  // Persist the imported native id for operators inspecting evidence.
  fs.writeFileSync(
    path.join(evidence, 'import-result.json'),
    JSON.stringify({
      importedPath,
      importedNativeId,
      sessionFileAfterImport: sessionFile,
      sessionFileAfterReuse: sessionFile2,
      ompChainImportPath: ompNewImports[0],
      foreignCodexSha256: beforeForeign.sha256,
      foreignCodexMtimeMs: beforeForeign.mtimeMs,
    }, null, 2),
    'utf8'
  );
}

async function runNativeOnlyPhase(page, meta) {
  await connectAndBoot(page);

  // Foreign rows must be absent under explicit sessionImportSources: [].
  await waitFor(
    page,
    (nativeId) => {
      const rows = [...document.querySelectorAll('.session-sidebar__switch')];
      const ids = rows.map((r) => r.dataset.sessionId || '');
      const sources = rows.map((r) => r.dataset.sessionSource || '');
      const hasNative = ids.includes(nativeId);
      const foreignSources = sources.filter((s) => s === 'omp' || s === 'codex' || s === 'grok' || s === 'grok/hyper');
      // Wait until the catalog has rendered at least the native seed, then
      // require zero foreign sources.
      return hasNative && foreignSources.length === 0;
    },
    'X6.1: foreign rows still present (or native seed missing) under sessionImportSources:[]',
    30000,
    meta.nativeId
  );
  const snap = await sidebarSnapshot(page);
  for (const id of [meta.ompId, meta.codexId, meta.grokId]) {
    if (snap.rows.some((r) => r.sessionId === id)) {
      fail(`X6.1: foreign session ${id} still listed under native-only policy`);
    }
  }
  for (const src of ['omp', 'codex', 'grok', 'grok/hyper']) {
    if (snap.rows.some((r) => r.source === src)) {
      fail(`X6.1: foreign source "${src}" still listed under native-only policy`);
    }
  }
  for (const label of ['OMP', 'Codex', 'Grok']) {
    if (snap.providerGroups.includes(label)) {
      fail(`X6.1: foreign provider group "${label}" still listed under native-only policy`);
    }
  }
  // Foreign files must still exist on disk (read-only discovery policy).
  for (const p of [meta.ompPath, meta.codexPath, meta.grokPath]) {
    if (!fs.existsSync(p)) fail(`X6.1: foreign source file was deleted under native-only policy: ${p}`);
  }
  record('X6.1');

  if (!snap.rows.some((r) => r.sessionId === meta.nativeId)) {
    fail(`X6.2: native seed ${meta.nativeId} missing under native-only policy`);
  }
  record('X6.2');
  await page.screenshot({ path: `${evidence}/external-x6-native-only.png`, fullPage: true });
}

async function main() {
  if (!url) fail('RPI_URL is required');
  if (!nativeSessionsRoot) fail('RPI_NATIVE_SESSIONS_ROOT is required');
  if (phase !== 'default' && phase !== 'native_only') {
    fail(`RPI_PHASE must be default|native_only (got ${phase})`);
  }
  const meta = loadSeedMeta();
  const phaseIds = PHASE_IDS[phase];

  const launchOptions = chromePath ? { executablePath: chromePath } : {};
  const browser = await chromium.launch(launchOptions);
  try {
    const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
    if (token) {
      await page.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
    }
    page.on('pageerror', (err) => {
      console.error(`web-external-sessions: page error: ${err.message}`);
    });

    if (phase === 'default') {
      await runDefaultPhase(page, meta);
    } else {
      await runNativeOnlyPhase(page, meta);
    }

    const missing = phaseIds.filter((id) => !executed.has(id));
    if (missing.length > 0) {
      fail(`${phase} phase evidence incomplete: ${missing.join(', ')} never executed`);
    }
    fs.mkdirSync(evidence, { recursive: true });
    const phaseEvidence = path.join(evidence, `coverage-assertions-${phase}.json`);
    fs.writeFileSync(phaseEvidence, JSON.stringify({ executed: [...executed] }, null, 2));
    console.log(
      `web-external-sessions: PASSED phase=${phase} (${executed.size}/${phaseIds.length} phase assertions; documented total ${DOCUMENTED_IDS.length}) — evidence at ${phaseEvidence}`
    );
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-external-sessions: playwright crashed: ${err && err.stack ? err.stack : err}`);
  process.exit(2);
});
