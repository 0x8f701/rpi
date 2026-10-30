#!/usr/bin/env node
// Session sidebar context-menu helpers — pure position/focus/item/undo
// predicates plus sessionExport download safety with injected fetch/DOM/URL
// shims. Run via `npm run build` (esbuild + node).
import assert from 'node:assert/strict';
import {
  contextMenuItems,
  copySessionContext,
  parseSessionDeleteResponse,
  firstEnabledMenuFocus,
  flipMenuPosition,
  forkToastMessage,
  hasSessionContextMenu,
  isSessionActionDisabled,
  isNativeSession,
  moveSession,
  nextMenuFocus,
  parseSessionCopyContextResponse,
  SESSION_ARCHIVE_UNDO_MS,
  SESSION_COPY_CONTEXT_FORMAT,
  MOVE_SESSION_TITLE,
  SESSION_MENU_LONG_PRESS_MS,
  sessionMoveToast,
  parseSessionMoveResponse,
  shouldOpenLongPress,
  shouldToastSessionActionError,
  DELETE_SWITCH_AWAY_TITLE,
} from '../src/panels/SessionSidebar';
import { sendPathCompleteRequest } from '../src/panels/SessionPanel';
import {
  downloadSessionExport,
  MAX_SESSION_EXPORT_BYTES,
  sessionExportFileName,
  sessionExportUrl,
  type SessionExportAnchor,
  type SessionExportDocument,
  type SessionExportResponseLike,
} from '../src/sessionExport';
import { withResolvers } from '../src/withResolvers';

let assertions = 0;
const check = (condition: unknown, message: string): void => {
  assert.ok(condition, message);
  assertions += 1;
};

/* Native mutation authority is stricter than provider membership: only an
 * exact `rpi` source with exact `native` status may mutate in place. */
check(isNativeSession({ status: 'native', source: 'rpi' }), 'canonical rpi/native row is native');
check(!isNativeSession({ status: 'native', source: 'native' }), 'legacy source alias native is malformed: not native');
check(!isNativeSession({ status: 'native', source: 'primary' }), 'legacy primary alias is malformed: not native');
check(!isNativeSession({ source: 'pi' }), 'pi source imports instead of mutating in place');
check(!isNativeSession({ status: 'native', source: 'pi' }), 'pi source with native status imports instead of mutating in place');
check(!isNativeSession({ status: 'foreign', source: 'codex' }), 'codex is not native');
check(!isNativeSession({ status: 'native', source: 'omp' }), 'canonical foreign provider is not native');
check(!isNativeSession({ status: 'foreign', source: 'grok' }), 'canonical foreign grok is not native');
check(!isNativeSession({ status: 'native', source: 'grok/hyper' }), 'legacy grok/hyper alias is malformed: no native authority');
check(!isNativeSession({ status: 'native', source: '' }), 'empty source is malformed: no implicit native default');
check(!isNativeSession({ status: 'native' }), 'missing source is malformed: no implicit native default');
check(!isNativeSession({ status: 'native', source: 'claude' }), 'unmappable source is malformed even with a native status');
check(!isNativeSession({ status: 'imported', source: 'rpi' }), 'unknown status is malformed, never defaulted to native');
check(!isNativeSession({ source: 'rpi', status: '  native  ' }), 'status must be the exact wire token, not a padded near-match');
check(!isNativeSession({ status: 'native', source: ' RPI ' }), 'native authority requires the exact raw rpi token');

check(isSessionActionDisabled(true), 'active row is disabled for delete (Move on the active row is enabled server-side)');
check(!isSessionActionDisabled(false), 'non-active row is eligible whether loaded (backend owns busy refusal) or unloaded');
check(!isSessionActionDisabled(false), 'non-active unloaded row is eligible');

{
  const items = contextMenuItems({ native: true, deleteDisabled: false });
  check(items.map((i) => i.id).join('|') === 'copy-context|export-html|fork-latest|fork-from|clone|rename|move|delete', 'native idle menu order');
  check(items.map((i) => i.label).join('|') === 'Copy context|Export HTML|Fork latest|Fork from…|Clone|Rename|Move session…|Delete', 'native idle menu labels');
  check(items.some((i) => i.id === 'copy-context'), 'Copy context stays on the native-row menu');
  check(items.every((i) => i.id !== 'copy-transcript'), 'Copy transcript tail is REMOVED (never requested — user requirement)');
  check(items.findIndex((i) => i.id === 'move') === items.findIndex((i) => i.id === 'delete') - 1, 'Move sits directly before Delete');
  check(items.findIndex((i) => i.id === 'rename') < items.findIndex((i) => i.id === 'move'), 'Copy context/export/fork/clone/rename order stays ahead of Move');
  check(items.every((i) => !i.disabled), 'native idle items enabled');
  check(items.find((i) => i.id === 'delete')?.danger === true, 'delete is danger');
  check(!items.some((i) => i.id === 'switch' || i.label === 'Switch'), 'native menu has no Switch');
}

{
  const items = contextMenuItems({ native: true, deleteDisabled: true });
  check(!items.some((i) => i.id === 'switch'), 'active native menu has no Switch');
  const del = items.find((i) => i.id === 'delete');
  check(del?.disabled === true, 'active delete disabled');
  check(del?.title === DELETE_SWITCH_AWAY_TITLE, 'delete title is exact switch-away guidance');
  check(items.find((i) => i.id === 'copy-context')?.disabled !== true, 'active native Copy context stays enabled');
}

{
  const items = contextMenuItems({ native: true, deleteDisabled: true });
  const move = items.find((i) => i.id === 'move');
  check(move?.label === 'Move session…', 'move item labels Move session…');
  check(move?.disabled !== true, 'active native move stays enabled (idle active move rebinds cwd server-side)');
  check(move?.title === MOVE_SESSION_TITLE, 'enabled move title explains saved-session relocation incl. the active-row cwd rebind');
  const del = items.find((i) => i.id === 'delete');
  check(del?.disabled === true, 'active delete stays disabled next to the enabled move');
  const idleMove = contextMenuItems({ native: true, deleteDisabled: false }).find((i) => i.id === 'move');
  check(idleMove?.disabled !== true, 'non-active native move enabled (loaded or unloaded)');
  check(idleMove?.title === MOVE_SESSION_TITLE, 'enabled move title explains saved-session relocation');
}

{
  const foreignIdle = contextMenuItems({ native: false, deleteDisabled: false });
  const foreignDisabled = contextMenuItems({ native: false, deleteDisabled: true });
  check(!foreignIdle.some((i) => i.id === 'move'), 'foreign menu never exposes Move');
  check(!foreignDisabled.some((i) => i.id === 'move'), 'disabled foreign menu never exposes Move');
  check(!contextMenuItems({ native: true, deleteDisabled: false }).some((i) => i.id === 'move' && i.disabled), 'unloaded native move defaults enabled');
}

{
  const idleForeign = contextMenuItems({ native: false, deleteDisabled: false });
  const activeForeign = contextMenuItems({ native: false, deleteDisabled: true });
  check(idleForeign.map((i) => i.id).join('|') === 'export-html', 'foreign menu is export-only');
  check(activeForeign.map((i) => i.id).join('|') === 'export-html', 'active foreign menu is export-only');
  check(idleForeign.map((i) => i.label).join('|') === 'Export HTML', 'foreign menu label');
  check(!idleForeign.some((i) => i.id === 'switch'), 'foreign menu has no Switch');
}

{
  const items = contextMenuItems({ native: true, deleteDisabled: false, exportDisabled: true });
  check(items.find((i) => i.id === 'export-html')?.disabled === true, 'in-flight export disables Export HTML');
  check(
    items.filter((i) => i.id !== 'export-html').every((i) => i.disabled !== true),
    'in-flight export does not disable other native items',
  );
}
{
  const items = contextMenuItems({ native: false, deleteDisabled: true, exportDisabled: true });
  check(items.map((i) => i.id).join('|') === 'export-html', 'foreign in-flight export remains export-only');
  check(items.find((i) => i.id === 'export-html')?.disabled === true, 'foreign in-flight Export HTML is disabled');
}
{
  const nativeCopying = contextMenuItems({ native: true, deleteDisabled: false, copyContextDisabled: true });
  check(nativeCopying.find((i) => i.id === 'copy-context')?.disabled === true, 'in-flight copy disables Copy context');
  check(
    nativeCopying.filter((i) => i.id !== 'copy-context').every((i) => i.disabled !== true),
    'in-flight copy does not disable other native items',
  );
  const foreignCopying = contextMenuItems({ native: false, deleteDisabled: true, copyContextDisabled: true });
  check(foreignCopying.map((i) => i.id).join('|') === 'export-html', 'foreign menu never exposes Copy context');
  check(foreignCopying.find((i) => i.id === 'export-html')?.disabled !== true, 'foreign copy flag does not affect Export HTML');
}
{
  const both = contextMenuItems({
    native: true,
    deleteDisabled: true,
    exportDisabled: true,
    copyContextDisabled: true,
  });
  check(both.find((i) => i.id === 'copy-context')?.disabled === true, 'copy disable is independent of export');
  check(both.find((i) => i.id === 'export-html')?.disabled === true, 'export disable is independent of copy');
  check(both.find((i) => i.id === 'delete')?.disabled === true, 'delete disable stays independent');
  check(
    both.filter((i) => i.id !== 'copy-context' && i.id !== 'export-html' && i.id !== 'delete').every((i) => i.disabled !== true),
    'copy and export disables do not spread to fork/clone/rename',
  );
}

check(hasSessionContextMenu({ status: 'native', source: 'rpi' }), 'canonical native row has menu capability');
check(!hasSessionContextMenu({ status: 'native', source: ' RPI ' }), 'menu authority requires the exact raw canonical token');
check(hasSessionContextMenu({ status: 'foreign', source: 'codex' }), 'canonical foreign row keeps export menu capability');
check(hasSessionContextMenu({ status: 'foreign', source: 'grok' }), 'canonical foreign grok keeps export menu capability');
check(!hasSessionContextMenu({ status: 'native', source: 'primary' }), 'legacy primary alias is malformed: no menu authority');
check(!hasSessionContextMenu({ status: 'foreign', source: 'grok/hyper' }), 'legacy grok/hyper alias is malformed: no menu authority');
check(!hasSessionContextMenu({ status: 'rpi', source: 'rpi' }), 'malformed status: no context menu at all');
check(!hasSessionContextMenu({ source: 'rpi' }), 'missing status is malformed: no context menu');
check(!hasSessionContextMenu({ status: 'native', source: '' }), 'empty source is malformed: no context menu');
check(!hasSessionContextMenu({ status: 'native', source: 'claude' }), 'unmappable source is malformed: no menu authority');

check(
  sessionExportUrl('https:', 'runner.example:8443', '01234567-89ab-4cde-8fab-0123456789ab') ===
    'https://runner.example:8443/download/session/01234567-89ab-4cde-8fab-0123456789ab',
  'session export URL targets the active listener authority',
);
check(sessionExportFileName('named-session.html') === 'named-session.html', 'valid export filename is preserved');
check(sessionExportFileName('nested/path\\name.html') === 'nested-path-name.html', 'export filename removes path separators');
assert.throws(
  () => sessionExportUrl('http:', 'runner.example', 'not-a-capability'),
  /download id is invalid/,
  'invalid download capability is rejected',
);
assert.throws(
  () => sessionExportFileName('session.jsonl'),
  /filename is invalid/,
  'non-HTML export filename is rejected',
);

{
  const items = contextMenuItems({
    native: true,
    deleteDisabled: false,
    confirmingDelete: true,
    copyContextDisabled: true,
    exportDisabled: true,
  });
  check(items.map((i) => i.id).join('|') === 'delete-confirm|delete-cancel', 'inline delete confirm items');
  check(items[0]?.danger === true, 'confirm delete is danger');
  check(items[1]?.id === 'delete-cancel' && items[1]?.label === 'Cancel', 'delete confirm keeps cancel');
}

{
  const fit = flipMenuPosition(10, 10, 100, 80, 800, 600);
  check(fit.left === 10 && fit.top === 10, 'fits without flip');
}
{
  const flipped = flipMenuPosition(780, 560, 100, 80, 800, 600);
  check(flipped.left === 680, `flips left when overflowing right: ${flipped.left}`);
  check(flipped.top === 480, `flips up when overflowing bottom: ${flipped.top}`);
}
{
  const clamped = flipMenuPosition(-40, -20, 100, 80, 120, 90);
  check(clamped.left >= 8, `clamps left to margin: ${clamped.left}`);
  check(clamped.top >= 8, `clamps top to margin: ${clamped.top}`);
}

check(nextMenuFocus(-1, 1, 3) === 0, 'arrow down from none focuses first');
check(nextMenuFocus(-1, -1, 3) === 2, 'arrow up from none focuses last');
check(nextMenuFocus(0, 1, 3) === 1, 'arrow down advances');
check(nextMenuFocus(2, 1, 3) === 0, 'arrow down wraps');
check(nextMenuFocus(0, -1, 3) === 2, 'arrow up wraps');
check(nextMenuFocus(0, 1, 3, [false, true, false]) === 2, 'arrow down skips disabled');
check(nextMenuFocus(2, -1, 3, [false, true, false]) === 0, 'arrow up skips disabled');
check(firstEnabledMenuFocus([true, false, false]) === 1, 'first enabled skips leading disabled');
check(firstEnabledMenuFocus([true, true, true]) === 0, 'all disabled falls back to 0');
check(nextMenuFocus(0, 1, 0) === -1, 'empty menu has no focus');

check(SESSION_MENU_LONG_PRESS_MS === 500, 'long press is 500ms');
check(!shouldOpenLongPress(499), '499ms does not open');
check(shouldOpenLongPress(500), '500ms opens');
check(shouldOpenLongPress(800), 'elapsed past threshold opens');
check(!shouldOpenLongPress(200, 500), 'custom threshold respected');

check(forkToastMessage('demo') === 'Forked from demo', 'fork toast names source');
check(SESSION_ARCHIVE_UNDO_MS === 5000, 'undo window is 5s');
const validDelete = { archived: true, sessionId: 'sess-9', undoToken: 'tok-9' };
const parsedDelete = parseSessionDeleteResponse(validDelete, 'sess-9');
check(parsedDelete.sessionId === 'sess-9', 'parsed delete keeps the session id');
check(parsedDelete.undoToken === 'tok-9', 'parsed delete keeps the undo token');

const invalidDeletes: Array<[unknown, string, string]> = [
  [null, 'sess-9', 'null delete response'],
  ['archived', 'sess-9', 'non-object delete response'],
  [{ archived: false, sessionId: 'sess-9', undoToken: 't' }, 'sess-9', 'archived=false delete response'],
  [{ archived: true, undoToken: 't' }, 'sess-9', 'delete response without sessionId'],
  [{ archived: true, sessionId: '  ', undoToken: 't' }, 'sess-9', 'blank delete sessionId'],
  [{ archived: true, sessionId: 's', undoToken: 't' }, 'sess-9', 'delete sessionId disagreeing with the captured row'],
  [{ archived: true, sessionId: 'sess-9x', undoToken: 't' }, 'sess-9', 'delete sessionId id-suffix mismatch'],
  [{ archived: true, sessionId: 'sess-9' }, 'sess-9', 'delete response without undoToken'],
  [{ archived: true, sessionId: 'sess-9', undoToken: '   ' }, 'sess-9', 'blank delete undoToken'],
  [{ archived: true, sessionId: 7, undoToken: 't' }, 'sess-9', 'non-string delete sessionId'],
  [{ archived: true, sessionId: 'sess-9', undoToken: 9 }, 'sess-9', 'non-string delete undoToken'],
];
for (const [response, expectedId, label] of invalidDeletes) {
  assert.throws(
    () => parseSessionDeleteResponse(response, expectedId),
    /session delete response is invalid/,
    `invalid delete response rejected: ${label}`,
  );
}
assert.throws(
  () => parseSessionDeleteResponse(validDelete, '  '),
  /session delete response is invalid/,
  'invalid delete response rejected: blank captured sessionId expectation',
);

function contextArtifact(truncated: boolean, body: string[] = []): string {
  return [
    '# Pi session context',
    `Format: ${SESSION_COPY_CONTEXT_FORMAT}`,
    `Truncated: ${truncated ? 'yes' : 'no'}`,
    ...body,
  ].join('\n');
}

{
  const text = contextArtifact(false);
  const parsed = parseSessionCopyContextResponse({
    text,
    format: SESSION_COPY_CONTEXT_FORMAT,
    truncated: false,
    sessionId: 'ignored',
  });
  check(parsed.text === text, 'valid copy-context keeps the artifact text');
  check(parsed.truncated === false, 'valid copy-context truncated is false');
}
{
  const text = contextArtifact(true);
  const parsed = parseSessionCopyContextResponse({
    text,
    format: SESSION_COPY_CONTEXT_FORMAT,
    truncated: true,
  });
  check(parsed.truncated === true, 'truncated copy-context truncated is true');
  check(parsed.text.includes('Truncated: yes'), 'truncated artifact keeps Truncated: yes');
}
{
  const crlf = `# Pi session context\r\nFormat: ${SESSION_COPY_CONTEXT_FORMAT}\r\nTruncated: no`;
  const parsed = parseSessionCopyContextResponse({
    text: crlf,
    format: SESSION_COPY_CONTEXT_FORMAT,
    truncated: false,
  });
  check(parsed.text === crlf, 'CRLF copy-context headers still validate');
}

const invalidCopyResponses: Array<[unknown, string]> = [
  [null, 'null copy-context response'],
  ['plain', 'non-object copy-context response'],
  [{ format: SESSION_COPY_CONTEXT_FORMAT, truncated: false }, 'missing text'],
  [{ text: contextArtifact(false), truncated: false }, 'missing format'],
  [{ text: contextArtifact(false), format: SESSION_COPY_CONTEXT_FORMAT }, 'missing truncated'],
  [{ text: 1, format: SESSION_COPY_CONTEXT_FORMAT, truncated: false }, 'non-string text'],
  [{ text: contextArtifact(false), format: 'pi-session-context/0', truncated: false }, 'wrong format field'],
  [{ text: contextArtifact(false), format: SESSION_COPY_CONTEXT_FORMAT, truncated: 'no' }, 'non-boolean truncated'],
  [{
    text: `# Pi session context\nFormat: other\nTruncated: no`,
    format: SESSION_COPY_CONTEXT_FORMAT,
    truncated: false,
  }, 'Format header disagrees'],
  [{
    text: contextArtifact(false),
    format: SESSION_COPY_CONTEXT_FORMAT,
    truncated: true,
  }, 'Truncated header disagrees with true'],
  [{
    text: contextArtifact(true),
    format: SESSION_COPY_CONTEXT_FORMAT,
    truncated: false,
  }, 'Truncated header disagrees with false'],
  [{
    text: 'Truncated: no',
    format: SESSION_COPY_CONTEXT_FORMAT,
    truncated: false,
  }, 'missing Format header'],
  [{
    text: `Format: ${SESSION_COPY_CONTEXT_FORMAT}`,
    format: SESSION_COPY_CONTEXT_FORMAT,
    truncated: false,
  }, 'missing Truncated header'],
  [{
    text: `preamble\n${contextArtifact(false)}`,
    format: SESSION_COPY_CONTEXT_FORMAT,
    truncated: false,
  }, 'preamble before canonical heading'],
  [{
    text: contextArtifact(false, [`Format: ${SESSION_COPY_CONTEXT_FORMAT}`]),
    format: SESSION_COPY_CONTEXT_FORMAT,
    truncated: false,
  }, 'duplicate Format header'],
  [{
    text: contextArtifact(false, ['Truncated: no']),
    format: SESSION_COPY_CONTEXT_FORMAT,
    truncated: false,
  }, 'duplicate Truncated header'],
  [{
    text: contextArtifact(false, ['x'.repeat(65_536)]),
    format: SESSION_COPY_CONTEXT_FORMAT,
    truncated: false,
  }, 'oversized UTF-8 payload'],
];
for (const [response, label] of invalidCopyResponses) {
  assert.throws(
    () => parseSessionCopyContextResponse(response),
    /session copy context response is invalid/,
    `${label} is rejected`,
  );
}

{
  const sessionPath = '<workspace>/session-store/native.jsonl';
  const text = contextArtifact(false);
  const sent: Record<string, unknown>[] = [];
  const written: string[] = [];
  const result = await copySessionContext({
    sessionPath,
    sendCommand: async (command) => {
      sent.push(command);
      return { text, format: SESSION_COPY_CONTEXT_FORMAT, truncated: false };
    },
    copyText: async (value) => {
      written.push(value);
      return true;
    },
  });
  check(sent.length === 1 && sent[0]?.type === 'session_copy_context' && sent[0]?.sessionPath === sessionPath, 'copy RPC uses session_copy_context and captured path');
  check(!('sessionId' in (sent[0] ?? {})), 'copy RPC omits session ids');
  check(written.length === 1 && written[0] === text, 'clipboard receives validated text');
  check(result.toast === 'Copied context', 'success toast is Copied context');
  check(result.truncated === false, 'success result is not truncated');
}
{
  const text = contextArtifact(true);
  const result = await copySessionContext({
    sessionPath: '<workspace>/session-store/native.jsonl',
    sendCommand: async () => ({ text, format: SESSION_COPY_CONTEXT_FORMAT, truncated: true }),
    copyText: async () => true,
  });
  check(result.toast === 'Copied context (truncated)', 'truncated toast includes (truncated)');
}
{
  const text = contextArtifact(false);
  let copied = 0;
  await assert.rejects(
    () => copySessionContext({
      sessionPath: '<workspace>/session-store/native.jsonl',
      sendCommand: async () => ({ text, format: SESSION_COPY_CONTEXT_FORMAT, truncated: false }),
      copyText: async () => {
        copied += 1;
        return false;
      },
    }),
    (error: unknown) => error instanceof Error
      && error.message === 'Failed to copy context'
      && shouldToastSessionActionError(error),
    'clipboard false is an error',
  );
  check(copied === 1, 'clipboard false still attempted one write');
}
{
  let copied = 0;
  await assert.rejects(
    () => copySessionContext({
      sessionPath: '<workspace>/session-store/native.jsonl',
      sendCommand: async () => ({ format: SESSION_COPY_CONTEXT_FORMAT, truncated: false }),
      copyText: async () => {
        copied += 1;
        return true;
      },
    }),
    /session copy context response is invalid/,
    'invalid copy-context response never reaches the clipboard',
  );
  check(copied === 0, 'invalid response does not write clipboard');
}
{
  const rpcError = Object.assign(new Error('rpc failed'), { rpc: true, toastOwned: true });
  let copied = 0;
  await assert.rejects(
    () => copySessionContext({
      sessionPath: '<workspace>/session-store/native.jsonl',
      sendCommand: async () => {
        throw rpcError;
      },
      copyText: async () => {
        copied += 1;
        return true;
      },
    }),
    (error: unknown) => error === rpcError,
    'RPC rejection is the original error for shouldToastSessionActionError',
  );
  check(copied === 0, 'RPC rejection does not write clipboard');
  check(!shouldToastSessionActionError(rpcError), 'toastOwned RPC rejection stays App-owned');
}

const EXPORT_ID = '01234567-89ab-4cde-8fab-0123456789ab';

function headers(values: Record<string, string | null>): SessionExportResponseLike['headers'] {
  return {
    get(name: string): string | null {
      const key = name.toLowerCase();
      return Object.prototype.hasOwnProperty.call(values, key) ? values[key] : null;
    },
  };
}

function fakeDocument(): { document: SessionExportDocument; clicks: number; removes: number; appended: number; last: SessionExportAnchor | null } {
  const state = { clicks: 0, removes: 0, appended: 0, last: null as SessionExportAnchor | null };
  return {
    document: {
      createElement(tagName: 'a'): SessionExportAnchor {
        if (tagName !== 'a') throw new Error(`unexpected tag ${tagName}`);
        const anchor: SessionExportAnchor = {
          href: '',
          download: '',
          hidden: false,
          click() { state.clicks += 1; },
          remove() { state.removes += 1; },
        };
        state.last = anchor;
        return anchor;
      },
      body: {
        appendChild() { state.appended += 1; },
      },
    },
    get clicks() { return state.clicks; },
    get removes() { return state.removes; },
    get appended() { return state.appended; },
    get last() { return state.last; },
  };
}

function exportOptions(overrides: {
  fetchImpl: (input: string, init?: { signal?: AbortSignal }) => Promise<SessionExportResponseLike>;
  document?: SessionExportDocument;
  createObjectURL?: (blob: { size: number }) => string;
  revokeObjectURL?: (url: string) => void;
  timeoutMs?: number;
  setTimeout?: (handler: () => void, timeout: number) => number;
  clearTimeout?: (id: number) => void;
}) {
  return {
    hostAuthority: 'runner.example:8443',
    pageProtocol: 'https:',
    setTimeout: overrides.setTimeout ?? ((handler: () => void) => {
      void handler;
      return 1;
    }),
    clearTimeout: overrides.clearTimeout ?? (() => {}),
    createObjectURL: overrides.createObjectURL ?? (() => 'blob:session-export'),
    revokeObjectURL: overrides.revokeObjectURL ?? (() => {}),
    document: overrides.document ?? fakeDocument().document,
    timeoutMs: overrides.timeoutMs,
    fetchImpl: overrides.fetchImpl,
  };
}

{
  let blobRead = false;
  await assert.rejects(
    () => downloadSessionExport(EXPORT_ID, 'session.html', exportOptions({
      fetchImpl: async () => ({
        ok: true,
        headers: headers({ 'content-type': 'application/json', 'content-length': '12' }),
        blob: async () => {
          blobRead = true;
          return { size: 12 };
        },
      }),
    })),
    /session export response is invalid/,
    'wrong content type is rejected',
  );
  check(!blobRead, 'wrong content type does not read the body');
}

{
  let blobRead = false;
  await assert.rejects(
    () => downloadSessionExport(EXPORT_ID, 'session.html', exportOptions({
      fetchImpl: async () => ({
        ok: true,
        headers: headers({ 'content-type': 'text/html', 'content-length': null }),
        blob: async () => {
          blobRead = true;
          return { size: 4 };
        },
      }),
    })),
    /session export exceeds the download limit/,
    'missing content-length is rejected',
  );
  check(!blobRead, 'missing content-length does not read the body');
}

{
  let blobRead = false;
  await assert.rejects(
    () => downloadSessionExport(EXPORT_ID, 'session.html', exportOptions({
      fetchImpl: async () => ({
        ok: true,
        headers: headers({
          'content-type': 'text/html; charset=utf-8',
          'content-length': String(MAX_SESSION_EXPORT_BYTES + 1),
        }),
        blob: async () => {
          blobRead = true;
          return { size: MAX_SESSION_EXPORT_BYTES + 1 };
        },
      }),
    })),
    /session export exceeds the download limit/,
    'oversized content-length is rejected',
  );
  check(!blobRead, 'oversized content-length does not read the body');
}

{
  await assert.rejects(
    () => downloadSessionExport(EXPORT_ID, 'session.html', exportOptions({
      fetchImpl: async () => ({
        ok: true,
        headers: headers({ 'content-type': 'text/html', 'content-length': '4' }),
        blob: async () => ({ size: 5 }),
      }),
    })),
    /session export exceeds the download limit/,
    'blob size mismatch is rejected',
  );
}

{
  let blobRead = false;
  await assert.rejects(
    () => downloadSessionExport(EXPORT_ID, 'session.html', exportOptions({
      fetchImpl: async () => ({
        ok: false,
        headers: headers({ 'content-type': 'text/html', 'content-length': '4' }),
        blob: async () => {
          blobRead = true;
          return { size: 4 };
        },
      }),
    })),
    /session export download failed/,
    'non-OK response is rejected',
  );
  check(!blobRead, 'non-OK response does not read the body');
}

{
  await assert.rejects(
    () => downloadSessionExport(EXPORT_ID, 'session.html', exportOptions({
      fetchImpl: async () => ({
        ok: true,
        headers: headers({ 'content-type': 'text/html', 'content-length': 'not-a-length' }),
        blob: async () => ({ size: 4 }),
      }),
    })),
    /session export exceeds the download limit/,
    'non-integer content-length is rejected',
  );
}

{
  await assert.rejects(
    () => downloadSessionExport(EXPORT_ID, 'session.html', exportOptions({
      fetchImpl: async () => {
        throw new Error('Failed to fetch https://runner.example:8443/download/session');
      },
    })),
    (error: unknown) => error instanceof Error
      && error.message === 'session export download failed',
    'generic fetch failure is wrapped without leaking the URL',
  );
}

{
  const { promise, reject } = withResolvers<SessionExportResponseLike>();
  await assert.rejects(
    () => downloadSessionExport(EXPORT_ID, 'session.html', exportOptions({
      timeoutMs: 5,
      setTimeout: (handler) => {
        handler();
        return 1;
      },
      fetchImpl: (_url, init) => {
        if (init?.signal?.aborted) {
          reject(new DOMException('aborted', 'AbortError'));
        } else {
          init?.signal?.addEventListener('abort', () => reject(new DOMException('aborted', 'AbortError')));
        }
        return promise;
      },
    })),
    /session export download timed out/,
    'aborted fetch surfaces a timeout',
  );
}

{
  const dom = fakeDocument();
  let fetches = 0;
  let created = 0;
  let revoked = 0;
  await downloadSessionExport(EXPORT_ID, 'named-session.html', exportOptions({
    document: dom.document,
    createObjectURL: (blob) => {
      check(blob.size === 12, 'success object URL uses the bounded blob');
      created += 1;
      return 'blob:session-export';
    },
    revokeObjectURL: (url) => {
      check(url === 'blob:session-export', 'success revokes the created object URL');
      revoked += 1;
    },
    fetchImpl: async (input) => {
      fetches += 1;
      check(
        input === 'https://runner.example:8443/download/session/01234567-89ab-4cde-8fab-0123456789ab',
        'success fetch targets the listener export URL',
      );
      return {
        ok: true,
        headers: headers({ 'content-type': 'text/html; charset=utf-8', 'content-length': '12' }),
        blob: async () => ({ size: 12 }),
      };
    },
  }));
  check(fetches === 1, 'success fetch runs once');
  check(created === 1, 'success creates one object URL');
  check(revoked === 1, 'success revokes exactly once');
  check(dom.appended === 1, 'success appends one anchor');
  check(dom.clicks === 1, 'success clicks the anchor once');
  check(dom.removes === 1, 'success removes the anchor once');
  check(dom.last?.download === 'named-session.html', 'success uses the sanitized filename');
  check(dom.last?.hidden === true, 'success anchor stays hidden');
}

// ---- Move session… behavioral wire flow (fake sendCommand) ----
{
  const commands: Array<Record<string, unknown>> = [];
  const sendCommand = async (command: Record<string, unknown>): Promise<unknown> => {
    commands.push(command);
    return { moved: true };
  };

  const result = await moveSession({
    sessionPath: '/sessions/--home-proj--/abc.jsonl',
    targetCwd: '  /tmp/target  ',
    sendCommand,
  });
  check(
    typeof result === 'object' && result !== null && 'moved' in result && result.moved === true,
    'moveSession resolves with the wire result',
  );
  check(commands.length === 1, 'moveSession sends exactly one command');
  check(
    JSON.stringify(commands[0]) === JSON.stringify({
      type: 'session_move',
      sessionPath: '/sessions/--home-proj--/abc.jsonl',
      targetCwd: '/tmp/target',
    }),
    'session_move payload is exactly {type,sessionPath,targetCwd} with a trimmed target',
  );
  check(sessionMoveToast() === 'Session moved', 'move toast is a fixed bounded message without the local path');
}

{
  const commands: Array<Record<string, unknown>> = [];
  const sendCommand = async (command: Record<string, unknown>): Promise<unknown> => {
    commands.push(command);
    return { moved: true };
  };
  await assert.rejects(
    () => moveSession({ sessionPath: '/sessions/a.jsonl', targetCwd: '   ', sendCommand }),
    /target directory is required/,
    'blank move target is rejected locally',
  );
  await assert.rejects(
    () => moveSession({ sessionPath: '/sessions/a.jsonl', targetCwd: '', sendCommand }),
    /target directory is required/,
    'empty move target is rejected locally',
  );
  await assert.rejects(
    () => moveSession({ sessionPath: '/sessions/a.jsonl', targetCwd: '  relative/dir  ', sendCommand }),
    /target directory must be an absolute path/,
    'relative move target is rejected locally — the picker demands an explicit absolute target',
  );
  check(commands.length === 0, 'rejected move targets never reach the wire');
}

{
  // PLATFORM-NEUTRAL ABSOLUTE TARGETS: Windows drive-root (both separators)
  // and strict UNC targets are accepted locally, while drive-relative,
  // share-less, and malformed strict-UNC doubles never reach the wire. The
  // raw trimmed target is preserved on the wire in every accepted shape.
  const commands: Array<Record<string, unknown>> = [];
  const sendCommand = async (command: Record<string, unknown>): Promise<unknown> => {
    commands.push(command);
    return { moved: true };
  };
  const accepted = [
    ['  C:\\projects\\beta  ', 'C:\\projects\\beta'],
    ['C:/projects/beta', 'C:/projects/beta'],
    ['  \\\\server\\share\\proj  ', '\\\\server\\share\\proj'],
    ['Z:\\', 'Z:\\'],
  ] as const;
  for (const [raw, wire] of accepted) {
    await moveSession({ sessionPath: '/sessions/a.jsonl', targetCwd: raw, sendCommand });
    check(
      JSON.stringify(commands[commands.length - 1]) === JSON.stringify({
        type: 'session_move',
        sessionPath: '/sessions/a.jsonl',
        targetCwd: wire,
      }),
      `platform-neutral absolute target ${JSON.stringify(raw)} is sent trimmed and raw`,
    );
  }
  check(commands.length === accepted.length, 'every platform-neutral absolute target reaches the wire exactly once');
  for (const target of [
    'workspace/relative',
    'C:relative\\path',
    'C:',
    '1:\\projects',
    ':\\projects',
    '\\\\server',
    '\\\\server\\',
    '\\\\',
    '\\\\\\server\\share',
    '\\\\server\\\\share',
    '\\\\server//share',
  ]) {
    await assert.rejects(
      () => moveSession({ sessionPath: '/sessions/a.jsonl', targetCwd: target, sendCommand }),
      /target directory must be an absolute path/,
      `non-absolute platform shape ${JSON.stringify(target)} is rejected locally`,
    );
  }
  check(commands.length === accepted.length, 'rejected platform shapes never reach the wire');
}

{
  let failures = 0;
  const sendCommand = async (): Promise<unknown> => {
    failures += 1;
    throw new Error('session_move failed: destination is not writable');
  };
  await assert.rejects(
    () => moveSession({ sessionPath: '/sessions/a.jsonl', targetCwd: '/tmp/t', sendCommand }),
    /destination is not writable/,
    'move wire errors propagate to the bounded error channel',
  );
  check(failures === 1, 'failed move sends exactly once');
}

// ---- `session_move` response destination-row validation (parse before the
// success toast; the row must be trustworthy for the target-project group)
{
  const SOURCE_PATH = '/sessions/--home-alpha--/c.jsonl';
  const DEST_PATH = '/sessions/--home-projects-beta--/c.jsonl';
  const valid = {
    moved: true,
    sessionId: 'sess-1',
    from: SOURCE_PATH,
    to: DEST_PATH,
    sidecars: [],
    children: [],
    rebound: { memory: 0, workflows: 0, trust: 0 },
    warnings: [],
    session: {
      source: 'rpi',
      sessionId: 'sess-1',
      name: null,
      cwd: '/home/projects/beta',
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
  const expected = {
    expectedSessionId: 'sess-1',
    expectedSessionPath: SOURCE_PATH,
    expectedTargetCwd: '/home/projects/beta',
  };
  const parsed = parseSessionMoveResponse(valid, expected);
  check(parsed.moved === true, 'parsed destination keeps the truthful moved=true marker');
  check(parsed.sessionId === 'sess-1', 'parsed destination keeps the session id');
  check(parsed.from === SOURCE_PATH, 'parsed destination keeps the outcome from');
  check(parsed.path === DEST_PATH, 'parsed destination keeps the moved file path');
  check(parsed.cwdRaw === '/home/projects/beta', 'parsed destination keeps the raw target cwd');
  check(parsed.source === 'rpi', 'parsed destination keeps the native source');
  check(parsed.to === parsed.path, 'parsed outcome destination agrees with the row path');
  // The parsed destination agrees with the EXPLICIT target lexically:
  // trimmed, trailing-slash-insensitive, never canonicalized.
  check(
    parseSessionMoveResponse(valid, { ...expected, expectedTargetCwd: ' /home/projects/beta/ ' }).cwdRaw === '/home/projects/beta',
    'expected cwd agreement tolerates trim + trailing slash only',
  );
  // The RAW wire row cwd is echoed UNTOUCHED: a padded wire cwd passes the
  // lexical agreement (trim and normalization are local to the comparison)
  // and comes back byte-for-byte.
  check(
    parseSessionMoveResponse(
      { ...valid, session: { ...valid.session, cwd: '  /home/projects/beta  ' } },
      expected,
    ).cwdRaw === '  /home/projects/beta  ',
    'a padded wire row cwd is echoed untouched; trim stays local to the comparison',
  );

  // TRUTHFUL IDEMPOTENT NO-OP: moved=false is a SUCCESS — the session
  // already resided in the target store — ONLY when the full caller-bound
  // tuple (sessionId sites, from, native row, to==path, cwd) validates
  // exactly like a real move. A successful backend no-op is never reported
  // as a failure, and a no-op with a broken tuple is still rejected.
  const residentValid = {
    ...valid,
    moved: false,
    to: SOURCE_PATH,
    session: { ...valid.session, path: SOURCE_PATH },
  };
  const residentParsed = parseSessionMoveResponse(residentValid, expected);
  check(residentParsed.moved === false, 'a backend no-op parses with the truthful moved=false marker');
  check(residentParsed.from === SOURCE_PATH, 'the no-op from is the resident source path');
  check(residentParsed.to === SOURCE_PATH, 'the no-op to is the resident source path');
  assert.throws(
    () => parseSessionMoveResponse({ ...residentValid, sessionId: 'sess-other' }, expected),
    /session move response is invalid/,
    'invalid move response rejected: moved=false no-op with an unbound top-level sessionId',
  );
  assert.throws(
    () => parseSessionMoveResponse({ ...residentValid, from: '/elsewhere/r.jsonl' }, expected),
    /session move response is invalid/,
    'invalid move response rejected: moved=false no-op with an unbound from',
  );
  assert.throws(
    () => parseSessionMoveResponse({ moved: false }, expected),
    /session move response is invalid/,
    'invalid move response rejected: moved=false without the destination tuple',
  );
  assert.throws(
    () => parseSessionMoveResponse({ ...valid, moved: 'yes' }, expected),
    /session move response is invalid/,
    'invalid move response rejected: non-boolean moved marker',
  );

  // EXACT raw identity/path positive: the captured expectations, the
  // top-level sessionId, the row sessionId, the captured source path, the
  // outcome `from`, the row path, and the outcome `to` are all compared as
  // untouched raw strings — the parsed destination echoes them verbatim
  // with no normalization applied anywhere.
  const RAW_SESSION_ID = 'sid 42';
  const RAW_SOURCE_PATH = '/sessions/--home-alpha--/s id 42.jsonl';
  const RAW_DEST_PATH = '/sessions/--home-projects-beta--/s id 42.jsonl';
  const rawValid = {
    ...valid,
    sessionId: RAW_SESSION_ID,
    from: RAW_SOURCE_PATH,
    to: RAW_DEST_PATH,
    session: { ...valid.session, sessionId: RAW_SESSION_ID, path: RAW_DEST_PATH },
  };
  const rawParsed = parseSessionMoveResponse(rawValid, {
    expectedSessionId: RAW_SESSION_ID,
    expectedSessionPath: RAW_SOURCE_PATH,
    expectedTargetCwd: '/home/projects/beta',
  });
  check(rawParsed.sessionId === RAW_SESSION_ID, 'exact raw sessionId is echoed verbatim');
  check(rawParsed.from === RAW_SOURCE_PATH, 'exact raw outcome from is echoed verbatim');
  check(rawParsed.path === RAW_DEST_PATH, 'exact raw path is echoed verbatim');
  check(rawParsed.to === RAW_DEST_PATH, 'exact raw outcome to is echoed verbatim');

  // LEXICAL `.`/`..` agreement — symmetric, string-only: the row cwd and
  // the explicit target agree after pure lexical `.`/`..` collapse on BOTH
  // sides (never a filesystem canonicalization); a root escape or a
  // relative path on either side is a rejection, and nested/different cwds
  // never collapse into agreement. The raw row cwd is echoed verbatim.
  check(
    parseSessionMoveResponse(
      { ...valid, session: { ...valid.session, cwd: '/home/projects/beta/../beta' } },
      expected,
    ).cwdRaw === '/home/projects/beta/../beta',
    'row cwd /a/../a lexically agrees with the captured target and is echoed raw',
  );
  check(
    parseSessionMoveResponse(valid, { ...expected, expectedTargetCwd: '/home/projects/../projects/beta' }).cwdRaw === '/home/projects/beta',
    'captured target /a/../a lexically agrees with the row cwd (symmetric)',
  );
  check(
    parseSessionMoveResponse(
      { ...valid, session: { ...valid.session, cwd: '/home/projects/./beta//' } },
      expected,
    ).cwdRaw === '/home/projects/./beta//',
    'row cwd dot segments and trailing slashes collapse lexically (raw echo preserved)',
  );
  const lexicalInvalid: Array<[unknown, string]> = [
    [{ ...valid, session: { ...valid.session, cwd: '/home/projects/beta/../..' } }, 'row cwd lexically escaping above the root'],
    [{ ...valid, session: { ...valid.session, cwd: '/../beta' } }, 'row cwd escaping the root immediately'],
    [{ ...valid, session: { ...valid.session, cwd: '/home/projects/beta/../../beta' } }, 'row cwd lexically resolving elsewhere through `..` segments'],
    [{ ...valid, session: { ...valid.session, cwd: 'home/projects/beta' } }, 'relative row cwd is never an absolute lexical match'],
    [{ ...valid, session: { ...valid.session, cwd: '/home/projects' } }, 'row cwd lexically resolving to the parent of the target'],
  ];
  for (const [response, label] of lexicalInvalid) {
    assert.throws(
      () => parseSessionMoveResponse(response, expected),
      /session move response is invalid/,
      `invalid move response rejected: ${label}`,
    );
  }
  const lexicalTargetInvalid: Array<[string, string]> = [
    ['/../beta', 'captured target lexically escaping above the root'],
    ['/home/projects/beta/../../beta', 'captured target lexically resolving elsewhere through `..` segments'],
    ['relative/beta', 'relative captured target is never an absolute lexical match'],
    ['/home/projects/beta/nested', 'captured target nested under (never equal to) the row cwd'],
  ];
  for (const [targetCwd, label] of lexicalTargetInvalid) {
    assert.throws(
      () => parseSessionMoveResponse(valid, { ...expected, expectedTargetCwd: targetCwd }),
      /session move response is invalid/,
      `invalid move response rejected: ${label}`,
    );
  }

  // PLATFORM-NEUTRAL lexical agreement: Windows drive-root and strict UNC
  // row cwds agree with their targets across both separators and drive
  // casing (pure string arithmetic — never canonicalization), and the RAW
  // row cwd is still echoed verbatim.
  check(
    parseSessionMoveResponse(
      { ...valid, session: { ...valid.session, cwd: 'C:\\projects\\beta' } },
      { ...expected, expectedTargetCwd: 'c:/projects/beta/' },
    ).cwdRaw === 'C:\\projects\\beta',
    'drive-root row cwd agrees with a mixed-separator lower-cased target (raw echo preserved)',
  );
  check(
    parseSessionMoveResponse(
      { ...valid, session: { ...valid.session, cwd: 'C:\\projects\\..\\projects\\beta' } },
      { ...expected, expectedTargetCwd: 'C:/projects/beta' },
    ).cwdRaw === 'C:\\projects\\..\\projects\\beta',
    'drive-root row cwd collapses `..` segments lexically against the drive-root captured target (raw echo preserved)',
  );
  check(
    parseSessionMoveResponse(
      { ...valid, session: { ...valid.session, cwd: '\\\\server\\share\\proj\\..\\beta' } },
      { ...expected, expectedTargetCwd: '\\\\server\\share\\beta' },
    ).cwdRaw === '\\\\server\\share\\proj\\..\\beta',
    'strict UNC row cwd collapses `..` lexically against the captured target (raw echo preserved)',
  );
  const platformInvalid: Array<[unknown, string, string]> = [
    [{ ...valid, session: { ...valid.session, cwd: 'C:\\projects\\beta' } }, '/home/projects/beta', 'drive-root row cwd never agrees with a POSIX captured target'],
    [{ ...valid, session: { ...valid.session, cwd: 'C:relative\\path' } }, 'C:relative\\path', 'drive-relative row cwd never normalizes'],
    [{ ...valid, session: { ...valid.session, cwd: 'C:' } }, 'C:', 'bare drive row cwd never normalizes'],
    [{ ...valid, session: { ...valid.session, cwd: '\\\\server\\share' } }, '/home/projects/beta', 'strict UNC row cwd never agrees with a POSIX captured target'],
    [{ ...valid, session: { ...valid.session, cwd: '\\\\server' } }, '\\\\server', 'share-less row cwd never normalizes'],
    [{ ...valid, session: { ...valid.session, cwd: '\\\\server\\\\share' } }, '\\\\server\\\\share', 'doubled strict-UNC separator row cwd never normalizes'],
    [{ ...valid, session: { ...valid.session, cwd: '\\\\\\server\\share' } }, '\\\\\\server\\share', 'triple-leading strict-UNC row cwd never normalizes'],
    [{ ...valid, session: { ...valid.session, cwd: '\\\\server\\share\\' } }, '\\\\server\\share\\', 'trailing strict-UNC separator row cwd never normalizes'],
    [{ ...valid, session: { ...valid.session, cwd: 'C:\\projects\\beta\\..\\..' } }, 'C:\\projects\\beta', 'drive-root row cwd collapsing onto the bare drive root never equals the target'],
    [{ ...valid, session: { ...valid.session, cwd: '\\\\server\\share\\..\\beta' } }, '\\\\server\\share\\beta', 'strict UNC row cwd escaping above the share root'],
  ];
  for (const [response, targetCwd, label] of platformInvalid) {
    assert.throws(
      () => parseSessionMoveResponse(response, { ...expected, expectedTargetCwd: targetCwd }),
      /session move response is invalid/,
      `invalid move response rejected: ${label}`,
    );
  }
  const platformTargetInvalid: Array<[string, string]> = [
    ['C:relative\\path', 'drive-relative captured target never normalizes'],
    ['C:', 'bare drive captured target never normalizes'],
    ['\\\\server', 'share-less captured target never normalizes'],
    ['\\\\server\\\\share', 'doubled strict-UNC captured target never normalizes'],
    ['\\\\\\server\\share', 'triple-leading captured target never normalizes'],
    ['\\\\server\\share\\', 'trailing strict-UNC captured target never normalizes'],
  ];
  for (const [targetCwd, label] of platformTargetInvalid) {
    assert.throws(
      () => parseSessionMoveResponse(valid, { ...expected, expectedTargetCwd: targetCwd }),
      /session move response is invalid/,
      `invalid move response rejected: ${label}`,
    );
  }
  // The normalizer is for cwd agreement ONLY: id/from/to/path stay raw — a
  // strict-UNC row path with padding is echoed verbatim against an equally
  // raw capture.
  check(
    parseSessionMoveResponse(
      {
        ...valid,
        sessionId: 'sid 7',
        from: '\\\\srv\\sh\\a.jsonl',
        to: '\\\\srv\\sh\\b.jsonl',
        session: {
          ...valid.session,
          sessionId: 'sid 7',
          cwd: '\\\\server\\share\\proj',
          path: '\\\\srv\\sh\\b.jsonl',
        },
      },
      {
        expectedSessionId: 'sid 7',
        expectedSessionPath: '\\\\srv\\sh\\a.jsonl',
        expectedTargetCwd: '\\\\server\\share\\proj',
      },
    ).path === '\\\\srv\\sh\\b.jsonl',
    'cwd-only normalization: raw UNC id/from/to/path are echoed verbatim',
  );

  // EXACT canonical authority ONLY: the destination row must prove the raw
  // tuple (source exactly 'rpi', status exactly 'native') through the same
  // catalogIdentityOf gate the sidebar uses — legacy aliases ('native',
  // 'primary'), dirty case, and padding are malformed and rejected; there
  // is no trim, no case folding, and no alias mapping.
  const aliases: Array<[unknown, string]> = [
    [{ ...valid, session: { ...valid.session, source: 'native' } }, 'legacy native source alias is malformed: rejected, never re-labeled rpi'],
    [{ ...valid, session: { ...valid.session, source: 'primary' } }, 'legacy primary source alias is malformed: rejected, never re-labeled rpi'],
  ];
  for (const [response, label] of aliases) {
    assert.throws(
      () => parseSessionMoveResponse(response, expected),
      /session move response is invalid/,
      `invalid move response rejected: ${label}`,
    );
  }

  // Fixture builders: a minimal fully-valid move response bound to
  // sessionId 's', source path '/src', destination '/p', target cwd '/t'.
  const row = (over: Record<string, unknown> = {}): Record<string, unknown> => ({
    source: 'rpi', sessionId: 's', cwd: '/t', path: '/p', status: 'native', ...over,
  });
  const wire = (over: Record<string, unknown> = {}): Record<string, unknown> => ({
    moved: true, sessionId: 's', from: '/src', to: '/p', session: row(), ...over,
  });
  const bound = (over: Record<string, string> = {}): Record<string, string> => ({
    expectedSessionId: 's', expectedSessionPath: '/src', expectedTargetCwd: '/t', ...over,
  });

  // ---- Padding table: a raw capture only matches a wire value padded the
  // exact same way. Identical padding on BOTH sides of every raw site is
  // accepted verbatim; one-sided padding on any single site is rejected. ----
  check(
    parseSessionMoveResponse(
      wire({ sessionId: ' s ', session: row({ sessionId: ' s ' }) }),
      bound({ expectedSessionId: ' s ' }),
    ).sessionId === ' s ',
    'identical padding across captured/top-level/row sessionId sites is raw-exact accepted',
  );
  check(
    parseSessionMoveResponse(wire({ to: ' /p ', session: row({ path: ' /p ' }) }), bound()).path === ' /p ',
    'identical row.path/to padding is raw-exact accepted',
  );
  check(
    parseSessionMoveResponse(wire({ from: ' /src ' }), bound({ expectedSessionPath: ' /src ' })).from === ' /src ',
    'identical from/expectedSessionPath padding is raw-exact accepted',
  );
  check(
    parseSessionMoveResponse(wire({ session: row({ cwd: ' /t ' }) }), bound()).cwdRaw === ' /t ',
    'padded row cwd lexically agrees with the captured target and is echoed raw',
  );
  check(
    parseSessionMoveResponse(wire(), bound({ expectedTargetCwd: ' /t ' })).cwdRaw === '/t',
    'padded captured target cwd is lexically trimmed against the raw row cwd',
  );
  check(
    parseSessionMoveResponse(wire({ session: row({ cwd: ' /t ' }) }), bound({ expectedTargetCwd: ' /t ' })).cwdRaw === ' /t ',
    'padded row cwd AND padded captured target cwd are accepted together (raw echo preserved)',
  );
  check(
    parseSessionMoveResponse(wire({ session: row({ cwd: '/t/' }) }), bound()).cwdRaw === '/t/',
    'trailing-slash row cwd lexically agrees with the captured target (raw echo preserved)',
  );
  check(
    parseSessionMoveResponse(wire({ session: row({ cwd: ' /t///' }) }), bound({ expectedTargetCwd: ' /t ' })).cwdRaw === ' /t///',
    'padded multi-trailing-slash row cwd AND padded captured target are accepted together (raw echo preserved)',
  );

  const invalid: Array<[unknown, Record<string, string>, string]> = [
    [null, bound(), 'null response'],
    ['moved', bound(), 'non-object response'],
    [wire({ moved: 'yes' }), bound(), 'non-boolean moved marker'],
    [{ moved: true }, bound(), 'response without destination row'],
    [wire({ session: 'row' }), bound(), 'non-object destination row'],
    [wire({ session: { source: 'rpi', sessionId: 's', cwd: '/t' } }), bound(), 'destination row without path'],
    [wire({ session: row({ sessionId: '' }) }), bound(), 'destination row without sessionId'],
    [wire({ session: row({ cwd: '' }) }), bound(), 'destination row without cwd'],
    [wire({ to: '/elsewhere/x.jsonl' }), bound(), 'outcome `to` disagreeing with the row path'],
    [wire({ sessionId: undefined }), bound(), 'outcome without top-level sessionId'],
    [wire({ sessionId: 'other' }), bound(), 'top-level sessionId disagreeing with the row sessionId'],
    [wire({ session: row({ sessionId: 'other' }) }), bound(), 'row sessionId disagreeing with the captured row'],
    [wire({ from: '' }), bound(), 'empty outcome from'],
    [wire({ from: '/other.jsonl' }), bound(), 'outcome from naming another source path'],
    [wire({ from: ' /src ' }), bound(), 'padded outcome from: no trim against the raw capture'],
    [wire(), bound({ expectedSessionPath: ' /src ' }), 'padded captured source path: no trim against the raw from'],
    [wire(), bound({ expectedSessionPath: '/other.jsonl' }), 'captured source path disagreeing with the outcome from'],
    [wire({ session: { sessionId: 's', cwd: '/t', path: '/p', status: 'native' } }), bound(), 'destination row without source field'],
    [wire({ session: row({ source: '' }) }), bound(), 'empty destination row source'],
    [wire({ session: row({ source: 'codex' }) }), bound(), 'foreign destination row source'],
    [wire({ session: row({ source: 5 }) }), bound(), 'non-string destination row source'],
    [wire({ session: { source: 'rpi', sessionId: 's', cwd: '/t', path: '/p' } }), bound(), 'destination row without status field'],
    [wire({ session: row({ status: 'foreign' }) }), bound(), 'non-native destination row status'],
    [wire({ session: row({ cwd: '/elsewhere' }) }), bound(), 'row cwd disagreeing with the explicit target'],
    [wire({ session: row({ cwd: '/t/nested' }) }), bound(), 'row cwd nested under (not equal to) the target'],
    [wire({ session: row({ source: 'RPI' }) }), bound(), 'dirty cased destination row source: no case folding'],
    [wire({ session: row({ source: ' rpi ' }) }), bound(), 'padded destination row source: no trim'],
    [wire({ session: row({ status: 'NATIVE' }) }), bound(), 'dirty cased destination row status: no case folding'],
    [wire({ session: row({ status: ' native ' }) }), bound(), 'padded destination row status: no trim'],
    [wire({ sessionId: ' s ' }), bound(), 'padded top-level sessionId: no trim'],
    [wire({ session: row({ sessionId: ' s ' }) }), bound(), 'padded destination row sessionId: no trim'],
    [wire({ session: row({ path: ' /p ' }) }), bound(), 'padded destination row path: no trim'],
    [wire({ to: ' /p ' }), bound(), 'padded outcome `to`: no trim'],
  ];
  for (const [response, expectations, label] of invalid) {
    assert.throws(
      () => parseSessionMoveResponse(response, expectations),
      /session move response is invalid/,
      `invalid move response rejected: ${label}`,
    );
  }
  assert.throws(
    () => parseSessionMoveResponse(valid, { ...expected, expectedSessionId: '' }),
    /session move response is invalid/,
    'invalid move response rejected: empty captured sessionId expectation',
  );
  assert.throws(
    () => parseSessionMoveResponse(valid, { ...expected, expectedSessionId: '  ' }),
    /session move response is invalid/,
    'invalid move response rejected: padded captured sessionId can never match the exact raw wire id',
  );
  assert.throws(
    () => parseSessionMoveResponse(valid, { ...expected, expectedSessionPath: '' }),
    /session move response is invalid/,
    'invalid move response rejected: empty captured source path expectation',
  );
  assert.throws(
    () => parseSessionMoveResponse(valid, { ...expected, expectedSessionPath: '  ' }),
    /session move response is invalid/,
    'invalid move response rejected: padded captured source path can never match the exact raw from',
  );
}

// ---- Move target directory suggestions: `path_complete` bound to the
// active source session where one is available (the same closure the
// sidebar hands to PathAutocomplete) ----
{
  const commands: Array<Record<string, unknown>> = [];
  const sendCommand = async (command: Record<string, unknown>): Promise<unknown> => ({
    base: '/tmp',
    prefix: '',
    entries: [{ name: 'target', path: '/tmp/target', kind: 'directory' }],
    truncated: false,
    __seen: commands.push(command),
  });

  const bound = await sendPathCompleteRequest(sendCommand, 'active-session-id', {
    type: 'path_complete',
    path: '/tm',
    kind: 'directory',
    limit: 100,
  });
  check(bound !== null && bound.entries.length === 1, 'bound move suggestions resolve');
  check(
    JSON.stringify(commands[commands.length - 1]) === JSON.stringify({
      type: 'path_complete',
      path: '/tm',
      kind: 'directory',
      limit: 100,
      sessionId: 'active-session-id',
    }),
    'path_complete binds the active source session id when available',
  );

  const unbound = await sendPathCompleteRequest(sendCommand, null, {
    type: 'path_complete',
    path: '/tm',
    kind: 'directory',
    limit: 100,
  });
  check(unbound !== null, 'unbound move suggestions still resolve');
  check(
    !('sessionId' in commands[commands.length - 1]),
    'path_complete omits sessionId when no active source session exists',
  );
}

{
  const sendCommand = async (): Promise<unknown> => {
    throw new Error('path_complete unavailable');
  };
  const quiet = await sendPathCompleteRequest(sendCommand, 'active-session-id', {
    type: 'path_complete',
    path: '/tm',
    kind: 'directory',
    limit: 100,
  });
  check(quiet === null, 'completion failures stay quiet so manual absolute entry never blocks');
}

console.log(`sessionContextMenu.test: ${assertions} assertions passed`);
