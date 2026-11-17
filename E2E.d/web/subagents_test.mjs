// Focused D93 subagents-panel verification against the real fixture
// (orchestration enabled, writer agent, steering mock with the
// "web-e2e-subagent" marker branch). Standalone evidence for the subagents
// acceptance: spawn (task_spawn), live status + activity, running-job detail
// modal (dialog a11y, task/status/elapsed/activity + non-empty recent
// history, Refresh, Escape/Close), hub message (hub_send), output view
// (job_output), cancel (job_cancel).
import { chromium } from 'playwright';
import { clickFeature } from './lib/feature-menu.mjs';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const evidence = process.env.RPI_EVIDENCE || '.';
const chromePath = process.env.RPI_CHROME || '';

function fail(message) {
  throw new Error(`d93: FAIL: ${message}`);
}

async function waitFor(page, fn, label, timeoutMs = 30000, arg) {
  try {
    await page.waitForFunction(fn, arg, { timeout: timeoutMs });
  } catch (err) {
    fail(`${label} (timeout ${timeoutMs}ms)`);
  }
}

// Independent authoritative job_list via HTTP /rpc (never the App's socket,
// so the probe cannot refresh or mask the mounted panel).
async function rpcJobList(page) {
  const response = await page.evaluate(async (bearer) => {
    const headers = { 'content-type': 'application/json' };
    if (bearer) headers.authorization = `Bearer ${bearer}`;
    const result = await fetch('/rpc', {
      method: 'POST',
      headers,
      body: JSON.stringify({ type: 'job_list' }),
    });
    return { status: result.status, text: await result.text() };
  }, token);
  let frame;
  try {
    frame = JSON.parse(response.text);
  } catch {
    fail(`job_list probe returned non-JSON status=${response.status}`);
  }
  if (response.status !== 200 || !frame.success) {
    fail(`job_list probe failed status=${response.status}: ${frame.error || response.text}`);
  }
  return frame.data || {};
}

async function main() {
  if (!url) fail('RPI_URL is required');
  const launchOptions = chromePath ? { executablePath: chromePath } : {};
  const browser = await chromium.launch(launchOptions);
  let page;
  try {
    page = await browser.newPage({ viewport: { width: 1360, height: 900 } });
    if (token) {
      await page.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
    }
    const consoleErrors = [];
    page.on('console', (msg) => { if (msg.type() === 'error') consoleErrors.push(msg.text()); });
    page.on('pageerror', (err) => consoleErrors.push(`pageerror: ${err.message}`));
    // agent_history must use the structured wire: limit only, never the
    // retired `lines`/`text` dual protocol.
    const historyRequests = [];
    // Read-only: the group observed from the App's own job_list frames.
    let firstGroupId = '';
    page.on('websocket', (ws) => {
      ws.on('framesent', (frame) => {
        const payload = frame.payload;
        if (typeof payload === 'string' && payload.includes('agent_history')) historyRequests.push(payload);
      });
      ws.on('framereceived', (frame) => {
        if (typeof frame.payload !== 'string' || !frame.payload.includes('"job_list"')) return;
        try {
          const msg = JSON.parse(frame.payload);
          if (msg?.command === 'job_list' && msg.success === true && typeof msg.data?.groupId === 'string') {
            firstGroupId ||= msg.data.groupId;
          }
        } catch { /* non-JSON frame */ }
      });
    });
    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true, 'WS never connected');

    // --- Subagents panel ---
    await clickFeature(page, '#subagents-toggle-btn');
    await waitFor(page, () => document.getElementById('subagents-panel') !== null, 'subagents panel did not open');
    // Orchestration fixture must be enabled or the spawn form is hidden. The
    // panel fetches job_list asynchronously on mount, so WAIT for the form.
    await waitFor(
      page,
      () => document.getElementById('subagents-panel')?.querySelector('#subagents-spawn-btn') !== null,
      'spawn form hidden (orchestration disabled in fixture?)',
      15000
    );

    // Filter acceptance: Running is the default tab with active ARIA state
    // (aria-selected + aria-pressed) and a reachable tabindex; Completed is
    // the inactive alternative. The bar itself is an ARIA tablist.
    const initialFilter = await page.evaluate(() => {
      const running = document.querySelector('#subagents-filter-running');
      const completed = document.querySelector('#subagents-filter-completed');
      const bar = document.querySelector('.subagents-panel__filter');
      return {
        tablistRole: bar?.getAttribute('role') || '',
        runningSelected: running?.getAttribute('aria-selected') === 'true',
        runningPressed: running?.getAttribute('aria-pressed') === 'true',
        runningActive: !!running?.classList.contains('is-active'),
        runningTabIndex: running?.getAttribute('tabindex'),
        completedSelected: completed?.getAttribute('aria-selected') === 'false',
        completedPressed: completed?.getAttribute('aria-pressed') === 'false',
      };
    });
    if (initialFilter.tablistRole !== 'tablist') {
      fail(`filter bar must be role=tablist, got "${initialFilter.tablistRole}"`);
    }
    if (!initialFilter.runningSelected) fail('Running must be the default selected filter');
    if (!initialFilter.runningPressed) fail('Running filter must set aria-pressed=true');
    if (!initialFilter.runningActive) fail('Running filter must carry the is-active class');
    if (initialFilter.runningTabIndex !== '0') {
      fail(`active filter tab must be tabindex=0, got "${initialFilter.runningTabIndex}"`);
    }
    if (!initialFilter.completedSelected) fail('Completed must not be selected initially');
    if (!initialFilter.completedPressed) fail('Completed filter must set aria-pressed=false');
    // Tabs pattern keyboard semantics: ArrowRight moves selection + focus to
    // the next tab; back to Running afterwards for the spawn flow.
    await page.focus('#subagents-filter-running');
    await page.keyboard.press('ArrowRight');
    await waitFor(
      page,
      () => document.querySelector('#subagents-filter-completed')?.getAttribute('aria-selected') === 'true',
      'ArrowRight did not activate the Completed tab'
    );
    const focusedTab = await page.evaluate(() => document.activeElement?.id || '');
    if (focusedTab !== 'subagents-filter-completed') {
      fail(`ArrowRight focus did not move to Completed: "${focusedTab}"`);
    }
    await page.click('#subagents-filter-running');
    await waitFor(
      page,
      () => document.querySelector('#subagents-filter-running')?.getAttribute('aria-selected') === 'true',
      'Running tab never reactivated after arrow-key navigation'
    );

    const detailsTask = [
      'web-e2e-subagent-details: audit the release notes and report findings.',
      'TASKHEAD the detail modal must render the complete assignment body.',
      'filler '.repeat(180).trim(),
      'TASKMID-中间-标记 this middle marker must sit beyond the 1024-character description cap.',
      'Markdown proof: **TASK-BOLD**, inline `src/task-proof.rs`, relative [proof](src/task-proof.rs), literal <img src=x onerror="window.__taskXss=1"> stays escaped.',
      'A trailing segment of roughly two hundred characters follows the middle marker before',
      'the fixture ends; every acceptance sentence stays in the task body and it still ends on DETAILSTAIL',
    ].join(' ');
    if (detailsTask.length <= 1024 || !detailsTask.endsWith('DETAILSTAIL')
      || detailsTask.indexOf('TASKHEAD') < 0 || detailsTask.indexOf('TASKMID-中间-标记') <= 1024) {
      fail(`details task fixture is not >1024 chars ending DETAILSTAIL (${detailsTask.length})`);
    }
    await page.selectOption('#subagents-agent-select', 'writer');
    await page.fill('#subagents-task-input', detailsTask);
    await page.click('#subagents-spawn-btn');

    // Job card appears with the task summary (description).
    await waitFor(
      page,
      () => {
        const cards = [...document.querySelectorAll('.subagent-job')];
        return cards.some((c) => (c.textContent || '').includes('web-e2e-subagent-details'));
      },
      'spawned subagent job never appeared in the panel'
    );
    const status = () =>
      page.evaluate(() => {
        const cards = [...document.querySelectorAll('.subagent-job')];
        const card = cards.find((c) => (c.textContent || '').includes('web-e2e-subagent-details'));
        return card ? card.getAttribute('data-status') || '' : '';
      });
    const live = await status();
    if (!['queued', 'running'].includes(live)) {
      fail(`spawned subagent must be live (queued/running), got "${live}"`);
    }
    // Live activity/elapsed one-liner rendered.
    const progress = await page.evaluate(() => {
      const cards = [...document.querySelectorAll('.subagent-job')];
      const card = cards.find((c) => (c.textContent || '').includes('web-e2e-subagent-details'));
      const line = card ? card.querySelector('[data-progress-line]') : null;
      return line ? line.textContent || '' : '';
    });
    if (!progress.trim()) fail('no live activity/elapsed line rendered');
    await page.screenshot({ path: `${evidence}/spawned.png`, fullPage: true });

    // Filter acceptance (live job): switching to Completed removes the running
    // card from the list and the empty state follows the active tab; switching
    // back to Running restores it (new spawns land here by default).
    await page.click('#subagents-filter-completed');
    await waitFor(
      page,
      () => {
        const cards = [...document.querySelectorAll('.subagent-job')];
        return !cards.some((c) => (c.textContent || '').includes('web-e2e-subagent-details'));
      },
      'live job still visible under the Completed filter'
    );
    const completedEmpty = await page.evaluate(() => {
      const empty = document.querySelector('[data-filter-empty]');
      return empty ? empty.textContent || '' : '';
    });
    if (!completedEmpty.includes('No settled subagent jobs yet')) {
      fail(`Completed filter empty state wrong while live: "${completedEmpty}"`);
    }
    await page.click('#subagents-filter-running');
    await waitFor(
      page,
      () => {
        const cards = [...document.querySelectorAll('.subagent-job')];
        return cards.some((c) => (c.textContent || '').includes('web-e2e-subagent-details'));
      },
      'live job never reappeared after switching back to Running'
    );
    // Filter switches must not reset per-job UI state: the open output pane
    // survives a Running -> Completed -> Running round trip (outputJobId is
    // panel-level state, the card is only hidden while filtered out). The
    // toggle button reads "Hide output" while the pane is open.
    await page.evaluate(() => {
      const cards = [...document.querySelectorAll('.subagent-job')];
      const card = cards.find((c) => (c.textContent || '').includes('web-e2e-subagent-details'));
      [...card.querySelectorAll('.subagent-job__action')]
        .find((b) => /output/i.test(b.textContent || ''))
        .click();
    });
    await waitFor(
      page,
      () => {
        const cards = [...document.querySelectorAll('.subagent-job')];
        const card = cards.find((c) => (c.textContent || '').includes('web-e2e-subagent-details'));
        return !!card && card.querySelector('[data-output-view]') !== null;
      },
      'job output pane never opened (filter-survival precondition)'
    );
    await page.click('#subagents-filter-completed');
    await page.click('#subagents-filter-running');
    await waitFor(
      page,
      () => {
        const cards = [...document.querySelectorAll('.subagent-job')];
        const card = cards.find((c) => (c.textContent || '').includes('web-e2e-subagent-details'));
        return !!card && card.querySelector('[data-output-view]') !== null;
      },
      'output pane did not survive the filter round trip'
    );
    // Close it again so the later dedicated output section starts clean.
    await page.evaluate(() => {
      const cards = [...document.querySelectorAll('.subagent-job')];
      const card = cards.find((c) => (c.textContent || '').includes('web-e2e-subagent-details'));
      [...card.querySelectorAll('.subagent-job__action')]
        .find((b) => /output/i.test(b.textContent || ''))
        .click();
    });
    await waitFor(
      page,
      () => {
        const cards = [...document.querySelectorAll('.subagent-job')];
        const card = cards.find((c) => (c.textContent || '').includes('web-e2e-subagent-details'));
        return !card || card.querySelector('[data-output-view]') === null;
      },
      'output pane never closed after the filter round trip'
    );

    // --- Running-job detail modal (D93) ---
    // The job is still running (the mock streams slowly). Open the modal via
    // the Details action and assert an accessible dialog with the task
    // description, live status/elapsed, latest activity, and NON-EMPTY recent
    // history — the acceptance that fails when a long-running job exposes only
    // status/output. Then exercise Refresh (refetch keeps the transcript live
    // with no error), close via Escape, reopen by clicking the card, and close
    // via the Close button, before continuing the hub/output/cancel flow.
    await page.evaluate(() => {
      const cards = [...document.querySelectorAll('.subagent-job')];
      const card = cards.find((c) => (c.textContent || '').includes('web-e2e-subagent-details'));
      card.querySelector('[data-details-trigger]').click();
    });
    await waitFor(
      page,
      () => document.querySelector('[data-details-dialog]') !== null,
      'detail modal never opened (Details trigger)'
    );
    // Dialog accessibility + correct job binding. Initial focus lands on Close.
    const detailsA11y = await page.evaluate(() => {
      const dlg = document.querySelector('[data-details-dialog]');
      const close = document.querySelector('[data-details-close]');
      const card = [...document.querySelectorAll('.subagent-job')]
        .find((c) => (c.textContent || '').includes('web-e2e-subagent-details'));
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
    if (detailsA11y.ariaModal !== 'true') fail(`detail modal must set aria-modal=true`);
    if (detailsA11y.ariaLabel !== 'Subagent job details') fail(`detail modal aria-label mismatch: "${detailsA11y.ariaLabel}"`);
    if (!detailsA11y.jobId || detailsA11y.jobId !== detailsA11y.cardJobId) {
      fail(`detail modal bound to wrong job: dialog=${detailsA11y.jobId} card=${detailsA11y.cardJobId}`);
    }
    if (!detailsA11y.focusOnClose) fail('detail modal initial focus did not land on the Close button');
    // The details modal is not tied to the list filter: a tab switch keeps it
    // open and bound to the same job (detailsJobId is panel-level state).
    // Programmatic clicks: the modal backdrop covers the panel, so a real
    // pointer click would (correctly) be intercepted by the backdrop.
    await page.evaluate(() => {
      document.querySelector('#subagents-filter-completed').click();
      document.querySelector('#subagents-filter-running').click();
    });
    const modalAfterFilter = await page.evaluate(() => {
      const dlg = document.querySelector('[data-details-dialog]');
      const card = [...document.querySelectorAll('.subagent-job')]
        .find((c) => (c.textContent || '').includes('web-e2e-subagent-details'));
      return {
        open: !!dlg,
        jobId: dlg?.getAttribute('data-job-id') || '',
        cardJobId: card?.getAttribute('data-job-id') || '',
      };
    });
    if (!modalAfterFilter.open) fail('detail modal closed when the filter changed');
    if (modalAfterFilter.jobId !== modalAfterFilter.cardJobId) {
      fail('detail modal rebound to a different job after filter change');
    }
    // The Task body is a durable fetch: Markdown only exists once available.
    await waitFor(
      page,
      () => document.querySelector('[data-details-description]')?.getAttribute('data-assignment-status') === 'available',
      'detail assignment never became available',
      20000
    );
    // Running job: full >1024 task body rendered as Markdown (strong + inline
    // code + relative link), hostile HTML escaped, typed status, elapsed. No
    // live-word badge and no Latest activity until a hub message exists.
    const detailsContent = await page.evaluate(() => {
      const txt = (sel) => (document.querySelector(sel)?.textContent || '').trim();
      const status = document.querySelector('[data-details-status] [role="status"]');
      const root = document.querySelector('[data-details-description]');
      const task = document.querySelector('[data-details-task-section]');
      const anchor = root?.querySelector('a[href="src/task-proof.rs"]');
      const beforeJob = document.querySelector('[data-details-dialog]')?.getAttribute('data-job-id') || '';
      const click = new MouseEvent('click', { bubbles: true, cancelable: true, view: window });
      anchor?.dispatchEvent(click);
      return {
        description: txt('[data-details-description]'),
        status: status?.textContent.trim() || '',
        statusRole: status?.getAttribute('role') || '',
        elapsed: txt('[data-details-elapsed]'),
        activity: document.querySelector('[data-details-activity]') !== null,
        liveWord: document.querySelector('[data-details-live]') !== null
          || /\blive\b/i.test(document.querySelector('[data-details-dialog]')?.textContent || ''),
        pathNavigation: root?.getAttribute('data-path-navigation') || '',
        strong: root?.querySelector('strong')?.textContent || '',
        code: root?.querySelector('code.md-code')?.textContent || '',
        href: anchor?.getAttribute('href') || '',
        clickUrl: anchor?.href || '',
        expectedHref: new URL('src/task-proof.rs', location.href).href,
        imgCount: task?.querySelectorAll('img').length ?? 0,
        literalHtml: (root?.textContent || '').includes('<img src=x onerror="window.__taskXss=1">'),
        xss: typeof window.__taskXss,
        linkPrevented: click.defaultPrevented,
        sameJob: beforeJob !== '' && beforeJob === document.querySelector('[data-details-dialog]')?.getAttribute('data-job-id'),
      };
    });
    if (detailsContent.description.length <= 1024
      || !detailsContent.description.includes('TASKHEAD')
      || !detailsContent.description.includes('TASKMID-中间-标记')
      || !detailsContent.description.endsWith('DETAILSTAIL')) {
      console.log(JSON.stringify({
        descLength: detailsContent.description.length,
        hasTaskHead: detailsContent.description.includes('TASKHEAD'),
        hasTaskMid: detailsContent.description.includes('TASKMID-中间-标记'),
        endsTail: detailsContent.description.endsWith('DETAILSTAIL'),
        ellipsis: detailsContent.description.includes('\u2026'),
      }));
      await page.screenshot({ path: `${evidence}/details-task-1024-crop.png`, fullPage: true });
      fail(`task description cropped before DETAILSTAIL (descLength=${detailsContent.description.length}, hasTaskMid=${detailsContent.description.includes('TASKMID-中间-标记')})`);
    }
    if (detailsContent.pathNavigation !== 'disabled' || detailsContent.imgCount !== 0
      || detailsContent.xss !== 'undefined' || !detailsContent.linkPrevented || !detailsContent.sameJob
      || !detailsContent.literalHtml) {
      fail(`Task body escaped its ownerless Markdown boundary: ${JSON.stringify(detailsContent)}`);
    }
    if (detailsContent.strong !== 'TASK-BOLD' || detailsContent.code !== 'src/task-proof.rs'
      || detailsContent.href !== 'src/task-proof.rs' || detailsContent.clickUrl !== detailsContent.expectedHref) {
      fail(`Task Markdown lost strong/code/relative link: ${JSON.stringify({ strong: detailsContent.strong, code: detailsContent.code, href: detailsContent.href, clickUrl: detailsContent.clickUrl })}`);
    }
    const beforeClick = await page.evaluate(() => [location.href, document.querySelector('[data-details-dialog]')?.getAttribute('data-job-id') || '']);
    await page.click('[data-details-description] a[href="src/task-proof.rs"]', { noWaitAfter: true });
    const afterClick = await page.evaluate(() => [location.href, document.querySelector('[data-details-dialog]')?.getAttribute('data-job-id') || '']);
    if (afterClick[0] !== beforeClick[0] || afterClick[1] !== beforeClick[1] || afterClick[1] === '') {
      fail(`real relative-link click navigated the ownerless modal: ${JSON.stringify({ beforeClick, afterClick })}`);
    }
    if (detailsContent.statusRole !== 'status' || !['queued', 'running'].includes(detailsContent.status.toLowerCase())) {
      fail(`detail status must be a typed live status, got "${detailsContent.status}"`);
    }
    if (!detailsContent.elapsed) fail('detail modal elapsed never rendered while running');
    if (detailsContent.activity) fail('Latest activity rendered without a hub message');
    if (detailsContent.liveWord) fail('detail modal still shows a live-word badge');
    const readPath = 'fixtures/subagent-details/workspace-relative/long-read-target-for-subagent-details-readability/long-relative-read-target.txt';
    await waitFor(
      page,
      (want) => (document.querySelector('[data-details-history]')?.textContent || '').includes(want),
      'detail history never showed the full relative read path',
      20000,
      readPath
    );
    // Structured history through the SHARED transcript surface: thinking fold,
    // markdown prose, and real ToolCards (read-text, read-image, grep, bash)
    // with args bound to results by toolCallId. The child may still be
    // streaming the hold turn, so the bash card is waited for, not assumed.
    await waitFor(
      page,
      () => {
        const root = document.querySelector('[data-details-history]');
        return !!root
          && root.querySelector('details.thinking') !== null
          && root.querySelector('.tool-card[data-tool-name="read"]') !== null
          && root.querySelector('.tool-card[data-tool-name="grep"]') !== null;
      },
      'structured history never showed thinking + read + grep cards',
      20000
    );
    const structuralProbe = await page.evaluate((path) => {
      const root = document.querySelector('[data-details-history]');
      const thinking = root?.querySelector('details.thinking');
      const assistant = root?.querySelector('.msg--assistant');
      const card = (name) => root?.querySelector(`.tool-card[data-tool-name="${name}"]`);
      const reads = [...(root?.querySelectorAll('.tool-card[data-tool-name="read"]') || [])];
      const readTxt = reads.find((c) => (c.textContent || '').includes(path));
      const readImg = reads.find((c) => c.querySelector('.tool-media__image') !== null);
      const bash = card('bash');
      const grep = card('grep');
      return {
        thinkingMarker: thinking?.querySelector('.thinking__body')?.textContent || '',
        thinkingOpen: thinking?.open ?? null,
        assistantStrong: assistant?.querySelector('.assistant-text strong')?.textContent || '',
        assistantHasDetailstail: !!assistant && (assistant.textContent || '').includes('DETAILSTAIL'),
        readTxtCallId: readTxt?.getAttribute('data-tool-call-id') || '',
        readTxtStatus: readTxt?.getAttribute('data-tool-status') || '',
        readTxtHasPath: !!readTxt && (readTxt.textContent || '').includes(path),
        readImgHasImage: !!readImg,
        grepCallId: grep?.getAttribute('data-tool-call-id') || '',
        grepHasPattern: !!grep && (grep.textContent || '').includes('DETAILSTAIL'),
        bashCommand: bash?.querySelector('.tool-card__command-text')?.textContent || '',
        bashOutput: bash?.querySelector('.tool-card__output')?.textContent || '',
      };
    }, readPath);
    if (!structuralProbe.thinkingMarker.includes('subagent-details-thinking-marker')) {
      fail(`thinking body missing the reasoning marker: "${structuralProbe.thinkingMarker}"`);
    }
    if (structuralProbe.thinkingOpen !== true) fail('thinking fold must default open');
    if (structuralProbe.assistantStrong !== 'bold-plan') {
      fail(`assistant prose did not render markdown <strong>: "${structuralProbe.assistantStrong}"`);
    }
    if (!structuralProbe.assistantHasDetailstail) fail('assistant prose does not show DETAILSTAIL');
    if (!structuralProbe.readTxtCallId || structuralProbe.readTxtStatus !== 'done' || !structuralProbe.readTxtHasPath) {
      fail(`read card lacks id/done/full-path args: ${JSON.stringify(structuralProbe)}`);
    }
    if (!structuralProbe.readImgHasImage) fail('image tool result did not render a media image');
    if (!structuralProbe.grepCallId || !structuralProbe.grepHasPattern) {
      fail(`grep card lacks id/pattern args: ${JSON.stringify(structuralProbe)}`);
    }
    // The bash card lands once the child's bash tool result is recorded —
    // before the hold turn. Its card binds command args to the result output.
    await waitFor(
      page,
      () => (document.querySelector('[data-details-history] .tool-card[data-tool-name="bash"] .tool-card__output')?.textContent || '').includes('DETAILSOUTTAIL'),
      'bash card never bound the DETAILSOUTTAIL result to its command',
      20000
    );
    // Thinking fold is a real <details>: closing hides the body, opening shows.
    const foldProbe = await page.evaluate(() => {
      const thinking = document.querySelector('[data-details-history] details.thinking');
      const body = thinking?.querySelector('.thinking__body');
      if (!thinking || !body) return { found: false };
      thinking.open = false;
      const hiddenH = body.getBoundingClientRect().height;
      thinking.open = true;
      return { found: true, hiddenH, openH: body.getBoundingClientRect().height };
    });
    if (!foldProbe.found || foldProbe.hiddenH !== 0 || foldProbe.openH <= 0) {
      fail(`thinking fold is not collapsible: ${JSON.stringify(foldProbe)}`);
    }
    // Header never covers the bounded transcript scroller: the head block ends
    // at or above the body's top edge and the body is the only overflow region.
    const occlusion = await page.evaluate(() => {
      const head = document.querySelector('.subagent-details__head')?.getBoundingClientRect();
      const body = document.querySelector('.subagent-details__body');
      const rect = body?.getBoundingClientRect();
      const style = body ? getComputedStyle(body) : null;
      return {
        headBottom: head?.bottom ?? -1,
        bodyTop: rect?.top ?? -1,
        bodyScrollable: !!style && style.overflowY === 'auto' && !!body && body.scrollHeight >= body.clientHeight,
      };
    });
    if (occlusion.headBottom > occlusion.bodyTop + 1) {
      fail(`dialog header overlaps the transcript body: ${JSON.stringify(occlusion)}`);
    }
    if (!occlusion.bodyScrollable) fail('detail body is not the bounded scroller');
    await page.screenshot({ path: `${evidence}/details-structured-desktop.png`, fullPage: true });
    // Refresh re-fetches the child transcript: the dialog stays open, history
    // keeps the concrete read action, and no error appears.
    await page.evaluate(() => document.querySelector('[data-details-refresh]').click());
    await waitFor(
      page,
      () => {
        const pre = document.querySelector('[data-details-history]');
        return (
          !!pre &&
          pre.textContent.includes('fixtures/subagent-details/workspace-relative/long-read-target-for-subagent-details-readability/long-relative-read-target.txt') &&
          document.querySelector('[data-details-error]') === null
        );
      },
      'detail modal Refresh broke the concrete recent history view',
      10000
    );
    // Close via Escape dismisses the modal.
    await page.keyboard.press('Escape');
    await waitFor(
      page,
      () => document.querySelector('[data-details-dialog]') === null,
      'detail modal did not close on Escape'
    );
    // Reopen by clicking the running job card itself (the card onClick path):
    // the previous modal's structured history must NOT linger — it reloads
    // from the wire — then close via the Close button (both dismissal paths).
    await page.evaluate(() => {
      const cards = [...document.querySelectorAll('.subagent-job')];
      const card = cards.find((c) => (c.textContent || '').includes('web-e2e-subagent-details'));
      // Click the description (no stopPropagation) so the section onClick opens
      // the modal rather than landing on the message input or action buttons.
      card.querySelector('.subagent-job__description').click();
    });
    await waitFor(
      page,
      () => document.querySelector('[data-details-dialog]') !== null,
      'detail modal never opened on card click'
    );
    await waitFor(
      page,
      () => {
        const root = document.querySelector('[data-details-history]');
        return !!root && root.querySelector('.tool-card[data-tool-name="bash"]') !== null
          && document.querySelector('[data-details-error]') === null;
      },
      'reopened modal never repopulated the structured history (stale/empty state)',
      20000
    );
    // Wire check: every agent_history request used the new protocol — limit
    // present, retired `lines`/`text` keys absent (no dual protocol).
    if (historyRequests.length === 0) fail('no agent_history request was captured on the wire');
    for (const payload of historyRequests) {
      const parsed = JSON.parse(payload);
      const req = parsed && typeof parsed === 'object' && 'cmd' in parsed ? parsed.cmd : parsed;
      if (typeof req.limit !== 'number') fail(`agent_history request lacks limit: ${payload.slice(0, 200)}`);
      if ('lines' in req || 'text' in req) fail(`agent_history request used the retired wire: ${payload.slice(0, 200)}`);
    }
    await page.evaluate(() => document.querySelector('[data-details-close]').click());
    await waitFor(
      page,
      () => document.querySelector('[data-details-dialog]') === null,
      'detail modal did not close on Close button'
    );

    // Message the subagent via hub_send. React controlled inputs ignore
    // `input.value=`; use the native setter so onChange fires.
    await page.evaluate(() => {
      const cards = [...document.querySelectorAll('.subagent-job')];
      const card = cards.find((c) => (c.textContent || '').includes('web-e2e-subagent-details'));
      const input = card.querySelector('.subagent-job__message-input');
      const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
      setter.call(input, 'status report?');
      input.dispatchEvent(new Event('input', { bubbles: true }));
    });
    await waitFor(
      page,
      () => {
        const cards = [...document.querySelectorAll('.subagent-job')];
        const card = cards.find((c) => (c.textContent || '').includes('web-e2e-subagent-details'));
        const btn = card ? card.querySelector('.subagent-job__message .subagent-job__action') : null;
        return !!btn && !btn.disabled;
      },
      'hub send button never enabled'
    );
    await page.evaluate(() => {
      const cards = [...document.querySelectorAll('.subagent-job')];
      const card = cards.find((c) => (c.textContent || '').includes('web-e2e-subagent-details'));
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
      const card = cards.find((c) => (c.textContent || '').includes('web-e2e-subagent-details'));
      [...card.querySelectorAll('.subagent-job__action')]
        .find((b) => (b.textContent || '').includes('Output'))
        .click();
    });
    await waitFor(
      page,
      () => {
        const cards = [...document.querySelectorAll('.subagent-job')];
        const card = cards.find((c) => (c.textContent || '').includes('web-e2e-subagent-details'));
        return !!card && card.querySelector('[data-output-view]') !== null;
      },
      'job output pane never opened'
    );

    // Cancel the job. The default Running filter hides the card the moment it
    // settles, so this doubles as the live->settled filter acceptance: the
    // card must disappear from Running and reappear under Completed.
    await page.evaluate(() => {
      const cards = [...document.querySelectorAll('.subagent-job')];
      const card = cards.find((c) => (c.textContent || '').includes('web-e2e-subagent-details'));
      [...card.querySelectorAll('.subagent-job__action')]
        .find((b) => (b.textContent || '').includes('Cancel'))
        .click();
    });
    await waitFor(
      page,
      () => {
        const cards = [...document.querySelectorAll('.subagent-job')];
        return !cards.some((c) => (c.textContent || '').includes('web-e2e-subagent-details'));
      },
      'settled job never disappeared from the Running filter'
    );
    // Running view now shows the filtered empty state (jobs exist, none live).
    const runningEmpty = await page.evaluate(() => {
      const empty = document.querySelector('[data-filter-empty]');
      return empty ? empty.textContent || '' : '';
    });
    if (!runningEmpty.includes('No active subagent jobs')) {
      fail(`Running filter empty state wrong after settle: "${runningEmpty}"`);
    }
    // Header aggregate stays GLOBAL — filtering never drops job state.
    const countsAfterSettle = await page.evaluate(() =>
      document.getElementById('subagents-counts')?.textContent || ''
    );
    if (!countsAfterSettle.includes('1 cancelled')) {
      fail(`header aggregate lost the settled job: "${countsAfterSettle}"`);
    }
    await page.screenshot({ path: `${evidence}/settled-running.png`, fullPage: true });

    // Switch to Completed: the settled card reappears with its final status.
    await page.click('#subagents-filter-completed');
    await waitFor(
      page,
      () => {
        const cards = [...document.querySelectorAll('.subagent-job')];
        const card = cards.find((c) => (c.textContent || '').includes('web-e2e-subagent-details'));
        return !!card && card.getAttribute('data-status') === 'cancelled';
      },
      'settled job never appeared under the Completed filter'
    );
    const completedActive = await page.evaluate(() => {
      const btn = document.querySelector('#subagents-filter-completed');
      return (
        btn?.getAttribute('aria-selected') === 'true' &&
        btn?.getAttribute('aria-pressed') === 'true' &&
        (btn?.classList.contains('is-active') || false)
      );
    });
    if (!completedActive) fail('Completed tab not active after switching');
    await page.screenshot({ path: `${evidence}/cancelled.png`, fullPage: true });

    // 切回 Running: the settled card is hidden again and the empty state
    // returns; then back to Completed so the mobile check sees content.
    await page.click('#subagents-filter-running');
    await waitFor(
      page,
      () => document.querySelector('[data-filter-empty]') !== null,
      'Running filter did not show empty state after switching back'
    );
    await page.click('#subagents-filter-completed');

    // Truncated-history child: one reply row larger than the agent_history
    // byte cap leaves an EMPTY slice whose truncation notice names the real
    // reason and count — never a silent empty transcript or a fake 'none'.
    await page.fill('#subagents-task-input', 'web-e2e-subagent-truncated: emit one oversized reply row');
    await page.click('#subagents-spawn-btn');
    await waitFor(
      page,
      () => {
        const cards = [...document.querySelectorAll('.subagent-job')];
        const card = cards.find((c) => (c.textContent || '').includes('web-e2e-subagent-truncated'));
        return !!card && ['completed', 'failed', 'cancelled'].includes(card.getAttribute('data-status') || '');
      },
      'truncated child never settled under the Completed filter',
      20000
    );
    await page.evaluate(() => {
      const card = [...document.querySelectorAll('.subagent-job')]
        .find((c) => (c.textContent || '').includes('web-e2e-subagent-truncated'));
      card.querySelector('[data-details-trigger]').click();
    });
    await waitFor(
      page,
      () => {
        const notice = document.querySelector('[data-details-truncation]');
        return !!notice && notice.getAttribute('data-truncation-reason') === 'bytes'
          && document.querySelector('[data-details-error]') === null;
      },
      'byte-truncated child never surfaced the explicit truncation notice',
      20000
    );
    const truncationProbe = await page.evaluate(() => ({
      notice: document.querySelector('[data-details-truncation]')?.textContent || '',
      cards: document.querySelectorAll('[data-details-history] .tool-card, [data-details-history] .msg').length,
      empty: document.querySelector('[data-details-empty]')?.textContent || '',
    }));
    if (!/older messages hidden \(size limit\)/.test(truncationProbe.notice)) {
      fail(`truncation notice does not name the real reason/count: "${truncationProbe.notice}"`);
    }
    if (truncationProbe.cards !== 0 || !truncationProbe.empty.includes('size limit')) {
      fail(`empty byte-truncated slice rendered stale rows or wrong empty text: ${JSON.stringify(truncationProbe)}`);
    }
    await page.screenshot({ path: `${evidence}/details-truncated.png`, fullPage: true });
    await page.keyboard.press('Escape');
    await waitFor(
      page,
      () => document.querySelector('[data-details-dialog]') === null,
      'truncated-child modal did not close on Escape'
    );

    // Mounted panel, no remount/refresh: the natural child completed, so the
    // live Running view must hold no card and the header no live count.
    await page.click('#subagents-filter-running');
    await waitFor(
      page,
      () => {
        const header = (document.getElementById('subagents-counts')?.textContent || '').trim();
        return document.querySelector('#subagents-filter-running')?.getAttribute('aria-selected') === 'true'
          && document.querySelectorAll('#subagents-job-list .subagent-job').length === 0
          && (document.querySelector('[data-filter-empty]')?.textContent || '').includes('No active subagent jobs')
          && !/\b(queued|running|cancelling)\b/.test(header);
      },
      'Running never cleared after the natural completion',
      15000
    );
    const runningTerminal = await page.evaluate(() => ({
      cards: [...document.querySelectorAll('#subagents-job-list .subagent-job')].map((c) => [c.dataset.jobId, c.dataset.status]),
      header: (document.getElementById('subagents-counts')?.textContent || '').trim(),
      empty: document.querySelector('[data-filter-empty]')?.textContent || '',
      selected: document.querySelector('#subagents-filter-running')?.getAttribute('aria-selected'),
    }));
    if (runningTerminal.selected !== 'true') fail('Running filter not selected after the natural completion');
    if (runningTerminal.cards.length !== 0) {
      fail(`completed child still rendered under Running: ${JSON.stringify(runningTerminal.cards)}`);
    }
    if (!/No active subagent jobs/.test(runningTerminal.empty)) {
      fail(`Running empty state missing after natural completion: "${runningTerminal.empty}"`);
    }
    if (/\b(queued|running|cancelling)\b/.test(runningTerminal.header)) {
      fail(`header still advertises live counts after completion: "${runningTerminal.header}"`);
    }
    // Independent HTTP /rpc truth (never the App socket): same group, all
    // terminal, exact natural child completed.
    const snapshot = await rpcJobList(page);
    if (!snapshot.groupId || (firstGroupId && snapshot.groupId !== firstGroupId)) {
      fail(`job_list group changed: "${firstGroupId}" -> "${snapshot.groupId}"`);
    }
    const liveBackend = snapshot.jobs.filter((job) => !['completed', 'failed', 'cancelled'].includes(job.status));
    if (liveBackend.length !== 0) {
      fail(`authoritative job_list is not all-terminal: ${liveBackend.map((j) => `${j.id}:${j.status}`).join(',')}`);
    }
    const counts = ['running', 'cancelling', 'queued', 'completed', 'failed', 'cancelled']
      .map((status) => [status, snapshot.jobs.filter((job) => job.status === status).length])
      .filter(([, n]) => n > 0)
      .map(([status, n]) => `${n} ${status}`);
    const expectedHeader = snapshot.jobs.length === 0 ? 'no jobs' : `Jobs · ${counts.join(' · ')}`;
    if (runningTerminal.header !== expectedHeader) {
      fail(`mounted header does not map the authoritative job_list: "${runningTerminal.header}" vs "${expectedHeader}"`);
    }
    await page.screenshot({ path: `${evidence}/terminal-running-desktop.png`, fullPage: true });
    await page.click('#subagents-filter-completed');
    await waitFor(
      page,
      (want) => JSON.stringify([...document.querySelectorAll('#subagents-job-list .subagent-job')]
        .map((card) => `${card.dataset.jobId}:${card.dataset.status}`).sort()) === JSON.stringify(want),
      'Completed never mapped the backend retained job ids/statuses exactly',
      15000,
      snapshot.jobs.map((job) => `${job.id}:${job.status}`).sort()
    );
    const completedChild = await page.evaluate(() => {
      const card = [...document.querySelectorAll('#subagents-job-list .subagent-job')]
        .find((node) => (node.textContent || '').includes('web-e2e-subagent-truncated'));
      return { id: card?.dataset.jobId || '', status: card?.dataset.status || '' };
    });
    if (completedChild.status !== 'completed'
      || !snapshot.jobs.some((job) => job.id === completedChild.id && job.status === 'completed'
        && (job.description || '').includes('web-e2e-subagent-truncated'))) {
      fail(`natural child not completed in the backend retained set: ${JSON.stringify(completedChild)}`);
    }

    // Markdown output view: the short-marker child settles NATURALLY with a
    // markdown yield payload (## heading + ```bash fence). Its output pane
    // must render structured markdown blocks — an h2 heading and a fenced
    // code <pre> — never the legacy plain-text pre (or raw "##" source).
    await page.fill('#subagents-task-input', 'web-e2e-subagent: audit the release notes and report findings');
    await page.click('#subagents-spawn-btn');
    await waitFor(
      page,
      () => {
        const cards = [...document.querySelectorAll('.subagent-job')];
        const card = cards.find((c) => (c.textContent || '').includes('web-e2e-subagent: audit'));
        return !!card && card.getAttribute('data-status') === 'completed';
      },
      'markdown-output child never settled under the Completed filter',
      60000
    );
    await page.evaluate(() => {
      const cards = [...document.querySelectorAll('.subagent-job')];
      const card = cards.find((c) => (c.textContent || '').includes('web-e2e-subagent: audit'));
      [...card.querySelectorAll('.subagent-job__action')]
        .find((b) => /output/i.test(b.textContent || ''))
        .click();
    });
    await waitFor(
      page,
      () => {
        const cards = [...document.querySelectorAll('.subagent-job')];
        const card = cards.find((c) => (c.textContent || '').includes('web-e2e-subagent: audit'));
        return !!card && card.querySelector('[data-output-view]') !== null;
      },
      'markdown output pane never opened'
    );
    const markdownOutput = await page.evaluate(() => {
      const cards = [...document.querySelectorAll('.subagent-job')];
      const card = cards.find((c) => (c.textContent || '').includes('web-e2e-subagent: audit'));
      const view = card?.querySelector('[data-output-view]');
      return {
        h2: view?.querySelector('.md-h2')?.textContent || '',
        fencePre: view?.querySelector('.md-fence pre') !== null,
        fenceLang: view?.querySelector('.md-fence__lang')?.textContent || '',
        legacyPre: view?.querySelector('pre.subagent-job__output-text') !== null,
        rawMarkdown: (view?.textContent || '').includes('## '),
      };
    });
    if (!markdownOutput.h2.includes('标题')) fail(`output markdown lost the h2 heading: "${markdownOutput.h2}"`);
    if (!markdownOutput.fencePre) fail('output markdown lost the bash fenced code pre');
    if (!markdownOutput.fenceLang.includes('bash')) fail(`output fence lost the bash lang label: "${markdownOutput.fenceLang}"`);
    if (markdownOutput.legacyPre) fail('output view still renders the legacy plain-text pre');
    if (markdownOutput.rawMarkdown) fail('output view shows raw markdown source instead of rendered blocks');
    // Close the pane so the later mobile pass starts clean.
    await page.evaluate(() => {
      const cards = [...document.querySelectorAll('.subagent-job')];
      const card = cards.find((c) => (c.textContent || '').includes('web-e2e-subagent: audit'));
      [...card.querySelectorAll('.subagent-job__action')]
        .find((b) => /output/i.test(b.textContent || ''))
        .click();
    });
    await waitFor(
      page,
      () => {
        const cards = [...document.querySelectorAll('.subagent-job')];
        const card = cards.find((c) => (c.textContent || '').includes('web-e2e-subagent: audit'));
        return !card || card.querySelector('[data-output-view]') === null;
      },
      'markdown output pane never closed'
    );

    // Mobile: no horizontal overflow — page, panel, and the filter bar all
    // fit a 390px phone viewport.
    await page.setViewportSize({ width: 390, height: 844 });
    await page.screenshot({ path: `${evidence}/terminal-completed-mobile.png`, fullPage: true });
    await page.evaluate(() => {
      const card = [...document.querySelectorAll('.subagent-job')]
        .find((node) => (node.textContent || '').includes('web-e2e-subagent-details'));
      card?.querySelector('[data-details-trigger]')?.click();
    });
    await waitFor(
      page,
      () => {
        const root = document.querySelector('[data-details-history]');
        return !!root
          && (root.textContent || '').includes('long-relative-read-target.txt')
          && root.querySelector('details.thinking') !== null
          && root.querySelector('.tool-card[data-tool-name="bash"]') !== null;
      },
      'mobile detail history never showed the structured transcript (read path + thinking + bash card)',
      15000
    );
    const mobile = await page.evaluate(() => {
      const panel = document.getElementById('subagents-panel');
      const filter = document.querySelector('.subagents-panel__filter');
      const dialog = document.querySelector('[data-details-dialog]');
      const history = document.querySelector('[data-details-history]');
      const head = document.querySelector('.subagent-details__head');
      const body = document.querySelector('.subagent-details__body');
      const text = history?.textContent || '';
      const assistantText = history?.querySelector('.msg--assistant .assistant-text')?.textContent || '';
      return {
        pageOverflow: document.documentElement.scrollWidth > window.innerWidth,
        panelOverflow: !!panel && panel.scrollWidth > panel.clientWidth,
        filterOverflow: !!filter && filter.scrollWidth > filter.clientWidth,
        dialogOverflow: !!dialog && dialog.scrollWidth > window.innerWidth,
        headOverlap: !!head && !!body && head.getBoundingClientRect().bottom > body.getBoundingClientRect().top + 1,
        tail: text.includes('DETAILSTAIL') && text.includes('long-relative-read-target.txt'),
        thinkingMarker: text.includes('subagent-details-thinking-marker'),
        imageCount: history?.querySelectorAll('.tool-media__image').length ?? 0,
        liveWord: document.querySelector('[data-details-live]') !== null,
        assistantTail: assistantText.includes('DETAILSTAIL'),
        ellipsis: assistantText.includes('\u2026') || assistantText.includes('...'),
      };
    });

    if (mobile.pageOverflow) fail('mobile: page scrolls horizontally');
    if (mobile.panelOverflow) fail('mobile: subagents panel overflows horizontally');
    if (mobile.filterOverflow) fail('mobile: filter bar overflows horizontally');
    if (mobile.dialogOverflow) fail('mobile: details dialog overflows horizontally');
    if (mobile.headOverlap) fail('mobile: dialog header covers the transcript body');
    if (!mobile.assistantTail) fail('mobile: assistant prose lost DETAILSTAIL (selector missed the bounded row)');
    if (!mobile.tail || mobile.ellipsis || mobile.liveWord) fail('mobile: details text is cropped, ellipsized, or badged live');
    if (!mobile.thinkingMarker || mobile.imageCount < 1) {
      fail('mobile: structured history lost the thinking body or the image card');
    }
    await page.screenshot({ path: `${evidence}/details-mobile-390.png`, fullPage: true });
    if (consoleErrors.length > 0) {
      fail(`console/page errors during the lane: ${consoleErrors.join(' | ')}`);
    }
    console.log('d93: PASSED (subagents spawn/live-activity/filter/detail-modal[structured-history/thinking/markdown/tool-cards/truncation]/hub-send/output-view/cancel/mobile, zero console errors)');
  } catch (err) {
    try {
      await page.screenshot({ path: `${evidence}/details-desktop-1360.png`, fullPage: true });
      await page.setViewportSize({ width: 390, height: 844 });
      await page.screenshot({ path: `${evidence}/details-mobile-390.png`, fullPage: true });
    } catch (shot) {
      console.error(`d93: screenshot failed: ${shot.stack || shot}`);
    }
    throw err;
  } finally {
    await browser.close();
  }

}

main().catch((err) => {
  console.error(`d93: crashed: ${err.stack || err}`);
  process.exit(2);
});
