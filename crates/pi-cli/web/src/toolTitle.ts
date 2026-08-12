// Human-readable tool-card titles — React-free pure helper (unit-tested by
// scripts/toolTitle.test.ts, rendered by App.ToolCard). The wire `toolName`
// stays untouched (data-tool-name keeps the raw value); this function only
// maps the visible card title: known tools get fixed titles, unknown
// snake_case/kebab-case names are converted to Title Case with common
// acronyms (IRC/RPC/LSP/URL/HTTP/JSON/SQL/API/ID) preserved.
//
// The input is redacted BEFORE title-casing so credential shapes survive the
// transform (splitting on `_`/`-` would otherwise destroy `sk-…`-style
// markers and let a raw secret through at display). The call site still
// wraps the result in safeText() for defense in depth.

import { redactSecrets } from './redact';

// Fixed titles for well-known wire tool names (key = normalized lowercase).
const KNOWN_TITLES: Record<string, string> = {
  task: 'Task',
  todo: 'Todo',
  edit: 'Edit',
  bash: 'Command',
  process: 'Process',
  read: 'Read',
  write: 'Write',
  hub: 'Hub',
  irc: 'IRC',
  web_search: 'Web Search',
  image_generate: 'Image Generate',
  generate_image: 'Image Generate',
  browser: 'Browser',
  ask: 'Ask',
  glob: 'Glob',
  grep: 'Grep',
  lsp: 'LSP',
};

// Acronyms preserved verbatim wherever they appear as a whole word.
const ACRONYMS: Record<string, true> = {
  IRC: true,
  RPC: true,
  LSP: true,
  URL: true,
  HTTP: true,
  JSON: true,
  SQL: true,
  API: true,
  ID: true,
};

/**
 * Map a wire tool name to its visible card title.
 *
 * Known tools return the fixed title; anything else is split on `_`/`-`/space
 * and Title-Cased word by word, with whole-word acronyms kept uppercase. The
 * result is meant to pass through safeText() at the call site; it never
 * changes the wire value.
 */
export function humanToolTitle(raw: unknown): string {
  const name = redactSecrets(raw == null ? '' : String(raw)).trim();
  if (name === '') return '';
  const known = KNOWN_TITLES[name.toLowerCase()];
  if (known !== undefined) return known;
  return name
    .split(/[_\-\s]+/)
    .filter((part) => part !== '')
    .map((part) => {
      const upper = part.toUpperCase();
      return ACRONYMS[upper] === true ? upper : part.charAt(0).toUpperCase() + part.slice(1);
    })
    .join(' ');
}
