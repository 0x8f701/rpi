#!/usr/bin/env node
// Focused regression for src/toolTitle.ts — the React-free helper that maps a
// wire `toolName` to its visible ToolCard title (known fixed titles, unknown
// snake_case/kebab-case → Title Case, acronyms preserved, hostile/secrets
// safe through safeText at the call site). The wire name itself is never
// changed — data-tool-name keeps the raw value; this helper only feeds the
// displayed card title.
//
// Run through `npm run build`, which bundles this file with Vite's installed
// esbuild into a disposable Node-compatible module before executing the
// focused assertions (same pattern as scripts/transcript.test.ts).
//
// Exit codes: 0 = every assertion held; 1 = a title regression.
import { humanToolTitle } from '../src/toolTitle.ts';
import { safeText } from '../src/redact.ts';

const failures = [];
let ran = 0;
function check(name, cond, detail) {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

// ---- known tools get fixed titles (wire value untouched elsewhere) ----
{
  const known = {
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
  for (const [wire, expected] of Object.entries(known)) {
    const actual = humanToolTitle(wire);
    check(`known '${wire}' → '${expected}'`, actual === expected, `got '${actual}'`);
  }
}

// ---- known lookup is case-insensitive ----
{
  check('known mixed-case BASH → Command', humanToolTitle('BASH') === 'Command');
  check('known mixed-case Web_Search → Web Search', humanToolTitle('Web_Search') === 'Web Search');
  check('known mixed-case Task → Task', humanToolTitle('Task') === 'Task');
  check('known surrounding whitespace → Read', humanToolTitle('  read ') === 'Read');
}

// ---- unknown snake_case → Title Case ----
{
  check('unknown web_browser_navigate → Web Browser Navigate', humanToolTitle('web_browser_navigate') === 'Web Browser Navigate');
  check('unknown workspace_stats → Workspace Stats', humanToolTitle('workspace_stats') === 'Workspace Stats');
  check('unknown my_mystery_tool → My Mystery Tool', humanToolTitle('my_mystery_tool') === 'My Mystery Tool');
}

// ---- unknown kebab-case → Title Case ----
{
  check('unknown image-generate → Image Generate', humanToolTitle('image-generate') === 'Image Generate');
  check('unknown my-tool → My Tool', humanToolTitle('my-tool') === 'My Tool');
  check('mixed separators fs_stats-watch → Fs Stats Watch', humanToolTitle('fs_stats-watch') === 'Fs Stats Watch');
}

// ---- acronyms preserved as whole words ----
{
  const cases = {
    get_json_schema: 'Get JSON Schema',
    http_server: 'HTTP Server',
    rpc_listener: 'RPC Listener',
    api_id: 'API ID',
    lsp_status: 'LSP Status',
    sql_query: 'SQL Query',
    url_fetch: 'URL Fetch',
    irc_bridge: 'IRC Bridge',
  };
  for (const [wire, expected] of Object.entries(cases)) {
    const actual = humanToolTitle(wire);
    check(`acronym '${wire}' → '${expected}'`, actual === expected, `got '${actual}'`);
  }
  check('acronym standalone irc → IRC', humanToolTitle('irc') === 'IRC');
  check('acronym lowercase api → API', humanToolTitle('api') === 'API');
}
// ---- hostile / edge inputs stay inert and redact-safe at display ----
{
  // Hostile HTML is split on spaces into inert word tokens and first-char
  // capitalized; React renders the title as textContent (never innerHTML) so
  // no element is created. Redaction is a no-op (no credential shapes).
  const hostile = humanToolTitle('<img src=x onerror=alert(1)>');
  check(
    'hostile toolName is split + capitalized but stays inert text',
    hostile === '<img Src=x Onerror=alert(1)>',
    `got '${hostile}'`,
  );
  check('hostile title survives safeText unchanged', safeText(hostile) === hostile);
  // A secret embedded in the wire name is redacted BEFORE title-casing so the
  // credential shape survives the transform and never leaks the raw suffix.
  const secret = humanToolTitle('sk-ABCDEFGHIJKLMNOPQRST');
  check('secret in toolName redacted by the helper', secret === '[REDACTED]', `got '${secret}'`);
  check('secret title still redacted after safeText', safeText(secret) === '[REDACTED]');
  // Redaction runs before the known lookup too: a credential-shaped wire name
  // never matches a known tool by accident.
  check('redacted-known shape does not match a known tool', humanToolTitle('bash sk-ABCDEFGHIJKLMNOPQRST') === 'Bash [REDACTED]');
}

// ---- empty / null inputs ----
{
  check('empty string → empty title', humanToolTitle('') === '');
  check('null → empty title', humanToolTitle(null) === '');
  check('undefined → empty title', humanToolTitle(undefined) === '');
  check('blank string → empty title', humanToolTitle('   ') === '');
}

if (failures.length > 0) {
  console.error(`toolTitle: ${failures.length} assertion(s) failed:`);
  for (const f of failures) console.error(`  - ${f}`);
  process.exit(1);
}
console.log(`toolTitle: ${ran} assertions passed`);
