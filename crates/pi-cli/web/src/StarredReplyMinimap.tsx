// Right-rail index of starred main-session assistant replies. The parent owns
// which replies are starred, what a jump does, and the authoritative toggle;
// this view only lists the previews it is given. Each row exposes TWO separate
// controls: the preview (jump, stamped with the authoritative entry id) and the
// gold star (unstar, a pure toggle-off that never jumps or collapses the rail).
import { useState } from 'react';
import type { StarredReply } from './replyStars';

export type StarredReplyMinimapProps = {
  replies: readonly StarredReply[];
  onJump: (entryId: string) => void;
  onUnstar: (entryId: string) => void;
};

export function StarredReplyMinimap({ replies, onJump, onUnstar }: StarredReplyMinimapProps) {
  const [expanded, setExpanded] = useState(false);
  if (replies.length === 0) return null;

  return (
    <nav
      className={expanded ? 'starred-reply-minimap' : 'starred-reply-minimap is-collapsed'}
      aria-label="Starred replies"
    >
      <button
        type="button"
        className="starred-reply-minimap__expand"
        aria-expanded={expanded}
        aria-label={expanded ? 'Hide starred replies' : 'Show starred replies'}
        onClick={() => setExpanded((open) => !open)}
      >
        <span className="starred-reply-minimap__mark" aria-hidden="true">★</span>
        <span className="starred-reply-minimap__count">{replies.length}</span>
      </button>
      <ol className="starred-reply-minimap__list">
        {replies.map((reply) => {
          const preview = reply.preview.trim();
          const visible = preview === '' ? `Reply ${reply.transcriptIndex + 1}` : preview;
          return (
            <li key={reply.entryId} className="starred-reply-minimap__item">
              <button
                type="button"
                className="starred-reply-minimap__jump"
                data-entry-id={reply.entryId}
                title={visible}
                aria-label={`Jump to ${visible}`}
                onClick={() => {
                  onJump(reply.entryId);
                  setExpanded(false);
                }}
              >
                <span className="starred-reply-minimap__label">{visible}</span>
              </button>
              <button
                type="button"
                className="starred-reply-minimap__unstar"
                title="Unstar reply"
                aria-label={`Unstar ${visible}`}
                onClick={() => onUnstar(reply.entryId)}
              >
                <span aria-hidden="true">×</span>
              </button>
            </li>
          );
        })}
      </ol>
    </nav>
  );
}
