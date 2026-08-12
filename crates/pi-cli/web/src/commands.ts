/**
 * Slash-command catalog pure helpers — shared by App.tsx's composer Command
 * picker and the node-runnable regression test (scripts/commands.test.ts). No
 * DOM/browser/socket dependency so `esbuild --platform=node` can bundle the
 * test in isolation.
 *
 * The backend `get_commands` RPC is the catalog authority: it projects each
 * command as `{name, description, source, argumentHint, requiresArguments}`.
 * These helpers only (a) normalize that wire into a stable list, (b) filter
 * it to the commands the Web composer can actually execute, (c) search it,
 * (d) compose the draft text for a selection (trailing space iff the backend
 * marks `requiresArguments`), and (e) parse a submitted input line back into a
 * supported command + argument tail for Main's dispatch wiring. No second
 * command catalog is hardcoded — only the 3-name Web-execution surface.
 */

/** One normalized command from the `get_commands` wire response. The backend
 *  projects `{name, description, source, argumentHint, requiresArguments, skillName}`;
 *  `source` is one of `builtin | prompt | skill | extension`. `argumentHint`
 *  is the usage placeholder (e.g. `<name>`, `[<from> <to>]`) and
 *  `requiresArguments` is true only for commands that cannot run bare. Skill
 *  entries carry `name: "skill:<bare>"` (the stable wire dispatch name) plus a
 *  bare `skillName` so the Web composer can compose `/skill <name>` without
 *  re-parsing the wire name. */
export interface CommandEntry {
  name: string;
  description: string;
  source: string;
  argumentHint: string;
  requiresArguments: boolean;
  skillName?: string;
}

/** The only slash commands the Web composer can execute. The picker filters
 *  the backend catalog to this surface so every shown item is dispatchable
 *  (Main wires real dispatch for these three). Static literal → Record. */
export const WEB_SUPPORTED_COMMANDS: Record<string, true> = {
  compact: true,
  skill: true,
  'code-review': true,
};

/** Coerce an `unknown` field to `string` — non-strings become `''` so a
 *  malformed wire entry never throws in the picker. Five call sites share
 *  this lockstep coercion. */
function stringField(value: unknown): string {
  return typeof value === 'string' ? value : '';
}

/** Normalize a `get_commands` wire response (`{commands: [...]}`) into a
 *  stable `CommandEntry[]`. The backend catalog is authoritative — this only
 *  filters malformed entries, coerces scalar fields, and de-duplicates by
 *  name (first occurrence wins, preserving backend order). `requiresArguments`
 *  is true only when the wire sends literal `true`; absent/non-bool → false.
 *  Skill entries (`source: "skill"`) keep their bare `skillName` when the wire
 *  sends a non-empty string. Returns `[]` for any shape that is not
 *  `{commands: array}`. */
export function normalizeCommands(data: unknown): CommandEntry[] {
  if (!data || typeof data !== 'object') return [];
  // `in` narrows so the property read is compiler-checked (no unchecked cast);
  // after the guard `data` is `object`, so `data.commands` is `unknown`.
  const commands = 'commands' in data ? data.commands : undefined;
  if (!Array.isArray(commands)) return [];
  const seen = new Set<string>();
  const out: CommandEntry[] = [];
  for (const raw of commands) {
    if (!raw || typeof raw !== 'object') continue;
    const name = stringField('name' in raw ? raw.name : undefined);
    if (!name || seen.has(name)) continue;
    seen.add(name);
    const source = stringField('source' in raw ? raw.source : undefined);
    // Skill candidates carry a bare skillName (`"skill:<bare>"` → `"<bare>"`).
    // Drop a skill entry whose skillName is missing/empty — without it the
    // composer cannot compose `/skill <name>`, so it is not a usable candidate.
    const skillName = stringField('skillName' in raw ? raw.skillName : undefined);
    if (source === 'skill' && !skillName) continue;
    const entry: CommandEntry = {
      name,
      description: stringField('description' in raw ? raw.description : undefined),
      source,
      argumentHint: stringField('argumentHint' in raw ? raw.argumentHint : undefined),
      requiresArguments: 'requiresArguments' in raw && raw.requiresArguments === true,
    };
    if (source === 'skill') entry.skillName = skillName;
    out.push(entry);
  }
  return out;
}

/** Filter a normalized catalog to the Web-executable surface, preserving
 *  backend order. The picker shows the WEB_SUPPORTED_COMMANDS builtins
 *  (compact/skill/code-review) PLUS loaded skill candidates
 *  (`source === "skill"`) so `/skill` can drill into the real loaded skills.
 *  Prompt and extension dynamic commands stay executable on the backend but
 *  never enter the Web picker surface (their dispatch is not wired here). */
export function filterSupportedCommands(commands: CommandEntry[]): CommandEntry[] {
  return commands.filter(
    (command) => command.name in WEB_SUPPORTED_COMMANDS || isSkillCandidate(command),
  );
}

/** True when `command` is a loaded skill candidate the picker can drill into.
 *  A usable candidate has `source === "skill"` AND a non-empty bare
 *  `skillName` (normalizeCommands already drops skill entries without one, so
 *  the skillName check is defensive against a hand-built entry). */
export function isSkillCandidate(command: CommandEntry): boolean {
  return command.source === 'skill' && !!command.skillName;
}

/** The skill candidates from a normalized+filtered catalog, in backend order.
 *  The picker switches to this list when the user selects/types `/skill`. */
export function skillCandidates(commands: CommandEntry[]): CommandEntry[] {
  return commands.filter(isSkillCandidate);
}

/** The primary Web-executable builtins (compact/skill/code-review) from a
 *  normalized+filtered catalog, in backend order. The picker shows this list
 *  first; selecting the `/skill` parent switches to [`skillCandidates`]. */
export function primaryCommands(commands: CommandEntry[]): CommandEntry[] {
  return commands.filter((command) => !isSkillCandidate(command));
}

/** Compose the composer text for a selected skill candidate: `/skill <name>`
 *  (no auto-submit). Uses the bare `skillName` so the wire `skill:<name>` never
 *  reaches the textarea. The trailing draft is intentional — Main's submit
 *  path parses `/skill <name>` back into the typed `skill` RPC. */
export function composeSkillCommandText(skillName: string): string {
  return `/skill ${skillName}`;
}

/** Case-insensitive search over a normalized catalog. An empty/whitespace
 *  query returns every command (the picker shows the full supported menu);
 *  otherwise a command matches when the query is a substring of its name, its
 *  bare `skillName` (for skill candidates), OR its description. */
export function filterCommands(commands: CommandEntry[], query: string): CommandEntry[] {
  const q = query.trim().toLowerCase();
  if (!q) return commands;
  return commands.filter(
    (command) =>
      command.name.toLowerCase().includes(q) ||
      (command.skillName ? command.skillName.toLowerCase().includes(q) : false) ||
      command.description.toLowerCase().includes(q),
  );
}

/** Interpret the current composer draft when the Command picker opens.
 * `/skill` opens the skill catalog; `/skill <query>` opens it prefiltered by
 * that query so a concrete skill can be found without first clicking the
 * generic `/skill` parent. Any other draft opens the primary command list. */
export function pickerIntentFromDraft(draft: string): { mode: 'commands' | 'skills'; query: string } {
  const match = draft.trim().match(/^\/skill(?:\s+(.*))?$/i);
  if (!match) return { mode: 'commands', query: '' };
  return { mode: 'skills', query: (match[1] || '').trim() };
}

/** Compose the text to insert into the composer for a selected command. A
 *  command the backend marks `requiresArguments` gets a trailing space so the
 *  user types its argument next (`/skill <name>`); optional-arg and no-arg
 *  commands are inserted bare (`/code-review`, `/compact`) — driving this off
 *  the backend field keeps get_commands the single source of truth. */
export function composeCommandText(name: string, requiresArguments: boolean): string {
  const slash = name.startsWith('/') ? name : `/${name}`;
  return requiresArguments ? `${slash} ` : slash;
}

/** Append a picker-selected draft to the current composer value, preserving
 *  the user's editable text: an empty/whitespace-only value takes the draft
 *  as-is, otherwise the draft goes on a NEW line (no extra blank line when
 *  the value already ends with `\n`). Pure — the DOM write happens in the
 *  component. Mirrors the STT commit's separator rule so a command appended
 *  to existing draft text stays on its own line. */
export function appendDraft(current: string, draft: string): string {
  // Nothing to append — return the value untouched. An empty draft must not
  // mutate the user's editable text (e.g. inject a dangling separator '\n').
  if (!draft) return current;
  const separator = current.trim() ? (current.endsWith('\n') ? '' : '\n') : '';
  return `${current}${separator}${draft}`;
}

/** Parse a submitted composer input line into a supported Web command + its
 *  argument tail, or `null` when the text is not one of the supported slash
 *  commands. Exposed so Main can wire real command dispatch off the textarea
 *  text without re-parsing in the RPC layer; this helper performs no RPC
 *  mapping (that remains Main's job). A bare `/compact` yields
 *  `{name:'compact', args:''}`; `/skill foo bar` yields
 *  `{name:'skill', args:'foo bar'}`. */
export function parseSupportedCommand(text: string): { name: string; args: string } | null {
  const trimmed = text.trim();
  if (!trimmed.startsWith('/')) return null;
  const rest = trimmed.slice(1);
  // First non-space run is the command name; the remainder (trimmed) is the
  // argument tail. A bare `/` or `/ ` is not a supported command.
  const match = rest.match(/^(\S+)\s*(.*)$/);
  if (!match) return null;
  const name = match[1];
  if (!(name in WEB_SUPPORTED_COMMANDS)) return null;
  return { name, args: match[2].trim() };
}