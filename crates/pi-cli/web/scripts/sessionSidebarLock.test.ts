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

function sliceFn(src: string, start: string, end: string): string {
  const at = src.indexOf(start);
  if (at < 0) return '';
  const stop = src.indexOf(end, at + start.length);
  return stop > at ? src.slice(at, stop) : src.slice(at, at + 2200);
}

const here = dirname(fileURLToPath(import.meta.url));
const sidebar = readFileSync(join(here, '..', 'src', 'panels', 'SessionSidebar.tsx'), 'utf8');

check('sidebar declares busyRef', sidebar.includes('const busyRef = useRef(false);'));

const run = sliceFn(sidebar, 'const run = useCallback(', 'const runSwitch = useCallback(');
const runSwitch = sliceFn(sidebar, 'const runSwitch = useCallback(', 'const runRefresh = useCallback(');
const runRefresh = sliceFn(sidebar, 'const runRefresh = useCallback(', 'const closeMenu = useCallback(');

check(
  'run claims busyRef before action()',
  run.includes('if (busyRef.current || switchingRef.current !== null) return false')
    && run.includes('busyRef.current = true')
    && run.indexOf('busyRef.current = true') < run.indexOf('await action()'),
);
check(
  'run releases busyRef then switchingRef owner-clear',
  run.includes('busyRef.current = false')
    && run.indexOf('busyRef.current = false') < run.indexOf('if (switchingRef.current === switchingKey)'),
);
check(
  'runRefresh claims busyRef before action()',
  runRefresh.includes('if (busyRef.current || switchingRef.current !== null) return false')
    && runRefresh.includes('busyRef.current = true')
    && runRefresh.indexOf('busyRef.current = true') < runRefresh.indexOf('await action()'),
);
check(
  'runRefresh releases busyRef then mounted setBusy',
  runRefresh.includes('busyRef.current = false')
    && runRefresh.indexOf('busyRef.current = false') < runRefresh.indexOf('if (mountedRef.current) setBusy(false)'),
);
check(
  'runSwitch skips when busyRef held without claiming it',
  runSwitch.includes('if (busyRef.current) return false')
    && !runSwitch.includes('busyRef.current = true')
    && runSwitch.includes('switchingRef.current = switchingKey'),
);
check(
  'runSwitch latest-click-wins still re-targets',
  !runSwitch.includes('if (switchingRef.current !== null)'),
);

console.log(`\nsessionSidebarLock.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);
