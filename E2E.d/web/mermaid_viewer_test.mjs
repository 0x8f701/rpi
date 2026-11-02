// Mermaid viewer Web E2E lane (playwright half of E2E.d/web/mermaid_viewer.sh).
//
// Environment:
//   RPI_URL        http://127.0.0.1:<port>/web
//   RPI_TOKEN      token file content (served via rpi-auth.<token> subprotocol)
//   RPI_CHROME     executable path of the system Chrome (optional)
//   RPI_EVIDENCE   evidence dir for screenshots
//
// Drives the REAL web bundle against the steering mock ("render rich content"
// -> valid mermaid flowchart; "render markdown extra branches" -> invalid
// mermaid sequenceDiagram that must degrade to the error host) and asserts
// the mermaidViewer.ts contract end-to-end (see mermaid_viewer.sh header).
//
// Exit: 0 = every assertion passed; 2 = assertion failure; the sh lane maps
// 1 to setup failure (node/chromium/npm), so this script never exits 1.

import { chromium } from 'playwright';
import fs from 'node:fs';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';

const HOST = '#transcript .md-mermaid-host--rendered';
const HOST_SVG = `${HOST} .md-mermaid-viewport svg`;
const VIEWER = '.mermaid-viewer';

function fail(message) {
  console.error(`web-mermaid-viewer: FAIL: ${message}`);
  process.exit(2);
}

async function waitFor(page, fn, label, timeoutMs = 25000, arg) {
  try {
    await page.waitForFunction(fn, arg, { timeout: timeoutMs });
  } catch {
    fail(`${label} (timeout ${timeoutMs}ms)`);
  }
}

// Zero console/page errors is a hard lane contract (both pages).
const consoleProblems = [];
function attachPage(page, tag) {
  page.on('console', (m) => {
    if (m.type() === 'error') consoleProblems.push(`[${tag}] console.error: ${m.text()}`);
  });
  page.on('pageerror', (err) => {
    consoleProblems.push(`[${tag}] pageerror: ${err.message}`);
  });
}

async function waitForIdle(page, label, timeoutMs = 30000) {
  await waitFor(
    page,
    // Production streaming gate (App.tsx): #send-btn flips to the Stop
    // action while a run is in flight. Missing element fails the gate.
    () => document.getElementById('send-btn')?.getAttribute('aria-label') === 'Send message',
    label,
    timeoutMs
  );
}

async function sendPrompt(page, prompt, label) {
  await waitForIdle(page, `${label}: session not idle before prompt`);
  const before = await page.evaluate(
    () => document.querySelectorAll('.msg--assistant .assistant-text').length
  );
  await page.fill('#prompt-input', prompt);
  await page.press('#prompt-input', 'Enter');
  await waitFor(
    page,
    (b) =>
      document.querySelectorAll('.msg--assistant .assistant-text').length > b &&
      document.getElementById('send-btn')?.getAttribute('aria-label') === 'Send message',
    `${label}: prompt never produced a reply`,
    45000,
    before
  );
}

async function awaitRenderedHost(page, label) {
  await waitFor(
    page,
    (hostSel) => document.querySelector(`${hostSel} svg`) !== null,
    `${label}: mermaid flowchart SVG never rendered`,
    45000,
    HOST
  );
}

// Mobile reloads into the persisted session; history may already carry the
// rendered host. Only prompt when it is missing (keeps the lane deterministic
// either way).
async function ensureRenderedHost(page, label) {
  try {
    await page.waitForFunction(
      (hostSel) => document.querySelector(`${hostSel} svg`) !== null,
      HOST,
      { timeout: 8000 }
    );
  } catch {
    await sendPrompt(page, 'render rich content', `${label}: mobile re-prompt`);
    await awaitRenderedHost(page, label);
  }
}

async function viewerOpen(page, label) {
  await waitFor(
    page,
    (viewerSel) => {
      const root = document.querySelector(viewerSel);
      return root !== null && root.hidden === false;
    },
    `${label}: viewer never opened`,
    25000,
    VIEWER
  );
}

/** Live viewer geometry read in-page (null while closed). */
function metricsFn() {
  const root = document.querySelector('.mermaid-viewer');
  if (!root || root.hidden) return null;
  const stage = root.querySelector('.mermaid-viewer__stage');
  const canvas = root.querySelector('.mermaid-viewer__canvas');
  const svg = canvas && canvas.querySelector('svg');
  if (!stage || !canvas || !svg) return null;
  const computed = getComputedStyle(canvas).transform;
  let m = null;
  try {
    m = new DOMMatrixReadOnly(computed === 'none' ? '' : computed);
  } catch {
    m = null;
  }
  if (!m) return null;
  const vb = (svg.getAttribute('viewBox') || '').trim().split(/[\s,]+/).map(Number);
  return {
    scale: m.a,
    tx: m.e,
    ty: m.f,
    stage: stage.getBoundingClientRect().toJSON(),
    svgBox: svg.getBoundingClientRect().toJSON(),
    vbW: vb.length === 4 && Number.isFinite(vb[2]) ? vb[2] : 0,
    vbH: vb.length === 4 && Number.isFinite(vb[3]) ? vb[3] : 0,
  };
}

/** Expected Fit transform for the metrics captured above (viewer default). */
function fitOf(m) {
  const fit = Math.min(40, Math.max(0.05, Math.min(m.stage.width / m.vbW, m.stage.height / m.vbH)));
  return {
    fit,
    tx: (m.stage.width - m.vbW * fit) / 2,
    ty: (m.stage.height - m.vbH * fit) / 2,
  };
}

// Predicate factory for waitForFunction: builds a unary page-side function
// whose body inlines the metricsFn source (no Node closure survives
// serialization) and evaluates `expr` against the live metrics `m` and the
// single serialized arg `e`. Playwright supplies exactly one arg to
// waitForFunction, so every metric predicate goes through this.
function metricsPredicate(expr) {
  return new Function(
    'e',
    `const m = (${metricsFn})(); return !!m && (${expr});`
  );
}

// CSS active-content scan for style attributes and <style> elements: no
// @import, and url() may only reference local (#...) fragments.
function assertNoActiveCss(css, label, where) {
  if (/@import\b/i.test(css)) {
    fail(`${label}: ${where} carries a CSS @import`);
  }
  for (const m of css.matchAll(/url\(\s*(['"]?)([^'")]*)\1\s*\)/gi)) {
    const target = m[2].trim();
    if (!target.startsWith('#')) {
      fail(`${label}: ${where} carries a non-local url() reference (${target.slice(0, 80)})`);
    }
  }
}

// Sanitization scan over an SVG markup string (downloaded file or live DOM).
function assertSanitizedSvg(svg, label) {
  if (!/<svg[\s>]/i.test(svg)) fail(`${label}: download is not an SVG document`);
  for (const [pattern, what] of [
    [/<script[\s>]/i, 'a <script> element'],
    [/<foreignobject[\s>]/i, 'a <foreignObject> element'],
    [/<(iframe|object|embed)[\s>]/i, 'an embed element'],
  ]) {
    if (pattern.test(svg)) fail(`${label}: downloaded SVG contains ${what}`);
  }
  const tags = svg.match(/<[a-zA-Z][^>]*>/g) || [];
  for (const tag of tags) {
    if (/\son[a-z]+\s*=\s*["']?/i.test(tag)) {
      fail(`${label}: downloaded SVG carries an on* event-handler attribute: ${tag.slice(0, 120)}`);
    }
    const urls = tag.matchAll(/\s(href|xlink:href|src|data)\s*=\s*["']([^"']*)["']/gi);
    for (const [, , value] of urls) {
      if (!value.trim().startsWith('#')) {
        fail(`${label}: downloaded SVG carries a non-local URL attribute (${value.slice(0, 80)})`);
      }
    }
  }
  // Serialized markup entity-escapes quotes inside attributes (outerHTML and
  // downloaded SVG alike); decode them so url("#local") style values are
  // scanned as written, not mangled into false non-local hits.
  const decodeCss = (css) =>
    css.replace(/&(quot|#34);/gi, '"').replace(/&(apos|#39);/gi, "'");
  const cssChunks = [];
  for (const m of svg.matchAll(/\sstyle\s*=\s*("([^"]*)"|'([^']*)')/gi)) {
    cssChunks.push([decodeCss(m[2] ?? m[3] ?? ''), 'a style attribute']);
  }
  for (const m of svg.matchAll(/<style[^>]*>([\s\S]*?)<\/style>/gi)) {
    cssChunks.push([decodeCss(m[1]), 'a <style> element']);
  }
  for (const [css, where] of cssChunks) assertNoActiveCss(css, label, where);
}

async function main() {
  if (!url) fail('RPI_URL is required');
  const browser = await chromium.launch(chromePath ? { executablePath: chromePath } : {});
  try {
    // ==================== Desktop 1280x800 ====================
    const desktop = await browser.newContext({
      viewport: { width: 1280, height: 800 },
      acceptDownloads: true,
    });
    await desktop.route('**/images/pic.png', (route) => route.fulfill({ status: 200, contentType: 'image/png', body: Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jRZkAAAAASUVORK5CYII=', 'base64') }));
    const page = await desktop.newPage();
    attachPage(page, 'desktop');
    if (token) {
      await page.addInitScript((t) => {
        window.localStorage.setItem('rpi-web-token', t);
      }, token);
    }
    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'page title missing');
    await waitFor(
      page,
      () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true,
      'WS did not reach "connected"'
    );

    await sendPrompt(page, 'render rich content', 'desktop rich prompt');
    await awaitRenderedHost(page, 'desktop');

    // 1. Host toolbar row: View + Download SVG, visible on the rendered host.
    const hostToolbar = await page.evaluate(() => {
      const host = document.querySelector('#transcript .md-mermaid-host--rendered');
      const buttons = [...host.querySelectorAll('.md-mermaid-toolbar__btn')];
      return {
        hasToolbar: !!host.querySelector('.md-mermaid-toolbar'),
        actions: buttons.map((b) => b.getAttribute('data-mermaid-action')),
        labels: buttons.map((b) => b.textContent.trim()),
        visible: buttons.every((b) => !!(b.offsetWidth || b.offsetHeight)),
      };
    });
    if (!hostToolbar.hasToolbar) fail('rendered mermaid host has no toolbar row');
    if (!(hostToolbar.actions.includes('view') && hostToolbar.actions.includes('download'))) {
      fail(`host toolbar actions wrong: ${JSON.stringify(hostToolbar.actions)}`);
    }
    if (!(hostToolbar.labels.includes('View') && hostToolbar.labels.includes('Download SVG'))) {
      fail(`host toolbar labels wrong: ${JSON.stringify(hostToolbar.labels)}`);
    }
    if (!hostToolbar.visible) fail('host toolbar buttons are not visible');
    await page.screenshot({ path: `${evidence}/mermaid-desktop-inline.png`, fullPage: true });

    // 2. View opens ONE aria-modal dialog (lazy singleton).
    await page.click(`${HOST} .md-mermaid-toolbar__btn[data-mermaid-action="view"]`);
    await viewerOpen(page, 'desktop View click');
    const dialog = await page.evaluate(() => {
      const roots = [...document.querySelectorAll('.mermaid-viewer')];
      const root = roots[0];
      const actions = [...root.querySelectorAll('button[data-viewer-action]')].map((b) =>
        b.getAttribute('data-viewer-action')
      );
      return {
        count: roots.length,
        role: root.getAttribute('role'),
        modal: root.getAttribute('aria-modal'),
        label: root.getAttribute('aria-label'),
        actions,
      };
    });
    if (dialog.count !== 1) fail(`expected exactly one viewer singleton, found ${dialog.count}`);
    if (dialog.role !== 'dialog' || dialog.modal !== 'true') {
      fail(`viewer dialog semantics wrong: role=${dialog.role} aria-modal=${dialog.modal}`);
    }
    if (dialog.label !== 'Mermaid diagram viewer') fail(`viewer aria-label wrong: ${dialog.label}`);
    for (const action of ['zoom-out', 'zoom-in', 'fit', 'zoom-100', 'download', 'close']) {
      if (!dialog.actions.includes(action)) fail(`viewer toolbar missing action: ${action}`);
    }
    // 3. Fit is the default transform (scale = fit ratio, diagram centered,
    //    fully inside the stage).
    let m = await page.evaluate(metricsFn);
    if (!m || m.vbW <= 0 || m.vbH <= 0) fail('viewer metrics unavailable after open');
    const fit = fitOf(m);
    if (Math.abs(m.scale - fit.fit) > 0.02) {
      fail(`viewer default is not Fit: scale=${m.scale.toFixed(4)} expected ${fit.fit.toFixed(4)}`);
    }
    if (Math.abs(m.tx - fit.tx) > 2 || Math.abs(m.ty - fit.ty) > 2) {
      fail(`viewer default is not centered: tx=${m.tx.toFixed(1)} ty=${m.ty.toFixed(1)} expected ${fit.tx.toFixed(1)}/${fit.ty.toFixed(1)}`);
    }
    if (
      m.svgBox.left < m.stage.left - 1 ||
      m.svgBox.top < m.stage.top - 1 ||
      m.svgBox.right > m.stage.right + 1 ||
      m.svgBox.bottom > m.stage.bottom + 1
    ) {
      fail('fit diagram escapes the viewer stage');
    }
    await page.evaluate(() => {
      window.__mvNode = document.querySelector('.mermaid-viewer');
    });
    await page.screenshot({ path: `${evidence}/mermaid-desktop-viewer.png` });

    await page.click(`${VIEWER} button[data-viewer-action="zoom-in"]`);
    await waitFor(
      page,
      metricsPredicate('Math.abs(m.scale - e) < 0.02'),
      'zoom-in never applied',
      10000,
      fit.fit * 1.25
    );
    await page.click(`${VIEWER} button[data-viewer-action="zoom-out"]`);
    await waitFor(
      page,
      metricsPredicate('Math.abs(m.scale - e) < 0.02'),
      'zoom-out never restored Fit',
      10000,
      fit.fit
    );
    await page.click(`${VIEWER} button[data-viewer-action="zoom-100"]`);
    await waitFor(
      page,
      metricsPredicate('Math.abs(m.scale - 1) < 0.001'),
      '100% never reached scale 1',
      10000
    );
    await page.click(`${VIEWER} button[data-viewer-action="fit"]`);
    await waitFor(
      page,
      metricsPredicate('Math.abs(m.scale - e) < 0.02'),
      'Fit never restored',
      10000,
      fit.fit
    );

    // Wheel zoom over the stage (deltaY<0 -> zoom in).
    const stageCenter = await page.evaluate(() => {
      const r = document.querySelector('.mermaid-viewer__stage').getBoundingClientRect();
      return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
    });
    const beforeWheel = await page.evaluate(metricsFn);
    await page.mouse.move(stageCenter.x, stageCenter.y);
    await page.mouse.wheel(0, -240);
    await waitFor(
      page,
      metricsPredicate('m.scale > e.scale * 1.2'),
      'wheel never zoomed in',
      10000,
      beforeWheel
    );
    // Drag pan on the stage (pointer events own pan).
    const beforeDrag = await page.evaluate(metricsFn);
    if (!beforeDrag) fail('viewer metrics missing before drag');
    await page.mouse.move(stageCenter.x, stageCenter.y);
    await page.mouse.down();
    await page.mouse.move(stageCenter.x + 90, stageCenter.y + 50, { steps: 6 });
    await page.mouse.up();
    await waitFor(
      page,
      metricsPredicate(
        'Math.abs(m.tx - (e.tx + 90)) < 2.5 && Math.abs(m.ty - (e.ty + 50)) < 2.5'
      ),
      'drag never panned the diagram',
      10000,
      beforeDrag
    );

    // 4. Escape closes and restores focus to the opener (the View button).
    await page.keyboard.press('Escape');
    await waitFor(
      page,
      () => {
        const root = document.querySelector('.mermaid-viewer');
        return root === null || root.hidden === true;
      },
      'Escape never closed the viewer'
    );
    const focusAfterEscape = await page.evaluate(() => ({
      action: document.activeElement?.getAttribute('data-mermaid-action') || '',
      tag: document.activeElement?.tagName || '',
    }));
    if (focusAfterEscape.action !== 'view') {
      fail(`Escape did not restore focus to the opener View button (active: ${focusAfterEscape.tag}.${focusAfterEscape.action})`);
    }

    // 2b. Double-click on the rendered SVG opens the SAME singleton.
    await page.dblclick(`${HOST} svg`);
    await viewerOpen(page, 'desktop dblclick');
    const sameNode = await page.evaluate(() => document.querySelector('.mermaid-viewer') === window.__mvNode);
    if (!sameNode) fail('dblclick opened a different viewer element (singleton broken)');
    const refit = await page.evaluate(metricsFn);
    if (!refit || Math.abs(refit.scale - fit.fit) > 0.02) {
      fail(`reopened viewer did not reset to Fit: scale=${refit ? refit.scale.toFixed(4) : 'null'}`);
    }

    // Close button also closes and restores focus.
    await page.click(`${VIEWER} button[data-viewer-action="close"]`);
    await waitFor(
      page,
      () => {
        const root = document.querySelector('.mermaid-viewer');
        return root === null || root.hidden === true;
      },
      'Close button never closed the viewer'
    );
    const focusAfterClose = await page.evaluate(
      () => document.activeElement?.closest?.('.mermaid-viewer') === null
        && document.activeElement?.getAttribute('data-mermaid-action') === 'view'
    );
    if (!focusAfterClose) fail('Close did not restore focus to the host View opener outside the hidden viewer');

    // 5. Host toolbar SVG download: filename/MIME/sanitized content.
    await page.evaluate(() => {
      window.__blobInfo = null;
      const orig = URL.createObjectURL.bind(URL);
      URL.createObjectURL = (blob) => {
        try {
          window.__blobInfo = { type: blob && blob.type, size: blob && blob.size };
        } catch {
          /* probe only */
        }
        return orig(blob);
      };
    });
    const [download] = await Promise.all([
      page.waitForEvent('download', { timeout: 15000 }),
      page.click(`${HOST} .md-mermaid-toolbar__btn[data-mermaid-action="download"]`),
    ]);
    if (download.suggestedFilename() !== 'mermaid-diagram.svg') {
      fail(`download filename wrong: ${download.suggestedFilename()}`);
    }
    const blobInfo = await page.evaluate(() => window.__blobInfo);
    if (!blobInfo || blobInfo.type !== 'image/svg+xml;charset=utf-8') {
      fail(`download blob MIME wrong: ${JSON.stringify(blobInfo)}`);
    }
    const downloadPath = await download.path();
    const svgText = await fs.promises.readFile(downloadPath, 'utf8');
    assertSanitizedSvg(svgText, 'host download');
    if (blobInfo.size !== svgText.length) {
      fail(`download blob size ${blobInfo.size} != file bytes ${svgText.length}`);
    }

    // Live host SVG must carry no active content either (strict-mode output).
    const hostSvg = await page.evaluate(
      (hostSel) => document.querySelector(`${hostSel} svg`)?.outerHTML || '',
      HOST
    );
    assertSanitizedSvg(hostSvg, 'host inline svg');

    // 6. Error diagram host: bounded fallback, NO controls, never opens the
    //    viewer (toolbar click route and dblclick route both dead).
    await sendPrompt(page, 'render markdown extra branches', 'desktop error prompt');
    await waitFor(
      page,
      () => document.querySelector('#transcript .md-mermaid-host--error') !== null,
      'invalid mermaid never produced the error host',
      30000
    );
    const errorHost = await page.evaluate(() => {
      const host = document.querySelector('#transcript .md-mermaid-host--error');
      return {
        note: host.querySelector('.md-mermaid-error__note')?.textContent || '',
        source: host.querySelector('.md-mermaid-error__source')?.textContent || '',
        buttons: host.querySelectorAll('button').length,
        toolbars: host.querySelectorAll('.md-mermaid-toolbar').length,
        svgs: host.querySelectorAll('svg').length,
      };
    });
    if (!errorHost.note.includes('Mermaid')) fail(`error host note missing: ${errorHost.note}`);
    if (!errorHost.source.includes('bad token here')) fail('error host lost the diagram source');
    if (errorHost.buttons !== 0 || errorHost.toolbars !== 0 || errorHost.svgs !== 0) {
      fail(`error host carries controls: ${JSON.stringify(errorHost)}`);
    }
    await page.screenshot({ path: `${evidence}/mermaid-desktop-error.png`, fullPage: true });
    await page.dblclick('#transcript .md-mermaid-host--error');
    await page.waitForTimeout(300);
    const viewerAfterError = await page.evaluate(() => {
      const root = document.querySelector('.mermaid-viewer');
      return root === null || root.hidden === true;
    });
    if (!viewerAfterError) fail('error host opened the viewer');

    // ==================== Mobile 390x844 (touch) ====================
    const mobile = await browser.newContext({
      viewport: { width: 390, height: 844 },
      isMobile: true,
      hasTouch: true,
      acceptDownloads: true,
    });
    await mobile.route('**/images/pic.png', (route) => route.fulfill({ status: 200, contentType: 'image/png', body: Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jRZkAAAAASUVORK5CYII=', 'base64') }));
    const mpage = await mobile.newPage();
    attachPage(mpage, 'mobile');
    if (token) {
      await mpage.addInitScript((t) => {
        window.localStorage.setItem('rpi-web-token', t);
      }, token);
    }
    await mpage.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(mpage, () => document.title === 'rpi web', 'mobile: page title missing');
    await waitFor(
      mpage,
      () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true,
      'mobile: WS did not reach "connected"'
    );
    await ensureRenderedHost(mpage, 'mobile');
    await mpage.screenshot({ path: `${evidence}/mermaid-mobile-inline.png`, fullPage: true });

    // 8. EXPLICIT View tap (never a dbltap) opens the viewer.
    await mpage.tap(`${HOST} .md-mermaid-toolbar__btn[data-mermaid-action="view"]`);
    await viewerOpen(mpage, 'mobile View tap');

    // 7. Viewer controls are >=44px touch targets on the phone viewport.
    const mobileControls = await mpage.evaluate(() => {
      const viewerButtons = [...document.querySelectorAll('.mermaid-viewer__btn')];
      const hostButtons = [...document.querySelectorAll('.md-mermaid-toolbar__btn')];
      return {
        viewer: viewerButtons.map((b) => ({
          action: b.getAttribute('data-viewer-action'),
          w: b.getBoundingClientRect().width,
          h: b.getBoundingClientRect().height,
        })),
        host: hostButtons.map((b) => ({
          action: b.getAttribute('data-mermaid-action'),
          w: b.getBoundingClientRect().width,
          h: b.getBoundingClientRect().height,
        })),
      };
    });
    const smallViewerBtn = mobileControls.viewer.find((b) => b.h < 44);
    if (smallViewerBtn) {
      fail(`mobile viewer control ${smallViewerBtn.action} is only ${smallViewerBtn.h.toFixed(1)}px tall (<44px)`);
    }
    for (const b of mobileControls.host) {
      console.log(
        `web-mermaid-viewer: host toolbar ${b.action} on mobile: ${b.w.toFixed(1)}x${b.h.toFixed(1)}px`
      );
      if ((b.action === 'view' || b.action === 'download') && b.h < 44) {
        fail(`mobile host ${b.action} button is only ${b.h.toFixed(1)}px tall (<44px)`);
      }
    }

    // Viewer fits the 390x844 viewport: dialog covers it exactly, the fitted
    // diagram stays inside the stage, no page horizontal overflow.
    const fitCheck = await mpage.evaluate(() => {
      const root = document.querySelector('.mermaid-viewer');
      const stage = root.querySelector('.mermaid-viewer__stage');
      const svg = stage.querySelector('svg');
      const rootRect = root.getBoundingClientRect().toJSON();
      const stageRect = stage.getBoundingClientRect().toJSON();
      const svgRect = svg.getBoundingClientRect().toJSON();
      return {
        rootRect,
        stageRect,
        svgRect,
        overflow:
          document.scrollingElement.scrollWidth - document.scrollingElement.clientWidth,
      };
    });
    const r = fitCheck.rootRect;
    if (Math.abs(r.left) > 1 || Math.abs(r.top) > 1 || Math.abs(r.width - 390) > 1 || Math.abs(r.height - 844) > 1) {
      fail(`mobile viewer does not cover the viewport: ${JSON.stringify(r)}`);
    }
    const s = fitCheck.stageRect;
    const b = fitCheck.svgRect;
    if (b.left < s.left - 1 || b.top < s.top - 1 || b.right > s.right + 1 || b.bottom > s.bottom + 1) {
      fail(`mobile fitted diagram escapes the stage: ${JSON.stringify({ stage: s, svg: b })}`);
    }
    if (fitCheck.overflow > 1) {
      fail(`mobile page has ${fitCheck.overflow}px horizontal overflow while the viewer is open`);
    }
    await mpage.screenshot({ path: `${evidence}/mermaid-mobile-viewer.png` });

    // Live viewer SVG carries no active content (same sanitizer choke point
    // as the download).
    const viewerSvg = await mpage.evaluate(
      () => document.querySelector('.mermaid-viewer__canvas svg')?.outerHTML || ''
    );
    assertSanitizedSvg(viewerSvg, 'mobile viewer svg');

    // Single-finger pan works on the stage. Real touch input: CDP touch is
    // translated by Chrome into pointer events with pointerType 'touch' (the
    // stage has touch-action: none, so the drag never degrades to scrolling,
    // and setPointerCapture sees an active pointer). The CDP session is
    // detached in a finally so a failed dispatch never leaks the session.
    const mCenter = await mpage.evaluate(() => {
      const r = document.querySelector('.mermaid-viewer__stage').getBoundingClientRect();
      return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
    });
    const mobileBeforeDrag = await mpage.evaluate(metricsFn);
    if (!mobileBeforeDrag) fail('mobile: viewer metrics missing before pan');
    const cdp = await mpage.context().newCDPSession(mpage);
    try {
      const touchMove = (i) =>
        cdp.send('Input.dispatchTouchEvent', {
          type: 'touchMove',
          touchPoints: [{ x: mCenter.x + (70 * i) / 6, y: mCenter.y + (40 * i) / 6 }],
        });
      await cdp.send('Input.dispatchTouchEvent', {
        type: 'touchStart',
        touchPoints: [{ x: mCenter.x, y: mCenter.y }],
      });
      for (let i = 1; i <= 6; i++) {
        await touchMove(i);
        await mpage.evaluate(() => new Promise((resolve) => requestAnimationFrame(() => resolve())));
      }
      await cdp.send('Input.dispatchTouchEvent', { type: 'touchEnd', touchPoints: [] });
    } finally {
      await cdp.detach();
    }
    await waitFor(
      mpage,
      metricsPredicate(
        'Math.abs(m.tx - (e.tx + 70)) < 60 && Math.abs(m.ty - (e.ty + 40)) < 60'
      ),
      'mobile: pan never moved the diagram',
      10000,
      mobileBeforeDrag
    );

    await mpage.waitForTimeout(350);
    await mpage.tap(`${VIEWER} button[data-viewer-action="close"]`);
    await waitFor(
      mpage,
      () => {
        const root = document.querySelector('.mermaid-viewer');
        return root === null || root.hidden === true;
      },
      'mobile: Close tap never closed the viewer'
    );

    // Zero console errors / page errors across both pages.
    if (consoleProblems.length > 0) {
      fail(`${consoleProblems.length} console/page problem(s):\n${consoleProblems.join('\n')}`);
    }

    console.log('web-mermaid-viewer: PASSED (desktop viewer + download + error host + mobile 390px)');
  } finally {
    await browser.close();
  }
}

main().catch((err) => {
  console.error(`web-mermaid-viewer: FAIL: ${err && err.stack ? err.stack : err}`);
  process.exit(2);
});