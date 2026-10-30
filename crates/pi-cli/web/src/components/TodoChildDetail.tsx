import type { CSSProperties, ReactNode } from 'react';
import { safeText } from '../redact';

export type TodoChildLifecycleStatus =
  | 'created'
  | 'idle'
  | 'running'
  | 'paused'
  | 'stopped'
  | 'failed';

export type TodoChildTaskStatus =
  | 'pending'
  | 'running'
  | 'completed'
  | 'failed'
  | 'interrupted';

export interface TodoChildParentProvenance {
  sessionId: string;
  entryId?: string | null;
  revision: number;
}

export interface TodoChildWorkspaceIdentity {
  workspace: string;
  cwd: string;
}

export interface TodoChildDetailTask {
  id: string;
  content: string;
  dependsOn: string[];
  status: TodoChildTaskStatus;
  agent?: string | null;
  role?: string | null;
  createdAt: number;
  summary?: string | null;
}
export type TodoChildReceiptStatus = 'queued' | 'running' | 'completed' | 'failed';

export interface TodoChildDetailReceipt {
  id: string;
  parentSessionId: string;
  childSessionId: string;
  messageId: string;
  acceptedAt: number;
  status: TodoChildReceiptStatus;
  summary?: string | null;
}

export interface TodoChildDetailAgent {
  id: string;
  displayName: string;
  role?: string | null;
  status: string;
  taskId?: string | null;
}

export interface TodoChildDetailJob {
  jobId: string;
  displayName: string;
  role?: string | null;
  status: string;
  taskId?: string | null;
  summary?: string | null;
}

export interface TodoChildDetailActivity {
  atMs: number;
  actor?: string | null;
  kind: string;
  text: string;
}

export interface TodoChildDetailIrcMessage {
  messageId: string;
  from: string;
  to: string;
  text: string;
  atMs: number;
  replyTo?: string | null;
}

export interface TodoChildDetailTranscriptEntry {
  id: string;
  role: string;
  actor?: string | null;
  text: string;
  atMs?: number | null;
  interrupted?: boolean;
}

export interface TodoChildDetailSupervisor {
  id: string;
  displayName: string;
  role?: string | null;
  status: string;
  taskId?: string | null;
}

export interface TodoChildDetailSnapshot {
  parentSessionId: string;
  childSessionId: string;
  todoId: string;
  parent: TodoChildParentProvenance;
  workspace: TodoChildWorkspaceIdentity;
  status: TodoChildLifecycleStatus;
  isRunning: boolean;
  revision: number;
  label?: string | null;
  createdAt: number;
  lastError?: string | null;
  transcriptEntries: number;
  tasks: TodoChildDetailTask[];
  receipts: TodoChildDetailReceipt[];
  supervisor?: TodoChildDetailSupervisor | null;
  agents: TodoChildDetailAgent[];
  jobs: TodoChildDetailJob[];
  activity: TodoChildDetailActivity[];
  irc: TodoChildDetailIrcMessage[];
  transcript: TodoChildDetailTranscriptEntry[];
}

export interface TodoChildDetailProps {
  snapshot: TodoChildDetailSnapshot;
}

const TASK_LIMIT = 12;
const AGENT_LIMIT = 8;
const JOB_LIMIT = 8;
const ACTIVITY_LIMIT = 8;
const IRC_LIMIT = 8;
const TRANSCRIPT_LIMIT = 12;
const RECEIPT_LIMIT = 8;
const DEPENDENCY_LIMIT = 4;
const META_CHAR_LIMIT = 160;
const CONTENT_CHAR_LIMIT = 800;
const TRANSCRIPT_CHAR_LIMIT = 2_000;

const ROOT_STYLE: CSSProperties = {
  display: 'grid',
  gap: '1rem',
  minWidth: 0,
  overflow: 'hidden',
};

const HEADER_STYLE: CSSProperties = {
  display: 'grid',
  gap: '0.5rem',
  minWidth: 0,
};

const DEFINITION_LIST_STYLE: CSSProperties = {
  display: 'grid',
  gridTemplateColumns: 'max-content minmax(0, 1fr)',
  gap: '0.25rem 0.75rem',
  margin: 0,
  minWidth: 0,
};

const TERM_STYLE: CSSProperties = {
  fontWeight: 600,
};

const DEFINITION_STYLE: CSSProperties = {
  margin: 0,
  minWidth: 0,
  overflowWrap: 'anywhere',
};

const SECTION_STYLE: CSSProperties = {
  display: 'grid',
  gap: '0.5rem',
  minWidth: 0,
};

const LIST_STYLE: CSSProperties = {
  display: 'grid',
  gap: '0.5rem',
  listStyle: 'none',
  margin: 0,
  padding: 0,
  minWidth: 0,
};

const ITEM_STYLE: CSSProperties = {
  display: 'grid',
  gap: '0.25rem',
  minWidth: 0,
  overflow: 'hidden',
};

const META_STYLE: CSSProperties = {
  display: 'flex',
  flexWrap: 'wrap',
  gap: '0.25rem 0.75rem',
  minWidth: 0,
  overflowWrap: 'anywhere',
};

const CLAMPED_TEXT_STYLE: CSSProperties = {
  display: '-webkit-box',
  WebkitBoxOrient: 'vertical',
  WebkitLineClamp: 3,
  overflow: 'hidden',
  overflowWrap: 'anywhere',
  whiteSpace: 'pre-wrap',
};

const TRANSCRIPT_TEXT_STYLE: CSSProperties = {
  ...CLAMPED_TEXT_STYLE,
  WebkitLineClamp: 6,
};

const MUTED_STYLE: CSSProperties = {
  opacity: 0.72,
};

const TASK_STATUS_LABEL: Record<TodoChildTaskStatus, string> = {
  pending: 'Pending',
  running: 'Running',
  completed: 'Completed',
  failed: 'Failed',
  interrupted: 'Interrupted',
};

const TASK_STATUS_MARKER: Record<TodoChildTaskStatus, string> = {
  pending: '○',
  running: '●',
  completed: '✓',
  failed: '!',
  interrupted: '!',
};

/**
 * Pure Todo detail renderer. It consumes a frozen wire projection and exposes
 * no commands, network behavior, mutation callbacks, or lifecycle logic.
 * Todo children share their inherited workspace; worktree, integration, and
 * merge concepts are intentionally absent.
 */
export function TodoChildDetail({ snapshot }: TodoChildDetailProps) {
  const todoId = compactText(snapshot.todoId, META_CHAR_LIMIT) || 'unknown';
  const lifecycle = statusLabel(snapshot.status);
  const parent = snapshot.parent;
  const workspace = snapshot.workspace;
  const supervisor = snapshot.supervisor ?? null;

  return (
    <article
      className="todo-child-detail"
      style={ROOT_STYLE}
      aria-label={`Todo detail ${todoId}`}
    >
      <header className="todo-child-detail__header" style={HEADER_STYLE} aria-label="Todo header">
        <h2>
          Todo <code>{todoId}</code>
        </h2>
        {snapshot.label ? <div>{compactText(snapshot.label, META_CHAR_LIMIT)}</div> : null}
        <dl style={DEFINITION_LIST_STYLE}>
          <dt style={TERM_STYLE}>Parent provenance</dt>
          <dd style={DEFINITION_STYLE}>
            <code>{compactText(parent.sessionId, META_CHAR_LIMIT) || 'unknown'}</code>
            {parent.entryId ? (
              <>
                {' · entry '}
                <code>{compactText(parent.entryId, META_CHAR_LIMIT)}</code>
              </>
            ) : null}
            {' · revision '}
            {parent.revision}
          </dd>

          <dt style={TERM_STYLE}>Workspace</dt>
          <dd style={DEFINITION_STYLE}>
            {compactText(workspace.workspace, META_CHAR_LIMIT) || 'unknown'}
          </dd>
          <dt style={TERM_STYLE}>CWD</dt>
          <dd style={DEFINITION_STYLE}>
            <code>{compactText(workspace.cwd, META_CHAR_LIMIT) || 'unknown'}</code>
          </dd>

          <dt style={TERM_STYLE}>Lifecycle status</dt>
          <dd style={DEFINITION_STYLE}>
            <strong role="status" aria-label={`Lifecycle status: ${lifecycle}`}>
              {lifecycle}
            </strong>
          </dd>

          <dt style={TERM_STYLE}>Revision</dt>
          <dd style={DEFINITION_STYLE}>{snapshot.revision}</dd>
          <dt style={TERM_STYLE}>Runtime</dt>
          <dd style={DEFINITION_STYLE}>{snapshot.isRunning ? 'running' : 'not running'}</dd>

          <dt style={TERM_STYLE}>Created</dt>
          <dd style={DEFINITION_STYLE}><Clock atMs={snapshot.createdAt} /></dd>

          <dt style={TERM_STYLE}>Transcript entries</dt>
          <dd style={DEFINITION_STYLE}>{snapshot.transcriptEntries}</dd>

          <dt style={TERM_STYLE}>Collections</dt>
          <dd style={DEFINITION_STYLE}>
            {snapshot.tasks.length} tasks · {snapshot.receipts.length} receipts · {snapshot.agents.length} agents ·{' '}
            {snapshot.jobs.length} jobs
          </dd>

          {snapshot.lastError ? (
            <>
              <dt style={TERM_STYLE}>Last error</dt>
              <dd style={DEFINITION_STYLE}>{boundedText(snapshot.lastError, CONTENT_CHAR_LIMIT)}</dd>
            </>
          ) : null}

          {supervisor ? (
            <>
              <dt style={TERM_STYLE}>Supervisor</dt>
              <dd style={DEFINITION_STYLE}>
                {compactText(supervisor.displayName, META_CHAR_LIMIT) || 'unknown'}
                {' · '}
                {statusLabel(supervisor.status)}
                {supervisor.role ? (
                  <>
                    {' · '}
                    {compactText(supervisor.role, META_CHAR_LIMIT)}
                  </>
                ) : null}
                {supervisor.taskId ? (
                  <>
                    {' · task '}
                    <code>{compactText(supervisor.taskId, META_CHAR_LIMIT)}</code>
                  </>
                ) : null}
              </dd>
            </>
          ) : null}
        </dl>
      </header>

      <DetailSection label="Tasks" count={snapshot.tasks.length}>
        <BoundedList
          items={snapshot.tasks}
          limit={TASK_LIMIT}
          empty="No tasks"
          noun="tasks"
          render={(task) => <TaskRow key={task.id} task={task} />}
        />
      </DetailSection>
      <DetailSection label="Receipts" count={snapshot.receipts.length}>
        <RecentList
          items={snapshot.receipts}
          limit={RECEIPT_LIMIT}
          empty="No receipts"
          render={(receipt) => (
            <li key={receipt.id} style={ITEM_STYLE}>
              <div style={META_STYLE}>
                <Clock atMs={receipt.acceptedAt} />
                <strong>{statusLabel(receipt.status)}</strong>
                <span>Receipt <code>{compactText(receipt.id, META_CHAR_LIMIT)}</code></span>
                <span>Message <code>{compactText(receipt.messageId, META_CHAR_LIMIT)}</code></span>
              </div>
              {receipt.summary ? (
                <div style={CLAMPED_TEXT_STYLE}>{boundedText(receipt.summary, CONTENT_CHAR_LIMIT)}</div>
              ) : null}
            </li>
          )}
        />
      </DetailSection>

      <DetailSection label="Agents" count={snapshot.agents.length}>
        <BoundedList
          items={snapshot.agents}
          limit={AGENT_LIMIT}
          empty="No agents"
          noun="agents"
          render={(agent) => <AgentRow key={agent.id} agent={agent} />}
        />
      </DetailSection>

      <DetailSection label="Jobs" count={snapshot.jobs.length}>
        <BoundedList
          items={snapshot.jobs}
          limit={JOB_LIMIT}
          empty="No jobs"
          noun="jobs"
          render={(job) => <JobRow key={job.jobId} job={job} />}
        />
      </DetailSection>

      <DetailSection label="Activity" count={snapshot.activity.length}>
        <RecentList
          items={snapshot.activity}
          limit={ACTIVITY_LIMIT}
          empty="No activity"
          render={(entry, index) => (
            <li key={`${entry.atMs}-${index}`} style={ITEM_STYLE}>
              <div style={META_STYLE}>
                <Clock atMs={entry.atMs} />
                {entry.actor ? <span>{compactText(entry.actor, META_CHAR_LIMIT)}</span> : null}
                <span>{statusLabel(entry.kind)}</span>
              </div>
              <div style={CLAMPED_TEXT_STYLE}>{boundedText(entry.text, CONTENT_CHAR_LIMIT)}</div>
            </li>
          )}
        />
      </DetailSection>

      <DetailSection label="IRC" count={snapshot.irc.length}>
        <RecentList
          items={snapshot.irc}
          limit={IRC_LIMIT}
          empty="No IRC messages"
          render={(message) => (
            <li key={message.messageId} style={ITEM_STYLE}>
              <div style={META_STYLE}>
                <Clock atMs={message.atMs} />
                <span>
                  {compactText(message.from, META_CHAR_LIMIT) || 'unknown'} →{' '}
                  {compactText(message.to, META_CHAR_LIMIT) || 'unknown'}
                </span>
                {message.replyTo ? (
                  <span style={MUTED_STYLE}>
                    reply <code>{compactText(message.replyTo, META_CHAR_LIMIT)}</code>
                  </span>
                ) : null}
              </div>
              <div style={CLAMPED_TEXT_STYLE}>{boundedText(message.text, CONTENT_CHAR_LIMIT)}</div>
            </li>
          )}
        />
      </DetailSection>

      <DetailSection label="Transcript" count={snapshot.transcript.length}>
        <RecentList
          items={snapshot.transcript}
          limit={TRANSCRIPT_LIMIT}
          empty="No transcript entries"
          render={(entry) => (
            <li key={entry.id} style={ITEM_STYLE}>
              <div style={META_STYLE}>
                {entry.atMs != null ? <Clock atMs={entry.atMs} /> : null}
                <strong>{statusLabel(entry.role)}</strong>
                {entry.actor ? <span>{compactText(entry.actor, META_CHAR_LIMIT)}</span> : null}
                {entry.interrupted ? (
                  <strong role="status" aria-label="Transcript entry status: Interrupted">
                    Interrupted
                  </strong>
                ) : null}
              </div>
              <div style={TRANSCRIPT_TEXT_STYLE}>
                {boundedText(entry.text, TRANSCRIPT_CHAR_LIMIT, true)}
              </div>
            </li>
          )}
        />
      </DetailSection>
    </article>
  );
}

function TaskRow({ task }: { task: TodoChildDetailTask }) {
  const status = TASK_STATUS_LABEL[task.status];
  const dependencies = task.dependsOn.slice(0, DEPENDENCY_LIMIT);
  const omittedDependencies = task.dependsOn.length - dependencies.length;

  return (
    <li style={ITEM_STYLE}>
      <div style={META_STYLE}>
        <span aria-hidden="true">{TASK_STATUS_MARKER[task.status]}</span>
        <strong role="status" aria-label={`Task status: ${status}`}>
          {status}
        </strong>
        <span>
          ID <code>{compactText(task.id, META_CHAR_LIMIT) || 'unknown'}</code>
        </span>
        <span>
          Dependencies:{' '}
          {dependencies.length === 0 ? (
            'none'
          ) : (
            <>
              {dependencies.map((dependency, index) => (
                <span key={`${dependency}-${index}`}>
                  {index > 0 ? ', ' : ''}
                  <code>{compactText(dependency, META_CHAR_LIMIT)}</code>
                </span>
              ))}
              {omittedDependencies > 0 ? ` +${omittedDependencies} more` : ''}
            </>
          )}
        </span>
        <span>Agent: {compactText(task.agent, META_CHAR_LIMIT) || 'unassigned'}</span>
        <span>Role: {compactText(task.role, META_CHAR_LIMIT) || 'unassigned'}</span>
        <span>Created <Clock atMs={task.createdAt} /></span>
      </div>
      <div style={CLAMPED_TEXT_STYLE}>{boundedText(task.content, CONTENT_CHAR_LIMIT)}</div>
      {task.summary ? (
        <div style={CLAMPED_TEXT_STYLE}>{boundedText(task.summary, CONTENT_CHAR_LIMIT)}</div>
      ) : null}
    </li>
  );
}

function AgentRow({ agent }: { agent: TodoChildDetailAgent }) {
  return (
    <li style={ITEM_STYLE}>
      <div style={META_STYLE}>
        <strong>{compactText(agent.displayName, META_CHAR_LIMIT) || compactText(agent.id, META_CHAR_LIMIT) || 'unknown'}</strong>
        <span>{statusLabel(agent.status)}</span>
        <span>Role: {compactText(agent.role, META_CHAR_LIMIT) || 'unassigned'}</span>
        {agent.taskId ? (
          <span>
            Task <code>{compactText(agent.taskId, META_CHAR_LIMIT)}</code>
          </span>
        ) : null}
      </div>
    </li>
  );
}

function JobRow({ job }: { job: TodoChildDetailJob }) {
  return (
    <li style={ITEM_STYLE}>
      <div style={META_STYLE}>
        <strong>{compactText(job.displayName, META_CHAR_LIMIT) || 'unknown'}</strong>
        <span>{statusLabel(job.status)}</span>
        <span>Role: {compactText(job.role, META_CHAR_LIMIT) || 'unassigned'}</span>
        <span>
          Job <code>{compactText(job.jobId, META_CHAR_LIMIT) || 'unknown'}</code>
        </span>
        {job.taskId ? (
          <span>
            Task <code>{compactText(job.taskId, META_CHAR_LIMIT)}</code>
          </span>
        ) : null}
      </div>
      {job.summary ? (
        <div style={CLAMPED_TEXT_STYLE}>{boundedText(job.summary, CONTENT_CHAR_LIMIT)}</div>
      ) : null}
    </li>
  );
}

function DetailSection({ label, count, children }: { label: string; count: number; children: ReactNode }) {
  return (
    <section className="todo-child-detail__section" style={SECTION_STYLE} aria-label={label}>
      <h3>
        {label} <span aria-label={`${count} total`}>({count})</span>
      </h3>
      {children}
    </section>
  );
}

function BoundedList<T>({
  items,
  limit,
  empty,
  noun,
  render,
}: {
  items: readonly T[];
  limit: number;
  empty: string;
  noun: string;
  render: (item: T, index: number) => ReactNode;
}) {
  if (items.length === 0) {
    return <Empty text={empty} />;
  }
  const visible = items.slice(0, limit);
  const omitted = items.length - visible.length;
  return (
    <ol style={LIST_STYLE}>
      {visible.map(render)}
      {omitted > 0 ? <Omitted count={omitted} label={`more ${noun}`} /> : null}
    </ol>
  );
}

function RecentList<T>({
  items,
  limit,
  empty,
  render,
}: {
  items: readonly T[];
  limit: number;
  empty: string;
  render: (item: T, index: number) => ReactNode;
}) {
  if (items.length === 0) {
    return <Empty text={empty} />;
  }
  const first = Math.max(0, items.length - limit);
  const visible = items.slice(first);
  return (
    <ol style={LIST_STYLE}>
      {first > 0 ? <Omitted count={first} label="earlier" /> : null}
      {visible.map(render)}
    </ol>
  );
}

function Empty({ text }: { text: string }) {
  return (
    <p role="note" style={MUTED_STYLE}>
      {text}
    </p>
  );
}

function Omitted({ count, label }: { count: number; label: string }) {
  return (
    <li role="note" style={MUTED_STYLE}>
      {count} {label}
    </li>
  );
}

function Clock({ atMs }: { atMs: number }) {
  const clock = formatClock(atMs);
  return <span aria-label={`Timestamp ${clock}`}>{clock}</span>;
}

function compactText(value: unknown, limit: number): string {
  return boundedText(value, limit).replace(/\s+/g, ' ').trim();
}

function boundedText(value: unknown, limit: number, preserveWhitespace = false): string {
  const safe = safeText(value);
  const text = preserveWhitespace ? safe.trim() : safe.replace(/\s+/g, ' ').trim();
  if (text.length <= limit) return text;
  let end = Math.max(0, limit - 1);
  if (end > 0) {
    const last = text.charCodeAt(end - 1);
    if (last >= 0xd800 && last <= 0xdbff) end -= 1;
  }
  return `${text.slice(0, end).trimEnd()}…`;
}

function statusLabel(status: string): string {
  const label = compactText(status, META_CHAR_LIMIT).replace(/[_-]+/g, ' ');
  return label ? `${label.charAt(0).toUpperCase()}${label.slice(1)}` : 'Unknown';
}

function formatClock(atMs: number): string {
  const totalSeconds = Math.max(0, Math.floor(atMs / 1_000));
  const seconds = totalSeconds % 60;
  const minutes = Math.floor(totalSeconds / 60) % 60;
  const hours = Math.floor(totalSeconds / 3_600) % 24;
  return `${String(hours).padStart(2, '0')}:${String(minutes).padStart(2, '0')}:${String(seconds).padStart(2, '0')}`;
}
