// P0 resource-metrics helper (Linux-only) for the web E2E harness.
// One-file API, no commands (no subprocesses): process RSS is read from
// /proc with Node fs, browser metrics come from CDP. Both I/O layers are
// injectable so tests can drive this module with fake fs / fake CDP.
// Metric definitions (all bytes unless noted):
//   rssBytes            sum of VmRSS over the process tree rooted at a pid
//                       (root + descendants; VmRSS is kB in /proc, converted
//                       to bytes here). Descendants come from
//                       /proc/<pid>/task/<pid>/children (thread-group ids).
//   threadCount         sum of Threads: over every readable process in the
//                       same tree (null when no process exposes Threads:).
//   tcpSockets          {total, established} TCP sockets visible to the tree
//                       root (per-pid /proc/<pid>/net/tcp(+6), the netns
//                       view — diagnostic only, null when unreadable).
//                       connectionFdSocketCount returns the root's own
//                       count; processTreeRssSnapshot's fdSocketCount sums
//                       the same per-process count over the whole tree.
//                       null when readdir/readlink is missing, when ANY
//                       process discovered in the tree has an unreadable
//                       status or fd table (EACCES/dumpable=0, a status or
//                       fd table racing exit), when any fd readlink misses
//                       after its fd directory was readable, or when the
//                       walk is truncated — fail-closed, not just the root.
//   slopeBytesPerMs     ordinary least squares over {tMs, rssBytes} samples,
//                       bytes per millisecond. null when <2 samples or the
//                       time span is 0.
//   plateau             true when the last `plateauWindowSamples` samples span
//                       <= `plateauToleranceBytes` (peak-to-peak RSS range).
//   jsHeapUsedBytes / jsHeapTotalBytes
//                       CDP Performance.getMetrics JSHeapUsedSize /
//                       JSHeapTotalSize (bytes).
//   domNodes / documents / jsEventListeners / layoutCount / recalcStyleCount
//                       CDP Performance.getMetrics Nodes / Documents /
//                       JSEventListeners / LayoutCount / RecalcStyleCount
//                       (counts). Performance durations (seconds) stay in the
//                       raw `metrics` dict, not in the curated fields.
//   switchMeasures      deltas between one before-capture and one
//                       after-capture snapshot taken around a session switch
//                       (null before a missing side; missing = sample failed,
//                       delta then null).
//
// Race policy (process tree): reads never throw for descendants racing exit
// (ENOENT/ESRCH-like failures are recorded as unreadable), a visited set
// prevents double counting, and the walk is bounded by `maxDescendants`.
// fdSocketCount is strictly fail-closed: an unreadable status on any
// discovered descendant, a truncated walk, or a per-fd readlink miss after
// a readable fd directory nulls the whole aggregate, because a partial sum
// can never distinguish a real release from a race. RSS/thread totals stay
// truthful over whatever was readable.
// A descendant spawned after its parent's children file was read is part of
// the next sample, not this one.
//
// No env is read or dumped; writeEvidenceJson redacts secret-looking keys
// and omits/normalizes path-shaped fields (cwd/path/retiredPath/filePath)
// plus path-shaped substrings inside scalar failure strings, so evidence
// keeps diagnostic names without host-local absolute paths.
//
// API:
//   processTreeRssSnapshot(rootPid, {fsp, now, maxDescendants}) -> snapshot|null
//   processTreeRssBytes(rootPid, {fsp}) -> Promise<number|null>
//   connectionFdSocketCount(rootPid, {fsp}) -> Promise<number|null>
//   browserMetrics(page, {cdp, cdpFactory, now}) -> Promise<obj>
//   captureResourceSnapshot({page, pid, fsp, cdp, cdpFactory, now}) -> Promise<sample>
//   sampleRssSlope(samples|{pid, samples=6, intervalMs=250, ...}, plateauOptions?) -> obj
//   switchMeasures(before, after, {label, durationMs}) -> obj
//   writeEvidenceJson(filePath, payload, {fsp, now}) -> Promise<{path, evidence}>
//   vmRssBytesFromStatus(statusText) -> number|null
//   threadsFromStatus(statusText) -> number|null
//   tcpConnectionCounts(tcpText) -> {total, established}
//   tcpConnectionCount(rootPid, {fsp}) -> Promise<{total, established}|null>
//   parseChildrenList(childrenText) -> number[]
//   redactValue(value) -> redacted deep copy
//   normalizeEvidencePath(value) -> diagnostic path token
//   redactPathSubstrings(text) -> string with embedded local paths normalized

import {
  readFile as defaultReadFile,
  writeFile as defaultWriteFile,
  mkdir as defaultMkdir,
  rename as defaultRename,
  readdir as defaultReaddir,
  readlink as defaultReadlink,
} from 'node:fs/promises';
import { dirname } from 'node:path';

const KB = 1024;
const defaultNow = () => Date.now();
const defaultDelay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

const defaultFs = {
  readFile: defaultReadFile,
  writeFile: defaultWriteFile,
  mkdir: defaultMkdir,
  rename: defaultRename,
  readdir: defaultReaddir,
  readlink: defaultReadlink,
};

export const EVIDENCE_SCHEMA = 'p0-resource-metrics.v1';

export const PLATEAU_DEFAULTS = Object.freeze({
  toleranceBytes: 8 * KB * KB,
  windowSamples: 5,
});

// CDP Performance.getMetrics name -> curated browserMetrics field.
export const BROWSER_METRIC_FIELDS = Object.freeze({
  JSHeapUsedSize: 'jsHeapUsedBytes',
  JSHeapTotalSize: 'jsHeapTotalBytes',
  Nodes: 'domNodes',
  Documents: 'documents',
  JSEventListeners: 'jsEventListeners',
  LayoutCount: 'layoutCount',
  RecalcStyleCount: 'recalcStyleCount',
});

// Secret-shaped evidence keys (never written to disk with values intact).
const SECRET_KEY_PATTERN = /token|secret|password|passwd|credential|authorization|api[-_]?key|cookie/i;
const PATH_KEY_PATTERN = /^(cwd|path|filePath|retiredPath|sessionsRoot|nativePath|file)$/i;
const ABSOLUTE_PATH_PATTERN = /^(?:\/|\\\\|[A-Za-z]:[\\/])/;
const LOCAL_PATH_LEAK_PATTERN = /(?:^|\/)(?:home|Users|mnt|opt|var\/folders)\//;
const EMBEDDED_PATH_PATTERN = /(?:(?:[A-Za-z]:)?\/(?:home|Users|mnt|opt|var\/folders)\/[^\s"'`]+|\/(?:tmp|var|opt|usr|root)\/[^\s"'`]+|[A-Za-z]:\\[^\s"'`]+)/g;

export function normalizeEvidencePath(value, { redactionText = '<path>' } = {}) {
  if (typeof value !== 'string' || value === '') return value;
  const unified = value.replace(/\\/g, '/');
  if (!ABSOLUTE_PATH_PATTERN.test(value) && !LOCAL_PATH_LEAK_PATTERN.test(unified)) {
    return value;
  }
  const base = unified.split('/').filter(Boolean).pop();
  return base ? `<path:${base}>` : redactionText;
}

export function redactPathSubstrings(text, { redactionText = '<path>' } = {}) {
  if (typeof text !== 'string' || text === '') return text;
  return text.replace(EMBEDDED_PATH_PATTERN, (match, offset) => {
    const before = text.slice(0, offset);
    const httpAt = Math.max(before.lastIndexOf('http://'), before.lastIndexOf('https://'));
    if (httpAt >= 0 && !/\s/.test(before.slice(httpAt))) return match;
    return normalizeEvidencePath(match, { redactionText });
  });
}

const procPath = (pid, file) => `/proc/${pid}/${file}`;

export function vmRssBytesFromStatus(statusText) {
  const m = /^VmRSS:\s+(\d+)\s+kB$/m.exec(String(statusText));
  return m === null ? null : Number(m[1]) * KB;
}

// Kernel thread-group count from a /proc/<pid>/status body; null when the
// field is absent (kernel threads, some containers).
export function threadsFromStatus(statusText) {
  const m = /^Threads:\s+(\d+)\s*$/m.exec(String(statusText));
  return m === null ? null : Number(m[1]);
}

// /proc/net/tcp(+6) line counts. The state field (st) is hex; 01 is
// TCP_ESTABLISHED. Header lines and malformed rows are skipped.
export function tcpConnectionCounts(tcpText) {
  let total = 0;
  let established = 0;
  for (const line of String(tcpText).split('\n').slice(1)) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    const columns = trimmed.split(/\s+/);
    const state = columns[3];
    if (!/^[0-9A-Fa-f]+$/.test(state || '')) continue;
    total += 1;
    if (state === '01') established += 1;
  }
  return { total, established };
}

// TCP sockets visible to rootPid's network namespace (per-pid net/tcp(+6)).
// Diagnostic-only release signal: null when neither file is readable (the
// process is gone, non-Linux, or the fs is a test fake without these paths).
export async function tcpConnectionCount(rootPid, { fsp = defaultFs } = {}) {
  if (!Number.isFinite(Number(rootPid))) return null;
  const root = Number(rootPid);
  let tcpText = null;
  let tcp6Text = null;
  try {
    tcpText = await fsp.readFile(procPath(root, 'net/tcp'), 'utf8');
  } catch (err) {
    if (err && !isReadMiss(err)) throw err;
  }
  try {
    tcp6Text = await fsp.readFile(procPath(root, 'net/tcp6'), 'utf8');
  } catch (err) {
    if (err && !isReadMiss(err)) throw err;
  }
  if (tcpText === null && tcp6Text === null) return null;
  let total = 0;
  let established = 0;
  for (const text of [tcpText, tcp6Text]) {
    if (text === null) continue;
    const counts = tcpConnectionCounts(text);
    total += counts.total;
    established += counts.established;
  }
  return { total, established };
}

export async function listenerEstablishedConnectionCount(rootPid, localPort, { fsp = defaultFs } = {}) {
  if (!Number.isFinite(Number(rootPid)) || !Number.isInteger(Number(localPort))) return null;
  const portHex = Number(localPort).toString(16).toUpperCase().padStart(4, '0');
  let readable = false;
  let established = 0;
  for (const file of ['net/tcp', 'net/tcp6']) {
    let text;
    try {
      text = await fsp.readFile(procPath(Number(rootPid), file), 'utf8');
      readable = true;
    } catch (err) {
      if (err && !isReadMiss(err)) throw err;
      continue;
    }
    for (const line of String(text).split('\n').slice(1)) {
      const columns = line.trim().split(/\s+/);
      if (columns.length < 4 || columns[3] !== '01') continue;
      const local = columns[1] || '';
      if (local.slice(local.lastIndexOf(':') + 1).toUpperCase() === portHex) established += 1;
    }
  }
  return readable ? established : null;
}

// Per-process socket fd count from /proc/<pid>/fd (socket:[ino] readlink
// targets). This is the connection-owned signal: netns /proc/<pid>/net/tcp
// is shared by every socket in the namespace and must not be used as a
// per-connection release gate. Returns null when the fd table is unreadable
// (fake fs, EACCES/dumpable=0, non-Linux) or when any fd that readdir listed
// can no longer be readlinked — the fd raced close between readdir and
// readlink, so the count is already stale and a partial sum is worse than
// no count.
export async function connectionFdSocketCount(rootPid, { fsp = defaultFs } = {}) {
  if (!Number.isFinite(Number(rootPid))) return null;
  return socketFdCountForPid(Number(rootPid), fsp);
}

// Per-pid socket fd count; null when the fd table is unreadable (missing
// readdir/readlink on the fake fs, EACCES/dumpable=0, process gone) or when
// any listed fd readlink misses after the directory was readable (fd raced
// close mid-scan) — never a partial count.
async function socketFdCountForPid(pid, fsp) {
  if (typeof fsp.readdir !== 'function' || typeof fsp.readlink !== 'function') return null;
  const fdDir = procPath(pid, 'fd');
  let entries;
  try {
    entries = await fsp.readdir(fdDir);
  } catch (err) {
    if (isReadMiss(err)) return null;
    throw err;
  }
  if (!Array.isArray(entries)) return null;
  let sockets = 0;
  for (const entry of entries) {
    const name = typeof entry === 'string' ? entry : (entry && entry.name);
    if (!name) continue;
    try {
      const target = await fsp.readlink(`${fdDir}/${name}`);
      if (typeof target === 'string' && target.startsWith('socket:')) sockets += 1;
    } catch (err) {
      if (isReadMiss(err)) return null; // fd entry vanished mid-scan: count already stale
      throw err;
    }
  }
  return sockets;
}

export function parseChildrenList(childrenText) {
  const pids = String(childrenText).match(/\d+/g) || [];
  return [...new Set(pids.map(Number))];
}

function isReadMiss(err) {
  return err && ['ENOENT', 'ESRCH', 'EACCES', 'EPERM'].includes(err.code);
}

// Snapshot of RSS over rootPid plus every living descendant discoverable via
// /proc/<pid>/task/<pid>/children. Returns null when the root process is gone
// (or its /proc entries are unreadable). Descendants that die mid-walk are
// recorded with rssBytes null and do not contribute to the RSS/thread totals;
// they (and a truncated walk) fail fdSocketCount closed instead.
export async function processTreeRssSnapshot(
  rootPid,
  { fsp = defaultFs, now = defaultNow, maxDescendants = 2048 } = {},
) {
  if (!Number.isFinite(Number(rootPid))) return null;
  const rootNum = Number(rootPid);

  let rootStatus;
  try {
    rootStatus = await fsp.readFile(procPath(rootNum, 'status'), 'utf8');
  } catch (err) {
    if (isReadMiss(err)) return null;
    throw err;
  }

  const visited = new Set([rootNum]);
  const queue = [rootNum];
  const processes = [];
  let unreadableCount = 0;
  let truncated = false;
  let threadTotal = 0;
  let threadProcs = 0;

  const pushProcs = (pid, rssBytes, unreadable, fdSockets) => {
    const entry = { pid, rssBytes, fdSockets: fdSockets ?? null };
    if (unreadable) {
      entry.unreadable = true;
      unreadableCount += 1;
    }
    processes.push(entry);
  };

  // Connection-owned socket signal: sum of per-process socket fds over the
  // tree. Fail-closed: a process whose fd table is unreadable (EACCES/
  // dumpable=0, fake fs without readdir/readlink, or a fd table that raced
  // exit between its status read and the fd walk) nulls the whole aggregate
  // so a release gate can never mistake a partial sum for a release. A
  // discovered descendant whose status is unreadable (raced exit before the
  // status read) also nulls it — its fd table is unknowable. Same for a
  // truncated walk: descendants beyond `maxDescendants` were never summed.
  const rootFdSockets = await socketFdCountForPid(rootNum, fsp);
  let fdSocketTotal = 0;
  // Set when any discovered process in the tree hides its fd table or
  // status, or when the walk is truncated: the whole aggregate nulls out
  // (fail-closed).
  let fdIncomplete = false;

  pushProcs(rootNum, vmRssBytesFromStatus(rootStatus), false, rootFdSockets);
  if (rootFdSockets !== null) {
    fdSocketTotal += rootFdSockets;
  } else {
    fdIncomplete = true;
  }
  const rootThreads = threadsFromStatus(rootStatus);
  if (rootThreads !== null) {
    threadTotal += rootThreads;
    threadProcs += 1;
  }
  while (queue.length > 0) {
    if (visited.size >= maxDescendants) {
      truncated = true;
      break;
    }
    const pid = queue.shift();
    let childrenText;
    try {
      childrenText = await fsp.readFile(procPath(pid, `task/${pid}/children`), 'utf8');
    } catch (err) {
      if (!isReadMiss(err)) throw err;
      continue; // process died or children are not visible; tree stays consistent
    }
    for (const childPid of parseChildrenList(childrenText)) {
      if (visited.has(childPid)) continue;
      if (visited.size + queue.length >= maxDescendants) {
        truncated = true;
        break;
      }
      visited.add(childPid);
      queue.push(childPid);
      let statusText;
      try {
        statusText = await fsp.readFile(procPath(childPid, 'status'), 'utf8');
      } catch (err) {
        if (isReadMiss(err)) {
          // Raced exit mid-walk: excluded from RSS/thread totals, but its
          // fd table is unknowable, so fdSocketCount fails closed too.
          pushProcs(childPid, null, true);
          fdIncomplete = true;
          continue;
        }
        throw err;
      }
      const childFdSockets = await socketFdCountForPid(childPid, fsp);
      pushProcs(childPid, vmRssBytesFromStatus(statusText), false, childFdSockets);
      const childThreads = threadsFromStatus(statusText);
      if (childThreads !== null) {
        threadTotal += childThreads;
        threadProcs += 1;
      }
      if (childFdSockets !== null) {
        fdSocketTotal += childFdSockets;
      } else {
        fdIncomplete = true;
      }
    }
  }

  let rssTotal = 0;
  for (const p of processes) {
    if (Number.isFinite(p.rssBytes)) rssTotal += p.rssBytes;
  }
  return {
    rootPid: rootNum,
    rssBytes: rssTotal,
    sampledAtMs: now(),
    processCount: processes.length,
    threadCount: threadProcs > 0 ? threadTotal : null,
    fdSocketCount: (fdIncomplete || truncated) ? null : fdSocketTotal,
    unreadableCount,
    truncated,
    processes,
  };
}


export async function processTreeRssBytes(rootPid, { fsp = defaultFs, maxDescendants = 2048 } = {}) {
  const snapshot = await processTreeRssSnapshot(rootPid, { fsp, maxDescendants });
  return snapshot === null ? null : snapshot.rssBytes;
}

// Curated CDP metrics for the browser under test. A CDP session is created
// from the Playwright page and detached before returning unless the caller
// opts into reuse with {cdpSession} (no detach) or provides {cdp} directly
// (pure fake for tests; page may be null).
export async function browserMetrics(page, { cdp = null, cdpSession = null, cdpFactory = null, now = defaultNow } = {}) {
  let session = cdp ?? cdpSession;
  let created = false;
  if (!session && cdpFactory) {
    session = await cdpFactory(page);
  } else if (!session && page) {
    session = await page.context().newCDPSession(page);
    created = true;
  }
  if (!session) {
    throw new Error('browserMetrics: no CDP source (pass page, opts.cdp, or opts.cdpFactory)');
  }
  try {
    await session.send('Performance.enable');
    const { metrics = [] } = await session.send('Performance.getMetrics');
    const byName = {};
    for (const { name, value } of metrics) byName[name] = value;
    const curated = {};
    for (const [cdpName, field] of Object.entries(BROWSER_METRIC_FIELDS)) {
      curated[field] = Number.isFinite(byName[cdpName]) ? byName[cdpName] : null;
    }
    return { sampledAtMs: now(), ...curated, metrics: byName };
  } finally {
    if (created) {
      try {
        session.detach();
      } catch {}
    }
  }
}

// One combined sample (RSS + browser) around which the driver brackets a
// session switch. Individual layers degrade to null instead of failing the
// whole sample. Shape: {tMs, rssBytes, fdSocketCount, jsHeapUsedBytes,
// jsHeapTotalBytes, domNodes, documents, jsEventListeners, layoutCount,
// recalcStyleCount, metrics, sampledAtMs?}.
export async function captureResourceSnapshot({ page = null, pid = null, fsp = defaultFs, cdp = null, cdpFactory = null, now = defaultNow } = {}) {
  const sample = { tMs: now(), rssBytes: null, fdSocketCount: null };
  if (pid !== null && pid !== undefined) {
    const snapshot = await processTreeRssSnapshot(pid, { fsp });
    sample.rssBytes = snapshot === null ? null : snapshot.rssBytes;
    sample.fdSocketCount = snapshot === null ? null : snapshot.fdSocketCount;
  }
  if (page !== null || cdp || cdpFactory) {
    const browser = await browserMetrics(page, { cdp, cdpFactory, now });
    Object.assign(sample, browser);
  }
  return sample;
}

// Sampling mode overloads the pure form: pass {pid, ...} to have the helper
// collect the samples itself (first sample immediate, then one per
// intervalMs; fs/clock/timer injectable), or pass a plain
// [{tMs, rssBytes}, ...] array and only get the summarizer.
export async function sampleRssSlope(samplesOrOptions, plateauOptions = {}) {
  if (Array.isArray(samplesOrOptions)) {
    return summarizeRssSamples(samplesOrOptions, plateauOptions);
  }
  if (samplesOrOptions === null || typeof samplesOrOptions !== 'object') {
    throw new TypeError('sampleRssSlope: pass [{tMs, rssBytes}, ...] or {pid, samples, intervalMs}');
  }
  const {
    pid,
    samples = 6,
    intervalMs = 250,
    fsp = defaultFs,
    now = defaultNow,
    delay = defaultDelay,
    maxDescendants = 2048,
  } = samplesOrOptions;
  if (!Number.isFinite(Number(pid))) {
    throw new Error(`sampleRssSlope: sampling mode needs a numeric pid (got ${pid})`);
  }
  const captured = [];
  for (let index = 0; index < samples; index += 1) {
    if (index > 0) await delay(intervalMs);
    captured.push({
      tMs: now(),
      rssBytes: await processTreeRssBytes(pid, { fsp, maxDescendants }),
    });
  }
  return { sourcePid: Number(pid), ...summarizeRssSamples(captured, plateauOptions) };
}

// Pure summarizer over [{tMs, rssBytes}, ...]. Non-finite rssBytes (tree
// vanished mid-sampling) are kept in `bytes` as null but excluded from
// slope/min/max/plateau math. bytes never reorders.
function summarizeRssSamples(
  samples,
  { plateauToleranceBytes = PLATEAU_DEFAULTS.toleranceBytes, plateauWindowSamples = PLATEAU_DEFAULTS.windowSamples } = {},
) {
  const pts = samples
    .filter((s) => s && Number.isFinite(s.tMs) && Number.isFinite(s.rssBytes))
    .map((s) => ({ t: s.tMs, y: s.rssBytes }))
    .sort((a, b) => a.t - b.t || a.y - b.y);

  const summary = {
    bytes: samples.map((s) => (s && Number.isFinite(s.rssBytes) ? s.rssBytes : null)),
    sampleCount: pts.length,
    tStartMs: pts.length > 0 ? pts[0].t : null,
    tEndMs: pts.length > 0 ? pts[pts.length - 1].t : null,
    elapsedMs: pts.length > 0 ? pts[pts.length - 1].t - pts[0].t : null,
    slopeBytesPerMs: null,
    slopeBytesPerSecond: null,
    interceptBytes: null,
    rssMinBytes: null,
    rssMaxBytes: null,
    plateau: { detected: false, toleranceBytes: plateauToleranceBytes, windowSamples: plateauWindowSamples, tailRangeBytes: null, tailSlopeBytesPerMs: null },
  };
  if (pts.length === 0) return summary;

  let min = Infinity;
  let max = -Infinity;
  let tMean = 0;
  let yMean = 0;
  for (const { t, y } of pts) {
    min = Math.min(min, y);
    max = Math.max(max, y);
    tMean += t;
    yMean += y;
  }
  tMean /= pts.length;
  yMean /= pts.length;
  summary.rssMinBytes = min;
  summary.rssMaxBytes = max;

  if (pts.length >= 2) {
    let cov = 0;
    let variance = 0;
    for (const { t, y } of pts) {
      cov += (t - tMean) * (y - yMean);
      variance += (t - tMean) * (t - tMean);
    }
    if (variance > 0) {
      const slope = cov / variance;
      summary.slopeBytesPerMs = slope;
      summary.slopeBytesPerSecond = slope * 1000;
      summary.interceptBytes = yMean - slope * tMean;
    }
  }

  const tail = pts.slice(Math.max(0, pts.length - plateauWindowSamples));
  if (tail.length >= 2) {
    let tMin = Infinity;
    let tMax = -Infinity;
    for (const { y } of tail) {
      tMin = Math.min(tMin, y);
      tMax = Math.max(tMax, y);
    }
    const range = tMax - tMin;
    summary.plateau.tailRangeBytes = range;
    summary.plateau.detected = range <= plateauToleranceBytes;
    if (tail.length >= 2) {
      const tMeanTail = tail.reduce((acc, p) => acc + p.t, 0) / tail.length;
      const yMeanTail = tail.reduce((acc, p) => acc + p.y, 0) / tail.length;
      let covTail = 0;
      let varTail = 0;
      for (const { t, y } of tail) {
        covTail += (t - tMeanTail) * (y - yMeanTail);
        varTail += (t - tMeanTail) * (t - tMeanTail);
      }
      if (varTail > 0) summary.plateau.tailSlopeBytesPerMs = covTail / varTail;
    }
  }
  return summary;
}

// Delta measures across one declared session switch. Deltas are null when the
// corresponding before/after value is missing, so a failed capture on one
// side can never be reported as growth.
export function switchMeasures(before, after, { label = '', durationMs = null } = {}) {
  const at = (snap, field) => (snap && Number.isFinite(snap[field]) ? snap[field] : null);
  const delta = (a, b) => (a !== null && b !== null ? b - a : null);
  const rssBeforeBytes = at(before, 'rssBytes');
  const rssAfterBytes = at(after, 'rssBytes');
  const heapBeforeBytes = at(before, 'jsHeapUsedBytes');
  const heapAfterBytes = at(after, 'jsHeapUsedBytes');
  const domBefore = at(before, 'domNodes');
  const domAfter = at(after, 'domNodes');
  return {
    label,
    durationMs,
    rssBeforeBytes,
    rssAfterBytes,
    rssDeltaBytes: delta(rssBeforeBytes, rssAfterBytes),
    jsHeapUsedBeforeBytes: heapBeforeBytes,
    jsHeapUsedAfterBytes: heapAfterBytes,
    jsHeapUsedDeltaBytes: delta(heapBeforeBytes, heapAfterBytes),
    domNodesBefore: domBefore,
    domNodesAfter: domAfter,
    domNodesDelta: delta(domBefore, domAfter),
  };
}

// RSS (and optional heap/DOM) change across a client disconnect. `released`
// is true only when rssDeltaBytes is a negative finite number; a missing
// sample on either side is null so the driver can fail closed instead of
// reporting a phantom release.
export function disconnectReleaseMeasures(connected, disconnected, { label = 'disconnect-release' } = {}) {
  const measures = switchMeasures(connected, disconnected, { label });
  const released = measures.rssDeltaBytes !== null ? measures.rssDeltaBytes < 0 : null;
  return { ...measures, released };
}

export function redactValue(value, { redactionText = '[REDACTED]' } = {}) {
  if (Array.isArray(value)) return value.map((item) => redactValue(item, { redactionText }));
  if (value && typeof value === 'object') {
    const out = {};
    for (const [key, val] of Object.entries(value)) {
      if (SECRET_KEY_PATTERN.test(key)) {
        out[key] = redactionText;
      } else if (PATH_KEY_PATTERN.test(key)) {
        out[key] = val == null ? val : normalizeEvidencePath(String(val));
      } else {
        out[key] = redactValue(val, { redactionText });
      }
    }
    return out;
  }
  if (typeof value === 'string') return redactPathSubstrings(value);
  return value;
}

// Structured evidence writer: JSON document stamped with the schema id, an
// ISO timestamp, and secret-key redaction. Atomic via tmp file + rename.
// Never dumps process.env; only the payload the caller built is written.
export async function writeEvidenceJson(filePath, payload, { fsp = defaultFs, now = defaultNow } = {}) {
  const writtenAtMs = now();
  const evidence = {
    schema: EVIDENCE_SCHEMA,
    writtenAtMs,
    writtenAtIso: new Date(writtenAtMs).toISOString(),
    evidence: redactValue(payload),
  };
  const tmpPath = `${filePath}.tmp`;
  await fsp.mkdir(dirname(filePath), { recursive: true });
  await fsp.writeFile(tmpPath, `${JSON.stringify(evidence, null, 2)}\n`, 'utf8');
  await fsp.rename(tmpPath, filePath);
  return { path: filePath, evidence };
}