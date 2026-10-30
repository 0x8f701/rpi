// Web trusted-plugin command lane (playwright half of
// E2E.d/web/plugin_command.sh).
//
// Environment:
//   RPI_URL              http://127.0.0.1:<port>/web
//   RPI_TOKEN            token file content
//   RPI_PLUGIN_COMMAND   registered extension command name
//   RPI_PLUGIN_RESULT    expected summary text (handler return)
//   RPI_CHROME           system Chrome executable (optional)
//   RPI_EVIDENCE         screenshot directory
//
// Assertions:
//   P1  picker lists /<command> after marketplace install
//   P2  real click inserts /<command> with no auto-submit (no user bubble)
//   P3  Enter paints a summary bubble labeled with the command and the result
//   P4  mobile 390×844 picker still lists /<command>
//   P5  zero pageerror / console.error (favicon filtered)

import { chromium } from 'playwright';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const commandName = process.env.RPI_PLUGIN_COMMAND || 'e2e-plugin-ping';
const commandResult = process.env.RPI_PLUGIN_RESULT || 'pong';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';
const pageErrors = [];
const consoleErrors = [];

function fail(message) {
  console.error(`web-plugin-command: FAIL: ${message}`);
  process.exit(2);
}

async function waitFor(page, fn, label, timeoutMs = 25000, arg) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await page.evaluate(fn, arg)) return;
    await page.waitForTimeout(120);
  }
  fail(`${label} (timeout ${timeoutMs}ms)`);
}

async function openPicker(page) {
  const alreadyOpen = await page.evaluate(
    () => document.querySelector('.command-picker__popover') !== null
  );
  if (alreadyOpen) return;
  await page.click('#command-btn');
  await waitFor(
    page,
    () => document.querySelector('.command-picker__popover') !== null,
    'command picker popover did not open'
  );
}

async function chooseCommand(page, name) {
  const row = page.locator('.command-picker__option').filter({
    has: page.locator('.command-picker__name', { hasText: new RegExp(`^/${name}$`) }),
  });
  await row.first().waitFor({ state: 'visible', timeout: 25000 });
  await row.first().click();
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
    page.on('pageerror', (err) => pageErrors.push(err.message));
    page.on('console', (message) => {
      if (message.type() === 'error') consoleErrors.push(message.text());
    });

    await page.setViewportSize({ width: 1280, height: 800 });
    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'P1: page title missing');
    await waitFor(
      page,
      () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true,
      'P1: WS did not reach connected'
    );

    await openPicker(page);
    await waitFor(
      page,
      (want) => {
        const names = Array.from(document.querySelectorAll('.command-picker__option .command-picker__name'))
          .map((el) => el.textContent.trim());
        return names.includes(`/${want}`);
      },
      `P1: picker did not list /${commandName}`,
      25000,
      commandName,
    );
    await page.fill('.command-picker__search', commandName);
    await waitFor(
      page,
      (want) => {
        const names = Array.from(document.querySelectorAll('.command-picker__option .command-picker__name'))
          .map((el) => el.textContent.trim());
        return names.length === 1 && names[0] === `/${want}`;
      },
      `P1: picker search did not isolate /${commandName}`,
      10000,
      commandName,
    );
    await page.screenshot({ path: `${evidence}/plugin-picker-desktop.png`, fullPage: true });

    await chooseCommand(page, commandName);
    await waitFor(
      page,
      (want) => (document.getElementById('prompt-input')?.value.trim() || '') === `/${want}`,
      `P2: selecting ${commandName} did not insert /${commandName}`,
      10000,
      commandName,
    );
    const autoSubmitted = await page.evaluate((want) =>
      Array.from(document.querySelectorAll('.msg--user')).some((el) =>
        (el.textContent || '').includes(`/${want}`)
      ),
      commandName,
    );
    if (autoSubmitted) fail('P2: plugin command auto-submitted — a user bubble appeared without Enter');

    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      (args) => {
        const bubbles = Array.from(document.querySelectorAll('.msg--summary'));
        return bubbles.some((b) => {
          const label = b.querySelector('.msg--summary__label')?.textContent?.trim() || '';
          const text = b.querySelector('.msg--summary__text')?.textContent || '';
          return label === args.name && text.includes(args.result);
        });
      },
      `P3: /${commandName} did not render the ${commandResult} summary bubble`,
      20000,
      { name: commandName, result: commandResult },
    );
    const leakedUser = await page.evaluate((want) =>
      Array.from(document.querySelectorAll('.msg--user')).some((el) =>
        (el.textContent || '').includes(`/${want}`)
      ),
      commandName,
    );
    if (leakedUser) fail('P3: intercepted /run still created a user bubble');
    await page.screenshot({ path: `${evidence}/plugin-pong-desktop.png`, fullPage: true });

    await page.setViewportSize({ width: 390, height: 844 });
    await openPicker(page);
    await page.fill('.command-picker__search', commandName);
    await waitFor(
      page,
      (want) => {
        const names = Array.from(document.querySelectorAll('.command-picker__option .command-picker__name'))
          .map((el) => el.textContent.trim());
        return names.length === 1 && names[0] === `/${want}`;
      },
      `P4: mobile picker search did not isolate /${commandName}`,
      25000,
      commandName,
    );
    await page.screenshot({ path: `${evidence}/plugin-picker-mobile.png`, fullPage: true });

    if (pageErrors.length) fail(`P5: page errors: ${JSON.stringify(pageErrors)}`);
    const clean = consoleErrors.filter((t) => !t.includes('favicon'));
    if (clean.length) fail(`P5: console errors: ${JSON.stringify(clean)}`);

    console.log(`web-plugin-command: PASSED (picker lists /${commandName}; no auto-submit; Enter -> ${commandResult}; desktop+mobile; 0 page/console errors)`);
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-plugin-command: FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});
