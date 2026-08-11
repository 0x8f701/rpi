#!/usr/bin/env node
// Focused goal RPC routing regression for src/goal.ts — the pure command
// routing rule that fixes the refreshGoal P0: goal_get/goal_journal must carry
// the OWNING sessionId so a background `goal_updated` for B while A is active
// (or a lifecycle A→B refresh captured before sessionIdRef advances) routes
// to B's runtime, not the active session's. Run through `npm run build`.
//
// Exit codes: 0 = every assertion held; 1 = a regression.
//
// Assertions exercise the routing DECISION (which session a command routes
// to), not source strings.
import { routeCommandSession, goalGetCommand, goalJournalCommand } from '../src/goal.ts';
const failures = [];
let ran = 0;
function check(name, cond, detail) {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

// ---- goal commands carry the owning sessionId ----
{
  check('goal_get carries sessionId', goalGetCommand('B').sessionId === 'B');
  check('goal_get type', goalGetCommand('B').type === 'goal_get');
  check('goal_journal carries sessionId', goalJournalCommand('B').sessionId === 'B');
  check('goal_journal type', goalJournalCommand('B').type === 'goal_journal');
}

// ---- the P0 fix: a goal command for B routes to B even when A is active ----
{
  // Background goal_updated for B while A is active.
  check('goal_get for B routes to B even when A active', routeCommandSession(goalGetCommand('B'), 'A') === 'B');
  check('goal_journal for B routes to B even when A active', routeCommandSession(goalJournalCommand('B'), 'A') === 'B');
  // Lifecycle A→B: refreshGoal(B) captured BEFORE sessionIdRef advances to B
  // (active is still A). The explicit sessionId on the command wins, so B's
  // goal is fetched from B's runtime, not A's.
  check('lifecycle A->B: goal_get for B routes to B while active still A', routeCommandSession(goalGetCommand('B'), 'A') === 'B');
}

// ---- the bug (pre-fix shape): no sessionId routes to the active session ----
{
  // Without an explicit sessionId, the command routes to the active session —
  // this is the cross-write: refreshGoal(B) with a bare {type:'goal_get'}
  // would query A and paint A's goal into B's cache. The fix stamps sessionId.
  check('pre-fix bare goal_get routes to active A (the bug)', routeCommandSession({ type: 'goal_get' }, 'A') === 'A');
}

// ---- routing edge cases ----
{
  // Explicit sessionId on an arbitrary command wins over active.
  check('explicit sessionId wins over active', routeCommandSession({ type: 'close_session', sessionId: 'X' }, 'A') === 'X');
  // Empty-string sessionId falls back to the active session.
  check('empty sessionId falls back to active', routeCommandSession({ type: 'goal_get', sessionId: '' }, 'A') === 'A');
  // Non-string sessionId falls back to active.
  check('non-string sessionId falls back to active', routeCommandSession({ type: 'goal_get', sessionId: 123 }, 'A') === 'A');
  // Boot/create: no sessionId and no active -> '' (no field stamped).
  check('boot command with no active -> empty', routeCommandSession({ type: 'get_state' }, null) === '');
  check('boot command with empty active -> empty', routeCommandSession({ type: 'get_state' }, '') === '');
  // No sessionId, active present -> active.
  check('no sessionId -> active', routeCommandSession({ type: 'get_state' }, 'A') === 'A');
}

// ---- end-to-end invariant: B's goal never reads A's runtime ----
{
  // Simulate the two RPCs refreshGoal(B) issues, with A active, and confirm
  // BOTH route to B — so B's cache is populated from B's runtime, not A's.
  const active = 'A';
  const target = 'B';
  const cmds = [goalGetCommand(target), goalJournalCommand(target)];
  const routed = cmds.map((c) => routeCommandSession(c, active));
  check('refreshGoal(B) routes both RPCs to B (no A cross-write)', routed.every((r) => r === 'B'), JSON.stringify(routed));
}

console.log(`\ngoal.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);