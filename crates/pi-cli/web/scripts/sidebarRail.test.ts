#!/usr/bin/env node
// Collapsed-rail hit-target contract for src/styles.css.
//
// The desktop collapsed rail applies `pointer-events: none` to every
// `.session-sidebar` descendant except `#rail-reopen-btn`. That exception is
// not enough: the button lives under `.session-sidebar__head`, which still
// matches the blanket rule, so the visible » is not hit-testable. This file
// parses the production stylesheet (no browser) and asserts the ancestor
// chain is re-enabled. Run through `npm run build`.
//
// Pre-fix CSS fails here because the stylesheet only excludes
// `#rail-reopen-btn` from the none rule and never sets pointer-events:auto
// on `.session-sidebar__head`.
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const failures: string[] = [];
let ran = 0;
function check(name: string, cond: unknown, detail?: string): void {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

const css = readFileSync(
  join(dirname(fileURLToPath(import.meta.url)), '..', 'src', 'styles.css'),
  'utf8',
);

check('desktop collapsed-rail marker exists', css.includes('#rail-reopen-btn { display: none; }'));
check(
  'blanket none still inerts hidden rail descendants',
  css.includes('.session-sidebar *:not(#rail-reopen-btn):not(.session-sidebar__head)')
    && /pointer-events:\s*none;/.test(css),
  'missing the :not(.session-sidebar__head) carve-out',
);
check(
  'head ancestor AND #rail-reopen-btn re-enable pointer-events',
  /\.session-sidebar__head,\s*\n\s*\.app-layout:not\(\.app-layout--drawer-open\) #rail-reopen-btn \{\s*\n\s*pointer-events: auto;/.test(css),
  'pre-fix CSS never set pointer-events:auto on .session-sidebar__head, so the visible child stayed unclickable',
);
check(
  'reopen button is displayed in the collapsed desktop rail',
  /\.app-layout:not\(\.app-layout--drawer-open\) #rail-reopen-btn \{\s*\n\s*display: inline-block;/.test(css),
);

// The collapsed » lives inside .session-sidebar__head. That header once
// declared `border-bottom: 1px solid var(--border)`, which the collapsed rail
// kept painting as a stray divider line across the top of the 44px strip
// right under the » (the aside base never had one). The owner of the rule is
// the head, so the contract is: no .session-sidebar__head rule anywhere in
// the cascade may declare a border-bottom, and no collapsed-rail override may
// re-add a border to the » ancestor chain.
const headRuleBlocks = css.match(/\.session-sidebar__head[^{}]*\{[^{}]*\}/g) ?? [];
check(
  'no .session-sidebar__head rule declares a border-bottom (» divider owner removed)',
  headRuleBlocks.length !== 0 && !headRuleBlocks.join('\n').includes('border-bottom'),
  'a border-bottom on the head paints a stray divider under the collapsed-rail »',
);
const collapsedHeadRules =
  css.match(/\.app-layout:not\(\.app-layout--drawer-open\)[^{}]*\.session-sidebar__head[^{}]*\{[^{}]*\}/g) ?? [];
check(
  'collapsed-rail overrides never re-add borders to the » ancestor chain',
  !/\bborder[a-z-]*\s*:/.test(collapsedHeadRules.join('\n')),
);

const mobileStart = css.indexOf('@media (max-width: 720px) {');
check('narrow mobile media block exists', mobileStart >= 0);
const mobileBlock = mobileStart >= 0 ? css.slice(mobileStart, mobileStart + 4000) : '';
check(
  'mobile block does not re-enable collapsed-rail pointer-events',
  !/pointer-events:\s*auto/.test(mobileBlock),
  'desktop-only hit-target override must not leak into ≤720px',
);

// ---------------------------------------------------------------------------
// Responsive drawer-state contract (src/App.tsx). ONE visibility state drives
// the desktop rail and the mobile drawer, so the breakpoint must own the
// default on every crossing: desktop→mobile collapses an inherited-open rail
// into the closed mobile drawer (the first hamburger press then OPENS), and
// mobile→desktop never force-opens (sidebarOpen=false is the supported
// collapsed-rail state; forcing it would clobber the user's collapse choice).
// The listener is dedicated and cleaned up, and the mobile pick closes the
// drawer through App's onSwitchComplete.
const appSource = readFileSync(
  join(dirname(fileURLToPath(import.meta.url)), '..', 'src', 'App.tsx'),
  'utf8',
);

check(
  'boot default: sidebar open on desktop, closed on mobile',
  appSource.includes("window.matchMedia('(min-width: 721px)').matches"),
  'sidebarOpen initializer must read the ≥721px breakpoint',
);
const collapseListener = appSource.match(
  /const mq = window\.matchMedia\('\(max-width: 720px\)'\);\s*const collapse = \(event: MediaQueryListEvent\) => \{\s*if \(event\.matches\) setSidebarOpen\(false\);\s*\};\s*mq\.addEventListener\('change', collapse\);\s*return \(\) => mq\.removeEventListener\('change', collapse\);/,
);
check(
  'desktop→mobile crossing collapses the inherited-open rail via a dedicated mq change listener',
  collapseListener !== null,
  'missing the (max-width: 720px) change → setSidebarOpen(false) crossing sync',
);
check(
  'mq change listener is removed on cleanup (no leak)',
  collapseListener !== null && collapseListener[0].includes("mq.removeEventListener('change', collapse)"),
);
check(
  'the only force-open path is the collapsed-rail » reopen button',
  (appSource.match(/setSidebarOpen\(true\)/g) ?? []).length === 1,
  'mobile→desktop must never force-open the rail; sidebarOpen=false is the supported deliberate state',
);
check(
  'mobile pick closes the drawer through onSwitchComplete',
  /onSwitchComplete=\{\(\) => \{\s*\/\/ Mobile: closing the drawer after a pick; desktop keeps the rail\.\s*if \(window\.matchMedia\('\(max-width: 720px\)'\)\.matches\) setSidebarOpen\(false\);\s*\}\}/.test(appSource),
);

console.log(`\nsidebarRail.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);