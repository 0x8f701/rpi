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
// streaming Stop states: textarea alone on row one; Command/upload/mic/action
// together on row two; every control >=44px; no wrap or horizontal overflow.

import { chromium } from 'playwright';

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
      action: box('#send-btn'),
      footer: box('.app-main > footer'),
      row: box('#composer-row'),
      buttons: box('#composer-buttons'),
      actionLabel: document.getElementById('send-btn')?.getAttribute('aria-label') || '',
      scrollWidth: document.documentElement.scrollWidth,
      innerWidth: window.innerWidth,
      innerHeight: window.innerHeight,
    };
  });
  const controls = [geometry.command, geometry.attach, geometry.mic, geometry.action];
  if (!geometry.textarea || !geometry.footer || !geometry.row || !geometry.buttons || controls.some((box) => !box)) {
    fail(`${width}px ${state}: composer geometry target missing`);
  }
  if (geometry.scrollWidth > geometry.innerWidth + 1) {
    fail(`${width}px ${state}: horizontal overflow ${geometry.scrollWidth}px > ${geometry.innerWidth}px`);
  }
  if (geometry.footer.bottom > geometry.innerHeight + 1 || geometry.footer.left < -1 || geometry.footer.right > geometry.innerWidth + 1) {
    fail(`${width}px ${state}: composer is outside viewport (${JSON.stringify(geometry.footer)})`);
  }
  if (geometry.textarea.height < 44) fail(`${width}px ${state}: textarea height ${geometry.textarea.height}px < 44px`);
  for (const [name, box] of [['Command', geometry.command], ['upload', geometry.attach], ['mic', geometry.mic], ['action', geometry.action]]) {
    if (box.width < 44 || box.height < 44) fail(`${width}px ${state}: ${name} control is ${box.width}x${box.height}px`);
  }
  const secondTop = geometry.command.top;
  const secondBottom = geometry.command.bottom;
  for (const [name, box] of [['upload', geometry.attach], ['mic', geometry.mic], ['action', geometry.action]]) {
    if (Math.abs(box.top - secondTop) > 1 || Math.abs(box.bottom - secondBottom) > 1) {
      fail(`${width}px ${state}: ${name} wrapped off the Command toolbar row`);
    }
  }
  const yLevels = new Set([
    Math.round(geometry.textarea.top),
    Math.round(geometry.command.top),
    Math.round(geometry.attach.top),
    Math.round(geometry.mic.top),
    Math.round(geometry.action.top),
  ]);
  if (yLevels.size !== 2) {
    fail(`${width}px ${state}: composer uses ${yLevels.size} y-levels instead of exactly two (${[...yLevels].join(', ')})`);
  }
  if (geometry.textarea.bottom > secondTop - 3) {
    fail(`${width}px ${state}: textarea is not isolated on row one (textarea bottom ${geometry.textarea.bottom}, toolbar top ${secondTop})`);
  }
  if (geometry.command.left < geometry.row.left - 1 || geometry.action.right > geometry.row.right + 1) {
    fail(`${width}px ${state}: toolbar escapes composer row bounds`);
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
    page.on('pageerror', (err) => {
      console.error(`web-mobile: page error: ${err.message}`);
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
    // F5: the header hamburger must be VISIBLE at phone width (the media-query
    // rule must win the cascade — an unconditional display:none after the
    // media block hid it at every width) and clicking it must open the
    // session sidebar drawer.
    const toggleDisplay = await page.evaluate(
      () => getComputedStyle(document.getElementById('sidebar-toggle-btn')).display
    );
    if (toggleDisplay === 'none') {
      fail('#sidebar-toggle-btn is hidden at phone width (CSS cascade regression)');
    }
    const toggleBox = await page.locator('#sidebar-toggle-btn').boundingBox();
    if (!toggleBox || toggleBox.width < 20 || toggleBox.height < 20) {
      fail(`#sidebar-toggle-btn has no clickable area at phone width (box ${JSON.stringify(toggleBox)})`);
    }
    await page.click('#sidebar-toggle-btn');
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
      'session sidebar drawer never opened from the hamburger'
    );
    const sessionsTitle = await page.evaluate(() => {
      const sidebar = document.querySelector('.session-sidebar');
      const title = document.querySelector('.session-sidebar__title');
      if (!sidebar || !title) return null;
      const sr = sidebar.getBoundingClientRect();
      const tr = title.getBoundingClientRect();
      return {
        text: title.textContent,
        leftInset: tr.left - sr.left,
        topInset: tr.top - sr.top,
        right: tr.right,
        bottom: tr.bottom,
        sidebarRight: sr.right,
        sidebarBottom: sr.bottom,
      };
    });
    if (!sessionsTitle || sessionsTitle.text?.toLowerCase() !== 'sessions') {
      fail(`mobile Sessions title missing: ${JSON.stringify(sessionsTitle)}`);
    }
    if (sessionsTitle.leftInset < 8 || sessionsTitle.topInset < 8 || sessionsTitle.right > sessionsTitle.sidebarRight || sessionsTitle.bottom > sessionsTitle.sidebarBottom) {
      fail(`mobile Sessions title is clipped against the drawer edge: ${JSON.stringify(sessionsTitle)}`);
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
    // The panel toggles now live IN the drawer (feature nav), so open the
    // unified Todo module with its task and child sections together.
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
    await page.click('#sidebar-toggle-btn');
    await waitFor(
      page,
      () => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true,
      'session sidebar did not reopen after panel close'
    );

    // Goal + Workflows open/close with the same shell contract (no overflow,
    // close >= 44, drawer collapses on open).
    for (const { toggle, panel, close } of [
      { toggle: '#goal-panel-btn', panel: '#goal-panel', close: '#goal-close-btn' },
      { toggle: '#workflow-toggle-btn', panel: '#workflow-panel', close: '#workflow-close-btn' },
    ]) {
      const open = await page.evaluate(
        () => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true
      );
      if (!open) {
        await page.click('#sidebar-toggle-btn');
        await waitFor(
          page,
          () => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true,
          `sidebar open before ${panel}`
        );
      }
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
    // Re-open the drawer so the feature-nav touch targets are on-screen.
    const drawerOpen = await page.evaluate(
      () => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true
    );
    if (!drawerOpen) {
      await page.click('#sidebar-toggle-btn');
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
    // Feature-nav touch targets are measured WHILE THE DRAWER IS OPEN: a
    // closed drawer is translated off-screen (translateX(-110%)), so any
    // hidden/occluded/offscreen target fails the geometry checks below instead
    // of passing a pure CSS-height probe of invisible buttons.
    const navTargets = await page.evaluate(() => {
      const rect = (sel) => {
        const el = document.querySelector(sel);
        if (!el) return null;
        const r = el.getBoundingClientRect();
        return { top: r.top, bottom: r.bottom, left: r.left, right: r.right, width: r.width, height: r.height };
      };
      const ids = [
        '#send-btn',
        '#settings-toggle-btn',
        '#todo-toggle-btn',
        '#goal-panel-btn',
        '#workflow-toggle-btn',
        '.session-sidebar__group-head',
        '.session-sidebar__switch',
      ];
      return {
        innerWidth: window.innerWidth,
        innerHeight: window.innerHeight,
        targets: ids.map((sel) => {
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
    });
    for (const t of navTargets.targets) {
      if (t.height < 44) fail(`touch target ${t.sel} is ${t.height}px (must be >= 44px)`);
      // Structurally fail hidden/offscreen targets: with the drawer open every
      // feature-nav button must sit inside the viewport horizontally (the
      // closed drawer's translateX(-110%) pushes right edge < 0) and not be
      // clipped above/below the viewport.
      if (t.left < -1 || t.right > navTargets.innerWidth + 1) {
        fail(`touch target ${t.sel} is off-screen in the open drawer: left ${t.left}, right ${t.right} (viewport ${navTargets.innerWidth}px)`);
      }
      if (t.top > navTargets.innerHeight || t.bottom < 0) {
        fail(`touch target ${t.sel} is off-screen vertically: top ${t.top}, bottom ${t.bottom} (viewport ${navTargets.innerHeight}px)`);
      }
    }
    // Open the unified Todo module from the open drawer; opening a panel
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
    console.log('web-mobile: PASSED (320/375/430 two-row Send/Stop composer geometry + core flow + panel open/close + no overflow + full-screen drawer + 44px targets)');

  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-mobile: FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});
