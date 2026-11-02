// Sidebar context-menu rename regression — PLAYWRIGHT-ONLY.
//
// Guards the fixed P0 regression: right-click a SessionSidebar row -> Rename,
// then natural (keyboard-event) multi-character typing must accumulate; the
// controlled value must never be re-selected after a keystroke. This is the
// sidebar PORTAL rename input (`.session-sidebar__menu-rename`), which the
// session_ux SUX1.2 lane does NOT cover (that lane exercises the Session
// panel `#session-rename-input`).
//
// Deterministic assertions:
//   SBR1.1  rename entry focuses the input once and selects the full title
//   SBR1.2  typing 'a','b','c' char-by-char accumulates to 'abc'; every
//           keystroke fires exactly one input event and ZERO select/focus
//           calls (the old bug re-ran a [menu]-keyed layout effect per
//           onChange, focus()+select()-ing the whole value each character)
//   SBR1.3  Enter sends exactly one set_session_name frame carrying the full
//           typed name and the captured sessionPath
//
// Instrumentation wraps HTMLInputElement.select / HTMLElement.focus and
// records per-character DOM value + selectionStart/End + select/focus call
// deltas, so the trace proves the mechanism, not just the outcome.
//
// Evidence (all names synthetic, no session paths/tokens):
//   rename-0-sidebar.png   sidebar rows before the menu
//   rename-1-menu.png      root context menu open
//   rename-2-open.png      rename input focused with the name preselected
//   rename-3-char-1.png    after typing 'a'
//   rename-3-char-2.png    after typing 'ab'
//   rename-3-char-3.png    after typing 'abc' (full multi-char value)
//   rename-5-typed.png     full value right before Enter
//   rename-6-final.png     final state after commit
//   rename-trace.json      full per-character trace + select/focus timeline
//
// Exit codes mirror the web-lane convention: 0 = regression PASS (typing
// accumulates, exactly one entry select), 2 = deterministic assertion
// failure (regression: character loss or per-keystroke re-select).

import { chromium } from 'playwright';
import fs from 'node:fs';
import path from 'node:path';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';

const SEED_SID = 'sidebar-rename-seed';
const TYPED = 'abc';

function fail(message) {
  console.error(`sidebar-menu-rename: FAIL: ${message}`);
  process.exit(2);
}

async function waitFor(page, fn, label, timeoutMs = 30000, arg, diagnose) {
  try {
    await page.waitForFunction(fn, arg, { timeout: timeoutMs });
  } catch {
    const detail = diagnose ? await page.evaluate(diagnose, arg).catch(() => null) : null;
    fail(`${label} (timeout ${timeoutMs}ms${detail === null ? '' : `; state ${JSON.stringify(detail)}`})`);
  }
}

async function snapshot(page) {
  return page.evaluate(() => {
    const input = document.querySelector('.session-sidebar__menu-rename');
    const c = window.__rpiRenameRepro;
    return {
      value: input ? input.value : null,
      selStart: input && input.value !== null ? input.selectionStart : null,
      selEnd: input && input.value !== null ? input.selectionEnd : null,
      focused: !!input && document.activeElement === input,
      menuOpen: document.querySelector('.session-sidebar__menu') !== null,
      selectCalls: c.selectCalls.length,
      focusCalls: c.focusCalls.length,
      inputEvents: c.inputs.length,
    };
  });
}

async function main() {
  if (!url) fail('RPI_URL is required');
  fs.mkdirSync(evidence, { recursive: true });

  const launchOptions = chromePath ? { executablePath: chromePath } : {};
  const browser = await chromium.launch(launchOptions);
  try {
    const context = await browser.newContext({ viewport: { width: 1280, height: 800 } });
    const page = await context.newPage();
    if (token) {
      await page.addInitScript((t) => {
        window.localStorage.setItem('rpi-web-token', t);
      }, token);
    }
    page.on('pageerror', (err) => console.error(`sidebar-menu-rename: pageerror: ${err.message}`));

    // Instrumentation installed BEFORE any app script runs:
    //  - wrap HTMLInputElement.prototype.select and HTMLElement.prototype.focus
    //    to record every call targeting an input (value + selection at call time)
    //  - record every 'input' event (capture) with value + selection
    //  - wrap WebSocket.send to capture the exact set_session_name frame
    await page.addInitScript(() => {
      const control = { selectCalls: [], focusCalls: [], inputs: [], frames: [] };
      window.__rpiRenameRepro = control;

      const nativeSelect = HTMLInputElement.prototype.select;
      HTMLInputElement.prototype.select = function (...args) {
        control.selectCalls.push({
          t: Math.round(performance.now()),
          cls: this.className,
          value: this.value,
          selStart: this.selectionStart,
          selEnd: this.selectionEnd,
          focused: document.activeElement === this,
        });
        return nativeSelect.apply(this, args);
      };
      const nativeFocus = HTMLElement.prototype.focus;
      HTMLElement.prototype.focus = function (...args) {
        if (this instanceof HTMLInputElement && this.className) {
          control.focusCalls.push({
            t: Math.round(performance.now()),
            cls: this.className,
            value: this.value,
            selStart: this.selectionStart,
            selEnd: this.selectionEnd,
          });
        }
        return nativeFocus.apply(this, args);
      };
      document.addEventListener('input', (event) => {
        const t = event.target;
        if (t instanceof HTMLInputElement) {
          control.inputs.push({
            t: Math.round(performance.now()),
            cls: t.className,
            value: t.value,
            selStart: t.selectionStart,
            selEnd: t.selectionEnd,
          });
        }
      }, true);

      const NativeWebSocket = window.WebSocket;
      class ReproWebSocket extends NativeWebSocket {
        send(data) {
          if (typeof data === 'string') {
            try {
              const frame = JSON.parse(data);
              if (frame && typeof frame.type === 'string') {
                control.frames.push({ t: Date.now(), frame });
                if (control.frames.length > 500) control.frames.shift();
              }
            } catch { /* non-JSON passes through */ }
          }
          return super.send(data);
        }
      }
      for (const key of ['CONNECTING', 'OPEN', 'CLOSING', 'CLOSED']) {
        Object.defineProperty(ReproWebSocket, key, { value: NativeWebSocket[key] });
      }
      window.WebSocket = ReproWebSocket;
    });

    await page.goto(url, { waitUntil: 'domcontentloaded' });

    // Boot: the seeded synthetic native row is listed.
    await waitFor(
      page,
      (sid) => [...document.querySelectorAll('.session-sidebar__switch')]
        .some((r) => r.dataset.sessionId === sid),
      'seeded sidebar row never appeared after boot',
      30000,
      SEED_SID,
      () => [...document.querySelectorAll('.session-sidebar__switch')].map((r) => r.dataset.sessionId),
    );
    await page.screenshot({ path: path.join(evidence, 'rename-0-sidebar.png'), fullPage: true });

    // Right-click the row -> root context menu.
    const row = page.locator(`.session-sidebar__switch[data-session-id="${SEED_SID}"]`);
    await row.click({ button: 'right' });
    await waitFor(page, () => document.querySelector('.session-sidebar__menu') !== null,
      'context menu never opened');
    await page.screenshot({ path: path.join(evidence, 'rename-1-menu.png'), fullPage: true });

    // Click Rename -> portal rename input.
    const clicked = await page.evaluate(() => {
      const items = [...document.querySelectorAll('.session-sidebar__menu [role="menuitem"]')];
      const item = items.find((el) => (el.textContent || '').trim() === 'Rename' && el.disabled !== true);
      if (!item) return false;
      item.click();
      return true;
    });
    if (!clicked) fail('menu item Rename missing or disabled');
    await waitFor(page, () => document.querySelector('.session-sidebar__menu-rename') !== null,
      'rename input never appeared');
    await page.waitForTimeout(150); // let the focus/select layout effect settle
    const initial = await snapshot(page);
    await page.screenshot({ path: path.join(evidence, 'rename-2-open.png'), fullPage: true });
    console.log(`[sidebar-menu-rename] initial: ${JSON.stringify(initial)}`);
    // SBR1.1 — entry focuses the input once and selects the full title.
    if (!initial.menuOpen) fail('SBR1.1: rename menu not open at entry');
    if (!initial.focused) fail('SBR1.1: rename input not focused at entry');
    if (initial.selectCalls < 1) fail('SBR1.1: rename entry never called select()');
    if (initial.selStart !== 0 || initial.selEnd !== initial.value.length) {
      fail(`SBR1.1: rename entry selection is [${initial.selStart},${initial.selEnd}) of '${initial.value}' (expected full range)`);
    }

    // SBR1.2 — deterministic per-character accumulation assertion.
    const perChar = [];
    let typed = '';
    let failure = null;
    for (const ch of TYPED) {
      const before = await snapshot(page);
      await page.keyboard.type(ch, { delay: 20 });
      typed += ch;
      await page.waitForTimeout(150); // React commit + layout effect settle
      const snap = await snapshot(page);
      perChar.push({
        typed,
        value: snap.value,
        selStart: snap.selStart,
        selEnd: snap.selEnd,
        focused: snap.focused,
        menuOpen: snap.menuOpen,
        selectCallsDelta: snap.selectCalls - before.selectCalls,
        focusCallsDelta: snap.focusCalls - before.focusCalls,
        inputEventsDelta: snap.inputEvents - before.inputEvents,
      });
      console.log(`[sidebar-menu-rename] after '${ch}': ${JSON.stringify(perChar[perChar.length - 1])}`);
      const shot = `rename-3-char-${typed.length}.png`;
      await page.screenshot({ path: path.join(evidence, shot), fullPage: true });
      if (snap.focused !== true) {
        failure = `SBR1.2: rename input lost focus while typing '${typed}'`;
        break;
      }
      if (snap.selectCalls - before.selectCalls !== 0) {
        failure = `SBR1.2: select() called ${snap.selectCalls - before.selectCalls}x while typing '${typed}' (expected 0 — value was re-selected, next keystroke would replace it)`;
        break;
      }
      if (snap.focusCalls - before.focusCalls !== 0) {
        failure = `SBR1.2: focus() called ${snap.focusCalls - before.focusCalls}x while typing '${typed}' (expected 0)`;
        break;
      }
      if (snap.inputEvents - before.inputEvents !== 1) {
        failure = `SBR1.2: typing '${ch}' produced ${snap.inputEvents - before.inputEvents} input events (expected 1)`;
        break;
      }
      if (snap.value !== typed) {
        failure = `SBR1.2: rename input did not accumulate '${typed}' (value '${snap.value}')`;
        break;
      }
    }

    // Enter-commit evidence (only meaningful when typing accumulated): Enter
    // must send exactly one set_session_name frame carrying the full typed name.
    let commit = null;
    if (!failure) {
      // Full typed value on screen right before commit (after screenshot).
      await page.screenshot({ path: path.join(evidence, 'rename-5-typed.png'), fullPage: true });
      await page.keyboard.press('Enter');
      try {
        await page.waitForFunction(
          () => (window.__rpiRenameRepro.frames || [])
            .some((e) => e.frame.type === 'set_session_name'),
          undefined,
          { timeout: 10000 },
        );
        const frames = await page.evaluate(() =>
          (window.__rpiRenameRepro.frames || [])
            .filter((e) => e.frame.type === 'set_session_name')
            .map((e) => e.frame),
        );
        if (frames.length !== 1) {
          failure = `SBR1.3: Enter sent ${frames.length} set_session_name frames (expected exactly 1)`;
        } else {
          commit = frames[0];
          if (commit.name !== TYPED) {
            failure = `SBR1.3: Enter sent set_session_name name='${commit.name}' (expected '${TYPED}')`;
          } else if (typeof commit.sessionPath !== 'string' || commit.sessionPath.length === 0) {
            failure = "SBR1.3: Enter sent set_session_name without the captured sessionPath";
          }
        }
      } catch {
        failure = 'SBR1.3: Enter never sent a set_session_name frame';
      }
      await page.screenshot({ path: path.join(evidence, 'rename-6-final.png'), fullPage: true });
    } else {
      await page.screenshot({ path: path.join(evidence, 'rename-6-final.png'), fullPage: true });
    }

    // Evidence trace (bounded timelines; synthetic names only).
    const trace = await page.evaluate(() => {
      const c = window.__rpiRenameRepro;
      return {
        selectCalls: c.selectCalls.slice(-60),
        focusCalls: c.focusCalls.slice(-30),
        inputs: c.inputs.slice(-30),
        frames: c.frames.filter((e) => e.frame.type === 'set_session_name'),
      };
    });
    fs.writeFileSync(
      path.join(evidence, 'rename-trace.json'),
      JSON.stringify({ typed: TYPED, initial, perChar, commit, ...trace }, null, 2),
    );

    if (failure) fail(failure);
    console.log('[sidebar-menu-rename] regression PASS: every character accumulated, zero select/focus calls while typing, one exact set_session_name commit');
  } finally {
    await browser.close();
  }
}

main().catch((err) => fail(err && err.stack ? err.stack : String(err)));