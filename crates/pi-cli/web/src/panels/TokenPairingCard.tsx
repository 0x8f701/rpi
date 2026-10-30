// Full-screen pairing card for the `token_required` boot hold.
//
// The old hold UX dumped the phone into the whole Settings panel with the
// token field buried in an empty catalog card. This overlay owns recovery:
// one large input, the exact recovery command for the host, and the current
// authority so the user knows WHICH listener they are pairing to. The hold
// re-probe keeps running behind it (a listener restart into tokenless mode
// releases the hold by itself), and submitting a non-empty token routes
// through the same handleTokenChange contract as Settings → Connection.
//
// Fail-closed: nothing here loosens the listener's token requirement — the
// card only hosts the input the boot preflight already demands.
import { memo, useEffect, useRef, useState } from 'react';

export interface TokenPairingCardProps {
  /** The exact listener authority (host:port) the token will be scoped to. */
  authority: string;
  /** Persist a non-empty token for this authority and reconnect. */
  onTokenChange: (token: string) => void;
}

export const TokenPairingCard = memo(function TokenPairingCard({
  authority,
  onTokenChange,
}: TokenPairingCardProps) {
  const [draft, setDraft] = useState('');
  const [showToken, setShowToken] = useState(false);
  const inputRef = useRef<HTMLInputElement | null>(null);
  useEffect(() => {
    inputRef.current?.focus();
  }, []);
  const submit = () => {
    const trimmed = draft.trim();
    if (trimmed) onTokenChange(trimmed);
  };
  return (
    <div className="token-pairing" role="dialog" aria-modal="true" aria-labelledby="token-pairing-title">
      <div className="token-pairing__card">
        <h2 id="token-pairing-title" className="token-pairing__title">Connect to this listener</h2>
        <p className="token-pairing__authority" aria-label={`Listener authority: ${authority}`}>
          <span className="token-pairing__authority-label">Listener</span>
          <span className="token-pairing__authority-value">{authority}</span>
        </p>
        <label className="token-pairing__label" htmlFor="token-pairing-input">
          Access token
        </label>
        <input
          id="token-pairing-input"
          ref={inputRef}
          type={showToken ? 'text' : 'password'}
          value={draft}
          autoComplete="off"
          autoCorrect="off"
          autoCapitalize="off"
          spellCheck={false}
          placeholder="paste the listener token"
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Enter') submit();
          }}
        />
        <button
          type="button"
          className="token-pairing__reveal"
          onClick={() => setShowToken((v) => !v)}
          aria-pressed={showToken}
          title={showToken ? 'Hide token' : 'Show token'}
          aria-label={showToken ? 'Hide token' : 'Show token'}
        >
          {showToken ? 'Hide' : 'Show'}
        </button>
        <button
          type="button"
          className="token-pairing__connect"
          disabled={draft.trim() === ''}
          onClick={submit}
        >
          Connect
        </button>
        <div className="token-pairing__hint">
          <p>
            On the host running this rpi listener, print the token file passed
            to <code>--listen-token-file</code> and paste it above:
          </p>
          <pre>cat &lt;path-from---listen-token-file&gt;</pre>
          <p>
            The token is saved for this listener only. This page keeps checking
            the listener automatically — if it restarts without a token, the
            connection resumes on its own.
          </p>
        </div>
      </div>
    </div>
  );
});