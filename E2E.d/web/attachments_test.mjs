// Web composer attachment intake E2E lane (playwright half of
// E2E.d/web/attachments.sh).
//
// Environment:
//   RPI_URL          http://127.0.0.1:<port>/web
//   RPI_TOKEN        token file content (served via rpi-auth.<token> subprotocol)
//   RPI_CHROME       executable path of the system Chrome (optional)
//   RPI_EVIDENCE     evidence dir for screenshots
//
// Asserts the v0.2.10 Web composer attachment intake — clipboard image paste,
// multi-file picker, multi-file drag/drop, Rust/TypeScript code upload, and
// the outgoing RPC prompt frame content/order — against the REAL `rpi
// --listen` binary + loopback mock provider in a real browser:
//
//   - Paste image via real ClipboardEvent/DataTransfer with a valid tiny PNG
//     File — the paste dispatch is CANCELED (defaultPrevented) only when the
//     clipboard carries files; a text-only ClipboardEvent is NOT canceled.
//   - Picker sets 2 code files together (.rs + .ts) via the hidden
//     input[type=file] — 2 code chips appear with RS/TS badge labels.
//   - Drop sends 2 code files together via synthetic DragEvent on the footer —
//     the drop-active highlight (footer[data-drop-active], .composer-drop)
//     appears on dragenter and clears on drop; 2 code chips appear.
//   - Chips preserve global intake order: pasted image -> picker .rs -> picker
//     .ts -> drop .rs -> drop .ts.
//   - PDF picker yields a visible error toast (toast--error with the PDF name)
//     and NO chip (PDFs are unsupported, rejected at intake).
//   - Send dispatches a prompt RPC — observed on the outgoing WS frame
//     (deterministic; the provider round-trip is NOT required): one images
//     block (the pasted PNG), message contains filename+source for ALL code
//     files in the same order as the chips, and NO PDF content.
//   - Attachments clear after dispatch (#composer-attachments leaves the DOM).
//   - The sent user bubble renders the image thumbnail INLINE (image preview
//     first, then the user's caption) — never the old "(image attached)"
//     placeholder and never the raw <attachment> transport wrapper.
//   - A second, image-only multi-image send renders 2 distinct thumbnails
//     with NO text; the ACK reconcile never duplicates or flickers bubbles.
//   - At a phone viewport the multi-image grid causes no horizontal overflow.
//   - Reload restores BOTH user bubbles with their thumbnails from
//     get_messages history (the persisted prompt image ContentBlocks render
//     directly).
//   - User-caption Markdown: a real send of a Markdown prompt (heading /
//     list / bold / inline code / code fence / hostile HTML) renders through
//     the SHARED renderMarkdown pipeline (MarkdownBody — same component the
//     collab guest view uses) — structural .md-* tags, hostile elements stay
//     inert literal text, NO whole-message pre wrapper, images keep rendering
//     BEFORE the caption, and a reload still renders the caption as Markdown.

import { chromium } from 'playwright';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';

// Fixture file values the playwright half asserts against (kept in lockstep
// with attachments.sh — documentation/example filenames and content only).
const PASTE_IMAGE = 'paste-screenshot.png';
const PICKER_RS = 'example.rs';
const PICKER_TS = 'example.ts';
const DROP_RS = 'snippet.rs';
const DROP_TS = 'snippet.ts';
const PDF_NAME = 'notes.pdf';
const PICKER_RS_CONTENT = 'fn main() { println!("Hello, world!"); }';
const PICKER_TS_CONTENT = 'const greeting: string = "Hello, world!";';
const DROP_RS_CONTENT = 'pub fn add(a: i32, b: i32) -> i32 { a + b }';
const DROP_TS_CONTENT = 'export const greet = (name: string): string => "Hi " + name;';
const USER_MESSAGE = 'please review the attached code';
// User-caption Markdown fixture: heading / list (bold + inline code items) /
// bold + inline code paragraph / fenced code block / hostile raw HTML that
// must stay inert literal text (never elements).
const USER_MARKDOWN = [
  '## User markdown e2e',
  '',
  '- item one',
  '- **bold item**',
  '- item with `inline code`',
  '',
  'Plain paragraph with **bold** and `code`.',
  '',
  '```ts',
  'const x: number = 1;',
  '```',
  '',
  '<script>alert(1)</script> and <img src="x" onerror="alert(2)"> stay literal.',
].join('\n');

function fail(message) {
  // Exit 2 (not 1): 1 means "playwright setup failure (node/chromium/npm)";
  // 2+ is an assertion failure (the lane reports it distinctly).
  console.error(`web-attachments: FAIL: ${message}`);
  process.exit(2);
}

async function waitFor(page, fn, label, timeoutMs = 25000, arg) {
  try {
    await page.waitForFunction(fn, arg, { timeout: timeoutMs });
  } catch {
    fail(`${label} (timeout ${timeoutMs}ms)`);
  }
}

/** Read the current chip info from the DOM in intake order. Each chip is a
 *  .composer-attachment with a __name span; image chips carry a __thumb img,
 *  code chips carry a __badge span with the extension label. */
async function readChips(page) {
  return page.evaluate(() =>
    Array.from(document.querySelectorAll('.composer-attachment')).map((chip) => ({
      name: chip.querySelector('.composer-attachment__name')?.textContent?.trim() || '',
      badge: chip.querySelector('.composer-attachment__badge')?.textContent?.trim() || '',
      isImage: chip.querySelector('.composer-attachment__thumb') !== null,
    }))
  );
}

async function main() {
  if (!url) fail('RPI_URL is required');
  const launchOptions = chromePath ? { executablePath: chromePath } : {};
  const browser = await chromium.launch(launchOptions);
  try {
    const page = await browser.newPage();
    if (token) {
      await page.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
    }
    page.on('pageerror', (err) => {
      console.error(`web-attachments: page error: ${err.message}`);
    });

    // Capture outgoing WS frames so the prompt dispatch can be observed
    // without requiring the provider round-trip to succeed (deterministic).
    const sentFrames = [];
    page.on('websocket', (ws) => {
      ws.on('framesent', (frame) => {
        const payload = typeof frame.payload === 'string' ? frame.payload : '';
        if (payload) sentFrames.push(payload);
      });
    });

    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'page title missing');
    await waitFor(page, () => document.getElementById('conn-state') !== null, 'conn-state missing');
    await waitFor(
      page,
      () => document.getElementById('conn-state').dataset.state === 'on',
      'WS did not reach "connected"'
    );

    // The composer surface must exist: #prompt-input textarea, #attach-btn,
    // and the hidden file input.
    if (!await page.locator('#prompt-input').count()) fail('#prompt-input not found');
    if (!await page.locator('#attach-btn').count()) fail('#attach-btn not found');
    if (!await page.locator('input[type=file]').count()) fail('hidden input[type=file] not found');

    // ------------------------------------------------------------------
    // Paste image via real ClipboardEvent/DataTransfer with a valid tiny
    // PNG File. The paste handler calls preventDefault ONLY when the
    // clipboard carries files. A text-only paste must NOT be canceled.
    // ------------------------------------------------------------------
    const pasteResult = await page.evaluate(() => {
      // Build a valid 1x1 PNG via canvas so the File carries real image bytes.
      const canvas = document.createElement('canvas');
      canvas.width = 1;
      canvas.height = 1;
      const ctx = canvas.getContext('2d');
      ctx.fillStyle = 'rgb(255,0,0)';
      ctx.fillRect(0, 0, 1, 1);
      const dataUrl = canvas.toDataURL('image/png');
      const base64 = dataUrl.split(',')[1];
      const binStr = atob(base64);
      const bytes = new Uint8Array(binStr.length);
      for (let i = 0; i < binStr.length; i++) bytes[i] = binStr.charCodeAt(i);
      const file = new File([bytes], 'paste-screenshot.png', { type: 'image/png' });
      const dt = new DataTransfer();
      dt.items.add(file);
      const event = new ClipboardEvent('paste', {
        clipboardData: dt,
        bubbles: true,
        cancelable: true,
      });
      const target = document.getElementById('prompt-input');
      target.dispatchEvent(event);
      return {
        defaultPrevented: event.defaultPrevented,
        fileCount: event.clipboardData ? event.clipboardData.files.length : -1,
      };
    });
    if (pasteResult.fileCount !== 1) {
      fail(`file paste: clipboardData.files should carry 1 file (got ${pasteResult.fileCount}) — ClipboardEvent DataTransfer not wired`);
    }
    if (!pasteResult.defaultPrevented) {
      fail('file paste: dispatch was NOT canceled (defaultPrevented=false) — onPaste did not call preventDefault for file clipboard data');
    }

    // Wait for the image chip to appear (FileReader.readAsDataURL is async).
    await waitFor(
      page,
      () => {
        const chips = document.querySelectorAll('.composer-attachment');
        return chips.length === 1 && chips[0].querySelector('.composer-attachment__thumb') !== null;
      },
      'file paste: image chip (with thumbnail) did not appear after paste'
    );
    const pasteChips = await readChips(page);
    if (pasteChips.length !== 1 || !pasteChips[0].isImage || pasteChips[0].name !== PASTE_IMAGE) {
      fail(`file paste: expected 1 image chip named ${PASTE_IMAGE} (got ${JSON.stringify(pasteChips)})`);
    }
    await page.screenshot({ path: `${evidence}/paste.png`, fullPage: true });

    // Text-only paste: the handler must NOT call preventDefault (no files).
    const textPasteResult = await page.evaluate(() => {
      const dt = new DataTransfer();
      dt.setData('text/plain', 'hello plain text');
      const event = new ClipboardEvent('paste', {
        clipboardData: dt,
        bubbles: true,
        cancelable: true,
      });
      const target = document.getElementById('prompt-input');
      target.dispatchEvent(event);
      return { defaultPrevented: event.defaultPrevented };
    });
    if (textPasteResult.defaultPrevented) {
      fail('text-only paste: dispatch WAS canceled (defaultPrevented=true) — onPaste should not preventDefault for text-only clipboard data');
    }
    // No new chip should have appeared from the text-only paste.
    const afterTextChips = await readChips(page);
    if (afterTextChips.length !== 1) {
      fail(`text-only paste: chip count changed from 1 to ${afterTextChips.length} — text paste should not create a chip`);
    }

    // ------------------------------------------------------------------
    // Picker sets 2 code files together (.rs + .ts) via the hidden
    // input[type=file]. Playwright setInputFiles dispatches the change
    // event; the React onChange handler calls onFilesChosen.
    // ------------------------------------------------------------------
    await page.setInputFiles('input[type=file]', [
      { name: PICKER_RS, mimeType: 'text/plain', buffer: Buffer.from(PICKER_RS_CONTENT) },
      { name: PICKER_TS, mimeType: 'text/plain', buffer: Buffer.from(PICKER_TS_CONTENT) },
    ]);
    await waitFor(
      page,
      () => document.querySelectorAll('.composer-attachment').length === 3,
      'picker: 2 code chips did not appear (expected 3 total chips)'
    );
    const pickerChips = await readChips(page);
    if (pickerChips.length !== 3) {
      fail(`picker: expected 3 chips (1 image + 2 code), got ${pickerChips.length}`);
    }
    if (pickerChips[1].name !== PICKER_RS || pickerChips[1].badge !== 'RS') {
      fail(`picker: first code chip should be ${PICKER_RS} with badge RS (got ${JSON.stringify(pickerChips[1])})`);
    }
    if (pickerChips[2].name !== PICKER_TS || pickerChips[2].badge !== 'TS') {
      fail(`picker: second code chip should be ${PICKER_TS} with badge TS (got ${JSON.stringify(pickerChips[2])})`);
    }
    if (pickerChips[1].isImage || pickerChips[2].isImage) {
      fail('picker: code chips should not have image thumbnails');
    }
    await page.screenshot({ path: `${evidence}/picker.png`, fullPage: true });

    // ------------------------------------------------------------------
    // Drop sends 2 code files together through the composer's drag event
    // path. dragenter activates the footer highlight; drop queues both.
    // ------------------------------------------------------------------

    // Dispatch to the COMPOSER footer, not the sidebar's own footer. Keep the
    // DataTransfer on window between dragenter and drop so the same two files
    // drive both events and the active-highlight screenshot is observable.
    await page.evaluate(({ rustSource, tsSource }) => {
      const dt = new DataTransfer();
      dt.items.add(new File([rustSource], 'snippet.rs', { type: 'text/plain' }));
      dt.items.add(new File([tsSource], 'snippet.ts', { type: 'text/plain' }));
      window.__attachmentDragTransfer = dt;
      document.querySelector('.app-main > footer').dispatchEvent(
        new DragEvent('dragenter', { dataTransfer: dt, bubbles: true, cancelable: true })
      );
    }, { rustSource: DROP_RS_CONTENT, tsSource: DROP_TS_CONTENT });
    await waitFor(
      page,
      () => document.querySelector('.app-main > footer').dataset.dropActive === 'true',
      'drop: footer[data-drop-active="true"] not set after dragenter'
    );
    await waitFor(
      page,
      () => document.querySelector('.composer-drop') !== null,
      'drop: .composer-drop hint not visible after dragenter'
    );
    await page.screenshot({ path: `${evidence}/drag-active.png`, fullPage: true });
    await page.evaluate(() => {
      const dt = window.__attachmentDragTransfer;
      const footer = document.querySelector('.app-main > footer');
      footer.dispatchEvent(new DragEvent('dragover', { dataTransfer: dt, bubbles: true, cancelable: true }));
      footer.dispatchEvent(new DragEvent('drop', { dataTransfer: dt, bubbles: true, cancelable: true }));
      delete window.__attachmentDragTransfer;
    });
    await waitFor(
      page,
      () => document.querySelectorAll('.composer-attachment').length === 5,
      'drop: 2 code chips did not appear (expected 5 total chips)'
    );
    // Drop-active highlight must clear after drop.
    await waitFor(
      page,
      () => document.querySelector('.app-main > footer').dataset.dropActive !== 'true',
      'drop: footer[data-drop-active] did not clear after drop'
    );
    await page.screenshot({ path: `${evidence}/drop.png`, fullPage: true });

    // ------------------------------------------------------------------
    // Chips preserve global intake order:
    // pasted image -> picker .rs -> picker .ts -> drop .rs -> drop .ts
    // ------------------------------------------------------------------
    const allChips = await readChips(page);
    const expectedNames = [PASTE_IMAGE, PICKER_RS, PICKER_TS, DROP_RS, DROP_TS];
    if (allChips.length !== 5) {
      fail(`chip order: expected 5 chips, got ${allChips.length}`);
    }
    const actualNames = allChips.map((c) => c.name);
    for (let i = 0; i < expectedNames.length; i++) {
      if (actualNames[i] !== expectedNames[i]) {
        fail(`chip order: position ${i} expected ${expectedNames[i]}, got ${actualNames[i]} (full: ${JSON.stringify(actualNames)})`);
      }
    }
    if (!allChips[0].isImage) {
      fail('chip order: first chip should be the pasted image (isImage=true)');
    }
    const expectedBadges = ['', 'RS', 'TS', 'RS', 'TS'];
    for (let i = 1; i < expectedNames.length; i++) {
      if (allChips[i].badge !== expectedBadges[i]) {
        fail(`chip order: chip ${i} (${expectedNames[i]}) badge should be ${expectedBadges[i]}, got ${allChips[i].badge}`);
      }
    }

    // ------------------------------------------------------------------
    // PDF picker yields a visible error toast and NO chip. PDFs are
    // unsupported (no content wire, no Web-side text extraction) and are
    // rejected at intake with a specific toast summary.
    // ------------------------------------------------------------------
    const chipCountBeforePdf = await page.evaluate(() => document.querySelectorAll('.composer-attachment').length);
    await page.setInputFiles('input[type=file]', [
      { name: PDF_NAME, mimeType: 'application/pdf', buffer: Buffer.from('%PDF-1.4\n%not a real pdf\n') },
    ]);
    // The error toast must appear with the PDF name and "unsupported".
    await waitFor(
      page,
      (pdf) => {
        const toasts = Array.from(document.querySelectorAll('.toast--error'));
        return toasts.some((t) => t.textContent.includes(pdf) && t.textContent.includes('unsupported'));
      },
      `PDF picker: error toast with "${PDF_NAME}" and "unsupported" did not appear`,
      15000,
      PDF_NAME
    );
    await page.screenshot({ path: `${evidence}/reject.png`, fullPage: true });
    // No new chip should have appeared (PDF was rejected).
    const chipCountAfterPdf = await page.evaluate(() => document.querySelectorAll('.composer-attachment').length);
    if (chipCountAfterPdf !== chipCountBeforePdf) {
      fail(`PDF picker: chip count changed from ${chipCountBeforePdf} to ${chipCountAfterPdf} — rejected PDF should not create a chip`);
    }

    // ------------------------------------------------------------------
    // Send dispatches a prompt RPC — observe the outgoing WS frame.
    // Provider success is NOT required; only that the composer dispatch
    // sends the prompt with the right images + message (deterministic).
    // ------------------------------------------------------------------
    await page.fill('#prompt-input', USER_MESSAGE);
    const sendBefore = sentFrames.length;
    await page.press('#prompt-input', 'Enter');

    let promptFrame = null;
    const deadline = Date.now() + 15000;
    while (Date.now() < deadline) {
      for (let i = sendBefore; i < sentFrames.length; i++) {
        let frame;
        try {
          frame = JSON.parse(sentFrames[i]);
        } catch {
          continue;
        }
        if (frame && frame.type === 'prompt') {
          promptFrame = frame;
          break;
        }
      }
      if (promptFrame) break;
      await page.waitForTimeout(200);
    }
    if (!promptFrame) {
      fail(`send: prompt RPC not observed on outgoing WS (sent ${sentFrames.length - sendBefore} frames after submit; last=${sentFrames.slice(-3).join(' | ') || 'none'})`);
    }

    // 6a. One images block: exactly 1 image entry from the pasted PNG.
    if (!Array.isArray(promptFrame.images)) {
      fail(`send: prompt frame has no images array (got ${typeof promptFrame.images})`);
    }
    if (promptFrame.images.length !== 1) {
      fail(`send: prompt frame should have exactly 1 image block (got ${promptFrame.images.length})`);
    }
    const img = promptFrame.images[0];
    if (img.type !== 'image') {
      fail(`send: images[0].type should be "image" (got ${img.type})`);
    }
    if (img.mimeType !== 'image/png') {
      fail(`send: images[0].mimeType should be "image/png" (got ${img.mimeType})`);
    }
    if (typeof img.data !== 'string' || img.data.length === 0) {
      fail(`send: images[0].data should be a non-empty base64 string (got ${typeof img.data}, len ${img.data?.length ?? 0})`);
    }

    // 6b. Message contains filename + source for ALL code files in chip order.
    const msg = typeof promptFrame.message === 'string' ? promptFrame.message : '';
    if (!msg) {
      fail('send: prompt frame message is empty or not a string');
    }
    const codeFiles = [
      { name: PICKER_RS, content: PICKER_RS_CONTENT },
      { name: PICKER_TS, content: PICKER_TS_CONTENT },
      { name: DROP_RS, content: DROP_RS_CONTENT },
      { name: DROP_TS, content: DROP_TS_CONTENT },
    ];
    // Each code file appears as `File: <name>` followed by its source content.
    for (const cf of codeFiles) {
      const header = `File: ${cf.name}`;
      if (!msg.includes(header)) {
        fail(`send: message missing "${header}" (message excerpt: ${msg.slice(0, 200)}…)`);
      }
      if (!msg.includes(cf.content)) {
        fail(`send: message missing source content for ${cf.name}: "${cf.content}" (message excerpt: ${msg.slice(0, 300)}…)`);
      }
    }
    // Code files must appear in the same order as the chips (strictly increasing
    // index of the `File:` headers).
    const headerPositions = codeFiles.map((cf) => msg.indexOf(`File: ${cf.name}`));
    for (let i = 0; i < headerPositions.length; i++) {
      if (headerPositions[i] < 0) {
        fail(`send: "File: ${codeFiles[i].name}" not found in message`);
      }
      if (i > 0 && headerPositions[i] <= headerPositions[i - 1]) {
        fail(`send: code file order wrong — "${codeFiles[i].name}" at ${headerPositions[i]} should come after "${codeFiles[i - 1].name}" at ${headerPositions[i - 1]}`);
      }
    }
    // The user's typed message appears at the end (after all code segments).
    if (!msg.includes(USER_MESSAGE)) {
      fail(`send: message missing user text "${USER_MESSAGE}" (message excerpt: ${msg.slice(-200)}…)`);
    }
    if (msg.indexOf(USER_MESSAGE) <= headerPositions[headerPositions.length - 1]) {
      fail('send: user text should appear after all code file segments in the message');
    }
    // No PDF content: the rejected PDF must not appear anywhere in the frame.
    if (msg.includes(PDF_NAME)) {
      fail(`send: rejected PDF "${PDF_NAME}" leaked into the prompt message`);
    }
    if (promptFrame.images.some((im) => im.mimeType === 'application/pdf')) {
      fail('send: rejected PDF leaked into the images array');
    }

    // ------------------------------------------------------------------
    // Attachments clear after dispatch.
    // ------------------------------------------------------------------
    await waitFor(
      page,
      () => document.getElementById('composer-attachments') === null,
      'attachments did not clear after dispatch (#composer-attachments still present)'
    );
    await page.screenshot({ path: `${evidence}/dispatch.png`, fullPage: true });

    // ------------------------------------------------------------------
    // Rejection matrix (classification + late-read): oversize file,
    // unsupported binary, aggregate wire budget, attachment-count cap,
    // invalid UTF-8, and the remove-chip budget reconciliation. Every
    // rejection must surface a visible skip toast and NO chip; the mixed
    // batch must group every skip reason into ONE toast summary.
    // ------------------------------------------------------------------

    // 1. Mixed classification rejects: an oversize .rs (> 2 MiB) + an
    //    unknown binary (.bin) — one toast, grouped by reason, no chips.
    const OVERSIZE_BYTES = 2 * 1024 * 1024 + 1;
    await page.setInputFiles('input[type=file]', [
      { name: 'huge.rs', mimeType: 'text/plain', buffer: Buffer.alloc(OVERSIZE_BYTES, 0x61) },
      { name: 'notes.bin', mimeType: 'application/octet-stream', buffer: Buffer.from([0x00, 0x01, 0x02]) },
    ]);
    await waitFor(
      page,
      () => {
        const toasts = Array.from(document.querySelectorAll('.toast--error'));
        return toasts.some(
          (t) =>
            t.textContent.includes('Skipped 2 file(s)') &&
            t.textContent.includes('over 2097152 bytes') &&
            t.textContent.includes('huge.rs') &&
            t.textContent.includes('unsupported') &&
            t.textContent.includes('notes.bin')
        );
      },
      'reject matrix: mixed oversize+unsupported batch never produced ONE grouped skip toast',
      15000
    );
    const chipsAfterRejects = await page.evaluate(() => document.querySelectorAll('.composer-attachment').length);
    if (chipsAfterRejects !== 0) {
      fail(`reject matrix: rejected batch created ${chipsAfterRejects} chips (expected 0)`);
    }

    // 2. Aggregate wire budget: two 1.6 MiB code files — the first fits, the
    //    second exceeds MAX_TOTAL_WIRE_BYTES (3 MiB) and is skipped, so only
    //    one chip may appear. Covers wireFootprint + the over-budget branch.
    const BUDGET_FILE = Buffer.alloc(1600000, 0x62); // ~1.53 MiB text
    await page.setInputFiles('input[type=file]', [
      { name: 'budget-a.rs', mimeType: 'text/plain', buffer: BUDGET_FILE },
      { name: 'budget-b.rs', mimeType: 'text/plain', buffer: BUDGET_FILE },
    ]);
    await waitFor(
      page,
      () => document.querySelectorAll('.composer-attachment').length === 1,
      'reject matrix: over-budget batch should queue exactly 1 chip (budget-a.rs)'
    );
    const budgetChips = await readChips(page);
    if (budgetChips.length !== 1 || budgetChips[0].name !== 'budget-a.rs') {
      fail(`reject matrix: over-budget batch queued the wrong chip(s): ${JSON.stringify(budgetChips)}`);
    }
    await waitFor(
      page,
      () => {
        const toasts = Array.from(document.querySelectorAll('.toast--error'));
        return toasts.some((t) => t.textContent.includes('exceeds total size limit') && t.textContent.includes('budget-b.rs'));
      },
      'reject matrix: over-budget toast (budget-b.rs exceeds total size limit) never appeared',
      15000
    );

    // 3. Count cap + remove-chip budget reconciliation: fill to
    //    MAX_ATTACHMENTS (8), the 9th is skipped with a too-many toast;
    //    removing one chip frees the slot so a new file is accepted; then
    //    remove every chip (each Remove button works and the strip leaves
    //    the DOM once empty).
    const FILL_FILES = Array.from({ length: 7 }, (_, i) => ({
      name: `fill-${i}.txt`,
      mimeType: 'text/plain',
      buffer: Buffer.from(`fill file ${i}`),
    }));
    await page.setInputFiles('input[type=file]', FILL_FILES);
    await waitFor(
      page,
      () => document.querySelectorAll('.composer-attachment').length === 8,
      'count cap: 8 chips (1 budget + 7 fill) never appeared'
    );
    await page.setInputFiles('input[type=file]', [
      { name: 'nine.txt', mimeType: 'text/plain', buffer: Buffer.from('the ninth file') },
    ]);
    await waitFor(
      page,
      () => {
        const toasts = Array.from(document.querySelectorAll('.toast--error'));
        return toasts.some((t) => t.textContent.includes('over 8 attachments') && t.textContent.includes('nine.txt'));
      },
      'count cap: too-many toast (over 8 attachments, nine.txt) never appeared',
      15000
    );
    const chipsAfterTooMany = await page.evaluate(() => document.querySelectorAll('.composer-attachment').length);
    if (chipsAfterTooMany !== 8) {
      fail(`count cap: 9th file should be rejected (chips ${chipsAfterTooMany}, expected 8)`);
    }
    // Remove one chip (the budget-a.rs chip) via its Remove button.
    await page.click('.composer-attachment .composer-attachment__remove');
    await waitFor(
      page,
      () => document.querySelectorAll('.composer-attachment').length === 7,
      'remove chip: first chip never left the queue'
    );
    const chipsAfterRemove = await readChips(page);
    if (chipsAfterRemove.some((c) => c.name === 'budget-a.rs')) {
      fail(`remove chip: budget-a.rs still queued after its Remove button: ${JSON.stringify(chipsAfterRemove)}`);
    }
    // The freed count slot must be reusable (intake budget reconciled).
    await page.setInputFiles('input[type=file]', [
      { name: 'refill.txt', mimeType: 'text/plain', buffer: Buffer.from('refill after remove') },
    ]);
    await waitFor(
      page,
      () => document.querySelectorAll('.composer-attachment').length === 8,
      'remove chip: the freed attachment slot was NOT reusable (refill.txt rejected)'
    );
    const refillChips = await readChips(page);
    if (!refillChips.some((c) => c.name === 'refill.txt')) {
      fail(`remove chip: refill.txt never queued after the slot freed: ${JSON.stringify(refillChips)}`);
    }
    // Remove every chip -> the attachment strip leaves the DOM.
    while (await page.locator('.composer-attachment').count()) {
      await page.click('.composer-attachment:first-child .composer-attachment__remove');
    }
    await waitFor(
      page,
      () => document.getElementById('composer-attachments') === null,
      'remove chip: removing the last chip did not drop #composer-attachments from the DOM'
    );
    await page.screenshot({ path: `${evidence}/reject-matrix.png`, fullPage: true });

    // 4. Late-read reject: a valid .py + a binary-garbage .py in one batch —
    //    the valid file queues, the invalid one is a late invalid-UTF-8 skip
    //    (buildCodeAttachment decodeUtf8OrReject branch).
    await page.setInputFiles('input[type=file]', [
      { name: 'good.py', mimeType: 'text/plain', buffer: Buffer.from('def ok():\n    return 1\n') },
      { name: 'bad.py', mimeType: 'text/plain', buffer: Buffer.from([0xff, 0xfe, 0x00, 0x80, 0xc3, 0x28]) },
    ]);
    await waitFor(
      page,
      () => {
        const chips = [...document.querySelectorAll('.composer-attachment')];
        return chips.length === 1 && (chips[0].querySelector('.composer-attachment__name')?.textContent || '').trim() === 'good.py';
      },
      'late reject: good.py chip never appeared (only the valid file should queue)'
    );
    await waitFor(
      page,
      () => {
        const toasts = Array.from(document.querySelectorAll('.toast--error'));
        return toasts.some((t) => t.textContent.includes('not valid UTF-8') && t.textContent.includes('bad.py'));
      },
      'late reject: invalid-UTF-8 toast (bad.py) never appeared',
      15000
    );
    await page.click('.composer-attachment .composer-attachment__remove');
    await waitFor(
      page,
      () => document.getElementById('composer-attachments') === null,
      'late reject: removing good.py did not clear the strip'
    );

    // 5. Image MIME/extension fallback: a pasted screenshot whose File type
    //    is EMPTY must still classify as an image via the extension fallback
    //    (imageMimeType -> image/png) and render a thumbnail chip; removing
    //    it clears the strip (composer empty for the next phase).
    await page.evaluate(() => {
      const canvas = document.createElement('canvas');
      canvas.width = 1;
      canvas.height = 1;
      const ctx = canvas.getContext('2d');
      ctx.fillStyle = 'rgb(0,0,255)';
      ctx.fillRect(0, 0, 1, 1);
      const dataUrl = canvas.toDataURL('image/png');
      const base64 = dataUrl.split(',')[1];
      const binStr = atob(base64);
      const bytes = new Uint8Array(binStr.length);
      for (let i = 0; i < binStr.length; i++) bytes[i] = binStr.charCodeAt(i);
      const file = new File([bytes], 'shot.png', { type: '' });
      const dt = new DataTransfer();
      dt.items.add(file);
      document.getElementById('prompt-input').dispatchEvent(
        new ClipboardEvent('paste', { clipboardData: dt, bubbles: true, cancelable: true })
      );
    });
    await waitFor(
      page,
      () => {
        const chips = [...document.querySelectorAll('.composer-attachment')];
        return chips.length === 1 && chips[0].querySelector('.composer-attachment__thumb') !== null;
      },
      'extension fallback: type-less shot.png never queued as an image chip'
    );
    await page.click('.composer-attachment .composer-attachment__remove');
    await waitFor(
      page,
      () => document.getElementById('composer-attachments') === null,
      'extension fallback: removing shot.png did not clear the strip'
    );

    // The sent user bubble renders the image thumbnail INLINE (not the old
    // "(image attached)" placeholder, not the raw <attachment> wrapper),
    // image preview first then the user's caption — the wire payload the
    // prompt frame carried, visible immediately on the bubble.
    // ------------------------------------------------------------------
    await waitFor(
      page,
      () => {
        const bubbles = document.querySelectorAll('.msg--user');
        if (bubbles.length !== 1) return false;
        const thumbs = bubbles[0].querySelectorAll('.msg--user__images .msg--user__image');
        return thumbs.length === 1
          && (thumbs[0].getAttribute('src') || '').startsWith('data:image/png;base64,');
      },
      'user bubble: the pasted image thumbnail did not render inline after send'
    );
    const firstBubble = await page.evaluate(() => {
      const bubble = document.querySelector('.msg--user');
      return {
        text: bubble ? bubble.textContent : '',
        thumbs: bubble ? bubble.querySelectorAll('.msg--user__image').length : -1,
        grid: bubble ? bubble.querySelectorAll('.msg--user__images').length : -1,
      };
    });
    if (!firstBubble.text.includes(USER_MESSAGE)) {
      fail(`user bubble: typed text "${USER_MESSAGE}" missing after send (text: ${JSON.stringify(firstBubble.text.slice(0, 120))})`);
    }
    if (firstBubble.thumbs !== 1 || firstBubble.grid !== 1) {
      fail(`user bubble: expected 1 thumbnail in 1 grid after text+image send (thumbs=${firstBubble.thumbs} grid=${firstBubble.grid})`);
    }
    if (firstBubble.text.includes('(image attached)')) {
      fail('user bubble: the "(image attached)" placeholder still renders for image sends');
    }
    if (firstBubble.text.includes('<attachment') || firstBubble.text.includes('</attachment>')) {
      fail('user bubble: the raw <attachment> transport wrapper still renders for image sends');
    }
    if (firstBubble.text.includes('[Image analyzed by')) {
      fail('user bubble: an auto-vision [Image analyzed by …] marker leaked into the user caption');
    }
    await page.screenshot({ path: `${evidence}/user-bubble-image.png`, fullPage: true });

    // ------------------------------------------------------------------
    // Multi-image image-only send: paste TWO more images together, dispatch
    // with NO typed text. The bubble must show both thumbnails in order and
    // NO placeholder text. Wait for the first run's assistant reply first
    // (the mock streams request 1 slowly by design) so the second prompt is
    // admitted.
    // ------------------------------------------------------------------
    await waitFor(
      page,
      () => {
        const assistants = document.querySelectorAll('.msg--assistant');
        return assistants.length === 1 && (assistants[0].textContent || '').includes('four-done');
      },
      'first run: assistant reply did not finalize before the image-only send'
    );
    await page.evaluate(() => {
      const canvas = document.createElement('canvas');
      canvas.width = 1;
      canvas.height = 1;
      const ctx = canvas.getContext('2d');
      ctx.fillStyle = 'rgb(0,255,0)';
      ctx.fillRect(0, 0, 1, 1);
      const greenDataUrl = canvas.toDataURL('image/png');
      const greenBase64 = greenDataUrl.split(',')[1];
      const greenBytes = new Uint8Array(atob(greenBase64).length);
      for (let i = 0; i < greenBytes.length; i++) greenBytes[i] = atob(greenBase64).charCodeAt(i);

      const canvas2 = document.createElement('canvas');
      canvas2.width = 1;
      canvas2.height = 1;
      const ctx2 = canvas2.getContext('2d');
      ctx2.fillStyle = 'rgb(0,0,255)';
      ctx2.fillRect(0, 0, 1, 1);
      const blueDataUrl = canvas2.toDataURL('image/png');
      const blueBase64 = blueDataUrl.split(',')[1];
      const blueBytes = new Uint8Array(atob(blueBase64).length);
      for (let i = 0; i < blueBytes.length; i++) blueBytes[i] = atob(blueBase64).charCodeAt(i);

      const dt = new DataTransfer();
      dt.items.add(new File([greenBytes], 'green.png', { type: 'image/png' }));
      dt.items.add(new File([blueBytes], 'blue.png', { type: 'image/png' }));
      document.getElementById('prompt-input').dispatchEvent(
        new ClipboardEvent('paste', { clipboardData: dt, bubbles: true, cancelable: true })
      );
    });
    await waitFor(
      page,
      () => document.querySelectorAll('.composer-attachment').length === 2,
      'image-only send: 2 image chips did not appear after multi-image paste'
    );
    await page.press('#prompt-input', 'Enter');

    // Both thumbnails render in the new (image-only) bubble; total user
    // bubbles must be exactly 2 — the ACK/history reconcile must not
    // duplicate or flicker the optimistic bubble away.
    await waitFor(
      page,
      () => {
        const bubbles = document.querySelectorAll('.msg--user');
        if (bubbles.length !== 2) return false;
        const last = bubbles[1];
        return last.querySelectorAll('.msg--user__image').length === 2
          && (last.querySelector('.msg--user__image')?.getAttribute('src') || '').startsWith('data:image/png;base64,');
      },
      'image-only send: 2-thumbnail bubble did not render (or user bubbles duplicated)'
    );
    const secondBubble = await page.evaluate(() => {
      const bubbles = document.querySelectorAll('.msg--user');
      const last = bubbles[bubbles.length - 1];
      return {
        text: last ? last.textContent : '',
        thumbs: last ? last.querySelectorAll('.msg--user__image').length : -1,
        srcs: last ? Array.from(last.querySelectorAll('.msg--user__image')).map((img) => img.getAttribute('src')) : [],
      };
    });
    if (secondBubble.thumbs !== 2) {
      fail(`image-only bubble: expected 2 thumbnails (got ${secondBubble.thumbs})`);
    }
    if (secondBubble.srcs[0] === secondBubble.srcs[1]) {
      fail('image-only bubble: two distinct pasted images collapsed to one thumbnail');
    }
    if (secondBubble.text.trim() !== '') {
      fail(`image-only bubble: expected NO text (got ${JSON.stringify(secondBubble.text.slice(0, 80))})`);
    }
    if (secondBubble.text.includes('(image attached)')) {
      fail('image-only bubble: "(image attached)" placeholder still rendered');
    }
    await page.screenshot({ path: `${evidence}/multi-image-bubble.png`, fullPage: true });

    // ------------------------------------------------------------------
    // Mobile: at a phone viewport the multi-image grid must not overflow
    // horizontally (the grid shrinks inside the bubble).
    // ------------------------------------------------------------------
    await page.setViewportSize({ width: 375, height: 667 });
    await page.waitForTimeout(300);
    const mobileMetrics = await page.evaluate(() => ({
      scrollWidth: document.documentElement.scrollWidth,
      innerWidth: window.innerWidth,
    }));
    if (mobileMetrics.scrollWidth > mobileMetrics.innerWidth + 1) {
      fail(`mobile: horizontal overflow with multi-image grid (scrollWidth ${mobileMetrics.scrollWidth} > viewport ${mobileMetrics.innerWidth})`);
    }
    await page.screenshot({ path: `${evidence}/multi-image-mobile.png`, fullPage: true });
    await page.setViewportSize({ width: 1280, height: 800 });

    // ------------------------------------------------------------------
    // Reload the page: the restored transcript must render BOTH user
    // bubbles' image thumbnails directly from get_messages history — the
    // backend persists the prompt's image ContentBlocks and the restore
    // path (messagesToItems) must render them.
    // ------------------------------------------------------------------
    await waitFor(
      page,
      () => (document.querySelectorAll('.msg--assistant')[1]?.textContent || '').includes('steering-followup-reply'),
      'second run: assistant reply did not finalize before reload'
    );
    await page.reload({ waitUntil: 'domcontentloaded' });
    await waitFor(page, () => document.title === 'rpi web', 'page title missing after reload');
    await waitFor(
      page,
      () => document.getElementById('conn-state')?.dataset.state === 'on',
      'WS did not reconnect after reload'
    );
    await waitFor(
      page,
      () => document.querySelectorAll('.msg--user').length === 2,
      'restore: expected 2 user bubbles after reload (image thumbnails must survive reload)'
    );
    const restored = await page.evaluate(() => {
      const bubbles = Array.from(document.querySelectorAll('.msg--user'));
      return bubbles.map((bubble) => ({
        text: bubble.textContent || '',
        thumbs: bubble.querySelectorAll('.msg--user__image').length,
      }));
    });
    if (restored.length !== 2) {
      fail(`restore: expected 2 user bubbles (got ${restored.length})`);
    }
    if (restored[0].thumbs !== 1 || !restored[0].text.includes(USER_MESSAGE)) {
      fail(`restore: first user bubble should carry 1 thumbnail + text (got ${JSON.stringify(restored[0])})`);
    }
    if (restored[1].thumbs !== 2 || restored[1].text.trim() !== '') {
      fail(`restore: second user bubble should carry 2 thumbnails and no text (got ${JSON.stringify(restored[1])})`);
    }
    if (restored.some((b) => b.text.includes('(image attached)'))) {
      fail('restore: "(image attached)" placeholder still rendered after reload');
    }
    await page.screenshot({ path: `${evidence}/restore.png`, fullPage: true });

    // ------------------------------------------------------------------
    // User-caption Markdown: the typed prompt's TEXT renders through the
    // SHARED renderMarkdown pipeline (MarkdownBody — the same component the
    // collab guest view renders), never as pre-wrapped plain text. A real
    // Chromium send of heading/list/bold/inline-code/fence/hostile HTML
    // must produce structural .md-* tags, keep hostile elements inert
    // literal text, show NO whole-message pre wrapper, keep images BEFORE
    // the caption, and survive reload (restored history renders Markdown).
    // ------------------------------------------------------------------
    const assertUserMarkdownBubble = async (page, bubbleIndex, label) => {
      const bubble = await page.evaluate((idx) => {
        const bubbles = document.querySelectorAll('.msg--user');
        const b = bubbles[idx];
        if (!b) return null;
        const textEl = b.querySelector('.msg--user__text');
        if (!textEl) return null;
        return {
          h2: textEl.querySelector('.md-h2')?.textContent?.trim() || '',
          listItems: Array.from(textEl.querySelectorAll('ul.md-list > li')).map((li) => li.textContent.trim()),
          strongs: Array.from(textEl.querySelectorAll('strong')).map((s) => s.textContent.trim()),
          codes: Array.from(textEl.querySelectorAll('code.md-code')).map((c) => c.textContent.trim()),
          fenceLang: textEl.querySelector('.md-fence__lang')?.textContent?.trim() || '',
          fenceCode: textEl.querySelector('.md-fence__pre code')?.textContent || '',
          fenceCopyCount: textEl.querySelectorAll('.md-fence__copy').length,
          firstChildIsPre: textEl.firstElementChild?.tagName === 'PRE',
          directPreWrapper: textEl.querySelector(':scope > pre') !== null,
          hostileCount: textEl.querySelectorAll('script, iframe, object, embed, img[onerror]').length,
          whiteSpace: getComputedStyle(textEl).whiteSpace,
          textContent: b.textContent || '',
        };
      }, bubbleIndex);
      if (!bubble) {
        fail(`user markdown (${label}): bubble ${bubbleIndex} or its .msg--user__text missing`);
      }
      if (bubble.h2 !== 'User markdown e2e') {
        fail(`user markdown (${label}): heading did not render as .md-h2 (got ${JSON.stringify(bubble.h2)})`);
      }
      if (bubble.listItems.length !== 3 || bubble.listItems[0] !== 'item one') {
        fail(`user markdown (${label}): list items wrong (got ${JSON.stringify(bubble.listItems)})`);
      }
      if (!bubble.listItems[1].includes('bold item') || !bubble.listItems[2].includes('inline code')) {
        fail(`user markdown (${label}): list item content missing (got ${JSON.stringify(bubble.listItems)})`);
      }
      if (!bubble.strongs.includes('bold item') || !bubble.strongs.includes('bold')) {
        fail(`user markdown (${label}): **bold** did not render as <strong> (got ${JSON.stringify(bubble.strongs)})`);
      }
      if (!bubble.codes.includes('inline code') || !bubble.codes.includes('code')) {
        fail(`user markdown (${label}): inline ` + '`code`' + ` did not render as .md-code (got ${JSON.stringify(bubble.codes)})`);
      }
      if (bubble.fenceLang !== 'ts' && bubble.fenceLang !== 'typescript') {
        fail(`user markdown (${label}): fenced block lang label wrong (lang=${JSON.stringify(bubble.fenceLang)} code=${JSON.stringify(bubble.fenceCode.slice(0, 60))})`);
      }
      if (!bubble.fenceCode.includes('const x: number = 1;')) {
        fail(`user markdown (${label}): fenced block body missing (code=${JSON.stringify(bubble.fenceCode.slice(0, 60))})`);
      }
      if (bubble.fenceCopyCount !== 1) {
        fail(`user markdown (${label}): fence copy button missing (count=${bubble.fenceCopyCount})`);
      }
      if (bubble.hostileCount !== 0) {
        fail(`user markdown (${label}): hostile elements rendered (script/iframe/object/embed/img[onerror] count=${bubble.hostileCount})`);
      }
      if (bubble.firstChildIsPre || bubble.directPreWrapper) {
        fail(`user markdown (${label}): whole-message pre wrapper still rendered (firstChildIsPre=${bubble.firstChildIsPre} directPre=${bubble.directPreWrapper})`);
      }
      if (bubble.whiteSpace === 'pre-wrap') {
        fail(`user markdown (${label}): .msg--user__text still pre-wraps (white-space=${bubble.whiteSpace})`);
      }
      if (!bubble.textContent.includes('<script>alert(1)</script>')) {
        fail(`user markdown (${label}): hostile <script> text did not stay literal`);
      }
      if (!bubble.textContent.includes('<img src="x" onerror="alert(2)">')) {
        fail(`user markdown (${label}): hostile <img> text did not stay literal`);
      }
    };

    await page.fill('#prompt-input', USER_MARKDOWN);
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      () => document.querySelectorAll('.msg--user').length === 3,
      'user markdown: third user bubble did not appear after send'
    );
    await waitFor(
      page,
      () => {
        const bubbles = document.querySelectorAll('.msg--user');
        const last = bubbles[bubbles.length - 1];
        return last !== undefined && last.querySelector('.msg--user__text .md-h2') !== null;
      },
      'user markdown: heading did not render structurally in the sent bubble'
    );
    await assertUserMarkdownBubble(page, 2, 'sent');
    // Image-caption order must NOT regress: bubble 0 keeps the attachment
    // preview BEFORE the markdown caption.
    const imageCaptionOrder = await page.evaluate(() => {
      const bubble = document.querySelector('.msg--user');
      const images = bubble?.querySelector('.msg--user__images');
      const text = bubble?.querySelector('.msg--user__text');
      return images !== null && text !== null && (images.compareDocumentPosition(text) & Node.DOCUMENT_POSITION_FOLLOWING) !== 0;
    });
    if (!imageCaptionOrder) {
      fail('user markdown: image-caption order regressed — images must render BEFORE the caption');
    }
    await page.screenshot({ path: `${evidence}/user-markdown.png`, fullPage: true });

    // Wait for the mock's odd-request slow stream (steer-3-…-four-done) to
    // finalize before reload, then assert the RESTORED caption still renders
    // as Markdown (reload 仍 Markdown) and the image bubbles keep order.
    await waitFor(
      page,
      () => (document.querySelectorAll('.msg--assistant')[2]?.textContent || '').includes('four-done'),
      'user markdown: third assistant reply did not finalize before reload'
    );
    await page.reload({ waitUntil: 'domcontentloaded' });
    await waitFor(page, () => document.title === 'rpi web', 'page title missing after markdown reload');
    await waitFor(
      page,
      () => document.getElementById('conn-state')?.dataset.state === 'on',
      'WS did not reconnect after markdown reload'
    );
    await waitFor(
      page,
      () => document.querySelectorAll('.msg--user').length === 3,
      'restore: expected 3 user bubbles after markdown reload'
    );
    await assertUserMarkdownBubble(page, 2, 'restored');
    const restoredOrder = await page.evaluate(() => {
      const bubbles = Array.from(document.querySelectorAll('.msg--user'));
      return bubbles.map((b) => {
        const images = b.querySelector('.msg--user__images');
        const text = b.querySelector('.msg--user__text');
        return {
          thumbs: b.querySelectorAll('.msg--user__image').length,
          orderOk: !text || (images !== null && (images.compareDocumentPosition(text) & Node.DOCUMENT_POSITION_FOLLOWING) !== 0),
        };
      });
    });
    if (restoredOrder.length !== 3 || !restoredOrder[0].orderOk || !restoredOrder[1].orderOk || restoredOrder[2].thumbs !== 0) {
      fail(`restore: image/caption order regressed after markdown reload (got ${JSON.stringify(restoredOrder)})`);
    }
    if (restoredOrder[0].thumbs !== 1 || restoredOrder[1].thumbs !== 2) {
      fail(`restore: thumbnail counts regressed after markdown reload (got ${JSON.stringify(restoredOrder)})`);
    }
    await page.screenshot({ path: `${evidence}/user-markdown-restore.png`, fullPage: true });

    // ------------------------------------------------------------------
    // Branch matrix: codeLanguage switch cases (one file per extension —
    // each distinct extension takes a different switch branch in
    // attachments.ts codeLanguage), text-type fallbacks, TEXT_BASENAMES,
    // the app/* text-type branch, and the sanitizeFileName + safeFence
    // wire contracts. MAX_ATTACHMENTS=8 caps each batch, so the matrix is
    // uploaded in batches of 8 and the chips removed between batches (the
    // remove path also re-exercises removeAttachment + budget reconcile).
    // ------------------------------------------------------------------
    const removeAllChips = async () => {
      while (await page.locator('.composer-attachment').count()) {
        await page.click('.composer-attachment:first-child .composer-attachment__remove');
      }
      await waitFor(
        page,
        () => document.getElementById('composer-attachments') === null,
        'branch matrix: chip strip never cleared between batches'
      );
    };
    const uploadBatch = async (names) => {
      await page.setInputFiles(
        'input[type=file]',
        names.map((name) => ({ name, mimeType: 'text/plain', buffer: Buffer.from(`content of ${name}`) }))
      );
      await waitFor(
        page,
        (n) => document.querySelectorAll('.composer-attachment').length === n,
        `branch matrix: batch of ${names.length} never fully queued (${names.join(', ')})`,
        20000,
        names.length
      );
      const chips = await readChips(page);
      const chipNames = chips.map((c) => c.name);
      for (const name of names) {
        if (!chipNames.includes(name)) {
          fail(`branch matrix: chip ${name} missing after batch upload (got ${JSON.stringify(chipNames)})`);
        }
      }
      await removeAllChips();
    };
    // The full codeLanguage switch surface: one distinct extension per file
    // (TEXT_EXTENSIONS), so every case branch executes through the real
    // intake -> buildCodeSegment path.
    const LANG_BATCHES = [
      ['a.rs', 'b.ts', 'c.tsx', 'd.js', 'e.mjs', 'f.cjs', 'g.jsx', 'h.py'],
      ['i.go', 'j.java', 'k.kt', 'l.kts', 'm.c', 'n.h', 'o.cc', 'p.cpp'],
      ['q.cxx', 'r.hpp', 's.hxx', 't.hh', 'u.cs', 'v.rb', 'w.php', 'x.swift'],
      ['y.scala', 'z.clj', 'aa.cljs', 'ab.ex', 'ac.exs', 'ad.erl', 'ae.hs', 'af.ml'],
      ['ag.fs', 'ah.fsx', 'ai.lua', 'aj.pl', 'ak.pm', 'al.r', 'am.dart', 'an.groovy'],
      ['ao.gradle', 'ap.jl', 'aq.nim', 'ar.cr', 'as.zig', 'at.v', 'au.sv', 'av.elm'],
      ['aw.purs', 'ax.sql', 'ay.sh', 'az.bash', 'ba.zsh', 'bb.fish', 'bc.ps1', 'bd.bat'],
      ['be.cmd', 'bf.html', 'bg.htm', 'bh.css', 'bi.scss', 'bj.sass', 'bk.less', 'bl.xml'],
      ['bm.vue', 'bn.svelte', 'bo.astro', 'bp.json', 'bq.json5', 'br.jsonc', 'bs.yaml', 'bt.yml'],
      ['bu.toml', 'bv.ini', 'bw.cfg', 'bx.conf', 'by.env', 'bz.properties', 'ca.csv', 'cb.tsv'],
      ['cc.md', 'cd.markdown', 'ce.rst', 'cf.adoc', 'cg.tex', 'ch.txt', 'ci.log', 'cj.lock'],
    ];
    for (const batch of LANG_BATCHES) {
      await uploadBatch(batch);
    }
    // TEXT_BASENAMES (Dockerfile/Makefile -> dockerfile/makefile language
    // hints; .gitignore -> ''), the app/* text-type branch (unknown
    // extension but application/json type), and a type-less .svg (xml).
    await page.setInputFiles('input[type=file]', [
      { name: 'Dockerfile', mimeType: 'text/plain', buffer: Buffer.from('FROM scratch') },
      { name: 'Makefile', mimeType: 'text/plain', buffer: Buffer.from('all:\n\techo hi') },
      { name: '.gitignore', mimeType: 'text/plain', buffer: Buffer.from('node_modules') },
      { name: 'notes.env', mimeType: 'text/plain', buffer: Buffer.from('A=1') },
      { name: 'data.weird', mimeType: 'application/json', buffer: Buffer.from('{"a":1}') },
      { name: 'pic.svg', mimeType: 'image/svg+xml', buffer: Buffer.from('<svg/>') },
      { name: 'raw.txt', mimeType: 'application/octet-stream', buffer: Buffer.from('binary-ish') },
      { name: 'plain.doc', mimeType: 'text/plain', buffer: Buffer.from('doc text') },
    ]);
    await waitFor(
      page,
      () => document.querySelectorAll('.composer-attachment').length === 8,
      'branch matrix: basenames/type-fallback batch never fully queued'
    );
    const fallbackChips = await readChips(page);
    const fallbackNames = fallbackChips.map((c) => c.name);
    for (const want of ['Dockerfile', 'Makefile', '.gitignore', 'notes.env', 'data.weird', 'pic.svg', 'raw.txt', 'plain.doc']) {
      if (!fallbackNames.includes(want)) {
        fail(`branch matrix: fallback batch missing ${want} (got ${JSON.stringify(fallbackNames)})`);
      }
    }
    const dockerfileBadge = fallbackChips.find((c) => c.name === 'Dockerfile')?.badge;
    const gitignoreBadge = fallbackChips.find((c) => c.name === '.gitignore')?.badge;
    if (dockerfileBadge !== 'TXT' || gitignoreBadge !== 'TXT') {
      fail(`branch matrix: basename badges wrong (Dockerfile=${dockerfileBadge}, .gitignore=${gitignoreBadge})`);
    }
    const weirdChip = fallbackChips.find((c) => c.name === 'data.weird');
    if (!weirdChip || weirdChip.isImage) {
      fail('branch matrix: application/json unknown-extension file did not queue as a code chip');
    }
    // The badge is a file's extension uppercased; Dockerfile/.gitignore have
    // no extension so they get TXT — the codeBadgeLabel else branch.
    await page.screenshot({ path: `${evidence}/branch-matrix.png`, fullPage: true });
    await removeAllChips();

    // ------------------------------------------------------------------
    // sanitizeFileName + safeFence wire contract: a control-char filename
    // (embedded newline) must collapse to one bounded line in the File:
    // header, and a backtick-heavy body must grow the fence past the
    // longest backtick run so the fenced block can never break out.
    // ------------------------------------------------------------------
    const EVIL_NAME = 'evil\nname.rs';
    const BACKTICK_BODY = 'fn evil() { /* ' + '`'.repeat(5) + ' */ }';
    await page.setInputFiles('input[type=file]', [
      { name: EVIL_NAME, mimeType: 'text/plain', buffer: Buffer.from(BACKTICK_BODY) },
    ]);
    await waitFor(
      page,
      () => {
        const chips = [...document.querySelectorAll('.composer-attachment')];
        return chips.length === 1 && (chips[0].querySelector('.composer-attachment__name')?.textContent || '').includes('evil');
      },
      'sanitize: control-char-named file never queued as a chip'
    );
    await page.fill('#prompt-input', 'final branch send');
    const sendBeforeFinal = sentFrames.length;
    await page.press('#prompt-input', 'Enter');
    let finalFrame = null;
    const finalDeadline = Date.now() + 15000;
    while (Date.now() < finalDeadline) {
      for (let i = sendBeforeFinal; i < sentFrames.length; i++) {
        let frame;
        try {
          frame = JSON.parse(sentFrames[i]);
        } catch {
          continue;
        }
        if (frame && frame.type === 'prompt') {
          finalFrame = frame;
          break;
        }
      }
      if (finalFrame) break;
      await page.waitForTimeout(200);
    }
    if (!finalFrame) fail('sanitize: final prompt RPC not observed on the outgoing WS');
    const finalMsg = typeof finalFrame.message === 'string' ? finalFrame.message : '';
    // sanitizeFileName: the embedded newline collapses to a space, the name
    // stays one line, and the fenced block carries the grown fence (longest
    // run 5 -> fence 6) with the language hint.
    if (!finalMsg.includes('File: evil name.rs')) {
      fail(`sanitize: File: header did not collapse the control-char name (message excerpt: ${finalMsg.slice(0, 120)})`);
    }
    if (finalMsg.includes('\nname.rs')) {
      fail('sanitize: control-char name leaked a raw newline into the wire header');
    }
    if (!finalMsg.includes('`'.repeat(6)) || finalMsg.includes('`'.repeat(7))) {
      fail('sanitize: safeFence did not grow past the 5-backtick run in the body');
    }
    if (!finalMsg.includes('rust')) {
      fail('sanitize: .rs language hint missing from the fenced block');
    }
    // The chips must clear after the send (success clear by id).
    await waitFor(
      page,
      () => document.getElementById('composer-attachments') === null,
      'sanitize: chips did not clear after the final send'
    );
    await page.screenshot({ path: `${evidence}/sanitize-wire.png`, fullPage: true });

    console.log('web-attachments: PASSED (paste image canceled + chip; text-only paste not canceled; picker 2 code files RS/TS; drop 2 code files + drop-active highlight; chips preserve global intake order; PDF rejection toast + no chip; outgoing prompt frame: 1 image block + code files in order + no PDF; attachments cleared after dispatch; user bubble renders the image thumbnail inline with text — no "(image attached)"; image-only multi-image send renders 2 distinct thumbnails with no text; mobile viewport has no horizontal overflow with the multi-image grid; reload restores both user bubbles with their thumbnails; user-caption Markdown (heading/list/bold/inline code/fence/hostile) renders structurally with no pre wrapper and hostile HTML inert — images stay before the caption and reload still renders Markdown; rejection matrix (oversize/over-budget/too-many/invalid-UTF-8/remove/extension-fallback); codeLanguage switch matrix + basenames/app-type fallbacks; sanitizeFileName + safeFence wire contract)');
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-attachments: FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});