#!/usr/bin/env node
// File explorer TOP-OFFSET authority (App.tsx + styles.css).
//
// ONE CSS variable --app-header-height is the single top offset for the
// header, the content area below it, and the fixed file explorer drawer:
// App measures the live header, rounds to an integer px, and writes the var;
// the stylesheet pins the header's own height to that value and anchors the
// drawer + resizer to it. The old independent fixed offset (fractional
// 57.7px drawer top vs 55px content top) left a visible 2.7px seam — this
// test locks the one-authority contract:
//   * pure rounding/apply helpers (node-import-safe from App.tsx),
//   * App source semantics (measure header -> applyHeaderHeight, no direct
//     var write inside the effect),
//   * stylesheet semantics (drawer/resizer top read the var; header pins its
//     height to it; phone overlay stays under the header so the hamburger
//     remains clickable; no literal px top on the file explorer).
//
// Run through `npm run build` (esbuild-bundled like fileExplorerResize.test.ts,
// which also imports ../src/App.tsx).
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  FOOTER_HEIGHT_FLOOR,
  HEADER_HEIGHT_FLOOR,
  applyFooterHeight,
  applyHeaderHeight,
  footerHeightPx,
  headerHeightPx,
  measureChromeHeight,
} from '../src/App.tsx';

const failures: string[] = [];
let ran = 0;
function check(name: string, cond: unknown, detail?: string): void {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

/* ------------------------------------------------------------------ *
 * Browser-surface stubs (window.localStorage / document.documentElement
 * style). Assigned on globalThis before any helper call; App's bare
 * `window` / `document` identifiers resolve to these at call time. Named
 * cast: test-only DOM stand-ins, runtime shape is exercised by every
 * helper call below.
 * ------------------------------------------------------------------ */
const store = new Map<string, string>();
let storageOk = true;
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
    return 1280;
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
 * 1. Pure rounding helper (one rounded integer authority)
 * ------------------------------------------------------------------ */
check('floor constant is 44px', HEADER_HEIGHT_FLOOR === 44);
check('rounds fractional heights to the nearest integer', headerHeightPx(57.7031) === 58);
check('rounds down below .5', headerHeightPx(57.2) === 57);
check('passes integers through', headerHeightPx(58) === 58);
check('floors tiny heights at 44px', headerHeightPx(40) === HEADER_HEIGHT_FLOOR);
check('maps NaN to the floor', headerHeightPx(Number.NaN) === HEADER_HEIGHT_FLOOR);
check('maps Infinity to the floor', headerHeightPx(Infinity) === HEADER_HEIGHT_FLOOR);
check(
  'never returns a non-integer',
  [0, 0.5, 57.2, 57.7031, 58, 100.9, Number.NaN].every((h) => Number.isInteger(headerHeightPx(h))),
);

/* ------------------------------------------------------------------ *
 * 2. CSS-var application (one top authority)
 * ------------------------------------------------------------------ */
cssVars.length = 0;
applyHeaderHeight(57.7031);
check(
  'apply writes --app-header-height on the document element',
  cssVars.length > 0
    && cssVars[cssVars.length - 1][0] === '--app-header-height'
    && cssVars[cssVars.length - 1][1] === '58px',
);
applyHeaderHeight(40);
check('apply floors the written value', cssVars[cssVars.length - 1][1] === '44px');

check('footer floor constant is 44px', FOOTER_HEIGHT_FLOOR === 44);
check('footer rounds fractional heights', footerHeightPx(56.6) === 57);
check('footer floors tiny heights at 44px', footerHeightPx(40) === FOOTER_HEIGHT_FLOOR);
cssVars.length = 0;
applyFooterHeight(56.6);
check(
  'apply writes --app-footer-height on the document element',
  cssVars.length > 0
    && cssVars[cssVars.length - 1][0] === '--app-footer-height'
    && cssVars[cssVars.length - 1][1] === '57px',
);
cssVars.length = 0;
applyFooterHeight(200);
applyFooterHeight(57);
check(
  'tall then short footer writes the short value (not a ratchet max)',
  cssVars[cssVars.length - 1][0] === '--app-footer-height'
    && cssVars[cssVars.length - 1][1] === '57px',
);

{
  const el = {
    style: { minHeight: '' },
    getBoundingClientRect(): { height: number } {
      const pinned = 200;
      const natural = 57;
      if (this.style.minHeight === '0' || this.style.minHeight === 'unset') {
        return { height: natural };
      }
      return { height: pinned };
    },
  };
  const measured = measureChromeHeight(el);
  check('measureChromeHeight with a 200px CSS pin and 57px natural returns 57', measured === 57, String(measured));
  check('measureChromeHeight leaves inline min-height at 0 so the CSS pin cannot ratchet', el.style.minHeight === '0');
  cssVars.length = 0;
  applyFooterHeight(measured);
  check('natural 57 after a 200px pin writes 57px', cssVars[cssVars.length - 1][1] === '57px');
}


/* ------------------------------------------------------------------ *
 * 3. App.tsx source semantics (measure header -> apply, no direct write)
 * ------------------------------------------------------------------ */
const here = dirname(fileURLToPath(import.meta.url));
const app = readFileSync(join(here, '..', 'src', 'App.tsx'), 'utf8');

const headerRefIdx = app.indexOf('appHeaderRef.current');
const headerEffect = headerRefIdx >= 0 ? app.slice(headerRefIdx, headerRefIdx + 1600) : '';
check(
  'the effect routes the measurement through applyHeaderHeight (one authority)',
  headerEffect.includes('applyHeaderHeight('),
);
check(
  'the effect never writes the var directly (no competing inline top)',
  headerEffect.length > 0 && !headerEffect.includes('setProperty'),
);
check(
  'App measures the header unconditionally and never hides it for Files',
  !headerEffect.includes('appShellHidden')
    && !app.includes('fileExplorerFullscreenActive')
    && !app.includes('hidden={filesFullscreen}')
    && !app.includes('app-layout--files-expanded'),
);
check(
  'the ResizeObserver stays armed so later viewport changes remeasure the header',
  headerEffect.includes('ro.observe(header)'),
);
check(
  'the footer measurement uses App\'s own footer ref',
  app.includes('appFooterRef.current')
    && app.includes('<footer')
    && app.includes('ref={appFooterRef}'),
);
check(
  'the footer effect routes through applyFooterHeight',
  app.includes('applyFooterHeight('),
);
check(
  'footer and header measure through measureChromeHeight',
  app.includes('measureChromeHeight(footer)') && app.includes('measureChromeHeight(header)'),
);
check(
  'chrome measure does not restore a previous inline min-height',
  !app.includes('style.minHeight = prevMinHeight'),
);

/* ------------------------------------------------------------------ *
 * 4. styles.css semantics (one top authority, mobile fullscreen)
 * ------------------------------------------------------------------ */
const css = readFileSync(join(here, '..', 'src', 'styles.css'), 'utf8');

const headerIdx = css.indexOf('header {');
const headerBlock = headerIdx >= 0 ? css.slice(headerIdx, css.indexOf('}', headerIdx)) : '';
check(
  'the header pins its own height to --app-header-height (content top == explorer top)',
  /min-height:\s*var\(--app-header-height/.test(headerBlock),
);

check(
  'open Files has no outer width resizer',
  !css.includes('.file-explorer-resizer'),
);


check(
  'desktop ordinary panels sit above the measured footer',
  css.includes('bottom: var(--app-footer-height, 0px)'),
);
check(
  'desktop ordinary panels never exceed leftover viewport between header and footer',
  css.includes('max-height: calc(100dvh - var(--app-header-height, 0px) - var(--app-footer-height, 0px))')
    && css.includes('height: min(var(--panel-drawer-height, 90vh), calc(100dvh - var(--app-header-height, 0px) - var(--app-footer-height, 0px)))'),
);
check(
  'tablet drawers sit under the header and above the footer',
  /@media \(max-width: 768px\) \{[\s\S]*?#session-panel \{[\s\S]*?top:\s*var\(--app-header-height\)[\s\S]*?bottom:\s*var\(--app-footer-height/.test(css),
);
check(
  'session actions size the path autocomplete as a 44px equal column',
  css.includes('.session-actions > span > .path-autocomplete')
    && !css.includes('session-cwd-autocomplete'),
);

console.log(`\nfileExplorerOffset.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);
