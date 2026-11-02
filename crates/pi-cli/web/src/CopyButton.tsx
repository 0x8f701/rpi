// Command/output copy control shared by ToolCard commandView and BashCard.
// Clipboard write is a boolean Promise (true only on actual success). The
// button tracks mount + attempt generation so a stale write, a rapid re-click,
// a payload/kind change, or unmount can never flash the wrong label.

import { useEffect, useRef, useState } from 'react';
import { redactedClipboardText } from './ansi';

export type CopyKind = 'command' | 'output';
type CopyTimerHandle = number | NodeJS.Timeout;
type CopyFlash = 'idle' | 'copied' | 'failed';

export const COPY_FLASH_MS = 1200;

/** Write `text` to the clipboard. Resolves true only after a successful
 *  write; writeText rejection, execCommand throw/false, missing clipboard,
 *  and any fallback-path throw all resolve false — never treated as success.
 *  The execCommand path wraps create/value/append/select/exec/remove in one
 *  try/finally so a throw cannot skip cleanup. */
export async function copyTextToClipboard(text: string): Promise<boolean> {
  const clipboard = typeof navigator !== 'undefined' ? navigator.clipboard : undefined;
  if (clipboard !== undefined && typeof clipboard.writeText === 'function') {
    try {
      await clipboard.writeText(text);
      return true;
    } catch {
      return false;
    }
  }
  if (typeof document === 'undefined') return false;
  let textarea: HTMLTextAreaElement | null = null;
  let copied = false;
  try {
    if (document.body === null) return false;
    textarea = document.createElement('textarea');
    textarea.value = text;
    textarea.setAttribute('readonly', '');
    textarea.style.position = 'fixed';
    textarea.style.left = '-9999px';
    textarea.style.opacity = '0';
    document.body.appendChild(textarea);
    textarea.select();
    copied = document.execCommand('copy') === true;
  } catch {
    copied = false;
  } finally {
    try {
      textarea?.remove();
    } catch {
      /* cleanup must not mask the copy result */
    }
  }
  return copied;
}

export function CopyButton({ className, kind, text }: { className: string; kind: CopyKind; text: string }) {
  const [flash, setFlash] = useState<CopyFlash>('idle');
  const mountedRef = useRef(true);
  const attemptRef = useRef(0);
  const timerRef = useRef<CopyTimerHandle | null>(null);

  const clearTimer = () => {
    if (timerRef.current !== null) {
      globalThis.clearTimeout(timerRef.current);
      timerRef.current = null;
    }
  };

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
      clearTimer();
    };
  }, []);

  useEffect(() => {
    attemptRef.current += 1;
    clearTimer();
    setFlash('idle');
  }, [kind, text]);

  const label = flash === 'copied' ? 'copied' : flash === 'failed' ? 'copy failed' : 'copy';
  const ariaLabel = flash === 'copied'
    ? (kind === 'command' ? 'Command copied' : 'Output copied')
    : flash === 'failed'
      ? 'Copy failed'
      : (kind === 'command' ? 'Copy command' : 'Copy output');

  return (
    <button
      type="button"
      className={className}
      aria-label={ariaLabel}
      aria-live="polite"
      onClick={() => {
        const attempt = ++attemptRef.current;
        clearTimer();
        void copyTextToClipboard(redactedClipboardText(text)).then((ok) => {
          if (!mountedRef.current || attempt !== attemptRef.current) return;
          setFlash(ok ? 'copied' : 'failed');
          timerRef.current = globalThis.setTimeout(() => {
            if (!mountedRef.current || attempt !== attemptRef.current) return;
            setFlash('idle');
            timerRef.current = null;
          }, COPY_FLASH_MS);
        });
      }}
    >
      {label}
    </button>
  );
}
