/** Turn-level composer streaming. Inner `message_end` is not completion;
 *  `turn_terminal` is. Snapshots replace the flag from `activeTurnId`. */
export function planTurnStreaming(
  type: string,
  current: boolean,
  activeTurnId?: unknown,
): boolean {
  if (type === 'turn_terminal') return false;
  if (type === 'subscription_snapshot' || type === 'session_resync') {
    return typeof activeTurnId === 'string' && activeTurnId !== '';
  }
  if (
    type === 'turn_started'
    || type === 'turn_start'
    || type === 'message_start'
    || type === 'message_update'
    || type === 'tool_execution_start'
    || type === 'tool_execution_update'
  ) {
    return true;
  }
  return current;
}
