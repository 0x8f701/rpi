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
//   2. while the run streams: primary submit reads "Steer", #abort-btn is
//      rendered and >= 44px, no horizontal overflow, composer on-screen
//   3. no horizontal page overflow (page and with the drawer open)
//   4. the drawer is full-screen width (== viewport)
//   5. the composer sits above the fold (bottom <= innerHeight)
//   6. idle composer: #abort-btn is NOT rendered (active-only), the textarea
//      is the dominant element (usable width >= 240px, height >= 44px)
//   7. touch targets are >= 44px (send/connect/panel-toggle; abort is
//      checked while streaming in #2)
//   8. #thinking-select is hidden at phone width (CSS media query)

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
    await page.click('#settings-toggle-btn');
    await waitFor(page, () => document.querySelector('#settings-token-input') !== null, 'settings token input missing');
    await page.fill('#settings-token-input', token);
    await page.click('#settings-token-save-btn');
    await page.click('#settings-close-btn');
    await waitFor(
      page,
      () => document.getElementById('conn-state').dataset.state === 'on',
      'WS did not reach "connected"'
    );
    // 1a. While the first (slow) stream is in flight, the active-only
    // composer contract holds: the primary submit reads "Steer", the Abort
    // control is rendered and meets the 44px touch target, and the composer
    // stays on-screen with no horizontal overflow.
    await page.fill('#prompt-input', 'hello from a phone');
    await page.press('#prompt-input', 'Enter');
    await waitFor(page, () => document.getElementById('stream-badge').hidden === false, 'streaming never started');
    const streaming = await page.evaluate(() => {
      const rect = (sel) => {
        const el = document.querySelector(sel);
        if (!el) return null;
        const r = el.getBoundingClientRect();
        return { top: r.top, bottom: r.bottom, left: r.left, width: r.width, height: r.height };
      };
      const send = document.querySelector('#send-btn');
      const abort = rect('#abort-btn');
      const footer = rect('footer');
      return {
        sendLabel: send ? send.textContent.trim() : '',
        abortH: abort ? abort.height : -1,
        footerBottom: footer ? footer.bottom : -1,
        scrollWidth: document.documentElement.scrollWidth,
        innerWidth: window.innerWidth,
        innerHeight: window.innerHeight,
      };
    });
    if (streaming.sendLabel !== 'Steer') {
      fail(`primary submit did not switch to Steer while streaming: "${streaming.sendLabel}"`);
    }
    if (streaming.abortH < 44) {
      fail(`#abort-btn touch target is ${streaming.abortH}px while streaming (must be >= 44px)`);
    }
    if (streaming.scrollWidth > streaming.innerWidth + 1) {
      fail(`horizontal overflow while streaming: scrollWidth ${streaming.scrollWidth} > viewport ${streaming.innerWidth}`);
    }
    if (streaming.footerBottom < 0 || streaming.footerBottom > streaming.innerHeight) {
      fail(`composer sits below the fold while streaming: bottom ${streaming.footerBottom} > innerHeight ${streaming.innerHeight}`);
    }
    await page.screenshot({ path: `${evidence}/mobile-streaming.png`, fullPage: true });
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

    // 2-6. Shell contract at 375×667 (idle state — the run has finished).
    const metrics = await page.evaluate(() => {
      const rect = (sel) => {
        const el = document.querySelector(sel);
        if (!el) return null;
        const r = el.getBoundingClientRect();
        return { top: r.top, bottom: r.bottom, left: r.left, width: r.width, height: r.height };
      };
      const footer = rect('footer');
      const ta = rect('#prompt-input');
      const drawer = rect('#todo-panel');
      // #abort-btn is active-only, so it is NOT in the DOM while idle; it is
      // asserted separately while streaming above.
      const targets = ['#send-btn', '#settings-toggle-btn', '#todos-toggle-btn'].map((sel) => ({
        sel,
        height: rect(sel) ? rect(sel).height : -1,
      }));
      const thinking = document.getElementById('thinking-select');
      const thinkingDisplay = thinking ? getComputedStyle(thinking).display : 'missing';
      return {
        innerWidth: window.innerWidth,
        innerHeight: window.innerHeight,
        scrollWidth: document.documentElement.scrollWidth,
        composerBottom: footer ? footer.bottom : -1,
        textareaW: ta ? ta.width : -1,
        textareaH: ta ? ta.height : -1,
        drawerWidth: drawer ? drawer.width : -1,
        targets,
        abortPresent: document.querySelector('#abort-btn') !== null,
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
    // Idle usable composer: with the dedicated Steer/Follow up controls gone
    // and Abort active-only, the textarea must be the dominant composer
    // element on a phone (materially more usable width than the old 4-button
    // row) and keep a usable entry height.
    if (metrics.textareaW < 240) {
      fail(`#prompt-input usable width is ${metrics.textareaW}px at 375px (must be >= 240px after removing dedicated Steer/Follow up)`);
    }
    if (metrics.textareaH < 44) {
      fail(`#prompt-input usable height is ${metrics.textareaH}px (must be >= 44px)`);
    }
    if (metrics.abortPresent) {
      fail('#abort-btn must not render while idle (active-only composer); found in DOM');
    }
    for (const t of metrics.targets) {
      if (t.height < 44) fail(`touch target ${t.sel} is ${t.height}px (must be >= 44px)`);
    }
    if (metrics.thinkingDisplay !== 'none') {
      fail(`#thinking-select should be hidden at phone width, computed display = ${metrics.thinkingDisplay}`);
    }
    await page.screenshot({ path: `${evidence}/mobile-contract.png`, fullPage: true });

    console.log('web-mobile: PASSED (core flow + no overflow + full-screen drawer + 44px targets + composer on-screen + usable mobile textarea + active-only abort)');
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-mobile: FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});
