// Focused D93 subagents-panel verification against the real fixture
// (orchestration enabled, writer agent, steering mock with the
// "web-e2e-subagent" marker branch). Standalone evidence for the subagents
// acceptance: spawn (task_spawn), live status + activity, running-job detail
// modal (dialog a11y, task/status/elapsed/activity + non-empty recent
// history, Refresh, Escape/Close), hub message (hub_send), output view
// (job_output), cancel (job_cancel).
import { chromium } from 'playwright';

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
    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true, 'WS never connected');

    // --- Subagents panel ---
    await page.click('#subagents-toggle-btn');
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
      'Keep every acceptance sentence in the task body so the description stays',
      'longer than two hundred characters and still ends on DETAILSTAIL',
    ].join(' ');
    if (detailsTask.length <= 200 || !detailsTask.endsWith('DETAILSTAIL')) {
      fail(`details task fixture is not >200 chars ending DETAILSTAIL (${detailsTask.length})`);
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
    // Running job: task tail, typed status, elapsed. No live-word badge and
    // no Latest activity until a hub message exists.
    const detailsContent = await page.evaluate(() => {
      const txt = (sel) => (document.querySelector(sel)?.textContent || '').trim();
      const status = document.querySelector('[data-details-status] [role="status"]');
      return {
        description: txt('[data-details-description]'),
        status: status?.textContent.trim() || '',
        statusRole: status?.getAttribute('role') || '',
        elapsed: txt('[data-details-elapsed]'),
        activity: document.querySelector('[data-details-activity]') !== null,
        liveWord: document.querySelector('[data-details-live]') !== null
          || /\blive\b/i.test(document.querySelector('[data-details-dialog]')?.textContent || ''),
      };
    });
    if (!detailsContent.description.endsWith('DETAILSTAIL')) {
      fail(`task description cropped before DETAILSTAIL: "${detailsContent.description}"`);
    }
    if (detailsContent.statusRole !== 'status' || !['queued', 'running'].includes(detailsContent.status.toLowerCase())) {
      fail(`detail status must be a typed live status, got "${detailsContent.status}"`);
    }
    if (!detailsContent.elapsed) fail('detail modal elapsed never rendered while running');
    if (detailsContent.activity) fail('Latest activity rendered without a hub message');
    if (detailsContent.liveWord) fail('detail modal still shows a live-word badge');
    const readPath = 'fixtures/subagent-details/workspace-relative/long-read-target-for-subagent-details-readability/long-relative-read-target.txt';
    await page.screenshot({ path: `${evidence}/details-desktop-1360.png`, fullPage: true });
    await waitFor(
      page,
      (want) => (document.querySelector('[data-details-history]')?.textContent || '').includes(want),
      'detail history never showed the full relative read path',
      20000,
      readPath
    );
    const historyProbe = await page.evaluate((path) => {
      const root = document.querySelector('[data-details-history]');
      const prose = [...(root?.querySelectorAll('[data-history-kind="text"]') || [])];
      const assistant = prose.find((row) => (row.textContent || '').startsWith('The first assistant paragraph'));
      const call = [...(root?.querySelectorAll('[data-history-kind="call"]') || [])]
        .find((row) => (row.textContent || '').includes(path));
      const summary = call?.querySelector('.subagent-details__history-text');
      const summaryStyle = summary ? getComputedStyle(summary) : null;
      const outcome = call?.querySelector('[data-history-status]');
      return {
        assistantTag: assistant?.querySelector('.subagent-details__history-tag')?.textContent || '',
        assistantText: assistant?.textContent || '',
        summary: summary?.textContent || '',
        summaryEllipsis: summaryStyle?.textOverflow === 'ellipsis' || summaryStyle?.webkitLineClamp === '2',
        summaryClipped: !!summary && summary.scrollHeight > summary.clientHeight + 1,
        outcome: outcome?.getAttribute('aria-label') || '',
        outcomeKind: outcome?.getAttribute('data-history-status') || '',
        toggle: call?.querySelector('[data-history-output-toggle]')?.getAttribute('aria-label') || '',
      };
    }, readPath);
    if (historyProbe.assistantTag) fail(`assistant prose still has a tag: "${historyProbe.assistantTag}"`);
    if (!historyProbe.assistantText.includes('DETAILSTAIL')) fail('assistant prose does not show DETAILSTAIL');
    if (!historyProbe.summary.includes(readPath)) fail('tool summary is not the full relative read path');
    if (historyProbe.summaryEllipsis || historyProbe.summaryClipped) fail('tool summary is ellipsized');
    if (historyProbe.outcome !== 'succeeded' || historyProbe.outcomeKind !== 'ok') {
      fail(`successful outcome is not an accessible succeeded glyph: ${historyProbe.outcome}/${historyProbe.outcomeKind}`);
    }
    if (historyProbe.toggle !== 'Show tool output') fail(`output toggle missing or already open: "${historyProbe.toggle}"`);
    await page.screenshot({ path: `${evidence}/details-desktop-1360-after.png`, fullPage: true });
    const readRow = page.locator('[data-history-kind="call"]').filter({ hasText: readPath }).first();
    await readRow.locator('[data-history-output-toggle]').click();
    await readRow.locator('[data-history-output]').waitFor({ state: 'visible' });
    const outputToggle = await page.evaluate((path) => {
      const row = [...document.querySelectorAll('[data-history-kind="call"]')]
        .find((node) => (node.textContent || '').includes(path));
      const button = row?.querySelector('[data-history-output-toggle]');
      const output = row?.querySelector('[data-history-output]');
      return {
        label: button?.getAttribute('aria-label') || '',
        tail: output?.textContent.includes('DETAILSOUTTAIL') || false,
      };
    }, readPath);
    if (outputToggle.label !== 'Hide tool output' || !outputToggle.tail) {
      fail(`output toggle did not reveal DETAILSOUTTAIL: ${JSON.stringify(outputToggle)}`);
    }
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
    // Reopen by clicking the running job card itself (the card onClick path)
    // and close via the Close button — proving both dismissal paths.
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

    // Mobile: no horizontal overflow — page, panel, and the filter bar all
    // fit a 390px phone viewport.
    await page.setViewportSize({ width: 390, height: 844 });
    await page.evaluate(() => {
      const card = [...document.querySelectorAll('.subagent-job')]
        .find((node) => (node.textContent || '').includes('web-e2e-subagent-details'));
      card?.querySelector('[data-details-trigger]')?.click();
    });
    await waitFor(
      page,
      () => (document.querySelector('[data-details-history]')?.textContent || '').includes('long-relative-read-target.txt'),
      'mobile detail history never showed the read path',
      10000
    );
    const mobile = await page.evaluate(() => {
      const panel = document.getElementById('subagents-panel');
      const filter = document.querySelector('.subagents-panel__filter');
      const dialog = document.querySelector('[data-details-dialog]');
      const history = document.querySelector('[data-details-history]');
      const text = history?.textContent || '';
      return {
        pageOverflow: document.documentElement.scrollWidth > window.innerWidth,
        panelOverflow: !!panel && panel.scrollWidth > panel.clientWidth,
        filterOverflow: !!filter && filter.scrollWidth > filter.clientWidth,
        dialogOverflow: !!dialog && dialog.scrollWidth > window.innerWidth,
        tail: text.includes('DETAILSTAIL') && text.includes('long-relative-read-target.txt'),
        liveWord: document.querySelector('[data-details-live]') !== null,
        ellipsis: text.includes('\u2026') || text.includes('...'),
      };
    });
    if (mobile.pageOverflow) fail('mobile: page scrolls horizontally');
    if (mobile.panelOverflow) fail('mobile: subagents panel overflows horizontally');
    if (mobile.filterOverflow) fail('mobile: filter bar overflows horizontally');
    if (mobile.dialogOverflow) fail('mobile: details dialog overflows horizontally');
    if (!mobile.tail || mobile.ellipsis || mobile.liveWord) fail('mobile: details text is cropped, ellipsized, or badged live');
    await page.screenshot({ path: `${evidence}/details-mobile-390.png`, fullPage: true });
    console.log('d93: PASSED (subagents spawn/live-activity/filter/detail-modal/hub-send/output-view/cancel/mobile)');
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
