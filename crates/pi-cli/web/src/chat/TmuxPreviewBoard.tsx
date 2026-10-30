// Tmux preview board — independent /chat runner rail: live pane previews, no
// send-keys. Same fail-closed fetch/seq/safeText contract as TmuxPanel.
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import type { RpcResponse } from '../types';
import { safeText } from '../redact';
import { AnsiText } from '../AnsiText';

import {
  parseTmuxPaneSnapshot,
  parseTmuxSessionList,
  type RunnerCommand,
  type TmuxPaneSnapshotData,
  type TmuxSessionListData,
  type TmuxSessionRow,
} from './protocol';
import {
  TMUX_NO_SERVER,
  TMUX_NO_SESSIONS,
  TMUX_PANE_UNAVAILABLE,
  TMUX_STATUS_UNAVAILABLE,
  tmuxErrorText,
} from './TmuxPanel';

/** Preview depth: every board card asks for this many lines. */
export const TMUX_PREVIEW_LINES = 24;

export interface TmuxPreviewBoardProps {
  runnerId: string;
  runnerLabel: string;
  sessionId: string | null;
  send: (command: RunnerCommand, options?: { sourceSessionId?: string }) => Promise<RpcResponse>;
  onOpenTerminal: (session: string) => void;
  refreshEpoch?: number;
}

export function TmuxPreviewBoard({ runnerId, runnerLabel, sessionId, send, onOpenTerminal, refreshEpoch = 0 }: TmuxPreviewBoardProps) {
  const commandOptions = useMemo(() => ({ sourceSessionId: sessionId ?? undefined }), [sessionId]);
  const [list, setList] = useState<TmuxSessionListData | null>(null);
  const [listFailed, setListFailed] = useState(false);
  const [listLoading, setListLoading] = useState(true);
  const [panes, setPanes] = useState<Record<string, TmuxPaneSnapshotData | null | undefined>>({});
  const boardSeq = useRef(0);
  const [boardBusy, setBoardBusy] = useState(false);
  const [boardError, setBoardError] = useState('');
  const [pendingDelete, setPendingDelete] = useState<string | null>(null);



  const fetchPanes = useCallback((rows: TmuxSessionRow[], seq: number) => {
    setPanes({});
    for (const row of rows) {
      send({ type: 'tmux_pane_snapshot', session: row.name, lines: TMUX_PREVIEW_LINES }, commandOptions)
        .then((response) => {
          const parsed = response.success ? parseTmuxPaneSnapshot(response.data) : null;
          if (seq === boardSeq.current) setPanes((prev) => ({ ...prev, [row.name]: parsed }));
        })
        .catch(() => {
          if (seq === boardSeq.current) setPanes((prev) => ({ ...prev, [row.name]: null }));
        });
    }
  }, [send, commandOptions]);

  const refreshList = useCallback(async () => {
    const seq = ++boardSeq.current;
    setListLoading(true);
    try {
      const response = await send({ type: 'tmux_list_sessions' }, commandOptions);
      const parsed = response.success ? parseTmuxSessionList(response.data) : null;
      if (seq === boardSeq.current) {
        if (parsed === null) {
          setListFailed(true);
        } else {
          setList(parsed);
          setListFailed(false);
          fetchPanes(parsed.sessions, seq);
        }
      }
    } catch {
      if (seq === boardSeq.current) setListFailed(true);
    } finally {
      if (seq === boardSeq.current) setListLoading(false);
    }
  }, [send, commandOptions, fetchPanes]);

  useEffect(() => {
    if (sessionId === null) return;
    void refreshList();
  }, [refreshList, sessionId, refreshEpoch]);


  const deleteSession = useCallback(async (name: string) => {
    if (boardBusy) return;
    if (pendingDelete !== name) {
      setPendingDelete(name);
      return;
    }
    setBoardBusy(true);
    setBoardError('');
    try {
      const response = await send({ type: 'tmux_session_delete', session: name }, commandOptions);
      if (!response.success) setBoardError(tmuxErrorText(new Error(response.error ?? 'Delete failed')));
    } catch (error) {
      setBoardError(tmuxErrorText(error));
    } finally {
      setPendingDelete(null);
      setBoardBusy(false);
      await refreshList();
    }
  }, [boardBusy, commandOptions, pendingDelete, refreshList, send]);


  const rows = [...(list?.sessions ?? [])].sort((left, right) => left.name.localeCompare(right.name));
  return (
    <section className="chat-tmux-preview" data-runner-id={runnerId} title={`Tmux sessions on ${safeText(runnerLabel)}`}>
      {boardError && <p className="chat-tmux-preview__status" role="status">{safeText(boardError)}</p>}
      {sessionId === null || (listFailed && list === null) ? (
        <p className="chat-tmux-preview__status">{TMUX_STATUS_UNAVAILABLE}</p>
      ) : listLoading && list === null ? (
        <p className="chat-tmux-preview__status chat-tmux-preview__status--placeholder" aria-hidden="true">--</p>
      ) : list && !list.tmuxAvailable ? (
        <p className="chat-tmux-preview__status">{TMUX_NO_SERVER}</p>
      ) : rows.length === 0 ? (
        <p className="chat-tmux-preview__status">{TMUX_NO_SESSIONS}</p>
      ) : (
        <div className="chat-tmux-preview__cards">
          {rows.map((row) => {
            const pane = panes[row.name];
            return (
              <div className="chat-tmux-preview__cell" key={row.name} data-session={safeText(row.name)}>
                <div className="chat-tmux-preview__cardhead">
                  <span className="chat-tmux-preview__name">{safeText(row.name)}</span>
                  <span className="chat-tmux-preview__meta">
                    {row.windows === 1 ? '1 window' : `${row.windows} windows`}
                  </span>
                  {row.attached && <span className="chat-tmux-preview__chip">attached</span>}
                  <button
                    className="chat-button chat-button--compact chat-tmux-preview__delete"
                    type="button"
                    aria-label={pendingDelete === row.name ? `Confirm delete ${row.name}` : `Delete session ${row.name}`}
                    title={pendingDelete === row.name ? `Confirm delete ${row.name}` : `Delete session ${row.name}`}
                    disabled={boardBusy}
                    onClick={() => void deleteSession(row.name)}
                  >
                    {pendingDelete === row.name ? 'Delete?' : '×'}
                  </button>
                </div>
                <button
                  className="chat-tmux-preview__card"
                  data-session={safeText(row.name)}
                  type="button"
                  aria-label={`Open terminal ${row.name}`}
                  title="Open terminal"
                  onClick={() => onOpenTerminal(row.name)}
                >
                  {pane === undefined ? (
                    <pre className="chat-tmux-preview__pre chat-tmux-preview__pre--placeholder" aria-hidden="true">--</pre>
                  ) : pane === null ? (
                    <p className="chat-tmux-preview__status">{TMUX_PANE_UNAVAILABLE}</p>
                  ) : (
                    <pre className="chat-tmux-preview__pre"><AnsiText text={pane.content} /></pre>
                  )}
                </button>
              </div>
            );
          })}
        </div>
      )}
    </section>
  );
}
