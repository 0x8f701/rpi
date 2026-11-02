// Focused live probe for the P0 connection-owned pressure release contract
// (RSS owner only).
//
// Reproduces the exact memory state of the failing release-dist run
// (20 MiB native session + 8-runtime pool + 2 MiB selected-branch pressure
// session) without the full marker contracts, then:
//   1. opens the exact 2 MiB pressure session live and checks the loaded
//      pool stays at 8 runtimes
//   2. samples the process tree to an RSS plateau (hard gate preserved)
//   3. installs the paused unread subscription over a raw ws pressure
//      client and waits for the RSS rise (>= 1 MiB) plus the connection
//      signal +1 (fd table when readable, else the listener-local-port
//      established count; fail-closed when neither exists)
//   4. causally destroys the pressure client and gates the release window
//      (sampleRssUntilRelease: 250ms cadence, 120 samples = 30s worst case,
//      8-sample confirmation) on the drop covering the installed magnitude,
//      the connection signal decrease, and an unchanged processCount, while
//      both control clients stay connected; the unrelated close happens
//      only after the verdict
//
// Verdicts:
//   exit 0  installed pressure released within the bounded window (RSS drop
//           >= installed bytes, connection signal decreased post-destroy)
//   exit 3  no release within the bounded window: the error message carries
//           the full delta series and the evidence JSON keeps every sample
//           (RSS, fdSocketCount, listenerEstablished, processCount,
//           tcpSockets) as the production allocator/resource blocker
//           record. Tolerances are never weakened.
//   exit 2  unexpected error (setup/transport): partial evidence is
//           attempted, then cleanup still runs in the outer finally.
//
// Usage:
//   node E2E.d/web/p0_release_probe.mjs
//   RPI_BIN=/path/to/rpi node E2E.d/web/p0_release_probe.mjs

import { spawn } from 'node:child_process';
import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import WebSocket from 'ws';
import {
  createControlClient,
  createNodePressureClient,
  P0ContractError,
  fillEvictReopenPool,
  generateAndLoadExactSession,
  installConnectionOwnedPressure,
  loadedIdsFromList,
  MAX_LOADED_RUNTIMES,
  openExactNativeSessionLive,
  releaseConnectionOwnedPressure,
  requireRpcData,
  sampleRssUntilPlateau,
} from './lib/p0_resource_driver.mjs';
import {
  nativeSessionPath,
  writeExactSelectedPressureSession,
} from './lib/p0_native_session.mjs';
import { writeEvidenceJson } from './lib/p0_resource_metrics.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const rpiBin = process.env.RPI_BIN || join(here, '..', '..', 'target', 'release-dist', 'rpi');
const evidenceRoot = '/tmp/p0-rss-probe-evidence';
const NATIVE_ID = 'p0-native-20mib';
const PRESSURE_ID = 'p0-pressure-2mib';
const LISTENER_DEADLINE_MS = 30000;

function fail(message) {
  throw new Error(`p0-release-probe: FAIL: ${message}`);
}

const log = (...rows) => console.log('p0-release-probe:', ...rows);
const mib = (bytes) => (bytes / (1024 * 1024)).toFixed(2) + ' MiB';

function seriesSummary(samples, baselineBytes) {
  const t0 = samples.length ? samples[0].tMs : 0;
  return samples.map((s, index) => {
    const delta = s.rssBytes === null ? 'unreadable'
      : `${((s.rssBytes - baselineBytes) / (1024 * 1024)).toFixed(2)}MiB`;
    const threads = s.threadCount === null ? '?' : s.threadCount;
    const tcp = s.tcpSockets ? s.tcpSockets.established : '?';
    return `#${index} +${s.tMs - t0}ms rss=${s.rssBytes === null ? 'null' : mib(s.rssBytes)} delta=${delta} threads=${threads} tcpEst=${tcp}`;
  }).join('\n');
}

async function waitForListener(child, deadlineMs) {
  let text = '';
  const startedAt = Date.now();
  while (Date.now() - startedAt < deadlineMs) {
    if (child.exitCode !== null) fail(`rpi exited early (code=${child.exitCode}): ${text.slice(0, 800)}`);
    const chunk = await new Promise((resolve) => {
      const timer = setTimeout(() => resolve(''), 200);
      child.stderr.once('data', (data) => {
        clearTimeout(timer);
        resolve(String(data));
      });
    });
    text += chunk;
    const banner = /Control plane listening on http:\/\/([0-9.]+):(\d+)/.exec(text);
    if (banner) return `http://${banner[1]}:${banner[2]}`;
  }
  fail(`listener banner not seen within ${deadlineMs}ms; stderr: ${text.slice(0, 800)}`);
  return null;
}

// Resolves true when the child exits within ms, false on timeout. The
// timer is always cleared on exit, so no timer is ever left scheduled
// after an earlier exit.
function waitExit(child, ms) {
  if (child.exitCode !== null) return Promise.resolve(true);
  return new Promise((resolve) => {
    const onExit = () => {
      clearTimeout(timer);
      resolve(true);
    };
    const timer = setTimeout(() => {
      child.removeListener('exit', onExit);
      resolve(false);
    }, ms);
    child.once('exit', onExit);
  });
}

// SIGTERM, await a bounded exit, then SIGKILL survivors and await their
// exit too. Never leaves a timer pending behind an already-exited child.
async function teardownProcess(child, label) {
  if (!child || child.exitCode !== null) return;
  try { child.kill('SIGTERM'); } catch {}
  if (!(await waitExit(child, 1000))) {
    log(`${label} survived SIGTERM; sending SIGKILL`);
    try { child.kill('SIGKILL'); } catch {}
    await waitExit(child, 5000);
  }
}

async function main() {
  const root = await mkdtemp(join(tmpdir(), 'p0-rss-probe-'));
  const workspace = join(root, 'workspace');
  const home = join(root, 'home');
  const rpiHome = join(home, '.rpi');
  const sessionsRoot = join(rpiHome, 'sessions');
  const token = `p0-rss-probe-${Date.now()}`;
  const nativePath = nativeSessionPath({
    sessionsRoot,
    cwd: workspace,
    sessionId: NATIVE_ID,
    timestamp: '2026-08-31T00:00:00.000Z',
  });
  await mkdir(workspace, { recursive: true });
  await mkdir(rpiHome, { recursive: true });
  await writeFile(join(root, 'token'), `${token}\n`);
  const written = await generateAndLoadExactSession({
    filePath: nativePath,
    cwd: workspace,
    sessionId: NATIVE_ID,
  });
  log(`native session written: ${written.bytes} bytes marker=${written.marker}`);

  // Local mock provider (same helper as p0_resource.sh) so the listener
  // starts exactly like the lane fixture; the probe itself never prompts.
  const portFile = join(root, 'mock-port.txt');
  const mock = spawn('python3', [join(here, 'lib', 'p0_resource_mock.py'), '--port-file', portFile], {
    stdio: ['ignore', 'ignore', 'inherit'],
  });
  let mockPort = null;
  for (let i = 0; i < 50; i += 1) {
    try {
      mockPort = Number((await readFile(portFile, 'utf8')).trim());
      break;
    } catch {
      await new Promise((resolve) => setTimeout(resolve, 200));
    }
  }
  // Fail-closed: models.json must never point at an unwritten or garbage
  // port, so validate the mock port right after the wait and before the
  // model config is written. Kill the mock so nothing lingers behind.
  if (!Number.isInteger(mockPort) || mockPort <= 0 || mockPort > 65535) {
    try { mock.kill('SIGKILL'); } catch {}
    fail(`mock provider did not produce a valid port (portFile=${portFile}, value=${mockPort})`);
  }
  const models = {
    providers: {
      'user-steering': {
        baseUrl: `http://127.0.0.1:${mockPort}`,
        api: 'openai-completions',
        models: [{ id: 'mock', name: 'P0 RSS Probe Mock', contextWindow: 32768, maxTokens: 2048 }],
      },
    },
  };
  await writeFile(join(rpiHome, 'models.json'), JSON.stringify(models, null, 2));

  const child = spawn(rpiBin, [
    '--offline',
    '--listen', '127.0.0.1:0',
    '--listen-plaintext',
    '--listen-token-file', join(root, 'token'),
    '--model', 'user-steering/mock',
    '--api-key', 'user-mock-key',
  ], {
    cwd: workspace,
    env: {
      HOME: home,
      USERPROFILE: home,
      RPI_HOME: rpiHome,
      RPI_OFFLINE: '1',
      RPI_SKIP_VERSION_CHECK: '1',
      PATH: process.env.PATH || '/usr/bin:/bin',
      LANG: 'C.UTF-8',
      LC_ALL: 'C.UTF-8',
    },
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  const url = (await waitForListener(child, LISTENER_DEADLINE_MS)).replace(/\/$/, '');
  const hostPort = url.replace(/^https?:\/\//, '').split('/')[0];
  const wsUrl = `ws://${hostPort}/ws`;
  // The listener's local TCP port feeds the listener-local-port established
  // count fallback when the /proc fd tables are unreadable.
  const listenerPort = Number(new URL(url).port);
  log(`listener at ${url} pid=${child.pid} port=${listenerPort}`);
  const pid = child.pid;

  let clientA = null;
  let clientB = null;
  let installed = null;
  let pressureClient = null;
  let released = null;
  let plateau = null;
  let pressureLive = null;
  let loadedPrePressure = [];
  let exitCode = 2;
  try {
    clientA = createControlClient({ url: wsUrl, token, WebSocketImpl: globalThis.WebSocket });
    clientB = createControlClient({ url: wsUrl, token, WebSocketImpl: globalThis.WebSocket });
    await Promise.all([clientA.ready, clientB.ready]);
    log('two control clients connected');

    const live = await openExactNativeSessionLive(clientA, written);
    log(`native session live: ${live.sessionId} incarnation=${live.identity.runtimeIncarnation}`);
    const pool = await fillEvictReopenPool(clientA);
    log(`pool filled: loaded=${pool.loadedCountAfterReopen} retired=${pool.retired}`);

    // Connection-owned pressure target: the exact 2 MiB selected-branch
    // pressure session opened live, with the loaded pool checked to stay at
    // 8 runtimes after the pressure open (same contract as the full lane).
    const pressurePath = nativeSessionPath({
      sessionsRoot,
      cwd: workspace,
      sessionId: PRESSURE_ID,
      timestamp: '2026-08-31T00:00:00.000Z',
    });
    const pressureWritten = await writeExactSelectedPressureSession({
      filePath: pressurePath,
      cwd: workspace,
      sessionId: PRESSURE_ID,
    });
    pressureLive = await openExactNativeSessionLive(clientA, pressureWritten);
    log(`pressure session live: ${pressureWritten.sessionId} bytes=${pressureWritten.bytes}`);
    const listed = await clientA.call({
      type: 'session_list',
      scope: 'all_projects',
      sessionId: pool.primary,
    });
    loadedPrePressure = loadedIdsFromList(await requireRpcData(listed, 'session_list:probe'));
    if (loadedPrePressure.length !== MAX_LOADED_RUNTIMES) {
      fail(`pressure open changed loaded pool: expected ${MAX_LOADED_RUNTIMES}, got ${loadedPrePressure.length}`);
    }

    // Plateau hard gate before any disconnect (preserved unchanged).
    plateau = await sampleRssUntilPlateau({
      pid,
      minSamples: 5,
      maxSamples: 20,
    });
    if (!plateau.plateau || plateau.plateau.detected !== true) {
      fail(`plateau.detected !== true: ${JSON.stringify(plateau.plateau)}`);
    }
    log(`plateau detected: slopeBytesPerMs=${plateau.slopeBytesPerMs}`);

    pressureClient = createNodePressureClient({ url: wsUrl, token, WebSocketImpl: WebSocket });
    await pressureClient.ready;
    installed = await installConnectionOwnedPressure({
      pid,
      pressureClient,
      identity: pressureLive.identity,
      listenerPort,
    });
    log(`pressure installed: signal=${installed.signalMode} h0=${installed.h0}B h1=${installed.h1}B `
      + `installedBytes=${installed.installedBytes}B `
      + (installed.signalMode === 'fd'
        ? `fd0=${installed.fd0} fd1=${installed.fd1}`
        : `conn0=${installed.conn0} conn1=${installed.conn1} listenerPort=${installed.listenerPort}`));

    // Causal destroy of the connection-owned pressure client only: the
    // release window is sampled while both control clients stay connected,
    // so an unrelated disconnect can never cause the release. The
    // unrelated close happens in cleanup, strictly after the verdict.
    try {
      released = await releaseConnectionOwnedPressure({
        pid,
        h1: installed.h1,
        installedBytes: installed.installedBytes,
        fd1: installed.signalMode === 'fd' ? installed.fd1 : undefined,
        listenerPort: installed.signalMode === 'listener' ? installed.listenerPort : undefined,
        conn1: installed.signalMode === 'listener' ? installed.conn1 : undefined,
        processCount1: installed.processCount,
        pressureClient: installed.pressureClient,
      });
      log(`release sampled with both control clients still connected (signal=${installed.signalMode})`);
    } catch (err) {
      if (err instanceof P0ContractError && err.rssEvidence && err.rssEvidence.release) {
        // Expected no-release verdict: retain the full sample-series
        // evidence, released=false, exit 3.
        released = err.rssEvidence.release;
        log(`no-release within the bounded window: reason=${released.reason}`);
      } else {
        throw err; // unexpected: outer catch exits 2
      }
    }
    pressureClient = null; // destroyed causally by releaseConnectionOwnedPressure

    exitCode = released && released.released === true ? 0 : 3;

    // Evidence is written BEFORE any cleanup, so the production
    // allocator/resource blocker record survives the teardown.
    const evidencePath = join(evidenceRoot, `p0-rss-probe-${new Date().toISOString().replace(/[:.]/g, '-')}.json`);
    const payload = {
      probe: 'p0-release-probe.v2',
      rpiBinary: rpiBin,
      pid,
      released: released !== null && released.released === true,
      signalMode: installed ? installed.signalMode : null,
      plateau: plateau ? plateau.plateau : null,
      pressure: installed ? {
        h0: installed.h0,
        h1: installed.h1,
        installedBytes: installed.installedBytes,
        signalMode: installed.signalMode,
        fd0: installed.fd0,
        fd1: installed.fd1,
        listenerPort: installed.listenerPort,
        conn0: installed.conn0,
        conn1: installed.conn1,
        processCount: installed.processCount,
        pressureIdentity: pressureLive ? pressureLive.identity : null,
        loadedPrePressure,
      } : null,
      release: released,
    };
    await writeEvidenceJson(evidencePath, payload, {});
    log(`evidence written: ${evidencePath}`);

    if (released && released.released === true) {
      const baseline = released.baselineBytes;
      console.log('\nVERDICT: CONNECTION-OWNED PRESSURE RELEASED');
      console.log(`  signal=${installed.signalMode} baseline=${baseline}B (${mib(baseline)})`);
      console.log(`  installedBytes=${released.installedBytes}B released at sample #${released.releasedAtSample} after ${released.releaseElapsedMs}ms`);
      console.log(`  releasedRss=${released.magnitudeBytes !== null ? baseline - released.magnitudeBytes : '?'}B delta=-${released.magnitudeBytes}B (${mib(released.magnitudeBytes)})`);
      console.log(`  confirmation window: ${released.sampleCountTaken} samples, stoppedEarly=${released.stoppedEarly}`);
      console.log(seriesSummary(released.samples, baseline));
    } else {
      console.log('\nVERDICT: NO RELEASE WITHIN BOUNDED WINDOW (production allocator/resource blocker)');
      if (released) console.log(seriesSummary(released.samples, released.baselineBytes));
    }
  } catch (err) {
    exitCode = 2;
    console.error(`p0-release-probe: ERROR: ${err instanceof Error ? err.stack || err.message : String(err)}`);
    // Evidence attempt before cleanup, with whatever was collected.
    try {
      const partialPath = join(evidenceRoot, `p0-rss-probe-error-${new Date().toISOString().replace(/[:.]/g, '-')}.json`);
      await writeEvidenceJson(partialPath, {
        probe: 'p0-release-probe.v2',
        rpiBinary: rpiBin,
        pid,
        exitCode,
        error: err instanceof Error ? err.message : String(err),
        plateau: plateau ? plateau.plateau : null,
        pressure: installed,
        release: released,
        pressureIdentity: pressureLive ? pressureLive.identity : null,
        loadedPrePressure,
      }, {});
      log(`partial evidence written: ${partialPath}`);
    } catch {}
  } finally {
    // Cleanup always runs, strictly after the verdict (and evidence write):
    // unrelated control clients, pressure client, child, mock, temp dir.
    if (pressureClient) {
      await pressureClient.destroy().catch(() => {});
      pressureClient = null;
    }
    try {
      if (clientA) await clientA.close(1000, 'probe-done');
      if (clientB) await clientB.close(1000, 'probe-done');
    } catch {}
    // Bounded teardown: SIGTERM both the rpi listener and the mock, await
    // their exit, SIGKILL any survivor and await again. The temp root is
    // only removed once both processes are gone, and the process exits
    // right after with no timer ever scheduled past an earlier exit.
    await Promise.all([
      teardownProcess(child, 'rpi'),
      teardownProcess(mock, 'mock'),
    ]);
    await rm(root, { recursive: true, force: true }).catch(() => {});
    // Stdout to a pipe is async in Node; drain queued verdict output via
    // an in-order write callback (no timer) before the hard exit.
    await new Promise((resolve) => {
      if (process.stdout.writableLength === 0) return resolve();
      process.stdout.write('', resolve);
    });
    process.exit(exitCode);
  }
}

main().catch((err) => {
  console.error(`p0-release-probe: ERROR: ${err instanceof Error ? err.stack || err.message : String(err)}`);
  process.exit(2);
});