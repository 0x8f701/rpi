// Personas wire shapes + pure helpers (backend: crates/pi-cli/src/modes/rpc.rs
// persona_* commands, which mirror the TUI `/persona` surface). Every model-
// or file-derived string passes through safeText() at render; the helpers here
// only validate wire shapes and build bounded display text — never HTML.
//
// RPC surface:
//   persona_list   -> { enabled, personas: PersonaRowWire[] }
//   persona_get    -> PersonaRowWire & { content, contentTruncated }
//   persona_create -> { name, created, message }   (mirrors /persona new)
//   persona_edit   -> { name, edited, message }    (mirrors /persona edit)
//   persona_remove -> { name, removed, message }   (confirm: true required)
//   persona_purge  -> { name, purged, message }    (confirm: true required)
//   persona_select -> { name, preferred, message } (mirrors --select)
//   persona_clear  -> { preferred: null, message } (mirrors --clear)
//   persona_current-> { name, message }            (mirrors --current)
//
// Task spawn for a persona is the EXISTING task_spawn RPC with `agent` set to
// the persona's agent name (the same path the `task` tool and `/persona run`
// use) — never a heuristic or a second persona store.

export interface PersonaRowWire {
  name: string;
  description: string;
  source: string;
  trusted: boolean;
  preferred: boolean;
  contractSummary: string;
  memoryEntries: number | null;
  sessionCount: number | null;
  /** Fixed literal `"unreadable"` or null — never path text. */
  stateError: string | null;
}

export interface PersonaDetailWire extends PersonaRowWire {
  content: string;
  contentTruncated: boolean;
}

/** Core persona-name charset, mirroring `persona_name_path_safe`. */
export const PERSONA_NAME_PATTERN = /^[A-Za-z0-9_-]+$/;
export const PERSONA_NAME_MAX = 64;

/** Editable content cap for the Web editor (UTF-16 units). The core discovery
 * bound is 256 KiB BYTES; 64 KiB UTF-16 units can never exceed it for any
 * Unicode input, so content edited here always passes the backend size check.
 * Larger definitions are shown read-only with a /persona edit hint. */
export const PERSONA_EDIT_MAX_UNITS = 64 * 1024;

/** Parse the frontmatter `name:` scalar mirroring the backend's
 * `parse_frontmatter` + `unquote` (crates/pi-coding/src/orchestration/
 * definitions.rs): double- or single-quoted values (`name: "mentor"` /
 * `'mentor'`), unquoted values with inline ` #…` comments, `#` comment lines,
 * CRLF or LF endings, and indented lines skipped (they are block children,
 * never the top-level name). Returns the declared name, or null when the
 * frontmatter is missing or the scalar cannot be resolved — callers treat
 * null as "no opinion" so a backend-legal file is never wrongly rejected. */
export function declaredFrontmatterName(content: string): string | null {
  const match = /^---(?:\r?\n)([\s\S]*?)(?:\r?\n)---(?:\r?\n|$)/.exec(content);
  if (!match) return null;
  for (const rawLine of match[1].split(/\r?\n/)) {
    const line = rawLine.replace(/\s+$/, '');
    const trimmed = line.trim();
    if (trimmed === '' || trimmed.startsWith('#') || trimmed.startsWith('- ')) continue;
    if (line.length > trimmed.length) continue; // indented = block child, not the name
    const colon = line.indexOf(':');
    if (colon < 0) continue;
    if (line.slice(0, colon).trim() !== 'name') continue;
    return unquoteFrontmatterScalar(line.slice(colon + 1).trim());
  }
  return null;
}

/** Mirror of the backend `unquote` for a frontmatter scalar. Returns null for
 * unterminated quotes (the backend errors on those too). */
function unquoteFrontmatterScalar(value: string): string | null {
  if (value.startsWith('"') || value.endsWith('"')) {
    if (!(value.length >= 2 && value.startsWith('"') && value.endsWith('"'))) return null;
    return value
      .slice(1, -1)
      .replace(/\\n/g, '\n')
      .replace(/\\t/g, '\t')
      .replace(/\\"/g, '"')
      .replace(/\\\\/g, '\\');
  }
  if (value.startsWith("'") || value.endsWith("'")) {
    if (!(value.length >= 2 && value.startsWith("'") && value.endsWith("'"))) return null;
    return value.slice(1, -1).replace(/''/g, "'");
  }
  const hash = value.indexOf(' #');
  return (hash >= 0 ? value.slice(0, hash) : value).replace(/\s+$/, '');
}

/** Validate a persona name against the core path-safe charset. Returns an
 * error message or null when valid. */
export function validatePersonaName(name: string): string | null {
  if (name.length === 0) return 'persona name is required';
  if (name.length > PERSONA_NAME_MAX) {
    return `persona name must be 1..=${PERSONA_NAME_MAX} characters`;
  }
  if (!PERSONA_NAME_PATTERN.test(name)) {
    return "persona name must contain only ASCII letters, digits, '_' or '-'";
  }
  return null;
}

/** Narrow a wire row; malformed rows fail closed (dropped, never rendered). */
function isPersonaRow(value: unknown): value is PersonaRowWire {
  if (!value || typeof value !== 'object') return false;
  const row = value as Record<string, unknown>;
  return (
    typeof row.name === 'string' &&
    typeof row.description === 'string' &&
    typeof row.source === 'string' &&
    typeof row.trusted === 'boolean' &&
    typeof row.preferred === 'boolean' &&
    typeof row.contractSummary === 'string' &&
    (row.memoryEntries === null || typeof row.memoryEntries === 'number') &&
    (row.sessionCount === null || typeof row.sessionCount === 'number') &&
    (row.stateError === null || typeof row.stateError === 'string')
  );
}

/** Parse a persona_list payload into rows (never throws). */
export function parsePersonaList(data: unknown): PersonaRowWire[] {
  if (!data || typeof data !== 'object' || !('personas' in data)) return [];
  const rows = data.personas;
  if (!Array.isArray(rows)) return [];
  return rows.filter(isPersonaRow);
}

/** Parse a persona_get payload into a detail view (or null when malformed). */
export function parsePersonaDetail(data: unknown): PersonaDetailWire | null {
  if (!isPersonaRow(data)) return null;
  // The row guard narrows to the PersonaRowWire interface (no index
  // signature), so widen to a record via a named const and validate the two
  // detail-only fields with typeof before assembling the typed view.
  const record = data as unknown as Record<string, unknown>;
  if (typeof record.content !== 'string') return null;
  if (typeof record.contentTruncated !== 'boolean') return null;
  return {
    name: data.name,
    description: data.description,
    source: data.source,
    trusted: data.trusted,
    preferred: data.preferred,
    contractSummary: data.contractSummary,
    memoryEntries: data.memoryEntries,
    sessionCount: data.sessionCount,
    stateError: data.stateError,
    content: record.content,
    contentTruncated: record.contentTruncated,
  };
}

/** Human-facing scope label, mirroring `persona_source_label`. */
export function sourceLabel(source: string): string {
  switch (source) {
    case 'user':
      return 'user scope';
    case 'project':
      return 'project scope';
    case 'bundled':
      return 'bundled';
    default:
      return source;
  }
}

/** One-line persistence summary for a row: memory/session counts are durable
 * persona state kept under the persona root (remove keeps them, purge deletes
 * them). Never renders path text. */
export function persistenceLine(row: PersonaRowWire): string {
  if (row.stateError) return 'memory/session state unreadable';
  const memory =
    row.memoryEntries == null
      ? 'unknown'
      : `${row.memoryEntries} ${row.memoryEntries === 1 ? 'entry' : 'entries'}`;
  const sessions =
    row.sessionCount == null
      ? 'unknown'
      : `${row.sessionCount} ${row.sessionCount === 1 ? 'archive' : 'archives'}`;
  return `memory: ${memory} · sessions: ${sessions}`;
}

/** Seed template for a new persona (mirrors `persona_editor_seed`). */
export function buildPersonaCreateContent(name: string): string {
  return (
    `---\nname: ${name}\ndescription: ${name} persona\n---\n` +
    `Describe ${name}'s behavior, personality, and contract here.\n` +
    `Optional frontmatter: personality, softBudget, maxTurns, timeoutSecs, tools.\n`
  );
}

/** The Web Run button wire: reuse the existing task_spawn RPC with the
 * persona's agent name — a task spawn pointing at the persona agent, exactly
 * like the TUI `/persona run <name> <assignment>`. */
export function buildPersonaRunCommand(name: string, task: string): Record<string, unknown> {
  return { type: 'task_spawn', args: { task, agent: name } };
}

/** Confirm-dialog vocabulary: remove vs purge must stay clearly distinct. */
export const REMOVE_NOTE =
  'Removes persona.md only. memory/ and sessions/ stay under the persona root, so the persona state survives.';
export const PURGE_NOTE =
  'Deletes the whole persona root: persona.md, memory/, and sessions/. The persona and all its persisted state are gone.';

/** Whether a definition can be edited in the Web editor without truncation
 * risk (the backend may hold files up to 256 KiB; only smaller ones round-trip
 * through the bounded textarea losslessly). */
export function isEditableContent(content: string): boolean {
  return content.length <= PERSONA_EDIT_MAX_UNITS;
}

/** Natural-language note shown in the panel footer: `让 <persona> …` is
 * resolved by the main agent through the existing orchestration agent
 * catalog/task tool — never by front-end prompt heuristics. */
export const NL_INVOCATION_NOTE =
  'Natural language: a prompt like “让 <persona> …” is handled by the main agent via the agent catalog/task tool — the same persisted definitions listed here.';

/** Extract the backend's human-facing `message` field from a persona RPC
 * response (persona_create/edit/remove/purge/select/clear/current), or null
 * when absent/malformed. */
export function responseMessage(data: unknown): string | null {
  if (!data || typeof data !== 'object' || !('message' in data)) return null;
  const record = data as Record<string, unknown>;
  return typeof record.message === 'string' ? record.message : null;
}

/** Minimal spawned-job shape returned by task_spawn (the Run button path). */
export interface PersonaSpawnWire {
  jobId?: string;
  agentId?: string;
}

/** Parse a task_spawn payload's `spawns` array (malformed entries dropped). */
export function parseSpawns(data: unknown): PersonaSpawnWire[] {
  if (!data || typeof data !== 'object' || !('spawns' in data)) return [];
  const record = data as Record<string, unknown>;
  const spawns = record.spawns;
  if (!Array.isArray(spawns)) return [];
  return spawns.flatMap((spawn) => {
    if (!spawn || typeof spawn !== 'object') return [];
    const spawnRecord = spawn as Record<string, unknown>;
    const jobId = typeof spawnRecord.jobId === 'string' ? spawnRecord.jobId : undefined;
    const agentId = typeof spawnRecord.agentId === 'string' ? spawnRecord.agentId : undefined;
    if (jobId === undefined && agentId === undefined) return [];
    return [{ jobId, agentId }];
  });
}
