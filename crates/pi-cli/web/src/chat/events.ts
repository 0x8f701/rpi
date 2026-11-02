/**
 * Per-runner live-event subscriber registry for the /chat console.
 *
 * Listeners are keyed by runnerId and live at module scope so a reconnect
 * generation reuses them — events resume flowing without a second subscribe
 * (and without duplicating callbacks). No frame buffer: consumers keep their
 * own state. The per-runner set is bounded.
 */

export const MAX_RUNNER_EVENT_LISTENERS = 8;

export type RunnerEventListener = (frame: unknown) => void;

const listenersByRunner = new Map<string, Set<RunnerEventListener>>();

export function subscribeRunnerEvents(
  runnerId: string,
  cb: RunnerEventListener,
): () => void {
  const id = runnerId.trim();
  if (!id) return () => {};
  let listeners = listenersByRunner.get(id);
  if (!listeners) {
    listeners = new Set();
    listenersByRunner.set(id, listeners);
  }
  if (listeners.size >= MAX_RUNNER_EVENT_LISTENERS && !listeners.has(cb)) {
    return () => {};
  }
  listeners.add(cb);
  return () => {
    const current = listenersByRunner.get(id);
    if (!current) return;
    current.delete(cb);
    if (current.size === 0) listenersByRunner.delete(id);
  };
}

export function dispatchRunnerEvent(runnerId: string, frame: unknown): void {
  const listeners = listenersByRunner.get(runnerId);
  if (!listeners || listeners.size === 0) return;
  for (const cb of [...listeners]) cb(frame);
}

export function runnerEventListenerCount(runnerId: string): number {
  return listenersByRunner.get(runnerId)?.size ?? 0;
}
