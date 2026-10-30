#!/usr/bin/env node
// PathAutocomplete — pure keyboard/highlight/join helpers, the
// generation-guarded completion controller (fake timers + scripted send),
// and combobox DOM markup via react-dom/server.
// Run via `npm run build` (esbuild + node).
import assert from 'node:assert/strict';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server.browser';
import {
  PathAutocomplete,
  PathCompletionListbox,
  createPathCompletionController,
  joinCompletion,
  nextHighlight,
  pathKeyAction,
  pathPrefixOf,
  PATH_COMPLETE_LIMIT,
  type PathCompleteEntry,
  type PathCompleteRequest,
  type PathCompleteResponse,
  type PathCompletionMenu,
} from '../src/PathAutocomplete';

let ran = 0;
const failures: string[] = [];
function check(name: string, cond: boolean, detail?: unknown): void {
  ran += 1;
  if (!cond) failures.push(`${name}${detail !== undefined ? `: ${JSON.stringify(detail)}` : ''}`);
}

const dirEntry: PathCompleteEntry = { name: 'work', path: '/tmp/work', kind: 'directory' };
const dirSlashEntry: PathCompleteEntry = { name: 'work', path: '/tmp/work/', kind: 'directory' };
const fileEntry: PathCompleteEntry = { name: 'main.rs', path: '/tmp/main.rs', kind: 'file' };
const response = (entries: PathCompleteEntry[], truncated = false): PathCompleteResponse => ({
  base: '/tmp',
  prefix: 'wo',
  entries,
  truncated,
});

// ---- prefix extraction ----
check('absolute path prefix', pathPrefixOf('/home/user/pro') === 'pro');
check('relative path prefix', pathPrefixOf('sub/dir/fo') === 'fo');
check('trailing slash has empty prefix', pathPrefixOf('/tmp/') === '');
check('bare word prefix', pathPrefixOf('work') === 'work');

// ---- highlight movement ----
check('down from none selects first', nextHighlight(-1, 1, 3) === 0);
check('up from none wraps to last', nextHighlight(-1, -1, 3) === 2);
check('down advances', nextHighlight(0, 1, 3) === 1);
check('down wraps', nextHighlight(2, 1, 3) === 0);
check('up wraps', nextHighlight(0, -1, 3) === 2);
check('empty menu stays none', nextHighlight(0, 1, 0) === -1);

// ---- keyboard contract ----
check('Enter with menu closed submits', pathKeyAction('Enter', false).type === 'submit');
check('arrows with menu closed are inert', pathKeyAction('ArrowDown', false).type === 'none');
check('Tab with menu closed is inert', pathKeyAction('Tab', false).type === 'none');
check('Escape with menu closed is inert', pathKeyAction('Escape', false).type === 'none');
check('Enter with menu open accepts', pathKeyAction('Enter', true).type === 'accept');
check('Tab with menu open accepts', pathKeyAction('Tab', true).type === 'accept');
check('Escape with menu open closes', pathKeyAction('Escape', true).type === 'close');
check('ArrowDown with menu open moves down', pathKeyAction('ArrowDown', true).type === 'move');
check('ArrowUp with menu open moves up', pathKeyAction('ArrowUp', true).type === 'move');
check('typing keys are inert', pathKeyAction('a', true).type === 'none');

// ---- accept joining ----
check('directory accept appends slash', joinCompletion(dirEntry) === '/tmp/work/');
check('directory with trailing slash stays', joinCompletion(dirSlashEntry) === '/tmp/work/');
check('file accept is exact', joinCompletion(fileEntry) === '/tmp/main.rs');

// ---- completion controller: debounce + generation guard ----
interface FakeTimer {
  fire(): Promise<void>;
  cancelCount(): number;
}
function fakeTimers(): {
  timers: { delay(fn: () => void, ms: number): () => void };
  lastTimer(): FakeTimer;
} {
  const live: Array<{ fn: () => void; cancels: number }> = [];
  return {
    timers: {
      delay(fn) {
        const entry = { fn, cancels: 0 };
        live.push(entry);
        return () => {
          entry.cancels += 1;
        };
      },
    },
    lastTimer: () => {
      const entry = live[live.length - 1];
      assert.ok(entry, 'no pending timer');
      return {
        fire: async () => {
          if (entry.cancels === 0) {
            entry.fn();
          }
          await Promise.resolve();
        },
        cancelCount: () => entry.cancels,
      };
    },
  };
}

function scriptedHost() {
  const sent: PathCompleteRequest[] = [];
  const shown: PathCompletionMenu[] = [];
  let hideCount = 0;
  const gates: Array<{
    promise: Promise<PathCompleteResponse | null>;
    resolve: (res: PathCompleteResponse | null) => void;
    reject: (err: unknown) => void;
  }> = [];
  return {
    sent,
    shown,
    hides: () => hideCount,
    host: {
      send(req: PathCompleteRequest): Promise<PathCompleteResponse | null> {
        sent.push(req);
        const gate = Promise.withResolvers<PathCompleteResponse | null>();
        gates.push(gate);
        return gate.promise;
      },
      show(menu: PathCompletionMenu) {
        shown.push(menu);
      },
      hide() {
        hideCount += 1;
      },
    },
    respond(index: number, res: PathCompleteResponse | null): void {
      gates[index].resolve(res);
    },
    reject(index: number, err: Error): void {
      gates[index].reject(err);
    },
  };
}

{
  const { timers, lastTimer } = fakeTimers();
  const { host, sent, shown, hides, respond, reject } = scriptedHost();
  const controller = createPathCompletionController(host, timers);
  controller.request('/tmp/wo', 'directory');
  check('request is debounced', sent.length === 0);
  check('nonblank request closes any prior menu immediately', hides() === 1);
  await lastTimer().fire();
  check('debounced request sent', sent.length === 1 && sent[0].path === '/tmp/wo');
  check('request carries wire shape', sent[0].type === 'path_complete' && sent[0].limit === PATH_COMPLETE_LIMIT);
  // Request B supersedes A: old menu already closed by the request itself.
  controller.request('/tmp/wor', 'directory');
  check('value change closes old menu before debounce', hides() === 2 && sent.length === 1);
  const timerB = lastTimer();
  respond(0, response([{ name: 'old', path: '/tmp/old', kind: 'directory' }]));
  await Promise.resolve();
  await Promise.resolve();
  check('stale response dropped, menu not opened', shown.length === 0);

  await timerB.fire();
  check('superseded request sent', sent.length === 2 && sent[1].path === '/tmp/wor');
  respond(1, response([dirEntry]));
  await Promise.resolve();
  await Promise.resolve();
  controller.cancel();
  check('cancel hides menu', hides() === 3);
  // Blank values hide immediately without sending.
  controller.request('   ', 'directory');
  check('blank value hides without send', hides() === 4 && sent.length === 2);

  // Error responses hide instead of throwing.
  controller.request('/tmp/w', 'directory');
  await lastTimer().fire();
  check('errored request sent', sent.length === 3);
  reject(2, new Error('denied'));
  await Promise.resolve();
  await Promise.resolve();
  await Promise.resolve();
  await Promise.resolve();
  check('rejected send hides menu', hides() === 6 && shown.length === 1);
}

{
  // Escape/blur semantics: cancel() must poison the in-flight request so a
  // late response cannot reopen the menu.
  const { timers, lastTimer } = fakeTimers();
  const { host, sent, shown, respond } = scriptedHost();
  const controller = createPathCompletionController(host, timers);

  controller.request('/tmp/wo', 'directory');
  await lastTimer().fire();
  check('cancel-scenario request sent', sent.length === 1);
  controller.cancel();
  respond(0, response([dirEntry]));
  await Promise.resolve();
  await Promise.resolve();
  check('late completion after cancel does not reopen', shown.length === 0);

  // A cancelled (never-fired) debounce must stay dead: firing its handle
  // after cancel sends nothing.
  controller.request('/tmp/wo2', 'directory');
  const zombie = lastTimer();
  controller.cancel();
  await zombie.fire();
  check('cancelled pending timer never sends', sent.length === 1);
  check('cancelled request closed menu', shown.length === 0);
}

{
  // Truncated pass-through + limit slice.
  const { timers, lastTimer } = fakeTimers();
  const { host, sent, shown, respond } = scriptedHost();
  const controller = createPathCompletionController(host, timers);
  const entries: PathCompleteEntry[] = Array.from({ length: PATH_COMPLETE_LIMIT + 20 }, (_, i) => ({
    name: `d${i}`,
    path: `/tmp/d${i}`,
    kind: 'directory',
  }));

  controller.request('/tmp/d', 'any');
  await lastTimer().fire();
  check('any-kind request passes kind through', sent[0]?.kind === 'any');
  respond(0, response(entries, true));
  await Promise.resolve();
  await Promise.resolve();
  check('entries capped at limit', shown.length === 1 && shown[0].entries.length === PATH_COMPLETE_LIMIT);
  check('truncated flag passed through', shown[0].truncated === true);
}

// ---- closed-state combobox DOM (effects never run in static markup) ----
{
  const html = renderToStaticMarkup(
    createElement(PathAutocomplete, {
      id: 'session-new-cwd-input',
      value: '',
      onChange: () => {},
      sendComplete: () => Promise.resolve(null),
      kind: 'directory',
      placeholder: 'New session cwd…',
      ariaLabel: 'New session working directory',
    }),
  );
  check('input keeps caller id', html.includes('id="session-new-cwd-input"'));
  check('input is a combobox', html.includes('role="combobox"'));
  check('menu closed collapses expansion', html.includes('aria-expanded="false"'));
  check('no listbox controls while closed', !html.includes('aria-controls='));
  check('no active descendant while closed', !html.includes('aria-activedescendant'));
  check('aria-label preserved', html.includes('aria-label="New session working directory"'));
  check('placeholder preserved', html.includes('placeholder="New session cwd…'));
  check('autocomplete list hint set', html.includes('aria-autocomplete="list"'));
  const requiredHtml = renderToStaticMarkup(
    createElement(PathAutocomplete, {
      id: 'x',
      value: '',
      onChange: () => {},
      sendComplete: () => Promise.resolve(null),
      kind: 'directory',
      ariaLabel: 'cwd',
      required: true,
    }),
  );
  check('required flag forwards to input', requiredHtml.includes('required=""'));
  check('required absent by default', !html.includes('required='));
  check('menu absent while closed', !html.includes('path-autocomplete__menu'));
  const disabledHtml = renderToStaticMarkup(
    createElement(PathAutocomplete, {
      id: 'x',
      value: '/tmp',
      onChange: () => {},
      sendComplete: () => Promise.resolve(null),
      kind: 'directory',
      ariaLabel: 'cwd',
      disabled: true,
    }),
  );
  check('disabled input renders disabled', disabledHtml.includes('disabled=""'));
}

// ---- open listbox DOM via the extracted listbox component ----
{
  const html = renderToStaticMarkup(
    createElement(PathCompletionListbox, {
      listId: 'session-new-cwd-input-listbox',
      idBase: 'session-new-cwd-input',
      menu: { entries: [dirEntry, fileEntry], truncated: true },
      highlight: 1,
      onAccept: () => {},
    }),
  );
  check('listbox id wired for aria-controls', html.includes('id="session-new-cwd-input-listbox"'));
  check('listbox role present', html.includes('role="listbox"'));
  check('options carry stable option ids', html.includes('id="session-new-cwd-input-option-0"'));
  check('highlighted option aria-selected', html.includes('aria-selected="true"'));
  check('unhighlighted option not selected', html.includes('aria-selected="false"'));
  check('active class on highlighted option', html.includes('path-autocomplete__option--active'));
  check('directory kind badge rendered', html.includes('path-autocomplete__kind--directory'));
  check('file kind badge rendered', html.includes('path-autocomplete__kind--file'));
  check('entry names rendered', html.includes('>work</span>') && html.includes('>main.rs</span>'));
  check('truncation hint rendered', html.includes('keep typing to narrow'));
}

if (failures.length > 0) {
  console.error(`pathAutocomplete.test: ${failures.length} FAILED`);
  for (const failure of failures) console.error(`  - ${failure}`);
  process.exit(1);
}
console.log(`pathAutocomplete.test: ${ran} assertions passed`);