/**
 * Pure slash-command dispatch decisions for the Web composer submit path.
 *
 * Takes a parseSupportedCommand result ({name, args}) and maps it to the
 * concrete RPC / panel action Main should take. No DOM/socket dependency —
 * scripts/slashParse.test.ts and scripts/loopGoal.test.ts exercise the
 * decision table.
 *
 * Supported surface (matches WEB_SUPPORTED_COMMANDS):
 *   /compact [--snap | instructions…]
 *   /skill <name>
 *   /code-review [<base> | <from> <to>]
 *   /commit [<ref>] — bare opens the recent-commit chooser, one ref reviews
 *   that pinned commit (usage line mirrors interactive_commands.rs)
 *   /loop [list|cancel <id>|delete <id>|update <id> [interval] [prompt]|create <interval> <prompt>|<interval> <prompt>]
 *   /goal [show|inspect|create [--tokens N] <objective>|pause|resume|complete|drop|pin <text>|pins|unpin <index>]
 *   /ps (bare only)
 *   /jobs (bare only)
 *   /irc (bare only)
 *   /todo [list|new [label]|select <id>|send [id] <message>|add <id> [options] <task>|status [id]|pause [id]|resume [id]|stop [id]|cancel [id]|archive <id>|start <task>|done <task>|drop <task>|clear|<markdown>]
 */

import { parseCodeReviewArgs } from './codeReview';

/** One parsed `/loop` invocation — mirrors `InteractiveLoopCommand` in
 *  crates/pi-cli/src/loop_commands.rs (the TUI parser is the authority). */
export type LoopAction =
  | { op: 'create'; interval: string; prompt: string }
  | { op: 'list' }
  | { op: 'update'; taskId: string; interval?: string; prompt?: string }
  | { op: 'delete'; taskId: string }
  | { op: 'cancel'; taskId: string };

/** One parsed `/goal` invocation — mirrors `InteractiveGoalCommand` in
 *  crates/pi-cli/src/goal_commands.rs (the TUI parser is the authority).
 *  `show` distinguishes the BARE `/goal` (which the TUI maps to its Goal
 *  panel, so the Web composer opens the Goal panel) from an explicit
 *  `show`/`get`/`inspect` (which dispatches the `goal_get` RPC). */
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

/** One parsed `/todo` invocation. Child operations retain the frozen
 * `todo_child_*` wire surface; ordinary DAG operations retain `todo_op`. */
export type TodoAction =
  | { op: 'open' }
  | { op: 'new'; label?: string }
  | { op: 'select'; childSessionId: string }
  | { op: 'send'; childSessionId?: string; message: string }
  | {
      op: 'add';
      childSessionId: string;
      content: string;
      dependsOn?: string[];
      agent?: string;
      role?: string;
      expectedRevision?: number;
    }
  | { op: 'status' | 'pause' | 'resume' | 'stop' | 'cancel'; childSessionId?: string }
  | { op: 'archive'; childSessionId: string }
  | { op: 'remove'; childSessionId: string }
  | { op: 'start' | 'done' | 'drop'; task: string }
  | { op: 'clear' };

export type SlashAction =
  | { type: 'compact'; mode: 'snap' }
  | { type: 'compact'; mode: 'llm'; customInstructions: string }
  | { type: 'skill'; name: string }
  | { type: 'code-review'; from?: string; to?: string; includeWorkingTree?: true }
  | { type: 'commit'; ref?: string }
  | { type: 'loop'; action: LoopAction }
  | { type: 'goal'; action: GoalAction }
  | { type: 'ps' }
  | { type: 'jobs' }
  | { type: 'irc' }
  | { type: 'todo'; action: TodoAction }
  | { type: 'error'; message: string };

/** TUI-equivalent usage lines (match loop_commands.rs / goal_commands.rs
 *  exactly, condensed from the multi-line create message to its usage line). */
const LOOP_CREATE_USAGE = 'usage: /loop [interval] <prompt>';
const LOOP_LIST_USAGE = 'usage: /loop list';
const LOOP_UPDATE_USAGE = 'usage: /loop update <id> [interval] [prompt]';
const LOOP_ID_USAGE = (verb: 'cancel' | 'delete') => `usage: /loop ${verb} <id>`;
const GOAL_USAGE =
  'usage: /goal [show|inspect|create [--tokens N] <objective>|pause|resume|complete|drop|pin <text>|pins|unpin <index>]';
const TODO_USAGE =
  'usage: /todo [list|new [label]|select <id>|send [id] <message>|add <id> [options] <task>|status [id]|pause [id]|resume [id]|stop [id]|cancel [id]|archive <id>|remove <id>|start <task>|done <task>|drop <task>|clear|<markdown>]';
const TODO_ADD_USAGE =
  'usage: /todo add <id> [--revision N] [--depends id,id] [--agent name] [--role role] <task>';
const TODO_START_USAGE = 'usage: /todo start <task>';
const TODO_DONE_USAGE = 'usage: /todo done <task>';
const TODO_DROP_USAGE = 'usage: /todo drop <task>';
const TODO_CLEAR_USAGE = 'usage: /todo clear';
const TODO_MARKDOWN_UNAVAILABLE =
  'Todo Markdown input is unavailable in Web; use the Todo panel controls instead';

/**
 * True when `value` is a valid loop interval token: a bare positive integer
 * (seconds) or digits followed by a single s/m/h/d suffix. Mirrors
 * `pi_coding::is_interval_token`.
 */
export function isIntervalToken(value: string): boolean {
  if (!value) return false;
  if (/^[0-9]+$/.test(value)) return true;
  if (value.length < 2) return false;
  const digits = value.slice(0, -1);
  const suffix = value.slice(-1);
  return digits.length > 0 && ['s', 'm', 'h', 'd'].includes(suffix) && /^[0-9]+$/.test(digits);
}

/** Parse the `/loop` primary surface: subcommand style
 *  `list | cancel <id> | delete <id> | update <id> [interval] [prompt] | create <interval> <prompt>`,
 *  falling back to the legacy bare create form `/loop <interval> <prompt>` when the
 *  first token is not a subcommand keyword. Mirrors
 *  `parse_loop_invocation` in loop_commands.rs — a bare `/loop` is a usage
 *  error (loop requires arguments), exactly like the TUI. */
export function parseLoopArgs(
  args: string,
): { ok: true; action: LoopAction } | { ok: false; message: string } {
  const argument = args.trim();
  if (!argument) return { ok: false, message: LOOP_CREATE_USAGE };
  const match = argument.match(/^(\S+)(?:\s+(.*))?$/);
  const subcommand = match ? match[1] : argument;
  const rest = (match && match[2] ? match[2] : '').trim();
  switch (subcommand) {
    case 'list':
      return rest === ''
        ? { ok: true, action: { op: 'list' } }
        : { ok: false, message: LOOP_LIST_USAGE };
    case 'cancel':
      return requiredLoopId(rest, 'cancel');
    case 'delete':
      return requiredLoopId(rest, 'delete');
    case 'update':
      return parseLoopUpdate(rest);
    case 'create':
      return parseLoopCreate(rest);
    default:
      // Legacy bare create form — subcommand keywords are never valid
      // interval tokens, so this cannot shadow a subcommand.
      return parseLoopCreate(argument);
  }
}

/** `/loop create <interval> <prompt>` and the legacy bare form. Mirrors
 *  `parse_create` + `pi_coding::parse_loop_args`: an unambiguous leading
 *  interval token; without one the invocation is a usage error. */
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
  if (interval === undefined) return { ok: false, message: LOOP_CREATE_USAGE };
  return { ok: true, action: { op: 'create', interval, prompt } };
}

/** `/loop update <id> [interval] [prompt]` — an id plus at least one field;
 *  a leading interval token selects interval+prompt, otherwise the whole tail
 *  is the prompt. Mirrors `parse_update` in loop_commands.rs. */
function parseLoopUpdate(argument: string): { ok: true; action: LoopAction } | { ok: false; message: string } {
  const parts = argument.split(/\s+/).filter((part) => part !== '');
  if (parts.length < 2) return { ok: false, message: LOOP_UPDATE_USAGE };
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

/** `/loop cancel <id>` / `/loop delete <id>` take exactly one id token.
 *  Mirrors `required_id` in loop_commands.rs. */
function requiredLoopId(
  argument: string,
  op: 'cancel' | 'delete',
): { ok: true; action: LoopAction } | { ok: false; message: string } {
  const parts = argument.split(/\s+/).filter((part) => part !== '');
  if (parts.length !== 1) return { ok: false, message: LOOP_ID_USAGE(op) };
  return { ok: true, action: { op, taskId: parts[0] } };
}

/**
 * Parse a `/goal` invocation: bare/`show`/`get`/`inspect` → show; lifecycle
 * verbs take no arguments; `pin <text>`; `pins`; `unpin <index>`;
 * `create|set [--tokens N] <objective>`; any other first token is the bare
 * create shorthand. Mirrors `parse_interactive_goal_command` in
 * goal_commands.rs. */
export function parseGoalArgs(
  args: string,
): { ok: true; action: GoalAction } | { ok: false; message: string } {
  const argument = args.trim();
  if (!argument || argument === 'show' || argument === 'get' || argument === 'inspect') {
    // Bare `/goal` opens the Goal panel (TUI parity: bare /goal -> panel);
    // explicit show/get/inspect dispatch the goal_get RPC. The `explicit`
    // flag lets Main tell them apart without re-parsing the wire text.
    return argument
      ? { ok: true, action: { op: 'show', explicit: true } }
      : { ok: true, action: { op: 'show' } };
  }
  const parts = argument.split(/\s+/).filter((part) => part !== '');
  const operation = parts[0];
  switch (operation) {
    case 'show':
    case 'get':
    case 'inspect':
      return parts.length === 1
        ? { ok: true, action: { op: 'show', explicit: true } }
        : { ok: false, message: GOAL_USAGE };
    case 'pause':
    case 'resume':
    case 'complete':
    case 'drop':
    case 'pins':
      return parts.length === 1
        ? { ok: true, action: { op: operation } }
        : { ok: false, message: GOAL_USAGE };
    case 'pin': {
      const text = argument.slice(operation.length).trim();
      if (!text) return { ok: false, message: 'usage: /goal pin <text>' };
      return { ok: true, action: { op: 'pin', text } };
    }
    case 'unpin': {
      if (parts.length !== 2) return { ok: false, message: 'usage: /goal unpin <index>' };
      const raw = parts[1];
      if (!/^[0-9]+$/.test(raw)) return { ok: false, message: 'usage: /goal unpin <index>' };
      const index = Number(raw);
      if (!Number.isSafeInteger(index)) return { ok: false, message: 'usage: /goal unpin <index>' };
      return { ok: true, action: { op: 'unpin', index } };
    }
    case 'create':
    case 'set':
      return parseGoalCreate(argument.slice(operation.length).trim());
    default:
      return parseGoalCreate(argument);
  }
}

/** `create [--tokens N] <objective>` — `--tokens` must precede the objective
 *  and take a positive integer; the rest is the objective verbatim. Mirrors
 *  `parse_create` in goal_commands.rs. */
function parseGoalCreate(argument: string): { ok: true; action: GoalAction } | { ok: false; message: string } {
  const parts = argument.split(/\s+/).filter((part) => part !== '');
  let tokenBudget: number | undefined;
  let objectiveParts: string[] = [];
  for (let i = 0; i < parts.length; i++) {
    if (parts[i] === '--tokens') {
      const raw = parts[i + 1];
      if (raw === undefined || !/^[0-9]+$/.test(raw)) {
        return { ok: false, message: '--tokens requires a positive integer' };
      }
      const value = Number(raw);
      if (!Number.isSafeInteger(value) || value === 0) {
        return { ok: false, message: '--tokens requires a positive integer' };
      }
      tokenBudget = value;
      i += 1;
    } else {
      objectiveParts = parts.slice(i);
      break;
    }
  }
  const objective = objectiveParts.join(' ');
  if (!objective.trim()) return { ok: false, message: 'goal objective must not be empty' };
  return {
    ok: true,
    action: { op: 'create', objective, ...(tokenBudget !== undefined ? { tokenBudget } : {}) },
  };
}

/** Parse the sole public Todo grammar. Known child verbs take precedence over
 * ordinary DAG verbs. Web has no existing Todo-Markdown parser, so Markdown
 * input is rejected locally instead of falling through as a model prompt. */
export function parseTodoArgs(
  args: string,
): { ok: true; action: TodoAction } | { ok: false; message: string } {
  const argument = args.trim();
  if (!argument) return { ok: true, action: { op: 'open' } };
  const match = argument.match(/^(\S+)(?:\s+([\s\S]*))?$/);
  const operation = (match?.[1] ?? argument).toLowerCase();
  const rest = (match?.[2] ?? '').trim();
  switch (operation) {
    case 'list':
      return rest
        ? { ok: false, message: 'usage: /todo list' }
        : { ok: true, action: { op: 'open' } };
    case 'new':
      return { ok: true, action: { op: 'new', ...(rest ? { label: rest } : {}) } };
    case 'select': {
      const parsed = parseTodoChildId(rest, 'usage: /todo select <id>', true);
      return parsed.ok
        ? { ok: true, action: { op: 'select', childSessionId: parsed.childSessionId } }
        : parsed;
    }
    case 'send':
      return parseTodoSend(rest);
    case 'add':
      return parseTodoAdd(rest);
    case 'status':
    case 'pause':
    case 'resume':
    case 'stop':
    case 'cancel': {
      const parsed = parseTodoChildId(rest, `usage: /todo ${operation} [id]`, false);
      return parsed.ok
        ? {
            ok: true,
            action: { op: operation, ...(parsed.childSessionId ? { childSessionId: parsed.childSessionId } : {}) },
          }
        : parsed;
    }
    case 'archive': {
      const parsed = parseTodoChildId(rest, 'usage: /todo archive <id>', true);
      return parsed.ok
        ? { ok: true, action: { op: 'archive', childSessionId: parsed.childSessionId } }
        : parsed;
    }
    case 'remove': {
      const parsed = parseTodoChildId(rest, 'usage: /todo remove <id>', true);
      return parsed.ok
        ? { ok: true, action: { op: 'remove', childSessionId: parsed.childSessionId } }
        : parsed;
    }
    case 'start':
      return rest
        ? { ok: true, action: { op: 'start', task: rest } }
        : { ok: false, message: TODO_START_USAGE };
    case 'done':
      return rest
        ? { ok: true, action: { op: 'done', task: rest } }
        : { ok: false, message: TODO_DONE_USAGE };
    case 'drop':
      return rest
        ? { ok: true, action: { op: 'drop', task: rest } }
        : { ok: false, message: TODO_DROP_USAGE };
    case 'clear':
      return rest
        ? { ok: false, message: TODO_CLEAR_USAGE }
        : { ok: true, action: { op: 'clear' } };
    default: {
      const first = argument.split('\n').find((line) => line.trim() !== '')?.trimStart() ?? '';
      const isMarkdown = first.startsWith('#')
        || first.startsWith('- ')
        || first.startsWith('* ')
        || first.startsWith('+ ');
      return isMarkdown
        ? { ok: false, message: TODO_MARKDOWN_UNAVAILABLE }
        : { ok: false, message: TODO_USAGE };
    }
  }
}


function parseTodoChildId(
  argument: string,
  usage: string,
  isRequired: true,
): { ok: true; childSessionId: string } | { ok: false; message: string };
function parseTodoChildId(
  argument: string,
  usage: string,
  isRequired: false,
): { ok: true; childSessionId?: string } | { ok: false; message: string };
function parseTodoChildId(
  argument: string,
  usage: string,
  isRequired: boolean,
): { ok: true; childSessionId?: string } | { ok: false; message: string } {
  const tokens = argument.split(/\s+/).filter(Boolean);
  if (tokens.length > 1 || (isRequired && tokens.length === 0)) {
    return { ok: false, message: usage };
  }
  const childSessionId = tokens[0];
  return { ok: true, ...(childSessionId ? { childSessionId } : {}) };
}

function parseTodoSend(
  argument: string,
): { ok: true; action: TodoAction } | { ok: false; message: string } {
  if (!argument) return { ok: false, message: 'usage: /todo send [id] <message>' };
  const match = argument.match(/^(\S+)\s+([\s\S]+)$/);
  if (match && match[1].length === 36 && [...match[1]].filter((character) => character === '-').length === 4) {
    const message = match[2].trim();
    if (!message) return { ok: false, message: 'usage: /todo send [id] <message>' };
    return { ok: true, action: { op: 'send', childSessionId: match[1], message } };
  }
  return { ok: true, action: { op: 'send', message: argument } };
}

function parseTodoAdd(
  argument: string,
): { ok: true; action: TodoAction } | { ok: false; message: string } {
  const tokens = argument.split(/\s+/).filter(Boolean);
  const childSessionId = tokens[0];
  if (!childSessionId) return { ok: false, message: TODO_ADD_USAGE };
  let expectedRevision: number | undefined;
  let dependsOn: string[] | undefined;
  let agent: string | undefined;
  let role: string | undefined;
  const content: string[] = [];
  for (let index = 1; index < tokens.length; index += 1) {
    const token = tokens[index];
    if (token === '--revision' || token === '--rev') {
      const raw = tokens[++index];
      if (raw === undefined) return { ok: false, message: TODO_ADD_USAGE };
      if (!/^[0-9]+$/.test(raw)) return { ok: false, message: 'revision must be an integer' };
      const value = Number(raw);
      if (!Number.isSafeInteger(value)) return { ok: false, message: 'revision must be an integer' };
      expectedRevision = value;
    } else if (token === '--depends') {
      const raw = tokens[++index];
      if (raw === undefined) return { ok: false, message: TODO_ADD_USAGE };
      dependsOn = raw.split(',').filter(Boolean);
    } else if (token === '--agent') {
      agent = tokens[++index];
      if (agent === undefined) return { ok: false, message: TODO_ADD_USAGE };
    } else if (token === '--role') {
      role = tokens[++index];
      if (role === undefined) return { ok: false, message: TODO_ADD_USAGE };
    } else {
      content.push(token);
    }
  }
  if (content.length === 0) return { ok: false, message: TODO_ADD_USAGE };
  return {
    ok: true,
    action: {
      op: 'add',
      childSessionId,
      content: content.join(' '),
      ...(dependsOn ? { dependsOn } : {}),
      ...(agent ? { agent } : {}),
      ...(role ? { role } : {}),
      ...(expectedRevision !== undefined ? { expectedRevision } : {}),
    },
  };
}


/**
 * True when `/compact` arguments select the deterministic snap path.
 * Mirrors the TUI rule: `--snap` alone or as a leading flag; any trailing
 * text after `--snap` is ignored (snap has no summarization instructions).
 */
export function isSnapCompactArgs(args: string): boolean {
  const trimmed = args.trim();
  if (!trimmed) return false;
  if (trimmed === '--snap') return true;
  return trimmed.startsWith('--snap') && /^--snap(?:\s|$)/.test(trimmed);
}

/**
 * Map a supported slash command + argument tail to a dispatch action.
 * Unknown names never reach this helper (parseSupportedCommand already
 * filters the Web surface); an unexpected name yields an error rather than
 * silently falling through to a prompt.
 */
export function resolveSlashAction(name: string, args: string): SlashAction {
  switch (name) {
    case 'compact': {
      if (isSnapCompactArgs(args)) {
        return { type: 'compact', mode: 'snap' };
      }
      return {
        type: 'compact',
        mode: 'llm',
        customInstructions: args.trim(),
      };
    }
    case 'skill': {
      const skillName = args.trim();
      if (!skillName) {
        return { type: 'error', message: 'usage: /skill <name>' };
      }
      // Skill names are a single token on the catalog; take the first token
      // so a pasted description after the name does not poison the RPC.
      const first = skillName.split(/\s+/)[0] ?? skillName;
      return { type: 'skill', name: first };
    }
    case 'code-review': {
      const parsed = parseCodeReviewArgs(args);
      if (!parsed.ok) {
        return { type: 'error', message: parsed.error };
      }
      if (parsed.from && parsed.to) {
        return { type: 'code-review', from: parsed.from, to: parsed.to };
      }
      if (parsed.from && parsed.includeWorkingTree) {
        return { type: 'code-review', from: parsed.from, includeWorkingTree: true };
      }
      return { type: 'code-review' };
    }
    case 'commit': {
      // Mirrors parse_commit_inspect_invocation (interactive_commands.rs):
      // bare opens the recent-commit chooser, one whitespace-delimited ref
      // reviews that pinned commit, any additional operand is the usage
      // error. Inspection only — no staging/checkout/commit here.
      const parts = args.trim().split(/\s+/).filter(Boolean);
      if (parts.length === 0) {
        return { type: 'commit' };
      }
      if (parts.length === 1) {
        return { type: 'commit', ref: parts[0] };
      }
      return { type: 'error', message: 'Usage: /commit [<ref>]' };
    }
    case 'loop': {
      const parsed = parseLoopArgs(args);
      if (!parsed.ok) {
        return { type: 'error', message: parsed.message };
      }
      return { type: 'loop', action: parsed.action };
    }
    case 'goal': {
      const parsed = parseGoalArgs(args);
      if (!parsed.ok) {
        return { type: 'error', message: parsed.message };
      }
      return { type: 'goal', action: parsed.action };
    }
    case 'ps': {
      if (args.trim()) {
        return { type: 'error', message: 'usage: /ps' };
      }
      return { type: 'ps' };
    }
    case 'jobs': {
      if (args.trim()) {
        return { type: 'error', message: 'usage: /jobs' };
      }
      return { type: 'jobs' };
    }
    case 'irc': {
      if (args.trim()) {
        return { type: 'error', message: 'usage: /irc' };
      }
      return { type: 'irc' };
    }
    case 'todo': {
      const parsed = parseTodoArgs(args);
      if (!parsed.ok) {
        return { type: 'error', message: parsed.message };
      }
      return { type: 'todo', action: parsed.action };
    }
    default:
      return { type: 'error', message: `unsupported command: /${name}` };
  }
}

/** Map a parsed Todo operation onto the existing RPC wire shapes. The
 * `todo_child_*` names and parent identity field stay frozen; DAG mutations
 * continue to use the flattened `todo_op` command. Bare/list has no wire. */
export function todoWire(action: TodoAction, parentSessionId: string): Record<string, unknown> | null {
  const childCommand = (type: string, extra: Record<string, unknown> = {}) => ({
    ...extra,
    type,
    parentSessionId,
  });
  switch (action.op) {
    case 'open':
      return null;
    case 'new':
      return childCommand('todo_child_create', action.label ? { label: action.label } : {});
    case 'select':
      return childCommand('todo_child_select', { childSessionId: action.childSessionId });
    case 'send':
      return childCommand('todo_child_send', {
        ...(action.childSessionId ? { childSessionId: action.childSessionId } : {}),
        message: action.message,
      });
    case 'add':
      return childCommand('todo_child_add_task', {
        childSessionId: action.childSessionId,
        content: action.content,
        ...(action.dependsOn ? { dependsOn: action.dependsOn } : {}),
        ...(action.agent ? { agent: action.agent } : {}),
        ...(action.role ? { role: action.role } : {}),
        ...(action.expectedRevision !== undefined ? { expectedRevision: action.expectedRevision } : {}),
      });
    case 'status':
    case 'pause':
    case 'resume':
    case 'stop':
    case 'cancel':
      return childCommand(`todo_child_${action.op}`, {
        ...(action.childSessionId ? { childSessionId: action.childSessionId } : {}),
      });
    case 'archive':
      return childCommand('todo_child_archive', { childSessionId: action.childSessionId });
    case 'remove':
      return childCommand('todo_child_remove', { childSessionId: action.childSessionId });
    case 'start':
      return { type: 'todo_op', op: 'start', task: action.task };
    case 'done':
      return { type: 'todo_op', op: 'done', task: action.task };
    case 'drop':
      return { type: 'todo_op', op: 'drop', task: action.task };
    case 'clear':
      return { type: 'todo_op', op: 'rm', cascade: true };
  }
}

/**
 * Map a parsed `/loop` action onto its existing RPC wire shape. Field names
 * mirror the backend `RpcCommand` serde: `loop_create` flattens
 * `LoopCreateRequest` (camelCase, `fireImmediately`), `loop_update` flattens
 * `LoopUpdateRequest` (`taskId`), `loop_delete`/`loop_cancel` take `taskId`.
 */
export function loopWire(action: LoopAction): Record<string, unknown> {
  switch (action.op) {
    case 'create':
      return {
        type: 'loop_create',
        interval: action.interval,
        prompt: action.prompt,
        // Mirrors LoopCreateRequest::immediate (the TUI create path).
        fireImmediately: true,
        durable: false,
      };
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

/**
 * Map a parsed `/goal` action onto its existing RPC wire shape. Field names
 * mirror the backend `RpcCommand` serde: `goal_create` takes `objective` +
 * `tokenBudget`, `goal_pin` takes `text`, `goal_unpin` takes `index`;
 * show/pins both use `goal_get` (the pins view formats the same state).
 *
 * Create/resume carry `activate: true` — the TUI parity switch on the RPC:
 * the backend then mirrors `/goal create`/`/goal resume` in goal_commands.rs
 * (`activate_goal` / `resume_goal_work`) and resolves with the activation
 * outcome (`started`/`queued`/`already_active`) instead of the goal, so the
 * summary path chains a `goal_get` for the state line. The Goal panel keeps
 * sending the mutation-only shapes (no `activate` field).
 */
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

/**
 * Map the bare `/ps` action onto its existing RPC wire shape: the backend
 * `process_list` RPC resolves with the owning session's `ProcessInfo[]`
 * (crates/pi-cli/src/modes/rpc.rs `RpcCommand::ProcessList`). App stamps the
 * active sessionId on the frame, like every other command.
 */
export function psWire(): Record<string, unknown> {
  return { type: 'process_list' };
}

/**
 * TUI parity guard: `/loop create` and `/loop update` are unavailable while
 * another turn is running (the TUI's loop dispatch rejects exactly these two
 * ops while `is_streaming`). List/delete/cancel remain available.
 */
export function loopRequiresIdle(action: LoopAction): boolean {
  return action.op === 'create' || action.op === 'update';
}

/** TUI loop aliases the Web composer does not wire (`loops`, `loop-update`,
 *  `loop-delete`, `loop-cancel` are separate backend builtin names). Typing
 *  them must NEVER fall through as a model prompt — Main intercepts them with
 *  this actionable error pointing at the canonical `/loop` surface. Returns
 *  `null` for any text that is not such an alias. */
const LOOP_ALIAS_USAGE: Record<string, string> = {
  loops: 'use /loop list',
  'loop-update': 'use /loop update <id> [interval] [prompt]',
  'loop-delete': 'use /loop delete <id>',
  'loop-cancel': 'use /loop cancel <id>',
};

export function unsupportedAliasMessage(text: string): string | null {
  const trimmed = text.trim();
  if (!trimmed.startsWith('/')) return null;
  const match = trimmed.slice(1).match(/^(\S+)/);
  if (!match) return null;
  const usage = LOOP_ALIAS_USAGE[match[1]];
  return usage ? `alias of /loop: ${usage}` : null;
}

/**
 * Format a compact/snapcompact RPC token report for a system-style bubble.
 * Defensive against missing fields so a partial backend never throws.
 */
export function formatCompactReport(data: unknown, label: string): string {
  const d = (data && typeof data === 'object' ? data : {}) as {
    tokensBefore?: unknown;
    estimatedTokensAfter?: unknown;
  };
  const before = d.tokensBefore;
  const after = d.estimatedTokensAfter;
  if (typeof before !== 'number') return `${label}: done`;
  const afterText = typeof after === 'number' ? String(after) : '?';
  const shrank = typeof after === 'number' && after < before ? ' (shrank)' : '';
  return `${label}: ${before} → ${afterText} estimated tokens${shrank}`;
}

/**
 * Format a skill RPC response (`{name, summary}`) for a visible bubble.
 * Falls back to stringifying summary alone when the wrapper is bare text.
 */
export function formatSkillResult(data: unknown, requestedName: string): string {
  if (typeof data === 'string') {
    return data.trim() ? data : `skill ${requestedName}: (empty)`;
  }
  const d = (data && typeof data === 'object' ? data : {}) as {
    name?: unknown;
    summary?: unknown;
  };
  const name = typeof d.name === 'string' && d.name ? d.name : requestedName;
  const summary = typeof d.summary === 'string' ? d.summary : '';
  if (summary) return summary;
  return `skill ${name}: (no summary)`;
}

/* ------------------------------------------------------------------ *
 * /loop + /goal + /ps visible-result formatters.
 *
 * All formatters are defensive (a partial wire entry never throws) and
 * bounded (row/pin counts and prompt/label lengths are capped so a hostile
 * or oversized backend payload cannot flood the transcript).
 * ------------------------------------------------------------------ */

/** Bounds for loop/goal formatters (backend caps: 50 loop tasks, 8 pins). */
const MAX_LOOP_ROWS = 20;
const MAX_LOOP_PROMPT_CHARS = 120;
const MAX_PIN_ROWS = 8;
const MAX_PIN_CHARS = 200;

function truncate(text: string, max: number): string {
  if (text.length <= max) return text;
  return `${text.slice(0, max)}…`;
}

/** Human schedule for a loop interval — mirrors
 *  `pi_coding::loop_interval_to_human` (`every N day(s)|hour(s)|minute(s)|second(s)`). */
export function loopIntervalToHuman(intervalSecs: unknown): string {
  if (typeof intervalSecs !== 'number' || !Number.isFinite(intervalSecs) || intervalSecs <= 0) {
    return 'every ?';
  }
  const plural = (number: number, unit: string): string =>
    number === 1 ? `every 1 ${unit}` : `every ${number} ${unit}s`;
  if (intervalSecs % 86400 === 0) return plural(intervalSecs / 86400, 'day');
  if (intervalSecs % 3600 === 0) return plural(intervalSecs / 3600, 'hour');
  if (intervalSecs % 60 === 0) return plural(intervalSecs / 60, 'minute');
  return plural(intervalSecs, 'second');
}

/** A typed view of the camelCase `LoopTask` wire entry. */
type LoopTaskWire = {
  id?: unknown;
  intervalSecs?: unknown;
  prompt?: unknown;
  lastFiredAt?: unknown;
  createdAt?: unknown;
  expiresAt?: unknown;
};

function asLoopTask(data: unknown): LoopTaskWire {
  return (data && typeof data === 'object' ? data : {}) as LoopTaskWire;
}

/** `next_fire_at = (lastFiredAt ?? createdAt) + intervalSecs`, as ISO text —
 *  mirrors `LoopTask::next_fire_at`. `''` when the wire entry lacks a
 *  parseable base timestamp (defensive; never throws). */
function loopNextFireAt(task: LoopTaskWire): string {
  const base = task.lastFiredAt ?? task.createdAt;
  if (typeof base !== 'string') return '';
  const baseMs = Date.parse(base);
  if (!Number.isFinite(baseMs)) return '';
  const intervalMs = (typeof task.intervalSecs === 'number' ? task.intervalSecs : 0) * 1000;
  return new Date(baseMs + intervalMs).toISOString();
}

/** One loop task row — mirrors the TUI list/update line shape
 *  `{id}  {schedule}  next {next_fire_at}  {prompt}`. */
export function formatLoopTaskRow(data: unknown): string {
  const task = asLoopTask(data);
  const id = typeof task.id === 'string' && task.id ? task.id : '?';
  const schedule = loopIntervalToHuman(task.intervalSecs);
  const prompt = truncate(typeof task.prompt === 'string' ? task.prompt : '', MAX_LOOP_PROMPT_CHARS);
  const next = loopNextFireAt(task);
  return next ? `${id}  ${schedule}  next ${next}  ${prompt}` : `${id}  ${schedule}  ${prompt}`;
}

/** `loop_list` result — `no active loops` or bounded task rows. */
export function formatLoopList(data: unknown): string {
  if (!Array.isArray(data) || data.length === 0) return 'no active loops';
  const rows = data.slice(0, MAX_LOOP_ROWS).map(formatLoopTaskRow);
  if (data.length > MAX_LOOP_ROWS) rows.push(`… and ${data.length - MAX_LOOP_ROWS} more`);
  return rows.join('\n');
}

/** Create/update task line — mirrors the TUI create/update output shapes. */
function formatLoopTaskVerbose(data: unknown, kind: 'created' | 'updated'): string {
  const task = asLoopTask(data);
  const id = typeof task.id === 'string' && task.id ? task.id : '?';
  const schedule = loopIntervalToHuman(task.intervalSecs);
  if (kind === 'created') {
    const expiresAt = typeof task.expiresAt === 'string' ? task.expiresAt : '';
    return expiresAt ? `scheduled ${id} · ${schedule} · expires ${expiresAt}` : `scheduled ${id} · ${schedule}`;
  }
  const prompt = truncate(typeof task.prompt === 'string' ? task.prompt : '', MAX_LOOP_PROMPT_CHARS);
  const next = loopNextFireAt(task);
  return next
    ? `updated loop ${id} · ${schedule} · next ${next} · ${prompt}`
    : `updated loop ${id} · ${schedule} · ${prompt}`;
}

/** Format a `/loop` RPC response for the shared summary bubble. Delete/cancel
 *  resolve with `false` (not an RPC error) when the task is unknown — that is
 *  surfaced as an actionable error, matching the TUI's Err message. */
export function formatLoopResult(
  action: LoopAction,
  data: unknown,
): { ok: true; text: string } | { ok: false; message: string } {
  switch (action.op) {
    case 'create':
      return { ok: true, text: formatLoopTaskVerbose(data, 'created') };
    case 'list':
      return { ok: true, text: formatLoopList(data) };
    case 'update':
      return { ok: true, text: formatLoopTaskVerbose(data, 'updated') };
    case 'delete':
      return data === true
        ? { ok: true, text: `deleted loop ${action.taskId}` }
        : { ok: false, message: `no active loop with id ${action.taskId}` };
    case 'cancel':
      return data === true
        ? { ok: true, text: `cancelled loop ${action.taskId}` }
        : { ok: false, message: `no active loop with id ${action.taskId}` };
  }
}

/** Human-readable goal pause reason — mirrors
 *  `format_pause_reason` in goal_commands.rs. */
export function goalPauseReasonLabel(reason: unknown): string {
  switch (reason) {
    case 'manual':
      return 'manually paused';
    case 'budget_exhausted':
      return 'budget exhausted; cannot resume';
    case 'resume_safety':
      return 'session resumed; run /goal resume';
    default:
      return '';
  }
}

/** Lifecycle label for a goal — mirrors `lifecycle_label` in goal_commands.rs
 *  (paused goals carry their human-readable pause reason). */
export function goalLifecycleLabel(goal: { lifecycle?: unknown; pauseReason?: unknown }): string {
  const lifecycle = typeof goal.lifecycle === 'string' ? goal.lifecycle : 'active';
  if (lifecycle === 'paused') {
    const reason = goalPauseReasonLabel(goal.pauseReason);
    return reason ? `paused (${reason})` : 'paused';
  }
  return lifecycle;
}

/** One goal state line from the camelCase `Goal` wire — mirrors
 *  `format_goal_state` in goal_commands.rs:
 *  `{lifecycle} · {tokensUsed}/{budget or "tokens used"} · {objective}`. */
export function formatGoalFromWire(data: unknown): string {
  const goal = (data && typeof data === 'object' ? data : {}) as {
    objective?: unknown;
    tokenBudget?: unknown;
    usage?: unknown;
    lifecycle?: unknown;
    pauseReason?: unknown;
  };
  const usage = (goal.usage && typeof goal.usage === 'object' ? goal.usage : {}) as {
    tokensUsed?: unknown;
  };
  const tokensUsed = typeof usage.tokensUsed === 'number' ? usage.tokensUsed : 0;
  const budget =
    typeof goal.tokenBudget === 'number'
      ? `${tokensUsed}/${goal.tokenBudget} tokens`
      : `${tokensUsed} tokens used`;
  const objective = typeof goal.objective === 'string' ? goal.objective : '';
  return `${goalLifecycleLabel(goal)} · ${budget} · ${objective}`;
}

/** `goal_get` (GoalState wire) → the current goal line, or `no goal`. */
export function formatGoalState(data: unknown): string {
  const state = (data && typeof data === 'object' ? data : {}) as { current?: unknown };
  const current = state.current;
  if (!current || typeof current !== 'object') return 'no goal';
  return formatGoalFromWire(current);
}

/** `goal_get` (GoalState wire) → numbered pins listing, mirroring
 *  `format_goal_pins` in goal_commands.rs (`no goal` / `no pins` markers). */
export function formatGoalPins(data: unknown): string {
  const state = (data && typeof data === 'object' ? data : {}) as { current?: unknown };
  const current = state.current;
  if (!current || typeof current !== 'object') return 'no goal';
  const pins = (current as { pins?: unknown }).pins;
  if (!Array.isArray(pins) || pins.length === 0) return 'no pins';
  const rows = pins
    .slice(0, MAX_PIN_ROWS)
    .map((pin, index) => `${index + 1}. ${truncate(typeof pin === 'string' ? pin : '', MAX_PIN_CHARS)}`);
  if (pins.length > MAX_PIN_ROWS) rows.push(`… and ${pins.length - MAX_PIN_ROWS} more`);
  return rows.join('\n');
}

/** TUI activation prefix for a `goal_create`/`goal_resume` `activate: true`
 *  outcome — mirrors the match in goal_commands.rs
 *  `execute_interactive_goal_command` (`Goal work started|queued|already
 *  active · {state}`). Unknown/non-string data formats no prefix (the
 *  mutation-only response shape resolves with the Goal directly). */
export function goalActivationPrefix(data: unknown): string {
  switch (data) {
    case 'started':
      return 'Goal work started';
    case 'queued':
      return 'Goal work queued';
    case 'already_active':
      return 'Goal work already active';
    default:
      return '';
  }
}

/** Format a `/goal` RPC response for the shared summary bubble. Every goal
 *  mutation RPC resolves with the mutated `Goal` wire, so the state line
 *  formats directly from the response; show/pins format the `goal_get`
 *  GoalState wire. Create/resume with `activate: true` resolve with the
 *  activation outcome — the summary path passes the chained `goal_get`
 *  GoalState as `stateData` so the TUI-parity
 *  `Goal work started|queued|already active · {state}` line can render. */
export function formatGoalResult(
  action: GoalAction,
  data: unknown,
  stateData?: unknown,
): { ok: true; text: string } | { ok: false; message: string } {
  switch (action.op) {
    case 'show':
      return { ok: true, text: formatGoalState(data) };
    case 'pins':
      return { ok: true, text: formatGoalPins(data) };
    case 'create':
    case 'resume': {
      const prefix = goalActivationPrefix(data);
      if (prefix) {
        // activate:true — `data` is the activation outcome, `stateData` is
        // the chained goal_get GoalState (TUI parity line).
        return { ok: true, text: `${prefix} · ${formatGoalState(stateData)}` };
      }
      // mutation-only shape — `data` is the Goal wire.
      return { ok: true, text: formatGoalFromWire(data) };
    }
    case 'pause':
    case 'complete':
    case 'drop':
    case 'pin':
    case 'unpin':
      return { ok: true, text: formatGoalFromWire(data) };
  }
}

/* ------------------------------------------------------------------ *
 * /ps visible-result formatter.
 *
 * Renders the `process_list` RPC result (`ProcessInfo[]`, camelCase wire —
 * crates/pi-coding/src/process/mod.rs) with TUI parity: `format_process_list`
 * in process_commands.rs joins `format_process_info` rows
 * (`{id}\t{state:?}\t{label}\tcursor {start}..{end}`) and prints
 * `No supervised processes` for an empty list. Rows are bounded to the
 * backend process cap (16) and labels are sanitized (control characters
 * stripped so a hostile spawn label cannot inject rows or columns) and
 * truncated, so an oversized or hostile payload stays a bounded, readable,
 * Markdown-safe listing.
 * ------------------------------------------------------------------ */

/** Bounds for the process formatter (backend caps: 16 supervised processes,
 *  label ≤ 64 bytes). */
const MAX_PROCESS_ROWS = 16;
const MAX_PROCESS_LABEL_CHARS = 64;

/** TUI state label — mirrors Rust `{:?}` Debug of `ProcessState` (the wire
 *  carries snake_case; Debug prints the variant name). Unknown states fall
 *  back to the raw wire string so a future backend variant still renders. */
const PROCESS_STATE_LABELS: Record<string, string> = {
  starting: 'Starting',
  running: 'Running',
  stopping: 'Stopping',
  exited: 'Exited',
  timed_out: 'TimedOut',
  expired: 'Expired',
  failed: 'Failed',
};

/** Sanitize a process label for Markdown: strip control characters (line
 *  breaks, tabs, \0 — a hostile spawn label must not inject rows or columns
 *  into the listing) and truncate to the backend label cap. Empty/non-string
 *  labels render as `(unlabeled)` exactly like the TUI. */
function formatProcessLabel(raw: unknown): string {
  const label = typeof raw === 'string' ? raw.replace(/[\u0000-\u001F\u007F\u2028\u2029]/g, '') : '';
  if (!label) return '(unlabeled)';
  return truncate(label, MAX_PROCESS_LABEL_CHARS);
}

/** A typed view of the camelCase `ProcessInfo` wire entry. */
type ProcessInfoWire = {
  id?: unknown;
  state?: unknown;
  label?: unknown;
  outputStartCursor?: unknown;
  outputCursor?: unknown;
};

function asProcessInfo(data: unknown): ProcessInfoWire {
  return (data && typeof data === 'object' ? data : {}) as ProcessInfoWire;
}

/** One process row — mirrors `format_process_info` in process_commands.rs:
 *  `{id}\t{state}\t{label}\tcursor {start}..{end}`. */
export function formatProcessRow(data: unknown): string {
  const process = asProcessInfo(data);
  const id = typeof process.id === 'string' && process.id ? process.id : '?';
  const rawState = typeof process.state === 'string' ? process.state : '';
  const state = PROCESS_STATE_LABELS[rawState] ?? (rawState || '?');
  const label = formatProcessLabel(process.label);
  const start = typeof process.outputStartCursor === 'number' ? process.outputStartCursor : 0;
  const cursor = typeof process.outputCursor === 'number' ? process.outputCursor : 0;
  return `${id}\t${state}\t${label}\tcursor ${start}..${cursor}`;
}

/** `process_list` result — `No supervised processes` or bounded process
 *  rows, mirroring `format_process_list` in process_commands.rs. */
export function formatProcessList(data: unknown): string {
  if (!Array.isArray(data) || data.length === 0) return 'No supervised processes';
  const rows = data.slice(0, MAX_PROCESS_ROWS).map(formatProcessRow);
  if (data.length > MAX_PROCESS_ROWS) rows.push(`… and ${data.length - MAX_PROCESS_ROWS} more`);
  return rows.join('\n');
}
