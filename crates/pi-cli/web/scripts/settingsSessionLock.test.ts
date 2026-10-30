#!/usr/bin/env node
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const failures: string[] = [];
let ran = 0;
function check(name: string, cond: boolean, detail = ''): void {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

const here = dirname(fileURLToPath(import.meta.url));
const settings = readFileSync(join(here, '..', 'src', 'panels', 'SettingsPanel.tsx'), 'utf8');
const session = readFileSync(join(here, '..', 'src', 'panels', 'SessionPanel.tsx'), 'utf8');

check('settings declares busyRef', settings.includes('const busyRef = useRef(false);'));

function sliceFn(src: string, start: string, end: string): string {
  const at = src.indexOf(start);
  if (at < 0) return '';
  const stop = src.indexOf(end, at + start.length);
  return stop > at ? src.slice(at, stop) : src.slice(at, at + 1600);
}

const openDraft = sliceFn(settings, 'const openDraft = useCallback(async () => {', 'const editLocalDraft');
const applyDraft = sliceFn(settings, 'const applyDraft = useCallback(async () => {', 'const cancelDraft');
const cancelDraft = sliceFn(settings, 'const cancelDraft = useCallback(async () => {', '}, [draftId, loadCatalog, sendCommand, updateLocalDrafts]);');
const commitReset = sliceFn(settings, 'const commitReset = useCallback(', 'const applyDraft = useCallback');
const commitOneEdit = sliceFn(settings, 'const commitOneEdit = useCallback((key: string) => {', 'const commitReset');

check(
  'openDraft validates catalogEnabled before claiming busyRef',
  openDraft.includes('if (!catalogEnabled) return')
    && openDraft.indexOf('if (!catalogEnabled) return') < openDraft.indexOf('busyRef.current = true'),
);
check(
  'openDraft claims busyRef before settings_open_draft',
  openDraft.includes('if (busyRef.current) return')
    && openDraft.includes('busyRef.current = true')
    && openDraft.indexOf('busyRef.current = true') < openDraft.indexOf("type: 'settings_open_draft'"),
);
check(
  'applyDraft claims busyRef before settings_apply',
  applyDraft.includes('if (!draftId || busyRef.current) return')
    && applyDraft.includes('busyRef.current = true')
    && applyDraft.indexOf('busyRef.current = true') < applyDraft.indexOf("type: 'settings_apply'"),
);
check(
  'cancelDraft validates draftId before claiming busyRef',
  cancelDraft.includes('if (!draftId) return')
    && cancelDraft.indexOf('if (!draftId) return') < cancelDraft.indexOf('busyRef.current = true'),
);
check(
  'cancelDraft claims busyRef before settings_cancel',
  cancelDraft.includes('if (busyRef.current) return')
    && cancelDraft.indexOf('busyRef.current = true') < cancelDraft.indexOf("type: 'settings_cancel'"),
);
check(
  'commitReset validates draftId before claiming busyRef',
  commitReset.includes('if (!draftId) return')
    && commitReset.indexOf('if (!draftId) return') < commitReset.indexOf('busyRef.current = true'),
);
check(
  'commitReset claims busyRef before settings_reset',
  commitReset.includes('if (busyRef.current) return')
    && commitReset.indexOf('busyRef.current = true') < commitReset.indexOf("type: 'settings_reset'"),
);
check(
  'commitOneEdit claims busyRef before commitEdit',
  commitOneEdit.includes('if (busyRef.current) return')
    && commitOneEdit.includes('busyRef.current = true')
    && commitOneEdit.indexOf('busyRef.current = true') < commitOneEdit.indexOf('commitEdit(key)'),
);
check('settings releases busyRef in finally', settings.includes('busyRef.current = false'));

const run = sliceFn(session, 'const run = useCallback(', 'const runLifecycle');
const runLifecycle = sliceFn(session, 'const runLifecycle = useCallback(', 'const loadOlder');
check('session declares busyRef', session.includes('const busyRef = useRef(false);'));
check(
  'run claims busyRef before action()',
  run.includes('if (busyRef.current) return')
    && run.includes('busyRef.current = true')
    && run.indexOf('busyRef.current = true') < run.indexOf('await action()'),
);
check(
  'runLifecycle claims busyRef before action()',
  runLifecycle.includes('if (busyRef.current) return')
    && runLifecycle.includes('busyRef.current = true')
    && runLifecycle.indexOf('busyRef.current = true') < runLifecycle.indexOf('await action()'),
);
check('run releases busyRef then mounted setBusy', run.includes('busyRef.current = false') && run.indexOf('busyRef.current = false') < run.indexOf('if (mountedRef.current) setBusy(false)'));
check('runLifecycle releases busyRef then mounted setBusy', runLifecycle.includes('busyRef.current = false') && runLifecycle.indexOf('busyRef.current = false') < runLifecycle.indexOf('if (mountedRef.current) setBusy(false)'));

console.log(`\nsettingsSessionLock.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);
