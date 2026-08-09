// Mobile viewport (375×667) web E2E lane (playwright half of E2E.d/web/mobile.sh).
//
// Environment:
//   RPI_URL        http://127.0.0.1:<port>/web
//   RPI_TOKEN      token file content (served via rpi-auth.<token> subprotocol)
//   RPI_SLOW_TAIL  tail of the FIRST slow mock reply ("chunk-four-done")
//   RPI_CHROME     executable path of the system Chrome (optional)
//   RPI_EVIDENCE   evidence dir for screenshots
//
// Asserts the phone-width shell contract (styles.css media queries):
//   1. core flow works at 375×667: connect, prompt round-trip, panel opens
//   2. no horizontal page overflow (page and with the drawer open)
//   3. the drawer is full-screen width (== viewport)
//   4. the composer sits above the fold (bottom <= innerHeight)
//   5. touch targets are >= 44px (send/connect/abort/panel-toggle)
//   6. #thinking-select is hidden at phone width (CSS media query)

import { chromium } from 'playwright';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const slowTail = process.env.RPI_SLOW_TAIL || 'chunk-four-done';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';

const VIEWPORT = { width: 375, height: 667 };

function fail(message) {
  // Exit 2 (not 1): 1 means "playwright setup failure (node/chromium/npm)",
  // which must NOT be confused with an assertion failure (the mock's request
  // counter is stateful and a rerun would see shifted replies).
  console.error(`web-mobile: FAIL: ${message}`);
  process.exit(2);
}

async function waitFor(page, fn, label, timeoutMs = 25000, arg) {
  try {
    await page.waitForFunction(fn, arg, { timeout: timeoutMs });
  } catch {
    fail(`${label} (timeout ${timeoutMs}ms)`);
  }
}

async function main() {
  if (!url) fail('RPI_URL is required');
  const launchOptions = chromePath ? { executablePath: chromePath } : {};
  const browser = await chromium.launch(launchOptions);
  try {
    const page = await browser.newPage({ viewport: VIEWPORT });
    page.on('pageerror', (err) => {
      console.error(`web-mobile: page error: ${err.message}`);
    });

    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'page title missing');
    await waitFor(page, () => document.querySelector('#conn-state') !== null, 'conn-state missing');

    // 1. Core flow: connect with the token, prompt round-trip, open a panel.
    await page.fill('#token-input', token);
    await page.click('#connect-btn');
    await waitFor(
      page,
      () => document.getElementById('conn-state').dataset.state === 'on',
      'WS did not reach "connected"'
    );
    await page.fill('#prompt-input', 'hello from a phone');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      (tail) => document.body.textContent.includes(tail),
      'slow reply never streamed into the DOM',
      30000,
      slowTail
    );
    // F5: the header hamburger must be VISIBLE at phone width (the media-query
    // rule must win the cascade — an unconditional display:none after the
    // media block hid it at every width) and clicking it must open the
    // session sidebar drawer.
    const toggleDisplay = await page.evaluate(
      () => getComputedStyle(document.getElementById('sidebar-toggle-btn')).display
    );
    if (toggleDisplay === 'none') {
      fail('#sidebar-toggle-btn is hidden at phone width (CSS cascade regression)');
    }
    const toggleBox = await page.locator('#sidebar-toggle-btn').boundingBox();
    if (!toggleBox || toggleBox.width < 20 || toggleBox.height < 20) {
      fail(`#sidebar-toggle-btn has no clickable area at phone width (box ${JSON.stringify(toggleBox)})`);
    }
    await page.click('#sidebar-toggle-btn');
    await waitFor(
      page,
      () => {
        const layout = document.querySelector('.app-layout');
        const sidebar = document.querySelector('.session-sidebar');
        return (
          layout !== null &&
          layout.classList.contains('app-layout--drawer-open') &&
          sidebar !== null &&
          (sidebar.getBoundingClientRect().left || -1) <= 1
        );
      },
      'session sidebar drawer never opened from the hamburger'
    );
    await page.screenshot({ path: `${evidence}/mobile-sidebar-open.png`, fullPage: true });
    // The panel toggles now live IN the drawer (feature nav), so open the
    // Todo panel from there while the drawer is still open.
    await page.click('#todos-toggle-btn');
    await waitFor(page, () => document.getElementById('todo-panel') !== null, 'todo panel did not open');
    await page.screenshot({ path: `${evidence}/mobile-panel.png`, fullPage: true });

    // 2-6. Shell contract at 375×667.
    const metrics = await page.evaluate(() => {
      const rect = (sel) => {
        const el = document.querySelector(sel);
        if (!el) return null;
        const r = el.getBoundingClientRect();
        return { top: r.top, bottom: r.bottom, left: r.left, width: r.width, height: r.height };
      };
      const composer = rect('#composer-buttons');
      const drawer = rect('#todo-panel');
      const targets = ['#send-btn', '#connect-btn', '#abort-btn', '#todos-toggle-btn'].map((sel) => ({
        sel,
        height: rect(sel) ? rect(sel).height : -1,
      }));
      const thinking = document.getElementById('thinking-select');
      const thinkingDisplay = thinking ? getComputedStyle(thinking).display : 'missing';
      return {
        innerWidth: window.innerWidth,
        innerHeight: window.innerHeight,
        scrollWidth: document.documentElement.scrollWidth,
        composerBottom: composer ? composer.bottom : -1,
        drawerWidth: drawer ? drawer.width : -1,
        targets,
        thinkingDisplay,
      };
    });

    if (metrics.scrollWidth > metrics.innerWidth + 1) {
      fail(`horizontal overflow: scrollWidth ${metrics.scrollWidth} > viewport ${metrics.innerWidth}`);
    }
    if (metrics.drawerWidth < metrics.innerWidth - 1) {
      fail(`drawer is not full-screen width: ${metrics.drawerWidth} vs viewport ${metrics.innerWidth}`);
    }
    if (metrics.composerBottom < 0 || metrics.composerBottom > metrics.innerHeight) {
      fail(`composer sits below the fold: bottom ${metrics.composerBottom} > innerHeight ${metrics.innerHeight}`);
    }
    for (const t of metrics.targets) {
      if (t.height < 44) fail(`touch target ${t.sel} is ${t.height}px (must be >= 44px)`);
    }
    if (metrics.thinkingDisplay !== 'none') {
      fail(`#thinking-select should be hidden at phone width, computed display = ${metrics.thinkingDisplay}`);
    }
    await page.screenshot({ path: `${evidence}/mobile-contract.png`, fullPage: true });

    console.log('web-mobile: PASSED (core flow + no overflow + full-screen drawer + 44px targets + composer on-screen)');
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-mobile: FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});
