import { useEffect, useRef, useState } from 'react';
import { safeText } from '../redact';
import { parseRunnerInviteLink, RUNNER_UNREACHABLE_MESSAGE } from './auth';
import { CHAT_INVITE_LAN_HINT, CHAT_LAN_NONE } from './chatEmpty';
import type { ProjectRecord } from './mapping';
import { runnerDisplayName } from './runners';

export interface InviteRunnerCandidate {
  key: string;
  runnerId: string | null;
  label: string;
  address: string;
  version: string | null;
  codename: string | null;
  state: 'online' | 'offline' | 'connecting' | 'token_required' | 'invited';
  error: string;
}

export interface InviteRunnerDialogProps {
  placement: 'lobby' | 'overlay';
  project: ProjectRecord;
  candidates: readonly InviteRunnerCandidate[];
  busyKey: string | null;
  onError: (message: string) => void;
  onInviteExisting: (runnerId: string) => Promise<boolean>;
  onAddAndInvite: (input: { label: string; address: string; token: string }) => Promise<boolean>;
  onSaveTokenAndInvite: (runnerId: string, token: string) => Promise<boolean>;
  onRemoveRunner: (runnerId: string) => Promise<void>;
  onClose: () => void;
}

function discoveredHttpsOrigin(address: string): boolean {
  try {
    return new URL(address).protocol === 'https:';
  } catch {
    return false;
  }
}

function shouldOfferCertTrust(candidate: InviteRunnerCandidate): boolean {
  if (!discoveredHttpsOrigin(candidate.address)) return false;
  return candidate.state === 'offline' || candidate.state === 'connecting';
}

function CertTrustLink({ address }: { address: string }) {
  return (
    <a
      className="chat-button chat-button--compact"
      href={`${address}/chat`}
      target="_blank"
      rel="noopener noreferrer"
    >
      Open to trust
    </a>
  );
}

function lanDiscoverAddPayload(candidate: InviteRunnerCandidate): { label: string; address: string; token: string } {
  return { label: candidate.codename || candidate.label, address: candidate.address, token: '' };
}

function candidateTone(state: InviteRunnerCandidate['state']): string {
  switch (state) {
    case 'online':
      return 'ok';
    case 'token_required':
      return 'error';
    case 'connecting':
      return 'warn';
    default:
      return 'muted';
  }
}

export function InviteRunnerDialog({
  placement,
  project,
  candidates,
  busyKey,
  onError,
  onInviteExisting,
  onAddAndInvite,
  onSaveTokenAndInvite,
  onRemoveRunner,
  onClose,
}: InviteRunnerDialogProps) {
  const [label, setLabel] = useState('');
  const [address, setAddress] = useState('');
  const [token, setToken] = useState('');
  const [tokenTarget, setTokenTarget] = useState<InviteRunnerCandidate | null>(null);
  const dialogRef = useRef<HTMLElement | null>(null);
  const overlay = placement === 'overlay';

  useEffect(() => {
    if (!overlay) return;
    const first = dialogRef.current?.querySelector<HTMLElement>('button, input');
    first?.focus();
  }, [overlay]);
  useEffect(() => {
    setLabel('');
    setAddress('');
    setToken('');
    setTokenTarget(null);
  }, [project.id]);
  const busy = busyKey !== null;

  const submitAdd = async (event: React.FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    if (busy) return;
    if (tokenTarget) {
      const saved = tokenTarget.runnerId
        ? await onSaveTokenAndInvite(tokenTarget.runnerId, token)
        : await onAddAndInvite({ label: tokenTarget.label, address: tokenTarget.address, token });
      if (!saved) return;
      setToken('');
      setTokenTarget(null);
      return;
    }
    let parsed;
    try {
      parsed = parseRunnerInviteLink(address);
    } catch (parseError) {
      onError(parseError instanceof Error ? parseError.message : 'Runner address is required.');
      return;
    }
    onError('');
    const added = await onAddAndInvite({
      label,
      address: parsed.endpoint.origin,
      token: token.trim() || parsed.token,
    });
    if (!added) return;
    setLabel('');
    setAddress('');
    setToken('');
  };

  return (
    <section
      ref={dialogRef}
      className={`chat-panel chat-panel--form chat-invite chat-invite--${placement}`}
      id="chat-invite"
      aria-labelledby={overlay ? 'chat-invite-title' : 'chat-empty-project-title'}
      role={overlay ? 'dialog' : undefined}
      aria-modal={overlay ? true : undefined}
    >
      {(overlay || tokenTarget) && (
        <div className="chat-panel__head">
          <h2 id="chat-invite-title">{tokenTarget ? 'Token required' : `Add runner · ${safeText(project.label)}`}</h2>
          {tokenTarget && !overlay && (
            <button
              className="chat-button chat-button--quiet"
              type="button"
              onClick={() => {
                setTokenTarget(null);
                setToken('');
              }}
            >
              Back
            </button>
          )}
          {overlay && (
            <button
              className="chat-button chat-button--quiet chat-panel__close"
              type="button"
              aria-label="Close add runner"
              title="Close"
              onClick={onClose}
            >
              ×
            </button>
          )}
        </div>
      )}
      <div className="chat-invite__lan" aria-labelledby="chat-lan-title">
        <h3 id="chat-lan-title">Discovered on LAN</h3>
        <p className="chat-invite__hint">{CHAT_INVITE_LAN_HINT}</p>
        {candidates.length === 0 && <p className="chat-empty">{CHAT_LAN_NONE}</p>}
        {candidates.map((candidate) => (
          <article className={`chat-machine-card chat-machine-card--lan${candidate.state === 'invited' ? ' is-added' : ''}`} key={candidate.key}>
            <div className="chat-machine-card__body">
              <span className="chat-status-dot" data-tone={candidateTone(candidate.state)} aria-hidden="true" />
              <div className="chat-machine-card__meta">
                <strong>{safeText(runnerDisplayName({
                  codename: candidate.codename,
                  label: candidate.label,
                  authority: candidate.address,
                }))}</strong>
                <span className="chat-runner-origin">{safeText(candidate.address)}</span>
                {candidate.version && <span className="chat-machine-card__hint">v{safeText(candidate.version)}</span>}
                {candidate.state !== 'invited' && candidate.error !== '' && candidate.error !== RUNNER_UNREACHABLE_MESSAGE && (
                  <span className="chat-runner-error">{safeText(candidate.error)}</span>
                )}
              </div>
            </div>
            {candidate.state === 'invited' && <span className="chat-added-pill">Added</span>}
            {candidate.state === 'online' && (
              <button
                className="chat-button chat-button--compact chat-button--primary"
                type="button"
                disabled={busy}
                onClick={() => void (candidate.runnerId
                  ? onInviteExisting(candidate.runnerId)
                  : onAddAndInvite(lanDiscoverAddPayload(candidate)))}
              >
                {busyKey === candidate.key ? 'Connecting…' : 'Add'}
              </button>
            )}
            {candidate.state === 'token_required' && (
              <button
                className="chat-button chat-button--compact"
                type="button"
                disabled={busy}
                onClick={() => {
                  setTokenTarget(candidate);
                  setLabel(candidate.label);
                  setAddress(candidate.address);
                  setToken('');
                }}
              >
                Token required
              </button>
            )}
            {candidate.state === 'offline' && candidate.runnerId === null && (
              <div className="chat-machine-card__actions">
                <span className="chat-added-pill">Offline</span>
                {shouldOfferCertTrust(candidate) && <CertTrustLink address={candidate.address} />}
                <button
                  className="chat-button chat-button--compact chat-button--primary"
                  type="button"
                  disabled={busy}
                  onClick={() => void onAddAndInvite(lanDiscoverAddPayload(candidate))}
                >
                  {busyKey === candidate.key ? 'Connecting…' : 'Add'}
                </button>
              </div>
            )}
            {(candidate.state === 'offline' || candidate.state === 'token_required') && candidate.runnerId !== null && (
              <div className="chat-machine-card__actions">
                {shouldOfferCertTrust(candidate) && <CertTrustLink address={candidate.address} />}
                <button
                  className="chat-button chat-button--compact chat-button--danger"
                  type="button"
                  disabled={busy}
                  onClick={() => void (candidate.runnerId ? onRemoveRunner(candidate.runnerId) : undefined)}
                >
                  {busyKey === `remove:${candidate.key}` ? 'Removing…' : 'Remove'}
                </button>
              </div>
            )}
            {candidate.state === 'connecting' && (
              <div className="chat-machine-card__actions">
                <span className="chat-added-pill">Connecting</span>
                {shouldOfferCertTrust(candidate) && <CertTrustLink address={candidate.address} />}
                {candidate.runnerId === null && (
                  <button
                    className="chat-button chat-button--compact chat-button--primary"
                    type="button"
                    disabled={busy}
                    onClick={() => void onAddAndInvite(lanDiscoverAddPayload(candidate))}
                  >
                    {busyKey === candidate.key ? 'Connecting…' : 'Add'}
                  </button>
                )}
              </div>
            )}
          </article>
        ))}
      </div>
      <h3>Add runner</h3>
      <form className="chat-runner-form" id="chat-invite-form" onSubmit={(event) => void submitAdd(event)}>
        <label className="chat-field">
          Label
          <input
            id="chat-runner-label"
            value={label}
            onChange={(event) => setLabel(event.target.value)}
            placeholder="Build runner"
            maxLength={80}
            disabled={tokenTarget !== null}
          />
        </label>
        <label className="chat-field">
          Runner link
          <input
            id="chat-runner-address"
            value={address}
            onChange={(event) => setAddress(event.target.value)}
            placeholder="https://192.0.2.10:8765/chat#token"
            inputMode="url"
            required={tokenTarget === null}
            disabled={tokenTarget !== null}
          />
        </label>
        {(tokenTarget !== null || token !== '') && (
          <label className="chat-field chat-field--wide">
            Token
            <input
              id="chat-runner-token"
              value={token}
              onChange={(event) => setToken(event.target.value)}
              type="password"
              autoComplete="off"
            />
          </label>
        )}
        <button className="chat-button chat-button--primary" id="chat-runner-add" type="submit" disabled={busy}>
          {busy ? 'Connecting…' : tokenTarget ? 'Save token and reconnect' : 'Add runner'}
        </button>
      </form>
      {tokenTarget && tokenTarget.error !== '' && tokenTarget.error !== RUNNER_UNREACHABLE_MESSAGE && (
        <p className="chat-runner-error" role="status">{safeText(tokenTarget.error)}</p>
      )}
    </section>
  );
}
