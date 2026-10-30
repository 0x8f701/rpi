import {
  acceptPicked,
  attachmentAccept,
  attachmentMeta,
  attachmentSizeCap,
  attachmentSkipMessage,
  base64Length,
  beginPickStream,
  buildPromptCommand,
  canSendAttachmentToTmux,
  completePdfUpload,
  completeVideoUpload,
  finishPickStream,
  formatSkipSummary,
  markUploading,
  MAX_CODE_BYTES,
  MAX_IMAGE_INPUT_BYTES,
  MAX_PDF_BYTES,
  MAX_TOTAL_WIRE_BYTES,
  MAX_VIDEO_BYTES,
  parsePickEvent,
  parseVideoUploadResponse,
  PASTED_TEXT_NAME,
  planPastedText,
  pushPickChunk,
  rememberTmuxFileDestination,
  removeAttachment,
  tmuxFileDestination,
  type AttachmentSkip,
  type PickBegin,
  type VideoFrame,
} from '../src/attachments';

const PNG_MAGIC = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
const JPEG_MAGIC = [0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, 0x4a, 0x46, 0x49, 0x46, 0x00, 0x01];
const GIF_MAGIC = [0x47, 0x49, 0x46, 0x38, 0x39, 0x61];
const WEBP_MAGIC = [0x52, 0x49, 0x46, 0x46, 0x00, 0x00, 0x00, 0x00, 0x57, 0x45, 0x42, 0x50];

interface PickedFile {
  header: PickBegin;
  data: string | Uint8Array;
}

function file(name: string, mimeType: string, payload: Buffer, encoding: 'base64' | 'text' = 'base64'): PickedFile {
  return {
    header: { type: 'begin', name, mimeType, size: payload.length, encoding, total: 1 },
    data: encoding === 'base64' ? payload.toString('base64') : payload.toString('utf8'),
  };
}

/** PDF/video payloads cross the picker bridge as decoded bytes (`bytes`). */
function byteFile(name: string, mimeType: string, payload: Buffer): PickedFile {
  return {
    header: { type: 'begin', name, mimeType, size: payload.length, encoding: 'bytes', total: 1 },
    data: new Uint8Array(payload),
  };
}

function image(name: string, mimeType: string, magic: number[], pad = 0): PickedFile {
  return file(name, mimeType, Buffer.concat([Buffer.from(magic), Buffer.alloc(pad)]));
}

function text(name: string, mimeType: string, body: string): PickedFile {
  return file(name, mimeType, Buffer.from(body, 'utf8'), 'text');
}

function frame(index: number, timestampSeconds: number): VideoFrame {
  return {
    index,
    timestampSeconds,
    mimeType: 'image/jpeg',
    data: Buffer.alloc(16, index + 1).toString('base64'),
  };
}

it('accepts every documented attachment type', () => {
  const cases: Array<[PickedFile, 'image' | 'video' | 'code' | 'pdf']> = [
    [image('shot.png', 'image/png', PNG_MAGIC, 12), 'image'],
    [image('photo.jpg', 'image/jpeg', JPEG_MAGIC, 12), 'image'],
    [image('anim.gif', 'image/gif', GIF_MAGIC, 12), 'image'],
    [image('modern.webp', 'image/webp', WEBP_MAGIC, 12), 'image'],
    [byteFile('clip.mkv', 'video/x-matroska', Buffer.alloc(2048, 7)), 'video'],
    [byteFile('report.pdf', 'application/pdf', Buffer.from('%PDF-1.7 body', 'utf8')), 'pdf'],
    [text('module.ts', 'text/typescript', 'const answer = 42;'), 'code'],
    [text('README.md', 'text/markdown', '# title'), 'code'],
  ];
  for (const [picked, kind] of cases) {
    const accepted = acceptPicked(picked.header, picked.data, []);
    expect(accepted.error).toBeNull();
    expect(accepted.items).toHaveLength(1);
    expect(accepted.items[0]).toMatchObject({ name: picked.header.name, kind, size: picked.header.size });
  }

  const video = byteFile('clip.MP4', 'video/mp4', Buffer.from([1, 2, 3, 4, 5]));
  const uploading = acceptPicked(video.header, video.data, []);
  expect(uploading.items[0]).toMatchObject({ kind: 'video', state: 'uploading', mimeType: 'video/mp4' });
  expect(uploading.upload).toEqual({ id: uploading.items[0].id });
  expect(Array.from(uploading.items[0].bytes ?? [])).toEqual([1, 2, 3, 4, 5]);

  const pdf = byteFile('report.PDF', 'application/pdf', Buffer.from('%PDF-1.7 body', 'utf8'));
  const pending = acceptPicked(pdf.header, pdf.data, []);
  expect(pending.items[0].state).toBe('uploading');
  expect(Array.from(pending.items[0].bytes ?? [])).toEqual(Array.from(Buffer.from('%PDF-1.7 body', 'utf8')));
});

it('refuses a file one byte over its own kind cap and accepts it at the cap', () => {
  const cases: Array<[PickedFile, number, string]> = [
    [text('notes.txt', 'text/plain', 'body'), MAX_CODE_BYTES, 'oversize'],
    [image('photo.png', 'image/png', PNG_MAGIC, 16), MAX_IMAGE_INPUT_BYTES, 'image-oversize'],
    [byteFile('report.pdf', 'application/pdf', Buffer.from('%PDF-1.7')), MAX_PDF_BYTES, 'pdf-oversize'],
    [byteFile('clip.mp4', 'video/mp4', Buffer.from([1, 2, 3, 4])), MAX_VIDEO_BYTES, 'video-oversize'],
  ];
  for (const [picked, cap, reason] of cases) {
    // One byte over the cap is refused for the KIND's reason, not for a generic
    // one; at the cap the same file is not refused for size at all.
    const refused = acceptPicked({ ...picked.header, size: cap + 1 }, picked.data, []);
    expect(refused.items).toEqual([]);
    expect(refused.skip?.reason).toBe(reason);
    expect(refused.error).toContain(picked.header.name);
    expect(acceptPicked({ ...picked.header, size: cap }, picked.data, []).skip?.reason).not.toBe(reason);
  }

  // A PDF exactly at the 32 MiB cap is accepted and enters the upload path.
  const atCap = acceptPicked(
    { type: 'begin', name: 'big.pdf', mimeType: 'application/pdf', size: MAX_PDF_BYTES, encoding: 'bytes', total: 4 },
    new Uint8Array(MAX_PDF_BYTES),
    [],
  );
  expect(atCap.error).toBeNull();
  expect(atCap.items[0]).toMatchObject({ kind: 'pdf', state: 'uploading', size: MAX_PDF_BYTES });

  expect(attachmentSizeCap('code')).toBe(MAX_CODE_BYTES);
  expect(attachmentSizeCap('image')).toBe(MAX_IMAGE_INPUT_BYTES);
  expect(attachmentSizeCap('pdf')).toBe(MAX_PDF_BYTES);
  expect(attachmentSizeCap('video')).toBe(MAX_VIDEO_BYTES);
});

it('refuses an image that is still too large after downscaling with a visible reason', () => {
  const event = parsePickEvent(JSON.stringify({ type: 'reject', name: 'IMG_9999.png', size: 5 * 1024 * 1024, reason: 'resized-oversize' }));
  expect(event).toEqual({ type: 'reject', name: 'IMG_9999.png', size: 5 * 1024 * 1024, reason: 'resized-oversize' });
  const message = attachmentSkipMessage('resized-oversize', 'IMG_9999.png');
  expect(message).toContain('IMG_9999.png');
  expect(message).toContain('scaled down to 2000px');
  expect(message).toContain('still too large');

  const bomb = parsePickEvent(JSON.stringify({ type: 'reject', name: 'bomb.png', size: 1024, reason: 'image-processing' }));
  expect(bomb).toEqual({ type: 'reject', name: 'bomb.png', size: 1024, reason: 'image-processing' });
  expect(attachmentSkipMessage('image-processing', 'bomb.png')).toContain('could not be decoded or resized safely');
});

it('labels the chip and the wire mimeType with the format of the bytes actually sent', () => {
  const reencoded = image('screenshot.png', 'image/png', JPEG_MAGIC, 24);
  const accepted = acceptPicked(reencoded.header, reencoded.data, []);
  expect(accepted.error).toBeNull();
  const chip = accepted.items[0];
  expect(chip.mimeType).toBe('image/jpeg');
  expect(chip.previewUrl).toBe(`data:image/jpeg;base64,${reencoded.data}`);
  expect(attachmentMeta(chip)).toContain('JPEG');
  expect(attachmentMeta(chip)).not.toContain('PNG');

  const diagram = image('diagram.png', 'image/png', PNG_MAGIC, 24);
  const png = acceptPicked(diagram.header, diagram.data, []);
  expect(png.items[0].mimeType).toBe('image/png');
  expect(attachmentMeta(png.items[0])).toContain('PNG');

  const built = buildPromptCommand(accepted.items, 'look at this', 'prompt');
  expect(built.ok).toBe(true);
  if (!built.ok) return;
  expect(built.command.images?.[0].mimeType).toBe('image/jpeg');

  const mislabeled = image('scan.png', 'image/png', [0x49, 0x49, 0x2a, 0x00], 24);
  const refused = acceptPicked(mislabeled.header, mislabeled.data, []);
  expect(refused.items).toEqual([]);
  expect(refused.error).toContain('scan.png');
  expect(refused.error).toContain('could not be read');
});

it('refuses an unsupported binary and a payload whose framing disagrees with its kind', () => {
  const binary = byteFile('installer.exe', 'application/octet-stream', Buffer.from([0x00, 0x01, 0x02, 0x62, 0x69, 0x6e]));
  const refused = acceptPicked(binary.header, binary.data, []);
  expect(refused.items).toEqual([]);
  expect(refused.skip).toEqual({ name: 'installer.exe', size: 6, reason: 'unsupported' });
  expect(refused.error).toContain('installer.exe');
  expect(refused.error).toContain('is not supported');

  const rejection = parsePickEvent(JSON.stringify({ type: 'reject', name: 'dump.txt', size: 4096, reason: 'invalid-utf8' }));
  expect(rejection).toEqual({ type: 'reject', name: 'dump.txt', size: 4096, reason: 'invalid-utf8' });
  expect(attachmentSkipMessage('invalid-utf8', 'dump.txt')).toContain('not valid UTF-8');

  // A video arriving as a text payload (or a PDF as base64) is refused rather
  // than half-read: the framing must match the kind the intake classified.
  const misFramed = byteFile('clip.mp4', 'video/mp4', Buffer.from([1, 2, 3, 4]));
  const wrongEncoding = acceptPicked({ ...misFramed.header, encoding: 'text' }, 'not bytes', []);
  expect(wrongEncoding.items).toEqual([]);
  expect(wrongEncoding.skip?.reason).toBe('unreadable');
});

it('refuses a truncated picker stream instead of attaching partial content', () => {
  const pdf = byteFile('report.pdf', 'application/pdf', Buffer.from('%PDF-1.7 scanned body', 'utf8'));
  const truncated = acceptPicked(pdf.header, new Uint8Array([1, 2, 3]), []);
  expect(truncated.items).toEqual([]);
  expect(truncated.skip?.reason).toBe('unreadable');
  expect(truncated.error).toContain('report.pdf');
  expect(truncated.error).toContain('could not be read');

  const png = image('shot.png', 'image/png', PNG_MAGIC, 8);
  const shortPng = acceptPicked({ ...png.header, total: 2 }, png.data.slice(0, 8), []);
  expect(shortPng.items).toEqual([]);
  expect(shortPng.error).toContain('shot.png');
});

it('reassembles a chunked byte stream into the exact file bytes', () => {
  const payload = Buffer.alloc(64 * 1024 + 7);
  for (let index = 0; index < payload.length; index += 1) payload[index] = (index * 7) % 251;
  const header: PickBegin = { type: 'begin', name: 'clip.mp4', mimeType: 'video/mp4', size: payload.length, encoding: 'bytes', total: 2 };
  const stream = beginPickStream(header);
  // The picker slices base64 on 4-char boundaries, so each chunk decodes alone.
  pushPickChunk(stream, payload.subarray(0, 3 * 1024).toString('base64'));
  pushPickChunk(stream, payload.subarray(3 * 1024).toString('base64'));
  const bytes = finishPickStream(stream);
  expect(bytes).toBeInstanceOf(Uint8Array);
  expect(Buffer.from(bytes as Uint8Array).equals(payload)).toBe(true);

  // A header above every per-kind cap allocates nothing, and the stream still
  // refuses on the cap rather than on a length mismatch.
  const oversize = beginPickStream({ ...header, size: MAX_VIDEO_BYTES + 1 });
  pushPickChunk(oversize, 'AAAA');
  expect((finishPickStream(oversize) as Uint8Array).length).toBe(0);
  expect(acceptPicked({ ...header, size: MAX_VIDEO_BYTES + 1 }, finishPickStream(oversize), []).skip?.reason).toBe('video-oversize');
});

it('settles a video upload into frames on the wire and blocks submit on failure', () => {
  const clip = byteFile('clip.mp4', 'video/mp4', Buffer.alloc(4096, 3));
  const accepted = acceptPicked(clip.header, clip.data, []);
  const id = accepted.items[0].id;

  const blocked = buildPromptCommand(accepted.items, 'what happens here?', 'prompt');
  expect(blocked.ok).toBe(false);
  if (blocked.ok) return;
  expect(blocked.error).toContain('clip.mp4');
  expect(blocked.error).toContain('still uploading');

  const ready = completeVideoUpload(accepted.items, id, {
    ok: true,
    frames: [frame(0, 0), frame(1, 1.5)],
    instruction: 'Video attachment: clip.mp4 — 2 chronological frames.',
    durationSeconds: 1.5,
    mimeType: 'video/mp4',
    sizeBytes: 4096,
  });
  expect(ready.error).toBeNull();
  const chip = ready.items[0];
  expect(chip).toMatchObject({ state: 'ready', kind: 'video', durationSeconds: 1.5 });
  expect(chip.bytes).toBeUndefined();
  expect(chip.previewUrl).toBe(`data:image/jpeg;base64,${frame(0, 0).data}`);
  expect(attachmentMeta(chip)).toContain('2 frames');
  expect(attachmentMeta(chip)).toContain('1.5 s');

  const built = buildPromptCommand(ready.items, 'what happens here?', 'prompt');
  expect(built.ok).toBe(true);
  if (!built.ok) return;
  expect(built.command.message).toContain('Video attachment: clip.mp4 — 2 chronological frames.');
  expect(built.command.message).toContain('what happens here?');
  expect(built.command.images).toEqual([
    { type: 'image', data: frame(0, 0).data, mimeType: 'image/jpeg' },
    { type: 'image', data: frame(1, 1.5).data, mimeType: 'image/jpeg' },
  ]);
});

it('keeps a failed upload as a retryable error chip and re-arms it', () => {
  const clip = byteFile('clip.mp4', 'video/mp4', Buffer.alloc(4096, 3));
  const accepted = acceptPicked(clip.header, clip.data, []);
  const id = accepted.items[0].id;

  const failed = completeVideoUpload(accepted.items, id, { ok: false, message: 'video analysis is unavailable: ffmpeg is not installed on the runner' });
  expect(failed.items).toHaveLength(1);
  const chip = failed.items[0];
  expect(chip).toMatchObject({ state: 'error', kind: 'video' });
  expect(chip.error).toContain('ffmpeg is not installed');
  expect(chip.bytes).toEqual(accepted.items[0].bytes);
  expect(attachmentMeta(chip)).toContain('upload failed');
  expect(failed.error).toBe('clip.mp4 failed to upload: video analysis is unavailable: ffmpeg is not installed on the runner');

  const blocked = buildPromptCommand(failed.items, 'send it', 'prompt');
  expect(blocked.ok).toBe(false);
  if (blocked.ok) return;
  expect(blocked.error).toContain('clip.mp4 failed to upload');
  expect(blocked.error).toContain('retry or remove it');

  const rearmed = markUploading(failed.items, id);
  expect(rearmed.items[0]).toMatchObject({ state: 'uploading', error: undefined });

  // Removing the chip drops the retry source with it.
  expect(removeAttachment(failed.items, id)).toEqual([]);
});

it('surfaces a failed PDF upload once, keeps the chip, and drops the bytes once ready', () => {
  const pdf = byteFile('report.pdf', 'application/pdf', Buffer.from('%PDF-1.7 scanned body', 'utf8'));
  const accepted = acceptPicked(pdf.header, pdf.data, []);
  const id = accepted.items[0].id;
  expect(accepted.upload).toEqual({ id });

  const blocked = buildPromptCommand(accepted.items, 'summarize this', 'prompt');
  expect(blocked.ok).toBe(false);

  const failed = completePdfUpload(accepted.items, id, { ok: false, message: 'the runner is busy with other PDF uploads — try again shortly' });
  expect(failed.items[0]).toMatchObject({ state: 'error' });
  expect(failed.error).toBe('report.pdf failed to upload: the runner is busy with other PDF uploads — try again shortly');

  // A chip removed while its upload was in flight settles silently.
  const removed = completePdfUpload([], id, { ok: false, message: 'gone' });
  expect(removed.items).toEqual([]);
  expect(removed.error).toBeNull();

  const done = completePdfUpload(failed.items, id, {
    ok: true,
    text: 'Extracted paper body',
    instruction: 'PDF attachment: report.pdf — extracted text layer.',
  });
  expect(done.error).toBeNull();
  expect(done.items[0]).toMatchObject({ name: 'report.pdf', kind: 'pdf', state: 'ready', text: 'Extracted paper body' });
  expect(done.items[0].bytes).toBeUndefined();
  expect(attachmentMeta(done.items[0])).toContain('PDF');
  expect(removeAttachment(done.items, id)).toEqual([]);
});

it('keeps the bytes of an upload the batch budget rejects so Retry can re-send it', () => {
  // A batch that already spends most of the one-prompt-frame budget: a 2 MiB
  // code file plus a PDF whose extracted text cannot fit beside it.
  const code = text('notes.txt', 'text/plain', 'x'.repeat(MAX_CODE_BYTES));
  const withCode = acceptPicked(code.header, code.data, []);
  expect(withCode.error).toBeNull();
  const pdf = byteFile('report.pdf', 'application/pdf', Buffer.from('%PDF-1.7 scanned body', 'utf8'));
  const accepted = acceptPicked(pdf.header, pdf.data, withCode.items);
  const pdfId = accepted.items[1].id;
  expect(accepted.upload).toEqual({ id: pdfId });

  const rejected = completePdfUpload(accepted.items, pdfId, {
    ok: true,
    text: 'y'.repeat(1_500_000),
    instruction: 'PDF attachment: report.pdf — extracted text layer.',
  });
  const chip = rejected.items[1];
  expect(chip).toMatchObject({ state: 'error' });
  expect(chip.error).toContain('exceeds the total send size limit');
  expect(chip.text).toBeUndefined();
  expect(attachmentMeta(chip)).toContain('upload failed');
  expect(rejected.error).toBe('report.pdf failed to upload: extracted text exceeds the total send size limit');
  // The retry source is still on the chip: the exact bytes the upload POSTs.
  expect(chip.bytes).toEqual(accepted.items[1].bytes);
  const rearmed = markUploading(rejected.items, pdfId).items[1];
  expect(rearmed).toMatchObject({ state: 'uploading' });
  expect(rearmed.bytes).toEqual(accepted.items[1].bytes);

  // Same contract for a video whose frames do not fit beside the code chip.
  // The result is built through the production validator: a frame is capped at
  // 384 KiB, six frames at 2 MiB in total, and the duration must be positive.
  const clip = byteFile('clip.mp4', 'video/mp4', Buffer.alloc(4096, 3));
  const clipAccepted = acceptPicked(clip.header, clip.data, withCode.items);
  const clipId = clipAccepted.items[1].id;
  const videoOutcome = parseVideoUploadResponse(200, {
    instruction: 'Video attachment: clip.mp4 — 6 chronological frames.',
    durationSeconds: 12,
    mimeType: 'video/mp4',
    sizeBytes: 4096,
    frames: Array.from({ length: 6 }, (_, index) => ({
      index,
      timestampSeconds: index * 2,
      mimeType: 'image/jpeg',
      data: Buffer.alloc(300_000, index + 1).toString('base64'),
    })),
  });
  expect(videoOutcome.ok).toBe(true);
  if (!videoOutcome.ok) return;
  const videoRejected = completeVideoUpload(clipAccepted.items, clipId, videoOutcome);
  expect(videoRejected.items[1]).toMatchObject({ state: 'error' });
  expect(videoRejected.items[1].error).toContain('the total send size limit');
  expect(videoRejected.items[1].bytes).toEqual(clipAccepted.items[1].bytes);
  expect(videoRejected.items[1].frames).toBeUndefined();
});

it('refuses a code file that no longer fits beside the media a ready upload produced', () => {
  // A settled video's frames ride the prompt frame, so they spend the budget
  // even though its raw bytes never do.
  const clip = byteFile('clip.mp4', 'video/mp4', Buffer.alloc(4096, 3));
  const clipAccepted = acceptPicked(clip.header, clip.data, []);
  const clipId = clipAccepted.items[0].id;
  const videoOutcome = parseVideoUploadResponse(200, {
    instruction: 'Video attachment: clip.mp4 — 6 chronological frames.',
    durationSeconds: 12,
    mimeType: 'video/mp4',
    sizeBytes: 4096,
    frames: Array.from({ length: 6 }, (_, index) => ({
      index,
      timestampSeconds: index * 2,
      mimeType: 'image/jpeg',
      data: Buffer.alloc(300_000, index + 1).toString('base64'),
    })),
  });
  expect(videoOutcome.ok).toBe(true);
  if (!videoOutcome.ok) return;
  const ready = completeVideoUpload(clipAccepted.items, clipId, videoOutcome);
  expect(ready.error).toBeNull();
  expect(ready.items[0].frames).toHaveLength(6);

  const code = text('notes.txt', 'text/plain', 'x'.repeat(MAX_CODE_BYTES));
  const refused = acceptPicked(code.header, code.data, ready.items);
  expect(refused.items).toEqual(ready.items);
  expect(refused.skip?.reason).toBe('over-budget');
  expect(refused.error).toContain('does not fit the 3.0 MiB attachment budget');

  // The same for a settled PDF's extracted text layer.
  const pdf = byteFile('report.pdf', 'application/pdf', Buffer.from('%PDF-1.7 scanned body', 'utf8'));
  const pdfAccepted = acceptPicked(pdf.header, pdf.data, []);
  const pdfId = pdfAccepted.items[0].id;
  const settledPdf = completePdfUpload(pdfAccepted.items, pdfId, {
    ok: true,
    text: 'y'.repeat(2_500_000),
    instruction: 'PDF attachment: report.pdf — extracted text layer.',
  });
  expect(settledPdf.error).toBeNull();
  const refusedPdf = acceptPicked(code.header, code.data, settledPdf.items);
  expect(refusedPdf.items).toEqual(settledPdf.items);
  expect(refusedPdf.skip?.reason).toBe('over-budget');
});

it('bounds the serialized prompt frame at submit, not only at intake', () => {
  // Two separately accepted batches exceed one prompt frame once combined: the
  // final bound is the last line of defence, on the produced payload.
  const code = text('notes.txt', 'text/plain', 'x'.repeat(MAX_CODE_BYTES));
  const first = acceptPicked(code.header, code.data, []);
  expect(first.error).toBeNull();
  const second = acceptPicked(code.header, code.data, []);
  expect(second.error).toBeNull();
  const combined = [...first.items, ...second.items];

  const built = buildPromptCommand(combined, 'review these', 'prompt');
  expect(built.ok).toBe(false);
  if (built.ok) return;
  expect(built.error).toBe('Attachments exceed the 3.0 MiB prompt budget — remove one or pick a smaller file.');

  // One batch alone stays sendable.
  const single = buildPromptCommand(first.items, 'review this', 'prompt');
  expect(single.ok).toBe(true);
});

it('groups a mixed skip batch into one summary naming the refused files', () => {
  const skipped: AttachmentSkip[] = [
    { name: 'installer.exe', size: 10, reason: 'unsupported' },
    { name: 'huge.png', size: MAX_IMAGE_INPUT_BYTES + 1, reason: 'image-oversize' },
    { name: 'dump.bin', size: 10, reason: 'unsupported' },
    { name: 'notes.txt', size: MAX_CODE_BYTES + 1, reason: 'oversize' },
  ];
  const summary = formatSkipSummary(skipped);
  expect(summary).toContain('Skipped 4 file(s)');
  expect(summary).toContain('2 unsupported (images, videos, PDFs, or text/code files only) (installer.exe, dump.bin)');
  expect(summary).toContain('huge.png');
  expect(summary).toContain('image input limit');
  expect(summary).toContain('notes.txt');
  expect(summary).toContain('per-file limit');
  expect(formatSkipSummary([])).toBeNull();
  expect(formatSkipSummary([{ name: 'a', size: 1, reason: 'unreadable' }])).toBe('Skipped 1 file(s): 1 could not be read (a).');
});

it('counts JSON escaping toward the serialized frame bound', () => {
  // A 2 MiB code file of U+0001 escapes to six bytes per character on the
  // wire: the raw-text estimate fits the budget, the serialized frame does not.
  const code = text('notes.txt', 'text/plain', '\u0001'.repeat(MAX_CODE_BYTES));
  const accepted = acceptPicked(code.header, code.data, []);
  expect(accepted.error).toBeNull();
  const built = buildPromptCommand(accepted.items, 'review', 'prompt', 'session-1');
  expect(built.ok).toBe(false);
  if (built.ok) return;
  expect(built.error).toBe('message and attachments exceed the 4.0 MiB send limit — shorten the message or remove some attachments');
});

it('counts the typed message toward the serialized frame, including non-ASCII', () => {
  // Two UTF-8 bytes per character: 2.2M characters are a 4.4 MiB frame.
  const refused = buildPromptCommand([], 'é'.repeat(2_200_000), 'prompt', 'session-1');
  expect(refused.ok).toBe(false);
  if (refused.ok) return;
  expect(refused.error).toBe('message and attachments exceed the 4.0 MiB send limit — shorten the message or remove some attachments');

  // A draft that fits stays sendable.
  const built = buildPromptCommand([], 'é'.repeat(100_000), 'prompt', 'session-1');
  expect(built.ok).toBe(true);
  if (!built.ok) return;
  expect(built.command.message).toBe('é'.repeat(100_000));
});

it('turns a large paste into a pasted-text attachment and leaves typing native', () => {
  const body = Array.from({ length: 40 }, (_, index) => `line ${index}`).join('\n');
  const pasted = planPastedText('draft', `draft${body}`);
  expect(pasted.type).toBe('attachment');
  if (pasted.type !== 'attachment') return;
  expect(pasted.text).toBe(body);
  expect(pasted.size).toBe(Buffer.byteLength(body, 'utf8'));
  const accepted = acceptPicked({
    type: 'begin',
    name: PASTED_TEXT_NAME,
    mimeType: 'text/plain',
    size: pasted.size,
    encoding: 'text',
    total: 1,
  }, pasted.text, []);
  expect(accepted.error).toBeNull();
  expect(accepted.items[0]).toMatchObject({ name: PASTED_TEXT_NAME, kind: 'code', language: 'text' });

  // A large single-line blob is an attachment too; a typed character is not.
  expect(planPastedText('', 'x'.repeat(9 * 1024)).type).toBe('attachment');
  expect(planPastedText('a', 'ab').type).toBe('native');
  expect(planPastedText('hello', 'hello world').type).toBe('native');
  expect(planPastedText('', '')).toEqual({ type: 'native' });

  const oversize = planPastedText('', 'y'.repeat(MAX_CODE_BYTES + 1));
  expect(oversize).toEqual({ type: 'oversize', size: MAX_CODE_BYTES + 1 });
});

it('validates a video upload response fail-closed', () => {
  const good = {
    instruction: 'Video attachment: clip.mp4 — 2 chronological frames.',
    durationSeconds: 1.5,
    mimeType: 'video/mp4',
    sizeBytes: 4096,
    frames: [frame(0, 0), frame(1, 1.5)],
  };
  const parsed = parseVideoUploadResponse(200, good);
  expect(parsed.ok).toBe(true);
  if (!parsed.ok) return;
  expect(parsed.frames).toHaveLength(2);
  expect(parsed.durationSeconds).toBe(1.5);

  const broken: unknown[] = [
    { ...good, frames: [] },
    { ...good, durationSeconds: 0 },
    { ...good, durationSeconds: 601 },
    { ...good, instruction: 'x'.repeat(4096) },
    { ...good, frames: [{ ...frame(1, 0) }] },
    { ...good, frames: [{ ...frame(0, 0), mimeType: 'image/png' }] },
    { ...good, frames: [{ ...frame(0, 0), data: 'A'.repeat(600 * 1024) }] },
    'not an object',
  ];
  for (const body of broken) {
    expect(parseVideoUploadResponse(200, body)).toEqual({ ok: false, message: 'the video upload endpoint returned an unexpected response' });
  }
  expect(parseVideoUploadResponse(413, null)).toEqual({ ok: false, message: 'the video is over the 64.0 MiB server upload limit' });
  expect(parseVideoUploadResponse(415, null)).toEqual({ ok: false, message: 'unsupported video container (mkv/mp4/webm/mov/avi/ogg)' });
  expect(parseVideoUploadResponse(503, { error: 'ffmpeg missing on the runner' })).toEqual({ ok: false, message: 'ffmpeg missing on the runner' });
});

it('offers the supported kinds in the picker accept list instead of a wildcard', () => {
  const accept = attachmentAccept();
  expect(accept).not.toContain('*');
  for (const entry of ['image/png', 'image/jpeg', 'image/gif', 'image/webp', 'video/mp4', 'video/x-matroska', '.mkv', '.webm', 'application/pdf', '.pdf', '.ts', '.md']) {
    expect(accept.split(',')).toContain(entry);
  }
});

it('sends an image or code chip to the terminal only with a live destination', () => {
  rememberTmuxFileDestination(null);
  expect(tmuxFileDestination()).toBeNull();

  const png = image('screenshot.png', 'image/png', PNG_MAGIC, 24);
  const code = text('module.ts', 'text/typescript', 'const answer = 42;');
  const pdf = byteFile('report.pdf', 'application/pdf', Buffer.from('%PDF-1.7 body', 'utf8'));
  const pngChip = acceptPicked(png.header, png.data, []).items[0];
  const codeChip = acceptPicked(code.header, code.data, []).items[0];
  const pdfChip = acceptPicked(pdf.header, pdf.data, []).items[0];
  expect(canSendAttachmentToTmux(pngChip)).toBe(true);
  expect(canSendAttachmentToTmux(codeChip)).toBe(true);
  expect(canSendAttachmentToTmux(pdfChip)).toBe(false);

  rememberTmuxFileDestination({ runnerId: 'r1', session: 'work', origin: 'http://127.0.0.1:8787', token: 'tok' });
  expect(tmuxFileDestination()).toEqual({ runnerId: 'r1', session: 'work', origin: 'http://127.0.0.1:8787', token: 'tok' });
  // An empty session (or a malformed origin) is not a destination.
  rememberTmuxFileDestination({ runnerId: 'r1', session: '', origin: 'http://127.0.0.1:8787', token: '' });
  expect(tmuxFileDestination()).toBeNull();
  rememberTmuxFileDestination({ runnerId: 'r1', session: 'work', origin: 'http://127.0.0.1:8787#frag', token: '' });
  expect(tmuxFileDestination()).toBeNull();
  rememberTmuxFileDestination(null);
});

it('carries an attached image as an image content block in the prompt payload', () => {
  const png = image('screenshot.png', 'image/png', PNG_MAGIC, 32);
  const accepted = acceptPicked(png.header, png.data, []);
  const built = buildPromptCommand(accepted.items, 'what is wrong here?', 'prompt');
  expect(built.ok).toBe(true);
  if (!built.ok) return;
  expect(built.command).toEqual({
    type: 'prompt',
    message: 'what is wrong here?',
    images: [{ type: 'image', data: png.data, mimeType: 'image/png' }],
  });
});

it('carries attached code as a fenced segment and no image blocks', () => {
  const code = text('sample.ts', 'text/typescript', 'const answer = 42;');
  const accepted = acceptPicked(code.header, code.data, []);
  const built = buildPromptCommand(accepted.items, 'review this', 'steer');
  expect(built.ok).toBe(true);
  if (!built.ok) return;
  expect(built.command).toEqual({
    type: 'steer',
    message: 'File: sample.ts\n```typescript\nconst answer = 42;\n```\n\nreview this',
  });
});

it('bounds the processed image on the wire rather than its raw input size', () => {
  const photo = image('IMG_1234.jpg', 'image/jpeg', JPEG_MAGIC, 600 * 1024);
  const accepted = acceptPicked(photo.header, photo.data, []);
  expect(accepted.error).toBeNull();
  const chip = accepted.items[0];
  expect(base64Length(chip.size)).toBeLessThanOrEqual(MAX_TOTAL_WIRE_BYTES);

  const built = buildPromptCommand(accepted.items, 'what is this?', 'prompt');
  expect(built.ok).toBe(true);
  if (!built.ok) return;
  expect(built.command.images).toEqual([{ type: 'image', data: photo.data, mimeType: 'image/jpeg' }]);

  // A big photo reserves only its PROCESSED cap (2 MiB encoded), so a second
  // one cannot share the frame: the reservation is what the budget counts.
  const big = image('big.png', 'image/png', PNG_MAGIC, 3 * 1024 * 1024);
  const first = acceptPicked(big.header, big.data, []);
  expect(first.error).toBeNull();
  const second = acceptPicked({ ...big.header, name: 'second.png' }, big.data, first.items);
  expect(second.items).toHaveLength(1);
  expect(second.skip?.reason).toBe('over-budget');
  expect(second.error).toContain('second.png');
});
