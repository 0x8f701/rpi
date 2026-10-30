import { rpcErrorCode, type RpcErrorCode } from './types';
import { routeCommandSession } from './goal';

export const SESSION_PROCESSING_CODE = 'session_processing' as const satisfies RpcErrorCode;

export type ComposerSubmitKind = 'prompt' | 'steer';

export function planComposerSubmitKind(opts: {
  streaming: boolean;
  requested: ComposerSubmitKind;
}): ComposerSubmitKind {
  if (opts.streaming) return 'steer';
  return opts.requested;
}

export function promptFailureRpcCode(error: unknown): RpcErrorCode | undefined {
  if (error === null || typeof error !== 'object' || Array.isArray(error) || !('rpcCode' in error)) {
    return undefined;
  }
  return rpcErrorCode({ code: error.rpcCode });
}

export function planPromptBusySteerRetry(opts: {
  submittedKind: ComposerSubmitKind;
  rpcCode: RpcErrorCode | undefined;
  convertedOnce: boolean;
}): 'steer' | 'none' {
  if (opts.convertedOnce) return 'none';
  if (opts.submittedKind !== 'prompt') return 'none';
  if (opts.rpcCode !== SESSION_PROCESSING_CODE) return 'none';
  return 'steer';
}

export function rpcFailureToastSuppressed(opts: {
  commandType: string;
  rpcCode: RpcErrorCode | undefined;
}): boolean {
  return opts.commandType === 'prompt' && opts.rpcCode === SESSION_PROCESSING_CODE;
}

export function composerSubmitWire(
  kind: ComposerSubmitKind,
  message: string,
): { type: 'prompt' | 'steer'; message: string } {
  return kind === 'steer' ? { type: 'steer', message } : { type: 'prompt', message };
}

/** The session a composer-dispatched command must be addressed to: the
 *  DISPLAYED command target — the same identity an id-less typed prompt
 *  resolves to (sendCommand's implicit target). Slash dispatch stamps an
 *  EXPLICIT `command.sessionId`, and routeCommandSession lets an explicit id
 *  win over the active session, so stamping `sessionIdRef.current` instead
 *  addresses the command to the listener PRIMARY after a warm reconnect: the
 *  bootstrap re-binds that ref while the displayed surface — and its
 *  `commandTarget` — stays where the user was. Boot, before any display
 *  authority is bound, falls back to the active session. */
export function composerCommandSessionId(
  displayed: { sessionId?: unknown } | null | undefined,
  activeSid: string | null,
): string {
  return routeCommandSession({ sessionId: displayed?.sessionId }, activeSid);
}

export async function dispatchComposerSubmit<T>(opts: {
  kind: ComposerSubmitKind;
  send: (kind: ComposerSubmitKind) => Promise<T>;
  onConvertToSteer: () => void;
}): Promise<{ kind: ComposerSubmitKind; data: T; converted: boolean }> {
  try {
    const data = await opts.send(opts.kind);
    return { kind: opts.kind, data, converted: false };
  } catch (error) {
    const rpcCode = promptFailureRpcCode(error);
    if (planPromptBusySteerRetry({
      submittedKind: opts.kind,
      rpcCode,
      convertedOnce: false,
    }) !== 'steer') {
      throw error;
    }
    opts.onConvertToSteer();
    const data = await opts.send('steer');
    return { kind: 'steer', data, converted: true };
  }
}
