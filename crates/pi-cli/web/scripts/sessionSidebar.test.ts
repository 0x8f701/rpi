import assert from 'node:assert/strict';
import {
  groupSessions,
  isNoiseProjectName,
  matchSessionSearch,
  projectFromSessionPath,
  projectNameOf,
  providerOf,
  sessionTitle,
  visibleSidebarSessions,
} from '../src/panels/SessionSidebar';
import { isLoadedCurrentSession } from '../src/panels/SessionPanel';
import type { SessionRowWire } from '../src/panels/SessionPanel';

let assertions = 0;
const check = (condition: unknown, message: string): void => {
  assert.ok(condition, message);
  assertions += 1;
};

const row = (overrides: Partial<SessionRowWire>): SessionRowWire => ({
  sessionId: 'session',
  path: '/sessions/native.jsonl',
  source: 'pi',
  status: 'native',
  loaded: false,
  ...overrides,
});

/* ---------- provider normalization ---------- */

check(providerOf(row({ source: 'pi' })) === 'rpi', 'native pi rows map to rpi');
check(providerOf(row({ source: 'native' })) === 'rpi', 'native alias maps to rpi');
check(providerOf(row({ source: 'primary' })) === 'rpi', 'primary alias maps to rpi');
check(providerOf(row({ source: 'omp' })) === 'OMP', 'omp rows map to OMP');
check(providerOf(row({ source: 'codex' })) === 'Codex', 'codex rows map to Codex');
check(providerOf(row({ source: 'grok' })) === 'Grok', 'grok rows map to Grok');
check(providerOf(row({ source: 'grok/hyper' })) === 'Grok', 'grok/hyper maps to Grok');
check(providerOf(row({ source: '' })) === null, 'empty source returns null');
check(providerOf(row({ source: 'unknown-agent' })) === null, 'unknown source returns null');
check(providerOf(row({ source: 'claude' })) === null, 'claude source returns null (not one of the four)');
check(providerOf(row({ source: 'droid' })) === null, 'droid source returns null (not one of the four)');

/* ---------- project path recovery (unchanged) ---------- */

check(projectFromSessionPath('/sessions/--workspace-projects-parth-generic-v1--/one.jsonl') === 'parth-generic-v1', 'encoded native project tail is recovered');
check(projectFromSessionPath('/foreign/rollout-codex-manager.jsonl') === 'rollout-codex-manager', 'foreign file basename is retained');
check(projectNameOf(row({ cwd: '/workspace/rpi' })) === 'rpi', 'authoritative cwd names the project');

/* ---------- session title (unchanged) ---------- */

check(sessionTitle(row({ name: '<b>named</b>', summary: 'ignored' })) === '<b>named</b>', 'session names remain literal text-node content');

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

const grouped = groupSessions([
  row({ sessionId: 'native-a', cwd: '/workspace/a', path: '/native/a.jsonl' }),
  row({ sessionId: 'native-b', cwd: '/workspace/b', path: '/native/b.jsonl' }),
  row({ sessionId: 'omp-a', source: 'omp', cwd: '/workspace/a', path: '/foreign/omp-a.jsonl' }),
  row({ sessionId: 'codex-a', source: 'codex', cwd: '/workspace/a', path: '/foreign/codex-a.jsonl' }),
  row({ sessionId: 'grok-b', source: 'grok', cwd: '/workspace/b', path: '/foreign/grok-b.jsonl' }),
]);
check(grouped.map((node) => node.key).join('|') === 'provider::rpi|provider::Codex|provider::Grok|provider::OMP', 'four provider roots in fixed order');
check(grouped.every((node) => node.kind === 'provider'), 'all roots are provider kind');
check(grouped[0]?.label === 'rpi', 'first root is rpi');
check(grouped[0]?.children[0]?.sessions[0]?.sessionId === 'native-a', 'rpi nests project a');
check(grouped[0]?.children[1]?.sessions[0]?.sessionId === 'native-b', 'rpi nests project b');
check(grouped[0]?.children[0]?.kind === 'project', 'child groups are project kind');
check(grouped[1]?.label === 'Codex', 'second root is Codex');
check(grouped[1]?.children[0]?.sessions[0]?.sessionId === 'codex-a', 'Codex nests by project');
check(grouped[2]?.label === 'Grok', 'third root is Grok');
check(grouped[2]?.children[0]?.sessions[0]?.sessionId === 'grok-b', 'Grok nests by project');
check(grouped[3]?.label === 'OMP', 'fourth root is OMP');
check(grouped[3]?.children[0]?.sessions[0]?.sessionId === 'omp-a', 'OMP nests by project');

/* ---------- noise project sessions go directly under provider ---------- */

const noiseGrouped = groupSessions([
  row({ sessionId: 'tmp-session', cwd: '/tmp', path: '/native/tmp.jsonl' }),
  row({ sessionId: 'uuid-session', cwd: '/a1b2c3d4-e5f6-7890-abcd-ef1234567890', path: '/native/uuid.jsonl' }),
  row({ sessionId: 'real-session', cwd: '/workspace/rpi', path: '/native/rpi.jsonl' }),
]);
const rpiRoot = noiseGrouped.find((n) => n.label === 'rpi');
check(rpiRoot !== undefined, 'rpi root exists');
check(rpiRoot?.children.length === 1, 'rpi has one project child (rpi only)');
check(rpiRoot?.children[0]?.label === 'rpi', 'rpi child is rpi');
check(rpiRoot?.sessions.length === 2, 'noise sessions listed directly under rpi');
check(rpiRoot?.sessions.some((r) => r.sessionId === 'tmp-session'), 'tmp session under rpi directly');
check(rpiRoot?.sessions.some((r) => r.sessionId === 'uuid-session'), 'uuid session under rpi directly');
check(!rpiRoot?.children.some((c) => isNoiseProjectName(c.label)), 'no noise project subgroups');

/* ---------- no tmp/UUID/source top-level groups ---------- */

const allLabels = noiseGrouped.map((n) => n.label);
check(allLabels.every((l) => ['rpi', 'Codex', 'Grok', 'OMP'].includes(l)), 'top-level groups are only the four providers');
check(!allLabels.includes('tmp'), 'tmp is not a top-level group');
check(!allLabels.includes('source'), 'source is not a top-level group');

/* ---------- unknown-source rows are filtered from the sidebar ---------- */

const filteredGrouped = groupSessions([
  row({ sessionId: 'native-1', cwd: '/workspace/a', path: '/native/a.jsonl' }),
  row({ sessionId: 'claude-1', source: 'claude', cwd: '/workspace/b', path: '/foreign/claude.jsonl' }),
  row({ sessionId: 'droid-1', source: 'droid', cwd: '/workspace/c', path: '/foreign/droid.jsonl' }),
  row({ sessionId: 'unknown-1', source: 'dirty-source', cwd: '/workspace/d', path: '/foreign/dirty.jsonl' }),
]);
const filteredLabels = filteredGrouped.map((n) => n.label);
check(filteredLabels.length === 1, 'only one provider root (rpi) when other sources are unknown');
check(filteredLabels[0] === 'rpi', 'rpi is the only root');
check(filteredGrouped[0]?.children[0]?.sessions[0]?.sessionId === 'native-1', 'native row retained');
check(!filteredLabels.includes('claude'), 'claude is not a provider group');
check(!filteredLabels.includes('droid'), 'droid is not a provider group');
check(!filteredLabels.includes('dirty-source'), 'dirty source is not a provider group');

/* ---------- search matching ---------- */

const searchRow = row({ sessionId: 'abc-123', name: 'My Cool Session', cwd: '/workspace/rpi', source: 'pi' });
check(matchSessionSearch(searchRow, 'cool'), 'matches by session title');
check(matchSessionSearch(searchRow, 'rpi'), 'matches by project basename');
check(matchSessionSearch(row({ sessionId: 'label-only', source: 'pi', cwd: '/none' }), 'rpi'), 'matches by provider label');
check(matchSessionSearch(searchRow, 'pi'), 'matches by wire source');
check(matchSessionSearch(searchRow, 'abc-123'), 'matches by session id');
check(!matchSessionSearch(searchRow, 'nonexistent'), 'no match returns false');
check(matchSessionSearch(searchRow, ''), 'empty query matches all');

const codexSearchRow = row({ sessionId: 'codex-1', source: 'codex', cwd: '/workspace/myapp' });
check(matchSessionSearch(codexSearchRow, 'codex'), 'matches by wire source');
check(matchSessionSearch(codexSearchRow, 'myapp'), 'matches by project basename');

/* ---------- temporary-workspace rows: default hide, search/active/loaded restore ---------- */

const tempRow = row({ sessionId: 'temp-1', name: undefined, cwd: '<tmp>/pi-noise', source: 'pi', temporary: true });
const normalRow = row({ sessionId: 'normal-1', cwd: '<workspace>/rpi', source: 'pi', temporary: false });

const defaultVisible = visibleSidebarSessions([tempRow, normalRow], '', null);
check(defaultVisible.length === 1, 'temporary rows are hidden by default');
check(defaultVisible[0]?.sessionId === 'normal-1', 'regular rows stay visible by default');
check(visibleSidebarSessions([tempRow, normalRow], '', 'temp-1').some((r) => r.sessionId === 'temp-1'), 'active temporary session stays visible');
check(visibleSidebarSessions([{ ...tempRow, loaded: true }, normalRow], '', null).some((r) => r.sessionId === 'temp-1'), 'loaded temporary session stays visible');
check(visibleSidebarSessions([tempRow, normalRow], 'temp-1', null).some((r) => r.sessionId === 'temp-1'), 'temporary row found by session id search');
const titledTemp = row({ sessionId: 'temp-2', name: 'quick experiment', cwd: '<tmp>/x', source: 'pi', temporary: true });
check(visibleSidebarSessions([titledTemp], 'experiment', null).some((r) => r.sessionId === 'temp-2'), 'temporary row found by title search');
const searched = visibleSidebarSessions([tempRow, normalRow], 'normal-1', null);
check(searched.length === 1 && searched[0]?.sessionId === 'normal-1', 'non-matching temporary rows stay hidden while searching');
const activeKept = visibleSidebarSessions([tempRow, normalRow], 'nomatch', 'normal-1');
check(activeKept.length === 1 && activeKept[0]?.sessionId === 'normal-1', 'active row is appended when it does not match the query');
check(visibleSidebarSessions([tempRow], '', null).length === 0, 'only a temporary row in the catalog hides entirely by default');

/* ---------- active session row selection (unchanged) ---------- */

check(isLoadedCurrentSession(row({ sessionId: 'active', loaded: false }), 'active', false), 'active row remains selected while its loaded overlay refreshes');
check(!isLoadedCurrentSession(row({ sessionId: 'other', loaded: true }), 'active', false), 'different session id is never active');
check(isLoadedCurrentSession(row({ sessionId: 'active', loaded: true }), 'active', true), 'loaded active identity remains selected after convergence');
check(!isLoadedCurrentSession(row({ sessionId: 'active', loaded: false }), 'active', true), 'duplicate stale identity loses selection when the loaded active identity exists');

console.log(`sessionSidebar.test: ${assertions} assertions passed`);