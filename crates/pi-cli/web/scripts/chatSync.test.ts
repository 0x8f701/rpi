#!/usr/bin/env node
// chatSync.test.ts — wire-envelope contract for the /chat LAN sync client.
// The listener answers POST /rpc with the shared RpcResponse envelope
// {success, data?, error?}; chat_mapping_get wraps {revision, savedAtMs,
// document} and chat_mapping_put wraps {ok|conflict, ...}. A malformed
// payload must throw — never normalize into a fabricated empty document.
import {
  applyRunnerTokens,
  fetchMappingSync,
  isSyncNetworkFailure,
  isSyncPayload,
  pushMappingSync,
} from '../src/chat/chatSync.ts';
import { hostTokenKey, loadTokenForAuthority, saveTokenForAuthority, type StorageLike } from '../src/hostToken.ts';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const failures: string[] = [];
let assertions = 0;

function check(name: string, condition: boolean, detail = ''): void {
  assertions += 1;
  if (!condition) failures.push(detail ? `${name} — ${detail}` : name);
}

async function rejection(promise: Promise<unknown>): Promise<string> {
  try {
    await promise;
  } catch (error) {
    return error instanceof Error ? error.message : String(error);
  }
  return '';
}

interface FetchCall { input: string; init: RequestInit | undefined; }

function installFetch(status: number, body: unknown): { calls: FetchCall[]; restore: () => void } {
  const calls: FetchCall[] = [];
  const previous = globalThis.fetch;
  (globalThis as { fetch: unknown }).fetch = (async (input: string, init: RequestInit) => {
    calls.push({ input, init });
    return {
      ok: status >= 200 && status < 300,
      status,
      json: async () => body,
    } as Response;
  }) as typeof fetch;
  return { calls, restore: () => { (globalThis as { fetch: unknown }).fetch = previous; } };
}

// Stand-in for mapping.ts's validateStoredDocument at the same strictness:
// version + arrays, mirroring the production callback ChatApp passes.
function isMappingDocument(value: unknown): boolean {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) return false;
  const raw = value as Record<string, unknown>;
  return raw.version === 1 && Array.isArray(raw.projects) && Array.isArray(raw.mappings);
}

const SYNC_DOCUMENT = {
  version: 1,
  projects: [{ id: 'p1', label: 'Product', cwd: '/workspace/project', createdAt: 1, updatedAt: 2 }],
  mappings: [],
};
const PAYLOAD = {
  mapping: SYNC_DOCUMENT,
  runners: [{ id: 'r1', label: 'Runner', origin: 'http://h:1', authority: 'h:1', createdAt: 1, updatedAt: 2, codename: null }],
  tokens: { 'h:1': 'tok' },
};

async function main(): Promise<void> {
  {
    const { calls, restore } = installFetch(200, {
      success: true,
      data: { revision: 2, savedAtMs: 5, document: PAYLOAD },
    });
    const result = await fetchMappingSync('http://host', 'tok', isMappingDocument);
    restore();
    check('get parses the success fixture', result.revision === 2 && result.savedAtMs === 5
      && result.document !== null && JSON.stringify(result.document) === JSON.stringify(PAYLOAD),
    JSON.stringify(result));
    check('get posts the chat_mapping_get envelope', calls.length === 1
      && calls[0].input === 'http://host/rpc'
      && JSON.parse(String(calls[0].init?.body)).type === 'chat_mapping_get'
      && (calls[0].init?.headers as Record<string, string>).Authorization === 'Bearer tok',
    JSON.stringify(calls));
  }

  {
    const { restore } = installFetch(200, { success: true, data: { revision: 0, savedAtMs: 0, document: null } });
    const result = await fetchMappingSync('http://host', 'tok', isMappingDocument);
    restore();
    check('get parses a null document', result.document === null && result.revision === 0, JSON.stringify(result));
  }

  {
    const { restore } = installFetch(200, { success: false, error: 'listener token rejected' });
    const message = await rejection(fetchMappingSync('http://host', 'tok', isMappingDocument));
    restore();
    check('get surfaces success:false with the body error', message === 'listener token rejected', message);
  }

  {
    const { restore } = installFetch(401, { success: false, error: 'unauthorized' });
    const message = await rejection(fetchMappingSync('http://host', 'tok', isMappingDocument));
    restore();
    check('get throws with the HTTP status', message === 'chat_mapping_get failed: 401', message);
  }

  {
    const { restore } = installFetch(200, {
      success: true,
      data: { revision: 2, savedAtMs: 5, document: { version: 1, projects: 'x', mappings: [] } },
    });
    const message = await rejection(fetchMappingSync('http://host', 'tok', isMappingDocument));
    restore();
    check('get rejects a malformed document instead of fabricating', message === 'chat_mapping_get returned an invalid payload.', message);
  }

  {
    const { restore } = installFetch(200, { success: true, data: { savedAtMs: 5, document: null } });
    const message = await rejection(fetchMappingSync('http://host', 'tok', isMappingDocument));
    restore();
    check('get rejects a payload missing revision', message === 'chat_mapping_get returned an invalid payload.', message);
  }

  {
    const { calls, restore } = installFetch(200, { success: true, data: { ok: true, revision: 3, savedAtMs: 7 } });
    const result = await pushMappingSync('http://host', 'tok', PAYLOAD, 2, isMappingDocument);
    restore();
    check('put parses the ok result', result.ok === true && result.revision === 3 && result.savedAtMs === 7, JSON.stringify(result));
    check('put posts the document and baseRevision', calls.length === 1
      && JSON.parse(String(calls[0].init?.body)).type === 'chat_mapping_put'
      && JSON.parse(String(calls[0].init?.body)).baseRevision === 2
      && JSON.stringify(JSON.parse(String(calls[0].init?.body)).document) === JSON.stringify(PAYLOAD),
    JSON.stringify(calls));
  }

  {
    const { restore } = installFetch(200, {
      success: true,
      data: { conflict: true, revision: 4, savedAtMs: 8, document: PAYLOAD },
    });
    const result = await pushMappingSync('http://host', 'tok', PAYLOAD, 1, isMappingDocument);
    restore();
    check('put parses the conflict fixture', result.conflict === true && result.revision === 4
      && result.savedAtMs === 8
      && 'document' in result && JSON.stringify(result.document) === JSON.stringify(PAYLOAD),
    JSON.stringify(result));
  }

  {
    const { restore } = installFetch(200, { success: false, error: 'put rejected' });
    const message = await rejection(pushMappingSync('http://host', 'tok', PAYLOAD, 2, isMappingDocument));
    restore();
    check('put surfaces success:false with the body error', message === 'put rejected', message);
  }

  {
    const { restore } = installFetch(200, { success: true, data: { revision: 3, savedAtMs: 7 } });
    const message = await rejection(pushMappingSync('http://host', 'tok', PAYLOAD, 2, isMappingDocument));
    restore();
    check('put rejects a payload that is neither ok nor conflict', message === 'chat_mapping_put returned an invalid payload.', message);
  }

  {
    const { restore } = installFetch(501, { success: false, error: 'unavailable' });
    const message = await rejection(pushMappingSync('http://host', 'tok', PAYLOAD, 2, isMappingDocument));
    restore();
    check('put throws with the HTTP status', message === 'chat_mapping_put failed: 501', message);
  }

  check('isSyncPayload accepts a valid document', isSyncPayload(PAYLOAD, isMappingDocument) === true);
  check('isSyncPayload rejects a non-object', isSyncPayload('x', isMappingDocument) === false);
  check('isSyncPayload rejects non-array runners', isSyncPayload(
    { mapping: SYNC_DOCUMENT, runners: 'x', tokens: {} }, isMappingDocument) === false);
  check('isSyncPayload rejects non-string tokens', isSyncPayload(
    { mapping: SYNC_DOCUMENT, runners: [], tokens: { 'h:1': 5 } }, isMappingDocument) === false);
  check('isSyncPayload rejects a mapping the validator declines', isSyncPayload(
    { mapping: { version: 1, projects: 'x', mappings: [] }, runners: [], tokens: {} }, isMappingDocument) === false);

  check('TypeError classifies as network failure', isSyncNetworkFailure(new TypeError('fetch failed')));
  check('404 classifies as network failure', isSyncNetworkFailure(new Error('chat_mapping_get failed: 404')));
  check('501 classifies as network failure', isSyncNetworkFailure(new Error('chat_mapping_put failed: 501')));
  check('401 does not classify as network failure', !isSyncNetworkFailure(new Error('chat_mapping_get failed: 401')));
  check('500 does not classify as network failure', !isSyncNetworkFailure(new Error('chat_mapping_put failed: 500')));
  check('payload-shape errors do not classify as network failure', !isSyncNetworkFailure(new Error('chat_mapping_get returned an invalid payload.')));

  {
    const values = new Map<string, string>();
    const store: StorageLike = {
      getItem: (key) => values.get(key) ?? null,
      setItem: (key, value) => { values.set(key, value); },
      removeItem: (key) => { values.delete(key); },
    };
    applyRunnerTokens(store, { 'h:1': 'synced' });
    check('empty local token is replaced by the synced token', loadTokenForAuthority(store, 'h:1') === 'synced');
    saveTokenForAuthority(store, 'h:1', 'stale');
    applyRunnerTokens(store, { 'h:1': 'synced' });
    check('mismatching nonempty local token is replaced by the synced token', loadTokenForAuthority(store, 'h:1') === 'synced');
    applyRunnerTokens(store, { 'h:1': 'synced' });
    check('matching nonempty local token stays the synced value', loadTokenForAuthority(store, 'h:1') === 'synced');
    applyRunnerTokens(store, { 'h:1': '' });
    check('empty synced token does not clear a nonempty local token', loadTokenForAuthority(store, 'h:1') === 'synced');
    check('token key is host-scoped', store.getItem(hostTokenKey('h:1')) === 'synced');
  }

  {
    const here = dirname(fileURLToPath(import.meta.url));
    const app = readFileSync(join(here, '../src/chat/ChatApp.tsx'), 'utf8');
    const keyAt = app.indexOf('const runnerRecordKey');
    const effectAt = app.indexOf('useEffect(() => {', keyAt);
    const keyBlock = keyAt >= 0 && effectAt > keyAt ? app.slice(keyAt, effectAt) : '';
    check('push fingerprint includes collectRunnerTokens', keyBlock.includes('tokens: collectRunnerTokens(storage)'));
    check('push effect still keys off runnerRecordKey', app.includes('}, [mapping.projects, mapping.mappings, pageAuthority, mappingStore, registry, storage, runnerRecordKey]);'));
  }

  console.log(`\nchatSync.test: ${assertions} assertions, ${failures.length} failure(s)`);
  if (failures.length) {
    console.error(failures.join('\n'));
    process.exit(1);
  }
}

main().catch((error) => {
  console.error(`chatSync.test: ${error instanceof Error ? error.message : String(error)}`);
  process.exit(1);
});