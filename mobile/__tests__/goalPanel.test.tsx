// Goal panel: strict goal_get parsing (a failed load is not "no goal"), the
// mutation-only goal_* wire (no `activate`), lifecycle gating, pins, journal
// bounding, per-session response ownership, and single-flight mutations.
//
// Jest entry point: `cd mobile && npx jest __tests__/goalPanel.test.tsx`.
//
// Fixtures mirror crates/pi-coding/src/goal.rs (serde camelCase, snake_case
// lifecycles, `pins` omitted while empty) — never invented fields. Every case
// mounts through withPanel(), which unmounts in a finally.

import React from 'react';
import renderer, { act } from 'react-test-renderer';
import { Text } from 'react-native';
import { GoalPanel, MAX_GOAL_PINS, MAX_JOURNAL_ROWS } from '../src/screens/GoalPanel';
import type { RunnerClient } from '../src/runnerClient';
import { DARK } from '../src/ui';

type WireCommand = Record<string, unknown>;

interface Sent {
  command: WireCommand;
  sessionId?: string;
}

function harness(handler: (command: WireCommand, sessionId?: string) => unknown): {
  client: RunnerClient;
  sent: WireCommand[];
  scopes: Array<string | undefined>;
  subscribeFrames: jest.Mock;
} {
  const sent: WireCommand[] = [];
  const scopes: Array<string | undefined> = [];
  const subscribeFrames = jest.fn(() => () => {});
  const client = {
    command: async (command: WireCommand, sessionId?: string) => {
      sent.push(command);
      scopes.push(sessionId);
      return handler(command, sessionId);
    },
    subscribeFrames,
  } as unknown as RunnerClient;
  return { client, sent, scopes, subscribeFrames };
}

interface PendingCall extends Sent {
  resolve: (value: unknown) => void;
}

function deferred(): {
  client: RunnerClient;
  calls: PendingCall[];
  settle: (index: number, value: unknown) => Promise<void>;
} {
  const calls: PendingCall[] = [];
  const client = {
    command: (command: WireCommand, sessionId?: string) => new Promise<unknown>((resolve) => {
      calls.push({ command, sessionId, resolve });
    }),
    subscribeFrames: () => () => {},
  } as unknown as RunnerClient;
  return {
    client,
    calls,
    settle: async (index, value) => {
      await act(async () => {
        calls[index].resolve(value);
      });
    },
  };
}

function ok(command: string, data: unknown): unknown {
  return { type: 'response', command, success: true, data };
}

function fail(command: string, error: string): unknown {
  return { type: 'response', command, success: false, error };
}

const GOAL = {
  id: 'goal-1',
  objective: 'Ship the panel parity port',
  tokenBudget: 5,
  pins: ['keep tests green'],
  lifecycle: 'active',
  createdAt: '2026-09-17T10:00:00Z',
  updatedAt: '2026-09-17T10:05:00Z',
  usage: { tokensUsed: 2, activeTimeSeconds: 90 },
};

function goal(overrides: WireCommand = {}): WireCommand {
  return { ...GOAL, ...overrides };
}

/** `current` omitted is what Rust serializes for a session without a goal. */
function state(current: unknown, revision = 4): WireCommand {
  return current === undefined
    ? { revision, lifecycleRevision: revision }
    : { current, revision, lifecycleRevision: revision };
}

function event(kind: WireCommand, revision = 1, eventGoal: WireCommand = goal()): WireCommand {
  return { revision, timestamp: '2026-09-17T10:00:00Z', kind, goal: eventGoal };
}

/** A goal_get/goal_journal pair answering with `loaded` (undefined = no goal). */
function goalServer(loaded: unknown, journal: WireCommand[] = []): (command: WireCommand) => unknown {
  return (command) => {
    if (command.type === 'goal_get') return ok('goal_get', state(loaded));
    if (command.type === 'goal_journal') return ok('goal_journal', journal);
    return fail(String(command.type), `unexpected ${command.type}`);
  };
}

async function withPanel(
  client: RunnerClient,
  onError: (message: string) => void,
  run: (tree: renderer.ReactTestRenderer, setSession: (sessionId: string) => Promise<void>) => Promise<void>,
): Promise<void> {
  const element = (sessionId: string) => (
    <GoalPanel palette={DARK} client={client} sessionId={sessionId} onClose={() => {}} onError={onError} />
  );
  let tree!: renderer.ReactTestRenderer;
  await act(async () => {
    tree = renderer.create(element('s1'));
  });
  try {
    await run(tree, async (sessionId) => {
      await act(async () => {
        tree.update(element(sessionId));
      });
    });
  } finally {
    await act(async () => {
      tree.unmount();
    });
  }
}

/** Every rendered string leaf: what a reader of the panel can actually see. */
function renderedText(tree: renderer.ReactTestRenderer): string[] {
  return tree.root
    .findAllByType(Text)
    .flatMap((node) => (typeof node.props.children === 'string' ? [node.props.children] : []));
}

/** The same leaves as one blob, for substring assertions on composed rows. */
function text(tree: renderer.ReactTestRenderer): string {
  return renderedText(tree).join('\n');
}

function control(tree: renderer.ReactTestRenderer, label: string): renderer.ReactTestInstance {
  return tree.root.findByProps({ accessibilityLabel: label });
}

function hasLabel(tree: renderer.ReactTestRenderer, label: string): boolean {
  return tree.root.findAllByProps({ accessibilityLabel: label }, { deep: false }).length > 0;
}

function isDisabled(tree: renderer.ReactTestRenderer, label: string): boolean {
  return control(tree, label).props.disabled === true;
}

async function press(tree: renderer.ReactTestRenderer, label: string): Promise<void> {
  await act(async () => {
    control(tree, label).props.onPress();
  });
}

async function typeInto(tree: renderer.ReactTestRenderer, label: string, value: string): Promise<void> {
  await act(async () => {
    control(tree, label).props.onChangeText(value);
  });
}

it('renders a loaded goal with its budget, usage, pins and journal replay', async () => {
  const server = harness(goalServer(goal(), [
    event({ type: 'created' }, 1),
    event({ type: 'usage_updated', delta: { tokens: 2, activeTimeSeconds: 90 } }, 2),
  ]));
  await withPanel(server.client, () => {}, async (tree) => {
    const shown = text(tree);
    expect(control(tree, 'Goal status').props.children).toBe('active');
    expect(shown).toContain('Ship the panel parity port');
    expect(shown).toContain('5 token budget');
    expect(shown).toContain('2/5 tokens \u00B7 90s active \u00B7 40%');
    expect(shown).toContain('Pins (1/8)');
    expect(shown).toContain('keep tests green');
    expect(shown).toContain('created');
    expect(shown).toContain('usage +2 tokens');
    // A loaded goal replaces the create form.
    expect(hasLabel(tree, 'Create goal')).toBe(false);
    expect(hasLabel(tree, 'Goal objective input')).toBe(false);
    expect(server.sent).toStrictEqual([{ type: 'goal_get' }, { type: 'goal_journal' }]);
    expect(server.scopes).toStrictEqual(['s1', 's1']);
    expect(server.subscribeFrames).toHaveBeenCalled();
  });
});

it('shows the create form when a load succeeds without a goal', async () => {
  const server = harness(goalServer(undefined));
  const errors: string[] = [];
  await withPanel(server.client, (message) => errors.push(message), async (tree) => {
    expect(text(tree)).toContain('No goal. Create one.');
    expect(hasLabel(tree, 'Create goal')).toBe(true);
    expect(text(tree)).toContain('No goal events yet.');
    expect(errors).toStrictEqual([]);
    expect(hasLabel(tree, 'Goal status')).toBe(false);
  });
});

it('keeps last-good unknown state on a failed load and reports through onError', async () => {
  const server = harness((command) => (
    command.type === 'goal_get' ? fail('goal_get', 'runner busy') : ok('goal_journal', [])
  ));
  const errors: string[] = [];
  await withPanel(server.client, (message) => errors.push(message), async (tree) => {
    expect(text(tree)).toContain('--');
    expect(text(tree)).toContain('runner busy');
    expect(hasLabel(tree, 'Create goal')).toBe(false);
    expect(errors).toStrictEqual(['runner busy']);
  });
});

it('treats a malformed goal_get payload as a failed load, not an empty goal', async () => {
  const server = harness((command) => (
    command.type === 'goal_get'
      ? ok('goal_get', { current: { objective: 'no id, no lifecycle' }, revision: 4, lifecycleRevision: 4 })
      : ok('goal_journal', [])
  ));
  const errors: string[] = [];
  await withPanel(server.client, (message) => errors.push(message), async (tree) => {
    expect(text(tree)).toContain('malformed goal_get response');
    expect(text(tree)).toContain('--');
    expect(hasLabel(tree, 'Create goal')).toBe(false);
    expect(errors).toStrictEqual(['malformed goal_get response']);
    expect(text(tree)).not.toContain('no id, no lifecycle');
  });
});

it('keeps the last-good goal when a lifecycle action is refused', async () => {
  const server = harness((command) => (
    command.type === 'goal_pause' ? fail('goal_pause', 'runner busy') : goalServer(goal())(command)
  ));
  const errors: string[] = [];
  await withPanel(server.client, (message) => errors.push(message), async (tree) => {
    await press(tree, 'Pause goal');
    expect(errors).toStrictEqual(['runner busy']);
    expect(text(tree)).toContain('runner busy');
    // Refused action: the goal line, its gating and the journal are untouched.
    expect(control(tree, 'Goal status').props.children).toBe('active');
    expect(hasLabel(tree, 'Pause goal')).toBe(true);
    expect(server.sent).toStrictEqual([{ type: 'goal_get' }, { type: 'goal_journal' }, { type: 'goal_pause' }]);
  });
});

it('preserves the create draft when the create is refused', async () => {
  const server = harness((command) => (
    command.type === 'goal_create' ? fail('goal_create', 'objective rejected') : goalServer(undefined)(command)
  ));
  const errors: string[] = [];
  await withPanel(server.client, (message) => errors.push(message), async (tree) => {
    await typeInto(tree, 'Goal objective input', 'Draft objective');
    await press(tree, 'Create goal');
    expect(server.sent[2]).toStrictEqual({ type: 'goal_create', objective: 'Draft objective' });
    expect(errors).toStrictEqual(['objective rejected']);
    expect(control(tree, 'Goal objective input').props.value).toBe('Draft objective');
    expect(hasLabel(tree, 'Create goal')).toBe(true);
  });
});

it('creates a goal with the objective only and renders the returned goal', async () => {
  const created = goal({ id: 'goal-2', objective: 'Created objective', pins: undefined, tokenBudget: undefined });
  const server = harness((command) => (
    command.type === 'goal_create'
      ? ok('goal_create', created)
      : command.type === 'goal_get'
        ? ok('goal_get', state(undefined))
        : ok('goal_journal', [])
  ));
  await withPanel(server.client, () => {}, async (tree) => {
    await typeInto(tree, 'Goal objective input', '  Created objective  ');
    await press(tree, 'Create goal');
    // Mutation-only wire: no `activate`, and no token-budget control exists.
    expect(server.sent[2]).toStrictEqual({ type: 'goal_create', objective: 'Created objective' });
    expect(text(tree)).toContain('Created objective');
    expect(hasLabel(tree, 'Create goal')).toBe(false);
    expect(text(tree)).toContain('no token budget');
    expect(text(tree)).toContain('No pins.');
  });
});

it('gates lifecycle actions and surfaces the pause reason', async () => {
  const cases: Array<[WireCommand, string[], string]> = [
    [goal(), ['Pause goal', 'Complete goal', 'Drop goal'], 'active'],
    [goal({ lifecycle: 'paused', pauseReason: 'budget_exhausted' }), ['Resume goal', 'Complete goal', 'Drop goal'], 'paused (budget_exhausted)'],
    [goal({ lifecycle: 'completed' }), [], 'completed'],
    [goal({ lifecycle: 'dropped' }), [], 'dropped'],
  ];
  for (const [loaded, labels, status] of cases) {
    const server = harness(goalServer(loaded));
    await withPanel(server.client, () => {}, async (tree) => {
      for (const label of ['Pause goal', 'Resume goal', 'Complete goal', 'Drop goal']) {
        expect(hasLabel(tree, label)).toBe(labels.includes(label));
      }
      expect(control(tree, 'Goal status').props.children).toBe(status);
      expect(text(tree)).toContain('Ship the panel parity port');
    });
  }
});

it('sends mutation-only payloads for the lifecycle actions', async () => {
  const server = harness((command) => (
    command.type === 'goal_resume'
      ? ok('goal_resume', goal({ lifecycle: 'active' }))
      : goalServer(goal({ lifecycle: 'paused', pauseReason: 'manual' }))(command)
  ));
  await withPanel(server.client, () => {}, async (tree) => {
    await press(tree, 'Resume goal');
    expect(server.sent[2]).toStrictEqual({ type: 'goal_resume' });
    expect(control(tree, 'Goal status').props.children).toBe('active');
    expect(hasLabel(tree, 'Pause goal')).toBe(true);
    expect(hasLabel(tree, 'Resume goal')).toBe(false);
    // The settled mutation re-syncs the journal.
    expect(server.sent[3]).toStrictEqual({ type: 'goal_journal' });
  });
});

it('adds pins with the typed text, removes pins by index, and clears the pin draft on success', async () => {
  const server = harness((command) => {
    if (command.type === 'goal_pin') return ok('goal_pin', goal({ pins: ['one', 'two', 'third'] }));
    if (command.type === 'goal_unpin') return ok('goal_unpin', goal({ pins: ['one'] }));
    return goalServer(goal({ pins: ['one', 'two'] }))(command);
  });
  await withPanel(server.client, () => {}, async (tree) => {
    await press(tree, 'Remove pin 2');
    expect(server.sent[2]).toStrictEqual({ type: 'goal_unpin', index: 1 });
    expect(text(tree)).not.toContain('two');

    await typeInto(tree, 'Goal pin', '  third  ');
    await press(tree, 'Add pin');
    expect(server.sent[4]).toStrictEqual({ type: 'goal_pin', text: 'third' });
    expect(control(tree, 'Goal pin').props.value).toBe('');
    expect(text(tree)).toContain('third');
  });
});

it('disables pinning with an empty draft and refuses a pin at the cap', async () => {
  const empty = harness(goalServer(goal({ pins: undefined })));
  await withPanel(empty.client, () => {}, async (tree) => {
    expect(isDisabled(tree, 'Add pin')).toBe(true);
    await typeInto(tree, 'Goal pin', 'first');
    expect(isDisabled(tree, 'Add pin')).toBe(false);
  });

  const full = harness(goalServer(goal({ pins: ['p1', 'p2', 'p3', 'p4', 'p5', 'p6', 'p7', 'p8'] })));
  await withPanel(full.client, () => {}, async (tree) => {
    expect(text(tree)).toContain(`Pins (${MAX_GOAL_PINS}/${MAX_GOAL_PINS})`);
    await typeInto(tree, 'Goal pin', 'ninth');
    expect(isDisabled(tree, 'Add pin')).toBe(true);
    await press(tree, 'Add pin');
    expect(full.sent).toStrictEqual([{ type: 'goal_get' }, { type: 'goal_journal' }]);
  });
});

it('bounds the journal to the newest rows and keeps them when a refresh fails', async () => {
  const history = Array.from({ length: 60 }, (_, index) => (
    event({ type: 'usage_updated', delta: { tokens: index + 1, activeTimeSeconds: 0 } }, index + 1)
  ));
  let journalCalls = 0;
  const server = harness((command) => {
    if (command.type === 'goal_complete') return ok('goal_complete', goal({ lifecycle: 'completed' }));
    if (command.type === 'goal_journal') {
      journalCalls += 1;
      return journalCalls === 1 ? ok('goal_journal', history) : fail('goal_journal', 'journal down');
    }
    return ok('goal_get', state(goal()));
  });
  await withPanel(server.client, () => {}, async (tree) => {
    expect(hasLabel(tree, 'Journal 1')).toBe(true);
    expect(hasLabel(tree, `Journal ${MAX_JOURNAL_ROWS}`)).toBe(true);
    expect(hasLabel(tree, `Journal ${MAX_JOURNAL_ROWS + 1}`)).toBe(false);
    // Oldest-first replay, newest kept: the first row is revision 11.
    expect(text(tree)).toContain('usage +11 tokens');
    expect(text(tree)).not.toContain('usage +10 tokens');

    await press(tree, 'Complete goal');
    expect(control(tree, 'Goal status').props.children).toBe('completed');
    expect(text(tree)).toContain('journal down');
    // The failed refresh kept the last-good replay.
    expect(hasLabel(tree, 'Journal 1')).toBe(true);
    expect(text(tree)).toContain('usage +11 tokens');
  });
});

it('runs one mutation at a time and re-syncs the journal only after it settles', async () => {
  const server = deferred();
  await withPanel(server.client, () => {}, async (tree) => {
    await server.settle(0, ok('goal_get', state(goal())));
    await server.settle(1, ok('goal_journal', []));
    await press(tree, 'Pause goal');
    expect(server.calls[2].command).toStrictEqual({ type: 'goal_pause' });
    expect(isDisabled(tree, 'Pause goal')).toBe(true);
    // A second press while the first is in flight is refused locally.
    await press(tree, 'Pause goal');
    expect(server.calls.length).toBe(3);
    await server.settle(2, ok('goal_pause', goal({ lifecycle: 'paused', pauseReason: 'manual' })));
    expect(control(tree, 'Goal status').props.children).toBe('paused (manual)');
    expect(server.calls[3].command).toStrictEqual({ type: 'goal_journal' });
  });
});

it('scopes every RPC to its session and drops a superseded session response', async () => {
  const server = deferred();
  await withPanel(server.client, () => {}, async (tree, setSession) => {
    expect(server.calls.map((call) => call.sessionId)).toStrictEqual(['s1', 's1']);
    await setSession('s2');
    expect(server.calls.map((call) => call.sessionId)).toStrictEqual(['s1', 's1', 's2', 's2']);
    await server.settle(2, ok('goal_get', state(goal({ objective: 'Second session goal' }))));
    await server.settle(3, ok('goal_journal', []));
    // The first session's responses arrive last and must not land.
    await server.settle(0, ok('goal_get', state(goal({ objective: 'First session goal' }))));
    await server.settle(1, ok('goal_journal', [event({ type: 'created' })]));
    expect(text(tree)).toContain('Second session goal');
    expect(text(tree)).not.toContain('First session goal');
    expect(text(tree)).toContain('No goal events yet.');
  });
});
