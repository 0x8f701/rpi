#!/usr/bin/env node
// File explorer OUTER drawer width resize contract (App.tsx + styles.css).
//
// Covers the desktop outer drawer width feature end to end at the unit level:
//   * pure clamp/storage helpers imported from App.tsx (node-import-safe:
//     every module-level window touch in App is guarded, so importing the
//     module in a server context is fine),
//   * the one boring, non-secret localStorage key,
//   * keyboard step controller,
//   * SSR/source semantics of App.tsx (typeof-window guard, ARIA separator
//     markup, desktop-only guards),
//   * stylesheet semantics: reservation, drawer, and resizer all read the ONE
//     width authority (--file-explorer-width) with the same min(...) guard,
//     and the resizer is hidden below the 936px overlay breakpoint.
//
// Run through `npm run build` (esbuild-bundled like transcript.test.ts, which
// also imports ../src/App.tsx).
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  FILE_EXPLORER_WIDTH_DEFAULT,
  FILE_EXPLORER_WIDTH_KEY,
  FILE_EXPLORER_WIDTH_MAX,
  FILE_EXPLORER_WIDTH_MIN,
  FILE_EXPLORER_WIDTH_STEP,
  FILE_EXPLORER_CONVERSATION_MIN,
  FILE_EXPLORER_SIDEBAR_OPEN,
  FILE_EXPLORER_SIDEBAR_COLLAPSED,
  applyFileExplorerWidth,
  clampFileExplorerWidth,
  fileExplorerOverlayActive,
  fileExplorerWidthCeiling,
  readStoredFileExplorerWidth,
  stepFileExplorerWidth,
  writeStoredFileExplorerWidth,
} from '../src/App.tsx';

const failures: string[] = [];
let ran = 0;
function check(name: string, cond: unknown, detail?: string): void {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

/* ------------------------------------------------------------------ *
 * Browser-surface stubs (window.localStorage / window.innerWidth /
 * document.documentElement.style). Assigned on globalThis before any
 * helper call; App's bare `window` / `document` identifiers resolve to
 * these at call time. Named cast: test-only DOM stand-ins, runtime shape
 * is exercised by every helper call below.
 * ------------------------------------------------------------------ */
const store = new Map<string, string>();
let storageOk = true;
let innerWidth = 1280;
const cssVars: Array<[string, string]> = [];

interface TestWindow {
  readonly innerWidth: number;
  matchMedia(): { matches: boolean };
  get localStorage(): {
    getItem(k: string): string | null;
    setItem(k: string, v: string): void;
  };
}
interface TestDocument {
  documentElement: { style: { setProperty(k: string, v: string): void } };
}
const testGlobal = globalThis as unknown as { window?: TestWindow; document?: TestDocument };

testGlobal.window = {
  get innerWidth() {
    return innerWidth;
  },
  matchMedia: () => ({ matches: false }),
  get localStorage() {
    if (!storageOk) throw new Error('storage blocked');
    return {
      getItem: (k: string) => (store.has(k) ? store.get(k)! : null),
      setItem: (k: string, v: string) => void store.set(k, v),
    };
  },
};
testGlobal.document = {
  documentElement: {
    style: {
      setProperty: (k: string, v: string) => void cssVars.push([k, v]),
    },
  },
};

/* ------------------------------------------------------------------ *
 * 1. Key + constants
 * ------------------------------------------------------------------ */
check('storage key is the one boring global key', FILE_EXPLORER_WIDTH_KEY === 'rpi-file-explorer-width');
check('default drawer width is 607px (506 + 20 percent)', FILE_EXPLORER_WIDTH_DEFAULT === 607);
check(
  'key carries nothing secret (no auth prefix, no host/session parts)',
  !FILE_EXPLORER_WIDTH_KEY.startsWith('rpi-auth.')
    && !FILE_EXPLORER_WIDTH_KEY.includes(':')
    && !FILE_EXPLORER_WIDTH_KEY.includes('#'),
);
check('conversation minimum is 576px (>=520, Send + composer survive beside the drawer)', FILE_EXPLORER_CONVERSATION_MIN === 576);
check('sidebar widths: 230 open / 44 collapsed (match styles.css)', FILE_EXPLORER_SIDEBAR_OPEN === 230 && FILE_EXPLORER_SIDEBAR_COLLAPSED === 44);
check(
  'ceiling at 1280px defaults to the OPEN 230px rail: 1280 − 230 − 576 = 474 (rail + conversation min reserved; the naive 1280 − 576 = 704 lets the drawer cover Send)',
  fileExplorerWidthCeiling(1280) === 474,
);
check(
  'ceiling at 1280px with the collapsed 44px rail: 1280 − 44 − 576 = 660',
  fileExplorerWidthCeiling(1280, FILE_EXPLORER_SIDEBAR_COLLAPSED) === 660,
);
check(
  'ceiling with a zero sidebar is 1280 − 576 = 704',
  fileExplorerWidthCeiling(1280, 0) === 704,
);
check('ceiling at 1000px with a zero rail equals viewport minus the 576px conversation minimum', fileExplorerWidthCeiling(1000, 0) === 424);
check('ceiling saturates at 900px on wide viewports', fileExplorerWidthCeiling(2000, 0) === 900);
check('ceiling at the 936px fit breakpoint is the 360px floor', fileExplorerWidthCeiling(936) === FILE_EXPLORER_WIDTH_MIN);
check('ceiling below 936px stays the 360px floor (overlay owns the drawer)', fileExplorerWidthCeiling(935) === FILE_EXPLORER_WIDTH_MIN);
check('ceiling at the 720px mobile breakpoint is the 360px floor (mobile is overlay-covered)', fileExplorerWidthCeiling(720) === FILE_EXPLORER_WIDTH_MIN);
check('ceiling on non-finite viewport falls back to the hard cap', fileExplorerWidthCeiling(Number.NaN) === FILE_EXPLORER_WIDTH_MAX);

check('overlay is inactive when sidebar + conversation + floor fit', !fileExplorerOverlayActive(1000, 0));
check('overlay engages on 1000px with the open 230px rail (1000 − 230 − 576 = 194 < 360)', fileExplorerOverlayActive(1000));
check('overlay boundary: exactly 360px leftover (1166px) is still side-by-side', !fileExplorerOverlayActive(1166, FILE_EXPLORER_SIDEBAR_OPEN));
check('overlay boundary: one pixel below 360 leftovers flips to overlay', fileExplorerOverlayActive(1165, FILE_EXPLORER_SIDEBAR_OPEN));
check('clamp raises values below the 360px floor', clampFileExplorerWidth(100, 1280) === 360);
check('clamp caps above the viewport ceiling (474 at 1280 with the open rail)', clampFileExplorerWidth(100000, 1280) === 474);
check('clamp respects the hard 900px cap independent of huge viewports', clampFileExplorerWidth(100000, 5000, 0) === 900);
check('clamp keeps in-range values', clampFileExplorerWidth(400, 1280) === 400);
check('clamp maps NaN to the default width', clampFileExplorerWidth(Number.NaN, 1280) === FILE_EXPLORER_WIDTH_DEFAULT);
check('clamp rounds fractional drag widths', clampFileExplorerWidth(400.6, 1280) === 401);
check('clamp never returns a non-integer', Number.isInteger(clampFileExplorerWidth(678.5, 970, 0)));
/* ------------------------------------------------------------------ *
 * 3. Storage round-trip (clamp-on-write, clamp-on-read, corruption)
 * ------------------------------------------------------------------ */
writeStoredFileExplorerWidth(600, 1280);
check('stored value is the plain clamped number only (no secrets)', store.get(FILE_EXPLORER_WIDTH_KEY) === '474');
check('read returns the stored value', readStoredFileExplorerWidth(1280) === 474);
store.clear();
writeStoredFileExplorerWidth(50, 1280);
check('write clamps below the floor', store.get(FILE_EXPLORER_WIDTH_KEY) === '360');

store.clear();
writeStoredFileExplorerWidth(100000, 1000);
check('write clamps to the viewport ceiling (1000 − 230 − 576 < 360 → floor)', store.get(FILE_EXPLORER_WIDTH_KEY) === '360');

store.set(FILE_EXPLORER_WIDTH_KEY, '100000');
check('read clamps a stale wide value against the current viewport', readStoredFileExplorerWidth(1000) === 360);

store.set(FILE_EXPLORER_WIDTH_KEY, 'garbage');
check('read maps corrupt payloads to the default', readStoredFileExplorerWidth(1280) === FILE_EXPLORER_WIDTH_DEFAULT);

store.clear();
check('empty store reads as the default', readStoredFileExplorerWidth(1280) === FILE_EXPLORER_WIDTH_DEFAULT);

const widthBeforeBlockedRead = readStoredFileExplorerWidth(1280);
storageOk = false;
check('blocked storage reads as the default', readStoredFileExplorerWidth(1280) === FILE_EXPLORER_WIDTH_DEFAULT && widthBeforeBlockedRead === FILE_EXPLORER_WIDTH_DEFAULT);
let blockedWriteThrew = false;
try {
  writeStoredFileExplorerWidth(600, 1280);
} catch {
  blockedWriteThrew = true;
}
check('blocked storage on write does not throw', !blockedWriteThrew);
storageOk = true;

/* ------------------------------------------------------------------ *
 * 4. CSS-var application (one width authority)
 * ------------------------------------------------------------------ */
cssVars.length = 0;
applyFileExplorerWidth(400, 1280);
check(
  'apply writes --file-explorer-width on the document element',
  cssVars.length > 0
    && cssVars[cssVars.length - 1][0] === '--file-explorer-width'
    && cssVars[cssVars.length - 1][1] === '400px',
);
applyFileExplorerWidth(100000, 1000);
check('apply clamps against the current viewport ceiling', cssVars[cssVars.length - 1][1] === '360px');

/* ------------------------------------------------------------------ *
 * 5. Keyboard step controller
 * ------------------------------------------------------------------ */
check('step grows by one step (ArrowLeft = drawer grows)', stepFileExplorerWidth(400, 1, 1280) === 400 + FILE_EXPLORER_WIDTH_STEP);
check('step shrinks by one step (ArrowRight)', stepFileExplorerWidth(400, -1, 1280) === 400 - FILE_EXPLORER_WIDTH_STEP);
check('step clamps at the floor', stepFileExplorerWidth(368, -1, 1280) === FILE_EXPLORER_WIDTH_MIN);
check('step clamps at the sidebar-aware ceiling (474 at 1280, open rail)', stepFileExplorerWidth(464, 1, 1280) === 474);
store.clear();
check('step stays pure (never touches storage)', stepFileExplorerWidth(400, 1, 1280) === 416 && store.size === 0);

store.set(FILE_EXPLORER_WIDTH_KEY, 'garbage');
check('read maps corrupt payloads to the default', readStoredFileExplorerWidth(1280) === FILE_EXPLORER_WIDTH_DEFAULT);

store.clear();
check('empty store reads as the default', readStoredFileExplorerWidth(1280) === FILE_EXPLORER_WIDTH_DEFAULT);



/* ------------------------------------------------------------------ *
 * 6. App.tsx source semantics (SSR safety + accessible separator wiring)
 * ------------------------------------------------------------------ */
const here = dirname(fileURLToPath(import.meta.url));
const app = readFileSync(join(here, '..', 'src', 'App.tsx'), 'utf8');

check(
  'initial width state is SSR-safe (typeof window guard before the storage read)',
  app.includes("typeof window !== 'undefined' ? readStoredFileExplorerWidth("),
);
check('resizer element is mounted', app.includes('id="file-explorer-resizer"'));
check('resizer is gated on the open/collapsed drawer state', /fileExplorerOpen && \(/.test(app));
const resizerIdx = app.indexOf('file-explorer-resizer');
const resizerWindow = resizerIdx >= 0 ? app.slice(resizerIdx, resizerIdx + 1000) : '';
check('resizer has role separator', resizerWindow.includes('role="separator"'));
check('resizer is a vertical separator', resizerWindow.includes('aria-orientation="vertical"'));
check(
  'resizer exposes aria-valuemin / aria-valuemax / aria-valuenow',
  resizerWindow.includes('aria-valuemin')
    && resizerWindow.includes('aria-valuemax')
    && resizerWindow.includes('aria-valuenow'),
);
check('resizer has an accessible name', resizerWindow.includes('aria-label'));
check('resizer is keyboard focusable', resizerWindow.includes('tabIndex={0}'));
check(
  'resizer wires pointer drag',
  resizerWindow.includes('onPointerDown')
    && resizerWindow.includes('onPointerMove')
    && resizerWindow.includes('onPointerUp')
    && resizerWindow.includes('onPointerCancel'),
);
check('resizer wires keyboard step', resizerWindow.includes('onKeyDown'));
check(
  'keyboard handler maps ArrowLeft (grow), ArrowRight (shrink), Home (max), End (min)',
  /ArrowLeft[\s\S]{0,200}stepFileExplorerWidth\([^\n]*, 1,/.test(app)
    && /ArrowRight[\s\S]{0,200}stepFileExplorerWidth\([^\n]*, -1,/.test(app)
    && /Home[\s\S]{0,200}fileExplorerWidthCeiling\(/.test(app)
    && /End[\s\S]{0,200}FILE_EXPLORER_WIDTH_MIN/.test(app),
);
check(
  'pointer drag grows the drawer when dragging left (drawer is right-side)',
  /startX: e\.clientX[\s\S]{0,400}drag\.startX - e\.clientX/.test(app),
);
check(
  'pointer/keyboard handlers stay desktop-only (≤720px guarded)',
  (app.match(/window\.matchMedia\('\(max-width: 720px\)'\)\.matches/g) ?? []).length >= 2,
);
check(
  'persisted value is the bare clamped number (numeric-string only)',
  /String\(clampFileExplorerWidth\([^;]*\)\)/.test(app),
);

/* ------------------------------------------------------------------ *
 * 6b. Overlay layout + header drawer close (App.tsx + panel source)
 * ------------------------------------------------------------------ */
check('app layout gains the --files-overlay class on the overlay flip', app.includes('app-layout--files-overlay'));
check('overlay flag comes from the sidebar-aware helper', /fileExplorerOverlayActive\(viewportWidth[\s\S]{0,80}fileExplorerSidebarPx\)/.test(app));
check('overlay viewport width is live React state', app.includes('const [viewportWidth, setViewportWidth]') && app.includes('setViewportWidth(viewportWidthNow)'));
check('App no longer passes an overlay close prop', !app.includes('overlay={typeof window') && !app.includes('viewState={') && !app.includes('onViewStateChange'));

const explorerPanel = readFileSync(join(here, '..', 'src', 'panels', 'FileExplorerPanel.tsx'), 'utf8');
const bodyIdx = explorerPanel.indexOf('file-explorer__body');
const filesHeadIdx = explorerPanel.indexOf('file-explorer__head');
const closeBtnIdx = explorerPanel.indexOf("id={fileExplorerControlId(toggleId, 'drawer-close-btn')}");
const filesHeadBlock = filesHeadIdx >= 0 && bodyIdx > filesHeadIdx
  ? explorerPanel.slice(filesHeadIdx, bodyIdx)
  : '';
check(
  'drawer close lives in the Files header',
  filesHeadIdx >= 0
    && closeBtnIdx > filesHeadIdx
    && closeBtnIdx < bodyIdx
    && explorerPanel.includes("FILE_EXPLORER_CLOSE_BTN_ID = 'file-explorer-drawer-close-btn'")
    && !explorerPanel.includes('file-explorer-hide-btn'),
);
check(
  'drawer close names Close file explorer and is not the title',
  filesHeadBlock.includes('aria-label="Close file explorer"')
    && filesHeadBlock.includes('onClick={closeDrawer}')
    && !filesHeadBlock.includes('file-explorer__title-button')
    && !filesHeadBlock.includes('Hide file explorer'),
);

const reviewPanel = readFileSync(join(here, '..', 'src', 'panels', 'CodeReviewPanel.tsx'), 'utf8');
check('code review tree toggle button exists', reviewPanel.includes('id="code-review-tree-toggle"'));
check(
  'tree toggle wires aria-pressed + contextual copy',
  reviewPanel.includes('className="code-review__tree-toggle"')
    && reviewPanel.includes('aria-pressed={treeCollapsed}')
    && reviewPanel.includes("treeCollapsed ? 'Expand file tree' : 'Collapse file tree'"),
);
check(
  'tree collapse is a section modifier class (code-review--tree-collapsed)',
  reviewPanel.includes("code-review${treeCollapsed ? ' code-review--tree-collapsed' : ''}"),
);
const fileListIdx = reviewPanel.indexOf('<CodeReviewFileList');
check(
  'file tree stays mounted (collapse never gates the component)',
  fileListIdx > 0 && !reviewPanel.slice(Math.max(0, fileListIdx - 40), fileListIdx).includes('treeCollapsed'),
);
check(
  'narrow viewports default the review tree collapsed',
  /const \[treeCollapsed, setTreeCollapsed\] = useState\(\(\) => window\.matchMedia\(NARROW_QUERY\)\.matches\)/.test(reviewPanel),
);

/* ------------------------------------------------------------------ *
 * 7. styles.css semantics (one width authority, overlay below 936px)
 * ------------------------------------------------------------------ */
const css = readFileSync(join(here, '..', 'src', 'styles.css'), 'utf8');
// The ONE authority expression: the 576px conversation-min guard is
// subtracted from the viewport before the drawer, so the ACTUAL conversation
// column never drops below 576px (URL-only cutover: one route per document,
// no reserved chrome beside it).
const CALC = String.raw`calc\(100vw - 576px\)`;
const MIN_EXPR = String.raw`min\(var\(--file-explorer-width, 506px\), 70vw, 900px, ${CALC}\)`;
const AUTHORITY_RE = new RegExp(MIN_EXPR, 'g');

const authorityUses = (css.match(AUTHORITY_RE) ?? []).length;
check(
  'the exact same min(...) width expression backs reservation, drawer, and resizer',
  authorityUses >= 3,
  `authority expression appears ${authorityUses}× (need ≥3)`,
);
check('the old fixed min(440px, 94vw) drawer width is fully retired', !css.includes('min(440px, 94vw)'));
check(
  'no route-scoped chrome remains in the stylesheet (URL-only cutover)',
  !/route/i.test(css),
);
check(
  'the conversation guard subtracts 576px in every authority use',
  (css.match(new RegExp(CALC, 'g')) ?? []).length >= 3,
);
check(
  'side-by-side reservation pads .app-main only where drawer + conversation min fit (≥936px)',
  new RegExp(String.raw`@media \(min-width: 936px\)\s*\{\s*\.app-layout--files-open \.app-main,\s*\.app-layout--files-collapsed \.app-main\s*\{\s*padding-right:\s*${MIN_EXPR};`).test(css),
);

const drawerIdx = css.indexOf('.file-explorer {');
const drawerBody = drawerIdx >= 0 ? css.slice(drawerIdx, css.indexOf('}', drawerIdx)) : '';
check('drawer width reads the shared var', new RegExp(`width: ${MIN_EXPR}`).test(drawerBody));

// Line-start anchor: the scoped kill rule (`body:has(#subagents-panel)
// .file-explorer-resizer {`) must not shadow the standalone authority rule.
const resizerCssIdx = css.indexOf('\n.file-explorer-resizer {');
const resizerCss = resizerCssIdx >= 0 ? css.slice(resizerCssIdx, css.indexOf('}', resizerCssIdx)) : '';
check(
  'resizer is fixed at the drawer left edge via the authority expression',
  resizerCss.includes('position: fixed') && new RegExp(`right: ${MIN_EXPR}`).test(resizerCss),
);
check(
  'resizer advertises a column-resize cursor and cancels touch scrolling',
  resizerCss.includes('cursor: col-resize') && resizerCss.includes('touch-action: none'),
);

const collapseCapIdx = css.indexOf('.app-layout--files-collapsed .file-explorer-resizer');
const collapseCap = collapseCapIdx >= 0 ? css.slice(collapseCapIdx, css.indexOf('}', collapseCapIdx)) : '';
check('closed-mode cap keeps the resizer bounded (height rule present)', /height:\s*\d+px/.test(collapseCap));

const overlayMqIdx = css.lastIndexOf('@media (max-width: 935px)');
const overlayBlock = overlayMqIdx >= 0 ? css.slice(overlayMqIdx) : '';
check(
  'overlay (≤935px) hides the resizer entirely (full-content drawer, no desktop control)',
  overlayBlock.includes('.file-explorer-resizer') && /display:\s*none/.test(overlayBlock),
);
check(
  'overlay drawer (≤935px) spans the full viewport width',
  /\.file-explorer \{[^}]*left:\s*0;[^}]*width:\s*100vw/.test(overlayBlock),
);
check(
  'overlay (≤935px) drops the desktop .app-main reservation (padding-right 0)',
  /@media \(max-width: 935px\)[\s\S]*?\.app-main[^{}]*\{[^}]*padding-right:\s*0/.test(css),
);

const stackedMqIdx = css.lastIndexOf('@media (max-width: 720px)');
const stackedBlock = stackedMqIdx >= 0 ? css.slice(stackedMqIdx) : '';
check(
  'stacked drawer (≤720px) covers the full viewport width',
  /\.file-explorer \{[^}]*left:\s*0;[^}]*width:\s*100vw/.test(stackedBlock),
);
console.log(`\nfileExplorerResize.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);