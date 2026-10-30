import { app, BrowserWindow, Menu, Notification, dialog, ipcMain, screen, session, shell } from 'electron';
import * as path from 'node:path';
import { accessSync, constants } from 'node:fs';
import { createServer } from 'node:net';
import { DesktopServer } from './server';
import { DesktopLogin } from './login';
import {
  DEFAULT_RUNNER_URL,
  MIN_WINDOW_SIZE,
  PIN_FILE_NAME,
  PROMPT_SUBMIT_SCRIPT,
  ROUTES,
  SETTINGS_FILE_NAME,
  WINDOW_STATE_FILE_NAME,
  applyContentSecurityPolicy,
  buildRouteUrl,
  nextRunnerConfig,
  runnerMenuEnabled,
  certificateDecision,
  clampWindowBounds,
  contentSecurityPolicyFor,
  errorMessage,
  errorPageHtml,
  externalSchemeAllowed,
  isDocumentResource,
  navigationAllowed,
  normalizeRunnerUrl,
  permissionAllowed,
  promptPageHtml,
  readPinSession,
  readSettings,
  readWindowState,
  resolveRunnerConfig,
  sameOrigin,
  sha256FingerprintFromPem,
  writePinSession,
  writeSettings,
  writeWindowState,
  type PinSession,
  type PinStore,
  type Route,
  type RunnerConfig,
} from './helpers';

const ERR_ABORTED = -3;

interface UntrustedCertificate {
  origin: string;
  hostname: string;
  fingerprint: string;
  reason: string;
}

type RunnerUrlPrompt =
  | { status: 'submitted'; value: string }
  | { status: 'cancelled' }
  | { status: 'failed'; error: string };

let mainWindow: BrowserWindow | null = null;
let config: RunnerConfig = { runnerUrl: DEFAULT_RUNNER_URL, route: 'web', error: null, local: true };
let pinSession: PinSession = { store: {}, unreadable: null, announcedUnreadable: false };
let pinsLoaded = false;
let pendingCertificates: UntrustedCertificate[] = [];
let certificatePromptActive = false;
let externalPromptQueue: Promise<void> = Promise.resolve();
const actionKinds = ['new-workspace', 'new-session', 'settings', 'attach', 'stop'] as const;
type ActionKind = typeof actionKinds[number];
let actions = new Set<ActionKind>();
let desktopServer: DesktopServer | undefined;
let starting: Promise<void> | undefined;
let quitting = false;
let quitReady = false;
const desktopLogin = new DesktopLogin();
let loginGeneration = 0;
let loginStarting: Promise<void> | undefined;

function configureProvider(): Promise<void> {
  if (loginStarting) return loginStarting;
  if (!config.local || quitting || !mainWindow) return Promise.resolve();
  const generation = ++loginGeneration;
  const operation = (async () => {
    await starting;
    if (generation !== loginGeneration || !config.local || quitting || !mainWindow) return;
    const binary = runnerBinary();
    const success = await desktopLogin.start(binary, binary === bundledRunnerPath() ? app.getPath('home') : process.cwd(), mainWindow);
    if (generation !== loginGeneration || !config.local || quitting) return;
    if (success) {
      if (desktopServer) await desktopServer.stop();
      if (generation === loginGeneration && config.local && !quitting) await startRunner();
    } else await showMessage(mainWindow, { type: 'info', title: 'Sign in / configure provider', message: 'Sign in was cancelled or could not complete. No provider access has been verified.', buttons: ['Close'] });
  })().catch(() => {
    if (generation === loginGeneration && !quitting) reportOneLine('Sign in / configure provider', 'Sign in could not complete. Retry from the Runner menu.');
  });
  loginStarting = operation;
  void operation.then(() => { if (loginStarting === operation) loginStarting = undefined; });
  return operation;
}

function choosePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const reservation = createServer();
    reservation.once('error', reject);
    reservation.listen({ host: '127.0.0.1', port: 0, exclusive: true }, () => {
      const address = reservation.address();
      reservation.close((error) => {
        if (error) reject(error);
        else if (address && typeof address !== 'string') resolve(address.port);
        else reject(new Error('Could not select a loopback port.'));
      });
    });
  });
}

function bundledRunnerPath(): string {
  return path.join(process.resourcesPath, process.platform === 'win32' ? 'rpi.exe' : 'rpi');
}

function runnerBinary(): string {
  const bundled = bundledRunnerPath();
  const fileName = process.platform === 'win32' ? 'rpi.exe' : 'rpi';
  const candidates = app.isPackaged ? [bundled] : [bundled,
    ...['release-dist', 'release'].map((profile) => path.resolve(__dirname, '../../../../target', profile, fileName))];
  for (const candidate of candidates) {
    try { accessSync(candidate, constants.X_OK); return candidate; }
    catch (failure) {
      if ((failure as NodeJS.ErrnoException).code !== 'ENOENT') throw failure;
    }
  }
  throw new Error(`Release rpi executable is missing. Build target/release-dist/${fileName} or target/release/${fileName}, or reinstall the desktop package.`);
}

function emitAction(kind: ActionKind): void {
  const contents = mainWindow?.webContents;
  if (contents && !contents.isDestroyed() && actions.has(kind) && sameOrigin(contents.getURL(), config.runnerUrl)) {
    if (kind === 'attach') {
      void contents.executeJavaScript(
        `if (window.top === window && window.location.origin === ${JSON.stringify(config.runnerUrl)}) window.dispatchEvent(new Event('rpi-desktop:attach'));`,
        true,
      ).catch((failure: unknown) => {
        reportOneLine('Attach files failed', errorMessage(failure));
      });
    } else {
      contents.send('rpi-desktop:action', { kind });
    }
  }
}

function startRunner(): Promise<void> {
  if (starting) return starting;
  if (quitting) return Promise.resolve();
  const operation = (async () => {
    actions.clear();
    buildMenu();
    const window = mainWindow ?? createWindow(false);
    mainWindow = window;
    try {
      await window.loadURL('data:text/html;charset=utf-8,' + encodeURIComponent('<!doctype html><title>Starting rpi</title><body style="background:#181818;color:#eee;font:18px system-ui;padding:48px"><h1>Starting rpi…</h1><p>Waiting for the runner. Startup failures can be retried from Runner → Retry connection.</p>'));
      if (config.error) throw new Error(config.error);
      if (!config.local && desktopServer) {
        await desktopServer.stop();
        desktopServer = undefined;
      }
      if (quitting) return;
      if (config.local) {
        if (!desktopServer) {
          const binaryPath = runnerBinary();
          const port = await choosePort();
          if (quitting) return;
          desktopServer = new DesktopServer({ binaryPath,
            cwd: binaryPath === bundledRunnerPath() ? app.getPath('home') : process.cwd(), port });
        }
        config.runnerUrl = await desktopServer.start();
      }
      if (quitting) return;
      mainWindow = createWindow();
      if (!window.isDestroyed()) window.close();
    } catch (failure) {
      if (quitting || window.isDestroyed()) return;
      const page = errorPageHtml({ failingUrl: targetUrl(), description: errorMessage(failure), errorCode: 0, runnerUrl: config.runnerUrl, route: config.route });
      await window.loadURL(`data:text/html;charset=utf-8,${encodeURIComponent(page)}`);
      const response = await showMessage(window, { type: 'error', title: 'Runner startup failed', message: errorMessage(failure), detail: 'Retry with Runner → Retry connection, configure a provider for the managed local runner, or set a remote runner URL.', buttons: config.local ? ['Close', 'Sign in / configure provider…'] : ['Close'], defaultId: 0, cancelId: 0 });
      if (response === 1 && config.local) setImmediate(() => { void configureProvider(); });
    }
  })();
  starting = operation;
  void operation.then(() => { starting = undefined; }, (failure) => {
    starting = undefined;
    reportOneLine('Runner startup failed', errorMessage(failure));
  });
  return operation;
}

function userDataFile(name: string): string {
  return path.join(app.getPath('userData'), name);
}

function currentPins(): PinStore {
  if (!pinsLoaded) {
    pinsLoaded = true;
    const read = readPinSession(userDataFile(PIN_FILE_NAME), pinSession);
    pinSession = read.session;
    if (read.notice !== null) reportOneLine('Certificate pins unreadable', read.notice);
  }
  return pinSession.store;
}

function trustCertificate(origin: string, fingerprint: string): boolean {
  const attempt = writePinSession(userDataFile(PIN_FILE_NAME), pinSession, origin, fingerprint, Date.now());
  pinSession = attempt.session;
  pinsLoaded = true;
  if (attempt.notice !== null) reportOneLine('Certificate pins unreadable', attempt.notice);
  if (!attempt.persisted) {
    reportOneLine('Certificate trust not saved', `${origin} was not pinned: ${attempt.error}`);
    return false;
  }
  return true;
}

function targetUrl(): string {
  return buildRouteUrl(config.runnerUrl, config.route);
}

function loadUrl(window: BrowserWindow, url: string): void {
  window.loadURL(url).catch((failure: unknown) => {
    console.error(`rpi desktop: could not load ${url}: ${errorMessage(failure)}`);
  });
}

async function applyRunnerSettings(window: BrowserWindow, next: { runnerUrl?: string; route?: Route; local?: boolean }): Promise<void> {
  if (starting || quitting) return;
  if (next.runnerUrl !== undefined || next.local === true) {
    ++loginGeneration;
    try { await desktopLogin.cancel(); }
    catch { reportOneLine('Runner setting not changed', 'The login process has not exited. Retry after it stops.'); return; }
    if (quitting) return;
  }
  config = nextRunnerConfig(config, next);
  try {
    writeSettings(userDataFile(SETTINGS_FILE_NAME), {
      ...(config.error !== null ? readSettings(userDataFile(SETTINGS_FILE_NAME))
        : config.local ? {} : { runnerUrl: config.runnerUrl }),
      route: config.route,
    });
  } catch (failure) {
    reportOneLine('Runner setting not saved', `Applied for this session only: ${errorMessage(failure)}`);
  }
  if (next.runnerUrl !== undefined || next.local === true) void startRunner();
  else if (config.error === null && sameOrigin(window.webContents.getURL(), config.runnerUrl)) loadUrl(window, targetUrl());
  else void startRunner();
}

function showMessage(window: BrowserWindow | null, options: Electron.MessageBoxOptions): Promise<number> {
  const pending =
    window !== null && !window.isDestroyed()
      ? dialog.showMessageBox(window, options)
      : dialog.showMessageBox(options);
  return pending.then((result) => result.response);
}

function reportOneLine(title: string, body: string): void {
  if (Notification.isSupported()) {
    new Notification({ title, body }).show();
    return;
  }
  void showMessage(mainWindow, { type: 'info', title, message: body, buttons: ['Close'] });
}

function hostnameOf(url: string): string {
  try {
    return new URL(url).hostname;
  } catch {
    return '';
  }
}

function originOf(url: string): string {
  try {
    return new URL(url).origin;
  } catch {
    return '';
  }
}

function queuePendingCertificate(pending: UntrustedCertificate): void {
  const duplicate = pendingCertificates.some(
    (entry) => entry.origin === pending.origin && entry.fingerprint === pending.fingerprint,
  );
  if (!duplicate) pendingCertificates.push(pending);
  void drainPendingCertificates();
}

async function drainPendingCertificates(): Promise<void> {
  if (certificatePromptActive) return;
  certificatePromptActive = true;
  try {
    while (pendingCertificates.length > 0) {
      const pending = pendingCertificates[0];
      const accepted = await confirmUntrustedCertificate(mainWindow, pending);
      pendingCertificates.shift();
      if (!accepted) continue;
      if (!trustCertificate(pending.origin, pending.fingerprint)) continue;
      const window = mainWindow;
      if (window !== null && !window.isDestroyed()) loadUrl(window, targetUrl());
    }
  } finally {
    certificatePromptActive = false;
  }
}

async function confirmUntrustedCertificate(
  window: BrowserWindow | null,
  pending: UntrustedCertificate,
): Promise<boolean> {
  if (pending.origin === '' || pending.hostname === '' || pending.fingerprint === '') {
    await showMessage(window, {
      type: 'error',
      title: 'Untrusted certificate',
      message: `The connection to ${config.runnerUrl} presented a certificate that could not be pinned.`,
      detail: pending.reason,
      buttons: ['Close'],
    });
    return false;
  }
  const response = await showMessage(window, {
    type: 'warning',
    title: 'Untrusted certificate',
    message: `The runner at ${pending.origin} presented an untrusted certificate.`,
    detail: `Reason: ${pending.reason}\nSHA-256 fingerprint: ${pending.fingerprint}\n\nTrusting remembers this certificate for ${pending.origin} (scheme, host and port). It is a remembered exception, not identity pinning: a replacement certificate is only challenged where Chromium itself rejects it.`,
    buttons: ['Trust and reload', 'Cancel'],
    defaultId: 1,
    cancelId: 1,
    noLink: true,
  });
  if (response === 0) return true;
  await showMessage(window, {
    type: 'warning',
    title: 'Certificate rejected',
    message: `Not trusting ${pending.origin}: the connection was closed.`,
    buttons: ['Close'],
  });
  return false;
}

async function trustCurrentCertificate(): Promise<void> {
  if (pendingCertificates.length > 0) {
    await drainPendingCertificates();
    return;
  }
  await showMessage(mainWindow, {
    type: 'info',
    title: 'Trust current certificate',
    message: 'No untrusted certificate is pending.',
    detail: `The runner at ${config.runnerUrl} is not waiting on a certificate decision. Reload /${config.route} to challenge it again.`,
    buttons: ['Close'],
  });
}

async function requestRunnerUrl(parent: BrowserWindow, current: string): Promise<RunnerUrlPrompt> {
  const promptWindow = new BrowserWindow({
    parent,
    modal: true,
    title: 'Set runner URL',
    width: 620,
    height: 190,
    resizable: false,
    minimizable: false,
    maximizable: false,
    webPreferences: { contextIsolation: true, nodeIntegration: false, sandbox: true, webSecurity: true },
  });
  promptWindow.removeMenu();
  hardenWebContents(promptWindow.webContents);
  let dismissed = false;
  promptWindow.once('close', () => {
    dismissed = true;
  });
  try {
    const page = promptPageHtml('Set runner URL', current);
    await promptWindow.loadURL(`data:text/html;charset=utf-8,${encodeURIComponent(page)}`);
    const submitted: unknown = await promptWindow.webContents.executeJavaScript(PROMPT_SUBMIT_SCRIPT, true);
    return typeof submitted === 'string' ? { status: 'submitted', value: submitted } : { status: 'cancelled' };
  } catch (failure) {
    if (dismissed) return { status: 'cancelled' };
    return { status: 'failed', error: errorMessage(failure) };
  } finally {
    if (!promptWindow.isDestroyed()) promptWindow.close();
  }
}

async function promptRunnerUrl(): Promise<void> {
  const window = mainWindow;
  if (window === null) return;
  const prompt = await requestRunnerUrl(window, config.runnerUrl);
  if (prompt.status === 'cancelled') return;
  if (prompt.status === 'failed') {
    await showMessage(window, {
      type: 'error',
      title: 'Set runner URL',
      message: 'The runner URL prompt could not be opened.',
      detail: prompt.error,
      buttons: ['Close'],
    });
    return;
  }
  let runnerUrl: string;
  try {
    runnerUrl = normalizeRunnerUrl(prompt.value);
  } catch (failure) {
    await showMessage(window, {
      type: 'error',
      title: 'Set runner URL',
      message: 'That runner URL was not accepted.',
      detail: errorMessage(failure),
      buttons: ['Close'],
    });
    return;
  }
  await applyRunnerSettings(window, { runnerUrl });
}

function configureSession(): void {
  const runnerSession = session.defaultSession;

  // No certificate verify proc: it carries no request port, so trust is granted only in certificate-error.

  runnerSession.setPermissionRequestHandler((_contents, permission, callback, details) => {
    const mediaTypes = 'mediaTypes' in details ? details.mediaTypes : undefined;
    callback(permissionAllowed(permission, details.requestingUrl ?? '', config.runnerUrl, mediaTypes));
  });

  runnerSession.setPermissionCheckHandler((_contents, permission, requestingOrigin, details) => {
    const mediaTypes = details.mediaType === undefined ? undefined : [details.mediaType];
    return permissionAllowed(permission, requestingOrigin, config.runnerUrl, mediaTypes);
  });

  runnerSession.webRequest.onHeadersReceived((details, callback) => {
    if (!sameOrigin(details.url, config.runnerUrl) || !isDocumentResource(details.resourceType)) {
      callback({});
      return;
    }
    const policy = contentSecurityPolicyFor(new URL(details.url).pathname, config.runnerUrl);
    callback({ responseHeaders: applyContentSecurityPolicy(details.responseHeaders, policy) });
  });

  runnerSession.on('will-download', (event, item) => {
    const source = item.getURL();
    if (!sameOrigin(source, config.runnerUrl)) {
      event.preventDefault();
      const origin = source === '' ? 'an unknown origin' : source;
      reportOneLine('Download blocked', `${item.getFilename()} came from ${origin}, outside ${config.runnerUrl}.`);
      return;
    }
    item.once('done', (_doneEvent, state) => {
      if (state === 'completed') {
        reportOneLine('Download complete', `${item.getFilename()} — saved to ${item.getSavePath()}`);
        return;
      }
      if (state === 'interrupted') {
        const destination = item.getSavePath() === '' ? 'no destination was chosen' : item.getSavePath();
        reportOneLine('Download failed', `${item.getFilename()} — ${destination}`);
      }
    });
  });
}

function requestExternalOpen(window: BrowserWindow | null, url: string): void {
  if (!externalSchemeAllowed(url)) return;
  externalPromptQueue = externalPromptQueue.then(() =>
    confirmExternalOpen(window, url).catch((failure: unknown) => {
      console.error(`rpi desktop: could not open ${url}: ${errorMessage(failure)}`);
    }),
  );
}

async function confirmExternalOpen(window: BrowserWindow | null, url: string): Promise<void> {
  const host = hostnameOf(url);
  const response = await showMessage(window, {
    type: 'question',
    title: 'Open external link',
    message: `Open ${host === '' ? 'this link' : host} outside rpi desktop?`,
    detail: `Destination: ${url}\n\nOnly continue if you recognise this address. A page can request this link without your intent.`,
    buttons: ['Open in browser', 'Cancel'],
    defaultId: 1,
    cancelId: 1,
    noLink: true,
  });
  if (response !== 0) return;
  await shell.openExternal(url);
}

function hardenWebContents(contents: Electron.WebContents): void {
  contents.on('will-navigate', (details) => {
    if (!navigationAllowed(details.url, config.runnerUrl, true)) details.preventDefault();
  });
  contents.on('will-redirect', (details) => {
    if (sameOrigin(details.url, config.runnerUrl)) return;
    details.preventDefault();
    reportOneLine('Redirect blocked', `The runner page redirected to ${details.url}, outside ${config.runnerUrl}.`);
  });
  contents.on('will-frame-navigate', (details) => {
    if (!navigationAllowed(details.url, config.runnerUrl, details.isMainFrame)) details.preventDefault();
  });
  contents.setWindowOpenHandler((details) => {
    requestExternalOpen(BrowserWindow.fromWebContents(contents), details.url);
    return { action: 'deny' };
  });
  contents.on('will-attach-webview', (event) => {
    event.preventDefault();
  });
}

function createWindow(load = true): BrowserWindow {
  const displays = screen.getAllDisplays().map((display) => display.bounds);
  const bounds = clampWindowBounds(
    readWindowState(userDataFile(WINDOW_STATE_FILE_NAME)),
    displays,
    screen.getPrimaryDisplay().bounds,
  );
  const window = new BrowserWindow({
    title: 'rpi',
    x: bounds.x,
    y: bounds.y,
    width: bounds.width,
    height: bounds.height,
    minWidth: MIN_WINDOW_SIZE.width,
    minHeight: MIN_WINDOW_SIZE.height,
    webPreferences: {
      contextIsolation: true, nodeIntegration: false, sandbox: true, webSecurity: true,
      preload: path.join(__dirname, 'preload.js'),
      additionalArguments: [`--rpi-desktop-origin=${load ? config.runnerUrl : ''}`],
    },
  });
  if (bounds.maximized) window.maximize();
  window.on('close', () => {
    const normal = window.getNormalBounds();
    try {
      writeWindowState(userDataFile(WINDOW_STATE_FILE_NAME), {
        width: normal.width,
        height: normal.height,
        x: normal.x,
        y: normal.y,
        maximized: window.isMaximized(),
      });
    } catch (failure) {
      console.error(`rpi desktop: window state not saved: ${errorMessage(failure)}`);
    }
  });
  window.on('closed', () => {
    if (mainWindow === window) {
      ++loginGeneration;
      void desktopLogin.cancel().catch(() => reportOneLine('Sign in shutdown failed', 'The login process has not exited. Quit again to retry shutdown.'));
    }
    if (mainWindow === window) mainWindow = null;
  });
  hardenWebContents(window.webContents);
  window.webContents.on('did-start-navigation', (_event, _url, _inPlace, isMainFrame) => {
    if (isMainFrame && !_inPlace && mainWindow === window) { actions.clear(); buildMenu(); }
  });
  window.webContents.on('did-fail-load', (_event, errorCode, errorDescription, validatedURL, isMainFrame) => {
    // -3 is ERR_ABORTED: a load this shell cancelled, not a failure to surface.
    if (!isMainFrame || errorCode === ERR_ABORTED) return;
    const failingUrl = validatedURL === '' ? targetUrl() : validatedURL;
    const page = errorPageHtml({
      failingUrl,
      description: errorDescription,
      errorCode,
      runnerUrl: config.runnerUrl,
      route: config.route,
    });
    loadUrl(window, `data:text/html;charset=utf-8,${encodeURIComponent(page)}`);
  });
  if (load) loadUrl(window, targetUrl());
  return window;
}

function buildMenu(): void {
  Menu.setApplicationMenu(
    Menu.buildFromTemplate([
      {
        label: 'File',
        submenu: [
          { label: 'New workspace…', enabled: actions.has('new-workspace'), click: () => emitAction('new-workspace') },
          { label: 'Attach files…', enabled: actions.has('attach'), click: () => emitAction('attach') },
          { type: 'separator' },
          { role: 'quit' },
        ],
      },
      {
        label: 'Session',
        submenu: [
          { label: 'New session…', enabled: actions.has('new-session'), click: () => emitAction('new-session') },
          { label: 'Stop generation', enabled: actions.has('stop'), click: () => emitAction('stop') },
          { label: 'Settings…', enabled: actions.has('settings'), click: () => emitAction('settings') },
        ],
      },
      { label: 'Edit', submenu: [{ role: 'undo' }, { role: 'redo' }, { type: 'separator' }, { role: 'cut' }, { role: 'copy' }, { role: 'paste' }, { role: 'selectAll' }] },
      {
        label: 'Runner',
        submenu: [
          { label: 'Sign in / configure provider…', enabled: runnerMenuEnabled(config, quitting).signIn, click: () => { void configureProvider(); } },
          { label: 'Retry connection', click: () => { void startRunner(); } },
          ...ROUTES.map((route): Electron.MenuItemConstructorOptions => ({
            label: `Open /${route}`,
            click: () => {
              if (mainWindow !== null) void applyRunnerSettings(mainWindow, { route });
            },
          })),
          { type: 'separator' },
          {
            label: 'Use local runner',
            enabled: runnerMenuEnabled(config, quitting).useLocal,
            click: () => {
              if (mainWindow !== null) void applyRunnerSettings(mainWindow, { local: true });
            },
          },
          {
            label: 'Set runner URL…',
            click: () => {
              void promptRunnerUrl();
            },
          },
          {
            label: 'Trust current certificate…',
            click: () => {
              void trustCurrentCertificate();
            },
          },
        ],
      },
      {
        label: 'View',
        submenu: [
          {
            label: 'Reload',
            accelerator: 'CmdOrCtrl+R',
            click: () => {
              const window = mainWindow;
              if (window === null || window.isDestroyed()) return;
              if (sameOrigin(window.webContents.getURL(), config.runnerUrl)) window.webContents.reload();
              else void startRunner();
            },
          },
          { role: 'toggleDevTools' },
          { type: 'separator' },
          { role: 'zoomIn' },
          { role: 'zoomOut' },
          { role: 'resetZoom' },
          { type: 'separator' },
          { role: 'togglefullscreen' },
        ],
      },
      {
        label: 'Help',
        submenu: [
          {
            label: 'About',
            click: () => {
              void showMessage(mainWindow, {
                type: 'info',
                title: 'About rpi desktop',
                message: `rpi desktop ${app.getVersion()}`,
                detail: `Runner: ${config.runnerUrl}\nRoute: /${config.route}\nElectron: ${process.versions.electron}`,
                buttons: ['Close'],
              });
            },
          },
        ],
      },
    ]),
  );
}

async function bootstrap(): Promise<void> {
  const stored = readSettings(userDataFile(SETTINGS_FILE_NAME));
  config = resolveRunnerConfig(process.argv.slice(1), process.env.RPI_DESKTOP_RUNNER, stored);
  configureSession();
  ipcMain.on('rpi-desktop:actions', (event, kinds: unknown) => {
    const contents = mainWindow?.webContents;
    if (!contents || event.sender !== contents || event.senderFrame !== contents.mainFrame
      || !sameOrigin(event.senderFrame.url, config.runnerUrl)
      || !Array.isArray(kinds) || kinds.length > actionKinds.length
      || !kinds.every((kind): kind is ActionKind => actionKinds.includes(kind))) return;
    actions = new Set(kinds);
    buildMenu();
  });
  buildMenu();
  await startRunner();
}

if (!app.requestSingleInstanceLock()) {
  app.quit();
} else {
  app.on('second-instance', () => {
    const window = mainWindow;
    if (window === null || window.isDestroyed()) return;
    if (window.isMinimized()) window.restore();
    window.focus();
  });

  app.on('certificate-error', (event, _contents, url, error, certificate, callback) => {
    const origin = originOf(url);
    const hostname = hostnameOf(url);
    const fingerprint = sha256FingerprintFromPem(certificate.data);
    if (certificateDecision(currentPins(), origin, hostname, fingerprint) === 'pinned') {
      event.preventDefault();
      callback(true);
      return;
    }
    callback(false);
    queuePendingCertificate({ origin, hostname, fingerprint, reason: error });
  });

  app.on('before-quit', (event) => {
    if (quitReady) return;
    event.preventDefault();
    if (quitting) return;
    quitting = true;
    void (async () => {
      ++loginGeneration;
      if (desktopServer) await desktopServer.stop();
      await desktopLogin.cancel();
      await loginStarting;
      await starting;
      if (desktopServer) await desktopServer.stop();
      quitReady = true;
      app.quit();
    })().catch((failure) => {
      quitting = false;
      reportOneLine('Runner shutdown failed', errorMessage(failure));
    });
  });
  app.on('window-all-closed', () => {
    if (process.platform !== 'darwin') app.quit();
  });

  app.on('activate', () => {
    if (mainWindow === null && !quitting) void startRunner();
  });

  void app
    .whenReady()
    .then(bootstrap)
    .catch((failure: unknown) => {
      console.error(`rpi desktop: startup failed: ${errorMessage(failure)}`);
      app.quit();
    });
}
