import { useEffect, useRef, useState } from 'react';

// Wire shapes of the `side_chat_list/new/switch/close/prompt` RPC commands
// (crates/pi-cli/src/modes/rpc.rs — SideChatRpcState snapshot).

export interface SideChatEntryWire {
  role: 'user' | 'assistant' | 'tool' | 'system';
  text: string;
  isError: boolean;
  isPartial: boolean;
}

export interface SideChatTabWire {
  name: string;
  streaming: boolean;
  status: string;
  entries: SideChatEntryWire[];
}

export interface SideChatSnapshot {
  active: string;
  maxTabs: number;
  tabs: SideChatTabWire[];
  accepted?: boolean;
  busy?: boolean;
}

interface SideChatPanelProps {
  snapshot: SideChatSnapshot | null;
  onNew: (name: string) => void;
  onSwitch: (name: string) => void;
  onClose: (name?: string) => void;
  onPrompt: (message: string) => void;
  onClosePanel: () => void;
}

/**
 * Side chat drawer: multi-tab list of parallel `/btw` sessions, the active
 * tab's transcript, and a prompt box. Polling is owned by App (an effect
 * while this panel is open); this component is presentational.
 */
export function SideChatPanel({ snapshot, onNew, onSwitch, onClose, onPrompt, onClosePanel }: SideChatPanelProps) {
  const [newTabName, setNewTabName] = useState('');
  const [prompt, setPrompt] = useState('');
  const transcriptRef = useRef<HTMLDivElement>(null);

  const tabs = snapshot?.tabs ?? [];
  const activeName = snapshot?.active ?? '';
  const active = tabs.find((tab) => tab.name === activeName) ?? tabs[0];
  const streaming = active?.streaming ?? false;

  // Keep the active tab's transcript pinned to the bottom.
  useEffect(() => {
    if (transcriptRef.current) {
      transcriptRef.current.scrollTop = transcriptRef.current.scrollHeight;
    }
  }, [active?.name, active?.entries.length, streaming]);

  const submit = () => {
    const text = prompt.trim();
    if (!text || streaming) return;
    onPrompt(text);
    setPrompt('');
  };

  return (
    <section className="panel-drawer" aria-label="Side chat">
      <div className="side-chat">
        <div className="side-chat__head">
          <span className="side-chat__title">Side chat — parallel sessions (rpi /btw)</span>
          <button type="button" className="panel-close" onClick={onClosePanel} title="Close panel" aria-label="Close side chat panel">
            ×
          </button>
        </div>
        <div className="side-chat__tabs" role="tablist" aria-label="Side chat tabs">
          {tabs.map((tab) => (
            <div key={tab.name} className={`side-chat__tab${tab.name === activeName ? ' side-chat__tab--active' : ''}`} role="presentation">
              <button
                type="button"
                role="tab"
                aria-selected={tab.name === activeName}
                className="side-chat__tab-select"
                onClick={() => onSwitch(tab.name)}
                title={tab.status}
              >
                {tab.name}
                {tab.streaming && <span className="side-chat__streaming" title="streaming">●</span>}
              </button>
              <button
                type="button"
                className="side-chat__tab-close"
                aria-label={`close tab ${tab.name}`}
                title={`Close ${tab.name}`}
                onClick={() => onClose(tab.name)}
              >
                ×
              </button>
            </div>
          ))}
          <form
            className="side-chat__new"
            onSubmit={(e) => {
              e.preventDefault();
              const name = newTabName.trim();
              if (name) {
                onNew(name);
                setNewTabName('');
              }
            }}
          >
            <input
              value={newTabName}
              onChange={(e) => setNewTabName(e.target.value)}
              placeholder="new tab name…"
              maxLength={32}
              aria-label="New side-chat tab name"
            />
            <button type="submit" disabled={!newTabName.trim() || tabs.length >= (snapshot?.maxTabs ?? 8)}>
              New
            </button>
          </form>
        </div>
        <div className="side-chat__transcript" ref={transcriptRef} aria-live="polite">
          {!active || active.entries.length === 0 ? (
            <div className="side-chat__empty">
              No messages yet in “{activeName || 'default'}”. Each tab is an isolated parallel agent forked from the main session.
            </div>
          ) : (
            active.entries.map((entry, index) => (
              <div
                key={`${index}-${entry.role}`}
                className={`side-chat__entry side-chat__entry--${entry.role}${entry.isError ? ' side-chat__entry--error' : ''}${entry.isPartial ? ' side-chat__entry--partial' : ''}`}
              >
                <span className="side-chat__role">{entry.role}</span>
                <span className="side-chat__text">{entry.text}</span>
              </div>
            ))
          )}
          {streaming && <div className="side-chat__typing">…</div>}
        </div>
        <div className="side-chat__composer">
          <textarea
            value={prompt}
            onChange={(e) => setPrompt(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter' && !e.shiftKey) {
                e.preventDefault();
                submit();
              }
            }}
            placeholder={streaming ? 'Side agent is busy — wait for the reply…' : 'Message the side agent… (Enter to send)'}
            rows={2}
            aria-label="Side chat prompt"
          />
          <button type="button" onClick={submit} disabled={!prompt.trim() || streaming} title={streaming ? 'side chat is busy' : 'Send side prompt'}>
            Send
          </button>
        </div>
      </div>
    </section>
  );
}
