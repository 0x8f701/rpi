// Tmux panel — per-runner tmux session browser for the chat gear overlay.
//
// ChatApp mounts it only while the focused runner is 'connected'. The panel
// owns its own state: a mount-time tmux_list_sessions, one tmux_pane_snapshot
// (lines: TMUX_PANE_LINES) per selected session, and tmux_send_keys for
// literal text plus the Enter/Esc/Ctrl-C key buttons. All three responses
// validate through the fail-closed parsers in protocol.ts
// (parseTmuxSessionList / parseTmuxPaneSnapshot / parseTmuxSendResult): any
// drift renders a fixed neutral line — never the raw payload — and every
// remote string reaches the DOM through safeText. Initial list and pane loads
// show `--` placeholders, never a 0 or an empty frame. A list refresh keeps
// the last-good rows on screen.

import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import type { RpcResponse } from '../types';
import { safeText } from '../redact';
import {
  parseTmuxPaneSnapshot,
  parseTmuxSendResult,
  parseTmuxSessionList,
  type RunnerCommand,
  type TmuxPaneSnapshotData,
  type TmuxSendKey,
  type TmuxSessionListData,
} from './protocol';

/** Pane history depth: every tmux_pane_snapshot asks for this many lines. */
export const TMUX_PANE_LINES = 120;

/** Remote error text is bounded before it reaches a React text node. */
export const TMUX_ERROR_MAX_CHARS = 240;

/** Fixed neutral copy — the panel never echoes a raw remote payload. */
export const TMUX_STATUS_UNAVAILABLE = 'Tmux status unavailable';
export const TMUX_NO_SERVER = 'No tmux server on this runner';
export const TMUX_NO_SESSIONS = 'No tmux sessions';
export const TMUX_PANE_UNAVAILABLE = 'Tmux pane unavailable';
export const TMUX_SEND_FAILED = 'Send keys failed.';

/** Bounded error copy. A RunnerCommandError message may itself carry remote
 *  text (runners.ts echoes response.error), so bound it here; the render
 *  still passes the result through safeText. */
export function tmuxErrorText(error: unknown): string {
  const text = error instanceof Error && error.message !== '' ? error.message : 'command failed';
  return text.length > TMUX_ERROR_MAX_CHARS ? `${text.slice(0, TMUX_ERROR_MAX_CHARS - 1)}\u2026` : text;
}

export interface TmuxPanelProps {
  runnerId: string;
  runnerLabel: string;
  /** The runner's primary session id (probe.sessionId). The WebSocket
   *  transport stamps every command frame with it after bootstrap — the
   *  backend rejects bare frames with "sessionId is required after
   *  WebSocket bootstrap". Null means the probe is incomplete: the panel
   *  renders the fixed neutral line and fires nothing. */
  sessionId: string | null;
  send: (command: RunnerCommand, options?: { sourceSessionId?: string }) => Promise<RpcResponse>;
  /** Lifted to ChatApp: opens the full-page PTY overlay for this session. */
  onOpenTerminal?: (session: string) => void;
}

export function TmuxPanel({ runnerId, runnerLabel, sessionId, send, onOpenTerminal }: TmuxPanelProps) {
  /** One options object per send: the raw identity rides only on the wire,
   *  never into the DOM. Memoized so its identity (and the callbacks /
   *  mount effect below that capture it) stays stable across renders. */
  const commandOptions = useMemo(() => ({ sourceSessionId: sessionId ?? undefined }), [sessionId]);
  const [list, setList] = useState<TmuxSessionListData | null>(null);
  const [listFailed, setListFailed] = useState(false);
  const [listLoading, setListLoading] = useState(true);
  const [selected, setSelected] = useState<string | null>(null);
  const [snapshot, setSnapshot] = useState<TmuxPaneSnapshotData | null>(null);
  const [snapshotLoading, setSnapshotLoading] = useState(false);
  const [snapshotFailed, setSnapshotFailed] = useState(false);
  const [text, setText] = useState('');
  const [keysError, setKeysError] = useState('');
  const [keysBusy, setKeysBusy] = useState(false);
  const listSeq = useRef(0);
  const snapshotSeq = useRef(0);
  const refreshList = useCallback(async () => {
    const seq = ++listSeq.current;
    setListLoading(true);
    try {
      const response = await send({ type: 'tmux_list_sessions' }, commandOptions);
      const parsed = response.success ? parseTmuxSessionList(response.data) : null;
      if (seq === listSeq.current) {
        if (parsed === null) {
          setListFailed(true);
        } else {
          setList(parsed);
          setListFailed(false);
        }
      }
    } catch {
      if (seq === listSeq.current) {
        setListFailed(true);
      }
    } finally {
      if (seq === listSeq.current) setListLoading(false);
    }
  }, [send, commandOptions]);

  const fetchSnapshot = useCallback(async (session: string) => {
    const seq = ++snapshotSeq.current;
    if (sessionId === null) return;
    setSnapshotLoading(true);
    try {
      const response = await send({ type: 'tmux_pane_snapshot', session, lines: TMUX_PANE_LINES }, commandOptions);
      const parsed = response.success ? parseTmuxPaneSnapshot(response.data) : null;
      if (seq === snapshotSeq.current) {
        if (parsed === null) {
          setSnapshot(null);
          setSnapshotFailed(true);
        } else {
          setSnapshot(parsed);
          setSnapshotFailed(false);
        }
      }
    } catch {
      if (seq === snapshotSeq.current) {
        setSnapshot(null);
        setSnapshotFailed(true);
      }
    } finally {
      if (seq === snapshotSeq.current) setSnapshotLoading(false);
    }
  }, [send, commandOptions, sessionId]);

  // The probe is the transport prerequisite: without the primary session id
  // the WebSocket backend rejects every command frame, so the panel never
  // fires and stays on the fixed neutral line until a remount with a probe.
  useEffect(() => {
    if (sessionId === null) return;
    void refreshList();
  }, [refreshList, sessionId]);


  /** Selecting a row fetches its pane snapshot; clicking it again closes the
   *  viewer (and drops any pending snapshot output via the seq guard). */
  const toggleSession = (name: string) => {
    if (selected === name) {
      // Invalidate any in-flight snapshot AND its finally-guard, or the
      // loading flag would stay stuck true forever after the close.
      snapshotSeq.current += 1;
      setSnapshotLoading(false);
      setSelected(null);
      setSnapshot(null);
      setSnapshotFailed(false);
      setKeysError('');
      return;
    }
    setSelected(name);
    setSnapshot(null);
    setSnapshotFailed(false);
    setKeysError('');
    void fetchSnapshot(name);
  };

  const sendKeys = async (command: RunnerCommand): Promise<boolean> => {
    if (keysBusy || sessionId === null) return false;
    setKeysBusy(true);
    setKeysError('');
    try {
      const response = await send(command, commandOptions);
      const parsed = response.success ? parseTmuxSendResult(response.data) : null;
      if (parsed === null) {
        setKeysError(TMUX_SEND_FAILED);
        return false;
      }
      return true;
    } catch (error) {
      setKeysError(tmuxErrorText(error));
      return false;
    } finally {
      setKeysBusy(false);
    }
  };

  const sendText = () => {
    if (selected === null || keysBusy || text.trim() === '') return;
    const payload = text;
    void sendKeys({ type: 'tmux_send_keys', session: selected, text: payload }).then((ok) => {
      if (ok) setText((current) => current === payload ? '' : current);
    });
  };

  const sendKey = (key: TmuxSendKey) => {
    if (selected === null || keysBusy) return;
    void sendKeys({ type: 'tmux_send_keys', session: selected, key });
  };

  const rows = list?.sessions ?? [];
  return (
    <section className="chat-tmux" data-runner-id={runnerId} title={`Tmux sessions on ${safeText(runnerLabel)}`}>
      <div className="chat-tmux__head">
        <h3 className="chat-tmux__title">Tmux sessions</h3>
        <button
          className="chat-button chat-button--compact"
          type="button"
          aria-label="Refresh tmux sessions"
          disabled={listLoading || sessionId === null}
          onClick={() => {
            void refreshList();
          }}
        >
          Refresh
        </button>
      </div>
      {sessionId === null ? (
        <p className="chat-tmux__status">{TMUX_STATUS_UNAVAILABLE}</p>
      ) : listLoading && list === null ? (
        <p className="chat-tmux__status chat-tmux__status--placeholder" aria-hidden="true">--</p>
      ) : listFailed && list === null ? (
        <p className="chat-tmux__status">{TMUX_STATUS_UNAVAILABLE}</p>
      ) : list && !list.tmuxAvailable ? (
        <p className="chat-tmux__status">{TMUX_NO_SERVER}</p>
      ) : list === null ? (
        <p className="chat-tmux__status chat-tmux__status--placeholder" aria-hidden="true">--</p>
      ) : rows.length === 0 ? (
        <p className="chat-tmux__status">{TMUX_NO_SESSIONS}</p>
      ) : (
        <ul className="chat-tmux__sessions">
          {rows.map((row) => (
            <li key={row.name} className="chat-tmux__item">
              <button
                className="chat-tmux__row"
                type="button"
                data-active={selected === row.name}
                onClick={() => toggleSession(row.name)}
              >
                <span className="chat-tmux__name">{safeText(row.name)}</span>
                <span className="chat-tmux__meta">{row.windows === 1 ? '1 window' : `${row.windows} windows`}</span>
                {row.attached && <span className="chat-tmux__chip">attached</span>}
              </button>
              {onOpenTerminal && (
                <button
                  className="chat-button chat-button--compact chat-tmux__open"
                  type="button"
                  aria-label="Open terminal"
                  title="Open terminal"
                  onClick={() => onOpenTerminal(row.name)}
                >
                  ⤢ Open terminal
                </button>
              )}
            </li>
          ))}
        </ul>
      )}
      {selected !== null && (
        <div className="chat-tmux__viewer" data-session={safeText(selected)}>
          <div className="chat-tmux__panehead">
            <span className="chat-tmux__pane-id">
              {snapshotLoading ? '--' : snapshot ? safeText(snapshot.pane) : '--'}
            </span>
            <span className="chat-tmux__dims">
              {snapshotLoading ? '--' : snapshot ? `${snapshot.width}\u00d7${snapshot.height}` : '--'}
            </span>
            <button
              className="chat-button chat-button--compact"
              type="button"
              aria-label="Refresh tmux pane"
              disabled={snapshotLoading}
              onClick={() => {
                void fetchSnapshot(selected);
              }}
            >
              Refresh
            </button>
          </div>
          {/* Same stale rule as the list: a pane refetch in flight replaces
           *  the last frame with the -- placeholder until it settles. */}
          {snapshotLoading ? (
            <pre className="chat-tmux__pre chat-tmux__pre--placeholder" aria-hidden="true">--</pre>
          ) : snapshotFailed ? (
            <p className="chat-tmux__status">{TMUX_PANE_UNAVAILABLE}</p>
          ) : snapshot === null ? (
            <pre className="chat-tmux__pre chat-tmux__pre--placeholder" aria-hidden="true">--</pre>
          ) : (
            <pre className="chat-tmux__pre">{safeText(snapshot.content)}</pre>
          )}
          <div className="chat-tmux__controls">
            <input
              className="chat-tmux__input"
              type="text"
              placeholder="Send literal text…"
              value={text}
              disabled={keysBusy}
              autoComplete="off"
              spellCheck={false}
              onChange={(event) => setText(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === 'Enter') {
                  event.preventDefault();
                  sendText();
                }
              }}
            />
            <button
              className="chat-button chat-button--compact chat-tmux__send"
              type="button"
              disabled={text.trim() === '' || keysBusy}
              onClick={() => {
                sendText();
              }}
            >
              Send
            </button>
            <button
              className="chat-button chat-button--compact chat-tmux__key"
              type="button"
              aria-label="Enter"
              disabled={keysBusy}
              onClick={() => sendKey('enter')}
            >
              Enter
            </button>
            <button
              className="chat-button chat-button--compact chat-tmux__key"
              type="button"
              aria-label="Esc"
              disabled={keysBusy}
              onClick={() => sendKey('escape')}
            >
              Esc
            </button>
            <button
              className="chat-button chat-button--compact chat-tmux__key"
              type="button"
              aria-label="Ctrl-C"
              disabled={keysBusy}
              onClick={() => sendKey('ctrl_c')}
            >
              Ctrl-C
            </button>
          </div>
          {keysError !== '' && (
            <p className="chat-tmux__error" role="alert">{safeText(keysError)}</p>
          )}
        </div>
      )}
    </section>
  );
}