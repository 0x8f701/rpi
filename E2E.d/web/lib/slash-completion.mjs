// Shared composer journey contract for lanes whose slash command is staged
// into the composer and dispatched with Enter.
//
// The inline autocomplete popup OWNS the selecting Enter: the composer's
// capture-phase listener consumes it, settles the draft on the picked token
// and dismisses the popup WITHOUT dispatching
// (crates/pi-cli/web/src/CommandPicker.tsx:277-308, acceptInline :244-257).
// Dispatch is an INDEPENDENT Enter on the settled token (App.tsx:9647-9656).
//
// This helper drives that journey on REAL keys — ArrowUp/Down move the active
// candidate onto the target, the first Enter selects, the second dispatches —
// and proves it on the lane's own outgoing-frame log: ZERO `dispatchType`
// frames after the selection, exactly ONE after the dispatch. Background
// catalog/heartbeat frames are other types and never count.
//
// Frozen interface: (page, { token, label, option?, sentFrames, dispatchType,
// timeoutMs? }).

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/** Raw payload of a logged outgoing frame (lanes log strings or {raw}). */
const payloadText = (entry) => (typeof entry === 'string' ? entry : entry?.raw ?? '');

/** Wire type of a frame payload, or null for non-JSON payloads. */
function frameType(payload) {
  try {
    const parsed = JSON.parse(payload);
    return parsed && typeof parsed.type === 'string' ? parsed.type : null;
  } catch {
    return null;
  }
}

export async function selectCompletionThenSubmit(page, options) {
  const { token, label, option = token, sentFrames, dispatchType, timeoutMs = 25000 } = options || {};
  if (!token || !label) throw new Error('selectCompletionThenSubmit: token and label are required');
  if (!Array.isArray(sentFrames) || !dispatchType) {
    throw new Error(`${label}: sentFrames and dispatchType are required (the selecting Enter must be proven wire-silent)`);
  }
  const types = new Set(Array.isArray(dispatchType) ? dispatchType : [dispatchType]);
  const sentBefore = sentFrames.length;
  const dispatched = () => sentFrames.slice(sentBefore)
    .filter((entry) => types.has(frameType(payloadText(entry)))).length;

  // 1. The inline popup must offer the target candidate.
  try {
    await page.waitForFunction(
      (want) => Array.from(document.querySelectorAll('.command-picker__popover [role="option"]'))
        .some((row) => (row.querySelector('.command-picker__name')?.textContent || '').trim() === want),
      option,
      { timeout: timeoutMs },
    );
  } catch {
    throw new Error(`${label}: the inline completion popup never offered ${JSON.stringify(option)} as a candidate`);
  }

  // 2. Real ArrowUp/Down move the ACTIVE candidate onto the target (never a DOM click).
  for (let guard = 0; guard < 40; guard += 1) {
    const pick = await page.evaluate((want) => {
      const rows = Array.from(document.querySelectorAll('.command-picker__popover [role="option"]'));
      const target = rows.findIndex((row) => (row.querySelector('.command-picker__name')?.textContent || '').trim() === want);
      const active = rows.findIndex((row) => row.getAttribute('aria-selected') === 'true');
      return { target, active };
    }, option);
    if (pick.target < 0 || pick.active < 0 || pick.active === pick.target) break;
    await page.press('#prompt-input', pick.target > pick.active ? 'ArrowDown' : 'ArrowUp');
  }
  try {
    await page.waitForFunction(
      (want) => (document.querySelector('.command-picker__popover [role="option"][aria-selected="true"] .command-picker__name')?.textContent || '').trim() === want,
      option,
      { timeout: Math.min(timeoutMs, 10000) },
    );
  } catch {
    throw new Error(`${label}: ArrowUp/Down never made ${JSON.stringify(option)} the active candidate`);
  }

  // 3. The SELECTING Enter settles the token and dismisses the popup — never dispatches.
  await page.press('#prompt-input', 'Enter');
  try {
    await page.waitForFunction(
      (want) => document.querySelector('.command-picker__popover') === null
        && (document.getElementById('prompt-input')?.value ?? '').trim() === want,
      token,
      { timeout: Math.min(timeoutMs, 15000) },
    );
  } catch {
    throw new Error(`${label}: the selecting Enter did not settle ${JSON.stringify(token)} with the popup dismissed`);
  }
  if (dispatched() !== 0) {
    throw new Error(`${label}: the selecting Enter already dispatched (${dispatched()} ${[...types].join('/')} frame(s))`);
  }

  // 4. The DISPATCH Enter on the settled token sends exactly one.
  await page.press('#prompt-input', 'Enter');
  const deadline = Date.now() + 15000;
  while (dispatched() === 0 && Date.now() < deadline) await sleep(25);
  const count = dispatched();
  if (count !== 1) {
    throw new Error(`${label}: the dispatch Enter sent ${count} ${[...types].join('/')} frame(s), expected exactly 1`);
  }
}
