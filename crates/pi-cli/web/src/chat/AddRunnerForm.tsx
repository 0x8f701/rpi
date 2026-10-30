import { useEffect, useRef, useState } from 'react';
import { parseRunnerInviteLink } from './auth';

export interface AddRunnerFormProps {
  placement: 'lobby' | 'overlay';
  busyKey: string | null;
  error: string;
  onError: (message: string) => void;
  onAdd: (input: { label: string; address: string; token: string }) => Promise<boolean>;
  onClose: () => void;
}

export function AddRunnerForm({ placement, busyKey, error, onError, onAdd, onClose }: AddRunnerFormProps) {
  const [label, setLabel] = useState('');
  const [address, setAddress] = useState('');
  const [token, setToken] = useState('');
  const [failedOrigin, setFailedOrigin] = useState('');
  const [pendingTrustRetry, setPendingTrustRetry] = useState(false);
  const dialogRef = useRef<HTMLElement | null>(null);
  const overlay = placement === 'overlay';
  const busy = busyKey !== null;

  useEffect(() => {
    if (!overlay) return;
    const first = dialogRef.current?.querySelector<HTMLElement>('button, input');
    first?.focus();
  }, [overlay]);
  const submitAdd = async () => {
    if (busy) return;
    let parsed;
    try {
      parsed = parseRunnerInviteLink(address);
    } catch (parseError) {
      onError(parseError instanceof Error ? parseError.message : 'Runner address is required.');
      return;
    }
    onError('');
    const added = await onAdd({
      label,
      address: parsed.endpoint.origin,
      token: token.trim() || parsed.token,
    });
    if (!added) {
      setFailedOrigin(parsed.endpoint.origin.startsWith('https://') ? parsed.endpoint.origin : '');
      return;
    }
    setFailedOrigin('');
    setPendingTrustRetry(false);
    setLabel('');
    setAddress('');
    setToken('');
  };
  // Certificates are accepted in a separate tab: when the user comes back,
  // retry the add immediately instead of making them find the button again.
  useEffect(() => {
    if (!pendingTrustRetry) return;
    const onVisible = () => {
      if (document.visibilityState !== 'visible') return;
      setPendingTrustRetry(false);
      void submitAdd();
    };
    document.addEventListener('visibilitychange', onVisible);
    window.addEventListener('focus', onVisible);
    return () => {
      document.removeEventListener('visibilitychange', onVisible);
      window.removeEventListener('focus', onVisible);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [pendingTrustRetry, address, label, token]);

  return (
    <section
      ref={dialogRef}
      className={`chat-panel chat-panel--form chat-invite chat-invite--${placement}`}
      id="chat-add-runner"
      aria-labelledby={overlay ? 'chat-add-runner-title' : undefined}
      role={overlay ? 'dialog' : undefined}
      aria-modal={overlay ? true : undefined}
    >
      {overlay && (
        <div className="chat-panel__head">
          <h2 id="chat-add-runner-title">Add runner</h2>
          <button
            className="chat-button chat-button--quiet chat-panel__close"
            type="button"
            aria-label="Close add runner"
            title="Close"
            onClick={onClose}
          >
            ×
          </button>
        </div>
      )}
      {!overlay && <h3>Add runner</h3>}
      <form className="chat-runner-form" id="chat-add-runner-form" onSubmit={(event) => { event.preventDefault(); void submitAdd(); }}>
        <label className="chat-field">
          Label
          <input
            id="chat-add-runner-label"
            value={label}
            onChange={(event) => setLabel(event.target.value)}
            placeholder="Build runner"
            maxLength={80}
          />
        </label>
        <label className="chat-field">
          Runner link
          <input
            id="chat-add-runner-address"
            value={address}
            onChange={(event) => setAddress(event.target.value)}
            placeholder="https://192.0.2.10:8765/chat#token"
            inputMode="url"
            required
          />
        </label>
        {token !== '' && (
          <label className="chat-field chat-field--wide">
            Token
            <input
              id="chat-add-runner-token"
              value={token}
              onChange={(event) => setToken(event.target.value)}
              type="password"
              autoComplete="off"
            />
          </label>
        )}
        <button className="chat-button chat-button--primary" id="chat-runner-add" type="submit" disabled={busy}>
          {busy ? 'Connecting…' : 'Add runner'}
        </button>
      </form>
      {failedOrigin !== '' && error !== '' && (
        <a
          className="chat-button chat-button--compact"
          href={`${failedOrigin}/chat`}
          target="_blank"
          rel="noopener noreferrer"
          onClick={() => setPendingTrustRetry(true)}
        >
          Open to trust
        </a>
      )}
    </section>
  );
}