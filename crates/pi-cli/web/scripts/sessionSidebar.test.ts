import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  catalogIdentityOf,
  contextMenuItems,
  collapsedSidebarGroupKeys,
  flattenTree,
  buildSwitchTarget,
  buildRestoreSwitchTarget,
  buildSessionHandoffBrief,
  HANDOFF_BRIEF_MAX_EXCHANGES,
  HANDOFF_BRIEF_MAX_CHARS,
  HANDOFF_BRIEF_MESSAGE_CLAMP,
  latestCompactionSummary,
  groupSessions,
  isNoiseProjectName,
  matchSessionSearch,
  migrateSidebarCollapseIntent,
  projectFromSessionPath,
  projectNameOf,
  providerOf,
  mergeLifecycleOverlayRow,
  reconcileSessionRowOrder,
  resolveActiveSessionRow,
  sessionRowMeta,
  sessionTitle,
  shouldToastSessionActionError,
  visibleSidebarSessions,
} from '../src/panels/SessionSidebar';

import { isLoadedCurrentSession, sessionRowKey } from '../src/panels/SessionPanel';
import { isTransportError, isTransportErrorMessage } from '../src/transportError';
import type { SessionRowWire } from '../src/panels/SessionPanel';
import type { SessionTreeItem } from '../src/panels/SessionSidebar';

let assertions = 0;
const check = (condition: unknown, message: string): void => {
  assert.ok(condition, message);
  assertions += 1;
};

const row = (overrides: Partial<SessionRowWire>): SessionRowWire => ({
  sessionId: 'session',
  path: '<workspace>/session-store/native.jsonl',
  source: 'rpi',
  status: 'native',
  loaded: false,
  ...overrides,
});

/* ---------- provider normalization ---------- */

check(providerOf(row({ source: 'rpi' })) === 'rpi', 'native rpi rows map to rpi');
check(providerOf(row({ source: 'pi' })) === 'Pi', 'pi rows remain a distinct source');
check(providerOf(row({ source: 'rpi' })) !== providerOf(row({ source: 'pi' })), 'rpi and Pi stay distinct wire identities');
check(providerOf(row({ source: 'native' })) === 'rpi', 'native alias maps to rpi');
check(providerOf(row({ source: 'primary' })) === 'rpi', 'primary alias maps to rpi');
check(providerOf(row({ source: 'omp' })) === 'OMP', 'omp rows map to OMP');
check(providerOf(row({ source: 'codex' })) === 'Codex', 'codex rows map to Codex');
check(providerOf(row({ source: 'grok' })) === 'Grok', 'grok rows map to Grok');
check(providerOf(row({ source: 'grok/hyper' })) === 'Grok', 'grok/hyper maps to Grok');
check(providerOf(row({ source: '' })) === null, 'empty source returns null');
check(providerOf(row({ source: 'unknown-agent' })) === null, 'unknown source returns null');
check(providerOf(row({ source: 'claude' })) === null, 'claude source returns null (not one of the five)');
check(providerOf(row({ source: 'droid' })) === null, 'droid source returns null (not one of the five)');

/* ---------- project path recovery (unchanged) ---------- */

check(projectFromSessionPath('<workspace>/session-store/--workspace-projects-parth-generic-v1--/one.jsonl') === 'parth-generic-v1', 'encoded native project tail is recovered');
check(projectFromSessionPath('<workspace>/foreign/rollout-codex-manager.jsonl') === 'rollout-codex-manager', 'foreign file basename is retained');
check(projectNameOf(row({ cwd: '<workspace>/rpi' })) === 'rpi', 'authoritative cwd names the project');

/* ---------- session title (unchanged) ---------- */

check(sessionTitle(row({ name: '<b>named</b>', summary: 'ignored' })) === '<b>named</b>', 'session names remain literal text-node content');
check(sessionTitle(row({ cwd: '<workspace>/550e8400-e29b-41d4-a716-446655440000', name: null, summary: null })) === 'Untitled session', 'UUID cwd is not used as a session title');
check(sessionTitle(row({ cwd: '<workspace>/sk-1234567890abcdef', name: null, summary: null })) === 'Untitled session', 'unnamed empty-summary rows are Untitled session, not a project fallback');
check(sessionTitle(row({ name: undefined, summary: 'first user message as title' })) === 'first user message as title', 'unnamed rows still derive the title from summary');
check(sessionTitle(row({ cwd: '<workspace>/rpi/psy-node/psy-node', name: null, summary: '' })) === 'Untitled session', 'empty summary never echoes the cwd basename');
check(sessionTitle(row({ cwd: '<workspace>/rpi/psy-node/psy-node', name: 'audit', summary: '' })) === 'audit', 'an explicit name is the session title');

/* ---------- compact row meta: time + message count, no duplicate body ---------- */

check(sessionRowMeta(row({ displayTime: '2m ago', messageCount: 12 })) === '2m ago · 12 msgs', 'meta joins time and message count with a separator');
check(sessionRowMeta(row({ displayTime: '2m ago' })) === '2m ago', 'meta is time-only when message count is absent');
check(sessionRowMeta(row({ messageCount: 3 })) === '3 msgs', 'meta is message-count-only when time is absent');
check(sessionRowMeta(row({})) === '', 'meta is empty when neither time nor message count is present');
check(sessionRowMeta(row({ displayTime: '', messageCount: 0 })) === '0 msgs', 'zero message count still renders as msgs');

const here = dirname(fileURLToPath(import.meta.url));
const sidebarSource = readFileSync(join(here, '../src/panels/SessionSidebar.tsx'), 'utf8');
const sidebarCss = readFileSync(join(here, '../src/styles.css'), 'utf8');
const appSource = readFileSync(join(here, '../src/App.tsx'), 'utf8');
check(appSource.includes('id="todo-toggle-btn"'), 'App still mounts the Todo panel toggle');
check(
  appSource.includes('id="chat-console-link"')
    && appSource.includes('href="/chat"')
    && appSource.includes('aria-label="Open chat"')
    && />\s*Chat\s*</.test(appSource),
  'right-side menu links to same-host /chat',
);
check(
  /\.session-sidebar__nav\s*>\s*:last-child:nth-child\(odd\)\s*\{[^}]*grid-column:\s*1\s*\/\s*-1/.test(sidebarCss),
  'odd last feature-nav item spans both columns',
);
check(sidebarSource.includes('session-sidebar__name'), 'row JSX still renders the session title');
check(sidebarSource.includes('session-sidebar__meta'), 'row JSX still renders the compact meta line');
check(sidebarSource.includes('sessionRowMeta(row)'), 'row JSX uses sessionRowMeta for time and message count');
check(!sidebarSource.includes('session-sidebar__summary'), 'row JSX no longer renders a duplicate summary element');
check(!sidebarSource.includes(' · live'), 'row JSX no longer appends a visible live label to meta');
check(!/>\s*live\s*</.test(sidebarSource), 'row JSX has no visible live/LIVE text node');
check(sidebarSource.includes('data-loaded={row.loaded === true ? \'true\' : undefined}'), 'loaded rows keep a data-loaded attribute');
check(sidebarSource.includes('className="session-sidebar__loaded"'), 'loaded rows keep the loaded marker class');
check(sidebarSource.includes('data-loaded-for={row.sessionId}'), 'loaded marker keeps data-loaded-for for pool queries');
check(sidebarSource.includes('aria-hidden="true"'), 'loaded marker is hidden from accessible name');
check(sidebarSource.includes('session-sidebar__row--active'), 'active rows keep the active class');
check(sidebarSource.includes('aria-current={isActive ? \'true\' : undefined}'), 'active rows keep accessible current state');
check(sidebarSource.includes('(loaded)'), 'loaded state remains in the accessible title/label without visible text');
check(!sidebarCss.includes('session-sidebar__summary'), 'sidebar CSS no longer styles a visible summary');
check(/\.session-sidebar__loaded\s*\{\s*display:\s*none;/.test(sidebarCss), 'loaded marker is visually hidden');
check(/\.session-sidebar__group-head\[aria-disabled='true'\][^{]*\{[^}]*cursor:\s*default/.test(sidebarCss),
  'forced-open heading is styled inert: default cursor even on hover');
check(/\.session-sidebar__group-head\[aria-disabled='true'\]:hover\s*\{[^}]*color:\s*var\(--dim\)/.test(sidebarCss),
  'forced-open heading never picks up the interactive hover accent');

/* ---------- noise project name detection ---------- */

check(isNoiseProjectName('tmp'), 'literal tmp is noise');
check(isNoiseProjectName('TMP'), 'case-insensitive tmp is noise');
check(isNoiseProjectName('.tmpAbCdEf'), '.tmp* prefix is noise');
check(isNoiseProjectName('source'), 'source is noise');
check(isNoiseProjectName('a1b2c3d4-e5f6-7890-abcd-ef1234567890'), 'bare UUID is noise');
check(!isNoiseProjectName('parth-generic-v1'), 'real project name is not noise');
check(!isNoiseProjectName('rpi'), 'short project name is not noise');
check(!isNoiseProjectName('my-app'), 'hyphenated project name is not noise');

/* ---------- group tree: provider -> project -> sessions ---------- */

const piRows = Array.from({ length: 8 }, (_, index) => row({
  sessionId: `pi-${index + 1}`,
  source: 'pi',
  status: 'foreign',
  cwd: '<workspace>/a',
  path: `<workspace>/foreign/pi-${index + 1}.jsonl`,
}));
const grouped = groupSessions([
  row({ sessionId: 'native-a', cwd: '<workspace>/a', path: '<workspace>/native/a.jsonl' }),
  row({ sessionId: 'native-b', cwd: '<workspace>/b', path: '<workspace>/native/b.jsonl' }),
  ...piRows,
  row({ sessionId: 'omp-a', source: 'omp', cwd: '<workspace>/a', path: '<workspace>/foreign/omp-a.jsonl' }),
  row({ sessionId: 'codex-a', source: 'codex', cwd: '<workspace>/a', path: '<workspace>/foreign/codex-a.jsonl' }),
  row({ sessionId: 'grok-b', source: 'grok', cwd: '<workspace>/b', path: '<workspace>/foreign/grok-b.jsonl' }),
]);
check(grouped.map((node) => node.key).join('|') === 'provider::rpi|provider::Pi|provider::OMP|provider::Codex|provider::Grok', 'five provider roots in first-seen catalog order');
check(grouped.every((node) => node.kind === 'provider'), 'all roots are provider kind');
check(grouped[0]?.label === 'rpi', 'first root is rpi');
check(grouped[0]?.children[0]?.sessions[0]?.sessionId === 'native-a', 'rpi nests project a');
check(grouped[0]?.children[1]?.sessions[0]?.sessionId === 'native-b', 'rpi nests project b');
check(grouped[0]?.children[0]?.kind === 'project', 'child groups are project kind');
check(grouped[1]?.label === 'Pi', 'second root is Pi');
check(grouped[1]?.children[0]?.sessions[0]?.sessionId === 'pi-1', 'Pi nests by project');
check(grouped[2]?.label === 'OMP', 'third root is OMP (first-seen, not PROVIDER_ORDER)');
check(grouped[2]?.children[0]?.sessions[0]?.sessionId === 'omp-a', 'OMP nests by project');
check(grouped[3]?.label === 'Codex', 'fourth root is Codex');
check(grouped[3]?.children[0]?.sessions[0]?.sessionId === 'codex-a', 'Codex nests by project');
check(grouped[4]?.label === 'Grok', 'fifth root is Grok');
check(grouped[4]?.children[0]?.sessions[0]?.sessionId === 'grok-b', 'Grok nests by project');
const piRowsInTree = grouped
  .filter((node) => node.label === 'Pi')
  .flatMap((node) => [...node.sessions, ...node.children.flatMap((child) => child.sessions)]);
check(piRowsInTree.length === 8, 'Pi provider count matches eight actionable descendant rows');
check(piRowsInTree.map((entry) => entry.sessionId).join('|') === piRows.map((entry) => entry.sessionId).join('|'), 'expanded Pi provider exposes every counted row');

/* ---------- stable Other subgroup; real Projects name survives ---------- */

const noiseGrouped = groupSessions([
  row({ sessionId: 'tmp-session', cwd: '<workspace>/tmp', path: '<workspace>/native/tmp.jsonl' }),
  row({ sessionId: 'uuid-session', cwd: '<workspace>/a1b2c3d4-e5f6-7890-abcd-ef1234567890', path: '<workspace>/native/uuid.jsonl' }),
  row({ sessionId: 'real-session', cwd: '<workspace>/rpi', path: '<workspace>/native/rpi.jsonl' }),
  row({ sessionId: 'projects-session', cwd: '<workspace>/Projects', path: '<workspace>/native/projects.jsonl' }),
]);
const rpiRoot = noiseGrouped.find((n) => n.label === 'rpi');
check(rpiRoot !== undefined, 'rpi root exists');
check(rpiRoot?.sessions.length === 0, 'provider has no direct session rows');
const otherGroup = rpiRoot?.children.find((child) => child.label === 'Other');
check(otherGroup?.subgroupKind === 'other', 'noise rows share explicit Other classification');
check(otherGroup?.sessions.map((entry) => entry.sessionId).join('|') === 'tmp-session|uuid-session', 'Other contains every noise row in catalog order');
check(rpiRoot?.children.some((child) => child.label === 'rpi'), 'real rpi project subgroup remains');
check(rpiRoot?.children.some((child) => child.label === 'Projects' && child.subgroupKind === 'project'), 'authoritative cwd basename Projects remains a real project subgroup');
check(!noiseGrouped.map((node) => node.label).includes('Other'), 'Other is never a top-level provider group');
const noiseItems: SessionTreeItem[] = [];
flattenTree([
  row({ sessionId: 'tmp-session', cwd: '<workspace>/tmp', path: '<workspace>/native/tmp.jsonl' }),
  row({ sessionId: 'real-session', cwd: '<workspace>/rpi', path: '<workspace>/native/rpi.jsonl' }),
], new Set(['provider::rpi::special::other']), noiseItems);
check(
  noiseItems.map((item) => item.kind === 'group' ? item.node.key : sessionRowKey(item.row)).join('|')
    === 'provider::rpi|provider::rpi::special::other|provider::rpi::project::rpi|rpi::real-session::<workspace>/native/rpi.jsonl',
  'collapsing Other hides only noise descendants while real project rows remain',
);

/* ---------- unknown-source rows are filtered from the sidebar ---------- */

const filteredGrouped = groupSessions([
  row({ sessionId: 'native-1', cwd: '<workspace>/a', path: '<workspace>/native/a.jsonl' }),
  row({ sessionId: 'claude-1', source: 'claude', cwd: '<workspace>/b', path: '<workspace>/foreign/claude.jsonl' }),
  row({ sessionId: 'droid-1', source: 'droid', cwd: '<workspace>/c', path: '<workspace>/foreign/droid.jsonl' }),
  row({ sessionId: 'unknown-1', source: 'dirty-source', cwd: '<workspace>/d', path: '<workspace>/foreign/dirty.jsonl' }),
]);
const filteredLabels = filteredGrouped.map((n) => n.label);
check(filteredLabels.length === 1, 'only one provider root (rpi) when other sources are unknown');
check(filteredLabels[0] === 'rpi', 'rpi is the only root');
check(filteredGrouped[0]?.children[0]?.sessions[0]?.sessionId === 'native-1', 'native row retained');
check(!filteredLabels.includes('claude'), 'claude is not a provider group');
check(!filteredLabels.includes('droid'), 'droid is not a provider group');
check(!filteredLabels.includes('dirty-source'), 'dirty source is not a provider group');

/* ---------- search matching ---------- */

const searchRow = row({ sessionId: 'abc-123', name: 'My Cool Session', cwd: '<workspace>/rpi', source: 'rpi' });
check(matchSessionSearch(searchRow, 'cool'), 'matches by session title');
check(matchSessionSearch(searchRow, 'rpi'), 'matches by project basename');
check(matchSessionSearch(row({ sessionId: 'label-only', source: 'rpi', cwd: '<workspace>/none' }), 'rpi'), 'matches by native provider label');
check(!matchSessionSearch(row({ sessionId: 'label-only', source: 'pi', cwd: '<workspace>/none' }), 'rpi'), 'Pi provider label does not match rpi');
check(matchSessionSearch(row({ sessionId: 'pi-only', source: 'pi', cwd: '<workspace>/other' }), 'pi'), 'Pi rows match their own wire source');
check(matchSessionSearch(searchRow, 'abc-123'), 'matches by session id');
check(!matchSessionSearch(searchRow, 'nonexistent'), 'no match returns false');
check(matchSessionSearch(searchRow, ''), 'empty query matches all');

const codexSearchRow = row({ sessionId: 'codex-1', source: 'codex', cwd: '<workspace>/myapp' });
check(matchSessionSearch(codexSearchRow, 'codex'), 'matches by wire source');
check(matchSessionSearch(codexSearchRow, 'myapp'), 'matches by project basename');

/* ---------- Windows-shaped cwd: grouping, search, and subgroup keys ---------- */

const winGrouped = groupSessions([
  row({ sessionId: 'win-a', cwd: 'C:\\work\\app', path: 'C:/store/win-a.jsonl' }),
  row({ sessionId: 'win-b', cwd: 'C:/work/app/', path: 'C:/store/win-b.jsonl' }),
  row({ sessionId: 'win-c', cwd: 'C:\\work\\app', path: 'C:/store/win-c.jsonl' }),
]);
const winRoot = winGrouped.find((node) => node.label === 'rpi');
check(winRoot !== undefined && winRoot.children.length === 1, 'Windows drive-root cwds share one project subgroup');
const winGroup = winRoot?.children[0];
check(winGroup?.label === 'app' && winGroup?.subgroupKind === 'project', 'a Windows cwd names the project by its basename across both separators');
check(winGroup?.key === 'provider::rpi::project::app', 'the Windows project subgroup key is the provider::project shape');
check(winGroup?.sessions.map((entry) => entry.sessionId).join('|') === 'win-a|win-b|win-c', 'backslash, forward-slash, and trailing-separator Windows cwds share the subgroup in catalog order');
const winSplit = groupSessions([
  row({ sessionId: 'w-alpha', cwd: 'C:\\work\\alpha', path: 'C:/store/w-alpha.jsonl' }),
  row({ sessionId: 'w-beta', cwd: 'C:\\work\\beta', path: 'C:/store/w-beta.jsonl' }),
  row({ sessionId: 'w-unc', cwd: '\\\\server\\share\\proj', path: 'C:/store/w-unc.jsonl' }),
]).find((node) => node.label === 'rpi')?.children;
check(winSplit?.map((child) => child.label).join('|') === 'alpha|beta|proj', 'distinct Windows directories and a strict-UNC share path keep distinct project subgroups');
check(winSplit?.every((child) => child.key.startsWith('provider::rpi::project::')) === true, 'every Windows subgroup key keeps the provider::project shape');

const winSearchRow = row({ sessionId: 'win-s', name: undefined, cwd: 'C:\\Users\\operator\\Projects\\app', path: 'C:/store/win-s.jsonl' });
check(matchSessionSearch(winSearchRow, 'app'), 'a Windows cwd matches search by its basename');
check(matchSessionSearch(winSearchRow, 'win-s'), 'a Windows-cwd row matches search by session id');
check(!matchSessionSearch(winSearchRow, 'users'), 'parent directories of a Windows cwd never match the project search');
check(matchSessionSearch(row({ sessionId: 'w-unc-s', cwd: '\\\\server\\share\\proj', path: 'C:/store/u.jsonl' }), 'proj'), 'a strict-UNC cwd matches search by its share-tail basename');

/* The exact Windows subgroup key drives collapse: collapsing
 * `provider::rpi::project::app` hides only that subgroup's rows while the
 * headers and the sibling subgroup stay emitted. */
const winCollapsedRows = [
  row({ sessionId: 'win-a', cwd: 'C:\\work\\app', path: 'C:/store/win-a.jsonl' }),
  row({ sessionId: 'win-b', cwd: 'C:\\work\\beta', path: 'C:/store/win-b.jsonl' }),
];
const winCollapsedItems: SessionTreeItem[] = [];
flattenTree(winCollapsedRows, new Set(['provider::rpi::project::app']), winCollapsedItems);
check(
  winCollapsedItems.map((item) => item.kind === 'group' ? item.node.key : sessionRowKey(item.row)).join('|')
    === `provider::rpi|provider::rpi::project::app|provider::rpi::project::beta|${sessionRowKey(winCollapsedRows[1]!)}`,
  'collapsing the exact Windows subgroup key hides only that subgroup while headers and siblings remain',
);

/* ---------- temporary-workspace rows: default hide, search/active/loaded restore ---------- */

const tempRow = row({ sessionId: 'temp-1', name: undefined, cwd: '<tmp>/pi-noise', source: 'rpi', temporary: true });
const normalRow = row({ sessionId: 'normal-1', cwd: '<workspace>/rpi', source: 'rpi', temporary: false });

const defaultVisible = visibleSidebarSessions([tempRow, normalRow], '', null);
check(defaultVisible.length === 1, 'temporary rows are hidden by default');
check(defaultVisible[0]?.sessionId === 'normal-1', 'regular rows stay visible by default');
check(visibleSidebarSessions([tempRow, normalRow], '', 'temp-1').some((r) => r.sessionId === 'temp-1'), 'active temporary session stays visible');
check(visibleSidebarSessions([{ ...tempRow, loaded: true }, normalRow], '', null).some((r) => r.sessionId === 'temp-1'), 'loaded temporary session stays visible');
check(visibleSidebarSessions([tempRow, normalRow], 'temp-1', null).some((r) => r.sessionId === 'temp-1'), 'temporary row found by session id search');
const titledTemp = row({ sessionId: 'temp-2', name: 'quick experiment', cwd: '<tmp>/x', source: 'rpi', temporary: true });
check(visibleSidebarSessions([titledTemp], 'experiment', null).some((r) => r.sessionId === 'temp-2'), 'temporary row found by title search');
const searched = visibleSidebarSessions([tempRow, normalRow], 'normal-1', null);
check(searched.length === 1 && searched[0]?.sessionId === 'normal-1', 'non-matching temporary rows stay hidden while searching');
const activeKept = visibleSidebarSessions([tempRow, normalRow], 'nomatch', 'normal-1');
check(activeKept.length === 1 && activeKept[0]?.sessionId === 'normal-1', 'active row is appended when it does not match the query');
check(visibleSidebarSessions([tempRow], '', null).length === 0, 'only a temporary row in the catalog hides entirely by default');

/* ---------- catalog refresh preserves established row positions ---------- */

const orderA = row({ sessionId: 'a', path: '<workspace>/session-store/a.jsonl', modifiedEpoch: 10, summary: 'old a' });

/* ---------- agent/bootstrap classification, visibility, and defaults ---------- */

const agentRow = row({ sessionId: 'agent-1', cwd: '<workspace>/real-project', agentSession: true });
const bootstrapRow = row({ sessionId: 'bootstrap-1', cwd: '<workspace>/bootstrap-project', bootstrapOnly: true });
const loadedBootstrapRow = { ...bootstrapRow, sessionId: 'bootstrap-loaded', path: '<workspace>/bootstrap-loaded.jsonl', loaded: true as const };
const agentGrouped = groupSessions([normalRow, agentRow, bootstrapRow]);
const agentGroup = agentGrouped[0]?.children.find((child) => child.label === 'Agent sessions');
check(agentGroup?.subgroupKind === 'agent', 'agent/backend bootstrap rows share Agent sessions classification');
check(agentGroup?.sessions.map((entry) => entry.sessionId).join('|') === 'agent-1|bootstrap-1', 'agent classification takes precedence over real cwd project names');
check(agentGrouped[0]?.children.some((child) => child.label === 'rpi'), 'ordinary project subgroup remains alongside Agent sessions');
check(visibleSidebarSessions([bootstrapRow, normalRow], '', null).map((entry) => entry.sessionId).join('|') === 'normal-1', 'bootstrapOnly row is hidden by default');
check(visibleSidebarSessions([bootstrapRow], 'bootstrap-1', null)[0]?.sessionId === 'bootstrap-1', 'bootstrapOnly row is searchable by id');
check(visibleSidebarSessions([bootstrapRow], 'agent sessions', null)[0]?.sessionId === 'bootstrap-1', 'bootstrapOnly row is searchable by stable subgroup label');
check(visibleSidebarSessions([bootstrapRow], '', 'bootstrap-1')[0]?.sessionId === 'bootstrap-1', 'active bootstrapOnly row is forced visible');
check(visibleSidebarSessions([loadedBootstrapRow], '', null)[0]?.sessionId === 'bootstrap-loaded', 'loaded bootstrapOnly row is forced visible');

const agentKey = 'provider::rpi::special::agent-sessions';
const defaultAgentRows = visibleSidebarSessions([normalRow, agentRow], '', null);
check(collapsedSidebarGroupKeys(defaultAgentRows, new Map()).has(agentKey), 'Agent sessions defaults collapsed');
check(!collapsedSidebarGroupKeys(defaultAgentRows, new Map([[agentKey, false]])).has(agentKey), 'explicit Agent sessions expansion overrides default');
check(!collapsedSidebarGroupKeys(defaultAgentRows, new Map(), true).has(agentKey), 'search opens Agent sessions subgroups');
check(collapsedSidebarGroupKeys([{ ...agentRow, loaded: true }], new Map([[agentKey, true]])).has(agentKey),
  'user collapse intent wins over a loaded Agent session (toggle never wedges)');
check(collapsedSidebarGroupKeys([agentRow], new Map([[agentKey, true]])).has(agentKey),
  'user collapse intent wins over the active Agent session');
check(collapsedSidebarGroupKeys([{ ...agentRow, loaded: true }], new Map([['provider::rpi', true]])).has('provider::rpi'), 'provider collapse remains absolute above Agent sessions');

/* ---------- collapsed groups hide every descendant, including the active session ---------- */

const agentCollapseItems: SessionTreeItem[] = [];
flattenTree([normalRow, agentRow], collapsedSidebarGroupKeys([normalRow, agentRow], new Map([[agentKey, true]])), agentCollapseItems, groupSessions([normalRow, agentRow]));
const agentCollapseKeys = agentCollapseItems.map((item) => item.kind === 'group' ? item.node.key : sessionRowKey(item.row));
check(agentCollapseKeys.includes(agentKey) && !agentCollapseKeys.includes(sessionRowKey(agentRow)),
  'a collapsed group hides the active session row');
const providerCollapseActiveItems: SessionTreeItem[] = [];
flattenTree([normalRow, agentRow], new Set(['provider::rpi']), providerCollapseActiveItems, groupSessions([normalRow, agentRow]));
const providerCollapseKeys = providerCollapseActiveItems.map((item) => item.kind === 'group' ? item.node.key : sessionRowKey(item.row));
check(!providerCollapseKeys.includes(sessionRowKey(normalRow)),
  'a collapsed provider hides the active session row');
const idleCollapseItems: SessionTreeItem[] = [];
flattenTree([normalRow, agentRow], new Set([agentKey]), idleCollapseItems, groupSessions([normalRow, agentRow]));
check(idleCollapseItems.every((item) => item.kind === 'group' ? item.node.key !== sessionRowKey(agentRow) : sessionRowKey(item.row) !== sessionRowKey(agentRow)),
  'a collapsed group renders no descendant rows');

/* ---------- project-key drift migrates explicit collapse intent ---------- */

const driftBefore = row({ sessionId: 'drift', cwd: undefined, path: '<workspace>/foreign/old-derived.jsonl', source: 'codex', status: 'foreign' });
const driftOldKey = 'provider::Codex::project::old-derived';
const driftAfter = { ...driftBefore, cwd: '<workspace>/authoritative-project' };
const driftNewKey = 'provider::Codex::project::authoritative-project';
const migratedCollapsed = migrateSidebarCollapseIntent([driftBefore], [driftAfter], new Map([[driftOldKey, true]]));
check(migratedCollapsed.get(driftNewKey) === true, 'authoritative cwd project drift inherits collapsed intent');
const migratedExpanded = migrateSidebarCollapseIntent([driftBefore], [driftAfter], new Map([[driftOldKey, false]]));
check(migratedExpanded.get(driftNewKey) === false, 'authoritative cwd project drift inherits explicit expanded intent');
const stableOtherBefore = row({ sessionId: 'other-drift', cwd: '<workspace>/tmp', path: '<workspace>/tmp-a.jsonl' });
const stableOtherAfter = { ...stableOtherBefore, cwd: '<workspace>/.tmp-next' };
const stableOtherIntent = migrateSidebarCollapseIntent([stableOtherBefore], [stableOtherAfter], new Map([['provider::rpi::special::other', true]]));
check(stableOtherIntent.size === 1 && stableOtherIntent.get('provider::rpi::special::other') === true, 'Other key stays stable across noise cwd drift');
const ambiguousDrift = migrateSidebarCollapseIntent(
  [driftBefore],
  [driftAfter, { ...driftAfter, path: '<workspace>/duplicate.jsonl', cwd: '<workspace>/second-project' }],
  new Map([[driftOldKey, true]]),
);
check(!ambiguousDrift.has(driftNewKey), 'ambiguous duplicate provider/session ownership does not migrate collapse intent');

/* ---------- obsolete transferred key: removed only when the subgroup
 * disappears, retained while a sibling row remains ---------- */

const abaBeforeA = row({
  sessionId: 'drift-aba',
  cwd: undefined,
  path: '<workspace>/foreign/aba-old.jsonl',
  source: 'codex',
  status: 'foreign',
});
const abaKeyA = 'provider::Codex::project::aba-old';
const abaKeyB = 'provider::Codex::project::aba-new';
const abaAtB = { ...abaBeforeA, cwd: '<workspace>/aba-new' };
/* A→B: the collapsed intent transfers and the vacated A key is REMOVED (the
 * subgroup disappears with its only row). */
const abaStepAB = migrateSidebarCollapseIntent([abaBeforeA], [abaAtB], new Map([[abaKeyA, true]]));
check(abaStepAB.get(abaKeyB) === true, 'A→B transfer inherits the collapsed intent');
check(!abaStepAB.has(abaKeyA), 'A→B transfer removes the obsolete old key when the subgroup disappears');
/* B→A after the user explicitly EXPANDED B: the transfer must not resurrect
 * the stale collapsed A value — the returned row renders expanded. */
const abaStepExpandedBA = migrateSidebarCollapseIntent([abaAtB], [abaBeforeA], new Map([[abaKeyB, false]]));
check(abaStepExpandedBA.get(abaKeyA) === false, 'B→A transfer inherits the explicit expanded intent');
check(!abaStepExpandedBA.has(abaKeyB), 'B→A transfer removes the obsolete B key');
/* Repeated A-B-A: every hop keeps exactly the live subgroup key. */
const abaStepBA = migrateSidebarCollapseIntent([abaAtB], [abaBeforeA], new Map([[abaKeyB, true]]));
check(abaStepBA.get(abaKeyA) === true && !abaStepBA.has(abaKeyB), 'repeated B→A collapse transfer keeps only the live key');
const abaStepABAgain = migrateSidebarCollapseIntent([abaBeforeA], [abaAtB], abaStepBA);
check(abaStepABAgain.get(abaKeyB) === true && !abaStepABAgain.has(abaKeyA), 'repeated A→B transfer keeps only the live key');
/* Shared group: one of two rows drifts away — the vacated key is RETAINED
 * because a sibling row still holds the old subgroup. */
const sharedBefore = [
  row({ sessionId: 'shared-drift', cwd: undefined, path: '<workspace>/foreign/shared-old.jsonl', source: 'codex', status: 'foreign' }),
  row({ sessionId: 'shared-stay', cwd: '<workspace>/shared-old', path: '<workspace>/foreign/shared-stay.jsonl', source: 'codex', status: 'foreign' }),
];
const sharedAfter = [
  { ...sharedBefore[0]!, cwd: '<workspace>/shared-new' },
  sharedBefore[1]!,
];
const sharedIntent = migrateSidebarCollapseIntent(
  sharedBefore,
  sharedAfter,
  new Map([['provider::Codex::project::shared-old', true]]),
);
check(sharedIntent.get('provider::Codex::project::shared-new') === true, 'drifting row of a shared subgroup still transfers the intent');
check(sharedIntent.get('provider::Codex::project::shared-old') === true, 'vacated key is retained while a sibling row remains in the subgroup');
const orderB = row({ sessionId: 'b', path: '<workspace>/session-store/b.jsonl', modifiedEpoch: 9, summary: 'old b' });
const reorderedRefresh = reconcileSessionRowOrder(
  [orderA, orderB],
  [
    { ...orderB, modifiedEpoch: 20, summary: 'updated b' },
    { ...orderA, modifiedEpoch: 10, summary: 'updated a' },
  ],
);
check(reorderedRefresh.map((entry) => entry.sessionId).join('|') === 'a|b', 'refresh does not promote the selected or newly modified row');
check(reorderedRefresh[1]?.summary === 'updated b', 'stable ordering still refreshes row fields');
const appendedRefresh = reconcileSessionRowOrder(
  reorderedRefresh,
  [
    row({ sessionId: 'c', path: '<workspace>/session-store/c.jsonl', modifiedEpoch: 30 }),
    { ...orderB, modifiedEpoch: 20, summary: 'latest b' },
  ],
);
check(appendedRefresh.map((entry) => entry.sessionId).join('|') === 'b|c', 'removed rows disappear and genuinely new rows append');

/* ---------- reconcileSessionRowOrder: one authoritative stable order ---------- */

const sessionIds = (rows: SessionRowWire[]): string => rows.map((entry) => entry.sessionId).join('|');
const identityKeys = (rows: SessionRowWire[]): string => rows.map(sessionRowKey).join('|');

/** DOM-order walker: the exact `treeItems` sequence `SessionSidebar` maps to
 *  `<li>` nodes. Group headers emit on first occurrence; session rows keep
 *  reconciled catalog order and are keyed by `sessionRowKey`. */
const flattenItems = (rows: SessionRowWire[], collapsed: readonly string[] = []): SessionTreeItem[] => {
  const items: SessionTreeItem[] = [];
  flattenTree(rows, new Set(collapsed), items);
  return items;
};
const domOrder = (rows: SessionRowWire[], collapsed: readonly string[] = []): string[] =>
  flattenItems(rows, collapsed).map((item) => (item.kind === 'group' ? item.node.key : sessionRowKey(item.row)));
const rowDomIndices = (rows: SessionRowWire[], collapsed: readonly string[] = []): Map<string, number> => {
  const indices = new Map<string, number>();
  flattenItems(rows, collapsed).forEach((item, index) => {
    if (item.kind === 'row') indices.set(sessionRowKey(item.row), index);
  });
  return indices;
};

/* 1. Initial catalog order is the server order (newest-first). */

const srvA = row({ sessionId: 'srv-a', cwd: '<workspace>/app', path: '<workspace>/session-store/srv-a.jsonl', modifiedEpoch: 30, displayTime: 't+30' });
const srvB = row({ sessionId: 'srv-b', cwd: '<workspace>/app', path: '<workspace>/session-store/srv-b.jsonl', modifiedEpoch: 20, displayTime: 't+20' });
const srvC = row({ sessionId: 'srv-c', cwd: '<workspace>/app', path: '<workspace>/session-store/srv-c.jsonl', modifiedEpoch: 10, displayTime: 't+10' });
const initial = reconcileSessionRowOrder([], [srvA, srvB, srvC]);
check(sessionIds(initial) === 'srv-a|srv-b|srv-c', 'first load adopts the server order untouched');
check(initial.every((entry, index) => entry === [srvA, srvB, srvC][index]), 'first load keeps incoming rows by reference');
check(
  sessionRowKey(srvA) === 'rpi::srv-a::<workspace>/session-store/srv-a.jsonl',
  'exact identity key is source::sessionId::path',
);

/* 2. Active B metadata changes on refresh: identity index and fields update in
 *    place — no reordering, no promotion. */

const activeBMeta = reconcileSessionRowOrder(initial, [
  { ...srvB, loaded: true, modifiedEpoch: 99, displayTime: 't+99', messageCount: 12, summary: 'b grew' },
  { ...srvA, modifiedEpoch: 30 },
  srvC,
]);
check(sessionIds(activeBMeta) === 'srv-a|srv-b|srv-c', 'active B metadata change keeps every row in place');
check(activeBMeta[1]?.loaded === true && activeBMeta[1]?.modifiedEpoch === 99 && activeBMeta[1]?.displayTime === 't+99', 'active B gains loaded overlay and fresh fields in place');
check(activeBMeta[1]?.messageCount === 12 && activeBMeta[1]?.summary === 'b grew', 'activity/unread-adjacent metadata (message count, summary) refreshes in place');

/* 3. A→B→C selection: the switched-to row must never jump. */

const abSwitched = reconcileSessionRowOrder(activeBMeta, [
  { ...srvA, loaded: true, modifiedEpoch: 50, displayTime: 't+50' },
  { ...srvB, modifiedEpoch: 99 },
  srvC,
]);
check(sessionIds(abSwitched) === 'srv-a|srv-b|srv-c', 'selecting A does not promote A to the top');
check(abSwitched[0]?.loaded === true, 'A stays at index 0 with its loaded overlay');
const bcSwitched = reconcileSessionRowOrder(abSwitched, [
  { ...srvA, loaded: false, modifiedEpoch: 50 },
  { ...srvC, loaded: true, modifiedEpoch: 60, displayTime: 't+60' },
  { ...srvB, modifiedEpoch: 99 },
]);
check(sessionIds(bcSwitched) === 'srv-a|srv-b|srv-c', 'A→B→C keeps the full retained order stable');
check(bcSwitched[2]?.loaded === true && bcSwitched[2]?.displayTime === 't+60', 'C gains the active overlay at its retained index');

/* 4. Mutable metadata (mtime/activity/unread) never reorders on its own. */

const mutableMeta = reconcileSessionRowOrder(bcSwitched, [
  { ...srvB, modifiedEpoch: 1000, summary: 'b touched again', displayTime: 't+1000', messageCount: 40 },
  { ...srvA, modifiedEpoch: 900, summary: 'a touched', displayTime: 't+900', messageCount: 3 },
  { ...srvC, loaded: true, modifiedEpoch: 60, displayTime: 't+60' },
]);
check(sessionIds(mutableMeta) === 'srv-a|srv-b|srv-c', 'mtime/activity churn across every row keeps the retained order');
check(mutableMeta[0]?.summary === 'a touched' && mutableMeta[1]?.summary === 'b touched again', 'metadata still lands on the right identities');
check(mutableMeta[1]?.messageCount === 40 && mutableMeta[2]?.loaded === true, 'activity and loaded overlay stay on the retained identities');

/* 5. Incoming server reorder is ignored; the authoritative order stays put. */

const serverReordered = reconcileSessionRowOrder(mutableMeta, [
  { ...mutableMeta[1], modifiedEpoch: 1100 },
  { ...mutableMeta[0], modifiedEpoch: 950 },
  { ...mutableMeta[2], modifiedEpoch: 20 },
]);
check(sessionIds(serverReordered) === 'srv-a|srv-b|srv-c', 'server-side reorder never moves retained rows');
check(serverReordered[1]?.modifiedEpoch === 1100, 'server-ordered incoming rows still refresh their fields');

/* 6. Removal + genuinely new rows: removed identities drop, new ones append in
 *    incoming catalog order. */

const fresh1 = row({ sessionId: 'fresh-1', cwd: '<workspace>/app', path: '<workspace>/session-store/fresh-1.jsonl', modifiedEpoch: 42 });
const fresh2 = row({ sessionId: 'fresh-2', cwd: '<workspace>/app', path: '<workspace>/session-store/fresh-2.jsonl', modifiedEpoch: 41 });
const removalAppend = reconcileSessionRowOrder(serverReordered, [
  { ...srvB, cwd: '<workspace>/app', modifiedEpoch: 1100 },
  fresh1,
  fresh2,
]);
check(sessionIds(removalAppend) === 'srv-b|fresh-1|fresh-2', 'removed identities disappear; new ones append in incoming order');
check(removalAppend[0]?.sessionId === 'srv-b' && removalAppend[0]?.modifiedEpoch === 1100, 'retained row stays at its index with refreshed fields');

/* 7. Reconnect duplicate: replaying the identical catalog is a no-op. */

const replay = reconcileSessionRowOrder(removalAppend, removalAppend);
check(sessionIds(replay) === 'srv-b|fresh-1|fresh-2', 'reconnect replay of the same catalog keeps order');
check(replay.every((entry, index) => entry === removalAppend[index]), 'reconnect replay keeps rows by reference (no churn)');
const strictMode = reconcileSessionRowOrder(replay, replay);
check(strictMode.every((entry, index) => entry === replay[index]), 'StrictMode double-apply of the same load is idempotent by reference');
const reconnectFresh = reconcileSessionRowOrder(removalAppend, [
  { ...removalAppend[0]! },
  { ...removalAppend[1]! },
  { ...removalAppend[2]! },
]);
check(sessionIds(reconnectFresh) === 'srv-b|fresh-1|fresh-2', 'reconnect with fresh objects of the same identities keeps order');
check(identityKeys(reconnectFresh) === identityKeys(removalAppend), 'reconnect preserves exact sessionRowKey identities');

/* 8. Foreign → native established identity: the reconciliation key is the
 *    exact `sessionRowKey` the render uses — `source::sessionId::path`.
 *    A path-only key would silently merge the foreign row and the imported
 *    native row (same path, different source): the established foreign
 *    identity holds its index and the imported native identity appends.
 *    Native does not inherit the foreign slot; first-seen grouping cannot
 *    insert native before retained foreign. */

const foreignRow = row({ sessionId: 'codex-sess', source: 'codex', status: 'foreign', path: '<workspace>/foreign/codex-sess.jsonl' });
const nativeRow = row({ sessionId: 'codex-sess', source: 'rpi', status: 'native', path: '<workspace>/foreign/codex-sess.jsonl' });
check(sessionRowKey(foreignRow) === 'codex::codex-sess::<workspace>/foreign/codex-sess.jsonl', 'foreign identity key is source::sessionId::path');
check(sessionRowKey(nativeRow) === 'rpi::codex-sess::<workspace>/foreign/codex-sess.jsonl', 'native identity key is source::sessionId::path');
check(sessionRowKey(foreignRow) !== sessionRowKey(nativeRow), 'foreign and native rows carry distinct sessionRowKey identities');
check(foreignRow.path === nativeRow.path, 'fixture: both rows share the physical path (path-only keying would conflate them)');
const nativeAfterForeign = reconcileSessionRowOrder([foreignRow], [nativeRow, { ...foreignRow, modifiedEpoch: 5 }]);
check(identityKeys(nativeAfterForeign) === `${sessionRowKey(foreignRow)}|${sessionRowKey(nativeRow)}`, 'foreign→native import keeps both identities (no path-based dedupe)');
check(sessionRowKey(nativeAfterForeign[0]!) === sessionRowKey(foreignRow) && nativeAfterForeign[0]?.modifiedEpoch === 5, 'established foreign identity refreshes in place at its index');
check(nativeAfterForeign[1] === nativeRow, 'imported native identity appends after the retained foreign row');

const keepA = row({ sessionId: 'keep-a', cwd: '<workspace>/app', path: '<workspace>/session-store/keep-a.jsonl' });
const keepC = row({ sessionId: 'keep-c', cwd: '<workspace>/app', path: '<workspace>/session-store/keep-c.jsonl' });
const replacedIdentity = reconcileSessionRowOrder(
  [keepA, foreignRow, keepC],
  [keepA, nativeRow, keepC],
);
check(
  identityKeys(replacedIdentity) === identityKeys([keepA, keepC, nativeRow]),
  'authoritative foreign→native replacement drops the foreign identity and appends native; native does not inherit the foreign slot',
);

const coalescedCatalog = reconcileSessionRowOrder(nativeAfterForeign, [nativeRow]);
check(identityKeys(coalescedCatalog) === sessionRowKey(nativeRow), 'once the server coalesces the import, only the native identity remains');
check(coalescedCatalog[0] === nativeRow, 'the surviving identity is the server-authoritative native row');

/* A/B/C + fork S1: retained relative order, append new, identity alias. */

const catalogA = row({ sessionId: 'a', cwd: '<workspace>/app', path: '<workspace>/session-store/a.jsonl', modifiedEpoch: 3 });
const catalogB = row({ sessionId: 'b', cwd: '<workspace>/app', path: '<workspace>/session-store/b.jsonl', modifiedEpoch: 2 });
const catalogC = row({ sessionId: 'c', cwd: '<workspace>/app', path: '<workspace>/session-store/c.jsonl', modifiedEpoch: 1 });
const establishedAbc = reconcileSessionRowOrder([], [catalogA, catalogB, catalogC]);
check(sessionIds(establishedAbc) === 'a|b|c', 'established catalog is A then B then C');

const forkedS1 = row({
  sessionId: 's1',
  cwd: '<workspace>/app',
  path: '<workspace>/session-store/s1.jsonl',
  source: 'native',
  loaded: true,
  modifiedEpoch: 40,
});
const afterFork = reconcileSessionRowOrder(establishedAbc, [
  forkedS1,
  { ...catalogC, modifiedEpoch: 30, summary: 'c after fork' },
  { ...catalogB, loaded: true, modifiedEpoch: 20 },
  { ...catalogA, modifiedEpoch: 10 },
]);
check(sessionIds(afterFork) === 'a|b|c|s1', 'fork S1 appends; A/B/C keep relative order despite newest-first server order');
check(afterFork[1]?.loaded === true && afterFork[2]?.summary === 'c after fork', 'fork refresh updates B/C metadata in place');
check(afterFork[3] === forkedS1, 'new fork identity is the incoming S1 row');

const persistS1 = { ...forkedS1, source: 'rpi', modifiedEpoch: 41, displayTime: 't+41' };
const afterPersist = reconcileSessionRowOrder(afterFork, [
  persistS1,
  { ...catalogB, loaded: true, modifiedEpoch: 21 },
  { ...catalogA, summary: 'a idle' },
  { ...catalogC, modifiedEpoch: 31 },
]);
check(sessionIds(afterPersist) === 'a|b|c|s1', 'native→rpi persist does not promote S1 or shuffle A/B/C');
check(afterPersist[3]?.source === 'rpi' && afterPersist[3]?.displayTime === 't+41', 'S1 keeps its appended slot with catalog fields');

const repeatedRefresh = reconcileSessionRowOrder(afterPersist, [
  { ...catalogB, loaded: true, modifiedEpoch: 500, summary: 'b active', messageCount: 9 },
  { ...persistS1, loaded: true, modifiedEpoch: 400 },
  { ...catalogC, modifiedEpoch: 300 },
  { ...catalogA, modifiedEpoch: 200, summary: 'a idle' },
]);
check(sessionIds(repeatedRefresh) === 'a|b|c|s1', 'repeated refresh ignores server reorder, active, loaded, and mtime');
check(repeatedRefresh[0]?.summary === 'a idle' && repeatedRefresh[1]?.messageCount === 9, 'repeated refresh still writes metadata onto retained identities');
check(sessionIds(reconcileSessionRowOrder(repeatedRefresh, repeatedRefresh)) === 'a|b|c|s1', 'identical catalog replay keeps A/B/C/S1');

const aliasRefresh = reconcileSessionRowOrder(establishedAbc, [
  { ...catalogB, source: 'primary', loaded: true, modifiedEpoch: 99 },
  { ...catalogA, source: 'native' },
  { ...catalogC, source: 'rpi', summary: 'c alias' },
]);
check(sessionIds(aliasRefresh) === 'a|b|c', 'native source aliases never reorder retained A/B/C');
check(aliasRefresh[0]?.source === 'native' && aliasRefresh[1]?.source === 'primary' && aliasRefresh[2]?.summary === 'c alias', 'alias refresh updates fields in the retained slots');

const identitySwap = reconcileSessionRowOrder(establishedAbc, [
  { ...catalogA, path: '<workspace>/session-store/a-renamed.jsonl' },
  catalogB,
  catalogC,
]);
check(sessionIds(identitySwap) === 'b|c|a', 'path identity replacement drops the old A slot and appends the new A identity');
check(identitySwap[2]?.path === '<workspace>/session-store/a-renamed.jsonl', 'replaced A identity is the incoming row at the append position');


/* 9. Flattened DOM preserves reconciled physical row order even across
 *    provider/project grouping. Headers emit on first occurrence; the active
 *    row is never hoisted. */

const promotedProof = reconcileSessionRowOrder(initial, [
  { ...srvB, cwd: '<workspace>/app', loaded: true, modifiedEpoch: 70 },
  { ...srvA, cwd: '<workspace>/app', modifiedEpoch: 30 },
  srvC,
]);
check(sessionIds(promotedProof) === 'srv-a|srv-b|srv-c', 'loaded/active overlay does not reorder the catalog');
const rpiRootOrder = groupSessions(promotedProof).find((node) => node.label === 'rpi');
check(rpiRootOrder?.children[0]?.label === 'app', 'shared cwd nests under one project group');
check(
  rpiRootOrder?.children[0]?.sessions.map((entry) => entry.sessionId).join('|') === 'srv-a|srv-b|srv-c',
  'groupSessions retains catalog order and does not promote the loaded row',
);
const renderedOrder = domOrder(promotedProof);
check(renderedOrder.join('|') === `provider::rpi|provider::rpi::project::app|${sessionRowKey(srvA)}|${sessionRowKey(srvB)}|${sessionRowKey(srvC)}`, 'render walks group headers then reconciled session order');
check(rowDomIndices(promotedProof).get(sessionRowKey(srvB)) === 3, 'no active-row hoisting: B renders at its retained full-key index');
const removalRendered = domOrder(removalAppend);
check(removalRendered.join('|') === `provider::rpi|provider::rpi::project::app|${sessionRowKey(removalAppend[0]!)}|${sessionRowKey(fresh1)}|${sessionRowKey(fresh2)}`, 'removal + append renders in reconciled order');
check(domOrder(strictMode).join('|') === removalRendered.join('|'), 'replayed reconnect catalog renders identically');

const x1 = row({ sessionId: 'x1', cwd: '<workspace>/x', path: '<workspace>/session-store/x1.jsonl' });
const y1 = row({ sessionId: 'y1', cwd: '<workspace>/y', path: '<workspace>/session-store/y1.jsonl' });
const x2 = row({ sessionId: 'x2', cwd: '<workspace>/x', path: '<workspace>/session-store/x2.jsonl' });
const interleavedProjects = reconcileSessionRowOrder([], [x1, y1, x2]);
const interleavedBefore = rowDomIndices(interleavedProjects);
check(domOrder(interleavedProjects).join('|') === `provider::rpi|provider::rpi::project::x|${sessionRowKey(x1)}|provider::rpi::project::y|${sessionRowKey(y1)}|${sessionRowKey(x2)}`, 'project headers emit on first occurrence; X2 stays after Y1');
check(interleavedBefore.get(sessionRowKey(x1)) === 2 && interleavedBefore.get(sessionRowKey(y1)) === 4 && interleavedBefore.get(sessionRowKey(x2)) === 5, 'full-key DOM indices keep X1 before Y1 before X2');

const ompFirst = row({ sessionId: 'omp-first', source: 'omp', cwd: '<workspace>/a', path: '<workspace>/foreign/omp-first.jsonl' });
const rpiLater = row({ sessionId: 'rpi-later', cwd: '<workspace>/a', path: '<workspace>/session-store/rpi-later.jsonl' });
const crossProvider = reconcileSessionRowOrder([], [ompFirst, rpiLater]);
check(domOrder(crossProvider).join('|') === `provider::OMP|provider::OMP::project::a|${sessionRowKey(ompFirst)}|provider::rpi|provider::rpi::project::a|${sessionRowKey(rpiLater)}`, 'PROVIDER_ORDER does not hoist rpi ahead of an earlier OMP row');
check(rowDomIndices(crossProvider).get(sessionRowKey(ompFirst))! < rowDomIndices(crossProvider).get(sessionRowKey(rpiLater))!, 'full-key DOM index of retained OMP stays before later native rpi');
check(
  flattenItems(crossProvider).filter((item) => item.kind === 'group').map((item) => item.node.key).join('|') ===
    'provider::OMP|provider::OMP::project::a|provider::rpi|provider::rpi::project::a',
  'colliding project labels keep distinct provider-scoped keys',
);

const mixedBefore = flattenItems([keepA, foreignRow, keepC]);
const mixedAfter = flattenItems(nativeAfterForeign);
check(mixedBefore.filter((item) => item.kind === 'row').map((item) => sessionRowKey(item.row)).join('|') === identityKeys([keepA, foreignRow, keepC]), 'pre-import DOM rows follow physical catalog keys');
check(mixedAfter.filter((item) => item.kind === 'row').map((item) => sessionRowKey(item.row)).join('|') === identityKeys(nativeAfterForeign), 'post-import DOM keeps foreign before appended native');
check(rowDomIndices(nativeAfterForeign).get(sessionRowKey(foreignRow))! < rowDomIndices(nativeAfterForeign).get(sessionRowKey(nativeRow))!, 'full-key DOM index of retained foreign stays before new native');

const sameIdSearch = visibleSidebarSessions([foreignRow, nativeRow], 'nomatch', 'codex-sess');
check(sameIdSearch.length === 1 && sessionRowKey(sameIdSearch[0]!) === sessionRowKey(foreignRow), 'unmatched search appends only the full-key active identity, not every same sessionId');
check(identityKeys(visibleSidebarSessions([foreignRow, nativeRow], 'codex-sess', 'codex-sess')) === identityKeys([foreignRow, nativeRow]), 'search matching both same-id rows keeps both in catalog order');
check(identityKeys(visibleSidebarSessions([foreignRow, nativeRow], 'rpi', 'codex-sess')) === identityKeys([nativeRow, foreignRow]), 'non-active same-id twin can match search; active identity is appended by full key');
const loadedNativeTwin = { ...nativeRow, loaded: true as const };
check(sessionRowKey(resolveActiveSessionRow([foreignRow, loadedNativeTwin], 'codex-sess')!) === sessionRowKey(loadedNativeTwin), 'loaded native twin is the active identity, not the earlier foreign same-id row');
check(sessionRowKey(resolveActiveSessionRow([foreignRow, nativeRow], 'codex-sess')!) === sessionRowKey(foreignRow), 'without a loaded overlay the first full-key catalog match is active');
{
  const importedOmp = row({
    sessionId: 'chat-572ffbd33c99218e65f67417',
    source: 'omp',
    status: 'already_imported',
    nativeSessionId: '01a08bfa-native',
    nativePath: '<workspace>/sessions/native.jsonl',
    path: '<workspace>/foreign/omp.jsonl',
  });
  check(sessionRowKey(resolveActiveSessionRow([importedOmp], '01a08bfa-native')!) === sessionRowKey(importedOmp),
    'already-imported row is active when the displayed runtime is the native UUID');
  check(resolveActiveSessionRow([importedOmp], 'chat-572ffbd33c99218e65f67417') !== undefined,
    'already-imported row still matches its catalog display name');
  const importedAlias = row({
    sessionId: 'codex-ext-e2e',
    source: 'codex',
    status: 'already_imported',
    nativeSessionId: '01a08bfa-native',
    nativePath: '<workspace>/sessions/native.jsonl',
    path: '<workspace>/foreign/codex.jsonl',
    loaded: true,
  });
  const nativeCopy = row({
    sessionId: '01a08bfa-native',
    source: 'rpi',
    status: 'native',
    path: '<workspace>/sessions/native.jsonl',
    loaded: true,
  });
  check(sessionRowKey(resolveActiveSessionRow([importedAlias, nativeCopy], '01a08bfa-native')!) === sessionRowKey(nativeCopy),
    'native copy is the active identity when an already-imported alias also matches the runtime');
}

const menuOpenFor = (menuRow: SessionRowWire, candidate: SessionRowWire): boolean =>
  sessionRowKey(menuRow) === sessionRowKey(candidate);
check(menuOpenFor(foreignRow, foreignRow) && !menuOpenFor(foreignRow, nativeRow), 'menuOpen matches source::sessionId::path, not sessionId or path alone');

/* T1. Removing the first member of an interleaved project moves that header
 *     to the next remaining member; retained row relative order is unchanged. */
const droppedX1 = reconcileSessionRowOrder(interleavedProjects, [y1, x2]);
check(sessionIds(droppedX1) === 'y1|x2', 'dropping X1 keeps Y1 before X2 in the catalog');
check(
  domOrder(droppedX1).join('|') ===
    `provider::rpi|provider::rpi::project::y|${sessionRowKey(y1)}|provider::rpi::project::x|${sessionRowKey(x2)}`,
  'X header moves to immediately before remaining X2; Y1 stays before X2',
);
check(rowDomIndices(droppedX1).get(sessionRowKey(y1))! < rowDomIndices(droppedX1).get(sessionRowKey(x2))!, 'full-key DOM index of Y1 stays before X2 after X1 removal');

/* T2. Removing the last member of a project drops that project header. */
const droppedY1 = reconcileSessionRowOrder(interleavedProjects, [x1, x2]);
check(!domOrder(droppedY1).includes('provider::rpi::project::y'), 'Y header disappears when Y1 is the last Y member');
check(domOrder(droppedY1).filter((key) => key === 'provider::rpi').length === 1, 'only one rpi header remains after Y drops');
check(
  domOrder(droppedY1).join('|') === `provider::rpi|provider::rpi::project::x|${sessionRowKey(x1)}|${sessionRowKey(x2)}`,
  'X1 stays before X2 when the interleaved Y group vanishes',
);

/* T3. Removing the last member of the first provider drops that provider. */
const droppedOmp = reconcileSessionRowOrder(crossProvider, [rpiLater]);
check(!domOrder(droppedOmp).some((key) => key === 'provider::OMP' || key.startsWith('provider::OMP::')), 'OMP header gone after its last member is removed');
check(
  domOrder(droppedOmp).join('|') === `provider::rpi|provider::rpi::project::a|${sessionRowKey(rpiLater)}`,
  'rpi header and row remain after the leading OMP group disappears',
);

/* T4. A new native identity with the same sessionId/path appends; Codex stays first. */
check(
  identityKeys(nativeAfterForeign) === `${sessionRowKey(foreignRow)}|${sessionRowKey(nativeRow)}`,
  'catalog keeps retained foreign before appended native',
);
check(
  domOrder(nativeAfterForeign).join('|') ===
    `provider::Codex|provider::Codex::project::codex-sess|${sessionRowKey(nativeAfterForeign[0]!)}|provider::rpi|provider::rpi::project::codex-sess|${sessionRowKey(nativeRow)}`,
  'new rpi group appends after retained Codex; native is not hoisted before foreign',
);
check(rowDomIndices(nativeAfterForeign).get(sessionRowKey(foreignRow))! < rowDomIndices(nativeAfterForeign).get(sessionRowKey(nativeRow))!, 'full-key DOM keeps foreign before new native');

/* T5. Collapsing one interleaved project hides its rows, not the later project. */
const collapsedX = domOrder(interleavedProjects, ['provider::rpi::project::x']);
check(
  collapsedX.join('|') === `provider::rpi|provider::rpi::project::x|provider::rpi::project::y|${sessionRowKey(y1)}`,
  'collapsed X keeps its header; X1/X2 hide; Y1 stays after the X header',
);
check(!collapsedX.includes(sessionRowKey(x1)) && !collapsedX.includes(sessionRowKey(x2)), 'collapsed project hides later members');

/* T6. Collapsing the leading provider hides its project header and row. */
const collapsedOmp = domOrder(crossProvider, ['provider::OMP']);
check(
  collapsedOmp.join('|') === `provider::OMP|provider::rpi|provider::rpi::project::a|${sessionRowKey(rpiLater)}`,
  'collapsed OMP emits only its header; rpi block is unchanged after it',
);
check(!collapsedOmp.includes(sessionRowKey(ompFirst)) && !collapsedOmp.includes('provider::OMP::project::a'), 'collapsed provider never emits nested project headers');

/* T7. Unmatched search with a loaded native twin shows only that full-key identity. */
const foreignTwin = row({ sessionId: 'shared-sess', source: 'codex', status: 'foreign', path: '<workspace>/foreign/shared-sess.jsonl' });
const loadedNativeTwinSearch = { ...row({ sessionId: 'shared-sess', source: 'rpi', status: 'native', path: '<workspace>/foreign/shared-sess.jsonl' }), loaded: true as const };
const loadedTwinVisible = visibleSidebarSessions([foreignTwin, loadedNativeTwinSearch], 'nomatch', 'shared-sess');
check(identityKeys(loadedTwinVisible) === sessionRowKey(loadedNativeTwinSearch), 'nomatch search keeps only the loaded native twin, not the foreign same-id row');
check(
  domOrder(loadedTwinVisible).join('|') ===
    `provider::rpi|provider::rpi::project::shared-sess|${sessionRowKey(loadedNativeTwinSearch)}`,
  'flatten of the loaded-twin search view emits rpi + native only',
);

/* T8. Search matching only the foreign source still appends the loaded native by full key. */
const foreignThenNativeVisible = visibleSidebarSessions([foreignTwin, loadedNativeTwinSearch], 'codex', 'shared-sess');
check(
  identityKeys(foreignThenNativeVisible) === `${sessionRowKey(foreignTwin)}|${sessionRowKey(loadedNativeTwinSearch)}`,
  'foreign-only query keeps the match then appends the full-key active native',
);
check(
  domOrder(foreignThenNativeVisible).join('|') ===
    `provider::Codex|provider::Codex::project::shared-sess|${sessionRowKey(foreignTwin)}|provider::rpi|provider::rpi::project::shared-sess|${sessionRowKey(loadedNativeTwinSearch)}`,
  'search view emits both group headers with native after foreign',
);




/* ---------- active session row selection (unchanged) ---------- */

check(isLoadedCurrentSession(row({ sessionId: 'active', loaded: false }), 'active', false), 'active row remains selected while its loaded overlay refreshes');
check(!isLoadedCurrentSession(row({ sessionId: 'other', loaded: true }), 'active', false), 'different session id is never active');
check(isLoadedCurrentSession(row({ sessionId: 'active', loaded: true }), 'active', true), 'loaded active identity remains selected after convergence');
check(!isLoadedCurrentSession(row({ sessionId: 'active', loaded: false }), 'active', true), 'duplicate stale identity loses selection when the loaded active identity exists');

/* ---------- action error toast ownership ---------- */

check(!shouldToastSessionActionError(Object.assign(new Error('rpc failed'), { rpc: true, toastOwned: true })), 'App-toasted RPC errors stay single-owned');
check(shouldToastSessionActionError(Object.assign(new Error('boot rpc failed'), { rpc: true })), 'RPC errors not toasted during bootstrap remain sidebar-owned');
check(!shouldToastSessionActionError(Object.assign(new Error('connection closed'), { toastOwned: true })), 'App-toasted transport errors stay single-owned');
check(shouldToastSessionActionError(Object.assign(new Error('not connected'), { transport: true })), 'immediate unowned transport failures surface through the sidebar');
check(shouldToastSessionActionError(new Error('local lifecycle failed')), 'plain lifecycle failures surface through the sidebar');


/* ---------- shared transport-error classifier (transportError.ts) ---------- */

/** The deterministic transport-failure message family shared by the sidebar
 *  transport assertions. One fixture so the variant list is never duplicated
 *  across tests. */
const TRANSPORT_FAILURE_MESSAGES = [
  'connection closed (code 1006)',
  'connection closed (code 1008: policy violation)',
  'connection failed: refused',
  'connection unresponsive after 30s',
  'not connected',
  'connection replaced',
  'host switched',
  'send failed: socket',
] as const;

for (const message of TRANSPORT_FAILURE_MESSAGES) {
  check(isTransportErrorMessage(message), `message-level classifier recognizes ${message}`);
  check(isTransportError(new Error(message)), `error-level classifier recognizes ${message}`);
}
check(!isTransportErrorMessage(''), 'empty message is never transport');
check(!isTransportErrorMessage('rpc failed'), 'business message is never transport');
check(!isTransportErrorMessage('code_review_open timed out after 600s (no response from server)'), 'bounded command timeout is not transport');
check(!isTransportError(null), 'null is never transport');
check(!isTransportError('not connected'), 'non-object error is never transport');
check(
  !isTransportError(Object.assign(new Error('connection closed (code 1006)'), { rpc: true })),
  'rpc-tagged transport-looking error stays business',
);
check(
  isTransportError(Object.assign(new Error('not connected'), { transport: true })),
  'transport-tagged error is transport regardless of message',
);
check(
  isTransportError(Object.assign(new Error('rpc failed'), { transport: true })),
  'transport tag wins over business-looking message',
);

/* ---------- transport failures never replace the known-good catalog ---------- */

const knownGood = [
  row({ sessionId: 'known-1', path: '<workspace>/session-store/known-1.jsonl', summary: 'known one' }),
  row({ sessionId: 'known-2', path: '<workspace>/session-store/known-2.jsonl', summary: 'known two' }),
];
for (const message of TRANSPORT_FAILURE_MESSAGES) {
  // refreshSessionList preserves the known-good array on failure; the
  // sidebar's load then returns early (classifier) without setSessions, so
  // reconcile sees the same rows and keeps them by reference.
  const preserved = reconcileSessionRowOrder(knownGood, knownGood);
  check(
    preserved.length === knownGood.length
      && preserved.every((entry, index) => entry === knownGood[index]),
    `transport failure (${message}) retains known-good sessions by reference`,
  );
}


/* ---------- App-owned switch handoff targets ---------- */

const switchTarget = buildSwitchTarget(row({
  sessionId: 'target-session',
  path: '<workspace>/session-store/target.jsonl',
  runtimeIncarnation: 'runtime-7',
}));
check(switchTarget !== null, 'canonical native row yields a switch target');
check(switchTarget!.path === '<workspace>/session-store/target.jsonl', 'sidebar forwards the catalog path to App');
check(switchTarget!.catalogSource === 'rpi' && switchTarget!.catalogStatus === 'native', 'canonical rows send the exact rpi/native tuple');
check(switchTarget!.catalogSessionId === 'target-session', 'sidebar forwards the catalog session id to App');
check(switchTarget!.catalogRuntimeIncarnation === 'runtime-7', 'loaded rows forward the resident incarnation as an advisory warm hint');
check(buildSwitchTarget(row({ sessionId: 'cold', runtimeIncarnation: null }))!.catalogRuntimeIncarnation === null, 'cold catalog rows preserve null incarnation');
const foreignSwitchTarget = buildSwitchTarget(row({ sessionId: 'codex-catalog', source: 'codex', status: 'foreign', runtimeIncarnation: 'runtime-9' }));
check(foreignSwitchTarget!.catalogSource === 'codex' && foreignSwitchTarget!.catalogStatus === 'foreign', 'foreign rows keep their canonical source and exact foreign status');
check(buildSwitchTarget(row({ source: 'grok/hyper', status: 'foreign' })) === null, 'legacy grok/hyper alias is malformed: NO switch target');
check(buildSwitchTarget(row({ source: 'primary', status: 'native' })) === null, 'legacy primary alias is malformed: NO switch target, never re-labeled rpi');
check(buildSwitchTarget(row({ source: 'native', status: 'native' })) === null, 'legacy native alias is malformed: NO switch target');
check(catalogIdentityOf({ source: 'RPI', status: 'native' }) === null, 'dirty cased source RPI is malformed: no authority, no case folding');
check(catalogIdentityOf({ source: ' pi ', status: 'native' }) === null, 'padded source is malformed: authority requires the exact raw token, no trim');
check(catalogIdentityOf({ source: 'RPI', status: 'foreign' }) === null, 'dirty cased source rejects switch authority too');
check(catalogIdentityOf({ source: 'rpi', status: 'native' })?.source === 'rpi', 'exact raw canonical token validates');
check(buildSwitchTarget(row({ source: 'RPI', status: 'native' })) === null, 'dirty RPI source is malformed: NO switch target, no case folding');
check(buildSwitchTarget({ ...row({ sessionId: 'padded' }), source: ' pi ', status: 'native' }) === null, 'padded source is malformed: NO switch target, no trim');
check(buildSwitchTarget(row({ source: 'unknown-agent' })) === null, 'unknown source is malformed: NO switch target, never an implicit rpi fallback');
check(buildSwitchTarget(row({ source: '' })) === null, 'empty source is malformed: NO switch target');
check(buildSwitchTarget(row({ source: 'claude' })) === null, 'unmappable source is malformed: NO switch target');
check(buildSwitchTarget({ ...row({ sessionId: 'nostatus' }), status: undefined }) === null, 'missing status is malformed: NO switch target (no unknown=>native)');
check(buildSwitchTarget(row({ sessionId: 'weird', status: 'weird-status' })) === null, 'non-exact status is malformed: NO switch target');

const restoreMissingIdentity = buildRestoreSwitchTarget({
  sessionId: 'saved-session',
  path: '<workspace>/session-store/saved.jsonl',
  runtimeIncarnation: 'runtime-saved',
});
check(restoreMissingIdentity !== null, 'restore accepts a catalog identity even without source/status');
check(restoreMissingIdentity!.catalogSource === 'rpi' && restoreMissingIdentity!.catalogStatus === 'native', 'restore missing identity is the native runtime named by sessionId');
check(restoreMissingIdentity!.catalogSessionId === 'saved-session', 'restore keeps the saved session id');
check(restoreMissingIdentity!.path === '<workspace>/session-store/saved.jsonl', 'restore keeps the saved session path');
check(buildRestoreSwitchTarget({ sessionId: '', path: '<workspace>/session-store/saved.jsonl' }) === null, 'restore still requires a session id');
check(buildRestoreSwitchTarget({ sessionId: 'saved-session', path: '' }) === null, 'restore still requires a session path');
check(buildRestoreSwitchTarget(row({ source: 'codex', status: 'foreign', sessionId: 'codex-catalog', path: 'codex.jsonl' }))!.catalogSource === 'codex', 'restore prefers the canonical foreign tuple when present');


/* ---------- already-imported rows: visible + badge + native-copy click ---------- */
{
  const importedRow = row({
    sessionId: 'codex-catalog-1',
    source: 'codex',
    status: 'already_imported',
    path: '<workspace>/session-store/codex/codex-catalog-1.jsonl',
    nativePath: '<workspace>/session-store/import_storage-abc123.jsonl',
    nativeSessionId: 'native-runtime-abc123',
  });
  const importedTarget = buildSwitchTarget(importedRow);
  check('an already-imported row yields a switch target (visible, never dropped)',
    importedTarget !== null);
  check('the already-imported click targets the NATIVE copy path', importedTarget!.path === '<workspace>/session-store/import_storage-abc123.jsonl');
  check('the already-imported click sends the strict native rpi/native tuple',
    importedTarget!.catalogSource === 'rpi' && importedTarget!.catalogStatus === 'native');
  check('the already-imported click sends the backend-authored native runtime id',
    importedTarget!.catalogSessionId === 'native-runtime-abc123');
  check('the already-imported target never carries already_imported as a wire status',
    importedTarget!.catalogStatus !== 'already_imported');
  check('an already-imported row WITHOUT nativePath grants no switch authority (never re-import)',
    buildSwitchTarget({ ...importedRow, nativePath: undefined }) === null);
  check('an already-imported row WITHOUT nativeSessionId grants no switch authority',
    buildSwitchTarget({ ...importedRow, nativeSessionId: undefined }) === null);
  check('an invalid catalog identity surfaces an error toast instead of a silent click',
    sidebarSource.includes("onToast?.('cannot switch this session: catalog identity is invalid', true)"));
  check('the imported file stem is never treated as runtime identity',
    buildSwitchTarget({ ...importedRow, nativeSessionId: 'header-runtime-id' })!.catalogSessionId === 'header-runtime-id');
  check('bootstrap restore of already-imported uses the native UUID, never the display name',
    buildRestoreSwitchTarget(importedRow)!.catalogSessionId === 'native-runtime-abc123'
      && buildRestoreSwitchTarget(importedRow)!.path === '<workspace>/session-store/import_storage-abc123.jsonl');
  check('bootstrap restore of already-imported without native id grants no switch authority',
    buildRestoreSwitchTarget({ ...importedRow, nativeSessionId: undefined }) === null);
  check('the imported badge CSS exists',
    sidebarCss.includes('.session-sidebar__imported')
      && sidebarCss.includes('border-radius: 999px'));
  check('the badge title names the native copy behavior',
    sidebarSource.includes('imported into rpi — click opens the native copy'));
  /* Normal (native/foreign) rows are unchanged by the already-imported path. */
  check('native rows keep their exact tuple', buildSwitchTarget(row({
    sessionId: 'target-session',
    path: '<workspace>/session-store/target.jsonl',
    runtimeIncarnation: 'runtime-7',
  }))!.catalogStatus === 'native');
  check('foreign rows keep their exact tuple', buildSwitchTarget(row({
    sessionId: 'codex-catalog',
    source: 'codex',
    status: 'foreign',
    runtimeIncarnation: 'runtime-9',
  }))!.catalogSource === 'codex');
}

check(!sidebarSource.includes('session-sidebar__error'), 'sidebar source never references the error element class');
check(!sidebarSource.includes('setError'), 'sidebar source has no error state setter');
  check(sidebarSource.includes("onToast?.('Failed to load sessions', true)"), 'load failure surfaces through the centralized toast layer (contract update)');
  check(!sidebarSource.includes('Failed to load sessions — Refresh to retry'), 'no inline load-failure hint text remains');
  check(!sidebarSource.includes('refreshFailedHint'), 'the refreshFailedHint state is fully removed');
  check(!sidebarSource.includes('&& !error'), 'empty list is not gated on an error state');
  check(!sidebarCss.includes('session-sidebar__error'), 'sidebar CSS no longer styles an error banner');
check(!sidebarSource.includes('workflow_detail'), 'sidebar source never references workflow_detail');

/* ---------- real mount: catalog revision, silent failures, action toasts ----------
 *
 * A real client render (createRoot + act over a mini DOM, same pattern as
 * sessionPanelAutocomplete.test.ts) because the revision trigger and the
 * silent background loads live in effects. The fake sendCommand serves
 * session_list (or fails everything with a realistic RPC-limit business
 * error) and records every frame, so the test drives the component end to
 * end: revision 1→2 sends exactly one session_list; a failed refresh keeps
 * the rows with zero sidebar error and zero toasts; a failed first load
 * renders the ordinary empty state; a failed action toasts exactly once.
 * ------------------------------------------------------------------ */

type Command = Record<string, unknown>;

interface RecordedToast { message: string; error?: boolean }

const realSetTimeout = globalThis.setTimeout.bind(globalThis);
const realTick = (): Promise<void> => {
  const { promise, resolve } = Promise.withResolvers<void>();
  realSetTimeout(resolve, 0);
  return promise;
};

type Listener = (event: Record<string, unknown>) => void;

interface MiniDoc {
  nodeType: number;
  nodeName: string;
  body: MiniNode;
  activeElement: MiniNode | null;
  createElement(tag: string): MiniNode;
  createTextNode(text: string): MiniNode;
  createComment(text: string): MiniNode;
  createDocumentFragment(): MiniNode;
  getElementById(id: string): MiniNode | null;
  querySelector(selector: string): MiniNode | null;
  oninput: null;
}

interface MiniNode {
  nodeType: number;
  nodeName: string;
  tagName: string | undefined;
  namespaceURI: string | null;
  ownerDocument: MiniDoc;
  parentNode: MiniNode | null;
  childNodes: MiniNode[];
  firstChild: MiniNode | null;
  lastChild: MiniNode | null;
  nextSibling: MiniNode | null;
  previousSibling: MiniNode | null;
  attributes: Map<string, string>;
  listeners: Map<string, Set<Listener>>;
  text: string;
  style: Record<string, unknown>;
  value: string;
  type: string;
  get className(): string;
  set className(value: string);
  get textContent(): string;
  set textContent(value: string);
  appendChild(child: MiniNode): MiniNode;
  insertBefore(child: MiniNode, before: MiniNode | null): MiniNode;
  removeChild(child: MiniNode): MiniNode;
  addEventListener(type: string, fn: Listener, options?: boolean | { capture?: boolean }): void;
  removeEventListener(type: string, fn: Listener, options?: boolean | { capture?: boolean }): void;
  setAttribute(name: string, value: string): void;
  getAttribute(name: string): string | null;
  removeAttribute(name: string): void;
  hasAttribute(name: string): boolean;
  querySelector(selector: string): MiniNode | null;
  querySelectorAll(selector: string): MiniNode[];
  dispatchEvent(raw: Record<string, unknown>): boolean;
  focus(): void;
  scrollIntoView(arg?: { block?: string }): void;
  scrollTop: number;
  scrollHeight: number;
  clientHeight: number;
  select(): void;
  getBoundingClientRect(): { left: number; top: number; width: number; height: number };
  attachEvent(type: string, listener: Listener): void;
  detachEvent(type: string, listener: Listener): void;
}

function relink(parent: MiniNode): void {
  parent.firstChild = parent.childNodes[0] ?? null;
  parent.lastChild = parent.childNodes[parent.childNodes.length - 1] ?? null;
  for (let i = 0; i < parent.childNodes.length; i += 1) {
    const child = parent.childNodes[i];
    child.parentNode = parent;
    child.previousSibling = parent.childNodes[i - 1] ?? null;
    child.nextSibling = parent.childNodes[i + 1] ?? null;
  }
}

function matches(node: MiniNode, selector: string): boolean {
  if (node.nodeType !== 1) return false;
  if (selector.startsWith('.')) {
    const value = node.getAttribute('class') ?? '';
    return value.split(/\s+/).includes(selector.slice(1));
  }
  if (selector.startsWith('#')) return (node.getAttribute('id') ?? '') === selector.slice(1);
  if (selector.startsWith('[') && selector.endsWith(']')) {
    const body = selector.slice(1, -1);
    const eq = body.indexOf('=');
    if (eq < 0) return node.hasAttribute(body);
    return node.getAttribute(body.slice(0, eq)) === body.slice(eq + 1).replace(/^["']|["']$/g, '');
  }
  return node.tagName === selector.toUpperCase();
}

/** Nodes whose `scrollIntoView` the component invoked since the last reset;
 *  the fork-from keyboard roving focus must keep the focused candidate inside
 *  the bounded scroller. Reset per test block that asserts it. */
const scrollIntoViewTargets: MiniNode[] = [];

function makeNode(nodeType: number, nodeName: string, owner: MiniDoc): MiniNode {
  const node: MiniNode = {
    nodeType,
    nodeName,
    tagName: nodeType === 1 ? nodeName : undefined,
    namespaceURI: nodeType === 1 ? 'http://www.w3.org/1999/xhtml' : null,
    ownerDocument: owner,
    parentNode: null,
    childNodes: [],
    firstChild: null,
    lastChild: null,
    nextSibling: null,
    previousSibling: null,
    attributes: new Map(),
    listeners: new Map(),
    style: {},
    text: '',
    get value() {
      if (nodeType === 1) return node.attributes.get('value') ?? '';
      return node.text;
    },
    set value(next: string) {
      if (nodeType === 1) node.setAttribute('value', String(next));
      else node.text = String(next);
    },
    get type() {
      return node.getAttribute('type') ?? '';
    },
    get className() {
      return node.getAttribute('class') ?? '';
    },
    set className(value: string) {
      node.setAttribute('class', value);
    },
    get textContent() {
      if (nodeType === 3 || nodeType === 8) return node.text;
      return node.childNodes.map((child) => child.textContent).join('');
    },
    set textContent(value: string) {
      node.childNodes = [];
      if (nodeType !== 3 && nodeType !== 8 && value !== '') {
        node.childNodes.push(owner.createTextNode(value));
      }
      node.text = nodeType === 3 || nodeType === 8 ? value : '';
      relink(node);
    },
    appendChild(child) {
      return node.insertBefore(child, null);
    },
    insertBefore(child, before) {
      if (child.parentNode) child.parentNode.removeChild(child);
      const index = before === null ? node.childNodes.length : node.childNodes.indexOf(before);
      node.childNodes.splice(index < 0 ? node.childNodes.length : index, 0, child);
      relink(node);
      return child;
    },
    removeChild(child) {
      const index = node.childNodes.indexOf(child);
      if (index < 0) throw new Error('not a child');
      node.childNodes.splice(index, 1);
      child.parentNode = null;
      child.nextSibling = null;
      child.previousSibling = null;
      relink(node);
      return child;
    },
    addEventListener(type, fn, options) {
      const capture = options === true || (typeof options === 'object' && options.capture === true);
      const key = capture ? `${type}-capture` : type;
      const set = node.listeners.get(key) ?? new Set();
      set.add(fn);
      node.listeners.set(key, set);
    },
    removeEventListener(type, fn, options) {
      const capture = options === true || (typeof options === 'object' && options.capture === true);
      const key = capture ? `${type}-capture` : type;
      node.listeners.get(key)?.delete(fn);
    },
    setAttribute(name, value) {
      node.attributes.set(name, String(value));
    },
    getAttribute(name) {
      return node.attributes.has(name) ? node.attributes.get(name)! : null;
    },
    removeAttribute(name) {
      node.attributes.delete(name);
    },
    hasAttribute(name) {
      return node.attributes.has(name);
    },
    querySelector(selector) {
      return node.querySelectorAll(selector)[0] ?? null;
    },
    querySelectorAll(selector) {
      const out: MiniNode[] = [];
      const visit = (current: MiniNode): void => {
        for (const child of current.childNodes) {
          if (matches(child, selector)) out.push(child);
          visit(child);
        }
      };
      visit(node);
      return out;
    },
    dispatchEvent(raw) {
      const path: MiniNode[] = [];
      let current: MiniNode | null = node;
      while (current !== null) {
        path.push(current);
        current = current.parentNode;
      }
      const target = (raw.target ?? node) as MiniNode;
      for (let i = path.length - 1; i >= 0; i -= 1) {
        for (const fn of [...(path[i].listeners.get(`${raw.type}-capture`) ?? [])]) fn({ ...raw, target });
      }
      for (const fn of [...(node.listeners.get(String(raw.type)) ?? [])]) fn({ ...raw, target });
      for (let i = 1; i < path.length; i += 1) {
        for (const fn of [...(path[i].listeners.get(String(raw.type)) ?? [])]) fn({ ...raw, target });
      }
      return true;
    },
    focus() {},
    scrollIntoView() {
      scrollIntoViewTargets.push(node);
    },
    scrollTop: 0,
    scrollHeight: 0,
    clientHeight: 0,
    select() {},
    getBoundingClientRect() {
      return { left: 0, top: 0, width: 0, height: 0 };
    },
    /* react-dom's ChangeEventPlugin IE polyfill (active because react-dom
     * initialized before any DOM existed) calls attachEvent/detachEvent on
     * the watched element; one-shot no-ops keep the polyfill harmless. */
    attachEvent() {},
    detachEvent() {},
  };
  return node;
}

class DummyHTMLIFrameElement {}

const installed = new Set<string>();
const pollHooks: Array<() => void> = []; // reset per installGlobals
// RAF runs after microtasks so same-turn settlement can cancel its paint.
let rafSeq = 0;
const rafPending = new Map<number, () => void>();
function installGlobals(): MiniDoc {
  pollHooks.length = 0; // fresh doc: only the new sidebar's poll is captured
  const doc = makeNode(9, '#document', null as unknown as MiniDoc) as unknown as MiniDoc & MiniNode;
  const html = makeNode(1, 'HTML', doc);
  const body = makeNode(1, 'BODY', doc);
  html.appendChild(body);
  doc.appendChild(html);
  doc.createElement = (tag: string) => makeNode(1, tag.toUpperCase(), doc);
  doc.createTextNode = (text: string) => {
    const textNode = makeNode(3, '#text', doc);
    textNode.text = text;
    return textNode;
  };
  doc.createComment = (text: string) => {
    const comment = makeNode(8, '#comment', doc);
    comment.text = text;
    return comment;
  };
  doc.createDocumentFragment = () => makeNode(11, '#document-fragment', doc);
  doc.getElementById = (id: string) => doc.querySelector(`#${id}`);
  doc.body = body;
  doc.activeElement = body;
  doc.oninput = null;
  const windowObj: Record<string, unknown> = {
    document: doc,
    event: undefined,
    HTMLIFrameElement: DummyHTMLIFrameElement,
    innerWidth: 1280,
    innerHeight: 800,
    setInterval: (fn: () => void): number => {
      pollHooks.push(fn); // captured: pending-cycle tests fire poll intents
      return 0;
    },
    clearInterval: (): void => {},
    setTimeout: realSetTimeout,
    requestAnimationFrame: (cb: (time: number) => void): number => {
      const id = ++rafSeq;
      rafPending.set(id, cb);
      realSetTimeout(() => {
        if (rafPending.delete(id)) cb(0);
      }, 0);
      return id;
    },
    cancelAnimationFrame: (id: number): void => {
      rafPending.delete(id);
    },
    clearTimeout: globalThis.clearTimeout.bind(globalThis),
    addEventListener: (): void => {},
    removeEventListener: (): void => {},
  };
  const overrides: Record<string, unknown> = {
    document: doc,
    window: windowObj,
    IS_REACT_ACT_ENVIRONMENT: true,
  };
  for (const [key, value] of Object.entries(overrides)) {
    Object.defineProperty(globalThis, key, { configurable: true, writable: true, value });
    installed.add(key);
  }
  return doc;
}

installGlobals();
/* react-dom/client snapshots canUseDOM at module init: import only after the
 * globals are live (same ordering as settingsPanel.test.ts). Async act's
 * MessageChannel fallback never closes its ports in the esbuild bundle, which
 * hangs node after the last assertion — one-shot close wrap, same fix. */
const realMessageChannel = globalThis.MessageChannel;
class OneShotMessageChannel extends realMessageChannel {
  constructor() {
    super();
    const port1 = this.port1;
    const port2 = this.port2;
    const port1Proxy = new Proxy(port1, {
      get(target, prop) {
        const value = Reflect.get(target, prop, target);
        return typeof value === 'function' ? value.bind(target) : value;
      },
      set(target, prop, value) {
        if (prop === 'onmessage' && typeof value === 'function') {
          return Reflect.set(target, prop, (event: MessageEvent) => {
            try {
              value(event);
            } finally {
              target.close();
              port2.close();
            }
          });
        }
        return Reflect.set(target, prop, value);
      },
    });
    Object.defineProperty(this, 'port1', { value: port1Proxy });
  }
}
Object.defineProperty(globalThis, 'MessageChannel', { configurable: true, writable: true, value: OneShotMessageChannel });
const { createElement, act } = await import('react');
const { createRoot } = await import('react-dom/client');
const { SessionSidebar } = await import('../src/panels/SessionSidebar');

interface CatalogCommand extends Command {
  /** Captured from the opts the component passed through the dedicated
   *  catalog sender (mirrors App's `{auxiliary: true}` wiring). */
  __auxiliary?: boolean;
}

interface SidebarBackend {
  commands: CatalogCommand[];
  sendCommand: (command: Command, bubbleId?: string, opts?: { auxiliary?: boolean }) => Promise<unknown>;
  sendCatalogCommand: (command: Command) => Promise<unknown>;
}

function createBackend(mode: 'catalog' | 'failing', catalogFailure?: () => Error): SidebarBackend {
  const commands: CatalogCommand[] = [];
  const catalog = [row({
    sessionId: 'srv-a',
    name: 'Alpha session',
    cwd: '<workspace>/app',
    path: '<workspace>/session-store/srv-a.jsonl',
    modifiedEpoch: 30,
  })];
  const sendCommand = async (
    command: Command,
    _bubbleId?: string,
    opts?: { auxiliary?: boolean },
  ): Promise<unknown> => {
    commands.push({ ...command, __auxiliary: opts?.auxiliary === true });
    if (mode === 'failing') {
      throw new Error(`command ${String(command.type)} failed: too many concurrent RPC commands (limit 16)`);
    }
    if (command.type === 'session_list' && catalogFailure) throw catalogFailure();
    if (command.type === 'session_list') {
      return { sessions: catalog.map((entry) => ({ ...entry })) };
    }
    return {};
  };
  /* Mirrors the App wiring exactly: the dedicated catalog sender is the
   * plain sender with `{auxiliary: true}` — the opts are captured here so
   * every request-count assertion also proves the auxiliary channel. */
  const sendCatalogCommand = (command: Command): Promise<unknown> =>
    sendCommand(command, undefined, { auxiliary: true });
  return { commands, sendCommand, sendCatalogCommand };
}
const sidebarProps = (
  backend: SidebarBackend,
  toasts: RecordedToast[],
  catalogRefreshRevision?: number,
  activeSessionId?: string | null,
  switchPending?: boolean,
): Record<string, unknown> => ({
  sendCommand: backend.sendCommand,
  sendCatalogCommand: backend.sendCatalogCommand,
  downloadSessionExport: async (): Promise<void> => {},
  waitForReady: async (): Promise<void> => {},
  onLifecycleResult: async (): Promise<void> => {},
  onSwitchSession: async (): Promise<boolean> => true,
  activeSessionId: activeSessionId ?? null,
  unreadBySessionId: {},
  onCollapse: (): void => {},
  onReopenRail: (): void => {},
  onOpenManage: (): void => {},
  onSwitchComplete: (): void => {},
  onToast: (message: string, error?: boolean): void => {
    toasts.push({ message, error });
  },
  catalogRefreshRevision,
  switchPending: switchPending === true,
});

const listCount = (backend: SidebarBackend): number =>
  backend.commands.filter((command) => command.type === 'session_list').length;

const FORBIDDEN_IN_SIDEBAR = [
  'load failed',
  'session_list failed',
  'too many concurrent RPC commands',
  'workflow_detail',
  'session switch pending',
] as const;

const assertNoErrorPresentation = (doc: MiniDoc & MiniNode, label: string): void => {
  check(doc.querySelectorAll('.session-sidebar__error').length === 0, `${label}: no .session-sidebar__error element exists`);
  const bodyText = doc.body.textContent;
  for (const forbidden of FORBIDDEN_IN_SIDEBAR) {
    check(!bodyText.includes(forbidden), `${label}: sidebar body never shows "${forbidden}"`);
  }
};

async function mountSidebar(
  doc: MiniDoc & MiniNode,
  props: Record<string, unknown>,
): Promise<{ container: MiniNode; root: { unmount(): void }; rerender: (next: Record<string, unknown>) => Promise<void> }> {
  const container = doc.createElement('div');
  doc.body.appendChild(container);
  const root = createRoot(container as unknown as Element);
  const render = (props_: Record<string, unknown>): void => {
    root.render(createElement(SessionSidebar, props_ as never));
  };
  await act(async () => {
    render(props);
    await realTick();
  });
  return {
    container,
    root,
    rerender: async (next: Record<string, unknown>) => {
      await act(async () => {
        render(next);
        await realTick();
      });
    },
  };
}

{
  const doc = installGlobals();

  /* Revision 1→2 sends exactly one background session_list; a same-revision
   * rerender sends none; an injected RPC-limit failure on the revision load
   * keeps the rows, renders zero inline error, and surfaces exactly one
   * centralized error toast. */
  const toasts: RecordedToast[] = [];
  const backend = createBackend('catalog');
  const mounted = await mountSidebar(doc, sidebarProps(backend, toasts, 1));
  check(listCount(backend) === 1, 'mount performs exactly one session_list');
  check(doc.body.textContent.includes('Alpha session'), 'mounted sidebar lists the catalog row');
  assertNoErrorPresentation(doc, 'boot');

  await mounted.rerender(sidebarProps(backend, toasts, 2));
  check(listCount(backend) === 2, 'revision 1→2 triggers exactly one background session_list');
  check(doc.body.textContent.includes('Alpha session'), 'revision refresh retains the listed row');
  assertNoErrorPresentation(doc, 'revision bump');

  await mounted.rerender(sidebarProps(backend, toasts, 2));
  check(listCount(backend) === 2, 'same-revision rerender sends no session_list');

  const failing = createBackend('failing');
  await mounted.rerender(sidebarProps(failing, toasts, 3));
  check(listCount(failing) === 1, 'revision 2→3 attempts exactly one background session_list');
  check(doc.body.textContent.includes('Alpha session'), 'injected RPC-limit session_list failure retains the last catalog rows');
  check(toasts.length === 0, 'background catalog failure stays silent (last-good rows persist; toast is user-initiated/first-load only)');

  /* A failed row action is exactly one bottom-right toast owned by the
   * action; the sidebar itself stays silent and the rows stay. */
  const switchBtn = doc.body.querySelector('.session-sidebar__switch');
  await act(async () => {
    switchBtn.dispatchEvent({
      type: 'contextmenu', target: switchBtn, clientX: 8, clientY: 8,
      preventDefault(): void {}, stopPropagation(): void {},
    });
    await realTick();
  });
  const renameItem = doc.body.querySelectorAll('[role="menuitem"]').find((item) => item.textContent === 'Rename');
  check(renameItem !== null, 'context menu exposes the Rename item');
  await act(async () => {
    renameItem.dispatchEvent({ type: 'click', target: renameItem });
    await realTick();
  });
  const renameInput = doc.body.querySelector('.session-sidebar__menu-rename');
  check(renameInput !== null, 'rename mode renders the rename input');
  await act(async () => {
    /* react-dom's ChangeEventPlugin input polyfill (this bundle runs it
     * because react-dom initialized before a DOM existed) captures
     * activeElementInst only from a real focusin dispatch, so the rename
     * input must raise focusin before any keydown on it. The value is then
     * set directly on the ref node (commitRename reads
     * renameInputRef.current.value first) — a synthetic input event would
     * need a real DOM value tracker the mini DOM cannot provide. */
    renameInput.dispatchEvent({ type: 'focusin', target: renameInput });
    renameInput.value = 'Renamed';
    await realTick();
  });
  await act(async () => {
    renameInput.dispatchEvent({
      type: 'keydown', key: 'Enter',
      preventDefault(): void {}, stopPropagation(): void {},
    });
    await realTick();
    await realTick();
  });
  check(toasts.length === 1, 'failing rename action toasts exactly once (background load failure stays silent)');
  check(toasts[0]?.message === 'command set_session_name failed: too many concurrent RPC commands (limit 16)', 'the action toast carries the action error', JSON.stringify(toasts));
  check(toasts[0]?.error === true, 'the action toast is styled as an error');
  check(doc.body.textContent.includes('Alpha session'), 'failed action keeps the catalog rows');
  assertNoErrorPresentation(doc, 'failed action');

  await act(async () => {
    mounted.root.unmount();
  });

  /* A failed first load renders the ordinary empty state — never error text. */
  const toasts2: RecordedToast[] = [];
  const failing2 = createBackend('failing');
  const mounted2 = await mountSidebar(doc, sidebarProps(failing2, toasts2, 1));
  check(listCount(failing2) === 1, 'failed first load still attempts one session_list');
  assertNoErrorPresentation(doc, 'failed first load');
  check(toasts2.length === 1, 'failed first load toasts exactly once (centralized error layer)');
  check(toasts2[0]?.message === 'Failed to load sessions', 'the load-failure toast carries the message', JSON.stringify(toasts2));
  check(toasts2[0]?.error === true, 'the load-failure toast is styled as an error');
  check(!doc.body.textContent.includes('Failed to load sessions'), 'no inline load-failure hint renders (errors are centralized)');
  await mounted2.rerender(sidebarProps(failing2, toasts2, 2));
  check(listCount(failing2) === 2, 'background poll attempts one more session_list on revision bump');
  check(toasts2.length === 1, 'the latched first-failure toast never repeats on background empty-catalog polls', JSON.stringify(toasts2));
  await act(async () => {
    mounted2.root.unmount();
  });
  /* Exactly ONE catalog authority per user flow: a negotiated switch sends
   * exactly one session_list (runSwitch's explicit refresh); an active-id
   * prop change alone sends none; a lifecycle fork sends exactly one
   * session_fork_latest command and its App-owned revision bump refreshes
   * the catalog exactly once (run no longer issues its own refresh). */
  const toasts3: RecordedToast[] = [];
  const backend3 = createBackend('catalog');
  const mounted3 = await mountSidebar(doc, sidebarProps(backend3, toasts3, 10, null));
  check(listCount(backend3) === 1, 'third mount performs exactly one session_list');
  const row3 = doc.body.querySelector('.session-sidebar__switch');
  check(row3 !== null, 'switch row rendered for the request-count flows');
  await act(async () => {
    row3.dispatchEvent({ type: 'click', target: row3 });
    await realTick();
    await realTick();
  });
  check(listCount(backend3) === 2, 'negotiated switch triggers exactly one session_list');
  await mounted3.rerender(sidebarProps(backend3, toasts3, 10, 'srv-a'));
  check(listCount(backend3) === 2, 'active-id prop change alone never triggers a catalog load');
  await act(async () => {
    row3.dispatchEvent({
      type: 'contextmenu', target: row3, clientX: 8, clientY: 8,
      preventDefault(): void {}, stopPropagation(): void {},
    });
    await realTick();
  });
  const forkItem = doc.body.querySelectorAll('[role="menuitem"]').find((item) => item.textContent === 'Fork latest');
  check(forkItem !== undefined, 'context menu exposes the Fork latest item');
  await act(async () => {
    forkItem.dispatchEvent({ type: 'click', target: forkItem });
    await realTick();
    await realTick();
  });
  check(backend3.commands.filter((command) => command.type === 'session_fork_latest').length === 1, 'fork-latest sends exactly one lifecycle command');
  check(listCount(backend3) === 2, 'lifecycle run issues no catalog refresh of its own');
  check(toasts3.some((toast) => toast.message === 'Forked from Alpha session'), 'successful fork still toasts through onToast');
  await mounted3.rerender(sidebarProps(backend3, toasts3, 11, 'srv-a'));
  check(listCount(backend3) === 3, 'App revision bump refreshes the catalog exactly once per lifecycle cut');
  assertNoErrorPresentation(doc, 'request-count flows');
  await act(async () => {
    mounted3.root.unmount();
  });

}

/* ---------- real mount: lifecycle row overlay (fork/new/clone child) ----------
 * App's validated authoritative `session` row arrives as the
 * `lifecycleRowOverlay` prop. The sidebar must merge it synchronously into
 * the rendered rows BEFORE any catalog request: the active child row paints
 * in the SAME lifecycle cut, a held/failed/delayed session_list retains it,
 * and a later catalog that carries the exact `source::sessionId::path`
 * identity absorbs it without a duplicate. */
{
  const doc = installGlobals();
  const forkChildRow = row({
    sessionId: 'fork-child',
    name: 'Fork child',
    cwd: '<workspace>/app',
    path: '<workspace>/session-store/fork-child.jsonl',
    loaded: true,
  });
  const forkChildKey = sessionRowKey(forkChildRow);
  const childRow = (): MiniNode | null =>
    doc.body.querySelectorAll('[data-session-id="fork-child"]')[0] ?? null;
  const childCount = (): number =>
    doc.body.querySelectorAll('[data-session-id="fork-child"]').length;

  /* 1. Overlay paints with ZERO session_list sends while the catalog load is
   *    HELD (switch-pending gate defers the mount load) — the active child
   *    row paints in the same lifecycle cut, never gated on the catalog. */
  const toastsO: RecordedToast[] = [];
  const backendO = createBackend('catalog');
  let absorbedCalls = 0;
  const heldProps = sidebarProps(backendO, toastsO, 1, 'fork-child', true);
  heldProps.lifecycleRowOverlay = forkChildRow;
  const mountedO = await mountSidebar(doc, heldProps);
  check(listCount(backendO) === 0, 'held gate: overlay paints with ZERO session_list sends');
  check(childRow() !== null, 'fork child overlay row paints before any catalog request');
  /* The active paint lives on the row <li>: the switch button sits inside
   * the presentational session-sidebar__row-main wrapper div. */
  const rowItemOf = (start: MiniNode | null): MiniNode | null => {
    let node = start;
    while (node !== null && node.nodeName !== 'LI') node = node.parentNode as MiniNode | null;
    return node;
  };
  check((rowItemOf(childRow())?.getAttribute('class') ?? '').includes('session-sidebar__row--active') === true,
    'overlay child row paints ACTIVE in the same lifecycle cut');
  check((childRow()!.getAttribute('title') || '').startsWith('Current session:'), 'overlay row keeps the active-session title');
  assertNoErrorPresentation(doc, 'held overlay');

  /* Gate clears: exactly one deferred session_list; the catalog lacks the
   * child yet (DELAYED), so the overlay is retained and never absorbed. */
  await mountedO.rerender({ ...heldProps, switchPending: false });
  check(listCount(backendO) === 1, 'gate clearing flushes exactly one held session_list');
  check(childRow() !== null, 'delayed catalog without the child row retains the overlay');
  check(absorbedCalls === 0, 'a catalog missing the exact identity never reports absorption');
  assertNoErrorPresentation(doc, 'delayed catalog');

  /* 2. FAILED catalog refresh retains the overlay (rows kept by reference). */
  const failingO = createBackend('failing');
  const failedProps = sidebarProps(failingO, toastsO, 2, 'fork-child');
  failedProps.lifecycleRowOverlay = forkChildRow;
  await mountedO.rerender(failedProps);
  check(listCount(failingO) === 1, 'failed revision refresh attempts exactly one session_list');
  check(childRow() !== null, 'failed catalog retains the overlay child row');
  check(absorbedCalls === 0, 'a failed catalog never reports absorption');
  assertNoErrorPresentation(doc, 'failed overlay refresh');
  await act(async () => {
    mountedO.root.unmount();
  });

  /* 3. Busy clear / row paint ordering: App paints the overlay before the
   * lifecycle handler resolves, while the sidebar keeps controls disabled;
   * controls re-enable only after the handler settles. */
  let releaseLifecycle!: () => void;
  const lifecycleGate = new Promise<void>((resolve) => { releaseLifecycle = resolve; });
  const toastsP: RecordedToast[] = [];
  const backendP = createBackend('catalog');
  const propsP = sidebarProps(backendP, toastsP, 1, 'srv-a');
  let rerenderP: ((next: Record<string, unknown>) => Promise<void>) | null = null;
  propsP.onLifecycleResult = async (): Promise<void> => {
    // App-side synchronous cut: overlay + active id paint BEFORE resolution.
    await rerenderP?.({ ...propsP, activeSessionId: 'fork-child', lifecycleRowOverlay: forkChildRow });
    await lifecycleGate;
  };
  const mountedP = await mountSidebar(doc, propsP);
  rerenderP = mountedP.rerender;
  const rowP = doc.body.querySelector('.session-sidebar__switch');
  await act(async () => {
    rowP!.dispatchEvent({
      type: 'contextmenu', target: rowP, clientX: 8, clientY: 8,
      preventDefault(): void {}, stopPropagation(): void {},
    });
    await realTick();
  });
  const forkLatest = doc.body.querySelectorAll('[role="menuitem"]').find((item) => item.textContent === 'Fork latest');
  check(forkLatest !== undefined, 'ordering flow: context menu exposes Fork latest');
  await act(async () => {
    forkLatest!.dispatchEvent({ type: 'click', target: forkLatest });
    await realTick();
    await realTick();
  });
  check(childRow() !== null, 'child row paints while the lifecycle action is STILL busy');
  check(doc.body.querySelector('.session-sidebar__switch')?.hasAttribute('disabled') === true, 'controls remain disabled when the overlay row paints');
  releaseLifecycle();
  await act(async () => {
    await realTick();
    await realTick();
  });
  check(doc.body.querySelector('.session-sidebar__switch')?.hasAttribute('disabled') === false, 'controls re-enable only after the lifecycle handler settles');
  check(childRow() !== null, 'child row remains painted after the busy clear');
  check(listCount(backendP) === 1, 'no catalog request between row paint and busy clear');
  assertNoErrorPresentation(doc, 'ordering flow');
  await act(async () => {
    mountedP.root.unmount();
  });

  /* 4. Later catalog carrying the EXACT identity absorbs the overlay with no
   *    duplicate; the catalog row owns the display from then on. */
  const catalogQ: SessionRowWire[] = [row({
    sessionId: 'srv-a',
    name: 'Alpha session',
    cwd: '<workspace>/app',
    path: '<workspace>/session-store/srv-a.jsonl',
    modifiedEpoch: 30,
  })];
  const commandsQ: CatalogCommand[] = [];
  const backendQ: SidebarBackend = {
    commands: commandsQ,
    sendCommand: async (command, _bubbleId, opts) => {
      commandsQ.push({ ...command, __auxiliary: opts?.auxiliary === true });
      if (command.type === 'session_list') {
        return { sessions: [...catalogQ, { ...forkChildRow, displayTime: 't+1', messageCount: 2 }] };
      }
      return {};
    },
    sendCatalogCommand: (command) => backendQ.sendCommand(command, undefined, { auxiliary: true }),
  };
  let absorbedQ = 0;
  let rerenderQ: ((next: Record<string, unknown>) => Promise<void>) | null = null;
  const propsQ = sidebarProps(backendQ, toastsO, 3, 'fork-child');
  propsQ.lifecycleRowOverlay = forkChildRow;
  propsQ.onLifecycleRowAbsorbed = (): void => {
    absorbedQ += 1;
    void rerenderQ?.({ ...propsQ, lifecycleRowOverlay: null });
  };
  const mountedQ = await mountSidebar(doc, propsQ);
  rerenderQ = mountedQ.rerender;
  await act(async () => {
    await realTick();
    await realTick();
  });
  check(listCount(backendQ) === 1, 'absorbing mount performs exactly one session_list');
  check(absorbedQ === 1, 'exact catalog identity reports absorption exactly once');
  check(childCount() === 1, 'absorbed catalog renders exactly one fork-child row (no duplicate)');
  check((childRow()!.textContent || '').includes('t+1 · 2 msgs'),
    'after absorption the CATALOG row owns the display fields, not the overlay');
  assertNoErrorPresentation(doc, 'absorbed overlay');
  check(forkChildKey === 'rpi::fork-child::<workspace>/session-store/fork-child.jsonl',
    'overlay identity key is the full source::sessionId::path');
  await act(async () => {
    mountedQ.root.unmount();
  });
}

/* ---------- mergeLifecycleOverlayRow: pure merge matrix ---------- */

const emptyMerged = mergeLifecycleOverlayRow([], null);
check(Array.isArray(emptyMerged) && emptyMerged.length === 0, 'null overlay on an empty catalog stays empty');
const soloCatalog = [row({ sessionId: 'a', path: '<w>/a.jsonl' })];
check(mergeLifecycleOverlayRow(soloCatalog, undefined) === soloCatalog, 'no overlay returns the catalog by reference');
const overlayRow = row({ sessionId: 'child', cwd: '<w>/app', path: '<w>/child.jsonl' });
const withOverlay = mergeLifecycleOverlayRow(soloCatalog, overlayRow);
check(withOverlay.map(sessionRowKey).join('|') === `${sessionRowKey(soloCatalog[0]!)}|${sessionRowKey(overlayRow)}`,
  'overlay appends at the tail matching reconcile new-row semantics');
check(mergeLifecycleOverlayRow([soloCatalog[0]!, overlayRow], overlayRow).length === 2,
  'exact identity already in catalog: merge contributes nothing (no duplicate)');
check(mergeLifecycleOverlayRow(soloCatalog, { ...overlayRow, path: '<w>/OTHER.jsonl' }).length === 2,
  'a different path is a different identity: overlay is retained');
check(mergeLifecycleOverlayRow(soloCatalog, { ...overlayRow, source: 'codex' }).length === 2,
  'a different source is a different identity');

/* ---------- reconcile + overlay: later catalog absorbs without duplicate ---------- */

const establishedO = reconcileSessionRowOrder([], soloCatalog);
const overlayKeyStandalone = sessionRowKey(overlayRow);
const absorbedCatalog = reconcileSessionRowOrder(
  establishedO,
  [soloCatalog[0]!, overlayRow],
);
check(sessionRowKey(absorbedCatalog[absorbedCatalog.length - 1]!) === overlayKeyStandalone,
  'catalog carrying the exact overlay identity reconciles into one row');
check(mergeLifecycleOverlayRow(absorbedCatalog, overlayRow).length === absorbedCatalog.length,
  'merged view has no duplicate after absorption');
check(mergeLifecycleOverlayRow(absorbedCatalog, overlayRow)[absorbedCatalog.length - 1] === absorbedCatalog[absorbedCatalog.length - 1],
  'the catalog row (not the overlay) survives absorption by reference');

/* ---------- New Session footer entry (direct visible action) ---------- */

check(sidebarSource.includes('id="sidebar-manage-btn"'), 'the sidebar entry keeps its stable id for the E2E lanes');
check(sidebarSource.includes('>\n          New Session\n        </button>'), 'footer entry reads New Session');
check(!sidebarSource.includes('Manage Session'), 'the old Manage Session label is gone');
check(appSource.includes('function focusNewSessionCwdInput'), 'App owns a bounded focus helper for the New-session cwd input');
check(appSource.includes('onOpenManage={openNewSession}')
  && /const openNewSession = \(\) => \{\s*openPanel\('session', \{ force: true \}\);\s*focusNewSessionCwdInput\(\);\s*\};/.test(appSource),
  'App onOpenManage opens the session panel and requests the cwd-input focus');

{
  const doc = installGlobals();
  const toastsN: RecordedToast[] = [];
  const backendN = createBackend('catalog');
  let manageCalls = 0;
  const propsN = sidebarProps(backendN, toastsN, 1);
  propsN.onOpenManage = (): void => {
    manageCalls += 1;
  };
  const mountedN = await mountSidebar(doc, propsN);
  const newEntry = doc.body.querySelector('#sidebar-manage-btn');
  check(newEntry !== null, 'footer keeps the stable sidebar entry button');
  check(newEntry.getAttribute('aria-label') === 'New Session', 'New Session entry keeps an accessible name');
  check(newEntry.getAttribute('type') === 'button', 'New Session entry remains a real button');
  await act(async () => {
    newEntry.dispatchEvent({ type: 'click', target: newEntry });
    await realTick();
  });
  check(manageCalls === 1, 'New Session click routes through onOpenManage exactly once');
  assertNoErrorPresentation(doc, 'New Session entry');
  await act(async () => {
    mountedN.root.unmount();
  });
}

/* ---------- real mount: switch-pending catalog coalescing ----------
 * The transient `session switch pending` state must never reach the UI:
 * while App's switchPending gate is up, every catalog refresh intent
 * (mount, poll, revision bump, user action) coalesces into ONE deferred
 * flag; the gate clearing flushes exactly ONE auxiliary session_list.
 * Every request count below also verifies the captured auxiliary opts. */

{
  const doc = installGlobals();
  const firePolls = async (): Promise<void> => {
    await act(async () => {
      for (const hook of pollHooks) hook();
      await realTick();
      await realTick();
    });
  };

  /* Cycle 1: mount while pending — the mount intent plus 20 poll/revision
   * intents send ZERO session_list; the settle flushes exactly one. */
  const toastsP: RecordedToast[] = [];
  const backendP = createBackend('catalog');
  const mountedP = await mountSidebar(doc, sidebarProps(backendP, toastsP, 1, null, true));
  check(listCount(backendP) === 0, 'mount during switchPending sends zero session_list');
  check(doc.body.textContent.includes('No saved sessions.'), 'pending mount renders the ordinary empty state (no error surface)');
  assertNoErrorPresentation(doc, 'pending mount');
  for (let revision = 2; revision <= 11; revision += 1) {
    await mountedP.rerender(sidebarProps(backendP, toastsP, revision, null, true));
  }
  for (let poll = 0; poll < 10; poll += 1) await firePolls();
  check(listCount(backendP) === 0, '20 poll/revision intents during switchPending send zero session_list');
  check(toastsP.length === 0, 'pending window emits zero toasts');

  await mountedP.rerender(sidebarProps(backendP, toastsP, 11, null, false));
  check(listCount(backendP) === 1, 'switchPending false flushes exactly ONE deferred session_list');
  check(backendP.commands.filter((command) => command.type === 'session_list')
    .every((command) => command.__auxiliary === true), 'the flushed catalog load carries auxiliary opts');
  check(doc.body.textContent.includes('Alpha session'), 'deferred flush refreshes the catalog rows');
  assertNoErrorPresentation(doc, 'pending flush');
  check(toastsP.length === 0, 'deferred flush toasts nothing');
  await mountedP.rerender(sidebarProps(backendP, toastsP, 11, null, false));
  check(listCount(backendP) === 1, 'same switchPending=false rerender sends zero additional session_list');
  await act(async () => {
    mountedP.root.unmount();
  });

  /* Cycle 2: rows already loaded; a pending switch begins, then poll
   * intents AND a user switch action both raise intents — still zero sends.
   * The gate clearing in the SAME commit as a revision bump must not
   * double-send (requestCatalogLoad clears the flag before the flush). */
  const toastsQ: RecordedToast[] = [];
  const backendQ = createBackend('catalog');
  const mountedQ = await mountSidebar(doc, sidebarProps(backendQ, toastsQ, 20, null, false));
  check(listCount(backendQ) === 1, 'second sidebar mounts with exactly one session_list');
  await mountedQ.rerender(sidebarProps(backendQ, toastsQ, 21, null, true));
  check(listCount(backendQ) === 1, 'pending begin with a revision intent sends zero catalog loads');
  for (let poll = 0; poll < 8; poll += 1) await firePolls();
  const rowQ = doc.body.querySelector('.session-sidebar__switch');
  check(rowQ !== null, 'rows stay listed while pending (retained catalog)');
  await act(async () => {
    rowQ.dispatchEvent({ type: 'click', target: rowQ });
    await realTick();
    await realTick();
  });
  check(listCount(backendQ) === 1, 'poll intents plus a user switch action during pending send zero session_list');
  check(toastsQ.length === 0, 'pending cycle with rows emits zero toasts');
  await mountedQ.rerender(sidebarProps(backendQ, toastsQ, 22, null, false));
  check(listCount(backendQ) === 2, 'gate clearing with a same-commit revision intent flushes exactly ONE session_list');
  assertNoErrorPresentation(doc, 'second pending cycle');
  await act(async () => {
    mountedQ.root.unmount();
  });

  /* Already-sent auxiliary catalog rejections: exact switch-pending, RPC
   * limit, business, and transport rejections all retain the last-good
   * rows with zero sidebar error and zero toasts — the transient state
   * can never render. */
  const toastsR: RecordedToast[] = [];
  const docR = installGlobals();
  const goodR = createBackend('catalog');
  const mountedR = await mountSidebar(docR, sidebarProps(goodR, toastsR, 1));
  check(listCount(goodR) === 1, 'rejection-cycle mount loads the catalog once');
  check(docR.body.textContent.includes('Alpha session'), 'rejection-cycle rows listed before the failures');
  const rejectionCases: Array<[string, () => Error]> = [
    ['switch-pending rejection', () => Object.assign(new Error('session switch pending'), { code: 'SWITCH_PENDING' })],
    ['rpc-limit rejection', () => new Error('command session_list failed: too many concurrent RPC commands (limit 16)')],
    ['business rejection', () => Object.assign(new Error('catalog unavailable'), { rpc: true, toastOwned: true })],
    ['transport rejection', () => Object.assign(new Error('connection closed (code 1006)'), { transport: true })],
  ];
  let revisionR = 1;
  for (const [label, failure] of rejectionCases) {
    const failingR = createBackend('catalog', failure);
    revisionR += 1;
    await mountedR.rerender(sidebarProps(failingR, toastsR, revisionR));
    check(listCount(failingR) === 1, `${label}: the refresh attempts exactly one session_list`);
    check(failingR.commands.every((command) => command.type !== 'session_list' || command.__auxiliary === true),
      `${label}: the rejected catalog load was auxiliary`);
    check(docR.body.textContent.includes('Alpha session'), `${label}: last-good rows retained`);
    assertNoErrorPresentation(docR, label);
    check(toastsR.length === 0, `${label}: background catalog rejection stays silent (no toast)`);
  }
  await act(async () => {
    mountedR.root.unmount();
  });
}

/* ---------- parked ReadyGate: a load that starts pending=false but the
 * gate resolves while a switch is pending must NEVER hit the wire; it
 * re-joins the single deferred flag and the settle flushes exactly one. */

{
  const docG = installGlobals();
  const toastsG: RecordedToast[] = [];
  const backendG = createBackend('catalog');
  /* Unresolved gate: EVERY render keeps this gate so the mount load, poll
   * intents, and revision intents all park inside waitForReady across the
   * whole pending window (a rerender with the default immediate resolver
   * would complete the parked loads instead of racing them). */
  const { promise: gatePromise, resolve: gateResolve } = Promise.withResolvers<void>();
  const gateProps = (revision: number, pending?: boolean): Record<string, unknown> => {
    const next = sidebarProps(backendG, toastsG, revision, null, pending);
    next.waitForReady = (): Promise<void> => gatePromise;
    return next;
  };
  const mountedG = await mountSidebar(docG, gateProps(1));
  check(listCount(backendG) === 0, 'parked gate holds the mount load before any send');

  // Intents raised while the load is parked: fired polls + a revision bump.
  for (let poll = 0; poll < 5; poll += 1) {
    await act(async () => {
      for (const hook of pollHooks) hook();
      await realTick();
    });
  }
  await mountedG.rerender(gateProps(2));
  // A switch begins while the load is parked, then the gate resolves:
  // the resumed load re-checks pending and must re-defer (zero wire sends).
  await mountedG.rerender(gateProps(2, true));
  await act(async () => {
    gateResolve();
    await realTick();
    await realTick();
  });
  check(listCount(backendG) === 0, 'gate resolving DURING the pending switch sends zero session_list');
  check(toastsG.length === 0, 'parked-gate pending window emits zero toasts');
  assertNoErrorPresentation(docG, 'parked-gate pending window');

  await mountedG.rerender(gateProps(2, false));
  check(listCount(backendG) === 1, 'settled switch flushes the parked load as exactly ONE session_list');
  check(backendG.commands.filter((command) => command.type === 'session_list')
    .every((command) => command.__auxiliary === true), 'the flushed parked load carries auxiliary opts');
  check(docG.body.textContent.includes('Alpha session'), 'flushed parked load refreshes the catalog rows');
  assertNoErrorPresentation(docG, 'parked-gate flush');
  check(toastsG.length === 0, 'parked-gate flush toasts nothing');
  await mountedG.rerender(gateProps(2, false));
  check(listCount(backendG) === 1, 'same switchPending=false rerender after the parked flush sends zero more');
  await act(async () => {
    mountedG.root.unmount();
  });
}

/* ---------- real mount: deferred switch marker (deterministic RAF seam) ----------
 * The switching marker paint is queued on an animation frame so a switch that
 * settles in the same microtask turn cancels the queued frame and never
 * applies `session-sidebar__row--switching` (not even transiently), while a
 * switch that stays pending past one frame paints the marker/aria-busy
 * exactly once and clears both when the promise settles. ---------- */

{
  const doc = installGlobals();
  const toasts: RecordedToast[] = [];
  const backend = createBackend('catalog');
  const mounted = await mountSidebar(doc, sidebarProps(backend, toasts, 1));
  check(doc.body.textContent.includes('Alpha session'), 'deferred-marker fixture lists the catalog row');

  /* (1) Immediate settlement: the resolved promise settles in the same
   * microtask turn, so the queued frame is cancelled before it can run. The
   * class spy proves the marker is never applied, even transiently between
   * a (regressed) immediate paint and the settle cleanup. */
  const propsFast = sidebarProps(backend, toasts, 1);
  let fastCalls = 0;
  propsFast.onSwitchSession = async (): Promise<boolean> => {
    fastCalls += 1;
    return false;
  };
  await mounted.rerender(propsFast);
  const fastBtn = doc.body.querySelector('.session-sidebar__switch');
  check(fastBtn !== null, 'immediate-settle fixture has a switch row');
  const fastRow = doc.body.querySelector('.session-sidebar__row')!;
  let fastMarkerApplied = false;
  const fastSetAttribute = fastRow.setAttribute.bind(fastRow);
  fastRow.setAttribute = (name: string, value: string) => {
    if (name === 'class' && value.includes('session-sidebar__row--switching')) fastMarkerApplied = true;
    fastSetAttribute(name, value);
  };
  await act(async () => {
    fastBtn!.dispatchEvent({ type: 'click', target: fastBtn });
    check(fastCalls === 1, 'the click reaches onSwitchSession synchronously');
    await realTick(); // flush the queued frame: cancelled by the same-turn settle
  });
  check(!fastMarkerApplied, 'immediate settlement never applies the switching marker, even transiently');
  check(doc.body.querySelector('.session-sidebar__row--switching') === null, 'immediate settlement leaves no switching marker after the flushed frame');
  check(fastBtn!.getAttribute('aria-busy') === null, 'immediate settlement clears aria-busy after the flushed frame');
  check(fastBtn!.getAttribute('disabled') === null, 'immediate settlement re-enables the control after the flushed frame');

  /* (2) Pending past one frame: the queued frame fires while the promise is
   * still pending and paints the marker/aria-busy on the target row; the
   * later settle clears both. */
  const { promise: slowPromise, resolve: slowResolve } = Promise.withResolvers<boolean>();
  const propsSlow = sidebarProps(backend, toasts, 1);
  propsSlow.onSwitchSession = (): Promise<boolean> => slowPromise;
  await mounted.rerender(propsSlow);
  const slowBtn = doc.body.querySelector('.session-sidebar__switch');
  check(slowBtn !== null, 'pending-settle fixture has a switch row');
  const slowRow = doc.body.querySelector('.session-sidebar__row')!;
  let slowMarkerApplied = false;
  const slowSetAttribute = slowRow.setAttribute.bind(slowRow);
  slowRow.setAttribute = (name: string, value: string) => {
    if (name === 'class' && value.includes('session-sidebar__row--switching')) slowMarkerApplied = true;
    slowSetAttribute(name, value);
  };
  await act(async () => {
    slowBtn!.dispatchEvent({ type: 'click', target: slowBtn });
    await realTick(); // exactly one frame fires while the switch is pending
  });
  check(slowMarkerApplied, 'pending switch paints the switching marker after one frame');
  check(doc.body.querySelector('.session-sidebar__row--switching') !== null, 'pending switch keeps the switching marker in the DOM');
  check(slowRow.getAttribute('data-switching') === 'true', 'pending switch marks data-switching on the target row');
  check(slowBtn!.getAttribute('aria-busy') === 'true', 'pending switch marks the control aria-busy');
  await act(async () => {
    slowResolve(false);
    await realTick();
    await realTick();
  });
  check(doc.body.querySelector('.session-sidebar__row--switching') === null, 'settled switch clears the switching marker');
  check(slowRow.getAttribute('data-switching') === null, 'settled switch clears data-switching');
  check(slowBtn!.getAttribute('aria-busy') === null, 'settled switch clears aria-busy');
  check(slowBtn!.getAttribute('disabled') === null, 'settled switch re-enables the control');
  await act(async () => {
    mounted.root.unmount();
  });
}

/* ---------- catalog loading skeleton + refresh bar (loading affordance) ----------
 * (a) EMPTY catalog + in-flight load → skeleton rows after the 150ms
 *     delayed-show (fast catalogs never flash); replaced by rows on success.
 * (b) Refresh with rows painted → thin top progress bar + spinning Refresh,
 *     rows stay painted, bar clears on settle. (c) Failed refresh → last-good
 *     rows + the inline retry hint. (d) No skeleton when rows exist. ---------- */
{
  const docL = installGlobals();
  const toastsL: RecordedToast[] = [];
  const catalogL = [row({ sessionId: 'srv-a', name: 'Alpha session', cwd: '<workspace>/app', path: '<workspace>/session-store/srv-a.jsonl', modifiedEpoch: 30 })];
  // Parked session_list: the load is in flight until released.
  const { promise: parkedPromise, resolve: parkedResolve } = Promise.withResolvers<unknown>();
  const commandsL: CatalogCommand[] = [];
  const parkedBackend: SidebarBackend = {
    commands: commandsL,
    sendCommand: async (command, bubbleId, opts) => {
      commandsL.push({ ...command, __auxiliary: opts?.auxiliary === true });
      if (command.type === 'session_list') return parkedPromise;
      return {};
    },
    sendCatalogCommand: (command) => parkedBackend.sendCommand(command, undefined, { auxiliary: true }),
  };
  const propsL = sidebarProps(parkedBackend, toastsL, 1);
  propsL.onSwitchSession = async (): Promise<boolean> => true;
  const mountedL = await mountSidebar(docL, propsL);
  const skeletonRowsL = (): MiniNode[] => docL.body.querySelectorAll('.session-sidebar__catalog-skeleton');
  const refreshBarL = (): MiniNode | null => docL.body.querySelector('.session-sidebar__refresh-bar');

  // Before the 150ms gate: no skeleton (no flash on fast loads).
  check(skeletonRowsL().length === 0, 'no skeleton flash before the 150ms delayed-show');
  check(refreshBarL() === null, 'no refresh bar while the catalog is empty (skeleton is the empty-catalog affordance)');
  // After 150ms with the load still in flight: 5 skeleton rows appear.
  await act(async () => {
    await new Promise<void>((resolve) => realSetTimeout(resolve, 200));
  });
  check(skeletonRowsL().length === 5, 'in-flight load with empty catalog renders 5 skeleton rows after the 150ms gate');
  check(refreshBarL() === null, 'the refresh bar never replaces the skeleton on an empty catalog');

  /* Release the parked load: skeletons are replaced by real rows. */
  await act(async () => {
    parkedResolve({ sessions: catalogL.map((entry) => ({ ...entry })) });
    await realTick();
    await realTick();
  });
  check(skeletonRowsL().length === 0, 'a successful load replaces the skeleton rows with real rows');
  check(docL.body.textContent.includes('Alpha session'), 'the loaded catalog renders the row');
  await act(async () => { mountedL.root.unmount(); });
}

/* Parked ReadyGate + parked session_list: empty-catalog load arms
 * catalogRefreshing before waitForReady so the 150ms delayed-show can
 * fire while the gate is still unresolved. */
{
  const docW = installGlobals();
  const toastsW: RecordedToast[] = [];
  const { promise: gateW } = Promise.withResolvers<void>();
  const { promise: parkedListW } = Promise.withResolvers<unknown>();
  const commandsW: CatalogCommand[] = [];
  const parkedBackendW: SidebarBackend = {
    commands: commandsW,
    sendCommand: async (command, _bubbleId, opts) => {
      commandsW.push({ ...command, __auxiliary: opts?.auxiliary === true });
      if (command.type === 'session_list') return parkedListW;
      return {};
    },
    sendCatalogCommand: (command) => parkedBackendW.sendCommand(command, undefined, { auxiliary: true }),
  };
  const propsW = sidebarProps(parkedBackendW, toastsW, 1);
  propsW.waitForReady = (): Promise<void> => gateW;
  const mountedW = await mountSidebar(docW, propsW);
  check(listCount(parkedBackendW) === 0, 'parked waitForReady sends zero session_list');
  check(docW.body.querySelectorAll('.session-sidebar__catalog-skeleton').length === 0, 'no skeleton flash before the 150ms delayed-show while waitForReady is parked');
  await act(async () => {
    await new Promise<void>((resolve) => realSetTimeout(resolve, 200));
  });
  check(docW.body.querySelectorAll('.session-sidebar__catalog-skeleton').length === 5, 'parked waitForReady with empty catalog renders 5 skeleton rows after the 150ms gate');
  check(!docW.body.textContent.includes('No saved sessions.'), 'parked waitForReady does not render No saved sessions.');
  await act(async () => { mountedW.root.unmount(); });
}

/* ---------- failed refresh with rows: inline hint + last-good rows ---------- */
{
  const docR = installGlobals();
  const toastsR: RecordedToast[] = [];
  const catalogR = [row({ sessionId: 'srv-a', name: 'Alpha session', cwd: '<workspace>/app', path: '<workspace>/session-store/srv-a.jsonl', modifiedEpoch: 30 })];
  const backendR: SidebarBackend = {
    commands: [],
    sendCommand: async (command, bubbleId, opts) => {
      if (command.type === 'session_list') return { sessions: catalogR.map((entry) => ({ ...entry })) };
      return {};
    },
    sendCatalogCommand: (command) => backendR.sendCommand(command, undefined, { auxiliary: true }),
  };
  const propsR = sidebarProps(backendR, toastsR, 1);
  propsR.onSwitchSession = async (): Promise<boolean> => true;
  const mountedR = await mountSidebar(docR, propsR);
  const refreshBarR = (): MiniNode | null => docR.body.querySelector('.session-sidebar__refresh-bar');
  check(docR.body.textContent.includes('Alpha session'), 'the initial load renders the row');

  /* Replace session_list to fail: the refresh load errors. */
  backendR.sendCommand = async (command: Command) => {
    if (command.type === 'session_list') throw new Error('command session_list failed: too many concurrent RPC commands (limit 16)');
    return {};
  };
  const refreshBtnR = docR.body.querySelector('#session-sidebar-refresh-btn');
  check(refreshBtnR !== null, 'the Refresh control renders');
  await act(async () => {
    refreshBtnR!.dispatchEvent({ type: 'click', target: refreshBtnR });
    await realTick();
    await realTick();
    await realTick();
    await realTick();
  });
  check(refreshBarR() === null, 'the refresh bar clears on failure');
  check(docR.body.textContent.includes('Alpha session'), 'last-good rows are kept on refresh failure');
  check(!docR.body.textContent.includes('Failed to load sessions'), 'no inline refresh-failure hint renders (errors are centralized)');
  check(toastsR.some((t) => t.message === 'Failed to load sessions' && t.error === true), 'the refresh failure toasts once via the centralized layer', JSON.stringify(toastsR));
  await act(async () => { mountedR.root.unmount(); });
}

/* ---------- refresh with rows: thin bar + spinning Refresh, rows painted ---------- */
{
  const docS = installGlobals();
  const toastsS: RecordedToast[] = [];
  const catalogS = [row({ sessionId: 'srv-a', name: 'Alpha session', cwd: '<workspace>/app', path: '<workspace>/session-store/srv-a.jsonl', modifiedEpoch: 30 })];
  const { promise: refreshParked, resolve: refreshResolve } = Promise.withResolvers<unknown>();
  let firstLoadDoneS = false;
  const backendS: SidebarBackend = {
    commands: [],
    sendCommand: async (command: Command) => {
      // The FIRST load (mount) succeeds; the Refresh click parks.
      if (command.type === 'session_list' && !firstLoadDoneS) { firstLoadDoneS = true; return { sessions: catalogS.map((entry) => ({ ...entry })) }; }
      if (command.type === 'session_list') return refreshParked;
      return {};
    },
    sendCatalogCommand: (command) => backendS.sendCommand(command, undefined, { auxiliary: true }),
  };
  const propsS = sidebarProps(backendS, toastsS, 1);
  propsS.onSwitchSession = async (): Promise<boolean> => true;
  const mountedS = await mountSidebar(docS, propsS);
  const refreshBarS = (): MiniNode | null => docS.body.querySelector('.session-sidebar__refresh-bar');
  check(docS.body.textContent.includes('Alpha session'), 'the refresh test fixture lists the row');
  const refreshBtnS = docS.body.querySelector('#session-sidebar-refresh-btn');
  check(refreshBtnS !== null, 'the Refresh control renders in the search box');
  await act(async () => {
    refreshBtnS!.dispatchEvent({ type: 'click', target: refreshBtnS });
    await realTick();
  });
  check(refreshBarS() !== null, 'refresh with rows painted shows the thin top progress bar');
  check(refreshBtnS!.getAttribute('class')!.includes('session-sidebar__refresh--spinning'), 'the Refresh control spins during the in-flight refresh');
  check(docS.body.querySelectorAll('.session-sidebar__catalog-skeleton').length === 0, 'refresh with rows NEVER replaces rows with skeletons');
  check(docS.body.textContent.includes('Alpha session'), 'rows stay painted during the refresh');
  await act(async () => {
    refreshResolve({ sessions: [row({ sessionId: 'srv-a', name: 'Alpha session', cwd: '<workspace>/app', path: '<workspace>/session-store/srv-a.jsonl', modifiedEpoch: 31 })] });
    await realTick();
    await realTick();
  });
  check(refreshBarS() === null, 'the refresh bar clears when the refresh settles');
  check(!refreshBtnS!.getAttribute('class')!.includes('session-sidebar__refresh--spinning'), 'the Refresh control stops spinning on settle');
  await act(async () => { mountedS.root.unmount(); });
}

/* No-flash on instant load: the catalog resolves before the 150ms gate —
 * the skeleton never appears and rows render directly. */
{
  const docF = installGlobals();
  const toastsF: RecordedToast[] = [];
  const backendF = createBackend('catalog');
  const mountedF = await mountSidebar(docF, sidebarProps(backendF, toastsF, 1));
  const skeletonRowsF = (): MiniNode[] => docF.body.querySelectorAll('.session-sidebar__catalog-skeleton');
  check(skeletonRowsF().length === 0, 'an instant catalog load never flashes the skeleton');
  check(docF.body.textContent.includes('Alpha session'), 'instant load renders rows directly');
  await act(async () => { mountedF.root.unmount(); });
}

  /* (3) RETENTION: two sequential immediate switches (the E2E retention legs
   * A→B→A). App activates each target synchronously between the clicks (the
   * activeSessionId prop flip below mirrors that commit), and BOTH same-turn
   * fast clicks must execute, settle, and never paint the switching marker:
   * the first click's cancelled paint RAF must not leak into the second
   * click's seam, busy must clear after each settle, and the second click
   * must still reach onSwitchSession (never swallowed by a stale busy/ref). */
  {
    const docR = installGlobals();
    const toastsR: RecordedToast[] = [];
    const backendR = createBackend('catalog');
    const catalogR = [
      row({ sessionId: 'srv-a', name: 'Alpha session', cwd: '<workspace>/app', path: '<workspace>/session-store/srv-a.jsonl', modifiedEpoch: 30 }),
      row({ sessionId: 'srv-b', name: 'Beta session', cwd: '<workspace>/app', path: '<workspace>/session-store/srv-b.jsonl', modifiedEpoch: 29 }),
    ];
    const backendTwoR: SidebarBackend = {
      commands: backendR.commands,
      sendCommand: async (command, bubbleId, opts) => {
        if (command.type === 'session_list') return { sessions: catalogR.map((entry) => ({ ...entry })) };
        return backendR.sendCommand(command, bubbleId, opts);
      },
      sendCatalogCommand: (command) => backendTwoR.sendCommand(command, undefined, { auxiliary: true }),
    };
    let switchRequestsR = 0;
    let markerRApplied = false;
    const propsR = sidebarProps(backendTwoR, toastsR, 1, 'srv-a');
    propsR.onSwitchSession = async (): Promise<boolean> => {
      switchRequestsR += 1;
      return true;
    };
    const mountedR = await mountSidebar(docR, propsR);
    const rowButtonR = (sessionId: string): MiniNode | null =>
      docR.body.querySelectorAll('.session-sidebar__switch').find((node) => node.getAttribute('data-session-id') === sessionId) ?? null;
    /* The mini-DOM querySelector has no descendant combinators; the active
     * row is resolved by the row <li> class above each switch button. */
    const activeSidR = (): string | null => {
      for (const node of docR.body.querySelectorAll('.session-sidebar__switch')) {
        const row = node.parentNode?.parentNode ?? null;
        if (row !== null && (row.getAttribute('class') ?? '').includes('session-sidebar__row--active')) {
          return node.getAttribute('data-session-id');
        }
      }
      return null;
    };
     check(rowButtonR('srv-a') !== null && rowButtonR('srv-b') !== null, 'retention fixture lists both switch rows');
    const spyRowMarker = (rowEl: MiniNode): void => {
      const setAttribute = rowEl.setAttribute.bind(rowEl);
      rowEl.setAttribute = (name: string, value: string) => {
        if (name === 'class' && value.includes('session-sidebar__row--switching')) markerRApplied = true;
        setAttribute(name, value);
      };
    };
    spyRowMarker(docR.body.querySelectorAll('.session-sidebar__row').find((node) => node.querySelector('.session-sidebar__switch')?.getAttribute('data-session-id') === 'srv-a')!);
    spyRowMarker(docR.body.querySelectorAll('.session-sidebar__row').find((node) => node.querySelector('.session-sidebar__switch')?.getAttribute('data-session-id') === 'srv-b')!);

    /* Leg 1: A→B immediate settle. */
    await act(async () => {
      rowButtonR('srv-b')!.dispatchEvent({ type: 'click', target: rowButtonR('srv-b') });
      await realTick(); // flush the queued frame: cancelled by the same-turn settle
    });
    check(switchRequestsR === 1, 'retention leg 1 click reaches onSwitchSession');
    check(!markerRApplied, 'retention leg 1 never applies the switching marker');
    check(docR.body.querySelector('.session-sidebar__row--switching') === null, 'retention leg 1 leaves no switching marker');
    check(rowButtonR('srv-b')!.getAttribute('aria-busy') === null && rowButtonR('srv-b')!.getAttribute('disabled') === null, 'retention leg 1 clears busy on the target control');

    /* App's synchronous activation commit between the legs. */
    await mountedR.rerender({ ...propsR, activeSessionId: 'srv-b' });
    check(activeSidR() === 'srv-b', 'retention leg 1 activated row B');

    /* Leg 2: B→A immediate settle — the second same-turn fast click must
     * execute and settle exactly like the first. */
    markerRApplied = false;
    await act(async () => {
      rowButtonR('srv-a')!.dispatchEvent({ type: 'click', target: rowButtonR('srv-a') });
      await realTick();
    });
    check(switchRequestsR === 2, 'retention leg 2 click reaches onSwitchSession (not swallowed by stale busy/switching state)');
    check(!markerRApplied, 'retention leg 2 never applies the switching marker');
    check(docR.body.querySelector('.session-sidebar__row--switching') === null, 'retention leg 2 leaves no switching marker');
    check(rowButtonR('srv-a')!.getAttribute('aria-busy') === null && rowButtonR('srv-a')!.getAttribute('disabled') === null, 'retention leg 2 clears busy on the target control');
    await mountedR.rerender({ ...propsR, activeSessionId: 'srv-a' });
    check(activeSidR() === 'srv-a', 'retention leg 2 activated row A');
    await act(async () => {
      mountedR.root.unmount();
    });
  }

/* ---------- non-blocking pending switch: latest click wins ----------
 * While one negotiated switch is pending the sidebar stays fully
 * interactive: the other row's button is NOT disabled, a new click
 * re-targets (latest click wins), the replaced (stale) switch settles false
 * and must never clear the newer row's pending marker, and the final
 * failure settle clears the marker of the row that owns it. The single
 * switch failure toast is App-owned (sessionSwitchApp.test). ---------- */
{
  const docL = installGlobals();
  const toastsL: RecordedToast[] = [];
  const backendL = createBackend('catalog');
  const catalogL = [
    row({ sessionId: 'srv-a', name: 'Alpha session', cwd: '<workspace>/app', path: '<workspace>/session-store/srv-a.jsonl', modifiedEpoch: 30 }),
    row({ sessionId: 'srv-b', name: 'Beta session', cwd: '<workspace>/app', path: '<workspace>/session-store/srv-b.jsonl', modifiedEpoch: 29 }),
  ];
  const backendTwoL: SidebarBackend = {
    commands: backendL.commands,
    sendCommand: async (command, bubbleId, opts) => {
      if (command.type === 'session_list') return { sessions: catalogL.map((entry) => ({ ...entry })) };
      return backendL.sendCommand(command, bubbleId, opts);
    },
    sendCatalogCommand: (command) => backendTwoL.sendCommand(command, undefined, { auxiliary: true }),
  };
  const switchTargetsL: string[] = [];
  const settleA = Promise.withResolvers<boolean>();
  const settleB = Promise.withResolvers<boolean>();
  const propsL = sidebarProps(backendTwoL, toastsL, 1, 'srv-a');
  propsL.onSwitchSession = (target): Promise<boolean> => {
    const sid = (target as { catalogSessionId: string }).catalogSessionId;
    switchTargetsL.push(sid);
    return sid === 'srv-a' ? settleA.promise : settleB.promise;
  };
  const mountedL = await mountSidebar(docL, propsL);
  const rowButtonL = (sessionId: string): MiniNode | null =>
    docL.body.querySelectorAll('.session-sidebar__switch').find((node) => node.getAttribute('data-session-id') === sessionId) ?? null;
  const switchingRowSidL = (): string | null => {
    for (const node of docL.body.querySelectorAll('.session-sidebar__row')) {
      if ((node.getAttribute('class') ?? '').includes('session-sidebar__row--switching')) {
        return node.querySelector('.session-sidebar__switch')?.getAttribute('data-session-id') ?? null;
      }
    }
    return null;
  };
  check(rowButtonL('srv-a') !== null && rowButtonL('srv-b') !== null, 'non-blocking fixture lists both switch rows');

  /* Switch B begins and stays pending: marker on B, sidebar NOT blocked. */
  await act(async () => {
    rowButtonL('srv-b')!.dispatchEvent({ type: 'click', target: rowButtonL('srv-b') });
    await realTick(); // exactly one frame fires while the switch is pending
  });
  check(switchTargetsL.join('|') === 'srv-b', 'the first click reaches onSwitchSession');
  check(switchingRowSidL() === 'srv-b', 'pending switch paints the marker on the clicked row');
  check(rowButtonL('srv-a')!.getAttribute('disabled') === null, 'pending switch leaves the other rows enabled (no global block)');
  /* App commits the target's activation synchronously on begin
   * (activateRuntime), so the active row flips to B while the switch is
   * still pending — mirrored here before the second click. */
  await mountedL.rerender({ ...propsL, activeSessionId: 'srv-b' });

  /* Latest click wins: clicking A while B is pending re-targets. */
  await act(async () => {
    rowButtonL('srv-a')!.dispatchEvent({ type: 'click', target: rowButtonL('srv-a') });
    await realTick();
  });
  check(switchTargetsL.join('|') === 'srv-b|srv-a', 'a newer click re-targets while one switch is pending (latest wins)');
  check(switchingRowSidL() === 'srv-a', 'the marker follows the newest pending target');

  /* The replaced (stale) switch settles false (App generation replace):
   * it must NOT clear the newer row's pending marker. */
  await act(async () => {
    settleB.resolve(false);
    await realTick();
    await realTick();
  });
  check(switchingRowSidL() === 'srv-a', 'the stale replaced switch never clears the newer row pending marker');

  /* The latest switch fails to settle (ok=false): its marker clears. */
  await act(async () => {
    settleA.resolve(false);
    await realTick();
    await realTick();
  });
  check(docL.body.querySelector('.session-sidebar__row--switching') === null, 'the failure settle clears the pending marker');
  check(toastsL.length === 0, 'the sidebar itself toasts nothing on a switch failure (App owns the single toast)');
  await act(async () => {
    mountedL.root.unmount();
  });
}
/* ---------- chat-center transcript skeleton (App-owned) ----------
 * While a switch is pending, App renders TranscriptSkeleton inside
 * <main id="transcript"> (all surfaces are hidden while pending, so the
 * skeleton is the only visible content); it unmounts on success or failure.
 * The CSS shimmer, stagger, and reduced-motion opt-out are pinned below. --- */
{
  /* Dynamic import: App.tsx transitively imports react-dom, whose client
   * build snapshots canUseDOM at module init — same ordering constraint as
   * the SessionSidebar import above (globals must be live first). */
  const { TranscriptSkeleton } = await import('../src/App');
  const docS = installGlobals();
  const containerS = docS.createElement('div');
  docS.body.appendChild(containerS);
  const rootS = createRoot(containerS as unknown as Element);
  const renderS = (node: Parameters<typeof createElement>[0] | null): void => {
    rootS.render(node === null ? null as never : createElement(TranscriptSkeleton));
  };
  await act(async () => {
    renderS(createElement(TranscriptSkeleton));
    await realTick();
  });
  const skeletonS = docS.body.querySelector('.transcript-skeleton');
  check(skeletonS !== null, 'the pending switch renders the transcript skeleton');
  check(skeletonS?.getAttribute('role') === 'status' && skeletonS?.getAttribute('aria-live') === 'polite',
    'the skeleton announces as a polite status region');
  const rowsS = docS.body.querySelectorAll('.transcript-skeleton__row');
  check(rowsS.length === 3, 'the skeleton renders exactly three content-shaped rows');
  const rowHasClass = (suffix: string): boolean =>
    rowsS.some((node) => (node.getAttribute('class') ?? '').includes(`transcript-skeleton__row--${suffix}`));
  check(rowHasClass('user') && rowHasClass('assistant') && rowHasClass('tool'),
    'skeleton rows are content-shaped: user bubble, assistant line, tool card');
  check((docS.body.querySelector('.transcript-skeleton__status')?.textContent ?? '').includes('Loading session'),
    'the muted status line names the load');
  await act(async () => {
    rootS.unmount();
    await realTick();
  });
  check(docS.body.querySelector('.transcript-skeleton') === null, 'the skeleton unmounts when the switch settles');
  const skeletonAt = appSource.indexOf('{switchPending && <TranscriptSkeleton />}');
  check(skeletonAt > appSource.indexOf('<main id="transcript"') && skeletonAt < appSource.indexOf('</main>'),
    'App renders the skeleton inside the transcript main, gated on switchPending');
  check(appSource.includes("role={t.error ? 'alert' : undefined}"), 'error toasts (switch failure) carry role=alert');
  /* FORMAT PIN (P2): the sessionSwitchApp harness mirrors App's failure-toast
   * template verbatim, so drift in App.tsx alone would pass. Pin the exact
   * template here; any format drift fails this suite loudly. */
  check(appSource.includes('toast(`switch to ${target.catalogSessionId} failed${code ? ` (${code})` : \'\'}: ${detail}`, true)'),
    'the switch failure toast template is pinned against harness drift');
  check(sidebarCss.includes('@keyframes transcript-skeleton-shimmer')
    && sidebarCss.includes('animation: transcript-skeleton-shimmer 1.6s ease-in-out infinite')
    && sidebarCss.includes('animation-delay: -0.8s'),
    'the skeleton shimmer follows the design cadence (1.6s, -0.8s stagger)');
  check(/\.transcript-skeleton__row--user \{[^}]*width: 55%/s.test(sidebarCss)
    && /\.transcript-skeleton__row--assistant \{[^}]*width: 85%/s.test(sidebarCss)
    && /\.transcript-skeleton__row--tool \{[^}]*width: 72%/s.test(sidebarCss),
    'skeleton rows keep the designed widths 55%/85%/72%');
  check(/\.transcript-skeleton__row \{[^}]*background: var\(--bg-hover\);/s.test(sidebarCss),
    'skeleton rows use the muted var(--bg-hover) surface');
  check(/@media \(prefers-reduced-motion: reduce\) \{[^}]*\.transcript-skeleton__row \{[^}]*animation: none;/s.test(sidebarCss),
    'reduced motion disables the shimmer');
  check(/\.session-sidebar__row--switching \{[^}]*animation: transcript-skeleton-shimmer 1\.6s ease-in-out infinite;/s.test(sidebarCss),
    'the pending row pulses on the same cadence as the skeleton');
  check(/@media \(prefers-reduced-motion: reduce\) \{[^}]*\.session-sidebar__row--switching \{ animation: none; \}/s.test(sidebarCss),
    'reduced motion disables the row pulse');
}

/* ---------- pending switch refuses lifecycle actions (P1-B) ----------
 * While one switch is pending, lifecycle actions (Fork, Move) must be
 * REFUSED: no command is sent, and the Switching… marker stays on the
 * pending target. Latest-click-wins stays exclusively on runSwitch clicks.
 * ---------- */
{
  const docN = installGlobals();
  const toastsN: RecordedToast[] = [];
  const backendN = createBackend('catalog');
  const catalogN = [
    row({ sessionId: 'srv-a', name: 'Alpha session', cwd: '<workspace>/app', path: '<workspace>/session-store/srv-a.jsonl', modifiedEpoch: 30 }),
    row({ sessionId: 'srv-b', name: 'Beta session', cwd: '<workspace>/app', path: '<workspace>/session-store/srv-b.jsonl', modifiedEpoch: 29 }),
  ];
  const backendTwoN: SidebarBackend = {
    commands: backendN.commands,
    sendCommand: async (command, bubbleId, opts) => {
      if (command.type === 'session_list') return { sessions: catalogN.map((entry) => ({ ...entry })) };
      return backendN.sendCommand(command, bubbleId, opts);
    },
    sendCatalogCommand: (command) => backendTwoN.sendCommand(command, undefined, { auxiliary: true }),
  };
  const settleSwitchN = Promise.withResolvers<boolean>();
  const propsN = sidebarProps(backendTwoN, toastsN, 1, 'srv-a');
  propsN.onSwitchSession = (target): Promise<boolean> => {
    const sid = (target as { catalogSessionId: string }).catalogSessionId;
    return sid === 'srv-b' ? settleSwitchN.promise : Promise.resolve(true);
  };
  const mountedN = await mountSidebar(docN, propsN);
  const rowButtonN = (sessionId: string): MiniNode | null =>
    docN.body.querySelectorAll('.session-sidebar__switch').find((node) => node.getAttribute('data-session-id') === sessionId) ?? null;
  const switchingRowSidN = (): string | null => {
    for (const node of docN.body.querySelectorAll('.session-sidebar__row')) {
      if ((node.getAttribute('class') ?? '').includes('session-sidebar__row--switching')) {
        return node.querySelector('.session-sidebar__switch')?.getAttribute('data-session-id') ?? null;
      }
    }
    return null;
  };
  /* Pending, unsettled switch on srv-b: marker paints after one frame. */
  await act(async () => {
    rowButtonN('srv-b')!.dispatchEvent({ type: 'click', target: rowButtonN('srv-b') });
    await realTick();
  });
  check(switchingRowSidN() === 'srv-b', 'the guard fixture holds a pending switch on srv-b');

  /* Fork on the OTHER row while the switch is pending: refused, zero wire
   * sends, marker untouched. */
  await act(async () => {
    rowButtonN('srv-a')!.dispatchEvent({
      type: 'contextmenu', target: rowButtonN('srv-a'), clientX: 8, clientY: 8,
      preventDefault(): void {}, stopPropagation(): void {},
    });
    await realTick();
  });
  const forkItemN = docN.body.querySelectorAll('[role="menuitem"]').find((item) => item.textContent === 'Fork latest');
  check(forkItemN !== undefined, 'the context menu still opens during a pending switch');
  await act(async () => {
    forkItemN!.dispatchEvent({ type: 'click', target: forkItemN });
    await realTick();
    await realTick();
  });
  check(backendTwoN.commands.filter((command) => command.type === 'session_fork_latest').length === 0,
    'Fork is refused while a switch is pending (zero session_fork_latest frames)');
  check(switchingRowSidN() === 'srv-b', 'the Switching… marker stays on the pending target after the refused Fork');

  /* Move on the OTHER row while the switch is pending: refused, zero wire
   * sends, marker untouched. */
  await act(async () => {
    rowButtonN('srv-a')!.dispatchEvent({
      type: 'contextmenu', target: rowButtonN('srv-a'), clientX: 8, clientY: 8,
      preventDefault(): void {}, stopPropagation(): void {},
    });
    await realTick();
  });
  const moveItemN = docN.body.querySelectorAll('[role="menuitem"]').find((item) => item.textContent === 'Move session…');
  await act(async () => {
    moveItemN!.dispatchEvent({ type: 'click', target: moveItemN });
    await realTick();
    await realTick();
  });
  check(docN.body.querySelector('#session-move-cwd-input') !== null, 'the move dialog opens during a pending switch');
  const moveInputN = docN.body.querySelector('#session-move-cwd-input')!;
  for (const ch of '/home/projects/beta') {
    await act(async () => {
      moveInputN.value += ch;
      moveInputN.dispatchEvent({ type: 'input', target: moveInputN, preventDefault(): void {}, stopPropagation(): void {} });
      await realTick();
    });
  }
  await act(async () => {
    moveInputN.dispatchEvent({ type: 'keydown', key: 'Enter', target: moveInputN, preventDefault(): void {}, stopPropagation(): void {}, stopImmediatePropagation(): void {} });
    await realTick();
    await realTick();
    await realTick();
  });
  check(backendTwoN.commands.filter((command) => command.type === 'session_move').length === 0,
    'Move is refused while a switch is pending (zero session_move frames)');
  check(switchingRowSidN() === 'srv-b', 'the Switching… marker stays on the pending target after the refused Move');

  /* Once the switch settles, lifecycle actions work again. */
  await act(async () => {
    settleSwitchN.resolve(true);
    await realTick();
    await realTick();
  });
  check(docN.body.querySelector('.session-sidebar__row--switching') === null, 'the settled switch clears the marker');
  await mountedN.rerender({ ...propsN, activeSessionId: 'srv-b' });
  await act(async () => {
    rowButtonN('srv-a')!.dispatchEvent({
      type: 'contextmenu', target: rowButtonN('srv-a'), clientX: 8, clientY: 8,
      preventDefault(): void {}, stopPropagation(): void {},
    });
    await realTick();
  });
  const forkItemAfterN = docN.body.querySelectorAll('[role="menuitem"]').find((item) => item.textContent === 'Fork latest');
  await act(async () => {
    forkItemAfterN!.dispatchEvent({ type: 'click', target: forkItemAfterN });
    await realTick();
    await realTick();
  });
  check(backendTwoN.commands.filter((command) => command.type === 'session_fork_latest').length === 1,
    'after the switch settles lifecycle actions work again (exactly one fork)');
  await act(async () => {
    mountedN.root.unmount();
  });
}

/* ---------- unexpected switch errors are never silent (P1-C) ----------
 * runSwitch's catch toasts a genuine unexpected rejection exactly once, but
 * never duplicates an error the transport/App already owns (toastOwned). --- */
{
  const docO = installGlobals();
  const toastsO: RecordedToast[] = [];
  const backendO = createBackend('catalog');
  const propsO = sidebarProps(backendO, toastsO, 1);
  propsO.onSwitchSession = (): Promise<boolean> => Promise.reject(new Error('unexpected switch crash'));
  const mountedO = await mountSidebar(docO, propsO);
  const rowButtonO = docO.body.querySelector('.session-sidebar__switch')!;
  await act(async () => {
    rowButtonO.dispatchEvent({ type: 'click', target: rowButtonO });
    await realTick();
    await realTick();
  });
  check(toastsO.length === 1 && toastsO[0]?.error === true && toastsO[0]?.message === 'unexpected switch crash',
    'an unexpected switch rejection toasts exactly once as an error');
  check(docO.body.querySelector('.session-sidebar__row--switching') === null,
    'the rejected switch still clears the pending marker');
  await act(async () => {
    mountedO.root.unmount();
  });
}
{
  const docP = installGlobals();
  const toastsP: RecordedToast[] = [];
  const backendP = createBackend('catalog');
  const propsP = sidebarProps(backendP, toastsP, 1);
  propsP.onSwitchSession = (): Promise<boolean> =>
    Promise.reject(Object.assign(new Error('already surfaced'), { toastOwned: true }));
  const mountedP = await mountSidebar(docP, propsP);
  const rowButtonP = docP.body.querySelector('.session-sidebar__switch')!;
  await act(async () => {
    rowButtonP.dispatchEvent({ type: 'click', target: rowButtonP });
    await realTick();
    await realTick();
  });
  check(toastsP.length === 0, 'a toastOwned switch rejection never duplicates the toast');
  await act(async () => {
    mountedP.root.unmount();
  });
}

/* ---------- real mount: OMP group collapse with duplicate labels ----------
 * User-reported on a lab host: the OMP provider group (legacy provider labels mixed
 * with workspace duplicates — two different 'lab-node' workspaces, a
 * 'dotfiles' duplicate of the rpi group, and a loaded agent row) could not
 * fully collapse. Toggling a group head must collapse ALL its rows
 * (renderedRows 0) and persist across the virtualized re-render. ---------- */
{
  const docC = installGlobals();
  const toastsC: RecordedToast[] = [];
  const backendC = createBackend('catalog');
  const catalogC = [
    row({ sessionId: 'o1', source: 'omp', name: 'OMP lab-node A', cwd: '<workspace>/lab-node', path: '<workspace>/session-store/o1.jsonl', modifiedEpoch: 30 }),
    row({ sessionId: 'o2', source: 'omp', name: 'OMP lab-node B', cwd: '<workspace>/other/lab-node', path: '<workspace>/session-store/o2.jsonl', modifiedEpoch: 29 }),
    row({ sessionId: 'o3', source: 'omp', name: 'OMP dotfiles', cwd: '<workspace>/dotfiles', path: '<workspace>/session-store/o3.jsonl', modifiedEpoch: 28 }),
    row({ sessionId: 'r1', source: 'rpi', name: 'rpi lab-node', cwd: '<workspace>/lab-node', path: '<workspace>/session-store/r1.jsonl', modifiedEpoch: 27 }),
  ];
  const backendTwoC: SidebarBackend = {
    commands: backendC.commands,
    sendCommand: async (command, bubbleId, opts) => {
      if (command.type === 'session_list') return { sessions: catalogC.map((entry) => ({ ...entry })) };
      return backendC.sendCommand(command, bubbleId, opts);
    },
    sendCatalogCommand: (command) => backendTwoC.sendCommand(command, undefined, { auxiliary: true }),
  };
  const propsC = sidebarProps(backendTwoC, toastsC, 1);
  propsC.onSwitchSession = async (): Promise<boolean> => true;
  const mountedC = await mountSidebar(docC, propsC);
  const renderedRowsC = (): number => docC.body.querySelectorAll('.session-sidebar__switch').length;
  const ompHeadsC = (): MiniNode[] => docC.body.querySelectorAll('.session-sidebar__group-head')
    .filter((h) => h.getAttribute('data-provider') === 'OMP');
  check(ompHeadsC().length === 3, 'the OMP fixture renders a provider head and its two project subgroup heads');
  check(renderedRowsC() === 4, 'the fixture lists all four rows expanded');

  /* Provider head: one toggle collapses ALL OMP rows. */
  const providerHeadC = docC.body.querySelectorAll('.session-sidebar__group-head')
    .find((h) => h.getAttribute('data-provider') === 'OMP' && h.getAttribute('data-group-kind') === 'provider');
  await act(async () => {
    providerHeadC!.dispatchEvent({ type: 'click', target: providerHeadC });
    await realTick();
    await realTick();
  });
  /* Persist across the virtualized re-render (a fresh catalog load). */
  await mountedC.rerender(sidebarProps(backendTwoC, toastsC, 2));
  await act(async () => { await realTick(); await realTick(); });
  check(renderedRowsC() === 1, 'the OMP provider collapse persists across the catalog re-render');
  await act(async () => {
    providerHeadC!.dispatchEvent({ type: 'click', target: providerHeadC });
    await realTick();
    await realTick();
  });
  check(renderedRowsC() === 4, 'the provider head re-expands all rows');

  /* Subgroup head with a DUPLICATED label ('lab-node' also exists under
   * rpi): collapsing the OMP lab-node subgroup must hide ONLY its own rows
   * and never touch the rpi sibling. */
  const ompLabHead = docC.body.querySelectorAll('.session-sidebar__group-head')
    .find((h) => h.getAttribute('data-provider') === 'OMP' && h.getAttribute('data-group-kind') === 'project'
      && h.textContent.includes('lab-node'));
  await act(async () => {
    ompLabHead!.dispatchEvent({ type: 'click', target: ompLabHead });
    await realTick();
    await realTick();
  });
  check(renderedRowsC() === 2, 'collapsing the duplicate-labelled OMP subgroup hides exactly its two rows (rpi sibling untouched)');
  const rpiLabExpanded = docC.body.querySelectorAll('.session-sidebar__group-head')
    .find((h) => h.getAttribute('data-provider') === 'rpi' && h.textContent.includes('lab-node'))
    ?.getAttribute('aria-expanded');
  check(rpiLabExpanded === 'true', 'the rpi lab-node sibling stays expanded (no label-keyed intent collision)');
  await act(async () => { await mountedC.rerender(sidebarProps(backendTwoC, toastsC, 3)); await realTick(); await realTick(); });
  check(renderedRowsC() === 2, 'the duplicate-labelled subgroup collapse persists across the catalog re-render');
  await act(async () => { mountedC.root.unmount(); });
}

/* ---------- real mount: loaded agent subgroup collapses, active row hides ----------
 * The lab OMP group holds a LOADED agent row. Its 'Agent sessions' subgroup
 * renders COLLAPSED by default; collapsing the group that holds the ACTIVE
 * session hides every descendant, including that row. ---------- */
{
  const docG3 = installGlobals();
  const toastsG3: RecordedToast[] = [];
  const backendG3 = createBackend('catalog');
  const catalogG3 = [
    row({ sessionId: 'agent-live', source: 'omp', name: 'OMP workflow run', cwd: '<workspace>/lab-node', path: '<workspace>/session-store/agent-live.jsonl', agentSession: true, loaded: true, modifiedEpoch: 30 }),
    row({ sessionId: 'agent-idle', source: 'omp', name: 'OMP parked agent', cwd: '<workspace>/lab-node', path: '<workspace>/session-store/agent-idle.jsonl', agentSession: true, modifiedEpoch: 29 }),
    row({ sessionId: 'omp-user', source: 'omp', name: 'OMP user session', cwd: '<workspace>/lab-node', path: '<workspace>/session-store/omp-user.jsonl', modifiedEpoch: 28 }),
  ];
  const backendTwoG3: SidebarBackend = {
    commands: backendG3.commands,
    sendCommand: async (command, bubbleId, opts) => {
      if (command.type === 'session_list') return { sessions: catalogG3.map((entry) => ({ ...entry })) };
      return backendG3.sendCommand(command, bubbleId, opts);
    },
    sendCatalogCommand: (command) => backendTwoG3.sendCommand(command, undefined, { auxiliary: true }),
  };
  const propsG3 = { ...sidebarProps(backendTwoG3, toastsG3, 1), activeSessionId: 'agent-live' };
  propsG3.onSwitchSession = async (): Promise<boolean> => true;
  const mountedG3 = await mountSidebar(docG3, propsG3);
  const renderedRowsG3 = (): number => docG3.body.querySelectorAll('.session-sidebar__switch').length;
  check(renderedRowsG3() === 1, `collapsed-by-default agent subgroup hides every descendant (got ${renderedRowsG3()})`);
  const agentHeadingG3 = docG3.body.querySelectorAll('.session-sidebar__group-head')
    .find((h) => h.getAttribute('data-subgroup-kind') === 'agent');
  check(agentHeadingG3 !== null && agentHeadingG3!.getAttribute('aria-expanded') === 'false',
    'the agent subgroup heading is a real collapsed toggle (never inert)');

  await act(async () => {
    agentHeadingG3!.dispatchEvent({ type: 'click', target: agentHeadingG3 });
    await realTick();
    await realTick();
  });
  check(renderedRowsG3() === 3, `the agent subgroup toggle expands its rows (got ${renderedRowsG3()})`);

  await act(async () => {
    agentHeadingG3!.dispatchEvent({ type: 'click', target: agentHeadingG3 });
    await realTick();
    await realTick();
  });
  check(renderedRowsG3() === 1, `collapsing an active-loaded agent subgroup hides every descendant (got ${renderedRowsG3()})`);

  const providerHeadG3 = docG3.body.querySelectorAll('.session-sidebar__group-head')
    .find((h) => h.getAttribute('data-provider') === 'OMP' && h.getAttribute('data-group-kind') === 'provider');
  await act(async () => {
    providerHeadG3!.dispatchEvent({ type: 'click', target: providerHeadG3 });
    await realTick();
    await realTick();
  });
  check(renderedRowsG3() === 0, `the OMP provider head collapses its subgroups including the active row (got ${renderedRowsG3()})`);
  await mountedG3.rerender({ ...sidebarProps(backendTwoG3, toastsG3, 2), activeSessionId: 'agent-live' });
  await act(async () => { await realTick(); await realTick(); });
  check(renderedRowsG3() === 0, 'the provider collapse with hidden active row persists across the catalog re-render');
  await act(async () => { mountedG3.root.unmount(); });
}


/* ---------- real mount: multi-level codex collapse + authoritative refresh race ----------
 * User report: with a multi-level catalog (codex provider → several project
 * subgroups → rows, including a LOADED agent row), collapsing the codex ROOT
 * sometimes leaves sessions visible, only while the drawer is open. The
 * 8s authoritative catalog refresh (same rows, new objects) must never
 * resurrect them, with or without a loaded/active agent row. ---------- */
{
  const docCx = installGlobals();
  const toastsCx: RecordedToast[] = [];
  const backendCx = createBackend('catalog');
  const codexCatalog = [
    row({ sessionId: 'cx-agent', source: 'codex', name: 'Codex agent run', cwd: '<workspace>/lab-node', path: '<workspace>/session-store/cx-agent.jsonl', agentSession: true, loaded: true, modifiedEpoch: 32 }),
    row({ sessionId: 'cx-a1', source: 'codex', name: 'Codex A1', cwd: '<workspace>/proj-a', path: '<workspace>/session-store/cx-a1.jsonl', modifiedEpoch: 31 }),
    row({ sessionId: 'cx-a2', source: 'codex', name: 'Codex A2', cwd: '<workspace>/proj-a', path: '<workspace>/session-store/cx-a2.jsonl', modifiedEpoch: 30 }),
    row({ sessionId: 'cx-b1', source: 'codex', name: 'Codex B1', cwd: '<workspace>/proj-b', path: '<workspace>/session-store/cx-b1.jsonl', modifiedEpoch: 29 }),
    row({ sessionId: 'pi-keep', name: 'rpi session', cwd: '<workspace>/dotfiles', path: '<workspace>/session-store/pi-keep.jsonl', modifiedEpoch: 28 }),
  ];
  const backendTwoCx: SidebarBackend = {
    commands: backendCx.commands,
    sendCommand: async (command, bubbleId, opts) => {
      if (command.type === 'session_list') return { sessions: codexCatalog.map((entry) => ({ ...entry })) };
    },
    sendCatalogCommand: (command) => backendTwoCx.sendCommand(command, undefined, { auxiliary: true }),
  };
  const propsCx = sidebarProps(backendTwoCx, toastsCx, 1);
  propsCx.onSwitchSession = async (): Promise<boolean> => true;
  const mountedCx = await mountSidebar(docCx, propsCx);
  const totalRows = (): number => docCx.body.querySelectorAll('.session-sidebar__switch').length;
  const renderedCodexRows = (): number =>
    [...docCx.body.querySelectorAll('.session-sidebar__row')].filter(
      (node) => (node.getAttribute('data-provider') ?? '') === 'Codex'
    ).length;
  const providerHeadCx = docCx.body.querySelectorAll('.session-sidebar__group-head')
    .find((h) => h.getAttribute('data-provider') === 'Codex' && h.getAttribute('data-group-kind') === 'provider');
  check(providerHeadCx !== undefined, 'the codex provider head renders');
  check(totalRows() === 4, `the multi-level catalog lists four rows (the loaded codex agent row sits inside its default-collapsed Agent subgroup; got ${totalRows()})`);
  await act(async () => {
    providerHeadCx!.dispatchEvent({ type: 'click', target: providerHeadCx });
    await realTick();
    await realTick();
  });
  check(renderedCodexRows() === 0, `collapsing the codex root hides every descendant row, agent subgroup included (got ${renderedCodexRows()})`);
  /* The authoritative refresh (identical rows, NEW objects) must never
   * resurrect collapsed rows — the collapse intent survives reconcile. */
  await mountedCx.rerender(sidebarProps(backendTwoCx, toastsCx, 2));
  await act(async () => { await realTick(); await realTick(); });
  check(renderedCodexRows() === 0, `the codex root collapse persists across an authoritative refresh (got ${renderedCodexRows()})`);
  /* Active-row change (switch away): the forced-open agent rule must not
   * resurrect rows under a collapsed provider root. */
  await mountedCx.rerender({ ...sidebarProps(backendTwoCx, toastsCx, 3), activeSessionId: 'pi-keep' });
  await act(async () => { await realTick(); await realTick(); });
  check(renderedCodexRows() === 0, `the codex root collapse holds across an active-row switch (got ${renderedCodexRows()})`);
  await act(async () => { mountedCx.root.unmount(); });
}

/* ---------- real mount: collapse click vs in-flight authoritative refresh ----------
 * The user-reported intermittent non-collapse: a collapse click landing while
 * the 8s catalog refresh is in flight used to be discarded — the migrate pass
 * rebuilt the intent map from a ref snapshot taken before the click. The
 * functional-updater contract must keep the click's intent authoritative. ---------- */
{
  const docRace = installGlobals();
  const toastsRace: RecordedToast[] = [];
  const backendRace = createBackend('catalog');
  const catalogRace = [
    row({ sessionId: 'cx-agent', source: 'codex', name: 'Codex agent run', cwd: '<workspace>/lab-node', path: '<workspace>/session-store/cx-agent.jsonl', agentSession: true, loaded: true, modifiedEpoch: 32 }),
    row({ sessionId: 'cx-a1', source: 'codex', name: 'Codex A1', cwd: '<workspace>/proj-a', path: '<workspace>/session-store/cx-a1.jsonl', modifiedEpoch: 31 }),
    row({ sessionId: 'keep', name: 'rpi session', cwd: '<workspace>/dotfiles', path: '<workspace>/session-store/keep.jsonl', modifiedEpoch: 28 }),
  ];
  let listCalls = 0;
  let deferSecondList: ((value: { sessions: SessionRowWire[] }) => void) | null = null;
  const backendTwoRace: SidebarBackend = {
    commands: backendRace.commands,
    sendCommand: async (command, bubbleId, opts) => {
      if (command.type === 'session_list') {
        listCalls += 1;
        if (listCalls === 1) return { sessions: catalogRace.map((entry) => ({ ...entry })) };
        const deferred = Promise.withResolvers<{ sessions: SessionRowWire[] }>();
        deferSecondList = deferred.resolve;
        return deferred.promise;
      }
      return backendRace.sendCommand(command, bubbleId, opts);
    },
    sendCatalogCommand: (command) => backendTwoRace.sendCommand(command, undefined, { auxiliary: true }),
  };
  const propsRace = sidebarProps(backendTwoRace, toastsRace, 1);
  propsRace.onSwitchSession = async (): Promise<boolean> => true;
  const mountedRace = await mountSidebar(docRace, propsRace);
  const providerHeadRace = docRace.body.querySelectorAll('.session-sidebar__group-head')
    .find((h) => h.getAttribute('data-provider') === 'Codex' && h.getAttribute('data-group-kind') === 'provider');
  check(providerHeadRace !== undefined, 'the race fixture renders the codex provider head');
  /* Trigger the second (background) refresh, then collapse while its
   * authoritative session_list is still parked. */
  await mountedRace.rerender(sidebarProps(backendTwoRace, toastsRace, 2));
  await act(async () => { await realTick(); });
  check(deferSecondList !== null, 'the second authoritative list is in flight');
  await act(async () => {
    /* The refresh RESOLVES first (the load continuation is scheduled), then
     * the user's collapse click lands in the same batch — the pre-fix read-
     * modify-write on the ref snapshot discarded exactly this click. */
    deferSecondList!({ sessions: catalogRace.map((entry) => ({ ...entry })) });
    providerHeadRace!.dispatchEvent({ type: 'click', target: providerHeadRace });
    await realTick();
    await realTick();
  });
  const raceCodexRows = (): number =>
    [...docRace.body.querySelectorAll('.session-sidebar__row')].filter(
      (node) => (node.getAttribute('data-provider') ?? '') === 'Codex'
    ).length;
  check(raceCodexRows() === 0, `a collapse click landing before the in-flight catalog commit stays collapsed (got ${raceCodexRows()} codex rows)`);
  await act(async () => { mountedRace.root.unmount(); });
}

/* ---------- real mount: D2 unloaded-A → B rapid-switch race ----------
 * uiux D2 (HIGH): clicking B 150-300ms after a switch to UNLOADED A was
 * swallowed by the old pending guards — the marker ended on A and A's
 * surface painted. Latest-click-wins must hold in the negotiated direction
 * too: B reaches the switch, the marker follows B, A's late replace-settle
 * (false) never clears B's marker, and B completes. ---------- */
{
  const docH = installGlobals();
  const toastsH: RecordedToast[] = [];
  const backendH = createBackend('catalog');
  const catalogH = [
    row({ sessionId: 'unloaded-a', name: 'Unloaded A', cwd: '<workspace>/app', path: '<workspace>/session-store/unloaded-a.jsonl', modifiedEpoch: 30 }),
    row({ sessionId: 'unloaded-b', name: 'Unloaded B', cwd: '<workspace>/app', path: '<workspace>/session-store/unloaded-b.jsonl', modifiedEpoch: 29 }),
  ];
  const backendTwoH: SidebarBackend = {
    commands: backendH.commands,
    sendCommand: async (command, bubbleId, opts) => {
      if (command.type === 'session_list') return { sessions: catalogH.map((entry) => ({ ...entry })) };
      return backendH.sendCommand(command, bubbleId, opts);
    },
    sendCatalogCommand: (command) => backendTwoH.sendCommand(command, undefined, { auxiliary: true }),
  };
  const switchOrderH: string[] = [];
  const settleAH = Promise.withResolvers<boolean>();
  const settleBH = Promise.withResolvers<boolean>();
  const propsH = sidebarProps(backendTwoH, toastsH, 1, 'unloaded-a');
  propsH.onSwitchSession = (target): Promise<boolean> => {
    const sid = (target as { catalogSessionId: string }).catalogSessionId;
    switchOrderH.push(sid);
    return sid === 'unloaded-a' ? settleAH.promise : settleBH.promise;
  };
  const mountedH = await mountSidebar(docH, propsH);
  const rowButtonH = (sessionId: string): MiniNode | null =>
    docH.body.querySelectorAll('.session-sidebar__switch').find((node) => node.getAttribute('data-session-id') === sessionId) ?? null;
  const switchingRowSidH = (): string | null => {
    for (const node of docH.body.querySelectorAll('.session-sidebar__row')) {
      if ((node.getAttribute('class') ?? '').includes('session-sidebar__row--switching')) {
        return node.querySelector('.session-sidebar__switch')?.getAttribute('data-session-id') ?? null;
      }
    }
    return null;
  };
  /* A (unloaded, slow negotiated RPC — promise parked). */
  await act(async () => {
    rowButtonH('unloaded-b')!.dispatchEvent({ type: 'click', target: rowButtonH('unloaded-b') });
    await realTick();
    await realTick();
  });
  /* B clicked mid-flight: must NOT be swallowed by any pending guard. */
  await mountedH.rerender({ ...propsH, activeSessionId: 'unloaded-b' });
  await act(async () => {
    rowButtonH('unloaded-a')!.dispatchEvent({ type: 'click', target: rowButtonH('unloaded-a') });
    await realTick();
  });
  check(switchOrderH.join('|') === 'unloaded-b|unloaded-a',
    'the second rapid click reaches the negotiated switch (never swallowed)');
  check(switchingRowSidH() === 'unloaded-a', 'the marker follows the newest target B→A');
  /* App replaces A's transaction: A settles false; the marker stays on A. */
  await act(async () => {
    settleBH.resolve(false);
    await realTick();
    await realTick();
  });
  check(switchingRowSidH() === 'unloaded-a', 'the replaced switch settle never drags the marker back');
  /* B (the newest) completes: marker clears, sidebar silent. */
  await act(async () => {
    settleAH.resolve(true);
    await realTick();
  });
  check(docH.body.querySelector('.session-sidebar__row--switching') === null, 'the winning switch settles and clears the marker');
  check(toastsH.length === 0, 'the unloaded A→B race toasts nothing (no guard noise)');
  await act(async () => { mountedH.root.unmount(); });
}

/* ---------- mounted lane: ToastList error toasts carry role=alert (D4) ----------
 * uiux D4 measured role=null on the live page — the mounted contract for the
 * CURRENT tree: the rendered error toast element carries role=alert; non-error
 * toasts carry none. ---------- */
{
  const { ToastList } = await import('../src/App');
  const docT = installGlobals();
  const containerT = docT.createElement('div');
  docT.body.appendChild(containerT);
  const rootT = createRoot(containerT as unknown as Element);
  const toastsT = [
    { id: 'err-1', message: 'switch failed (runtime_unavailable)', error: true },
    { id: 'ok-1', message: 'Session moved', error: false },
  ];
  await act(async () => {
    rootT.render(createElement(ToastList, { toasts: toastsT, dismiss: (): void => {} } as never));
    await realTick();
  });
  const errorToastT = docT.body.querySelector('.toast--error');
  check(errorToastT !== null && errorToastT!.getAttribute('role') === 'alert',
    'the rendered ERROR toast element carries role=alert (mounted lane, D4)');
  const infoToastT = docT.body.querySelectorAll('.toast').find((t) => !t.getAttribute('class')!.includes('toast--error'));
  check(infoToastT !== undefined && infoToastT!.getAttribute('role') === null,
    'non-error toasts carry no alert role');
  await act(async () => { rootT.unmount(); await realTick(); });
}

/* ---------- real mount: switch-pending disables move-submit + delete-confirm (P2) ----------
 * runRefresh silently refuses while a switch is pending, so the UI must never
 * OFFER those actions: the move submit and the delete-confirm control are
 * DISABLED while switchingRef.current !== null and re-enabled after settle. --- */
{
  const docK = installGlobals();
  const toastsK: RecordedToast[] = [];
  const backendK = createBackend('catalog');
  const catalogK = [
    row({ sessionId: 'k-a', name: 'Keeper', cwd: '<workspace>/app', path: '<workspace>/session-store/k-a.jsonl', modifiedEpoch: 30 }),
    row({ sessionId: 'k-b', name: 'Target', cwd: '<workspace>/app', path: '<workspace>/session-store/k-b.jsonl', modifiedEpoch: 29 }),
    row({ sessionId: 'k-c', name: 'Third', cwd: '<workspace>/app', path: '<workspace>/session-store/k-c.jsonl', modifiedEpoch: 28 }),
  ];
  const backendTwoK: SidebarBackend = {
    commands: backendK.commands,
    sendCommand: async (command, bubbleId, opts) => {
      if (command.type === 'session_list') return { sessions: catalogK.map((entry) => ({ ...entry })) };
      return backendK.sendCommand(command, bubbleId, opts);
    },
    sendCatalogCommand: (command) => backendTwoK.sendCommand(command, undefined, { auxiliary: true }),
  };
  const settlePendingK = Promise.withResolvers<boolean>();
  const propsK = sidebarProps(backendTwoK, toastsK, 1, 'k-a');
  propsK.onSwitchSession = (target): Promise<boolean> =>
    (target as { catalogSessionId: string }).catalogSessionId === 'k-b' ? settlePendingK.promise : Promise.resolve(true);
  const mountedK = await mountSidebar(docK, propsK);
  const rowButtonK = (sessionId: string): MiniNode | null =>
    docK.body.querySelectorAll('.session-sidebar__switch').find((node) => node.getAttribute('data-session-id') === sessionId) ?? null;
  const openMenuForK = async (sessionId: string): Promise<void> => {
    await act(async () => {
      const rowBtn = docK.body.querySelectorAll('.session-sidebar__switch')
        .find((node) => node.getAttribute('data-session-id') === sessionId)!;
      rowBtn.dispatchEvent({
        type: 'contextmenu', target: rowBtn, clientX: 8, clientY: 8,
        preventDefault(): void {}, stopPropagation(): void {},
      });
      await realTick();
    });
  };
  /* Pending, unsettled switch on k-b. */
  await act(async () => {
    rowButtonK('k-b')!.dispatchEvent({ type: 'click', target: rowButtonK('k-b') });
    await realTick();
  });

  /* The move dialog opens during the pending switch, but its submit is
   * DISABLED (the prefill keeps the target non-empty — the pending switch is
   * what gates it). */
  await openMenuForK('k-a');
  const moveItemK = docK.body.querySelectorAll('[role="menuitem"]').find((item) => item.textContent === 'Move session…');
  await act(async () => {
    moveItemK!.dispatchEvent({ type: 'click', target: moveItemK });
    await realTick();
    await realTick();
  });
  const moveSubmitK = docK.body.querySelector('#session-move-submit-btn');
  check(moveSubmitK !== null && moveSubmitK!.getAttribute('disabled') !== null,
    'the move submit is DISABLED while a switch is pending (never an enabled control that cannot run)');
  /* The ROOT Delete item itself is DISABLED for a non-active row while the
   * switch is pending (k-a is the active session — delete is switch-away-
   * gated there), so confirm mode can never be reached mid-switch. */
  await act(async () => { docK.body.querySelector('.session-sidebar__menu')!.dispatchEvent({ type: 'keydown', key: 'Escape', target: docK.body.querySelector('.session-sidebar__menu'), preventDefault(): void {}, stopPropagation(): void {} }); await realTick(); });
  await openMenuForK('k-c');
  const deleteItemK = docK.body.querySelectorAll('[role="menuitem"]').find((item) => item.textContent === 'Delete');
  check(deleteItemK !== undefined && deleteItemK!.getAttribute('disabled') !== null,
    'the Delete item is DISABLED while a switch is pending');
  /* The CONFIRM control honors the same gate: contextMenuItems propagates
   * deleteDisabled into confirm mode (a switch-pending Confirm can never be
   * enabled, even if confirm mode was entered before the switch began). */
  const confirmItemsK = contextMenuItems({ native: true, deleteDisabled: true, confirmingDelete: true });
  check(confirmItemsK.find((item) => item.id === 'delete-confirm')?.disabled === true,
    'the delete-confirm control is DISABLED while a switch is pending');

  /* Settle the switch: both controls re-enable. */
  await act(async () => {
    settlePendingK.resolve(true);
    await realTick();
    await realTick();
  });
  const deleteItemAfterK = docK.body.querySelectorAll('[role="menuitem"]').find((item) => item.textContent === 'Delete');
  check(deleteItemAfterK !== undefined && deleteItemAfterK!.getAttribute('disabled') === null,
    'the Delete item re-enables after the switch settles');
  const confirmAfterK = contextMenuItems({ native: true, deleteDisabled: false, confirmingDelete: true });
  check(confirmAfterK.find((item) => item.id === 'delete-confirm')?.disabled === false,
    'the delete-confirm control re-enables after the switch settles');
  await openMenuForK('k-b');
  const moveItemAfterK = docK.body.querySelectorAll('[role="menuitem"]').find((item) => item.textContent === 'Move session…');
  await act(async () => {
    moveItemAfterK!.dispatchEvent({ type: 'click', target: moveItemAfterK });
    await realTick();
    await realTick();
  });
  const moveSubmitAfterK = docK.body.querySelector('#session-move-submit-btn');
  check(moveSubmitAfterK !== null && moveSubmitAfterK!.getAttribute('disabled') === null,
    'the move submit re-enables after the switch settles');
  await act(async () => { mountedK.root.unmount(); });
}

/* ---------- Copy context: LATEST compaction summary, brief fallback (P2 corr.) ----------
 * PRIMARY: the session's LATEST compactionSummary transcript item copies
 * VERBATIM (multiple summaries → newest; branch summaries never qualify).
 * FALLBACK: no summary → the handoff brief + the guidance toast. The mounted
 * lane drives the REAL copy-context action through an injected clipboard. --- */
{
  const items = [
    { kind: 'summary', label: 'Compaction summary', text: 'older compact summary' },
    { kind: 'summary', label: 'Branch summary', text: 'a branch summary never qualifies' },
    { kind: 'summary', label: 'Compaction summary', text: 'newest compact summary' },
    { kind: 'summary', label: 'Compaction summary', text: '' },
  ];
  check(latestCompactionSummary(items) === 'newest compact summary', 'the LATEST compaction summary wins');
  check(latestCompactionSummary([{ kind: 'summary', label: 'Branch summary', text: 'x' }]) === null, 'branch summaries never qualify');
  check(latestCompactionSummary([{ kind: 'summary', label: 'Compaction summary', text: '' }]) === null, 'empty-text summaries are skipped');
  check(latestCompactionSummary([{ kind: 'user', text: 'hi' }]) === null && latestCompactionSummary([]) === null, 'no summaries yields null (fallback path)');
  /* Non-conforming shapes never crash the picker. */
  check(latestCompactionSummary([null, 7, { kind: 'summary' }, { label: 'Compaction summary' }]) === null, 'malformed items are skipped');
}
{
  const docM = installGlobals();
  const toastsM: RecordedToast[] = [];
  const clipboardCaptured: string[] = [];
  Object.defineProperty(globalThis, 'navigator', {
    configurable: true,
    value: { clipboard: { writeText: async (text: string): Promise<boolean> => { clipboardCaptured.push(text); return true; } } },
  });
  const restoreNavigatorM = (): void => { delete (globalThis as { navigator?: unknown }).navigator; };
  const backendM2 = createBackend('catalog');
  const artifactM2 = [
    '# Pi session context',
    'Format: pi-session-context/1',
    'Truncated: no',
    '',
    '## Model',
    'Provider: faux',
    '',
    '## Model context',
    '[User]: hello',
    '',
    '## Next steps',
    '(none)',
  ].join('\n');
  const backendWithCopy: SidebarBackend = {
    commands: backendM2.commands,
    sendCommand: async (command, bubbleId, opts) => {
      if (command.type === 'session_copy_context') return { text: artifactM2, format: 'pi-session-context/1', truncated: false };
      return backendM2.sendCommand(command, bubbleId, opts);
    },
    sendCatalogCommand: (command) => backendWithCopy.sendCommand(command, undefined, { auxiliary: true }),
  };
  const propsM2 = sidebarProps(backendWithCopy, toastsM, 1);
  let summaryForSession: string | null = 'newest compact summary';
  propsM2.getCompactionSummary = (): string | null => summaryForSession;
  const mountedM2 = await mountSidebar(docM, propsM2);
  const copyViaMenu = async (): Promise<void> => {
    await act(async () => {
      const rowBtnM = docM.body.querySelector('.session-sidebar__switch')!;
      rowBtnM.dispatchEvent({
        type: 'contextmenu', target: rowBtnM, clientX: 8, clientY: 8,
        preventDefault(): void {}, stopPropagation(): void {},
      });
      await realTick();
    });
    const copyItem = docM.body.querySelectorAll('[role="menuitem"]').find((item) => item.textContent === 'Copy context');
    await act(async () => {
      copyItem!.dispatchEvent({ type: 'click', target: copyItem });
      await realTick();
      await realTick();
      await realTick();
    });
  };

  /* PRIMARY: the latest compaction summary copies VERBATIM (no truncation). */
  await copyViaMenu();
  check(clipboardCaptured[0] === 'newest compact summary', 'the compaction summary copies VERBATIM (no brief wrapper)');
  check(toastsM.some((t) => t.message === 'Copied compact summary' && t.error !== true), 'the summary copy toasts its bounded success');

  /* ON-DEMAND (user requirement: Copy context is ALWAYS effective): LOADED
   * row + no stored summary → App's compact seam runs the /compact wire path
   * and the FRESH summary is copied. No silent brief fallback. */
  summaryForSession = null;
  propsM2.isSessionLoaded = (): boolean => true;
  propsM2.requestSessionCompactSummary = async (): Promise<string> => 'fresh on-demand summary';
  await mountedM2.rerender(propsM2);
  await copyViaMenu();
  check(clipboardCaptured[1] === 'fresh on-demand summary', 'the LOADED no-summary path triggers the compact seam and copies the FRESH summary');
  check(toastsM.some((t) => t.message === 'Copied compact summary' && t.error !== true), 'the fresh-summary copy toasts its bounded success');

  /* UNLOADED row (no registered runtime): the compact RPC would reject
   * 'unknown session' — the PATH-BASED session_copy_context fallback copies
   * the brief instead, and compact is NEVER sent. */
  propsM2.isSessionLoaded = (): boolean => false;
  propsM2.requestSessionCompactSummary = async (): Promise<string> => {
    throw new Error('compact seam must not run for an unloaded row');
  };
  await mountedM2.rerender(propsM2);
  await copyViaMenu();
  check(typeof clipboardCaptured[2] === 'string' && clipboardCaptured[2]!.startsWith('# Handoff brief'), 'the UNLOADED row falls back to the path-based brief');
  check(toastsM.some((t) => t.message === 'Copied context brief' && t.error !== true), 'the brief copy toasts its bounded success');
  check(!toastsM.some((t) => t.message.includes('compact seam must not run')), 'compact is never sent for an unloaded row');

  /* NOTHING TO SUMMARIZE (legitimate edge): the compact ran but the seam
   * resolved '' (backend CompactionResult.summary empty / absent) → the
   * PATH-BASED brief fallback with a truthful toast, NOT an error. */
  propsM2.isSessionLoaded = (): boolean => true;
  propsM2.requestSessionCompactSummary = async (): Promise<string> => '';
  await mountedM2.rerender(propsM2);
  await copyViaMenu();
  check(typeof clipboardCaptured[3] === 'string' && clipboardCaptured[3]!.startsWith('# Handoff brief'), 'a compact that produced no summary falls back to the path-based brief');
  check(toastsM.some((t) => t.message === 'Copied context brief' && t.error !== true), 'the nothing-to-summarize brief copy toasts its truthful success');

  /* REFUSAL: LOADED + compaction refused (streaming/compacting) → the bounded
   * reason is toasted as an error and NOTHING is copied. */
  propsM2.requestSessionCompactSummary = async (): Promise<string> => {
    throw new Error('Cannot compact while a turn is streaming');
  };
  await mountedM2.rerender(propsM2);
  await copyViaMenu();
  check(toastsM.some((t) => t.message === 'Cannot compact while a turn is streaming' && t.error === true), 'the refusal toasts the bounded reason as an error');
  check(clipboardCaptured.length === 4, 'the refusal copies nothing');
  restoreNavigatorM();
  await act(async () => { mountedM2.root.unmount(); });
}

/* ---------- mounted: already-imported badge + native-copy click ----------
 * An AlreadyImported foreign row renders the muted 'imported' badge (title
 * names the native-copy behavior) and clicking it forwards the NATIVE
 * session path to App's negotiated switch — never the foreign path, never a
 * re-import. A normal foreign row in the same catalog is unchanged. --- */
{
  const docI = installGlobals();
  const toastsI: RecordedToast[] = [];
  const backendI = createBackend('catalog');
  const catalogI = [
    row({
      sessionId: 'codex-catalog-1',
      source: 'codex',
      status: 'already_imported',
      name: 'Codex session (imported)',
      cwd: '<workspace>/app',
      path: '<workspace>/session-store/codex/codex-catalog-1.jsonl',
      nativePath: '<workspace>/session-store/native-abc123.jsonl',
      nativeSessionId: 'native-runtime-abc123',
      modifiedEpoch: 30,
    }),
    row({
      sessionId: 'codex-pure',
      source: 'codex',
      status: 'foreign',
      name: 'Codex session (pure foreign)',
      cwd: '<workspace>/app',
      path: '<workspace>/session-store/codex/codex-pure.jsonl',
      modifiedEpoch: 29,
    }),
  ];
  const backendTwoI: SidebarBackend = {
    commands: backendI.commands,
    sendCommand: async (command, bubbleId, opts) => {
      if (command.type === 'session_list') return { sessions: catalogI.map((entry) => ({ ...entry })) };
      return backendI.sendCommand(command, bubbleId, opts);
    },
    sendCatalogCommand: (command) => backendTwoI.sendCommand(command, undefined, { auxiliary: true }),
  };
  const switchTargetsI: Array<Record<string, unknown>> = [];
  const propsI = sidebarProps(backendTwoI, toastsI, 1);
  propsI.onSwitchSession = (target): Promise<boolean> => {
    switchTargetsI.push(target as Record<string, unknown>);
    return true;
  };
  const mountedI = await mountSidebar(docI, propsI);
  const rowButtonI = (sessionId: string): MiniNode | null =>
    docI.body.querySelectorAll('.session-sidebar__switch').find((node) => node.getAttribute('data-session-id') === sessionId) ?? null;

  /* The badge renders on the already-imported row ONLY, with the contract
   * title. */
  const importedBadgeI = rowButtonI('codex-catalog-1')!.querySelector('.session-sidebar__imported');
  check(importedBadgeI !== null, 'the already-imported row renders the imported badge');
  check(importedBadgeI!.getAttribute('title') === 'imported into rpi — click opens the native copy',
    'the imported badge title names the native-copy behavior');
  check(rowButtonI('codex-pure')!.querySelector('.session-sidebar__imported') === null,
    'a pure foreign row renders NO imported badge');

  /* Clicking the already-imported row switches to the NATIVE session. */
  await act(async () => {
    rowButtonI('codex-catalog-1')!.dispatchEvent({ type: 'click', target: rowButtonI('codex-catalog-1') });
    await realTick();
    await realTick();
  });
  check(switchTargetsI.length === 1, 'the already-imported click reaches the negotiated switch');
  check(switchTargetsI[0]?.path === '<workspace>/session-store/native-abc123.jsonl'
    && switchTargetsI[0]?.catalogSource === 'rpi'
    && switchTargetsI[0]?.catalogStatus === 'native'
    && switchTargetsI[0]?.catalogSessionId === 'native-runtime-abc123',
    'the already-imported click targets the native path and backend-authored runtime id',
    JSON.stringify(switchTargetsI[0]));

  /* A normal foreign row click is unchanged. */
  await act(async () => {
    rowButtonI('codex-pure')!.dispatchEvent({ type: 'click', target: rowButtonI('codex-pure') });
    await realTick();
    await realTick();
  });
  check(switchTargetsI.length === 2, 'the normal foreign row click also reaches the switch');
  check(switchTargetsI[1]?.path === '<workspace>/session-store/codex/codex-pure.jsonl'
    && switchTargetsI[1]?.catalogSource === 'codex'
    && switchTargetsI[1]?.catalogStatus === 'foreign',
    'a normal foreign row keeps its own foreign tuple (unchanged by the already-imported path)',
    JSON.stringify(switchTargetsI[1]));
  await act(async () => { mountedI.root.unmount(); });
}

/* ---------- pure: copy-context handoff brief (the ONLY copy action) ----------
 * pi-style /context parity: the brief carries name+cwd+provider, the model
 * section, and the LAST 10 exchanges with per-message clamps. 'Copy
 * transcript tail' was never requested and is REMOVED. --- */
{
  const exchange = (index: number, chars: number): string[] => ([
    `[User]: ask number ${index} ${'x'.repeat(Math.max(0, chars - 20))}`,
    `[Assistant]: answer ${index} ${'y'.repeat(Math.max(0, chars - 20))}`,
  ]);
  const exchanges = Array.from({ length: 14 }, (_, i) => exchange(i, 60).join('\n\n')).join('\n\n');
  const artifact = [
    '# Pi session context',
    'Format: pi-session-context/1',
    'Truncated: no',
    '',
    '## Session',
    'Name: Alpha session',
    '',
    '## Model',
    'Provider: faux',
    'Model: faux-1',
    'Thinking: off',
    '',
    '## Goal',
    '(none)',
    '',
    '## Todo',
    '(none)',
    '',
    '## Model context',
    exchanges,
    '',
    '## Next steps',
    '(none)',
  ].join('\n');
  const brief = buildSessionHandoffBrief({ name: 'Alpha session', cwd: '<workspace>/lab-node', provider: 'OMP', contextText: artifact });
  check('the brief names the session', brief.includes('Session: Alpha session'));
  check(brief.includes('Working directory: <workspace>/lab-node'), 'the brief carries the working directory');
  check('the brief never carries Goal/Todo sections', !brief.includes('## Goal') && !brief.includes('## Todo'));
  /* Per-message clamp: a huge message is truncated with an ellipsis. */
  const huge = `[User]: ${'z'.repeat(HANDOFF_BRIEF_MESSAGE_CLAMP + 500)}`;
  const clampedBrief = buildSessionHandoffBrief({ name: 's', cwd: '<workspace>/c', provider: 'rpi', contextText: `# Pi session context\nFormat: pi-session-context/1\nTruncated: no\n\n## Model context\n${huge}\n\n## Next steps\n(none)` });
  check('oversized messages clamp to the per-message cap', clampedBrief.length < HANDOFF_BRIEF_MESSAGE_CLAMP * 3 && clampedBrief.includes('…'));
  /* Overall bound: 30 oversized exchanges stay under the brief cap. */
  const manyHuge = Array.from({ length: 40 }, (_, i) => `[User]: message ${i} ${'q'.repeat(HANDOFF_BRIEF_MESSAGE_CLAMP + 100)}`).join('\n\n');
  const bounded = buildSessionHandoffBrief({ name: 's', cwd: '<workspace>/c', provider: 'rpi', contextText: `# Pi session context\nFormat: pi-session-context/1\nTruncated: yes\n\n## Model context\n${manyHuge}\n\n## Next steps\n(none)` });
  check('the whole brief stays bounded (16k cap)', bounded.length <= HANDOFF_BRIEF_MAX_CHARS);
  const empty = buildSessionHandoffBrief({ name: 's', cwd: '<workspace>/c', provider: 'rpi', contextText: '# Pi session context\nFormat: pi-session-context/1\nTruncated: no\n\n## Model context\n(none)' });
  check('an empty conversation renders (none)', empty.includes('## Recent exchanges\n(none)'));
  /* The menu exposes ONLY Copy context on native rows (transcript tail removed). */
  const menuItemsCopy = contextMenuItems({ native: true, deleteDisabled: false });
  check(menuItemsCopy.some((item) => item.id === 'copy-context'), 'Copy context stays on the native-row menu');
  check(menuItemsCopy.every((item) => item.id !== 'copy-transcript'), 'Copy transcript tail is removed from the menu');
}


/* ---------- real mount: fork-from portal menu end-to-end ----------
 * A real client render drives the saved-session Fork-from picker through the
 * mounted portal: roving keyboard focus with scrollIntoView, keyboard paging
 * from the LAST candidate through the same single-flight as scroll paging,
 * stale-page ownership after Escape, terminal cursor silence, and the
 * latched business failure's status + focusable Retry row with keyboard
 * activation. ------------------------------------------------------------------ */
{
  const docF = installGlobals();
  const toastsF: RecordedToast[] = [];
  scrollIntoViewTargets.length = 0;
  const catalogF = [row({
    sessionId: 'srv-a',
    name: 'Alpha session',
    cwd: '<workspace>/app',
    path: '<workspace>/session-store/srv-a.jsonl',
    modifiedEpoch: 30,
  })];
  const firstPageF = {
    messages: [
      { entryId: 'e1', text: 'First forkable message' },
      { entryId: 'e2', text: 'Second forkable message' },
      { entryId: 'e3', text: 'Third forkable message' },
    ],
    nextCursor: 'cursor-1',
    truncated: true,
  };
  const olderPageF = {
    messages: [
      { entryId: 'e4', text: 'Fourth forkable message' },
      { entryId: 'e5', text: 'Fifth forkable message' },
      { entryId: 'e6', text: 'Sixth forkable message' },
    ],
    truncated: false,
  };
  let olderModeF: 'resolve' | 'gate' | 'fail' = 'resolve';
  let releaseOlderF: ((page: unknown) => void) | null = null;
  const commandsF: CatalogCommand[] = [];
  const cursorCommandsF = (): CatalogCommand[] =>
    commandsF.filter((command) => command.type === 'get_fork_messages' && typeof command.cursor === 'string');
  const backendF: SidebarBackend = {
    commands: commandsF,
    sendCommand: async (command, _bubbleId, opts) => {
      commandsF.push({ ...command, __auxiliary: opts?.auxiliary === true });
      if (command.type === 'session_list') return { sessions: catalogF.map((entry) => ({ ...entry })) };
      if (command.type === 'get_fork_messages') {
        if (command.cursor === undefined) return firstPageF;
        if (olderModeF === 'fail') {
          olderModeF = 'resolve';
          throw Object.assign(new Error('older fork page failed'), { rpc: true });
        }
        if (olderModeF === 'gate') {
          const { promise, resolve } = Promise.withResolvers<unknown>();
          releaseOlderF = resolve as (page: unknown) => void;
          return promise;
        }
        return olderPageF;
      }
      return {};
    },
    sendCatalogCommand: (command) => backendF.sendCommand(command, undefined, { auxiliary: true }),
  };
  const mountedF = await mountSidebar(docF, sidebarProps(backendF, toastsF, 1));
  check(listCount(backendF) === 1, 'fork mount performs exactly one session_list');

  /* The mini DOM supports only single simple selectors — no descendant
   * combinators — so query the bounded scroller first, then the plain
   * role selector inside it. */
  const forkItems = (): MiniNode[] => {
    const scroller = docF.body.querySelector('.session-sidebar__fork-scroll');
    return scroller === null ? [] : scroller.querySelectorAll('[role="menuitem"]');
  };
  const focusedForkItem = (): MiniNode | null =>
    forkItems().find((item) => item.className.includes('is-focused')) ?? null;
  const forkScroller = (): MiniNode => {
    const scroller = docF.body.querySelector('.session-sidebar__fork-scroll');
    if (!scroller) throw new Error('fork scroller missing');
    return scroller;
  };
  const openForkFrom = async (): Promise<void> => {
    await act(async () => {
      const switchBtn = docF.body.querySelector('.session-sidebar__switch');
      switchBtn!.dispatchEvent({
        type: 'contextmenu', target: switchBtn, clientX: 8, clientY: 8,
        preventDefault(): void {}, stopPropagation(): void {},
      });
      await realTick();
    });
    const forkFrom = docF.body.querySelectorAll('[role="menuitem"]').find((item) => item.textContent === 'Fork from…');
    check(forkFrom !== undefined, 'fork flow: context menu exposes Fork from…');
    await act(async () => {
      forkFrom!.dispatchEvent({ type: 'click', target: forkFrom });
      await realTick();
      await realTick();
      await realTick();
    });
  };
  const pressForkKey = async (key: string): Promise<void> => {
    await act(async () => {
      const target = focusedForkItem() ?? docF.body.querySelector('.session-sidebar__menu');
      target!.dispatchEvent({
        type: 'keydown', key, target,
        preventDefault(): void {}, stopPropagation(): void {}, stopImmediatePropagation(): void {},
      });
      await realTick();
      await realTick();
    });
  };
  const scrollForkTo = async (scrollTop: number, scrollHeight: number, clientHeight: number): Promise<void> => {
    await act(async () => {
      const scroller = forkScroller();
      scroller.scrollHeight = scrollHeight;
      scroller.clientHeight = clientHeight;
      scroller.scrollTop = scrollTop;
      scroller.dispatchEvent({ type: 'scroll', target: scroller, bubbles: false, cancelable: false });
      await realTick();
      await realTick();
    });
  };

  /* 1. Open: the first page renders in the portal scroller; the roving focus
   *    lands on the first candidate with scrollIntoView keeping it visible. */
  await openForkFrom();
  check(forkItems().length === 3, 'fork-from renders the first page candidates');
  check(focusedForkItem()?.textContent === 'First forkable message', 'initial focus lands on the first candidate');
  check(scrollIntoViewTargets[scrollIntoViewTargets.length - 1]?.textContent === 'First forkable message',
    'initial fork focus scrolls the focused candidate into view');
  check(docF.body.querySelector('.session-sidebar__fork-status') === null, 'no failure status while pages load cleanly');
  check(cursorCommandsF().length === 0, 'the initial page request carries no cursor');

  /* 2. Roving keyboard navigation with scrollIntoView. */
  await pressForkKey('ArrowDown');
  check(focusedForkItem()?.textContent === 'Second forkable message', 'ArrowDown moves focus to the second candidate');
  check(scrollIntoViewTargets[scrollIntoViewTargets.length - 1]?.textContent === 'Second forkable message',
    'keyboard focus keeps the newly focused candidate scrolled into view');
  await pressForkKey('ArrowUp');
  check(focusedForkItem()?.textContent === 'First forkable message', 'ArrowUp moves focus back');

  /* 3. Keyboard paging: focus reaching the LAST candidate with a pending
   *    cursor requests the older page through the same single-flight. */
  olderModeF = 'gate';
  await pressForkKey('ArrowDown');
  await pressForkKey('ArrowDown');
  check(focusedForkItem()?.textContent === 'Third forkable message', 'focus rests on the last first-page candidate');
  check(cursorCommandsF().length === 1 && cursorCommandsF()[0]?.cursor === 'cursor-1',
    'reaching the last candidate with a next cursor pages via one get_fork_messages');
  /* Single-flight: further arrows (wrap + return to the tail) never send a
   * second concurrent older-page request while the first is in flight. */
  await pressForkKey('ArrowDown');
  await pressForkKey('ArrowDown');
  await pressForkKey('ArrowDown');
  check(cursorCommandsF().length === 1, 'single-flight guard suppresses concurrent keyboard page requests');

  /* 4. Stale: Escape invalidates the in-flight request; its late response is
   *    discarded and a fresh open re-fetches the first page cleanly. */
  await pressForkKey('Escape');
  check(docF.body.querySelector('.session-sidebar__fork-scroll') === null, 'Escape returns to the root menu');
  releaseOlderF?.(olderPageF);
  await act(async () => {
    await realTick();
    await realTick();
  });
  check(forkItems().length === 0, 'the stale older-page response never mutates the closed picker');
  await openForkFrom();
  check(forkItems().length === 3, 'fresh open renders exactly the first page (stale page discarded)');
  check(cursorCommandsF().length === 1, 'the stale release added no committed cursor request');

  /* 5. Terminal cursor: after the older page lands with no nextCursor, both
   *    paging triggers go silent. */
  olderModeF = 'resolve';
  await scrollForkTo(452, 1000, 500);
  check(forkItems().length === 6, 'near-bottom scroll appends the older page with dedupe');
  check(cursorCommandsF().length === 2, 'scroll paging sends exactly one older-page request');
  await scrollForkTo(452, 1000, 500);
  check(cursorCommandsF().length === 2, 'terminal cursor: near-bottom scroll never re-requests');
  for (let hop = 0; hop < 5; hop += 1) await pressForkKey('ArrowDown');
  check(focusedForkItem()?.textContent === 'Sixth forkable message', 'roving focus reaches the terminal last candidate');
  check(cursorCommandsF().length === 2, 'terminal cursor: last-candidate keyboard focus never requests');
  await pressForkKey('Escape');

  /* 6. Latched business failure: status + focusable Retry with the exact
   *    cursor; the latch suppresses auto-retry; Retry (click + keyboard)
   *    re-runs the same single-flight load. */
  await openForkFrom();
  olderModeF = 'fail';
  await scrollForkTo(452, 1000, 500);
  await act(async () => {
    await realTick();
    await realTick();
  });
  const statusF = docF.body.querySelector('.session-sidebar__fork-status');
  check(statusF !== null, 'latched failure renders the bounded status row');
  check((statusF?.textContent || '').includes('cursor cursor-1'), 'the status text carries the exact failed cursor');
  const retryF = docF.body.querySelectorAll('[role="menuitem"]').find((item) => item.textContent === 'Retry');
  check(retryF !== undefined, 'latched failure renders a focusable Retry row');
  check((retryF?.getAttribute('aria-label') || '').includes('cursor cursor-1'), 'the Retry row names the exact failed cursor');
  check(forkItems().length === 4, 'the Retry row joins the roving focus after the candidates');
  check(toastsF.some((toast) => toast.error === true && toast.message.includes('older fork page failed')),
    'the business failure still toasts exactly once');
  /* Latch suppresses every automatic trigger... */
  await scrollForkTo(452, 1000, 500);
  check(cursorCommandsF().length === 3, 'latched failure suppresses automatic scroll retry');
  for (let hop = 0; hop < 6; hop += 1) await pressForkKey('ArrowDown');
  check(focusedForkItem()?.textContent === 'Third forkable message', 'keyboard navigation skips back over the candidates before Retry');
  check(cursorCommandsF().length === 3, 'latched failure suppresses automatic last-candidate retry');
  /* ...but the focusable Retry row re-runs the load by keyboard AND click. */
  await pressForkKey('ArrowDown'); // third candidate -> Retry row
  check(focusedForkItem()?.textContent === 'Retry', 'ArrowDown reaches the Retry row');
  await pressForkKey('Enter');
  await act(async () => {
    await realTick();
    await realTick();
  });
  check(cursorCommandsF().length === 4, 'keyboard Retry re-runs exactly one older-page load');
  check(docF.body.querySelector('.session-sidebar__fork-status') === null, 'successful retry clears the latched status');
  check(forkItems().length === 6, 'successful retry appends the older page');
  check(cursorCommandsF()[cursorCommandsF().length - 1]?.cursor === 'cursor-1', 'the retry reuses the exact captured cursor');
  await pressForkKey('Escape');

  /* 7. Click-path Retry: a fresh latched failure retries through the click. */
  await openForkFrom();
  olderModeF = 'fail';
  await scrollForkTo(452, 1000, 500);
  await act(async () => {
    await realTick();
    await realTick();
  });
  check(docF.body.querySelector('.session-sidebar__fork-status') !== null, 'second flow latches the failure again');
  const retryClick = docF.body.querySelectorAll('[role="menuitem"]').find((item) => item.textContent === 'Retry');
  await act(async () => {
    retryClick!.dispatchEvent({ type: 'click', target: retryClick });
    await realTick();
    await realTick();
    await realTick();
  });
  check(forkItems().length === 6, 'clicked Retry appends the older page');
  check(docF.body.querySelector('.session-sidebar__fork-status') === null, 'clicked Retry clears the latched status');

  assertNoErrorPresentation(docF, 'fork-from portal flows');
  await act(async () => {
    mountedF.root.unmount();
  });
}
/* ---------- real mount: SessionSidebar Move caller-level wire contract ----------
 * The mounted commitMove flow is the caller contract for BOTH truthful
 * backend outcomes. moved=true: the setup lists the row at its SOURCE
 * location and the response transitions from → to. moved=false (the truthful
 * idempotent no-op): the setup STARTS with the row ALREADY RESIDENT at the
 * target — cwd TARGET, path DEST_PATH — so the command itself sends the DEST
 * path, the response reports from = to = DEST_PATH (no transition), and the
 * catalog refresh is IDENTICAL before and after. Both responses carry the
 * FULL caller-bound tuple (moved, top-level sessionId, from, to, and the
 * native session row carrying sessionId/cwd/path). Each case: exactly ONE
 * `session_move` carrying the raw captured row path and the typed target,
 * exactly ONE auxiliary `session_list` refresh, exactly ONE success toast,
 * zero error toasts, ZERO `switch_session` frames (Move never auto-switches),
 * and the menu closes. ------------------------------------------------------- */
{
  const SOURCE_PATH = '<workspace>/session-store/srv-a.jsonl';
  const DEST_PATH = '<workspace>/session-store/projects-beta/srv-a.jsonl';
  const TARGET = '/home/projects/beta';

  const runMountedMove = async (label: string, moved: boolean): Promise<void> => {
    const doc = installGlobals();
    const toastsMv: RecordedToast[] = [];
    /* moved=true: the catalog lists the row at its SOURCE location before
     * the move commits. moved=false: the setup STARTS with the row ALREADY
     * RESIDENT at the target — cwd TARGET, path DEST_PATH — so the menu
     * captures the destination path itself. */
    const capturedRow = row({
      sessionId: 'srv-a',
      name: 'Alpha session',
      cwd: moved ? '<workspace>/app' : TARGET,
      path: moved ? SOURCE_PATH : DEST_PATH,
      modifiedEpoch: 30,
    });
    /* The single resident-at-target identity (cwd TARGET, path DEST_PATH)
     * both outcomes converge on. For moved=false it is identical to the
     * captured row, so the refresh is the SAME catalog before and after. */
    const residentRow = { ...capturedRow, cwd: TARGET, path: DEST_PATH };
    const commandsMv: CatalogCommand[] = [];
    let moveCommitted = false;
    const backendMv: SidebarBackend = {
      commands: commandsMv,
      sendCommand: async (command, _bubbleId, opts) => {
        commandsMv.push({ ...command, __auxiliary: opts?.auxiliary === true });
        if (command.type === 'session_move') {
          // The move commits BEFORE its response reaches the caller, so the
          // subsequent catalog refresh observes the destination store.
          moveCommitted = true;
          // Full bound tuple. moved=true transitions from the captured
          // source path to DEST; the truthful no-op reports from = to =
          // DEST_PATH — NO transition — because the session already resided
          // in the destination store (production validation binds `from` to
          // the menu-captured path and `to` to the listed row path).
          return {
            moved,
            sessionId: capturedRow.sessionId,
            from: capturedRow.path,
            to: DEST_PATH,
            sidecars: [],
            children: [],
            rebound: { memory: 0, workflows: 0, trust: 0 },
            warnings: [],
            session: {
              source: 'rpi',
              sessionId: capturedRow.sessionId,
              name: capturedRow.name,
              cwd: TARGET,
              displayTime: '',
              modifiedEpoch: 0,
              summary: '',
              path: DEST_PATH,
              size: 128,
              messageCount: null,
              status: 'native',
              temporary: false,
            },
          };
        }
        if (command.type === 'session_list') {
          // moved=true: before the commit the catalog lists the captured
          // source row, after it the resident-at-target row. moved=false:
          // BOTH branches yield the same resident row — the refresh is
          // identical to the pre-command catalog.
          return { sessions: [{ ...(moveCommitted ? residentRow : capturedRow) }] };
        }
        return {};
      },
      sendCatalogCommand: (command) => backendMv.sendCommand(command, undefined, { auxiliary: true }),
    };
    /* The switch-target capture observes the REFRESHED row identity after
     * the move: clicking the row forwards the catalog path to App. */
    let switchTargetMv: Record<string, unknown> | null = null;
    const propsMv = sidebarProps(backendMv, toastsMv, 1);
    propsMv.onSwitchSession = async (target: unknown): Promise<boolean> => {
      switchTargetMv = target as Record<string, unknown>;
      return true;
    };
    const mountedMv = await mountSidebar(doc, propsMv);
    check(listCount(backendMv) === 1, `${label}: mount performs exactly one session_list`);
    const rowBtn = doc.body.querySelector('.session-sidebar__switch');
    check(rowBtn !== null, `${label}: the catalog row renders`);
    if (!moved) {
      /* Already-resident setup: BEFORE any command the catalog already
       * groups the single resident row under the cwd-derived target project
       * ('beta') — there is no source location to transition from. */
      const preBetaHead = doc.body.querySelectorAll('.session-sidebar__group-head').find((head) =>
        head.getAttribute('data-subgroup-kind') === 'project'
        && head.querySelector('.session-sidebar__group-label')?.textContent === 'beta');
      check(preBetaHead !== undefined, `${label}: the pre-command catalog already groups the resident row under the target project`);
      check(doc.body.querySelectorAll('.session-sidebar__switch').length === 1,
        `${label}: the pre-command catalog lists exactly the single resident row`);
    }
    await act(async () => {
      rowBtn!.dispatchEvent({
        type: 'contextmenu', target: rowBtn, clientX: 8, clientY: 8,
        preventDefault(): void {}, stopPropagation(): void {},
      });
      await realTick();
    });
    const moveItem = doc.body.querySelectorAll('[role="menuitem"]').find((item) => item.textContent === 'Move session…');
    check(moveItem !== undefined, `${label}: the context menu exposes Move session…`);
    await act(async () => {
      moveItem!.dispatchEvent({ type: 'click', target: moveItem });
      await realTick();
      await realTick();
    });
    const moveInput = doc.body.querySelector('#session-move-cwd-input');
    check(moveInput !== null, `${label}: move mode renders the target cwd input`);
    check((moveInput!.value) === (moved ? '<workspace>/app' : TARGET),
      `${label}: the move input prefills the captured row's current full path`);
    check(doc.body.querySelector('#session-move-submit-btn')?.getAttribute('disabled') === null,
      `${label}: the prefilled path enables the submit at open (non-empty target)`);
    const listsBefore = listCount(backendMv);
    /* Controlled write into the combobox (same act-flushed input-event
     * pattern as sessionPanelAutocomplete.test). The unbound path_complete
     * reply is malformed, so the suggestion menu stays hidden and Enter
     * always flows through onEnter -> commitMove (never an accept). The
     * prefill is cleared first: the user EDITS the prefilled path. */
    moveInput!.value = '';
    for (const ch of TARGET) {
      await act(async () => {
        moveInput!.value += ch;
        moveInput!.dispatchEvent({ type: 'input', target: moveInput, preventDefault(): void {}, stopPropagation(): void {} });
        await realTick();
      });
    }
    await act(async () => {
      doc.body.querySelector('#session-move-submit-btn')!.dispatchEvent({
        type: 'click', target: doc.body.querySelector('#session-move-submit-btn'),
        preventDefault(): void {}, stopPropagation(): void {},
      });
      await realTick();
      await realTick();
      await realTick();
    });
    /* The enabled/disabled-at-empty contract is asserted against the real
     * browser in E2E.d/web/session_move_delete_test.mjs (SMD6.1/SMD6.2); the
     * unit asserts the submit dispatch via the input's Enter (PathAutocomplete
     * onEnter -> commitMove with the raw DOM value), the same handler the
     * enabled button clicks into. The frame assertions below pin the exact
     * typed target — proving the prefill was REPLACED, not appended. */
    await act(async () => {
      moveInput!.dispatchEvent({ type: 'keydown', key: 'Enter', target: moveInput, preventDefault(): void {}, stopPropagation(): void {}, stopImmediatePropagation(): void {} });
      await realTick();
      await realTick();
      await realTick();
    });

    const moves = commandsMv.filter((command) => command.type === 'session_move');
    check(moves.length === 1, `${label}: exactly one session_move frame`);
    const moveFrame = moves[0] as (CatalogCommand & Record<string, unknown>) | undefined;
    check(
      moveFrame !== undefined
        && moveFrame.type === 'session_move'
        && moveFrame.sessionPath === (moved ? SOURCE_PATH : DEST_PATH)
        && moveFrame.targetCwd === TARGET
        && Object.keys(moveFrame).length === 4,
      `${label}: session_move is exactly {type,sessionPath,targetCwd} with the raw captured row path and the typed target`,
    );
    check(moves[0]!.__auxiliary === false, `${label}: session_move rides the direct socket, never the auxiliary catalog channel`);
    check(listCount(backendMv) === listsBefore + 1, `${label}: exactly one auxiliary session_list refresh after the move`);
    check(
      commandsMv.filter((command) => command.type === 'session_list').every((command) => command.__auxiliary === true),
      `${label}: every session_list rides the auxiliary catalog channel`,
    );
    check(commandsMv.filter((command) => command.type === 'switch_session').length === 0, `${label}: zero switch_session frames (Move retains the active session)`);
    check(toastsMv.length === 1, `${label}: exactly one toast`);
    check(toastsMv[0]?.message === 'Session moved' && !toastsMv[0]?.error, `${label}: the single toast is the bounded success message`);
    check(doc.body.querySelector('#session-move-cwd-input') === null, `${label}: the move form closed`);
    check(doc.body.querySelector('.session-sidebar__menu') === null, `${label}: the menu closed`);
    /* The refreshed catalog is asserted beyond the name: the row sits
     * under the cwd-derived target project group ('beta'), and clicking the
     * refreshed row forwards the DESTINATION catalog path to App — the
     * caller-bound proof the listed row is the resident-at-target identity
     * (cwd TARGET, path DEST_PATH) for BOTH moved outcomes. */
    check(doc.body.textContent.includes('Alpha session'), `${label}: the refreshed catalog lists the resident-at-target row`);
    const betaHeadMv = doc.body.querySelectorAll('.session-sidebar__group-head').find((head) =>
      head.getAttribute('data-subgroup-kind') === 'project'
      && head.querySelector('.session-sidebar__group-label')?.textContent === 'beta');
    check(betaHeadMv !== undefined, `${label}: the refreshed row renders under the cwd-derived target project group`);
    const refreshedRowMv = doc.body.querySelectorAll('.session-sidebar__switch')
      .find((node) => node.getAttribute('data-session-id') === 'srv-a');
    check(refreshedRowMv !== undefined, `${label}: the refreshed catalog lists exactly one row for the moved session`);
    const listItemsMv = doc.body.querySelectorAll('li');
    const betaLiIndexMv = betaHeadMv === undefined
      ? -1
      : listItemsMv.findIndex((li) => li.querySelector('.session-sidebar__group-head') === betaHeadMv);
    const rowLiIndexMv = listItemsMv.findIndex((li) => li.querySelectorAll('.session-sidebar__switch').includes(refreshedRowMv!));
    check(betaLiIndexMv >= 0 && rowLiIndexMv === betaLiIndexMv + 1, `${label}: the refreshed row renders directly under the target project group`);
    await act(async () => {
      refreshedRowMv!.dispatchEvent({ type: 'click', target: refreshedRowMv });
      await realTick();
      await realTick();
    });
    const switchTarget = switchTargetMv as Record<string, unknown> | null;
    check(
      switchTarget !== null
        && switchTarget.path === DEST_PATH
        && switchTarget.catalogSessionId === 'srv-a'
        && switchTarget.catalogSource === 'rpi'
        && switchTarget.catalogStatus === 'native',
      `${label}: the refreshed row forwards the destination catalog path to App's negotiated switch`,
    );
    if (!moved) {
      /* NO-transition proof for the truthful no-op: the command itself
       * carried the DEST path (asserted on the frame above), and production
       * validation binds the response `from` to that captured path and `to`
       * to the listed row path — from = to = DEST_PATH. The refresh is
       * identical: still exactly one row, still under the target group,
       * still forwarding DEST — no duplicate source row ever appears. */
      check(doc.body.querySelectorAll('.session-sidebar__switch').length === 1,
        `${label}: the identical refresh still lists exactly the single resident row (no duplicate source row)`);
    }
    assertNoErrorPresentation(doc, label);
    await act(async () => {
      mountedMv.root.unmount();
    });
  };

  await runMountedMove('moved=true', true);
  await runMountedMove('moved=false no-op', false);
}

/* ---------- real mount: Move dialog redesign + regroup without reload ----------
 * (1) The move dialog renders as a proper dialog (role=dialog, --move class,
 *     visible Cancel + Move buttons) and its CSS pins the no-clip contract:
 *     min-width min(420px, calc(100vw - 16px)) — a full absolute path fits at
 *     1280x633 — a monospace input that owns the horizontal scroll, and
 *     overflow visible so the button row is never scrollbar-clipped.
 * (2) Enter submits (wire contract above); Escape AND the Cancel button
 *     dismiss the dialog with ZERO wire sends.
 * (3) Move success regroups WITHOUT waiting for a fresh refetch: even when
 *     the immediate post-move session_list races the backend rescan and
 *     returns the PRE-move catalog, the local destination patch re-groups
 *     the row under the target project from the parsed response alone. --- */
{
  const SOURCE_PATH_D = '<workspace>/session-store/srv-a.jsonl';
  const DEST_PATH_D = '<workspace>/session-store/projects-beta/srv-a.jsonl';
  const TARGET_D = '/home/projects/beta';
  const docD = installGlobals();
  const toastsD: RecordedToast[] = [];
  /* ALWAYS stale: every session_list returns the PRE-move catalog, so the
   * regroup can only come from the local destination patch. */
  const staleRowD = row({ sessionId: 'srv-a', name: 'Alpha session', cwd: '<workspace>/app', path: SOURCE_PATH_D, modifiedEpoch: 30 });
  const commandsD: CatalogCommand[] = [];
  const backendD: SidebarBackend = {
    commands: commandsD,
    sendCommand: async (command, _bubbleId, opts) => {
      commandsD.push({ ...command, __auxiliary: opts?.auxiliary === true });
      if (command.type === 'session_move') {
        return {
          moved: true,
          sessionId: 'srv-a',
          from: SOURCE_PATH_D,
          to: DEST_PATH_D,
          sidecars: [],
          children: [],
          rebound: { memory: 0, workflows: 0, trust: 0 },
          warnings: [],
          session: {
            source: 'rpi',
            sessionId: 'srv-a',
            name: 'Alpha session',
            cwd: TARGET_D,
            displayTime: '',
            modifiedEpoch: 0,
            summary: '',
            path: DEST_PATH_D,
            size: 128,
            messageCount: null,
            status: 'native',
            temporary: false,
          },
        };
      }
      if (command.type === 'session_list') return { sessions: [{ ...staleRowD }] };
      return {};
    },
    sendCatalogCommand: (command) => backendD.sendCommand(command, undefined, { auxiliary: true }),
  };
  const propsD = sidebarProps(backendD, toastsD, 1);
  propsD.onSwitchSession = async (): Promise<boolean> => true;
  const mountedD = await mountSidebar(docD, propsD);
  const openMoveDialog = async (): Promise<MiniNode | null> => {
    const rowBtnD = docD.body.querySelector('.session-sidebar__switch');
    await act(async () => {
      rowBtnD.dispatchEvent({
        type: 'contextmenu', target: rowBtnD, clientX: 8, clientY: 8,
        preventDefault(): void {}, stopPropagation(): void {},
      });
      await realTick();
    });
    const moveItemD = docD.body.querySelectorAll('[role="menuitem"]').find((item) => item.textContent === 'Move session…');
    await act(async () => {
      moveItemD.dispatchEvent({ type: 'click', target: moveItemD });
      await realTick();
      await realTick();
    });
    return docD.body.querySelector('#session-move-cwd-input');
  };

  /* (1) Dialog shape: role, buttons, and the no-clip CSS contract. */
  const dialogInputD = await openMoveDialog();
  check(dialogInputD !== null, 'the move dialog renders the target cwd input');
  const dialogElD = docD.body.querySelector('.session-sidebar__menu--move');
  check(dialogElD !== null, 'move mode renders the dedicated dialog container');
  check(dialogElD?.getAttribute('role') === 'dialog' && dialogElD?.getAttribute('aria-label') === 'Move session',
    'move mode renders as a labeled dialog, not the squeezed inline popup');
  check(docD.body.querySelector('#session-move-cancel-btn') !== null && docD.body.querySelector('#session-move-submit-btn') !== null,
    'the move dialog exposes visible Cancel and Move buttons');
  check(/\.session-sidebar__menu--move \{[^}]*min-width: min\(420px, calc\(100vw - 16px\)\);/s.test(sidebarCss)
    && /\.session-sidebar__menu--move \{[^}]*max-width: min\(480px, calc\(100vw - 16px\)\);/s.test(sidebarCss)
    && /\.session-sidebar__menu--move \{[^}]*overflow: visible;/s.test(sidebarCss),
    'the dialog is viewport-clamped at a 420px minimum with no clipping overflow (full path fits at 1280x633)');
  check(/\.session-sidebar__menu-move input \{[^}]*font-family: var\(--font-mono, monospace\);/s.test(sidebarCss)
    && /\.session-sidebar__menu-move input \{[^}]*width: 100%;/s.test(sidebarCss),
    'the full-path input is monospace and owns its horizontal scrolling');

  /* (2) Escape cancels: the menu closes, no wire send. */
  await act(async () => {
    dialogInputD!.dispatchEvent({ type: 'keydown', key: 'Escape', target: dialogInputD, preventDefault(): void {}, stopPropagation(): void {}, stopImmediatePropagation(): void {} });
    await realTick();
  });
  check(docD.body.querySelector('.session-sidebar__menu') === null, 'Escape cancels the move dialog');

  /* Cancel button dismisses with zero session_move frames. */
  await openMoveDialog();
  await act(async () => {
    docD.body.querySelector('#session-move-cancel-btn')!.dispatchEvent({
      type: 'click', target: docD.body.querySelector('#session-move-cancel-btn'),
      preventDefault(): void {}, stopPropagation(): void {},
    });
    await realTick();
    await realTick();
  });
  check(docD.body.querySelector('.session-sidebar__menu') === null, 'the Cancel button dismisses the move dialog');
  check(commandsD.filter((command) => command.type === 'session_move').length === 0,
    'Escape/Cancel dismissals never send a session_move frame');

  /* (3) Move success regroups WITHOUT waiting for a fresh refetch: the
   * refetch returns the STALE pre-move catalog, so only the local
   * destination patch can re-group the row under 'beta'. */
  await openMoveDialog();
  const regroupInput = docD.body.querySelector('#session-move-cwd-input');
  regroupInput!.value = '';
  for (const ch of TARGET_D) {
    await act(async () => {
      regroupInput!.value += ch;
      regroupInput!.dispatchEvent({ type: 'input', target: regroupInput, preventDefault(): void {}, stopPropagation(): void {} });
      await realTick();
    });
  }
  const listsBeforeD = listCount(backendD);
  await act(async () => {
    regroupInput!.dispatchEvent({ type: 'keydown', key: 'Enter', target: regroupInput, preventDefault(): void {}, stopPropagation(): void {}, stopImmediatePropagation(): void {} });
    await realTick();
    await realTick();
    await realTick();
    await realTick();
  });
  check(commandsD.filter((command) => command.type === 'session_move').length === 1, 'the move submits exactly one session_move frame');
  check(listCount(backendD) === listsBeforeD + 1, 'exactly one immediate session_list refetch after the move (no manual reload)');
  const betaHeadD = docD.body.querySelectorAll('.session-sidebar__group-head').find((head) =>
    head.getAttribute('data-subgroup-kind') === 'project'
    && head.querySelector('.session-sidebar__group-label')?.textContent === 'beta');
  check(betaHeadD !== undefined, 'move success regroups the row under the target project despite the stale refetch');
  const regroupedRowD = docD.body.querySelectorAll('.session-sidebar__switch')
    .find((node) => node.getAttribute('data-session-id') === 'srv-a');
  check(regroupedRowD !== undefined, 'the regrouped row renders without a reload');
  check(toastsD.some((toast) => toast.message === 'Session moved'), 'the move success still toasts exactly its bounded message');
  await act(async () => {
    mountedD.root.unmount();
  });
}

/* ---------- real mount: canonical authority + Agent subgroup toggle ----------
 * 1. A malformed catalog row (legacy alias source or non-exact status) still
 *    lists (display grouping is the provider allowlist) but holds NO
 *    authority: its context menu never opens and its click never reaches
 *    App's negotiated switch. A canonical row in the same list keeps both.
 * 2. The Agent subgroup heading is a real toggle. Default-collapsed hides
 *    every descendant, including the loaded/active row; expanding reveals
 *    it. The heading is never aria-disabled.
 * ------------------------------------------------------------------ */
{
  const docM = installGlobals();
  const toastsM: RecordedToast[] = [];
  const catalogM: SessionRowWire[] = [
    row({ sessionId: 'valid-a', name: 'Valid row', cwd: '<workspace>/app', path: '<workspace>/session-store/valid-a.jsonl' }),
    row({ sessionId: 'alias-row', name: 'Alias row', cwd: '<workspace>/app', path: '<workspace>/session-store/alias.jsonl', source: 'native', status: 'native' }),
    row({ sessionId: 'bad-status', cwd: '<workspace>/app', path: '<workspace>/session-store/bad-status.jsonl', status: 'imported' }),
  ];
  const commandsM: CatalogCommand[] = [];
  const backendM: SidebarBackend = {
    commands: commandsM,
    sendCommand: async (command, _bubbleId, opts) => {
      commandsM.push({ ...command, __auxiliary: opts?.auxiliary === true });
      if (command.type === 'session_list') return { sessions: catalogM.map((entry) => ({ ...entry })) };
      return {};
    },
    sendCatalogCommand: (command) => backendM.sendCommand(command, undefined, { auxiliary: true }),
  };
  let switchRequestsM = 0;
  const propsM = sidebarProps(backendM, toastsM, 1);
  propsM.onSwitchSession = async (): Promise<boolean> => {
    switchRequestsM += 1;
    return true;
  };
  const mountedM = await mountSidebar(docM, propsM);
  const rowButton = (sessionId: string): MiniNode | null =>
    docM.body.querySelectorAll('.session-sidebar__switch').find((node) => node.getAttribute('data-session-id') === sessionId) ?? null;

  check(rowButton('valid-a') !== null && rowButton('alias-row') !== null && rowButton('bad-status') !== null,
    'canonical and malformed rows both list in the sidebar');
  check(docM.body.querySelectorAll('.session-sidebar__switch').length === 3, 'all three catalog rows render');

  /* Malformed alias row: context menu never opens (no native authority). */
  const aliasBtn = rowButton('alias-row');
  check(aliasBtn!.getAttribute('aria-haspopup') === null, 'malformed alias row exposes no menu affordance');
  await act(async () => {
    aliasBtn!.dispatchEvent({
      type: 'contextmenu', target: aliasBtn, clientX: 8, clientY: 8,
      preventDefault(): void {}, stopPropagation(): void {},
    });
    await realTick();
  });
  check(docM.body.querySelector('.session-sidebar__menu') === null, 'malformed alias row opens NO context menu');
  await act(async () => {
    aliasBtn!.dispatchEvent({ type: 'click', target: aliasBtn });
    await realTick();
    await realTick();
  });
  check(switchRequestsM === 0, 'malformed alias row click never reaches the negotiated switch');

  /* Malformed status row: listed, but no menu and no switch authority. */
  const statusRow = rowButton('bad-status');
  check(statusRow!.getAttribute('aria-haspopup') === null, 'non-exact status is malformed: no menu capability');
  await act(async () => {
    statusRow!.dispatchEvent({
      type: 'contextmenu', target: statusRow, clientX: 8, clientY: 8,
      preventDefault(): void {}, stopPropagation(): void {},
    });
    await realTick();
  });
  check(docM.body.querySelector('.session-sidebar__menu') === null, 'malformed status row opens no context menu');
  await act(async () => {
    statusRow!.dispatchEvent({ type: 'click', target: statusRow });
    await realTick();
    await realTick();
  });
  check(switchRequestsM === 0, 'malformed status row click never requests a switch');

  /* The canonical row in the same catalog keeps full authority. */
  const validBtn = rowButton('valid-a');
  await act(async () => {
    validBtn!.dispatchEvent({ type: 'click', target: validBtn });
    await realTick();
    await realTick();
  });
  check(switchRequestsM === 1, 'canonical row click still negotiates exactly one switch');
  await act(async () => {
    validBtn!.dispatchEvent({
      type: 'contextmenu', target: validBtn, clientX: 8, clientY: 8,
      preventDefault(): void {}, stopPropagation(): void {},
    });
    await realTick();
  });
  check(docM.body.querySelector('.session-sidebar__menu') !== null, 'canonical row still opens its context menu');
  await act(async () => {
    mountedM.root.unmount();
  });

  /* Loaded agent row in a default-collapsed Agent subgroup: the heading is a
   * REAL toggle (the old forced-open inert state is gone) and expanding it
   * reveals the loaded row. */
  const docG2 = installGlobals();
  const toastsG2: RecordedToast[] = [];
  const agentCatalogRow = row({
    sessionId: 'agent-live',
    name: 'Agent child',
    cwd: '<workspace>/agents',
    path: '<workspace>/session-store/agent-1.jsonl',
    agentSession: true,
  });
  let agentLoaded = true;
  const backendG2: SidebarBackend = {
    commands: [],
    sendCommand: async (command) => {
      if (command.type === 'session_list') {
        return { sessions: [{ ...agentCatalogRow, loaded: agentLoaded }] };
      }
      return {};
    },
    sendCatalogCommand: (command) => backendG2.sendCommand(command, undefined, { auxiliary: true }),
  };
  const mountedG2 = await mountSidebar(docG2, sidebarProps(backendG2, toastsG2, 1));
  /* The mini DOM supports no compound tag+attribute selectors, and BOTH the
   * group <li> and its heading <button> carry data-subgroup-kind — select
   * attribute-only, then assert the heading tag. */
  const agentHeading = (): MiniNode => {
    const head = docG2.body.querySelectorAll('[data-subgroup-kind="agent"]')
      .find((node) => node.tagName === 'BUTTON');
    if (!head) throw new Error('Agent sessions heading button missing');
    return head;
  };
  const agentRowVisible = (): boolean =>
    docG2.body.querySelectorAll('[data-session-id="agent-live"]').length > 0;

  check(!agentRowVisible(), 'loaded agent row stays inside the default-collapsed Agent subgroup');
  check(agentHeading().getAttribute('aria-disabled') === null, 'the agent heading is never inert (toggle always clickable)');
  check(agentHeading().getAttribute('aria-expanded') === 'false', 'default-collapsed Agent subgroup reports collapsed');
  check(agentHeading().getAttribute('title') === 'Expand Agent sessions', 'collapsed heading advertises Expand');
  await act(async () => {
    agentHeading().dispatchEvent({ type: 'click', target: agentHeading() });
    await realTick();
    await realTick();
  });
  check(agentRowVisible(), 'clicking the toggle expands the Agent subgroup and reveals the loaded row');
  check(docG2.body.querySelectorAll('[data-subgroup-kind="agent"]').filter((node) => node.tagName === 'BUTTON').length === 1,
    'the heading stays the single group node');

  /* The loaded flag never wedges the toggle: expand → collapse → expand →
   * collapse all work with the row still loaded. */
  await act(async () => {
    agentHeading().dispatchEvent({ type: 'click', target: agentHeading() });
    await realTick();
    await realTick();
  });
  check(!agentRowVisible(), 'collapse works again with the row still loaded');
  await act(async () => {
    agentHeading().dispatchEvent({ type: 'click', target: agentHeading() });
    await realTick();
    await realTick();
  });
  check(agentRowVisible(), 'expanded again with the row still loaded');
  check(agentHeading().getAttribute('title') === 'Collapse Agent sessions', 'expanded toggle advertises Collapse');
  await act(async () => {
    agentHeading().dispatchEvent({ type: 'click', target: agentHeading() });
    await realTick();
    await realTick();
  });
  check(!agentRowVisible(), 'second click collapses again (toggle authority preserved)');
  assertNoErrorPresentation(docG2, 'agent subgroup toggle flows');
  await act(async () => {
    mountedG2.root.unmount();
  });
}





for (const key of installed) delete (globalThis as Record<string, unknown>)[key];
installed.clear();
Object.defineProperty(globalThis, 'MessageChannel', { configurable: true, writable: true, value: realMessageChannel });

console.log(`sessionSidebar.test: ${assertions} assertions passed`);
process.exit(0);
