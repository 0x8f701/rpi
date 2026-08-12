#!/usr/bin/env node
// Focused unit regression for the React-free job-card elapsed projection.
// The running-details modal must render elapsed immediately rather than wait
// for the panel's one-second clock tick.
//   - a live (queued/running) job ALWAYS yields a duration for a real clock;
//   - only the not-started sentinel (now === 0) yields null while live;
//   - settled jobs use the finished - started window.
// Bundled by `npm run build` (esbuild) and executed before the production
// bundle, so an elapsed regression fails the build.
import {
  elapsedMs,
  formatDuration,
  SUBAGENT_FILTER_LABEL,
  subagentJobBucket,
  type JobStatus,
  type JobWire,
} from '../src/jobCard.ts';

const failures = [];
let ran = 0;
function check(name, cond, detail) {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

/** Minimal live job; override fields per scenario. */
function job(status: JobStatus, overrides: Partial<JobWire> = {}): JobWire {
  return {
    id: 'j-1',
    agentId: 'writer',
    agent: 'writer',
    parentId: '',
    status,
    createdAt: 1_000,
    startedAt: 1_000,
    finishedAt: null,
    ...overrides,
  };
}

// ---- formatDuration: boundary formatting ----
check('formatDuration 0ms', formatDuration(0) === '0ms');
check('formatDuration 999ms stays ms', formatDuration(999) === '999ms');
check('formatDuration 1000ms is 1s', formatDuration(1_000) === '1s');
check('formatDuration 59s stays s', formatDuration(59_999) === '59s');
check('formatDuration 60s is 1m 0s', formatDuration(60_000) === '1m 0s');
check('formatDuration 61.5s is 1m 1s', formatDuration(61_500) === '1m 1s');
check('formatDuration 1h is 60m 0s (no hours unit)', formatDuration(3_600_000) === '60m 0s');

// ---- elapsedMs: live window (queued/running) ----
const t = 5_000;
// With a real clock the live label is available on the first render.
check('live job yields non-null for a real clock', elapsedMs(job('running'), t) !== null);
check('running elapsed counts from startedAt', elapsedMs(job('running'), t) === 4_000);
check('queued elapsed falls back to createdAt', elapsedMs(job('queued', { startedAt: null }), t) === 4_000);
check('now === 0 sentinel yields null while live', elapsedMs(job('running'), 0) === null);
check('now === 0 sentinel yields null while queued', elapsedMs(job('queued', { startedAt: null }), 0) === null);
check('elapsed clamps at 0 before start', elapsedMs(job('running', { startedAt: 9_000 }), t) === 0);

// ---- elapsedMs: settled window (finished - started) ----
check('completed uses finished - started', elapsedMs(job('completed', { finishedAt: 9_000 }), t) === 8_000);
check('completed without finishedAt is null', elapsedMs(job('completed', { finishedAt: null }), t) === null);
check('failed uses finished - started', elapsedMs(job('failed', { finishedAt: 8_000 }), t) === 7_000);
check('cancelled without window is null', elapsedMs(job('cancelled', { finishedAt: null }), t) === null);

// ---- subagentJobBucket: Running = live work (queued+running), else Completed ----
check('queued is Running (fresh spawn stays visible)', subagentJobBucket('queued') === 'running');
check('running is Running', subagentJobBucket('running') === 'running');
check('completed is Completed', subagentJobBucket('completed') === 'completed');
check('failed is Completed', subagentJobBucket('failed') === 'completed');
check('cancelled is Completed', subagentJobBucket('cancelled') === 'completed');
check('Running label is Running', SUBAGENT_FILTER_LABEL.running === 'Running');
check('Completed label is Completed', SUBAGENT_FILTER_LABEL.completed === 'Completed');

// A settled job must never stay in the Running bucket: the panel list filter
// derives purely from status, so the live->settled transition moves the card.
check(
  'live->settled moves bucket',
  subagentJobBucket('running') !== subagentJobBucket('completed'),
);

if (failures.length > 0) {
  console.error(`jobCard: ${failures.length}/${ran} checks FAILED:\n- ${failures.join('\n- ')}`);
  process.exit(1);
}
console.log(`jobCard: ${ran} checks passed`);
