if (typeof Promise.withResolvers !== 'function') {
  Promise.withResolvers = function withResolvers() {
    let resolve;
    let reject;
    const promise = new Promise((resolvePromise, rejectPromise) => {
      resolve = resolvePromise;
      reject = rejectPromise;
    });
    return { promise, resolve, reject };
  };
}

if (typeof globalThis.navigator === 'undefined') {
  globalThis.navigator = { userAgent: 'node' };
}

if (typeof globalThis.WebSocket === 'undefined') {
  globalThis.WebSocket = { CONNECTING: 0, OPEN: 1, CLOSING: 2, CLOSED: 3 };
}
