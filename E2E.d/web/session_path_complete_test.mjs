// Session path-completion + authenticated catalog web E2E lane (playwright
// half of E2E.d/web/session_path_complete.sh).
//
// Environment:
//   RPI_URL            http://127.0.0.1:<port>/web
//   RPI_TOKEN          the GOOD token (file content, rpi-auth.<token> subprotocol)
//   RPI_COMPLETION_DIR absolute path of an existing directory under the
//                      displayed session's workspace root (the completion target)
//   RPI_WORKSPACE      the workspace root the listener session runs in
//   RPI_EVIDENCE       evidence dir for screenshots
//
// Asserts against the REAL tokened backend:
//   1. Token auth + catalog: the token_required hold renders zero sidebar
//      rows and emits zero frames/responses/sockets; the good token opens
//      EXACTLY one socket whose chosen subprotocol is exactly
//      rpi-auth.<token>; the session_list catalog loads only over that
//      authenticated socket and matches the rendered rows exactly
//   2. Renamed identity: the lane renames the session through the panel so
//      BOTH authoritative names (active session_list row + get_state) are
//      nonempty, agree with each other and with the rendered panel name
//   3. Success: filling a valid existing workdir prefix under the displayed
//      session's authoritative cwd emits EXACTLY one path_complete request
//      (exact shape: kind=directory, limit=100, sessionId=active) with
//      EXACTLY one id-correlated success response carrying exactly
//      {name, path=completionDir, kind:'directory'}; the menu renders
//      exactly that option; clicking it sets the input to
//      completionDir + '/' with the menu closed (aria-expanded=false)
//   4. Missing in-workspace + manual failure: EXACTLY one exact-shape probe
//      with EXACTLY one correlated empty reply, menu closed, manual input
//      untouched, zero toasts; manual Enter emits EXACTLY one new_session +
//      EXACTLY one correlated failure with its committed owned toast
// Every phase snapshots frame/response baselines: exactly one NEW matching
// wire frame and exactly one ALL id-correlated response — duplicates,
// extra shapes, and malformed frames fail the lane. Zero toasts of ANY kind
// in clean phases; zero console/page errors gated at the end. The token
// value never crosses into a log line.

import { chromium } from 'playwright';
import { writeFileSync } from 'node:fs';

import { isAbsolute, relative as pathRelative, resolve as pathResolve } from 'node:path';
const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const completionDir = process.env.RPI_COMPLETION_DIR || '';
const workspace = process.env.RPI_WORKSPACE || '';
const evidence = process.env.RPI_EVIDENCE || '.';
const completionName = completionDir.split('/').filter(Boolean).pop() || '';
const sessionName = 'path-complete-e2e';

const SIDEBAR_ERROR_STRINGS = [
  'load failed',
  'session_list failed',
  'too many concurrent RPC commands',
  'workflow_detail',
  'session switch pending',
];

function fail(message) {
  // Exit 2 (not 1): 1 means "playwright setup failure (node/chromium/npm)".
  console.error(`web-session-path-complete: FAIL: ${message}`);
  process.exit(2);
}

async function waitFor(page, fn, label, timeoutMs = 25000, arg) {
  try {
    await page.waitForFunction(fn, arg, { timeout: timeoutMs });
  } catch {
    fail(`${label} (timeout ${timeoutMs}ms)`);
  }
}

/** Like waitFor, but resolves to the predicate's final value. */
async function waitForValue(page, fn, label, timeoutMs = 25000, arg) {
  try {
    const handle = await page.waitForFunction(fn, arg, { timeout: timeoutMs });
    return await handle.jsonValue();
  } catch {
    return fail(`${label} (timeout ${timeoutMs}ms)`);
  }
}

/** Surface scan for a clean phase: zero toasts of ANY kind, zero
 *  session-panel status nodes, zero sidebar error nodes, none of the
 *  sidebar error strings in the body (minus #toasts), and zero
 *  surface events since the phase baseline. */
async function assertCleanSurface(page, label, surfaceBase = 0) {
  const surface = await page.evaluate(({ banned, base }) => {
    const toasts = document.getElementById('toasts')?.textContent || '';
    const body = (document.body.textContent || '').replace(toasts, '');
    return {
      toasts: document.querySelectorAll('#toasts .toast').length,
      sidebarErrors: document.querySelectorAll('.session-sidebar__error').length,
      panelStatus: document.querySelectorAll('#session-panel .panel__status').length,
      errorStrings: banned.filter((needle) => body.includes(needle)),
      surfaceEvents: window.__rpiSurfaceEvents.length - base,
    };
  }, { banned: SIDEBAR_ERROR_STRINGS, base: surfaceBase });
  if (surface.toasts !== 0) fail(`${label}: ${surface.toasts} toast node(s)`);
  if (surface.sidebarErrors !== 0) fail(`${label}: sidebar rendered ${surface.sidebarErrors} error node(s)`);
  if (surface.panelStatus !== 0) fail(`${label}: session panel rendered a status node`);
  if (surface.errorStrings.length !== 0) {
    fail(`${label}: body carries error strings: ${surface.errorStrings.join(' | ')}`);
  }
  if (surface.surfaceEvents !== 0) {
    fail(`${label}: ${surface.surfaceEvents} surface event(s) (toast/status/error) since the phase baseline`);
  }
  return surface;
}

async function assertNoBrowserErrors(page, consoleErrors, pageErrors, label) {
  if (pageErrors.length !== 0) fail(`${label}: page errors: ${pageErrors.join(' | ')}`);
  if (consoleErrors.length !== 0) fail(`${label}: console errors: ${consoleErrors.join(' | ')}`);
}

/** In-page gate: EXACTLY one new frame of `info.type` after the frame
 *  baseline whose keys match `info.expect` exactly, then EXACTLY one
 *  id-correlated response after the response baseline whose payload matches
 *  `info.reply`. Returns {ok, detail} so the caller can fail with the
 *  precise wire violation. */
function wireGate(page, spec) {
  return waitForValue(
    page,
    (info) => {
      const frames = window.__rpiFrames
        .slice(info.frameBase)
        .filter((f) => f.type === info.type);
      if (frames.length === 0) return null;
      if (frames.length !== 1) {
        return { ok: false, detail: `expected exactly one new ${info.type} frame, got ${frames.length}: ${JSON.stringify(frames)}` };
      }
      const request = frames[0];
      const requestKeys = Object.keys(request).sort().join(',');
      if (requestKeys !== info.keys) {
        return { ok: false, detail: `${info.type} frame keys must be exactly [${info.keys}], got [${requestKeys}]: ${JSON.stringify(request)}` };
      }
      if (typeof request.id !== 'string' || request.id === '') {
        return { ok: false, detail: `${info.type} frame id must be a nonempty string: ${JSON.stringify(request)}` };
      }
      for (const [key, expected] of Object.entries(info.expect)) {
        if (request[key] !== expected) {
          return { ok: false, detail: `${info.type} frame key ${key} must be ${JSON.stringify(expected)}, got ${JSON.stringify(request[key])}: ${JSON.stringify(request)}` };
        }
      }
      const replies = window.__rpiResponses
        .slice(info.responseBase)
        .filter((r) => r.id === request.id);
      if (replies.length === 0) return null;
      if (replies.length !== 1) {
        return { ok: false, detail: `expected exactly one ${info.type} response for id ${request.id}, got ${replies.length}: ${JSON.stringify(replies)}` };
      }
      const reply = replies[0];
      if (reply.type !== 'response' || reply.command !== info.type) {
        return { ok: false, detail: `${info.type} response must be type=response command=${info.type}, got type=${JSON.stringify(reply.type)} command=${JSON.stringify(reply.command)}` };
      }
      const envelopeKeys = Object.keys(reply).sort();
      if (JSON.stringify(envelopeKeys) !== JSON.stringify(info.envelopeKeys.slice().sort())) {
        return { ok: false, detail: `${info.type} response envelope keys must be exactly ${JSON.stringify(info.envelopeKeys)}, got ${JSON.stringify(envelopeKeys)}: ${JSON.stringify(reply)}` };
      }
      if (reply.sessionId !== info.owner) {
        return { ok: false, detail: `${info.type} response sessionId must be the owner ${JSON.stringify(info.owner)}, got ${JSON.stringify(reply.sessionId)}` };
      }
      if (reply.success !== info.reply.success) {
        return { ok: false, detail: `${info.type} response success must be ${info.reply.success}: ${JSON.stringify(reply)}` };
      }
      if (info.reply.errorIncludes !== undefined
        && !(reply.error || '').includes(info.reply.errorIncludes)) {
        return { ok: false, detail: `${info.type} failure text must include ${JSON.stringify(info.reply.errorIncludes)}: ${JSON.stringify(reply)}` };
      }
      if (info.reply.entry !== undefined || info.reply.entriesEmpty === true) {
        const payloadKeys = Object.keys(reply.data || {}).sort().join(',');
        if (payloadKeys !== 'base,entries,prefix,truncated') {
          return { ok: false, detail: `${info.type} payload keys must be exactly [base,entries,prefix,truncated], got [${payloadKeys}]: ${JSON.stringify(reply.data)}` };
        }
        if (typeof reply.data.base !== 'string' || typeof reply.data.prefix !== 'string'
          || typeof reply.data.truncated !== 'boolean'
          || reply.data.base !== info.reply.base
          || reply.data.prefix !== info.reply.prefix
          || reply.data.truncated !== false) {
          return { ok: false, detail: `${info.type} payload must be base=${JSON.stringify(info.reply.base)} prefix=${JSON.stringify(info.reply.prefix)} truncated=false, got ${JSON.stringify(reply.data)}` };
        }
        const entries = reply.data.entries;
        if (info.reply.entry !== undefined) {
          const exact = entries.filter(
            (entry) => Object.keys(entry).sort().join(',') === 'kind,name,path'
              && entry.name === info.reply.entry.name
              && entry.path === info.reply.entry.path
              && entry.kind === info.reply.entry.kind,
          );
          if (entries.length !== 1 || exact.length !== 1) {
            return { ok: false, detail: `${info.type} entries must be exactly one exact-key ${JSON.stringify(info.reply.entry)}, got ${JSON.stringify(entries)}` };
          }
        }
        if (info.reply.entriesEmpty === true && entries.length !== 0) {
          return { ok: false, detail: `${info.type} entries must be an empty array, got ${JSON.stringify(entries)}` };
        }
      }
      return { ok: true, id: request.id };
    },
    spec.label,
    spec.timeoutMs ?? 15000,
    spec.info,
  );
}

/** Post-gate stability: a real bounded interval in which NO new frame or
 *  response may arrive and the surface must stay continually empty (or
 *  hold exactly the one owned toast), then a re-count proving the phase
 *  still holds exactly its single wire pair. Surface events are tracked by
 *  a MutationObserver ledger so transient flashes never escape. */
async function awaitStablePair(page, info, label) {
  await page.evaluate(() => new Promise((resolve) => {
    requestAnimationFrame(() => requestAnimationFrame(resolve));
  }));
  const recount = await page.evaluate((bounds) => new Promise((resolve) => {
    const start = Date.now();
    const timer = setInterval(() => {
      const toasts = [...document.querySelectorAll('#toasts .toast')].map((t) => t.textContent || '');
      const toastOk = bounds.ownedToast === undefined
        ? toasts.length === 0
        : toasts.length === 1 && toasts[0] === bounds.ownedToast;
      const events = window.__rpiSurfaceEvents.slice(bounds.surfaceBase);
      const eventsOk = bounds.ownedToast === undefined
        ? events.length === 0
        : events.length === 1 && events[0].kind === 'toast' && events[0].change === 'added' && events[0].text === bounds.ownedToast;
      const dirty = !toastOk
        || !eventsOk
        || document.querySelectorAll('#session-panel .panel__status').length !== 0
        || document.querySelectorAll('.session-sidebar__error').length !== 0;
      const settled = Date.now() - start >= 500;
      if (dirty || settled) {
        clearInterval(timer);
        resolve({
          dirty,
          frames: window.__rpiFrames.slice(bounds.frameBase).filter((f) => f.type === bounds.type).length,
          replies: window.__rpiResponses.slice(bounds.responseBase)
            .filter((r) => r.id === bounds.id).length,
          toasts,
        });
      }
    }, 50);
  }), {
    frameBase: info.frameBase,
    responseBase: info.responseBase,
    surfaceBase: info.surfaceBase ?? 0,
    type: info.type,
    id: info.id,
    ownedToast: info.ownedToast,
  });
  if (recount.dirty) {
    fail(`${label}: surface did not stay settled for 500ms (toasts=${JSON.stringify(recount.toasts)})`);
  }
  if (recount.frames !== 1 || recount.replies !== 1) {
    fail(`${label}: wire pair must stay exactly one request + one correlated response (frames=${recount.frames}, replies=${recount.replies})`);
  }
}


async function main() {
  if (!url) fail('RPI_URL is required');
  if (!completionDir) fail('RPI_COMPLETION_DIR is required');
  if (!workspace) fail('RPI_WORKSPACE is required');
  if (!completionName) fail('RPI_COMPLETION_DIR must end in a directory name');
  const launchOptions = process.env.RPI_CHROME ? { executablePath: process.env.RPI_CHROME } : {};
  const browser = await chromium.launch(launchOptions);
  const consoleErrors = [];
  const pageErrors = [];
  try {
    const page = await browser.newPage();
    page.on('pageerror', (err) => {
      pageErrors.push(String(err && err.message ? err.message : err));
      console.error(`web-session-path-complete: page error: ${err.message}`);
    });
    page.on('console', (msg) => {
      if (msg.type() === 'error') consoleErrors.push(msg.text());
    });
    await page.addInitScript((goodToken) => {
      window.__rpiFrames = [];
      window.__rpiResponses = [];
      window.__rpiSockets = [];
      const NativeWebSocket = window.WebSocket;
      const NativeParse = JSON.parse;
      class ProbeWebSocket extends NativeWebSocket {
        constructor(...args) {
          super(...args);
          const socket = {
            offered: Array.from(args[1] ?? []).map(String),
            protocol: '',
            opened: false,
          };
          window.__rpiSockets.push(socket);
          this.addEventListener('open', () => {
            socket.opened = true;
            socket.protocol = this.protocol || '';
          });
          this.addEventListener('message', (event) => {
            if (typeof event.data !== 'string') return;
            try {
              const value = NativeParse.call(this, event.data);
              if (value && typeof value === 'object') {
                window.__rpiResponses.push(value);
              }
            } catch { /* observability only */ }
          });
        }
        send(data) {
          if (typeof data === 'string') {
            try {
              const frame = NativeParse.call(this, data);
              if (['session_list', 'get_state', 'path_complete', 'new_session', 'set_session_name'].includes(frame?.type)) {
                window.__rpiFrames.push(frame);
              }
            } catch { /* observability only */ }
          }
          super.send(data);
        }
      }
      for (const key of ['CONNECTING', 'OPEN', 'CLOSING', 'CLOSED']) {
        Object.defineProperty(ProbeWebSocket, key, { value: NativeWebSocket[key] });
      }
      window.WebSocket = ProbeWebSocket;
      window.__rpiAuthProtocol = `rpi-auth.${goodToken}`;
      window.__rpiResponses = [];
      window.__rpiSurfaceEvents = [];
      const surfaceObserver = new MutationObserver((mutations) => {
        for (const mutation of mutations) {
          for (const node of mutation.addedNodes) {
            if (node.nodeType !== 1) continue;
            const toast = node.matches?.('#toasts .toast') ? node : node.querySelector?.('#toasts .toast');
            if (toast) {
              window.__rpiSurfaceEvents.push({ kind: 'toast', text: toast.textContent || '', change: 'added' });
              continue;
            }
            if (node.matches?.('#session-panel .panel__status, .session-sidebar__error')
              || node.querySelector?.('#session-panel .panel__status, .session-sidebar__error')) {
              window.__rpiSurfaceEvents.push({ kind: 'status', text: '', change: 'added' });
            }
          }
          for (const node of mutation.removedNodes) {
            if (node.nodeType !== 1) continue;
            const toast = node.matches?.('#toasts .toast') ? node : node.querySelector?.('#toasts .toast');
            if (toast) {
              window.__rpiSurfaceEvents.push({ kind: 'toast', text: toast.textContent || '', change: 'removed' });
              continue;
            }
            if (node.matches?.('#session-panel .panel__status, .session-sidebar__error')
              || node.querySelector?.('#session-panel .panel__status, .session-sidebar__error')) {
              window.__rpiSurfaceEvents.push({ kind: 'status', text: '', change: 'removed' });
            }
          }
        }
      });
      surfaceObserver.observe(document.documentElement || document, { childList: true, subtree: true });
    }, token);

    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'page title missing');

    await waitFor(
      page,
      () => document.getElementById('token-pairing-input') !== null
        && document.querySelector('.token-pairing__connect') !== null,
      'tokened boot never opened the pairing card in the token_required state'
    );
    const hold = await page.evaluate(() => ({
      sidebarRows: document.querySelectorAll('.session-sidebar__row').length,
      frames: window.__rpiFrames.length,
      responses: window.__rpiResponses.length,
      sockets: window.__rpiSockets.length,
      catalogResponses: window.__rpiResponses.filter(
        (r) => (r.command === 'session_list' || r.command === 'get_state') && r.success === true,
      ).length,
      surfaceEvents: window.__rpiSurfaceEvents.length,
    }));
    if (hold.sidebarRows !== 0) fail(`token_required hold rendered ${hold.sidebarRows} sidebar row(s); the catalog must load only after authentication`);
    if (hold.frames !== 0 || hold.responses !== 0 || hold.catalogResponses !== 0 || hold.sockets !== 0) {
      fail(`token_required hold emitted ${hold.frames} frame(s)/${hold.responses} response(s)/${hold.sockets} socket(s); zero catalog traffic may precede the good token`);
    }
    if (hold.surfaceEvents !== 0) {
      fail(`token_required hold emitted ${hold.surfaceEvents} surface event(s); zero toasts/status/errors may precede the good token`);
    }
    await page.fill('#token-pairing-input', token);
    await page.click('.token-pairing__connect');
    await waitFor(
      page,
      () => (document.querySelector('.empty-hint')?.textContent || '').includes('Send a prompt to start the session.'),
      'WS never reached connected with the good token'
    );
    const sockets = await page.evaluate((authProtocol) => {
      const opened = window.__rpiSockets.filter((s) => s.opened);
      return {
        total: window.__rpiSockets.length,
        opened: opened.length,
        authed: opened.filter((s) => s.protocol === authProtocol).length,
        offered: window.__rpiSockets.map((s) => s.offered),
      };
    }, `rpi-auth.${token}`);
    if (sockets.total !== 1 || sockets.opened !== 1 || sockets.authed !== 1) {
      fail(`exactly one socket generation expected (total=${sockets.total}, opens=${sockets.opened}, authed=${sockets.authed})`);
    }
    const expectedProtocols = JSON.stringify([`rpi-auth.${token}`]);
    if (sockets.offered.some((offered) => JSON.stringify(offered) !== expectedProtocols)) {
      fail(`every socket attempt must offer exactly [rpi-auth.\u003ctoken\u003e]: ${JSON.stringify(sockets.offered)}`);
    }

    await page.click('#session-toggle-btn');
    await waitFor(page, () => document.getElementById('session-panel') !== null, 'session panel did not open');
    await waitFor(
      page,
      () => document.querySelector('[data-testid="session-name-value"]') !== null,
      'session panel info never rendered'
    );
    const renameBase = await page.evaluate(() => ({
      frames: window.__rpiFrames.length,
      responses: window.__rpiResponses.length,
      surface: window.__rpiSurfaceEvents.length,
    }));
    await page.fill('#session-rename-input', sessionName);
    await page.click('#session-rename-btn');
    const renamed = await waitForValue(
      page,
      (info) => {
        const frames = window.__rpiFrames.slice(info.frameBase).filter((f) => f.type === 'set_session_name');
        if (frames.length === 0) return null;
        if (frames.length !== 1) {
          return { ok: false, detail: `expected exactly one new set_session_name frame, got ${frames.length}` };
        }
        const request = frames[0];
        const requestKeys = Object.keys(request).sort().join(',');
        if (requestKeys !== 'id,name,sessionId,type') {
          return { ok: false, detail: `set_session_name keys must be exactly [id,name,sessionId,type], got [${requestKeys}]: ${JSON.stringify(request)}` };
        }
        if (typeof request.id !== 'string' || request.id === '') {
          return { ok: false, detail: `set_session_name id must be a nonempty string: ${JSON.stringify(request)}` };
        }
        const states = window.__rpiResponses.filter(
          (r) => r.command === 'get_state' && r.success === true && r.data?.sessionId,
        );
        const displayedId = states.length > 0 ? String(states[states.length - 1].data.sessionId) : '';
        if (request.sessionId !== displayedId) {
          return { ok: false, detail: `set_session_name sessionId must be the exact displayed id ${JSON.stringify(displayedId)}, got ${JSON.stringify(request.sessionId)}` };
        }
        if (request.name !== info.name) {
          return { ok: false, detail: `set_session_name name must be ${JSON.stringify(info.name)}, got ${JSON.stringify(request.name)}` };
        }
        const replies = window.__rpiResponses
          .slice(info.responseBase)
          .filter((r) => r.id === request.id);
        if (replies.length === 0) return null;
        if (replies.length !== 1) {
          return { ok: false, detail: `expected exactly one set_session_name response for ${request.id}, got ${replies.length}` };
        }
        if (replies[0].success !== true) return { ok: false, detail: `set_session_name failed: ${replies[0].error}` };
        const renameEnvelope = Object.keys(replies[0]).sort();
        if (JSON.stringify(renameEnvelope) !== JSON.stringify(['command', 'id', 'sessionId', 'success', 'type'])) {
          return { ok: false, detail: `set_session_name envelope keys must be exactly ['command','id','sessionId','success','type'], got ${JSON.stringify(renameEnvelope)}` };
        }
        if (replies[0].type !== 'response' || replies[0].command !== 'set_session_name' || replies[0].success !== true) {
          return { ok: false, detail: `set_session_name response must be type=response command=set_session_name success=true, got ${JSON.stringify(replies[0])}` };
        }
        if (replies[0].sessionId !== displayedId) {
          return { ok: false, detail: `set_session_name response sessionId must be the owner ${JSON.stringify(displayedId)}, got ${JSON.stringify(replies[0].sessionId)}` };
        }
        const rendered = document.querySelector('[data-testid="session-name-value"]')?.textContent || '';
        if (rendered !== info.name) return null;
        return { ok: true, displayedSessionId: displayedId, id: request.id };
      },
      'panel rename never settled with the rendered name',
      20000,
      { frameBase: renameBase.frames, responseBase: renameBase.responses, name: sessionName },
    );
    if (!renamed.ok) fail(`rename phase: ${renamed.detail}`);
    await awaitStablePair(page, {
      type: 'set_session_name',
      frameBase: renameBase.frames,
      responseBase: renameBase.responses,
      surfaceBase: renameBase.surface,
      id: renamed.id,
    }, 'rename phase');
    const displayedSessionId = renamed.displayedSessionId;

    await waitFor(
      page,
      () => document.querySelectorAll('.session-sidebar__row').length >= 1,
      'sidebar catalog never rendered any row over the tokened socket'
    );
    const catalogBase = await page.evaluate(() => ({
      frames: window.__rpiFrames.length,
      responses: window.__rpiResponses.length,
      surface: window.__rpiSurfaceEvents.length,
    }));
    const renamedCatalog = await waitForValue(
      page,
      (info) => {
        const frames = window.__rpiFrames.slice(info.frameBase).filter((f) => f.type === 'session_list');
        if (frames.length === 0) return null;
        if (frames.length !== 1) {
          return { ok: false, detail: `expected exactly one new session_list frame after the rename, got ${frames.length}: ${JSON.stringify(frames)}` };
        }
        const request = frames[0];
        const requestKeys = Object.keys(request).sort().join(',');
        if (requestKeys !== 'id,scope,sessionId,type') {
          return { ok: false, detail: `session_list frame keys must be exactly [id,scope,sessionId,type], got [${requestKeys}]: ${JSON.stringify(request)}` };
        }
        if (request.scope !== 'all_projects') {
          return { ok: false, detail: `session_list scope must be all_projects, got ${JSON.stringify(request.scope)}` };
        }
        if (request.sessionId !== info.owner) {
          return { ok: false, detail: `session_list sessionId must be the active owner ${JSON.stringify(info.owner)}, got ${JSON.stringify(request.sessionId)}` };
        }
        if (typeof request.id !== 'string' || request.id === '') {
          return { ok: false, detail: `session_list id must be a nonempty string: ${JSON.stringify(request)}` };
        }
        const replies = window.__rpiResponses
          .slice(info.responseBase)
          .filter((r) => r.id === request.id);
        if (replies.length === 0) return null;
        if (replies.length !== 1) {
          return { ok: false, detail: `expected exactly one session_list response for id ${request.id}, got ${replies.length}: ${JSON.stringify(replies)}` };
        }
        const reply = replies[0];
        const envelopeKeys = Object.keys(reply).sort();
        if (JSON.stringify(envelopeKeys) !== JSON.stringify(['command', 'data', 'id', 'sessionId', 'success', 'type'])) {
          return { ok: false, detail: `session_list response envelope keys must be exactly ['command','data','id','sessionId','success','type'], got ${JSON.stringify(envelopeKeys)}: ${JSON.stringify(reply)}` };
        }
        if (reply.type !== 'response' || reply.command !== 'session_list' || reply.success !== true) {
          return { ok: false, detail: `session_list response must be type=response command=session_list success=true, got ${JSON.stringify(reply)}` };
        }
        if (reply.sessionId !== info.owner) {
          return { ok: false, detail: `session_list response sessionId must be the owner ${JSON.stringify(info.owner)}, got ${JSON.stringify(reply.sessionId)}` };
        }
        if (!reply.data || typeof reply.data !== 'object' || Object.keys(reply.data).sort().join(',') !== 'sessions') {
          return { ok: false, detail: `session_list payload keys must be exactly [sessions], got ${JSON.stringify(reply.data)}` };
        }
        if (!Array.isArray(reply.data.sessions)) {
          return { ok: false, detail: `session_list response must carry a sessions array: ${JSON.stringify(reply)}` };
        }
        if (!reply.data.sessions.some(
          (row) => String(row.sessionId) === info.owner && String(row.name || '') === info.name,
        )) {
          return { ok: false, detail: `session_list response must carry the renamed active row: ${JSON.stringify(reply.data.sessions)}` };
        }
        return { ok: true, id: request.id, response: reply };
      },
      'sidebar poll never produced exactly one post-rename session_list with the renamed active row',
      10000,
      { frameBase: catalogBase.frames, responseBase: catalogBase.responses, owner: displayedSessionId, name: sessionName },
    );
    await awaitStablePair(page, {
      type: 'session_list',
      frameBase: catalogBase.frames,
      responseBase: catalogBase.responses,
      surfaceBase: catalogBase.surface,
      id: renamedCatalog.id,
    }, 'post-rename catalog pair');
    const catalog = await page.evaluate((wire) => {
      const states = window.__rpiResponses.filter(
        (r) => r.command === 'get_state' && r.success === true && r.data?.sessionId,
      );
      const rows = [...document.querySelectorAll('.session-sidebar__row')];
      return {
        sessionList: wire,
        getState: states.length > 0 ? states[states.length - 1] : null,
        dom: rows.map((row) => ({
          sessionId: row.querySelector('.session-sidebar__switch')?.dataset.sessionId || '',
          source: row.querySelector('.session-sidebar__switch')?.dataset.sessionSource || '',
        })),
        active: document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '',
      };
    }, renamedCatalog.response);
    if (catalog.getState === null) fail('captured no successful get_state response frame');
    if (catalog.sessionList === null) fail('captured no successful session_list response frame');
    const authoritative = catalog.sessionList.data.sessions;
    const authoritativeIds = authoritative.map((row) => String(row.sessionId || ''));
    if (authoritative.some((row) => !row.sessionId || !row.path || !row.cwd)) {
      fail(`authoritative catalog rows must carry exact identity: ${JSON.stringify(authoritative)}`);
    }
    const domIds = catalog.dom.map((row) => row.sessionId);
    if (JSON.stringify([...domIds].sort()) !== JSON.stringify([...authoritativeIds].sort())) {
      fail(`rendered catalog rows do not match the authoritative session_list: dom=${JSON.stringify(domIds)} wire=${JSON.stringify(authoritativeIds)}`);
    }
    for (const row of catalog.dom) {
      const wire = authoritative.find((row2) => String(row2.sessionId) === row.sessionId);
      if (!wire || String(wire.source || '') !== row.source) {
        fail(`row ${row.sessionId}: rendered source ${row.source} does not match the authoritative row ${JSON.stringify(wire)}`);
      }
    }
    if (catalog.active === '' || catalog.getState.data.sessionId === undefined
      || catalog.active !== String(catalog.getState.data.sessionId)) {
      fail(`active DOM row id ${JSON.stringify(catalog.active)} must be nonempty and exactly equal get_state sessionId ${JSON.stringify(catalog.getState.data.sessionId)}`);
    }
    const activeId = catalog.active;
    if (!authoritativeIds.includes(activeId)) {
      fail(`active session ${activeId} is not an exact catalog row identity: ${JSON.stringify(authoritativeIds)}`);
    }
    const activeWire = authoritative.find((row) => String(row.sessionId) === activeId);
    if (!activeWire || String(activeWire.status || '') !== 'native') {
      fail(`active catalog row must be the native primary: ${JSON.stringify(activeWire)}`);
    }
    if (!activeWire.path || String(activeWire.cwd || '') !== String(catalog.getState.data.cwd)) {
      fail(`active row path/cwd ${JSON.stringify(activeWire)} must be nonempty and match get_state cwd ${JSON.stringify(catalog.getState.data.cwd)}`);
    }
    if (!activeWire.name) fail(`active catalog row name must be nonempty after the rename: ${JSON.stringify(activeWire)}`);
    await page.screenshot({ path: `${evidence}/01-catalog-loaded.png`, fullPage: true });

    const panel = await page.evaluate(() => ({
      id: (() => {
        const dts = [...document.querySelectorAll('#session-panel dl dt')];
        const idx = dts.findIndex((dt) => dt.textContent === 'Session id');
        return idx >= 0 ? (dts[idx]?.nextElementSibling?.textContent || '').trim() : '';
      })(),
      name: document.querySelector('[data-testid="session-name-value"]')?.textContent || '',
      cwd: document.querySelector('[data-testid="session-cwd-value"]')?.getAttribute('title') || '',
      fileTitle: document.querySelector('[data-testid="session-file-value"]')?.getAttribute('title') || '',
    }));
    const state = (await page.evaluate(() => {
      const states = window.__rpiResponses.filter(
        (r) => r.command === 'get_state' && r.success === true && r.data?.sessionId,
      );
      return states.length > 0 ? states[states.length - 1].data : null;
    })) || null;
    if (state === null) fail('captured no successful get_state for the displayed panel');
    if (!state.sessionName) fail(`get_state sessionName must be nonempty after the rename: ${JSON.stringify(state.sessionName)}`);
    if (state.sessionName !== activeWire.name) {
      fail(`authoritative names disagree: get_state sessionName ${JSON.stringify(state.sessionName)} vs active session_list row name ${JSON.stringify(activeWire.name)}`);
    }
    if (panel.id !== String(state.sessionId || '') || panel.id !== activeId) {
      fail(`rendered panel session id ${JSON.stringify(panel.id)} must exactly equal get_state sessionId and the active catalog row ${JSON.stringify(state.sessionId)}`);
    }
    if (panel.name !== state.sessionName) {
      fail(`rendered panel session name ${JSON.stringify(panel.name)} must exactly equal the agreed authoritative name ${JSON.stringify(state.sessionName)}`);
    }
    if (workspace !== String(state.cwd || '') || String(activeWire.cwd || '') !== panel.cwd || panel.cwd !== workspace) {
      fail(`env workspace ${JSON.stringify(workspace)} must equal get_state cwd ${JSON.stringify(state.cwd)}, the active session_list row cwd ${JSON.stringify(activeWire.cwd)} and the rendered panel cwd ${JSON.stringify(panel.cwd)}`);
    }
    if (!state.sessionFile || panel.fileTitle !== String(state.sessionFile)) {
      fail(`panel session file ${JSON.stringify(panel.fileTitle)} must be present and exactly match get_state sessionFile ${JSON.stringify(state.sessionFile)}`);
    }
    if (panel.fileTitle !== String(activeWire.path || '')) {
      fail(`panel session file ${JSON.stringify(panel.fileTitle)} does not match the active session_list row path ${JSON.stringify(activeWire.path)}`);
    }
    const workspaceResolved = pathResolve(workspace);
    const completionResolved = pathResolve(completionDir);
    const containment = pathRelative(workspaceResolved, completionResolved);
    if (isAbsolute(containment) || containment === '..' || containment.startsWith('../')
      || workspaceResolved !== String(state.cwd || '')) {
      fail(`completion target ${JSON.stringify(completionDir)} must normalize inside the authoritative displayed cwd ${JSON.stringify(workspaceResolved)} (relative=${JSON.stringify(containment)})`);
    }
    if (!completionName.startsWith('exi')) {
      fail(`completion prefix probe must be derived from the target name (got ${JSON.stringify(completionName)})`);
    }
    const successBase = await page.evaluate(() => ({
      frames: window.__rpiFrames.length,
      responses: window.__rpiResponses.length,
      surface: window.__rpiSurfaceEvents.length,
    }));
    await page.fill('#session-new-cwd-input', `${workspace}/exi`);
    await waitFor(
      page,
      () => document.querySelector('.path-autocomplete__menu') !== null,
      'valid existing workdir prefix never opened the suggestion menu'
    );
    const successGate = await wireGate(page, {
      label: 'valid workdir probe never produced exactly one exact-shape request with its id-correlated exact suggestion',
      info: {
        type: 'path_complete',
        frameBase: successBase.frames,
        responseBase: successBase.responses,
        keys: 'id,kind,limit,path,sessionId,type',
        owner: String(catalog.getState.data.sessionId),
        envelopeKeys: ['type', 'command', 'id', 'sessionId', 'success', 'data'],
        expect: {
          path: `${workspace}/exi`,
          kind: 'directory',
          limit: 100,
          sessionId: String(catalog.getState.data.sessionId),
        },
        reply: {
          success: true,
          base: workspace,
          prefix: 'exi',
          entry: { name: completionName, path: completionDir, kind: 'directory' },
        },
      },
    });
    if (!successGate.ok) fail(`success phase: ${successGate.detail}`);
    const menu = await page.evaluate(() => ({
      expanded: document.getElementById('session-new-cwd-input')?.getAttribute('aria-expanded'),
      names: [...document.querySelectorAll('.path-autocomplete__option .path-autocomplete__name')].map((n) => n.textContent || ''),
      kinds: [...document.querySelectorAll('.path-autocomplete__option .path-autocomplete__kind')].map((n) => n.textContent || ''),
    }));
    if (!menu.names.includes(completionName)) {
      fail(`completion menu did not surface the exact existing directory ${completionName}: ${JSON.stringify(menu)}`);
    }
    if (!menu.kinds.every((kind) => kind === 'dir')) {
      fail(`completion menu suggested a non-directory row: ${JSON.stringify(menu)}`);
    }
    if (menu.expanded !== 'true') fail('suggestion menu did not mark the combobox aria-expanded');
    const optionMatches = await page.evaluate((expected) => {
      const options = [...document.querySelectorAll('.path-autocomplete__option')];
      const exact = options.filter(
        (option) => (option.querySelector('.path-autocomplete__name')?.textContent || '') === expected,
      );
      return { total: options.length, exact: exact.length };
    }, completionName);
    if (optionMatches.exact !== 1 || optionMatches.total !== 1) {
      fail(`menu must render EXACTLY one suggestion matching ${JSON.stringify(completionName)} (total=${optionMatches.total}, exact=${optionMatches.exact})`);
    }
    await awaitStablePair(page, {
      type: 'path_complete',
      frameBase: successBase.frames,
      responseBase: successBase.responses,
      surfaceBase: successBase.surface,
      id: successGate.id,
    }, 'valid workdir probe');
    const drillBase = await page.evaluate(() => ({
      frames: window.__rpiFrames.length,
      responses: window.__rpiResponses.length,
      surface: window.__rpiSurfaceEvents.length,
    }));
    await page.locator('.path-autocomplete__option').first().click();
    const selected = await page.evaluate(() => ({
      value: document.getElementById('session-new-cwd-input')?.value || '',
      expanded: document.getElementById('session-new-cwd-input')?.getAttribute('aria-expanded'),
      menu: document.querySelector('.path-autocomplete__menu') !== null,
    }));
    if (selected.value !== `${completionDir}/`) {
      fail(`selecting the exact suggestion must replace the input with ${JSON.stringify(`${completionDir}/`)} (got ${JSON.stringify(selected.value)})`);
    }
    const drillGate = await wireGate(page, {
      label: 'drill re-probe never produced exactly one exact-shape request with exactly one correlated reply',
      info: {
        type: 'path_complete',
        frameBase: drillBase.frames,
        responseBase: drillBase.responses,
        keys: 'id,kind,limit,path,sessionId,type',
        owner: String(catalog.getState.data.sessionId),
        envelopeKeys: ['type', 'command', 'id', 'sessionId', 'success', 'data'],
        expect: {
          path: `${completionDir}/`,
          kind: 'directory',
          limit: 100,
          sessionId: String(catalog.getState.data.sessionId),
        },
        reply: { success: true, base: completionDir, prefix: '', entriesEmpty: true },
      },
    });
    if (!drillGate.ok) fail(`drill phase: ${drillGate.detail}`);
    const drilled = await page.evaluate(() => ({
      expanded: document.getElementById('session-new-cwd-input')?.getAttribute('aria-expanded'),
      menu: document.querySelector('.path-autocomplete__menu') !== null,
    }));
    if (drilled.menu || drilled.expanded !== 'false') {
      fail(`empty drill reply left suggestions open: ${JSON.stringify(drilled)}`);
    }
    await awaitStablePair(page, {
      type: 'path_complete',
      frameBase: drillBase.frames,
      responseBase: drillBase.responses,
      surfaceBase: drillBase.surface,
      id: drillGate.id,
    }, 'drill re-probe');
    await assertCleanSurface(page, 'valid workdir completion', drillBase.surface);
    await page.screenshot({ path: `${evidence}/02-completion-menu.png`, fullPage: true });

    const absentDir = pathResolve(workspace, 'absent-e2e-dir');
    const absentBase = await page.evaluate(() => ({
      frames: window.__rpiFrames.length,
      responses: window.__rpiResponses.length,
      surface: window.__rpiSurfaceEvents.length,
    }));
    await page.fill('#session-new-cwd-input', absentDir);
    const absentGate = await wireGate(page, {
      label: 'absent-workdir probe never produced exactly one exact-shape request with exactly one correlated quiet reply',
      info: {
        type: 'path_complete',
        frameBase: absentBase.frames,
        responseBase: absentBase.responses,
        keys: 'id,kind,limit,path,sessionId,type',
        owner: String(catalog.getState.data.sessionId),
        envelopeKeys: ['type', 'command', 'id', 'sessionId', 'success', 'data'],
        expect: {
          path: absentDir,
          kind: 'directory',
          limit: 100,
          sessionId: String(catalog.getState.data.sessionId),
        },
        reply: { success: true, base: workspace, prefix: 'absent-e2e-dir', entriesEmpty: true },
      },
    });
    if (!absentGate.ok) fail(`absent phase: ${absentGate.detail}`);
    await awaitStablePair(page, {
      type: 'path_complete',
      frameBase: absentBase.frames,
      responseBase: absentBase.responses,
      surfaceBase: absentBase.surface,
      id: absentGate.id,
    }, 'absent-workdir quiet probe');
    const closed = await page.evaluate(() => ({
      expanded: document.getElementById('session-new-cwd-input')?.getAttribute('aria-expanded'),
      menu: document.querySelector('.path-autocomplete__menu') !== null,
      value: document.getElementById('session-new-cwd-input')?.value || '',
      bodyText: document.body.textContent || '',
    }));
    if (closed.menu || closed.expanded === 'true') {
      fail('absent-workdir probe opened a suggestion menu; an empty completion must fail closed');
    }
    if (closed.value !== absentDir) {
      fail(`absent-workdir probe must leave manual input untouched (got ${JSON.stringify(closed.value)})`);
    }
    await assertCleanSurface(page, 'absent-workdir completion', absentBase.surface);
    const invalidDirectory = pathResolve(workspace, 'not-a-directory.txt');
    writeFileSync(invalidDirectory, 'directory rejection fixture');
    await page.fill('#session-new-cwd-input', invalidDirectory);
    const manualBase = await page.evaluate(() => ({
      frames: window.__rpiFrames.length,
      responses: window.__rpiResponses.length,
      surface: window.__rpiSurfaceEvents.length,
    }));
    await page.press('#session-new-cwd-input', 'Enter');
    const manual = await waitForValue(page, (info) => {
      const frames = window.__rpiFrames
        .slice(info.frameBase)
        .filter((f) => f.type === 'new_session');
      if (frames.length === 0) return null;
      if (frames.length !== 1) {
        return { ok: false, detail: `expected exactly one new new_session frame, got ${frames.length}: ${JSON.stringify(frames)}` };
      }
      const request = frames[0];
      const requestKeys = Object.keys(request).sort().join(',');
      if (requestKeys !== 'cwd,id,sessionId,type') {
        return { ok: false, detail: `new_session keys must be exactly [cwd,id,sessionId,type], got [${requestKeys}]: ${JSON.stringify(request)}` };
      }
      if (typeof request.id !== 'string' || request.id === '') {
        return { ok: false, detail: `new_session id must be a nonempty string: ${JSON.stringify(request)}` };
      }
      if (request.cwd !== info.cwd) {
        return { ok: false, detail: `new_session cwd must be ${JSON.stringify(info.cwd)}, got ${JSON.stringify(request.cwd)}` };
      }
      if (request.sessionId !== info.sessionId) {
        return { ok: false, detail: `new_session sessionId must be the active owner ${JSON.stringify(info.sessionId)}, got ${JSON.stringify(request.sessionId)}` };
      }
      const replies = window.__rpiResponses
        .slice(info.responseBase)
        .filter((r) => r.id === request.id);
      if (replies.length === 0) return null;
      if (replies.length !== 1) {
        return { ok: false, detail: `expected exactly one new_session response for id ${request.id}, got ${replies.length}: ${JSON.stringify(replies)}` };
      }
      const failure = replies[0];
      const failureKeys = Object.keys(failure).sort();
      if (JSON.stringify(failureKeys) !== JSON.stringify(['command', 'error', 'id', 'sessionId', 'success', 'type'])) {
        return { ok: false, detail: `new_session failure envelope keys must be exactly ['command','error','id','sessionId','success','type'], got ${JSON.stringify(failureKeys)}: ${JSON.stringify(failure)}` };
      }
      if (failure.sessionId !== info.owner) {
        return { ok: false, detail: `new_session failure envelope sessionId must be the owner ${JSON.stringify(info.owner)}, got ${JSON.stringify(failure.sessionId)}` };
      }
      if (failure.success !== false || failure.type !== 'response' || failure.command !== 'new_session') {
        return { ok: false, detail: `new_session failure must be success=false type=response command=new_session, got ${JSON.stringify(failure)}` };
      }
      if (failure.error !== 'new session path is not a directory') {
        return { ok: false, detail: `new_session did not reject the file path: ${JSON.stringify(failure.error)}` };
      }
      const owned = `command new_session failed: ${failure.error || ''}`;
      const toasts = [...document.querySelectorAll('#toasts .toast')].map((t) => t.textContent || '');
      if (toasts.length !== 1 || toasts[0] !== owned) return null;
      return { ok: true, id: request.id, error: failure.error || '', toast: toasts[0] };
    }, 'manual Enter never produced exactly one exact-shape new_session with exactly one correlated failure and its committed owned toast', 15000, {
      frameBase: manualBase.frames,
      responseBase: manualBase.responses,
      cwd: invalidDirectory,
      sessionId: String(catalog.getState.data.sessionId),
      owner: String(catalog.getState.data.sessionId),
    });
    if (!manual.ok) fail(`manual phase: ${manual.detail}`);
    if (manual.error !== 'new session path is not a directory') {
      fail(`new_session did not reject the file path: ${JSON.stringify(manual.error)}`);
    }
    if (manual.toast !== 'command new_session failed: new session path is not a directory') {
      fail(`file-path rejection toast did not match its owned error: ${JSON.stringify(manual.toast)}`);
    }
    await awaitStablePair(page, {
      type: 'new_session',
      frameBase: manualBase.frames,
      responseBase: manualBase.responses,
      surfaceBase: manualBase.surface,
      id: manual.id,
      ownedToast: manual.toast,
    }, 'manual phase');
    await page.screenshot({ path: `${evidence}/03-outside-fail-closed.png`, fullPage: true });
    await assertNoBrowserErrors(page, consoleErrors, pageErrors, 'lane end');

    console.log('web-session-path-complete: PASSED (tokened catalog identity + valid-workdir suggestions + missing-workdir fail-closed + manual new_session failure)');
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-session-path-complete: FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});