// XSS + secret-redaction web E2E lane (playwright half of E2E.d/web/xss.sh).
//
// Environment:
//   RPI_URL        http://127.0.0.1:<port>/web
//   RPI_TOKEN      token file content (served via rpi-auth.<token> subprotocol)
//   RPI_XSS_TEXT   the exact model payload the mock returns (must render inert)
//   RPI_XSS_SECRET the credential string that must be redacted to [REDACTED]
//   RPI_APPROVAL_MARKER  prompt marker that triggers the fixture extension's
//                        input hook (which issues an interactive confirm)
//   RPI_APPROVAL_EXT_ID  the fixture extension id shown on the approval card
//   RPI_APPROVAL_SECRET  credential embedded in the approval message
//   RPI_CHROME     executable path of the system Chrome (optional)
//   RPI_EVIDENCE   evidence dir for screenshots
//
// Asserts (regression guard for crates/pi-cli/web/src/redact.ts + markdown.ts):
//   1. the payload arrives in the transcript as INERT TEXT: no dialog, no
//      `window.__xss` global, no <img>/<script> element inside the assistant
//      message, and the raw payload is visible as escaped text
//   2. the sk-* credential is redacted to [REDACTED] everywhere on the page
//      (body text never contains the raw secret)
//   3. a browser-realistic extension_ui_request approval card (the fixture
//      extension's input hook issues a confirm with a hostile title/message):
//      the card renders the payload as INERT TEXT (no window.__xss2, no live
//      elements), no toast carries the payload or raises an error toast, the
//      extension id + terminal notice render, and the embedded credential is
//      redacted on the card

import { chromium } from 'playwright';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const payload = process.env.RPI_XSS_TEXT || '';
const secret = process.env.RPI_XSS_SECRET || '';
const approvalMarker = process.env.RPI_APPROVAL_MARKER || 'REQUEST_APPROVAL';
const approvalExtId = process.env.RPI_APPROVAL_EXT_ID || 'web-xss-approval';
const approvalSecret = process.env.RPI_APPROVAL_SECRET || '';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';

function fail(message) {
  // Exit 2 (not 1): 1 means "playwright setup failure (node/chromium/npm)",
  // which must NOT be confused with an assertion failure (the mock's request
  // counter is stateful and a rerun would see shifted replies).
  console.error(`web-xss: FAIL: ${message}`);
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
  if (!payload) fail('RPI_XSS_TEXT is required');
  const launchOptions = chromePath ? { executablePath: chromePath } : {};
  const browser = await chromium.launch(launchOptions);
  try {
    const page = await browser.newPage();
    let dialogSeen = '';
    page.on('dialog', async (dialog) => {
      dialogSeen = dialog.type();
      await dialog.dismiss().catch(() => {});
    });
    page.on('pageerror', (err) => {
      console.error(`web-xss: page error: ${err.message}`);
    });

    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'page title missing');
    await waitFor(page, () => document.querySelector('#conn-state') !== null, 'conn-state missing');

    // Connect via the rpi-auth.<token> subprotocol.
    await page.fill('#token-input', token);
    await page.click('#connect-btn');
    await waitFor(
      page,
      () => document.getElementById('conn-state').dataset.state === 'on',
      'WS did not reach "connected"'
    );

    // One prompt; the xss mock scenario streams the hostile payload back.
    await page.fill('#prompt-input', 'render whatever you found');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      () => document.getElementById('stream-badge').hidden === true,
      'streaming badge did not clear after the reply completed'
    );
    await page.screenshot({ path: `${evidence}/xss.png`, fullPage: true });

    // 1. Inert rendering: no dialog, no global, no live elements, literal text.
    if (dialogSeen !== '') fail(`a browser dialog (${dialogSeen}) was triggered by model output`);
    const checks = await page.evaluate(
      ({ imgLiteral, scriptLiteral, rawSecret }) => {
        const msg = document.querySelector('.msg--assistant .assistant-text');
        return {
          xssGlobal: Boolean(window.__xss),
          msgFound: msg !== null,
          imgsInMsg: msg ? msg.querySelectorAll('img').length : -1,
          scriptsInMsg: msg ? msg.querySelectorAll('script').length : -1,
          imgLiteralInMsg: msg ? msg.textContent.includes(imgLiteral) : false,
          scriptLiteralInMsg: msg ? msg.textContent.includes(scriptLiteral) : false,
          redactedOnPage: document.body.textContent.includes('[REDACTED]'),
          secretLeaked: document.body.textContent.includes(rawSecret || ''),
        };
      },
      {
        imgLiteral: '<img src=x onerror=alert(1)>',
        scriptLiteral: "window.__xss='pwned'",
        rawSecret: secret,
      }
    );
    if (!checks.msgFound) fail('assistant message never rendered');
    if (checks.xssGlobal) fail('model output executed: window.__xss was set');
    if (checks.imgsInMsg !== 0) fail(`model output created ${checks.imgsInMsg} <img> element(s)`);
    if (checks.scriptsInMsg !== 0) fail(`model output created ${checks.scriptsInMsg} <script> element(s)`);
    if (!checks.imgLiteralInMsg || !checks.scriptLiteralInMsg) {
      fail('the hostile payload is not visible as escaped text (was it swallowed?)');
    }
    if (!checks.redactedOnPage) fail('credential was not redacted to [REDACTED]');
    if (checks.secretLeaked) fail('raw credential leaked into the page');
    await page.screenshot({ path: `${evidence}/xss-redacted.png`, fullPage: true });

    // 3. extension_ui_request approval card (browser-realistic: a real
    //    QuickJS extension's input hook issues an interactive confirm through
    //    the rpi listener; the web client must surface it as a card, not
    //    execute it, and not leak it into a toast).
    await page.fill('#prompt-input', `please ${approvalMarker} now`);
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      () => document.querySelector('.approval') !== null,
      'approval card never rendered from extension_ui_request',
      30000
    );
    const approval = await page.evaluate(
      ({ extId }) => {
        const card = document.querySelector('.approval');
        const text = (sel) => (card ? card.querySelector(sel)?.textContent || '' : '');
        return {
          title: text('.approval__title'),
          question: text('.approval__question'),
          ext: text('.approval__extension'),
          note: text('.approval__note'),
          xss2Global: Boolean(window.__xss2),
          imgsInCard: card ? card.querySelectorAll('img').length : -1,
          scriptsInCard: card ? card.querySelectorAll('script').length : -1,
          toasts: Array.from(document.querySelectorAll('#toasts .toast')).map((t) => t.textContent),
          errorToasts: document.querySelectorAll('#toasts .toast--error').length,
        };
      },
      { extId: approvalExtId }
    );
    const hostileImg = "<img src=x onerror=window.__xss2='pwned'>";
    const hostileScript = "<script>window.__xss2='pwned'</script>";
    if (!approval.title.includes(hostileImg)) {
      fail(`approval title did not render the hostile literal as inert text: ${approval.title}`);
    }
    if (!approval.question.includes(hostileScript)) {
      fail(`approval question did not render the hostile literal as inert text: ${approval.question}`);
    }
    if (approval.xss2Global) fail('approval payload executed: window.__xss2 was set');
    if (approval.imgsInCard !== 0) fail(`approval card created ${approval.imgsInCard} <img> element(s)`);
    if (approval.scriptsInCard !== 0) fail(`approval card created ${approval.scriptsInCard} <script> element(s)`);
    if (approval.ext !== approvalExtId) fail(`approval card extension id wrong: ${approval.ext}`);
    if (!approval.note.includes('Answer in the terminal')) {
      fail(`approval card lost its terminal notice: ${approval.note}`);
    }
    if (approvalSecret && approval.question.includes(approvalSecret)) {
      fail('approval credential leaked unredacted into the card');
    }
    if (approvalSecret && !approval.question.includes('[REDACTED]')) {
      fail('approval credential was not redacted to [REDACTED] on the card');
    }
    if (!approval.toasts.some((t) => t.includes('Approval needed'))) {
      fail(`approval toast missing; toasts: ${JSON.stringify(approval.toasts)}`);
    }
    if (approval.toasts.some((t) => t.includes(hostileImg) || t.includes('__xss2') || (approvalSecret && t.includes(approvalSecret)))) {
      fail(`hostile payload or credential leaked into a toast: ${JSON.stringify(approval.toasts)}`);
    }
    if (approval.errorToasts > 0) fail(`approval flow raised ${approval.errorToasts} error toast(s)`);
    await page.screenshot({ path: `${evidence}/xss-approval.png`, fullPage: true });

    console.log('web-xss: PASSED (inert render + [REDACTED] + approval card safe text/no toast)');
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-xss: FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});
