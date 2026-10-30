// Web code-review tree/paging/comment-markdown regression lane (playwright
// half of E2E.d/web/code_review_paging.sh).
//
// Environment:
//   RPI_URL          http://127.0.0.1:<port>/web
//   RPI_TOKEN        token file content (served via rpi-auth.<token> subprotocol)
//   RPI_DIRTY_FILE   tracked file modified in the working tree ("greet.txt")
//   RPI_BIG_FILE     >4000-line paging fixture: complete body, eager-fetched
//                    to terminal, DOM window grown by scroll
//   RPI_UICAP_FILE   >20000-line diff fixture whose window pins at the UI cap
//   RPI_BYTECAP_FILE tracked-modified fixture whose diff exceeds the backend
//                    8 MiB per-file cap (terminal page byteCapped=true)
//   RPI_EVIDENCE     evidence dir for screenshots
//   RPI_OPEN_DELAY_MS how long the first code_review_open response after the
//                     WebSocket wrapper is installed is withheld (>30s, the
//                     old fast-ack horizon that used to close the OPEN socket
//                     with a 4001 transport-stale close); default 35000
//
// Assertions:
//   T1  the review file list is a NESTED tree: directory rows
//       li.code-review__tree-row[data-tree-kind="dir"] with
//       button.code-review__tree-dir + aria-expanded (on the li) wrap the
//       fixture's nested files (src/other.rs + src/deep/feature.rs)
//   T2  clicking a directory row collapses it: aria-expanded flips to false
//       and its child FILE rows disappear from the visible list; clicking
//       again expands and the children reappear
//   T9  the loaded placeholder's hunk (which originates ENTIRELY beyond the
//       global truncation) is selectable and commentable: clicking its
//       header, typing a comment and pressing Enter resolves the loaded
//       hunk identity and renders the accepted thread card
//   T8  rust diff lines (src/other.rs) render hljs token spans with verbatim
//       textContent, unchanged line numbers/prefix/kind backgrounds, and a
//       hostile <script> diff line stays LITERAL text with no side effect
//   C1  a submitted comment carrying **bold**, a list, a ```rust``` fence,
//       and hostile <script>/<img onerror> HTML renders markdown (strong,
//       ul>li, pre.md-fence__pre > code.hljs with the .md-fence__lang label)
//       while the hostile HTML stays LITERAL text (textContent carries
//       "<script>", no element created, window.__crPwned undefined, no
//       dialog)
//   C2  the ASSISTANT review reply (mock-routed markdown matrix) renders the
//       same markdown + literal-hostile contract in its own comment
//   L1  /code-review HEAD (single base) sends code_review_open with
//       from=HEAD, includeWorkingTree=true, and no `to` key
//       (!Object.hasOwn(frame, 'to')); the comparison label is HEAD →
//       working tree
//   L2  a comment on src/deep/feature.rs routed through the mock
//       `cr grep probe` tool path shows the header streaming-dot +
//       Abort while the reply is live; after the tool follow-up the
//       thread is terminal: no Abort, no .code-review__streaming, no
//       .code-review__busy, composer enabled, the user comment still
//       present, exactly one assistant reply whose text includes the
//       fixture grep needle + matched path (src/deep/feature.rs) from
//       the RAW tool result — a stub/fail grep fails the lane
//   L3  a genuine live code_review_snapshot poll captured while the
//       reply was in-flight is HELD unchanged (old revision/comments/
//       active fields, request id + send/capture times tracked) and
//       delivered only AFTER terminal; delivery FAILS if the response
//       could be expired by the 30s pending timeout, and the App's
//       pending settlement (timeout-timer clear) proves the response
//       was consumed — not just shim-delivered; overlay stays idle
//       (no resurrected Abort/dot/busy)
//   L4  D7 reconnect recovery stays terminal-idle (no Abort/dot/busy)
//   T6  responsive breakpoint + mobile tab bar, driven while the panel stays
//       mounted: resizing the viewport across the 900px breakpoint fires the
//       matchMedia change handler (Files/Diff/Thread tab bar appears), each
//       tab click drives the single-pane narrow UI (active tab flips, only
//       the active pane is visible), Back-to-diff returns from the Thread
//       pane, and resizing back to desktop removes the tab bar
//   D1  a second /code-review open, whose FIRST code_review_open response is
//       withheld RPI_OPEN_DELAY_MS (>30s — past the old fast-ack horizon that
//       used to classify the still-OPEN socket as transport-stale and close
//       it with a 4001), does NOT tear down the transport: the WebSocket
//       generation observed at open time stays constant across the whole
//       delay with ZERO closes on the wrapper
//   D2  an unrelated get_state (with the active sessionId) round-trips
//       mid-delay (~5s in) — conn stays "on", generation unchanged
//   D3  the heartbeat liveness probe works during the delay: a JSON pong
//       (from the App's own `{type:"ping"}` probe) arrives before the open
//       settles, so the socket was provably exchanging frames while the
//       open was pending
//   D4  the same get_state round-trips again past 30s (the old transport-
//       stale deadline) on the SAME generation — no close, no reconnect
//   D5  once the delayed response lands, the panel settles with the
//       HEAD→working tree snapshot and the loader clears — with no transport
//       toast, no inline `.code-review__error`, and no "connection closed"
//       anywhere
//   D6  a REAL abnormal close — the lane KILLS the rpi listener process so
//       the transport is severed at the socket level with NO WebSocket.close
//       call; the close handshake can never complete and Chromium reports the
//       genuine abnormal-closure code 1006 — surfaces EXACTLY one transport
//       toast and exactly one close record with code === 1006 on the killed
//       generation; the native abnormal-close path is never loosened, and the
//       panel/transcript surfaces stay mounted
//   D7  the product reconnects on its own: the lane respawns the listener on
//       the SAME port (--resume the pre-outage primary session) while the
//       shim fails three product reconnect attempts synchronously before any
//       TCP socket opens (so no native ERR_CONNECTION_REFUSED is
//       manufactured); generation advances past the killed one, the socket
//       returns to OPEN, the panel is still mounted and its snapshot probe
//       reconciles the retained controller (same comparison label) with no
//       inline/sidebar error and no extra toasts
//   D8  the ENTIRE segment (delay + kill + recovery) runs with ZERO browser
//       console errors and ZERO page errors (favicon noise filtered) —
//       native ERR_CONNECTION_REFUSED handling is never loosened because
//       the kill is a real 1006, not a refused connect

import { chromium } from 'playwright';
import fs from 'node:fs';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const dirtyFile = process.env.RPI_DIRTY_FILE || 'greet.txt';
const bigFile = process.env.RPI_BIG_FILE || 'big.txt';
const uicapFile = process.env.RPI_UICAP_FILE || 'uicap.txt';
const bytecapFile = process.env.RPI_BYTECAP_FILE || 'zz-bytecap.txt';
const expectedTotals = process.env.RPI_EXPECTED_TOTALS || '';
const navFile = process.env.RPI_NAV_FILE || 'nav.txt';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';
// Marker/session-id files live in the lane work dir (RPI_WORK, defaulting to
// the evidence dir) so the lane script can orchestrate the real listener
// kill + same-port respawn (the reconnect lane's proven handshake).
const work = process.env.RPI_WORK || evidence;
const OPEN_DELAY_MS = Number(process.env.RPI_OPEN_DELAY_MS || 35000);

function fail(message) {
  console.error(`web-code-review-paging: FAIL: ${message}`);
  process.exit(2);
}

const socketFrames = { sent: [], received: [] };

async function waitFor(page, fn, label, timeoutMs = 25000, arg) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await page.evaluate(fn, arg)) return;
    await page.waitForTimeout(120);
  }
  fail(`${label} (timeout ${timeoutMs}ms)`);
}

/** Best-effort JSON decode of a raw WS frame payload; null for non-JSON. */
function parseFrame(raw) {
  try {
    return JSON.parse(raw);
  } catch {
    return null;
  }
}
async function waitForNode(fn, label, timeoutMs = 25000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (fn()) return;
    await new Promise((resolve) => setTimeout(resolve, 120));
  }
  fail(`${label} (timeout ${timeoutMs}ms)`);
}

function fileDiffCommands() {
  return socketFrames.sent
    .map((f) => parseFrame(f.raw))
    .filter((f) => f && f.type === 'code_review_file_diff');
}

function fileDiffResponses() {
  return socketFrames.received
    .map((f) => parseFrame(f.raw))
    .filter((f) => f && f.type === 'response' && f.data
      && typeof f.data.path === 'string' && Array.isArray(f.data.hunks));
}

function assertPageChain(path, totalLines, pageSize = 1000) {
  const cmds = fileDiffCommands().filter((f) => f.path === path);
  const expectedCount = Math.ceil(totalLines / pageSize);
  if (cmds.length !== expectedCount) {
    fail(`paging chain: expected ${expectedCount} code_review_file_diff pages for ${path}, got ${cmds.length}`);
  }
  const responses = new Map(
    fileDiffResponses().filter((r) => r.data.path === path).map((r) => [r.id, r.data]),
  );
  for (let i = 0; i < cmds.length; i++) {
    const cmd = cmds[i];
    const wantCursor = i * pageSize;
    if ((cmd.cursor ?? 0) !== wantCursor) {
      fail(`paging chain: ${path} page ${i} requested cursor ${cmd.cursor}, expected ${wantCursor}`);
    }
    if (cmd.maxLines !== pageSize) {
      fail(`paging chain: ${path} page ${i} requested maxLines ${cmd.maxLines}, expected ${pageSize}`);
    }
    const page = responses.get(cmd.id);
    if (!page) fail(`paging chain: no response for ${path} page ${i} (cursor ${wantCursor})`);
    if (page.cursor !== wantCursor) {
      fail(`paging chain: ${path} page ${i} response cursor ${page.cursor} != request ${wantCursor}`);
    }
    if (page.totalLines !== totalLines) {
      fail(`paging chain: ${path} page ${i} totalLines ${page.totalLines} != ${totalLines}`);
    }
    const terminal = i === cmds.length - 1;
    if (terminal) {
      if (page.hasMore !== false) {
        fail(`paging chain: ${path} terminal page must report hasMore=false (got ${page.hasMore})`);
      }
      if (page.nextCursor !== undefined) {
        fail(`paging chain: ${path} terminal page must not carry nextCursor (got ${page.nextCursor})`);
      }
    } else if (page.hasMore !== true || page.nextCursor !== wantCursor + pageSize) {
      fail(`paging chain: ${path} page ${i} must continue at ${wantCursor + pageSize} (got hasMore=${page.hasMore}, nextCursor=${page.nextCursor})`);
    }
  }
}
function assertByteCapChain(path) {
  const cmds = fileDiffCommands().filter((f) => f.path === path);
  if (cmds.length === 0) fail(`byte-cap chain: no code_review_file_diff pages observed for ${path}`);
  const responses = new Map(
    fileDiffResponses().filter((r) => r.data.path === path).map((r) => [r.id, r.data]),
  );
  let wantCursor = 0;
  for (let i = 0; i < cmds.length; i++) {
    const cmd = cmds[i];
    if ((cmd.cursor ?? 0) !== wantCursor) {
      fail(`byte-cap chain: ${path} page ${i} requested cursor ${cmd.cursor}, expected ${wantCursor}`);
    }
    if (cmd.maxLines !== 1000) {
      fail(`byte-cap chain: ${path} page ${i} requested maxLines ${cmd.maxLines}, expected 1000`);
    }
    const page = responses.get(cmd.id);
    if (!page) fail(`byte-cap chain: no response for ${path} page ${i} (cursor ${wantCursor})`);
    if (page.cursor !== wantCursor) {
      fail(`byte-cap chain: ${path} page ${i} response cursor ${page.cursor} != request ${wantCursor}`);
    }
    if (page.byteCapped !== true) {
      fail(`byte-cap chain: ${path} page ${i} must carry byteCapped=true (backend per-file cap)`);
    }
    const terminal = i === cmds.length - 1;
    if (terminal) {
      if (page.hasMore !== false) {
        fail(`byte-cap chain: ${path} terminal page must report hasMore=false (got ${page.hasMore})`);
      }
      if (page.nextCursor !== undefined) {
        fail(`byte-cap chain: ${path} terminal page must not carry nextCursor (got ${page.nextCursor})`);
      }
    } else {
      if (page.hasMore !== true || page.nextCursor === undefined) {
        fail(`byte-cap chain: ${path} page ${i} must continue (got hasMore=${page.hasMore}, nextCursor=${page.nextCursor})`);
      }
      wantCursor = page.nextCursor;

    }
  }
}
async function scrollDiffToBottom(page) {
  await page.evaluate(() => {
    const el = document.querySelector('.code-review__diff:not(.is-hidden)');
    if (!el) throw new Error('diff viewport not found');
    el.scrollTop = el.scrollHeight;
  });
}
async function assertPendingOpenUnmounted(page, label) {
  const mounted = await page.evaluate(() => document.getElementById('code-review-panel') !== null);
  if (mounted) fail(`${label}: code-review panel mounted before the delayed code_review_open settled`);
}

async function assertEntryLoader(page, label) {
  const state = await page.evaluate(() => {
    const loader = document.querySelector('.code-review__loader');
    if (!loader) return null;
    const rect = loader.getBoundingClientRect();
    const style = getComputedStyle(loader);
    const activeBar = getComputedStyle(loader, '::after');
    return {
      width: rect.width,
      height: rect.height,
      borderRadius: style.borderRadius,
      animationName: activeBar.animationName,
      hasCircularSpinner: loader.querySelector('.code-review__spinner') !== null,
      withinViewport:
        rect.left >= 0
        && rect.right <= window.innerWidth
        && rect.top >= 0
        && rect.bottom <= window.innerHeight,
    };
  });
  if (!state) fail(`${label}: entry loader is absent while code_review_open is pending`);
  if (
    state.width < 240
    || state.height < 6
    || state.width / state.height < 20
    || state.borderRadius !== '0px'
  ) {
    fail(`${label}: entry loader is not a visible flat bar: ${JSON.stringify(state)}`);
  }
  if (state.animationName !== 'code-review-loader-slide') {
    fail(`${label}: entry loader has no active loading animation: ${JSON.stringify(state)}`);
  }
  if (state.hasCircularSpinner || !state.withinViewport) {
    fail(`${label}: entry loader retained the tiny spinner or overflowed: ${JSON.stringify(state)}`);
  }
}




/** Poll a marker file until it exists (lane-script handshake) or fail. */
async function waitForFile(file, label, timeoutMs = 60000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (fs.existsSync(file)) return;
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  fail(`${label} (timeout ${timeoutMs}ms: ${file})`);
}

/** Click a file row by its full repo-relative path (data-file-path). */
async function clickFile(page, want) {
  const clicked = await page.evaluate((pathText) => {
    const btn = Array.from(document.querySelectorAll('.code-review__file')).find((b) =>
      b.getAttribute('data-file-path') === pathText
    );
    if (!btn) return false;
    btn.click();
    return true;
  }, want);
  if (!clicked) fail(`file row ${want} not found for click`);
}

/** Directory rows as {id, expanded}. aria-expanded lives on the li. */
const dirRows = () => {
  const rows = Array.from(document.querySelectorAll('li.code-review__tree-row[data-tree-kind="dir"]'));
  return rows.map((li) => ({
    id: li.getAttribute('data-tree-id') || '',
    expanded: li.getAttribute('aria-expanded'),
  }));
};

/** Count of FILE rows that are actually visible (not inside a collapsed dir). */
const visibleFileRows = () => {
  const btns = Array.from(document.querySelectorAll('button.code-review__file'));
  return btns.filter((btn) => {
    const style = window.getComputedStyle(btn);
    return style.display !== 'none' && style.visibility !== 'hidden' && btn.offsetParent !== null;
  }).length;
};


async function main() {
  if (!url) fail('RPI_URL is required');
  const launchOptions = chromePath ? { executablePath: chromePath } : {};
  const browser = await chromium.launch(launchOptions);
  try {
    const page = await browser.newPage();
    if (token) {
      await page.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
    }
    let dialogs = 0;
    page.on('dialog', (dialog) => {
      dialogs += 1;
      dialog.dismiss().catch(() => {});
    });
    const pageErrors = [];
    const consoleErrors = [];
    page.on('pageerror', (err) => {
      pageErrors.push(String(err && err.message ? err.message : err));
      console.error(`web-code-review-paging: page error: ${err.message}`);
    });
    page.on('console', (msg) => {
      // Native console errors from the browser are contract failures here.
      // The kill in the delayed-open segment is a REAL 1006 close, never a
      // refused connect, so this policy never needs to loosen native
      // ERR_CONNECTION_REFUSED handling (the reconnect lane's seam for that
      // case is a separate fixture).
      if (msg.type() === 'error') consoleErrors.push(msg.text());
    });
    // Raw WS frame capture for the delayed-open segment (D1-D7): records
    // the JSON pong the heartbeat probe provokes and the code_review_open
    // command, independent of the in-page wrapper's view.
    page.on('websocket', (socket) => {
      socket.on('framesent', (frame) => {
        if (typeof frame.payload !== 'string') return;
        socketFrames.sent.push({ raw: frame.payload, at: Date.now() });
      });
      socket.on('framereceived', (frame) => {
        if (typeof frame.payload !== 'string') return;
        socketFrames.received.push({ raw: frame.payload, at: Date.now() });
      });
    });
    // Delayed-open observer (D1-D8): wraps EVERY WebSocket from page load so
    // the bootstrap socket itself is observable. Records generation/closes/
    // JSON pongs and hands the live socket to __crDelayTest.arm(), which
    // withholds the first code_review_open response by RPI_OPEN_DELAY_MS
    // (>30s — past the old fast-ack horizon that used to classify the still-
    // OPEN socket as transport-stale and close it with a 4001). Arming is
    // deferred to the D segment so the T1-T6 opens are never delayed.
    await page.addInitScript(() => {
      const NativeWebSocket = window.WebSocket;
      // Pending-timer observation for the L3 consumption proof: the App's
      // PendingRegistry (crates/pi-cli/web/src/pending.ts) arms each command's
      // bounded timeout via window.setTimeout and clears it on settlement
      // (PendingRegistry.take). Wrapping both lets the shim prove the App
      // actually CONSUMED a held code_review_snapshot response: the request's
      // pending timer is armed synchronously before its ws.send, and take()
      // clears it synchronously during delivery.
      const NativeSetTimeout = window.setTimeout.bind(window);
      const NativeClearTimeout = window.clearTimeout.bind(window);
      let lastArmedTimer = null;
      const clearedTimers = new Set();
      window.setTimeout = function (fn, ms, ...args) {
        const timer = NativeSetTimeout(fn, ms, ...args);
        lastArmedTimer = timer;
        return timer;
      };
      window.clearTimeout = function (timer) {
        if (timer != null) clearedTimers.add(timer);
        return NativeClearTimeout(timer);
      };
      const state = {
        generation: 0,
        closes: [],
        pongs: 0,
        delayedOpenIds: [],
        ws: null,
        armed: false,
        delayMs: 0,
        attempts: 0,
        failedAttempts: [],
        liveSnapshotQueue: [],
        delayedLiveRaw: [],
        heldSnapshots: [],
        snapshotSentAt: {},
        pendingTimerFor: {},
        clearedTimers,
        captureLiveSnapshots: false,
        replayedLiveSnapshot: false,
        liveCaptureArmed: false,
        liveOrigOnMessage: null,
      };
      let failuresRemaining = 0;
      class ObservedWebSocket extends NativeWebSocket {
        constructor(...args) {
          state.attempts += 1;
          // Real listener outage (D6): the next product reconnect attempts
          // fail synchronously in the constructor BEFORE Chromium opens a TCP
          // socket, so the respawning listener on the same port is never hit
          // by a refused connect while it is down (keeps the strict
          // zero-console-error policy intact — no native
          // ERR_CONNECTION_REFUSED is manufactured, matching the reconnect
          // lane's seam).
          if (failuresRemaining > 0) {
            failuresRemaining -= 1;
            state.failedAttempts.push(state.attempts);
            throw new Error('web-code-review-paging outage');
          }
          super(...args);
          state.generation += 1;
          const gen = state.generation;
          state.ws = this;
          this.addEventListener('close', (ev) => {
            state.closes.push({ gen, code: ev.code, reason: ev.reason, at: Date.now() });
          });
          this.addEventListener('message', (ev) => {
            try {
              const parsed = typeof ev.data === 'string' ? JSON.parse(ev.data) : null;
              if (parsed && parsed.type === 'pong') state.pongs += 1;
            } catch {
              /* ignore */
            }
          });
        }
      }
      for (const key of ['CONNECTING', 'OPEN', 'CLOSING', 'CLOSED']) {
        Object.defineProperty(ObservedWebSocket, key, { value: NativeWebSocket[key] });
      }
      window.WebSocket = ObservedWebSocket;
      window.__crDelay = state;
      window.__crDelayTest = {
        arm(delayMs) {
          const ws = state.ws;
          if (!ws || ws.readyState !== NativeWebSocket.OPEN) {
            throw new Error('no live websocket to wrap');
          }
          state.armed = true;
          state.delayMs = delayMs;
          const delayed = new Set();
          const origSend = ws.send.bind(ws);
          ws.send = function (data) {
            try {
              const parsed = typeof data === 'string' ? JSON.parse(data) : null;
              if (state.armed && parsed && parsed.type === 'code_review_open' && parsed.id && delayed.size === 0) {
                delayed.add(parsed.id);
                state.delayedOpenIds.push({ id: parsed.id, gen: state.generation, at: Date.now() });
              }
            } catch {
              /* ignore */
            }
            return origSend(data);
          };
          const origOnMessage = ws.onmessage;
          ws.onmessage = function (ev) {
            let wait = 0;
            try {
              const parsed = typeof ev.data === 'string' ? JSON.parse(ev.data) : null;
              if (parsed && parsed.type === 'response' && parsed.id && delayed.has(parsed.id)) {
                delayed.delete(parsed.id);
                wait = state.delayMs;
              }
            } catch {
              /* ignore */
            }
            const deliver = () => {
              if (typeof origOnMessage === 'function') origOnMessage.call(ws, ev);
            };
            if (wait > 0) window.setTimeout(deliver, wait);
            else deliver();
          };
        },
        disarm() {
          state.armed = false;
        },
        failNextAttempts(count) {
          failuresRemaining = count;
          state.failuresRemaining = count;
        },
        /** Capture genuine live code_review_snapshot responses
         *  (isStreaming or activeCount>0) into liveSnapshotQueue.
         *  Snapshot polls that are live are HELD (original JSON
         *  unchanged) and delivered only after terminal — never
         *  rewritten onto a later poll. Comment responses still
         *  deliver immediately so Abort/dot can appear. */
        armLiveSnapshotCapture() {
          const ws = state.ws;
          if (!ws || ws.readyState !== NativeWebSocket.OPEN) {
            throw new Error('no live websocket to capture snapshots');
          }
          if (state.liveCaptureArmed) return;
          state.liveCaptureArmed = true;
          state.captureLiveSnapshots = true;
          state.delayedLiveRaw = [];
          state.liveSnapshotQueue = [];
          state.heldSnapshots = [];
          state.snapshotSentAt = {};
          state.pendingTimerFor = {};
          state.replayedLiveSnapshot = false;
          const pendingSnapshot = new Set();
          const origSend = ws.send.bind(ws);
          ws.send = function (data) {
            try {
              const parsed = typeof data === 'string' ? JSON.parse(data) : null;
              if (parsed && parsed.type === 'code_review_snapshot' && parsed.id) {
                pendingSnapshot.add(parsed.id);
                // The App's sendCommand arms the pending timeout timer
                // synchronously before ws.send, so the most recently armed
                // timer IS this request's pending timer.
                state.snapshotSentAt[parsed.id] = Date.now();
                state.pendingTimerFor[parsed.id] = lastArmedTimer;
              }
            } catch {
              /* ignore */
            }
            return origSend(data);
          };
          const origOnMessage = ws.onmessage;
          state.liveOrigOnMessage = origOnMessage;
          ws.onmessage = function (ev) {
            try {
              const parsed = typeof ev.data === 'string' ? JSON.parse(ev.data) : null;
              if (parsed && parsed.type === 'response' && parsed.data && typeof parsed.data === 'object') {
                const data = parsed.data;
                const live = data.isStreaming === true || (typeof data.activeCount === 'number' && data.activeCount > 0);
                if (state.captureLiveSnapshots && live) {
                  state.liveSnapshotQueue.push(JSON.parse(JSON.stringify(data)));
                }
                if (state.captureLiveSnapshots && live && pendingSnapshot.has(parsed.id)) {
                  pendingSnapshot.delete(parsed.id);
                  state.delayedLiveRaw.push(ev.data);
                  state.heldSnapshots.push({
                    id: parsed.id,
                    sentAt: state.snapshotSentAt[parsed.id] ?? Date.now(),
                    capturedAt: Date.now(),
                  });
                  return;
                }
              }
            } catch {
              /* ignore */
            }
            if (typeof origOnMessage === 'function') origOnMessage.call(ws, ev);
          };
        },
        /** Deliver every held live snapshot frame unchanged, but only after
         *  proving it cannot be expired by the 30s pending timeout and that
         *  the App actually consumed it (its pending timeout timer was
         *  cleared by PendingRegistry.take during delivery). */
        flushDelayedLiveSnapshots() {
          if (!state.delayedLiveRaw.length) {
            throw new Error('no delayed live snapshot to deliver');
          }
          // The consumption proof below relies on the pending timer being
          // cleared ONLY by PendingRegistry.take (response settlement). A
          // socket close/replace would also clear timers via drainExcept
          // WITHOUT consuming the response — so refuse to deliver on a
          // closed transport (the L3 window keeps the socket open; a close
          // here is a lane failure, not a delivery).
          if (!state.ws || state.ws.readyState !== NativeWebSocket.OPEN) {
            throw new Error('L3: live socket is not OPEN at flush time — held response cannot be consumed');
          }
          // Fail BEFORE delivery if any held response can be expired by the
          // 30s pending timeout (DEFAULT_COMMAND_TIMEOUT_MS for
          // code_review_snapshot in crates/pi-cli/web/src/pending.ts): the
          // App's PendingRegistry removes the entry when the timer fires, so
          // delivering an expired response would be a silent no-op and the
          // lane would false-pass.
          const now = Date.now();
          for (const held of state.heldSnapshots) {
            if (now - held.sentAt >= 30000) {
              throw new Error(`L3: held code_review_snapshot response (id ${held.id}) can be expired by the 30s pending timeout (elapsed ${now - held.sentAt}ms) — refusing to deliver a no-op`);
            }
          }
          state.captureLiveSnapshots = false;
          const ws = state.ws;
          const deliver = state.liveOrigOnMessage;
          for (const raw of state.delayedLiveRaw) {
            const original = new MessageEvent('message', { data: raw });
            if (typeof deliver === 'function') deliver.call(ws, original);
          }
          // Prove the App CONSUMED each held response: onResponse ->
          // PendingRegistry.take(id) clears the request's pending timeout
          // timer synchronously during delivery. A held response whose timer
          // was never cleared was ignored (expired/drained/never registered)
          // — the lane must fail rather than pass on a shim-only delivery.
          for (const held of state.heldSnapshots) {
            const timer = state.pendingTimerFor[held.id];
            if (!timer || !state.clearedTimers.has(timer)) {
              throw new Error(`L3: held code_review_snapshot response (id ${held.id}) was delivered but the App never settled its pending entry (timeout timer not cleared) — the response was ignored`);
            }
          }
          state.replayedLiveSnapshot = true;
        },
      };
    });

    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'page title missing');
    // Boot authority is the observed wrapper socket (the #conn-state element
    // is gone from the product): the wrapper saw a connection, the live
    // socket is OPEN, AND the reconnect bootstrap has bound the primary
    // session (the active sidebar row). The old conn-state 'on' pill was set
    // only after that bootstrap completed; passing on raw socket OPEN alone
    // would let the panel mount with sessionId=null and remount (re-running
    // openReview) when the session binds.
    await waitFor(
      page,
      () => window.__crDelay && window.__crDelay.generation >= 1
        && window.__crDelay.ws && window.__crDelay.ws.readyState === WebSocket.OPEN
        && !!document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId,
      'WS did not reach connected'
    );

    // L1: /code-review HEAD (single base) → from=HEAD + includeWorkingTree,
    // no `to`; comparison label is HEAD → working tree.
    const openBefore = socketFrames.sent.length;
    await page.fill('#prompt-input', '/code-review HEAD');
    await page.press('#prompt-input', 'Enter');
    await waitFor(page, () => document.getElementById('code-review-panel') !== null, 'code-review panel did not open');
    await waitFor(
      page,
      () => {
        const label = document.querySelector('.code-review__label')?.textContent || '';
        return label.includes('HEAD') && label.includes('working tree');
      },
      'comparison label missing'
    );
    if (!expectedTotals) fail('RPI_EXPECTED_TOTALS missing from the lane');
    await waitFor(
      page,
      (want) => (document.querySelector('.code-review__totals')?.textContent || '').trim() === want,
      'header authoritative totals missing or wrong',
      15000,
      expectedTotals
    );
    const headerTruncated = await page.evaluate(() =>
      /truncated/i.test(document.querySelector('.code-review__head')?.textContent || '')
    );
    if (headerTruncated) fail('header rendered a `· truncated` chip from the global preview cap');
    let headOpen = null;
    const headOpenDeadline = Date.now() + 15000;
    while (Date.now() < headOpenDeadline) {
      for (let i = openBefore; i < socketFrames.sent.length; i++) {
        const frame = parseFrame(socketFrames.sent[i].raw);
        if (frame && frame.type === 'code_review_open') {
          headOpen = frame;
          break;
        }
      }
      if (headOpen) break;
      await page.waitForTimeout(100);
    }
    if (!headOpen || headOpen.from !== 'HEAD' || headOpen.includeWorkingTree !== true || Object.hasOwn(headOpen, 'to')) {
      fail(`L1: /code-review HEAD must send code_review_open from=HEAD includeWorkingTree=true with no to key (got ${JSON.stringify(headOpen)})`);
    }
    await page.screenshot({ path: `${evidence}/code-review-base-head.png`, fullPage: true });

    // ---- T1: the file list is a nested tree ----
    await waitFor(
      page,
      () => document.querySelectorAll('li.code-review__tree-row[data-tree-kind="dir"]').length >= 1,
      'T1: no directory rows rendered (tree missing)'
    );
    const dirs = await page.evaluate(dirRows);
    const dirIds = dirs.map((d) => d.id);
    if (!dirIds.includes('dir:src')) fail(`T1: dir:src missing from the tree (got ${dirIds.join(', ')})`);
    if (!dirIds.includes('dir:src/deep')) fail(`T1: nested dir:src/deep missing from the tree (got ${dirIds.join(', ')})`);
    if (!dirs.every((d) => d.expanded === 'true')) {
      fail(`T1: directory rows must start expanded (${JSON.stringify(dirs)})`);
    }
    const allFilesVisible = await page.evaluate(visibleFileRows);
    if (allFilesVisible < 4) {
      fail(`T1: expected >=4 visible file rows (greet, big, src/other, src/deep/feature), got ${allFilesVisible}`);
    }
    await page.screenshot({ path: `${evidence}/code-review-tree-open.png`, fullPage: true });

    // ---- T2: collapse + expand a directory row ----
    const collapsed = await page.evaluate(() => {
      const li = Array.from(document.querySelectorAll('li.code-review__tree-row'))
        .find((row) => row.getAttribute('data-tree-id') === 'dir:src');
      const btn = li && li.querySelector('button.code-review__tree-dir');
      if (!btn) return null;
      btn.click();
      return true;
    });
    if (!collapsed) fail('T2: dir:src row button not found');
    await waitFor(
      page,
      () => {
        const li = Array.from(document.querySelectorAll('li.code-review__tree-row'))
          .find((row) => row.getAttribute('data-tree-id') === 'dir:src');
        return li && li.getAttribute('aria-expanded') === 'false';
      },
      'T2: aria-expanded did not flip to false on collapse'
    );
    await page.waitForTimeout(200);
    const filesWhileCollapsed = await page.evaluate(visibleFileRows);
    if (filesWhileCollapsed >= allFilesVisible) {
      fail(`T2: collapsing dir:src did not hide its child files (visible ${filesWhileCollapsed} >= ${allFilesVisible})`);
    }
    await page.screenshot({ path: `${evidence}/code-review-tree-collapsed.png`, fullPage: true });
    // Expand again: children reappear.
    await page.evaluate(() => {
      const li = Array.from(document.querySelectorAll('li.code-review__tree-row'))
        .find((row) => row.getAttribute('data-tree-id') === 'dir:src');
      li.querySelector('button.code-review__tree-dir').click();
    });
    await waitFor(
      page,
      () => {
        const li = Array.from(document.querySelectorAll('li.code-review__tree-row'))
          .find((row) => row.getAttribute('data-tree-id') === 'dir:src');
        return li && li.getAttribute('aria-expanded') === 'true';
      },
      'T2: aria-expanded did not flip back to true on expand'
    );
    await page.waitForTimeout(200);
    const filesExpanded = await page.evaluate(visibleFileRows);
    if (filesExpanded !== allFilesVisible) {
      fail(`T2: expanding dir:src did not restore its children (visible ${filesExpanded} != ${allFilesVisible})`);
    }

    await page.locator(`button.code-review__file[data-file-path="${navFile}"]`).click();
    await waitFor(
      page,
      () =>
        document.querySelectorAll('.code-review__hunk').length === 2
        && (document.querySelector('.code-review__hunk-nav-position')?.textContent || '').trim() === '0 / 2',
      'T2N: two-hunk navigation did not initialize'
    );
    const previousHunk = page.getByRole('button', { name: 'Previous hunk', exact: true });
    const nextHunk = page.getByRole('button', { name: 'Next hunk', exact: true });
    if (!(await previousHunk.isDisabled()) || await nextHunk.isDisabled()) {
      fail('T2N: initial hunk navigation boundaries are wrong');
    }
    await nextHunk.click();
    await waitFor(
      page,
      () =>
        (document.querySelector('.code-review__hunk-nav-position')?.textContent || '').trim() === '1 / 2'
        && (document.querySelector('.code-review__hunk.is-selected')?.textContent || '').includes('nav-changed-002'),
      'T2N: Next did not select the first hunk'
    );
    await nextHunk.click();
    await waitFor(
      page,
      () =>
        (document.querySelector('.code-review__hunk-nav-position')?.textContent || '').trim() === '2 / 2'
        && (document.querySelector('.code-review__hunk.is-selected')?.textContent || '').includes('nav-second-hunk'),
      'T2N: Next did not jump to the second hunk'
    );
    const desktopNavigation = await page.evaluate(() => {
      const pane = document.querySelector('.code-review__diff');
      const selected = document.querySelector('.code-review__hunk.is-selected');
      const buttons = Array.from(document.querySelectorAll('.code-review__hunk-nav-btn'));
      if (!pane || !selected || buttons.length !== 2) return null;
      const paneRect = pane.getBoundingClientRect();
      const selectedRect = selected.getBoundingClientRect();
      return {
        scrollTop: pane.scrollTop,
        selectedVisible: selectedRect.top >= paneRect.top && selectedRect.bottom <= paneRect.bottom,
        buttonBoxes: buttons.map((button) => button.getBoundingClientRect().toJSON()),
        previousDisabled: buttons[0].disabled,
        nextDisabled: buttons[1].disabled,
      };
    });
    if (
      !desktopNavigation
      || desktopNavigation.scrollTop <= 0
      || !desktopNavigation.selectedVisible
      || desktopNavigation.previousDisabled
      || !desktopNavigation.nextDisabled
      || desktopNavigation.buttonBoxes.some((box) => box.width < 32 || box.height < 32)
    ) {
      fail(`T2N: desktop hunk jump did not reveal the selected hunk: ${JSON.stringify(desktopNavigation)}`);
    }
    await previousHunk.click();
    await waitFor(
      page,
      () =>
        (document.querySelector('.code-review__hunk-nav-position')?.textContent || '').trim() === '1 / 2'
        && (document.querySelector('.code-review__hunk.is-selected')?.textContent || '').includes('nav-changed-002'),
      'T2N: Previous did not return to the first hunk'
    );
    await page.screenshot({ path: `${evidence}/code-review-hunk-navigation-desktop.png`, fullPage: true });

    await clickFile(page, bigFile);
    await waitFor(
      page,
      () => document.querySelectorAll('.code-review__line').length === 4000,
      'T3: first 4000-line window did not render for the >4000-line fixture',
      20000
    );
    const t3State = await page.evaluate(() => ({
      lines: document.querySelectorAll('.code-review__line').length,
      loadMore: document.querySelector('.code-review__load-more') !== null,
      loadFull: document.querySelector('.code-review__load-full') !== null,
      banner: document.querySelector('.code-review__banner--truncated') !== null,
      status: document.querySelector('.code-review__page-status')?.textContent || null,
    }));
    if (t3State.lines !== 4000) {
      fail(`T3: first window must be exactly the 4000-line soft cap (got ${t3State.lines})`);
    }
    if (t3State.loadMore || t3State.loadFull) {
      fail('T3: manual .code-review__load-more/.code-review__load-full buttons must be gone (scroll owns paging)');
    }
    if (t3State.banner) fail('T3: the ordinary >4000-line fixture must not render a Large/truncated banner');
    if (t3State.status) {
      fail(`T3: ordinary >4000-line paging must render no bounded status (got ${JSON.stringify(t3State.status)})`);
    }
    const t3DeepestVisible = await page.evaluate(() =>
      Array.from(document.querySelectorAll('.code-review__line-text')).some((el) =>
        (el.textContent || '').includes('changed-line-04001')
      )
    );
    if (t3DeepestVisible) fail('T3: changed-line-04001 must be BEYOND the first 4000-line window');
    await page.screenshot({ path: `${evidence}/code-review-big-first-window.png`, fullPage: true });

    await waitForNode(
      () => fileDiffCommands().filter((f) => f.path === bigFile).length === 9
        && fileDiffResponses().filter((r) => r.data.path === bigFile).length === 9,
      'T4: the eager fetch did not settle the expected 9 backend pages',
      30000
    );
    assertPageChain(bigFile, 8200);
    await scrollDiffToBottom(page);
    await waitFor(
      page,
      () => document.querySelectorAll('.code-review__line').length === 8000,
      'T4: near-bottom scroll did not grow the window 4000 → 8000',
      20000
    );
    await page.screenshot({ path: `${evidence}/code-review-big-scroll-8000.png`, fullPage: true });
    await scrollDiffToBottom(page);
    await waitFor(
      page,
      () => {
        const rows = Array.from(document.querySelectorAll('.code-review__line-text'));
        return document.querySelectorAll('.code-review__line').length === 8200
          && rows.some((el) => (el.textContent || '').includes('changed-line-04001'))
          && rows.some((el) => (el.textContent || '').includes('changed-line-04100'));
      },
      'T4: scrolling to the terminal window did not reveal the deepest fixture lines',
      20000
    );
    const t4State = await page.evaluate(() => ({
      banner: document.querySelector('.code-review__banner--truncated') !== null,
      status: document.querySelector('.code-review__page-status')?.textContent || null,
      loadMore: document.querySelector('.code-review__load-more') !== null,
      loadFull: document.querySelector('.code-review__load-full') !== null,
    }));
    if (t4State.banner || t4State.status || t4State.loadMore || t4State.loadFull) {
      fail(`T4: fully-paged ordinary file must render no banner/status/buttons (got ${JSON.stringify(t4State)})`);
    }
    await page.screenshot({ path: `${evidence}/code-review-big-terminal.png`, fullPage: true });

    await clickFile(page, uicapFile);
    await waitFor(
      page,
      () => document.querySelectorAll('.code-review__line').length === 4000,
      'T5: first 4000-line window did not render for the UI-cap fixture',
      20000
    );
    const uicapInitialScrollTop = await page.evaluate(
      () => document.querySelector('.code-review__diff:not(.is-hidden)')?.scrollTop ?? null,
    );
    if (uicapInitialScrollTop !== 0) {
      fail(`T5: selecting a file must reset the diff viewport to the top (scrollTop=${uicapInitialScrollTop})`);
    }
    await waitForNode(
      () => fileDiffCommands().filter((f) => f.path === uicapFile).length === 44
        && fileDiffResponses().filter((r) => r.data.path === uicapFile).length === 44,
      'T5: the eager fetch did not settle the expected 44 backend pages',
      60000
    );
    assertPageChain(uicapFile, 44000);
    for (const wantWindow of [8000, 12000, 16000, 20000]) {
      await scrollDiffToBottom(page);
      await waitFor(
        page,
        (want) => document.querySelectorAll('.code-review__line').length === want,
        `T5: near-bottom scroll did not grow the window to ${wantWindow}`,
        20000,
        wantWindow
      );
    }
    const uicapStatusText = 'Loaded 20,000 of 44,000 lines — the UI display limit is 20,000 lines.';
    await waitFor(
      page,
      (want) => (document.querySelector('.code-review__page-status')?.textContent || '') === want,
      'T5: UI display cap did not report the precise Loaded N of M status',
      10000,
      uicapStatusText
    );
    const t5State = await page.evaluate(() => ({
      lines: document.querySelectorAll('.code-review__line').length,
      status: document.querySelector('.code-review__page-status')?.textContent || '',
      loadMore: document.querySelector('.code-review__load-more') !== null,
      loadFull: document.querySelector('.code-review__load-full') !== null,
    }));
    if (t5State.lines !== 20000) fail(`T5: window must pin at the 20000-line UI cap (got ${t5State.lines})`);
    if (t5State.loadMore || t5State.loadFull) fail('T5: manual load buttons must stay absent at the UI cap');
    await scrollDiffToBottom(page);
    await page.waitForTimeout(800);
    const t5Pinned = await page.evaluate((want) => ({
      lines: document.querySelectorAll('.code-review__line').length,
      status: document.querySelector('.code-review__page-status')?.textContent || '',
    }), uicapStatusText);
    if (t5Pinned.lines !== 20000 || t5Pinned.status !== uicapStatusText) {
      fail(`T5: window grew past the UI cap on a further near-bottom scroll (got ${JSON.stringify(t5Pinned)})`);
    }
    await clickFile(page, bytecapFile);
    await waitForNode(() => {
      const responses = fileDiffResponses().filter((r) => r.data.path === bytecapFile);
      return responses.length > 0
        && responses.some((r) => r.data.hasMore === false && r.data.byteCapped === true);
    }, 'T10: the byte-capped fixture never settled a terminal byteCapped=true page', 60000);
    assertByteCapChain(bytecapFile);
    const bytecapTerminal = fileDiffResponses()
      .filter((r) => r.data.path === bytecapFile && r.data.hasMore === false)
      .map((r) => r.data)
      .pop();
    if (!bytecapTerminal || bytecapTerminal.byteCapped !== true) {
      fail('T10: terminal byte-capped page missing from the observed wire');
    }
    for (let grow = 0; grow < 8; grow++) {
      const rendered = await page.evaluate(() => document.querySelectorAll('.code-review__line').length);
      if (rendered >= bytecapTerminal.totalLines) break;
      await scrollDiffToBottom(page);
      await page.waitForTimeout(200);
    }
    const bytecapStatus = `Loaded ${Number(bytecapTerminal.totalLines).toLocaleString()} lines — the file body exceeds the backend size limit; later lines are not available and the total line count is unknown.`;
    await waitFor(
      page,
      (want) => document.querySelectorAll('.code-review__line').length === want.totalLines
        && (document.querySelector('.code-review__page-status')?.textContent || '') === want.status,
      'T10: byte-capped file did not render the full parsed body with the exact byte-cap status',
      20000,
      { totalLines: bytecapTerminal.totalLines, status: bytecapStatus }
    );
    const t10State = await page.evaluate(() => ({
      status: document.querySelector('.code-review__page-status')?.textContent || '',
      lines: document.querySelectorAll('.code-review__line').length,
      loadMore: document.querySelector('.code-review__load-more') !== null,
      loadFull: document.querySelector('.code-review__load-full') !== null,
      uiCapCopy: /UI display limit/i.test(
        document.querySelector('.code-review__diff')?.textContent || ''
      ),
    }));
    if (t10State.status !== bytecapStatus) {
      fail(`T10: byte-cap status must be the product's exact copy (got ${JSON.stringify(t10State.status)})`);
    }
    if (t10State.loadMore || t10State.loadFull) {
      fail('T10: manual .code-review__load-more/.code-review__load-full buttons must stay absent at the byte cap');
    }
    if (t10State.uiCapCopy) {
      fail('T10: the byte-cap state must render the byte-cap copy, never the UI display limit copy');
    }
    await page.screenshot({ path: `${evidence}/code-review-bytecap.png`, fullPage: false });

    await clickFile(page, 'zz-later.txt');
    await waitFor(
      page,
      () =>
        Array.from(document.querySelectorAll('.code-review__line-text')).some((el) =>
          (el.textContent || '').includes('later changed')
        ),
      'T7: empty placeholder zz-later.txt did not auto-load its diff without a click',
      20000
    );
    const placeholderState = await page.evaluate(() => ({
      lineCount: document.querySelectorAll('.code-review__line').length,
      noHunks: Array.from(document.querySelectorAll('.code-review__empty')).some((el) =>
        (el.textContent || '').includes('No hunks in this file')
      ),
      hljsSpans: Array.from(document.querySelectorAll('.code-review__line-text span[class^="hljs-"]'))
        .length,
    }));
    if (placeholderState.lineCount < 2) {
      fail(`T7: placeholder rendered too few lines (${placeholderState.lineCount})`);
    }
    if (placeholderState.noHunks) {
      fail('T7: placeholder pane must never claim "No hunks in this file"');
    }
    if (placeholderState.hljsSpans !== 0) {
      fail(`T7: unknown-language placeholder body must stay plain (${placeholderState.hljsSpans} hljs spans)`);
    }
    await waitForNode(
      () => fileDiffCommands().filter((f) => f.path === 'zz-later.txt').length === 1,
      'T7: the placeholder did not eager-fetch exactly one backend page',
      15000
    );
    await waitForNode(
      () => (fileDiffResponses().filter((r) => r.data.path === 'zz-later.txt').length === 1),
      'T7: the placeholder backend page response never settled',
      15000
    );
    assertPageChain('zz-later.txt', 2);
    const laterBound = await page.evaluate(() => ({
      status: document.querySelector('.code-review__page-status')?.textContent || null,
      banner: document.querySelector('.code-review__banner--truncated') !== null,
      truncatedCopy: /truncated/i.test(
        document.querySelector('.code-review__diff')?.textContent || ''
      ),
    }));
    if (laterBound.status || laterBound.banner) {
      fail(`T7: completed ordinary placeholder must render no truncated marker/status (got ${JSON.stringify(laterBound)})`);
    }
    if (laterBound.truncatedCopy) {
      fail('T7: the diff pane must not render any truncated copy after the placeholder completed');
    }
    await page.screenshot({ path: `${evidence}/code-review-placeholder-autoload.png`, fullPage: true });

    // ---- T9: a hunk originating BEYOND the global truncation is selectable
    // and commentable ----
    // zz-later.txt's diff exists only in the loaded per-file data (the 2 MiB
    // snapshot never carried it), so its hunk header appears only after the
    // eager per-file fetch. Selecting it, typing a comment and pressing Enter
    // must resolve the loaded hunk identity against the authoritative RPC
    // cache and queue the thread (user comment card + hunk badge).
    await waitFor(
      page,
      () => document.querySelector('.code-review__hunk-header:not(:disabled)') !== null,
      'T9: loaded hunk header did not become commentable after the eager fetch',
      20000
    );
    await page.click('.code-review__hunk-header:not(:disabled)');
    await page.waitForTimeout(500);
    const initialSelectionState = await page.evaluate(() => ({
      enabledHeaders: Array.from(document.querySelectorAll('.code-review__hunk-header:not(:disabled)')).map((el) => ({
        text: el.textContent || '',
        pressed: el.getAttribute('aria-pressed'),
      })),
      selectedHunks: document.querySelectorAll('.code-review__hunk.is-selected').length,
      composer: document.querySelector('.code-review__comment-input') !== null,
      threadText: document.querySelector('.code-review__comments')?.textContent || '',
    }));
    if (!initialSelectionState.composer) {
      fail(`T9: selection state ${JSON.stringify(initialSelectionState)}`);
    }
    const beyondText = 'comment on a hunk beyond global truncation';
    await page.fill('.code-review__comment-input', beyondText);
    await page.press('.code-review__comment-input', 'Enter');
    await waitFor(
      page,
      (expectedText) =>
        Array.from(document.querySelectorAll('.code-review__comment--user .code-review__comment-text')).some(
          (el) => (el.textContent || '').includes(expectedText),
        ),
      'T9: the loaded-hunk comment never rendered as an accepted thread card',
      25000,
      beyondText
    );
    const beyondThread = await page.evaluate(() => ({
      badge: document.querySelector('.code-review__hunk-badge')?.textContent || '',
      userComments: Array.from(document.querySelectorAll('.code-review__comment--user')).length,
    }));
    if (beyondThread.badge !== '1' && beyondThread.userComments < 1) {
      fail(`T9: loaded hunk thread missing (badge=${JSON.stringify(beyondThread)})`);
    }
    await page.screenshot({ path: `${evidence}/code-review-loaded-hunk-comment.png`, fullPage: true });

    // ---- T8: hljs token spans on rust diff lines, verbatim + hostile literal ----
    await clickFile(page, 'src/other.rs');
    await waitFor(
      page,
      () =>
        Array.from(document.querySelectorAll('.code-review__line-text span[class^="hljs-"]')).length > 0,
      'T8: rust diff lines rendered no hljs token spans',
      15000
    );
    const rustLines = await page.evaluate(() =>
      Array.from(document.querySelectorAll('.code-review__line')).map((row) => ({
        kind: row.className,
        oldNo: row.querySelector('.code-review__line-no--old')?.textContent || '',
        newNo: row.querySelector('.code-review__line-no--new')?.textContent || '',
        prefix: row.querySelector('.code-review__line-prefix')?.textContent || '',
        text: row.querySelector('.code-review__line-text')?.textContent || '',
        spans: row.querySelectorAll('.code-review__line-text span[class^="hljs-"]').length,
      }))
    );
    const fnLine = rustLines.find((l) => l.text.includes('fn src_helper'));
    if (!fnLine || fnLine.spans === 0) {
      fail(`T8: 'fn src_helper' line missing or unhighlighted (${JSON.stringify(fnLine)})`);
    }
    if (fnLine.text !== 'fn src_helper() -> u32 {') {
      fail(`T8: highlighted line textContent drifted from the source (${fnLine.text})`);
    }
    const letLine = rustLines.find((l) => l.text.includes('let cards'));
    if (!letLine || letLine.spans === 0) {
      fail('T8: "let cards" line missing or unhighlighted');
    }
    // Line numbers, +/- prefix, and kind backgrounds must be untouched by
    // highlighting.
    if (fnLine.kind.includes('--addition') !== true) {
      fail(`T8: highlighted line lost its addition background class (${fnLine.kind})`);
    }
    if (fnLine.prefix !== '+' || fnLine.oldNo !== '' || fnLine.newNo === '') {
      fail(`T8: prefix/line numbers changed on the highlighted line (${JSON.stringify(fnLine)})`);
    }
    // The hostile <script> diff line stays LITERAL text with no side effect.
    const hostileLine = rustLines.find((l) => l.text.includes('<script>'));
    if (!hostileLine) {
      fail('T8: hostile <script> diff line missing from the rendered diff');
    }
    const hostileSafe = await page.evaluate(() => ({
      scriptEls: document.querySelectorAll('.code-review__diff script').length,
      pwned: typeof window.__crPwned !== 'undefined',
    }));
    if (hostileSafe.scriptEls !== 0) fail('T8: hostile diff line created a real <script> element');
    if (hostileSafe.pwned) fail('T8: hostile diff line executed (window.__crPwned set)');
    if (dialogs > 0) fail(`T8: hostile diff line opened ${dialogs} dialog(s)`);
    await page.screenshot({ path: `${evidence}/code-review-highlighted.png`, fullPage: true });

    // ---- C1/C2: comment markdown + hostile HTML literal ----
    await clickFile(page, dirtyFile);
    await waitFor(
      page,
      () => document.querySelector('.code-review__line--deletion') !== null,
      'dirty file diff did not render'
    );
    // Explicit hunk selection opens the composer.
    await page.click('.code-review__hunk-header');
    await waitFor(
      page,
      () => document.querySelector('.code-review__comment-input') !== null,
      'composer did not open after hunk selection'
    );
    const hostile = '<script>window.__crPwned=1</script><img src=x onerror=window.__crPwned=2>';
    const commentText = `review markdown matrix **user bold**\n- item one\n- item two\n\n\`\`\`rust\nfn user_rust() -> u32 { 1 }\n\`\`\`\n\n${hostile}`;
    await page.fill('.code-review__comment-input', commentText);
    await page.press('.code-review__comment-input', 'Enter');

    // The USER comment renders markdown + literal hostile HTML.
    await waitFor(
      page,
      () =>
        Array.from(document.querySelectorAll('.code-review__comment--user')).some((el) => {
          const text = el.querySelector('.code-review__comment-text')?.textContent || '';
          return text.includes('user bold') && text.includes('user_rust');
        }),
      'C1: user comment with markdown never rendered',
      20000
    );
    const userComment = await page.evaluate(() => {
      const el = document.querySelector('.code-review__comment--user .code-review__comment-text');
      if (!el) return null;
      const pre = el.querySelector('pre.md-fence__pre, pre.md-fence');
      const fence = pre
        ? {
            langLabel: el.querySelector('.md-fence__lang')?.textContent?.trim() || '',
            codeText: (pre.querySelector('code')?.textContent || '').trim(),
          }
        : null;
      return {
        strong: el.querySelector('strong')?.textContent || '',
        listItems: Array.from(el.querySelectorAll('ul li')).map((li) => li.textContent),
        fence,
        text: el.textContent || '',
        scriptEl: el.querySelector('script') !== null,
        imgX: el.querySelector('img[src="x"]') !== null,
      };
    });
    if (!userComment || userComment.strong !== 'user bold') fail(`C1: user comment did not render <strong> (${JSON.stringify(userComment)})`);
    if (!userComment.listItems.includes('item one') || !userComment.listItems.includes('item two')) {
      fail(`C1: user comment did not render the list (${JSON.stringify(userComment.listItems)})`);
    }
    if (!userComment.fence || !userComment.fence.codeText.includes('user_rust')) {
      fail(`C1: user comment did not render the rust fence (${JSON.stringify(userComment.fence)})`);
    }
    if (userComment.scriptEl || userComment.imgX) {
      fail('C1: hostile HTML created real elements in the user comment');
    }
    if (!userComment.text.includes('<script>')) {
      fail('C1: hostile HTML must stay LITERAL text (escaped) in the user comment');
    }
    const noSideEffects = await page.evaluate(() => typeof window.__crPwned !== 'undefined');
    if (noSideEffects) fail('C1: hostile script executed (window.__crPwned set)');
    if (dialogs > 0) fail(`C1: hostile HTML opened ${dialogs} dialog(s)`);
    await page.screenshot({ path: `${evidence}/code-review-comment-user.png`, fullPage: true });

    // The ASSISTANT reply (mock-routed markdown matrix) renders markdown +
    // literal hostile HTML.
    await waitFor(
      page,
      () =>
        Array.from(document.querySelectorAll('.code-review__comment--assistant')).some((el) => {
          const text = el.querySelector('.code-review__comment-text')?.textContent || '';
          return text.includes('review bold verdict') && text.includes('review_rust');
        }),
      'C2: assistant review reply with markdown never rendered',
      30000
    );
    const assistantComment = await page.evaluate(() => {
      const el = document.querySelector('.code-review__comment--assistant .code-review__comment-text');
      if (!el) return null;
      const pre = el.querySelector('pre.md-fence__pre, pre.md-fence');
      const fence = pre
        ? {
            langLabel: el.querySelector('.md-fence__lang')?.textContent?.trim() || '',
            codeText: (pre.querySelector('code')?.textContent || '').trim(),
          }
        : null;
      return {
        strong: el.querySelector('strong')?.textContent || '',
        listItems: Array.from(el.querySelectorAll('ul li')).map((li) => li.textContent),
        fence,
        text: el.textContent || '',
        scriptEl: el.querySelector('script') !== null,
        imgX: el.querySelector('img[src="x"]') !== null,
      };
    });
    if (!assistantComment || assistantComment.strong !== 'review bold verdict') {
      fail(`C2: assistant comment did not render <strong> (${JSON.stringify(assistantComment)})`);
    }
    if (!assistantComment.listItems.includes('review item alpha') || !assistantComment.listItems.includes('review item beta')) {
      fail(`C2: assistant comment did not render the list (${JSON.stringify(assistantComment.listItems)})`);
    }
    if (!assistantComment.fence || !assistantComment.fence.codeText.includes('review_rust')) {
      fail(`C2: assistant comment did not render the rust fence (${JSON.stringify(assistantComment.fence)})`);
    }
    if (assistantComment.scriptEl || assistantComment.imgX) {
      fail('C2: hostile HTML created real elements in the assistant comment');
    }
    if (!assistantComment.text.includes('<script>')) {
      fail('C2: hostile HTML must stay LITERAL text in the assistant comment');
    }
    const noSideEffects2 = await page.evaluate(() => typeof window.__crPwned !== 'undefined');
    if (noSideEffects2) fail('C2: hostile script executed via the assistant comment (window.__crPwned set)');
    if (dialogs > 0) fail(`C2: hostile HTML opened ${dialogs} dialog(s)`);
    await page.screenshot({ path: `${evidence}/code-review-comment-assistant.png`, fullPage: true });

    // ---- L2/L3: tool-path reply, live Abort+dot, terminal idle, late poll ----
    // src/deep/feature.rs is unused by C1/C2; the mock routes "cr grep probe"
    // through ONE real grep tool call then echoes the RAW tool-result
    // (needle + matched path). Comment RPC returns immediately while the
    // reply is live (Abort + streaming-dot).
    await page.evaluate(() => {
      if (!window.__crDelayTest || typeof window.__crDelayTest.armLiveSnapshotCapture !== 'function') {
        throw new Error('L3: live-snapshot capture helper missing');
      }
      window.__crDelayTest.armLiveSnapshotCapture();
    });
    await clickFile(page, 'src/deep/feature.rs');
    await waitFor(
      page,
      () => Array.from(document.querySelectorAll('.code-review__line-text')).some((el) =>
        (el.textContent || '').includes('deep feature changed')
      ),
      'L2: src/deep/feature.rs diff did not render'
    );
    await page.click('.code-review__hunk-header');
    await waitFor(
      page,
      () => document.querySelector('.code-review__comment-input') !== null,
      'L2: composer did not open for the tool-path hunk'
    );
    const grepProbe = 'cr grep probe';
    const grepNeedle = 'needle';
    const grepPath = 'src/deep/feature.rs';
    await page.fill('.code-review__comment-input', grepProbe);
    await page.press('.code-review__comment-input', 'Enter');
    await waitFor(
      page,
      (want) =>
        Array.from(document.querySelectorAll('.code-review__comment--user')).some((el) =>
          (el.querySelector('.code-review__comment-text')?.textContent || '').includes(want)
        ),
      'L2: user comment did not land before the tool-path reply',
      20000,
      grepProbe
    );
    await waitFor(
      page,
      () => {
        const abort = Array.from(document.querySelectorAll('button.code-review__action--stop'))
          .some((b) => (b.textContent || '').includes('Stop'));
        const headerDot = document.querySelector('.code-review__streaming') !== null;
        const hunkDot = document.querySelector('.code-review__hunk .code-review__streaming-dot') !== null;
        return abort && (headerDot || hunkDot);
      },
      'L2: streaming-dot + Stop did not appear during the tool-path reply',
      15000
    );
    await waitFor(
      page,
      () => (window.__crDelay.delayedLiveRaw || []).length > 0,
      'L3: no genuine live snapshot poll was captured-and-held while the reply was in-flight',
      10000
    );
    const heldSnapshots = await page.evaluate(() =>
      (window.__crDelay.heldSnapshots || []).map((h) => ({
        id: h.id,
        sentAt: h.sentAt,
        capturedAt: h.capturedAt,
      }))
    );
    if (!heldSnapshots.length || heldSnapshots.some((h) => !h.id || !h.sentAt || !h.capturedAt)) {
      fail(`L3: held snapshot request ids/capture times not tracked (${JSON.stringify(heldSnapshots)})`);
    }
    await page.screenshot({ path: `${evidence}/code-review-tool-live.png`, fullPage: true });
    await waitFor(
      page,
      ({ want, needle, pathText }) => {
        const assistant = Array.from(document.querySelectorAll('.code-review__comment--assistant .code-review__comment-text'))
          .map((el) => el.textContent || '');
        const echo = assistant.find((t) => t.includes('cr-grep-echo') && t.includes(needle) && t.includes(pathText));
        if (!echo) return false;
        const stub = /stub|not allowed|permission|tool (is )?unavailable|failed closed|read-only|fail(ed|ure)/i.test(echo);
        if (stub) return false;
        const abort = Array.from(document.querySelectorAll('button.code-review__action--stop'))
          .some((b) => (b.textContent || '').includes('Stop'));
        const streaming = document.querySelector('.code-review__streaming') !== null;
        const hunkDot = document.querySelector('.code-review__hunk .code-review__streaming-dot') !== null;
        const busy = document.querySelector('.code-review__busy') !== null;
        const input = document.querySelector('.code-review__comment-input');
        const userRestored = Array.from(document.querySelectorAll('.code-review__comment--user .code-review__comment-text'))
          .some((el) => (el.textContent || '').includes(want));
        return !abort && !streaming && !hunkDot && !busy && input && !input.disabled && userRestored && assistant.length === 1;
      },
      'L2: tool-path reply never reached terminal idle with real grep needle+path (stub/fail grep fails)',
      30000,
      { want: grepProbe, needle: grepNeedle, pathText: grepPath }
    );
    const grepEcho = await page.evaluate(() => {
      const el = document.querySelector('.code-review__comment--assistant .code-review__comment-text');
      return el ? (el.textContent || '') : '';
    });
    if (/stub|not allowed|permission|tool (is )?unavailable|failed closed|read-only|fail(ed|ure)/i.test(grepEcho)) {
      fail(`L2: grep tool failed closed / stubbed (echo=${JSON.stringify(grepEcho)})`);
    }
    if (!grepEcho.includes('cr-grep-echo') || !grepEcho.includes(grepNeedle) || !grepEcho.includes(grepPath)) {
      fail(`L2: assistant reply must include cr-grep-echo plus fixture grep needle + matched path (echo=${JSON.stringify(grepEcho)})`);
    }
    await page.screenshot({ path: `${evidence}/code-review-tool-terminal.png`, fullPage: true });

    // L3: deliver the captured LIVE snapshot JSON unchanged (old
    // revision/comments/active fields intact) AFTER terminal. Overlay
    // must stay idle — a delayed in-flight poll cannot resurrect Abort.
    const capturedLive = await page.evaluate(() => {
      const raw = (window.__crDelay.delayedLiveRaw || [])[0];
      if (typeof raw !== 'string') return null;
      let parsed;
      try {
        parsed = JSON.parse(raw);
      } catch {
        return null;
      }
      const live = parsed && parsed.data;
      if (!live) return null;
      return {
        isStreaming: live.isStreaming,
        activeCount: live.activeCount,
        snapshotId: live.snapshotId,
        revision: live.revision,
        generation: live.generation,
        threadCount: Array.isArray(live.threads) ? live.threads.length : 0,
        userCommentPresent: Array.isArray(live.threads)
          && live.threads.some((t) => Array.isArray(t.comments)
            && t.comments.some((c) => typeof c.text === 'string' && c.text.includes('cr grep probe'))),
        unchanged: true,
      };
    });
    if (!capturedLive || (capturedLive.isStreaming !== true && !(capturedLive.activeCount > 0))) {
      fail(`L3: held snapshot was not a genuine live frame (${JSON.stringify(capturedLive)})`);
    }
    await page.evaluate(() => {
      window.__crDelayTest.flushDelayedLiveSnapshots();
    });
    await waitFor(
      page,
      () => window.__crDelay && window.__crDelay.replayedLiveSnapshot === true,
      'L3: held live snapshot was never delivered after terminal',
      5000
    );
    // Wait through React settlement (a render cycle) before reading the
    // terminal DOM so a resurrected indicator cannot be missed by sampling
    // before the app's state update flushed.
    await page.waitForTimeout(100);
    const lateIdle = await page.evaluate(() => ({
      abort: Array.from(document.querySelectorAll('button.code-review__action--stop'))
        .some((b) => (b.textContent || '').includes('Stop')),
      streaming: document.querySelector('.code-review__streaming') !== null,
      hunkDot: document.querySelector('.code-review__hunk .code-review__streaming-dot') !== null,
      busy: document.querySelector('.code-review__busy') !== null,
      replayed: !!(window.__crDelay && window.__crDelay.replayedLiveSnapshot),
    }));
    if (lateIdle.abort || lateIdle.streaming || lateIdle.hunkDot || lateIdle.busy) {
      fail(`L3: delayed live snapshot resurrected live UI (${JSON.stringify(lateIdle)})`);
    }
    await page.screenshot({ path: `${evidence}/code-review-late-idle.png`, fullPage: true });

    // ---- T6: responsive breakpoint + mobile tab bar (narrow-viewport UI) ----
    // The desktop workspace renders no tab bar. Resizing the viewport across
    // the 900px breakpoint WHILE the panel is mounted fires the matchMedia
    // change handler, which surfaces the Files/Diff/Thread tab bar. Each tab
    // click drives the single-pane narrow UI (only the active pane is
    // visible), Back-to-diff returns from the Thread pane, and resizing back
    // to desktop removes the tab bar. This is a real user flow: the panel
    // stays open across the resize (window resize -> matchMedia change ->
    // narrow layout), not a mobile-context reload.
    const tabBarAtDesktop = await page.evaluate(() => document.querySelector('.code-review__tab-bar') !== null);
    if (tabBarAtDesktop) fail('T6: tab bar rendered while the workspace is desktop-width');
    await page.setViewportSize({ width: 700, height: 800 });
    await waitFor(
      page,
      () => document.querySelector('.code-review__tab-bar') !== null,
      'T6: resizing under 900px did not surface the mobile tab bar'
    );
    const narrowTabs = await page.evaluate(() =>
      Array.from(document.querySelectorAll('.code-review__tab')).map((t) => t.textContent.trim())
    );
    if (narrowTabs.join(',') !== 'Files,Diff,Thread') {
      fail(`T6: narrow tab bar wrong (got ${narrowTabs.join(',')})`);
    }
    const activeOnResize = await page.evaluate(
      () => document.querySelector('.code-review__tab.is-active')?.textContent.trim() || ''
    );
    if (activeOnResize !== 'Files') {
      fail(`T6: narrow workspace opens on the ${activeOnResize} tab instead of Files`);
    }
    const clickTab = async (name) => {
      const clicked = await page.evaluate((want) => {
        const tab = Array.from(document.querySelectorAll('.code-review__tab')).find((t) =>
          t.textContent.trim() === want
        );
        if (!tab) return false;
        tab.click();
        return true;
      }, name);
      if (!clicked) fail(`T6: ${name} tab not found for click`);
    };
    const visibleRect = (sel) =>
      page.evaluate((s) => {
        const el = document.querySelector(s);
        if (!el) return false;
        const rect = el.getBoundingClientRect();
        return rect.width > 0 && rect.height > 0;
      }, sel);

    // Click the Diff tab -> the diff pane becomes the visible single pane.
    await clickTab('Diff');
    await waitFor(
      page,
      () => document.querySelector('.code-review__tab.is-active')?.textContent.trim() === 'Diff',
      'T6: clicking the Diff tab did not activate it'
    );
    if (!(await visibleRect('.code-review__diff'))) fail('T6: diff pane not visible on the Diff tab');
    await page.screenshot({ path: `${evidence}/code-review-narrow-diff-tab.png`, fullPage: true });

    await page.setViewportSize({ width: 390, height: 844 });
    await clickTab('Files');
    await page.locator(`button.code-review__file[data-file-path="${navFile}"]`).click();
    await waitFor(
      page,
      () =>
        document.querySelector('.code-review__tab.is-active')?.textContent.trim() === 'Diff'
        && (document.querySelector('.code-review__hunk-nav-position')?.textContent || '').trim() === '0 / 2',
      'T6: selecting the hunk-navigation fixture did not return to the mobile Diff pane'
    );
    await page.getByRole('button', { name: 'Next hunk', exact: true }).click();
    await waitFor(
      page,
      () => (document.querySelector('.code-review__hunk-nav-position')?.textContent || '').trim() === '1 / 2',
      'T6: first mobile Next hunk did not select the first hunk'
    );
    await page.getByRole('button', { name: 'Next hunk', exact: true }).click();
    await waitFor(
      page,
      () =>
        document.querySelector('.code-review__tab.is-active')?.textContent.trim() === 'Diff'
        && (document.querySelector('.code-review__hunk-nav-position')?.textContent || '').trim() === '2 / 2'
        && (document.querySelector('.code-review__hunk.is-selected')?.textContent || '').includes('nav-second-hunk'),
      'T6: mobile Next hunk did not stay in Diff and reveal the second hunk'
    );
    const mobileNavigation = await page.evaluate(() => {
      const pane = document.querySelector('.code-review__diff');
      const selected = document.querySelector('.code-review__hunk.is-selected');
      const buttons = Array.from(document.querySelectorAll('.code-review__hunk-nav-btn'));
      if (!pane || !selected || buttons.length !== 2) return null;
      const paneRect = pane.getBoundingClientRect();
      const selectedRect = selected.getBoundingClientRect();
      return {
        selectedVisible: selectedRect.top >= paneRect.top && selectedRect.bottom <= paneRect.bottom,
        buttonBoxes: buttons.map((button) => button.getBoundingClientRect().toJSON()),
        activeTab: document.querySelector('.code-review__tab.is-active')?.textContent.trim() || '',
        overflow: document.documentElement.scrollWidth > window.innerWidth,
      };
    });
    if (
      !mobileNavigation
      || !mobileNavigation.selectedVisible
      || mobileNavigation.activeTab !== 'Diff'
      || mobileNavigation.overflow
      || mobileNavigation.buttonBoxes.some((box) => box.width < 44 || box.height < 44)
    ) {
      fail(`T6: mobile hunk navigation geometry/state failed: ${JSON.stringify(mobileNavigation)}`);
    }
    await page.screenshot({ path: `${evidence}/code-review-hunk-navigation-mobile.png`, fullPage: true });

    // Click the Thread tab -> the thread dock (with Back-to-diff) becomes
    // the visible single pane.
    await clickTab('Thread');
    await waitFor(
      page,
      () => document.querySelector('.code-review__tab.is-active')?.textContent.trim() === 'Thread',
      'T6: clicking the Thread tab did not activate it'
    );
    if (!(await visibleRect('.code-review__back'))) fail('T6: Back-to-diff missing on the Thread tab');
    await page.screenshot({ path: `${evidence}/code-review-narrow-thread-tab.png`, fullPage: true });

    // Back-to-diff returns to the diff pane.
    await page.click('.code-review__back');
    await waitFor(
      page,
      () => document.querySelector('.code-review__tab.is-active')?.textContent.trim() === 'Diff',
      'T6: Back-to-diff did not return to the Diff tab'
    );

    // Click the Files tab -> the file rail is the visible single pane again.
    await clickTab('Files');
    await waitFor(
      page,
      () => document.querySelector('.code-review__tab.is-active')?.textContent.trim() === 'Files',
      'T6: clicking the Files tab did not activate it'
    );
    if (await visibleRect('.code-review__diff')) fail('T6: diff pane still visible while the Files tab is active');
    await page.screenshot({ path: `${evidence}/code-review-narrow-files-tab.png`, fullPage: true });

    // Resize back to desktop: the change handler fires again (narrow->wide).
    await page.setViewportSize({ width: 1280, height: 800 });
    await waitFor(
      page,
      () => document.querySelector('.code-review__tab-bar') === null,
      'T6: resizing above 900px did not remove the mobile tab bar'
    );
    await page.screenshot({ path: `${evidence}/code-review-back-to-desktop.png`, fullPage: true });

    // ---- D1-D8: >30s delayed code_review_open + kill-1006 recovery ----
    // The first code_review_open response AFTER the wrapper is installed is
    // withheld RPI_OPEN_DELAY_MS (>30s, past the old fast-ack horizon that
    // used to classify the still-OPEN socket as transport-stale and close it
    // with a 4001 "connection unresponsive" close + toast). The contract:
    // command latency never owns transport liveness — the socket generation
    // stays constant across the delay (zero closes), unrelated get_state
    // commands round-trip mid-delay and again past 30s, the heartbeat JSON
    // pong proves frame exchange, no toast/inline error appears, and after
    // the response lands the panel settles with the same HEAD→working tree
    // snapshot. A REAL abnormal close — the lane KILLS the rpi listener so
    // the transport is severed at the socket level (no synthetic
    // close(code), so the close handshake can never complete and Chromium
    // reports the genuine abnormal-closure code 1006) — then produces
    // exactly one close with code === 1006 + one transport toast on the
    // killed generation, the lane respawns the listener on the SAME port
    // (--resume the pre-outage primary) while the shim fails the product's
    // reconnect attempts synchronously (no native ERR_CONNECTION_REFUSED is
    // manufactured), and the panel reconciles the retained controller —
    // with ZERO browser console/page errors (native ERR_CONNECTION_REFUSED
    // handling is never loosened).
    // D1: the pre-navigation wrapper observed the live bootstrap socket.
    // Arm it now: the FIRST code_review_open response that flows over the
    // live socket is withheld OPEN_DELAY_MS (>30s).
    await page.evaluate((delayMs) => {
      if (!window.__crDelay || window.__crDelay.generation === 0) {
        throw new Error('WebSocket wrapper did not observe a connection');
      }
      window.__crDelayTest.arm(delayMs);
    }, OPEN_DELAY_MS);

    // Close the existing panel for real (its mount-time code_review_open was
    // sent BEFORE the wrapper was armed, so it must not be the delayed one).
    // A streaming reply or unsent draft surfaces the guarded confirm; wait
    // for either the confirm dialog or a successful unmount, then accept the
    // confirm if it appeared.
    await page.evaluate(() => {
      const closeBtn = document.querySelector('button.code-review__close');
      if (closeBtn) closeBtn.click();
    });
    await waitFor(
      page,
      () => document.getElementById('code-review-panel') === null
        || document.querySelector('.code-review__confirm') !== null,
      'D: panel neither closed nor surfaced the guarded close confirm',
      5000
    );
    const confirmClose = await page.evaluate(() => {
      const confirmDialog = document.querySelector('.code-review__confirm');
      if (!confirmDialog) return false;
      const primary = Array.from(document.querySelectorAll('.code-review__confirm-actions button'))
        .find((b) => (b.textContent || '').includes('Close panel'));
      if (primary) {
        primary.click();
        return true;
      }
      return false;
    });
    if (confirmClose) {
      await page.waitForTimeout(200);
    }
    await waitFor(
      page,
      () => document.getElementById('code-review-panel') === null,
      'D: closing the existing panel for the delayed-open segment failed'
    );

    // Seed the primary session with a real prompt round-trip BEFORE the
    // delayed open: the respawned listener --resumes this exact session id
    // after the kill (D7's panel-preserved contract requires the pre-outage
    // owner to be the post-restart primary). T9 + C2 + L2 comments already
    // consumed mock requests 1-3, so this prompt is even → instant
    // "steering-followup-reply" (odd requests still stream chunk-four-done).
    await page.fill('#prompt-input', 'seed the primary session for the respawn');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      () => document.body.textContent.includes('chunk-four-done')
        || document.body.textContent.includes('steering-followup-reply'),
      'D: prompt round-trip did not complete before the kill',
      30000
    );

    await page.fill('#prompt-input', '/code-review');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      () => window.__crDelay.delayedOpenIds.length > 0,
      'D: code_review_open was not observed on the live socket',
      10000
    );
    const delayGen = await page.evaluate(() => window.__crDelay.generation);
    if (!delayGen) fail('D: WebSocket wrapper did not observe a connection');
    const delayOpen = await page.evaluate(() => window.__crDelay.delayedOpenIds[0]);
    if (!delayOpen || !delayOpen.id) fail(`D: delayed code_review_open id missing: ${JSON.stringify(delayOpen)}`);
    await assertPendingOpenUnmounted(page, 'D: immediately after open send');
    // D3 baseline: pongs counted so far (T1-T6 pings may already have fired).
    // The delay window (>=30s) must add at least one more JSON pong — proof
    // the heartbeat probe exchanged frames DURING the pending open.
    const pongsBefore = await page.evaluate(() => window.__crDelay.pongs);

    // D2: unrelated get_state round-trips mid-delay on the SAME generation.
    await page.waitForTimeout(5000);
    const mid = await page.evaluate(() => {
      const ws = window.__crDelay.ws;
      const socketOpen = !!ws && ws.readyState === WebSocket.OPEN;
      const sessionId = document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '';
      if (!socketOpen) {
        return { ok: false, reason: `socket not open (readyState=${ws && ws.readyState})` };
      }
      if (!sessionId) return { ok: false, reason: 'no canonical active sidebar row sessionId for get_state' };
      const id = `cr-delay-state-mid-${Date.now()}`;
      const prev = ws.onmessage;
      ws.onmessage = function (ev) {
        try {
          const parsed = typeof ev.data === 'string' ? JSON.parse(ev.data) : null;
          if (parsed && parsed.type === 'response' && parsed.id === id) {
            window.__crDelay.getState = { ...(window.__crDelay.getState || {}), mid: parsed.success === true };
          }
        } catch {
          /* ignore */
        }
        if (typeof prev === 'function') prev.call(ws, ev);
      };
      ws.send(JSON.stringify({ type: 'get_state', id, sessionId }));
      return { ok: true, socketOpen, generation: window.__crDelay.generation, closes: window.__crDelay.closes };
    });
    if (!mid.ok) fail(`D2: mid-delay get_state could not send: ${mid.reason}`);
    await waitFor(
      page,
      () => window.__crDelay.getState && window.__crDelay.getState.mid === true,
      'D2: mid-delay get_state did not round-trip',
      10000
    );
    const midState = await page.evaluate(() => ({
      socketOpen: !!window.__crDelay.ws && window.__crDelay.ws.readyState === WebSocket.OPEN,
      generation: window.__crDelay.generation,
      closes: window.__crDelay.closes,
    }));
    if (!midState.socketOpen) fail('D2: socket left OPEN mid-delay');
    if (midState.generation !== delayGen) fail(`D2: socket generation changed mid-delay: ${delayGen} -> ${midState.generation}`);
    if (midState.closes.length) fail(`D2: socket closed mid-delay: ${JSON.stringify(midState.closes)}`);
    await assertPendingOpenUnmounted(page, 'D2');

    // D4: same get_state round-trips again PAST 30s (old stale deadline).
    await page.waitForTimeout(28000);
    const late = await page.evaluate(() => {
      const ws = window.__crDelay.ws;
      const socketOpen = !!ws && ws.readyState === WebSocket.OPEN;
      const sessionId = document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '';
      if (!socketOpen) {
        return { ok: false, reason: `socket not open (readyState=${ws && ws.readyState})` };
      }
      if (!sessionId) return { ok: false, reason: 'no canonical active sidebar row sessionId for get_state' };
      const id = `cr-delay-state-late-${Date.now()}`;
      const prev = ws.onmessage;
      ws.onmessage = function (ev) {
        try {
          const parsed = typeof ev.data === 'string' ? JSON.parse(ev.data) : null;
          if (parsed && parsed.type === 'response' && parsed.id === id) {
            window.__crDelay.getState = { ...(window.__crDelay.getState || {}), late: parsed.success === true };
          }
        } catch {
          /* ignore */
        }
        if (typeof prev === 'function') prev.call(ws, ev);
      };
      ws.send(JSON.stringify({ type: 'get_state', id, sessionId }));
      return { ok: true, socketOpen, generation: window.__crDelay.generation, closes: window.__crDelay.closes };
    });
    if (!late.ok) fail(`D4: late get_state could not send: ${late.reason}`);
    await waitFor(
      page,
      () => window.__crDelay.getState && window.__crDelay.getState.late === true,
      'D4: late (>30s) get_state did not round-trip',
      10000
    );
    const lateState = await page.evaluate(() => ({
      socketOpen: !!window.__crDelay.ws && window.__crDelay.ws.readyState === WebSocket.OPEN,
      generation: window.__crDelay.generation,
      closes: window.__crDelay.closes,
      pongs: window.__crDelay.pongs,
      transportToasts: document.querySelectorAll('#toasts .toast[data-toast-kind="transport"]').length,
      inlineError: document.querySelector('.code-review__error')?.textContent || null,
    }));
    if (!lateState.socketOpen) fail('D4: socket left OPEN after 30s');
    if (lateState.generation !== delayGen) fail(`D4: socket generation changed after 30s: ${delayGen} -> ${lateState.generation}`);
    if (lateState.closes.length) fail(`D4: socket closed after 30s: ${JSON.stringify(lateState.closes)}`);
    if (lateState.transportToasts !== 0) fail(`D4: transport toast during delay: ${lateState.transportToasts}`);
    if ((lateState.inlineError || '').includes('connection closed')) {
      fail(`D4: transport error rendered inline during delay: ${lateState.inlineError}`);
    }
    await page.screenshot({ path: `${evidence}/code-review-delay-alive.png`, fullPage: true });
    await page.setViewportSize({ width: 390, height: 844 });
    await page.screenshot({ path: `${evidence}/code-review-delay-alive-mobile.png`, fullPage: true });
    await page.setViewportSize({ width: 1280, height: 800 });

    // D5: the delayed response lands (the open settles) with no transport
    // toast and no inline error; the panel renders the comparison label.
    await waitFor(
      page,
      () => document.getElementById('code-review-panel') !== null
        && document.querySelector('.code-review__loader') === null
        && (document.querySelector('.code-review__label')?.textContent || '').includes('HEAD')
        && (document.querySelector('.code-review__label')?.textContent || '').includes('working tree'),
      'D5: delayed open never settled the HEAD→working tree panel',
      OPEN_DELAY_MS + 15000
    );
    const settled = await page.evaluate(() => ({
      socketOpen: !!window.__crDelay.ws && window.__crDelay.ws.readyState === WebSocket.OPEN,
      generation: window.__crDelay.generation,
      closes: window.__crDelay.closes,
      pongs: window.__crDelay.pongs,
      panel: document.getElementById('code-review-panel') !== null,
      loader: document.querySelector('.code-review__loader') !== null,
      label: document.querySelector('.code-review__label')?.textContent || '',
      inlineError: document.querySelector('.code-review__error')?.textContent || null,
      transportToasts: document.querySelectorAll('#toasts .toast[data-toast-kind="transport"]').length,
    }));
    if (!settled.socketOpen) fail('D5: socket left OPEN after delayed open');
    if (settled.generation !== delayGen) fail(`D5: generation changed after delayed open: ${delayGen} -> ${settled.generation}`);
    if (settled.closes.length) fail(`D5: socket closed after delayed open: ${JSON.stringify(settled.closes)}`);
    if (settled.loader) fail('D5: panel still shows the open loader after the delayed response landed');
    if (!settled.label.includes('HEAD') || !settled.label.includes('working tree')) {
      fail(`D5: panel did not settle to the HEAD→working tree snapshot (label=${JSON.stringify(settled.label)})`);
    }
    if (settled.transportToasts !== 0) fail(`D5: transport toast after delayed open: ${settled.transportToasts}`);
    if ((settled.inlineError || '').includes('connection closed') || (settled.inlineError || '').includes('1006')) {
      fail(`D5: transport error rendered inline after open: ${settled.inlineError}`);
    }
    // D3: the heartbeat probe provably exchanged a JSON pong DURING the
    // delay window (>=30s with the open pending): the count grew past the
    // arm-time baseline.
    if (settled.pongs <= pongsBefore) {
      fail(`D3: no JSON pong observed during the delay window (heartbeat probe dead): ${JSON.stringify({ pongsBefore, pongs: settled.pongs, openId: delayOpen.id })}`);
    }
    await page.screenshot({ path: `${evidence}/code-review-delay-settled.png`, fullPage: true });

    // D6: REAL abnormal close — the lane kills the rpi listener process so
    // the transport is severed at the socket level (no WebSocket.close call,
    // no synthetic close code): the close handshake can never complete and
    // Chromium reports the genuine abnormal-closure code 1006. The lane
    // respawns the listener on the SAME port; the shim fails the first three
    // product reconnect attempts synchronously before any TCP socket opens so
    // Chromium never manufactures native ERR_CONNECTION_REFUSED console
    // errors while the port is down (the reconnect lane's proven seam).
    const preKillPanel = await page.evaluate(() => ({
      socketOpen: !!window.__crDelay.ws && window.__crDelay.ws.readyState === WebSocket.OPEN,
      generation: window.__crDelay.generation,
      panel: document.getElementById('code-review-panel') !== null,
      label: document.querySelector('.code-review__label')?.textContent || '',
      closes: window.__crDelay.closes.slice(),
    }));
    const primarySessionId = await page.evaluate(() =>
      document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || ''
    );
    if (!primarySessionId) fail('D6: no active primary session id for the respawn --resume');
    fs.writeFileSync(`${work}/listener-primary-session-id`, `${primarySessionId}\n`);
    await page.evaluate(() => {
      window.__crDelayTest.failNextAttempts(3);
    });
    fs.writeFileSync(`${work}/kill-server.marker`, 'kill now\n');
    // The close event (observed on the wrapper) is the kill authority — the
    // #conn-state element is gone from the product.
    await waitFor(
      page,
      () => (window.__crDelay.closes || []).length > 0,
      'D6: real listener kill did not close the live socket',
      30000
    );
    // The toast render follows the onclose handler's state update; wait for
    // it so the exact-count sample below is race-free.
    await waitFor(
      page,
      () => document.querySelectorAll('#toasts .toast[data-toast-kind="transport"]').length === 1,
      'D6: transport toast did not appear for the outage',
      10000
    );
    // D6: exactly one close on the killed generation AND exactly one
    // transport toast for the outage (the onclose handler publishes the
    // truthful close cause). Sampled while the socket is still down so the
    // toast cannot have auto-dismissed yet.
    const outageCloseState = await page.evaluate(() => ({
      closes: window.__crDelay.closes.slice(),
      generation: window.__crDelay.generation,
      socketOpen: !!window.__crDelay.ws && window.__crDelay.ws.readyState === WebSocket.OPEN,
      transportToasts: document.querySelectorAll('#toasts .toast[data-toast-kind="transport"]').length,
      attempts: window.__crDelay.attempts,
      failedAttempts: window.__crDelay.failedAttempts.slice(),
    }));
    const killCloses = outageCloseState.closes.filter((c) => c.gen === delayGen);
    if (killCloses.length !== 1 || killCloses[0].code !== 1006) {
      fail(`D6: expected exactly one 1006 on the killed generation (real listener kill — no synthetic close(code) is used), got ${JSON.stringify(killCloses)}`);
    }
    if (outageCloseState.transportToasts !== 1) {
      fail(`D6: expected exactly one transport toast after the real 1006, got ${outageCloseState.transportToasts}`);
    }
    fs.writeFileSync(`${work}/outage-observed.marker`, 'real close code 1006 observed\n');
    // The lane respawns the listener on the same port. The three shimmed
    // failed generations prove the product keeps retrying while the port is
    // down; then the respawned listener accepts and the client recovers.
    const failedBaseline = outageCloseState.failedAttempts.length;
    await waitFor(
      page,
      (baseline) => (window.__crDelay.failedAttempts || []).length >= baseline + 3,
      'D7: the product did not execute three failed reconnect attempts during the outage',
      40000,
      failedBaseline
    );
    // The lane respawns the listener on the same port and writes
    // server-up.marker; the next product reconnect attempt is the real one.
    await waitForFile(`${work}/server-up.marker`, 'D7: lane never respawned the listener');
    // Recovery authority is the END STATE (pill-independent): the wrapper saw
    // the new generation's socket OPEN, the panel remounted after the
    // reconnect bootstrap re-bound the resumed primary, and the fresh
    // openReview settled (loader cleared, comparison label reconciled). The
    // epoch reconcile then re-opens the review once the respawned listener is
    // authoritative (the controller died with the killed process — the probe
    // reports no open review and the panel reopens, showing a fresh loader).
    // Require the settled state to PERSIST so the sample cannot race that
    // reopen (event-gated polling, no fixed sleep).
    await waitFor(
      page,
      (killedGen) => {
        if (!(window.__crDelay.generation > killedGen
          && window.__crDelay.ws && window.__crDelay.ws.readyState === WebSocket.OPEN)) {
          window.__crDelay._settledAt = 0;
          return false;
        }
        const panel = document.getElementById('code-review-panel');
        if (!panel || panel.querySelector('.code-review__loader')) {
          window.__crDelay._settledAt = 0;
          return false;
        }
        const label = panel.querySelector('.code-review__label')?.textContent || '';
        if (!(label.includes('HEAD') && label.includes('working tree'))) {
          window.__crDelay._settledAt = 0;
          return false;
        }
        const now = Date.now();
        if (!window.__crDelay._settledAt) window.__crDelay._settledAt = now;
        return now - window.__crDelay._settledAt >= 1500;
      },
      'D7: did not reach the recovered panel state after the listener respawned',
      90000,
      delayGen
    );
    // The transport toast clears after recovery (same contract as the
    // reconnect lane); the D6 assertion already pinned the exact count while
    // the outage was live.
    await waitFor(
      page,
      () => document.querySelectorAll('#toasts .toast[data-toast-kind="transport"]').length === 0,
      'D7: transport toast did not clear after recovery',
      15000
    );
    const recovered = await page.evaluate(() => ({
      socketOpen: !!window.__crDelay.ws && window.__crDelay.ws.readyState === WebSocket.OPEN,
      generation: window.__crDelay.generation,
      closes: window.__crDelay.closes,
      pongs: window.__crDelay.pongs,
      panel: document.getElementById('code-review-panel') !== null,
      label: document.querySelector('.code-review__label')?.textContent || '',
      loader: document.querySelector('.code-review__loader') !== null,
      inlineError: document.querySelector('.code-review__error')?.textContent || null,
      sidebarError: document.querySelector('.session-sidebar__error')?.textContent || null,
      transportToasts: document.querySelectorAll('#toasts .toast[data-toast-kind="transport"]').length,
      abort: Array.from(document.querySelectorAll('button.code-review__action--stop'))
        .some((b) => (b.textContent || '').includes('Stop')),
      streaming: document.querySelector('.code-review__streaming') !== null,
      hunkDot: document.querySelector('.code-review__hunk .code-review__streaming-dot') !== null,
      busy: document.querySelector('.code-review__busy') !== null,
    }));
    if (!recovered.socketOpen) fail('D7: socket not OPEN after recovery');
    if (recovered.generation <= delayGen) fail(`D7: post-kill generation did not advance: ${JSON.stringify(recovered)}`);
    if (recovered.closes.length !== outageCloseState.closes.length) {
      fail(`D7: unexpected extra closes after recovery: ${JSON.stringify(recovered.closes)}`);
    }
    if (recovered.transportToasts !== 0) {
      fail(`D7: transport toast survived recovery: ${recovered.transportToasts}`);
    }
    if (!recovered.panel) fail('D7: panel unmounted across the reconnect (must stay mounted)');
    if (recovered.loader) fail('D7: panel stuck on the open loader after reconnect');
    if (!recovered.label.includes('HEAD') || !recovered.label.includes('working tree')) {
      fail(`D7: panel did not reconcile the retained controller (label=${JSON.stringify(recovered.label)})`);
    }
    if ((recovered.inlineError || '').includes('connection closed') || (recovered.inlineError || '').includes('1006')) {
      fail(`D7: transport error rendered inline after reconnect: ${recovered.inlineError}`);
    }
    if (recovered.sidebarError) fail(`D7: sidebar error after reconnect: ${recovered.sidebarError}`);
    if (recovered.abort || recovered.streaming || recovered.hunkDot || recovered.busy) {
      fail(`L4: reconnect recovery must stay terminal-idle (got ${JSON.stringify({
        abort: recovered.abort,
        streaming: recovered.streaming,
        hunkDot: recovered.hunkDot,
        busy: recovered.busy,
      })})`);
    }
    await page.screenshot({ path: `${evidence}/code-review-delay-recovered.png`, fullPage: true });

    // D8: the whole segment ran with ZERO browser console/page errors.
    const cleanConsole = consoleErrors.filter((t) => !t.includes('favicon'));
    if (pageErrors.length) fail(`D8: page errors: ${JSON.stringify(pageErrors)}`);
    if (cleanConsole.length) fail(`D8: console errors: ${JSON.stringify(cleanConsole)}`);
    const sentParsed = socketFrames.sent.map((f) => parseFrame(f.raw)).filter(Boolean);
    const delaySummary = {
      delayMs: OPEN_DELAY_MS,
      openCommand: delayOpen,
      midState,
      lateState,
      settled: {
        generation: settled.generation,
        closes: settled.closes,
        pongs: settled.pongs,
        label: settled.label,
        transportToasts: settled.transportToasts,
        inlineError: settled.inlineError,
      },
      killCloses,
      recovered: {
        socketOpen: recovered.socketOpen,
        generation: recovered.generation,
        panel: recovered.panel,
        label: recovered.label,
        transportToasts: recovered.transportToasts,
        inlineError: recovered.inlineError,
        sidebarError: recovered.sidebarError,
      },
      sentOpen: sentParsed.filter((f) => f && f.type === 'code_review_open'),
      sentGetState: sentParsed.filter((f) => f && f.type === 'get_state').slice(-6),
      pongsOnWire: socketFrames.received.map((f) => parseFrame(f.raw)).filter((f) => f && f.type === 'pong').length,
      pageErrors,
      consoleErrors: cleanConsole,
    };
    fs.writeFileSync(`${evidence}/code-review-delayed-open.json`, JSON.stringify(delaySummary, null, 2));

    console.log('web-code-review-paging: PASSED (nested tree collapse/expand; authoritative header +N/-M totals with no `· truncated` chip; >4000-line diff paged with NO Load buttons — eager backend pages with exact cursor progression 0→…→8000, scroll-grown DOM window 4000→8200 to the terminal line, no banner/status; >20000-line UI-cap fixture pinned at 20000 with the precise Loaded 20,000 of 44,000 status; per-file byte-cap fixture loaded on selection with byteCapped=true on every page, exact nextCursor-linked chain, terminal hasMore=false over the parsed count, and the distinct byte-cap status; empty globally-truncated placeholder auto-loads via one terminal backend page with no truncated marker; loaded placeholder hunk beyond global truncation is selectable + commentable with an accepted thread card; user + assistant comment markdown bold/list/rust fence with hostile HTML literal and no side effect; /code-review HEAD single-base open + tool-path live Abort/dot then terminal idle + late rewritten snapshot stays idle + reconnect idle; responsive resize across the 900px breakpoint surfaces the Files/Diff/Thread tab bar, tab clicks drive the single-pane narrow UI, Back-to-diff returns, resize back removes the tab bar; >30s delayed code_review_open keeps the SAME socket generation alive — zero closes, mid + late get_state round-trips, JSON pong, no toast/inline error, panel settles to HEAD→working tree; real 1006 drop then produces exactly one close + one transport toast, reconnect advances the generation and the panel reconciles the retained controller with zero browser errors)');
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-code-review-paging: FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});
