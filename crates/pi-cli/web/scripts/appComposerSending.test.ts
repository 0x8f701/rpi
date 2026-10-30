#!/usr/bin/env node
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const failures: string[] = [];
let ran = 0;
function check(name: string, cond: boolean): void {
  ran += 1;
  if (!cond) failures.push(name);
}

const src = readFileSync(join(dirname(fileURLToPath(import.meta.url)), '..', 'src', 'App.tsx'), 'utf8');
const submitAt = src.indexOf("const submit = useCallback((kind: 'prompt' | 'steer') => {");
const submitEnd = src.indexOf('const submitComposerEnter = useCallback(', submitAt);
const submit = submitAt >= 0 && submitEnd > submitAt ? src.slice(submitAt, submitEnd) : '';
const inputGate = submit.indexOf('if (!input) return;');
const lockGate = submit.indexOf('if (composerSendingRef.current) return;');
const emptyGate = submit.indexOf("if (!text && codeMessage === '' && videoMarker === '' && pdfMarker === '' && images.length === 0) return;");
const claim = submit.indexOf('composerSendingRef.current = true;');
const send = submit.indexOf('sendCommand(');
const dispatch = submit.indexOf('dispatchComposerSubmit({');
const promptClear = submit.lastIndexOf("input.value = '';", dispatch);

check('composer declares exclusive ref', src.includes('const composerSendingRef = useRef(false);'));
check('submit gates reentry after input guard and before sending', inputGate >= 0 && lockGate > inputGate && send > lockGate);
check('submit claims after empty and attachment validation before sending', emptyGate >= 0 && claim > emptyGate && send > claim && /return;\s*composerSendingRef\.current = true;\s*let dispatched: Promise<unknown> \| null = null;\s*try \{/.test(submit.slice(emptyGate)));
check('submit holds composerSendingRef until dispatched sendCommand settles', /let dispatched: Promise<unknown> \| null = null;/.test(submit) && /if \(dispatched\)/.test(submit) && /dispatched\.finally/.test(submit) && !/\}\s*finally\s*\{\s*composerSendingRef\.current = false;\s*\}\s*\}, \[/.test(submit));
check('attachment prompt dispatch holds the lock across sendCommand', /dispatched = dispatchComposerSubmit\(\{/.test(submit));
check('prompt dispatch still clears composer optimistically', promptClear > claim && dispatch > promptClear && /input\.value = '';\s*flushComposerResize\(input\);/.test(submit.slice(promptClear, dispatch)));
check('App source remains intact', src.split('\n').length > 8000 && !src.includes('[Showing' + ' lines'));

console.log(`\nappComposerSending.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);
