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
// cwd; new_session inherits the source cwd and records under the target
// project's encoded default dir), and the Web Session sidebar/panel wiring.
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
//   P3  P3.1 New session inherits project B (project + cwd stay B)
//       P3.2 the new session's file path is under B's encoded dir, never A's
//       P3.3 prompt round-trip flushes the new session (sidebar row appears)
//       P3.4 project-B sidebar group grows to two rows and the new row is
//           the flushed session (summary title) under project B
//   P4  P4.1 on-disk: B's encoded dir holds exactly one NEW session file and
//           its recorded header cwd == project B
//       P4.2 on-disk: A's encoded dir holds only the seeded A file (no new
//           session landed under A)
//
// Every passing contract records its machine-readable ID (P0.1..P4.2); on
// full success the lane writes $RPI_EVIDENCE/coverage-assertions.json
// ({ "executed": [...] }). The Web coverage matrix
// (E2E.d/web/coverage_matrix.mjs, feature "all-project session catalog")
// requires ALL of DOCUMENTED_IDS below: the matrix fails when any named
// contract is absent, and this lane fails — before writing evidence — unless
// every documented ID actually executed.

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
const executed = new Set();
function record(id) {
  executed.add(id);
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
  const tid = typeof id === 'string' ? (id.match(/^P\d+\.\d+/) || [null])[0] : null;
  if (tid) record(tid);
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
  const launchOptions = chromePath ? { executablePath: chromePath } : {};
  const browser = await chromium.launch(launchOptions);
  try {
    const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
    if (token) {
      await page.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
    }
    page.on('pageerror', (err) => {
      console.error(`web-projects: page error: ${err.message}`);
    });

    /* ---------------- P0: boot + all-project catalog ---------------- */
    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'P0.1: page title missing');
    await waitFor(
      page,
      () => document.getElementById('conn-state')?.dataset.state === 'on',
      'P0.1: WS did not reach "connected"'
    );
    // Boot restore picks the NEWEST catalog row (project A seed — the lane
    // fixture pins A's mtime one second newer than B's), so the active
    // session is the seeded project-A session, not the primary runtime.
    await waitRowActive(page, 'project-a-seed', 'P0.1');
    await page.screenshot({ path: `${evidence}/projects-p0-boot.png`, fullPage: true });

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
    if (!cwdA.includes(projectA)) {
      fail(`P1.1: panel backend cwd is not project A (got ${cwdA})`);
    }
    record('P1.1');
    const fileA = await panelSessionFileTitle(page);
    if (!fileA.includes(sessionDirA)) {
      fail(`P1.2: active session file is not under A's encoded dir (got ${fileA})`);
    }
    const hintA = await panelHintText(page);
    if (!hintA.includes(PROJECT_A_LABEL)) {
      fail(`P1.2: New-session hint does not name project A (got ${hintA})`);
    }
    record('P1.2');
    await page.screenshot({ path: `${evidence}/projects-p1-panel-a.png`, fullPage: true });

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
    if (!cwdB.includes(projectB)) {
      fail(`P2.2: panel backend cwd is not project B (got ${cwdB})`);
    }
    record('P2.2');
    const fileB = await panelSessionFileTitle(page);
    if (!fileB.includes(sessionDirB)) {
      fail(`P2.3: active session file is not under B's encoded dir (got ${fileB})`);
    }
    const hintB = await panelHintText(page);
    if (!hintB.includes(PROJECT_B_LABEL)) {
      fail(`P2.3: New-session hint does not name project B (got ${hintB})`);
    }
    record('P2.3');
    await page.screenshot({ path: `${evidence}/projects-p2-panel-b.png`, fullPage: true });

    /* ---------------- P3: New session inherits B ---------------- */
    const beforeNew = await panelSessionFileTitle(page);
    await page.click('#session-new-btn');
    // After New, the panel reloads with the FRESH session: its recorder path
    // differs from the seeded B file (project text stays 'project-b', so wait
    // on the file path — never on the unchanged project label).
    await waitFor(
      page,
      (prev) => {
        const el = document.querySelector('[data-testid="session-file-value"]');
        const title = el ? el.getAttribute('title') || el.textContent || '' : '';
        return title !== prev && title.endsWith('.jsonl');
      },
      'P3.1: new session file path never appeared in the panel',
      30000,
      beforeNew
    );
    const panelProject = await panelProjectText(page);
    if (panelProject !== PROJECT_B_LABEL) {
      fail(`P3.1: new session project field is not project B (got ${panelProject})`);
    }
    const cwdNew = await panelCwdTitle(page);
    if (!cwdNew.includes(projectB)) {
      fail(`P3.1: new session backend cwd is not project B (got ${cwdNew})`);
    }
    record('P3.1');
    const fileNew = await panelSessionFileTitle(page);
    if (!fileNew.includes(sessionDirB)) {
      fail(`P3.2: new session file is not under B's encoded dir (got ${fileNew})`);
    }
    if (fileNew.includes(sessionDirA)) {
      fail(`P3.2: new session file leaked under A's encoded dir (got ${fileNew})`);
    }
    if (path.dirname(fileNew) !== sessionDirB) {
      fail(`P3.2: new session file parent is not exactly B's encoded dir (got ${fileNew})`);
    }
    if (fileNew === beforeNew) {
      fail('P3.2: New session did not create a new session file (path unchanged)');
    }
    record('P3.2');

    // P3.3: one prompt round-trip flushes the new B session to disk and gives
    // the sidebar a catalog row titled by its summary.
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

    // P3.4: project-B subgroup (under the rpi provider) now holds TWO rows
    // (seeded B + the new session); project A's subgroup is untouched.
    await waitFor(
      page,
      (label) => {
        const groups = [...document.querySelectorAll('.session-sidebar__group')];
        for (const group of groups) {
          const labelEl = group.querySelector('.session-sidebar__group-label');
          if (labelEl && (labelEl.textContent || '').trim() === label) {
            return (group.querySelector('.session-sidebar__group-count')?.textContent || '').trim() === '2';
          }
        }
        return false;
      },
      'P3.4: project-B group count never reached 2',
      30000,
      PROJECT_B_LABEL
    );
    // The flush's catalog row can lag the group count: right after the
    // round-trip the sidebar still shows the overlay row (empty summary ->
    // project-name fallback title) until the next session_list re-list picks
    // up the persisted summary. Wait for the scoped project-B row to be
    // titled by the flush summary before reading it.
    await waitFor(
      page,
      (label) => {
        const list = document.querySelector('.session-sidebar__list');
        const children = list ? [...list.children] : [];
        const start = children.findIndex((li) => {
          const labelEl = li.querySelector('.session-sidebar__group-label');
          return labelEl && (labelEl.textContent || '').trim() === label;
        });
        if (start === -1) return false;
        for (let i = start + 1; i < children.length; i += 1) {
          const li = children[i];
          if (li.classList.contains('session-sidebar__group')) break;
          const row = li.querySelector('.session-sidebar__switch');
          if (row && row.dataset.sessionId !== 'project-b-seed') {
            const title = row.querySelector('.session-sidebar__name')?.textContent || '';
            if (title.includes('projects-new-flush')) return true;
          }
        }
        return false;
      },
      'P3.4: new session row (flush summary) never appeared under project B',
      30000,
      PROJECT_B_LABEL
    );
    const { newRowId, newRowTitle } = await page.evaluate((label) => {
      // Scope to the project-B GROUP. The sidebar flattens group headers and
      // rows as SIBLING <li>s under .session-sidebar__list (a group <li>
      // contains only its head button), so walk the list: every row following
      // the project-B group header, until the next group header, belongs to
      // project B. The group-count wait above guarantees exactly two rows
      // there (seeded B + the new session), so the new row is the only switch
      // in that span that is not the B seed. Searching globally would match
      // the listener's primary-runtime row first (titled by its project
      // fallback, e.g. "workspace"), not the new session.
      const list = document.querySelector('.session-sidebar__list');
      const children = list ? [...list.children] : [];
      const start = children.findIndex((li) => {
        const labelEl = li.querySelector('.session-sidebar__group-label');
        return labelEl && (labelEl.textContent || '').trim() === label;
      });
      let fresh = null;
      if (start !== -1) {
        for (let i = start + 1; i < children.length; i += 1) {
          const li = children[i];
          if (li.classList.contains('session-sidebar__group')) break;
          const row = li.querySelector('.session-sidebar__switch');
          if (row && row.dataset.sessionId !== 'project-b-seed') {
            fresh = row;
            break;
          }
        }
      }
      return {
        newRowId: fresh ? fresh.dataset.sessionId || '' : '',
        newRowTitle: fresh ? (fresh.querySelector('.session-sidebar__name')?.textContent || '').trim() : '',
      };
    }, PROJECT_B_LABEL);
    if (!newRowId) {
      fail('P3.4: new session row never appeared in the sidebar');
    }
    // The new row is the flushed new session: its sidebar title is the
    // round-trip's user-message summary, and it is grouped under project B.
    if (!newRowTitle.includes('projects-new-flush')) {
      fail(`P3.4: new session row title is not the flush summary (got ${newRowTitle})`);
    }
    record('P3.4');
    await page.screenshot({ path: `${evidence}/projects-p3-new-b.png`, fullPage: true });

    /* ---------------- P4: on-disk storage contract ---------------- */
    // The flush raced slightly with the rendered echo; poll until B's encoded
    // dir holds the new file, then assert the exact content/counts.
    const deadline = Date.now() + 30000;
    let bFiles = sessionFiles(sessionDirB);
    while (bFiles.length < 2 && Date.now() < deadline) {
      await new Promise((resolve) => setTimeout(resolve, 250));
      bFiles = sessionFiles(sessionDirB);
    }
    if (bFiles.length !== 2) {
      fail(`P4.1: B's encoded dir does not hold exactly 2 session files (got ${bFiles.join(', ')})`);
    }
    if (!bFiles.some((name) => name.includes('project-b-seed'))) {
      fail(`P4.1: B's encoded dir lost the seeded B file (got ${bFiles.join(', ')})`);
    }
    const newFileName = bFiles.find((name) => !name.includes('project-b-seed'));
    if (!newFileName) {
      fail('P4.1: no new session file under B\'s encoded dir');
    }
    const firstLine = fs.readFileSync(path.join(sessionDirB, newFileName), 'utf8').split('\n', 1)[0];
    const header = JSON.parse(firstLine);
    if (header.type !== 'session' || header.version !== 3 || header.cwd !== projectB) {
      fail(`P4.1: new session header does not record project B (${firstLine})`);
    }
    record('P4.1');

    const aFiles = sessionFiles(sessionDirA);
    if (aFiles.length !== 1 || !aFiles[0].includes('project-a-seed')) {
      fail(`P4.2: A's encoded dir changed (cross-project leak: ${aFiles.join(', ')})`);
    }
    record('P4.2');
    await page.screenshot({ path: `${evidence}/projects-p4-disk-proof.png`, fullPage: true });

    // The lane may only report PASS (and may only write evidence) once the
    // FULL documented matrix executed.
    const missing = DOCUMENTED_IDS.filter((id) => !executed.has(id));
    if (missing.length > 0) {
      fail(`P-lane evidence incomplete: ${missing.join(', ')} never executed (coverage matrix contract)`);
    }
    fs.mkdirSync(evidence, { recursive: true });
    fs.writeFileSync(path.join(evidence, 'coverage-assertions.json'), JSON.stringify({ executed: [...executed] }, null, 2));

    console.log(`web-projects: PASSED (${executed.size}/${DOCUMENTED_IDS.length} assertions, P0 all-project catalog, P1 backend cwd A, P2 project-B switch, P3 New inherits B + B-dir storage, P4 on-disk proof) — evidence at ${path.join(evidence, 'coverage-assertions.json')}`);
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-projects: playwright crashed: ${err && err.stack ? err.stack : err}`);
  process.exit(2);
});
