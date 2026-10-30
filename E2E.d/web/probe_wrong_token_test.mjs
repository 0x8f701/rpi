// Diagnostic probe: wrong-token toast behavior against a given bundle.
import { chromium } from 'playwright';

const url = process.env.RPI_URL;
const wrongToken = process.env.RPI_WRONG_TOKEN || 'definitely-wrong-token';
const evidence = process.env.RPI_EVIDENCE || '.';

const browser = await chromium.launch({});
const page = await browser.newPage();
page.on('pageerror', (err) => console.error('PROBE-PAGEERROR:', err.message));
page.on('console', (msg) => {
  if (msg.type() === 'error') console.error('PROBE-CONSOLE-ERROR:', msg.text());
});
await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
await page.waitForFunction(() => document.title === 'rpi web', { timeout: 20000 });
console.log('PROBE: title ok');
await page.waitForFunction(
  () => (document.querySelector('.empty-hint')?.textContent || '').includes('Connecting to the control plane'),
  { timeout: 15000 }
);
console.log('PROBE: empty-hint settled');
await page.click('#settings-toggle-btn');
await page.waitForFunction(() => document.querySelector('#settings-token-input') !== null, { timeout: 10000 });
await page.fill('#settings-token-input', wrongToken);
await page.click('#settings-token-save-btn');
await page.click('#settings-close-btn');
await page.waitForTimeout(5000);
const state = await page.evaluate(() => ({
  toasts: Array.from(document.querySelectorAll('#toasts .toast')).map((t) => ({
    text: t.textContent,
    cls: t.className,
    kind: t.getAttribute('data-toast-kind'),
  })),
  hint: document.querySelector('.empty-hint')?.textContent || '',
  sendDisabled: document.getElementById('send-btn')?.hasAttribute('disabled') ?? null,
}));
console.log('PROBE-STATE:', JSON.stringify(state, null, 2));
await page.screenshot({ path: `${evidence}/probe-wrong.png`, fullPage: true });
await browser.close();
process.exit(0);
