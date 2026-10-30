import type { ReactElement } from 'react';

export type HostConnectionState =
  | 'off'
  | 'connecting'
  | 'on'
  | 'reconnecting'
  | 'token_required'
  | 'offline';

export type HostConnectionChipModel =
  | { kind: 'pulse' }
  | { kind: 'error'; text: string };

export const HOST_TOKEN_REQUIRED_ERROR = 'This listener requires an access token.';
export const HOST_RECONNECTING_STATUS = 'Reconnecting…';

export function hostConnectionChip(
  connState: HostConnectionState,
  stableError: string,
  streamStall = false,
): HostConnectionChipModel | null {
  if (streamStall || connState === 'connecting' || connState === 'reconnecting') {
    return { kind: 'pulse' };
  }
  if (connState === 'token_required' || connState === 'offline') {
    const text = stableError.trim();
    if (text === '') return null;
    return { kind: 'error', text };
  }
  return null;
}

export function HostConnectionChip(props: {
  connState: HostConnectionState;
  stableError: string;
  streamStall?: boolean;
}): ReactElement | null {
  const model = hostConnectionChip(props.connState, props.stableError, props.streamStall === true);
  if (!model) return null;
  if (model.kind === 'pulse') {
    return (
      <span
        id="host-connection-chip"
        className="host-connection-chip host-connection-chip--pulse chat-slow-pulse"
        data-state={props.connState}
        data-kind="pulse"
        role="status"
        aria-label={HOST_RECONNECTING_STATUS}
      />
    );
  }
  return (
    <span
      id="host-connection-chip"
      className="host-connection-chip host-connection-chip--error"
      data-state={props.connState}
      data-kind="error"
      role="status"
    >
      {model.text}
    </span>
  );
}
