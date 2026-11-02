#!/usr/bin/env node
// Focused regression for src/headerCatalog.ts — the Web header option
// catalogs (model rows + thinking levels) that feed the HeaderSelect.
//
// User-visible contract: the catalogs are scoped to the DISPLAYED session.
// Thinking options derive from that session's model and the model catalog
// from its auth, so a bootstrap-only fetch leaves every other session's real
// thinking level outside the options list — the select then renders its
// "thinking…" placeholder forever even though the server holds the value
// (get_state / subscription cuts always carry thinkingLevel). Before the fix
// there was no per-session catalog module and the stale bootstrap list was
// never replaced, so this suite fails at import/assertion time.
//
// Run through `npm run build` (esbuild + node), same as modelSelect.test.ts.
//
// Exit codes: 0 = every assertion held; 1 = catalog/normalization regression.
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server.browser';
import { HeaderSelect } from '../src/App.tsx';
import {
  EMPTY_HEADER_CATALOGS,
  normalizeHeaderLevels,
  normalizeHeaderModels,
} from '../src/headerCatalog.ts';
import { groupModelsByProvider } from '../src/modelSelect.ts';

const failures = [];
let ran = 0;
function check(name, cond, detail) {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

// ---- model rows: provider/id identity required, malformed rows drop ----
{
  const rows = normalizeHeaderModels([
    { id: 'm-1', name: 'Model One', provider: 'prov' },
    { id: 'm-2', name: '', provider: 'prov' },
    { id: '', name: 'broken', provider: 'prov' },
    { id: 'm-3', name: 'no provider', provider: '' },
    null,
    'not-an-object',
    { id: 'm-1', name: 'dup', provider: 'prov' },
  ]);
  check(
    'model rows keep provider/id identity, drop malformed rows, dedupe',
    rows.length === 2
      && rows[0].id === 'm-1' && rows[0].provider === 'prov' && rows[0].name === 'Model One'
      && rows[1].id === 'm-2' && rows[1].name === '',
    JSON.stringify(rows),
  );
  check('non-array model catalog normalizes to empty', normalizeHeaderModels(undefined).length === 0);
}
// ---- thinking levels: string entries only, non-string entries drop ----
{
  const levels = normalizeHeaderLevels(['off', 'high', 7, null, '', 'high', 'xhigh']);
  check(
    'thinking levels keep string entries, drop non-strings, dedupe',
    levels.join(',') === 'off,high,xhigh',
    JSON.stringify(levels),
  );
  check('non-array levels catalog normalizes to empty', normalizeHeaderLevels('off').length === 0);
  check('empty catalogs start unpopulated', EMPTY_HEADER_CATALOGS.models.length === 0 && EMPTY_HEADER_CATALOGS.levels.length === 0);
}

// ---- the user-visible regression: a displayed session whose real level is
// ---- absent from the STALE bootstrap options list must render the
// ---- placeholder (spec pin), and refreshing the catalogs for that session
// ---- must select the session's own level again (the fix) ----
{
  const stale = ['off'];
  const stuck = renderToStaticMarkup(createElement(HeaderSelect, {
    id: 'thinking-select',
    title: 'Thinking level (set_thinking_level)',
    value: 'high',
    disabled: false,
    placeholder: 'thinking…',
    levels: stale,
    onChange: () => {},
  }));
  check(
    'a real level outside the stale options list renders the placeholder, never a fake value',
    !/<option\b[^>]*selected=/.test(stuck),
    stuck,
  );
  const healed = renderToStaticMarkup(createElement(HeaderSelect, {
    id: 'thinking-select',
    title: 'Thinking level (set_thinking_level)',
    value: 'high',
    disabled: false,
    placeholder: 'thinking…',
    levels: normalizeHeaderLevels(['off', 'minimal', 'low', 'medium', 'high']),
    onChange: () => {},
  }));
  check(
    'after the per-session refresh the session’s real level selects its own option',
    /<option\b[^>]*value="high"[^>]*selected=""/.test(healed)
      && !(healed.match(/<option[^>]*selected[^>]*>[^<]*<\/option>/)?.[0] ?? '').includes('thinking…'),
    healed,
  );
}

// Live psy /web 390: catalogs were populated (get_available_models 473,
// thinking levels off..high) while HeaderSelect value stayed "". Nested cut
// state carried model+thinkingLevel but applyState dropped it, so the
// placeholder option stayed selected.
{
  const groups = groupModelsByProvider([
    { provider: 'cliproxy', id: 'gpt-5.6-sol', name: 'GPT-5.6 Sol' },
  ]);
  const emptyKey = renderToStaticMarkup(createElement(HeaderSelect, {
    id: 'model-select',
    title: 'Model (set_model)',
    value: '',
    disabled: false,
    placeholder: 'model…',
    groups,
    onChange: () => {},
  }));
  check(
    'populated model catalog with an empty selected key still renders the placeholder',
    /<option\b[^>]*value=""[^>]*selected=""/.test(emptyKey)
      && emptyKey.includes('model…')
      && emptyKey.includes('GPT-5.6 Sol'),
    emptyKey,
  );
  const emptyLevel = renderToStaticMarkup(createElement(HeaderSelect, {
    id: 'thinking-select',
    title: 'Thinking level (set_thinking_level)',
    value: '',
    disabled: false,
    placeholder: 'thinking…',
    levels: ['off', 'minimal', 'low', 'medium', 'high'],
    onChange: () => {},
  }));
  check(
    'populated thinking catalog with an empty selected key still renders the placeholder',
    /<option\b[^>]*value=""[^>]*selected=""/.test(emptyLevel)
      && emptyLevel.includes('thinking…')
      && emptyLevel.includes('medium'),
    emptyLevel,
  );
  const recovered = renderToStaticMarkup(createElement(HeaderSelect, {
    id: 'model-select',
    title: 'Model (set_model)',
    value: 'cliproxy/gpt-5.6-sol',
    disabled: false,
    placeholder: 'model…',
    groups,
    onChange: () => {},
  }));
  check(
    'stamped cut model key selects its own option instead of the placeholder',
    /<option\b[^>]*value="cliproxy\/gpt-5.6-sol"[^>]*selected=""/.test(recovered)
      && !(recovered.match(/<option[^>]*selected[^>]*>[^<]*<\/option>/)?.[0] ?? '').includes('model…'),
    recovered,
  );
}

if (failures.length > 0) {
  console.error(`headerCatalog: ${failures.length}/${ran} checks FAILED:\n- ${failures.join('\n- ')}`);
  process.exit(1);
}
console.log(`headerCatalog: ${ran} checks passed`);