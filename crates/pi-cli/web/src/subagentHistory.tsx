// Child-transcript rows for the Subagents details modal.
//
// The input is the bounded, redacted text of the `agent_history` RPC, built by
// history_message_label (crates/pi-coding/src/orchestration/runtime.rs): one
// label per line, each already capped (HISTORY_LABEL_MAX_CHARS) —
//   user: <line> · assistant: <line> · custom: <line>
//   assistant · <concrete tool call target>   (one line per call)
//   [tool: <name>] · ok|error                 (outcome tag line)
//   [bash] <first line of the command>
// Two-space-indented lines are CONTINUATIONS of the row above them: extra
// prose lines for a text row, the bounded output preview for a call row.
// The producer emits the tag line today and the preview lines are the pending
// half of this contract (see the backend plan in the details-modal work); the
// client renders them the moment they arrive and invents nothing when they
// are absent. The core already bounds every label, so this module never
// truncates again: rows carry the FULL target (path / command), CSS soft-wraps
// instead of a `real…` stub, and prose renders through the shared MarkdownBody
// pipeline — the same renderer the transcript and side chat use — never as
// flat role-prefixed text; outcome tokens parse into closed state HERE.

import { useEffect, useRef, useState } from 'react';
import { MarkdownBody } from './MarkdownBody';
import { safeText } from './redact';
import { humanToolTitle } from './toolTitle';

/** Closed state parsed here from the wire outcome token; unknown stays opaque. */
export type HistoryToolStatus =
  | { kind: 'ok' }
  | { kind: 'error' }
  | { kind: 'unknown'; raw: string };

/** Closed child-message roles the wire protocol defines. */
export type HistoryRole = 'user' | 'assistant' | 'custom';

export type HistoryTranscriptRow =
  | { kind: 'text'; role: HistoryRole; text: string }
  | { kind: 'call'; name: string; summary: string; status: HistoryToolStatus; output: string }
  | { kind: 'line'; text: string };

type HistoryDraft =
  | HistoryTranscriptRow
  | { kind: 'result'; name: string; status: HistoryToolStatus };

const TOOL_RESULT = /^\[tool:\s*(.+?)\](?:\s*·\s*(.*))?$/;
const ROLE_TEXT = /^([a-z]+):\s*(.*)$/;
const INVOCATION = /^(?:assistant|tool)\s*·\s*(.+)$/i;
const NAMED_COMMAND = /^\[([a-z][a-z0-9_-]*)\]\s+(.*)$/i;
/** Continuation marker: exactly the two-space indent the label producer adds.
 *  No label ever starts with whitespace, so this cannot swallow a real row. */
const CONTINUATION = /^ {2}(.*)$/;

const ROLE_BY_TOKEN: Record<string, HistoryRole> = Object.assign(Object.create(null), {
  user: 'user',
  assistant: 'assistant',
  custom: 'custom',
});

function parseToolStatus(token: string | undefined): HistoryToolStatus {
  const raw = (token ?? '').trim();
  if (raw === 'ok') return { kind: 'ok' };
  if (raw === 'error') return { kind: 'error' };
  return { kind: 'unknown', raw };
}

function splitToolSummary(summary: string): { name: string; rest: string } {
  const trimmed = summary.trim();
  const match = /^([A-Za-z][A-Za-z0-9_-]*)(?:\s+(.*))?$/.exec(trimmed);
  if (!match) return { name: 'tool', rest: trimmed };
  return { name: match[1].toLowerCase(), rest: (match[2] ?? '').trim() };
}

function parseHistoryLine(line: string): HistoryDraft {
  const result = TOOL_RESULT.exec(line);
  if (result) {
    return { kind: 'result', name: result[1].trim().toLowerCase(), status: parseToolStatus(result[2]) };
  }
  const roleMatch = ROLE_TEXT.exec(line);
  const knownRole = roleMatch !== null ? ROLE_BY_TOKEN[roleMatch[1]] : undefined;
  if (roleMatch && knownRole) return { kind: 'text', role: knownRole, text: roleMatch[2] };
  const invocation = INVOCATION.exec(line);
  if (invocation) {
    const split = splitToolSummary(invocation[1]);
    return { kind: 'call', name: split.name, summary: split.rest, status: { kind: 'unknown', raw: '' }, output: '' };
  }
  const named = NAMED_COMMAND.exec(line);
  if (named) {
    return { kind: 'call', name: named[1].toLowerCase(), summary: named[2].trim(), status: { kind: 'unknown', raw: '' }, output: '' };
  }
  return { kind: 'line', text: line };
}

function rememberPending(
  pending: Map<string, number[]>,
  index: number,
  name: string,
): void {
  const key = name.toLowerCase();
  const list = pending.get(key) ?? [];
  list.push(index);
  pending.set(key, list);
}

function takePending(pending: Map<string, number[]>, name: string): number | null {
  const list = pending.get(name.toLowerCase());
  if (!list || list.length === 0) return null;
  const index = list.shift();
  if (list.length === 0) pending.delete(name.toLowerCase());
  return index ?? null;
}

/** Append one continuation line to the row it belongs to: prose gains lines
 *  (Markdown paragraphs survive the join), a call row gains its output
 *  preview. `owner` is the row the previous label was written to OR merged
 *  into — tool results merge into the call row they belong to, which is not
 *  always the last row. A stream slice can begin mid-block, so a leading
 *  continuation with no owner becomes a plain row rather than being attached
 *  to an unrelated one. */
function extendRow(rows: HistoryTranscriptRow[], owner: number, line: string): void {
  const row = owner >= 0 ? rows[owner] : undefined;
  const joined = (body: string): string => (body === '' ? line : `${body}\n${line}`);
  switch (row?.kind) {
    case 'text':
      rows[owner] = { ...row, text: joined(row.text) };
      return;
    case 'call':
      rows[owner] = { ...row, output: joined(row.output) };
      return;
    case 'line':
      rows[owner] = { ...row, text: joined(row.text) };
      return;
    default:
      rows.push({ kind: 'line', text: line });
  }
}

export function historyTranscriptRows(text: string): HistoryTranscriptRow[] {
  const rows: HistoryTranscriptRow[] = [];
  const pending = new Map<string, number[]>();
  // Row index the next continuation line extends.
  let owner = -1;
  for (const raw of text.split('\n')) {
    const line = raw.trimEnd();
    if (line.trim() === '') continue;
    const continuation = CONTINUATION.exec(line);
    if (continuation) {
      extendRow(rows, owner, continuation[1]);
      continue;
    }
    const parsed = parseHistoryLine(line);
    if (parsed.kind === 'result') {
      const matched = takePending(pending, parsed.name);
      if (matched !== null) {
        const target = rows[matched];
        if (target && target.kind === 'call') {
          rows[matched] = { ...target, status: parsed.status };
          owner = matched;
          continue;
        }
      }
      rows.push({ kind: 'call', name: parsed.name, summary: '', status: parsed.status, output: '' });
      owner = rows.length - 1;
      continue;
    }
    if (parsed.kind === 'call') rememberPending(pending, rows.length, parsed.name);
    rows.push(parsed);
    owner = rows.length - 1;
  }
  return rows;
}

/** Humanized row label: tool action for call rows, role for prose rows, null for unparsed lines. */
export function historyRowTag(row: HistoryTranscriptRow): string | null {
  switch (row.kind) {
    case 'text':
      return row.role;
    case 'call':
      return humanToolTitle(row.name);
    case 'line':
      return null;
  }
}

function historyRowKey(row: HistoryTranscriptRow, index: number): string {
  switch (row.kind) {
    case 'text':
      return `${index}:text:${row.role}`;
    case 'call':
      return `${index}:call:${row.name}`;
    case 'line':
      return `${index}:line`;
  }
}

/** Call row: full monospace target, compact outcome glyph, one labeled Output
 *  toggle; failed output starts open and re-opens when the status turns error. */
function CallRowView(props: { row: Extract<HistoryTranscriptRow, { kind: 'call' }> }) {
  const row = props.row;
  const isError = row.status.kind === 'error';
  const [override, setOverride] = useState<boolean | null>(null);
  const wasErrorRef = useRef(isError);
  useEffect(() => {
    if (isError && !wasErrorRef.current) setOverride(true);
    wasErrorRef.current = isError;
  }, [isError]);
  const open = override ?? isError;
  const hasStatus = row.status.kind !== 'unknown' || row.status.raw !== '';
  const tag = historyRowTag(row);
  return (
    <div className="subagent-details__history-row" data-history-kind="call">
      <div className="subagent-details__history-line">
        {tag !== null && (
          <span className="subagent-details__history-tag">{safeText(tag)}</span>
        )}
        <span className="subagent-details__history-text">{safeText(row.summary)}</span>
        {hasStatus && (row.status.kind === 'unknown' ? (
          <span className="subagent-details__history-status" data-history-status={row.status.raw}>
            {safeText(row.status.raw)}
          </span>
        ) : (
          <span
            className="subagent-details__history-status"
            data-history-status={row.status.kind}
            role="img"
            aria-label={isError ? 'failed' : 'succeeded'}
            title={isError ? 'failed' : 'succeeded'}
          >
            {isError ? '\u2717' : '\u2713'}
          </span>
        ))}
        {row.output !== '' && (
          <button
            type="button"
            className="subagent-details__history-output-toggle"
            data-history-output-toggle
            aria-expanded={open}
            aria-label={open ? 'Hide tool output' : 'Show tool output'}
            onClick={() => setOverride(!open)}
          >
            Output
          </button>
        )}
      </div>
      {row.output !== '' && open && (
        <pre className="subagent-details__history-output" data-history-output>
          {safeText(row.output)}
        </pre>
      )}
    </div>
  );
}

/** Prose/line row: full body, no clamp, no control; assistant prose unlabeled, user/custom keep theirs. */
function HistoryRowView(props: { row: HistoryTranscriptRow }) {
  const row = props.row;
  if (row.kind === 'call') return <CallRowView row={row} />;
  const tag = historyRowTag(row);
  const showTag = !(row.kind === 'text' && row.role === 'assistant');
  return (
    <div
      className="subagent-details__history-row"
      data-history-kind={row.kind}
    >
      <div className="subagent-details__history-line">
        {tag !== null && showTag && (
          <span className="subagent-details__history-tag">{safeText(tag)}</span>
        )}
        {row.kind === 'text' ? (
          <MarkdownBody className="subagent-details__history-md" text={row.text} />
        ) : (
          <span className="subagent-details__history-text">{safeText(row.text)}</span>
        )}
      </div>
    </div>
  );
}

export function SubagentHistoryList(props: { text: string; error: string }) {
  const placeholder =
    props.text !== ''
      ? null
      : props.error !== ''
        ? '(transcript unavailable)'
        : '(no transcript yet — waiting for child activity)';
  const rows = historyTranscriptRows(props.text);
  return (
    <div className="subagent-details__transcript" data-details-history>
      {placeholder !== null ? (
        <div className="subagent-details__history-row">
          <span className="subagent-details__history-text">{placeholder}</span>
        </div>
      ) : (
        rows.map((row, index) => (
          <HistoryRowView key={historyRowKey(row, index)} row={row} />
        ))
      )}
    </div>
  );
}
