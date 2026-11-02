import AsyncStorage from '@react-native-async-storage/async-storage';
import {
  fetchMappingSync,
  isSyncNetworkFailure,
  parseImportedRunner,
  pushMappingSync,
  type MappingSyncPayload,
} from './chatSync';
import { parseStoredMappingDocument } from './store';
import type { MappingStore } from './store';
import type { RunnerRegistry } from './registry';

const SYNC_META_KEY = 'rpi-mobile-chat-sync-meta:v1';

interface OriginSyncMeta {
  revision: number;
  savedAtMs: number;
}

interface SyncMetaFile {
  byOrigin: Record<string, OriginSyncMeta>;
}

let mappingSyncInFlight = false;
let mappingSyncQueued = false;
let mappingSyncReporter: ((message: string) => void) | null = null;

function isStoredMappingDocument(mapping: unknown): boolean {
  return parseStoredMappingDocument(mapping) !== null;
}

async function readMeta(): Promise<SyncMetaFile> {
  try {
    const raw = await AsyncStorage.getItem(SYNC_META_KEY);
    if (!raw) return { byOrigin: {} };
    const parsed = JSON.parse(raw) as SyncMetaFile;
    if (!parsed || typeof parsed !== 'object' || !parsed.byOrigin || typeof parsed.byOrigin !== 'object') {
      return { byOrigin: {} };
    }
    return { byOrigin: parsed.byOrigin };
  } catch {
    return { byOrigin: {} };
  }
}

async function writeMeta(file: SyncMetaFile): Promise<void> {
  try {
    await AsyncStorage.setItem(SYNC_META_KEY, JSON.stringify(file));
  } catch {
    return;
  }
}

function localEmpty(registry: RunnerRegistry, mappingStore: MappingStore): boolean {
  return mappingStore.isEmpty() && registry.exportRecords().length === 0;
}

async function adoptRemote(
  registry: RunnerRegistry,
  mappingStore: MappingStore,
  document: MappingSyncPayload,
): Promise<void> {
  const parsedRunners = document.runners.map(parseImportedRunner).filter((row): row is NonNullable<typeof row> => row !== null);
  // The synced token map is keyed by authority alone, so it is applied WITH the
  // origins this document pairs those authorities with: a credential minted for
  // one scheme must never be adopted by a local row on the other.
  await registry.applyTokens(document.tokens, parsedRunners);
  const { idByRemoteId } = await registry.importRecords(parsedRunners, document.tokens);
  const parsed = parseStoredMappingDocument(document.mapping);
  if (!parsed) throw new Error('Synced chat mapping document is invalid.');
  mappingStore.adoptDocument({
    version: 1,
    projects: parsed.projects,
    mappings: parsed.mappings.flatMap((record) => {
      const runnerId = idByRemoteId.get(record.runnerId) ?? record.runnerId;
      const local = registry.snapshot().some((row) => row.id === runnerId);
      return local ? [{ ...record, runnerId }] : [];
    }),
  });
}

function currentPayload(registry: RunnerRegistry, mappingStore: MappingStore): MappingSyncPayload {
  return {
    mapping: mappingStore.exportDocument(),
    runners: registry.exportRecords(),
    tokens: registry.tokenMap(),
  };
}

async function pushOrigin(
  origin: string,
  token: string,
  registry: RunnerRegistry,
  mappingStore: MappingStore,
  meta: SyncMetaFile,
): Promise<void> {
  const fingerprint = JSON.stringify(currentPayload(registry, mappingStore));
  for (let iteration = 0; iteration < 3; iteration += 1) {
    const base = meta.byOrigin[origin] ?? { revision: 0, savedAtMs: 0 };
    const result = await pushMappingSync(origin, token, currentPayload(registry, mappingStore), base.revision, isStoredMappingDocument);
    if ('conflict' in result && result.conflict === true) {
      await adoptRemote(registry, mappingStore, result.document);
    }
    meta.byOrigin[origin] = { revision: result.revision, savedAtMs: result.savedAtMs };
    await writeMeta(meta);
    if (JSON.stringify(currentPayload(registry, mappingStore)) === fingerprint) return;
  }
}

export async function runMappingSync(
  registry: RunnerRegistry,
  mappingStore: MappingStore,
  mode: 'boot' | 'push',
): Promise<void> {
  if (mappingSyncInFlight) {
    mappingSyncQueued = true;
    return;
  }
  mappingSyncInFlight = true;
  try {
    const meta = await readMeta();
    const runners = registry.snapshot().filter((row) => row.token);
    for (const runner of runners) {
      try {
        if (mode === 'boot') {
          const remote = await fetchMappingSync(runner.origin, runner.token, isStoredMappingDocument);
          const originMeta = meta.byOrigin[runner.origin] ?? { revision: 0, savedAtMs: 0 };
          if (remote.document && (localEmpty(registry, mappingStore) || remote.savedAtMs > originMeta.savedAtMs)) {
            await adoptRemote(registry, mappingStore, remote.document);
            meta.byOrigin[runner.origin] = { revision: remote.revision, savedAtMs: remote.savedAtMs };
            await writeMeta(meta);
          }
        }
        if (!localEmpty(registry, mappingStore)) {
          await pushOrigin(runner.origin, runner.token, registry, mappingStore, meta);
        }
      } catch (error) {
        if (isSyncNetworkFailure(error)) continue;
        throw error;
      }
    }
  } finally {
    mappingSyncInFlight = false;
    if (mappingSyncQueued) {
      mappingSyncQueued = false;
      void runMappingSync(registry, mappingStore, 'push').catch((error) => {
        if (isSyncNetworkFailure(error)) return;
        mappingSyncReporter?.(error instanceof Error ? error.message : String(error));
      });
    }
  }
}

export function wireMappingSync(
  registry: RunnerRegistry,
  mappingStore: MappingStore,
  reportError: (message: string) => void,
): void {
  mappingSyncReporter = reportError;
  const push = () => {
    void runMappingSync(registry, mappingStore, 'push').catch((error) => {
      if (isSyncNetworkFailure(error)) return;
      reportError(error instanceof Error ? error.message : String(error));
    });
  };
  registry.setPersistHandler(push);
  mappingStore.setPersistHandler(push);
}
