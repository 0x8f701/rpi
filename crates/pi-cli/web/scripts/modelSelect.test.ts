#!/usr/bin/env node
// Focused regression for src/modelSelect.ts — the React-free helper that
// groups the Web model <select> by provider so identically named models stay
// distinguishable. RPC identity is never rewritten: option value remains
// `${provider}/${id}`, which is what onModelChange / get_state already use.
//
// Run through `npm run build`, which bundles this file with Vite's installed
// esbuild into a disposable Node-compatible module before executing the
// focused assertions (same pattern as scripts/toolTitle.test.ts).
//
// Exit codes: 0 = every assertion held; 1 = a grouping/identity regression.
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server.browser';
import { HeaderSelect } from '../src/App.tsx';
import {
  EMPTY_SESSION_MODEL_STATE,
  SessionModelMutationQueue,
  applySessionModelEvent,
  groupModelsByProvider,
  keepSelectedModelOption,
  reconcileSessionModel,
  requestSessionModel,
  sessionModelView,
  settleSessionModel,
  wireModelKey,
} from '../src/modelSelect.ts';

const failures = [];
let ran = 0;
function check(name, cond, detail) {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

function valuesOf(groups) {
  return groups.flatMap((g) => g.models.map((m) => m.value));
}

function labelsOf(groups) {
  return groups.flatMap((g) => g.models.map((m) => m.label));
}

// ---- identity is provider/id, never the display name ----
{
  const groups = groupModelsByProvider([
    { provider: 'openai', id: 'gpt-4', name: 'GPT-4' },
    { provider: 'azure', id: 'gpt-4', name: 'GPT-4' },
  ]);
  check(
    'option values stay provider/id even when labels collide',
    valuesOf(groups).join('|') === 'azure/gpt-4|openai/gpt-4',
    valuesOf(groups).join('|'),
  );
  check(
    'duplicate display names remain two options',
    labelsOf(groups).filter((l) => l === 'GPT-4').length === 2,
  );
}
// ---- duplicate display names remain separate under their provider groups ----
{
  const groups = groupModelsByProvider([
    { provider: 'openai', id: 'gpt-4', name: 'GPT-4' },
    { provider: 'azure', id: 'gpt-4', name: 'GPT-4' },
    { provider: 'openai', id: 'o1', name: 'o1' },
  ]);
  check('two providers for two GPT-4s', groups.length === 2, `n=${groups.length}`);
  check(
    'providers sort deterministically (azure before openai)',
    groups.map((g) => g.provider).join('|') === 'azure|openai',
    groups.map((g) => g.provider).join('|'),
  );
  const azure = groups.find((g) => g.provider === 'azure');
  const openai = groups.find((g) => g.provider === 'openai');
  check('azure group has the azure GPT-4', azure?.models.length === 1);
  check('openai group keeps both of its models', openai?.models.length === 2);
  const gpt4Values = valuesOf(groups).filter((_, i) => labelsOf(groups)[i] === 'GPT-4');
  check('duplicate GPT-4 labels yield two options', gpt4Values.length === 2, gpt4Values.join(','));
  check('azure GPT-4 value is azure/gpt-4', gpt4Values.includes('azure/gpt-4'));
  check('openai GPT-4 value is openai/gpt-4', gpt4Values.includes('openai/gpt-4'));
  check(
    'current azure/gpt-4 selection still matches an option value',
    valuesOf(groups).includes('azure/gpt-4'),
  );
  check(
    'current openai/gpt-4 selection still matches an option value',
    valuesOf(groups).includes('openai/gpt-4'),
  );
}

// ---- input order does not affect group or model order ----
{
  const a = groupModelsByProvider([
    { provider: 'zebra', id: 'z-2', name: 'Beta' },
    { provider: 'apple', id: 'a-2', name: 'Zed' },
    { provider: 'zebra', id: 'z-1', name: 'Alpha' },
    { provider: 'apple', id: 'a-1', name: 'Able' },
  ]);
  const b = groupModelsByProvider([
    { provider: 'apple', id: 'a-1', name: 'Able' },
    { provider: 'apple', id: 'a-2', name: 'Zed' },
    { provider: 'zebra', id: 'z-1', name: 'Alpha' },
    { provider: 'zebra', id: 'z-2', name: 'Beta' },
  ]);
  check(
    'shuffled catalog produces the same provider order',
    a.map((g) => g.provider).join('|') === 'apple|zebra',
  );
  check(
    'shuffled catalog produces the same option values',
    valuesOf(a).join('|') === valuesOf(b).join('|'),
    `${valuesOf(a).join('|')} vs ${valuesOf(b).join('|')}`,
  );
  check(
    'models inside a provider sort by label then id',
    valuesOf(a).join('|') === 'apple/a-1|apple/a-2|zebra/z-1|zebra/z-2',
    valuesOf(a).join('|'),
  );
}

// ---- missing name falls back to id; invalid rows drop; first-wins dedupe ----
{
  const groups = groupModelsByProvider([
    null,
    undefined,
    { provider: 'openai', id: '', name: 'nope' },
    { provider: '', id: 'x', name: 'nope' },
    { id: 'orphan', name: 'no provider' },
    { provider: 'local', name: 'no id' },
    { provider: 'local', id: 'llama', name: '' },
    { provider: 'local', id: 'llama', name: 'ignored duplicate' },
    { provider: 'local', id: 'phi', name: 'Phi' },
  ]);
  check('invalid rows are dropped', groups.length === 1, `n=${groups.length}`);
  check('empty name uses id as label', groups[0]?.models[0]?.label === 'llama');
  check('empty-name option value stays provider/id', groups[0]?.models[0]?.value === 'local/llama');
  check('duplicate provider/id keeps the first label', groups[0]?.models[0]?.name === '');
  check(
    'remaining sibling is still listed',
    groups[0]?.models.map((m) => m.value).join('|') === 'local/llama|local/phi',
    groups[0]?.models.map((m) => m.value).join('|'),
  );
}

// ---- empty catalog ----
check('empty catalog yields no groups', groupModelsByProvider([]).length === 0);

// ---- model/thinking mutations preserve request order per session ----
{
  const queue = new SessionModelMutationQueue();
  const order = [];
  let releaseFirst = () => {};
  const firstGate = new Promise<void>((resolve) => { releaseFirst = resolve; });
  const first = queue.enqueue('session-a', async () => {
    order.push('first-start');
    await firstGate;
    order.push('first-end');
  });
  const second = queue.enqueue('session-a', async () => {
    order.push('second');
  });
  await Promise.resolve();
  await Promise.resolve();
  check('same-session second mutation waits', order.join('|') === 'first-start', order.join('|'));
  releaseFirst();
  await Promise.all([first, second]);
  check(
    'same-session mutations finish in request order',
    order.join('|') === 'first-start|first-end|second',
    order.join('|'),
  );
}

// ---- different sessions do not share a model mutation queue ----
{
  const queue = new SessionModelMutationQueue();
  const started = [];
  let releaseA = () => {};
  const gateA = new Promise<void>((resolve) => { releaseA = resolve; });
  const sessionA = queue.enqueue('session-a', async () => {
    started.push('a');
    await gateA;
  });
  const sessionB = queue.enqueue('session-b', async () => {
    started.push('b');
  });
  await Promise.resolve();
  await Promise.resolve();
  check('different-session mutations start independently', started.join('|') === 'a|b', started.join('|'));
  releaseA();
  await Promise.all([sessionA, sessionB]);
}

// ---- a failed mutation does not poison later selections ----
{
  const queue = new SessionModelMutationQueue();
  await queue.enqueue('session-a', async () => {
    throw new Error('expected failure');
  }).catch(() => undefined);
  let recovered = false;
  await queue.enqueue('session-a', async () => {
    recovered = true;
  });
  check('failed mutation releases the per-session queue', recovered);
}

// ---- a wire state event carrying model fills the key; incomplete rows
// ---- yield '' so the select keeps its placeholder ----
{
  check(
    'get_state/model_select row fills the provider/id key',
    wireModelKey({ provider: 'prov', id: 'm-1' }) === 'prov/m-1',
  );
  check('model row without provider keeps the key unknown', wireModelKey({ id: 'm-1' }) === '');
  check('model row without id keeps the key unknown', wireModelKey({ provider: 'prov' }) === '');
  check('missing model row keeps the key unknown', wireModelKey(null) === '');
}

// ---- the header select always renders the empty placeholder first: an
// ---- unknown controlled key ('') shows the placeholder, never the first
// ---- model; a known key still selects its own option ----
{
  const groups = [
    { provider: 'prov', models: [{ id: 'm-1', name: 'Model One', value: 'prov/m-1', label: 'Model One' }] },
  ];
  const unknown = renderToStaticMarkup(createElement(HeaderSelect, {
    id: 'model-select',
    title: 'Model (set_model)',
    value: '',
    disabled: false,
    placeholder: 'model…',
    groups,
    onChange: () => {},
  }));
  const selected = unknown.match(/<option[^>]*selected[^>]*>[^<]*<\/option>/)?.[0] ?? '';
  check('unknown model key renders the placeholder option', unknown.includes('>model…</option>'), unknown);
  check(
    'unknown model key selects the placeholder, never the first model',
    selected.includes('>model…</option>') && !selected.includes('Model One'),
    selected || 'no selected option',
  );
  const known = renderToStaticMarkup(createElement(HeaderSelect, {
    id: 'model-select',
    title: 'Model (set_model)',
    value: 'prov/m-1',
    disabled: false,
    placeholder: 'model…',
    groups,
    onChange: () => {},
  }));
  check(
    'known model key still selects its own option, not the placeholder',
    /<option\b[^>]*value="prov\/m-1"[^>]*selected=""/.test(known)
      && !(known.match(/<option[^>]*selected[^>]*>[^<]*<\/option>/)?.[0] ?? '').includes('model…'),
    known,
  );
  const thinking = renderToStaticMarkup(createElement(HeaderSelect, {
    id: 'thinking-select',
    title: 'Thinking level (set_thinking_level)',
    value: '',
    disabled: false,
    placeholder: 'thinking…',
    levels: ['off', 'high'],
    onChange: () => {},
  }));
  check(
    'unknown thinking level renders the thinking placeholder first',
    thinking.indexOf('>thinking…</option>') < thinking.indexOf('>off</option>')
      && (thinking.match(/<option[^>]*selected[^>]*>[^<]*<\/option>/)?.[0] ?? '').includes('thinking…'),
    thinking,
  );
}

{
  const sid = 'sess-a';
  const fromState = reconcileSessionModel(EMPTY_SESSION_MODEL_STATE, sid, 'prov/old');
  check('first get_state fills an empty session model slot', sessionModelView(fromState)[sid] === 'prov/old');

  const pick = requestSessionModel(fromState, sid, 'cliproxy/gpt-6-astra');
  check('optimistic pick shows the new key while set_model is in flight', sessionModelView(pick.state)[sid] === 'cliproxy/gpt-6-astra');
  const second = requestSessionModel(pick.state, sid, 'other/new');
  check('a second pick while one is in flight does not replace the pending key', second.send === false && sessionModelView(second.state)[sid] === 'cliproxy/gpt-6-astra');
  const duringPending = reconcileSessionModel(pick.state, sid, 'prov/old');
  check('watchdog get_state cannot revert a pending pick', sessionModelView(duringPending)[sid] === 'cliproxy/gpt-6-astra');

  const seq = pick.state.pending.get(sid).seq;
  const settled = settleSessionModel(pick.state, sid, seq, true, 'cliproxy/gpt-6-astra');
  check('set_model ack settles the picked key', sessionModelView(settled)[sid] === 'cliproxy/gpt-6-astra');
  const reconnect = reconcileSessionModel(settled, sid, 'other/restored');
  check('a later get_state with a different model updates after the pick settled', sessionModelView(reconnect)[sid] === 'other/restored');
  const lateAck = settleSessionModel(settled, sid, seq, true, 'prov/old');
  check('a late set_model ack for the previous model cannot replace a settled pick', sessionModelView(lateAck)[sid] === 'cliproxy/gpt-6-astra');

  const failedPick = requestSessionModel(fromState, sid, 'cliproxy/gpt-6-astra');
  const reverted = settleSessionModel(failedPick.state, sid, failedPick.state.pending.get(sid).seq, false);
  check('a failed set_model reverts the header to the previous settled model', sessionModelView(reverted)[sid] === 'prov/old');

  const fromEvent = applySessionModelEvent(settled, sid, 'other/new');
  check('a live model_select event updates the settled header key', sessionModelView(fromEvent)[sid] === 'other/new');
}

{
  const groups = groupModelsByProvider([
    { provider: 'prov', id: 'm-1', name: 'Model One' },
  ]);
  const kept = keepSelectedModelOption(groups, 'cliproxy/gpt-6-astra');
  check(
    'catalog gap injects the session model so the header can keep showing it',
    kept.some((group) => group.models.some((model) => model.value === 'cliproxy/gpt-6-astra')),
    JSON.stringify(kept),
  );
  const markup = renderToStaticMarkup(createElement(HeaderSelect, {
    id: 'model-select',
    title: 'Model (set_model)',
    value: 'cliproxy/gpt-6-astra',
    disabled: false,
    placeholder: 'model…',
    groups: kept,
    onChange: () => {},
  }));
  check(
    'a persisted key missing from the catalog still selects its own option, not model…',
    /<option\b[^>]*value="cliproxy\/gpt-6-astra"[^>]*selected=""/.test(markup)
      && !(markup.match(/<option[^>]*selected[^>]*>[^<]*<\/option>/)?.[0] ?? '').includes('model…'),
    markup,
  );
  const alreadyPresent = keepSelectedModelOption(groups, 'prov/m-1');
  check(
    'keepSelectedModelOption is a no-op when the catalog already has the key',
    alreadyPresent.length === 1 && alreadyPresent[0].models.map((model) => model.value).join('|') === 'prov/m-1',
  );
}


if (failures.length > 0) {
  console.error(`modelSelect: ${failures.length}/${ran} checks FAILED:\n- ${failures.join('\n- ')}`);
  process.exit(1);
}
console.log(`modelSelect: ${ran} checks passed`);
