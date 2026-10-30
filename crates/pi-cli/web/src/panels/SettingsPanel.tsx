// Settings panel — schema-driven settings browser with typed edit and
// the server-held draft/apply flow (mirrors the TUI settings panel):
//
//   settings_inspect    -> SettingsCatalogSnapshot (categories + definitions +
//                          effective values + provenance)
//   settings_open_draft {scope:"global"} -> { draftId }
//   settings_set        {draftId, key, value}  — typed validation server-side
//   settings_reset      {draftId, key}
//   settings_validate   {draftId}
//   settings_apply      {draftId} -> SettingApplyOutcome (persists)
//   settings_cancel     {draftId}
//
// Secret keys (definition.secret / valueType.kind === "secret") render
// redacted and are deliberately NOT writable — the server refuses them, the
// panel never offers an edit control. EVERY model/display string passes
// through safeText().

import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { safeText } from '../redact';
import { applyPlantumlServerFromCatalog } from '../plantumlEncode';
import type { McpServerSettingsWire, SandboxSettingsWire } from '../types';


export interface SettingValueTypeWire {
  kind: string;
  nonEmpty?: boolean;
  min?: number;
  max?: number;
}

export interface SettingDefWire {
  key: string;
  category: string;
  valueType: SettingValueTypeWire;
  defaultJson: string;
  description: string;
  enumValues: string[];
  scopes: string;
  behavior: string;
  secret: boolean;
  trustSensitive: boolean;
}

export interface SettingValueViewWire {
  definition: SettingDefWire;
  effectiveValue: unknown | SandboxSettingsWire | McpServerSettingsWire[];
  source: string;
  globalValue?: unknown | SandboxSettingsWire | McpServerSettingsWire[];
  projectValue?: unknown | SandboxSettingsWire | McpServerSettingsWire[];
  sessionOverrideValue?: unknown | SandboxSettingsWire | McpServerSettingsWire[];
  editableGlobal: boolean;
  editableProject: boolean;
  redacted: boolean;
}

export interface SettingsCatalogWire {
  projectTrusted: boolean;
  globalPath: string;
  projectPath: string;
  values: SettingValueViewWire[];
}

interface DraftSnapshotWire {
  draftId: string;
  scope: string;
  dirty: boolean;
  values: SettingValueViewWire[];
}

interface ApplyOutcomeWire {
  appliedLive?: boolean;
  reloaded?: boolean;
  restartRequired?: boolean;
  results?: Array<{ key: string; needsReload?: boolean; needsRestart?: boolean }>;
}

interface SettingsPanelProps {
  sendCommand: (command: Record<string, unknown>) => Promise<unknown>;
  /** Re-fetch get_state into the app shell after apply (runtime settings). */
  refreshState: () => Promise<unknown>;
  /** True only once the App websocket is authoritative-ready (`connState ===
   *  'on'`). While false — notably the `token_required` boot hold that
   *  auto-opens this panel — the catalog auto-load, Refresh, and Edit stay
   *  silent: `settings_inspect` / `settings_open_draft` would reject with
   *  `not connected` and surface a bogus error banner. The Connection token
   *  UI below stays fully usable; the first flip to true loads the catalog
   *  exactly once. */
  catalogEnabled: boolean;
  /** Current connection token (rpi-auth.<token> subprotocol). */
  token: string;
  /** Persist a new token for the current listener authority and reconnect. */
  onTokenChange: (token: string) => void;
  onClose: () => void;
}

const CATEGORY_LABELS: Record<string, string> = {
  models: 'Models',
  session: 'Session',
  compaction: 'Compaction',
  retryTransport: 'Retry & Transport',
  terminalUi: 'Terminal & UI',
  orchestration: 'Orchestration',
  resources: 'Resources',
  trustSecurity: 'Trust & Security',
  live: 'Live',
};

const CATEGORY_ORDER = [
  'models',
  'session',
  'compaction',
  'retryTransport',
  'terminalUi',
  'orchestration',
  'resources',
  'trustSecurity',
  'live',
];

function categoryLabel(category: string): string {
  return CATEGORY_LABELS[category] || category;
}

/** Render a JSON value as a short human string for the row summary. */
function valueSummary(value: unknown): string {
  if (value === null || value === undefined) return '—';
  if (typeof value === 'string') return value === '' ? '(empty)' : value;
  if (typeof value === 'boolean' || typeof value === 'number') return String(value);
  try {
    const text = JSON.stringify(value);
    return text.length > 80 ? `${text.slice(0, 80)}…` : text;
  } catch {
    return String(value);
  }
}

function isSecretView(view: SettingValueViewWire): boolean {
  return view.definition.secret || view.definition.valueType.kind === 'secret';
}

function parseTypedValue(kind: string, raw: string): { ok: boolean; value?: unknown; message?: string } {
  switch (kind) {
    case 'boolean':
      return raw === 'true' ? { ok: true, value: true } : raw === 'false' ? { ok: true, value: false } : { ok: false, message: 'expected true or false' };
    case 'enum':
    case 'string':
      return { ok: true, value: raw };
    case 'integer':
    case 'unsignedInteger':
    case 'number': {
      if (raw.trim() === '') return { ok: false, message: 'expected a number' };
      const number = Number(raw);
      return Number.isFinite(number) ? { ok: true, value: number } : { ok: false, message: 'expected a number' };
    }
    case 'stringList':
    case 'array':
    case 'object': {
      try {
        const parsed = JSON.parse(raw);
        if (kind === 'stringList' && (!Array.isArray(parsed) || parsed.some((item) => typeof item !== 'string'))) {
          return { ok: false, message: 'expected a JSON array of strings' };
        }
        if (kind === 'object' && parsed !== null && (typeof parsed !== 'object' || Array.isArray(parsed))) {
          return { ok: false, message: 'expected a JSON object or null' };
        }
        return { ok: true, value: parsed };
      } catch {
        return { ok: false, message: 'invalid JSON' };
      }
    }
    default:
      return { ok: true, value: raw };
  }
}

export interface SettingLocalDraft {
  raw: string;
  baseline: string;
  dirty: boolean;
  revision: number;
}

export type SettingLocalDrafts = Map<string, SettingLocalDraft>;

export function settingValueToRaw(view: SettingValueViewWire, value: unknown): string {
  const kind = view.definition.valueType.kind;
  if (kind === 'string' || kind === 'enum') return typeof value === 'string' ? value : '';
  if (kind === 'boolean') return value === true ? 'true' : 'false';
  if (kind === 'integer' || kind === 'unsignedInteger' || kind === 'number') {
    return typeof value === 'number' ? String(value) : '';
  }
  return value === undefined || value === null ? '' : JSON.stringify(value, null, 2);
}

export function reconcileSettingLocalDrafts(
  current: SettingLocalDrafts,
  values: Array<{ view: SettingValueViewWire; value: unknown }>,
  replace = false,
): SettingLocalDrafts {
  const next = replace ? new Map<string, SettingLocalDraft>() : new Map(current);
  for (const { view, value } of values) {
    const key = view.definition.key;
    const raw = settingValueToRaw(view, value);
    const existing = next.get(key);
    if (!existing || !existing.dirty) {
      next.set(key, {
        raw,
        baseline: raw,
        dirty: false,
        revision: existing?.revision ?? 0,
      });
    }
  }
  return next;
}

export function editSettingLocalDraft(
  current: SettingLocalDrafts,
  key: string,
  raw: string,
  baseline: string,
): SettingLocalDrafts {
  const existing = current.get(key) ?? { raw: baseline, baseline, dirty: false, revision: 0 };
  const next = new Map(current);
  next.set(key, {
    raw,
    baseline: existing.baseline,
    dirty: raw !== existing.baseline,
    revision: existing.revision + 1,
  });
  return next;
}


export function stagedSettingKeys(
  catalog: SettingsCatalogWire | null,
  draftValues: Map<string, SettingValueViewWire>,
): Set<string> {
  const keys = new Set<string>();
  for (const view of catalog?.values ?? []) {
    const staged = draftValues.get(view.definition.key);
    if (staged && settingValueToRaw(view, staged.effectiveValue) !== settingValueToRaw(view, view.effectiveValue)) {
      keys.add(view.definition.key);
    }
  }
  return keys;
}

export function mergeSettingSnapshotDrafts(
  current: SettingLocalDrafts,
  values: Array<{ view: SettingValueViewWire; value: unknown }>,
  acknowledgedKey?: string,
): SettingLocalDrafts {
  const next = reconcileSettingLocalDrafts(current, values);
  if (acknowledgedKey !== undefined) {
    const acknowledged = values.find(({ view }) => view.definition.key === acknowledgedKey);
    const local = next.get(acknowledgedKey);
    if (acknowledged && local) {
      const raw = settingValueToRaw(acknowledged.view, acknowledged.value);
      if (local.raw === raw) next.set(acknowledgedKey, { ...local, baseline: raw, dirty: false });
    }
  }
  return next;
}

export function dirtySettingKeys(drafts: SettingLocalDrafts): string[] {
  return Array.from(drafts, ([key, draft]) => (draft.dirty ? key : null)).filter((key): key is string => key !== null);
}

export function shouldSubmitSettingEnter(
  key: string,
  isComposing: boolean,
  multiline = false,
  modified = false,
): boolean {
  return key === 'Enter' && !isComposing && (!multiline || modified);
}

export interface SettingRequestResult {
  generation: number;
  accepted: boolean;
  ok: boolean;
  data?: unknown;
  error?: unknown;
}

interface PendingSettingRequest {
  generation: number;
  revision: number;
  requestValue: string;
  promise: Promise<SettingRequestResult>;
}

/** Per-key request gate: changed drafts and newer submissions supersede old replies. */
export class SettingRequestGate {
  private nextGeneration = 0;
  private latestGeneration = 0;
  private readonly epochByKey = new Map<string, number>();
  private readonly pendingByKey = new Map<string, PendingSettingRequest>();

  submit(
    key: string,
    revision: number,
    requestValue: string,
    getCurrentRevision: () => number | undefined,
    run: () => Promise<unknown>,
  ): Promise<SettingRequestResult> {
    const pending = this.pendingByKey.get(key);
    if (pending && pending.revision === revision && pending.requestValue === requestValue) {
      return pending.promise;
    }

    const generation = ++this.nextGeneration;
    const epoch = this.epochByKey.get(key) ?? 0;
    this.latestGeneration = generation;
    this.pendingByKey.clear();

    let operation: Promise<unknown>;
    try {
      operation = run();
    } catch (error) {
      operation = Promise.reject(error);
    }

    const promise = operation
      .then(
        (data): SettingRequestResult => ({
          generation,
          accepted: this.isCurrent(key, generation, revision, epoch, getCurrentRevision),
          ok: true,
          data,
        }),
        (error): SettingRequestResult => ({
          generation,
          accepted: this.isCurrent(key, generation, revision, epoch, getCurrentRevision),
          ok: false,
          error,
        }),
      )
      .finally(() => {
        if (this.pendingByKey.get(key)?.generation === generation) this.pendingByKey.delete(key);
      });

    this.pendingByKey.set(key, { generation, revision, requestValue, promise });
    return promise;
  }

  invalidate(key: string): void {
    this.epochByKey.set(key, (this.epochByKey.get(key) ?? 0) + 1);
    this.pendingByKey.delete(key);
  }

  invalidateAll(): void {
    for (const key of this.pendingByKey.keys()) this.epochByKey.set(key, (this.epochByKey.get(key) ?? 0) + 1);
    this.pendingByKey.clear();
    this.latestGeneration = ++this.nextGeneration;
  }

  private isCurrent(
    key: string,
    generation: number,
    revision: number,
    epoch: number,
    getCurrentRevision: () => number | undefined,
  ): boolean {
    return (
      epoch === (this.epochByKey.get(key) ?? 0)
      && this.latestGeneration === generation
      && getCurrentRevision() === revision
    );
  }
}

export function SettingsPanel({ sendCommand, refreshState, catalogEnabled, token, onTokenChange, onClose }: SettingsPanelProps) {
  const [catalog, setCatalog] = useState<SettingsCatalogWire | null>(null);
  const [draftId, setDraftId] = useState<string | null>(null);
  const [draftValues, setDraftValues] = useState<Map<string, SettingValueViewWire>>(new Map());
  const [localDrafts, setLocalDrafts] = useState<SettingLocalDrafts>(new Map());
  const localDraftsRef = useRef(localDrafts);
  const requestGateRef = useRef(new SettingRequestGate());
  const [selectedCategory, setSelectedCategory] = useState('models');
  const [status, setStatus] = useState('');
  const [busy, setBusy] = useState(false);
  const busyRef = useRef(false);
  // Connection token draft: edited locally, committed via onTokenChange
  // (Save/Enter) so typing never reconnects mid-keystroke.
  const [tokenDraft, setTokenDraft] = useState(token);
  const mountedRef = useRef(true);

  const updateLocalDrafts = useCallback((update: (current: SettingLocalDrafts) => SettingLocalDrafts) => {
    const next = update(localDraftsRef.current);
    localDraftsRef.current = next;
    setLocalDrafts(next);
  }, []);

  const loadCatalog = useCallback(async () => {
    const data = await sendCommand({ type: 'settings_inspect' });
    if (!mountedRef.current) return;
    // Own RPC contract: SettingsCatalogSnapshot (rpc.rs settings_inspect).
    const snapshot = data as SettingsCatalogWire;
    setCatalog(snapshot);
    applyPlantumlServerFromCatalog(snapshot);
    if (snapshot?.values && snapshot.values.length > 0) {
      const categories = new Set(snapshot.values.map((view) => view.definition.category));
      if (!categories.has(selectedCategory)) {
        setSelectedCategory(CATEGORY_ORDER.find((category) => categories.has(category)) || 'models');
      }
    }
  }, [selectedCategory]);

  // Catalog auto-load, gated on catalogEnabled: the token_required boot hold
  // auto-opens this panel with NO socket, and an unconditional
  // settings_inspect here rejected with `not connected` and surfaced a
  // persistent `catalog load failed: not connected` banner. While disabled
  // nothing is sent and the status stays quiet; the FIRST flip to enabled
  // (token saved, authoritative bootstrap done) loads the catalog exactly
  // once. Manual Refresh (button, disabled while disabled) and the
  // post-apply reload below are separate, deliberate calls.
  const autoLoadedRef = useRef(false);
  useEffect(() => {
    mountedRef.current = true;
    if (!catalogEnabled || autoLoadedRef.current) {
      return () => {
        mountedRef.current = false;
      };
    }
    autoLoadedRef.current = true;
    loadCatalog().catch((err: Error) => {
      if (mountedRef.current) setStatus(`catalog load failed: ${safeText(err.message)}`);
    });
    return () => {
      mountedRef.current = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [catalogEnabled]);

  // Keep the draft in sync when the saved token changes elsewhere (boot
  // restore from localStorage, a commit from this panel after a reconnect).
  useEffect(() => {
    setTokenDraft(token);
  }, [token]);

  const applyDraftSnapshot = useCallback((data: unknown, replaceLocalDrafts = false, acknowledgedKey?: string) => {
    const snapshot = data as DraftSnapshotWire;
    if (!snapshot?.draftId) return;
    setDraftId(snapshot.draftId);
    const snapshotValues = Array.isArray(snapshot.values) ? snapshot.values : [];
    const values = new Map<string, SettingValueViewWire>();
    for (const view of snapshotValues) values.set(view.definition.key, view);
    setDraftValues(values);
    updateLocalDrafts((current) => replaceLocalDrafts
      ? reconcileSettingLocalDrafts(current, snapshotValues.map((view) => ({ view, value: view.effectiveValue })), true)
      : mergeSettingSnapshotDrafts(current, snapshotValues.map((view) => ({ view, value: view.effectiveValue })), acknowledgedKey));
  }, [updateLocalDrafts]);

  const openDraft = useCallback(async () => {
    // Quiet no-op while the catalog gate is closed: no socket means
    // settings_open_draft could only fail with a transport banner.
    if (!catalogEnabled) return;
    if (busyRef.current) return;
    busyRef.current = true;
    setBusy(true);
    setStatus('');
    try {
      const data = await sendCommand({ type: 'settings_open_draft', scope: 'global' });
      applyDraftSnapshot(data, true);
      setStatus('draft open — edits are staged until Apply');
    } catch (err) {
      setStatus(`open draft failed: ${safeText((err as Error).message)}`);
    } finally {
      busyRef.current = false;
      if (mountedRef.current) setBusy(false);
    }
  }, [applyDraftSnapshot, catalogEnabled, sendCommand]);

  const editLocalDraft = useCallback((view: SettingValueViewWire, value: unknown, raw: string) => {
    const key = view.definition.key;
    requestGateRef.current.invalidate(key);
    updateLocalDrafts((current) => editSettingLocalDraft(
      current,
      key,
      raw,
      settingValueToRaw(view, value),
    ));
  }, [updateLocalDrafts]);

  const commitEdit = useCallback(
    async (key: string): Promise<boolean> => {
      if (!draftId) return false;
      const def = catalog?.values.find((view) => view.definition.key === key)?.definition;
      const local = localDraftsRef.current.get(key);
      if (!def || !local?.dirty) return true;
      const parsed = parseTypedValue(def.valueType.kind, local.raw);
      if (!parsed.ok) {
        setStatus(`${key}: ${parsed.message}`);
        return false;
      }

      setStatus('');
      const submittedDraftId = draftId;
      const revision = local.revision;
      const raw = local.raw;
      const result = await requestGateRef.current.submit(
        key,
        revision,
        raw,
        () => localDraftsRef.current.get(key)?.revision,
        () => sendCommand({ type: 'settings_set', draftId: submittedDraftId, key, value: parsed.value }),
      );
      if (!mountedRef.current || draftId !== submittedDraftId) return false;
      if (!result.accepted) return !localDraftsRef.current.get(key)?.dirty;
      if (!result.ok) {
        setStatus(`${key}: ${safeText((result.error as Error).message)}`);
        return false;
      }
      applyDraftSnapshot(result.data, false, key);
      return true;
    },
    [applyDraftSnapshot, catalog, draftId, sendCommand]
  );

  const submitLocalDrafts = useCallback(async (): Promise<boolean> => {
    for (const key of dirtySettingKeys(localDraftsRef.current)) {
      if (!await commitEdit(key)) return false;
    }
    return true;
  }, [commitEdit]);

  const commitOneEdit = useCallback((key: string) => {
    if (busyRef.current) return;
    busyRef.current = true;
    setBusy(true);
    void commitEdit(key).finally(() => {
      busyRef.current = false;
      if (mountedRef.current) setBusy(false);
    });
  }, [commitEdit]);

  const commitReset = useCallback(
    async (key: string) => {
      if (!draftId) return;
      if (busyRef.current) return;
      busyRef.current = true;
      requestGateRef.current.invalidateAll();
      setBusy(true);
      setStatus('');
      try {
        const data = await sendCommand({ type: 'settings_reset', draftId, key });
        applyDraftSnapshot(data);
        const snapshot = data as DraftSnapshotWire;
        const resetView = snapshot.values?.find((view) => view.definition.key === key);
        if (resetView) {
          updateLocalDrafts((current) => reconcileSettingLocalDrafts(
            new Map(Array.from(current).filter(([draftKey]) => draftKey !== key)),
            [{ view: resetView, value: resetView.effectiveValue }],
          ));
        }
        setStatus(`${key} reset to default`);
      } catch (err) {
        setStatus(`${key}: ${safeText((err as Error).message)}`);
      } finally {
        busyRef.current = false;
        if (mountedRef.current) setBusy(false);
      }
    },
    [applyDraftSnapshot, draftId, sendCommand, updateLocalDrafts]
  );

  const applyDraft = useCallback(async () => {
    if (!draftId || busyRef.current) return;
    busyRef.current = true;
    setBusy(true);
    setStatus('');
    try {
      if (!await submitLocalDrafts()) return;
      const data = await sendCommand({ type: 'settings_apply', draftId });
      const outcome = data as ApplyOutcomeWire;
      await refreshState();
      await loadCatalog();
      requestGateRef.current.invalidateAll();
      setDraftId(null);
      setDraftValues(new Map());
      updateLocalDrafts(() => new Map());
      if (outcome?.restartRequired) {
        setStatus('settings applied — restart required');
      } else {
        setStatus('settings applied');
      }
    } catch (err) {
      setStatus(`apply failed: ${safeText((err as Error).message)}`);
    } finally {
      busyRef.current = false;
      if (mountedRef.current) setBusy(false);
    }
  }, [draftId, loadCatalog, refreshState, sendCommand, submitLocalDrafts, updateLocalDrafts]);

  const cancelDraft = useCallback(async () => {
    if (!draftId) return;
    if (busyRef.current) return;
    busyRef.current = true;
    requestGateRef.current.invalidateAll();
    setBusy(true);
    setStatus('');
    try {
      await sendCommand({ type: 'settings_cancel', draftId });
      setDraftId(null);
      setDraftValues(new Map());
      updateLocalDrafts(() => new Map());
      await loadCatalog();
      setStatus('draft cancelled');
    } catch (err) {
      setStatus(`cancel failed: ${safeText((err as Error).message)}`);
    } finally {
      busyRef.current = false;
      if (mountedRef.current) setBusy(false);
    }
  }, [draftId, loadCatalog, sendCommand, updateLocalDrafts]);

  const grouped = useMemo(() => {
    const groups = new Map<string, SettingValueViewWire[]>();
    for (const view of catalog?.values || []) {
      const category = view.definition.category;
      const list = groups.get(category);
      if (list) list.push(view);
      else groups.set(category, [view]);
    }
    return CATEGORY_ORDER.filter((category) => groups.has(category)).map((category) => ({
      category,
      views: groups.get(category) || [],
    }));
  }, [catalog]);

  const activeCategory = grouped.find((group) => group.category === selectedCategory) || grouped[0];
  const stagedKeys = useMemo(() => stagedSettingKeys(catalog, draftValues), [catalog, draftValues]);

  const valueOf = useCallback(
    (view: SettingValueViewWire): unknown => {
      const staged = draftValues.get(view.definition.key);
      return staged ? staged.effectiveValue : view.effectiveValue;
    },
    [draftValues]
  );

  return (
    <section id="settings-panel" className="panel" aria-label="Settings panel">
      <div className="panel__head">
        <span className="panel__title">Settings</span>
        <span className="panel__hint">
          {draftId ? 'draft open — edits staged until Apply' : 'browse effective settings'}
        </span>
        {draftId ? (
          <>
            <button
              id="settings-apply-btn"
              type="button"
              onClick={applyDraft}
              disabled={busy}
              title="Stage local edits and persist the settings draft"
            >
              Apply
            </button>
            <button id="settings-cancel-btn" type="button" onClick={cancelDraft} disabled={busy} title="Discard the staged draft">
              Cancel
            </button>
          </>
        ) : (
          <button id="settings-edit-btn" type="button" onClick={openDraft} disabled={busy || !catalogEnabled} title="Open a global-scope edit draft">
            Edit settings
          </button>
        )}
        <button id="settings-refresh-btn" type="button" onClick={() => loadCatalog().catch(() => {})} disabled={busy || !catalogEnabled}>
          Refresh
        </button>
        <button id="settings-close-btn" type="button" className="panel-close" onClick={onClose} title="Close panel" aria-label="Close settings panel">
          ✕
        </button>
      </div>

      {status !== '' && (
        <div className={`panel__status${status.includes('failed') ? ' panel__status--error' : ''}`}>
          {safeText(status)}
        </div>
      )}

      <div className="panel__body settings-body">
        <section className="settings-connection" aria-label="Connection settings">
          <div className="settings-connection__title">Connection</div>
          <div className="settings-connection__row">
            <label className="settings-connection__label" htmlFor="settings-token-input">
              Token
            </label>
            <input
              id="settings-token-input"
              type="password"
              value={tokenDraft}
              autoComplete="off"
              spellCheck={false}
              placeholder="rpi-auth token (optional)"
              title="Sent as the rpi-auth.<token> WebSocket subprotocol. Saved only for the current listener authority; Save/Enter reconnects."
              onChange={(e) => setTokenDraft(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Enter') onTokenChange(tokenDraft);
              }}
            />
            <button
              id="settings-token-save-btn"
              type="button"
              onClick={() => onTokenChange(tokenDraft)}
              disabled={busy}
              title="Save the token and reconnect"
            >
              Save
            </button>
          </div>
          <div className="settings-connection__hint">
            Leave blank when the listener runs without <code>--listen-token-file</code>. When the
            listener was started with a token, enter it here and press Save — the page saves it only
            for the current listener authority and reconnects immediately.
          </div>
        </section>
        <nav className="settings-categories" aria-label="Settings categories">
          {grouped.map((group) => (
            <button
              key={group.category}
              type="button"
              className={`settings-category${group.category === selectedCategory ? ' settings-category--active' : ''}`}
              onClick={() => setSelectedCategory(group.category)}
            >
              <span className="settings-category__name">{categoryLabel(group.category)}</span>
              <span className="settings-category__count">{group.views.length}</span>
            </button>
          ))}
          {catalog && (
            <div className="settings-paths">
              <div title={catalog.globalPath ? safeText(catalog.globalPath) : ''}>global: {catalog.globalPath ? safeText(catalog.globalPath) : '—'}</div>
              <div title={catalog.projectPath ? safeText(catalog.projectPath) : ''}>project: {catalog.projectPath ? safeText(catalog.projectPath) : '—'}</div>
              <div>project trusted: {catalog.projectTrusted ? 'yes' : 'no'}</div>
            </div>
          )}
        </nav>

        <div className="settings-rows">
          {!activeCategory && <div className="panel__empty">No settings catalog loaded.</div>}
          {activeCategory?.views.map((view) => {
            const def = view.definition;
            const secret = isSecretView(view);
            const editable = draftId !== null && view.editableGlobal && !secret;
            const value = valueOf(view);
            const localDraft = localDrafts.get(def.key);
            const raw = localDraft?.raw ?? settingValueToRaw(view, value);
            const rawDefault = def.defaultJson ? valueSummary(JSON.parse(def.defaultJson)) : '—';
            return (
              <div key={def.key} className="setting-row" data-setting-key={def.key}>
                <div className="setting-row__head">
                  <span className="setting-row__key">{safeText(def.key)}</span>
                  {(stagedKeys.has(def.key) || localDraft?.dirty) && <span className="setting-row__dirty">dirty</span>}
                </div>
                <div className="setting-row__desc">{safeText(def.description)}</div>
                <div className="setting-row__control">
                  <SettingControl
                    view={view}
                    raw={raw}
                    editable={editable}
                    onChange={(nextRaw) => editLocalDraft(view, value, nextRaw)}
                    onCommit={() => commitOneEdit(def.key)}
                  />
                </div>
                <div className="setting-row__meta">
                  <span>default: {safeText(rawDefault)}</span>
                  <span>source: {safeText(view.source)}</span>
                  <span>apply: {safeText(def.behavior)}</span>
                  {!view.editableGlobal && <span className="setting-row__meta-warn">not writable in global scope</span>}
                </div>
                {draftId && editable && (
                  <button
                    type="button"
                    className="setting-row__reset"
                    onClick={() => commitReset(def.key)}
                    disabled={busy}
                    title="Reset this key to its default in the draft"
                  >
                    Reset to default
                  </button>
                )}
              </div>
            );
          })}
        </div>
      </div>
    </section>
  );
}

interface SettingControlProps {
  view: SettingValueViewWire;
  raw: string;
  editable: boolean;
  onChange: (raw: string) => void;
  onCommit: () => void;
}

export function SettingControl({ view, raw, editable, onChange, onCommit }: SettingControlProps) {
  const def = view.definition;
  const kind = def.valueType.kind;
  const secret = isSecretView(view);
  const commitOnEnter = (event: React.KeyboardEvent<HTMLInputElement | HTMLSelectElement>) => {
    if (shouldSubmitSettingEnter(event.key, event.nativeEvent.isComposing)) {
      event.preventDefault();
      onCommit();
    }
  };

  if (secret) {
    return (
      <span className="setting-row__secret" title="Secret material is redacted in every settings view and never writable through the web UI">
        🔒 {view.redacted ? '[redacted]' : '[secret]'}
      </span>
    );
  }

  if (kind === 'boolean') {
    const checked = raw === 'true';
    return (
      <label className="setting-row__bool">
        <input
          type="checkbox"
          checked={checked}
          disabled={!editable}
          onChange={(event) => onChange(String(event.target.checked))}
          onKeyDown={commitOnEnter}
        />
        <span>{checked ? 'enabled' : 'disabled'}</span>
      </label>
    );
  }

  if (kind === 'enum') {
    return (
      <select
        value={raw}
        disabled={!editable}
        onChange={(event) => onChange(event.target.value)}
        onKeyDown={commitOnEnter}
      >
        {(def.enumValues || []).map((option) => (
          <option key={option} value={option}>
            {safeText(option)}
          </option>
        ))}
      </select>
    );
  }

  if (kind === 'integer' || kind === 'unsignedInteger' || kind === 'number') {
    return (
      <input
        type="number"
        value={raw}
        disabled={!editable}
        placeholder={typeof def.valueType.min === 'number' ? `min ${def.valueType.min}` : ''}
        onChange={(event) => onChange(event.target.value)}
        onKeyDown={commitOnEnter}
      />
    );
  }

  if (kind === 'string') {
    return (
      <input
        type="text"
        value={raw}
        disabled={!editable}
        placeholder={def.valueType.nonEmpty ? 'non-empty string' : 'string'}
        onChange={(event) => onChange(event.target.value)}
        onKeyDown={commitOnEnter}
      />
    );
  }

  return (
    <textarea
      className="setting-row__json"
      rows={4}
      value={raw}
      disabled={!editable}
      spellCheck={false}
      onChange={(event) => onChange(event.target.value)}
      onKeyDown={(event) => {
        if (shouldSubmitSettingEnter(event.key, event.nativeEvent.isComposing, true, event.ctrlKey || event.metaKey)) {
          event.preventDefault();
          onCommit();
        }
      }}
    />
  );
}
