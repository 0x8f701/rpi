// Additive Web orchestration projection authority verification. The baseline
// D93 lane remains in subagents_test.mjs and runs first.
import { chromium } from 'playwright';
import { clickFeature } from './lib/feature-menu.mjs';

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

// Frontend-only fault injection on the REAL app socket: never fakes the backend.
async function installJobOutputSeam(page) {
  await page.addInitScript(() => {
    const Native = window.WebSocket;
    class S extends Native { constructor(...a) { super(...a); (window.__seamSockets ??= []).push(this); } }
    for (const k of ['CONNECTING', 'OPEN', 'CLOSING', 'CLOSED']) Object.defineProperty(S, k, { value: Native[k] });
    window.WebSocket = S;
  });
}
async function armJobOutputSeam(page) {
  await page.evaluate(() => {
    const socket = (window.__seamSockets || []).find((s) => s.readyState === 1 && typeof s.onmessage === 'function');
    if (!socket || socket.readyState !== 1 || typeof socket.onmessage !== 'function') throw new Error('no live app socket');
    const modes = new Map(), requestJobs = new Map(), state = { held: new Map(), released: [] };
    const deliver = socket.onmessage, send = socket.send.bind(socket);
    const emit = (data) => deliver.call(socket, new MessageEvent('message', { data }));
    socket.send = (data) => {
      try {
        const frame = JSON.parse(data);
        if (frame.type === 'job_output') { modes.get(frame.jobId)?.ids.push(frame.id); requestJobs.set(frame.id, frame.jobId); }
      } catch { /* not JSON */ }
      return send(data);
    };
    socket.onmessage = (event) => {
      let frame; try { frame = JSON.parse(event.data); } catch { /* pass */ }
      const mode = frame?.type === 'response' ? modes.get(requestJobs.get(frame.id)) : null;
      if (!mode?.ids.includes(frame.id)) { deliver.call(socket, event); return; }
      if (mode.kind === 'hold') { state.held.set(frame.id, { raw: event.data, jobId: requestJobs.get(frame.id) }); return; }
      emit(JSON.stringify(mode.kind === 'fail' ? { ...frame, success: false, error: mode.message, data: null }
        : { ...frame, data: { ...frame.data, assignment: { status: 'unavailable', reason: mode.reason } } }));
    };
    window.__jobOutputSeam = {
      hold: (id) => modes.set(id, { kind: 'hold', ids: [] }),
      fail: (id, message) => modes.set(id, { kind: 'fail', ids: [], message }),
      unavailable: (id, reason) => modes.set(id, { kind: 'unavailable', ids: [], reason }),
      clear: (id) => modes.delete(id),
      release: (id) => [...state.held].forEach(([rid, e]) => { if (e.jobId !== id) return; state.held.delete(rid); state.released.push(rid); emit(e.raw); }),
      sent: (id) => (modes.get(id)?.ids.length || 0) > 0,
      held: () => [...state.held.keys()],
      released: () => state.released.length,
      snap: () => {
        const d = document.querySelector('[data-details-dialog]'), b = document.querySelector('[data-details-description]');
        return { id: d?.getAttribute('data-job-id') || '', status: b?.getAttribute('data-assignment-status') || '',
          text: (b?.textContent || '').trim(), error: (document.querySelector('[data-details-assignment-error]')?.textContent || '').trim() };
      },
    };
  });
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
    await installJobOutputSeam(page);
    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true, 'WebSocket never connected');
    await armJobOutputSeam(page);
    await clickFeature(page, '#subagents-toggle-btn');
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

    const liveJobs = (await rpc(page, { type: 'job_list' })).jobs.filter((job) => first.backendIds.includes(job.id));
    const [jobA, jobB] = ['gpt', 'glm'].map((agent) => liveJobs.find((job) => job.agent === agent)?.id);
    if (!jobA || !jobB) fail(`cannot map live agents to IDs: ${JSON.stringify(liveJobs)}`);
    const seam = (name, ...args) => page.evaluate(({ name, args }) => window.__jobOutputSeam[name](...args), { name, args });

    // A held → B available → stale A release must not repaint B.
    await seam('hold', jobA);
    await page.click(`.subagent-job[data-job-id="${jobA}"] [data-details-trigger]`);
    await waitFor(page, (id) => window.__jobOutputSeam.sent(id) && window.__jobOutputSeam.held().length > 0 && window.__jobOutputSeam.snap().status === 'loading', 'A response was not withheld while loading', 15000, jobA);
    await page.click('[data-details-close]');
    await page.click(`.subagent-job[data-job-id="${jobB}"] [data-details-trigger]`);
    await waitFor(page, (id) => { const s = window.__jobOutputSeam.snap(); return s.id === id && s.status === 'available' && s.text.includes('glm inspect seed.txt'); }, 'B assignment never became available', 15000, jobB);
    const bBefore = await seam('snap');
    await seam('release', jobA);
    await waitFor(page, () => window.__jobOutputSeam.released() > 0 && window.__jobOutputSeam.held().length === 0, 'A release never happened', 5000);
    const bAfter = await seam('snap');
    if (bAfter.id !== jobB || bAfter.status !== bBefore.status || bAfter.text !== bBefore.text || bAfter.error !== '') fail(`stale A changed B: ${JSON.stringify({ bBefore, bAfter })}`);

    // Same B refresh: precise RPC failure keeps last-good, then success clears it.
    await seam('fail', jobB, 'injected refresh job_output failure');
    await page.click('[data-details-refresh]');
    await waitFor(page, (id) => { const s = window.__jobOutputSeam.snap(); return s.id === id && s.error.includes(`job_output failed for ${id}:`) && s.error.includes('injected refresh job_output failure'); }, 'RPC failure never surfaced with job id', 15000, jobB);
    const afterFailure = await seam('snap');
    if (afterFailure.status !== 'available' || afterFailure.text !== bBefore.text) fail(`refresh failure lost last-good: ${JSON.stringify({ bBefore, afterFailure })}`);
    await seam('clear', jobB);
    await page.click('[data-details-refresh]');
    await waitFor(page, ({ id, text }) => { const s = window.__jobOutputSeam.snap(); return s.id === id && s.status === 'available' && s.text === text && s.error === ''; }, 'successful refresh did not clear the error', 15000, { id: jobB, text: bBefore.text });

    // Typed unavailable is a successful payload, never an RPC failure.
    await seam('unavailable', jobB, 'request_unavailable');
    await page.click('[data-details-refresh]');
    await waitFor(page, (id) => { const s = window.__jobOutputSeam.snap(); return s.id === id && s.status === 'unavailable' && s.error === '' && s.text.includes('Full task unavailable (request_unavailable)'); }, 'typed unavailable was conflated with an RPC failure', 15000, jobB);
    await seam('clear', jobB);
    await page.click('[data-details-close]');

    // First-ever failure has no last-good: error state, still with the job id.
    await seam('fail', jobA, 'injected first job_output failure');
    await page.click(`.subagent-job[data-job-id="${jobA}"] [data-details-trigger]`);
    await waitFor(page, (id) => { const s = window.__jobOutputSeam.snap(); return s.id === id && s.error.includes(`job_output failed for ${id}:`) && s.error.includes('injected first job_output failure') && s.text.includes('Full task not loaded. Refresh to retry.'); }, 'first failure never rendered an error state', 15000, jobA);
    if ((await seam('snap')).status !== 'error') fail('first failure was not typed as error');
    await seam('clear', jobA);
    await page.click('[data-details-close]');

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
