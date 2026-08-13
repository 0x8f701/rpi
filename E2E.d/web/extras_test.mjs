// Side chat web E2E lane (playwright half of E2E.d/web/extras.sh).
//
// Environment:
//   RPI_URL        http://127.0.0.1:<port>/web
//   RPI_TOKEN      token file content (served via rpi-auth.<token> subprotocol)
//   RPI_CHROME     executable path of the system Chrome (optional)
//   RPI_EVIDENCE   evidence dir for screenshots
//
// Asserts:
//   Side chat (multi-tab):
//     1. the panel opens and starts with the default tab
//     2. a NEW tab is created from the panel form (second tab appears and is
//        activated)
//     3. prompting the tab round-trips through the real side-chat session and
//        the assistant entry renders the streamed mock reply (the side-agent
//        turn may land on either the odd slow or the even instant path, so
//        any non-empty assistant text satisfies the assertion)

import { chromium } from 'playwright';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';

function fail(message) {
  // Exit 2 (not 1): 1 means "playwright setup failure (node/chromium/npm)",
  // which must NOT be confused with an assertion failure (the mock's request
  // counter is stateful and a rerun would see shifted replies).
  console.error(`web-extras: FAIL: ${message}`);
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
    const page = await browser.newPage();
    if (token) {
      await page.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
    }
    page.on('pageerror', (err) => {
      console.error(`web-extras: page error: ${err.message}`);
    });

    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'page title missing');
    await waitFor(page, () => document.querySelector('#conn-state') !== null, 'conn-state missing');

    await waitFor(
      page,
      () => document.getElementById('conn-state').dataset.state === 'on',
      'WS did not reach "connected"'
    );

    // ---------- Side chat ----------
    await page.click('#sidechat-toggle-btn');
    await waitFor(page, () => document.querySelector('.side-chat') !== null, 'side chat panel did not open');

    // 1. Default tab exists.
    await waitFor(
      page,
      () => document.querySelectorAll('.side-chat__tab').length >= 1,
      'side chat never showed the default tab'
    );

    // 2. Create a second tab from the panel form. (The RPC validates tab
    //    names: letters/digits/underscores only — keep the name hyphen-free.)
    await page.fill('.side-chat__new input', 'regression_tab');
    await page.click('.side-chat__new button');
    await waitFor(
      page,
      () =>
        Array.from(document.querySelectorAll('.side-chat__tab-select')).some((b) =>
          b.textContent.includes('regression_tab')
        ),
      'the new tab never appeared in the tab list'
    );
    await waitFor(
      page,
      () =>
        Array.from(document.querySelectorAll('.side-chat__tab-select')).some(
          (b) => b.textContent.includes('regression_tab') && b.getAttribute('aria-selected') === 'true'
        ),
      'the new tab was not activated'
    );
    await page.screenshot({ path: `${evidence}/sidechat-tabs.png`, fullPage: true });

    // 3. Prompt the tab; the side-chat turn round-trips through the real
    //    side session (request 2+ of the steering mock). The side agent may
    //    land on the odd slow stream or the even instant reply, so any
    //    non-empty assistant entry text satisfies the assertion.
    await page.fill('.side-chat__composer textarea', 'hello side agent');
    await page.click('.side-chat__composer button');
    await waitFor(
      page,
      () =>
        Array.from(document.querySelectorAll('.side-chat__entry--assistant .side-chat__text')).some((el) =>
          el.textContent.trim() !== ''
        ),
      'side-chat assistant entry never rendered a reply',
      90000
    );
    const sideEntry = await page.evaluate(() => {
      const entries = Array.from(document.querySelectorAll('.side-chat__entry--assistant'));
      const last = entries[entries.length - 1];
      return last
        ? { text: last.querySelector('.side-chat__text')?.textContent || '', error: last.classList.contains('side-chat__entry--error') }
        : null;
    });
    if (!sideEntry) fail('side-chat assistant entry missing');
    if (sideEntry.error) fail(`side-chat turn ended in an error entry: ${sideEntry.text}`);
    await page.screenshot({ path: `${evidence}/sidechat-reply.png`, fullPage: true });

    console.log('web-extras: PASSED (side chat multi-tab + prompt round-trip)');
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-extras: FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});
