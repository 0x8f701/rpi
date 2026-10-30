#!/usr/bin/env node
// Session catalog refresh contract for the production `refreshSessionList`
// helper used by the SessionSidebar catalog. The Session panel no longer
// fetches or renders the saved-session catalog — the sidebar is the single
// catalog surface — so this test also guards the panel source against
// session_list/refreshSessionList use.
//
// Run via `npm run build` (esbuild + node).

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  isLiveSessionLoad,
  refreshSessionList,
  shouldToastSessionActionError,
} from '../src/panels/SessionPanel';
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
  name: undefined,
  summary: undefined,
  cwd: '/workspace/rpi',
  displayTime: undefined,
  messageCount: undefined,
  loaded: false,
  temporary: false,
  ...overrides,
});

/* ---------- 0. Session panel no longer fetches or renders the catalog ---------- */
{
  const here = dirname(fileURLToPath(import.meta.url));
  const panel = readFileSync(join(here, '../src/panels/SessionPanel.tsx'), 'utf8');
  // The preserved refreshSessionList helper legitimately mentions session_list
  // (it is the Sidebar catalog contract), so the guards target the panel's
  // own load path and call pattern, not the helper definition.
  const loadStart = panel.indexOf('const load = useCallback');
  const loadEnd = panel.indexOf('loadRef.current = load;');
  const loadBody = panel.slice(loadStart, loadEnd);
  check(loadStart !== -1 && loadEnd !== -1, 'SessionPanel load function is locatable');
  check(!loadBody.includes('session_list'), 'SessionPanel load never sends session_list');
  check(!loadBody.includes('refreshSessionList'), 'SessionPanel load never calls refreshSessionList');
  check(!panel.includes('refreshSessionList(sendCommand'), 'SessionPanel never calls refreshSessionList anywhere');
  check(!panel.includes('Saved sessions'), 'SessionPanel source has no Saved sessions section');
  check(!panel.includes('session-row__switch'), 'SessionPanel source has no panel switch rows');
  check(!panel.includes('onSwitchSession'), 'SessionPanel source has no onSwitchSession prop');
}

export const TRANSPORT_ERROR_MESSAGES = [
  'connection closed (code 1006)',
  'connection closed (code 1008: policy violation)',
  'connection failed: refused',
  'connection unresponsive after 30s',
  'not connected',
  'connection replaced',
  'host switched',
  'send failed: socket',
] as const;

/* ---------- 1. A refresh sends a new session_list request ---------- */
{
  let calls = 0;
  let lastCommand: Record<string, unknown> | null = null;
  const sendCommand = async (command: Record<string, unknown>): Promise<unknown> => {
    calls += 1;
    lastCommand = command;
    return { sessions: [row({ sessionId: 'fresh-1' })] };
  };
  await refreshSessionList(sendCommand, []);
  check(calls === 1, 'refresh triggers exactly one sendCommand call');
  check(lastCommand?.type === 'session_list', 'request type is session_list');
  check(lastCommand?.scope === 'all_projects', 'request scope is all_projects (disk-wide)');
}

/* ---------- 2. New disk sessions appear (rows replaced on success) ---------- */
{
  const oldRows = [row({ sessionId: 'old-1' }), row({ sessionId: 'old-2' })];
  const newRows = [
    row({ sessionId: 'old-1' }),
    row({ sessionId: 'new-from-disk' }),
  ];
  const sendCommand = async (): Promise<unknown> => ({ sessions: newRows });
  const result = await refreshSessionList(sendCommand, oldRows);
  check(result.error === null, 'no error on success');
  check(result.sessions.length === 2, 'returned rows replace old list');
  check(
    result.sessions.some((r) => r.sessionId === 'new-from-disk'),
    'new disk session appears after refresh',
  );
  check(
    !result.sessions.some((r) => r.sessionId === 'old-2'),
    'stale session no longer present after refresh',
  );
}

/* ---------- 3. Error preserves old list (not destroyed) ---------- */
{
  const oldRows = [row({ sessionId: 'survivor-1' }), row({ sessionId: 'survivor-2' })];
  const sendCommand = async (): Promise<unknown> => {
    throw new Error('backend unavailable');
  };
  const result = await refreshSessionList(sendCommand, oldRows);
  check(result.error !== null, 'error is surfaced');
  check(result.error === 'backend unavailable', 'error message is preserved');
  check(result.sessions.length === 2, 'old list length preserved on error');
  check(
    result.sessions.every((r) => oldRows.some((o) => o.sessionId === r.sessionId)),
    'old rows are the exact same objects on error',
  );
  check(
    result.sessions === oldRows,
    'old array reference is returned directly on error (no copy)',
  );
}

/* ---------- 4. Invalid responses preserve the known-good catalog ---------- */
for (const response of [{}, { sessions: 'not-an-array' }]) {
  const oldRows = [row({ sessionId: 'known-good' })];
  const sendCommand = async (): Promise<unknown> => response;
  const result = await refreshSessionList(sendCommand, oldRows);
  check(result.error === 'session_list response is missing a sessions array', 'invalid response surfaces a protocol error');
  check(result.sessions === oldRows, 'invalid response preserves the known-good array');
}

/* ---------- 5. A valid empty catalog replaces old rows ---------- */
{
  const sendCommand = async (): Promise<unknown> => ({ sessions: [] });
  const result = await refreshSessionList(sendCommand, [row({ sessionId: 'removed' })]);
  check(result.error === null, 'valid empty catalog succeeds');
  check(result.sessions.length === 0, 'valid empty catalog replaces old rows');
}

/* ---------- 6. Multiple sequential refreshes each trigger a request ---------- */
{
  let calls = 0;
  const sendCommand = async (): Promise<unknown> => {
    calls += 1;
    return { sessions: [row({ sessionId: `s-${calls}` })] };
  };
  await refreshSessionList(sendCommand, []);
  await refreshSessionList(sendCommand, []);
  await refreshSessionList(sendCommand, []);
  check(calls === 3, 'three refreshes trigger three independent requests');
}

/* ---------- 7. Only the newest live load may commit ---------- */
{
  check(isLiveSessionLoad(3, 3, 1, 1) === true, 'matching request and mounted gens are live');
  check(isLiveSessionLoad(2, 3, 1, 1) === false, 'older request gen is stale even on the same instance');
  check(isLiveSessionLoad(3, 3, 1, 2) === false, 'replaced instance gen is stale even for the newest request');
  check(isLiveSessionLoad(1, 2, 1, 2) === false, 'older request on a replaced instance is stale');
  check(isLiveSessionLoad(4, 4, 2, 2) === true, 'newest request on the current instance is live');
}

/* ---------- 8. Transport refresh failures preserve the known-good catalog ---------- */
for (const message of TRANSPORT_ERROR_MESSAGES) {
  const oldRows = [row({ sessionId: 'known-good' })];
  const cause = new Error(message);
  const result = await refreshSessionList(async () => {
    throw cause;
  }, oldRows);
  check(result.sessions === oldRows, `${message}: refresh preserves the known-good catalog`);
  check(result.errorCause === cause, `${message}: refresh preserves typed error identity`);
}

/* ---------- 9. Panel toast ownership: App-surfaced failures are never re-toasted ---------- */
{
  check(
    shouldToastSessionActionError(new Error('backend unavailable')),
    'plain business failure still needs a panel toast',
  );
  check(
    shouldToastSessionActionError(Object.assign(new Error('connection closed (code 1006)'), { rpc: true })),
    'transport-looking rpc failure still needs a panel toast when unowned',
  );
  check(
    !shouldToastSessionActionError(Object.assign(new Error('command new_session failed: denied'), { toastOwned: true })),
    'App-owned RPC rejection is never re-toasted by the panel',
  );
  check(shouldToastSessionActionError(null) && shouldToastSessionActionError('boom'), 'non-object rejections stay panel-owned');
}

console.log(`\nsessionRefresh.test: ${assertions} assertions passed (${assertions - 7} helper + 7 panel-source guards)`);