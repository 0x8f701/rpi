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

const src = readFileSync(join(dirname(fileURLToPath(import.meta.url)), '..', 'src', 'panels', 'SubagentsPanel.tsx'), 'utf8');
const spawnAt = src.indexOf('const spawn = async () => {');
const spawn = spawnAt >= 0 ? src.slice(spawnAt, spawnAt + 1100) : '';
const sendAt = src.indexOf('const sendMessage = async (agentId: string, jobId: string) => {');
const send = sendAt >= 0 ? src.slice(sendAt, sendAt + 1100) : '';

check(
  'spawn locks with busyRef before task_spawn',
  spawn.includes('if (!task || busyRef.current) return')
    && spawn.includes('busyRef.current = true')
    && spawn.indexOf('busyRef.current = true') < spawn.indexOf("type: 'task_spawn'"),
);
check(
  'spawn keeps draft until jobs',
  spawn.includes('current.trim() === task') && !spawn.includes("setSpawnTask('');"),
);
check('spawn keeps task_spawn returned no jobs', spawn.includes('task_spawn returned no jobs'));
check(
  'sendMessage locks with hubSendingRef before hub_send',
  send.includes('if (!body || hubSendingRef.current) return')
    && send.includes('hubSendingRef.current = true')
    && send.indexOf('hubSendingRef.current = true') < send.indexOf("type: 'hub_send'"),
);
check(
  'sendMessage trim-matches messageDrafts',
  send.includes("const current = prev[jobId] || ''")
    && send.includes('if (current.trim() !== body) return prev')
    && send.includes('{ ...prev, [jobId]: \'\' }'),
);

console.log(`\nsubagentsPanel.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);
