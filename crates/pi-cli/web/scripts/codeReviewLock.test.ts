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
const web = readFileSync(join(here, '..', 'src', 'panels', 'CodeReviewPanel.tsx'), 'utf8');
const android = readFileSync(join(here, '..', '..', '..', '..', 'mobile', 'src', 'screens', 'ReviewPanel.tsx'), 'utf8');

check('web declares mutatingRef', web.includes('const mutatingRef = useRef(false);'));

const comment = sliceFn(web, 'const submitComment = useCallback(() => {', '// Close/refresh are guarded');
const refresh = sliceFn(web, 'const refresh = useCallback(() => {', 'const abort = useCallback(() => {');
const abort = sliceFn(web, 'const abort = useCallback(() => {', 'const runAutoReview = useCallback(() => {');
const openReview = sliceFn(web, 'const openReview = useCallback((forceOpen = false) => {', 'openReviewRef.current = openReview;');

check(
  'submitComment returns on byte-limit before claiming mutatingRef',
  comment.includes('CODE_REVIEW_MAX_COMMENT_BYTES')
    && comment.indexOf('CODE_REVIEW_MAX_COMMENT_BYTES') < comment.indexOf('mutatingRef.current = true'),
);
check(
  'submitComment returns on empty text+images before claiming mutatingRef',
  comment.includes('if (!text && images.length === 0) return')
    && comment.indexOf('if (!text && images.length === 0) return') < comment.indexOf('mutatingRef.current = true'),
);
check(
  'submitComment claims mutatingRef before code_review_comment',
  comment.includes('if (mutatingRef.current) return')
    && comment.includes('mutatingRef.current = true')
    && comment.indexOf('mutatingRef.current = true') < comment.indexOf("type: 'code_review_comment'"),
);
check('submitComment keeps live.value === submittedText draft clear', comment.includes('live.value === submittedText'));
check(
  'refresh claims mutatingRef before code_review_refresh',
  refresh.includes('if (mutatingRef.current) return')
    && refresh.includes('mutatingRef.current = true')
    && refresh.indexOf('mutatingRef.current = true') < refresh.indexOf("type: 'code_review_refresh'"),
);
check(
  'abort validates identity before claiming mutatingRef',
  abort.includes('if (!selectedIdentity || !selectedIsCancellable) return')
    && abort.indexOf('if (!selectedIdentity || !selectedIsCancellable) return') < abort.indexOf('mutatingRef.current = true'),
);
check(
  'abort claims mutatingRef before sendCommand',
  abort.includes('if (mutatingRef.current) return')
    && abort.includes('mutatingRef.current = true')
    && abort.indexOf('mutatingRef.current = true') < abort.indexOf('sendCommand(reviewCommand(buildCodeReviewAbortPayload(identity)))'),
);
check(
  'openReview claims mutatingRef before sendCommand',
  openReview.includes('mutatingRef.current = true')
    && openReview.indexOf('mutatingRef.current = true') < openReview.indexOf('sendCommand(reviewCommand(command)'),
);
check(
  'openReview does not recurse openReview(true) on no-open snapshot',
  !openReview.includes('if (snapshotAlreadyOpen) openReview(true)'),
);
check(
  'openReview no-open fallback sends codeReviewOpenCommand under the same claim',
  openReview.includes('isNoOpenCodeReviewError(err) && snapshotAlreadyOpen')
    && openReview.includes('return sendCommand(')
    && openReview.includes('codeReviewOpenCommand(openArgs)')
    && openReview.indexOf('mutatingRef.current = true') < openReview.indexOf('codeReviewOpenCommand(openArgs)'),
);
check(
  'codeReviewOpenCommand is code_review_open',
  readFileSync(join(here, '..', 'src', 'codeReview.ts'), 'utf8').includes("const command: Record<string, unknown> = { type: 'code_review_open' }"),
);
check('web mutatingRef released in finally before seq-gated setBusy', web.includes('mutatingRef.current = false'));

check('android declares mutatingRef', android.includes('const mutatingRef = useRef(false);'));
const androidComment = sliceFn(android, 'const submitComment = useCallback(async () => {', 'const stopReply = useCallback(async () => {');
const androidStop = sliceFn(android, 'const stopReply = useCallback(async () => {', 'const pickModel = useCallback(async (value: string) => {');
const androidModel = sliceFn(android, 'const pickModel = useCallback(async (value: string) => {', 'useEffect(() => {');
check(
  'android submitComment claims mutatingRef',
  androidComment.includes("if (!identity || !identity.snapshotId || text === '' || mutatingRef.current) return")
    && androidComment.includes('mutatingRef.current = true')
    && androidComment.indexOf('mutatingRef.current = true') < androidComment.indexOf("setBusy('comment')"),
);
check(
  'android submitComment trim-matches draft',
  androidComment.includes("setDraft((current) => current.trim() === text ? '' : current)")
    && !androidComment.includes("setDraft('');"),
);
check('android submitComment keeps Comment rejected', androidComment.includes("'Comment rejected'"));
check(
  'android stopReply claims mutatingRef instead of busy',
  androidStop.includes('mutatingRef.current')
    && !androidStop.includes('busy !== null')
    && androidStop.includes("'Stop failed'"),
);
check(
  'android pickModel claims mutatingRef instead of busy',
  androidModel.includes('if (mutatingRef.current) return')
    && androidModel.includes('mutatingRef.current = true')
    && !androidModel.includes('busy !== null')
    && androidModel.includes("'Model change failed'"),
);

console.log(`\ncodeReviewLock.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);
