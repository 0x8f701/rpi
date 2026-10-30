// Additive Web orchestration projection authority verification. The baseline
// D93 lane remains in subagents_test.mjs and runs first.
import { chromium } from 'playwright';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const evidence = process.env.RPI_EVIDENCE || '.';
const chromePath = process.env.RPI_CHROME || '';

function fail(message) {
  console.error(`web-subagents-authority: FAIL: ${message}`);
  process.exit(2);
}

async function waitFor(page, fn, label, timeoutMs = 30000, arg) {
  try {
    await page.waitForFunction(fn, arg, { timeout: timeoutMs });
  } catch {
    fail(`${label} (timeout ${timeoutMs}ms)`);
  }
}

async function rpc(page, command) {
  const response = await page.evaluate(async ({ command, token }) => {
    const headers = { 'content-type': 'application/json' };
    if (token) headers.authorization = `Bearer ${token}`;
    const result = await fetch('/rpc', {
      method: 'POST',
      headers,
      body: JSON.stringify(command),
    });
    return { status: result.status, text: await result.text() };
  }, { command, token });
  let frame;
  try {
    frame = JSON.parse(response.text);
  } catch {
    fail(`RPC ${command.type} returned non-JSON status=${response.status}`);
  }
  if (response.status !== 200 || !frame.success) {
    fail(`RPC ${command.type} failed status=${response.status}: ${frame.error || response.text}`);
  }
  return frame.data || {};
}

function sorted(values) {
  return [...values].sort();
}

async function projection(page) {
  const backend = await rpc(page, { type: 'job_list' });
  const liveJobs = (backend.jobs || []).filter((job) => ['queued', 'running'].includes(job.status));
  const dom = await page.evaluate(() => ({
    ids: [...document.querySelectorAll('#subagents-job-list .subagent-job[data-job-id]')]
      .map((card) => card.getAttribute('data-job-id')).filter(Boolean).sort(),
    agents: [...document.querySelectorAll('#subagents-job-list .subagent-job[data-job-id]')]
      .map((card) => (card.querySelector('.subagent-job__title')?.textContent || '').match(/\(([^()]+)\)\s*$/)?.[1] || '')
      .filter(Boolean).sort(),
  }));
  return {
    groupId: backend.groupId || '',
    backendIds: sorted(liveJobs.map((job) => job.id)),
    backendAgents: sorted(liveJobs.map((job) => job.agent)),
    domIds: dom.ids,
    domAgents: dom.agents,
  };
}

async function main() {
  if (!url) fail('RPI_URL is required');
  const browser = await chromium.launch(chromePath ? { executablePath: chromePath } : {});
  const consoleErrors = [];
  const pageErrors = [];
  try {
    const page = await browser.newPage();
    page.on('console', (message) => {
      if (message.type() === 'error') consoleErrors.push(message.text());
    });
    page.on('pageerror', (error) => pageErrors.push(error.message));
    if (token) {
      await page.addInitScript((value) => window.localStorage.setItem('rpi-web-token', value), token);
    }
    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true, 'WebSocket never connected');
    await page.click('#subagents-toggle-btn');
    await waitFor(page, () => document.querySelector('#subagents-panel #subagents-spawn-btn') !== null,
      'Subagents spawn form unavailable', 15000);

    const catalog = await page.locator('#subagents-agent-select option').allTextContents();
    for (const agent of ['gpt', 'glm']) {
      if (!catalog.some((label) => label.trim() === agent)) fail(`missing ${agent} in catalog: ${catalog.join(', ')}`);
      await page.selectOption('#subagents-agent-select', agent);
      await page.fill('#subagents-task-input', `web-e2e-subagent: ${agent} inspect seed.txt and remain live`);
      await page.click('#subagents-spawn-btn');
      await waitFor(page, (task) => [...document.querySelectorAll('#subagents-job-list .subagent-job')]
        .some((card) => (card.textContent || '').includes(task)), `${agent} card never appeared`, 15000, `${agent} inspect seed.txt`);
    }

    await waitFor(page, () => {
      const cards = [...document.querySelectorAll('#subagents-job-list .subagent-job[data-job-id]')];
      return cards.length === 2 && cards.every((card) => card.getAttribute('data-status') === 'running');
    }, 'exactly two jobs never reached running DOM state', 20000);
    await waitFor(page, async ({ token }) => {
      const headers = { 'content-type': 'application/json' };
      if (token) headers.authorization = `Bearer ${token}`;
      const response = await fetch('/rpc', { method: 'POST', headers, body: JSON.stringify({ type: 'job_list' }) });
      const frame = await response.json();
      const jobs = (frame.data?.jobs || []).filter((job) => job.status === 'running');
      return response.ok && frame.success && typeof frame.data?.groupId === 'string' && frame.data.groupId !== ''
        && jobs.length === 2;
    }, 'backend never reported two running jobs with groupId', 20000, { token });

    const first = await projection(page);
    const expectedAgents = ['glm', 'gpt'];
    if (!first.groupId) fail('authoritative job_list omitted groupId');
    if (first.backendIds.length !== 2 || first.domIds.length !== 2) fail(`expected two live IDs: ${JSON.stringify(first)}`);
    if (first.backendIds.join(',') !== first.domIds.join(',')) fail(`backend/DOM IDs differ: ${JSON.stringify(first)}`);
    if (first.backendAgents.join(',') !== expectedAgents.join(',') || first.domAgents.join(',') !== expectedAgents.join(',')) {
      fail(`exact GPT+GLM agents missing: ${JSON.stringify(first)}`);
    }

    await page.fill('#prompt-input', 'presentation hub wait typed');
    await page.press('#prompt-input', 'Enter');
    await waitFor(page, () => {
      const cards = [...document.querySelectorAll('.tool-card[data-tool-name="hub"][data-tool-call-id]')];
      return cards.some((card) => (card.getAttribute('data-tool-call-id') || '')
        .startsWith('call-presentation-hub-wait-typed-')
        && card.getAttribute('data-tool-status') === 'running');
    }, 'real typed Hub wait never reached running state while jobs were live', 10000);
    const liveDuringWait = await projection(page);
    if (liveDuringWait.backendIds.join(',') !== first.backendIds.join(',')
      || liveDuringWait.domIds.join(',') !== first.domIds.join(',')) {
      fail(`two-job projection changed during Hub wait: ${JSON.stringify({ first, liveDuringWait })}`);
    }
    const waitDelivery = await rpc(page, { type: 'hub_send', to: 'Main', body: 'authority-hub-wait-delivered' });
    if (!Array.isArray(waitDelivery.receipts) || waitDelivery.receipts.length !== 1
      || waitDelivery.receipts[0]?.error) {
      fail(`typed Hub wait delivery failed: ${JSON.stringify(waitDelivery)}`);
    }
    await waitFor(page, () => [...document.querySelectorAll('.tool-card[data-tool-name="hub"][data-tool-call-id]')]
      .some((card) => (card.getAttribute('data-tool-call-id') || '')
        .startsWith('call-presentation-hub-wait-typed-')
        && card.getAttribute('data-tool-status') === 'done'
        && (card.textContent || '').includes('authority-hub-wait-delivered')),
    'typed Hub wait never settled from the delivered message', 10000);

    const selectedFilter = await page.locator('[aria-label="Filter subagent jobs by status"] [aria-selected="true"]').getAttribute('data-filter');
    const alternateFilter = selectedFilter === 'running' ? 'completed' : 'running';
    await page.click(`#subagents-filter-${alternateFilter}`);
    await waitFor(page, (filter) => document.querySelector(`#subagents-filter-${filter}`)?.getAttribute('aria-selected') === 'true',
      'alternate job filter never activated', 30000, alternateFilter);
    await page.click(`#subagents-filter-${selectedFilter}`);
    await waitFor(page, (filter) => document.querySelector(`#subagents-filter-${filter}`)?.getAttribute('aria-selected') === 'true'
      && document.querySelectorAll('#subagents-job-list .subagent-job[data-job-id]').length === 2,
    'two running jobs did not return after filter event cycle', 30000, selectedFilter);
    const afterEvents = await projection(page);
    if (afterEvents.groupId !== first.groupId || afterEvents.backendIds.join(',') !== first.backendIds.join(',')
      || afterEvents.domIds.join(',') !== first.domIds.join(',')) {
      fail(`projection drifted after observable filter/event cycle: ${JSON.stringify({ first, afterEvents })}`);
    }

    const lifecycle = await page.evaluate(() => {
      const cards = [...document.querySelectorAll('.tool-card[data-tool-call-id]')];
      const counts = {};
      for (const card of cards) {
        const id = card.getAttribute('data-tool-call-id') || '';
        if (id) counts[id] = (counts[id] || 0) + 1;
      }
      return {
        count: cards.length,
        nonEmptyIds: cards.map((card) => card.getAttribute('data-tool-call-id') || '').filter(Boolean),
        duplicates: Object.entries(counts).filter(([, count]) => count > 1),
        shutdown: cards.filter((card) => (card.textContent || '').includes('orchestration shut down'))
          .map((card) => ({ id: card.getAttribute('data-tool-call-id') || '', text: card.textContent || '' })),
      };
    });
    if (lifecycle.duplicates.length !== 0) fail(`duplicate toolCallId cards: ${JSON.stringify(lifecycle.duplicates)}`);
    if (lifecycle.count === 0 || lifecycle.nonEmptyIds.length === 0) {
      fail(`tool lifecycle assertion was vacuous: ${JSON.stringify(lifecycle)}`);
    }
    if (lifecycle.shutdown.length !== 0) fail(`shutdown cards while jobs live: ${JSON.stringify(lifecycle.shutdown)}`);
    if (consoleErrors.length !== 0) fail(`console errors: ${JSON.stringify(consoleErrors)}`);
    if (pageErrors.length !== 0) fail(`page errors: ${JSON.stringify(pageErrors)}`);

    const payload = {
      groupId: first.groupId,
      backendIds: first.backendIds,
      domIds: first.domIds,
      agents: first.backendAgents,
      duplicateToolCallIds: lifecycle.duplicates,
      shutdownCardsWhileLive: lifecycle.shutdown,
      consoleErrors,
      toolCardCount: lifecycle.count,
      nonEmptyToolCallIds: lifecycle.nonEmptyIds,
      pageErrors,
    };
    await page.screenshot({ path: `${evidence}/authority-two-live-jobs.png`, fullPage: true });

    const cancellation = await rpc(page, { type: 'job_cancel', jobIds: first.backendIds });
    if (sorted(cancellation.cancelled || []).join(',') !== first.backendIds.join(',')) {
      fail(`cancel receipt mismatch: ${JSON.stringify(cancellation)}`);
    }
    await waitFor(page, () => document.querySelectorAll('#subagents-job-list .subagent-job[data-job-id]').length === 0,
      'cancelled authority jobs remained in Running panel', 20000);
    const settled = await rpc(page, { type: 'job_list' });
    const settledAuthorityIds = sorted((settled.jobs || [])
      .filter((job) => first.backendIds.includes(job.id))
      .map((job) => job.id));
    if (settledAuthorityIds.join(',') !== first.backendIds.join(',')) {
      fail(`settled authority ID set drifted: ${JSON.stringify(settled.jobs)}`);
    }
    await page.screenshot({ path: `${evidence}/authority-two-jobs-settled.png`, fullPage: true });
    if (consoleErrors.length !== 0) fail(`console errors after settlement: ${JSON.stringify(consoleErrors)}`);
    if (pageErrors.length !== 0) fail(`page errors after settlement: ${JSON.stringify(pageErrors)}`);
    console.log(`web-subagents-authority: PASSED ${JSON.stringify(payload)}`);
  } finally {
    await browser.close();
  }
}

main().catch((error) => {
  console.error(`web-subagents-authority: crashed: ${error.stack || error}`);
  process.exit(2);
});
