import type { ReactElement } from 'react';
import type { InviteRunnerCandidate } from './InviteRunnerDialog';
import { runnerDisplayName } from './runners';
import {
  CHAT_WORKSPACE_EMPTY_BODY,
  CHAT_WORKSPACE_EMPTY_TITLE,
} from './chatEmpty';
export function ChatEmptyState(props: {
  onAction: () => void;
  lanCandidates?: readonly InviteRunnerCandidate[];
  lanBusyKey?: string | null;
  onAddLan?: (candidate: InviteRunnerCandidate) => void;
}): ReactElement {
  const lan = props.lanCandidates ?? [];
  return (
    <section className="chat-empty chat-empty--hero" id="chat-empty-workspace">
      <p className="chat-empty__kicker">LAN Chat</p>
      <h2 className="chat-empty__title">{CHAT_WORKSPACE_EMPTY_TITLE}</h2>
      <p className="chat-empty__body">{CHAT_WORKSPACE_EMPTY_BODY}</p>
      <button
        className="chat-button chat-button--primary"
        id="chat-empty-new-project"
        type="button"
        onClick={props.onAction}
      >
        New project
      </button>
      {lan.length > 0 && (
        <div className="chat-empty__lan" aria-label="Discovered on LAN">
          <span className="chat-workspace-label">Discovered on LAN</span>
          {lan.map((candidate) => (
            <div className="chat-rail-lan__row" key={candidate.key}>
              <span className="chat-status-dot" data-tone={candidate.state === 'online' ? 'ok' : 'muted'} aria-hidden="true" />
              <span className="chat-rail-lan__name">
                {runnerDisplayName({ codename: candidate.codename, label: candidate.label, authority: candidate.address })}
              </span>
              {candidate.state === 'offline' && candidate.address.startsWith('https://') && (
                <a
                  className="chat-button chat-button--compact"
                  href={`${candidate.address}/chat`}
                  target="_blank"
                  rel="noopener noreferrer"
                >
                  Trust
                </a>
              )}
              <button
                className="chat-button chat-button--compact chat-button--primary"
                type="button"
                disabled={props.lanBusyKey !== null && props.lanBusyKey !== undefined}
                onClick={() => props.onAddLan?.(candidate)}
              >
                {props.lanBusyKey === candidate.key ? 'Connecting…' : 'Add'}
              </button>
            </div>
          ))}
        </div>
      )}
    </section>
  );
}
