// Presentational overview for one host-owned code-review TLDR markdown body.
// Parent owns snapshot identity, generation, request start, and the central
// error toast. This pane never parses headings into business state and never
// invents a blank body while the host still supplies the last approved text.
//
// The body renders through the shared MarkdownBody (escape-first, Mermaid
// hosts, path chips). The pane opens expanded (full scroll area). Collapse
// details applies the bounded CSS viewport; Expand details restores it
// without splitting the string. File links stay with the document markdown
// owner (`data-session-id` ancestor); this pane does not take an open-file callback.

import { useCallback, useState } from 'react';
import { MarkdownBody } from '../MarkdownBody';

export type CodeReviewTldrState = 'idle' | 'running' | 'done' | 'failed';

export interface CodeReviewTldrPaneProps {
  /** One markdown document: overview then details. Not split by headings. */
  markdown: string;
  state: CodeReviewTldrState;
  /** Session stamped as the markdown path owner. Empty leaves chips inert. */
  sessionId?: string;
  onStop: () => void;
  onClose: () => void;
  /** Fires after Markdown/Mermaid/image layout mutations. */
  onLayoutChange?: () => void;
}

const STATE_LABEL: Record<CodeReviewTldrState, string> = {
  idle: 'Idle',
  running: 'Generating',
  done: 'Ready',
  failed: 'Failed',
};

/** One markdown body that opens expanded, with one collapse/expand toggle. */
export function CodeReviewTldrPane({
  markdown,
  state,
  sessionId = '',
  onStop,
  onClose,
  onLayoutChange,
}: CodeReviewTldrPaneProps) {
  const [expanded, setExpanded] = useState(true);
  const running = state === 'running';
  const toggle = useCallback(() => {
    setExpanded((value) => !value);
  }, []);

  return (
    <section
      className={`code-review-tldr code-review-tldr--${state}`}
      aria-label="Code review summary"
      aria-busy={running}
      data-tldr-state={state}
      data-session-id={sessionId}
    >
      <div className="code-review-tldr__head">
        <span className="code-review-tldr__title">Summary</span>
        <span className="code-review-tldr__status" role="status" aria-live="polite">
          {running ? (
            <span className="code-review__spinner" aria-hidden="true" />
          ) : null}
          {STATE_LABEL[state]}
        </span>
        <div className="code-review-tldr__actions">
          {running ? (
            <button
              type="button"
              className="code-review__action code-review__action--stop"
              onClick={onStop}
              title="Stop generating summary"
              aria-label="Stop generating summary"
            >
              <span aria-hidden="true">■</span>
              Stop
            </button>
          ) : null}
          <button
            type="button"
            className="code-review__close panel-close"
            onClick={onClose}
            title="Close summary"
            aria-label="Close summary"
          >
            ✕
          </button>
        </div>
      </div>
      <div
        className={`code-review-tldr__viewport${expanded ? ' is-expanded' : ''}`}
        data-expanded={expanded ? 'true' : 'false'}
      >
        <MarkdownBody
          className="code-review-tldr__body"
          text={markdown}
          onLayoutChange={onLayoutChange}
        />
      </div>
      <button
        type="button"
        className="code-review-tldr__toggle"
        aria-expanded={expanded}
        onClick={toggle}
      >
        {expanded ? 'Collapse details' : 'Expand details'}
      </button>
    </section>
  );
}
