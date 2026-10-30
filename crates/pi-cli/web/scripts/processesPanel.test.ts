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

const src = readFileSync(join(dirname(fileURLToPath(import.meta.url)), '..', 'src', 'panels', 'ProcessesPanel.tsx'), 'utf8');
const spawnAt = src.indexOf('const spawnProcess = useCallback(async () => {');
const stopAt = src.indexOf('const stopProcess = useCallback(async (processId: string) => {');
const spawn = spawnAt >= 0 ? src.slice(spawnAt, stopAt > spawnAt ? stopAt : spawnAt + 1200) : '';
const stop = stopAt >= 0 ? src.slice(stopAt, src.indexOf('return (', stopAt)) : '';

check('panel declares busyRef', src.includes('const busyRef = useRef(false);'));
check(
  'spawn locks with busyRef before process_spawn',
  spawn.includes('if (!trimmed || busyRef.current) return')
    && spawn.includes('busyRef.current = true')
    && spawn.indexOf('busyRef.current = true') < spawn.indexOf("type: 'process_spawn'"),
);
check(
  'spawn validates cwd before claiming the lock',
  spawn.includes('cannot spawn.') && spawn.indexOf('cannot spawn.') < spawn.indexOf('busyRef.current = true'),
);
check(
  'spawn keeps draft until accepted ACK',
  spawn.includes('current.trim() === trimmed') && !spawn.includes("setSpawnCommand('');"),
);
check(
  'spawn clears label only when unchanged',
  spawn.includes('setSpawnLabel((current) => current === rawLabel') && !spawn.includes("setSpawnLabel('');"),
);
check('spawn surfaces process spawn failed', spawn.includes('process spawn failed'));
check('spawn releases the lock in finally', spawn.includes('busyRef.current = false'));
check(
  'stop locks with busyRef before process_stop',
  stop.includes('if (busyRef.current) return')
    && stop.indexOf('busyRef.current = true') < stop.indexOf("type: 'process_stop'")
    && stop.includes('busyRef.current = false'),
);
check('poll skips while the lock is held', src.includes('if (busy || busyRef.current) return;'));

console.log(`\nprocessesPanel.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);
