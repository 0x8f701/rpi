// Registry pairing behavior: the product dedup contract (one runner per network
// authority) meets credential transport. Every assertion watches a CONSUMER of
// the record — the socket the client dials (URL scheme plus the `rpi-auth`
// subprotocol the credential rides), the runner/token documents a reload
// restores from, and the token map a sync push carries — never a private field
// of the registry. The last block drives the real `runMappingSync` boot path
// with only its two HTTP transports mocked.
import AsyncStorage from '@react-native-async-storage/async-storage';
import { fetchMappingSync, pushMappingSync } from '../src/chatSync';
import { runMappingSync } from '../src/mappingSync';
import { parseRunnerInviteLink } from '../src/protocol';
import { RUNNER_STORAGE_KEY, RunnerRegistry, TOKEN_STORAGE_KEY } from '../src/registry';
import { MappingStore } from '../src/store';

jest.mock('../src/chatSync', () => ({
  ...jest.requireActual('../src/chatSync'),
  fetchMappingSync: jest.fn(),
  pushMappingSync: jest.fn(),
}));

const fetchSync = fetchMappingSync as unknown as jest.Mock;
const pushSync = pushMappingSync as unknown as jest.Mock;

/** Stand-in for the React Native WebSocket global: records every dial with the
 *  protocol list the credential rides on, keeps the outbound frames, and lets a
 *  test observe that a retired socket really closed. */
class FakeSocket {
  static readonly CONNECTING = 0;
  static readonly OPEN = 1;
  static readonly CLOSING = 2;
  static readonly CLOSED = 3;
  static instances: FakeSocket[] = [];

  readyState: number = FakeSocket.CONNECTING;
  readonly sent: string[] = [];
  onopen: (() => void) | null = null;
  onmessage: ((event: { data: string }) => void) | null = null;
  onclose: ((event: { code: number; reason: string }) => void) | null = null;
  onerror: (() => void) | null = null;

  constructor(readonly url: string, readonly protocols: string[] = []) {
    FakeSocket.instances.push(this);
  }

  send(payload: string): void {
    this.sent.push(payload);
  }

  close(): void {
    this.readyState = FakeSocket.CLOSED;
  }
}

const transportGlobal = globalThis as { WebSocket?: unknown };
const realWebSocket = transportGlobal.WebSocket;

beforeAll(() => {
  transportGlobal.WebSocket = FakeSocket;
});

afterAll(() => {
  transportGlobal.WebSocket = realWebSocket;
});

beforeEach(async () => {
  FakeSocket.instances = [];
  fetchSync.mockReset();
  pushSync.mockReset();
  pushSync.mockResolvedValue({ ok: true, revision: 2, savedAtMs: 2 });
  await AsyncStorage.clear();
});

// TEST-NET-1 (RFC 5737): reserved for documentation, so no real runner address
// can appear in a fixture.
const AUTHORITY = '192.0.2.15:8765';

/** The socket of the most recent dial: `connect` dials once per pairing, so a
 *  stale endpoint or a duplicate client shows up as the wrong socket here. */
function lastSocket(): FakeSocket {
  const socket = FakeSocket.instances[FakeSocket.instances.length - 1];
  if (!socket) throw new Error('the registry dialed no socket');
  return socket;
}

async function storedRunners(): Promise<Array<Record<string, unknown>>> {
  const raw = await AsyncStorage.getItem(RUNNER_STORAGE_KEY);
  const parsed = raw ? JSON.parse(raw) : null;
  return Array.isArray(parsed?.runners) ? parsed.runners : [];
}

async function storedTokens(): Promise<Record<string, string>> {
  const raw = await AsyncStorage.getItem(TOKEN_STORAGE_KEY);
  const parsed = raw ? JSON.parse(raw) : null;
  return parsed?.tokens ?? {};
}

it('re-points the existing HTTP runner to a later HTTPS pairing and its credential', async () => {
  const registry = new RunnerRegistry();
  const { endpoint: httpEndpoint, token: httpToken } = parseRunnerInviteLink(`http://${AUTHORITY}/chat#old-token`);
  const httpRecord = await registry.add(httpEndpoint, httpToken, 'psy');
  const httpClient = registry.client(httpRecord.id);
  const httpSocket = lastSocket();
  expect(httpSocket.url).toBe(`ws://${AUTHORITY}/ws`);
  expect(httpSocket.protocols).toEqual(['rpi-auth.old-token']);

  const { endpoint: httpsEndpoint, token: httpsToken } = parseRunnerInviteLink(`https://${AUTHORITY}/chat#new-token`);
  const httpsRecord = await registry.add(httpsEndpoint, httpsToken, 'psy');
  const httpsSocket = lastSocket();

  // One row, one client, one dial: the HTTP socket is retired, and the new
  // credential never rides it.
  expect(httpsRecord.id).toBe(httpRecord.id);
  expect(registry.snapshot()).toHaveLength(1);
  expect(FakeSocket.instances).toHaveLength(2);
  expect(httpSocket.readyState).toBe(FakeSocket.CLOSED);
  expect(registry.client(httpsRecord.id)).not.toBe(httpClient);
  expect(httpsSocket.url).toBe(`wss://${AUTHORITY}/ws`);
  expect(httpsSocket.protocols).toEqual(['rpi-auth.new-token']);

  // The record consumers (workspace uploads post to view.origin with
  // view.token) see the paired endpoint, not the stale one.
  expect(registry.snapshot()[0].origin).toBe(`https://${AUTHORITY}`);
  expect(registry.snapshot()[0].token).toBe('new-token');

  const rows = await storedRunners();
  expect(rows).toHaveLength(1);
  expect(rows[0].origin).toBe(`https://${AUTHORITY}`);
  expect(rows[0].authority).toBe(AUTHORITY);
  expect(await storedTokens()).toEqual({ [AUTHORITY]: 'new-token' });

  // A reload reconnects on the paired endpoint with the paired credential.
  const restored = new RunnerRegistry();
  await restored.load();
  restored.connectAll();
  const restoredSocket = lastSocket();
  expect(restored.snapshot()).toHaveLength(1);
  expect(restoredSocket.url).toBe(`wss://${AUTHORITY}/ws`);
  expect(restoredSocket.protocols).toEqual(['rpi-auth.new-token']);
});

it('never replays an HTTPS credential on the plaintext endpoint the user paired', async () => {
  const registry = new RunnerRegistry();
  const { endpoint: httpsEndpoint, token: httpsToken } = parseRunnerInviteLink(`https://${AUTHORITY}/chat#https-token`);
  await registry.add(httpsEndpoint, httpsToken, 'psy');
  const httpsSocket = lastSocket();

  const { endpoint: httpEndpoint, token: httpToken } = parseRunnerInviteLink(`http://${AUTHORITY}/chat`);
  expect(httpToken).toBe('');
  const httpRecord = await registry.add(httpEndpoint, httpToken, 'psy');
  const httpSocket = lastSocket();

  expect(registry.snapshot()).toHaveLength(1);
  expect(httpsSocket.readyState).toBe(FakeSocket.CLOSED);
  expect(httpSocket.url).toBe(`ws://${AUTHORITY}/ws`);
  expect(httpSocket.protocols).toEqual([]);
  expect(registry.snapshot()[0].origin).toBe(`http://${AUTHORITY}`);
  expect(registry.snapshot()[0].token).toBe('');

  // Neither the sync document nor the persisted token map may carry the
  // retired HTTPS credential.
  expect(registry.tokenMap()).toEqual({});
  expect(await storedTokens()).toEqual({});
  expect((await storedRunners())[0].origin).toBe(`http://${AUTHORITY}`);

  const restored = new RunnerRegistry();
  await restored.load();
  restored.connectAll();
  const restoredSocket = lastSocket();
  expect(restored.client(httpRecord.id)).not.toBeNull();
  expect(restoredSocket.url).toBe(`ws://${AUTHORITY}/ws`);
  expect(restoredSocket.protocols).toEqual([]);
});

it('keeps the credential of a re-pairing that supplies no token on the same origin', async () => {
  const registry = new RunnerRegistry();
  const { endpoint, token } = parseRunnerInviteLink(`https://${AUTHORITY}/chat#https-token`);
  await registry.add(endpoint, token, 'psy');

  const { endpoint: bareEndpoint, token: bareToken } = parseRunnerInviteLink(`https://${AUTHORITY}/chat`);
  await registry.add(bareEndpoint, bareToken, 'psy');
  const socket = lastSocket();

  expect(registry.snapshot()).toHaveLength(1);
  expect(socket.url).toBe(`wss://${AUTHORITY}/ws`);
  expect(socket.protocols).toEqual(['rpi-auth.https-token']);
  expect(await storedTokens()).toEqual({ [AUTHORITY]: 'https-token' });

  const { endpoint: replacedEndpoint, token: replacedToken } = parseRunnerInviteLink(`https://${AUTHORITY}/web#replacement-token`);
  await registry.add(replacedEndpoint, replacedToken, 'psy');
  const replacedSocket = lastSocket();

  expect(registry.snapshot()).toHaveLength(1);
  expect(registry.snapshot()[0].label).toBe('psy');
  expect(replacedSocket.url).toBe(`wss://${AUTHORITY}/ws`);
  expect(replacedSocket.protocols).toEqual(['rpi-auth.replacement-token']);
  expect(await storedTokens()).toEqual({ [AUTHORITY]: 'replacement-token' });
});

describe('synced credential adoption', () => {
  const PEER = '192.0.2.16:8765';
  const VARIANT = '192.0.2.17:8765';
  const FRESH = '192.0.2.18:8765';
  const PEER_ORIGIN = `https://${PEER}`;

  /** A profile whose runner was paired over HTTPS and then re-paired to
   *  plaintext without a token, plus one peer runner that still holds a
   *  credential — the peer is what makes `runMappingSync('boot')` fetch and
   *  adopt a remote document. */
  async function downgradedProfile(): Promise<{ registry: RunnerRegistry; store: MappingStore }> {
    const registry = new RunnerRegistry();
    const { endpoint: secure, token: secureToken } = parseRunnerInviteLink(`https://${AUTHORITY}/chat#https-token`);
    await registry.add(secure, secureToken, 'psy');
    const { endpoint: plaintext, token: noToken } = parseRunnerInviteLink(`http://${AUTHORITY}/chat`);
    await registry.add(plaintext, noToken, 'psy');
    const { endpoint: peer, token: peerToken } = parseRunnerInviteLink(`${PEER_ORIGIN}/chat#peer-token`);
    await registry.add(peer, peerToken, 'peer');
    return { registry, store: new MappingStore() };
  }

  /** The document a remote profile pushes: its runner rows plus the
   *  authority-keyed token map that rode with them. */
  function syncedDocument(
    rows: Array<{ id: string; origin: string; authority: string }>,
    tokens: Record<string, string>,
  ): unknown {
    return {
      mapping: { version: 1, projects: [], mappings: [] },
      runners: rows.map((row) => ({ label: 'psy', createdAt: 1, ...row })),
      tokens,
    };
  }

  it('refuses a synced HTTPS credential for a local row downgraded to plaintext', async () => {
    const { registry, store } = await downgradedProfile();
    fetchSync.mockResolvedValue({
      revision: 1,
      savedAtMs: 1,
      document: syncedDocument(
        [{ id: 'remote-psy', origin: `https://${AUTHORITY}`, authority: AUTHORITY }],
        { [AUTHORITY]: 'remote-https-token' },
      ),
    });

    await runMappingSync(registry, store, 'boot');

    // The adoption really ran: the peer's own credential drove the fetch.
    expect(fetchSync).toHaveBeenCalledTimes(1);
    expect(fetchSync.mock.calls[0][0]).toBe(PEER_ORIGIN);
    expect(fetchSync.mock.calls[0][1]).toBe('peer-token');

    // The document pairs that authority with an HTTPS origin, so its token is
    // never adopted by the local plaintext row — and no second row appears.
    expect(registry.snapshot()).toHaveLength(2);
    const downgraded = registry.snapshot().find((row) => row.authority === AUTHORITY);
    expect(downgraded?.origin).toBe(`http://${AUTHORITY}`);
    expect(downgraded?.token).toBe('');
    expect(registry.tokenMap()).toEqual({ [PEER]: 'peer-token' });
    expect(await storedTokens()).toEqual({ [PEER]: 'peer-token' });

    // What the fix exists for: the next dial (and the next upload, which posts
    // to view.origin with view.token) carries no credential, while the peer
    // keeps its own.
    const dialsBefore = FakeSocket.instances.length;
    const restored = new RunnerRegistry();
    await restored.load();
    restored.connectAll();
    expect(FakeSocket.instances.slice(dialsBefore).map((socket) => [socket.url, socket.protocols])).toEqual([
      [`ws://${AUTHORITY}/ws`, []],
      [`wss://${PEER}/ws`, ['rpi-auth.peer-token']],
    ]);
  });

  it('adopts a synced credential the document pairs with the same endpoint', async () => {
    const { registry, store } = await downgradedProfile();
    const { endpoint: variant, token: noVariantToken } = parseRunnerInviteLink(`http://${VARIANT}/chat`);
    await registry.add(variant, noVariantToken, 'variant');
    fetchSync.mockResolvedValue({
      revision: 1,
      savedAtMs: 1,
      document: syncedDocument(
        [
          { id: 'remote-psy', origin: `http://${AUTHORITY}`, authority: AUTHORITY },
          { id: 'remote-variant', origin: `http://${VARIANT}/`, authority: VARIANT },
          { id: 'remote-fresh', origin: `https://${FRESH}`, authority: FRESH },
        ],
        { [AUTHORITY]: 'remote-http-token', [VARIANT]: 'remote-variant-token', [FRESH]: 'remote-fresh-token' },
      ),
    });

    await runMappingSync(registry, store, 'boot');

    expect(fetchSync).toHaveBeenCalledTimes(1);
    expect(registry.tokenMap()).toEqual({
      [AUTHORITY]: 'remote-http-token',
      [VARIANT]: 'remote-variant-token',
      [PEER]: 'peer-token',
      [FRESH]: 'remote-fresh-token',
    });
    expect(await storedTokens()).toEqual(registry.tokenMap());

    // Every row dials its own endpoint with its own credential: the adopted
    // tokens (same origin, and the same endpoint written with a trailing
    // slash), the untouched peer, and the freshly inserted authority.
    const dialsBefore = FakeSocket.instances.length;
    const restored = new RunnerRegistry();
    await restored.load();
    restored.connectAll();
    expect(FakeSocket.instances.slice(dialsBefore).map((socket) => [socket.url, socket.protocols])).toEqual([
      [`ws://${AUTHORITY}/ws`, ['rpi-auth.remote-http-token']],
      [`wss://${PEER}/ws`, ['rpi-auth.peer-token']],
      [`ws://${VARIANT}/ws`, ['rpi-auth.remote-variant-token']],
      [`wss://${FRESH}/ws`, ['rpi-auth.remote-fresh-token']],
    ]);
  });
});
