// Web client E2E (playwright lane of E2E.d/web/run.sh).
//
// Environment:
//   RPI_URL         http://127.0.0.1:<port>/web
//   RPI_TOKEN       token file content (served via rpi-auth.<token> subprotocol)
//   RPI_SLOW_TAIL   tail of the slow mock reply ("chunk-four-done")
//   RPI_FAST_REPLY  instant mock reply text ("steering-followup-reply")
//   RPI_ABORTED_TAIL final chunk that must NEVER render after abort ("-done")
//   RPI_CHROME      executable path of the system Chrome (optional)
//   RPI_EVIDENCE    evidence dir for screenshots
//
// Asserts: page loads, WS connects (subprotocol), full prompt round-trip
// streams into the DOM, abort cuts a slow stream, later prompt recovers, the
// Todo DAG panel creates/completes/reopens a task with live state, the rich
// content renderer (table/task-list/mermaid/KaTeX), the Workflow panel
// creates a workflow (live status) then cancels it, the Settings panel
// browses by category (secret keys redacted + not editable), edits `theme`
// in a draft, applies, and reflects the persisted value, the Session panel
// renders session info, renames the session (panel + header), lists saved
// sessions, and switches to a new session, the Subagents panel spawns and
// cancels a live job, and the structured Task tool card renders Goal/
// Constraints/Contract + child without raw JSON.

import { chromium } from 'playwright';
import fs from 'node:fs';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const slowTail = process.env.RPI_SLOW_TAIL || 'chunk-four-done';
const fastReply = process.env.RPI_FAST_REPLY || 'steering-followup-reply';
const abortedTail = process.env.RPI_ABORTED_TAIL || '-done';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';

function fail(message) {
  // Exit 2 (not 1): 1 is reserved for playwright SETUP failure (node/
  // chromium/npm), which must NOT be confused with an assertion failure (the
  // mock's request counter is stateful and a rerun would see shifted
  // replies). Any non-1 exit fails the lane.
  console.error(`web: FAIL: ${message}`);
  process.exit(2);
}

async function waitFor(page, fn, label, timeoutMs = 25000, arg) {
  try {
    await page.waitForFunction(fn, arg, { timeout: timeoutMs });
  } catch (err) {
    fail(`${label} (timeout ${timeoutMs}ms)`);
  }
}

async function lastAssistantText(page) {
  return page.evaluate(() => {
    const nodes = document.querySelectorAll('.msg--assistant .assistant-text');
    return nodes.length ? nodes[nodes.length - 1].textContent : '';
  });
}

async function main() {
  if (!url) fail('RPI_URL is required');
  const launchOptions = chromePath
    ? { executablePath: chromePath }
    : {};
  const browser = await chromium.launch(launchOptions);
  try {
    const page = await browser.newPage();
    if (token) {
      await page.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
    }
    page.on('pageerror', (err) => {
      console.error(`web: page error: ${err.message}`);
    });

    // 1. Page loads: GET /web serves the self-contained client.
    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'page title missing');
    await waitFor(page, () => document.querySelector('#conn-state') !== null, 'conn-state missing');

    // 2. WS connects via boot localStorage token (rpi-auth.<token>).
    await waitFor(
      page,
      () => document.getElementById('conn-state').dataset.state === 'on',
      'WS did not reach "connected" (wrong token or listener?)'
    );

    // 3. Prompt round-trip streams the FULL slow reply into the DOM.
    await page.fill('#prompt-input', 'hello from the web e2e');
    await page.press('#prompt-input', 'Enter');
    await waitFor(page, (tail) => document.body.textContent.includes(tail), 'full slow reply never streamed into the DOM', 30000, slowTail);
    await waitFor(
      page,
      () => document.getElementById('stream-badge').hidden === true,
      'streaming badge did not clear after the reply completed'
    );
    await page.screenshot({ path: `${evidence}/roundtrip.png`, fullPage: true });

    // Second prompt hits the instant path of the mock (request 2).
    await page.fill('#prompt-input', 'again');
    await page.press('#prompt-input', 'Enter');
    await waitFor(page, (reply) => document.body.textContent.includes(reply), 'fast reply never streamed into the DOM', 30000, fastReply);

    // 4. Unified Stop action aborts the slow stream (request 3) mid-flight.
    await page.fill('#prompt-input', 'stream a long answer');
    await page.press('#prompt-input', 'Enter');
    await waitFor(page, () => document.body.textContent.includes('steer-3-'), 'third stream never started');
    await waitFor(
      page,
      () => document.getElementById('send-btn')?.getAttribute('aria-label') === 'Stop generating',
      'unified composer action never switched to Stop'
    );
    await page.click('#send-btn');
    await waitFor(
      page,
      () => document.getElementById('stream-badge').hidden === true,
      'streaming badge did not clear after abort'
    );
    const abortedText = await lastAssistantText(page);
    if (abortedText.includes(abortedTail)) {
      fail(`aborted stream still rendered the final chunks: ${abortedText}`);
    }
    await page.screenshot({ path: `${evidence}/abort.png`, fullPage: true });

    // Recovery: the rpi's turn gate takes a moment to release after an
    // abort; without this settle the next prompt is accepted but never
    // reaches the provider. Then request 4 must round-trip as a NEW
    // assistant message (count-based: the reply text equals request 2's, so
    // a textContent check would stale-pass on the old DOM content).
    await page.waitForTimeout(6000);
    const assistantCountBeforeRecovery = await page.evaluate(
      () => document.querySelectorAll('.msg--assistant .assistant-text').length
    );
    await page.fill('#prompt-input', 'recovery prompt');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      (before) => {
        const nodes = [...document.querySelectorAll('.msg--assistant .assistant-text')];
        return nodes.length > before && (nodes[nodes.length - 1]?.textContent || '').includes('steering-followup-reply');
      },
      'post-abort prompt did not round-trip as a new message',
      30000,
      assistantCountBeforeRecovery
    );

    // 5. Todo DAG panel: open it, add a task (todo_op append), complete it
    //    (todo_op done), reopen it (todo_op start), assert panel state.
    await page.click('#todos-toggle-btn');
    await waitFor(page, () => document.getElementById('todo-panel') !== null, 'todo panel did not open');
    await page.fill('#todo-add-phase', 'Plan');
    await page.fill('#todo-add-content', 'web e2e task');
    await page.click('#todo-add-btn');
    await waitFor(
      page,
      () => {
        const rows = [...document.querySelectorAll('.todo-task')];
        return rows.some((row) => row.textContent.includes('web e2e task'));
      },
      'added task never appeared in the todo panel'
    );
    const taskStatus = () =>
      page.evaluate(() => {
        const rows = [...document.querySelectorAll('.todo-task')];
        const row = rows.find((r) => r.textContent.includes('web e2e task'));
        if (!row) return '';
        return row.querySelector('.todo-task__bullet')?.getAttribute('aria-label') || '';
      });
    const created = await taskStatus();
    if (created !== 'pending' && created !== 'in_progress') {
      fail(`new task must be pending or in_progress, got "${created}"`);
    }
    // Complete via the row action.
    await page.evaluate(() => {
      const rows = [...document.querySelectorAll('.todo-task')];
      const row = rows.find((r) => r.textContent.includes('web e2e task'));
      if (!row) throw new Error('task row missing');
      const btn = row.querySelector('.todo-task__action[data-action="complete"]');
      if (!btn) throw new Error('complete button missing');
      btn.click();
    });
    await waitFor(page, () => {
      const rows = [...document.querySelectorAll('.todo-task')];
      const row = rows.find((r) => r.textContent.includes('web e2e task'));
      return !!row && row.querySelector('.todo-task__bullet')?.getAttribute('aria-label') === 'completed';
    }, 'task never reached completed status');
    await waitFor(
      page,
      () => (document.getElementById('todo-counts')?.textContent || '').includes('1 done'),
      'counts never reflected the completed task'
    );
    // Reopen the completed task (todo_op start) and assert in_progress.
    await page.evaluate(() => {
      const rows = [...document.querySelectorAll('.todo-task')];
      const row = rows.find((r) => r.textContent.includes('web e2e task'));
      if (!row) throw new Error('task row missing');
      const btn = row.querySelector('.todo-task__action[data-action="reopen"]');
      if (!btn) throw new Error('reopen button missing');
      btn.click();
    });
    await waitFor(page, () => {
      const rows = [...document.querySelectorAll('.todo-task')];
      const row = rows.find((r) => r.textContent.includes('web e2e task'));
      return !!row && row.querySelector('.todo-task__bullet')?.getAttribute('aria-label') === 'in_progress';
    }, 'reopened task never returned to in_progress');
    // Detail pane: select the task and assert dependency + ready metadata.
    await page.evaluate(() => {
      const rows = [...document.querySelectorAll('.todo-task')];
      const row = rows.find((r) => r.textContent.includes('web e2e task'));
      if (!row) throw new Error('task row missing');
      row.click();
    });
    await waitFor(
      page,
      () => (document.getElementById('todo-detail')?.textContent || '').includes('web e2e task'),
      'task detail pane never rendered'
    );
    await page.screenshot({ path: `${evidence}/todo-panel.png`, fullPage: true });

    // 5b. Todo header counts layout contract (real DOM geometry): every
    // `N label` counter is atomic — the digit and label never split across
    // lines — the WHOLE counts line stays on one row at desktop (the panel
    // is widened to 460px so it fits), and the close chip shares the head
    // row without being squeezed out.
    const countsLayout = await page.evaluate(() => {
      const countsEl = document.getElementById('todo-counts');
      const closeBtn = document.getElementById('todo-close-btn');
      const panel = document.getElementById('todo-panel');
      if (!countsEl || !closeBtn || !panel) return null;
      const lineHeight = parseFloat(getComputedStyle(countsEl).lineHeight) || 0;
      const countsBox = countsEl.getBoundingClientRect();
      const panelBox = panel.getBoundingClientRect();
      const closeBox = closeBtn.getBoundingClientRect();
      // Atom rects: measure every `<digits> <label>` text run (the JSX
      // renders each counter as its own text node, so map match offsets onto
      // the text nodes and use a Range to get the run's real box).
      const text = countsEl.textContent || '';
      const walker = document.createTreeWalker(countsEl, NodeFilter.SHOW_TEXT);
      const nodes = [];
      let node;
      while ((node = walker.nextNode())) nodes.push(node);
      let acc = 0;
      const runs = nodes.map((n) => {
        const r = { node: n, start: acc, end: acc + n.textContent.length };
        acc = r.end;
        return r;
      });
      const atoms = [];
      for (const m of text.matchAll(/\d+\s+\S+/g)) {
        const start = m.index;
        const end = start + m[0].length;
        let sNode = null;
        let sOff = 0;
        let eNode = null;
        let eOff = 0;
        for (const run of runs) {
          if (sNode === null && start < run.end) { sNode = run.node; sOff = start - run.start; }
          if (end <= run.end) { eNode = run.node; eOff = end - run.start; break; }
        }
        if (sNode && eNode) {
          const range = document.createRange();
          range.setStart(sNode, sOff);
          range.setEnd(eNode, eOff);
          const r = range.getBoundingClientRect();
          atoms.push({ text: m[0], height: r.height });
        }
      }
      return {
        countsText: text,
        lineHeight,
        countsHeight: countsBox.height,
        countsSingleLine: countsBox.height <= lineHeight * 1.5,
        atoms,
        atomsSingleLine: atoms.every((a) => a.height <= lineHeight * 1.5),
        panelWidth: panelBox.width,
        closeInsidePanel: closeBox.right <= panelBox.right - 1 && closeBox.left >= panelBox.left,
        closeSharesHeadRow: Math.abs(closeBox.top + closeBox.height / 2 - (countsBox.top + countsBox.height / 2)) < lineHeight,
      };
    });
    if (!countsLayout) fail('todo panel header (counts/close) missing');
    if (countsLayout.atoms.length < 4) fail(`todo counts atoms missing: "${countsLayout.countsText}"`);
    if (!countsLayout.countsSingleLine) {
      fail(`todo counts wrapped at desktop: "${countsLayout.countsText}" height ${countsLayout.countsHeight} vs line-height ${countsLayout.lineHeight}`);
    }
    if (!countsLayout.atomsSingleLine) {
      fail(`todo counts atom split across lines at desktop: ${JSON.stringify(countsLayout.atoms)}`);
    }
    const doneAtom = countsLayout.atoms.find((a) => a.text.endsWith('done'));
    if (!doneAtom) fail('todo counts missing the "N done" atom');
    if (countsLayout.panelWidth < 440) {
      fail(`todo panel not widened on desktop: ${countsLayout.panelWidth}px (counts need a single line)`);
    }
    if (!countsLayout.closeInsidePanel || !countsLayout.closeSharesHeadRow) {
      fail(`todo close chip squeezed or misplaced: inside=${countsLayout.closeInsidePanel} sameRow=${countsLayout.closeSharesHeadRow}`);
    }

    // 5c. The same contract at 390×844 mobile: the counts line MAY wrap
    // between counters on phones, but every `N label` atom (including
    // `0 done`) stays intact and the drawer never overflows the viewport.
    const mobilePage = await browser.newPage({ viewport: { width: 390, height: 844 } });
    if (token) {
      await mobilePage.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
    }
    await mobilePage.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(mobilePage, () => document.getElementById('conn-state')?.dataset.state === 'on', 'mobile WS did not connect');
    // Panel toggles live inside the session-sidebar drawer on phones.
    await mobilePage.click('#sidebar-toggle-btn');
    await waitFor(
      mobilePage,
      () => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open'),
      'mobile sidebar drawer never opened'
    );
    await mobilePage.click('#todos-toggle-btn');
    await waitFor(mobilePage, () => document.getElementById('todo-panel') !== null, 'mobile todo panel did not open');
    const mobileLayout = await mobilePage.evaluate(() => {
      const countsEl = document.getElementById('todo-counts');
      const closeBtn = document.getElementById('todo-close-btn');
      const panel = document.getElementById('todo-panel');
      if (!countsEl || !closeBtn || !panel) return null;
      const lineHeight = parseFloat(getComputedStyle(countsEl).lineHeight) || 0;
      const countsBox = countsEl.getBoundingClientRect();
      const panelBox = panel.getBoundingClientRect();
      const closeBox = closeBtn.getBoundingClientRect();
      const text = countsEl.textContent || '';
      const walker = document.createTreeWalker(countsEl, NodeFilter.SHOW_TEXT);
      const nodes = [];
      let node;
      while ((node = walker.nextNode())) nodes.push(node);
      let acc = 0;
      const runs = nodes.map((n) => {
        const r = { node: n, start: acc, end: acc + n.textContent.length };
        acc = r.end;
        return r;
      });
      const atoms = [];
      for (const m of text.matchAll(/\d+\s+\S+/g)) {
        const start = m.index;
        const end = start + m[0].length;
        let sNode = null;
        let sOff = 0;
        let eNode = null;
        let eOff = 0;
        for (const run of runs) {
          if (sNode === null && start < run.end) { sNode = run.node; sOff = start - run.start; }
          if (end <= run.end) { eNode = run.node; eOff = end - run.start; break; }
        }
        if (sNode && eNode) {
          const range = document.createRange();
          range.setStart(sNode, sOff);
          range.setEnd(eNode, eOff);
          const r = range.getBoundingClientRect();
          atoms.push({ text: m[0], height: r.height });
        }
      }
      return {
        countsText: text,
        lineHeight,
        atoms,
        atomsSingleLine: atoms.every((a) => a.height <= lineHeight * 1.5),
        panelWidth: panelBox.width,
        countsRight: countsBox.right,
        panelRight: panelBox.right,
        closeInsidePanel: closeBox.right <= panelBox.right - 1 && closeBox.left >= panelBox.left,
        closeSharesHeadRow: Math.abs(closeBox.top + closeBox.height / 2 - (countsBox.top + countsBox.height / 2)) < lineHeight,
        scrollWidth: document.documentElement.scrollWidth,
        innerWidth: window.innerWidth,
      };
    });
    await mobilePage.close();
    if (!mobileLayout) fail('mobile todo panel header (counts/close) missing');
    if (mobileLayout.atoms.length < 4) fail(`mobile todo counts atoms missing: "${mobileLayout.countsText}"`);
    if (!mobileLayout.atomsSingleLine) {
      fail(`mobile todo counts atom split across lines at 390px: ${JSON.stringify(mobileLayout.atoms)}`);
    }
    const mobileDoneAtom = mobileLayout.atoms.find((a) => a.text.endsWith('done'));
    if (!mobileDoneAtom) fail('mobile todo counts missing the "N done" atom');
    if (mobileLayout.panelWidth < mobileLayout.innerWidth - 1) {
      fail(`mobile todo panel not full-screen: ${mobileLayout.panelWidth} vs viewport ${mobileLayout.innerWidth}`);
    }
    if (mobileLayout.countsRight > mobileLayout.panelRight - 1) {
      fail(`mobile todo counts overflow the panel: counts right ${mobileLayout.countsRight} vs panel right ${mobileLayout.panelRight}`);
    }
    if (mobileLayout.scrollWidth > mobileLayout.innerWidth + 1) {
      fail(`mobile horizontal overflow with todo panel open: scrollWidth ${mobileLayout.scrollWidth} > viewport ${mobileLayout.innerWidth}`);
    }
    if (!mobileLayout.closeInsidePanel || !mobileLayout.closeSharesHeadRow) {
      fail(`mobile todo close chip squeezed or misplaced: inside=${mobileLayout.closeInsidePanel} sameRow=${mobileLayout.closeSharesHeadRow}`);
    }

    // 6. Rich content: the mock returns markdown (table + task list), a
    // mermaid flowchart fence, $...$/$$...$$ math, and a `rust,ignore` fence.
    // The upgraded renderer must produce the structured content and highlight
    // Rust tokens through the END of the code block, with no raw fence markers.
    //
    // The mock routes RICH_TEXT by prompt CONTENT ("render rich content" as
    // the request's last user message), so request parity cannot matter.
    // Under parallel-lane load the rpi's post-abort turn gate can release
    // late, shifting which prompt a given request carries; retry the prompt
    // (bounded) until a rich reply actually lands instead of flaking.
    const richPromptAttempt = async () => {
      const before = await page.evaluate(() => document.querySelectorAll('.msg--assistant .assistant-text').length);
      await page.fill('#prompt-input', 'render rich content');
      await page.press('#prompt-input', 'Enter');
      await waitFor(
        page,
        (b) => {
          const nodes = [...document.querySelectorAll('.msg--assistant .assistant-text')];
          if (nodes.length <= b) return false;
          if (document.querySelector('table.md-table')) return true; // rich reply rendered
          // A different reply settled (parity/retry fallback): signal retry.
          return document.getElementById('stream-badge')?.hidden === true;
        },
        'rich prompt never produced a reply',
        30000,
        before
      );
      return page.evaluate(() => !!document.querySelector('table.md-table'));
    };
    let richRendered = false;
    for (let attempt = 1; attempt <= 3 && !richRendered; attempt++) {
      richRendered = await richPromptAttempt();
    }
    if (!richRendered) fail('markdown table never rendered (after 3 prompt attempts)');
    await waitFor(page, () => document.querySelector('.md-task-glyph') !== null, 'task-list glyph never rendered', 30000);
    await waitFor(page, () => document.querySelector('.assistant-text svg') !== null, 'mermaid SVG never rendered', 30000);
    await waitFor(page, () => document.querySelector('.assistant-text .katex') !== null, 'KaTeX math never rendered', 30000);
    await waitFor(
      page,
      () => {
        const rustFence = [...document.querySelectorAll('.md-fence')].find(
          (fence) => fence.querySelector('.md-fence__lang')?.textContent?.trim() === 'rust'
        );
        if (!rustFence) return false;
        const code = rustFence.querySelector('code');
        const lateLiteral = [...rustFence.querySelectorAll('.hljs-literal')].some(
          (node) => node.textContent === 'None'
        );
        const lateKeyword = [...rustFence.querySelectorAll('.hljs-keyword')].some(
          (node) => node.textContent === 'match'
        );
        return !!code && code.textContent.includes('None => Err') && lateLiteral && lateKeyword;
      },
      'Rust fence metadata did not highlight tokens through the end of the block',
      30000
    );
    const richText = await lastAssistantText(page);
    if (richText.includes('```')) {
      fail(`raw fence markers leaked into the transcript: ${richText}`);
    }
    if (richText.includes('|---')) {
      fail(`raw table separator leaked into the transcript: ${richText}`);
    }
    await page.screenshot({ path: `${evidence}/rich.png`, fullPage: true });

    // 7. Workflow panel: create a workflow, assert it appears with a live
    //    status, then cancel it and assert the cancelled status in the list.
    //    Step 5 left the Todo panel open; it overlays the header, so close it
    //    first (the single `activePanel` state shows one panel at a time).
    await page.click('#todo-close-btn');
    await waitFor(page, () => document.getElementById('todo-panel') === null, 'todo panel did not close');
    await page.click('#workflow-toggle-btn');
    await waitFor(page, () => document.getElementById('workflow-panel') !== null, 'workflow panel did not open');
    await page.fill('#workflow-create-name', 'web-e2e-workflow');
    await page.fill('#workflow-create-objective', 'created from the browser e2e');
    await page.click('#workflow-create-btn');
    await waitFor(
      page,
      () => {
        const rows = [...document.querySelectorAll('.workflow-row')];
        return rows.some((row) => row.textContent.includes('web-e2e-workflow'));
      },
      'created workflow never appeared in the workflow list',
      30000
    );
    const workflowStatus = () =>
      page.evaluate(() => {
        const rows = [...document.querySelectorAll('.workflow-row')];
        const row = rows.find((r) => r.textContent.includes('web-e2e-workflow'));
        if (!row) return '';
        return row.getAttribute('data-status') || '';
      });
    const createdStatus = await workflowStatus();
    if (!['queued', 'planning', 'running', 'paused', 'integrating'].includes(createdStatus)) {
      fail(`created workflow must be live (queued/planning/running/paused/integrating), got "${createdStatus}"`);
    }
    // The created workflow is auto-selected; cancel it from the detail pane.
    await page.click('#workflow-cancel-btn');
    await waitFor(
      page,
      async () => {
        const rows = [...document.querySelectorAll('.workflow-row')];
        const row = rows.find((r) => r.textContent.includes('web-e2e-workflow'));
        return !!row && row.getAttribute('data-status') === 'cancelled';
      },
      'cancel never applied (status never reached cancelled)',
      30000
    );
    await page.screenshot({ path: `${evidence}/workflow.png`, fullPage: true });

    // 8. Settings panel: browse by category, secret refusal (live.sttApiKey
    //    renders redacted with no editable control), open a global draft,
    //    change `theme` (a LIVE string setting), apply, and assert the panel
    //    reflects the persisted value.
    await page.click('#settings-toggle-btn');
    await waitFor(page, () => document.getElementById('settings-panel') !== null, 'settings panel did not open');
    await page.click('.settings-category:has-text("Terminal")');
    await waitFor(
      page,
      () => document.querySelector('[data-setting-key="theme"]') !== null,
      'theme setting row never rendered'
    );
    // Secret refusal: the live.sttApiKey row must show the redacted marker
    // and expose no input/select/textarea control.
    await page.click('.settings-category:has-text("Live")');
    await waitFor(
      page,
      () => document.querySelector('[data-setting-key="live.sttApiKey"]') !== null,
      'live.sttApiKey row never rendered'
    );
    const secretText = await page.evaluate(() => {
      const row = document.querySelector('[data-setting-key="live.sttApiKey"]');
      return row ? row.textContent : '';
    });
    if (!secretText.includes('[redacted]') && !secretText.includes('[secret]')) {
      fail(`secret key must render redacted, got: ${secretText}`);
    }
    const secretControlCount = await page.evaluate(() => {
      const row = document.querySelector('[data-setting-key="live.sttApiKey"]');
      return row ? row.querySelectorAll('input, select, textarea').length : -1;
    });
    if (secretControlCount !== 0) {
      fail(`secret key must not be editable, found ${secretControlCount} controls`);
    }
    // Open a global draft and change theme.
    await page.click('.settings-category:has-text("Terminal")');
    await page.click('#settings-edit-btn');
    await waitFor(page, () => document.getElementById('settings-apply-btn') !== null, 'draft never opened (Apply button missing)');
    const themeInputSel = '[data-setting-key="theme"] input[type="text"]';
    await waitFor(page, (sel) => document.querySelector(sel) !== null, 'theme input never rendered in draft mode', 25000, themeInputSel);
    await page.fill(themeInputSel, 'e2e-theme');
    await page.evaluate(() => {
      const el = document.querySelector('[data-setting-key="theme"] input[type="text"]');
      if (el) el.blur();
    });
    await waitFor(
      page,
      () => (document.querySelector('[data-setting-key="theme"] .setting-row__dirty')?.textContent || '') === 'dirty',
      'theme edit never staged as dirty in the draft'
    );
    await page.click('#settings-apply-btn');
    await waitFor(page, () => document.getElementById('settings-edit-btn') !== null, 'apply never closed the draft (Edit button missing)');
    await waitFor(
      page,
      () => {
        const input = document.querySelector('[data-setting-key="theme"] input[type="text"]');
        return !!input && input.value === 'e2e-theme';
      },
      'applied theme never reflected in the settings panel'
    );
    await page.screenshot({ path: `${evidence}/settings.png`, fullPage: true });

    // 9. Session panel: current session info renders, rename round-trips
    //    into the panel + header, the saved-sessions list is populated, and
    //    new session switches to a fresh session id.
    // Persistent left sidebar: lists saved sessions with the current one
    // marked active, and New switches to a fresh session.
    await waitFor(page, () => document.getElementById('session-sidebar') !== null, 'session sidebar did not render');
    await waitFor(
      page,
      () => document.querySelectorAll('.session-sidebar__row').length >= 1,
      'session sidebar never listed saved sessions'
    );
    await page.click('#session-toggle-btn');
    await waitFor(page, () => document.getElementById('session-panel') !== null, 'session panel did not open');
    const nameValueSel = '[data-testid="session-name-value"]';
    await waitFor(page, (sel) => document.querySelector(sel) !== null, 'current session name never rendered', 25000, nameValueSel);
    await page.fill('#session-rename-input', 'web e2e session');
    await page.click('#session-rename-btn');
    await waitFor(
      page,
      () => document.querySelector('[data-testid="session-name-value"]')?.textContent === 'web e2e session',
      'session rename never reflected in the panel'
    );
    await waitFor(
      page,
      () => (document.getElementById('session-name')?.textContent || '').includes('web e2e session'),
      'session rename never reflected in the header'
    );
    await waitFor(
      page,
      () => document.querySelectorAll('.session-row').length >= 1,
      'no saved sessions listed in the session panel'
    );
    const sessionIdBefore = await page.evaluate(() => {
      const dts = [...document.querySelectorAll('#session-panel dl dt')];
      const idx = dts.findIndex((dt) => dt.textContent === 'Session id');
      return idx >= 0 ? dts[idx]?.nextElementSibling?.textContent || '' : '';
    });
    if (!sessionIdBefore) {
      fail('current session id never rendered');
    }
    // F4 session cutover contract: the transcript must be POPULATED before
    // the switch (so the clear is observable), the switch to a different
    // session id must CLEAR it (the server never replays messages over RPC),
    // and a same-id refresh must keep it.
    const msgsBeforeSwitch = await page.$$eval('#transcript .msg', (els) => els.length);
    if (msgsBeforeSwitch === 0) {
      fail('session lifecycle: no transcript messages before the switch — the clear would be unobservable');
    }
    await page.click('#session-new-btn');
    const newIdOk = await page
      .waitForFunction(
        (b) => {
          const dts = [...document.querySelectorAll('#session-panel dl dt')];
          const idx = dts.findIndex((dt) => dt.textContent === 'Session id');
          const dd = idx >= 0 ? dts[idx].nextElementSibling : null;
          const id = dd ? dd.textContent || '' : '';
          return id !== '' && id !== b;
        },
        sessionIdBefore,
        { timeout: 25000 }
      )
      .then(() => true)
      .catch(() => false);
    if (!newIdOk) {
      const dump = await page.evaluate((b) => JSON.stringify({
        sessionId: (() => {
          const dts = [...document.querySelectorAll('#session-panel dl dt')];
          const idx = dts.findIndex((dt) => dt.textContent === 'Session id');
          const dd = idx >= 0 ? dts[idx].nextElementSibling : null;
          return dd ? dd.textContent || '' : '';
        })(),
        before: b,
        status: document.querySelector('#session-panel .panel__status')?.textContent || '',
        headerSession: document.getElementById('session-name')?.textContent || '',
      }), sessionIdBefore);
      fail(`new session never produced a different session id — ${dump}`);
    }
    // F4: a NEW session id must show the EMPTY new-session view — never the
    // previous session's messages under the new header.
    await waitFor(
      page,
      () =>
        document.querySelectorAll('#transcript .msg').length === 0 &&
        document.querySelector('#transcript .empty-hint') !== null,
      'session switch never cleared the old transcript (empty new-session view missing)'
    );
    const staleTranscript = await page.evaluate(
      () => document.getElementById('transcript')?.textContent || ''
    );
    if (staleTranscript.includes('hello from the web e2e')) {
      fail("session switch retained the old session's messages in the transcript");
    }
    await page.screenshot({ path: `${evidence}/session.png`, fullPage: true });

    // 10. Subagents panel: spawn a faux subagent via the panel (task_spawn),
    //     assert the job card appears with a live status + activity line,
    //     message it (hub_send), view its output (job_output), then cancel it
    //     (job_cancel) and assert the settled status in the list.
    // Close the session drawer first: panels are fixed overlays and would
    // intercept the header toggle click (same fix as the workflow step).
    await page.click('#session-close-btn');
    await waitFor(page, () => document.getElementById('session-panel') === null, 'session panel did not close');
    await page.click('#subagents-toggle-btn');
    await waitFor(page, () => document.getElementById('subagents-panel') !== null, 'subagents panel did not open');
    // The orchestration fixture must be enabled; otherwise the panel hides the
    // spawn form and this step fails fast with a clear message. The panel
    // fetches job_list asynchronously on mount, so WAIT for the spawn form
    // rather than checking once (a race would fail spuriously).
    await waitFor(
      page,
      () => document.getElementById('subagents-panel')?.querySelector('#subagents-spawn-btn') !== null,
      'subagents panel did not show the spawn form (orchestration disabled in fixture?)',
      15000
    );
    await page.selectOption('#subagents-agent-select', 'writer');
    await page.fill('#subagents-task-input', 'web-e2e-subagent: audit the release notes and report findings');
    await page.click('#subagents-spawn-btn');
    // The spawned child's marker prompt streams slowly in the mock, so the job
    // stays queued/running long enough to cancel deterministically.
    await waitFor(
      page,
      () => {
        const cards = [...document.querySelectorAll('.subagent-job')];
        return cards.some((card) => (card.textContent || '').includes('audit the release notes'));
      },
      'spawned subagent job never appeared in the panel',
      30000
    );
    const subagentStatus = () =>
      page.evaluate(() => {
        const cards = [...document.querySelectorAll('.subagent-job')];
        const card = cards.find((c) => (c.textContent || '').includes('audit the release notes'));
        return card ? card.getAttribute('data-status') || '' : '';
      });
    const liveStatus = await subagentStatus();
    if (!['queued', 'running'].includes(liveStatus)) {
      fail(`spawned subagent must be live (queued/running) before cancel, got "${liveStatus}"`);
    }
    // Activity one-liner: the progress line must render (stage or activity ·
    // elapsed) for the live job.
    const progressLine = await page.evaluate(() => {
      const cards = [...document.querySelectorAll('.subagent-job')];
      const card = cards.find((c) => (c.textContent || '').includes('audit the release notes'));
      const line = card ? card.querySelector('[data-progress-line]') : null;
      return line ? line.textContent || '' : '';
    });
    if (!progressLine.trim()) {
      fail('spawned subagent never rendered a live activity/elapsed line');
    }
    await page.screenshot({ path: `${evidence}/subagents-spawned.png`, fullPage: true });
    // The job is still running because the mock streams slowly. Open the
    // modal via the Details trigger and assert an
    // accessible dialog (role=dialog, aria-modal, aria-label, correct
    // data-job-id, initial focus on Close) showing the task description, live
    // status/elapsed, latest activity, the live badge, and NON-EMPTY recent
    // history while the job runs — the acceptance that fails when a
    // long-running job exposes only status/output. Exercise Refresh
    // (refetch keeps the transcript live with no error), close via Escape,
    // reopen by clicking the card, and close via the Close button.
    await page.evaluate(() => {
      const cards = [...document.querySelectorAll('.subagent-job')];
      const card = cards.find((c) => (c.textContent || '').includes('audit the release notes'));
      if (!card) throw new Error('subagent card missing before modal open');
      card.querySelector('[data-details-trigger]').click();
    });
    await waitFor(page, () => document.querySelector('[data-details-dialog]') !== null, 'detail modal never opened (Details trigger)');
    const detailsA11y = await page.evaluate(() => {
      const dlg = document.querySelector('[data-details-dialog]');
      const close = document.querySelector('[data-details-close]');
      const card = [...document.querySelectorAll('.subagent-job')]
        .find((c) => (c.textContent || '').includes('audit the release notes'));
      return {
        role: dlg?.getAttribute('role') || '',
        ariaModal: dlg?.getAttribute('aria-modal') || '',
        ariaLabel: dlg?.getAttribute('aria-label') || '',
        jobId: dlg?.getAttribute('data-job-id') || '',
        cardJobId: card?.getAttribute('data-job-id') || '',
        focusOnClose: !!close && document.activeElement === close,
      };
    });
    if (detailsA11y.role !== 'dialog') fail(`detail modal must be role=dialog, got "${detailsA11y.role}"`);
    if (detailsA11y.ariaModal !== 'true') fail('detail modal must set aria-modal=true');
    if (detailsA11y.ariaLabel !== 'Subagent job details') fail(`detail modal aria-label mismatch: "${detailsA11y.ariaLabel}"`);
    if (!detailsA11y.jobId || detailsA11y.jobId !== detailsA11y.cardJobId) {
      fail(`detail modal bound to wrong job: dialog=${detailsA11y.jobId} card=${detailsA11y.cardJobId}`);
    }
    if (!detailsA11y.focusOnClose) fail('detail modal initial focus did not land on the Close button');
    const detailsContent = await page.evaluate(() => {
      const txt = (sel) => (document.querySelector(sel)?.textContent || '').trim();
      return {
        description: txt('[data-details-description]'),
        status: txt('[data-details-status] .subagent-job__status'),
        elapsed: txt('[data-details-elapsed]'),
        activity: txt('[data-details-activity]'),
        live: document.querySelector('[data-details-live]') !== null,
      };
    });
    if (!detailsContent.description.includes('audit the release notes')) {
      fail(`detail modal description missing the task text: "${detailsContent.description}"`);
    }
    if (!['queued', 'running'].includes(detailsContent.status)) {
      fail(`detail modal status must be live (queued/running), got "${detailsContent.status}"`);
    }
    if (!detailsContent.elapsed) fail('detail modal elapsed never rendered while running');
    if (!detailsContent.activity) fail('detail modal latest activity never rendered');
    if (!detailsContent.live) fail('detail modal live badge missing while the job is running');
    // Recent history MUST be non-empty while the job is still running.
    await waitFor(
      page,
      () => {
        const pre = document.querySelector('[data-details-history]');
        const text = pre ? pre.textContent.trim() : '';
        return text !== '' && !text.startsWith('(no transcript yet') && !text.startsWith('(transcript unavailable)');
      },
      'detail modal recent history never became non-empty while the job was running',
      15000
    );
    const detailsErrorShown = await page.evaluate(
      () => document.querySelector('[data-details-error]') !== null,
    );
    if (detailsErrorShown) {
      fail('detail modal reported an agent_history error while the job was running');
    }
    await page.screenshot({ path: `${evidence}/subagents-modal-running.png`, fullPage: true });
    // Refresh re-fetches the child transcript: dialog stays open, history
    // remains non-empty, no error appears.
    await page.evaluate(() => document.querySelector('[data-details-refresh]').click());
    await waitFor(
      page,
      () => {
        const pre = document.querySelector('[data-details-history]');
        return !!pre && pre.textContent.trim() !== '' && document.querySelector('[data-details-error]') === null;
      },
      'detail modal Refresh broke the recent history view',
      10000
    );
    // Close via Escape dismisses the modal.
    await page.keyboard.press('Escape');
    await waitFor(page, () => document.querySelector('[data-details-dialog]') === null, 'detail modal did not close on Escape');
    // Reopen by clicking the running job card (the section onClick path) and
    // close via the Close button — proving both dismissal paths.
    await page.evaluate(() => {
      const cards = [...document.querySelectorAll('.subagent-job')];
      const card = cards.find((c) => (c.textContent || '').includes('audit the release notes'));
      card.querySelector('.subagent-job__description').click();
    });
    await waitFor(page, () => document.querySelector('[data-details-dialog]') !== null, 'detail modal never opened on card click');
    await page.evaluate(() => document.querySelector('[data-details-close]').click());
    await waitFor(page, () => document.querySelector('[data-details-dialog]') === null, 'detail modal did not close on Close button');

    // Message the subagent via hub_send (per-job message input + Send).
    // React controlled inputs ignore `input.value=`; use the native setter so
    // onChange fires and the Send button enables.
    await page.evaluate(() => {
      const cards = [...document.querySelectorAll('.subagent-job')];
      const card = cards.find((c) => (c.textContent || '').includes('audit the release notes'));
      if (!card) throw new Error('subagent card missing');
      const input = card.querySelector('.subagent-job__message-input');
      if (!input) throw new Error('message input missing');
      const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
      setter.call(input, 'status report?');
      input.dispatchEvent(new Event('input', { bubbles: true }));
    });
    await waitFor(
      page,
      () => {
        const cards = [...document.querySelectorAll('.subagent-job')];
        const card = cards.find((c) => (c.textContent || '').includes('audit the release notes'));
        const btn = card ? card.querySelector('.subagent-job__message .subagent-job__action') : null;
        return !!btn && !btn.disabled;
      },
      'hub send button never enabled after typing a message'
    );
    await page.evaluate(() => {
      const cards = [...document.querySelectorAll('.subagent-job')];
      const card = cards.find((c) => (c.textContent || '').includes('audit the release notes'));
      card.querySelector('.subagent-job__message .subagent-job__action').click();
    });
    await waitFor(
      page,
      () => (document.querySelector('[data-panel-toast]')?.textContent || '').includes('message delivered'),
      'hub_send never reported a delivered receipt'
    );

    // View the job's delivered output (job_output -> output pane).
    await page.evaluate(() => {
      const cards = [...document.querySelectorAll('.subagent-job')];
      const card = cards.find((c) => (c.textContent || '').includes('audit the release notes'));
      const buttons = [...card.querySelectorAll('.subagent-job__action')];
      const output = buttons.find((b) => (b.textContent || '').includes('Output'));
      output.click();
    });
    await waitFor(
      page,
      () => {
        const cards = [...document.querySelectorAll('.subagent-job')];
        const card = cards.find((c) => (c.textContent || '').includes('audit the release notes'));
        return !!card && card.querySelector('[data-output-view]') !== null;
      },
      'job output pane never opened'
    );

    // Cancel moves the job out of the default Running filter. Switch to
    // Completed and assert the same card is retained with cancelled status.
    await page.evaluate(() => {
      const cards = [...document.querySelectorAll('.subagent-job')];
      const card = cards.find((c) => (c.textContent || '').includes('audit the release notes'));
      const buttons = [...card.querySelectorAll('.subagent-job__action')];
      const cancel = buttons.find((b) => (b.textContent || '').includes('Cancel'));
      cancel.click();
    });
    await waitFor(
      page,
      () => ![...document.querySelectorAll('.subagent-job')]
        .some((card) => (card.textContent || '').includes('audit the release notes')),
      'cancelled subagent remained under Running',
      30000
    );
    await page.click('.subagents-panel__filter-btn[data-filter="completed"]');
    await waitFor(
      page,
      () => {
        const cards = [...document.querySelectorAll('.subagent-job')];
        const card = cards.find((c) => (c.textContent || '').includes('audit the release notes'));
        return !!card && card.getAttribute('data-status') === 'cancelled';
      },
      'subagent job never appeared as cancelled under Completed',
      30000
    );
    await page.screenshot({ path: `${evidence}/subagents.png`, fullPage: true });

    // Structured Task tool card (main transcript): real `task` tool call with
    // shared context + one child. Assert Goal/Constraints/Contract, child
    // name/agent/status, and that raw args JSON is not the default view.
    const taskCardBefore = await page.evaluate(() => document.querySelectorAll('.tool-card--task').length);
    await page.fill('#prompt-input', 'tool-card coverage task spawn');
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      (b) => document.querySelectorAll('.tool-card--task').length > b,
      'task tool-card never rendered in the transcript',
      45000,
      taskCardBefore,
    );
    const taskCard = await page.evaluate(() => {
      const cards = [...document.querySelectorAll('.tool-card--task')];
      const last = cards[cards.length - 1];
      if (!last) return null;
      const sections = [...last.querySelectorAll('.tool-card__section')].map((s) => ({
        label: s.querySelector('.tool-card__section-label')?.textContent || '',
        body: s.querySelector('.tool-card__section-body')?.textContent || '',
      }));
      const child = last.querySelector('.tool-card__child');
      const argsPre = last.querySelector('.tool-card__args');
      const rawOpen = last.querySelector('details.tool-card__raw[open]');
      return {
        sections,
        childName: child?.querySelector('.tool-card__child-name')?.textContent || '',
        childAgent: child?.querySelector('.tool-card__child-agent')?.textContent || '',
        childStatus: child?.getAttribute('data-status') || child?.querySelector('.tool-card__child-status')?.textContent || '',
        childTarget: child?.querySelector('.tool-card__child-target')?.textContent || '',
        rawVisible: !!argsPre && !last.querySelector('details.tool-card__raw'),
        rawExpanded: !!rawOpen,
        text: last.textContent || '',
      };
    });
    if (!taskCard) fail('task tool-card evaluate returned null');
    const goal = taskCard.sections.find((s) => s.label === 'Goal');
    const constraints = taskCard.sections.find((s) => s.label === 'Constraints');
    const contract = taskCard.sections.find((s) => s.label === 'Contract');
    if (!goal || !goal.body.includes('Ship the Task card')) fail(`task Goal missing: ${JSON.stringify(taskCard.sections)}`);
    if (!constraints || !constraints.body.includes('Be precise')) fail(`task Constraints missing: ${JSON.stringify(taskCard.sections)}`);
    if (!contract || !contract.body.includes('Keep stable ids')) fail(`task Contract missing: ${JSON.stringify(taskCard.sections)}`);
    if (taskCard.childName !== 'Alpha') fail(`task child name wrong: ${taskCard.childName}`);
    if (taskCard.childAgent !== 'writer') fail(`task child agent wrong: ${taskCard.childAgent}`);
    if (!taskCard.childTarget.includes('audit the release notes')) fail(`task child target wrong: ${taskCard.childTarget}`);
    if (!taskCard.childStatus) fail('task child status missing');
    if (taskCard.rawVisible || taskCard.rawExpanded) fail('task card dumped raw JSON as the default view');
    if (taskCard.text.includes('"tasks"') && !taskCard.text.includes('Goal')) fail('task card looks like raw JSON dump');
    if (taskCard.text.includes('Goal null')) fail(`task card rendered "Goal null": ${taskCard.text.slice(0, 200)}`);
    await page.screenshot({ path: `${evidence}/task-card.png`, fullPage: true });

    console.log('web: playwright PASSED (page load, WS connect, prompt round-trip, abort, recovery, todo panel, rich content, workflow create/cancel, settings browse/edit/apply + secret refusal, session info/rename/switch/new, subagents spawn/live/detail-modal/message/output/cancel, task tool card)');
  } finally {
    await browser.close();
  }
}

main().catch((err) => {
  console.error(`web: playwright crashed: ${err.stack || err}`);
  process.exit(2);
});
