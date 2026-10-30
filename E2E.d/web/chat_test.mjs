// /chat runner lifecycle E2E against two real isolated rpi listeners, driven
// through the CURRENT /chat UI exactly:
//   mount #chat-projects -> New project button/form (#chat-project-add-btn,
//   #chat-project-form) -> .chat-project-row selection -> empty-project LAN
//   lobby (#chat-empty-project + #chat-invite) or overlay Invite when the
//   project already has a runner (#chat-invite-runner-btn) -> the correct
//   token-required two-step (first add halts in token_required; the dialog
//   candidate's "Token required" button reveals #chat-runner-token; submit
//   becomes "Save token and reconnect") -> connected .chat-chip runner state
//   -> gear menu (#chat-project-gear) for mapped cwd / reconnect / delete.
// Preserved contracts: wrong-token rejection, corrected-token recovery,
// two distinct durable runners, isolated project session files per runner
// home, reload restore/reconnect, backend-shutdown reconnect, same-port
// restart recovery, delete cleanup (mapping archive + token key removal),
// and token/cwd redaction in rendered text and durable storage.
//
// STAGING-ONLY LANE: Playwright writes every artifact (screenshots, the
// assertions manifest with its registered artifact list) into ONE unique
// staging directory handed over by the shell (RPI_STAGING), closes every
// page/browser while the handlers are still effective, runs the final
// browser-error union gate, and exits 0 WITHOUT publishing anything. The
// run-directory rename, the exact manifest/artifact validation, and the ONE
// current-pointer commit belong to chat.sh, which performs them ONLY after
// the backend RPC, route, cleanup, and listener-close gates pass — a pointer
// can never name a run before the whole lane passed. On ANY failed exit
// (assertion, timeout, or browser-error gate, including the INJECT_FAIL
// self-test) this script removes ONLY its own staging dir; pre-existing
// published runs and the previous pointer are never touched, and the shell
// owns every failure-path removal afterwards.
// Self-test injection: INJECT_FAIL=1 injects a real uncaught page error and
// a real console.error right after the app mounts, so the final
// browser-error gate must fail the run: staging is removed, nothing is
// published, and the shell's post-mortem gate proves the prior pointer and
// prior published runs survived byte-identical (exit 3 escalation when this
// script's own staging cleanup failed).
import { chromium } from 'playwright';
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';

const url = process.env.RPI_URL;
const runnerAOrigin = process.env.RPI_RUNNER_A_ORIGIN;
const runnerBOrigin = process.env.RPI_RUNNER_B_ORIGIN;
const runnerAToken = process.env.RPI_RUNNER_A_TOKEN || '';
const runnerBToken = process.env.RPI_RUNNER_B_TOKEN || '';
const wrongToken = process.env.RPI_WRONG_TOKEN || '';
const laneNonce = process.env.RPI_LANE_NONCE || '';
const runnerACwd = process.env.RPI_RUNNER_A_CWD || '';
const runnerBCwd = process.env.RPI_RUNNER_B_CWD || '';
const runnerAProject = process.env.RPI_RUNNER_A_PROJECT || '';
const runnerBProject = process.env.RPI_RUNNER_B_PROJECT || '';
const runnerAHome = process.env.RPI_RUNNER_A_HOME || '';
const runnerBHome = process.env.RPI_RUNNER_B_HOME || '';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';
const work = process.env.RPI_WORK || '.';
const INJECT_FAIL = process.env.INJECT_FAIL === '1';
// Whole-run per-page browser error accumulation (final union gate consumes
// these after every page/browser has closed).
const pageErrors = [];
const consoleErrors = [];
// Actual-form-submission and runner-command recorders. The submit counter
// observes REAL submit events on the project form (capture, from the page);
// the WebSocket recorder counts every outgoing runner frame whose JSON
// payload is a new_session command, so "exactly one create" is proven on
// the wire rather than inferred from backend state.
const IDENTITY_UNAVAILABLE = 'Runner session identity is unavailable';
const CONNECTION_CLOSED_1006 = 'Connection closed (code 1006)';
const TOKEN_REQUIRED = 'Token required';
const wireRecorder = { newSessionCommands: 0, getMessagesBySession: {}, sockets: [], promptsByHost: {} };
function recordRunnerSocket(webSocket) {
  wireRecorder.sockets.push(webSocket.url());
  webSocket.on('framesent', (frame) => {
    const payload = typeof frame.payload === 'string' ? frame.payload : '';
    if (!payload) return;
    try {
      const parsed = JSON.parse(payload);
      if (parsed && parsed.type === 'new_session') wireRecorder.newSessionCommands += 1;
      if (parsed && parsed.type === 'prompt') {
        const host = new URL(webSocket.url()).host;
        wireRecorder.promptsByHost[host] = (wireRecorder.promptsByHost[host] || 0) + 1;
      }
      if (parsed && parsed.type === 'get_messages' && typeof parsed.sessionId === 'string') {
        const sessionId = parsed.sessionId;
        wireRecorder.getMessagesBySession[sessionId] = (wireRecorder.getMessagesBySession[sessionId] || 0) + 1;
      }
    } catch {
      // Non-JSON or fragmented control frame: never a runner command.
    }
  });
}

// Event-driven kill/respawn signaling with the shell: the kill request is
// written into a FIFO the shell blocks on (no polling, no deadline races on
// either side), and the respawned runner is announced on a second FIFO this
// lane blocks on. The shell creates both FIFOs before this driver starts
// and keeps them open, so writes here never block on open.
const killFifo = path.join(work, 'kill-runner-b.fifo');
const upFifo = path.join(work, 'runner-b-up.fifo');

function requestRunnerBKill() {
  fs.writeFileSync(killFifo, 'kill runner b\n');
}

function awaitRunnerBUp(timeoutMs = 90000) {
  return new Promise((resolve, reject) => {
    const stream = fs.createReadStream(upFifo);
    const timer = setTimeout(() => {
      stream.destroy();
      reject(new Error(`runner B up signal never arrived (timeout ${timeoutMs}ms)`));
    }, timeoutMs);
    stream.once('data', () => { clearTimeout(timer); stream.destroy(); resolve(); });
    stream.once('error', (error) => { clearTimeout(timer); reject(error); });
    stream.once('end', () => {
      clearTimeout(timer);
      reject(new Error('runner-b-up fifo closed without the up signal'));
    });
  });
}

const WRONG_TOKEN_AUTH_NOISE = 'HTTP Authentication failed; no valid credentials available';
let ignoreWrongTokenAuthNoise = false;
function isKnownConsoleNoise(text) {
  if (text.includes('favicon')) return true;
  if (!ignoreWrongTokenAuthNoise) return false;
  const host = runnerHost(runnerBOrigin);
  const ws = new URL(runnerBOrigin).protocol === 'https:' ? 'wss:' : 'ws:';
  return text.includes(WRONG_TOKEN_AUTH_NOISE) && text.includes(`${ws}//${host}/ws`);
}

// Run-scoped STAGING ONLY: every artifact this run creates (screenshots,
// the assertions manifest) is written into the ONE unique staging
// directory the shell handed over via RPI_STAGING, and every artifact is
// REGISTERED only after its write/screenshot succeeded. This script NEVER
// renames the staging set, NEVER writes a run directory, and NEVER touches
// the current pointer: chat.sh performs the exact manifest/artifact
// validation and the single run-dir rename + pointer commit AFTER the
// backend RPC, route, cleanup, and listener-close gates pass. A failed run
// removes ONLY its staging dir; published evidence and the prior pointer
// are unreachable from here.
const PASS_EVIDENCE_FILE = 'playwright-assertions.json';
const stagingDir = process.env.RPI_STAGING
  || path.join(evidence, `.staging-web-chat-${crypto.randomBytes(12).toString('hex')}-${process.pid}`);
// The shell normally hands over an ATOMICALLY claimed, collision-resistant
// staging dir. If this driver ever computes its own fallback name (secure
// random hex + PID), a pre-existing dir is REJECTED outright — never staged
// into, and never later deleted by this attempt.
if (!process.env.RPI_STAGING && fs.existsSync(stagingDir)) {
  // Prelaunch fatal: no browser lifecycle exists yet (see fatal() below).
  fatal(`fallback staging dir already exists (collision rejected): ${stagingDir}`);
}
const laneArtifacts = []; // staged artifact names, AFTER their write succeeded


async function laneShot(page, name, opts = {}) {
  fs.mkdirSync(stagingDir, { recursive: true });
  await page.screenshot({ path: path.join(stagingDir, name), ...opts });
  laneArtifacts.push(name);
  return path.join(stagingDir, name);
}

function cleanupStaging() {
  fs.rmSync(stagingDir, { recursive: true, force: true });
}

/** Bounded browser close: resolves within `timeoutMs` or REJECTS — a hung
 *  or failed close is NEVER silently swallowed on the success path (a
 *  rejection fails the lane), and the close error itself is preserved
 *  either way. There is no Browser.process() API to SIGKILL the child
 *  from here: the producer runs in a setsid process group the shell owns
 *  (chat_terminate_pw in chat.sh), which authoritatively terminates the
 *  whole browser tree on EVERY exit path. */
async function closeBrowser(browser, timeoutMs = 15000) {
  const closing = browser.close();
  // The loser of the race must never surface as an unhandled rejection.
  closing.catch(() => {});
  let timer;
  try {
    await Promise.race([
      closing,
      new Promise((_resolve, reject) => {
        timer = setTimeout(
          () => reject(new Error(`browser close exceeded ${timeoutMs}ms`)),
          timeoutMs,
        );
      }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}

/** Stage the final assertions manifest. The manifest carries the run nonce
 *  and the EXACT registered artifact list (screenshots written by this run)
 *  so the shell's commit gate can validate the staged set precisely before
 *  any run-dir rename: exact lane and nonce, unique contained relative
 *  artifact names, nothing missing or extra. */
function stageManifest(assertions) {
  fs.mkdirSync(stagingDir, { recursive: true });
  fs.writeFileSync(path.join(stagingDir, PASS_EVIDENCE_FILE), JSON.stringify({
    lane: 'web-chat',
    nonce: laneNonce,
    artifacts: laneArtifacts,
    assertions,
  }, null, 2));
  laneArtifacts.push(PASS_EVIDENCE_FILE);
}

/** Lane failure: THROWS — every in-lane failure is handled by main's single
 *  catch, which runs the BOUNDED browser close before the process exits.
 *  No browser-lifecycle failure may bypass that close with a direct
 *  process.exit. */
function fail(message) {
  throw new Error(message);
}

/** Prelaunch fatal: the browser does not exist yet, so there is no browser
 *  lifecycle to bound. Remove ONLY this attempt's staging dir — the run-dir
 *  rename and pointer commit belong to the shell, so nothing published can
 *  exist to roll back here, and the prior pointer and prior published runs
 *  are never reachable from this script — then exit. Never called after
 *  chromium.launch. */
function fatal(message) {
  console.error(`web-chat: FAIL: ${message}`);
  cleanupStaging();
  if (INJECT_FAIL && fs.existsSync(stagingDir)) {
    // Deterministic self-test escalation: exit 2 must mean staging is gone;
    // the shell's post-mortem gate owns the pointer/prior-run assertions.
    console.error(`web-chat: FAIL: staging dir survived failure cleanup: ${stagingDir}`);
    process.exit(3);
  }
  process.exit(2);
}

async function waitFor(page, fn, label, timeoutMs = 30000, arg) {
  try {
    await page.waitForFunction(fn, arg, { timeout: timeoutMs });
  } catch (error) {
    fail(`${label} (timeout ${timeoutMs}ms): ${error && error.message ? error.message : error}`);
  }
}

function nativeSessionFiles(root) {
  if (!fs.existsSync(root)) return [];
  return fs.readdirSync(root, { withFileTypes: true }).flatMap((entry) => {
    const child = path.join(root, entry.name);
    if (entry.isDirectory()) return nativeSessionFiles(child);
    return entry.isFile() && entry.name.endsWith('.jsonl') ? [child] : [];
  });
}

function sessionHeader(file) {
  const firstLine = fs.readFileSync(file, 'utf8').split('\n', 1)[0];
  return JSON.parse(firstLine);
}

function mappedSessionFiles(home, cwd) {
  return nativeSessionFiles(path.join(home, 'sessions'))
    .filter((file) => sessionHeader(file).cwd === cwd);
}

function runnerHost(origin) {
  return new URL(origin).host;
}

async function registryRunners(page) {
  return page.evaluate(() => {
    try {
      const doc = JSON.parse(window.localStorage.getItem('rpi-web-chat-runners:v1') || '{}');
      return Array.isArray(doc.runners) ? doc.runners : [];
    } catch {
      return [];
    }
  });
}

async function runnerIdByOrigin(page, origin) {
  const host = runnerHost(origin);
  const match = (await registryRunners(page)).find((runner) => runner.authority === host || runner.origin === origin);
  return match ? match.id : '';
}

async function visibleChatError(page) {
  return page.evaluate((needles) => {
    const main = document.getElementById('chat-project-main') || document.body;
    const text = main.textContent || '';
    for (const needle of needles) {
      if (text.includes(needle)) return needle;
    }
    const nodes = [...document.querySelectorAll('.chat-form-error, .chat-session__status--error, .chat-session__feedback')];
    const visible = nodes.filter((node) => node.getClientRects().length > 0)
      .map((node) => (node.textContent || '').trim())
      .filter(Boolean);
    return visible[0] || '';
  }, [IDENTITY_UNAVAILABLE, CONNECTION_CLOSED_1006]);
}

async function assertNoVisibleChatError(page, label) {
  const found = await visibleChatError(page);
  if (found) fail(`${label}: ${found}`);
}

async function waitChatSessionReady(page, label) {
  await waitFor(page, (needle) => {
    const body = document.body.textContent || '';
    if (body.includes(needle)) return false;
    const session = document.querySelector('.chat-session');
    if (!session) return false;
    const errors = [...session.querySelectorAll('.chat-form-error, .chat-session__status--error')]
      .filter((node) => node.getClientRects().length > 0);
    if (errors.length > 0) return false;
    const loading = [...session.querySelectorAll('.chat-session__status')]
      .some((node) => (node.textContent || '').includes('Loading transcript'));
    if (loading) return false;
    const ta = session.querySelector('.chat-session__composer textarea');
    return ta instanceof HTMLTextAreaElement && !ta.disabled;
  }, `${label}: 1:1 session did not become ready`, 30000, IDENTITY_UNAVAILABLE);
}

async function waitSessionIdle(page, label) {
  await waitFor(page, () => {
    const session = document.querySelector('.chat-session');
    if (!session) return false;
    const ta = session.querySelector('.chat-session__composer textarea');
    if (!(ta instanceof HTMLTextAreaElement) || ta.disabled) return false;
    const sending = [...session.querySelectorAll('.chat-button--primary')]
      .some((btn) => (btn.textContent || '').includes('Sending'));
    if (sending) return false;
    return ![...session.querySelectorAll('.chat-session-message--assistant')]
      .some((el) => el.getAttribute('data-status') === 'streaming');
  }, label, 45000);
}

async function waitSessionTurn(page, prompt, label) {
  await waitFor(page, (text) => {
    const session = document.querySelector('.chat-session');
    if (!session) return false;
    const items = [...session.querySelectorAll('.chat-session-message')];
    const promptIndex = items.findIndex((el) => (
      el.classList.contains('chat-session-message--user')
      && (el.textContent || '').includes(text)
    ));
    if (promptIndex < 0) return false;
    const sending = [...session.querySelectorAll('.chat-button--primary')]
      .some((btn) => (btn.textContent || '').includes('Sending'));
    if (sending) return false;
    const later = items.slice(promptIndex + 1)
      .filter((el) => el.classList.contains('chat-session-message--assistant'));
    const assistant = later[later.length - 1];
    return Boolean(assistant) && assistant.getAttribute('data-status') !== 'streaming';
  }, label, 45000, prompt);
}

// Per-runner connection state is the status dot on `.chat-chip[data-runner-id]`
// (ChatApp.tsx:2322-2323, statusTone:469-474). Connected is data-tone="ok";
// the chip label is the runner name only.
async function waitChip(page, runnerId, online, label, timeoutMs) {
  await waitFor(page, ({ id, wantOnline }) => {
    const chip = document.querySelector(`.chat-chip[data-runner-id="${id}"]`);
    const tone = chip?.querySelector('.chat-status-dot')?.getAttribute('data-tone');
    return chip !== null && (tone === 'ok') === wantOnline;
  }, label, timeoutMs, { id: runnerId, wantOnline: online });
}

async function createProject(page, label, cwd) {
  await page.click('#chat-project-add-btn');
  await waitFor(page, () => document.getElementById('chat-project-form') !== null, `${label}: New project form never opened`);
  await page.fill('#chat-project-label', label);
  await page.fill('#chat-project-cwd', cwd);
  await page.click('#chat-project-add');
  await waitFor(page, (name) => [...document.querySelectorAll('.chat-project-row')]
    .some((row) => window.__chatProjectRowName(row) === name), `${label} row was not stored`, 15000, label);
  await page.evaluate((name) => {
    const row = [...document.querySelectorAll('.chat-project-row')].find((candidate) => (
      window.__chatProjectRowName(candidate) === name
    ));
    if (!(row instanceof HTMLElement)) throw new Error(`project row is missing: ${name}`);
    row.click();
  }, label);
  try {
    await page.waitForFunction((name) => [...document.querySelectorAll('.chat-project-row')].some(
      (row) => window.__chatProjectRowName(row) === name
        && row.getAttribute('data-selected') === 'true',
    ), label, { timeout: 15000 });
  } catch {
    const dump = await page.evaluate(() => [...document.querySelectorAll('.chat-project-row')].map((row) => ({
      label: window.__chatProjectRowName(row),
      selected: row.getAttribute('data-selected'),
    })));
    fail(`${label} was not selected (timeout 15000ms) rows=${JSON.stringify(dump)}`);
  }
}

async function openInvite(page) {
  await page.click('#chat-invite-runner-btn');
  await waitFor(page, () => document.getElementById('chat-invite') !== null, 'Invite runner dialog never opened');
}

async function addRunnerAddress(page, label, origin) {
  const back = page.locator('#chat-invite button', { hasText: 'Back' });
  if (await back.count()) await back.click();
  await waitFor(page, () => {
    const input = document.getElementById('chat-runner-label');
    return input instanceof HTMLInputElement && !input.disabled;
  }, `${label}: invite address fields stayed disabled`);
  await page.fill('#chat-runner-label', label);
  await page.fill('#chat-runner-address', origin);
  await page.click('#chat-runner-add');
}

// The token field only exists inside the two-step: the first add (no token
// field is rendered) halts a token-gated runner in token_required, and the
// dialog candidate then offers the "Token required" button which swaps the
// form into the "Token required" state (#chat-runner-token + "Save token
// and reconnect"). saveTokenAndInvite awaits the first successful probe
// before mapping the project, and the dialog closes only on success.
async function completeTokenTwoStep(page, origin, token, label) {
  const card = page.locator('#chat-invite .chat-machine-card', { hasText: origin }).first();
  await card.locator('button', { hasText: 'Token required' }).click({ timeout: 30000 });
  await page.fill('#chat-runner-token', token);
  await page.click('#chat-runner-add');
  try {
    await page.waitForFunction(() => document.getElementById('chat-invite') === null, null, { timeout: 45000 });
  } catch {
    const dump = await page.evaluate(() => ({
      inviteClass: document.getElementById('chat-invite')?.className || '',
      submit: (document.getElementById('chat-runner-add')?.textContent || '').trim(),
      submitDisabled: document.getElementById('chat-runner-add')?.hasAttribute('disabled') === true,
      toasts: [...document.querySelectorAll('.chat-session-toast__text')].map((el) => (el.textContent || '').trim()),
      cards: [...document.querySelectorAll('#chat-invite .chat-machine-card')].map((el) => ({
        text: (el.textContent || '').replace(/\s+/g, ' ').trim().slice(0, 280),
        buttons: [...el.querySelectorAll('button')].map((btn) => (btn.textContent || '').trim()),
      })),
      chips: [...document.querySelectorAll('.chat-chip[data-runner-id]')].map((chip) => ({
        id: chip.getAttribute('data-runner-id'),
        tone: chip.querySelector('.chat-status-dot')?.getAttribute('data-tone'),
      })),
      mappings: (() => {
        try {
          const doc = JSON.parse(window.localStorage.getItem('rpi-web-chat-mapping:v1') || '{}');
          return Array.isArray(doc.mappings)
            ? doc.mappings.map((row) => ({ projectId: row.projectId, runnerId: row.runnerId, state: row.state }))
            : [];
        } catch {
          return [];
        }
      })(),
    }));
    fail(`${label}: invite dialog never closed after the token two-step dump=${JSON.stringify(dump)}`);
  }
}

async function clickProjectRow(page, label) {
  await page.evaluate((name) => {
    const row = [...document.querySelectorAll('.chat-project-row')].find((candidate) => (
      window.__chatProjectRowName(candidate) === name
    ));
    if (!(row instanceof HTMLElement)) throw new Error(`project row is missing: ${name}`);
    row.click();
  }, label);
  await waitFor(page, (name) => [...document.querySelectorAll('.chat-project-row')].some(
    (row) => window.__chatProjectRowName(row) === name
      && row.getAttribute('data-selected') === 'true',
  ), `${label} row could not be selected`, 15000, label);
}

async function main() {
  // Prelaunch fatals: no browser lifecycle exists yet — these exit directly
  // instead of throwing into the browser-lifecycle owner below.
  if (!laneNonce) fatal('lane nonce (RPI_LANE_NONCE) is required for the staged manifest');
  if (!url || !runnerAOrigin || !runnerBOrigin || !runnerAToken || !runnerBToken
    || !runnerACwd || !runnerBCwd || !runnerAProject || !runnerBProject || !runnerAHome || !runnerBHome) {
    fatal('runner URLs, tokens, cwd values, project directories, and profile roots are required');
  }
  const launchOptions = chromePath ? { executablePath: chromePath } : {};
  // A launch failure rejects BEFORE the try: no browser lifecycle exists to
  // bound, so the top-level handler owns it (prelaunch fatal).
  const browser = await chromium.launch(launchOptions);
  try {
    const page = await browser.newPage({ viewport: { width: 1360, height: 900 } });
    await page.addInitScript((token) => {
      window.localStorage.setItem('rpi-web-token', token);
      window.__chatProjectRowName = (row) => {
        const label = row.querySelector('.chat-project-row__label');
        if (!label) return '';
        return [...label.childNodes]
          .filter((node) => !(node instanceof Element) || !node.classList.contains('chat-channel-hash'))
          .map((node) => node.textContent || '')
          .join('')
          .trim();
      };
    }, runnerAToken);
    page.on('pageerror', (error) => pageErrors.push(error.message));
    page.on('console', (message) => {
      if (message.type() === 'error' && !isKnownConsoleNoise(message.text())) consoleErrors.push(message.text());
    });
    page.on('websocket', recordRunnerSocket);
    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.getElementById('chat-projects') !== null, '/chat app did not mount');
    // Observe REAL form submissions on the project form (capture phase, so
    // the counter sees the browser's own submit dispatch — not the handler).
    await page.evaluate(() => {
      window.__chatE2E = { projectFormSubmits: 0 };
      document.addEventListener('submit', (event) => {
        const form = event.target;
        if (!(form instanceof HTMLFormElement)) return;
        if (form.id === 'chat-project-form' || form.closest('#chat-project-form')) {
          window.__chatE2E.projectFormSubmits += 1;
        }
      }, true);
    });
    if (INJECT_FAIL) {
      // Deterministic self-test: a REAL uncaught page error and a REAL
      // console.error, observed through the handlers above; the final
      // browser-error union gate must fail the run. The shell owns the
      // pointer/prior-run post-mortem assertions — nothing is published
      // from this script under any outcome.
      await page.evaluate(() => { setTimeout(() => { throw new Error('self-test injected pageerror'); }, 0); });
      await page.evaluate(() => console.error('self-test injected console.error'));
      await page.waitForTimeout(250);
    }
    if (await page.locator('.chat-project-row').count() !== 0) fail('project rail was not empty on first load');
    await waitFor(page, () => document.getElementById('chat-workspace-bar') !== null, 'workspace bar did not mount');
    await waitFor(page, () => document.getElementById('chat-empty-workspace') !== null, 'workspace empty hero did not mount');
    const workspaceHero = await page.locator('#chat-empty-workspace').textContent() || '';
    if (!workspaceHero.includes('Projects are channels for people and rpi agents')) {
      fail('workspace empty hero title is missing');
    }
    if (!workspaceHero.includes('Conversations stay on the rpi server')) {
      fail('workspace empty hero did not explain LAN persistence');
    }
    if ((await page.locator('#chat-empty-new-project').count()) !== 1) {
      fail('workspace empty New project control is missing');
    }
    await laneShot(page, 'chat-empty-workspace-desktop.png', { fullPage: true });
    await page.setViewportSize({ width: 390, height: 844 });
    await waitFor(page, () => {
      const hero = document.getElementById('chat-empty-workspace');
      const rail = document.getElementById('chat-projects-rail');
      return hero !== null && hero.getClientRects().length > 0
        && (rail === null || rail.hidden === true || rail.getClientRects().length === 0);
    }, 'mobile empty workspace did not show the hero in one pane');
    await laneShot(page, 'chat-empty-workspace-mobile.png', { fullPage: true });
    await page.setViewportSize({ width: 1360, height: 900 });
    await waitFor(page, () => {
      const rail = document.getElementById('chat-projects-rail');
      const add = document.getElementById('chat-project-add-btn');
      return rail !== null && add !== null && rail.hidden === false && add.getClientRects().length > 0
        && document.getElementById('chat-empty-workspace') !== null;
    }, 'desktop rail did not return after the mobile empty screenshot');

    // ---- Negative: relative cwd is rejected locally by the EXACT form ----
    // One bounded alert, one attempt, no project/mapping/session mutation,
    // no retry, typed fields preserved, busy gate released.
    await page.click('#chat-empty-new-project');
    await waitFor(page, () => document.getElementById('chat-project-form') !== null,
      'negative: New project form never opened');
    await page.fill('#chat-project-label', 'Negative Relative');
    await page.fill('#chat-project-cwd', 'workspace/relative');
    await page.click('#chat-project-add');
    await waitFor(page, () => {
      const alerts = [...document.querySelectorAll('.chat-session-toast')];
      const message = alerts[0]?.querySelector('.chat-session-toast__text')?.textContent || '';
      return alerts.length === 1 && (alerts[0].getAttribute('role')) === 'alert'
        && message === 'Project directory must be absolute.';
    }, 'negative: relative cwd rejection alert never rendered', 15000);
    if ((await page.inputValue('#chat-project-label')) !== 'Negative Relative'
      || (await page.inputValue('#chat-project-cwd')) !== 'workspace/relative') {
      fail('negative: relative rejection lost the typed fields');
    }
    if (await page.locator('#chat-project-add').isDisabled()) {
      fail('negative: relative rejection kept the busy gate engaged');
    }
    for (let sample = 0; sample < 3; sample += 1) {
      await page.waitForTimeout(700);
      const alertCount = await page.locator('.chat-session-toast').count();
      if (alertCount !== 1) fail('negative: relative rejection retried or duplicated alerts');
      if ((await page.locator('.chat-project-row', { hasText: 'Negative Relative' }).count()) !== 0) {
        fail('negative: relative rejection stored a project row');
      }
    }
    const relativeSubmits = await page.evaluate(() => window.__chatE2E.projectFormSubmits);
    if (relativeSubmits !== 1) {
      fail(`negative: relative rejection recorded ${relativeSubmits} real form submissions (expected exactly one)`);
    }
    if (wireRecorder.newSessionCommands !== 0) {
      fail(`negative: relative rejection sent ${wireRecorder.newSessionCommands} runner new_session commands (expected zero)`);
    }
    const relativeStorage = await page.evaluate(() => window.localStorage.getItem('rpi-web-chat-mapping:v1') || '');
    if (relativeStorage.includes('Negative Relative') || relativeStorage.includes('workspace/relative')) {
      fail('negative: relative rejection leaked into durable mapping storage');
    }
    await page.locator('#chat-project-form .chat-project-modal__close').click();
    await waitFor(page, () => document.getElementById('chat-project-form') === null,
      'negative: project form never closed after the relative rejection');

    // ---- Runner A: invite through the token-required two-step ----
    await createProject(page, 'Mapped Project A', runnerAProject);
    await waitFor(page, () => document.getElementById('chat-empty-project') !== null, 'project lobby did not mount after create');
    const projectHero = await page.locator('#chat-empty-project').textContent() || '';
    if (!projectHero.includes('Add runners to this channel')) fail('project empty lobby title is missing');
    await laneShot(page, 'chat-empty-project-desktop.png', { fullPage: true });
    if ((await page.locator('#chat-invite').count()) !== 1) {
      fail('project lobby did not include the LAN invite surface');
    }
    if ((await page.locator('#chat-invite-runner-btn').count()) !== 1) {
      fail('project header Invite runner control is missing');
    }
    const inviteText = await page.locator('#chat-invite').textContent() || '';
    if (!inviteText.includes('Listeners on this network appear here')) fail('invite surface is missing the LAN hint');
    if (!inviteText.includes('Discovered on LAN')) fail('invite surface is missing LAN discovery');
    await laneShot(page, 'chat-invite-dialog.png', { fullPage: true });
    await addRunnerAddress(page, 'Runner A', runnerAOrigin);
    await completeTokenTwoStep(page, runnerAOrigin, runnerAToken, 'runner A');
    const runnerAId = await runnerIdByOrigin(page, runnerAOrigin);
    if (!runnerAId) fail('runner A has no durable registry identity');
    await waitChip(page, runnerAId, true, 'runner A did not authenticate and probe', 30000);
    // Gear menu runner details confirm the backend working directory without
    // ever exposing its path.
    await page.click('#chat-project-gear-btn');
    await waitFor(page, () => document.getElementById('chat-project-gear') !== null, 'project settings menu never opened');
    const gearA = await page.locator('#chat-project-gear').textContent() || '';
    if (!gearA.includes('Working directory') || !gearA.includes(runnerAProject)) {
      fail('runner A details did not show the mapped project working directory');
    }
    if (gearA.includes(runnerACwd)) fail('runner A details exposed its backend working directory');
    await page.click('#chat-project-gear-btn');

    // ---- Runner B: WRONG token via the two-step must never connect ----
    await createProject(page, 'Mapped Project B', runnerBProject);
    await openInvite(page);
    await addRunnerAddress(page, 'Runner B', runnerBOrigin);
    const cardB = page.locator('#chat-invite .chat-machine-card', { hasText: runnerBOrigin }).first();
    await cardB.locator('button', { hasText: 'Token required' }).click({ timeout: 30000 });
    ignoreWrongTokenAuthNoise = true;
    await page.fill('#chat-runner-token', wrongToken);
    await page.click('#chat-runner-add');
    await waitFor(page, (required) => {
      const alert = document.querySelector('.chat-session-toast');
      const message = alert?.querySelector('.chat-session-toast__text')?.textContent || '';
      return alert !== null && message.trim() === required;
    }, 'runner B wrong-token did not surface Token required', 30000, TOKEN_REQUIRED);
    if (await page.locator('#chat-runner-add').isDisabled()) fail('token form remained disabled after the wrong-token failure');
    if ((await page.inputValue('#chat-runner-token')) === '') fail('wrong-token cleared the token field');
    const runnerBIdWrong = await runnerIdByOrigin(page, runnerBOrigin);
    if (!runnerBIdWrong) fail('runner B wrong-token attempt left no durable record');
    const bHost = runnerHost(runnerBOrigin);
    const bSockets = wireRecorder.sockets.filter((socketUrl) => {
      try { return new URL(socketUrl).host === bHost; } catch { return false; }
    });
    if (bSockets.length !== 1) fail(`wrong-token opened ${bSockets.length} sockets to runner B (expected 1)`);
    const storedWs = new URL(runnerBOrigin).protocol === 'https:' ? 'wss:' : 'ws:';
    const otherWs = storedWs === 'wss:' ? 'ws:' : 'wss:';
    if (bSockets.some((socketUrl) => socketUrl.startsWith(otherWs))) {
      fail('wrong-token opened an alternate websocket scheme');
    }
    if (!bSockets[0].startsWith(storedWs)) fail('wrong-token socket did not use the stored origin scheme');
    if (!(await cardB.textContent() || '').includes(TOKEN_REQUIRED)) {
      fail('runner B card did not stay Token required');
    }
    // Negative stability: for a bounded window the wrong-token runner is
    // never invited (no "Added" pill) and never online (no "Add" button).
    for (let sample = 0; sample < 10; sample += 1) {
      await page.waitForTimeout(1000);
      const cardText = await cardB.textContent().catch(() => null);
      if (cardText === null) fail('invite dialog closed while the wrong-token runner was still unconnected');
      if (cardText.includes('Added')) fail('runner B was invited with the wrong token');
      if ((await cardB.locator('button', { hasText: /^Add$/ }).count()) > 0) fail('runner B reached online state with the wrong token');
    }

    await cardB.locator('button', { hasText: 'Remove' }).click({ timeout: 45000 });
    await waitFor(page, (origin) => {
      const invite = document.getElementById('chat-invite');
      if (invite === null) return true;
      return ![...invite.querySelectorAll('.chat-machine-card')].some((card) => (
        (card.textContent || '').includes(origin)
      ));
    }, 'removed wrong-token runner B stayed in the lobby', 30000, runnerBOrigin);
    if ((await registryRunners(page)).some((runner) => runner.id === runnerBIdWrong)) {
      fail('removed wrong-token runner B stayed in the durable registry');
    }
    ignoreWrongTokenAuthNoise = false;

    await openInvite(page);
    await addRunnerAddress(page, 'Runner B', runnerBOrigin);
    await completeTokenTwoStep(page, runnerBOrigin, runnerBToken, 'runner B corrected token');
    const runnerBId = await runnerIdByOrigin(page, runnerBOrigin);
    if (!runnerBId || runnerBId === runnerAId) fail('two distinct runner identities were not registered');
    await waitChip(page, runnerBId, true, 'runner B did not authenticate with the corrected token', 30000);
    if ((await registryRunners(page)).length !== 2) fail('two distinct runner records were not present');

    // ---- Active mapping measurement: now that BOTH runners exist, the
    // durable mapping document carries EXACTLY two active mappings — one
    // per project, each bound to its OWN runner. The measured count (never
    // a literal) is what the staged manifest asserts.
    const mappingStorageNow = await page.evaluate(
      () => window.localStorage.getItem('rpi-web-chat-mapping:v1') || '{}',
    );
    let mappingDocument = null;
    try {
      mappingDocument = JSON.parse(mappingStorageNow);
    } catch { /* a malformed durable mapping document fails the block below */ }
    if (!mappingDocument || !Array.isArray(mappingDocument.projects)
      || !Array.isArray(mappingDocument.mappings)) {
      fail('active mapping measurement: durable mapping document is missing or malformed');
    }
    const activeMappings = mappingDocument.mappings.filter((mapping) => mapping.state === 'active');
    if (activeMappings.length !== 2) {
      fail(`active mapping measurement: expected exactly two active mappings after both runners exist, found ${activeMappings.length}`);
    }
    for (const [label, projectCwd, runnerId] of [
      ['Mapped Project A', runnerAProject, runnerAId],
      ['Mapped Project B', runnerBProject, runnerBId],
    ]) {
      const project = mappingDocument.projects.find(
        (record) => record.label === label && record.cwd === projectCwd,
      );
      if (!project) fail(`active mapping measurement: no durable project record for ${label}`);
      const perProject = activeMappings.filter((mapping) => mapping.projectId === project.id);
      if (perProject.length !== 1 || perProject[0].runnerId !== runnerId) {
        fail(`active mapping measurement: ${label} does not carry exactly one active mapping bound to its own runner`);
      }
    }
    const measuredActiveMappings = activeMappings.length; // exactly 2

    const mappingCases = [
      { label: 'Mapped Project A', cwd: runnerAProject, home: runnerAHome },
      { label: 'Mapped Project B', cwd: runnerBProject, home: runnerBHome },
    ];
    for (const mappingCase of mappingCases) {
      if (mappedSessionFiles(mappingCase.home, mappingCase.cwd).length !== 1) {
        fail(`${mappingCase.label} did not create exactly one native session for its runner project`);
      }
    }
    if (mappedSessionFiles(runnerAHome, runnerBProject).length !== 0) {
      fail('runner A profile contains runner B project session state');
    }
    if (mappedSessionFiles(runnerBHome, runnerAProject).length !== 0) {
      fail('runner B profile contains runner A project session state');
    }
    const mappedAFile = mappedSessionFiles(runnerAHome, runnerAProject)[0];
    const mappedBFile = mappedSessionFiles(runnerBHome, runnerBProject)[0];
    if (!mappedAFile || !mappedBFile) fail('mapped project session files are unavailable');
    const mappedAHeader = sessionHeader(mappedAFile);
    const mappedBHeader = sessionHeader(mappedBFile);
    if (!mappedAHeader.id || !mappedBHeader.id || mappedAHeader.id === mappedBHeader.id) {
      fail('mapped project session identities were not isolated');
    }
    await laneShot(page, 'chat-project-mappings-active.png', { fullPage: true });
    await clickProjectRow(page, 'Mapped Project A');
    await page.locator('#chat-channel-target').click();
    await waitFor(page, () => {
      const broadcast = document.querySelector('.chat-broadcast');
      return broadcast !== null && (broadcast.textContent || '').includes('Send one message to the agent team.');
    }, 'Broadcast conversation did not mount');
    await laneShot(page, 'chat-broadcast.png', { fullPage: true });
    await page.fill('.chat-broadcast textarea', 'broadcast ping');
    await page.click('.chat-broadcast .chat-button--primary');
    await waitFor(page, () => {
      const broadcast = document.querySelector('.chat-broadcast');
      if (!broadcast) return false;
      const you = [...broadcast.querySelectorAll('.chat-session-message--user')]
        .some((el) => (el.textContent || '').includes('broadcast ping'));
      const assistants = [...broadcast.querySelectorAll('.chat-session-message--assistant')];
      const named = assistants.filter((el) => (el.querySelector('.chat-session-message__role')?.textContent || '').startsWith('@'));
      const live = broadcast.querySelectorAll([
        '.chat-broadcast-card .chat-runner-state[data-state="dispatching"]',
        '.chat-broadcast-card .chat-runner-state[data-state="streaming"]',
        '.chat-broadcast-card .chat-runner-state[data-state="cancelling"]',
      ].join(','));
      return you && named.length >= 1 && live.length === 0;
    }, 'Channel shared log did not finish with You plus a named runner reply', 45000);
    await clickProjectRow(page, 'Mapped Project A');
    await page.locator(`.chat-chip[data-runner-id="${runnerAId}"]`).click();
    await waitChatSessionReady(page, 'Project A 1:1');
    await page.fill('.chat-session .chat-session__composer textarea', 'one to one ping');
    await page.click('.chat-session .chat-session__composer .chat-button--primary');
    await waitSessionTurn(page, 'one to one ping', 'Project A 1:1 send did not complete');
    await assertNoVisibleChatError(page, 'Project A 1:1 send');

    await clickProjectRow(page, 'Mapped Project A');
    await openInvite(page);
    const runnerBCard = page.locator('#chat-invite .chat-machine-card')
      .filter({ has: page.locator('.chat-runner-origin', { hasText: runnerHost(runnerBOrigin) }) })
      .filter({ has: page.locator('button', { hasText: /^Add$/ }) })
      .first();
    await runnerBCard.waitFor({ state: 'visible', timeout: 15000 });
    await runnerBCard.locator('button', { hasText: /^Add$/ }).click();
    let inviteOutcome;
    try {
      const inviteHandle = await page.waitForFunction((ids) => {
        const err = [...document.querySelectorAll('#chat-invite .chat-form-error, .chat-session-toast')]
          .map((node) => (node.textContent || '').trim())
          .find((text) => text !== '');
        const chips = [...document.querySelectorAll('.chat-chip[data-runner-id]')].map((chip) => ({
          id: chip.getAttribute('data-runner-id'),
          tone: chip.querySelector('.chat-status-dot')?.getAttribute('data-tone') || '',
        }));
        const a = chips.find((chip) => chip.id === ids.a);
        const b = chips.find((chip) => chip.id === ids.b);
        if (a && b && a.tone === 'ok' && b.tone === 'ok') {
          return { ok: true, err: '', mappings: [] };
        }
        if (err) {
          let mappings = [];
          try {
            const doc = JSON.parse(window.localStorage.getItem('rpi-web-chat-mapping:v1') || '{}');
            mappings = Array.isArray(doc.mappings)
              ? doc.mappings.map((row) => ({ projectId: row.projectId, runnerId: row.runnerId, state: row.state }))
              : [];
          } catch {
            mappings = [];
          }
          return { ok: false, err, mappings };
        }
        return false;
      }, { a: runnerAId, b: runnerBId }, { timeout: 45000 });
      inviteOutcome = await inviteHandle.jsonValue();
    } catch (error) {
      const dump = await page.evaluate((ids) => {
        let mappings = [];
        try {
          const doc = JSON.parse(window.localStorage.getItem('rpi-web-chat-mapping:v1') || '{}');
          mappings = Array.isArray(doc.mappings)
            ? doc.mappings.map((row) => ({ projectId: row.projectId, runnerId: row.runnerId, state: row.state }))
            : [];
        } catch {
          mappings = [];
        }
        return {
          err: [...document.querySelectorAll('#chat-invite .chat-form-error, .chat-session-toast')]
            .map((node) => (node.textContent || '').trim()).filter(Boolean),
          chips: [...document.querySelectorAll('.chat-chip[data-runner-id]')].map((chip) => ({
            id: chip.getAttribute('data-runner-id'),
            text: (chip.textContent || '').replace(/\s+/g, ' ').trim(),
          })),
          cards: [...document.querySelectorAll('#chat-invite .chat-machine-card')].map((card) => ({
            origin: (card.querySelector('.chat-runner-origin')?.textContent || '').trim(),
            pill: (card.querySelector('.chat-added-pill')?.textContent || '').trim(),
            buttons: [...card.querySelectorAll('button')].map((btn) => (btn.textContent || '').trim()),
          })),
          mappings,
          ids,
        };
      }, { a: runnerAId, b: runnerBId });
      fail(`Project A did not host both runners after inviting the second runner dump=${JSON.stringify(dump)} error=${error instanceof Error ? error.message : String(error)}`);
    }
    if (!inviteOutcome.ok) {
      fail(`Project A second invite was rejected: ${inviteOutcome.err} mappings=${JSON.stringify(inviteOutcome.mappings)}`);
    }
    const overlayInvite = page.locator('#chat-invite.chat-invite--overlay');
    if (await overlayInvite.count()) {
      await overlayInvite.locator('button[aria-label="Close add runner"]').click();
    }
    await waitFor(page, () => document.querySelector('#chat-invite.chat-invite--overlay') === null, 'Invite dialog stayed open after the second invite');
    await page.locator('#chat-channel-target').click();
    await waitFor(page, () => document.querySelector('.chat-broadcast') !== null, 'Broadcast conversation did not remount after inviting the second runner');
    const promptsBeforeMention = { ...wireRecorder.promptsByHost };
    const mentionA = (await registryRunners(page)).find((runner) => runner.id === runnerAId);
    const mentionName = ((mentionA && mentionA.codename) || (mentionA && mentionA.label) || 'Runner A').trim().replace(/\s+/g, '-');
    await page.fill('.chat-broadcast textarea', '@');
    await waitFor(page, () => document.getElementById('chat-channel-mentions') !== null, 'mention picker did not open');
    await page.locator('#chat-channel-mentions .chat-mention-menu__item', { hasText: `@${mentionName}` }).click();
    await waitFor(page, (token) => {
      const area = document.querySelector('.chat-broadcast textarea');
      return area instanceof HTMLTextAreaElement && area.value.includes(`@${token}`);
    }, 'mention picker did not insert the runner token', 15000, mentionName);
    const mentionDraft = await page.locator('.chat-broadcast textarea').inputValue();
    await page.fill('.chat-broadcast textarea', `${mentionDraft}assign this`);
    await page.click('.chat-broadcast .chat-button--primary');
    await waitFor(page, (token) => {
      const broadcast = document.querySelector('.chat-broadcast');
      const log = broadcast?.querySelector('.chat-broadcast__transcript');
      if (!broadcast || !log) return false;
      const rows = [...log.querySelectorAll(':scope > .chat-session-message')];
      const mentionYou = rows.find((el) => (
        el.classList.contains('chat-session-message--user')
        && (el.textContent || '').includes(`@${token} assign this`)
      ));
      if (!mentionYou) return false;
      const namedAfter = [];
      let afterMention = false;
      for (const row of rows) {
        if (row === mentionYou) {
          afterMention = true;
          continue;
        }
        if (!afterMention || !row.classList.contains('chat-session-message--assistant')) continue;
        if ((row.querySelector('.chat-session-message__role')?.textContent || '').startsWith('@')) {
          namedAfter.push(row);
        }
      }
      const live = broadcast.querySelectorAll([
        '.chat-broadcast-card .chat-runner-state[data-state="dispatching"]',
        '.chat-broadcast-card .chat-runner-state[data-state="streaming"]',
        '.chat-broadcast-card .chat-runner-state[data-state="cancelling"]',
      ].join(','));
      return namedAfter.length === 1 && live.length === 0;
    }, 'mentioned runner did not finish as the only Channel recipient', 45000, mentionName);
    const hostA = runnerHost(runnerAOrigin);
    const hostB = runnerHost(runnerBOrigin);
    const promptsA = (wireRecorder.promptsByHost[hostA] || 0) - (promptsBeforeMention[hostA] || 0);
    const promptsB = (wireRecorder.promptsByHost[hostB] || 0) - (promptsBeforeMention[hostB] || 0);
    if (promptsA < 1) fail('mention send did not prompt the addressed runner');
    if (promptsB !== 0) fail('mention send prompted a runner that was not addressed');

    const bodyBeforeReload = await page.locator('body').textContent() || '';
    for (const sensitive of [
      runnerAToken,
      runnerBToken,
      wrongToken,
      runnerACwd,
      runnerBCwd,
      runnerAProject,
      runnerBProject,
      mappedAFile,
      mappedBFile,
    ]) {
      if (bodyBeforeReload.includes(sensitive)) fail('sensitive runner or project state appeared in rendered text');
    }
    await laneShot(page, 'chat-runners-connected.png', { fullPage: true });
    await page.setViewportSize({ width: 390, height: 844 });
    await clickProjectRow(page, 'Mapped Project A');
    await waitFor(page, () => {
      const main = document.getElementById('chat-project-main');
      const back = document.getElementById('chat-mobile-back-projects');
      return main !== null && main.hidden === false && main.getClientRects().length > 0
        && back !== null && back.getClientRects().length > 0;
    }, 'mobile conversation pane did not show the selected project');
    const phoneComposer = await page.evaluate(() => {
      const row = document.querySelector('.chat-session__composer-row');
      const textarea = row && row.querySelector('textarea');
      const send = row && row.querySelector('.chat-button--primary');
      if (!row || !textarea || !send) return null;
      const t = textarea.getBoundingClientRect();
      const s = send.getBoundingClientRect();
      const c = row.getBoundingClientRect();
      return {
        textareaTop: t.top,
        textareaLeft: t.left,
        sendTop: s.top,
        sendLeft: s.left,
        sendWidth: s.width,
        composerWidth: c.width,
      };
    });
    if (!phoneComposer) fail('mobile conversation composer was missing');
    if (phoneComposer.sendLeft <= phoneComposer.textareaLeft) {
      fail('phone Send is not to the right of the textarea');
    }
    if (Math.abs(phoneComposer.sendTop - phoneComposer.textareaTop) > 8) {
      fail('phone Send wrapped below the textarea');
    }
    if (phoneComposer.sendWidth >= phoneComposer.composerWidth * 0.8) {
      fail('phone Send is full-width');
    }
    const phoneHeader = await page.evaluate(() => {
      const add = document.getElementById('chat-invite-runner-btn');
      const gear = document.getElementById('chat-project-gear-btn');
      const details = [...document.querySelectorAll('.chat-project-header__actions button')]
        .find((button) => button.textContent.trim() === 'Details');
      if (!add || !gear || !details) return null;
      const a = add.getBoundingClientRect();
      const g = gear.getBoundingClientRect();
      const d = details.getBoundingClientRect();
      const sandwiched = (a.left < g.left && g.left < d.left) || (d.left < g.left && g.left < a.left);
      return {
        sandwiched,
        gearLeft: g.left,
        addLeft: a.left,
        detailsLeft: d.left,
        actionsRight: document.querySelector('.chat-project-header__actions')?.getBoundingClientRect().right ?? 0,
        gearRight: g.right,
      };
    });
    if (!phoneHeader) fail('mobile project header actions were missing');
    if (phoneHeader.sandwiched) fail('phone Settings gear is between Details and Add runner');
    if (phoneHeader.gearLeft < Math.max(phoneHeader.addLeft, phoneHeader.detailsLeft)) {
      fail('phone Settings gear is not at the far right of the header actions');
    }
    const phoneBack = await page.evaluate(() => {
      const back = document.getElementById('chat-mobile-back-projects');
      const title = document.querySelector('.chat-project-header h1');
      const header = document.querySelector('.chat-project-header');
      const row = document.querySelector('.chat-project-header__row');
      if (!back || !title || !header || !row) return null;
      const b = back.getBoundingClientRect();
      const t = title.getBoundingClientRect();
      return {
        backWidth: b.width,
        headerWidth: header.getBoundingClientRect().width,
        sameRow: row.contains(back) && row.contains(title) && Math.abs(b.top - t.top) <= 20,
        backLeftOfTitle: b.left < t.left,
        label: back.getAttribute('aria-label'),
        text: (back.textContent || '').trim(),
      };
    });
    if (!phoneBack) fail('mobile project Back control was missing');
    if (phoneBack.backWidth >= phoneBack.headerWidth * 0.5) fail('phone Back is a full-width row');
    if (!phoneBack.sameRow || !phoneBack.backLeftOfTitle) fail('phone Back is not on the title row');
    if (phoneBack.label !== 'Back to Projects' || phoneBack.text !== '←') {
      fail('phone Back is not a compact ← control');
    }
    await laneShot(page, 'chat-conversation-mobile.png', { fullPage: true });
    await page.setViewportSize({ width: 1360, height: 900 });
    await waitFor(page, () => {
      const add = document.getElementById('chat-project-add-btn');
      return add !== null && add.getClientRects().length > 0;
    }, 'desktop rail did not return after the mobile conversation screenshot');

    // ---- Durable restore: both runners reconnect, mappings stay active ----
    await page.reload({ waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.getElementById('chat-projects') !== null, '/chat app did not remount');
    const restored = await registryRunners(page);
    if (restored.length !== 2 || new Set(restored.map((runner) => runner.id)).size !== 2) {
      fail('durable runner registry did not restore exactly two runner records');
    }
    for (const projectName of ['Mapped Project A', 'Mapped Project B']) {
      await clickProjectRow(page, projectName);
      const member = projectName === 'Mapped Project A' ? runnerAId : runnerBId;
      await waitChip(page, member, true, `durable registry did not reconnect ${projectName}'s runner`, 45000);
    }
    await clickProjectRow(page, 'Mapped Project A');
    await page.locator('#chat-channel-target').click();
    await waitFor(page, (token) => {
      const broadcast = document.querySelector('.chat-broadcast');
      if (!broadcast) return false;
      const you = [...broadcast.querySelectorAll('.chat-session-message--user')]
        .some((el) => (el.textContent || '').includes('broadcast ping'));
      const mention = [...broadcast.querySelectorAll('.chat-session-message--user')]
        .some((el) => (el.textContent || '').includes(`@${token} assign this`));
      const assistants = [...broadcast.querySelectorAll('.chat-session-message--assistant')];
      return you && mention && assistants.length >= 1;
    }, 'channel log did not restore You plus the mention and a runner reply after reload', 15000, mentionName);
    if (mappedSessionFiles(runnerAHome, runnerAProject).length !== 1
      || mappedSessionFiles(runnerBHome, runnerBProject).length !== 1) {
      fail('reload reconciliation duplicated or lost mapped session files');
    }

    // ---- Backend shutdown: runner B goes down, runner A unaffected ----
    await clickProjectRow(page, 'Mapped Project B');
    requestRunnerBKill();
    await waitFor(page, (id) => {
      const chip = document.querySelector(`.chat-chip[data-runner-id="${id}"]`);
      return chip !== null && chip.querySelector('.chat-status-dot')?.getAttribute('data-tone') !== 'ok';
    }, 'runner B did not go down after backend shutdown', 30000, runnerBId);
    await clickProjectRow(page, 'Mapped Project A');
    await waitChip(page, runnerAId, true, 'runner A did not remain connected while runner B was down', 30000);
    await laneShot(page, 'chat-runner-b-reconnecting.png', { fullPage: true });

    const mappedBSessionId = mappedBHeader.id;
    const getMessagesBeforeRecover = wireRecorder.getMessagesBySession[mappedBSessionId] || 0;
    await awaitRunnerBUp();
    await clickProjectRow(page, 'Mapped Project B');
    await waitChip(page, runnerBId, true, 'runner B did not recover after same-port restart', 45000);
    await clickProjectRow(page, 'Mapped Project A');
    await waitChip(page, runnerAId, true, 'runner A changed state during runner B recovery', 30000);
    await clickProjectRow(page, 'Mapped Project B');
    // The mapping row of the recovered runner is its conversation chip: it
    // only renders for an active mapping, so presence IS the active state.
    await waitChip(page, runnerBId, true, 'runner B project mapping did not remain after same-port backend restart', 30000);
    if (mappedSessionFiles(runnerBHome, runnerBProject).length !== 1) {
      fail('runner B restart recovery duplicated or lost its mapped session file');
    }
    await page.locator(`.chat-chip[data-runner-id="${runnerBId}"]`).click();
    await waitChatSessionReady(page, 'runner B same-port recovery');
    await assertNoVisibleChatError(page, 'runner B same-port recovery');
    const getMessagesAfterRecover = wireRecorder.getMessagesBySession[mappedBSessionId] || 0;
    if (getMessagesAfterRecover <= getMessagesBeforeRecover) {
      fail('mapped get_messages was not sent after runner B same-port recovery');
    }
    await waitSessionIdle(page, 'runner B was not idle before recovery send');
    await page.fill('.chat-session .chat-session__composer textarea', 'ping after recovery');
    await page.click('.chat-session .chat-session__composer .chat-button--primary');
    await waitSessionTurn(page, 'ping after recovery', 'mapped send after runner B recovery did not complete');
    await assertNoVisibleChatError(page, 'runner B same-port recovery send');
    await laneShot(page, 'chat-runner-b-recovered.png', { fullPage: true });

    // ---- Deletion: runner-settings Delete runner archives the mapped session ----
    await page.locator(`.chat-runner-row[data-runner-id="${runnerBId}"]`).click();
    await waitFor(page, () => document.getElementById('chat-runner-gear-btn') !== null, 'runner B detail never opened for deletion');
    await page.click('#chat-runner-gear-btn');
    await waitFor(page, () => document.getElementById('chat-runner-delete') !== null, 'runner B settings menu never opened for deletion');
    await page.click('#chat-runner-delete');
    await waitFor(page, () => {
      const btn = document.getElementById('chat-runner-delete');
      return btn !== null && (btn.getAttribute('aria-label') || '') === 'Confirm delete runner';
    }, 'runner B delete did not arm Confirm delete runner');
    await page.click('#chat-runner-delete');
    await waitFor(page, (id) => document.querySelector(`.chat-chip[data-runner-id="${id}"]`) === null
      && document.querySelector(`.chat-runner-row[data-runner-id="${id}"]`) === null,
      'runner B was not removed from the project', 45000, runnerBId);
    await waitFor(page, (id) => {
      try {
        const doc = JSON.parse(window.localStorage.getItem('rpi-web-chat-runners:v1') || '{}');
        return Array.isArray(doc.runners) && doc.runners.every((runner) => runner.id !== id);
      } catch {
        return false;
      }
    }, 'runner B registry record was not persisted away', 45000, runnerBId);
    await clickProjectRow(page, 'Mapped Project A');
    await waitChip(page, runnerAId, true, 'runner A changed state when runner B was deleted', 30000);
    await page.reload({ waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.getElementById('chat-projects') !== null, '/chat app did not remount after deletion');
    await clickProjectRow(page, 'Mapped Project A');
    await waitChip(page, runnerAId, true, 'runner A did not restore after deletion reload', 30000);
    await clickProjectRow(page, 'Mapped Project B');
    if ((await page.locator(`.chat-chip[data-runner-id="${runnerBId}"]`).count()) !== 0) {
      fail('deleted runner B resurrected on Project B after reload');
    }
    const afterDelete = await registryRunners(page);
    if (afterDelete.some((runner) => runner.id === runnerBId)) fail('deleted runner B returned after reload');
    if (afterDelete.length !== 1) fail('deletion cleanup left an unexpected runner record');
    if (mappedSessionFiles(runnerBHome, runnerBProject).length !== 0) {
      fail('runner B deletion did not archive its mapped project session');
    }

    const negativeMissingCwd = `${runnerAProject.slice(0, runnerAProject.lastIndexOf('/'))}/chat-negative-missing`;
    if (fs.existsSync(negativeMissingCwd)) fail('negative: missing-directory fixture already exists');
    await createProject(page, 'Negative Missing', negativeMissingCwd);
    await openInvite(page);
    const createsBeforeMissing = wireRecorder.newSessionCommands;
    const cardAMissing = page.locator('#chat-invite .chat-machine-card', { hasText: runnerAOrigin }).first();
    await cardAMissing.locator('button', { hasText: /^Add$/ }).click({ timeout: 30000 });
    await waitFor(page, () => {
      const alerts = [...document.querySelectorAll('.chat-session-toast')];
      let stored = [];
      try {
        stored = JSON.parse(window.localStorage.getItem('rpi-web-chat-mapping:v1') || '{}').mappings || [];
      } catch { /* ignore */ }
      return alerts.length === 0 && stored.filter((mapping) => mapping.state === 'active').length >= 2;
    }, 'negative: missing-directory invite did not persist an active mapping', 30000);
    const missingDeadline = Date.now() + 15000;
    while (!fs.existsSync(negativeMissingCwd) && Date.now() < missingDeadline) {
      await page.waitForTimeout(100);
    }
    if (!fs.existsSync(negativeMissingCwd)) {
      fail('negative: missing-directory invite did not auto-create the project directory');
    }
    const missingCreates = wireRecorder.newSessionCommands - createsBeforeMissing;
    if (missingCreates !== 1) {
      fail(`negative: missing-directory invite sent ${missingCreates} runner new_session commands (expected exactly one)`);
    }
    const missingStorage = await page.evaluate(() => window.localStorage.getItem('rpi-web-chat-mapping:v1') || '{}');
    let storedMappings = [];
    try {
      storedMappings = JSON.parse(missingStorage).mappings || [];
    } catch { /* corrupt storage fails the block below */ }
    if (storedMappings.filter((mapping) => mapping.state === 'active').length < 2) {
      fail('negative: missing-directory auto-create did not persist an active mapping');
    }

    const localStorageSnapshot = await page.evaluate(() => Object.fromEntries(
      Array.from({ length: window.localStorage.length }, (_, index) => {
        const key = window.localStorage.key(index) || '';
        return [key, window.localStorage.getItem(key) || ''];
      }),
    ));
    // The production token key is the EXACT authority, percent-encoded:
    // `rpi-web-token:<encodeURIComponent(authority)>` (hostToken.ts
    // hostTokenKey). Deletion must REMOVE the key entirely — require its
    // absence, not merely an empty value.
    const runnerBTokenKey = `rpi-web-token:${encodeURIComponent(runnerHost(runnerBOrigin))}`;
    if (Object.prototype.hasOwnProperty.call(localStorageSnapshot, runnerBTokenKey)) {
      fail('runner B token key remained after deletion');
    }
    const registryJson = localStorageSnapshot['rpi-web-chat-runners:v1'] || '';
    const mappingJson = localStorageSnapshot['rpi-web-chat-mapping:v1'] || '';
    for (const secret of [runnerAToken, runnerBToken, wrongToken]) {
      if (registryJson.includes(secret) || mappingJson.includes(secret)) {
        fail('durable chat storage stored a raw token');
      }
    }
    if (!mappingJson.includes(mappedAFile)) {
      fail('mapping storage lost runner A project identity');
    }
    if (mappingJson.includes(mappedBFile)) {
      fail('mapping cleanup retained archived runner B project identity');
    }
    const finalMain = await page.locator('#chat-project-main').textContent() || '';
    if (finalMain.includes(IDENTITY_UNAVAILABLE) || finalMain.includes(CONNECTION_CLOSED_1006)) {
      fail('visible main still shows identity-unavailable or 1006 copy');
    }
    const finalBody = await page.locator('body').textContent() || '';
    for (const sensitive of [
      runnerAToken,
      runnerBToken,
      wrongToken,
      runnerACwd,
      runnerBCwd,
      runnerAProject,
      runnerBProject,
      mappedAFile,
      mappedBFile,
    ]) {
      if (finalBody.includes(sensitive)) fail('sensitive runner or project state appeared in final rendered text');
    }
    // Designated final proof screenshot + pass marker JSON are STAGED only —
    // never published before the gates pass.
    await laneShot(page, 'chat-runner-delete-cleanup.png', { fullPage: true });
    // The assertions manifest is STAGED (registered artifacts + exact
    // assertion keys; projectMappingsActive is the MEASURED count, never a
    // literal) — never published: the shell commits the run dir and
    // pointer only after the backend RPC, route, cleanup, and
    // listener-close gates pass.
    stageManifest({
      routeMounted: true,
      emptyWorkspaceHero: true,
      emptyWorkspaceMobile: true,
      emptyProjectHero: true,
      inviteLanHint: true,
      wrongTokenRejected: true,
      tokenTwoStepRecovered: true,
      twoRealRunnersConnected: true,
      durableReloadRestored: true,
      channelSharedLog: true,
      channelTwoRunnersOneProject: true,
      channelMentionTargeted: true,
      channelLogReloadRestored: true,
      independentReconnect: true,
      samePortRecovery: true,
      projectMappingsActive: measuredActiveMappings,
      projectSessionIdentityIsolated: true,
      projectSessionCwdIsolated: true,
      mappingReloadRecovered: true,
      deletionPersisted: true,
      mappedSessionCleanup: true,
      tokenRedaction: true,
      relativeLocalSubmitOnce: true,
      relativeZeroRunnerCreates: true,
      missingDirectoryExactlyOneCreate: true,
      zeroBrowserErrors: true,
    });

    // All actions executed: close the page/browser while the handlers are
    // still effective, so close-time errors land in the same arrays. The
    // close is BOUNDED: a rejection or a close that exceeds the deadline
    // FAILS the run — never a silent pass (the shell's setsid group kill
    // owns any surviving browser child on every exit path).
    await closeBrowser(browser);

    // Final union gate AFTER everything is closed.
    if (pageErrors.length || consoleErrors.length) {
      fail(`browser errors: ${JSON.stringify({ pageErrors, consoleErrors })}`);
    }
    // STAGING ONLY: leave the staged set untouched for the shell's full-lane
    // commit (backend RPC/route/cleanup/listener-close gates + exact
    // manifest/artifact validation + ONE run-dir rename + ONE pointer
    // replace), then report the staged path. This script NEVER prints PASS:
    // the lane PASS line is printed by chat.sh only after the commit.
    console.log(`web-chat: STAGED ${stagingDir}`);
    console.log('web-chat: STAGE COMPLETE (staged; publication and the PASS line are owned by chat.sh)');
  } catch (error) {
    // Bounded close on EVERY failure path: a hung or failed close only
    // REJECTS (the shell's group kill owns a surviving child) while the
    // ORIGINAL error below still fails the lane. No lane failure
    // bypasses this close with a direct process.exit — the exit happens
    // only here, after the close has settled.
    await closeBrowser(browser).catch(() => {});
    // A failed run removes ONLY its own staging dir; the run-dir rename and
    // pointer commit belong to the shell, so nothing published can exist to
    // roll back here. The prior pointer and prior published runs are never
    // reachable from this script.
    cleanupStaging();
    console.error(`web-chat: FAIL: ${error && error.message ? error.message : error}`);
    if (INJECT_FAIL && fs.existsSync(stagingDir)) {
      // Deterministic self-test escalation: exit 2 must mean staging is
      // gone; the shell's post-mortem gate owns the pointer/prior-run
      // assertions.
      console.error(`web-chat: FAIL: staging dir survived failure cleanup: ${stagingDir}`);
      process.exit(3);
    }
    process.exit(2);
  }
}

main().catch((error) => {
  // Top-level failure OUTSIDE the browser lifecycle (e.g. a rejected
  // chromium.launch): no browser exists to bound a close on. Remove ONLY
  // this attempt's staging dir, log the failure, and exit NONZERO — the
  // error is never swallowed, and the previously published evidence and
  // prior pointer are never touched (the shell owns every publication
  // failure path).
  console.error(`web-chat: FAIL: ${error && error.message ? error.message : error}`);
  cleanupStaging();
  if (INJECT_FAIL && fs.existsSync(stagingDir)) {
    // Deterministic self-test escalation: exit 2 must mean staging is
    // gone; the shell's post-mortem gate owns the pointer/prior-run
    // assertions.
    console.error(`web-chat: FAIL: staging dir survived failure cleanup: ${stagingDir}`);
    process.exit(3);
  }
  process.exit(2);
});
