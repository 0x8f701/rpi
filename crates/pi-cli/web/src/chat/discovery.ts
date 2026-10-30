import type { StorageLike } from '../hostToken';
import { loadInitialAuthorityToken, normalizeHostAuthority } from '../hostToken';
import { detachTransportHandlers, shouldScheduleReconnect } from '../socket';
import type { RpcResponse } from '../types';
import {
  fetchRunnerStatus,
  parseRunnerEndpoint,
  type PageHttpProtocol,
  type RunnerStatus,
} from './auth';
import {
  RUNNER_RECONNECT_INITIAL_MS,
  isRpcResponse,
  nextReconnectDelay,
  runnerProtocols,
} from './protocol';

export interface DiscoveredRunner {
  origin: string;
  host: string;
  port: number;
  label: string;
  version: string | null;
  scheme: string;
  codename: string | null;
}

export type InviteCandidateReach =
  | { reachable: true; status: RunnerStatus }
  | { reachable: false };

export type DiscoveryListener = (runners: DiscoveredRunner[]) => void;

type PendingDiscoveryEvent =
  | { type: 'runner_discovered'; runner: DiscoveredRunner }
  | { type: 'runner_gone'; origin: string };

export interface DiscoveryLocation {
  protocol: string;
  host: string;
}

export interface DiscoveryClientOptions {
  storage?: StorageLike | null;
  location?: DiscoveryLocation;
  webSocketFactory?: (url: string, protocols: string[]) => WebSocket;
  setTimeoutFn?: (handler: () => void, timeout: number) => number;
  clearTimeoutFn?: (timeout: number) => void;
  fetchFn?: typeof fetch;
}

function defaultStorage(): StorageLike | null {
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

function defaultLocation(): DiscoveryLocation {
  try {
    return { protocol: window.location.protocol, host: window.location.host };
  } catch {
    return { protocol: 'http:', host: '' };
  }
}

function defaultWebSocketFactory(url: string, protocols: string[]): WebSocket {
  return new WebSocket(url, protocols);
}

function defaultSetTimeout(handler: () => void, timeout: number): number {
  return window.setTimeout(handler, timeout);
}

function defaultClearTimeout(timeout: number): void {
  window.clearTimeout(timeout);
}

function asDiscoveredRunner(value: unknown): DiscoveredRunner | null {
  if (!value || typeof value !== 'object') return null;
  if (!('origin' in value) || typeof value.origin !== 'string' || !value.origin.trim()) return null;
  if (!('host' in value) || typeof value.host !== 'string' || !value.host.trim()) return null;
  if (!('port' in value) || typeof value.port !== 'number' || !Number.isFinite(value.port) || value.port <= 0) {
    return null;
  }
  const scheme = 'scheme' in value && typeof value.scheme === 'string' && value.scheme.trim()
    ? value.scheme
    : 'ws';
  const label = 'label' in value && typeof value.label === 'string' && value.label.trim()
    ? value.label
    : value.host;
  const version = 'version' in value && typeof value.version === 'string' && value.version.trim()
    ? value.version
    : null;
  const codename = 'codename' in value && typeof value.codename === 'string' && value.codename.trim()
    ? value.codename
    : null;
  return {
    origin: value.origin,
    host: value.host,
    port: value.port,
    label,
    version,
    scheme,
    codename,
  };
}

function asPendingDiscoveryEvent(value: unknown): PendingDiscoveryEvent | null {
  if (!value || typeof value !== 'object' || !('type' in value)) return null;
  if (value.type === 'runner_discovered') {
    const runner = asDiscoveredRunner(value);
    return runner ? { type: 'runner_discovered', runner } : null;
  }
  if (value.type === 'runner_gone' && 'origin' in value && typeof value.origin === 'string') {
    return { type: 'runner_gone', origin: value.origin };
  }
  return null;
}

function upsert(list: DiscoveredRunner[], runner: DiscoveredRunner): DiscoveredRunner[] {
  const next = list.filter((candidate) => candidate.origin !== runner.origin);
  next.push(runner);
  next.sort((left, right) => left.origin.localeCompare(right.origin));
  return next;
}

export function originAuthority(raw: string): string {
  try {
    const url = new URL(raw.includes('://') ? raw : `http://${raw}`);
    return url.host;
  } catch {
    return raw.replace(/^(wss?|https?):\/\//, '');
  }
}

export function discoveredAlreadyAdded(discovered: DiscoveredRunner, addedOrigins: string[]): boolean {
  const key = originAuthority(discovered.origin);
  return addedOrigins.some((origin) => originAuthority(origin) === key);
}

export function discoveredHttpsOnHttpPage(origin: string, pageProtocol?: PageHttpProtocol): boolean {
  if (pageProtocol !== 'http:') return false;
  try {
    return new URL(parseRunnerEndpoint(origin, pageProtocol).origin).protocol === 'https:';
  } catch {
    return false;
  }
}

export async function fetchInviteCandidateStatus(
  origin: string,
  fetchFn: typeof fetch = fetch,
  pageProtocol?: PageHttpProtocol,
  signal?: AbortSignal,
): Promise<InviteCandidateReach> {
  if (discoveredHttpsOnHttpPage(origin, pageProtocol)) return { reachable: false };
  const endpoint = parseRunnerEndpoint(origin, pageProtocol);
  const result = await fetchRunnerStatus(endpoint, fetchFn, signal);
  return result.ok
    ? { reachable: true, status: result.status }
    : { reachable: false };
}

export class DiscoveryClient {
  private socket: WebSocket | null = null;
  private runners: DiscoveredRunner[] = [];
  private readonly listeners = new Set<DiscoveryListener>();
  private sequence = 0;
  private generation = 0;
  private disposed = false;
  private snapshotReady = false;
  private reconnectDelay = RUNNER_RECONNECT_INITIAL_MS;
  private reconnectTimer: number | null = null;
  private readonly pending = new Map<string, (response: RpcResponse) => void>();
  private pendingEvents: PendingDiscoveryEvent[] = [];
  private readonly storage: StorageLike | null;
  private readonly location: DiscoveryLocation;
  private readonly webSocketFactory: (url: string, protocols: string[]) => WebSocket;
  private readonly setTimeoutFn: (handler: () => void, timeout: number) => number;
  private readonly clearTimeoutFn: (timeout: number) => void;

  constructor(options: DiscoveryClientOptions = {}) {
    this.storage = options.storage === undefined ? defaultStorage() : options.storage;
    this.location = options.location ?? defaultLocation();
    this.webSocketFactory = options.webSocketFactory ?? defaultWebSocketFactory;
    this.setTimeoutFn = options.setTimeoutFn ?? defaultSetTimeout;
    this.clearTimeoutFn = options.clearTimeoutFn ?? defaultClearTimeout;
  }

  subscribe(listener: DiscoveryListener): () => void {
    this.listeners.add(listener);
    listener(this.runners);
    return () => {
      this.listeners.delete(listener);
    };
  }

  start(): void {
    if (this.disposed || this.socket || this.reconnectTimer !== null) return;
    this.connect();
  }

  stop(): void {
    this.disposed = true;
    this.cancelReconnect();
    this.generation += 1;
    this.replaceSocket();
  }

  private emit(): void {
    const snapshot = this.runners.map((runner) => ({ ...runner }));
    for (const listener of this.listeners) listener(snapshot);
  }

  private connect(): void {
    if (this.disposed) return;
    this.cancelReconnect();
    this.replaceSocket();
    const generation = ++this.generation;
    this.snapshotReady = false;
    this.pendingEvents = [];

    const scheme = this.location.protocol === 'https:' ? 'wss:' : 'ws:';
    const host = this.location.host;
    const token = loadInitialAuthorityToken(this.storage, normalizeHostAuthority(host));
    let socket: WebSocket;
    try {
      socket = this.webSocketFactory(`${scheme}//${host}/ws`, runnerProtocols(token));
    } catch {
      this.scheduleReconnect(generation);
      return;
    }
    this.socket = socket;
    socket.onopen = () => {
      if (!this.isCurrent(generation, socket)) return;
      this.requestSnapshot(socket);
    };
    socket.onmessage = (event: MessageEvent) => {
      if (!this.isCurrent(generation, socket) || typeof event.data !== 'string') return;
      let frame: unknown;
      try {
        frame = JSON.parse(event.data);
      } catch {
        return;
      }
      if (!frame || typeof frame !== 'object') return;
      if (isRpcResponse(frame) && typeof frame.id === 'string') {
        const pending = this.pending.get(frame.id);
        if (pending) {
          this.pending.delete(frame.id);
          pending(frame);
        }
        return;
      }
      const discoveryEvent = asPendingDiscoveryEvent(frame);
      if (!discoveryEvent) return;
      if (!this.snapshotReady) {
        this.pendingEvents.push(discoveryEvent);
        return;
      }
      this.applyDiscoveryEvent(discoveryEvent);
      this.emit();
    };
    socket.onclose = () => {
      if (this.generation !== generation || this.socket !== socket) return;
      detachTransportHandlers(socket);
      this.socket = null;
      this.pending.clear();
      this.pendingEvents = [];
      this.snapshotReady = false;
      if (!shouldScheduleReconnect(null, !this.disposed)) return;
      this.scheduleReconnect(generation);
    };
    socket.onerror = () => undefined;
  }

  private requestSnapshot(socket: WebSocket): void {
    const id = `disc-${++this.sequence}`;
    const generation = this.generation;
    this.pending.set(id, (response) => {
      if (this.generation !== generation || this.socket !== socket) return;
      if (!response.success || !response.data || typeof response.data !== 'object') return;
      if (!('runners' in response.data) || !Array.isArray(response.data.runners)) return;
      this.runners = response.data.runners
        .map(asDiscoveredRunner)
        .filter((row): row is DiscoveredRunner => row !== null);
      const buffered = this.pendingEvents;
      this.pendingEvents = [];
      for (const discoveryEvent of buffered) this.applyDiscoveryEvent(discoveryEvent);
      this.snapshotReady = true;
      this.reconnectDelay = RUNNER_RECONNECT_INITIAL_MS;
      this.emit();
    });
    socket.send(JSON.stringify({ type: 'get_discovered_runners', id }));
  }

  private applyDiscoveryEvent(event: PendingDiscoveryEvent): void {
    if (event.type === 'runner_discovered') {
      this.runners = upsert(this.runners, event.runner);
      return;
    }
    const gone = originAuthority(event.origin);
    this.runners = this.runners.filter((runner) => originAuthority(runner.origin) !== gone);
  }

  private isCurrent(generation: number, socket: WebSocket): boolean {
    return !this.disposed && this.generation === generation && this.socket === socket;
  }

  private scheduleReconnect(generation: number): void {
    if (this.disposed || this.reconnectTimer !== null) return;
    if (!shouldScheduleReconnect(null, !this.disposed)) return;
    const delay = this.reconnectDelay;
    this.reconnectDelay = nextReconnectDelay(delay);
    this.reconnectTimer = this.setTimeoutFn(() => {
      this.reconnectTimer = null;
      if (this.disposed || this.generation !== generation) return;
      this.connect();
    }, delay);
  }

  private cancelReconnect(): void {
    if (this.reconnectTimer === null) return;
    this.clearTimeoutFn(this.reconnectTimer);
    this.reconnectTimer = null;
  }

  private replaceSocket(): void {
    this.pending.clear();
    this.pendingEvents = [];
    const socket = this.socket;
    this.socket = null;
    this.snapshotReady = false;
    if (!socket) return;
    detachTransportHandlers(socket);
    try {
      socket.close();
    } catch {
      /* already closed */
    }
  }
}
