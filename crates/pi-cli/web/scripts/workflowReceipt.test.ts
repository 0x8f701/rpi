#!/usr/bin/env node
// Focused receipt-classification regression for src/panels/WorkflowPanel.tsx.
// Outcome tones stay verbatim from the backend contract: queued/woken/revived
// are in-flight (pending), processed is never promoted to success, and failed
// plus any unknown outcome stay failed. Run through `npm run build`.
//
// Exit codes: 0 = every assertion held; 1 = a regression.
import { receiptPresentation, receiptTone, type IrcReceipt } from '../src/panels/WorkflowPanel';

const failures: string[] = [];
let ran = 0;
function check(name: string, condition: unknown, detail = ''): void {
  ran += 1;
  if (!condition) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

// ---- in-flight outcomes classify as pending ----
check('queued is pending', receiptTone('queued') === 'pending');
check('woken is pending', receiptTone('woken') === 'pending');
check('revived is pending', receiptTone('revived') === 'pending');

// ---- processed stays the backend delivery classification, never success ----
check('processed stays processed', receiptTone('processed') === 'processed');
check('processed is not pending', receiptTone('processed') !== 'pending');
check('processed is not failed', receiptTone('processed') !== 'failed');

// ---- failed and any unknown outcome are failed ----
check('failed is failed', receiptTone('failed') === 'failed');
check('unknown outcome is failed', receiptTone('something-new') === 'failed');

// ---- receipts keep requested alias, outcome tone, and error ----
{
  const receipts: IrcReceipt[] = [
    { to: 'researcher', outcome: 'woken' },
    { to: 'job-uuid-agent', requested: 'writer', outcome: 'failed', error: 'mailbox closed' },
    { to: 'planner', outcome: 'processed', messageId: 'msg-1' },
  ];
  const rendered = receipts.map(receiptPresentation);
  const expected = [
    { target: 'researcher', tone: 'pending', outcome: 'woken', error: '', messageId: '' },
    { target: 'writer', tone: 'failed', outcome: 'failed', error: 'mailbox closed', messageId: '' },
    { target: 'planner', tone: 'processed', outcome: 'processed', error: '', messageId: 'msg-1' },
  ];
  check(
    'receipts retain requested alias, verbatim outcome, tone, error, and message id',
    JSON.stringify(rendered) === JSON.stringify(expected),
    JSON.stringify(rendered),
  );
}

console.log(`\nworkflowReceipt.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);
