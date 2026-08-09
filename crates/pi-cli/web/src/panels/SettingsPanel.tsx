// Settings panel (D92) — schema-driven settings browser with typed edit and
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
  effectiveValue: unknown;
  source: string;
  globalValue?: unknown;
  projectValue?: unknown;
  sessionOverrideValue?: unknown;
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
        if (kind === 'array' && !Array.isArray(parsed)) return { ok: false, message: 'expected a JSON array' };
        if (kind === 'object' && (parsed === null || typeof parsed !== 'object' || Array.isArray(parsed))) {
          return { ok: false, message: 'expected a JSON object' };
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

export function SettingsPanel({ sendCommand, refreshState, onClose }: SettingsPanelProps) {
  const [catalog, setCatalog] = useState<SettingsCatalogWire | null>(null);
  const [draftId, setDraftId] = useState<string | null>(null);
  const [draftDirty, setDraftDirty] = useState(false);
  const [draftValues, setDraftValues] = useState<Map<string, SettingValueViewWire>>(new Map());
  const [selectedCategory, setSelectedCategory] = useState('models');
  const [status, setStatus] = useState('');
  const [busy, setBusy] = useState(false);
  const mountedRef = useRef(true);

  const loadCatalog = useCallback(async () => {
    const data = await sendCommand({ type: 'settings_inspect' });
    if (!mountedRef.current) return;
    // Own RPC contract: SettingsCatalogSnapshot (rpc.rs settings_inspect).
    const snapshot = data as SettingsCatalogWire;
    setCatalog(snapshot);
    if (snapshot?.values && snapshot.values.length > 0) {
      const categories = new Set(snapshot.values.map((view) => view.definition.category));
      if (!categories.has(selectedCategory)) {
        setSelectedCategory(CATEGORY_ORDER.find((category) => categories.has(category)) || 'models');
      }
    }
  }, [selectedCategory]);

  useEffect(() => {
    mountedRef.current = true;
    loadCatalog().catch((err: Error) => {
      if (mountedRef.current) setStatus(`catalog load failed: ${safeText(err.message)}`);
    });
    return () => {
      mountedRef.current = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const applyDraftSnapshot = useCallback((data: unknown) => {
    const snapshot = data as DraftSnapshotWire;
    if (!snapshot?.draftId) return;
    setDraftId(snapshot.draftId);
    setDraftDirty(snapshot.dirty === true);
    const values = new Map<string, SettingValueViewWire>();
    for (const view of Array.isArray(snapshot.values) ? snapshot.values : []) {
      values.set(view.definition.key, view);
    }
    setDraftValues(values);
  }, []);

  const openDraft = useCallback(async () => {
    setBusy(true);
    setStatus('');
    try {
      const data = await sendCommand({ type: 'settings_open_draft', scope: 'global' });
      applyDraftSnapshot(data);
      setStatus('draft open — edits are staged until Apply');
    } catch (err) {
      setStatus(`open draft failed: ${safeText((err as Error).message)}`);
    } finally {
      if (mountedRef.current) setBusy(false);
    }
  }, [applyDraftSnapshot, sendCommand]);

  const commitEdit = useCallback(
    async (key: string, raw: string) => {
      if (!draftId) return;
      const def = catalog?.values.find((view) => view.definition.key === key)?.definition;
      if (!def) return;
      const parsed = parseTypedValue(def.valueType.kind, raw);
      if (!parsed.ok) {
        setStatus(`${key}: ${parsed.message}`);
        return;
      }
      setBusy(true);
      setStatus('');
      try {
        const data = await sendCommand({ type: 'settings_set', draftId, key, value: parsed.value });
        applyDraftSnapshot(data);
      } catch (err) {
        setStatus(`${key}: ${safeText((err as Error).message)}`);
      } finally {
        if (mountedRef.current) setBusy(false);
      }
    },
    [applyDraftSnapshot, catalog, draftId, sendCommand]
  );

  const commitReset = useCallback(
    async (key: string) => {
      if (!draftId) return;
      setBusy(true);
      setStatus('');
      try {
        const data = await sendCommand({ type: 'settings_reset', draftId, key });
        applyDraftSnapshot(data);
        setStatus(`${key} reset to default`);
      } catch (err) {
        setStatus(`${key}: ${safeText((err as Error).message)}`);
      } finally {
        if (mountedRef.current) setBusy(false);
      }
    },
    [applyDraftSnapshot, draftId, sendCommand]
  );

  const applyDraft = useCallback(async () => {
    if (!draftId) return;
    setBusy(true);
    setStatus('');
    try {
      const data = await sendCommand({ type: 'settings_apply', draftId });
      const outcome = data as ApplyOutcomeWire;
      await refreshState();
      await loadCatalog();
      setDraftId(null);
      setDraftValues(new Map());
      setDraftDirty(false);
      if (outcome?.restartRequired) {
        setStatus('settings applied — restart required');
      } else {
        setStatus('settings applied');
      }
    } catch (err) {
      setStatus(`apply failed: ${safeText((err as Error).message)}`);
    } finally {
      if (mountedRef.current) setBusy(false);
    }
  }, [draftId, loadCatalog, refreshState, sendCommand]);

  const cancelDraft = useCallback(async () => {
    if (!draftId) return;
    setBusy(true);
    setStatus('');
    try {
      await sendCommand({ type: 'settings_cancel', draftId });
      setDraftId(null);
      setDraftValues(new Map());
      setDraftDirty(false);
      await loadCatalog();
      setStatus('draft cancelled');
    } catch (err) {
      setStatus(`cancel failed: ${safeText((err as Error).message)}`);
    } finally {
      if (mountedRef.current) setBusy(false);
    }
  }, [draftId, loadCatalog, sendCommand]);

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

  const valueOf = useCallback(
    (view: SettingValueViewWire): unknown => {
      const staged = draftValues.get(view.definition.key);
      return staged ? staged.effectiveValue : view.effectiveValue;
    },
    [draftValues]
  );

  return (
    <section id="settings-panel" className="panel" aria-label="Settings panel">
      <header className="panel__head">
        <span className="panel__title">Settings</span>
        <span className="panel__hint">
          {draftId ? 'draft open — edits staged until Apply' : 'browse effective settings'}
        </span>
        {draftId ? (
          <>
            <button id="settings-apply-btn" type="button" onClick={applyDraft} disabled={busy} title="Persist the staged draft">
              Apply
            </button>
            <button id="settings-cancel-btn" type="button" onClick={cancelDraft} disabled={busy} title="Discard the staged draft">
              Cancel
            </button>
          </>
        ) : (
          <button id="settings-edit-btn" type="button" onClick={openDraft} disabled={busy} title="Open a global-scope edit draft">
            Edit settings
          </button>
        )}
        <button id="settings-refresh-btn" type="button" onClick={() => loadCatalog().catch(() => {})} disabled={busy}>
          Refresh
        </button>
        <button id="settings-close-btn" type="button" onClick={onClose} title="Close panel">
          ✕
        </button>
      </header>

      {status !== '' && (
        <div className={`panel__status${status.includes('failed') ? ' panel__status--error' : ''}`}>
          {safeText(status)}
        </div>
      )}

      <div className="panel__body settings-body">
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
            const rawDefault = def.defaultJson ? valueSummary(JSON.parse(def.defaultJson)) : '—';
            return (
              <div key={def.key} className="setting-row" data-setting-key={def.key}>
                <div className="setting-row__head">
                  <span className="setting-row__key">{safeText(def.key)}</span>
                  {draftDirty && <span className="setting-row__dirty">dirty</span>}
                </div>
                <div className="setting-row__desc">{safeText(def.description)}</div>
                <div className="setting-row__control">
                  <SettingControl
                    view={view}
                    value={value}
                    editable={editable}
                    busy={busy}
                    onCommit={(raw) => commitEdit(def.key, raw)}
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
  value: unknown;
  editable: boolean;
  busy: boolean;
  onCommit: (raw: string) => void;
}

function SettingControl({ view, value, editable, busy, onCommit }: SettingControlProps) {
  const def = view.definition;
  const kind = def.valueType.kind;
  const secret = isSecretView(view);

  if (secret) {
    return (
      <span className="setting-row__secret" title="Secret material is redacted in every settings view and never writable through the web UI">
        🔒 {view.redacted ? '[redacted]' : '[secret]'}
      </span>
    );
  }

  if (kind === 'boolean') {
    const checked = value === true;
    return (
      <label className="setting-row__bool">
        <input
          type="checkbox"
          checked={editable ? checked : Boolean(value)}
          disabled={!editable || busy}
          onChange={(e) => onCommit(String(e.target.checked))}
        />
        <span>{checked ? 'enabled' : 'disabled'}</span>
      </label>
    );
  }

  if (kind === 'enum') {
    const current = typeof value === 'string' ? value : '';
    return (
      <select
        value={current}
        disabled={!editable || busy}
        onChange={(e) => onCommit(e.target.value)}
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
    const numberValue = typeof value === 'number' ? String(value) : '';
    return (
      <input
        type="number"
        value={editable ? numberValue : numberValue}
        disabled={!editable || busy}
        placeholder={typeof def.valueType.min === 'number' ? `min ${def.valueType.min}` : ''}
        onChange={(e) => onCommit(e.target.value)}
        onBlur={(e) => onCommit(e.target.value)}
      />
    );
  }

  if (kind === 'string') {
    const text = typeof value === 'string' ? value : '';
    return (
      <input
        type="text"
        value={editable ? text : text}
        disabled={!editable || busy}
        placeholder={def.valueType.nonEmpty ? 'non-empty string' : 'string'}
        onChange={(e) => onCommit(e.target.value)}
        onBlur={(e) => onCommit(e.target.value)}
      />
    );
  }

  // stringList / array / object: JSON text area.
  const json = value === undefined || value === null ? '' : JSON.stringify(value, null, 2);
  return (
    <textarea
      className="setting-row__json"
      rows={4}
      value={json}
      disabled={!editable || busy}
      spellCheck={false}
      onBlur={(e) => onCommit(e.target.value)}
    />
  );
}
