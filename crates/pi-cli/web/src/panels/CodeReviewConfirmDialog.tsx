// Presentational confirm overlay for guarded close/refresh in the
// code-review workspace. Parent owns confirmAction state + focus restore.

export interface CodeReviewConfirmDialogProps {
  message: string;
  primaryLabel: string;
  onConfirm: () => void;
  onCancel: () => void;
}

/** Inline alertdialog: primary action first so focus lands on it. */
export function CodeReviewConfirmDialog({
  message,
  primaryLabel,
  onConfirm,
  onCancel,
}: CodeReviewConfirmDialogProps) {
  return (
    <div className="code-review__confirm-backdrop">
      <div className="code-review__confirm" role="alertdialog" aria-label="Confirm action">
        <p>{message}</p>
        <div className="code-review__confirm-actions">
          <button type="button" className="code-review__action" onClick={onConfirm}>
            {primaryLabel}
          </button>
          <button type="button" className="code-review__action" onClick={onCancel}>
            Keep editing
          </button>
        </div>
      </div>
    </div>
  );
}
