import { collapseSteeringPreview } from './steeringQueueHelpers';
import { safeText } from './redact';

export function SteeringQueue(props: {
  previews: string[];
  /** Steer/follow_up submissions the backend rejected. Rendered with an
   *  error state and dismissible; never silently drained by a snapshot. */
  failedPreviews?: string[];
  onDismissFailedAt?: (index: number) => void;
  busy: boolean;
  onSendNow: () => void;
  onCancelAt: (index: number) => void;
}) {
  if (props.previews.length === 0 && (props.failedPreviews ?? []).length === 0) return null;
  return (
    <div id="steering-queue" role="region" aria-label="Steering">
      <ul className="steering-queue__list">
        {(props.failedPreviews ?? []).map((preview, index) => (
          <li
            key={`failed:${index}:${preview}`}
            className="steering-queue__row steering-queue__row--failed"
          >
            <span className="steering-queue__label">Failed</span>
            <span className="steering-queue__text" title={preview}>
              {safeText(collapseSteeringPreview(preview))}
            </span>
            <button
              type="button"
              className="steering-queue__remove"
              aria-label="Dismiss failed steering message"
              title="Dismiss (the message was rejected — re-send it manually)"
              onClick={() => props.onDismissFailedAt?.(index)}
            >
              ×
            </button>
          </li>
        ))}
        {props.previews.map((preview, index) => (
          <li key={`${index}:${preview}`} className="steering-queue__row">
            <span className="steering-queue__label">Steering</span>
            <span className="steering-queue__text" title={preview}>
              {safeText(collapseSteeringPreview(preview))}
            </span>
            <button
              type="button"
              className="steering-queue__send"
              disabled={props.busy}
              aria-label="Send all now"
              title="Send all now (sends every queued message now and aborts the turn)"
              onClick={props.onSendNow}
            >
              <span aria-hidden="true">➤</span>
            </button>
            <button
              type="button"
              className="steering-queue__remove"
              disabled={props.busy}
              aria-label="Remove steering message"
              title="Remove from queue"
              onClick={() => props.onCancelAt(index)}
            >
              ×
            </button>
          </li>
        ))}
      </ul>
    </div>
  );
}