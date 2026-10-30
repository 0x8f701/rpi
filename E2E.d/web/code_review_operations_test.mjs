import fs from 'node:fs';
import path from 'node:path';
import http from 'node:http';
import assert from 'node:assert/strict';

const env = process.env;
const files = { dirty: env.RPI_DIRTY_FILE || 'app.rs', critical: env.RPI_CRIT_FILE || 'crit.rs', overdesign: env.RPI_WARN_FILE || 'warn.rs', naming: env.RPI_INFO_FILE || 'info.rs', preview: env.RPI_PREVIEW_FILE || 'notes.md' };
const IDENTITY_KEYS = ['snapshotId', 'path', 'oldStart', 'oldCount', 'newStart', 'newCount', 'contentHash'];
const FINDINGS_SCHEMA_KEYS = ['category', 'side', 'startLine', 'endLine', 'summary', 'evidence'];
const triageSchema = { type: 'object', additionalProperties: false, required: ['core', 'skipped'], properties: { core: { type: 'array', items: { type: 'string' } }, skipped: { type: 'integer', minimum: 0 } } };
const providerMode = process.argv[2] === '--provider';
const TLDR_GOOD = [
  'Pinned diff changes crit.rs, warn.rs, info.rs, app.rs, and notes.md.',
  '',
  '## Details',
  '',
  '- `crit.rs` `crit_one` adds `todo!()` on the new side.',
  '- `warn.rs` inserts `WARN-CHANGED` between the two base lines.',
  '- `info.rs` replaces the second baseline line with `INFO-BETA`.',
  '- `app.rs` changes `beta` to `BETA`.',
  '- `notes.md` appends `PREVIEW-EXTRA line`.',
  '',
  '```mermaid',
  'flowchart LR',
  '  crit_one --> WARN_CHANGED',
  '  INFO_BETA --> notes',
  '```',
].join('\n');
const TLDR_LATE = [
  'STALE-LATE-SNAPSHOT must never paint over the pinned summary.',
  '',
  'This body arrived after Stop and is rejected.',
].join('\n');

function lastUserText(body) {
  const lastUser = [...body.messages].reverse().find(message => message.role === 'user');
  if (!lastUser) return '';
  if (typeof lastUser.content === 'string') return lastUser.content;
  assert.ok(Array.isArray(lastUser.content), 'message content must be text or typed text blocks');
  return lastUser.content.map(part => {
    assert.equal(part?.type, 'text', 'unexpected non-text model input block');
    assert.equal(typeof part.text, 'string', 'typed text block has no text');
    return part.text;
  }).join('\n');
}

function promptIdentities(text) {
  return text.split('\n').filter(line => line.startsWith('Hunk identity: ')).map(line => {
    const fields = {};
    for (const field of line.slice('Hunk identity: '.length).split(/\s+/)) {
      const cut = field.indexOf('=');
      assert.ok(cut > 0, `identity field lacks '=': ${field}`);
      fields[field.slice(0, cut)] = field.slice(cut + 1);
    }
    assert.deepEqual(Object.keys(fields), IDENTITY_KEYS, `identity line keys: ${line}`);
    for (const key of ['oldStart', 'oldCount', 'newStart', 'newCount']) {
      assert.match(fields[key], /^\d+$/, `${key} is not an integer`);
      fields[key] = Number(fields[key]);
    }
    assert.match(fields.contentHash, /^[0-9a-f]{64}$/, 'contentHash is not 64 lowercase hex');
    assert.ok(fields.snapshotId.length > 0 && fields.path.length > 0, 'identity missing snapshot or path');
    return fields;
  });
}

function numberedRows(text, identity) {
  const marker = `Hunk identity: snapshotId=${identity.snapshotId} path=${identity.path} oldStart=${identity.oldStart} oldCount=${identity.oldCount} newStart=${identity.newStart} newCount=${identity.newCount} contentHash=${identity.contentHash}`;
  const start = text.indexOf(marker);
  assert.ok(start >= 0, `identity block missing for ${identity.path}`);
  const body = text.slice(start + marker.length).split('\n').slice(1);
  const rows = [];
  for (const line of body) {
    if (line.startsWith('Hunk identity: ') || line.startsWith('File: ') && rows.length > 0) break;
    const match = line.match(/^\s*(\d+|-)\s+(\d+|-)\s+([ +\-@])(.*)$/);
    if (!match) continue;
    rows.push({ oldNo: match[1] === '-' ? null : Number(match[1]), newNo: match[2] === '-' ? null : Number(match[2]), marker: match[3], text: match[4] });
  }
  return rows;
}

function finding(category, side, line, summary, evidence) {
  return { category, side, startLine: line, endLine: line, summary, evidence };
}

function findingsFor(identity, rows, mode) {
  const added = rows.filter(row => row.marker === '+').map(row => row.newNo);
  const deleted = rows.filter(row => row.marker === '-').map(row => row.oldNo);
  if (identity.path === files.critical) {
    const line = added.find(number => rows.some(row => row.newNo === number && row.text.includes('todo!()')));
    assert.equal(typeof line, 'number', 'crit.rs todo!() row missing');
    const second = added.find(number => rows.some(row => row.newNo === number && row.text.includes('fn data()')));
    assert.equal(typeof second, 'number', 'crit.rs fn data() row missing');
    assert.notEqual(second, line, 'crit.rs findings share one new row');
    return [
      finding('critical', 'new', line, 'Critical: todo!() has no failure path', 'todo!()'),
      finding('naming', 'new', second, 'Naming: data() renames crit_two', 'fn data() {}'),
    ];
  }
  if (identity.path === files.overdesign) {
    const line = added.find(number => rows.some(row => row.newNo === number && row.text.includes('WARN-CHANGED')));
    assert.equal(typeof line, 'number', 'warn.rs WARN-CHANGED row missing');
    return [finding('overdesign', 'new', line, 'Overdesign: WARN-CHANGED wraps the base', 'WARN-CHANGED')];
  }
  if (identity.path === files.naming && mode === 'highlight') {
    const line = deleted[0];
    assert.equal(typeof line, 'number', 'info.rs deletion row missing');
    return [finding('naming', 'old', line, 'Naming: deleted baseline still names the old side', rows.find(row => row.oldNo === line && row.marker === '-').text.trim())];
  }
  return [];
}

function findingsDocument(text, mode) {
  const identities = promptIdentities(text);
  if (mode === 'auto') assert.equal(identities.length, 1, 'Auto findings prompt must scope exactly one hunk');
  if (mode === 'highlight') {
    assert.ok(identities.length >= Object.keys(files).length, 'Highlight scope dropped a fixture hunk');
    assert.deepEqual(identities.map(identity => identity.path).sort(), Object.values(files).sort(), 'Highlight scope paths');
  }
  return {
    hunks: identities.map(identity => ({
      identity,
      findings: findingsFor(identity, numberedRows(text, identity), mode),
    })),
  };
}

function assertFindingsSchema(schema) {
  assert.equal(schema.type, 'object');
  assert.equal(schema.additionalProperties, false);
  assert.deepEqual(schema.required, ['hunks']);
  const item = schema.properties.hunks.items;
  assert.deepEqual(item.required, ['identity', 'findings']);
  assert.deepEqual(item.properties.identity.required, IDENTITY_KEYS);
  assert.deepEqual(item.properties.findings.items.required, FINDINGS_SCHEMA_KEYS);
  assert.deepEqual(item.properties.findings.items.properties.category.enum, ['critical', 'overdesign', 'naming']);
  assert.deepEqual(item.properties.findings.items.properties.side.enum, ['old', 'new']);
}

function assertNoTools(body) {
  const tools = body.tools;
  assert.ok(tools == null || (Array.isArray(tools) && tools.length === 0), 'TLDR request carried tools');
  assert.ok(body.tool_choice == null || body.tool_choice === 'none', 'TLDR request enabled tool choice');
}

async function provider(portFile) {
  const state = { requests: [], errors: [], held: [], released: [], fail: new Set(), tldrLate: false };
  const holds = new Set();
  const pending = new Map();
  const arrivals = [];
  let sequence = 0;
  const server = http.createServer(async (req, res) => {
    try {
      const endpoint = new URL(req.url, 'http://localhost');
      const json = value => { res.writeHead(200, { 'content-type': 'application/json' }); res.end(JSON.stringify(value)); };
      if (endpoint.pathname === '/control/state') return json(state);
      if (endpoint.pathname === '/control/await') {
        const ready = () => state.requests.filter(entry => entry.kind === endpoint.searchParams.get('kind')).length >= Number(endpoint.searchParams.get('count'));
        if (!ready()) await new Promise(resolve => arrivals.push({ ready, resolve }));
        return json(state);
      }
      if (endpoint.pathname === '/control/hold') { holds.add(endpoint.searchParams.get('kind')); return json({ ok: true }); }
      if (endpoint.pathname === '/control/fail') {
        const kind = endpoint.searchParams.get('kind');
        if (kind) state.fail.add(kind); else state.fail.clear();
        return json({ ok: true });
      }
      if (endpoint.pathname === '/control/tldr-late') { state.tldrLate = true; return json({ ok: true }); }
      if (endpoint.pathname === '/control/release') {
        const kind = endpoint.searchParams.get('kind');
        holds.delete(kind);
        for (const [id, entry] of pending) if (entry.kind === kind) { pending.delete(id); entry.finish(); state.released.push(id); }
        if (kind === 'tldr' && state.tldrLate) state.tldrLate = false;
        return json({ ok: true });
      }
      assert.equal(req.method, 'POST', `unexpected provider endpoint ${req.method} ${req.url}`);
      assert.match(endpoint.pathname, /\/chat\/completions$/, 'unexpected completion path');
      let raw = '';
      for await (const chunk of req) { raw += chunk; assert.ok(raw.length < 4 * 1024 * 1024, 'provider request exceeds bound'); }
      const body = JSON.parse(raw);
      const schemaName = body.response_format?.json_schema?.name;
      const userText = lastUserText(body);
      let kind, answer, failing = false;
      const identities = userText.includes('Requested hunks:') ? promptIdentities(userText) : [];
      if (schemaName === 'review_triage') {
        kind = 'triage';
        assert.equal(body.response_format?.type, 'json_schema', 'Stage1 plain-text completion rejected');
        assert.equal(body.response_format.json_schema.strict, true);
        assert.deepEqual(body.response_format.json_schema.schema, triageSchema, 'Stage1 schema differs from strict contract');
        assert.ok(!userText.includes('TRIAGE:'), 'obsolete trailer triage protocol');
        const tokens = [...userText.matchAll(/Hunk: ([^\n]+)/g)].map(match => match[1]);
        assert.deepEqual([...tokens].sort(), Object.values(files).map(file => `${file}@${file === files.preview ? '4-4' : '1-1'}`).sort(), 'triage must scan all five exact hunks');
        const core = [files.critical, files.overdesign].map(file => `${file}@1-1`);
        answer = JSON.stringify({ core, skipped: tokens.length - core.length });
        state.core = core;
      } else if (identities.length === 1 && userText.includes('Hunk identity:')) {
        kind = 'auto';
        assert.equal(schemaName, undefined, 'auto hunk agent unexpectedly forced review_findings');
        assert.ok(body.tools == null || body.tools.length > 0, 'auto hunk agent dropped its tools');
        assert.match(userText, /Respond in the language tagged `zh(?:-CN)?`\./, 'configured 中文 missing at auto findings boundary');
        assert.ok(!userText.includes('REVIEW-VERDICT:'), 'retired verdict trailer requested');
        failing = state.fail.has('auto');
        const selected = (state.core || []).filter(token => identities[0].path === token.split('@')[0]);
        assert.equal(selected.length, 1, 'auto review must target exactly one selected core hunk');
        answer = failing ? JSON.stringify({ hunks: [{ identity: { ...identities[0], newStart: identities[0].newStart + 50 }, findings: [] }] }) : JSON.stringify(findingsDocument(userText, 'auto'));
      } else if (schemaName === 'review_findings' && identities.length > 1) {
        kind = 'highlight';
        assert.equal(body.response_format?.type, 'json_schema', 'findings plain-text completion rejected');
        assert.equal(body.response_format.json_schema.strict, true);
        assertFindingsSchema(body.response_format.json_schema.schema);
        assert.deepEqual(body.response_format.json_schema.schema.properties.hunks.items.properties.findings.items.properties.category.enum, ['critical', 'overdesign', 'naming'], 'findings category enum');
        assert.match(userText, /Respond in the language tagged `zh(?:-CN)?`\./, 'configured 中文 missing at highlight findings boundary');
        assert.ok(!userText.includes('Respond with a JSON array'), 'bare highlight array prompt must be gone');
        const mode = state.fail.has('highlight-range') ? 'range' : state.fail.has('highlight') ? 'invalid' : 'ok';
        if (mode === 'invalid') answer = 'deliberately invalid highlight reply';
        else if (mode === 'range') {
          const document = findingsDocument(userText, 'highlight');
          const target = document.hunks.find(hunk => hunk.identity.path === files.critical);
          target.findings[0].endLine = target.identity.newStart + target.identity.newCount + 40;
          answer = JSON.stringify(document);
        } else answer = JSON.stringify(findingsDocument(userText, 'highlight'));
        failing = mode !== 'ok';
      } else if (userText.startsWith('Write one Markdown document about only the supplied current diff.')) {
        kind = 'tldr';
        assert.equal(schemaName, undefined, 'TLDR request carried a json schema');
        assert.equal(body.response_format, undefined, 'TLDR request carried response_format');
        assertNoTools(body);
        assert.equal(body.messages.filter(message => message.role === 'user').length, 1, 'TLDR prompt included conversation history');
        assert.ok(userText.includes('comparisonLabel:'), 'TLDR prompt missing comparison label');
        assert.match(userText, /Respond in the language tagged `zh(?:-CN)?`\./, 'configured 中文 missing at TLDR boundary');
        for (const file of Object.values(files)) assert.ok(userText.includes(`File: ${file}`), `TLDR prompt omitted ${file}`);
        answer = state.tldrLate ? TLDR_LATE : TLDR_GOOD;
      } else if (userText.includes('User comment:\nOPS accepted comment')) {
        kind = 'comment';
        answer = Array.from({ length: 90 }, (_, index) => `Comment reply ${index + 1}: accepted follow proof.`).join('\n');
      } else throw new Error(`unexpected model prompt: schema=${schemaName ?? 'none'} lastUser=${userText.slice(0, 180)}`);
      const id = ++sequence;
      state.requests.push({ id, kind, model: body.model, scope: kind === 'auto' || kind === 'highlight' ? promptIdentities(userText).map(identity => identity.path) : undefined, target: kind === 'auto' ? promptIdentities(userText)[0]?.path : undefined });
      const finish = () => {
        const chunk = content => `data: ${JSON.stringify({ id: `ops-${id}`, object: 'chat.completion.chunk', model: body.model, choices: [{ index: 0, delta: { role: 'assistant', content }, finish_reason: null }] })}\n\n`;
        const terminal = `data: ${JSON.stringify({ id: `ops-${id}`, choices: [{ index: 0, delta: {}, finish_reason: 'stop' }] })}\n\ndata: [DONE]\n\n`;
        res.writeHead(200, { 'content-type': 'text/event-stream' });
        if (kind !== 'comment' || failing) {
          res.end(chunk(answer) + terminal);
          return;
        }
        const splitAt = answer.indexOf('Comment reply 41');
        assert.ok(splitAt > 0, 'streamed reply split marker missing');
        res.write(chunk(answer.slice(0, splitAt)));
        setTimeout(() => res.end(chunk(answer.slice(splitAt)) + terminal), 3000);
      };
      if (holds.has(kind)) { state.held.push(id); pending.set(id, { kind, finish }); } else finish();
      for (const arrival of arrivals) if (arrival.ready()) arrival.resolve();
    } catch (error) {
      console.error(`provider request error: ${error.message}`);
      state.errors.push(error.stack || String(error));
      res.writeHead(400, { 'content-type': 'application/json' });
      res.end(JSON.stringify({ error: { message: error.message } }));
    }
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  fs.writeFileSync(portFile, String(server.address().port));
}

function sameIdentity(left, right) {
  return IDENTITY_KEYS.every(key => left?.[key] === right?.[key]);
}

function resultFor(snapshot, file) {
  return (snapshot.reviewFindings || []).find(result => result.identity?.path === file && result.identity.snapshotId === snapshot.snapshotId);
}

async function journey() {
  const { chromium } = await import('playwright');
  for (const key of ['RPI_URL', 'RPI_EVIDENCE', 'RPI_WORKSPACE', 'RPI_ATTACH_FILE']) assert.ok(env[key], `${key} is required`);
  const attachFile = env.RPI_ATTACH_FILE;
  const attachPath = path.join(env.RPI_WORKSPACE, attachFile);
  const pngBytes = fs.readFileSync(attachPath);
  assert.ok(pngBytes.length >= 8 && pngBytes[0] === 0x89 && pngBytes[1] === 0x50 && pngBytes[2] === 0x4e && pngBytes[3] === 0x47, 'RPI_ATTACH_FILE is not a PNG');
  const evidence = env.RPI_EVIDENCE;
  const config = JSON.parse(fs.readFileSync(path.join(env.RPI_WORKSPACE, '../home/.rpi/models.json'), 'utf8'));
  const base = config.providers['user-steering'].baseUrl;
  const control = async (action, kind = '') => {
    const response = await fetch(`${base}/control/${action}?kind=${kind}`, { signal: AbortSignal.timeout(30000) });
    assert.equal(response.status, 200, `provider control ${action}/${kind}`);
    return response.json();
  };
  const report = { status: 'FAILED', screenshots: [], errors: [], provider: null, frames: null };
  const browser = await chromium.launch(env.RPI_CHROME ? { executablePath: env.RPI_CHROME } : {});
  const page = await browser.newPage({ viewport: { width: 1280, height: 900 } });
  let phase = 'bootstrap';
  const check = (condition, message) => assert.ok(condition, `${phase}: ${message}`);
  const wait = async (fn, label, arg) => {
    try { await page.waitForFunction(fn, arg, { timeout: 30000 }); }
    catch (error) { throw new Error(`${phase}: ${label}; ${error.message}`); }
  };
  const capture = () => page.evaluate(() => window.__ops);
  const snapshots = frames => frames.received.filter(frame => frame.success === true && frame.data?.autoReviewState).map(frame => frame.data);
  const stateWait = async (field, value, from = 0) => {
    try {
      await page.waitForFunction(({ field, value, from }) => window.__ops.received.slice(from).some(frame =>
        frame.success === true && frame.data?.[field] === value),
      { field, value, from }, { timeout: 30000 });
    } catch (error) {
      const latest = snapshots(await capture()).at(-1);
      const detail = latest?.autoReviewError ?? latest?.highlightError ?? latest?.tldrError ?? 'no error';
      throw new Error(`${phase}: ${field} entered ${latest?.[field] ?? 'no state'} before ${value}: ${typeof detail === 'string' ? detail : JSON.stringify(detail)}; ${error.message}`);
    }
  };
  const mark = async () => page.evaluate(() => { window.__ops.received = []; });
  const holdSnapshots = async () => page.evaluate(() => { window.__ops.holdSnapshots = true; window.__ops.heldSnapshots = []; });
  const releaseSnapshots = async () => page.evaluate(() => {
    const held = window.__ops.heldSnapshots || [];
    window.__ops.holdSnapshots = false; window.__ops.heldSnapshots = [];
    for (const raw of held) window.__opsSocket.dispatchEvent(new MessageEvent('message', { data: raw }));
  });
  const gapEscapeStop = async operation => {
    const stopsBefore = (await capture()).sent.filter(frame => frame.type === 'code_review_stop').length;
    await page.evaluate(() => { document.body.tabIndex = -1; document.body.focus(); });
    await page.keyboard.press('Escape');
    await wait(({ stopsBefore, operation }) => {
      const sent = window.__ops.sent.filter(frame => frame.type === 'code_review_stop');
      return sent.length > stopsBefore && sent.at(-1).operation === operation;
    }, 'ACK-gap Escape Stop frame', { stopsBefore, operation });
    const framesNow = await capture();
    assert.equal(framesNow.sent.filter(frame => frame.type === 'code_review_close').length, 0, `${operation} gap Escape closed the panel`);
    check(await page.locator('#code-review-panel').isVisible(), `${operation} gap Escape closed the panel`);
    return framesNow;
  };
  const barrier = async () => {
    const id = `ops-barrier-${Date.now()}`;
    await page.evaluate(id => { const owner = window.__ops.sent.findLast(frame => frame.type === 'code_review_snapshot'); window.__opsSocket.send(JSON.stringify({ type: 'code_review_snapshot', id, sessionId: owner.sessionId })); }, id);
    await wait(id => window.__ops.received.some(frame => frame.id === id && frame.success), 'authoritative post-release snapshot', id);
    return (await capture()).received.find(frame => frame.id === id).data;
  };
  const terminal = async () => {
    await wait(() => !document.querySelector('.code-review__head .code-review__action--stop, .code-review__head [aria-busy="true"], .code-review__streaming-dot, .code-review__thinking-shimmer'), 'terminal DOM still running');
    check(await page.locator('.code-review__head .code-review__action--stop, .code-review__head [aria-busy="true"], .code-review__streaming-dot, .code-review__thinking-shimmer').count() === 0, 'terminal DOM retained activity');
    check(await page.locator('.code-review__error, .code-review__thread-stale, .code-review__window-error, .code-review__preview-error').count() === 0, 'inline review error or stale-thread warning exists');
    check(!(await page.locator('#code-review-panel').innerText()).includes('Key changes'), 'obsolete Key changes card exists');
    check(await page.locator('.code-review__hunk.has-highlight, .code-review__hunk-highlight-summary').count() === 0, 'whole-hunk highlight chrome remains');
  };
  const operationButtonId = operation => operation === 'autoReview' ? '#code-review-auto-review' : operation === 'highlight' ? '#code-review-highlight' : '#code-review-tldr';
  const waitOperationStop = async operation => {
    const id = operationButtonId(operation);
    await wait(id => {
      const el = document.querySelector(id);
      if (!el) return false;
      return (el.className || '').includes('code-review__action--stop') || (el.getAttribute('aria-label') || '').toLowerCase().includes('stop') || (el.textContent || '').includes('Stop');
    }, 'operation Stop after 151ms gate', id);
  };
  const shot = async name => {
    const surfaceState = await page.evaluate(() => {
      const overflow = [...document.querySelectorAll('body, #code-review-panel, .code-review__head, .code-review__composer, #toasts, #toasts .toast, #toasts .toast__message')]
        .filter(element => element.scrollWidth > element.clientWidth + 1)
        .map(element => element.id || element.className || element.tagName);
      const protectedRegions = [
        document.querySelector('.code-review__head'),
        document.querySelector('.code-review__tab-bar'),
        document.querySelector('.code-review-tldr__head'),
        document.querySelector('.code-review__composer'),
      ].filter(Boolean).map(element => element.getBoundingClientRect());
      const toastOverlap = [...document.querySelectorAll('#toasts .toast')].some(toast => {
        const box = toast.getBoundingClientRect();
        return protectedRegions.some(region => box.left < region.right && box.right > region.left && box.top < region.bottom && box.bottom > region.top);
      });
      return { overflow, toastOverlap };
    });
    check(surfaceState.overflow.length === 0, `horizontal overflow: ${JSON.stringify(surfaceState.overflow)}`);
    check(!surfaceState.toastOverlap, 'toast overlaps Code Review header, tabs, summary controls, or composer');
    const filename = path.join(evidence, `${name}.png`);
    await page.screenshot({ path: filename, fullPage: true }); report.screenshots.push(filename);
  };
  const requireActionToast = async (message = 'operations action failure') => {
    await wait(needle => {
      const toast = [...document.querySelectorAll('#toasts .toast--error')].find(el => (el.textContent || '').includes(needle));
      const host = document.getElementById('toasts');
      if (!toast || !host) return false;
      const box = toast.getBoundingClientRect();
      const hostBox = host.getBoundingClientRect();
      const protectedRegions = [
        document.querySelector('.code-review__head'),
        document.querySelector('.code-review__tab-bar'),
        document.querySelector('.code-review-tldr__head'),
        document.querySelector('.code-review__composer'),
      ].filter(Boolean).map(element => element.getBoundingClientRect());
      const overlapsReviewChrome = protectedRegions.some(region => box.left < region.right && box.right > region.left && box.top < region.bottom && box.bottom > region.top);
      const vw = window.innerWidth;
      const vh = window.innerHeight;
      return box.left >= -1 && box.right <= vw + 1 && box.top >= -1 && box.bottom <= vh + 1 && hostBox.left >= -1 && hostBox.right <= vw + 1 && !overlapsReviewChrome;
    }, 'action toast missing, overflowed, or overlapped Code Review chrome', message);
  };
  const requireSummaryHitTargets = async () => {
    const controls = await page.locator('.code-review-tldr__actions button').evaluateAll(buttons => buttons.map(button => {
      const box = button.getBoundingClientRect();
      return {
        label: button.getAttribute('aria-label'),
        reachable: button.contains(document.elementFromPoint(box.left + box.width / 2, box.top + box.height / 2)),
      };
    }));
    check(controls.some(control => control.label === 'Close summary'), 'summary Close control missing');
    check(controls.every(control => control.reachable), `summary controls obscured: ${JSON.stringify(controls)}`);
  };
  const pairedShot = async (name, beforeEach) => {
    await header();
    if (beforeEach) await beforeEach();
    await shot(`${name}-desktop`);
    await page.setViewportSize({ width: 390, height: 844 });
    if (await page.locator('.code-review__tab-bar').count()) await page.getByRole('button', { name: 'Diff', exact: true }).click();
    await header();
    if (beforeEach) await beforeEach();
    await shot(`${name}-mobile`);
    await page.setViewportSize({ width: 1280, height: 900 });
  };
  const proveHover = async id => {
    await page.locator('.code-review__title').hover();
    const before = await page.locator(id).evaluate(el => {
      const style = getComputedStyle(el);
      return { backgroundColor: style.backgroundColor, color: style.color, borderTopColor: style.borderTopColor, borderRightColor: style.borderRightColor, borderBottomColor: style.borderBottomColor, borderLeftColor: style.borderLeftColor };
    });
    await page.locator(id).hover();
    await wait(({ id, before }) => {
      const el = document.querySelector(id);
      if (!el || !el.matches(':hover')) return false;
      const style = getComputedStyle(el);
      return style.backgroundColor !== before.backgroundColor || style.color !== before.color || style.borderTopColor !== before.borderTopColor || style.borderRightColor !== before.borderRightColor || style.borderBottomColor !== before.borderBottomColor || style.borderLeftColor !== before.borderLeftColor;
    }, 'hover computed style unchanged', { id, before });
    await page.locator('.code-review__title').hover();
  };
  const proveFocusRing = async id => {
    await page.locator(id).focus();
    await page.keyboard.press('Tab');
    await page.keyboard.press('Shift+Tab');
    await wait(id => {
      const el = document.querySelector(id);
      if (!el || el !== document.activeElement || !el.matches(':focus-visible')) return false;
      const style = getComputedStyle(el);
      return style.outlineStyle !== 'none' && Number.parseFloat(style.outlineWidth) > 0 && style.outlineColor !== 'transparent' && style.outlineColor !== 'rgba(0, 0, 0, 0)';
    }, 'keyboard focus ring absent', id);
    await page.evaluate(() => { if (document.activeElement instanceof HTMLElement) document.activeElement.blur(); });
  };
  const header = async () => {
    const controls = await page.locator('.code-review__head .code-review__window-controls button').evaluateAll(nodes => nodes.map(node => ({ label: node.getAttribute('aria-label'), box: node.getBoundingClientRect().toJSON() })));
    const labels = controls.map(control => control.label);
    check(labels.length === 3 && /file tree$/.test(labels[0] || '') && /focus mode/i.test(labels[1] || '') && labels[2] === 'Close code review panel', `${phase}: window controls are not tree, focus, and panel close: ${JSON.stringify(labels)}`);
    for (const control of controls) {
      const box = control.box;
      check(control.label && box.width >= 44 && box.height >= 44 && Math.abs(box.width - box.height) <= 1, 'window control lacks accessible label/square 44px geometry');
    }
    for (let index = 0; index < controls.length - 1; index++) check(Math.abs(controls[index].box.y - controls[index + 1].box.y) < 2 && controls[index + 1].box.x >= controls[index].box.x, 'window controls are not in header order');
    if (await page.locator('.code-review-tldr').count()) {
      const closes = await page.locator('#code-review-panel').getByRole('button', { name: /close/i }).evaluateAll(nodes => nodes.map(node => node.getAttribute('aria-label')));
      check(closes.includes('Close summary') && closes.includes('Close code review panel') && new Set(closes).size === closes.length, `summary close is not distinct from panel close: ${JSON.stringify(closes)}`);
    }
    const mobile = (page.viewportSize() || { width: 1280 }).width <= 768;
    if (mobile) {
      const actions = await page.locator('#code-review-auto-review, #code-review-highlight, #code-review-tldr').evaluateAll(nodes => nodes.map(node => node.getBoundingClientRect().toJSON()));
      check(actions.length === 3, 'Auto Review/Highlight/TLDR missing for mobile target size');
      for (const box of actions) check(box.height >= 44, `mobile action target too small: ${JSON.stringify(box)}`);
      const tabs = page.locator('.code-review__tab-bar .code-review__tab');
      if (await tabs.count()) {
        const tabBoxes = await tabs.evaluateAll(nodes => nodes.map(node => node.getBoundingClientRect().toJSON()));
        for (const box of tabBoxes) check(box.height >= 44, `mobile tab target too small: ${JSON.stringify(box)}`);
      }
    }
    await proveHover('#code-review-auto-review');
    await proveFocusRing('#code-review-tree-toggle');
  };
  const select = async file => {
    if (page.viewportSize().width < 721) await page.getByRole('button', { name: 'Files', exact: true }).click();
    await page.locator(`button.code-review__file[data-file-path="${file}"]`).click();
    await page.locator('button.code-review__hunk-header:not([disabled])').waitFor();
  };
  const openHunk = async file => {
    await select(file);
    const headerButton = page.locator('button.code-review__hunk-header');
    await headerButton.waitFor({ state: 'attached' });
    if ((await headerButton.getAttribute('aria-pressed')) !== 'true') await headerButton.click();
    await page.locator('.code-review__hunk.is-selected').waitFor({ state: 'attached' });
    const toggle = page.locator('.code-review__hunk.is-selected .code-review__hunk-toggle');
    if ((await toggle.getAttribute('aria-expanded')) === 'false') await toggle.click();
    await page.locator('.code-review__hunk.is-selected .code-review__hunk-lines:not(.is-collapsed)').waitFor({ state: 'attached' });
    if ((page.viewportSize()?.width ?? 1280) < 721 && await page.locator('.code-review__tab-bar').count()) {
      await page.getByRole('button', { name: 'Thread', exact: true }).click();
    }
    await page.locator('.code-review__thread').waitFor();
    check((await headerButton.getAttribute('aria-pressed')) === 'true', `${file} hunk was not selected`);
  };
  const assertLangCentered = async () => {
    const lang = page.getByLabel('Auto review reply language');
    const metrics = await lang.evaluate(el => {
      const style = getComputedStyle(el);
      const box = el.getBoundingClientRect();
      return { textAlign: style.textAlign, left: box.left, right: box.right, top: box.top, bottom: box.bottom, width: box.width, height: box.height };
    });
    const viewport = page.viewportSize() || { width: 1280, height: 900 };
    check(metrics.textAlign === 'center', `Chinese selector text not centered: ${JSON.stringify(metrics)}`);
    check(metrics.left >= 0 && metrics.right <= viewport.width + 1 && metrics.top >= 0 && metrics.bottom <= viewport.height + 1, `Chinese selector geometry escaped viewport: ${JSON.stringify(metrics)}`);
  };
  const issueButtons = () => page.getByRole('group', { name: 'Issue navigation' }).getByRole('button');
  const issuePosition = () => page.getByRole('group', { name: 'Issue navigation' }).locator('.code-review__hunk-nav-position');
  const assertRowMarks = async (file, finding) => {
    const hunk = page.locator('.code-review__hunk');
    const marked = await hunk.locator('.code-review__line.has-finding--danger, .code-review__line.has-finding--warn, .code-review__line.has-finding--info').evaluateAll(nodes => nodes.map(node => ({
      oldNo: node.querySelector('.code-review__line-no--old')?.textContent.trim() || '',
      newNo: node.querySelector('.code-review__line-no--new')?.textContent.trim() || '',
      selected: node.classList.contains('is-finding-selected'),
      text: node.querySelector('.code-review__line-text')?.textContent || '',
    })));
    const expectedNo = String(finding.startLine);
    const matching = marked.filter(row => (finding.side === 'old' ? row.oldNo : row.newNo) === expectedNo);
    check(matching.length === 1 && matching[0].selected, `${file} ${finding.summary} row mark missing or unselected: ${JSON.stringify(marked)}`);
    const foreign = marked.filter(row => (finding.side === 'old' ? row.oldNo : row.newNo) !== expectedNo && row.selected);
    check(foreign.length === 0, `${file} selected a row outside the finding: ${JSON.stringify(foreign)}`);
    const lineCount = await hunk.locator('.code-review__line--addition, .code-review__line--deletion, .code-review__line--context').count();
    check(marked.length < lineCount, `${file} tinted every hunk row`);
  };
  const assertFindingDetail = async finding => {
    const selected = page.locator('.code-review__finding.is-selected');
    await selected.waitFor();
    const text = await selected.innerText();
    check(text.includes(`${finding.category} · ${finding.side} L${finding.startLine}–${finding.endLine}`), `detail meta mismatch: ${text}`);
    check(text.includes(finding.summary) && text.includes(finding.evidence), `detail body mismatch: ${text}`);
    check(await page.locator('.code-review__finding.is-selected').count() === 1, 'more than one finding detail selected');
  };
  const assertNoWarningInfo = async snapshot => {
    const serialized = JSON.stringify(snapshot.reviewFindings || []);
    check(!/"(?:warning|info|clean)"/.test(serialized), `warning/info/clean leaked into findings: ${serialized}`);

  };
  try {
    page.on('pageerror', error => report.errors.push(`page: ${error.message}`));
    page.on('console', message => { if (message.type() === 'error') report.errors.push(`console: ${message.text()}`); });
    await page.addInitScript(token => {
      localStorage.setItem('rpi-web-token', token); localStorage.setItem(`rpi-web-token:${location.host}`, token);
      const Native = WebSocket, frames = { sent: [], received: [], allReceived: [], parseErrors: [], toasts: [], failNext: false, holdSnapshots: false, heldSnapshots: [] };
      class Traced extends Native {
        constructor(...args) {
          super(...args);
          window.__opsSocket = this;
          this.addEventListener('message', event => {
            try {
              const frame = JSON.parse(event.data);
              if (frames.holdSnapshots && frame.command === 'code_review_snapshot') {
                frames.heldSnapshots.push(event.data);
                event.stopImmediatePropagation();
                return;
              }
              frames.received.push(frame); frames.allReceived.push(frame);
            } catch (error) { frames.parseErrors.push(String(error)); }
          });
        }
        send(raw) {
          const frame = JSON.parse(raw); frames.sent.push(frame);
          if (frames.failNext && frame.type === 'code_review_highlight') {
            frames.failNext = false;
            queueMicrotask(() => this.dispatchEvent(new MessageEvent('message', { data: JSON.stringify({ type: 'response', id: frame.id, command: frame.type, success: false, error: 'operations action failure', sessionId: frame.sessionId }) })));
            return;
          }
          return super.send(raw);
        }
      }
      window.WebSocket = Traced; window.__ops = frames;
      new MutationObserver(records => { for (const record of records) for (const node of record.addedNodes) if (node instanceof Element) for (const toast of [node, ...node.querySelectorAll('.toast')]) if (toast.matches('.toast')) frames.toasts.push(toast.textContent); }).observe(document, { childList: true, subtree: true });
    }, env.RPI_TOKEN || '');
    await page.goto(env.RPI_URL, { waitUntil: 'domcontentloaded' });
    await page.locator('#send-btn:not([disabled])').waitFor();
    await page.fill('#prompt-input', '/code-review HEAD'); await page.press('#prompt-input', 'Enter');
    await page.locator('#code-review-panel').waitFor(); await select(files.critical);
    await page.locator('button.code-review__hunk-header').click();
    const language = page.getByLabel('Auto review reply language');
    check(await language.locator('option:checked').innerText() === '中文', '中文 not preselected');
    await language.click(); await language.selectOption({ label: '中文' });
    const model = page.getByLabel('Review reply model');
    await wait(() => {
      const el = document.querySelector('.code-review__model-select');
      const snap = window.__ops.received.findLast(frame => frame.success && frame.data?.reviewModelProvider !== undefined);
      return el && el.value === 'user-steering/mock' && snap && snap.data.reviewModelProvider === 'user-steering' && snap.data.reviewModelId === 'mock';
    }, 'default review model identity not selected');
    const defaultModel = await model.evaluate(el => ({ value: el.value, label: el.selectedOptions[0]?.textContent ?? '' }));
    check(defaultModel.value === 'user-steering/mock', `default model identity wrong: ${JSON.stringify(defaultModel)}`);
    check(defaultModel.label === 'Steering Mock', `default model display label wrong: ${JSON.stringify(defaultModel)}`);
    await assertLangCentered();
    await model.click();
    const modelValue = await model.locator('option').evaluateAll(options => options.find(option => option.value.includes('mock-2'))?.value);
    check(modelValue, 'second fixture model absent'); await model.selectOption(modelValue);
    await wait(() => window.__ops.received.some(frame => frame.command === 'code_review_set_model' && frame.success), 'model click ack');
    const switched = await model.evaluate(el => ({ value: el.value, label: el.selectedOptions[0]?.textContent ?? '' }));
    check(switched.value === 'user-steering/mock-2', `model switch identity wrong: ${JSON.stringify(switched)}`);
    check(switched.label === 'Steering Mock Two', `model switch display label wrong: ${JSON.stringify(switched)}`);
    await header();

    phase = 'two-stage'; await control('hold', 'triage'); await control('hold', 'auto'); await mark();
    await page.locator('#code-review-auto-review').click(); await stateWait('autoReviewState', 'triaging');
    await wait(() => document.querySelector('.code-review__head .code-review__auto-progress')?.textContent?.trim() === 'Screening…', 'Screening… progress absent');
    await control('await', 'triage&count=1'); await control('release', 'triage'); await stateWait('autoReviewState', 'running');
    let frames = await capture(), running = snapshots(frames).find(snap => snap.autoReviewState === 'running');
    assert.deepEqual(running.autoReviewCounts, { submitted: 2, completed: 0, failed: 0 }, 'Stage2 exact initial counts');
    await wait(() => document.querySelector('.code-review__head .code-review__auto-progress')?.textContent?.trim() === 'Review 0/2', 'Review 0/2 progress absent');
    const assertThreadFollowsTail = async surface => {
      await wait(() => {
        const thread = document.querySelector('.code-review__thread');
        if (!thread || thread.scrollHeight <= thread.clientHeight) return false;
        return Math.abs(thread.scrollHeight - thread.clientHeight - thread.scrollTop) <= 1;
      }, `${surface} thread did not follow overflowing tail`);
      const metrics = await page.locator('.code-review__thread').evaluate(node => ({ bottom: node.scrollHeight - node.clientHeight, scrollTop: node.scrollTop }));
      check(Math.abs(metrics.bottom - metrics.scrollTop) <= 1, `${surface} thread did not follow tail: ${JSON.stringify(metrics)}`);
    };
    await control('await', 'auto&count=2'); await control('release', 'auto');
    await stateWait('autoReviewState', 'done'); await terminal();
    frames = await capture(); const done = snapshots(frames).findLast(snap => snap.autoReviewState === 'done');
    assert.deepEqual(done.autoReviewCounts, { submitted: 2, completed: 2, failed: 0 }, 'Stage2 terminal counts');
    const autoCritical = resultFor(done, files.critical);
    const autoOverdesign = resultFor(done, files.overdesign);
    check(autoCritical && autoCritical.findings.length === 2, 'auto crit.rs did not publish two findings');
    check(autoOverdesign && autoOverdesign.findings.length === 1, 'auto warn.rs did not publish its finding');
    check(autoCritical.findings.every(item => item.side === 'new') && autoOverdesign.findings[0].category === 'overdesign', 'auto findings lost category or side');
    await assertNoWarningInfo(done);
    assert.deepEqual(done.threads.map(thread => thread.identity.path).sort(), [files.critical, files.overdesign].sort(), 'Stage2 reviewed non-selected hunks');
    await openHunk(files.critical);
    await page.locator('.code-review__thread').evaluate(node => { node.scrollTop = 0; node.dispatchEvent(new Event('scroll')); });
    const frozenThread = await page.locator('.code-review__thread').evaluate(node => ({ bottom: node.scrollHeight - node.clientHeight, scrollTop: node.scrollTop }));
    check(frozenThread.scrollTop <= 1, `manual thread scroll was stolen before follow proof: ${JSON.stringify(frozenThread)}`);
    phase = 'thread-follow';
    await page.locator('.code-review__thread').evaluate(node => { node.scrollTop = node.scrollHeight; node.dispatchEvent(new Event('scroll')); });
    await page.setViewportSize({ width: 1280, height: 860 });
    if (frozenThread.bottom > 1) await assertThreadFollowsTail('desktop resumed');
    await page.locator('.code-review__thread').evaluate(node => { node.scrollTop = 0; node.dispatchEvent(new Event('scroll')); });
    await openHunk(files.overdesign);
    await shot('01-thread-follow-desktop');
    await page.setViewportSize({ width: 390, height: 844 });
    await header();
    await page.getByRole('button', { name: 'Thread', exact: true }).click();
    await page.locator('.code-review__thread').evaluate(node => { node.scrollTop = 0; node.dispatchEvent(new Event('scroll')); });
    await openHunk(files.critical);
    await page.getByRole('button', { name: 'Thread', exact: true }).click();
    await shot('01-thread-follow-mobile');
    await page.setViewportSize({ width: 1280, height: 900 });
    await header();
    await pairedShot('01-triage');

    phase = 'batch-failure';
    const lastGood = done.reviewFindings;
    await control('hold', 'auto'); await control('fail', 'auto'); await control('hold', 'triage'); await mark();
    await page.locator('#code-review-auto-review').click();
    await stateWait('autoReviewState', 'triaging');
    await control('await', 'triage&count=2'); await control('release', 'triage'); await stateWait('autoReviewState', 'running');
    await page.getByRole('button', { name: 'Close code review panel', exact: true }).click();
    await page.locator('.code-review__confirm').waitFor();
    check((await page.locator('.code-review__confirm').innerText()).includes('in-flight review work'), 'close warning missing during triage');
    await page.getByRole('button', { name: 'Keep editing' }).click();
    await page.locator('.code-review__confirm').waitFor({ state: 'detached' });
    await page.locator('#code-review-panel button[title="Reload the diff snapshot"]').click();
    await page.locator('.code-review__confirm').waitFor();
    check((await page.locator('.code-review__confirm').innerText()).includes('Review work is still in flight'), 'refresh warning missing during triage');
    await page.getByRole('button', { name: 'Keep editing' }).click();
    await page.locator('.code-review__confirm').waitFor({ state: 'detached' });
    await control('release', 'auto'); await stateWait('autoReviewState', 'failed');
    await page.locator('#toasts .toast--error').filter({ hasText: 'Auto review' }).waitFor();
    const batchFailureToasts = (await capture()).toasts.length;
    const preserved = await barrier();
    await terminal();
    check((await capture()).toasts.length === batchFailureToasts, 'batch failure emitted duplicate toasts');
    check(await page.locator('.code-review__error, .code-review__thread-stale').count() === 0, 'batch failure leaked an inline error node');
    assert.equal(JSON.stringify(preserved.reviewFindings), JSON.stringify(lastGood), 'invalid auto range replaced last-good findings');
    await pairedShot('07-batch-failure');
    await control('fail', '');

    phase = 'highlight'; await mark(); await page.locator('#code-review-highlight').click(); await stateWait('highlightState', 'done');
    const painted = snapshots(await capture()).findLast(snap => snap.highlightState === 'done');
    check(painted.highlightHunks == null && painted.autoReview == null, 'aggregate highlight metadata remains');
    const highlighted = painted.reviewFindings.filter(result => result.findings.length > 0);
    assert.equal(highlighted.length, 3, 'exactly three non-empty highlight identities');
    const empty = painted.reviewFindings.filter(result => result.findings.length === 0);
    check(empty.some(result => result.identity.path === files.dirty) && empty.some(result => result.identity.path === files.preview), 'Highlight omitted a clean scoped hunk');
    for (const result of painted.reviewFindings) {
      assert.deepEqual(Object.keys(result.identity).sort(), [...IDENTITY_KEYS].sort(), `${result.identity.path} identity fields`);
      assert.equal(result.identity.snapshotId, painted.snapshotId, `${result.identity.path} snapshot id`);
      assert.match(result.identity.contentHash, /^[0-9a-f]{64}$/, `${result.identity.path} content hash`);
    }
    const crit = resultFor(painted, files.critical);
    const warn = resultFor(painted, files.overdesign);
    const info = resultFor(painted, files.naming);
    assert.equal(crit.findings.length, 2, 'same-hunk pair missing');
    assert.equal(warn.findings.length, 1, 'other-file finding missing');
    assert.equal(info.findings[0].side, 'old', 'old-side deletion was not published');
    await assertNoWarningInfo(painted);
    const issueOrder = [];
    await openHunk(files.critical);
    check(await issueButtons().nth(0).isEnabled(), 'previous issue disabled before a selection');
    check(await issueButtons().nth(1).isEnabled(), 'next issue disabled before a selection');
    await issueButtons().nth(0).click();
    check((await issuePosition().innerText()).trim() === '4 / 4', 'unselected previous did not land on the last finding');
    const lastDetail = await page.locator('.code-review__finding.is-selected').innerText();
    check(lastDetail.includes(warn.findings[0].summary), `unselected previous selected ${lastDetail}`);
    for (let step = 0; step < 3; step++) await issueButtons().nth(0).click();
    check((await issuePosition().innerText()).trim() === '1 / 4', 'previous did not walk back to the first finding');
    check(await issueButtons().nth(0).isDisabled(), 'previous stayed enabled on the first finding');
    for (let index = 0; index < 4; index++) {
      const position = (await issuePosition().innerText()).trim();
      check(position === `${index + 1} / 4`, `issue position ${position}`);
      const detail = await page.locator('.code-review__finding.is-selected').innerText();
      issueOrder.push(detail.split('\n')[1]);
      const currentFile = await page.locator('.code-review__diff-path').innerText();
      const finding = painted.reviewFindings.flatMap(result => result.findings.map(item => ({ ...item, path: result.identity.path }))).find(item => detail.includes(item.summary));
      check(finding && currentFile.includes(finding.path), `selection did not open ${finding?.path}`);
      await assertRowMarks(finding.path, finding);
      await assertFindingDetail(finding);
      if (index < 3) {
        check(await issueButtons().nth(1).isEnabled(), 'next disabled before the last finding');
        await issueButtons().nth(1).click();
      }
    }
    check(await issueButtons().nth(1).isDisabled(), 'next issue wrapped past the last finding');
    await issueButtons().nth(0).click();
    check((await issuePosition().innerText()).trim() === '3 / 4', 'previous did not step back one');
    await issueButtons().nth(0).click();
    await issueButtons().nth(0).click();
    check((await issuePosition().innerText()).trim() === '1 / 4', 'previous did not return to the first finding');
    check(await issueButtons().nth(0).isDisabled(), 'previous issue wrapped past the first finding');
    assert.deepEqual(issueOrder, [
      crit.findings[0].summary,
      crit.findings[1].summary,
      info.findings[0].summary,
      warn.findings[0].summary,
    ], `navigation left snapshot file order: ${JSON.stringify(issueOrder)}`);
    await pairedShot('02-findings');
    await terminal();

    phase = 'highlight-reject';
    const highlightGood = painted.reviewFindings;
    await control('fail', 'highlight-range'); await mark();
    await page.locator('#code-review-highlight').click();
    await stateWait('highlightState', 'failed');
    await page.locator('#toasts .toast--error').filter({ hasText: 'Highlight:' }).waitFor();
    const rejected = await barrier();
    assert.equal(rejected.highlightState, 'failed', 'invalid range did not fail the highlight');
    assert.equal(JSON.stringify(rejected.reviewFindings), JSON.stringify(highlightGood), 'out-of-range highlight replaced last-good findings');
    check((await capture()).toasts.filter(text => text.includes('Highlight:')).length === 1, 'invalid highlight emitted more than one error');
    await control('fail', '');
    await terminal();

    phase = 'tldr';
    await control('hold', 'tldr'); await mark();
    await page.locator('#code-review-tldr').click();
    await wait(command => window.__ops.received.some(frame => frame.success && frame.command === command && frame.data?.state === 'running' && typeof frame.data?.tldrGeneration === 'number'), 'TLDR start ack', 'code_review_tldr');
    await stateWait('tldrState', 'running');
    await page.locator('.code-review-tldr').waitFor();
    check(await page.locator('.code-review-tldr').getAttribute('data-tldr-state') === 'running', 'TLDR pane did not open running');
    await control('await', 'tldr&count=1');
    await pairedShot('09-tldr-running', async () => {
      await requireActionToast('Highlight:');
      await requireSummaryHitTargets();
      check(await page.getByRole('button', { name: 'Stop generating summary', exact: true }).count() === 1, 'summary Stop control missing');
    });
    const tldrFrom = (await capture()).received.length;
    await page.locator('.code-review-tldr').getByRole('button', { name: 'Stop generating summary', exact: true }).click();
    await requireActionToast('Highlight:');
    await stateWait('tldrState', 'idle', tldrFrom);
    await control('tldr-late'); await control('release', 'tldr');
    const stoppedTldr = await barrier();
    assert.equal(stoppedTldr.tldrState, 'idle', 'late TLDR revived a stopped empty summary');
    assert.equal(stoppedTldr.tldrMarkdown, '', 'late TLDR snapshot painted markdown');
    const stoppedBody = (await page.locator('.code-review-tldr__body').innerText()).trim();
    check(stoppedBody === '', `stopped TLDR painted markdown: ${stoppedBody}`);
    check(!stoppedBody.includes('STALE-LATE-SNAPSHOT'), 'late TLDR marker painted over the empty same-snapshot summary');
    await control('hold', 'tldr'); await mark();
    await page.locator('#code-review-tldr').click();
    await control('await', 'tldr&count=2'); await control('release', 'tldr'); await stateWait('tldrState', 'done');
    const summary = page.locator('.code-review-tldr');
    await summary.waitFor();
    check(await summary.getAttribute('data-tldr-state') === 'done', 'TLDR did not settle done');
    const viewport = summary.locator('.code-review-tldr__viewport');
    check(await viewport.getAttribute('data-expanded') === 'true', 'TLDR did not open expanded');
    check(await summary.getByRole('button', { name: 'Collapse details', exact: true }).getAttribute('aria-expanded') === 'true', 'open TLDR control is not Collapse details');
    const openedText = await summary.locator('.code-review-tldr__body').innerText();
    check(openedText.includes('PREVIEW-EXTRA') && openedText.includes('crit_one'), 'opened expanded markdown dropped the supplied detail');
    check((await viewport.evaluate(node => getComputedStyle(node).overflowY)) === 'auto', 'expanded TLDR viewport does not scroll');
    await summary.locator('.md-mermaid-host svg, .md-mermaid-host--rendered svg').waitFor();
    check(await summary.locator('.md-mermaid-error').count() === 0, 'Mermaid degraded to the source fallback');
    await summary.getByRole('button', { name: 'Collapse details', exact: true }).click();
    check(await viewport.getAttribute('data-expanded') === 'false', 'Collapse details did not clip');
    check((await viewport.evaluate(node => getComputedStyle(node).overflow)) === 'hidden', 'compact TLDR viewport is not clipped');
    const collapsedText = await summary.locator('.code-review-tldr__body').innerText();
    check(collapsedText.includes('PREVIEW-EXTRA') && collapsedText.includes('crit_one') && collapsedText === openedText, 'collapse changed the markdown body');
    await summary.getByRole('button', { name: 'Expand details', exact: true }).click();
    check(await viewport.getAttribute('data-expanded') === 'true', 'Expand details did not unclip');
    check((await viewport.evaluate(node => getComputedStyle(node).overflowY)) === 'auto', 'reexpand did not restore scroll');
    const reexpandedText = await summary.locator('.code-review-tldr__body').innerText();
    check(reexpandedText.includes('PREVIEW-EXTRA') && reexpandedText.includes('crit_one') && reexpandedText === openedText, 'reexpand changed the markdown body');
    await summary.locator('.md-mermaid-host svg, .md-mermaid-host--rendered svg').waitFor();
    check(await summary.locator('.md-mermaid-error').count() === 0, 'Mermaid missing after reexpand');
    await pairedShot('09-tldr-expanded', async () => {
      if (await page.locator('.code-review-tldr__viewport[data-expanded="false"]').count()) {
        await page.locator('.code-review-tldr').getByRole('button', { name: 'Expand details', exact: true }).click();
      }
    });
    const goodMarkdown = (await barrier()).tldrMarkdown;
    check(goodMarkdown.includes('Pinned diff changes'), 'done TLDR lost the overview');
    await control('hold', 'tldr'); await control('tldr-late'); await mark();
    await page.locator('#code-review-tldr').click();
    await stateWait('tldrState', 'running');
    await control('await', 'tldr&count=3');
    const lateFrom = (await capture()).received.length;
    await page.locator('#code-review-tldr').click();
    await stateWait('tldrState', 'done', lateFrom);
    await control('release', 'tldr');
    const afterLate = await barrier();
    assert.equal(afterLate.tldrMarkdown, goodMarkdown, 'late TLDR replaced the last-good snapshot');
    assert.equal(afterLate.tldrState, 'done', 'late TLDR changed the settled state');
    check(!(await page.locator('.code-review-tldr__body').innerText()).includes('STALE-LATE-SNAPSHOT'), 'stale TLDR body painted');
    await page.locator('.code-review-tldr').getByRole('button', { name: 'Close summary', exact: true }).click();
    await page.locator('.code-review-tldr').waitFor({ state: 'detached' });
    await terminal();

    phase = 'targeted-stop';
    for (const [operation, kind, field, terminalValue, escape] of [['autoReview', 'triage', 'autoReviewState', 'failed', false], ['highlight', 'highlight', 'highlightState', 'idle', true]]) {
      await control('hold', kind); await mark();
      await page.locator(operationButtonId(operation)).click();
      await stateWait(field, operation === 'autoReview' ? 'triaging' : 'running');
      await control('await', `${kind}&count=${operation === 'autoReview' ? 3 : 2}`);
      await waitOperationStop(operation);
      if (operation === 'highlight') {
        await page.getByRole('button', { name: 'Close code review panel', exact: true }).click();
        await page.locator('.code-review__confirm').waitFor();
        check((await page.locator('.code-review__confirm').innerText()).includes('in-flight review work'), 'close warning missing during highlight');
        await page.getByRole('button', { name: 'Keep editing' }).click();
        await page.locator('.code-review__confirm').waitFor({ state: 'detached' });
      }
      await pairedShot(`03-running-${operation}`);
      const afterStopFrom = (await capture()).received.length;
      if (escape) {
        await page.evaluate(() => { document.body.tabIndex = -1; document.body.focus(); });
        await page.keyboard.press('Escape');
      } else {
        await page.locator(operationButtonId(operation)).click();
      }
      await stateWait(field, terminalValue, afterStopFrom);
      check(await page.locator('#code-review-panel').isVisible(), 'Esc closed panel before stopping');
      const stopped = snapshots(await capture()).at(-1);
      await control('release', kind);
      const after = await barrier();
      assert.equal(after[field], terminalValue, 'late response revived/reset stopped operation');
      const other = operation === 'autoReview' ? 'highlightState' : 'autoReviewState';
      assert.equal(after[other], stopped[other], 'targeted Stop changed the other operation');
      await terminal(); await pairedShot(`03-stop-${operation}`);
    }

    phase = 'comment-follow';
    await openHunk(files.dirty);
    await page.getByRole('textbox', { name: 'Hunk comment', exact: true }).fill('OPS accepted comment one');
    await page.getByRole('textbox', { name: 'Hunk comment', exact: true }).press('Enter');
    await wait(() => document.querySelector('.code-review__thread')?.textContent.includes('Comment reply 1'), 'accepted comment reply absent');
    await assertThreadFollowsTail('accepted comment follows');
    await page.locator('.code-review__thread').evaluate(node => { node.scrollTop = 0; node.dispatchEvent(new Event('scroll')); });
    await wait(() => {
      const thread = document.querySelector('.code-review__thread');
      return thread && thread.scrollHeight > thread.clientHeight && thread.scrollTop <= 1;
    }, 'suspended thread not at top');
    const frozenTop = await page.locator('.code-review__thread').evaluate(node => node.scrollTop);
    await page.getByRole('textbox', { name: 'Hunk comment', exact: true }).fill('OPS accepted comment two');
    await page.getByRole('textbox', { name: 'Hunk comment', exact: true }).press('Enter');
    await wait(() => document.querySelector('.code-review__thread')?.textContent.includes('OPS accepted comment two'), 'second same-hunk comment absent');
    await wait(() => {
      const thread = document.querySelector('.code-review__thread');
      return thread && thread.scrollTop <= 1;
    }, 'accepted same-hunk comment jumped an unpinned reading position');
    await page.locator('.code-review__thread').evaluate(node => { node.scrollTop = 0; node.dispatchEvent(new Event('scroll')); });
    await wait(top => (document.querySelector('.code-review__thread')?.scrollTop ?? 1) <= top + 1, 'thread did not suspend after scroll up', frozenTop);
    await page.locator('.code-review__thread').evaluate(node => { node.scrollTop = node.scrollHeight; node.dispatchEvent(new Event('scroll')); });
    await assertThreadFollowsTail('returned to bottom resumes');

    phase = 'ack-gap';
    for (const [operation, ackCommand, ackField] of [['autoReview', 'code_review_auto_review', 'batchId'], ['highlight', 'code_review_highlight', 'highlightGeneration'], ['tldr', 'code_review_tldr', 'tldrGeneration']]) {
      await holdSnapshots(); await mark();
      await control('hold', operation === 'autoReview' ? 'triage' : operation);
      const startsBefore = (await capture()).sent.filter(frame => frame.type === ackCommand).length;
      await page.locator(operationButtonId(operation)).click();
      await wait(ackCommand => window.__ops.received.some(frame => frame.success && frame.command === ackCommand && frame.data?.state !== undefined), 'start ACK during the gap', ackCommand);
      check((await page.locator(operationButtonId(operation)).getAttribute('class') || '').includes('code-review__action--stop'), `${operation} gap button is not Stop with snapshots held`);
      const ackFrames = (await capture()).received.filter(frame => frame.success && frame.command === ackCommand);
      assert.ok(ackFrames.some(frame => typeof frame.data[ackField] === 'number'), `${ackCommand} ACK carries no ${ackField} identity`);
      if (operation === 'highlight') {
        const stopsBefore = (await capture()).sent.filter(frame => frame.type === 'code_review_stop').length;
        await page.locator(operationButtonId(operation)).click();
        await wait(({ stopsBefore, operation }) => window.__ops.sent.filter(frame => frame.type === 'code_review_stop').length > stopsBefore && window.__ops.sent.filter(frame => frame.type === 'code_review_stop').at(-1).operation === operation, 'second-click Stop frame', { stopsBefore, operation });
      } else {
        await gapEscapeStop(operation);
      }
      const after = await capture();
      assert.equal(after.sent.filter(frame => frame.type === 'code_review_close').length, 0, `${operation} gap closed the review`);
      assert.equal(after.sent.filter(frame => frame.type === ackCommand).length, startsBefore + 1, `${operation} gap issued a second start`);
      check(await page.locator(operationButtonId(operation)).evaluate(el => el.classList.contains('code-review__action--stop')), `${operation} gap lost the Stop chrome before release`);
      await releaseSnapshots();
      const gapField = operation === 'autoReview' ? 'autoReviewState' : operation === 'highlight' ? 'highlightState' : 'tldrState';
      const gapValue = operation === 'autoReview' ? 'failed' : operation === 'highlight' ? 'idle' : 'done';
      await wait(({ field, value }) => window.__ops.received.some(frame => frame.success === true && frame.data?.[field] === value), `${gapField}=${gapValue}`, { field: gapField, value: gapValue });
      await control('release', operation === 'autoReview' ? 'triage' : operation);
      await terminal();
    }

    phase = 'action-toast'; await page.evaluate(() => { window.__ops.failNext = true; });
    const toastBefore = (await capture()).toasts.length;
    await page.locator('#code-review-highlight').click();
    await page.locator('#toasts .toast--error').filter({ hasText: 'operations action failure' }).waitFor();
    check((await capture()).toasts.length === toastBefore + 1, 'action failure emitted duplicate App toast');
    await terminal();
    await pairedShot('06-action-toast', async () => {
      await requireActionToast();
      await requireSummaryHitTargets();
    });
    await page.locator('.code-review-tldr').getByRole('button', { name: 'Close summary', exact: true }).click();
    await page.locator('.code-review-tldr').waitFor({ state: 'detached' });
    await requireActionToast();

    phase = 'reopen-cycle';
    for (let cycle = 1; cycle <= 2; cycle++) {
      const toastsBefore = (await capture()).toasts.length;
      const receivedBefore = (await capture()).received.length;
      await page.getByRole('button', { name: 'Close code review panel', exact: true }).click();
      await page.locator('#code-review-panel').waitFor({ state: 'detached' });
      await page.fill('#prompt-input', '/code-review HEAD'); await page.press('#prompt-input', 'Enter');
      await page.locator('#code-review-panel').waitFor();
      await openHunk(files.critical);
      await wait(() => {
        const el = document.querySelector('.code-review__model-select');
        return el && el.value === 'user-steering/mock';
      }, `reopen ${cycle} default model identity not selected`);
      const reopened = await page.getByLabel('Review reply model').evaluate(el => ({ value: el.value, label: el.selectedOptions[0]?.textContent ?? '' }));
      check(reopened.value === 'user-steering/mock' && reopened.label === 'Steering Mock', `reopen ${cycle} model identity wrong: ${JSON.stringify(reopened)}`);
      check(await page.locator('.code-review__hunk-lines:not(.is-collapsed)').count() > 0, `reopen ${cycle} hunk body not usable`);
      check((await capture()).toasts.length === toastsBefore, `reopen ${cycle} emitted an error toast`);
      const postReopen = (await capture()).received.slice(receivedBefore);
      check(!postReopen.some(frame => frame.command === 'code_review_snapshot' && frame.success === false), `reopen ${cycle} snapshot failed after panel live`);
      await header();
      await assertLangCentered();
      await shot(`08-reopen-${cycle}-desktop`);
      await page.setViewportSize({ width: 390, height: 844 });
      await header();
      await assertLangCentered();
      await shot(`08-reopen-${cycle}-mobile`);
      await page.setViewportSize({ width: 1280, height: 900 });
      await header();
    }
    for (const [name, viewport] of [['desktop', { width: 1280, height: 900 }], ['mobile', { width: 390, height: 844 }]]) {
      phase = `${name}-surface`; await page.setViewportSize(viewport); await header(); await select(files.preview);
      await page.getByRole('button', { name: 'Preview', exact: true }).click();
      await page.locator('.code-review__preview-markdown').filter({ hasText: 'PREVIEW-EXTRA line' }).waitFor();
      check(await page.getByRole('button', { name: 'Preview', exact: true }).getAttribute('aria-pressed') === 'true', 'Preview segment not active');
      await shot(`04-${name}-preview`); await page.getByRole('button', { name: 'Source', exact: true }).click();
      await openHunk(files.dirty);
      if (name === 'mobile') await page.getByRole('button', { name: 'Thread', exact: true }).click();
      const input = page.getByRole('textbox', { name: 'Hunk comment', exact: true }); const before = await input.boundingBox();
      await input.hover(); await input.click(); await input.fill(Array.from({ length: 12 }, (_, index) => `safe unsent line ${index}`).join('\n'));
      await wait(() => document.querySelector('.code-review__comment-input').clientHeight > 100, 'composer auto-grow');
      const after = await input.boundingBox(); check(after.height > before.height && after.x >= 0 && after.x + after.width <= viewport.width + 1 && after.y + after.height <= viewport.height, 'composer geometry escaped viewport/did not grow');
      check(await input.evaluate(node => node === document.activeElement && node.matches(':hover')), 'composer focus/hover missing');
      check((await page.locator('.code-review__action--send').innerText()).includes('➤'), 'composer send icon wrong');
      const panel = page.locator('#code-review-panel');
      const attach = panel.getByRole('button', { name: 'Attach files', exact: true });
      const mic = panel.getByRole('button', { name: 'Microphone', exact: true });
      check(await attach.isEnabled(), 'hunk Attach files is disabled');
      check(await mic.isEnabled() && await mic.getAttribute('aria-label') === 'Microphone' && await mic.getAttribute('title') === 'Hold to talk (voice to text)' && await mic.getAttribute('aria-pressed') === 'false', 'mic is not an enabled idle control');
      if (viewport.width <= 768) {
        const send = panel.locator('.code-review__action--send');
        const targets = await Promise.all([attach, mic, send].map(el => el.boundingBox()));
        check(targets[0] && targets[0].width >= 44 && targets[0].height >= 44 && Math.abs(targets[0].width - targets[0].height) <= 1, `mobile attach target too small: ${JSON.stringify(targets[0])}`);
        check(targets[1] && targets[1].width >= 44 && targets[1].height >= 44 && Math.abs(targets[1].width - targets[1].height) <= 1, `mobile mic target too small: ${JSON.stringify(targets[1])}`);
        check(targets[2] && targets[2].height >= 44, `mobile send target too small: ${JSON.stringify(targets[2])}`);
      }
      const placeholder = `${await attach.getAttribute('title') || ''} ${await mic.getAttribute('title') || ''} ${await page.locator('#code-review-panel .code-review__composer').innerText()}`;
      check(!/not supported|unsupported/i.test(placeholder), 'unsupported placeholder remains on attach/mic');
      const picker = panel.locator('input[type=file]');
      check(await picker.count() === 1 && (await picker.getAttribute('accept') || '').includes('image/png'), 'hunk hidden image input missing image/png accept');
      const [chooser] = await Promise.all([page.waitForEvent('filechooser'), attach.click()]);
      check(await chooser.element().evaluate(el => el.closest('#code-review-panel') !== null && el.getAttribute('type') === 'file'), 'filechooser was not the hunk hidden image input');
      await chooser.setFiles(attachPath);
      await wait(({ name }) => {
        const chip = document.querySelector('#code-review-panel .code-review__composer-attachments .composer-attachment');
        const thumb = chip && chip.querySelector('img.composer-attachment__thumb');
        const label = chip && (chip.querySelector('.composer-attachment__name')?.textContent || '').trim();
        return !!(chip && label === name && thumb && (thumb.getAttribute('src') || '').length > 0);
      }, 'hunk image chip name/thumbnail', { name: attachFile });
      await shot(`05-${name}-composer`);
      await panel.getByRole('button', { name: `Remove ${attachFile}`, exact: true }).click();
      await wait(() => !document.querySelector('#code-review-panel .code-review__composer-attachments .composer-attachment'), 'hunk image chip remained after remove');
      await input.fill(''); await terminal();
    }
    report.provider = await control('state'); frames = await capture();
    assert.deepEqual(report.provider.errors, [], 'strict provider rejected a runtime request');
    const kinds = report.provider.requests.map(entry => entry.kind);
    assert.equal(kinds.filter(kind => kind === 'triage').length, 4, 'final triage request count');
    assert.equal(kinds.filter(kind => kind === 'auto').length, 4, 'auto findings request count');
    assert.equal(kinds.filter(kind => kind === 'highlight').length, 4, 'highlight request count');
    assert.equal(kinds.filter(kind => kind === 'tldr').length, 4, 'tldr request count');
    assert.equal(kinds.filter(kind => kind === 'comment').length, 2, 'accepted comment reply count');
    assert.equal(report.provider.released.length, report.provider.held.length, 'unreleased held model requests');
    const autoTargets = report.provider.requests.filter(entry => entry.kind === 'auto').map(entry => entry.target);
    check(autoTargets.every(target => target === files.critical || target === files.overdesign), `auto scoped an unselected hunk: ${JSON.stringify(autoTargets)}`);
    check(report.provider.requests.filter(entry => entry.kind === 'highlight').every(entry => entry.scope.length === 5), 'highlight scope was not the full fixture');
    check(report.provider.requests.every(entry => entry.model === 'mock-2'), 'model picker did not reach provider boundary');
    assert.equal(frames.sent.filter(frame => frame.type === 'code_review_comment').length, 2, 'accepted same-hunk comment count');
    assert.deepEqual(frames.sent.filter(frame => frame.type === 'code_review_stop').map(frame => frame.operation), ['tldr', 'tldr', 'autoReview', 'highlight', 'autoReview', 'highlight', 'tldr'], 'exact targeted and gap stop frames');
    assert.equal(frames.sent.filter(frame => frame.type === 'code_review_auto_review').length, 4, 'auto click count');
    assert.equal(frames.sent.filter(frame => frame.type === 'code_review_highlight').length, 5, 'highlight click count');
    assert.equal(frames.sent.filter(frame => frame.type === 'code_review_tldr').length, 4, 'tldr click count');
    for (const [operation, field, expected, startCommand] of [['autoReview', 'autoReviewState', 'failed', 'code_review_auto_review'], ['highlight', 'highlightState', 'idle', 'code_review_highlight']]) {
      const ackIndex = frames.allReceived.findIndex(frame => frame.command === 'code_review_stop' && frame.success && frame.data?.operation === operation);
      check(ackIndex >= 0, `${operation} stop acknowledgement absent`);
      const nextStart = frames.allReceived.findIndex((frame, index) => index > ackIndex && frame.command === startCommand && frame.success);
      const bound = nextStart === -1 ? frames.allReceived.length : nextStart;
      const later = frames.allReceived.slice(ackIndex + 1, bound).filter(frame => frame.success && frame.data?.[field] !== undefined);
      check(later.length > 0 && later.every(frame => frame.data[field] === expected), `${operation} revived in captured post-stop frames`);
    }
    const lateTldrStop = frames.allReceived.findLastIndex(frame => frame.command === 'code_review_stop' && frame.success && frame.data?.operation === 'tldr');
    const afterTldrStop = frames.allReceived.slice(lateTldrStop + 1).filter(frame => frame.success && frame.data?.tldrMarkdown);
    check(afterTldrStop.every(frame => !String(frame.data.tldrMarkdown).includes('STALE-LATE-SNAPSHOT')), 'late TLDR markdown was accepted');
    assert.deepEqual(frames.parseErrors, [], 'unparseable socket frames'); assert.deepEqual(report.errors, [], 'browser errors');
    const lateToasts = frames.toasts.slice(toastBefore);
    check(lateToasts.length === 1 && lateToasts.every(text => text.includes('operations action failure')), `final capture added ${JSON.stringify(lateToasts)} instead of exactly one operations action failure toast`);
    await terminal();
    report.frames = { sent: frames.sent.length, received: frames.allReceived.length, stops: 7, autoReview: 4, highlight: 5, tldr: 4, comment: 2 };
    report.status = 'PASSED';
  } catch (error) {
    report.status = 'FAILED';
    report.errors.push(`${phase}: ${error.stack || error}`);
    try {
      const filename = path.join(evidence, `failure-${phase}.png`);
      await page.screenshot({ path: filename, fullPage: true });
      report.screenshots.push(filename);
    } catch { /* page gone */ }
    try { report.frames = await capture(); } catch { /* page gone */ }
    process.exitCode = 2;
  }
  finally {
    fs.writeFileSync(path.join(evidence, 'code-review-operations-report.json'), JSON.stringify(report, null, 2));
    console.log(JSON.stringify(report)); await browser.close();
  }
}

(providerMode ? provider(process.argv[3]) : journey()).catch(error => { console.error(error.stack || error); process.exitCode = 2; });
