import { spawn, type ChildProcessWithoutNullStreams } from 'node:child_process';
import * as path from 'node:path';
import { randomUUID } from 'node:crypto';
import type { BrowserWindow, IpcMainEvent } from 'electron';
import type * as ElectronModule from 'electron';

const LIMIT = 1024 * 1024;
const CHANNEL = 'rpi-login:command';
type RecordValue = Record<string, unknown>;
export type LoginFrame = RecordValue & { type: string };
function object(value: unknown): value is RecordValue { return value !== null && typeof value === 'object' && !Array.isArray(value); }
function shape(value: unknown, required: string[], optional: string[] = []): value is RecordValue {
  return object(value) && required.every((key) => Object.hasOwn(value, key)) && Object.keys(value).every((key) => required.includes(key) || optional.includes(key));
}
function strings(value: RecordValue, keys: string[]): boolean { return keys.every((key) => typeof value[key] === 'string'); }
function optionalStrings(value: RecordValue, keys: string[]): boolean { return keys.every((key) => value[key] === undefined || typeof value[key] === 'string'); }
function prompt(value: unknown): boolean {
  if (!object(value)) return false;
  if (value.type === 'select') return shape(value, ['type', 'message', 'options']) && typeof value.message === 'string' && Array.isArray(value.options)
    && value.options.length > 0 && value.options.every((option) => shape(option, ['id', 'label'], ['description']) && strings(option, ['id', 'label']) && optionalStrings(option, ['description']))
    && new Set(value.options.map((option) => option.id)).size === value.options.length;
  return typeof value.type === 'string' && ['text', 'secret', 'manual_code'].includes(value.type) && shape(value, ['type', 'message'], ['placeholder']) && typeof value.message === 'string' && optionalStrings(value, ['placeholder']);
}
function event(value: unknown): boolean {
  if (!object(value)) return false;
  switch (value.type) {
    case 'progress': return shape(value, ['type', 'message']) && strings(value, ['message']);
    case 'info': return shape(value, ['type', 'message'], ['links']) && strings(value, ['message']) && (value.links === undefined || Array.isArray(value.links) && value.links.every((link) => shape(link, ['url'], ['label']) && strings(link, ['url']) && optionalStrings(link, ['label'])));
    case 'auth_url': return shape(value, ['type', 'url'], ['instructions']) && strings(value, ['url']) && optionalStrings(value, ['instructions']);
    case 'device_code': return shape(value, ['type', 'user_code', 'verification_uri'], ['interval_seconds', 'expires_in_seconds']) && strings(value, ['user_code', 'verification_uri']) && ['interval_seconds', 'expires_in_seconds'].every((key) => value[key] === undefined || typeof value[key] === 'number' && Number.isFinite(value[key]));
    default: return false;
  }
}
export function eventLinks(value: RecordValue | undefined): string[] {
  const urls = !value ? [] : value.type === 'auth_url' ? [value.url] : value.type === 'device_code' ? [value.verification_uri] : value.type === 'info' && Array.isArray(value.links) ? value.links.map((link) => link.url) : [];
  return urls.filter((url): url is string => { try { return typeof url === 'string' && ['http:', 'https:'].includes(new URL(url).protocol); } catch { return false; } });
}

export class LoginProtocol {
  private buffer = Buffer.alloc(LIMIT);
  private length = 0;
  private nextId = 1;
  outstanding: LoginFrame | undefined;
  currentEvent: RecordValue | undefined;
  eventId = 0;
  terminal: 'complete' | 'error' | undefined;
  push(chunk: Buffer, receive: (frame: LoginFrame) => void): void {
    for (let offset = 0; offset < chunk.length;) {
      const newline = chunk.indexOf(10, offset);
      const end = newline < 0 ? chunk.length : newline;
      const count = end - offset;
      if (this.length + count > LIMIT) throw new Error('invalid_input');
      chunk.copy(this.buffer, this.length, offset, end);
      this.length += count;
      if (newline < 0) return;
      const raw = new TextDecoder('utf-8', { fatal: true }).decode(this.buffer.subarray(0, this.length));
      this.length = 0;
      let frame: unknown;
      try { frame = JSON.parse(raw); } catch { throw new Error('invalid_input'); }
      if (this.terminal || !object(frame)) throw new Error('invalid_input');
      switch (frame.type) {
        case 'prompt':
          if (!shape(frame, ['type', 'id', 'prompt']) || frame.id !== this.nextId || this.outstanding || !prompt(frame.prompt)) throw new Error('invalid_input');
          this.nextId++;
          this.outstanding = frame as LoginFrame;
          break;
        case 'event':
          if (!shape(frame, ['type', 'event']) || !event(frame.event)) throw new Error('invalid_input');
          this.currentEvent = frame.event as RecordValue;
          this.eventId++;
          break;
        case 'complete':
          if (!shape(frame, ['type', 'providerId', 'credentialType']) || !strings(frame, ['providerId', 'credentialType']) || this.outstanding) throw new Error('invalid_input');
          this.terminal = 'complete';
          break;
        case 'error':
          if (!shape(frame, ['type', 'code']) || typeof frame.code !== 'string' || !['cancelled', 'invalid_input', 'io', 'authentication_failed'].includes(frame.code)) throw new Error('invalid_input');
          this.terminal = 'error';
          break;
        default: throw new Error('invalid_input');
      }
      receive(frame as LoginFrame);
      offset = end + 1;
    }
  }
  response(id: unknown, value: unknown): string {
    const pending = this.outstanding;
    if (!pending || pending.id !== id || typeof value !== 'string') throw new Error('invalid_input');
    const offered = pending.prompt as RecordValue;
    if (offered.type === 'select' && !(offered.options as RecordValue[]).some((option) => option.id === value)) throw new Error('invalid_input');
    const line = JSON.stringify({ type: 'response', id, value }) + '\n';
    if (Buffer.byteLength(line) - 1 > LIMIT) throw new Error('invalid_input');
    this.outstanding = undefined;
    return line;
  }
  finish(code: number | null, signal: string | null): boolean {
    if (this.length || !this.terminal || signal || (this.terminal === 'complete' ? code !== 0 : code === 0)) throw new Error('invalid_input');
    return this.terminal === 'complete';
  }
}

export function trustedLoginSender(event: IpcMainEvent, window: BrowserWindow, document: string): boolean {
  return !window.isDestroyed() && event.sender === window.webContents && event.senderFrame === window.webContents.mainFrame && event.senderFrame?.url === document && window.webContents.getURL() === document;
}

export class DesktopLogin {
  private operation: Promise<boolean> | undefined;
  private stop: (() => void) | undefined;
  constructor(private readonly electron: typeof ElectronModule = require('electron'), private readonly spawnProcess: typeof spawn = spawn) {}
  start(binaryPath: string, cwd: string, parent: BrowserWindow): Promise<boolean> {
    if (this.operation) return this.operation;
    const operation = this.run(binaryPath, cwd, parent).catch(() => false);
    this.operation = operation;
    void operation.finally(() => { if (this.operation === operation) this.operation = undefined; });
    return operation;
  }
  async cancel(): Promise<void> {
    this.stop?.();
    let timer: NodeJS.Timeout | undefined;
    try {
      await Promise.race([this.operation, new Promise<never>((_resolve, reject) => {
        timer = setTimeout(() => reject(new Error('Login process did not exit after cancellation.')), 4000);
      })]);
    } finally { clearTimeout(timer); }
  }
  private run(binaryPath: string, cwd: string, parent: BrowserWindow): Promise<boolean> {
    return new Promise((resolve) => {
      const { BrowserWindow, ipcMain, shell } = this.electron;
      const attempt = randomUUID();
      const document = 'data:text/html;charset=utf-8,' + encodeURIComponent(`<!doctype html><meta charset="utf-8"><meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'"><title>Sign in / configure provider</title><style>
        *{box-sizing:border-box}body{font:14px/1.5 system-ui,sans-serif;margin:0;padding:24px;background:#16181d;color:#e8e8e8;color-scheme:dark}
        h1{font-size:17px;margin:0 44px 14px 0}p{margin:8px 0 20px;color:#b7bdc8}main{display:grid;grid-template-columns:1fr auto;gap:16px 8px}section{grid-column:1/-1}section:empty{display:none}
        form{display:contents}label{grid-column:1/-1;grid-row:2;display:grid;gap:8px}input,select{width:100%;min-width:0;height:40px;padding:8px 10px;background:#22252c}
        input,select,button{font:14px system-ui,sans-serif;color:#e8e8e8;border:1px solid #3c414b;border-radius:0}button{min-height:40px;padding:8px 16px;background:#2b303a;cursor:pointer}button:hover{background:#363d49}button:focus-visible,input:focus-visible,select:focus-visible{outline:2px solid #91b9ff;outline-offset:2px}
        button[type=submit]{grid-column:2;grid-row:3;min-width:112px}#login>button{grid-column:1;grid-row:3;justify-self:start}section button{width:100%;margin-top:8px;text-align:left;overflow-wrap:anywhere}.auth-close{position:absolute;right:16px;top:16px;width:40px;height:40px;padding:0;background:transparent;border-color:transparent;font-size:22px}
      </style><h1>Sign in / configure provider</h1><p>Credentials are saved by rpi. Saving credentials does not verify provider access.</p><main id="login"></main>`);
      const window = new BrowserWindow({ parent, modal: true, title: 'Sign in / configure provider', width: 620, height: 560, backgroundColor: '#16181d', webPreferences: { preload: path.join(__dirname, 'login-preload.js'), sandbox: true, contextIsolation: true, nodeIntegration: false, webSecurity: true, additionalArguments: [`--rpi-login-attempt=${attempt}`] } });
      window.setMenu(null);
      const protocol = new LoginProtocol();
      let child: ChildProcessWithoutNullStreams | undefined;
      let cancelled = false;
      let settled = false;
      let ready = false;
      let killTimer: NodeJS.Timeout | undefined;
      let shutdownTimer: NodeJS.Timeout | undefined;
      const send = (frame: LoginFrame) => { if (ready && !window.isDestroyed()) window.webContents.send('rpi-login:state', { attempt, frame, eventId: protocol.eventId }); };
      const finish = (success: boolean) => {
        if (settled) return;
        settled = true;
        clearTimeout(killTimer);
        clearTimeout(shutdownTimer);
        ipcMain.removeListener(CHANNEL, command);
        this.stop = undefined;
        if (!window.isDestroyed()) window.destroy();
        resolve(success && !cancelled);
      };
      const cancel = () => {
        if (cancelled || settled) return;
        cancelled = true;
        if (!window.isDestroyed()) window.destroy();
        if (!child) { finish(false); return; }
        if (child.stdin.writable) child.stdin.end('{"type":"cancel"}\n');
        killTimer = setTimeout(() => { child?.kill('SIGKILL'); }, 1500);
        shutdownTimer = setTimeout(() => {
          if (!settled) this.electron.dialog.showErrorBox('Sign in shutdown failed', 'The login process has not exited after cancellation. No new sign-in attempt can start until it exits. Quit again to retry shutdown.');
        }, 4000);
      };
      this.stop = cancel;
      const command = (ipc: IpcMainEvent, value: unknown) => {
        if (cancelled || settled || !trustedLoginSender(ipc, window, document) || !object(value) || value.attempt !== attempt) return;
        try {
          if (shape(value, ['attempt', 'type']) && value.type === 'ready') {
            ready = true;
            if (protocol.currentEvent) send({ type: 'event', event: protocol.currentEvent });
            if (protocol.outstanding) send(protocol.outstanding);
          } else if (shape(value, ['attempt', 'type']) && value.type === 'cancel') cancel();
          else if (shape(value, ['attempt', 'type', 'id', 'value']) && value.type === 'response') {
            const line = protocol.response(value.id, value.value);
            if (!child?.stdin.writable) throw new Error('io');
            child.stdin.write(line, (error) => { if (error) cancel(); });
          } else if (shape(value, ['attempt', 'type', 'index', 'eventId']) && value.type === 'open' && Number.isSafeInteger(value.index) && value.eventId === protocol.eventId && !protocol.terminal) {
            const url = eventLinks(protocol.currentEvent)[value.index as number];
            if (!url) throw new Error('invalid_input');
            void shell.openExternal(url).catch(() => cancel());
          } else cancel();
        } catch { cancel(); }
      };
      ipcMain.on(CHANNEL, command);
      window.on('closed', cancel);
      window.webContents.on('will-navigate', (event) => event.preventDefault());
      window.webContents.on('will-redirect', (event) => event.preventDefault());
      window.webContents.on('will-frame-navigate', (event) => event.preventDefault());
      window.webContents.on('will-attach-webview', (event) => event.preventDefault());
      window.webContents.setWindowOpenHandler(() => ({ action: 'deny' }));
      window.webContents.on('render-process-gone', cancel);
      void window.loadURL(document).catch(cancel);
      try {
        child = this.spawnProcess(binaryPath, ['login', '--json'], { cwd, stdio: ['pipe', 'pipe', 'pipe'] });
        child.stderr.resume();
        child.stdin.on('error', cancel);
        child.stdout.on('error', cancel);
        child.on('error', () => { if (!child?.pid) finish(false); else cancel(); });
        child.stdout.on('data', (chunk: Buffer) => { if (cancelled) return; try { protocol.push(chunk, send); } catch { cancel(); } });
        child.on('close', (code, signal) => { try { finish(!cancelled && protocol.finish(code, signal)); } catch { finish(false); } });
      } catch { finish(false); }
    });
  }
}
