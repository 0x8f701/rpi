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

const src = readFileSync(join(dirname(fileURLToPath(import.meta.url)), '..', 'src', 'chat', 'SessionView.tsx'), 'utf8');
const submitAt = src.indexOf('const submit = useCallback(async () => {');
const submit = submitAt >= 0 ? src.slice(submitAt, submitAt + 2800) : '';
const catchAt = submit.indexOf('} catch (error) {');
const finallyAt = submit.indexOf('} finally {');
const catchBody = catchAt >= 0 && finallyAt > catchAt ? submit.slice(catchAt, finallyAt) : '';
const finallyBody = finallyAt >= 0 ? submit.slice(finallyAt) : '';
const claimAt = submit.indexOf('sendingRef.current = true');
const dispatchAt = submit.indexOf('dispatchComposerPrompt');
const emptyAt = submit.indexOf('if (!acceptedText) return;');
const disconnectedAt = submit.indexOf("onTransportError('Runner is not connected.');");

check('sendingRef is declared next to sending state', src.includes('const [sending, setSending] = useState(false);\n  const sendingRef = useRef(false);'));
check('submit checks sendingRef.current before work', submit.includes('if (sendingRef.current || composerLocked) return;') && !submit.includes('if (sending || composerLocked)'));
check('empty and disconnected guards return before claiming the ref', emptyAt >= 0 && disconnectedAt >= 0 && claimAt >= 0 && emptyAt < claimAt && disconnectedAt < claimAt);
check('submit claims sendingRef.current = true before dispatchComposerPrompt', claimAt >= 0 && dispatchAt >= 0 && claimAt < dispatchAt);
check('finally sets sendingRef.current = false', finallyBody.includes('sendingRef.current = false'));
check('draft still clears with current === submittedDraft', submit.includes('current === submittedDraft'));
check('catch still restores setDraft(submittedDraft)', catchBody.includes('setDraft(submittedDraft)'));
check('sending remains presentation', src.includes("sending ? 'Sending…'") && src.includes('sending || draft.trim()'));

console.log(`\nsessionViewSending.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);
