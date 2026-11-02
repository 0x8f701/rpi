#!/usr/bin/env node
// Bounded fork-candidate paging contract for the Session panel and the
// sidebar fork-from menu. get_fork_messages returns newest-first bounded
// pages; both surfaces append near the bottom of a bounded scroll container
// without a selectable pseudo "Load older" row.
//
// Run via `npm run build` (esbuild + node).
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  forkControlsEnabled,
  mergeForkMessagePages,
  nextForkEntrySelection,
  ownsForkPageResponse,
  parseForkMessagesPage,
} from '../src/panels/SessionPanel';
import { ownsSavedForkPageResponse, shouldLoadOlderForkFromFocus, shouldLoadOlderForkFromScroll } from '../src/panels/SessionSidebar';
import type { ForkMessageWire } from '../src/panels/SessionPanel';

let assertions = 0;
const check = (condition: unknown, message: string): void => {
  assert.ok(condition, message);
  assertions += 1;
};

const message = (entryId: string, text?: string): ForkMessageWire => ({
  entryId,
  text: text ?? `prompt ${entryId}`,
});

/* ---------- parseForkMessagesPage ---------- */

{
  const page = parseForkMessagesPage({
    messages: [
      { entryId: 'e1', text: 'first line preview' },
      { entryId: 'e0', text: 'older' },
      'not-an-object',
      { entryId: 5, text: 'bad' },
      { entryId: 'e1', text: 'duplicate dropped' },
    ],
    nextCursor: 'e0',
    truncated: true,
    total: 57,
    extraKey: { ignored: true },
  });
  check(page.messages.length === 2, 'valid rows parse and duplicates drop');
  check(page.messages[0]?.entryId === 'first line preview' ? false : page.messages[0]?.entryId === 'e1', 'first row identity kept');
  check(page.messages[0]?.text === 'first line preview', 'first row text kept');
  check(page.nextCursor === 'e0', 'nextCursor parsed');
  check(page.truncated === true, 'truncated parsed');
  check(page.total === 57, 'total parsed');
}
{
  const page = parseForkMessagesPage({ messages: [] });
  check(page.messages.length === 0, 'valid empty page parses');
  check(page.truncated === false, 'empty page is not truncated');
  check(page.nextCursor === undefined, 'empty page has no cursor');
  check(page.total === undefined, 'empty page has no total');
}
for (const response of [null, undefined, 42, 'text', {}, { messages: 'no' }, { messages: [null] }]) {
  const page = parseForkMessagesPage(response);
  check(Array.isArray(page.messages), `malformed response ${JSON.stringify(response)} degrades to an array`);
}
{
  const page = parseForkMessagesPage({ messages: [{ entryId: 'e9', text: 't' }], nextCursor: 'e9' });
  check(page.truncated === true, 'nextCursor presence implies truncation');
}

/* ---------- mergeForkMessagePages: disjoint append, dedupe, identity ---------- */

{
  const page0 = [message('e4'), message('e3'), message('e2')];
  const page1 = [message('e2'), message('e1'), message('e0')];
  const merged = mergeForkMessagePages(page0, page1);
  check(merged.map((m) => m.entryId).join('|') === 'e4|e3|e2|e1|e0', 'older page appends without duplicates');
  check(mergeForkMessagePages(page0, []).length === 3, 'empty older page keeps identity by reference');
  check(mergeForkMessagePages(page0, [message('e3'), message('e1')]).map((m) => m.entryId).join('|') === 'e4|e3|e2|e1', 'partial overlap merges only new rows');
  check(mergeForkMessagePages(page0, [message('e4')]) === page0, 'fully-overlapping page is a no-op by reference');
}

/* ---------- selection stickiness ---------- */

{
  check(nextForkEntrySelection([message('e9'), message('e8')], '') === 'e9', 'unset selection defaults to newest-first head');
  check(nextForkEntrySelection([message('e9'), message('e8')], 'e8') === 'e8', 'sticky selection inside the loaded pages is retained');
  check(nextForkEntrySelection([message('e9')], 'e0') === 'e0', 'sticky selection from an older page survives a page that omits it');
  check(nextForkEntrySelection([], '') === null, 'an empty page yields no selection');
  check(nextForkEntrySelection([], 'e0') === 'e0', 'an empty page retains a sticky selection');
}

/* ---------- enablement: truncated/nonempty enables; zero-user disables ---------- */

{
  check(forkControlsEnabled([message('e1')], '') === true, 'nonempty page enables Fork');
  check(forkControlsEnabled([], 'e0') === true, 'sticky selected entry keeps Fork enabled across pages');
  check(forkControlsEnabled([], '') === false, 'empty page with no selection disables Fork');
}

/* ---------- stale response ownership: exact session/cursor/generation ---------- */

{
  check(ownsForkPageResponse('sess-a', 'sess-a', 'c1', 'c1', 4, 4, 2, 2), 'exact page owner is accepted');
  check(!ownsForkPageResponse('sess-a', 'sess-b', 'c1', 'c1', 4, 4, 2, 2), 'different active session rejects the page');
  check(!ownsForkPageResponse('sess-a', 'sess-a', 'c1', 'c2', 4, 4, 2, 2), 'replaced cursor rejects the page');
  check(!ownsForkPageResponse('sess-a', 'sess-a', 'c1', 'c1', 4, 5, 2, 2), 'newer page request rejects the stale response');
  check(!ownsForkPageResponse('sess-a', 'sess-a', 'c1', 'c1', 4, 4, 2, 3), 'remount rejects the old response');
}
{
  const current = {
    row: { source: 'rpi', sessionId: 'saved-a', path: '<workspace>/sessions/a.jsonl' },
    x: 0,
    y: 0,
    mode: 'fork-from' as const,
    focus: 0,
    forkMessages: [message('e2')],
    forkNextCursor: 'e2',
    renameValue: 'saved-a',
  };
  const exactKey = 'rpi::saved-a::<workspace>/sessions/a.jsonl';
  check(ownsSavedForkPageResponse(current, exactKey, 'fork-from', 'e2', 4, 4, 2, 2), 'saved page exact owner is accepted');
  check(!ownsSavedForkPageResponse(current, 'rpi::saved-b::<workspace>/sessions/b.jsonl', 'fork-from', 'e2', 4, 4, 2, 2), 'saved page rejects another row');
  check(!ownsSavedForkPageResponse(current, exactKey, 'fork-from', 'e1', 4, 4, 2, 2), 'saved page rejects a replaced cursor');
  check(!ownsSavedForkPageResponse({ ...current, mode: 'root' }, exactKey, 'fork-from', 'e2', 4, 4, 2, 2), 'saved page rejects a replaced menu mode');
  check(!ownsSavedForkPageResponse(current, exactKey, 'fork-from', 'e2', 4, 5, 2, 2), 'saved page rejects an older request generation');
  check(!ownsSavedForkPageResponse(current, exactKey, 'fork-from', 'e2', 4, 4, 2, 3), 'saved page rejects an older mount generation');
  check(!ownsSavedForkPageResponse(null, exactKey, 'fork-from', 'e2', 4, 4, 2, 2), 'closed menu rejects the saved page');
}

/* ---------- sidebar scroll trigger: near-bottom, terminal, single-flight ---------- */

check(shouldLoadOlderForkFromScroll(452, 500, 1000, true, false, false), 'near-bottom scroll requests the next saved fork page');
check(!shouldLoadOlderForkFromScroll(400, 500, 1000, true, false, false), 'scroll outside the 48px threshold does not request');
check(!shouldLoadOlderForkFromScroll(452, 500, 1000, false, false, false), 'terminal cursor state never requests');
check(!shouldLoadOlderForkFromScroll(452, 500, 1000, true, true, false), 'single-flight guard suppresses concurrent scroll requests');
check(!shouldLoadOlderForkFromScroll(452, 500, 1000, true, false, true), 'latched page error suppresses automatic retry loops');

/* ---------- sidebar keyboard trigger: last candidate, terminal, single-flight ---------- */

check(shouldLoadOlderForkFromFocus(2, 3, true, false, false), 'keyboard focus reaching the LAST candidate requests the next saved fork page');
check(!shouldLoadOlderForkFromFocus(1, 3, true, false, false), 'keyboard focus before the last candidate never requests');
check(!shouldLoadOlderForkFromFocus(2, 3, false, false, false), 'terminal cursor state never requests from keyboard focus');
check(!shouldLoadOlderForkFromFocus(2, 3, true, true, false), 'single-flight guard suppresses concurrent keyboard page requests');
check(!shouldLoadOlderForkFromFocus(2, 3, true, false, true), 'latched page error suppresses automatic keyboard retry loops');
check(!shouldLoadOlderForkFromFocus(2, 0, true, false, false), 'an empty candidate list never triggers keyboard paging');

/* ---------- backend paging invariant contract ----------
 * The paginator's wire contract: a `get_fork_messages` page is terminal
 * exactly when `nextCursor` is absent, and the cursor is opaque server
 * state. The sidebar therefore NEVER fabricates a cursor: every paged
 * request re-sends the exact captured `forkNextCursor`, and BOTH automatic
 * triggers (near-bottom scroll, last-candidate keyboard focus) are gated on
 * its presence — an absent cursor can never produce a request. This is the
 * explicit contract the backend paginator and the sidebar share. */

check(!shouldLoadOlderForkFromScroll(452, 500, 1000, false, false, false), 'invariant: absent nextCursor disables the scroll trigger');
check(!shouldLoadOlderForkFromFocus(2, 3, false, false, false), 'invariant: absent nextCursor disables the keyboard trigger');
check(shouldLoadOlderForkFromScroll(452, 500, 1000, true, false, false)
  && shouldLoadOlderForkFromFocus(2, 3, true, false, false),
  'invariant: a present nextCursor is the sole authority that arms both triggers');

/* ---------- SessionPanel source contract ---------- */

{
  const here = dirname(fileURLToPath(import.meta.url));
  const panel = readFileSync(join(here, '../src/panels/SessionPanel.tsx'), 'utf8');
  const loadStart = panel.indexOf('const load = useCallback');
  const loadEnd = panel.indexOf('loadRef.current = load;');
  const loadBody = panel.slice(loadStart, loadEnd);
  check(loadStart !== -1 && loadEnd !== -1, 'SessionPanel load function is locatable');
  check(!loadBody.includes('session_list'), 'SessionPanel load never sends session_list');
  check(loadBody.includes("sendCommand({ type: 'get_fork_messages', sessionId })"), 'load requests the fork page with explicit owner sessionId');
  check(!loadBody.includes('cursor:'), 'initial load fetches the first page only');
  check(loadBody.includes('parseForkMessagesPage('), 'load parses the bounded page helper');
  check(!panel.includes('refreshSessionList(parseForkMessagesPage'), 'guard: helpers stay independent');
  check(panel.includes('forkControlsEnabled(forkMessages, forkEntry)'), 'fork controls disable only on empty page without sticky entry');
  check(!panel.includes('id="session-fork-older-btn"'), 'panel removes the manual Load older control');
  check(panel.includes('id="session-fork-picker-btn"') && panel.includes('id="session-fork-list"'), 'panel renders a compact picker backed by the bounded fork list');
  check(panel.includes('hidden={!forkPickerOpen}') && panel.includes('onScroll={onForkListScroll}'), 'candidate rows stay collapsed until requested and retain scroll paging');
  check(panel.includes("sendCommand({ type: 'get_fork_messages', cursor, sessionId })"), 'scroll paging carries the captured explicit owner sessionId');
  check(panel.includes('mergeForkMessagePages(old, page.messages)'), 'scroll paging merges into the loaded page with dedupe');
  check(panel.includes('ownsForkPageResponse('), 'scroll paging validates exact session, cursor, request, and mount ownership');
  check(panel.includes('forkOlderError &&') && panel.includes('id="session-fork-retry-btn"'), 'panel latches paging errors and exposes explicit retry');
  check(panel.includes('forkOlderRequestRef.current += 1'), 'first-page replacement invalidates older-page responses');
}

/* ---------- SessionSidebar source contract ---------- */

{
  const here = dirname(fileURLToPath(import.meta.url));
  const sidebar = readFileSync(join(here, '../src/panels/SessionSidebar.tsx'), 'utf8');
  check(sidebar.includes("sendCommand({ type: 'get_fork_messages', sessionPath: row.path })"), 'sidebar fork-from requests the saved-session page');
  check(sidebar.includes("sessionPath: row.path, cursor }"), 'invariant: paged sidebar requests carry only the server-captured forkNextCursor cursor');
  check(sidebar.includes('mergeForkMessagePages(current.forkMessages, page.messages)'), 'older menu pages append with dedupe');
  check(!sidebar.includes('Load older…') && !sidebar.includes('fork-from-older'), 'sidebar has no selectable Load older pseudo row');
  check(sidebar.includes('const count = menu.forkMessages.length;'), 'keyboard focus counts only actual fork candidates');
  check(sidebar.includes('className="session-sidebar__fork-scroll"'), 'fork candidates render in a dedicated bounded scroll container');
  const css = readFileSync(join(here, '../src/styles.css'), 'utf8');
  check(/\.session-sidebar__fork-scroll\s*\{[^}]*max-height:[^}]*overflow-y:\s*auto;/s.test(css), 'fork picker owns a bounded vertical scroller');
  check(sidebar.includes('onScroll={onForkFromScroll}'), 'fork scroll invokes automatic near-bottom paging');
  check(sidebar.includes('shouldLoadOlderForkFromScroll('), 'scroll paging uses terminal and single-flight predicate');
  check(sidebar.includes("forkScrollRef.current?.contains(event.target as Node)"), 'wheel/touch inside the fork scroller never closes the menu');
  check(sidebar.includes("if (requestGen === forkMenuRequestRef.current) forkMenuLoadingOlderRef.current = false"), 'only the owning request clears single-flight state');
  check(sidebar.includes("page.messages.length === 0"), 'empty initial page keeps the no-forkable-messages guard');
  check(sidebar.includes('forkFailedCursor: cursor'), 'business errors latch the failed cursor as RENDERED menu state');
  check(sidebar.includes('session-sidebar__fork-status'), 'latched failure renders a bounded status row');
  check(sidebar.includes("`Couldn't load older messages (cursor ${menu.forkFailedCursor})`"), 'the status text carries the exact failed cursor');
  check(sidebar.includes("role=\"status\""), 'the failure status is announced as a live region');
  check(sidebar.includes('aria-label={`Retry loading older messages (cursor ${menu.forkFailedCursor})`}'), 'the Retry row names the exact failed cursor');
  check(sidebar.includes('onClick={retryForkOlderFrom}'), 'the Retry row activates through the shared retry path');
  check(sidebar.includes('retryForkOlderFrom();'), 'keyboard Enter/Space on the Retry row re-runs the same single-flight load');
  check(sidebar.includes('forkFailedCursor === menu.forkNextCursor'), 'both paging triggers gate on the exact latched cursor');
  check(sidebar.includes('shouldLoadOlderForkFromFocus('), 'last-candidate keyboard focus pages through the shared predicate');
  check(sidebar.includes('focused.scrollIntoView({ block: \'nearest\' })'), 'fork-from roving focus keeps the focused candidate scrolled into view');
  check(sidebar.includes("menu.focus === menu.forkMessages.length ? ' is-focused' : ''"), 'the Retry row participates in the roving focus highlighting');
  check(sidebar.includes('menuStateRef.current'), 'saved page success and error paths read exact current menu ownership');
  check(sidebar.includes('forkMenuRequestRef.current'), 'saved page requests use an invalidatable generation');
  check(sidebar.includes('forkMenuLoadingOlderRef.current'), 'saved scroll paging prevents duplicate inflight requests');
  check(sidebar.includes("current, rowKey, 'root', undefined"), 'initial saved page applies only to the still-open queried root menu');
  check(sidebar.includes("current, rowKey, 'fork-from', cursor"), 'older saved page applies only to the queried row and cursor');
  check(sidebar.includes('forkNextCursor: undefined'), 'Escape resets the fork cursor before returning to root mode');
}

console.log(`sessionForkCandidates.test: ${assertions} assertions passed`);