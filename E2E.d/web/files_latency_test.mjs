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
// SUBDIRECTORY TRANSCRIPT LINKS (FL6.1-FL6.6, after A/B isolation):
// FL6.1 is the cwd <workspace>/pkg control and should open
// nested/link-target.txt. FL6.2 switches to session-a-seed (workspace root)
// and clicks the shortened nested/link-target.txt:1; the real file is
// pkg/nested/link-target.txt inside that root. FL6.3 one basename read
// returns both dup-name.txt copies as typed choices and opens neither.
// FL6.4 clicks the outside absolute citation and
// forbids a read/selection of outside/outside-only.txt; a path marker on
// that citation is allowed. The 1500-message parent is not the click surface.
// FL6.5 clicks a BARE root-basename citation README.md:2 — the assistant
// fixture text carries no longer path context, so the render must stamp the
// exact basename (data-md-path="README.md", never data-md-resolved="none")
// and the click must open the already-seeded root README.md through
// session-a-seed's correlated, session-stamped file_explorer_read with no
// file-unavailable error.
// FL6.6 cites bare `homonym.rs`. One owner read returns both full paths as
// typed choices and opens neither; the pkg/a choice then shows HOMONYM-A only.

// EXPLORER REVEAL (FL6.8 desktop 1440x900, FL6.9 mobile 390x844; riding the
// FL6.7 rewards.rs citation, READ-ONLY — no directory is expanded, no toggle
// is clicked, and no scrollIntoView compensates): after the citation jump the
// tree carries EXACTLY ONE selected row whose `title` is the full
// workspace-relative path (visible label stays the basename), every ancestor
// directory row is aria-expanded="true", and on desktop the drawer stays open
// with that row inside the tree scroll viewport. The top of the selected-file
// content — every visible element above the file body — must carry the same
// full path; a basename-only header can never pass. On the compact mobile pane
// (tree collapsed by design) only the DOM selection identity is asserted, and
// the content path is checked on the visible content pane. Failures park
// files-latency-reveal-{tree,content}-<breakpoint>.png plus the raw DOM sample
// in the evidence root before exit 2.

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
// EXPLORER OCCLUSION (FL0.7 desktop 1440x900, FL0.8 mobile 390x844): with
// Files open, elementFromPoint inside the explorer rect must not hit
// #sidebar-manage-btn or .session-sidebar__foot. With a non-preview file
// (not preview.txt; .md/.html/.svg are previewable) and the tree not
// collapsed, the source pane's top must not be an empty 32px content
// toolbar, and — the content top now being the visible path strip
// ([data-testid=file-explorer-content-path]) — the source must start exactly
// at that strip's visible bottom (no gap, no overlap; ±0.5px). The strip's
// full-path text is asserted by FL6.8/FL6.9 on the citation open. MAIN
// COMPOSER HIDDEN: with Files open the ENTIRE main composer
// footer wrapper — #prompt-input, the #composer-main row (slash picker,
// context meter, attachments), mic, live, and Send — must be invisible,
// unhittable, and unfocusable (no Tab reach); the file Ask dock stays
// independently usable (real Ask round trip + dock hit targets). Closing
// Files must restore the composer: the pre-open draft survives, focus
// lands, and typing works (composer-after-close below).
// CORNER LEAK (FL0.9 desktop only): interior occlusion probes can never see
// the reported bottom-left leak — the New Session control sits BELOW the
// explorer's bottom edge, outside the explorer rect. The New Session/foot
// rects are captured with Files CLOSED at 1440x900 (the real open Sessions
// rail, immediately before Files is reopened over them); with Files open the
// explorer rect must COVER those captured corner points (no disjoint bottom
// edge), each point must resolve INTO the open explorer (no z-order pierce;
// the live rail being unmounted/hidden under Files is valid, but a missing
// pre-open baseline is a failure), and #prompt-input must stay
// HIDDEN/unhittable — the main composer is never left visible beside the
// drawer.
// ASK FULLSCREEN (same FL0.7 desktop and FL0.8 mobile records, not a new
// lane): opening a file mounts section.file-conversation. #file-explorer-focus-btn
// (fileExplorerControlId(file-explorer-toggle-btn, 'focus-btn')) must be a
// visible hittable toggle. Entering fullscreen adds file-explorer--ask-focus,
// hides the file tree and selected-file content, and lets the dock fill the
// explorer body while the #root > header stays disjoint, the MAIN composer
// stays hidden (never the elementFromPoint target), and the dock/body
// bottoms sit ON the layout viewport bottom (the hidden footer leaves no
// band).
// The button, Escape, and a close/reopen each clear the class; Escape does
// not close the drawer. FL6.6 and FL6.7 reuse this check on their already
// opened non-empty bodies (HOMONYM-A vs HOMONYM-B, REWARDS-NODE-A vs
// REWARDS-NODE-FIX-B) so an empty diff cannot pass as either file.
// EXPLORER BOTTOM / DOCK / SCOPE (same FL0.7 / FL0.8 records; the check also
// rides the FL6.6 / FL6.7 fullscreen reuse): with a real non-empty file open
// the explorer's flex body bottom must sit ON the layout viewport bottom
// (±1px — no extra outer whitespace band, no underlap); in ask-focus
// the dock, body, and viewport bottom edges must ALL coincide (±1px, both
// directions); on mobile 390x844 the Ask
// dock is a bounded conversation dock (height <= min(280, body*0.45)+1) while
// the visible tree/content viewport stays LARGER than the dock, the scope
// header, textarea, and Send/Stop stay fully inside the dock, and each is the
// elementFromPoint target at its center; in every phase the dock shows a
// "File conversation" scope header whose path equals the active file, and an
// empty non-streaming conversation shows the quiet empty-state message that
// disappears once entries exist and never shows while streaming.
// EMPTY DIFF (same FL0.7 / FL0.8, last step, after the cold-open and
// Refresh timings): the only diff control is a changed-row click or its
// "Open diff for …" chip. README.md is the seeded clean tracked file and
// has neither, so clicking it reads the file; this lane does NOT claim a
// browser empty-diff "No changes" pass. The reachable check opens the
// existing dirty mod6/sub0/file-0.ts diff (non-empty, and not "No changes"
// or "Empty file"), then one real click on [data-testid=file-explorer-open-file]
// ("Open file") must send exactly one session-stamped file_explorer_read
// for that relative path and show the file body.

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
import { clickFeature, openFeatureMenu, openSessionsDrawer } from './lib/feature-menu.mjs';

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
let tabTraversalSeq = 0; // per-traversal nonce source (see the Tab journey below)

async function stageShot(page, name) {
  fs.mkdirSync(stagingDir, { recursive: true });
  const staged = path.join(stagingDir, name);
  await page.screenshot({ path: staged, fullPage: true });
  laneArtifacts.push(name);
  return staged;
}
async function journeyFrame(page, name, caption) {
  const file = path.join(evidence, name);
  await page.screenshot({ path: file, fullPage: true });
  console.log(`[web-files-latency:frame] ${name} ${caption}`);
}

// RED diagnostics ONLY (never a pass path): a failed reveal assertion parks
// the full-page frame AND the raw read-only DOM sample in the evidence ROOT —
// outside the staging set fail() removes, so cleanupStaging() cannot erase
// them — then fails the lane.
async function revealFailure(page, label, name, sample, message) {
  const notes = [];
  try {
    await journeyFrame(page, name, `${label} explorer reveal failure`);
  } catch (error) {
    notes.push(`screenshot: ${(error && error.message) || error}`);
  }
  try {
    fs.writeFileSync(
      path.join(os.tmpdir(), `rpi-reveal-${name.replace(/\.png$/, '')}.json`),
      `${JSON.stringify({ label, message, sample }, null, 2)}\n`,
    );
  } catch (error) {
    notes.push(`sample: ${(error && error.message) || error}`);
  }
  if (notes.length > 0) console.error(`web-files-latency: ${label} reveal diagnostics capture failed: ${notes.join(' | ')}`);
  fail(message);
}

// RED diagnostics ONLY (never a pass path): a focus-order walk that never
// cycles dumps its trace and ONE full-page frame into the evidence ROOT —
// outside the staging set fail() removes, so cleanupStaging() cannot erase
// them. Nodes are summarized by tag/id/class/geometry only, never text.
async function dumpFocusTrace(page, label, payload) {
  const tag = label.split(/\s+/)[0] || 'desktop';
  try {
    fs.writeFileSync(path.join(evidence, `files-latency-focus-${tag}.json`), `${JSON.stringify(payload, null, 2)}\n`);
    await journeyFrame(page, `files-latency-focus-${tag}.png`, 'focus-order failure frame');
  } catch (error) {
    console.log(`[web-files-latency:diag] focus trace dump failed: ${(error && error.message) || error}`);
  }
}

// Routine acceptance frames go through the SAME staged publication as every
// registered artifact (stageShot); the unregistered journeyFrame bypass
// above stays ONLY for failure diagnostics a RED must leave inspectable.
async function stageFrame(page, name, caption) {
  await stageShot(page, name);
  console.log(`[web-files-latency:frame] ${name} ${caption}`);
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

async function waitFor(page, fn, label, timeoutMs = 30000, arg, { diagnostic = false, onSample } = {}) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    try {
      const result = await page.evaluate(fn, arg);
      if (onSample) onSample(result);
      if (onSample ? result.matched : result) return;
    } catch (error) {
      throw new Error(`${label}: predicate evaluation failed: ${error instanceof Error ? error.message : String(error)}`);
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
const EXPLORER_TOOLBAR_H = 32;
const PREVIEW_FILE_RE = /\.(md|markdown|html|htm|svg)$/i;


// Returns null when the open explorer does not yield the New Session control
// or the sidebar footer to elementFromPoint. A missing explorer, a missing
// control, or any interior point that hits those nodes is a failure — the
// sample must name the point so a pass cannot be an empty scan.
function explorerOcclusionError(label, sample) {
  if (!sample || sample.explorer == null) return `${label}: file explorer is not open`;
  if (!sample.button && !sample.foot) return `${label}: neither #sidebar-manage-btn nor .session-sidebar__foot is mounted`;
  const probes = Array.isArray(sample.probes) ? sample.probes : [];
  if (probes.length === 0) return `${label}: no elementFromPoint probes inside the explorer`;
  const pierced = probes.filter((probe) => probe.sidebarFoot === true);
  if (pierced.length > 0) {
    const first = pierced[0];
    return `${label}: explorer elementFromPoint(${first.x},${first.y}) hit ${first.hit} (${pierced.length} sidebar-foot hits)`;
  }
  return null;
}

// CORNER LEAK gate (FL0.9, desktop): the interior occlusion scan above
// samples only points inside the explorer rect, so a leak that lives BELOW
// the explorer's bottom edge (the bottom-left New Session corner) is
// structurally invisible to it. This gate probes the corner itself using the
// REAL rects captured with Files closed (the pre-open Sessions rail), and
// separates the two failure modes the fix must distinguish: a geometry gap
// (the explorer rect does not even cover the captured corner point) and a
// z-order pierce (the corner point is covered by the explorer rect but
// elementFromPoint still resolves to the sidebar foot). The captured point
// must also actually hit the open explorer — the live rail no longer being
// mounted/hittable while Files covers it is valid, a missing baseline is not.
// The MAIN composer must stay HIDDEN while Files is open.
function explorerCornerLeakError(label, sample) {
  if (!sample || sample.explorer == null) return `${label}: file explorer is not open`;
  if (!sample.button && !sample.foot) return `${label}: no pre-open sidebar corner baseline was captured`;
  const probes = Array.isArray(sample.probes) ? sample.probes : [];
  if (probes.length === 0) return `${label}: no corner elementFromPoint probes were sampled`;
  for (const probe of probes) {
    if (probe.insideExplorer !== true) {
      return `${label}: explorer rect ${JSON.stringify(sample.explorer)} does not cover the ${probe.target} corner point (${probe.x},${probe.y}) — the bottom edge never overlaps the sidebar foot`;
    }
    if (probe.sidebarFoot === true) {
      return `${label}: corner point (${probe.x},${probe.y}) hit ${probe.hit} instead of the open explorer`;
    }
    if (probe.hitExplorer !== true) {
      return `${label}: corner point (${probe.x},${probe.y}) hit ${probe.hit} instead of the open explorer`;
    }
  }
  if (sample.composerHittable !== false) return `${label}: #prompt-input is still hittable while Files is open — the MAIN composer must stay hidden`;
  return null;
}

// MAIN COMPOSER HIDDEN (FL0.7 desktop / FL0.8 mobile, riding the explorer
// journey): with Files open the ENTIRE main composer footer wrapper — the
// prompt textarea, the slash-command/context/attachment row (#composer-main),
// mic, live, and Send — must be invisible (per control, not just the
// wrappers: a zero-rect wrapper can still leak overflow-visible children),
// never the elementFromPoint target, and must not take programmatic focus
// (the focus() probe; not a full Tab traversal). The file Ask dock stays
// independently usable: when the dock is mounted its textarea and
// Send/Stop must still be the real hit targets at their centers (the real
// Ask round trip proves the end-to-end path). Callers that run after a
// file was opened pass requireAsk so a missing dock can never let the
// hidden-composer gate pass vacuously. Closing Files must restore the
// composer — draft, focus, and typing are asserted by
// composerAfterExplorerCloseError below.
function mainComposerHiddenError(label, sample, { requireAsk = false } = {}) {
  if (!sample || sample.explorerOpen !== true) return `${label}: file explorer is not open`;
  if (sample.footerVisible === true) return `${label}: the main composer FOOTER WRAPPER is visible while Files is open`;
  if (sample.composerMainVisible === true) return `${label}: #composer-main is visible while Files is open`;
  if (sample.composerVisible === true) return `${label}: #prompt-input is visible while Files is open`;
  const controls = sample.controls || {};
  const ids = Object.keys(controls);
  if (ids.length === 0) return `${label}: no main composer controls were sampled`;
  for (const id of ids) {
    if (controls[id].visible === true) return `${label}: #${id} is still rendered/visible while Files is open`;
    if (controls[id].hittable === true) return `${label}: #${id} is still the elementFromPoint target while Files is open`;
    if (controls[id].focusable === true) return `${label}: #${id} still takes focus while Files is open (not hidden from focus reach)`;
  }
  if (requireAsk && !sample.ask) return `${label}: the file Ask dock is not mounted while Files is open (requireAsk)`;
  if (sample.ask) {
    if (sample.ask.textareaHit !== true) return `${label}: the file Ask textarea is not hittable while Files is open`;
    if (sample.ask.sendHit !== true) return `${label}: the file Ask Send/Stop is not hittable while Files is open`;
  }
  return null;
}

async function readMainComposerOcclusion(page) {
  return page.evaluate(() => {
    const section = document.querySelector('section.file-explorer');
    const visibleOf = (node) => {
      if (!node) return false;
      const rect = node.getBoundingClientRect();
      const style = getComputedStyle(node);
      return rect.width > 0 && rect.height > 0 && style.display !== 'none' && style.visibility !== 'hidden';
    };
    const hitOf = (node) => {
      const rect = node?.getBoundingClientRect();
      if (!rect || rect.width < 1 || rect.height < 1) return false;
      const hit = document.elementFromPoint(rect.x + rect.width / 2, rect.y + Math.min(12, rect.height / 2));
      return Boolean(hit && (hit === node || node.contains?.(hit)));
    };
    const focusableOf = (node) => {
      if (!node) return false;
      const before = document.activeElement;
      let took = false;
      try {
        node.focus();
        took = document.activeElement === node;
      } finally {
        try {
          if (before && before !== node && before.focus) before.focus();
          else node.blur();
        } catch { /* a restore failure must never mask the probe result */ }
      }
      return took;
    };
    const composer = document.getElementById('prompt-input');
    const controls = {};
    for (const id of ['prompt-input', 'command-btn', 'attach-btn', 'mic-btn', 'live-btn', 'send-btn']) {
      const node = document.getElementById(id);
      controls[id] = {
        mounted: node != null,
        visible: visibleOf(node),
        hittable: hitOf(node),
        focusable: focusableOf(node),
      };
    }
    const dock = document.querySelector('section.file-conversation');
    const ask = dock
      ? {
          textareaHit: hitOf(dock.querySelector('textarea.file-conversation__input')),
          sendHit: hitOf(dock.querySelector('.file-conversation__action--send, .file-conversation__action--stop')),
        }
      : null;
    return {
      explorerOpen: section != null,
      footerVisible: visibleOf(composer?.closest('footer')),
      composerMainVisible: visibleOf(document.getElementById('composer-main')),
      composerVisible: visibleOf(composer),
      controls,
      ask,
    };
  });
}

// BACKGROUND REPLY REACHABILITY: the visible transcript's settled reply
// actions are the bulk of the focus order underneath the Files overlay. Every
// mounted strip must compute visibility:hidden and refuse focus while Files
// covers the transcript, then compute visible and take focus again after
// Files closes. Real DOM only (computed style + activeElement), and an empty
// strip set fails so neither direction can pass vacuously.
async function readBackgroundReplyReachability(page) {
  return page.evaluate(() => {
    const actions = [...document.querySelectorAll('#transcript .transcript-surface:not([hidden]) .assistant-reply-action')];
    const visibilities = actions.map((node) => getComputedStyle(node).visibility);
    const rendered = actions.filter((node) => node.getClientRects().length > 0);
    const before = document.activeElement;
    const first = rendered[0] || actions[0];
    let focusable = false;
    if (first) {
      try {
        first.focus({ preventScroll: true });
        focusable = document.activeElement === first;
      } finally {
        try {
          if (before && before !== first && typeof before.focus === 'function') before.focus({ preventScroll: true });
        } catch { /* a restore failure must never mask the probe result */ }
        if (document.activeElement === first) {
          try { first.blur(); } catch { /* same: probe result already captured */ }
        }
      }
    }
    return {
      explorerOpen: document.querySelector('section.file-explorer') != null,
      count: actions.length,
      renderedCount: rendered.length,
      visible: visibilities.length > 0 && visibilities.every((value) => value === 'visible'),
      hidden: visibilities.length > 0 && visibilities.every((value) => value === 'hidden'),
      visibility: visibilities[0] || '',
      focusable,
    };
  });
}

function backgroundReplyReachabilityError(label, sample, reachable) {
  if (!sample || sample.explorerOpen === reachable) {
    return `${label}: file explorer is ${sample && sample.explorerOpen ? 'still open' : 'not open'} during the ${reachable ? 'restore' : 'hide'} probe`;
  }
  if (!(sample.count > 0)) return `${label}: no reply actions under the visible transcript (vacuous probe)`;
  if (reachable ? sample.visible !== true : sample.hidden !== true) {
    return `${label}: ${sample.count} reply actions compute visibility=${sample.visibility} (expected every ${reachable ? 'visible' : 'hidden'})`;
  }
  if (reachable && !(sample.renderedCount > 0)) {
    return `${label}: no laid-out reply action to focus after Files closed (${sample.count} mounted)`;
  }
  if (sample.focusable !== reachable) {
    return `${label}: reply action focus probe expected ${reachable}, got ${sample.focusable}`;
  }
  return null;
}

// File view only, and only when that view has no action. A diff toolbar
// that holds Open file is a real control, not the empty 32px bar. The content
// TOP is now the user-requested visible path strip
// (`[data-testid="file-explorer-content-path"]`), so with no tree toggle and
// no Open file button the source must start EXACTLY at that strip's visible
// bottom — no gap and no overlap (±0.5px). The strip itself must be rendered
// and visible, the content's padding-top stays 0, and the content viewport's
// computed border-top must still be measurable (a real border, never a gap
// allowance). On mobile the tree toggle may occupy the toolbar while the tree
// stays collapsed; an empty bar is still a failure.
function sourceTopGapError(label, sample) {
  if (!sample || sample.path == null || sample.path === '') return `${label}: no non-preview file is open`;
  const base = sample.path.split('/').pop() || '';
  if (base === 'preview.txt' || PREVIEW_FILE_RE.test(base)) {
    return `${label}: opened ${sample.path}, which is previewable or preview.txt`;
  }
  if (sample.contentHidden === true) return `${label}: source content is hidden`;
  if (typeof sample.sourceOffset !== 'number' || !Number.isFinite(sample.sourceOffset)) {
    return `${label}: source pane top was not measured`;
  }
  if (typeof sample.borderTopWidth !== 'number' || !Number.isFinite(sample.borderTopWidth)) {
    return `${label}: content border-top was not measured`;
  }
  if (typeof sample.paddingTop !== 'number' || !Number.isFinite(sample.paddingTop)) {
    return `${label}: content padding-top was not measured`;
  }
  if (sample.diffView === true || sample.openFileAction === true) {
    return `${label}: file-view padding was measured on the diff Open file toolbar`;
  }
  const mobile = sample.layout === 'content';
  if (!mobile && sample.treeCollapsed !== false) {
    return `${label}: tree is collapsed, so the toolbar may hold the tree toggle`;
  }
  const treeControl = sample.toolbarTreeToggle === true;
  if (sample.paddingTop > 0.5) {
    return `${label}: content padding-top is ${sample.paddingTop}px`;
  }
  if (sample.toolbarChildren === 0 && sample.toolbarHeight >= EXPLORER_TOOLBAR_H) {
    return `${label}: empty content toolbar is ${sample.toolbarHeight}px above ${sample.path}`;
  }
  if (sample.pathStripPresent !== true) {
    return `${label}: the content top path strip is not rendered above ${sample.path}`;
  }
  if (sample.pathStripVisible !== true) {
    return `${label}: the content top path strip has no visible box`;
  }
  if (typeof sample.pathStripBottom !== 'number' || !Number.isFinite(sample.pathStripBottom)) {
    return `${label}: the content top path strip bottom was not measured`;
  }
  if (typeof sample.sourceTop !== 'number' || !Number.isFinite(sample.sourceTop)) {
    return `${label}: source pane top was not measured`;
  }
  if (!treeControl) {
    const gap = sample.sourceTop - sample.pathStripBottom;
    if (Math.abs(gap) > 0.5) {
      return `${label}: file view source starts ${gap}px ${gap > 0 ? 'below' : 'above'} the visible path strip bottom (source top ${sample.sourceTop}px, strip bottom ${sample.pathStripBottom}px, content-relative offset ${sample.sourceOffset}px, content border-top ${sample.borderTopWidth}px)`;
    }
  }
  return null;
}


// After Files closes, the composer must be the elementFromPoint target, be
// focusable, accept a typed character, and keep the draft that was written
// before the close.
function composerAfterExplorerCloseError(label, sample) {
  if (!sample || sample.explorerGone !== true) return `${label}: file explorer is still open`;
  if (sample.draftBefore == null || sample.draftBefore === '') return `${label}: no draft was written before close`;
  if (typeof sample.width !== 'number' || typeof sample.height !== 'number' || sample.width < 1 || sample.height < 1) {
    return `${label}: #prompt-input has no geometry`;
  }
  if (sample.hitPrompt !== true) return `${label}: elementFromPoint hit ${sample.hit || 'nothing'} instead of #prompt-input`;
  if (sample.focused !== true) return `${label}: #prompt-input did not take focus`;
  if (sample.typed !== true) return `${label}: typing into #prompt-input did not land`;
  if (!String(sample.draftAfter || '').startsWith(sample.draftBefore)) {
    return `${label}: draft was not retained (before ${JSON.stringify(sample.draftBefore)}, after ${JSON.stringify(sample.draftAfter)})`;
  }
  return null;
}

function headerClipError(label, sample) {
  const probe = sample && sample.scroll ? ` scrollY=${sample.scroll.scrollY} scrollingTop=${sample.scroll.scrollingTop} root=${JSON.stringify(sample.scroll.root)} body=${JSON.stringify(sample.scroll.body)}` : '';
  if (!sample || sample.missing === 1) return `${label}: app header missing${probe}`;
  if (typeof sample.top !== 'number' || sample.top < 0) return `${label}: header top ${sample.top}${probe}`;
  if (typeof sample.height !== 'number' || sample.height < 1) return `${label}: header height ${sample.height}${probe}`;
  if (sample.clip === 1) return `${label}: header top clipped by ancestor${probe}`;
  if (sample.hitHeader !== 1) return `${label}: header top band missed${probe}`;
  return null;
}

function selfCheckExplorerJourneyGates() {
  const occluded = {
    explorer: { top: 48, bottom: 843 },
    button: { top: 842, bottom: 886 },
    foot: { top: 833, bottom: 892 },
    probes: [{ x: 40, y: 842.5, sidebarFoot: true, hit: '#sidebar-manage-btn' }],
  };
  if (explorerOcclusionError('desktop', occluded) === null) {
    throw new Error('occlusion self-check accepted a New Session pierce');
  }
  const clear = {
    explorer: { top: 48, bottom: 843 },
    button: { top: 850, bottom: 894 },
    foot: { top: 844, bottom: 900 },
    probes: [{ x: 40, y: 800, sidebarFoot: false, hit: '.file-explorer' }],
  };
  if (explorerOcclusionError('desktop', clear) !== null) {
    throw new Error('occlusion self-check rejected a disjoint explorer');
  }
  const cornerGap = {
    explorer: { x: 0, y: 48, width: 1440, height: 796 },
    button: { x: 8, y: 856, width: 214, height: 30 },
    foot: { x: 0, y: 844, width: 230, height: 56 },
    probes: [{ target: 'button', x: 115, y: 871, sidebarFoot: true, insideExplorer: false, hit: '#sidebar-manage-btn' }],
    composerHittable: false,
  };
  if (explorerCornerLeakError('desktop', cornerGap) === null) {
    throw new Error('corner self-check accepted a bottom edge that never covers the sidebar foot');
  }
  const cornerPierce = {
    explorer: { x: 0, y: 48, width: 1440, height: 852 },
    button: { x: 8, y: 856, width: 214, height: 30 },
    foot: { x: 0, y: 844, width: 230, height: 56 },
    probes: [{ target: 'button', x: 115, y: 871, sidebarFoot: true, insideExplorer: true, hit: '#sidebar-manage-btn' }],
    composerHittable: false,
  };
  if (explorerCornerLeakError('desktop', cornerPierce) === null) {
    throw new Error('corner self-check accepted a sidebar-foot z-order pierce');
  }
  const cornerComposerVisible = {
    explorer: { x: 0, y: 48, width: 1440, height: 852 },
    button: { x: 8, y: 856, width: 214, height: 30 },
    foot: { x: 0, y: 844, width: 230, height: 56 },
    probes: [{ target: 'button', x: 115, y: 871, sidebarFoot: false, insideExplorer: true, hitExplorer: true, hit: 'section.file-explorer' }],
    composerHittable: true,
  };
  if (explorerCornerLeakError('desktop', cornerComposerVisible) === null) {
    throw new Error('corner self-check accepted a visible main composer while Files is open');
  }
  const cornerClean = {
    explorer: { x: 0, y: 48, width: 1440, height: 852 },
    button: { x: 8, y: 856, width: 214, height: 30 },
    foot: { x: 0, y: 844, width: 230, height: 56 },
    probes: [
      { target: 'button', x: 115, y: 871, sidebarFoot: false, insideExplorer: true, hitExplorer: true, hit: 'section.file-explorer' },
      { target: 'foot', x: 115, y: 872, sidebarFoot: false, insideExplorer: true, hitExplorer: true, hit: 'section.file-explorer' },
    ],
    composerHittable: false,
  };
  if (explorerCornerLeakError('desktop', cornerClean) !== null) {
    throw new Error('corner self-check rejected a covered, pierce-free corner with the composer hidden');
  }
  const cornerNoBaseline = { ...cornerClean, button: null, foot: null, probes: [] };
  if (explorerCornerLeakError('desktop', cornerNoBaseline) === null) {
    throw new Error('corner self-check accepted a corner sample with no pre-open sidebar baseline');
  }
  const cornerNoHit = { ...cornerClean, probes: [{ target: 'button', x: 115, y: 871, sidebarFoot: false, insideExplorer: true, hitExplorer: false, hit: '.app-main' }] };
  if (explorerCornerLeakError('desktop', cornerNoHit) === null) {
    throw new Error('corner self-check accepted a corner point that misses the open explorer');
  }
  // A REAL measurement, never asserted as a constant: the visible path strip
  // (its box, including its own border) sits below the content's top border.
  const STRIP_BOTTOM = 30.19;
  const strip = {
    pathStripPresent: true,
    pathStripVisible: true,
    pathStripBottom: STRIP_BOTTOM,
    sourceTop: STRIP_BOTTOM,
  };
  const emptyBar = {
    path: 'mod7/sub0/file-0.ts',
    layout: 'split',
    treeCollapsed: false,
    contentHidden: false,
    toolbarChildren: 0,
    toolbarTreeToggle: false,
    toolbarHeight: 32,
    sourceOffset: 32,
    borderTopWidth: 1,
    paddingTop: 0,
    ...strip,
  };
  if (sourceTopGapError('desktop', emptyBar) === null) {
    throw new Error('source-top self-check accepted an empty 32px toolbar');
  }
  const flush = { ...emptyBar, toolbarHeight: 0, sourceOffset: STRIP_BOTTOM };
  if (sourceTopGapError('desktop', flush) !== null) {
    throw new Error(`source-top self-check rejected a source flush with the visible path strip bottom (${sourceTopGapError('desktop', flush)})`);
  }
  if (sourceTopGapError('desktop', { ...flush, sourceTop: STRIP_BOTTOM + 30.1875 }) === null) {
    throw new Error('source-top self-check accepted a 30.1875px gap below the visible path strip');
  }
  if (sourceTopGapError('desktop', { ...flush, sourceTop: STRIP_BOTTOM - 1 }) === null) {
    throw new Error('source-top self-check accepted source/strip overlap');
  }
  if (sourceTopGapError('desktop', { ...flush, pathStripPresent: false }) === null) {
    throw new Error('source-top self-check accepted a missing content top path strip');
  }
  if (sourceTopGapError('desktop', { ...flush, pathStripVisible: false }) === null) {
    throw new Error('source-top self-check accepted an invisible content top path strip');
  }
  if (sourceTopGapError('desktop', { ...flush, pathStripBottom: null }) === null) {
    throw new Error('source-top self-check accepted an unmeasured path strip bottom');
  }
  if (sourceTopGapError('desktop', { ...flush, sourceTop: null }) === null) {
    throw new Error('source-top self-check accepted an unmeasured source top');
  }
  if (sourceTopGapError('desktop', { ...flush, paddingTop: 4 }) === null) {
    throw new Error('source-top self-check accepted content padding-top');
  }
  const mobileTree = {
    ...flush,
    layout: 'content',
    treeCollapsed: true,
    toolbarChildren: 1,
    toolbarTreeToggle: true,
    toolbarHeight: 32,
    sourceOffset: STRIP_BOTTOM + 32,
    sourceTop: STRIP_BOTTOM + 32,
  };
  if (sourceTopGapError('mobile', mobileTree) !== null) {
    throw new Error('source-top self-check rejected a mobile tree toggle');
  }
  const openFileBar = {
    ...flush,
    toolbarChildren: 1,
    toolbarHeight: 33,
    sourceOffset: STRIP_BOTTOM + 33,
    sourceTop: STRIP_BOTTOM + 33,
    diffView: true,
    openFileAction: true,
  };
  const openFileError = sourceTopGapError('desktop', openFileBar);
  if (openFileError === null || openFileError.includes('empty content toolbar')) {
    throw new Error('source-top self-check treated the Open file toolbar as an empty bar');
  }
  if (sourceTopGapError('desktop', { ...flush, path: 'preview.txt' }) === null) {
    throw new Error('source-top self-check accepted preview.txt');
  }
  const lost = {
    explorerGone: true,
    draftBefore: 'fx-draft',
    width: 0,
    height: 0,
    hitPrompt: false,
    hit: '',
    focused: false,
    typed: false,
    draftAfter: '',
  };
  if (composerAfterExplorerCloseError('desktop', lost) === null) {
    throw new Error('composer self-check accepted a missing prompt');
  }
  const kept = {
    explorerGone: true,
    draftBefore: 'fx-draft',
    width: 400,
    height: 44,
    hitPrompt: true,
    hit: '#prompt-input',
    focused: true,
    typed: true,
    draftAfter: 'fx-draftZ',
  };
  if (composerAfterExplorerCloseError('desktop', kept) !== null) {
    throw new Error('composer self-check rejected a focused retained draft');
  }
  const unfocused = { ...kept, focused: false, typed: false, draftAfter: kept.draftBefore };
  if (composerAfterExplorerCloseError('desktop', unfocused) === null) {
    throw new Error('composer self-check accepted a composer the product did not auto-focus');
  }
  const hiddenClean = {
    explorerOpen: true,
    footerVisible: false,
    composerMainVisible: false,
    composerVisible: false,
    controls: {
      'prompt-input': { mounted: true, visible: false, hittable: false, focusable: false },
      'attach-btn': { mounted: true, visible: false, hittable: false, focusable: false },
      'mic-btn': { mounted: true, visible: false, hittable: false, focusable: false },
      'live-btn': { mounted: true, visible: false, hittable: false, focusable: false },
      'send-btn': { mounted: true, visible: false, hittable: false, focusable: false },
    },
    ask: { textareaHit: true, sendHit: true },
  };
  if (mainComposerHiddenError('desktop', hiddenClean) !== null) {
    throw new Error('composer-hidden self-check rejected a fully hidden composer with a live Ask dock');
  }
  if (mainComposerHiddenError('desktop', { ...hiddenClean, ask: null }) !== null) {
    throw new Error('composer-hidden self-check rejected an unmounted Ask dock sample');
  }
  if (mainComposerHiddenError('desktop', { ...hiddenClean, composerVisible: true }) === null) {
    throw new Error('composer-hidden self-check accepted a visible #prompt-input');
  }
  if (mainComposerHiddenError('desktop', { ...hiddenClean, footerVisible: true }) === null) {
    throw new Error('composer-hidden self-check accepted a visible footer wrapper');
  }
  if (mainComposerHiddenError('desktop', { ...hiddenClean, composerMainVisible: true }) === null) {
    throw new Error('composer-hidden self-check accepted a visible #composer-main');
  }
  const hittableSend = { ...hiddenClean, controls: { ...hiddenClean.controls, 'send-btn': { mounted: true, visible: false, hittable: true, focusable: false } } };
  if (mainComposerHiddenError('desktop', hittableSend) === null) {
    throw new Error('composer-hidden self-check accepted a hittable Send while Files is open');
  }
  const tabReachable = { ...hiddenClean, controls: { ...hiddenClean.controls, 'prompt-input': { mounted: true, visible: false, hittable: false, focusable: true } } };
  if (mainComposerHiddenError('desktop', tabReachable) === null) {
    throw new Error('composer-hidden self-check accepted a focusable prompt while Files is open');
  }
  const deadAsk = { ...hiddenClean, ask: { textareaHit: false, sendHit: true } };
  if (mainComposerHiddenError('desktop', deadAsk) === null) {
    throw new Error('composer-hidden self-check accepted an unhittable Ask textarea');
  }
  if (mainComposerHiddenError('desktop', { ...hiddenClean, explorerOpen: false }) === null) {
    throw new Error('composer-hidden self-check accepted a closed explorer');
  }
  if (mainComposerHiddenError('desktop', { ...hiddenClean, controls: {} }) === null) {
    throw new Error('composer-hidden self-check accepted an empty control sample');
  }
  const visibleLeak = { ...hiddenClean, controls: { ...hiddenClean.controls, 'mic-btn': { mounted: true, visible: true, hittable: false, focusable: false } } };
  if (mainComposerHiddenError('desktop', visibleLeak) === null) {
    throw new Error('composer-hidden self-check accepted a rendered (even unhittable) control while Files is open');
  }
  if (mainComposerHiddenError('desktop', { ...hiddenClean, ask: null }) !== null) {
    throw new Error('composer-hidden self-check rejected an unmounted Ask dock without requireAsk');
  }
  if (mainComposerHiddenError('desktop', { ...hiddenClean, ask: null }, { requireAsk: true }) === null) {
    throw new Error('composer-hidden self-check accepted a missing Ask dock with requireAsk');
  }
}
// EXPLORER BOTTOM AUTHORITY: with Files open on a real file, the explorer
// flex body must end EXACTLY at the LAYOUT VIEWPORT bottom (±1px) — the
// main composer footer is hidden while Files is open, so there is no
// footer band left to stop above. This rejects both an extra outer
// whitespace band (body.bottom < viewport bottom) and any underlap
// (body.bottom > viewport bottom).
function explorerBottomAlignmentError(label, sample) {
  if (!Number.isFinite(sample.bodyBottom) || !Number.isFinite(sample.viewportBottom)) {
    return `${label}: explorer body/viewport geometry was not sampled`;
  }
  if (Math.abs(sample.bodyBottom - sample.viewportBottom) > 1) {
    return `${label}: explorer body bottom ${sample.bodyBottom} does not sit on the layout viewport bottom ${sample.viewportBottom} (±1px)`;
  }
  return null;
}

// MOBILE BOUNDED DOCK (narrow only): the Ask dock is a capped conversation
// dock, not a second equal main viewport. Its height must stay within
// min(280, body*0.45)+1, the visible file viewport must be LARGER than the
// dock (an equal split or a dock-dominated layout rejects), the scope
// header, textarea, and Send/Stop action must sit fully inside the dock,
// and each control (plus the viewport itself) must be the real
// elementFromPoint target at its center.
function mobileBoundedDockError(label, sample) {
  if (sample.narrow !== true) return null;
  if (!Number.isFinite(sample.bodyHeight) || !Number.isFinite(sample.dockHeight)) {
    return `${label}: mobile dock/body heights were not sampled`;
  }
  const bound = Math.min(280, sample.bodyHeight * 0.45);
  if (sample.dockHeight > bound + 1) {
    return `${label}: mobile Ask dock height ${sample.dockHeight} exceeds its ${bound}px conversation-dock bound`;
  }
  if (!Number.isFinite(sample.viewportHeight) || sample.viewportHeight <= sample.dockHeight) {
    return `${label}: mobile file viewport height ${sample.viewportHeight} is not larger than the bounded Ask dock height ${sample.dockHeight}`;
  }
  if (sample.viewportHit !== true) return `${label}: elementFromPoint at the mobile file viewport center does not hit the viewport`;
  for (const [name, rect, inside, hit] of [
    ['textarea', sample.textareaRect, sample.textareaInsideDock, sample.textareaHit],
    ['send/stop action', sample.sendRect, sample.sendInsideDock, sample.sendHit],
  ]) {
    if (inside !== true) return `${label}: mobile Ask dock ${name} ${JSON.stringify(rect)} is not fully inside the dock`;
    if (hit !== true) return `${label}: elementFromPoint at the mobile Ask dock ${name} center does not hit it`;
  }
  return null;
}

// SCOPE: the dock always names what it is and which file it belongs to. The
// scope header must be present and visible, carry the exact quiet title,
// stamp the ACTIVE file path in data-path (never read off panel text), and
// the rendered path itself must be the elementFromPoint target at its
// center.
function conversationScopeError(label, sample) {
  if (sample.scopePresent !== true) return `${label}: file conversation scope header is missing`;
  if (sample.scopeVisible !== true) return `${label}: file conversation scope header is not visible`;
  if (sample.scopeTitle !== 'File conversation') return `${label}: scope title is ${JSON.stringify(sample.scopeTitle)}, want "File conversation"`;
  if (sample.expectedPath === '' || sample.scopePath !== sample.expectedPath) {
    return `${label}: scope path ${JSON.stringify(sample.scopePath)} does not match the active file ${JSON.stringify(sample.expectedPath)}`;
  }
  if (sample.scopePathHit !== true) return `${label}: elementFromPoint at the scope path center does not hit the path`;
  return null;
}

// EMPTY STATE: an empty, non-streaming conversation must show the quiet
// empty-state message; the message must vanish once real entries exist and
// must never show while streaming.
function conversationEmptyStateError(label, sample) {
  if (sample.streaming === true) {
    if (sample.emptyPresent === true) return `${label}: empty-state message is shown while the conversation is streaming`;
    return null;
  }
  if (sample.transcriptEntries === 0) {
    if (sample.emptyPresent !== true || sample.emptyVisible !== true) {
      return `${label}: empty file conversation shows no empty-state message`;
    }
    if (!sample.emptyText.includes('Ask about this file')) {
      return `${label}: empty-state text ${JSON.stringify(sample.emptyText)} does not state the file conversation scope`;
    }
    return null;
  }
  if (sample.emptyPresent === true) return `${label}: empty-state message is still shown with ${sample.transcriptEntries} transcript entries`;
  return null;
}

// FOCUS BOTTOM ALIGNMENT (both directions): in ask-focus the dock bottom,
// body bottom, and LAYOUT VIEWPORT bottom must ALL coincide (±1px) — the
// main composer footer is hidden while Files is open, so the viewport
// bottom is the shared lower edge (a one-sided "dock above footer" never
// passes).
function focusBottomAlignmentError(label, sample) {
  for (const field of ['dockBottom', 'bodyBottom', 'viewportBottom']) {
    if (!Number.isFinite(sample[field])) return `${label}: fullscreen focus geometry ${field} was not sampled`;
  }
  if (Math.abs(sample.dockBottom - sample.bodyBottom) > 1 || Math.abs(sample.bodyBottom - sample.viewportBottom) > 1) {
    return `${label}: fullscreen focus bottom edges diverge (dock ${sample.dockBottom}, body ${sample.bodyBottom}, viewport ${sample.viewportBottom}) — all three must coincide (±1px)`;
  }
  return null;
}
const ASK_FOCUS_BTN = '#file-explorer-focus-btn';

function askFullscreenError(label, sample) {
  if (!sample || typeof sample !== 'object') return `${label}: fullscreen sample missing`;
  const phase = sample.phase;
  const explorerOpen = sample.explorerOpen === true;
  const focused = sample.focused === true;
  const treeVisible = sample.treeVisible === true;
  const contentVisible = sample.contentVisible === true;
  const dockFills = sample.dockFills === true;
  const narrow = sample.narrow === true;
  const treeMounted = sample.treeMounted === true;
  if (phase === 'ready' || phase === 'reset' || phase === 'escaped') {
    if (!explorerOpen) return `${label}: Files is not open while fullscreen is ${phase}`;
    if (phase !== 'escaped') {
      if (sample.disabled === true) return `${label}: fullscreen button is disabled with a file open`;
      if (sample.visible !== true || sample.hittable !== true) return `${label}: fullscreen button is not a visible hit target`;
      if (sample.pressed !== false || focused) return `${label}: fullscreen did not start or reset off`;
      if (sample.label !== 'Show conversation fullscreen') return `${label}: fullscreen label is ${JSON.stringify(sample.label)}`;
    }
    if (focused || sample.pressed === true) return `${label}: fullscreen stayed on after ${phase}`;
    if (!sample.dock || !contentVisible || !treeMounted) return `${label}: dock, current content, or mounted tree is missing while fullscreen is off`;
    if (!narrow && !treeVisible) return `${label}: desktop tree is hidden while fullscreen is off`;
    if (sample.bodyRequired === true && !sample.bodyExact) return `${label}: open file body is not the expected non-empty text`;
    if (sample.bodyRequired === true && sample.otherBody) return `${label}: the other same-name file body is visible`;
    if (phase === 'escaped' && sample.active !== ASK_FOCUS_BTN.slice(1)) return `${label}: Escape did not focus the visible fullscreen button`;
    const bottomError = explorerBottomAlignmentError(label, sample);
    if (bottomError !== null) return bottomError;
    const dockBoundError = mobileBoundedDockError(label, sample);
    if (dockBoundError !== null) return dockBoundError;
    const scopeError = conversationScopeError(label, sample);
    if (scopeError !== null) return scopeError;
    const emptyError = conversationEmptyStateError(label, sample);
    if (emptyError !== null) return emptyError;
    return null;
  }
  if (phase === 'focused') {
    if (!explorerOpen || !focused) return `${label}: fullscreen did not enter`;
    if (sample.pressed !== true || sample.label !== 'Exit fullscreen conversation') {
      return `${label}: focused button is not pressed as Exit fullscreen conversation`;
    }
    if (treeVisible || contentVisible) return `${label}: file tree or content stays visible in fullscreen`;
    if (!sample.dock || !dockFills) {
      const box = (rect) => rect ? `x=${rect.x} y=${rect.y} w=${rect.width} h=${rect.height}` : 'missing';
      return `${label}: conversation dock does not fill the explorer body dock(${box(sample.dockRect)}) body(${box(sample.bodyRect)})`;
    }
    if (sample.headerOutside !== true) return `${label}: app header is covered by the fullscreen drawer`;
    if (sample.composerHittable !== false) return `${label}: elementFromPoint still hits #prompt-input while the fullscreen drawer is open — the MAIN composer must stay hidden`;
    // Dock-bottom alignment is enforced by focusBottomAlignmentError below:
    // dock, body, and layout-viewport bottoms must ALL coincide (±1px).
    if (sample.bodyRequired === true && !sample.bodyExact) return `${label}: fullscreen dock lost the expected file body`;
    const focusBottomError = focusBottomAlignmentError(label, sample);
    if (focusBottomError !== null) return focusBottomError;
    const focusScopeError = conversationScopeError(label, sample);
    if (focusScopeError !== null) return focusScopeError;
    const focusEmptyError = conversationEmptyStateError(label, sample);
    if (focusEmptyError !== null) return focusEmptyError;
    return null;
  }
  if (phase === 'reopened') {
    if (!explorerOpen || focused) return `${label}: close/reopen kept fullscreen`;
    if (sample.pressed !== false || sample.visible !== true || sample.hittable !== true) {
      return `${label}: reopened fullscreen button is not a visible off hit target`;
    }
    return null;
  }
  return `${label}: unknown fullscreen phase ${JSON.stringify(phase)}`;
}

function selfCheckAskFullscreenGate() {
  const ready = {
    phase: 'ready', explorerOpen: true, disabled: false, visible: true, hittable: true,
    pressed: false, focused: false, label: 'Show conversation fullscreen', dock: true,
    treeVisible: true, treeMounted: true, contentVisible: true, narrow: false, bodyRequired: true, bodyExact: true, otherBody: false,
    viewportBottom: 844, bodyBottom: 844, bodyHeight: 700, dockBottom: 836, dockHeight: 320,
    viewportHeight: 400, viewportHit: true,
    textareaRect: { x: 8, y: 700, width: 300, height: 44 }, sendRect: { x: 8, y: 752, width: 60, height: 32 },
    textareaInsideDock: true, sendInsideDock: true, textareaHit: true, sendHit: true,
    scopePresent: true, scopeVisible: true, scopeTitle: 'File conversation', scopePath: 'mod7/sub0/file-0.ts', scopePathHit: true,
    emptyPresent: true, emptyVisible: true, emptyText: 'Ask about this file. Messages stay in this file conversation.',
    transcriptEntries: 0, streaming: false, expectedPath: 'mod7/sub0/file-0.ts',
  };
  const emptyBody = { ...ready, bodyExact: false };
  if (askFullscreenError('desktop', emptyBody) === null) throw new Error('fullscreen self-check accepted an empty file body');
  const diffDock = { ...ready, bodyRequired: false, bodyExact: false };
  const narrowReady = { ...ready, narrow: true, treeVisible: false, dockHeight: 280, viewportHeight: 400 };
  if (askFullscreenError('mobile', narrowReady) !== null) throw new Error('fullscreen self-check rejected a mounted hidden mobile tree');
  if (askFullscreenError('mobile', { ...narrowReady, treeMounted: false }) === null) throw new Error('fullscreen self-check accepted an unmounted mobile tree');
  if (askFullscreenError('desktop', diffDock) !== null) throw new Error('fullscreen self-check rejected a diff dock');
  const focused = {
    ...ready, phase: 'focused', pressed: true, focused: true, label: 'Exit fullscreen conversation',
    treeVisible: false, contentVisible: false, dockFills: true,
    headerOutside: true, composerHittable: false,
    dockBottom: 844,
  };
  if (askFullscreenError('desktop', focused) !== null) throw new Error('fullscreen self-check rejected a filling dock');
  if (askFullscreenError('desktop', { ...focused, treeVisible: true }) === null) {
    throw new Error('fullscreen self-check accepted a visible tree');
  }
  if (askFullscreenError('desktop', { ...focused, headerOutside: false }) === null) {
    throw new Error('fullscreen self-check accepted a covered header');
  }
  if (askFullscreenError('desktop', { ...focused, composerHittable: true }) === null) {
    throw new Error('fullscreen self-check accepted a visible main composer while Files is open');
  }
  if (askFullscreenError('desktop', { ...focused, dockBottom: 850 }) === null) {
    throw new Error('fullscreen self-check accepted a dock past the viewport bottom');
  }
  const escaped = { ...focused, phase: 'escaped', focused: false, pressed: false, treeVisible: true, contentVisible: true, active: ASK_FOCUS_BTN.slice(1) };
  if (askFullscreenError('desktop', escaped) !== null) throw new Error('fullscreen self-check rejected focus on the visible fullscreen button');
  if (askFullscreenError('desktop', { ...focused, phase: 'escaped', explorerOpen: false, focused: false, pressed: false }) === null) {
    throw new Error('fullscreen self-check accepted Escape closing Files');
  }
  // Explorer bottom / bounded dock / scope / empty gate self-checks: each
  // documented failure mode must be REJECTED, starting with the current
  // tree's own 14px outer whitespace band geometry.
  const misaligned = { ...ready, bodyBottom: 830 };
  if (askFullscreenError('desktop', misaligned) === null) throw new Error('fullscreen self-check accepted a 14px bottom whitespace band');
  const underlap = { ...ready, bodyBottom: 850 };
  if (askFullscreenError('desktop', underlap) === null) throw new Error('fullscreen self-check accepted an explorer body past the viewport bottom');
  const tallDock = { ...narrowReady, dockHeight: 340 };
  if (askFullscreenError('mobile', tallDock) === null) throw new Error('fullscreen self-check accepted an unbounded mobile Ask dock');
  const smallViewport = { ...narrowReady, viewportHeight: 200 };
  if (askFullscreenError('mobile', smallViewport) === null) throw new Error('fullscreen self-check accepted a mobile viewport smaller than the Ask dock');
  const outsideSend = { ...narrowReady, sendInsideDock: false };
  if (askFullscreenError('mobile', outsideSend) === null) throw new Error('fullscreen self-check accepted a mobile send button outside the dock');
  const deadSend = { ...narrowReady, sendHit: false };
  if (askFullscreenError('mobile', deadSend) === null) throw new Error('fullscreen self-check accepted an unhittable mobile send button');
  const noScope = { ...ready, scopePresent: false };
  if (askFullscreenError('desktop', noScope) === null) throw new Error('fullscreen self-check accepted a missing scope header');
  const wrongPath = { ...ready, scopePath: 'pkg/b/dup-name.txt' };
  if (askFullscreenError('desktop', wrongPath) === null) throw new Error('fullscreen self-check accepted a scope path mismatch');
  const noEmpty = { ...ready, emptyPresent: false };
  if (askFullscreenError('desktop', noEmpty) === null) throw new Error('fullscreen self-check accepted a missing empty-state message');
  const staleEmpty = { ...ready, transcriptEntries: 3 };
  if (askFullscreenError('desktop', staleEmpty) === null) throw new Error('fullscreen self-check accepted an empty-state message over real entries');
  const streamingEmpty = { ...ready, streaming: true, emptyPresent: true };
  if (askFullscreenError('desktop', streamingEmpty) === null) throw new Error('fullscreen self-check accepted an empty-state message while streaming');
  const focusSplit = { ...focused, bodyBottom: 830 };
  if (askFullscreenError('desktop', focusSplit) === null) throw new Error('fullscreen self-check accepted a focus body off the viewport bottom');
  const focusDockShort = { ...focused, dockBottom: 840 };
  if (askFullscreenError('desktop', focusDockShort) === null) throw new Error('fullscreen self-check accepted a focus dock short of the body bottom');
}

selfCheckAskFullscreenGate();

// EXPLORER REVEAL after a citation jump (FL6.8 desktop / FL6.9 mobile).
// READ-ONLY samples: they never expand a directory, click a toggle, or call
// scrollIntoView, so a missing reveal can never be compensated by the test.
//   - the tree must carry EXACTLY ONE selected row whose `title` is the full
//     workspace-relative path (the visible label stays the basename), with
//     every ancestor directory row aria-expanded="true" — on desktop the pane
//     must already be open and the row inside the tree scroll viewport; on
//     mobile the compact pane may be collapsed, but the exact selection and
//     expanded chain must still be there for the pane's own toggle;
//   - the top of the content viewport — every visible element ABOVE the file
//     body — must carry that same full path (basename-only is the failure).
function explorerTreeRevealView(arg) {
  const path = String(arg?.path || '');
  const parts = path.split('/');
  const ancestors = [];
  for (let index = 1; index < parts.length; index += 1) ancestors.push(parts.slice(0, index).join('/'));
  const rectOf = (node) => {
    if (!node) return null;
    const rect = node.getBoundingClientRect();
    return { x: rect.x, y: rect.y, width: rect.width, height: rect.height, top: rect.top, bottom: rect.bottom, left: rect.left, right: rect.right };
  };
  const visible = (node) => {
    if (!node) return false;
    const rect = node.getBoundingClientRect();
    if (rect.width < 1 || rect.height < 1) return false;
    const style = getComputedStyle(node);
    return style.display !== 'none' && style.visibility !== 'hidden';
  };
  const tree = document.querySelector('[data-testid="file-explorer-tree"]');
  const scroller = document.querySelector('[data-testid="file-explorer-tree"] .file-explorer__tree-list');
  // The tree pane renders EXACTLY ONE branch: the citation-choice/search
  // results or the real workspace tree (`ul.file-explorer__tree[data-depth]`,
  // which SearchResults never carries). Only the workspace tree counts as a
  // revealed selection — a highlighted chooser/search row is not the tree.
  const treeLists = [...document.querySelectorAll('[data-testid="file-explorer-tree"] ul.file-explorer__tree[data-depth]')];
  const resultLists = [...document.querySelectorAll('[data-testid="file-explorer-tree"] [data-testid="file-explorer-search-results"]')];
  const inResults = (node) => resultLists.some((list) => list.contains(node));
  const treeRow = (selector) => [...document.querySelectorAll(`[data-testid="file-explorer-tree"] button${selector}`)]
    .filter((node) => !inResults(node) && node.closest('ul.file-explorer__tree[data-depth]') != null);
  const dirRows = treeRow('.file-explorer__row--dir');
  const activeRows = treeRow('.file-explorer__row--active');
  return {
    path,
    treePresent: tree != null,
    treeVisible: visible(tree),
    treeRect: rectOf(tree),
    treeMode: treeLists.length > 0 ? 'tree' : (resultLists.length > 0 ? 'results' : 'none'),
    treeListCount: treeLists.length,
    resultListCount: resultLists.length,
    searchValue: document.querySelector('[data-testid="file-explorer-search"]')?.value ?? null,
    scrollerPresent: scroller != null,
    scrollerVisible: visible(scroller),
    scrollerRect: rectOf(scroller),
    scrollerScrollTop: scroller ? scroller.scrollTop : null,
    activeCount: activeRows.length,
    activeRows: activeRows.map((node) => ({
      title: node.getAttribute('title') || '',
      text: (node.textContent || '').trim(),
      visible: visible(node),
      rect: rectOf(node),
    })),
    ancestors: ancestors.map((ancestor) => {
      const row = dirRows.find((node) => node.getAttribute('title') === ancestor) || null;
      return {
        path: ancestor,
        found: row != null,
        expanded: row ? row.getAttribute('aria-expanded') : null,
        visible: visible(row),
        rect: rectOf(row),
      };
    }),
  };
}

function explorerTreeRevealError(label, sample, path, requireOpen) {
  const at = (message) => `${label}: ${message}`;
  const basename = path.split('/').pop();
  if (!sample || !sample.treePresent) return at('the workspace tree is not mounted after the citation jump');
  if (sample.treeMode !== 'tree') {
    return at(`the tree pane shows ${sample.treeMode === 'results' ? 'the citation choices / file search results' : 'neither the workspace tree nor search results'} (search=${JSON.stringify(sample.searchValue)}) instead of the workspace tree — the opened file is not selected in the tree`);
  }
  if (requireOpen && !sample.treeVisible) {
    return at(`the workspace tree is collapsed after the citation jump (${JSON.stringify(sample.treeRect)}) — the selected file is not visible without a manual toggle`);
  }
  if (requireOpen && (!sample.scrollerPresent || !sample.scrollerVisible)) {
    return at(`the workspace tree scroll viewport is missing or invisible (${JSON.stringify(sample.scrollerRect)})`);
  }
  if (sample.activeCount !== 1) {
    return at(`expected exactly ONE selected tree row, found ${sample.activeCount}: ${JSON.stringify((sample.activeRows || []).map((row) => row.title))}`);
  }
  const active = sample.activeRows[0];
  if (active.title !== path) {
    return at(`the selected tree row is ${JSON.stringify(active.title)}, want the full workspace-relative path ${JSON.stringify(path)}`);
  }
  if (requireOpen && active.text !== basename) {
    return at(`the selected tree row label is ${JSON.stringify(active.text)}, want the visible basename ${JSON.stringify(basename)}`);
  }
  if (requireOpen && !active.visible) return at('the selected tree row has no visible box');
  for (const ancestor of sample.ancestors || []) {
    if (!ancestor.found) return at(`ancestor directory row ${JSON.stringify(ancestor.path)} is not rendered — the citation did not expand its directory chain`);
    if (ancestor.expanded !== 'true') return at(`ancestor directory row ${JSON.stringify(ancestor.path)} is aria-expanded=${JSON.stringify(ancestor.expanded)}, want "true"`);
  }
  if (requireOpen) {
    const box = sample.scrollerRect;
    const row = active.rect;
    const inside = row && box && row.top >= box.top - 1 && row.bottom <= box.bottom + 1
      && row.left >= box.left - 1 && row.right <= box.right + 1;
    if (!inside) {
      return at(`the selected tree row ${JSON.stringify(row)} is outside the tree scroll viewport ${JSON.stringify(box)} (scrollTop=${sample.scrollerScrollTop}) — the reveal did not scroll it into view`);
    }
  }
  return null;
}

function explorerContentPathView(arg) {
  const path = String(arg?.path || '');
  const basename = path.split('/').pop();
  const normalize = (value) => String(value || '').replace(/[\u200b-\u200d\ufeff]/g, '').replace(/\s+/g, '');
  const rectOf = (node) => {
    if (!node) return null;
    const rect = node.getBoundingClientRect();
    return { x: rect.x, y: rect.y, width: rect.width, height: rect.height, top: rect.top, bottom: rect.bottom, left: rect.left, right: rect.right };
  };
  const visible = (node) => {
    if (!node) return false;
    const rect = node.getBoundingClientRect();
    if (rect.width < 1 || rect.height < 1) return false;
    const style = getComputedStyle(node);
    return style.display !== 'none' && style.visibility !== 'hidden' && style.opacity !== '0';
  };
  const viewport = document.querySelector('[data-testid="file-explorer-content"]');
  if (!viewport) {
    return { path, basename, contentPresent: false, contentVisible: false, contentRect: null, codePresent: false, codeVisible: false, codeRect: null, bandText: '', bandNodes: [] };
  }
  const contentRect = rectOf(viewport);
  const code = viewport.querySelector('.file-explorer__code');
  const codeRect = rectOf(code);
  const bandTexts = [];
  const bandNodes = [];
  for (const node of viewport.querySelectorAll('*')) {
    if (code && code.contains(node)) continue;
    const text = (node.textContent || '').trim();
    if (!text) continue;
    const rect = rectOf(node);
    if (!rect || rect.width < 1 || rect.height < 1) continue;
    const style = getComputedStyle(node);
    if (style.display === 'none' || style.visibility === 'hidden' || style.opacity === '0') continue;
    // Content TOP only: above the file body and fully inside the viewport.
    if (codeRect && rect.bottom > codeRect.top + 1) continue;
    if (rect.top < contentRect.top - 1 || rect.bottom > contentRect.bottom + 1) continue;
    bandTexts.push(text);
    bandNodes.push({ text: text.slice(0, 200), rect });
  }
  return {
    path,
    basename,
    contentPresent: true,
    contentVisible: visible(viewport),
    contentRect,
    codePresent: code != null,
    codeVisible: visible(code),
    codeRect,
    bandText: normalize(bandTexts.join(' ')),
    bandNodes: bandNodes.slice(0, 12),
  };
}

function explorerContentPathError(label, sample, path) {
  const at = (message) => `${label}: ${message}`;
  if (!sample || !sample.contentPresent) return at('the selected-file content viewport is not mounted');
  if (!sample.contentVisible) return at(`the selected-file content viewport has no visible box (${JSON.stringify(sample.contentRect)}) — the opened file cannot be identified`);
  if (!sample.codePresent || !sample.codeVisible) return at('the selected file body (.file-explorer__code) is not visible, so the content top cannot be checked');
  const want = String(path).replace(/[\u200b-\u200d\ufeff]/g, '').replace(/\s+/g, '');
  const band = sample.bandText || '';
  if (!band.includes(want)) {
    return at(`the content top shows ${JSON.stringify(band)}, which does not carry the full repo-relative path ${JSON.stringify(path)}${band === String(sample.basename || '') ? ' — only the basename is visible' : ''}`);
  }
  if (!(sample.bandNodes || []).length) return at('no visible element above the file body carries the path');
  return null;
}

function selfCheckExplorerRevealGates() {
  const path = 'psy-node/psy_node_redis/src/store/rewards.rs';
  const box = { x: 0, y: 0, width: 300, height: 200, top: 0, bottom: 200, left: 0, right: 300 };
  const row = { x: 0, y: 40, width: 300, height: 22, top: 40, bottom: 62, left: 0, right: 300 };
  const ancestors = ['psy-node', 'psy-node/psy_node_redis', 'psy-node/psy_node_redis/src', 'psy-node/psy_node_redis/src/store']
    .map((value) => ({ path: value, found: true, expanded: 'true', visible: true, rect: row }));
  const okTree = {
    path,
    treePresent: true,
    treeVisible: true,
    treeRect: box,
    treeMode: 'tree',
    treeListCount: 1,
    resultListCount: 0,
    searchValue: '',
    scrollerPresent: true,
    scrollerVisible: true,
    scrollerRect: box,
    scrollerScrollTop: 120,
    activeCount: 1,
    activeRows: [{ title: path, text: 'rewards.rs', visible: true, rect: row }],
    ancestors,
  };
  if (explorerTreeRevealError('self', okTree, path, true) !== null) throw new Error('reveal self-check rejected an exact revealed row');
  const treeRejects = {
    'a missing tree': { ...okTree, treePresent: false },
    'the results branch instead of the tree': { ...okTree, treeMode: 'results', resultListCount: 1, treeListCount: 0, activeCount: 0, activeRows: [] },
    'a collapsed desktop pane': { ...okTree, treeVisible: false },
    'no selected row': { ...okTree, activeCount: 0, activeRows: [] },
    'two selected rows': { ...okTree, activeCount: 2, activeRows: [{ ...okTree.activeRows[0] }, { ...okTree.activeRows[0] }] },
    'a basename title': { ...okTree, activeRows: [{ ...okTree.activeRows[0], title: 'rewards.rs' }] },
    'a collapsed ancestor': { ...okTree, ancestors: ancestors.map((item, index) => (index === 2 ? { ...item, expanded: 'false' } : item)) },
    'a missing ancestor': { ...okTree, ancestors: [ancestors[0], { path: ancestors[1].path, found: false, expanded: null, visible: false, rect: null }, ...ancestors.slice(2)] },
    'a row below the scrollport': { ...okTree, activeRows: [{ ...okTree.activeRows[0], rect: { ...row, top: 260, bottom: 282 } }] },
  };
  for (const [name, sample] of Object.entries(treeRejects)) {
    if (explorerTreeRevealError('self', sample, path, true) === null) throw new Error(`reveal self-check accepted ${name}`);
  }
  // Mobile: the compact pane may be collapsed, but the exact selection and
  // the expanded chain are still mandatory (no geometry while hidden).
  if (explorerTreeRevealError('self', { ...okTree, treeVisible: false, scrollerVisible: false, activeRows: [{ ...okTree.activeRows[0], visible: false, rect: null }] }, path, false) !== null) {
    throw new Error('reveal self-check rejected a collapsed mobile pane that still carries the exact selection');
  }
  if (explorerTreeRevealError('self', { ...okTree, treeVisible: false, activeCount: 0, activeRows: [] }, path, false) === null) {
    throw new Error('reveal self-check accepted a collapsed mobile pane with no selected row');
  }
  const okContent = {
    path,
    basename: 'rewards.rs',
    contentPresent: true,
    contentVisible: true,
    contentRect: { ...box, width: 400, height: 300, right: 400, bottom: 300 },
    codePresent: true,
    codeVisible: true,
    codeRect: { x: 0, y: 40, width: 400, height: 260, top: 40, bottom: 300, left: 0, right: 400 },
    bandText: 'psy-node/psy_node_redis/src/store/rewards.rs',
    bandNodes: [{ text: path, rect: row }],
  };
  if (explorerContentPathError('self', okContent, path) !== null) throw new Error('reveal self-check rejected a full content path');
  const contentRejects = {
    'a basename-only content top': { ...okContent, bandText: 'rewards.rs' },
    'an empty content top': { ...okContent, bandText: '', bandNodes: [] },
    'a hidden content pane': { ...okContent, contentVisible: false },
    'a missing file body': { ...okContent, codePresent: false, codeVisible: false, bandText: '' },
  };
  for (const [name, sample] of Object.entries(contentRejects)) {
    if (explorerContentPathError('self', sample, path) === null) throw new Error(`reveal self-check accepted ${name}`);
  }
}

selfCheckExplorerRevealGates();

async function openFilesFromHeader(page, label) {
  const diagnoseToggle = async () => page.evaluate(() => {
    const button = document.getElementById('file-explorer-toggle-btn');
    const rows = [];
    for (let node = button; node; node = node.parentElement) {
      const rect = node.getBoundingClientRect();
      const style = getComputedStyle(node);
      rows.push(`${node.tagName}${node.id ? '#' + node.id : ''}:y=${rect.y}:h=${rect.height}:display=${style.display}:visibility=${style.visibility}`);
    }
    return rows.join('|');
  });
  const toggle = page.locator('#file-explorer-toggle-btn');
  let box = await toggle.boundingBox();
  if (!box || box.width < 1 || box.height < 1) {
    await openFeatureMenu(page);
    try {
      await waitFor(page, () => {
        const btn = document.getElementById('file-explorer-toggle-btn');
        const menu = document.getElementById('feature-menu');
        const box = btn?.getBoundingClientRect();
        const nav = btn?.closest('nav');
        const navRect = nav?.getBoundingClientRect();
        const navStyle = nav ? getComputedStyle(nav) : null;
        const sample = (x, y) => {
          const hit = document.elementFromPoint(x, y);
          return { x, y, hit: hit?.id ? '#' + hit.id : (typeof hit?.className === 'string' ? hit.className : hit?.tagName || '') };
        };
        const points = box ? [sample(box.x + box.width / 2, box.y + box.height / 2), sample(box.x + box.width / 2, box.y + 5), sample(box.x + box.width / 2, box.y + Math.min(12, box.height / 2))] : [];
        const menuOpen = menu instanceof HTMLElement && menu.hidden === false && getComputedStyle(menu).display !== 'none';
        const toggleBox = Boolean(box && box.width >= 1 && box.height >= 1);
        const hit = box ? document.elementFromPoint(box.x + box.width / 2, box.y + Math.min(12, box.height / 2)) : null;
        const hitButton = hit?.closest('#file-explorer-toggle-btn') === btn;
        window.__fxFilesToggleProbe = { menuOpen, toggleBox, hitButton, hitClass: typeof hit?.className === 'string' ? hit.className : hit?.tagName || '', menuClass: typeof menu?.className === 'string' ? menu.className : '', button: box ? { x: box.x, y: box.y, w: box.width, h: box.height, bottom: box.bottom } : null, nav: navRect && navStyle ? { x: navRect.x, y: navRect.y, w: navRect.width, h: navRect.height, bottom: navRect.bottom, overflow: navStyle.overflow, clipPath: navStyle.clipPath, scrollHeight: nav.scrollHeight, clientHeight: nav.clientHeight, gridTemplateRows: navStyle.gridTemplateRows } : null, points };
        return menuOpen && toggleBox && hitButton;
      }, `${label}: opening the feature menu did not reveal a hittable Files toggle`, 10000, undefined, { diagnostic: true });
    } catch (error) {
      const probe = await page.evaluate(() => window.__fxFilesToggleProbe || null);
      await journeyFrame(page, 'files-latency-toggle-hidden.png', 'Files toggle hidden');
      throw new Error(`${error instanceof Error ? error.message : String(error)} probe=${JSON.stringify(probe)} ancestors=${await diagnoseToggle()}`);
    }
    box = await toggle.boundingBox();
  }
  if (!box || box.width < 1 || box.height < 1) fail(`${label}: Files toggle has no box ancestors=${await diagnoseToggle()}`);
  const covered = await toggle.evaluate((node) => {
    const rect = node.getBoundingClientRect();
    const hit = document.elementFromPoint(rect.x + rect.width / 2, rect.y + Math.min(12, rect.height / 2));
    return hit?.closest('#file-explorer-toggle-btn') !== node;
  });
  if (covered) fail(`${label}: Files toggle is covered`);
  await clickFeature(page, '#file-explorer-toggle-btn');
}

async function assertAskFullscreen(page, label, path, marker, other = '', reopen = true, onPhase = null) {
  let entered = false;
  try {
  const read = () => page.evaluate(({ buttonId, marker: want, other: otherMarker, expectedPath: wantPath }) => {
    const section = document.querySelector('section.file-explorer');
    const button = document.getElementById(buttonId);
    const box = button?.getBoundingClientRect();
    const hit = box && box.width >= 1 && box.height >= 1
      ? document.elementFromPoint(box.left + box.width / 2, box.top + box.height / 2)
      : null;
    const shown = (selector) => [...document.querySelectorAll(selector)].some((node) => {
      const rect = node.getBoundingClientRect();
      const style = getComputedStyle(node);
      return rect.width > 0 && rect.height > 0 && style.display !== 'none' && style.visibility !== 'hidden';
    });
    const dock = document.querySelector('section.file-conversation');
    const body = document.querySelector('[data-testid="file-explorer-layout"]');
    const rectOf = (node) => {
      if (!node) return null;
      const rect = node.getBoundingClientRect();
      return { x: rect.x, y: rect.y, width: rect.width, height: rect.height };
    };
    const dockBox = dock?.getBoundingClientRect();
    const bodyBox = body?.getBoundingClientRect();
    const text = `${document.querySelector('[data-testid="file-explorer-content"]')?.textContent || ''}\n${dock?.textContent || ''}`;
    const header = document.querySelector('#root > header')?.getBoundingClientRect();
    const composer = document.getElementById('prompt-input')?.getBoundingClientRect();
    const explorer = section?.getBoundingClientRect();
    const outside = (rect) => explorer && rect && rect.height > 0 && (rect.bottom <= explorer.top + 1 || rect.top >= explorer.bottom - 1);
    // The MAIN composer footer is HIDDEN while Files is open, so the Ask
    // dock's lower edge is the LAYOUT VIEWPORT BOTTOM, not a footer band.
    // composerHittable records whether #prompt-input is still the real
    // elementFromPoint target at its center — it must NOT be while the
    // drawer owns the body.
    const composerHittable = (() => {
      if (!composer || composer.width < 1 || composer.height < 1) return false;
      const node = document.elementFromPoint(composer.x + composer.width / 2, composer.y + composer.height / 2);
      return Boolean(node && (node.id === 'prompt-input' || node.closest?.('#prompt-input')));
    })();
    const visibleNow = (node) => {
      if (!node) return false;
      const rect = node.getBoundingClientRect();
      const style = getComputedStyle(node);
      return rect.width > 0 && rect.height > 0 && style.display !== 'none' && style.visibility !== 'hidden';
    };
    const insideDock = (rect) => Boolean(dockBox && rect && rect.width >= 1 && rect.height >= 1
      && rect.left >= dockBox.left - 1 && rect.right <= dockBox.right + 1
      && rect.top >= dockBox.top - 1 && rect.bottom <= dockBox.bottom + 1);
    const centerHit = (node) => {
      const rect = node?.getBoundingClientRect();
      if (!rect || rect.width < 1 || rect.height < 1) return false;
      const hit = document.elementFromPoint(rect.x + rect.width / 2, rect.y + rect.height / 2);
      return Boolean(hit && (hit === node || node.contains(hit)));
    };
    const viewport = ['.file-explorer__tree-viewport', '.file-explorer__content-viewport']
      .flatMap((selector) => [...document.querySelectorAll(selector)])
      .find(visibleNow) || null;
    const viewportBox = viewport?.getBoundingClientRect() || null;
    const viewportHit = (() => {
      if (!viewport || !viewportBox || viewportBox.width < 1 || viewportBox.height < 1) return false;
      const hit = document.elementFromPoint(viewportBox.x + viewportBox.width / 2, viewportBox.y + viewportBox.height / 2);
      return Boolean(hit && viewport.contains(hit));
    })();
    const textarea = dock?.querySelector('textarea.file-conversation__input') || null;
    const action = dock?.querySelector('.file-conversation__action--send, .file-conversation__action--stop') || null;
    const scope = dock?.querySelector('[data-testid="file-conversation-scope"]') || null;
    const scopePathEl = scope?.querySelector('.file-conversation__scope-path') || null;
    const emptyEl = dock?.querySelector('[data-testid="file-conversation-empty"]') || null;
    const transcript = dock?.querySelector('.file-conversation__transcript') || null;
    return {
      explorerOpen: section != null,
      disabled: button?.disabled === true,
      visible: box != null && box.width >= 1 && box.height >= 1,
      hittable: hit?.closest(`#${buttonId}`) === button,
      pressed: button?.getAttribute('aria-pressed') === 'true',
      focused: section?.classList.contains('file-explorer--ask-focus') === true,
      label: button?.getAttribute('aria-label') || '',
      dock: dock != null,
      treeVisible: shown('[data-testid="file-explorer-tree"]'),
      treeMounted: document.querySelector('[data-testid="file-explorer-tree"]') != null,
      narrow: window.innerWidth < 721,
      contentVisible: shown('[data-testid="file-explorer-content"]'),
      dockFills: !!(dockBox && bodyBox && dockBox.width >= bodyBox.width - 2 && dockBox.height >= bodyBox.height - 2),
      dockRect: rectOf(dock),
      bodyRect: rectOf(body),
      headerOutside: outside(header),
      composerHittable,
      bodyExact: want !== '' && text.includes(want),
      bodyRequired: want !== '',
      otherBody: otherMarker !== '' && text.includes(otherMarker),
      active: document.activeElement?.id || '',
      viewportBottom: document.documentElement.clientHeight,
      bodyBottom: bodyBox ? bodyBox.bottom : null,
      bodyHeight: bodyBox ? bodyBox.height : null,
      dockBottom: dockBox ? dockBox.bottom : null,
      dockHeight: dockBox ? dockBox.height : null,
      viewportHeight: viewportBox ? viewportBox.height : null,
      viewportHit,
      textareaRect: rectOf(textarea),
      sendRect: rectOf(action),
      textareaInsideDock: insideDock(textarea?.getBoundingClientRect()),
      sendInsideDock: insideDock(action?.getBoundingClientRect()),
      textareaHit: centerHit(textarea),
      sendHit: centerHit(action),
      scopePresent: scope != null,
      scopeVisible: visibleNow(scope),
      scopeTitle: scope?.querySelector('.file-conversation__scope-title')?.textContent || '',
      scopePath: scope?.getAttribute('data-path') || '',
      scopePathHit: centerHit(scopePathEl),
      emptyPresent: emptyEl != null,
      emptyVisible: visibleNow(emptyEl),
      emptyText: emptyEl?.textContent || '',
      transcriptEntries: transcript ? transcript.querySelectorAll(':scope > .side-chat__entry').length : 0,
      streaming: !!transcript?.querySelector(':scope > .side-chat__typing'),
      expectedPath: wantPath,
    };
  }, { buttonId: ASK_FOCUS_BTN.slice(1), marker, other, expectedPath: path });
  const check = async (phase) => {
    const sample = await read();
    const error = askFullscreenError(label, { ...sample, phase });
    if (onPhase) await onPhase(phase, sample);
    if (error !== null) throw new Error(error);
  };
  const button = page.locator(ASK_FOCUS_BTN);
  await button.waitFor({ state: 'visible', timeout: 10000 });
  await check('ready');
  await button.click();
  await page.waitForFunction(() => document.querySelector('section.file-explorer')?.classList.contains('file-explorer--ask-focus') === true);
  entered = true;
  await check('focused');
  await button.click();
  await page.waitForFunction(() => document.querySelector('section.file-explorer')?.classList.contains('file-explorer--ask-focus') !== true);
  await check('reset');
  await button.click();
  await page.waitForFunction(() => document.querySelector('section.file-explorer')?.classList.contains('file-explorer--ask-focus') === true);
  await page.evaluate(() => document.querySelector('section.file-explorer')?.focus());
  await page.keyboard.press('Escape');
  await page.waitForFunction(() => document.querySelector('section.file-explorer')?.classList.contains('file-explorer--ask-focus') !== true);
  await check('escaped');
  if (!reopen) return;
  await page.click('#file-explorer-drawer-close-btn');
  await page.waitForFunction(() => document.querySelector('section.file-explorer') == null);
  await openFilesFromHeader(page, `${label} reopen`);
  await page.waitForFunction((title) => document.querySelector(`[data-testid="file-explorer-tree"] button[title="${title}"]`)?.classList.contains('file-explorer__row--active') === true, path, { timeout: 20000 });
  await button.waitFor({ state: 'visible', timeout: 10000 });
  await check('reopened');
  const row = page.locator(`[data-testid="file-explorer-tree"] button[title="${path}"]`).first();
  if (marker !== '' && (await row.getAttribute('aria-expanded')) === 'false') await row.click();
  if (marker !== '') {
    await page.waitForFunction(({ title, want }) => {
      const active = document.querySelector(`[data-testid="file-explorer-tree"] button[title="${title}"]`);
      const text = document.querySelector('[data-testid="file-explorer-content"]')?.textContent || '';
      return active?.classList.contains('file-explorer__row--active') === true && text.includes(want);
    }, { title: path, want: marker }, { timeout: 20000 });
  }
  } finally {
    const focused = await page.evaluate(() => document.querySelector('section.file-explorer')?.classList.contains('file-explorer--ask-focus') === true);
    if (entered && focused) {
      await page.locator('#file-explorer-focus-btn').click();
      await page.waitForFunction(() => document.querySelector('section.file-explorer')?.classList.contains('file-explorer--ask-focus') !== true);
    }
  }
}
selfCheckExplorerJourneyGates();


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
//        FL0.7 desktop 1440x900 Files open: explorer elementFromPoint misses
//              #sidebar-manage-btn and .session-sidebar__foot; a non-preview
//              file with the tree expanded has no empty 32px content toolbar;
//              #file-explorer-focus-btn is a visible hit target, fullscreen
//              hides the tree and file content while the dock fills the
//              body, the header stays disjoint, the MAIN COMPOSER STAYS
//              HIDDEN (invisible, unhittable, unfocusable — the whole
//              footer wrapper) while the dock/body bottoms sit on the
//              layout viewport bottom, and button/Escape/close
//              reopen reset it; closing Files restores #prompt-input
//              (hittable, focused, typed) and keeps the draft. Last, after
//              those checks, the dirty
//              mod6/sub0/file-0.ts row opens a non-empty diff and one
//              [data-testid=file-explorer-open-file] click reads that
//              relative path. README.md has no diff control, so "No
//              changes" is not a browser assertion here.
//        FL0.9 desktop-only CORNER LEAK: the explorer rect must cover the
//              #sidebar-manage-btn/.session-sidebar__foot centers (geometry,
//              not just z-order), those points must not pierce to the
//              sidebar foot, and #prompt-input stays hidden/unhittable.
//        FL0.2/FL0.3 desktop 1440x900 Refresh glyph-only busy (held
//              responses; the busy wait gates on the ACTUAL .is-refreshing/
//              aria-busy DOM state plus the parked held responses — never an
//              elapsed wall-clock conjunct) + released settle
//        FL0.8 the SAME occlusion/source-top/composer/fullscreen/open-file
//              contract at 390x844 once the mobile Files pane has settled,
//              before Refresh. Same empty-diff limitation as FL0.7.
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
//        FL6.3 one dup-name.txt read returns file_ambiguous for pkg/a and
//              pkg/b, opens neither, and reopen sends nothing
//        FL6.4 sibling-of-root absolute citation issues no read and does
//              not show LINK-OUTSIDE; a path marker is allowed
//        FL6.5 bare root-basename README.md:2 citation (no path context)
//              stamps the exact basename at render (no
//              data-md-resolved="none") and opens the root README via the
//              session-stamped correlated read; no file-unavailable error
//        FL6.6 one homonym.rs read returns both bridge copies as typed
//              choices; the pkg/a choice opens HOMONYM-A only, then the same
//              fullscreen contract runs on that non-empty body
//        FL6.7 both rewards.rs copies are offered; A shows REWARDS-NODE-A
//              and not REWARDS-NODE-FIX-B, then the same fullscreen contract
//              runs on that body. An empty diff cannot pass as either file.
//        FL6.8 / FL6.9 (recorded INSIDE the two FL6.7 rewards passes, so they
//              precede FL6.7 in the ledger) — after the A choice opens: the
//              tree has exactly one selected row with the full
//              workspace-relative title, expanded ancestor rows, and (FL6.8
//              desktop only) an open drawer with the row inside the tree
//              scroll viewport; the content top names that full path, never
//              just the basename. FL6.9 asserts the same selection identity on
//              the collapsed mobile pane plus the content path.

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
  'FL6.1', 'FL6.2', 'FL6.3', 'FL6.4', 'FL6.5', 'FL6.6', 'FL6.8', 'FL6.9', 'FL6.7',
  'FL0.7', 'FL0.9', 'FL0.8',
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
            && (frame.command.startsWith('file_explorer') || frame.command.startsWith('file_conversation') || frame.command === 'get_state')) {
            window.__fxFrames.push({
              dir: 'in', type: frame.command, id: frame.id ?? '', success: frame.success === true,
              path: typeof frame.data?.path === 'string' ? frame.data.path : '',
              conversationId: typeof frame.data?.conversationId === 'string' ? frame.data.conversationId : '',
              code: typeof frame.data?.code === 'string' ? frame.data.code : '',
              candidates: Array.isArray(frame.data?.candidates) ? frame.data.candidates : [],
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
          if (typeof frame?.type === 'string' && (frame.type.startsWith('file_explorer') || frame.type.startsWith('file_conversation') || frame.type === 'get_state')) {
            window.__fxFrames.push({
              dir: 'out', type: frame.type, id: frame.id ?? '', path: frame.path ?? '', conversationId: frame.conversationId ?? '', session: frame.sessionId ?? '', t: performance.now(),
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
    await openFilesFromHeader(page, 'initial open');
    await waitFor(
      page,
      () => document.querySelector('[data-testid="file-explorer-tree"] .file-explorer__row--dir') != null,
      'workspace tree rows never rendered'
    );
    const explorerJourneyDraft = `fx-explorer-draft-${laneNonce}`;
    const readExplorerOcclusion = () => page.evaluate(() => {
      const section = document.querySelector('section.file-explorer');
      const button = document.getElementById('sidebar-manage-btn');
      const foot = document.querySelector('.session-sidebar__foot');
      const box = (node) => {
        if (!node) return null;
        const rect = node.getBoundingClientRect();
        return { top: rect.top, bottom: rect.bottom, left: rect.left, right: rect.right, width: rect.width, height: rect.height };
      };
      const explorer = box(section);
      const probes = [];
      if (explorer && explorer.width > 1 && explorer.height > 1) {
        const xs = [explorer.left + 8, (explorer.left + explorer.right) / 2, explorer.right - 8];
        const ys = [explorer.top + 8, (explorer.top + explorer.bottom) / 2, explorer.bottom - 1];
        for (const y of ys) {
          for (const x of xs) {
            if (x < explorer.left || x > explorer.right || y < explorer.top || y > explorer.bottom) continue;
            const node = document.elementFromPoint(x, y);
            const sidebarFoot = Boolean(node && (node.id === 'sidebar-manage-btn' || node.closest?.('#sidebar-manage-btn, .session-sidebar__foot')));
            probes.push({ x, y, sidebarFoot, hit: node ? (node.id ? `#${node.id}` : (node.className || node.tagName)) : '' });
          }
        }
      }
      return { explorer, button: box(button), foot: box(foot), probes };
    });
    const readSourceTop = () => page.evaluate(() => {
      const content = document.querySelector('[data-testid="file-explorer-content"]');
      const toolbar = content?.querySelector(':scope > .file-explorer__content-toolbar') || null;
      const source = content?.querySelector('.file-explorer__source, .file-explorer__code');
      const pathStrip = content?.querySelector('[data-testid="file-explorer-content-path"]') || null;
      const contentRect = content?.getBoundingClientRect();
      const sourceRect = source?.getBoundingClientRect();
      const toolbarRect = toolbar?.getBoundingClientRect();
      const pathStripRect = pathStrip?.getBoundingClientRect();
      const style = content ? getComputedStyle(content) : null;
      const pathStripStyle = pathStrip ? getComputedStyle(pathStrip) : null;
      const active = document.querySelector('[data-testid="file-explorer-tree"] .file-explorer__row--active .file-explorer__row-path');

      return {
        path: (active?.textContent || '').trim(),
        layout: document.querySelector('[data-testid="file-explorer-layout"]')?.getAttribute('data-layout') || '',
        treeCollapsed: document.querySelector('[data-testid="file-explorer-layout"]')?.classList.contains('file-explorer__body--tree-collapsed') === true,
        contentHidden: content == null || content.hidden === true || content.getAttribute('hidden') != null,
        toolbarChildren: toolbar ? toolbar.childElementCount : -1,
        toolbarTreeToggle: toolbar?.querySelector('.file-explorer__tree-toggle') != null,
        toolbarHeight: toolbarRect ? toolbarRect.height : -1,
        sourceOffset: contentRect && sourceRect ? sourceRect.top - contentRect.top : null,
        sourceTop: sourceRect ? sourceRect.top : null,
        pathStripPresent: pathStrip != null,
        pathStripVisible: Boolean(pathStripRect && pathStripRect.width >= 1 && pathStripRect.height >= 1
          && pathStripStyle && pathStripStyle.display !== 'none' && pathStripStyle.visibility !== 'hidden'),
        pathStripBottom: pathStripRect ? pathStripRect.bottom : null,
        borderTopWidth: style ? Number.parseFloat(style.borderTopWidth) : null,
        paddingTop: style ? Number.parseFloat(style.paddingTop) : null,
        diffView: content?.querySelector('.file-explorer__diff') != null,
        openFileAction: content?.querySelector('[data-testid="file-explorer-open-file"]') != null,
      };
    });
    const seededFileTitle = 'mod6/sub0/file-0.ts';
    // RED diagnostics for a missing/disabled tree row: the rendered rows, the
    // tree's OWN error surfaces (the dir-listing rejection text), and the
    // `file_explorer_list` METADATA — request path/id/session plus the
    // response success/code/error. No payloads, file bytes, or tokens.
    const captureTreeDiagnostics = () => page.evaluate(() => ({
      rows: [...document.querySelectorAll('[data-testid="file-explorer-tree"] button')].map((node) => ({
        className: typeof node.className === 'string' ? node.className : '',
        text: (node.querySelector('.file-explorer__row-path')?.textContent || node.textContent || '').trim(),
        title: node.getAttribute('title') || '',
        expanded: node.getAttribute('aria-expanded'),
        disabled: node.disabled === true,
      })),
      errors: [...document.querySelectorAll('[data-testid="file-explorer-tree"] .file-explorer__error')].map((node) => ({
        className: typeof node.className === 'string' ? node.className : '',
        text: (node.textContent || '').trim().slice(0, 240),
      })),
      listFrames: (Array.isArray(window.__fxFrames) ? window.__fxFrames : [])
        .filter((frame) => frame && frame.type === 'file_explorer_list')
        .slice(-24)
        .map((frame) => ({
          dir: frame.dir,
          id: frame.id,
          path: frame.path || '',
          session: frame.session || '',
          success: frame.success,
          code: frame.code || '',
          error: frame.error == null ? '' : String(frame.error).slice(0, 240),
        })),
    }));
    const expandDir = async (label, name) => {
      const dir = page.locator('[data-testid="file-explorer-tree"] button[role="button"], [data-testid="file-explorer-tree"] button').filter({ hasText: new RegExp(`^${name}$`) }).first();
      try {
        await dir.waitFor({ state: 'visible', timeout: 10000 });
      } catch (error) {
        const tree = await captureTreeDiagnostics();
        fail(`${label}: directory ${name} is not a visible tree button (${tree.rows.length} rows: ${JSON.stringify(tree)})`);
      }
      const expanded = await dir.getAttribute('aria-expanded');
      if (expanded !== 'true') {
        await dir.scrollIntoViewIfNeeded();
        const box = await dir.boundingBox();
        if (!box || box.width < 1 || box.height < 1) {
          fail(`${label}: directory ${name} has no geometry after scroll`);
        }
        const hittable = await dir.evaluate((node) => {
          const rect = node.getBoundingClientRect();
          if (rect.width < 1 || rect.height < 1) return false;
          const hit = document.elementFromPoint(rect.left + rect.width / 2, rect.top + Math.min(rect.height / 2, rect.height - 1));
          return hit === node || node.contains(hit);
        });
        if (!hittable) fail(`${label}: directory ${name} is not the element under its center`);
        await dir.click();
      }
      try {
        await waitFor(
          page,
          (dirName) => [...document.querySelectorAll('[data-testid="file-explorer-tree"] button')]
            .some((node) => (node.querySelector('.file-explorer__row-path')?.textContent || '').trim() === dirName
              && node.getAttribute('aria-expanded') === 'true'),
          `${label}: ${name} never expanded`,
          20000,
          name,
          { diagnostic: true },
        );
      } catch (error) {
        const tree = await captureTreeDiagnostics();
        fail(`${label}: ${name} never expanded (${tree.rows.length} rows: ${JSON.stringify(tree)})`);
      }
    };
    const openNonPreviewFile = async (label) => {
      const collapsed = await page.evaluate(() => document.querySelector('[data-testid="file-explorer-layout"]')?.classList.contains('file-explorer__body--tree-collapsed') === true);
      if (collapsed) {
        const treeBtn = page.locator('#file-explorer-tree-toggle');
        await treeBtn.waitFor({ state: 'visible', timeout: 10000 });
        const box = await treeBtn.boundingBox();
        if (!box || box.width < 1 || box.height < 1) fail(`${label}: tree toggle is collapsed but has no geometry`);
        const hit = await treeBtn.evaluate((node) => {
          const rect = node.getBoundingClientRect();
          const point = document.elementFromPoint(rect.left + rect.width / 2, rect.top + rect.height / 2);
          return point === node || node.contains(point);
        });
        if (!hit) fail(`${label}: tree toggle is not the element under its center`);
        await treeBtn.click();
        await waitFor(
          page,
          () => document.querySelector('[data-testid="file-explorer-layout"]')?.classList.contains('file-explorer__body--tree-collapsed') !== true
            && document.querySelector('[data-testid="file-explorer-tree"] .file-explorer__row--dir') != null,
          `${label}: tree toggle did not reveal the directory rows`,
          10000,
        );
      }
      await expandDir(label, 'mod6');
      await expandDir(label, 'sub0');
      const row = page.getByRole('button', { name: 'file-0.ts', exact: true }).and(page.locator(`[title="${seededFileTitle}"]`)).first();
      const found = await row.waitFor({ state: 'visible', timeout: 10000 }).then(() => true, () => false);
      if (!found) {
        const tree = await captureTreeDiagnostics();
        fail(`${label}: seeded ${seededFileTitle} is not a visible button (${tree.rows.length} rows: ${JSON.stringify(tree)})`);
      }
      await row.scrollIntoViewIfNeeded();
      const rowBox = await row.boundingBox();
      if (!rowBox || rowBox.width < 1 || rowBox.height < 1) {
        fail(`${label}: seeded ${seededFileTitle} has no geometry after scroll`);
      }
      const rowHit = await row.evaluate((node) => {
        const rect = node.getBoundingClientRect();
        const hit = document.elementFromPoint(rect.left + Math.min(24, rect.width / 2), rect.top + rect.height / 2);
        return hit === node || node.contains(hit);
      });
      if (!rowHit) fail(`${label}: seeded ${seededFileTitle} is not the element under its hit point`);
      await row.click();
      const layout = await page.evaluate(() => document.querySelector('[data-testid="file-explorer-layout"]')?.getAttribute('data-layout') || '');
      if (layout === 'tree') {
        const treeBtn = page.locator('#file-explorer-tree-toggle');
        await treeBtn.waitFor({ state: 'visible', timeout: 10000 });
        await treeBtn.click();
      }
      try {
        await waitFor(
          page,
          (title) => {
            const content = document.querySelector('[data-testid="file-explorer-content"]');
            const source = content?.querySelector('.file-explorer__source, .file-explorer__code');
            const hidden = content == null || content.hidden === true || content.getAttribute('hidden') != null;
            const open = document.querySelector(`[data-testid="file-explorer-tree"] button[title="${title}"]`);
            return !hidden && source != null && open?.classList.contains('file-explorer__row--active') === true;
          },
          `${label}: seeded ${seededFileTitle} source never opened`,
          20000,
          seededFileTitle,
          { diagnostic: true },
        );
      } catch (error) {
        const tree = await captureTreeDiagnostics();
        fail(`${label}: seeded ${seededFileTitle} source never opened (${tree.rows.length} rows: ${JSON.stringify(tree)})`);
      }
    };
    const ensureTreeExpanded = async (step) => {
      const collapsed = await page.evaluate(() => document.querySelector('[data-testid="file-explorer-layout"]')?.classList.contains('file-explorer__body--tree-collapsed') === true);
      if (collapsed) {
        const treeBtn = page.locator('#file-explorer-tree-toggle');
        await treeBtn.waitFor({ state: 'visible', timeout: 10000 });
        await treeBtn.click();
        await waitFor(
          page,
          () => document.querySelector('[data-testid="file-explorer-layout"]')?.classList.contains('file-explorer__body--tree-collapsed') !== true,
          `${step}: tree toggle did not reveal the directory rows`,
          10000,
        );
      }
      await expandDir(step, 'mod6');
      await expandDir(step, 'sub0');
    };
    const readHeaderClip = () => page.evaluate(() => {
      const header = document.querySelector('#root > header');
      if (!header) return { missing: 1 };
      const rect = header.getBoundingClientRect();
      const bandY = Math.max(0, rect.top) + 1;
      const hit = document.elementFromPoint(rect.left + Math.min(24, Math.max(1, rect.width / 2)), bandY);
      let clip = 0;
      for (let node = header.parentElement; node; node = node.parentElement) {
        const style = getComputedStyle(node);
        const clips = style.overflow === 'hidden' || style.overflowY === 'hidden' || style.overflow === 'clip' || style.overflowY === 'clip';
        if (!clips) continue;
        const parent = node.getBoundingClientRect();
        if (parent.top > rect.top + 0.5) clip = 1;
      }
      return {
        missing: 0,
        top: rect.top,
        height: rect.height,
        clip,
        hitHeader: hit === header || header.contains(hit) ? 1 : 0,
        scroll: {
          scrollY: window.scrollY,
          documentScroll: document.documentElement.scrollTop,
          bodyScroll: document.body.scrollTop,
          rootScroll: document.getElementById('root')?.scrollTop ?? null,
          scrollingTop: document.scrollingElement?.scrollTop ?? null,
          root: document.getElementById('root')?.getBoundingClientRect(),
          body: document.body.getBoundingClientRect(),
        },
      };
    });
    const readAskDiagnostic = () => page.evaluate((buttonId) => {
      const button = document.getElementById(buttonId);
      const active = document.querySelector('[data-testid="file-explorer-tree"] button.file-explorer__row--active');
      const content = document.querySelector('[data-testid="file-explorer-content"]');
      const pane = content?.querySelector('.file-explorer__code, .file-explorer__source');
      return {
        activeTitle: active?.getAttribute('title') || '',
        activeClass: typeof active?.className === 'string' ? active.className : '',
        paneClass: typeof pane?.className === 'string' ? pane.className : '',
        dock: document.querySelector('section.file-conversation') != null,
        disabled: button?.disabled === true,
      };
    }, ASK_FOCUS_BTN.slice(1));
    let sidebarCornerBaseline = null; // FL0.9 pre-open Sessions rail snapshot
    const assertExplorerJourney = async (label, id) => {
      const errors = [];
      const keep = async (name, run) => {
        try {
          await run();
          console.log(`[web-files-latency:check] ${label} ${name} PASS`);
        } catch (error) {
          const message = error instanceof Error ? error.message : String(error);
          errors.push(`${name}: ${message}`);
          console.log(`[web-files-latency:check] ${label} ${name} FAIL ${message}`);
        }
      };
      await keep('occlusion', async () => {
        const occlusionError = explorerOcclusionError(label, await readExplorerOcclusion());
        if (occlusionError !== null) throw new Error(occlusionError);
        await stageFrame(page, label.startsWith('desktop') ? 'files-latency-desktop-open.png' : 'files-latency-mobile-open-frame.png', 'Files open before file tab');
      });
      await keep('background-replies-hidden', async () => {
        const replyError = backgroundReplyReachabilityError(`${label} Files open`, await readBackgroundReplyReachability(page), false);
        if (replyError !== null) throw new Error(replyError);
      });
      await keep('main-composer-hidden', async () => {
        const hiddenError = mainComposerHiddenError(`${label} Files open`, await readMainComposerOcclusion(page));
        if (hiddenError !== null) throw new Error(hiddenError);
        // REAL keyboard Tab traversal (not a focus() equivalence claim):
        // tab through the live focus order until it cycles; the main
        // composer footer must never be reached while Files is open. The
        // cycle marker carries a PER-CALL nonce: a bare boolean would
        // survive across journeys and let the mobile traversal "cycle"
        // instantly on elements the desktop pass already marked, passing
        // vacuously without ever walking the mobile focus order.
        const tabNonce = `tab-${laneNonce}-${tabTraversalSeq++}`;
        const visited = new Set();
        // Failure-only focus trace: the walk's STARTING active element, the
        // live focusable-element count, the first 20 visited node summaries,
        // and the final active element. Summaries carry tag/id/class/geometry
        // ONLY (never text content); they are dumped to the evidence root by
        // dumpFocusTrace() before the never-cycled hard fail below.
        const focusState = () => page.evaluate(() => {
          const el = document.activeElement;
          const rect = el ? el.getBoundingClientRect() : null;
          return {
            active: el
              ? {
                tag: el.tagName || '',
                id: el.id || '',
                cls: typeof el.className === 'string' ? el.className : '',
                rect: rect ? [Math.round(rect.x), Math.round(rect.y), Math.round(rect.width), Math.round(rect.height)] : null,
              }
              : null,
            focusable: document.querySelectorAll('a[href],button,input,select,textarea,[tabindex]').length,
          };
        });
        const focusStart = await focusState();
        const trail = [];
        for (let step = 0; step < 300; step++) {
          await page.keyboard.press('Tab');
          const probe = await page.evaluate((nonce) => {
            const el = document.activeElement;
            const footer = document.getElementById('prompt-input')?.closest('footer');
            const background = el?.closest?.('.conversation-with-minimap, .session-sidebar') || null;
            const cycled = el != null && el.__fxTabSeenNonce === nonce;
            if (el) {
              try { el.__fxTabSeenNonce = nonce; } catch { /* cross-realm or frozen node */ }
            }
            let node = null;
            if (el) {
              const rect = el.getBoundingClientRect();
              node = {
                tag: el.tagName || '',
                id: el.id || '',
                cls: typeof el.className === 'string' ? el.className : '',
                rect: [Math.round(rect.x), Math.round(rect.y), Math.round(rect.width), Math.round(rect.height)],
              };
            }
            return {
              key: el ? `${el.tagName}#${el.id || ''}` : 'none',
              node,
              cycled,
              inBackground: background != null,
              inFooter: footer ? footer.contains(el) : false,
            };
          }, tabNonce);
          if (trail.length < 20) trail.push({ step: step + 1, ...(probe.node || {}) });
          if (probe.inBackground) throw new Error(`${label}: Tab traversal reached the covered transcript/sidebar at step ${step + 1} (${probe.key})`);
          if (probe.inFooter) throw new Error(`${label}: Tab traversal reached the main composer footer at step ${step + 1} (${probe.key})`);
          if (probe.cycled) return; // the whole live focus order completed without touching the composer
          visited.add(probe.key);
        }
        const focusEnd = await focusState();
        await dumpFocusTrace(page, label, {
          label,
          steps: 300,
          uniqueStops: visited.size,
          startActive: focusStart.active,
          focusableAtStart: focusStart.focusable,
          visitedFirst20: trail,
          finalActive: focusEnd.active,
          focusableAtFailure: focusEnd.focusable,
        });
        throw new Error(`${label}: Tab traversal never cycled within 300 steps (${visited.size} stops) — the focus surface is unbounded`);
      });
      await keep('header-open', async () => {
        const headerOpen = await readHeaderClip();
        const scroll = headerOpen.scroll || {};
        console.log(`[web-files-latency:scalars] header-open headerTop=${headerOpen.top} scrollY=${scroll.scrollY} scrollingTop=${scroll.scrollingTop} rootScroll=${scroll.rootScroll}`);
        const headerOpenError = headerClipError(`${label} Files open`, headerOpen);
        if (headerOpenError !== null) throw new Error(headerOpenError);
      });
      await keep('open-file', () => openNonPreviewFile(label));
      const opened = !errors.some((item) => item.startsWith('open-file:'));
      await keep('fullscreen', async () => {
        if (!opened) throw new Error(`${label}: fullscreen not executed because the diff row did not open`);
        const diagnostic = await readAskDiagnostic();
        console.log(`[web-files-latency:diag] ${label} ask ${JSON.stringify(diagnostic)}`);
        await stageFrame(page, label.startsWith('desktop') ? 'files-latency-desktop-file.png' : 'files-latency-mobile-file.png', 'opened diff before fullscreen');
        await assertAskFullscreen(page, label, seededFileTitle, '', '', true, async (phase, sample) => {
          if (phase === 'ready') {
            await stageFrame(page, label.startsWith('desktop') ? 'files-latency-desktop-ask-normal.png' : 'files-latency-mobile-ask-bounded.png', 'normal-mode file conversation (desktop empty / mobile bounded dock)');
            return;
          }
          if (phase !== 'focused') return;
          const dock = sample?.dockRect || {};
          const body = sample?.bodyRect || {};
          console.log(`[web-files-latency:scalars] fullscreen dockTop=${dock.top} dockBottom=${dock.bottom} dockLeft=${dock.left} dockRight=${dock.right} dockWidth=${dock.width} dockHeight=${dock.height} bodyTop=${body.top} bodyBottom=${body.bottom} bodyLeft=${body.left} bodyRight=${body.right} bodyWidth=${body.width} bodyHeight=${body.height}`);
          await stageFrame(page, label.startsWith('desktop') ? 'files-latency-desktop-fullscreen.png' : 'files-latency-mobile-fullscreen.png', 'conversation fullscreen');
        });
      });
      await keep('real-ask', async () => {
        // Real per-file Ask round trip on the live RPC path: the mock echo
        // `sessions-reply: <prompt>` must name THIS question exactly.
        const isMobile = !label.startsWith('desktop');
        const question = `file-ask-${isMobile ? 'mobile' : 'desktop'}-${laneNonce}`
          + (isMobile ? ` ${Array.from({ length: 120 }, (_, i) => `fixture-line-${i}-of-the-seeded-transcript-body`).join(' ')}` : '');
        const dockSample = () => page.evaluate(() => {
          const dock = document.querySelector('section.file-conversation');
          const rect = (node) => (node ? node.getBoundingClientRect() : null);
          const inside = (outer, inner) => Boolean(outer && inner
            && inner.left >= outer.left - 1 && inner.right <= outer.right + 1
            && inner.top >= outer.top - 1 && inner.bottom <= outer.bottom + 1);
          const dockBox = rect(dock);
          const textarea = dock?.querySelector('textarea.file-conversation__input');
          const send = dock?.querySelector('.file-conversation__action--send, .file-conversation__action--stop');
          const hit = (node) => {
            const box = node?.getBoundingClientRect();
            if (!box || box.width < 1 || box.height < 1) return false;
            const target = document.elementFromPoint(box.x + box.width / 2, box.y + box.height / 2);
            return Boolean(target && (target === node || node.contains(target)));
          };
          return {
            conversationId: dock?.getAttribute('data-conversation-id') || '',
            entries: dock ? dock.querySelectorAll('.side-chat__transcript > .side-chat__entry').length : 0,
            scopePath: dock?.querySelector('[data-testid="file-conversation-scope"]')?.getAttribute('data-path') || '',
            emptyShown: !!dock?.querySelector('[data-testid="file-conversation-empty"]'),
            assistants: dock ? dock.querySelectorAll('.side-chat__transcript > .side-chat__entry--assistant').length : 0,
            replies: (dock?.querySelector('.file-conversation__transcript')?.textContent || '').includes('sessions-reply:'),
            dockHeight: dockBox ? dockBox.height : null,
            controlsInside: inside(dockBox, rect(textarea)) && inside(dockBox, rect(send)),
            controlsHittable: hit(textarea) && hit(send),
            bodyHeight: document.querySelector('[data-testid="file-explorer-layout"]')?.getBoundingClientRect().height || null,
          };
        });
        const before = await dockSample();
        if (isMobile) {
          const beforeMax = Math.min(280, (before.bodyHeight || 0) * 0.45) + 1;
          if (before.dockHeight == null || before.dockHeight > beforeMax || !before.controlsInside || !before.controlsHittable) {
            throw new Error(`${label}: mobile Ask dock is not normal-bounded before the ask ${JSON.stringify(before)}`);
          }
        }
        const input = page.locator('textarea.file-conversation__input');
        await input.click();
        await input.fill(question);
        await page.locator('section.file-conversation .file-conversation__action--send').click();
        await waitFor(
          page,
          (want) => {
            const dock = document.querySelector('section.file-conversation');
            return dock != null
              && (dock.querySelector('.file-conversation__transcript')?.textContent || '').includes(`sessions-reply: ${want.question}`)
              && dock.querySelectorAll('.side-chat__transcript > .side-chat__entry').length >= want.entriesBefore + 2
              && !dock.querySelector('[data-testid="file-conversation-empty"]');
          },
          `${label}: file Ask reply never echoed the exact question`,
          45000,
          { question, entriesBefore: before.entries },
        );
        const after = await dockSample();
        console.log(`[web-files-latency:scalars] real-ask ${JSON.stringify(after)}`);
        if (after.scopePath !== seededFileTitle) throw new Error(`${label}: after the Ask, scope path is ${JSON.stringify(after.scopePath)}, want ${JSON.stringify(seededFileTitle)}`);
        if (after.emptyShown) throw new Error(`${label}: empty-state message is still shown after the reply`);
        if (!after.controlsInside || !after.controlsHittable) throw new Error(`${label}: dock composer drifted out of the dock after the reply`);
        if (isMobile) {
          const boundedMax = Math.min(280, (after.bodyHeight || 0) * 0.45) + 1;
          if (after.dockHeight == null || after.dockHeight > boundedMax) throw new Error(`${label}: normal-mode mobile Ask dock is not bounded after the long reply (height ${after.dockHeight}, max ${boundedMax})`);
          const scroll = await page.evaluate(() => {
            const transcript = document.querySelector('section.file-conversation .file-conversation__transcript');
            if (!transcript) return null;
            const scrollable = transcript.scrollHeight > transcript.clientHeight;
            transcript.scrollTop = transcript.scrollHeight;
            return { scrollable, bottomReached: transcript.scrollTop + transcript.clientHeight >= transcript.scrollHeight - 1 };
          });
          if (!scroll || !scroll.scrollable || !scroll.bottomReached) throw new Error(`${label}: long mock reply is not scrollable to the bottom in the bounded dock ${JSON.stringify(scroll)}`);
        }
        // Switch files: scope follows the ACTIVE file, the fresh conversation
        // starts empty, and switching back restores the reply. On any
        // switch-step failure, dump the dock identity/state plus the last
        // file_conversation wire frames into the evidence root.
        const switchDiag = async (tag, error, samples) => {
          const state = await page.evaluate((want) => {
            const dock = document.querySelector('section.file-conversation');
            const pane = document.querySelector('[data-testid="file-explorer-content"] .file-explorer__code, [data-testid="file-explorer-content"] .file-explorer__diff, [data-testid="file-explorer-content"] .file-explorer__source');
            return {
              scopePath: dock?.querySelector('[data-testid="file-conversation-scope"]')?.getAttribute('data-path') || '',
              conversationId: dock?.getAttribute('data-conversation-id') || '',
              entries: dock ? dock.querySelectorAll('.side-chat__transcript > .side-chat__entry').length : 0,
              assistants: dock ? dock.querySelectorAll('.side-chat__transcript > .side-chat__entry--assistant').length : 0,
              emptyShown: !!dock?.querySelector('[data-testid="file-conversation-empty"]'),
              replyMarker: (dock?.querySelector('.file-conversation__transcript')?.textContent || '').includes('sessions-reply:'),
              replyExact: (dock?.querySelector('.file-conversation__transcript')?.textContent || '').includes(`sessions-reply: ${want.question}`),
              activeRowTitle: document.querySelector('[data-testid="file-explorer-tree"] button.file-explorer__row--active')?.getAttribute('title') || '',
              paneClass: typeof pane?.className === 'string' ? pane.className : '',
              layout: document.querySelector('[data-testid="file-explorer-layout"]')?.getAttribute('data-layout') || '',
              drawerOpen: document.querySelector('section.file-explorer') != null,
            };
          }, { question }).catch((dumpError) => ({ dumpFailed: true, dumpError: dumpError instanceof Error ? dumpError.message : String(dumpError) }));
          const frames = await page.evaluate(() => window.__fxFrames.filter((f) => f.type.startsWith('file_conversation')).slice(-12)).catch((framesError) => ({ frameDumpFailed: true, frameError: framesError instanceof Error ? framesError.message : String(framesError) }));
          // The row the step asked for is not rendered: the tree's own error
          // text plus the `file_explorer_list` request/response metadata make
          // the listing rejection (code/message) and the exact dir path
          // visible in this SAME dump. No payloads or bytes.
          const tree = await captureTreeDiagnostics().catch((treeError) => ({ treeDumpFailed: true, treeError: treeError instanceof Error ? treeError.message : String(treeError) }));
          const payload = { tag, lane: label, error: error instanceof Error ? error.message : String(error), state, convFrames: frames, tree, samples: samples || [] };
          try {
            fs.writeFileSync(path.join(evidence, `files-latency-switch-diag-${tag}.json`), `${JSON.stringify(payload, null, 2)}\n`);
            console.log(`[web-files-latency:diag] switch ${tag} DUMPED ${path.join(evidence, `files-latency-switch-diag-${tag}.json`)}`);
          } catch { /* unwritable evidence root; stdout still carries it */ }
          console.log(`[web-files-latency:diag] switch ${tag} ${JSON.stringify(payload)}`);
          throw error;
        };
        // diagnostic: true so a timeout THROWS into the dump instead of
        // fail()-exiting the process.
        const other = 'mod6/sub0/file-1.ts';
        try {
          await ensureTreeExpanded(`${label} to-sibling`);
          const otherRow = page.locator(`[data-testid="file-explorer-tree"] button[title="${other}"]`).first();
          await otherRow.waitFor({ state: 'visible', timeout: 10000 });
          await otherRow.scrollIntoViewIfNeeded();
          await otherRow.click();
          await waitFor(
            page,
            (want) => document.querySelector('[data-testid="file-conversation-scope"]')?.getAttribute('data-path') === want,
            `${label}: scope did not follow the switched file`,
            20000,
            other,
            { diagnostic: true },
          );
        } catch (error) {
          await switchDiag('to-sibling', error);
        }
        const fresh = await dockSample();
        if (fresh.emptyShown !== true || fresh.assistants !== 0) {
          throw new Error(`${label}: switched file did not start a fresh empty conversation ${JSON.stringify(fresh)}`);
        }
        const samples = [];
        const onSample = (result) => {
          const { matched, ...sample } = result || {};
          if (samples.length === 0 || JSON.stringify(samples[samples.length - 1]) !== JSON.stringify(sample)) {
            samples.push(sample);
            if (samples.length > 40) samples.shift();
          }
        };
        await ensureTreeExpanded(`${label} back-to-seed`);
        await page.locator(`[data-testid="file-explorer-tree"] button[title="${seededFileTitle}"]`).click();
        try {
          // Same conversation id as right after the reply, at least the
          // captured entry count, and the reply marker in the transcript.
          await waitFor(
            page,
            (want) => {
              const dock = document.querySelector('section.file-conversation');
              const transcript = dock?.querySelector('.file-conversation__transcript');
              const sample = {
                scopePath: dock?.querySelector('[data-testid="file-conversation-scope"]')?.getAttribute('data-path') || '',
                conversationId: dock?.getAttribute('data-conversation-id') || '',
                entries: dock ? dock.querySelectorAll('.side-chat__transcript > .side-chat__entry').length : 0,
                assistants: dock ? dock.querySelectorAll('.side-chat__transcript > .side-chat__entry--assistant').length : 0,
                replyMarker: (transcript?.textContent || '').includes('sessions-reply:'),
              };
              return {
                ...sample,
                matched: sample.scopePath === want.seededFileTitle
                  && sample.conversationId === want.conversationId
                  && sample.entries >= want.entries
                  && sample.replyMarker,
              };
            },
            `${label}: switching back lost the file conversation`,
            20000,
            { seededFileTitle, conversationId: after.conversationId, entries: after.entries },
            { diagnostic: true, onSample },
          );
        } catch (error) {
          await switchDiag('back-to-seed', error, samples);
        }
        await stageFrame(page, isMobile ? 'files-latency-mobile-ask-replies.png' : 'files-latency-desktop-ask-replies.png', 'file conversation with real Ask replies');
      });
      await keep('composer', async () => {
        // The MAIN composer is HIDDEN while Files is open, so the draft is
        // written while the composer is restored: close Files, type the
        // draft, reopen Files (the composer must hide again), then close
        // and require the exact draft + focus + typing restoration.
        if (await page.locator('section.file-explorer').count()) {
          await page.click('#file-explorer-drawer-close-btn');
        }
        await waitFor(page, () => document.querySelector('section.file-explorer') == null, `${label}: file explorer never closed for the draft`);
        await page.fill('#prompt-input', explorerJourneyDraft);
        const draftBefore = await page.evaluate(() => document.getElementById('prompt-input')?.value || '');
        if (draftBefore !== explorerJourneyDraft) throw new Error(`${label}: draft was not written before close`);
        await openFilesFromHeader(page, `${label} composer reopen`);
        await waitFor(page, () => document.querySelector('section.file-explorer') != null, `${label}: file explorer never reopened over the draft`);
        await waitFor(page, () => document.querySelector('section.file-conversation') != null, `${label}: the file Ask dock never remounted over the reopened file`);
        const composerHiddenError = mainComposerHiddenError(`${label} draft reopen`, await readMainComposerOcclusion(page), { requireAsk: true });
        if (composerHiddenError !== null) throw new Error(composerHiddenError);
        await page.click('#file-explorer-drawer-close-btn');
        await waitFor(page, () => document.querySelector('section.file-explorer') == null, `${label}: file explorer never closed`);
        // AUTO-FOCUS IS THE PRODUCT'S JOB: the close routes converge on one
        // transition effect that focuses #prompt-input one rAF after the
        // hidden-class removal. The lane waits event-driven for that focus
        // and NEVER focuses the input itself — the sample below records the
        // product-driven state only.
        await waitFor(
          page,
          () => document.activeElement?.id === 'prompt-input',
          `${label}: #prompt-input did not auto-focus when Files closed`,
          10000,
        );
        const readScroll = () => page.evaluate(() => {
          const root = document.getElementById('root');
          const header = document.querySelector('#root > header');
          const chain = [];
          for (let node = header?.parentElement; node; node = node.parentElement) {
            chain.push(`${node.id ? '#' + node.id : node.tagName}:${node.scrollTop}/${node.scrollHeight}/${node.clientHeight}`);
          }
          return { scrollY: window.scrollY, scrollingTop: document.scrollingElement?.scrollTop ?? null, rootScroll: root?.scrollTop ?? null, chain: chain.join(',') };
        });
        const scrollBefore = await readScroll();
        const typedMark = 'Z';
        const composer = await page.evaluate((mark) => {
          const input = document.getElementById('prompt-input');
          const rect = input?.getBoundingClientRect();
          const hit = rect ? document.elementFromPoint(rect.left + rect.width / 2, rect.top + Math.min(12, rect.height / 2)) : null;
          const hitPrompt = hit?.closest?.('#prompt-input') === input;
          // No input.focus() here: `focused` must be the PRODUCT's
          // auto-focus, not something the test inflicted.
          return {
            explorerGone: document.querySelector('section.file-explorer') == null,
            width: rect?.width ?? 0,
            height: rect?.height ?? 0,
            hitPrompt,
            hit: hit ? (hit.id ? `#${hit.id}` : (typeof hit.className === 'string' && hit.className ? hit.className : hit.tagName)) : '',
            focused: document.activeElement === input,
            mark,
          };
        }, typedMark);
        if (composer.hitPrompt && composer.focused) await page.keyboard.type(composer.mark);
        const scrollAfter = await readScroll();
        console.log(`[web-files-latency:scalars] composer-before ${JSON.stringify(scrollBefore)} composer-after ${JSON.stringify(scrollAfter)}`);
        const draftAfter = await page.evaluate(() => document.getElementById('prompt-input')?.value || '');
        const composerError = composerAfterExplorerCloseError(label, {
          ...composer,
          draftBefore,
          typed: draftAfter === `${draftBefore}${composer.mark}`,
          draftAfter,
        });
        if (composerError !== null) {
          const layout = await page.evaluate(() => {
            const root = document.getElementById('root');
            const rows = [...(root?.children || [])].map((node) => {
              const rect = node.getBoundingClientRect();
              const style = getComputedStyle(node);
              const marginTop = Number.parseFloat(style.marginTop) || 0;
              const marginBottom = Number.parseFloat(style.marginBottom) || 0;
              return { tag: node.tagName, id: node.id || '', className: typeof node.className === 'string' ? node.className : '', top: rect.top, height: rect.height, marginTop, marginBottom, flex: style.flex, flexBasis: style.flexBasis, minHeight: style.minHeight, cssHeight: style.height };
            });
            const measure = (node) => {
              const rect = node.getBoundingClientRect();
              const style = getComputedStyle(node);
              return { tag: node.tagName, id: node.id || '', className: typeof node.className === 'string' ? node.className : '', top: rect.top, bottom: rect.bottom, height: rect.height, clientHeight: node.clientHeight, scrollHeight: node.scrollHeight, scrollTop: node.scrollTop, marginTop: Number.parseFloat(style.marginTop) || 0, marginBottom: Number.parseFloat(style.marginBottom) || 0, flex: style.flex, minHeight: style.minHeight, overflow: style.overflow };
            };
            const prompt = document.getElementById('prompt-input');
            const ancestors = [];
            for (let node = prompt; node && node !== root; node = node.parentElement) ancestors.push(measure(node));
            if (root) ancestors.push(measure(root));
            const transcript = document.getElementById('transcript');
            const appLayout = document.querySelector('.app-layout');
            const footer = document.querySelector('footer');
            const sum = rows.reduce((total, row) => total + row.height + row.marginTop + row.marginBottom, 0);
            return { children: rows, ancestors, transcript: transcript ? measure(transcript) : null, layoutChildren: [...(appLayout?.children || [])].map(measure), footer: footer ? measure(footer) : null, rootClient: root?.clientHeight ?? null, rootScroll: root?.scrollHeight ?? null, rootScrollTop: root?.scrollTop ?? null, childSum: sum };
          });
          console.log(`[web-files-latency:scalars] composer-layout ${JSON.stringify(layout)}`);
          throw new Error(composerError);
        }
        await stageFrame(page, label.startsWith('desktop') ? 'files-latency-desktop-closed-composer.png' : 'files-latency-mobile-closed-composer.png', 'composer after Files closed');
        if (label.startsWith('desktop')) {
          // FL0.9 baseline at THIS 1440x900 view: Files is already closed, so
          // really open the Sessions rail and snapshot its corner rects — the
          // next keep reopens Files OVER these exact coordinates.
          if (!await page.evaluate(() => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true)) await openSessionsDrawer(page);
          await page.locator('#session-sidebar-search').waitFor({ state: 'visible', timeout: 10000 });
          sidebarCornerBaseline = await page.evaluate(() => {
            const rect = (node) => { if (!node) return null; const r = node.getBoundingClientRect(); return r.width >= 1 && r.height >= 1 ? { x: r.x, y: r.y, width: r.width, height: r.height } : null; };
            return { button: rect(document.getElementById('sidebar-manage-btn')), foot: rect(document.querySelector('.session-sidebar__foot')) };
          });
          if (!sidebarCornerBaseline.button || !sidebarCornerBaseline.foot) throw new Error(`${label}: pre-open sidebar corner baseline missing a displayed rect`);
        }
      });
      await keep('background-replies-restored', async () => {
        const replyError = backgroundReplyReachabilityError(`${label} Files closed`, await readBackgroundReplyReachability(page), true);
        if (replyError !== null) throw new Error(replyError);
      });
      await keep('header-closed', async () => {
        const headerClosed = await readHeaderClip();
        const scroll = headerClosed.scroll || {};
        console.log(`[web-files-latency:scalars] header-closed headerTop=${headerClosed.top} scrollY=${scroll.scrollY} scrollingTop=${scroll.scrollingTop} rootScroll=${scroll.rootScroll}`);
        const headerClosedError = headerClipError(`${label} Files closed`, headerClosed);
        if (headerClosedError !== null) throw new Error(headerClosedError);
        await page.fill('#prompt-input', '');
        if ((await page.locator('section.file-explorer').count()) === 0) {
          await openFilesFromHeader(page, `${label} reopen`);
        }
        await waitFor(
          page,
          () => document.querySelector('[data-testid="file-explorer-tree"] .file-explorer__row--dir') != null,
          `${label}: Files did not reopen after the composer check`,
        );
      });
      await keep('diff-open-file', async () => {
        const diffPath = seededFileTitle;
        const diffMarker = 'export const mod6_sub0_file0 = 1; // dirty';
        if ((await page.locator('section.file-explorer').count()) === 0) {
          await openFilesFromHeader(page, `${label} diff reopen`);
        }
        const showingDiff = await page.evaluate(({ title, marker }) => {
          const content = document.querySelector('[data-testid="file-explorer-content"]');
          const hidden = content == null || content.hidden === true || content.getAttribute('hidden') != null;
          const text = content?.textContent || '';
          return !hidden
            && content?.querySelector('.file-explorer__diff') != null
            && text.includes(marker)
            && !text.includes('No changes')
            && !text.includes('Empty file')
            && document.querySelector(`[data-testid="file-explorer-tree"] button[title="${title}"]`)?.classList.contains('file-explorer__row--active') === true;
        }, { title: diffPath, marker: diffMarker });
        if (!showingDiff) await openNonPreviewFile(`${label} diff`);
        await waitFor(
          page,
          ({ title, marker }) => {
            const content = document.querySelector('[data-testid="file-explorer-content"]');
            const text = content?.textContent || '';
            return content?.querySelector('.file-explorer__diff') != null
              && text.includes(marker)
              && !text.includes('No changes')
              && !text.includes('Empty file')
              && document.querySelector(`[data-testid="file-explorer-tree"] button[title="${title}"]`)?.classList.contains('file-explorer__row--active') === true;
          },
          `${label}: ${diffPath} diff never showed its dirty body`,
          20000,
          { title: diffPath, marker: diffMarker },
        );
        const before = await page.evaluate(() => window.__fxFrames.filter((frame) => frame.dir === 'out' && frame.type === 'file_explorer_read').length);
        const openFile = page.locator('[data-testid="file-explorer-open-file"]');

        await openFile.waitFor({ state: 'visible', timeout: 10000 });
        if ((await openFile.getAttribute('aria-label')) !== 'Open file') {
          throw new Error(`${label}: Open file control is not labeled Open file`);
        }
        await openFile.click();
        const cold = label.startsWith('desktop');
        await waitFor(
          page,
          ({ title, marker, reads, cold: wantCold }) => {
            const matching = window.__fxFrames.filter((frame) => frame.dir === 'out' && frame.type === 'file_explorer_read' && frame.path === title && frame.session === 'session-a-seed');
            const added = window.__fxFrames.filter((frame) => frame.dir === 'out' && frame.type === 'file_explorer_read').slice(reads);
            const content = document.querySelector('[data-testid="file-explorer-content"]');
            const text = content?.textContent || '';
            const shown = content?.querySelector('.file-explorer__diff') == null
              && text.includes(marker)
              && !text.includes('Empty file')
              && document.querySelector('[data-testid="file-explorer-open-file"]') == null;
            const exactCold = added.length === 1 && added[0].path === title && added[0].session === 'session-a-seed';
            const exactWarm = added.length === 0 && matching.length === 1;
            return shown && (wantCold ? exactCold : exactWarm);
          },
          `${label}: Open file did not ${cold ? 'read' : 'reuse the cached read of'} ${diffPath}`,
          20000,
          { title: diffPath, marker: diffMarker, reads: before, cold },
        );

        const stillFocused = await page.evaluate(() => document.querySelector('section.file-explorer')?.classList.contains('file-explorer--ask-focus') === true);
        if (stillFocused) throw new Error(`${label}: padding saw fullscreen still active after Open file`);
        const sourceError = sourceTopGapError(label, await readSourceTop());
        if (sourceError !== null) throw new Error(sourceError);
        await stageFrame(page, label.startsWith('desktop') ? 'files-latency-desktop-file-content.png' : 'files-latency-mobile-file-content.png', 'opened file body flush to the visible path strip bottom');

      });
      if (errors.length > 0) fail(`${label}: ${errors.join(' | ')}`);
      record(id);
    };


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
    await clickFeature(page, '#file-explorer-toggle-btn');
    // The reopen's listing refresh is SERIALIZED by the panel (root first,
    // then each expanded dir, one settled request at a time), so the
    // store-driven expanded row paints BEFORE this path's own
    // file_explorer_list is even dispatched. Waiting on the row alone sampled
    // the exact-one count at a moment when the target request did not exist
    // yet (observed delta 0). This existing wait therefore also requires THIS
    // path's request to have been issued since the baseline; the exact-count
    // assertion, timeouts, and the correlated-response check are unchanged.
    await waitFor(
      page,
      ({ dir, before }) => {
        const buttons = [...document.querySelectorAll('[data-testid="file-explorer-tree"] button.file-explorer__row--dir')];
        const expanded = buttons.some((b) => (b.querySelector('.file-explorer__row-path')?.textContent || '') === dir
          && b.getAttribute('aria-expanded') === 'true');
        const issued = window.__fxFrames.filter((f) => f.dir === 'out' && f.type === 'file_explorer_list' && f.path === dir).length;
        return expanded && issued > before;
      },
      'close/reopen did not restore the exact expansion geometry and dispatch its own path listing',
      20000,
      { dir: toggleDir, before: requestsBeforeClose.length }
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

    /* ---- T4: A/B session view isolation ----
       Open Files covers the session rail, so a switch closes it through the
       header control, clicks the now-visible row, then reopens Files on the
       accepted owner. Per-session tree geometry is the slot cache. */
    const staleShrink = await page.getByRole('button', { name: 'Shrink file explorer back to the panel' }).count();
    if (staleShrink !== 0) fail(`T4: obsolete Files shrink control is still mounted (${staleShrink})`);
    const openSession = async (sid) => {
      if (await page.locator('section.file-explorer').count()) {
        const close = page.locator('#file-explorer-drawer-close-btn');
        const closeBox = await close.boundingBox();
        if (!closeBox || closeBox.width < 1 || closeBox.height < 1) {
          fail(`${sid}: Files close is not visible before the session switch`);
        }
        await close.click();
        await waitFor(
          page,
          () => document.querySelector('section.file-explorer') === null,
          `${sid}: Files did not close before the session switch`,
          10000,
        );
      }
      // Session navigation is its own responsibility: the rail must be
      // exposed explicitly — never via a Files-entry side effect.
      if (!await page.evaluate(() => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true)) {
        await openSessionsDrawer(page);
      }
      await page.locator('#session-sidebar-search').waitFor({ state: 'visible', timeout: 10000 });
      const row = page.locator(`.session-sidebar__row [data-session-id="${sid}"]`);
      // Selected-only groups collapse non-active sessions by default, so the
      // row may not be mounted: the exact-sid search forces it to render,
      // the same helper shape the sessions lane uses (switchViaSearch).
      await page.fill('#session-sidebar-search', sid);
      await waitFor(
        page,
        (want) => [...document.querySelectorAll('.session-sidebar__switch')]
          .some((r) => r.dataset.sessionId === want),
        `${sid}: the exact-sid search never rendered the session row`,
        15000,
        sid
      );
      await row.scrollIntoViewIfNeeded();
      const rowBox = await row.boundingBox();
      if (!rowBox || rowBox.width < 1 || rowBox.height < 1) {
        fail(`${sid}: session row is not visible for the switch`);
      }
      const covered = await row.evaluate((node) => {
        const box = node.getBoundingClientRect();
        const hit = document.elementFromPoint(box.x + box.width / 2, box.y + Math.min(12, box.height / 2));
        return hit?.closest('.session-sidebar__switch') !== node;
      });
      if (covered) fail(`${sid}: session row is covered, so the click would hit another surface`);
      await row.click();
      await waitFor(
        page,
        (id) => document.getElementById('session-lifecycle')?.dataset.displayedSessionId === id
          && document.querySelector(`.session-sidebar__row--active [data-session-id="${id}"]`) != null,
        `${sid} never became the displayed owner`,
        30000,
        sid,
      );
      await page.fill('#session-sidebar-search', '');
      await waitFor(
        page,
        (want) => document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId === want,
        `${sid}: the active row never re-rendered after clearing the search`,
        15000,
        sid
      );
      await openFilesFromHeader(page, `${sid} reopen`);
      await waitFor(
        page,
        (id) => document.querySelector('section.file-explorer') !== null
          && document.getElementById('session-lifecycle')?.dataset.displayedSessionId === id,
        `${sid}: Files did not reopen on the displayed owner`,
        20000,
        sid,
      );
    };
    await openSession('session-b-seed');
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
    if (await page.locator('section.file-explorer').count()) {
      const close = page.locator('#file-explorer-drawer-close-btn');
      const closeBox = await close.boundingBox();
      if (!closeBox || closeBox.width < 1 || closeBox.height < 1) {
        fail('FL6: Files close is not visible before creating the subdirectory session');
      }
      await close.click();
      await waitFor(
        page,
        () => document.querySelector('section.file-explorer') === null,
        'FL6: Files did not close before the subdirectory session',
        10000,
      );
    }
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
    const openFilesFromHeaderHere = (label) => openFilesFromHeader(page, label);
    const clickTranscriptChip = async (marker, text) => {
      if (await page.locator('section.file-explorer').count()) {
        const close = page.locator('#file-explorer-drawer-close-btn');
        const closeBox = await close.boundingBox();
        if (!closeBox || closeBox.width < 1 || closeBox.height < 1) {
          fail(`${marker}: Files close is not visible before the transcript citation`);
        }
        await close.click();
        await waitFor(
          page,
          () => document.querySelector('section.file-explorer') === null,
          `${marker}: Files did not close before the transcript citation`,
          10000,
        );
      }
      // STABLE SEMANTIC RESOLUTION — the old flow bound the chip once,
      // then scrollIntoViewIfNeeded'd the bound element; the transcript
      // legitimately re-projects its window (and the Files close/restore
      // band shift reflows it), the <code> node behind the chip is
      // replaced, and the stale binding threw "detached". The chip's
      // identity is (marker message, chip text), so EVERY attempt here
      // re-resolves it FRESH in-page, scrolls the live node, and only
      // returns the coordinates when the fragment is hittable AND stable
      // across two rAFs. A genuinely missing/unhittable chip keeps the
      // deadline and fails with the last probe — nothing detached is
      // swallowed, and the real pointer click below stays real.
      const deadline = Date.now() + 20000;
      let lastProbe = null;
      for (;;) {
        const point = await page.evaluate(({ marker: mark, chipText }) => {
          const message = [...document.querySelectorAll('#transcript .transcript-surface:not([hidden]) .msg--assistant')]
            .find((node) => (node.textContent || '').includes(mark));
          if (!message) return null;
          const chipNode = [...message.querySelectorAll('code')]
            .find((code) => (code.textContent || '') === chipText);
          if (!chipNode) return null;
          const round = (rect) => ({ x: Math.round(rect.x), y: Math.round(rect.y), w: Math.round(rect.width), h: Math.round(rect.height) });
          const fragments = () => [...chipNode.getClientRects()].map(round);
          const hittableFragment = () => [...chipNode.getClientRects()]
            .find((rect) => rect.width >= 1 && rect.height >= 1 && rect.bottom > 0 && rect.top < innerHeight);
          chipNode.scrollIntoView({ block: 'center' });
          const fragment = hittableFragment();
          if (!fragment) return { fragments: fragments(), point: null, hit: null };
          const candidate = { x: fragment.x + fragment.width / 2, y: fragment.y + fragment.height / 2 };
          const hit = document.elementFromPoint(candidate.x, candidate.y);
          if (hit?.closest('code') !== chipNode) {
            return { fragments: fragments(), point: null, hit: hit ? `${hit.tagName}.${hit.id || hit.className || ''}` : null };
          }
          // Stability gate: the SAME node must still own a hittable
          // fragment with a fresh hit two rAFs later — the click fires at
          // the FRESH point, never a stale-coordinate comparison.
          return new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(() => {
            if (!chipNode.isConnected) return resolve(null);
            const stable = hittableFragment();
            if (!stable) return resolve(null);
            const stablePoint = { x: Math.round(stable.x + stable.width / 2), y: Math.round(stable.y + stable.height / 2) };
            const stableHit = document.elementFromPoint(stablePoint.x, stablePoint.y);
            if (stableHit?.closest('code') !== chipNode) {
              resolve({ fragments: fragments(), point: null, hit: stableHit ? `${stableHit.tagName}.${stableHit.id || stableHit.className || ''}` : null });
              return;
            }
            resolve({ fragments: fragments(), point: stablePoint, ownsChip: true });
          })));
        }, { marker, chipText: text });
        if (point && point.point && point.ownsChip) {
          await page.mouse.click(point.point.x, point.point.y);
          return;
        }
        lastProbe = point;
        if (Date.now() > deadline) {
          fail(`${marker}: transcript citation ${text} never stabilized as a hittable chip (${JSON.stringify(lastProbe)})`);
        }
        await page.waitForTimeout(100);
      }
    };
    const readsBeforeControl = await outgoingReadCount();
    await clickTranscriptChip(controlMarker, controlChip);
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
    // The MAIN composer is hidden while Files is open, so a composer prompt
    // must close the drawer first; the citation click below reopens it.
    if (await page.locator('section.file-explorer').count()) {
      await page.click('#file-explorer-drawer-close-btn');
      await waitFor(
        page,
        () => document.querySelector('section.file-explorer') === null,
        'FL6.2: Files did not close before the root-link composer prompt',
        10000,
      );
    }
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
    await clickTranscriptChip(rootMarker, shortChip);
    try {
      await waitFor(
        page,
        ({ before, exact, resolved }) => {
          // The wire may converge either as a miss-then-rooted retry or as a
          // single resolved read; the product contract is the OUTCOME: the
          // rooted file is the last successful read and renders exactly.
          const reads = window.__fxFrames.filter((frame) => frame.type === 'file_explorer_read').slice(before);
          const sawRequest = reads.some((frame) => frame.dir === 'out' && (frame.path === exact || frame.path === resolved));
          const lastSuccess = [...reads].reverse().find((frame) => frame.dir === 'in' && frame.success === true);
          const content = document.querySelector('[data-testid="file-explorer-content"]');
          return sawRequest
            && lastSuccess != null
            && lastSuccess.path === resolved
            && content != null
            && (content.textContent || '').includes('LINK-NESTED-EXACT')
            && content.querySelector('.file-explorer__error') == null;
        },
        'FL6.2: shortened citation did not open the rooted file',
        20000,
        { before: readsBeforeRoot, exact: nestedRel, resolved: rootedRel },
        { diagnostic: true },
      );
    } catch (error) {
      await fl6Dump('FL6.2', error);
    }
    record('FL6.2');
    const readsBeforeDup = await page.evaluate(() => window.__fxFrames.length);
    await clickTranscriptChip(rootMarker, `${dupBase}:1`);
    const dupA = `pkg/a/${dupBase}`;
    const dupB = `pkg/b/${dupBase}`;
    const framesAfter = (before) => page.evaluate((start) => window.__fxFrames.slice(start).map((frame) => ({
      dir: frame.dir, type: frame.type, path: frame.path || '', session: frame.session || '', id: frame.id,
      success: frame.success, code: frame.code || '', candidates: frame.candidates || [], error: frame.error,
    })), before);
    try {
      await waitFor(page, ({ first, second }) => {
        const values = [...document.querySelectorAll('[data-file-ambiguous-choice]')]
          .map((node) => node.getAttribute('data-file-ambiguous-choice') || '');
        const text = document.querySelector('[data-testid="file-explorer-content"]')?.textContent || '';
        return values.length === 2 && values[0] === first && values[1] === second
          && !text.includes('LINK-DUP-A') && !text.includes('LINK-DUP-B');
      }, 'FL6.3: dup-name.txt did not offer both typed choices without opening one', 20000,
      { first: dupA, second: dupB }, { diagnostic: true });
    } catch (error) {
      await fl6Dump('FL6.3', error);
    }
    const dupWire = await framesAfter(readsBeforeDup);
    const dupOut = dupWire.filter((frame) => frame.dir === 'out' && frame.type === 'file_explorer_read');
    const dupIn = dupWire.find((frame) => frame.dir === 'in' && frame.id === dupOut[0]?.id);
    if (dupOut.length !== 1 || dupOut[0].path !== dupBase || dupOut[0].session !== 'session-a-seed'
      || dupWire.some((frame) => frame.type === 'file_explorer_search' || frame.type === 'get_state')
      || dupIn?.success !== false || dupIn.code !== 'file_ambiguous'
      || JSON.stringify(dupIn.candidates) !== JSON.stringify([dupA, dupB])) {
      await fl6Dump('FL6.3-wire', new Error(`FL6.3: basename was not one typed ambiguity (${JSON.stringify(dupWire)})`));
    }
    const wireBeforeDupReopen = await page.evaluate(() => window.__fxFrames.length);
    await clickTranscriptChip(rootMarker, `${dupBase}:1`);
    try {
      await waitFor(page, ({ first, second }) => {
        const values = [...document.querySelectorAll('[data-file-ambiguous-choice]')]
          .map((node) => node.getAttribute('data-file-ambiguous-choice') || '');
        return values.length === 2 && values[0] === first && values[1] === second;
      }, 'FL6.3: reopening the basename dropped a typed choice', 10000,
      { first: dupA, second: dupB }, { diagnostic: true });
    } catch (error) {
      await fl6Dump('FL6.3-reopen', error);
    }
    const dupReopen = await framesAfter(wireBeforeDupReopen);
    if (dupReopen.some((frame) => frame.type === 'file_explorer_read' || frame.type === 'file_explorer_search' || frame.type === 'get_state')) {
      await fl6Dump('FL6.3-reopen-wire', new Error(`FL6.3: reopen rediscovered the basename (${JSON.stringify(dupReopen)})`));
    }
    record('FL6.3');
    await clickTranscriptChip(rootMarker, `${nestedRel}:1`);
    try {
      await waitFor(page, (marker) => {
        const text = document.querySelector('[data-testid="file-explorer-content"]')?.textContent || '';
        return text.includes(marker)
          && !text.includes('LINK-DUP-A')
          && !text.includes('LINK-OUTSIDE')
          && document.querySelector('[data-file-ambiguous-choice]') == null
          && document.querySelector('[data-testid="file-explorer-content"] .file-explorer__error') == null;
      }, 'FL6.4: rooted file was not restored before the outside citation', 20000, 'LINK-NESTED-EXACT', { diagnostic: true });
    } catch (error) {
      await fl6Dump('FL6.4-restore', error);
    }
    const readsBeforeOutside = await outgoingReadCount();

    await clickTranscriptChip(rootMarker, `${outsideAbs}:1`);
    await page.evaluate(() => new Promise((resolve) => setTimeout(resolve, 300)));
    const outsideReads = (await readFrames()).filter((frame) => frame.dir === 'out').slice(readsBeforeOutside);
    if (outsideReads.length !== 0 || await page.locator('section.file-explorer').count() !== 0) {
      await fl6Dump('FL6.4', new Error(`outside citation opened a file (${JSON.stringify({ outsideReads })})`));
    }
    await openFilesFromHeaderHere('FL6.4 reopen');
    await waitFor(
      page,
      () => (document.querySelector('[data-testid="file-explorer-content"]')?.textContent || '').includes('LINK-NESTED-EXACT')
        && !(document.querySelector('[data-testid="file-explorer-content"]')?.textContent || '').includes('LINK-OUTSIDE')
        && document.querySelector('[data-testid="file-explorer-content"] .file-explorer__error') == null,
      'FL6.4: reopening Files lost the rooted file or showed the outside file',
      10000,
    );
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
    // The MAIN composer is hidden while Files is open, so a composer prompt
    // must close the drawer first; the citation click below reopens it.
    if (await page.locator('section.file-explorer').count()) {
      await page.click('#file-explorer-drawer-close-btn');
      await waitFor(
        page,
        () => document.querySelector('section.file-explorer') === null,
        'FL6.5: Files did not close before the root-readme composer prompt',
        10000,
      );
    }
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
    await clickTranscriptChip(readmeMarker, readmeChip);
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
    const homonymMarker = `homonym-${laneNonce}`;
    const homonymChip = 'homonym.rs';
    const homonymA = 'pkg/a/bridge/homonym.rs';
    const homonymB = 'pkg/b/bridge/homonym.rs';
    // The MAIN composer is hidden while Files is open, so a composer prompt
    // must close the drawer first; the citation click below reopens it.
    if (await page.locator('section.file-explorer').count()) {
      await page.click('#file-explorer-drawer-close-btn');
      await waitFor(
        page,
        () => document.querySelector('section.file-explorer') === null,
        'FL6.6: Files did not close before the homonym composer prompt',
        10000,
      );
    }
    await page.fill('#prompt-input', `${homonymMarker}\nbare homonym cite \`${homonymChip}\` with no path context`);
    const homonymOwner = await page.evaluate(() =>
      document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '');
    if (homonymOwner !== 'session-a-seed') fail(`FL6.6: prompt filled while ${homonymOwner} was active`);
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      ({ mark, chip, sid }) => {
        const node = [...document.querySelectorAll('#transcript .transcript-surface:not([hidden]) .msg--assistant')]
          .find((el) => (el.textContent || '').includes(mark));
        const chipNode = node ? [...node.querySelectorAll('code')].find((code) => (code.textContent || '') === chip) : null;
        return (node?.closest('[data-session-id]')?.getAttribute('data-session-id') || '') === sid
          && chipNode?.getAttribute('data-md-path') === chip;
      },
      'FL6.6: bare homonym.rs citation did not render',
      45000,
      { mark: homonymMarker, chip: homonymChip, sid: 'session-a-seed' },
    );
    const readsBeforeHomonym = await page.evaluate(() => window.__fxFrames.length);
    await clickTranscriptChip(homonymMarker, homonymChip);
    try {
      await waitFor(page, ({ first, second }) => {
        const values = [...document.querySelectorAll('[data-file-ambiguous-choice]')]
          .map((node) => node.getAttribute('data-file-ambiguous-choice') || '');
        const text = document.querySelector('[data-testid="file-explorer-content"]')?.textContent || '';
        const search = document.querySelector('[data-testid="file-explorer-search"]')?.value || '';
        return values.length === 2 && values[0] === first && values[1] === second
          && search === '' && !text.includes('HOMONYM-A') && !text.includes('HOMONYM-B');
      }, 'FL6.6: homonym.rs did not offer both typed choices without opening one', 20000,
      { first: homonymA, second: homonymB }, { diagnostic: true });
    } catch (error) {
      await fl6Dump('FL6.6', error);
    }
    const homonymWire = await framesAfter(readsBeforeHomonym);
    const homonymOut = homonymWire.filter((frame) => frame.dir === 'out' && frame.type === 'file_explorer_read');
    const homonymIn = homonymWire.find((frame) => frame.dir === 'in' && frame.id === homonymOut[0]?.id);
    if (homonymOut.length !== 1 || homonymOut[0].path !== homonymChip || homonymOut[0].session !== 'session-a-seed'
      || homonymWire.some((frame) => frame.type === 'file_explorer_search' || frame.type === 'get_state')
      || homonymIn?.success !== false || homonymIn.code !== 'file_ambiguous'
      || JSON.stringify(homonymIn.candidates) !== JSON.stringify([homonymA, homonymB])) {
      await fl6Dump('FL6.6-wire', new Error(`FL6.6: basename was not one typed ambiguity (${JSON.stringify(homonymWire)})`));
    }
    await page.locator(`[data-file-ambiguous-choice="${homonymA}"]`).click();
    await waitFor(page, (path) => {
      const text = document.querySelector('[data-testid="file-explorer-content"]')?.textContent || '';
      return text.includes('HOMONYM-A') && !text.includes('HOMONYM-B')
        && (document.querySelector('button.file-explorer__row--active')?.getAttribute('title') || '') === path;
    }, 'FL6.6: pkg/a choice did not open HOMONYM-A only', 20000, homonymA);
    await assertAskFullscreen(page, 'FL6.6', homonymA, 'HOMONYM-A', 'HOMONYM-B', false);

    record('FL6.6');
    const rewardsCite = 'psy_node_redis/src/store/rewards.rs';
    const rewardsA = `psy-node/${rewardsCite}`;
    const rewardsB = `psy-node-fix/${rewardsCite}`;
    const rewardsMarker = `rewards-suffix-${laneNonce}`;
    const rewardsChip = `${rewardsCite}:1`;
    const choiceSel = '[data-file-ambiguous-choice], [data-testid="file-ambiguous-choices"] button, [data-testid="file-explorer-ambiguous"] button';
    const notFoundText = (value) => /file not found|file_unavailable|file is unavailable|不存在/i.test(value);
    // The MAIN composer is hidden while Files is open, so a composer prompt
    // must close the drawer first; the citation click below reopens it.
    if (await page.locator('section.file-explorer').count()) {
      await page.click('#file-explorer-drawer-close-btn');
      await waitFor(
        page,
        () => document.querySelector('section.file-explorer') === null,
        'FL6.7: Files did not close before the rewards composer prompt',
        10000,
      );
    }
    await page.fill('#prompt-input', `${rewardsMarker}\nexact suffix cite \`${rewardsChip}\` with no longer path`);
    const rewardsOwner = await page.evaluate(() =>
      document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '');
    if (rewardsOwner !== 'session-a-seed') fail(`FL6.7: prompt filled while ${rewardsOwner} was active`);
    await page.press('#prompt-input', 'Enter');
    try {
      await waitFor(page, ({ mark, chip, cite, sid }) => {
        const node = [...document.querySelectorAll('#transcript .transcript-surface:not([hidden]) .msg--assistant')]
          .find((el) => (el.textContent || '').includes(mark));
        const chipNode = node ? [...node.querySelectorAll('code')].find((code) => (code.textContent || '') === chip) : null;
        return (node?.closest('[data-session-id]')?.getAttribute('data-session-id') || '') === sid
          && chipNode?.getAttribute('data-md-path') === cite
          && (chipNode.getAttribute('data-md-resolved') ?? '') !== 'none';
      }, 'FL6.7: exact rewards.rs citation did not render as the user path', 45000,
      { mark: rewardsMarker, chip: rewardsChip, cite: rewardsCite, sid: 'session-a-seed' }, { diagnostic: true });
    } catch (error) {
      await fl6Dump('FL6.7-render', error);
    }
    const wireBeforeRewards = await page.evaluate(() => window.__fxFrames.length);
    await clickTranscriptChip(rewardsMarker, rewardsChip);
    try {
      await waitFor(page, ({ cite, pathA, pathB, sel }) => {
        const labels = [...document.querySelectorAll(sel)].map((node) => (node.textContent || '').trim());
        const surfaced = `${[...document.querySelectorAll('#toasts .toast--error')].map((node) => node.textContent || '').join('\n')}\n${document.querySelector('[data-testid="file-explorer-content"] .file-explorer__error')?.textContent || ''}`;
        if (/file not found|file_unavailable|file is unavailable|不存在/i.test(surfaced)) {
          throw new Error(`FL6.7: file-not-found verdict for two real suffix copies (${JSON.stringify(surfaced)})`);
        }
        return labels.includes(pathA) && labels.includes(pathB)
          && (document.querySelector('[data-testid="file-explorer-search"]')?.value ?? '') !== cite;
      }, 'FL6.7: expected both full relative paths as choices, not a file-not-found verdict', 20000,
      { cite: rewardsCite, pathA: rewardsA, pathB: rewardsB, sel: choiceSel }, { diagnostic: true });
    } catch (error) {
      await fl6Dump('FL6.7-choices', error);
    }
    const rewardsWire = await page.evaluate((before) => window.__fxFrames.slice(before).map((frame) => ({
      dir: frame.dir, type: frame.type, path: frame.path || '', session: frame.session || '', code: frame.code || '', error: frame.error,
    })), wireBeforeRewards);
    const rewardsReads = rewardsWire.filter((frame) => frame.dir === 'out' && frame.type === 'file_explorer_read');
    const rewardsReply = rewardsWire.find((frame) => frame.dir === 'in' && frame.type === 'file_explorer_read');
    if (rewardsReads.length !== 1 || rewardsReads[0].path !== rewardsCite || rewardsReads[0].session !== 'session-a-seed'
      || rewardsWire.some((frame) => frame.type === 'file_explorer_search' || frame.type === 'get_state')
      || notFoundText(`${rewardsReply?.code || ''} ${rewardsReply?.error || ''}`)) {
      await fl6Dump('FL6.7-wire', new Error(`FL6.7: citation rediscovered instead of one owner read (${JSON.stringify(rewardsWire)})`));
    }
    const rewardsPass = async (label, shot) => {
      if (await page.locator('section.file-explorer').count() === 0) await openFilesFromHeaderHere(label);
      const choicesVisible = async () => page.evaluate(({ pathA, pathB, sel }) => {
        const labels = [...document.querySelectorAll(sel)].map((node) => (node.textContent || '').trim());
        return labels.includes(pathA) && labels.includes(pathB);
      }, { pathA: rewardsA, pathB: rewardsB, sel: choiceSel });
      if (!(await choicesVisible())) {
        // The chooser is a render branch of a NULL activePath, so toggling
        // the tree can never create it. Re-enter the citation through the
        // REAL transcript chip (close -> click): App's stored-choices path
        // then reopens the drawer with activePath null and the SAME candidates
        // and issues zero file_explorer_read/search/get_state RPCs. This is
        // what the mobile pass depends on — the desktop pass left the file
        // open, so the chooser is no longer rendered anywhere.
        await clickTranscriptChip(rewardsMarker, rewardsChip);
      }
      try {
        await waitFor(page, ({ pathA, pathB, sel }) => {
          const labels = [...document.querySelectorAll(sel)].map((node) => (node.textContent || '').trim());
          return labels.includes(pathA) && labels.includes(pathB);
        }, `${label}: choices missing`, 20000,
        { pathA: rewardsA, pathB: rewardsB, sel: choiceSel }, { diagnostic: true });
      } catch (error) {
        await fl6Dump(`${label}-open`, error);
      }
      const beforeChoice = await page.evaluate(() => window.__fxFrames.length);
      await page.locator(choiceSel, { hasText: rewardsA }).click();
      await waitFor(page, ({ want, other }) => {
        const text = document.querySelector('[data-testid="file-explorer-content"]')?.textContent || '';
        return text.includes(want) && !text.includes(other);
      }, `${label}: A content missing`, 20000, { want: 'REWARDS-NODE-A', other: 'REWARDS-NODE-FIX-B' });
      const choiceWire = await page.evaluate((start) => window.__fxFrames.slice(start).filter((frame) => frame.dir === 'out').map((frame) => ({
        type: frame.type, path: frame.path || '', session: frame.session || '',
      })), beforeChoice);
      const choiceReads = choiceWire.filter((frame) => frame.type === 'file_explorer_read');
      const shownA = await page.evaluate((path) => {
        const text = document.querySelector('[data-testid="file-explorer-content"]')?.textContent || '';
        const active = document.querySelector('button.file-explorer__row--active')?.getAttribute('title') || '';
        return text.includes('REWARDS-NODE-A') && !text.includes('REWARDS-NODE-FIX-B') && active === path;
      }, rewardsA);
      const priorA = await page.evaluate((path) => window.__fxFrames.filter((frame) => frame.dir === 'out' && frame.type === 'file_explorer_read' && frame.path === path && frame.session === 'session-a-seed').length, rewardsA);
      const exactCold = choiceReads.length === 1 && choiceReads[0].path === rewardsA && choiceReads[0].session === 'session-a-seed';
      const exactWarm = choiceReads.length === 0 && priorA === 1 && shownA;
      if ((label.endsWith('desktop') ? !exactCold : !exactWarm)
        || choiceWire.some((frame) => frame.type === 'file_explorer_search' || frame.type === 'get_state' || (frame.type === 'file_explorer_list' && frame.path === rewardsCite))) {
        await fl6Dump(`${label}-choice`, new Error(`${label}: ${label.endsWith('desktop') ? 'cold' : 'warm'} A was not exact (${JSON.stringify({ choiceWire, priorA, shownA })})`));
      }
      // EXPLORER REVEAL after the citation jump (read-only; nothing here
      // expands a directory, clicks a toggle, or scrolls). Desktop keeps the
      // sidebar open, so the pane, the exact selected row, the expanded
      // ancestor chain AND the row's position inside the tree scrollport are
      // asserted; the compact mobile pane may still be collapsed, so only the
      // DOM selection identity is asserted there while the content path is
      // checked on the visible content pane.
      const revealTag = label.endsWith('mobile') ? 'mobile' : 'desktop';
      // Render settle ONLY: the parent-chain listings and the one reveal
      // scroll land in later commits than the content read. The loop re-samples
      // until the gate passes or the deadline; the sample that the gate judges
      // below is itself authoritative, so the loop can shorten the wait but
      // can never turn a failing sample into a pass.
      const revealDeadline = Date.now() + 15000;
      let treeReveal = await page.evaluate(explorerTreeRevealView, { path: rewardsA });
      let treeRevealIssue = explorerTreeRevealError(label, treeReveal, rewardsA, revealTag === 'desktop');
      while (treeRevealIssue !== null && Date.now() < revealDeadline) {
        await page.waitForTimeout(100);
        treeReveal = await page.evaluate(explorerTreeRevealView, { path: rewardsA });
        treeRevealIssue = explorerTreeRevealError(label, treeReveal, rewardsA, revealTag === 'desktop');
      }
      if (treeRevealIssue) await revealFailure(page, label, `files-latency-reveal-tree-${revealTag}.png`, treeReveal, treeRevealIssue);
      const contentReveal = await page.evaluate(explorerContentPathView, { path: rewardsA });
      const contentRevealIssue = explorerContentPathError(label, contentReveal, rewardsA);
      if (contentRevealIssue) await revealFailure(page, label, `files-latency-reveal-content-${revealTag}.png`, contentReveal, contentRevealIssue);
      await stageFrame(page, `files-latency-reveal-${revealTag}.png`, `${revealTag}: cited file selected in the tree and named at the content top`);
      record(revealTag === 'desktop' ? 'FL6.8' : 'FL6.9');
      await assertAskFullscreen(page, label, rewardsA, 'REWARDS-NODE-A', 'REWARDS-NODE-FIX-B', false);
      const beforeReopen = await page.evaluate(() => window.__fxFrames.length);
      await clickTranscriptChip(rewardsMarker, rewardsChip);
      if (!(await choicesVisible())) {
        const treeBtn = page.locator('#file-explorer-tree-toggle');
        const box = await treeBtn.boundingBox();
        const hittable = box && box.width >= 1 && box.height >= 1 && await treeBtn.evaluate((node) => {
          const rect = node.getBoundingClientRect();
          const hit = document.elementFromPoint(rect.x + rect.width / 2, rect.y + rect.height / 2);
          return hit?.closest('#file-explorer-tree-toggle') === node;
        });
        if (!hittable) fail(`${label}: choices are hidden and the tree toggle is not the visible control`);
        await treeBtn.click();
      }
      try {
        await waitFor(page, ({ pathA, pathB, sel }) => {
          const labels = [...document.querySelectorAll(sel)].map((node) => (node.textContent || '').trim());
          return labels.includes(pathA) && labels.includes(pathB);
        }, `${label}: reopen dropped a choice`, 20000,
        { pathA: rewardsA, pathB: rewardsB, sel: choiceSel }, { diagnostic: true });
      } catch (error) {
        await fl6Dump(`${label}-reopen`, error);
      }
      const reopened = await page.evaluate((start) => window.__fxFrames.slice(start).map((frame) => ({
        dir: frame.dir, type: frame.type, path: frame.path || '',
      })), beforeReopen);
      if (reopened.some((frame) => frame.type === 'file_explorer_search' || frame.type === 'get_state' || frame.type === 'file_explorer_read' || (frame.dir === 'out' && frame.type === 'file_explorer_list' && frame.path === rewardsCite))) {
        await fl6Dump(`${label}-wire`, new Error(`${label}: citation rediscovered (${JSON.stringify(reopened)})`));
      }
      const owner = await page.evaluate(() =>
        document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '');
      if (owner !== 'session-a-seed') fail(`${label}: owner changed to ${owner}`);
      await page.locator(choiceSel, { hasText: rewardsB }).click();
      await waitFor(page, ({ want, other }) => {
        const text = document.querySelector('[data-testid="file-explorer-content"]')?.textContent || '';
        return text.includes(want) && !text.includes(other);
      }, `${label}: B content missing`, 20000, { want: 'REWARDS-NODE-FIX-B', other: 'REWARDS-NODE-A' });
      await stageShot(page, shot);
    };
    await rewardsPass('FL6.7 desktop', 'files-latency-rewards-suffix.png');
    await page.setViewportSize({ width: 390, height: 844 });
    await rewardsPass('FL6.7 mobile', 'files-latency-rewards-suffix-mobile.png');
    await page.setViewportSize({ width: 1440, height: 900 });
    record('FL6.7');
    await stageShot(page, 'files-latency-subdir-link.png');
    await page.setViewportSize({ width: 1440, height: 900 });
    const treeBtn = page.locator('#file-explorer-tree-toggle');
    if (await treeBtn.isVisible()) await treeBtn.click();
    const search = page.locator('[data-testid="file-explorer-search"]');
    await search.waitFor({ state: 'visible', timeout: 10000 });
    await search.fill('mod6');
    await search.fill('');
    await waitFor(page, () => document.querySelector('[data-testid="file-explorer-tree"] .file-explorer__row--dir') != null && document.querySelector('[data-file-ambiguous-choice]') == null, 'clearing search did not restore the workspace tree');
    await assertExplorerJourney('desktop 1440x900 Files open', 'FL0.7');
    const readCornerLeak = () => page.evaluate((baseline) => {
      const rect = (node) => {
        if (!node) return null;
        const r = node.getBoundingClientRect();
        return r.width > 0 || r.height > 0 ? { x: r.x, y: r.y, width: r.width, height: r.height } : null;
      };
      const explorerNode = document.querySelector('section.file-explorer');
      const explorer = rect(explorerNode);
      const pointState = (x, y) => {
        const node = document.elementFromPoint(x, y);
        return {
          x: Math.round(x * 10) / 10,
          y: Math.round(y * 10) / 10,
          sidebarFoot: Boolean(node && (node.id === 'sidebar-manage-btn' || node.closest?.('#sidebar-manage-btn, .session-sidebar__foot'))),
          hitExplorer: Boolean(explorerNode && (node === explorerNode || node.closest?.('section.file-explorer') === explorerNode)),
          hit: node ? (node.id ? `#${node.id}` : (typeof node.className === 'string' ? node.className : node.tagName)) : '',
        };
      };
      const probes = [];
      for (const [target, box] of [['button', baseline?.button], ['foot', baseline?.foot]]) {
        if (!box) continue;
        const probe = pointState(box.x + box.width / 2, box.y + box.height / 2);
        probe.target = target;
        probe.insideExplorer = Boolean(explorer
          && probe.x >= explorer.x && probe.x <= explorer.x + explorer.width
          && probe.y >= explorer.y && probe.y <= explorer.y + explorer.height);
        probes.push(probe);
      }
      const composerBox = rect(document.getElementById('prompt-input'));
      const composerNode = composerBox
        ? document.elementFromPoint(composerBox.x + composerBox.width / 2, composerBox.y + composerBox.height / 2)
        : null;
      return {
        explorer,
        button: baseline?.button ?? null,
        foot: baseline?.foot ?? null,
        probes,
        composerHittable: Boolean(composerNode && (composerNode.id === 'prompt-input' || composerNode.closest?.('#prompt-input'))),
      };
    }, sidebarCornerBaseline);
    const cornerSample = await readCornerLeak();
    // Corner evidence frame lands in the ACTUAL evidence root (not staging)
    // so a RED leaves the bottom-left corner inspectable — clipped to the
    // sidebar-foot/composer band plus the drawer's bottom edge above it.
    const cornerFrame = path.join(evidence, 'files-latency-desktop-corner.png');
    await page.screenshot({ path: cornerFrame, clip: { x: 0, y: 640, width: 720, height: 260 } });
    console.log('[web-files-latency:frame] files-latency-desktop-corner.png Files open bottom-left corner (New Session band)');
    const cornerError = explorerCornerLeakError('desktop 1440x900 corner', cornerSample);
    if (cornerError !== null) fail(cornerError);
    console.log('[web-files-latency:check] desktop 1440x900 Files open corner-leak PASS');
    record('FL0.9');
    await page.setViewportSize({ width: 390, height: 844 });
    await assertExplorerJourney('mobile 390x844 Files open', 'FL0.8');



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