// Full-page tmux PTY overlay for /chat. Header + body + footer match the
// code-review overlay family: the workspace bar and project header stay
// visible; Esc is a terminal key and never detaches.
//
// Wire (docs/tmux-web-terminal-design.md):
//   attach  {session, cols, rows} → {terminalId, session, cols, rows}
//   output  {terminalId, data: base64}  (streamed)
//   input   {terminalId, data: base64}  (fire-and-forget)
//   resize  {terminalId, cols, rows}
//   detach  {terminalId}
//   exit    {terminalId, code}
// Close during an in-flight attach still detaches a late successful
// terminalId so the server 16-terminal cap cannot leak. Close dismisses
// immediately; detach runs in the background and retries once on failure.
// Unmount also attempts best-effort detach for an attached terminal.

import { useCallback, useEffect, useRef, useState, type ChangeEvent, type ClipboardEvent, type DragEvent } from 'react';
import { FitAddon } from '@xterm/addon-fit';
import { Terminal } from '@xterm/xterm';
import type { RpcResponse } from '../types';
import { safeText } from '../redact';
import { RunnerCommandError } from './runners';
import { rememberTmuxFileDestination, uploadTmuxOverlayFile } from '../tmuxFile';
import {
  parseTmuxTerminalAttach,
  parseTmuxTerminalExit,
  parseTmuxTerminalHistory,
  parseTmuxTerminalOutput,
  type RunnerCommand,
} from './protocol';

export function planTmuxOverlayPinTop(opts: {
  visTop: number;
  visBottom: number;
  barBottom: number;
  barHeight: number;
}): number {
  const belowBar = opts.barHeight > 0
    ? Math.max(opts.barBottom, opts.visTop + opts.barHeight)
    : opts.visTop;
  const top = Math.max(opts.visTop, belowBar);
  return Math.min(opts.visBottom, top);
}

export const TMUX_TERMINAL_DETACHED = 'Detached';
export const TMUX_TERMINAL_DETACH_FAILED = 'Detach failed';
export const TMUX_TERMINAL_ATTACH_FAILED = 'Terminal attach failed';
export const TMUX_TERMINAL_FIT_FAILED = 'Fit failed';

export const TMUX_TERMINAL_MIN_HOST_WIDTH = 64;
export const TMUX_TERMINAL_MIN_HOST_HEIGHT = 24;

export function tmuxTerminalHostReady(host: { clientWidth: number; clientHeight: number }): boolean {
  return host.clientWidth >= TMUX_TERMINAL_MIN_HOST_WIDTH && host.clientHeight >= TMUX_TERMINAL_MIN_HOST_HEIGHT;
}

export function tmuxTerminalGeometryReady(cols: number, rows: number): boolean {
  return cols >= 8 && rows >= 4;
}

export interface TmuxTerminalProps {
  runnerId: string;
  runnerLabel: string;
  session: string;
  authority: string;
  send: (command: RunnerCommand, options?: { sourceSessionId?: string }) => Promise<RpcResponse>;
  onEvent: (callback: (frame: unknown) => void) => () => void;
  onClose: () => void;
  onError?: (message: string) => void;
  loadToken?: (authority: string) => string;
  hidden?: boolean;
}

/** UTF-8 text → standard base64 for the PTY input/output frames. */
export function encodeTmuxTerminalData(text: string): string {
  const bytes = new TextEncoder().encode(text);
  let binary = '';
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary);
}

/** Standard base64 → bytes for `term.write`. Null on drift / invalid encoding. */
export function decodeTmuxTerminalData(data: string): Uint8Array | null {
  try {
    const binary = atob(data);
    const bytes = new Uint8Array(binary.length);
    for (let i = 0; i < binary.length; i += 1) bytes[i] = binary.charCodeAt(i);
    return bytes;
  } catch {
    return null;
  }
}

function fileFromDataTransfer(data: DataTransfer | null | undefined): File | undefined {
  if (!data) return undefined;
  const listed = data.files[0];
  if (listed) return listed;
  const items = data.items;
  if (!items) return undefined;
  for (let i = 0; i < items.length; i += 1) {
    const item = items[i];
    if (item && item.kind === 'file') {
      const file = item.getAsFile();
      if (file) return file;
    }
  }
  return undefined;
}


export function TmuxTerminal({ runnerId, runnerLabel, session, authority, send, onEvent, onClose, onError, loadToken, hidden }: TmuxTerminalProps) {
  const overlayRef = useRef<HTMLElement | null>(null);
  const hostRef = useRef<HTMLDivElement | null>(null);
  const termRef = useRef<Terminal | null>(null);
  const fitRef = useRef<FitAddon | null>(null);
  const terminalIdRef = useRef<string | null>(null);
  const outputLinesRef = useRef(0);
  const closedRef = useRef(false);
  const detachingRef = useRef(false);
  const unmountedRef = useRef(false);
  const sendRef = useRef(send);
  const onCloseRef = useRef(onClose);
  const onErrorRef = useRef(onError);
  const loadTokenRef = useRef(loadToken);
  sendRef.current = send;
  onCloseRef.current = onClose;
  onErrorRef.current = onError;
  loadTokenRef.current = loadToken;
  const pendingInputRef = useRef('');
  const settleRef = useRef<number>();
  const detachingIdRef = useRef<string | null>(null);
  const hiddenRef = useRef(hidden);
  hiddenRef.current = hidden;
  const sendTerminalBytesRef = useRef<(data: string) => void>(() => {});
  const historyOpenRef = useRef(true);
  const heldLiveRef = useRef<Uint8Array[]>([]);
  const [stickyCtrl, setStickyCtrl] = useState(false);
  const stickyCtrlRef = useRef(false);
  stickyCtrlRef.current = stickyCtrl;
  const [historyReady, setHistoryReady] = useState(false);
  const [fitting, setFitting] = useState(false);
  const fittingRef = useRef(false);
  const [ptyBusy, setPtyBusy] = useState<'attach' | 'detach' | null>(null);
  const [reconnecting, setReconnecting] = useState(false);
  const lastOutputRef = useRef(0);
  const outputQueueRef = useRef(Promise.resolve());
  const fitThisDeviceRef = useRef<() => void>(() => {});
  const reconnectTerminalRef = useRef<() => void>(() => {});
  const manuallyDetachedRef = useRef(false);
  const [attached, setAttached] = useState(false);
  const [uploading, setUploading] = useState(false);
  const uploadInputRef = useRef<HTMLInputElement | null>(null);

  const runDetach = useCallback(async (terminalId: string): Promise<boolean> => {
    const sendDetach = () => sendRef.current({ type: 'tmux_terminal_detach', terminalId });
    try {
      if ((await sendDetach()).success) return true;
    } catch {
      // first attempt rejected (busy slots / transport)
    }
    try {
      if ((await sendDetach()).success) return true;
    } catch {
      // retry rejected — detach is best-effort
    }
    if (!unmountedRef.current) onErrorRef.current?.(TMUX_TERMINAL_DETACH_FAILED);
    return false;
  }, []);

  const detachIfAttached = useCallback(() => {
    const terminalId = terminalIdRef.current;
    // Guard per terminal: a detach of the OLD handle must not swallow the
    // detach of a freshly attached one.
    if (terminalId === null || (detachingRef.current && detachingIdRef.current === terminalId)) {
      return Promise.resolve(false);
    }
    detachingRef.current = true;
    detachingIdRef.current = terminalId;
    return runDetach(terminalId).then((ok) => {
      if (ok && terminalIdRef.current === terminalId) {
        terminalIdRef.current = null;
      }
      detachingRef.current = false;
      detachingIdRef.current = null;
      return ok;
    });
  }, [runDetach]);

  const closeOverlay = useCallback(() => {
    // Close hides the overlay and keeps the PTY attached so reopening the
    // same session is instant; unmount (overlay replaced or page left) is
    // what detaches.
    onCloseRef.current();
  }, []);

  useEffect(() => {
    rememberTmuxFileDestination({ runnerId, session, authority });
  }, [authority, runnerId, session]);

  useEffect(() => {
    const host = hostRef.current;
    if (!host) return;
    closedRef.current = false;
    unmountedRef.current = false;
    let cancelled = false;
    let attachStarted = false;
    let attachInFlight = false;
    let retryAttach = false;
    let healthCheckInFlight = false;
    outputLinesRef.current = 0;
    outputQueueRef.current = Promise.resolve();
    heldLiveRef.current = [];
    historyOpenRef.current = true;
    setHistoryReady(false);
    const term = new Terminal({
      cursorBlink: true,
      fontFamily: "'JetBrainsMono Nerd Font', 'JetBrainsMono NF', 'Symbols Nerd Font', 'ui-monospace', 'SFMono-Regular', 'Menlo', 'Consolas', 'monospace'",
      // Phone screens fit ~2× fewer columns than a desktop at the default
      // 15px desktop; phones default to 8 so ~64 columns fit 390px and the
      // prompt line never wraps into a scroll.
      fontSize: typeof window !== 'undefined' && window.innerWidth <= 719 ? 8 : 15,
      scrollback: 2000,
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(host);
    // tmux owns the application screen; its outer alternate buffer would hide replayed history.
    const bufferModes = new Set([47, 1047, 1049]);
    const modeDisposables = ['h', 'l'].map((final) => term.parser.registerCsiHandler(
      { prefix: '?', final },
      (params) => {
        const modes = params.filter((param): param is number => typeof param === 'number');
        if (!modes.some((mode) => bufferModes.has(mode))) return false;
        for (const mode of modes) {
          if (!bufferModes.has(mode)) term.write(`\x1b[?${mode}${final}`);
        }
        return true;
      },
    ));
    const daDisposables = [
      term.parser.registerCsiHandler({ final: 'c' }, () => true),
      term.parser.registerCsiHandler({ prefix: '>', final: 'c' }, () => true),
    ];
    termRef.current = term;
    fitRef.current = fit;
    reconnectTerminalRef.current = () => {
      if (cancelled || closedRef.current || manuallyDetachedRef.current) return;
      // A second click during an in-flight attach must not queue a retry
      // that later churns the freshly attached PTY.
      if (attachInFlight) return;
      terminalIdRef.current = null;
      heldLiveRef.current = [];
      historyOpenRef.current = true;
      setHistoryReady(false);
      setAttached(false);
      retryAttach = true;
      tryAttach();
    };
    const recoverTerminal = (terminalId: string, error: unknown) => {
      if (cancelled || closedRef.current || terminalIdRef.current !== terminalId) return;
      if (!(error instanceof RunnerCommandError && error.kind === 'transport')
        && !(error instanceof Error && error.message.includes('tmux terminal not found:'))) return;
      setAttached(false);
      terminalIdRef.current = null;
      retryAttach = true;
      setReconnecting(true);
      // The old connection owns this handle; cleanup must not delay recovery.
      void sendRef.current({ type: 'tmux_terminal_detach', terminalId }).catch(() => {});
      tryAttach();
    };
    const sendSize = async (fitWindow = false) => {
      if (cancelled || closedRef.current) return;
      const terminalId = terminalIdRef.current;
      if (terminalId === null) return;
      try {
        const response = await sendRef.current({ type: 'tmux_terminal_resize', terminalId, cols: term.cols, rows: term.rows, ...(fitWindow ? { fitWindow: true } : {}) });
        if (!response.success) {
          if (fitWindow) {
            if (!cancelled) onErrorRef.current?.(TMUX_TERMINAL_FIT_FAILED);
            return;
          }
          recoverTerminal(terminalId, new Error(response.error));
        }
      } catch (error) {
        if (fitWindow) {
          if (!cancelled) onErrorRef.current?.(TMUX_TERMINAL_FIT_FAILED);
          return;
        }
        recoverTerminal(terminalId, error);
      }
    };
    let applyingWindowSize = false;
    const fitAndSendSize = () => {
      applyingWindowSize = true;
      fit.fit();
      applyingWindowSize = false;
      void sendSize(false);
    };
    fitThisDeviceRef.current = () => {
      if (fittingRef.current) return;
      fittingRef.current = true;
      setFitting(true);
      const buffer = term.buffer.active;
      const keep = Math.min(buffer.baseY - buffer.viewportY + term.rows + 200, 1000);
      if (term.options.scrollback !== undefined && term.options.scrollback > keep) {
        term.options.scrollback = keep;
      }
      fit.fit();
      void sendSize(true).finally(() => {
        fittingRef.current = false;
        if (!cancelled && !unmountedRef.current) setFitting(false);
      });
    };
    let scrollingDown = false;
    const restoreLivePrompt = () => {
      if (scrollingDown && term.buffer.active.viewportY >= term.buffer.active.baseY) {
        scrollingDown = false;
        term.scrollToBottom();
        sendSize();
      }
    };
    // Capture direction before xterm handles the wheel; onScroll also covers smooth scrolling.
    const onWheel = (event: WheelEvent) => {
      scrollingDown = event.deltaY > 0;
      restoreLivePrompt();
    };
    const scrollDisposable = term.onScroll(restoreLivePrompt);
    host.addEventListener('wheel', onWheel, { capture: true, passive: true });
    let panY: number | null = null;
    let panTime = 0;
    let velocity = 0;
    let lineRemainder = 0;
    let flingFrame: number | null = null;
    const stopFling = () => {
      if (flingFrame !== null) cancelAnimationFrame(flingFrame);
      flingFrame = null;
      velocity = 0;
      lineRemainder = 0;
    };
    const scrollPixels = (pixels: number): boolean => {
      const cellHeight = (term.element?.querySelector('.xterm-screen')?.getBoundingClientRect().height ?? 0) / term.rows;
      if (cellHeight <= 0 || closedRef.current) return false;
      const buffer = term.buffer.active;
      lineRemainder += pixels / cellHeight;
      const lines = Math.max(-buffer.viewportY, Math.min(buffer.baseY - buffer.viewportY, Math.trunc(lineRemainder)));
      lineRemainder -= lines;
      if (lines !== 0) {
        scrollingDown = lines > 0;
        term.scrollLines(lines);
        restoreLivePrompt();
      }
      const atEdge = pixels < 0 ? buffer.viewportY <= 0 : buffer.viewportY >= buffer.baseY;
      if (atEdge) lineRemainder = 0;
      return !atEdge;
    };
    const startFling = () => {
      if (Math.abs(velocity) < 0.05 || typeof requestAnimationFrame !== 'function') return;
      let lastTime = performance.now();
      const tick = (time: number) => {
        flingFrame = null;
        const dt = Math.min(64, Math.max(0, time - lastTime));
        lastTime = time;
        if (!scrollPixels(velocity * dt)) {
          stopFling();
          return;
        }
        velocity *= Math.pow(0.95, dt / (1000 / 60));
        if (Math.abs(velocity) < 0.05) {
          stopFling();
          return;
        }
        flingFrame = requestAnimationFrame(tick);
      };
      flingFrame = requestAnimationFrame(tick);
    };
    const onTouchStart = (event: TouchEvent) => {
      stopFling();
      panTime = performance.now();
      scrollingDown = false;
      panY = event.touches.length === 1 ? event.touches[0].clientY : null;
    };
    const onTouchMove = (event: TouchEvent) => {
      if (event.touches.length === 1 && panY !== null) {
        event.preventDefault();
        const y = event.touches[0].clientY;
        const pixels = panY - y;
        const time = performance.now();
        const dt = time - panTime;
        if (dt > 0) {
          velocity = dt > 80 ? 0 : velocity * 0.6 + (pixels / dt) * 0.4;
          panTime = time;
        }
        panY = y;
        if (!scrollPixels(pixels)) velocity = 0;
        return;
      }
      stopFling();
      panY = null;
    };
    const onTouchEnd = (event: TouchEvent) => {
      const shouldFling = event.type === 'touchend' && event.touches.length === 0
        && panY !== null && performance.now() - panTime <= 80;
      panY = null;
      if (shouldFling) startFling();
      else stopFling();
    };
    host.addEventListener('touchstart', onTouchStart, { passive: true });
    host.addEventListener('touchmove', onTouchMove, { passive: false });
    host.addEventListener('touchend', onTouchEnd);
    host.addEventListener('touchcancel', onTouchEnd);

    const sendTerminalBytes = (data: string) => {
      const terminalId = terminalIdRef.current;
      if (terminalId === null) {
        // Never sent: hold the bytes and flush them once the attach lands.
        pendingInputRef.current += data;
        return;
      }
      void sendRef.current({
        type: 'tmux_terminal_input',
        terminalId,
        data: encodeTmuxTerminalData(data),
      }).then((response) => {
        if (!response.success) recoverTerminal(terminalId, new Error(response.error));
      }).catch((error: unknown) => recoverTerminal(terminalId, error));
    };
    sendTerminalBytesRef.current = sendTerminalBytes;
    const dataDisposable = term.onData((data) => {
      // A sticky Ctrl from the soft-key bar modifies the next typed key the
      // way the phone keyboard cannot.
      let wire = data;
      if (stickyCtrlRef.current) {
        // Only A-Z has a control code; anything else just consumes the
        // modifier so Ctrl never leaks into a later key.
        if (data.length === 1 && /^[a-z]$/i.test(data)) {
          wire = String.fromCharCode(data.toUpperCase().charCodeAt(0) - 64);
        }
        setStickyCtrl(false);
      }
      // scrollToBottom() full-rebuilds every visible row even when already at
      // the bottom; only jump when the user is scrolled up in history.
      if (term.buffer.active.viewportY !== term.buffer.active.baseY) term.scrollToBottom();
      if (closedRef.current) return;
      sendTerminalBytes(wire);
    });
    const resizeDisposable = term.onResize(() => {
      if (fittingRef.current || applyingWindowSize) return;
      void sendSize();
    });

    const tryAttach = () => {
      if (cancelled || closedRef.current || manuallyDetachedRef.current || attachInFlight || (attachStarted && !retryAttach)) return;
      // A live terminal is never overwritten: a stale retry flag from a
      // double-click must not churn PTYs.
      if (terminalIdRef.current !== null) { retryAttach = false; return; }
      fit.fit();
      if (!tmuxTerminalHostReady(host) || !tmuxTerminalGeometryReady(term.cols, term.rows)) return;
      attachStarted = true;
      attachInFlight = true;
      setPtyBusy('attach');
      const recovering = retryAttach;
      retryAttach = false;
      if (recovering) setReconnecting(true);
      const cols = term.cols;
      const rows = term.rows;
      void sendRef.current({ type: 'tmux_terminal_attach', session, cols, rows }).then((response) => {
        const parsed = response.success ? parseTmuxTerminalAttach(response.data) : null;
        if (parsed === null) {
          retryAttach = recovering;
          if (!cancelled) onErrorRef.current?.(TMUX_TERMINAL_ATTACH_FAILED);
          if (!cancelled && !retryAttach) setHistoryReady(true);
          return;
        }
        if (cancelled || closedRef.current || manuallyDetachedRef.current) {
          void runDetach(parsed.terminalId);
          return;
        }
        terminalIdRef.current = parsed.terminalId;
        retryAttach = false;
        historyOpenRef.current = true;
        setAttached(true);
        lastOutputRef.current = Date.now();
        fitAndSendSize();
        term.scrollToBottom();
        // Bytes typed while unattached were never sent; deliver them now.
        if (pendingInputRef.current !== '') {
          const pending = pendingInputRef.current;
          pendingInputRef.current = '';
          sendTerminalBytes(pending);
        }
        host.querySelector('textarea')?.focus({ preventScroll: true });
      }).catch((error: unknown) => {
        if (cancelled || closedRef.current) return;
        retryAttach = recovering || (error instanceof RunnerCommandError && error.kind === 'transport');
        onErrorRef.current?.(TMUX_TERMINAL_ATTACH_FAILED);
      }).finally(() => {
        attachInFlight = false;
        if (!cancelled && !closedRef.current) {
          setPtyBusy(null);
          setReconnecting(retryAttach);
        }
      });
    };
    const applyVisualViewportPin = () => {
      const overlay = overlayRef.current;
      if (!overlay) return;
      const visualViewport = window.visualViewport;
      if (!visualViewport) {
        overlay.style.position = '';
        overlay.style.top = '';
        overlay.style.bottom = '';
        overlay.style.height = '';
        overlay.style.left = '';
        overlay.style.width = '';
        overlay.style.right = '';
        return;
      }
      const bar = typeof document.getElementById === 'function'
        ? document.getElementById('chat-workspace-bar')
        : null;
      const barRect = bar ? bar.getBoundingClientRect() : null;
      const visTop = visualViewport.offsetTop;
      const visBottom = visTop + visualViewport.height;
      const top = planTmuxOverlayPinTop({
        visTop,
        visBottom,
        barBottom: barRect ? barRect.bottom : visTop,
        barHeight: barRect ? barRect.height : 0,
      });
      overlay.style.position = 'fixed';
      overlay.style.left = `${visualViewport.offsetLeft}px`;
      overlay.style.right = 'auto';
      overlay.style.width = `${visualViewport.width}px`;
      overlay.style.top = `${top}px`;
      overlay.style.bottom = 'auto';
      overlay.style.height = `${Math.max(0, visBottom - top)}px`;
    };
    const onHostLayout = () => {
      if (cancelled || closedRef.current || hiddenRef.current) return;
      applyVisualViewportPin();
      if (attachStarted) {
        const proposed = fit.proposeDimensions();
        const geometryChanged = proposed && (proposed.cols !== term.cols || proposed.rows !== term.rows);
        if (geometryChanged) {
          fitAndSendSize();
        }
        if (term.buffer.active.viewportY !== term.buffer.active.baseY) term.scrollToBottom();
        clearTimeout(settleRef.current);
        settleRef.current = window.setTimeout(() => {
          settleRef.current = undefined;
          if (cancelled || closedRef.current || hiddenRef.current) return;
          applyVisualViewportPin();
          fitAndSendSize();
          term.scrollToBottom();
        }, 350);
        return;
      }
      tryAttach();
    };
    const observer = new ResizeObserver(onHostLayout);
    observer.observe(host);
    window.addEventListener('resize', onHostLayout);
    window.visualViewport?.addEventListener('resize', onHostLayout);
    window.visualViewport?.addEventListener('scroll', onHostLayout);
    tryAttach();
    // Backgrounded browsers throttle setInterval; a stale terminalId must be
    // probed on the first visible/focus/pageshow, not the next 5s tick.
    const runHealthCheck = (immediate: boolean) => {
      if (cancelled || closedRef.current || attachInFlight || healthCheckInFlight) return;
      if (retryAttach) {
        tryAttach();
        return;
      }
      if (terminalIdRef.current === null) return;
      if (!immediate && Date.now() - lastOutputRef.current < 2000) return;
      healthCheckInFlight = true;
      void sendSize().finally(() => { healthCheckInFlight = false; });
    };
    const healthTimer = window.setInterval(() => runHealthCheck(false), 2000);
    const onWake = () => runHealthCheck(true);
    const onVisibility = () => {
      if (document.visibilityState === 'hidden') return;
      runHealthCheck(true);
    };
    document.addEventListener('visibilitychange', onVisibility);
    window.addEventListener('focus', onWake);
    window.addEventListener('pageshow', onWake);
    return () => {
      window.removeEventListener('focus', onWake);
      window.removeEventListener('pageshow', onWake);
      document.removeEventListener('visibilitychange', onVisibility);
      stopFling();
      cancelled = true;
      closedRef.current = true;
      unmountedRef.current = true;
      observer.disconnect();
      window.clearInterval(healthTimer);
      clearTimeout(settleRef.current);
      settleRef.current = undefined;
      window.removeEventListener('resize', onHostLayout);
      window.visualViewport?.removeEventListener('resize', onHostLayout);
      window.visualViewport?.removeEventListener('scroll', onHostLayout);
      host.removeEventListener('touchstart', onTouchStart);
      host.removeEventListener('touchmove', onTouchMove);
      host.removeEventListener('touchend', onTouchEnd);
      host.removeEventListener('touchcancel', onTouchEnd);
      host.removeEventListener('wheel', onWheel, true);
      scrollDisposable.dispose();
      dataDisposable.dispose();
      modeDisposables.forEach((disposable) => disposable.dispose());
      daDisposables.forEach((disposable) => disposable.dispose());
      resizeDisposable.dispose();
      void detachIfAttached();
      term.dispose();
      termRef.current = null;
      fitRef.current = null;
    };
  }, [detachIfAttached, runDetach, session]);

  useEffect(() => onEvent((frame) => {
    if (frame && typeof frame === 'object' && 'type' in frame && frame.type === 'runner_connected') {
      reconnectTerminalRef.current();
      return;
    }
    const terminalId = terminalIdRef.current;
    if (terminalId === null) return;
    const history = parseTmuxTerminalHistory(frame);
    if (history) {
      if (history.terminalId !== terminalId) return;
      lastOutputRef.current = Date.now();
      const bytes = history.data === '' ? new Uint8Array() : decodeTmuxTerminalData(history.data);
      if (bytes === null) return;
      const term = termRef.current;
      if (!term) return;
      const finishReplay = () => {
        historyOpenRef.current = false;
        term.scrollToBottom();
        const held = heldLiveRef.current;
        heldLiveRef.current = [];
        for (const chunk of held) {
          outputQueueRef.current = outputQueueRef.current.then(() => new Promise<void>((resolve) => {
            if (termRef.current !== term || closedRef.current) {
              resolve();
              return;
            }
            term.write(chunk, () => resolve());
          }));
        }
        setHistoryReady(true);
      };
      outputQueueRef.current = outputQueueRef.current.then(() => new Promise<void>((resolve) => {
        if (termRef.current !== term || closedRef.current) {
          resolve();
          return;
        }
        if (bytes.length === 0) {
          if (history.final) finishReplay();
          resolve();
          return;
        }
        term.write(bytes, () => {
          if (history.final) finishReplay();
          resolve();
        });
      }));
      return;
    }
    const output = parseTmuxTerminalOutput(frame);
    if (output) {
      if (output.terminalId !== terminalId) return;
      lastOutputRef.current = Date.now();
      const bytes = decodeTmuxTerminalData(output.data);
      if (bytes === null) return;
      const term = termRef.current;
      if (!term) return;
      let lines = Math.ceil(bytes.length / term.cols);
      for (const byte of bytes) if (byte === 10) lines++;
      outputLinesRef.current += lines;
      // Column changes reflow the entire scrollback; a hard cap keeps any
      // later resize O(2000) instead of growing without bound.
      const cap = 2000;
      if ((term.options.scrollback ?? 4000) < cap) {
        term.options.scrollback = Math.min(cap, Math.max(outputLinesRef.current, (term.options.scrollback ?? 4000) * 2));
      }
      if (historyOpenRef.current) {
        heldLiveRef.current.push(bytes);
        return;
      }
      outputQueueRef.current = outputQueueRef.current.then(() => new Promise<void>((resolve) => {
        if (termRef.current !== term || closedRef.current) {
          resolve();
          return;
        }
        term.write(bytes, () => {
          resolve();
        });
      }));
      return;
    }
    const exit = parseTmuxTerminalExit(frame);
    if (exit && exit.terminalId === terminalId) {
      terminalIdRef.current = null;
      historyOpenRef.current = true;
      setAttached(false);
    }
  }), [onEvent]);

  const onOverlayKeyDown = (event: React.KeyboardEvent<HTMLElement>) => {
    // Esc is a terminal key: never detach, and never let ChatApp's Escape
    // handler steal it if this overlay is focused.
    if (event.key === 'Escape') {
      event.stopPropagation();
    }
  };

  const footStatus = fitting
    ? 'Fitting…'
    : ptyBusy === 'detach'
      ? 'Detaching…'
      : ptyBusy === 'attach'
        ? 'Attaching…'
        : reconnecting
          ? 'Reconnecting…'
          : '';

  const sendOverlayFile = (file: File | undefined) => {
    if (!file || uploading) return;
    setUploading(true);
    const destination = { runnerId, session, authority };
    const tokenLoader = loadTokenRef.current;
    void uploadTmuxOverlayFile(file, destination, (host) => tokenLoader?.(host) ?? '', {
      pageProtocol: globalThis.location?.protocol ?? 'http:',
    }).then((result) => {
      if (result.ok) return;
      onErrorRef.current?.(result.message);
    }).catch(() => {
      onErrorRef.current?.('tmux-file upload failed — check the connection and try again');
    }).finally(() => {
      setUploading(false);
    });
  };

  const onOverlayUploadChange = (event: ChangeEvent<HTMLInputElement>) => {
    const input = event.currentTarget;
    const file = input.files?.[0];
    input.value = '';
    sendOverlayFile(file);
  };

  const onOverlayDrop = (event: DragEvent<HTMLElement>) => {
    if (event.dataTransfer.files.length === 0 && !Array.from(event.dataTransfer.types).includes('Files')) return;
    event.preventDefault();
    event.stopPropagation();
    sendOverlayFile(event.dataTransfer.files[0] ?? fileFromDataTransfer(event.dataTransfer));
  };

  const onOverlayDragOver = (event: DragEvent<HTMLElement>) => {
    if (event.dataTransfer.files.length === 0 && !Array.from(event.dataTransfer.types).includes('Files')) return;
    event.preventDefault();
    event.stopPropagation();
    event.dataTransfer.dropEffect = 'copy';
  };

  const onOverlayPaste = (event: ClipboardEvent<HTMLElement>) => {
    const file = event.clipboardData?.files[0] ?? fileFromDataTransfer(event.clipboardData);
    if (!file) return;
    event.preventDefault();
    event.stopPropagation();
    sendOverlayFile(file);
  };

  return (
    <section
      ref={overlayRef}
      className="chat-tmux-terminal"
      role="dialog"
      aria-modal="true"
      aria-label="Tmux terminal"
      aria-hidden={hidden || undefined}
      style={hidden ? { display: 'none' } : undefined}
      onKeyDown={onOverlayKeyDown}
      onDragOverCapture={onOverlayDragOver}
      onDropCapture={onOverlayDrop}
      onPasteCapture={onOverlayPaste}
    >
      <header className="chat-tmux-terminal__head">
        <span className="chat-tmux-terminal__title">
          {safeText(runnerLabel)} · {safeText(session)}
        </span>
        <div className="chat-tmux-terminal__actions">
          <button
            className="chat-button chat-button--compact chat-tmux-terminal__fit"
            type="button"
            aria-label="Fit terminal to this device"
            disabled={fitting}
            onClick={() => fitThisDeviceRef.current()}
          >
            <span aria-hidden="true">⤢</span>
            <span className="chat-tmux-terminal__btn-text">Fit</span>
          </button>
          <button
            className="chat-button chat-button--compact chat-tmux-terminal__attach"
            type="button"
            aria-label={attached ? 'Detach terminal' : 'Attach terminal'}
            disabled={ptyBusy !== null}
            onClick={() => {
              if (ptyBusy !== null) return;
              if (attached) {
                manuallyDetachedRef.current = true;
                setPtyBusy('detach');
                void detachIfAttached().finally(() => {
                  setPtyBusy(null);
                  setAttached(terminalIdRef.current !== null);
                  setReconnecting(false);
                });
                return;
              }
              manuallyDetachedRef.current = false;
              reconnectTerminalRef.current();
            }}
          >{attached
            ? <><span aria-hidden="true">⊟</span><span className="chat-tmux-terminal__btn-text">Detach</span></>
            : <><span aria-hidden="true">↗</span><span className="chat-tmux-terminal__btn-text">Attach</span></>}</button>
          <button
            className="chat-button chat-button--compact chat-tmux-terminal__close"
            type="button"
            aria-label="Close terminal"
            onClick={closeOverlay}
          >
            <span aria-hidden="true">←</span>
            <span className="chat-tmux-terminal__btn-text">Close</span>
          </button>
        </div>
      </header>
      <div
        className={historyReady ? 'chat-tmux-terminal__body' : 'chat-tmux-terminal__body chat-tmux-terminal__body--pending'}
        style={{ touchAction: 'none' }}
        aria-busy={!historyReady}
      >
        {/* FitAddon measures this element's parent; keeping the horizontal
         * padding on the body means fit never overestimates the width. */}
        <div className="chat-tmux-terminal__viewport" ref={hostRef} />
      </div>
      <div className="chat-tmux-terminal__keys" role="toolbar" aria-label="Terminal modifier keys">
        {([
          ['Esc', 'Escape', '\x1b'],
          ['Tab', 'Tab', '\t'],
          ['↑', 'Arrow up', '\x1b[A'],
          ['↓', 'Arrow down', '\x1b[B'],
          ['→', 'Arrow right', '\x1b[C'],
          ['←', 'Arrow left', '\x1b[D'],
        ] as const).map(([label, name, sequence]) => (
          <button
            key={label}
            className="chat-button chat-button--compact chat-tmux-terminal__key"
            type="button"
            aria-label={`${name} key`}
            onClick={() => {
              termRef.current?.focus();
              sendTerminalBytesRef.current(sequence);
            }}
          >{label}</button>
        ))}
        <button
          className="chat-button chat-button--compact chat-tmux-terminal__key"
          type="button"
          aria-label="Sticky control key"
          aria-pressed={stickyCtrl}
          onClick={() => {
            termRef.current?.focus();
            setStickyCtrl((value) => !value);
          }}
        >Ctrl</button>
        <button
          className={[
            'chat-button',
            'chat-button--compact',
            'chat-tmux-terminal__key',
            uploading ? 'chat-tmux-terminal__key--busy' : '',
          ].filter(Boolean).join(' ')}
          type="button"
          aria-label="Upload file to terminal"
          disabled={uploading}
          onClick={() => {
            if (uploading) return;
            uploadInputRef.current?.click();
          }}
        ><span aria-hidden="true">📎</span><span className="chat-tmux-terminal__btn-text"> Upload</span></button>
        <input
          id="chat-tmux-overlay-upload"
          ref={uploadInputRef}
          type="file"
          hidden
          disabled={uploading}
          onChange={onOverlayUploadChange}
        />
      </div>
      <footer className={['chat-tmux-terminal__foot', footStatus !== '' ? 'chat-footer-pulse' : ''].filter(Boolean).join(' ')}>
        {footStatus}
      </footer>
    </section>
  );
}
