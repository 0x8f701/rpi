// Web reply-actions E2E lane (playwright half of E2E.d/web/reply_actions.sh).
//
// FROZEN UI INTERFACE (Main contract):
//   - the settled MAIN-transcript assistant row `.msg--assistant[data-entry-id]`
//     carries EXACTLY THREE buttons in ONE actions group, top-left, stacked
//     VERTICALLY: favorite topmost (`button.assistant-star-toggle`, class
//     preserved), then copy-source (`button[data-reply-action="copy-markdown"]`),
//     then save.md (`button[data-reply-action="save-markdown"]`) — one shared
//     x column, y strictly increasing star→copy→save.
//   - Three buttons ALWAYS visible (no hover dependency): desktop resting
//     opacity > 0 with a real center hit AFTER the control is truly scrolled
//     into the viewport (an offscreen control is never judged as a no-hover
//     defect — bg2397); phone (touch context) opacity '1' and >=44x44.
//   - copy-markdown writes the RAW markdown: text blocks concatenated, NO
//     trim, NO metadata. Byte-exact against the mock constant. (Redaction is
//     acceptable for a public reply source — the mock constants carry no
//     secret-shaped text, so byte-exactness holds either way.)
//   - save-markdown downloads UTF-8 bytes === the raw markdown; filename =
//     summary digest (<=30 unicode points, no language requirement,
//     path-illegal + control chars stripped, no leading/trailing space/dot)
//     + the STABLE reply timestamp in LOCAL YYYYMMDD-HHmmss. Two saves on one
//     reply and a page reload produce the SAME filename; the phone surface
//     saving the same reply produces the same filename too.
//   - clipboard FAILURE surfaces the app's CENTRAL error toast (`#toasts`
//     child with `toast--error` / role=alert) — this lane asserts the central
//     toast contract and invents NO flash surface of its own.
//   - blob download: exactly one createObjectURL + one revokeObjectURL per
//     save, revoked URL identical to the created URL.
//
// READY GATE (bg2400): ready = #reconnect-overlay ABSENT (the app removes it
// only when connState is 'on'/'connected' AND the boot/rebind finished —
// reconnect.ts reconnectOverlayVisible) AND the model catalog populated
// (get_available_models only fires on connState==='on'). One exact-state
// waitFor, no sleeps; a persisting overlay FAILS and is never removed here.
//
// DURABLE PUBLICATION GATE (bg2394; sessions-lane waitForSettledStar
// pattern): a temporary LIVE row can reach data-status="final" BEFORE the
// canonical hydrate lands — the authoritative entry id and the star chrome
// arrive later. Every entry-id/button read waits for the REAL publication.
//
// RICH FIXTURE = the EXISTING mock constants RICH_TEXT / RICH_TEXT_EXTRA
// (E2E.d/lib/user_mock_server.py, routed by EXACT prompt text): RICH_TEXT ←
// 'render rich content' (:1083); RICH_TEXT_EXTRA ← 'render markdown extra
// branches' (:1095). TRAP (bg2398): 'render rich content extra' SUBSTRING-
// matches the :1083 branch and re-delivers RICH_TEXT — never use it. The lane
// shell extracts both via importlib into expected-*.md files (single source
// of truth) and this half reads those BYTES.
//
// /chat SURFACE leg is a HARD ASSERTION: the FULL three-button interface
// (star via the SHARED reply-star store + copy + save) must exist on a FINAL
// hydrated /chat row, stacked vertically, with a real star click/un-toggle
// through the shared store. Only a temporary LIVE row without an entry id may
// omit the star; absence FAILS the lane — never a PENDING pass. The row is
// taken only after authoritative hydration. The bootstrap's single expected
// auth console error is the ONLY tolerated non-fatal category (exact-string
// match during the bootstrap window only, counted and phase-attributed).
//
// EMPTY-REPLY handling is a PURE MODULE-BOUNDARY contract (the filename
// builder's empty-input fallback, owned by the UI owner's unit test) — not an
// E2E leg and never a PENDING claim.
//
// HYDRATION-DUPLICATION CONTRACT (bg2398): DOM assistant entry ids must be
// UNIQUE across the transcript; wire authoritative ids stay INTERNAL (only
// counts/bools surface). PROVENANCE DISCRIMINATION: the collector takes the
// wrapper `id` of get_messages response rows ({id, message:{role}}) as the
// ONLY authoritative wire set; live event `itemId`s go to a SEPARATE
// internal collection and NEVER count as authoritative — a synthetic DOM id
// seen only as a live itemId must stay REJECTED (negative control at RA16).
//
// STRUCTURED ERROR EVIDENCE (bg2406): every console error and pageerror is
// recorded as a SAFE entry — kind, capture phase, url PATH (no query), len,
// sha, and an EXACT-documented-category bool — never raw text. The zero-error
// contract counts non-tolerated entries; tolerated entries remain visible and
// phase-attributed (never widened, never silently dropped).
//
// Page-context predicates never close over module constants: every expected
// value crosses as the waitFor/evaluate ARG.
//
// Environment:
//   RPI_URL                 http://127.0.0.1:<port>/web
//   RPI_TOKEN               token file content (rpi-auth.<token> subprotocol)
//   RPI_CHROME              executable path of the system Chrome (optional)
//   RPI_EVIDENCE            evidence dir (screenshots + assertion ledger)
//   RPI_EXPECTED_RICH       file holding the exact RICH_TEXT bytes
//   RPI_EXPECTED_RICH_EXTRA file holding the exact RICH_TEXT_EXTRA bytes
//   RPI_CHAT_URL            http://127.0.0.1:<port>/chat
//   RPI_RUNNER_ORIGIN       runner origin for the /chat invite dialog
//   RPI_RUNNER_TOKEN        runner token for the /chat invite dialog
//   RPI_RUNNER_CWD          runner workspace cwd for the /chat project form
//
// Exit: 0 = PASSED; 1 = setup failure (node/chromium/env); >=2 = assertion or
// journey failure. No reply content and no raw filename ever reach the
// evidence (filenames are reported as len/sha fingerprints only).
import { chromium } from 'playwright';
import fs from 'node:fs';
import { createHash } from 'node:crypto';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';
const richPath = process.env.RPI_EXPECTED_RICH || '';
const richExtraPath = process.env.RPI_EXPECTED_RICH_EXTRA || '';
const chatUrl = process.env.RPI_CHAT_URL || '';
const runnerOrigin = process.env.RPI_RUNNER_ORIGIN || '';
const runnerToken = process.env.RPI_RUNNER_TOKEN || '';
const runnerCwd = process.env.RPI_RUNNER_CWD || '';

const DOCUMENTED_IDS = [
  'RA0-boot-connected',
  'RA1-rich-reply-settles',
  'RA2-three-buttons-vertical-visible',
  'RA3-copy-clipboard-byte-exact',
  'RA4-save-filename-bytes-blob',
  'RA5-two-saves-same-filename',
  'RA6-reload-stable-filename-and-raw',
  'RA7-extra-reply-settles',
  'RA8-extra-byte-exact-and-stable',
  'RA9-copy-fail-central-toast',
  'RA10-phone-restores-reply',
  'RA11-phone-three-buttons-touch',
  'RA12-phone-copy-byte-exact',
  'RA13-phone-save-same-filename',
  'RA14-phone-extra-row-touch',
  'RA15-chat-surface-contract',
  'RA16-zero-browser-errors',
];
// Fixture resource + exercise counters (bg2411): the served PNG and proof
// that the route was actually hit by a real lazy-image fetch.
const PNG_1X1 = Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=', 'base64');
let picRouteHits = 0;
let picRouteBytes = 0;
const executed = new Set();
function record(id) {
  if (executed.has(id)) fail(`assertion ${id} recorded twice`);
  executed.add(id);
}

class LaneFail extends Error {}
// PHASE CONTRACT: every failure names its exact pipeline phase.
let CURRENT_PHASE = 'boot';
function fail(message) {
  throw new LaneFail(`[phase=${CURRENT_PHASE}] ${message}`);
}

function fp(payload) {
  const text = typeof payload === 'string' ? payload : String(payload ?? '');
  return `len=${Buffer.byteLength(text, 'utf8')} sha256=${createHash('sha256').update(text, 'utf8').digest('hex')}`;
}

async function waitFor(page, fn, label, timeoutMs = 25000, arg, diagFn) {
  try {
    await page.waitForFunction(fn, arg, { timeout: timeoutMs });
  } catch (waitError) {
    // GATE-TIMEOUT DIAGNOSTICS CONTRACT (bg2403): a timed-out gate captures
    // its diagnostics BEFORE failing — never after, never conditioned on the
    // gate having passed. diagFn is one of the safe-field diagnosers; its own
    // failure is fingerprinted, never fatal-masked.
    let diag = '';
    if (diagFn) {
      try { diag = ` diag=${JSON.stringify(await diagFn(page))}`; } catch (e) { diag = ` diagFailed=${fp(e?.message || e)}`; }
    }
    fail(`${label} (timeout ${timeoutMs}ms)${diag} waitError=${fp(waitError?.message || waitError)}`);
  }
}

async function diagnose(page, label) {
  const shot = `${evidence}/diag-${label}.png`;
  await page.screenshot({ path: shot, fullPage: true }).catch(() => {});
  fs.chmodSync(shot, 0o600);
  const safe = await page.evaluate(() => {
    const rows = [...document.querySelectorAll('main#transcript .msg--assistant')];
    const ids = rows.map((r) => r.getAttribute('data-entry-id')).filter((v) => typeof v === 'string' && v !== '');
    const m = window.__wsMeta || { out: {}, totalOut: 0, wireAssistantIds: [] };
    return {
      assistantRowCount: rows.length,
      assistantTextLengths: rows.map((r) => (r.querySelector('.assistant-text')?.textContent || '').length),
      rowStatuses: rows.map((r) => r.getAttribute('data-status') || ''),
      entryIdPresent: rows.map((r) => ((r.getAttribute('data-entry-id') || r.querySelector('button.assistant-star-toggle')?.getAttribute('data-entry-id') || '') !== '')),
      replyActionCounts: {
        copy: rows.filter((r) => r.querySelector('button[data-reply-action="copy-markdown"]') !== null).length,
        save: rows.filter((r) => r.querySelector('button[data-reply-action="save-markdown"]') !== null).length,
      },
      savingReplyPresent: (document.querySelector('main#transcript [data-runtime-key] .transcript-content')?.textContent || '').includes('Saving reply…'),
      // wsMeta is SANITIZED: wire ids stay INTERNAL; counts/bools only.
      wsMeta: {
        promptFramesSent: m.out.prompt || 0,
        frameTypes: m.out,
        totalOut: m.totalOut,
        wireAssistantIdsCount: m.wireAssistantIds.length,
        wireAssistantIdsUnique: new Set(m.wireAssistantIds).size === m.wireAssistantIds.length,
        domEntryIdsPresent: ids.length,
        domEntryIdsUnique: new Set(ids).size === ids.length,
        domEntryIdsAllInWire: ids.length > 0 && m.wireAssistantIds.length > 0 ? ids.every((id) => m.wireAssistantIds.includes(id)) : null,
      },
      userRowCount: document.querySelectorAll('main#transcript .msg--user').length,
      rowShapes: rows.map((r) => ({
        looksLikeA: r.querySelector('table.md-table') !== null,
        looksLikeB: r.querySelector('.assistant-text blockquote') !== null,
      })),
      rowsRenderedTextIdentical: (() => { const texts = rows.map((r) => r.querySelector('.assistant-text')?.textContent || ''); return texts.length > 1 && texts.every((t) => t === texts[0]); })(),
      overlay: (() => {
        const o = document.getElementById('reconnect-overlay');
        if (!o) return { present: false };
        const text = o.querySelector('.reconnect-overlay__text')?.textContent || o.textContent || '';
        return {
          present: true,
          category: text.startsWith('Connecting') ? 'connecting' : text.startsWith('Reconnect') ? 'reconnecting' : 'other',
          textLen: text.length,
          textSha: window.__djb2 ? window.__djb2(text) : '',
          ariaBusy: o.getAttribute('aria-busy') || '',
        };
      })(),
      activeSidebarSessionIdLen: (document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '').length,
      wsFramesSent: m.totalOut || 0,
      // Attempt/toast timelines — fixed fields only (t/category/len/hash/result).
      clipLog: window.__clipLog || [],
      toastLog: window.__toastLog || [],
      toastCount: document.querySelectorAll('#toasts .toast').length,
      modelOptionCount: [...document.querySelectorAll('#thinking-select option')].filter((o) => (o.value || '').length > 0 || (o.textContent || '').length > 0).length,
      // IMG FACTS (bg2411, safe fields only): per-image src PATHNAME (no
      // query), loading attribute, decode state, viewport presence.
      assistantImageFacts: [...document.querySelectorAll('main#transcript .msg--assistant img')].slice(0, 8).map((img) => {
        let pathname = '';
        try { pathname = new URL(img.currentSrc || img.getAttribute('src') || '', location.href).pathname; } catch { pathname = '(unparseable)'; }
        const r = img.getBoundingClientRect();
        return { pathname, loading: img.getAttribute('loading') || '', complete: img.complete, naturalWidth: img.naturalWidth, inViewport: r.top < window.innerHeight && r.bottom > 0 };
      }),
    };
  });
  return {
    ...safe,
    // IMG FACTS + route-exercise counters (bg2411, safe fields only):
    // per-image src PATHNAME (no query), loading attribute, decode state,
    // viewport presence — plus whether the fixture route was actually hit
    // (hits) and how many bytes it served.
    imageFacts: safe.assistantImageFacts,
    picRouteHits,
    picRouteBytes,
    browserErrors: errorEvidence([page]),
    screenshot: `diag-${label}.png`,
  };
}

/** Structured browser-error collector (bg2406): every console error and
 *  pageerror is recorded as a SAFE entry — kind, capture phase, url PATH
 *  (no query), len, sha, and an EXACT-documented-category bool — never the
 *  raw text. The optional tolerated channel matches ONLY the one documented
 *  chat bootstrap auth string (exact equality) while toleratedWhile() holds;
 *  tolerated entries are still recorded with their phase (never widened,
 *  never silently dropped, never contradicting the zero-error contract). */
// KNOWN-ERROR SIGNATURES (bg2408): fixed, well-known browser error strings
// hashed at module load (privacy constants — hash matching, never raw
// output). Classification ONLY: a known category makes a failure
// self-describing; it adds NO tolerance and suppresses NOTHING.
const sha16 = (text) => createHash('sha256').update(text, 'utf8').digest('hex').slice(0, 16);
const KNOWN_ERROR_SIGNATURES = [
  'Failed to load resource: the server responded with a status of 401 (Unauthorized)',
  'Failed to load resource: the server responded with a status of 404 (Not Found)',
  'Failed to load resource: the server responded with a status of 403 (Forbidden)',
  'Uncaught (in promise) NotAllowedError: Write permission denied.',
  'Failed to execute \'writeText\' on \'Clipboard\': Document is not focused.',
].map((text) => ({ category: text.includes('401') ? 'resource-401'
  : text.includes('404') ? 'resource-404'
  : text.includes('403') ? 'resource-403'
  : text.includes('Write permission denied') ? 'clipboard-write-denied'
  : 'clipboard-not-focused', len: text.length, sha: sha16(text) }));
const knownCategoryFor = (text) => {
  const hit = KNOWN_ERROR_SIGNATURES.find((s) => s.len === text.length && s.sha === sha16(text));
  return hit ? hit.category : 'unknown';
};

function attachErrorCollector(page, { toleratedExact, toleratedWhile } = {}) {
  const entries = [];
  let toleratedCount = 0;
  const push = (kind, text, isTolerated, location) => {
    entries.push({
      kind,
      tolerated: isTolerated === true,
      phase: CURRENT_PHASE,
      urlPath: new URL(page.url()).pathname,
      len: text.length,
      sha: sha16(text),
      knownCategory: knownCategoryFor(text),
      knownMatch: knownCategoryFor(text) !== 'unknown',
      matchesDocumentedAuth: toleratedExact !== undefined && text === toleratedExact,
      locPath: location ? new URL(location.url, page.url()).pathname : '',
      locLine: location && Number.isInteger(location.lineNumber) ? location.lineNumber : null,
    });
    if (isTolerated === true) toleratedCount += 1;
  };
  page.on('pageerror', (e) => push('pageerror', e?.message || String(e), false, null));
  page.on('console', (m) => {
    if (m.type() !== 'error') return;
    const text = m.text();
    const tolerated = toleratedExact !== undefined && text === toleratedExact && toleratedWhile();
    push('console', text, tolerated, m.location ? m.location() : null);
  });
  // Failed network responses (status >= 400): safe metadata only — status,
  // resource type, and the URL PATH (no query, no body). Classification
  // companion to the console entries (e.g. favicon 404 vs real API 401).
  page.__netFailures = [];
  page.on('response', (response) => {
    if (response.status() < 400) return;
    page.__netFailures.push({
      phase: CURRENT_PHASE,
      status: response.status(),
      resourceType: response.request().resourceType(),
      urlPath: new URL(response.url()).pathname,
    });
  });
  page.__errorEntries = entries;
  page.__toleratedCount = () => toleratedCount;
}

async function newPage(browser, { width, height, mobile }) {
  const context = await browser.newContext({
    viewport: { width, height },
    acceptDownloads: true,
    permissions: ['clipboard-read', 'clipboard-write'],
    ...(mobile ? { isMobile: true, hasTouch: true } : {}),
  });
  if (token) {
    await context.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
  }
  // WS metadata hook (diagnosis only — NEVER payloads out): outgoing frame
  // TYPE counts; incoming frames walked ONLY for assistant-role message ids
  // kept INTERNAL and surfaced as counts/bools (bg2398 hydration-dup
  // discrimination without leaking ids). Subclass wrap preserves instanceof
  // and statics; the onmessage setter wraps the app handler.
  await context.addInitScript(() => {
    window.__wsMeta = { out: {}, totalOut: 0, wireAssistantIds: [], liveAssistantItemIds: [] };
    const collectAssistantIds = (node) => {
      if (!node || typeof node !== 'object') return;
      if (Array.isArray(node)) { for (const item of node) collectAssistantIds(item); return; }
      // WIRE SHAPE (server serialization, session_runtime_manager.rs): a
      // durable row is a WRAPPER { id, parentId, timestamp,
      // message: { role, ... } } — the get_messages response row whose
      // wrapper `id` IS the authoritative recorder entry id the DOM
      // data-entry-id is hydrated from. A live event frame carries the
      // same message under a synthetic `itemId` stamp; those are the
      // LIVE stream and go into liveAssistantItemIds — NEVER into
      // wireAssistantIds, never mixed with the authoritative set (no
      // itemId fallback: seeing any wire itemId must never make a
      // synthetic DOM id count as authoritative). role and the id NEVER
      // sit on the same node: `message.role` decides, the id comes from
      // the wrapper. node.role+node.id matching matches no real wire row.
      const message = node.message !== null && typeof node.message === 'object' && !Array.isArray(node.message) ? node.message : null;
      if (message !== null && message.role === 'assistant') {
        if (typeof node.id === 'string' && node.id !== '') {
          window.__wsMeta.wireAssistantIds.push(node.id);
        } else if (typeof node.itemId === 'string' && node.itemId !== '') {
          window.__wsMeta.liveAssistantItemIds.push(node.itemId);
        }
      }
      for (const key of Object.keys(node)) collectAssistantIds(node[key]);
    };
    const origSend = WebSocket.prototype.send;
    WebSocket.prototype.send = function (data) {
      try {
        const t = typeof data === 'string' ? (JSON.parse(data) || {}).type : '';
        if (typeof t === 'string' && t !== '') {
          window.__wsMeta.out[t] = (window.__wsMeta.out[t] || 0) + 1;
          window.__wsMeta.totalOut += 1;
        }
      } catch { /* non-JSON or binary frame: type unknown, not recorded */ }
      return origSend.call(this, data);
    };
    class WrappedWebSocket extends WebSocket {
      set onmessage(fn) {
        super.onmessage = fn ? (event) => {
          try { collectAssistantIds(JSON.parse(event.data)); } catch { /* non-JSON frame */ }
          return fn.call(this, event);
        } : null;
      }
    }
    window.WebSocket = WrappedWebSocket;
  });
  // Attempt/toast timelines (fixed contract — t/category/len/hash/result
  // ONLY, never text): every REAL clipboard write is recorded by wrapping the
  // native writeText once at context init; RA9's forced-fail records its own
  // entry; every toast node is recorded (error class/role + len + hash) by a
  // #toasts MutationObserver. Per-attempt toast attribution (bg2399).
  await context.addInitScript(() => {
    const djb2 = (s) => { let h = 5381; for (let i = 0; i < s.length; i += 1) { h = ((h * 33) ^ s.charCodeAt(i)) >>> 0; } return h.toString(16).padStart(8, '0'); };
    window.__djb2 = djb2;
    window.__clipLog = [];
    window.__toastLog = [];
    const recordToast = (node) => {
      if (node.__toastLogged) return;
      node.__toastLogged = true;
      const text = node.textContent || '';
      window.__toastLog.push({ t: Date.now(), error: node.classList.contains('toast--error') || node.getAttribute('role') === 'alert', len: text.length, sha: djb2(text) });
    };
    const startObserver = () => {
      const host = document.getElementById('toasts');
      if (!host) { window.setTimeout(startObserver, 50); return; }
      for (const t of host.querySelectorAll('.toast')) recordToast(t);
      new MutationObserver((muts) => {
        for (const m of muts) {
          for (const n of m.addedNodes) {
            if (n.nodeType !== 1) continue;
            if (n.matches?.('.toast')) recordToast(n);
            else n.querySelectorAll?.('.toast').forEach(recordToast);
          }
        }
      }).observe(host, { childList: true, subtree: true });
    };
    startObserver();
    const real = navigator.clipboard;
    const origWrite = real.writeText.bind(real);
    const wrappedClipboard = {
      writeText: (text) => {
        const entry = { kind: 'native', t: Date.now(), len: typeof text === 'string' ? text.length : -1, sha: typeof text === 'string' ? djb2(text) : '', result: 'pending' };
        window.__clipLog.push(entry);
        const p = origWrite(text);
        p.then(() => { entry.result = 'resolved'; }, () => { entry.result = 'rejected'; });
        return p;
      },
      readText: real.readText ? real.readText.bind(real) : undefined,
    };
    window.__wrappedClipboard = wrappedClipboard;
    Object.defineProperty(navigator, 'clipboard', { configurable: true, value: wrappedClipboard });
  });
  // Blob log: wraps BEFORE app code runs (init script) — one entry per
  // createObjectURL and per revokeObjectURL (URLs + blob type/size only).
  await context.addInitScript(() => {
    window.__blobLog = [];
    const origCreate = URL.createObjectURL.bind(URL);
    const origRevoke = URL.revokeObjectURL.bind(URL);
    URL.createObjectURL = (blob) => {
      const u = origCreate(blob);
      window.__blobLog.push({ kind: 'create', url: u, type: blob && blob.type, size: blob && blob.size });
      return u;
    };
    URL.revokeObjectURL = (u) => {
      window.__blobLog.push({ kind: 'revoke', url: u });
      return origRevoke(u);
    };
  });
  const page = await context.newPage();
  attachErrorCollector(page);
  // FIXTURE RESOURCE (bg2410): RICH_TEXT references the relative image
//  ./images/pic.png, which this lane's fixture does not serve — the 404 was
//  a missing FIXTURE resource, not a product error. Register an EXACT-path
//  route (pathname equality enforced in the handler; anything else falls
//  through) serving a real 1x1 PNG BEFORE any goto, so the image truly
//  decodes on every surface sharing this context (desktop, phone, /chat).
//  No business mocking, no console-404 tolerance, MD bytes untouched.
//  Hit/byte counters prove the fixture route was actually EXERCISED.
  await context.route('**/images/pic.png', (route) => {
    if (new URL(route.request().url()).pathname !== '/images/pic.png') { route.fallback().catch(() => {}); return; }
    picRouteHits += 1;
    picRouteBytes = PNG_1X1.length;
    route.fulfill({ status: 200, contentType: 'image/png', body: PNG_1X1 });
  });
  await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
  await waitFor(page, () => document.title === 'rpi web', 'page title missing', 20000, undefined, diagnose);
  await waitFor(
    page,
    () => document.getElementById('prompt-input') !== null
      && document.getElementById('send-btn')?.getAttribute('aria-label') === 'Send message',
    'composer did not settle idle',
    25000,
    undefined,
    diagnose,
  );
  await waitReady(page);
  return { context, page };
}

/** REAL READY GATE (bg2400): see the header contract. */
async function waitReady(page) {
  await waitFor(
    page,
    () => [...document.querySelectorAll('#thinking-select option')]
      .some((o) => (o.value || '').length > 0 || (o.textContent || '').length > 0)
      && document.getElementById('reconnect-overlay') === null,
    'page never reached ready (model select populated AND #reconnect-overlay absent — a persisting overlay FAILS, it is never ignored or removed)',
    30000,
    undefined,
    diagnose,
  );
}

/** The real journey: fill + REAL send-button click, then SUBMISSION PROOF
 *  (new user row AND cleared composer) before any assistant settle wait. */
async function sendPrompt(page, text) {
  const before = await page.evaluate(() => document.querySelectorAll('main#transcript .msg--user').length);
  await page.fill('#prompt-input', text);
  await page.click('#send-btn');
  try {
    await page.waitForFunction(
      (prev) => document.querySelectorAll('main#transcript .msg--user').length > prev
        && (document.getElementById('prompt-input')?.value || '') === '',
      before,
      { timeout: 10000 },
    );
  } catch (waitError) {
    const diag = await diagnose(page, 'submit-timeout').catch((e) => ({ diagFailed: fp(e?.message || e) }));
    fail(`journey: submit did not dispatch (no new user row / prompt not cleared) waitError=${fp(waitError?.message || waitError)} diag=${JSON.stringify(diag)}`);
  }
}

/** Wait for a NEW settled assistant reply (prev = assistant-row count taken
 *  BEFORE the prompt was submitted); then the DURABLE PUBLICATION GATE; then
 *  the entry-id read. `marker` = 'table' | 'blockquote' render proof. */
async function waitNewSettledReply(page, prevCount, marker) {
  try {
    await page.waitForFunction(
      ({ prev, mark }) => {
        const rows = [...document.querySelectorAll('main#transcript .msg--assistant')];
        const fresh = rows.slice(prev);
        return fresh.length > 0
          && fresh.every((row) => (row.querySelector('.assistant-text')?.textContent || '').length > 0)
          && fresh.some((row) => row.querySelector(mark === 'table' ? 'table.md-table' : '.assistant-text blockquote') !== null)
          && document.getElementById('send-btn')?.getAttribute('aria-label') === 'Send message';
      },
      { prev: prevCount, mark: marker },
      { timeout: 40000 },
    );
  } catch (waitError) {
    const diag = await diagnose(page, 'settle-timeout').catch((e) => ({ diagFailed: fp(e?.message || e) }));
    fail(`real reply never settled (timeout 40000ms) waitError=${fp(waitError?.message || waitError)} diag=${JSON.stringify(diag)}`);
  }
  // DURABLE PUBLICATION GATE: star chrome stamped with the authoritative
  // entry id AND no persist-in-flight marker ('Saving reply…') anywhere.
  try {
    await page.waitForFunction(
      (prev) => {
        const root = document.querySelector('main#transcript [data-runtime-key] .transcript-content');
        if (!root || (root.textContent || '').includes('Saving reply…')) return false;
        return [...root.querySelectorAll('.msg--assistant')].slice(prev).some((row) => {
          const button = row.querySelector('button.assistant-star-toggle');
          return button?.getAttribute('aria-label') === 'Star'
            && (button.getAttribute('data-entry-id') || row.getAttribute('data-entry-id') || '') !== '';
        });
      },
      prevCount,
      { timeout: 30000 },
    );
  } catch (waitError) {
    const diag = await diagnose(page, 'publication-timeout').catch((e) => ({ diagFailed: fp(e?.message || e) }));
    fail(`canonical hydration never published the settled reply (no authoritative entry id / star chrome; timeout 30000ms) waitError=${fp(waitError?.message || waitError)} diag=${JSON.stringify(diag)}`);
  }
  const info = await page.evaluate((prev) => {
    const rows = [...document.querySelectorAll('main#transcript .msg--assistant')].slice(prev);
    const published = rows.filter((r) => {
      const button = r.querySelector('button.assistant-star-toggle');
      return button !== null && (button.getAttribute('data-entry-id') || r.getAttribute('data-entry-id') || '') !== '';
    });
    const row = published[published.length - 1] || rows[rows.length - 1];
    return {
      entryId: row?.getAttribute('data-entry-id')
        || row?.querySelector('button.assistant-star-toggle')?.getAttribute('data-entry-id')
        || '',
      textLength: (row?.querySelector('.assistant-text')?.textContent || '').length,
    };
  }, prevCount);
  if (info.entryId === '') fail('published reply STILL carries no authoritative entry id — not fabricating one; the three-button contract stays unchanged');
  return info;
}

function replyRowSelector(entryId) {
  if (!/^[A-Za-z0-9][A-Za-z0-9:_-]*$/.test(entryId)) {
    fail(`reply entry id has unexpected shape (len=${entryId.length}) — refusing to build a locator from it`);
  }
  return `main#transcript .msg--assistant[data-entry-id="${entryId}"]`;
}

function replyButtonSelector(entryId, action) {
  if (action === 'star') return `${replyRowSelector(entryId)} button.assistant-star-toggle`;
  return `${replyRowSelector(entryId)} button[data-reply-action="${action}"]`;
}

/** Entry-id-scoped census of the row's reply-action group ONLY (per-fence
 *  copy buttons inside the markdown body are separate working controls, out
 *  of scope, left functional — bg2396). The CONTROL (never the whole long
 *  row) is truly scrolled into the viewport before its hit probe (bg2397). */
function census(entryId) {
  const row = [...document.querySelectorAll('main#transcript .msg--assistant')]
    .find((r) => r.getAttribute('data-entry-id') === entryId);
  if (!row) return { missing: `row with entry id (len ${entryId.length})` };
  const actionsGroup = row.querySelector('.msg--assistant__actions');
  if (!actionsGroup) return { missing: 'reply-action group .msg--assistant__actions on the row' };
  const inViewport = (r) => r.top >= 0 && r.left >= 0
    && r.bottom <= (window.innerHeight || document.documentElement.clientHeight)
    && r.right <= (window.innerWidth || document.documentElement.clientWidth);
  const buttons = [...actionsGroup.querySelectorAll('button')].map((b) => {
    let r = b.getBoundingClientRect();
    if (!inViewport(r)) {
      b.scrollIntoView({ block: 'center' });
      r = b.getBoundingClientRect();
    }
    const action = b.getAttribute('data-reply-action')
      || (b.classList.contains('assistant-star-toggle') ? 'star' : `unknown:${b.className.slice(0, 40)}`);
    const hitEl = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
    return {
      action,
      cls: b.className,
      aria: b.getAttribute('aria-label') || '',
      w: r.width, h: r.height, x: r.left, y: r.top,
      opacity: getComputedStyle(b).opacity,
      inViewport: inViewport(r),
      hit: !!hitEl && (hitEl === b || b.contains(hitEl)),
      hitCls: hitEl ? String(hitEl.className).slice(0, 60) : '(none at center point)',
    };
  });
  const byAction = (name) => buttons.filter((b) => b.action === name).length;
  return {
    buttonTotal: buttons.length,
    star: byAction('star'),
    copyMarkdown: byAction('copy-markdown'),
    saveMarkdown: byAction('save-markdown'),
    buttons,
  };
}

function assertThreeButtons(counts, surface) {
  if (counts.missing) fail(`${surface}: ${counts.missing}`);
  if (counts.buttonTotal !== 3 || counts.star !== 1 || counts.copyMarkdown !== 1 || counts.saveMarkdown !== 1) {
    fail(`${surface}: expected exactly star+copy-markdown+save-markdown, got totals ${JSON.stringify({ buttonTotal: counts.buttonTotal, star: counts.star, copyMarkdown: counts.copyMarkdown, saveMarkdown: counts.saveMarkdown })}`);
  }
  const [star, copy, save] = counts.buttons;
  if (star.action !== 'star' || copy.action !== 'copy-markdown' || save.action !== 'save-markdown') {
    fail(`${surface}: DOM button order is not favorite/copy/save: ${JSON.stringify(counts.buttons.map((b) => b.action))}`);
  }
  if (Math.abs(star.x - copy.x) > 1 || Math.abs(copy.x - save.x) > 1) {
    fail(`${surface}: buttons are not one vertical column (x): star.x=${star.x} copy.x=${copy.x} save.x=${save.x}`);
  }
  if (!(star.y < copy.y && copy.y < save.y)) {
    fail(`${surface}: vertical stack order violated (y must increase star→copy→save): star.y=${star.y} copy.y=${copy.y} save.y=${save.y}`);
  }
  for (const b of counts.buttons) {
    if (!(b.w > 0 && b.h > 0 && parseFloat(b.opacity) > 0)) fail(`${surface}: button ${b.action} not visible without hover: ${JSON.stringify(b)}`);
    if (!b.inViewport) fail(`${surface}: button ${b.action} still outside the viewport even after its own real scrollIntoView — an offscreen control is NEVER judged as a no-hover defect`);
    if (!b.hit) fail(`${surface}: button ${b.action} not clickable without hover while truly in-viewport (hitCls=${b.hitCls}): ${JSON.stringify(b)}`);
  }
}

function assertTouch(counts, surface) {
  for (const b of counts.buttons) {
    if (!(b.w >= 44 && b.h >= 44 && b.opacity === '1')) fail(`${surface}: touch contract violated for ${b.action} (>=44px, opacity 1): ${JSON.stringify(b)}`);
    if (!b.inViewport) fail(`${surface}: button ${b.action} still outside the viewport even after its own real scrollIntoView — an offscreen control is NEVER judged as a no-hover defect`);
    if (!b.hit) fail(`${surface}: button ${b.action} not tappable without hover while truly in-viewport (hitCls=${b.hitCls}): ${JSON.stringify(b)}`);
  }
}

async function readClipboard(page) {
  return page.evaluate(async () => navigator.clipboard.readText());
}

async function clickReplyButton(page, entryId, action) {
  await page.click(replyButtonSelector(entryId, action));
}

async function tapReplyButton(page, entryId, action) {
  await page.tap(replyButtonSelector(entryId, action));
}

function assertMdFilename(name, surface) {
  if (!name.endsWith('.md')) fail(`${surface}: filename lacks .md (len ${name.length}, sha ${createHash('sha256').update(name, 'utf8').digest('hex').slice(0, 16)})`);
  if (/[\u0000-\u001f\u007f]/.test(name)) fail(`${surface}: filename carries control characters`);
  const stem = name.slice(0, -3);
  const match = /^(.+)-(\d{8}-\d{6})$/.exec(stem);
  if (!match) fail(`${surface}: filename lacks digest + YYYYMMDD-HHmmss timestamp (len ${stem.length}, sha ${createHash('sha256').update(stem, 'utf8').digest('hex').slice(0, 16)})`);
  const digest = match[1];
  const points = [...digest].length;
  if (points < 1 || points > 30) fail(`${surface}: digest ${points} unicode points outside 1..30`);
  if (/[/\\:*?"<>|]/.test(digest)) fail(`${surface}: digest carries path-illegal characters`);
  if (/^[ .]|[ .]$/.test(digest)) fail(`${surface}: digest has leading/trailing space or dot`);
  return { points };
}

function errorEvidence(pages) {
  const all = pages.flatMap((p) => p.__errorEntries || []);
  const fatal = all.filter((e) => e.tolerated !== true);
  const netFailures = pages.flatMap((p) => p.__netFailures || []);
  const serialized = JSON.stringify({ errors: all, netFailures });
  return { count: fatal.length, tolerated: all.length - fatal.length, entries: all, netFailures, sha256: createHash('sha256').update(serialized).digest('hex') };
}

/** /chat SessionView journey — the reviewed chat_mermaid bootstrap. */
async function openChatSessionView(page) {
  await page.locator('#chat-project-add-btn:visible, #chat-empty-new-project:visible').first().click();
  await waitFor(page, () => document.getElementById('chat-project-form') !== null, 'New project form never opened', 15000);
  await page.fill('#chat-project-label', 'Reply Actions Project');
  await page.fill('#chat-project-cwd', runnerCwd);
  await page.click('#chat-project-add');
  await waitFor(page, () => [...document.querySelectorAll('.chat-project-row')].some(
    (row) => row.getAttribute('data-selected') === 'true'
      && (row.querySelector('.chat-project-row__label')?.textContent || '').includes('Reply Actions Project'),
  ), 'new project row was never created and selected', 15000);
  await page.click('#chat-invite-runner-btn');
  await waitFor(page, () => document.getElementById('chat-invite') !== null, 'Invite runner dialog never opened', 15000);
  await page.fill('#chat-runner-label', 'Reply Actions Runner');
  await page.fill('#chat-runner-address', runnerOrigin);
  await page.click('#chat-runner-add');
  const runnerCard = page.locator('#chat-invite .chat-machine-card', { hasText: runnerOrigin }).first();
  await runnerCard.locator('button', { hasText: 'Token required' }).click({ timeout: 30000 });
  await page.fill('#chat-runner-token', runnerToken);
  await page.click('#chat-runner-add');
  await waitFor(page, () => document.getElementById('chat-invite') === null, 'invite dialog never closed after the token two-step', 45000);
  await waitFor(page, () => {
    const chip = document.querySelector('.chat-chip[data-runner-id]');
    return chip !== null && chip.querySelector('.chat-status-dot')?.getAttribute('data-tone') === 'ok';
  }, 'connected runner chip never appeared', 30000);
  await page.locator('.chat-chip[data-runner-id]').first().click();
  await waitFor(page, () => document.querySelector('.chat-session__transcript') !== null, 'SessionView transcript never mounted', 30000);
}

let browser;
try {
  browser = await chromium.launch(chromePath ? { executablePath: chromePath } : {});
} catch (error) {
  console.error(`web-reply-actions: SETUP FAILED: chromium launch failed (fingerprint ${fp(error?.message || error)})`);
  process.exitCode = 1;
}

if (process.exitCode === undefined || process.exitCode === 0) {
  try {
    if (!url) fail('RPI_URL missing');
    if (!richPath || !fs.existsSync(richPath)) fail('RPI_EXPECTED_RICH missing (lane shell must extract the mock constants)');
    if (!richExtraPath || !fs.existsSync(richExtraPath)) fail('RPI_EXPECTED_RICH_EXTRA missing');
    if (!chatUrl || !runnerOrigin || !runnerToken || !runnerCwd) fail('RPI_CHAT_URL / RPI_RUNNER_ORIGIN / RPI_RUNNER_TOKEN / RPI_RUNNER_CWD are required for the /chat hard leg');
    const richRaw = fs.readFileSync(richPath);
    const richExtraRaw = fs.readFileSync(richExtraPath);
    const richUtf8 = richRaw.toString('utf8');
    const richExtraUtf8 = richExtraRaw.toString('utf8');
    if (richUtf8.length === 0 || richExtraUtf8.length === 0) fail('extracted expectation is empty');

    // ================= Desktop 1280x800 =================
    CURRENT_PHASE = 'desktop.boot';
    const desktop = await newPage(browser, { width: 1280, height: 800, mobile: false });
    record('RA0-boot-connected');

    CURRENT_PHASE = 'replyA';
    let prevAssistant = await desktop.page.evaluate(() => document.querySelectorAll('main#transcript .msg--assistant').length);
    await sendPrompt(desktop.page, 'render rich content');
    const replyA = await waitNewSettledReply(desktop.page, prevAssistant, 'table');
    record('RA1-rich-reply-settles');

    let counts = await desktop.page.evaluate(census, replyA.entryId);
    assertThreeButtons(counts, 'desktop reply A');
    record('RA2-three-buttons-vertical-visible');

    await clickReplyButton(desktop.page, replyA.entryId, 'copy-markdown');
    await waitFor(
      desktop.page,
      () => navigator.clipboard.readText().then((t) => t.length > 0),
      'clipboard never received the copied markdown',
      8000,
      undefined,
      diagnose,
    );
    const copiedA = await readClipboard(desktop.page);
    if (copiedA !== richUtf8 || Buffer.byteLength(copiedA, 'utf8') !== richRaw.length) {
      fail(`RA3: clipboard is not the byte-exact raw markdown (len ${Buffer.byteLength(copiedA, 'utf8')} vs ${richRaw.length}, sha ${createHash('sha256').update(copiedA, 'utf8').digest('hex').slice(0, 16)})`);
    }
    record('RA3-copy-clipboard-byte-exact');

    // PER-SAVE BLOB BASELINE: the invariant is per SAVE, not cumulative —
    // each save must add EXACTLY one createObjectURL and one
    // revokeObjectURL, and the revoked URL must be the very URL that save
    // created. A cumulative count-equal check passes 0/0 and would also
    // pass a save that double-creates and double-revokes.
    const blobLogLength = (page) => page.evaluate(() => (window.__blobLog || []).length);
    const assertBlobSave = async (page, baseline, surface) => {
      await waitFor(
        page,
        (base) => {
          const fresh = (window.__blobLog || []).slice(base);
          return fresh.some((e) => e.kind === 'create') && fresh.some((e) => e.kind === 'revoke');
        },
        `${surface}: the save's blob create+revoke never completed`,
        8000,
        baseline,
        diagnose,
      );
      const fresh = await page.evaluate((base) => (window.__blobLog || []).slice(base), baseline);
      const creates = fresh.filter((e) => e.kind === 'create');
      const revokes = fresh.filter((e) => e.kind === 'revoke');
      if (creates.length !== 1 || revokes.length !== 1) {
        fail(`${surface}: blob delta must be EXACTLY one create + one revoke (create=${creates.length} revoke=${revokes.length})`);
      }
      if (creates[0].url !== revokes[0].url) {
        fail(`${surface}: the revoked blob URL is not the URL that save created (create len ${creates[0].url.length}, revoke len ${revokes[0].url.length})`);
      }
    };

    const blobBaseline1 = await blobLogLength(desktop.page);
    const [download1] = await Promise.all([
      desktop.page.waitForEvent('download', { timeout: 15000 }),
      clickReplyButton(desktop.page, replyA.entryId, 'save-markdown'),
    ]);
    const filename1 = download1.suggestedFilename();
    assertMdFilename(filename1, 'desktop save #1');
    const bytes1 = fs.readFileSync(await download1.path());
    if (Buffer.compare(bytes1, richRaw) !== 0) {
      fail(`RA4: saved .md bytes differ from the raw markdown (file ${bytes1.length}B vs expected ${richRaw.length}B)`);
    }
    await assertBlobSave(desktop.page, blobBaseline1, 'desktop save #1');
    record('RA4-save-filename-bytes-blob');

    const blobBaseline2 = await blobLogLength(desktop.page);
    const [download2] = await Promise.all([
      desktop.page.waitForEvent('download', { timeout: 15000 }),
      clickReplyButton(desktop.page, replyA.entryId, 'save-markdown'),
    ]);
    if (download2.suggestedFilename() !== filename1) {
      fail(`RA5: two saves of one reply produced different filenames (lens ${filename1.length}/${download2.suggestedFilename().length})`);
    }
    await assertBlobSave(desktop.page, blobBaseline2, 'desktop save #2');
    record('RA5-two-saves-same-filename');

    CURRENT_PHASE = 'replyA.reload';
    await desktop.page.reload({ waitUntil: 'domcontentloaded' });
    await waitFor(desktop.page, () => document.title === 'rpi web', 'page title missing after reload', 20000, undefined, diagnose);
    await waitReady(desktop.page);
    await waitFor(
      desktop.page,
      (id) => document.querySelector(`main#transcript .msg--assistant[data-entry-id="${id}"] button[data-reply-action="save-markdown"]`) !== null,
      'restored transcript never remounted reply A with its save control',
      40000,
      replyA.entryId,
      diagnose,
    );
    const blobBaseline3 = await blobLogLength(desktop.page);
    const [download3] = await Promise.all([
      desktop.page.waitForEvent('download', { timeout: 15000 }),
      clickReplyButton(desktop.page, replyA.entryId, 'save-markdown'),
    ]);
    if (download3.suggestedFilename() !== filename1) {
      fail(`RA6: filename not stable across reload (lens ${filename1.length}/${download3.suggestedFilename().length})`);
    }
    const bytes3 = fs.readFileSync(await download3.path());
    if (Buffer.compare(bytes3, richRaw) !== 0) fail('RA6: reloaded save bytes diverged from the raw markdown');
    await clickReplyButton(desktop.page, replyA.entryId, 'copy-markdown');
    await waitFor(
      desktop.page,
      (want) => navigator.clipboard.readText().then((t) => t === want),
      'restored copy is not byte-exact raw markdown',
      8000,
      richUtf8,
      diagnose,
    );
    await assertBlobSave(desktop.page, blobBaseline3, 'desktop save #3');
    record('RA6-reload-stable-filename-and-raw');

    CURRENT_PHASE = 'replyB';
    prevAssistant = await desktop.page.evaluate(() => document.querySelectorAll('main#transcript .msg--assistant').length);
    // bg2398 ROOT CAUSE (source-determined): 'render rich content extra'
  // SUBSTRING-matches the RICH_TEXT branch (user_mock_server.py:1083) and
  // delivered A's constant again. The REAL RICH_TEXT_EXTRA marker is 'render
  // markdown extra branches' (:1095). Never relax the marker/byte asserts.
    await sendPrompt(desktop.page, 'render markdown extra branches');
    const replyB = await waitNewSettledReply(desktop.page, prevAssistant, 'blockquote');
    if (replyB.entryId === replyA.entryId) fail('RA7: second reply reused reply A entry id');
    record('RA7-extra-reply-settles');
    // FIXTURE-IMAGE PROOF AT THE RIGHT STAGE (bg2413): the relative
//  ./images/pic.png lives in RICH_TEXT_EXTRA — reply B, NOT reply A (A's
//  only img is the data URI, which decodes normally). The lazy img is
//  located by EXACT pathname /images/pic.png, really scrolled into view
//  (firing the lazy fetch through the fixture route), and must decode
//  (complete + naturalWidth > 0); the route counters prove the fixture was
//  exercised. Source bytes untouched; zero-error kept.
    CURRENT_PHASE = 'replyB.image';
    await waitFor(
      desktop.page,
      (id) => {
        const imgs = [...document.querySelectorAll(`main#transcript .msg--assistant[data-entry-id="${id}"] img`)];
        const pic = imgs.find((img) => {
          try { return new URL(img.currentSrc || img.getAttribute('src') || '', location.href).pathname === '/images/pic.png'; } catch { return false; }
        });
        if (!pic) return false;
        const r = pic.getBoundingClientRect();
        const inVp = r.height > 0 && r.top < window.innerHeight && r.bottom > 0;
        if (!inVp) { pic.scrollIntoView({ block: 'center' }); return false; }
        return pic.complete === true && pic.naturalWidth > 0;
      },
      'the fixture image never decoded in reply B (exact /images/pic.png, lazy fetch fired by a real scroll, naturalWidth > 0)',
      45000,
      replyB.entryId,
      diagnose,
    );
    if (picRouteHits < 1) fail(`the /images/pic.png fixture route was never exercised (hits=${picRouteHits}) — the decode proof must come from the fixture resource`);

    counts = await desktop.page.evaluate(census, replyB.entryId);
    assertThreeButtons(counts, 'desktop reply B');
    await clickReplyButton(desktop.page, replyB.entryId, 'copy-markdown');
    await waitFor(
      desktop.page,
      (want) => navigator.clipboard.readText().then((t) => t === want),
      'reply B clipboard is not the byte-exact extra markdown',
      8000,
      richExtraUtf8,
      diagnose,
    );
    const blobBaselineB1 = await blobLogLength(desktop.page);
    const [downloadB1] = await Promise.all([
      desktop.page.waitForEvent('download', { timeout: 15000 }),
      clickReplyButton(desktop.page, replyB.entryId, 'save-markdown'),
    ]);
    const filenameB1 = downloadB1.suggestedFilename();
    assertMdFilename(filenameB1, 'reply B save #1');
    if (Buffer.compare(fs.readFileSync(await downloadB1.path()), richExtraRaw) !== 0) fail('RA8: reply B saved bytes diverged');
    await assertBlobSave(desktop.page, blobBaselineB1, 'reply B save #1');
    const blobBaselineB2 = await blobLogLength(desktop.page);
    const [downloadB2] = await Promise.all([
      desktop.page.waitForEvent('download', { timeout: 15000 }),
      clickReplyButton(desktop.page, replyB.entryId, 'save-markdown'),
    ]);
    if (downloadB2.suggestedFilename() !== filenameB1) {
      fail(`RA8: reply B saves diverged (lens ${filenameB1.length}/${downloadB2.suggestedFilename().length})`);
    }
    if (filenameB1 === filename1) fail('RA8: two different replies produced the same filename');
    await assertBlobSave(desktop.page, blobBaselineB2, 'reply B save #2');
    record('RA8-extra-byte-exact-and-stable');

    // ---- Clipboard failure -> CENTRAL error toast (no invented flash) ----
    CURRENT_PHASE = 'replyA.toastfail';
    // MECHANISM (CopyButton.copyTextToClipboard): navigator.clipboard is
  // resolved AT CALL TIME; writeText rejection is CAUGHT -> helper returns
  // false -> the copy-markdown caller toasts the CENTRAL error. Per-attempt
  // baselines (bg2399): the invariant is 'the successful copy adds NO toast
  // beyond the failed attempt's own', measured immediately around the success
  // click; the attempt/toast timelines attribute every toast.
    const preFail = await desktop.page.evaluate(() => ({ toasts: document.querySelectorAll('#toasts .toast').length, attempts: window.__clipLog.length }));
    await desktop.page.evaluate(() => {
      window.__wrappedClipboard = window.__wrappedClipboard || navigator.clipboard;
      Object.defineProperty(navigator, 'clipboard', {
        configurable: true,
        value: {
          writeText: (text) => {
            window.__clipLog.push({ kind: 'forced-fail', t: Date.now(), len: typeof text === 'string' ? text.length : -1, sha: typeof text === 'string' ? window.__djb2(text) : '', result: 'rejected' });
            return Promise.reject(new Error('denied'));
          },
          readText: window.__wrappedClipboard.readText,
        },
      });
    });
    await clickReplyButton(desktop.page, replyA.entryId, 'copy-markdown');
    await waitFor(
      desktop.page,
      (prev) => {
        const toasts = [...document.querySelectorAll('#toasts .toast')];
        return toasts.length > prev
          && toasts.slice(prev).some((t) => t.classList.contains('toast--error') || t.getAttribute('role') === 'alert');
      },
      'clipboard rejection never surfaced the central error toast',
      8000,
      preFail.toasts,
      diagnose,
    );
    const afterFail = await desktop.page.evaluate(() => ({ toasts: document.querySelectorAll('#toasts .toast').length, attempts: window.__clipLog.length }));
    if (afterFail.attempts <= preFail.attempts) fail('RA9: the forced-fail copy attempt was never recorded in the attempt timeline');
    await desktop.page.screenshot({ path: `${evidence}/reply-actions-copyfail-toast.png`, fullPage: false });
    fs.chmodSync(`${evidence}/reply-actions-copyfail-toast.png`, 0o600);
    await desktop.page.evaluate(() => {
      Object.defineProperty(navigator, 'clipboard', { configurable: true, value: window.__wrappedClipboard });
    });
    const preSuccess = await desktop.page.evaluate(() => ({ toasts: document.querySelectorAll('#toasts .toast').length, attempts: window.__clipLog.length }));
    await clickReplyButton(desktop.page, replyA.entryId, 'copy-markdown');
    await waitFor(
      desktop.page,
      (want) => navigator.clipboard.readText().then((t) => t === want),
      'post-restore copy is not byte-exact raw markdown',
      8000,
      richUtf8,
      diagnose,
    );
    const postSuccess = await desktop.page.evaluate(() => ({ toasts: document.querySelectorAll('#toasts .toast').length, attempts: window.__clipLog.length, lastAttempt: (window.__clipLog[window.__clipLog.length - 1] || {}).result || '' }));
    if (postSuccess.lastAttempt !== 'resolved') fail(`RA9: the post-restore copy attempt did not resolve (result=${postSuccess.lastAttempt})`);
    if (postSuccess.attempts <= preSuccess.attempts) fail(`RA9: the post-restore copy recorded NO new attempt (attempts ${preSuccess.attempts} -> ${postSuccess.attempts}) — stale clipboard content must never false-pass`);
    if (postSuccess.toasts > preSuccess.toasts) {
      fail(`RA9: the successful copy raised a NEW toast (toasts preSuccess=${preSuccess.toasts} -> postSuccess=${postSuccess.toasts}; lingering PRIOR-failure toasts are excluded by the per-attempt baseline; attempt/toast timelines are in the diagnostics)`);
    }
    record('RA9-copy-fail-central-toast');
    await desktop.page.screenshot({ path: `${evidence}/reply-actions-desktop.png`, fullPage: false });
    fs.chmodSync(`${evidence}/reply-actions-desktop.png`, 0o600);

    // ============ Phone 390x844 (touch) — inherits the SAME replies =========
    CURRENT_PHASE = 'phone';
    const phone = await newPage(browser, { width: 390, height: 844, mobile: true });
    await waitFor(
      phone.page,
      (id) => document.getElementById('reconnect-overlay') === null
        && (() => { const row = document.querySelector(`main#transcript .msg--assistant[data-entry-id="${id}"]`); return !!row && !!row.querySelector('button.assistant-star-toggle'); })()
        && document.querySelector(`main#transcript .msg--assistant[data-entry-id="${id}"] button[data-reply-action="save-markdown"]`) !== null,
      'phone restore never reached true ready (overlay absent + same entry id hydrated + reply-action controls mounted)',
      40000,
      replyA.entryId,
      diagnose,
    );
    record('RA10-phone-restores-reply');
    const phoneCounts = await phone.page.evaluate(census, replyA.entryId);
    assertThreeButtons(phoneCounts, 'phone reply A');
    assertTouch(phoneCounts, 'phone reply A');
    record('RA11-phone-three-buttons-touch');
    await tapReplyButton(phone.page, replyA.entryId, 'copy-markdown');
    await waitFor(
      phone.page,
      (want) => navigator.clipboard.readText().then((t) => t === want),
      'phone copy is not the byte-exact raw markdown',
      8000,
      richUtf8,
      diagnose,
    );
    record('RA12-phone-copy-byte-exact');
    const blobBaselinePhone = await blobLogLength(phone.page);
    const [phoneDownload] = await Promise.all([
      phone.page.waitForEvent('download', { timeout: 15000 }),
      tapReplyButton(phone.page, replyA.entryId, 'save-markdown'),
    ]);
    if (phoneDownload.suggestedFilename() !== filename1) {
      fail(`RA13: phone save of the same reply produced a different filename (lens ${filename1.length}/${phoneDownload.suggestedFilename().length})`);
    }
    if (Buffer.compare(fs.readFileSync(await phoneDownload.path()), richRaw) !== 0) fail('RA13: phone save bytes diverged');
    await assertBlobSave(phone.page, blobBaselinePhone, 'phone save');
    record('RA13-phone-save-same-filename');
    await waitFor(
      phone.page,
      (id) => document.querySelector(`main#transcript .msg--assistant[data-entry-id="${id}"] button[data-reply-action="save-markdown"]`) !== null,
      'phone restore never mounted reply B',
      20000,
      replyB.entryId,
      diagnose,
    );
    const phoneCountsB = await phone.page.evaluate(census, replyB.entryId);
    assertThreeButtons(phoneCountsB, 'phone reply B');
    assertTouch(phoneCountsB, 'phone reply B');
    // Same fixture-image proof on the PHONE's reply B (bg2410's 404 was at
//  this exact stage: the phone context restores reply B whose relative img
//  lazy-fetches /images/pic.png — the fixture route must serve it and it
//  must decode).
    CURRENT_PHASE = 'phone.replyB.image';
    await waitFor(
      phone.page,
      (id) => {
        const imgs = [...document.querySelectorAll(`main#transcript .msg--assistant[data-entry-id="${id}"] img`)];
        const pic = imgs.find((img) => {
          try { return new URL(img.currentSrc || img.getAttribute('src') || '', location.href).pathname === '/images/pic.png'; } catch { return false; }
        });
        if (!pic) return false;
        const r = pic.getBoundingClientRect();
        const inVp = r.height > 0 && r.top < window.innerHeight && r.bottom > 0;
        if (!inVp) { pic.scrollIntoView({ block: 'center' }); return false; }
        return pic.complete === true && pic.naturalWidth > 0;
      },
      'the fixture image never decoded in the phone reply B (exact /images/pic.png, lazy fetch fired by a real scroll, naturalWidth > 0)',
      45000,
      replyB.entryId,
      diagnose,
    );
    record('RA14-phone-extra-row-touch');
    await phone.page.screenshot({ path: `${evidence}/reply-actions-phone.png`, fullPage: false });
    fs.chmodSync(`${evidence}/reply-actions-phone.png`, 0o600);

    // ====== /chat surface leg — HARD ASSERTION (Main contract) =============
    CURRENT_PHASE = 'chat';
    const chatPage = await desktop.context.newPage();
    let chatBootstrapping = true;
    const expectedBootstrapError = `WebSocket connection to '${runnerOrigin.replace(/^http/, 'ws')}/ws' failed: HTTP Authentication failed; no valid credentials available`;
    attachErrorCollector(chatPage, { toleratedExact: expectedBootstrapError, toleratedWhile: () => chatBootstrapping });
    await chatPage.goto(chatUrl, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(chatPage, () => document.getElementById('chat-projects') !== null, '/chat app did not mount', 20000, undefined, diagnose);
    await openChatSessionView(chatPage);
    chatBootstrapping = false;
    // /chat has its OWN #reconnect-overlay (ChatApp) — the same hit-covering
  // class as bg2400; wait for it to be ABSENT before prompting (never
  // removed; a persisting overlay FAILS).
    await waitFor(chatPage, () => document.getElementById('reconnect-overlay') === null, '/chat reconnect-overlay never cleared (ready gate)', 30000, undefined, diagnose);
    CURRENT_PHASE = 'chat.turn';
    await chatPage.fill('[aria-label="Session prompt"]', 'render rich content');
    await chatPage.press('[aria-label="Session prompt"]', 'Enter');
    await waitFor(
      chatPage,
      () => [...document.querySelectorAll('.chat-session__transcript .chat-session-message--assistant')]
        .some((row) => row.getAttribute('data-status') === 'final' && row.querySelector('table.md-table') !== null),
      '/chat reply never settled with the rich table rendered',
      45000,
      undefined,
      diagnose,
    );
    // AUTHORITATIVE HYDRATION GATE for /chat (bg2402/bg2403): a rendered
  // `final` row is NOT canonical yet — take the row ONLY after its star
  // button carries an entry id; never skip the star, never mask. Timeout
  // captures the chat-safe diagnostics (which require NOTHING to exist).
    CURRENT_PHASE = 'chat.publish';
    await waitFor(
      chatPage,
      () => {
        const row = [...document.querySelectorAll('.chat-session__transcript .chat-session-message--assistant')]
          .find((r) => r.getAttribute('data-status') === 'final' && r.querySelector('table.md-table') !== null);
        if (!row) return false;
        const star = row.querySelector('button.assistant-star-toggle');
        return star !== null && (star.getAttribute('data-entry-id') || '') !== '';
      },
      '/chat reply never reached authoritative hydration (star chrome with entry id on the final row)',
      30000,
      undefined,
      diagnose,
    );
    CURRENT_PHASE = 'chat.census';
    const chatCensus = await chatPage.evaluate(() => {
      const row = [...document.querySelectorAll('.chat-session__transcript .chat-session-message--assistant')]
        .find((r) => r.getAttribute('data-status') === 'final' && r.querySelector('table.md-table') !== null);
      if (!row) return { missing: 'settled /chat assistant row' };
      // SAME ACTION SCOPE as /web (bg2396): prefer .msg--assistant__actions;
  // otherwise row buttons EXCLUDING the markdown-body containers.
      const actionsGroup = row.querySelector('.msg--assistant__actions');
      const bodyButtonSet = new Set(row.querySelectorAll('.chat-session-markdown button, .chat-session-thinking button, .thinking__body button, .chat-session-message--assistant details button'));
      const scope = actionsGroup || row;
      const inVp = (rect) => rect.top >= 0 && rect.left >= 0 && rect.bottom <= (window.innerHeight || document.documentElement.clientHeight) && rect.right <= (window.innerWidth || document.documentElement.clientWidth);
      const buttons = [...scope.querySelectorAll('button')]
        .filter((b) => !bodyButtonSet.has(b))
        .map((b) => {
          let r = b.getBoundingClientRect();
          if (!inVp(r)) { b.scrollIntoView({ block: 'center' }); r = b.getBoundingClientRect(); }
          const hitEl = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
          return {
            action: b.getAttribute('data-reply-action') || (b.classList.contains('assistant-star-toggle') ? 'star' : `unknown:${b.className.slice(0, 40)}`),
            w: r.width, h: r.height, x: r.left, y: r.top,
            opacity: getComputedStyle(b).opacity,
            inViewport: inVp(r),
            hit: !!hitEl && (hitEl === b || b.contains(hitEl)),
            hitCls: hitEl ? String(hitEl.className).slice(0, 60) : '(none at center point)',
          };
        });
      const byAction = (name) => buttons.filter((b) => b.action === name).length;
      const m = window.__wsMeta || { out: {}, wireAssistantIds: [] };
      return {
        buttonTotal: buttons.length,
        star: byAction('star'),
        copyMarkdown: byAction('copy-markdown'),
        saveMarkdown: byAction('save-markdown'),
        buttons,
        diag: {
          rowStatuses: [...document.querySelectorAll('.chat-session__transcript .chat-session-message--assistant')].map((r) => r.getAttribute('data-status') || ''),
          rowEntryIdPresent: (() => { const star = row.querySelector('button.assistant-star-toggle'); return !!star && (star.getAttribute('data-entry-id') || '') !== ''; })(),
          actionsGroupPresent: actionsGroup !== null,
          bodyButtonCount: row.querySelectorAll('.chat-session-markdown button, .chat-session-thinking button, .thinking__body button, .chat-session-message--assistant details button').length,
          getMessagesSent: m.out.get_messages || 0,
          wireResponseHasIds: m.wireAssistantIds.length > 0,
          wireAssistantIdsCount: m.wireAssistantIds.length,
        },
      };
    });
    if (chatCensus.missing) fail(`/chat: ${chatCensus.missing}`);
    if (chatCensus.buttonTotal !== 3 || chatCensus.star !== 1 || chatCensus.copyMarkdown !== 1 || chatCensus.saveMarkdown !== 1) {
      fail(`RA15: /chat final hydrated row must expose the FULL three-button contract (star/copy-markdown/save-markdown, Main contract — all replies, no silent star exclusion), got ${JSON.stringify({ buttonTotal: chatCensus.buttonTotal, star: chatCensus.star, copyMarkdown: chatCensus.copyMarkdown, saveMarkdown: chatCensus.saveMarkdown })} diag=${JSON.stringify(chatCensus.diag)}`);
    }
    const [cStar, cCopy, cSave] = chatCensus.buttons;
    if (cStar.action !== 'star' || cCopy.action !== 'copy-markdown' || cSave.action !== 'save-markdown') {
      fail(`RA15: /chat DOM button order is not star/copy/save: ${JSON.stringify(chatCensus.buttons.map((b) => b.action))}`);
    }
    if (Math.abs(cStar.x - cCopy.x) > 1 || Math.abs(cCopy.x - cSave.x) > 1) {
      fail(`RA15: /chat buttons are not one vertical column (x): star.x=${cStar.x} copy.x=${cCopy.x} save.x=${cSave.x}`);
    }
    if (!(cStar.y < cCopy.y && cCopy.y < cSave.y)) {
      fail(`RA15: /chat vertical stack order violated (y must increase star→copy→save): star.y=${cStar.y} copy.y=${cCopy.y} save.y=${cSave.y}`);
    }
    for (const b of chatCensus.buttons) {
      if (!(b.w > 0 && b.h > 0 && parseFloat(b.opacity) > 0)) fail(`RA15: /chat button ${b.action} not visible without hover: ${JSON.stringify(b)}`);
      if (!b.inViewport) fail(`RA15: /chat button ${b.action} still outside the viewport even after its own real scrollIntoView — an offscreen control is NEVER judged as a no-hover defect`);
      if (!b.hit) fail(`RA15: /chat button ${b.action} not clickable without hover while truly in-viewport (hitCls=${b.hitCls}): ${JSON.stringify(b)}`);
    }
    // Star wiring to the SHARED reply-star store: real click/un-toggle.
    await chatPage.click('.chat-session__transcript .chat-session-message--assistant button.assistant-star-toggle');
    await waitFor(chatPage, () => document.querySelector('.chat-session__transcript .chat-session-message--assistant button.assistant-star-toggle')?.getAttribute('aria-pressed') === 'true', 'RA15: /chat star click never set aria-pressed=true', 8000, undefined, diagnose);
    await chatPage.click('.chat-session__transcript .chat-session-message--assistant button.assistant-star-toggle');
    await waitFor(chatPage, () => document.querySelector('.chat-session__transcript .chat-session-message--assistant button.assistant-star-toggle')?.getAttribute('aria-pressed') === 'false', 'RA15: /chat star un-toggle never restored aria-pressed=false', 8000, undefined, diagnose);
    await chatPage.click('.chat-session__transcript .chat-session-message--assistant button[data-reply-action="copy-markdown"]');
    await waitFor(chatPage, (want) => navigator.clipboard.readText().then((t) => t === want), '/chat copy is not the byte-exact raw markdown', 8000, richUtf8, diagnose);
    const blobBaselineChat1 = await blobLogLength(chatPage);
    const [chatDownload1] = await Promise.all([
      chatPage.waitForEvent('download', { timeout: 15000 }),
      chatPage.click('.chat-session__transcript .chat-session-message--assistant button[data-reply-action="save-markdown"]'),
    ]);
    const chatFilename1 = chatDownload1.suggestedFilename();
    assertMdFilename(chatFilename1, '/chat save #1');
    if (Buffer.compare(fs.readFileSync(await chatDownload1.path()), richRaw) !== 0) fail('RA15: /chat saved bytes diverged from the raw markdown');
    await assertBlobSave(chatPage, blobBaselineChat1, '/chat save #1');
    const blobBaselineChat2 = await blobLogLength(chatPage);
    const [chatDownload2] = await Promise.all([
      chatPage.waitForEvent('download', { timeout: 15000 }),
      chatPage.click('.chat-session__transcript .chat-session-message--assistant button[data-reply-action="save-markdown"]'),
    ]);
    if (chatDownload2.suggestedFilename() !== chatFilename1) {
      fail(`RA15: /chat two saves diverged (lens ${chatFilename1.length}/${chatDownload2.suggestedFilename().length})`);
    }
    await assertBlobSave(chatPage, blobBaselineChat2, '/chat save #2');
    await chatPage.screenshot({ path: `${evidence}/reply-actions-chat.png`, fullPage: false }).catch(() => {});
    fs.chmodSync(`${evidence}/reply-actions-chat.png`, 0o600);
    await chatPage.close();
    desktop.page.__errorEntries.push(...chatPage.__errorEntries);
    record('RA15-chat-surface-contract');

    CURRENT_PHASE = 'final';
    const errors = errorEvidence([desktop.page, phone.page]);
    if (errors.count > 0) fail(`browser errors: count=${errors.count} tolerated=${errors.tolerated} sha256=${errors.sha256} entries=${JSON.stringify(errors.entries)} netFailures=${JSON.stringify(errors.netFailures)}`);
    // HYDRATION-DUPLICATION CONTRACT (bg2398): DOM assistant entry ids must be
  // UNIQUE; wire authoritative ids stay INTERNAL (counts/bools only).
    const uniqueness = await desktop.page.evaluate(() => {
      const rows = [...document.querySelectorAll('main#transcript .msg--assistant')];
      const ids = rows.map((r) => r.getAttribute('data-entry-id')).filter((v) => typeof v === 'string' && v !== '');
      const m = window.__wsMeta || null;
      const wire = m !== null && Array.isArray(m.wireAssistantIds) ? m.wireAssistantIds : null;
      const liveIds = m !== null && Array.isArray(m.liveAssistantItemIds) ? m.liveAssistantItemIds : null;
      const inWire = (cands, w) => cands.length > 0 && w.length > 0 && cands.every((id) => w.includes(id));
      // NEGATIVE CONTROLS, computed IN-PAGE so ids never surface (counts
      // and bools only). Fixtures come from the ACTUAL wrapped wire data
      // this run observed: (a) the empty-wire case must evaluate FALSE,
      // never a vacuous pass; (b) a synthetic id observed ONLY as a live
      // event itemId — never a durable wrapper id — must be REJECTED by
      // the same membership predicate; if the live stream happened to
      // carry only durable ids this run, the derived variant of an actual
      // durable id (guaranteed absent) keeps the rejection exercised.
      const liveOnlyIds = liveIds === null ? [] : liveIds.filter((id) => wire !== null && !wire.includes(id));
      const synthetic = liveOnlyIds.length > 0
        ? liveOnlyIds[0]
        : (wire !== null && wire.length > 0 ? `${wire[0]}-e2e-synthetic` : 'e2e-synthetic-unwired');
      return {
        wireHookPresent: m !== null,
        liveItemIdsCollected: liveIds !== null,
        liveAssistantItemIdsCount: liveIds === null ? -1 : liveIds.length,
        liveOnlyIdsCount: liveOnlyIds.length,
        domEntryIdsPresent: ids.length,
        domEntryIdsUnique: new Set(ids).size === ids.length,
        wireAssistantIdsCount: wire === null ? -1 : wire.length,
        // FALSE (never null) when either side is empty: an empty DOM set, a
        // missing hook, or an empty wire set is a FAIL — never a vacuous pass.
        domEntryIdsAllInWire: ids.length > 0 && wire !== null && wire.length > 0
          ? ids.every((id) => wire.includes(id))
          : false,
        negativeEmptyWireFalse: !inWire(ids, []),
        negativeLiveOnlyRejected: !inWire([...ids, synthetic], wire === null ? [] : wire),
      };
    });
    if (!uniqueness.domEntryIdsUnique) fail(`hydration duplicate rows detected (DOM assistant entry ids not unique: present=${uniqueness.domEntryIdsPresent}; wire ids stay internal — bools only)`);
    if (!uniqueness.wireHookPresent) fail('the WS metadata hook never ran (window.__wsMeta missing) — wire provenance is unprovable and this gate fails closed');
    if (!uniqueness.liveItemIdsCollected) fail('the WS metadata hook lost its liveAssistantItemIds collection — the authoritative-vs-live discrimination is unprovable and this gate fails closed');
    if (uniqueness.domEntryIdsPresent === 0) fail('no DOM assistant entry id was present at the final gate — an empty DOM set is never a vacuous pass');
    if (uniqueness.wireAssistantIdsCount <= 0) fail('no authoritative assistant wrapper id was ever observed on the wire (collector matched no get_messages row) — an empty wire set is never a vacuous pass');
    if (!uniqueness.domEntryIdsAllInWire) fail('a DOM assistant entry id was never seen on the wire (synthetic/unhydrated id or broken provenance; ids stay internal)');
    if (uniqueness.negativeEmptyWireFalse !== true) fail('negative control FAILED: the provenance predicate did not evaluate false on an empty wire set — a vacuous pass path exists');
    if (uniqueness.negativeLiveOnlyRejected !== true) fail(`negative control FAILED: a synthetic id observed only as a live itemId (liveOnly=${uniqueness.liveOnlyIdsCount}, live total=${uniqueness.liveAssistantItemIdsCount}) was ACCEPTED as wire-provenanced — live itemIds must never count as authoritative recorder entry ids`);
    record('RA16-zero-browser-errors');

    const summary = {
      result: 'PASSED',
      executed: [...executed],
      documented: DOCUMENTED_IDS,
      counts: { documented: DOCUMENTED_IDS.length, executed: executed.size },
      filenameLenReplyA: filename1.length,
      filenameLenReplyB: filenameB1.length,
      filenameLenChat: chatFilename1.length,
      chatBootstrapAuthErrors: chatPage.__toleratedCount(),
    };
    fs.writeFileSync(`${evidence}/reply-actions-assertions.json`, `${JSON.stringify(summary, null, 2)}\n`);
    fs.chmodSync(`${evidence}/reply-actions-assertions.json`, 0o600);
    console.log(`web-reply-actions: PASSED (${executed.size}/${DOCUMENTED_IDS.length} executed; /web + /chat both hard-asserted) — evidence ${evidence}`);
    await desktop.context.close();
    await phone.context.close();
  } catch (error) {
    try {
  // LEDGER PRIVACY CONTRACT (bg2401): LaneFail carries MY OWN composed
  // contract text (safe); any UNEXPECTED error is reduced to a SAFE category
  // + method + fingerprint — never the raw message/stack.
      const composed = error instanceof LaneFail;
      const raw = error instanceof Error ? error.message : String(error);
      const category = composed ? 'contract-failure'
        : /is not defined|ReferenceError/.test(raw) ? 'reference-undefined'
        : /waitForFunction/.test(raw) ? 'waitForFunction'
        : /Timeout|timed out/.test(raw) ? 'timeout'
        : 'unexpected';
      const method = (raw.match(/([A-Za-z_$][\w$]*) is not defined/) || [])[1]
        || (raw.match(/(?:page|context|browser)\.(\w+)/) || [])[1] || '';
      fs.writeFileSync(`${evidence}/reply-actions-assertions.json`, `${JSON.stringify({ result: 'FAILED', phase: CURRENT_PHASE, failureCategory: category, failureMethod: method, failureFp: composed ? undefined : fp(error?.stack || raw), failure: composed ? error.message : undefined, executedSoFar: [...executed] }, null, 2)}\n`);
      fs.chmodSync(`${evidence}/reply-actions-assertions.json`, 0o600);
    } catch { /* best-effort ledger preservation */ }
    if (error instanceof LaneFail) {
      console.error(`web-reply-actions: FAIL: ${error.message}`);
      process.exitCode = 2;
    } else {
      console.error(`web-reply-actions: UNEXPECTED ERROR (fingerprint ${fp(error?.stack || error)})`);
      process.exitCode = process.exitCode === undefined || process.exitCode === 0 ? 2 : process.exitCode;
    }
  } finally {
    if (browser) {
      try {
        await browser.close();
      } catch (error) {
        console.error(`web-reply-actions: browser close FAILED, exit code escalated (fingerprint ${fp(error?.message || error)})`);
        process.exitCode = process.exitCode === undefined || process.exitCode === 0 ? 1 : process.exitCode;
      }
    }
  }
}
