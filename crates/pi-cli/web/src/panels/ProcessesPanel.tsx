// Processes panel: spawn, inspect, and stop supervised processes (/ps).
//
// Wired to the `process_spawn` / `process_list` / `process_logs` /
// `process_stop` RPC commands. Recurring loops live in the separate
// Loop panel (/loop).
import { useCallback, useEffect, useRef, useState } from 'react';

interface ProcessInfoWire {
  id: string;
  label?: string;
  state: string;
  pid?: number;
  exitCode?: number;
}

function asProcessInfo(data: unknown): ProcessInfoWire | null {
  const raw = (data && typeof data === 'object' ? data : {}) as Record<string, unknown>;
  const id = typeof raw.id === 'string' || typeof raw.id === 'number' ? String(raw.id) : null;
  if (!id) return null;
  return {
    id,
    label: typeof raw.label === 'string' ? raw.label : undefined,
    state: typeof raw.state === 'string' ? raw.state : 'unknown',
    pid: typeof raw.pid === 'number' ? raw.pid : undefined,
    exitCode: typeof raw.exitCode === 'number' ? raw.exitCode : undefined,
  };
}

function processDisplayName(proc: ProcessInfoWire): string {
  return proc.label ?? proc.id;
}

interface ProcessesPanelProps {
  sendCommand: (command: Record<string, unknown>) => Promise<unknown>;
  onClose: () => void;
  /** Session working directory: spawn cwd for supervised processes. */
  workspaceCwd: string;
}

export function ProcessesPanel({ sendCommand, onClose, workspaceCwd }: ProcessesPanelProps) {
  const [processes, setProcesses] = useState<Array<ProcessInfoWire>>([]);
  const [spawnCommand, setSpawnCommand] = useState('');
  const [spawnLabel, setSpawnLabel] = useState('');
  const [openLogsId, setOpenLogsId] = useState<string | null>(null);
  const [logsText, setLogsText] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // Exclusive lock for spawn/stop: React `busy` is presentation only, so a
  // same-render double click cannot fan two process_spawn commands.
  const busyRef = useRef(false);

  const refresh = useCallback(async () => {
    try {
      const procData = await sendCommand({ type: 'process_list' }) as unknown;
      const list = Array.isArray(procData)
        ? procData.map(asProcessInfo).filter((p): p is ProcessInfoWire => p !== null)
        : [];
      setProcesses(list);
      setOpenLogsId((open) => (open !== null && list.some((p) => p.id === open) ? open : null));
      setError(null);
    } catch {
      setError('Failed to load processes');
    }
  }, [sendCommand]);

  const loadLogs = useCallback(async (processId: string) => {
    try {
      const data = await sendCommand({ type: 'process_logs', processId, limitBytes: 16384 }) as {
        chunks?: Array<{ dataBase64?: string }>;
        lost?: boolean;
        lostBytes?: number;
      } | null;
      const chunks = Array.isArray(data?.chunks) ? data.chunks : [];
      const byteArrays = chunks.map((chunk) => {
        if (typeof chunk.dataBase64 !== 'string') return new Uint8Array(0);
        const binary = atob(chunk.dataBase64);
        const bytes = new Uint8Array(binary.length);
        for (let i = 0; i < binary.length; i += 1) bytes[i] = binary.charCodeAt(i);
        return bytes;
      });
      const total = byteArrays.reduce((sum, bytes) => sum + bytes.length, 0);
      const merged = new Uint8Array(total);
      let offset = 0;
      for (const bytes of byteArrays) {
        merged.set(bytes, offset);
        offset += bytes.length;
      }
      const text = total > 0 ? new TextDecoder().decode(merged) : '';
      const lostPrefix = data?.lost === true && typeof data.lostBytes === 'number' && data.lostBytes > 0
        ? `(lost ${data.lostBytes} bytes)\n`
        : '';
      setLogsText(text.length > 0 ? lostPrefix + text : lostPrefix || '(no output)');
    } catch (err) {
      setLogsText(`(failed to load output: ${err instanceof Error ? err.message : 'unknown'})`);
    }
  }, [sendCommand]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // Poll while mounted so exit states and open output tails stay current.
  useEffect(() => {
    const timer = window.setInterval(() => {
      if (busy || busyRef.current) return;
      void refresh();
      if (openLogsId !== null) void loadLogs(openLogsId);
    }, 3000);
    return () => window.clearInterval(timer);
  }, [refresh, busy, openLogsId, loadLogs]);

  const toggleLogs = useCallback(async (processId: string) => {
    if (openLogsId === processId) {
      setOpenLogsId(null);
      return;
    }
    setOpenLogsId(processId);
    setLogsText('');
    await loadLogs(processId);
  }, [openLogsId, loadLogs]);

  const spawnProcess = useCallback(async () => {
    const trimmed = spawnCommand.trim();
    const rawLabel = spawnLabel;
    const cwd = workspaceCwd.trim();
    if (!trimmed || busyRef.current) return;
    if (!cwd) {
      setError('Session working directory unknown — cannot spawn.');
      return;
    }
    busyRef.current = true;
    setBusy(true);
    setError(null);
    try {
      await sendCommand({
        type: 'process_spawn',
        spec: {
          argv: ['bash', '-c', trimmed],
          cwd,
          ...(rawLabel.trim() ? { label: rawLabel.trim() } : {}),
        },
      });
      setSpawnCommand((current) => current.trim() === trimmed ? '' : current);
      setSpawnLabel((current) => current === rawLabel ? '' : current);
      await refresh();
    } catch (err) {
      setError(err instanceof Error ? err.message : 'process spawn failed');
    } finally {
      busyRef.current = false;
      setBusy(false);
    }
  }, [spawnCommand, spawnLabel, sendCommand, refresh, workspaceCwd]);

  const stopProcess = useCallback(async (processId: string) => {
    if (busyRef.current) return;
    busyRef.current = true;
    setBusy(true);
    setError(null);
    try {
      await sendCommand({ type: 'process_stop', processId });
      await refresh();
      if (openLogsId === processId) await loadLogs(processId);
    } catch (err) {
      setError(err instanceof Error ? err.message : 'process stop failed');
    } finally {
      busyRef.current = false;
      setBusy(false);
    }
  }, [sendCommand, refresh, openLogsId, loadLogs]);

  return (
    <section id="processes-panel" className="panel" aria-label="Processes panel">
      <div className="panel__head">
        <span className="panel__title">Processes</span>
        <span className="panel__hint">supervised commands</span>
        <button id="processes-close-btn" type="button" className="panel-close" onClick={onClose} title="Close panel" aria-label="Close processes panel">✕</button>
      </div>
      <div className="panel__body processes-body">
        <section className="processes-spawn" aria-label="Spawn a process">
          <div className="loop-list__title">New process</div>
          <input
            id="processes-spawn-command"
            type="text"
            value={spawnCommand}
            placeholder="command to run (bash -c …)"
            autoComplete="off"
            spellCheck={false}
            onChange={(e) => setSpawnCommand(e.target.value)}
            onKeyDown={(e) => { if (e.key === 'Enter' && spawnCommand.trim()) void spawnProcess(); }}
            disabled={busy}
          />
          <input
            id="processes-spawn-label"
            type="text"
            value={spawnLabel}
            placeholder="label (optional)"
            autoComplete="off"
            spellCheck={false}
            onChange={(e) => setSpawnLabel(e.target.value)}
            onKeyDown={(e) => { if (e.key === 'Enter' && spawnCommand.trim()) void spawnProcess(); }}
            disabled={busy}
          />
          <button
            id="processes-spawn-btn"
            type="button"
            className="loop-create__btn"
            disabled={busy || !spawnCommand.trim()}
            onClick={() => void spawnProcess()}
          >
            Run
          </button>
          {error && <div className="loop-error" role="status">{error}</div>}
        </section>
        <section className="loop-list" aria-label="Supervised processes">
          <div className="loop-list__title">Processes ({processes.length})</div>
          {processes.length === 0 && <div className="loop-empty">No supervised processes.</div>}
          {processes.map((proc) => (
            <div key={proc.id} className="process-block">
              <div className="process-row">
                <span className="process-row__state" data-state={proc.state}>{proc.state}</span>
                <span className="process-row__label">{processDisplayName(proc)}</span>
                {proc.pid != null && <span className="process-row__pid">pid {proc.pid}</span>}
                {proc.exitCode != null && <span className="process-row__pid">exit {proc.exitCode}</span>}
                <button
                  type="button"
                  className="process-row__logs"
                  disabled={busy}
                  onClick={() => void toggleLogs(proc.id)}
                  title={`Output of ${processDisplayName(proc)}`}
                  aria-label={`Toggle output: ${processDisplayName(proc)}`}
                >
                  {openLogsId === proc.id ? 'Hide' : 'Output'}
                </button>
                {(proc.state === 'running' || proc.state === 'starting') && (
                  <button
                    type="button"
                    className="process-row__stop"
                    disabled={busy}
                    onClick={() => void stopProcess(proc.id)}
                    title={`Stop ${processDisplayName(proc)}`}
                    aria-label={`Stop process: ${processDisplayName(proc)}`}
                  >
                    Stop
                  </button>
                )}
              </div>
              {openLogsId === proc.id && (
                <pre id="process-logs" className="process-logs">{logsText}</pre>
              )}
            </div>
          ))}
        </section>
      </div>
    </section>
  );
}