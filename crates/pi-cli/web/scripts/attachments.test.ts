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
  MAX_FILE_BYTES,
  MAX_PROMPT_FRAME_BYTES,
  MAX_TOTAL_WIRE_BYTES,
  aggregateWire,
  attachmentAccept,
  attachmentsToImageBlocks,
  base64Length,
  buildCodeMessage,
  buildCodeSegment,
  classifyAttachments,
  classifyKind,
  codeBadgeLabel,
  codeLanguage,
  codeSegmentWireBytes,
  decodeUtf8OrReject,
  formatSkipSummary,
  imageMimeType,
  isImageFile,
  isTextFile,
  readAttachmentsInOrder,
  reconcileIntakeBudget,
  removeSentAttachments,
  safeFence,
  sanitizeFileName,
  utf8ByteLength,
  wireFootprint,
  type AttachSkip,
  type ComposerAttachment,
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

// ---- codeBadgeLabel ----
check('codeBadgeLabel uses extension', codeBadgeLabel('lib.rs') === 'RS');
check('codeBadgeLabel uses tsx extension', codeBadgeLabel('App.tsx') === 'TSX');
check('codeBadgeLabel falls back to TXT for no extension', codeBadgeLabel('Dockerfile') === 'TXT');
check('codeBadgeLabel empty -> TXT', codeBadgeLabel('') === 'TXT');

// ---- formatSkipSummary: null for empty, grouped summary, late reasons ----
{
  check('formatSkipSummary empty -> null', formatSkipSummary([]) === null);
  const one = formatSkipSummary([{ name: 'doc.pdf', size: 10, type: 'application/pdf', reason: 'unsupported' }] as AttachSkip[]);
  check('formatSkipSummary single reason', one === 'Skipped 1 file(s): 1 unsupported (images or text/code files only) (doc.pdf).', one ?? '');
  const mixed = formatSkipSummary([
    { name: 'a.pdf', size: 10, type: 'application/pdf', reason: 'unsupported' },
    { name: 'big.png', size: MAX_FILE_BYTES + 1, type: 'image/png', reason: 'oversize' },
    { name: 'b.pdf', size: 10, type: 'application/pdf', reason: 'unsupported' },
  ] as AttachSkip[]);
  check('formatSkipSummary groups same reason with sample', mixed != null && mixed.includes('2 unsupported (images or text/code files only)') && mixed.includes('a.pdf, b.pdf') && mixed.includes('1 over'), mixed ?? '');
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
}

console.log(`\nattachments.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  for (const fail of failures) console.log(`  FAIL ${fail}`);
  process.exit(1);
}
process.exit(0);