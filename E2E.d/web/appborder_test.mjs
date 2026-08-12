// Web app-main panel border regression lane (playwright half of
// E2E.d/web/appborder.sh).
//
// Environment:
//   RPI_URL          http://127.0.0.1:<port>/web
//   RPI_TOKEN        token file content (served via rpi-auth.<token> subprotocol)
//   RPI_CHROME       executable path of the system Chrome (optional)
//   RPI_EVIDENCE     evidence dir for screenshots
//
// Asserts the user-visible panel-edge contract (real computed styles):
//   B1  desktop default (dark): .app-main computed border-left/right are
//       1px solid resolving to the theme's --border-strong token
//       (dark rgb(58,67,80)); header border-bottom and .app-main > footer
//       border-top are the same edge; .session-sidebar has NO border-right
//       (no doubled seam); #transcript background == --bg and
//       scrollbar-gutter == stable
//   B2  desktop light (data-theme="light" on <html>): the same .app-main
//       edges resolve to the LIGHT --border-strong (rgb(143,149,158))
//   B3  rail collapse via #sidebar-toggle-btn keeps the .app-main border-left
//       edge (single strong line at x=230 open and x=44 collapsed)
//   B4  mobile (390x844): .app-main border-left/right-style == none (flush
//       edges, no squeeze) and documentElement.scrollWidth <= clientWidth
//       (tolerance 0 — the transcript's own internal overflow is excluded by
//       measuring documentElement, never #transcript)

import { chromium } from 'playwright';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';

function fail(message) {
  console.error(`web-appborder: FAIL: ${message}`);
  process.exit(2);
}

async function waitFor(page, fn, label, timeoutMs = 25000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await page.evaluate(fn)) return;
    await page.waitForTimeout(120);
  }
  fail(`${label} (timeout ${timeoutMs}ms)`);
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
      console.error(`web-appborder: page error: ${err.message}`);
    });

    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'page title missing');
    await waitFor(page, () => document.getElementById('conn-state') !== null, 'conn-state missing');
    await waitFor(
      page,
      () => document.getElementById('conn-state').dataset.state === 'on',
      'WS did not reach "connected"'
    );

    // ---- B1: desktop dark panel edges ----
    const b1 = await page.evaluate(() => {
      const appMain = document.querySelector('.app-main');
      if (!appMain) return null;
      const header = document.querySelector('header');
      const footer = document.querySelector('.app-main > footer');
      const rail = document.querySelector('.session-sidebar');
      const transcript = document.getElementById('transcript');
      const cs = (el) => (el ? getComputedStyle(el) : null);
      const probeToken = (name) => {
        const probe = document.createElement('div');
        probe.style.border = `1px solid var(${name})`;
        document.body.appendChild(probe);
        const color = getComputedStyle(probe).borderLeftColor;
        probe.remove();
        return color;
      };
      return {
        mainLeft: cs(appMain).borderLeftWidth + ' ' + cs(appMain).borderLeftStyle + ' ' + cs(appMain).borderLeftColor,
        mainRight: cs(appMain).borderRightWidth + ' ' + cs(appMain).borderRightStyle + ' ' + cs(appMain).borderRightColor,
        headerBottom: cs(header).borderBottomWidth + ' ' + cs(header).borderBottomStyle + ' ' + cs(header).borderBottomColor,
        footerTop: cs(footer).borderTopWidth + ' ' + cs(footer).borderTopStyle + ' ' + cs(footer).borderTopColor,
        railRight: cs(rail).borderRightWidth + ' ' + cs(rail).borderRightStyle + ' ' + cs(rail).borderRightColor,
        transcriptBg: cs(transcript).backgroundColor,
        transcriptGutter: cs(transcript).scrollbarGutter,
        token: probeToken('--border-strong'),
        bgToken: probeToken('--bg'),
      };
    });
    if (!b1) fail('.app-main not found in the DOM');
    const expectedDark = `1px solid ${b1.token}`;
    if (b1.mainLeft !== expectedDark) fail(`B1: .app-main border-left must be 1px solid --border-strong (got "${b1.mainLeft}", token "${b1.token}")`);
    if (b1.mainRight !== expectedDark) fail(`B1: .app-main border-right must be 1px solid --border-strong (got "${b1.mainRight}")`);
    if (b1.headerBottom !== expectedDark) fail(`B1: header border-bottom must be 1px solid --border-strong (got "${b1.headerBottom}")`);
    if (b1.footerTop !== expectedDark) fail(`B1: .app-main > footer border-top must be 1px solid --border-strong (got "${b1.footerTop}")`);
    if (!b1.railRight.startsWith('0px none ')) {
      fail(`B1: .session-sidebar must carry NO border-right on desktop (got "${b1.railRight}")`);
    }
    if (b1.transcriptBg !== b1.bgToken) fail(`B1: #transcript background must resolve to --bg (got "${b1.transcriptBg}", --bg "${b1.bgToken}")`);
    if (b1.transcriptGutter !== 'stable') fail(`B1: #transcript scrollbar-gutter must be stable (got "${b1.transcriptGutter}")`);
    await page.screenshot({ path: `${evidence}/appborder-dark.png`, fullPage: true });

    // ---- B2: desktop light theme ----
    await page.evaluate(() => document.documentElement.setAttribute('data-theme', 'light'));
    const b2 = await page.evaluate(() => {
      const appMain = document.querySelector('.app-main');
      const cs = getComputedStyle(appMain);
      const probe = document.createElement('div');
      probe.style.border = '1px solid var(--border-strong)';
      document.body.appendChild(probe);
      const token = getComputedStyle(probe).borderLeftColor;
      probe.remove();
      return {
        mainLeft: cs.borderLeftWidth + ' ' + cs.borderLeftStyle + ' ' + cs.borderLeftColor,
        mainRight: cs.borderRightWidth + ' ' + cs.borderRightStyle + ' ' + cs.borderRightColor,
        token,
      };
    });
    const expectedLight = `1px solid ${b2.token}`;
    if (b2.mainLeft !== expectedLight) fail(`B2: light .app-main border-left must track the light --border-strong (got "${b2.mainLeft}", token "${b2.token}")`);
    if (b2.mainRight !== expectedLight) fail(`B2: light .app-main border-right must track the light --border-strong (got "${b2.mainRight}")`);
    await page.screenshot({ path: `${evidence}/appborder-light.png`, fullPage: true });
    await page.evaluate(() => document.documentElement.removeAttribute('data-theme'));

    // ---- B3: rail collapse keeps the .app-main edge ----
    await waitFor(
      page,
      () => document.querySelector('.app-layout--drawer-open') !== null || document.querySelector('.session-sidebar') !== null,
      'sidebar rail never rendered'
    );
    await page.click('#sidebar-toggle-btn');
    await waitFor(
      page,
      () => document.querySelector('.app-layout--drawer-open') === null,
      'rail did not collapse on toggle'
    );
    const b3 = await page.evaluate(() => {
      const cs = getComputedStyle(document.querySelector('.app-main'));
      return cs.borderLeftWidth + ' ' + cs.borderLeftStyle + ' ' + cs.borderLeftColor;
    });
    const expectedCollapsed = `1px solid ${b1.token}`;
    if (b3 !== expectedCollapsed) {
      fail(`B3: .app-main border-left must stay 1px solid --border-strong with the rail collapsed (got "${b3}")`);
    }
    await page.screenshot({ path: `${evidence}/appborder-rail-collapsed.png`, fullPage: true });
    await page.click('#sidebar-toggle-btn');

    // ---- B4: mobile flush edges + zero horizontal overflow ----
    await page.setViewportSize({ width: 390, height: 844 });
    await page.waitForTimeout(300);
    const b4 = await page.evaluate(() => {
      const cs = getComputedStyle(document.querySelector('.app-main'));
      return {
        borderLeft: cs.borderLeftWidth + ' ' + cs.borderLeftStyle,
        borderRight: cs.borderRightWidth + ' ' + cs.borderRightStyle,
        scrollWidth: document.documentElement.scrollWidth,
        clientWidth: document.documentElement.clientWidth,
      };
    });
    if (b4.borderLeft !== '0px none') fail(`B4: mobile .app-main border-left must be none (got "${b4.borderLeft}")`);
    if (b4.borderRight !== '0px none') fail(`B4: mobile .app-main border-right must be none (got "${b4.borderRight}")`);
    if (b4.scrollWidth > b4.clientWidth) {
      fail(`B4: mobile horizontal overflow (scrollWidth ${b4.scrollWidth} > clientWidth ${b4.clientWidth})`);
    }
    await page.screenshot({ path: `${evidence}/appborder-mobile.png`, fullPage: true });

    console.log('web-appborder: PASSED (desktop dark+light .app-main 1px solid --border-strong edges, header/footer edges, no rail seam, stable transcript surface, rail-collapse edge, mobile flush edges + zero overflow)');
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-appborder: FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});
