// Web Files-drawer directory-toggle latency lane — PLAYWRIGHT-ONLY
// (E2E.d/web/files_latency.sh).
//
// Environment:
//   RPI_URL        http://127.0.0.1:<port>/web
//   RPI_TOKEN      token file content (served via rpi-auth.<token> subprotocol)
//   RPI_CHROME     executable path of the system Chrome (optional)
//   RPI_EVIDENCE   evidence dir for screenshots + timings JSON
//   RPI_MESSAGES   seeded transcript message count (>=1500)
//   RPI_TOGGLE_DIR top-level directory toggled in the measurement (mod7)
//   RPI_SECOND_DIR directory toggled in session B only (mod3)
//
// What is proven (see files_latency.sh for the lane contract): real pointer
// click -> synchronous store write + file_explorer_list RPC send ->
// MutationObserver aria-expanded/child commit -> next rAF paint, with the
// commit landing by the first animation frame and before the exact successful
// listing response; exactly one listing RPC
// on first expansion, zero on collapse/re-expand; the >=1500-message
// transcript parent's fiber props NEVER change identity during the toggle;
// drawer close + outer reopen restores exact expansion geometry
// with no `›`/`‹` chrome; A/B session view isolation; zero console/page
// errors (counts + messages are published with the timings evidence).
// SUBDIRECTORY TRANSCRIPT LINKS (FL6.1-FL6.5, after A/B isolation):
// FL6.1 is the cwd <workspace>/pkg control and should open
// nested/link-target.txt. FL6.2 switches to session-a-seed (workspace root)
// and clicks the shortened nested/link-target.txt:1; the real file is
// pkg/nested/link-target.txt inside that root. FL6.3 leaves the duplicate
// basename unselected. FL6.4 clicks the outside absolute citation and
// forbids a read/selection of outside/outside-only.txt; a path marker on
// that citation is allowed. The 1500-message parent is not the click surface.
// FL6.5 clicks a BARE root-basename citation README.md:2 — the assistant
// fixture text carries no longer path context, so the render must stamp the
// exact basename (data-md-path="README.md", never data-md-resolved="none")
// and the click must open the already-seeded root README.md through
// session-a-seed's correlated, session-stamped file_explorer_read with no
// file-unavailable error.
// REFRESH GLYPH-ONLY BUSY (desktop 1440x900 AND mobile 390x844): with the
// listing/changes responses held by the probe, the ONLY busy surface is the
// Refresh button — `.is-refreshing`/`aria-busy` on that one node (never a
// pane shimmer/overlay/loading class), the accessible name stays `Refresh`
// with no visible Refresh text, the glyph animates, and the header/tree
// boxes do not move. The busy wait gates on that actual DOM state (plus the
// parked held responses) and NEVER on an elapsed wall-clock conjunct: the
// held responses keep the state up indefinitely, so the wait is fully
// event-driven.
// JOURNEY SURFACES: at BOTH breakpoints the changed journeys assert zero
// horizontal overflow (body, the .file-explorer shell, its header, and the
// #toasts toast/error layer) and zero visible error toasts. The 390x844
// mobile pass reuses the SAME page/fixture to capture and assert the settled
// initial pane loading, the glyph-only Refresh busy, and the released settle,
// then restores 1440x900 for the measurement steps below (screenshots:
// files-latency-mobile-open.png + files-latency-refresh-busy-mobile.png
// alongside the desktop pair).
// Observed durations are REPORTED (JSON evidence + stdout) — the lane
// asserts structural/ordering contracts, never an invented ms budget; every
// derived duration is computed from responseFrame.t/click/commit/frame
// arithmetic and gated FINITE and within its allowed sign range (NaN, an
// object frame instead of its .t timestamp, or an out-of-range duration can
// never pass). Assertion IDs are recorded EXPLICITLY (record(id) at each
// assertion site) into an exact-once ledger and published as an explicit
// coverage-assertions.json (documented/executed/counts) alongside the
// timings — never timings-only. Timings + screenshots are written to a
// run-scoped staging dir; artifacts are REGISTERED only after their
// write/screenshot succeeded, and the whole staged set is published ONLY
// after ALL gates pass — as ONE immutable run directory
// ($RPI_EVIDENCE/runs/web-files-latency-<nonce>/, one invocation nonce for
// staging dir + run dir + pointer) plus ONE atomically-renamed
// manifest/current pointer ($RPI_EVIDENCE/current.json). The publication
// helper validates the staged set EXACTLY (unique declared names, normalized
// relative paths contained in the run dir, lstat regular files only — no
// symlinks — and nothing missing or extra) inside its rollback scope, and
// TRACKS every path it creates (staging dir, renamed run dir, pointer temp
// file): a fault or validation failure BEFORE the pointer commit removes all
// three and preserves the previous pointer byte-identical (the
// fault-injection self-check injects at manifest write, stage->run rename,
// pointer write, and pointer rename, and rejects
// invalid/duplicate/missing/extra/symlink/traversal staged sets, against a
// throwaway evidence root).
// Evidence is never published early, partially, or via sequential canonical
// replacement; a failed (or crashed) run removes ONLY its staging dir.
//
// Exit 2 on assertion failure. No skip, no fallback.

// Playwright is imported only for a browser run; RPI_SELF_CHECK=1 exercises
// the pure evidence gates AND the fault-injection publication self-check
// without requiring the ephemeral browser dependency.
import { createFreshSessionViaManage } from './lib/session-lifecycle.mjs';

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_SELF_CHECK === '1'
  // Self-check mode runs the pure gates AND the fault-injection publication
  // self-check against a THROWAWAY evidence root, so the real publication
  // helper (staging dir, run-dir rename, pointer tmp + commit) is exercised
  // without touching any operator evidence directory.
  ? fs.mkdtempSync(path.join(os.tmpdir(), 'web-files-latency-selfcheck-'))
  : (process.env.RPI_EVIDENCE || '.');
const messages = Number(process.env.RPI_MESSAGES || '1500');
const toggleDir = process.env.RPI_TOGGLE_DIR || 'mod7';
const secondDir = process.env.RPI_SECOND_DIR || 'mod3';
const linkCwd = process.env.RPI_LINK_CWD || '';
const outsideFile = process.env.RPI_OUTSIDE_FILE || '';

// Run-scoped staging: every artifact this run creates (screenshots + the
// timings JSON) is written into a UNIQUE staging dir under the evidence root
// and published ONLY after ALL gates pass — as ONE immutable run directory
// plus ONE atomically-renamed manifest/current pointer (see the header).
// ONE invocation nonce derives the staging dir, the run dir, and the
// pointer, so no two publications can ever mix generations.
const laneNonce = `${process.pid}-${Date.now()}`;
const stagingDir = path.join(evidence, `.staging-web-files-latency-${laneNonce}`);
const runDirName = `runs/web-files-latency-${laneNonce}`;
const laneArtifacts = []; // registered artifact names, AFTER their write succeeded
let published = false;

async function stageShot(page, name) {
  fs.mkdirSync(stagingDir, { recursive: true });
  const staged = path.join(stagingDir, name);
  await page.screenshot({ path: staged, fullPage: true });
  laneArtifacts.push(name);
  return staged;
}

function stageWrite(name, data) {
  fs.mkdirSync(stagingDir, { recursive: true });
  const staged = path.join(stagingDir, name);
  fs.writeFileSync(staged, data);
  laneArtifacts.push(name);
  return staged;
}

function cleanupStaging() {
  fs.rmSync(stagingDir, { recursive: true, force: true });
}

// Exact staged-artifact validation: unique declared names, NORMALIZED
// relative paths contained inside the run dir (no absolute paths, no `..`
// traversal, no `.`/`//` segments), lstat REGULAR files only (a symlink —
// even one pointing at a real file — is rejected), and the EXACT staged set:
// every declared name present, nothing missing, nothing extra. Returns null
// when the staged set is exactly the declared one.
function stagedArtifactSetError(declaredNames, manifestName) {
  const declared = new Set();
  for (const name of [...declaredNames, manifestName]) {
    if (typeof name !== 'string' || name === '') {
      return `declared artifact name is not a non-empty string: ${JSON.stringify(name)}`;
    }
    const normalized = path.posix.normalize(name);
    if (path.posix.isAbsolute(name) || normalized !== name || normalized === '..' || normalized.startsWith('../')) {
      return `declared artifact is not a normalized contained relative path: ${JSON.stringify(name)}`;
    }
    if (declared.has(name)) return `duplicate declared artifact name: ${name}`;
    declared.add(name);
    let stats;
    try {
      stats = fs.lstatSync(path.join(stagingDir, name));
    } catch {
      return `declared artifact missing from the staged set: ${name}`;
    }
    if (!stats.isFile()) {
      return `declared artifact is not a regular file (symlinks/dirs rejected): ${name}`;
    }
  }
  const actual = new Set();
  const walk = (dir, prefix) => {
    for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
      if (entry.isDirectory()) walk(path.join(dir, entry.name), `${prefix}${entry.name}/`);
      else actual.add(`${prefix}${entry.name}`);
    }
  };
  walk(stagingDir, '');
  const missing = [...declared].filter((name) => !actual.has(name));
  const extras = [...actual].filter((name) => !declared.has(name));
  if (missing.length > 0 || extras.length > 0) {
    return `exact staged-set violation: missing=${JSON.stringify(missing)}, unexpected=${JSON.stringify(extras.sort())}`;
  }
  return null;
}

// Set-atomic, failure-clean publication: the manifest is written into the
// staging set, the `runs/` parent is created (so a FRESH evidence root can
// publish), the WHOLE staging directory is renamed ONCE into the immutable
// run directory, and then the single manifest/current pointer is written to
// a temp file and renamed atomically into place. Every publication step —
// the exact staged-artifact validation (unique names, normalized contained
// relative paths, lstat regular files only, exact staged set), the manifest
// write, the stage->run rename, and the pointer tmp write + rename — runs
// INSIDE one rollback scope: the helper TRACKS every path it created
// (staging dir, renamed run dir, pointer temp file) and any failure BEFORE
// the pointer commit removes all three and leaves the previous pointer
// byte-identical — a consumer either sees the previous publication or the
// complete new one, never a partial set, an orphan run dir, or a mixed
// generation. `faultAt` is self-check-ONLY fault injection
// (selfCheckPublicationFaults); production callers never pass it.
function publishStagedArtifacts(manifest, faultAt) {
  fs.mkdirSync(evidence, { recursive: true });
  fs.mkdirSync(path.join(evidence, 'runs'), { recursive: true });
  const runDir = path.join(evidence, runDirName);
  const pointer = {
    lane: 'web-files-latency',
    nonce: laneNonce,
    run: runDirName,
    manifest: 'manifest.json',
    coverage: 'coverage-assertions.json',
    artifacts: [...laneArtifacts, 'manifest.json'],
  };
  const pointerPath = path.join(evidence, 'current.json');
  const pointerTmp = `${pointerPath}.tmp-${process.pid}`;
  let renamed = false;
  try {
    if (faultAt === 'manifest') throw new Error('injected: manifest write');
    fs.writeFileSync(path.join(stagingDir, 'manifest.json'), `${JSON.stringify(manifest, null, 2)}\n`);
    // Exact artifact validation inside the rollback scope: a staged set that
    // is not exactly the declared one (duplicate, missing, extra, symlink,
    // or escaping name) can never be published.
    const artifactError = stagedArtifactSetError(laneArtifacts, 'manifest.json');
    if (artifactError !== null) throw new Error(`exact staged-artifact validation failed: ${artifactError}`);
    if (faultAt === 'rename') throw new Error('injected: stage->run rename');
    fs.renameSync(stagingDir, runDir);
    renamed = true;
    if (faultAt === 'pointer-write') throw new Error('injected: pointer write');
    fs.writeFileSync(pointerTmp, `${JSON.stringify(pointer, null, 2)}\n`);
    if (faultAt === 'pointer-rename') throw new Error('injected: pointer rename');
    fs.renameSync(pointerTmp, pointerPath);
  } catch (err) {
    // Pre-pointer-commit failure: remove THIS attempt's staging dir, renamed
    // run dir, and pointer temp file; the previous pointer (if any) keeps
    // naming the previous complete publication.
    fs.rmSync(stagingDir, { recursive: true, force: true });
    if (renamed) fs.rmSync(runDir, { recursive: true, force: true });
    fs.rmSync(pointerTmp, { force: true });
    throw err;
  }
  published = true;
}

function fail(message) {
  console.error(`web-files-latency: FAIL ${message}`);
  // A failed run removes ONLY its own staging dir; published evidence under
  // the evidence root is left untouched (never published early/partially).
  cleanupStaging();
  process.exit(2);
}

async function waitFor(page, fn, label, timeoutMs = 30000, arg, { diagnostic = false } = {}) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    try {
      if (await page.evaluate(fn, arg)) return;
    } catch {
      // Mid-navigation evaluate throws; keep polling until the deadline.
    }
    if (Date.now() > deadline) {
      const message = `timeout: ${label}`;
      if (diagnostic) throw new Error(message);
      fail(message);
    }
    await page.waitForTimeout(100);
  }
}

/** Row buttons of the workspace tree, keyed by visible dir name. */
function treeRowState(dirs) {
  const buttons = [...document.querySelectorAll('[data-testid="file-explorer-tree"] button.file-explorer__row--dir')];
  const state = {};
  for (const dir of dirs) {
    const target = buttons.find((b) => (b.querySelector('.file-explorer__row-path')?.textContent || '') === dir);
    state[dir] = target ? target.getAttribute('aria-expanded') : null;
  }
  return state;
}

// In-page held-Refresh busy state (desktop AND mobile): the ACTUAL
// `.is-refreshing` DOM state — button class + `aria-busy` — with BOTH held
// responses parked by the probe. Purely event-driven: no elapsed wall clock
// anywhere, the held responses keep the state up until the release marker.
function refreshBusyHeldState() {
  const chrome = window.__fxRefreshChrome();
  return chrome.refreshing
    && chrome.ariaBusy === 'true'
    && chrome.parked.includes('file_explorer_list')
    && chrome.parked.includes('file_explorer_changes');
}

// In-page settled state after the held responses release: the release
// marker observed, the busy class/aria dropped, and the tree painted.
function refreshSettledState(marker) {
  const chrome = window.__fxRefreshChrome();
  return chrome.marker === marker
    && chrome.refreshing === false
    && chrome.ariaBusy !== 'true'
    && chrome.treePainted;
}

// Pure glyph-only busy gate (desktop AND mobile): exactly ONE
// `.is-refreshing`/`aria-busy` node — the Refresh button itself — with
// aria-label Refresh, no visible Refresh text, a glyph with positive
// geometry that is animating, NO pane shimmer/overlay/loading class, the
// tree painted, and the header/tree bounding boxes unmoved.
function refreshBusyInvariantsError(phase, before, running) {
  if (running.accessibleName !== 'Refresh' || running.visibleLabel !== '') {
    return `${phase}: Refresh busy name/text is ${JSON.stringify(running.accessibleName)}/${JSON.stringify(running.visibleLabel)}`;
  }
  if (!running.glyphBox || running.glyphBox.width < 1 || running.glyphBox.height < 1) {
    return `${phase}: Refresh glyph has no positive geometry (${JSON.stringify(running.glyphBox)})`;
  }
  if (!running.busyOnlyButton) {
    return `${phase}: is-refreshing/aria-busy escaped the Refresh button`;
  }
  if (!running.glyphAnimating) return `${phase}: glyph is not animating during Refresh busy`;
  if (running.otherAnimating) {
    return `${phase}: a non-glyph element is animating during Refresh busy`;
  }
  if (running.shimmer || running.paneLoading || running.loadingClass) {
    return `${phase}: pane shimmer/overlay/loading class appeared during Refresh busy`;
  }
  if (!running.treePainted) return `${phase}: tree content unpainted during Refresh busy`;
  const moved = ['header', 'tree'].find((key) => {
    const a = before[key];
    const b = running[key];
    return !a || !b || a.x !== b.x || a.y !== b.y || a.width !== b.width || a.height !== b.height;
  });
  if (moved) {
    return `${phase}: ${moved} bounding box moved ${JSON.stringify(before[moved])} -> ${JSON.stringify(running[moved])}`;
  }
  return null;
}

function refreshSettledInvariantsError(phase, settled) {
  if (settled.accessibleName !== 'Refresh' || settled.visibleLabel !== '') {
    return `${phase}: Refresh settled name/text is ${JSON.stringify(settled.accessibleName)}/${JSON.stringify(settled.visibleLabel)}`;
  }
  if (settled.refreshing || settled.ariaBusy === 'true') {
    return `${phase}: busy class/aria remained after the release`;
  }
  if (!settled.treePainted) return `${phase}: tree content unpainted after the release`;
  if (settled.shimmer || settled.paneLoading) {
    return `${phase}: pane overlay remained after the release`;
  }
  return null;
}

function selfCheckRefreshInvariants() {
  const boxes = {
    header: { x: 1, y: 2, width: 30, height: 44 },
    tree: { x: 3, y: 50, width: 200, height: 400 },
  };
  const before = { header: { ...boxes.header }, tree: { ...boxes.tree } };
  const valid = {
    accessibleName: 'Refresh', visibleLabel: '', refreshing: true, ariaBusy: 'true', glyphAnimating: true,
    glyphBox: { width: 12, height: 12 },
    shimmer: false, paneLoading: false, loadingClass: false, treePainted: true,
    otherAnimating: false,
    busyOnlyButton: true, header: { ...boxes.header }, tree: { ...boxes.tree },
  };
  if (refreshBusyInvariantsError('self-check', before, valid) !== null) {
    throw new Error('refresh busy invariants self-check rejected a valid glyph-only busy sample');
  }
  for (const patch of [
    { visibleLabel: 'Refresh' },
    { accessibleName: 'Refreshing' },
    { glyphBox: { width: 0, height: 12 } },
    { busyOnlyButton: false },
    { glyphAnimating: false },
    { otherAnimating: true },
    { shimmer: true },
    { paneLoading: true },
    { loadingClass: true },
    { treePainted: false },
    { header: { ...boxes.header, x: 9 } },
    { tree: null },
  ]) {
    if (refreshBusyInvariantsError('self-check', before, { ...valid, ...patch }) === null) {
      throw new Error(`refresh busy invariants self-check accepted ${JSON.stringify(patch)}`);
    }
  }
  const validSettled = {
    accessibleName: 'Refresh', visibleLabel: '', refreshing: false, ariaBusy: 'false', treePainted: true,
    shimmer: false, paneLoading: false,
  };
  if (refreshSettledInvariantsError('self-check', validSettled) !== null) {
    throw new Error('refresh settled invariants self-check rejected a valid settled sample');
  }
  for (const patch of [
    { accessibleName: null },
    { visibleLabel: 'Refresh' },
    { refreshing: true },
    { ariaBusy: 'true' },
    { treePainted: false },
    { shimmer: true },
    { paneLoading: true },
  ]) {
    if (refreshSettledInvariantsError('self-check', { ...validSettled, ...patch }) === null) {
      throw new Error(`refresh settled invariants self-check accepted ${JSON.stringify(patch)}`);
    }
  }
}

selfCheckRefreshInvariants();

// In-page sampler for the JOURNEY SURFACE gates (both breakpoints, every
// changed-journey assertion point): horizontal overflow of body, the
// .file-explorer shell, its header, and the #toasts layer (rendered nodes
// whose scrollWidth exceeds clientWidth beyond a 1px subpixel tolerance),
// plus every VISIBLE error toast inside the ONE dedicated toast/error layer.
function journeySurfaceSample() {
  const overflowing = [];
  for (const selector of ['body', '.file-explorer', '[data-testid="file-explorer-header"]', '#toasts']) {
    for (const node of document.querySelectorAll(selector)) {
      if (node.getClientRects().length === 0) continue;
      if (node.scrollWidth > node.clientWidth + 1) {
        overflowing.push(`${selector} scrollWidth=${node.scrollWidth} clientWidth=${node.clientWidth}`);
      }
    }
  }
  const toastLayer = document.getElementById('toasts');
  const errorToasts = toastLayer
    ? [...toastLayer.querySelectorAll('.toast--error')]
      .filter((toast) => toast.getClientRects().length > 0 && (toast.textContent || '').trim() !== '')
      .map((toast) => (toast.textContent || '').trim())
    : [];
  return { overflowing, toastLayerPresent: toastLayer != null, errorToasts };
}

// Pure gate over a sampled journey surface; `label` names the breakpoint +
// journey point so a failure identifies exactly where the surface broke.
function journeySurfaceGateError(label, sample) {
  if (!sample || typeof sample !== 'object'
    || !Array.isArray(sample.overflowing) || !Array.isArray(sample.errorToasts)) {
    return `${label}: journey surface sample is malformed: ${JSON.stringify(sample)}`;
  }
  if (sample.toastLayerPresent !== true) {
    return `${label}: #toasts toast/error layer missing from the DOM`;
  }
  if (sample.overflowing.length > 0) {
    return `${label}: horizontal overflow in ${JSON.stringify(sample.overflowing)}`;
  }
  if (sample.errorToasts.length > 0) {
    return `${label}: visible error toast(s): ${JSON.stringify(sample.errorToasts)}`;
  }
  return null;
}
function mobileHeaderTargetError(targets) {
  if (!Array.isArray(targets) || targets.length === 0) {
    return `mobile File Explorer header controls missing: ${JSON.stringify(targets)}`;
  }
  const tooSmall = targets.filter((target) => target.width < 44 || target.height < 44);
  return tooSmall.length === 0
    ? null
    : `mobile File Explorer targets below 44px: ${JSON.stringify(tooSmall)}`;
}

function selfCheckMobileHeaderTargets() {
  if (mobileHeaderTargetError([{ label: 'Refresh', width: 44, height: 44 }]) !== null
    || mobileHeaderTargetError([{ label: 'Close', width: 43, height: 44 }]) === null) {
    throw new Error('mobile File Explorer target gate failed its negative control');
  }
}

selfCheckMobileHeaderTargets();

function selfCheckJourneySurfaceGate() {
  const clean = { overflowing: [], toastLayerPresent: true, errorToasts: [] };
  if (journeySurfaceGateError('label', clean) !== null) {
    throw new Error('journey surface gate self-check rejected a clean surface');
  }
  for (const broken of [
    { overflowing: ['body scrollWidth=900 clientWidth=390'] },
    { toastLayerPresent: false },
    { errorToasts: ['listing failed'] },
    null,
    { overflowing: 'oops', toastLayerPresent: true, errorToasts: [] },
  ]) {
    if (journeySurfaceGateError('label', broken) === null) {
      throw new Error(`journey surface gate self-check accepted ${JSON.stringify(broken)}`);
    }
  }
}

selfCheckJourneySurfaceGate();

// Node-side assertion over the live page surface at one journey point.
async function assertJourneySurface(page, label) {
  const surfaceError = journeySurfaceGateError(label, await page.evaluate(journeySurfaceSample));
  if (surfaceError !== null) fail(surfaceError);
}

function latencyEvidenceError(sample) {
  if (sample.clickAt == null) return 'pointerdown probe never observed the real click';
  if (sample.sendAt == null) return 'file_explorer_list send was never observed';
  if (sample.commitAt == null) return 'MutationObserver never observed the aria-expanded/child commit';
  if (sample.firstFrameAt == null) return 'next-animation-frame probe never fired';
  if (sample.responseAt == null) return 'exact file_explorer_list response was never observed';
  if (sample.responseSuccess !== true) return `file_explorer_list response failed: ${JSON.stringify(sample.responseError)}`;
  if (sample.sendAt < sample.clickAt) return 'file_explorer_list send predates the real pointer click';
  if (sample.sendAt > sample.commitAt) return 'file_explorer_list send landed after the DOM commit';
  if (sample.firstFrameAt - sample.commitAt < -1) return 'toggle commit missed the first animation frame after the click';
  if (sample.responseAt < sample.commitAt) return 'listing response arrived before the expansion commit';
  return null;
}

function selfCheckLatencyEvidence() {
  const valid = { clickAt: 1, sendAt: 2, commitAt: 3, firstFrameAt: 4, responseAt: 5, responseSuccess: true };
  if (latencyEvidenceError(valid) !== null) throw new Error('latency evidence self-check rejected a valid sequence');
  const invalid = [
    { ...valid, clickAt: null },
    { ...valid, sendAt: 0 },
    { ...valid, sendAt: 4 },
    { ...valid, firstFrameAt: 1 },
    { ...valid, responseAt: 2 },
    { ...valid, responseSuccess: false, responseError: 'expected failure' },
  ];
  for (const sample of invalid) {
    if (latencyEvidenceError(sample) === null) {
      throw new Error(`latency evidence self-check accepted ${JSON.stringify(sample)}`);
    }
  }
}

selfCheckLatencyEvidence();

// Pure finite/nonnegative gate over the DERIVED durations (all computed from
// responseFrame.t / clickAt / commitAt / firstFrameAt arithmetic): a NaN, an
// Infinity, or a value outside its allowed sign range can never be published
// as evidence. commitMinusFirstAnimationFrameMs tolerates the same -1ms
// boundary jitter as the ordering gate above; every other duration is
// nonnegative by contract.
function timingGateError(durations) {
  for (const { name, value, min = 0 } of durations) {
    if (typeof value !== 'number' || !Number.isFinite(value)) {
      return `${name} is not a finite number: ${String(value)}`;
    }
    if (value < min) return `${name}=${value} is below the allowed minimum ${min}`;
  }
  return null;
}

// Pure publication gate: evidence is published ONLY when this returns null —
// any coverage/completeness mismatch or captured browser error
// (console.error/pageerror/websocket socketerror) blocks it (staged
// artifacts are dropped by fail(), never published early).
function publicationGateError({ coverageError, browserErrors }) {
  if (coverageError !== null) return `coverage: ${coverageError}`;
  if (browserErrors.length > 0) {
    return `browser errors captured during the lane (${browserErrors.length}, first: ${browserErrors[0]})`;
  }
  return null;
}

function selfCheckTimingAndPublicationGates() {
  const firstExpand = {
    clickToRpcSendMs: 1.5,
    clickToCommitMs: 2.5,
    commitMinusFirstAnimationFrameMs: -0.4,
    clickToFirstFrameMs: 3,
    listingResponseArrivalAfterCommitMs: 12,
  };
  if (timingGateError([
    { name: 'clickToRpcSendMs', value: firstExpand.clickToRpcSendMs },
    { name: 'clickToCommitMs', value: firstExpand.clickToCommitMs },
    { name: 'commitMinusFirstAnimationFrameMs', value: firstExpand.commitMinusFirstAnimationFrameMs, min: -1 },
    { name: 'clickToFirstFrameMs', value: firstExpand.clickToFirstFrameMs },
    { name: 'listingResponseArrivalAfterCommitMs', value: firstExpand.listingResponseArrivalAfterCommitMs },
  ]) !== null) {
    throw new Error('timing gate self-check rejected a valid duration set');
  }
  const invalidDurations = [
    { name: 'clickToCommitMs', value: Number.NaN },
    { name: 'clickToCommitMs', value: Number.POSITIVE_INFINITY },
    { name: 'clickToCommitMs', value: -0.1 },
    // An object frame used where its .t timestamp belongs (the exact bug
    // this gate exists for: object arithmetic yields NaN).
    { name: 'listingResponseArrivalAfterCommitMs', value: Number.NaN },
  ];
  for (const entry of invalidDurations) {
    if (timingGateError([{ name: entry.name, value: entry.value }]) === null) {
      throw new Error(`timing gate self-check accepted ${JSON.stringify(entry)}`);
    }
  }
  if (timingGateError([{ name: 'commitMinusFirstAnimationFrameMs', value: -0.5, min: -1 }]) !== null) {
    throw new Error('timing gate self-check rejected the allowed -1ms boundary jitter');
  }
  if (timingGateError([{ name: 'commitMinusFirstAnimationFrameMs', value: -1.5, min: -1 }]) === null) {
    throw new Error('timing gate self-check accepted a duration beyond the -1ms boundary');
  }
  if (publicationGateError({ coverageError: null, browserErrors: [] }) !== null) {
    throw new Error('publication gate self-check blocked an error-free lane');
  }
  for (const coverageError of ['missing=["FL1.1"]', 'unexpected=["FL9.9"]']) {
    if (publicationGateError({ coverageError, browserErrors: [] }) === null) {
      throw new Error(`publication gate self-check published despite coverage error ${coverageError}`);
    }
  }
  for (const errors of [['console: boom'], ['pageerror: boom'], ['websocket: Error: socket boom'], ['console.error: a', 'pageerror: b']]) {
    if (publicationGateError({ coverageError: null, browserErrors: errors }) === null) {
      throw new Error(`publication gate self-check published despite browser errors ${JSON.stringify(errors)}`);
    }
  }
}

selfCheckTimingAndPublicationGates();

// Assertion matrix (feature -> IDs, recorded as machine-readable evidence):
//   FL0 FL0.1 boot: page title + WS connected + seeded session A active with
//              the seeded transcript parent really mounted
//        FL0.2/FL0.3 desktop 1440x900 Refresh glyph-only busy (held
//              responses; the busy wait gates on the ACTUAL .is-refreshing/
//              aria-busy DOM state plus the parked held responses — never an
//              elapsed wall-clock conjunct) + released settle
//        FL0.4/FL0.5/FL0.6 the SAME journey at 390x844: the settled initial
//              pane loading, the glyph-only Refresh busy, the released
//              settle + the 1440x900 viewport restore for the steps below
//   FL1 FL1.2 the >=1500-message transcript parent never re-renders during
//              the toggle (zero transcript mutations + zero parent fiber
//              prop-identity commits) — its observation window settles
//              BEFORE the timing gate, so it records first
//        FL1.1 first expansion: exactly one file_explorer_list RPC for the
//              toggled path; commit lands by the first animation frame; the
//              exact correlated successful listing response arrives AFTER
//              the commit (every derived duration finite/nonnegative-gated)
//   FL2 FL2.1 collapse + re-expand issue ZERO new listing RPCs
//   FL3 FL3.1 drawer close + outer reopen restores the exact
//              expansion geometry with no legacy `›`/`‹` chrome
//        FL3.2 close/reopen dispatches exactly ONE path-correlated
//              file_explorer_list REQUEST for the toggled path and settles
//              it with exactly ONE exact successful RESPONSE
//   FL4 FL4.1 A/B session view isolation: A geometry survives the B
//              round-trip with zero cross-session leak
//        FL4.2 B geometry survives the return trip (isolation both ways)
//   FL5 FL5.1 zero browser errors (console.error/pageerror/websocket
//              socketerror) captured across the whole lane
//   FL6 subdirectory transcript links (not the 1500-message parent):
//        FL6.1 cwd <workspace>/pkg control: nested/link-target.txt:1 opens
//              that exact file
//        FL6.2 workspace-root session reads nested/link-target.txt first
//              (response fails), then pkg/nested/link-target.txt (succeeds)
//              and shows LINK-NESTED-EXACT; no tree row is required
//        FL6.3 duplicate basename issues no homonym read
//        FL6.4 sibling-of-root absolute citation issues no read and does
//              not show LINK-OUTSIDE; a path marker is allowed
//        FL6.5 bare root-basename README.md:2 citation (no path context)
//              stamps the exact basename at render (no
//              data-md-resolved="none") and opens the root README via the
//              session-stamped correlated read; no file-unavailable error
//
// Recording discipline: assertion IDs are recorded EXPLICITLY (record(id) at
// each assertion site) — never implicitly from waitFor labels. DOCUMENTED_IDS
// is declared in the lane's ACTUAL execution order; every documented ID must
// execute EXACTLY ONCE, and the exact-once ledger (the Map's real insertion
// order + real counts) is published with the timings as an explicit
// coverage-assertions.json — never timings-only, never a reconstruction of
// the documented list.
const DOCUMENTED_IDS = [
  'FL0.1',
  'FL0.2', 'FL0.3',
  'FL0.4', 'FL0.5', 'FL0.6',
  'FL1.2', 'FL1.1',
  'FL2.1',
  'FL3.1', 'FL3.2',
  'FL4.1', 'FL4.2',
  'FL6.1', 'FL6.2', 'FL6.3', 'FL6.4', 'FL6.5',
  'FL5.1',
];
const executedCounts = new Map();
function record(id) {
  if (!DOCUMENTED_IDS.includes(id)) {
    fail(`record: unexpected assertion id ${JSON.stringify(id)} (not in DOCUMENTED_IDS)`);
  }
  const count = (executedCounts.get(id) || 0) + 1;
  executedCounts.set(id, count);
  if (count !== 1) {
    fail(`record: assertion id ${id} recorded ${count}x — every documented id must execute exactly once`);
  }
  console.log(`[web-files-latency:assert] ${id} (#${count})`);
}

// Pure exact-once completeness gate over the explicit per-ID counts ledger:
// missing (count 0), duplicate execution (count > 1), unexpected ids,
// duplicate DOCUMENTED_IDS entries, and an execution order that is not
// EXACTLY the documented order (elementwise Map-insertion equality — never
// a sorted/set comparison) can never pass.
function documentedIdsCompletenessError(documentedIds, counts) {
  const documented = new Set(documentedIds);
  const missing = documentedIds.filter((id) => (counts.get(id) || 0) === 0);
  const unexpected = [...counts.keys()].filter((id) => !documented.has(id));
  const duplicates = documentedIds.filter((id, index) => documentedIds.indexOf(id) !== index);
  const repeated = [...counts].filter(([, count]) => count > 1).map(([id]) => id);
  const setsClean = missing.length === 0 && unexpected.length === 0
    && duplicates.length === 0 && repeated.length === 0;
  // Only meaningful once the key sets are identical: the Map's insertion
  // order must equal the documented order position for position.
  if (setsClean && [...counts.keys()].some((id, index) => id !== documentedIds[index])) {
    return `assertion order mismatch: executed Map insertion order ${JSON.stringify([...counts.keys()])} is not exactly the documented order ${JSON.stringify([...documentedIds])}`;
  }
  if (setsClean) return null;
  return `assertion coverage mismatch: missing=${JSON.stringify(missing)}, unexpected=${JSON.stringify(unexpected)}, duplicate documented ids=${JSON.stringify([...new Set(duplicates)])}, ids executed more than once=${JSON.stringify(repeated)}`;
}

function selfCheckCoverageLedger() {
  const counts = (pairs) => new Map(pairs);
  if (documentedIdsCompletenessError(['A', 'B'], counts([['A', 1], ['B', 1]])) !== null) {
    throw new Error('files-latency completeness self-check rejected an exact match');
  }
  for (const invalid of [counts([]), counts([['A', 1]]), counts([['A', 1], ['B', 0]])]) {
    if (documentedIdsCompletenessError(['A', 'B'], invalid) === null) {
      throw new Error(`completeness self-check accepted missing ids ${JSON.stringify([...invalid])}`);
    }
  }
  if (documentedIdsCompletenessError(['A', 'B'], counts([['A', 1], ['B', 1], ['C', 1]])) === null) {
    throw new Error('completeness self-check accepted an unexpected id');
  }
  if (documentedIdsCompletenessError(['A', 'B'], counts([['A', 2], ['B', 1]])) === null) {
    throw new Error('completeness self-check accepted a duplicate execution');
  }
  // Reordered execution (same exact-once set, different Map insertion
  // order) must fail the elementwise documented-order gate.
  if (documentedIdsCompletenessError(['A', 'B'], counts([['B', 1], ['A', 1]])) === null) {
    throw new Error('completeness self-check accepted a reordered execution ledger');
  }
  if (documentedIdsCompletenessError(['A', 'A'], counts([['A', 1]])) === null) {
    throw new Error('completeness self-check accepted duplicate documented ids');
  }
}

selfCheckCoverageLedger();

// Wire browser-error capture and return the REAL ledger array the handlers
// push into — the same array instance the publication gate consumes. Same
// wiring as the sessions_workflow lane: pageerror + console.error + WS
// socketerror, labeled with the lane name.
function monitorBrowserErrors(page, label) {
  const errors = [];
  page.on('pageerror', (err) => {
    errors.push(`${label} pageerror: ${err.message}`);
  });
  page.on('console', (message) => {
    if (message.type() === 'error') errors.push(`${label} console: ${message.text()}`);
  });
  page.on('websocket', (socket) => {
    socket.on('socketerror', (error) => {
      errors.push(`${label} websocket: ${String(error)}`);
    });
  });
  return errors;
}

// Self-check that invokes the SAME monitorBrowserErrors wiring the lane uses
// (with a stand-in page object implementing Playwright's `.on`) and asserts
// the RETURNED ledger is the real one: pageerror / console.error / websocket
// socketerror entries land in it with the lane label, while non-error
// console messages never do.
function selfCheckMonitorBrowserErrors() {
  const handlers = new Map();
  const fakePage = {
    on(event, handler) {
      if (!handlers.has(event)) handlers.set(event, []);
      handlers.get(event).push(handler);
    },
  };
  const ledger = monitorBrowserErrors(fakePage, 'files-latency-self-check');
  if (!Array.isArray(ledger)) throw new Error('monitorBrowserErrors self-check: returned ledger is not an array');
  handlers.get('pageerror')[0](new Error('page boom'));
  handlers.get('console')[0]({ type: () => 'warning', text: () => 'ignored' });
  handlers.get('console')[0]({ type: () => 'error', text: () => 'console boom' });
  handlers.get('websocket')[0]({ on: (_event, handler) => handler(new Error('socket boom')) });
  const expected = [
    'files-latency-self-check pageerror: page boom',
    'files-latency-self-check console: console boom',
    'files-latency-self-check websocket: Error: socket boom',
  ];
  if (JSON.stringify(ledger) !== JSON.stringify(expected)) {
    throw new Error(`monitorBrowserErrors self-check ledger mismatch: ${JSON.stringify(ledger)}`);
  }
}

selfCheckMonitorBrowserErrors();

// Exact-artifact self-check over the ACTUAL production publication helper:
// a staged set that is not EXACTLY the declared one must be rejected with a
// full rollback (staging dir removed, no run dir, no pointer temp, previous
// pointer byte-identical) — a duplicate name, a missing declared artifact,
// an extra unregistered file, a symlinked artifact (even one pointing at a
// real file), and a `..` traversal name can never publish.
function selfCheckStagedArtifactValidation() {
  const pointerPath = path.join(evidence, 'current.json');
  const sentinel = '{"sentinel":"previous publication"}\n';
  const reset = () => {
    fs.rmSync(stagingDir, { recursive: true, force: true });
    fs.rmSync(path.join(evidence, runDirName), { recursive: true, force: true });
    fs.writeFileSync(pointerPath, sentinel);
    laneArtifacts.length = 0;
  };
  const reject = (label) => {
    let threw = false;
    try {
      publishStagedArtifacts({ probe: true });
    } catch (err) {
      threw = true;
      if (!String(err.message).includes('exact staged-artifact validation failed')) {
        throw new Error(`staged-artifact self-check (${label}): failed for the wrong reason: ${err.message}`);
      }
    }
    if (!threw) throw new Error(`staged-artifact self-check: invalid staged set ${label} was published`);
    if (fs.existsSync(stagingDir)) throw new Error(`staged-artifact self-check: staging dir survived rejection (${label})`);
    if (fs.existsSync(path.join(evidence, runDirName))) throw new Error(`staged-artifact self-check: run dir survived rejection (${label})`);
    if (fs.readdirSync(evidence).some((name) => name.startsWith('current.json.tmp-'))) {
      throw new Error(`staged-artifact self-check: pointer temp survived rejection (${label})`);
    }
    if (fs.readFileSync(pointerPath, 'utf8') !== sentinel) {
      throw new Error(`staged-artifact self-check: previous pointer mutated by rejection (${label})`);
    }
  };
  reset();
  stageWrite('probe.json', '{"probe":1}\n');
  laneArtifacts.push('probe.json');
  laneArtifacts.push('probe.json');
  reject('duplicate');
  // Missing declared artifact (registered but never written).
  reset();
  stageWrite('probe.json', '{"probe":1}\n');
  laneArtifacts.push('ghost.json');
  reject('missing');
  // Extra unregistered file inside the staged set.
  reset();
  stageWrite('probe.json', '{"probe":1}\n');
  fs.writeFileSync(path.join(stagingDir, 'extra.json'), '{"extra":1}\n');
  reject('extra');
  // Symlinked artifact: even a symlink pointing at a REAL file is rejected.
  fs.rmSync(stagingDir, { recursive: true, force: true });
  fs.rmSync(path.join(evidence, runDirName), { recursive: true, force: true });
  const outsideTarget = path.join(evidence, 'outside-target.txt');
  fs.writeFileSync(outsideTarget, 'real bytes\n');
  fs.mkdirSync(stagingDir, { recursive: true });
  fs.symlinkSync(outsideTarget, path.join(stagingDir, 'link.json'));
  laneArtifacts.length = 0;
  laneArtifacts.push('probe.json', 'link.json');
  fs.writeFileSync(path.join(stagingDir, 'probe.json'), '{"probe":1}\n');
  reject('symlink');
  fs.rmSync(outsideTarget, { force: true });
  // Traversal name escaping the run dir.
  reset();
  stageWrite('probe.json', '{"probe":1}\n');
  laneArtifacts.push('../escape.json');
  reject('traversal');
}

// Fault-injection self-check over the ACTUAL publication helper (not a
// reimplementation): seeds a previous publication pointer, stages a fresh
// artifact set, and injects a fault at each pre-pointer-commit stage
// (manifest write, stage->run rename, pointer write, pointer rename). Each
// injected run must throw, leave NO staging dir / renamed run dir / pointer
// temp file behind, and preserve the previous pointer byte-identical; a
// clean invocation then publishes for real (run dir exists, pointer
// atomically replaced, no temp residue).
function selfCheckPublicationFaults() {
  const pointerPath = path.join(evidence, 'current.json');
  const sentinel = '{"sentinel":"previous publication"}\n';
  for (const faultAt of ['manifest', 'rename', 'pointer-write', 'pointer-rename']) {
    fs.rmSync(stagingDir, { recursive: true, force: true });
    fs.rmSync(path.join(evidence, runDirName), { recursive: true, force: true });
    fs.writeFileSync(pointerPath, sentinel);
    laneArtifacts.length = 0;
    stageWrite('probe.json', '{"probe":1}\n');
    let threw = false;
    try {
      publishStagedArtifacts({ probe: true }, faultAt);
    } catch {
      threw = true;
    }
    if (!threw) throw new Error(`publication fault self-check: fault ${faultAt} did not throw`);
    if (fs.existsSync(stagingDir)) throw new Error(`publication fault self-check: staging dir survived fault ${faultAt}`);
    if (fs.existsSync(path.join(evidence, runDirName))) throw new Error(`publication fault self-check: run dir survived fault ${faultAt}`);
    if (fs.readdirSync(evidence).some((name) => name.startsWith('current.json.tmp-'))) {
      throw new Error(`publication fault self-check: pointer temp survived fault ${faultAt}`);
    }
    if (fs.readFileSync(pointerPath, 'utf8') !== sentinel) {
      throw new Error(`publication fault self-check: old pointer mutated by fault ${faultAt}`);
    }
  }
  laneArtifacts.length = 0;
  stageWrite('probe.json', '{"probe":1}\n');
  publishStagedArtifacts({ probe: true });
  if (fs.existsSync(stagingDir)) throw new Error('publication fault self-check: staging dir survived a successful publish');
  if (!fs.existsSync(path.join(evidence, runDirName, 'probe.json'))
    || !fs.existsSync(path.join(evidence, runDirName, 'manifest.json'))) {
    throw new Error('publication fault self-check: successful publish did not materialize the run dir');
  }
  const pointer = JSON.parse(fs.readFileSync(pointerPath, 'utf8'));
  if (pointer.lane !== 'web-files-latency' || pointer.run !== runDirName
    || pointer.nonce !== laneNonce
    || pointer.manifest !== 'manifest.json'
    || pointer.coverage !== 'coverage-assertions.json'
    || !Array.isArray(pointer.artifacts) || !pointer.artifacts.includes('probe.json')) {
    throw new Error(`publication fault self-check: pointer does not name the new run: ${JSON.stringify(pointer)}`);
  }
  if (fs.readdirSync(evidence).some((name) => name.startsWith('current.json.tmp-'))) {
    throw new Error('publication fault self-check: pointer temp survived a successful publish');
  }
}

function installProbes(bigArrayMin) {
  window.__fxFrames = [];
  window.__fxHold = { types: [], parked: [], released: 0, marker: null };
  window.__fxHoldArm = (types) => {
    window.__fxHold.types = [...types];
    window.__fxHold.parked = [];
    window.__fxHold.marker = null;
  };
  window.__fxHoldRelease = (marker) => {
    const hold = window.__fxHold;
    hold.types = [];
    hold.marker = marker;
    const parked = hold.parked.splice(0);
    for (const entry of parked) {
      entry.socket.dispatchEvent(new MessageEvent('message', { data: entry.data }));
    }
    hold.released += parked.length;
    return parked.length;
  };
  window.__fxRefreshChrome = () => {
    const btn = document.getElementById('file-explorer-refresh-btn');
    const glyph = btn && btn.querySelector('.file-explorer__refresh-glyph');
    const rect = (el) => {
      const r = el.getBoundingClientRect();
      return { x: Math.round(r.x), y: Math.round(r.y), width: Math.round(r.width), height: Math.round(r.height) };
    };
    const explorer = document.querySelector('.file-explorer');
    const header = document.querySelector('[data-testid="file-explorer-header"]');
    const tree = document.querySelector('[data-testid="file-explorer-tree"]');
    const busyNodes = explorer ? [...explorer.querySelectorAll('[aria-busy="true"]')] : [];
    const refreshingNodes = explorer ? [...explorer.querySelectorAll('.is-refreshing')] : [];
    const otherAnimating = explorer
      ? [...explorer.querySelectorAll('*')].some(
          (el) =>
            el !== glyph &&
            el.getAnimations().some((a) => a.playState === 'running' || a.playState === 'pending'),
        )
      : false;
    const glyphText = glyph ? glyph.textContent || '' : '';
    return {
      accessibleName: btn ? btn.getAttribute('aria-label') : null,
      visibleLabel: btn ? (btn.textContent || '').replace(glyphText, '').trim() : null,
      refreshing: !!(btn && btn.classList.contains('is-refreshing')),
      ariaBusy: btn ? btn.getAttribute('aria-busy') : null,
      glyphBox: glyph && glyph.getClientRects().length > 0 ? rect(glyph) : null,
      glyphAnimating: !!(glyph && glyph.getAnimations().some((a) => a.playState === 'running' || a.playState === 'pending')),
      otherAnimating,
      shimmer: document.querySelector('.file-explorer .file-explorer__pane-shimmer') != null,
      paneLoading: document.querySelector('.file-explorer .file-explorer__pane-loading') != null,
      loadingClass: document.querySelector('.file-explorer .file-explorer__loading') != null,
      treePainted: !!(tree && tree.querySelector('.file-explorer__row--dir')),
      busyOnlyButton: refreshingNodes.length === 1 && refreshingNodes[0] === btn
        && busyNodes.length > 0 && busyNodes.every((el) => el === btn),
      header: header ? rect(header) : null,
      tree: tree ? rect(tree) : null,
      parked: window.__fxHold.parked.map((p) => p.command),
      marker: window.__fxHold.marker,
    };
  };
  const NativeWebSocket = window.WebSocket;
  class FxProbeWebSocket extends NativeWebSocket {
    constructor(...args) {
      super(...args);
      this.addEventListener('message', (event) => {
        if (typeof event.data !== 'string') return;
        try {
          const frame = JSON.parse(event.data);
          if (frame?.type === 'response'
            && typeof frame.command === 'string'
            && frame.command.startsWith('file_explorer')) {
            window.__fxFrames.push({
              dir: 'in', type: frame.command, id: frame.id ?? '', success: frame.success === true,
              path: typeof frame.data?.path === 'string' ? frame.data.path : '',
              code: typeof frame.data?.code === 'string' ? frame.data.code : '',
              error: frame.error ?? null, t: performance.now(),
            });
            if (window.__fxHold.types.includes(frame.command)) {
              window.__fxHold.parked.push({ id: frame.id, command: frame.command, data: event.data, socket: this });
              event.stopImmediatePropagation();
            }
          }
        } catch { /* non-JSON frames are ignored by the probe */ }
      }, true);
    }
    send(data) {
      if (typeof data === 'string') {
        try {
          const frame = JSON.parse(data);
          if (typeof frame?.type === 'string' && frame.type.startsWith('file_explorer')) {
            window.__fxFrames.push({
              dir: 'out', type: frame.type, id: frame.id ?? '', path: frame.path ?? '', session: frame.sessionId ?? '', t: performance.now(),
            });
          }
        } catch { /* non-JSON frames pass through unchanged */ }
      }
      return super.send(data);
    }
  }
  for (const key of ['CONNECTING', 'OPEN', 'CLOSING', 'CLOSED']) {
    Object.defineProperty(FxProbeWebSocket, key, { value: NativeWebSocket[key] });
  }
  window.WebSocket = FxProbeWebSocket;

  window.__fx = { commits: 0, parentCommits: 0, bigArrayLength: 0, lastBigProps: undefined };
  let sawBigPropsOnce = false;
  window.__REACT_DEVTOOLS_GLOBAL_HOOK__ = {
    renderers: new Map(),
    supportsFiber: true,
    inject(internals) {
      this.renderers.set(internals.id, internals);
      return internals.id;
    },
    onCommitFiberRoot(_rendererId, root) {
      const fx = window.__fx;
      fx.commits += 1;
      const stack = [root.current];
      let visited = 0;
      while (stack.length > 0 && visited < 12000) {
        const fiber = stack.pop();
        if (!fiber) continue;
        visited += 1;
        const props = fiber.memoizedProps;
        if (props && typeof props === 'object') {
          for (const key in props) {
            const value = props[key];
            if (Array.isArray(value) && value.length >= bigArrayMin) {
              if (!sawBigPropsOnce) {
                sawBigPropsOnce = true;
                fx.bigArrayLength = value.length;
              } else if (fx.lastBigProps !== undefined && fx.lastBigProps !== props) {
                fx.parentCommits += 1;
              }
              fx.lastBigProps = props;
              return;
            }
          }
        }
        if (fiber.child) stack.push(fiber.child);
        if (fiber.sibling) stack.push(fiber.sibling);
      }
    },
  };

  window.__fxArm = () => {
    const fx = window;
    fx.__fxClickAt = null;
    fx.__fxCommitAt = null;
    fx.__fxFirstFrameAt = null;
    fx.__fxTranscriptMutations = 0;
    const transcript = document.getElementById('transcript');
    const observer = new MutationObserver((records) => {
      const now = performance.now();
      for (const record of records) {
        const target = record.target;
        const insideExplorer = target instanceof Element
          && target.closest('[data-testid="file-explorer-tree"]') != null;
        if (!insideExplorer) {
          if (transcript && target instanceof Node && transcript.contains(target)) {
            fx.__fxTranscriptMutations += 1;
          }
          continue;
        }
        const ariaFlip = record.type === 'attributes';
        const childRows = record.type === 'childList' && record.addedNodes.length > 0;
        if ((ariaFlip || childRows) && fx.__fxCommitAt == null) fx.__fxCommitAt = now;
      }
    });
    observer.observe(document.body, {
      childList: true, subtree: true, attributes: true, attributeFilter: ['aria-expanded'],
    });
    fx.__fxPointerArm = (event) => {
      if (fx.__fxClickAt == null) fx.__fxClickAt = performance.now();
      requestAnimationFrame(() => {
        if (fx.__fxFirstFrameAt == null) fx.__fxFirstFrameAt = performance.now();
      });
      document.removeEventListener('pointerdown', fx.__fxPointerArm, true);
    };
    document.addEventListener('pointerdown', fx.__fxPointerArm, true);
  };
}

async function main() {
  if (!url) fail('RPI_URL is required');
  const launchOptions = chromePath ? { executablePath: chromePath } : {};
  const { chromium } = await import('playwright');
  const browser = await chromium.launch(launchOptions);
  const timings = { messages, bigArrayLength: 0, toggles: {}, browserErrors: { count: 0, messages: [] } };
  try {
    const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
    // Capture pageerror + console.error + websocket socketerror for the
    // whole lane through the SAME monitorBrowserErrors helper the lane's
    // self-check exercises; counts + messages are published with the
    // timings evidence and ANY captured error blocks publication
    // (publicationGateError).
    const browserErrors = monitorBrowserErrors(page, 'files-latency-lane');
    await page.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
    await page.addInitScript(`(${installProbes.toString()})(${Math.max(1200, messages - 300)})`);
    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'page title missing');
    await waitFor(
      page,
      () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true,
      'WS did not reach connected'
    );
    await waitFor(
      page,
      (sid) => document.querySelector(`.session-sidebar__row--active [data-session-id="${sid}"]`) != null,
      'session-a-seed never became the active session',
      30000,
      'session-a-seed'
    );

    // The large parent is really mounted: the fiber probe saw the seeded
    // transcript array (>= messages-300 entries).
    const parentState = await page.evaluate(() => {
      const lifecycle = document.getElementById('session-lifecycle');
      return {
        bigArrayLength: window.__fx.bigArrayLength || 0,
        stateMessages: lifecycle?.dataset.stateMessages || '',
        projectedDom: lifecycle?.dataset.projectedDom || '',
        rows: [...document.querySelectorAll('.session-sidebar__switch')].map((row) => ({
          id: row.dataset.sessionId || '',
          text: row.textContent || '',
          active: row.closest('.session-sidebar__row')?.classList.contains('session-sidebar__row--active') === true,
        })),
        transcript: (document.getElementById('transcript')?.textContent || '').slice(0, 500),
      };
    });
    const bigLength = parentState.bigArrayLength;
    if (bigLength < messages - 300) {
      fail(`parent render probe never saw the seeded transcript array: ${JSON.stringify(parentState)}`);
    }
    record('FL0.1');

    // Open the Files drawer and let the INITIAL loads settle so the measured
    // toggle is not polluted by the drawer's first root/changes fetch.
    await page.click('#file-explorer-toggle-btn');
    await waitFor(
      page,
      () => document.querySelector('[data-testid="file-explorer-tree"] .file-explorer__row--dir') != null,
      'workspace tree rows never rendered'
    );
    await stageShot(page, 'files-latency-open.png');
    await waitFor(
      page,
      () => document.querySelector('[data-testid="file-explorer-tree"] .file-explorer__muted') == null,
      'initial workspace/changes load never settled'
    );

    const boxesBefore = await page.evaluate(() => window.__fxRefreshChrome());
    if (!boxesBefore.treePainted || boxesBefore.refreshing || boxesBefore.ariaBusy === 'true') {
      fail('Refresh journey: panes were not idle/painted before the Refresh click');
    }
    await assertJourneySurface(page, 'desktop 1440x900 idle before Refresh');
    // The held responses keep the busy state up indefinitely, so the busy
    // wait is fully event-driven: it gates ONLY on the actual
    // `.is-refreshing`/`aria-busy` DOM state plus both parked held
    // responses — the elapsed wall-clock conjunct is gone; the glyph-only
    // invariants below prove the state structurally.
    await page.evaluate(() => window.__fxHoldArm(['file_explorer_list', 'file_explorer_changes']));
    await page.click('#file-explorer-refresh-btn');
    await waitFor(
      page,
      refreshBusyHeldState,
      'held refresh list/changes never left the button in running busy'
    );
    const running = await page.evaluate(() => window.__fxRefreshChrome());
    const busyError = refreshBusyInvariantsError('desktop 1440x900 Refresh busy', boxesBefore, running);
    if (busyError !== null) fail(busyError);
    await assertJourneySurface(page, 'desktop 1440x900 Refresh busy');
    await stageShot(page, 'files-latency-refresh-busy.png');
    record('FL0.2');
    const releaseMarker = `fx-refresh-release-${Date.now()}`;
    const released = await page.evaluate((marker) => window.__fxHoldRelease(marker), releaseMarker);
    if (released < 2) fail(`Refresh release: expected held list+changes, released ${released}`);
    await waitFor(
      page,
      refreshSettledState,
      'refresh busy survived authoritative release',
      30000,
      releaseMarker
    );
    const settled = await page.evaluate(() => window.__fxRefreshChrome());
    const settledError = refreshSettledInvariantsError('desktop 1440x900', settled);
    if (settledError !== null) fail(settledError);
    await assertJourneySurface(page, 'desktop 1440x900 settled after release');
    record('FL0.3');

    /* ---- FL0.4-FL0.6: the SAME changed journey at 390x844 ----
       The SAME page/fixture is reused at the phone viewport: the settled
       initial pane loading, the held-response glyph-only Refresh busy, and
       the released settle are each captured and asserted, then the viewport
       is restored to 1440x900 because the measurement steps below (T1-T4)
       run on the desktop layout. Every wait gates on actual DOM state; the
       busy wait never consults the wall clock. */
    await page.setViewportSize({ width: 390, height: 844 });
    await waitFor(
      page,
      () => {
        const chrome = window.__fxRefreshChrome();
        return chrome.treePainted
          && !chrome.refreshing
          && chrome.ariaBusy !== 'true'
          && !chrome.shimmer && !chrome.paneLoading && !chrome.loadingClass
          && document.querySelector('[data-testid="file-explorer-tree"] .file-explorer__muted') == null;
      },
      'mobile 390x844 initial pane loading never settled after the viewport switch'
    );
    await assertJourneySurface(page, 'mobile 390x844 settled open');
    const mobileHeaderTargets = await page.evaluate(() =>
      [...document.querySelectorAll('[data-testid="file-explorer-header"] button')]
        .filter((button) => button.getClientRects().length > 0)
        .map((button) => {
          const rect = button.getBoundingClientRect();
          return {
            label: button.getAttribute('aria-label') || button.textContent || '',
            width: rect.width,
            height: rect.height,
          };
        }),
    );
    const mobileTargetError = mobileHeaderTargetError(mobileHeaderTargets);
    if (mobileTargetError !== null) fail(mobileTargetError);
    await stageShot(page, 'files-latency-mobile-open.png');
    record('FL0.4');

    const mobileBoxesBefore = await page.evaluate(() => window.__fxRefreshChrome());
    if (!mobileBoxesBefore.treePainted || mobileBoxesBefore.refreshing || mobileBoxesBefore.ariaBusy === 'true') {
      fail('mobile Refresh journey: panes were not idle/painted before the Refresh click');
    }
    await page.evaluate(() => window.__fxHoldArm(['file_explorer_list', 'file_explorer_changes']));
    await page.click('#file-explorer-refresh-btn');
    await waitFor(
      page,
      refreshBusyHeldState,
      'mobile held refresh list/changes never left the button in running busy'
    );
    const mobileRunning = await page.evaluate(() => window.__fxRefreshChrome());
    const mobileBusyError = refreshBusyInvariantsError('mobile 390x844 Refresh busy', mobileBoxesBefore, mobileRunning);
    if (mobileBusyError !== null) fail(mobileBusyError);
    await assertJourneySurface(page, 'mobile 390x844 Refresh busy');
    await stageShot(page, 'files-latency-refresh-busy-mobile.png');
    record('FL0.5');

    const mobileReleaseMarker = `fx-refresh-release-mobile-${Date.now()}`;
    const mobileReleased = await page.evaluate((marker) => window.__fxHoldRelease(marker), mobileReleaseMarker);
    if (mobileReleased < 2) fail(`mobile Refresh release: expected held list+changes, released ${mobileReleased}`);
    await waitFor(
      page,
      refreshSettledState,
      'mobile refresh busy survived authoritative release',
      30000,
      mobileReleaseMarker
    );
    const mobileSettled = await page.evaluate(() => window.__fxRefreshChrome());
    const mobileSettledError = refreshSettledInvariantsError('mobile 390x844', mobileSettled);
    if (mobileSettledError !== null) fail(mobileSettledError);
    await assertJourneySurface(page, 'mobile 390x844 settled after release');
    // Restore the desktop viewport: T1-T4 below measure the desktop layout,
    // so the restore itself must be verified, not assumed.
    await page.setViewportSize({ width: 1440, height: 900 });
    await waitFor(
      page,
      () => (document.getElementById('file-explorer-refresh-btn')?.getBoundingClientRect().width ?? 0) > 0
        && document.querySelector('[data-testid="file-explorer-tree"] .file-explorer__row--dir') != null,
      'desktop 1440x900 layout never restored after the mobile pass'
    );
    record('FL0.6');

    const outListFrames = () => page.evaluate(
      () => window.__fxFrames.filter((f) => f.dir === 'out' && f.type === 'file_explorer_list')
    );

    /* ---- T1: first expansion of the measured directory ---- */
    const dirButton = page
      .locator('[data-testid="file-explorer-tree"] button.file-explorer__row--dir', { hasText: toggleDir })
      .first();
    await dirButton.waitFor({ state: 'visible', timeout: 20000 });
    await page.evaluate(() => window.__fxArm());
    await page.evaluate(() => { window.__fx.parentCommits = 0; });
    await dirButton.click();
    await waitFor(
      page,
      (dir) => {
        const buttons = [...document.querySelectorAll('[data-testid="file-explorer-tree"] button.file-explorer__row--dir')];
        return buttons.some((b) => (b.querySelector('.file-explorer__row-path')?.textContent || '') === dir
          && b.getAttribute('aria-expanded') === 'true');
      },
      'aria-expanded never flipped true for the toggled directory',
      10000,
      toggleDir
    );
    // Child rows must be committed with the flip — loading state or rows,
    // the subtree is never blank.
    await waitFor(
      page,
      () => {
        const viewport = document.querySelector('[data-testid="file-explorer-tree"]');
        return viewport != null && viewport.textContent.includes('sub0');
      },
      'expanded directory never rendered its child rows',
      10000
    );
    await page.evaluate(() => new Promise((resolve) => requestAnimationFrame(() => resolve())));
    const m1 = await page.evaluate(() => ({
      clickAt: window.__fxClickAt,
      commitAt: window.__fxCommitAt,
      firstFrameAt: window.__fxFirstFrameAt,
      transcriptMutations: window.__fxTranscriptMutations,
      parentCommits: window.__fx.parentCommits,
    }));
    const frames1 = await outListFrames();
    const dirFrames1 = frames1.filter((f) => f.path === toggleDir);
    if (dirFrames1.length !== 1) {
      fail(`T1: expected exactly 1 file_explorer_list RPC for first expansion of ${toggleDir}, saw ${dirFrames1.length}`);
    }
    const sendFrame = dirFrames1[0];
    const responseFrame = await page.evaluate((requestId) => window.__fxFrames.find(
      (frame) => frame.dir === 'in' && frame.type === 'file_explorer_list' && frame.id === requestId
    ) ?? null, sendFrame.id);
    const evidenceError = latencyEvidenceError({
      clickAt: m1.clickAt,
      sendAt: sendFrame.t,
      commitAt: m1.commitAt,
      firstFrameAt: m1.firstFrameAt,
      responseAt: responseFrame?.t ?? null,
      responseSuccess: responseFrame?.success === true,
      responseError: responseFrame?.error ?? null,
    });
    if (evidenceError !== null) fail(`T1: ${evidenceError}`);
    const commitVsFrame = m1.firstFrameAt - m1.commitAt;
    if (m1.transcriptMutations !== 0) {
      fail(`T1: ${m1.transcriptMutations} transcript mutations during the toggle — the large parent re-rendered`);
    }
    if (m1.parentCommits !== 0) {
      fail(`T1: transcript-carrying fiber props changed identity ${m1.parentCommits}x during the toggle — top-level parent re-render`);
    }
    record('FL1.2');
    // Every derived duration comes from explicit timestamp arithmetic — the
    // response arrival uses responseFrame.t (NOT the frame object itself,
    // whose object arithmetic would silently yield NaN) — and the finite/
    // nonnegative timing gate runs BEFORE anything is recorded as evidence.
    const firstExpandDurations = {
      clickToRpcSendMs: sendFrame.t - m1.clickAt,
      clickToCommitMs: m1.commitAt - m1.clickAt,
      commitMinusFirstAnimationFrameMs: commitVsFrame,
      clickToFirstFrameMs: m1.firstFrameAt - m1.clickAt,
      listingResponseArrivalAfterCommitMs: responseFrame.t - m1.commitAt,
    };
    const firstExpandTimingError = timingGateError([
      { name: 'clickToRpcSendMs', value: firstExpandDurations.clickToRpcSendMs },
      { name: 'clickToCommitMs', value: firstExpandDurations.clickToCommitMs },
      { name: 'commitMinusFirstAnimationFrameMs', value: firstExpandDurations.commitMinusFirstAnimationFrameMs, min: -1 },
      { name: 'clickToFirstFrameMs', value: firstExpandDurations.clickToFirstFrameMs },
      { name: 'listingResponseArrivalAfterCommitMs', value: firstExpandDurations.listingResponseArrivalAfterCommitMs },
    ]);
    if (firstExpandTimingError !== null) fail(`T1: ${firstExpandTimingError}`);
    record('FL1.1');
    timings.toggles.firstExpand = {
      ...firstExpandDurations,
      transcriptMutations: m1.transcriptMutations,
      parentCommits: m1.parentCommits,
    };

    /* ---- T2: collapse + re-expand = zero listing RPCs ---- */
    const rpcBefore = frames1.length;
    await dirButton.click();
    await waitFor(
      page,
      (dir) => {
        const buttons = [...document.querySelectorAll('[data-testid="file-explorer-tree"] button.file-explorer__row--dir')];
        return buttons.some((b) => (b.querySelector('.file-explorer__row-path')?.textContent || '') === dir
          && b.getAttribute('aria-expanded') === 'false');
      },
      'collapse never flipped aria-expanded to false',
      10000,
      toggleDir
    );
    await dirButton.click();
    await waitFor(
      page,
      (dir) => {
        const buttons = [...document.querySelectorAll('[data-testid="file-explorer-tree"] button.file-explorer__row--dir')];
        return buttons.some((b) => (b.querySelector('.file-explorer__row-path')?.textContent || '') === dir
          && b.getAttribute('aria-expanded') === 'true');
      },
      're-expand never restored aria-expanded true',
      10000,
      toggleDir
    );
    const rpcAfter = (await outListFrames()).length;
    if (rpcAfter !== rpcBefore) {
      fail(`T2: collapse/re-expand issued ${rpcAfter - rpcBefore} new listing RPCs — the cached directory was refetched`);
    }
    record('FL2.1');
    timings.toggles.reExpandZeroRpc = { listRpcsBefore: rpcBefore, listRpcsAfter: rpcAfter };

    /* ---- T3: drawer close + outer reopen restores geometry ---- */
    // Exact PATH-CORRELATED close/reopen listing counts: the close/reopen
    // remount must dispatch exactly ONE file_explorer_list REQUEST for the
    // toggled path (fresh listing refetch on remount is the close/reopen
    // contract — geometry is what must survive) and settle it with exactly
    // ONE exact correlated success RESPONSE for that request id. The request
    // baseline is captured BEFORE the close click — after the reopen both
    // sides of the diff would already contain the remount's request and the
    // exact-one contract would be unverifiable. A zero delta, a duplicate, a
    // request for a different path, or a failed/mismatched response can
    // never pass.
    const pathListRequests = () => page.evaluate(
      (dir) => window.__fxFrames.filter((f) => f.dir === 'out' && f.type === 'file_explorer_list' && f.path === dir),
      toggleDir
    );
    const requestsBeforeClose = await pathListRequests();
    await page.click('#file-explorer-drawer-close-btn');
    await waitFor(
      page,
      () => document.querySelector('[data-testid="file-explorer-tree"]') == null,
      'drawer never closed via the drawer close button'
    );
    const outerFilesToggle = page.locator('#file-explorer-toggle-btn');
    if (!(await outerFilesToggle.isVisible())) {
      await page.click('#sidebar-toggle-btn');
      await outerFilesToggle.waitFor({ state: 'visible', timeout: 10000 });
    }
    await outerFilesToggle.click();
    await waitFor(
      page,
      (dir) => {
        const buttons = [...document.querySelectorAll('[data-testid="file-explorer-tree"] button.file-explorer__row--dir')];
        return buttons.some((b) => (b.querySelector('.file-explorer__row-path')?.textContent || '') === dir
          && b.getAttribute('aria-expanded') === 'true');
      },
      'close/reopen did not restore the exact expansion geometry',
      20000,
      toggleDir
    );
    const expandedCount = await page.evaluate(() => document.querySelectorAll('[data-testid="file-explorer-tree"] button.file-explorer__row--dir[aria-expanded="true"]').length);
    if (expandedCount !== 1) {
      fail(`T3: expected only the toggled dir expanded after reopen, saw ${expandedCount}`);
    }
    const drawerText = await page.evaluate(() => document.querySelector('.file-explorer')?.textContent || '');
    if (drawerText.includes('›') || drawerText.includes('‹')) {
      fail('T3: legacy `›`/`‹` tree-collapse chrome reappeared in the Files drawer');
    }
    record('FL3.1');
    const requestsAfterReopen = await pathListRequests();
    if (requestsAfterReopen.length - requestsBeforeClose.length !== 1) {
      fail(`T3: expected exactly 1 path-correlated file_explorer_list request after close/reopen of ${toggleDir}, saw ${requestsAfterReopen.length - requestsBeforeClose.length}`);
    }
    const reopenRequest = requestsAfterReopen[requestsAfterReopen.length - 1];
    await waitFor(
      page,
      (requestId) => window.__fxFrames.some(
        (frame) => frame.dir === 'in' && frame.type === 'file_explorer_list' && frame.id === requestId
      ),
      `close/reopen listing response ${reopenRequest.id} never arrived`,
      10000,
      reopenRequest.id
    );
    const reopenResponses = await page.evaluate((requestId) => window.__fxFrames.filter(
      (frame) => frame.dir === 'in' && frame.type === 'file_explorer_list' && frame.id === requestId
    ), reopenRequest.id);
    if (reopenResponses.length !== 1) {
      fail(`T3: expected exactly 1 correlated file_explorer_list response for request ${reopenRequest.id}, saw ${reopenResponses.length}`);
    }
    if (reopenResponses[0].success !== true) {
      fail(`T3: close/reopen listing response failed: ${JSON.stringify(reopenResponses[0].error)}`);
    }
    timings.toggles.closeReopen = {
      // Exact path-correlated wire evidence: exactly one listing request for
      // the toggled path with exactly one correlated success response.
      pathCorrelatedListRequests: 1,
      correlatedSuccessResponses: 1,
      expandedRowsAfterReopen: expandedCount,
    };
    record('FL3.2');

    /* ---- T4: A/B session view isolation ---- */
    await page.click('.session-sidebar__row [data-session-id="session-b-seed"]');
    await waitFor(
      page,
      (sid) => document.querySelector(`.session-sidebar__row--active [data-session-id="${sid}"]`) != null,
      'session-b-seed never activated',
      30000,
      'session-b-seed'
    );
    // B's drawer starts closed (own slot); open it and check the store default.
    await page.click('#file-explorer-toggle-btn');
    await waitFor(
      page,
      () => document.querySelector('[data-testid="file-explorer-tree"] .file-explorer__row--dir') != null,
      'session B drawer never rendered its tree',
      20000
    );
    const bInitial = await page.evaluate(treeRowState, [toggleDir, secondDir]);
    if (bInitial[toggleDir] !== 'false') {
      fail(`T4: session B leaked session A's expansion (${toggleDir} aria-expanded=${bInitial[toggleDir]})`);
    }
    const bButton = page
      .locator('[data-testid="file-explorer-tree"] button.file-explorer__row--dir', { hasText: secondDir })
      .first();
    await bButton.click();
    await waitFor(
      page,
      (dir) => {
        const buttons = [...document.querySelectorAll('[data-testid="file-explorer-tree"] button.file-explorer__row--dir')];
        return buttons.some((b) => (b.querySelector('.file-explorer__row-path')?.textContent || '') === dir
          && b.getAttribute('aria-expanded') === 'true');
      },
      'session B toggle never expanded its own directory',
      10000,
      secondDir
    );

    const openSession = async (sid) => {
      await page.click(`.session-sidebar__row [data-session-id="${sid}"]`);
      await waitFor(
        page,
        (id) => document.querySelector(`.session-sidebar__row--active [data-session-id="${id}"]`) != null,
        `${sid} never re-activated`,
        30000,
        sid
      );
    };

    // Back to A: exact A geometry survives the B round-trip.
    await openSession('session-a-seed');
    await waitFor(
      page,
      () => document.querySelector('[data-testid="file-explorer-tree"] button.file-explorer__row--dir') != null,
      'session A tree never rendered after the B round-trip',
      20000
    );
    const aState = await page.evaluate(treeRowState, [toggleDir, secondDir]);
    if (aState[toggleDir] !== 'true' || aState[secondDir] !== 'false') {
      fail(`T4: session A geometry corrupted after the B round-trip (${JSON.stringify(aState)})`);
    }
    record('FL4.1');

    // B again: B geometry survives too — no cross-session leak either way.
    await openSession('session-b-seed');
    await waitFor(
      page,
      () => document.querySelector('[data-testid="file-explorer-tree"] button.file-explorer__row--dir') != null,
      'session B tree never rendered on return',
      20000
    );
    const bState = await page.evaluate(treeRowState, [toggleDir, secondDir]);
    if (bState[secondDir] !== 'true' || bState[toggleDir] !== 'false') {
      fail(`T4: session B geometry leaked after returning (${JSON.stringify(bState)})`);
    }
    record('FL4.2');
    await stageShot(page, 'files-latency-isolation.png');

    /* ---- FL6: subdirectory transcript links ----
       FL6.1 control: session cwd is <workspace>/pkg, so
       nested/link-target.txt exists there. FL6.2 failing click:
       session-a-seed cwd is the workspace root; the visible citation is
       the shortened nested/link-target.txt:1 and the real file inside
       that root is pkg/nested/link-target.txt. */
    if (!linkCwd || !linkCwd.endsWith('/pkg')) {
      fail(`FL6: RPI_LINK_CWD must be the seeded pkg subdirectory (got ${JSON.stringify(linkCwd)})`);
    }
    const workspaceRoot = linkCwd.slice(0, -'/pkg'.length);
    const linkSession = await createFreshSessionViaManage(page, waitFor, 'FL6 subdirectory session', {
      closePanel: false,
      explicitCwd: linkCwd,
    });
    const linkPanelCwd = await page.evaluate(() => {
      const el = document.querySelector('[data-testid="session-cwd-value"]');
      return el ? el.getAttribute('title') || '' : '';
    });
    if (linkSession.explicitCwd !== linkCwd || linkPanelCwd !== linkCwd) {
      fail(`FL6: subdirectory session cwd mismatch (typed=${linkSession.explicitCwd}, panel=${linkPanelCwd}, want=${linkCwd})`);
    }
    await page.locator('#session-close-btn').click();
    await waitFor(page, () => document.getElementById('session-panel') === null, 'FL6: session panel did not close');
    const controlMarker = `link-control-${laneNonce}`;
    const rootMarker = `link-root-${laneNonce}`;
    const nestedRel = 'nested/link-target.txt';
    const rootedRel = `pkg/${nestedRel}`;
    const dupBase = 'dup-name.txt';
    const outsideAbs = outsideFile;
    if (!outsideAbs.endsWith('/outside-sentinel/outside-only.txt') || outsideAbs.startsWith(`${workspaceRoot}/`)) {
      fail(`FL6: RPI_OUTSIDE_FILE must be a sibling of the workspace root (got ${JSON.stringify(outsideAbs)})`);
    }
    const controlPrompt = [
      controlMarker,
      `control \`${nestedRel}:1\``,
      `duplicate \`${dupBase}:1\` beside a/${dupBase} and b/${dupBase}`,
      `outside \`${outsideAbs}:1\``,
    ].join('\n');
    const controlChip = `${nestedRel}:1`;
    await page.fill('#prompt-input', controlPrompt);
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      (mark) => {
        const hit = [...document.querySelectorAll('#transcript .transcript-surface:not([hidden]) .msg--assistant .assistant-text')]
          .find((node) => (node.textContent || '').includes(mark));
        return hit != null && document.getElementById('send-btn')?.getAttribute('aria-label') === 'Send message';
      },
      'FL6.1: pkg-cwd assistant never echoed the control citation',
      45000,
      controlMarker,
    );
    const controlOwner = await page.evaluate((mark) => {
      const node = [...document.querySelectorAll('#transcript .transcript-surface:not([hidden]) .msg--assistant')]
        .find((el) => (el.textContent || '').includes(mark));
      return node ? node.getAttribute('data-workspace-cwd') || '' : '';
    }, controlMarker);
    if (controlOwner !== linkCwd) {
      fail(`FL6.1: control owner cwd is ${JSON.stringify(controlOwner)}, want ${linkCwd}`);
    }
    const visibleChip = (marker, text) => page
      .locator('#transcript .transcript-surface:not([hidden]) .msg--assistant', { hasText: marker })
      .locator('code', { hasText: text })
      .first();
    const fl6Dump = async (label, error) => {
      const original = error && error.message ? error.message : String(error);
      const notes = [];
      let dump = null;
      try {
        dump = await page.evaluate(() => ({
          active: document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '',
          send: document.getElementById('send-btn')?.getAttribute('aria-label') || '',
          content: (document.querySelector('[data-testid="file-explorer-content"]')?.textContent || '').slice(0, 400),
          contentError: document.querySelector('[data-testid="file-explorer-content"] .file-explorer__error')?.textContent || '',
          reads: window.__fxFrames.filter((frame) => frame.type === 'file_explorer_read').slice(-12),
        }));
      } catch (diagErr) {
        notes.push(`evaluate: ${(diagErr && diagErr.message) || diagErr}`);
      }
      const shot = `/tmp/rpi-fl6-${label}.png`;
      const jsonPath = `/tmp/rpi-fl6-${label}.json`;
      try {
        await page.screenshot({ path: shot, fullPage: true });
      } catch (diagErr) {
        notes.push(`screenshot: ${(diagErr && diagErr.message) || diagErr}`);
      }
      try {
        fs.writeFileSync(jsonPath, `${JSON.stringify({ original, dump }, null, 2)}\n`);
      } catch (diagErr) {
        notes.push(`write: ${(diagErr && diagErr.message) || diagErr}`);
      }
      if (notes.length > 0) {
        console.error(`web-files-latency: ${label} diagnostics capture failed: ${notes.join(' | ')}`);
      }
      fail(original);
    };
    const readFrames = () => page.evaluate(() => window.__fxFrames.filter(
      (frame) => frame.type === 'file_explorer_read',
    ).map((frame) => ({
      dir: frame.dir, path: frame.path || '', session: frame.session || '', id: frame.id, success: frame.success, code: frame.code || '', error: frame.error,
    })));
    const outgoingReadCount = () => page.evaluate(() => window.__fxFrames.filter(
      (frame) => frame.dir === 'out' && frame.type === 'file_explorer_read',
    ).length);
    const openedContent = () => page.evaluate(() => ({
      text: (document.querySelector('[data-testid="file-explorer-content"]')?.textContent || '').slice(0, 300),
      error: document.querySelector('[data-testid="file-explorer-content"] .file-explorer__error')?.textContent || '',
    }));
    const readsBeforeControl = await outgoingReadCount();
    await visibleChip(controlMarker, controlChip).click();
    try {
      await waitFor(
        page,
        (before) => {
          const reads = window.__fxFrames.filter((frame) => frame.type === 'file_explorer_read').slice(before);
          const sent = reads.find((frame) => frame.dir === 'out' && frame.path === 'nested/link-target.txt');
          const ok = sent && reads.some((frame) => frame.dir === 'in' && frame.id === sent.id && frame.success === true);
          const content = document.querySelector('[data-testid="file-explorer-content"]');
          return Boolean(ok)
            && content != null
            && (content.textContent || '').includes('LINK-NESTED-EXACT')
            && content.querySelector('.file-explorer__error') == null;
        },
        'FL6.1: pkg-cwd click did not open nested/link-target.txt',
        20000,
        readsBeforeControl,
        { diagnostic: true },
      );
    } catch (error) {
      await fl6Dump('FL6.1', error);
    }
    record('FL6.1');
    await openSession('session-a-seed');
    await waitFor(
      page,
      ({ sid, root }) => {
        const active = document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '';
        const visible = document.querySelector('#transcript .transcript-surface:not([hidden])');
        const owner = visible?.querySelector('.msg--assistant')?.getAttribute('data-workspace-cwd') || '';
        return active === sid && owner === root && (visible?.textContent || '').includes('session-a-seed message');
      },
      'FL6.2: session-a-seed did not become the stable visible owner',
      30000,
      { sid: 'session-a-seed', root: workspaceRoot },
    );
    const rootPrompt = [
      rootMarker,
      `short \`${nestedRel}:1\` beside ${rootedRel}`,
      `duplicate \`${dupBase}:1\` beside pkg/a/${dupBase} and pkg/b/${dupBase}`,
      `outside \`${outsideAbs}:1\``,
    ].join('\n');
    const shortChip = `${nestedRel}:1`;
    await page.fill('#prompt-input', rootPrompt);
    const acceptedOwner = await page.evaluate(() =>
      document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '');
    if (acceptedOwner !== 'session-a-seed') {
      fail(`FL6.2: prompt filled while ${acceptedOwner} was active, not session-a-seed`);
    }
    await page.press('#prompt-input', 'Enter');
    try {
      await waitFor(
        page,
        ({ mark, chip, root, sid }) => {
          const active = document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '';
          const node = [...document.querySelectorAll('#transcript .transcript-surface:not([hidden]) .msg--assistant')]
            .find((el) => (el.textContent || '').includes(mark));
          return active === sid
            && node != null
            && node.getAttribute('data-workspace-cwd') === root
            && [...node.querySelectorAll('code')].some((code) => (code.textContent || '') === chip)
            && document.getElementById('send-btn')?.getAttribute('aria-label') === 'Send message';
        },
        'FL6.2: root session never rendered the shortened citation',
        45000,
        { mark: rootMarker, chip: shortChip, root: workspaceRoot, sid: 'session-a-seed' },
        { diagnostic: true },
      );
    } catch (error) {
      const original = error && error.message ? error.message : String(error);
      const notes = [];
      let dump = null;
      try {
        dump = await page.evaluate(({ mark, chip, sid }) => {
          const visible = document.querySelector('#transcript .transcript-surface:not([hidden])');
          const assistants = [...document.querySelectorAll('#transcript .msg--assistant')].slice(-4).map((node) => ({
            hidden: node.closest('.transcript-surface')?.hasAttribute('hidden') === true,
            cwd: node.getAttribute('data-workspace-cwd') || '',
            text: (node.textContent || '').slice(0, 500),
            codes: [...node.querySelectorAll('code')].map((code) => code.textContent || '').slice(0, 8),
          }));
          return {
            active: document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '',
            expectedSession: sid,
            send: document.getElementById('send-btn')?.getAttribute('aria-label') || '',
            composer: document.getElementById('prompt-input')?.value || '',
            visibleCwd: visible?.querySelector('.msg--assistant')?.getAttribute('data-workspace-cwd') || '',
            markerInDocument: (document.body.textContent || '').includes(mark),
            chip,
            assistants,
          };
        }, { mark: rootMarker, chip: shortChip, sid: 'session-a-seed' });
      } catch (diagErr) {
        notes.push(`evaluate: ${(diagErr && diagErr.message) || diagErr}`);
      }
      try {
        await page.screenshot({ path: '/tmp/rpi-fl62-render.png', fullPage: true });
      } catch (diagErr) {
        notes.push(`screenshot: ${(diagErr && diagErr.message) || diagErr}`);
      }
      try {
        fs.writeFileSync('/tmp/rpi-fl62-render.json', `${JSON.stringify({ original, dump }, null, 2)}\n`);
      } catch (diagErr) {
        notes.push(`write: ${(diagErr && diagErr.message) || diagErr}`);
      }
      if (notes.length > 0) {
        console.error(`web-files-latency: FL6.2 render diagnostics capture failed: ${notes.join(' | ')}`);
      }
      fail(original);
    }
    const readsBeforeRoot = await outgoingReadCount();
    await visibleChip(rootMarker, shortChip).click();
    try {
      await waitFor(
        page,
        ({ before, exact, resolved }) => {
          const outs = window.__fxFrames.filter((frame) => frame.dir === 'out' && frame.type === 'file_explorer_read').slice(before);
          const exactAt = outs.findIndex((frame) => frame.path === exact);
          const resolvedAt = outs.findIndex((frame) => frame.path === resolved);
          if (exactAt < 0 || resolvedAt <= exactAt) return false;
          const incoming = window.__fxFrames.filter((frame) => frame.dir === 'in' && frame.type === 'file_explorer_read');
          const exactReply = incoming.find((frame) => frame.id === outs[exactAt].id);
          const resolvedReply = incoming.find((frame) => frame.id === outs[resolvedAt].id);
          const content = document.querySelector('[data-testid="file-explorer-content"]');
          return exactReply?.success === false
            && resolvedReply?.success === true
            && content != null
            && (content.textContent || '').includes('LINK-NESTED-EXACT')
            && content.querySelector('.file-explorer__error') == null;
        },
        'FL6.2: shortened citation did not miss the exact path and then open the rooted file',
        20000,
        { before: readsBeforeRoot, exact: nestedRel, resolved: rootedRel },
        { diagnostic: true },
      );
    } catch (error) {
      await fl6Dump('FL6.2', error);
    }
    record('FL6.2');
    const readsBeforeDup = await outgoingReadCount();
    await visibleChip(rootMarker, `${dupBase}:1`).click();
    await page.evaluate(() => new Promise((resolve) => setTimeout(resolve, 300)));
    const dupReads = (await readFrames()).filter((frame) => frame.dir === 'out').slice(readsBeforeDup);
    const dupContent = await openedContent();
    if (dupReads.some((frame) => frame.path === dupBase || frame.path.endsWith(`/${dupBase}`)) || dupContent.text.includes('LINK-DUP-') || !dupContent.text.includes('LINK-NESTED-EXACT')) {
      await fl6Dump('FL6.3', new Error(`duplicate basename read or opened a homonym ${JSON.stringify({ dupReads, dupContent })}`));
    }
    record('FL6.3');
    const readsBeforeOutside = await outgoingReadCount();
    await visibleChip(rootMarker, `${outsideAbs}:1`).click();
    await page.evaluate(() => new Promise((resolve) => setTimeout(resolve, 300)));
    const outsideReads = (await readFrames()).filter((frame) => frame.dir === 'out').slice(readsBeforeOutside);
    const outsideContent = await openedContent();
    if (outsideReads.length !== 0 || outsideContent.text.includes('LINK-OUTSIDE') || !outsideContent.text.includes('LINK-NESTED-EXACT')) {
      await fl6Dump('FL6.4', new Error(`outside citation read or opened a file ${JSON.stringify({ outsideReads, outsideContent })}`));
    }
    record('FL6.4');
    /* ---- FL6.5: root bare-basename citation (no path context) ----
       The workspace-root README.md already exists (seeded with the
       FL-ROOT-README-EXACT marker on line 2). The assistant fixture text
       carries ONLY the bare `README.md:2` token — no longer path context —
       so the render must stamp the exact basename and the click must open
       it through session-a-seed's correlated, session-stamped
       file_explorer_read with no file-unavailable error. */
    const readmeMarker = `root-readme-${laneNonce}`;
    const readmeChip = 'README.md:2';
    await page.fill('#prompt-input', `${readmeMarker}\nbare root readme cite \`README.md:2\` with no longer path context`);
    const readmeOwner = await page.evaluate(() =>
      document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '');
    if (readmeOwner !== 'session-a-seed') {
      fail(`FL6.5: prompt filled while ${readmeOwner} was active, not session-a-seed`);
    }
    await page.press('#prompt-input', 'Enter');
    try {
      await waitFor(
        page,
        ({ mark, chip, root, sid }) => {
          const active = document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '';
          const node = [...document.querySelectorAll('#transcript .transcript-surface:not([hidden]) .msg--assistant')]
            .find((el) => (el.textContent || '').includes(mark));
          const chipNode = node ? [...node.querySelectorAll('code')].find((code) => (code.textContent || '') === chip) : null;
          return active === sid
            && node != null
            && node.getAttribute('data-workspace-cwd') === root
            && (node.closest('[data-session-id]')?.getAttribute('data-session-id') || '') === sid
            && chipNode != null
            && chipNode.getAttribute('data-md-path') === 'README.md'
            && (chipNode.getAttribute('data-md-resolved') ?? '') !== 'none'
            && document.getElementById('send-btn')?.getAttribute('aria-label') === 'Send message';
        },
        'FL6.5: root bare-basename citation rendered inert (old data-md-resolved="none" render)',
        45000,
        { mark: readmeMarker, chip: readmeChip, root: workspaceRoot, sid: 'session-a-seed' },
        { diagnostic: true },
      );
    } catch (error) {
      await fl6Dump('FL6.5-render', error);
    }
    const readsBeforeReadme = await outgoingReadCount();
    await visibleChip(readmeMarker, readmeChip).click();
    try {
      await waitFor(
        page,
        ({ before, path, session, marker }) => {
          const outs = window.__fxFrames.filter((frame) => frame.dir === 'out' && frame.type === 'file_explorer_read').slice(before);
          const sent = outs.find((frame) => frame.path === path && frame.session === session);
          if (!sent) return false;
          const reply = window.__fxFrames.find((frame) => frame.dir === 'in' && frame.type === 'file_explorer_read' && frame.id === sent.id);
          const content = document.querySelector('[data-testid="file-explorer-content"]');
          return reply != null
            && reply.success === true
            && reply.path === path
            && content != null
            && (content.textContent || '').includes(marker)
            && content.querySelector('.file-explorer__error') == null;
        },
        'FL6.5: bare README.md:2 citation did not open the root README through the session-stamped correlated read',
        20000,
        { before: readsBeforeReadme, path: 'README.md', session: 'session-a-seed', marker: 'FL-ROOT-README-EXACT' },
        { diagnostic: true },
      );
    } catch (error) {
      await fl6Dump('FL6.5', error);
    }
    const readmeUnavailable = await page.evaluate(() => ({
      contentError: document.querySelector('[data-testid="file-explorer-content"] .file-explorer__error')?.textContent || '',
      unavailableToasts: [...document.querySelectorAll('#toasts .toast--error')]
        .filter((toast) => toast.getClientRects().length > 0)
        .map((toast) => (toast.textContent || '').trim())
        .filter((text) => text.includes('README.md') || text.includes('unavailable') || text.includes('不存在')),
    }));
    if (readmeUnavailable.contentError !== '' || readmeUnavailable.unavailableToasts.length > 0) {
      await fl6Dump('FL6.5', new Error(`FL6.5: file-unavailable error surfaced ${JSON.stringify(readmeUnavailable)}`));
    }
    record('FL6.5');
    await stageShot(page, 'files-latency-subdir-link.png');



    /* ---- wrap up: publish ONLY after every gate passes ---- */
    // Gate 1: the finite/nonnegative timing gate already ran on T1's derived
    // durations; re-verify nothing non-finite crept into the published shape.
    const publishedDurations = Object.entries(timings.toggles)
      .flatMap(([stage, entry]) => Object.entries(entry)
        .filter(([, value]) => typeof value === 'number')
        .map(([name, value]) => ({ name: `${stage}.${name}`, value })));
    const wrapTimingError = timingGateError(publishedDurations.map((d) => (
      d.name === 'firstExpand.commitMinusFirstAnimationFrameMs' ? { ...d, min: -1 } : d
    )));
    if (wrapTimingError !== null) fail(`wrap-up: ${wrapTimingError}`);
    // Gate 2: zero captured browser errors (console.error/pageerror/
    // websocket socketerror) FIRST — FL5.1 is part of the documented FL
    // ledger, so it MUST be recorded BEFORE the exact-once coverage gate
    // computes over the ledger (computing the gate before the record would
    // always see FL5.1 missing and false-fail the lane). Any captured error
    // still blocks publication.
    timings.browserErrors = { count: browserErrors.length, messages: browserErrors };
    if (browserErrors.length > 0) {
      fail(`wrap-up: ${browserErrors.length} captured browser error(s): ${browserErrors.join(' | ')}`);
    }
    record('FL5.1');
    // The REAL ledger: derived from the executedCounts Map itself — the
    // completeness gate above proved its insertion order elementwise-equal
    // to DOCUMENTED_IDS and every count exactly 1.
    const executedOrder = [...executedCounts.keys()];
    const counts = Object.fromEntries(executedCounts);
    const coverageError = documentedIdsCompletenessError(DOCUMENTED_IDS, executedCounts);
    const publicationError = publicationGateError({ coverageError, browserErrors });
    if (publicationError !== null) fail(`wrap-up: ${publicationError}`);
    timings.bigArrayLength = bigLength;
    timings.toggles.firstExpand.clickToCommitMs = Number(timings.toggles.firstExpand.clickToCommitMs.toFixed(3));
    stageWrite('files-latency-timings.json', `${JSON.stringify(timings, null, 2)}\n`);
    // Explicit exact assertion ledger: documented ids, the Map's ACTUAL
    // insertion order (real execution order — never a filtered
    // reconstruction of the documented list), and the real per-id counts —
    // published alongside the timings, never timings-only.
    stageWrite('coverage-assertions.json', `${JSON.stringify({
      documented: [...DOCUMENTED_IDS],
      executed: executedOrder,
      counts,
      browserErrors: { count: browserErrors.length, messages: browserErrors },
    }, null, 2)}\n`);
    // ALL gates passed: publish the staged set as ONE immutable run
    // directory plus ONE atomically-renamed manifest/current pointer —
    // never early, never partially, never via sequential canonical
    // replacement.
    publishStagedArtifacts({
      lane: 'web-files-latency',
      nonce: laneNonce,
      documented: [...DOCUMENTED_IDS],
      executed: executedOrder,
      counts,
      timings: JSON.parse(JSON.stringify(timings)),
    });
    console.log('web-files-latency: observed timings (reported, no invented pass threshold)');
    console.log(JSON.stringify(timings.toggles, null, 2));
    console.log(`web-files-latency: browser errors: ${JSON.stringify(timings.browserErrors)}`);
    console.log(`web-files-latency: evidence run ${runDirName} (pointer: ${path.join(evidence, 'current.json')})`);
    console.log('web-files-latency: PASS');
  } finally {
    await browser.close();
    // Anything still staged here was never published (publish renames the
    // staging dir away); drop it so no run-scoped staging survives.
    if (!published) cleanupStaging();
  }
}

if (process.env.RPI_SELF_CHECK === '1') {
  selfCheckStagedArtifactValidation();
  selfCheckPublicationFaults();
  // The fault-injection self-check ran against a throwaway evidence root;
  // remove it so a self-check leaves zero residue.
  fs.rmSync(evidence, { recursive: true, force: true });
  console.log('web-files-latency: self-check PASS');
} else {
  main().catch((err) => {
    console.error(`web-files-latency: playwright crashed: ${err && err.stack ? err.stack : err}`);
    // A crash (not a fail() assertion) must still leave zero staging residue
    // and zero published artifacts.
    if (!published) cleanupStaging();
    process.exit(2);
  });
}