// Panel chrome E2E: desktop #panel-drawer-resizer sits on #loop-panel top edge
// (never strikethrough); toggle "Loop"; mobile resizer display:none.
// Env: RPI_URL / RPI_TOKEN / RPI_CHROME / RPI_EVIDENCE (same as presentation_test.mjs).
// Exit unchanged: 0=pass+evidence; 2=assertion fail; 1=playwright setup fail.

import { createRequire } from 'node:module';
import fs from 'node:fs';
import path from 'node:path';

const require = createRequire(import.meta.url);
const { chromium } = require('playwright');

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';

const pageErrors = [];
const consoleErrors = [];
const executed = new Map();

function record(id) {
  executed.set(id, (executed.get(id) || 0) + 1);
  console.log(`[panel-chrome:assert] ${id}`);
}
function fail(message) {
  console.error(`panel-chrome: FAIL: ${message}`);
  process.exit(2);
}

function observeBrowserErrors(page) {
  page.on('pageerror', (err) => pageErrors.push(err.message));
  page.on('console', (msg) => {
    if (msg.type() === 'error') consoleErrors.push(msg.text());
  });
}

async function connectPage(page, viewport) {
  if (token) {
    await page.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
  }
  await page.setViewportSize(viewport);
  await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
  await page.waitForFunction(
    () => document.title === 'rpi web',
    undefined,
    { timeout: 30000 },
  );
  await page.waitForFunction(
    () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true,
    undefined,
    { timeout: 30000 },
  );
}

function rects(page) {
  return page.evaluate(() => {
    const box = (sel) => {
      const el = document.querySelector(sel);
      return el ? el.getBoundingClientRect().toJSON() : null;
    };
    const resizer = document.querySelector('#panel-drawer-resizer');
    return {
      panel: box('#loop-panel'),
      resizer: box('#panel-drawer-resizer'),
      resizerDisplay: resizer ? getComputedStyle(resizer).display : null,
      toggleText: (document.getElementById('loop-toggle-btn')?.textContent || '').trim(),
    };
  });
}

const browser = await chromium.launch({ headless: true, executablePath: chromePath || undefined });
try {
  const page = await browser.newPage();
  observeBrowserErrors(page);

  await connectPage(page, { width: 1440, height: 900 });
  await page.click('#loop-toggle-btn');
  await page.waitForSelector('#loop-panel', { timeout: 5000 });
  await page.waitForTimeout(400);

  const desktop = await rects(page);
  if (!desktop.panel || !desktop.resizer) fail(`Loop panel or resizer missing: ${JSON.stringify(desktop)}`);
  if (desktop.toggleText !== 'Loop') fail(`toggle label is "${desktop.toggleText}", expected "Loop"`);
  record('loop-label');

  const gap = desktop.resizer.top - desktop.panel.top;
  if (gap > 1 || gap < -6) {
    fail(`resizer not centered on the panel top edge (resizer.top ${desktop.resizer.top} vs panel.top ${desktop.panel.top}, gap ${gap.toFixed(1)}px, allowed [-6, +1]) — off-edge handles paint over panel text or float detached`);
  }
  record('resizer-on-panel-edge');

  await page.screenshot({ path: path.join(evidence, 'panel-chrome-desktop.png') });

  await page.setViewportSize({ width: 390, height: 844 });
  await page.waitForTimeout(400);
  const mobile = await rects(page);
  if (mobile.resizerDisplay !== 'none') fail(`mobile resizer must be display:none, got "${mobile.resizerDisplay}"`);
  record('mobile-resizer-hidden');
  await page.screenshot({ path: path.join(evidence, 'panel-chrome-mobile.png') });

  if (pageErrors.length > 0) fail(`page errors: ${pageErrors.join(' | ')}`);
  if (consoleErrors.length > 0) fail(`console errors: ${consoleErrors.join(' | ')}`);

  fs.writeFileSync(
    path.join(evidence, 'panel-chrome-assertions.json'),
    JSON.stringify({ executed: [...executed.keys()], desktop, mobile }, null, 2),
  );
  console.log('panel-chrome: PASS');
} finally {
  await browser.close();
}
