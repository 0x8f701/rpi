import type { EventFrame } from './types';

export type ActivityStripStateName = 'error' | 'tool' | 'child' | 'thinking' | 'working';

export interface ActivityTool {
  id: string;
  name: string;
}

export interface ActivityChild {
  jobId: string;
  agentId: string;
  name: string;
}

export interface ActivityStripState {
  sessionId: string;
  turnId: string;
  isStreaming: boolean;
  terminalTurnIds: string[];
  error: string;
  tools: ActivityTool[];
  children: ActivityChild[];
  isThinking: boolean;
}

export interface ActivityStripView {
  state: 'reconnecting' | ActivityStripStateName;
  text: string;
}

const TERMINAL_TURN_LIMIT = 16;
const LABEL_LIMIT = 120;
const ACTIVE_TOOL_LIMIT = 64;
const ACTIVE_CHILD_LIMIT = 64;

function boundedLabel(value: unknown): string {
  if (typeof value !== 'string') return '';
  const compact = value.replace(/\s+/g, ' ').trim();
  if (compact.length <= LABEL_LIMIT) return compact;
  return `${compact.slice(0, LABEL_LIMIT - 1)}…`;
}

function record(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
    ? value as Record<string, unknown>
    : null;
}

function stringField(value: Record<string, unknown> | null, ...keys: string[]): string {
  if (!value) return '';
  for (const key of keys) {
    if (typeof value[key] === 'string') return value[key] as string;
  }
  return '';
}

function terminalTurnIds(state: ActivityStripState, turnId: string): string[] {
  if (turnId === '' || state.terminalTurnIds.includes(turnId)) return state.terminalTurnIds;
  return [...state.terminalTurnIds, turnId].slice(-TERMINAL_TURN_LIMIT);
}

export function applyTaskSpawnDetails(
  state: ActivityStripState | undefined,
  sessionId: string,
  turnId: string,
  details: unknown,
): ActivityStripState {
  if (!Array.isArray(details)) return state ?? emptyActivityStripState(sessionId);
  return details.reduce((nextState, spawn) => {
    const child = record(spawn);
    const jobId = stringField(child, 'jobId', 'job_id');
    const agentId = stringField(child, 'agentId', 'agent_id');
    const status = stringField(child, 'status');
    if (jobId === '' || agentId === '' || (status !== 'queued' && status !== 'running')) {
      return nextState;
    }
    return applyActivityStripEvent(
      nextState,
      {
        type: 'job_updated',
        sessionId,
        turnId,
        job: {
          id: jobId,
          agentId,
          agent: stringField(child, 'agent'),
          parentId: 'Main',
          status,
        },
      },
      sessionId,
    );
  }, state ?? emptyActivityStripState(sessionId));
}

export function emptyActivityStripState(sessionId: string): ActivityStripState {
  return {
    sessionId,
    turnId: '',
    isStreaming: false,
    terminalTurnIds: [],
    error: '',
    tools: [],
    children: [],
    isThinking: false,
  };
}

export function reconcileActivityStrip(
  state: ActivityStripState | undefined,
  sessionId: string,
  isStreaming: boolean,
  activeTurnId = '',
): ActivityStripState {
  const current = state?.sessionId === sessionId ? state : emptyActivityStripState(sessionId);
  if (!isStreaming) {
    return { ...emptyActivityStripState(sessionId), terminalTurnIds: current.terminalTurnIds };
  }
  const turnId = activeTurnId || current.turnId;
  if (turnId === '' || current.terminalTurnIds.includes(turnId)) return current;
  if (current.turnId !== turnId) {
    return { ...emptyActivityStripState(sessionId), turnId, isStreaming: true };
  }
  return { ...current, isStreaming: true, error: '' };
}

export function applyActivityStripEvent(
  state: ActivityStripState | undefined,
  frame: EventFrame,
  sessionId: string,
): ActivityStripState {
  const current = state?.sessionId === sessionId ? state : emptyActivityStripState(sessionId);
  const turnId = typeof frame.turnId === 'string' ? frame.turnId : '';
  if (turnId !== '' && current.terminalTurnIds.includes(turnId)) return current;
  if (frame.type === 'turn_started') {
    if (turnId === '') return current;
    return { ...emptyActivityStripState(sessionId), turnId, isStreaming: true, terminalTurnIds: current.terminalTurnIds };
  }
  if (turnId === '' || current.turnId === '' || current.turnId !== turnId) return current;
  if (!current.isStreaming && frame.type !== 'turn_terminal') return current;


  switch (frame.type) {
    case 'message_start': {
      const message = record(frame.message);
      return stringField(message, 'role') === 'assistant'
        ? { ...current, isStreaming: true, error: '' }
        : current;
    }
    case 'message_update': {
      const event = record(frame.assistantMessageEvent);
      const type = stringField(event, 'type');
      if (type === 'thinking_delta') return { ...current, isStreaming: true, isThinking: true, error: '' };
      if (type === 'text_delta') return { ...current, isStreaming: true, isThinking: false, error: '' };
      return current;
    }
    case 'tool_execution_start': {
      const toolCallId = typeof frame.toolCallId === 'string' ? frame.toolCallId : '';
      if (toolCallId === '') return current;
      const existing = current.tools.find((tool) => tool.id === toolCallId);
      const tools = existing
        ? current.tools.map((tool) => tool.id === toolCallId ? { id: toolCallId, name: boundedLabel(frame.toolName) || tool.name } : tool)
        : current.tools.length < ACTIVE_TOOL_LIMIT
          ? [...current.tools, { id: toolCallId, name: boundedLabel(frame.toolName) || 'tool' }]
          : current.tools;
      return { ...current, isStreaming: true, error: '', tools };
    }
    case 'tool_execution_end': {
      const toolCallId = typeof frame.toolCallId === 'string' ? frame.toolCallId : '';
      if (toolCallId === '' || !current.tools.some((tool) => tool.id === toolCallId)) return current;
      return { ...current, tools: current.tools.filter((tool) => tool.id !== toolCallId) };
    }
    case 'job_updated': {
      const job = record(frame.job);
      const jobId = stringField(job, 'id', 'jobId');
      const status = stringField(job, 'status');
      if (jobId === '') return current;
      if (status === 'queued' || status === 'running') {
        const existing = current.children.find((child) => child.jobId === jobId);
        const jobParentId = stringField(job, 'parentId', 'parent_id');
        if (!existing && jobParentId !== 'Main') return current;
        const child = {
          jobId,
          agentId: stringField(job, 'agentId', 'agent_id') || existing?.agentId || '',
          name: boundedLabel(stringField(job, 'displayName', 'agent', 'agentId', 'agent_id')) || existing?.name || 'agent',
        };
        const children = existing
          ? current.children.map((candidate) => candidate.jobId === jobId ? child : candidate)
          : current.children.length < ACTIVE_CHILD_LIMIT
            ? [...current.children, child]
            : current.children;
        return { ...current, isStreaming: true, error: '', children };
      }
      if (!current.children.some((child) => child.jobId === jobId)) return current;
      return { ...current, children: current.children.filter((child) => child.jobId !== jobId) };
    }
    case 'agent_updated': {
      const agent = record(frame.agent);
      const agentId = stringField(agent, 'id');
      if (agentId === '') return current;
      const name = boundedLabel(stringField(agent, 'displayName', 'display_name'));
      if (name === '' || !current.children.some((child) => child.agentId === agentId)) return current;
      return {
        ...current,
        children: current.children.map((child) => child.agentId === agentId ? { ...child, name } : child),
      };
    }
    case 'turn_terminal': {
      if (turnId === '' || (current.turnId !== '' && turnId !== current.turnId)) return current;
      const terminals = terminalTurnIds(current, turnId);
      return { ...emptyActivityStripState(sessionId), terminalTurnIds: terminals };
    }
    default:
      return current;
  }
}

export function activityStripView(
  state: ActivityStripState | undefined,
  connectionState: string,
  streamStall = false,
): ActivityStripView | null {
  const hasActiveTurn = state !== undefined && state.turnId !== '' && state.isStreaming;
  if (streamStall) {
    return { state: 'reconnecting', text: 'Reconnecting…' };
  }
  if ((connectionState === 'reconnecting' || connectionState === 'connecting') && hasActiveTurn) {
    return { state: 'reconnecting', text: 'Reconnecting…' };
  }
  if (!state) return null;
  if (state.error !== '') return { state: 'error', text: `Error: ${state.error}` };
  const tool = state.tools.at(-1);
  if (tool) return { state: 'tool', text: `Running ${tool.name || 'tool'}…` };
  const child = state.children.at(-1);
  if (child) return { state: 'child', text: `Waiting for ${child.name || 'agent'}…` };
  if (state.isThinking) return { state: 'thinking', text: 'Thinking…' };
  if (state.isStreaming) return { state: 'working', text: 'Working…' };
  return null;
}
