export const RECONNECT_INITIAL_DELAY_MS = 250;
export const RECONNECT_MAX_DELAY_MS = 5_000;
export const RECONNECT_OVERLAY_TEXT = '连接断开，正在重连…';
export const CONNECTING_OVERLAY_TEXT = '正在连接…';
export const BOOTSTRAP_RETRY_DELAY_MS = 1_500;
export const BOOTSTRAP_RETRY_LIMIT = 2;

export type ReconnectOverlayConnState =
  | 'off'
  | 'connecting'
  | 'on'
  | 'reconnecting'
  | 'token_required'
  | 'connected'
  | 'offline';

export function nextReconnectDelay(current: number): number {
  return Math.min(current * 2, RECONNECT_MAX_DELAY_MS);
}

export function reconnectDelaySchedule(
  initial: number = RECONNECT_INITIAL_DELAY_MS,
  max: number = RECONNECT_MAX_DELAY_MS,
  steps = 8,
): number[] {
  const delays = [initial];
  for (let i = 1; i < steps; i += 1) {
    delays.push(Math.min(delays[i - 1] * 2, max));
  }
  return delays;
}

export async function withBootstrapRetry<T>(
  work: () => Promise<T>,
  options: {
    isAbort: (error: unknown) => boolean;
    wait: (ms: number) => Promise<void>;
    delayMs?: number;
    limit?: number;
  },
): Promise<T> {
  const delayMs = options.delayMs ?? BOOTSTRAP_RETRY_DELAY_MS;
  const limit = options.limit ?? BOOTSTRAP_RETRY_LIMIT;
  let retries = 0;
  for (;;) {
    try {
      return await work();
    } catch (error) {
      if (options.isAbort(error)) throw error;
      if (retries >= limit) throw error;
      retries += 1;
      await options.wait(delayMs);
    }
  }
}

export function waitReconnectRetry(ms: number): Promise<void> {
  return new Promise((resolve) => {
    setTimeout(resolve, ms);
  });
}

export function reconnectOverlayVisible(
  connState: ReconnectOverlayConnState,
  boot = false,
): boolean {
  if (connState === 'token_required' || connState === 'off') return false;
  if (connState === 'on' || connState === 'connected') return boot;
  return connState === 'connecting'
    || connState === 'reconnecting'
    || connState === 'offline';
}

export function reconnectOverlayText(connState: ReconnectOverlayConnState): string {
  return connState === 'connecting' ? CONNECTING_OVERLAY_TEXT : RECONNECT_OVERLAY_TEXT;
}

export type RefusedHandshakePlan =
  | { kind: 'auth_hold'; connState: 'off' }
  | { kind: 'reconnect'; connState: 'reconnecting' };

export function planRefusedHandshake(input: {
  socketOpened: boolean;
  everConnected: boolean;
  hasToken: boolean;
  authFailed: boolean;
}): RefusedHandshakePlan {
  // A stored token plus a never-OPEN close (browser 1006) is also how
  // listener restart and proxy refuse look. Hold only when an authenticated
  // HTTP probe confirmed 401; otherwise keep bounded reconnect.
  if (!input.socketOpened && !input.everConnected && input.hasToken && input.authFailed) {
    return { kind: 'auth_hold', connState: 'off' };
  }
  return { kind: 'reconnect', connState: 'reconnecting' };
}

export type TransportOutageToastPlan =
  | { kind: 'skip' }
  | { kind: 'emit'; message: string; toastKind: 'transport'; timeoutMs: null };

export function planTransportOutageToast(input: {
  everConnected: boolean;
  outageActive: boolean;
  message: string;
}): TransportOutageToastPlan {
  if (!input.everConnected || input.outageActive) return { kind: 'skip' };
  return {
    kind: 'emit',
    message: input.message,
    toastKind: 'transport',
    timeoutMs: null,
  };
}
