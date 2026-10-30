export type LoopAction =
  | { op: 'create'; interval: string; prompt: string }
  | { op: 'list' }
  | { op: 'update'; taskId: string; interval?: string; prompt?: string }
  | { op: 'delete'; taskId: string }
  | { op: 'cancel'; taskId: string };

export type GoalAction =
  | { op: 'show'; explicit?: boolean }
  | { op: 'create'; objective: string; tokenBudget?: number }
  | { op: 'pause' }
  | { op: 'resume' }
  | { op: 'complete' }
  | { op: 'drop' }
  | { op: 'pin'; text: string }
  | { op: 'pins' }
  | { op: 'unpin'; index: number };

export type TodoAction =
  | { op: 'open' }
  | { op: 'start' | 'done' | 'drop'; task: string }
  | { op: 'clear' };

export type SlashAction =
  | { type: 'compact'; mode: 'snap' }
  | { type: 'compact'; mode: 'llm'; customInstructions: string }
  | { type: 'skill'; name: string }
  | { type: 'model'; spec: string }
  | { type: 'loop'; action: LoopAction }
  | { type: 'goal'; action: GoalAction }
  | { type: 'todo'; action: TodoAction }
  | { type: 'error'; message: string };

export const WEB_COMPOSER_COMMANDS: Record<string, true> = {
  compact: true,
  skill: true,
  loop: true,
  goal: true,
  todo: true,
  model: true,
};

export function parseSlashLine(text: string): { name: string; args: string } | null {
  const trimmed = text.trim();
  if (!trimmed.startsWith('/')) return null;
  const rest = trimmed.slice(1);
  const match = rest.match(/^(\S+)\s*([\s\S]*)$/);
  if (!match) return null;
  return { name: match[1], args: match[2].trim() };
}

export function isIntervalToken(value: string): boolean {
  if (!value) return false;
  if (/^[0-9]+$/.test(value)) return true;
  if (value.length < 2) return false;
  const digits = value.slice(0, -1);
  const suffix = value.slice(-1);
  return digits.length > 0 && ['s', 'm', 'h', 'd'].includes(suffix) && /^[0-9]+$/.test(digits);
}

function parseLoopCreate(argument: string): { ok: true; action: LoopAction } | { ok: false; message: string } {
  const trimmed = argument.trim();
  const space = trimmed.search(/\s/);
  let interval: string | undefined;
  let prompt = trimmed;
  if (space !== -1) {
    const first = trimmed.slice(0, space);
    const rest = trimmed.slice(space).trimStart();
    if (isIntervalToken(first) && rest !== '') {
      interval = first;
      prompt = rest;
    }
  }
  if (interval === undefined) return { ok: false, message: 'usage: /loop [interval] <prompt>' };
  return { ok: true, action: { op: 'create', interval, prompt } };
}

export function parseLoopArgs(args: string): { ok: true; action: LoopAction } | { ok: false; message: string } {
  const argument = args.trim();
  if (!argument) return { ok: false, message: 'usage: /loop [interval] <prompt>' };
  const match = argument.match(/^(\S+)(?:\s+(.*))?$/);
  const subcommand = match ? match[1] : argument;
  const rest = (match && match[2] ? match[2] : '').trim();
  switch (subcommand) {
    case 'list':
      return rest === '' ? { ok: true, action: { op: 'list' } } : { ok: false, message: 'usage: /loop list' };
    case 'cancel':
    case 'delete': {
      const parts = rest.split(/\s+/).filter(Boolean);
      if (parts.length !== 1) return { ok: false, message: `usage: /loop ${subcommand} <id>` };
      return { ok: true, action: { op: subcommand, taskId: parts[0] } };
    }
    case 'update': {
      const parts = rest.split(/\s+/).filter(Boolean);
      if (parts.length < 2) return { ok: false, message: 'usage: /loop update <id> [interval] [prompt]' };
      const [taskId, ...fields] = parts;
      if (isIntervalToken(fields[0])) {
        return {
          ok: true,
          action: {
            op: 'update',
            taskId,
            interval: fields[0],
            ...(fields.length > 1 ? { prompt: fields.slice(1).join(' ') } : {}),
          },
        };
      }
      return { ok: true, action: { op: 'update', taskId, prompt: fields.join(' ') } };
    }
    case 'create':
      return parseLoopCreate(rest);
    default:
      return parseLoopCreate(argument);
  }
}

export function parseGoalArgs(args: string): { ok: true; action: GoalAction } | { ok: false; message: string } {
  const argument = args.trim();
  if (!argument || argument === 'show' || argument === 'get' || argument === 'inspect') {
    return argument
      ? { ok: true, action: { op: 'show', explicit: true } }
      : { ok: true, action: { op: 'show' } };
  }
  const parts = argument.split(/\s+/).filter(Boolean);
  const operation = parts[0];
  switch (operation) {
    case 'pause':
    case 'resume':
    case 'complete':
    case 'drop':
    case 'pins':
      return parts.length === 1 ? { ok: true, action: { op: operation } } : { ok: false, message: 'usage: /goal' };
    case 'pin': {
      const text = argument.slice(operation.length).trim();
      if (!text) return { ok: false, message: 'usage: /goal pin <text>' };
      return { ok: true, action: { op: 'pin', text } };
    }
    case 'unpin': {
      if (parts.length !== 2 || !/^[0-9]+$/.test(parts[1])) {
        return { ok: false, message: 'usage: /goal unpin <index>' };
      }
      return { ok: true, action: { op: 'unpin', index: Number(parts[1]) } };
    }
    case 'create':
    case 'set':
      return parseGoalCreate(argument.slice(operation.length).trim());
    default:
      return parseGoalCreate(argument);
  }
}

function parseGoalCreate(argument: string): { ok: true; action: GoalAction } | { ok: false; message: string } {
  const parts = argument.split(/\s+/).filter(Boolean);
  let tokenBudget: number | undefined;
  let start = 0;
  if (parts[0] === '--tokens') {
    const raw = parts[1];
    if (raw === undefined || !/^[0-9]+$/.test(raw) || Number(raw) === 0) {
      return { ok: false, message: '--tokens requires a positive integer' };
    }
    tokenBudget = Number(raw);
    start = 2;
  }
  const objective = parts.slice(start).join(' ');
  if (!objective.trim()) return { ok: false, message: 'goal objective must not be empty' };
  return { ok: true, action: { op: 'create', objective, ...(tokenBudget !== undefined ? { tokenBudget } : {}) } };
}

export function parseTodoArgs(args: string): { ok: true; action: TodoAction } | { ok: false; message: string } {
  const argument = args.trim();
  if (!argument || argument === 'list') return { ok: true, action: { op: 'open' } };
  const match = argument.match(/^(\S+)(?:\s+([\s\S]*))?$/);
  const operation = (match?.[1] ?? argument).toLowerCase();
  const rest = (match?.[2] ?? '').trim();
  switch (operation) {
    case 'start':
    case 'done':
    case 'drop':
      return rest ? { ok: true, action: { op: operation, task: rest } } : { ok: false, message: `usage: /todo ${operation} <task>` };
    case 'clear':
      return rest ? { ok: false, message: 'usage: /todo clear' } : { ok: true, action: { op: 'clear' } };
    default:
      return { ok: true, action: { op: 'open' } };
  }
}

export function resolveSlashAction(name: string, args: string): SlashAction {
  switch (name) {
    case 'compact':
      if (args.trim() === '--snap' || args.trim().startsWith('--snap')) {
        return { type: 'compact', mode: 'snap' };
      }
      return { type: 'compact', mode: 'llm', customInstructions: args.trim() };
    case 'skill': {
      const skillName = args.trim().split(/\s+/)[0] ?? '';
      if (!skillName) return { type: 'error', message: 'usage: /skill <name>' };
      return { type: 'skill', name: skillName };
    }
    case 'model': {
      const spec = args.trim();
      if (!spec) return { type: 'error', message: 'usage: /model <provider/id>' };
      return { type: 'model', spec };
    }
    case 'loop': {
      const parsed = parseLoopArgs(args);
      return parsed.ok ? { type: 'loop', action: parsed.action } : { type: 'error', message: parsed.message };
    }
    case 'goal': {
      const parsed = parseGoalArgs(args);
      return parsed.ok ? { type: 'goal', action: parsed.action } : { type: 'error', message: parsed.message };
    }
    case 'todo': {
      const parsed = parseTodoArgs(args);
      return parsed.ok ? { type: 'todo', action: parsed.action } : { type: 'error', message: parsed.message };
    }
    default:
      return { type: 'error', message: `unsupported command: /${name}` };
  }
}

export function goalWire(action: GoalAction): Record<string, unknown> {
  switch (action.op) {
    case 'show':
    case 'pins':
      return { type: 'goal_get' };
    case 'create':
      return {
        type: 'goal_create',
        objective: action.objective,
        ...(action.tokenBudget !== undefined ? { tokenBudget: action.tokenBudget } : {}),
        activate: true,
      };
    case 'pause':
      return { type: 'goal_pause' };
    case 'resume':
      return { type: 'goal_resume', activate: true };
    case 'complete':
      return { type: 'goal_complete' };
    case 'drop':
      return { type: 'goal_drop' };
    case 'pin':
      return { type: 'goal_pin', text: action.text };
    case 'unpin':
      return { type: 'goal_unpin', index: action.index };
  }
}

export function loopWire(action: LoopAction): Record<string, unknown> {
  switch (action.op) {
    case 'create':
      return { type: 'loop_create', interval: action.interval, prompt: action.prompt, fireImmediately: true, durable: false };
    case 'list':
      return { type: 'loop_list' };
    case 'update':
      return {
        type: 'loop_update',
        taskId: action.taskId,
        ...(action.interval !== undefined ? { interval: action.interval } : {}),
        ...(action.prompt !== undefined ? { prompt: action.prompt } : {}),
      };
    case 'delete':
      return { type: 'loop_delete', taskId: action.taskId };
    case 'cancel':
      return { type: 'loop_cancel', taskId: action.taskId };
  }
}

export function parseModelSpec(spec: string): { provider: string; modelId: string } | null {
  const slash = spec.indexOf('/');
  if (slash <= 0 || slash === spec.length - 1) return null;
  return { provider: spec.slice(0, slash), modelId: spec.slice(slash + 1) };
}
