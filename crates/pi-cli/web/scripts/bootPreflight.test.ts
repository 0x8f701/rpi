#!/usr/bin/env node
// Bounded /status boot-preflight regression for src/bootPreflight.ts and its
// App.tsx wiring — proves the empty-token boot decides on the strict
// { tokenRequired } body (required/optional/unavailable/malformed/saved
// token), that the token_required hold suppresses the reconnect timer until a
// non-empty token is saved (which reconnects exactly once), and that the
// probe never logs or echoes a token value. Run through `npm run build`.
//
// Exit codes: 0 = every assertion held; 1 = a regression.
import fs from 'node:fs';
import path from 'node:path';
import {
  BOOT_PREFLIGHT_TIMEOUT_MS,
  decideBootConnection,
  fetchBootStatus,
  probeListenAuth,
  tokenCommitWhileHeld,
  type BootPreflightOptions,
  shouldReprobeTokenRequirement,
  shouldReleaseTokenHold,
  wakeTransportAction,
  TOKEN_REPROBE_FAILURES,
  TOKEN_HOLD_REPROBE_MS,
} from '../src/bootPreflight.ts';

const failures: string[] = [];
let ran = 0;
function check(name: string, cond: boolean, detail?: string) {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

// ---- strict status fixtures (no secret values anywhere) ----
const REQUIRED_STATUS = { tokenRequired: true, version: '0.2.11', codename: 'alpha-one' };
const OPTIONAL_STATUS = { tokenRequired: false, version: '0.2.11', codename: null };

// ---- decideBootConnection: the five boot decisions ----
{
  check('required + no stored token holds for the token', decideBootConnection(REQUIRED_STATUS, '') === 'token_required');
  check('optional + no stored token connects tokenless', decideBootConnection(OPTIONAL_STATUS, '') === 'connect');
  check('unavailable preflight + empty token still connects (never bricks)', decideBootConnection(null, '') === 'connect');
  check('missing preflight + empty token still connects', decideBootConnection(undefined, '') === 'connect');
  check('saved token connects immediately even when required', decideBootConnection(REQUIRED_STATUS, 'fixture-token') === 'connect');
  check('saved token connects immediately when preflight failed', decideBootConnection(null, 'fixture-token') === 'connect');
}

{
  const seen: Array<{ url: string; method: string; auth: string | null }> = [];
  const fetchFn = (async (input: RequestInfo | URL, init?: RequestInit) => {
    seen.push({
      url: String(input),
      method: init?.method ?? 'GET',
      auth: (init?.headers as Record<string, string> | undefined)?.Authorization ?? null,
    });
    return { status: 401 } as Response;
  }) as typeof fetch;
  const probe = await probeListenAuth('192.0.2.15:8765', 'fixture-token', { fetchFn, pageProtocol: 'https:' });
  check('401 on POST /rpc is unauthorized', probe === 'unauthorized');
  check('auth probe hits /rpc once', seen.length === 1 && seen[0].url === 'https://192.0.2.15:8765/rpc', seen.map((s) => s.url).join(','));
  check('auth probe is POST', seen[0].method === 'POST');
  check('auth probe sends the bearer without logging it in this assertion name', seen[0].auth === 'Bearer fixture-token');
}
{
  const probe = await probeListenAuth('192.0.2.15:8765', 'fixture-token', {
    fetchFn: (async () => ({ status: 400 }) as Response) as typeof fetch,
    pageProtocol: 'https:',
  });
  check('non-401 HTTP means the bearer was accepted', probe === 'accepted');
}
{
  const probe = await probeListenAuth('192.0.2.15:8765', 'fixture-token', {
    fetchFn: (async () => { throw new Error('refused'); }) as unknown as typeof fetch,
    pageProtocol: 'https:',
  });
  check('unreachable listener is not a bad-token hold', probe === 'unreachable');
}
{
  const probe = await probeListenAuth('', 'fixture-token');
  check('empty authority degrades to unreachable', probe === 'unreachable');
}
{
  const probe = await probeListenAuth('192.0.2.15:8765', '');
  check('empty token degrades to unreachable', probe === 'unreachable');
}

// ---- tokenCommitWhileHeld: the save transition ----
{
  check('empty save stays held', tokenCommitWhileHeld('') === 'stay-held');
  check('non-empty save releases the hold and reconnects', tokenCommitWhileHeld('fixture-token') === 'release-and-connect');
}

// ---- fetchBootStatus: strict parse, bounded, failure-degrades-to-null ----
{
  const seen: Array<{ url: string; method: string; signal: AbortSignal | undefined }> = [];
  const fetchFn = (async (input: RequestInfo | URL, init?: RequestInit) => {
    seen.push({
      url: String(input),
      method: init?.method ?? 'GET',
      signal: init?.signal as AbortSignal | undefined,
    });
    return { ok: true, json: async () => REQUIRED_STATUS } as Response;
  }) as typeof fetch;
  const status = await fetchBootStatus('192.0.2.15:8765', { fetchFn, pageProtocol: 'https:' });
  check('required status parses through the strict reader', status !== null && status.tokenRequired === true, JSON.stringify(status));
  check('codename is preserved', status?.codename === 'alpha-one');
  check('preflight hits exactly <origin>/status once', seen.length === 1 && seen[0].url === 'https://192.0.2.15:8765/status', seen.map((s) => s.url).join(','));
  check('preflight is a GET', seen[0].method === 'GET');
  check('preflight carries an abort signal (bounded)', seen[0].signal instanceof AbortSignal);
}
{
  const status = await fetchBootStatus('192.0.2.15:8765', {
    fetchFn: (async () => ({ ok: true, json: async () => ({ tokenRequired: 'yes', version: '0.2.11' }) }) as unknown as Response) as typeof fetch,
    pageProtocol: 'https:',
  });
  check('malformed body degrades to null (connect as before)', status === null);
}
{
  const status = await fetchBootStatus('192.0.2.15:8765', {
    fetchFn: (async () => ({ ok: false, status: 404, json: async () => ({}) }) as unknown as Response) as typeof fetch,
    pageProtocol: 'https:',
  });
  check('non-ok response degrades to null', status === null);
}
{
  const status = await fetchBootStatus('192.0.2.15:8765', {
    fetchFn: (async () => { throw new Error('refused'); }) as unknown as typeof fetch,
    pageProtocol: 'https:',
  });
  check('unreachable endpoint degrades to null', status === null);
}
{
  // Bounded: a fetch that ignores the abort signal still resolves null at the
  // deadline instead of hanging the boot.
  const started = Date.now();
  const { promise: never, reject } = Promise.withResolvers<never>();
  // A spec-compliant fetch rejects when its abort signal fires; the mock
  // mirrors that so the test exercises the real deadline path.
  const fetchFn = ((_input: unknown, init?: RequestInit) => {
    (init?.signal as AbortSignal).addEventListener('abort', () => reject(new Error('aborted')));
    return never;
  }) as unknown as typeof fetch;
  const status = await fetchBootStatus('192.0.2.15:8765', {
    fetchFn,
    timeoutMs: 25,
    pageProtocol: 'https:',
  });
  check('hung preflight resolves null at the deadline', status === null, `${Date.now() - started}ms`);
  check('hung preflight is bounded near the timeout', Date.now() - started < 2000, `${Date.now() - started}ms`);
}
{
  // The default timeout constant stays bounded (the boot must never wait long).
  check('preflight timeout is bounded', BOOT_PREFLIGHT_TIMEOUT_MS > 0 && BOOT_PREFLIGHT_TIMEOUT_MS <= 10000, String(BOOT_PREFLIGHT_TIMEOUT_MS));
}
{
  const options: BootPreflightOptions = { fetchFn: (async () => { throw new Error('no'); }) as typeof fetch };
  const status = await fetchBootStatus('', options);
  check('empty authority degrades to null (no throw)', status === null);
}

// ---- App.tsx wiring: hold suppression, single reconnect on save, render ----
{
  const source = fs.readFileSync(path.join(process.cwd(), 'src/App.tsx'), 'utf8');
  const schedule = source.slice(source.indexOf('const scheduleReconnect'), source.indexOf('const rescheduleSilenceTimer'));
  check('scheduleReconnect checks the token hold', schedule.includes('tokenHoldRef.current'), schedule.slice(0, 200));
  check('held reconnect projects token_required', schedule.includes("setConnState('token_required')"));
  const holdAt = schedule.indexOf('tokenHoldRef.current');
  const armAt = schedule.indexOf('window.setTimeout');
  check('hold guard precedes arming the retry timer', holdAt !== -1 && armAt !== -1 && holdAt < armAt);

  const commit = source.slice(source.indexOf('const handleTokenChange'), source.indexOf('const bootWithPreflight'));
  check('held save consults the pure release decision', commit.includes("tokenCommitWhileHeld(trimmed) === 'stay-held'"));
  check('held save clears the hold before reconnecting', commit.includes('tokenHoldRef.current = false'));
  check('held save reconnects exactly once', commit.split('connect()').length - 1 === 1, commit);

  const boot = source.slice(source.indexOf('const bootWithPreflight'), source.indexOf('/* ---------------- event dispatch ---------------- */'));
  check('boot preflight decides via the pure helper', boot.includes('decideBootConnection(status, tokenRef.current)'));
  check('token_required boot renders the pairing card', source.includes('TokenPairingCard authority={hostRef.current}'));
  check('boot preflight guards against a superseded probe', boot.includes('preflightGenRef.current'));

  const mount = source.slice(source.indexOf('loadInitialAuthorityToken(tokenStorage'), source.indexOf("ws.close(1000, 'unload')"));
  check('empty saved token boots through the preflight', mount.includes("if (saved === '') bootWithPreflight();"));
  check('saved token boots straight into connect', mount.includes('else connect();'));

  check('dead bootProbeRef is removed', !source.includes('bootProbeRef'));
  check('ConnState carries the token_required literal', source.includes("'reconnecting' | 'token_required'"));
  check('empty hint renders the actionable token_required text', source.includes('This listener requires an access token'));
  check('empty hint names the pairing card entry point', source.includes('Paste it in the pairing'));
  check('first-contact token close probes /rpc before auth hold', source.includes('probeListenAuth(host, presented)'));
  check('auth hold still requires confirmed unauthorized', source.includes("authFailed: probe === 'unauthorized'"));
}

{
  check('held wake always re-probes', wakeTransportAction({ tokenHold: true, socketOpen: false, hasStoredToken: false }) === 'preflight');
  check('held wake with a leftover open socket still re-probes', wakeTransportAction({ tokenHold: true, socketOpen: true, hasStoredToken: false }) === 'preflight');
  check('empty-token closed wake uses preflight not connect', wakeTransportAction({ tokenHold: false, socketOpen: false, hasStoredToken: false }) === 'preflight');
  check('empty-token open wake pings', wakeTransportAction({ tokenHold: false, socketOpen: true, hasStoredToken: false }) === 'ping');
  check('stored-token open wake pings', wakeTransportAction({ tokenHold: false, socketOpen: true, hasStoredToken: true }) === 'ping');
  check('stored-token closed wake reconnects immediately', wakeTransportAction({ tokenHold: false, socketOpen: false, hasStoredToken: true }) === 'reconnect');
  check('open socket still pings after long application silence', wakeTransportAction({
    tokenHold: false, socketOpen: true, hasStoredToken: true,
  }) === 'ping');
  check('held wake still re-probes even with an open leftover socket', wakeTransportAction({
    tokenHold: true, socketOpen: true, hasStoredToken: true,
  }) === 'preflight');
}

// ---- reconnect limbo guards (user report: /web websocket often fails to
// ---- auto-recover across listener restarts) ----
{
  // A1: a listener restart can flip a tokenless-accepted endpoint into
  // token-required mode; every refused handshake loops silently because the
  // ever-connected close surface is suppressed. After exactly
  // TOKEN_REPROBE_FAILURES consecutive never-opened failures the loop must
  // re-probe /status instead of blind-connecting.
  check('the re-probe cadence constant is bounded', TOKEN_REPROBE_FAILURES === 3);
  check('no failures never re-probe', shouldReprobeTokenRequirement(0) === false);
  check('first and second failures never re-probe', shouldReprobeTokenRequirement(1) === false && shouldReprobeTokenRequirement(2) === false);
  check('the third consecutive failure re-probes', shouldReprobeTokenRequirement(3) === true);
  check('the counter resets after a probe (4th never re-probes)', shouldReprobeTokenRequirement(4) === false);
  check('the next third failure re-probes again', shouldReprobeTokenRequirement(6) === true);
  // A2: the token_required hold must release itself when the listener
  // restarts tokenless (or the probe is unavailable — never brick).
  check('hold releases when the listener restarted tokenless', shouldReleaseTokenHold(OPTIONAL_STATUS, '') === true);
  check('hold releases when the probe is unavailable (never bricks)', shouldReleaseTokenHold(null, '') === true);
  check('hold stays on a definitive token_required', shouldReleaseTokenHold(REQUIRED_STATUS, '') === false);
  check('a stored token always releases the hold', shouldReleaseTokenHold(REQUIRED_STATUS, 'fixture-token') === true);
  // The hold re-probe cadence is bounded (the held page must not spam /status).
  check('the hold re-probe cadence is bounded 1s..60s', TOKEN_HOLD_REPROBE_MS >= 1000 && TOKEN_HOLD_REPROBE_MS <= 60_000, String(TOKEN_HOLD_REPROBE_MS));
}

{
  const source = fs.readFileSync(path.join(process.cwd(), 'src/App.tsx'), 'utf8');
  const schedule = source.slice(source.indexOf('const scheduleReconnect'), source.indexOf('const rescheduleSilenceTimer'));
  check('scheduleReconnect consults the never-opened re-probe guard',
    schedule.includes('shouldReprobeTokenRequirement(neverOpenedFailuresRef.current)'));
  check('the re-probe resets the failure counter before probing',
    schedule.includes('neverOpenedFailuresRef.current = 0'));
  check('scheduleReconnect arms the hold re-probe through the handoff ref',
    schedule.includes('bootProbeHandoffRef.current()'));
  const heartbeat = source.slice(source.indexOf('const closeForHeartbeat'), source.indexOf('const rescheduleSilenceTimer'));
  check('heartbeat close does not retire while the page is hidden',
    heartbeat.includes("heartbeatMayRetire(document.visibilityState === 'visible')")
      && heartbeat.indexOf('heartbeatMayRetire') < heartbeat.indexOf('const ws = wsRef.current'));
  check('heartbeat failure retires the socket before waiting for onclose',
    heartbeat.includes('detachTransportHandlers(ws)')
      && heartbeat.includes('wsRef.current = null')
      && heartbeat.indexOf('wsRef.current = null') < heartbeat.indexOf('ws.close(4000, reason)'));
  check('heartbeat failure resets reconnect delay immediately',
    heartbeat.includes('delayRef.current = RECONNECT_INITIAL_DELAY_MS')
      && heartbeat.indexOf('delayRef.current = RECONNECT_INITIAL_DELAY_MS')
        < heartbeat.indexOf('scheduleReconnect()'));
  check('heartbeat failure schedules reconnect without onclose',
    heartbeat.includes('scheduleReconnect()')
      && heartbeat.includes("opts?.reconnect === 'immediate'"));
  check('scheduleReconnect reconnects through the connect ref',
    schedule.includes('connectRef.current()'));
  const ping = source.slice(source.indexOf('const sendHeartbeatPing'), source.indexOf('const clearHeartbeatTimers'));
  check('heartbeat ping retires CLOSING/CLOSED without waiting for OPEN',
    ping.includes('planHeartbeatPing')
      && ping.includes("plan === 'retire'")
      && ping.includes("closeForHeartbeat('heartbeat timeout')"));
  check('heartbeat ping consults last inbound when planning',
    ping.includes('lastMessageAtRef.current'));
  check('pong deadline consults last inbound',
    source.includes('shouldCloseForMissedPong(pingSentAtRef.current, Date.now(), lastMessageAtRef.current)'));
  const onmessage = source.slice(source.indexOf('ws.onmessage = (event) =>'), source.indexOf('ws.onclose'));
  check('any inbound after ping clears the pong wait before pong-only handling',
    onmessage.includes('lastMessageAtRef.current = Date.now()')
      && onmessage.includes('pingSentAtRef.current = null')
      && onmessage.indexOf('pingSentAtRef.current = null') < onmessage.indexOf('isPongFrame')
      && onmessage.includes('pongTimerRef.current = null'));
  check('connect arms a handshake deadline for CONNECTING sockets',
    source.includes('shouldRetireHandshake(ws.readyState, handshakeStartedAtRef.current, Date.now())')
      && source.includes('TRANSPORT_HANDSHAKE_DEADLINE_MS')
      && source.includes('if (ws.readyState !== WebSocket.OPEN) neverOpenedFailuresRef.current += 1'));
  check('heartbeat timers clear the handshake deadline',
    source.includes('handshakeTimerRef.current = null')
      && source.includes('handshakeStartedAtRef.current = null'));
  const close = source.slice(source.indexOf('ws.onclose = (event)'), source.indexOf('ws.onerror = () =>'));
  check('a never-opened close increments the failure counter',
    close.includes('if (!socketOpened) neverOpenedFailuresRef.current += 1;'));
  const openWrap = source.slice(source.indexOf('let socketOpened = false'), source.indexOf('ws.onmessage = (event) =>'));
  check('a successful open resets the failure streak',
    openWrap.includes('socketOpened = true;') && openWrap.includes('neverOpenedFailuresRef.current = 0;'));

  const boot = source.slice(source.indexOf('const bootWithPreflight'), source.indexOf('/* ---------------- event dispatch ---------------- */'));
  check('bootWithPreflight publishes itself through the handoff ref',
    boot.includes('bootProbeHandoffRef.current = bootWithPreflight;'));
  check('the token_required boot arms the hold re-probe', boot.includes('armHoldReprobe();'));
  /* P2: a hold that re-probes into a healthy TOKENLESS listener must RELEASE
   * the hold before connecting — otherwise the next drop would return
   * token_required and re-pin the page (shouldReleaseTokenHold dead code). */
  const releaseBranch = boot.slice(boot.indexOf("!== 'token_required')"));
  check('the connect path releases the hold via shouldReleaseTokenHold',
    releaseBranch.includes('shouldReleaseTokenHold(status, tokenRef.current)')
      && releaseBranch.includes('tokenHoldRef.current = false;')
      && releaseBranch.indexOf('shouldReleaseTokenHold') < releaseBranch.indexOf('connect();'));
  check('the hold re-probe timer is cleared on a real connect',
    source.includes('if (holdReprobeTimerRef.current !== null) {')
      && source.includes('window.clearTimeout(holdReprobeTimerRef.current);'));
  check('the hold re-probe timer is cleared on unmount',
    source.includes('The hold re-probe timer must not fire into a torn-down component.'));
  check('wake consults wakeTransportAction instead of connecting blindly',
    boot.includes('wakeTransportAction({')
      && boot.includes("action === 'preflight'")
      && boot.includes('bootProbeHandoffRef.current()'));
  check('open-socket wake pings and does not tear down as stale',
    boot.includes("action === 'ping'")
      && boot.includes('sendHeartbeatPing()')
      && !boot.includes("action === 'stale'")
      && !boot.includes("closeForHeartbeat('stale on wake'"));
  check('wake ping clears a background-aged outstanding ping before probing',
    boot.includes('pingSentAtRef.current = null')
      && boot.indexOf("action === 'ping'") < boot.indexOf('pingSentAtRef.current = null')
      && boot.indexOf('pingSentAtRef.current = null') < boot.indexOf('sendHeartbeatPing()'));
  check('wake ping clears a background-aged silence timer before probing',
    boot.includes('silenceTimerRef.current = null')
      && boot.indexOf("action === 'ping'") < boot.indexOf('silenceTimerRef.current = null')
      && boot.indexOf('silenceTimerRef.current = null') < boot.indexOf('sendHeartbeatPing()'));
  check('connect supersedes an in-flight empty-token preflight',
    source.includes('preflightGenRef.current += 1;')
      && source.indexOf('A real connect supersedes any in-flight empty-token /status preflight')
        < source.indexOf("ws = new WebSocket(`${scheme}${hostValue}/ws`, protocols);"));
}

console.log(`\nbootPreflight.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  for (const failure of failures) console.error(`FAIL ${failure}`);
  process.exit(1);
}
process.exit(0);