// Live P0 session-acceptance driver (playwright half of p0_resource.sh).
// Two Chromium pages on one listener plus process-tree RSS. Injectable
// helpers live in lib/p0_resource_driver.mjs.

import { chromium } from 'playwright';
import WebSocket from 'ws';
import fs from 'node:fs';
import path from 'node:path';
import {
  closeBrowserClients,
  createControlClient,
  createNodePressureClient,
  createPageWebSocketImpl,
  runP0ResourceContract,
} from './lib/p0_resource_driver.mjs';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';
const rpiPid = process.env.RPI_PID ? Number(process.env.RPI_PID) : null;
const nativePath = process.env.RPI_NATIVE_SESSION_PATH || '';
const nativeCwd = process.env.RPI_NATIVE_CWD || '';
const nativeId = process.env.RPI_NATIVE_SESSION_ID || 'p0-native-20mib';
const pressurePath = process.env.RPI_PRESSURE_SESSION_PATH || '';
const pressureCwd = process.env.RPI_PRESSURE_SESSION_CWD || nativeCwd;
const pressureId = process.env.RPI_PRESSURE_SESSION_ID || 'p0-pressure-2mib';

function fail(message) {
  console.error(`web-p0-resource: FAIL: ${message}`);
  process.exit(2);
}

async function main() {
  if (!url) fail('RPI_URL is required');
  fs.mkdirSync(evidence, { recursive: true });
  const evidencePath = path.join(evidence, 'p0-resource-evidence.json');
  const wsUrl = url.replace(/^http/, 'ws').replace(/\/web\/?$/, '/ws');
  // The listener's local TCP port feeds the listener-local-port
  // established-count fallback when the /proc fd tables are unreadable.
  const listenerPort = Number(new URL(url).port);
  const launchOptions = chromePath ? { executablePath: chromePath } : {};
  const browser = await chromium.launch(launchOptions);
  let pageA = null;
  let pageB = null;
  let pressureClient = null;
  let scoutClient = null;
  try {
    const contextA = await browser.newContext({ viewport: { width: 1280, height: 800 } });
    const contextB = await browser.newContext({ viewport: { width: 1280, height: 800 } });
    pageA = await contextA.newPage();
    pageB = await contextB.newPage();
    if (token) {
      await pageA.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
      await pageB.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
    }
    await pageA.goto(url, { waitUntil: 'domcontentloaded' });
    await pageB.goto(url, { waitUntil: 'domcontentloaded' });
    const pagesByLabel = { A: pageA, B: pageB };

    const written = await runP0ResourceContract({
      evidencePath,
      nativeSession: nativePath ? {
        filePath: nativePath,
        cwd: nativeCwd,
        sessionId: nativeId,
      } : null,
      pressureSession: pressurePath ? {
        filePath: pressurePath,
        cwd: pressureCwd,
        sessionId: pressureId,
      } : null,
      pid: rpiPid,
      page: pageA,
      pages: [pageA, pageB],
      disconnectBrowserClients: closeBrowserClients,
      clientFactory: async (label) => createControlClient({
        url: wsUrl,
        token,
        WebSocketImpl: createPageWebSocketImpl(pagesByLabel[label]),
      }),
      pressureClientFactory: async () => {
        pressureClient = createNodePressureClient({ url: wsUrl, token, WebSocketImpl: WebSocket });
        return pressureClient;
      },
      listenerPort,
      scoutClientFactory: async () => {
        scoutClient = createControlClient({
          url: wsUrl,
          token,
          WebSocketImpl: WebSocket,
        });
        return scoutClient;
      },
    });
    if (!written.evidence.evidence.ok) fail(`contract failed: ${JSON.stringify(written.evidence.evidence.failures)}`);
    console.log('web-p0-resource: PASSED');
  } finally {
    if (pressureClient) {
      await pressureClient.destroy().catch(() => {});
      pressureClient = null;
    }
    if (scoutClient) {
      await scoutClient.close(1000, 'p0-scout-disconnect').catch(() => {});
      scoutClient = null;
    }
    await closeBrowserClients({ pages: [pageA, pageB] }).catch(() => {});
    pageA = null;
    pageB = null;
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  fail(err instanceof Error ? err.stack || err.message : String(err));
});
