// Mobile viewport Web E2E lane (playwright half of E2E.d/web/mobile.sh).
//
// Environment:
//   RPI_URL        http://127.0.0.1:<port>/web
//   RPI_TOKEN      token file content (served via rpi-auth.<token> subprotocol)
//   RPI_SLOW_TAIL  tail of the FIRST slow mock reply ("chunk-four-done")
//   RPI_CHROME     executable path of the system Chrome (optional)
//   RPI_EVIDENCE   evidence dir for screenshots
//
// Asserts the phone-width shell contract (styles.css media queries), including
// exact two-row composer geometry at 320/375/430px in both idle Send and
// streaming Stop states: textarea alone on row one; Command/upload/mic/live/action
// together on row two; every control >=44px; no wrap or horizontal overflow.

import { chromium } from 'playwright';
import { openSessionsDrawer } from './lib/feature-menu.mjs';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const slowTail = process.env.RPI_SLOW_TAIL || 'chunk-four-done';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';

const VIEWPORT = { width: 375, height: 667 };

function fail(message) {
  // Exit 2 (not 1): 1 means "playwright setup failure (node/chromium/npm)",
  // which must NOT be confused with an assertion failure (the mock's request
  // counter is stateful and a rerun would see shifted replies).
  console.error(`web-mobile: FAIL: ${message}`);
  process.exit(2);
}

async function waitFor(page, fn, label, timeoutMs = 25000, arg) {
  try {
    await page.waitForFunction(fn, arg, { timeout: timeoutMs });
  } catch {
    fail(`${label} (timeout ${timeoutMs}ms)`);
  }
}

// Feature actions live in the #feature-menu portal, so every panel toggle
// click must first open the real header trigger when the menu is closed.
async function openFeatureMenu(page) {
  const expanded = await page.evaluate(
    () => document.getElementById('feature-menu-btn')?.getAttribute('aria-expanded') === 'true'
  );
  if (expanded) return;
  await page.click('#feature-menu-btn');
  await waitFor(
    page,
    () => document.getElementById('feature-menu-btn')?.getAttribute('aria-expanded') === 'true',
    'feature menu did not open',
  );
}

async function assertComposerGeometry(page, width, state) {
  await page.setViewportSize({ width, height: 667 });
  await waitFor(
    page,
    (expected) => document.getElementById('send-btn')?.getAttribute('aria-label') === expected,
    `${width}px ${state}: composer action state did not settle`,
    10000,
    state === 'streaming-stop' ? 'Stop generating' : 'Send message',
  );
  const geometry = await page.evaluate(() => {
    const box = (selector) => {
      const el = document.querySelector(selector);
      if (!el) return null;
      const r = el.getBoundingClientRect();
      return { top: r.top, bottom: r.bottom, left: r.left, right: r.right, width: r.width, height: r.height };
    };
    return {
      textarea: box('#prompt-input'),
      command: box('#command-btn'),
      attach: box('#attach-btn'),
      mic: box('#mic-btn'),
      live: box('#live-btn'),
      action: box('#send-btn'),
      footer: box('.app-main > footer'),
      row: box('#composer-row'),
      buttons: box('#composer-buttons'),
      context: box('#composer-context'),
      contextBar: box('.composer-context__bar'),
      contextPercent: box('.composer-context__percent'),
      contextTotal: box('.composer-context__total'),
      actionLabel: document.getElementById('send-btn')?.getAttribute('aria-label') || '',
      scrollWidth: document.documentElement.scrollWidth,
      innerWidth: window.innerWidth,
      innerHeight: window.innerHeight,
    };
  });
  const captureFailure = async (message) => {
    await page.screenshot({ path: `${evidence}/mobile-${width}-${state}-failure.png`, fullPage: false }).catch(() => {});
    console.log(`web-mobile: failed geometry ${width}px ${state}: ${JSON.stringify(geometry)}`);
    fail(message);
  };
  const controls = [geometry.command, geometry.attach, geometry.mic, geometry.live, geometry.action];
  if (!geometry.textarea || !geometry.footer || !geometry.row || !geometry.buttons || controls.some((box) => !box)) {
    await captureFailure(`${width}px ${state}: composer geometry target missing`);
  }
  if (geometry.scrollWidth > geometry.innerWidth + 1) {
    await captureFailure(`${width}px ${state}: horizontal overflow ${geometry.scrollWidth}px > ${geometry.innerWidth}px`);
  }
  if (geometry.footer.bottom > geometry.innerHeight + 1 || geometry.footer.left < -1 || geometry.footer.right > geometry.innerWidth + 1) {
    await captureFailure(`${width}px ${state}: composer is outside viewport (${JSON.stringify(geometry.footer)})`);
  }
  if (geometry.textarea.height < 44) await captureFailure(`${width}px ${state}: textarea height ${geometry.textarea.height}px < 44px`);
  for (const [name, box] of [['Command', geometry.command], ['upload', geometry.attach], ['mic', geometry.mic], ['live', geometry.live], ['action', geometry.action]]) {
    if (box.width < 44 || box.height < 44) await captureFailure(`${width}px ${state}: ${name} control is ${box.width}x${box.height}px`);
  }
  const secondTop = geometry.command.top;
  const secondBottom = geometry.command.bottom;
  for (const [name, box] of [['upload', geometry.attach], ['mic', geometry.mic], ['live', geometry.live], ['action', geometry.action]]) {
    if (Math.abs(box.top - secondTop) > 1 || Math.abs(box.bottom - secondBottom) > 1) {
      await captureFailure(`${width}px ${state}: ${name} wrapped off the Command toolbar row`);
    }
  }
  const yLevels = new Set([
    Math.round(geometry.textarea.top),
    Math.round(geometry.command.top),
    Math.round(geometry.attach.top),
    Math.round(geometry.mic.top),
    Math.round(geometry.live.top),
    Math.round(geometry.action.top),
  ]);
  if (yLevels.size !== 2) {
    await captureFailure(`${width}px ${state}: composer uses ${yLevels.size} y-levels instead of exactly two (${[...yLevels].join(', ')})`);
  }
  if (geometry.textarea.bottom > secondTop - 3) {
    await captureFailure(`${width}px ${state}: textarea is not isolated on row one (textarea bottom ${geometry.textarea.bottom}, toolbar top ${secondTop})`);
  }
  if (geometry.command.left < geometry.row.left - 1 || geometry.action.right > geometry.row.right + 1) {
    await captureFailure(`${width}px ${state}: toolbar escapes composer row bounds`);
  }
  await page.screenshot({ path: `${evidence}/mobile-${width}-${state}.png`, fullPage: true });
}

async function main() {
  if (!url) fail('RPI_URL is required');
  const launchOptions = chromePath ? { executablePath: chromePath } : {};
  const browser = await chromium.launch(launchOptions);
  try {
    const page = await browser.newPage({ viewport: VIEWPORT });
    if (token) {
      await page.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
    }
    const browserErrors = [];
    page.on('pageerror', (err) => {
      browserErrors.push(`pageerror: ${err.message}`);
    });
    page.on('console', (message) => {
      if (message.type() === 'error') browserErrors.push(`console: ${message.text()}`);
    });

    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'page title missing');
    // 1. Core flow: connect with the token, prompt round-trip, open a panel.
    await waitFor(
      page,
      () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true,
      'WS did not reach "connected"'
    );
    // 1a. While the first stream is in flight, the unified composer action
    // switches to Stop, stays at least 44px high, and remains on-screen.
    await page.fill('#prompt-input', 'hello from a phone');
    await page.press('#prompt-input', 'Enter');
    await waitFor(page, () => document.getElementById('send-btn')?.getAttribute('aria-label') === 'Stop generating', 'streaming never started');
    for (const width of [320, 375, 430]) {
      await assertComposerGeometry(page, width, 'streaming-stop');
    }
    await page.setViewportSize(VIEWPORT);
    const streaming = await page.evaluate(() => {
      const rect = (sel) => {
        const el = document.querySelector(sel);
        if (!el) return null;
        const r = el.getBoundingClientRect();
        return { top: r.top, bottom: r.bottom, left: r.left, width: r.width, height: r.height };
      };
      const action = document.querySelector('#send-btn');
      const actionBox = rect('#send-btn');
      const footer = rect('.app-main > footer');
      return {
        actionLabel: action?.getAttribute('aria-label') || '',
        actionH: actionBox ? actionBox.height : -1,
        footerBottom: footer ? footer.bottom : -1,
        scrollWidth: document.documentElement.scrollWidth,
        innerWidth: window.innerWidth,
        innerHeight: window.innerHeight,
      };
    });
    if (streaming.actionLabel !== 'Stop generating') {
      fail(`unified action did not switch to Stop while streaming: "${streaming.actionLabel}"`);
    }
    if (streaming.actionH < 44) {
      fail(`#send-btn Stop touch target is ${streaming.actionH}px while streaming (must be >= 44px)`);
    }
    if (streaming.scrollWidth > streaming.innerWidth + 1) {
      fail(`horizontal overflow while streaming: scrollWidth ${streaming.scrollWidth} > viewport ${streaming.innerWidth}`);
    }
    if (streaming.footerBottom < 0 || streaming.footerBottom > streaming.innerHeight) {
      fail(`composer sits below the fold while streaming: bottom ${streaming.footerBottom} > innerHeight ${streaming.innerHeight}`);
    }
    await page.screenshot({ path: `${evidence}/mobile-streaming.png`, fullPage: true });
    await waitFor(
      page,
      (tail) => document.body.textContent.includes(tail),
      'slow reply never streamed into the DOM',
      30000,
      slowTail
    );
    await waitFor(
      page,
      () =>
        document.getElementById('send-btn')?.getAttribute('aria-label') === 'Send message',
      'stream did not settle back to the idle Send action',
      30000
    );
    for (const width of [320, 375, 430]) {
      await assertComposerGeometry(page, width, 'idle-send');
    }
    await page.setViewportSize(VIEWPORT);
    // F5: the header feature menu trigger must be VISIBLE at phone width (the
    // media-query rule must win the cascade — an unconditional display:none
    // after the media block hid it at every width) and its Sessions entry
    // must open the session sidebar drawer.
    const menuDisplay = await page.evaluate(
      () => getComputedStyle(document.getElementById('feature-menu-btn')).display
    );
    if (menuDisplay === 'none') {
      fail('#feature-menu-btn is hidden at phone width (CSS cascade regression)');
    }
    const menuBox = await page.locator('#feature-menu-btn').boundingBox();
    if (!menuBox || menuBox.width < 20 || menuBox.height < 20) {
      fail(`#feature-menu-btn has no clickable area at phone width (box ${JSON.stringify(menuBox)})`);
    }
    await openSessionsDrawer(page);
    await waitFor(
      page,
      () => {
        const layout = document.querySelector('.app-layout');
        const sidebar = document.querySelector('.session-sidebar');
        return (
          layout !== null &&
          layout.classList.contains('app-layout--drawer-open') &&
          sidebar !== null &&
          (sidebar.getBoundingClientRect().left || -1) <= 1
        );
      },
      'session sidebar drawer never opened from the feature menu'
    );
    await waitFor(
      page,
      () => document.querySelector('.session-sidebar__row--active') !== null,
      'mobile session catalog never settled an active row',
    );
    const sessionList = await page.evaluate(() => {
      const section = (selector) => {
        const el = document.querySelector(selector);
        if (!el) return null;
        const style = getComputedStyle(el);
        const box = el.getBoundingClientRect();
        return { height: box.height, top: box.top, bottom: box.bottom, flex: style.flex, minHeight: style.minHeight };
      };
      const list = document.querySelector('.session-sidebar__list');
      const row = document.querySelector('.session-sidebar__row--active');
      const listBox = list?.getBoundingClientRect();
      const rowBox = row?.getBoundingClientRect();
      return {
        list: section('.session-sidebar__list'),
        footer: section('.session-sidebar__foot'),
        row: section('.session-sidebar__row--active'),
        activeButton: section('.session-sidebar__row--active .session-sidebar__switch'),
        rowVisible: Boolean(listBox && rowBox && rowBox.height >= 44 && rowBox.top >= listBox.top - 1 && rowBox.bottom <= listBox.bottom + 1),
      };
    });
    if (!sessionList.list || sessionList.list.height < 44 || !sessionList.rowVisible) {
      await page.screenshot({ path: `${evidence}/mobile-sidebar-list-failure.png`, fullPage: false }).catch(() => {});
      console.log(`web-mobile: session list geometry: ${JSON.stringify(sessionList)}`);
      fail(`mobile session list has no visible active row: ${JSON.stringify(sessionList)}`);
    }
    // Same ≤720px drawer breakpoint as the shell: the New Session footer
    // control stays a real touch target. Height only — padding is not asserted.
    const newSessionTouch = await page.evaluate(() => {
      const button = document.getElementById('sidebar-manage-btn');
      const rect = button?.getBoundingClientRect();
      return {
        innerWidth: window.innerWidth,
        narrow: window.matchMedia('(max-width: 720px)').matches,
        width: rect ? rect.width : -1,
        height: rect ? rect.height : -1,
      };
    });
    if (!newSessionTouch.narrow || newSessionTouch.width <= 0 || newSessionTouch.height < 44) {
      await page.screenshot({ path: `${evidence}/mobile-new-session-height-failure.png`, fullPage: false }).catch(() => {});
      fail(`New Session touch target is ${newSessionTouch.width}x${newSessionTouch.height}px under the ≤720px drawer (${JSON.stringify(newSessionTouch)})`);
    }
    await page.screenshot({ path: `${evidence}/mobile-sidebar-open.png`, fullPage: true });
    const todoNavPresence = await page.evaluate(() => ({
      todo: document.querySelectorAll('#todo-toggle-btn').length,
      legacyTasks: document.querySelectorAll('#todos-toggle-btn').length,
      legacyChildren: document.querySelectorAll('#todo-child-toggle-btn').length,
    }));
    if (todoNavPresence.todo !== 1 || todoNavPresence.legacyTasks !== 0 || todoNavPresence.legacyChildren !== 0) {
      fail(`Todo feature-nav presence mismatch: ${JSON.stringify(todoNavPresence)}`);
    }
    // The panel toggles now live in the #feature-menu portal, so open the
    // menu first, then the unified Todo module with its task and child
    // sections together.
    await openFeatureMenu(page);
    await page.click('#todo-toggle-btn');
    await waitFor(page, () => {
      const panels = document.querySelectorAll('#todo-panel');
      const panel = panels.length === 1 ? panels[0] : null;
      return panel !== null
        && panel.querySelector('.todo-panel__tasks') !== null
        && panel.querySelector('.todo-child-section') !== null;
    }, 'unified Todo module did not open with task and child sections');
    // Opening a panel from the feature nav MUST close the session drawer on
    // phones so the full-screen panel is not covered and its close control
    // stays reachable (z-index alone is not enough for the return path).
    const panelOpenState = await page.evaluate(() => {
      const layout = document.querySelector('.app-layout');
      const panel = document.getElementById('todo-panel');
      const close = document.getElementById('todo-close-btn');
      const pr = panel?.getBoundingClientRect();
      const cr = close?.getBoundingClientRect();
      return {
        drawerOpen: !!layout?.classList.contains('app-layout--drawer-open'),
        panelW: pr ? pr.width : -1,
        panelH: pr ? pr.height : -1,
        closeW: cr ? cr.width : -1,
        closeH: cr ? cr.height : -1,
        closeAria: close?.getAttribute('aria-label') || '',
        scrollWidth: document.documentElement.scrollWidth,
        innerWidth: window.innerWidth,
      };
    });
    if (panelOpenState.drawerOpen) {
      fail('session sidebar stayed open after opening the Todo panel (mobile dead-end)');
    }
    if (panelOpenState.panelW < panelOpenState.innerWidth - 1) {
      fail(`todo panel is not full-screen width: ${panelOpenState.panelW} vs ${panelOpenState.innerWidth}`);
    }
    if (panelOpenState.panelH < 100) {
      fail(`todo panel content height is ${panelOpenState.panelH} (must be non-zero / usable)`);
    }
    if (panelOpenState.closeW < 44 || panelOpenState.closeH < 44) {
      fail(`#todo-close-btn touch target is ${panelOpenState.closeW}x${panelOpenState.closeH} (must be >= 44px)`);
    }
    if (!panelOpenState.closeAria) {
      fail('#todo-close-btn is missing aria-label');
    }
    if (panelOpenState.scrollWidth > panelOpenState.innerWidth + 1) {
      fail(`horizontal overflow with todo panel open: ${panelOpenState.scrollWidth} > ${panelOpenState.innerWidth}`);
    }
    await page.screenshot({ path: `${evidence}/mobile-panel.png`, fullPage: true });

    // Close must work and return to the shell (no dead-end full-screen panel).
    await page.click('#todo-close-btn');
    await waitFor(page, () => document.getElementById('todo-panel') === null, 'todo panel did not close');

    // Re-open the session drawer after closing a panel (return path).
    await openSessionsDrawer(page);
    await waitFor(
      page,
      () => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true,
      'session sidebar did not reopen after panel close'
    );

    // Goal + Workflows open/close with the same shell contract (no overflow,
    // close >= 44, drawer collapses on open). Both toggles live in the
    // feature menu, which closes the drawer when it opens on phones.
    for (const { toggle, panel, close } of [
      { toggle: '#goal-panel-btn', panel: '#goal-panel', close: '#goal-close-btn' },
      { toggle: '#workflow-toggle-btn', panel: '#workflow-panel', close: '#workflow-close-btn' },
    ]) {
      await openFeatureMenu(page);
      await page.click(toggle);
      await waitFor(page, (sel) => document.querySelector(sel) !== null, `${panel} did not open`, 15000, panel);
      const state = await page.evaluate(({ panelSel, closeSel }) => {
        const p = document.querySelector(panelSel);
        const c = document.querySelector(closeSel);
        const pr = p?.getBoundingClientRect();
        const cr = c?.getBoundingClientRect();
        return {
          drawerOpen: document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true,
          panelW: pr ? pr.width : -1,
          panelH: pr ? pr.height : -1,
          closeW: cr ? cr.width : -1,
          closeH: cr ? cr.height : -1,
          closeAria: c?.getAttribute('aria-label') || '',
          scrollWidth: document.documentElement.scrollWidth,
          innerWidth: window.innerWidth,
        };
      }, { panelSel: panel, closeSel: close });
      if (state.drawerOpen) fail(`${panel}: session sidebar stayed open`);
      if (state.panelW < state.innerWidth - 1) fail(`${panel}: width ${state.panelW} vs viewport ${state.innerWidth}`);
      if (state.panelH < 100) fail(`${panel}: height ${state.panelH}`);
      await page.click(close);
      await waitFor(page, (sel) => document.querySelector(sel) === null, `${panel} did not close`, 10000, panel);
    }

    // 2-6. Shell contract at 375×667 (idle state — the run has finished).
    // Ensure no feature panel remains open for the composer metrics.
    await page.evaluate(() => {
      for (const id of ['todo-close-btn', 'workflow-close-btn', 'goal-close-btn', 'subagents-close-btn']) {
        document.getElementById(id)?.click();
      }
    });
    // Re-open the drawer so the sidebar-owned touch targets are on-screen.
    const drawerOpen = await page.evaluate(
      () => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true
    );
    if (!drawerOpen) {
      await openSessionsDrawer(page);
      await waitFor(
        page,
        () => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true,
        'sidebar open for shell contract metrics'
      );
    }
    // The drawer class flips immediately but the open transition runs 0.18s;
    // wait for it to settle so geometry is measured fully on-screen (a
    // mid-transition drawer would still be translated off-screen).
    await waitFor(
      page,
      () => (document.querySelector('.session-sidebar')?.getBoundingClientRect().left ?? -999) >= -1,
      'session drawer did not finish opening'
    );
    // Sidebar targets are measured WHILE THE DRAWER IS OPEN: a closed drawer
    // is translated off-screen (translateX(-110%)), so any hidden/occluded/
    // offscreen target fails the geometry checks below instead of passing a
    // pure CSS-height probe of invisible buttons.
    const probeTargets = (ids) => page.evaluate((selectors) => {
      const rect = (sel) => {
        const el = document.querySelector(sel);
        if (!el) return null;
        const r = el.getBoundingClientRect();
        return { top: r.top, bottom: r.bottom, left: r.left, right: r.right, width: r.width, height: r.height };
      };
      return {
        innerWidth: window.innerWidth,
        innerHeight: window.innerHeight,
        targets: selectors.map((sel) => {
          const r = rect(sel);
          return {
            sel,
            left: r ? r.left : -1,
            right: r ? r.right : -1,
            top: r ? r.top : -1,
            bottom: r ? r.bottom : -1,
            width: r ? r.width : -1,
            height: r ? r.height : -1,
          };
        }),
      };
    }, ids);
    const checkTargets = (probe, scope) => {
      for (const t of probe.targets) {
        if (t.height < 44) fail(`touch target ${t.sel} is ${t.height}px (must be >= 44px)`);
        if (t.left < -1 || t.right > probe.innerWidth + 1) {
          fail(`touch target ${t.sel} is off-screen in the ${scope}: left ${t.left}, right ${t.right} (viewport ${probe.innerWidth}px)`);
        }
        if (t.top > probe.innerHeight || t.bottom < 0) {
          fail(`touch target ${t.sel} is off-screen vertically in the ${scope}: top ${t.top}, bottom ${t.bottom} (viewport ${probe.innerHeight}px)`);
        }
      }
    };
    checkTargets(
      await probeTargets(['#send-btn', '.session-sidebar__group-head', '.session-sidebar__switch']),
      'open drawer',
    );
    // Feature toggles live in the #feature-menu portal, so measure them with
    // that menu actually open. Opening it closes the phone drawer by design.
    await openFeatureMenu(page);
    checkTargets(
      await probeTargets(['#settings-toggle-btn', '#todo-toggle-btn', '#goal-panel-btn', '#workflow-toggle-btn']),
      'open feature menu',
    );
    // Open the unified Todo module from the open feature menu; opening a panel
    // closes the drawer by design, so the full-screen width is asserted on
    // the panel itself in a separate step (not on the closed drawer).
    await page.click('#todo-toggle-btn');
    await waitFor(page, () => {
      const panels = document.querySelectorAll('#todo-panel');
      const panel = panels.length === 1 ? panels[0] : null;
      return panel !== null
        && panel.querySelector('.todo-panel__tasks') !== null
        && panel.querySelector('.todo-child-section') !== null;
    }, 'unified Todo module did not reopen with task and child sections');
    const todoMetrics = await page.evaluate(() => {
      const pr = document.getElementById('todo-panel')?.getBoundingClientRect();
      return {
        innerWidth: window.innerWidth,
        panelW: pr ? pr.width : -1,
      };
    });
    if (todoMetrics.panelW < todoMetrics.innerWidth - 1) {
      fail(`todo panel is not full-screen width: ${todoMetrics.panelW} vs viewport ${todoMetrics.innerWidth}`);
    }
    const metrics = await page.evaluate(() => {
      const rect = (sel) => {
        const el = document.querySelector(sel);
        if (!el) return null;
        const r = el.getBoundingClientRect();
        return { top: r.top, bottom: r.bottom, left: r.left, width: r.width, height: r.height };
      };
      const footer = rect('.app-main > footer');
      const ta = rect('#prompt-input');
      const thinking = document.getElementById('thinking-select');
      const thinkingDisplay = thinking ? getComputedStyle(thinking).display : 'missing';
      return {
        innerWidth: window.innerWidth,
        innerHeight: window.innerHeight,
        scrollWidth: document.documentElement.scrollWidth,
        composerBottom: footer ? footer.bottom : -1,
        textareaW: ta ? ta.width : -1,
        textareaH: ta ? ta.height : -1,
        actionLabel: document.getElementById('send-btn')?.getAttribute('aria-label') || '',
        thinkingDisplay,
      };
    });

    if (metrics.scrollWidth > metrics.innerWidth + 1) {
      fail(`horizontal overflow: scrollWidth ${metrics.scrollWidth} > viewport ${metrics.innerWidth}`);
    }
    if (metrics.composerBottom < 0 || metrics.composerBottom > metrics.innerHeight) {
      fail(`composer sits below the fold: bottom ${metrics.composerBottom} > innerHeight ${metrics.innerHeight}`);
    }
    // Idle usable composer: one unified Send/Stop action leaves the textarea
    // as the dominant composer element on a phone.
    if (metrics.textareaW < 240) {
      fail(`#prompt-input usable width is ${metrics.textareaW}px at 375px (must be >= 240px after removing dedicated Steer/Follow up)`);
    }
    if (metrics.textareaH < 44) {
      fail(`#prompt-input usable height is ${metrics.textareaH}px (must be >= 44px)`);
    }
    if (metrics.actionLabel !== 'Send message') {
      fail(`unified action did not return to Send while idle: "${metrics.actionLabel}"`);
    }
    if (metrics.thinkingDisplay === 'none' || metrics.thinkingDisplay === 'missing') {
      fail(`#thinking-select must be visible at phone width (header has room), computed display = ${metrics.thinkingDisplay}`);
    }
    if (metrics.thinkingDisplay !== 'inline-block' && metrics.thinkingDisplay !== 'inline' && metrics.thinkingDisplay !== 'block') {
      fail(`#thinking-select unexpected display at phone width: ${metrics.thinkingDisplay}`);
    }

    if (await page.locator('#todo-panel').count()) {
      await page.click('#todo-close-btn');
      await waitFor(page, () => document.getElementById('todo-panel') === null, 'todo panel stayed open before the starred-reply journey');
    }
    await page.setViewportSize({ width: 390, height: 844 });
    await waitFor(
      page,
      () => document.getElementById('send-btn')?.getAttribute('aria-label') === 'Send message',
      '390x844 composer did not stay idle before the starred-reply journey',
    );
    const starMarker = 'steer-1-';
    await waitFor(page, (marker) => {
      const row = [...document.querySelectorAll('main#transcript [data-runtime-key] .transcript-content .msg--assistant')]
        .find((candidate) => (candidate.querySelector('.assistant-text')?.textContent || '').includes(marker));
      const button = row?.querySelector('button.assistant-star-toggle');
      return !!button
        && !(row.textContent || '').includes('Saving reply…')
        && button.getAttribute('aria-label') === 'Star'
        && button.getAttribute('aria-pressed') === 'false';
    }, '390x844 settled assistant reply never exposed Star', 30000, starMarker);
    const before = await page.evaluate((marker) => {
      const row = [...document.querySelectorAll('main#transcript .msg--assistant')]
        .find((candidate) => (candidate.querySelector('.assistant-text')?.textContent || '').includes(marker));
      const button = row?.querySelector('button.assistant-star-toggle');
      return {
        entryId: button?.getAttribute('data-entry-id') || row?.getAttribute('data-entry-id') || '',
        itemId: row?.getAttribute('data-item-id') || '',
      };
    }, starMarker);
    if (before.entryId === '' || before.itemId === '') fail(`390x844 star has no authoritative identity (${JSON.stringify(before)})`);
    const userStars = await page.evaluate(() => document.querySelectorAll('.msg--user button.assistant-star-toggle, .tool-card button.assistant-star-toggle').length);
    if (userStars !== 0) fail(`390x844 exposed ${userStars} star controls outside assistant replies`);
    await page.locator('main#transcript .msg--assistant', { hasText: starMarker }).locator('button.assistant-star-toggle').click();
    await waitFor(page, (entryId) => {
      const button = [...document.querySelectorAll('main#transcript button.assistant-star-toggle')]
        .find((candidate) => (candidate.getAttribute('data-entry-id') || candidate.closest('[data-entry-id]')?.getAttribute('data-entry-id')) === entryId);
      const listed = [...document.querySelectorAll('.starred-reply-minimap button.starred-reply-minimap__jump')]
        .some((candidate) => candidate.getAttribute('data-entry-id') === entryId);
      const rail = document.querySelector('.starred-reply-minimap');
      const box = rail?.getBoundingClientRect();
      return button?.getAttribute('aria-pressed') === 'true'
        && button.getAttribute('aria-label') === 'Unstar'
        && listed
        && !!rail
        && rail.classList.contains('is-collapsed')
        && !!box
        && box.width >= 44
        && box.width <= 44.5;
    }, '390x844 Star did not settle a collapsed 44px minimap', 10000, before.entryId);
    /* Real-box sizing (no CSS probe): Sol round-4 UNIFIED side-band contract
     * — EVERY viewport (this 390x844 included) renders the star in the row's
     * reserved LEFT BAND: fully left-outside the text block, intersecting the
     * first Range text line, hidden .assistant-star-toggle__label with a
     * visible glyph, resting opacity 1 (fully visible on coarse AND fine).
     * The old above-text fallback (flow) is REMOVED — a flow layout now
     * FAILS this lane (RED) until the owning CSS change lands. The 44px gate
     * keys on the same `(pointer: coarse), (hover: none), (max-width: 720px)`
     * media the stylesheet uses. */
    const replyStarBox = await page.evaluate((marker) => {
      const row = [...document.querySelectorAll('main#transcript .msg--assistant')]
        .find((candidate) => (candidate.querySelector('.assistant-text')?.textContent || '').includes(marker));
      const button = row?.querySelector('button.assistant-star-toggle');
      const text = row?.querySelector('.assistant-text');
      if (!button || !text) return null;
      const box = button.getBoundingClientRect();
      const textBox = text.getBoundingClientRect();
      const glyph = button.querySelector('span');
      const actions = button.closest('.msg--assistant__actions');
      const label = button.querySelector('.assistant-star-toggle__label');
      const labelStyle = label ? getComputedStyle(label) : null;
      const actionsBox = actions?.getBoundingClientRect();
      const content = document.querySelector('main#transcript [data-runtime-key] .transcript-content');
      /* Sol: the first-line gate uses the REAL rect of the first rendered
       * codepoint — TreeWalker SHOW_TEXT, first non-empty text node with a
       * non-zero visible rect, then a char Range over the first
       * non-whitespace codepoint (surrogate-safe). NO fallback to block
       * rects or line-height estimates; no visible text = null (asserted
       * failure). */
      const firstChar = (() => {
        const walker = document.createTreeWalker(text, NodeFilter.SHOW_TEXT, {
          acceptNode: (node) => {
            if (!node.nodeValue || node.nodeValue.trim().length === 0) return NodeFilter.FILTER_REJECT;
            const probe = document.createRange();
            probe.selectNodeContents(node);
            for (const rect of probe.getClientRects()) {
              if (rect.width > 0 && rect.height > 0) return NodeFilter.FILTER_ACCEPT;
            }
            return NodeFilter.FILTER_REJECT;
          },
        });
        for (let node = walker.nextNode(); node; node = walker.nextNode()) {
          const value = node.nodeValue;
          for (let i = 0; i < value.length;) {
            const code = value.codePointAt(i);
            const units = code > 0xffff ? 2 : 1;
            const ch = String.fromCodePoint(code);
            i += units;
            if (/\S/.test(ch)) {
              const r = document.createRange();
              r.setStart(node, i - units);
              r.setEnd(node, i);
              const rects = r.getClientRects();
              if (rects.length > 0 && rects[0].width > 0 && rects[0].height > 0) {
                return { left: rects[0].left, right: rects[0].right, top: rects[0].top, height: rects[0].height };
              }
            }
          }
        }
        return null;
      })();
      return {
        outsideGate: window.matchMedia('(min-width: 769px) and (pointer: fine) and (hover: hover)').matches,
        width: box.width,
        height: box.height,
        top: box.top,
        bottom: box.bottom,
        right: box.right,
        left: box.left,
        /* Row-based no-clip bound: the star lives in the row's reserved
         * left band, inside the row's own paint box — left >= rowLeft plus
         * right <= text left prove containment by construction. */
        rowLeft: row.getBoundingClientRect().left,
        outsideRoom: content ? parseFloat(getComputedStyle(content).paddingLeft) + 11 : -1,
        hitTarget: (() => {
          const hitEl = document.elementFromPoint(box.left + box.width / 2, box.top + box.height / 2);
          return !!hitEl && (hitEl === button || button.contains(hitEl));
        })(),
        textBlockLeft: textBox.left,
        firstLineTop: firstChar ? firstChar.top : -1,
        firstLineHeight: firstChar ? firstChar.height : -1,
        firstCharRect: firstChar ? { left: firstChar.left, right: firstChar.right } : null,
        textContentLeft: textBox.left + parseFloat(getComputedStyle(text).paddingLeft),
        glyphLeft: glyph ? glyph.getBoundingClientRect().left : -1,
        inActions: actions !== null,
        actionsPaddingLeft: actions ? parseFloat(getComputedStyle(actions).paddingLeft) : -1,
        actionsBeforeBody: actions !== null
          && (actions.compareDocumentPosition(text) & Node.DOCUMENT_POSITION_FOLLOWING) !== 0,
        actionsBottom: actionsBox ? actionsBox.bottom : -1,
        textTop: textBox.top,
        controls: row.querySelectorAll('button.assistant-star-toggle').length,
        labelDisplay: labelStyle ? labelStyle.display : 'absent',
        glyphDisplay: glyph ? getComputedStyle(glyph).display : 'absent',
        glyphW: glyph ? glyph.getBoundingClientRect().width : -1,
        glyphH: glyph ? glyph.getBoundingClientRect().height : -1,
        unstarLabel: (label?.textContent || '').trim(),
        unstarLabelVisible: !!label
          && labelStyle.display !== 'none'
          && labelStyle.visibility !== 'hidden'
          && label.getBoundingClientRect().height > 0,
      };
    }, starMarker);
    if (!replyStarBox) fail('390x844 starred reply lost its star control before the sizing probe');
    if (replyStarBox.controls !== 1) {
      fail(`390x844 reply exposes ${replyStarBox.controls} star controls (must be exactly 1)`);
    }
    /* Sol unified contract: the accessible Unstar name stays on the button,
     * but the VISIBLE .assistant-star-toggle__label is hidden and the glyph
     * carries the presentation. */
    if (replyStarBox.unstarLabel !== 'Unstar') {
      fail(`390x844 reply star lost its accessible Unstar name (${JSON.stringify(replyStarBox)})`);
    }
    if (replyStarBox.labelDisplay !== 'none') {
      fail(`390x844 .assistant-star-toggle__label must be display:none (got ${replyStarBox.labelDisplay})`);
    }
    if (replyStarBox.glyphDisplay === 'none' || replyStarBox.glyphDisplay === 'absent'
      || !(replyStarBox.glyphW > 0 && replyStarBox.glyphH > 0)) {
      fail(`390x844 star glyph must stay visible next to the hidden label (${JSON.stringify(replyStarBox)})`);
    }
    const coarsePointer = await page.evaluate(() => window.matchMedia('(pointer: coarse), (hover: none)').matches);
    const starMinTarget = coarsePointer ? 44 : 20;
    if (replyStarBox.width < starMinTarget || replyStarBox.height < starMinTarget) {
      fail(`390x844 reply star target is ${replyStarBox.width}x${replyStarBox.height}px (must be >= ${starMinTarget} on ${coarsePointer ? 'coarse' : 'fine'} pointer)`);
    }
    /* Rects do not prove hittability: paint containment or an overlay can
     * clip the box while its rect still reads full-size. The star must
     * receive hits at its own center — and the real Playwright star click
     * above already exercised the same target. */
    if (!replyStarBox.hitTarget) fail(`390x844 elementFromPoint at the star center missed the button (${JSON.stringify(replyStarBox)})`);
    // UNCONDITIONAL side-band geometry (no media branch, no fallback): the
    // star must hang fully left-outside the text block, intersect the first
    // Range text line, and stay inside the row's reserved band.
    if (replyStarBox.right > replyStarBox.textBlockLeft + 1) {
      fail(`390x844 reply star is not fully left-outside the text block (right ${replyStarBox.right} vs text left ${replyStarBox.textBlockLeft}) — the legacy above-body flow is NOT an accepted state`);
    }
    if (replyStarBox.firstLineTop < 0 || !(replyStarBox.firstLineHeight > 0)) {
      fail(`390x844 reply body produced no visible first-codepoint text rect (TreeWalker found none; ${JSON.stringify(replyStarBox.firstCharRect)})`);
    }
    if (!(replyStarBox.top < replyStarBox.firstLineTop + replyStarBox.firstLineHeight && replyStarBox.bottom > replyStarBox.firstLineTop)) {
      fail(`390x844 reply star does not intersect the first text line (${JSON.stringify(replyStarBox)})`);
    }
    if (replyStarBox.left < replyStarBox.rowLeft - 1) {
      fail(`390x844 reply star left the row's reserved band (left ${replyStarBox.left} vs row left ${replyStarBox.rowLeft})`);
    }
    /* Resting chrome: the star must be FULLY VISIBLE at rest on this phone
     * lane (the ≤720px / coarse media sizes it 44px but never hides it).
     * Park the real pointer first so a click-hover fill cannot masquerade as
     * the rest state; a hidden resting star (opacity < 1) FAILS here. */
    await page.mouse.move(0, 0);
    await waitFor(page, (entryId) => {
      const button = [...document.querySelectorAll('main#transcript button.assistant-star-toggle')]
        .find((candidate) => (candidate.getAttribute('data-entry-id') || candidate.closest('[data-entry-id]')?.getAttribute('data-entry-id')) === entryId);
      return !!button
        && !button.matches(':hover')
        && !button.matches(':focus-visible')
        && parseFloat(getComputedStyle(button).opacity) === 1;
    }, '390x844 star never settled fully visible and hovering-free', 5000, before.entryId);
    const restingStar = await page.evaluate((entryId) => {
      const button = [...document.querySelectorAll('main#transcript button.assistant-star-toggle')]
        .find((candidate) => (candidate.getAttribute('data-entry-id') || candidate.closest('[data-entry-id]')?.getAttribute('data-entry-id')) === entryId);
      if (!button) return null;
      const style = getComputedStyle(button);
      return {
        hovered: button.matches(':hover'),
        focusVisible: button.matches(':focus-visible'),
        opacity: parseFloat(style.opacity),
        backgroundColor: style.backgroundColor,
        outlineStyle: style.outlineStyle,
      };
    }, before.entryId);
    if (!restingStar || restingStar.hovered || restingStar.focusVisible) {
      fail(`390x844 the parked star is not in a real resting state (${JSON.stringify(restingStar)})`);
    }
    if (restingStar.opacity !== 1) {
      fail(`390x844 resting star must be fully visible at opacity 1 (got ${restingStar.opacity}) — hidden resting chrome is not an accepted state`);
    }
    if (restingStar.outlineStyle !== 'none') {
      fail(`390x844 resting star must not carry a focus ring (${JSON.stringify(restingStar)})`);
    }
    /* Real pointer hover must paint the bg-hover fill — the hover feedback
     * contract — while the star stays at opacity 1. */
    const rowStar = page.locator('main#transcript .msg--assistant', { hasText: starMarker }).locator('button.assistant-star-toggle');
    await rowStar.hover();
    await waitFor(page, ({ entryId, restBackground }) => {
      const button = [...document.querySelectorAll('main#transcript button.assistant-star-toggle')]
        .find((candidate) => (candidate.getAttribute('data-entry-id') || candidate.closest('[data-entry-id]')?.getAttribute('data-entry-id')) === entryId);
      if (!button || !button.matches(':hover')) return false;
      const box = button.getBoundingClientRect();
      const hit = document.elementFromPoint(box.left + box.width / 2, box.top + box.height / 2);
      const style = getComputedStyle(button);
      return parseFloat(style.opacity) === 1
        && style.backgroundColor !== restBackground
        && !/^(transparent|rgba\(0, 0, 0, 0\))$/.test(style.backgroundColor)
        && !!hit && (hit === button || button.contains(hit));
    }, '390x844 real hover must paint the star hover fill at opacity 1', 5000, { entryId: before.entryId, restBackground: restingStar.backgroundColor });
    await page.mouse.move(0, 0);
    await waitFor(page, ({ entryId, restBackground }) => {
      const button = [...document.querySelectorAll('main#transcript button.assistant-star-toggle')]
        .find((candidate) => (candidate.getAttribute('data-entry-id') || candidate.closest('[data-entry-id]')?.getAttribute('data-entry-id')) === entryId);
      if (!button || button.matches(':hover')) return false;
      const style = getComputedStyle(button);
      return parseFloat(style.opacity) === 1 && style.backgroundColor === restBackground;
    }, '390x844 pointer leaving the star must restore its fully-visible resting chrome', 5000, { entryId: before.entryId, restBackground: restingStar.backgroundColor });
    /* Keyboard focus must produce REAL feedback — the 1px accent ring — on a
     * star that was already visible at rest. Revealing a hidden control on
     * focus is NOT accepted focus feedback. */
    await page.click('#prompt-input');
    let keyboardStar = null;
    for (let press = 0; press < 60 && !(keyboardStar && keyboardStar.reached); press += 1) {
      await page.keyboard.press('Shift+Tab');
      keyboardStar = await page.evaluate(() => {
        const active = document.activeElement;
        const star = active instanceof HTMLElement && active.classList.contains('assistant-star-toggle') ? active : null;
        return star
          ? { reached: true, entryId: star.getAttribute('data-entry-id') || '', focusVisible: star.matches(':focus-visible') }
          : { reached: false };
      });
    }
    if (!keyboardStar || !keyboardStar.reached || !keyboardStar.focusVisible || keyboardStar.entryId === '') {
      fail(`390x844 real Shift+Tab keyboard never landed :focus-visible on an assistant star (${JSON.stringify(keyboardStar)})`);
    }
    const keyboardFeedback = await page.evaluate((entryId) => {
      const button = [...document.querySelectorAll('main#transcript button.assistant-star-toggle')]
        .find((candidate) => (candidate.getAttribute('data-entry-id') || candidate.closest('[data-entry-id]')?.getAttribute('data-entry-id')) === entryId);
      if (!button) return null;
      const style = getComputedStyle(button);
      return {
        active: document.activeElement === button,
        focusVisible: button.matches(':focus-visible'),
        opacity: parseFloat(style.opacity),
        outlineStyle: style.outlineStyle,
        outlineWidth: style.outlineWidth,
        outlineColor: style.outlineColor,
        outlineOffset: style.outlineOffset,
      };
    }, keyboardStar.entryId);
    if (!keyboardFeedback || !keyboardFeedback.active || !keyboardFeedback.focusVisible) {
      fail(`390x844 the focused star did not stay the :focus-visible activeElement (${JSON.stringify(keyboardFeedback)})`);
    }
    if (keyboardFeedback.opacity !== 1) {
      fail(`390x844 keyboard focus must not be the moment a hidden star appears — resting opacity must already be 1 (got ${keyboardFeedback.opacity})`);
    }
    if (keyboardFeedback.outlineStyle !== 'solid'
      || keyboardFeedback.outlineWidth !== '1px'
      || /^(transparent|rgba\(0, 0, 0, 0\))$/.test(keyboardFeedback.outlineColor)
      || keyboardFeedback.outlineOffset !== '-1px') {
      fail(`390x844 keyboard :focus-visible must draw its real 1px accent ring (${JSON.stringify(keyboardFeedback)})`);
    }
    await page.keyboard.press('Tab');
    const expand = page.getByRole('button', { name: 'Show starred replies', exact: true });
    const expandBox = await expand.boundingBox();
    if (!expandBox || expandBox.width < 44 || expandBox.height < 44) {
      fail(`390x844 starred-replies expand target is below 44px (${JSON.stringify(expandBox)})`);
    }
    await expand.click();
    await waitFor(page, () => {
      const rail = document.querySelector('.starred-reply-minimap');
      const box = rail?.getBoundingClientRect();
      return !!rail && !rail.classList.contains('is-collapsed') && !!box && box.width > 44;
    }, '390x844 minimap did not expand');
    /* Rail row contract: one 44px row, the preview is the jump and the gold
     * star is a SEPARATE unstar. The unstar must never carry data-entry-id —
     * button[data-entry-id] is the row's jump identity snapshot. */
    const railRowBox = await page.evaluate(() => {
      const item = document.querySelector('.starred-reply-minimap__item');
      const jump = item?.querySelector('.starred-reply-minimap__jump');
      const unstar = item?.querySelector('.starred-reply-minimap__unstar');
      if (!item || !jump || !unstar) return null;
      const itemBox = item.getBoundingClientRect();
      const jumpBox = jump.getBoundingClientRect();
      const unstarBox = unstar.getBoundingClientRect();
      const glyph = unstar.querySelector('span');
      return {
        rowHeight: itemBox.height,
        jumpHeight: jumpBox.height,
        jumpEntryId: jump.getAttribute('data-entry-id'),
        jumpTitle: jump.getAttribute('title'),
        jumpLabel: jump.getAttribute('aria-label'),
        unstarWidth: unstarBox.width,
        unstarHeight: unstarBox.height,
        unstarLabel: unstar.getAttribute('aria-label'),
        unstarTitle: unstar.getAttribute('title'),
        unstarText: (unstar.textContent || '').trim(),
        unstarBorderLeftWidth: parseFloat(getComputedStyle(unstar).borderLeftWidth),
        unstarBorderLeftColor: getComputedStyle(unstar).borderLeftColor,
        unstarStamped: unstar.hasAttribute('data-entry-id'),
        unstarGlyphFontSize: glyph ? parseFloat(getComputedStyle(glyph).fontSize) : -1,
        sameRow: jump.parentElement === unstar.parentElement,
      };
    });
    if (!railRowBox) fail('390x844 rail row did not expose separate jump and unstar controls');
    if (railRowBox.rowHeight < 44 || railRowBox.jumpHeight < 44 || railRowBox.unstarWidth < 44 || railRowBox.unstarHeight < 44) {
      fail(`390x844 rail row hits are below 44px (${JSON.stringify(railRowBox)})`);
    }
    if (!railRowBox.sameRow || !railRowBox.jumpEntryId || railRowBox.jumpLabel !== `Jump to ${railRowBox.jumpTitle}`) {
      fail(`390x844 rail jump control lost its row identity (${JSON.stringify(railRowBox)})`);
    }
    if (railRowBox.unstarStamped || railRowBox.unstarLabel !== `Unstar ${railRowBox.jumpTitle}` || railRowBox.unstarTitle !== 'Unstar reply') {
      fail(`390x844 rail unstar control is mislabelled or stamped as a jump entry (${JSON.stringify(railRowBox)})`);
    }
    if (railRowBox.unstarText !== '×'
      || railRowBox.unstarBorderLeftWidth !== 1
      || /^(transparent|rgba\(0, 0, 0, 0\))$/.test(railRowBox.unstarBorderLeftColor)) {
      fail(`390x844 rail unstar is not the divided delete glyph (${JSON.stringify(railRowBox)})`);
    }
    if (railRowBox.unstarGlyphFontSize !== 16) {
      fail(`390x844 rail unstar glyph is ${railRowBox.unstarGlyphFontSize}px (must be 16px)`);
    }
    const fit = await page.evaluate(() => {
      const rect = (selector) => {
        const matches = [...document.querySelectorAll(selector)].map((el) => {
          const r = el.getBoundingClientRect();
          return { top: r.top, right: r.right, bottom: r.bottom, left: r.left, width: r.width, height: r.height };
        }).filter((box) => box.width > 0 && box.height > 0);
        return matches[0] || null;
      };
      const overlaps = (a, b) => !!a && !!b && a.left < b.right - 1 && a.right > b.left + 1 && a.top < b.bottom - 1 && a.bottom > b.top + 1;
      const header = rect('header');
      const footer = rect('.app-main > footer');
      const transcript = rect('main#transcript');
      const minimap = rect('.starred-reply-minimap');
      return {
        header,
        footer,
        transcript,
        minimap,
        headerOverlap: overlaps(minimap, header),
        footerOverlap: overlaps(minimap, footer),
        textOverlap: overlaps(minimap, transcript),
        scrollWidth: document.documentElement.scrollWidth,
        innerWidth: window.innerWidth,
        innerHeight: window.innerHeight,
      };
    });
    if (!fit.minimap || !fit.header || !fit.footer || !fit.transcript) fail(`390x844 star chrome missing (${JSON.stringify(fit)})`);
    if (fit.headerOverlap || fit.footerOverlap || fit.textOverlap) fail(`390x844 expanded minimap overlaps chrome (${JSON.stringify(fit)})`);
    if (fit.minimap.bottom > fit.innerHeight + 1 || fit.minimap.right > fit.innerWidth + 1) fail(`390x844 expanded minimap leaves the viewport (${JSON.stringify(fit.minimap)})`);
    if (fit.scrollWidth > fit.innerWidth + 1) fail(`390x844 star journey overflow ${fit.scrollWidth} > ${fit.innerWidth}`);
    await page.screenshot({ path: `${evidence}/mobile-390-star-expanded.png`, fullPage: false });
    await page.locator(`.starred-reply-minimap button.starred-reply-minimap__jump[data-entry-id="${before.entryId}"]`).click();
    await waitFor(page, (identity) => {
      const rail = document.querySelector('.starred-reply-minimap');
      const row = document.querySelector(`main#transcript [data-item-id="${identity.itemId}"]`);
      const surface = document.querySelector('main#transcript');
      if (!rail?.classList.contains('is-collapsed') || !row || !surface) return false;
      const rowBox = row.getBoundingClientRect();
      const surfaceBox = surface.getBoundingClientRect();
      return rowBox.bottom > surfaceBox.top && rowBox.top < surfaceBox.bottom && (row.querySelector('.assistant-text')?.textContent || '').includes(identity.marker);
    }, '390x844 minimap jump did not reveal its own reply and collapse', 10000, { ...before, marker: starMarker });
    await page.screenshot({ path: `${evidence}/mobile-390-star-jumped.png`, fullPage: false });
    /* The rail's ★ is a pure unstar: clicking it clears the reply's pressed
     * state and the emptied rail WITHOUT jumping the transcript. The jump
     * control stays the preview text only. */
    await expand.click();
    await waitFor(page, () => {
      const rail = document.querySelector('.starred-reply-minimap');
      return !!rail && !rail.classList.contains('is-collapsed');
    }, '390x844 minimap did not re-expand for the unstar journey');
    const unstarButtons = page.locator('.starred-reply-minimap__unstar');
    if (await unstarButtons.count() !== 1) {
      fail(`390x844 rail exposed ${await unstarButtons.count()} unstar controls for one starred reply`);
    }
    await unstarButtons.click();
    await waitFor(page, (want) => {
      const button = [...document.querySelectorAll('main#transcript button.assistant-star-toggle')]
        .find((candidate) => (candidate.getAttribute('data-entry-id') || candidate.closest('[data-entry-id]')?.getAttribute('data-entry-id')) === want.entryId);
      const surface = document.querySelector('main#transcript');
      const row = [...document.querySelectorAll('main#transcript .msg--assistant')]
        .find((candidate) => (candidate.querySelector('.assistant-text')?.textContent || '').includes(want.marker));
      const rowBox = row?.getBoundingClientRect();
      const surfaceBox = surface?.getBoundingClientRect();
      return button?.getAttribute('aria-pressed') === 'false'
        && button.getAttribute('aria-label') === 'Star'
        && document.querySelector('.starred-reply-minimap') === null
        && !!rowBox && !!surfaceBox && rowBox.bottom > surfaceBox.top && rowBox.top < surfaceBox.bottom;
    }, '390x844 rail Unstar did not clear the star, the empty rail, and keep its reply on-screen', 10000, { entryId: before.entryId, marker: starMarker });
    await page.screenshot({ path: `${evidence}/mobile-390-star-unstarred.png`, fullPage: false });
    if (browserErrors.length > 0) fail(`unexpected browser errors: ${browserErrors.join(' | ')}`);
    console.log('web-mobile: PASSED (320/375/430 two-row Send/Stop composer geometry + core flow + panel open/close + no overflow + full-screen drawer + 44px targets)');

  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-mobile: FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});
