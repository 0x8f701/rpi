/**
 * Goal RPC routing pure helpers — shared by App.tsx's refreshGoal flow and the
 * node-runnable regression test (scripts/goal.test.ts). No DOM/browser/socket
 * dependency so `esbuild --platform=node` can bundle the test in isolation.
 *
 * The routing rule mirrors App.tsx `sendCommand` exactly: an explicit
 * `command.sessionId` wins over the ACTIVE session; a null/empty/absent
 * sessionId falls back to the active session. Centralizing it here makes the
 * background `goal_updated` / lifecycle A→B cross-write invariant testable
 * without a WebSocket (the P0 was refreshGoal sending `goal_get`/`goal_journal`
 * with no sessionId, so a background refresh routed to the ACTIVE session and
 * painted the wrong session's goal into the target's cache).
 */

/** Resolve the session a command routes to. An explicit non-empty
 *  `command.sessionId` (lifecycle/background targeting another session) wins
 *  over `activeSid`; otherwise the active session is used; otherwise '' (boot
 *  / create commands omit it → no `sessionId` field is stamped). */
export function routeCommandSession(
  command: { sessionId?: unknown },
  activeSid: string | null,
): string {
  const explicit = command.sessionId;
  if (typeof explicit === 'string' && explicit !== '') return explicit;
  if (typeof activeSid === 'string' && activeSid !== '') return activeSid;
  return '';
}

/** `goal_get` command targeted at the OWNING session `sid`, so a background
 *  refresh can never query the active session's goal. */
export function goalGetCommand(sid: string): { type: 'goal_get'; sessionId: string } {
  return { type: 'goal_get', sessionId: sid };
}

/** `goal_journal` command targeted at the OWNING session `sid`. */
export function goalJournalCommand(sid: string): { type: 'goal_journal'; sessionId: string } {
  return { type: 'goal_journal', sessionId: sid };
}