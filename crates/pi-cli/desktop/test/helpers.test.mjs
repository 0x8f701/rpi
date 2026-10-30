#!/usr/bin/env node
import * as fs from 'node:fs';
import { createRequire } from 'node:module';
import * as os from 'node:os';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';

const packageDir = path.dirname(path.dirname(fileURLToPath(import.meta.url)));

function fileTimes(directory, extension) {
  let names;
  try {
    names = fs.readdirSync(directory);
  } catch {
    return [];
  }
  return names
    .filter((name) => name.endsWith(extension))
    .map((name) => fs.statSync(path.join(directory, name)).mtimeMs);
}

function assertFreshDist() {
  const outputs = fileTimes(path.join(packageDir, 'dist'), '.js');
  if (outputs.length === 0) {
    console.error('helpers.test: dist/ has no compiled output — run `npm run build` first.');
    process.exit(1);
  }
  const sources = [
    ...fileTimes(path.join(packageDir, 'src'), '.ts'),
    fs.statSync(path.join(packageDir, 'tsconfig.json')).mtimeMs,
  ];
  const newestSource = Math.max(...sources);
  const oldestOutput = Math.min(...outputs);
  if (oldestOutput < newestSource) {
    console.error('helpers.test: dist/ is older than src/ — run `npm run build` first.');
    process.exit(1);
  }
}

assertFreshDist();

const require = createRequire(import.meta.url);
const helpers = require('../dist/helpers.js');
const {
  CHAT_CONTENT_SECURITY_POLICY,
  DEFAULT_RUNNER_URL,
  DEFAULT_WINDOW_SIZE,
  MIN_WINDOW_SIZE,
  applyContentSecurityPolicy,
  buildRouteUrl,
  certificateDecision,
  clampWindowBounds,
  nextRunnerConfig,
  runnerMenuEnabled,
  contentSecurityPolicyFor,
  errorPageHtml,
  externalSchemeAllowed,
  isDocumentResource,
  loadPinStore,
  navigationAllowed,
  normalizeRunnerUrl,
  permissionAllowed,
  promptPageHtml,
  readPinSession,
  readSettings,
  readWindowState,
  resolveRunnerConfig,
  sameOrigin,
  setPin,
  sha256FingerprintFromPem,
  writePinSession,
  writePinStore,
  writeSettings,
  writeWindowState,
} = helpers;

const failures = [];
let ran = 0;

function check(name, condition) {
  ran += 1;
  if (!condition) failures.push(name);
}

function throwsWith(action, fragment) {
  try {
    action();
    return false;
  } catch (failure) {
    return failure instanceof Error && failure.message.includes(fragment);
  }
}

const FP_A = 'a'.repeat(64);
const FP_B = 'b'.repeat(64);
const RUNNER_ORIGIN = 'https://127.0.0.1:8765';

check('bare host:port gains https', normalizeRunnerUrl('127.0.0.1:8765') === 'https://127.0.0.1:8765');
check('bare host with a /web path gains https', normalizeRunnerUrl('localhost:9000/web') === 'https://localhost:9000');
check('scheme-less IPv6 gains https', normalizeRunnerUrl('[::1]:8765') === 'https://[::1]:8765');
check('input is trimmed', normalizeRunnerUrl('  runner.example:8443  ') === 'https://runner.example:8443');
check('existing http scheme is kept', normalizeRunnerUrl('http://runner.example:8765') === 'http://runner.example:8765');
check('/web suffix normalizes to the origin', normalizeRunnerUrl('https://runner.example:8765/web') === 'https://runner.example:8765');
check('/chat suffix normalizes to the origin', normalizeRunnerUrl('https://runner.example:8765/chat') === 'https://runner.example:8765');
check('trailing slash is dropped', normalizeRunnerUrl('https://runner.example:8765/') === 'https://runner.example:8765');
check('default https port is dropped', normalizeRunnerUrl('https://runner.example:443') === 'https://runner.example');
check('credentials are rejected', throwsWith(() => normalizeRunnerUrl('https://user:secret@runner.example:8765'), 'credentials'));
check('user-only credentials are rejected', throwsWith(() => normalizeRunnerUrl('user@runner.example:8765'), 'credentials'));
check('query is rejected', throwsWith(() => normalizeRunnerUrl('https://runner.example:8765/?token=1'), 'query'));
check('unknown path is rejected', throwsWith(() => normalizeRunnerUrl('https://runner.example:8765/api'), 'path'));
check('bare origin path is rejected', throwsWith(() => normalizeRunnerUrl('https://runner.example:8765/web/'), 'path'));
check('file scheme is rejected', throwsWith(() => normalizeRunnerUrl('file:///tmp/index.html'), 'http or https'));
check('ftp scheme is rejected', throwsWith(() => normalizeRunnerUrl('ftp://runner.example'), 'http or https'));
check('empty input is rejected', throwsWith(() => normalizeRunnerUrl('   '), 'empty'));
check('out-of-range port is rejected', throwsWith(() => normalizeRunnerUrl('https://runner.example:99999'), 'not a valid URL'));

check('web route maps to /web', buildRouteUrl('127.0.0.1:8765', 'web') === 'https://127.0.0.1:8765/web');
check('chat route maps to /chat', buildRouteUrl('https://127.0.0.1:8765', 'chat') === 'https://127.0.0.1:8765/chat');
check('route overrides a conflicting suffix', buildRouteUrl('https://127.0.0.1:8765/chat', 'web') === 'https://127.0.0.1:8765/web');
check('route never yields the bare origin', buildRouteUrl('https://127.0.0.1:8765', 'chat') !== 'https://127.0.0.1:8765');
check('unknown route is rejected', throwsWith(() => buildRouteUrl('https://127.0.0.1:8765', 'collab'), 'unknown route'));

const stored = { runnerUrl: 'https://stored.example:1', route: 'chat' };
check('--runner wins over env and settings', resolveRunnerConfig(['--runner', 'https://argv.example:2/web'], 'https://env.example:3', stored).runnerUrl === 'https://argv.example:2');
check('env wins over settings', resolveRunnerConfig([], 'https://env.example:3', stored).runnerUrl === 'https://env.example:3');
check('settings win over the default', resolveRunnerConfig([], undefined, stored).runnerUrl === 'https://stored.example:1');
check('default is used when nothing is configured', resolveRunnerConfig([], undefined, null).runnerUrl === DEFAULT_RUNNER_URL);
check('empty env falls through to settings', resolveRunnerConfig([], '   ', stored).runnerUrl === 'https://stored.example:1');
check('--route overrides the stored route', resolveRunnerConfig(['--route', 'web'], undefined, stored).route === 'web');
check('stored route applies without --route', resolveRunnerConfig([], undefined, stored).route === 'chat');
check('bogus --route falls back to the stored route', resolveRunnerConfig(['--route', 'collab'], undefined, stored).route === 'chat');
check('route defaults to web', resolveRunnerConfig([], undefined, null).route === 'web');
check('no error when the runner resolves', resolveRunnerConfig([], undefined, stored).error === null);
const rejected = resolveRunnerConfig(['--runner', 'https://runner.example:8765/?token=1'], undefined, stored);
check('rejected --runner reports the reason', rejected.error !== null && rejected.error.includes('query'));
check('rejected --runner falls back to the default', rejected.runnerUrl === DEFAULT_RUNNER_URL);
check('unconfigured startup owns a local runner', resolveRunnerConfig([], undefined, null).local === true);
check('route-only settings do not persist a local endpoint', resolveRunnerConfig([], undefined, { route: 'chat' }).local === true);
check('explicit default endpoint connects without spawning', resolveRunnerConfig(['--runner', DEFAULT_RUNNER_URL], undefined, null).local === false);
check('environment endpoint connects without spawning', resolveRunnerConfig([], DEFAULT_RUNNER_URL, null).local === false);
check('stored endpoint connects without spawning', resolveRunnerConfig([], undefined, stored).local === false);
check('invalid explicit endpoint never starts a local runner', rejected.local === false);
const missingRunner = resolveRunnerConfig(['--runner'], undefined, null);
const remote = nextRunnerConfig(
  { runnerUrl: DEFAULT_RUNNER_URL, route: 'web', error: null, local: true },
  { runnerUrl: 'https://stored.example:1' },
);
check('a remote URL leaves managed-local mode', remote.local === false && remote.runnerUrl === 'https://stored.example:1');
const restored = nextRunnerConfig(remote, { local: true });
check('local restore drops the stored URL', restored.local === true && restored.runnerUrl === DEFAULT_RUNNER_URL && restored.route === 'web');
check('route-only change keeps the remote runner', nextRunnerConfig(remote, { route: 'chat' }).runnerUrl === 'https://stored.example:1' && nextRunnerConfig(remote, { route: 'chat' }).local === false);
const localMenu = { runnerUrl: DEFAULT_RUNNER_URL, route: 'web', error: null, local: true };
check('local mode offers sign-in and hides local restore', runnerMenuEnabled(localMenu, false).signIn === true && runnerMenuEnabled(localMenu, false).useLocal === false);
check('remote mode hides sign-in and offers local restore', runnerMenuEnabled(remote, false).signIn === false && runnerMenuEnabled(remote, false).useLocal === true);
check('quitting disables both runner switches', runnerMenuEnabled(remote, true).useLocal === false && runnerMenuEnabled(localMenu, true).signIn === false);
check('missing runner value is rejected without local fallback', missingRunner.local === false && missingRunner.error === '--runner requires a URL');

const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'rpi-desktop-helpers-'));
const pinFile = path.join(dir, 'trusted-pins.json');

check('a missing pin store is a first run', loadPinStore(pinFile).status === 'missing');
check('a missing pin store reads empty', Object.keys(loadPinStore(pinFile).store).length === 0);

writePinStore(pinFile, setPin({}, 'HTTPS://Runner.Example:8765/web', FP_A, 1000));
const reopened = loadPinStore(pinFile);
check('a written pin store loads as ok', reopened.status === 'ok' && reopened.error === null);
check('pin store round-trips the fingerprint', reopened.store['https://runner.example:8765'].sha256 === FP_A);
check('pin store round-trips addedAt', reopened.store['https://runner.example:8765'].addedAt === 1000);
check('pin store keys are normalized origins', Object.keys(reopened.store).join() === 'https://runner.example:8765');

writePinStore(pinFile, setPin(reopened.store, 'https://runner.example:8765', FP_B, 2000));
const overwritten = loadPinStore(pinFile).store;
check('overwrite replaces the fingerprint', overwritten['https://runner.example:8765'].sha256 === FP_B);
check('overwrite keeps one pin per origin', Object.keys(overwritten).length === 1);
check('overwrite updates addedAt', overwritten['https://runner.example:8765'].addedAt === 2000);
check('atomic write leaves no temp file', fs.readdirSync(dir).join() === 'trusted-pins.json');

fs.writeFileSync(
  pinFile,
  JSON.stringify({
    'https://Dup.Example:8765': { sha256: FP_A, addedAt: 2000 },
    'https://dup.example:8765': { sha256: FP_B, addedAt: 1000 },
  }),
);
check('newest pin wins when origins normalize together', loadPinStore(pinFile).store['https://dup.example:8765'].sha256 === FP_A);

fs.writeFileSync(pinFile, '{"broken": seen');
const corrupt = loadPinStore(pinFile);
check('a corrupt pin store is reported, not silently empty', corrupt.status === 'unreadable' && corrupt.error !== null);
check('a corrupt pin store yields no pins', Object.keys(corrupt.store).length === 0);

fs.writeFileSync(pinFile, JSON.stringify({ 'https://bad.example:8765': { sha256: 'nope', addedAt: 1 } }));
const malformedEntry = loadPinStore(pinFile);
check('a malformed entry marks the store unreadable', malformedEntry.status === 'unreadable');
check('a malformed entry is not silently adopted', Object.keys(malformedEntry.store).length === 0);

fs.writeFileSync(pinFile, JSON.stringify({ 'legacy-host-only.example': { sha256: FP_A, addedAt: 1 } }));
check('legacy host-only keys are reported unreadable', loadPinStore(pinFile).status === 'unreadable');
check('legacy host-only keys are never adopted', Object.keys(loadPinStore(pinFile).store).length === 0);

fs.writeFileSync(
  pinFile,
  JSON.stringify({
    'https://good.example:8765': { sha256: FP_A, addedAt: 1 },
    'https://bad.example:8765': { sha256: 'nope', addedAt: 1 },
  }),
);
const partial = loadPinStore(pinFile);
check('a partly malformed store keeps its readable pins', partial.store['https://good.example:8765'].sha256 === FP_A);
check('a partly malformed store is still reported', partial.status === 'unreadable');
fs.writeFileSync(pinFile, '{}');
const emptyStore = loadPinStore(pinFile);
check('an empty object is a readable empty store', emptyStore.status === 'ok' && Object.keys(emptyStore.store).length === 0);

const unreadable = loadPinStore(dir);
check('an unreadable pin store path is reported', unreadable.status === 'unreadable' && unreadable.error !== null);

const nestedPins = path.join(dir, 'nested', 'trusted-pins.json');
writePinStore(nestedPins, setPin({}, 'https://runner.example:8765', FP_A, 1));
check('pin store creates its parent directory', fs.existsSync(nestedPins));

const recoveryFile = path.join(dir, 'recovery-pins.json');
const recoveryOrigin = 'https://runner.example:8765';
fs.writeFileSync(recoveryFile, '{"broken": seen');
const refused = writePinSession(recoveryFile, { store: { [recoveryOrigin]: { sha256: FP_A, addedAt: 1 } }, unreadable: null, announcedUnreadable: false }, recoveryOrigin, FP_B, 2);
check('a trust attempt on an unreadable store is refused', refused.persisted === false);
check('the refusal keeps the write fail-closed', refused.persisted === false && refused.session.unreadable !== null);
check('an unreadable store yields no pins, leaving the prompt to decide', Object.keys(refused.session.store).length === 0);
check('the unreadable store is announced once', refused.notice !== null);
check('the unreadable file is left exactly as it was', fs.readFileSync(recoveryFile, 'utf8') === '{"broken": seen');
const refusedAgain = writePinSession(recoveryFile, refused.session, recoveryOrigin, FP_B, 3);
check('a later attempt is still refused', refusedAgain.persisted === false);
check('the announcement is not repeated in the same session', refusedAgain.notice === null);
fs.writeFileSync(recoveryFile, '{}');
const recovered = writePinSession(recoveryFile, refusedAgain.session, recoveryOrigin, FP_B, 4);
check('a repaired store accepts the next attempt without a restart', recovered.persisted === true);
check('the repaired store holds the new pin', loadPinStore(recoveryFile).store[recoveryOrigin].sha256 === FP_B);
check('the repaired store reads cleanly', loadPinStore(recoveryFile).status === 'ok');
check('a readable store is never announced', readPinSession(recoveryFile, recovered.session).notice === null);

const readFirst = readPinSession(recoveryFile, { store: {}, unreadable: null, announcedUnreadable: false });
check('a readable read yields no announcement', readFirst.notice === null && readFirst.session.unreadable === null);
fs.writeFileSync(recoveryFile, '{"broken": seen');
const readBroken = readPinSession(recoveryFile, readFirst.session);
check('an unreadable read is announced', readBroken.notice !== null && readBroken.session.unreadable !== null);
check('the announcement latches for the process', readPinSession(recoveryFile, readBroken.session).notice === null);
check('an unreadable read yields no pins', Object.keys(readBroken.session.store).length === 0);

check('malformed fingerprint is refused', throwsWith(() => setPin({}, 'https://runner.example:8765', 'sha256/nope', 1), 'malformed'));
check('non-origin endpoint is refused', throwsWith(() => setPin({}, 'runner.example', FP_A, 1), 'non-origin endpoint'));
check('credentialed endpoint is refused', throwsWith(() => setPin({}, 'https://user:pass@runner.example:8765', FP_A, 1), 'non-origin endpoint'));
check('colon-separated fingerprints are normalized', setPin({}, 'https://runner.example:8765', FP_A.replace(/(..)/g, '$1:').slice(0, -1), 1)['https://runner.example:8765'].sha256 === FP_A);
check('existing pins are preserved on set', setPin({ 'https://other.example:8765': { sha256: FP_B, addedAt: 5 } }, 'https://runner.example:8765', FP_A, 6)['https://other.example:8765'].sha256 === FP_B);

const PORT_A = 'https://runner.example:8765';
const PORT_B = 'https://runner.example:9443';
const oneOrigin = setPin({}, PORT_A, FP_A, 1);
const twoOrigins = setPin(oneOrigin, PORT_B, FP_B, 2);

check('exact origin with a matching pin is accepted', certificateDecision(oneOrigin, PORT_A, 'runner.example', FP_A) === 'pinned');
check('exact origin lookup ignores host case', certificateDecision(oneOrigin, 'https://Runner.Example:8765', 'Runner.Example', FP_A) === 'pinned');
check('a different port on the pinned host is refused', certificateDecision(oneOrigin, PORT_B, 'runner.example', FP_A) === 'default');
check('a different scheme at the pinned host and port is refused', certificateDecision(oneOrigin, 'http://runner.example:8765', 'runner.example', FP_A) === 'default');
check('a mismatched fingerprint on the pinned origin is refused', certificateDecision(oneOrigin, PORT_A, 'runner.example', FP_B) === 'default');
check('each port keeps its own pin', twoOrigins[PORT_A].sha256 === FP_A && twoOrigins[PORT_B].sha256 === FP_B);
check('the second port does not replace the first', Object.keys(twoOrigins).length === 2);
check('port A accepts only its own pin', certificateDecision(twoOrigins, PORT_A, 'runner.example', FP_A) === 'pinned');
check('port B accepts its own pin', certificateDecision(twoOrigins, PORT_B, 'runner.example', FP_B) === 'pinned');
check('port B does not accept port A fingerprint', certificateDecision(twoOrigins, PORT_B, 'runner.example', FP_A) === 'default');
check('an untrusted port on a pinned host falls through', certificateDecision(twoOrigins, 'https://runner.example:8443', 'runner.example', FP_A) === 'default');
check('an unrelated origin falls through', certificateDecision(oneOrigin, 'https://other.example:8765', 'other.example', FP_A) === 'default');
check('a hostname that does not match the pinned origin is refused', certificateDecision(oneOrigin, PORT_A, 'other.example', FP_A) === 'default');
check('unfingerprintable certificate falls through', certificateDecision(oneOrigin, PORT_A, 'runner.example', '') === 'default');
check('a bare hostname can never satisfy the decision', certificateDecision(oneOrigin, 'runner.example', 'runner.example', FP_A) === 'default');
check('a scheme-only endpoint can never satisfy the decision', certificateDecision(oneOrigin, 'https://runner.example', 'runner.example', FP_A) === 'default');
check('a scheme-less host:port can never satisfy the decision', certificateDecision(oneOrigin, 'runner.example:8765', 'runner.example', FP_A) === 'default');
check('IPv6 brackets are ignored when matching', certificateDecision(setPin({}, 'https://[::1]:8765', FP_A, 1), 'https://[::1]:8765', '::1', FP_A) === 'pinned');
check('a legacy host-only stored key never matches', certificateDecision({ 'runner.example': { sha256: FP_A, addedAt: 1 } }, PORT_A, 'runner.example', FP_A) === 'default');
check('a pinned https origin covers the same host:port WebSocket', certificateDecision(oneOrigin, 'wss://runner.example:8765/ws', 'runner.example', FP_A) === 'pinned');
check('a WebSocket with a mismatched fingerprint is refused', certificateDecision(oneOrigin, 'wss://runner.example:8765/ws', 'runner.example', FP_B) === 'default');
check('a WebSocket to an untrusted port falls through', certificateDecision(oneOrigin, 'wss://runner.example:9443/ws', 'runner.example', FP_A) === 'default');
check('a secure WebSocket pin is stored under the https origin', setPin({}, 'wss://runner.example:8765/ws', FP_A, 1)['https://runner.example:8765'].sha256 === FP_A);

const fixtureBody =
  'MIIDHTCCAgWgAwIBAgIUYSnnS+RbRnjYYf74q9u1PH6HlvcwDQYJKoZIhvcNAQELBQAwHjEcMBoGA1UEAwwTcnBpLWRlc2t0b3AtZml4dHVyZTAeFw0yNjA5MTcwNjMwNDVaFw0zNjA5MTQwNjMwNDVaMB4xHDAaBgNVBAMME3JwaS1kZXNrdG9wLWZpeHR1cmUwggEiMA0GCSqGSIb3DQEBAQUAA4IBDwAwggEKAoIBAQCGQy0qGk7nDZhJ+BV6yMG/niNqJM5y6yIDW4aqF6PNsrMzNeGn4NSeNpSKXtkdy7IKG5LhNS8JcG086liDr6zzZkqXWS38jvMmNX5DFggaQ+L+MOO3sxdNHFOQb7NKbcmD9k+PR68WgjtH+bp8fN9TuXggxs/9spVxgj9431FGTeBBQl0aTee+b+A8qist1Brt2fHckh9fY4TyXhLaP+1dm0Jgi8BkTZ2QLlMjnIfIDVhM7q9aGR+iS6CiAUXgVQq5AuPuvNVuDem4Rvh+wSavEa0rYqCfD4E9TvB3m/bG7NszxNbl8EgyuQLtRhjLpUdVvE/SI7tfGC0HMARQCWMFAgMBAAGjUzBRMB0GA1UdDgQWBBR45dvrE8ltjcNsYBwCTTLjGL/E8TAfBgNVHSMEGDAWgBR45dvrE8ltjcNsYBwCTTLjGL/E8TAPBgNVHRMBAf8EBTADAQH/MA0GCSqGSIb3DQEBCwUAA4IBAQBi/E8zNo/CBPlQF9fnyBpvl8NlozvYRkpBQhBhPArlQ4t2X3vZDfkhVVtAse5BNNLfMfkVvPiDAF62NIin9C08umBWqn5X1QPsnNShZXPk55SnGTPhZspVg+ASFNX9YtNu6TR09pW/FIDfFNexqGW1Ekq8obAe3vwe1BEg/XnRnAyAqQDLYK5S9i78xsyUdDe8Ar+hcmw+gr8HJQMIlrLh5eBYSLZrurN7CEOUPVhRtJn1ZIhqVGn0v3lMecqomzmovx0AE7vb/WGHgX5llC2866d+Mb1hIU8uYkqU+7alZ3HkykcTXUr/7JgJ2SLGOCclnmk0vH65NtCFMpZaM1Lk';
const fixtureFingerprint = 'a8689076a54b7232f5373c586365ed09bd3da3ee7f856cc21296c2374ab0156a';
check('PEM fingerprint matches the certificate DER SHA-256', sha256FingerprintFromPem(`-----BEGIN CERTIFICATE-----\n${fixtureBody}\n-----END CERTIFICATE-----\n`) === fixtureFingerprint);
check('PEM fingerprint survives re-wrapped lines', sha256FingerprintFromPem(`-----BEGIN CERTIFICATE-----\n${fixtureBody.replace(/(.{64})/g, '$1\n')}-----END CERTIFICATE-----`) === fixtureFingerprint);
check('empty PEM yields no fingerprint', sha256FingerprintFromPem('-----BEGIN CERTIFICATE-----\n-----END CERTIFICATE-----') === '');
check('missing PEM yields no fingerprint', sha256FingerprintFromPem('') === '');

check('same-origin runner page is allowed', sameOrigin(`${RUNNER_ORIGIN}/web`, RUNNER_ORIGIN) === true);
check('same-origin runner route is allowed', sameOrigin(`${RUNNER_ORIGIN}/chat`, RUNNER_ORIGIN) === true);
check('different port is refused', sameOrigin('https://127.0.0.1:8766/web', RUNNER_ORIGIN) === false);
check('different host is refused', sameOrigin('https://runner.example/web', RUNNER_ORIGIN) === false);
check('subdomain of the runner host is refused', sameOrigin('https://evil.127.0.0.1:8765/web', RUNNER_ORIGIN) === false);
check('lookalike host is refused', sameOrigin('https://127.0.0.1.evil.test:8765/web', RUNNER_ORIGIN) === false);
check('downgrade to http is refused', sameOrigin('http://127.0.0.1:8765/web', RUNNER_ORIGIN) === false);
check('external origin is refused', sameOrigin('https://example.test/', RUNNER_ORIGIN) === false);
check('opaque origin is refused', sameOrigin('data:text/html,hello', RUNNER_ORIGIN) === false);
check('about:blank is refused', sameOrigin('about:blank', RUNNER_ORIGIN) === false);
check('malformed url is refused', sameOrigin('not a url', RUNNER_ORIGIN) === false);

check('microphone is allowed from the runner origin', permissionAllowed('media', `${RUNNER_ORIGIN}/web`, RUNNER_ORIGIN) === true);
check('notifications are allowed from the runner origin', permissionAllowed('notifications', `${RUNNER_ORIGIN}/chat`, RUNNER_ORIGIN) === true);
check('clipboard write is allowed from the runner origin', permissionAllowed('clipboard-sanitized-write', `${RUNNER_ORIGIN}/web`, RUNNER_ORIGIN) === true);
check('microphone is refused from another origin', permissionAllowed('media', 'https://example.test/web', RUNNER_ORIGIN) === false);
check('notifications are refused from another origin', permissionAllowed('notifications', 'https://example.test/web', RUNNER_ORIGIN) === false);
check('microphone is refused from a lookalike origin', permissionAllowed('media', 'https://127.0.0.1.evil.test:8765/web', RUNNER_ORIGIN) === false);
check('geolocation is refused even from the runner origin', permissionAllowed('geolocation', `${RUNNER_ORIGIN}/web`, RUNNER_ORIGIN) === false);
check('unlisted permission is refused', permissionAllowed('midi', `${RUNNER_ORIGIN}/web`, RUNNER_ORIGIN) === false);
check('camera requests are refused at the request layer', permissionAllowed('media', `${RUNNER_ORIGIN}/web`, RUNNER_ORIGIN, ['video']) === false);
check('camera and microphone together are refused', permissionAllowed('media', `${RUNNER_ORIGIN}/web`, RUNNER_ORIGIN, ['audio', 'video']) === false);
check('audio-only media is allowed', permissionAllowed('media', `${RUNNER_ORIGIN}/web`, RUNNER_ORIGIN, ['audio']) === true);
check('an unknown media subtype is allowed', permissionAllowed('media', `${RUNNER_ORIGIN}/web`, RUNNER_ORIGIN, ['unknown']) === true);
check('media without reported types is allowed', permissionAllowed('media', `${RUNNER_ORIGIN}/web`, RUNNER_ORIGIN, undefined) === true);

check('a runner-origin download is allowed', sameOrigin(`${RUNNER_ORIGIN}/session/one.md`, RUNNER_ORIGIN) === true);
check('a blob download created by the runner is allowed', sameOrigin(`blob:${RUNNER_ORIGIN}/9d1f-4c`, RUNNER_ORIGIN) === true);
check('a foreign blob download is refused', sameOrigin('blob:https://evil.test/9d1f-4c', RUNNER_ORIGIN) === false);
check('a foreign-origin download is refused', sameOrigin('https://evil.test/file.md', RUNNER_ORIGIN) === false);
check('a data: download is refused', sameOrigin('data:application/octet-stream;base64,QUJD', RUNNER_ORIGIN) === false);
check('a download with no URL is refused', sameOrigin('', RUNNER_ORIGIN) === false);

check('https links open externally', externalSchemeAllowed('https://example.test/x') === true);
check('http links open externally', externalSchemeAllowed('http://example.test/x') === true);
check('mailto links open externally', externalSchemeAllowed('mailto:runner@example.test') === true);
check('file links stay in the shell', externalSchemeAllowed('file:///etc/passwd') === false);
check('javascript links stay in the shell', externalSchemeAllowed('javascript:alert(1)') === false);
check('malformed links stay in the shell', externalSchemeAllowed('%%%') === false);

check('runner-origin frame is allowed', navigationAllowed(`${RUNNER_ORIGIN}/assets/preview.html`, RUNNER_ORIGIN, false) === true);
check('runner-origin root frame is allowed', navigationAllowed(`${RUNNER_ORIGIN}/web`, RUNNER_ORIGIN, false) === true);
check('about:blank frame is allowed', navigationAllowed('about:blank', RUNNER_ORIGIN, false) === true);
check('about:srcdoc frame is allowed', navigationAllowed('about:srcdoc', RUNNER_ORIGIN, false) === true);
check('data: frame is refused', navigationAllowed('data:text/html,<p>remote</p>', RUNNER_ORIGIN, false) === false);
check('arbitrary data:text/html frame is refused', navigationAllowed(`data:text/html;base64,${Buffer.from('<script>fetch("https://evil.test")</script>').toString('base64')}`, RUNNER_ORIGIN, false) === false);
check('runner-origin blob: frame is allowed', navigationAllowed(`blob:${RUNNER_ORIGIN}/9d1f-4c`, RUNNER_ORIGIN, false) === true);
check('foreign-origin blob: frame is refused', navigationAllowed('blob:https://example.test/9d1f-4c', RUNNER_ORIGIN, false) === false);
check('blob: frame with a downgraded runner origin is refused', navigationAllowed('blob:http://127.0.0.1:8765/9d1f-4c', RUNNER_ORIGIN, false) === false);
check('blob: frame with a lookalike origin is refused', navigationAllowed('blob:https://127.0.0.1.evil.test:8765/9d1f-4c', RUNNER_ORIGIN, false) === false);
check('opaque-origin blob: frame is refused', navigationAllowed('blob:null/9d1f-4c', RUNNER_ORIGIN, false) === false);
check('frame on another host is refused', navigationAllowed('https://example.test/frame', RUNNER_ORIGIN, false) === false);
check('frame on another port is refused', navigationAllowed('https://127.0.0.1:8766/frame', RUNNER_ORIGIN, false) === false);
check('http downgrade of the runner is refused', navigationAllowed('http://127.0.0.1:8765/frame', RUNNER_ORIGIN, false) === false);
check('lookalike subdomain frame is refused', navigationAllowed('https://127.0.0.1.evil.test:8765/frame', RUNNER_ORIGIN, false) === false);
check('subdomain of the runner host is refused', navigationAllowed('https://evil.127.0.0.1:8765/frame', RUNNER_ORIGIN, false) === false);
check('file frame is refused', navigationAllowed('file:///etc/passwd', RUNNER_ORIGIN, false) === false);
check('about:config frame is refused', navigationAllowed('about:config', RUNNER_ORIGIN, false) === false);
check('malformed frame url is refused', navigationAllowed('not a url', RUNNER_ORIGIN, false) === false);
check('empty frame url is refused', navigationAllowed('', RUNNER_ORIGIN, false) === false);

check('top-level runner route is allowed', navigationAllowed(`${RUNNER_ORIGIN}/chat`, RUNNER_ORIGIN, true) === true);
check('top-level data: navigation is refused', navigationAllowed('data:text/html,<p>remote</p>', RUNNER_ORIGIN, true) === false);
check('top-level runner-origin blob: navigation is refused', navigationAllowed(`blob:${RUNNER_ORIGIN}/9d1f-4c`, RUNNER_ORIGIN, true) === false);
check('top-level about:blank navigation is refused', navigationAllowed('about:blank', RUNNER_ORIGIN, true) === false);
check('top-level foreign navigation is refused', navigationAllowed('https://example.test/', RUNNER_ORIGIN, true) === false);

check(
  'chat document offers the user-added runner schemes',
  CHAT_CONTENT_SECURITY_POLICY ===
    "default-src 'self'; script-src 'self' 'unsafe-inline' 'unsafe-eval'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self' data:; connect-src 'self' ws: wss: http: https:; media-src 'self' blob: data:; frame-src 'self' data: blob:; object-src 'none'; base-uri 'self'; form-action 'self'",
);
const webPolicy = contentSecurityPolicyFor('/web', RUNNER_ORIGIN);
check('web document keeps the multi-runner policy out', webPolicy.includes("connect-src 'self' wss://127.0.0.1:8765;"));
check('web document rejects any-origin ws scheme sources', !webPolicy.includes('ws: wss:') && !webPolicy.includes("'self' ws:"));
check('web document rejects cross-origin http sources', !webPolicy.includes('http: https:'));
check('web document derives ws: for a plaintext runner', contentSecurityPolicyFor('/web', 'http://127.0.0.1:8765').includes("connect-src 'self' ws://127.0.0.1:8765;"));
check('chat document keeps the multi-runner policy', contentSecurityPolicyFor('/chat', RUNNER_ORIGIN) === CHAT_CONTENT_SECURITY_POLICY);
check('collab document does not get the multi-runner policy', !contentSecurityPolicyFor('/collab/ws/room-1', RUNNER_ORIGIN).includes("connect-src 'self' ws: wss:"));
check('collab document is runner-scoped like web', contentSecurityPolicyFor('/collab/ws/room-1', RUNNER_ORIGIN) === webPolicy);
check('web policy differs only in connect-src', webPolicy.replace("'self' wss://127.0.0.1:8765;", "'self' ws: wss: http: https:;") === CHAT_CONTENT_SECURITY_POLICY);
check('main frame responses carry a document policy', isDocumentResource('mainFrame') === true);
check('sub frame responses carry a document policy', isDocumentResource('subFrame') === true);
check('script responses carry no document policy', isDocumentResource('script') === false);
check('stylesheet responses carry no document policy', isDocumentResource('stylesheet') === false);
check('xhr responses carry no document policy', isDocumentResource('xhr') === false);
check('webSocket handshakes carry no document policy', isDocumentResource('webSocket') === false);
check('image responses carry no document policy', isDocumentResource('image') === false);
check('fetch/other responses carry no document policy', isDocumentResource('other') === false);
const patched = applyContentSecurityPolicy({ 'Content-Type': ['text/html'], 'content-security-policy': ["default-src 'none'"] }, CHAT_CONTENT_SECURITY_POLICY);
check('unrelated headers survive CSP injection', patched['Content-Type'][0] === 'text/html');
check('CSP is injected exactly once', Object.keys(patched).filter((name) => name.toLowerCase() === 'content-security-policy').length === 1);
check('injected CSP replaces the listener policy', patched['Content-Security-Policy'][0] === CHAT_CONTENT_SECURITY_POLICY);
check('undefined header values are dropped', Object.keys(applyContentSecurityPolicy({ 'x-empty': undefined }, CHAT_CONTENT_SECURITY_POLICY)).length === 1);
check('CSP injection tolerates missing headers', applyContentSecurityPolicy(undefined, CHAT_CONTENT_SECURITY_POLICY)['Content-Security-Policy'][0] === CHAT_CONTENT_SECURITY_POLICY);

const settingsFile = path.join(dir, 'settings.json');
writeSettings(settingsFile, { runnerUrl: 'https://runner.example:8765', route: 'chat' });
check('settings round-trip', JSON.stringify(readSettings(settingsFile)) === JSON.stringify({ runnerUrl: 'https://runner.example:8765', route: 'chat' }));
fs.writeFileSync(settingsFile, JSON.stringify({ runnerUrl: 7, route: 'collab' }));
check('invalid settings fields are dropped', JSON.stringify(readSettings(settingsFile)) === '{}');
check('missing settings file reads empty', JSON.stringify(readSettings(path.join(dir, 'no-settings.json'))) === '{}');

const stateFile = path.join(dir, 'window-state.json');
writeWindowState(stateFile, { width: 900, height: 700, x: 10, y: 20, maximized: true });
check('window state round-trip', JSON.stringify(readWindowState(stateFile)) === JSON.stringify({ width: 900, height: 700, x: 10, y: 20, maximized: true }));
fs.writeFileSync(stateFile, JSON.stringify({ width: 'wide', x: null, maximized: 'yes' }));
check('garbage window state is dropped', JSON.stringify(readWindowState(stateFile)) === '{}');

const primary = { x: 0, y: 0, width: 1920, height: 1080 };
const secondary = { x: 1920, y: 0, width: 1280, height: 1024 };
const displays = [primary, secondary];

const offscreen = clampWindowBounds({ width: 1000, height: 700, x: 9000, y: 9000, maximized: false }, displays, primary);
check('offscreen position is centred on the primary display', offscreen.x === 460 && offscreen.y === 190);
check('offscreen restore keeps the saved size', offscreen.width === 1000 && offscreen.height === 700);
const negative = clampWindowBounds({ width: 1000, height: 700, x: -400, y: -100, maximized: false }, displays, primary);
check('negative position is clamped inside the display', negative.x === 0 && negative.y === 0);
const onSecondary = clampWindowBounds({ width: 800, height: 600, x: 2000, y: 100, maximized: false }, displays, primary);
check('position on a secondary display is kept', onSecondary.x === 2000 && onSecondary.y === 100);
const oversized = clampWindowBounds({ width: 5000, height: 3000, x: 10, y: 10, maximized: false }, displays, primary);
check('oversized window is shrunk to the display', oversized.width === 1920 && oversized.height === 1080);
check('shrunk window is moved inside the display', oversized.x === 0 && oversized.y === 0);
const undersized = clampWindowBounds({ width: 10, height: 10, x: 100, y: 100, maximized: false }, displays, primary);
check('undersized window is raised to the minimum', undersized.width === MIN_WINDOW_SIZE.width && undersized.height === MIN_WINDOW_SIZE.height);
const missing = clampWindowBounds(null, displays, primary);
check('missing state uses the default size', missing.width === DEFAULT_WINDOW_SIZE.width && missing.height === DEFAULT_WINDOW_SIZE.height);
check('missing state is centred on the primary display', missing.x === Math.round((1920 - DEFAULT_WINDOW_SIZE.width) / 2) && missing.y === Math.round((1080 - DEFAULT_WINDOW_SIZE.height) / 2));
const nonFinite = clampWindowBounds({ width: Number.NaN, height: Number.POSITIVE_INFINITY, x: Number.NaN, y: 20 }, displays, primary);
check('non-finite state falls back to defaults', nonFinite.width === DEFAULT_WINDOW_SIZE.width && nonFinite.height === DEFAULT_WINDOW_SIZE.height);
check('non-finite position is centred', nonFinite.x === Math.round((1920 - DEFAULT_WINDOW_SIZE.width) / 2));
const maximized = clampWindowBounds({ maximized: true }, displays, primary);
check('maximized flag survives clamping', maximized.maximized === true);
check('absent maximized flag is false', clampWindowBounds(null, displays, primary).maximized === false);
const noDisplays = clampWindowBounds({ width: 800, height: 600, x: 40, y: 40, maximized: false }, [], primary);
check('no connected displays keeps the size', noDisplays.width === 800 && noDisplays.height === 600);
check('no connected displays drops the position', noDisplays.x === 0 && noDisplays.y === 0);

const errorPage = errorPageHtml({
  failingUrl: 'https://127.0.0.1:8765/web"><script>alert(1)</script>',
  description: 'ERR_CERT_AUTHORITY_INVALID',
  errorCode: -202,
  runnerUrl: RUNNER_ORIGIN,
  route: 'web',
});
check('error page escapes the failing URL', !errorPage.includes('<script>'));
check('error page reports the failure description', errorPage.includes('ERR_CERT_AUTHORITY_INVALID'));
check('error page reports the error code', errorPage.includes('-202'));
check('error page names the retry URL', errorPage.includes(`${RUNNER_ORIGIN}/web`));
check('error page names the certificate action', errorPage.includes('Trust current certificate'));
check('error page names the runner URL action', errorPage.includes('Set runner URL'));
const chatErrorPage = errorPageHtml({ failingUrl: 'https://127.0.0.1:8765/chat', description: 'ERR_CONNECTION_REFUSED', errorCode: -102, runnerUrl: RUNNER_ORIGIN, route: 'chat' });
check('error page retries the selected route', chatErrorPage.includes(`${RUNNER_ORIGIN}/chat`));

const promptPage = promptPageHtml('Set runner URL', 'https://127.0.0.1:8765" onfocus="alert(1)');
check('prompt escapes quotes in the initial value', promptPage.includes('https://127.0.0.1:8765&quot; onfocus=&quot;alert(1)'));
check('prompt echoes the current runner URL', promptPageHtml('Set runner URL', RUNNER_ORIGIN).includes(`value="${RUNNER_ORIGIN}"`));
check('prompt escapes its title', promptPageHtml('<b>title</b>', RUNNER_ORIGIN).includes('<title>&lt;b&gt;title&lt;/b&gt;</title>'));

fs.rmSync(dir, { recursive: true, force: true });

console.log(`\nhelpers.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);
