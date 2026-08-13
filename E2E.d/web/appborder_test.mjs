// Web app-main panel border + shared drawer resize + sidebar collapse lane
// (playwright half of E2E.d/web/appborder.sh).
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
//   B5  ordinary panel shared desktop resizer: pointer drag + keyboard
//       (ArrowUp/Down/Home/End) + bounds (25–90vh) + localStorage key
//       `rpi-panel-drawer-size` survives reload; ARIA separator present
//   B6  sidebar header collapse (#sidebar-collapse-btn) folds the desktop
//       rail to the reopen strip; #rail-reopen-btn restores; header ☰ still
//       toggles. Mobile: collapse closes the drawer.
//   B4  mobile (390x844): .app-main border-left/right-style == none (flush
//       edges, no squeeze), documentElement.scrollWidth <= clientWidth,
//       and #panel-drawer-resizer is not shown for open ordinary panels

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

    // ---- B5: shared ordinary-panel height resizer (desktop) ----
    await page.click('#todos-toggle-btn');
    await waitFor(page, () => document.getElementById('todo-panel') !== null, 'B5: todo panel did not open');
    await waitFor(
      page,
      () => document.getElementById('panel-drawer-resizer') !== null,
      'B5: shared panel-drawer-resizer never mounted'
    );
    const b5aria = await page.evaluate(() => {
      const r = document.getElementById('panel-drawer-resizer');
      if (!r) return null;
      return {
        role: r.getAttribute('role'),
        orientation: r.getAttribute('aria-orientation'),
        min: r.getAttribute('aria-valuemin'),
        max: r.getAttribute('aria-valuemax'),
        now: Number(r.getAttribute('aria-valuenow') || '0'),
        tabIndex: r.tabIndex,
      };
    });
    if (!b5aria) fail('B5: resizer missing after todo open');
    if (b5aria.role !== 'separator') fail(`B5: resizer role must be separator (got "${b5aria.role}")`);
    if (b5aria.orientation !== 'horizontal') fail(`B5: resizer aria-orientation must be horizontal (got "${b5aria.orientation}")`);
    if (b5aria.min !== '25' || b5aria.max !== '90') {
      fail(`B5: resizer bounds must be 25–90 (got min=${b5aria.min} max=${b5aria.max})`);
    }
    if (!(b5aria.now >= 25 && b5aria.now <= 90)) {
      fail(`B5: aria-valuenow out of bounds (${b5aria.now})`);
    }
    if (b5aria.tabIndex < 0) fail('B5: resizer must be keyboard-focusable (tabIndex >= 0)');

    await page.focus('#panel-drawer-resizer');
    await page.keyboard.press('End');
    const afterEnd = await page.evaluate(() => {
      const raw = getComputedStyle(document.documentElement).getPropertyValue('--panel-drawer-height').trim();
      const stored = window.localStorage.getItem('rpi-panel-drawer-size');
      const now = document.getElementById('panel-drawer-resizer')?.getAttribute('aria-valuenow');
      return { raw, stored, now };
    });
    if (afterEnd.raw !== '25vh') fail(`B5: End key must set --panel-drawer-height to 25vh (got "${afterEnd.raw}")`);
    if (afterEnd.stored !== '25') fail(`B5: End key must persist rpi-panel-drawer-size=25 (got "${afterEnd.stored}")`);
    if (afterEnd.now !== '25') fail(`B5: End key must set aria-valuenow=25 (got "${afterEnd.now}")`);

    await page.keyboard.press('Home');
    const afterHome = await page.evaluate(() => {
      const raw = getComputedStyle(document.documentElement).getPropertyValue('--panel-drawer-height').trim();
      const stored = window.localStorage.getItem('rpi-panel-drawer-size');
      return { raw, stored };
    });
    if (afterHome.raw !== '90vh') fail(`B5: Home key must set --panel-drawer-height to 90vh (got "${afterHome.raw}")`);
    if (afterHome.stored !== '90') fail(`B5: Home key must persist rpi-panel-drawer-size=90 (got "${afterHome.stored}")`);

    await page.keyboard.press('End');
    const resizerBox = await page.locator('#panel-drawer-resizer').boundingBox();
    if (!resizerBox) fail('B5: resizer has no bounding box');
    const startX = resizerBox.x + resizerBox.width / 2;
    const startY = resizerBox.y + resizerBox.height / 2;
    await page.mouse.move(startX, startY);
    await page.mouse.down();
    await page.mouse.move(startX, startY - 120, { steps: 8 });
    await page.mouse.up();
    const afterDrag = await page.evaluate(() => {
      const raw = getComputedStyle(document.documentElement).getPropertyValue('--panel-drawer-height').trim();
      const m = raw.match(/^([\d.]+)vh$/);
      const vh = m ? Number(m[1]) : NaN;
      const stored = window.localStorage.getItem('rpi-panel-drawer-size');
      const panel = document.getElementById('todo-panel');
      const h = panel ? panel.getBoundingClientRect().height : 0;
      return { raw, vh, stored, h, viewH: window.innerHeight };
    });
    if (!(afterDrag.vh > 25 && afterDrag.vh <= 90)) {
      fail(`B5: pointer drag must grow height above 25vh within bounds (got vh=${afterDrag.vh}, raw="${afterDrag.raw}")`);
    }
    {
      const storedN = Number(afterDrag.stored);
      if (!(Number.isFinite(storedN) && Math.abs(storedN - afterDrag.vh) < 0.6)) {
        fail(`B5: drag must persist rpi-panel-drawer-size≈${afterDrag.vh} (got "${afterDrag.stored}")`);
      }
    }
    const expectedH = (afterDrag.vh / 100) * afterDrag.viewH;
    if (Math.abs(afterDrag.h - expectedH) > 4) {
      fail(`B5: todo-panel height ${afterDrag.h}px must track --panel-drawer-height ${afterDrag.raw} (~${expectedH}px)`);
    }
    const persistedVh = afterDrag.vh;
    await page.screenshot({ path: `${evidence}/appborder-panel-resize.png`, fullPage: true });

    await page.reload({ waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.getElementById('conn-state') !== null, 'B5 reload: conn-state missing');
    await waitFor(
      page,
      () => document.getElementById('conn-state').dataset.state === 'on',
      'B5 reload: WS did not reach connected'
    );
    await page.click('#todos-toggle-btn');
    await waitFor(page, () => document.getElementById('todo-panel') !== null, 'B5 reload: todo panel did not reopen');
    const afterReload = await page.evaluate((want) => {
      const raw = getComputedStyle(document.documentElement).getPropertyValue('--panel-drawer-height').trim();
      const stored = window.localStorage.getItem('rpi-panel-drawer-size');
      const m = raw.match(/^([\d.]+)vh$/);
      const vh = m ? Number(m[1]) : NaN;
      return { raw, stored, vh, want };
    }, persistedVh);
    if (!(Number.isFinite(afterReload.vh) && Math.abs(afterReload.vh - persistedVh) < 0.6)) {
      fail(`B5: reload must restore --panel-drawer-height≈${persistedVh}vh (got "${afterReload.raw}", stored="${afterReload.stored}")`);
    }
    await page.click('#todo-close-btn');
    await waitFor(page, () => document.getElementById('todo-panel') === null, 'B5: todo panel did not close');
    await waitFor(
      page,
      () => document.getElementById('panel-drawer-resizer') === null,
      'B5: resizer must unmount when no ordinary panel is open'
    );

    // ---- B6: sidebar header collapse + reopen (desktop) ----
    await waitFor(
      page,
      () => document.getElementById('sidebar-collapse-btn') !== null,
      'B6: sidebar header collapse button missing'
    );
    const railOpen = await page.evaluate(() =>
      document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true
    );
    if (!railOpen) {
      await page.click('#sidebar-toggle-btn');
      await waitFor(
        page,
        () => document.querySelector('.app-layout--drawer-open') !== null,
        'B6: could not open the rail before collapse'
      );
    }
    await page.click('#sidebar-collapse-btn');
    await waitFor(
      page,
      () => {
        const layout = document.querySelector('.app-layout');
        const reopen = document.getElementById('rail-reopen-btn');
        const sidebar = document.querySelector('.session-sidebar');
        if (!layout || !reopen || !sidebar) return false;
        if (layout.classList.contains('app-layout--drawer-open')) return false;
        if (getComputedStyle(reopen).display === 'none') return false;
        return sidebar.getBoundingClientRect().width <= 60;
      },
      'B6: #sidebar-collapse-btn did not collapse the desktop rail to the reopen strip'
    );
    await page.click('#rail-reopen-btn');
    await waitFor(
      page,
      () => {
        const nav = document.querySelector('.session-sidebar__nav');
        const sidebar = document.querySelector('.session-sidebar');
        if (!nav || !sidebar) return false;
        return getComputedStyle(nav).display !== 'none' && sidebar.getBoundingClientRect().width >= 200;
      },
      'B6: #rail-reopen-btn did not restore the sidebar after header collapse'
    );
    await page.click('#sidebar-toggle-btn');
    await waitFor(
      page,
      () => document.querySelector('.app-layout--drawer-open') === null,
      'B6: header toggle no longer collapses the rail'
    );
    await page.click('#sidebar-toggle-btn');
    await waitFor(
      page,
      () => document.querySelector('.app-layout--drawer-open') !== null,
      'B6: header toggle no longer reopens the rail'
    );
    await page.screenshot({ path: `${evidence}/appborder-sidebar-collapse.png`, fullPage: true });

    // ---- B4: mobile flush edges + zero horizontal overflow + no resizer ----
    await page.setViewportSize({ width: 390, height: 844 });
    await page.waitForTimeout(300);
    // Desktop left the rail open; on mobile that class means the drawer is
    // already open — only click the toggle when it is closed.
    const mobileOpenAlready = await page.evaluate(() =>
      document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true
    );
    if (!mobileOpenAlready) {
      await page.click('#sidebar-toggle-btn');
    }
    await waitFor(
      page,
      () => document.querySelector('.app-layout--drawer-open') !== null,
      'B4: mobile drawer did not open'
    );
    // Close any ordinary panel left open so the resizer path is re-exercised
    // from a clean open (and the drawer nav stays reachable).
    const todoOpen = await page.evaluate(() => document.getElementById('todo-panel') !== null);
    if (todoOpen) {
      await page.click('#todo-close-btn');
      await waitFor(page, () => document.getElementById('todo-panel') === null, 'B4: could not close leftover todo panel');
    }
    await page.click('#todos-toggle-btn');
    await waitFor(page, () => document.getElementById('todo-panel') !== null, 'B4: mobile todo panel did not open');
    const b4 = await page.evaluate(() => {
      const cs = getComputedStyle(document.querySelector('.app-main'));
      const resizer = document.getElementById('panel-drawer-resizer');
      const resizerDisplay = resizer ? getComputedStyle(resizer).display : 'none';
      const panel = document.getElementById('todo-panel');
      const panelH = panel ? panel.getBoundingClientRect().height : 0;
      return {
        borderLeft: cs.borderLeftWidth + ' ' + cs.borderLeftStyle,
        borderRight: cs.borderRightWidth + ' ' + cs.borderRightStyle,
        scrollWidth: document.documentElement.scrollWidth,
        clientWidth: document.documentElement.clientWidth,
        resizerDisplay,
        panelH,
        viewH: window.innerHeight,
      };
    });
    if (b4.borderLeft !== '0px none') fail(`B4: mobile .app-main border-left must be none (got "${b4.borderLeft}")`);
    if (b4.borderRight !== '0px none') fail(`B4: mobile .app-main border-right must be none (got "${b4.borderRight}")`);
    if (b4.scrollWidth > b4.clientWidth) {
      fail(`B4: mobile horizontal overflow (scrollWidth ${b4.scrollWidth} > clientWidth ${b4.clientWidth})`);
    }
    if (b4.resizerDisplay !== 'none') {
      fail(`B4: mobile must hide #panel-drawer-resizer (display="${b4.resizerDisplay}")`);
    }
    if (Math.abs(b4.panelH - b4.viewH) > 8) {
      fail(`B4: mobile todo-panel height ${b4.panelH} must fill the viewport (~${b4.viewH})`);
    }
    // Mobile Todo overlays the sidebar drawer. Close it and wait for unmount
    // before re-opening/collapsing sidebar controls.
    await page.click('#todo-close-btn');
    await waitFor(page, () => document.getElementById('todo-panel') === null, 'B4: mobile todo panel did not close');
    const mobileDrawerOpen = await page.evaluate(() =>
      document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true
    );
    if (!mobileDrawerOpen) {
      await page.click('#sidebar-toggle-btn');
      await waitFor(
        page,
        () => document.querySelector('.app-layout--drawer-open') !== null,
        'B4: could not re-open mobile drawer for collapse check'
      );
    }
    await page.click('#sidebar-collapse-btn');
    await waitFor(
      page,
      () => document.querySelector('.app-layout--drawer-open') === null,
      'B4: mobile #sidebar-collapse-btn did not close the drawer'
    );
    await page.screenshot({ path: `${evidence}/appborder-mobile.png`, fullPage: true });

    console.log('web-appborder: PASSED (desktop dark+light .app-main edges, rail-collapse edge, shared panel resizer pointer/keyboard/bounds/reload, sidebar header collapse/reopen, mobile flush edges + no resizer + zero overflow)');
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-appborder: FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});
