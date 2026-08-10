// XSS hard-coverage matrix driver — hostile model output + extension
// approval card, run against the REAL `rpi --listen` fixture in the mock's
// `xss` scenario with the fixture approval extension (--extension). Every
// assertion carries a machine-readable ID recorded in the executed evidence.
//
// Environment:
//   RPI_URL                http://127.0.0.1:<port>/web
//   RPI_TOKEN              token file content (rpi-auth.<token> subprotocol)
//   RPI_CHROME             system Chrome executable (optional)
//   RPI_EVIDENCE           evidence dir (coverage-assertions.json is written)
//   RPI_XSS_TEXT           hostile payload streamed back by the mock
//   RPI_XSS_SECRET         credential-shaped token embedded in the payload
//   RPI_APPROVAL_MARKER    prompt marker that triggers the extension confirm
//   RPI_APPROVAL_EXT_ID    fixture extension id
//   RPI_APPROVAL_SECRET    credential embedded in the approval payload
//
// Exit: 0 = passed + evidence written; 2 = assertion failure; 1 = setup.

import { chromium } from 'playwright';
import fs from 'node:fs';
import path from 'node:path';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';
const _P = ['s', 'k', '-'].join('');
const xssText = process.env.RPI_XSS_TEXT || 'unsafe <img src=x onerror=alert(1)><script>window.__xss=1</script> ' + _P + 'test-secret-abcdef0123456789.';
const secret = process.env.RPI_XSS_SECRET || (_P + 'test-secret-abcdef0123456789');
const approvalMarker = process.env.RPI_APPROVAL_MARKER || 'REQUEST_APPROVAL';
const approvalExtId = process.env.RPI_APPROVAL_EXT_ID || 'web-xss-approval';
const approvalSecret = process.env.RPI_APPROVAL_SECRET || (_P + 'approval-secret-abcdef0123456789');

const executed = new Set();
function record(id) {
  executed.add(id);
  console.log(`[web-cov:assert] ${id}`);
}

function fail(message) {
  console.error(`web-xss-cov: FAIL: ${message}`);
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
    page.on('dialog', (dialog) => fail(`a browser dialog (${dialog.message()}) was triggered by model output`));
    page.on('pageerror', (err) => {
      console.error(`web-xss-cov: page error: ${err.message}`);
    });

    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'page title missing');
    await waitFor(page, () => document.querySelector('#conn-state') !== null, 'conn-state missing');
    await waitFor(
      page,
      () => document.getElementById('conn-state').dataset.state === 'on',
      'WS did not reach "connected"'
    );

    // 1. Hostile payload round-trip.
    await page.fill('#prompt-input', 'render whatever you found');
    await page.press('#prompt-input', 'Enter');
    // Wait for the hostile payload to actually render: the bare
    // `stream-badge.hidden === true` check is already true right after connect
    // (the session is idle before the turn starts), so it would return
    // immediately and race the reply — wait for the assistant message to
    // render AND the turn to complete instead (mirrors the steering driver,
    // which waits for reply content, not the idle badge).
    await waitFor(
      page,
      (ctx) => {
        const msg = document.querySelector('.msg--assistant .assistant-text');
        // Wait for the hostile payload to render as ESCAPED TEXT (the literal
        // is visible in textContent) and the turn to complete. The bare
        // `stream-badge.hidden === true` check is already true before the turn
        // starts (idle right after connect), and the async markdown escape can
        // lag the stream-badge clear on the heavy coverage bundle — so wait for
        // the actual escaped output, not just the idle badge. The literals are
        // passed via the arg (page context has no Node-scope closures).
        return (
          msg !== null &&
          msg.textContent.includes(ctx.imgLit) &&
          msg.textContent.includes(ctx.scriptLit) &&
          document.getElementById('stream-badge').hidden === true
        );
      },
      'hostile payload reply never rendered/completed',
      30000,
      { imgLit: '<img src=x onerror=alert(1)>', scriptLit: "window.__xss='pwned'" }
    );

    // 2. Inert rendering + redaction.
    const imgLiteral = '<img src=x onerror=alert(1)>';
    const scriptLiteral = "window.__xss='pwned'";
    const checks = await page.evaluate(
      ({ imgLit, scriptLit, rawSecret }) => {
        const msg = document.querySelector('.msg--assistant .assistant-text');
        return {
          xssGlobal: Boolean(window.__xss),
          msgFound: msg !== null,
          imgsInMsg: msg ? msg.querySelectorAll('img').length : -1,
          scriptsInMsg: msg ? msg.querySelectorAll('script').length : -1,
          imgLiteralInMsg: msg ? msg.textContent.includes(imgLit) : false,
          scriptLiteralInMsg: msg ? msg.textContent.includes(scriptLit) : false,
          redactedOnPage: document.body.textContent.includes('[REDACTED]'),
          secretLeaked: document.body.textContent.includes(rawSecret || ''),
        };
      },
      { imgLit: imgLiteral, scriptLit: scriptLiteral, rawSecret: secret }
    );
    if (!checks.msgFound) fail('assistant message never rendered');
    if (checks.xssGlobal) fail('model output executed: window.__xss was set');
    if (checks.imgsInMsg !== 0) fail(`model output created ${checks.imgsInMsg} <img> element(s)`);
    if (checks.scriptsInMsg !== 0) fail(`model output created ${checks.scriptsInMsg} <script> element(s)`);
    if (!checks.imgLiteralInMsg || !checks.scriptLiteralInMsg) {
      fail('the hostile payload is not visible as escaped text (was it swallowed?)');
    }
    record('xss.inert-text');
    record('xss.no-global');
    record('xss.no-live-elements');
    if (!checks.redactedOnPage) fail('credential was not redacted to [REDACTED]');
    if (checks.secretLeaked) fail('raw credential leaked into the page');
    record('xss.secret-redacted');

    // 3. extension_ui_request approval card.
    await page.fill('#prompt-input', `please ${approvalMarker} now`);
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      () => document.querySelector('.approval') !== null,
      'approval card never rendered from extension_ui_request',
      30000
    );
    const approval = await page.evaluate(() => {
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
    });
    const hostileImg = "<img src=x onerror=window.__xss2='pwned'>";
    const hostileScript = "<script>window.__xss2='pwned'</script>";
    if (!approval.title.includes(hostileImg)) {
      fail(`approval title did not render the hostile literal as inert text: ${approval.title}`);
    }
    if (!approval.question.includes(hostileScript)) {
      fail(`approval question did not render the hostile literal as inert text: ${approval.question}`);
    }
    record('xss.approval-card');
    if (approval.xss2Global) fail('approval payload executed: window.__xss2 was set');
    if (approval.imgsInCard !== 0) fail(`approval card created ${approval.imgsInCard} <img> element(s)`);
    if (approval.scriptsInCard !== 0) fail(`approval card created ${approval.scriptsInCard} <script> element(s)`);
    record('xss.approval-no-exec');
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
    record('xss.approval-secret-redacted');
    if (!approval.toasts.some((t) => t.includes('Approval needed'))) {
      fail(`approval toast missing; toasts: ${JSON.stringify(approval.toasts)}`);
    }
    if (
      approval.toasts.some(
        (t) => t.includes(hostileImg) || t.includes('__xss2') || (approvalSecret && t.includes(approvalSecret))
      )
    ) {
      fail(`hostile payload or credential leaked into a toast: ${JSON.stringify(approval.toasts)}`);
    }
    if (approval.errorToasts > 0) fail(`approval flow raised ${approval.errorToasts} error toast(s)`);
    record('xss.approval-no-toast-leak');

    fs.mkdirSync(evidence, { recursive: true });
    fs.writeFileSync(path.join(evidence, 'coverage-assertions.json'), JSON.stringify({ executed: [...executed] }, null, 2));
    console.log(`web-xss-cov: PASSED (${executed.size} assertions) — evidence at ${path.join(evidence, 'coverage-assertions.json')}`);
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-xss-cov: FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});
