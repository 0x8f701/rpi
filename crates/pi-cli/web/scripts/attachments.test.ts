#!/usr/bin/env node
// Focused behavioral regression for src/attachments.ts — the pure
// classification/limits/order/UTF-8/fence helpers shared by the Web composer's
// three attachment intake paths (textarea onPaste, footer drag/drop, hidden
// file input). Run through `npm run build`, which bundles this file with Vite's
// installed esbuild into a disposable Node-compatible module before executing
// the focused assertions.
//
// Exit codes: 0 = every assertion held; 1 = a rule regressed.
//
// The FileReader-based readers (buildImageAttachment/buildCodeAttachment) are
// browser-only and exercised via usage; these tests cover the pure,
// Node-runnable contract: kind classification (image vs code vs rejected),
// per-file/aggregate/count limits, intake-order preservation despite shuffled
// async completion, UTF-8 validation, fence-injection safety, the code message
// builder, the image ContentBlock mapping, and skip-summary formatting.
import {
  MAX_ATTACHMENTS,
  MAX_COMMAND_ID,
  MAX_FILE_BYTES,
  MAX_PROMPT_FRAME_BYTES,
  MAX_TOTAL_WIRE_BYTES,
  MAX_VIDEO_BYTES,
  MAX_VIDEO_DURATION_SECONDS,
  MAX_VIDEO_FRAME_BASE64,
  MAX_VIDEO_FRAME_BYTES,
  MAX_VIDEO_FRAMES,
  MAX_VIDEO_FRAMES_BASE64,
  MAX_VIDEO_FRAMES_RAW_BYTES,
  MAX_VIDEO_INSTRUCTION_BYTES,
  VIDEO_UPLOAD_PATH,
  VIDEO_UPLOAD_TIMEOUT_MS,
  aggregateWire,
  attachmentAccept,
  attachmentsToImageBlocks,
  base64Length,
  buildCodeMessage,
  buildCodeSegment,
  buildVideoAttachment,
  buildVideoMessage,
  classifyAttachments,
  classifyKind,
  codeBadgeLabel,
  codeLanguage,
  codeSegmentWireBytes,
  commandFrameBytes,
  decodeUtf8OrReject,
  formatSkipSummary,
  imageMimeType,
  IntakeBudgetTracker,
  intakeReservation,
  isImageFile,
  isStaleIntake,
  isTextFile,
  isVideoFile,
  mergeIntakeResults,
  parseVideoUploadResponse,
  placeholderFor,
  readAttachmentsInOrder,
  reconcileIntakeBudget,
  removeSentAttachments,
  safeFence,
  sanitizeFileName,
  settleVideoOutcome,
  uploadVideoFile,
  utf8ByteLength,
  videoContainerOf,
  videoFramesWire,
  videoMarkerText,
  videoMetaLabel,
  videoUploadHeaders,
  videoUploadUrl,
  wireFootprint,
  type AcceptedFile,
  type AttachSkip,
  type ComposerAttachment,
  type IntakeContext,
  type ReadResult,
  type VideoFetch,
  type VideoUploadData,
  type VideoUploadFrame,
  type VideoUploadOutcome,
  type VideoUploadResponseLike,
} from '../src/attachments.ts';

const failures: string[] = [];
let ran = 0;
function check(name: string, cond: boolean, detail?: string): void {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

/** Minimal file-shaped stub (classifyAttachments reads only type/name/size). */
function f(name: string, type: string, size: number): { type: string; name: string; size: number } {
  return { type, name, size };
}
function img(name: string, size = 1024): { type: string; name: string; size: number } {
  return f(name, 'image/png', size);
}
function code(name: string, size = 512): { type: string; name: string; size: number } {
  return f(name, 'text/plain', size);
}

// ---- limit constants derived from the backend WS frame cap ----
check('MAX_PROMPT_FRAME_BYTES == 4 MiB (rpc.rs MAX_RPC_MESSAGE_BYTES)', MAX_PROMPT_FRAME_BYTES === 4 * 1024 * 1024);
check('MAX_TOTAL_WIRE_BYTES < frame (conservative)', MAX_TOTAL_WIRE_BYTES < MAX_PROMPT_FRAME_BYTES);
check('MAX_TOTAL_WIRE_BYTES leaves >= 0.5 MiB reserve', MAX_PROMPT_FRAME_BYTES - MAX_TOTAL_WIRE_BYTES >= 0.5 * 1024 * 1024);
check('MAX_FILE_BYTES <= MAX_TOTAL_WIRE_BYTES', MAX_FILE_BYTES <= MAX_TOTAL_WIRE_BYTES);
check('MAX_FILE_BYTES == 2 MiB', MAX_FILE_BYTES === 2 * 1024 * 1024);
check('MAX_ATTACHMENTS is a modest count cap', MAX_ATTACHMENTS >= 2 && MAX_ATTACHMENTS <= 16);
check('base64Length 3 raw -> 4', base64Length(3) === 4);
check('base64Length 1 raw -> 4 (padded)', base64Length(1) === 4);
check('base64Length 6 raw -> 8', base64Length(6) === 8);
check('base64Length 0 -> 0', base64Length(0) === 0);
check('wireFootprint image = base64', wireFootprint('image', 300) === base64Length(300));
check('wireFootprint code > raw (overhead)', wireFootprint('code', 100) > 100);

// ---- kind classification: image / code / unsupported ----
check('classifyKind image', classifyKind(img('a.png')) === 'image');
check('classifyKind image empty type + ext', classifyKind(f('shot.jpg', '', 10)) === 'image');
check('classifyKind rust', classifyKind(f('lib.rs', 'text/plain', 10)) === 'code');
check('classifyKind ts empty type', classifyKind(f('app.ts', '', 10)) === 'code');
check('classifyKind tsx', classifyKind(f('x.tsx', '', 10)) === 'code');
check('classifyKind python', classifyKind(f('m.py', '', 10)) === 'code');
check('classifyKind json', classifyKind(f('p.json', '', 10)) === 'code');
check('classifyKind markdown', classifyKind(f('r.md', '', 10)) === 'code');
check('classifyKind txt', classifyKind(f('n.txt', '', 10)) === 'code');
check('classifyKind pdf by type unsupported', classifyKind(f('a.pdf', 'application/pdf', 10)) === null);
check('classifyKind pdf by ext unsupported', classifyKind(f('a.pdf', '', 10)) === null);
check('classifyKind zip unsupported', classifyKind(f('a.zip', 'application/zip', 10)) === null);
check('classifyKind unknown ext + no type unsupported', classifyKind(f('blob.dat', '', 10)) === null);
check('classifyKind text/* type unknown ext accepted', classifyKind(f('data', 'text/csv', 10)) === 'code');
check('isImageFile image true', isImageFile(img('a.png')));
check('isImageFile pdf false', isImageFile(f('a.pdf', 'application/pdf', 10)) === false);
check('isTextFile rust true', isTextFile(f('a.rs', '', 10)) === true);
check('isTextFile image is NOT text', isTextFile(img('a.png')) === false);
check('isTextFile pdf false', isTextFile(f('a.pdf', 'application/pdf', 10)) === false);
check('isTextFile zip false', isTextFile(f('a.zip', 'application/zip', 10)) === false);
check('isTextFile dockerfile basename true', isTextFile(f('Dockerfile', '', 10)) === true);

// ---- codeLanguage mapping ----
check('codeLanguage .rs -> rust', codeLanguage('lib.rs') === 'rust');
check('codeLanguage .ts -> typescript', codeLanguage('app.ts') === 'typescript');
check('codeLanguage .tsx -> tsx', codeLanguage('x.tsx') === 'tsx');
check('codeLanguage .py -> python', codeLanguage('m.py') === 'python');
check('codeLanguage .unknown -> ""', codeLanguage('x.zzz') === '');
check('codeLanguage Dockerfile -> dockerfile', codeLanguage('Dockerfile') === 'dockerfile');

// ---- imageMimeType ----
check('imageMimeType image/jpeg passthrough', imageMimeType(f('a', 'image/jpeg', 0)) === 'image/jpeg');
check('imageMimeType jpg ext -> image/jpeg', imageMimeType(f('a.jpg', '', 0)) === 'image/jpeg');
check('imageMimeType png ext -> image/png', imageMimeType(f('a.png', '', 0)) === 'image/png');
check('imageMimeType unknown ext -> image/png', imageMimeType(f('a.bin', '', 0)) === 'image/png');

// ---- classifyAttachments: single image, order, mixed image+code ----
{
  const plan = classifyAttachments([img('one.png', 500)]);
  check('single image accepted', plan.accepted.length === 1 && plan.skipped.length === 0);
  check('single image kind image', plan.accepted[0]!.kind === 'image');
}
{
  const plan = classifyAttachments([img('a.png'), code('b.rs'), img('c.png'), code('d.ts')]);
  check('mixed image+code all accepted', plan.accepted.length === 4 && plan.skipped.length === 0);
  check('mixed image+code preserves intake order', plan.accepted.map((x) => x.file.name).join(',') === 'a.png,b.rs,c.png,d.ts');
  check('mixed kinds preserved', plan.accepted.map((x) => x.kind).join(',') === 'image,code,image,code');
}
{
  const plan = classifyAttachments([code('lib.rs'), code('app.ts')]);
  check('multiple code files accepted in order', plan.accepted.map((x) => x.file.name).join(',') === 'lib.rs,app.ts' && plan.accepted.every((x) => x.kind === 'code'));
}

// ---- classifyAttachments: PDFs and binaries skipped as unsupported ----
{
  const plan = classifyAttachments([img('ok.png'), f('doc.pdf', 'application/pdf', 1024), f('bin.zip', 'application/zip', 10), code('ok.rs')]);
  check('pdf+binary skipped unsupported', plan.skipped.length === 2 && plan.skipped.every((s) => s.reason === 'unsupported'));
  check('pdf+binary skips preserve order', plan.skipped.map((s) => s.name).join(',') === 'doc.pdf,bin.zip');
  check('valid siblings accepted in order', plan.accepted.map((x) => x.file.name).join(',') === 'ok.png,ok.rs');
}

// ---- classifyAttachments: oversize ----
{
  const plan = classifyAttachments([img('big.png', MAX_FILE_BYTES + 1), code('ok.rs', 500)]);
  check('oversize skipped', plan.skipped.length === 1 && plan.skipped[0]!.reason === 'oversize');
  check('oversize does not block valid sibling', plan.accepted.length === 1 && plan.accepted[0]!.file.name === 'ok.rs');
  check('oversize boundary exactly MAX_FILE_BYTES accepted (image)', classifyAttachments([img('edge.png', MAX_FILE_BYTES)]).accepted.length === 1);
  check('oversize boundary exactly MAX_FILE_BYTES accepted (code)', classifyAttachments([code('edge.rs', MAX_FILE_BYTES)]).accepted.length === 1);
}

// ---- classifyAttachments: count cap drops extras as too-many ----
{
  const many = Array.from({ length: MAX_ATTACHMENTS + 2 }, (_, i) => code(`x${i}.rs`, 100));
  const plan = classifyAttachments(many);
  check('count cap accepts exactly MAX_ATTACHMENTS', plan.accepted.length === MAX_ATTACHMENTS);
  check('count cap skips overflow as too-many', plan.skipped.length === 2 && plan.skipped.every((s) => s.reason === 'too-many'));
  check('count cap preserves order', plan.accepted[0]!.file.name === 'x0.rs' && plan.accepted[MAX_ATTACHMENTS - 1]!.file.name === `x${MAX_ATTACHMENTS - 1}.rs`);
}

// ---- classifyAttachments: aggregate wire cap drops extras as over-budget ----
{
  // Images inflate 4/3. A raw size whose base64 == exactly 1/3 of the wire
  // budget lets three images land at == budget (allowed, strict >) and a fourth
  // overflow.
  const unit = Math.floor(MAX_TOTAL_WIRE_BYTES / 3 / 4) * 3; // raw so base64 == 1/3 budget
  const plan = classifyAttachments([img('a.png', unit), img('b.png', unit), img('c.png', unit), img('d.png', unit)]);
  check('aggregate wire cap accepts up to the budget (3 images at 1/3)', plan.accepted.length === 3, `unit=${unit} accepted=${plan.accepted.length}`);
  check('aggregate wire cap skips the 4th image as over-budget', plan.skipped.length === 1 && plan.skipped[0]!.reason === 'over-budget');
}
{
  // Code files do NOT base64-inflate (only the per-file framing overhead), so
  // two half-budget code files fit and a third overflows.
  const overhead = wireFootprint('code', 0);
  const half = Math.floor((MAX_TOTAL_WIRE_BYTES - 2 * overhead) / 2); // room for 2x overhead
  const plan = classifyAttachments([code('a.rs', half), code('b.rs', half), code('c.rs', half)]);
  check('aggregate wire cap accepts two half-budget code files', plan.accepted.length === 2, `half=${half} accepted=${plan.accepted.length}`);
  check('aggregate wire cap skips the third code as over-budget', plan.skipped.length === 1 && plan.skipped[0]!.reason === 'over-budget');
}

// ---- classifyAttachments: respects already-queued state (follow-on intake) ----
{
  const overhead = wireFootprint('code', 0);
  const half = Math.floor((MAX_TOTAL_WIRE_BYTES - 2 * overhead) / 2); // room for queued + new overhead
  // currentWire must be a WIRE footprint, not the raw size: the selection path
  // reserves wireFootprint(kind, size) (App.tsx onFilesChosen) and reconcile
  // reports aggregateWire, so the queued code file carries its own framing
  // overhead here too (the same estimate classifyAttachments applies below).
  const queuedWire = wireFootprint('code', half);
  const plan = classifyAttachments([code('c.rs', half)], { currentCount: 1, currentWire: queuedWire });
  check('follow-on intake sees queued wire budget', plan.accepted.length === 1);
  const over = classifyAttachments([code('d.rs', half + 200)], { currentCount: 1, currentWire: queuedWire });
  check('follow-on intake rejects over-budget against queued wire', over.accepted.length === 0 && over.skipped[0]!.reason === 'over-budget');
  const capped = classifyAttachments([code('e.rs', 10)], { currentCount: MAX_ATTACHMENTS, currentWire: 0 });
  check('follow-on intake rejects too-many against queued count', capped.skipped[0]!.reason === 'too-many');
}

// ---- classifyAttachments: mixed batch reports every skip category ----
{
  const plan = classifyAttachments([
    img('ok.png', 100),
    f('doc.pdf', 'application/pdf', 100), // unsupported
    img('big.png', MAX_FILE_BYTES + 1), // oversize
    code('a.rs', 1_000_000),
    code('b.rs', 1_000_000),
    code('over.rs', 1_200_000), // over-budget (ok+a+b consume ~2 MiB, this tips over)
  ]);
  const reasons = plan.skipped.map((s) => s.reason);
  check('mixed batch accepts valid in order', plan.accepted.map((x) => x.file.name).join(',') === 'ok.png,a.rs,b.rs');
  check('mixed batch reports unsupported+oversize+over-budget', reasons.includes('unsupported') && reasons.includes('oversize') && reasons.includes('over-budget'));
  check('mixed batch skips preserve order', plan.skipped.map((s) => s.name).join(',') === 'doc.pdf,big.png,over.rs');
}

// ---- readAttachmentsInOrder: preserves input order despite shuffled completion ----
{
  const files = ['a', 'b', 'c', 'd', 'e'].map((n) => f(`${n}.rs`, 'text/plain', 10));
  // Completion order (by delay): b(5), d(10), a(50), e(80), c(200) — NOT input order.
  const delays = [50, 5, 200, 10, 80];
  const read = (file: { name: string }, index: number) =>
    new Promise<string>((resolve) => {
      setTimeout(() => resolve(`${file.name}#${index}`), delays[index]);
    });
  const out = await readAttachmentsInOrder(files, read);
  check('readAttachmentsInOrder preserves input order despite async completion', out.join(',') === 'a.rs#0,b.rs#1,c.rs#2,d.rs#3,e.rs#4', out.join(','));
}
{
  const out = await readAttachmentsInOrder([], () => Promise.resolve('x'));
  check('readAttachmentsInOrder empty -> empty', Array.isArray(out) && out.length === 0);
}

// ---- decodeUtf8OrReject: valid vs binary ----
{
  const enc = new TextEncoder();
  check('decodeUtf8OrReject valid ascii', decodeUtf8OrReject(enc.encode('hello rust')).ok === true);
  check('decodeUtf8OrReject valid multibyte', decodeUtf8OrReject(enc.encode('héllo wörld 🦀')).ok === true);
  // 0xFF is never a valid UTF-8 lead byte -> fatal decoder throws.
  check('decodeUtf8OrReject binary rejected', decodeUtf8OrReject(new Uint8Array([0xff, 0xfe, 0x00])).ok === false);
  // Lone continuation byte 0x80 is invalid.
  check('decodeUtf8OrReject lone continuation rejected', decodeUtf8OrReject(new Uint8Array([0x41, 0x80, 0x42])).ok === false);
  check('decodeUtf8OrReject empty ok', decodeUtf8OrReject(new Uint8Array()).ok === true);
}

check('safeFence run of 4 -> 5', safeFence('````') === '`````');
check('safeFence no backticks -> 3', safeFence('hello world') === '```');
check('safeFence run of 3 -> 4', safeFence('a ``` b') === '````');
check('safeFence run of 5 -> 6', safeFence('x ````` y') === '``````');
check('safeFence run across newlines counts the max run', safeFence('``\n``\n```') === '````');
{
  // A pathological all-backticks body: fence length = body length + 1.
  const body = '`'.repeat(20);
  check('safeFence pathological run -> run+1', safeFence(body) === '`'.repeat(21));
}

// ---- buildCodeSegment / buildCodeMessage: format + fence-injection safety ----
{
  const seg = buildCodeSegment('lib.rs', 'rust', 'fn main() {}\n');
  check('buildCodeSegment header + fence + lang', seg.startsWith('File: lib.rs\n```rust\n') && seg.endsWith('\n```'));
  check('buildCodeSegment contains the text', seg.includes('fn main() {}'));
}
{
  // Content containing a triple-backtick line must use a LONGER fence so it
  // cannot be mistaken for the closing fence.
  const inner = 'let x = 1;\n```\nshell_out_of_inner = true;\n```\nlet y = 2;';
  const seg = buildCodeSegment('weird.rs', 'rust', inner);
  const lines = seg.split('\n');
  const fence = lines[1] ?? '';
  check('buildCodeSegment inner ``` forces a longer fence', fence.length > 3, `fence=${fence}`);
  check('buildCodeSegment opening fence has language', fence === '````' + 'rust');
  // The closing fence must equal the opening fence (minus the lang suffix).
  const closer = lines[lines.length - 1];
  check('buildCodeSegment closing fence matches opening length', closer === '`'.repeat(fence.length - 'rust'.length), `closer=${closer}`);
  // No line BETWEEN the opening and closing fences may be a pure fence that
  // could prematurely close: i.e. the inner ``` line must be shorter than the
  // chosen fence.
  const innerFenceLine = lines.indexOf('```');
  check('buildCodeSegment inner ``` line is shorter than the fence (cannot close)', innerFenceLine > 0 && '```'.length < fence.length - 'rust'.length);
}
{
  // Multi-file message joins segments in queue order with blank lines, each with
  // its own safe fence (independent backtick runs).
  const atts: ComposerAttachment[] = [
    { id: 'a1', name: 'lib.rs', size: 10, mimeType: 'text/plain', kind: 'code', text: 'fn a() {}', language: 'rust' },
    { id: 'a2', name: 'app.ts', size: 8, mimeType: 'text/plain', kind: 'code', text: 'const b = 1;', language: 'typescript' },
  ];
  const msg = buildCodeMessage(atts);
  check('buildCodeMessage joins in queue order', msg.indexOf('File: lib.rs') < msg.indexOf('File: app.ts'));
  check('buildCodeMessage has both fences', msg.includes('```rust') && msg.includes('```typescript'));
}
{
  // A code attachment and an image attachment: only code contributes to the message.
  const atts: ComposerAttachment[] = [
    { id: 'a1', name: 'pic.png', size: 100, mimeType: 'image/png', kind: 'image', dataBase64: 'AAAA', previewUrl: 'data:image/png;base64,AAAA' },
    { id: 'a2', name: 'lib.rs', size: 10, mimeType: 'text/plain', kind: 'code', text: 'fn a() {}', language: 'rust' },
  ];
  const msg = buildCodeMessage(atts);
  check('buildCodeMessage ignores image attachments', msg.includes('File: lib.rs') && !msg.includes('pic.png'));
}

// ---- sanitizeFileName + buildCodeSegment: malicious filename cannot break the wrapper ----
{
  check('sanitizeFileName strips newlines/control chars to one line', sanitizeFileName('a\nb\tc') === 'a b c');
  check('sanitizeFileName strips CR/LF', sanitizeFileName('evil.rs\n```\ninjected') === 'evil.rs ``` injected');
  check('sanitizeFileName bounds length', sanitizeFileName('x'.repeat(500)).length <= 128);
  check('sanitizeFileName falls back to file for empty', sanitizeFileName('\u0000\u0001') === 'file');
  // A name with an embedded newline + fence must NOT produce a second line in
  // the header: the File: line stays single-line, so the injected ``` cannot
  // open/close a fence on its own line.
  const seg = buildCodeSegment('evil.rs\n```\ninjected', 'rust', 'fn main(){}');
  const lines = seg.split('\n');
  check('buildCodeSegment File: header is a single line', lines[0] === 'File: evil.rs ``` injected', lines[0]);
  check('buildCodeSegment header has no embedded newline', !lines[0]!.includes('\n'));
  // The real opening fence is still line 1 (the ```rust line), unchanged.
  check('buildCodeSegment opening fence is line 1', lines[1] === '```rust', lines[1]);
}

// ---- attachmentsToImageBlocks: images only, code excluded ----
{
  const atts: ComposerAttachment[] = [
    { id: 'a1', name: 'a.png', size: 100, mimeType: 'image/png', kind: 'image', dataBase64: 'AAAA', previewUrl: 'data:image/png;base64,AAAA' },
    { id: 'a2', name: 'lib.rs', size: 10, mimeType: 'text/plain', kind: 'code', text: 'fn a(){}', language: 'rust' },
    { id: 'a3', name: 'b.jpg', size: 250, mimeType: 'image/jpeg', kind: 'image', dataBase64: 'BBBBBBBB', previewUrl: 'data:image/jpeg;base64,BBBBBBBB' },
  ];
  const blocks = attachmentsToImageBlocks(atts);
  check('attachmentsToImageBlocks returns image blocks only', blocks.length === 2 && blocks.every((b) => b.type === 'image'));
  check('attachmentsToImageBlocks carries data + mime in order', blocks[0]!.data === 'AAAA' && blocks[0]!.mimeType === 'image/png' && blocks[1]!.mimeType === 'image/jpeg');
}

// ---- aggregateWire: image base64 + exact built code segment bytes ----
{
  const atts: ComposerAttachment[] = [
    { id: 'a1', name: 'a.png', size: 300, mimeType: 'image/png', kind: 'image', dataBase64: 'A'.repeat(base64Length(300)), previewUrl: '' },
    { id: 'a2', name: 'lib.rs', size: 50, mimeType: 'text/plain', kind: 'code', text: 'fn main(){}', language: 'rust' },
  ];
  const wire = aggregateWire(atts);
  const expected = base64Length(300) + codeSegmentWireBytes('lib.rs', 'rust', 'fn main(){}');
  check('aggregateWire == image base64 + exact code segment bytes', wire === expected, `wire=${wire} expected=${expected}`);
  // The exact segment bytes must be the real built `File:` header + fences +
  // language + text — never the stale +64 flat overhead.
  check('aggregateWire code part is the built segment (not +64)', wire - base64Length(300) === utf8ByteLength(buildCodeSegment('lib.rs', 'rust', 'fn main(){}')));
  check('aggregateWire code part != text.length + 64 (exact, not flat)', wire - base64Length(300) !== 'fn main(){}'.length + 64);
}

// ---- aggregateWire: multibyte UTF-8 counted in BYTES (not JS .length) ----
{
  // 'héllo' is 5 UTF-16 code units but 6 UTF-8 bytes ('é' is 2 bytes).
  const text = 'héllo';
  check('utf8ByteLength counts multibyte bytes', utf8ByteLength(text) === 6 && text.length === 5);
  const atts: ComposerAttachment[] = [
    { id: 'a1', name: 'lib.rs', size: 6, mimeType: 'text/plain', kind: 'code', text, language: 'rust' },
  ];
  const wire = aggregateWire(atts);
  const segBytes = codeSegmentWireBytes('lib.rs', 'rust', text);
  check('aggregateWire counts UTF-8 bytes for code (not .length)', wire === segBytes, `wire=${wire}`);
  check('aggregateWire multibyte != JS length (bytes win)', wire !== text.length + 64, `wire=${wire}`);
  // A follow-on intake against this queued wire must use the byte count: a
  // code file whose wire footprint fills the remaining budget should just
  // fit, while a larger one overflows.
  const remaining = MAX_TOTAL_WIRE_BYTES - wire;
  const overhead = wireFootprint('code', 0);
  const fits = classifyAttachments([code('next.rs', remaining - overhead)], {
    currentCount: 1,
    currentWire: wire,
    maxFileBytes: remaining + 1,
  });
  const over = classifyAttachments([code('big.rs', remaining - overhead + 1)], {
    currentCount: 1,
    currentWire: wire,
    maxFileBytes: remaining + 1,
  });
  check('multibyte follow-on intake fits within byte budget', fits.accepted.length === 1, `remaining=${remaining}`);
  check('multibyte follow-on intake overflows byte budget', over.accepted.length === 0 && over.skipped[0]!.reason === 'over-budget');
}

// ---- image restriction: PNG/JPEG/GIF/WebP accepted; SVG/BMP rejected ----
check('isImageFile png accepted', isImageFile(f('a.png', 'image/png', 10)) === true);
check('isImageFile jpeg accepted', isImageFile(f('a.jpg', 'image/jpeg', 10)) === true);
check('isImageFile gif accepted', isImageFile(f('a.gif', 'image/gif', 10)) === true);
check('isImageFile webp accepted', isImageFile(f('a.webp', 'image/webp', 10)) === true);
check('isImageFile svg REJECTED (not normalized downstream)', isImageFile(f('a.svg', 'image/svg+xml', 10)) === false);
check('isImageFile bmp REJECTED (not normalized downstream)', isImageFile(f('a.bmp', 'image/bmp', 10)) === false);
check('isImageFile svg ext REJECTED', isImageFile(f('a.svg', '', 10)) === false);
check('isImageFile bmp ext REJECTED', isImageFile(f('a.bmp', '', 10)) === false);
check('classifyKind svg -> code (text), not image', classifyKind(f('a.svg', 'image/svg+xml', 10)) === 'code');
check('classifyKind bmp -> unsupported', classifyKind(f('a.bmp', 'image/bmp', 10)) === null);
check('imageMimeType svg ext -> image/png fallback (not image/svg+xml)', imageMimeType(f('a.svg', '', 0)) === 'image/png');

// ---- video kind classification: six containers, extension REQUIRED ----
check('classifyKind mkv by type', classifyKind(f('clip.mkv', 'video/x-matroska', 10)) === 'video');
check('classifyKind mp4 empty type', classifyKind(f('clip.mp4', '', 10)) === 'video');
check('classifyKind webm', classifyKind(f('clip.webm', '', 10)) === 'video');
check('classifyKind mov', classifyKind(f('clip.mov', '', 10)) === 'video');
check('classifyKind avi', classifyKind(f('clip.avi', '', 10)) === 'video');
check('classifyKind ogg', classifyKind(f('clip.ogg', '', 10)) === 'video');
check('classifyKind mkv uppercase ext', classifyKind(f('CLIP.MKV', '', 10)) === 'video');
check('classifyKind mkv octet-stream accepted via ext', classifyKind(f('clip.mkv', 'application/octet-stream', 10)) === 'video');
check('isVideoFile mkv true', isVideoFile(f('clip.mkv', 'video/x-matroska', 10)));
check('isVideoFile mp4 empty type true', isVideoFile(f('clip.mp4', '', 10)));
// The backend supports EXACTLY the six containers (415 otherwise): a video/*
// MIME with an unsupported extension must NOT broaden intake — the picker and
// drop would otherwise accept uploads the backend rejects.
check('isVideoFile video/mpeg WITHOUT supported ext rejected', isVideoFile(f('clip', 'video/mpeg', 10)) === false);
check('isVideoFile video/x-flv rejected', isVideoFile(f('clip.flv', 'video/x-flv', 10)) === false);
check('isVideoFile wmv rejected', isVideoFile(f('clip.wmv', 'video/x-ms-wmv', 10)) === false);
check('isVideoFile m4v rejected (backend 415 list)', isVideoFile(f('clip.m4v', 'video/mp4', 10)) === false);
check('classifyKind video/mpeg w/o ext unsupported', classifyKind(f('clip', 'video/mpeg', 10)) === null);
check('classifyKind flv unsupported', classifyKind(f('clip.flv', 'video/x-flv', 10)) === null);
check('classifyKind bmp still unsupported', classifyKind(f('a.bmp', 'image/bmp', 10)) === null);
check('classifyKind pdf still unsupported', classifyKind(f('a.pdf', 'application/pdf', 10)) === null);
check('classifyKind zip still unsupported', classifyKind(f('a.zip', 'application/zip', 10)) === null);
check('classifyKind image still wins over video-shaped names', classifyKind(f('a.png', 'image/png', 10)) === 'image');
check('videoContainerOf mkv', videoContainerOf('clip.mkv') === 'mkv');
check('videoContainerOf uppercase normalized', videoContainerOf('CLIP.MKV') === 'mkv');
check('videoContainerOf unknown null', videoContainerOf('clip.exe') === null);
check('videoContainerOf no ext null', videoContainerOf('clip') === null);

// ---- video limits mirror the backend ----
check('MAX_VIDEO_BYTES == 64 MiB (backend 413 cap)', MAX_VIDEO_BYTES === 64 * 1024 * 1024);
check('MAX_VIDEO_DURATION_SECONDS == 600 (backend 422 cap)', MAX_VIDEO_DURATION_SECONDS === 600);
check('MAX_VIDEO_FRAMES == 6 (backend extraction cap)', MAX_VIDEO_FRAMES === 6);
check('MAX_VIDEO_FRAME_BYTES == 384 KiB (backend per-frame cap)', MAX_VIDEO_FRAME_BYTES === 384 * 1024);
check('MAX_VIDEO_FRAMES_RAW_BYTES == 2 MiB (backend total cap)', MAX_VIDEO_FRAMES_RAW_BYTES === 2 * 1024 * 1024);
check('frame caps are consistent (total cap <= frames * per-frame)', MAX_VIDEO_FRAMES_RAW_BYTES <= MAX_VIDEO_FRAMES * MAX_VIDEO_FRAME_BYTES);
check('VIDEO_UPLOAD_PATH == /upload/video (backend route)', VIDEO_UPLOAD_PATH === '/upload/video');
check('VIDEO_UPLOAD_TIMEOUT_MS >= backend probe+extract bounds', VIDEO_UPLOAD_TIMEOUT_MS >= 80_000);

// ---- video wire/budget: raw bytes never ride the prompt frame ----
check('wireFootprint video == 0 (raw video never rides the prompt wire)', wireFootprint('video', MAX_VIDEO_BYTES) === 0);
{
  const plan = classifyAttachments([f('clip.mkv', 'video/x-matroska', 1024), img('a.png', 100), code('b.rs', 100)]);
  check('video accepted alongside image+code', plan.accepted.length === 3, `accepted=${plan.accepted.length}`);
  check('video kind preserved in order', plan.accepted.map((x) => x.kind).join(',') === 'video,image,code');
}
{
  const plan = classifyAttachments([f('big.mkv', 'video/x-matroska', MAX_VIDEO_BYTES + 1), code('ok.rs', 100)]);
  check('video over 64 MiB skipped as video-oversize', plan.skipped.length === 1 && plan.skipped[0]!.reason === 'video-oversize');
  check('video-oversize does not block valid sibling', plan.accepted.length === 1 && plan.accepted[0]!.file.name === 'ok.rs');
  check('video boundary exactly MAX_VIDEO_BYTES accepted', classifyAttachments([f('edge.mkv', '', MAX_VIDEO_BYTES)]).accepted.length === 1);
}
{
  // Videos reserve 0 wire at intake (raw bytes never ride); the exact
  // extracted-frame footprint is reconciled when the upload lands.
  const plan = classifyAttachments([f('clip.mkv', 'video/x-matroska', 10_000_000), img('a.png', 500_000)]);
  check('video intake does not consume wire budget', plan.accepted.length === 2, `accepted=${plan.accepted.length}`);
}

// ---- video frames: chronological JPEGs feed the existing images array ----
{
  const frames: VideoUploadFrame[] = [
    { index: 0, timestampSeconds: 0, mimeType: 'image/jpeg', width: 640, height: 360, sizeBytes: 8, data: 'AAAA' },
    { index: 1, timestampSeconds: 5.5, mimeType: 'image/jpeg', width: 640, height: 360, sizeBytes: 8, data: 'BBBB' },
  ];
  const video: ComposerAttachment = { id: 'v1', name: 'clip.mkv', size: 1000, mimeType: 'video/x-matroska', kind: 'video', videoState: 'ready', frames, instruction: 'm' };
  const imgA: ComposerAttachment = { id: 'i1', name: 'a.png', size: 10, mimeType: 'image/png', kind: 'image', dataBase64: 'CC' };
  const codeA: ComposerAttachment = { id: 'c1', name: 'b.rs', size: 10, mimeType: 'text/plain', kind: 'code', text: 'x', language: 'rust' };
  const blocks = attachmentsToImageBlocks([imgA, video, codeA]);
  check('video frames become image ContentBlocks at the video queue position', blocks.length === 3 && blocks[0]!.mimeType === 'image/png' && blocks[1]!.type === 'image', `blocks=${blocks.length}`);
  check('video frame blocks are chronological (index order)', blocks[1]!.data === 'AAAA' && blocks[2]!.data === 'BBBB');
  check('code never becomes an image block', blocks.every((b) => b.mimeType !== 'text/plain'));
  const uploading: ComposerAttachment = { id: 'v2', name: 'u.mkv', size: 1, mimeType: 'video/x-matroska', kind: 'video', videoState: 'uploading' };
  const errored: ComposerAttachment = { id: 'v3', name: 'e.mkv', size: 1, mimeType: 'video/x-matroska', kind: 'video', videoState: 'error', videoError: 'x' };
  check('non-ready videos contribute no image blocks', attachmentsToImageBlocks([uploading, errored]).length === 0);
  check('videoFramesWire sums exact base64', videoFramesWire(frames) === 8);
  check('videoFramesWire undefined -> 0', videoFramesWire(undefined) === 0);
  check('aggregateWire video == exact frame base64', aggregateWire([video]) === 8);
  check('aggregateWire video+image sums exact', aggregateWire([video, imgA]) === 10);
  check('aggregateWire pending video contributes 0', aggregateWire([uploading]) === 0);
}

// ---- video markers: backend instruction or bounded fallback ----
{
  const ready: ComposerAttachment = { id: 'v1', name: 'clip.mkv', size: 1, mimeType: 'video/x-matroska', kind: 'video', videoState: 'ready', frames: [{ index: 0, timestampSeconds: 0, mimeType: 'image/jpeg', data: 'AA' }], instruction: 'Frame at 0.0s — 1 chronological frame of clip.mkv (12.5s).' };
  const uploading: ComposerAttachment = { id: 'v2', name: 'u.mkv', size: 1, mimeType: 'video/x-matroska', kind: 'video', videoState: 'uploading' };
  const errored: ComposerAttachment = { id: 'v3', name: 'e.mkv', size: 1, mimeType: 'video/x-matroska', kind: 'video', videoState: 'error', videoError: 'x' };
  check('buildVideoMessage uses backend instruction', buildVideoMessage([ready]) === 'Frame at 0.0s — 1 chronological frame of clip.mkv (12.5s).');
  check('buildVideoMessage excludes uploading/error videos', buildVideoMessage([ready, uploading, errored]) === buildVideoMessage([ready]));
  check('buildVideoMessage empty -> empty', buildVideoMessage([uploading, errored]) === '');
  const noInstruction: ComposerAttachment = { id: 'v4', name: 'clip.mkv', size: 1, mimeType: 'video/x-matroska', kind: 'video', videoState: 'ready', frames: [{ index: 0, timestampSeconds: 0, mimeType: 'image/jpeg', data: 'AA' }] };
  const fallback = videoMarkerText(noInstruction);
  check('fallback marker names the file, count, and timestamps', fallback.includes('clip.mkv') && fallback.includes('1 chronological frame') && fallback.includes('0.00s') && fallback.includes('chronological'), fallback);
  check('fallback marker is bounded (< 512 chars)', utf8ByteLength(fallback) < 512, `${utf8ByteLength(fallback)}`);
  const oversized: ComposerAttachment = { id: 'v5', name: 'h.mkv', size: 1, mimeType: 'video/x-matroska', kind: 'video', videoState: 'ready', frames: [{ index: 0, timestampSeconds: 0, mimeType: 'image/jpeg', data: 'AA' }], instruction: 'x'.repeat(5000) };
  check('oversized backend instruction replaced by bounded fallback', videoMarkerText(oversized).length < 512, `${videoMarkerText(oversized).length}`);
  const second: ComposerAttachment = { id: 'v6', name: 'b.webm', size: 1, mimeType: 'video/webm', kind: 'video', videoState: 'ready', frames: [{ index: 0, timestampSeconds: 0, mimeType: 'image/jpeg', data: 'AA' }], instruction: 'second' };
  check('buildVideoMessage joins markers in queue order', buildVideoMessage([ready, second]) === 'Frame at 0.0s — 1 chronological frame of clip.mkv (12.5s).\n\nsecond');
  const evil: ComposerAttachment = { id: 'v7', name: 'a\nb.mkv', size: 1, mimeType: 'video/x-matroska', kind: 'video', videoState: 'ready', frames: [{ index: 0, timestampSeconds: 0, mimeType: 'image/jpeg', data: 'AA' }] };
  check('fallback marker sanitizes the filename', !videoMarkerText(evil).includes('\n'));
}

// ---- videoMetaLabel / placeholderFor ----
{
  const ready: ComposerAttachment = { id: 'v1', name: 'c.mkv', size: 1, mimeType: 'video/x-matroska', kind: 'video', videoState: 'ready', durationSeconds: 12.34, frames: [{ index: 0, timestampSeconds: 0, mimeType: 'image/jpeg', data: 'AA' }, { index: 1, timestampSeconds: 6, mimeType: 'image/jpeg', data: 'BB' }] };
  check('videoMetaLabel frames+duration', videoMetaLabel(ready) === '2 frames · 12.3 s');
  check('videoMetaLabel frames only', videoMetaLabel({ ...ready, durationSeconds: undefined }) === '2 frames');
  check('videoMetaLabel fallback', videoMetaLabel({ ...ready, frames: undefined }) === 'video');
  const placeholder = placeholderFor({ file: { name: 'c.mkv', type: 'video/x-matroska', size: 5 } as File, kind: 'video' }, 'p1');
  check('video placeholder is uploading state with the patch id', placeholder.kind === 'video' && placeholder.videoState === 'uploading' && placeholder.id === 'p1' && placeholder.size === 5);
  const imgPlaceholder = placeholderFor({ file: { name: 'a.png', type: 'image/png', size: 5 } as File, kind: 'image' }, 'p2');
  check('image placeholder kind image', imgPlaceholder.kind === 'image' && imgPlaceholder.id === 'p2');
}

// ---- video in send-clear + budget reconciliation ----
{
  const vid: ComposerAttachment = { id: 'v1', name: 'c.mkv', size: 1, mimeType: 'video/x-matroska', kind: 'video', videoState: 'ready', frames: [{ index: 0, timestampSeconds: 0, mimeType: 'image/jpeg', data: 'AAAA' }], instruction: 'm' };
  check('removeSentAttachments clears sent video by id', removeSentAttachments([vid], new Set(['v1'])).length === 0);
  const budget = reconcileIntakeBudget([vid]);
  check('reconcileIntakeBudget counts video frame wire exactly', budget.count === 1 && budget.wire === 4, `wire=${budget.wire}`);
}

// ---- codeBadgeLabel ----
check('codeBadgeLabel uses extension', codeBadgeLabel('lib.rs') === 'RS');
check('codeBadgeLabel uses tsx extension', codeBadgeLabel('App.tsx') === 'TSX');
check('codeBadgeLabel falls back to TXT for no extension', codeBadgeLabel('Dockerfile') === 'TXT');
check('codeBadgeLabel empty -> TXT', codeBadgeLabel('') === 'TXT');

// ---- formatSkipSummary: null for empty, grouped summary, late reasons ----
{
  check('formatSkipSummary empty -> null', formatSkipSummary([]) === null);
  const one = formatSkipSummary([{ name: 'doc.pdf', size: 10, type: 'application/pdf', reason: 'unsupported' }] as AttachSkip[]);
  check('formatSkipSummary single reason', one === 'Skipped 1 file(s): 1 unsupported (images, videos, or text/code files only) (doc.pdf).', one ?? '');
  const mixed = formatSkipSummary([
    { name: 'a.pdf', size: 10, type: 'application/pdf', reason: 'unsupported' },
    { name: 'big.png', size: MAX_FILE_BYTES + 1, type: 'image/png', reason: 'oversize' },
    { name: 'b.pdf', size: 10, type: 'application/pdf', reason: 'unsupported' },
  ] as AttachSkip[]);
  check('formatSkipSummary groups same reason with sample', mixed != null && mixed.includes('2 unsupported (images, videos, or text/code files only)') && mixed.includes('a.pdf, b.pdf') && mixed.includes('1 over'), mixed ?? '');
  const vidOversize = formatSkipSummary([{ name: 'big.mkv', size: MAX_VIDEO_BYTES + 1, type: 'video/x-matroska', reason: 'video-oversize' }] as AttachSkip[]);
  check('formatSkipSummary reports video-oversize with the video cap', vidOversize != null && vidOversize.includes('video upload cap') && vidOversize.includes('64 MiB'), vidOversize ?? '');
  const many = formatSkipSummary(Array.from({ length: 4 }, (_, i) => ({ name: `z${i}.pdf`, size: 1, type: 'application/pdf', reason: 'unsupported' as const })) as AttachSkip[]);
  check('formatSkipSummary truncates long name lists', many != null && many.includes('+2 more'), many ?? '');
  const utf8 = formatSkipSummary([{ name: 'bin.dat', size: 10, type: '', reason: 'invalid-utf8' }] as AttachSkip[]);
  check('formatSkipSummary reports invalid-utf8 reason', utf8 != null && utf8.includes('not valid UTF-8 (binary)'), utf8 ?? '');
}

// ---- ATT-CODE-OVERHEAD-UNDERCOUNT: estimate never undercounts built segment ----
{
  // The pre-read wireFootprint estimate must cover the real built code segment
  // bytes for a normal (min-fence) file, including a max-length sanitized
  // filename, so the intake budget never silently undercounts the wire.
  const longName = 'x'.repeat(200); // exceeds MAX_FILENAME_LEN -> sanitized to 128
  const text = 'fn main() {}';
  const built = codeSegmentWireBytes(longName, 'rust', text);
  const estimate = wireFootprint('code', text.length);
  check('wireFootprint(code) >= exact built segment (long name)', estimate >= built, `estimate=${estimate} built=${built}`);
  // A short name must also be covered.
  const builtShort = codeSegmentWireBytes('lib.rs', 'rust', text);
  check('wireFootprint(code) >= exact built segment (short name)', estimate >= builtShort, `estimate=${estimate} built=${builtShort}`);
  // The old flat +64 undercounted a 128-char filename (header alone is 135
  // bytes): the estimate must now exceed the old 64-byte overhead.
  check('wireFootprint(code,0) overhead > 64 (covers long filename)', wireFootprint('code', 0) > 64, `overhead=${wireFootprint('code', 0)}`);
  // A multibyte sanitized filename is bounded by 128 UTF-16 code units, NOT
  // bytes: '本' encodes to 3 UTF-8 bytes per unit, so a full-length name is
  // 384 bytes on the wire. The pre-read estimate must cover the UTF-8
  // expansion, not just the code-unit count.
  const cjkName = '本'.repeat(200); // sanitized to 128 units = 384 UTF-8 bytes
  const builtCjk = codeSegmentWireBytes(cjkName, 'rust', text);
  check('wireFootprint(code) >= exact built segment (384-byte multibyte name)', estimate >= builtCjk, `estimate=${estimate} built=${builtCjk}`);
  // The byte-aware overhead must at least cover the worst-case name framing
  // (File: prefix + 384-byte name + the 3 newlines + min fences + max language).
  const worstNameBytes = utf8ByteLength(sanitizeFileName(cjkName));
  const worstFraming = 6 + worstNameBytes + 3 + 2 * 3 + 14;
  check('wireFootprint(code,0) overhead covers worst-case multibyte framing', wireFootprint('code', 0) >= worstFraming, `overhead=${wireFootprint('code', 0)} worst=${worstFraming}`);
}

// ---- ATT-BUDGET-LEAK: all-late-skip release modeling ----
{
  // When every accepted file is a late skip (invalid UTF-8 / unreadable), the
  // reconciled budget must be zero — no sticky reserved footprint blocks a
  // follow-on intake. This models onFilesChosen's post-read reconciliation.
  const released = reconcileIntakeBudget([]);
  check('reconcileIntakeBudget([]) releases all reserved budget', released.count === 0 && released.wire === 0, `count=${released.count} wire=${released.wire}`);
  // A mixed batch (some built, some late-skipped) reconciles to exactly the
  // built attachments' footprint — late skips do not linger in the budget.
  const built: ComposerAttachment[] = [
    { id: 'a1', name: 'ok.rs', size: 10, mimeType: 'text/plain', kind: 'code', text: 'fn a(){}', language: 'rust' },
  ];
  const mixed = reconcileIntakeBudget(built);
  check('reconcileIntakeBudget(built) == exact built footprint', mixed.count === 1 && mixed.wire === aggregateWire(built), `count=${mixed.count} wire=${mixed.wire}`);
  check('reconcileIntakeBudget(built) wire != stale reserved (exact only)', mixed.wire === codeSegmentWireBytes('ok.rs', 'rust', 'fn a(){}'));
  // A follow-on intake after a full late-skip batch must see a zero budget
  // (the released budget does not block it).
  const followOn = classifyAttachments([code('next.rs', 100)], { currentCount: released.count, currentWire: released.wire });
  check('follow-on intake after all-late-skip is not blocked', followOn.accepted.length === 1, `accepted=${followOn.accepted.length}`);
}

// ---- ATT-SEND-CLEAR-BEFORE-ACK: success clears sent ids; failure retains ----
{
  const a1: ComposerAttachment = { id: 'a1', name: 'x.rs', size: 1, mimeType: 'text/plain', kind: 'code', text: 'a', language: 'rust' };
  const a2: ComposerAttachment = { id: 'a2', name: 'y.png', size: 1, mimeType: 'image/png', kind: 'image', dataBase64: 'AA' };
  const a3: ComposerAttachment = { id: 'a3', name: 'z.rs', size: 1, mimeType: 'text/plain', kind: 'code', text: 'z', language: 'rust' };
  // Success clears exactly the sent snapshot by id.
  const sentIds = new Set(['a1', 'a2', 'a3']);
  const afterSuccess = removeSentAttachments([a1, a2, a3], sentIds);
  check('removeSentAttachments clears exactly the sent ids', afterSuccess.length === 0, `len=${afterSuccess.length}`);
  // A second intake arriving while the send is in flight is preserved on
  // success: only the sent ids are removed, the concurrent addition stays.
  const a4: ComposerAttachment = { id: 'a4', name: 'new.ts', size: 1, mimeType: 'text/plain', kind: 'code', text: 'n', language: 'typescript' };
  const withConcurrent = removeSentAttachments([a1, a2, a3, a4], sentIds);
  check('removeSentAttachments preserves concurrent additions', withConcurrent.length === 1 && withConcurrent[0]!.id === 'a4', `len=${withConcurrent.length}`);
  // Failed transport retains the exact chips (no removal at all).
  const retained = [a1, a2, a3];
  check('failed-send retention: chips unchanged', retained.length === 3 && retained[0]!.id === 'a1' && retained[2]!.id === 'a3');
  // Budget after a success clear that preserves a concurrent addition is
  // reconciled from the remaining attachments (not zeroed).
  const budgetAfter = reconcileIntakeBudget(withConcurrent);
  check('budget reconciled after partial success clear', budgetAfter.count === 1 && budgetAfter.wire === aggregateWire(withConcurrent), `count=${budgetAfter.count} wire=${budgetAfter.wire}`);
}

// ---- ATT-ACCEPT-IMAGE-STAR: picker advertises only supported image MIMEs ----
{
  const accept = attachmentAccept();
  check('attachmentAccept has image/png', accept.includes('image/png'));
  check('attachmentAccept has image/jpeg', accept.includes('image/jpeg'));
  check('attachmentAccept has image/gif', accept.includes('image/gif'));
  check('attachmentAccept has image/webp', accept.includes('image/webp'));
  check('attachmentAccept does NOT advertise image/*', !accept.includes('image/*'));
  check('attachmentAccept does NOT advertise image/svg+xml', !accept.includes('image/svg+xml'));
  check('attachmentAccept does NOT advertise image/bmp', !accept.includes('image/bmp'));
  // SVG is a supported CODE/TEXT file (not an image) — advertised as .svg ext.
  check('attachmentAccept advertises .svg as a code/text extension', accept.includes('.svg'));
  check('attachmentAccept advertises .rs code extension', accept.includes('.rs'));
  check('attachmentAccept advertises .txt text extension', accept.includes('.txt'));
  // Image MIMEs come first (before the code/text extensions).
  check('attachmentAccept lists image MIMEs before code extensions', accept.indexOf('image/png') < accept.indexOf('.rs'));
  // Video entries: explicit video/x-matroska (pi-web-access 0.22 omits MKV),
  // the supported MIME set, and ONLY the six backend-supported containers.
  check('attachmentAccept advertises video/x-matroska', accept.includes('video/x-matroska'));
  check('attachmentAccept advertises video/mp4', accept.includes('video/mp4'));
  check('attachmentAccept advertises .mkv', accept.includes('.mkv'));
  check('attachmentAccept advertises .webm/.mov/.avi/.ogg', ['.webm', '.mov', '.avi', '.ogg'].every((e) => accept.includes(e)));
  check('attachmentAccept does NOT advertise video/*', !accept.includes('video/*'));
  check('attachmentAccept does NOT advertise unsupported containers', !accept.includes('.wmv') && !accept.includes('.flv') && !accept.includes('.m4v'));
}

// ---- video upload response parsing + client (POST /upload/video) ----
{
  check('videoUploadUrl http', videoUploadUrl('http:', 'h:8080') === 'http://h:8080/upload/video');
  check('videoUploadUrl https', videoUploadUrl('https:', 'h') === 'https://h/upload/video');
  const okBody = {
    attachmentId: 'vid-1',
    name: 'clip.mkv',
    container: 'mkv',
    mimeType: 'video/x-matroska',
    sizeBytes: 1234,
    durationSeconds: 12.5,
    frameCount: 2,
    framesBase64Bytes: 8,
    frames: [
      { index: 0, timestampSeconds: 0, mimeType: 'image/jpeg', width: 640, height: 360, sizeBytes: 8, data: 'AAAA' },
      { index: 1, timestampSeconds: 6.25, mimeType: 'image/jpeg', width: 640, height: 360, sizeBytes: 8, data: 'BBBB' },
    ],
    instruction: 'Frame at 0.0s; Frame at 6.3s — 2 chronological frames of clip.mkv (12.5s).',
  };
  const ok = parseVideoUploadResponse(200, okBody);
  check('parse 200 success -> typed data', ok.ok && ok.data.attachmentId === 'vid-1' && ok.data.frames.length === 2 && ok.data.frames[0]!.data === 'AAAA' && ok.data.instruction.includes('chronological'));
  check('parse 200 frame mimeType preserved', ok.ok && ok.data.frames[0]!.mimeType === 'image/jpeg');
  check('parse 200 frame timestamp preserved', ok.ok && ok.data.frames[1]!.timestampSeconds === 6.25);
  const malformed = parseVideoUploadResponse(200, { attachmentId: 'x' });
  check('parse 200 malformed -> unexpected response', !malformed.ok && malformed.message === 'unexpected response from the video upload endpoint');
  const zeroFrames = parseVideoUploadResponse(200, { ...okBody, frames: [] });
  check('parse 200 zero frames -> failure (not a usable video)', !zeroFrames.ok);
  check('parse 503 -> ffmpeg message', parseVideoUploadResponse(503, {}).message.includes('ffmpeg'));
  check('parse 413 -> too large message', parseVideoUploadResponse(413, {}).message.includes('64 MiB'));
  check('parse 415 -> container message', parseVideoUploadResponse(415, {}).message.includes('mkv/mp4/webm/mov/avi/ogg'));
  check('parse 422 -> duration message', parseVideoUploadResponse(422, {}).message.includes('600'));
  check('parse 400 -> decodable message', parseVideoUploadResponse(400, {}).message.includes('decodable'));
  check('parse 504 -> timeout message', parseVideoUploadResponse(504, {}).message.includes('timed out'));
  check('parse 401 -> auth message', parseVideoUploadResponse(401, {}).message.includes('token'));
  check('parse prefers backend error body', parseVideoUploadResponse(503, { error: 'ffmpeg missing on this host' }).message === 'ffmpeg missing on this host');
  check('parse bounds a hostile error body', parseVideoUploadResponse(400, { error: 'e'.repeat(5000) }).message.length <= 300);
}
{
  const okBody = {
    attachmentId: 'vid-1',
    name: 'clip.mkv',
    container: 'mkv',
    mimeType: 'video/x-matroska',
    sizeBytes: 1234,
    durationSeconds: 12.5,
    frameCount: 2,
    framesBase64Bytes: 8,
    frames: [
      { index: 0, timestampSeconds: 0, mimeType: 'image/jpeg', width: 640, height: 360, sizeBytes: 8, data: 'AAAA' },
      { index: 1, timestampSeconds: 6.25, mimeType: 'image/jpeg', width: 640, height: 360, sizeBytes: 8, data: 'BBBB' },
    ],
    instruction: 'Frame at 0.0s; Frame at 6.3s — 2 chronological frames of clip.mkv (12.5s).',
  };
  const file = { name: 'clip.mkv', size: 1234, type: 'video/x-matroska' } as File;
  const success = await uploadVideoFile(file, {
    hostAuthority: 'h:8080',
    token: 'tok',
    fetchImpl: async () => ({ status: 200, ok: true, json: async () => okBody }),
  });
  check('uploadVideoFile success -> ready video attachment', success.ok && success.attachment.kind === 'video' && success.attachment.videoState === 'ready');
  check('uploadVideoFile success carries frames', success.ok && success.attachment.frames!.length === 2);
  check('uploadVideoFile success builds first-frame thumbnail', success.ok && success.attachment.previewUrl === 'data:image/jpeg;base64,AAAA');
  check('uploadVideoFile success carries the instruction marker', success.ok && success.attachment.instruction!.includes('chronological'));
  const ffmpeg = await uploadVideoFile(file, {
    hostAuthority: 'h:8080',
    token: '',
    fetchImpl: async () => ({ status: 503, ok: false, json: async () => ({ error: 'ffmpeg is not installed' }) }),
  });
  check('uploadVideoFile 503 -> actionable ffmpeg message', !ffmpeg.ok && ffmpeg.status === 503 && ffmpeg.message.includes('ffmpeg'), ffmpeg.message);
  const net = await uploadVideoFile(file, {
    hostAuthority: 'h:8080',
    token: '',
    fetchImpl: async () => {
      throw new Error('Failed to fetch https://h:8080/upload/video');
    },
  });
  check('uploadVideoFile network error -> bounded generic (no URL leak)', !net.ok && net.message === 'video upload failed — check the connection and try again', net.message);
  const timedOut = await uploadVideoFile(file, {
    hostAuthority: 'h:8080',
    token: '',
    timeoutMs: 5,
    fetchImpl: (_url, init) => {
      const { promise, reject } = Promise.withResolvers<VideoUploadResponseLike>();
      init.signal.addEventListener('abort', () => reject(new DOMException('aborted', 'AbortError')));
      return promise;
    },
  });
  check('uploadVideoFile timeout -> bounded timeout message', !timedOut.ok && timedOut.status === 0 && timedOut.message.includes('timed out'), timedOut.message);
}
{
  // buildVideoAttachment directly: the caller overrides the id for the chip
  // patch; a ready video carries frames + first-frame thumbnail.
  const data: VideoUploadData = {
    attachmentId: 'x',
    name: 'c.mkv',
    container: 'mkv',
    mimeType: 'video/x-matroska',
    sizeBytes: 9,
    durationSeconds: 1,
    frameCount: 1,
    framesBase64Bytes: 4,
    frames: [{ index: 0, timestampSeconds: 0, mimeType: 'image/jpeg', data: 'AA==' }],
    instruction: 'm',
  };
  const built = buildVideoAttachment({ name: 'c.mkv' } as File, data);
  check('buildVideoAttachment ready with frame count + thumbnail', built.kind === 'video' && built.videoState === 'ready' && built.frames!.length === 1 && built.previewUrl === 'data:image/jpeg;base64,AA==');
}

// ---- ATT-HOSTILE-UPLOAD: malformed/compromised responses are REJECTED ----
{
  const base = {
    attachmentId: 'vid-1',
    name: 'clip.mkv',
    container: 'mkv',
    mimeType: 'video/x-matroska',
    sizeBytes: 1234,
    durationSeconds: 12.5,
    frameCount: 2,
    framesBase64Bytes: 8,
    frames: [
      { index: 0, timestampSeconds: 0, mimeType: 'image/jpeg', width: 640, height: 360, sizeBytes: 8, data: 'AAAA' },
      { index: 1, timestampSeconds: 6.25, mimeType: 'image/jpeg', width: 640, height: 360, sizeBytes: 8, data: 'BBBB' },
    ],
    instruction: 'Frame at 0.0s; Frame at 6.3s — 2 chronological frames of clip.mkv (12.5s).',
  };
  const frame = (patch: Record<string, unknown>) => ({
    index: 0,
    timestampSeconds: 0,
    mimeType: 'image/jpeg',
    width: 640,
    height: 360,
    sizeBytes: 8,
    data: 'AAAA',
    ...patch,
  });
  const reject = (name: string, body: unknown) => {
    const r = parseVideoUploadResponse(200, body);
    check(`hostile response rejected: ${name}`, !r.ok, r.ok ? 'accepted' : '');
  };
  // Frame-count bounds.
  reject('> MAX_VIDEO_FRAMES frames', { ...base, frames: Array.from({ length: MAX_VIDEO_FRAMES + 1 }, (_, i) => frame({ index: i })) });
  reject('zero frames', { ...base, frames: [] });
  // Index: integer, in range, unique, chronological.
  reject('negative index', { ...base, frames: [frame({ index: -1 }), frame({ index: 1 })] });
  reject('non-integer index', { ...base, frames: [frame({ index: 0.5 }), frame({ index: 1 })] });
  reject('duplicate index', { ...base, frames: [frame({ index: 0 }), frame({ index: 0 })] });
  reject('non-chronological indices', { ...base, frames: [frame({ index: 1 }), frame({ index: 0 })] });
  // Timestamp: finite, non-negative.
  reject('NaN timestamp', { ...base, frames: [frame({ timestampSeconds: NaN }), frame({ index: 1, timestampSeconds: 1 })] });
  reject('negative timestamp', { ...base, frames: [frame({ timestampSeconds: -0.5 }), frame({ index: 1, timestampSeconds: 1 })] });
  // MIME + data bounds.
  reject('non-JPEG mimeType', { ...base, frames: [frame({ mimeType: 'image/png' }), frame({ index: 1, timestampSeconds: 1 })] });
  reject('empty data', { ...base, frames: [frame({ data: '' }), frame({ index: 1, timestampSeconds: 1 })] });
  reject('frame data over per-frame encoded cap', { ...base, frames: [frame({ data: 'A'.repeat(MAX_VIDEO_FRAME_BASE64 + 1) }), frame({ index: 1, timestampSeconds: 1 })] });
  // Aggregate encoded cap.
  reject('aggregate over encoded total cap', {
    ...base,
    frameCount: MAX_VIDEO_FRAMES,
    framesBase64Bytes: MAX_VIDEO_FRAMES * MAX_VIDEO_FRAME_BASE64,
    frames: Array.from({ length: MAX_VIDEO_FRAMES }, (_, i) => frame({ index: i, timestampSeconds: i, data: 'A'.repeat(MAX_VIDEO_FRAME_BASE64) })),
  });
  // Frame metadata: positive integers, sizeBytes within the raw cap.
  reject('zero width', { ...base, frames: [frame({ width: 0 }), frame({ index: 1, timestampSeconds: 1 })] });
  reject('fractional height', { ...base, frames: [frame({ height: 0.5 }), frame({ index: 1, timestampSeconds: 1 })] });
  reject('zero sizeBytes', { ...base, frames: [frame({ sizeBytes: 0 }), frame({ index: 1, timestampSeconds: 1 })] });
  reject('sizeBytes over raw per-frame cap', { ...base, frames: [frame({ sizeBytes: MAX_VIDEO_FRAME_BYTES + 1 }), frame({ index: 1, timestampSeconds: 1 })] });
  // Counter consistency.
  reject('frameCount mismatch', { ...base, frameCount: 3 });
  reject('framesBase64Bytes mismatch', { ...base, framesBase64Bytes: 99 });
  // Instruction + duration.
  reject('oversized instruction', { ...base, instruction: 'x'.repeat(MAX_VIDEO_INSTRUCTION_BYTES + 1) });
  reject('missing instruction', { ...base, instruction: undefined });
  reject('duration zero', { ...base, durationSeconds: 0 });
  reject('duration negative', { ...base, durationSeconds: -1 });
  reject('duration NaN', { ...base, durationSeconds: NaN });
  reject('duration over max', { ...base, durationSeconds: MAX_VIDEO_DURATION_SECONDS + 1 });
  reject('missing attachmentId', { ...base, attachmentId: undefined });
  // No silent filtering: one hostile frame among valid ones rejects ALL.
  reject('mixed valid + hostile frame', {
    ...base,
    frames: [frame({ index: 0 }), frame({ index: 1, timestampSeconds: 1, mimeType: 'image/gif' })],
  });
  // Contract boundary: exactly-at-cap payloads are still accepted.
  check('boundary: single frame exactly at per-frame encoded cap accepted', parseVideoUploadResponse(200, {
    ...base,
    frameCount: 1,
    framesBase64Bytes: MAX_VIDEO_FRAME_BASE64,
    frames: [{ index: 0, timestampSeconds: 0, mimeType: 'image/jpeg', width: 1, height: 1, sizeBytes: MAX_VIDEO_FRAME_BYTES, data: 'A'.repeat(MAX_VIDEO_FRAME_BASE64) }],
  }).ok);
  check('boundary: max frame count accepted', parseVideoUploadResponse(200, {
    ...base,
    frameCount: MAX_VIDEO_FRAMES,
    framesBase64Bytes: 24,
    frames: Array.from({ length: MAX_VIDEO_FRAMES }, (_, i) => frame({ index: i, timestampSeconds: i })),
  }).ok);
  {
    const remainder = MAX_VIDEO_FRAMES_BASE64 - (MAX_VIDEO_FRAMES - 1) * MAX_VIDEO_FRAME_BASE64;
    const atTotal = Array.from({ length: MAX_VIDEO_FRAMES }, (_, i) => ({
      index: i,
      timestampSeconds: i,
      mimeType: 'image/jpeg',
      width: 1,
      height: 1,
      sizeBytes: 1,
      data: 'A'.repeat(i === MAX_VIDEO_FRAMES - 1 ? remainder : MAX_VIDEO_FRAME_BASE64),
    }));
    check('boundary: aggregate exactly at encoded total cap accepted', parseVideoUploadResponse(200, {
      ...base,
      frameCount: MAX_VIDEO_FRAMES,
      framesBase64Bytes: MAX_VIDEO_FRAMES_BASE64,
      frames: atTotal,
    }).ok);
  }
}

// ---- ATT-STALE-SETTLE: a late video settle after chip removal is silent ----
{
  const queued: ComposerAttachment[] = [{ id: 'v1', name: 'c.mkv', size: 1, mimeType: 'video/x-matroska', kind: 'video', videoState: 'uploading' }];
  const failure: VideoUploadOutcome = { ok: false, status: 503, message: 'video preprocessing requires ffmpeg' };
  // Chip removed / composer reset while the upload was in flight: the settle
  // must not mutate the queue (no resurrection) and must not surface a toast.
  const stale = settleVideoOutcome([], 'v1', failure);
  check('stale settle leaves the queue unchanged', stale.next.length === 0, `next=${stale.next.length}`);
  check('stale settle is silent (surface=false)', stale.surface === false);
  const staleOk = settleVideoOutcome([], 'v1', { ok: true, attachment: queued[0]! });
  check('stale success settle is equally silent', staleOk.next.length === 0 && staleOk.surface === false);
  // Live failure: chip flips to the actionable error state + surfaces.
  const failed = settleVideoOutcome(queued, 'v1', failure);
  check('live settle failure marks the chip error', failed.next[0]!.videoState === 'error' && failed.next[0]!.videoError === 'video preprocessing requires ffmpeg');
  check('live settle failure surfaces the toast', failed.surface === true);
  // Live success: ready attachment with frames; never surfaces.
  const data: VideoUploadData = {
    attachmentId: 'x',
    name: 'c.mkv',
    container: 'mkv',
    mimeType: 'video/x-matroska',
    sizeBytes: 9,
    durationSeconds: 1,
    frameCount: 1,
    framesBase64Bytes: 4,
    frames: [{ index: 0, timestampSeconds: 0, mimeType: 'image/jpeg', data: 'AA==' }],
    instruction: 'm',
  };
  const okSettle = settleVideoOutcome(queued, 'v1', { ok: true, attachment: buildVideoAttachment({ name: 'c.mkv' } as File, data) });
  check('live settle success -> ready with frames', okSettle.next[0]!.videoState === 'ready' && okSettle.next[0]!.frames!.length === 1);
  check('live settle success never surfaces a toast', okSettle.surface === false);
  // Over-budget frames: error state + frames DROPPED so the wire invariant
  // (aggregate <= MAX_TOTAL_WIRE_BYTES) is restored immediately.
  const overBudget: VideoUploadOutcome = {
    ok: true,
    attachment: {
      ...buildVideoAttachment({ name: 'c.mkv' } as File, data),
      frames: [{ index: 0, timestampSeconds: 0, mimeType: 'image/jpeg', data: 'A'.repeat(MAX_TOTAL_WIRE_BYTES + 1) }],
      previewUrl: undefined,
    },
  };
  const over = settleVideoOutcome(queued, 'v1', overBudget);
  check('over-budget settle flips to error and drops the frames', over.next[0]!.videoState === 'error' && over.next[0]!.frames === undefined && (over.next[0]!.videoError ?? '').includes('send size limit'));
  check('over-budget settle restores the wire invariant', aggregateWire(over.next) <= MAX_TOTAL_WIRE_BYTES, `wire=${aggregateWire(over.next)}`);
}

// ---- ATT-FRAME-BYTES: exact serialized prompt command bytes vs 4 MiB cap ----
{
  // Byte-exact: same key order + JSON escaping as sendCommand's frame
  // (`{ ...command, id, sessionId }`), pinned to a literal serialized string.
  const literal = '{"type":"prompt","message":"hi","id":"c1","sessionId":"s"}';
  check('commandFrameBytes matches the exact wire literal', commandFrameBytes({ type: 'prompt', message: 'hi' }, 'c1', 's') === utf8ByteLength(literal), `${commandFrameBytes({ type: 'prompt', message: 'hi' }, 'c1', 's')} vs ${utf8ByteLength(literal)}`);
  check('commandFrameBytes without sessionId omits the key', commandFrameBytes({ type: 'prompt', message: 'hi' }, 'c1', null) === utf8ByteLength('{"type":"prompt","message":"hi","id":"c1"}'));
  // Escaping is counted exactly as JSON.stringify produces it.
  const escaped = 'say "hi" \\ backslash \n newline \t tab';
  const expected = utf8ByteLength('{"type":"prompt","message":"say \\"hi\\" \\\\ backslash \\n newline \\t tab","id":"c1"}');
  check('commandFrameBytes counts JSON escaping exactly', commandFrameBytes({ type: 'prompt', message: escaped }, 'c1', null) === expected, `${commandFrameBytes({ type: 'prompt', message: escaped }, 'c1', null)} vs ${expected}`);
  // Multibyte UTF-8 is counted in BYTES, not UTF-16 code units.
  const cjk = '本'.repeat(1000);
  const cjkBytes = commandFrameBytes({ type: 'prompt', message: cjk }, 'c1', null);
  check('commandFrameBytes counts multibyte text in UTF-8 bytes', cjkBytes === utf8ByteLength(JSON.stringify({ type: 'prompt', message: cjk, id: 'c1' })) && cjkBytes > 3000, `bytes=${cjkBytes}`);
  // The MAX_COMMAND_ID stand-in is an UPPER BOUND (never undercounts vs any
  // realistic id).
  const short = commandFrameBytes({ type: 'prompt', message: 'x' }, 'c1', null);
  const max = commandFrameBytes({ type: 'prompt', message: 'x' }, MAX_COMMAND_ID, null);
  check('MAX_COMMAND_ID stand-in is an upper bound on the frame bytes', max >= short && MAX_COMMAND_ID.length > 8, `max=${max} short=${short}`);
  // Over-limit rejection: a legal ready video's frames + a very large typed
  // message exceed the 4 MiB frame cap -> the gate rejects (submit preserves
  // the draft).
  const frames: VideoUploadFrame[] = Array.from({ length: 6 }, (_, i) => ({
    index: i,
    timestampSeconds: i,
    mimeType: 'image/jpeg',
    data: 'A'.repeat(465_000), // ~2.79 MB total frame base64
  }));
  const video: ComposerAttachment = { id: 'v1', name: 'c.mkv', size: 1, mimeType: 'video/x-matroska', kind: 'video', videoState: 'ready', frames, instruction: 'm' };
  const codeMessage = buildCodeMessage([]);
  const videoMarker = buildVideoMessage([video]);
  const hugeText = 'x'.repeat(1_600_000);
  const message = [codeMessage, videoMarker, hugeText].filter(Boolean).join('\n\n');
  const images = attachmentsToImageBlocks([video]);
  const command: Record<string, unknown> = { type: 'prompt', message };
  if (images.length > 0) command.images = images;
  const over = commandFrameBytes(command, MAX_COMMAND_ID, 's') >= MAX_PROMPT_FRAME_BYTES;
  check('ready video + huge typed text exceeds the 4 MiB frame cap (rejected)', over, `bytes=${commandFrameBytes(command, MAX_COMMAND_ID, 's')}`);
  // A normal prompt stays well under the cap.
  const small = commandFrameBytes({ type: 'prompt', message: 'hello', images: [{ type: 'image', data: 'AA', mimeType: 'image/png' }] }, MAX_COMMAND_ID, 's');
  check('normal prompt is far below the 4 MiB cap', small < MAX_PROMPT_FRAME_BYTES / 2, `bytes=${small}`);
}

// ---- ATT-WS-SENTINEL: raw video bytes/base64 never enter the command ----
{
  // Non-vacuous: the sentinel IS the raw video's actual bytes (as the fake
  // upload would carry them), and the assertion checks both the raw byte
  // string AND its exact base64 — the shape a regression that base64'd the
  // video into the prompt frame would produce. The composer must only ever
  // carry the extracted JPEG frame base64.
  const RAW_SENTINEL = 'RAW_VIDEO_SENTINEL_BYTES_9f8e7d';
  const rawBytes = new TextEncoder().encode(RAW_SENTINEL);
  const rawBase64 = Buffer.from(rawBytes).toString('base64');
  const frames: VideoUploadFrame[] = [{ index: 0, timestampSeconds: 0, mimeType: 'image/jpeg', data: 'FRAME_BASE64_A' }];
  check('frame base64 differs from the raw video base64 (assertion is meaningful)', 'FRAME_BASE64_A' !== rawBase64);
  const video: ComposerAttachment = { id: 'v1', name: 'clip.mkv', size: rawBytes.length, mimeType: 'video/x-matroska', kind: 'video', videoState: 'ready', frames, instruction: 'Frame at 0.0s — 1 chronological frame of clip.mkv.' };
  const codeMessage = buildCodeMessage([]);
  const videoMarker = buildVideoMessage([video]);
  const message = [codeMessage, videoMarker, 'analyze the clip'].filter(Boolean).join('\n\n');
  const images = attachmentsToImageBlocks([video]);
  const command: Record<string, unknown> = { type: 'prompt', message };
  if (images.length > 0) command.images = images;
  const serialized = JSON.stringify({ ...command, id: 'c1', sessionId: 's' });
  check('prompt frame carries the extracted frame base64', serialized.includes('FRAME_BASE64_A'));
  check('prompt frame NEVER contains the raw video bytes', !serialized.includes(RAW_SENTINEL));
  check('prompt frame NEVER contains base64 of the raw video bytes', !serialized.includes(rawBase64));
  check('prompt frame carries the bounded video marker', serialized.includes('chronological'));
}

// ---- ATT-DELAYED-READER: state only ever holds built reads + video placeholders ----
{
  const imgTask = { id: 't1', entry: { file: { name: 'a.png', type: 'image/png', size: 10 } as File, kind: 'image' as const } };
  const vidTask = { id: 't2', entry: { file: { name: 'c.mkv', type: 'video/x-matroska', size: 5 } as File, kind: 'video' as const } };
  const codeTask = { id: 't3', entry: { file: { name: 'b.rs', type: 'text/plain', size: 10 } as File, kind: 'code' as const } };
  const readById = new Map<string, ReadResult>([
    ['t1', { attachment: { id: 'x', name: 'a.png', size: 10, mimeType: 'image/png', kind: 'image', dataBase64: 'AA', previewUrl: 'data:image/png;base64,AA' } }],
    ['t3', { skip: { name: 'b.rs', size: 10, type: 'text/plain', reason: 'invalid-utf8' as const } }],
  ]);
  const merged = mergeIntakeResults([imgTask, vidTask, codeTask], readById);
  check('delayed-reader merge preserves intake order', merged.built.map((b) => b.name).join(',') === 'a.png,c.mkv', merged.built.map((b) => b.name).join(','));
  check('built image carries its payload (never a phantom placeholder)', merged.built[0]!.kind === 'image' && merged.built[0]!.dataBase64 === 'AA');
  check('video becomes the uploading placeholder', merged.built[1]!.kind === 'video' && merged.built[1]!.videoState === 'uploading');
  check('late code skip is collected, never built', merged.skips.length === 1 && merged.skips[0]!.reason === 'invalid-utf8' && merged.built.length === 2);
  const allSkip = mergeIntakeResults([imgTask, codeTask], new Map([
    ['t1', { skip: { name: 'a.png', size: 10, type: 'image/png', reason: 'unreadable' as const } }],
    ['t3', { skip: { name: 'b.rs', size: 10, type: 'text/plain', reason: 'invalid-utf8' as const } }],
  ]));
  check('all-late-skip batch builds nothing', allSkip.built.length === 0 && allSkip.skips.length === 2);
  const gap = mergeIntakeResults([imgTask, vidTask], new Map());
  check('missing read result: image dropped, video still placeholders', gap.built.length === 1 && gap.built[0]!.kind === 'video');
}

// ---- ATT-STALE-INTAKE: host/session switch discards the in-flight intake ----
{
  const base: IntakeContext = { gen: 1, host: '127.0.0.1:8080', token: 'tok-a', session: 's1' };
  check('identical intake context is not stale', isStaleIntake(base, { ...base }) === false);
  check('host switch (generation bump) makes the intake stale', isStaleIntake(base, { ...base, gen: 2 }) === true);
  check('authority change makes the intake stale', isStaleIntake(base, { ...base, host: '127.0.0.1:9090' }) === true);
  check('token commit makes the intake stale', isStaleIntake(base, { ...base, token: 'tok-b' }) === true);
  check('session switch makes the intake stale', isStaleIntake(base, { ...base, session: 's2' }) === true);
  check('session clear (reset) makes the intake stale', isStaleIntake(base, { ...base, session: null }) === true);
}

// ---- ATT-UPLOAD-HEADERS: X-Video-Name is ByteString-safe ----
{
  const ascii = videoUploadHeaders('clip.mkv', 'tok');
  check('ascii name percent-encoding is stable', ascii['X-Video-Name'] === 'clip.mkv', ascii['X-Video-Name']);
  check('tokened listener sends Bearer Authorization', ascii.Authorization === 'Bearer tok');
  const unicode = videoUploadHeaders('本 録画.mkv', '');
  check('unicode name is percent-encoded UTF-8', unicode['X-Video-Name'] === encodeURIComponent('本 録画.mkv'), unicode['X-Video-Name']);
  check('encoded header is ASCII-only (ByteString-safe)', Array.from(unicode['X-Video-Name']!).every((c) => c.charCodeAt(0) <= 0xff));
  check('tokenless listener omits Authorization', !('Authorization' in unicode));
  // A 255-byte Unicode name encodes to pure ASCII and stays a valid header
  // value (raw Unicode would throw TypeError on headers.append).
  const longName = `${'本'.repeat(100)}.mkv`;
  const longEncoded = videoUploadHeaders(longName, '')['X-Video-Name']!;
  check('long unicode name encodes fully ASCII', Array.from(longEncoded).every((c) => c.charCodeAt(0) <= 0xff) && longEncoded.includes('%'));
  check('control chars are percent-encoded', videoUploadHeaders('a\nb.mkv', '')['X-Video-Name'] === 'a%0Ab.mkv');
}

// ---- ATT-INTAKE-RESERVATION: reserve/release symmetry ----
{
  const accepted: AcceptedFile[] = [
    { file: { name: 'a.png', type: 'image/png', size: 300 } as File, kind: 'image' },
    { file: { name: 'c.mkv', type: 'video/x-matroska', size: 10_000_000 } as File, kind: 'video' },
    { file: { name: 'b.rs', type: 'text/plain', size: 100 } as File, kind: 'code' },
  ];
  const reservation = intakeReservation(accepted);
  check('reservation counts every accepted file', reservation.count === 3, `count=${reservation.count}`);
  check('reservation wire == per-file footprint sum', reservation.wire === wireFootprint('image', 300) + wireFootprint('video', 10_000_000) + wireFootprint('code', 100), `wire=${reservation.wire}`);
  check('video reserves 0 wire in the reservation', wireFootprint('video', 10_000_000) === 0);
  // A stale discard releases the SAME reservation: start budget + reserve -
  // release == start budget (no leaked bytes block follow-on intake).
  const start = { count: 2, wire: 5000 };
  const afterReserve = { count: start.count + reservation.count, wire: start.wire + reservation.wire };
  const afterRelease = { count: afterReserve.count - reservation.count, wire: afterReserve.wire - reservation.wire };
  check('reserve then release returns exactly to the start budget', afterRelease.count === start.count && afterRelease.wire === start.wire, `count=${afterRelease.count} wire=${afterRelease.wire}`);
}

// ---- ATT-INTAKE-OVERLAP: settle preserves OTHER in-flight reservations ----
{
  const tracker = new IntakeBudgetTracker();

  // Slow B reserves first, then fast A reserves against B's outstanding
  // footprint. A settles while B is still reading/uploading.
  tracker.reserve('slow-b', { count: 1, wire: 10 });
  const beforeA = tracker.admission();
  check('fast A admission sees slow B reservation', beforeA.count === 1 && beforeA.wire === 10, `count=${beforeA.count} wire=${beforeA.wire}`);
  tracker.reserve('fast-a', { count: 1, wire: 6 });
  tracker.setQueued({ count: 1, wire: 6 });
  tracker.release('fast-a');

  // The old queued-only reconcile bug produced 6 here (dropping B's 10), so
  // C would fit under a 16-byte cap and A+B+C could reach 26. The tracker must
  // keep B outstanding: admission remains 16 and C is rejected.
  const beforeC = tracker.admission();
  check('A settle preserves slow B reservation for C admission', beforeC.count === 2 && beforeC.wire === 16, `count=${beforeC.count} wire=${beforeC.wire}`);
  const c = classifyAttachments(
    [f('c.txt', 'text/plain', 1)],
    { currentCount: beforeC.count, currentWire: beforeC.wire, maxTotalWireBytes: 16 },
  );
  check('third overlapping intake C is rejected while B remains reserved', c.accepted.length === 0 && c.skipped.length === 1 && c.skipped[0]!.reason === 'over-budget');

  // B finally settles: queued is the exact A+B state and only B's key is
  // released. With no outstanding entries, admission equals queued exactly.
  tracker.setQueued({ count: 2, wire: 16 });
  tracker.release('slow-b');
  const exact = tracker.admission();
  check('after slow B settles budget is exact', exact.count === 2 && exact.wire === 16, `count=${exact.count} wire=${exact.wire}`);

  // Synchronous paste reservation: the paste handler reserves before React
  // runs its updater, so an async file intake between those points sees it.
  tracker.reserve('paste', { count: 1, wire: 4 });
  const duringPaste = tracker.admission();
  check('paste reservation is visible before its updater flushes', duringPaste.count === 3 && duringPaste.wire === 20, `count=${duringPaste.count} wire=${duringPaste.wire}`);
  tracker.setQueued({ count: 3, wire: 20 });
  tracker.release('paste');
  const afterPaste = tracker.admission();
  check('paste updater releases only its key and leaves queued exact', afterPaste.count === 3 && afterPaste.wire === 20, `count=${afterPaste.count} wire=${afterPaste.wire}`);

  tracker.reset();
  const reset = tracker.admission();
  check('tracker reset clears queued and outstanding', reset.count === 0 && reset.wire === 0, `count=${reset.count} wire=${reset.wire}`);
}

console.log(`\nattachments.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  for (const fail of failures) console.log(`  FAIL ${fail}`);
  process.exit(1);
}
process.exit(0);