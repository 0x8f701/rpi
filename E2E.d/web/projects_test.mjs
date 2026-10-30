// Web all-project session catalog + cross-project New-session storage E2E
// lane — PLAYWRIGHT-ONLY (E2E.d/web/projects.sh).
//
// Environment:
//   RPI_URL           http://127.0.0.1:<port>/web
//   RPI_TOKEN         token file content (served via rpi-auth.<token> subprotocol)
//   RPI_CHROME        executable path of the system Chrome (optional)
//   RPI_EVIDENCE      evidence dir for screenshots + executed-assertion evidence
//   RPI_PROJECT_A     absolute cwd of project A (the listener's cwd)
//   RPI_PROJECT_B     absolute cwd of project B (sibling temp project)
//   RPI_SESSION_DIR_A encoded default session dir for project A (seeded)
//   RPI_SESSION_DIR_B encoded default session dir for project B (seeded)
//
// REQUIRES the typed DefaultTree session storage (`session_list` scope
// `all_projects` over the active profile's native sessions tree), the
// MultiSessionRuntimeManager lifecycle (switch_session restores the recorded
// cwd; New Session requires an explicitly selected validated cwd and records
// under that project's encoded default dir), and the Web Session
// sidebar/panel wiring.
// The lane FAILS (exit 2) whenever any assertion fails; there is no
// agent-browser fallback and no skip. No close/unload path is exercised.
//
// Mock scenario: `sessions` (E2E.d/lib/user_mock_server.py) — replies are
// routed by exact prompt text; anything unrecognized is an instant echo
// `sessions-reply: <prompt>`.
//
// Assertion matrix (feature -> IDs, used by the coverage report):
//   P0  P0.1 boot: WS connected and the newest catalog row (project A seed)
//           restored as the active session
//       P0.2 sidebar shows the rpi provider group with BOTH project subgroups
//           (workspace + project-b) under it; no tmp/UUID/source top-level
//       P0.3 both seeded rows (project-a-seed, project-b-seed) listed
//   P1  P1.1 Session panel backend cwd/project fields = project A
//       P1.2 panel session-file lives under A's encoded dir + the New-session
//           hint names project A
//   P2  P2.1 switching to the project-B row activates project B
//       P2.2 panel remounts: project/cwd fields = project B (backend cwd B)
//       P2.3 panel session-file under B's encoded dir + hint names project B
//   P3  P3.1 New Session explicitly selects project A while source stays B
//       P3.2 target backend project/cwd and session file all derive from A
//       P3.3 prompt round-trip flushes the target session (sidebar row appears)
//       P3.4 project-A sidebar group grows by one with the flushed target
//   P4  P4.1 on-disk: A's encoded dir holds exactly one NEW session file and
//           its recorded header cwd == project A
//       P4.2 on-disk/source isolation: B's encoded dir remains only its seed,
//           and the original B runtime still reports B cwd + unchanged file
//
// Every passing contract records its machine-readable ID (P0.1..P4.2) in the
// exact-once ledger; ONLY after the final completeness gate proves every
// documented id executed exactly once may the lane write evidence and
// report PASS. It then writes $RPI_EVIDENCE/coverage-assertions.json with
// the exact shape
//   { "documented": [...DOCUMENTED_IDS],
//     "executed": [...recorded ids in execution order],
//     "counts": { "<id>": 1, ... } }
// The Web coverage matrix
// (E2E.d/web/coverage_matrix.mjs, feature "all-project session catalog")
// requires ALL of DOCUMENTED_IDS below: the matrix fails when any named
// contract is absent, and this lane fails — before writing evidence — unless
// every documented ID actually executed exactly once.
//
// Unrecorded hard-fail contracts (no coverage-matrix ID, same exit-2 gate):
//   * the New-session directory input starts EMPTY on every mounted panel
//     (never prefilled from the current session cwd): New is disabled and
//     Enter emits ZERO new_session frames until a directory is typed or
//     accepted from the suggestions — with zero toasts and zero panel status
//   * an explicit directory (fill) sends exactly one new_session carrying the
//     trimmed selected sibling project cwd, and the input resets empty after
//     the target panel mounts (P3 wire proof)
//   * an invalid directory yields exactly ONE rejection toast, ZERO panel
//     status above Current session, and a fully unchanged source session
//     with busy released
//   * fork lifecycle (A -> Manage New explicit cwd B -> immediate Fork): the
//     immediate race seeds the New-B target forkable with one deterministic
//     prompt round-trip, then issues the fork at the target's first
//     observable active+forkable moment through the sidebar production
//     "Fork latest" control — the `session_fork_latest` frame carries NO
//     render-captured stale source owner (its top-level sessionId is the NEW
//     TARGET: App routes the ownerless command through the synchronous
//     commandTarget) and its sessionPath is the target's exact session file;
//     the settled panel-listbox flow keeps its `fork` frame under the same
//     owner rule. In both flows the fork child's authoritative state reports
//     cwd == project B with its file under B's encoded dir, the child row is
//     ACTIVE from the lifecycle cut (validated `session` overlay), and busy
//     releases — desktop + mobile
import { chromium } from 'playwright';
import fs from 'node:fs';
import path from 'node:path';
const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';
const projectA = process.env.RPI_PROJECT_A || '';
const projectB = process.env.RPI_PROJECT_B || '';
const sessionDirA = process.env.RPI_SESSION_DIR_A || '';
const sessionDirB = process.env.RPI_SESSION_DIR_B || '';

const PROJECT_A_LABEL = projectA.split(/[\\/]+/).filter(Boolean).pop() || 'workspace';
const PROJECT_B_LABEL = projectB.split(/[\\/]+/).filter(Boolean).pop() || 'project-b';

// Machine-readable executed-assertion evidence for the coverage matrix.
const DOCUMENTED_IDS = [
  'P0.1', 'P0.2', 'P0.3',
  'P1.1', 'P1.2',
  'P2.1', 'P2.2', 'P2.3',
  'P3.1', 'P3.2', 'P3.3', 'P3.4',
  'P4.1', 'P4.2',
];
// Exact-once ledger for the coverage matrix: every documented contract id
// maps to the assertion label that recorded it. record() is the ONLY write
// path and is called explicitly at each assertion site — waitFor never
// records implicitly (a wait is a mechanism, not an executed contract).
// Duplicates and ids outside DOCUMENTED_IDS fail the lane immediately; the
// final gate fails when any documented id never executed.
const executed = new Map();
function record(id, label = '') {
  if (!DOCUMENTED_IDS.includes(id)) {
    fail(`unexpected assertion id recorded: ${id} (not in DOCUMENTED_IDS)`);
  }
  if (executed.has(id)) {
    fail(`duplicate assertion id recorded: ${id} (first recorded by ${executed.get(id)})`);
  }
  executed.set(id, label);
  console.log(`[web-projects:assert] ${id}`);
}

function fail(message) {
  // Exit 2 (not 1): run.sh treats 1 as "npm install unavailable -> fall
  // back", which is FORBIDDEN for this lane. Any non-zero exit fails it.
  console.error(`web-projects: FAIL: ${message}`);
  process.exit(2);
}

async function waitFor(page, fn, id, timeoutMs = 30000, arg) {
  try {
    await page.waitForFunction(fn, arg, { timeout: timeoutMs });
  } catch {
    fail(`${id} (timeout ${timeoutMs}ms)`);
  }
}

/** Full path of the currently active session from the Session panel
 *  (formatSessionPath keeps the untruncated value in the `title`). */
async function panelSessionFileTitle(page) {
  return page.evaluate(() => {
    const el = document.querySelector('[data-testid="session-file-value"]');
    return el ? el.getAttribute('title') || el.textContent || '' : '';
  });
}

async function panelCwdTitle(page) {
  return page.evaluate(() => {
    const el = document.querySelector('[data-testid="session-cwd-value"]');
    return el ? el.getAttribute('title') || el.textContent || '' : '';
  });
}

async function panelProjectText(page) {
  return page.evaluate(() => {
    const el = document.querySelector('[data-testid="session-project-value"]');
    return el ? (el.textContent || '').trim() : '';
  });
}

async function panelHintText(page) {
  return page.evaluate(() => {
    const el = document.querySelector('[data-testid="session-new-location-hint"]');
    return el ? (el.textContent || '').trim() : '';
  });
}

/** Wait for a sidebar row with data-session-id `sid` to become active. */
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

/** Group-label texts of the sidebar project tree. */
async function groupLabels(page) {
  return page.evaluate(() =>
    [...document.querySelectorAll('.session-sidebar__group-label')].map((el) => (el.textContent || '').trim())
  );
}

/** Provider-level group-label texts (top-level rpi/Codex/Grok/OMP only). */
async function providerGroupLabels(page) {
  return page.evaluate(() =>
    [...document.querySelectorAll('[data-group-kind="provider"] .session-sidebar__group-label')].map((el) => (el.textContent || '').trim())
  );
}

/** Session ids of every sidebar row. */
async function sidebarSessionIds(page) {
  return page.evaluate(() =>
    [...document.querySelectorAll('.session-sidebar__switch')].map((r) => r.dataset.sessionId || '')
  );
}

/** Directory listing of a session dir restricted to .jsonl files. */
function sessionFiles(dir) {
  let names;
  try {
    names = fs.readdirSync(dir);
  } catch {
    return [];
  }
  return names.filter((name) => name.endsWith('.jsonl')).sort();
}

async function main() {
  if (!url) fail('RPI_URL is required');
  if (!projectA || !projectB || !sessionDirA || !sessionDirB) {
    fail('RPI_PROJECT_A/RPI_PROJECT_B/RPI_SESSION_DIR_A/RPI_SESSION_DIR_B are required');
  }
  // Startup removes any stale coverage-assertions.json from a previous run:
  // the matrix must never read leftover evidence from an earlier lane run.
  const coveragePath = path.join(evidence, 'coverage-assertions.json');
  fs.rmSync(coveragePath, { force: true });
  fs.mkdirSync(evidence, { recursive: true });
  const launchOptions = chromePath ? { executablePath: chromePath } : {};
  const browser = await chromium.launch(launchOptions);
  try {
    const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
    if (token) {
      await page.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
    }
    // Observability-only WebSocket subclass: records outgoing new_session and
    // fork-lifecycle frames (never mutates them) so the lane can prove the
    // exact wire contracts — an empty New input submits nothing, an explicit
    // selection sends exactly one frame with the trimmed selected directory,
    // and the immediate Fork carries the exact lifecycle owner. Same capture
    // pattern as the sessions lane's SessionAttachmentTestWebSocket.
    await page.addInitScript(() => {
      window.__rpiNewSessionFrames = [];
      window.__rpiForkFrames = [];
      window.__rpiForkPages = [];
      const NativeWebSocket = window.WebSocket;
      class NewSessionProbeWebSocket extends NativeWebSocket {
        constructor(...args) {
          super(...args);
          const requests = new Map();
          this.forkPageRequests = requests;
          this.addEventListener('message', (event) => {
            const frame = JSON.parse(event.data);
            const sessionId = requests.get(frame.id);
            if (sessionId) window.__rpiForkPages.push({ sessionId, frame });
          });
        }
        send(data) {
          if (typeof data === 'string') {
            try {
              const frame = JSON.parse(data);
              if (frame?.type === 'get_fork_messages') this.forkPageRequests.set(frame.id, frame.sessionId);
              if (frame?.type === 'new_session') {
                window.__rpiNewSessionFrames.push(frame);
              }
              if (frame?.type === 'fork' || frame?.type === 'session_fork_latest') {
                window.__rpiForkFrames.push(frame);
              }
            } catch {
              // Non-JSON frames pass through unchanged.
            }
          }
          return super.send(data);
        }
      }
      for (const key of ['CONNECTING', 'OPEN', 'CLOSING', 'CLOSED']) {
        Object.defineProperty(NewSessionProbeWebSocket, key, { value: NativeWebSocket[key] });
      }
      window.WebSocket = NewSessionProbeWebSocket;
    });
    page.on('pageerror', (err) => {
      console.error(`web-projects: page error: ${err.message}`);
    });

    await page.addInitScript((sid) => {
      window.localStorage.setItem(
        `rpi-web-session:${encodeURIComponent(window.location.host.trim())}`,
        sid,
      );
    }, 'project-a-seed');

    /* ---------------- P0: boot + all-project catalog ---------------- */
    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'P0.1: page title missing');
    await waitFor(
      page,
      () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true,
      'P0.1: WS did not reach "connected"'
    );
    // Boot restore picks the NEWEST catalog row (project A seed — the lane
    // fixture pins A's mtime one second newer than B's), so the active
    // session is the seeded project-A session, not the primary runtime.
    await waitRowActive(page, 'project-a-seed', 'P0.1');
    await page.screenshot({ path: `${evidence}/projects-p0-boot.png`, fullPage: true });
    record('P0.1');

    // P0.2: rpi provider group with both project subgroups; no tmp/UUID/source
    // top-level groups.
    await waitFor(
      page,
      (expected) => {
        const labels = [...document.querySelectorAll('.session-sidebar__group-label')]
          .map((el) => (el.textContent || '').trim());
        return expected.every((l) => labels.includes(l));
      },
      'P0.2: sidebar never showed both project groups',
      30000,
      [PROJECT_A_LABEL, PROJECT_B_LABEL]
    );
    const labels = await groupLabels(page);
    if (!labels.includes(PROJECT_A_LABEL) || !labels.includes(PROJECT_B_LABEL)) {
      fail(`P0.2: project groups missing (got ${labels.join(', ')})`);
    }
    const providers = await providerGroupLabels(page);
    if (!providers.includes('rpi')) {
      fail(`P0.2: rpi provider group missing (got ${providers.join(', ')})`);
    }
    const forbiddenProviders = providers.filter(
      (g) => !['rpi', 'Codex', 'Grok', 'OMP'].includes(g)
    );
    if (forbiddenProviders.length > 0) {
      fail(`P0.2: non-provider top-level groups found: ${forbiddenProviders.join(', ')}`);
    }
    record('P0.2');

    // P0.3: both seeded rows listed (ids are fixture-fixed).
    await waitFor(
      page,
      () => {
        const ids = [...document.querySelectorAll('.session-sidebar__switch')]
          .map((r) => r.dataset.sessionId || '');
        return ids.includes('project-a-seed') && ids.includes('project-b-seed');
      },
      'P0.3: seeded project rows never appeared in the sidebar',
      30000
    );
    const bootIds = await sidebarSessionIds(page);
    if (!bootIds.includes('project-a-seed') || !bootIds.includes('project-b-seed')) {
      fail(`P0.3: seeded rows missing (got ${bootIds.join(', ')})`);
    }
    record('P0.3');

    /* ---- Search exercise: project filtering + active visibility + clear ---- */
    const searchActiveSid = await page.evaluate(() => {
      const row = document.querySelector('.session-sidebar__row--active .session-sidebar__switch');
      return row?.dataset.sessionId || '';
    });
    const searchAllIds = await sidebarSessionIds(page);
    if (!searchActiveSid || !searchAllIds.includes(searchActiveSid)) {
      fail(`search: active session ${searchActiveSid} not found before search (got ${searchAllIds.join(', ')})`);
    }
    // Filter by project-B name — row count must drop and active stays visible.
    await page.fill('#session-sidebar-search', PROJECT_B_LABEL);
    await waitFor(
      page,
      (expected) => {
        const ids = [...document.querySelectorAll('.session-sidebar__switch')].map((r) => r.dataset.sessionId || '');
        return ids.length < expected;
      },
      'search: project-B filter did not reduce row count',
      10000,
      searchAllIds.length
    );
    {
      const filtered = await page.evaluate(() =>
        [...document.querySelectorAll('.session-sidebar__switch')].map((r) => r.dataset.sessionId || '')
      );
      if (!filtered.includes(searchActiveSid)) {
        fail(`search: active session ${searchActiveSid} disappeared during project-B filter (got ${filtered.join(', ')})`);
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
      searchAllIds.length
    );
    /* ---------------- P1: active session is project A ---------------- */
    await page.click('#session-toggle-btn');
    await waitFor(page, () => document.getElementById('session-panel') !== null, 'P1.1: session panel did not open');
    await waitFor(
      page,
      (l) => (document.querySelector('[data-testid="session-project-value"]')?.textContent || '').trim() === l,
      'P1.1: panel project field never showed project A',
      30000,
      PROJECT_A_LABEL
    );
    const cwdA = await panelCwdTitle(page);
    if (cwdA !== projectA) {
      fail(`P1.1: panel backend cwd is not project A (got ${cwdA})`);
    }
    record('P1.1');
    const fileA = await panelSessionFileTitle(page);
    if (path.dirname(fileA) !== sessionDirA) {
      fail(`P1.2: active session file is not under A's encoded dir (got ${fileA})`);
    }
    const hintA = await panelHintText(page);
    if (!hintA.includes(PROJECT_A_LABEL)) {
      fail(`P1.2: New-session hint does not name project A (got ${hintA})`);
    }
    record('P1.2');
    await page.screenshot({ path: `${evidence}/projects-p1-panel-a.png`, fullPage: true });

    /* ---- empty-input New (unrecorded contract): explicitly inert ----
     * The Manage panel never prefills the directory from the current
     * session: the input mounts EMPTY, New stays disabled, and Enter emits
     * ZERO new_session frames while the current session stays active. One
     * error-surface violation fails the lane. */
    {
      const framesBefore = await page.evaluate(() => window.__rpiNewSessionFrames.length);
      const prefill = await page.inputValue('#session-new-cwd-input');
      if (prefill !== '') {
        fail(`empty New: directory input is not empty on mount (got ${JSON.stringify(prefill)})`);
      }
      const placeholder = await page.getAttribute('#session-new-cwd-input', 'placeholder');
      if (!placeholder || !placeholder.toLowerCase().includes('select')) {
        fail(`empty New: placeholder does not demand an explicit selection (got ${placeholder})`);
      }
      const newDisabled = await page.evaluate(() =>
        document.getElementById('session-new-btn')?.hasAttribute('disabled') ?? true
      );
      if (!newDisabled) {
        fail('empty New: New session button must be disabled while the directory input is empty');
      }
      const activeBefore = await page.evaluate(() =>
        document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || ''
      );
      await page.press('#session-new-cwd-input', 'Enter');
      await page.waitForTimeout(1000);
      const frames = await page.evaluate((before) => window.__rpiNewSessionFrames.slice(before), framesBefore);
      if (frames.length !== 0) {
        fail(`empty New: empty input must emit zero new_session frames (got ${JSON.stringify(frames)})`);
      }
      const activeAfter = await page.evaluate(() =>
        document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || ''
      );
      if (activeAfter !== activeBefore) {
        fail(`empty New: active session moved (got ${activeAfter}, expected ${activeBefore})`);
      }
      const surface = await page.evaluate(() => ({
        errorToasts: document.querySelectorAll('#toasts .toast--error').length,
        panelStatusGone: document.querySelector('#session-panel .panel__status') === null,
      }));
      if (surface.errorToasts !== 0 || !surface.panelStatusGone) {
        fail(`empty New: unexpected error surface (toasts=${surface.errorToasts}, panelStatus=${surface.panelStatusGone ? 'absent' : 'present'})`);
      }
      await page.screenshot({ path: `${evidence}/projects-p1-empty-new-disabled.png`, fullPage: true });
    }
    /* ---------------- P2: switch to project B ---------------- */
    await page.evaluate(() => {
      const row = [...document.querySelectorAll('.session-sidebar__switch')]
        .find((r) => r.dataset.sessionId === 'project-b-seed');
      row.click();
    });
    await waitRowActive(page, 'project-b-seed', 'P2.1');
    record('P2.1');

    // The panel remounts on session change (key = session id) and reloads
    // get_state: project/cwd flip to project B.
    await waitFor(
      page,
      (l) => (document.querySelector('[data-testid="session-project-value"]')?.textContent || '').trim() === l,
      'P2.2: panel project field never showed project B',
      30000,
      PROJECT_B_LABEL
    );
    const cwdB = await panelCwdTitle(page);
    if (cwdB !== projectB) {
      fail(`P2.2: panel backend cwd is not project B (got ${cwdB})`);
    }
    record('P2.2');
    const fileB = await panelSessionFileTitle(page);
    if (path.dirname(fileB) !== sessionDirB) {
      fail(`P2.3: active session file is not under B's encoded dir (got ${fileB})`);
    }
    const hintB = await panelHintText(page);
    if (!hintB.includes(PROJECT_B_LABEL)) {
      fail(`P2.3: New-session hint does not name project B (got ${hintB})`);
    }
    record('P2.3');
    await page.screenshot({ path: `${evidence}/projects-p2-panel-b.png`, fullPage: true });

    /* -------- P3: explicitly create under A while source is B -------- */
    const sourceSessionId = await page.evaluate(() =>
      document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || ''
    );
    if (sourceSessionId !== 'project-b-seed') {
      fail(`P3.1: explicit target did not start from the seeded B source (got ${sourceSessionId})`);
    }
    const sourceFile = await panelSessionFileTitle(page);
    const projectACountBefore = await page.evaluate((label) => {
      const group = [...document.querySelectorAll('.session-sidebar__group')].find((candidate) =>
        (candidate.querySelector('.session-sidebar__group-label')?.textContent || '').trim() === label
      );
      return Number.parseInt((group?.querySelector('.session-sidebar__group-count')?.textContent || '0').trim(), 10);
    }, PROJECT_A_LABEL);
    const projectAFilesBefore = sessionFiles(sessionDirA).length;
    const explicitFramesBefore = await page.evaluate(() => window.__rpiNewSessionFrames.length);
    await page.fill('#session-new-cwd-input', projectA);
    await page.click('#session-new-btn');
    await waitFor(
      page,
      (sourceId) => {
        const active = document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '';
        const el = document.querySelector('[data-testid="session-file-value"]');
        const file = el ? el.getAttribute('title') || el.textContent || '' : '';
        return active !== '' && active !== sourceId && file.endsWith('.jsonl');
      },
      'P3.1: selected target session never became active',
      30000,
      sourceSessionId
    );
    const targetSessionId = await page.evaluate(() =>
      document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || ''
    );
    if (!targetSessionId || targetSessionId === sourceSessionId) {
      fail(`P3.1: target identity did not diverge from source (source=${sourceSessionId}, target=${targetSessionId})`);
    }
    // Wire contract for the explicit selection: page.fill types the selected
    // directory into the always-empty picker, so the frame must carry the
    // trimmed explicit sibling project cwd — exactly one new_session frame.
    // The panel remounts for the target and the directory input must reset
    // to EMPTY (never prefilled from the activated target's cwd).
    const explicitFrames = await page.evaluate(
      (before) => window.__rpiNewSessionFrames.slice(before),
      explicitFramesBefore
    );
    if (explicitFrames.length !== 1 || explicitFrames[0]?.cwd !== projectA) {
      fail(`P3.1: explicit edit must send exactly one new_session with cwd=${projectA} (got ${JSON.stringify(explicitFrames)})`);
    }
    const inputAfter = await page.inputValue('#session-new-cwd-input');
    if (inputAfter !== '') {
      fail(`P3.1: New-session directory input did not reset empty after the target mounted (got ${JSON.stringify(inputAfter)})`);
    }
    const targetNewDisabled = await page.evaluate(() =>
      document.getElementById('session-new-btn')?.hasAttribute('disabled') ?? true
    );
    if (!targetNewDisabled) {
      fail('P3.1: New session button must start disabled again on the freshly mounted target panel');
    }
    const panelProject = await panelProjectText(page);
    if (panelProject !== PROJECT_A_LABEL) {
      fail(`P3.1: selected target project field is not project A (got ${panelProject})`);
    }
    const cwdNew = await panelCwdTitle(page);
    if (cwdNew !== projectA) {
      fail(`P3.1: selected target backend cwd is not project A (got ${cwdNew})`);
    }
    record('P3.1');
    const fileNew = await panelSessionFileTitle(page);
    if (!fileNew.includes(sessionDirA)) {
      fail(`P3.2: selected target session is not under A's encoded dir (got ${fileNew})`);
    }
    if (fileNew.includes(sessionDirB)) {
      fail(`P3.2: selected target session leaked under B's encoded dir (got ${fileNew})`);
    }
    if (path.dirname(fileNew) !== sessionDirA) {
      fail(`P3.2: selected target session parent is not exactly A's encoded dir (got ${fileNew})`);
    }
    if (fileNew === sourceFile) {
      fail('P3.2: New session did not create a distinct target session file');
    }
    record('P3.2');

    // P3.3: one prompt round-trip flushes the selected A session to disk and
    // gives the sidebar a catalog row titled by its summary.
    await page.fill('#prompt-input', 'projects-new-flush');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      (r) => document.body.textContent.includes(r),
      'P3.3: new session prompt never round-tripped',
      30000,
      'sessions-reply: projects-new-flush'
    );
    record('P3.3');

    // P3.4: project-A subgroup grows by exactly one selected target; project
    // B remains represented by the unchanged source seed.
    await waitFor(
      page,
      ({ label, expected }) => {
        const group = [...document.querySelectorAll('.session-sidebar__group')].find((candidate) =>
          (candidate.querySelector('.session-sidebar__group-label')?.textContent || '').trim() === label
        );
        return Number.parseInt((group?.querySelector('.session-sidebar__group-count')?.textContent || '0').trim(), 10) === expected;
      },
      'P3.4: project-A group did not grow by one',
      30000,
      { label: PROJECT_A_LABEL, expected: projectACountBefore + 1 }
    );
    // The flush's catalog row can lag the group count, so poll until the
    // selected project-A row acquires the persisted summary; on timeout dump
    // the exact sidebar row state for evidence before failing.
    {
      const deadline = Date.now() + 30000;
      let acquired = false;
      while (Date.now() < deadline) {
        // The reconciled sidebar retains existing row positions and appends
        // genuinely-new rows at the tail, so a project's rows are NOT all
        // adjacent between group headers; search every rendered row.
        acquired = await page.evaluate((want) => {
          const row = [...document.querySelectorAll('.session-sidebar__switch')]
            .find((r) => r.dataset.sessionId === want);
          return row !== undefined
            && (row.querySelector('.session-sidebar__name')?.textContent || '').includes('projects-new-flush');
        }, targetSessionId);
        if (acquired) break;
        await page.waitForTimeout(250);
      }
      if (!acquired) {
        const dump = await page.evaluate((target) => ({
          list: [...(document.querySelector('.session-sidebar__list')?.children ?? [])].map((li) => ({
            cls: li.className,
            label: (li.querySelector('.session-sidebar__group-label')?.textContent || '').trim(),
            count: (li.querySelector('.session-sidebar__group-count')?.textContent || '').trim(),
            sid: li.querySelector('.session-sidebar__switch')?.dataset.sessionId || '',
            name: (li.querySelector('.session-sidebar__name')?.textContent || '').slice(0, 80),
          })),
          targetSessionId: target,
        }), targetSessionId);
        console.error(`web-projects: P3.4 timeout dump: ${JSON.stringify(dump)}`);
        await page.screenshot({ path: `${evidence}/projects-p3.4-timeout.png`, fullPage: true });
        fail('P3.4: selected project-A row never acquired the flush summary');
      }
    }
    const { newRowId, newRowTitle } = await page.evaluate((want) => {
      const row = [...document.querySelectorAll('.session-sidebar__switch')]
        .find((r) => r.dataset.sessionId === want);
      return {
        newRowId: row ? row.dataset.sessionId || '' : '',
        newRowTitle: row ? (row.querySelector('.session-sidebar__name')?.textContent || '').trim() : '',
      };
    }, targetSessionId);
    if (newRowId !== targetSessionId) {
      fail(`P3.4: selected target row identity mismatch (expected ${targetSessionId}, got ${newRowId})`);
    }
    // The new row is the flushed selected session, grouped under project A.
    if (!newRowTitle.includes('projects-new-flush')) {
      fail(`P3.4: selected session row title is not the flush summary (got ${newRowTitle})`);
    }
    record('P3.4');
    await page.screenshot({ path: `${evidence}/projects-p3-new-a-from-b.png`, fullPage: true });

    /* ---------------- P4: disk + source isolation contract ---------------- */
    // Poll until the exact selected target file exists under A's encoded dir,
    // then assert A grew by one and the durable header records A.
    const deadline = Date.now() + 30000;
    let aFiles = sessionFiles(sessionDirA);
    while (!aFiles.some((name) => path.join(sessionDirA, name) === fileNew) && Date.now() < deadline) {
      await new Promise((resolve) => setTimeout(resolve, 250));
      aFiles = sessionFiles(sessionDirA);
    }
    if (
      aFiles.length !== projectAFilesBefore + 1 ||
      !aFiles.some((name) => name.includes('project-a-seed'))
    ) {
      fail(`P4.1: A's encoded dir did not grow by exactly one selected target (${aFiles.join(', ')})`);
    }
    const newFileName = aFiles.find((name) => path.join(sessionDirA, name) === fileNew);
    if (!newFileName) {
      fail(`P4.1: selected target file did not land under A (${fileNew})`);
    }
    const firstLine = fs.readFileSync(path.join(sessionDirA, newFileName), 'utf8').split('\n', 1)[0];
    const header = JSON.parse(firstLine);
    if (header.type !== 'session' || header.version !== 3 || header.cwd !== projectA) {
      fail(`P4.1: selected target header does not record project A (${firstLine})`);
    }
    record('P4.1');

    const bFiles = sessionFiles(sessionDirB);
    if (bFiles.length !== 1 || !bFiles[0].includes('project-b-seed')) {
      fail(`P4.2: source B's encoded dir changed (${bFiles.join(', ')})`);
    }
    await page.evaluate((sessionId) => {
      const row = [...document.querySelectorAll('.session-sidebar__switch')]
        .find((candidate) => candidate.dataset.sessionId === sessionId);
      if (row) row.click();
    }, sourceSessionId);
    await waitRowActive(page, sourceSessionId, 'P4.2');
    await waitFor(
      page,
      ({ cwd, sessionFile }) => {
        const cwdEl = document.querySelector('[data-testid="session-cwd-value"]');
        const fileEl = document.querySelector('[data-testid="session-file-value"]');
        const currentCwd = cwdEl ? cwdEl.getAttribute('title') || cwdEl.textContent || '' : '';
        const currentFile = fileEl ? fileEl.getAttribute('title') || fileEl.textContent || '' : '';
        return currentCwd === cwd && currentFile === sessionFile;
      },
      'P4.2: source B panel did not restore its original backend state',
      30000,
      { cwd: projectB, sessionFile: sourceFile }
    );
    const sourceCwdAfter = await panelCwdTitle(page);
    const sourceFileAfter = await panelSessionFileTitle(page);
    if (sourceCwdAfter !== projectB || sourceFileAfter !== sourceFile) {
      fail(`P4.2: source runtime mutated (cwd=${sourceCwdAfter}, file=${sourceFileAfter})`);
    }
    record('P4.2');
    await page.screenshot({ path: `${evidence}/projects-p4-source-b-unchanged.png`, fullPage: true });

    /* ---- invalid explicit cwd (unrecorded contract): one toast, no panel error ----
     * An edited input with a missing directory is an operator-selected
     * workspace root the backend must reject: exactly ONE bottom-right
     * toast (App-owned new_session failure, never doubled by the panel),
     * ZERO in-panel status above Current session, source session fully
     * unchanged, and the busy state released. */
    {
      const framesBefore = await page.evaluate(() => window.__rpiNewSessionFrames.length);
      const toastsBefore = await page.evaluate(() => document.querySelectorAll('#toasts .toast').length);
      const invalidCwd = '/nonexistent-e2e-invalid-cwd-probe';
      await page.fill('#session-new-cwd-input', invalidCwd);
      await page.click('#session-new-btn');
      await waitFor(
        page,
        ({ before }) => document.querySelectorAll('#toasts .toast').length === before + 1,
        'invalid cwd: rejection never surfaced exactly one toast',
        30000,
        { before: toastsBefore }
      );
      const surface = await page.evaluate(({ before }) => ({
        toasts: [...document.querySelectorAll('#toasts .toast')].map((t) => t.textContent || ''),
        panelStatusGone: document.querySelector('#session-panel .panel__status') === null,
        active: document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '',
        newBtnDisabled: document.getElementById('session-new-btn')?.hasAttribute('disabled') ?? true,
      }), { before: toastsBefore });
      const newToasts = surface.toasts.slice(toastsBefore);
      if (newToasts.length !== 1 || !newToasts[0].includes('new_session')) {
        fail(`invalid cwd: expected exactly one rejection toast naming new_session (got ${JSON.stringify(newToasts)})`);
      }
      if (!surface.panelStatusGone) {
        fail('invalid cwd: panel rendered an in-panel status surface');
      }
      if (surface.active !== sourceSessionId) {
        fail(`invalid cwd: active session moved (got ${surface.active}, expected ${sourceSessionId})`);
      }
      if (surface.newBtnDisabled) {
        fail('invalid cwd: New session button stayed busy after the rejection');
      }
      const frames = await page.evaluate((before) => window.__rpiNewSessionFrames.slice(before), framesBefore);
      if (frames.length !== 1 || frames[0]?.cwd !== invalidCwd) {
        fail(`invalid cwd: expected exactly one new_session carrying the edited cwd (got ${JSON.stringify(frames)})`);
      }
      const cwdAfter = await panelCwdTitle(page);
      const fileAfter = await panelSessionFileTitle(page);
      if (cwdAfter !== projectB || fileAfter !== sourceFile) {
        fail(`invalid cwd: current session surface changed (cwd=${cwdAfter}, file=${fileAfter})`);
      }
      await page.screenshot({ path: `${evidence}/projects-p5-invalid-cwd-one-toast.png`, fullPage: true });
    }

    /* ---------------- P6: A -> New B -> immediate Fork (fork lifecycle) ----------------
     * Two STRICTLY separated flows (the immediate one is never the settled
     * one, and the settled one is never called immediate):
     *
     * P6-immediate — the REAL production New-B→Fork race (desktop + mobile):
     *   1. New Session (explicit cwd project B) cuts the source and activates
     *      the fresh target B; the exact new_session frame is captured on the
     *      wire and the target row paints immediately from the validated
     *      `session` overlay (no reload/poll).
     *   2. B is seeded forkable deterministically BEFORE the fork cut: one
     *      prompt round-trip records one durable user entry — the exact
     *      prerequisite the backend contract test seeds
     *      (session_runtime_manager.rs
     *      production_new_explicit_cwd_envelope_routes_immediate_fork_to_target:
     *      a real prompt on the target runtime, then the immediate fork).
     *   3. At the FIRST observable moment B is both active and forkable, the
     *      fork is issued through the sidebar's production "Fork latest"
     *      control — NO prompt, NO panel refresh, NO settling delay in
     *      between (the panel's fork list is never touched; the listbox flow
     *      belongs to P6-settled). The session_fork_latest frame is captured
     *      on the wire.
     *   Asserted: exactly one fork-lifecycle frame whose top-level owner is
     *   the NEW TARGET B and whose sessionPath is B's exact session file
     *   (App routes the ownerless command through the synchronous
     *   commandTarget — never a render-captured stale pre-New source), the
     *   child's authoritative cwd == project B with its file under B's
     *   encoded dir (never the source's A, never B's own file), the durable
     *   child header's parentSession lineage points at B's exact session
     *   file, the child row is ACTIVE from the lifecycle cut via App's
     *   validated `session` row overlay with zero error toasts, busy
     *   releases, the child composer carries exactly the seeded fork point
     *   text (never B's unsent draft), and returning to B restores B's exact
     *   unsent draft.
     *
     * P6-settled (kept separately, never called immediate): the panel's
     *   bounded #session-fork-list flow — prompt, panel refresh, listbox
     *   selection through the ARIA listbox, panel Fork button — proving the
     *   current listbox + panel controls still fork the current runtime. */

    /* ---- P6-immediate: the REAL production New-B→Fork race (desktop) ---- */
    {
      const activeBefore = await page.evaluate(() =>
        document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || ''
      );
      if (activeBefore !== sourceSessionId) {
        fail(`P6 immediate: race did not start from the seeded project-B source (got ${activeBefore})`);
      }
      // (1) New Session (explicit cwd project B) cuts the source -> fresh
      // target B and activates it. The exact new_session frame is captured;
      // the target row must paint immediately from the validated `session`
      // overlay — the same painted state as the activation, no reload/poll.
      const newBframesBefore = await page.evaluate(() => window.__rpiNewSessionFrames.length);
      // The preceding invalid-cwd phase intentionally surfaced one error
      // toast that may still be auto-dismissing; tag the stale nodes and
      // assert NO UNTAGGED (newly emitted) error toast appears.
      const staleErrorToasts = await page.evaluate(() => {
        let tagged = 0;
        document.querySelectorAll('#toasts .toast--error').forEach((t) => {
          t.dataset.e2eStale = '1';
          tagged += 1;
        });
        return tagged;
      });
      await page.fill('#session-new-cwd-input', projectB);
      await page.click('#session-new-btn');
      await waitFor(
        page,
        (sourceId) => {
          const active = document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '';
          return active !== '' && active !== sourceId;
        },
        'P6 immediate: New-B target never became active',
        30000,
        sourceSessionId
      );
      const newBId = await page.evaluate(() =>
        document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || ''
      );
      if (!newBId || newBId === sourceSessionId) {
        fail(`P6 immediate: New-B target identity mismatch (got ${JSON.stringify(newBId)})`);
      }
      const newBFrames = await page.evaluate((before) => window.__rpiNewSessionFrames.slice(before), newBframesBefore);
      if (newBFrames.length !== 1 || newBFrames[0]?.cwd !== projectB || typeof newBFrames[0]?.id !== 'string') {
        fail(`P6 immediate: expected exactly one new_session frame with cwd=${projectB} (got ${JSON.stringify(newBFrames)})`);
      }
      const newBOverlay = await page.evaluate((target) => ({
        rows: [...document.querySelectorAll('.session-sidebar__switch')].map((r) => r.dataset.sessionId || ''),
        active: document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '',
        newErrorToasts: document.querySelectorAll('#toasts .toast--error:not([data-e2e-stale])').length,
      }), newBId);
      if (newBOverlay.active !== newBId
        || newBOverlay.rows.filter((id) => id === newBId).length !== 1
        || newBOverlay.newErrorToasts !== 0) {
        fail(`P6 immediate: New-B target row was not painted immediately from the lifecycle overlay (no reload/poll): ${JSON.stringify({ ...newBOverlay, staleErrorToasts })}`);
      }
      // The new-B panel owns its mount load: the directory input resets EMPTY
      // on the freshly keyed target panel, and the fresh composer starts
      // EMPTY (a source draft never leaks into the fresh target).
      const freshInput = await page.inputValue('#session-new-cwd-input');
      if (freshInput !== '') {
        fail(`P6 immediate: New-B target panel did not reset the directory input empty (got ${JSON.stringify(freshInput)})`);
      }
      const newBComposer = await page.inputValue('#prompt-input');
      if (newBComposer !== '') {
        fail(`P6 immediate: New-B target composer leaked a draft (got ${JSON.stringify(newBComposer)})`);
      }
      // B's exact session file (the immediate fork frame's sessionPath and
      // the child's lineage anchor).
      await waitFor(
        page,
        (dir) => {
          const el = document.querySelector('[data-testid="session-file-value"]');
          const file = el ? el.getAttribute('title') || el.textContent || '' : '';
          return file.endsWith('.jsonl');
        },
        'P6 immediate: New-B target panel never reported its session file',
        30000,
        sessionDirB
      );
      const newBFile = await panelSessionFileTitle(page);
      if (path.dirname(newBFile) !== sessionDirB) {
        fail(`P6 immediate: New-B target file is not under B's encoded dir (got ${newBFile})`);
      }

      // (2) Deterministic forkable seed BEFORE the fork cut: one prompt
      // round-trip records one durable user entry on the New-B target.
      await page.fill('#prompt-input', 'projects-immediate-fork-seed');
      await page.press('#prompt-input', 'Enter');
      await waitFor(
        page,
        (r) => document.body.textContent.includes(r),
        'P6 immediate: forkable seed prompt never round-tripped',
        30000,
        'sessions-reply: projects-immediate-fork-seed'
      );
      // An UNSent B-side draft rides B's view slot into the fork cut: the
      // child composer must carry ONLY the fork's editorText, never this.
      await page.fill('#prompt-input', 'projects-immediate-newb-draft-must-not-leak');

      // (3) The race: at B's first observable active+forkable moment issue
      // the fork through the sidebar's production "Fork latest" control — no
      // prompt, no panel refresh, no settling delay, no listbox.
      const rowsBeforeImmediateFork = await page.evaluate(() =>
        [...document.querySelectorAll('.session-sidebar__switch')].map((r) => r.dataset.sessionId || '')
      );
      const forkFramesBefore = await page.evaluate(() => window.__rpiForkFrames.length);
      // Tag stale toasts (an auto-dismissing one from an earlier phase may
      // still be present); assert NO UNTAGGED (newly emitted) error toast.
      const staleErrorToastsFork = await page.evaluate(() => {
        let tagged = 0;
        document.querySelectorAll('#toasts .toast--error').forEach((t) => {
          t.dataset.e2eStale = '1';
          tagged += 1;
        });
        return tagged;
      });
      await page.locator(`.session-sidebar__switch[data-session-id="${newBId}"]`).click({ button: 'right' });
      await page.locator('.session-sidebar__menu-item', { hasText: 'Fork latest' }).first().click();
      await waitFor(
        page,
        ({ before, source }) => {
          const frames = window.__rpiForkFrames.slice(before);
          if (frames.length === 0) return false;
          const active = document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '';
          return active !== '' && active !== source && active !== frames[0]?.sessionId;
        },
        'P6 immediate: fork child never activated',
        30000,
        { before: forkFramesBefore, source: newBId }
      );
      const immediateForkFrames = await page.evaluate((before) => window.__rpiForkFrames.slice(before), forkFramesBefore);
      if (immediateForkFrames.length !== 1) {
        fail(`P6 immediate: expected exactly one fork-lifecycle frame (got ${JSON.stringify(immediateForkFrames)})`);
      }
      const immediateForkFrame = immediateForkFrames[0];
      if (immediateForkFrame?.type !== 'session_fork_latest') {
        fail(`P6 immediate: expected the sidebar production session_fork_latest frame (got ${JSON.stringify(immediateForkFrame)})`);
      }
      // The fork frame's top-level owner must be the NEW TARGET B — the
      // synchronous commandTarget — never the render-captured pre-New source.
      if (immediateForkFrame?.sessionId !== newBId) {
        fail(`P6 immediate: fork frame owner must be the New-B target ${newBId} (got ${JSON.stringify(immediateForkFrames)})`);
      }
      if (immediateForkFrame?.sessionPath !== newBFile) {
        fail(`P6 immediate: fork frame sessionPath must be B's exact session file ${newBFile} (got ${JSON.stringify(immediateForkFrames)})`);
      }
      // Immediate overlay: the child row is present exactly once and ACTIVE
      // in the same painted state as the cut — no reload, no catalog poll —
      // with zero error toasts.
      const forkChildId = await page.evaluate(() =>
        document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || ''
      );
      if (!forkChildId || forkChildId === newBId || forkChildId === sourceSessionId) {
        fail(`P6 immediate: fork child row is not the active sidebar row (got ${JSON.stringify(forkChildId)})`);
      }
      const forkOverlay = await page.evaluate(() => ({
        rows: [...document.querySelectorAll('.session-sidebar__switch')].map((r) => r.dataset.sessionId || ''),
        active: document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '',
        newErrorToasts: document.querySelectorAll('#toasts .toast--error:not([data-e2e-stale])').length,
        sidebarError: document.querySelector('.session-sidebar__error')?.textContent || '',
      }));
      if (rowsBeforeImmediateFork.includes(forkChildId)) {
        fail(`P6 immediate: fork child row pre-existed the lifecycle cut: ${JSON.stringify(rowsBeforeImmediateFork)}`);
      }
      if (forkOverlay.rows.filter((id) => id === forkChildId).length !== 1) {
        fail(`P6 immediate: fork child row not immediately present exactly once after the lifecycle cut (no reload/poll): ${JSON.stringify({ rowsBeforeImmediateFork, ...forkOverlay })}`);
      }
      if (forkOverlay.active !== forkChildId || forkOverlay.newErrorToasts !== 0 || forkOverlay.sidebarError !== '') {
        fail(`P6 immediate: fork child overlay state invalid (no reload/poll): ${JSON.stringify({ ...forkOverlay, staleErrorToastsFork })}`);
      }
      // The panel remounts for the child and its mount load reports the
      // child's authoritative state.
      await waitFor(
        page,
        (l) => (document.querySelector('[data-testid="session-project-value"]')?.textContent || '').trim() === l,
        'P6 immediate: fork child panel never showed project B',
        30000,
        PROJECT_B_LABEL
      );
      // The child's authoritative state: cwd == project B, file under B's
      // encoded dir, never the source's A and never B's own file.
      const childCwd = await panelCwdTitle(page);
      const childFile = await panelSessionFileTitle(page);
      if (childCwd !== projectB) {
        fail(`P6 immediate: fork child backend cwd is not project B (got ${childCwd})`);
      }
      if (path.dirname(childFile) !== sessionDirB) {
        fail(`P6 immediate: fork child session file is not under B's encoded dir (got ${childFile})`);
      }
      if (childFile === newBFile || childFile === sourceFile) {
        fail(`P6 immediate: fork child reused an existing session file (child=${childFile}, newB=${newBFile})`);
      }
      // Durable lineage on disk: the child header records cwd B and
      // parentSession == B's exact session file.
      let childHeader = null;
      const diskDeadline = Date.now() + 30000;
      while (Date.now() < diskDeadline) {
        try {
          childHeader = JSON.parse(fs.readFileSync(childFile, 'utf8').split('\n', 1)[0]);
          break;
        } catch {
          await new Promise((resolve) => setTimeout(resolve, 250));
        }
      }
      if (!childHeader || childHeader.type !== 'session' || childHeader.version !== 3
        || childHeader.cwd !== projectB || childHeader.parentSession !== newBFile) {
        fail(`P6 immediate: fork child durable lineage is not B (header=${childHeader ? JSON.stringify(childHeader) : 'missing'}, expected parent=${newBFile})`);
      }
      // The child composer carries EXACTLY the seeded fork point text (the
      // backend's latest nonempty user message) — B's unsent draft never
      // leaks into the child.
      const immediateChildComposer = await page.inputValue('#prompt-input');
      if (immediateChildComposer !== 'projects-immediate-fork-seed') {
        fail(`P6 immediate: fork child composer must hold exactly the seeded fork point (got ${JSON.stringify(immediateChildComposer)})`);
      }
      if (immediateChildComposer.includes('projects-immediate-newb-draft-must-not-leak')) {
        fail('P6 immediate: fork child composer leaked the New-B unsent draft');
      }
      // Busy released: no NEW error toasts, exactly one "Forked from"
      // success toast, and the sidebar error surface untouched.
      const forkOutcome = await page.evaluate(() => ({
        newErrorToasts: document.querySelectorAll('#toasts .toast--error:not([data-e2e-stale])').length,
        newToasts: [...document.querySelectorAll('#toasts .toast')].map((t) => t.textContent || ''),
      }));
      if (forkOutcome.newErrorToasts !== 0 || forkOverlay.sidebarError !== '') {
        fail(`P6 immediate: fork success surfaced errors: ${JSON.stringify({ forkOutcome, staleErrorToastsFork })}`);
      }
      if (!forkOutcome.newToasts.some((text) => text.includes('Forked from'))) {
        fail(`P6 immediate: fork success never surfaced the "Forked from" outcome toast (got ${JSON.stringify(forkOutcome.newToasts)})`);
      }
      await page.screenshot({ path: `${evidence}/projects-p6-immediate-fork-child-b.png`, fullPage: true });

      // Return leg: switching back to B restores B's exact unsent draft.
      await page.evaluate((target) => {
        const row = [...document.querySelectorAll('.session-sidebar__switch')]
          .find((candidate) => candidate.dataset.sessionId === target);
        if (!row) throw new Error(`missing New-B row ${target}`);
        row.click();
      }, newBId);
      await waitRowActive(page, newBId, 'P6 immediate return');
      await waitFor(
        page,
        (draft) => (document.getElementById('prompt-input')?.value ?? '') === draft,
        'P6 immediate: returning to the New-B target did not restore its exact unsent draft',
        20000,
        'projects-immediate-newb-draft-must-not-leak'
      );
    }

    /* ---- P6-settled: the panel's bounded fork-listbox flow (NOT the
     * immediate race). Starts by switching back to the seeded source. ---- */
    await page.evaluate((sessionId) => {
      const row = [...document.querySelectorAll('.session-sidebar__switch')]
        .find((candidate) => candidate.dataset.sessionId === sessionId);
      if (row) row.click();
    }, sourceSessionId);
    await waitRowActive(page, sourceSessionId, 'P6 settled');
    {
      const activeBefore = await page.evaluate(() =>
        document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || ''
      );
      if (activeBefore !== sourceSessionId) {
        fail(`P6 settled: flow did not start from the seeded project-B source (got ${activeBefore})`);
      }
      // A forkable user message on the seeded B session, then one panel
      // refresh so the fork select carries it.
      await page.fill('#prompt-input', 'projects-fork-source');
      await page.press('#prompt-input', 'Enter');
      await waitFor(
        page,
        (r) => document.body.textContent.includes(r),
        'P6: fork-source prompt never round-tripped',
        30000,
        'sessions-reply: projects-fork-source'
      );
      await page.click('#session-refresh-btn');
      await waitFor(
        page,
        () => [...document.querySelectorAll('#session-fork-list [role="option"]')]
          .some((o) => o.id.startsWith('session-fork-option-') && (o.textContent || '').includes('projects-fork-source')),
        'P6: fork listbox never exposed the source message',
        30000
      );

      // Composer isolation (New-B cut): an UNSent source draft must survive in
      // the source view slot and never leak into the fresh target composer.
      // (The production React New→Fork race itself is proven in P6-immediate
      //   above; this settled flow only exercises the listbox controls.)

      // New explicit cwd B while source is the B seed: the fresh target
      // session under B becomes active.
      const newBframesBefore = await page.evaluate(() => window.__rpiNewSessionFrames.length);
      await page.fill('#session-new-cwd-input', projectB);
      await page.click('#session-new-btn');
      await waitFor(
        page,
        (sourceId) => {
          const active = document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '';
          return active !== '' && active !== sourceId;
        },
        'P6: New-B target never became active',
        30000,
        sourceSessionId
      );
      const newBId = await page.evaluate(() =>
        document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || ''
      );
      const newBFrames = await page.evaluate((before) => window.__rpiNewSessionFrames.slice(before), newBframesBefore);
      if (newBFrames.length !== 1 || newBFrames[0]?.cwd !== projectB) {
        fail(`P6: expected exactly one new_session with cwd=${projectB} (got ${JSON.stringify(newBFrames)})`);
      }
      // The new-B panel owns its mount load: the directory input resets EMPTY
      // on the freshly keyed target panel.
      const freshInput = await page.inputValue('#session-new-cwd-input');
      if (freshInput !== '') {
        fail(`P6: New-B target panel did not reset the directory input empty (got ${JSON.stringify(freshInput)})`);
      }

      // Composer isolation (New-B cut): the fresh target composer/attachments
      // start EMPTY — the source draft never leaks into the child.
      const newBComposer = await page.inputValue('#prompt-input');
      if (newBComposer !== '') {
        fail(`P6: New-B target composer leaked a draft (got ${JSON.stringify(newBComposer)})`);
      }

      // Give the new B session one forkable message and reload the panel.
      await page.fill('#prompt-input', 'projects-fork-from-b');
      await page.press('#prompt-input', 'Enter');
      await waitFor(
        page,
        (r) => document.body.textContent.includes(r),
        'P6: fork-from-B prompt never round-tripped',
        30000,
        'sessions-reply: projects-fork-from-b'
      );
      await page.click('#session-refresh-btn');
      await waitFor(
        page,
        () => [...document.querySelectorAll('#session-fork-list [role="option"]')]
          .some((o) => o.id.startsWith('session-fork-option-') && (o.textContent || '').includes('projects-fork-from-b')),
        'P6: fork listbox never exposed the new-B message',
        30000
      );

      // Composer isolation (Fork cut): a newB-side UNSent draft must stay in
      // the newB slot; the fork child composer carries ONLY the fork's
      // editorText (the selected user prompt), never the newB draft.
      await page.fill('#prompt-input', 'projects-newb-draft-must-not-leak');
      const selectedForkOption = await page.evaluate(() =>
        document.querySelector('#session-fork-list [role="option"][aria-selected="true"]')?.textContent || ''
      );
      if (!selectedForkOption.includes('projects-fork-from-b')) {
        fail(`P6: fork listbox selected the wrong fork point (got ${JSON.stringify(selectedForkOption)})`);
      }

      // Settled Fork from the current (target B) panel — this flow already
      // paid the prompt + refresh + listbox costs, so it is NOT the immediate
      // race (P6-immediate proves that). The wire frame must still name the
      // NEW TARGET, never the stale render-captured source.
      const forkFramesBefore = await page.evaluate(() => window.__rpiForkFrames.length);
      await page.click('#session-fork-btn');
      await waitFor(
        page,
        ({ before, source }) => {
          const frames = window.__rpiForkFrames.slice(before);
          if (frames.length === 0) return false;
          const active = document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '';
          return active !== '' && active !== source && active !== window.__rpiForkFrames[before]?.sessionId;
        },
        'P6 settled: fork child never activated',
        30000,
        { before: forkFramesBefore, source: newBId }
      );
      const forkFrames = (await page.evaluate((before) => window.__rpiForkFrames.slice(before), forkFramesBefore))
        .filter((frame) => frame?.type === 'fork');
      if (forkFrames.length !== 1) {
        fail(`P6 settled: expected exactly one panel fork frame (got ${JSON.stringify(forkFrames)})`);
      }
      // The fork child's authoritative state: cwd == project B and its file
      // under B's encoded dir (the old bug produced a child with cwd A).
      const childCwd = await panelCwdTitle(page);
      const childFile = await panelSessionFileTitle(page);
      if (childCwd !== projectB) {
        fail(`P6: fork child backend cwd is not project B (got ${childCwd})`);
      }
      if (path.dirname(childFile) !== sessionDirB) {
        fail(`P6: fork child session file is not under B's encoded dir (got ${childFile})`);
      }
      if (childFile === sourceFile) {
        fail('P6: fork child reused the source session file');
      }
      // The fork child row is ACTIVE in the sidebar from the lifecycle cut.
      const childActiveId = await page.evaluate(() =>
        document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || ''
      );
      if (!childActiveId || childActiveId === newBId || childActiveId === sourceSessionId) {
        fail(`P6: fork child row is not the active sidebar row (got ${childActiveId})`);
      }
      await waitFor(page, (sessionId) => {
        const response = window.__rpiForkPages.findLast((page) => page.sessionId === sessionId)?.frame;
        return response?.success === true && Array.isArray(response.data?.messages);
      }, 'P6: child authoritative fork page never arrived', 30000, childActiveId);
      const childForkState = await page.evaluate((sessionId) => ({
        page: window.__rpiForkPages.findLast((page) => page.sessionId === sessionId).frame.data,
        forkDisabled: document.getElementById('session-fork-btn')?.hasAttribute('disabled'),
        cloneDisabled: document.getElementById('session-clone-btn')?.hasAttribute('disabled'),
        options: document.querySelectorAll('#session-fork-list [role="option"]').length,
      }), childActiveId);
      if (childForkState.page.messages.length !== 0 || childForkState.options !== 0
        || childForkState.forkDisabled !== true || childForkState.cloneDisabled !== false) {
        fail(`P6: empty fork child controls disagree with authoritative page: ${JSON.stringify(childForkState)}`);
      }
      // Composer isolation (Fork cut): the child composer carries EXACTLY the
      // fork's editorText (the selected fork point), and the newB-side UNSent
      // draft never leaks into the child.
      const childComposer = await page.inputValue('#prompt-input');
      if (childComposer !== selectedForkOption) {
        fail(`P6: fork child composer must hold exactly the fork editorText (got ${JSON.stringify(childComposer)}, expected ${JSON.stringify(selectedForkOption)})`);
      }
      if (childComposer.includes('projects-newb-draft-must-not-leak')) {
        fail(`P6: fork child composer leaked the newB-side draft (got ${JSON.stringify(childComposer)})`);
      }
      await page.screenshot({ path: `${evidence}/projects-p6-fork-child-b.png`, fullPage: true });

      /* Mobile viewport: the SAME immediate New-B→Fork race. New explicit B,
       * a deterministic forkable seed, then the fork issued at B's first
       * observable active+forkable moment through the production "Fork
       * latest" sidebar control in the drawer — no prompt, no panel refresh,
       * no settling delay, no listbox. The wire frame owner and the child
       * state must match the desktop contract exactly. */
      const mobile = await browser.newPage({ viewport: { width: 390, height: 740 } });
      try {
        if (token) {
          await mobile.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
        }
        await mobile.addInitScript(() => {
          window.__rpiNewSessionFrames = [];
          window.__rpiForkFrames = [];
          const NativeWebSocket = window.WebSocket;
          class ForkProbeWebSocket extends NativeWebSocket {
            send(data) {
              if (typeof data === 'string') {
                try {
                  const frame = JSON.parse(data);
                  if (frame?.type === 'new_session') window.__rpiNewSessionFrames.push(frame);
                  if (frame?.type === 'fork' || frame?.type === 'session_fork_latest') {
                    window.__rpiForkFrames.push(frame);
                  }
                } catch {
                  // Non-JSON frames pass through unchanged.
                }
              }
              return super.send(data);
            }
          }
          for (const key of ['CONNECTING', 'OPEN', 'CLOSING', 'CLOSED']) {
            Object.defineProperty(ForkProbeWebSocket, key, { value: NativeWebSocket[key] });
          }
          window.WebSocket = ForkProbeWebSocket;
        });
        await mobile.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
        await waitFor(
          mobile,
          () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true,
          'P6 mobile: WS did not reach "connected"'
        );
        const mobileSource = await mobile.evaluate(() =>
          document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || ''
        );
        if (!mobileSource) {
          fail('P6 mobile: no active source session after boot');
        }
        await mobile.click('#sidebar-toggle-btn');
        await mobile.click('#session-toggle-btn');
        await waitFor(mobile, () => document.getElementById('session-panel') !== null, 'P6 mobile: session panel did not open');
        // (1) New Session (explicit cwd project B): the fresh target under B
        // activates with exactly one new_session wire frame.
        const mobileNewFramesBefore = await mobile.evaluate(() => window.__rpiNewSessionFrames.length);
        await mobile.fill('#session-new-cwd-input', projectB);
        await mobile.click('#session-new-btn');
        await waitFor(
          mobile,
          (source) => {
            const active = document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '';
            return active !== '' && active !== source;
          },
          'P6 mobile: New-B target never became active',
          30000,
          mobileSource
        );
        const mobileNewB = await mobile.evaluate(() =>
          document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || ''
        );
        const mobileNewFrames = await mobile.evaluate((before) => window.__rpiNewSessionFrames.slice(before), mobileNewFramesBefore);
        if (mobileNewFrames.length !== 1 || mobileNewFrames[0]?.cwd !== projectB) {
          fail(`P6 mobile: expected exactly one new_session with cwd=${projectB} (got ${JSON.stringify(mobileNewFrames)})`);
        }
        // (2) Deterministic forkable seed BEFORE the fork cut.
        await mobile.fill('#prompt-input', 'projects-mobile-immediate-fork-seed');
        await mobile.press('#prompt-input', 'Enter');
        await waitFor(
          mobile,
          (r) => document.body.textContent.includes(r),
          'P6 mobile: forkable seed prompt never round-tripped',
          30000,
          'sessions-reply: projects-mobile-immediate-fork-seed'
        );
        await mobile.fill('#prompt-input', 'projects-mobile-newb-draft-must-not-leak');
        // (3) The race on mobile: close the panel, open the drawer, and issue
        // the fork through the row's production "Fork latest" control — no
        // prompt, no panel refresh, no settling delay, no listbox.
        await mobile.click('#session-close-btn');
        await waitFor(mobile, () => document.getElementById('session-panel') === null, 'P6 mobile: session panel did not close before the fork');
        await mobile.click('#sidebar-toggle-btn');
        await waitFor(
          mobile,
          () => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true,
          'P6 mobile: drawer never opened for the immediate fork'
        );
        const mobileForkFramesBefore = await mobile.evaluate(() => window.__rpiForkFrames.length);
        await mobile.locator(`.session-sidebar__switch[data-session-id="${mobileNewB}"]`).click({ button: 'right' });
        await mobile.locator('.session-sidebar__menu-item', { hasText: 'Fork latest' }).first().click();
        await waitFor(
          mobile,
          ({ before, source }) => {
            const frames = window.__rpiForkFrames.slice(before);
            const active = document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '';
            return frames.length > 0 && active !== '' && active !== source && active !== frames[0]?.sessionId;
          },
          'P6 mobile: fork child never activated',
          30000,
          { before: mobileForkFramesBefore, source: mobileNewB }
        );
        const mobileForkFrames = await mobile.evaluate((before) => window.__rpiForkFrames.slice(before), mobileForkFramesBefore);
        if (mobileForkFrames.length !== 1 || mobileForkFrames[0]?.type !== 'session_fork_latest'
          || mobileForkFrames[0]?.sessionId !== mobileNewB) {
          fail(`P6 mobile: fork frame owner must be the New-B target ${mobileNewB} (got ${JSON.stringify(mobileForkFrames)})`);
        }
        const mobileChildId = await mobile.evaluate(() =>
          document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || ''
        );
        if (!mobileChildId || mobileChildId === mobileNewB || mobileChildId === mobileSource) {
          fail(`P6 mobile: fork child row is not the active sidebar row (got ${mobileChildId})`);
        }
        if ((await mobile.evaluate(() => document.querySelectorAll('#toasts .toast--error').length)) !== 0) {
          fail('P6 mobile: fork success surfaced error toasts');
        }
        // The child's authoritative panel state: cwd == project B, file under
        // B's encoded dir (the panel remounts for the fork child). The fork's
        // onSwitchComplete closes the mobile drawer after the successful cut;
        // wait for that close, then reopen the drawer to reach the panel
        // toggle (a click while still open would CLOSE it).
        await waitFor(
          mobile,
          () => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') !== true,
          'P6 mobile: drawer never closed after the immediate fork cut'
        );
        await mobile.click('#sidebar-toggle-btn');
        await waitFor(
          mobile,
          () => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true,
          'P6 mobile: drawer never reopened for the fork child panel'
        );
        await mobile.click('#session-toggle-btn');
        await waitFor(mobile, () => document.getElementById('session-panel') !== null, 'P6 mobile: session panel did not open for the fork child');
        await waitFor(
          mobile,
          (l) => (document.querySelector('[data-testid="session-project-value"]')?.textContent || '').trim() === l,
          'P6 mobile: fork child panel never showed project B',
          30000,
          PROJECT_B_LABEL
        );
        const mobileChildCwd = await panelCwdTitle(mobile);
        const mobileChildFile = await panelSessionFileTitle(mobile);
        if (mobileChildCwd !== projectB || path.dirname(mobileChildFile) !== sessionDirB) {
          fail(`P6 mobile: fork child state is not under project B (cwd=${mobileChildCwd}, file=${mobileChildFile})`);
        }
        const mobileChildComposer = await mobile.inputValue('#prompt-input');
        if (mobileChildComposer !== 'projects-mobile-immediate-fork-seed') {
          fail(`P6 mobile: fork child composer must hold exactly the seeded fork point (got ${JSON.stringify(mobileChildComposer)})`);
        }
        await mobile.screenshot({ path: `${evidence}/projects-p6-mobile-fork-child-b.png`, fullPage: true });
      } finally {
        await mobile.close().catch(() => {});
      }
    }

    // FINAL completeness gate: the lane may only report PASS (and may only
    // write evidence) once EVERY documented id executed EXACTLY ONCE. A
    // partial ledger can never pass this lane. Startup removed any stale
    // coverage-assertions.json; the atomic tmp+rename below is the only
    // write — a torn or partial evidence file can never be observed by the
    // matrix.
    const missing = DOCUMENTED_IDS.filter((id) => !executed.has(id));
    if (missing.length > 0) {
      fail(`projects evidence incomplete: ${missing.join(', ')} never executed (coverage matrix contract)`);
    }
    const repeatedIds = [...executed.keys()].filter((id) => !DOCUMENTED_IDS.includes(id));
    if (repeatedIds.length > 0) {
      fail(`projects evidence drift: executed-but-undocumented ids: ${repeatedIds.join(', ')}`);
    }
    const coverageTmp = `${coveragePath}.tmp`;
    // Publish the REAL ledger: executed is the Map's insertion order (actual
    // execution order — never a reconstruction of DOCUMENTED_IDS) and counts
    // the real per-ID ledger. The gates above proved documented == executed
    // as exact-once sets; the matrix's ordered gate additionally proves the
    // runtime order matches the documented order.
    fs.writeFileSync(coverageTmp, `${JSON.stringify({
      documented: [...DOCUMENTED_IDS],
      executed: [...executed.keys()],
      counts: Object.fromEntries(executed),
    }, null, 2)}\n`);
    fs.renameSync(coverageTmp, coveragePath);

    console.log(`web-projects: PASSED (${executed.size}/${DOCUMENTED_IDS.length} assertions, P0 all-project catalog, P1 backend cwd A, P2 project-B switch, P3 explicit A target from B, P4 A-disk placement + source-B isolation, P6 fork lifecycle A→New B→immediate Fork desktop+mobile) — evidence at ${coveragePath}`);
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-projects: playwright crashed: ${err && err.stack ? err.stack : err}`);
  process.exit(2);
});
