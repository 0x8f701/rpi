import { createHash } from 'node:crypto';
import * as fs from 'node:fs';
import * as path from 'node:path';

export const DEFAULT_RUNNER_URL = 'https://127.0.0.1:8765';
export const SETTINGS_FILE_NAME = 'settings.json';
export const PIN_FILE_NAME = 'trusted-pins.json';
export const WINDOW_STATE_FILE_NAME = 'window-state.json';

const CSP_BEFORE_CONNECT =
  "default-src 'self'; script-src 'self' 'unsafe-inline' 'unsafe-eval'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self' data:; connect-src ";
const CSP_AFTER_CONNECT =
  "; media-src 'self' blob: data:; frame-src 'self' data: blob:; object-src 'none'; base-uri 'self'; form-action 'self'";

export const CHAT_CONTENT_SECURITY_POLICY = `${CSP_BEFORE_CONNECT}'self' ws: wss: http: https:${CSP_AFTER_CONNECT}`;

export function contentSecurityPolicyFor(documentPath: string, runnerUrl: string): string {
  if (documentPath === '/chat') return CHAT_CONTENT_SECURITY_POLICY;
  return `${CSP_BEFORE_CONNECT}'self' ${runnerUrl.replace(/^http/, 'ws')}${CSP_AFTER_CONNECT}`;
}

export const RUNNER_PERMISSIONS: readonly string[] = ['media', 'notifications', 'clipboard-sanitized-write'];

export type Route = 'web' | 'chat';
export const ROUTES: readonly Route[] = ['web', 'chat'];

export interface WindowSize {
  width: number;
  height: number;
}

export const DEFAULT_WINDOW_SIZE: WindowSize = { width: 1280, height: 860 };
export const MIN_WINDOW_SIZE: WindowSize = { width: 480, height: 360 };

export interface DisplayBounds {
  x: number;
  y: number;
  width: number;
  height: number;
}

export interface WindowState {
  width: number;
  height: number;
  x: number;
  y: number;
  maximized: boolean;
}

export interface Settings {
  runnerUrl?: string;
  route?: Route;
}

export interface RunnerConfig {
  runnerUrl: string;
  route: Route;
  error: string | null;
  local: boolean;
}

export interface TrustedPin {
  sha256: string;
  addedAt: number;
}

export type PinStore = Record<string, TrustedPin>;

export type CertificateDecision = 'pinned' | 'default';

export function certificateDecision(
  store: PinStore,
  endpoint: string,
  requestHostname: string,
  fingerprint: string,
): CertificateDecision {
  const key = pinLookupOrigin(endpoint);
  if (key === null) return 'default';
  const pin = store[key];
  if (pin === undefined) return 'default';
  if (normalizePinHost(new URL(key).hostname) !== normalizePinHost(requestHostname)) return 'default';
  const presented = normalizeFingerprint(fingerprint);
  return presented !== '' && presented === pin.sha256 ? 'pinned' : 'default';
}

export interface LoadFailure {
  failingUrl: string;
  description: string;
  errorCode: number;
  runnerUrl: string;
  route: Route;
}

export function errorMessage(failure: unknown): string {
  return failure instanceof Error ? failure.message : String(failure);
}

export function readJsonFile(file: string): unknown {
  try {
    return JSON.parse(fs.readFileSync(file, 'utf8'));
  } catch {
    return null;
  }
}

export function writeJsonFileAtomic(file: string, value: unknown): void {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  const temp = `${file}.${process.pid}.tmp`;
  fs.writeFileSync(temp, `${JSON.stringify(value, null, 2)}\n`, 'utf8');
  fs.renameSync(temp, file);
}

export function parseRoute(value: string | undefined | null): Route | null {
  return value === 'web' || value === 'chat' ? value : null;
}

export function readSettings(file: string): Settings {
  const raw = readJsonFile(file);
  if (typeof raw !== 'object' || raw === null) return {};
  const fields = raw as Record<string, unknown>;
  const settings: Settings = {};
  if (typeof fields.runnerUrl === 'string') settings.runnerUrl = fields.runnerUrl;
  const route = parseRoute(typeof fields.route === 'string' ? fields.route : null);
  if (route !== null) settings.route = route;
  return settings;
}

export function writeSettings(file: string, settings: Settings): void {
  writeJsonFileAtomic(file, settings);
}

const SCHEME_PATTERN = /^[a-z][a-z0-9+.-]*:\/\//i;
const RUNNER_PATHS: readonly string[] = ['/', '/web', '/chat'];

export function normalizeRunnerUrl(raw: string): string {
  const trimmed = raw.trim();
  if (trimmed === '') throw new Error('runner URL is empty');
  const withScheme = SCHEME_PATTERN.test(trimmed) ? trimmed : `https://${trimmed}`;
  let url: URL;
  try {
    url = new URL(withScheme);
  } catch {
    throw new Error(`not a valid URL: ${trimmed}`);
  }
  if (url.protocol !== 'https:' && url.protocol !== 'http:') {
    throw new Error(`runner URL must use http or https: ${trimmed}`);
  }
  if (url.username !== '' || url.password !== '') {
    throw new Error(`runner URL must not embed credentials: ${trimmed}`);
  }
  if (url.search !== '') {
    throw new Error(`runner URL must not carry a query: ${trimmed}`);
  }
  if (!RUNNER_PATHS.includes(url.pathname)) {
    throw new Error(`runner URL path must be /, /web or /chat: ${trimmed}`);
  }
  return url.origin;
}

export function buildRouteUrl(runnerUrl: string, route: Route): string {
  if (parseRoute(route) === null) throw new Error(`unknown route: ${String(route)}`);
  // A /web or /chat suffix on the runner URL never overrides the selected route.
  return `${normalizeRunnerUrl(runnerUrl)}/${route}`;
}

function flagValue(argv: readonly string[], flag: string): string | undefined {
  const index = argv.indexOf(flag);
  const value = index < 0 ? undefined : argv[index + 1];
  return value === undefined || value.startsWith('-') ? undefined : value;
}

export function resolveRunnerConfig(
  argv: readonly string[],
  env: string | undefined,
  stored: Settings | null,
): RunnerConfig {
  const fromEnv = env !== undefined && env.trim() !== '' ? env : undefined;
  const requested = flagValue(argv, '--runner') ?? fromEnv ?? stored?.runnerUrl;
  let runnerUrl = DEFAULT_RUNNER_URL;
  let error: string | null = argv.includes('--runner') && flagValue(argv, '--runner') === undefined
    ? '--runner requires a URL' : null;
  if (typeof requested === 'string') {
    try {
      runnerUrl = normalizeRunnerUrl(requested);
    } catch (failure) {
      error = errorMessage(failure);
    }
  }
  const route = parseRoute(flagValue(argv, '--route')) ?? stored?.route ?? 'web';
  return { runnerUrl, route, error, local: requested === undefined && !argv.includes('--runner') };
}


export interface RunnerSettingsPatch {
  runnerUrl?: string;
  route?: Route;
  /** Drop a stored remote runner and start the owned loopback process again. */
  local?: boolean;
}

/** Next shell config after a menu change. A remote URL leaves managed-local
 *  mode; `local` returns to it and drops the stored URL. */
export function nextRunnerConfig(current: RunnerConfig, patch: RunnerSettingsPatch): RunnerConfig {
  if (patch.local === true) {
    return { runnerUrl: DEFAULT_RUNNER_URL, route: patch.route ?? current.route, error: null, local: true };
  }
  const runnerUrl = patch.runnerUrl ?? current.runnerUrl;
  return {
    runnerUrl,
    route: patch.route ?? current.route,
    error: patch.runnerUrl === undefined ? current.error : null,
    local: patch.runnerUrl === undefined && current.local,
  };
}

/** Sign-in only exists for the owned runner. Local restore only exists once a remote URL is stored. */
export function runnerMenuEnabled(config: RunnerConfig, quitting: boolean): { signIn: boolean; useLocal: boolean } {
  return { signIn: config.local && !quitting, useLocal: !config.local && !quitting };
}
export function normalizeFingerprint(raw: string): string {
  return raw.replace(/:/g, '').replace(/\s+/g, '').toLowerCase();
}

function isFingerprint(value: string): boolean {
  return /^[0-9a-f]{64}$/.test(value);
}

export function normalizePinHost(hostname: string): string {
  return hostname.trim().toLowerCase().replace(/^\[(.*)\]$/, '$1');
}

export function normalizePinOrigin(endpoint: string): string | null {
  let parsed: URL;
  try {
    parsed = new URL(endpoint.trim());
  } catch {
    return null;
  }
  if (parsed.protocol !== 'https:' && parsed.protocol !== 'http:') return null;
  if (parsed.username !== '' || parsed.password !== '') return null;
  return parsed.origin;
}

/** The pin names the TLS certificate of one host:port. A secure WebSocket
 *  presents the certificate of the https origin it connects to, so it must
 *  resolve to that same key instead of a scheme of its own. */
export function pinLookupOrigin(endpoint: string): string | null {
  const origin = normalizePinOrigin(endpoint);
  if (origin !== null) return origin;
  let parsed: URL;
  try {
    parsed = new URL(endpoint.trim());
  } catch {
    return null;
  }
  if (parsed.protocol !== 'ws:' && parsed.protocol !== 'wss:') return null;
  if (parsed.username !== '' || parsed.password !== '') return null;
  return `${parsed.protocol === 'wss:' ? 'https:' : 'http:'}//${parsed.host}`;
}

export function sha256FingerprintFromPem(pem: string): string {
  const body = pem.replace(/-----[^-]+-----/g, '').replace(/\s+/g, '');
  if (body === '') return '';
  return createHash('sha256').update(Buffer.from(body, 'base64')).digest('hex');
}

export type PinStoreStatus = 'ok' | 'missing' | 'unreadable';

export interface PinStoreLoad {
  status: PinStoreStatus;
  store: PinStore;
  error: string | null;
}

export function loadPinStore(file: string): PinStoreLoad {
  let raw: string;
  try {
    raw = fs.readFileSync(file, 'utf8');
  } catch (failure) {
    const code = (failure as NodeJS.ErrnoException).code;
    if (code === 'ENOENT') return { status: 'missing', store: {}, error: null };
    return { status: 'unreadable', store: {}, error: errorMessage(failure) };
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch (failure) {
    return { status: 'unreadable', store: {}, error: `not valid JSON: ${errorMessage(failure)}` };
  }
  if (typeof parsed !== 'object' || parsed === null) {
    return { status: 'unreadable', store: {}, error: 'expected a JSON object of pins' };
  }
  const store: PinStore = {};
  let rejected = 0;
  for (const [endpoint, value] of Object.entries(parsed as Record<string, unknown>)) {
    if (typeof value !== 'object' || value === null) {
      rejected += 1;
      continue;
    }
    const pin = value as Record<string, unknown>;
    const key = normalizePinOrigin(endpoint);
    const sha256 = typeof pin.sha256 === 'string' ? normalizeFingerprint(pin.sha256) : '';
    if (key === null || !isFingerprint(sha256)) {
      rejected += 1;
      continue;
    }
    const addedAt = typeof pin.addedAt === 'number' && Number.isFinite(pin.addedAt) ? pin.addedAt : 0;
    const existing = store[key];
    if (existing !== undefined && existing.addedAt > addedAt) continue;
    store[key] = { sha256, addedAt };
  }
  if (rejected > 0) {
    return { status: 'unreadable', store, error: `${rejected} stored pin(s) are malformed` };
  }
  return { status: 'ok', store, error: null };
}

export function writePinStore(file: string, store: PinStore): void {
  writeJsonFileAtomic(file, store);
}

export interface PinSession {
  store: PinStore;
  unreadable: string | null;
  announcedUnreadable: boolean;
}

export interface PinReadResult {
  session: PinSession;
  notice: string | null;
}

export function readPinSession(file: string, session: PinSession): PinReadResult {
  const loaded = loadPinStore(file);
  const unreadable = loaded.status === 'unreadable' ? loaded.error ?? 'unreadable' : null;
  return {
    session: {
      store: loaded.store,
      unreadable,
      announcedUnreadable: session.announcedUnreadable || unreadable !== null,
    },
    notice: unreadable !== null && !session.announcedUnreadable ? `the pin store could not be read: ${unreadable}` : null,
  };
}

export type PinWriteResult =
  | { persisted: true; session: PinSession; notice: string | null }
  | { persisted: false; session: PinSession; notice: string | null; error: string };

export function writePinSession(
  file: string,
  session: PinSession,
  endpoint: string,
  fingerprint: string,
  addedAt: number,
): PinWriteResult {
  const read = readPinSession(file, session);
  if (read.session.unreadable !== null) {
    return {
      persisted: false,
      session: read.session,
      notice: read.notice,
      error: `the pin store is unreadable: ${read.session.unreadable}`,
    };
  }
  const next = setPin(read.session.store, endpoint, fingerprint, addedAt);
  try {
    writePinStore(file, next);
  } catch (failure) {
    return { persisted: false, session: read.session, notice: read.notice, error: errorMessage(failure) };
  }
  return { persisted: true, session: { ...read.session, store: next }, notice: read.notice };
}

export function setPin(store: PinStore, endpoint: string, fingerprint: string, addedAt: number): PinStore {
  const sha256 = normalizeFingerprint(fingerprint);
  if (!isFingerprint(sha256)) {
    throw new Error(`refusing to pin a malformed SHA-256 fingerprint: ${fingerprint}`);
  }
  const key = pinLookupOrigin(endpoint);
  if (key === null) throw new Error(`refusing to pin a non-origin endpoint: ${endpoint}`);
  return { ...store, [key]: { sha256, addedAt } };
}

function finiteOrNull(value: number | undefined): number | null {
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
}

function dimension(value: number | undefined, fallback: number): number {
  const parsed = finiteOrNull(value);
  return parsed === null || parsed <= 0 ? fallback : Math.round(parsed);
}

function clamp(value: number, min: number, max: number): number {
  return Math.min(Math.max(value, min), max);
}

function overlapArea(rect: DisplayBounds, display: DisplayBounds): number {
  const width = Math.min(rect.x + rect.width, display.x + display.width) - Math.max(rect.x, display.x);
  const height = Math.min(rect.y + rect.height, display.y + display.height) - Math.max(rect.y, display.y);
  return width > 0 && height > 0 ? width * height : 0;
}

export function readWindowState(file: string): Partial<WindowState> {
  const raw = readJsonFile(file);
  if (typeof raw !== 'object' || raw === null) return {};
  const fields = raw as Record<string, unknown>;
  const state: Partial<WindowState> = {};
  for (const field of ['width', 'height', 'x', 'y'] as const) {
    const value = fields[field];
    if (typeof value === 'number' && Number.isFinite(value)) state[field] = value;
  }
  if (typeof fields.maximized === 'boolean') state.maximized = fields.maximized;
  return state;
}

export function writeWindowState(file: string, state: WindowState): void {
  writeJsonFileAtomic(file, state);
}

export function clampWindowBounds(
  state: Partial<WindowState> | null | undefined,
  displays: readonly DisplayBounds[],
  primary: DisplayBounds,
  defaults: WindowSize = DEFAULT_WINDOW_SIZE,
): WindowState {
  const maximized = state?.maximized === true;
  const width = dimension(state?.width, defaults.width);
  const height = dimension(state?.height, defaults.height);
  if (displays.length === 0) return { width, height, x: 0, y: 0, maximized };
  const x = finiteOrNull(state?.x);
  const y = finiteOrNull(state?.y);
  let target: DisplayBounds | null = null;
  if (x !== null && y !== null) {
    let best = 0;
    for (const display of displays) {
      const area = overlapArea({ x, y, width, height }, display);
      if (area > best) {
        best = area;
        target = display;
      }
    }
  }
  const display = target ?? primary;
  const size = {
    width: clamp(width, MIN_WINDOW_SIZE.width, Math.max(MIN_WINDOW_SIZE.width, display.width)),
    height: clamp(height, MIN_WINDOW_SIZE.height, Math.max(MIN_WINDOW_SIZE.height, display.height)),
  };
  // A position overlapping no connected display is centred instead of clamped.
  if (x === null || y === null || target === null) {
    return {
      ...size,
      x: Math.round(display.x + (display.width - size.width) / 2),
      y: Math.round(display.y + (display.height - size.height) / 2),
      maximized,
    };
  }
  return {
    ...size,
    x: clamp(x, display.x, display.x + display.width - size.width),
    y: clamp(y, display.y, display.y + display.height - size.height),
    maximized,
  };
}

export function sameOrigin(url: string, origin: string): boolean {
  try {
    return new URL(url).origin === origin;
  } catch {
    return false;
  }
}

export function permissionAllowed(
  permission: string,
  requestingUrl: string,
  runnerOrigin: string,
  mediaTypes?: readonly string[],
): boolean {
  if (!RUNNER_PERMISSIONS.includes(permission) || !sameOrigin(requestingUrl, runnerOrigin)) return false;
  return !(permission === 'media' && (mediaTypes ?? []).includes('video'));
}

const LOCAL_FRAME_URLS: readonly string[] = ['about:blank', 'about:srcdoc'];

export function navigationAllowed(url: string, runnerOrigin: string, isMainFrame: boolean): boolean {
  let parsed: URL;
  try {
    parsed = new URL(url);
  } catch {
    return false;
  }
  const onRunner = parsed.origin === runnerOrigin;
  if (isMainFrame) return onRunner && (parsed.protocol === 'http:' || parsed.protocol === 'https:');
  if (onRunner) return true;
  return LOCAL_FRAME_URLS.includes(url);
}

const DOCUMENT_RESOURCE_TYPES: readonly string[] = ['mainFrame', 'subFrame'];

export function isDocumentResource(resourceType: string): boolean {
  return DOCUMENT_RESOURCE_TYPES.includes(resourceType);
}

const EXTERNAL_SCHEMES: readonly string[] = ['http:', 'https:', 'mailto:'];

export function externalSchemeAllowed(url: string): boolean {
  try {
    return EXTERNAL_SCHEMES.includes(new URL(url).protocol);
  } catch {
    return false;
  }
}

export function applyContentSecurityPolicy(
  responseHeaders: Record<string, string[] | undefined> | undefined,
  policy: string,
): Record<string, string[]> {
  const headers: Record<string, string[]> = {};
  for (const [name, value] of Object.entries(responseHeaders ?? {})) {
    if (value === undefined || name.toLowerCase() === 'content-security-policy') continue;
    headers[name] = value;
  }
  headers['Content-Security-Policy'] = [policy];
  return headers;
}

function escapeHtml(value: string): string {
  return value
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}

const PAGE_STYLE = `body{font:14px/1.5 system-ui,sans-serif;margin:0;padding:24px;background:#16181d;color:#e8e8e8}
h1{font-size:17px;margin:0 0 14px}
p{margin:8px 0}
li{margin:4px 0}
code{font:13px ui-monospace,SFMono-Regular,monospace;background:#22252c;padding:1px 5px;border-radius:3px}
label{display:block;margin-bottom:8px}
input{width:100%;box-sizing:border-box;font:13px ui-monospace,SFMono-Regular,monospace;padding:6px 8px;background:#22252c;color:#e8e8e8;border:1px solid #3c414b;border-radius:4px}
footer{margin-top:14px;display:flex;gap:8px;justify-content:flex-end}
button{font:13px system-ui,sans-serif;padding:5px 12px;background:#2b303a;color:#e8e8e8;border:1px solid #3c414b;border-radius:4px}`;

export function errorPageHtml(failure: LoadFailure): string {
  const retry = buildRouteUrl(failure.runnerUrl, failure.route);
  return `<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>Runner unreachable</title><style>
${PAGE_STYLE}
</style></head><body>
<h1>rpi desktop could not load the runner</h1>
<p>Failing URL: <code>${escapeHtml(failure.failingUrl)}</code></p>
<p>Failure: ${escapeHtml(failure.description)} <code>${failure.errorCode}</code></p>
<ul>
<li>Retry: <code>View &rarr; Reload</code> (Ctrl/Cmd+R) or <code>Runner &rarr; Open /${failure.route}</code> &mdash; <code>${escapeHtml(retry)}</code>.</li>
<li>Wrong address: <code>Runner &rarr; Set runner URL&hellip;</code>.</li>
<li>Self-signed certificate refused: <code>Runner &rarr; Trust current certificate&hellip;</code>.</li>
</ul>
<p>The listener serves this client at <code>/web</code> or <code>/chat</code> only.</p>
</body></html>`;
}

export function promptPageHtml(title: string, initialValue: string): string {
  return `<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>${escapeHtml(title)}</title><style>
${PAGE_STYLE}
</style></head><body>
<form id="runner-form">
<label for="runner-url">Runner URL &mdash; <code>https://host:port</code>, optionally ending in <code>/web</code> or <code>/chat</code></label>
<input id="runner-url" name="runner-url" value="${escapeHtml(initialValue)}" spellcheck="false" autocomplete="off">
<footer><button type="button" id="runner-cancel">Cancel</button><button type="submit">Use this runner</button></footer>
</form>
</body></html>`;
}

export const PROMPT_SUBMIT_SCRIPT = `(() => {
  const { promise, resolve } = Promise.withResolvers();
  const form = document.getElementById('runner-form');
  const input = document.getElementById('runner-url');
  if (form === null || input === null) {
    resolve(null);
    return promise;
  }
  input.focus();
  input.select();
  form.addEventListener('submit', (event) => {
    event.preventDefault();
    resolve(input.value.trim());
  });
  document.getElementById('runner-cancel')?.addEventListener('click', () => resolve(null));
  return promise;
})()`;
