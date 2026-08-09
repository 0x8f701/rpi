import { useState } from 'react';

// Wire shapes of the maintenance RPC commands: compact/snapcompact (A→B
// token report), get_entries + rewind, handoff, queue_list/queue_cancel.

interface RewindEntry {
  index: number;
  type: string;
  preview: string;
}

interface QueueView {
  steering: string[];
  followUp: string[];
  total: number;
}

interface MaintenancePanelProps {
  /** sendCommand bound to the App websocket (resolves with the RPC data). */
  rpc: (command: Record<string, unknown>) => Promise<unknown>;
  onClosePanel: () => void;
}

function entryPreview(entry: unknown): string {
  const e = (entry || {}) as {
    type?: string;
    message?: { content?: Array<{ type?: string; text?: string }> };
    content?: unknown;
    summary?: string;
    customType?: string;
  };
  const fromMessage =
    Array.isArray(e.message?.content)
      ? e.message.content
          .filter((b) => b && b.type === 'text' && typeof b.text === 'string')
          .map((b) => b.text as string)
          .join(' ')
          .trim()
      : '';
  if (fromMessage) return fromMessage;
  if (typeof e.content === 'string' && e.content.trim()) return e.content;
  if (typeof e.summary === 'string' && e.summary.trim()) return e.summary;
  return `[${e.type ?? 'entry'}]`;
}

function tokenReport(data: unknown, label: string): string {
  const d = (data || {}) as {
    tokensBefore?: number;
    estimatedTokensAfter?: number;
    firstKeptEntryId?: string;
  };
  const before = d.tokensBefore;
  const after = d.estimatedTokensAfter;
  if (typeof before !== 'number') return `${label}: no token report in response`;
  const afterText = typeof after === 'number' ? String(after) : '?';
  return `${label}: ${before} → ${afterText} estimated tokens${typeof after === 'number' && after < before ? ' (shrank)' : ''}`;
}

interface ResultView {
  title: string;
  text: string;
  error?: boolean;
}

/** Maintenance actions row: compact (A→B report), rewind, handoff, queue. */
export function MaintenancePanel({ rpc, onClosePanel }: MaintenancePanelProps) {
  const [busy, setBusy] = useState<string | null>(null);
  const [result, setResult] = useState<ResultView | null>(null);
  const [rewindEntries, setRewindEntries] = useState<RewindEntry[] | null>(null);
  const [queue, setQueue] = useState<QueueView | null>(null);
  const [handoff, setHandoff] = useState<string | null>(null);

  const run = async (title: string, command: Record<string, unknown>, render: (data: unknown) => string) => {
    setBusy(title);
    setResult(null);
    try {
      const data = await rpc(command);
      setResult({ title, text: render(data) });
    } catch (error) {
      setResult({ title, text: (error as Error).message || String(error), error: true });
    } finally {
      setBusy(null);
    }
  };

  const loadRewindList = () => {
    setBusy('rewind');
    setResult(null);
    rpc({ type: 'get_entries' })
      .then((data) => {
        const entries = ((data as { entries?: unknown[] }).entries || []) as unknown[];
        setRewindEntries(
          entries.map((entry, index) => ({
            index,
            type: String(((entry as { type?: unknown }) || {}).type ?? 'entry'),
            preview: entryPreview(entry),
          }))
        );
      })
      .catch((error: Error) => {
        setResult({ title: 'rewind', text: error.message || String(error), error: true });
      })
      .finally(() => setBusy(null));
  };

  const doRewind = (index: number) => {
    run(
      'rewind',
      { type: 'rewind', index },
      (data) => {
        const d = data as { retainedEntries?: number; droppedEntries?: number; archivePath?: string; checkpoint?: string };
        const where = d.checkpoint ? `checkpoint ${d.checkpoint}` : `entry ${index - 1}`;
        return `Rewound to ${where}: kept ${d.retainedEntries ?? '?'}, dropped ${d.droppedEntries ?? '?'} record(s); archived tail to ${d.archivePath ?? '?'}`;
      }
    );
  };

  const loadQueue = () => {
    setBusy('queue');
    setResult(null);
    rpc({ type: 'queue_list' })
      .then((data) => {
        const d = data as QueueView;
        setQueue({ steering: d.steering || [], followUp: d.followUp || [], total: d.total || 0 });
      })
      .catch((error: Error) => {
        setResult({ title: 'queue', text: error.message || String(error), error: true });
      })
      .finally(() => setBusy(null));
  };

  const cancelQueue = () => {
    run(
      'queue cancel',
      { type: 'queue_cancel' },
      (data) => `Cancelled ${(data as { cancelled?: number }).cancelled ?? 0} queued prompt(s)`
    );
    setQueue(null);
  };

  const generateHandoff = () => {
    setBusy('handoff');
    setResult(null);
    rpc({ type: 'handoff' })
      .then((data) => setHandoff(String((data as { text?: string }).text ?? '')))
      .catch((error: Error) => {
        setResult({ title: 'handoff', text: error.message || String(error), error: true });
      })
      .finally(() => setBusy(null));
  };

  return (
    <section className="panel-drawer" aria-label="Maintenance">
      <div className="maintenance">
        <div className="maintenance__head">
          <span className="maintenance__title">Maintenance</span>
          <button type="button" className="panel-close" onClick={onClosePanel} title="Close panel" aria-label="Close maintenance panel">
            ×
          </button>
        </div>
        <div className="maintenance__row">
          <button type="button" className="maintenance__action" disabled={busy !== null} onClick={() => run('compact', { type: 'compact' }, (d) => tokenReport(d, 'Compact'))} title="Compact the context via the LLM summarizer">
            Compact
          </button>
          <button type="button" className="maintenance__action" disabled={busy !== null} onClick={() => run('snapcompact', { type: 'snapcompact' }, (d) => tokenReport(d, 'Snapcompact'))} title="Deterministic archive compaction, no LLM call">
            Snapcompact
          </button>
          <button type="button" className="maintenance__action" disabled={busy !== null} onClick={loadRewindList} title="List session entries to rewind to">
            Rewind…
          </button>
          <button type="button" className="maintenance__action" disabled={busy !== null} onClick={generateHandoff} title="Render the handoff envelope">
            Handoff
          </button>
          <button type="button" className="maintenance__action" disabled={busy !== null} onClick={loadQueue} title="View queued steering/follow-up prompts">
            Queue…
          </button>
        </div>

        {rewindEntries && (
          <div className="maintenance__block">
            <div className="maintenance__block-title">
              Rewind targets (get_entries) — click an entry to roll back before it
            </div>
            <div className="maintenance__list">
              {rewindEntries.length === 0 && <div className="maintenance__empty">No session records yet.</div>}
              {rewindEntries.map((entry) => (
                <button
                  key={entry.index}
                  type="button"
                  className="maintenance__list-row"
                  disabled={busy !== null}
                  onClick={() => doRewind(entry.index)}
                  title={`rewind to entry ${entry.index}`}
                >
                  <span className="maintenance__list-index">{entry.index}</span>
                  <span className="maintenance__list-type">{entry.type}</span>
                  <span className="maintenance__list-preview">{entry.preview}</span>
                </button>
              ))}
            </div>
          </div>
        )}

        {queue && (
          <div className="maintenance__block">
            <div className="maintenance__block-title">
              Queue ({queue.total} pending) — steering and follow-up prompts waiting for the active run
            </div>
            <div className="maintenance__queue">
              {queue.total === 0 && <div className="maintenance__empty">Queue is empty.</div>}
              {queue.steering.map((text, index) => (
                <div key={`s-${index}`} className="maintenance__queue-row">
                  <span className="maintenance__queue-kind">steer</span> {text}
                </div>
              ))}
              {queue.followUp.map((text, index) => (
                <div key={`f-${index}`} className="maintenance__queue-row">
                  <span className="maintenance__queue-kind">follow-up</span> {text}
                </div>
              ))}
            </div>
            <button type="button" className="maintenance__action maintenance__action--danger" disabled={busy !== null} onClick={cancelQueue} title="Drain and discard every queued prompt">
              Cancel queue
            </button>
          </div>
        )}

        {handoff !== null && (
          <div className="maintenance__block">
            <div className="maintenance__block-title">Handoff envelope</div>
            <pre className="maintenance__handoff">{handoff === '' ? '(empty handoff)' : handoff}</pre>
          </div>
        )}

        {result && (
          <div className={`maintenance__result${result.error ? ' maintenance__result--error' : ''}`}>
            <span className="maintenance__result-title">{result.title}:</span> {result.text}
          </div>
        )}
        {busy && <div className="maintenance__busy">running {busy}…</div>}
      </div>
    </section>
  );
}
