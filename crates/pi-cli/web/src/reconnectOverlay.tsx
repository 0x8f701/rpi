import { RECONNECT_OVERLAY_TEXT } from './reconnect';

export function ReconnectOverlay(props: { visible: boolean; text?: string }): JSX.Element | null {
  if (!props.visible) return null;
  return (
    <div
      id="reconnect-overlay"
      className="reconnect-overlay"
      role="alert"
      aria-live="assertive"
      aria-busy="true"
    >
      <div className="reconnect-overlay__panel">
        <span className="reconnect-overlay__spinner" aria-hidden="true" />
        <p className="reconnect-overlay__text">{props.text ?? RECONNECT_OVERLAY_TEXT}</p>
      </div>
    </div>
  );
}
