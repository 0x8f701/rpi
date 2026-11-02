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

const src = readFileSync(join(dirname(fileURLToPath(import.meta.url)), '..', 'src', 'panels', 'LoopPanel.tsx'), 'utf8');
const createAt = src.indexOf('const create = useCallback(async () => {');
const create = createAt >= 0 ? src.slice(createAt, createAt + 900) : '';
const cancelAt = src.indexOf('const cancelLoop = useCallback(async (taskId: string) => {');
const cancel = cancelAt >= 0 ? src.slice(cancelAt, cancelAt + 500) : '';

check('create locks with busyRef before loop_create', create.includes('if (!trimmedPrompt || !interval || busyRef.current) return') && create.includes('busyRef.current = true') && create.indexOf('busyRef.current = true') < create.indexOf("type: 'loop_create'"));
check('create keeps draft until accepted ACK', create.includes('current.trim() === trimmedPrompt') && !create.includes("setPrompt('');"));
check('create surfaces loop create failed', create.includes('loop create failed'));
check('cancel locks with busyRef before loop_delete', cancel.includes('if (busyRef.current) return') && cancel.includes("type: 'loop_delete'"));

console.log(`\nloopPanel.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);
