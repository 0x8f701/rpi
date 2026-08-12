// Streaming transcript scroll-pinning E2E lane (playwright half of
// E2E.d/web/scroll.sh) — hardened regression gate for useScrollPin.
//
// Environment:
//   RPI_URL          http://127.0.0.1:<port>/web
//   RPI_TOKEN        token file content (served via rpi-auth.<token> subprotocol)
//   RPI_CHROME       executable path of the system Chrome (optional)
//   RPI_EVIDENCE     evidence dir for screenshots + metrics JSON
//
// Mock scenario: `scroll` (E2E.d/lib/user_mock_server.py) —
//   "scroll-long-a"  long slow stream (44 chunks, 0.35s, tail marker
//                    "scroll-long-a-done") — pinned stream + unpin phases
//   "scroll-final-md" raw-markdown deltas ending in a final render with a
//                    tall rust code fence (react-commit growth), a Mermaid
//                    fence (async SVG hydration), and a data-URL image
//                    (async decode) — tail marker "scroll-final-md-done"
//   "scroll-narrow"  short stream (8 chunks, 0.3s, tail marker
//                    "scroll-narrow-done") for the narrow/mobile viewport
//                    phase (header badge / Abort button / composer reflow)
//   "scroll-echo"    instant echo for the second session's round-trip
//
// Why the OLD logic fails this lane (root cause, from v0.2.9 history):
//   The old nearBottomRef + useEffect([activeItems]) design pinned stream
//   deltas synchronously too (same `scrollTop = scrollHeight` in the delta
//   handler), so a plain-stream gate can NOT discriminate. The old design's
//   two real gaps, both targeted here:
//   1. NO ResizeObserver: async height growth AFTER a React commit — Mermaid
//      SVG hydration, image decode — changes scrollHeight with no delta and
//      no activeItems change, so nothing ever re-pins (S5 fails: remaining
//      jumps by the async growth and never comes back).
//   2. NO forcePin on session activation: switching to a session whose
//      nearBottomRef is stale-false (user scrolled away earlier) restores
//      the transcript mid-content instead of pinning to the bottom (S7
//      fails: the activated session inherits the previous scroll state).
//   A third contract every phase enforces: the DOCUMENT must never scroll
//   (window.scrollY / document.scrollingElement.scrollTop === 0) — the app
//   owns scrolling entirely inside #transcript (no window/body double
//   scroll), and browser scroll anchoring is disabled (overflow-anchor:
//   none) so it can never shift the viewport on its own (S2.3, S3.2, ...).
//
// Method: a page-level sampler (addInitScript) records a continuous trace —
//   every rAF frame (scrollTop/scrollHeight/clientHeight/remaining + window
//   scroll metrics + header/footer heights + badge/abort presence) plus
//   classified events (text-delta / react-commit / mermaid / resize) and
//   scroll events. Assertions run over the trace segments per phase, so a
//   single missed pin surfaces as the FIRST frame where remaining exceeded
//   the tolerance, with the triggering DOM event and before/after metrics.
//
// Assertion matrix (feature "scroll pinning"; every documented ID must run):
//   S0.1 page title, S0.2 WS connected, S0.3 primary session row
//   S1.1 first streamed chunk lands while the transcript is pinned
//   S2.1 long stream: EVERY frame sample bounded (remaining <= max) while
//        pinned; window/document scroll metrics pinned at 0
//   S2.2 direct DOM deltas: scrollTop advances synchronously — every
//        text-delta growth leaves remaining bounded at the next frame
//   S2.3 no unexpected scrolls while pinned (no browser-anchoring drift):
//        every scroll event in the pinned window lands at the bottom
//   S3.1 a manual scroll away unpins (remaining > offset)
//   S3.2 unpinned: viewport EXACTLY frozen — every frame AND every scroll
//        event: scrollTop identical, window/document scroll pinned at 0
//   S3.3 the transcript keeps growing while unpinned
//   S4.1 returning to the bottom re-pins; later deltas stay glued (every
//        frame bounded)
//   S5.1 streaming -> final rendered markdown: EVERY frame from first delta
//        through commit, Mermaid hydration and image decode bounded
//   S5.2 the final render's async growth (mermaid svg swap / resize) was
//        observed AND absorbed: scrollTop advanced tracking scrollHeight
//   S6.1 narrow/mobile pinned stream: header/footer/composer layout changes
//        (badge, Abort button, textarea growth) never break the pin —
//        every frame bounded, layout change actually measured
//   S6.2 narrow/mobile unpinned stream: viewport EXACTLY frozen while the
//        layout changes and content keeps growing
//   S6.3 narrow transcript keeps growing while unpinned
//   S7.1 pre-switch unpin registered (session A is genuinely unpinned)
//   S7.2 a new session activates with its own empty view + echo round-trip
//   S7.3 switching back force-pins the activated transcript (every frame
//        bounded; never inherits the previous scroll position or pin state)

import { chromium } from 'playwright';
import fs from 'node:fs';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';

// The app's own pin tolerance is 80px (PIN_TOLERANCE_PX in scrollPin.ts);
// assertions allow a little more so a frame-boundary race (a delta between
// the app's synchronous pin and the measurement) cannot false-fail, while
// any real mid-transcript jump (a missed delta, async mermaid/image growth,
// a lost forcePin) fails hard.
const PIN_MAX_REMAINING = 120;
// Where "scroll away" lands: 200px above the bottom, comfortably beyond the
// app's pin tolerance and never at the very top of a long transcript.
const SCROLL_AWAY_OFFSET = 200;
// Multiline draft typed into the composer mid-stream (narrow phase): the
// textarea autoResize grows the footer ~2 lines with NO item change — a
// guaranteed header/footer-height layout event to measure against the pin.
const NARROW_DRAFT = 'line one\nline two\nline three\nline four';

// In-page sampler: continuous rAF trace + classified DOM events + scroll
// events + a ResizeObserver on the content wrapper. Installed before any
// app script so no growth event can escape. See header comment for the
// classification contract.
const PROBE_SCRIPT = `
(() => {
  if (window.__scrollProbe) return;
  const MAX_SAMPLES = 24000;
  const trace = [];
  let scrollEl = null;
  let contentTarget = null;

  function metrics() {
    const el = document.getElementById('transcript');
    if (!el) return null;
    const scroller = document.scrollingElement || document.documentElement;
    const header = document.querySelector('header');
    const footer = document.querySelector('.app-main > footer');
    const badge = document.getElementById('stream-badge');
    return {
      st: el.scrollTop,
      ch: el.clientHeight,
      sh: el.scrollHeight,
      rem: el.scrollHeight - el.scrollTop - el.clientHeight,
      winY: window.scrollY,
      docTop: scroller.scrollTop,
      headerH: header ? header.getBoundingClientRect().height : -1,
      footerH: footer ? footer.getBoundingClientRect().height : -1,
      badge: badge ? !badge.hidden : false,
    };
  }

  function push(kind, extra) {
    const m = metrics();
    if (!m) return;
    trace.push(Object.assign({ t: performance.now(), kind }, extra, m));
    if (trace.length > MAX_SAMPLES) trace.splice(0, trace.length - MAX_SAMPLES);
  }

  function onContainerScroll() { push('scroll', { ev: 'container' }); }
  function attachScroll() {
    if (scrollEl) scrollEl.removeEventListener('scroll', onContainerScroll);
    scrollEl = document.getElementById('transcript');
    if (scrollEl) scrollEl.addEventListener('scroll', onContainerScroll, { passive: true });
  }
  window.addEventListener('scroll', () => { push('scroll', { ev: 'window' }); }, { passive: true });

  const ro = new ResizeObserver((entries) => {
    for (const e of entries) {
      push('resize', { node: (e.target && (e.target.className || e.target.id)) || '?' });
    }
  });
  function attachResize() {
    const content = document.querySelector('.transcript-content');
    if (content && content !== contentTarget) {
      if (contentTarget) ro.unobserve(contentTarget);
      ro.observe(content);
      contentTarget = content;
    }
  }

  const mo = new MutationObserver((mutations) => {
    let cls = null;
    let detail = null;
    for (const m of mutations) {
      if (m.type === 'characterData') {
        const host = m.target.parentElement && m.target.parentElement.closest
          ? m.target.parentElement.closest('.assistant-text, .thinking__body')
          : null;
        if (host) {
          const prev = (m.oldValue || '').length;
          const now = (m.target.nodeValue || '').length;
          if (now > prev && !cls) { cls = 'text-delta'; detail = { node: host.className, grow: now - prev }; }
        }
      } else if (m.type === 'childList') {
        for (const n of m.addedNodes) {
          if (n.nodeType === 3) {
            const host = m.target && m.target.closest ? m.target.closest('.assistant-text') : null;
            if (host && !cls) { cls = 'text-delta'; detail = { node: host.className, grow: (n.nodeValue || '').length }; }
          } else if (n.nodeType === 1 && n.matches) {
            if (n.matches('.md-mermaid-host')) { cls = cls || 'commit'; detail = detail || { node: 'md-mermaid-host' }; }
            else if (n.matches('svg') && m.target && m.target.closest && m.target.closest('.md-mermaid-host')) { cls = 'mermaid'; detail = { node: 'md-mermaid-host svg' }; }
            else if (n.matches('.md-mermaid-host--rendered, .md-mermaid-host--error')) { cls = 'mermaid'; detail = { node: n.className }; }
            else if (n.matches('.msg--assistant, .msg--user, .md-fence, .md-image, .thinking, .tool-card, .bash-card, .approval')) { cls = cls || 'commit'; detail = detail || { node: n.className }; }
          }
        }
      }
    }
    if (cls) push('mut', { cls, node: (detail && detail.node) || '', grow: (detail && detail.grow) || 0 });
  });
  function attachObservers() {
    const root = document.documentElement;
    if (!root) return false;
    mo.observe(root, { childList: true, subtree: true, characterData: true, characterDataOldValue: true });
    bootMo.observe(root, { childList: true, subtree: true });
    return true;
  }

  // Track #transcript and .transcript-content appearing (React mounts late).
  const bootMo = new MutationObserver(() => { attachScroll(); attachResize(); });
  if (!attachObservers()) {
    document.addEventListener('DOMContentLoaded', attachObservers, { once: true });
  }

  function frame() {
    push('frame', {});
    requestAnimationFrame(frame);
  }
  requestAnimationFrame(frame);

  function boot() { attachScroll(); attachResize(); }
  if (document.body) boot();
  else document.addEventListener('DOMContentLoaded', boot);

  window.__scrollProbe = {
    markPhase(name) { trace.push({ t: performance.now(), kind: 'phase', name }); },
    scrollAway(offset) {
      const el = document.getElementById('transcript');
      if (!el) return null;
      el.scrollTop = Math.max(0, el.scrollHeight - el.clientHeight - offset);
      // Dispatch the scroll event synchronously so React's onScroll flips the
      // pin state deterministically (no race with the next WS delta).
      el.dispatchEvent(new Event('scroll'));
      trace.push({ t: performance.now(), kind: 'scroll', ev: 'test-scroll-away' });
      return { st: el.scrollTop, rem: el.scrollHeight - el.scrollTop - el.clientHeight };
    },
    scrollBottom() {
      const el = document.getElementById('transcript');
      if (!el) return null;
      el.scrollTop = el.scrollHeight;
      el.dispatchEvent(new Event('scroll'));
      trace.push({ t: performance.now(), kind: 'scroll', ev: 'test-scroll-bottom' });
      return el.scrollHeight - el.scrollTop - el.clientHeight;
    },
    setDraft(text) {
      // React-controlled textarea: use the native setter + input event so
      // autoResize grows the composer (footer height change, no item change).
      const input = document.getElementById('prompt-input');
      if (!input) return null;
      const setter = Object.getOwnPropertyDescriptor(window.HTMLTextAreaElement.prototype, 'value').set;
      setter.call(input, text);
      input.dispatchEvent(new Event('input', { bubbles: true }));
      return input.value.length;
    },
    imageState() {
      return [...document.querySelectorAll('img.md-image')].map((img) => ({
        src: (img.src || '').slice(0, 40),
        complete: img.complete,
        w: img.naturalWidth,
        h: img.naturalHeight,
      }));
    },
    metrics,
    dump() { return trace.slice(); },
  };
})();
`;

const DOCUMENTED_IDS = [
  'S0.1', 'S0.2', 'S0.3',
  'S1.1',
  'S2.1', 'S2.2', 'S2.3',
  'S3.1', 'S3.2', 'S3.3',
  'S4.1',
  'S5.1', 'S5.2',
  'S6.1', 'S6.2', 'S6.3',
  'S7.1', 'S7.2', 'S7.3',
];
const executed = new Set();
const assertionsLog = [];

function record(id) {
  executed.add(id);
  assertionsLog.push({ id, ok: true });
  console.log(`[web-scroll:assert] ${id}`);
}

function fail(message) {
  // Throw (not process.exit): the caller's try/finally must still run so the
  // failure evidence (metrics trace + screenshot) is written before main()
  // exits 2. Exit 2 (not 1): 1 means "playwright setup failure
  // (node/chromium/npm)", which must NOT be confused with an assertion
  // failure.
  const err = new Error(message);
  err.scrollFail = true;
  throw err;
}

function waitFor(page, fn, label, timeoutMs = 30000, arg) {
  return page.waitForFunction(fn, arg, { timeout: timeoutMs }).catch(() => {
    fail(`${label} (timeout ${timeoutMs}ms)`);
  });
}

async function transcriptMetrics(page) {
  return page.evaluate(() => {
    const el = document.getElementById('transcript');
    if (!el) return null;
    return {
      scrollTop: el.scrollTop,
      clientHeight: el.clientHeight,
      scrollHeight: el.scrollHeight,
      remaining: el.scrollHeight - el.scrollTop - el.clientHeight,
    };
  });
}

/* ------------------------- trace segmentation ------------------------- */

function segment(trace, name) {
  const start = trace.findIndex((s) => s.kind === 'phase' && s.name === name);
  if (start < 0) return null;
  const end = trace.findIndex((s, i) => i > start && s.kind === 'phase');
  const slice = trace.slice(start + 1, end < 0 ? undefined : end);
  return {
    frames: slice.filter((s) => s.kind === 'frame'),
    events: slice.filter((s) => s.kind === 'mut' || s.kind === 'resize'),
    scrolls: slice.filter((s) => s.kind === 'scroll'),
  };
}

function describeViolation(seg, viol) {
  const prev = [...seg.frames].reverse().find((f) => f.t < viol.s.t);
  const trigger = [...seg.events].reverse().find((e) => e.t <= viol.s.t && viol.s.t - e.t < 600);
  return {
    after: {
      t: +viol.s.t.toFixed(2),
      st: viol.s.st,
      sh: viol.s.sh,
      ch: viol.s.ch,
      rem: viol.s.rem,
    },
    before: prev
      ? { t: +prev.t.toFixed(2), st: prev.st, sh: prev.sh, ch: prev.ch, rem: prev.rem }
      : null,
    trigger: trigger
      ? `${trigger.kind}:${trigger.cls || trigger.ev || ''}${trigger.node ? ' (' + trigger.node + ')' : ''}${trigger.grow ? ' +' + trigger.grow + ' chars' : ''} @${trigger.t.toFixed(2)}s`
      : 'none',
    winY: viol.s.winY,
    docTop: viol.s.docTop,
  };
}

function failViolation(id, message, seg, viol) {
  const d = describeViolation(seg, viol);
  const where = d.after
    ? ` | first jump: before=${JSON.stringify(d.before)} after=${JSON.stringify(d.after)} trigger=${d.trigger} winY=${d.winY} docTop=${d.docTop}`
    : '';
  fail(`${id}: ${message}${where}`);
}

/* ------------------------- segment assertions ------------------------- */

// Pinned contract: every painted-frame sample bounded + window/doc scroll at
// 0. ResizeObserver can correct post-rAF image growth before paint; the probe's
// rAF may capture that pre-correction geometry once. Accept exactly one such
// sample only when the immediately following frame is bounded again.
function pinnedViolation(seg) {
  for (let index = 0; index < seg.frames.length; index += 1) {
    const s = seg.frames[index];
    if (s.rem > PIN_MAX_REMAINING) {
      const next = seg.frames[index + 1];
      if (!next || next.rem > PIN_MAX_REMAINING) {
        return { s, what: `pinned remaining ${s.rem}px > ${PIN_MAX_REMAINING}px (not recovered by next frame)` };
      }
    }
    if (s.winY !== 0 || s.docTop !== 0) return { s, what: `window/document scrolled (winY=${s.winY} docTop=${s.docTop})` };
  }
  for (const s of seg.scrolls) {
    if (s.rem > PIN_MAX_REMAINING) return { s, what: `scroll event left remaining ${s.rem}px > ${PIN_MAX_REMAINING}px (anchoring interference?)` };
  }
  return null;
}

// Unpinned contract: scrollTop EXACTLY frozen on every frame and every
// scroll event; window/document scroll at 0.
function frozenViolation(seg, frozenSt) {
  for (const s of seg.frames) {
    if (s.st !== frozenSt) return { s, what: `viewport moved while unpinned (scrollTop ${frozenSt} -> ${s.st})` };
    if (s.winY !== 0 || s.docTop !== 0) return { s, what: `window/document scrolled (winY=${s.winY} docTop=${s.docTop})` };
  }
  for (const s of seg.scrolls) {
    if (s.st !== frozenSt) return { s, what: `scroll event moved viewport while unpinned (scrollTop ${frozenSt} -> ${s.st})` };
  }
  return null;
}

// Direct DOM delta sync: every text-delta growth while pinned must leave the
// next frame bounded AND advance scrollTop by the growth (minus tolerance).
function deltaSyncViolations(seg) {
  const frames = seg.frames;
  const out = [];
  for (const ev of seg.events) {
    if (ev.cls !== 'text-delta') continue;
    const next = frames.find((f) => f.t >= ev.t);
    const prev = [...frames].reverse().find((f) => f.t < ev.t);
    if (!next || !prev) continue;
    if (next.rem > PIN_MAX_REMAINING) {
      out.push({ ev, next, prev, why: `remaining ${next.rem}px after text-delta (+${ev.grow} chars)` });
    } else if (next.sh > prev.sh) {
      const followed = next.st - prev.st;
      const grown = next.sh - prev.sh;
      if (followed < grown - 2 * PIN_MAX_REMAINING) {
        out.push({ ev, next, prev, why: `scrollTop advanced ${followed}px but content grew ${grown}px` });
      }
    }
  }
  return out;
}

function summaryOf(seg) {
  const frames = seg.frames;
  const events = seg.events;
  const hs = frames.map((f) => f.headerH).filter((v) => v >= 0);
  const fs = frames.map((f) => f.footerH).filter((v) => v >= 0);
  const cs = frames.map((f) => f.ch).filter((v) => v > 0);
  const nums = (a) => (a.length ? { min: Math.min(...a), max: Math.max(...a) } : null);
  return {
    frames: frames.length,
    remaining: nums(frames.map((f) => f.rem)),
    scrollTop: nums(frames.map((f) => f.st)),
    scrollHeight: nums(frames.map((f) => f.sh)),
    clientHeight: nums(cs),
    headerH: nums(hs),
    footerH: nums(fs),
    maxWinY: frames.reduce((m, f) => Math.max(m, f.winY), 0),
    maxDocTop: frames.reduce((m, f) => Math.max(m, f.docTop), 0),
    badgeShown: frames.some((f) => f.badge),
    badgeHidden: frames.some((f) => !f.badge),
    textDeltaEvents: events.filter((e) => e.cls === 'text-delta').length,
    commitEvents: events.filter((e) => e.cls === 'commit').length,
    mermaidEvents: events.filter((e) => e.cls === 'mermaid').length,
    resizeEvents: events.filter((e) => e.kind === 'resize').length,
  };
}

/* ------------------------------- main --------------------------------- */

async function main() {
  if (!url) fail('RPI_URL is required');
  const launchOptions = chromePath ? { executablePath: chromePath } : {};
  const browser = await chromium.launch(launchOptions);
  const traceHandle = { value: null }; // filled on dump for metrics/failure output
  let didPass = false;
  try {
    // A short viewport (600px) makes the streamed transcript overflow early,
    // so the pin assertions exercise real scrolling, not a content-fit no-op.
    const page = await browser.newPage({ viewport: { width: 1280, height: 600 } });
    if (token) {
      await page.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
    }
    await page.addInitScript(PROBE_SCRIPT);
    page.on('pageerror', (err) => {
      console.error(`web-scroll: page error: ${err.message}`);
    });

    const probeDump = () => page.evaluate(() => window.__scrollProbe.dump());
    const markPhase = (name) => page.evaluate((n) => window.__scrollProbe.markPhase(n), name);
    const probeScrollAway = (offset) => page.evaluate((o) => window.__scrollProbe.scrollAway(o), offset);
    const probeScrollBottom = () => page.evaluate(() => window.__scrollProbe.scrollBottom());
    const probeSetDraft = (text) => page.evaluate((t) => window.__scrollProbe.setDraft(t), text);
    const probeImageState = () => page.evaluate(() => window.__scrollProbe.imageState());

    /* ---------------- S0: boot + connect ---------------- */
    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'S0.1: page title missing');
    await waitFor(page, () => document.getElementById('conn-state') !== null, 'S0.1: conn-state missing');
    await waitFor(
      page,
      () => document.getElementById('conn-state').dataset.state === 'on',
      'S0.2: WS did not reach "connected"'
    );
    record('S0.1');
    record('S0.2');
    const sessionA = await page.evaluate(() => {
      const rows = [...document.querySelectorAll('.session-sidebar__switch')];
      return rows.length ? rows[0].dataset.sessionId || '' : '';
    });
    if (!sessionA) fail('S0.3: primary session row never appeared in the sidebar');
    record('S0.3');
    await page.screenshot({ path: `${evidence}/scroll-s0-boot.png`, fullPage: true });

    /* ---------------- S1: long stream starts pinned ---------------- */
    await page.fill('#prompt-input', 'scroll-long-a');
    await markPhase('S1-first-delta');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      () => document.body.textContent.includes('scroll-long-a-00'),
      'S1.1: long stream never started (first delta missing)',
      30000
    );
    const boot = await transcriptMetrics(page);
    if (!boot) fail('S1.1: transcript metrics unavailable');
    if (boot.remaining > PIN_MAX_REMAINING) {
      fail(`S1.1: transcript not pinned to the bottom at stream start (remaining ${boot.remaining}px > ${PIN_MAX_REMAINING}px)`);
    }
    record('S1.1');

    /* ---------------- S2: pinned long stream, continuous sampling ---------------- */
    await markPhase('S2-pinned');
    await waitFor(
      page,
      () => document.body.textContent.includes('scroll-long-a-12'),
      'S2: stream stalled before chunk 12',
      30000
    );
    await page.screenshot({ path: `${evidence}/scroll-s2-pinned.png`, fullPage: true });

    const trace2 = await probeDump();
    const seg2 = segment(trace2, 'S2-pinned');
    if (!seg2) fail('S2: trace segment "S2-pinned" missing');
    let viol = pinnedViolation(seg2);
    if (viol) failViolation('S2.1', `pinned stream violated: ${viol.what}`, seg2, viol);
    record('S2.1');
    const syncViolations = deltaSyncViolations(seg2);
    if (syncViolations.length > 0) {
      const v = syncViolations[0];
      fail(`S2.2: direct DOM delta did not advance scrollTop synchronously (${v.why}; delta +${v.ev.grow} chars; frame before st=${v.prev.st}/sh=${v.prev.sh} rem=${v.prev.rem} -> after st=${v.next.st}/sh=${v.next.sh} rem=${v.next.rem})`);
    }
    record('S2.2');
    const scrollViol = seg2.scrolls.find((s) => s.rem > PIN_MAX_REMAINING);
    if (scrollViol) {
      failViolation('S2.3', `scroll event while pinned did not land at the bottom: ${scrollViol.rem}px remaining (browser scroll-anchoring interference?)`, seg2, { s: scrollViol });
    }
    record('S2.3');

    /* ---------------- S3: scroll away unpins; deltas freeze the viewport ---------------- */
    // The transition (scroll-away + first post-unpin frames) lives in its own
    // gap segment so the pinned (S2) and unpinned (S3-unpinned) windows each
    // contain only frames in their own pin state.
    await markPhase('S3-gap');
    await probeScrollAway(SCROLL_AWAY_OFFSET);
    await waitFor(
      page,
      (offset) => {
        const el = document.getElementById('transcript');
        return el.scrollHeight - el.scrollTop - el.clientHeight > offset;
      },
      'S3.1: scroll away never unpinned the transcript',
      15000,
      SCROLL_AWAY_OFFSET
    );
    record('S3.1');
    await markPhase('S3-unpinned');
    const frozen = await transcriptMetrics(page);
    if (!frozen) fail('S3.2: transcript metrics unavailable');

    await waitFor(
      page,
      () => document.body.textContent.includes('scroll-long-a-16'),
      'S3.2: stream stalled after scroll-up (chunk 16)',
      30000
    );
    await waitFor(
      page,
      () => document.body.textContent.includes('scroll-long-a-20'),
      'S3.2: stream stalled after scroll-up (chunk 20)',
      30000
    );
    const after = await transcriptMetrics(page);
    if (!after) fail('S3.2: transcript metrics unavailable');
    if (after.scrollHeight <= frozen.scrollHeight + 50) {
      fail(`S3.3: transcript did not keep growing while unpinned (scrollHeight ${frozen.scrollHeight} -> ${after.scrollHeight})`);
    }
    record('S3.3');
    const trace3 = await probeDump();
    const seg3 = segment(trace3, 'S3-unpinned');
    if (!seg3) fail('S3: trace segment "S3-unpinned" missing');
    viol = frozenViolation(seg3, frozen.scrollTop);
    if (viol) failViolation('S3.2', `unpinned viewport not frozen: ${viol.what}`, seg3, viol);
    record('S3.2');
    await page.screenshot({ path: `${evidence}/scroll-s3-unpinned.png`, fullPage: true });

    /* ---------------- S4: returning to the bottom re-pins ---------------- */
    // The re-pin transition lives in its own gap segment: the S3-unpinned
    // window must end before the programmatic scroll-to-bottom, and the
    // S4-repinned window must start only after the pin has landed.
    await markPhase('S4-gap');
    await probeScrollBottom();
    await waitFor(
      page,
      (limit) => {
        const el = document.getElementById('transcript');
        return el.scrollHeight - el.scrollTop - el.clientHeight <= limit;
      },
      'S4.1: return to the bottom never re-pinned the transcript',
      15000,
      PIN_MAX_REMAINING
    );
    await markPhase('S4-repinned');
    await waitFor(
      page,
      () => document.body.textContent.includes('scroll-long-a-24'),
      'S4.1: stream stalled after re-pin (chunk 24)',
      30000
    );
    const repinned = await transcriptMetrics(page);
    if (!repinned) fail('S4.1: transcript metrics unavailable');
    if (repinned.remaining > PIN_MAX_REMAINING) {
      fail(`S4.1: stream jumped away after re-pin (remaining ${repinned.remaining}px > ${PIN_MAX_REMAINING}px)`);
    }
    if (repinned.scrollTop <= after.scrollTop) {
      fail(`S4.1: re-pin never advanced the view (scrollTop ${after.scrollTop} -> ${repinned.scrollTop})`);
    }
    const trace4 = await probeDump();
    const seg4 = segment(trace4, 'S4-repinned');
    if (!seg4) fail('S4: trace segment "S4-repinned" missing');
    viol = pinnedViolation(seg4);
    if (viol) failViolation('S4.1', `re-pinned stream violated: ${viol.what}`, seg4, viol);
    record('S4.1');
    await page.screenshot({ path: `${evidence}/scroll-s4-repinned.png`, fullPage: true });

    /* ---------------- S5: streaming -> final rendered markdown ---------------- */
    // Let the long stream finish and its message_end commit land so the
    // final-md phase is a clean single-turn transition.
    await waitFor(
      page,
      () => document.body.textContent.includes('scroll-long-a-done'),
      'S5: long stream never completed (marker missing)',
      30000
    );
    await markPhase('S5-final-md');
    await page.fill('#prompt-input', 'scroll-final-md');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      () => document.body.textContent.includes('scroll-final-md-done'),
      'S5.1: final-md stream never delivered (marker missing)',
      30000
    );
    // React commit: the final render with the tall code fence + mermaid host.
    await waitFor(
      page,
      () => document.querySelector('#transcript .md-fence') !== null,
      'S5.1: final markdown render never committed (.md-fence missing)',
      30000
    );
    // Async hydration: mermaid SVG replaces the host div (no React commit).
    await waitFor(
      page,
      () => document.querySelector('.md-mermaid-host--rendered svg') !== null,
      'S5.2: mermaid never hydrated (.md-mermaid-host--rendered svg missing)',
      30000
    );
    // Let the async layout settle (image decode included) before closing the
    // phase, so the bounded-remaining assertion spans the whole transition.
    await page.waitForTimeout(800);
    const imageState = await probeImageState();
    await page.screenshot({ path: `${evidence}/scroll-s5-final-md.png`, fullPage: true });

    const trace5 = await probeDump();
    const seg5 = segment(trace5, 'S5-final-md');
    if (!seg5) fail('S5: trace segment "S5-final-md" missing');
    const s5 = summaryOf(seg5);
    if (s5.commitEvents < 1) fail('S5.1: no react-commit mutation observed during final-md phase');
    viol = pinnedViolation(seg5);
    if (viol) failViolation('S5.1', `streaming->final markdown broke the pin: ${viol.what}`, seg5, viol);
    record('S5.1');
    if (s5.mermaidEvents < 1 && s5.resizeEvents < 1) {
      fail('S5.2: no async growth (mermaid svg swap / resize) observed after the commit');
    }
    // The view must have followed the async growth: net scrollTop advance
    // tracks the net scrollHeight growth within the tolerance.
    const f5 = seg5.frames;
    if (f5.length >= 2) {
      const first = f5[0];
      const last = f5[f5.length - 1];
      const stAdvance = last.st - first.st;
      const shGrowth = last.sh - first.sh;
      if (shGrowth > 2 * PIN_MAX_REMAINING && stAdvance < shGrowth - 2 * PIN_MAX_REMAINING) {
        fail(`S5.2: async growth not followed by the view (scrollHeight +${shGrowth}px, scrollTop +${stAdvance}px over the final-md window)`);
      }
      if (last.rem > PIN_MAX_REMAINING) {
        fail(`S5.2: final state drifted (remaining ${last.rem}px > ${PIN_MAX_REMAINING}px)`);
      }
    }
    record('S5.2');

    /* ---------------- S6: narrow/mobile viewport phase ---------------- */
    await page.setViewportSize({ width: 480, height: 760 });
    await page.waitForTimeout(500); // let the narrow layout settle
    await markPhase('S6-narrow-pinned');
    await page.fill('#prompt-input', 'scroll-narrow');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      () => document.body.textContent.includes('scroll-narrow-00'),
      'S6.1: narrow stream never started (first delta missing)',
      30000
    );
    // Deterministic no-item-change layout event: grow the composer draft so
    // autoResize pushes the footer down while the stream is pinned.
    await probeSetDraft(NARROW_DRAFT);
    await page.waitForTimeout(400);
    await probeSetDraft('');
    await waitFor(
      page,
      () => document.body.textContent.includes('scroll-narrow-done'),
      'S6.1: narrow stream never completed (marker missing)',
      30000
    );
    const trace6a = await probeDump();
    const seg6a = segment(trace6a, 'S6-narrow-pinned');
    if (!seg6a) fail('S6: trace segment "S6-narrow-pinned" missing');
    const s6a = summaryOf(seg6a);
    // Non-vacuous: the layout really changed (badge/abort toggles + composer
    // growth) and the transcript viewport resized with it.
    const layoutSpan = (a) => (a ? a.max - a.min : 0);
    const chromeDelta = layoutSpan(s6a.headerH) + layoutSpan(s6a.footerH);
    if (chromeDelta < 2) {
      fail(`S6.1: no measurable header/footer layout change (headerH ${JSON.stringify(s6a.headerH)}, footerH ${JSON.stringify(s6a.footerH)}) — cannot prove the pin survives chrome changes`);
    }
    if (layoutSpan(s6a.clientHeight) < 2) {
      fail(`S6.1: transcript clientHeight never changed (${JSON.stringify(s6a.clientHeight)}) — narrow phase vacuous`);
    }
    if (!s6a.badgeShown || !s6a.badgeHidden) {
      fail('S6.1: streaming badge never toggled during the narrow stream');
    }
    viol = pinnedViolation(seg6a);
    if (viol) failViolation('S6.1', `narrow pinned stream violated across layout changes: ${viol.what}`, seg6a, viol);
    record('S6.1');
    await page.screenshot({ path: `${evidence}/scroll-s6-narrow-pinned.png`, fullPage: true });

    // Unpinned narrow stream: viewport must stay EXACTLY frozen while the
    // same layout changes happen and the transcript keeps growing.
    await markPhase('S6-gap');
    await probeScrollAway(SCROLL_AWAY_OFFSET);
    await waitFor(
      page,
      (offset) => {
        const el = document.getElementById('transcript');
        return el.scrollHeight - el.scrollTop - el.clientHeight >= offset;
      },
      'S6.2: narrow scroll away never unpinned the transcript',
      15000,
      SCROLL_AWAY_OFFSET
    );
    await markPhase('S6-narrow-unpinned');
    const frozenNarrow = await transcriptMetrics(page);
    if (!frozenNarrow) fail('S6.2: transcript metrics unavailable');
    await page.fill('#prompt-input', 'scroll-narrow');
    await page.press('#prompt-input', 'Enter');
    await probeSetDraft(NARROW_DRAFT);
    await page.waitForTimeout(400);
    await probeSetDraft('');
    await waitFor(
      page,
      () => document.body.textContent.includes('scroll-narrow-done'),
      'S6.2: second narrow stream never completed (marker missing)',
      30000
    );
    const afterNarrow = await transcriptMetrics(page);
    if (!afterNarrow) fail('S6.2: transcript metrics unavailable');
    if (afterNarrow.scrollHeight <= frozenNarrow.scrollHeight + 50) {
      fail(`S6.3: narrow transcript did not keep growing while unpinned (scrollHeight ${frozenNarrow.scrollHeight} -> ${afterNarrow.scrollHeight})`);
    }
    record('S6.3');
    const trace6b = await probeDump();
    const seg6b = segment(trace6b, 'S6-narrow-unpinned');
    if (!seg6b) fail('S6: trace segment "S6-narrow-unpinned" missing');
    viol = frozenViolation(seg6b, frozenNarrow.scrollTop);
    if (viol) failViolation('S6.2', `narrow unpinned viewport not frozen: ${viol.what}`, seg6b, viol);
    record('S6.2');
    await page.screenshot({ path: `${evidence}/scroll-s6-narrow-unpinned.png`, fullPage: true });

    /* ---------------- S7: session switch force-pins the activated session ---------------- */
    // Gap segment: resize back to the desktop layout, settle, and unpin A.
    await markPhase('S7-gap');
    await page.setViewportSize({ width: 1280, height: 600 });
    await page.waitForTimeout(500);
    // Unpin session A again so the switch-back must OVERRIDE a stale
    // unpinned state (nearBottom=false) instead of inheriting it.
    await probeScrollAway(SCROLL_AWAY_OFFSET);
    await waitFor(
      page,
      (offset) => {
        const el = document.getElementById('transcript');
        return el.scrollHeight - el.scrollTop - el.clientHeight >= offset;
      },
      'S7.1: pre-switch unpin never registered',
      15000,
      SCROLL_AWAY_OFFSET
    );
    const preSwitch = await transcriptMetrics(page);
    if (!preSwitch) fail('S7.1: transcript metrics unavailable');
    if (preSwitch.remaining <= PIN_MAX_REMAINING) {
      fail(`S7.1: session A not unpinned before the switch (remaining ${preSwitch.remaining}px)`);
    }
    record('S7.1');
    await markPhase('S7-switch');

    await page.click('#sidebar-new-session-btn');
    await waitFor(
      page,
      () => document.querySelector('#transcript .empty-hint') !== null,
      'S7.2: new session did not show the empty view'
    );
    await page.fill('#prompt-input', 'scroll-echo');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      () => document.body.textContent.includes('scroll-echo-reply'),
      'S7.2: new session echo never round-tripped',
      30000
    );
    record('S7.2');
    await page.screenshot({ path: `${evidence}/scroll-s7-new-session.png`, fullPage: true });

    // Switch back to A: its long transcript must be pinned to the bottom
    // even though A was unpinned before the switch (forcePin on activation).
    await waitFor(
      page,
      (s) => {
        const rows = [...document.querySelectorAll('.session-sidebar__switch')];
        const row = rows.find((candidate) => candidate.dataset.sessionId === s);
        return row !== undefined && !row.disabled;
      },
      'S7.3: session A row never became clickable',
      30000,
      sessionA
    );
    await page.evaluate((s) => {
      const rows = [...document.querySelectorAll('.session-sidebar__switch')];
      const row = rows.find((candidate) => candidate.dataset.sessionId === s);
      row.click();
    }, sessionA);
    await waitFor(
      page,
      (s) => {
        const row = [...document.querySelectorAll('.session-sidebar__row')]
          .find((candidate) => candidate.querySelector('.session-sidebar__switch')?.dataset.sessionId === s);
        return row?.classList.contains('session-sidebar__row--active') === true;
      },
      'S7.3: session A never became active again',
      30000,
      sessionA
    );
    await waitFor(
      page,
      () => document.getElementById('transcript').textContent.includes('scroll-long-a-00'),
      'S7.3: session A transcript never restored',
      30000
    );
    // forcePin lands via a passive effect (a frame or two after the commit);
    // wait for the pin, then gate every frame of the settled window.
    await waitFor(
      page,
      (limit) => {
        const el = document.getElementById('transcript');
        return el.scrollHeight - el.scrollTop - el.clientHeight <= limit;
      },
      'S7.3: session switch did not pin the activated transcript to the bottom',
      15000,
      PIN_MAX_REMAINING
    );
    await markPhase('S7-switch-back');
    await page.waitForTimeout(600);
    const restored = await transcriptMetrics(page);
    if (!restored) fail('S7.3: transcript metrics unavailable');
    if (restored.remaining > PIN_MAX_REMAINING) {
      fail(`S7.3: activated transcript resumed mid-transcript (remaining ${restored.remaining}px > ${PIN_MAX_REMAINING}px)`);
    }
    const trace7 = await probeDump();
    const seg7 = segment(trace7, 'S7-switch-back');
    if (!seg7) fail('S7: trace segment "S7-switch-back" missing');
    viol = pinnedViolation(seg7);
    if (viol) failViolation('S7.3', `activated session not force-pinned: ${viol.what}`, seg7, viol);
    // Prove the pre-switch state was genuinely unpinned (the force-pin had
    // to OVERRIDE it — old logic inherits the stale false and fails here).
    const seg7pre = segment(trace7, 'S7-switch');
    if (!seg7pre || !seg7pre.frames.some((f) => f.rem >= SCROLL_AWAY_OFFSET)) {
      fail('S7.3: pre-switch unpinned state not visible in the trace — force-pin test invalid');
    }
    record('S7.3');
    await page.screenshot({ path: `${evidence}/scroll-s7-switched-back.png`, fullPage: true });

    /* ---------------- evidence + metrics ---------------- */
    const missing = DOCUMENTED_IDS.filter((id) => !executed.has(id));
    if (missing.length > 0) {
      fail(`documented assertions never executed: ${missing.join(', ')}`);
    }
    const trace = await probeDump();
    traceHandle.value = trace;
    const phases = {};
    for (const name of ['S1-first-delta', 'S2-pinned', 'S3-unpinned', 'S4-repinned', 'S5-final-md', 'S6-narrow-pinned', 'S6-narrow-unpinned', 'S7-switch', 'S7-switch-back']) {
      const seg = segment(trace, name);
      if (seg) phases[name] = summaryOf(seg);
    }
    const metrics = {
      lane: 'web-scroll',
      result: 'PASSED',
      finishedAt: new Date().toISOString(),
      constants: { PIN_MAX_REMAINING, SCROLL_AWAY_OFFSET },
      phases,
      assertions: assertionsLog,
      finalImageState: imageState,
      trace,
    };
    fs.writeFileSync(`${evidence}/scroll-metrics.json`, JSON.stringify(metrics));
    fs.writeFileSync(
      `${evidence}/coverage-assertions.json`,
      JSON.stringify({ executed: [...executed].sort() }, null, 2)
    );
    didPass = true;
    console.log('web-scroll: PASSED (per-frame bounded pin, synchronous delta advance, exact unpinned freeze, re-pin on return, async final-markdown/mermaid/image pin, narrow-layout pin, activated-session force-pin)');
  } finally {
    // On failure, still write the metrics + a screenshot so the failure is
    // diagnosable (first jump before/after + triggering event).
    if (!didPass) {
      try {
        const trace = traceHandle.value || (await browser.contexts()[0]?.pages()[0]?.evaluate(() => window.__scrollProbe && window.__scrollProbe.dump()) || null);
        if (trace) {
          traceHandle.value = trace;
          fs.writeFileSync(`${evidence}/scroll-metrics.json`, JSON.stringify({
            lane: 'web-scroll',
            result: 'FAILED',
            finishedAt: new Date().toISOString(),
            constants: { PIN_MAX_REMAINING, SCROLL_AWAY_OFFSET },
            assertions: assertionsLog,
            trace,
          }));
        }
        const pg = browser.contexts()[0]?.pages()[0];
        if (pg) await pg.screenshot({ path: `${evidence}/scroll-failure.png`, fullPage: true }).catch(() => {});
      } catch {
        // best-effort evidence only
      }
    }

    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-scroll: FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});
