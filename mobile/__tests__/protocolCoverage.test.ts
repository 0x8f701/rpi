// Protocol coverage for the RN panel surface.
//
// Three contracts are pinned here:
//   1. Every row the /web panels speak is sendable from RN with the exact
//      wire field names the Rust `RpcCommand` enum deserializes, and a frame
//      that drops a required field or carries the wrong kind is refused
//      before it reaches the runner.
//   2. Every reader a panel consumes fails closed: a malformed payload yields
//      null (or the one documented empty page) and never a fabricated value,
//      and a well-formed payload round-trips every field the panel reads.
//   3. The transport helpers the previous RN slice landed — namespaced
//      command ids, per-command timeout classes, and the response-ownership
//      gate — still behave exactly as before the union was extended.
import {
  buildFileConversationPromptPayload,
  classifyResponseOwnership,
  CODE_REVIEW_SCOPE_USAGE,
  commandTimeoutMs,
  createClientNamespace,
  createConversationId,
  DEFAULT_COMMAND_TIMEOUT_MS,
  FILE_CONVERSATION_CAPACITY_LIMIT,
  FILE_CONVERSATION_PROMPT_MAX_BYTES,
  isValidConversationId,
  LONG_OP_TIMEOUT_MS,
  nextCommandId,
  parseAgentHistory,
  parseCodeReviewCloseResult,
  parseCodeReviewCommits,
  parseCodeReviewFileContextPage,
  parseCodeReviewFileDiffPage,
  parseCodeReviewScope,
  parseCodeReviewSetModelAck,
  parseFileConversationClose,
  parseFileConversationErrorCode,
  parseFileConversationSnapshot,
  parseForkMessagesPage,
  parseHubReceiptList,
  parseHubSendResult,
  parseJobOutput,
  parseLifecycleTarget,
  parsePathCompletion,
  parsePersonaDetail,
  parsePersonaList,
  parsePersonaMessage,
  parseProcessList,
  parseProcessLogs,
  parseQueueCancelResult,
  parseRunnerCommand,
  parseSessionCopyContext,
  parseSessionHandoff,
  parseSessionRow,
  parseSettingsApplyOutcome,
  parseSettingsCatalog,
  parseSettingsDraft,
  parseSideChatSnapshot,
  parseStagedDownload,
  parseSteeringSendNow,
  parseTaskSpawnSpawns,
  parseTodoChildCreateId,
  parseTodoChildDetail,
  parseTodoChildManifest,
  parseTodoChildStatus,
  parseTmuxSendResult,
  parseWorkflowDetail,
  parseWorkflowHubSendResult,
  parseWorkflowList,
  parseWorkflowSnapshotRow,
  PATH_COMPLETE_LIMIT,
  RUNNER_COMMAND_WIRE,
  SESSION_COPY_CONTEXT_FORMAT,
  type FileConversationRangeWire,
  type RunnerCommand,
} from '../src/protocol';

/** Rows the RN panels must be able to send. */
const LISTED_ROWS = [
  'queue_send_now',
  'queue_cancel',
  'code_review_open',
  'code_review_comment',
  'code_review_refresh',
  'code_review_abort',
  'code_review_close',
  'code_review_file_diff',
  'code_review_file_context',
  'code_review_set_model',
  'code_review_commits',
  'settings_inspect',
  'settings_search',
  'settings_open_draft',
  'settings_get_draft',
  'settings_set',
  'settings_reset',
  'settings_validate',
  'settings_apply',
  'settings_cancel',
  'persona_list',
  'persona_get',
  'persona_create',
  'persona_edit',
  'persona_remove',
  'persona_purge',
  'persona_select',
  'persona_clear',
  'persona_current',
  'process_spawn',
  'process_list',
  'process_describe',
  'process_logs',
  'process_write',
  'process_keys',
  'process_resize',
  'process_signal',
  'process_stop',
  'process_wait',
  'workflow_create',
  'workflow_list',
  'workflow_get',
  'workflow_detail',
  'workflow_pause',
  'workflow_resume',
  'workflow_cancel',
  'workflow_integrate',
  'workflow_remove',
  'workflow_hub_send',
  'side_chat_new',
  'side_chat_switch',
  'side_chat_close',
  'side_chat_prompt',
  'side_chat_list',
  'side_chat_abort',
  'file_conversation_open',
  'file_conversation_prompt',
  'file_conversation_snapshot',
  'file_conversation_abort',
  'file_conversation_close',
  'todo_child_create',
  'todo_child_list',
  'todo_child_select',
  'todo_child_send',
  'todo_child_status',
  'todo_child_stop',
  'todo_child_pause',
  'todo_child_resume',
  'todo_child_cancel',
  'todo_child_archive',
  'todo_child_remove',
  'todo_child_add_task',
  'todo_child_detail',
  'todo_child_hub_send',
  'session_handoff',
  'clone',
  'fork',
  'get_fork_messages',
  'session_restore',
  'session_copy_context',
  'export_html',
  'path_complete',
  'file_explorer_stage_download',
  'tmux_send_keys',
  'job_output',
  'agent_history',
  'task_spawn',
  'hub_send',
] as const;

/** Payloads no reader may accept: none of them is a wire object or list. */
const NON_PAYLOADS = [null, undefined, 0, 'text', true];

/** The same values plus an array, which is never a wire OBJECT. */
const NOT_AN_OBJECT = [...NON_PAYLOADS, []];

function wireRow(type: string): Record<string, string> {
  return RUNNER_COMMAND_WIRE[type as RunnerCommand['type']] as Record<string, string>;
}

/** A legal value for one field spec. */
function sampleValue(spec: string): unknown {
  const kind = spec.endsWith('?') ? spec.slice(0, -1) : spec;
  if (kind.startsWith('enum:')) return kind.slice('enum:'.length).split('|')[0];
  switch (kind) {
    case 'number':
      return 7;
    case 'boolean':
      return true;
    case 'string[]':
      return ['a'];
    case 'array':
      return [];
    case 'object':
      return { argv: ['true'] };
    default:
      return 'value';
  }
}

/** A value of the WRONG kind for one field spec; `null` for `any`, which
 *  accepts every JSON value. */
function wrongValue(spec: string): unknown {
  const kind = spec.endsWith('?') ? spec.slice(0, -1) : spec;
  if (kind.startsWith('enum:')) return 'not-a-listed-literal';
  switch (kind) {
    case 'string':
      return 42;
    case 'number':
      return '7';
    case 'boolean':
      return 'true';
    case 'string[]':
      return 'not-a-list';
    case 'array':
      return 'not-an-array';
    case 'object':
      return 'not-an-object';
    default:
      return null;
  }
}

function buildFrame(type: string): Record<string, unknown> {
  const frame: Record<string, unknown> = { type };
  for (const [field, spec] of Object.entries(wireRow(type))) {
    frame[field] = sampleValue(spec);
  }
  return frame;
}

describe('command wire rows', () => {
  it('covers every row the panel surface names', () => {
    for (const type of LISTED_ROWS) {
      expect(wireRow(type)).toBeDefined();
    }
  });

  it('accepts each listed row with exactly its declared wire fields', () => {
    for (const type of LISTED_ROWS) {
      const frame = buildFrame(type);
      expect(parseRunnerCommand(frame)).toEqual(frame);
      expect(Object.keys(frame)).toEqual(['type', ...Object.keys(wireRow(type))]);
    }
  });

  it('keeps validating the rows the chat shell already sent', () => {
    for (const type of ['get_state', 'prompt', 'compact', 'switch_session', 'tmux_terminal_input']) {
      const frame = buildFrame(type);
      expect(parseRunnerCommand(frame)).toEqual(frame);
    }
  });

  it('rejects a listed row that drops any required field', () => {
    let checked = 0;
    for (const type of LISTED_ROWS) {
      for (const [field, spec] of Object.entries(wireRow(type))) {
        if (spec.endsWith('?')) continue;
        const frame = buildFrame(type);
        delete frame[field];
        expect(parseRunnerCommand(frame)).toBeNull();
        checked += 1;
      }
    }
    // The rejection sweep is real coverage, not an empty loop.
    expect(checked).toBeGreaterThan(60);
  });

  it('rejects a listed row whose field carries the wrong kind', () => {
    let checked = 0;
    for (const type of LISTED_ROWS) {
      for (const [field, spec] of Object.entries(wireRow(type))) {
        if (spec === 'any') continue;
        const frame = buildFrame(type);
        frame[field] = wrongValue(spec);
        expect(parseRunnerCommand(frame)).toBeNull();
        checked += 1;
      }
    }
    expect(checked).toBeGreaterThan(60);
  });

  it('rejects optional fields that are present with the wrong kind', () => {
    expect(parseRunnerCommand({ type: 'fork', entryId: 'e1', sessionPath: 7 })).toBeNull();
    expect(parseRunnerCommand({ type: 'queue_cancel', scope: 'everything' })).toBeNull();
    expect(parseRunnerCommand({ type: 'path_complete', path: 'src', kind: 'both' })).toBeNull();
    expect(parseRunnerCommand({ type: 'agent_history', agentId: 'a1', lines: -1 })).toBeNull();
    expect(parseRunnerCommand({ type: 'persona_remove', name: 'mentor', confirm: 'yes' })).toBeNull();
  });

  it('refuses frames the union does not declare', () => {
    for (const input of NOT_AN_OBJECT) {
      expect(parseRunnerCommand(input)).toBeNull();
    }
    expect(parseRunnerCommand({})).toBeNull();
    expect(parseRunnerCommand({ type: 'not_a_command' })).toBeNull();
    expect(parseRunnerCommand({ type: 'constructor' })).toBeNull();
    expect(parseRunnerCommand({ type: 'toString' })).toBeNull();
    expect(parseRunnerCommand({ message: 'hi' })).toBeNull();
  });

  it('permits the id and sessionId the transport stamps onto a frame', () => {
    const frame = { type: 'get_state', id: 'cnamespace-1', sessionId: 'session-1' };
    expect(parseRunnerCommand(frame)).toEqual(frame);
  });

  it('accepts a frame built from the union type', () => {
    const frame: RunnerCommand = {
      type: 'code_review_comment',
      snapshotId: 'snap-1',
      path: 'src/a.ts',
      oldStart: 1,
      oldCount: 2,
      newStart: 1,
      newCount: 3,
      contentHash: 'hash',
      comment: 'why?',
    };
    expect(parseRunnerCommand(frame)).toEqual(frame);
    expect(parseRunnerCommand({ ...frame, comment: 5 })).toBeNull();
  });
});

describe('queue tray readers', () => {
  it('reads a steering send-now result and refuses anything else', () => {
    expect(parseSteeringSendNow({ sent: 2, combined: true })).toEqual({ sent: 2, combined: true });
    expect(parseSteeringSendNow({ sent: 0, combined: false })).toEqual({ sent: 0, combined: false });
    expect(parseSteeringSendNow({ sent: '2', combined: true })).toBeNull();
    expect(parseSteeringSendNow({ sent: 2 })).toBeNull();
    for (const input of NOT_AN_OBJECT) expect(parseSteeringSendNow(input)).toBeNull();
  });

  it('reads a queue cancel count and refuses anything else', () => {
    expect(parseQueueCancelResult({ cancelled: 3 })).toEqual({ cancelled: 3 });
    expect(parseQueueCancelResult({ cancelled: -1 })).toBeNull();
    expect(parseQueueCancelResult({})).toBeNull();
    for (const input of NOT_AN_OBJECT) expect(parseQueueCancelResult(input)).toBeNull();
  });
});

describe('session lifecycle and transfer readers', () => {
  const TARGET = {
    sessionId: 'session-1',
    runtimeIncarnation: 'incarnation-1',
    state: { sessionId: 'session-1', sessionFile: '/sessions/session-1.jsonl', cwd: '/workspace' },
    session: {
      sessionId: 'session-1',
      runtimeIncarnation: 'incarnation-1',
      path: '/sessions/session-1.jsonl',
      cwd: '/workspace',
      status: 'native',
      loaded: true,
      source: 'rpi',
    },
  };

  it('reads catalog-shaped session rows without changing their fields', () => {
    const row = { sessionId: 'session-1', path: '/sessions/session-1.jsonl', cwd: '/workspace', name: 'Mapped' };
    expect(parseSessionRow(row)).toEqual(row);
  });

  it('reads a new-session snapshot using the session row cwd', () => {
    expect(parseSessionRow({ ...TARGET, state: { ...TARGET.state, cwd: '/other' } })).toEqual({
      sessionId: 'session-1', path: '/sessions/session-1.jsonl', cwd: '/workspace', name: null,
    });
  });

  it('reads snapshot path and cwd from state when absent from the session row', () => {
    expect(parseSessionRow({ ...TARGET, session: { ...TARGET.session, path: undefined, cwd: undefined } })).toEqual({
      sessionId: 'session-1', path: '/sessions/session-1.jsonl', cwd: '/workspace', name: null,
    });
  });

  it('rejects snapshot rows with conflicting identities or paths', () => {
    expect(parseSessionRow({ ...TARGET, session: { ...TARGET.session, sessionId: 'session-2' } })).toBeNull();
    expect(parseSessionRow({ ...TARGET, state: { ...TARGET.state, sessionId: 'session-2' } })).toBeNull();
    expect(parseSessionRow({ ...TARGET, session: { ...TARGET.session, path: '/other.jsonl' } })).toBeNull();
  });

  it('rejects session rows without a session identity, path, or cwd', () => {
    expect(parseSessionRow({ ...TARGET, sessionId: undefined })).toBeNull();
    expect(parseSessionRow({ ...TARGET, session: { ...TARGET.session, path: undefined }, state: { ...TARGET.state, sessionFile: undefined } })).toBeNull();
    expect(parseSessionRow({ ...TARGET, session: { ...TARGET.session, cwd: undefined }, state: { ...TARGET.state, cwd: undefined } })).toBeNull();
    for (const input of NOT_AN_OBJECT) expect(parseSessionRow(input)).toBeNull();
  });

  it('accepts a lifecycle target only when identity, state, and row agree', () => {
    expect(parseLifecycleTarget(TARGET)).toEqual({
      sessionId: 'session-1',
      runtimeIncarnation: 'incarnation-1',
      path: '/sessions/session-1.jsonl',
      cwd: '/workspace',
    });
    expect(parseLifecycleTarget({ ...TARGET, session: { ...TARGET.session, source: 'foreign' } })).toBeNull();
    expect(parseLifecycleTarget({ ...TARGET, session: { ...TARGET.session, loaded: false } })).toBeNull();
    expect(parseLifecycleTarget({ ...TARGET, session: { ...TARGET.session, status: 'foreign' } })).toBeNull();
    expect(parseLifecycleTarget({ ...TARGET, session: { ...TARGET.session, path: '/other.jsonl' } })).toBeNull();
    expect(parseLifecycleTarget({ ...TARGET, state: { ...TARGET.state, cwd: '' } })).toBeNull();
    expect(parseLifecycleTarget({ ...TARGET, runtimeIncarnation: '' })).toBeNull();
    for (const input of NOT_AN_OBJECT) expect(parseLifecycleTarget(input)).toBeNull();
  });

  it('accepts a session handoff only as a self-describing native envelope', () => {
    const envelope = {
      sessionId: 'session-2',
      catalogSessionId: 'session-2',
      runtimeIncarnation: 'incarnation-2',
      catalogSource: 'rpi',
      catalogStatus: 'native',
      subscriptionRequired: true,
      lifecycleResponseVersion: 1,
      cursor: 12,
      notice: 'handoff created',
    };
    expect(parseSessionHandoff(envelope)).toEqual({
      sessionId: 'session-2',
      runtimeIncarnation: 'incarnation-2',
      catalogSessionId: 'session-2',
      cursor: 12,
      notice: 'handoff created',
    });
    expect(parseSessionHandoff({ ...envelope, notice: null })).toEqual({
      sessionId: 'session-2',
      runtimeIncarnation: 'incarnation-2',
      catalogSessionId: 'session-2',
      cursor: 12,
      notice: null,
    });
    expect(parseSessionHandoff({ ...envelope, catalogSource: 'claude' })).toBeNull();
    expect(parseSessionHandoff({ ...envelope, catalogStatus: 'foreign' })).toBeNull();
    expect(parseSessionHandoff({ ...envelope, catalogSessionId: 'session-9' })).toBeNull();
    expect(parseSessionHandoff({ ...envelope, subscriptionRequired: false })).toBeNull();
    expect(parseSessionHandoff({ ...envelope, lifecycleResponseVersion: 2 })).toBeNull();
    expect(parseSessionHandoff({ ...envelope, notice: 5 })).toBeNull();
    expect(parseSessionHandoff({ ...envelope, messages: [] })).toBeNull();
    expect(parseSessionHandoff({ ...envelope, state: {} })).toBeNull();
    for (const input of NOT_AN_OBJECT) expect(parseSessionHandoff(input)).toBeNull();
  });

  it('reads a bounded fork page, dropping duplicates and rejecting bad rows', () => {
    expect(parseForkMessagesPage({
      messages: [{ entryId: 'e1', text: 'one' }, { entryId: 'e1', text: 'duplicate' }, { entryId: 'e2', text: 'two' }],
      nextCursor: 'cursor-2',
      total: 9,
      truncated: false,
    })).toEqual({
      messages: [{ entryId: 'e1', text: 'one' }, { entryId: 'e2', text: 'two' }],
      nextCursor: 'cursor-2',
      truncated: true,
      total: 9,
    });
    expect(parseForkMessagesPage({ messages: [{ entryId: 'e1', text: 'one' }], truncated: false })).toEqual({
      messages: [{ entryId: 'e1', text: 'one' }],
      nextCursor: null,
      truncated: false,
      total: null,
    });
    for (const input of NON_PAYLOADS) {
      expect(parseForkMessagesPage(input)).toBeNull();
    }
    // A page must carry its `messages` list (the backend always serializes it):
    // anything else fails closed instead of answering an empty page, which
    // would overwrite the loaded list's cursor and hide older messages.
    expect(parseForkMessagesPage([])).toBeNull();
    expect(parseForkMessagesPage({})).toBeNull();
    expect(parseForkMessagesPage({ messages: 'none' })).toBeNull();
    expect(parseForkMessagesPage({ nextCursor: 'cursor-2' })).toBeNull();
    // A page carrying ANY malformed candidate fails closed too: a silently
    // truncated list would hide candidates and could fork the wrong message.
    expect(parseForkMessagesPage({ messages: [{ entryId: 'e1', text: 'one' }, null] })).toBeNull();
    expect(parseForkMessagesPage({ messages: [{ entryId: 'e1', text: 'one' }, { entryId: 'e2', text: 5 }] })).toBeNull();
    expect(parseForkMessagesPage({ messages: [{ entryId: 'e1', text: 'one' }, { entryId: '', text: 'x' }] })).toBeNull();
    // An EMPTY messages list is still a legal page: it ends the paging.
    expect(parseForkMessagesPage({ messages: [], truncated: false })).toEqual({
      messages: [],
      nextCursor: null,
      truncated: false,
      total: null,
    });
  });

  it('accepts a copy-context artifact only in its canonical header form', () => {
    const text = [
      '# Pi session context',
      `Format: ${SESSION_COPY_CONTEXT_FORMAT}`,
      'Truncated: no',
      '',
      '## Transcript',
      'user: hi',
    ].join('\n');
    expect(parseSessionCopyContext({ text, format: SESSION_COPY_CONTEXT_FORMAT, truncated: false }))
      .toEqual({ text, truncated: false });
    expect(parseSessionCopyContext({ text, format: 'other/9', truncated: false })).toBeNull();
    expect(parseSessionCopyContext({ text, format: SESSION_COPY_CONTEXT_FORMAT, truncated: 'no' })).toBeNull();
    expect(parseSessionCopyContext({
      text: text.replace('user: hi', 'Format: pi-session-context/1'),
      format: SESSION_COPY_CONTEXT_FORMAT,
      truncated: false,
    })).toBeNull();
    expect(parseSessionCopyContext({
      text: '# Pi session context\nFormat: pi-session-context/1\nTruncated: yes\n',
      format: SESSION_COPY_CONTEXT_FORMAT,
      truncated: false,
    })).toBeNull();
    expect(parseSessionCopyContext({ format: SESSION_COPY_CONTEXT_FORMAT, truncated: false })).toBeNull();
    for (const input of NOT_AN_OBJECT) expect(parseSessionCopyContext(input)).toBeNull();
  });

  it('reads a staged download from export and stage-download alike', () => {
    const staged = { downloadId: '11111111-1111-1111-1111-111111111111', fileName: 'session-1.html' };
    expect(parseStagedDownload(staged)).toEqual(staged);
    expect(parseStagedDownload({ downloadId: '' , fileName: 'x' })).toBeNull();
    expect(parseStagedDownload({ downloadId: 'x' })).toBeNull();
    expect(parseStagedDownload({ fileName: 'x' })).toBeNull();
    for (const input of NOT_AN_OBJECT) expect(parseStagedDownload(input)).toBeNull();
  });

  it('reads a path completion, capping and dropping unknown kinds', () => {
    expect(parsePathCompletion({
      base: '/workspace',
      prefix: 'src',
      entries: [
        { name: 'src', path: '/workspace/src', kind: 'directory' },
        { name: 'package.json', path: '/workspace/package.json', kind: 'file' },
        { name: 'link', path: '/workspace/link', kind: 'symlink' },
      ],
      truncated: false,
    })).toEqual({
      base: '/workspace',
      prefix: 'src',
      entries: [
        { name: 'src', path: '/workspace/src', kind: 'directory' },
        { name: 'package.json', path: '/workspace/package.json', kind: 'file' },
      ],
      truncated: false,
    });
    const overflowing = Array.from({ length: PATH_COMPLETE_LIMIT + 1 }, (_unused, index) => ({
      name: `f${index}`,
      path: `/workspace/f${index}`,
      kind: 'file',
    }));
    const capped = parsePathCompletion({ base: '/workspace', prefix: 'f', entries: overflowing, truncated: false });
    expect(capped?.entries).toHaveLength(PATH_COMPLETE_LIMIT);
    expect(capped?.truncated).toBe(true);
    expect(parsePathCompletion({ base: '/workspace', prefix: 'f', entries: 'none', truncated: false })).toBeNull();
    expect(parsePathCompletion({ prefix: 'f', entries: [], truncated: false })).toBeNull();
    for (const input of NOT_AN_OBJECT) expect(parsePathCompletion(input)).toBeNull();
  });

  it('reads a tmux send result only with the literal success marker', () => {
    expect(parseTmuxSendResult({ session: 'dev', sent: true })).toEqual({ session: 'dev', sent: true });
    expect(parseTmuxSendResult({ session: 'dev' })).toBeNull();
    expect(parseTmuxSendResult({ session: 'dev', sent: 'true' })).toBeNull();
    expect(parseTmuxSendResult({ session: '', sent: true })).toBeNull();
    for (const input of NOT_AN_OBJECT) expect(parseTmuxSendResult(input)).toBeNull();
  });
});

describe('orchestration readers', () => {
  const JOB = {
    id: 'job-1',
    agentId: 'agent-1',
    agent: 'Worker',
    parentId: 'main',
    description: 'port the reader',
    todoTaskId: 'task-9',
    workflowId: 'workflow-1',
    workflowGeneration: 3,
    status: 'completed',
    createdAt: 1000,
    startedAt: 1100,
    finishedAt: 2200,
    result: {
      index: 1,
      id: 'job-1',
      agent: 'Worker',
      status: 'completed',
      output: 'done',
      usage: { promptTokens: 5, completionTokens: 6, totalTokens: 11 },
      error: null,
      artifactRef: 'artifact://job-1',
      historyRef: 'history://job-1',
      artifactUri: 'file:///artifacts/job-1',
    },
    softBudgetExhausted: true,
  };

  it('round-trips a job snapshot and refuses a drifted one', () => {
    expect(parseJobOutput({ job: JOB })).toEqual({ job: JOB });
    expect(parseJobOutput({ job: { ...JOB, status: 'exploded' } })).toBeNull();
    expect(parseJobOutput({ job: { ...JOB, agentId: '' } })).toBeNull();
    expect(parseJobOutput({ job: { ...JOB, createdAt: -1 } })).toBeNull();
    expect(parseJobOutput({ job: { ...JOB, result: { ...JOB.result, artifactUri: 7 } } })).toBeNull();
    expect(parseJobOutput({})).toBeNull();
    for (const input of NOT_AN_OBJECT) expect(parseJobOutput(input)).toBeNull();
  });

  it('round-trips a child transcript and refuses anything else', () => {
    expect(parseAgentHistory({ agentId: 'agent-1', text: 'hello' })).toEqual({ agentId: 'agent-1', text: 'hello' });
    expect(parseAgentHistory({ agentId: 'agent-1', text: '' })).toEqual({ agentId: 'agent-1', text: '' });
    expect(parseAgentHistory({ agentId: 'agent-1' })).toBeNull();
    expect(parseAgentHistory({ agentId: '', text: 'x' })).toBeNull();
    for (const input of NOT_AN_OBJECT) expect(parseAgentHistory(input)).toBeNull();
  });

  it('keeps only the spawn rows that name a job or an agent', () => {
    expect(parseTaskSpawnSpawns({
      spawns: [{ jobId: 'job-1', agentId: 'agent-1' }, { agentId: 'agent-2' }, {}, { jobId: 5 }, null],
    })).toEqual([
      { jobId: 'job-1', agentId: 'agent-1' },
      { jobId: null, agentId: 'agent-2' },
    ]);
    expect(parseTaskSpawnSpawns({ spawns: 'none' })).toEqual([]);
    for (const input of NON_PAYLOADS) expect(parseTaskSpawnSpawns(input)).toEqual([]);
    expect(parseTaskSpawnSpawns([])).toEqual([]);
  });

  it('reads hub receipts for the direct, workflow, and child send rows', () => {
    const receipts = [
      { to: 'Worker', outcome: 'woken', messageId: 'message-1', error: null },
      { to: 'Other', outcome: 'failed', messageId: null, error: 'unknown orchestration agent' },
    ];
    expect(parseHubSendResult({ receipts })).toEqual({ receipts });
    expect(parseWorkflowHubSendResult({ recipient: 'Worker', receipts })).toEqual({ recipient: 'Worker', receipts });
    expect(parseHubReceiptList(receipts)).toEqual(receipts);
    expect(parseHubSendResult({ receipts: [{ outcome: 'woken' }] })).toBeNull();
    expect(parseHubSendResult({ receipts: [{ to: 'Worker' }] })).toBeNull();
    expect(parseHubSendResult({ receipts: 'none' })).toBeNull();
    expect(parseWorkflowHubSendResult({ receipts })).toBeNull();
    expect(parseHubReceiptList({ receipts })).toBeNull();
    for (const input of NOT_AN_OBJECT) {
      expect(parseHubSendResult(input)).toBeNull();
      expect(parseWorkflowHubSendResult(input)).toBeNull();
    }
    // A bare receipt list is a list payload: an empty one is legal.
    for (const input of NON_PAYLOADS) expect(parseHubReceiptList(input)).toBeNull();
    expect(parseHubReceiptList([])).toEqual([]);
  });
});

describe('persona and process readers', () => {
  const PERSONA_ROW = {
    name: 'mentor',
    description: 'review partner',
    source: 'user',
    trusted: true,
    preferred: false,
    contractSummary: 'plans then reviews',
    memoryEntries: 2,
    sessionCount: 1,
    stateError: null,
  };

  it('drops malformed persona rows instead of rendering them', () => {
    expect(parsePersonaList({ personas: [PERSONA_ROW, { name: 'broken' }], enabled: true })).toEqual([PERSONA_ROW]);
    expect(parsePersonaList({ personas: 'none' })).toEqual([]);
    for (const input of NON_PAYLOADS) expect(parsePersonaList(input)).toEqual([]);
    expect(parsePersonaList({ personas: [] })).toEqual([]);
  });

  it('reads a persona detail and its bounded definition', () => {
    const detail = { ...PERSONA_ROW, content: '---\nname: mentor\n---\n', contentTruncated: false };
    expect(parsePersonaDetail(detail)).toEqual(detail);
    expect(parsePersonaDetail({ ...PERSONA_ROW })).toBeNull();
    expect(parsePersonaDetail({ ...detail, contentTruncated: 'no' })).toBeNull();
    expect(parsePersonaDetail({ ...detail, name: '' })).toBeNull();
    for (const input of NOT_AN_OBJECT) expect(parsePersonaDetail(input)).toBeNull();
  });

  it('reads the human-facing message of a persona mutation', () => {
    expect(parsePersonaMessage({ name: 'mentor', created: true, message: 'created mentor' })).toBe('created mentor');
    expect(parsePersonaMessage({ created: true })).toBeNull();
    expect(parsePersonaMessage({ message: 7 })).toBeNull();
    for (const input of NOT_AN_OBJECT) expect(parsePersonaMessage(input)).toBeNull();
  });

  it('normalizes a process list and drops anonymous rows', () => {
    expect(parseProcessList([
      { id: 'process-1', label: 'dev server', state: 'running', pid: 42 },
      { id: 7, state: 'exited', exitCode: 0 },
      {},
    ])).toEqual([
      { id: 'process-1', label: 'dev server', state: 'running', pid: 42, exitCode: null },
      { id: '7', label: null, state: 'exited', pid: null, exitCode: 0 },
    ]);
    expect(parseProcessList('none')).toBeNull();
    for (const input of NON_PAYLOADS) expect(parseProcessList(input)).toBeNull();
    expect(parseProcessList([])).toEqual([]);
  });

  it('reads a bounded process log page and refuses a damaged one', () => {
    const page = {
      requestedCursor: 0,
      startCursor: 4,
      cursor: 12,
      eof: true,
      lost: true,
      lostBytes: 4,
      chunks: [{ startCursor: 4, cursor: 12, stream: 'stdout', dataBase64: 'b2s=' }],
    };
    expect(parseProcessLogs(page)).toEqual(page);
    expect(parseProcessLogs({ ...page, chunks: [{ startCursor: 4, cursor: 12, stream: 'stdout' }] })).toBeNull();
    expect(parseProcessLogs({ ...page, chunks: 'none' })).toBeNull();
    expect(parseProcessLogs({ ...page, lost: 'yes' })).toBeNull();
    expect(parseProcessLogs({ ...page, cursor: -1 })).toBeNull();
    for (const input of NOT_AN_OBJECT) expect(parseProcessLogs(input)).toBeNull();
  });
});

describe('settings readers', () => {
  const DEFINITION = {
    key: 'compaction.enabled',
    category: 'compaction',
    valueType: { kind: 'boolean', nonEmpty: false, min: null, max: null },
    defaultJson: 'true',
    description: 'auto compact when the context fills',
    enumValues: [],
    scopes: 'globalAndProject',
    behavior: 'live',
    secret: false,
    trustSensitive: false,
  };
  const VIEW = {
    definition: DEFINITION,
    effectiveValue: true,
    source: 'default',
    globalValue: null,
    projectValue: null,
    sessionOverrideValue: null,
    editableGlobal: true,
    editableProject: true,
    redacted: false,
  };

  it('round-trips a catalog and defaults the per-kind value extras', () => {
    const catalog = {
      projectTrusted: true,
      globalPath: '/workspace/global/settings.json',
      projectPath: '/workspace/.pi/settings.json',
      values: [{ ...VIEW, definition: { ...DEFINITION, valueType: { kind: 'boolean' } } }],
    };
    expect(parseSettingsCatalog(catalog)).toEqual({
      ...catalog,
      values: [VIEW],
    });
    expect(parseSettingsCatalog({ ...catalog, values: [{ ...VIEW, editableGlobal: 'yes' }] })).toBeNull();
    expect(parseSettingsCatalog({ ...catalog, values: [{ ...VIEW, definition: { ...DEFINITION, enumValues: 'none' } }] })).toBeNull();
    expect(parseSettingsCatalog({ ...catalog, projectTrusted: 'yes' })).toBeNull();
    for (const input of NOT_AN_OBJECT) expect(parseSettingsCatalog(input)).toBeNull();
  });

  it('round-trips a draft and refuses one without values', () => {
    const draft = { draftId: 'draft-1', scope: 'global', dirty: true, values: [VIEW] };
    expect(parseSettingsDraft(draft)).toEqual(draft);
    expect(parseSettingsDraft({ ...draft, values: undefined })).toBeNull();
    expect(parseSettingsDraft({ ...draft, draftId: '' })).toBeNull();
    expect(parseSettingsDraft({ ...draft, dirty: 'yes' })).toBeNull();
    for (const input of NOT_AN_OBJECT) expect(parseSettingsDraft(input)).toBeNull();
  });

  it('accepts Rust writes and rejects incomplete apply acknowledgments', () => {
    const outcome = {
      appliedLive: true,
      reloaded: true,
      restartRequired: false,
      writes: [{ key: 'compaction.enabled', needsReload: true, needsRestart: false }],
    };
    expect(parseSettingsApplyOutcome(outcome)).toEqual({
      appliedLive: true,
      reloaded: true,
      restartRequired: false,
      results: outcome.writes,
    });
    expect(parseSettingsApplyOutcome({
      ...outcome,
      reloaded: false,
    })).toBeNull();
    expect(parseSettingsApplyOutcome({})).toBeNull();
    expect(parseSettingsApplyOutcome({ ...outcome, appliedLive: 'yes' })).toBeNull();
    expect(parseSettingsApplyOutcome({ ...outcome, writes: [{ key: 'compaction.enabled' }] })).toBeNull();
    expect(parseSettingsApplyOutcome({ ...outcome, writes: undefined, results: outcome.writes })).toBeNull();
    for (const input of NOT_AN_OBJECT) expect(parseSettingsApplyOutcome(input)).toBeNull();
  });
});

describe('workflow readers', () => {
  const ROW = {
    workflowId: 'workflow-1',
    name: 'protocol audit',
    objective: 'audit the wire',
    status: 'running',
    generation: 2,
    worktree: 'workflow-1',
    branch: 'main',
    supervisorAgentId: 'agent-supervisor',
    supervisorJobId: 'job-supervisor',
    failure: null,
    integration: null,
  };

  const DETAIL = {
    id: 'workflow-1',
    sessionId: 'session-1',
    generation: 2,
    name: 'protocol audit',
    objective: 'audit the wire',
    status: 'running',
    failure: null,
    supervisor: {
      name: 'Supervisor',
      role: 'main',
      status: 'running',
      task: 'plan the audit',
      taskId: null,
      activity: [{ atMs: 10, kind: 'irc', text: 'IRQ sent' }],
    },
    subagents: [{
      name: 'Worker',
      role: 'worker',
      status: 'running',
      task: 'port the reader',
      taskId: 'task-1',
      activity: [{ atMs: 11, kind: 'task', text: 'started' }],
    }],
    jobs: [{
      displayName: 'Worker',
      jobId: 'job-1',
      role: 'worker',
      status: 'running',
      taskSummary: 'port the reader',
      todoTaskId: 'task-1',
      createdAtMs: 20,
      startedAtMs: 21,
      finishedAtMs: null,
      softBudgetExhausted: false,
    }],
    activeTasks: [{ taskId: 'task-1', summary: 'port the reader' }],
    worktree: { label: 'workflow-1', branch: 'main' },
    integration: {
      step: 'pending',
      summary: '',
      filesChanged: 0,
      insertions: 0,
      deletions: 0,
      conflicts: [],
    },
    planningActivity: [{ atMs: 30, kind: 'thinking', text: 'weighing options' }],
    planningStartedAtMs: 25,
    recentIrc: [{ sender: 'Worker', to: 'Main', text: 'hello', atMs: 31, messageId: 'message-1', replyTo: null }],
  };

  it('round-trips workflow rows and refuses a drifted status', () => {
    expect(parseWorkflowList({ workflows: [ROW] })).toEqual([ROW]);
    expect(parseWorkflowSnapshotRow(ROW)).toEqual(ROW);
    expect(parseWorkflowList({ workflows: [{ ...ROW, status: 'melted' }] })).toBeNull();
    expect(parseWorkflowList({ workflows: [{ ...ROW, workflowId: '' }] })).toBeNull();
    expect(parseWorkflowSnapshotRow({ ...ROW, objective: 7 })).toBeNull();
    for (const input of NOT_AN_OBJECT) {
      expect(parseWorkflowList(input)).toBeNull();
      expect(parseWorkflowSnapshotRow(input)).toBeNull();
    }
  });

  it('round-trips a detail projection and refuses a drifted nested row', () => {
    expect(parseWorkflowDetail(DETAIL)).toEqual(DETAIL);
    expect(parseWorkflowDetail({ ...DETAIL, worktree: undefined })).toBeNull();
    expect(parseWorkflowDetail({ ...DETAIL, integration: { ...DETAIL.integration, step: 'melted' } })).toBeNull();
    expect(parseWorkflowDetail({ ...DETAIL, subagents: 'none' })).toBeNull();
    expect(parseWorkflowDetail({ ...DETAIL, recentIrc: [{ sender: 'Worker' }] })).toBeNull();
    expect(parseWorkflowDetail({ ...DETAIL, jobs: [{ displayName: 'Worker' }] })).toBeNull();
    expect(parseWorkflowDetail({ ...DETAIL, supervisor: { name: 'Supervisor' } })).toBeNull();
    expect(parseWorkflowDetail({ ...DETAIL, planningActivity: [{ atMs: 30, kind: '', text: 'x' }] })).toBeNull();
    for (const input of NOT_AN_OBJECT) expect(parseWorkflowDetail(input)).toBeNull();
  });

  it('keeps an absent failure and empty worktree label legal', () => {
    const empty = { ...DETAIL, worktree: { label: '', branch: '' }, failure: null, supervisor: null };
    expect(parseWorkflowDetail(empty)).toEqual(empty);
  });
});

describe('side chat readers', () => {
  const SNAPSHOT = {
    active: 'side_a',
    maxTabs: 4,
    tabs: [{
      name: 'side_a',
      streaming: false,
      status: 'idle',
      entries: [
        {
          id: 'entry-1',
          kind: 'user',
          role: 'user',
          text: 'hi',
          toolName: null,
          toolArgs: { command: 'ls' },
          output: null,
          thinking: null,
          isError: false,
          isPartial: false,
        },
        {
          id: 'entry-2',
          kind: 'tool',
          role: 'tool',
          text: 'ran ls',
          toolName: 'bash',
          toolArgs: { command: 'ls' },
          output: 'a.ts',
          thinking: null,
          isError: false,
          isPartial: true,
        },
      ],
    }],
  };

  it('round-trips a snapshot and reports a prompt verdict only when stamped', () => {
    expect(parseSideChatSnapshot(SNAPSHOT)).toEqual({ ...SNAPSHOT, accepted: null, busy: null });
    expect(parseSideChatSnapshot({ ...SNAPSHOT, accepted: false, busy: true }))
      .toEqual({ ...SNAPSHOT, accepted: false, busy: true });
    expect(parseSideChatSnapshot({ ...SNAPSHOT, accepted: 'yes' })).toBeNull();
    expect(parseSideChatSnapshot({ ...SNAPSHOT, tabs: [{ ...SNAPSHOT.tabs[0], entries: [{ id: 'x' }] }] })).toBeNull();
    expect(parseSideChatSnapshot({ ...SNAPSHOT, tabs: [{ ...SNAPSHOT.tabs[0], entries: 'none' }] })).toBeNull();
    expect(parseSideChatSnapshot({ ...SNAPSHOT, maxTabs: -1 })).toBeNull();
    for (const input of NOT_AN_OBJECT) expect(parseSideChatSnapshot(input)).toBeNull();
  });
});

describe('todo child readers', () => {
  const TASK = {
    id: 'task-1',
    content: 'port the reader',
    dependsOn: [],
    status: 'running',
    agent: 'Worker',
    role: 'worker',
    createdAt: 5,
    summary: 'work in progress',
  };
  const RECEIPT = {
    id: 'receipt-1',
    parentSessionId: 'parent-1',
    childSessionId: 'child-1',
    messageId: 'message-1',
    acceptedAt: 6,
    status: 'running',
    summary: null,
  };
  const AGENT = { id: 'agent-1', displayName: 'Worker', role: 'worker', status: 'running', taskId: 'task-1' };
  const DETAIL = {
    parentSessionId: 'parent-1',
    childSessionId: 'child-1',
    todoId: 'child-1',
    parent: { sessionId: 'parent-1', entryId: 'entry-1', revision: 2 },
    workspace: { workspace: 'workspace-1', cwd: '/workspace' },
    status: 'running',
    isRunning: true,
    revision: 4,
    label: 'child',
    createdAt: 1,
    lastError: null,
    transcriptEntries: 1,
    tasks: [TASK],
    receipts: [RECEIPT],
    supervisor: null,
    agents: [AGENT],
    jobs: [{
      jobId: 'job-1',
      displayName: 'Worker',
      role: 'worker',
      status: 'running',
      taskId: 'task-1',
      summary: 'work in progress',
    }],
    activity: [{ atMs: 7, actor: 'Worker', kind: 'task', text: 'started' }],
    irc: [{ messageId: 'message-2', from: 'Worker', to: 'Main', text: 'hello', atMs: 8, replyTo: null }],
    transcript: [{ id: 'entry-1', role: 'assistant', actor: 'Worker', text: 'working', atMs: 9, interrupted: false }],
  };

  it('reads a manifest and refuses a duplicate child identity', () => {
    const entry = {
      childSessionId: 'child-1',
      label: 'child',
      createdAt: 1,
      status: 'running',
      parentEntryId: 'entry-1',
      parentRevision: 2,
      kind: 'todo',
    };
    expect(parseTodoChildManifest([entry])).toEqual([entry]);
    expect(parseTodoChildManifest([entry, { ...entry }])).toBeNull();
    expect(parseTodoChildManifest([{ ...entry, kind: 'session' }])).toBeNull();
    expect(parseTodoChildManifest([{ ...entry, createdAt: -1 }])).toBeNull();
    expect(parseTodoChildManifest({ entries: [entry] })).toBeNull();
    for (const input of NON_PAYLOADS) expect(parseTodoChildManifest(input)).toBeNull();
    expect(parseTodoChildManifest([])).toEqual([]);
  });

  it('round-trips a detail snapshot and enforces its own invariants', () => {
    expect(parseTodoChildDetail(DETAIL)).toEqual(DETAIL);
    expect(parseTodoChildDetail({ ...DETAIL, isRunning: false })).toBeNull();
    expect(parseTodoChildDetail({ ...DETAIL, todoId: 'other-child' })).toBeNull();
    expect(parseTodoChildDetail({ ...DETAIL, parent: { ...DETAIL.parent, sessionId: 'other-parent' } })).toBeNull();
    expect(parseTodoChildDetail({
      ...DETAIL,
      receipts: [{ ...RECEIPT, childSessionId: 'other-child' }],
    })).toBeNull();
    expect(parseTodoChildDetail({ ...DETAIL, tasks: [TASK, { ...TASK }] })).toBeNull();
    expect(parseTodoChildDetail({ ...DETAIL, supervisor: AGENT })).toBeNull();
    expect(parseTodoChildDetail({ ...DETAIL, lastError: 7 })).toBeNull();
    for (const input of NOT_AN_OBJECT) expect(parseTodoChildDetail(input)).toBeNull();
  });

  it('reads a lifecycle status answer and refuses a drifted one', () => {
    const status = {
      parentSessionId: 'parent-1',
      childSessionId: 'child-1',
      status: 'running',
      isRunning: true,
      transcriptEntries: 1,
      revision: 4,
      label: 'child',
      createdAt: 1,
      lastError: null,
      tasks: [TASK],
      receipts: [RECEIPT],
    };
    expect(parseTodoChildStatus(status)).toEqual(status);
    expect(parseTodoChildStatus({ ...status, status: 'paused' })).toBeNull();
    expect(parseTodoChildStatus({ ...status, receipts: [{ ...RECEIPT, id: 'receipt-1' }, { ...RECEIPT }] })).toBeNull();
    expect(parseTodoChildStatus({ ...status, tasks: 'none' })).toBeNull();
    for (const input of NOT_AN_OBJECT) expect(parseTodoChildStatus(input)).toBeNull();
  });

  it('trusts a created child id only when every identity names the parent', () => {
    const created = {
      identity: { parentSessionId: 'parent-1', childSessionId: 'child-1' },
      snapshot: { parentSessionId: 'parent-1', childSessionId: 'child-1' },
    };
    expect(parseTodoChildCreateId(created, 'parent-1')).toBe('child-1');
    expect(parseTodoChildCreateId(created, 'other-parent')).toBeNull();
    expect(parseTodoChildCreateId({
      identity: { parentSessionId: 'parent-1', childSessionId: 'child-1' },
      snapshot: { parentSessionId: 'parent-1', childSessionId: 'child-2' },
    }, 'parent-1')).toBeNull();
    for (const input of NOT_AN_OBJECT) expect(parseTodoChildCreateId(input, 'parent-1')).toBeNull();
  });
});

describe('code review readers', () => {
  const DIFF_PAGE = {
    snapshotId: 'snapshot-1',
    path: 'src/a.ts',
    previousPath: 'src/old.ts',
    binary: false,
    status: 'modified',
    hunks: [{
      index: 0,
      header: '@@ -1,2 +1,3 @@',
      oldStart: 1,
      oldCount: 2,
      newStart: 1,
      newCount: 3,
      contentHash: 'hash-1',
      totalLines: 3,
      complete: true,
      lineStart: 0,
      lines: [
        { kind: 'context', text: 'const a = 1;', oldNo: 1, newNo: 1 },
        { kind: 'addition', text: 'const b = 2;', newNo: 2 },
      ],
    }],
    cursor: 0,
    nextCursor: 1000,
    hasMore: true,
    totalLines: 3,
    hunkCount: 1,
    byteCapped: false,
  };

  const CONTEXT_PAGE = {
    snapshotId: 'snapshot-1',
    path: 'src/a.ts',
    oldLineCount: 3,
    newLineCount: 3,
    binary: false,
    lines: [{ kind: 'context', text: 'const a = 1;', oldNo: 1, newNo: 1 }],
  };

  it('round-trips a single-file diff page and refuses a damaged one', () => {
    expect(parseCodeReviewFileDiffPage(DIFF_PAGE)).toEqual(DIFF_PAGE);
    expect(parseCodeReviewFileDiffPage({ ...DIFF_PAGE, hunks: [{ ...DIFF_PAGE.hunks[0], contentHash: '' }] })).toBeNull();
    expect(parseCodeReviewFileDiffPage({ ...DIFF_PAGE, hunks: 'none' })).toBeNull();
    expect(parseCodeReviewFileDiffPage({ ...DIFF_PAGE, nextCursor: -1 })).toBeNull();
    expect(parseCodeReviewFileDiffPage({ ...DIFF_PAGE, path: '' })).toBeNull();
    for (const input of NOT_AN_OBJECT) expect(parseCodeReviewFileDiffPage(input)).toBeNull();
  });

  it('coerces diff lines and refuses a page without identity', () => {
    const coerced = parseCodeReviewFileDiffPage({
      ...DIFF_PAGE,
      hunks: [{
        ...DIFF_PAGE.hunks[0],
        lines: [{ kind: 'unknown-kind', text: 5 }, { kind: 'deletion', text: 'gone', oldNo: 3.7 }],
      }],
    });
    expect(coerced?.hunks[0].lines).toEqual([
      { kind: 'meta', text: '' },
      { kind: 'deletion', text: 'gone', oldNo: 3 },
    ]);
  });

  it('round-trips a file context page and refuses a damaged one', () => {
    expect(parseCodeReviewFileContextPage(CONTEXT_PAGE)).toEqual(CONTEXT_PAGE);
    expect(parseCodeReviewFileContextPage({ ...CONTEXT_PAGE, snapshotId: '' })).toBeNull();
    expect(parseCodeReviewFileContextPage({ ...CONTEXT_PAGE, lines: 'none' })).toBeNull();
    for (const input of NOT_AN_OBJECT) expect(parseCodeReviewFileContextPage(input)).toBeNull();
  });

  it('reads a close result, including the stale-review no-op', () => {
    expect(parseCodeReviewCloseResult({ closed: true })).toEqual({ closed: true, staleReview: false });
    expect(parseCodeReviewCloseResult({ closed: false, staleReview: true })).toEqual({ closed: false, staleReview: true });
    expect(parseCodeReviewCloseResult({ staleReview: true })).toBeNull();
    expect(parseCodeReviewCloseResult({ closed: 'true' })).toBeNull();
    for (const input of NOT_AN_OBJECT) expect(parseCodeReviewCloseResult(input)).toBeNull();
  });

  it('reads a model switch ack only when the review accepted it', () => {
    expect(parseCodeReviewSetModelAck({ ok: true, model: 'Sonnet 4.5' })).toEqual({ model: 'Sonnet 4.5' });
    expect(parseCodeReviewSetModelAck({ ok: false, model: 'Sonnet 4.5' })).toBeNull();
    expect(parseCodeReviewSetModelAck({ ok: true })).toBeNull();
    for (const input of NOT_AN_OBJECT) expect(parseCodeReviewSetModelAck(input)).toBeNull();
  });
});

describe('landed transport helpers', () => {
  it('still namespaces command ids per client generation', () => {
    const namespace = createClientNamespace(() => Uint8Array.from([1, 2, 3, 4, 5, 6]));
    expect(namespace).toBe('010203040506');
    expect(nextCommandId(namespace, 1)).toBe(`c${namespace}-1`);
    expect(nextCommandId(namespace, 1)).not.toMatch(/^m\d+$/);
    const other = createClientNamespace(() => Uint8Array.from([6, 5, 4, 3, 2, 1]));
    expect(nextCommandId(other, 1)).not.toBe(nextCommandId(namespace, 1));
    expect(nextCommandId(namespace, 2)).not.toBe(nextCommandId(namespace, 1));
  });

  it('still classes timeouts per command type with a fast-ack fallback', () => {
    expect(DEFAULT_COMMAND_TIMEOUT_MS).toBe(30_000);
    expect(LONG_OP_TIMEOUT_MS).toBe(600_000);
    for (const type of ['compact', 'snapcompact', 'get_messages', 'code_review_snapshot', 'new_session']) {
      expect(commandTimeoutMs(type)).toBe(LONG_OP_TIMEOUT_MS);
    }
    // A row added by this slice must NOT inherit the long bound by accident.
    for (const type of ['queue_send_now', 'side_chat_list', 'workflow_detail', 'agent_history', 'path_complete']) {
      expect(commandTimeoutMs(type)).toBe(DEFAULT_COMMAND_TIMEOUT_MS);
    }
  });

  it('still gates response ownership by command and source session', () => {
    expect(classifyResponseOwnership({ command: 'get_state', success: true }, { type: 'get_state' })).toBe('accept');
    expect(classifyResponseOwnership({ command: 'get_messages', success: true }, { type: 'get_state' }))
      .toBe('command-mismatch');
    expect(classifyResponseOwnership(
      { command: 'get_state', sessionId: 'other', success: true },
      { type: 'get_state', sessionId: 'session-1' },
    )).toBe('source-ownership-mismatch');
    expect(classifyResponseOwnership(
      { command: 'get_state', sessionId: 'session-1', success: true },
      { type: 'get_state', sessionId: 'session-1' },
    )).toBe('accept');
    // Lifecycle failures stay request-owned; a success envelope must be
    // target-owned by the payload session.
    expect(classifyResponseOwnership(
      { command: 'clone', sessionId: 'session-1', success: false },
      { type: 'clone', sessionId: 'session-1' },
    )).toBe('accept');
    expect(classifyResponseOwnership(
      { command: 'clone', sessionId: 'session-1', success: true, data: { sessionId: 'session-2' } },
      { type: 'clone', sessionId: 'session-1' },
    )).toBe('target-ownership-mismatch');
    expect(classifyResponseOwnership(
      { command: 'clone', sessionId: 'session-2', success: true, data: { sessionId: 'session-2' } },
      { type: 'clone', sessionId: 'session-1' },
    )).toBe('accept');
  });

  // The runner tags a successful `switch_session` with the TARGET runtime, so the
  // gate must exempt it like the other lifecycle cuts; otherwise every Resume is
  // rejected as a source mismatch and the switch silently never applies.
  it('accepts a target-owned successful switch_session and keeps failures source-owned', () => {
    expect(classifyResponseOwnership(
      { command: 'switch_session', sessionId: 'session-2', success: true, data: { sessionId: 'session-2' } },
      { type: 'switch_session', sessionId: 'session-1' },
    )).toBe('accept');
    expect(classifyResponseOwnership(
      { command: 'switch_session', sessionId: 'session-2', success: true, data: { sessionId: 'session-3' } },
      { type: 'switch_session', sessionId: 'session-1' },
    )).toBe('target-ownership-mismatch');
    expect(classifyResponseOwnership(
      { command: 'switch_session', sessionId: 'session-1', success: false },
      { type: 'switch_session', sessionId: 'session-1' },
    )).toBe('accept');
    expect(classifyResponseOwnership(
      { command: 'switch_session', sessionId: 'session-2', success: false },
      { type: 'switch_session', sessionId: 'session-1' },
    )).toBe('source-ownership-mismatch');
  });
});

describe('file conversation wire', () => {
  const CONV = '5f0c9e42-1f2a-4b8e-9c3d-7a6b5c4d3e2f';
  const ENTRY = {
    id: 'e1',
    kind: 'user',
    role: 'user',
    text: 'explain this file',
    toolName: null,
    toolArgs: null,
    output: null,
    thinking: null,
    isError: false,
    isPartial: false,
  };

  function conversationSnapshot(overrides: Record<string, unknown> = {}): Record<string, unknown> {
    return {
      conversationId: CONV,
      path: 'src/app.ts',
      streaming: false,
      status: 'idle',
      entries: [],
      ...overrides,
    };
  }

  it('mints and enforces the RFC 4122 v4 conversation id contract', () => {
    expect(isValidConversationId(CONV)).toBe(true);
    expect(createConversationId(() => new Uint8Array(16).fill(0xab))).toBe('abababab-abab-4bab-abab-abababababab');
    expect(isValidConversationId(createConversationId(() => new Uint8Array(16).fill(0xab)))).toBe(true);
    for (const bad of [
      null,
      7,
      '5F0C9E42-1F2A-4B8E-9C3D-7A6B5C4D3E2F',
      '5f0c9e421f2a4b8e9c3d7a6b5c4d3e2f',
      '5f0c9e42-1f2a-5b8e-9c3d-7a6b5c4d3e2f',
      '5f0c9e42-1f2a-4b8e-c3d-7a6b5c4d3e2f',
      '5f0c9e42-1f2a-4b8e-9c3d-7a6b5c4d3e2f0',
      '',
    ]) {
      expect(isValidConversationId(bad)).toBe(false);
    }
  });

  it('binds a snapshot to the open tab and refuses a stranger conversation', () => {
    const snapshot = conversationSnapshot({ entries: [ENTRY] });
    expect(parseFileConversationSnapshot(snapshot, CONV)).toEqual(snapshot);
    expect(parseFileConversationSnapshot({ ...snapshot, conversationId: 'not-a-uuid' }, 'not-a-uuid')).toBeNull();
    expect(parseFileConversationSnapshot(
      { ...snapshot, conversationId: '9e8d7c6b-5a4f-4e3d-2c1b-0a9b8c7d6e5f' },
      CONV,
    )).toBeNull();
    for (const input of NOT_AN_OBJECT) expect(parseFileConversationSnapshot(input, CONV)).toBeNull();
  });

  it('requires exact snapshot fields and reuses the side-chat entry parser', () => {
    expect(parseFileConversationSnapshot(conversationSnapshot({ path: '' }), CONV)).toBeNull();
    expect(parseFileConversationSnapshot(conversationSnapshot({ streaming: 'no' }), CONV)).toBeNull();
    expect(parseFileConversationSnapshot(conversationSnapshot({ status: 'streaming' }), CONV)).toEqual(
      conversationSnapshot({ status: 'streaming' }),
    );
    expect(parseFileConversationSnapshot(conversationSnapshot({ status: null }), CONV)).toBeNull();
    expect(parseFileConversationSnapshot(conversationSnapshot({ status: undefined }), CONV)).toBeNull();
    expect(parseFileConversationSnapshot(conversationSnapshot({ status: 7 }), CONV)).toBeNull();
    expect(parseFileConversationSnapshot(conversationSnapshot({ entries: [ENTRY] }), CONV)).not.toBeNull();
    expect(parseFileConversationSnapshot(conversationSnapshot({ entries: [{ ...ENTRY, kind: 'kernel' }] }), CONV)).toBeNull();
    expect(parseFileConversationSnapshot(conversationSnapshot({ entries: [{ ...ENTRY, isPartial: 1 }] }), CONV)).toBeNull();
    expect(parseFileConversationSnapshot(conversationSnapshot({ entries: 'not-a-list' }), CONV)).toBeNull();
  });

  it('discloses context truncation and refuses a malformed explicit context', () => {
    const truncated = parseFileConversationSnapshot(
      conversationSnapshot({ context: { includedRange: { startLine: 1, endLine: 400 }, truncated: true } }),
      CONV,
    );
    expect(truncated?.context).toEqual({ includedRange: { startLine: 1, endLine: 400 }, truncated: true });
    const excluded = parseFileConversationSnapshot(
      conversationSnapshot({
        context: { requestedRange: { startLine: 1, endLine: 9000 }, includedRange: null, truncated: false },
      }),
      CONV,
    );
    expect(excluded?.context).toEqual({
      requestedRange: { startLine: 1, endLine: 9000 },
      includedRange: null,
      truncated: false,
    });
    expect(parseFileConversationSnapshot(conversationSnapshot({ context: undefined }), CONV)?.context).toBeUndefined();
    for (const context of [
      null,
      'context',
      {},
      { includedRange: { startLine: 0, endLine: 9 }, truncated: true },
      { includedRange: { startLine: 9, endLine: 4 }, truncated: true },
      { includedRange: { startLine: 1.5, endLine: 4 }, truncated: true },
      { includedRange: { startLine: 1, endLine: 4 } },
      { includedRange: null, truncated: 'no' },
      { includedRange: null, truncated: true, requestedRange: { startLine: 2 } },
    ]) {
      expect(parseFileConversationSnapshot(conversationSnapshot({ context }), CONV)).toBeNull();
    }
  });

  it('keeps accepted and busy verbatim instead of a draft success semantic', () => {
    const rejected = parseFileConversationSnapshot(conversationSnapshot({ accepted: false, busy: true }), CONV);
    expect(rejected?.accepted).toBe(false);
    expect(rejected?.busy).toBe(true);
    const quiet = parseFileConversationSnapshot(conversationSnapshot(), CONV);
    expect(quiet?.accepted).toBeUndefined();
    expect(quiet?.busy).toBeUndefined();
    expect('accepted' in quiet!).toBe(false);
    expect('busy' in quiet!).toBe(false);
    const accepted = parseFileConversationSnapshot(
      conversationSnapshot({ accepted: true, busy: false, streaming: true, status: 'streaming' }),
      CONV,
    );
    expect(accepted?.accepted).toBe(true);
    expect(accepted?.busy).toBe(false);
    expect(parseFileConversationSnapshot(conversationSnapshot({ accepted: 'yes' }), CONV)).toBeNull();
    expect(parseFileConversationSnapshot(conversationSnapshot({ busy: null }), CONV)).toBeNull();
  });

  it('answers close only with the exact conversation and a literal closed true', () => {
    expect(parseFileConversationClose({ conversationId: CONV, closed: true }, CONV)).toEqual({
      conversationId: CONV,
      closed: true,
    });
    expect(parseFileConversationClose({ conversationId: CONV, closed: false }, CONV)).toBeNull();
    expect(parseFileConversationClose({ conversationId: CONV }, CONV)).toBeNull();
    expect(parseFileConversationClose(
      { conversationId: '9e8d7c6b-5a4f-4e3d-2c1b-0a9b8c7d6e5f', closed: true },
      CONV,
    )).toBeNull();
    for (const input of NOT_AN_OBJECT) expect(parseFileConversationClose(input, CONV)).toBeNull();
  });

  it('types the failure codes and propagates unknown codes as failures', () => {
    for (const code of [
      'file_conversation_capacity',
      'file_conversation_superseded',
      'file_conversation_not_found',
      'file_conversation_binding_mismatch',
      'file_conversation_owner_capture_failed',
      'file_conversation_selection_too_large',
      'file_conversation_selection_out_of_range',
      'file_conversation_selection_invalid',
      'file_conversation_prompt_empty',
      'file_conversation_prompt_too_large',
      'file_conversation_read_failed',
      'file_conversation_open_failed',
      'file_line_too_large',
      'file_symlink',
      'file_unavailable',
      'directory_unavailable',
    ]) {
      expect(parseFileConversationErrorCode({ code })).toBe(code);
    }
    expect(parseFileConversationErrorCode({ code: 'not_a_file_conversation_code' })).toBeNull();
    expect(parseFileConversationErrorCode({ code: 'code_review_not_open' })).toBeNull();
    expect(parseFileConversationErrorCode({ code: 'toString' })).toBeNull();
    expect(parseFileConversationErrorCode({ code: 'constructor' })).toBeNull();
    expect(parseFileConversationErrorCode({})).toBeNull();
    expect(parseFileConversationErrorCode({ code: 7 })).toBeNull();
    for (const input of NOT_AN_OBJECT) expect(parseFileConversationErrorCode(input)).toBeNull();
  });

  it('builds prompt payloads through the dispatch gate, refusing bad selections and oversize', () => {
    expect(buildFileConversationPromptPayload(CONV, 'explain this')).toEqual({
      ok: true,
      command: { type: 'file_conversation_prompt', conversationId: CONV, message: 'explain this' },
    });
    const scoped = buildFileConversationPromptPayload(CONV, 'explain this', { startLine: 12, endLine: 18 });
    expect(scoped).toEqual({
      ok: true,
      command: {
        type: 'file_conversation_prompt',
        conversationId: CONV,
        message: 'explain this',
        selection: { startLine: 12, endLine: 18 },
      },
    });
    if (scoped.ok) expect(parseRunnerCommand(scoped.command)).toEqual(scoped.command);
    expect(buildFileConversationPromptPayload(CONV, 'm', { startLine: 12, endLine: 5 } as FileConversationRangeWire).ok).toBe(false);
    expect(buildFileConversationPromptPayload(CONV, 'm', { startLine: 0, endLine: 5 } as FileConversationRangeWire).ok).toBe(false);
    expect(buildFileConversationPromptPayload(CONV, 'm', { startLine: 3 } as unknown as FileConversationRangeWire).ok).toBe(false);
    expect(buildFileConversationPromptPayload(CONV, 'x'.repeat(8192)).ok).toBe(true);
    expect(buildFileConversationPromptPayload(CONV, 'x'.repeat(8193)).ok).toBe(false);
    expect(buildFileConversationPromptPayload(CONV, 'é'.repeat(4096)).ok).toBe(true);
    expect(buildFileConversationPromptPayload(CONV, 'é'.repeat(4097)).ok).toBe(false);
    expect(buildFileConversationPromptPayload('not-a-uuid', 'm').ok).toBe(false);
    expect(FILE_CONVERSATION_CAPACITY_LIMIT).toBe(8);
    expect(FILE_CONVERSATION_PROMPT_MAX_BYTES).toBe(8192);
  });

  it('gates prompt bytes on Hermes where TextEncoder is absent', () => {
    const encoder = globalThis.TextEncoder;
    Reflect.deleteProperty(globalThis, 'TextEncoder');
    try {
      expect(buildFileConversationPromptPayload(CONV, 'x'.repeat(8192)).ok).toBe(true);
      expect(buildFileConversationPromptPayload(CONV, 'x'.repeat(8193)).ok).toBe(false);
      expect(buildFileConversationPromptPayload(CONV, 'é'.repeat(4096)).ok).toBe(true);
      expect(buildFileConversationPromptPayload(CONV, 'é'.repeat(4097)).ok).toBe(false);
      expect(buildFileConversationPromptPayload(CONV, '👍'.repeat(2048)).ok).toBe(true);
      expect(buildFileConversationPromptPayload(CONV, '👍'.repeat(2049)).ok).toBe(false);
    } finally {
      if (encoder !== undefined) globalThis.TextEncoder = encoder;
    }
  });
});

describe('commit view wire', () => {
  it('opens one commit review, mutually exclusive with the range forms', () => {
    const commit = parseCodeReviewScope('commit', '4be64ed', '');
    expect(commit).toEqual({ ok: true, command: { type: 'code_review_open', commit: '4be64ed' } });
    if (commit.ok) expect(parseRunnerCommand(commit.command)).toEqual(commit.command);
    expect(parseCodeReviewScope('commit', '4be64ed', 'main').ok).toBe(false);
    expect(parseCodeReviewScope('commit', '', '').ok).toBe(false);
    expect(parseCodeReviewScope('commit', 'two tokens', '').ok).toBe(false);
    expect(parseCodeReviewScope('workingTree', '', '')).toEqual({ ok: true, command: { type: 'code_review_open' } });
    expect(parseCodeReviewScope('base', 'main', '')).toEqual({
      ok: true,
      command: { type: 'code_review_open', from: 'main', includeWorkingTree: true },
    });
    expect(parseCodeReviewScope('range', 'main', 'HEAD')).toEqual({
      ok: true,
      command: { type: 'code_review_open', from: 'main', to: 'HEAD' },
    });
    expect(CODE_REVIEW_SCOPE_USAGE).toBe('usage: /code-review [<base> | <from> <to>]');
  });

  it('reads code_review_commits and refuses malformed or duplicated rows', () => {
    const row = {
      commit: '4be64ed1',
      parentCommit: '9ab77c22',
      parentCount: 1,
      subject: 'fix: file conversation binding',
      author: 'cj',
      authorDate: '2026-10-25T14:03:00+02:00',
    };
    const root = { ...row, commit: '00000001', parentCommit: null, parentCount: 0 };
    const merge = { ...row, commit: '22223333', parentCount: 2 };
    const data = { commits: [row, root, merge], truncated: false };
    expect(parseCodeReviewCommits(data)).toEqual(data);
    expect(parseCodeReviewCommits({ commits: [], truncated: true })).toEqual({ commits: [], truncated: true });
    expect(parseCodeReviewCommits({ commits: [{ ...row, subject: '', author: '' }], truncated: false }))
      .toEqual({ commits: [{ ...row, subject: '', author: '' }], truncated: false });
    for (const bad of [
      { ...data, commits: [{ ...row, commit: '' }] },
      { ...data, commits: [{ ...row, parentCommit: undefined }] },
      { ...data, commits: [{ ...row, parentCount: -1 }] },
      { ...data, commits: [{ ...row, authorDate: 'yesterday' }] },
      { ...data, commits: [{ ...row, authorDate: 1_761_400_000 }] },
      { ...data, commits: [{ ...row, authorDate: '2026-10-25 14:03:00+02:00' }] },
      { ...data, commits: [{ ...row, subject: 5 }] },
      { ...data, commits: 'not-a-list' },
      { commits: [row, { ...row }], truncated: false },
      { commits: [row], truncated: 'no' },
      { commits: [row] },
    ]) {
      expect(parseCodeReviewCommits(bad)).toBeNull();
    }
    for (const input of NOT_AN_OBJECT) expect(parseCodeReviewCommits(input)).toBeNull();
  });
});
