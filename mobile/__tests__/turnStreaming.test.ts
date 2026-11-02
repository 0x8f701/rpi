import { planTurnStreaming } from '../src/turnStreaming';

describe('planTurnStreaming', () => {
  it('does not treat message_end as completion', () => {
    expect(planTurnStreaming('message_end', true)).toBe(true);
    expect(planTurnStreaming('message_end', false)).toBe(false);
  });

  it('clears only on turn_terminal or an idle snapshot', () => {
    expect(planTurnStreaming('turn_terminal', true)).toBe(false);
    expect(planTurnStreaming('turn_end', true)).toBe(true);
    expect(planTurnStreaming('subscription_snapshot', false, 'turn-1')).toBe(true);
    expect(planTurnStreaming('subscription_snapshot', true, null)).toBe(false);
    expect(planTurnStreaming('session_resync', true, '')).toBe(false);
  });

  it('starts or keeps the turn on live work events', () => {
    expect(planTurnStreaming('message_start', false)).toBe(true);
    expect(planTurnStreaming('tool_execution_start', false)).toBe(true);
    expect(planTurnStreaming('tool_execution_update', false)).toBe(true);
    expect(planTurnStreaming('queue_update', true)).toBe(true);
  });
});
