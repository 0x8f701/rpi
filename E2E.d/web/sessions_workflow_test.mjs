// Web workflow DAG acceptance lane — PLAYWRIGHT-ONLY (E2E.d/web/sessions_workflow.sh).
//
// Environment:
//   RPI_URL         http://127.0.0.1:<port>/web
//   RPI_TOKEN       token file content (served via rpi-auth.<token> subprotocol)
//   RPI_CHROME      executable path of the system Chrome (optional)
//   RPI_EVIDENCE    evidence dir for screenshots + executed-assertion evidence
//   RPI_MOCK_PORT   loopback mock provider control port (worker hold/release)
//
// Dedicated, RPC-only acceptance flow for the pending "healthy Workflow DAG
// completion + fail-closed" contract (previously embedded in the multi-session
// lane's T5.3): every workflow interaction goes through workflow_* RPC frames
// on short-lived subprotocol sockets — no composer prompts, no streaming
// races. Requires the MultiSessionRuntimeManager backend (top-level sessionId
// routing; lifecycle responses carry {sessionId,...}).
//
// Mock scenario: `sessions` (E2E.d/lib/user_mock_server.py) — supervisor
// planning ("You plan workflow <id>. Objective: ...") commits the exact
// two-task DAG compile -> ship and delegated workers are held open until
// POST /__release-session-workers; the objective "sessions-fail-closed-
// objective" deterministically produces NO plan (planning rejection text).
//
// Assertion matrix (feature -> IDs, recorded as machine-readable evidence):
//   WF0 WF0.1 boot page title; WF0.2 token connect reaches connected;
//        WF0.3 primary session row; WF0.4 RPC-created session B row
//   WF1 WF1.1 fail-closed probe lands terminal failed
//        WF1.2 failure retains its non-empty reason (planning produced no
//              tasks) — never hangs, never false-passes as completed
//   WF2 WF2.1 first dependency wave authoritatively held
//        WF2.2 held DAG is live progress, never completed (no false-pass
//              status inference); compile running, ship pending with the
//              exact dependsOn edge
//        WF2.3 dependent ship wave becomes held after compile completion
//        WF2.4 released DAG reaches the authoritative completed state with
//              an applied:<commit> integration and full identity
//        WF2.5 terminal DAG: exactly two tasks, both completed, ownership
//              rows exact-once
//   WF3 WF3.1 second healthy workflow in session B completes healthily
//        WF3.2 workflow_list never crosses session ownership
//   WF4 WF4.1 mock-side counts match exactly-once (planning 6, workers 4,
//              completed 4, held 0, per-workflow assignment counts 1+1)
//        WF4.2 completed is terminal: re-polls keep status completed with
//              the same applied integration (no status inference flip)
//   WF5 WF5.1 real WS-level response hold armed for the EXACT workflowId:
//              the pre-click workflow_detail is physically delivered and its
//              response withheld (request ID tracked in the in-flight set);
//              the EXACT outgoing workflow_cancel request id and the
//              subsequent application-socket workflow_list id are recorded
//              and BOTH exact success responses are awaited while the old
//              detail response stays physically held; the authoritative list
//              generation/status is parsed from the list payload; releasing
//              the held response yields EXACTLY ONE new current-generation
//              request with a fresh ID whose exact success response carries
//              identity/generation/status matching the authoritative list;
//              no panel error rows, no toasts
//        WF5.2 the panel's exact Remove button deletes the workflow: with
//              the pre-click detail response held, the exact workflow_remove
//              request id + the subsequent application-socket workflow_list
//              settle with success responses while the held response stays
//              parked; the authoritative list no longer contains the id;
//              after release ZERO new workflow_detail requests/responses
//              ever fire (the stale response is silently discarded by
//              tombstone + request epoch), the row disappears, no "workflow
//              was not found" surfaces anywhere
//        WF5.3 removed/terminal UI verified at desktop and mobile viewport
//              widths with evidence screenshots
//
// Recording discipline: assertion IDs are recorded EXPLICITLY (record(id) at
// each assertion site) — never implicitly from waitFor labels. Every
// documented ID must execute EXACTLY ONCE (a duplicate or unexpected record
// is a hard failure) and no documented ID may stay missing. Browser
// console.error/pageerror are captured and their counts/messages are
// published with the counts.
//
// Run-scoped staging: every artifact this run creates (screenshots, the
// coverage assertions JSON) is written into a UNIQUE staging dir under the
// evidence root. Existing published evidence is NEVER touched during the run
// or on failure — a failed run removes ONLY its staging dir. Artifacts are
// REGISTERED only after their write/screenshot actually succeeded, so a
// failed write can never enter the published set. A passing run publishes
// ONLY after ALL gates (assertion coverage + browser-error) pass, and the
// publication itself is set-atomic: ONE immutable run directory
// ($RPI_EVIDENCE/runs/web-sessions-workflow-<nonce>/ holding every artifact
// plus manifest.json; one invocation nonce derives staging dir, run dir, and
// pointer) appears via a single directory rename, and ONE
// atomically-renamed manifest/current pointer ($RPI_EVIDENCE/current.json)
// and canonical artifact names are never replaced sequentially. The
// publication helper validates the staged set EXACTLY (unique declared
// names, normalized relative paths contained in the run dir, lstat regular
// files only — no symlinks — nothing missing or extra) inside its rollback
// scope and TRACKS every path it creates (staging dir, renamed run dir,
// pointer temp file): a fault or validation failure BEFORE the pointer
// commit removes all three and preserves the previous pointer
// byte-identical (the fault-injection self-check injects at manifest write,
// stage->run rename, pointer write, and pointer rename, and rejects
// invalid/duplicate/missing/extra/symlink/traversal staged sets, against a
// throwaway evidence root). On full success the lane prints the PASSED
// line; any failure exits 2.
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_SELF_CHECK === '1'
  // Self-check mode runs the pure gates AND the fault-injection publication
  // self-check against a THROWAWAY evidence root, so the real publication
  // helper (staging dir, run-dir rename, pointer tmp + commit) is exercised
  // without touching any operator evidence directory.
  ? fs.mkdtempSync(path.join(os.tmpdir(), 'web-sessions-workflow-selfcheck-'))
  : (process.env.RPI_EVIDENCE || '.');
const mockPort = process.env.RPI_MOCK_PORT || '';

const DOCUMENTED_IDS = [
  'WF0.1', 'WF0.2', 'WF0.3',
  'WF0.4',
  'WF1.1', 'WF1.2',
  'WF2.1', 'WF2.2', 'WF2.3', 'WF2.4', 'WF2.5',
  'WF3.1', 'WF3.2',
  'WF4.1', 'WF4.2',
  'WF5.1', 'WF5.2', 'WF5.3',
];
// Explicit per-ID execution counts: record(id) is called ONLY at assertion
// sites (never implicitly from waitFor labels), so the counts Map is the
// authoritative exact-once ledger — DOCUMENTED_IDS is declared in the lane's
// ACTUAL execution order, every documented ID must land at exactly 1 in that
// exact insertion order, and any other key is an unexpected record.
const executedCounts = new Map();
function record(id) {
  if (!DOCUMENTED_IDS.includes(id)) {
    fail(`record: unexpected assertion id ${JSON.stringify(id)} (not in DOCUMENTED_IDS)`);
  }
  const count = (executedCounts.get(id) || 0) + 1;
  executedCounts.set(id, count);
  if (count !== 1) {
    fail(`record: assertion id ${id} recorded ${count}x — every documented id must execute exactly once`);
  }
  console.log(`[web-wf:assert] ${id} (#${count})`);
}

// ONE invocation nonce derives the staging dir, the run dir, and the
// pointer, so no two publications can ever mix generations.
const laneNonce = `${process.pid}-${Date.now()}`;
const stagingDir = path.join(evidence, `.staging-web-sessions-workflow-${laneNonce}`);
const runDirName = `runs/web-sessions-workflow-${laneNonce}`;
const laneArtifacts = []; // registered artifact names, AFTER their write succeeded
let published = false;

async function stageShot(page, name) {
  fs.mkdirSync(stagingDir, { recursive: true });
  const staged = path.join(stagingDir, name);
  await page.screenshot({ path: staged, fullPage: true });
  laneArtifacts.push(name);
  return staged;
}

function stageWrite(name, data) {
  fs.mkdirSync(stagingDir, { recursive: true });
  const staged = path.join(stagingDir, name);
  fs.writeFileSync(staged, data);
  laneArtifacts.push(name);
  return staged;
}

function cleanupStaging() {
  fs.rmSync(stagingDir, { recursive: true, force: true });
}

// Exact staged-artifact validation: unique declared names, NORMALIZED
// relative paths contained inside the run dir (no absolute paths, no `..`
// traversal, no `.`/`//` segments), lstat REGULAR files only (a symlink —
// even one pointing at a real file — is rejected), and the EXACT staged set:
// every declared name present, nothing missing, nothing extra. Returns null
// when the staged set is exactly the declared one.
function stagedArtifactSetError(declaredNames, manifestName) {
  const declared = new Set();
  for (const name of [...declaredNames, manifestName]) {
    if (typeof name !== 'string' || name === '') {
      return `declared artifact name is not a non-empty string: ${JSON.stringify(name)}`;
    }
    const normalized = path.posix.normalize(name);
    if (path.posix.isAbsolute(name) || normalized !== name || normalized === '..' || normalized.startsWith('../')) {
      return `declared artifact is not a normalized contained relative path: ${JSON.stringify(name)}`;
    }
    if (declared.has(name)) return `duplicate declared artifact name: ${name}`;
    declared.add(name);
    let stats;
    try {
      stats = fs.lstatSync(path.join(stagingDir, name));
    } catch {
      return `declared artifact missing from the staged set: ${name}`;
    }
    if (!stats.isFile()) {
      return `declared artifact is not a regular file (symlinks/dirs rejected): ${name}`;
    }
  }
  const actual = new Set();
  const walk = (dir, prefix) => {
    for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
      if (entry.isDirectory()) walk(path.join(dir, entry.name), `${prefix}${entry.name}/`);
      else actual.add(`${prefix}${entry.name}`);
    }
  };
  walk(stagingDir, '');
  const missing = [...declared].filter((name) => !actual.has(name));
  const extras = [...actual].filter((name) => !declared.has(name));
  if (missing.length > 0 || extras.length > 0) {
    return `exact staged-set violation: missing=${JSON.stringify(missing)}, unexpected=${JSON.stringify(extras.sort())}`;
  }
  return null;
}

// Set-atomic, failure-clean publication: the manifest is written into the
// staging set, the `runs/` parent is created (so a FRESH evidence root can
// publish), the WHOLE staging directory is renamed ONCE into the immutable
// run directory, and then the single manifest/current pointer is written to
// a temp file and renamed atomically into place. Every publication step —
// the exact staged-artifact validation (unique names, normalized contained
// relative paths, lstat regular files only, exact staged set), the manifest
// write, the stage->run rename, and the pointer tmp write + rename — runs
// INSIDE one rollback scope: the helper TRACKS every path it created
// (staging dir, renamed run dir, pointer temp file) and any failure BEFORE
// the pointer commit removes all three and leaves the previous pointer
// byte-identical — a consumer either sees the previous publication or the
// complete new one, never a partial set, an orphan run dir, or a mixed
// generation. `faultAt` is self-check-ONLY fault injection
// (selfCheckPublicationFaults); production callers never pass it.
function publishStagedArtifacts(manifest, faultAt) {
  fs.mkdirSync(evidence, { recursive: true });
  fs.mkdirSync(path.join(evidence, 'runs'), { recursive: true });
  const runDir = path.join(evidence, runDirName);
  const pointer = {
    lane: 'web-sessions-workflow',
    nonce: laneNonce,
    run: runDirName,
    manifest: 'manifest.json',
    coverage: 'coverage-assertions.json',
    artifacts: [...laneArtifacts, 'manifest.json'],
  };
  const pointerPath = path.join(evidence, 'current.json');
  const pointerTmp = `${pointerPath}.tmp-${process.pid}`;
  let renamed = false;
  try {
    if (faultAt === 'manifest') throw new Error('injected: manifest write');
    fs.writeFileSync(path.join(stagingDir, 'manifest.json'), `${JSON.stringify(manifest, null, 2)}\n`);
    // Exact artifact validation inside the rollback scope: a staged set that
    // is not exactly the declared one (duplicate, missing, extra, symlink,
    // or escaping name) can never be published.
    const artifactError = stagedArtifactSetError(laneArtifacts, 'manifest.json');
    if (artifactError !== null) throw new Error(`exact staged-artifact validation failed: ${artifactError}`);
    if (faultAt === 'rename') throw new Error('injected: stage->run rename');
    fs.renameSync(stagingDir, runDir);
    renamed = true;
    if (faultAt === 'pointer-write') throw new Error('injected: pointer write');
    fs.writeFileSync(pointerTmp, `${JSON.stringify(pointer, null, 2)}\n`);
    if (faultAt === 'pointer-rename') throw new Error('injected: pointer rename');
    fs.renameSync(pointerTmp, pointerPath);
  } catch (err) {
    // Pre-pointer-commit failure: remove THIS attempt's staging dir, renamed
    // run dir, and pointer temp file; the previous pointer (if any) keeps
    // naming the previous complete publication.
    fs.rmSync(stagingDir, { recursive: true, force: true });
    if (renamed) fs.rmSync(runDir, { recursive: true, force: true });
    fs.rmSync(pointerTmp, { force: true });
    throw err;
  }
  published = true;
}

function fail(message) {
  // Exit 2 (not 1): run.sh treats 1 as "npm install unavailable -> fall
  // back", which is FORBIDDEN for this lane. Any non-zero exit fails it.
  console.error(`web-sessions-workflow: FAIL: ${message}`);
  // A failed run removes ONLY its own staging dir; published evidence under
  // the evidence root is left untouched (never published early/partially).
  cleanupStaging();
  process.exit(2);
}

const WORKFLOW_PROGRESS_STATUSES = new Set(['queued', 'planning', 'running', 'paused', 'integrating']);
const WORKFLOW_FAILURE_STATUSES = new Set(['failed', 'cancelled', 'conflicted']);
const APPLIED_INTEGRATION = /^applied:[0-9A-Za-z][0-9A-Za-z._\/-]*$/;

function workflowFailureReason(workflow) {
  const reason = typeof workflow?.failure === 'string' && workflow.failure.trim() !== ''
    ? workflow.failure.trim()
    : typeof workflow?.integration === 'string' && workflow.integration.trim() !== ''
      ? workflow.integration.trim()
      : 'no surfaced reason';
  return reason.slice(0, 512);
}

// Pure snapshot gate: callers decide whether a terminal error is expected
// (the fail-closed probe) or lane-fatal (the healthy workflows).
function classifyWorkflowSnapshot(workflow, expectedWorkflowId, expectedName) {
  if (!workflow || typeof workflow !== 'object') {
    return { kind: 'terminal-error', message: `snapshot is not an object: ${JSON.stringify(workflow)}` };
  }
  const returnedWorkflowId = typeof workflow.workflowId === 'string' ? workflow.workflowId : workflow.id;
  if (returnedWorkflowId !== expectedWorkflowId) {
    return { kind: 'terminal-error', message: `workflowId mismatch: expected ${expectedWorkflowId}, got ${JSON.stringify(returnedWorkflowId)}` };
  }
  if (workflow.name !== expectedName) {
    return { kind: 'terminal-error', message: `workflow name mismatch: expected ${expectedName}, got ${JSON.stringify(workflow.name)}` };
  }
  if (!Number.isSafeInteger(workflow.generation) || workflow.generation < 1) {
    return { kind: 'terminal-error', message: `invalid workflow generation: ${JSON.stringify(workflow.generation)}` };
  }
  if (WORKFLOW_FAILURE_STATUSES.has(workflow.status)) {
    return { kind: 'terminal-error', message: `terminal ${workflow.status}: ${workflowFailureReason(workflow)}` };
  }
  if (workflow.status === 'completed') {
    if (typeof workflow.integration !== 'string' || !APPLIED_INTEGRATION.test(workflow.integration)) {
      return { kind: 'terminal-error', message: `completed workflow lacks applied:<nonempty hex/ref> integration: ${JSON.stringify(workflow.integration)}` };
    }
    return { kind: 'success' };
  }
  if (WORKFLOW_PROGRESS_STATUSES.has(workflow.status)) return { kind: 'progress' };
  return { kind: 'terminal-error', message: `unknown workflow status ${JSON.stringify(workflow.status)}` };
}

function selfCheckWorkflowClassifier() {
  const base = { workflowId: 'wf-check', name: 'check', generation: 1 };
  const cases = [
    [{ ...base, status: 'running' }, 'progress'],
    [{ ...base, status: 'completed', integration: 'applied:deadbeef' }, 'success'],
    [{ ...base, status: 'completed', integration: 'applied:' }, 'terminal-error'],
    [{ ...base, status: 'weird' }, 'terminal-error'],
    [{ ...base, status: 'failed', failure: 'expected probe failure' }, 'terminal-error'],
    [{ ...base, workflowId: 'foreign', status: 'running' }, 'terminal-error'],
  ];
  for (const [snapshot, expectedKind] of cases) {
    const actual = classifyWorkflowSnapshot(snapshot, base.workflowId, base.name).kind;
    if (actual !== expectedKind) throw new Error(`workflow classifier self-check: expected ${expectedKind}, got ${actual}`);
  }
}

selfCheckWorkflowClassifier();

// Pure inbound-response authority gate over a recorded settled response
// entry: accepted only when the entry id matches the awaited request, the
// recorded inbound command is the EXACT expected command (never a type
// inferred from the outgoing request id), and success is true.
function responseAuthorityError(entry, requestId, expectedCommand) {
  if (!entry || entry.id !== requestId) return `no settled response recorded for request ${requestId}`;
  if (entry.command !== expectedCommand) {
    return `response for ${requestId} carried inbound command ${JSON.stringify(entry.command ?? null)}, expected ${JSON.stringify(expectedCommand)}`;
  }
  if (entry.success !== true) {
    return `response for ${requestId} was not a success: ${JSON.stringify(entry.error ?? null)}`;
  }
  return null;
}

// Pure exact-count gate over recorded outgoing action frames: a missing
// counter (0) or a duplicate (>=2) can never pass.
function exactActionCountError(count, expected, what) {
  if (count !== expected) return `expected exactly ${expected} ${what}, got ${count}`;
  return null;
}

function boundedActionCountError(count, min, max, what) {
  if (count < min || count > max) return `expected ${min}..${max} ${what}, got ${count}`;
  return null;
}

// Pure exact-once coverage gate over the explicit per-ID counts ledger:
// missing (count 0), duplicate execution (count > 1), unexpected ids,
// duplicate DOCUMENTED_IDS entries, and an execution order that is not
// EXACTLY the documented order (elementwise Map-insertion equality — never
// a sorted/set comparison) can never pass.
function documentedIdsCompletenessError(documentedIds, executedCounts) {
  const documented = new Set(documentedIds);
  const missing = documentedIds.filter((id) => (executedCounts.get(id) || 0) === 0);
  const unexpected = [...executedCounts.keys()].filter((id) => !documented.has(id));
  const duplicates = documentedIds.filter((id, index) => documentedIds.indexOf(id) !== index);
  const repeated = [...executedCounts].filter(([, count]) => count > 1).map(([id]) => id);
  const setsClean = missing.length === 0 && unexpected.length === 0
    && duplicates.length === 0 && repeated.length === 0;
  // Only meaningful once the key sets are identical: the Map's insertion
  // order must equal the documented order position for position.
  if (setsClean && [...executedCounts.keys()].some((id, index) => id !== documentedIds[index])) {
    return `assertion order mismatch: executed Map insertion order ${JSON.stringify([...executedCounts.keys()])} is not exactly the documented order ${JSON.stringify([...documentedIds])}`;
  }
  if (setsClean) return null;
  return `assertion coverage mismatch: missing=${JSON.stringify(missing)}, unexpected=${JSON.stringify(unexpected)}, duplicate documented ids=${JSON.stringify([...new Set(duplicates)])}, ids executed more than once=${JSON.stringify(repeated)}`;
}

function selfCheckResponseAuthority() {
  const rejected = (entry, id, cmd) => responseAuthorityError(entry, id, cmd) !== null;
  const cases = [
    // Correct id + WRONG inbound command: must hard-fail, never false-pass.
    [{ id: 'r1', command: 'workflow_remove', success: true }, 'r1', 'workflow_cancel', true],
    // Missing inbound command: must hard-fail.
    [{ id: 'r1', command: '', success: true }, 'r1', 'workflow_cancel', true],
    [{ id: 'r1', success: true }, 'r1', 'workflow_cancel', true],
    // Id mismatch: must hard-fail.
    [{ id: 'r2', command: 'workflow_cancel', success: true }, 'r1', 'workflow_cancel', true],
    // Non-success: must hard-fail.
    [{ id: 'r1', command: 'workflow_cancel', success: false }, 'r1', 'workflow_cancel', true],
    // Exact id + exact inbound command + success: accepted.
    [{ id: 'r1', command: 'workflow_cancel', success: true }, 'r1', 'workflow_cancel', false],
  ];
  for (const [entry, id, cmd, mustReject] of cases) {
    const error = responseAuthorityError(entry, id, cmd);
    if (mustReject === (error === null)) {
      throw new Error(`response authority self-check: entry ${JSON.stringify(entry)} vs expected ${cmd} -> ${error ?? 'accepted'}`);
    }
  }
}

function selfCheckActionCountsAndCoverage() {
  const exactCases = [
    [0, 1, false], // missing action counter cannot pass
    [2, 1, false], // duplicate actions cannot pass
    [1, 1, true],
    [1, 0, false],
    [1, 2, false],
  ];
  for (const [count, expected, ok] of exactCases) {
    const isOk = exactActionCountError(count, expected, 'workflow requests') === null;
    if (isOk !== ok) throw new Error(`exact action count self-check: count=${count} expected=${expected} accepted=${isOk}`);
  }
  const boundedCases = [[0, false], [1, true], [2, true], [3, false]];
  for (const [count, ok] of boundedCases) {
    const isOk = boundedActionCountError(count, 1, 2, 'workflow_list requests') === null;
    if (isOk !== ok) throw new Error(`bounded action count self-check: count=${count} accepted=${isOk}`);
  }
  const counts = (pairs) => new Map(pairs);
  if (documentedIdsCompletenessError(['A', 'B'], counts([['A', 1], ['B', 1]])) !== null) {
    throw new Error('documented IDs completeness self-check rejected an exact match');
  }
  // Missing id (count 0 / absent) must fail.
  for (const invalid of [counts([]), counts([['A', 1]]), counts([['A', 1], ['B', 0]])]) {
    if (documentedIdsCompletenessError(['A', 'B'], invalid) === null) {
      throw new Error(`documented IDs completeness self-check accepted missing ids ${JSON.stringify([...invalid])}`);
    }
  }
  // Unexpected id must fail.
  if (documentedIdsCompletenessError(['A', 'B'], counts([['A', 1], ['B', 1], ['C', 1]])) === null) {
    throw new Error('documented IDs completeness self-check accepted an unexpected id');
  }
  // Duplicate execution (count > 1) must fail.
  if (documentedIdsCompletenessError(['A', 'B'], counts([['A', 2], ['B', 1]])) === null) {
    throw new Error('documented IDs completeness self-check accepted a duplicate execution');
  }
  // Reordered execution (same exact-once set, different Map insertion
  // order) must fail the elementwise documented-order gate.
  if (documentedIdsCompletenessError(['A', 'B'], counts([['B', 1], ['A', 1]])) === null) {
    throw new Error('documented IDs completeness self-check accepted a reordered execution ledger');
  }
  // Duplicate DOCUMENTED_IDS entries must fail.
  if (documentedIdsCompletenessError(['A', 'A'], counts([['A', 1]])) === null) {
    throw new Error('documented IDs completeness self-check accepted duplicate documented IDs');
  }
}

// Pure publication gate: evidence is published ONLY when this returns null.
// Any coverage/completeness mismatch or captured browser console/page error
// blocks publication — staged artifacts can never be published early or
// partially (fail() drops the staging dir instead).
function publicationGateError({ coverageError, browserErrors }) {
  if (coverageError !== null) return `coverage: ${coverageError}`;
  if (browserErrors.length > 0) {
    return `browser errors captured during the lane (${browserErrors.length}, first: ${browserErrors[0]})`;
  }
  return null;
}

function selfCheckPublicationGate() {
  if (publicationGateError({ coverageError: null, browserErrors: [] }) !== null) {
    throw new Error('publication gate self-check blocked a fully-passing lane');
  }
  if (publicationGateError({ coverageError: 'missing=["WF1.1"]', browserErrors: [] }) === null) {
    throw new Error('publication gate self-check published despite a coverage mismatch');
  }
  if (publicationGateError({ coverageError: null, browserErrors: ['lane console: boom'] }) === null) {
    throw new Error('publication gate self-check published despite a captured console error');
  }
  if (publicationGateError({ coverageError: null, browserErrors: ['lane pageerror: boom'] }) === null) {
    throw new Error('publication gate self-check published despite a captured page error');
  }
}

selfCheckResponseAuthority();
selfCheckActionCountsAndCoverage();
selfCheckPublicationGate();
selfCheckMonitorBrowserErrors();


// Wire browser-error capture and return the REAL ledger array the handlers
// push into — the same array instance the publication gate consumes.
function monitorBrowserErrors(page, label) {
  const errors = [];
  page.on('pageerror', (err) => {
    errors.push(`${label} pageerror: ${err.message}`);
  });
  page.on('console', (message) => {
    if (message.type() === 'error') errors.push(`${label} console: ${message.text()}`);
  });
  page.on('websocket', (socket) => {
    socket.on('socketerror', (error) => {
      errors.push(`${label} websocket: ${String(error)}`);
    });
  });
  return errors;
}

// Self-check that invokes the SAME monitorBrowserErrors wiring the lane
// uses (with a stand-in page object implementing Playwright's `.on`) and
// asserts the RETURNED ledger is the real one: pageerror / console.error /
// websocket socketerror entries land in it with the lane label, while
// non-error console messages never do.
function selfCheckMonitorBrowserErrors() {
  const handlers = new Map();
  const fakePage = {
    on(event, handler) {
      if (!handlers.has(event)) handlers.set(event, []);
      handlers.get(event).push(handler);
    },
  };
  const ledger = monitorBrowserErrors(fakePage, 'wf-self-check');
  if (!Array.isArray(ledger)) throw new Error('monitorBrowserErrors self-check: returned ledger is not an array');
  handlers.get('pageerror')[0](new Error('page boom'));
  handlers.get('console')[0]({ type: () => 'warning', text: () => 'ignored' });
  handlers.get('console')[0]({ type: () => 'error', text: () => 'console boom' });
  handlers.get('websocket')[0]({ on: (_event, handler) => handler(new Error('socket boom')) });
  const expected = [
    'wf-self-check pageerror: page boom',
    'wf-self-check console: console boom',
    'wf-self-check websocket: Error: socket boom',
  ];
  if (JSON.stringify(ledger) !== JSON.stringify(expected)) {
    throw new Error(`monitorBrowserErrors self-check ledger mismatch: ${JSON.stringify(ledger)}`);
  }
}

// Exact-artifact self-check over the ACTUAL production publication helper:
// a staged set that is not EXACTLY the declared one must be rejected with a
// full rollback (staging dir removed, no run dir, no pointer temp, previous
// pointer byte-identical) — a duplicate name, a missing declared artifact,
// an extra unregistered file, a symlinked artifact (even one pointing at a
// real file), and a `..` traversal name can never publish.
function selfCheckStagedArtifactValidation() {
  const pointerPath = path.join(evidence, 'current.json');
  const sentinel = '{"sentinel":"previous publication"}\n';
  const reset = () => {
    fs.rmSync(stagingDir, { recursive: true, force: true });
    fs.rmSync(path.join(evidence, runDirName), { recursive: true, force: true });
    fs.writeFileSync(pointerPath, sentinel);
    laneArtifacts.length = 0;
  };
  const reject = (label) => {
    let threw = false;
    try {
      publishStagedArtifacts({ probe: true });
    } catch (err) {
      threw = true;
      if (!String(err.message).includes('exact staged-artifact validation failed')) {
        throw new Error(`staged-artifact self-check (${label}): failed for the wrong reason: ${err.message}`);
      }
    }
    if (!threw) throw new Error(`staged-artifact self-check: invalid staged set ${label} was published`);
    if (fs.existsSync(stagingDir)) throw new Error(`staged-artifact self-check: staging dir survived rejection (${label})`);
    if (fs.existsSync(path.join(evidence, runDirName))) throw new Error(`staged-artifact self-check: run dir survived rejection (${label})`);
    if (fs.readdirSync(evidence).some((name) => name.startsWith('current.json.tmp-'))) {
      throw new Error(`staged-artifact self-check: pointer temp survived rejection (${label})`);
    }
    if (fs.readFileSync(pointerPath, 'utf8') !== sentinel) {
      throw new Error(`staged-artifact self-check: previous pointer mutated by rejection (${label})`);
    }
  };
  reset();
  stageWrite('probe.json', '{"probe":1}\n');
  laneArtifacts.push('probe.json');
  laneArtifacts.push('probe.json');
  reject('duplicate');
  // Missing declared artifact (registered but never written).
  reset();
  stageWrite('probe.json', '{"probe":1}\n');
  laneArtifacts.push('ghost.json');
  reject('missing');
  // Extra unregistered file inside the staged set.
  reset();
  stageWrite('probe.json', '{"probe":1}\n');
  fs.writeFileSync(path.join(stagingDir, 'extra.json'), '{"extra":1}\n');
  reject('extra');
  // Symlinked artifact: even a symlink pointing at a REAL file is rejected.
  fs.rmSync(stagingDir, { recursive: true, force: true });
  fs.rmSync(path.join(evidence, runDirName), { recursive: true, force: true });
  const outsideTarget = path.join(evidence, 'outside-target.txt');
  fs.writeFileSync(outsideTarget, 'real bytes\n');
  fs.mkdirSync(stagingDir, { recursive: true });
  fs.symlinkSync(outsideTarget, path.join(stagingDir, 'link.json'));
  laneArtifacts.length = 0;
  laneArtifacts.push('probe.json', 'link.json');
  fs.writeFileSync(path.join(stagingDir, 'probe.json'), '{"probe":1}\n');
  reject('symlink');
  fs.rmSync(outsideTarget, { force: true });
  // Traversal name escaping the run dir.
  reset();
  stageWrite('probe.json', '{"probe":1}\n');
  laneArtifacts.push('../escape.json');
  reject('traversal');
}
// Fault-injection self-check over the ACTUAL publication helper (not a
// reimplementation): seeds a previous publication pointer, stages a fresh
// artifact set, and injects a fault at EVERY pre-pointer-commit stage
// (manifest write, stage->run rename, pointer write, pointer rename). Each
// injected invocation must throw, leave NO staging dir / renamed run dir /
// pointer temp file behind, and preserve the previous pointer
// byte-identical. A clean invocation then publishes for real: the staging
// dir disappears via the single rename, the run dir materializes, the
// pointer names the new run, and no pointer temp residue remains.
function selfCheckPublicationFaults() {
  const pointerPath = path.join(evidence, 'current.json');
  const sentinel = '{"sentinel":"previous publication"}\n';
  for (const faultAt of ['manifest', 'rename', 'pointer-write', 'pointer-rename']) {
    fs.rmSync(stagingDir, { recursive: true, force: true });
    fs.rmSync(path.join(evidence, runDirName), { recursive: true, force: true });
    fs.writeFileSync(pointerPath, sentinel);
    laneArtifacts.length = 0;
    stageWrite('probe.json', '{"probe":1}\n');
    let threw = false;
    try {
      publishStagedArtifacts({ probe: true }, faultAt);
    } catch {
      threw = true;
    }
    if (!threw) throw new Error(`publication fault self-check: fault ${faultAt} did not throw`);
    if (fs.existsSync(stagingDir)) throw new Error(`publication fault self-check: staging dir survived fault ${faultAt}`);
    if (fs.existsSync(path.join(evidence, runDirName))) throw new Error(`publication fault self-check: run dir survived fault ${faultAt}`);
    if (fs.readdirSync(evidence).some((name) => name.startsWith('current.json.tmp-'))) {
      throw new Error(`publication fault self-check: pointer temp survived fault ${faultAt}`);
    }
    if (fs.readFileSync(pointerPath, 'utf8') !== sentinel) {
      throw new Error(`publication fault self-check: old pointer mutated by fault ${faultAt}`);
    }
  }
  laneArtifacts.length = 0;
  stageWrite('probe.json', '{"probe":1}\n');
  publishStagedArtifacts({ probe: true });
  if (fs.existsSync(stagingDir)) throw new Error('publication fault self-check: staging dir survived a successful publish');
  if (!fs.existsSync(path.join(evidence, runDirName, 'probe.json'))
    || !fs.existsSync(path.join(evidence, runDirName, 'manifest.json'))) {
    throw new Error('publication fault self-check: successful publish did not materialize the run dir');
  }
  const pointer = JSON.parse(fs.readFileSync(pointerPath, 'utf8'));
  if (pointer.lane !== 'web-sessions-workflow' || pointer.run !== runDirName
    || pointer.nonce !== laneNonce
    || pointer.manifest !== 'manifest.json'
    || pointer.coverage !== 'coverage-assertions.json'
    || !Array.isArray(pointer.artifacts) || !pointer.artifacts.includes('probe.json')) {
    throw new Error(`publication fault self-check: pointer does not name the new run: ${JSON.stringify(pointer)}`);
  }
  if (fs.readdirSync(evidence).some((name) => name.startsWith('current.json.tmp-'))) {
    throw new Error('publication fault self-check: pointer temp survived a successful publish');
  }
}

async function waitFor(page, fn, id, timeoutMs = 30000, arg, diagnose) {
  try {
    await page.waitForFunction(fn, arg, { timeout: timeoutMs });
  } catch {
    const detail = diagnose ? await page.evaluate(diagnose, arg).catch(() => null) : null;
    fail(`${id} (timeout ${timeoutMs}ms${detail === null ? '' : `; state ${JSON.stringify(detail)}`})`);
  }
}

async function rowIds(page) {
  return page.evaluate(() =>
    [...document.querySelectorAll('.session-sidebar__switch')].map((r) => r.dataset.sessionId || '')
  );
}

const mockControl = async (mockPortValue, path) => {
  if (!mockPortValue) fail('WF: RPI_MOCK_PORT is required for event-gated workflow proof');
  const response = await fetch(`http://127.0.0.1:${mockPortValue}${path}`, { method: 'POST' });
  if (!response.ok) fail(`WF: mock control ${path} failed with ${response.status}`);
  return response.status === 204 ? {} : response.json();
};

async function main() {
  if (!url) fail('RPI_URL is required');
  const { chromium } = await import('playwright');
  fs.mkdirSync(evidence, { recursive: true });
  const browser = await chromium.launch(chromePath ? { executablePath: chromePath } : {});
  let rpcSequence = 0;
  try {
    const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
    if (token) {
      await page.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
    }
    // REAL WS-LEVEL workflow gate. Every app socket passes through a wrapper
    // that:
    //   * records every outgoing workflow_* REQUEST (type, request ID,
    //     workflowId, sending socket tag, monotonic seq) in __wfDetailState
    //     .actions; workflow_detail requests additionally land in .frames and
    //     the in-flight set (.pending = request ids sent without a DELIVERED
    //     response);
    //   * records every settled workflow_* RESPONSE (id, EXACT inbound
    //     frame.command, success) and
    //     the full workflow_list / workflow_detail response payloads in
    //     .payloads, so the lane can parse the AUTHORITATIVE list
    //     generation/status instead of inferring state from DOM mutations;
    //   * when .holdFor is set to an exact workflowId, the FIRST
    //     workflow_detail request for THAT id is still physically DELIVERED
    //     to the server (the server receives it and works), but its RESPONSE
    //     is withheld from the app (stopImmediatePropagation) and parked in
    //     .heldResponse — a real response hold, not a fake pending marker;
    //   * releaseHeldDetail() re-delivers the parked response into the app.
    //     The single-flight acceptance is asserted against exact per-workflow
    //     request counts, request IDs, response IDs and the in-flight set —
    //     never against fixed sleeps alone.
    await page.addInitScript(() => {
      window.__wfDetailState = {
        frames: [],         // workflow_detail REQUESTS {seq,id,workflowId,socket,at}
        actions: [],        // every workflow_* REQUEST {seq,id,type,workflowId,socket,at}
        responses: [],      // settled workflow_* RESPONSES {id,command,success,data,error,at}
        payloads: {},       // id -> {success,data,error} for list/detail responses
        pending: new Set(), // workflow_detail request ids without a DELIVERED response
        holdFor: null,      // exact workflowId whose next detail response is held
        held: null,
        heldResponse: null,
      };
      let wfSocketSeq = 0;
      let wfFrameSeq = 0;
      const OriginalWebSocket = window.WebSocket;
      window.WebSocket = function (url, protocols) {
        const ws = protocols !== undefined ? new OriginalWebSocket(url, protocols) : new OriginalWebSocket(url);
        const socketId = `wf-socket-${++wfSocketSeq}`;
        const originalSend = ws.send.bind(ws);
        ws.send = function (data) {
          try {
            const frame = typeof data === 'string' ? JSON.parse(data) : null;
            if (frame && typeof frame.type === 'string' && frame.type.startsWith('workflow_')) {
              const record = {
                seq: ++wfFrameSeq,
                id: frame.id || '',
                type: frame.type,
                workflowId: frame.workflowId || '',
                socket: socketId,
                at: Date.now(),
              };
              window.__wfDetailState.actions.push(record);
              if (frame.type === 'workflow_detail') {
                window.__wfDetailState.frames.push(record);
                window.__wfDetailState.pending.add(record.id);
                if (window.__wfDetailState.held === null
                  && window.__wfDetailState.holdFor !== null
                  && record.workflowId === window.__wfDetailState.holdFor) {
                  // ARMED HOLD for the EXACT workflowId: the request is
                  // still physically delivered below (the server receives
                  // it and works); only its RESPONSE is withheld, in the
                  // message listener.
                  window.__wfDetailState.holdFor = null;
                  window.__wfDetailState.held = { ...record, ws: this };
                }
              }
            }
          } catch { /* non-JSON frame: not a workflow_* request */ }
          return originalSend(data);
        };
        ws.addEventListener('message', (event) => {
          try {
            const frame = typeof event.data === 'string' ? JSON.parse(event.data) : null;
            if (!frame || frame.type !== 'response') return;
            const state = window.__wfDetailState;
            if (state.held && frame.id === state.held.id && frame.command === 'workflow_detail') {
              // RESPONSE HOLD: the request was physically received and
              // worked server-side; withhold its response from the app.
              state.heldResponse = { id: frame.id, raw: event.data, at: Date.now() };
              event.stopImmediatePropagation();
              return;
            }
            const request = state.actions.find((row) => row.id === frame.id);
            if (!request) return;
            // Record the EXACT inbound frame.command — never a type inferred
            // from the outgoing request id — so a correct request id with a
            // missing/wrong command can never pass a response assertion.
            const command = typeof frame.command === 'string' ? frame.command : '';
            state.responses.push({
              id: frame.id,
              command,
              success: frame.success === true,
              data: frame.data ?? null,
              error: frame.error ?? null,
              at: Date.now(),
            });
            if (command === 'workflow_list' || command === 'workflow_detail') {
              state.payloads[frame.id] = {
                success: frame.success === true,
                data: frame.data ?? null,
                error: frame.error ?? null,
                at: Date.now(),
              };
            }
            if (command === 'workflow_detail') state.pending.delete(frame.id);
          } catch { /* non-JSON frame */ }
        });
        return ws;
      };
      window.WebSocket.prototype = OriginalWebSocket.prototype;
      window.WebSocket.CONNECTING = 0;
      window.WebSocket.OPEN = 1;
      window.WebSocket.CLOSING = 2;
      window.WebSocket.CLOSED = 3;
    });
    const browserErrors = monitorBrowserErrors(page, 'workflow-lane');
    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'WF0.1: page title missing', 20000);
    record('WF0.1');
    await waitFor(
      page,
      () => document.getElementById('send-btn')?.hasAttribute('disabled') !== true,
      'WF0.2: WS did not reach "connected"'
    );
    record('WF0.2');
    await waitFor(
      page,
      () => {
        const rows = [...document.querySelectorAll('.session-sidebar__switch')];
        return rows.length >= 1 && rows.every((r) => (r.dataset.sessionId || '') !== '');
      },
      'WF0.3: primary session row never appeared in the sidebar'
    );
    const sessionA = (await rowIds(page))[0];
    if (!sessionA) fail('WF0.3: could not read the primary session id');
    record('WF0.3');
    await stageShot(page, 'workflow-t0-boot.png');

    const rpc = (sessionId, command) => {
      const id = `wf-lane-${++rpcSequence}`;
      const wsUrl = url.replace(/^http/, 'ws').replace(/\/web\/?$/, '/ws');
      return page.evaluate(
        ({ wsUrl: target, token: authToken, command: request, sessionId: owner, id: requestId }) => new Promise((resolve, reject) => {
          const protocols = authToken ? [`rpi-auth.${authToken}`] : [];
          const ws = new WebSocket(target, protocols);
          const timer = window.setTimeout(() => {
            ws.close();
            reject(new Error(`rpc timed out: ${request.type}`));
          }, 30000);
          ws.addEventListener('open', () => ws.send(JSON.stringify({ ...request, sessionId: owner, id: requestId })));
          ws.addEventListener('message', (event) => {
            let frame;
            try {
              frame = JSON.parse(String(event.data));
            } catch {
              return;
            }
            if (frame?.type !== 'response' || frame.id !== requestId) return;
            window.clearTimeout(timer);
            ws.close();
            if (frame.sessionId !== owner) {
              const created = frame.data && frame.data.sessionId;
              if (request.type !== 'new_session' || frame.sessionId !== created) {
                reject(new Error(`rpc response owner mismatch: expected ${owner}, got ${JSON.stringify(frame.sessionId)}`));
                return;
              }
            }
            if (!frame.success) {
              reject(new Error(frame.error || 'rpc failed'));
              return;
            }
            resolve(frame.data || {});
          });
          ws.addEventListener('error', () => {
            window.clearTimeout(timer);
            reject(new Error(`rpc transport failed: ${request.type}`));
          });
        }),
        { wsUrl, token, command, sessionId, id }
      );
    };

    // Session B arrives through the backend lifecycle (new_session RPC), not
    // the composer: the workflow lane must not depend on prompt dispatch.
    const knownIds = [sessionA];
    await rpc(sessionA, { type: 'new_session' });
    await waitFor(
      page,
      (known) => {
        const ids = [...document.querySelectorAll('.session-sidebar__switch')].map((r) => r.dataset.sessionId || '');
        return ids.some((sid) => sid !== '' && !known.includes(sid));
      },
      'WF0.4: RPC-created session B row never appeared in the sidebar',
      30000,
      knownIds
    );
    const sessionB = (await rowIds(page)).find((sid) => sid !== '' && !knownIds.includes(sid));
    if (!sessionB) fail('WF0.4: could not read the RPC-created session B id');
    record('WF0.4');
    knownIds.push(sessionB);

    const waitForSessionWorkflow = async (sessionId, workflowId, expectedName, label, expectedTerminal = false) => {
      const deadline = Date.now() + 60000;
      let last = 'snapshot missing';
      while (Date.now() < deadline) {
        let workflow;
        try {
          workflow = await rpc(sessionId, { type: 'workflow_get', workflowId });
        } catch (err) {
          last = err instanceof Error ? err.message : String(err);
          await new Promise((resolve) => setTimeout(resolve, 150));
          continue;
        }
        const classification = classifyWorkflowSnapshot(workflow, workflowId, expectedName);
        if (classification.kind === 'terminal-error') {
          if (expectedTerminal) return classification;
          fail(`${label}: ${classification.message}`);
        }
        if (classification.kind === 'success') {
          if (expectedTerminal) return { kind: 'unexpected-success', message: JSON.stringify(workflow) };
          return workflow;
        }
        last = JSON.stringify(workflow);
        await new Promise((resolve) => setTimeout(resolve, 150));
      }
      if (expectedTerminal) return { kind: 'timeout', message: last };
      fail(`${label}: workflow never completed healthily: ${last}`);
    };

    const requireHealthyIdentity = (workflow, expectedName, label) => {
      if (!Number.isSafeInteger(workflow.generation) || workflow.generation < 1) fail(`${label}: ${expectedName} generation is not positive`);
      if (workflow.name !== expectedName) fail(`${label}: ${expectedName} create name mismatch`);
      if (typeof workflow.worktree !== 'string' || workflow.worktree.trim() === '') fail(`${label}: ${expectedName} missing worktree`);
      if (typeof workflow.branch !== 'string' || workflow.branch.trim() === '') fail(`${label}: ${expectedName} missing branch`);
      if (!workflow.branch.includes(workflow.workflowId)) fail(`${label}: ${expectedName} branch lost workflow identity`);
    };

    const requireTerminalDag = async (sessionId, workflow, expectedName, objective, label) => {
      const detail = await rpc(sessionId, { type: 'workflow_detail', workflowId: workflow.workflowId });
      if (detail.id !== workflow.workflowId || detail.name !== expectedName
        || detail.generation !== workflow.generation || detail.status !== 'completed') {
        fail(`${label}: completed workflow detail identity/status mismatch: ${JSON.stringify(detail)}`);
      }
      const tasks = Array.isArray(detail.todo) ? detail.todo.flatMap((phase) => Array.isArray(phase.tasks) ? phase.tasks : []) : [];
      if (tasks.length !== 2) fail(`${label}: completed DAG must contain exactly two tasks: ${JSON.stringify(tasks)}`);
      const identityTag = `[workflowId=${workflow.workflowId};name=${objective}]`;
      const compile = tasks.find((task) => task.content === `compile widget ${identityTag}`);
      const ship = tasks.find((task) => task.content === `ship widget ${identityTag}`);
      if (!compile || !ship) fail(`${label}: expected compile/ship tasks missing: ${JSON.stringify(tasks)}`);
      if (compile.status !== 'completed' || ship.status !== 'completed') {
        fail(`${label}: terminal DAG tasks must both be completed: ${JSON.stringify({ compile: compile.status, ship: ship.status })}`);
      }
      if (!Array.isArray(ship.dependsOn) || ship.dependsOn.length !== 1 || ship.dependsOn[0] !== compile.id) {
        fail(`${label}: terminal ship task must depend on exactly [compile.id]: ${JSON.stringify(ship)}`);
      }
      if (tasks.some((task) => task.status === 'abandoned' || !task.id)) fail(`${label}: terminal DAG has abandoned/missing task identity`);
      const ownership = Array.isArray(workflow.ownership) ? workflow.ownership : [];
      const ownedTaskIds = new Set(ownership.map((row) => row.todoTaskId));
      if (ownership.length !== 2 || ownedTaskIds.size !== 2
        || !ownedTaskIds.has(compile.id) || !ownedTaskIds.has(ship.id)
        || ownership.some((row) => row.workflowId !== workflow.workflowId || row.generation !== workflow.generation)) {
        fail(`${label}: completed task ownership is not exact-once: ${JSON.stringify(ownership)}`);
      }
      return { compile, ship };
    };

    /* ---------------- WF1: fail-closed planning probe ---------------- */
    const failureCreated = await rpc(sessionA, {
      type: 'workflow_create', name: 'fail-closed-probe', objective: 'sessions-fail-closed-objective',
    });
    const failureClassification = await waitForSessionWorkflow(
      sessionA,
      failureCreated.workflowId,
      'fail-closed-probe',
      'WF1 fail-closed probe',
      true
    );
    if (failureClassification.kind !== 'terminal-error'
      || !failureClassification.message.includes('terminal failed')
      || failureClassification.message.includes('no surfaced reason')) {
      fail(`WF1.1: planning without a committed typed plan did not land terminal failed with a retained reason: ${JSON.stringify(failureClassification)}`);
    }
    record('WF1.1');
    if (!failureClassification.message.includes('planning produced no tasks')) {
      fail(`WF1.2: fail-closed reason lost its planning context: ${failureClassification.message}`);
    }
    record('WF1.2');
    await rpc(sessionA, { type: 'workflow_remove', workflowId: failureCreated.workflowId });

    /* ---------------- WF2: healthy multi-node DAG on session A ---------------- */
    const workflowACreated = await rpc(sessionA, {
      type: 'workflow_create', name: 'a-wf', objective: 'a-wf-objective',
    });
    if (!workflowACreated.workflowId) fail('WF2.1: workflow create response missing workflowId');
    const heldDeadline = Date.now() + 30000;
    let heldStats = {};
    while (Date.now() < heldDeadline) {
      heldStats = await mockControl(mockPort, '/__session-workers-held');
      if (heldStats.held >= 1 && heldStats.workers >= 1) break;
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
    if (heldStats.held < 1 || heldStats.workers < 1) fail(`WF2.1: first dependency wave never became authoritatively held: ${JSON.stringify(heldStats)}`);
    record('WF2.1');
    {
      const progress = await rpc(sessionA, { type: 'workflow_detail', workflowId: workflowACreated.workflowId });
      const progressClassification = classifyWorkflowSnapshot(progress, workflowACreated.workflowId, 'a-wf');
      if (progressClassification.kind !== 'progress') {
        fail(`WF2.2: held workflow was not live progress (no false-pass completion allowed): ${progressClassification.message || JSON.stringify(progress)}`);
      }
      const tasks = Array.isArray(progress.todo) ? progress.todo.flatMap((phase) => phase.tasks || []) : [];
      const identityTag = `[workflowId=${workflowACreated.workflowId};name=a-wf-objective]`;
      const compile = tasks.find((task) => task.content === `compile widget ${identityTag}`);
      const ship = tasks.find((task) => task.content === `ship widget ${identityTag}`);
      if (!compile || !ship || !['in_progress', 'completed'].includes(compile.status)
        || ship.status !== 'pending' || ship.ready !== false || !Array.isArray(ship.dependsOn)
        || ship.dependsOn.length !== 1 || ship.dependsOn[0] !== compile.id) {
        fail(`WF2.2: DAG did not expose compile-running/ship-blocked dependency transition: ${JSON.stringify(tasks)}`);
      }
      record('WF2.2');
    }
    await mockControl(mockPort, '/__release-session-workers');
    const secondWaveDeadline = Date.now() + 30000;
    while (Date.now() < secondWaveDeadline) {
      heldStats = await mockControl(mockPort, '/__session-workers-held');
      if (heldStats.completed >= 1 && heldStats.held >= 1 && heldStats.workers >= 2) break;
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
    if (heldStats.completed < 1 || heldStats.held < 1 || heldStats.workers < 2) {
      fail(`WF2.3: dependent ship wave never became held after compile completion: ${JSON.stringify(heldStats)}`);
    }
    record('WF2.3');
    await mockControl(mockPort, '/__release-session-workers');
    const workflowA = await waitForSessionWorkflow(sessionA, workflowACreated.workflowId, 'a-wf', 'WF2.4 session A');
    requireHealthyIdentity(workflowA, 'a-wf', 'WF2.4');
    if (workflowA.status !== 'completed') fail(`WF2.4: a-wf did not remain completed: ${workflowA.status}`);
    await requireTerminalDag(sessionA, workflowA, 'a-wf', 'a-wf-objective', 'WF2.5');
    record('WF2.4');
    record('WF2.5');
    await stageShot(page, 'workflow-t2-completed.png');

    /* ---------------- WF3: second healthy workflow on session B + isolation ---------------- */
    const workflowBCreated = await rpc(sessionB, {
      type: 'workflow_create', name: 'b-wf', objective: 'b-wf-objective',
    });
    if (!workflowBCreated.workflowId) fail('WF3.1: workflow create response missing workflowId');
    const bHeldDeadline = Date.now() + 30000;
    let bHeldStats = {};
    while (Date.now() < bHeldDeadline) {
      bHeldStats = await mockControl(mockPort, '/__session-workers-held');
      if (bHeldStats.held >= 1 && bHeldStats.workers >= 3) break;
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
    if (bHeldStats.held < 1 || bHeldStats.workers < 3) fail(`WF3.1: b-wf first wave never became authoritatively held: ${JSON.stringify(bHeldStats)}`);
    await mockControl(mockPort, '/__release-session-workers');
    const bSecondWaveDeadline = Date.now() + 30000;
    while (Date.now() < bSecondWaveDeadline) {
      bHeldStats = await mockControl(mockPort, '/__session-workers-held');
      if (bHeldStats.completed >= 2 && bHeldStats.held >= 1 && bHeldStats.workers >= 4) break;
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
    if (bHeldStats.completed < 2 || bHeldStats.held < 1 || bHeldStats.workers < 4) {
      fail(`WF3.1: b-wf ship wave never became held: ${JSON.stringify(bHeldStats)}`);
    }
    await mockControl(mockPort, '/__release-session-workers');
    const workflowB = await waitForSessionWorkflow(sessionB, workflowBCreated.workflowId, 'b-wf', 'WF3.1 session B');
    requireHealthyIdentity(workflowB, 'b-wf', 'WF3.1');
    if (workflowB.status !== 'completed') fail(`WF3.1: b-wf did not remain completed: ${workflowB.status}`);
    await requireTerminalDag(sessionB, workflowB, 'b-wf', 'b-wf-objective', 'WF3.1');
    record('WF3.1');
    if (workflowA.workflowId === workflowB.workflowId) fail(`WF3.1: A/B workflowId collision ${workflowA.workflowId}`);
    if (workflowA.worktree === workflowB.worktree) fail(`WF3.1: A/B worktree collision ${workflowA.worktree}`);
    if (workflowA.branch === workflowB.branch) fail(`WF3.1: A/B branch collision ${workflowA.branch}`);

    const [listA, listB] = await Promise.all([
      rpc(sessionA, { type: 'workflow_list' }),
      rpc(sessionB, { type: 'workflow_list' }),
    ]);
    const idsA = new Set((listA.workflows || []).map((workflow) => workflow.workflowId));
    const idsB = new Set((listB.workflows || []).map((workflow) => workflow.workflowId));
    if (!idsA.has(workflowA.workflowId) || idsA.has(workflowB.workflowId)) fail(`WF3.2: session A workflow list crossed ownership: ${JSON.stringify(listA.workflows)}`);
    if (!idsB.has(workflowB.workflowId) || idsB.has(workflowA.workflowId)) fail(`WF3.2: session B workflow list crossed ownership: ${JSON.stringify(listB.workflows)}`);
    record('WF3.2');
    await stageShot(page, 'workflow-t3-isolation.png');

    /* ---------------- WF4: exact-once counts + terminal stability ---------------- */
    const finalStats = await mockControl(mockPort, '/__session-workflow-stats');
    const expectedAssignments = new Map([
      [workflowACreated.workflowId, 'a-wf-objective'],
      [workflowBCreated.workflowId, 'b-wf-objective'],
    ]);
    const observedAssignments = finalStats.assignments || {};
    if (finalStats.planning !== 6 || finalStats.held !== 0
      || finalStats.workers !== 4 || finalStats.completed !== 4
      || Object.keys(observedAssignments).length !== expectedAssignments.size
      || [...expectedAssignments].some(([workflowId, name]) => {
        const recordRow = observedAssignments[workflowId];
        return recordRow?.name !== name
          || Object.keys(recordRow.tasks || {}).length !== 2
          || recordRow.tasks.compile !== 1
          || recordRow.tasks.ship !== 1;
      })) {
      fail(`WF4.1: required tasks did not complete exactly once per workflow: ${JSON.stringify(finalStats)}`);
    }
    record('WF4.1');
    for (const [owner, workflow, expectedName, expectedId] of [
      [sessionA, workflowA, 'a-wf', workflowACreated.workflowId],
      [sessionB, workflowB, 'b-wf', workflowBCreated.workflowId],
    ]) {
      for (let poll = 0; poll < 3; poll += 1) {
        const stable = await rpc(owner, { type: 'workflow_get', workflowId: expectedId });
        if (stable.status !== 'completed'
          || stable.integration !== workflow.integration
          || stable.generation !== workflow.generation) {
          fail(`WF4.2: ${expectedName} completion was not terminal across re-polls: ${JSON.stringify(stable)}`);
        }
        if (classifyWorkflowSnapshot(stable, expectedId, expectedName).kind !== 'success') {
          fail(`WF4.2: ${expectedName} re-poll lost the authoritative completed classification: ${JSON.stringify(stable)}`);
        }
        await new Promise((resolve) => setTimeout(resolve, 1000));
      }
    }
    record('WF4.2');


    /* ---------------- WF5: Workflow panel Cancel + Remove — real held-response single-flight ---------------- */
    const countPanelErrors = () => page.evaluate(
      () => document.querySelectorAll('#workflow-panel .workflow-panel__error').length
    );
    const countErrorToasts = () => page.evaluate(
      () => document.querySelectorAll('#toasts .toast--error').length
    );
    const bodyHasText = (needle) => page.evaluate(
      (probe) => document.body.textContent.includes(probe), needle
    );
    const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
    const detailFramesFor = (workflowId) => page.evaluate(
      (wfId) => window.__wfDetailState.frames.filter((frame) => frame.workflowId === wfId).length,
      workflowId
    );
    const detailPendingFor = (workflowId) => page.evaluate(
      (wfId) => [...window.__wfDetailState.pending].filter((id) => window.__wfDetailState.frames.some((frame) => frame.id === id && frame.workflowId === wfId)).length,
      workflowId
    );
    const waitLiveWorkflow = async (owner, workflowId, label) => {
      const deadline = Date.now() + 30000;
      while (Date.now() < deadline) {
        const workflow = await rpc(owner, { type: 'workflow_get', workflowId });
        if (WORKFLOW_PROGRESS_STATUSES.has(workflow.status)) return workflow;
        await sleep(150);
      }
      fail(`${label}: workflow never reached a live (non-terminal) status`);
    };
    // Arm the physical response hold for the EXACT workflowId: the next
    // workflow_detail request carrying THIS id is still physically delivered
    // to the server, but its response is withheld from the app.
    const armDetailHold = (workflowId) => page.evaluate(
      (wfId) => { window.__wfDetailState.holdFor = wfId; }, workflowId
    );
    // EXACT outgoing workflow_* requests on the application socket, filtered
    // by the wrapper's socket tag (lane RPC sockets are tagged separately).
    const actionRequestsFor = (socket, type, workflowId) => page.evaluate(
      (probe) => window.__wfDetailState.actions.filter(
        (row) => row.socket === probe.socket && row.type === probe.type && row.workflowId === probe.workflowId
      ),
      { socket, type, workflowId }
    );
    const waitForActionRequest = async (socket, type, workflowId, label, timeoutMs = 10000) => {
      const deadline = Date.now() + timeoutMs;
      while (Date.now() < deadline) {
        const matches = await actionRequestsFor(socket, type, workflowId);
        if (matches.length === 1) return matches[0];
        if (matches.length > 1) fail(`${label}: expected exactly one ${type} request for the workflow, got ${matches.length}`);
        await sleep(50);
      }
      fail(`${label}: no ${type} request was observed on the application socket`);
    };
    // The exact application-socket workflow_list issued AFTER a given action
    // request (an action settlement refreshes the list on the same socket).
    const waitForAppListAfter = async (socket, minSeq, label, timeoutMs = 10000) => {
      const deadline = Date.now() + timeoutMs;
      while (Date.now() < deadline) {
        const matches = await page.evaluate(
          (probe) => window.__wfDetailState.actions.filter(
            (row) => row.socket === probe.socket && row.type === 'workflow_list' && row.seq > probe.minSeq
          ),
          { socket, minSeq }
        );
        if (matches.length >= 1) return matches[0];
        await sleep(50);
      }
      fail(`${label}: no workflow_list request followed the action on the application socket`);
    };
    const appListRequestsAfter = (socket, minSeq) => page.evaluate(
      (probe) => window.__wfDetailState.actions.filter(
        (row) => row.socket === probe.socket && row.type === 'workflow_list' && row.seq > probe.minSeq
      ),
      { socket, minSeq }
    );
    const appListCountAfter = async (socket, minSeq) => (await appListRequestsAfter(socket, minSeq)).length;
    const listPayloadFor = (requestId) => page.evaluate(
      (id) => window.__wfDetailState.payloads[id] ?? null, requestId
    );
    const waitForHeldResponse = async (workflowId, label, timeoutMs = 20000) => {
      const deadline = Date.now() + timeoutMs;
      while (Date.now() < deadline) {
        // A held snapshot is only valid when the REAL response has physically
        // arrived and been withheld: heldResponse.id === held.id, the frame
        // appears exactly once for this workflow, and the request id is
        // still in the in-flight (pending) set.
        const snapshot = await page.evaluate((wfId) => {
          const state = window.__wfDetailState;
          const held = state.held;
          if (!held || held.workflowId !== wfId) return null;
          if (!state.heldResponse || state.heldResponse.id !== held.id) return null;
          const idFrames = state.frames.filter((frame) => frame.id === held.id);
          if (idFrames.length !== 1 || idFrames[0].workflowId !== wfId) return null;
          if (!state.pending.has(held.id)) return null;
          return {
            id: held.id,
            socket: held.socket,
            seq: held.seq,
            framesForWorkflow: state.frames.filter((frame) => frame.workflowId === wfId).length,
          };
        }, workflowId);
        if (snapshot) return snapshot;
        await sleep(100);
      }
      fail(`${label}: no workflow_detail request was physically received with its response held`);
    };
    const heldResponseStillParked = (heldId) => page.evaluate((id) => {
      const state = window.__wfDetailState;
      return Boolean(state.held && state.held.id === id
        && state.heldResponse && state.heldResponse.id === id
        && state.pending.has(id));
    }, heldId);
    const releaseHeldDetail = () => page.evaluate(() => {
      const held = window.__wfDetailState.held;
      if (!held) return 0;
      window.__wfDetailState.held = null;
      held.ws.dispatchEvent(new MessageEvent('message', { data: window.__wfDetailState.heldResponse.raw }));
      return 1;
    });
    const waitForResponse = async (requestId, label, timeoutMs = 20000) => {
      const deadline = Date.now() + timeoutMs;
      while (Date.now() < deadline) {
        const entry = await page.evaluate(
          (id) => window.__wfDetailState.responses.find((row) => row.id === id) ?? null,
          requestId
        );
        if (entry) return entry;
        await sleep(50);
      }
      fail(`${label}: no response was ever delivered for request ${requestId}`);
    };
    // Wait for the EXACT success response of a recorded request id: the
    // settled entry must carry the matching id AND the EXACT expected
    // inbound command AND success — a correct request id with a missing or
    // wrong command hard-fails instead of false-passing.
    const requireSuccessResponse = async (requestId, type, label) => {
      const entry = await waitForResponse(requestId, label);
      const authority = responseAuthorityError(entry, requestId, type);
      if (authority !== null) {
        const payload = await listPayloadFor(requestId);
        fail(`${label}: ${authority}; payload ${JSON.stringify(payload)}`);
      }
      return entry;
    };
    // Settled workflow_detail RESPONSE count for a workflow (the released
    // stale response counts; any NEW one must not exist after an
    // authoritative remove).
    const detailResponsesFor = (workflowId) => page.evaluate(
      (wfId) => {
        const state = window.__wfDetailState;
        const ids = new Set(state.frames.filter((frame) => frame.workflowId === wfId).map((frame) => frame.id));
        return state.responses.filter((row) => row.command === 'workflow_detail' && ids.has(row.id)).length;
      }, workflowId
    );

    const cancelWorkflowCreated = await rpc(sessionA, {
      type: 'workflow_create', name: 'cancel-wf', objective: 'cancel-wf-objective',
    });
    if (!cancelWorkflowCreated.workflowId) fail('WF5.1: cancel-wf create response missing workflowId');
    await waitLiveWorkflow(sessionA, cancelWorkflowCreated.workflowId, 'WF5.1');

    await page.getByRole('button', { name: 'Open Workflows panel' }).click();
    await waitFor(page, () => Boolean(document.getElementById('workflow-panel')), 'WF5.1: workflow panel never opened', 10000);
    await waitFor(
      page,
      (wfId) => Boolean(document.getElementById(`workflow-row-${wfId}`)),
      'WF5.1: cancel-wf row never appeared in the workflow list',
      30000,
      cancelWorkflowCreated.workflowId
    );
    // WF5.1: arm the gate for the EXACT workflowId so the next
    // workflow_detail request carrying cancel-wf's id is physically
    // delivered but its RESPONSE is withheld, select the row, and require
    // the FULL held precondition: real response arrived + withheld
    // (heldResponse id === held id), the request id appears exactly once,
    // and the id is still in the in-flight set.
    await armDetailHold(cancelWorkflowCreated.workflowId);
    await page.click(`#workflow-row-${cancelWorkflowCreated.workflowId}`);
    const heldRequest = await waitForHeldResponse(cancelWorkflowCreated.workflowId, 'WF5.1: no workflow_detail request was physically received with its response held');
    const framesBeforeClick = await detailFramesFor(cancelWorkflowCreated.workflowId);
    if (framesBeforeClick !== 1) fail(`WF5.1: expected exactly 1 pre-click workflow_detail frame for the held workflow, got ${framesBeforeClick}`);
    await waitFor(
      page,
      () => {
        const button = document.getElementById('workflow-cancel-btn');
        return Boolean(button) && !button.hasAttribute('disabled');
      },
      'WF5.1: Cancel button never became available for the live workflow',
      15000
    );
    const pendingBefore = await detailPendingFor(cancelWorkflowCreated.workflowId);
    if (pendingBefore !== 1) fail(`WF5.1: expected exactly 1 pending pre-click workflow_detail request, got ${pendingBefore}`);

    // With the pre-click response physically held: record the EXACT outgoing
    // workflow_cancel request id and the EXACT subsequent
    // application-socket workflow_list id, then wait for BOTH exact success
    // responses — the old detail response stays parked the whole time and
    // the server/WS must observe NO second workflow_detail request. No DOM
    // mutation is used as a settlement signal.
    await page.click('#workflow-cancel-btn');
    const cancelAction = await waitForActionRequest(heldRequest.socket, 'workflow_cancel', cancelWorkflowCreated.workflowId, 'WF5.1');
    const cancelListRequest = await waitForAppListAfter(heldRequest.socket, cancelAction.seq, 'WF5.1');
    await requireSuccessResponse(cancelAction.id, 'workflow_cancel', 'WF5.1');
    if (!(await heldResponseStillParked(heldRequest.id))) fail('WF5.1: the held response left the park before the cancel response settled');
    await requireSuccessResponse(cancelListRequest.id, 'workflow_list', 'WF5.1');
    if (!(await heldResponseStillParked(heldRequest.id))) fail('WF5.1: the held response left the park before the authoritative list settled');
    // Parse the AUTHORITATIVE list generation/status from the list payload.
    const cancelListPayload = await listPayloadFor(cancelListRequest.id);
    const cancelListEntry = (cancelListPayload?.data?.workflows || [])
      .find((workflow) => workflow.workflowId === cancelWorkflowCreated.workflowId);
    if (!cancelListEntry) fail(`WF5.1: the authoritative list lost the cancelled workflow: ${JSON.stringify(cancelListPayload)}`);
    if (cancelListEntry.status !== 'cancelled') fail(`WF5.1: authoritative list status is not cancelled: ${JSON.stringify(cancelListEntry)}`);
    if (!Number.isSafeInteger(cancelListEntry.generation) || cancelListEntry.generation < 1) {
      fail(`WF5.1: authoritative list generation is not positive: ${JSON.stringify(cancelListEntry)}`);
    }
    // Exact single-flight while held: exactly one cancel action, exactly one
    // settlement list, still exactly one detail frame with the held id
    // in-flight.
    const cancelActionError = exactActionCountError(
      (await actionRequestsFor(heldRequest.socket, 'workflow_cancel', cancelWorkflowCreated.workflowId)).length,
      1,
      'workflow_cancel request for the workflow'
    );
    if (cancelActionError !== null) fail(`WF5.1: ${cancelActionError} (more than one workflow_cancel request was observed)`);
    const cancelListError = exactActionCountError(
      await appListCountAfter(heldRequest.socket, cancelAction.seq),
      1,
      'application-socket workflow_list after the cancel action'
    );
    if (cancelListError !== null) fail(`WF5.1: ${cancelListError}`);
    if (await detailFramesFor(cancelWorkflowCreated.workflowId) !== 1) fail('WF5.1: a SECOND workflow_detail request was observed while the pre-click response was held');
    if (await detailPendingFor(cancelWorkflowCreated.workflowId) !== 1) fail('WF5.1: the pre-click request left the in-flight set before its response was released');
    // One poll period of observation: the SAME held response remains parked
    // (id in the in-flight set) and still no second physical.
    await sleep(1600);
    if (!(await heldResponseStillParked(heldRequest.id))) fail('WF5.1: the held response did not stay parked through the action');
    if (await detailFramesFor(cancelWorkflowCreated.workflowId) !== 1) fail('WF5.1: a SECOND workflow_detail request appeared during the held window');

    // Release the held response: the stale result is discarded and EXACTLY
    // ONE new current-generation detail fires, with a DIFFERENT request id,
    // whose exact success response must carry identity + generation +
    // status matching the authoritative list.
    if (await releaseHeldDetail() !== 1) fail('WF5.1: failed to release the held response');
    const releaseDeadline = Date.now() + 20000;
    while (Date.now() < releaseDeadline) {
      if (await detailFramesFor(cancelWorkflowCreated.workflowId) >= 2) break;
      await sleep(100);
    }
    await sleep(1600);
    if (await detailFramesFor(cancelWorkflowCreated.workflowId) !== 2) fail(`WF5.1: expected EXACTLY 2 workflow_detail requests total (held + one current-gen refresh), got ${await detailFramesFor(cancelWorkflowCreated.workflowId)}`);
    const cancelFrameIds = await page.evaluate(
      (wfId) => window.__wfDetailState.frames.filter((frame) => frame.workflowId === wfId).map((frame) => frame.id),
      cancelWorkflowCreated.workflowId
    );
    if (cancelFrameIds.length !== 2 || cancelFrameIds[0] !== heldRequest.id || cancelFrameIds[1] === heldRequest.id) {
      fail('WF5.1: the refresh request did not carry a fresh request id distinct from the held one');
    }
    await requireSuccessResponse(cancelFrameIds[1], 'workflow_detail', 'WF5.1');
    const freshDetailPayload = await listPayloadFor(cancelFrameIds[1]);
    const freshDetail = freshDetailPayload?.data ?? null;
    if (!freshDetail || freshDetail.id !== cancelWorkflowCreated.workflowId) {
      fail(`WF5.1: the fresh detail response lost the workflow identity: ${JSON.stringify(freshDetailPayload)}`);
    }
    if (freshDetail.generation !== cancelListEntry.generation || freshDetail.status !== cancelListEntry.status) {
      fail(`WF5.1: fresh detail generation/status ${JSON.stringify({ generation: freshDetail.generation, status: freshDetail.status })} does not match the authoritative list ${JSON.stringify({ generation: cancelListEntry.generation, status: cancelListEntry.status })}`);
    }
    if (await detailPendingFor(cancelWorkflowCreated.workflowId) !== 0) fail('WF5.1: the in-flight set did not drain after both detail responses settled');
    if (await countPanelErrors() !== 0) fail('WF5.1: panel error row after Cancel settlement');
    if (await countErrorToasts() !== 0) fail('WF5.1: error toast after Cancel settlement');
    record('WF5.1');
    await stageShot(page, 'workflow-t5-cancelled-desktop.png');

    // WF5.2: Remove success is a terminal tombstone — the exact Remove button
    // deletes the workflow; with the pre-click detail response held, ZERO new
    // workflow_detail requests may fire even after the held response is
    // released (the stale not-found failure is discarded by tombstone +
    // request epoch, never an error-message regex).
    const removeWorkflowCreated = await rpc(sessionA, {
      type: 'workflow_create', name: 'remove-wf', objective: 'remove-wf-objective',
    });
    if (!removeWorkflowCreated.workflowId) fail('WF5.2: remove-wf create response missing workflowId');
    await waitLiveWorkflow(sessionA, removeWorkflowCreated.workflowId, 'WF5.2');
    await waitFor(
      page,
      (wfId) => Boolean(document.getElementById(`workflow-row-${wfId}`)),
      'WF5.2: remove-wf row never appeared in the workflow list',
      30000,
      removeWorkflowCreated.workflowId
    );
    // Arm the gate for the EXACT workflowId, select the row, and require the
    // full held precondition before the Remove click.
    await armDetailHold(removeWorkflowCreated.workflowId);
    await page.click(`#workflow-row-${removeWorkflowCreated.workflowId}`);
    const heldRemoveRequest = await waitForHeldResponse(removeWorkflowCreated.workflowId, 'WF5.2: no detail request was physically received with its response held');
    const removeFramesBeforeClick = await detailFramesFor(removeWorkflowCreated.workflowId);
    if (removeFramesBeforeClick !== 1) fail(`WF5.2: expected exactly 1 pre-click workflow_detail frame for remove-wf, got ${removeFramesBeforeClick}`);
    await waitFor(
      page,
      () => {
        const button = document.getElementById('workflow-remove-btn');
        return Boolean(button) && !button.hasAttribute('disabled');
      },
      'WF5.2: Remove button never became available',
      15000
    );
    // With the pre-click response physically held: record the EXACT outgoing
    // workflow_remove request id and the EXACT subsequent application-socket
    // workflow_list id, then wait for BOTH exact success responses while the
    // old detail response stays parked. No DOM mutation is used as the
    // settlement signal.
    await page.click('#workflow-remove-btn');
    const removeAction = await waitForActionRequest(heldRemoveRequest.socket, 'workflow_remove', removeWorkflowCreated.workflowId, 'WF5.2');
    const removeListRequest = await waitForAppListAfter(heldRemoveRequest.socket, removeAction.seq, 'WF5.2');
    await requireSuccessResponse(removeAction.id, 'workflow_remove', 'WF5.2');
    if (!(await heldResponseStillParked(heldRemoveRequest.id))) fail('WF5.2: the held response left the park before the remove response settled');
    await requireSuccessResponse(removeListRequest.id, 'workflow_list', 'WF5.2');
    // Parse the AUTHORITATIVE list: the removed id must be gone forever.
    const removeListPayload = await listPayloadFor(removeListRequest.id);
    const removeListEntry = (removeListPayload?.data?.workflows || [])
      .find((workflow) => workflow.workflowId === removeWorkflowCreated.workflowId);
    if (removeListEntry) fail(`WF5.2: the authoritative list still contains the removed workflow: ${JSON.stringify(removeListEntry)}`);
    if (!(await heldResponseStillParked(heldRemoveRequest.id))) fail('WF5.2: the held response did not stay parked through the authoritative tombstone list');
    if (await detailFramesFor(removeWorkflowCreated.workflowId) !== 1) fail('WF5.2: a SECOND workflow_detail request was observed after the Remove click');
    if (await detailPendingFor(removeWorkflowCreated.workflowId) !== 1) fail('WF5.2: the held remove-wf request left the in-flight set before its response was released');
    // The authoritative tombstone landed: the row leaves the list. This is a
    // downstream projection check of the ACCEPTED list — never the
    // settlement signal itself.
    await waitFor(
      page,
      (wfId) => !document.getElementById(`workflow-row-${wfId}`),
      'WF5.2: removed workflow row never left the workflow list',
      30000,
      removeWorkflowCreated.workflowId
    );
    // Release the held response: the server answered for a workflow whose
    // removal is already authoritative — the app must discard it silently
    // and NEVER detail the removed id again: zero new detail REQUESTS and
    // zero new detail RESPONSES forever after the authoritative list.
    if (await releaseHeldDetail() !== 1) fail('WF5.2: failed to release the held response');
    await sleep(1600);
    const removeFramesAfterRelease = await detailFramesFor(removeWorkflowCreated.workflowId);
    if (removeFramesAfterRelease !== 1) fail(`WF5.2: expected ZERO new workflow_detail requests after releasing the held response, got ${removeFramesAfterRelease - 1}`);
    if (await detailResponsesFor(removeWorkflowCreated.workflowId) !== 1) {
      fail('WF5.2: a NEW workflow_detail response was observed after the authoritative remove (only the released stale response may settle)');
    }
    if (await detailPendingFor(removeWorkflowCreated.workflowId) !== 0) fail('WF5.2: the released stale response did not settle out of the in-flight set');
    if (await countPanelErrors() !== 0) fail('WF5.2: panel error row after Remove settlement');
    if (await countErrorToasts() !== 0) fail('WF5.2: error toast after Remove settlement');
    if (await bodyHasText('workflow was not found')) fail('WF5.2: "workflow was not found" surfaced after an authoritative remove');
    // Reassert the terminal action envelope after the remove/list settlement.
    // The action itself is exact-once. Lists are deliberately bounded 1..2:
    // one runAction settlement refresh is required, while workflow_removed
    // may drive one documented event refresh for the selected row.
    const settledRemoveActionError = exactActionCountError(
      (await actionRequestsFor(heldRemoveRequest.socket, 'workflow_remove', removeWorkflowCreated.workflowId)).length,
      1,
      'workflow_remove request for the workflow'
    );
    if (settledRemoveActionError !== null) fail(`WF5.2: ${settledRemoveActionError}`);
    const settledRemoveLists = await appListRequestsAfter(heldRemoveRequest.socket, removeAction.seq);
    const settledRemoveListError = boundedActionCountError(
      settledRemoveLists.length,
      1,
      2,
      'application-socket workflow_list requests after the remove action'
    );
    if (settledRemoveListError !== null) fail(`WF5.2: ${settledRemoveListError} (one settlement list plus at most one workflow_removed event refresh)`);
    if (settledRemoveLists[0]?.id !== removeListRequest.id) {
      fail(`WF5.2: the first post-remove list changed identity: expected ${removeListRequest.id}, got ${JSON.stringify(settledRemoveLists[0]?.id)}`);
    }
    for (const listRequest of settledRemoveLists) {
      await requireSuccessResponse(listRequest.id, 'workflow_list', 'WF5.2');
      const payload = await listPayloadFor(listRequest.id);
      const removedEntry = (payload?.data?.workflows || [])
        .find((workflow) => workflow.workflowId === removeWorkflowCreated.workflowId);
      if (removedEntry) {
        fail(`WF5.2: post-remove list ${listRequest.id} resurrected the removed workflow: ${JSON.stringify(removedEntry)}`);
      }
    }
    record('WF5.2');

    // WF5.3: removed/terminal UI at desktop and mobile widths.
    await page.setViewportSize({ width: 390, height: 844 });
    await sleep(1600);
    if (await countPanelErrors() !== 0) fail('WF5.3: panel error row at mobile width after removal');
    if (await countErrorToasts() !== 0) fail('WF5.3: error toast at mobile width after removal');
    await stageShot(page, 'workflow-t5-removed-mobile.png');
    await page.setViewportSize({ width: 1280, height: 800 });
    await sleep(1000);
    if (await countPanelErrors() !== 0) fail('WF5.3: panel error row at desktop width after removal');
    if (await countErrorToasts() !== 0) fail('WF5.3: error toast at desktop width after removal');
    await stageShot(page, 'workflow-t5-removed-desktop.png');
    record('WF5.3');

    // ALL gates must pass BEFORE anything is published: the exact-once
    // coverage gate over the explicit counts ledger AND the zero-browser-
    // error gate (console.error + pageerror captured with messages). Only
    // then is the staged set published as ONE immutable run directory plus
    // ONE atomically-renamed manifest/current pointer — never early, never
    // partially, never via sequential canonical replacement.
    // The REAL ledger: derived from the executedCounts Map itself — the
    // completeness gate below proves its insertion order elementwise-equal
    // to DOCUMENTED_IDS and every count exactly 1.
    const executedOrder = [...executedCounts.keys()];
    const counts = Object.fromEntries(executedCounts);
    const coverageError = documentedIdsCompletenessError(DOCUMENTED_IDS, executedCounts);
    const gateError = publicationGateError({ coverageError, browserErrors });
    if (gateError !== null) fail(`WF: ${gateError}`);
    stageWrite(
      'coverage-assertions.json',
      `${JSON.stringify({
        documented: [...DOCUMENTED_IDS],
        executed: executedOrder,
        counts,
        browserErrors: { count: browserErrors.length, messages: browserErrors },
      }, null, 2)}\n`
    );
    publishStagedArtifacts({
      lane: 'web-sessions-workflow',
      nonce: laneNonce,
      documented: [...DOCUMENTED_IDS],
      executed: executedOrder,
      counts,
      browserErrors: { count: browserErrors.length, messages: browserErrors },
    });
    console.log(`web-sessions-workflow: PASSED (${executedCounts.size}/${DOCUMENTED_IDS.length} assertions, every documented id exactly once, fail-closed probe, healthy compile->ship DAG completion + applied integration, exact-once counts + ownership, workflow_list isolation, terminal stability, panel Cancel/Remove single-flight without error toasts, ${browserErrors.length} browser errors) — evidence run ${runDirName} (pointer: ${path.join(evidence, 'current.json')})`);
    console.log(`web-sessions-workflow: assertion counts ${JSON.stringify(counts)}`);
  } finally {
    await browser.close().catch(() => {});
    // Anything still staged here was never published (publish renames the
    // staging dir away); drop it so no run-scoped staging survives.
    if (!published) cleanupStaging();
  }
}

if (process.env.RPI_SELF_CHECK === '1') {
  selfCheckStagedArtifactValidation();
  selfCheckPublicationFaults();
  // The fault-injection self-check ran against a throwaway evidence root;
  // remove it so a self-check leaves zero residue.
  fs.rmSync(evidence, { recursive: true, force: true });
  console.log('web-sessions-workflow: self-check PASS');
} else {
  main().catch((err) => {
    console.error(`web-sessions-workflow: playwright crashed: ${err && err.stack ? err.stack : err}`);
    // A crash (not a fail() assertion) must still leave zero staging residue
    // and zero published artifacts.
    if (!published) cleanupStaging();
    process.exit(2);
  });
}