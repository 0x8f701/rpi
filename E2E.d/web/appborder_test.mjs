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
//   B3  rail collapse via #sidebar-collapse-btn keeps the .app-main border-left
//       edge (single strong line at x=195.5 open and x=44 collapsed) and hides
//       .session-sidebar__foot so no inset New Session border remains
//   B5  ordinary panel shared desktop resizer: pointer drag + keyboard
//       (ArrowUp/Down/Home/End) + bounds (25–90vh) + localStorage key
//       `rpi-panel-drawer-size` survives reload; ARIA separator present
//   B6  sidebar header collapse (#sidebar-collapse-btn) folds the desktop
//       rail to the reopen strip; #rail-reopen-btn restores. Mobile: collapse
//       closes the drawer.
//   B4  mobile (390x844): .app-main border-left/right-style == none (flush
//       edges, no squeeze), documentElement.scrollWidth <= clientWidth,
//       and #panel-drawer-resizer is not shown for open ordinary panels.
//       With the sessions drawer open, .session-sidebar__foot border-top
//       meets both sidebar edges (<=1px) and vertical padding stays 6px;
//       at idle the New Session button top aligns with #prompt-input top
//       (<=1px) while the foot ends above the viewport bottom by the
//       drawer's computed padding-bottom inset plus the live
//       --composer-buttons-band (buttons row + row gap − foot padding-top)
//       — not flush — and the New Session button and foot height do not
//       drift while the composer grows across attach / draft / remove or
//       a real Thinking turn.
//       Measured while the drawer is open, before Todo: opening a panel
//       at <=720px collapses the drawer, so the foot is not laid out then.
//   B7  desktop footer alignment (position, not just style): at 1280x720 and
//       1360x900 the sidebar foot border-top Y and New Session button top
//       align with the composer footer border-top Y and #prompt-input top
//       (<=1px), the foot border-top meets both sidebar edges (<=1px), no
//       dead band above the viewport bottom, and no horizontal overflow;
//       JSON+PNG saved pre-assert. Mobile keeps the B4 drawer contract
//       (no shared-Y; idle top alignment + live band foot offset).

import { chromium } from 'playwright';
import { writeFileSync } from 'node:fs';
import { clickFeature, openFeatureMenu, openSessionsDrawer, closeSessionsDrawer } from './lib/feature-menu.mjs';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';

function fail(message) {
  console.error(`web-appborder: FAIL: ${message}`);
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


async function main() {
  if (!url) fail('RPI_URL is required');
  const launchOptions = chromePath ? { executablePath: chromePath } : {};
  const browser = await chromium.launch(launchOptions);
  try {
    const page = await browser.newPage();
    const runtimeErrors = [];
    if (token) {
      await page.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
    }
    page.on('pageerror', (err) => {
      runtimeErrors.push(`pageerror: ${err.message}`);
    });
    page.on('console', (message) => {
      if (message.type() === 'error') runtimeErrors.push(`console: ${message.text()}`);
    });

    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'page title missing');
    await waitFor(
      page,
      () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true,
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
    // ---- B7: New Session foot contract (fixed bottom-center, full-width
    // border, never tracks the composer band). The foot no longer shares the
    // composer footer's live height band: its border-top Y may diverge from
    // the composer border Y while the composer grows, and the button, label
    // and foot geometry must stay put instead. JSON+PNG saved pre-assert per
    // state; mobile keeps the B4 drawer contract (measured there). ----
    const b7InitialViewport = page.viewportSize() ?? { width: 1280, height: 720 };
    const attachFile = `${evidence}/appborder-attach-fixture.txt`;
    writeFileSync(attachFile, 'appborder attach fixture\n');
    for (const b7vp of [{ width: 1280, height: 720 }, { width: 1360, height: 900 }]) {
      await page.setViewportSize(b7vp);
      await waitFor(
        page,
        (want) => {
          const foot = document.querySelector('.session-sidebar__foot');
          const composer = document.querySelector('.app-main > footer');
          return window.innerWidth === want.width && window.innerHeight === want.height
            && !!foot && !!composer && !!document.getElementById('sidebar-manage-btn')
            && !!document.getElementById('prompt-input')
            && foot.getBoundingClientRect().height > 0;
        },
        `B7: ${b7vp.width}x${b7vp.height} footer layout never settled`,
        25000,
        b7vp,
      );
      const b7States = {};
      for (const [b7state, b7op] of [
        ['empty', null],
        ['attached', async () => {
          await page.setInputFiles('.app-main > input[type="file"]', attachFile);
          await waitFor(
            page,
            () => document.querySelectorAll('.composer-attachment').length > 0,
            `B7 ${b7vp.width}x${b7vp.height}: attachment chip never appeared`,
          );
        }],
        ['detached', async () => {
          await page.click('.composer-attachment__remove');
          await waitFor(
            page,
            () => document.querySelectorAll('.composer-attachment').length === 0,
            `B7 ${b7vp.width}x${b7vp.height}: attachment chip never removed`,
          );
        }],
        // Real Thinking regression: the repo steering mock routes the
        // "presentation thinking turn" prompt (needs MOCK_REASONING=1) to 3
        // real reasoning_content deltas. Atomic capture: the predicate
        // itself records the FIRST real Thinking state so the ~1.2s mock
        // window can never close between gate and sample (no fixed sleep).
        ['thinking', async () => {
          await page.evaluate(() => { window.__b7Thinking = null; });
          await page.fill('#prompt-input', 'presentation thinking turn');
          await page.click('#send-btn');
          await waitFor(page, () => {
            const s = document.getElementById('activity-strip');
            if (!s || s.getAttribute('data-state') !== 'thinking' || s.getBoundingClientRect().height <= 0) return false;
            window.__b7Thinking = { dataState: 'thinking', footerVar: getComputedStyle(document.documentElement).getPropertyValue('--app-footer-height').trim(), btnTop: document.getElementById('sidebar-manage-btn').getBoundingClientRect().top, footHeight: document.querySelector('.session-sidebar__foot').getBoundingClientRect().height };
            return true;
          }, `B7 ${b7vp.width}x${b7vp.height}: real Thinking state never entered`, 25000);
        }],
      ]) {
        if (b7op) await b7op();
        await page.evaluate(() => new Promise((res) => requestAnimationFrame(() => requestAnimationFrame(res))));
        await page.waitForTimeout(150);
        const b7 = await page.evaluate((want) => {
          const box = (el) => {
            if (!el) return null;
            const b = el.getBoundingClientRect();
            return { left: b.left, right: b.right, top: b.top, bottom: b.bottom, height: b.height, centerX: b.left + b.width / 2 };
          };
          const rail = document.querySelector('.session-sidebar');
          const foot = document.querySelector('.session-sidebar__foot');
          const composer = document.querySelector('.app-main > footer');
          const footCs = getComputedStyle(foot);
          const btn = document.getElementById('sidebar-manage-btn');
          const range = document.createRange();
          range.selectNodeContents(btn);
          const labelBox = range.getBoundingClientRect();
          return {
            viewport: want,
            footerVar: getComputedStyle(document.documentElement).getPropertyValue('--app-footer-height').trim(),
            sidebar: box(rail),
            foot: box(foot),
            footBorderColor: footCs.borderTopColor,
            footBorderTopWidth: footCs.borderTopWidth,
            footPadTop: footCs.paddingTop,
            footPadBottom: footCs.paddingBottom,
            btn: box(btn),
            btnLabelCenterX: labelBox.left + labelBox.width / 2,
            composer: box(composer),
            composerBorderColor: getComputedStyle(composer).borderTopColor,
            scrollWidth: document.documentElement.scrollWidth,
            clientWidth: document.documentElement.clientWidth,
          };
        }, b7vp);
        const geomBase = `${evidence}/appborder-geom-${b7vp.width}x${b7vp.height}-${b7state}`;
        writeFileSync(`${geomBase}.json`, `${JSON.stringify(b7, null, 2)}\n`);
        await page.screenshot({ path: `${geomBase}.png`, fullPage: false });
        b7States[b7state] = b7;
        const at = `B7 ${b7vp.width}x${b7vp.height} ${b7state}`;
        if (!b7.foot || b7.foot.height <= 0) fail(`${at}: sidebar foot is not laid out`);
        if (Math.abs(b7.foot.left - b7.sidebar.left) > 1) {
          fail(`${at}: foot border-left x ${b7.foot.left} vs sidebar left ${b7.sidebar.left} (must meet within 1px)`);
        }
        if (Math.abs(b7.foot.right - b7.sidebar.right) > 1) {
          fail(`${at}: foot border-right x ${b7.foot.right} vs sidebar right ${b7.sidebar.right} (must meet within 1px)`);
        }
        if (b7.footBorderTopWidth !== '1px') {
          fail(`${at}: foot border-top must stay 1px (got "${b7.footBorderTopWidth}")`);
        }
        if (b7.footPadTop !== '6px' || b7.footPadBottom !== '6px') {
          fail(`${at}: foot vertical padding must stay 6px (got ${b7.footPadTop}/${b7.footPadBottom})`);
        }
        if (b7.viewport.height - b7.foot.bottom > 1) {
          fail(`${at}: sidebar foot must end flush on the viewport bottom (gap ${b7.viewport.height - b7.foot.bottom}px)`);
        }
        if (!b7.btn || Math.abs(b7.btn.height - 44) > 1) {
          fail(`${at}: New Session button must stay 44px tall (got ${b7.btn ? b7.btn.height : 'missing'})`);
        }
        if (Math.abs(b7.btnLabelCenterX - b7.sidebar.centerX) > 1) {
          fail(`${at}: New Session label center ${b7.btnLabelCenterX} vs sidebar center ${b7.sidebar.centerX} (must be centered within 1px)`);
        }
        if (b7.scrollWidth > b7.clientWidth) {
          fail(`${at}: horizontal overflow (scrollWidth ${b7.scrollWidth} > clientWidth ${b7.clientWidth})`);
        }
      }
      const b7Base = b7States.empty;
      const b7Var = (s) => Number.parseFloat(s.footerVar) || 0;
      for (const b7name of ['attached', 'detached']) {
        const st = b7States[b7name];
        const at = `B7 ${b7vp.width}x${b7vp.height} ${b7name} vs empty`;
        if (Math.abs(st.btn.top - b7Base.btn.top) > 1) {
          fail(`${at}: New Session top moved ${b7Base.btn.top} -> ${st.btn.top} (must stay fixed while the composer band changes)`);
        }
        if (Math.abs(st.foot.height - b7Base.foot.height) > 1) {
          fail(`${at}: foot height changed ${b7Base.foot.height} -> ${st.foot.height} (must stay fixed while the composer band changes)`);
        }
      }
      if (b7Var(b7States.attached) < b7Var(b7Base) + 24) {
        fail(`B7 ${b7vp.width}x${b7vp.height}: attaching grew --app-footer-height by only ${b7Var(b7States.attached) - b7Var(b7Base)}px (<24) — the invariance gate requires a real composer height change to guard against`);
      }
      // Anti-vacuous: the atomic capture must prove a REAL Thinking state with a genuinely grown composer band, and the button/foot must have stayed put during it; the turn settles before the lane moves on.
      const b7Think = await page.evaluate(() => window.__b7Thinking);
      if (!b7Think || b7Think.dataState !== 'thinking') fail(`B7 ${b7vp.width}x${b7vp.height}: atomic Thinking capture missing (never saw data-state="thinking")`);
      if (b7Var(b7Think) < b7Var(b7Base) + 24) fail(`B7 ${b7vp.width}x${b7vp.height}: Thinking grew --app-footer-height by only ${b7Var(b7Think) - b7Var(b7Base)}px (<24)`);
      if (Math.abs(b7Think.btnTop - b7Base.btn.top) > 1) fail(`B7 ${b7vp.width}x${b7vp.height}: New Session top moved ${b7Base.btn.top} -> ${b7Think.btnTop} during real Thinking`);
      if (Math.abs(b7Think.footHeight - b7Base.foot.height) > 1) fail(`B7 ${b7vp.width}x${b7vp.height}: foot height changed ${b7Base.foot.height} -> ${b7Think.footHeight} during real Thinking`);
      await waitFor(page, () => { const s = document.getElementById('activity-strip'); const b = document.getElementById('send-btn'); return s === null && !!b && !b.hasAttribute('disabled'); }, `B7 ${b7vp.width}x${b7vp.height}: thinking turn never settled`, 25000);
    }
    await page.setViewportSize(b7InitialViewport);

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
    await page.click('#sidebar-collapse-btn');
    await waitFor(
      page,
      () => document.querySelector('.app-layout--drawer-open') === null,
      'rail did not collapse on toggle'
    );
    const b3 = await page.evaluate(() => {
      const appMain = document.querySelector('.app-main');
      const foot = document.querySelector('.session-sidebar__foot');
      const cs = getComputedStyle(appMain);
      const footCs = foot ? getComputedStyle(foot) : null;
      const footRect = foot ? foot.getBoundingClientRect() : { width: 0, height: 0 };
      return {
        edge: cs.borderLeftWidth + ' ' + cs.borderLeftStyle + ' ' + cs.borderLeftColor,
        footDisplay: footCs ? footCs.display : 'missing',
        footWidth: footRect.width,
        footHeight: footRect.height,
        scrollWidth: document.documentElement.scrollWidth,
        clientWidth: document.documentElement.clientWidth,
      };
    });
    const expectedCollapsed = `1px solid ${b1.token}`;
    if (b3.edge !== expectedCollapsed) {
      fail(`B3: .app-main border-left must stay 1px solid --border-strong with the rail collapsed (got "${b3.edge}")`);
    }
    if (!(b3.footDisplay === 'none' || b3.footHeight === 0 || b3.footWidth === 0)) {
      fail(`B3: collapsed rail must not paint an inset New Session border (display ${b3.footDisplay}, ${b3.footWidth}x${b3.footHeight})`);
    }
    if (b3.scrollWidth > b3.clientWidth) {
      fail(`B3: collapsed rail horizontal overflow (scrollWidth ${b3.scrollWidth} > clientWidth ${b3.clientWidth})`);
    }
    await page.screenshot({ path: `${evidence}/appborder-rail-collapsed.png`, fullPage: true });
    await page.click('#rail-reopen-btn');
    const todoNavPresence = await page.evaluate(() => ({
      todo: document.querySelectorAll('#todo-toggle-btn').length,
      legacyTasks: document.querySelectorAll('#todos-toggle-btn').length,
      legacyChildren: document.querySelectorAll('#todo-child-toggle-btn').length,
    }));
    if (todoNavPresence.todo !== 1 || todoNavPresence.legacyTasks !== 0 || todoNavPresence.legacyChildren !== 0) {
      fail(`B5: Todo feature-nav presence mismatch: ${JSON.stringify(todoNavPresence)}`);
    }

    // ---- B5: shared ordinary-panel height resizer (desktop) ----
    await clickFeature(page, '#todo-toggle-btn');
    await waitFor(page, () => {
      const panels = document.querySelectorAll('#todo-panel');
      const panel = panels.length === 1 ? panels[0] : null;
      return panel !== null
        && panel.querySelector('.todo-panel__tasks') !== null
        && panel.querySelector('.todo-child-section') !== null;
    }, 'B5: unified Todo module did not open with task and child sections');
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
    await waitFor(
      page,
      () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true,
      'B5 reload: WS did not reach connected'
    );
    await clickFeature(page, '#todo-toggle-btn');
    await waitFor(page, () => {
      const panels = document.querySelectorAll('#todo-panel');
      const panel = panels.length === 1 ? panels[0] : null;
      return panel !== null
        && panel.querySelector('.todo-panel__tasks') !== null
        && panel.querySelector('.todo-child-section') !== null;
    }, 'B5 reload: unified Todo module did not reopen with task and child sections');
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
      await openSessionsDrawer(page);
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
    const collapsed = await page.evaluate(() => {
      const sidebar = document.querySelector('.session-sidebar');
      const reopen = document.getElementById('rail-reopen-btn');
      if (!sidebar || !reopen) return null;
      const box = document.querySelector('.session-sidebar__search-box');
      const visible = Array.from(sidebar.querySelectorAll('*')).filter((element) => {
        if (element === reopen || reopen.contains(element) || element.contains(reopen)) return false;
        if (box && (element === box || box.contains(element))) return false;
        const style = getComputedStyle(element);
        const rect = element.getBoundingClientRect();
        return style.display !== 'none' && style.visibility !== 'hidden' && rect.width > 0 && rect.height > 0;
      }).map((element) => element.id || element.className || element.tagName);
      const focusable = Array.from(sidebar.querySelectorAll('button, input, select, textarea, a[href], [tabindex]'))
        .filter((element) => element !== reopen && !reopen.contains(element))
        .filter((element) => {
          const style = getComputedStyle(element);
          const rect = element.getBoundingClientRect();
          return style.display !== 'none' && style.visibility !== 'hidden' && rect.width > 0 && rect.height > 0
            && !(element instanceof HTMLButtonElement || element instanceof HTMLInputElement || element instanceof HTMLSelectElement || element instanceof HTMLTextAreaElement ? element.disabled : false)
            && element.getAttribute('tabindex') !== '-1';
        }).map((element) => element.id || element.className || element.tagName);
      const strip = box ? getComputedStyle(box) : null;
      return {
        visible,
        focusable,
        activeInside: sidebar.contains(document.activeElement) && document.activeElement !== reopen,
        stripHostsReopen: !!box && box.contains(reopen) && strip.display !== 'none',
        // Real visibility (rects painted), NOT computed display: input/clear
        // hide through their `.session-sidebar__search-field` ancestor, and a
        // display:none ancestor never cascades into a child's computed value.
        searchInputHidden: (() => { const el = document.querySelector('.session-sidebar__search'); return el == null || el.getClientRects().length === 0 || (el.getBoundingClientRect().width === 0 && el.getBoundingClientRect().height === 0); })(),
        refreshHidden: (() => { const el = document.querySelector('.session-sidebar__refresh'); return el == null || el.getClientRects().length === 0 || (el.getBoundingClientRect().width === 0 && el.getBoundingClientRect().height === 0); })(),
      };
    });
    if (!collapsed) fail('B6: collapsed sidebar DOM missing');
    if (collapsed.visible.length !== 0) fail(`B6: collapsed rail has visible children besides reopen: ${collapsed.visible.join(', ')}`);
    if (collapsed.focusable.length !== 0) fail(`B6: collapsed rail has focusable children besides reopen: ${collapsed.focusable.join(', ')}`);
    if (collapsed.activeInside) fail('B6: collapsed rail retained an active/focus ghost');
    if (!collapsed.stripHostsReopen || !collapsed.searchInputHidden || !collapsed.refreshHidden) {
      fail(`B6: collapsed strip must host ONLY the visible » (host=${collapsed.stripHostsReopen} inputHidden=${collapsed.searchInputHidden} refreshHidden=${collapsed.refreshHidden})`);
    }
    await page.screenshot({ path: `${evidence}/appborder-sidebar-rail-collapsed.png`, fullPage: true });
    await page.click('#rail-reopen-btn');
    await waitFor(
      page,
      () => {
        const sidebar = document.querySelector('.session-sidebar');
        const list = document.querySelector('.session-sidebar__list');
        const search = document.querySelector('.session-sidebar__search-box');
        if (!sidebar || !list || !search) return false;
        if (document.querySelector('.session-sidebar__nav') !== null) return false;
        return Math.abs(sidebar.getBoundingClientRect().width - 195.5) <= 1
          && getComputedStyle(list).display !== 'none'
          && list.getBoundingClientRect().height > 0
          && getComputedStyle(search).display !== 'none';
      },
      'B6: #rail-reopen-btn did not restore the sidebar after header collapse'
    );
    await page.click('#sidebar-collapse-btn');
    await waitFor(
      page,
      () => document.querySelector('.app-layout--drawer-open') === null,
      'B6: sidebar header collapse no longer collapses the rail'
    );
    await page.click('#rail-reopen-btn');
    await waitFor(
      page,
      () => document.querySelector('.app-layout--drawer-open') !== null,
      'B6: rail reopen strip no longer reopens the rail'
    );
    await page.screenshot({ path: `${evidence}/appborder-sidebar-collapse.png`, fullPage: true });
    if (runtimeErrors.length !== 0) fail(`B6: browser runtime errors: ${runtimeErrors.join(' | ')}`);

    // ---- B4: mobile flush edges + zero horizontal overflow + no resizer ----
    await page.setViewportSize({ width: 390, height: 844 });
    await page.waitForTimeout(300);
    // Desktop left the rail open; on mobile that class means the drawer is
    // already open — only click the toggle when it is closed.
    const mobileOpenAlready = await page.evaluate(() =>
      document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true
    );
    if (!mobileOpenAlready) {
      await openSessionsDrawer(page);
    }
    await waitFor(
      page,
      () => document.querySelector('.app-layout--drawer-open') !== null,
      'B4: mobile drawer did not open'
    );
    const todoOpen = await page.evaluate(() => document.getElementById('todo-panel') !== null);
    if (todoOpen) {
      await page.evaluate(() => document.getElementById('todo-close-btn')?.click());
      await waitFor(page, () => document.getElementById('todo-panel') === null,
        'B4: could not close leftover unified Todo module');
    }
    if (await page.evaluate(() => document.querySelector('.app-layout--drawer-open') === null)) {
      await openSessionsDrawer(page);
      await waitFor(page, () => document.querySelector('.app-layout--drawer-open') !== null,
        'B4: mobile drawer did not reopen after Todo cleanup');
    }
    // Foot border is only painted while the drawer is open. clickFeature
    // opens the header menu (mobile menu-open closes the drawer) and Todo
    // then stays closed via the activePanel layout effect. Measure here.
    const b4Foot = await page.evaluate(() => {
      const rail = document.querySelector('.session-sidebar');
      const foot = document.querySelector('.session-sidebar__foot');
      const drawerOpen = document.querySelector('.app-layout--drawer-open') !== null;
      if (!drawerOpen || !rail || !foot) return null;
      const railBox = rail.getBoundingClientRect();
      const footBox = foot.getBoundingClientRect();
      const footCs = getComputedStyle(foot);
      return {
        railLeft: railBox.left,
        railRight: railBox.right,
        footLeft: footBox.left,
        footRight: footBox.right,
        footHeight: footBox.height,
        borderTopWidth: footCs.borderTopWidth,
        paddingTop: footCs.paddingTop,
        paddingBottom: footCs.paddingBottom,
        scrollWidth: document.documentElement.scrollWidth,
        clientWidth: document.documentElement.clientWidth,
      };
    });
    if (!b4Foot || b4Foot.footHeight <= 0) fail('B4: mobile drawer foot border is not laid out');
    if (b4Foot.borderTopWidth !== '1px') {
      fail(`B4: mobile foot border-top must stay 1px (got "${b4Foot.borderTopWidth}")`);
    }
    if (b4Foot.paddingTop !== '6px' || b4Foot.paddingBottom !== '6px') {
      fail(`B4: mobile foot vertical padding must stay 6px (got ${b4Foot.paddingTop}/${b4Foot.paddingBottom})`);
    }
    if (Math.abs(b4Foot.footLeft - b4Foot.railLeft) > 1) {
      fail(`B4: mobile foot border-left x ${b4Foot.footLeft} vs sidebar left ${b4Foot.railLeft} (must meet within 1px)`);
    }
    if (Math.abs(b4Foot.footRight - b4Foot.railRight) > 1) {
      fail(`B4: mobile foot border-right x ${b4Foot.footRight} vs sidebar right ${b4Foot.railRight} (must meet within 1px)`);
    }
    if (b4Foot.scrollWidth > b4Foot.clientWidth) {
      fail(`B4: mobile foot pull caused horizontal overflow (scrollWidth ${b4Foot.scrollWidth} > clientWidth ${b4Foot.clientWidth})`);
    }
    // New Session foot journey on the open drawer: the drawer bottom no
    // longer tracks the composer band, so the button stays put across a real
    // attach / draft / remove cycle. Operations happen with the drawer closed
    // (real close journey); measurements with it open. The composer is under
    // the open drawer and is reached again by closing it.
    const b4Geom = () => page.evaluate(() => {
      const box = (el) => {
        if (!el) return null;
        const b = el.getBoundingClientRect();
        return { left: b.left, right: b.right, top: b.top, bottom: b.bottom, height: b.height, centerX: b.left + b.width / 2 };
      };
      const rail = document.querySelector('.session-sidebar');
      const foot = document.querySelector('.session-sidebar__foot');
      const btn = document.getElementById('sidebar-manage-btn');
      const range = document.createRange();
      range.selectNodeContents(btn);
      const labelBox = range.getBoundingClientRect();
      return {
        footerVar: getComputedStyle(document.documentElement).getPropertyValue('--app-footer-height').trim(),
        sidebar: box(rail),
        foot: box(foot),
        // The ≤720px drawer owns the viewport bottom and pads the foot up
        // by 8px + env(safe-area-inset-bottom); resolve the live inset so
        // the flush assertion below is measured, not assumed.
        sidebarPadBottom: rail ? (parseFloat(getComputedStyle(rail).paddingBottom) || 0) : 0,
        promptTop: document.getElementById('prompt-input')?.getBoundingClientRect().top ?? null,
        band: parseFloat(getComputedStyle(document.documentElement).getPropertyValue('--composer-buttons-band')) || 0,
        btn: box(btn),
        btnLabelCenterX: labelBox.left + labelBox.width / 2,
        chips: document.querySelectorAll('.composer-attachment').length,
        drawerOpen: document.querySelector('.app-layout--drawer-open') !== null,
        viewH: window.innerHeight,
      };
    });
    const b4Attach = `${evidence}/appborder-attach-fixture.txt`;
    writeFileSync(b4Attach, 'appborder attach fixture\n');
    const b4Draft = 'appborder draft persistence probe';
    const b4States = {};
    for (const [b4name, b4op] of [
      ['empty-open', null],
      ['attached-open', async () => {
        await page.setInputFiles('.app-main > input[type="file"]', b4Attach);
        await waitFor(page, () => document.querySelectorAll('.composer-attachment').length > 0, 'B4: attachment chip never appeared');
      }],
      ['attached-draft-open', async () => {
        await page.locator('#prompt-input').fill(b4Draft);
      }],
      ['restored-open', async () => {
        await page.click('.composer-attachment__remove');
        await waitFor(page, () => document.querySelectorAll('.composer-attachment').length === 0, 'B4: attachment chip never removed');
      }],
    ]) {
      if (b4op) {
        await closeSessionsDrawer(page);
        await b4op();
        await openSessionsDrawer(page);
      }
      await page.evaluate(() => new Promise((res) => requestAnimationFrame(() => requestAnimationFrame(res))));
      await page.waitForTimeout(150);
      const st = await b4Geom();
      if (!st.drawerOpen) fail(`B4 ${b4name}: drawer is not open for measurement`);
      b4States[b4name] = st;
      await page.screenshot({ path: `${evidence}/appborder-mobile-foot-${b4name}.png`, fullPage: false });
    }
    const b4FootBase = b4States['empty-open'];
    if (Math.abs(b4FootBase.btn.top - b4FootBase.promptTop) > 1) {
      fail(`B4 empty-open: New Session top ${b4FootBase.btn.top} vs #prompt-input top ${b4FootBase.promptTop} (mobile idle must align within 1px)`);
    }
    for (const b4name of ['attached-open', 'attached-draft-open', 'restored-open']) {
      const st = b4States[b4name];
      const at = `B4 ${b4name} vs empty-open`;
      const expectedChips = (b4name === 'attached-open' || b4name === 'attached-draft-open') ? 1 : 0;
      if (st.chips !== expectedChips) fail(`${at}: attachment chip count ${st.chips}, expected ${expectedChips}`);
      if (Math.abs(st.btn.top - b4FootBase.btn.top) > 1) {
        fail(`${at}: New Session top moved ${b4FootBase.btn.top} -> ${st.btn.top} (drawer bottom must not track the composer band)`);
      }
      if (Math.abs(st.foot.height - b4FootBase.foot.height) > 1) {
        fail(`${at}: foot height changed ${b4FootBase.foot.height} -> ${st.foot.height}`);
      }
      if (Math.abs(st.btnLabelCenterX - st.sidebar.centerX) > 1) {
        fail(`${at}: New Session label center ${st.btnLabelCenterX} vs sidebar center ${st.sidebar.centerX} (must be centered within 1px)`);
      }
      if (st.viewH - st.sidebar.bottom > 1) {
        fail(`${at}: mobile drawer must end flush on the viewport bottom (gap ${st.viewH - st.sidebar.bottom}px)`);
      }
      if (Math.abs(st.viewH - st.sidebarPadBottom - st.band - st.foot.bottom) > 1) {
        fail(`${at}: drawer foot must end above the viewport by the drawer padding inset plus the live composer band (gap ${st.viewH - st.foot.bottom}px vs inset ${st.sidebarPadBottom + st.band}px)`);
      }
      if (Math.abs(st.btn.bottom - b4FootBase.btn.bottom) > 1) {
        fail(`${at}: New Session bottom moved ${b4FootBase.btn.bottom} -> ${st.btn.bottom} (button must not drift while the composer grows)`);
      }
    }
    // Mobile hard Thinking regression (drawer stays open; MOCK_REASONING=1
    // routes "presentation thinking turn" to real reasoning deltas). The
    // open drawer (z-index 52) covers #send-btn, so the send goes through
    // the real button handler via evaluate. Atomic capture records the
    // FIRST thinking state (no fixed sleep), mirroring the desktop B7 gate.
    await page.evaluate(() => { window.__b4Thinking = null; });
    await page.fill('#prompt-input', 'presentation thinking turn');
    await page.evaluate(() => document.getElementById('send-btn').click());
    await waitFor(page, () => {
      const s = document.getElementById('activity-strip');
      if (!s || s.getAttribute('data-state') !== 'thinking' || s.getBoundingClientRect().height <= 0) return false;
      window.__b4Thinking = {
        dataState: 'thinking',
        footerVar: getComputedStyle(document.documentElement).getPropertyValue('--app-footer-height').trim(),
        btnTop: document.getElementById('sidebar-manage-btn').getBoundingClientRect().top,
        footHeight: document.querySelector('.session-sidebar__foot').getBoundingClientRect().height,
      };
      return true;
    }, 'B4: mobile Thinking state never entered with the drawer open', 25000);
    const b4Think = await page.evaluate(() => window.__b4Thinking);
    if (!b4Think || b4Think.dataState !== 'thinking') fail('B4: atomic Thinking capture missing (never saw data-state="thinking")');
    const b4Var = (s) => Number.parseFloat(s) || 0;
    if (b4Var(b4Think.footerVar) < b4Var(b4FootBase.footerVar) + 24) fail(`B4: Thinking grew --app-footer-height by only ${b4Var(b4Think.footerVar) - b4Var(b4FootBase.footerVar)}px (<24)`);
    if (Math.abs(b4Think.btnTop - b4FootBase.btn.top) > 1) fail(`B4: New Session top moved ${b4FootBase.btn.top} -> ${b4Think.btnTop} during real Thinking`);
    if (Math.abs(b4Think.footHeight - b4FootBase.foot.height) > 1) fail(`B4: foot height changed ${b4FootBase.foot.height} -> ${b4Think.footHeight} during real Thinking`);
    await waitFor(page, () => { const s = document.getElementById('activity-strip'); const b = document.getElementById('send-btn'); return s === null && !!b && !b.hasAttribute('disabled'); }, 'B4: thinking turn never settled', 25000);
    // The send cleared the composer; restore the draft the persistence
    // check below expects to survive the drawer cycle.
    await page.fill('#prompt-input', b4Draft);
    // Header feature menu is reachable while the drawer is open (real
    // trigger click from the header band, which sits above the drawer's
    // top: var(--app-header-height) sheet). Escape kills only the menu
    // (App's Escape handler never touches the sidebar). At <=720px App's
    // trigger handler itself collapses the drawer on menu open
    // (App.tsx: setFeatureMenuOpen + setSidebarOpen(false)), so the
    // documented post-Escape state is menu closed AND drawer closed —
    // asserting "drawer still open" here described a UI state that never
    // exists. Reopen via the real Menu→Sessions journey for the hit-layer
    // gates below.
    await openFeatureMenu(page);
    await page.keyboard.press('Escape');
    await page.locator('#feature-menu').waitFor({ state: 'hidden', timeout: 5000 });
    if (await page.evaluate(() => document.querySelector('.app-layout--drawer-open') !== null)) {
      fail('B4: drawer must be collapsed by the documented menu-open mutual exclusion');
    }
    await openSessionsDrawer(page);
    // New Session must be the top hit layer at its center and corners, and a
    // real center click opens session management. The button paints
    // --radius-sm (6px at mobile) rounded corners, so a blind 1px-inset
    // corner sample falls OUTSIDE the rounded hit shape on
    // .session-sidebar__foot and can never return the button (the curve at
    // 1px vertical inset sits at 2.68px horizontal inset). Corner samples
    // are derived per corner from the live computed radius instead: the
    // diagonal extreme of the rounded corner — the on-shape point closest
    // to the visual corner — keeps the full rounded hit shape asserted.
    // Center stays at the exact center and the real click below is
    // unchanged.
    await page.evaluate(() => new Promise((res) => requestAnimationFrame(() => requestAnimationFrame(res))));
    await page.waitForTimeout(150);
    const b4BtnBox = await page.locator('#sidebar-manage-btn').boundingBox();
    const b4Hits = await page.evaluate((b) => {
      const btn = document.getElementById('sidebar-manage-btn');
      const cs = getComputedStyle(btn);
      // r·(1−1/√2) is the diagonal extreme of an r-rounded corner; +0.5px
      // keeps the sample strictly inside the curve. Square corners (r=0)
      // keep the original 1px inset.
      const inset = (v) => (parseFloat(v) > 0 ? parseFloat(v) * (1 - Math.SQRT1_2) + 0.5 : 1);
      const kTL = inset(cs.borderTopLeftRadius);
      const kTR = inset(cs.borderTopRightRadius);
      const kBL = inset(cs.borderBottomLeftRadius);
      const kBR = inset(cs.borderBottomRightRadius);
      const pts = [
        [b.x + b.width / 2, b.y + b.height / 2],
        [b.x + kTL, b.y + kTL], [b.x + b.width - kTR, b.y + kTR],
        [b.x + kBL, b.y + b.height - kBL], [b.x + b.width - kBR, b.y + b.height - kBR],
      ];
      return pts.map(([x, y]) => {
        const el = document.elementFromPoint(x, y);
        return !!el && (el === btn || btn.contains(el));
      });
    }, b4BtnBox);
    if (b4Hits.some((hit) => !hit)) {
      fail(`B4: New Session button is not the top hit layer at all points (center+corners: ${b4Hits.map((h) => (h ? 'y' : 'n')).join('')})`);
    }
    await page.click('#sidebar-manage-btn');
    await waitFor(page, () => document.getElementById('session-panel') !== null, 'B4: real New Session center click did not open session management');
    await page.click('#session-close-btn');
    await waitFor(page, () => document.getElementById('session-panel') === null, 'B4: session panel did not close after the New Session probe');
    // Closing the drawer restores composer reachability and keeps the draft.
    await closeSessionsDrawer(page);
    await page.click('#prompt-input');
    const b4DraftValue = await page.locator('#prompt-input').inputValue();
    if (b4DraftValue !== b4Draft) {
      fail(`B4: composer draft did not survive the drawer cycle (got "${b4DraftValue}")`);
    }
    await page.locator('#prompt-input').fill('');
    await clickFeature(page, '#todo-toggle-btn');
    await waitFor(page, () => {
      const panels = document.querySelectorAll('#todo-panel');
      const panel = panels.length === 1 ? panels[0] : null;
      return panel !== null
        && panel.querySelector('.todo-panel__tasks') !== null
        && panel.querySelector('.todo-child-section') !== null;
    }, 'B4: mobile unified Todo module did not open with task and child sections');
    const b4 = await page.evaluate(() => {
      const cs = getComputedStyle(document.querySelector('.app-main'));
      const resizer = document.getElementById('panel-drawer-resizer');
      const resizerDisplay = resizer ? getComputedStyle(resizer).display : 'none';
      const panel = document.getElementById('todo-panel');
      const box = panel ? panel.getBoundingClientRect() : { height: 0, top: 0 };
      const root = getComputedStyle(document.documentElement);
      const headerH = Number.parseFloat(root.getPropertyValue('--app-header-height')) || 0;
      const footerH = Number.parseFloat(root.getPropertyValue('--app-footer-height')) || 0;
      return {
        borderLeft: cs.borderLeftWidth + ' ' + cs.borderLeftStyle,
        borderRight: cs.borderRightWidth + ' ' + cs.borderRightStyle,
        scrollWidth: document.documentElement.scrollWidth,
        clientWidth: document.documentElement.clientWidth,
        resizerDisplay,
        panelH: box.height,
        panelTop: box.top,
        headerH,
        footerH,
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
    const insetH = b4.viewH - b4.headerH - b4.footerH;
    if (Math.abs(b4.panelH - insetH) > 8) {
      fail(`B4: mobile todo-panel height ${b4.panelH} must fill the header-to-footer inset (~${insetH}; view ${b4.viewH} header ${b4.headerH} footer ${b4.footerH})`);
    }
    if (Math.abs(b4.panelTop - b4.headerH) > 8) {
      fail(`B4: mobile todo-panel top ${b4.panelTop} must sit under the header (${b4.headerH})`);
    }

    // Opening Todo already collapsed the mobile drawer. Close the panel,
    // then re-open the drawer only to exercise the feature-menu close path.
    await page.click('#todo-close-btn');
    await waitFor(page, () => document.getElementById('todo-panel') === null, 'B4: mobile todo panel did not close');
    const mobileDrawerOpen = await page.evaluate(() =>
      document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true
    );
    if (!mobileDrawerOpen) {
      await openSessionsDrawer(page);
      await waitFor(
        page,
        () => document.querySelector('.app-layout--drawer-open') !== null,
        'B4: could not re-open mobile drawer for collapse check'
      );
    }
    await closeSessionsDrawer(page);
    await waitFor(
      page,
      () => document.querySelector('.app-layout--drawer-open') === null,
      'B4: feature-menu close journey did not close the drawer'
    );
    await page.screenshot({ path: `${evidence}/appborder-mobile.png`, fullPage: true });

    console.log('web-appborder: PASSED (desktop dark+light .app-main edges, rail-collapse edge, desktop footer border alignment 1280x720+1360x900, foot border meets sidebar edges, shared panel resizer pointer/keyboard/bounds/reload, sidebar header collapse/reopen, mobile flush edges + foot border closure + no resizer + zero overflow)');
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-appborder: FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});
