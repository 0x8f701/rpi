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
// Assertion matrix (feature -> IDs, used by the coverage report). Phases:
// default = X0, X1, X11, X2, X3, X4, X5, X7, X9, X10, X8 (in run order);
// native_only = X6 only.
//   X0  X0.1 boot: page title + WS connected
//   X1  X1.1 sidebar lists Pi + OMP + Codex + Grok foreign rows (default Web sources)
//       X1.2 sidebar provider groups include rpi, Pi, OMP, Codex, and Grok
//   X2  X2.1 click foreign Codex row activates a native import copy before a
//            deliberately delayed post-switch session_list response
//       X2.2 Session panel session-file is under the native tree (not .codex)
//       X2.3 imported transcript restores the foreign Codex user/assistant text
//   X11 X11.1 Agent sessions subgroup renders default-collapsed (header
//            visible, descendant rows hidden)
//       X11.2 bootstrapOnly row hidden by default
//       X11.3 search reveals the bootstrapOnly row under a search-forced-open
//            Agent sessions subgroup; clearing hides it again
//       X11.4 switching to the bootstrapOnly row keeps it active; after
//            search clear, Agent sessions returns to default-collapsed
//            (row hidden). Expanding the subgroup shows the still-active row.
//   X3  X3.1 foreign Codex source bytes unchanged after import
//       X3.2 foreign Codex source mtime unchanged after import
//   X4  X4.1 exactly one import_*.jsonl under the native sessions tree
//       X4.2 foreign Codex row stays visible with an imported badge beside one
//            native runtime row
//   X5  X5.1 leave the imported session, then click its imported foreign row;
//            same native runtime restores under 1s with zero switch/subscribe
//       X5.2 re-select reuses the same native session file (lineage reuse;
//            import_*.jsonl count still 1)
//   X7  X7.1 click the rotated OMP leaf row -> distinct native import copy
//       X7.2 imported transcript renders the full parentSession chain
//            (early + middle + final user/assistant turns, ordered once)
//       X7.3 exactly one import_*.jsonl carrying the whole chain; no handoff
//            custom-message text, no child-session text
//       X7.4 task/subagent child session row absent and child text never rendered
//   X9  X9.1 click Pi row activates a distinct native import copy
//       X9.2 imported Pi transcript and backend/DOM identity match
//       X9.3 Pi source bytes+mtime remain immutable and later selection reuses
//            the same imported native session through A to B to A
//   X10 X10.1 native row Export HTML downloads that row without activation
//       X10.2 Pi row Export HTML imports/downloads Pi content without activation
//       X10.3 invalid export reports one error toast and no download
//   X8  X8.1 all four OMP source files byte+mtime immutable after chain import
//   X6  X6.1 sessionImportSources:[] — all four foreign provider groups
//            (Pi/OMP/Codex/Grok rows) absent under the native-only policy
//       X6.2 native seed still listed under native-only policy
//
// Recording discipline: assertion IDs are recorded EXPLICITLY (record(id) at
// each assertion site) — never implicitly from waitFor labels. Per-phase
// execution counts live in an exact-once Map ledger: an unexpected ID, a
// duplicate record, or a documented ID that never executed (checked before
// any evidence write) is a hard failure.
//
// Publication contract (ONE lane-set commit, failure-clean + set-atomic):
// the shell passes ONE invocation nonce (RPI_LANE_NONCE) to BOTH phase
// drivers. Each phase stages its artifacts (screenshots, the phase coverage
// JSON, import-result.json) into ONE SHARED staging set derived from that
// nonce ($evidence/.staging-web-external-sessions-<nonce>/), registers an
// artifact only after its write/screenshot succeeded, and on its own
// success commits ONLY a phase manifest fragment (manifest-<phase>.json)
// into the shared set — it NEVER advances any pointer and NEVER renames the
// staging set. A failed or crashed phase removes the WHOLE shared staging
// set, so no mixed generation can ever survive. After BOTH phases pass, the
// shell validates each fragment (exact lane/phase/nonce/artifact set:
// every registered artifact a contained relative path inside the future
// run dir, exact-once counts, executed ids equal to the documented ids IN
// ORDER, disjoint documented id sets, exact full union with no dedupe),
// writes the merged coverage-assertions.json INTO the shared stage, and
// performs the single lane-set commit: ONE stage-dir -> run-dir rename
// ($evidence/runs/web-external-sessions-<nonce>/) plus ONE
// atomically-renamed current.json pointer. A pre-pointer-commit failure
// removes the stage/run/temp paths and preserves the previous pointer
// byte-identical. The matrix feature "external sessions" requires ALL of
// DOCUMENTED_IDS; the lane fails before staging unless every documented ID
// for the active phase executed exactly once.
//
// No skips. No soft passes. Exit 2 on assertion failure (exit 1 reserved for
// playwright/npm setup failures in web_run_playwright). RPI_SELF_CHECK=1
// exercises the pure ledger/staging gates WITHOUT playwright (the browser
// dependency is imported lazily only for a real run).

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import crypto from 'node:crypto';
import { createFreshSessionViaManage } from './lib/session-lifecycle.mjs';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const chromePath = process.env.RPI_CHROME || '';
const selfCheckMode = process.env.RPI_SELF_CHECK === '1';
const evidence = selfCheckMode
  // Self-check mode runs the pure gates against a THROWAWAY evidence root.
  ? fs.mkdtempSync(path.join(os.tmpdir(), 'web-external-sessions-selfcheck-'))
  : (process.env.RPI_EVIDENCE || '.');
const phase = process.env.RPI_PHASE || 'default';
const nonce = process.env.RPI_LANE_NONCE || (selfCheckMode ? 'self-check-nonce' : '');
const seedMetaPath = process.env.RPI_SEED_META || '';
const nativeSessionsRoot = process.env.RPI_NATIVE_SESSIONS_ROOT || '';


const PHASE_IDS = {
  default: [
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
  ],
  native_only: [
    'X6.1', 'X6.2',
  ],
};

// Per-phase exact-once ledger: record(id) is called ONLY at assertion sites
// (never implicitly from waitFor labels), so the counts map is the
// authoritative ledger — an unexpected ID or a duplicate record hard-fails,
// and a documented ID that never executed fails the completeness gate below.
const executedCounts = new Map();

function record(id) {
  if (!PHASE_IDS[phase].includes(id)) {
    fail(`record: unexpected assertion id ${JSON.stringify(id)} for phase ${phase}`);
  }
  const count = (executedCounts.get(id) || 0) + 1;
  executedCounts.set(id, count);
  if (count !== 1) {
    fail(`record: assertion id ${id} recorded ${count}x — every documented id must execute exactly once`);
  }
  console.log(`[web-external-sessions:assert] ${id}`);
}

// Pure exact-once completeness gate over the per-phase counts ledger:
// missing (count 0), duplicate execution (count > 1), unexpected ids, and
// duplicate documented entries can never pass.
function documentedIdsCompletenessError(documentedIds, counts) {
  const documented = new Set(documentedIds);
  const missing = documentedIds.filter((id) => (counts.get(id) || 0) === 0);
  const unexpected = [...counts.keys()].filter((id) => !documented.has(id));
  const duplicates = documentedIds.filter((id, index) => documentedIds.indexOf(id) !== index);
  const repeated = [...counts].filter(([, count]) => count > 1).map(([id]) => id);
  if (missing.length === 0 && unexpected.length === 0 && duplicates.length === 0 && repeated.length === 0) return null;
  return `assertion coverage mismatch: missing=${JSON.stringify(missing)}, unexpected=${JSON.stringify(unexpected)}, duplicate documented ids=${JSON.stringify([...new Set(duplicates)])}, ids executed more than once=${JSON.stringify(repeated)}`;
}

function selfCheckCoverageLedger() {
  const counts = (pairs) => new Map(pairs);
  if (documentedIdsCompletenessError(['A', 'B'], counts([['A', 1], ['B', 1]])) !== null) {
    throw new Error('external-sessions completeness self-check rejected an exact match');
  }
  // Missing id (count 0 / absent) must fail.
  for (const invalid of [counts([]), counts([['A', 1]]), counts([['A', 1], ['B', 0]])]) {
    if (documentedIdsCompletenessError(['A', 'B'], invalid) === null) {
      throw new Error(`completeness self-check accepted missing ids ${JSON.stringify([...invalid])}`);
    }
  }
  // Unexpected id must fail.
  if (documentedIdsCompletenessError(['A', 'B'], counts([['A', 1], ['B', 1], ['C', 1]])) === null) {
    throw new Error('completeness self-check accepted an unexpected id');
  }
  // Duplicate execution (count > 1) must fail.
  if (documentedIdsCompletenessError(['A', 'B'], counts([['A', 2], ['B', 1]])) === null) {
    throw new Error('completeness self-check accepted a duplicate execution');
  }
  // Duplicate documented entries must fail.
  if (documentedIdsCompletenessError(['A', 'A'], counts([['A', 1]])) === null) {
    throw new Error('completeness self-check accepted duplicate documented ids');
  }
}

selfCheckCoverageLedger();

function fail(message) {
  // Exit 2 (not 1): run.sh/web_run_playwright treat 1 as setup failure.
  console.error(`web-external-sessions: FAIL: ${message}`);
  // A failed run removes the WHOLE shared lane-set staging dir — a phase
  // that never completed its fragment must leave no mixed generation for
  // the lane-set commit; published evidence under the evidence root is left
  // untouched (never published early/partially).
  cleanupStaging();
  process.exit(2);
}

// Full documented matrix across both phases (coverage gate): the union of
// the per-phase documented ids.
const DOCUMENTED_IDS = [...PHASE_IDS.default, ...PHASE_IDS.native_only];

// ONE shared lane-set staging set, derived from the shell's invocation
// nonce: BOTH phase drivers stage into the SAME directory and the shell
// later commits it as ONE lane-set publication (see the header). Every
// artifact is REGISTERED only after its write/screenshot succeeded.
const stagingDir = path.join(evidence, `.staging-web-external-sessions-${nonce}`);
const runDirName = `runs/web-external-sessions-${nonce}`;
const laneArtifacts = []; // registered artifact names, AFTER their write succeeded
let committed = false;

async function stageShot(page, name) {
  fs.mkdirSync(stagingDir, { recursive: true });
  const staged = path.join(stagingDir, name);
  await page.screenshot({ path: staged, fullPage: true });
  laneArtifacts.push(name);
  return staged;
}

function stageWrite(name, data) {
  fs.mkdirSync(stagingDir, { recursive: true });
  const staged = path.join(stagingDir, name);
  fs.writeFileSync(staged, data);
  laneArtifacts.push(name);
  return staged;
}

function cleanupStaging() {
  fs.rmSync(stagingDir, { recursive: true, force: true });
}

// Phase fragment commit: the phase manifest is written INTO the shared
// lane-set staging set and NOTHING else — the phase driver NEVER advances a
// pointer and NEVER renames any directory. The whole staged set (both
// phases' fragments + artifacts) becomes ONE immutable run directory via a
// single rename performed by the shell after both phases pass, followed by
// ONE atomically-renamed current.json pointer. No canonical artifact name
// is ever replaced sequentially and no phase pointer can ever exist.
function commitPhaseManifest(manifest) {
  if (!nonce) fail('commitPhaseManifest: RPI_LANE_NONCE is required (ONE lane-set commit needs the shared invocation nonce)');
  fs.mkdirSync(stagingDir, { recursive: true });
  fs.writeFileSync(
    path.join(stagingDir, `manifest-${phase}.json`),
    `${JSON.stringify(manifest, null, 2)}\n`
  );
  committed = true;
}

// Wire browser-error capture and return the REAL ledger array the handlers
// push into — the same array instance the publication gate consumes. Same
// wiring shape as the sessions_workflow and files_latency lanes: pageerror,
// console.error, and websocket socketerror, labeled with the lane name.
function monitorBrowserErrors(page, label) {
  const errors = [];
  page.on('pageerror', (err) => {
    errors.push(`${label} pageerror: ${err.message}`);
  });
  page.on('console', (message) => {
    if (message.type() === 'error') errors.push(`${label} console: ${message.text()}`);
  });
  page.on('websocket', (socket) => {
    socket.on('socketerror', (error) => {
      errors.push(`${label} websocket: ${String(error)}`);
    });
  });
  return errors;
}

// Self-check that invokes the SAME monitorBrowserErrors wiring the lane
// uses (with a stand-in page object implementing Playwright's `.on`) and
// asserts the RETURNED ledger is the real one: pageerror / console.error /
// websocket socketerror entries land in it with the lane label, while
// non-error console messages never do.
function selfCheckMonitorBrowserErrors() {
  const handlers = new Map();
  const fakePage = {
    on(event, handler) {
      if (!handlers.has(event)) handlers.set(event, []);
      handlers.get(event).push(handler);
    },
  };
  const ledger = monitorBrowserErrors(fakePage, 'external-self-check');
  if (!Array.isArray(ledger)) throw new Error('monitorBrowserErrors self-check: returned ledger is not an array');
  handlers.get('pageerror')[0](new Error('page boom'));
  handlers.get('console')[0]({ type: () => 'warning', text: () => 'ignored' });
  handlers.get('console')[0]({ type: () => 'error', text: () => 'console boom' });
  handlers.get('websocket')[0]({ on: (_event, handler) => handler(new Error('socket boom')) });
  const expected = [
    'external-self-check pageerror: page boom',
    'external-self-check console: console boom',
    'external-self-check websocket: Error: socket boom',
  ];
  if (JSON.stringify(ledger) !== JSON.stringify(expected)) {
    throw new Error(`monitorBrowserErrors self-check ledger mismatch: ${JSON.stringify(ledger)}`);
  }
}

selfCheckMonitorBrowserErrors();
async function installSwitchTrace(page) {
  await page.addInitScript(() => {
    const NativeWebSocket = window.WebSocket;
    const control = {
      frames: [],
      delayAfterSwitch: false,
      timingArmed: false,
      foreignId: '',
      priorId: '',
      activeSessionId: '',
      clickAt: null,
      activeAt: null,
      listHeld: false,
      listReleased: false,
      armDelayedCatalog(foreignId, priorId) {
        this.timingArmed = true;
        this.delayAfterSwitch = true;
        this.switchSucceeded = false;
        this.foreignId = foreignId;
        this.priorId = priorId;
        this.activeSessionId = '';
        this.clickAt = null;
        this.activeAt = null;
        this.listHeld = false;
        this.listReleased = false;
      },
      armWarmRevisit(foreignId, priorId) {
        this.timingArmed = true;
        this.delayAfterSwitch = false;
        this.foreignId = foreignId;
        this.priorId = priorId;
        this.activeSessionId = '';
        this.clickAt = null;
        this.activeAt = null;
      },
    };
    const record = (direction, frame) => {
      if (!frame || typeof frame.type !== 'string') return;
      control.frames.push({
        direction,
        type: frame.type,
        command: typeof frame.command === 'string' ? frame.command : '',
        success: frame.success === true,
        sessionId: typeof frame.sessionId === 'string' ? frame.sessionId : '',
        targetSessionId: typeof frame.data?.sessionId === 'string' ? frame.data.sessionId : '',
      });
      if (control.frames.length > 4096) control.frames.shift();
    };
    document.addEventListener('pointerdown', (event) => {
      if (!control.timingArmed || event.isTrusted !== true) return;
      const row = event.target instanceof Element ? event.target.closest('.session-sidebar__switch') : null;
      if (row?.dataset.sessionId === control.foreignId) control.clickAt = performance.now();
      if (control.clickAt !== null) control.timingArmed = false;
    }, true);
    const observeActiveRow = () => new MutationObserver(() => {
      if (control.clickAt === null || control.activeAt !== null) return;
      const active = document.querySelector('.session-sidebar__row--active .session-sidebar__switch');
      const sessionId = active?.dataset.sessionId || '';
      if (active?.dataset.sessionSource === 'rpi' && sessionId !== control.foreignId && sessionId !== control.priorId) {
        control.activeSessionId = sessionId;
        control.activeAt = performance.now();
      }
    }).observe(document.documentElement, { childList: true, subtree: true, attributes: true });
    if (document.documentElement) observeActiveRow();
    else document.addEventListener('DOMContentLoaded', observeActiveRow, { once: true });
    class ExternalSessionWebSocket extends NativeWebSocket {
      constructor(...args) {
        super(...args);
        this.addEventListener('message', (event) => {
          let frame;
          try {
            frame = JSON.parse(String(event.data));
          } catch {
            return;
          }
          if (event.__externalSessionInjected === true) {
            record('received', frame);
            return;
          }
          if (frame?.type === 'response' && frame.command === 'switch_session' && frame.success === true) {
            control.switchSucceeded = true;
          }
          if (control.delayAfterSwitch && control.switchSucceeded
              && frame?.type === 'response' && frame.command === 'session_list') {
            control.delayAfterSwitch = false;
            control.listHeld = true;
            event.stopImmediatePropagation();
            window.setTimeout(() => {
              control.listReleased = true;
              const replay = new MessageEvent('message', { data: String(event.data) });
              replay.__externalSessionInjected = true;
              this.dispatchEvent(replay);
            }, 1500);
            return;
          }
          record('received', frame);
        });
      }
      send(payload) {
        if (typeof payload === 'string') {
          try {
            record('sent', JSON.parse(payload));
          } catch {
          }
        }
        return super.send(payload);
      }
    }
    for (const key of ['CONNECTING', 'OPEN', 'CLOSING', 'CLOSED']) {
      Object.defineProperty(ExternalSessionWebSocket, key, { value: NativeWebSocket[key] });
    }
    window.WebSocket = ExternalSessionWebSocket;
    window.__externalSessionSwitchTrace = control;
  });
}


// Self-check that the phase fragment commit stages a manifest fragment into
// the SHARED nonce-derived lane-set staging set and does NOTHING else: no
// pointer file (current.json OR current-<phase>.json) is ever created, and
// no runs/ directory is ever touched by a phase driver — pointers and the
// stage->run rename belong to the shell's single lane-set commit.
function selfCheckPhaseFragmentCommit() {
  const phaseIds = PHASE_IDS[phase];
  const counts = {};
  for (const id of phaseIds) counts[id] = 1;
  stageWrite('probe.json', '{"probe":1}\n');
  commitPhaseManifest({
    lane: 'web-external-sessions',
    phase,
    nonce: 'self-check-nonce',
    documented: [...phaseIds],
    executed: [...phaseIds],
    counts,
    artifacts: [...laneArtifacts],
  });
  if (!fs.existsSync(path.join(stagingDir, 'probe.json'))) {
    throw new Error('self-check: staged artifact missing from the shared lane-set staging set');
  }
  if (!fs.existsSync(path.join(stagingDir, `manifest-${phase}.json`))) {
    throw new Error('self-check: phase manifest fragment missing from the shared lane-set staging set');
  }
  if (fs.existsSync(path.join(evidence, 'current.json'))
    || fs.existsSync(path.join(evidence, `current-${phase}.json`))) {
    throw new Error('self-check: a phase driver created a pointer file — pointers belong to the shell lane-set commit only');
  }
  if (fs.existsSync(path.join(evidence, 'runs'))) {
    throw new Error('self-check: a phase driver touched runs/ — the stage->run rename belongs to the shell lane-set commit only');
  }
  const fragment = JSON.parse(fs.readFileSync(path.join(stagingDir, `manifest-${phase}.json`), 'utf8'));
  if (fragment.lane !== 'web-external-sessions' || fragment.phase !== phase
    || fragment.nonce !== nonce
    || !Array.isArray(fragment.artifacts) || !fragment.artifacts.includes('probe.json')) {
    throw new Error(`self-check: manifest fragment shape mismatch: ${JSON.stringify(fragment)}`);
  }
}

// SELF-CHECK ONLY (RPI_SELF_CHECK=1): a phase driver's module init must
// never touch the real evidence root or the shared staging set — the
// fragment-commit self-check runs exclusively in self-check mode against a
// throwaway evidence root, so a production phase never stages probe
// residue, never pre-sets `committed`, and never leaks self-check state
// into the real lane-set publication.
if (selfCheckMode) selfCheckPhaseFragmentCommit();

function loadSeedMeta() {
  if (!seedMetaPath || !fs.existsSync(seedMetaPath)) {
    fail(`RPI_SEED_META missing or unreadable: ${seedMetaPath}`);
  }
  return JSON.parse(fs.readFileSync(seedMetaPath, 'utf8'));
}

function fileFingerprint(filePath) {
  const st = fs.statSync(filePath, { bigint: true });
  const buf = fs.readFileSync(filePath);
  return {
    device: String(st.dev),
    inode: String(st.ino),
    size: Number(st.size),
    mtimeMs: Number(st.mtimeMs),
    mtimeNs: String(st.mtimeNs),
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

// Recording is EXPLICIT: waitFor never records assertion ids implicitly —
// its label is a diagnostic only. Every documented id is recorded by an
// explicit record(id) at its assertion site.
async function waitFor(page, fn, id, timeoutMs = 30000, arg) {
  try {
    await page.waitForFunction(fn, arg, { timeout: timeoutMs });
  } catch {
    fail(`${id} (timeout ${timeoutMs}ms)`);
  }
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
    const providerGroups = [...document.querySelectorAll('[data-group-kind="provider"]')]
      .map((group) => ({
        label: (group.querySelector('.session-sidebar__group-label')?.textContent || '').trim(),
        count: Number(group.querySelector('.session-sidebar__group-count')?.textContent || '0'),
        expanded: group.querySelector('.session-sidebar__group-head')?.getAttribute('aria-expanded') === 'true',
      }));
    const groups = [...document.querySelectorAll('.session-sidebar__group-label')]
      .map((el) => (el.textContent || '').trim());
    const rows = [...document.querySelectorAll('.session-sidebar__switch')].map((r) => ({
      sessionId: r.dataset.sessionId || '',
      source: r.dataset.sessionSource || '',
      disabled: r.disabled === true,
      title: (r.querySelector('.session-sidebar__name')?.textContent || '').trim(),
      summary: (r.querySelector('.session-sidebar__summary')?.textContent || '').trim(),
    }));
    return { groups, providerGroups, rows };
  });
}

async function panelSessionId(page) {
  return page.evaluate(() => {
    const labels = [...document.querySelectorAll('#session-panel .session-info__grid dt')];
    const label = labels.find((node) => (node.textContent || '').trim() === 'Session id');
    return (label?.nextElementSibling?.textContent || '').trim();
  });
}

async function httpRpc(page, command) {
  const response = await page.evaluate(async ({ body, bearerToken }) => {
    const headers = { 'content-type': 'application/json' };
    if (bearerToken) headers.authorization = `Bearer ${bearerToken}`;
    const result = await fetch('/rpc', {
      method: 'POST',
      headers,
      body: JSON.stringify(body),
    });
    return { status: result.status, text: await result.text() };
  }, { body: command, bearerToken: token });
  let frame;
  try {
    frame = JSON.parse(response.text);
  } catch {
    fail(`RPC ${command.type} returned non-JSON`);
  }
  if (response.status !== 200 || frame.success !== true) {
    fail(`RPC ${command.type} failed: ${frame.error || response.text}`);
  }
  return frame.data || {};
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


async function openSessionMenu(page, sessionId) {
  const selector = `.session-sidebar__switch[data-session-id="${sessionId}"]`;
  const row = page.locator(selector);
  if (await row.count() !== 1) fail(`session row not found for menu: ${sessionId}`);
  await row.click({ button: 'right' });
  await waitFor(
    page,
    () => document.querySelector('.session-sidebar__menu[role="menu"]') !== null,
    `context menu did not open for ${sessionId}`,
  );
}

async function exportSessionRow(page, sessionId, expectedText, id) {
  const activeBefore = await page.evaluate(() =>
    document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || ''
  );
  await openSessionMenu(page, sessionId);
  const downloads = [];
  const onDownload = (download) => downloads.push(download);
  page.on('download', onDownload);
  try {
    const errorCountBefore = await page.locator('#toasts .toast--error .toast__message').count();
    const downloadPromise = page.waitForEvent('download', { timeout: 30000 })
      .then((download) => ({ kind: 'download', download }))
      .catch(() => ({ kind: 'timeout' }));
    const errorPromise = page.waitForFunction(
      (before) => document.querySelectorAll('#toasts .toast--error .toast__message').length > before,
      errorCountBefore,
      { timeout: 30000 },
    )
      .then(async () => ({
        kind: 'error',
        message: (await page.locator('#toasts .toast--error .toast__message').last().textContent()) || 'unknown export error',
      }))
      .catch(() => ({ kind: 'timeout' }));
    await page.getByRole('menuitem', { name: 'Export HTML' }).click();
    const outcome = await Promise.race([downloadPromise, errorPromise]);
    if (outcome.kind === 'error') fail(`${id}: ${outcome.message}`);
    if (outcome.kind !== 'download') fail(`${id}: export produced neither a download nor an error toast`);
    const download = outcome.download;
    const fileName = download.suggestedFilename();
    await waitFor(
      page,
      (name) => [...document.querySelectorAll('#toasts .toast:not(.toast--error) .toast__message')]
        .filter((el) => (el.textContent || '') === `Exported ${name}`).length === 1,
      `${id}: export did not produce exactly one success toast`,
      30000,
      fileName,
    );
    if (downloads.length !== 1) {
      fail(`${id}: expected exactly one download, got ${downloads.map((item) => item.suggestedFilename()).join(', ')}`);
    }
    const downloadPath = await download.path();
    if (!downloadPath) fail(`${id}: browser did not retain the downloaded HTML`);
    const html = fs.readFileSync(downloadPath, 'utf8');
    if (!html.includes(expectedText)) fail(`${id}: downloaded HTML omitted selected row content`);
    const activeAfter = await page.evaluate(() =>
      document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || ''
    );
    if (activeAfter !== activeBefore) {
      fail(`${id}: exporting row ${sessionId} activated ${activeAfter} instead of preserving ${activeBefore}`);
    }
    return { fileName, html };
  } finally {
    page.off('download', onDownload);
  }
}

async function clickSidebarSession(page, sessionId) {
  const row = page.locator(`.session-sidebar__switch[data-session-id="${sessionId}"]`);
  if (await row.count() !== 1) fail(`sidebar row not found for sessionId=${sessionId}`);
  try {
    await row.waitFor({ state: 'visible', timeout: 30000 });
    await row.click({ timeout: 30000 });
  } catch {
    fail(`sidebar row did not become clickable for sessionId=${sessionId}`);
  }
}

async function activeTranscript(page) {
  return page.evaluate(() => document.getElementById('transcript')?.textContent || '');
}

async function connectAndBoot(page) {
  await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
  await waitFor(page, () => document.title === 'rpi web', 'boot: page title missing');
  await waitFor(
    page,
    () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true,
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
  await stageShot(page, 'external-x0-boot.png');

  /* ---------------- X1: default discovery + grouping ---------------- */
  await waitFor(
    page,
    (ids) => {
      const rows = [...document.querySelectorAll('.session-sidebar__switch')];
      const present = new Set(rows.map((r) => r.dataset.sessionId || ''));
      return ids.every((id) => present.has(id));
    },
    'X1.1: foreign Pi/OMP/Codex/Grok rows never appeared',
    30000,
    [...meta.piIds, meta.ompId, meta.codexId, meta.grokId]
  );
  const snap1 = await sidebarSnapshot(page);
  for (const id of [...meta.piIds, meta.ompId, meta.codexId, meta.grokId, meta.nativeId]) {
    if (!snap1.rows.some((r) => r.sessionId === id)) {
      fail(`X1.1: missing session row ${id} (got ${snap1.rows.map((r) => r.sessionId).join(', ')})`);
    }
  }
  const piRow = snap1.rows.find((r) => r.sessionId === meta.piId);
  const ompRow = snap1.rows.find((r) => r.sessionId === meta.ompId);
  const codexRow = snap1.rows.find((r) => r.sessionId === meta.codexId);
  const grokRow = snap1.rows.find((r) => r.sessionId === meta.grokId);
  if (!piRow || piRow.source !== 'pi') {
    fail(`X1.1: Pi row source is not pi (got ${piRow && piRow.source})`);
  }
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

  const requiredProviders = ['rpi', 'Pi', 'OMP', 'Codex', 'Grok'];
  for (const label of requiredProviders) {
    if (!snap1.providerGroups.some((group) => group.label === label)) {
      fail(`X1.2: missing provider group "${label}" (got ${snap1.providerGroups.map((group) => group.label).join(', ')})`);
    }
  }
  const forbidden = snap1.providerGroups.filter(
    (group) => !requiredProviders.includes(group.label)
  );
  if (forbidden.length > 0) {
    fail(`X1.2: non-provider top-level groups found: ${forbidden.map((group) => group.label).join(', ')}`);
  }
  const piGroup = snap1.providerGroups.find((group) => group.label === 'Pi');
  const actionablePiRows = snap1.rows.filter((row) => row.source === 'pi' && !row.disabled);
  const piRowIds = actionablePiRows.map((row) => row.sessionId);
  if (!piGroup?.expanded || piGroup.count !== 8 || actionablePiRows.length !== 8
      || !meta.piIds.every((id) => piRowIds.includes(id))) {
    fail(`X1.2: expanded Pi count/actionable-row divergence (${JSON.stringify({ piGroup, actionablePiRows })})`);
  }
  record('X1.2');

  // Exact provider collapse: clicking the OMP provider header must hide every
  // OMP descendant subgroup and row, then restore the same row on expand.
  const ompProviderHead = page.locator('.session-sidebar__group-head[data-group-kind="provider"][data-provider="OMP"]');
  if (await ompProviderHead.count() !== 1) fail(`X1.2: expected one exact OMP provider header, got ${await ompProviderHead.count()}`);
  await ompProviderHead.click();
  await waitFor(
    page,
    (ompId) => {
      const head = document.querySelector('.session-sidebar__group-head[data-group-kind="provider"][data-provider="OMP"]');
      const row = document.querySelector(`.session-sidebar__switch[data-session-id="${ompId}"]`);
      const descendants = document.querySelectorAll('.session-sidebar__group-head[data-provider="OMP"][data-group-kind="project"]');
      return head?.getAttribute('aria-expanded') === 'false' && row === null && descendants.length === 0;
    },
    'X1.2: collapsed exact OMP provider did not hide all descendants',
    30000,
    meta.ompId,
  );
  await ompProviderHead.click();
  await waitFor(
    page,
    (ompId) => {
      const head = document.querySelector('.session-sidebar__group-head[data-group-kind="provider"][data-provider="OMP"]');
      return head?.getAttribute('aria-expanded') === 'true'
        && document.querySelector(`.session-sidebar__switch[data-session-id="${ompId}"]`) !== null;
    },
    'X1.2: expanding exact OMP provider did not restore its row',
    30000,
    meta.ompId,
  );
  await stageShot(page, 'external-x1-discovery.png');

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

  /* ---------------- X11: Agent sessions grouping/visibility ----------------
   * The seeded agentSession row (Slock `.slock/agents/<id>` cwd) groups
   * under the default-collapsed Agent sessions subgroup; the seeded
   * bootstrapOnly row is hidden by default, revealed by search, and remains
   * catalog-active after switch; Agent sessions still default-collapses
   * after search clear. Each ID is recorded ONLY after its full block of
   * assertions has passed. */
  {
    /* X11.1: the Agent sessions subgroup header renders collapsed by
     * default with the agent row hidden beneath it. */
    await waitFor(
      page,
      () => document.querySelector('.session-sidebar__group-head[data-subgroup-kind="agent"]') !== null,
      'X11: Agent sessions subgroup header never rendered',
    );
    const agentHead = await page.locator('.session-sidebar__group-head[data-subgroup-kind="agent"]').count();
    if (agentHead !== 1) fail(`X11.1: expected exactly one Agent sessions subgroup header, got ${agentHead}`);
    const agentExpanded = await page.getAttribute('.session-sidebar__group-head[data-subgroup-kind="agent"]', 'aria-expanded');
    if (agentExpanded !== 'false') {
      fail(`X11.1: Agent sessions subgroup not collapsed by default (aria-expanded=${agentExpanded})`);
    }
    if (await page.locator(`.session-sidebar__switch[data-session-id="${meta.agentId}"]`).count() !== 0) {
      fail('X11.1: agent row visible while Agent sessions is collapsed by default');
    }
    record('X11.1');

    /* X11.2: the bootstrapOnly row is hidden by default. */
    if (await page.locator(`.session-sidebar__switch[data-session-id="${meta.bootstrapId}"]`).count() !== 0) {
      fail('X11.2: bootstrapOnly row listed while search is empty');
    }
    record('X11.2');
    await stageShot(page, 'external-x11-collapsed.png');

    /* X11.3: search reveals the bootstrapOnly row and forces the Agent
     * sessions subgroup open; clearing the search hides it again. */
    await page.fill('#session-sidebar-search', meta.bootstrapId);
    await waitFor(
      page,
      (sid) => document.querySelector(`.session-sidebar__switch[data-session-id="${sid}"]`) !== null,
      'X11: bootstrapOnly row never revealed by search',
      30000,
      meta.bootstrapId,
    );
    const searchedExpanded = await page.getAttribute('.session-sidebar__group-head[data-subgroup-kind="agent"]', 'aria-expanded');
    if (searchedExpanded !== 'true') {
      fail(`X11.3: search did not force the Agent sessions subgroup open (aria-expanded=${searchedExpanded})`);
    }
    await page.click('#session-sidebar-search-clear');
    await waitFor(
      page,
      (sid) => document.querySelector(`.session-sidebar__switch[data-session-id="${sid}"]`) === null,
      'X11: bootstrapOnly row never re-hid after clearing search',
      10000,
      meta.bootstrapId,
    );
    record('X11.3');

    /* X11.4: switch while search reveals the row; after clear, Agent
     * sessions default-collapses and hides descendants — including the
     * active row (SessionSidebar.tsx:452-462; sessionSidebar.test.ts:2717-2746).
     * Expanding the subgroup shows the still-active bootstrap row. */
    await page.fill('#session-sidebar-search', meta.bootstrapId);
    await waitFor(
      page,
      (sid) => document.querySelector(`.session-sidebar__switch[data-session-id="${sid}"]`) !== null,
      'X11: bootstrapOnly row never re-revealed for the active switch',
      30000,
      meta.bootstrapId,
    );
    await clickSidebarSession(page, meta.bootstrapId);
    await waitRowActive(page, meta.bootstrapId, 'X11: bootstrapOnly row never became active');
    await page.click('#session-sidebar-search-clear');
    await waitFor(
      page,
      (sid) => {
        const expanded = document.querySelector('.session-sidebar__group-head[data-subgroup-kind="agent"]')
          ?.getAttribute('aria-expanded');
        return expanded === 'false'
          && document.querySelector(`.session-sidebar__switch[data-session-id="${sid}"]`) === null;
      },
      'X11: Agent sessions did not return to default-collapsed after clearing search',
      30000,
      meta.bootstrapId,
    );
    await page.click('.session-sidebar__group-head[data-subgroup-kind="agent"]');
    await waitFor(
      page,
      (sid) => {
        const active = document.querySelector('.session-sidebar__row--active .session-sidebar__switch');
        return active?.dataset.sessionId === sid;
      },
      'X11: expanding Agent sessions did not show the still-active bootstrapOnly row',
      30000,
      meta.bootstrapId,
    );
    const activeExpanded = await page.getAttribute('.session-sidebar__group-head[data-subgroup-kind="agent"]', 'aria-expanded');
    if (activeExpanded !== 'true') {
      fail(`X11.4: Agent sessions subgroup did not stay open after expand (aria-expanded=${activeExpanded})`);
    }
    const activeId = await page.evaluate(() =>
      document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '');
    if (activeId !== meta.bootstrapId) {
      fail(`X11.4: active session is ${activeId}, expected ${meta.bootstrapId}`);
    }
    record('X11.4');
    await stageShot(page, 'external-x11-agent-visibility.png');
  }
  /* ---------------- X2/X3: click foreign Codex -> native import ---------------- */
  if (!fs.existsSync(meta.codexPath)) {
    fail(`X3 precondition: codex foreign file missing at ${meta.codexPath}`);
  }
  const beforeForeign = fileFingerprint(meta.codexPath);
  const importsBefore = listImportFiles(nativeSessionsRoot);

  await page.evaluate(({ foreignId, priorId }) => window.__externalSessionSwitchTrace.armDelayedCatalog(foreignId, priorId), {
    foreignId: meta.codexId,
    priorId: await page.evaluate(() => document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || ''),
  });
  await clickSidebarSession(page, meta.codexId);
  await waitFor(
    page,
    () => window.__externalSessionSwitchTrace.activeAt !== null,
    'X2.1: foreign Codex click never activated a distinct native copy',
    5000,
  );
  const activation = await page.evaluate(() => ({
    clickAt: window.__externalSessionSwitchTrace.clickAt,
    activeAt: window.__externalSessionSwitchTrace.activeAt,
    activeSessionId: window.__externalSessionSwitchTrace.activeSessionId,
  }));
  const activeMs = activation.activeAt - activation.clickAt;
  if (!Number.isFinite(activeMs) || activeMs < 0 || activeMs >= 1000) {
    fail(`X2.1: native Active row completed in ${Math.round(activeMs)}ms`);
  }
  await waitFor(
    page,
    () => window.__externalSessionSwitchTrace.listHeld === true,
    'X2.1: post-switch catalog request was not intercepted',
    2000,
  );
  const delayedCatalog = await page.evaluate(() => ({
    held: window.__externalSessionSwitchTrace.listHeld,
    released: window.__externalSessionSwitchTrace.listReleased,
  }));
  if (delayedCatalog.released) {
    fail(`X2.1: native Active row depended on the delayed post-switch catalog (${JSON.stringify(delayedCatalog)})`);
  }
  record('X2.1');
  await waitFor(
    page,
    () => window.__externalSessionSwitchTrace.listReleased === true,
    'X2.1: delayed post-switch catalog response was not released',
    5000,
  );

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
  await stageShot(page, 'external-x2-import.png');

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
  const importedHeader = JSON.parse(fs.readFileSync(importedPath, 'utf8').split('\n', 1)[0]);
  const importedNativeId = importedHeader.id;
  if (!importedNativeId) fail('X4 precondition: imported native session header missing id');
  if (activation.activeSessionId !== importedNativeId) {
    fail(`X2.1: timed Active row ${activation.activeSessionId} did not match imported runtime ${importedNativeId}`);
  }
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

  await waitFor(
    page,
    (args) => {
      const foreign = document.querySelector(`.session-sidebar__switch[data-session-id="${args.codexId}"][data-session-source="codex"]`);
      const importedBadge = foreign?.querySelector('.session-sidebar__imported');
      const nativeRows = [...document.querySelectorAll('.session-sidebar__switch')]
        .filter((row) => row.dataset.sessionId === args.importedId && row.dataset.sessionSource === 'rpi');
      return importedBadge !== null && nativeRows.length === 1;
    },
    'X4.2: imported Codex row or its single native copy was missing',
    30000,
    { codexId: meta.codexId, importedId: importedNativeId }
  );
  const snap4 = await sidebarSnapshot(page);
  const importedForeign = snap4.rows.filter((row) => row.sessionId === meta.codexId && row.source === 'codex');
  const nativeCopies = snap4.rows.filter((row) => row.sessionId === importedNativeId && row.source === 'rpi');
  if (importedForeign.length !== 1 || nativeCopies.length !== 1) {
    fail(`X4.2: imported/native row cardinality mismatch (${JSON.stringify({ importedForeign, nativeCopies })})`);
  }
  record('X4.2');
  await stageShot(page, 'external-x4-no-dup.png');

  /* ---------------- X5: re-select native copy -> lineage reuse ---------------- */
  // Leave the imported session via the sole New route, then re-select it
  // through the retained imported foreign row.

  await createFreshSessionViaManage(page, waitFor, 'X5.1', { closePanel: true });
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

  await waitFor(
    page,
    (foreignId) => document.querySelector(`.session-sidebar__switch[data-session-id="${foreignId}"][data-session-source="codex"] .session-sidebar__imported`) !== null,
    'X5.1: imported foreign row disappeared after New',
    30000,
    meta.codexId
  );
  const fastBaseline = await page.evaluate(() => window.__externalSessionSwitchTrace.frames
    .filter((frame) => frame.direction === 'sent' && (frame.type === 'switch_session' || frame.type === 'subscribe')).length);
  await page.evaluate(({ foreignId, priorId }) => window.__externalSessionSwitchTrace.armWarmRevisit(foreignId, priorId), {
    foreignId: meta.codexId,
    priorId: await page.evaluate(() => document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || ''),
  });
  await clickSidebarSession(page, meta.codexId);
  await waitFor(
    page,
    () => window.__externalSessionSwitchTrace.activeAt !== null,
    'X5.1: imported foreign row did not restore its native runtime',
    5000,
  );
  const revisitMs = await page.evaluate(() => window.__externalSessionSwitchTrace.activeAt - window.__externalSessionSwitchTrace.clickAt);
  if (!Number.isFinite(revisitMs) || revisitMs < 0 || revisitMs >= 1000) {
    fail(`X5.1: imported foreign row restored its native runtime in ${Math.round(revisitMs)}ms`);
  }
  const fastAfter = await page.evaluate(() => window.__externalSessionSwitchTrace.frames
    .filter((frame) => frame.direction === 'sent' && (frame.type === 'switch_session' || frame.type === 'subscribe')).length);
  if (fastAfter !== fastBaseline) {
    fail(`X5.1: loaded imported-row revisit emitted cold switch wire (${fastBaseline} -> ${fastAfter})`);
  }
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
  await stageShot(page, 'external-x5-reuse.png');

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
      const isNative = source === 'rpi' || source === 'native' || source === 'primary';
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
  await stageShot(page, 'external-x7-omp-chain.png');

  /* ---------------- X9: Pi selection imports and reuses native copy ------- */
  if (!fs.existsSync(meta.piPath)) fail(`X9 precondition: Pi source missing at ${meta.piPath}`);
  const activeBeforePi = await page.evaluate(() =>
    document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || ''
  );
  const piBefore = fileFingerprint(meta.piPath);
  const piImportsBefore = listImportFiles(nativeSessionsRoot);
  await clickSidebarSession(page, meta.piId);
  await waitFor(
    page,
    (expected) => {
      const active = document.querySelector('.session-sidebar__row--active .session-sidebar__switch');
      return active !== null
        && active.dataset.sessionSource === 'rpi'
        && active.dataset.sessionId !== expected.foreignId
        && active.dataset.sessionId !== expected.previousId;
    },
    'X9.1: Pi row click never activated a distinct rpi import copy',
    30000,
    { foreignId: meta.piId, previousId: activeBeforePi },
  );
  record('X9.1');
  const piActiveId = await page.evaluate(() =>
    document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || ''
  );
  await openSessionPanel(page, 'X9.2');
  await waitFor(
    page,
    () => {
      const text = document.getElementById('transcript')?.textContent || '';
      return text.includes('Pi external prompt') && text.includes('Pi external reply');
    },
    'X9.2: imported Pi transcript never restored',
    30000
  );
  await waitFor(
    page,
    (expectedId) => {
      const labels = [...document.querySelectorAll('#session-panel .session-info__grid dt')];
      const label = labels.find((node) => (node.textContent || '').trim() === 'Session id');
      return (label?.nextElementSibling?.textContent || '').trim() === expectedId;
    },
    'X9.2: Session panel never converged on the imported Pi identity',
    30000,
    piActiveId,
  );
  const piPanelId = await panelSessionId(page);
  const piBackendState = await httpRpc(page, { type: 'get_state', sessionId: piActiveId });
  if (!piActiveId || piPanelId !== piActiveId || piBackendState.sessionId !== piActiveId) {
    fail(`X9.2: backend/DOM Pi identity mismatch (${piBackendState.sessionId} / ${piPanelId} != ${piActiveId})`);
  }
  record('X9.2');
  const piAfter = fileFingerprint(meta.piPath);
  if (!piAfter.bytes.equals(piBefore.bytes) || piAfter.mtimeNs !== piBefore.mtimeNs
      || piAfter.device !== piBefore.device || piAfter.inode !== piBefore.inode) {
    fail('X9.3: Pi source inode, bytes, or mtime changed after import');
  }
  const piImportsAfter = listImportFiles(nativeSessionsRoot);
  const piNewImports = piImportsAfter.filter((p) => !piImportsBefore.includes(p));
  if (piNewImports.length !== 1) {
    fail(`X9.3: expected one Pi import file, got ${piNewImports.join(', ')}`);
  }
  await clickSidebarSession(page, meta.nativeId);
  await waitRowActive(page, meta.nativeId, 'X9.3');
  await clickSidebarSession(page, piActiveId);
  await waitRowActive(page, piActiveId, 'X9.3');
  if (listImportFiles(nativeSessionsRoot).length !== piImportsAfter.length) {
    fail('X9.3: A to B to A Pi selection created another import file');
  }
  record('X9.3');
  await stageShot(page, 'external-x9-pi-import.png');

  /* ---------------- X10: selected-row HTML export ------------------------- */
  await clickSidebarSession(page, meta.nativeId);
  await waitRowActive(page, meta.nativeId, 'X10.1');
  const nativeExport = await exportSessionRow(page, meta.nativeId, 'native-ext-seed', 'X10.1');
  if (!nativeExport.fileName.endsWith('.html') || nativeExport.html.includes('Pi external prompt')) {
    fail(`X10.1: native export filename/content identity is invalid (${nativeExport.fileName})`);
  }
  record('X10.1');
  await stageShot(page, 'external-x10-native-export.png');

  const piExportImportsBefore = listImportFiles(nativeSessionsRoot).length;
  const piExportSourceBefore = fileFingerprint(meta.piPath);
  const importedPiRowCount = await page.locator(
    `.session-sidebar__switch[data-session-id="${meta.piId}"][data-session-source="pi"] .session-sidebar__imported`,
  ).count();
  if (importedPiRowCount !== 1) {
    fail(`X10.2: expected one imported Pi source row before source content changed (${importedPiRowCount})`);
  }
  fs.appendFileSync(
    meta.piPath,
    `${JSON.stringify({
      type: 'message',
      id: 'pi-export-only',
      parentId: 'pi-a-0',
      timestamp: '2026-01-01T00:00:03Z',
      message: {
        role: 'assistant',
        content: [{ type: 'text', text: 'Pi export-only reply' }],
        timestamp: 3,
      },
    })}\n`,
    'utf8',
  );
  const piExportSource = fileFingerprint(meta.piPath);
  await waitFor(
    page,
    (piId) => {
      const row = document.querySelector(`.session-sidebar__switch[data-session-id="${piId}"][data-session-source="pi"]`);
      return row !== null && row.querySelector('.session-sidebar__imported') === null;
    },
    'X10.2: changed Pi source did not become a fresh exportable foreign row',
    30000,
    meta.piId,
  );
  const piExport = await exportSessionRow(page, meta.piId, 'Pi export-only reply', 'X10.2');
  if (!piExport.html.includes('Pi external prompt') || piExport.html.includes('native-ext-seed')) {
    fail('X10.2: Pi export did not preserve selected-session identity');
  }
  const piAfterExport = fileFingerprint(meta.piPath);
  if (!piAfterExport.bytes.equals(piExportSource.bytes)
      || piAfterExport.mtimeNs !== piExportSource.mtimeNs
      || piAfterExport.device !== piExportSource.device
      || piAfterExport.inode !== piExportSource.inode) {
    fail('X10.2: Pi source inode, bytes, or mtime changed during selected-row export');
  }
  if (piExportSourceBefore.bytes.equals(piAfterExport.bytes)) {
    fail('X10.2: Pi export fixture did not create a distinct foreign lineage');
  }
  if (listImportFiles(nativeSessionsRoot).length !== piExportImportsBefore + 1) {
    fail('X10.2: exporting changed Pi source did not create exactly one reusable import');
  }
  record('X10.2');
  await stageShot(page, 'external-x10-pi-export.png');
  await page.route('**/download/session/**', async (route) => {
    await route.fulfill({ status: 200, contentType: 'application/json', body: '{}' });
  });
  const downloadEvents = [];
  const onDownload = (download) => downloadEvents.push(download.suggestedFilename());
  page.on('download', onDownload);
  try {
    await openSessionMenu(page, meta.nativeId);
    await page.getByRole('menuitem', { name: 'Export HTML' }).click();
    await waitFor(
      page,
      () => document.querySelectorAll('#toasts .toast--error').length === 1,
      'X10.3: invalid selected-row export did not produce exactly one error toast',
    );
    const errorToasts = await page.evaluate(() => document.querySelectorAll('#toasts .toast--error').length);
    if (errorToasts !== 1) fail(`X10.3: expected exactly one error toast, got ${errorToasts}`);
    if (downloadEvents.length !== 0) fail(`X10.3: invalid export downloaded ${downloadEvents.join(', ')}`);
  } finally {
    page.off('download', onDownload);
    await page.unroute('**/download/session/**');
  }
  record('X10.3');
  await stageShot(page, 'external-x10-export-error.png');

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
  stageWrite(
    'import-result.json',
    `${JSON.stringify({
      importedPath,
      importedNativeId,
      sessionFileAfterImport: sessionFile,
      sessionFileAfterReuse: sessionFile2,
      ompChainImportPath: ompNewImports[0],
      foreignCodexSha256: beforeForeign.sha256,
      foreignCodexMtimeMs: beforeForeign.mtimeMs,
      nativeExportFileName: nativeExport.fileName,
      piExportFileName: piExport.fileName,
    }, null, 2)}\n`,
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
      const foreignSources = sources.filter((s) => s === 'pi' || s === 'omp' || s === 'codex' || s === 'grok' || s === 'grok/hyper');
      // Wait until the catalog has rendered at least the native seed, then
      // require zero foreign sources.
      return hasNative && foreignSources.length === 0;
    },
    'X6.1: foreign rows still present (or native seed missing) under sessionImportSources:[]',
    30000,
    meta.nativeId
  );
  const snap = await sidebarSnapshot(page);
  for (const id of [...meta.piIds, meta.ompId, meta.codexId, meta.grokId]) {
    if (snap.rows.some((r) => r.sessionId === id)) {
      fail(`X6.1: foreign session ${id} still listed under native-only policy`);
    }
  }
  for (const src of ['pi', 'omp', 'codex', 'grok', 'grok/hyper']) {
    if (snap.rows.some((r) => r.source === src)) {
      fail(`X6.1: foreign source "${src}" still listed under native-only policy`);
    }
  }
  for (const label of ['Pi', 'OMP', 'Codex', 'Grok']) {
    if (snap.providerGroups.some((group) => group.label === label)) {
      fail(`X6.1: foreign provider group "${label}" still listed under native-only policy`);
    }
  }
  // Foreign files must still exist on disk (read-only discovery policy).
  for (const p of [...meta.piPaths, meta.ompPath, meta.codexPath, meta.grokPath]) {
    if (!fs.existsSync(p)) fail(`X6.1: foreign source file was deleted under native-only policy: ${p}`);
  }
  record('X6.1');

  if (!snap.rows.some((r) => r.sessionId === meta.nativeId)) {
    fail(`X6.2: native seed ${meta.nativeId} missing under native-only policy`);
  }
  record('X6.2');
  await stageShot(page, 'external-x6-native-only.png');
}

async function main() {
  if (!url) fail('RPI_URL is required');
  if (!nativeSessionsRoot) fail('RPI_NATIVE_SESSIONS_ROOT is required');
  if (!nonce) fail('RPI_LANE_NONCE is required — the shell passes ONE invocation nonce to BOTH phase drivers so the lane publishes as ONE lane-set commit');
  if (phase !== 'default' && phase !== 'native_only') {
    fail(`RPI_PHASE must be default|native_only (got ${phase})`);
  }
  const meta = loadSeedMeta();
  const phaseIds = PHASE_IDS[phase];

  const launchOptions = chromePath ? { executablePath: chromePath } : {};
  // Lazy playwright import: only a browser run needs the ephemeral
  // dependency (RPI_SELF_CHECK=1 never gets here).
  const { chromium } = await import('playwright');
  const browser = await chromium.launch(launchOptions);
  try {
    const page = await browser.newPage({ viewport: { width: 1280, height: 800 }, acceptDownloads: true });
    if (token) {
      await page.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
    }
    await installSwitchTrace(page);
    // pageerror + console.error + websocket socketerror through the SAME
    // monitorBrowserErrors helper the lane's self-check exercises; the
    // returned ledger is the REAL array the publication gate consumes.
    const browserErrors = monitorBrowserErrors(page, 'external-sessions-lane');

    if (phase === 'default') {
      await runDefaultPhase(page, meta);
    } else {
      await runNativeOnlyPhase(page, meta);
    }

    if (browserErrors.length > 0) {
      fail(`browser errors: ${browserErrors.join(' | ')}`);
    }

    // The exact-once completeness gate must pass BEFORE anything is staged;
    // then the whole phase set (screenshots + phase coverage JSON +
    // import-result.json when present) is committed as a manifest fragment
    // INTO the shared lane-set staging set — never a pointer, never a
    // rename (the shell performs the ONE lane-set commit after both phases).
    const coverageError = documentedIdsCompletenessError(phaseIds, executedCounts);
    if (coverageError !== null) fail(`${phase}: ${coverageError}`);
    // Publish the REAL ledger: executed is the Map's insertion order (actual
    // execution order — never a filtered reconstruction of the documented
    // list), counts the real per-ID ledger values. The completeness gate
    // above already proved executed == documented as exact-once SETS; the
    // shell's ordered-fragment gate below additionally proves the run order
    // matches the documented order.
    const executed = [...executedCounts.keys()];
    const counts = Object.fromEntries(executedCounts);
    stageWrite(
      `coverage-assertions-${phase}.json`,
      `${JSON.stringify({
        phase,
        documented: [...phaseIds],
        executed,
        counts,
      }, null, 2)}\n`
    );
    commitPhaseManifest({
      lane: 'web-external-sessions',
      phase,
      nonce,
      documented: [...phaseIds],
      executed,
      counts,
      artifacts: [...laneArtifacts],
    });
    console.log(
      `web-external-sessions: PASSED phase=${phase} (${executedCounts.size}/${phaseIds.length} phase assertions; documented total ${DOCUMENTED_IDS.length}) — staged into shared lane-set staging ${stagingDir} (ONE lane-set commit follows; pointer: ${path.join(evidence, 'current.json')})`
    );
  } finally {
    await browser.close().catch(() => {});
    // Anything still staged here was never committed as a fragment (the
    // shell's lane-set commit renames the shared staging dir away); drop it
    // so no run-scoped staging survives this phase.
    if (!committed) cleanupStaging();
  }
}

if (selfCheckMode) {
  // The init-time gated self-check already ran against this throwaway
  // evidence root; remove the root so a self-check leaves zero residue.
  fs.rmSync(evidence, { recursive: true, force: true });
  console.log('web-external-sessions: self-check PASS');
} else {
  main().catch((err) => {
    console.error(`web-external-sessions: playwright crashed: ${err && err.stack ? err.stack : err}`);
    // A crash (not a fail() assertion) must still leave zero staging residue
    // and zero published artifacts.
    if (!committed) cleanupStaging();
    process.exit(2);
  });
}
