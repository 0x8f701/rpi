import { planTurnStreaming } from '../src/turnStreaming';

function check(name: string, ok: boolean, detail?: string): void {
  if (!ok) throw new Error(`${name}${detail ? `: ${detail}` : ''}`);
  console.log(`ok ${name}`);
}

check('message_end keeps a live turn', planTurnStreaming('message_end', true) === true);
check('message_end does not start a turn', planTurnStreaming('message_end', false) === false);
check('turn_terminal ends the turn', planTurnStreaming('turn_terminal', true) === false);
check('turn_end is advisory', planTurnStreaming('turn_end', true) === true);
check('message_start starts the turn', planTurnStreaming('message_start', false) === true);
check('tool_execution_start keeps the turn live', planTurnStreaming('tool_execution_start', false) === true);
check('snapshot with activeTurnId is live', planTurnStreaming('subscription_snapshot', false, 'turn-1') === true);
check('snapshot without activeTurnId is idle', planTurnStreaming('subscription_snapshot', true, null) === false);
check('session_resync empty id is idle', planTurnStreaming('session_resync', true, '') === false);
check('queue_update does not flip the flag', planTurnStreaming('queue_update', true) === true);

console.log('turnStreaming.test: 10 assertions, 0 failure(s)');
