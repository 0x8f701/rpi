// Runner registry persisted to AsyncStorage. Mirrors crates/pi-cli/web/src/chat/runners.ts
// storage contract: runner records independent of any project.
import AsyncStorage from '@react-native-async-storage/async-storage';
import { runnerDisplayName, splitRunnerAddress, type RunnerEndpoint } from './protocol';
import { RunnerClient, type RunnerConnectionState, type RunnerProbe } from './runnerClient';
import type { TransportFailureKind } from './transportError';

export const RUNNER_STORAGE_KEY = 'rpi-mobile-chat-runners:v1';
export const TOKEN_STORAGE_KEY = 'rpi-mobile-chat-tokens:v1';

export interface RunnerRecord {
  id: string;
  label: string;
  origin: string;
  authority: string;
  token: string;
  codename: string | null;
  createdAt: number;
}

export interface RunnerView extends RunnerRecord {
  state: RunnerConnectionState;
  error: string;
  errorKind: TransportFailureKind | null;
  probe: RunnerProbe | null;
}

export function runnerRecordWithoutToken(record: RunnerRecord): Omit<RunnerRecord, 'token'> {
  return {
    id: record.id,
    label: record.label,
    origin: record.origin,
    authority: record.authority,
    codename: record.codename,
    createdAt: record.createdAt,
  };
}

function isPersistedRunner(row: unknown): row is Omit<RunnerRecord, 'token'> {
  if (!row || typeof row !== 'object') return false;
  const value = row as Record<string, unknown>;
  return typeof value.id === 'string'
    && typeof value.origin === 'string'
    && typeof value.authority === 'string'
    && typeof value.createdAt === 'number'
    && !value.origin.includes('#');
}

export function runnerViewName(view: RunnerView): string {
  return runnerDisplayName({ codename: view.codename, label: view.label, authority: view.authority });
}

/** One runner row of a synced mapping document (its wire reader is
 *  `chatSync.parseImportedRunner`): the row `importRecords` inserts and the
 *  origin context `applyTokens` reads. */
export interface ImportedRunnerRow {
  id: string;
  label: string;
  origin: string;
  authority: string;
  createdAt: number;
}

/** Whether two origins name the same endpoint: scheme plus authority. A synced
 *  origin was written by another profile, so it is normalized rather than
 *  compared verbatim, and an unparseable origin never matches anything. */
function sameEndpoint(left: string, right: string): boolean {
  const a = splitRunnerAddress(left, null);
  const b = splitRunnerAddress(right, null);
  return a !== null && b !== null && a.scheme === b.scheme && a.authority === b.authority;
}

function parseTokenMap(raw: string | null): Record<string, string> {
  if (!raw) return {};
  try {
    const parsed = JSON.parse(raw);
    if (!parsed || typeof parsed !== 'object' || !parsed.tokens || typeof parsed.tokens !== 'object') return {};
    const tokens: Record<string, string> = {};
    for (const [authority, token] of Object.entries(parsed.tokens as Record<string, unknown>)) {
      if (typeof token === 'string' && token) tokens[authority] = token;
    }
    return tokens;
  } catch {
    return {};
  }
}

export class RunnerRegistry {
  private readonly clients = new Map<string, RunnerClient>();
  private readonly listeners = new Set<(views: RunnerView[]) => void>();
  private records: RunnerRecord[] = [];

  async load(): Promise<void> {
    let raw: string | null = null;
    let tokenRaw: string | null = null;
    try {
      raw = await AsyncStorage.getItem(RUNNER_STORAGE_KEY);
      tokenRaw = await AsyncStorage.getItem(TOKEN_STORAGE_KEY);
    } catch {
      raw = null;
      tokenRaw = null;
    }
    const tokens = parseTokenMap(tokenRaw);
    if (!raw) return;
    try {
      const parsed = JSON.parse(raw);
      if (Array.isArray(parsed?.runners)) {
        const stored: Array<Omit<RunnerRecord, 'token'>> = parsed.runners.filter(isPersistedRunner);
        this.records = stored.map((row) => ({
          ...row,
          label: typeof row.label === 'string' ? row.label : '',
          codename: typeof row.codename === 'string' ? row.codename : null,
          token: tokens[row.authority] ?? '',
        }));
      }
    } catch {
      this.records = [];
    }
  }

  private onPersist: (() => void) | null = null;

  setPersistHandler(handler: () => void): void {
    this.onPersist = handler;
  }

  private async persist(): Promise<void> {
    const tokens: Record<string, string> = {};
    for (const record of this.records) {
      if (record.token) tokens[record.authority] = record.token;
    }
    try {
      await AsyncStorage.setItem(
        RUNNER_STORAGE_KEY,
        JSON.stringify({ version: 1, runners: this.records.map(runnerRecordWithoutToken) }),
      );
      await AsyncStorage.setItem(TOKEN_STORAGE_KEY, JSON.stringify({ version: 1, tokens }));
    } catch {
      // quota — records live in memory this session
    }
    this.onPersist?.();
  }

  private emit(): void {
    const views = this.snapshot();
    for (const listener of this.listeners) listener(views);
  }

  subscribe(listener: (views: RunnerView[]) => void): () => void {
    this.listeners.add(listener);
    listener(this.snapshot());
    return () => {
      this.listeners.delete(listener);
    };
  }

  snapshot(): RunnerView[] {
    return [...this.records]
      .sort((left, right) => left.createdAt - right.createdAt)
      .map((record) => {
        const client = this.clients.get(record.id);
        return {
          ...record,
          state: client?.state ?? 'offline',
          error: client?.error ?? '',
          errorKind: client?.errorKind ?? null,
          probe: client?.probe ?? null,
        };
      });
  }

  /** Retire the live client of `id` and dial the record's current endpoint and
   *  token again: an origin or credential the user just supplied must never
   *  ride the socket an earlier pairing opened. */
  private reconnect(id: string): void {
    this.clients.get(id)?.close();
    this.clients.delete(id);
    this.connect(id);
  }

  async add(endpoint: RunnerEndpoint, token: string, label: string): Promise<RunnerRecord> {
    const duplicate = this.records.find((row) => row.authority === endpoint.authority);
    if (duplicate) {
      // Dedup stays keyed on the network authority, so one runner is one row —
      // but the endpoint the user paired is authoritative for that row. The
      // same authority under another scheme is another origin, and a
      // credential minted for one scheme never crosses to the other: only a
      // token THIS pairing supplied crosses, so an HTTPS token is never
      // replayed on a plaintext endpoint (nor the reverse).
      const schemeChanged = !sameEndpoint(duplicate.origin, endpoint.origin);
      duplicate.origin = endpoint.origin;
      duplicate.token = token || (schemeChanged ? '' : duplicate.token);
      duplicate.label = label.trim() || duplicate.label;
      await this.persist();
      this.reconnect(duplicate.id);
      return duplicate;
    }
    const record: RunnerRecord = {
      id: `${Date.now().toString(36)}${Math.random().toString(36).slice(2, 8)}`,
      label: label.trim(),
      origin: endpoint.origin,
      authority: endpoint.authority,
      token,
      codename: null,
      createdAt: Date.now(),
    };
    this.records = [...this.records, record];
    await this.persist();
    this.connect(record.id);
    return record;
  }

  async remove(id: string): Promise<void> {
    this.clients.get(id)?.close();
    this.clients.delete(id);
    this.records = this.records.filter((row) => row.id !== id);
    await this.persist();
    this.emit();
  }

  connect(id: string): void {
    const record = this.records.find((row) => row.id === id);
    if (!record) return;
    if (this.clients.has(id)) return;
    const client = new RunnerClient(
      { origin: record.origin, authority: record.authority, webSocketUrl: record.origin.replace(/^http/, 'ws') + '/ws' },
      record.token,
      () => {
        this.emit();
      },
    );
    this.clients.set(id, client);
    this.emit();
    client.connect();
  }

  connectAll(): void {
    for (const record of this.records) this.connect(record.id);
  }

  client(id: string): RunnerClient | null {
    return this.clients.get(id) ?? null;
  }

  async updateToken(id: string, token: string): Promise<void> {
    const record = this.records.find((row) => row.id === id);
    if (!record) throw new Error('Runner was not found.');
    record.token = token;
    await this.persist();
    this.reconnect(id);
  }

  exportRecords(): Array<Omit<RunnerRecord, 'token'>> {
    return this.records.map(runnerRecordWithoutToken);
  }

  tokenMap(): Record<string, string> {
    const tokens: Record<string, string> = {};
    for (const record of this.records) {
      if (record.token) tokens[record.authority] = record.token;
    }
    return tokens;
  }

  /** Adopt synced credentials for rows this profile has never authenticated.
   *  The synced token map is keyed by authority alone, so a token is applied
   *  only WITH the origin the same document pairs that authority with: a
   *  credential minted for one scheme must never reach a row on the other.
   *  (Fresh rows are not that risk: `importRecords` inserts a document row with
   *  that row's own origin, so its token already rides its own endpoint.) */
  async applyTokens(
    tokens: Record<string, string>,
    rows: Array<Pick<ImportedRunnerRow, 'origin' | 'authority'>>,
  ): Promise<void> {
    let changed = false;
    for (const record of this.records) {
      if (record.token) continue;
      const incoming = tokens[record.authority];
      if (!incoming) continue;
      const synced = rows.find((row) => row.authority === record.authority);
      if (!synced || !sameEndpoint(record.origin, synced.origin)) continue;
      record.token = incoming;
      changed = true;
    }
    if (changed) await this.persist();
  }

  async importRecords(rows: ImportedRunnerRow[], tokens: Record<string, string>): Promise<{ imported: number; idByRemoteId: Map<string, string> }> {
    const idByRemoteId = new Map<string, string>();
    const inserted: string[] = [];
    for (const row of rows) {
      const byId = this.records.find((existing) => existing.id === row.id);
      if (byId) {
        idByRemoteId.set(row.id, byId.id);
        continue;
      }
      const byAuthority = this.records.find((existing) => existing.authority === row.authority);
      if (byAuthority) {
        idByRemoteId.set(row.id, byAuthority.id);
        continue;
      }
      this.records = [...this.records, {
        id: row.id,
        label: row.label,
        origin: row.origin,
        authority: row.authority,
        token: tokens[row.authority] ?? '',
        codename: null,
        createdAt: row.createdAt,
      }];
      idByRemoteId.set(row.id, row.id);
      inserted.push(row.id);
    }
    if (inserted.length > 0) await this.persist();
    for (const id of inserted) this.connect(id);
    return { imported: inserted.length, idByRemoteId };
  }

  watermark(): number {
    if (this.records.length === 0) return 0;
    return Math.max(...this.records.map((row) => row.createdAt));
  }
}
