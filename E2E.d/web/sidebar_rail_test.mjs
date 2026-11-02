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
//       then REAL-click #sidebar-toggle-btn to OPEN (must not skip), assert
//       drawer/sidebar visible and #rail-reopen-btn hidden, then REAL-click
//       close. A no-op toggle handler fails the open-click wait.
//
// Pre-fix CSS fails R3/R4/R5: pointer-events:none on .session-sidebar__head

import { chromium } from 'playwright';

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
      && document.getElementById('sidebar-toggle-btn') !== null,
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
  const nav = document.querySelector('.session-sidebar__nav');
  const reopen = document.getElementById('rail-reopen-btn');
  if (!layout || !sidebar) return false;
  if (!layout.classList.contains('app-layout--drawer-open')) return false;
  if (sidebar.getBoundingClientRect().width < 200) return false;
  if (nav && getComputedStyle(nav).display === 'none') return false;
  if (reopen && getComputedStyle(reopen).display !== 'none') return false;
  return true;
}

function isMobileClosed() {
  const layout = document.querySelector('.app-layout');
  const sidebar = document.querySelector('.session-sidebar');
  const toggle = document.getElementById('sidebar-toggle-btn');
  if (!layout || !sidebar || !toggle) return false;
  if (window.innerWidth >= 721) return false;
  if (layout.classList.contains('app-layout--drawer-open')) return false;
  if (getComputedStyle(toggle).display === 'none') return false;
  return getComputedStyle(sidebar).display === 'none';
}

function isMobileOpen() {
  const layout = document.querySelector('.app-layout');
  const sidebar = document.querySelector('.session-sidebar');
  const reopen = document.getElementById('rail-reopen-btn');
  const toggle = document.getElementById('sidebar-toggle-btn');
  const title = document.querySelector('.session-sidebar__title');
  if (!layout || !sidebar || !toggle || !title) return false;
  if (window.innerWidth >= 721) return false;
  if (!layout.classList.contains('app-layout--drawer-open')) return false;
  if (getComputedStyle(toggle).display === 'none') return false;
  const sidebarStyle = getComputedStyle(sidebar);
  if (sidebarStyle.display === 'none' || sidebarStyle.visibility === 'hidden') return false;
  const box = sidebar.getBoundingClientRect();
  if (box.height < 80 || box.width < 200) return false;
  const titleStyle = getComputedStyle(title);
  if (titleStyle.display === 'none' || title.getBoundingClientRect().height <= 0) return false;
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
      const head = document.querySelector('.session-sidebar__head');
      const collapse = document.getElementById('sidebar-collapse-btn');
      const searchBox = document.querySelector('.session-sidebar__search-box');
      return {
        width: sidebar ? sidebar.getBoundingClientRect().width : -1,
        reopenDisplay: reopen ? getComputedStyle(reopen).display : 'missing',
        reopenEvents: reopen ? getComputedStyle(reopen).pointerEvents : 'missing',
        headEvents: head ? getComputedStyle(head).pointerEvents : 'missing',
        collapseDisplay: collapse ? getComputedStyle(collapse).display : 'missing',
        searchBoxDisplay: searchBox ? getComputedStyle(searchBox).display : 'missing',
        searchBoxEvents: searchBox ? getComputedStyle(searchBox).pointerEvents : 'missing',
      };
    });
    if (!(collapsed.width >= 40 && collapsed.width <= 48)) {
      fail(`R2: collapsed rail width must be ~44px (got ${collapsed.width})`);
    }
    if (collapsed.reopenDisplay === 'none' || collapsed.reopenDisplay === 'missing') {
      fail(`R2: #rail-reopen-btn must be visible (display=${collapsed.reopenDisplay})`);
    }
    if (collapsed.reopenEvents === 'none' || collapsed.headEvents === 'none') {
      fail(`R2: reopen hit chain still pointer-events:none (btn=${collapsed.reopenEvents} head=${collapsed.headEvents}) — pre-fix CSS fails here`);
    }
    if (collapsed.collapseDisplay !== 'none') {
      fail(`R2: hidden collapse control must stay display:none (got ${collapsed.collapseDisplay})`);
    }
    if (collapsed.searchBoxDisplay !== 'none') {
      fail(`R2: hidden search box must stay display:none (got ${collapsed.searchBoxDisplay})`);
    }
    if (collapsed.searchBoxEvents !== 'none') {
      fail(`R2: hidden search box must stay pointer-events:none (got ${collapsed.searchBoxEvents})`);
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
    const leftoverOpen = await page.evaluate(
      () => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true,
    );
    if (leftoverOpen) {
      await page.locator('#sidebar-toggle-btn').click({ force: false, timeout: 10000 });
    }
    await waitFor(page, isMobileClosed, 'R6: mobile drawer did not reach a closed starting state');
    await page.locator('#sidebar-toggle-btn').click({ force: false, timeout: 10000 });
    await waitFor(
      page,
      isMobileOpen,
      'R6: real click on #sidebar-toggle-btn did not OPEN the mobile drawer (a no-op toggle or leftover-open skip would fail here)',
    );
    const mobileOpen = await page.evaluate(() => {
      const sidebar = document.querySelector('.session-sidebar');
      const reopen = document.getElementById('rail-reopen-btn');
      const toggle = document.getElementById('sidebar-toggle-btn');
      return {
        drawer: document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true,
        sidebarDisplay: sidebar ? getComputedStyle(sidebar).display : 'missing',
        sidebarHeight: sidebar ? sidebar.getBoundingClientRect().height : -1,
        reopenDisplay: reopen ? getComputedStyle(reopen).display : 'missing',
        toggleDisplay: toggle ? getComputedStyle(toggle).display : 'missing',
      };
    });
    if (!mobileOpen.drawer) fail('R6: mobile drawer class missing after open click');
    if (mobileOpen.sidebarDisplay === 'none' || mobileOpen.sidebarHeight <= 0) {
      fail(`R6: mobile sidebar must be visible after open click (display=${mobileOpen.sidebarDisplay} height=${mobileOpen.sidebarHeight})`);
    }
    if (mobileOpen.reopenDisplay !== 'none') {
      fail(`R6: #rail-reopen-btn must stay hidden on <720px (display=${mobileOpen.reopenDisplay})`);
    }
    if (mobileOpen.toggleDisplay === 'none') {
      fail('R6: #sidebar-toggle-btn must remain the mobile drawer control');
    }
    await page.screenshot({ path: `${evidence}/sidebar-rail-mobile-open.png`, fullPage: true });
    await page.locator('#sidebar-toggle-btn').click({ force: false, timeout: 10000 });
    await waitFor(page, isMobileClosed, 'R6: real click on #sidebar-toggle-btn did not CLOSE the mobile drawer');
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
