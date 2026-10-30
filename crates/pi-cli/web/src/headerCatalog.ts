// Header option catalogs (model rows + thinking levels) for the Web
// HeaderSelect. Both catalogs are scoped to the DISPLAYED session: the
// thinking options derive from that session's model and the model catalog
// from its auth. A bootstrap-only fetch leaves every other session's real
// thinking level outside the options list — the select then renders its
// "thinking…" placeholder forever even though the server holds the value
// (get_state / subscription cuts always carry thinkingLevel), so the App
// refetches both catalogs whenever the displayed session or its model
// changes.

export interface HeaderModelRow {
  id: string;
  name: string;
  provider: string;
}

export interface HeaderCatalogs {
  models: HeaderModelRow[];
  levels: string[];
}

export const EMPTY_HEADER_CATALOGS: HeaderCatalogs = { models: [], levels: [] };

/** Rows of a `get_available_models` response. Identity stays provider/id (the
 * HeaderSelect option value); rows without both drop so a partial catalog can
 * never inject an unselectable option, and duplicates keep the first row. */
export function normalizeHeaderModels(wire: unknown): HeaderModelRow[] {
  if (!Array.isArray(wire)) return [];
  const seen = new Set<string>();
  const rows: HeaderModelRow[] = [];
  for (const raw of wire) {
    if (!raw || typeof raw !== 'object' || Array.isArray(raw)) continue;
    const record = raw as Record<string, unknown>;
    const provider = record.provider;
    const id = record.id;
    if (typeof provider !== 'string' || provider === '' || typeof id !== 'string' || id === '') continue;
    const key = `${provider}/${id}`;
    if (seen.has(key)) continue;
    seen.add(key);
    rows.push({ id, provider, name: typeof record.name === 'string' ? record.name : '' });
  }
  return rows;
}

/** Levels of a `get_available_thinking_levels` response. Non-string entries
 * drop so every option value stays comparable to a state's thinkingLevel. */
export function normalizeHeaderLevels(wire: unknown): string[] {
  if (!Array.isArray(wire)) return [];
  const seen = new Set<string>();
  const levels: string[] = [];
  for (const raw of wire) {
    if (typeof raw !== 'string' || raw === '' || seen.has(raw)) continue;
    seen.add(raw);
    levels.push(raw);
  }
  return levels;
}