// Side chat + maintenance web E2E lane (playwright half of E2E.d/web/extras.sh).
//
// Environment:
//   RPI_URL        http://127.0.0.1:<port>/web
//   RPI_TOKEN      token file content (served via rpi-auth.<token> subprotocol)
//   RPI_SLOW_TAIL  tail of the FIRST slow mock reply ("chunk-four-done")
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
//   Maintenance (compact A→B / rewind / handoff / queue):
//     4. Snapcompact renders the A→B token report ("N → M estimated tokens")
//     5. Rewind lists session records (block appears)
//     6. Handoff renders the envelope
//     7. the queue view renders and Cancel queue reports the drain
//
// The MAIN session is primed with one prompt first so the maintenance panel
// has real session records to compact/rewind against.

import { chromium } from 'playwright';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const slowTail = process.env.RPI_SLOW_TAIL || 'chunk-four-done';
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

/** Click the maintenance action button whose text includes `label`. */
async function clickMaintenanceAction(page, label) {
  const clicked = await page.evaluate((want) => {
    const btn = Array.from(document.querySelectorAll('.maintenance__action')).find((b) =>
      b.textContent.includes(want)
    );
    if (!btn) return false;
    btn.click();
    return true;
  }, label);
  if (!clicked) fail(`maintenance action "${label}" not found`);
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

    // Prime the MAIN session with two turns (request 1 = slow stream,
    // request 2 = instant) so the maintenance panel has compactible records.
    await page.fill('#prompt-input', 'prime the main session');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      (tail) => document.body.textContent.includes(tail),
      'main-session reply never streamed into the DOM',
      30000,
      slowTail
    );
    await page.fill('#prompt-input', 'prime again');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      (tail) => document.body.textContent.includes(tail),
      'second main-session reply never arrived',
      30000,
      'steering-followup-reply'
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

    // ---------- Maintenance ----------
    // The side-chat drawer overlays the header; close it first (the single
    // `activePanel` state shows one panel at a time).
    await page.click('.side-chat .panel-close');
    await waitFor(page, () => document.querySelector('.side-chat') === null, 'side chat panel did not close');
    await page.click('#maintenance-toggle-btn');
    await waitFor(page, () => document.querySelector('.maintenance') !== null, 'maintenance panel did not open');

    // 4. Snapcompact A→B token report.
    await clickMaintenanceAction(page, 'Snapcompact');
    await waitFor(
      page,
      () =>
        Array.from(document.querySelectorAll('.maintenance__result')).some((r) =>
          r.textContent.includes('estimated tokens')
        ),
      'snapcompact never rendered the A→B token report'
    );
    const ab = await page.evaluate(() =>
      Array.from(document.querySelectorAll('.maintenance__result'))
        .map((r) => r.textContent)
        .join(' | ')
    );
    if (!ab.includes('→')) fail(`snapcompact report has no A→B arrow: ${ab}`);
    await page.screenshot({ path: `${evidence}/maintain-snapcompact.png`, fullPage: true });

    // 5. Rewind lists session records.
    await clickMaintenanceAction(page, 'Rewind…');
    await waitFor(
      page,
      () =>
        document.querySelector('.maintenance__list') !== null ||
        Array.from(document.querySelectorAll('.maintenance__result')).some((r) =>
          r.textContent.includes('rewind')
        ),
      'rewind list never appeared'
    );
    await page.screenshot({ path: `${evidence}/maintain-rewind.png`, fullPage: true });

    // 6. Handoff envelope renders.
    await clickMaintenanceAction(page, 'Handoff');
    await waitFor(
      page,
      () => document.querySelector('.maintenance__handoff') !== null,
      'handoff envelope never rendered'
    );
    await page.screenshot({ path: `${evidence}/maintain-handoff.png`, fullPage: true });

    // 7. Queue view + cancel.
    await clickMaintenanceAction(page, 'Queue…');
    await waitFor(
      page,
      () => document.querySelector('.maintenance__queue') !== null,
      'queue view never rendered'
    );
    await clickMaintenanceAction(page, 'Cancel queue');
    await waitFor(
      page,
      () =>
        Array.from(document.querySelectorAll('.maintenance__result')).some((r) =>
          r.textContent.includes('Cancelled')
        ),
      'queue cancel never reported'
    );
    await page.screenshot({ path: `${evidence}/maintain-queue.png`, fullPage: true });

    console.log('web-extras: PASSED (side chat multi-tab + prompt round-trip; snapcompact A→B, rewind, handoff, queue/cancel)');
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-extras: FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});
