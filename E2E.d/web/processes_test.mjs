// Web Processes panel lane (playwright half of E2E.d/web/processes.sh).
//
// Environment:
//   RPI_URL        http://127.0.0.1:<port>/web
//   RPI_TOKEN      token file content
//   RPI_CHROME     system Chrome executable (optional)
//   RPI_EVIDENCE   screenshot directory
//
// Assertions:
//   P1  #processes-toggle-btn opens #processes-panel; empty list is visible
//   P2  Run dispatches exactly one process_spawn (bash -c + cwd + label)
//   P3  labeled row appears; Stop dispatches process_stop
//   P4  mobile 390x844 drawer still opens #processes-panel
//   P5  zero pageerror / console.error (favicon filtered)

import { chromium } from 'playwright';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';
const pageErrors = [];
const consoleErrors = [];
const sentFrames = [];
const SPAWN_LABEL = 'e2e-sleep';
const SPAWN_COMMAND = 'printf processes-e2e-ready; sleep 90';

function fail(message) {
  console.error(`web-processes: FAIL: ${message}`);
  process.exit(2);
}

function parseFrame(payload) {
  if (typeof payload !== 'string' || payload === '') return null;
  try {
    const parsed = JSON.parse(payload);
    return parsed && typeof parsed === 'object' ? parsed : null;
  } catch {
    return null;
  }
}

function framesOf(type) {
  return sentFrames.filter((frame) => frame.type === type);
}

async function waitForDom(page, fn, label, timeoutMs = 25000, arg) {
  try {
    await page.waitForFunction(fn, arg, { timeout: timeoutMs });
  } catch {
    fail(`${label} (timeout ${timeoutMs}ms)`);
  }
}

async function waitForFrames(type, want, label, timeoutMs = 15000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (framesOf(type).length >= want) return;
    await new Promise((resolve) => setTimeout(resolve, 80));
  }
  fail(`${label} (timeout ${timeoutMs}ms; got ${framesOf(type).length})`);
}

async function openProcessesPanel(page) {
  const drawerOpen = await page.evaluate(
    () => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true
  );
  if (!drawerOpen) {
    const rail = page.locator('#rail-reopen-btn');
    if (await rail.isVisible()) {
      await rail.click();
    } else {
      await page.locator('#sidebar-toggle-btn').click();
    }
    await waitForDom(
      page,
      () => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true,
      'P1: sidebar drawer did not open'
    );
  }
  await waitForDom(
    page,
    () => document.getElementById('processes-toggle-btn') !== null,
    'P1: #processes-toggle-btn missing'
  );
  await page.locator('#processes-toggle-btn').click({ force: true });
  await waitForDom(
    page,
    () => document.getElementById('processes-panel') !== null,
    'P1: #processes-panel did not open'
  );
}

async function main() {
  if (!url) fail('RPI_URL is required');
  const launchOptions = chromePath ? { executablePath: chromePath } : {};
  const browser = await chromium.launch(launchOptions);
  try {
    const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
    if (token) {
      await page.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
    }
    page.on('pageerror', (err) => pageErrors.push(err.message));
    page.on('console', (message) => {
      if (message.type() === 'error') consoleErrors.push(message.text());
    });
    page.on('websocket', (ws) => {
      ws.on('framesent', (frame) => {
        const parsed = parseFrame(typeof frame.payload === 'string' ? frame.payload : '');
        if (parsed && typeof parsed.type === 'string') sentFrames.push(parsed);
      });
    });

    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitForDom(page, () => document.title === 'rpi web', 'P1: page title missing');
    await waitForDom(
      page,
      () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true,
      'P1: WS did not reach connected'
    );

    await openProcessesPanel(page);
    await waitForFrames('process_list', 1, 'P1: opening the panel did not dispatch process_list');
    const empty = await page.evaluate(() => {
      const emptyRow = document.querySelector('#processes-panel .loop-empty');
      return emptyRow ? emptyRow.textContent.trim() : '';
    });
    if (empty !== 'No supervised processes.') {
      fail(`P1: empty list missing (got ${JSON.stringify(empty)})`);
    }

    const listBeforeSpawn = framesOf('process_list').length;
    await page.fill('#processes-spawn-command', SPAWN_COMMAND);
    await page.fill('#processes-spawn-label', SPAWN_LABEL);
    await page.click('#processes-spawn-btn');
    await waitForFrames('process_spawn', 1, 'P2: Run did not dispatch process_spawn');
    if (framesOf('process_spawn').length !== 1) {
      fail(`P2: expected exactly one process_spawn, got ${framesOf('process_spawn').length}`);
    }
    const spawn = framesOf('process_spawn')[0];
    const spec = spawn && spawn.spec && typeof spawn.spec === 'object' ? spawn.spec : null;
    const argv = spec && Array.isArray(spec.argv) ? spec.argv : [];
    if (argv.length !== 3 || argv[0] !== 'bash' || argv[1] !== '-c' || argv[2] !== SPAWN_COMMAND) {
      fail(`P2: process_spawn argv was ${JSON.stringify(argv)}`);
    }
    if (typeof spec.cwd !== 'string' || spec.cwd.trim() === '') {
      fail('P2: process_spawn cwd was empty');
    }
    if (spec.label !== SPAWN_LABEL) {
      fail(`P2: process_spawn label was ${JSON.stringify(spec.label)}`);
    }
    await waitForDom(
      page,
      (label) => Array.from(document.querySelectorAll('.process-row__label'))
        .some((el) => (el.textContent || '').trim() === label),
      'P3: spawned row with label never appeared',
      20000,
      SPAWN_LABEL
    );
    const spawnError = await page.evaluate(() => {
      const err = document.querySelector('#processes-panel .loop-error');
      return err ? err.textContent.trim() : '';
    });
    if (spawnError) fail(`P2: spawn surfaced an error: ${spawnError}`);
    await page.screenshot({ path: `${evidence}/processes-desktop-running.png`, fullPage: true });

    const stop = page.locator('.process-row__stop').first();
    await stop.waitFor({ state: 'visible', timeout: 15000 });
    await stop.click();
    await waitForFrames('process_stop', 1, 'P3: Stop did not dispatch process_stop');
    if (framesOf('process_stop').length !== 1) {
      fail(`P3: expected exactly one process_stop, got ${framesOf('process_stop').length}`);
    }
    if (framesOf('process_list').length < listBeforeSpawn) {
      fail('P3: process_list count went backwards after spawn/stop');
    }
    await page.click('#processes-close-btn');
    await waitForDom(
      page,
      () => document.getElementById('processes-panel') === null,
      'P3: close did not unmount #processes-panel'
    );

    await page.setViewportSize({ width: 390, height: 844 });
    await waitForDom(
      page,
      () => window.innerWidth === 390 && window.innerHeight === 844,
      'P4: mobile viewport did not become 390x844'
    );
    await openProcessesPanel(page);
    await page.screenshot({ path: `${evidence}/processes-mobile.png`, fullPage: true });
    const mobileTitle = await page.evaluate(() => {
      const title = document.querySelector('#processes-panel .panel__title');
      return title ? title.textContent.trim() : '';
    });
    if (mobileTitle !== 'Processes') {
      fail(`P4: mobile panel title missing (got ${JSON.stringify(mobileTitle)})`);
    }

    if (pageErrors.length) fail(`P5: page errors: ${JSON.stringify(pageErrors)}`);
    const clean = consoleErrors.filter((t) => !t.includes('favicon'));
    if (clean.length) fail(`P5: console errors: ${JSON.stringify(clean)}`);

    console.log('web-processes: PASSED (panel open + process_spawn/stop; desktop+mobile; 0 page/console errors)');
  } finally {
    await browser.close();
  }
}

main().catch((err) => {
  console.error(`web-processes: FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});
