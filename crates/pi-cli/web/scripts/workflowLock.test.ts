#!/usr/bin/env node
// Exclusive-lock source scan for src/panels/WorkflowPanel.tsx: create and IRC
// send claim a ref synchronously before sendCommand, and drafts clear only
// after ACK via trim-match. Run through a cheap esbuild node build.
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const failures: string[] = [];
let ran = 0;
function check(name: string, cond: boolean, detail = ''): void {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

const src = readFileSync(join(dirname(fileURLToPath(import.meta.url)), '..', 'src', 'panels', 'WorkflowPanel.tsx'), 'utf8');
const createAt = src.indexOf('const createWorkflow = useCallback(() => {');
const create = createAt >= 0 ? src.slice(createAt, createAt + 1400) : '';
const sendAt = src.indexOf('const sendIrc = useCallback(');
const send = sendAt >= 0 ? src.slice(sendAt, sendAt + 1400) : '';

check(
  'create locks with creatingRef before workflow_create',
  create.includes('if (!name || !objective || creatingRef.current) return')
    && create.includes('creatingRef.current = true')
    && create.indexOf('creatingRef.current = true') < create.indexOf("type: 'workflow_create'"),
);
check(
  'create keeps drafts until accepted ACK',
  create.includes("current.trim() === name ? '' : current")
    && create.includes("current.trim() === objective ? '' : current")
    && !create.includes("setCreateName('');")
    && !create.includes("setCreateObjective('');"),
);
check(
  'sendIrc locks with ircSendingRef before workflow_hub_send',
  send.includes('if (!body || !recipient || ircSendingRef.current) return')
    && send.includes('ircSendingRef.current = true')
    && send.indexOf('ircSendingRef.current = true') < send.indexOf("type: 'workflow_hub_send'"),
);
check(
  'sendIrc keeps body until accepted ACK',
  send.includes("current.trim() === body ? '' : current")
    && !send.includes("setIrcBody('');"),
);

console.log(`\nworkflowLock.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);
