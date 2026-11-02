// /chat SessionView mermaid scroll+geometry E2E (playwright half of
// E2E.d/web/chat_mermaid.sh).
//
// Drives the REAL /chat SessionView (chat/SessionView.tsx + shared
// markdown.ts) against the real rpi runner and the steering mock:
//   "render rich content"  -> narrow mermaid flowchart + KaTeX
//   "render wide mermaid"  -> long-labeled LR chain, wider than any viewport
// and asserts the SessionView bottom-pin contract end-to-end (see the .sh
// header for the full list). Bootstrap uses the CURRENT /chat UI: mount
// #chat-projects -> New project form -> .chat-project-row selection ->
// Invite runner lobby/overlay (#chat-invite) -> token-required two-step
// ("Token required" candidate -> #chat-runner-token -> "Save token and
// reconnect") -> runner chip opens SessionView.
// SessionView transcript. Screenshots land in the evidence dir for the
// desktop 1360x900 and mobile 390x844 surfaces.
//
// Exit: 0 = every assertion passed; 2 = assertion failure; the sh lane maps
// 1 to setup failure (node/chromium/npm unavailable), so this script never
// exits 1.
import { chromium } from 'playwright';

const chatUrl = process.env.RPI_CHAT_URL;
const runnerOrigin = process.env.RPI_RUNNER_ORIGIN;
const runnerToken = process.env.RPI_RUNNER_TOKEN || '';
const runnerCwd = process.env.RPI_RUNNER_CWD || '';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';

// Same bound as the /web scroll lane: absorbs the app's synchronous pin vs.
// the measurement race; any real mid-transcript jump fails hard.
const PIN_MAX_REMAINING = 120;

let failures = 0;
function fail(message) {
  console.error(`web-chat-mermaid: FAIL: ${message}`);
  failures += 1;
  process.exit(2);
}

async function waitFor(page, fn, label, timeoutMs = 30000, arg) {
  try {
    await page.waitForFunction(fn, arg, { timeout: timeoutMs });
  } catch {
    fail(`${label} (timeout ${timeoutMs}ms)`);
  }
}

const TRANSCRIPT = '.chat-session__transcript';
async function waitForPinned(page, label) {
  const deadline = Date.now() + 15000;
  while (Date.now() < deadline) {
    const geometry = await page.evaluate(geometryFn);
    if (geometry?.hostVisible && geometry.remaining <= PIN_MAX_REMAINING) return;
    await page.waitForTimeout(100);
  }
  fail(label);
}

// The SessionView pin+geometry contract, read from the live DOM.
function geometryFn() {
  const transcript = document.querySelector('.chat-session__transcript');
  if (!transcript) return null;
  const svg = transcript.querySelector('.md-mermaid-host--rendered svg');
  if (!svg) return null;
  const viewport = svg.closest('.md-mermaid-viewport');
  const host = svg.closest('.md-mermaid-host');
  const svgBox = svg.getBoundingClientRect();
  const viewportBox = viewport.getBoundingClientRect();
  const cs = getComputedStyle(svg);
  return {
    scrollTop: transcript.scrollTop,
    scrollHeight: transcript.scrollHeight,
    clientHeight: transcript.clientHeight,
    remaining: transcript.scrollHeight - transcript.scrollTop - transcript.clientHeight,
    wideMark: svg.getAttribute('data-mermaid-wide'),
    minWidthStyle: cs.minWidth,
    svgWidth: svgBox.width,
    svgLeftGap: svgBox.left - viewportBox.left,
    svgRightGap: viewportBox.right - svgBox.right,
    viewportClientWidth: viewport.clientWidth,
    viewportScrollWidth: viewport.scrollWidth,
    hostClientWidth: host ? host.clientWidth : 0,
    hostVisible: !!host && host.clientWidth > 0 && host.offsetHeight > 0,
  };
}

async function sendPrompt(page, text) {
  await page.fill('[aria-label="Session prompt"]', text);
  await page.press('[aria-label="Session prompt"]', 'Enter');
}

async function openSessionView(page, cwd) {
  // Current /chat bootstrap: New project -> select row -> Invite runner
  // dialog -> token-required two-step -> connected runner -> SessionView.
  if (await page.locator('#chat-mobile-back-projects').isVisible()) await page.click('#chat-mobile-back-projects');
  await page.locator('#chat-project-add-btn:visible, #chat-empty-new-project:visible').first().click();
  await waitFor(page, () => document.getElementById('chat-project-form') !== null, 'New project form never opened', 15000);
  await page.fill('#chat-project-label', 'Mermaid Project');
  await page.fill('#chat-project-cwd', cwd);
  await page.click('#chat-project-add');
  await waitFor(page, () => [...document.querySelectorAll('.chat-project-row')].some(
    (row) => {
      const label = row.querySelector('.chat-project-row__label');
      if (!label) return false;
      const name = [...label.childNodes]
        .filter((node) => !(node instanceof Element) || !node.classList.contains('chat-channel-hash'))
        .map((node) => node.textContent || '')
        .join('')
        .trim();
      return name === 'Mermaid Project' && row.getAttribute('data-selected') === 'true';
    },
  ), 'new project row was never created and selected', 15000);
  // Tap the row: on mobile this also switches the pane to the conversation.
  if (await page.locator('#chat-mobile-back-projects').isVisible()) await page.click('#chat-mobile-back-projects');
  await page.locator('.chat-project-row', { hasText: 'Mermaid Project' }).first().click();
  await waitFor(page, () => [...document.querySelectorAll('.chat-project-row')].some(
    (row) => {
      const label = row.querySelector('.chat-project-row__label');
      if (!label) return false;
      const name = [...label.childNodes]
        .filter((node) => !(node instanceof Element) || !node.classList.contains('chat-channel-hash'))
        .map((node) => node.textContent || '')
        .join('')
        .trim();
      return name === 'Mermaid Project' && row.getAttribute('data-selected') === 'true';
    },
  ), 'project row was never selected', 15000);
  await page.click('#chat-invite-runner-btn');
  await waitFor(page, () => document.getElementById('chat-invite') !== null, 'Invite runner dialog never opened', 15000);
  await page.fill('#chat-runner-label', 'Mermaid Runner');
  await page.fill('#chat-runner-address', runnerOrigin);
  await page.click('#chat-runner-add');
  // Token-gated runner: the first add halts in token_required and the dialog
  // offers the correct two-step: "Token required" candidate -> token field ->
  // "Save token and reconnect".
  const runnerCard = page.locator('#chat-invite .chat-machine-card', { hasText: runnerOrigin }).first();
  await runnerCard.locator('button', { hasText: 'Token required' }).click({ timeout: 30000 });
  await page.fill('#chat-runner-token', runnerToken);
  await page.click('#chat-runner-add');
  // saveTokenAndInvite awaits the first successful probe, then maps the
  // project; the dialog closes only after the mapping is active.
  await waitFor(page, () => document.getElementById('chat-invite') === null, 'invite dialog never closed after the token two-step', 45000);
  await waitFor(page, () => {
    const chip = document.querySelector('.chat-chip[data-runner-id]');
    return chip !== null && chip.querySelector('.chat-status-dot')?.getAttribute('data-tone') === 'ok';
  }, 'connected runner chip never appeared', 30000);
  await page.locator('.chat-chip[data-runner-id]').first().click();
  await waitFor(page, () => document.querySelector('.chat-session__transcript') !== null, 'SessionView transcript never mounted', 30000);
}
async function main() {
  const bootstrapping = new WeakSet();
  const observeConsole = (page, label) => {
    bootstrapping.add(page);
    page.on('console', (msg) => {
      if (msg.type() !== 'error') return;
      const expected = `WebSocket connection to '${runnerOrigin.replace(/^http/, 'ws')}/ws' failed: HTTP Authentication failed; no valid credentials available`;
      if (bootstrapping.has(page) && msg.text() === expected) return;
      pageErrors.push(`${label}: ${msg.text()}`);
    });
  };
  if (!chatUrl || !runnerOrigin || !runnerToken || !runnerCwd) {
    fail('chat URL, runner origin, token, and workspace cwd are required');
  }
  const launchOptions = chromePath ? { executablePath: chromePath } : {};
  const browser = await chromium.launch(launchOptions);
  const pageErrors = [];
  try {
    // ---------------- Desktop 1360x900 ----------------
    const page = await browser.newPage({ viewport: { width: 1360, height: 900 } });
    page.on('pageerror', (error) => pageErrors.push(`page error: ${error.message}`));
    observeConsole(page, 'console error');
    await page.goto(chatUrl, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.getElementById('chat-projects') !== null, '/chat app did not mount');

    await openSessionView(page, runnerCwd);
    bootstrapping.delete(page);

    // 1+2. Narrow diagram: async mermaid hydration must leave a PINNED
    // viewport glued to the bottom (the user-reported blank-middle defect).
    await sendPrompt(page, 'render rich content');
    await waitFor(page, () => document.querySelector('.chat-session__transcript .md-mermaid-host--rendered svg') !== null, 'mermaid SVG never rendered in /chat', 45000);
    await waitForPinned(page, 'transcript did not stay pinned across async mermaid hydration');
    let g = await page.evaluate(geometryFn);
    if (g.scrollTop <= 0) fail(`pinned viewport never scrolled to the bottom: ${JSON.stringify(g)}`);
    // 3. Narrow center geometry: not wide, centered by margin-inline:auto.
    if (g.wideMark !== null) fail(`narrow diagram misclassified wide: ${JSON.stringify(g)}`);
    if (Math.abs(g.svgLeftGap - g.svgRightGap) > 2) fail(`narrow diagram not visually centered: ${JSON.stringify(g)}`);
    if (g.svgWidth > g.viewportClientWidth) fail(`narrow diagram overflows its viewport: ${JSON.stringify(g)}`);
    await page.screenshot({ path: `${evidence}/chat-mermaid-desktop-narrow.png`, fullPage: false });

    // 4. A genuine user scroll away is NEVER stolen: real wheel-up, then a
    // new streamed reply must not move the viewport.
    await page.locator(TRANSCRIPT).hover();
    await page.mouse.wheel(0, -400);
    await page.waitForTimeout(300);
    const frozenAt = await page.evaluate(() => {
      const transcript = document.querySelector('.chat-session__transcript');
      return {
        scrollTop: transcript.scrollTop,
        scrollHeight: transcript.scrollHeight,
        clientHeight: transcript.clientHeight,
      };
    });
    if (frozenAt.scrollHeight - frozenAt.scrollTop - frozenAt.clientHeight < PIN_MAX_REMAINING) {
      fail('wheel-up did not actually scroll the viewport away from the bottom');
    }
    await sendPrompt(page, 'plain follow-up please');
    await waitFor(page, () => (document.querySelectorAll('.chat-session__transcript .chat-session-message--assistant').length >= 2), 'second assistant message never arrived', 45000);
    await page.waitForTimeout(800); // let trailing deltas/hydration settle
    const afterSecond = await page.evaluate(() => document.querySelector('.chat-session__transcript').scrollTop);
    if (Math.abs(afterSecond - frozenAt.scrollTop) > 2) {
      fail(`user scroll-up was stolen: scrollTop ${frozenAt.scrollTop} -> ${afterSecond}`);
    }

    // 5. Re-pin at the bottom, then the WIDE diagram: wide classification +
    // horizontal scroll inside its own viewport row.
    await page.mouse.wheel(0, 4000);
    await page.waitForTimeout(300);
    await sendPrompt(page, 'render wide mermaid');
    await waitFor(page, () => document.querySelector('.chat-session__transcript svg[data-mermaid-wide="true"]') !== null, 'wide diagram never classified wide', 45000);
    await waitForPinned(page, 'wide hydration did not keep the re-pinned viewport at the bottom');
    const wide = await page.evaluate(() => {
      const transcript = document.querySelector('.chat-session__transcript');
      const svg = transcript.querySelector('svg[data-mermaid-wide="true"]');
      const viewport = svg.closest('.md-mermaid-viewport');
      return {
        minWidth: svg.style.minWidth,
        viewportClientWidth: viewport.clientWidth,
        viewportScrollWidth: viewport.scrollWidth,
        svgWidth: svg.getBoundingClientRect().width,
      };
    });
    if (!(parseFloat(wide.minWidth) > 0)) fail(`wide svg lacks intrinsic min-width: ${JSON.stringify(wide)}`);
    if (wide.viewportScrollWidth > wide.viewportClientWidth) {
      fail(`wide diagram scrolled; /chat SessionView scales inside the cap (chat/styles.css:1767-1788): ${JSON.stringify(wide)}`);
    }
    if (wide.svgWidth > wide.viewportClientWidth + 1) {
      fail(`wide svg overflowed the /chat mermaid cap: ${JSON.stringify(wide)}`);
    }
    await page.screenshot({ path: `${evidence}/chat-mermaid-desktop-wide.png`, fullPage: false });

    if (pageErrors.length > 0) fail(`page errors on desktop: ${pageErrors.join(' | ')}`);
    await page.close();

    // ---------------- Mobile 390x844 ----------------
    // A fresh browser page owns a FRESH localStorage (no durable restore):
    // bootstrap the same current-UI journey — New project -> invite runner
    // with the token-required two-step -> tap the project row to open the
    // conversation pane -> SessionView.
    const mobile = await browser.newPage({ viewport: { width: 390, height: 844 } });
    mobile.on('pageerror', (error) => pageErrors.push(`mobile page error: ${error.message}`));
    observeConsole(mobile, 'mobile console error');
    await mobile.goto(chatUrl, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(mobile, () => document.getElementById('chat-projects') !== null, 'mobile /chat did not mount');
    await openSessionView(mobile, runnerCwd);
    bootstrapping.delete(mobile);
    await sendPrompt(mobile, 'render rich content');
    await waitFor(mobile, () => document.querySelector('.chat-session__transcript .md-mermaid-host--rendered svg') !== null, 'mobile mermaid SVG never rendered', 45000);
    await waitForPinned(mobile, 'mobile transcript did not stay pinned across async mermaid hydration');
    const mobileNarrow = await mobile.evaluate(geometryFn);
    if (mobileNarrow.svgWidth > mobileNarrow.viewportClientWidth + 1) {
      fail(`mobile diagram overflowed the /chat mermaid cap: ${JSON.stringify(mobileNarrow)}`);
    }
    if (Math.abs(mobileNarrow.svgLeftGap - mobileNarrow.svgRightGap) > 2) {
      fail(`fitting mobile diagram must be centered: ${JSON.stringify(mobileNarrow)}`);
    }
    await mobile.screenshot({ path: `${evidence}/chat-mermaid-mobile-narrow.png`, fullPage: false });
    const mobileHostsBefore = await mobile.locator('.chat-session__transcript .md-mermaid-host--rendered svg').count();
    await sendPrompt(mobile, 'render wide mermaid');
    await waitFor(mobile, (before) => document.querySelectorAll('.chat-session__transcript .md-mermaid-host--rendered svg').length > before, 'mobile second diagram never rendered', 45000, mobileHostsBefore);
    const mobileWide = await mobile.evaluate(() => {
      const svg = [...document.querySelectorAll('.chat-session__transcript svg[data-mermaid-wide="true"]')].at(-1);
      const viewport = svg.closest('.md-mermaid-viewport');
      return { viewportClientWidth: viewport.clientWidth, viewportScrollWidth: viewport.scrollWidth };
    });
    if (mobileWide.viewportScrollWidth > mobileWide.viewportClientWidth) {
      fail(`mobile wide diagram scrolled; /chat SessionView scales inside the cap: ${JSON.stringify(mobileWide)}`);
    }
    await mobile.screenshot({ path: `${evidence}/chat-mermaid-mobile-wide.png`, fullPage: false });
    if (pageErrors.length > 0) fail(`page errors on mobile: ${pageErrors.join(' | ')}`);
    await mobile.close();

    console.log('web-chat-mermaid: PASSED (async mermaid pin, narrow center geometry, user-upscroll frozen, wide fit-inside-cap, desktop 1360x900 + mobile 390x844)');
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((error) => {
  console.error(`web-chat-mermaid: FAIL: ${error && error.message ? error.message : error}`);
  process.exit(2);
});