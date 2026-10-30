import assert from 'node:assert/strict';
import { test } from 'node:test';
import { EventEmitter } from 'node:events';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { createRequire } from 'node:module';
const require = createRequire(import.meta.url);
import { runInNewContext } from 'node:vm';
const output = new URL('../dist/login.js', import.meta.url);
assert.ok(fs.statSync(output).mtimeMs >= fs.statSync(new URL('../src/login.ts', import.meta.url)).mtimeMs, 'Build desktop before login tests.');
const { DesktopLogin, LoginProtocol, trustedLoginSender, eventLinks } = require(output.pathname);
const complete = { type: 'complete', providerId: 'synthetic-local-provider', credentialType: 'api_key' };
const prompt = { type: 'prompt', id: 1, prompt: { type: 'select', message: 'Synthetic choice', options: [{ id: 'synthetic', label: 'Synthetic' }] } };
const feed = (protocol, frame) => protocol.push(Buffer.from(JSON.stringify(frame) + '\n'), () => {});

test('protocol requires exact advertised selection and increasing prompt IDs', () => {
  const protocol = new LoginProtocol();
  feed(protocol, prompt);
  assert.throws(() => protocol.response(2, 'synthetic'));
  assert.throws(() => protocol.response(1, 'not-advertised'));
  assert.equal(protocol.response(1, 'synthetic'), '{"type":"response","id":1,"value":"synthetic"}\n');
  assert.throws(() => protocol.response(1, 'synthetic'));
  assert.throws(() => feed(protocol, prompt));
  feed(protocol, { ...prompt, id: 2 });
  assert.throws(() => feed(protocol, { ...prompt, id: 3 }));
});

test('protocol rejects malformed fields, tags, UTF8 and oversize incremental frames', () => {
  for (const value of [{ ...prompt, extra: true }, { ...prompt, id: '1' }, { type: 'unknown' }, { type: 'error', code: 'SYNTHETIC_SECRET_MUST_NOT_ESCAPE' }, { ...prompt, prompt: { type: 'secret', message: 'synthetic', unexpected: true } }]) assert.throws(() => feed(new LoginProtocol(), value));
  for (const discriminator of [['secret'], {}, 17]) assert.throws(() => feed(new LoginProtocol(), { type: 'prompt', id: 1, prompt: { type: discriminator, message: 'Synthetic secret' } }));
  for (const code of [['authentication_failed'], {}, 17]) assert.throws(() => feed(new LoginProtocol(), { type: 'error', code }));
  assert.throws(() => new LoginProtocol().push(Buffer.from([0xff, 10]), () => {}));
  const bounded = new LoginProtocol();
  bounded.push(Buffer.alloc(1024 * 1024, 32), () => {});
  assert.throws(() => bounded.push(Buffer.from('x'), () => {}));
});

test('only complete plus exit0 succeeds; EOF and trailing frames fail', () => {
  const protocol = new LoginProtocol();
  feed(protocol, complete);
  assert.equal(protocol.finish(0, null), true);
  assert.throws(() => protocol.finish(1, null));
  assert.throws(() => protocol.finish(null, 'SIGTERM'));
  assert.throws(() => feed(protocol, complete));
  assert.throws(() => new LoginProtocol().finish(0, null));
  const partial = new LoginProtocol();
  partial.push(Buffer.from(JSON.stringify(complete)), () => {});
  assert.throws(() => partial.finish(0, null));
  const failure = new LoginProtocol();
  feed(failure, { type: 'error', code: 'authentication_failed' });
  assert.equal(failure.finish(1, null), false);
  assert.throws(() => failure.finish(0, null));
});

test('sender requires owned top frame and exact document; event URLs are HTTP(S) only', () => {
  const document = 'data:owned';
  const contents = { mainFrame: { url: document }, getURL: () => document };
  const window = { isDestroyed: () => false, webContents: contents };
  const ipc = { sender: contents, senderFrame: contents.mainFrame };
  assert.equal(trustedLoginSender(ipc, window, document), true);
  assert.equal(trustedLoginSender({ ...ipc, sender: {} }, window, document), false);
  assert.equal(trustedLoginSender({ ...ipc, senderFrame: { url: document } }, window, document), false);
  contents.mainFrame.url = 'https://remote.invalid';
  assert.equal(trustedLoginSender(ipc, window, document), false);
  assert.deepEqual(eventLinks({ type: 'info', links: [{ url: 'file:///synthetic' }, { url: 'javascript:synthetic' }, { url: 'https://example.invalid/auth' }] }), ['https://example.invalid/auth']);
});

function electronFixture() {
  const ipcMain = new EventEmitter();
  const windows = [];
  const opened = [];
  class Window extends EventEmitter {
    constructor(options) {
      super(); this.options = options; this.destroyed = false; this.url = '';
      this.webContents = new EventEmitter();
      this.webContents.mainFrame = { url: '' };
      this.webContents.getURL = () => this.url;
      this.webContents.send = (channel, state) => this.emit('state', channel, state);
      this.webContents.setWindowOpenHandler = (handler) => { this.openHandler = handler; };
      windows.push(this);
    }
    isDestroyed() { return this.destroyed; }
    setMenu(menu) { this.menu = menu; }
    destroy() { if (!this.destroyed) { this.destroyed = true; this.emit('closed'); } }
    async loadURL(url) { this.url = url; this.webContents.mainFrame.url = url; }
    command(value, overrides = {}) {
      ipcMain.emit('rpi-login:command', { sender: this.webContents, senderFrame: this.webContents.mainFrame, ...overrides }, { attempt: this.options.webPreferences.additionalArguments[0].split('=')[1], ...value });
    }
  }
  return { ipcMain, BrowserWindow: Window, shell: { openExternal: async (url) => { opened.push(url); } }, windows, opened };
}
function fixture(t, source) {
  const cwd = fs.mkdtempSync(path.join(os.tmpdir(), 'rpi-login-synthetic-'));
  const binary = path.join(cwd, 'rpi');
  fs.writeFileSync(binary, `#!${process.execPath}\n${source}`, { mode: 0o700 });
  t.after(() => fs.rmSync(cwd, { recursive: true, force: true }));
  return { cwd, binary };
}

test('owner is single-flight, denies navigation, observes cancellation outside a prompt', async (t) => {
  const { cwd, binary } = fixture(t, `process.stdout.write(JSON.stringify({type:'event',event:{type:'progress',message:'Synthetic polling'}})+'\\n'); process.stdin.on('data',()=>process.exit(1)); setInterval(()=>{},1000);`);
  const electron = electronFixture();
  const owner = new DesktopLogin(electron);
  const operation = owner.start(binary, cwd, {});
  t.after(() => owner.cancel());
  assert.equal(owner.start(binary, cwd, {}), operation);
  const window = electron.windows[0];
  assert.equal(electron.windows.length, 1);
  assert.equal(window.options.webPreferences.sandbox, true);
  assert.equal(window.options.webPreferences.contextIsolation, true);
  assert.equal(window.options.webPreferences.nodeIntegration, false);
  for (const name of ['will-navigate', 'will-redirect', 'will-frame-navigate', 'will-attach-webview']) {
    let blocked = false;
    window.webContents.emit(name, { preventDefault() { blocked = true; } });
    assert.equal(blocked, true);
  }
  assert.deepEqual(window.openHandler({ url: 'https://example.invalid' }), { action: 'deny' });
  const polling = new Promise((resolve) => window.once('state', resolve));
  window.command({ type: 'ready' });
  await polling;
  window.command({ type: 'cancel' }, { sender: {} });
  assert.equal(window.destroyed, false);
  await owner.cancel();
  assert.equal(await operation, false);
  assert.equal(window.destroyed, true);
  assert.equal(electron.ipcMain.listenerCount('rpi-login:command'), 0);
});

test('owner requires complete and successful exit and never forwards stderr', async (t) => {
  for (const code of [0, 1]) {
    const { cwd, binary } = fixture(t, `process.stderr.write('SYNTHETIC_SECRET_MUST_NOT_ESCAPE'); process.stdout.write(${JSON.stringify(JSON.stringify(complete) + '\n')},()=>process.exit(${code}));`);
    const electron = electronFixture();
    const owner = new DesktopLogin(electron);
    const operation = owner.start(binary, cwd, {});
    const states = [];
    electron.windows[0].on('state', (_channel, state) => states.push(state));
    electron.windows[0].command({ type: 'ready' });
    assert.equal(await operation, code === 0);
    assert.equal(JSON.stringify(states).includes('SYNTHETIC_SECRET_MUST_NOT_ESCAPE'), false);
    await owner.cancel();
  }
});

test('current event reference is required and arbitrary URL commands cannot open browsers', async (t) => {
  const { cwd, binary } = fixture(t, `process.stdout.write(JSON.stringify({type:'event',event:{type:'auth_url',url:'https://example.invalid/synthetic'}})+'\\n'); process.stdin.on('data',()=>process.exit(1)); setInterval(()=>{},1000);`);
  const electron = electronFixture();
  const owner = new DesktopLogin(electron);
  const operation = owner.start(binary, cwd, {});
  t.after(() => owner.cancel());
  const window = electron.windows[0];
  const statePromise = new Promise((resolve) => window.once('state', (_channel, state) => resolve(state)));
  window.command({ type: 'ready' });
  const state = await statePromise;
  window.command({ type: 'open', index: 0, eventId: state.eventId }, { senderFrame: { url: window.url } });
  window.command({ type: 'open', index: 0, eventId: state.eventId, attempt: 'wrong-attempt' });
  assert.deepEqual(electron.opened, []);
  window.command({ type: 'open', index: 0, eventId: state.eventId });
  assert.deepEqual(electron.opened, ['https://example.invalid/synthetic']);
  window.command({ type: 'open', index: 0, eventId: state.eventId - 1 });
  assert.equal(await operation, false);
  assert.equal(electron.opened.length, 1);
});

test('actual preload clears password before submit and exposes no remote-page bridge', () => {
  const preload = new URL('../dist/login-preload.js', import.meta.url);
  assert.ok(fs.statSync(preload).mtimeMs >= fs.statSync(new URL('../src/login-preload.ts', import.meta.url)).mtimeMs, 'Build desktop before preload tests.');
  class Element {
    constructor(tag) { this.tag = tag; this.children = []; this.value = ''; this.textContent = ''; }
    append(...children) { this.children.push(...children); }
    replaceChildren(...children) { this.children = children; }
    setAttribute(name, value) { this[name] = value; }
    focus() {}
  }
  class Select extends Element {}
  const root = new Element('main');
  const listeners = {};
  const sent = [];
  let receive;
  const ipcRenderer = { send: (...args) => sent.push(args), on: (_channel, callback) => { receive = callback; } };
  const window = { addEventListener: (name, callback) => { listeners[name] = callback; } };
  const body = new Element('body');
  runInNewContext(fs.readFileSync(preload, 'utf8'), { require: () => ({ ipcRenderer }), exports: {}, process: { argv: ['--rpi-login-attempt=synthetic-attempt'], isMainFrame: true }, window, document: { body, getElementById: () => root, createElement: (tag) => tag === 'select' ? new Select(tag) : new Element(tag) }, HTMLSelectElement: Select, URL });
  listeners.DOMContentLoaded();
  assert.equal(window.rpiLogin, undefined);
  receive({}, { attempt: 'synthetic-attempt', frame: { type: 'prompt', id: 1, prompt: { type: 'secret', message: '<img src=synthetic>' } } });
  const form = root.children[1];
  const label = form.children[0];
  assert.equal(label.textContent, '<img src=synthetic>');
  const password = label.children[0];
  assert.equal(password.type, 'password');
  password.value = 'SYNTHETIC_LOCAL_TEST_SECRET';
  form.onsubmit({ preventDefault() {} });
  assert.equal(password.value, '');
  assert.equal(form.children.length, 0);
  assert.equal(sent.at(-1)[1].value, 'SYNTHETIC_LOCAL_TEST_SECRET');
  listeners.pagehide();
  assert.equal(form.children.length, 0);
  receive({}, { attempt: 'synthetic-attempt', frame: { type: 'prompt', id: 2, prompt: { type: 'secret', message: 'Synthetic secret' } } });
  const closingPassword = form.children[0].children[0];
  closingPassword.value = 'SYNTHETIC_LOCAL_TEST_SECRET';
  const close = body.children.find((element) => element['aria-label'] === 'Close sign in');
  close.onclick();
  assert.equal(closingPassword.value, '');
  assert.equal(form.children.length, 0);
  assert.equal(sent.at(-1)[1].type, 'cancel');
});

test('cancel escalates for an owned child ignoring stdin and observes exit', async (t) => {
  const { cwd, binary } = fixture(t, `process.stdin.resume(); process.stdout.write(JSON.stringify({type:'event',event:{type:'progress',message:'Synthetic callback wait'}})+'\\n'); setInterval(()=>{},1000);`);
  const electron = electronFixture();
  const owner = new DesktopLogin(electron);
  const operation = owner.start(binary, cwd, {});
  t.after(() => owner.cancel());
  const window = electron.windows[0];
  const ready = new Promise((resolve) => window.once('state', resolve));
  window.command({ type: 'ready' });
  await ready;
  await owner.cancel();
  assert.equal(await operation, false);
  assert.equal(electron.ipcMain.listenerCount('rpi-login:command'), 0);
});

test('UI cancellation reports missing close after kill while retaining child ownership', async (t) => {
  t.mock.timers.enable({ apis: ['setTimeout'] });
  const electron = electronFixture();
  const failures = [];
  electron.dialog = { showErrorBox: (title, message) => failures.push({ title, message }) };
  const child = new EventEmitter();
  child.pid = 123;
  child.stdin = new EventEmitter();
  child.stdin.writable = true;
  const commands = [];
  child.stdin.end = (command) => commands.push(command);
  child.stdout = new EventEmitter();
  child.stderr = { resume() {} };
  const signals = [];
  child.kill = (signal) => { signals.push(signal); return true; };
  let spawns = 0;
  const owner = new DesktopLogin(electron, () => { spawns++; return child; });
  const operation = owner.start('/synthetic/rpi', '/synthetic', {});
  let resolved = false;
  void operation.then(() => { resolved = true; });
  electron.windows[0].command({ type: 'cancel' });
  assert.deepEqual(commands, ['{"type":"cancel"}\n']);
  t.mock.timers.tick(1500);
  assert.deepEqual(signals, ['SIGKILL']);
  assert.deepEqual(failures, []);
  t.mock.timers.tick(2500);
  assert.equal(failures.length, 1);
  assert.equal(failures[0].title, 'Sign in shutdown failed');
  await Promise.resolve();
  assert.equal(resolved, false);
  assert.equal(owner.start('/synthetic/rpi', '/synthetic', {}), operation);
  assert.equal(spawns, 1);
  child.emit('close', null, 'SIGKILL');
  assert.equal(await operation, false);
  assert.equal(electron.ipcMain.listenerCount('rpi-login:command'), 0);
  await owner.cancel();
});
