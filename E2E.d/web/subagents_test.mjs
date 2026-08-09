// Focused D93 subagents-panel verification against the real fixture
// (orchestration enabled, writer agent, steering mock with the
// "web-e2e-subagent" marker branch). Standalone evidence for the subagents
// acceptance: spawn (task_spawn), live status + activity, hub message
// (hub_send), output view (job_output), cancel (job_cancel).
import { chromium } from 'playwright';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const evidence = process.env.RPI_EVIDENCE || '.';
const chromePath = process.env.RPI_CHROME || '';

function fail(message) {
  console.error(`d93: FAIL: ${message}`);
  process.exit(2);
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
  try {
    const page = await browser.newPage();
    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.getElementById('conn-state') !== null, 'conn-state missing');
    if (token) await page.fill('#token-input', token);
    await page.click('#connect-btn');
    await waitFor(page, () => document.getElementById('conn-state').dataset.state === 'on', 'WS never connected');

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

    // Spawn a faux subagent via the panel (task_spawn; agent auto/writer).
    await page.selectOption('#subagents-agent-select', 'writer');
    await page.fill('#subagents-task-input', 'web-e2e-subagent: audit the release notes and report findings');
    await page.click('#subagents-spawn-btn');

    // Job card appears with the task summary (description).
    await waitFor(
      page,
      () => {
        const cards = [...document.querySelectorAll('.subagent-job')];
        return cards.some((c) => (c.textContent || '').includes('audit the release notes'));
      },
      'spawned subagent job never appeared in the panel'
    );
    const status = () =>
      page.evaluate(() => {
        const cards = [...document.querySelectorAll('.subagent-job')];
        const card = cards.find((c) => (c.textContent || '').includes('audit the release notes'));
        return card ? card.getAttribute('data-status') || '' : '';
      });
    const live = await status();
    if (!['queued', 'running'].includes(live)) {
      fail(`spawned subagent must be live (queued/running), got "${live}"`);
    }
    // Live activity/elapsed one-liner rendered.
    const progress = await page.evaluate(() => {
      const cards = [...document.querySelectorAll('.subagent-job')];
      const card = cards.find((c) => (c.textContent || '').includes('audit the release notes'));
      const line = card ? card.querySelector('[data-progress-line]') : null;
      return line ? line.textContent || '' : '';
    });
    if (!progress.trim()) fail('no live activity/elapsed line rendered');
    await page.screenshot({ path: `${evidence}/spawned.png`, fullPage: true });

    // Message the subagent via hub_send. React controlled inputs ignore
    // `input.value=`; use the native setter so onChange fires.
    await page.evaluate(() => {
      const cards = [...document.querySelectorAll('.subagent-job')];
      const card = cards.find((c) => (c.textContent || '').includes('audit the release notes'));
      const input = card.querySelector('.subagent-job__message-input');
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
      'hub send button never enabled'
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
      [...card.querySelectorAll('.subagent-job__action')]
        .find((b) => (b.textContent || '').includes('Output'))
        .click();
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

    // Cancel the job and assert the settled cancelled status.
    await page.evaluate(() => {
      const cards = [...document.querySelectorAll('.subagent-job')];
      const card = cards.find((c) => (c.textContent || '').includes('audit the release notes'));
      [...card.querySelectorAll('.subagent-job__action')]
        .find((b) => (b.textContent || '').includes('Cancel'))
        .click();
    });
    await waitFor(
      page,
      () => {
        const cards = [...document.querySelectorAll('.subagent-job')];
        const card = cards.find((c) => (c.textContent || '').includes('audit the release notes'));
        return !!card && card.getAttribute('data-status') === 'cancelled';
      },
      'subagent job never reached cancelled status'
    );
    await page.screenshot({ path: `${evidence}/cancelled.png`, fullPage: true });
    console.log('d93: PASSED (subagents spawn/live-activity/hub-send/output-view/cancel)');
  } finally {
    await browser.close();
  }
}

main().catch((err) => {
  console.error(`d93: crashed: ${err.stack || err}`);
  process.exit(2);
});
