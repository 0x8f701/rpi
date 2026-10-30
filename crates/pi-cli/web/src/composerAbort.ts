/** Stop tap issues abort for the displayed session. A client streaming flag is not a gate. */

export type ComposerAbortCommand = { type: 'abort'; sessionId: string };

export type ComposerAbortPlan =
  | { kind: 'idle' }
  | { kind: 'abort'; command: ComposerAbortCommand; streaming: false; footerStopped: true };

export type ComposerAbortTapResult = {
  footerStopped: boolean;
  command: ComposerAbortCommand | null;
};

export type ComposerActionClickKind = 'ignore' | 'abort' | 'submit';

export function planComposerAbort(sessionId: string | null | undefined): ComposerAbortPlan {
  if (typeof sessionId !== 'string' || sessionId === '') {
    return { kind: 'idle' };
  }
  return {
    kind: 'abort',
    command: { type: 'abort', sessionId },
    streaming: false,
    footerStopped: true,
  };
}

export function applyComposerAbortTap(sessionId: string | null | undefined): ComposerAbortTapResult {
  const plan = planComposerAbort(sessionId);
  if (plan.kind === 'idle') {
    return { footerStopped: false, command: null };
  }
  return { footerStopped: true, command: plan.command };
}

export function shouldHoldOptimisticStop(
  abortPending: boolean,
  incomingStreaming: boolean,
): boolean {
  return abortPending === true && incomingStreaming === true;
}

/** Display authority, not the active ref a warm reconnect rebinds to the listener primary. */
export function displayedSessionMatches(
  targetSessionId: string,
  displayedSessionId: string | null | undefined,
): boolean {
  return typeof displayedSessionId === 'string'
    && displayedSessionId !== ''
    && targetSessionId === displayedSessionId;
}

/** Idle snapshot while Stop is still pending. Only the displayed session toasts. */
export function abortIdleSnapshotToasts(
  abortPending: boolean,
  incomingStreaming: boolean,
  displayed: boolean,
): boolean {
  return abortPending === true && incomingStreaming === false && displayed === true;
}

export function composerActionClickKind(input: {
  stopArmed: boolean;
  abortGestureClick: boolean;
}): ComposerActionClickKind {
  if (input.abortGestureClick) return 'ignore';
  if (input.stopArmed) return 'abort';
  return 'submit';
}

export function composerAbortChrome(aborting: boolean): {
  dataAborting: 'true' | undefined;
  ariaPressed: true | undefined;
} {
  if (!aborting) {
    return { dataAborting: undefined, ariaPressed: undefined };
  }
  return { dataAborting: 'true', ariaPressed: true };
}

export function dropSessionAborting(
  aborting: Record<string, boolean>,
  sessionId: string,
): Record<string, boolean> {
  if (aborting[sessionId] !== true) return aborting;
  const next = { ...aborting };
  delete next[sessionId];
  return next;
}
