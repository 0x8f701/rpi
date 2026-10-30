#!/usr/bin/env node
// Focused fork/new/clone lifecycle P0 regression (no browser, no socket):
//
//   active A -> Manage New explicit cwd B -> New succeeds -> immediate Fork
//
// Locks the frozen cross-node contract and the three frontend fixes:
//   1. Current-runtime Fork/Clone commands carry NO render-captured
//      sessionId; App.sendCommand routes the ownerless frame through the
//      SYNCHRONOUS displayState.commandTarget (B after the New cut) — the
//      stale A panel can never pin the fork to A. Saved-row fork/clone keep
//      sessionPath (SessionSidebar, unchanged).
//   2. The stale old panel performs NO post-lifecycle load; the keyed target
//      panel owns its mount load.
//   3. App validates the authoritative lifecycle target + `session` row
//      (parseLifecycleResult) and rejects ANY mismatch before mutating; the
//      validated row is the ONLY row injected into the sidebar overlay.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

import { routeCommandSession } from '../src/goal';
import {
  parseLifecycleResult,
  sessionRowKey,
  type SessionRowWire,
} from '../src/panels/SessionPanel';
import { mergeLifecycleOverlayRow, reconcileSessionRowOrder } from '../src/panels/SessionSidebar';

let assertions = 0;
function check(condition: unknown, message: string): void {
  assert.ok(condition, message);
  assertions += 1;
}

const here = dirname(fileURLToPath(import.meta.url));
const panelSource = readFileSync(join(here, '../src/panels/SessionPanel.tsx'), 'utf8');
const sidebarSource = readFileSync(join(here, '../src/panels/SessionSidebar.tsx'), 'utf8');
const appSource = readFileSync(join(here, '../src/App.tsx'), 'utf8');

/* ---------- 1. Routing: ownerless Fork/Clone follow commandTarget ---------- */

// Old A panel after New B: the fork command carries no explicit owner, so
// App.sendCommand's implicitTarget (displayState.commandTarget) decides — B,
// never the stale panel's render-captured A.
const ownerlessFork: Record<string, unknown> = { type: 'fork', entryId: 'e1' };
const ownerlessClone: Record<string, unknown> = { type: 'clone' };
const explicitFork: Record<string, unknown> = { type: 'fork', entryId: 'e1', sessionId: 'A' };
const newSessionFrame: Record<string, unknown> = { type: 'new_session', cwd: '/w/B' };
check(routeCommandSession(ownerlessFork, 'B') === 'B',
  'ownerless fork routes to the synchronous commandTarget B');
check(routeCommandSession(ownerlessClone, 'B') === 'B',
  'ownerless clone routes to the synchronous commandTarget B');
check(routeCommandSession({ ...ownerlessFork }, '') === '',
  'ownerless lifecycle command with no authority omits the frame owner (no field)');
check(routeCommandSession(explicitFork, 'B') === 'A',
  'an explicit owner still wins (saved-row frames route by their own owner)');
check(routeCommandSession(newSessionFrame, 'A') === 'A',
  'new_session routes to the source runtime that executes the create');

// The panel source no longer stamps the render-captured sessionId on
// Clone/Fork — the exact stale-A bug.
check(panelSource.includes("sendCommand({ type: 'clone' })"), 'Clone sends NO explicit owner');
check(!panelSource.includes("type: 'clone', sessionId"), 'Clone no longer carries the render-captured sessionId');
check(panelSource.includes("sendCommand({ type: 'fork', entryId: forkEntry })"), 'Fork sends NO explicit owner');
check(!/type: 'fork', entryId: forkEntry, sessionId/.test(panelSource),
  'Fork no longer carries the render-captured sessionId');
// Saved-row fork/clone keep sessionPath (unchanged contract).
check(sidebarSource.includes("{ type: 'session_fork_latest', sessionPath: row.path }"), 'saved-row fork-latest keeps sessionPath');
check(sidebarSource.includes("{ type: 'clone', sessionPath: row.path }"), 'saved-row clone keeps sessionPath');
check(sidebarSource.includes("{ type: 'fork', entryId, sessionPath: row.path }"), 'saved-row fork keeps sessionPath');

/* ---------- 2. Stale old-panel post-lifecycle load removed ---------- */

const runLifecycleBody = panelSource.slice(
  panelSource.indexOf('const runLifecycle = useCallback('),
  panelSource.indexOf('[busy, onLifecycleResult, reportFailure]'),
);
check(runLifecycleBody.length > 0, 'runLifecycle keeps its exact single-caller dep list');
// The invariant is the procedural path (before the JSX return): a lifecycle
// completion must not trigger a panel load. The user-facing session-refresh
// button inside the returned JSX is a manual load and stays allowed.
check(!runLifecycleBody.slice(0, runLifecycleBody.indexOf('return (')).includes('load('),
  'runLifecycle performs NO post-lifecycle panel load');
check(runLifecycleBody.includes('await onLifecycleResult(result)'), 'runLifecycle still consumes the result atomically');
check(panelSource.includes('const run = useCallback(') && /await load\(\{ waitForReady: false \}\)/.test(
  panelSource.slice(panelSource.indexOf('const run = useCallback('), panelSource.indexOf('const runLifecycle = useCallback(')),
), 'non-lifecycle run keeps its best-effort refresh (rename path unchanged)');

/* ---------- 3. Frozen contract parser: accept the exact shape ---------- */

const baseSession: Record<string, unknown> = {
  source: 'rpi',
  sessionId: 'child-B',
  runtimeIncarnation: 'child-B-inc',
  cwd: '/workspace/B',
  path: '/store/B.jsonl',
  status: 'native',
  loaded: true,
};
const lifecycleData = (overrides: Record<string, unknown> = {}, sessionOverrides: Record<string, unknown> = {}): Record<string, unknown> => ({
  sessionId: 'child-B',
  runtimeIncarnation: 'child-B-inc',
  state: { sessionId: 'child-B', cwd: '/workspace/B', sessionFile: '/store/B.jsonl' },
  messages: [],
  session: Object.keys(sessionOverrides).length > 0 ? { ...baseSession, ...sessionOverrides } : baseSession,
  ...overrides,
});

const parsed = parseLifecycleResult(lifecycleData());
check(parsed.sessionId === 'child-B' && parsed.runtimeIncarnation === 'child-B-inc',
  'valid contract payload parses to the exact target identity');
check(parsed.state.cwd === '/workspace/B', 'parser returns the authoritative state snapshot');
check(sessionRowKey(parsed.session) === 'rpi::child-B::/store/B.jsonl',
  'authoritative row keys as source::sessionId::path');
check(parsed.session.source === 'rpi' && parsed.session.status === 'native' && parsed.session.loaded === true,
  'authoritative row is a loaded native rpi row');
const named = parseLifecycleResult(lifecycleData({}, { name: 'Child', summary: 'sum', displayTime: 'now', messageCount: 3 }));
check(named.session.name === 'Child' && named.session.summary === 'sum' && named.session.displayTime === 'now'
  && named.session.messageCount === 3, 'bounded display fields pass through from the authoritative row');

/* ---------- 4. Contract mismatches reject before any mutation ---------- */

const mismatchCases: Array<[string, unknown]> = [
  ['missing sessionId', { runtimeIncarnation: 'inc', state: { sessionId: 'x', cwd: '/w', sessionFile: '/s' }, session: { source: 'rpi', sessionId: 'x', runtimeIncarnation: 'inc', cwd: '/w', path: '/s', status: 'native', loaded: true } }],
  ['empty sessionId', { ...lifecycleData(), sessionId: '' }],
  ['missing runtimeIncarnation', { ...lifecycleData(), runtimeIncarnation: '' }],
  ['missing state', { ...lifecycleData(), state: undefined }],
  ['state sessionId disagreement', { ...lifecycleData({ state: { sessionId: 'OTHER', cwd: '/workspace/B', sessionFile: '/store/B.jsonl' } }) }],
  ['state missing cwd', { ...lifecycleData({ state: { sessionId: 'child-B', sessionFile: '/store/B.jsonl' } }) }],
  ['state missing sessionFile', { ...lifecycleData({ state: { sessionId: 'child-B', cwd: '/workspace/B' } }) }],
  ['missing session row', { ...lifecycleData(), session: undefined }],
  ['session sessionId disagreement', lifecycleData({}, { sessionId: 'OTHER' })],
  ['session incarnation disagreement', lifecycleData({}, { runtimeIncarnation: 'OTHER' })],
  ['session cwd disagreement', lifecycleData({}, { cwd: '/workspace/OTHER' })],
  ['session path disagreement', lifecycleData({}, { path: '/store/OTHER.jsonl' })],
  ['foreign source', lifecycleData({}, { source: 'codex' })],
  ['non-native status', lifecycleData({}, { status: 'foreign' })],
  ['not loaded', lifecycleData({}, { loaded: false })],
  ['wrong-typed name', lifecycleData({}, { name: 42 })],
  ['wrong-typed messageCount', lifecycleData({}, { messageCount: 'many' })],
];
for (const [label, wire] of mismatchCases) {
  assert.throws(() => parseLifecycleResult(wire), `contract mismatch rejects: ${label}`);
}
check(parseLifecycleResult(lifecycleData()).sessionId === 'child-B', 'parser is reusable after rejections (pure, no state)');

// App must validate BEFORE mutating: parse precedes every cancellation,
// state write, activation, overlay injection, and revision bump.
const appLifecycleBody = appSource.slice(
  appSource.indexOf('const onLifecycleResult = useCallback('),
  appSource.indexOf('const settlePendingSwitch = useCallback('),
);
check(appLifecycleBody.includes('const target = parseLifecycleResult(result);'),
  'App.onLifecycleResult validates through parseLifecycleResult');
check(appLifecycleBody.indexOf('const target = parseLifecycleResult(result);')
  < appLifecycleBody.indexOf('cancelTextRendersForSession(previousSession)'),
  'validation precedes the first source-session mutation');
check(appLifecycleBody.indexOf('const target = parseLifecycleResult(result);')
  < appLifecycleBody.indexOf('resetComposerAttachments()'),
  'validation precedes composer reset');
check(appLifecycleBody.indexOf('setLifecycleRow(target.session);')
  < appLifecycleBody.indexOf('setCatalogRefreshRevision((revision) => revision + 1);'),
  'overlay row is injected BEFORE the catalog revision bump');
check(!appLifecycleBody.includes('return refreshState();'),
  'the legacy missing-sessionId refreshState fallback is gone (strict contract)');
check(appSource.includes('lifecycleRowOverlay={lifecycleRow}'), 'App passes the overlay row to the sidebar');
check(appSource.includes('onLifecycleRowAbsorbed={clearLifecycleRow}'), 'App owns the overlay clear callback');

/* ---------- 5. Sidebar merge: synchronous, pre-request, absorb-once ---------- */

check(sidebarSource.includes('mergeLifecycleOverlayRow(sessions, lifecycleRowOverlay)'),
  'rendered rows merge the overlay synchronously during render');
check(sidebarSource.indexOf('const mergedSessions = useMemo(') < sidebarSource.indexOf('loadRef.current().catch(() => {})'),
  'merge is defined before the first catalog request can run');
check(sidebarSource.includes('onLifecycleRowAbsorbedRef.current?.();'),
  'absorption clears the App overlay through the injected callback');
const absorptionEffect = sidebarSource.slice(
  sidebarSource.indexOf('Overlay absorption'),
  sidebarSource.indexOf('const requestCatalogLoad'),
);
check(absorptionEffect.includes('sessionsRef.current.some'), 'absorption tests the reconciled private catalog');
check(!/setSessions\(\(current\) => reconcileSessionRowOrder\(current, listResult\.sessions\)\)/.test(absorptionEffect),
  'the overlay never enters the private catalog state (separate channel)');

const soloCatalog = [
  { sessionId: 'A', path: '/store/A.jsonl', source: 'rpi', status: 'native', loaded: false } as const,
];
const overlayRow = {
  sessionId: 'child-B',
  cwd: '/workspace/B',
  path: '/store/B.jsonl',
  source: 'rpi',
  status: 'native',
  loaded: true,
  runtimeIncarnation: 'child-B-inc',
} as const;
const merged = mergeLifecycleOverlayRow([...soloCatalog], overlayRow);
check(merged.length === 2 && sessionRowKey(merged[1]!) === 'rpi::child-B::/store/B.jsonl',
  'overlay row appends to the rendered view immediately');

// Failed/delayed catalog: refreshSessionList preserves the known-good rows by
// reference, so the overlay (a separate channel) is retained, never merged
// into a stale catalog.
const absorbedCatalog = reconcileSessionRowOrder(
  [...soloCatalog],
  [...soloCatalog, overlayRow],
);
check(absorbedCatalog.length === 2 && sessionRowKey(absorbedCatalog[1]!) === 'rpi::child-B::/store/B.jsonl',
  'later catalog carries the exact overlay identity exactly once');
check(mergeLifecycleOverlayRow(absorbedCatalog, overlayRow).length === 2,
  'merged view has NO duplicate after absorption');
check(mergeLifecycleOverlayRow(absorbedCatalog, overlayRow)[1] === absorbedCatalog[1],
  'the catalog row (not the overlay) owns the row after absorption');
check(mergeLifecycleOverlayRow([...soloCatalog], overlayRow).length === 2
  && mergeLifecycleOverlayRow([...soloCatalog], { ...overlayRow, path: '/store/OTHER.jsonl' }).length === 2,
  'a catalog WITHOUT the exact identity keeps the overlay (delayed) — and different identities never absorb');

/* ---------- 6. Frame flow: A -> New B -> immediate Fork (child cwd/path B) ---- */

// App seam mirror: sendCommand routes the owner through the SYNCHRONOUS
// displayState.commandTarget; a validated lifecycle result cuts the authority
// synchronously (same statement block) before any later send.
class AppRoutingSeam {
  commandTarget = 'A';
  activeSid = 'A';
  overlay: SessionRowWire | null = null;
  readonly frames: Array<Record<string, unknown>> = [];

  send(command: Record<string, unknown>): void {
    const implicit = this.commandTarget || this.activeSid;
    const sid = routeCommandSession(command, implicit);
    this.frames.push(sid ? { ...command, sessionId: sid } : { ...command });
  }

  lifecycle(result: unknown): void {
    const target = parseLifecycleResult(result);
    this.commandTarget = target.sessionId;
    this.activeSid = target.sessionId;
    this.overlay = target.session;
  }
}

{
  const seam = new AppRoutingSeam();
  // Active A; the user opens Manage and New-explicitly selects cwd B.
  seam.send({ type: 'new_session', cwd: '/workspace/B' });
  check((seam.frames[0] as { sessionId?: string }).sessionId === 'A',
    'new_session routes to the source owner A (the runtime executing the create)');
  // Backend success: full snapshot whose target is B with cwd/path under B.
  seam.lifecycle(lifecycleData({}, {}));
  check(seam.commandTarget === 'child-B' && seam.activeSid === 'child-B' && seam.overlay !== null,
    'validated lifecycle cut moves the SYNCHRONOUS command target to the returned child');
  // The STALE A panel's immediate Fork click: command carries NO sessionId.
  const stalePanelFork: Record<string, unknown> = { type: 'fork', entryId: 'entry-1' };
  check(!('sessionId' in stalePanelFork), 'the fork command object omits the render-captured owner');
  seam.send(stalePanelFork);
  check((seam.frames[seam.frames.length - 1] as { sessionId?: string }).sessionId === 'child-B',
    'immediate Fork from the stale panel routes to the returned child, never A');
  // The backend forks FROM B: the child's authoritative state cwd/path are B's.
  const childState = { sessionId: 'fork-of-B', cwd: '/workspace/B', sessionFile: '/store/B-fork.jsonl' };
  const childRow = {
    source: 'rpi', sessionId: 'fork-of-B', runtimeIncarnation: 'fork-inc',
    cwd: '/workspace/B', path: '/store/B-fork.jsonl', status: 'native', loaded: true,
  };
  const forkResult = parseLifecycleResult({
    sessionId: 'fork-of-B', runtimeIncarnation: 'fork-inc',
    state: childState, session: childRow, messages: [],
  });
  check(forkResult.session.cwd === '/workspace/B' && forkResult.session.path === '/store/B-fork.jsonl',
    'fork child authoritative row carries cwd/path B');
  // Sidebar view: the overlay paints the child before any session_list.
  const view = mergeLifecycleOverlayRow([], forkResult.session);
  check(view.length === 1 && sessionRowKey(view[0]!) === 'rpi::fork-of-B::/store/B-fork.jsonl',
    'fork child paints from the overlay alone before any catalog response');
  // Contract mismatch on the fork result: rejection mutates nothing.
  const beforeTarget = seam.commandTarget;
  const beforeOverlay = seam.overlay;
  assert.throws(() => seam.lifecycle({
    sessionId: 'fork-of-B', runtimeIncarnation: 'fork-inc',
    state: { sessionId: 'OTHER', cwd: '/workspace/B', sessionFile: '/store/B-fork.jsonl' },
    session: childRow, messages: [],
  }), 'mismatched fork snapshot rejects');
  check(seam.commandTarget === beforeTarget && seam.overlay === beforeOverlay,
    'rejected fork snapshot mutates neither the routing target nor the overlay');
}

console.log(`sessionLifecycleFork.test: ${assertions} assertions passed`);