// Loop panel: create, list, and cancel recurring loop tasks (/loop).
//
// Wired to the `loop_create` / `loop_list` / `loop_delete` RPC commands.
// Supervised processes live in the separate Processes panel (/ps).
import { useCallback, useEffect, useRef, useState } from 'react';

const INTERVAL_CHOICES = ['30s', '1m', '5m', '15m', '1h', '4h'] as const;

interface LoopTask {
  id: string;
  interval: string;
  prompt: string;
}

function asLoopTask(data: unknown): LoopTask {
  const raw = (data && typeof data === 'object' ? data : {}) as Record<string, unknown>;
  const intervalSecs = typeof raw.intervalSecs === 'number' && raw.intervalSecs > 0 ? raw.intervalSecs : 0;
  const interval = intervalSecs >= 86400 ? `${Math.floor(intervalSecs / 86400)}d`
    : intervalSecs >= 3600 ? `${Math.floor(intervalSecs / 3600)}h`
    : intervalSecs >= 60 ? `${Math.floor(intervalSecs / 60)}m`
    : intervalSecs > 0 ? `${intervalSecs}s` : '?';
  return {
    id: typeof raw.id === 'string' ? raw.id : '?',
    interval,
    prompt: typeof raw.prompt === 'string' ? raw.prompt.slice(0, 80) : '',
  };
}

interface LoopPanelProps {
  sendCommand: (command: Record<string, unknown>) => Promise<unknown>;
  onClose: () => void;
}

export function LoopPanel({ sendCommand, onClose }: LoopPanelProps) {
  const [loops, setLoops] = useState<Array<LoopTask>>([]);
  const [prompt, setPrompt] = useState('');
  const [interval, setIntervalValue] = useState<string>('5m');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const busyRef = useRef(false);

  const refresh = useCallback(async () => {
    try {
      const loopData = await sendCommand({ type: 'loop_list' }) as unknown;
      setLoops(Array.isArray(loopData) ? loopData.map(asLoopTask) : []);
      setError(null);
    } catch {
      setError('Failed to load loops');
    }
  }, [sendCommand]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const create = useCallback(async () => {
    const trimmedPrompt = prompt.trim();
    if (!trimmedPrompt || !interval || busyRef.current) return;
    busyRef.current = true;
    setBusy(true);
    setError(null);
    try {
      await sendCommand({
        type: 'loop_create',
        interval,
        prompt: trimmedPrompt,
        fireImmediately: true,
        durable: false,
      });
      setPrompt((current) => current.trim() === trimmedPrompt ? '' : current);
      await refresh();
    } catch (err) {
      setError(err instanceof Error ? err.message : 'loop create failed');
    } finally {
      busyRef.current = false;
      setBusy(false);
    }
  }, [prompt, interval, sendCommand, refresh]);

  const cancelLoop = useCallback(async (taskId: string) => {
    if (busyRef.current) return;
    busyRef.current = true;
    setBusy(true);
    setError(null);
    try {
      await sendCommand({ type: 'loop_delete', taskId });
      await refresh();
    } catch (err) {
      setError(err instanceof Error ? err.message : 'loop delete failed');
    } finally {
      busyRef.current = false;
      setBusy(false);
    }
  }, [sendCommand, refresh]);

  return (
    <section id="loop-panel" className="panel" aria-label="Loop panel">
      <div className="panel__head">
        <span className="panel__title">Loop</span>
        <span className="panel__hint">recurring prompts</span>
        <button id="loop-close-btn" type="button" className="panel-close" onClick={onClose} title="Close panel" aria-label="Close loop panel">✕</button>
      </div>
      <div className="panel__body loop-body">
        <section className="loop-create" aria-label="Create a recurring loop">
          <div className="loop-create__title">New loop</div>
          <div className="loop-create__row">
            <select
              id="loop-interval-input"
              value={interval}
              onChange={(e) => setIntervalValue(e.target.value)}
              disabled={busy}
            >
              {INTERVAL_CHOICES.map((choice) => (
                <option key={choice} value={choice}>{choice}</option>
              ))}
            </select>
          </div>
          <div className="loop-create__row">
            <textarea
              id="loop-prompt-input"
              value={prompt}
              placeholder="Prompt to run on each fire…"
              rows={2}
              onChange={(e) => setPrompt(e.target.value)}
              onKeyDown={(e) => { if (e.key === 'Enter' && !e.shiftKey && prompt.trim()) { e.preventDefault(); void create(); } }}
              disabled={busy}
            />
          </div>
          <button
            id="loop-create-btn"
            type="button"
            className="loop-create__btn"
            disabled={busy || !prompt.trim()}
            onClick={() => void create()}
          >
            {busy ? 'Working…' : 'Create loop'}
          </button>
          {error && <div className="loop-error" role="status">{error}</div>}
        </section>
        <section className="loop-list" aria-label="Active loops">
          <div className="loop-list__title">Active loops ({loops.length})</div>
          {loops.length === 0 && <div className="loop-empty">No active loops.</div>}
          {loops.map((loop) => (
            <div key={loop.id} className="loop-row">
              <span className="loop-row__interval">{loop.interval}</span>
              <span className="loop-row__prompt">{loop.prompt}</span>
              <button
                type="button"
                className="loop-row__cancel"
                disabled={busy}
                onClick={() => void cancelLoop(loop.id)}
                title={`Cancel loop ${loop.id}`}
                aria-label={`Cancel loop: ${loop.prompt}`}
              >
                ✕
              </button>
            </div>
          ))}
        </section>
      </div>
    </section>
  );
}