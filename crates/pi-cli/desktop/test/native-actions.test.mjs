import assert from 'node:assert/strict';
import { readFileSync, statSync } from 'node:fs';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';
import { EventEmitter } from 'node:events';
import { test } from 'node:test';

const require = createRequire(import.meta.url);
const directory = fileURLToPath(new URL('../', import.meta.url));
function compiled(name) {
  const output = `${directory}dist/${name}.js`;
  assert.ok(statSync(output).mtimeMs >= statSync(`${directory}src/${name}.ts`).mtimeMs,
    `Build desktop before testing ${name}.`);
  return readFileSync(output, 'utf8');
}

function host() {
  const origin = 'http://127.0.0.1:32123';
  const ipc = new EventEmitter();
  const window = new EventTarget();
  window.top = window;
  window.location = { origin };
  let bridge;
  let activation = false;
  let chooserOpened = 0;
  const received = [];
  const renderer = vm.createContext({
    exports: {}, window, Event,
    process: { isMainFrame: true, argv: [`--rpi-desktop-origin=${origin}`] },
    require: (name) => {
      assert.equal(name, 'electron');
      return {
        ipcRenderer: ipc,
        contextBridge: { exposeInMainWorld: (name, value) => {
          assert.equal(name, 'rpiDesktop');
          bridge = value;
        } },
      };
    },
  });
  vm.runInContext(compiled('preload'), renderer);
  assert.ok(bridge, 'trusted preload exposes the bridge');
  const unsubscribe = bridge.onAction(({ kind }) => {
    received.push(kind);
    // Model Chromium's documented chooser gate, not the product transport.
    if (kind === 'attach' && activation) chooserOpened += 1;
  });
  const contents = {
    isDestroyed: () => false,
    getURL: () => `${window.location.origin}/web`,
    send: (channel, payload) => ipc.emit(channel, {}, payload),
    executeJavaScript: async (script, userGesture) => {
      activation = userGesture === true;
      try { return vm.runInContext(script, renderer); }
      finally { activation = false; }
    },
  };
  const electron = {
    app: { requestSingleInstanceLock: () => false, quit() {} },
    Notification: { isSupported: () => false },
    dialog: { showMessageBox: async () => ({ response: 0 }) },
  };
  const main = vm.createContext({
    exports: {}, console, process, URL, __dirname: `${directory}dist`,
    require: (name) => name === 'electron' ? electron
      : name.startsWith('./') ? require(`${directory}dist/${name.slice(2)}.js`) : require(name),
    testWindow: { webContents: contents }, testOrigin: origin,
  });
  vm.runInContext(compiled('main') + '\nmainWindow = testWindow; config = { runnerUrl: testOrigin, route: "web", error: null, local: false }; actions = new Set(["attach", "settings"]); globalThis.invokeNativeAction = emitAction;', main);
  return { main, window, received, unsubscribe,
    chooserCount: () => chooserOpened,
    invoke: async (kind) => { main.invokeNativeAction(kind); await Promise.resolve(); },
  };
}

test('native Attach reaches the existing renderer handler with chooser activation', async () => {
  const fixture = host();
  await fixture.invoke('attach');
  assert.deepEqual(fixture.received, ['attach']);
  assert.equal(fixture.chooserCount(), 1, 'native Attach must grant activation to the existing file input handler');
  fixture.unsubscribe();
  await fixture.invoke('attach');
  assert.equal(fixture.chooserCount(), 1, 'unsubscribed handlers cannot open another chooser');
});

test('unsupported and foreign-origin native actions cannot reach the renderer', async () => {
  const fixture = host();
  await fixture.invoke('stop');
  fixture.window.location.origin = 'https://foreign.example';
  await fixture.invoke('attach');
  assert.deepEqual(fixture.received, []);
  assert.equal(fixture.chooserCount(), 0);
});

test('non-attachment actions retain their existing renderer delivery', async () => {
  const fixture = host();
  await fixture.invoke('settings');
  assert.deepEqual(fixture.received, ['settings']);
  assert.equal(fixture.chooserCount(), 0);
});
