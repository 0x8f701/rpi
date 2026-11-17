// Web collapsed-rail reopen hit-target lane (playwright half of
// E2E.d/web/sidebar_rail.sh).
//
// Environment:
//   RPI_URL          http://127.0.0.1:<port>/web
//   RPI_TOKEN        token file content (served via rpi-auth.<token> subprotocol)
//   RPI_CHROME       executable path of the system Chrome (optional)
//   RPI_EVIDENCE     evidence dir for screenshots
//
// Asserts the user-visible collapsed-rail contract with REAL clicks (no
// locator.force / dispatchEvent):
//   R1  desktop 1280x800 starts with app-layout--drawer-open
//   R2  real click #sidebar-collapse-btn -> 44px rail + visible #rail-reopen-btn
//   R3  real click #rail-reopen-btn -> drawer-open + sidebar content restored
//   R4  collapse/reopen cycle repeats
//   R5  keyboard Enter then Space on the focused reopen button restore the rail
//   R6  mobile 390x844: explicitly close the leftover desktop-open drawer,
//       then open through the header feature menu (open ☰ → REAL-click
//       #sidebar-menu-item, must not skip), assert drawer/sidebar visible and
//       #rail-reopen-btn hidden, then close through the feature menu again.
//       A no-op menu handler fails the open wait.
//
// Pre-fix CSS fails R3/R4/R5: pointer-events:none on .session-sidebar__search-box

import { chromium } from 'playwright';
import { openSessionsDrawer, closeSessionsDrawer } from './lib/feature-menu.mjs';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';

function fail(message) {
  console.error(`web-sidebar-rail: FAIL: ${message}`);
  process.exit(2);
}

async function waitFor(page, fn, label, timeoutMs = 25000) {
  try {
    await page.waitForFunction(fn, { timeout: timeoutMs });
  } catch (error) {
    fail(`${label} (timeout ${timeoutMs}ms)`);
  }
}

async function waitBooted(page, label) {
  await waitFor(page, () => document.title === 'rpi web', `${label}: page title missing`);
  await waitFor(
    page,
    () => document.getElementById('session-sidebar') !== null
      && document.getElementById('sidebar-collapse-btn') !== null
      && document.getElementById('rail-reopen-btn') !== null
      && document.getElementById('feature-menu-btn') !== null,
    `${label}: session sidebar chrome did not mount`,
  );
}
function isCollapsedRail() {
  const layout = document.querySelector('.app-layout');
  const reopen = document.getElementById('rail-reopen-btn');
  const sidebar = document.querySelector('.session-sidebar');
  if (!layout || !reopen || !sidebar) return false;
  if (layout.classList.contains('app-layout--drawer-open')) return false;
  const style = getComputedStyle(reopen);
  if (style.display === 'none' || style.visibility === 'hidden') return false;
  const width = sidebar.getBoundingClientRect().width;
  return width >= 40 && width <= 48;
}

function isOpenRail() {
  const layout = document.querySelector('.app-layout');
  const sidebar = document.querySelector('.session-sidebar');
  const list = document.querySelector('.session-sidebar__list');
  const search = document.querySelector('.session-sidebar__search-box');
  const reopen = document.getElementById('rail-reopen-btn');
  if (!layout || !sidebar) return false;
  if (!layout.classList.contains('app-layout--drawer-open')) return false;
  if (Math.abs(sidebar.getBoundingClientRect().width - 195.5) > 1) return false;
  for (const el of [search, list]) {
    if (!el) return false;
    const style = getComputedStyle(el);
    if (style.display === 'none' || style.visibility === 'hidden') return false;
    if (el.getBoundingClientRect().height <= 0) return false;
  }
  if (reopen && getComputedStyle(reopen).display !== 'none') return false;
  return true;
}

function isMobileClosed() {
  const layout = document.querySelector('.app-layout');
  const sidebar = document.querySelector('.session-sidebar');
  const trigger = document.getElementById('feature-menu-btn');
  if (!layout || !sidebar || !trigger) return false;
  if (window.innerWidth >= 721) return false;
  if (layout.classList.contains('app-layout--drawer-open')) return false;
  if (getComputedStyle(trigger).display === 'none') return false;
  return getComputedStyle(sidebar).display === 'none';
}

function isMobileOpen() {
  const layout = document.querySelector('.app-layout');
  const sidebar = document.querySelector('.session-sidebar');
  const reopen = document.getElementById('rail-reopen-btn');
  const trigger = document.getElementById('feature-menu-btn');
  if (!layout || !sidebar || !trigger) return false;
  if (window.innerWidth >= 721) return false;
  if (!layout.classList.contains('app-layout--drawer-open')) return false;
  if (getComputedStyle(trigger).display === 'none') return false;
  const sidebarStyle = getComputedStyle(sidebar);
  if (sidebarStyle.display === 'none' || sidebarStyle.visibility === 'hidden') return false;
  const box = sidebar.getBoundingClientRect();
  if (box.height < 80 || box.width < 200) return false;
  if (reopen && getComputedStyle(reopen).display !== 'none') return false;
  return true;
}

async function collapseViaHeader(page, label) {
  await page.locator('#sidebar-collapse-btn').click({ force: false, timeout: 10000 });
  await waitFor(page, isCollapsedRail, `${label}: collapse did not produce a 44px rail with visible #rail-reopen-btn`);
}

async function reopenViaButton(page, label) {
  const btn = page.locator('#rail-reopen-btn');
  await btn.click({ force: false, timeout: 10000 });
  await waitFor(page, isOpenRail, `${label}: real click on #rail-reopen-btn did not restore the drawer`);
}

async function main() {
  if (!url) fail('RPI_URL is required');
  const launchOptions = {
    args: ['--no-sandbox'],
    ...(chromePath ? { executablePath: chromePath } : {}),
  };
  const browser = await chromium.launch(launchOptions);
  try {
    const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
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
    await waitBooted(page, 'R1');
    const desktopOpen = await page.evaluate(isOpenRail);
    if (!desktopOpen) fail('R1: desktop >720px must start with an open session rail');
    await page.screenshot({ path: `${evidence}/sidebar-rail-open.png`, fullPage: true });

    await collapseViaHeader(page, 'R2');
    const collapsed = await page.evaluate(() => {
      const sidebar = document.querySelector('.session-sidebar');
      const reopen = document.getElementById('rail-reopen-btn');
      const collapse = document.getElementById('sidebar-collapse-btn');
      const searchBox = document.querySelector('.session-sidebar__search-box');
      // Real visibility (boxes painted), NOT computed display: the search
      // input/clear hide through their `.session-sidebar__search-field`
      // ancestor (display:none does not cascade into a child's computed
      // display), so only rect/clientRects prove invisibility.
      const noBoxes = (el) => el == null || el.getClientRects().length === 0
        || (el.getBoundingClientRect().width === 0 && el.getBoundingClientRect().height === 0);
      const searchInput = document.querySelector('.session-sidebar__search');
      const searchClear = document.querySelector('.session-sidebar__search-clear');
      const refresh = document.querySelector('.session-sidebar__refresh');
      return {
        width: sidebar ? sidebar.getBoundingClientRect().width : -1,
        reopenDisplay: reopen ? getComputedStyle(reopen).display : 'missing',
        reopenEvents: reopen ? getComputedStyle(reopen).pointerEvents : 'missing',
        collapseDisplay: collapse ? getComputedStyle(collapse).display : 'missing',
        searchBoxDisplay: searchBox ? getComputedStyle(searchBox).display : 'missing',
        searchBoxEvents: searchBox ? getComputedStyle(searchBox).pointerEvents : 'missing',
        searchInputHidden: noBoxes(searchInput),
        searchClearHidden: noBoxes(searchClear),
        refreshHidden: noBoxes(refresh),
      };
    });
    if (!(collapsed.width >= 40 && collapsed.width <= 48)) {
      fail(`R2: collapsed rail width must be ~44px (got ${collapsed.width})`);
    }
    if (collapsed.reopenDisplay === 'none' || collapsed.reopenDisplay === 'missing') {
      fail(`R2: #rail-reopen-btn must be visible (display=${collapsed.reopenDisplay})`);
    }
    if (collapsed.reopenEvents === 'none' || collapsed.searchBoxEvents === 'none') {
      fail(`R2: reopen hit chain still pointer-events:none (btn=${collapsed.reopenEvents} box=${collapsed.searchBoxEvents}) — pre-fix CSS fails here`);
    }
    if (collapsed.collapseDisplay !== 'none') {
      fail(`R2: hidden collapse control must stay display:none (got ${collapsed.collapseDisplay})`);
    }
    if (collapsed.searchBoxDisplay === 'none' || collapsed.searchBoxDisplay === 'missing') {
      fail(`R2: the collapsed strip must keep the search-box row visible as the » host (display=${collapsed.searchBoxDisplay})`);
    }
    if (!collapsed.searchInputHidden || !collapsed.searchClearHidden || !collapsed.refreshHidden) {
      fail(`R2: collapsed strip must hide the search input/clear/refresh (rect-based: input=${collapsed.searchInputHidden} clear=${collapsed.searchClearHidden} refresh=${collapsed.refreshHidden})`);
    }
    await page.screenshot({ path: `${evidence}/sidebar-rail-collapsed.png`, fullPage: true });

    await reopenViaButton(page, 'R3');
    await page.screenshot({ path: `${evidence}/sidebar-rail-reopened.png`, fullPage: true });

    await collapseViaHeader(page, 'R4-collapse');
    await reopenViaButton(page, 'R4-reopen');

    await collapseViaHeader(page, 'R5-enter-setup');
    await page.locator('#rail-reopen-btn').focus();
    await page.keyboard.press('Enter');
    await waitFor(page, isOpenRail, 'R5: Enter on focused #rail-reopen-btn did not restore the drawer');

    await collapseViaHeader(page, 'R5-space-setup');
    await page.locator('#rail-reopen-btn').focus();
    await page.keyboard.press('Space');
    await waitFor(page, isOpenRail, 'R5: Space on focused #rail-reopen-btn did not restore the drawer');
    await page.screenshot({ path: `${evidence}/sidebar-rail-keyboard.png`, fullPage: true });

    await page.setViewportSize({ width: 390, height: 844 });
    await waitFor(
      page,
      () => window.innerWidth === 390 && window.innerHeight === 844,
      'R6: viewport did not settle at 390x844',
    );
    // The app's matchMedia listener auto-collapses the drawer when the
    // viewport enters the ≤720px branch. Wait for that product transition
    // instead of clicking a leftover-open drawer: the click races the
    // listener (see open -> effect close -> click re-opens).
    await waitFor(page, isMobileClosed, 'R6: mobile drawer did not auto-close after the viewport change');
    await openSessionsDrawer(page);
    await waitFor(
      page,
      isMobileOpen,
      'R6: real menu journey (#sidebar-menu-item) did not OPEN the mobile drawer (a no-op menu handler or leftover-open skip would fail here)',
    );
    const mobileOpen = await page.evaluate(() => {
      const sidebar = document.querySelector('.session-sidebar');
      const reopen = document.getElementById('rail-reopen-btn');
      const trigger = document.getElementById('feature-menu-btn');
      return {
        drawer: document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true,
        sidebarDisplay: sidebar ? getComputedStyle(sidebar).display : 'missing',
        sidebarHeight: sidebar ? sidebar.getBoundingClientRect().height : -1,
        reopenDisplay: reopen ? getComputedStyle(reopen).display : 'missing',
        triggerDisplay: trigger ? getComputedStyle(trigger).display : 'missing',
      };
    });
    if (!mobileOpen.drawer) fail('R6: mobile drawer class missing after open click');
    if (mobileOpen.sidebarDisplay === 'none' || mobileOpen.sidebarHeight <= 0) {
      fail(`R6: mobile sidebar must be visible after open click (display=${mobileOpen.sidebarDisplay} height=${mobileOpen.sidebarHeight})`);
    }
    if (mobileOpen.reopenDisplay !== 'none') {
      fail(`R6: #rail-reopen-btn must stay hidden on <720px (display=${mobileOpen.reopenDisplay})`);
    }
    if (mobileOpen.triggerDisplay === 'none') {
      fail('R6: #feature-menu-btn must remain the mobile drawer control');
    }
    await page.screenshot({ path: `${evidence}/sidebar-rail-mobile-open.png`, fullPage: true });
    await closeSessionsDrawer(page);
    await waitFor(page, isMobileClosed, 'R6: feature-menu close journey did not CLOSE the mobile drawer');
    await page.screenshot({ path: `${evidence}/sidebar-rail-mobile-closed.png`, fullPage: true });

    if (runtimeErrors.length !== 0) fail(`R7: browser runtime errors: ${runtimeErrors.join(' | ')}`);
    console.log('web-sidebar-rail: PASS');
  } finally {
    await browser.close();
  }
}

main().catch((err) => {
  console.error(`web-sidebar-rail: FAIL: ${err?.stack || err}`);
  process.exit(2);
});
