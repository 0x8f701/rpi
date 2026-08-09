// V8 coverage collection hook — preloaded with `node --import` by
// E2E.d/web/lib/fixture.sh::web_run_playwright when RPI_COVERAGE_DIR is set.
//
// It patches the lane's own `playwright` module so that every page the lane
// opens collects V8 JS coverage (page.coverage.startJSCoverage) and, on
// browser close, dumps the raw entries to $RPI_COVERAGE_DIR/<lane>-<seq>.json.
// The reporter (crates/pi-cli/web/scripts/coverage-report.mjs) converts those
// payloads to Istanbul coverage through the bundle's inline source map.
//
// When RPI_COVERAGE_DIR is unset the hook is a strict no-op passthrough, so
// normal lanes are unaffected.
//
// The hook imports `playwright` from the LANE's ephemeral install
// (process.cwd()/node_modules), matching the module instance the lane itself
// imports, so the patch is visible to it.

import fs from 'node:fs';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

const dir = process.env.RPI_COVERAGE_DIR || '';
const lane = process.env.RPI_COVERAGE_LANE || 'lane';
const enabled = dir.length > 0;

if (enabled) {
  fs.mkdirSync(dir, { recursive: true });

  async function loadPlaywright() {
    const candidates = [
      path.join(process.cwd(), 'node_modules', 'playwright', 'index.js'),
      path.join(process.cwd(), 'node_modules', 'playwright-core', 'index.js'),
    ];
    for (const candidate of candidates) {
      if (fs.existsSync(candidate)) {
        return import(pathToFileURL(candidate).href);
      }
    }
    return import('playwright');
  }

  const pw = await loadPlaywright();
  const chromium = pw.chromium || pw.default?.chromium;
  if (!chromium) {
    console.error('[coverage-hook] playwright module found but chromium export missing — coverage collection disabled');
    process.exit(1);
  }

  let pageSeq = 0;
  const tracked = new Set(); // pages still collecting

  async function startOn(page) {
    if (tracked.has(page)) return;
    tracked.add(page);
    try {
      await page.coverage.startJSCoverage({ resetOnNavigation: false });
    } catch (err) {
      tracked.delete(page);
      console.error(`[coverage-hook] startJSCoverage failed: ${err && err.message ? err.message : err}`);
    }
  }

  async function dumpFrom(page) {
    if (!tracked.has(page)) return;
    tracked.delete(page);
    let entries;
    try {
      entries = await page.coverage.stopJSCoverage();
    } catch (err) {
      console.error(`[coverage-hook] stopJSCoverage failed: ${err && err.message ? err.message : err}`);
      return;
    }
    const usable = entries.filter((e) => e && typeof e.source === 'string' && Array.isArray(e.functions) && e.functions.length > 0);
    if (usable.length === 0) {
      console.error('[coverage-hook] page produced no usable coverage entries');
      return;
    }
    const file = path.join(dir, `${lane}-${++pageSeq}.json`);
    fs.writeFileSync(file, JSON.stringify(usable));
    console.error(`[coverage-hook] wrote ${file} (${usable.length} scripts)`);
  }

  function wrapContext(context) {
    const origNewPage = context.newPage.bind(context);
    context.newPage = async (...args) => {
      const page = await origNewPage(...args);
      await startOn(page);
      return page;
    };
    return context;
  }

  function wrapBrowser(browser) {
    const origNewPage = browser.newPage.bind(browser);
    browser.newPage = async (...args) => {
      const page = await origNewPage(...args);
      await startOn(page);
      return page;
    };
    const origNewContext = browser.newContext.bind(browser);
    browser.newContext = async (...args) => {
      const context = await origNewContext(...args);
      return wrapContext(context);
    };
    const origClose = browser.close.bind(browser);
    browser.close = async (...args) => {
      for (const page of [...tracked]) {
        await dumpFrom(page).catch(() => {});
      }
      return origClose(...args);
    };
    return browser;
  }

  const origLaunch = chromium.launch.bind(chromium);
  chromium.launch = async (...args) => wrapBrowser(await origLaunch(...args));

  const origConnect = chromium.connect && chromium.connect.bind(chromium);
  if (origConnect) {
    chromium.connect = async (...args) => wrapBrowser(await origConnect(...args));
  }
}
