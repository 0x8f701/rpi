/**
 * Deferred-promise capability mirroring `Promise.withResolvers` (ES2024;
 * Node ≥ 22) implemented on plain `new Promise`.
 *
 * The web modules share the node-runnable test harness (scripts/*.test.ts),
 * which the Test workflow executes under Node 20 (test.yml "Set up Node for
 * Web build" pins `node-version: 20`) — `Promise.withResolvers` does not
 * exist there, so any exercised path crashes the whole `npm run build`
 * (esbuild bundles the tests, then node runs each bundle). This helper is
 * also the compatibility-safe choice for the embedded single-file browser
 * bundle, since vite ships no polyfill step.
 */

export interface WithResolvers<T> {
  promise: Promise<T>;
  resolve: (value: T | PromiseLike<T>) => void;
  reject: (reason?: unknown) => void;
}

/** Create a promise plus its resolve/reject functions — the
 *  `Promise.withResolvers()` contract, minus the Node-22-only builtin. */
export function withResolvers<T>(): WithResolvers<T> {
  let resolve!: (value: T | PromiseLike<T>) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}
