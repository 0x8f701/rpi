#!/usr/bin/env node
import { hostTokenKey, type StorageLike } from '../src/hostToken.ts';
import {
  DiscoveryClient,
  discoveredAlreadyAdded,
  discoveredHttpsOnHttpPage,
  fetchInviteCandidateStatus,
  originAuthority,
  type DiscoveredRunner,
} from '../src/chat/discovery.ts';
import {
  RUNNER_AUTH_PROTOCOL_PREFIX,
  RUNNER_RECONNECT_INITIAL_MS,
  RUNNER_RECONNECT_MAX_MS,
} from '../src/chat/protocol.ts';

const failures: string[] = [];
let assertions = 0;
let activeBlock = 'module initialization';

const PAGE_HOST = '192.0.2.10:8765';
const SCOPED_TOKEN = 'scoped-secret';
const URL_TOKEN = 'url-secret';

function check(name: string, condition: boolean, detail = ''): void {
  assertions += 1;
  if (!condition) failures.push(`${activeBlock}: ${name}${detail ? ` (${detail})` : ''}`);
}

function sample(origin: string, host = '192.0.2.10', port = 8765): DiscoveredRunner {
  return { origin, host, port, label: 'dev box', version: '0.2.11', scheme: 'wss' };
}

function storage(): StorageLike & { values: Map<string, string> } {
  const values = new Map<string, string>();
  return {
    values,
    getItem: (key) => values.get(key) ?? null,
    setItem: (key, value) => { values.set(key, value); },
    removeItem: (key) => { values.delete(key); },
  };
}

interface FakeTimer {
  id: number;
  delay: number;
  handler: () => void;
}

function timers() {
  let nextId = 0;
  const scheduled = new Map<number, FakeTimer>();
  return {
    scheduled,
    setTimeoutFn: (handler: () => void, delay: number) => {
      const id = ++nextId;
      scheduled.set(id, { id, delay, handler });
      return id;
    },
    clearTimeoutFn: (id: number) => { scheduled.delete(id); },
    runDelay: (delay: number) => {
      const timer = [...scheduled.values()].find((candidate) => candidate.delay === delay);
      if (!timer) return false;
      scheduled.delete(timer.id);
      timer.handler();
      return true;
    },
  };
}

class FakeWebSocket {
  static readonly CONNECTING = 0;
  static readonly OPEN = 1;
  static readonly CLOSING = 2;
  static readonly CLOSED = 3;
  readyState = FakeWebSocket.CONNECTING;
  protocol = '';
  onopen: ((event: Event) => void) | null = null;
  onmessage: ((event: MessageEvent) => void) | null = null;
  onerror: ((event: Event) => void) | null = null;
  onclose: ((event: CloseEvent) => void) | null = null;
  sent: string[] = [];
  closed = false;

  constructor(readonly url: string, readonly protocols: string[]) {}

  open(): void {
    this.readyState = FakeWebSocket.OPEN;
    this.protocol = this.protocols[0] ?? '';
    this.onopen?.({ target: this } as unknown as Event);
  }

  send(frame: string): void {
    if (this.readyState !== FakeWebSocket.OPEN) throw new Error('not open');
    this.sent.push(frame);
  }

  respondDiscovered(runners: unknown[]): void {
    this.deliver(this.snapshotResponse(runners));
  }

  snapshotResponse(runners: unknown[]): string {
    const sent = this.sent
      .map((frame) => JSON.parse(frame) as { id?: string; type?: string })
      .find((frame) => frame.type === 'get_discovered_runners');
    if (!sent?.id) throw new Error(`missing get_discovered_runners on ${this.url}`);
    return JSON.stringify({
      type: 'response',
      id: sent.id,
      command: 'get_discovered_runners',
      success: true,
      data: { runners },
    });
  }

  emitDiscovered(runner: DiscoveredRunner): void {
    this.deliver(JSON.stringify({ type: 'runner_discovered', ...runner }));
  }

  emitGone(origin: string): void {
    this.deliver(JSON.stringify({ type: 'runner_gone', origin }));
  }

  deliver(data: string): void {
    this.onmessage?.({
      target: this,
      data,
    } as MessageEvent);
  }

  close(code = 1000, reason = ''): void {
    this.closed = true;
    this.readyState = FakeWebSocket.CLOSED;
    this.onclose?.({ target: this, code, reason } as CloseEvent);
  }
}

function installWindow(search = `?token=${URL_TOKEN}`): {
  location: { protocol: string; host: string };
  sockets: FakeWebSocket[];
} {
  const location = {
    protocol: 'https:',
    host: PAGE_HOST,
    search,
  };
  const sockets: FakeWebSocket[] = [];
  class WindowWebSocket extends FakeWebSocket {
    constructor(url: string, protocols: string[] = []) {
      super(url, protocols);
      sockets.push(this);
    }
  }
  Object.defineProperty(globalThis, 'window', {
    configurable: true,
    value: { location, localStorage: null },
  });
  Object.defineProperty(globalThis, 'WebSocket', {
    configurable: true,
    value: WindowWebSocket,
  });
  return { location, sockets };
}
async function main(): Promise<void> {
  activeBlock = 'origin authority';
  check('https origin extracts host:port', originAuthority('https://192.0.2.10:8765') === '192.0.2.10:8765');
  check('wss origin extracts host:port', originAuthority('wss://192.0.2.10:8765') === '192.0.2.10:8765');
  check('bare host:port is unchanged', originAuthority('192.0.2.10:8765') === '192.0.2.10:8765');

  {
    activeBlock = 'invite candidate reuses status';
    const fetched: string[] = [];
    const fetchFn = (async (input: RequestInfo | URL) => {
      const url = String(input);
      fetched.push(url);
      if (url === 'https://192.0.2.15:8765/status') {
        return {
          ok: true,
          json: async () => ({ tokenRequired: false, version: '0.2.11', codename: 'alpha-one' }),
        } as Response;
      }
      throw new Error('refused');
    }) as typeof fetch;
    const reachable = await fetchInviteCandidateStatus('192.0.2.15:8765', fetchFn, 'https:');
    check('invite reachable uses https page default', fetched[0] === 'https://192.0.2.15:8765/status', fetched[0] ?? '');
    check('invite reachable returns status', reachable.reachable === true && reachable.reachable && reachable.status.tokenRequired === false);
    check('invite reachable keeps codename', reachable.reachable && reachable.status.codename === 'alpha-one');
    const unreachable = await fetchInviteCandidateStatus('http://192.0.2.99:8765', fetchFn, 'https:');
    check('invite unreachable stays false', unreachable.reachable === false);
    check('invite unreachable does not invent token copy', fetched.includes('http://192.0.2.99:8765/status'));
    check('http page does not probe https discovered origins', discoveredHttpsOnHttpPage('https://192.0.2.15:8765', 'http:') === true);
    check('https page still probes https discovered origins', discoveredHttpsOnHttpPage('https://192.0.2.15:8765', 'https:') === false);
    check('http page still probes http discovered origins', discoveredHttpsOnHttpPage('http://192.0.2.15:8765', 'http:') === false);
    const httpSkipFetched: string[] = [];
    const httpSkipFetch = (async (input: RequestInfo | URL) => {
      httpSkipFetched.push(String(input));
      throw new Error('http page must not fetch https discovered status');
    }) as typeof fetch;
    const skipped = await fetchInviteCandidateStatus('https://192.0.2.15:8765', httpSkipFetch, 'http:');
    check('http page marks https discovered origin unreachable without fetch', skipped.reachable === false && httpSkipFetched.length === 0);
  }

  {
    activeBlock = 'discovery codename parse';
    const { location } = installWindow('');
    const store = storage();
    const sockets: FakeWebSocket[] = [];
    const seen: DiscoveredRunner[][] = [];
    const client = new DiscoveryClient({
      storage: store,
      location,
      webSocketFactory: (url, protocols) => {
        const socket = new FakeWebSocket(url, protocols);
        sockets.push(socket);
        return socket as unknown as WebSocket;
      },
    });
    client.subscribe((runners) => { seen.push(runners); });
    client.start();
    sockets[0].open();
    sockets[0].respondDiscovered([
      { origin: 'https://192.0.2.20:8765', host: '192.0.2.20', port: 8765, label: 'named', version: '0.2.11', scheme: 'wss', codename: 'alpha-one' },
      { origin: 'https://192.0.2.21:8765', host: '192.0.2.21', port: 8765, label: 'nulled', version: '0.2.11', scheme: 'wss', codename: null },
      { origin: 'https://192.0.2.22:8765', host: '192.0.2.22', port: 8765, label: 'missing', version: '0.2.11', scheme: 'wss' },
      { origin: 'https://192.0.2.23:8765', host: '192.0.2.23', port: 8765, label: 'malformed', version: '0.2.11', scheme: 'wss', codename: 42 },
      { origin: 'https://192.0.2.24:8765', host: '192.0.2.24', port: 8765, label: 'blank', version: '0.2.11', scheme: 'wss', codename: '   ' },
    ]);
    const latest = seen.at(-1) ?? [];
    const byLabel = (label: string) => latest.find((runner) => runner.label === label);
    check('string codename is parsed', byLabel('named')?.codename === 'alpha-one', byLabel('named')?.codename ?? '');
    check('null codename stays null', byLabel('nulled')?.codename === null, String(byLabel('nulled')?.codename));
    check('missing codename defaults to null', byLabel('missing')?.codename === null, String(byLabel('missing')?.codename));
    check('malformed codename defaults to null', byLabel('malformed')?.codename === null, String(byLabel('malformed')?.codename));
    check('blank codename defaults to null', byLabel('blank')?.codename === null, String(byLabel('blank')?.codename));
    check('all five discovery rows survive parsing', latest.length === 5, String(latest.length));
    client.stop();
  }

  {
    activeBlock = 'discovery event codename parse';
    const { location } = installWindow('');
    const store = storage();
    const sockets: FakeWebSocket[] = [];
    const seen: DiscoveredRunner[][] = [];
    const client = new DiscoveryClient({
      storage: store,
      location,
      webSocketFactory: (url, protocols) => {
        const socket = new FakeWebSocket(url, protocols);
        sockets.push(socket);
        return socket as unknown as WebSocket;
      },
    });
    client.subscribe((runners) => { seen.push(runners); });
    client.start();
    sockets[0].open();
    sockets[0].respondDiscovered([]);
    sockets[0].emitDiscovered({
      origin: 'https://192.0.2.25:8765',
      host: '192.0.2.25',
      port: 8765,
      label: 'live named',
      version: '0.2.11',
      scheme: 'wss',
      codename: 'beta-two',
    });
    sockets[0].emitDiscovered({
      origin: 'https://192.0.2.26:8765',
      host: '192.0.2.26',
      port: 8765,
      label: 'live null',
      version: '0.2.11',
      scheme: 'wss',
      codename: null,
    });
    const latest = seen.at(-1) ?? [];
    check('live event string codename is parsed', latest.find((runner) => runner.label === 'live named')?.codename === 'beta-two');
    check('live event null codename stays null', latest.find((runner) => runner.label === 'live null')?.codename === null);
    client.stop();
  }

  activeBlock = 'already added';
  check(
    'https discovery matches stored https origin',
    discoveredAlreadyAdded(sample('https://192.0.2.10:8765'), ['https://192.0.2.10:8765']),
  );
  check(
    'wss gone origin still matches stored https runner',
    discoveredAlreadyAdded(sample('wss://192.0.2.10:8765'), ['https://192.0.2.10:8765']),
  );
  check(
    'different host stays a candidate',
    !discoveredAlreadyAdded(sample('https://192.0.2.11:8765', '192.0.2.11'), ['https://192.0.2.10:8765']),
  );

  {
    activeBlock = 'authority token, URL token ignored';
    const { location, sockets } = installWindow();
    const store = storage();
    store.setItem(hostTokenKey(PAGE_HOST), SCOPED_TOKEN);
    const client = new DiscoveryClient({ storage: store, location });
    client.start();
    check('connect opens one serving-listener socket', sockets.length === 1, String(sockets.length));
    check(
      'websocket URL has no query string',
      sockets[0].url === `wss://${PAGE_HOST}/ws`,
      sockets[0].url,
    );
    check(
      'authority-scoped token is the rpi-auth subprotocol',
      sockets[0].protocols.length === 1
        && sockets[0].protocols[0] === `${RUNNER_AUTH_PROTOCOL_PREFIX}${SCOPED_TOKEN}`,
      sockets[0].protocols.join(','),
    );
    check(
      'URL ?token= is not used as the websocket subprotocol',
      !sockets[0].protocols.some((protocol) => protocol.includes(URL_TOKEN)),
      sockets[0].protocols.join(','),
    );
    client.stop();
  }

  {
    activeBlock = 'URL token alone is ignored';
    const { location, sockets } = installWindow();
    const store = storage();
    const client = new DiscoveryClient({ storage: store, location });
    client.start();
    check(
      'empty authority storage opens an unauthenticated socket even when URL has ?token=',
      sockets[0].protocols.length === 0,
      sockets[0].protocols.join(','),
    );
    client.stop();
  }

  {
    activeBlock = 'close reconnects with backoff and fresh snapshot';
    const { location } = installWindow('');
    const store = storage();
    store.setItem(hostTokenKey(PAGE_HOST), SCOPED_TOKEN);
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const seen: DiscoveredRunner[][] = [];
    const first = sample('https://192.0.2.11:8765', '192.0.2.11', 8765);
    const second = sample('https://192.0.2.12:8765', '192.0.2.12', 8765);
    const client = new DiscoveryClient({
      storage: store,
      location,
      webSocketFactory: (url, protocols) => {
        const socket = new FakeWebSocket(url, protocols);
        sockets.push(socket);
        return socket as unknown as WebSocket;
      },
      setTimeoutFn: fakeTimers.setTimeoutFn,
      clearTimeoutFn: fakeTimers.clearTimeoutFn,
    });
    client.subscribe((runners) => { seen.push(runners); });
    client.start();
    sockets[0].open();
    check(
      'first generation requests get_discovered_runners',
      sockets[0].sent.some((frame) => JSON.parse(frame).type === 'get_discovered_runners'),
    );
    sockets[0].respondDiscovered([first]);
    check('first snapshot is emitted', seen.at(-1)?.[0]?.origin === first.origin, seen.at(-1)?.[0]?.origin ?? '');

    sockets[0].emitDiscovered(second);
    check(
      'live events apply after the generation snapshot',
      seen.at(-1)?.some((runner) => runner.origin === second.origin) === true,
    );

    sockets[0].close(1006, '');
    check('close schedules the initial reconnect delay', [...fakeTimers.scheduled.values()][0]?.delay === RUNNER_RECONNECT_INITIAL_MS);
    check('close does not open the next socket until backoff fires', sockets.length === 1, String(sockets.length));
    check('reconnect timer creates a replacement socket', fakeTimers.runDelay(RUNNER_RECONNECT_INITIAL_MS));
    check('reconnect opened a second generation', sockets.length === 2, String(sockets.length));
    check(
      'reconnect still uses the authority-scoped rpi-auth subprotocol',
      sockets[1].protocols[0] === `${RUNNER_AUTH_PROTOCOL_PREFIX}${SCOPED_TOKEN}`,
      sockets[1].protocols.join(','),
    );
    sockets[1].open();
    check(
      'new generation re-fetches get_discovered_runners',
      sockets[1].sent.some((frame) => JSON.parse(frame).type === 'get_discovered_runners'),
    );

    const snapshotPayload = sockets[1].snapshotResponse([second]);
    const lateDiscovered = sample('https://192.0.2.13:8765', '192.0.2.13', 8765);
    const emitsBeforeReplay = seen.length;
    const rosterBeforeReplay = seen.at(-1) ?? [];
    sockets[1].emitDiscovered(lateDiscovered);
    sockets[1].emitGone(second.origin);
    check(
      'buffered events do not mutate state before snapshot install',
      seen.length === emitsBeforeReplay
        && (seen.at(-1) ?? []).every((runner, index) => runner.origin === rosterBeforeReplay[index]?.origin),
      (seen.at(-1) ?? []).map((runner) => runner.origin).join(','),
    );

    sockets[1].deliver(snapshotPayload);
    const latest = seen.at(-1) ?? [];
    check('snapshot install emits once after replay', seen.length === emitsBeforeReplay + 1, String(seen.length));
    check('replayed runner_discovered is applied after snapshot install', latest.some((runner) => runner.origin === lateDiscovered.origin));
    check('replayed runner_gone is applied after snapshot install', !latest.some((runner) => runner.origin === second.origin));
    check('fresh snapshot keeps only replayed post-capture rows', latest.length === 1, String(latest.length));
    client.stop();
  }

  {
    activeBlock = 'failed generations back off then snapshot resets delay';
    const { location } = installWindow('');
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const delays: number[] = [];
    const expected = [250, 500, 1000, 2000, 4000, 8000, 15000, 15000];
    const client = new DiscoveryClient({
      storage: store,
      location,
      webSocketFactory: (url, protocols) => {
        const socket = new FakeWebSocket(url, protocols);
        sockets.push(socket);
        return socket as unknown as WebSocket;
      },
      setTimeoutFn: fakeTimers.setTimeoutFn,
      clearTimeoutFn: fakeTimers.clearTimeoutFn,
    });
    client.start();
    for (const delay of expected) {
      const socket = sockets.at(-1);
      if (!socket) {
        check(`generation ${sockets.length} opened a socket`, false);
        break;
      }
      socket.close(1006, '');
      const scheduled = [...fakeTimers.scheduled.values()][0]?.delay;
      delays.push(scheduled ?? -1);
      check(`failed generation ${sockets.length} schedules ${delay}ms`, scheduled === delay, String(scheduled));
      check(`backoff ${delay}ms opens the next socket`, fakeTimers.runDelay(delay));
    }
    check('backoff sequence is 250,500,1000,... capped at 15000', delays.join(',') === expected.join(','), delays.join(','));

    const recovered = sockets.at(-1);
    recovered?.open();
    recovered?.respondDiscovered([sample('https://192.0.2.14:8765', '192.0.2.14', 8765)]);
    recovered?.close(1006, '');
    const resetDelay = [...fakeTimers.scheduled.values()][0]?.delay;
    check('completed snapshot resets the next delay to 250', resetDelay === RUNNER_RECONNECT_INITIAL_MS, String(resetDelay));
    check('reset delay is not still at the cap', resetDelay !== RUNNER_RECONNECT_MAX_MS, String(resetDelay));
    client.stop();
  }

  {
    activeBlock = 'stop cancels reconnect retries';
    const { location } = installWindow('');
    const store = storage();
    const fakeTimers = timers();
    const sockets: FakeWebSocket[] = [];
    const client = new DiscoveryClient({
      storage: store,
      location,
      webSocketFactory: (url, protocols) => {
        const socket = new FakeWebSocket(url, protocols);
        sockets.push(socket);
        return socket as unknown as WebSocket;
      },
      setTimeoutFn: fakeTimers.setTimeoutFn,
      clearTimeoutFn: fakeTimers.clearTimeoutFn,
    });
    client.start();
    sockets[0].open();
    sockets[0].close(1006, '');
    check('close queued a reconnect timer', fakeTimers.scheduled.size === 1, String(fakeTimers.scheduled.size));
    client.stop();
    check('stop cancels the pending reconnect timer', fakeTimers.scheduled.size === 0, String(fakeTimers.scheduled.size));
    check('stop does not leave the closed socket able to reconnect', sockets[0].onclose === null);
    check('firing the cancelled delay opens no socket', fakeTimers.runDelay(RUNNER_RECONNECT_INITIAL_MS) === false);
    check('no replacement socket after stop', sockets.length === 1, String(sockets.length));
  }

  console.log(`\nchatDiscovery.test: ${assertions} assertions, ${failures.length} failure(s)`);
  if (failures.length) {
    console.error(failures.join('\n'));
    process.exit(1);
  }
}

void main().catch((error: unknown) => {
  console.error(error instanceof Error ? error.stack ?? error.message : String(error));
  process.exit(1);
});
